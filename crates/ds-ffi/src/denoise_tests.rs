#![allow(clippy::float_cmp)] // exact values: products of exact defaults

use std::sync::atomic::{AtomicU64, Ordering};

use ds_core::{CfaPattern, Orientation, RawImage};
use ds_export::{ExportItem, OutputSpace, Rendered, Renderer};
use ds_pipeline::{CaptureInfo, OutputFormat, Quality, RenderRequest};
use futures_executor::block_on;

use super::*;

/// The store is process-wide: tests that touch it run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());
static NEXT_KEY: AtomicU64 = AtomicU64::new(0x00de_0000);

fn serial() -> std::sync::MutexGuard<'static, ()> {
    lock(&SERIAL)
}

/// Model `model`, results in memory only (or under `dir`), nothing remembered.
fn reset(model: Option<&str>, dir: Option<&Path>) {
    set_denoise_model(None).unwrap();
    set_denoise_model(model.map(str::to_owned)).unwrap();
    set_denoise_provider(None);
    set_denoise_cache(dir.map(|d| d.to_string_lossy().into_owned()), 1 << 30).unwrap();
}

/// A noisy synthetic Bayer frame, `seed` making each one different content.
fn photo(w: u32, h: u32, seed: u32) -> Loaded {
    let data = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            let n = (i ^ seed.wrapping_mul(0x9E37_79B9)).wrapping_mul(2_654_435_761) >> 22; // 0..1024
            (1500 + x * 4 + y * 3 + n) as u16
        })
        .collect();
    Loaded {
        key: ImageKey(NEXT_KEY.fetch_add(1, Ordering::Relaxed)),
        raw: Arc::new(RawImage {
            size: Size::new(w, h),
            cfa: CfaPattern::Rggb,
            data,
            black_level: [512.0; 4],
            white_level: 16383.0,
            wb_as_shot: [2.0, 1.0, 1.5],
            wb_daylight: [2.0, 1.0, 1.5],
            cam_to_xyz: [
                [0.4124, 0.3576, 0.1805],
                [0.2126, 0.7152, 0.0722],
                [0.0193, 0.1192, 0.9505],
            ],
            orientation: Orientation::Normal,
            default_crop: None,
        }),
        capture: CaptureInfo::default(),
    }
}

fn ex(path: &str, p: &Loaded, adj: &Adjustments) -> Result<Resolved, String> {
    for_export(Path::new(path), p, adj, &CancelToken::new())
}

/// A stand-in model: a 3×3 box blur of each plane (it removes noise).
fn blur(t: usize, buf: &mut [u16]) {
    for plane in buf.chunks_mut(t * t) {
        let src: Vec<f32> = plane
            .iter()
            .map(|v| half::f16::from_bits(*v).to_f32())
            .collect();
        for y in 0..t {
            for x in 0..t {
                let mut sum = 0.0;
                let mut n = 0.0;
                for (dx, dy) in [
                    (-1i32, -1i32),
                    (0, -1),
                    (1, -1),
                    (-1, 0),
                    (0, 0),
                    (1, 0),
                    (-1, 1),
                    (0, 1),
                    (1, 1),
                ] {
                    let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                    if sx >= 0 && sy >= 0 && (sx as usize) < t && (sy as usize) < t {
                        sum += src[sy as usize * t + sx as usize];
                        n += 1.0;
                    }
                }
                plane[y * t + x] = half::f16::from_f32(sum / n).to_bits();
            }
        }
    }
}

/// Run every tile of `task` through `model` via the FFI calls (caller-memory pointers).
fn run_tiles(task: &DenoiseTask, model: impl Fn(usize, &mut [u16])) {
    let n = task.tile_count().unwrap();
    let t = task.tile_size().unwrap() as usize;
    let mut buf = vec![0u16; 3 * t * t];
    for i in 0..n {
        task.read_tile(i, buf.as_mut_ptr() as u64, buf.len() as u64)
            .unwrap();
        model(t, &mut buf);
        task.write_tile(
            i,
            buf.as_ptr() as u64,
            buf.len() as u64,
            t as u64,
            (t * t) as u64,
        )
        .unwrap();
    }
}

