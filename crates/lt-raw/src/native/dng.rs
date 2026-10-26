//! DNG (Adobe DNG Specification 1.7): the main raw IFD's CFA or
//! `LinearRaw` data, uncompressed (8/16-bit or bit-packed), lossless JPEG,
//! Deflate or lossy JPEG compressed, in strips or tiles; then
//! `LinearizationTable`, `BlackLevel` (with its repeat pattern),
//! `WhiteLevel`, `ActiveArea`, `DefaultCrop*`, the CFA pattern and plane
//! colours, and colour from the file's own `ColorMatrix` and
//! `AsShotNeutral` / `AsShotWhiteXY`.
//!
//! Not applied: opcode lists, `BlackLevelDeltaH/V`, `DefaultScale` and the
//! profile tags (`ForwardMatrix`, `CameraCalibration`, DCP data): the
//! develop pipeline's colour model is the `ColorMatrix` one (`crate::color`).

use lt_core::CfaPattern;
use rayon::prelude::*;

use super::unpack::Layout;
use super::{Blocks, Sensor, deflate, ljpeg_block, orientation, uncompressed};
use crate::RawError;
use crate::container::Container;
use crate::decode::{D65_LIGHT_SOURCE as D65, cfa_from_tag};
use crate::source::Source;
use crate::tiff::{Ifd, tag};

const PHOTOMETRIC_CFA: u32 = 32803;
const PHOTOMETRIC_LINEAR_RAW: u32 = 34892;
const COMPRESSION_NONE: u32 = 1;
const COMPRESSION_LJPEG: u32 = 7;
const COMPRESSION_DEFLATE: u32 = 8;
const COMPRESSION_DEFLATE_OLD: u32 = 32946;
const COMPRESSION_LOSSY: u32 = 34892;

/// The main image: `NewSubfileType` 0 with CFA or `LinearRaw` data, the
/// largest if there are several.
fn main_ifd<'c>(src: &dyn Source, c: &'c Container) -> Result<Option<&'c Ifd>, RawError> {
    let mut best: Option<(u64, &Ifd)> = None;
    for ifd in c.tiff.chain.iter().chain(&c.subs) {
        if ifd.uint(src, tag::NEW_SUBFILE_TYPE)?.unwrap_or(0) != 0
            || !matches!(
                ifd.uint(src, tag::PHOTOMETRIC)?,
                Some(PHOTOMETRIC_CFA | PHOTOMETRIC_LINEAR_RAW)
            )
        {
            continue;
        }
        let (Some(w), Some(h)) = (
            ifd.uint(src, tag::IMAGE_WIDTH)?,
            ifd.uint(src, tag::IMAGE_LENGTH)?,
        ) else {
            continue;
        };
        let area = u64::from(w) * u64::from(h);
        if best.is_none_or(|(a, _)| area > a) {
            best = Some((area, ifd));
        }
    }
    Ok(best.map(|(_, i)| i))
}

/// The 2×2 CFA, mapped through `CFAPlaneColor` to R = 0, G = 1, B = 2.
fn cfa(src: &dyn Source, raw: &Ifd) -> Result<Option<CfaPattern>, RawError> {
    let dim = raw.uints(src, tag::CFA_REPEAT_DIM)?.unwrap_or(vec![2, 2]);
    let Some(pat) = raw.uints(src, tag::CFA_PATTERN)? else {
        return Ok(None);
    };
    if dim.get(..2) != Some(&[2, 2]) || pat.len() != 4 {
        return Ok(None);
    }
    let planes = raw
        .uints(src, tag::CFA_PLANE_COLOR)?
        .unwrap_or(vec![0, 1, 2]);
    let mapped: Option<Vec<u32>> = pat
        .iter()
        .map(|&p| planes.get(p as usize).copied())
        .collect();
    Ok(mapped.as_deref().and_then(cfa_from_tag))
}

/// Rationals (or integers) of `tag` as `f64`, `None` when absent.
fn reals(src: &dyn Source, ifd: &Ifd, t: u16) -> Result<Option<Vec<f64>>, RawError> {
    Ok(ifd
        .rationals(src, t)?
        .map(|v| v.iter().map(|r| r.to_f64().unwrap_or(0.0)).collect()))
}

