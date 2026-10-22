//! The general path through `rawler`, for every format the fast paths skip.
//!
//! rawler indexes buffers with header-derived offsets in places, so every
//! call is wrapped in `catch_unwind`: a malformed file must be an error,
//! never a crash of the app.

use ds_core::{CfaPattern, Orientation, RawImage, Size};
use rawler::RawlerError;
use rawler::decoders::RawDecodeParams;
use rawler::rawimage::RawPhotometricInterpretation;
use rawler::rawsource::RawSource;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::RawError;
use crate::cameras::matrix_from_map;
use crate::color;
use crate::meta::{CaptureTime, ImageMeta, Rational, pick_iso};

fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, RawlerError>) -> Result<T, RawError> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(map_err(e)),
        Err(_) => Err(RawError::Decode(format!("rawler panicked during {what}"))),
    }
}

/// How rawler's NEF decoder starts its message for `HighEfficency` (sic) and
/// `HighEfficencyStar` compression.
const NIKON_HE: &str = "NEF compression Some(HighEfficency";

fn map_err(e: RawlerError) -> RawError {
    match e {
        RawlerError::Unsupported {
            what, model, make, ..
        } => RawError::Unsupported {
            make,
            model,
            reason: what,
        },
        // rawler reports Nikon's High Efficiency codecs, which it lacks, as a
        // failure; those are valid files Darkslide can't develop yet, not
        // damaged ones. Only that exact message: rawler says "not supported"
        // about corrupt data too (an LJPEG predictor, a CRX plane count).
        RawlerError::DecoderFailed(s) if s.starts_with(NIKON_HE) => RawError::Unsupported {
            make: "Nikon".into(),
            model: String::new(),
            reason: "High Efficiency (HE/HE*) compression isn't supported by the RAW decoder yet"
                .into(),
        },
        RawlerError::DecoderFailed(s) => RawError::Decode(s),
    }
}

/// Metadata via rawler (non-TIFF containers such as CR3/RAF). Decoder
/// construction only parses headers; no pixel buffers are allocated here.
pub(crate) fn read_metadata(src: &RawSource) -> Result<ImageMeta, RawError> {
    let md = guarded("metadata", || {
        let dec = rawler::get_decoder(src)?;
        dec.raw_metadata(src, &RawDecodeParams::default())
    })?;
    let e = &md.exif;
    let shutter = e
        .exposure_time
        .map(|r| Rational { num: r.n, den: r.d })
        .filter(|r| r.num > 0 && r.den > 0);
    let ratio = |r: Option<rawler::formats::tiff::Rational>| {
        r.filter(|r| r.d != 0 && r.n != 0)
            .map(|r| (f64::from(r.n) / f64::from(r.d)) as f32)
    };
    Ok(ImageMeta {
        make: md.make.clone(),
        model: md.model.clone(),
        lens: e.lens_model.clone().filter(|s| !s.is_empty()),
        iso: pick_iso(
            e.iso_speed_ratings.map(u32::from),
            e.sensitivity_type.is_some(),
            e.recommended_exposure_index,
            e.iso_speed,
        ),
        shutter,
        shutter_display: shutter.map(Rational::shutter_display),
        aperture: ratio(e.fnumber),
        focal_length: ratio(e.focal_length),
        focal_length_35mm: None,
        capture_time: e.date_time_original.as_deref().and_then(|dt| {
            CaptureTime::parse(
                dt,
                e.sub_sec_time_original.as_deref(),
                e.offset_time_original.as_deref(),
            )
        }),
        orientation: Orientation::from_exif(e.orientation.unwrap_or(1)),
        dimensions: None,
        serial: e.serial_number.clone().filter(|s| !s.is_empty()),
        artist: e.artist.clone().filter(|s| !s.is_empty()),
        copyright: e.copyright.clone().filter(|s| !s.is_empty()),
        ..ImageMeta::default()
    })
}

/// Full decode via rawler, converted to the ds-core contract.
///
/// Callers must have bounded the file's declared dimensions first
/// (`guard`): rawler allocates from header values before reading data.
/// `orientation` comes from our own EXIF read (rawler leaves it `Normal`
/// for ARW); `white_override` replaces rawler's white level when our own
/// parse knows better (Sony: DNG `WhiteLevel` 0xC61D). Returns the image
/// and the EXIF `LightSource` code of the color matrix used (21 = D65).
pub(crate) fn decode(
    src: &RawSource,
    orientation: Option<Orientation>,
    white_override: Option<f32>,
) -> Result<(RawImage, u16), RawError> {
    let img = guarded("decode", || {
        rawler::decode(src, &RawDecodeParams::default())
    })?;
    let orientation = match orientation {
        Some(o) => o,
        None => read_metadata(src).map_or(Orientation::Normal, |m| m.orientation),
    };
    convert(img, orientation, white_override)
}

