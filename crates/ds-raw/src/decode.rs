//! The Sony uncompressed ARW fast path (the native decoders are in `native`).

use ds_core::{CfaPattern, Orientation, RawImage, Size};
use rayon::prelude::*;

use crate::RawError;
use crate::color;
use crate::container::{Container, crop_rect};
use crate::sony;
use crate::source::Source;
use crate::tiff::{Endian, tag};

/// Rows read per parallel task in the fast path (~390 KB of A9 data).
const BAND_ROWS: usize = 32;
/// EXIF `LightSource` code for D65.
pub(crate) const D65_LIGHT_SOURCE: u16 = 21;
/// TIFF `PhotometricInterpretation` for CFA data.
const PHOTOMETRIC_CFA: u32 = 32803;

/// Everything the fast path needs, validated against the file.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub strip_off: u64,
    pub raw_w: u32,
    pub raw_h: u32,
    /// Active area inside the raw frame: x, y, w, h.
    pub crop: [u32; 4],
    /// CFA of the *uncropped* frame at (0, 0).
    pub cfa: CfaPattern,
    /// R, G1, G2, B.
    pub black: [u16; 4],
    pub white: u16,
    pub wb: [f32; 3],
    pub cam_to_xyz: [[f32; 3]; 3],
    pub wb_daylight: [f32; 3],
    /// EXIF `LightSource` code of the matrix used.
    pub illuminant: u16,
    pub orientation: Orientation,
}

/// Map a 2×2 TIFF `CFAPattern` (0 = R, 1 = G, 2 = B) to a Bayer pattern.
pub(crate) fn cfa_from_tag(p: &[u32]) -> Option<CfaPattern> {
    Some(match p {
        [0, 1, 1, 2] => CfaPattern::Rggb,
        [2, 1, 1, 0] => CfaPattern::Bggr,
        [1, 0, 2, 1] => CfaPattern::Grbg,
        [1, 2, 0, 1] => CfaPattern::Gbrg,
        _ => return None,
    })
}

/// Build a fast-path plan for Sony uncompressed ARW: one strip of exactly
/// `w × h` little-endian 16-bit words, one sample per pixel, CFA photometric.
/// `Ok(None)` means "not this layout, use the general decoder".
///
/// `BitsPerSample` is not required to be 16: Sony writes 12 there (the A9
/// included) while storing 14-bit values in 16-bit words. The strip byte
/// count equal to `w × h × 2` is what proves the 16-bit layout; the value
/// range is bounded by the white level.
pub(crate) fn sony_plan(src: &dyn Source, c: &Container) -> Result<Option<Plan>, RawError> {
    if !c.is_sony(src)? || c.tiff.endian != Endian::Little {
        return Ok(None);
    }
    let Some(raw) = c.raw_ifd(src)? else {
        return Ok(None);
    };
    if raw.uint(src, tag::COMPRESSION)? != Some(1)
        || raw.uint(src, tag::SAMPLES_PER_PIXEL)?.unwrap_or(1) != 1
        || raw.uint(src, tag::PHOTOMETRIC)? != Some(PHOTOMETRIC_CFA)
        || !raw
            .uint(src, tag::BITS_PER_SAMPLE)?
            .is_some_and(|b| (8..=16).contains(&b))
    {
        return Ok(None);
    }
    let (Some(w), Some(h), Some(off), Some(count)) = (
        raw.uint(src, tag::IMAGE_WIDTH)?,
        raw.uint(src, tag::IMAGE_LENGTH)?,
        raw.uint(src, tag::STRIP_OFFSETS)?,
        raw.uint(src, tag::STRIP_BYTE_COUNTS)?,
    ) else {
        return Err(RawError::Malformed(
            "raw IFD lacks size or strip tags".into(),
        ));
    };
    if w < 2 || h < 2 {
        return Err(RawError::Malformed(format!("raw size {w}x{h}")));
    }
    let need = u64::from(w) * u64::from(h) * 2;
    if u64::from(count) != need {
        return Ok(None);
    }
    crate::source::check_range(u64::from(off), need, 0, src.end())?;
    let Some(cfa) = raw
        .uints(src, tag::CFA_PATTERN)?
        .as_deref()
        .and_then(cfa_from_tag)
    else {
        return Ok(None);
    };
    let lv = sony::levels(src, &c.tiff, raw)?;
    let (Some(black), Some(white), Some([r, g1, g2, b])) = (lv.black, lv.white, lv.wb) else {
        return Ok(None);
    };
    let wb = color::normalize_wb(r, g1, g2, b)
        .ok_or_else(|| RawError::Malformed(format!("bad WB {:?}", lv.wb)))?;
    if black.iter().any(|&b| b >= white) {
        return Err(RawError::Malformed(format!(
            "black {black:?} not below white {white}"
        )));
    }
    let Some((matrix, illuminant)) = crate::cameras::matrix(&c.make(src)?, &c.model(src)?) else {
        return Ok(None); // the general decoders report the unsupported camera
    };
    let cam_to_xyz = color::cam_to_xyz(&matrix)
        .ok_or_else(|| RawError::Malformed("degenerate color matrix".into()))?;
    let wb_daylight = color::daylight_multipliers(&matrix)
        .ok_or_else(|| RawError::Malformed("degenerate color matrix".into()))?;
    let orientation = c
        .ifd0()
        .uint(src, tag::ORIENTATION)
        .ok()
        .flatten()
        .map_or(Orientation::Normal, |v| Orientation::from_exif(v as u16));
    Ok(Some(Plan {
        strip_off: u64::from(off),
        raw_w: w,
        raw_h: h,
        crop: crop_rect(src, raw, w, h)?,
        cfa,
        black,
        white,
        wb,
        cam_to_xyz,
        wb_daylight,
        illuminant,
        orientation,
    }))
}

