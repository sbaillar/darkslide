//! AI Denoise through the FFI: the app runs the model (Core ML), Rust does everything
//! around it (see `ds_pipeline::denoise`).
//!
//! - [`begin_denoise`] decodes the RAW (cached), demosaics it at full resolution and
//!   returns a [`DenoiseTask`]: the app reads each tile into its model's input
//!   ([`DenoiseTask::read_tile`]), runs the model and hands the output back
//!   ([`DenoiseTask::write_tile`]), from as many threads as it has model instances, then
//!   [`DenoiseTask::finish`] stitches the tiles and stores the result.
//! - Results are kept per photo **content** (BLAKE3 of the decoded sensor data, so a
//!   rename, a re-tag or a copy of the file finds it), model (`set_denoise_model`, the
//!   model's SHA-256) and `ds_pipeline::denoise::VERSION`: in memory (the last few) and on
//!   disk (`set_denoise_cache`, bounded, least recently used first out).
//! - Every develop render goes through [`effective`]: with AI Denoise on
//!   (`noise.ai_denoise`, strength `noise.ai_strength` > 0) and a result available, it
//!   renders the denoised image blended at that strength (cached per photo and strength;
//!   a slider drag's drafts reuse the photo's last blend), with the luminance NR slider
//!   scaled down by the strength. Without a result it renders the original.
//! - Export resolves each photo with [`for_export`] just before rendering it: a missing
//!   result is made then (the app's [`DenoiseProvider`]), and a photo that asks for AI
//!   Denoise is never exported without it.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use ds_core::{Adjustments, RawImage, Size};
use ds_pipeline::denoise::{self, DenoiseJob, Denoised};
use ds_pipeline::{ImageKey, Priority};

use crate::develop::{Loaded, develop, pipeline_error};
use crate::error::DsError;
use crate::jobs::{CancelToken, guard, run_job_on_thread};

/// Denoised images kept in memory (~144 MB each for a 24 MP RAW): the photo on screen
/// and its two neighbours (warmed by the viewer's prefetch).
const MEMORY_RESULTS: usize = 3;
/// Blended images kept in memory (one per photo and strength, same size), plus the
/// original's linear image of the last photo blended.
const MEMORY_BLENDS: usize = 2;
/// Magic and format version of a cached result file: magic, version, width, height (u32
/// LE), BLAKE3 of the samples (32 bytes), then the samples (u16 LE).
const MAGIC: &[u8; 4] = b"DSDN";
const FILE_VERSION: u32 = 2;
const HEADER: usize = 48;
/// Temporary files older than this are left over from a crash: trimmed.
const STALE_TEMP: std::time::Duration = std::time::Duration::from_secs(3600);
const EXT: &str = "dsdn";
// Result files hold the samples in memory order, little-endian (every Mac).
const _: () = assert!(cfg!(target_endian = "little"));

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Identity of a result: content, model and pipeline version (hex, also the file name).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ResultKey(String);

/// What a render actually develops: the blend and its adjustments (`None`: as asked).
type Resolved = Option<(Loaded, Adjustments)>;

/// One blend in progress or done: concurrent renders of the same photo wait for it.
type BlendCell = Arc<OnceLock<Result<Loaded, String>>>;

/// The app's AI Denoise, for export: denoises a photo now (blocking) when it has no
/// result. Called from export workers, never the main thread.
#[uniffi::export(with_foreign)]
pub trait DenoiseProvider: Send + Sync {
    /// Make sure `path` has a result for the app's model, denoising it now if needed
    /// (waiting for the model's check at launch). `Ok(false)`: this build has no AI
    /// Denoise model, so the original is exported. An error fails that photo's export,
    /// not the export; `cancel` is cancelled when the export is.
    fn ensure(&self, path: String, cancel: Arc<CancelToken>) -> Result<bool, DsError>;
}

#[derive(Default)]
struct Store {
    model: Option<String>,
    dir: Option<(PathBuf, u64)>,
    provider: Option<Arc<dyn DenoiseProvider>>,
    /// Most recently used last.
    results: VecDeque<(ResultKey, Arc<Denoised>)>,
    blends: VecDeque<(ResultKey, u32, BlendCell)>,
    /// The original's linear image of the last photo blended (a strength change then
    /// only re-blends).
    original: Option<(ResultKey, Arc<Denoised>)>,
    /// Content hashes of decoded RAWs (by their file-version key).
    content: HashMap<ImageKey, [u8; 32]>,
    /// Blends whose noise is seeded, released once the last render holding one is done.
    seeds: Vec<(Weak<RawImage>, denoise::NoiseSeed)>,
}

