//! Resampling and output sharpening on [`Image16`] buffers.
//!
//! **Resizing happens in linear light.** Values are output-encoded with the
//! sRGB transfer curve (also used by Display P3), so each sample is decoded
//! through a 64 Ki-entry LUT, filtered, and re-encoded through a 1 Mi-entry
//! LUT. Averaging encoded values instead darkens fine detail (a black/white
//! checkerboard would become 50% *encoded* grey, ~128, instead of the correct
//! ~188).
//!
//! The filter is a separable Catmull-Rom whose support widens with the
//! downscale factor (area-correct, no aliasing). It is evaluated one output
//! row at a time (vertical taps into a per-thread row, then horizontal taps),
//! parallel over output rows, so the only allocation besides the output is
//! one source-width row per thread: no image-sized intermediate.
//!
//! Sharpening works in place in row bands with a rolling row copy, so it
//! needs no second full-size buffer.

use std::sync::OnceLock;

use lt_core::Size;
use rayon::prelude::*;

use crate::image::Image16;

fn catmull_rom(x: f32) -> f32 {
    let x = x.abs();
    if x < 1.0 {
        1.5 * x * x * x - 2.5 * x * x + 1.0
    } else if x < 2.0 {
        -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0
    } else {
        0.0
    }
}

/// Filter taps for one axis: for each output index, (first source index, weights).
pub(crate) fn taps(src: u32, dst: u32) -> Vec<(usize, Vec<f32>)> {
    let ratio = f64::from(src) / f64::from(dst);
    let fscale = ratio.max(1.0);
    let support = 2.0 * fscale;
    (0..dst)
        .map(|o| {
            let center = (f64::from(o) + 0.5) * ratio;
            let lo = ((center - support).floor().max(0.0)) as usize;
            let hi = ((center + support).ceil() as usize).min(src as usize);
            let mut w: Vec<f32> = (lo..hi)
                .map(|i| catmull_rom(((i as f64 + 0.5 - center) / fscale) as f32))
                .collect();
            let sum: f32 = w.iter().sum();
            if sum.abs() > f32::EPSILON {
                for v in &mut w {
                    *v /= sum;
                }
            }
            (lo, w)
        })
        .collect()
}

#[inline]
fn to_u16(v: f32) -> u16 {
    v.round().clamp(0.0, 65_535.0) as u16
}

/// sRGB EOTF (encoded 0..1 → linear 0..1).
pub fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Inverse sRGB EOTF (linear 0..1 → encoded 0..1).
pub fn linear_to_srgb(v: f64) -> f64 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// u16 encoded → linear f32.
fn decode_lut() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| {
        (0..=u16::MAX)
            .map(|v| srgb_to_linear(f64::from(v) / 65_535.0) as f32)
            .collect()
    })
}

const ENC_BITS: u32 = 20;

/// Linear (quantized to 2^20 steps) → u16 encoded. Fine enough that the
/// steepest part of the curve (slope 12.92 near black) moves < 1 code value.
pub(crate) fn encode_lut() -> &'static [u16] {
    static LUT: OnceLock<Vec<u16>> = OnceLock::new();
    LUT.get_or_init(|| {
        let n = 1u32 << ENC_BITS;
        (0..=n)
            .map(|i| to_u16(linear_to_srgb(f64::from(i) / f64::from(n)) as f32 * 65_535.0))
            .collect()
    })
}

#[inline]
pub(crate) fn encode(lut: &[u16], v: f32) -> u16 {
    let n = (1u32 << ENC_BITS) as f32;
    lut[(v.clamp(0.0, 1.0) * n).round() as usize]
}

/// Resize to `to` in linear light. Returns a clone when the size is unchanged.
pub fn resize(img: &Image16, to: Size) -> Image16 {
    if img.size == to {
        return img.clone();
    }
    let sw = img.size.width as usize;
    let dw = to.width as usize;
    let hx = taps(img.size.width, to.width);
    let vy = taps(img.size.height, to.height);
    let (dec, enc) = (decode_lut(), encode_lut());

    let mut data = vec![0u16; dw * to.height as usize * 3];
    data.par_chunks_mut(dw * 3)
        .zip(vy.par_iter())
        .for_each_init(
            || vec![0f32; sw * 3],
            |col, (out, (start, w))| {
                // Vertical taps → one linear source-width row.
                col.fill(0.0);
                for (k, &wk) in w.iter().enumerate() {
                    let row = &img.data[(start + k) * sw * 3..(start + k + 1) * sw * 3];
                    for (a, &v) in col.iter_mut().zip(row) {
                        *a += wk * dec[usize::from(v)];
                    }
                }
                // Horizontal taps → encoded output row.
                for (o, (hs, hw)) in out.as_chunks_mut::<3>().0.iter_mut().zip(&hx) {
                    let mut acc = [0f32; 3];
                    for (k, &wk) in hw.iter().enumerate() {
                        let p = (hs + k) * 3;
                        acc[0] += wk * col[p];
                        acc[1] += wk * col[p + 1];
                        acc[2] += wk * col[p + 2];
                    }
                    *o = acc.map(|a| encode(enc, a));
                }
            },
        );
    Image16 { size: to, data }
}

