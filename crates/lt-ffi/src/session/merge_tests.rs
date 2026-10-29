#![allow(clippy::float_cmp)] // exact values are intended where asserted

use super::*;
use crate::session::open_session;
use futures_executor::block_on;
use lt_session::{ImageMetaLite, Session};
use std::io::Cursor;

/// XYZ → linear sRGB, standing in for a camera's colour matrix.
const XYZ_TO_CAM: [f32; 9] = [
    3.2406, -1.5372, -0.4986, -0.9689, 1.8758, 0.0415, 0.0557, -0.2040, 1.0570,
];

fn hash(i: i64, j: i64) -> f32 {
    let mut h = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (j as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
    h ^= h >> 31;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// Smooth corner-rich texture in `0.1..0.9`.
fn texture(x: f32, y: f32) -> f32 {
    let noise = |cell: f32, o: i64| {
        let (u, v) = (x / cell, y / cell);
        let (i, j) = (u.floor(), v.floor());
        let s = |t: f32| t * t * (3.0 - 2.0 * t);
        let (fx, fy) = (s(u - i), s(v - j));
        let (i, j) = (i as i64 + o, j as i64);
        let top = hash(i, j) + (hash(i + 1, j) - hash(i, j)) * fx;
        let bottom = hash(i, j + 1) + (hash(i + 1, j + 1) - hash(i, j + 1)) * fx;
        top + (bottom - top) * fy
    };
    0.1 + 0.5 * noise(11.0, 0) + 0.3 * noise(4.0, 1000)
}

/// A linear DNG of `w`×`h` whose pixel `(x, y)` is `f(x, y)` (normalized, clipped at 1).
fn dng(path: &Path, w: u32, h: u32, f: impl Fn(u32, u32) -> f32) {
    dng_oriented(path, w, h, Orientation::Normal, f);
}

/// [`dng`] with an EXIF orientation (the sensor data stays `w`×`h`).
fn dng_oriented(
    path: &Path,
    w: u32,
    h: u32,
    orientation: Orientation,
    f: impl Fn(u32, u32) -> f32,
) {
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let v = f(x, y);
            for k in [0.8, 1.0, 1.2] {
                data.push(((v * k).clamp(0.0, 1.0) * 65535.0).round() as u16);
            }
        }
    }
    let raw = RawImage {
        size: Size::new(w, h),
        cfa: CfaPattern::LinearRgb,
        data,
        black_level: [0.0; 4],
        white_level: 65535.0,
        wb_as_shot: [1.9, 1.0, 1.5],
        wb_daylight: lt_raw::color::daylight_multipliers(&XYZ_TO_CAM).unwrap(),
        cam_to_xyz: lt_raw::color::cam_to_xyz(&XYZ_TO_CAM).unwrap(),
        orientation,
        default_crop: None,
    };
    let mut out = Cursor::new(Vec::new());
    let meta = lt_raw::DngMeta {
        make: "Test",
        model: "Cam",
        ..lt_raw::DngMeta::default()
    };
    lt_raw::write_linear_dng(&mut out, raw, &meta).unwrap();
    fs::write(path, out.into_inner()).unwrap();
}

/// HDR scene in reference units: spans ~9 stops.
fn radiance(x: u32, y: u32) -> f32 {
    let (fx, fy) = (x as f32, y as f32);
    (8.0 * fx / 256.0 - 6.0).exp2() * texture(fx, fy) * 2.0
}

/// A session at `root` holding `files` (written by `make`) in Capture; the open handle
/// and the variant ids in file order.
fn session_with(
    root: &Path,
    files: &[&str],
    make: impl Fn(&Path, usize),
) -> (Arc<SessionHandle>, Vec<i64>) {
    let mut s = Session::create(root, "Merge").unwrap();
    fs::create_dir_all(root.join("Capture")).unwrap();
    let mut items = Vec::new();
    for (i, name) in files.iter().enumerate() {
        let rel = format!("Capture/{name}");
        make(&root.join(&rel), i);
        let bytes = fs::read(root.join(&rel)).unwrap();
        items.push((
            rel,
            FileHash(*blake3::hash(&bytes).as_bytes()),
            ImageMetaLite::default(),
        ));
    }
    s.add_originals(items).unwrap();
    drop(s);
    let h = block_on(open_session(
        root.to_string_lossy().into_owned(),
        CancelToken::new(),
    ))
    .unwrap();
    let ids = files
        .iter()
        .map(|n| variant_named(&h, n).variant_id)
        .collect();
    (h, ids)
}

