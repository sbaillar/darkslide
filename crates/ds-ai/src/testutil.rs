//! Pixel fixtures for unit tests: scores are always computed from real
//! pixels, never from hand-typed metric values.

use crate::LumaImage;

pub fn img(w: usize, h: usize, mut f: impl FnMut(usize, usize) -> u8) -> LumaImage {
    LumaImage::from_gray8(w, h, (0..w * h).map(|i| f(i % w, i / w)).collect()).unwrap()
}

/// Isotropic random texture of 2×2 blocks, with a smooth ramp in the right
/// sixth: like a real frame, it has a flat area where noise can be measured
/// (pure random texture is indistinguishable from noise).
pub fn texture(w: usize, h: usize, seed: u64) -> LumaImage {
    img(w, h, |x, y| {
        if x >= w - w / 6 {
            return (90 + 60 * y / h.max(1)) as u8;
        }
        let mut s = ((x / 2) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ ((y / 2) as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ seed;
        s ^= s >> 29;
        s = s.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        s ^= s >> 32;
        (40 + s % 176) as u8
    })
}

/// Lens-like blur (a box applied twice ≈ triangle kernel, no box
/// artifacts), radii `rx`, `ry`, only inside `region` (`x0, y0, x1, y1` in
/// pixels; the whole frame when `None`).
pub fn blur_region(src: &LumaImage, rx: usize, ry: usize, region: Option<[usize; 4]>) -> LumaImage {
    let once = box_region(src, rx, ry, region);
    box_region(&once, rx, ry, region)
}

fn box_region(src: &LumaImage, rx: usize, ry: usize, region: Option<[usize; 4]>) -> LumaImage {
    let (w, h) = (src.width(), src.height());
    let [x0, y0, x1, y1] = region.unwrap_or([0, 0, w, h]);
    let px = src.pixels();
    let at = |x: isize, y: isize| {
        f32::from(px[y.clamp(0, h as isize - 1) as usize * w + x.clamp(0, w as isize - 1) as usize])
    };
    img(w, h, |x, y| {
        if !(x0..x1).contains(&x) || !(y0..y1).contains(&y) {
            return px[y * w + x];
        }
        let (mut acc, mut n) = (0.0, 0.0);
        for dy in -(ry as isize)..=ry as isize {
            for dx in -(rx as isize)..=rx as isize {
                acc += at(x as isize + dx, y as isize + dy);
                n += 1.0;
            }
        }
        (acc / n).round() as u8
    })
}

pub fn blur(src: &LumaImage, rx: usize, ry: usize) -> LumaImage {
    blur_region(src, rx, ry, None)
}

/// Multiply every pixel by `k` (exposure change in display space).
pub fn scale(src: &LumaImage, k: f32) -> LumaImage {
    let d = src
        .pixels()
        .iter()
        .map(|&p| (f32::from(p) * k).round().clamp(0.0, 255.0) as u8)
        .collect();
    LumaImage::from_gray8(src.width(), src.height(), d).unwrap()
}