/// Denoise `loaded` with the blur model and store the result.
fn denoise_with_blur(loaded: &Loaded) {
    let task = Arc::new(start(loaded, false).unwrap());
    run_tiles(&task, blur);
    block_on(task.finish(CancelToken::new())).unwrap();
}

fn ai(strength: f32) -> Adjustments {
    let mut a = Adjustments::default();
    a.noise.ai_denoise = true;
    a.noise.ai_strength = strength;
    a
}

fn render(loaded: &Loaded, adj: &Adjustments) -> Vec<u8> {
    let req = RenderRequest {
        quality: Quality::Full,
        ..RenderRequest::new(Size::new(4096, 4096))
    };
    let size = ds_pipeline::output_size(&loaded.raw, adj, &req).unwrap();
    let mut out = vec![0u8; size.pixels() as usize * 4];
    develop()
        .render(loaded, adj, &req, OutputFormat::Rgba8, &mut out)
        .unwrap();
    out
}

/// Mean absolute difference of horizontally adjacent pixels (a noise measure).
fn roughness(rgba: &[u8], w: usize) -> f64 {
    let mut sum = 0.0;
    let mut n = 0.0;
    for row in rgba.chunks(w * 4) {
        for x in 0..w - 1 {
            sum += f64::from(row[x * 4 + 1].abs_diff(row[(x + 1) * 4 + 1]));
            n += 1.0;
        }
    }
    sum / n
}

#[test]
fn without_a_model_or_a_result_the_original_is_rendered() {
    let _s = serial();
    let p = photo(64, 48, 1);
    reset(None, None);
    assert!(effective(&p, &ai(50.0), Priority::Interactive, false).is_none());
    assert!(
        matches!(start(&p, false), Err(DsError::InvalidArgument { message }) if message.contains("no denoise model"))
    );
    assert!(!ready(&p));
    // No model: the app decides (still checking it at launch: wait; none in this build:
    // export the original). Without the app, the photo fails rather than export plain.
    let err = ex("/x/S.ARW", &p, &ai(50.0)).unwrap_err();
    assert!(err.contains("isn't available"), "{err}");
    set_denoise_provider(Some(Arc::new(NoModel)));
    assert!(
        matches!(ex("/x/S.ARW", &p, &ai(50.0)), Ok(None)),
        "no model in this build: export renders the original"
    );
    set_denoise_provider(Some(Arc::new(Liar)));
    let err = ex("/x/S.ARW", &p, &ai(50.0)).unwrap_err();
    assert!(err.contains("model isn't loaded"), "{err}");
    set_denoise_provider(None);
    assert!(
        matches!(ex("/x/S.ARW", &p, &Adjustments::default()), Ok(None)),
        "AI Denoise off: nothing needed"
    );
    reset(Some("model-a"), None);
    assert!(
        effective(&p, &ai(50.0), Priority::Interactive, false).is_none(),
        "not denoised yet"
    );
    assert!(
        ex("/x/S.ARW", &p, &ai(50.0))
            .unwrap_err()
            .contains("isn't available"),
        "no provider: the photo fails"
    );
    // An empty attempt isn't remembered: no blend cell is left behind.
    assert!(lock(store()).blends.is_empty());
}

