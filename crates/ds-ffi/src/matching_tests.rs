//! Tests of exposure / white-balance matching. Synthetic frames cover the logic; the
//! sample ARWs (when present) check it on real scenes.

#![allow(clippy::float_cmp)] // exact expected values (zero deltas, clamped bounds)

use std::sync::atomic::{AtomicU64, Ordering};

use ds_core::{CfaPattern, Orientation, RawImage, Size};
use ds_pipeline::{CaptureInfo, ImageKey};
use futures_executor::block_on;

use super::*;

static NEXT_KEY: AtomicU64 = AtomicU64::new(0x3A7C_0000);

fn key() -> ImageKey {
    ImageKey(NEXT_KEY.fetch_add(1, Ordering::Relaxed))
}

/// Scene luminance (fraction of the sensor clip at `k = 0`): 4×4 px blocks whose 256
/// levels, log-uniform over 2^-8 ..= 2^-1, repeat (shuffled) in every 64×64 px tile,
/// so any tile-aligned region has the same statistics and leaving one out of the
/// meter doesn't move the median.
fn scene(x: u32, y: u32) -> f32 {
    let idx = (x / 4) % 16 + 16 * ((y / 4) % 16);
    let u = ((idx * 97) % 256) as f32 / 255.0;
    (-8.0 + 7.0 * u).exp2()
}

const W: u32 = 512;
const H: u32 = 256;
const BLACK: f32 = 512.0;
const WHITE: f32 = 16_383.0;

/// A Bayer frame of [`scene`] exposed `k` stops brighter, clipped at the white level;
/// `blown` sets that rectangle (x0, y0, x1, y1) to the clip.
fn frame_raw(k: f32, blown: Option<(u32, u32, u32, u32)>) -> RawImage {
    let gain = k.exp2();
    let wb = [2.0f32, 1.0, 1.5];
    let mut data = vec![0u16; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let c = CfaPattern::Rggb.color_at(x, y).unwrap() as usize;
            let inside = blown
                .is_some_and(|(x0, y0, x1, y1)| (x0..x1).contains(&x) && (y0..y1).contains(&y));
            // A slightly warm neutral, white-balanced by `wb`.
            let v = if inside {
                1.0
            } else {
                (scene(x, y) * gain / wb[c]).min(1.0)
            };
            data[(y * W + x) as usize] = (BLACK + v * (WHITE - BLACK)).round() as u16;
        }
    }
    RawImage {
        size: Size::new(W, H),
        cfa: CfaPattern::Rggb,
        data,
        black_level: [BLACK; 4],
        white_level: WHITE,
        wb_as_shot: wb,
        wb_daylight: wb,
        cam_to_xyz: [
            [0.412_4, 0.357_6, 0.180_5],
            [0.212_6, 0.715_2, 0.072_2],
            [0.019_3, 0.119_2, 0.950_5],
        ],
        orientation: Orientation::Normal,
        default_crop: None,
    }
}

/// EXIF of a frame `k` stops brighter than 1/250 s, f/4, ISO 400 (by shutter).
fn exif(k: f32) -> ExifExposure {
    ExifExposure {
        shutter_s: k.exp2() / 250.0,
        aperture: 4.0,
        iso: 400.0,
    }
}

fn frame(raw: RawImage, ev: f32, exif: Option<ExifExposure>) -> Frame {
    let mut adj = Adjustments::default();
    adj.exposure.exposure_ev = ev;
    Frame {
        loaded: Loaded {
            key: key(),
            raw: Arc::new(raw),
            capture: CaptureInfo::default(),
        },
        adj,
        exif,
        transient: false,
    }
}

fn never() -> bool {
    false
}

/// Metered log2 of `f` with `delta` applied.
fn metered(f: &Frame, delta: f32) -> f32 {
    meter_frame(f, delta, &MeterConfig::default())
        .unwrap()
        .plane
        .meter(f32::INFINITY, &MeterConfig::default())
        .median
        .log2()
}

