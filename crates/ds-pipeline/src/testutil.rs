//! Synthetic sensor data for unit tests.

use ds_core::{CfaPattern, Orientation, RawImage, Size};

use crate::color::REC2020_TO_XYZ;

/// A raw whose white-balanced camera values equal `f(x, y)` (an RGB triple in 0..=1 of
/// the clip level), with an identity-like camera (camera RGB = linear Rec.2020).
pub fn raw_from_fn(w: u32, h: u32, f: impl Fn(u32, u32) -> [f32; 3]) -> RawImage {
    let wb = [2.0f32, 1.0, 1.5];
    let (black, white) = (256.0f32, 16_383.0f32);
    let mut data = vec![0u16; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let c = CfaPattern::Rggb.color_at(x, y).unwrap() as usize;
            let v = (f(x, y)[c] / wb[c]).clamp(0.0, 1.0);
            data[(y * w + x) as usize] = (black + v * (white - black)).round() as u16;
        }
    }
    RawImage {
        size: Size::new(w, h),
        cfa: CfaPattern::Rggb,
        data,
        black_level: [black; 4],
        white_level: white,
        wb_as_shot: wb,
        cam_to_xyz: REC2020_TO_XYZ,
        wb_daylight: [1.0; 3],
        orientation: Orientation::Normal,
        default_crop: None,
    }
}

/// Uniform neutral gray at `level` (fraction of clip after WB).
pub fn gray_raw(w: u32, h: u32, level: f32) -> RawImage {
    raw_from_fn(w, h, |_, _| [level; 3])
}

/// Deterministic noise in [-1, 1].
pub fn noise(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h & 0xFFFF) as f32 / 32767.5 - 1.0
}
