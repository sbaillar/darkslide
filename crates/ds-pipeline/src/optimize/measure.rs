//! Statistics of an analysis render (8-bit sRGB, ≤ 1024 px).
//!
//! Tone comes from the render's luma histogram (binned on the GPU in the output pass).
//! Colour, haze, detail, sky and subject statistics are one strided scan of the pixels:
//! every second pixel of every second row, ~0.17 Mpx at 1024 × 683.

use serde::{Deserialize, Serialize};

use super::SceneSignals;
use crate::RenderedImage;
use crate::auto::Histogram;

/// Pixels darker than this (max channel, 8-bit) have no meaningful saturation.
const DARK_MAX: u8 = 20;
/// Dark-channel blocks (px, at the analysis size).
const HAZE_BLOCK: usize = 16;
/// Luma at or above this (8-bit) counts as clipped to white.
const CLIP_LUMA: usize = 254;

/// What one analysis render shows. Tones and saturations are display values in 0..=1.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderStats {
    pub median_luma: f32,
    /// Standard deviation of luma (global contrast).
    pub luma_std: f32,
    /// 0.1% and 99.9% luma percentiles.
    pub p001: f32,
    pub p999: f32,
    /// Fraction of pixels clipped to white (luma ≥ 254) / crushed to black (luma ≤ 1).
    pub clipped: f32,
    pub crushed: f32,
    /// Fraction of pixels darker than 6% luma.
    pub dark: f32,
    /// HSV saturation of pixels brighter than black: mean and 90th percentile.
    pub mean_saturation: f32,
    pub p90_saturation: f32,
    /// Dark-channel haze score: the median 16 px block's darkest channel against the
    /// atmospheric light (the brightest 1% of blocks' mean). 0 clear … 1 fog.
    pub haze: f32,
    /// Mean |luma − its 3×3 mean| (local contrast, fine detail).
    pub detail: f32,
    /// Sky (from the scene's mask): fraction clipped and mean luma.
    pub sky_clipped: Option<f32>,
    pub sky_luma: Option<f32>,
    /// Mean luma inside the salient subject.
    pub subject_luma: Option<f32>,
}

/// Measure `img` (8-bit RGBA) with its histogram `h`. `to_e` maps final-frame normalized
/// coordinates to the E frame ([`crate::frame_mapping`]), to read the scene's sky mask
/// and subject box.
pub(crate) fn render_stats(
    img: &RenderedImage,
    h: &Histogram,
    to_e: Option<[f64; 6]>,
    scene: &SceneSignals,
) -> RenderStats {
    let (w, ht) = (img.size.width as usize, img.size.height as usize);
    let mut s = tone(h);
    if w < 3 || ht < 3 || img.rgba8.len() < w * ht * 4 {
        return s;
    }
    let px = |x: usize, y: usize| -> [u8; 3] {
        let i = (y * w + x) * 4;
        [img.rgba8[i], img.rgba8[i + 1], img.rgba8[i + 2]]
    };
    let luma = |p: [u8; 3]| -> u32 {
        (2126 * u32::from(p[0]) + 7152 * u32::from(p[1]) + 722 * u32::from(p[2]) + 5000) / 10_000
    };

    let mut sat_hist = [0u32; 101];
    let (mut detail_sum, mut detail_n) = (0u64, 0u64);
    let (mut sky_n, mut sky_clip, mut sky_sum) = (0u64, 0u64, 0u64);
    let (mut subj_n, mut subj_sum) = (0u64, 0u64);
    let sky = scene.sky_mask.as_ref().filter(|_| to_e.is_some());
    let subject = scene.subject.filter(|_| to_e.is_some());
    for y in (1..ht - 1).step_by(2) {
        for x in (1..w - 1).step_by(2) {
            let p = px(x, y);
            let l = luma(p);
            let (mx, mn) = (p[0].max(p[1]).max(p[2]), p[0].min(p[1]).min(p[2]));
            if mx > DARK_MAX {
                sat_hist[usize::from(mx - mn) * 100 / usize::from(mx)] += 1;
            }
            let mut sum = 0u32;
            for (dx, dy) in [
                (0, 0),
                (1, 0),
                (0, 1),
                (1, 1),
                (2, 0),
                (0, 2),
                (2, 1),
                (1, 2),
                (2, 2),
            ] {
                sum += luma(px(x + dx - 1, y + dy - 1));
            }
            detail_sum += u64::from((l * 9).abs_diff(sum));
            detail_n += 9;
            if let Some(m) = to_e {
                let (u, v) = ((x as f64 + 0.5) / w as f64, (y as f64 + 0.5) / ht as f64);
                let (eu, ev) = (m[0] * u + m[1] * v + m[2], m[3] * u + m[4] * v + m[5]);
                if sky.is_some_and(|s| s.is_sky(eu, ev)) {
                    sky_n += 1;
                    sky_sum += u64::from(l);
                    sky_clip += u64::from(l as usize >= CLIP_LUMA);
                }
                if subject.is_some_and(|r| {
                    (f64::from(r.x)..f64::from(r.x + r.w)).contains(&eu)
                        && (f64::from(r.y)..f64::from(r.y + r.h)).contains(&ev)
                }) {
                    subj_n += 1;
                    subj_sum += u64::from(l);
                }
            }
        }
    }
    let sat_n: u32 = sat_hist.iter().sum();
    if sat_n > 0 {
        s.mean_saturation = sat_hist
            .iter()
            .enumerate()
            .map(|(i, n)| i as f64 * f64::from(*n))
            .sum::<f64>() as f32
            / sat_n as f32
            / 100.0;
        s.p90_saturation = percentile_bins(&sat_hist, 0.9) as f32 / 100.0;
    }
    s.detail = (detail_sum as f64 / detail_n.max(1) as f64 / 255.0) as f32;
    if sky_n > 0 {
        s.sky_clipped = Some(sky_clip as f32 / sky_n as f32);
        s.sky_luma = Some(sky_sum as f32 / sky_n as f32 / 255.0);
    }
    if subj_n > 0 {
        s.subject_luma = Some(subj_sum as f32 / subj_n as f32 / 255.0);
    }
    s.haze = haze(w, ht, &px);
    s
}

