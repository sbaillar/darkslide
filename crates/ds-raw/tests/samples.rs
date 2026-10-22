//! Integration tests against the real Sony A9 (ILCE-9) samples in `samples/`.
//! Each test is a no-op when the git-ignored samples are absent.
#![allow(clippy::float_cmp)] // levels are exact integers stored as f32

#[cfg(feature = "rawler")]
use ds_core::CfaPattern;
use ds_core::Size;

fn samples() -> Vec<std::path::PathBuf> {
    ds_testdata::samples_with_ext("arw")
}

#[test]
fn metadata_matches_the_a9() {
    for p in samples() {
        let m = ds_raw::read_metadata(&p).unwrap();
        assert_eq!(m.make, "SONY", "{p:?}");
        assert_eq!(m.model, "ILCE-9");
        assert_eq!(m.dimensions, Some(Size::new(6000, 4000)));
        assert!(m.iso.is_some_and(|i| (50..=204_800).contains(&i)));
        let s = m.shutter.unwrap();
        assert!(s.num > 0 && s.den > 0);
        assert!(
            m.shutter_display
                .as_deref()
                .is_some_and(|d| d.starts_with("1/") || d.ends_with('"'))
        );
        assert!(m.aperture.is_some_and(|a| (0.9..64.0).contains(&a)));
        assert!(m.focal_length.is_some_and(|f| f > 1.0));
        assert!(m.lens.is_some());
        let t = m.capture_time.unwrap();
        assert!(t.year >= 2017 && t.utc_offset_minutes.is_some());
        assert!(m.shutter_count.is_some_and(|c| c > 0));
    }
}

#[test]
fn preview_is_a_full_jpeg_with_raw_orientation() {
    for p in samples() {
        let pv = ds_raw::embedded_preview(&p).unwrap();
        assert!(pv.jpeg.starts_with(&[0xFF, 0xD8, 0xFF]), "{p:?} SOI");
        assert!(pv.jpeg.ends_with(&[0xFF, 0xD9]), "{p:?} EOI");
        assert_eq!(pv.size, Size::new(1616, 1080));
        let m = ds_raw::read_metadata(&p).unwrap();
        assert_eq!(pv.orientation, m.orientation);
        // Stored in sensor orientation like the raw data, so one orientation serves both.
        let raw = m.dimensions.unwrap();
        assert_eq!(pv.size.is_portrait(), raw.is_portrait());
        let ar = |s: Size| f64::from(s.width) / f64::from(s.height);
        assert!((ar(pv.size) - ar(raw)).abs() < 0.01);
    }
}

#[cfg(feature = "rawler")] // the A9 colour matrix comes from rawler's camera data
#[test]
fn decode_levels_wb_and_geometry() {
    for p in samples() {
        let img = ds_raw::decode(&p).unwrap();
        img.validate().unwrap();
        assert_eq!(img.size, Size::new(6000, 4000));
        assert_eq!(img.cfa, CfaPattern::Rggb); // crop origin (12, 12) keeps RGGB
        assert_eq!(img.black_level, [512.0; 4]);
        // Per-file DNG WhiteLevel (0xC61D): 16380 or 16383 on the A9, matching where the data clips.
        assert!(
            img.white_level == 16380.0 || img.white_level == 16383.0,
            "{p:?} white {}",
            img.white_level
        );
        let [r, g, b] = img.wb_as_shot;
        assert_eq!(g, 1.0);
        assert!(
            (1.2..4.0).contains(&r) && (1.0..3.0).contains(&b),
            "{p:?} wb {:?}",
            img.wb_as_shot
        );
        let m = ds_raw::read_metadata(&p).unwrap();
        assert_eq!(img.orientation, m.orientation);
        // Neutral maps to D65 white.
        for (i, row) in img.cam_to_xyz.iter().enumerate() {
            let s: f32 = row.iter().sum();
            assert!((f64::from(s) - ds_raw::color::D65_WHITE[i]).abs() < 1e-4);
        }
        // Real sensor data: nothing far below black, clipping at ≤ 16383.
        let min = *img.data.iter().min().unwrap();
        let max = *img.data.iter().max().unwrap();
        assert!(
            min > 300 && f32::from(max) <= img.white_level,
            "{p:?} range {min}..{max}"
        );
        let mean = img.data.iter().map(|&v| f64::from(v)).sum::<f64>() / img.data.len() as f64;
        assert!(mean > 520.0, "{p:?} mean {mean}");
    }
}

