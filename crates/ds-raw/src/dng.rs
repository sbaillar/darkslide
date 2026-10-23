//! Linear DNG writer, for images Darkslide makes (HDR merges, panoramas): demosaiced
//! camera RGB (`LinearRaw`, 16 bits per sample, lossless JPEG) with the camera's colour
//! matrix and as-shot neutral, so the file reads back through [`crate::decode`] as the
//! same [`RawImage`] and develops like any RAW.
//!
//! The writer is rawler's: in process with the `rawler` feature, else the rawler
//! helper's ([`crate::helper`]); without either [`write_linear_dng`] returns
//! [`RawError::Unsupported`]. Replacing it is phase 2 of the native decoder work
//! (`docs/RAWDEC.md`).

#[cfg(feature = "rawler")]
use std::collections::HashMap;
use std::io::{Seek, Write};
use std::path::Path;

#[cfg(feature = "rawler")]
use ds_core::CfaPattern;
use ds_core::RawImage;
#[cfg(feature = "rawler")]
use rawler::decoders::{Camera, RawDecodeParams};
#[cfg(feature = "rawler")]
use rawler::dng::writer::DngWriter;
#[cfg(feature = "rawler")]
use rawler::dng::{CropMode, DngCompression, DngPhotometricConversion};
#[cfg(feature = "rawler")]
use rawler::formats::tiff::CompressionMethod;
#[cfg(feature = "rawler")]
use rawler::imgop::xyz::Illuminant;
#[cfg(feature = "rawler")]
use rawler::pixarray::PixU16;
#[cfg(feature = "rawler")]
use rawler::rawimage::{BlackLevel, RawPhotometricInterpretation, WhiteLevel};
#[cfg(feature = "rawler")]
use rawler::tags::{DngTag, TiffCommonTag};

use crate::RawError;

#[cfg(feature = "rawler")]
const NEW_SUBFILE_PREVIEW: u32 = 1;
#[cfg(feature = "rawler")]
const PHOTOMETRIC_YCBCR: u16 = 6;
#[cfg(feature = "rawler")]
const TAG_JPEG_OFFSET: u16 = 0x0201;
#[cfg(feature = "rawler")]
const TAG_JPEG_LENGTH: u16 = 0x0202;
#[cfg(feature = "rawler")]
const TAG_SOFTWARE: u16 = 0x0131;
/// DNG `PreviewColorSpace`: sRGB.
#[cfg(feature = "rawler")]
const PREVIEW_SRGB: u32 = 2;

/// What goes into a written DNG besides the pixels.
#[derive(Debug, Clone, Copy, Default)]
pub struct DngMeta<'a> {
    pub make: &'a str,
    pub model: &'a str,
    /// A RAW whose EXIF (capture time, exposure, lens, …) the DNG carries.
    pub exif_from: Option<&'a Path>,
    /// An sRGB JPEG preview and its pixel size (as stored, before orientation).
    pub preview: Option<(&'a [u8], u32, u32)>,
    pub software: &'a str,
}

#[cfg(feature = "rawler")]
fn err(e: impl std::fmt::Display) -> RawError {
    RawError::Decode(format!("DNG write failed: {e}"))
}

