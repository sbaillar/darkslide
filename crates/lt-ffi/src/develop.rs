//! The process-wide develop context every live render goes through: the viewer,
//! grid thumbnails of edited photos, preset previews, auto tools, and export.
//!
//! - One [`GpuRenderer`] for the process (it is `Clone + Send + Sync` and schedules
//!   `Priority::Interactive` renders ahead of `Priority::Background` ones, so an export
//!   or thumbnail batch never starves the viewer). When no GPU adapter is available the
//!   CPU reference renderer is used and [`Develop::backend`] says so ("CPU rendering").
//! - A decoded-RAW cache keyed by path + size + mtime (a replaced file is decoded again),
//!   bounded by bytes, so navigating back and forth doesn't re-decode.
//!
//! Callers own their output buffers (`render` writes into `out`), so the Swift side
//! can hand in its Metal buffer's memory with no intermediate copy.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use lt_core::{Adjustments, RawImage};
use lt_pipeline::{
    CaptureInfo, CpuRenderer, GpuRenderer, ImageKey, OutputFormat, PipelineError, RenderRequest,
    RenderedFrame, Renderer,
};

use crate::error::DsError;

/// One decode in progress (or done): concurrent loads of a file wait on it.
type Decoding = OnceLock<Result<Loaded, DsError>>;

/// A decoded original ready to render.
#[derive(Clone)]
pub(crate) struct Loaded {
    pub key: ImageKey,
    pub raw: Arc<RawImage>,
    pub capture: CaptureInfo,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loaded")
            .field("key", &self.key)
            .field("capture", &self.capture)
            .finish_non_exhaustive()
    }
}

/// Default budget of the decoded-RAW cache (a 24 MP Sony RAW is ~48 MB decoded).
pub(crate) const RAW_CACHE_BYTES: u64 = 512 << 20;

struct RawCache {
    budget: u64,
    bytes: u64,
    /// Most recently used last.
    entries: VecDeque<(ImageKey, PathBuf, Loaded, u64)>,
    /// The file the viewer shows: never evicted for other decodes (a long export or a
    /// thumbnail storm used to push it out, costing the next drag a 40 ms re-decode).
    pinned: Option<PathBuf>,
}

impl RawCache {
    fn get(&mut self, key: ImageKey) -> Option<Loaded> {
        let i = self.entries.iter().position(|e| e.0 == key)?;
        let e = self.entries.remove(i)?;
        let loaded = e.2.clone();
        self.entries.push_back(e);
        Some(loaded)
    }

    fn insert(&mut self, path: PathBuf, loaded: Loaded) -> Vec<ImageKey> {
        let size = raw_bytes(&loaded.raw);
        // An older version of the same file (or a duplicate decode) leaves the cache.
        let mut evicted: Vec<ImageKey> = self
            .entries
            .iter()
            .filter(|e| e.1 == path && e.0 != loaded.key)
            .map(|e| e.0)
            .collect();
        self.entries.retain(|e| e.1 != path);
        self.entries.push_back((loaded.key, path, loaded, size));
        self.bytes = self.entries.iter().map(|e| e.3).sum();
        // Evict least recently used first, skipping the pinned file and the entry just
        // inserted (it is about to be used).
        while self.bytes > self.budget {
            let victim = self.entries.iter().position(|e| {
                e.0 != self.entries.back().map_or(e.0, |b| b.0)
                    && self.pinned.as_ref() != Some(&e.1)
            });
            let Some(i) = victim else { break };
            if let Some(e) = self.entries.remove(i) {
                self.bytes -= e.3;
                evicted.push(e.0);
            }
        }
        evicted
    }
}

fn raw_bytes(raw: &RawImage) -> u64 {
    std::mem::size_of_val(raw.data.as_slice()) as u64
}

/// GPU renders that fail with a GPU error this many times in a row (each after the
/// pipeline's own device rebuild) switch the process to the CPU renderer (A4: a failed
/// rebuild, e.g. an unplugged eGPU, would otherwise fail every render for good).
const GPU_FAILURES_BEFORE_CPU: u32 = 3;

/// See the module docs.
pub(crate) struct Develop {
    gpu: Result<GpuRenderer, String>,
    /// Set once the GPU keeps failing after rebuilds: renders use the CPU from then on.
    gpu_lost: Mutex<Option<String>>,
    gpu_failures: AtomicU32,
    /// Shared: clones render concurrently (the viewer never waits for an export).
    cpu: CpuRenderer,
    raws: Mutex<RawCache>,
    /// Decodes in progress: concurrent loads of the same file wait for one decode.
    loading: Mutex<HashMap<ImageKey, Arc<Decoding>>>,
}

