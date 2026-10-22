//! Perceptual hashes: dHash (9×8 gradient signs) and pHash (low 8×8 of a
//! 32×32 DCT-II against its median), 64 bits each.
//!
//! The frame is box-averaged once onto a ≤64×64 grid (contiguous row/column
//! runs, so the full-resolution pass is a vectorizable sum), and both hash
//! inputs are area-resampled from that grid. Images smaller than the grid in
//! either direction are upsampled by the same area resampler, so 1-pixel-wide
//! inputs still hash deterministically.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::LumaImage;

/// dHash + pHash. Hash distance is the combined normalized Hamming distance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct PerceptualHash {
    pub dhash: u64,
    pub phash: u64,
}

impl PerceptualHash {
    /// `(hamming(dhash) + hamming(phash)) / 128`, in `0..=1`. Unrelated
    /// images land near 0.5; burst neighbours usually well under 0.3.
    pub fn distance(&self, other: &PerceptualHash) -> f32 {
        let bits =
            (self.dhash ^ other.dhash).count_ones() + (self.phash ^ other.phash).count_ones();
        bits as f32 / 128.0
    }

    pub fn of(img: &LumaImage) -> PerceptualHash {
        if img.is_empty() {
            return PerceptualHash::default();
        }
        let (gw, gh) = grid_size(img.width(), img.height());
        Self::from_box_sums(&box_sums(img, gw, gh), img.width(), img.height())
    }

    /// Finish from the per-box luma sums of [`box_sums`] (computed on the CPU
    /// or, in a GPU batch, on the device). `w`, `h` > 0.
    pub(crate) fn from_box_sums(sums: &[u32], w: usize, h: usize) -> PerceptualHash {
        let (gw, gh) = grid_size(w, h);
        let xs = box_edges(w, gw);
        let ys = box_edges(h, gh);
        let grid: Vec<f32> = sums
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                let (bx, by) = (i % gw, i / gw);
                s as f32 / ((xs[bx + 1] - xs[bx]) * (ys[by + 1] - ys[by])) as u32 as f32
            })
            .collect();
        PerceptualHash {
            dhash: dhash(&area_resample(&grid, gw, gh, 9, 8)),
            phash: phash(&area_resample(&grid, gw, gh, 32, 32)),
        }
    }
}

/// Edge of the box grid both hashes are resampled from.
pub(crate) const GRID: usize = 64;

/// Box grid size for a `w × h` frame: `min(GRID, w) × min(GRID, h)`.
pub(crate) fn grid_size(w: usize, h: usize) -> (usize, usize) {
    (GRID.min(w), GRID.min(h))
}

/// Box `b` of `n` covers `edges[b]..edges[b + 1]` of `len` pixels.
fn box_edges(len: usize, n: usize) -> Vec<usize> {
    (0..=n).map(|b| b * len / n).collect()
}

/// Luma sums on a `gw × gh` grid of contiguous boxes (see [`grid_size`]).
pub(crate) fn box_sums(img: &LumaImage, gw: usize, gh: usize) -> Vec<u32> {
    let (w, h) = (img.width(), img.height());
    let xs = box_edges(w, gw);
    let mut sums = vec![0u32; gw * gh];
    for by in 0..gh {
        let acc = &mut sums[by * gw..(by + 1) * gw];
        for y in by * h / gh..(by + 1) * h / gh {
            let row = img.row(y);
            for (b, a) in acc.iter_mut().enumerate() {
                *a += row[xs[b]..xs[b + 1]]
                    .iter()
                    .map(|&p| u32::from(p))
                    .sum::<u32>();
            }
        }
    }
    sums
}

/// Separable area (box) resampling with exact fractional coverage; works for
/// both down- and upsampling.
fn area_resample(src: &[f32], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f32> {
    let weights = |s: usize, d: usize| -> Vec<Vec<(usize, f32)>> {
        let scale = s as f64 / d as f64;
        (0..d)
            .map(|o| {
                let (a, b) = (o as f64 * scale, (o + 1) as f64 * scale);
                let mut taps = Vec::new();
                let mut i = a.floor() as usize;
                while (i as f64) < b && i < s {
                    let cover = (b.min(i as f64 + 1.0) - a.max(i as f64)) / scale;
                    taps.push((i, cover as f32));
                    i += 1;
                }
                taps
            })
            .collect()
    };
    let (wx, wy) = (weights(sw, dw), weights(sh, dh));
    let mut tmp = vec![0.0f32; dw * sh];
    for y in 0..sh {
        for (x, taps) in wx.iter().enumerate() {
            tmp[y * dw + x] = taps.iter().map(|&(i, k)| src[y * sw + i] * k).sum();
        }
    }
    let mut out = vec![0.0f32; dw * dh];
    for (y, taps) in wy.iter().enumerate() {
        for x in 0..dw {
            out[y * dw + x] = taps.iter().map(|&(i, k)| tmp[i * dw + x] * k).sum();
        }
    }
    out
}

/// Brightness step (DN) a dHash neighbour must exceed. Without it, flat
/// areas (sky, studio backdrops) produce coin-flip bits that differ between
/// otherwise identical frames.
const DHASH_EPS: f32 = 1.0;

/// 9×8 input → 64 bits: bit set when the right neighbour is brighter.
fn dhash(g: &[f32]) -> u64 {
    let mut bits = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            if g[y * 9 + x + 1] > g[y * 9 + x] + DHASH_EPS {
                bits |= 1 << (y * 8 + x);
            }
        }
    }
    bits
}