#[test]
fn a_denoised_photo_renders_smoother_and_strength_zero_is_bit_identical() {
    let _s = serial();
    reset(Some("model-a"), None);
    let p = photo(160, 120, 2);
    denoise_with_blur(&p);
    assert!(ready(&p));
    assert!(ex("/x/S.ARW", &p, &ai(50.0)).unwrap().is_some());

    let original = render(&p, &Adjustments::default());
    // Strength 0 (AI Denoise on) renders the original itself: bit-identical.
    assert_eq!(render(&p, &ai(0.0)), original);
    let mut off = ai(100.0);
    off.noise.ai_denoise = false;
    assert_eq!(render(&p, &off), original, "off whatever the strength");

    let full = render(&p, &ai(100.0));
    let half = render(&p, &ai(50.0));
    let (r0, r50, r100) = (
        roughness(&original, 160),
        roughness(&half, 160),
        roughness(&full, 160),
    );
    assert!(
        r100 < r50 && r50 < r0,
        "noise falls with strength: {r0} {r50} {r100}"
    );

    // Grid thumbnails (bulk) show the original.
    assert!(effective(&p, &ai(100.0), Priority::Bulk, false).is_none());
    assert!(wanted(&ai(1.0)) && !wanted(&ai(0.0)));
    // The luminance slider is taken over in proportion to the strength.
    let (_, a) = effective(&p, &ai(50.0), Priority::Interactive, false).unwrap();
    assert_eq!(
        a.noise.luminance,
        Adjustments::default().noise.luminance * 0.5
    );
    let (l100, a100) = effective(&p, &ai(100.0), Priority::Interactive, false).unwrap();
    assert_eq!(a100.noise.luminance, 0.0);
    assert_eq!(l100.raw.cfa, CfaPattern::LinearRgb);
    assert_ne!(l100.key, p.key, "its own renderer caches");
}

#[test]
fn blends_are_cached_per_strength_and_bounded() {
    let _s = serial();
    reset(Some("model-a"), None);
    let p = photo(96, 64, 3);
    denoise_with_blur(&p);
    let k = |s: f32| {
        effective(&p, &ai(s), Priority::Interactive, false)
            .unwrap()
            .0
    };
    let a = k(40.0);
    let again = k(40.0);
    assert_eq!(a.key, again.key);
    assert!(
        Arc::ptr_eq(&a.raw, &again.raw),
        "the cached blend, not a new one"
    );
    let b = k(41.0);
    assert_ne!(a.key, b.key);
    k(42.0);
    k(43.0);
    let s = lock(store());
    assert_eq!(s.blends.len(), MEMORY_BLENDS);
    assert!(
        s.original.is_some(),
        "the original's linear image is kept for re-blends"
    );
}

#[test]
fn results_persist_on_disk_and_survive_a_restart() {
    let _s = serial();
    let t = tempfile::tempdir().unwrap();
    reset(Some("model-a"), Some(t.path()));
    let p = photo(96, 64, 4);
    denoise_with_blur(&p);
    let files: Vec<_> = std::fs::read_dir(t.path())
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(files.len(), 1, "one result file, no temp files left");
    let expected = render(&p, &ai(100.0));
    assert!(
        warm(&p, &ai(100.0)),
        "a neighbour's result is read into memory"
    );
    assert!(!warm(&p, &Adjustments::default()));
    // A "restart": memory forgotten, the disk cache still there.
    reset(Some("model-a"), Some(t.path()));
    assert!(lock(store()).results.is_empty());
    assert!(ready(&p));
    assert_eq!(
        render(&p, &ai(100.0)),
        expected,
        "the same result from disk"
    );
    // Another model never uses it (cache invalidation by key)...
    reset(Some("model-b"), Some(t.path()));
    assert!(!ready(&p));
    assert!(effective(&p, &ai(100.0), Priority::Interactive, false).is_none());
    // ...nor does another pipeline denoise version.
    let content = content_hash(&p);
    assert_ne!(
        key_of(&content, "model-a", 1, 7),
        key_of(&content, "model-a", 2, 7)
    );
    assert_ne!(
        key_of(&content, "model-a", 1, 7),
        key_of(&content, "model-b", 1, 7)
    );
    assert_eq!(
        key_of(&content, "model-a", 1, 7),
        key_of(&content, "model-a", 1, 7)
    );
    assert_ne!(
        key_of(&content, "model-a", 1, 7),
        key_of(&content, "model-a", 1, 8),
        "a new pipeline version (BASE demosaic) misses"
    );
}

/// A way to damage a result file's bytes.
type Damage = fn(&mut Vec<u8>);

