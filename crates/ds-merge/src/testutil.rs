//! Synthetic scenes for the tests: a corner-rich value-noise texture and frames made
//! from a radiance function.

use crate::{Frame, WHITE};

fn hash(i: i64, j: i64, seed: u64) -> f32 {
    let mut h = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (j as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        ^ seed.wrapping_mul(0x1656_67b1_9e37_79f9);
    h ^= h >> 31;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 29;
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// Smooth value noise in `0..1` with features about `cell` px apart.
pub(crate) fn vnoise(x: f32, y: f32, cell: f32, seed: u64) -> f32 {
    let (u, v) = (x / cell, y / cell);
    let (i, j) = (u.floor(), v.floor());
    let (fx, fy) = (u - i, v - j);
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let (sx, sy) = (s(fx), s(fy));
    let (i, j) = (i as i64, j as i64);
    let top = hash(i, j, seed) + (hash(i + 1, j, seed) - hash(i, j, seed)) * sx;
    let bottom = hash(i, j + 1, seed) + (hash(i + 1, j + 1, seed) - hash(i, j + 1, seed)) * sx;
    top + (bottom - top) * sy
}

/// A textured reflectance in about `0.1..0.9`.
pub(crate) fn texture(x: f32, y: f32) -> f32 {
    0.1 + 0.5 * vnoise(x, y, 11.0, 1) + 0.3 * vnoise(x, y, 4.0, 2)
}

/// Tiny deterministic noise in `-1..1`.
pub(crate) fn jitter(x: usize, y: usize, k: usize, seed: u64) -> f32 {
    hash(x as i64 * 3 + k as i64, y as i64, seed) * 2.0 - 1.0
}

/// A frame of `radiance(x, y) → linear RGB` at `exposure`, clipped at 1, with relative
/// photon-like noise of `noise` (fraction of the value).
pub(crate) fn frame_from(
    w: u32,
    h: u32,
    exposure: f32,
    noise: f32,
    seed: u64,
    radiance: impl Fn(f32, f32) -> [f32; 3],
) -> Frame {
    let mut data = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let r = radiance(x as f32, y as f32);
            for (k, c) in r.iter().enumerate() {
                let v = c * exposure;
                let v = v * (1.0 + noise * jitter(x, y, k, seed)) + 0.0002 * jitter(y, x, k, seed);
                data.push((v.clamp(0.0, 1.0) * WHITE).round() as u16);
            }
        }
    }
    Frame::new(w, h, data).unwrap()
}