fn variant_named(h: &Arc<SessionHandle>, name: &str) -> crate::session::VariantRecord {
    block_on(h.clone().variants(CancelToken::new()))
        .unwrap()
        .into_iter()
        .find(|v| v.file_name == name)
        .unwrap_or_else(|| panic!("no {name}"))
}

const HDR: MergeOptions = MergeOptions {
    kind: MergeKind::Hdr,
    align: true,
    deghost: Deghost::Medium,
    projection: PanoProjection::Auto,
    auto_crop: true,
};

fn brackets(root: &Path) -> (Arc<SessionHandle>, Vec<i64>) {
    session_with(root, &["B1.dng", "B2.dng", "B3.dng"], |p, i| {
        let e = [0.25f32, 1.0, 4.0][i];
        dng(p, 256, 192, |x, y| radiance(x, y) * e);
    })
}

struct Rec(Arc<Mutex<Vec<ProgressEvent>>>);

impl ProgressListener for Rec {
    fn on_progress(&self, e: ProgressEvent) -> Result<(), DsError> {
        lock(&self.0).push(e);
        Ok(())
    }
}

#[test]
fn hdr_merge_adds_a_linear_dng_next_to_the_sources() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("S");
    let (h, ids) = brackets(&root);
    let before: Vec<Vec<u8>> = ["B1", "B2", "B3"]
        .iter()
        .map(|n| fs::read(root.join(format!("Capture/{n}.dng"))).unwrap())
        .collect();
    // The first selected photo's title and credits go to the merged photo.
    let iptc = lt_core::IptcMeta {
        title: "Falls".into(),
        copyright: "© Alexa".into(),
        ..lt_core::IptcMeta::default()
    };
    lock(&h.session)
        .set_iptc(&[VariantId(ids[0])], &iptc)
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let out = block_on(h.clone().merge_photos(
        ids.clone(),
        HDR,
        Some(Arc::new(Rec(seen.clone()))),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(out.file_name, "B1-HDR.dng");
    let dest = root.join("Capture/B1-HDR.dng");
    assert_eq!(Path::new(&out.original_path), dest);
    // The sources are untouched.
    for (n, b) in ["B1", "B2", "B3"].iter().zip(&before) {
        assert_eq!(&fs::read(root.join(format!("Capture/{n}.dng"))).unwrap(), b);
    }
    // Registered, starting 2 EV up (the reference is the middle frame, the darkest is −2).
    let v = variant_named(&h, "B1-HDR.dng");
    assert_eq!((v.variant_id, v.image_id), (out.variant_id, out.image_id));
    let adj = lock(&h.session)
        .load_adjustments(VariantId(out.variant_id))
        .unwrap();
    assert!(
        (adj.exposure.exposure_ev - 2.0).abs() < 0.05,
        "{}",
        adj.exposure.exposure_ev
    );
    assert!(adj.lens.enabled);
    assert_eq!(
        lock(&h.session)
            .variant(VariantId(out.variant_id))
            .unwrap()
            .iptc,
        iptc
    );
    // Linear, full size, and the radiance of the reference frame at 2 EV down.
    let raw = lt_raw::decode(&dest).unwrap();
    assert_eq!(
        (raw.cfa, raw.size),
        (CfaPattern::LinearRgb, Size::new(256, 192))
    );
    let mid = lt_raw::decode(&root.join("Capture/B2.dng")).unwrap();
    for (x, y) in [(60u32, 50u32), (128, 96), (200, 150)] {
        let i = ((y * 256 + x) * 3 + 1) as usize;
        let (got, want) = (f32::from(raw.data[i]) * 4.0, f32::from(mid.data[i]));
        if want < 60_000.0 && want > 500.0 {
            assert!(
                (got - want).abs() / want < 0.03,
                "({x},{y}) {got} vs {want}"
            );
        }
    }
    // It carries a preview for the grid.
    assert!(lt_raw::embedded_preview(&dest).is_ok());
    let events = lock(&seen).clone();
    assert!(
        events
            .iter()
            .all(|e| e.kind == "merge" && e.total == PERMILLE)
    );
    assert_eq!(events.last().map(|e| e.completed), Some(PERMILLE));
    // A second merge never replaces the first.
    let again = block_on(h.clone().merge_photos(ids, HDR, None, CancelToken::new())).unwrap();
    assert_eq!(again.file_name, "B1-HDR-2.dng");
    assert!(dest.is_file());
}

#[test]
fn panorama_merge_is_wider_with_lens_corrections_off() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("P");
    let f = 300.0f64;
    let shoot = |p: &Path, i: usize| {
        let yaw = [-15.0f64, 15.0][i].to_radians();
        dng(p, 320, 240, |x, y| {
            let (cx, cy) = ((f64::from(x) - 159.5) / f, (f64::from(y) - 119.5) / f);
            // Rotate the camera ray about the vertical axis.
            let (s, c) = yaw.sin_cos();
            let (rx, rz) = (c * cx + s, -s * cx + c);
            let (lon, lat) = (rx.atan2(rz), cy.atan2(rx.hypot(rz)));
            texture((lon * f) as f32, (lat * f) as f32) * 0.6
        });
    };
    let (h, ids) = session_with(&root, &["L.dng", "R.dng"], shoot);
    let opts = MergeOptions {
        kind: MergeKind::Panorama,
        ..HDR
    };
    let preview = block_on(merge_preview(
        vec![
            root.join("Capture/L.dng").to_string_lossy().into_owned(),
            root.join("Capture/R.dng").to_string_lossy().into_owned(),
        ],
        opts,
        400,
        CancelToken::new(),
    ))
    .unwrap();
    assert!(preview.width > preview.height && preview.width <= 400);
    assert_eq!(preview.projection, PanoProjection::Perspective);
    let out = block_on(h.clone().merge_photos(ids, opts, None, CancelToken::new())).unwrap();
    assert_eq!(out.file_name, "L-Pano.dng");
    let raw = lt_raw::decode(Path::new(&out.original_path)).unwrap();
    assert!(
        raw.size.width > 400 && raw.size.height > 150,
        "{:?}",
        raw.size
    );
    let adj = lock(&h.session)
        .load_adjustments(VariantId(out.variant_id))
        .unwrap();
    assert!(!adj.lens.enabled);
}

