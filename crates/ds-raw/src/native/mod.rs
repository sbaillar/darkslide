//! Native sensor decoders: DNG, Sony ARW, Canon CR2, Nikon NEF and Pentax
//! PEF, written from public specifications and from the sample files
//! themselves (provenance per decoder in `docs/RAWDEC.md`).
//!
//! Each format module turns a parsed [`Container`] into a [`Sensor`] (the
//! full frame plus levels, crop and colour), or `None` when it doesn't cover
//! the file (another compression, an unknown layout): the caller then falls
//! back to rawler, or reports the file unsupported without it. [`finish`]
//! turns a [`Sensor`] into the [`RawImage`] contract every decoder shares.
//!
//! Work is split over rayon by tiles, strips, slices or row bands; buffers
//! are allocated per block, never per pixel.

mod arw;
mod cr2;
mod deflate;
mod dng;
pub(crate) mod ljpeg;
mod nef;
mod pef;
#[cfg(test)]
mod testfile;
mod unpack;

use ds_core::{CfaPattern, Orientation, RawImage, Size};
use rayon::prelude::*;

use crate::container::Container;
use crate::source::Source;
use crate::tiff::{Ifd, tag};
use crate::{DecodeInfo, DecodePath, RawError, cameras, color};

/// A decoded sensor frame and what developing it needs.
#[derive(Debug, Clone)]
pub(crate) struct Sensor {
    pub make: String,
    pub model: String,
    pub width: usize,
    pub height: usize,
    /// Samples per pixel: 1 (CFA) or 3 ([`CfaPattern::LinearRgb`]).
    pub cpp: usize,
    /// `width × height × cpp` samples, row-major.
    pub data: Vec<u16>,
    /// The CFA at the frame's top-left pixel, or `LinearRgb`.
    pub cfa: CfaPattern,
    /// Black level by position in the frame's 2×2 CFA repeat (row-major,
    /// from the top-left pixel); for `LinearRgb`, R, G, B (fourth unused).
    pub black: [f32; 4],
    pub white: f32,
    /// Recommended crop `[x, y, w, h]` in the frame, `None` for all of it.
    pub crop: Option<[usize; 4]>,
    /// As-shot white balance multipliers R, G, B (any scale), NaN if unknown.
    pub wb: [f32; 3],
    /// `ColorMatrix` (XYZ → camera) and its EXIF `LightSource` code; `None`
    /// to look the camera up in [`cameras`].
    pub matrix: Option<([f32; 9], u16)>,
    pub orientation: Orientation,
}

/// Decode with the native decoder for this file, `Ok(None)` if there is none.
pub(crate) fn decode(
    src: &dyn Source,
    c: &Container,
) -> Result<Option<(RawImage, DecodeInfo)>, RawError> {
    let make = c.make(src)?;
    let sensor = if c.ifd0().get(tag::DNG_VERSION).is_some() {
        dng::decode(src, c)?
    } else if make.eq_ignore_ascii_case("SONY") {
        arw::decode(src, c)?
    } else if make.eq_ignore_ascii_case("Canon") {
        cr2::decode(src, c)?
    } else if make.to_ascii_uppercase().starts_with("NIKON") {
        nef::decode(src, c)?
    } else if make.to_ascii_uppercase().starts_with("PENTAX")
        || make.to_ascii_uppercase().starts_with("RICOH")
    {
        pef::decode(src, c)?
    } else {
        None
    };
    sensor.map(finish).transpose()
}

/// IFD0 `Orientation` (normal when absent or unreadable).
pub(crate) fn orientation(src: &dyn Source, c: &Container) -> Orientation {
    c.ifd0()
        .uint(src, tag::ORIENTATION)
        .ok()
        .flatten()
        .map_or(Orientation::Normal, |v| Orientation::from_exif(v as u16))
}

/// Black levels by 2×2 position → ds-core R, G, B, G2 (G on the red rows).
pub(crate) fn black_by_color(pos: [f32; 4], pattern: CfaPattern) -> [f32; 4] {
    if pattern == CfaPattern::LinearRgb {
        return [pos[0], pos[1], pos[2], pos[1]];
    }
    let mut out = [0.0; 4];
    for y in 0..2u32 {
        for x in 0..2u32 {
            let slot = match pattern.color_at(x, y) {
                Some(0) => 0,
                Some(2) => 2,
                // Green: G if this row holds red, G2 otherwise.
                _ if pattern.color_at(0, y) == Some(0) || pattern.color_at(1, y) == Some(0) => 1,
                _ => 3,
            };
            out[slot] = pos[(y * 2 + x) as usize];
        }
    }
    out
}