#[test]
fn damaged_cache_files_are_dropped_and_unreadable_ones_kept() {
    use std::os::unix::fs::PermissionsExt;
    let _s = serial();
    let t = tempfile::tempdir().unwrap();
    let p = photo(64, 48, 5);
    let damages: [(&str, Damage); 5] = [
        ("truncated", |b| b.truncate(b.len() / 2)),
        ("bad magic", |b| b[..4].copy_from_slice(b"XXXX")),
        ("another size", |b| {
            b[12..16].copy_from_slice(&49u32.to_le_bytes());
        }),
        ("trailing bytes", |b| b.push(0)),
        ("a flipped sample", |b| {
            let n = b.len();
            b[n - 7] ^= 0x40;
        }),
    ];
    for (what, damage) in damages {
        reset(Some("model-a"), Some(t.path()));
        denoise_with_blur(&p);
        let file = std::fs::read_dir(t.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut bytes = std::fs::read(&file).unwrap();
        damage(&mut bytes);
        std::fs::write(&file, &bytes).unwrap();
        reset(Some("model-a"), Some(t.path())); // memory forgotten
        assert!(
            effective(&p, &ai(100.0), Priority::Interactive, false).is_none(),
            "{what}: not used"
        );
        assert!(!file.exists(), "{what}: the damaged file is removed");
        assert!(!ready(&p), "{what}");
    }
    // A file that can't be read right now (permissions) is kept for later.
    reset(Some("model-a"), Some(t.path()));
    denoise_with_blur(&p);
    let file = std::fs::read_dir(t.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    reset(Some("model-a"), Some(t.path()));
    assert!(effective(&p, &ai(100.0), Priority::Interactive, false).is_none());
    assert!(file.exists(), "kept: not damaged, just unreadable now");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(effective(&p, &ai(100.0), Priority::Interactive, false).is_some());
    // denoise_ready reads only the header: a valid header and length count.
    assert!(ready(&p));
}

#[test]
fn stale_temporary_files_are_trimmed() {
    let _s = serial();
    let t = tempfile::tempdir().unwrap();
    reset(Some("model-a"), Some(t.path()));
    let old = t.path().join(".ds-1-1.tmp");
    let fresh = t.path().join(".ds-1-2.tmp");
    let other = t.path().join("notes.tmp");
    for f in [&old, &fresh, &other] {
        std::fs::write(f, b"x").unwrap();
    }
    let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    for f in [&old, &other] {
        File::options()
            .write(true)
            .open(f)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
    }
    denoise_with_blur(&photo(64, 48, 6));
    assert!(!old.exists(), "a crash's leftover goes");
    assert!(fresh.exists(), "a write in progress stays");
    assert!(other.exists(), "not ours");
}

/// The app's provider, recording calls; it denoises with the blur model.
struct Provider {
    photo: Loaded,
    calls: Mutex<Vec<String>>,
    fail: bool,
}

impl DenoiseProvider for Provider {
    fn ensure(&self, path: String, cancel: Arc<CancelToken>) -> Result<bool, DsError> {
        lock(&self.calls).push(path);
        if self.fail || cancel.is_cancelled() {
            return Err(DsError::Cancelled);
        }
        denoise_with_blur(&self.photo);
        Ok(true)
    }
}

/// Says it denoised, but didn't.
struct Liar;

impl DenoiseProvider for Liar {
    fn ensure(&self, _: String, _: Arc<CancelToken>) -> Result<bool, DsError> {
        Ok(true)
    }
}

/// A build without the model.
struct NoModel;

impl DenoiseProvider for NoModel {
    fn ensure(&self, _: String, _: Arc<CancelToken>) -> Result<bool, DsError> {
        Ok(false)
    }
}

/// The model turned out damaged.
struct Damaged;

impl DenoiseProvider for Damaged {
    fn ensure(&self, _: String, _: Arc<CancelToken>) -> Result<bool, DsError> {
        Err(DsError::Unsupported {
            message: "The AI Denoise model in this build is damaged.".into(),
        })
    }
}

#[test]
fn export_makes_a_missing_result_just_before_rendering() {
    let _s = serial();
    reset(Some("model-a"), None);
    let p = photo(96, 64, 31);
    let path = Path::new("/photos/S.ARW");
    let provider = Arc::new(Provider {
        photo: p.clone(),
        calls: Mutex::default(),
        fail: false,
    });
    set_denoise_provider(Some(provider.clone()));
    let (l, a) = ex("/photos/S.ARW", &p, &ai(60.0)).unwrap().unwrap();
    assert_eq!(lock(&provider.calls).as_slice(), ["/photos/S.ARW"]);
    assert_eq!(
        l.raw.cfa,
        CfaPattern::LinearRgb,
        "exactly the blend that is rendered"
    );
    assert!(!a.noise.ai_denoise, "not resolved twice");
    assert!(
        lock(store()).blends.is_empty(),
        "the export's blend stays out of the viewer's"
    );
    assert!(lock(store()).original.is_none(), "and so does its original");
    // While a render holds the blend its noise stays seeded; dropping it releases it.
    let seeded = ds_pipeline::raw_noise(&l.raw);
    drop(ExportBlend(Some(l.key)));
    assert_eq!(
        ds_pipeline::raw_noise(&l.raw),
        seeded,
        "still held: still seeded"
    );
    let seed = denoise::seed_of(&l.raw);
    drop(l);
    sweep();
    assert!(
        !lock(store()).seeds.iter().any(|(_, s)| *s == seed),
        "released with the last holder"
    );
    // Ready now: no second call.
    ex("/photos/S.ARW", &p, &ai(60.0)).unwrap().unwrap();
    assert_eq!(lock(&provider.calls).len(), 1);
    // A provider failure (or a cancel) fails that photo, with why.
    reset(Some("model-a"), None);
    set_denoise_provider(Some(Arc::new(Provider {
        photo: p.clone(),
        calls: Mutex::default(),
        fail: true,
    })));
    let err = ex("/photos/S.ARW", &p, &ai(60.0)).unwrap_err();
    assert!(err.contains("cancelled"), "{err}");
    // The app's own reason, as is (not "failed: unsupported: …").
    set_denoise_provider(Some(Arc::new(Damaged)));
    let err = ex("/photos/S.ARW", &p, &ai(60.0)).unwrap_err();
    assert_eq!(err, "The AI Denoise model in this build is damaged.");
    // An export cancelled before a photo asks: no denoise is started for it.
    let calls = Arc::new(Provider {
        photo: p.clone(),
        calls: Mutex::default(),
        fail: false,
    });
    set_denoise_provider(Some(calls.clone()));
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = for_export(path, &p, &ai(60.0), &cancel).unwrap_err();
    assert!(err.contains("cancelled"), "{err}");
    assert!(lock(&calls.calls).is_empty());
    // A provider that "succeeds" without a result: the photo fails, never exported plain.
    set_denoise_provider(Some(Arc::new(Liar)));
    let err = ex("/photos/S.ARW", &p, &ai(60.0)).unwrap_err();
    assert!(
        err.contains("hasn't been run") || err.contains("dropped"),
        "{err}"
    );
    set_denoise_provider(None);
}

#[test]
fn drafts_reuse_the_last_blend_while_a_strength_is_dragged() {
    let _s = serial();
    reset(Some("model-a"), None);
    let p = photo(96, 64, 32);
    denoise_with_blur(&p);
    let (settled, _) = effective(&p, &ai(40.0), Priority::Interactive, false).unwrap();
    let (draft, a) = effective(&p, &ai(47.0), Priority::Interactive, true).unwrap();
    assert_eq!(
        draft.key, settled.key,
        "the 40% blend stands in during the drag"
    );
    assert!(
        (a.noise.luminance - Adjustments::default().noise.luminance * 0.6).abs() < 1e-4,
        "with its own strength's luminance"
    );
    let (exact, _) = effective(&p, &ai(47.0), Priority::Interactive, false).unwrap();
    assert_ne!(exact.key, settled.key, "the settled render makes 47%");
    reset(Some("model-a"), None);
    assert!(
        effective(&p, &ai(47.0), Priority::Interactive, true).is_none(),
        "no blend to stand in and no result: the original"
    );
}

#[test]
fn the_disk_cache_keeps_to_its_budget_oldest_first() {
    let _s = serial();
    let t = tempfile::tempdir().unwrap();
    // Room for about two results of 96×64 (36 KB each).
    reset(Some("model-a"), Some(t.path()));
    set_denoise_cache(Some(t.path().to_string_lossy().into_owned()), 80_000).unwrap();
    let photos: Vec<_> = (0..3).map(|i| photo(96, 64, 10 + i)).collect();
    let mut seen = std::collections::HashSet::new();
    let base = std::time::SystemTime::now() - std::time::Duration::from_secs(1000);
    for (i, p) in photos.iter().enumerate() {
        denoise_with_blur(p);
        // Distinct, increasing modification times (file systems may have 1 s resolution).
        for e in std::fs::read_dir(t.path()).unwrap().filter_map(Result::ok) {
            if seen.insert(e.path()) {
                let at = base + std::time::Duration::from_secs(10 * i as u64);
                File::options()
                    .write(true)
                    .open(e.path())
                    .unwrap()
                    .set_modified(at)
                    .unwrap();
            }
        }
    }
    let left = std::fs::read_dir(t.path()).unwrap().count();
    assert_eq!(left, 2, "the oldest result went");
    // Memory forgotten: the first photo needs denoising again, the last two don't.
    reset(Some("model-a"), Some(t.path()));
    set_denoise_cache(Some(t.path().to_string_lossy().into_owned()), 80_000).unwrap();
    assert!(!ready(&photos[0]));
    assert!(ready(&photos[1]) && ready(&photos[2]));
}

#[test]
fn tile_calls_validate_caller_memory_and_the_task_is_single_use() {
    let _s = serial();
    reset(Some("model-a"), None);
    let p = photo(64, 48, 20);
    let task = Arc::new(start(&p, true).unwrap());
    assert_eq!(task.tile_count().unwrap(), 1);
    assert_eq!(task.tile_size().unwrap(), 512);
    let mut buf = vec![0u16; 3 * 512 * 512 + 1];
    let ptr = buf.as_mut_ptr() as u64;
    let len = (buf.len() - 1) as u64;
    assert!(task.read_tile(0, 0, len).is_err(), "null");
    assert!(task.read_tile(0, ptr + 1, len).is_err(), "misaligned");
    assert!(task.read_tile(0, ptr, 10).is_err(), "short");
    assert!(
        task.read_tile(0, ptr, u64::MAX).is_err(),
        "length out of range"
    );
    assert!(task.read_tile(7, ptr, len).is_err(), "no such tile");
    assert!(
        task.write_tile(0, ptr, len, 511, 512 * 512).is_err(),
        "bad stride"
    );
    // Finishing before every tile is written fails and uses the task up.
    assert!(block_on(task.clone().finish(CancelToken::new())).is_err());
    assert!(task.tile_count().is_err());
    assert!(task.read_tile(0, ptr, len).is_err());
    assert!(task.write_tile(0, ptr, len, 512, 512 * 512).is_err());
    assert!(
        block_on(task.finish(CancelToken::new())).is_err(),
        "finished already"
    );
    assert!(!ready(&p));
    assert!(format!("{:?}", start(&p, false).unwrap()).contains("DenoiseTask"));
}

#[test]
fn export_uses_the_denoised_image_and_refuses_without_one() {
    let _s = serial();
    reset(Some("model-a"), None);
    let p = photo(96, 64, 30);
    let loader_photo = p.clone();
    let json = serde_json::to_string(&ai(100.0)).unwrap();
    let r = crate::export::DevelopRenderer::with_loader(
        [(1, (PathBuf::from("/nonexistent/S.ARW"), Some(json)))].into(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        None,
        Arc::new(move |_: &Path| Ok(loader_photo.clone())),
    );
    let item = ExportItem {
        id: 1,
        name: "S".into(),
        seq: 1,
        ..ExportItem::default()
    };
    let err = r.render(&item, OutputSpace::Srgb).unwrap_err();
    assert!(err.contains("AI Denoise"), "{err}");
    denoise_with_blur(&p);
    let Rendered::Rgb16 { size, data } = r.render(&item, OutputSpace::Srgb).unwrap() else {
        panic!("16-bit full-size render expected");
    };
    assert_eq!(size, Size::new(96, 64));
    // The full-resolution denoised image: smoother than the original's export.
    let mut plain = Adjustments {
        noise: ai(100.0).noise,
        ..Adjustments::default()
    };
    plain.noise.ai_denoise = false;
    let r0 = crate::export::DevelopRenderer::with_loader(
        [(
            1,
            (
                PathBuf::from("/nonexistent/S.ARW"),
                Some(serde_json::to_string(&plain).unwrap()),
            ),
        )]
        .into(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        None,
        Arc::new(move |_: &Path| Ok(p.clone())),
    );
    let Rendered::Rgb16 { data: orig, .. } = r0.render(&item, OutputSpace::Srgb).unwrap() else {
        panic!("16-bit");
    };
    let rough = |d: &[u16]| {
        d.chunks(96 * 3)
            .flat_map(|row| {
                (0..95).map(move |x| f64::from(row[x * 3 + 1].abs_diff(row[(x + 1) * 3 + 1])))
            })
            .sum::<f64>()
    };
    assert!(
        rough(&data) < 0.8 * rough(&orig),
        "{} vs {}",
        rough(&data),
        rough(&orig)
    );
}

#[test]
fn denoise_ready_and_begin_denoise_read_files() {
    let _s = serial();
    reset(Some("model-a"), None);
    let missing = "/nonexistent/denoise/S.ARW".to_owned();
    assert!(block_on(denoise_ready(missing.clone(), CancelToken::new())).is_err());
    assert!(block_on(begin_denoise(missing, false, CancelToken::new())).is_err());
    let Some(sample) = ds_testdata::sample_arw() else {
        return; // the rest needs a real RAW
    };
    let path = sample.to_string_lossy().into_owned();
    assert!(!block_on(denoise_ready(path.clone(), CancelToken::new())).unwrap());
    let task = block_on(begin_denoise(path.clone(), true, CancelToken::new())).unwrap();
    assert_eq!(task.tile_count().unwrap(), 13 * 9, "a 6000×4000 A9 frame");
    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(block_on(begin_denoise(path, false, cancelled)).is_err());
}

/// Timing of a 24 MP result's disk round trip (`cargo test --release -p ds-ffi --lib
/// result_file_timing -- --ignored --nocapture`): written with its checksum, header check,
/// full read with checksum verification.
#[test]
#[ignore = "timing, run by hand"]
fn result_file_timing() {
    let t = tempfile::tempdir().unwrap();
    let size = Size::new(6000, 4000);
    let d = Denoised {
        size,
        data: (0..size.pixels() * 3)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 16) as u16)
            .collect(),
    };
    let key = ResultKey("timing".into());
    let now = std::time::Instant::now;
    let t0 = now();
    write_result(t.path(), u64::MAX, &key, &d).unwrap();
    let write = t0.elapsed();
    let t0 = now();
    let sum = checksum(&d.data);
    let hash = t0.elapsed();
    lock(store()).dir = Some((t.path().to_path_buf(), u64::MAX));
    let t0 = now();
    assert!(has_result(&key, size));
    let header = t0.elapsed();
    let t0 = now();
    let back = read_result(&file_of(t.path(), &key), size)
        .ok()
        .flatten()
        .unwrap();
    let read = t0.elapsed();
    lock(store()).dir = None;
    assert_eq!(back.data, d.data);
    assert_eq!(sum, checksum(&back.data));
    println!(
        "24 MP result (144 MB): write+fsync {write:?}, checksum {hash:?}, header check {header:?}, read+verify {read:?}"
    );
}
