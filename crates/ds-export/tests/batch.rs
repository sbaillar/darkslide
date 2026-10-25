use std::collections::HashSet;
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ds_core::Size;
use ds_export::{
    BatchOptions, ExifMeta, ExportError, ExportFormat, ExportItem, ExportRecipe, Gps, HeicEncoder,
    Image16, OutputSpace, Rendered, Renderer, Sizing, run_batch,
};
use ds_template::Template;

fn gradient(w: u32, h: u32) -> Rendered {
    let data = (0..w * h)
        .flat_map(|i| {
            [
                ((i % w) * 65_535 / w) as u16,
                ((i / w) * 65_535 / h) as u16,
                20_000,
            ]
        })
        .collect();
    Rendered::Rgb16 {
        size: Size::new(w, h),
        data,
    }
}

fn item(id: u64, name: &str) -> ExportItem {
    ExportItem {
        id,
        name: name.into(),
        seq: id,
        meta: ExifMeta {
            make: Some("SONY".into()),
            model: Some("ILCE-9".into()),
            gps: Some(Gps {
                latitude: 1.0,
                longitude: 2.0,
                altitude_m: None,
            }),
            ..Default::default()
        },
    }
}

fn files(dir: &Path) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .map(|rd| {
            rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn no_progress(_: &ds_export::ExportProgress) {}

#[test]
fn multiple_recipes_one_render_per_space() {
    let d = tempfile::tempdir().unwrap();
    let mut jpg = ExportRecipe::new(
        "web",
        ExportFormat::Jpeg { quality: 90 },
        d.path().join("web"),
    );
    jpg.sizing = Sizing::LongEdge(40);
    jpg.sharpen = 0.5;
    jpg.file_name = Template::parse("{camera}_{seq:3}").unwrap();
    jpg.exif.strip_gps = true;
    let png = ExportRecipe::new("png", ExportFormat::Png8, d.path().join("png"));
    let mut tif = ExportRecipe::new("tif", ExportFormat::Tiff16, d.path().join("tif"));
    tif.space = OutputSpace::DisplayP3;
    let renders = Mutex::new(Vec::new());
    let render = |it: &ExportItem, space: OutputSpace| {
        renders.lock().unwrap().push((it.id, space));
        Ok(gradient(80, 60))
    };
    let seen = Mutex::new(Vec::new());
    let progress = |p: &ds_export::ExportProgress| seen.lock().unwrap().push(p.clone());
    let items = [item(1, "DSC001"), item(2, "DSC002")];
    let rep = run_batch(
        &items,
        &[jpg, png, tif],
        &render,
        BatchOptions::default(),
        &progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(!rep.cancelled);
    assert_eq!(rep.written.len(), 6);
    // Two renders per item: one sRGB (jpg+png), one P3 (tif).
    let r = renders.lock().unwrap();
    assert_eq!(r.len(), 4);
    assert_eq!(
        r.iter().filter(|x| x.1 == OutputSpace::DisplayP3).count(),
        2
    );
    assert_eq!(
        files(&d.path().join("web")),
        ["ILCE-9_001.jpg", "ILCE-9_002.jpg"]
    );
    assert_eq!(files(&d.path().join("png")), ["DSC001.png", "DSC002.png"]);
    assert_eq!(files(&d.path().join("tif")), ["DSC001.tif", "DSC002.tif"]);
    let web = rep.written.iter().find(|w| w.recipe == 0).unwrap();
    assert_eq!(web.size, Size::new(40, 30));
    assert!(web.bytes > 0 && web.bytes == fs::metadata(&web.path).unwrap().len());
    // GPS stripped for the web recipe, camera kept.
    let bytes = fs::read(&web.path).unwrap();
    let e = exif::Reader::new()
        .read_from_container(&mut Cursor::new(&bytes))
        .unwrap();
    assert!(
        e.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY)
            .is_none()
    );
    assert!(e.get_field(exif::Tag::Model, exif::In::PRIMARY).is_some());
    let s = seen.lock().unwrap();
    assert_eq!(s.len(), 2);
    assert!(s.iter().any(|p| p.images_done == 2 && p.images_total == 2));
}

#[test]
fn memory_bound_limits_concurrent_renders() {
    let d = tempfile::tempdir().unwrap();
    let r = ExportRecipe::new("r", ExportFormat::Png8, d.path());
    let live = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let threads = Mutex::new(HashSet::new());
    let render = |_: &ExportItem, _| {
        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        threads.lock().unwrap().insert(std::thread::current().id());
        std::thread::yield_now();
        live.fetch_sub(1, Ordering::SeqCst);
        Ok(gradient(16, 16))
    };
    let items: Vec<_> = (0..24).map(|i| item(i, &format!("img{i}"))).collect();
    let opts = BatchOptions {
        max_in_flight: 3,
        heic: None,
        jpeg: None,
        upscaler: None,
        text: None,
        cancel: None,
    };
    let rep = run_batch(
        &items,
        &[r],
        &render,
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.written.len(), 24);
    assert!(peak.load(Ordering::SeqCst) <= 3);
    assert!(threads.lock().unwrap().len() <= 3);
    assert_eq!(
        format!("{opts:?}"),
        "BatchOptions { max_in_flight: 3, heic: false, jpeg: false, upscaler: false, text: false, cancel: false }"
    );
}

#[test]
fn per_image_failures_do_not_stop_the_batch() {
    let d = tempfile::tempdir().unwrap();
    let r = ExportRecipe::new("r", ExportFormat::Jpeg { quality: 80 }, d.path());
    let render = |it: &ExportItem, _| match it.id {
        1 => Err("decode failed".to_string()),
        2 => Ok(Rendered::Rgba8 {
            size: Size::new(4, 4),
            data: vec![0; 10],
        }),
        3 => Ok(Rendered::Rgba8 {
            size: Size::new(2, 2),
            data: vec![200; 16],
        }),
        _ => Ok(gradient(8, 8)),
    };
    let items = [item(0, "a"), item(1, "b"), item(2, "c"), item(3, "d")];
    let rep = run_batch(
        &items,
        &[r],
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.written.len(), 2);
    assert_eq!(rep.failed.len(), 2);
    let f1 = rep.failed.iter().find(|f| f.item == Some(1)).unwrap();
    assert!(matches!(&f1.error, ExportError::Render(m) if m == "decode failed"));
    assert_eq!(f1.recipe, None);
    let f2 = rep.failed.iter().find(|f| f.item == Some(2)).unwrap();
    assert!(matches!(f2.error, ExportError::InvalidImage(_)));
    assert_eq!(files(d.path()), ["a.jpg", "d.jpg"]);
}

#[test]
fn invalid_recipe_reported_once_valid_ones_run() {
    let d = tempfile::tempdir().unwrap();
    let bad = ExportRecipe::new("bad", ExportFormat::Jpeg { quality: 0 }, d.path());
    let good = ExportRecipe::new("good", ExportFormat::Png16, d.path());
    let render = |_: &ExportItem, _| Ok(gradient(4, 4));
    let rep = run_batch(
        &[item(0, "x"), item(1, "y")],
        &[bad, good],
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.written.len(), 2);
    assert_eq!(rep.failed.len(), 1);
    assert_eq!((rep.failed[0].item, rep.failed[0].recipe), (None, Some(0)));
    assert!(rep.failed[0].error.to_string().contains("bad: quality 0"));
}

#[test]
fn cancel_mid_batch_leaves_no_temp_files() {
    let d = tempfile::tempdir().unwrap();
    let r = ExportRecipe::new("t", ExportFormat::Tiff16, d.path());
    let cancel = AtomicBool::new(false);
    let count = AtomicUsize::new(0);
    // Cancel while the third image is being rendered.
    let render = |_: &ExportItem, _| {
        if count.fetch_add(1, Ordering::SeqCst) == 2 {
            cancel.store(true, Ordering::SeqCst);
        }
        Ok(gradient(64, 64))
    };
    let items: Vec<_> = (0..20).map(|i| item(i, &format!("n{i}"))).collect();
    let opts = BatchOptions {
        max_in_flight: 1,
        heic: None,
        jpeg: None,
        upscaler: None,
        text: None,
        cancel: None,
    };
    let rep = run_batch(
        &items,
        std::slice::from_ref(&r),
        &render,
        opts,
        &no_progress,
        &cancel,
    );
    assert!(rep.cancelled);
    assert_eq!(rep.written.len(), 2);
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert_eq!(files(d.path()), ["n0.tif", "n1.tif"]);
    // Already cancelled: nothing is rendered or created.
    let out = d.path().join("x");
    let r = ExportRecipe::new("x", ExportFormat::Png8, &out);
    let rep = run_batch(&items, &[r], &render, opts, &no_progress, &cancel);
    assert!(rep.cancelled && rep.written.is_empty() && !out.exists());
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[test]
fn collisions_across_items_and_unicode_forms() {
    let d = tempfile::tempdir().unwrap();
    // Pre-existing NFC file; incoming item spelled NFD.
    fs::write(d.path().join("Caf\u{e9}.jpg"), b"keep me").unwrap();
    let r = ExportRecipe::new("r", ExportFormat::Jpeg { quality: 80 }, d.path());
    let render = |_: &ExportItem, _| Ok(gradient(8, 8));
    let items = [item(0, "Cafe\u{301}"), item(1, "DSC1"), item(2, "dsc1")];
    let opts = BatchOptions {
        max_in_flight: 1,
        heic: None,
        jpeg: None,
        upscaler: None,
        text: None,
        cancel: None,
    };
    let rep = run_batch(
        &items,
        &[r],
        &render,
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(
        fs::read(d.path().join("Caf\u{e9}.jpg")).unwrap(),
        b"keep me"
    );
    let names = files(d.path());
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(names.contains(&"Caf\u{e9}-1.jpg".to_string()), "{names:?}");
    assert!(
        names.contains(&"DSC1.jpg".to_string()) && names.contains(&"dsc1-1.jpg".to_string()),
        "{names:?}"
    );
}

#[cfg(unix)]
#[test]
fn unwritable_destination_fails_each_item_cleanly() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let ro = d.path().join("ro");
    fs::create_dir(&ro).unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    let r = ExportRecipe::new("r", ExportFormat::Png8, &ro);
    let render = |_: &ExportItem, _| Ok(gradient(4, 4));
    let rep = run_batch(
        &[item(0, "a"), item(1, "b")],
        &[r],
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(rep.failed.len(), 2);
    assert!(
        rep.failed
            .iter()
            .all(|f| matches!(f.error, ExportError::Io { .. }))
    );
    assert_eq!(files(&ro), [] as [std::string::String; 0]);
}

/// Renders at the recipe's size itself, like the GPU pipeline (full frame
/// 80×60). Item 2's sized render fails, item 3's panics.
#[derive(Default)]
struct SelfSizing {
    full: AtomicUsize,
    sized: Mutex<Vec<(u64, f32)>>,
}

impl Renderer for SelfSizing {
    fn render(&self, _: &ExportItem, _: OutputSpace) -> Result<Rendered, String> {
        self.full.fetch_add(1, Ordering::SeqCst);
        Ok(gradient(80, 60))
    }

    fn render_for(
        &self,
        it: &ExportItem,
        recipe: &ExportRecipe,
    ) -> Option<Result<Rendered, String>> {
        self.sized.lock().unwrap().push((it.id, recipe.sharpen));
        let size = recipe.target_size(Size::new(80, 60)).unwrap();
        match it.id {
            2 => Some(Err("device lost".into())),
            3 => panic!("boom"),
            _ => Some(Ok(gradient(size.width, size.height))),
        }
    }
}

#[test]
fn sized_render_write_errors_name_the_recipe() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let ro = d.path().join("ro");
    fs::create_dir(&ro).unwrap();
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
    let r = SelfSizing::default();
    let rep = run_batch(
        &[item(1, "a")],
        &[ExportRecipe::new("r", ExportFormat::Png8, &ro)],
        &r,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(r.full.load(Ordering::SeqCst), 0);
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(
        (rep.failed[0].item, rep.failed[0].recipe),
        (Some(1), Some(0))
    );
    assert!(matches!(rep.failed[0].error, ExportError::Io { .. }));
}

struct Heic(AtomicUsize);
impl HeicEncoder for Heic {
    fn encode(
        &self,
        img: &Image16,
        _: u8,
        _: &[u8],
        _: Option<&[u8]>,
        _: &[String],
        _: &ds_core::IptcMeta,
        _: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![1; img.data.len()])
    }
}

#[test]
fn heic_hook_and_missing_hook() {
    let d = tempfile::tempdir().unwrap();
    let r = ExportRecipe::new("h", ExportFormat::Heic { quality: 80 }, d.path());
    let render = |_: &ExportItem, _| Ok(gradient(4, 4));
    let heic = Heic(AtomicUsize::new(0));
    let opts = BatchOptions {
        max_in_flight: 2,
        heic: Some(&heic),
        jpeg: None,
        upscaler: None,
        text: None,
        cancel: None,
    };
    let rep = run_batch(
        &[item(0, "a")],
        std::slice::from_ref(&r),
        &render,
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert_eq!(heic.0.load(Ordering::SeqCst), 1);
    assert_eq!(files(d.path()), ["a.heic"]);
    assert_eq!(rep.written[0].bytes, 48);
    let rep = run_batch(
        &[item(1, "b")],
        &[r],
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(matches!(rep.failed[0].error, ExportError::NoHeicEncoder));
    assert_eq!(files(d.path()), ["a.heic"]);
}

/// The hardware JPEG hook replaces mozjpeg for JPEG recipes only; without it
/// JPEG is mozjpeg (a real JPEG), and HEIC still needs the HEIC hook.
#[test]
fn hardware_jpeg_hook_replaces_mozjpeg() {
    let d = tempfile::tempdir().unwrap();
    let jpg = ExportRecipe::new("j", ExportFormat::Jpeg { quality: 85 }, d.path());
    let render = |_: &ExportItem, _| Ok(gradient(4, 4));
    let hw = Heic(AtomicUsize::new(0));
    let opts = BatchOptions {
        max_in_flight: 1,
        heic: None,
        jpeg: Some(&hw),
        upscaler: None,
        text: None,
        cancel: None,
    };
    let rep = run_batch(
        &[item(0, "a")],
        std::slice::from_ref(&jpg),
        &render,
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(hw.0.load(Ordering::SeqCst), 1);
    assert_eq!(fs::read(d.path().join("a.jpg")).unwrap(), vec![1; 48]);
    // HEIC is not routed to the JPEG hook.
    let heic = ExportRecipe::new("h", ExportFormat::Heic { quality: 80 }, d.path());
    let rep = run_batch(
        &[item(1, "b")],
        &[heic],
        &render,
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(matches!(rep.failed[0].error, ExportError::NoHeicEncoder));
    assert_eq!(hw.0.load(Ordering::SeqCst), 1);
    // No hook: mozjpeg writes a real JPEG.
    let rep = run_batch(
        &[item(2, "c")],
        &[jpg],
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty());
    assert_eq!(
        &fs::read(d.path().join("c.jpg")).unwrap()[..2],
        &[0xFF, 0xD8]
    );
}

#[test]
fn oversize_output_is_rejected_per_recipe() {
    let d = tempfile::tempdir().unwrap();
    let mut r = ExportRecipe::new("r", ExportFormat::Jpeg { quality: 80 }, d.path());
    r.sizing = Sizing::LongEdge(65_000);
    r.allow_upscale = true;
    let render = |_: &ExportItem, _| Ok(gradient(2, 1));
    // 65000×32500 fits the edge limit but would need ~12 GB: rejected, not attempted.
    let rep = run_batch(
        &[item(0, "a")],
        &[r],
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(
        matches!(rep.failed[0].error, ExportError::TooLarge(_)),
        "{:?}",
        rep.failed
    );
    let mut r = ExportRecipe::new("r", ExportFormat::Jpeg { quality: 80 }, d.path());
    r.sizing = Sizing::Percent(4_000_000.0);
    r.allow_upscale = true;
    let rep = run_batch(
        &[item(0, "a")],
        &[r],
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(matches!(rep.failed[0].error, ExportError::TooLarge(_)));
}

struct PanickingHeic;
impl HeicEncoder for PanickingHeic {
    fn encode(
        &self,
        _: &Image16,
        _: u8,
        _: &[u8],
        _: Option<&[u8]>,
        _: &[String],
        _: &ds_core::IptcMeta,
        _: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        panic!("ImageIO blew up")
    }
}

/// A panicking renderer, HEIC encoder or progress callback fails that
/// item only; the rest of the batch completes and the report is returned.
#[test]
fn panicking_callbacks_fail_one_item_not_the_batch() {
    let d = tempfile::tempdir().unwrap();
    let jpg = ExportRecipe::new("j", ExportFormat::Jpeg { quality: 80 }, d.path().join("j"));
    let render = |it: &ExportItem, _| {
        assert!(it.id != 2, "renderer bug on item 2");
        Ok(gradient(8, 8))
    };
    let progress_calls = AtomicUsize::new(0);
    let progress = |_: &ds_export::ExportProgress| {
        progress_calls.fetch_add(1, Ordering::SeqCst);
        panic!("UI callback bug");
    };
    let items = [item(1, "a"), item(2, "b"), item(3, "c")];
    let rep = run_batch(
        &items,
        std::slice::from_ref(&jpg),
        &render,
        BatchOptions::default(),
        &progress,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.written.len(), 2, "{:?}", rep.failed);
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(rep.failed[0].item, Some(2));
    assert!(
        rep.failed[0]
            .error
            .to_string()
            .contains("renderer panicked"),
        "{}",
        rep.failed[0].error
    );
    assert_eq!(progress_calls.load(Ordering::SeqCst), 3);
    assert_eq!(files(&d.path().join("j")), ["a.jpg", "c.jpg"]);

    let heic = ExportRecipe::new("h", ExportFormat::Heic { quality: 80 }, d.path().join("h"));
    let opts = BatchOptions {
        max_in_flight: 1,
        heic: Some(&PanickingHeic),
        jpeg: None,
        upscaler: None,
        text: None,
        cancel: None,
    };
    let rep = run_batch(
        &[item(1, "a")],
        &[heic, jpg],
        &render,
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.written.len(), 1, "the JPEG recipe still ran");
    assert!(
        rep.failed[0]
            .error
            .to_string()
            .contains("HEIC encoder panicked")
    );
    assert!(files(&d.path().join("h")).is_empty(), "no temp left behind");
}

/// A batch onto a real exFAT volume (no `RENAME_EXCL`, no hard
/// links) writes every output, resolves collisions, and flushes the device.
#[cfg(target_os = "macos")]
#[test]
fn batch_exports_to_exfat_volume() {
    use ds_fsutil::testvol::{DiskImage, Fs};
    let t = tempfile::tempdir().unwrap();
    let img = DiskImage::create(t.path(), Fs::ExFat, 64, "EXPBATCH").unwrap();
    let out = img.mount_point().join("Output");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("a.jpg"), b"existing").unwrap();
    let mut r = ExportRecipe::new("j", ExportFormat::Jpeg { quality: 90 }, &out);
    r.file_name = Template::parse("{name}").unwrap();
    let render = |_: &ExportItem, _| Ok(gradient(16, 12));
    let rep = run_batch(
        &[item(1, "a"), item(2, "A"), item(3, &"x".repeat(240))],
        &[r],
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let mut names = files(&out);
    names.retain(|n| !n.starts_with("._"));
    assert_eq!(names.len(), 4, "{names:?}");
    // a and A collide (case-insensitive exFAT); workers race for -1/-2.
    let mut folded: Vec<String> = names[..3].iter().map(|n| n.to_lowercase()).collect();
    folded.sort();
    assert_eq!(folded, ["a-1.jpg", "a-2.jpg", "a.jpg"], "{names:?}");
    assert_eq!(fs::read(out.join("a.jpg")).unwrap(), b"existing");
}

/// A renderer that sizes itself (the develop pipeline) is asked per recipe and its
/// image is written as is (no CPU resize or sharpening); recipes it declines fall back
/// to one full-size render per output space.
#[test]
fn renderers_that_size_themselves_get_each_recipe() {
    struct SelfSizing {
        full: AtomicUsize,
        sized: AtomicUsize,
    }
    impl ds_export::Renderer for SelfSizing {
        fn render(&self, _: &ExportItem, _: OutputSpace) -> Result<Rendered, String> {
            self.full.fetch_add(1, Ordering::SeqCst);
            Ok(gradient(64, 32))
        }
        fn render_for(
            &self,
            _: &ExportItem,
            recipe: &ExportRecipe,
        ) -> Option<Result<Rendered, String>> {
            match recipe.sizing {
                // Declined: the batch renders full size and resizes on the CPU.
                Sizing::Percent(_) => None,
                Sizing::LongEdge(999) => Some(Err("gpu lost".into())),
                _ => {
                    self.sized.fetch_add(1, Ordering::SeqCst);
                    // Deliberately not the size the CPU path would pick: written as is.
                    Some(Ok(gradient(10, 7)))
                }
            }
        }
    }
    let d = tempfile::tempdir().unwrap();
    let recipe = |name: &str, sizing| {
        let mut r = ExportRecipe::new(name, ExportFormat::Png8, d.path());
        r.file_name = Template::parse(&format!("{{name}}_{name}")).unwrap();
        r.sizing = sizing;
        r
    };
    let mut a = recipe("a", Sizing::LongEdge(20));
    a.sharpen = 1.0;
    let r = SelfSizing {
        full: AtomicUsize::new(0),
        sized: AtomicUsize::new(0),
    };
    let rep = run_batch(
        &[item(1, "x")],
        &[
            a,
            recipe("b", Sizing::Percent(50.0)),
            recipe("c", Sizing::Percent(25.0)),
        ],
        &r,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(r.sized.load(Ordering::SeqCst), 1);
    assert_eq!(
        r.full.load(Ordering::SeqCst),
        1,
        "one full render for both declined recipes"
    );
    let size_of = |n: &str| {
        rep.written
            .iter()
            .find(|w| w.path.file_name().unwrap() == n)
            .unwrap()
            .size
    };
    assert_eq!(
        size_of("x_a.png"),
        Size::new(10, 7),
        "the renderer's size, as is"
    );
    assert_eq!(size_of("x_b.png"), Size::new(32, 16));
    assert_eq!(size_of("x_c.png"), Size::new(16, 8));
    // A sized render that fails is that recipe's failure.
    let rep = run_batch(
        &[item(2, "y")],
        &[recipe("bad", Sizing::LongEdge(999))],
        &r,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(rep.failed[0].recipe, Some(0));
    assert!(matches!(&rep.failed[0].error, ExportError::Render(m) if m == "gpu lost"));
}

/// Renders 8-bit RGBA at the recipe's size (a GPU-packed develop); item 9's buffer
/// is short.
struct Rgba8Sizing;

impl Rgba8Sizing {
    fn pixels(size: Size) -> Vec<u8> {
        (0..size.pixels() as usize * 4)
            .map(|i| (i * 37 % 251) as u8)
            .collect()
    }
}

impl Renderer for Rgba8Sizing {
    fn render(&self, _: &ExportItem, _: OutputSpace) -> Result<Rendered, String> {
        Err("sized renders only".into())
    }

    fn render_for(
        &self,
        it: &ExportItem,
        recipe: &ExportRecipe,
    ) -> Option<Result<Rendered, String>> {
        let size = recipe.target_size(Size::new(24, 16)).unwrap();
        let mut data = Self::pixels(size);
        if it.id == 9 {
            data.pop();
        }
        Some(Ok(Rendered::Rgba8 { size, data }))
    }
}

/// An 8-bit render is encoded as is by PNG8 and mozjpeg JPEG, and through a 16-bit
/// image (v x 257) for TIFF16 and the hardware JPEG hook; a short buffer fails.
#[test]
fn eight_bit_renders_encode_directly_where_they_can() {
    let d = tempfile::tempdir().unwrap();
    let mut png8 = ExportRecipe::new("p", ExportFormat::Png8, d.path());
    png8.file_name = Template::parse("{name}_p").unwrap();
    let mut jpg = ExportRecipe::new("j", ExportFormat::Jpeg { quality: 90 }, d.path());
    jpg.sizing = Sizing::LongEdge(12);
    let tif = ExportRecipe::new("t", ExportFormat::Tiff16, d.path());
    let recipes = [png8.clone(), jpg.clone(), tif];
    let rep = run_batch(
        &[item(1, "a")],
        &recipes,
        &Rgba8Sizing,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(ds_export::encodes_8_bit(&png8, BatchOptions::default()));
    assert!(ds_export::encodes_8_bit(&jpg, BatchOptions::default()));
    let rgb: Vec<u8> = Rgba8Sizing::pixels(Size::new(24, 16))
        .chunks(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    // PNG8: the render's bytes.
    let mut r = png::Decoder::new(Cursor::new(fs::read(d.path().join("a_p.png")).unwrap()))
        .read_info()
        .unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    assert_eq!(buf, rgb);
    // JPEG at its size; TIFF16 at 16 bits.
    let jpeg = fs::read(d.path().join("a.jpg")).unwrap();
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    let mut t = tiff::decoder::Decoder::new(Cursor::new(fs::read(d.path().join("a.tif")).unwrap()))
        .unwrap();
    match t.read_image().unwrap() {
        tiff::decoder::DecodingResult::U16(v) => {
            assert!(v.iter().zip(&rgb).all(|(&a, &b)| a == u16::from(b) * 257));
        }
        _ => panic!("expected u16"),
    }
    // The hardware JPEG hook takes 16-bit input.
    let hw = Heic(AtomicUsize::new(0));
    let opts = BatchOptions {
        max_in_flight: 1,
        heic: None,
        jpeg: Some(&hw),
        upscaler: None,
        text: None,
        cancel: None,
    };
    assert!(!ds_export::encodes_8_bit(&jpg, opts));
    let rep = run_batch(
        &[item(2, "b")],
        std::slice::from_ref(&jpg),
        &Rgba8Sizing,
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(
        fs::read(d.path().join("b.jpg")).unwrap(),
        vec![1; 12 * 8 * 3]
    );
    // A short 8-bit buffer fails that output, for both routes.
    let rep = run_batch(
        &[item(9, "c")],
        &[png8, recipes[2].clone()],
        &Rgba8Sizing,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.failed.len(), 2);
    assert!(
        rep.failed
            .iter()
            .all(|f| matches!(f.error, ExportError::InvalidImage(_)))
    );
}

struct FullDisk(AtomicUsize);
impl HeicEncoder for FullDisk {
    fn encode(
        &self,
        _: &Image16,
        _: u8,
        _: &[u8],
        _: Option<&[u8]>,
        _: &[String],
        _: &ds_core::IptcMeta,
        _: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("write failed: No space left on device (os error 28)".into())
    }
}

/// A full destination stops the batch at the first disk-full failure instead of
/// failing every later file.
#[test]
fn a_full_disk_stops_the_batch() {
    let d = tempfile::tempdir().unwrap();
    let r = ExportRecipe::new("h", ExportFormat::Heic { quality: 80 }, d.path());
    let render = |_: &ExportItem, _| Ok(gradient(4, 4));
    let full = FullDisk(AtomicUsize::new(0));
    let opts = BatchOptions {
        max_in_flight: 1,
        heic: Some(&full),
        jpeg: None,
        upscaler: None,
        text: None,
        cancel: None,
    };
    let items: Vec<_> = (0..6).map(|i| item(i, &format!("f{i}"))).collect();
    let rep = run_batch(
        &items,
        std::slice::from_ref(&r),
        &render,
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.disk_full && !rep.cancelled);
    assert_eq!(
        full.0.load(Ordering::SeqCst),
        1,
        "no file after the first full write"
    );
    assert_eq!(rep.failed.len(), 1);
    assert!(rep.failed[0].error.is_disk_full());
    assert_eq!(files(d.path()), [] as [std::string::String; 0]);
    assert!(!ExportError::Encode("bad quality".into()).is_disk_full());
    assert!(
        ExportError::Io {
            path: d.path().into(),
            source: std::io::Error::from_raw_os_error(28),
        }
        .is_disk_full()
    );
}

/// AI Upscale double: nearest neighbour 2× (or no model, or a failure).
struct Nearest {
    calls: AtomicUsize,
    outcome: fn(&Image16) -> Result<Option<Image16>, String>,
}

impl ds_export::Upscaler for Nearest {
    fn upscale(&self, img: &Image16) -> Result<Option<Image16>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.outcome)(img)
    }
}

fn nearest_2x(img: &Image16) -> Image16 {
    let (w, h) = (img.size.width as usize, img.size.height as usize);
    let data = (0..4 * w * h)
        .flat_map(|i| {
            let (x, y) = (i % (2 * w), i / (2 * w));
            let p = ((y / 2) * w + x / 2) * 3;
            [img.data[p], img.data[p + 1], img.data[p + 2]]
        })
        .collect();
    Image16::new(Size::new(2 * w as u32, 2 * h as u32), data).unwrap()
}

#[test]
fn ai_upscale_enlarges_through_the_model_only_when_the_output_is_larger() {
    let d = tempfile::tempdir().unwrap();
    let recipe = |name: &str, sizing| {
        let mut r = ExportRecipe::new(name, ExportFormat::Tiff16, d.path().join(name));
        r.sizing = sizing;
        r.ai_upscale = true;
        r
    };
    // Original size with AI Upscale is twice the render; 150% resamples the 2× image
    // down; a smaller size never runs the model; a recipe without it never enlarges.
    let double = recipe("double", Sizing::Percent(200.0));
    let one_half = recipe("x15", Sizing::Percent(150.0));
    let small = recipe("small", Sizing::LongEdge(20));
    let mut plain = recipe("plain", Sizing::Percent(200.0));
    plain.ai_upscale = false;
    let up = Nearest {
        calls: AtomicUsize::new(0),
        outcome: |img| Ok(Some(nearest_2x(img))),
    };
    let opts = BatchOptions {
        upscaler: Some(&up),
        ..BatchOptions::default()
    };
    let rep = run_batch(
        &[item(1, "a")],
        &[double, one_half, small, plain],
        &|_: &ExportItem, _| Ok(gradient(40, 30)),
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let size = |r: usize| rep.written.iter().find(|w| w.recipe == r).unwrap().size;
    assert_eq!(size(0), Size::new(80, 60));
    assert_eq!(size(1), Size::new(60, 45));
    assert_eq!(size(2), Size::new(20, 15));
    assert_eq!(
        size(3),
        Size::new(40, 30),
        "without AI Upscale: never enlarged"
    );
    assert_eq!(up.calls.load(Ordering::SeqCst), 2);
    // The 2× output is the model's, exactly (no resampling at 200%).
    let tif = fs::read(&rep.written.iter().find(|w| w.recipe == 0).unwrap().path).unwrap();
    let mut dec = tiff::decoder::Decoder::new(Cursor::new(tif)).unwrap();
    let tiff::decoder::DecodingResult::U16(px) = dec.read_image().unwrap() else {
        panic!("16-bit")
    };
    let Rendered::Rgb16 { data: src, .. } = gradient(40, 30) else {
        unreachable!()
    };
    assert_eq!(&px[..6], &[src[0], src[1], src[2], src[0], src[1], src[2]]);
    assert_eq!(&px[80 * 3..80 * 3 + 3], &src[..3], "row 1 repeats row 0");
}

#[test]
fn ai_upscale_without_a_model_resamples_and_a_failure_fails_the_output() {
    let d = tempfile::tempdir().unwrap();
    let mut r = ExportRecipe::new("r", ExportFormat::Png8, d.path());
    r.sizing = Sizing::Original;
    r.ai_upscale = true;
    let none = Nearest {
        calls: AtomicUsize::new(0),
        outcome: |_| Ok(None),
    };
    let run = |r: &ExportRecipe, up: Option<&dyn ds_export::Upscaler>| {
        run_batch(
            &[item(1, "a")],
            std::slice::from_ref(r),
            &|_: &ExportItem, _| Ok(gradient(40, 30)),
            BatchOptions {
                upscaler: up,
                ..BatchOptions::default()
            },
            &no_progress,
            &AtomicBool::new(false),
        )
    };
    // "Original" is the render's size: nothing to enlarge, the model isn't asked.
    let rep = run(&r, Some(&none));
    assert_eq!(rep.written[0].size, Size::new(40, 30));
    assert_eq!(none.calls.load(Ordering::SeqCst), 0);
    r.sizing = Sizing::LongEdge(70);
    let rep = run(&r, Some(&none));
    assert_eq!(
        rep.written[0].size,
        Size::new(70, 53),
        "resampled without a model"
    );
    assert_eq!(none.calls.load(Ordering::SeqCst), 1);
    let rep = run(&r, None);
    assert_eq!(rep.written[0].size, Size::new(70, 53), "no upscaler at all");
    let failing = Nearest {
        calls: AtomicUsize::new(0),
        outcome: |_| Err("model failed".into()),
    };
    let rep = run(&r, Some(&failing));
    assert_eq!(rep.written, [] as [ds_export::ExportedFile; 0]);
    assert!(matches!(&rep.failed[0].error, ExportError::Render(m) if m == "model failed"));
    let panicking = Nearest {
        calls: AtomicUsize::new(0),
        outcome: |_| panic!("boom"),
    };
    let rep = run(&r, Some(&panicking));
    assert!(matches!(&rep.failed[0].error, ExportError::Render(m) if m.contains("panicked")));
}

/// A PNG file's samples (8- or 16-bit, as stored) and width.
fn png_samples(path: &Path) -> (u32, Vec<u16>) {
    let mut r = png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    let buf = &buf[..info.buffer_size()];
    let px = if info.bit_depth == png::BitDepth::Sixteen {
        buf.as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_be_bytes(*b))
            .collect()
    } else {
        buf.iter().map(|&v| u16::from(v)).collect()
    };
    (info.width, px)
}

/// The watermark is drawn last, at output size, on every path (CPU resize and
/// sharpen, an 8-bit sized render encoded as is, a sized render through 16 bits):
/// its pixels are exactly the logo's, unsharpened; the rest equals the same
/// recipe without a watermark.
#[test]
fn watermark_is_drawn_after_resize_and_sharpen() {
    use std::sync::Arc;

    use ds_export::{Anchor, Logo, Mark, Watermark};
    let d = tempfile::tempdir().unwrap();
    // An opaque mid-grey 4×4 logo.
    let logo_path = d.path().join("logo.png");
    {
        let mut enc = png::Encoder::new(fs::File::create(&logo_path).unwrap(), 4, 4);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&[128; 48]).unwrap();
        wr.finish().unwrap();
    }
    let wm = Watermark {
        mark: Mark::Logo {
            logo: Arc::new(Logo::load(&logo_path).unwrap()),
            scale: 0.25,
        },
        anchor: Anchor::TopLeft,
        inset: 0.0,
        opacity: 1.0,
    };
    let recipe = |name: &str, format: ExportFormat, mark: bool| {
        let mut r = ExportRecipe::new(name, format, d.path().join("out"));
        r.file_name = Template::parse(&format!("{{name}}_{name}")).unwrap();
        r.sizing = Sizing::LongEdge(20);
        r.sharpen = 2.0;
        r.watermark = mark.then(|| wm.clone());
        r
    };
    let cpu = |_: &ExportItem, _: OutputSpace| -> Result<Rendered, String> { Ok(gradient(80, 60)) };
    let cases: [(&str, &dyn Renderer, ExportFormat, u16); 3] = [
        ("cpu", &cpu, ExportFormat::Png8, 128),
        ("rgba8", &Rgba8Sizing, ExportFormat::Png8, 128),
        ("rgb16", &Rgba8Sizing, ExportFormat::Png16, 128 * 257),
    ];
    for (name, renderer, format, grey) in cases {
        let plain_name = format!("{name}plain");
        let rep = run_batch(
            &[item(1, "a")],
            &[
                recipe(name, format, true),
                recipe(&plain_name, format, false),
            ],
            renderer,
            BatchOptions::default(),
            &no_progress,
            &AtomicBool::new(false),
        );
        assert!(rep.failed.is_empty(), "{name}: {:?}", rep.failed);
        let (w, marked) = png_samples(&d.path().join("out").join(format!("a_{name}.png")));
        let (_, plain) = png_samples(&d.path().join("out").join(format!("a_{plain_name}.png")));
        // A quarter of the width (20 px for the CPU's 80×60 → 20×15, 24×16 → 20×13).
        assert_eq!(w, 20, "{name}");
        for (i, (m, p)) in marked.chunks(3).zip(plain.chunks(3)).enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            if x < 5 && y < 5 {
                assert_eq!(m, [grey; 3], "{name} ({x},{y})");
            } else {
                assert_eq!(m, p, "{name} ({x},{y})");
            }
        }
    }
}

/// AI Upscale and a watermark together: the mark is drawn last, on the upscaled and
/// resized output, at its own size.
#[test]
fn the_watermark_is_drawn_after_the_ai_upscale() {
    use std::sync::Arc;

    use ds_export::{Anchor, Logo, Mark, Watermark};
    let d = tempfile::tempdir().unwrap();
    let logo_path = d.path().join("logo.png");
    {
        let mut enc = png::Encoder::new(fs::File::create(&logo_path).unwrap(), 4, 4);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&[128; 48]).unwrap();
        wr.finish().unwrap();
    }
    let mut r = ExportRecipe::new("big", ExportFormat::Png8, d.path().join("out"));
    r.sizing = Sizing::Percent(200.0);
    r.ai_upscale = true;
    r.watermark = Some(Watermark {
        mark: Mark::Logo {
            logo: Arc::new(Logo::load(&logo_path).unwrap()),
            scale: 0.25,
        },
        anchor: Anchor::TopLeft,
        inset: 0.0,
        opacity: 1.0,
    });
    let up = Nearest {
        calls: AtomicUsize::new(0),
        outcome: |img| Ok(Some(nearest_2x(img))),
    };
    let rep = run_batch(
        &[item(1, "a")],
        &[r],
        &|_: &ExportItem, _| Ok(gradient(40, 30)),
        BatchOptions {
            upscaler: Some(&up),
            ..BatchOptions::default()
        },
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(up.calls.load(Ordering::SeqCst), 1);
    let (w, px) = png_samples(&rep.written[0].path);
    assert_eq!(w, 80, "upscaled 2×");
    // A quarter of the 80 px output: a 20×20 grey logo in the corner, then the image.
    assert_eq!(&px[..3], &[128; 3]);
    assert_eq!(&px[(19 * 80 + 19) * 3..][..3], &[128; 3]);
    assert_ne!(&px[(19 * 80 + 20) * 3..][..3], &[128; 3]);
}

/// A text watermark that comes out blank for a photo (no copyright) is reported per
/// file, so the app can say so; one with text, and a logo-free recipe, are not.
#[test]
fn blank_watermark_text_is_reported_per_file() {
    use ds_export::{Anchor, Mark, Watermark};
    struct NoText;
    impl ds_export::TextRasterizer for NoText {
        fn rasterize(&self, _: &str, _: &str, _: f32) -> Result<ds_export::TextMask, String> {
            Ok(ds_export::TextMask {
                width: 1,
                height: 1,
                coverage: vec![255],
            })
        }
    }
    let d = tempfile::tempdir().unwrap();
    let mut r = ExportRecipe::new("r", ExportFormat::Png8, d.path());
    r.file_name = Template::parse("{name}_{seq}").unwrap();
    r.watermark = Some(Watermark {
        mark: Mark::Text {
            text: "{copyright}".into(),
            font: String::new(),
            size: 0.1,
            color: [255; 3],
            shadow: false,
        },
        anchor: Anchor::BottomRight,
        inset: 0.0,
        opacity: 1.0,
    });
    let plain = ExportRecipe::new("plain", ExportFormat::Png8, d.path().join("plain"));
    let mut with = item(1, "a");
    with.meta.copyright = Some("(c) S".into());
    let without = item(2, "b");
    let opts = BatchOptions {
        text: Some(&NoText),
        ..BatchOptions::default()
    };
    let render =
        |_: &ExportItem, _: OutputSpace| -> Result<Rendered, String> { Ok(gradient(8, 8)) };
    let rep = run_batch(
        &[with, without],
        &[r, plain],
        &render,
        opts,
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let mut skipped: Vec<_> = rep
        .written
        .iter()
        .map(|w| (w.item, w.recipe, w.watermark_skipped))
        .collect();
    skipped.sort_unstable();
    assert_eq!(
        skipped,
        [(1, 0, false), (1, 1, false), (2, 0, true), (2, 1, false)]
    );
}

/// A recipe with an output profile converts the render to it and embeds it, and
/// renders 16-bit even for an 8-bit format.
#[test]
fn output_profile_converts_and_embeds() {
    use ds_export::proof::{IccProfile, OutputProfile, ProofIntent, ProofSettings};
    let d = tempfile::tempdir().unwrap();
    let srgb = IccProfile::from_bytes(ds_export::icc::profile(OutputSpace::Srgb)).unwrap();
    let settings = ProofSettings {
        intent: ProofIntent::RelativeColorimetric,
        black_point_compensation: true,
    };
    let mut png = ExportRecipe::new("p", ExportFormat::Png8, d.path());
    png.space = OutputSpace::DisplayP3;
    png.output_profile = Some(OutputProfile::new(srgb.clone(), settings).unwrap());
    assert!(!ds_export::encodes_8_bit(&png, BatchOptions::default()));
    // A P3 render: saturated green, and a neutral.
    let render = |_: &ExportItem, space: OutputSpace| {
        assert_eq!(space, OutputSpace::DisplayP3);
        Ok(Rendered::Rgb16 {
            size: Size::new(2, 1),
            data: vec![0, 65_535, 0, 30_000, 30_000, 30_000],
        })
    };
    let rep = run_batch(
        &[item(1, "a")],
        std::slice::from_ref(&png),
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let mut r = png::Decoder::new(Cursor::new(fs::read(d.path().join("a.png")).unwrap()))
        .read_info()
        .unwrap();
    assert_eq!(r.info().icc_profile.as_deref(), Some(srgb.bytes()));
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    // P3 green clipped to sRGB green; the neutral stays neutral.
    assert!(buf[0] < 2 && buf[1] == 255 && buf[2] < 2, "{buf:?}");
    assert!(buf[3] == buf[4] && buf[4] == buf[5], "{buf:?}");
}

/// An output-profile recipe converts from the Rec.2020 render: a colour outside
/// Display P3 but inside the target (Adobe RGB's green) arrives unclipped.
#[test]
fn output_profile_keeps_colours_outside_p3() {
    use ds_export::proof::{IccProfile, OutputProfile, ProofIntent, ProofSettings};
    let d = tempfile::tempdir().unwrap();
    let adobe_bytes =
        ds_export::icc::build_profile("Adobe-like", [(0.64, 0.33), (0.21, 0.71), (0.15, 0.06)]);
    let adobe = IccProfile::from_bytes(&adobe_bytes).unwrap();
    // Adobe RGB's pure green, in the render's Rec.2020 encoding.
    let to_wide: lcms2::Transform<[u16; 3], [u16; 3]> = lcms2::Transform::new(
        &lcms2::Profile::new_icc(&adobe_bytes).unwrap(),
        lcms2::PixelFormat::RGB_16,
        &lcms2::Profile::new_icc(ds_export::icc::profile(OutputSpace::Rec2020)).unwrap(),
        lcms2::PixelFormat::RGB_16,
        lcms2::Intent::RelativeColorimetric,
    )
    .unwrap();
    let mut green = [[0u16; 3]];
    to_wide.transform_pixels(&[[0, 65_535, 0]], &mut green);
    let mut tif = ExportRecipe::new("t", ExportFormat::Png16, d.path());
    tif.space = OutputSpace::Rec2020;
    tif.output_profile = Some(
        OutputProfile::new(
            adobe,
            ProofSettings {
                intent: ProofIntent::RelativeColorimetric,
                black_point_compensation: false,
            },
        )
        .unwrap(),
    );
    let render = |_: &ExportItem, space: OutputSpace| {
        assert_eq!(space, OutputSpace::Rec2020);
        Ok(Rendered::Rgb16 {
            size: Size::new(1, 1),
            data: green[0].to_vec(),
        })
    };
    let rep = run_batch(
        &[item(1, "g")],
        std::slice::from_ref(&tif),
        &render,
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let mut r = png::Decoder::new(Cursor::new(fs::read(d.path().join("g.png")).unwrap()))
        .read_info()
        .unwrap();
    assert_eq!(r.info().icc_profile.as_deref(), Some(&adobe_bytes[..]));
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    let px: Vec<u16> = buf
        .chunks(2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .collect();
    // Adobe's own green, softly compressed into Adobe RGB as a plain render into it would
    // be (`ds_core::gamut`), not clipped to P3 first.
    let c = ds_core::gamut::compress([0.0, 1.0, 0.0]);
    let enc = |v: f32| -> u16 {
        let e = if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (e * 65_535.0).round() as u16
    };
    let want = c.map(enc);
    assert!(
        px.iter().zip(want).all(|(a, b)| a.abs_diff(b) < 800),
        "{px:?} vs {want:?}"
    );
}

/// An upscaler is app-side code: a result of the wrong size, or with fewer samples
/// than its size says, fails that output with a render error. It used to reach the
/// resampler, whose out-of-range read killed the whole batch.
#[test]
fn a_bad_upscaler_result_fails_the_output_not_the_batch() {
    let d = tempfile::tempdir().unwrap();
    let mut r = ExportRecipe::new("r", ExportFormat::Png8, d.path());
    r.sizing = Sizing::Percent(150.0);
    r.ai_upscale = true;
    let run = |outcome: fn(&Image16) -> Result<Option<Image16>, String>| {
        let up = Nearest {
            calls: AtomicUsize::new(0),
            outcome,
        };
        run_batch(
            &[item(1, "a"), item(2, "b")],
            std::slice::from_ref(&r),
            &|_: &ExportItem, _| Ok(gradient(40, 30)),
            BatchOptions {
                upscaler: Some(&up),
                ..BatchOptions::default()
            },
            &no_progress,
            &AtomicBool::new(false),
        )
    };
    let wrong_size: fn(&Image16) -> _ = |_| {
        Ok(Some(
            Image16::new(Size::new(10, 10), vec![0; 10 * 10 * 3]).unwrap(),
        ))
    };
    let short_buffer: fn(&Image16) -> _ = |_| {
        Ok(Some(Image16 {
            size: Size::new(80, 60),
            data: vec![0; 10],
        }))
    };
    for (name, outcome) in [("wrong size", wrong_size), ("short buffer", short_buffer)] {
        let rep = run(outcome);
        assert!(rep.written.is_empty(), "{name}");
        assert_eq!(
            rep.failed.len(),
            2,
            "{name}: both items fail, the batch ends"
        );
        assert!(
            rep.failed
                .iter()
                .all(|f| matches!(&f.error, ExportError::Render(m) if m.contains("upscaler"))),
            "{name}: {:?}",
            rep.failed
        );
    }
    assert_eq!(files(d.path()), Vec::<String>::new());
    let ok = run(|img| Ok(Some(nearest_2x(img))));
    assert!(
        ok.failed.is_empty() && ok.written.len() == 2,
        "{:?}",
        ok.failed
    );
}

/// The single-output entry points check the recipe themselves; only `run_batch` did.
#[test]
fn single_exports_validate_their_recipe() {
    let d = tempfile::tempdir().unwrap();
    let Rendered::Rgb16 { size, data } = gradient(16, 8) else {
        unreachable!()
    };
    let full = Image16::new(size, data).unwrap();
    let mut bad = ExportRecipe::new("bad", ExportFormat::Jpeg { quality: 0 }, d.path());
    let namer = ds_export::Namer::default();
    let it = item(1, "a");
    let e = ds_export::export_one(&it, &full, &bad, None, &namer).unwrap_err();
    assert!(matches!(e, ExportError::InvalidRecipe(_)), "{e}");
    let e =
        ds_export::export_prepared(&it, &full, &bad, BatchOptions::default(), &namer).unwrap_err();
    assert!(matches!(e, ExportError::InvalidRecipe(_)), "{e}");
    assert_eq!(files(d.path()), [] as [std::string::String; 0]);
    bad.format = ExportFormat::Jpeg { quality: 80 };
    let w = ds_export::export_one(&it, &full, &bad, None, &namer).unwrap();
    assert_eq!(
        (w.size, w.bytes, w.notes.len()),
        (size, fs::metadata(&w.path).unwrap().len(), 0)
    );
}

/// Metadata that does not fit a JPEG is trimmed, the export still succeeds, and the
/// report says so.
#[test]
fn trimmed_metadata_is_reported_per_file() {
    let d = tempfile::tempdir().unwrap();
    let mut crowded = item(1, "crowded");
    crowded.meta.keywords = (0..3000)
        .map(|i| format!("{i:04}-{}", "k".repeat(40)))
        .collect();
    let rep = run_batch(
        &[crowded, item(2, "plain")],
        &[ExportRecipe::new(
            "j",
            ExportFormat::Jpeg { quality: 90 },
            d.path(),
        )],
        &|_: &ExportItem, _| Ok(gradient(16, 8)),
        BatchOptions::default(),
        &no_progress,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let by = |id| rep.written.iter().find(|w| w.item == id).unwrap();
    assert!(
        by(1).notes.iter().any(|n| n.contains("keywords left out")),
        "{:?}",
        by(1).notes
    );
    assert_eq!(by(2).notes, [] as [std::string::String; 0]);
    for w in &rep.written {
        assert_eq!(w.bytes, fs::metadata(&w.path).unwrap().len());
        mozjpeg::Decompress::new_mem(&fs::read(&w.path).unwrap()).expect("decodable");
    }
}