impl Develop {
    fn new(gpu: Result<GpuRenderer, String>, budget: u64) -> Self {
        Self {
            gpu,
            gpu_lost: Mutex::new(None),
            gpu_failures: AtomicU32::new(0),
            cpu: lt_pipeline::cpu_renderer(),
            raws: Mutex::new(RawCache {
                budget,
                bytes: 0,
                entries: VecDeque::new(),
                pinned: None,
            }),
            loading: Mutex::default(),
        }
    }

    /// The shared GPU renderer, if there is one (and it hasn't been given up on).
    pub fn gpu(&self) -> Option<&GpuRenderer> {
        if lock(&self.gpu_lost).is_some() {
            return None;
        }
        self.gpu.as_ref().ok()
    }

    /// "Apple M4 Pro (Metal)" or "CPU rendering (<why>)".
    pub fn backend(&self) -> String {
        if let Some(why) = lock(&self.gpu_lost).as_ref() {
            return format!("CPU rendering (GPU unavailable: {why})");
        }
        match &self.gpu {
            Ok(g) => g.adapter_name().to_owned(),
            Err(why) => format!("CPU rendering ({why})"),
        }
    }

    /// Keep `path`'s decode cached whatever else is decoded (the photo on screen), or
    /// release the pin with `None`.
    pub fn pin(&self, path: Option<&Path>) {
        lock(&self.raws).pinned = path.map(Path::to_path_buf);
    }

    /// Whether the current version of `path` is in the decoded-RAW cache (tests).
    #[cfg(test)]
    pub fn is_cached(&self, path: &Path) -> bool {
        let Ok(md) = std::fs::metadata(path) else {
            return false;
        };
        let key = image_key(path, &md);
        lock(&self.raws).entries.iter().any(|e| e.0 == key)
    }

    /// Decode `path` (or return the cached decode of the same file version).
    pub fn load(&self, path: &Path) -> Result<Loaded, DsError> {
        let md = std::fs::metadata(path)?;
        let key = image_key(path, &md);
        if let Some(l) = lock(&self.raws).get(key) {
            return Ok(l);
        }
        let cell = lock(&self.loading).entry(key).or_default().clone();
        let r = cell.get_or_init(|| self.decode(path, key)).clone();
        // Remove only this decode's cell: a later decode of the same key may already
        // have replaced it (A7).
        let mut loading = lock(&self.loading);
        if loading.get(&key).is_some_and(|c| Arc::ptr_eq(c, &cell)) {
            loading.remove(&key);
        }
        r
    }

    /// Decode for a one-off tool run over many photos (matching): the cached decode
    /// when there is one, else a fresh decode that is *not* cached, so the run doesn't
    /// evict the viewer's photos. `true` with a fresh decode: the caller drops its
    /// renderer caches ([`Self::evict`]) when done with it.
    pub(crate) fn load_transient(&self, path: &Path) -> Result<(Loaded, bool), DsError> {
        let md = std::fs::metadata(path)?;
        let key = image_key(path, &md);
        if let Some(l) = lock(&self.raws).get(key) {
            return Ok((l, false));
        }
        Ok((Self::decode_uncached(path, key)?, true))
    }

    fn decode_uncached(path: &Path, key: ImageKey) -> Result<Loaded, DsError> {
        let raw = lt_raw::decode(path)?;
        let capture = lt_raw::read_metadata(path)
            .map(|m| capture_info(&m))
            .unwrap_or_default();
        Ok(Loaded {
            key,
            raw: Arc::new(raw),
            capture,
        })
    }

    fn decode(&self, path: &Path, key: ImageKey) -> Result<Loaded, DsError> {
        let loaded = Self::decode_uncached(path, key)?;
        let evicted = lock(&self.raws).insert(path.to_owned(), loaded.clone());
        for k in evicted {
            self.evict(k);
        }
        Ok(loaded)
    }

    /// Drop the renderer caches for `key` (the decoded RAW, or an AI-denoised image,
    /// left its cache, or a transient frame such as a matching probe is done).
    pub(crate) fn evict(&self, key: ImageKey) {
        if let Ok(g) = &self.gpu {
            g.clone().evict(key);
        }
        self.cpu.clone().evict(key);
    }