/// Tone statistics from the luma histogram.
fn tone(h: &Histogram) -> RenderStats {
    let total = h.total.max(1) as f64;
    let (mut sum, mut sq) = (0f64, 0f64);
    for (i, n) in h.l.iter().enumerate() {
        let v = i as f64 / 255.0;
        sum += v * f64::from(*n);
        sq += v * v * f64::from(*n);
    }
    let mean = sum / total;
    let dark_bins = (0.06f64 * 255.0).ceil() as usize;
    RenderStats {
        median_luma: f32::from(h.luma_percentile(0.5)) / 255.0,
        luma_std: (sq / total - mean * mean).max(0.0).sqrt() as f32,
        p001: f32::from(h.luma_percentile(0.001)) / 255.0,
        p999: f32::from(h.luma_percentile(0.999)) / 255.0,
        clipped: (h.l[CLIP_LUMA..].iter().map(|n| f64::from(*n)).sum::<f64>() / total) as f32,
        crushed: ((f64::from(h.l[0]) + f64::from(h.l[1])) / total) as f32,
        dark: (h.l[..dark_bins].iter().map(|n| f64::from(*n)).sum::<f64>() / total) as f32,
        ..RenderStats::default()
    }
}

/// Dark-channel haze score (see [`RenderStats::haze`]).
fn haze(w: usize, h: usize, px: &impl Fn(usize, usize) -> [u8; 3]) -> f32 {
    let (bw, bh) = (w / HAZE_BLOCK, h / HAZE_BLOCK);
    if bw == 0 || bh == 0 {
        return 0.0;
    }
    let mut dark = Vec::with_capacity(bw * bh);
    let mut bright = Vec::with_capacity(bw * bh);
    for by in 0..bh {
        for bx in 0..bw {
            let (mut mn, mut sum) = (255u8, 0u32);
            for y in (by * HAZE_BLOCK..(by + 1) * HAZE_BLOCK).step_by(2) {
                for x in (bx * HAZE_BLOCK..(bx + 1) * HAZE_BLOCK).step_by(2) {
                    let p = px(x, y);
                    mn = mn.min(p[0].min(p[1]).min(p[2]));
                    sum += u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]);
                }
            }
            dark.push(mn);
            bright.push(sum);
        }
    }
    let n = dark.len();
    let mid = n / 2;
    let median_dark = f32::from(*dark.select_nth_unstable(mid).1);
    let top = (n / 100).max(1);
    bright.select_nth_unstable(n - top);
    let samples = (HAZE_BLOCK / 2).pow(2) as f32 * 3.0;
    let light = bright[n - top..].iter().map(|v| *v as f32).sum::<f32>() / top as f32 / samples;
    (median_dark / light.max(76.0)).clamp(0.0, 1.0)
}

/// Bin below which `frac` of the counts lie.
fn percentile_bins(bins: &[u32], frac: f64) -> usize {
    let total: u64 = bins.iter().map(|n| u64::from(*n)).sum();
    let target = ((frac * total as f64).ceil() as u64).max(1);
    let mut acc = 0u64;
    for (i, n) in bins.iter().enumerate() {
        acc += u64::from(*n);
        if acc >= target {
            return i;
        }
    }
    bins.len() - 1
}