/// Crop, map levels and resolve colour: the [`RawImage`] for `s`.
pub(crate) fn finish(s: Sensor) -> Result<(RawImage, DecodeInfo), RawError> {
    let Sensor {
        make,
        model,
        width: fw,
        height: fh,
        cpp,
        mut data,
        cfa,
        black,
        white,
        crop,
        wb,
        matrix,
        orientation,
    } = s;
    if data.len() != fw * fh * cpp || fw < 2 || fh < 2 {
        return Err(RawError::Malformed(format!(
            "decoded {fw}x{fh}x{cpp} frame has {} samples",
            data.len()
        )));
    }
    let unsupported = |reason: &str| RawError::Unsupported {
        make: make.clone(),
        model: model.clone(),
        reason: reason.into(),
    };
    let (matrix, illuminant) = match matrix {
        Some(m) => m,
        None => cameras::matrix(&make, &model)
            .ok_or_else(|| unsupported("no colour matrix for this camera"))?,
    };
    let cam_to_xyz =
        color::cam_to_xyz(&matrix).ok_or_else(|| unsupported("degenerate color matrix"))?;
    let wb_daylight = color::daylight_multipliers(&matrix)
        .ok_or_else(|| unsupported("degenerate color matrix"))?;
    let wb_as_shot = color::normalize_wb(wb[0], wb[1], f32::NAN, wb[2]).unwrap_or(wb_daylight);

    let [cx, cy, w, h] = crop
        .filter(|&[x, y, w, h]| w >= 2 && h >= 2 && x + w <= fw && y + h <= fh)
        .unwrap_or([0, 0, fw, fh]);
    if [cx, cy, w, h] != [0, 0, fw, fh] {
        // In place: row y's destination never reaches a later row's source.
        let (row, frow) = (w * cpp, fw * cpp);
        for y in 0..h {
            let from = (cy + y) * frow + cx * cpp;
            data.copy_within(from..from + row, y * row);
        }
        data.truncate(w * h * cpp);
    }
    let img = RawImage {
        size: Size::new(w as u32, h as u32),
        cfa: cfa.shifted(cx as u32, cy as u32),
        data,
        black_level: black_by_color(black, cfa),
        white_level: white,
        wb_as_shot,
        wb_daylight,
        cam_to_xyz,
        orientation,
        default_crop: None,
    };
    img.validate()?;
    let info = DecodeInfo {
        path: DecodePath::Native,
        matrix_illuminant: illuminant,
    };
    Ok((img, info))
}

/// Crop `[x, y, w, h]` from margins `[left, top, right, bottom]` of a frame.
pub(crate) fn from_margins(m: [usize; 4], fw: usize, fh: usize) -> Option<[usize; 4]> {
    let w = fw.checked_sub(m[0] + m[2])?;
    let h = fh.checked_sub(m[1] + m[3])?;
    Some([m[0], m[1], w, h])
}

/// Strips or tiles of an image, as TIFF lays them out.
#[derive(Debug, Clone)]
pub(crate) struct Blocks {
    /// Pixel size of one block (strips: image width × `RowsPerStrip`).
    pub bw: usize,
    pub bh: usize,
    /// Blocks per row of blocks.
    pub across: usize,
    /// `(offset, length)` of each block, in raster order.
    pub spans: Vec<(u64, u64)>,
}

/// Most strips/tiles read for one image.
const MAX_BLOCKS: u64 = 1 << 16;

impl Blocks {
    /// The strips or tiles of `ifd` for a `w × h` image, checked against the file.
    pub fn of(src: &dyn Source, ifd: &Ifd, w: usize, h: usize) -> Result<Self, RawError> {
        let tiled = ifd.get(tag::TILE_OFFSETS).is_some();
        let (offs_tag, lens_tag) = if tiled {
            (tag::TILE_OFFSETS, tag::TILE_BYTE_COUNTS)
        } else {
            (tag::STRIP_OFFSETS, tag::STRIP_BYTE_COUNTS)
        };
        let (bw, bh) = if tiled {
            let (Some(tw), Some(th)) = (
                ifd.uint(src, tag::TILE_WIDTH)?,
                ifd.uint(src, tag::TILE_LENGTH)?,
            ) else {
                return Err(RawError::Malformed("tiles without a tile size".into()));
            };
            (tw as usize, th as usize)
        } else {
            let rps = ifd
                .uint(src, tag::ROWS_PER_STRIP)?
                .map_or(h, |r| r as usize);
            (w, rps.clamp(1, h.max(1)))
        };
        if bw == 0 || bh == 0 {
            return Err(RawError::Malformed("zero block size".into()));
        }
        let across = w.div_ceil(bw);
        let need = across * h.div_ceil(bh);
        let (Some(offs), Some(lens)) = (
            ifd.uints_n(src, offs_tag, MAX_BLOCKS)?,
            ifd.uints_n(src, lens_tag, MAX_BLOCKS)?,
        ) else {
            return Err(RawError::Malformed("image without strips or tiles".into()));
        };
        if offs.len() < need || lens.len() < need {
            return Err(RawError::Malformed(format!(
                "{} blocks for a {w}x{h} image of {bw}x{bh} blocks",
                offs.len().min(lens.len())
            )));
        }
        let end = src.end();
        let spans = offs
            .iter()
            .zip(&lens)
            .take(need)
            .map(|(&o, &l)| {
                let (o, l) = (u64::from(o), u64::from(l));
                crate::source::check_range(o, l, 0, end).map(|()| (o, l))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            bw,
            bh,
            across,
            spans,
        })
    }