/// Black level by position in the frame's 2×2 repeat (or per channel for
/// `LinearRaw`). The DNG repeat pattern starts at the active area's origin
/// `(ax, ay)`; entries are averaged into the frame's 2×2 positions they
/// fall on (exact for 1×1 and 2×2 patterns).
fn black(
    src: &dyn Source,
    raw: &Ifd,
    cpp: usize,
    ax: usize,
    ay: usize,
) -> Result<[f32; 4], RawError> {
    let dim = raw
        .uints(src, tag::BLACK_LEVEL_REPEAT_DIM)?
        .unwrap_or(vec![1, 1]);
    let (rows, cols) = match dim.as_slice() {
        [r, c, ..] if *r > 0 && *c > 0 => (*r as usize, *c as usize),
        _ => (1, 1),
    };
    let Some(v) = reals(src, raw, tag::BLACK_LEVEL)? else {
        return Ok([0.0; 4]);
    };
    if v.len() < rows * cols * cpp {
        return Err(RawError::Malformed(format!(
            "{} BlackLevel values for a {rows}x{cols}x{cpp} pattern",
            v.len()
        )));
    }
    let mut out = [0.0f32; 4];
    if cpp > 1 {
        for (s, o) in out.iter_mut().enumerate().take(cpp.min(3)) {
            *o = v[s] as f32;
        }
        return Ok(out);
    }
    for (slot, o) in out.iter_mut().enumerate() {
        let (sx, sy) = (slot % 2, slot / 2);
        let (mut sum, mut n) = (0.0f64, 0u32);
        for i in 0..rows {
            for j in 0..cols {
                let row_hits = (ay + i) % 2 == sy || rows % 2 == 1;
                if row_hits && ((ax + j) % 2 == sx || cols % 2 == 1) {
                    sum += v[i * cols + j];
                    n += 1;
                }
            }
        }
        *o = (sum / f64::from(n.max(1))) as f32;
    }
    Ok(out)
}

/// `ColorMatrix1/2` with their illuminants: the D65 one, else the one with
/// the lowest `LightSource` code (the convention of [`crate::color`]).
fn matrix(src: &dyn Source, ifd0: &Ifd) -> Result<Option<([f32; 9], u16)>, RawError> {
    let mut best: Option<([f32; 9], u16)> = None;
    for (mt, it) in [
        (tag::COLOR_MATRIX_1, tag::CALIBRATION_ILLUMINANT_1),
        (tag::COLOR_MATRIX_2, tag::CALIBRATION_ILLUMINANT_2),
    ] {
        let Some(m) = reals(src, ifd0, mt)? else {
            continue;
        };
        let Ok(m) = <[f64; 9]>::try_from(m.as_slice()) else {
            continue; // not a 3-colour camera
        };
        let ill = ifd0.uint(src, it)?.unwrap_or(0) as u16;
        let better = match best {
            None => true,
            Some((_, b)) => b != D65 && (ill == D65 || ill < b),
        };
        if better {
            best = Some((m.map(|v| v as f32), ill));
        }
    }
    Ok(best)
}

/// As-shot multipliers: `1 / AsShotNeutral`, or the camera's response to
/// `AsShotWhiteXY` through `matrix`; NaN when neither is there.
fn as_shot(src: &dyn Source, ifd0: &Ifd, matrix: Option<&[f32; 9]>) -> Result<[f32; 3], RawError> {
    if let Some(n) = reals(src, ifd0, tag::AS_SHOT_NEUTRAL)?
        && let [r, g, b, ..] = n[..]
    {
        return Ok([r, g, b].map(|v| (1.0 / v) as f32));
    }
    if let (Some(xy), Some(m)) = (reals(src, ifd0, tag::AS_SHOT_WHITE_XY)?, matrix)
        && let [x, y, ..] = xy[..]
        && y > 0.0
    {
        let xyz = [x / y, 1.0, (1.0 - x - y) / y];
        let cam: [f64; 3] =
            std::array::from_fn(|i| (0..3).map(|j| f64::from(m[i * 3 + j]) * xyz[j]).sum());
        return Ok(cam.map(|v| (1.0 / v) as f32));
    }
    Ok([f32::NAN; 3])
}

/// A lossy-JPEG (baseline DCT, 8-bit) block of `bw`-pixel rows with `cpp`
/// samples per pixel; the decoder converts YCbCr to RGB.
fn lossy_block(bytes: &[u8], out: &mut [u16], bw: usize, cpp: usize) -> Result<(), RawError> {
    let err = |e: zune_jpeg::errors::DecodeErrors| {
        RawError::Malformed(format!("DNG lossy JPEG tile: {e:?}"))
    };
    let cs = if cpp == 1 {
        zune_core::colorspace::ColorSpace::Luma
    } else {
        zune_core::colorspace::ColorSpace::RGB
    };
    let opts = zune_core::options::DecoderOptions::default().jpeg_set_out_colorspace(cs);
    let mut dec =
        zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(bytes), opts);
    dec.decode_headers().map_err(err)?;
    let (Some((jw, jh)), Some(cs)) = (dec.dimensions(), dec.output_colorspace()) else {
        return Err(RawError::Malformed(
            "DNG lossy JPEG tile without a frame".into(),
        ));
    };
    if cs.num_components() != cpp || jw > bw || jw * jh * cpp > out.len() {
        return Err(RawError::Malformed(format!(
            "DNG lossy JPEG tile {jw}x{jh}x{} in a {bw}-wide block of {cpp}",
            cs.num_components()
        )));
    }
    let pixels = dec.decode().map_err(err)?;
    for (src_row, dst_row) in pixels
        .chunks_exact(jw * cpp)
        .zip(out.chunks_exact_mut(bw * cpp))
    {
        for (d, &s) in dst_row.iter_mut().zip(src_row) {
            *d = u16::from(s);
        }
    }
    Ok(())
}