#[test]
fn a_bracketed_set_converges_on_the_reference() {
    let reference = frame(frame_raw(0.0, None), 0.0, Some(exif(0.0)));
    let ks = [-2.5f32, -1.3, -0.5, 0.7, 1.5];
    let targets: Vec<_> = ks
        .iter()
        .map(|&k| Ok(frame(frame_raw(k, None), 0.0, Some(exif(k)))))
        .collect();
    let frames: Vec<Frame> = targets
        .iter()
        .map(|t: &Result<Frame, DsError>| t.as_ref().unwrap().clone())
        .collect();
    let out = match_frames(&reference, targets, &never).unwrap();
    assert_eq!(out.len(), ks.len());
    let r = metered(&reference, 0.0);
    for ((k, m), f) in ks.iter().zip(&out).zip(&frames) {
        eprintln!("synthetic {k:+}: {m:?}");
        assert_eq!(m.source, ExposureMatchSource::Metered, "{k}: {m:?}");
        // The scene was exposed k stops brighter: −k brings it back.
        assert!((m.delta_ev + k).abs() <= 0.1, "{k}: {m:?}");
        // …and its render meters like the reference's.
        assert!((metered(f, m.delta_ev) - r).abs() <= 0.1, "{k}: {m:?}");
        assert!(m.renders >= 2 && m.renders <= 5, "{k}: {m:?}");
        assert!(!m.exif_disagrees && m.error.is_none());
        let e = m.exif_delta_ev.unwrap();
        assert!((e + k).abs() < 1e-4, "{k}: {e}");
    }
}

#[test]
fn the_reference_exposure_slider_counts() {
    // Reference pushed +0.6 in the edit, target already at +0.2: it needs +0.4 more.
    let reference = frame(frame_raw(0.0, None), 0.6, Some(exif(0.0)));
    let out = match_frames(
        &reference,
        [Ok(frame(frame_raw(0.0, None), 0.2, Some(exif(0.0))))],
        &never,
    )
    .unwrap();
    assert!((out[0].delta_ev - 0.4).abs() <= 0.1, "{:?}", out[0]);
    assert!((out[0].exif_delta_ev.unwrap() - 0.4).abs() < 1e-5);
}

#[test]
fn a_blown_region_does_not_skew_the_match() {
    let reference = frame(frame_raw(0.0, None), 0.0, None);
    // A quarter of the frame blown (a window) on a darker and on an equal frame.
    let blown = Some((W / 4, H / 4, 3 * W / 4, 3 * H / 4));
    let targets = [-1.3f32, 0.0, 0.7].map(|k| Ok(frame(frame_raw(k, blown), 0.0, None)));
    let out = match_frames(&reference, targets, &never).unwrap();
    for (k, m) in [-1.3f32, 0.0, 0.7].iter().zip(&out) {
        eprintln!("synthetic blown {k:+}: {m:?}");
        assert_eq!(m.source, ExposureMatchSource::Metered);
        assert!((m.delta_ev + k).abs() <= 0.1, "{k}: {m:?}");
    }
}

#[test]
fn unmeterable_frames_fall_back_to_exif() {
    let reference = frame(frame_raw(0.0, None), 0.0, Some(exif(0.0)));
    // Pushed 9 stops: everything clipped, nothing to meter.
    let white = frame(frame_raw(9.0, None), 0.0, Some(exif(1.0)));
    // Same, without EXIF: nothing to go on.
    let bare = frame(frame_raw(9.0, None), 0.0, None);
    let out = match_frames(&reference, [Ok(white), Ok(bare)], &never).unwrap();
    assert_eq!(out[0].source, ExposureMatchSource::Exif, "{:?}", out[0]);
    assert!((out[0].delta_ev + 1.0).abs() < 1e-5);
    assert_eq!(out[1].source, ExposureMatchSource::Unmatched);
    assert_eq!(out[1].delta_ev, 0.0);

    // An unmeterable reference: EXIF for everyone, no target renders.
    let reference = frame(frame_raw(9.0, None), 0.0, Some(exif(0.0)));
    let out = match_frames(
        &reference,
        [Ok(frame(frame_raw(-1.0, None), 0.0, Some(exif(-1.0))))],
        &never,
    )
    .unwrap();
    assert_eq!(
        (out[0].source, out[0].renders),
        (ExposureMatchSource::Exif, 0)
    );
    assert!((out[0].delta_ev - 1.0).abs() < 1e-5);
}

