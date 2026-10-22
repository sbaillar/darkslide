use super::*;
use crate::encode::encode;
use crate::image::Image16;
use crate::recipe::{ExportFormat, OutputSpace};
use chrono::NaiveDate;
use ds_core::{CfaPattern, Orientation};

/// Adobe `ColorMatrix2` of the Sony ILCE-9.
const A9: [f32; 9] = [
    0.6389, -0.1703, -0.0378, -0.4562, 1.2265, 0.2587, -0.067, 0.1489, 0.655,
];

fn info(w: u32, h: u32) -> DngInfo {
    let preview = Image16::new(Size::new(16, 8), vec![30_000; 16 * 8 * 3]).unwrap();
    DngInfo {
        size: Size::new(w, h),
        cam_to_xyz: ds_raw::color::cam_to_xyz(&A9).unwrap(),
        wb_as_shot: [2.31, 1.0, 1.62],
        wb_daylight: ds_raw::color::daylight_multipliers(&A9).unwrap(),
        orientation: 6,
        default_crop: None,
        exif: ExifMeta {
            make: Some("SONY".into()),
            model: Some("ILCE-9".into()),
            lens_model: Some("FE 24-70mm F2.8 GM".into()),
            capture_time: NaiveDate::from_ymd_opt(2026, 9, 20)
                .unwrap()
                .and_hms_opt(14, 3, 9),
            exposure_time: Some((1, 250)),
            f_number: Some(2.8),
            iso: Some(3200),
            focal_length_mm: Some(35.0),
            gps: Some(crate::exif::Gps {
                latitude: 45.5,
                longitude: -73.6,
                altitude_m: None,
            }),
            ..ExifMeta::default()
        },
        preview: Some(
            encode(
                &preview,
                ExportFormat::Jpeg { quality: 80 },
                crate::icc::profile(OutputSpace::Srgb),
                None,
                None,
            )
            .unwrap(),
        ),
    }
}

fn pixels(w: u32, h: u32) -> Vec<u16> {
    (0..w * h * 3)
        .map(|i| ((i * 977) % 65_536) as u16)
        .collect()
}

fn write(dir: &Path, info: DngInfo, data: &[u16], chunk_rows: usize) -> PathBuf {
    let row = info.size.width as usize * 3;
    let mut w = LinearDngWriter::create(dir, "DSC00412-Upscaled.dng", info).unwrap();
    for c in data.chunks(row * chunk_rows) {
        w.write_rows(c).unwrap();
    }
    w.finish(&Namer::default()).unwrap()
}

#[test]
fn a_linear_dng_decodes_to_the_same_pixels_and_colour() {
    let d = tempfile::tempdir().unwrap();
    let (w, h) = (40u32, 37u32);
    let data = pixels(w, h);
    let i = info(w, h);
    let path = write(d.path(), i.clone(), &data, 7);
    assert_eq!(path, d.path().join("DSC00412-Upscaled.dng"));
    let raw = ds_raw::decode(&path).unwrap();
    assert_eq!(raw.cfa, CfaPattern::LinearRgb);
    assert_eq!(raw.size, Size::new(w, h));
    assert_eq!(raw.data, data);
    assert_eq!((raw.white_level, raw.black_level), (65_535.0, [0.0; 4]));
    assert_eq!(raw.orientation, Orientation::from_exif(6));
    for c in 0..3 {
        assert!(
            (raw.wb_as_shot[c] - i.wb_as_shot[c]).abs() < 1e-4,
            "{:?}",
            raw.wb_as_shot
        );
        assert!(
            (raw.wb_daylight[c] - i.wb_daylight[c]).abs() < 1e-4,
            "{:?}",
            raw.wb_daylight
        );
        for k in 0..3 {
            assert!(
                (raw.cam_to_xyz[c][k] - i.cam_to_xyz[c][k]).abs() < 1e-4,
                "{:?} vs {:?}",
                raw.cam_to_xyz,
                i.cam_to_xyz
            );
        }
    }
    // Nothing else is left in the folder (the temp file was renamed).
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 1);
}

