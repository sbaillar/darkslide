//! Frames (linear RGB, `u16`) and the small grey images registration works on.

use ds_core::{CfaPattern, Orientation, RawImage};
use rayon::prelude::*;

use crate::{MergeError, WHITE};

/// Linear camera RGB before white balance, interleaved, row-major; `65535` = the clip.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u16>,
}

impl Frame {
    pub fn new(w: u32, h: u32, data: Vec<u16>) -> Result<Self, MergeError> {
        if w < 8 || h < 8 {
            return Err(MergeError::Invalid(format!(
                "a {w}×{h} frame is too small (at least 8×8)"
            )));
        }
        if data.len() != w as usize * h as usize * 3 {
            return Err(MergeError::Invalid(format!(
                "a {w}×{h} frame needs {} samples, got {}",
                w as usize * h as usize * 3,
                data.len()
            )));
        }
        Ok(Self { w, h, data })
    }

    /// Normalized RGB at integer `(x, y)`.
    #[inline]
    pub(crate) fn px(&self, x: usize, y: usize) -> [f32; 3] {
        let i = (y * self.w as usize + x) * 3;
        let d = &self.data[i..i + 3];
        [d[0], d[1], d[2]].map(|v| f32::from(v) * (1.0 / WHITE))
    }

    /// Bilinear normalized RGB at `(x, y)` (pixel centres on integers); `None` outside.
    /// Up to half a pixel past the edge reads the edge (a near-identity alignment must
    /// not lose the border).
    #[inline]
    pub(crate) fn sample(&self, x: f32, y: f32) -> Option<[f32; 3]> {
        let (w, h) = (self.w as usize, self.h as usize);
        let (xm, ym) = ((w - 1) as f32, (h - 1) as f32);
        if !(x >= -0.5 && y >= -0.5 && x <= xm + 0.5 && y <= ym + 0.5) {
            return None;
        }
        let (x, y) = (x.clamp(0.0, xm), y.clamp(0.0, ym));
        let (x0, y0) = ((x as usize).min(w - 2), (y as usize).min(h - 2));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let (a, b) = (self.px(x0, y0), self.px(x0 + 1, y0));
        let (c, d) = (self.px(x0, y0 + 1), self.px(x0 + 1, y0 + 1));
        Some(std::array::from_fn(|k| {
            let top = a[k] + (b[k] - a[k]) * fx;
            let bottom = c[k] + (d[k] - c[k]) * fx;
            top + (bottom - top) * fy
        }))
    }

    /// The frame box-averaged by `factor` (previews). `factor` 1 is a copy.
    #[must_use]
    pub fn downsampled(&self, factor: u32) -> Self {
        let f = factor.max(1) as usize;
        let (w, h) = ((self.w as usize / f).max(1), (self.h as usize / f).max(1));
        let sw = self.w as usize;
        let mut data = vec![0u16; w * h * 3];
        let n = (f * f) as u32;
        data.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
            for x in 0..w {
                let mut acc = [0u32; 3];
                for yy in y * f..y * f + f {
                    let src = &self.data[(yy * sw + x * f) * 3..(yy * sw + x * f + f) * 3];
                    for p in src.as_chunks::<3>().0 {
                        for k in 0..3 {
                            acc[k] += u32::from(p[k]);
                        }
                    }
                }
                for k in 0..3 {
                    row[x * 3 + k] = ((acc[k] + n / 2) / n) as u16;
                }
            }
        });
        Self {
            w: w as u32,
            h: h as u32,
            data,
        }
    }

    /// The frame turned upright for display (`o` is the EXIF orientation of the sensor
    /// data), so frames shot in different orientations stitch in one space.
    #[must_use]
    pub fn upright(self, o: Orientation) -> Self {
        if o == Orientation::Normal {
            return self;
        }
        let (sw, sh) = (self.w as usize, self.h as usize);
        let (w, h) = if o.swaps_axes() { (sh, sw) } else { (sw, sh) };
        let mut data = vec![0u16; w * h * 3];
        data.par_chunks_mut(w * 3)
            .enumerate()
            .for_each(|(dy, row)| {
                for dx in 0..w {
                    let (sx, sy) = match o {
                        Orientation::Normal => (dx, dy),
                        Orientation::FlipH => (sw - 1 - dx, dy),
                        Orientation::Rotate180 => (sw - 1 - dx, sh - 1 - dy),
                        Orientation::FlipV => (dx, sh - 1 - dy),
                        Orientation::Transpose => (dy, dx),
                        Orientation::Rotate90 => (dy, sh - 1 - dx),
                        Orientation::Transverse => (sw - 1 - dy, sh - 1 - dx),
                        Orientation::Rotate270 => (sw - 1 - dy, dx),
                    };
                    let i = (sy * sw + sx) * 3;
                    row[dx * 3..dx * 3 + 3].copy_from_slice(&self.data[i..i + 3]);
                }
            });
        Self {
            w: w as u32,
            h: h as u32,
            data,
        }
    }
}