    /// Decode every block in parallel with `codec(bytes, samples)` and place
    /// it in `out` (`w × h × cpp`, row-major). Strips (and full-width tiles)
    /// decode straight into their rows of `out`, so `samples` may be shorter
    /// than a whole block at the bottom edge; other tiles decode into a
    /// whole-tile buffer (`bw × bh × cpp`) that is clipped to the image.
    pub fn decode<F>(
        &self,
        src: &dyn Source,
        out: &mut [u16],
        w: usize,
        cpp: usize,
        codec: F,
    ) -> Result<(), RawError>
    where
        F: Fn(&[u8], &mut [u16]) -> Result<(), RawError> + Sync,
    {
        let (bw, bh) = (self.bw, self.bh);
        let band = w * bh * cpp;
        if self.across == 1 && bw == w {
            return out
                .par_chunks_mut(band)
                .zip(self.spans.par_iter())
                .try_for_each(|(dst, &(off, len))| codec(&src.read(off, len)?, dst));
        }
        out.par_chunks_mut(band)
            .enumerate()
            .try_for_each(|(by, band_out)| {
                let decoded: Vec<Vec<u16>> = (0..self.across)
                    .into_par_iter()
                    .map(|bx| {
                        let (off, len) = self.spans[by * self.across + bx];
                        let mut buf = vec![0u16; bw * bh * cpp];
                        codec(&src.read(off, len)?, &mut buf)?;
                        Ok(buf)
                    })
                    .collect::<Result<_, RawError>>()?;
                for (bx, buf) in decoded.iter().enumerate() {
                    let x0 = bx * bw;
                    let n = bw.min(w - x0) * cpp;
                    for (r, dst) in band_out.chunks_exact_mut(w * cpp).enumerate() {
                        dst[x0 * cpp..x0 * cpp + n].copy_from_slice(&buf[r * bw * cpp..][..n]);
                    }
                }
                Ok(())
            })
    }
}

/// Rows per parallel task when unpacking uncompressed strips.
const BAND_ROWS: usize = 32;

/// Uncompressed data in `blocks`: strips are read in parallel row bands
/// (positional reads of just those rows), tiles one tile at a time.
pub(crate) fn uncompressed(
    src: &dyn Source,
    blocks: &Blocks,
    layout: unpack::Layout,
    out: &mut [u16],
    w: usize,
    cpp: usize,
) -> Result<(), RawError> {
    if blocks.across > 1 || blocks.bw != w {
        let tile_row = layout.row_bytes(blocks.bw * cpp);
        return blocks.decode(src, out, w, cpp, |bytes, buf| {
            for (r, dst) in buf.chunks_exact_mut(blocks.bw * cpp).enumerate() {
                let row = bytes
                    .get(r * tile_row..(r + 1) * tile_row)
                    .ok_or_else(|| RawError::Truncated("uncompressed tile".into()))?;
                layout.unpack(row, dst);
            }
            Ok(())
        });
    }
    let row_samples = w * cpp;
    let row_bytes = layout.row_bytes(row_samples) as u64;
    let rps = blocks.bh;
    out.par_chunks_mut(row_samples * BAND_ROWS)
        .enumerate()
        .try_for_each(|(band, mut dst)| {
            let mut y = band * BAND_ROWS;
            while !dst.is_empty() {
                let (strip, first) = (y / rps, y % rps);
                let n = (rps - first).min(dst.len() / row_samples);
                let (off, len) = blocks.spans[strip];
                let start = first as u64 * row_bytes;
                if start + n as u64 * row_bytes > len {
                    return Err(RawError::Truncated(format!("strip {strip} is {len} bytes")));
                }
                let bytes = src.read(off + start, n as u64 * row_bytes)?;
                let (rows, rest) = dst.split_at_mut(n * row_samples);
                for (s, d) in bytes
                    .chunks_exact(row_bytes as usize)
                    .zip(rows.chunks_exact_mut(row_samples))
                {
                    layout.unpack(s, d);
                }
                dst = rest;
                y += n;
            }
            Ok(())
        })
}