#[test]
fn errors_per_target_and_for_the_call() {
    let reference = frame(frame_raw(0.0, None), 0.0, Some(exif(0.0)));
    // A target whose RAW can't render (empty): EXIF, with the error.
    let mut empty = frame_raw(0.0, None);
    empty.size = Size::new(0, 0);
    empty.data.clear();
    let broken = frame(empty.clone(), 0.0, Some(exif(-2.0)));
    let out = match_frames(
        &reference,
        [
            Ok(broken),
            Err(DsError::invalid("unreadable")),
            Ok(frame(empty.clone(), 0.0, None)),
        ],
        &never,
    )
    .unwrap();
    assert_eq!(out[0].source, ExposureMatchSource::Exif);
    assert!(
        (out[0].delta_ev - 2.0).abs() < 1e-5 && out[0].error.is_some(),
        "{:?}",
        out[0]
    );
    assert_eq!(out[1].source, ExposureMatchSource::Unmatched);
    assert!(out[1].error.as_deref().unwrap().contains("unreadable"));
    assert_eq!(out[2].source, ExposureMatchSource::Unmatched);
    assert!(out[2].error.is_some());

    // A reference that can't render fails the call.
    let bad_ref = frame(empty, 0.0, None);
    assert!(match_frames(&bad_ref, [], &never).is_err());

    // Cancelled: before a target, between renders, or a cancelled load.
    let t = || Ok(frame(frame_raw(-1.0, None), 0.0, None));
    assert!(matches!(
        match_frames(&reference, [t()], &|| true),
        Err(DsError::Cancelled)
    ));
    assert!(matches!(
        match_frames(&reference, [Err(DsError::Cancelled)], &never),
        Err(DsError::Cancelled)
    ));
    let calls = std::sync::atomic::AtomicU32::new(0);
    let cancel_in_solve = || calls.fetch_add(1, Ordering::Relaxed) >= 1;
    assert!(matches!(
        match_frames(&reference, [t()], &cancel_in_solve),
        Err(DsError::Cancelled)
    ));
}

#[test]
fn exif_is_read_from_metadata() {
    let meta = ds_raw::ImageMeta {
        shutter: Some(ds_raw::Rational { num: 1, den: 250 }),
        aperture: Some(4.0),
        iso: Some(400),
        ..ds_raw::ImageMeta::default()
    };
    assert_eq!(exif_of(&meta), Some(exif(0.0)));
    assert_eq!(exif_of(&ds_raw::ImageMeta { iso: None, ..meta }), None);
}

// ---------------------------------------------------------------- white balance ---

#[test]
fn relative_wb_lands_on_the_reference() {
    let r = WbValue {
        kelvin: 5600.0,
        tint: 4.0,
    };
    let t = WbValue {
        kelvin: 3200.0,
        tint: -12.0,
    };
    // Exactly the reference (no float round trip through the delta).
    assert_eq!(relative_wb(r, t), Some(r));
    let odd = WbValue {
        kelvin: 4321.0,
        tint: -5.0,
    };
    assert_eq!(
        relative_wb(
            odd,
            WbValue {
                kelvin: 5123.7,
                tint: 3.7
            }
        ),
        Some(odd)
    );
    // Already matching (within the sliders' rounding): left alone.
    assert_eq!(
        relative_wb(
            r,
            WbValue {
                kelvin: 5600.3,
                tint: 4.02
            }
        ),
        None
    );
    // Clamped to the sliders.
    let wild = WbValue {
        kelvin: 90_000.0,
        tint: 400.0,
    };
    assert_eq!(
        relative_wb(wild, t),
        Some(WbValue {
            kelvin: 50000.0,
            tint: 150.0
        })
    );
}