/// `ActiveArea` as `[x, y, w, h]`, and the crop inside it from
/// `DefaultCropOrigin/Size` (the active area when absent or out of range).
fn areas(
    src: &dyn Source,
    raw: &Ifd,
    w: usize,
    h: usize,
) -> Result<([usize; 4], [usize; 4]), RawError> {
    let active = match raw.uints(src, tag::ACTIVE_AREA)?.as_deref() {
        Some(&[t, l, b, r, ..]) if l < r && t < b && r as usize <= w && b as usize <= h => {
            [l as usize, t as usize, (r - l) as usize, (b - t) as usize]
        }
        _ => [0, 0, w, h],
    };
    let crop = match (
        reals(src, raw, tag::DEFAULT_CROP_ORIGIN)?.as_deref(),
        reals(src, raw, tag::DEFAULT_CROP_SIZE)?.as_deref(),
    ) {
        (Some(&[ox, oy, ..]), Some(&[cw, ch, ..]))
            if ox >= 0.0
                && oy >= 0.0
                && ox.round() + cw.round() <= active[2] as f64
                && oy.round() + ch.round() <= active[3] as f64 =>
        {
            [
                active[0] + ox.round() as usize,
                active[1] + oy.round() as usize,
                cw.round() as usize,
                ch.round() as usize,
            ]
        }
        _ => active,
    };
    Ok((active, crop))
}