#[test]
fn previews_errors_and_cancellation() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("E");
    let (h, ids) = brackets(&root);
    let paths: Vec<String> = ["B1", "B2", "B3"]
        .iter()
        .map(|n| {
            root.join(format!("Capture/{n}.dng"))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let p = block_on(merge_preview(paths.clone(), HDR, 128, CancelToken::new())).unwrap();
    assert_eq!((p.width, p.height), (128, 96));
    assert_eq!(p.projection, PanoProjection::Perspective);
    assert!((p.exposure_ev - 2.0).abs() < 0.1);
    assert!(p.jpeg.starts_with(&[0xFF, 0xD8]));

    let err = block_on(
        h.clone()
            .merge_photos(vec![ids[0]], HDR, None, CancelToken::new()),
    )
    .unwrap_err();
    assert!(matches!(err, DsError::InvalidArgument { .. }), "{err:?}");
    // The same photo twice is one photo.
    let err = block_on(
        h.clone()
            .merge_photos(vec![ids[0], ids[0]], HDR, None, CancelToken::new()),
    )
    .unwrap_err();
    assert!(matches!(err, DsError::InvalidArgument { .. }));
    let t = CancelToken::new();
    t.cancel();
    assert!(matches!(
        block_on(h.clone().merge_photos(ids.clone(), HDR, None, t)),
        Err(DsError::Cancelled)
    ));
    assert!(!root.join("Capture/B1-HDR.dng").exists());
    let names: Vec<String> = fs::read_dir(root.join("Capture"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| {
            Path::new(n)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("dng"))
        })
        .collect();
    assert_eq!(names.len(), 3, "{names:?}");
    let err = block_on(merge_preview(
        vec![paths[0].clone()],
        HDR,
        128,
        CancelToken::new(),
    ))
    .unwrap_err();
    assert!(matches!(err, DsError::InvalidArgument { .. }));
    let missing = block_on(merge_preview(
        vec![paths[0].clone(), "/nonexistent.dng".into()],
        HDR,
        128,
        CancelToken::new(),
    ));
    assert!(missing.is_err());
}

#[test]
fn helpers() {
    assert_eq!(candidate_name("A", MergeKind::Hdr, 1), "A-HDR.dng");
    assert_eq!(candidate_name("A", MergeKind::Panorama, 3), "A-Pano-3.dng");
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("A-HDR.dng"), b"mine").unwrap();
    let bytes = |b: &'static [u8]| {
        move |out: &mut BufWriter<fs::File>| -> Result<(), DsError> { Ok(out.write_all(b)?) }
    };
    let (name, hash) = write_new(d.path(), "A", MergeKind::Hdr, bytes(b"new")).unwrap();
    assert_eq!(name, "A-HDR-2.dng");
    assert_eq!(hash, FileHash(*blake3::hash(b"new").as_bytes()));
    assert_eq!(fs::read(d.path().join("A-HDR.dng")).unwrap(), b"mine");
    assert_eq!(fs::read(d.path().join("A-HDR-2.dng")).unwrap(), b"new");
    assert!(write_new(&d.path().join("missing"), "A", MergeKind::Hdr, bytes(b"x")).is_err());
    // A failing writer leaves no temp file behind.
    let failed = write_new(d.path(), "A", MergeKind::Hdr, |_| Err(DsError::Cancelled));
    assert!(matches!(failed, Err(DsError::Cancelled)));
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 2);
    // Only our own bytes are ever taken back.
    assert!(!remove_if_ours(&d.path().join("A-HDR.dng"), hash));
    assert!(d.path().join("A-HDR.dng").is_file());
    assert!(remove_if_ours(&d.path().join("A-HDR-2.dng"), hash));
    assert!(!d.path().join("A-HDR-2.dng").exists());

    let meta = lt_raw::ImageMeta {
        shutter: Some(lt_raw::Rational { num: 1, den: 100 }),
        iso: Some(400),
        aperture: Some(4.0),
        focal_length_35mm: Some(24),
        ..lt_raw::ImageMeta::default()
    };
    let e = exposures(&[Some(&meta), Some(&meta)]);
    assert!((e[1].unwrap() - 0.25).abs() < 1e-6);
    assert_eq!(exposures(&[None, None]), vec![None, None]);
    // One frame without ISO: no frame's EXIF exposure is trusted.
    let no_iso = lt_raw::ImageMeta {
        iso: None,
        ..meta.clone()
    };
    assert_eq!(exposures(&[Some(&meta), Some(&no_iso)]), vec![None, None]);
    assert_eq!(exposures(&[Some(&meta), None]), vec![None, None]);
    assert_eq!(focal_px(Some(&meta), 6000), Some(4000.0));
    assert_eq!(focal_px(Some(&lt_raw::ImageMeta::default()), 6000), None);

    let a = initial_edit(Adjustments::default(), 7.0, MergeKind::Panorama);
    assert_eq!(a.exposure.exposure_ev, MAX_EV);
    assert!(!a.lens.enabled);
    assert!(matches!(
        merge_error(MergeError::Cancelled),
        DsError::Cancelled
    ));
    assert!(matches!(
        merge_error(MergeError::TooFew),
        DsError::InvalidArgument { .. }
    ));
    for p in [
        PanoProjection::Auto,
        PanoProjection::Spherical,
        PanoProjection::Cylindrical,
        PanoProjection::Perspective,
    ] {
        assert_eq!(PanoProjection::from(lt_merge::Projection::from(p)), p);
    }
    assert_eq!(
        lt_merge::Deghost::from(Deghost::Low),
        lt_merge::Deghost::Low
    );
    assert_eq!(
        lt_merge::Deghost::from(Deghost::High),
        lt_merge::Deghost::High
    );
    assert_eq!(
        lt_merge::Deghost::from(Deghost::Off),
        lt_merge::Deghost::Off
    );
}

