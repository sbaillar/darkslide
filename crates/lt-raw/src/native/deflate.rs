//! DNG Deflate blocks (TIFF compression 8 / 32946, zlib-wrapped) with the
//! TIFF and DNG predictors: horizontal differencing (2, and DNG's 34892 /
//! 34893 over 2 or 4 pixels) and the floating-point predictor (Adobe TIFF
//! Technical Note 3: 3, and DNG's 34894 / 34895). Integer samples are 8 or
//! 16 bits; floating-point ones 16, 24 (DNG's 1-7-16 format) or 32 bits.

use crate::RawError;
use crate::tiff::Endian;

/// How a Deflate block's samples are stored.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Format {
    pub bits: u32,
    pub float: bool,
    pub predictor: u32,
    pub endian: Endian,
    /// Samples per pixel (the predictor's stride unit).
    pub cpp: usize,
    /// Pixels per row of the block.
    pub width: usize,
    /// Floating-point samples are mapped `0..=white` → `0..=65535`.
    pub white: f32,
}

fn bad(msg: impl Into<String>) -> RawError {
    RawError::Malformed(format!("DNG Deflate: {}", msg.into()))
}

impl Format {
    /// Check the combination is one DNG defines (and we read).
    pub fn check(&self) -> Result<(), RawError> {
        let ok_bits = if self.float {
            matches!(self.bits, 16 | 24 | 32)
        } else {
            matches!(self.bits, 8 | 16)
        };
        let ok_pred = match self.predictor {
            1 => true,
            2 | 34892 | 34893 => !self.float,
            3 | 34894 | 34895 => self.float,
            _ => false,
        };
        if ok_bits && ok_pred && self.white > 0.0 {
            Ok(())
        } else {
            Err(RawError::Unsupported {
                make: String::new(),
                model: String::new(),
                reason: format!(
                    "DNG Deflate with {} {}-bit samples and predictor {}",
                    if self.float { "float" } else { "integer" },
                    self.bits,
                    self.predictor
                ),
            })
        }
    }

    /// Horizontal predictor stride in pixels.
    fn stride(&self) -> usize {
        match self.predictor {
            34892 | 34894 => 2,
            34893 | 34895 => 4,
            _ => 1,
        }
    }
}

/// Inflate `bytes` into `out` (whole rows of `f.width × f.cpp` samples).
pub(crate) fn block(bytes: &[u8], out: &mut [u16], f: &Format) -> Result<(), RawError> {
    let bps = f.bits as usize / 8;
    let mut raw = vec![0u8; out.len() * bps];
    let n = miniz_oxide::inflate::decompress_slice_iter_to_slice(
        &mut raw,
        std::iter::once(bytes),
        true,
        false,
    )
    .map_err(|e| bad(format!("{e:?}")))?;
    if n != raw.len() {
        return Err(RawError::Truncated(format!(
            "Deflate block holds {n} of {} bytes",
            raw.len()
        )));
    }
    let row = f.width * f.cpp;
    let step = f.stride() * f.cpp;
    for (bytes, dst) in raw
        .chunks_exact_mut(row * bps)
        .zip(out.chunks_exact_mut(row))
    {
        if f.float {
            float_row(bytes, dst, f, step);
        } else {
            int_row(bytes, dst, f, step);
        }
    }
    Ok(())
}

fn int_row(bytes: &[u8], dst: &mut [u16], f: &Format, step: usize) {
    if f.bits == 8 {
        for (d, &b) in dst.iter_mut().zip(bytes) {
            *d = u16::from(b);
        }
    } else {
        for (d, b) in dst.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *d = f.endian.u16(*b);
        }
    }
    if f.predictor != 1 {
        let mask = if f.bits == 8 { 0xFF } else { 0xFFFF };
        for i in step..dst.len() {
            dst[i] = dst[i].wrapping_add(dst[i - step]) & mask;
        }
    }
}