/// Forget the seeded noise of blends nobody holds any more (a render in flight keeps its
/// blend, and so its seed, alive).
fn sweep() {
    let dead: Vec<_> = {
        let mut s = lock(store());
        let (dead, alive): (Vec<_>, Vec<_>) =
            s.seeds.drain(..).partition(|(w, _)| w.strong_count() == 0);
        s.seeds = alive;
        dead
    };
    for (_, seed) in dead {
        denoise::release(seed);
    }
}

fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

/// Use the model with this identity (its SHA-256): results of another model are never
/// used. `None` (or never called) turns AI Denoise off: renders and exports use the
/// originals.
#[uniffi::export]
pub fn set_denoise_model(model_id: Option<String>) -> Result<(), DsError> {
    guard(|| {
        let mut s = lock(store());
        if s.model != model_id {
            s.model = model_id;
            s.results.clear();
            s.original = None;
            let gone = drop_blends(&mut s, |_| true);
            drop(s);
            evict(gone);
        }
        Ok(())
    })
}

/// The app's AI Denoise for exports (`None`: exports fail photos without a result).
#[uniffi::export]
pub fn set_denoise_provider(provider: Option<Arc<dyn DenoiseProvider>>) {
    lock(store()).provider = provider;
}

/// Keep results on disk in `dir` (created if needed), at most `max_bytes` (the least
/// recently used files go first); `None` keeps them in memory only.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI passes owned values
pub fn set_denoise_cache(dir: Option<String>, max_bytes: u64) -> Result<(), DsError> {
    guard(|| {
        let dir = dir.map(PathBuf::from);
        if let Some(d) = &dir {
            std::fs::create_dir_all(d)?;
        }
        lock(store()).dir = dir.map(|d| (d, max_bytes));
        Ok(())
    })
}

/// Content hash of a decoded RAW (memoized per file version).
fn content_hash(loaded: &Loaded) -> [u8; 32] {
    if let Some(h) = lock(store()).content.get(&loaded.key) {
        return *h;
    }
    let mut hasher = blake3::Hasher::new();
    // Serial: this may run inside a blend's `OnceLock` init, which must not wait on a
    // thread pool other renders may be blocked in.
    hasher.update(bytemuck::cast_slice(&loaded.raw.data));
    let mut size = [0u8; 8];
    size[..4].copy_from_slice(&loaded.raw.size.width.to_le_bytes());
    size[4..].copy_from_slice(&loaded.raw.size.height.to_le_bytes());
    hasher.update(&size);
    let h = *hasher.finalize().as_bytes();
    let mut s = lock(store());
    if s.content.len() > 4096 {
        s.content.clear();
    }
    s.content.insert(loaded.key, h);
    h
}

/// The result key of `loaded` under the current model (`None`: no model set).
fn result_key(loaded: &Loaded) -> Option<ResultKey> {
    let model = lock(store()).model.clone()?;
    Some(key_of(
        &content_hash(loaded),
        &model,
        denoise::VERSION,
        ds_pipeline::PIPELINE_VERSION,
    ))
}

/// Content, model, the denoise encoding/stitching version and the pipeline version (the
/// BASE demosaic the result was made from).
fn key_of(content: &[u8; 32], model: &str, version: u32, pipeline: u32) -> ResultKey {
    let mut h = blake3::Hasher::new();
    h.update(content);
    h.update(model.as_bytes());
    h.update(&version.to_le_bytes());
    h.update(&pipeline.to_le_bytes());
    ResultKey(h.finalize().to_hex()[..32].to_owned())
}

fn file_of(dir: &Path, key: &ResultKey) -> PathBuf {
    dir.join(format!("{}.{EXT}", key.0))
}