/// A quick half-size [`Frame`] of a raw for previews: each 2×2 Bayer block becomes one
/// pixel (R, mean G, B), black-subtracted and scaled to the clip; a linear RGB raw is
/// only normalized (full size).
pub fn quick_frame(raw: &RawImage) -> Result<Frame, MergeError> {
    raw.validate()
        .map_err(|e| MergeError::Invalid(e.to_string()))?;
    let (sw, sh) = (raw.size.width as usize, raw.size.height as usize);
    let norm = |v: u16, c: usize| {
        let b = raw.black_level[c];
        ((f32::from(v) - b) / (raw.white_level - b) * WHITE).clamp(0.0, WHITE)
    };
    if raw.cfa == CfaPattern::LinearRgb {
        let data = raw
            .data
            .par_chunks(3)
            .flat_map_iter(|p| {
                [norm(p[0], 0), norm(p[1], 1), norm(p[2], 2)].map(|v| v.round() as u16)
            })
            .collect();
        return Frame::new(sw as u32, sh as u32, data);
    }
    // R, G, B, G2 slot of each 2×2 position (see `RawImage::black_level`).
    let slot = |x: u32, y: u32| match raw.cfa.color_at(x, y) {
        Some(0) => 0,
        Some(2) => 2,
        _ if raw.cfa.color_at(x ^ 1, y) == Some(0) => 1,
        _ => 3,
    };
    let slots = [slot(0, 0), slot(1, 0), slot(0, 1), slot(1, 1)];
    let (w, h) = (sw / 2, sh / 2);
    let mut data = vec![0u16; w * h * 3];
    data.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let mut rgb = [0.0f32; 3];
            for (k, &s) in slots.iter().enumerate() {
                let v = norm(raw.data[(2 * y + k / 2) * sw + 2 * x + k % 2], s);
                match s {
                    0 => rgb[0] = v,
                    2 => rgb[2] = v,
                    _ => rgb[1] += v * 0.5,
                }
            }
            for c in 0..3 {
                row[x * 3 + c] = rgb[c].round() as u16;
            }
        }
    });
    Frame::new(w as u32, h as u32, data)
}

/// Luminance-like weights of camera RGB (green-heavy, like the sensor's own response).
#[inline]
pub(crate) fn luma(p: [f32; 3]) -> f32 {
    0.25 * p[0] + 0.5 * p[1] + 0.25 * p[2]
}

/// A single-channel `f32` image.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Gray {
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Gray {
    #[inline]
    pub(crate) fn at(&self, x: usize, y: usize) -> f32 {
        self.px[y * self.w + x]
    }

    /// Bilinear value at `(x, y)`, clamped to the edge.
    #[inline]
    pub(crate) fn sample(&self, x: f32, y: f32) -> f32 {
        let x = x.clamp(0.0, (self.w - 1) as f32);
        let y = y.clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let top = self.at(x0, y0) + (self.at(x1, y0) - self.at(x0, y0)) * fx;
        let bottom = self.at(x0, y1) + (self.at(x1, y1) - self.at(x0, y1)) * fx;
        top + (bottom - top) * fy
    }
}

/// Integer box-downsample factor that brings the long edge to at most `target`.
pub(crate) fn factor_for(w: u32, h: u32, target: u32) -> u32 {
    w.max(h).div_ceil(target.max(1)).max(1)
}

/// A sample at or above this (any channel) is at the clip.
pub(crate) const SATURATED: f32 = 0.98;

/// Registration works on box-averaged images; each frame must keep at least this many
/// pixels along its short edge at the analysis factor (an extreme aspect ratio would
/// shrink to nothing).
const MIN_ANALYSIS_EDGE: usize = 8;

/// `Invalid` unless every frame is still at least [`MIN_ANALYSIS_EDGE`] px on its short
/// edge once box-averaged by `factor`.
pub(crate) fn check_analysis<'a>(
    frames: impl IntoIterator<Item = &'a Frame>,
    factor: u32,
) -> Result<(), MergeError> {
    let f = factor.max(1) as usize;
    for fr in frames {
        if (fr.w.min(fr.h) as usize) / f < MIN_ANALYSIS_EDGE {
            return Err(MergeError::Invalid(format!(
                "a {}×{} photo is too elongated to register",
                fr.w, fr.h
            )));
        }
    }
    Ok(())
}