    /// Render `loaded` with `adj` into `out` (see [`Renderer::render_frame_into`]). The
    /// request's capture info is filled from the file when the caller left it empty.
    pub fn render(
        &self,
        loaded: &Loaded,
        adj: &Adjustments,
        req: &RenderRequest,
        format: OutputFormat,
        out: &mut [u8],
    ) -> Result<RenderedFrame, DsError> {
        let mut req = req.clone();
        if req.capture == CaptureInfo::default() {
            req.capture = loaded.capture.clone();
        }
        if req.inpainter.is_none() {
            req.inpainter = crate::removal::inpainter();
        }
        // AI Denoise: the denoised image at the edit's strength, when there is one.
        let denoised = crate::denoise::effective(
            loaded,
            adj,
            req.priority,
            req.quality == lt_pipeline::Quality::Draft,
        );
        let (loaded, adj) = denoised.as_ref().map_or((loaded, adj), |(l, a)| (l, a));
        // No hop off rayon workers is needed: the pipeline runs all its parallel loops on
        // its own pool (`lt_pipeline::par`), so a caller blocked on a GPU class slot never
        // holds a worker the slot holder needs (M3; regression test: lt-pipeline
        // `tests/deadlock.rs`).
        self.render_now(loaded, adj, &req, format, out)
    }

    /// Record a GPU failure (after the pipeline's rebuild attempt). Returns whether the
    /// process has now switched to the CPU renderer.
    fn gpu_failed(&self, why: &str) -> bool {
        let n = self.gpu_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= GPU_FAILURES_BEFORE_CPU {
            *lock(&self.gpu_lost) = Some(why.to_owned());
            return true;
        }
        false
    }