/// The result for `key`: from memory, else from the disk cache.
fn result(key: &ResultKey, size: Size) -> Option<Arc<Denoised>> {
    let dir = {
        let mut s = lock(store());
        if let Some(i) = s.results.iter().position(|e| &e.0 == key) {
            let e = s.results.remove(i)?;
            let r = e.1.clone();
            s.results.push_back(e);
            return Some(r);
        }
        s.dir.clone()
    };
    let (dir, _) = dir?;
    let path = file_of(&dir, key);
    match read_result(&path, size) {
        Ok(Some(d)) => {
            // Least recently used goes first: a read counts as a use.
            let _ = File::options()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_modified(std::time::SystemTime::now()));
            let d = Arc::new(d);
            remember(key.clone(), d.clone());
            Some(d)
        }
        Ok(None) | Err(ReadError::Unreadable) => None, // unreadable now (permissions, I/O): kept
        Err(ReadError::Damaged) => {
            // Damaged or from another layout: drop it, the photo is denoised again.
            let _ = ds_fsutil::remove_if_exists(&path);
            None
        }
    }
}

/// Whether `key` has a result (memory, or a disk file whose header and length fit:
/// the samples are only read, and checked, when the result is used).
fn has_result(key: &ResultKey, size: Size) -> bool {
    let dir = {
        let s = lock(store());
        if s.results.iter().any(|e| &e.0 == key) {
            return true;
        }
        s.dir.clone()
    };
    let Some((dir, _)) = dir else { return false };
    let Ok(mut f) = File::open(file_of(&dir, key)) else {
        return false;
    };
    let mut header = [0u8; HEADER];
    let len = f.metadata().map_or(0, |m| m.len());
    f.read_exact(&mut header).is_ok()
        && header_size(&header) == Some(size)
        && len == (HEADER + size.pixels() as usize * 6) as u64
}

/// The image size a result file header describes (`None`: not a current result file).
fn header_size(header: &[u8; HEADER]) -> Option<Size> {
    let word =
        |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
    (&header[..4] == MAGIC && word(4) == FILE_VERSION).then(|| Size::new(word(8), word(12)))
}

#[derive(Debug)]
enum ReadError {
    /// Not a valid result (format, size, checksum, truncated): delete it.
    Damaged,
    /// Couldn't read it now (permissions, I/O): keep it.
    Unreadable,
}

impl From<std::io::Error> for ReadError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::Damaged // truncated
        } else {
            Self::Unreadable
        }
    }
}

fn remember(key: ResultKey, d: Arc<Denoised>) {
    let mut s = lock(store());
    s.results.retain(|e| e.0 != key);
    s.results.push_back((key, d));
    while s.results.len() > MEMORY_RESULTS {
        s.results.pop_front();
    }
}

/// Read a result file and check its samples' checksum; `Ok(None)` when there is none.
fn read_result(path: &Path, size: Size) -> Result<Option<Denoised>, ReadError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ReadError::Unreadable),
    };
    let mut r = BufReader::with_capacity(1 << 20, file);
    let mut header = [0u8; HEADER];
    r.read_exact(&mut header)?;
    match header_size(&header) {
        None => return Err(ReadError::Damaged),
        Some(s) if s != size => return Err(ReadError::Damaged),
        Some(_) => {}
    }
    let mut data = vec![0u16; size.pixels() as usize * 3];
    r.read_exact(bytemuck::cast_slice_mut(&mut data))?;
    if r.read(&mut [0u8; 1])? != 0 {
        return Err(ReadError::Damaged);
    }
    if checksum(&data) != header[16..] {
        return Err(ReadError::Damaged);
    }
    Ok(Some(Denoised { size, data }))
}

fn checksum(data: &[u16]) -> [u8; 32] {
    // Serial: see `content_hash`.
    *blake3::hash(bytemuck::cast_slice(data)).as_bytes()
}

