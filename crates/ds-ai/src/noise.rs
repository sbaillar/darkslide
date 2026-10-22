//! Noise estimate on the flattest tiles.
//!
//! **σ.** Immerkær's residual `[1 -2 1; -2 4 -2; 1 -2 1]` cancels flat areas
//! *and* linear ramps (skies); for Gaussian noise of σ its response has std
//! `6σ`, so `σ = 1.4826 · median(|r|) / 6`. Real noise in a demosaiced,
//! JPEG-compressed preview is spatially correlated, and a 1-pixel kernel only
//! sees its high-frequency part (box-correlated σ 8 reads as ~2.8). The
//! kernel is therefore also applied with its taps 2, 3 and 4 pixels apart.
//! Noise plateaus once the spacing exceeds its correlation length while
//! texture keeps growing, so the wider estimates are only used on a plateau
//! (σ₄ ≤ 1.15 σ₃).
//!
//! **Laplacian floor.** What noise alone adds to the Laplacian variance (the
//! sharpness measure): the lower quartile over the flattest tiles, capped at
//! `20σ²` — the white-noise gain of the 4-neighbour Laplacian, which
//! correlated noise can only lower. The cap keeps a busy frame's
//! background texture (its "flattest" tiles are still texture) from being
//! subtracted from the subject.

use serde::{Deserialize, Serialize};

use crate::LumaImage;
use crate::tiles::TileGrid;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NoiseConfig {
    /// Fraction of eligible tiles (lowest local variance first) sampled.
    pub flat_fraction: f32,
}