/// Convert a rawler image: crop to the recommended area, shift the CFA,
/// reorder levels, normalize WB and derive `cam_to_xyz`.
pub(crate) fn convert(
    mut img: rawler::RawImage,
    orientation: Orientation,
    white_override: Option<f32>,
) -> Result<(RawImage, u16), RawError> {
    let unsupported = |img: &rawler::RawImage, reason: &str| RawError::Unsupported {
        make: img.make.clone(),
        model: img.model.clone(),
        reason: reason.into(),
    };
    img.data.force_integer();
    let rawler::RawImageData::Integer(full) = &img.data else {
        return Err(RawError::Decode("rawler returned non-integer data".into()));
    };
    let (fw, fh, cpp) = (img.width, img.height, img.cpp);
    if full.len() != fw * fh * cpp || fw < 2 || fh < 2 {
        return Err(RawError::Malformed(format!(
            "rawler image {fw}x{fh}x{cpp} has {} samples",
            full.len()
        )));
    }
    let (full_cfa, cpp_ok) = match &img.photometric {
        RawPhotometricInterpretation::Cfa(cfg) if cpp == 1 => (bayer(&cfg.cfa), true),
        RawPhotometricInterpretation::LinearRaw if cpp == 3 => (Some(CfaPattern::LinearRgb), true),
        _ => (None, false),
    };
    let Some(full_cfa) = full_cfa.filter(|_| cpp_ok) else {
        return Err(unsupported(
            &img,
            "only 2x2 Bayer or linear RGB sensors are supported",
        ));
    };

    let rect = img
        .crop_area
        .or(img.active_area)
        .map(|r| [r.p.x, r.p.y, r.d.w, r.d.h])
        .filter(|&[x, y, w, h]| w >= 2 && h >= 2 && x + w <= fw && y + h <= fh)
        .unwrap_or([0, 0, fw, fh]);
    let [cx, cy, w, h] = rect;
    let mut data = Vec::with_capacity(w * h * cpp);
    for row in full[cy * fw * cpp..].chunks_exact(fw * cpp).take(h) {
        data.extend_from_slice(&row[cx * cpp..(cx + w) * cpp]);
    }

    let black_level = black_levels(&img.blacklevel, full_cfa);
    let white_level = white_override.unwrap_or_else(|| {
        img.whitelevel
            .0
            .first()
            .copied()
            .unwrap_or(u32::from(u16::MAX)) as f32
    });
    let (matrix, illuminant) =
        matrix_from_map(&img.color_matrix).ok_or_else(|| unsupported(&img, "no color matrix"))?;
    let cam_to_xyz =
        color::cam_to_xyz(&matrix).ok_or_else(|| unsupported(&img, "degenerate color matrix"))?;
    let wb_daylight = color::daylight_multipliers(&matrix)
        .ok_or_else(|| unsupported(&img, "degenerate color matrix"))?;
    let [r, g, b, _] = img.wb_coeffs;
    let wb_as_shot = color::normalize_wb(r, g, f32::NAN, b).unwrap_or(wb_daylight);

    let out = RawImage {
        size: Size::new(w as u32, h as u32),
        cfa: full_cfa.shifted(cx as u32, cy as u32),
        data,
        black_level,
        white_level,
        wb_as_shot,
        wb_daylight,
        cam_to_xyz,
        orientation,
        default_crop: None,
    };
    out.validate()?;
    Ok((out, illuminant))
}

fn bayer(cfa: &rawler::CFA) -> Option<CfaPattern> {
    if cfa.width != 2 || cfa.height != 2 {
        return None;
    }
    let p: Vec<u32> = [(0, 0), (0, 1), (1, 0), (1, 1)]
        .iter()
        .map(|&(r, c)| cfa.color_at(r, c) as u32)
        .collect();
    crate::decode::cfa_from_tag(&p)
}