/// Write a result file atomically (temp → fsync → rename), then trim the cache to its
/// budget (oldest first, never the file just written).
fn write_result(dir: &Path, budget: u64, key: &ResultKey, d: &Denoised) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = file_of(dir, key);
    let tmp = ds_fsutil::temp_path(dir, "tmp");
    let res = (|| {
        let f = File::options().write(true).create_new(true).open(&tmp)?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        w.write_all(MAGIC)?;
        for v in [FILE_VERSION, d.size.width, d.size.height] {
            w.write_all(&v.to_le_bytes())?;
        }
        w.write_all(&checksum(&d.data))?;
        w.write_all(bytemuck::cast_slice(&d.data))?;
        let f = w
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res?;
    trim(dir, budget, &path)
}

/// Delete the least recently used result files until the cache fits `budget`, and
/// temporary files a crash left behind.
fn trim(dir: &Path, budget: u64, keep: &Path) -> std::io::Result<()> {
    let paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    for p in &paths {
        let stale = p.extension().is_some_and(|e| e == "tmp")
            && p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".ds-"))
            && std::fs::metadata(p)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > STALE_TEMP));
        if stale {
            let _ = ds_fsutil::remove_if_exists(p);
        }
    }
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = paths
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == EXT))
        .filter_map(|p| {
            let m = std::fs::metadata(&p).ok()?;
            Some((m.modified().ok()?, m.len(), p))
        })
        .collect();
    files.sort();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    for (_, len, p) in files {
        if total <= budget {
            break;
        }
        if p != keep && ds_fsutil::remove_if_exists(&p).is_ok() {
            total -= len;
        }
    }
    Ok(())
}

/// Drop cached blends matching `which`; returns them for [`evict`] (called once the
/// store's lock is released).
fn drop_blends(s: &mut Store, which: impl Fn(&ResultKey) -> bool) -> Vec<Loaded> {
    let mut gone = Vec::new();
    s.blends.retain(|(k, _, cell)| {
        let keep = !which(k);
        if !keep && let Some(Ok(l)) = cell.get() {
            gone.push(l.clone());
        }
        keep
    });
    gone
}

/// Drop the renderers' caches of blends that left the store (their seeds go when the
/// last render holding them is done: [`sweep`]).
fn evict(blends: Vec<Loaded>) {
    for l in &blends {
        develop().evict(l.key);
    }
    drop(blends);
    sweep();
}

/// Warm what a render of `loaded` with `adj` will need (a neighbour of the photo on
/// screen): its result read from disk into memory, so moving to it doesn't wait for a
/// 144 MB read. Returns whether there was one.
pub(crate) fn warm(loaded: &Loaded, adj: &Adjustments) -> bool {
    strength(adj)
        .and(result_key(loaded))
        .is_some_and(|k| result(&k, loaded.raw.size).is_some())
}

/// Whether `adj` asks for AI Denoise.
pub(crate) fn wanted(adj: &Adjustments) -> bool {
    strength(adj).is_some()
}

/// Strength in hundredths (1..=100), `None` when AI Denoise is off.
fn strength(adj: &Adjustments) -> Option<u32> {
    let q = (adj.noise.ai_fraction() * 100.0).round() as u32;
    (q > 0).then_some(q)
}

/// What a render of `loaded` with `adj` actually develops: the denoised image at the
/// edit's strength with the luminance NR scaled down by it, or `None` (render the
/// original: AI Denoise off, a grid thumbnail, or no result for this photo and model
/// yet). `draft` (a slider being dragged) reuses the photo's last blend rather than make
/// one per slider step; the settled render makes the exact one.
pub(crate) fn effective(
    loaded: &Loaded,
    adj: &Adjustments,
    priority: Priority,
    draft: bool,
) -> Option<(Loaded, Adjustments)> {
    // Grid thumbnails (bulk renders) show the original: denoising is invisible at their
    // size, and a full-resolution blend per thumbnail would push the viewer's out.
    if priority == Priority::Bulk {
        return None;
    }
    resolve(loaded, adj, priority, draft).ok().flatten()
}