/// Linear luminance of `frame` box-averaged by `factor`; `1.0` (clipped) where any
/// channel of the box clips, so a clipped channel never biases a measurement.
pub(crate) fn luma_image(frame: &Frame, factor: u32) -> Gray {
    let f = factor.max(1) as usize;
    let (w, h) = (frame.w as usize / f, frame.h as usize / f);
    let mut px = vec![0.0f32; w * h];
    let inv = 1.0 / (f * f) as f32;
    px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let (mut acc, mut top) = (0.0, 0.0f32);
            for yy in y * f..y * f + f {
                for xx in x * f..x * f + f {
                    let p = frame.px(xx, yy);
                    acc += luma(p);
                    top = top.max(p[0]).max(p[1]).max(p[2]);
                }
            }
            *o = if top >= SATURATED { 1.0 } else { acc * inv };
        }
    });
    Gray { w, h, px }
}

/// Separable Gaussian blur (edges clamped).
pub(crate) fn blur(g: &Gray, sigma: f32) -> Gray {
    let r = (sigma * 3.0).ceil() as isize;
    let kernel: Vec<f32> = (-r..=r)
        .map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = kernel.iter().sum();
    let kernel: Vec<f32> = kernel.iter().map(|k| k / sum).collect();
    let (w, h) = (g.w, g.h);
    let mut tmp = vec![0.0f32; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let src = &g.px[y * w..(y + 1) * w];
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (k, kv) in kernel.iter().enumerate() {
                let xx = (x as isize + k as isize - r).clamp(0, w as isize - 1) as usize;
                acc += kv * src[xx];
            }
            *o = acc;
        }
    });
    let mut px = vec![0.0f32; w * h];
    px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (k, kv) in kernel.iter().enumerate() {
            let yy = (y as isize + k as isize - r).clamp(0, h as isize - 1) as usize;
            let src = &tmp[yy * w..(yy + 1) * w];
            for (o, s) in row.iter_mut().zip(src) {
                *o += kv * s;
            }
        }
    });
    Gray { w, h, px }
}