#[cfg(feature = "rawler")]
fn inverse(m: &[[f32; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let a = m.map(|r| r.map(f64::from));
    let c =
        |r0: usize, r1: usize, c0: usize, c1: usize| a[r0][c0] * a[r1][c1] - a[r0][c1] * a[r1][c0];
    let det = a[0][0] * c(1, 2, 1, 2) - a[0][1] * c(1, 2, 0, 2) + a[0][2] * c(1, 2, 0, 1);
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    Some([
        [
            c(1, 2, 1, 2) / det,
            -c(0, 2, 1, 2) / det,
            c(0, 1, 1, 2) / det,
        ],
        [
            -c(1, 2, 0, 2) / det,
            c(0, 2, 0, 2) / det,
            -c(0, 1, 0, 2) / det,
        ],
        [
            c(1, 2, 0, 1) / det,
            -c(0, 2, 0, 1) / det,
            c(0, 1, 0, 1) / det,
        ],
    ])
}

/// The Adobe `ColorMatrix` (XYZ → camera, D65) that reads back as `img`'s `cam_to_xyz`
/// and daylight multipliers: `diag(1 / wb_daylight) · cam_to_xyz⁻¹` (see [`crate::color`]).
#[cfg(feature = "rawler")]
fn color_matrix(img: &RawImage) -> Option<Vec<f32>> {
    let inv = inverse(&img.cam_to_xyz)?;
    Some(
        (0..9)
            .map(|i| (inv[i / 3][i % 3] / f64::from(img.wb_daylight[i / 3])) as f32)
            .collect(),
    )
}

/// Write `img` (a [`CfaPattern::LinearRgb`] raw) as a linear DNG to `out`.
/// Takes the image by value: its samples move into the writer (no copy of a merged
/// frame that may be hundreds of MB).
///
/// Without the `rawler` feature the rawler helper writes it (and the file is copied
/// to `out`); without a helper either, this is [`RawError::Unsupported`].
#[cfg(not(feature = "rawler"))]
#[allow(clippy::needless_pass_by_value)] // the rawler build's signature, which moves the samples
pub fn write_linear_dng<W: Write + Seek>(
    mut out: W,
    img: RawImage,
    meta: &DngMeta<'_>,
) -> Result<(), RawError> {
    let Some(h) = crate::helper::client()? else {
        return Err(RawError::Unsupported {
            make: String::new(),
            model: String::new(),
            reason: format!("writing DNG {}", crate::helper::NOT_INSTALLED),
        });
    };
    img.validate()?;
    out.write_all(&crate::helper::write_dng(&h, &img, meta)?)?;
    Ok(())
}

/// Write `img` (a [`CfaPattern::LinearRgb`] raw) as a linear DNG to `out`.
/// Takes the image by value: its samples move into the writer (no copy of a merged
/// frame that may be hundreds of MB).
#[cfg(feature = "rawler")]
pub fn write_linear_dng<W: Write + Seek>(
    out: W,
    img: RawImage,
    meta: &DngMeta<'_>,
) -> Result<(), RawError> {
    img.validate()?;
    if img.cfa != CfaPattern::LinearRgb {
        return Err(RawError::Malformed(
            "only linear RGB images are written as DNG".into(),
        ));
    }
    let matrix =
        color_matrix(&img).ok_or_else(|| RawError::Malformed("degenerate colour matrix".into()))?;
    let (w, h) = (img.size.width as usize, img.size.height as usize);
    let cam = Camera {
        make: meta.make.to_owned(),
        model: meta.model.to_owned(),
        clean_make: meta.make.to_owned(),
        clean_model: meta.model.to_owned(),
        color_matrix: HashMap::from([(Illuminant::D65, matrix)]),
        ..Camera::new()
    };
    let black = BlackLevel::new(
        &img.black_level[..3]
            .iter()
            .map(|b| b.round() as u16)
            .collect::<Vec<_>>(),
        1,
        1,
        3,
    );
    let white = WhiteLevel(vec![img.white_level.round() as u32; 3]);
    let [r, g, b] = img.wb_as_shot;
    let raw = rawler::RawImage::new(
        cam,
        PixU16::new_with(img.data, w * 3, h),
        3,
        [r, g, b, f32::NAN],
        RawPhotometricInterpretation::LinearRaw,
        Some(black),
        Some(white),
        false,
    );

    let mut dng = DngWriter::new(out, [1, 4, 0, 0]).map_err(err)?;
    dng.load_base_tags(&raw).map_err(err)?;
    if let Some(src) = meta.exif_from
        && let Some(md) = source_metadata(src)
    {
        dng.load_metadata(&md).map_err(err)?;
    }
    let root = dng.root_ifd_mut();
    root.add_tag(TiffCommonTag::Orientation, img.orientation.to_exif());
    if !meta.software.is_empty() {
        root.add_untyped_tag(TAG_SOFTWARE, meta.software);
    }
    if let Some((jpeg, pw, ph)) = meta.preview {
        let off = dng.dng.write_data(jpeg).map_err(err)?;
        let len = u32::try_from(jpeg.len()).map_err(err)?;
        let mut sub = dng.subframe(NEW_SUBFILE_PREVIEW);
        let ifd = sub.ifd_mut();
        ifd.add_tag(TiffCommonTag::ImageWidth, pw);
        ifd.add_tag(TiffCommonTag::ImageLength, ph);
        ifd.add_tag(TiffCommonTag::Compression, CompressionMethod::ModernJPEG);
        ifd.add_tag(TiffCommonTag::PhotometricInt, PHOTOMETRIC_YCBCR);
        ifd.add_tag(TiffCommonTag::SamplesPerPixel, 3_u16);
        ifd.add_tag(TiffCommonTag::BitsPerSample, [8_u16, 8, 8]);
        ifd.add_tag(TiffCommonTag::StripOffsets, off);
        ifd.add_tag(TiffCommonTag::StripByteCounts, len);
        ifd.add_tag(TiffCommonTag::RowsPerStrip, ph);
        ifd.add_untyped_tag(TAG_JPEG_OFFSET, off);
        ifd.add_untyped_tag(TAG_JPEG_LENGTH, len);
        ifd.add_tag(DngTag::PreviewColorSpace, PREVIEW_SRGB);
        sub.finalize().map_err(err)?;
    }
    let mut main = dng.subframe_on_root(0);
    main.raw_image(
        &raw,
        CropMode::None,
        DngCompression::Lossless,
        DngPhotometricConversion::Original,
        1,
    )
    .map_err(err)?;
    main.finalize().map_err(err)?;
    dng.close().map_err(err)
}

/// rawler's metadata of the RAW at `path` (`None` when it can't be read).
#[cfg(feature = "rawler")]
fn source_metadata(path: &Path) -> Option<rawler::decoders::RawMetadata> {
    // The same header guards as `RawFile::metadata`, before rawler parses it.
    crate::open(path).ok()?.check_header().ok()?;
    std::panic::catch_unwind(|| {
        let src = crate::rawler_source_at(path).ok()?;
        let dec = rawler::get_decoder(&src).ok()?;
        dec.raw_metadata(&src, &RawDecodeParams::default()).ok()
    })
    .ok()
    .flatten()
}

#[cfg(all(test, feature = "rawler"))]
pub(crate) mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;
    use crate::preview::tests::fake_jpeg;
    use ds_core::{Orientation, Size};
    use std::io::Cursor;

    pub(crate) fn linear(w: u32, h: u32) -> RawImage {
        // The A9's colour, from rawler's camera DB.
        let (m, _) = crate::cameras::matrix("SONY", "ILCE-9").unwrap();
        RawImage {
            size: Size::new(w, h),
            cfa: CfaPattern::LinearRgb,
            data: (0..w * h * 3).map(|i| (i * 37 % 65_536) as u16).collect(),
            black_level: [0.0; 4],
            white_level: 65535.0,
            wb_as_shot: [2.1, 1.0, 1.6],
            wb_daylight: crate::color::daylight_multipliers(&m).unwrap(),
            cam_to_xyz: crate::color::cam_to_xyz(&m).unwrap(),
            orientation: Orientation::Rotate90,
            default_crop: None,
        }
    }

    fn write(img: &RawImage, meta: &DngMeta<'_>) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        write_linear_dng(&mut out, img.clone(), meta).unwrap();
        out.into_inner()
    }

    /// A written DNG decodes back to the same pixels, levels, colour and orientation,
    /// and carries its preview and camera name.
    #[test]
    fn round_trips_through_the_decoder() {
        let img = linear(300, 200);
        let jpeg = fake_jpeg(160, 120, 3);
        let meta = DngMeta {
            make: "SONY",
            model: "ILCE-9",
            exif_from: None,
            preview: Some((&jpeg, 160, 120)),
            software: "Darkslide",
        };
        let bytes = write(&img, &meta);
        let back = crate::decode_bytes(&bytes).unwrap();
        assert_eq!(back.size, img.size);
        assert_eq!(back.cfa, CfaPattern::LinearRgb);
        assert_eq!(back.data, img.data);
        assert_eq!(back.black_level, [0.0; 4]);
        assert_eq!(back.white_level, 65535.0);
        assert_eq!(back.orientation, Orientation::Rotate90);
        for c in 0..3 {
            assert!(
                (back.wb_as_shot[c] - img.wb_as_shot[c]).abs() < 1e-3,
                "{:?}",
                back.wb_as_shot
            );
            assert!(
                (back.wb_daylight[c] - img.wb_daylight[c]).abs() < 2e-3,
                "{:?}",
                back.wb_daylight
            );
            for k in 0..3 {
                assert!((back.cam_to_xyz[c][k] - img.cam_to_xyz[c][k]).abs() < 2e-3);
            }
        }
        let f = crate::RawFile::from_bytes(&bytes).unwrap();
        let p = f.preview().unwrap();
        // The stored JPEG, up to its end marker.
        assert!(jpeg.starts_with(&p.jpeg) && p.jpeg.ends_with(&[0xFF, 0xD9]));
        assert_eq!(p.size, Size::new(160, 120));
        let md = f.metadata().unwrap();
        assert_eq!((md.make.as_str(), md.model.as_str()), ("SONY", "ILCE-9"));
        // No preview, no software: still a valid DNG.
        let bare = write(
            &img,
            &DngMeta {
                make: "X",
                model: "Y",
                ..DngMeta::default()
            },
        );
        assert_eq!(crate::decode_bytes(&bare).unwrap().data, img.data);
    }

    #[test]
    fn rejects_mosaics_and_degenerate_colour() {
        let mut img = linear(16, 16);
        img.cfa = CfaPattern::Rggb;
        img.data.truncate(256);
        let mut out = Cursor::new(Vec::new());
        assert!(matches!(
            write_linear_dng(&mut out, img.clone(), &DngMeta::default()),
            Err(RawError::Malformed(_))
        ));
        let mut img = linear(16, 16);
        img.cam_to_xyz = [[0.0; 3]; 3];
        assert!(matches!(
            write_linear_dng(&mut out, img.clone(), &DngMeta::default()),
            Err(RawError::Malformed(_))
        ));
        assert!(source_metadata(Path::new("/nonexistent.arw")).is_none());
        // A format whose header isn't bounded never reaches rawler's parser.
        let dir = tempfile::tempdir().unwrap();
        let x3f = dir.path().join("x.X3F");
        let mut f = b"FOVb".to_vec();
        f.resize(256, 0);
        std::fs::write(&x3f, f).unwrap();
        assert!(source_metadata(&x3f).is_none());
    }

    /// EXIF comes along from a real RAW when samples are present.
    #[test]
    fn carries_the_source_exif() {
        let Some(arw) = ds_testdata::sample_arw() else {
            return;
        };
        let src = crate::read_metadata(&arw).unwrap();
        let img = linear(64, 48);
        let bytes = write(
            &img,
            &DngMeta {
                make: &src.make,
                model: &src.model,
                exif_from: Some(&arw),
                ..DngMeta::default()
            },
        );
        let md = crate::read_metadata_bytes(&bytes).unwrap();
        assert_eq!(md.capture_time, src.capture_time);
        assert_eq!(md.iso, src.iso);
        assert_eq!(md.shutter, src.shutter);
    }
}
