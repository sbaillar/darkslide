use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use ds_core::{Adjustments, Size};
use ds_export::{ExifMeta, Upscaler};
use ds_session::Session;
use futures_executor::block_on;

use super::*;

/// Adobe `ColorMatrix2` of the Sony ILCE-9.
const A9: [f32; 9] = [
    0.6389, -0.1703, -0.0378, -0.4562, 1.2265, 0.2587, -0.067, 0.1489, 0.655,
];

/// A linear DNG "RAW" (`w`×`h`, a gradient with texture) at `dir/name`.
fn source_dng(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
    let info = DngInfo {
        size: Size::new(w, h),
        cam_to_xyz: ds_raw::color::cam_to_xyz(&A9).unwrap(),
        wb_as_shot: [2.2, 1.0, 1.6],
        wb_daylight: ds_raw::color::daylight_multipliers(&A9).unwrap(),
        orientation: 8,
        default_crop: None,
        exif: ExifMeta {
            make: Some("SONY".into()),
            model: Some("ILCE-9".into()),
            iso: Some(800),
            ..ExifMeta::default()
        },
        preview: None,
    };
    let mut wr = LinearDngWriter::create(dir, name, info).unwrap();
    let data: Vec<u16> = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            let t = ((x * 7 + y * 3) % 11) * 300;
            [2_000 + x * 40 + t, 3_000 + y * 50 + t, 4_000 + t]
        })
        .map(|v| v.min(40_000) as u16)
        .collect();
    wr.write_rows(&data).unwrap();
    wr.finish(&Namer::default()).unwrap()
}