/// [`effective`], with why a wanted blend couldn't be made.
fn resolve(
    loaded: &Loaded,
    adj: &Adjustments,
    priority: Priority,
    draft: bool,
) -> Result<Resolved, String> {
    let Some(q) = strength(adj) else {
        return Ok(None);
    };
    let Some(key) = result_key(loaded) else {
        return Ok(None); // no model: AI Denoise is unavailable
    };
    sweep();
    let (cell, gone) = {
        let mut s = lock(store());
        let exact = s.blends.iter().position(|e| e.0 == key && e.1 == q);
        // A drag reuses the photo's last blend, made or still being made (one cell per
        // drag, not one per slider step).
        let any_done = || {
            s.blends
                .iter()
                .rposition(|e| e.0 == key && e.2.get().is_none_or(Result::is_ok))
        };
        if let Some(i) = exact.or_else(|| draft.then(any_done).flatten()) {
            let e = s.blends.remove(i).expect("index from position");
            let cell = e.2.clone();
            s.blends.push_back(e);
            (cell, Vec::new())
        } else {
            let cell = BlendCell::default();
            s.blends.push_back((key.clone(), q, cell.clone()));
            let excess = s.blends.len().saturating_sub(MEMORY_BLENDS);
            let gone = s
                .blends
                .drain(..excess)
                .filter_map(|(_, _, c)| c.get().cloned().and_then(Result::ok))
                .collect();
            (cell, gone)
        }
    };
    evict(gone);
    let blended = cell
        .get_or_init(|| blend(loaded, &key, q, priority, Blend::Viewer))
        .clone();
    match blended {
        Ok(b) => {
            let q = blend_strength(&b).unwrap_or(q);
            Ok(Some((b, rendered_adjustments(adj, q))))
        }
        Err(e) => {
            // No result yet, or it failed: forget the cell so a later render tries again.
            lock(store())
                .blends
                .retain(|e| !(e.0 == key && Arc::ptr_eq(&e.2, &cell)));
            Err(e)
        }
    }
}

/// The strength a cached blend was made at (a draft may render another strength's).
fn blend_strength(b: &Loaded) -> Option<u32> {
    lock(store())
        .blends
        .iter()
        .find(|e| {
            e.2.get()
                .is_some_and(|r| r.as_ref().is_ok_and(|l| l.key == b.key))
        })
        .map(|e| e.1)
}

/// What a blend at strength `q` is developed with: the luminance slider scaled down by
/// the strength, and AI Denoise off (what is rendered is already the denoised image).
fn rendered_adjustments(adj: &Adjustments, q: u32) -> Adjustments {
    let mut adj = adj.clone();
    adj.noise.luminance *= 1.0 - q as f32 / 100.0;
    adj.noise.ai_denoise = false;
    adj
}

/// Who a blend is for: the viewer's are cached (with the photo's original), an export's
/// are its own and touch neither.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Blend {
    Viewer,
    Export,
}

fn blend(
    loaded: &Loaded,
    key: &ResultKey,
    q: u32,
    priority: Priority,
    who: Blend,
) -> Result<Loaded, String> {
    let den = result(key, loaded.raw.size).ok_or("AI Denoise hasn't been run on this photo yet")?;
    let s = q as f32 / 100.0;
    let original = if q < 100 {
        let cached = lock(store())
            .original
            .as_ref()
            .filter(|(k, _)| k == key)
            .map(|(_, o)| o.clone());
        if let Some(o) = cached {
            Some(o)
        } else {
            let o = Arc::new(
                denoise::linear_original(&loaded.raw, priority).map_err(|e| e.to_string())?,
            );
            if who == Blend::Viewer {
                lock(store()).original = Some((key.clone(), o.clone()));
            }
            Some(o)
        }
    } else {
        None
    };
    let raw = Arc::new(
        denoise::denoised_raw(&loaded.raw, &den, original.as_deref(), s)
            .map_err(|e| e.to_string())?,
    );
    lock(store())
        .seeds
        .push((Arc::downgrade(&raw), denoise::seed_of(&raw)));
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ("ai-denoise", loaded.key, &key.0, q, who == Blend::Export).hash(&mut h);
    Ok(Loaded {
        key: ImageKey(h.finish()),
        raw,
        capture: loaded.capture.clone(),
    })
}

