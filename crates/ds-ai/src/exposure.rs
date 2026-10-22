//! Exposure quality from the luma histogram: clipping, mean/median, and a
//! dynamic-range estimate. Luma is display-referred (sRGB-encoded 8-bit).

use serde::{Deserialize, Serialize};

use crate::LumaImage;

/// Histogram cut points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposureConfig {
    /// Pixels at or above this code value count as blown highlights. JPEG
    /// previews rarely hit exactly 255, hence the margin.
    pub highlight_level: u8,
    /// Pixels at or below this code value count as crushed shadows.
    pub shadow_level: u8,
}

impl Default for ExposureConfig {
    fn default() -> Self {
        Self {
            highlight_level: 250,
            shadow_level: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Exposure {
    /// Fraction (0..=1) of pixels at or above `highlight_level`.
    pub highlights_clipped: f32,
    /// Fraction (0..=1) of pixels at or below `shadow_level`.
    pub shadows_clipped: f32,
    /// Mean luma, 0..=1 (display-referred).
    pub mean: f32,
    /// Median luma, 0..=1.
    pub median: f32,
    /// Scene range between the 0.5th and 99.5th luma percentiles, in stops
    /// (after undoing the sRGB curve). Bounded by the 8-bit floor at ~11.7.
    pub dynamic_range_stops: f32,
}

/// Luma histogram (256 bins). Four sub-histograms hide the store-to-load
/// dependency when neighbouring pixels share a bin.
pub(crate) fn histogram(img: &LumaImage) -> [u32; 256] {
    let mut h = [[0u32; 256]; 4];
    let px = img.pixels();
    let (chunks, rest) = px.as_chunks::<4>();
    for c in chunks {
        h[0][c[0] as usize] += 1;
        h[1][c[1] as usize] += 1;
        h[2][c[2] as usize] += 1;
        h[3][c[3] as usize] += 1;
    }
    for &p in rest {
        h[0][p as usize] += 1;
    }
    let mut out = [0u32; 256];
    for (i, o) in out.iter_mut().enumerate() {
        *o = h[0][i] + h[1][i] + h[2][i] + h[3][i];
    }
    out
}

/// Smallest code value whose cumulative count exceeds `q` (< 1) of `total`,
/// the histogram's sum.
fn percentile(hist: &[u32; 256], total: u64, q: f64) -> u8 {
    let target = (q * total as f64).floor() as u64;
    let mut acc = 0u64;
    hist.iter()
        .position(|&c| {
            acc += u64::from(c);
            acc > target
        })
        .expect("q < 1 is reached within the histogram") as u8
}

fn srgb_to_linear(code: u8) -> f64 {
    let v = f64::from(code) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn measure(hist: &[u32; 256], cfg: ExposureConfig) -> Exposure {
    let total: u64 = hist.iter().map(|&c| u64::from(c)).sum();
    if total == 0 {
        return Exposure::default();
    }
    let t = total as f64;
    let count = |r: std::ops::RangeInclusive<usize>| {
        hist[r].iter().map(|&c| u64::from(c)).sum::<u64>() as f64
    };
    let sum: f64 = hist
        .iter()
        .enumerate()
        .map(|(v, &c)| v as f64 * f64::from(c))
        .sum();
    let lo = percentile(hist, total, 0.005);
    let hi = percentile(hist, total, 0.995);
    // Floor the dark end at half a code value so black frames give a finite 0.
    let floor = srgb_to_linear(1) / 2.0;
    let dr = (srgb_to_linear(hi).max(floor) / srgb_to_linear(lo).max(floor)).log2();
    Exposure {
        highlights_clipped: (count(cfg.highlight_level as usize..=255) / t) as f32,
        shadows_clipped: (count(0..=cfg.shadow_level as usize) / t) as f32,
        mean: (sum / t / 255.0) as f32,
        median: f32::from(percentile(hist, total, 0.5)) / 255.0,
        dynamic_range_stops: dr as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp_of(data: Vec<u8>) -> Exposure {
        let n = data.len();
        let img = LumaImage::from_gray8(n, 1, data).unwrap();
        measure(&histogram(&img), ExposureConfig::default())
    }

    #[test]
    fn histogram_counts_every_pixel_including_remainder() {
        let img = LumaImage::from_gray8(7, 1, vec![0, 0, 5, 255, 255, 255, 9]).unwrap();
        let h = histogram(&img);
        assert_eq!((h[0], h[5], h[9], h[255]), (2, 1, 1, 3));
        assert_eq!(h.iter().sum::<u32>(), 7);
    }

    #[test]
    fn clipping_fractions_and_moments() {
        let mut d = vec![128u8; 90];
        d.extend([255; 6]);
        d.extend([0; 4]);
        let e = exp_of(d);
        assert!((e.highlights_clipped - 0.06).abs() < 1e-6);
        assert!((e.shadows_clipped - 0.04).abs() < 1e-6);
        assert!((e.median - 128.0 / 255.0).abs() < 1e-6);
        let mean = (90.0 * 128.0 + 6.0 * 255.0) / 100.0 / 255.0;
        assert!((e.mean - mean).abs() < 1e-6);
        assert!(e.dynamic_range_stops > 10.0, "{e:?}");
    }

    #[test]
    fn black_uniform_and_empty_frames() {
        let black = exp_of(vec![0; 64]);
        assert_eq!(black.shadows_clipped, 1.0);
        assert_eq!(black.dynamic_range_stops, 0.0);
        assert_eq!(black.mean, 0.0);
        let gray = exp_of(vec![100; 64]);
        assert_eq!(gray.dynamic_range_stops, 0.0);
        assert_eq!((gray.highlights_clipped, gray.shadows_clipped), (0.0, 0.0));
        assert_eq!(
            measure(&[0; 256], ExposureConfig::default()),
            Exposure::default()
        );
    }

    #[test]
    fn percentile_edges() {
        let mut h = [0u32; 256];
        h[10] = 1;
        assert_eq!(percentile(&h, 1, 0.0), 10);
        assert!((srgb_to_linear(255) - 1.0).abs() < 1e-12);
        assert!((srgb_to_linear(10) - 10.0 / 255.0 / 12.92).abs() < 1e-12);
    }
}