/// Floating-point predictor: undo the byte differencing over the row, then
/// gather each sample's bytes from the per-byte planes (most significant
/// plane first), convert to `f32` and scale to 16 bits.
fn float_row(bytes: &mut [u8], dst: &mut [u16], f: &Format, step: usize) {
    let bps = f.bits as usize / 8;
    let n = dst.len();
    let scale = 65535.0 / f.white;
    let mut sample = [0u8; 4];
    if f.predictor == 1 {
        for (d, b) in dst.iter_mut().zip(bytes.chunks_exact(bps)) {
            for (k, s) in sample[..bps].iter_mut().enumerate() {
                *s = match f.endian {
                    Endian::Big => b[k],
                    Endian::Little => b[bps - 1 - k],
                };
            }
            *d = to_u16(decode_float(&sample[..bps]), scale);
        }
        return;
    }
    for i in step..bytes.len() {
        bytes[i] = bytes[i].wrapping_add(bytes[i - step]);
    }
    for (i, d) in dst.iter_mut().enumerate() {
        for (k, s) in sample[..bps].iter_mut().enumerate() {
            *s = bytes[k * n + i];
        }
        *d = to_u16(decode_float(&sample[..bps]), scale);
    }
}

fn to_u16(v: f32, scale: f32) -> u16 {
    (v * scale).round().clamp(0.0, 65535.0) as u16
}