/// Rows per sharpening band (parallel unit).
const BAND: usize = 64;

/// Unsharp mask with a ~0.8 px [1 2 1] blur: `v + amount × (v − blur)`,
/// computed in place. `amount <= 0` leaves the image unchanged.
pub fn sharpen_in_place(img: &mut Image16, amount: f32) {
    if amount <= 0.0 {
        return;
    }
    let (w, h) = (img.size.width as usize, img.size.height as usize);
    let stride = w * 3;
    // The original row just above and just below each band (edge-clamped),
    // captured before any band is modified: 2 rows per band.
    let halos: Vec<(Vec<u16>, Vec<u16>)> = (0..h.div_ceil(BAND))
        .map(|b| {
            let above = (b * BAND).saturating_sub(1);
            let below = ((b + 1) * BAND).min(h - 1);
            (
                img.data[above * stride..(above + 1) * stride].to_vec(),
                img.data[below * stride..(below + 1) * stride].to_vec(),
            )
        })
        .collect();
    img.data
        .par_chunks_mut(BAND * stride)
        .zip(halos.par_iter())
        .for_each(|(band, (above, below))| {
            let rows = band.len() / stride;
            let mut prev = above.clone();
            let mut cur = vec![0u16; stride];
            for r in 0..rows {
                cur.copy_from_slice(&band[r * stride..(r + 1) * stride]);
                let (head, tail) = band.split_at_mut((r + 1) * stride);
                let next: &[u16] = if r + 1 < rows { &tail[..stride] } else { below };
                let out = &mut head[r * stride..];
                sharpen_row(out, [&prev, &cur, next], w, amount);
                std::mem::swap(&mut prev, &mut cur);
            }
        });
}