/// rawler black levels (positional within the CFA repeat, or one value)
/// → ds-core R, G, B, G2 (G on the red rows, G2 on the blue rows).
fn black_levels(bl: &rawler::rawimage::BlackLevel, pattern: CfaPattern) -> [f32; 4] {
    let v = bl.as_vec();
    if v.is_empty() {
        return [0.0; 4];
    }
    if pattern == CfaPattern::LinearRgb || v.len() != 4 || bl.width != 2 || bl.height != 2 {
        let at = |i: usize| v.get(i).copied().unwrap_or(v[0]);
        return if pattern == CfaPattern::LinearRgb {
            [at(0), at(1), at(2), at(1)]
        } else {
            [v[0]; 4]
        };
    }
    let mut out = [0.0; 4];
    for y in 0..2u32 {
        for x in 0..2u32 {
            let lvl = v[(y * 2 + x) as usize];
            let slot = match pattern.color_at(x, y) {
                Some(0) => 0,
                Some(2) => 2,
                // Green: G if this row holds red, G2 otherwise.
                _ if pattern.color_at(0, y) == Some(0) || pattern.color_at(1, y) == Some(0) => 1,
                _ => 3,
            };
            out[slot] = lvl;
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // tests compare exactly-representable values
mod tests {
    use super::*;
    use rawler::rawimage::BlackLevel;

    #[test]
    fn missing_codecs_are_unsupported_not_damage() {
        for he in ["HighEfficency", "HighEfficencyStar"] {
            let e = map_err(RawlerError::DecoderFailed(format!(
                "NEF compression Some({he}) is not supported"
            )));
            assert!(
                matches!(&e, RawError::Unsupported { reason, .. } if reason.contains("HE/HE*")),
                "{e:?}"
            );
            assert_eq!(
                e.to_string(),
                "unsupported camera Nikon: High Efficiency (HE/HE*) compression isn't \
                 supported by the RAW decoder yet"
            );
        }
        // rawler's "not supported" about corrupt streams stays damage (the review's cases).
        for s in [
            "stream ended early",
            "ljpeg: predictor 9 not supported",
            "Creating LJPEG decompressor for ARW LJPEG tile (0,0) failed: ljpeg: More than 16 \
             bits per channel is not supported.",
            "Failed to decode raw: General error: Plane configration 3 is not supported",
            "Failed to decode raw: General error: Tile column count 300 is not supported",
            "NEF compression None is not supported",
        ] {
            let e = map_err(RawlerError::DecoderFailed(s.into()));
            assert!(matches!(e, RawError::Decode(_)), "{s}: {e:?}");
        }
    }

    #[test]
    fn black_levels_map_positions_to_colors() {
        let bl = BlackLevel::new(&[10u16, 20, 30, 40], 2, 2, 1);
        assert_eq!(
            black_levels(&bl, CfaPattern::Rggb),
            [10.0, 20.0, 40.0, 30.0]
        );
        // GBRG: row0 = G B, row1 = R G → G2 is (0,0), B (1,0), R (0,1), G (1,1).
        assert_eq!(
            black_levels(&bl, CfaPattern::Gbrg),
            [30.0, 40.0, 20.0, 10.0]
        );
        let single = BlackLevel::new(&[512u16], 1, 1, 1);
        assert_eq!(black_levels(&single, CfaPattern::Rggb), [512.0; 4]);
        let rgb = BlackLevel::new(&[1u16, 2, 3], 1, 1, 3);
        assert_eq!(
            black_levels(&rgb, CfaPattern::LinearRgb),
            [1.0, 2.0, 3.0, 2.0]
        );
        let one = BlackLevel::new(&[7u16], 1, 1, 1);
        assert_eq!(black_levels(&one, CfaPattern::LinearRgb), [7.0; 4]);
        let empty = BlackLevel {
            levels: vec![],
            cpp: 1,
            width: 1,
            height: 1,
        };
        assert_eq!(black_levels(&empty, CfaPattern::Rggb), [0.0; 4]);
    }

    #[test]
    fn bayer_from_rawler_cfa() {
        assert_eq!(bayer(&rawler::CFA::new("RGGB")), Some(CfaPattern::Rggb));
        assert_eq!(bayer(&rawler::CFA::new("GBRG")), Some(CfaPattern::Gbrg));
        assert_eq!(bayer(&rawler::CFA::new("")), None);
        assert_eq!(
            bayer(&rawler::CFA::new("RGBGGGRGBGGGRGBGGGRGBGGGRGBGGGRGBGGG")),
            None
        );
    }

    #[test]
    fn errors_map_and_panics_are_caught() {
        let e = guarded::<()>("t", || Err(RawlerError::DecoderFailed("boom".into()))).unwrap_err();
        assert!(matches!(e, RawError::Decode(s) if s == "boom"));
        let e = guarded::<()>("t", || {
            Err(RawlerError::Unsupported {
                what: "w".into(),
                model: "M".into(),
                make: "K".into(),
                mode: String::new(),
            })
        })
        .unwrap_err();
        assert!(
            matches!(e, RawError::Unsupported { make, model, reason } if make == "K" && model == "M" && reason == "w")
        );
        let e = guarded::<()>("t", || panic!("rawler bug")).unwrap_err();
        assert!(matches!(e, RawError::Decode(s) if s.contains("panicked")));
        assert_eq!(guarded("t", || Ok(5)).unwrap(), 5);
    }

    /// rawler `LinearRaw` (cpp = 3, e.g. linear DNG / Sony YCbCr modes):
    /// cropped with 3 samples per pixel, per-channel black, WB forced to 1.
    #[test]
    fn convert_linear_rgb() {
        use rawler::imgop::{Dim2, Point, Rect};
        use rawler::pixarray::PixU16;
        use rawler::rawimage::WhiteLevel;
        let cam = rawler::global_loader()
            .get_cameras()
            .get(&("SONY".to_string(), "ILCE-9".to_string(), String::new()))
            .unwrap()
            .clone();
        let (w, h) = (4usize, 3usize);
        let data: Vec<u16> = (0..(w * h * 3) as u16).map(|v| 100 + v).collect();
        let mut img = rawler::RawImage::new(
            cam,
            PixU16::new_with(data, w * 3, h),
            3,
            [1.0, 1.0, 1.0, f32::NAN],
            RawPhotometricInterpretation::LinearRaw,
            Some(BlackLevel::new(&[10u16, 20, 30], 1, 1, 3)),
            Some(WhiteLevel(vec![4000; 3])),
            false,
        );
        img.crop_area = Some(Rect::new(Point::new(1, 1), Dim2::new(2, 2)));
        let (out, ill) = convert(img, Orientation::Rotate90, None).unwrap();
        assert_eq!(ill, crate::decode::D65_LIGHT_SOURCE);
        assert_eq!(out.cfa, CfaPattern::LinearRgb);
        assert_eq!(out.size, Size::new(2, 2));
        // Row 1, pixels 1..3 → sample indices (1*4 + 1)*3 .. +6.
        let start = (w + 1) * 3;
        let expect: Vec<u16> = (start..start + 6)
            .chain(start + w * 3..start + w * 3 + 6)
            .map(|i| 100 + i as u16)
            .collect();
        assert_eq!(out.data, expect);
        assert_eq!(out.black_level, [10.0, 20.0, 30.0, 20.0]);
        assert_eq!(out.white_level, 4000.0);
        assert_eq!(out.wb_as_shot, [1.0, 1.0, 1.0]);
        assert_eq!(out.orientation, Orientation::Rotate90);
    }

    /// Unusable WB coefficients fall back to the matrix's daylight multipliers;
    /// a cpp that matches no photometric interpretation is unsupported.
    #[test]
    fn convert_wb_fallback_and_bad_layouts() {
        use rawler::pixarray::PixU16;
        use rawler::rawimage::{CFAConfig, WhiteLevel};
        let cam = rawler::global_loader()
            .get_cameras()
            .get(&("SONY".to_string(), "ILCE-9".to_string(), String::new()))
            .unwrap()
            .clone();
        let mk = |cpp: usize, photometric: RawPhotometricInterpretation| {
            rawler::RawImage::new(
                cam.clone(),
                PixU16::new_with(vec![600u16; 4 * 4 * cpp], 4 * cpp, 4),
                cpp,
                [f32::NAN; 4],
                photometric,
                Some(BlackLevel::new(&[512u16], 1, 1, 1)),
                Some(WhiteLevel(vec![16383])),
                false,
            )
        };
        let cfa = RawPhotometricInterpretation::Cfa(CFAConfig::new_from_camera(&cam));
        let (out, _) = convert(mk(1, cfa.clone()), Orientation::Normal, None).unwrap();
        let day =
            color::daylight_multipliers(&matrix_from_map(&cam.color_matrix).unwrap().0).unwrap();
        assert_eq!(out.wb_as_shot, day);
        assert_eq!(out.black_level, [512.0; 4]);
        assert!(matches!(
            convert(mk(3, cfa), Orientation::Normal, None),
            Err(RawError::Unsupported { .. })
        ));
    }
}