fn photo(path: &str, wb: WhiteBalance) -> MatchPhoto {
    let adj = Adjustments {
        white_balance: wb,
        ..Adjustments::default()
    };
    MatchPhoto {
        path: path.into(),
        adjustments_json: serde_json::to_string(&adj).unwrap(),
    }
}

fn custom(temp_k: f32, tint: f32) -> WhiteBalance {
    WhiteBalance::Custom { temp_k, tint }
}

#[test]
fn wb_match_of_custom_settings_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.ARW");
    std::fs::write(&junk, b"not a raw").unwrap();
    let junk = junk.to_string_lossy().into_owned();
    let reference = photo(&junk, custom(5200.0, 8.0));
    let targets = [
        photo(&junk, custom(3000.0, -20.0)),
        photo(&junk, custom(5200.0, 8.0)),
        // As Shot needs the file's camera colour: unreadable here.
        photo(&junk, WhiteBalance::AsShot),
        MatchPhoto {
            path: junk.clone(),
            adjustments_json: "{".into(),
        },
    ];
    let out = block_on(match_white_balance(
        reference,
        targets.to_vec(),
        CancelToken::new(),
    ))
    .unwrap();
    let wb = out[0].white_balance.unwrap();
    assert!(
        (wb.kelvin - 5200.0).abs() < 0.5 && (wb.tint - 8.0).abs() < 0.05,
        "{wb:?}"
    );
    assert_eq!(
        out[1],
        WbMatchResult {
            white_balance: None,
            error: None
        }
    );
    assert!(out[2].white_balance.is_none() && out[2].error.is_some());
    assert!(out[3].error.as_deref().unwrap().contains("JSON"));
    // The reference itself must resolve.
    let r = block_on(match_white_balance(
        photo(&junk, WhiteBalance::AsShot),
        targets.to_vec(),
        CancelToken::new(),
    ));
    assert!(r.is_err());
    // A cancel stops before the next target.
    assert!(matches!(
        match_wb(&photo(&junk, custom(5200.0, 8.0)), &targets, &|| true),
        Err(DsError::Cancelled)
    ));
}