/// White level our own parse trusts for a Sony file that goes through
/// rawler: the DNG `WhiteLevel` (0xC61D) in the raw IFD, which tracks where
/// the data actually clips (16380/16383 on the A9; rawler reads 0x787F =
/// 15360 and would clip ~0.09 EV early). `None` for non-Sony files or when
/// the tag is absent (rawler's own value is kept, e.g. for 12-bit bodies
/// where it rescales 0x787F itself).
#[cfg(feature = "rawler")]
pub(crate) fn sony_white_override(src: &dyn Source, c: &Container) -> Option<f32> {
    if !c.is_sony(src).ok()? {
        return None;
    }
    let raw = c.raw_ifd(src).ok()??;
    let w = raw.uint(src, tag::WHITE_LEVEL).ok()??;
    u16::try_from(w).ok().filter(|&w| w > 0).map(f32::from)
}

/// Per-color black (R, G1, G2, B) → ds-core order (R, G, B, G2).
fn black_rgbg2(b: [u16; 4]) -> [f32; 4] {
    [b[0], b[1], b[3], b[2]].map(f32::from)
}

/// Execute a plan: parallel banded positional reads + a tight LE unpack.
pub(crate) fn run_plan(src: &dyn Source, p: &Plan) -> Result<RawImage, RawError> {
    let [cx, cy, w, h] = p.crop.map(|v| v as usize);
    let stride = p.raw_w as usize * 2;
    let mut data = vec![0u16; w * h];
    data.par_chunks_mut(w * BAND_ROWS)
        .enumerate()
        .try_for_each_init(Vec::new, |buf: &mut Vec<u8>, (band, out)| {
            let rows = out.len() / w;
            let y0 = cy + band * BAND_ROWS;
            buf.resize(rows * stride, 0);
            src.read_into(p.strip_off + (y0 * stride) as u64, buf)?;
            for (row, dst) in buf.chunks_exact(stride).zip(out.chunks_exact_mut(w)) {
                let s = &row[cx * 2..(cx + w) * 2];
                for (d, px) in dst.iter_mut().zip(s.as_chunks::<2>().0) {
                    *d = u16::from_le_bytes(*px);
                }
            }
            Ok::<(), RawError>(())
        })?;
    let img = RawImage {
        size: Size::new(w as u32, h as u32),
        cfa: p.cfa.shifted(cx as u32, cy as u32),
        data,
        black_level: black_rgbg2(p.black),
        white_level: f32::from(p.white),
        wb_as_shot: p.wb,
        cam_to_xyz: p.cam_to_xyz,
        wb_daylight: p.wb_daylight,
        orientation: p.orientation,
        default_crop: None,
    };
    img.validate()?;
    Ok(img)
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // tests compare exactly-representable values
mod tests {
    use super::*;

    #[test]
    fn cfa_tag_mapping() {
        assert_eq!(cfa_from_tag(&[0, 1, 1, 2]), Some(CfaPattern::Rggb));
        assert_eq!(cfa_from_tag(&[2, 1, 1, 0]), Some(CfaPattern::Bggr));
        assert_eq!(cfa_from_tag(&[1, 0, 2, 1]), Some(CfaPattern::Grbg));
        assert_eq!(cfa_from_tag(&[1, 2, 0, 1]), Some(CfaPattern::Gbrg));
        assert_eq!(cfa_from_tag(&[0, 1, 2]), None);
        assert_eq!(cfa_from_tag(&[3, 1, 1, 2]), None);
    }

    #[test]
    fn black_reorders_to_rgbg2() {
        assert_eq!(black_rgbg2([1, 2, 3, 4]), [1.0, 2.0, 4.0, 3.0]);
    }
}