/// For export: exactly what to render for `path` (`loaded` with `adj`), just before it
/// renders. A photo asking for AI Denoise without a result is denoised now by the app's
/// [`DenoiseProvider`] (which also waits for the model's check at launch); if it still
/// can't be blended, `Err` fails that photo with why: it is never exported without AI
/// Denoise. `None`: render `loaded` as is (AI Denoise off, or a build without a model,
/// as the provider says). The blend is the export's own: the viewer's cache is untouched;
/// hold an [`ExportBlend`] of it while rendering.
pub(crate) fn for_export(
    path: &Path,
    loaded: &Loaded,
    adj: &Adjustments,
    cancel: &Arc<CancelToken>,
) -> Result<Resolved, String> {
    let Some(q) = strength(adj) else {
        return Ok(None);
    };
    let ensure = || -> Result<bool, String> {
        if cancel.is_cancelled() {
            return Err("AI Denoise was cancelled".into());
        }
        let provider = lock(store()).provider.clone();
        let Some(provider) = provider else {
            return Err("AI Denoise isn't available to this export".into());
        };
        provider
            .ensure(path.to_string_lossy().into_owned(), cancel.clone())
            .map_err(|e| match e {
                DsError::Cancelled => "AI Denoise was cancelled".into(),
                DsError::Unsupported { message } | DsError::Internal { message } => message,
                e => e.to_string(),
            })
    };
    if result_key(loaded).is_none() && !ensure()? {
        return Ok(None); // this build has no AI Denoise model
    }
    let Some(key) = result_key(loaded) else {
        return Err("AI Denoise isn't available: its model isn't loaded".into());
    };
    // Twice at most: a result evicted between its making and its use is made again.
    for _ in 0..2 {
        if !has_result(&key, loaded.raw.size) && !ensure()? {
            return Err("AI Denoise isn't available: its model isn't loaded".into());
        }
        match blend(loaded, &key, q, Priority::Background, Blend::Export) {
            Ok(b) => return Ok(Some((b, rendered_adjustments(adj, q)))),
            Err(_) if !has_result(&key, loaded.raw.size) => {}
            Err(e) => return Err(format!("AI Denoise couldn't be applied: {e}")),
        }
    }
    Err("AI Denoise result was dropped before the export could use it".into())
}

/// An export's blend, rendered: when this goes (the render is done, whatever its outcome),
/// its renderer caches go too (they were for it alone), and its noise seed once the last
/// copy of the blend is dropped.
pub(crate) struct ExportBlend(pub Option<ImageKey>);

impl Drop for ExportBlend {
    fn drop(&mut self) {
        if let Some(key) = self.0 {
            develop().evict(key);
        }
        sweep();
    }
}

/// Whether `path` has an AI Denoise result for the current model (memory or disk).
#[uniffi::export]
pub async fn denoise_ready(path: String, token: Arc<CancelToken>) -> Result<bool, DsError> {
    run_job_on_thread("ds-denoise-status", token, move |_| {
        let loaded = develop().load(Path::new(&path))?;
        Ok(ready(&loaded))
    })
    .await
}

/// Ready, checked: the result is read into memory and its checksum verified (the photo
/// asking is about to be rendered with it, so the read isn't wasted). A damaged file is
/// deleted, so the app denoises the photo again instead of showing the original.
fn ready(loaded: &Loaded) -> bool {
    result_key(loaded).is_some_and(|k| result(&k, loaded.raw.size).is_some())
}

/// One photo being denoised (see the module docs). Tiles are planar RGB `f16`
/// (`[3][tile_size][tile_size]`, sRGB-encoded, 0..1).
#[derive(uniffi::Object)]
pub struct DenoiseTask {
    job: Mutex<Option<Arc<DenoiseJob>>>,
    key: ResultKey,
    priority: Priority,
}

impl std::fmt::Debug for DenoiseTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DenoiseTask")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Start denoising `path` with the current model (`set_denoise_model`): decodes the RAW
/// (cached) and demosaics it. `background`: a batch job (the pipeline's low-priority
/// pool), otherwise the photo on screen.
#[uniffi::export]
pub async fn begin_denoise(
    path: String,
    background: bool,
    token: Arc<CancelToken>,
) -> Result<Arc<DenoiseTask>, DsError> {
    run_job_on_thread("ds-denoise", token, move |cancelled| {
        let loaded = develop().load(Path::new(&path))?;
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        start(&loaded, background).map(Arc::new)
    })
    .await
}

fn start(loaded: &Loaded, background: bool) -> Result<DenoiseTask, DsError> {
    let key = result_key(loaded).ok_or_else(|| DsError::invalid("no denoise model set"))?;
    let priority = if background {
        Priority::Background
    } else {
        Priority::Interactive
    };
    let job = DenoiseJob::new(&loaded.raw, priority).map_err(pipeline_error)?;
    Ok(DenoiseTask {
        job: Mutex::new(Some(Arc::new(job))),
        key,
        priority,
    })
}