/// Decode the main image of a DNG; `None` for layouts we don't read.
pub(super) fn decode(src: &dyn Source, c: &Container) -> Result<Option<Sensor>, RawError> {
    let Some(raw) = main_ifd(src, c)? else {
        return Ok(None);
    };
    let u = |t| raw.uint(src, t);
    let (Some(w), Some(h)) = (u(tag::IMAGE_WIDTH)?, u(tag::IMAGE_LENGTH)?) else {
        return Ok(None);
    };
    let (w, h) = (w as usize, h as usize);
    let spp = u(tag::SAMPLES_PER_PIXEL)?.unwrap_or(1) as usize;
    let (cpp, pattern) = match (u(tag::PHOTOMETRIC)?, spp) {
        (Some(PHOTOMETRIC_CFA), 1) => match cfa(src, raw)? {
            Some(p) => (1, p),
            None => return Ok(None), // not a 2×2 RGB Bayer pattern
        },
        (Some(PHOTOMETRIC_LINEAR_RAW), 3) => (3, CfaPattern::LinearRgb),
        _ => return Ok(None),
    };
    let bps = u(tag::BITS_PER_SAMPLE)?.unwrap_or(1);
    let float = u(tag::SAMPLE_FORMAT)? == Some(3);
    let blocks = Blocks::of(src, raw, w, h)?;
    let mut data = vec![0u16; w * h * cpp];
    let mut white = u(tag::WHITE_LEVEL)?.map(|v| v as f32);
    match u(tag::COMPRESSION)?.unwrap_or(COMPRESSION_NONE) {
        COMPRESSION_NONE if !float => {
            let layout = Layout::tiff(bps, c.tiff.endian)?;
            uncompressed(src, &blocks, layout, &mut data, w, cpp)?;
        }
        COMPRESSION_LJPEG if !float => blocks.decode(src, &mut data, w, cpp, ljpeg_block)?,
        COMPRESSION_DEFLATE | COMPRESSION_DEFLATE_OLD => {
            let f = deflate::Format {
                bits: bps,
                float,
                predictor: u(tag::PREDICTOR)?.unwrap_or(1),
                endian: c.tiff.endian,
                cpp,
                width: blocks.bw,
                white: if float { white.unwrap_or(1.0) } else { 65535.0 },
            };
            f.check()?;
            blocks.decode(src, &mut data, w, cpp, |b, out| deflate::block(b, out, &f))?;
            if float {
                // Scaled to 16 bits: white is now the top of the range.
                white = Some(65535.0);
            }
        }
        COMPRESSION_LOSSY if bps == 8 && !float => {
            let bw = blocks.bw;
            blocks.decode(src, &mut data, w, cpp, |b, out| {
                lossy_block(b, out, bw, cpp)
            })?;
        }
        _ => return Ok(None),
    }

    if let Some(table) = raw.uints_n(src, tag::LINEARIZATION_TABLE, 1 << 16)?
        && !table.is_empty()
    {
        let table: Vec<u16> = table.iter().map(|&v| v as u16).collect();
        let last = table.len() - 1;
        data.par_chunks_mut(1 << 16).for_each(|chunk| {
            for v in chunk {
                *v = table[usize::from(*v).min(last)];
            }
        });
    }

    let (active, crop) = areas(src, raw, w, h)?;
    let white = white.unwrap_or(((1u64 << bps.min(16)) - 1) as f32);
    let ifd0 = c.ifd0();
    let matrix = matrix(src, ifd0)?;
    Ok(Some(Sensor {
        make: c.make(src)?,
        model: c.model(src)?,
        width: w,
        height: h,
        cpp,
        data,
        cfa: pattern,
        black: black(src, raw, cpp, active[0], active[1])?,
        white,
        crop: Some(crop),
        wb: as_shot(src, ifd0, matrix.as_ref().map(|(m, _)| m))?,
        // Without a matrix the camera table is asked (as for any RAW).
        matrix,
        orientation: orientation(src, c),
    }))
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // decoded levels are exact
mod tests {
    use super::super::deflate::tests::encode as deflate_encode;
    use super::super::ljpeg::enc::{Opts, encode as ljpeg_encode};
    use super::super::testfile::{Link, Node, build, sample};
    use super::super::unpack::tests::pack_msb;
    use super::*;
    use crate::tiff::Endian;
    use crate::tiff::build::Val;
    use crate::{DecodePath, RawFile};
    use lt_core::{Orientation, Size};

    /// A DNG, described by what changes between tests.
    #[derive(Debug, Clone)]
    struct Spec {
        endian: Endian,
        w: usize,
        h: usize,
        cpp: usize,
        bps: u32,
        compression: u32,
        /// Tiles of this size, else strips of `rows_per_strip`.
        tile: Option<(usize, usize)>,
        rows_per_strip: usize,
        cfa: [u8; 4],
        plane_color: Option<[u8; 3]>,
        black: Vec<(u32, u32)>,
        black_dim: Option<[u16; 2]>,
        white: Option<u32>,
        /// `ActiveArea` (top, left, bottom, right).
        active: Option<[u32; 4]>,
        crop: Option<[(u32, u32); 4]>,
        linearization: Option<Vec<u16>>,
        matrices: Vec<(u16, [i32; 9], u16)>,
        neutral: Option<[(u32, u32); 3]>,
        white_xy: Option<[(u32, u32); 2]>,
        predictor: u32,
        float: bool,
        raw_in_ifd0: bool,
        photometric: u32,
    }

    const SONY_A9: [i32; 9] = [6389, -1703, -378, -4562, 12265, 2587, -670, 1489, 6550];

    impl Default for Spec {
        fn default() -> Self {
            Self {
                endian: Endian::Little,
                w: 24,
                h: 18,
                cpp: 1,
                bps: 16,
                compression: COMPRESSION_NONE,
                tile: None,
                rows_per_strip: 5,
                cfa: [0, 1, 1, 2],
                plane_color: None,
                black: vec![(256, 1)],
                black_dim: None,
                white: Some(16000),
                active: None,
                crop: None,
                linearization: None,
                matrices: vec![(tag::COLOR_MATRIX_2, SONY_A9, 21)],
                neutral: Some([(4, 10), (1, 1), (6, 10)]),
                white_xy: None,
                predictor: 1,
                float: false,
                raw_in_ifd0: false,
                photometric: PHOTOMETRIC_CFA,
            }
        }
    }

    impl Spec {
        fn max(&self) -> u32 {
            if self.compression == COMPRESSION_LOSSY {
                256
            } else {
                self.linearization
                    .as_ref()
                    .map_or(1 << self.bps.min(16), |t| t.len() as u32)
            }
        }

        fn samples(&self) -> Vec<u16> {
            let max = self.max();
            let mut v = Vec::with_capacity(self.w * self.h * self.cpp);
            for y in 0..self.h {
                for x in 0..self.w {
                    for c in 0..self.cpp {
                        // Lossy tiles: flat per tile so the JPEG round trip is exact.
                        v.push(if self.compression == COMPRESSION_LOSSY {
                            let (tw, th) = self.tile.unwrap_or((self.w, self.rows_per_strip));
                            sample(x / tw, y / th, c, max) * 7 % 256
                        } else {
                            sample(x, y, c, max)
                        });
                    }
                }
            }
            v
        }

        /// One block's samples (`bw × bh × cpp`, zero beyond the image).
        fn block(&self, all: &[u16], x0: usize, y0: usize, bw: usize, bh: usize) -> Vec<u16> {
            let mut b = vec![0u16; bw * bh * self.cpp];
            for y in 0..bh.min(self.h - y0) {
                for x in 0..bw.min(self.w - x0) {
                    for c in 0..self.cpp {
                        b[(y * bw + x) * self.cpp + c] =
                            all[((y0 + y) * self.w + x0 + x) * self.cpp + c];
                    }
                }
            }
            b
        }

        fn encode(&self, b: &[u16], bw: usize, bh: usize) -> Vec<u8> {
            let row = bw * self.cpp;
            match self.compression {
                COMPRESSION_NONE => b
                    .chunks_exact(row)
                    .flat_map(|r| match (self.bps, self.endian) {
                        (8, _) => r.iter().map(|&v| v as u8).collect::<Vec<_>>(),
                        (16, Endian::Little) => r.iter().flat_map(|v| v.to_le_bytes()).collect(),
                        (16, Endian::Big) => r.iter().flat_map(|v| v.to_be_bytes()).collect(),
                        (n, _) => pack_msb(r, n),
                    })
                    .collect(),
                COMPRESSION_LJPEG => {
                    // Bayer data as 2 components of half width, as cameras write it.
                    let n = if self.cpp == 1 { 2 } else { self.cpp };
                    let o = Opts {
                        precision: self.bps as u8,
                        ..Opts::default()
                    };
                    ljpeg_encode(b, row / n, bh, n, &o)
                }
                COMPRESSION_DEFLATE => deflate_encode(
                    &b.iter()
                        .map(|&v| {
                            if self.float {
                                f32::from(v) / 65535.0
                            } else {
                                f32::from(v)
                            }
                        })
                        .collect::<Vec<_>>(),
                    &deflate::Format {
                        bits: self.bps,
                        float: self.float,
                        predictor: self.predictor,
                        endian: self.endian,
                        cpp: self.cpp,
                        width: bw,
                        white: 1.0,
                    },
                ),
                COMPRESSION_LOSSY => {
                    let px: Vec<u8> = b.iter().map(|&v| v as u8).collect();
                    let mut out = Vec::new();
                    let ct = if self.cpp == 3 {
                        jpeg_encoder::ColorType::Rgb
                    } else {
                        jpeg_encoder::ColorType::Luma
                    };
                    jpeg_encoder::Encoder::new(&mut out, 100)
                        .encode(&px, bw as u16, bh as u16, ct)
                        .unwrap();
                    out
                }
                // A codec we don't read: what's inside doesn't matter.
                _ => b.iter().flat_map(|v| v.to_le_bytes()).collect(),
            }
        }

        fn build(&self) -> (Vec<u8>, Vec<u16>) {
            let all = self.samples();
            let (bw, bh) = self.tile.unwrap_or((self.w, self.rows_per_strip));
            let mut blocks = Vec::new();
            for y0 in (0..self.h).step_by(bh) {
                for x0 in (0..self.w).step_by(bw) {
                    let b = self.block(&all, x0, y0, bw, bh);
                    // Strips hold only their rows (the last one may be short).
                    let rows = if self.tile.is_some() {
                        bh
                    } else {
                        bh.min(self.h - y0)
                    };
                    blocks.push(self.encode(&b[..bw * rows * self.cpp], bw, rows));
                }
            }
            let mut raw = vec![
                (tag::NEW_SUBFILE_TYPE, Val::Long(vec![0])),
                (tag::IMAGE_WIDTH, Val::Long(vec![self.w as u32])),
                (tag::IMAGE_LENGTH, Val::Long(vec![self.h as u32])),
                (
                    tag::BITS_PER_SAMPLE,
                    Val::Short(vec![self.bps as u16; self.cpp]),
                ),
                (tag::COMPRESSION, Val::Short(vec![self.compression as u16])),
                (tag::PHOTOMETRIC, Val::Short(vec![self.photometric as u16])),
                (tag::SAMPLES_PER_PIXEL, Val::Short(vec![self.cpp as u16])),
                (tag::CFA_REPEAT_DIM, Val::Short(vec![2, 2])),
                (tag::CFA_PATTERN, Val::Byte(self.cfa.to_vec())),
                (tag::BLACK_LEVEL, Val::Rational(self.black.clone())),
            ];
            if let Some((tw, th)) = self.tile {
                raw.push((tag::TILE_WIDTH, Val::Long(vec![tw as u32])));
                raw.push((tag::TILE_LENGTH, Val::Long(vec![th as u32])));
            } else {
                raw.push((tag::ROWS_PER_STRIP, Val::Long(vec![bh as u32])));
            }
            if let Some(p) = self.plane_color {
                raw.push((tag::CFA_PLANE_COLOR, Val::Byte(p.to_vec())));
            }
            if let Some(d) = self.black_dim {
                raw.push((tag::BLACK_LEVEL_REPEAT_DIM, Val::Short(d.to_vec())));
            }
            if let Some(wl) = self.white {
                raw.push((tag::WHITE_LEVEL, Val::Long(vec![wl])));
            }
            if let Some(a) = self.active {
                raw.push((tag::ACTIVE_AREA, Val::Long(a.to_vec())));
            }
            if let Some([ox, oy, cw, ch]) = self.crop {
                raw.push((tag::DEFAULT_CROP_ORIGIN, Val::Rational(vec![ox, oy])));
                raw.push((tag::DEFAULT_CROP_SIZE, Val::Rational(vec![cw, ch])));
            }
            if let Some(t) = &self.linearization {
                raw.push((tag::LINEARIZATION_TABLE, Val::Short(t.clone())));
            }
            if self.predictor != 1 {
                raw.push((tag::PREDICTOR, Val::Short(vec![self.predictor as u16])));
            }
            if self.float {
                raw.push((tag::SAMPLE_FORMAT, Val::Short(vec![3; self.cpp])));
            }
            let mut ifd0 = vec![
                (tag::DNG_VERSION, Val::Byte(vec![1, 6, 0, 0])),
                (tag::MAKE, Val::Ascii("Maker")),
                (tag::MODEL, Val::Ascii("Model 1")),
                (tag::ORIENTATION, Val::Short(vec![6])),
            ];
            for (t, m, ill) in &self.matrices {
                ifd0.push((*t, Val::SRational(m.iter().map(|&v| (v, 10000)).collect())));
                let it = if *t == tag::COLOR_MATRIX_1 {
                    tag::CALIBRATION_ILLUMINANT_1
                } else {
                    tag::CALIBRATION_ILLUMINANT_2
                };
                ifd0.push((it, Val::Short(vec![*ill])));
            }
            if let Some(n) = self.neutral {
                ifd0.push((tag::AS_SHOT_NEUTRAL, Val::Rational(n.to_vec())));
            }
            if let Some(xy) = self.white_xy {
                ifd0.push((tag::AS_SHOT_WHITE_XY, Val::Rational(xy.to_vec())));
            }
            let nodes = if self.raw_in_ifd0 {
                ifd0.extend(raw.into_iter().filter(|(t, _)| *t != tag::NEW_SUBFILE_TYPE));
                vec![Node::new(Link::Chain, ifd0).with_blocks(blocks, self.tile.is_some())]
            } else {
                ifd0.push((tag::NEW_SUBFILE_TYPE, Val::Long(vec![1])));
                vec![
                    Node::new(Link::Chain, ifd0),
                    Node::new(Link::Sub(0), raw).with_blocks(blocks, self.tile.is_some()),
                ]
            };
            (build(self.endian, &nodes), all)
        }

        /// Build, decode natively and check the samples: `(x, y, w, h)` of
        /// the expected crop, after `map` (linearization).
        fn check(&self, crop: [usize; 4]) -> lt_core::RawImage {
            let (f, all) = self.build();
            let file = RawFile::from_bytes(&f).unwrap();
            let (img, info) = file.decode_with_info().unwrap();
            assert_eq!(info.path, DecodePath::Native);
            let [cx, cy, w, h] = crop;
            assert_eq!(img.size, Size::new(w as u32, h as u32), "{self:?}");
            let lin = |v: u16| {
                self.linearization
                    .as_ref()
                    .map_or(v, |t| t[usize::from(v).min(t.len() - 1)])
            };
            let mut want = Vec::new();
            for y in cy..cy + h {
                for x in cx..cx + w {
                    for c in 0..self.cpp {
                        want.push(lin(all[(y * self.w + x) * self.cpp + c]));
                    }
                }
            }
            if self.compression == COMPRESSION_LOSSY {
                assert!(
                    img.data.iter().zip(&want).all(|(a, b)| a.abs_diff(*b) <= 2),
                    "lossy tiles decode to their flat colour"
                );
            } else if self.float {
                // Half floats keep 11 significant bits.
                let tol = if self.bps == 16 { 32 } else { 1 };
                assert!(
                    img.data
                        .iter()
                        .zip(&want)
                        .all(|(a, b)| a.abs_diff(*b) <= tol)
                );
            } else {
                assert_eq!(img.data, want, "{self:?}");
            }
            assert_eq!(img.orientation, Orientation::Rotate90);
            img
        }
    }

    #[test]
    fn uncompressed_strips_every_bit_depth_and_byte_order() {
        for (bps, endian) in [
            (16, Endian::Little),
            (16, Endian::Big),
            (8, Endian::Little),
            (10, Endian::Big),
            (12, Endian::Little),
            (14, Endian::Big),
        ] {
            let s = Spec {
                bps,
                endian,
                black: vec![(1, 1)],
                ..Spec::default()
            };
            let img = s.check([0, 0, 24, 18]);
            assert_eq!(img.cfa, CfaPattern::Rggb);
            assert_eq!(img.white_level, 16000.0);
        }
        // Uncompressed tiles, and the raw image in IFD0 (Leica style).
        Spec {
            tile: Some((16, 16)),
            raw_in_ifd0: true,
            ..Spec::default()
        }
        .check([0, 0, 24, 18]);
    }

    #[test]
    fn ljpeg_tiles_and_strips_with_edges() {
        for tile in [Some((16, 8)), None] {
            let s = Spec {
                compression: COMPRESSION_LJPEG,
                bps: 14,
                tile,
                ..Spec::default()
            };
            s.check([0, 0, 24, 18]);
        }
        // A strip encoded taller than the image keeps the rows that fit.
        let s = Spec {
            compression: COMPRESSION_LJPEG,
            rows_per_strip: 18,
            ..Spec::default()
        };
        s.check([0, 0, 24, 18]);
    }

    #[test]
    fn active_area_default_crop_black_pattern_and_plane_colors() {
        let s = Spec {
            active: Some([2, 4, 18, 22]), // x 4..22, y 2..18
            crop: Some([(2, 1), (1, 1), (14, 1), (12, 1)]),
            black: vec![(100, 1), (200, 1), (300, 1), (401, 2)],
            black_dim: Some([2, 2]),
            // Pattern values index CFAPlaneColor: 0 → B, 1 → G, 2 → R.
            cfa: [2, 1, 1, 0],
            plane_color: Some([2, 1, 0]),
            white: None,
            ..Spec::default()
        };
        let img = s.check([6, 3, 14, 12]);
        // Frame CFA RGGB, crop at (6, 3): odd row → GBRG.
        assert_eq!(img.cfa, CfaPattern::Gbrg);
        // Repeat from the active origin (4, 2): even offsets, so position
        // (0,0) is R=100, (1,0) G=200, (0,1) G2=300, (1,1) B=200.5.
        assert_eq!(img.black_level, [100.0, 200.0, 200.5, 300.0]);
        assert_eq!(img.white_level, 65535.0, "default for 16 bits");
        // A crop past the active area falls back to the active area.
        let s = Spec {
            active: Some([2, 4, 18, 22]),
            crop: Some([(0, 1), (0, 1), (24, 1), (12, 1)]),
            ..Spec::default()
        };
        s.check([4, 2, 18, 16]);
    }

    #[test]
    fn odd_black_pattern_offsets_and_linear_raw() {
        // A 1x2 pattern from an odd active origin averages into the 2x2.
        let s = Spec {
            active: Some([1, 1, 18, 24]),
            black: vec![(10, 1), (20, 1)],
            black_dim: Some([1, 2]),
            ..Spec::default()
        };
        let img = s.check([1, 1, 23, 17]);
        // Frame column 0 is pattern column 1 (20), column 1 is 10: R = 20,
        // G = 10, B = 10, G2 = 20.
        assert_eq!(img.black_level, [20.0, 10.0, 10.0, 20.0]);
        // LinearRaw RGB, lossless JPEG tiles, a linearization table.
        let table: Vec<u16> = (0..4096u32).map(|i| (i * i / 256) as u16).collect();
        let s = Spec {
            cpp: 3,
            photometric: PHOTOMETRIC_LINEAR_RAW,
            compression: COMPRESSION_LJPEG,
            bps: 12,
            tile: Some((8, 8)),
            linearization: Some(table),
            black: vec![(1, 1), (2, 1), (3, 1)],
            ..Spec::default()
        };
        let img = s.check([0, 0, 24, 18]);
        assert_eq!(img.cfa, CfaPattern::LinearRgb);
        assert_eq!(img.black_level, [1.0, 2.0, 3.0, 2.0]);
    }

    #[test]
    fn deflate_and_lossy_tiles() {
        for (bps, float, predictor) in [
            (16, false, 2),
            (8, false, 34892),
            (32, true, 3),
            (16, true, 34894),
        ] {
            let s = Spec {
                compression: COMPRESSION_DEFLATE,
                bps,
                float,
                predictor,
                tile: Some((16, 8)),
                white: if float { None } else { Some(16000) },
                ..Spec::default()
            };
            let img = s.check([0, 0, 24, 18]);
            if float {
                assert_eq!(img.white_level, 65535.0, "floats scale to 16 bits");
            }
        }
        // A predictor DNG doesn't define for these samples is unsupported.
        let s = Spec {
            compression: COMPRESSION_DEFLATE,
            predictor: 3,
            ..Spec::default()
        };
        let (f, _) = s.build();
        assert!(matches!(
            RawFile::from_bytes(&f).unwrap().decode(),
            Err(RawError::Unsupported { .. })
        ));
        for cpp in [3, 1] {
            let s = Spec {
                cpp,
                photometric: if cpp == 3 {
                    PHOTOMETRIC_LINEAR_RAW
                } else {
                    PHOTOMETRIC_CFA
                },
                compression: COMPRESSION_LOSSY,
                bps: 8,
                tile: Some((16, 16)),
                black: vec![(0, 1); cpp],
                white: Some(255),
                ..Spec::default()
            };
            s.check([0, 0, 24, 18]);
        }
    }

    #[test]
    fn colour_from_the_file() {
        let a: [i32; 9] = [7000, -2000, -500, -5000, 13000, 2000, -800, 1700, 7000];
        // D65 wins over A regardless of order; AsShotNeutral → 1 / neutral.
        let s = Spec {
            matrices: vec![
                (tag::COLOR_MATRIX_1, a, 17),
                (tag::COLOR_MATRIX_2, SONY_A9, 21),
            ],
            ..Spec::default()
        };
        let (f, _) = s.build();
        let (img, info) = RawFile::from_bytes(&f).unwrap().decode_with_info().unwrap();
        assert_eq!(info.matrix_illuminant, 21);
        assert!((img.wb_as_shot[0] - 2.5).abs() < 1e-5);
        assert!((img.wb_as_shot[2] - 1.0 / 0.6).abs() < 1e-5);
        // No D65: the lowest illuminant code; WB from AsShotWhiteXY.
        let s = Spec {
            matrices: vec![
                (tag::COLOR_MATRIX_1, a, 23),
                (tag::COLOR_MATRIX_2, SONY_A9, 17),
            ],
            neutral: None,
            white_xy: Some([(3127, 10000), (3290, 10000)]),
            ..Spec::default()
        };
        let (f, _) = s.build();
        let (img, info) = RawFile::from_bytes(&f).unwrap().decode_with_info().unwrap();
        assert_eq!(info.matrix_illuminant, 17);
        // D65's xy through the matrix: the daylight multipliers.
        for c in 0..3 {
            assert!(
                (img.wb_as_shot[c] - img.wb_daylight[c]).abs() < 2e-3,
                "{img:?}"
            );
        }
        // Neither: daylight; no matrix at all: the camera table, which
        // doesn't know this camera.
        let s = Spec {
            neutral: None,
            ..Spec::default()
        };
        let (f, _) = s.build();
        let img = RawFile::from_bytes(&f).unwrap().decode().unwrap();
        assert_eq!(img.wb_as_shot, img.wb_daylight);
        let s = Spec {
            matrices: vec![(tag::COLOR_MATRIX_2, [1; 9], 21)],
            ..Spec::default()
        };
        let (f, _) = s.build();
        let mut nine = f.clone();
        // 4-colour matrices (12 values) are skipped like a missing one.
        let s12 = Spec {
            matrices: vec![],
            ..Spec::default()
        };
        let (f12, _) = s12.build();
        assert!(matches!(
            RawFile::from_bytes(&f12).unwrap().decode(),
            Err(RawError::Unsupported { reason, .. }) if reason.contains("colour matrix")
        ));
        nine.truncate(nine.len());
        assert!(RawFile::from_bytes(&nine).unwrap().decode().is_err());
    }

    #[test]
    fn layouts_not_covered_are_left_to_the_fallback() {
        let none = |s: Spec| {
            let (f, _) = s.build();
            let file = RawFile::from_bytes(&f).unwrap();
            assert!(file.decode_native().unwrap().is_none(), "{s:?}");
        };
        none(Spec {
            cfa: [0, 1, 3, 2], // a fourth colour
            ..Spec::default()
        });
        none(Spec {
            photometric: 2, // RGB is not a raw photometric
            ..Spec::default()
        });
        none(Spec {
            compression: 34713, // a vendor codec
            ..Spec::default()
        });
        none(Spec {
            cpp: 3,
            photometric: PHOTOMETRIC_CFA, // CFA must be one sample
            ..Spec::default()
        });
        none(Spec {
            float: true, // uncompressed floats
            bps: 32,
            ..Spec::default()
        });
    }

    #[test]
    fn damaged_data_is_an_error() {
        // A block past the end of the file.
        let (mut f, _) = Spec {
            compression: COMPRESSION_LJPEG,
            ..Spec::default()
        }
        .build();
        let len = f.len();
        f.truncate(len - 40);
        assert!(RawFile::from_bytes(&f).unwrap().decode().is_err());
        // Too few BlackLevel values for the repeat pattern.
        let (f, _) = Spec {
            black: vec![(1, 1)],
            black_dim: Some([2, 2]),
            ..Spec::default()
        }
        .build();
        assert!(matches!(
            RawFile::from_bytes(&f).unwrap().decode(),
            Err(RawError::Malformed(_))
        ));
        // A lossy tile that is not a JPEG, and one wider than its block.
        let s = Spec {
            compression: COMPRESSION_LOSSY,
            bps: 8,
            tile: Some((16, 16)),
            ..Spec::default()
        };
        let (mut f, _) = s.build();
        let soi = f.windows(2).position(|w| w == [0xFF, 0xD8]).unwrap();
        f[soi + 1] = 0;
        assert!(RawFile::from_bytes(&f).unwrap().decode().is_err());
        assert!(lossy_block(&s.encode(&[9; 16 * 16], 16, 16), &mut [0; 256], 8, 1).is_err());
    }
}