    pub(crate) fn render_now(
        &self,
        loaded: &Loaded,
        adj: &Adjustments,
        req: &RenderRequest,
        format: OutputFormat,
        out: &mut [u8],
    ) -> Result<RenderedFrame, DsError> {
        if let Some(g) = self.gpu() {
            // One retry: the pipeline rebuilds a lost device on the next render.
            for attempt in 0..2 {
                match g
                    .clone()
                    .render_frame_into(loaded.key, &loaded.raw, adj, req, format, out)
                {
                    Ok(f) => {
                        self.gpu_failures.store(0, Ordering::Relaxed);
                        return Ok(f);
                    }
                    Err(PipelineError::Gpu(why)) => {
                        if self.gpu_failed(&why) {
                            break; // this render goes to the CPU
                        }
                        if attempt == 1 {
                            return Err(pipeline_error(PipelineError::Gpu(why)));
                        }
                    }
                    Err(e) => return Err(pipeline_error(e)),
                }
            }
        }
        // The shared CPU renderer: clones render concurrently, so the viewer never waits
        // for an export (M7).
        self.cpu
            .clone()
            .render_frame_into(loaded.key, &loaded.raw, adj, req, format, out)
            .map_err(pipeline_error)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The process-wide context (created on first use).
pub(crate) fn develop() -> &'static Develop {
    static CTX: OnceLock<Develop> = OnceLock::new();
    CTX.get_or_init(|| {
        Develop::new(
            lt_pipeline::gpu_renderer().map_err(|e| e.to_string()),
            RAW_CACHE_BYTES,
        )
    })
}

/// Stable identity of one version of a file: path, size, mtime, inode and ctime. The
/// inode and ctime change when a file is replaced even if the size and mtime were
/// preserved (`cp -p`, `rsync -a` from a backup write a new file), so a replacement
/// never serves a stale decode or thumbnail (L2), without hashing gigabytes.
pub(crate) fn image_key(path: &Path, md: &std::fs::Metadata) -> ImageKey {
    use std::os::unix::fs::MetadataExt;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    md.len().hash(&mut h);
    md.modified().ok().hash(&mut h);
    md.ino().hash(&mut h);
    (md.ctime(), md.ctime_nsec()).hash(&mut h);
    ImageKey(h.finish())
}

/// What the pipeline needs from the capture metadata (lens profile, ISO for NR).
pub(crate) fn capture_info(m: &lt_raw::ImageMeta) -> CaptureInfo {
    CaptureInfo {
        lens: m.lens.clone(),
        focal_length: m.focal_length,
        aperture: m.aperture,
        iso: m.iso,
        crop_factor: None,
        camera: Some(format!("{} {}", m.make, m.model).trim().to_owned()).filter(|c| !c.is_empty()),
    }
}

pub(crate) fn pipeline_error(e: PipelineError) -> DsError {
    match e {
        PipelineError::Cancelled => DsError::Cancelled,
        PipelineError::InvalidRequest(m) => DsError::invalid(m),
        PipelineError::InvalidRaw(e) => DsError::Unsupported {
            message: e.to_string(),
        },
        e @ (PipelineError::NoAdapter | PipelineError::Gpu(_)) => DsError::Internal {
            message: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lt_core::Size;

    #[test]
    fn renders_a_real_sample_on_the_shared_context_and_caches_the_decode() {
        let Some(path) = lt_testdata::sample_arw() else {
            return;
        };
        // The cache check uses its own context: other tests share the process-wide
        // cache and can evict this decode between the two loads (seen in full runs).
        let own = Develop::new(Err("cache check".into()), RAW_CACHE_BYTES);
        let first = own.load(&path).unwrap();
        let second = own.load(&path).unwrap();
        assert!(
            Arc::ptr_eq(&first.raw, &second.raw),
            "second load comes from the cache"
        );
        let d = develop();
        let a = d.load(&path).unwrap();
        assert!(
            a.capture.iso.is_some() && a.capture.lens.is_some(),
            "{:?}",
            a.capture
        );
        let req = RenderRequest {
            histogram: true,
            ..RenderRequest::new(Size::new(600, 600))
        };
        let size = lt_pipeline::output_size(&a.raw, &Adjustments::default(), &req).unwrap();
        let mut out = vec![0u8; size.pixels() as usize * 4];
        let f = d
            .render(
                &a,
                &Adjustments::default(),
                &req,
                OutputFormat::Rgba8,
                &mut out,
            )
            .unwrap();
        assert_eq!(f.size, size);
        assert!(f.histogram.is_some());
        assert!(out.chunks(4).any(|p| p[0] > 20), "not black");
        assert_ne!(d.backend().len(), 0);
    }

    #[test]
    fn concurrent_loads_of_one_file_decode_once() {
        let Some(path) = lt_testdata::sample_arw() else {
            return;
        };
        let d = Develop::new(Err("test".into()), RAW_CACHE_BYTES);
        let loads: Vec<Loaded> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..4).map(|_| s.spawn(|| d.load(&path).unwrap())).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(
            loads.iter().all(|l| Arc::ptr_eq(&l.raw, &loads[0].raw)),
            "one decode shared"
        );
        assert_eq!(lock(&d.raws).entries.len(), 1);
        assert!(lock(&d.loading).is_empty());
    }

    #[test]
    fn cpu_fallback_and_errors() {
        let d = Develop::new(Err("no adapter".into()), 1);
        assert!(d.gpu().is_none());
        assert_eq!(d.backend(), "CPU rendering (no adapter)");
        let t = tempfile::tempdir().unwrap();
        let bad = t.path().join("x.ARW");
        std::fs::write(&bad, b"not a raw").unwrap();
        assert!(matches!(
            d.load(&bad),
            Err(DsError::Unsupported { .. } | DsError::Io { .. })
        ));
        assert!(matches!(
            d.load(&t.path().join("missing.ARW")),
            Err(DsError::Io { .. })
        ));
        assert!(matches!(
            pipeline_error(PipelineError::Cancelled),
            DsError::Cancelled
        ));
        assert!(matches!(
            pipeline_error(PipelineError::InvalidRequest("x".into())),
            DsError::InvalidArgument { .. }
        ));
        assert!(matches!(
            pipeline_error(PipelineError::NoAdapter),
            DsError::Internal { .. }
        ));
    }

    #[test]
    fn cpu_renders_and_the_cache_is_bounded() {
        let samples = lt_testdata::samples_with_ext("ARW");
        let [p1, p2, ..] = samples.as_slice() else {
            return;
        };
        // Budget of one byte: every insert evicts the previous decode.
        let d = Develop::new(Err("test".into()), 1);
        let a = d.load(p1).unwrap();
        let _ = d.load(p2).unwrap();
        let again = d.load(p1).unwrap();
        assert!(!Arc::ptr_eq(&a.raw, &again.raw), "evicted, decoded again");
        assert_eq!(lock(&d.raws).entries.len(), 1);
        let req = RenderRequest::new(Size::new(200, 200));
        let size = lt_pipeline::output_size(&again.raw, &Adjustments::default(), &req).unwrap();
        let mut out = vec![0u8; size.pixels() as usize * 4];
        d.render(
            &again,
            &Adjustments::default(),
            &req,
            OutputFormat::Rgba8,
            &mut out,
        )
        .unwrap();
        let mut small = vec![0u8; 4];
        assert!(matches!(
            d.render(
                &again,
                &Adjustments::default(),
                &req,
                OutputFormat::Rgba8,
                &mut small
            ),
            Err(DsError::InvalidArgument { .. })
        ));
    }

    /// A small synthetic Bayer frame (a CPU render of it takes milliseconds).
    fn synthetic(n: u32, seed: u32) -> RawImage {
        let data = (0..n * n)
            .map(|i| {
                let v = i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 20;
                (1000 + v % 3000) as u16
            })
            .collect();
        RawImage {
            size: lt_core::Size::new(n, n),
            cfa: lt_core::CfaPattern::Rggb,
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
            orientation: lt_core::Orientation::Normal,
            default_crop: None,
        }
    }

    fn background_render(d: &Develop, l: &Loaded) -> bool {
        let req = RenderRequest {
            priority: lt_pipeline::Priority::Background,
            ..RenderRequest::new(lt_core::Size::new(128, 128))
        };
        let adj = Adjustments::default();
        let size = lt_pipeline::output_size(&l.raw, &adj, &req).unwrap();
        let mut out = vec![0u8; size.pixels() as usize * 4];
        d.render(l, &adj, &req, OutputFormat::Rgba8, &mut out)
            .is_ok()
    }

    /// L4: the viewer's photo stays decoded whatever else is decoded.
    #[test]
    fn the_pinned_file_is_never_evicted() {
        let samples = lt_testdata::samples_with_ext("ARW");
        let [p1, p2, p3, ..] = samples.as_slice() else {
            return;
        };
        // Budget of one byte: without the pin every insert evicts the rest.
        let d = Develop::new(Err("test".into()), 1);
        let first = d.load(p1).unwrap();
        d.pin(Some(p1));
        d.load(p2).unwrap();
        d.load(p3).unwrap();
        assert!(d.is_cached(p1), "pinned");
        assert!(Arc::ptr_eq(&first.raw, &d.load(p1).unwrap().raw));
        d.pin(None);
        d.load(p2).unwrap();
        assert!(!d.is_cached(p1), "unpinned, evicted again");
    }

    /// A4: repeated GPU failures (a rebuild that keeps failing) switch the process to
    /// the CPU renderer, and the backend says so.
    #[test]
    fn repeated_gpu_failures_fall_back_to_the_cpu() {
        let d = Develop::new(Err("none".into()), RAW_CACHE_BYTES);
        assert!(!d.gpu_failed("lost"));
        assert!(!d.gpu_failed("lost"));
        assert!(d.gpu_failed("lost again"));
        assert!(d.gpu().is_none());
        assert_eq!(d.backend(), "CPU rendering (GPU unavailable: lost again)");
        // The CPU renders still work (the shared CPU renderer; M7's "the viewer never
        // waits for an export" is the pipeline's concern and tested there).
        let l = Loaded {
            key: ImageKey(1),
            raw: Arc::new(synthetic(64, 1)),
            capture: CaptureInfo::default(),
        };
        assert!(background_render(&d, &l));
        let req = RenderRequest::new(lt_core::Size::new(32, 32));
        let size = lt_pipeline::output_size(&l.raw, &Adjustments::default(), &req).unwrap();
        let mut out = vec![0u8; size.pixels() as usize * 4];
        d.render(
            &l,
            &Adjustments::default(),
            &req,
            OutputFormat::Rgba8,
            &mut out,
        )
        .unwrap();
    }

    /// L2: a replacement with the same size and mtime (`cp -p`) is a new version.
    #[test]
    fn replaced_files_with_preserved_mtime_get_a_new_key() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (t.path().join("a"), t.path().join("b"));
        std::fs::write(&a, b"one").unwrap();
        std::fs::write(&b, b"two").unwrap();
        let before = image_key(&a, &std::fs::metadata(&a).unwrap());
        let mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
        std::fs::rename(&b, &a).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let after = image_key(&a, &std::fs::metadata(&a).unwrap());
        assert_ne!(before, after);
    }

    /// A7: a finished load removes only its own in-progress cell.
    #[test]
    fn a_late_waiter_keeps_a_newer_decode_cell() {
        let d = Develop::new(Err("test".into()), RAW_CACHE_BYTES);
        let key = ImageKey(9);
        let newer: Arc<Decoding> = Arc::default();
        lock(&d.loading).insert(key, newer.clone());
        let t = tempfile::tempdir().unwrap();
        // A load of a missing file errors before touching the map.
        assert!(d.load(&t.path().join("x")).is_err());
        assert!(
            lock(&d.loading)
                .get(&key)
                .is_some_and(|c| Arc::ptr_eq(c, &newer))
        );
    }
}