/// A lossless JPEG block into `out`. A frame larger than `out` (a last strip
/// encoded at full height) keeps what fits; a smaller one is an error.
pub(crate) fn ljpeg_block(bytes: &[u8], out: &mut [u16]) -> Result<(), RawError> {
    let lj = ljpeg::Ljpeg::parse(bytes)?;
    let n = lj.frame.samples();
    if n == out.len() {
        return lj.decode_into(out);
    }
    if n < out.len() {
        return Err(RawError::Malformed(format!(
            "lossless JPEG frame of {n} samples for a block of {}",
            out.len()
        )));
    }
    let mut filled = 0;
    lj.decode_rows(|_, r| {
        let k = r.len().min(out.len() - filled);
        out[filled..filled + k].copy_from_slice(&r[..k]);
        filled += k;
        Ok(())
    })
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn sensor() -> Sensor {
        let (w, h) = (6, 4);
        Sensor {
            make: "SONY".into(),
            model: "ILCE-9".into(),
            width: w,
            height: h,
            cpp: 1,
            data: (0..(w * h) as u16).collect(),
            cfa: CfaPattern::Rggb,
            black: [1.0, 2.0, 3.0, 4.0],
            white: 4000.0,
            crop: Some([1, 1, 4, 2]),
            wb: [2.0, 1.0, 1.5],
            matrix: Some((
                [
                    0.6389, -0.1703, -0.0378, -0.4562, 1.2265, 0.2587, -0.067, 0.1489, 0.655,
                ],
                21,
            )),
            orientation: Orientation::Rotate90,
        }
    }

    #[test]
    fn finish_crops_in_place_and_maps_levels() {
        let (img, info) = finish(sensor()).unwrap();
        assert_eq!(img.size, Size::new(4, 2));
        assert_eq!(img.data, vec![7, 8, 9, 10, 13, 14, 15, 16]);
        assert_eq!(img.cfa, CfaPattern::Bggr);
        assert_eq!(img.black_level, [1.0, 2.0, 4.0, 3.0]);
        assert_eq!(img.wb_as_shot, [2.0, 1.0, 1.5]);
        assert_eq!(img.orientation, Orientation::Rotate90);
        assert_eq!(info.path, DecodePath::Native);
        assert_eq!(info.matrix_illuminant, 21);
        // A crop outside the frame is ignored; unknown WB → daylight.
        let mut s = sensor();
        s.crop = Some([4, 0, 4, 4]);
        s.wb = [f32::NAN; 3];
        let (img, _) = finish(s).unwrap();
        assert_eq!(img.size, Size::new(6, 4));
        assert_eq!(img.wb_as_shot, img.wb_daylight);
    }

    #[test]
    fn finish_rejects_bad_frames_and_unknown_cameras() {
        let mut s = sensor();
        s.data.pop();
        assert!(matches!(finish(s), Err(RawError::Malformed(_))));
        let mut s = sensor();
        s.matrix = None;
        s.make = "NOPE".into();
        assert!(
            matches!(finish(s), Err(RawError::Unsupported { reason, .. }) if reason.contains("colour matrix"))
        );
        let mut s = sensor();
        s.matrix = Some(([0.0; 9], 21));
        assert!(matches!(finish(s), Err(RawError::Unsupported { .. })));
    }

    #[test]
    fn linear_and_gbrg_black_and_margins() {
        assert_eq!(
            black_by_color([1.0, 2.0, 3.0, 9.0], CfaPattern::LinearRgb),
            [1.0, 2.0, 3.0, 2.0]
        );
        // GBRG: row0 = G B, row1 = R G → G2 is (0,0), B (1,0), R (0,1), G (1,1).
        assert_eq!(
            black_by_color([10.0, 20.0, 30.0, 40.0], CfaPattern::Gbrg),
            [30.0, 40.0, 20.0, 10.0]
        );
        assert_eq!(from_margins([1, 2, 3, 4], 10, 10), Some([1, 2, 6, 4]));
        assert_eq!(from_margins([8, 0, 8, 0], 10, 10), None);
    }
}