/// Two real A9 frames at one focal length (when samples have them) preview as an HDR
/// merge.
#[test]
fn real_samples_preview() {
    let samples = lt_testdata::samples_with_ext("arw");
    let focal = |p: &PathBuf| lt_raw::read_metadata(p).ok().and_then(|m| m.focal_length);
    let pair = samples.iter().enumerate().find_map(|(i, a)| {
        samples[i + 1..]
            .iter()
            .find(|b| focal(a).is_some() && focal(a) == focal(b))
            .map(|b| (a, b))
    });
    let Some((a, b)) = pair else {
        return;
    };
    let paths = [a, b]
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let p = block_on(merge_preview(paths, HDR, 256, CancelToken::new())).unwrap();
    assert!(p.width.max(p.height) <= 256 && !p.jpeg.is_empty());
}

/// Mixed cameras and zoom settings are refused (one colour matrix, one focal length).
#[test]
fn mixed_cameras_and_focal_lengths_are_refused() {
    let a9 = lt_raw::ImageMeta {
        make: "SONY".into(),
        model: "ILCE-9".into(),
        focal_length: Some(24.0),
        ..lt_raw::ImageMeta::default()
    };
    let other = lt_raw::ImageMeta {
        model: "ILCE-7M4".into(),
        ..a9.clone()
    };
    let zoomed = lt_raw::ImageMeta {
        focal_length: Some(28.0),
        ..a9.clone()
    };
    let close = lt_raw::ImageMeta {
        focal_length: Some(24.3),
        ..a9.clone()
    };
    assert!(check_compatible(&[Some(&a9), Some(&close)]).is_ok());
    assert!(check_compatible(&[None, None]).is_ok());
    for bad in [&other, &zoomed] {
        let e = check_compatible(&[Some(&a9), Some(bad)]).unwrap_err();
        assert!(matches!(e, DsError::InvalidArgument { .. }), "{e:?}");
    }
    // Through a merge: a panorama of two cameras is refused before any work.
    let d = tempfile::tempdir().unwrap();
    let mut sources: Vec<Source> = [&a9, &other]
        .iter()
        .map(|m| Source {
            path: PathBuf::new(),
            frame: Frame::new(8, 8, vec![0; 192]).unwrap(),
            meta: Some((*m).clone()),
            color: RawImage {
                size: Size::new(8, 8),
                cfa: CfaPattern::LinearRgb,
                data: Vec::new(),
                black_level: [0.0; 4],
                white_level: 65535.0,
                wb_as_shot: [1.0; 3],
                wb_daylight: [1.0; 3],
                cam_to_xyz: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                orientation: Orientation::Normal,
                default_crop: None,
            },
        })
        .collect();
    let monitor = Monitor {
        token: &CancelToken::new(),
        report: &|_| {},
        last: AtomicU64::new(0),
    };
    let pano = MergeOptions {
        kind: MergeKind::Panorama,
        ..HDR
    };
    assert!(matches!(
        run_merge(&mut sources, pano, &[], &monitor),
        Err(DsError::InvalidArgument { message }) if message.contains("one camera")
    ));
    drop(d);
}