#[test]
fn the_dng_carries_the_source_metadata_and_preview() {
    let d = tempfile::tempdir().unwrap();
    let (w, h) = (24u32, 20u32);
    let i = info(w, h);
    let path = write(d.path(), i.clone(), &pixels(w, h), 20);
    let m = ds_raw::read_metadata(&path).unwrap();
    assert_eq!((m.make.as_str(), m.model.as_str()), ("SONY", "ILCE-9"));
    assert_eq!(m.lens.as_deref(), Some("FE 24-70mm F2.8 GM"));
    assert_eq!(m.iso, Some(3200));
    assert_eq!(m.aperture, Some(2.8));
    assert_eq!(m.focal_length, Some(35.0));
    assert_eq!(m.shutter.map(|r| (r.num, r.den)), Some((1, 250)));
    let t = m.capture_time.unwrap();
    assert_eq!(
        (t.year, t.month, t.day, t.hour, t.minute, t.second),
        (2026, 9, 20, 14, 3, 9)
    );
    assert_eq!(m.orientation, Orientation::from_exif(6));
    let g = m.gps.unwrap();
    assert!((g.latitude - 45.5).abs() < 1e-6 && (g.longitude + 73.6).abs() < 1e-6);
    let p = ds_raw::embedded_preview(&path).unwrap();
    assert_eq!(p.jpeg, i.preview.unwrap());
    assert_eq!(p.size, Size::new(16, 8));
}

#[test]
fn the_default_crop_is_applied_on_decode_and_no_preview_is_fine() {
    let d = tempfile::tempdir().unwrap();
    let (w, h) = (30u32, 20u32);
    let data = pixels(w, h);
    let mut i = info(w, h);
    i.default_crop = Some([4, 2, 20, 16]);
    i.preview = None;
    i.exif.gps = None;
    i.exif.make = None; // the camera model alone names it
    let path = write(d.path(), i, &data, 3);
    let raw = ds_raw::decode(&path).unwrap();
    assert_eq!(raw.size, Size::new(20, 16));
    let row = w as usize * 3;
    assert_eq!(&raw.data[..60], &data[2 * row + 12..2 * row + 72]);
    assert!(ds_raw::embedded_preview(&path).is_err());
}

#[test]
fn names_are_never_reused_and_bad_input_leaves_nothing_behind() {
    let d = tempfile::tempdir().unwrap();
    let (w, h) = (8u32, 4u32);
    let data = pixels(w, h);
    let first = write(d.path(), info(w, h), &data, 4);
    let second = write(d.path(), info(w, h), &data, 4);
    assert_ne!(first, second, "an existing file is never replaced");
    assert_eq!(second, d.path().join("DSC00412-Upscaled-1.dng"));

    let mut wr = LinearDngWriter::create(d.path(), "x.dng", info(w, h)).unwrap();
    assert!(wr.write_rows(&data[..5]).is_err(), "not whole rows");
    wr.write_rows(&data[..24]).unwrap();
    assert_eq!(wr.rows(), 1);
    assert!(wr.write_rows(&data).is_err(), "more rows than the image");
    assert!(wr.finish(&Namer::default()).is_err(), "rows missing");
    // The unfinished file's temp is gone; only the two finished files remain.
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 2);

    let mut bad = info(w, h);
    bad.cam_to_xyz = [[0.0; 3]; 3];
    assert!(LinearDngWriter::create(d.path(), "y.dng", bad).is_err());
    let mut bad = info(w, h);
    bad.wb_as_shot[0] = 0.0;
    assert!(LinearDngWriter::create(d.path(), "y.dng", bad).is_err());
    assert!(matches!(
        LinearDngWriter::create(d.path(), "y.dng", info(40_000, 20_000)),
        Err(ExportError::TooLarge(_))
    ));
    assert!(LinearDngWriter::create(d.path(), "y.dng", info(0, 4)).is_err());
    let file = d.path().join("file");
    fs::write(&file, b"").unwrap();
    assert!(
        LinearDngWriter::create(&file, "y.dng", info(w, h)).is_err(),
        "not a folder"
    );
}

/// Offsets are 32-bit in a classic TIFF: a preview that pushes a directory past 4 GB
/// is refused instead of writing offsets that wrapped.
#[test]
fn offsets_past_four_gigabytes_are_refused() {
    let big = info(26_700, 26_700);
    let at_end = DATA + 26_700 * 26_700 * 6;
    assert!(at_end < u64::from(u32::MAX), "the pixels alone fit");
    let r = tail(&big, vec![0xFF; 40 << 20]);
    assert!(matches!(r, Err(ExportError::TooLarge(s)) if s == Size::new(26_700, 26_700)));
    // Without the preview the same image's directories still fit.
    assert!(tail(&big, Vec::new()).is_ok());
}

/// The location is the caller's to keep or drop: it is written only when `gps` is set.
#[test]
fn gps_is_written_only_when_the_source_has_it() {
    let d = tempfile::tempdir().unwrap();
    let (w, h) = (8u32, 4u32);
    let with = write(d.path(), info(w, h), &pixels(w, h), 4);
    assert!(ds_raw::read_metadata(&with).unwrap().gps.is_some());
    let mut none = info(w, h);
    none.exif.gps = None;
    let without = write(d.path(), none, &pixels(w, h), 4);
    assert!(ds_raw::read_metadata(&without).unwrap().gps.is_none());
}