impl Default for NoiseConfig {
    fn default() -> Self {
        Self {
            flat_fraction: 0.15,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Noise {
    /// Estimated noise standard deviation in 8-bit code values (DN).
    pub sigma: f32,
    /// Fraction of the frame's pixels the estimate was taken from. 0 means
    /// no usable flat area (e.g. all clipped): treat `sigma` as unknown.
    pub sampled_fraction: f32,
    /// What noise alone adds to the Laplacian variance (DN²); see the module docs.
    pub laplacian_floor: f32,
}

/// Laplacian variance / σ² for white noise (kernel `4,-1,-1,-1,-1`); an
/// upper bound for correlated noise.
const LAPLACIAN_NOISE_GAIN: f64 = 20.0;

/// Tap spacings of the residual kernel (see the module docs).
pub(crate) const STEPS: [usize; 4] = [1, 2, 3, 4];

/// Largest possible `|r|` for 8-bit input: `16 · 255`.
pub(crate) const MAX_RESIDUAL: usize = 16 * 255;
const BINS: usize = MAX_RESIDUAL + 1;

/// Tiles are eligible when well away from both clip points: clipped regions
/// have no noise and would drag the estimate to zero.
fn eligible(grid: &TileGrid) -> Vec<usize> {
    (0..grid.stats.len())
        .filter(|&i| {
            let s = &grid.stats[i];
            if s.n < 16 {
                return false;
            }
            let mean = s.luma as f64 / s.n as f64;
            (12.0..=243.0).contains(&mean) && (s.clipped as f64) < 0.01 * s.n as f64
        })
        .collect()
}

/// The flattest eligible tiles the estimate is taken from (empty when no
/// tile is usable).
pub(crate) fn flat_tiles(grid: &TileGrid, cfg: NoiseConfig) -> Vec<usize> {
    let mut idx = eligible(grid);
    if idx.is_empty() {
        return idx;
    }
    let frac = if cfg.flat_fraction.is_finite() {
        cfg.flat_fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let k = ((idx.len() as f32 * frac).ceil() as usize).clamp(1, idx.len());
    idx.sort_by(|a, b| {
        grid.stats[*a]
            .luma_var()
            .total_cmp(&grid.stats[*b].luma_var())
            .then(a.cmp(b))
    });
    idx.truncate(k);
    idx
}

/// The pixel range of tile `t` at which the residual kernel with taps
/// `step` apart fits inside the frame: `(x0, y0, x1, y1)`, exclusive end
/// (empty when `x0 >= x1` or `y0 >= y1`).
pub(crate) fn residual_bounds(
    grid: &TileGrid,
    t: usize,
    step: usize,
    w: usize,
    h: usize,
) -> (usize, usize, usize, usize) {
    let (x0, y0, x1, y1) = grid.bounds(t, w, h);
    (
        x0.max(step),
        y0.max(step),
        x1.min(w.saturating_sub(step)),
        y1.min(h.saturating_sub(step)),
    )
}

/// Histogram of `|r|` (`MAX_RESIDUAL + 1` bins) over tiles `idx` with taps
/// `step` apart; returns the sample count.
fn residual_hist(
    img: &LumaImage,
    grid: &TileGrid,
    idx: &[usize],
    step: usize,
    hist: &mut [u32],
) -> u64 {
    let (w, h) = (img.width(), img.height());
    let mut n = 0u64;
    for &t in idx {
        let (x0, y0, x1, y1) = residual_bounds(grid, t, step, w, h);
        for y in y0..y1 {
            let (up, mid, dn) = (img.row(y - step), img.row(y), img.row(y + step));
            for x in x0..x1 {
                let p = |r: &[u8], k: usize| i32::from(r[x + k * step - step]);
                let r = p(up, 0) - 2 * p(up, 1) + p(up, 2) - 2 * p(mid, 0) + 4 * p(mid, 1)
                    - 2 * p(mid, 2)
                    + p(dn, 0)
                    - 2 * p(dn, 1)
                    + p(dn, 2);
                hist[r.unsigned_abs() as usize] += 1;
                n += 1;
            }
        }
    }
    n
}

pub(crate) fn measure(img: &LumaImage, grid: &TileGrid, cfg: NoiseConfig) -> Noise {
    let idx = flat_tiles(grid, cfg);
    let mut hists = vec![0u32; STEPS.len() * (MAX_RESIDUAL + 1)];
    for (hist, step) in hists.as_chunks_mut::<BINS>().0.iter_mut().zip(STEPS) {
        residual_hist(img, grid, &idx, step, hist);
    }
    from_hists(grid, &idx, &hists, img.width(), img.height())
}

/// Finish the estimate from the residual histograms of tiles `idx`, one
/// `MAX_RESIDUAL + 1`-bin histogram per entry of [`STEPS`], concatenated.
/// Shared by the CPU path and the GPU batch path (which builds the
/// histograms on the device).
pub(crate) fn from_hists(
    grid: &TileGrid,
    idx: &[usize],
    hists: &[u32],
    w: usize,
    h: usize,
) -> Noise {
    if idx.is_empty() {
        return Noise::default();
    }
    let mut sigmas = [0.0f64; STEPS.len()];
    let mut sampled = 0u64;
    for ((sigma, step), hist) in sigmas
        .iter_mut()
        .zip(STEPS)
        .zip(hists.as_chunks::<BINS>().0)
    {
        let n: u64 = hist.iter().map(|&c| u64::from(c)).sum();
        if n > 0 {
            *sigma = 1.4826 * interpolated_median(hist, n) / 6.0;
        }
        if step == 1 {
            sampled = n;
        }
    }
    // Noise, correlated or not, plateaus once the taps are further apart
    // than its correlation length (3×3-correlated noise reads 0.22σ, 0.56σ,
    // σ, σ); texture keeps growing with scale. Trust the wide kernels only on
    // that plateau.
    let [s1, s2, s3, s4] = sigmas;
    let sigma = if s4 <= 1.15 * s3 {
        s1.max(s2).max(s3).max(s4)
    } else {
        s1
    };
    // Lower quartile: the flattest tiles of a busy frame still hold texture.
    let mut floors: Vec<f64> = idx.iter().map(|&t| grid.stats[t].laplacian_var()).collect();
    floors.sort_by(f64::total_cmp);
    Noise {
        sigma: sigma as f32,
        sampled_fraction: (sampled as f64 / (w * h) as f64) as f32,
        laplacian_floor: floors[floors.len() / 4].min(LAPLACIAN_NOISE_GAIN * sigma * sigma) as f32,
    }
}

/// Median of an integer histogram holding `n > 0` samples, treating bin `v`
/// as covering `[v - 0.5, v + 0.5)` (bin 0: `[0, 0.5)`, values are absolute).
fn interpolated_median(hist: &[u32], n: u64) -> f64 {
    let half = n as f64 / 2.0;
    let mut acc = 0.0;
    let (v, before, c) = hist
        .iter()
        .enumerate()
        .find_map(|(v, &c)| {
            let (prev, c) = (acc, f64::from(c));
            acc += c;
            (c > 0.0 && acc >= half).then_some((v, prev, c))
        })
        .expect("histogram holds n samples");
    let (lo, width) = if v == 0 {
        (0.0, 0.5)
    } else {
        (v as f64 - 0.5, 1.0)
    };
    lo + width * (half - before) / c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiles::tile_pass;

    /// Deterministic xorshift → approximately Gaussian via the sum of 4 uniforms.
    pub(crate) fn gauss_noise(n: usize, sigma: f32, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 53) as f32
        };
        // Sum of 4 U(0,1) has variance 4/12 = 1/3.
        (0..n)
            .map(|_| ((next() + next() + next() + next()) - 2.0) * sigma * 3f32.sqrt())
            .collect()
    }

    fn noisy(w: usize, h: usize, sigma: f32) -> LumaImage {
        let n = gauss_noise(w * h, sigma, 42);
        // A ramp background: the kernel must cancel it.
        let data = (0..w * h)
            .map(|i| (60.0 + (i % w) as f32 * 0.3 + n[i]).round() as u8)
            .collect();
        LumaImage::from_gray8(w, h, data).unwrap()
    }

    fn est(img: &LumaImage) -> Noise {
        measure(img, &tile_pass(img, 32), NoiseConfig::default())
    }

    #[test]
    fn recovers_known_sigma_on_a_ramp() {
        for sigma in [2.0f32, 5.0, 10.0] {
            let e = est(&noisy(320, 240, sigma));
            assert!(
                (e.sigma - sigma).abs() < 0.2 * sigma + 0.3,
                "sigma {sigma}: {e:?}"
            );
            assert!(e.sampled_fraction > 0.05);
        }
        let clean = est(&noisy(320, 240, 0.0));
        assert!(clean.sigma < 0.3, "{clean:?}");
    }

    #[test]
    fn correlated_noise_is_not_underestimated() {
        // Box-correlated noise of σ 8 (3×3 blurred white noise,
        // renormalized) used to read ~2.8 with the 1-pixel kernel alone.
        let (w, h) = (320, 240);
        let white = gauss_noise(w * h, 1.0, 9);
        let data = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w).clamp(1, w - 2), (i / w).clamp(1, h - 2));
                let mut a = 0.0;
                for dy in 0..3 {
                    for dx in 0..3 {
                        a += white[(y + dy - 1) * w + x + dx - 1];
                    }
                }
                (100.0 + (i % w) as f32 * 0.2 + 8.0 * a / 3.0).round() as u8
            })
            .collect();
        let e = est(&LumaImage::from_gray8(w, h, data).unwrap());
        assert!(e.sigma > 6.0 && e.sigma < 10.0, "{e:?}");
        assert!(f64::from(e.laplacian_floor) <= 20.0 * f64::from(e.sigma).powi(2) + 1e-3);
    }

    #[test]
    fn busy_frame_floor_is_capped_by_the_noise_level() {
        // No flat area at all; the flattest tiles are texture, not noise.
        let img = crate::testutil::img(
            256,
            256,
            |x, y| if (x / 2 + y / 2) % 2 == 0 { 60 } else { 190 },
        );
        let e = est(&img);
        assert!(
            f64::from(e.laplacian_floor) <= 20.0 * f64::from(e.sigma).powi(2) + 1e-3,
            "{e:?}"
        );
    }

    #[test]
    fn noisier_image_scores_higher() {
        assert!(est(&noisy(200, 200, 8.0)).sigma > est(&noisy(200, 200, 3.0)).sigma);
    }

    #[test]
    fn clipped_black_and_tiny_images_have_no_estimate() {
        for (w, h, v) in [
            (100, 100, 0u8),
            (100, 100, 255),
            (2, 2, 128),
            (1, 300, 128),
            (0, 0, 0),
        ] {
            let img = LumaImage::from_gray8(w, h, vec![v; w * h]).unwrap();
            assert_eq!(est(&img), Noise::default(), "{w}x{h}@{v}");
        }
    }

    #[test]
    fn fraction_edge_values_still_sample() {
        let img = noisy(128, 128, 4.0);
        let g = tile_pass(&img, 32);
        let one = measure(
            &img,
            &g,
            NoiseConfig {
                flat_fraction: f32::NAN,
            },
        );
        let all = measure(&img, &g, NoiseConfig { flat_fraction: 1.0 });
        assert!(one.sampled_fraction > 0.0 && one.sampled_fraction < all.sampled_fraction);
    }

    #[test]
    fn median_interpolation() {
        assert!((interpolated_median(&[4, 0, 0], 4) - 0.25).abs() < 1e-12);
        assert!((interpolated_median(&[0, 0, 2], 2) - 2.0).abs() < 1e-12);
    }
}