/// Cancelling mid-merge (after decoding, during the merge) leaves nothing behind;
/// cancelling once the file is written keeps it, registered.
#[test]
fn cancelling_mid_merge_leaves_nothing_and_late_cancels_keep_the_photo() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("C");
    let (h, ids) = brackets(&root);
    let capture_files = || {
        let mut v: Vec<String> = fs::read_dir(root.join("Capture"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != ".lt")
            .collect();
        v.sort();
        v
    };
    let before = capture_files();
    let t = CancelToken::new();
    let seen = AtomicU64::new(0);
    let (tr, sr) = (&t, &seen);
    let cancel_at = |at: u64| {
        move |p: u64| {
            sr.fetch_max(p, Ordering::Relaxed);
            if p >= at {
                tr.cancel();
            }
        }
    };
    let r = h.merge_now(&decode_file, &ids, HDR, &t, &cancel_at(400));
    assert!(matches!(r, Err(DsError::Cancelled)), "{r:?}");
    assert!(
        seen.load(Ordering::Relaxed) < 900,
        "stopped during the merge"
    );
    assert_eq!(capture_files(), before, "no DNG and no temp file");
    assert!(variant_named_opt(&h, "B1-HDR.dng").is_none());

    let late = CancelToken::new();
    let report = |p: u64| {
        if p >= 980 {
            late.cancel();
        }
    };
    let out = h
        .merge_now(&decode_file, &ids, HDR, &late, &report)
        .unwrap();
    assert!(late.is_cancelled());
    assert_eq!(out.file_name, "B1-HDR.dng");
    assert!(variant_named_opt(&h, "B1-HDR.dng").is_some());
}