/// Sharpen the middle of three original rows into `out`.
fn sharpen_row(out: &mut [u16], rows: [&[u16]; 3], w: usize, amount: f32) {
    for x in 0..w {
        let xs = [x.saturating_sub(1), x, (x + 1).min(w - 1)];
        for c in 0..3 {
            let mut blur = 0f32;
            for (ry, wy) in rows.iter().zip([1.0f32, 2.0, 1.0]) {
                for (&cx, wx) in xs.iter().zip([1.0f32, 2.0, 1.0]) {
                    blur += wy * wx * f32::from(ry[cx * 3 + c]);
                }
            }
            let v = f32::from(rows[1][x * 3 + c]);
            out[x * 3 + c] = to_u16(v + amount * (v - blur / 16.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sharpen(mut img: Image16, amount: f32) -> Image16 {
        sharpen_in_place(&mut img, amount);
        img
    }

    fn flat(w: u32, h: u32, v: u16) -> Image16 {
        Image16::new(Size::new(w, h), vec![v; (w * h * 3) as usize]).unwrap()
    }

    #[test]
    fn flat_stays_flat_down_and_up() {
        for to in [Size::new(7, 5), Size::new(33, 21), Size::new(1, 1)] {
            let out = resize(&flat(20, 13, 40_000), to);
            assert_eq!(out.size, to);
            assert!(out.data.iter().all(|&v| v == 40_000), "{to:?}");
        }
    }

    #[test]
    fn same_size_is_identity() {
        let img = Image16::new(Size::new(2, 1), vec![1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(resize(&img, img.size), img);
    }

    #[test]
    fn downscale_averages_fine_detail() {
        // 1-px checkerboard of black/white must become mid-grey, not alias.
        let (w, h) = (64u32, 64u32);
        let data = (0..w * h)
            .flat_map(|i| {
                let v = if (i % w + i / w) % 2 == 0 { 0 } else { 65_535 };
                [v, v, v]
            })
            .collect();
        let out = resize(
            &Image16::new(Size::new(w, h), data).unwrap(),
            Size::new(16, 16),
        );
        // Linear-light average of 0 and 1 is 0.5 linear = 0.7354 encoded
        // (≈188/255), not 50% encoded (128/255).
        let want = (linear_to_srgb(0.5) * 65_535.0) as i32;
        for &v in &out.data {
            assert!((i32::from(v) - want).abs() < 700, "{v} vs {want}");
        }
        let v8 = f64::from(out.data[0]) / 257.0;
        assert!(
            (v8 - 187.5).abs() <= 1.0,
            "{v8} (encoded average would be ~128)"
        );
    }

    #[test]
    fn gradient_preserved_and_channels_independent() {
        let (w, h) = (100u32, 4u32);
        let data = (0..w * h)
            .flat_map(|i| {
                let x = (i % w) as u16;
                [x * 600, 1000, 65_535 - x * 600]
            })
            .collect();
        let out = resize(
            &Image16::new(Size::new(w, h), data).unwrap(),
            Size::new(50, 2),
        );
        let row: Vec<_> = out.data[..150].chunks(3).collect();
        assert!(
            row.windows(2)
                .all(|p| p[1][0] >= p[0][0] && p[1][2] <= p[0][2])
        );
        assert!(row.iter().all(|p| p[1] == 1000));
    }

    #[test]
    fn sharpen_boosts_edges_and_keeps_flat() {
        let img = flat(8, 8, 30_000);
        assert_eq!(sharpen(img.clone(), 1.0), img);
        assert_eq!(sharpen(img.clone(), 0.0), img);
        // Vertical edge.
        let data = (0..64u32)
            .flat_map(|i| [if i % 8 < 4 { 20_000 } else { 40_000 }; 3])
            .collect();
        let edge = Image16::new(Size::new(8, 8), data).unwrap();
        let s = sharpen(edge.clone(), 1.0);
        let px = |im: &Image16, x: usize| im.data[(8 * 3 + x) * 3];
        assert!(px(&s, 3) < px(&edge, 3), "dark side darker");
        assert!(px(&s, 4) > px(&edge, 4), "bright side brighter");
        assert_eq!(px(&s, 0), 20_000);
        // Clamps rather than wrapping.
        let data = (0..4u32)
            .flat_map(|i| [if i % 2 == 0 { 0 } else { 65_535 }; 3])
            .collect();
        let s = sharpen(Image16::new(Size::new(2, 2), data).unwrap(), 5.0);
        assert_eq!(s.data[0], 0);
        assert_eq!(s.data[3], 65_535);
    }

    /// Reference (allocating) unsharp mask, the pre-in-place implementation.
    fn sharpen_reference(img: &Image16, amount: f32) -> Vec<u16> {
        let (w, h) = (img.size.width as usize, img.size.height as usize);
        let stride = w * 3;
        let mut out = vec![0u16; img.data.len()];
        for y in 0..h {
            let rows = [y.saturating_sub(1), y, (y + 1).min(h - 1)]
                .map(|r| &img.data[r * stride..(r + 1) * stride]);
            sharpen_row(&mut out[y * stride..(y + 1) * stride], rows, w, amount);
        }
        out
    }

    #[test]
    fn in_place_sharpen_matches_reference_across_band_edges() {
        // Heights around multiples of BAND, noisy content.
        for h in [1u32, 2, 63, 64, 65, 129, 200] {
            let w = 7u32;
            let data: Vec<u16> = (0..w * h * 3)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 16) as u16)
                .collect();
            let img = Image16::new(Size::new(w, h), data).unwrap();
            let want = sharpen_reference(&img, 1.3);
            let mut got = img.clone();
            sharpen_in_place(&mut got, 1.3);
            assert_eq!(got.data, want, "h={h}");
        }
    }

    #[test]
    fn transfer_functions_roundtrip() {
        for v in [0.0, 0.001, 0.04, 0.2, 0.5, 1.0] {
            assert!((linear_to_srgb(srgb_to_linear(v)) - v).abs() < 1e-9, "{v}");
        }
        let enc = encode_lut();
        for v in [0u16, 1, 100, 1000, 30_000, 65_535] {
            let lin = decode_lut()[usize::from(v)];
            assert!(
                (i32::from(encode(enc, lin)) - i32::from(v)).abs() <= 1,
                "{v}"
            );
        }
        assert_eq!(encode(enc, -0.2), 0);
        assert_eq!(encode(enc, 1.7), 65_535);
    }
}