/// A big-endian IEEE half (16), DNG 24-bit (1 sign, 7 exponent bits with
/// bias 63, 16 mantissa bits) or IEEE single (32) float.
fn decode_float(b: &[u8]) -> f32 {
    let (bits, exp_bits, man_bits) = match b.len() {
        2 => (u32::from(u16::from_be_bytes([b[0], b[1]])), 5, 10),
        3 => (u32::from_be_bytes([0, b[0], b[1], b[2]]), 7, 16),
        _ => return f32::from_be_bytes([b[0], b[1], b[2], b[3]]),
    };
    let sign = if bits >> (exp_bits + man_bits) & 1 == 1 {
        -1.0
    } else {
        1.0
    };
    let exp = (bits >> man_bits) & ((1 << exp_bits) - 1);
    let man = bits & ((1 << man_bits) - 1);
    let bias = (1 << (exp_bits - 1)) - 1;
    let frac = man as f32 / (1u32 << man_bits) as f32;
    let v = if exp == 0 {
        frac * 2f32.powi(1 - bias)
    } else if exp == (1 << exp_bits) - 1 {
        if man == 0 { f32::INFINITY } else { f32::NAN }
    } else {
        (1.0 + frac) * 2f32.powi(exp as i32 - bias)
    };
    sign * v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Encode samples the way a DNG writer would (the inverse of [`block`]).
    pub fn encode(samples: &[f32], f: &Format) -> Vec<u8> {
        let row = f.width * f.cpp;
        let bps = f.bits as usize / 8;
        let step = f.stride() * f.cpp;
        let mut raw = Vec::new();
        for r in samples.chunks_exact(row) {
            if f.float {
                let enc: Vec<[u8; 4]> = r
                    .iter()
                    .map(|&v| match f.bits {
                        16 => {
                            let h = half(v).to_be_bytes();
                            [h[0], h[1], 0, 0]
                        }
                        24 => {
                            let b = v.to_bits();
                            let (s, e, m) = (b >> 31, ((b >> 23) & 0xFF) as i32, b & 0x7F_FFFF);
                            let e24 = if e == 0 { 0 } else { (e - 127 + 63) as u32 };
                            let v24 = (s << 23) | (e24 << 16) | (m >> 7);
                            let x = v24.to_be_bytes();
                            [x[1], x[2], x[3], 0]
                        }
                        _ => v.to_be_bytes(),
                    })
                    .collect();
                let mut planes = vec![0u8; r.len() * bps];
                if f.predictor == 1 {
                    for (i, e) in enc.iter().enumerate() {
                        for k in 0..bps {
                            planes[i * bps + k] = match f.endian {
                                Endian::Big => e[k],
                                Endian::Little => e[bps - 1 - k],
                            };
                        }
                    }
                } else {
                    for (i, e) in enc.iter().enumerate() {
                        for k in 0..bps {
                            planes[k * r.len() + i] = e[k];
                        }
                    }
                    for i in (step..planes.len()).rev() {
                        planes[i] = planes[i].wrapping_sub(planes[i - step]);
                    }
                }
                raw.extend(planes);
            } else {
                let mut v: Vec<u16> = r.iter().map(|&x| x as u16).collect();
                if f.predictor != 1 {
                    for i in (step..v.len()).rev() {
                        v[i] = v[i].wrapping_sub(v[i - step]);
                        if f.bits == 8 {
                            v[i] &= 0xFF;
                        }
                    }
                }
                for x in v {
                    match (f.bits, f.endian) {
                        (8, _) => raw.push(x as u8),
                        (_, Endian::Little) => raw.extend(x.to_le_bytes()),
                        (_, Endian::Big) => raw.extend(x.to_be_bytes()),
                    }
                }
            }
        }
        miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6)
    }

    fn half(v: f32) -> u16 {
        // Truncating conversion of in-range values (subnormals included).
        let b = v.to_bits();
        let s = (b >> 16) & 0x8000;
        if v.abs() < 2f32.powi(-14) {
            return (s | (v.abs() / 2f32.powi(-24)) as u32) as u16;
        }
        let e = ((b >> 23) & 0xFF) as i32 - 127 + 15;
        (s | ((e as u32) << 10) | ((b >> 13) & 0x3FF)) as u16
    }

    fn fmt(bits: u32, float: bool, predictor: u32, endian: Endian) -> Format {
        Format {
            bits,
            float,
            predictor,
            endian,
            cpp: 3,
            width: 5,
            white: 1.0,
        }
    }

    #[test]
    fn integer_predictors_round_trip() {
        let v: Vec<f32> = (0..30).map(|i| ((i * 977) % 4096) as f32).collect();
        for (bits, p, e) in [
            (16, 1, Endian::Little),
            (16, 2, Endian::Big),
            (16, 34892, Endian::Little),
            (16, 34893, Endian::Little),
            (8, 2, Endian::Little),
        ] {
            let mut f = fmt(bits, false, p, e);
            f.white = 65535.0;
            let src: Vec<f32> = if bits == 8 {
                v.iter().map(|x| x % 256.0).collect()
            } else {
                v.clone()
            };
            let z = encode(&src, &f);
            let mut out = vec![0u16; 30];
            block(&z, &mut out, &f).unwrap();
            let want: Vec<u16> = src.iter().map(|&x| x as u16).collect();
            assert_eq!(out, want, "{bits} bits, predictor {p}");
        }
    }

    #[test]
    fn float_predictors_round_trip() {
        let v: Vec<f32> = (0..30).map(|i| f32::from(i as u8) / 32.0).collect();
        for (bits, p, e) in [
            (32, 1, Endian::Little),
            (32, 3, Endian::Little),
            (16, 34894, Endian::Big),
            (24, 34895, Endian::Little),
            (24, 1, Endian::Big),
            (16, 1, Endian::Little),
        ] {
            let f = fmt(bits, true, p, e);
            let z = encode(&v, &f);
            let mut out = vec![0u16; 30];
            block(&z, &mut out, &f).unwrap();
            let want: Vec<u16> = v.iter().map(|&x| to_u16(x, 65535.0)).collect();
            assert_eq!(out, want, "{bits}-bit float, predictor {p}");
        }
    }

    #[test]
    fn floats_decode_specials() {
        // Exact decodes: compare bit patterns.
        let bits = |b: &[u8]| decode_float(b).to_bits();
        assert_eq!(bits(&[0x3C, 0x00]), 1.0f32.to_bits());
        assert_eq!(bits(&[0xC0, 0x00]), (-2.0f32).to_bits());
        assert_eq!(bits(&[0x7C, 0x00]), f32::INFINITY.to_bits());
        assert!(decode_float(&[0x7C, 0x01]).is_nan());
        assert_eq!(bits(&[0x00, 0x01]), 2f32.powi(-24).to_bits()); // subnormal
        assert_eq!(bits(&[0x3F, 0x00, 0x00]), 1.0f32.to_bits());
        assert_eq!(bits(&1.5f32.to_be_bytes()), 1.5f32.to_bits());
        assert_eq!(to_u16(-1.0, 65535.0), 0);
        assert_eq!(to_u16(2.0, 65535.0), 65535);
    }

    #[test]
    fn bad_streams_and_formats() {
        let f = fmt(16, false, 1, Endian::Little);
        let mut out = vec![0u16; 15];
        assert!(block(b"not zlib", &mut out, &f).is_err());
        let short = miniz_oxide::deflate::compress_to_vec_zlib(&[0u8; 10], 6);
        assert!(matches!(
            block(&short, &mut out, &f),
            Err(RawError::Truncated(_))
        ));
        assert!(f.check().is_ok());
        assert!(fmt(12, false, 1, Endian::Little).check().is_err());
        assert!(fmt(16, false, 3, Endian::Little).check().is_err());
        assert!(fmt(32, true, 2, Endian::Little).check().is_err());
        assert!(fmt(32, true, 7, Endian::Little).check().is_err());
        let mut z = fmt(32, true, 1, Endian::Little);
        z.white = 0.0;
        assert!(z.check().is_err());
    }
}