fn variant_named_opt(h: &Arc<SessionHandle>, name: &str) -> Option<crate::session::VariantRecord> {
    block_on(h.clone().variants(CancelToken::new()))
        .unwrap()
        .into_iter()
        .find(|v| v.file_name == name)
}

/// A portrait bracket: the DNG keeps the sensor data and its orientation, and the
/// embedded preview is stored in sensor orientation too, so a reader turning it by
/// the DNG's orientation shows it upright (not turned twice).
#[test]
fn portrait_hdr_preview_is_turned_once() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("R");
    let (h, ids) = session_with(&root, &["P1.dng", "P2.dng", "P3.dng"], |p, i| {
        let e = [0.25f32, 1.0, 4.0][i];
        dng_oriented(p, 256, 192, Orientation::Rotate90, |x, y| {
            radiance(x, y) * e
        });
    });
    let out = block_on(h.clone().merge_photos(ids, HDR, None, CancelToken::new())).unwrap();
    let dest = Path::new(&out.original_path);
    let raw = lt_raw::decode(dest).unwrap();
    assert_eq!(
        (raw.size, raw.orientation),
        (Size::new(256, 192), Orientation::Rotate90)
    );
    let p = lt_raw::embedded_preview(dest).unwrap();
    assert_eq!(p.orientation, Orientation::Rotate90);
    // Stored landscape like the sensor; displayed (turned once) portrait like the photo.
    assert!(p.size.width > p.size.height, "{:?}", p.size);
    assert!(p.orientation.apply_to(p.size).is_portrait());
}