/// Triangular dither in `-1..1` LSB, deterministic per pixel and channel.
#[inline]
pub(crate) fn dither(x: usize, y: usize, k: usize) -> f32 {
    let mut s = (x as u64) << 32 ^ (y as u64) << 2 ^ k as u64;
    s = s.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    s ^= s >> 29;
    s = s.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    s ^= s >> 32;
    let a = (s & 0xffff) as f32 / 65536.0;
    let b = ((s >> 16) & 0xffff) as f32 / 65536.0;
    a - b
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;

    fn ramp(w: u32, h: u32) -> Frame {
        let data = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [x * 100, y * 100, 7].map(|v| v as u16)
            })
            .collect();
        Frame::new(w, h, data).unwrap()
    }

    #[test]
    fn frames_validate_and_sample_bilinearly() {
        assert!(Frame::new(8, 8, vec![0; 10]).is_err());
        assert!(Frame::new(4, 8, vec![0; 96]).is_err());
        // The two failures say which one it is.
        let Err(MergeError::Invalid(small)) = Frame::new(4, 4, vec![0; 48]) else {
            panic!("accepted");
        };
        assert!(small.contains("too small"), "{small}");
        let Err(MergeError::Invalid(count)) = Frame::new(8, 8, vec![0; 10]) else {
            panic!("accepted");
        };
        assert!(count.contains("needs 192 samples, got 10"), "{count}");
        let f = ramp(10, 8);
        let p = f.sample(2.5, 3.25).unwrap();
        assert!((p[0] * WHITE - 250.0).abs() < 0.01);
        assert!((p[1] * WHITE - 325.0).abs() < 0.01);
        assert_eq!(f.sample(9.0, 7.0).unwrap()[0] * WHITE, 900.0);
        assert_eq!(f.sample(-0.4, 1.0), f.sample(0.0, 1.0));
        assert!(f.sample(-0.6, 1.0).is_none());
        assert!(f.sample(1.0, 7.6).is_none());
        assert!(f.sample(f32::NAN, 1.0).is_none());
    }

    #[test]
    fn analysis_rejects_frames_that_shrink_to_nothing() {
        let sliver = Frame::new(8, 10_000, vec![0; 8 * 10_000 * 3]).unwrap();
        let ok = ramp(64, 48);
        assert!(check_analysis([&ok], 1).is_ok());
        assert!(check_analysis([&ok], 6).is_ok());
        assert!(check_analysis([&sliver], 1).is_ok());
        assert!(matches!(
            check_analysis([&ok, &sliver], 5),
            Err(MergeError::Invalid(m)) if m.contains("8×10000")
        ));
        // Luma on a frame it would empty must not be asked for; the check is the guard.
        assert_eq!(luma_image(&ok, 6).w, 10);
    }

    #[test]
    fn downsampling_averages_boxes() {
        let f = ramp(10, 8);
        let d = f.downsampled(2);
        assert_eq!((d.w, d.h), (5, 4));
        // Box (2..4, 2..4): x 200/300 → 250, y 200/300 → 250.
        assert_eq!(&d.data[(5 + 1) * 3..(5 + 1) * 3 + 3], &[250, 250, 7]);
        assert_eq!(f.downsampled(1), f);
        assert_eq!(factor_for(6000, 4000, 1000), 6);
        assert_eq!(factor_for(500, 400, 1000), 1);
        let mut clipped = f.clone();
        clipped.data[0] = 65535;
        assert_eq!(luma_image(&clipped, 2).at(0, 0), 1.0);
        let g = luma_image(&f, 2);
        assert_eq!((g.w, g.h), (5, 4));
        let want = 0.25 * 250.0 + 0.5 * 250.0 + 0.25 * 7.0;
        assert!((g.at(1, 1) * WHITE - want).abs() < 0.01);
    }

    /// Every orientation lands each source pixel where the EXIF definition puts it.
    #[test]
    fn upright_follows_exif_orientation() {
        let f = ramp(10, 8);
        let corner = |o: Orientation| {
            let u = f.clone().upright(o);
            (u.w, u.h, [u.data[0] / 100, u.data[1] / 100])
        };
        assert_eq!(corner(Orientation::Normal), (10, 8, [0, 0]));
        assert_eq!(corner(Orientation::FlipH), (10, 8, [9, 0]));
        assert_eq!(corner(Orientation::Rotate180), (10, 8, [9, 7]));
        assert_eq!(corner(Orientation::FlipV), (10, 8, [0, 7]));
        assert_eq!(corner(Orientation::Transpose), (8, 10, [0, 0]));
        // 90° clockwise: the display's top-left was the sensor's bottom-left.
        assert_eq!(corner(Orientation::Rotate90), (8, 10, [0, 7]));
        assert_eq!(corner(Orientation::Transverse), (8, 10, [9, 7]));
        assert_eq!(corner(Orientation::Rotate270), (8, 10, [9, 0]));
        // Rotate90 then its top-right is the sensor's top-left.
        let r = f.clone().upright(Orientation::Rotate90);
        let i = 7 * 3;
        assert_eq!([r.data[i], r.data[i + 1]], [0, 0]);
    }

    #[test]
    fn quick_frames_bin_bayer_blocks() {
        use ds_core::Size;
        let mut raw = RawImage {
            size: Size::new(16, 16),
            cfa: CfaPattern::Grbg,
            data: vec![0; 256],
            black_level: [100.0, 200.0, 300.0, 400.0],
            white_level: 1100.0,
            wb_as_shot: [2.0, 1.0, 1.5],
            wb_daylight: [2.0, 1.0, 1.5],
            cam_to_xyz: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            orientation: Orientation::Normal,
            default_crop: None,
        };
        // GRBG: G (on the red row) at (0,0), R (1,0), B (0,1), G2 (1,1).
        for y in 0..16 {
            for x in 0..16 {
                raw.data[y * 16 + x] = match (x % 2, y % 2) {
                    (0, 0) => 700,  // G: (700 − 200) / 900 = 0.5556
                    (1, 0) => 600,  // R: 0.5
                    (0, 1) => 1200, // B: clipped to 1
                    _ => 500,       // G2: (500 − 400) / 700 = 0.1429
                };
            }
        }
        let f = quick_frame(&raw).unwrap();
        assert_eq!((f.w, f.h), (8, 8));
        let p = f.px(3, 5);
        assert!((p[0] - 0.5).abs() < 1e-4);
        assert!((p[1] - f32::midpoint(5.0 / 9.0, 1.0 / 7.0)).abs() < 1e-4);
        assert_eq!(p[2], 1.0);
        raw.cfa = CfaPattern::LinearRgb;
        raw.size = Size::new(8, 8);
        raw.data = vec![650; 192];
        let f = quick_frame(&raw).unwrap();
        assert_eq!((f.w, f.h), (8, 8));
        assert!((f.px(0, 0)[0] - 0.55).abs() < 1e-4);
        raw.data.pop();
        assert!(matches!(quick_frame(&raw), Err(MergeError::Invalid(_))));
    }

    #[test]
    fn blur_preserves_constants_and_spreads_impulses() {
        let mut g = Gray {
            w: 21,
            h: 21,
            px: vec![1.0; 441],
        };
        let b = blur(&g, 1.5);
        assert!(b.px.iter().all(|v| (v - 1.0).abs() < 1e-5));
        g.px.fill(0.0);
        g.px[10 * 21 + 10] = 1.0;
        let b = blur(&g, 1.5);
        let total: f32 = b.px.iter().sum();
        assert!((total - 1.0).abs() < 1e-4);
        assert!(b.at(10, 10) > b.at(11, 10) && b.at(11, 10) > b.at(13, 10));
        assert!((b.sample(10.0, 10.0) - b.at(10, 10)).abs() < 1e-6);
        assert!((b.sample(-5.0, 30.0) - b.at(0, 20)).abs() < 1e-6);
    }
}