/// Validated caller memory of `f16` elements.
pub(crate) fn elements(ptr: u64, len: u64) -> Result<(usize, usize), DsError> {
    let addr = usize::try_from(ptr).map_err(|_| DsError::invalid("pointer out of range"))?;
    if addr == 0 || addr % std::mem::align_of::<u16>() != 0 {
        return Err(DsError::invalid("tile pointer is null or misaligned"));
    }
    let len = usize::try_from(len)
        .ok()
        .filter(|&l| l.checked_mul(2).is_some_and(|b| isize::try_from(b).is_ok()))
        .ok_or_else(|| DsError::invalid("tile length out of range"))?;
    Ok((addr, len))
}

impl DenoiseTask {
    fn job(&self) -> Result<Arc<DenoiseJob>, DsError> {
        lock(&self.job)
            .clone()
            .ok_or_else(|| DsError::invalid("denoise task already finished"))
    }
}

#[uniffi::export]
impl DenoiseTask {
    /// Number of tiles (indices `0..tile_count`).
    pub fn tile_count(&self) -> Result<u32, DsError> {
        guard(|| Ok(self.job()?.grid().len() as u32))
    }

    /// Edge of a (square) tile in pixels.
    pub fn tile_size(&self) -> Result<u32, DsError> {
        guard(|| Ok(self.job()?.grid().tile()))
    }

    /// Write tile `index`'s model input into caller memory: `dest_len` ≥ 3 × `tile_size`²
    /// `f16` elements at `dest_ptr`, valid and untouched by the caller during the call
    /// (a model's input buffer). About 1 ms; call it from the model's worker thread.
    pub fn read_tile(&self, index: u32, dest_ptr: u64, dest_len: u64) -> Result<(), DsError> {
        guard(|| {
            let job = self.job()?;
            let (addr, len) = elements(dest_ptr, dest_len)?;
            // SAFETY: non-null, aligned for u16 and len·2 ≤ isize::MAX were checked in
            // `elements`; the caller guarantees the memory is valid, writable and not
            // accessed elsewhere for the duration of this call.
            #[allow(unsafe_code)]
            let out = unsafe { std::slice::from_raw_parts_mut(addr as *mut u16, len) };
            job.read_tile(index as usize, out).map_err(pipeline_error)
        })
    }

    /// Take tile `index`'s model output from caller memory: `src_len` `f16` elements at
    /// `src_ptr`, element `(c, y, x)` at `c·channel_stride + y·row_stride + x`. Each tile
    /// once. About 1 ms; call it from the model's worker thread.
    pub fn write_tile(
        &self,
        index: u32,
        src_ptr: u64,
        src_len: u64,
        row_stride: u64,
        channel_stride: u64,
    ) -> Result<(), DsError> {
        guard(|| {
            let job = self.job()?;
            let (addr, len) = elements(src_ptr, src_len)?;
            let stride = |s: u64| usize::try_from(s).map_err(|_| DsError::invalid("stride"));
            // SAFETY: as in `read_tile`; the memory is only read.
            #[allow(unsafe_code)]
            let data = unsafe { std::slice::from_raw_parts(addr as *const u16, len) };
            job.write_tile(
                index as usize,
                data,
                stride(row_stride)?,
                stride(channel_stride)?,
            )
            .map_err(pipeline_error)
        })
    }

    /// Stitch the tiles and store the result (memory, and the disk cache when set):
    /// renders of this photo with AI Denoise on use it from now on. Every tile must have
    /// been written; the task can't be used afterwards.
    pub async fn finish(self: Arc<Self>, token: Arc<CancelToken>) -> Result<(), DsError> {
        let job = lock(&self.job)
            .take()
            .ok_or_else(|| DsError::invalid("denoise task already finished"))?;
        run_job_on_thread("ds-denoise-finish", token, move |_| {
            let job = Arc::try_unwrap(job)
                .map_err(|_| DsError::invalid("tiles are still being read or written"))?;
            let den = Arc::new(job.finish(self.priority).map_err(pipeline_error)?);
            store_result(&self.key, &den)
        })
        .await
    }
}

/// Keep a fresh result (memory and disk) and drop blends made before it.
fn store_result(key: &ResultKey, den: &Arc<Denoised>) -> Result<(), DsError> {
    remember(key.clone(), den.clone());
    let (dir, gone) = {
        let mut s = lock(store());
        let gone = drop_blends(&mut s, |k| k == key);
        (s.dir.clone(), gone)
    };
    evict(gone);
    if let Some((dir, budget)) = dir {
        write_result(&dir, budget, key, den)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "denoise_tests.rs"]
mod tests;