#[cfg(feature = "rawler")]
#[test]
fn fast_path_matches_rawler_bit_for_bit() {
    let Some(p) = ds_testdata::sample_arw() else {
        return;
    };
    let fast = ds_raw::decode(&p).unwrap();
    let raw = rawler::decode_file(&p).unwrap();
    let rawler::RawImageData::Integer(full) = &raw.data else {
        panic!("float data")
    };
    let crop = raw.crop_area.unwrap();
    assert_eq!(
        (crop.p.x, crop.p.y, crop.d.w, crop.d.h),
        (12, 12, 6000, 4000)
    );
    for y in (0..4000).step_by(97) {
        let a = &fast.data[y * 6000..(y + 1) * 6000];
        let b = &full[(y + 12) * raw.width + 12..(y + 12) * raw.width + 12 + 6000];
        assert_eq!(a, b, "row {y}");
    }
    let [r, _, b, _] = raw.wb_coeffs;
    assert!((fast.wb_as_shot[0] - r).abs() < 1e-4 && (fast.wb_as_shot[2] - b).abs() < 1e-4);
}

#[test]
fn is_supported_on_samples() {
    for p in samples() {
        assert!(ds_raw::is_supported(&p));
    }
}

/// Compressed / non-fast-path Sony files go through rawler, which reads the
/// SR2 0x787F white (15360 on the A9, ~0.09 EV below where the data clips).
/// Our DNG `WhiteLevel` override must win there too.
#[cfg(feature = "rawler")]
#[test]
fn rawler_path_white_level_matches_fast_path() {
    let Some(p) = ds_testdata::sample_arw() else {
        return;
    };
    let fast = ds_raw::decode(&p).unwrap();
    let raw = rawler::decode_file(&p).unwrap();
    assert_eq!(raw.whitelevel.0[0], 15360, "rawler's own A9 white level");
    // Force the rawler path on the same bytes by renaming the raw IFD's
    // PhotometricInterpretation entry (0x0106 SHORT 1 = 32803), so the fast
    // path's layout check declines.
    let mut bytes = std::fs::read(&p).unwrap();
    let pat = [0x06u8, 0x01, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x23, 0x80];
    let at = bytes
        .windows(pat.len())
        .position(|w| w == pat)
        .expect("photometric entry");
    bytes[at] = 0x07; // tag 0x0106 → 0x0107
    let (slow, info) = ds_raw::RawFile::from_bytes(&bytes)
        .unwrap()
        .decode_with_info()
        .unwrap();
    assert_eq!(info.path, ds_raw::DecodePath::Rawler);
    assert_eq!(slow.white_level, fast.white_level);
    assert_eq!(slow.data, fast.data);
    assert_eq!(slow.black_level, fast.black_level);
    assert_eq!(slow.orientation, fast.orientation);
}

#[test]
fn sony_af_point_and_shot_numbers_on_samples() {
    for p in samples() {
        let m = ds_raw::open(&p).unwrap().metadata().unwrap();
        // All samples were shot with a central flexible spot.
        let af = m.af_point.unwrap();
        assert!(
            (0.35..0.65).contains(&af.x) && (0.35..0.65).contains(&af.y),
            "{p:?} {af:?}"
        );
        assert!(m.shot_number.is_some_and(|n| n > 0));
        assert!(m.sequence_image_number.is_some_and(|n| n >= 1));
        assert!(ds_raw::filename_sequence(&p).is_some());
    }
    // SKB06204: 7th frame of a burst, shot 15583 since power-up (exiftool Tag9400c).
    if let Some(p) = samples().into_iter().find(|p| p.ends_with("SKB06204.ARW")) {
        let m = ds_raw::read_metadata(&p).unwrap();
        assert_eq!(m.shot_number, Some(15583));
        assert_eq!(m.sequence_image_number, Some(7));
        assert_eq!(m.iso, Some(4000));
        assert_eq!(ds_raw::filename_sequence(&p), Some(6204));
    }
}

#[cfg(feature = "rawler")] // the A9 colour matrix comes from rawler's camera data
#[test]
fn bytes_api_on_a_sample() {
    let Some(p) = ds_testdata::sample_arw() else {
        return;
    };
    let bytes = std::fs::read(&p).unwrap();
    let f = ds_raw::open(&p).unwrap();
    assert_eq!(
        ds_raw::read_metadata_bytes(&bytes).unwrap(),
        f.metadata().unwrap()
    );
    assert_eq!(
        ds_raw::decode_bytes(&bytes).unwrap().data,
        f.decode().unwrap().data
    );
}

/// The daylight response must survive into `RawImage` so the pipeline can
/// convert Kelvin ↔ multipliers; the A9's as-shot WB in these (non-daylight)
/// samples must sit near, but not on, the D65 multipliers.
#[cfg(feature = "rawler")] // the A9 colour matrix comes from rawler's camera data
#[test]
fn sample_daylight_multipliers_present() {
    for path in ds_testdata::samples_with_ext("arw") {
        let img = ds_raw::decode(&path).unwrap();
        let [r, g, b] = img.wb_daylight;
        assert_eq!(g, 1.0, "{}", path.display());
        assert!(
            (2.0..3.5).contains(&r) && (1.0..2.0).contains(&b),
            "{:?}",
            img.wb_daylight
        );
    }
}