/// A second preview of the same files reuses their decodes; a changed file is decoded again.
#[test]
fn previews_reuse_decodes_until_a_file_changes() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("Q");
    let (_h, _) = brackets(&root);
    let paths: Vec<String> = ["B1", "B2", "B3"]
        .iter()
        .map(|n| {
            root.join(format!("Capture/{n}.dng"))
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let first = block_on(merge_preview(paths.clone(), HDR, 96, CancelToken::new())).unwrap();
    assert!(paths.iter().all(|p| preview_cached(Path::new(p), 96)));
    let again = block_on(merge_preview(paths.clone(), HDR, 96, CancelToken::new())).unwrap();
    assert_eq!((first.width, first.height), (again.width, again.height));
    dng(Path::new(&paths[1]), 256, 192, |x, y| radiance(x, y) * 0.9);
    assert!(!preview_cached(Path::new(&paths[1]), 96));
}

/// Peak memory of a 9 × 24 MP HDR merge through the whole path (decode stand-in that
/// allocates a Bayer mosaic like `lt_raw::decode`, RCD, merge, preview, DNG write,
/// registration). Run alone, release:
/// `cargo test -p lt-ffi --release --lib merge_memory_budget -- --ignored --nocapture`.
#[test]
#[ignore = "budget: 9 × 24 MP, run alone in release"]
fn merge_memory_budget() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("M");
    let names: Vec<String> = (1..=9).map(|i| format!("F{i}.ARW")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let (h, ids) = session_with(&root, &refs, |p, i| fs::write(p, [i as u8; 16]).unwrap());
    let (w, ht) = (6000u32, 4000u32);
    let decode = |p: &Path| -> Result<RawImage, DsError> {
        let i = p.file_stem().unwrap().to_string_lossy()[1..]
            .parse::<u32>()
            .unwrap();
        let e = (f32::from(i as u16) - 5.0).exp2() * 0.5;
        let data = (0..w * ht)
            .map(|k| {
                let (x, y) = ((k % w) as f32, (k / w) as f32);
                (512.0 + (texture(x, y) * e).min(1.0) * 15_000.0) as u16
            })
            .collect();
        Ok(RawImage {
            size: Size::new(w, ht),
            cfa: CfaPattern::Rggb,
            data,
            black_level: [512.0; 4],
            white_level: 16_383.0,
            wb_as_shot: [2.0, 1.0, 1.5],
            wb_daylight: lt_raw::color::daylight_multipliers(&XYZ_TO_CAM).unwrap(),
            cam_to_xyz: lt_raw::color::cam_to_xyz(&XYZ_TO_CAM).unwrap(),
            orientation: Orientation::Normal,
            default_crop: None,
        })
    };
    let start = std::time::Instant::now();
    let out = h
        .merge_now(&decode, &ids, HDR, &CancelToken::new(), &|_| {})
        .unwrap();
    let secs = start.elapsed().as_secs_f32();
    // SAFETY: getrusage writes into the zeroed struct we own.
    #[allow(unsafe_code)]
    let peak = unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &raw mut u);
        u.ru_maxrss as f64 / 1e9 // bytes on macOS
    };
    eprintln!(
        "9 × 24 MP HDR merge: {secs:.1} s, peak RSS {peak:.2} GB ({})",
        out.file_name
    );
    assert!(peak < 2.5, "peak {peak:.2} GB");
}

#[test]
fn best_shot_keeps_the_best_scored_frames_in_capture_order() {
    let scored = |s: &[Option<f32>]| -> Vec<BurstSignals> {
        s.iter()
            .map(|&score| BurstSignals {
                faces: Vec::new(),
                score,
            })
            .collect()
    };
    assert_eq!(burst_frames(&[], 3), vec![0, 1, 2]);
    let n = BURST_MAX_FRAMES + 2;
    // Two unscored frames and the lowest score drop out; the rest keep their order.
    let mut s: Vec<Option<f32>> = (0..n).map(|i| Some(0.5 + i as f32 / 100.0)).collect();
    s[1] = None;
    s[4] = Some(f32::NAN);
    s[7] = Some(0.1);
    let keep = burst_frames(&scored(&s), n);
    assert_eq!(keep.len(), BURST_MAX_FRAMES);
    assert!(!keep.contains(&1) && !keep.contains(&4) && keep.contains(&7));
    assert!(keep.windows(2).all(|w| w[0] < w[1]));
    // Without scores: the first ones.
    assert_eq!(
        burst_frames(&[], n),
        (0..BURST_MAX_FRAMES).collect::<Vec<_>>()
    );
    assert_eq!(
        kept(MergeKind::Hdr, &[], n).len(),
        n,
        "only Best Shot drops frames"
    );
    assert_eq!(pick(&["a", "b", "c"], &[0, 2, 9]), vec!["a", "c"]);
    assert!(check_signals(&[], 3).is_ok());
    assert!(check_signals(&scored(&[None, None]), 2).is_ok());
    assert!(matches!(
        check_signals(&scored(&[None]), 2),
        Err(DsError::InvalidArgument { .. })
    ));
    assert_eq!(candidate_name("A", MergeKind::Burst, 1), "A-Best.dng");
    assert_eq!(candidate_name("A", MergeKind::Burst, 2), "A-Best-2.dng");
}