/// `COS[u][x] = cos((2x + 1) u π / 64)` for the 8 lowest frequencies.
static COS: LazyLock<[[f32; 32]; 8]> = LazyLock::new(|| {
    let mut t = [[0.0f32; 32]; 8];
    for (u, row) in t.iter_mut().enumerate() {
        for (x, v) in row.iter_mut().enumerate() {
            *v = ((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI / 64.0).cos() as f32;
        }
    }
    t
});

/// 32×32 input → 64 bits: low 8×8 DCT-II coefficients above their median.
fn phash(g: &[f32]) -> u64 {
    let c = &*COS;
    // Rows first (keep 8 frequencies), then columns.
    let mut rows = [[0.0f32; 8]; 32];
    for (y, r) in rows.iter_mut().enumerate() {
        for (u, v) in r.iter_mut().enumerate() {
            *v = (0..32).map(|x| g[y * 32 + x] * c[u][x]).sum();
        }
    }
    let mut coef = [0.0f32; 64];
    for v in 0..8 {
        for u in 0..8 {
            coef[v * 8 + u] = (0..32).map(|y| rows[y][u] * c[v][y]).sum();
        }
    }
    let mut sorted = coef;
    sorted.sort_unstable_by(f32::total_cmp);
    let median = f32::midpoint(sorted[31], sorted[32]);
    coef.iter().enumerate().fold(
        0u64,
        |bits, (i, &v)| if v > median { bits | 1 << i } else { bits },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> LumaImage {
        let data = (0..w * h).map(|i| f(i % w, i / w)).collect();
        LumaImage::from_gray8(w, h, data).unwrap()
    }

    fn scene(w: usize, h: usize, shift: usize) -> LumaImage {
        img(w, h, |x, y| {
            let (xs, ys) = ((x + shift) as f32 / w as f32, y as f32 / h as f32);
            let blob = if (xs - 0.4).powi(2) + (ys - 0.5).powi(2) < 0.04 {
                120.0
            } else {
                0.0
            };
            (40.0 + 100.0 * xs + 60.0 * ys + blob).min(255.0) as u8
        })
    }

    #[test]
    fn identical_and_near_identical_are_close() {
        let a = PerceptualHash::of(&scene(320, 200, 0));
        assert_eq!(a.distance(&a), 0.0);
        let b = PerceptualHash::of(&scene(320, 200, 3));
        assert!(a.distance(&b) < 0.15, "{}", a.distance(&b));
        let different =
            PerceptualHash::of(&img(320, 200, |x, y| ((x / 16 + y / 16) % 2 * 200) as u8));
        assert!(a.distance(&different) > 0.25, "{}", a.distance(&different));
    }

    #[test]
    fn hash_is_scale_invariant() {
        let a = PerceptualHash::of(&scene(640, 400, 0));
        let b = PerceptualHash::of(&scene(160, 100, 0));
        assert!(a.distance(&b) < 0.1, "{}", a.distance(&b));
    }

    #[test]
    fn degenerate_inputs_hash_without_panicking() {
        assert_eq!(
            PerceptualHash::of(&img(0, 0, |_, _| 0)),
            PerceptualHash::default()
        );
        for (w, h) in [(1, 1), (1, 500), (500, 1), (3, 2)] {
            let h1 = PerceptualHash::of(&img(w, h, |x, y| ((x * 31 + y * 17) % 256) as u8));
            let h2 = PerceptualHash::of(&img(w, h, |x, y| ((x * 31 + y * 17) % 256) as u8));
            assert_eq!(h1, h2);
        }
        // A flat frame has no gradients at all.
        assert_eq!(PerceptualHash::of(&img(50, 50, |_, _| 90)).dhash, 0);
    }

    #[test]
    fn area_resample_preserves_mean_and_upsamples() {
        let src: Vec<f32> = (0..15).map(|v| v as f32).collect();
        let down = area_resample(&src, 5, 3, 2, 1);
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
        assert!((mean(&down) - mean(&src)).abs() < 1e-4);
        let up = area_resample(&[7.0], 1, 1, 4, 3);
        assert!(up.iter().all(|&v| (v - 7.0).abs() < 1e-6));
    }

    #[test]
    fn dhash_bit_layout() {
        let mut g = [0.0f32; 72];
        g[1] = 2.0; // row 0: x=0 → x=1 rises
        g[3] = 0.5; // below the step: ignored
        assert_eq!(dhash(&g), 1);
    }
}