#[test]
fn match_decodes_leave_the_viewer_cache_alone() {
    let Some(p) = ds_testdata::sample_arw() else {
        return;
    };
    // Not cached: a fresh, transient decode that stays out of the decoded-RAW cache.
    let tmp = tempfile::tempdir().unwrap();
    let copy = tmp.path().join("match.ARW");
    std::fs::copy(&p, &copy).unwrap();
    let me = photo(&copy.to_string_lossy(), WhiteBalance::AsShot);
    let f = load_frame(&me).unwrap();
    assert!(f.transient && !develop().is_cached(&copy));
    assert!(f.exif.is_some());
    let out = block_on(match_exposures(
        me.clone(),
        vec![me.clone()],
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(out[0].source, ExposureMatchSource::Metered);
    assert!(!develop().is_cached(&copy));
    // Cached (the viewer's photo): that decode is used and kept.
    let viewer = develop().load(&copy).unwrap();
    let g = load_frame(&me).unwrap();
    assert!(!g.transient && g.loaded.key == viewer.key);
    drop(Transient::of(&g));
    assert!(develop().is_cached(&copy));
}

#[test]
fn wb_match_reads_as_shot_from_the_samples() {
    let Some(p) = ds_testdata::samples_with_ext("ARW").into_iter().next() else {
        return;
    };
    let p = p.to_string_lossy().into_owned();
    // The reference at As Shot, a target with a custom WB: the target gets the
    // reference's as-shot Kelvin/tint, which is what the readout shows.
    let as_shot = resolved_wb(Path::new(&p), &Adjustments::default()).unwrap();
    let out = block_on(match_white_balance(
        photo(&p, WhiteBalance::AsShot),
        vec![
            photo(&p, custom(9000.0, 30.0)),
            photo(&p, WhiteBalance::AsShot),
        ],
        CancelToken::new(),
    ))
    .unwrap();
    let wb = out[0].white_balance.unwrap();
    assert!((wb.kelvin - as_shot.kelvin).abs() < 0.5 && (wb.tint - as_shot.tint).abs() < 0.05);
    // The same file at As Shot already matches.
    assert_eq!(out[1].white_balance, None);
    // A target at As Shot matched to a custom reference.
    let out = block_on(match_white_balance(
        photo(&p, custom(4100.0, -3.0)),
        vec![photo(&p, WhiteBalance::AsShot)],
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(
        out[0].white_balance,
        Some(WbValue {
            kelvin: 4100.0,
            tint: -3.0
        })
    );
}

// ---------------------------------------------------------------- FFI + samples ---

#[test]
fn ffi_errors() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.ARW");
    std::fs::write(&junk, b"not a raw").unwrap();
    let junk = junk.to_string_lossy().into_owned();
    let p = |path: &str, json: &str| MatchPhoto {
        path: path.into(),
        adjustments_json: json.into(),
    };
    // Unreadable reference, bad JSON, missing file: the call fails.
    for r in [p(&junk, "{}"), p(&junk, "{"), p("/nonexistent/x.ARW", "{}")] {
        assert!(block_on(match_exposures(r, vec![], CancelToken::new())).is_err());
    }
}

/// A sample scaled by 2^k (clipped at the white level), with its EXIF scaled to match.
fn scaled(raw: &RawImage, exif: Option<ExifExposure>, k: f32, ev: f32) -> Frame {
    let mut r = raw.clone();
    let g = k.exp2();
    let black = raw.black_level[0];
    for v in &mut r.data {
        *v = (black + (f32::from(*v) - black).max(0.0) * g)
            .min(raw.white_level)
            .round() as u16;
    }
    frame(
        r,
        ev,
        exif.map(|e| ExifExposure {
            shutter_s: e.shutter_s * g,
            ..e
        }),
    )
}

#[test]
fn ffi_matches_scaled_samples() {
    let Some(p) = ds_testdata::sample_arw() else {
        return;
    };
    // The FFI path end to end: a photo matched to itself needs nothing.
    let me = MatchPhoto {
        path: p.to_string_lossy().into_owned(),
        adjustments_json: "{}".into(),
    };
    let out = block_on(match_exposures(me.clone(), vec![me], CancelToken::new())).unwrap();
    assert_eq!(out[0].source, ExposureMatchSource::Metered);
    assert!(
        out[0].delta_ev.abs() < 0.05 && out[0].renders == 1,
        "{:?}",
        out[0]
    );
    assert!(out[0].exif_delta_ev.unwrap().abs() < 1e-6);

    // Scaled copies of a real scene (−1.3 stays under the clip, +0.7 clips), with and
    // without a blown quarter.
    let raw = ds_raw::decode(&p).unwrap();
    let exif = ds_raw::read_metadata(&p).ok().as_ref().and_then(exif_of);
    let reference = frame(raw.clone(), 0.0, exif);
    let mut blown = scaled(&raw, exif, -1.3, 0.0);
    let mut r = (*blown.loaded.raw).clone();
    let (w, h) = (r.size.width as usize, r.size.height as usize);
    for y in h / 3..2 * h / 3 {
        for x in w / 3..2 * w / 3 {
            r.data[y * w + x] = r.white_level as u16;
        }
    }
    blown.loaded.raw = Arc::new(r);
    let ks = [-1.3f32, 0.7, -1.3];
    let targets = vec![
        Ok(scaled(&raw, exif, -1.3, 0.0)),
        Ok(scaled(&raw, exif, 0.7, 0.0)),
        Ok(blown),
    ];
    let out = match_frames(&reference, targets, &never).unwrap();
    for (k, m) in ks.iter().zip(&out) {
        eprintln!("sample scaled {k:+}: {m:?}");
        assert_eq!(m.source, ExposureMatchSource::Metered);
        assert!((m.delta_ev + k).abs() <= 0.1, "{k}: {m:?}");
    }
}