/// A portrait burst scene, upright coordinates (192 × 256): a textured background and one
/// face whose eyes are open or closed.
fn burst_scene(x: f32, y: f32, open: bool) -> f32 {
    if ((x - 96.0) / 30.0).powi(2) + ((y - 128.0) / 38.0).powi(2) > 1.0 {
        return texture(x, y) * 0.7;
    }
    let mut v = 0.45 + 0.05 * texture(x * 0.3, y * 0.3);
    for cx in [84.0, 108.0] {
        let d = ((x - cx) / 7.0).powi(2) + ((y - 118.0) / 4.0).powi(2);
        if d <= 1.0 {
            v = match (open, d < 0.35) {
                (true, true) => 0.04,
                (true, false) => 0.85,
                (false, _) if (y - 118.0).abs() < 0.8 => 0.15,
                (false, _) => v,
            };
        }
    }
    v
}

const BURST_SHIFTS: [(f32, f32); 3] = [(0.0, 0.0), (2.0, -1.5), (-1.5, 1.0)];

/// Three portrait frames (the sensor holds them turned, `Rotate90`): only the second has
/// its eyes open.
fn burst(root: &Path) -> (Arc<SessionHandle>, Vec<i64>, Vec<String>, Vec<BurstSignals>) {
    let names = ["S1.dng", "S2.dng", "S3.dng"];
    let (h, ids) = session_with(root, &names, |p, i| {
        let (dx, dy) = BURST_SHIFTS[i];
        // Upright (ux, uy) of sensor (x, y) under Rotate90: (191 − y, x).
        dng_oriented(p, 256, 192, Orientation::Rotate90, |x, y| {
            burst_scene((191 - y) as f32 + dx, x as f32 + dy, i == 1)
        });
    });
    let paths = names
        .iter()
        .map(|n| root.join("Capture").join(n).to_string_lossy().into_owned())
        .collect();
    let signals = BURST_SHIFTS
        .iter()
        .enumerate()
        .map(|(i, &(dx, dy))| BurstSignals {
            faces: vec![BurstFaceSignal {
                bounds: crate::frame::NormRect {
                    x: (66.0 - dx) / 192.0,
                    y: (90.0 - dy) / 256.0,
                    w: 60.0 / 192.0,
                    h: 76.0 / 256.0,
                },
                eyes_open: Some(if i == 1 { 0.95 } else { 0.05 }),
                quality: Some(0.7),
            }],
            score: Some([0.9, 0.6, 0.5][i]),
        })
        .collect();
    (h, ids, paths, signals)
}

#[test]
fn best_shot_takes_the_open_eyes_and_writes_an_upright_dng() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("B");
    let (h, ids, paths, signals) = burst(&root);
    let p = block_on(burst_preview(
        paths.clone(),
        signals.clone(),
        256,
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!((p.width, p.height), (192, 256), "upright");
    assert_eq!(
        (p.faces_replaced, p.faces_kept, p.unaligned_frames),
        (1, 0, 0)
    );
    assert_eq!(p.exposure_ev, 0.0);
    assert!(matches!(
        block_on(burst_preview(
            paths,
            signals[..1].to_vec(),
            256,
            CancelToken::new()
        )),
        Err(DsError::InvalidArgument { .. })
    ));

    let out = block_on(
        h.clone()
            .merge_burst(ids, signals, None, CancelToken::new()),
    )
    .unwrap();
    assert_eq!(out.file_name, "S1-Best.dng");
    assert!(variant_named_opt(&h, "S1-Best.dng").is_some());
    let raw = lt_raw::decode(Path::new(&out.original_path)).unwrap();
    assert_eq!(
        (raw.size, raw.orientation),
        (Size::new(192, 256), Orientation::Normal)
    );
    // The pupils are dark (open eyes), not the reference's closed lids.
    let green =
        |x: u32, y: u32| f32::from(raw.data[((y * 192 + x) * 3 + 1) as usize]) / raw.white_level;
    for x in [84, 108] {
        assert!(green(x, 118) < 0.15, "pupil at {x}: {}", green(x, 118));
    }
    // Away from the face: the reference.
    assert!((green(10, 10) - burst_scene(10.0, 10.0, false)).abs() < 0.01);
    let adj = lock(&h.session)
        .load_adjustments(VariantId(out.variant_id))
        .unwrap();
    assert_eq!(adj.exposure.exposure_ev, 0.0);
    assert!(
        adj.lens.enabled,
        "geometry is the camera's: lens corrections stay on"
    );
}