/// Run a nearest-neighbour 2× double over tiles `range` of `task`.
fn run_tiles(task: &UpscaleTask, range: std::ops::Range<u32>) {
    let t = task.tile_size() as usize;
    let o = t * task.output_scale() as usize;
    let mut input = vec![0u16; 3 * t * t];
    let mut output = vec![0u16; 3 * o * o];
    for i in range {
        task.read_tile(i, input.as_mut_ptr() as u64, input.len() as u64)
            .unwrap();
        for c in 0..3 {
            for y in 0..o {
                for x in 0..o {
                    output[c * o * o + y * o + x] = input[c * t * t + (y / 2) * t + x / 2];
                }
            }
        }
        task.write_tile(
            i,
            output.as_ptr() as u64,
            output.len() as u64,
            o as u64,
            (o * o) as u64,
        )
        .unwrap();
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
#[allow(clippy::too_many_lines)] // one end-to-end story: name, upscale, DNG, session
fn a_photo_upscales_into_a_new_dng_in_the_session_with_its_adjustments() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("2026-10-07 Upscale");
    let mut session = Session::create(&root, "Upscale").unwrap();
    let capture = root.join("Capture");
    let src = source_dng(&capture, "DSC00001.dng", 300, 200);
    let (meta, _) = read_meta(&src).unwrap();
    let hash = identity::hash_original(&src, None, &|| false).unwrap().file;
    let image = session
        .add_original("Capture/DSC00001.dng", hash, meta)
        .unwrap();
    let source_variant = session.variants(image).unwrap()[0];
    let mut adj = Adjustments::default();
    adj.exposure.exposure_ev = 0.7;
    session.save_adjustments(source_variant, &adj).unwrap();
    let iptc = ds_core::IptcMeta {
        title: "Falls".into(),
        creator: "Alexa".into(),
        ..ds_core::IptcMeta::default()
    };
    session.set_iptc(&[source_variant], &iptc).unwrap();
    let handle = Arc::new(SessionHandle::plain(session));

    let name = block_on(handle.clone().free_derived_name(
        src.to_string_lossy().into_owned(),
        "Upscaled".into(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(name, "DSC00001-Upscaled.dng");
    let task = block_on(begin_upscale(
        src.to_string_lossy().into_owned(),
        name,
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(
        (task.tile_count(), task.tile_size(), task.output_scale()),
        (2, 256, 2)
    );
    run_tiles(&task, 0..2);
    assert!(
        task.take_image().is_none(),
        "a photo's rows went to its DNG"
    );
    let out = block_on(task.clone().finish(CancelToken::new())).unwrap();
    assert_eq!(Path::new(&out), capture.join("DSC00001-Upscaled.dng"));
    assert!(
        block_on(task.finish(CancelToken::new())).is_err(),
        "finished once"
    );

    // Twice the size, the same colour and orientation, and the pixels of the double.
    let original = ds_raw::decode(&src).unwrap();
    let up = ds_raw::decode(Path::new(&out)).unwrap();
    assert_eq!(up.size, Size::new(600, 400));
    for (a, b) in up
        .cam_to_xyz
        .iter()
        .flatten()
        .zip(original.cam_to_xyz.iter().flatten())
    {
        assert!(
            (a - b).abs() < 1e-5,
            "{:?} vs {:?}",
            up.cam_to_xyz,
            original.cam_to_xyz
        );
    }
    assert_eq!(up.orientation, original.orientation);
    for (x, y) in [(0usize, 0usize), (321, 17), (599, 399), (450, 260)] {
        for c in 0..3 {
            let got = f32::from(up.data[(y * 600 + x) * 3 + c]);
            let want = f32::from(original.data[((y / 2) * 300 + x / 2) * 3 + c]);
            assert!(
                (got - want).abs() <= want * 3e-3 + 2.0,
                "({x},{y}) {got} vs {want}"
            );
        }
    }
    let m = ds_raw::read_metadata(Path::new(&out)).unwrap();
    assert_eq!((m.model.as_str(), m.iso), ("ILCE-9", Some(800)));

    // Added to the session as a new photo that starts with the source's edit.
    let v = block_on(
        handle
            .clone()
            .add_derived(out.clone(), source_variant.0, CancelToken::new()),
    )
    .unwrap();
    let s = lock(&handle.session);
    assert_eq!(s.image_count().unwrap(), 2);
    assert_eq!(s.load_adjustments(VariantId(v)).unwrap(), adj);
    assert_eq!(
        s.variant(VariantId(v)).unwrap().iptc,
        iptc,
        "title, caption and credits travel with it"
    );
    let info = s.image(s.variant(VariantId(v)).unwrap().image).unwrap();
    assert_eq!(info.rel_path.as_str(), "Capture/DSC00001-Upscaled.dng");
    assert_eq!((info.meta.width, info.meta.height), (600, 400));
    assert!(
        info.content_hash.is_some(),
        "the DNG's sensor data identifies it"
    );
    drop(s);
    refusals(&handle, d.path(), &capture, &out, source_variant);
}

/// `add_derived` refuses a file outside the session, an unreadable one, and one already
/// added.
fn refusals(
    handle: &Arc<SessionHandle>,
    outside: &Path,
    capture: &Path,
    out: &str,
    source: VariantId,
) {
    // Outside the session, unreadable, or already added: refused.
    let elsewhere = source_dng(outside, "other.dng", 8, 8);
    let add = |p: &Path| {
        block_on(handle.clone().add_derived(
            p.to_string_lossy().into_owned(),
            source.0,
            CancelToken::new(),
        ))
    };
    assert!(add(&elsewhere).is_err());
    fs::write(capture.join("junk.dng"), b"not a dng").unwrap();
    assert!(add(&capture.join("junk.dng")).is_err());
    assert!(add(Path::new(out)).is_err());
}

#[test]
fn a_cancelled_or_incomplete_upscale_leaves_no_file() {
    let d = tempfile::tempdir().unwrap();
    let src = source_dng(d.path(), "a.dng", 300, 200);
    let raw = ds_raw::decode(&src).unwrap();
    // Cancelled half way: the task is dropped.
    let task = UpscaleTask::for_raw(&src, "a-Upscaled.dng", &raw).unwrap();
    run_tiles(&task, 0..1);
    drop(task);
    assert_eq!(names(d.path()), ["a.dng"]);
    // Finished with a tile missing: an error, and nothing placed.
    let task = Arc::new(UpscaleTask::for_raw(&src, "a-Upscaled.dng", &raw).unwrap());
    run_tiles(&task, 1..2);
    let e = block_on(task.clone().finish(CancelToken::new())).unwrap_err();
    assert!(e.to_string().contains("1 of 2 tiles"), "{e}");
    assert_eq!(names(d.path()), ["a.dng"]);
    // ... and the task can't take tiles any more.
    let o = vec![0u16; 3 * 512 * 512];
    assert!(
        task.write_tile(0, o.as_ptr() as u64, o.len() as u64, 512, 512 * 512)
            .is_err()
    );
    // Only next to the original, on a volume the session writes to.
    let e = UpscaleTask::for_raw(Path::new("/nonexistent/ds-upscale/a.dng"), "x.dng", &raw)
        .unwrap_err();
    assert!(
        e.to_string()
            .contains("can't be written next to the original"),
        "{e}"
    );
    assert!(
        UpscaleTask::for_raw(Path::new("/"), "x.dng", &raw).is_err(),
        "no folder"
    );
    // A file that isn't a RAW can't be upscaled.
    fs::write(d.path().join("x.ARW"), b"nope").unwrap();
    let path = d.path().join("x.ARW").to_string_lossy().into_owned();
    assert!(block_on(begin_upscale(path, "x.dng".into(), CancelToken::new())).is_err());
    let cancelled = CancelToken::new();
    cancelled.cancel();
    let path = src.to_string_lossy().into_owned();
    assert!(block_on(begin_upscale(path, "x.dng".into(), cancelled)).is_err());
}

/// Review U-M1: a cancel that arrives while the DNG is being finished, before it is
/// placed, leaves no file.
#[test]
fn a_cancel_during_finish_leaves_no_file() {
    let d = tempfile::tempdir().unwrap();
    let src = source_dng(d.path(), "a.dng", 300, 200);
    let raw = ds_raw::decode(&src).unwrap();
    let task = UpscaleTask::for_raw(&src, "a-Upscaled.dng", &raw).unwrap();
    run_tiles(&task, 0..2);
    assert!(matches!(
        task.finish_with(&|| true),
        Err(DsError::Cancelled)
    ));
    drop(task);
    assert_eq!(names(d.path()), ["a.dng"]);
    // Not cancelled: placed.
    let task = UpscaleTask::for_raw(&src, "a-Upscaled.dng", &raw).unwrap();
    run_tiles(&task, 0..2);
    let out = task.finish_with(&|| false).unwrap();
    assert!(out.ends_with("a-Upscaled.dng"));
}

/// Review U-M2: the new photo's name is free in the session too (a photo whose file
/// was deleted in Finder, a sidecar left behind), so adding it never fails after the
/// whole DNG was written.
#[test]
fn the_derived_name_is_free_in_the_session_as_well_as_on_disk() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("S");
    let mut session = Session::create(&root, "S").unwrap();
    let capture = root.join("Capture");
    let src = source_dng(&capture, "DSC1.dng", 300, 200);
    // An earlier upscale, then deleted in Finder: its record is missing, not gone.
    let gone = source_dng(&capture, "DSC1-Upscaled.dng", 8, 8);
    let (meta, _) = read_meta(&gone).unwrap();
    let hash = identity::hash_original(&gone, None, &|| false)
        .unwrap()
        .file;
    session
        .add_original("Capture/DSC1-Upscaled.dng", hash, meta.clone())
        .unwrap();
    fs::remove_file(&gone).unwrap();
    // A file on disk takes -1; a sidecar left behind takes -2.
    source_dng(&capture, "DSC1-Upscaled-1.dng", 8, 8);
    let sidecar = root.join(
        ds_session::RelPath::new("Capture/DSC1-Upscaled-2.dng")
            .unwrap()
            .sidecar_rel(1),
    );
    fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
    fs::write(&sidecar, b"{}").unwrap();
    let (src_meta, _) = read_meta(&src).unwrap();
    let src_hash = identity::hash_original(&src, None, &|| false).unwrap().file;
    let image = session
        .add_original("Capture/DSC1.dng", src_hash, src_meta)
        .unwrap();
    let source_variant = session.variants(image).unwrap()[0];
    let handle = Arc::new(SessionHandle::plain(session));
    let name = block_on(handle.clone().free_derived_name(
        src.to_string_lossy().into_owned(),
        "Upscaled".into(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(name, "DSC1-Upscaled-3.dng");
    // The whole run then adds the photo.
    let task = block_on(begin_upscale(
        src.to_string_lossy().into_owned(),
        name,
        CancelToken::new(),
    ))
    .unwrap();
    run_tiles(&task, 0..2);
    let out = block_on(task.finish(CancelToken::new())).unwrap();
    assert!(out.ends_with("DSC1-Upscaled-3.dng"));
    block_on(
        handle
            .clone()
            .add_derived(out, source_variant.0, CancelToken::new()),
    )
    .unwrap();
    // A source outside the session is refused.
    let elsewhere = source_dng(d.path(), "x.dng", 8, 8);
    assert!(
        block_on(handle.clone().free_derived_name(
            elsewhere.to_string_lossy().into_owned(),
            "Upscaled".into(),
            CancelToken::new(),
        ))
        .is_err()
    );
}

#[test]
fn tile_calls_reject_bad_memory() {
    let img = Image16::new(Size::new(40, 30), vec![1000; 40 * 30 * 3]).unwrap();
    let task = UpscaleTask::for_image(&img).unwrap();
    let mut buf = vec![0u16; 3 * 256 * 256];
    assert!(task.read_tile(0, 0, 10).is_err(), "null");
    assert!(
        task.read_tile(0, buf.as_mut_ptr() as u64 + 1, 10).is_err(),
        "misaligned"
    );
    assert!(
        task.read_tile(0, buf.as_mut_ptr() as u64, 10).is_err(),
        "short"
    );
    assert!(
        task.read_tile(9, buf.as_mut_ptr() as u64, buf.len() as u64)
            .is_err()
    );
    let p = buf.as_ptr() as u64;
    assert!(
        task.write_tile(0, p, buf.len() as u64, 512, 512 * 512)
            .is_err(),
        "short"
    );
    assert!(
        task.write_tile(0, p, buf.len() as u64, u64::MAX, 0)
            .is_err(),
        "stride"
    );
    assert!(task.take_image().is_none(), "nothing upscaled yet");
    assert!(
        block_on(Arc::new(task).finish(CancelToken::new())).is_err(),
        "not a photo"
    );
    assert!(format!("{:?}", UpscaleTask::for_image(&img).unwrap()).contains("UpscaleTask"));
}

/// The app's provider, as a double: runs the nearest double over every tile, or not.
pub(crate) struct Provider {
    pub(crate) calls: AtomicUsize,
    run: bool,
    answer: fn() -> Result<bool, DsError>,
}

impl UpscaleProvider for Provider {
    fn upscale(&self, task: Arc<UpscaleTask>, _: Arc<CancelToken>) -> Result<bool, DsError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.run {
            run_tiles(&task, 0..task.tile_count());
        }
        (self.answer)()
    }
}

pub(crate) fn provider(run: bool, answer: fn() -> Result<bool, DsError>) -> Arc<Provider> {
    Arc::new(Provider {
        calls: AtomicUsize::new(0),
        run,
        answer,
    })
}

#[test]
fn exports_upscale_through_the_apps_provider() {
    let img = Image16::new(
        Size::new(40, 30),
        (0..40 * 30 * 3).map(|i| (i * 37 % 60_000) as u16).collect(),
    )
    .unwrap();
    let up = |p: Option<Arc<Provider>>| {
        ExportUpscaler::new(p.map(|p| p as Arc<dyn UpscaleProvider>)).upscale(&img)
    };
    let big = up(Some(provider(true, || Ok(true)))).unwrap().unwrap();
    assert_eq!(big.size, Size::new(80, 60));
    for (x, y) in [(0usize, 0usize), (79, 59), (13, 44)] {
        for c in 0..3 {
            let got = big.data[(y * 80 + x) * 3 + c];
            let want = img.data[((y / 2) * 40 + x / 2) * 3 + c];
            assert!(got.abs_diff(want) <= 32, "({x},{y}) {got} vs {want}");
        }
    }
    assert!(up(None).unwrap().is_none(), "no provider: resample");
    // Review U-M3: a render whose 2× would pass the upscaler's limit (here 65 MP → 260
    // MP) is resampled, the model never asked. (Zeroed memory: not touched.)
    let p = provider(true, || Ok(true));
    let huge = Image16::new(Size::new(9_000, 7_200), vec![0; 9_000 * 7_200 * 3]).unwrap();
    let big = ExportUpscaler::new(Some(p.clone() as Arc<dyn UpscaleProvider>)).upscale(&huge);
    assert!(big.unwrap().is_none());
    assert_eq!(p.calls.load(Ordering::SeqCst), 0);
    assert!(
        up(Some(provider(false, || Ok(false)))).unwrap().is_none(),
        "no model"
    );
    let e = up(Some(provider(false, || Ok(true)))).unwrap_err();
    assert!(e.contains("every tile"), "{e}");
    assert_eq!(
        up(Some(provider(false, || Err(DsError::Cancelled)))).unwrap_err(),
        "cancelled"
    );
    let e = up(Some(provider(false, || Err(DsError::invalid("boom"))))).unwrap_err();
    assert!(e.contains("boom"), "{e}");
    let u = ExportUpscaler::new(None);
    u.cancel();
    assert!(u.cancel_token().is_cancelled());
}
