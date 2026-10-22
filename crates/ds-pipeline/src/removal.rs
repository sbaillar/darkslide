//! Content-aware removal (`Adjustments::removals`): the brushed region is replaced with
//! plausible background and composited into the **RAW samples**, so every later stage —
//! demosaic, the BASE pyramid, crop, straighten, lens corrections, the whole develop — sees
//! the fill as part of the photo, on the CPU and the GPU alike, at every zoom level.
//!
//! Per removal, in order (each one sees the ones before it):
//! 1. The strokes (corrected-E-frame coordinates) are mapped to the sensor through the
//!    render geometry (keystone, lens distortion, EXIF orientation, default crop) and
//!    rasterised as the hole, grown by [`DILATE`] px plus the feather, in [`TILE`] px
//!    tiles (only those the strokes reach).
//! 2. The hole is filled in windows, decided per cluster of touching tiles: one around
//!    an object (≥ [`MIN_CONTEXT`] px of context, as square as the frame allows), or, for
//!    a long thin stroke, one per tile with [`TILE_CONTEXT`] px around it, in order, each
//!    seeing the earlier ones' fill and the later ones' hole as unknown. A window larger
//!    than [`MAX_FULL_EDGE`] (a big object) is filled from a reduced image (box-averaged
//!    band by band) and finished tile by tile at full resolution, texture drawn from the
//!    whole window, so no removal holds a full-resolution frame. A window is demosaiced
//!    at full resolution, turned upright, encoded for display (invertibly: highlights roll off, wide-gamut colours
//!    are desaturated by a factor undone after) and downscaled to the inpainter's size:
//!    the app's model ([`Inpainter`], e.g. `LaMa` on the Neural Engine), else `PatchMatch`
//!    ([`crate::inpaint`]) at [`PATCHMATCH_EDGE`].
//! 3. The fill is upscaled back to sensor resolution and un-tone-mapped; the texture band
//!    the model's resolution lost is transferred from the known surroundings (luminance
//!    band-pass blocks chosen by tone), then the fill is re-mosaiced with synthetic sensor
//!    noise from the crop's own noise model (variance linear in the signal, fitted on the
//!    known RAW samples), so its grain is the camera's at every scale.
//! 4. The new samples are blended in over the feather.
//!
//! The result of each removal is its [`Patches`] (final RAW samples of the sensor
//! rectangles it changes, per tile), cached per (image, removal chain): in memory
//! (byte-bounded) and, when the app sets a directory ([`set_cache_dir`]), on disk (least
//! recently used first beyond [`DISK_BYTES`]). The chain hash covers every removal up to it,
//! the inpainter's identity, the geometry the strokes are mapped with and
//! [`crate::PIPELINE_VERSION`], so editing a removal, adding a lens profile or getting the
//! model recomputes exactly what changed. Renders then use a derived RAW (the original with
//! the patches applied) under a derived [`ImageKey`]: the BASE, GPU upload and mask caches
//! work unchanged. Concurrent renders of a new chain compute it once.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use ds_core::adjust::{BrushStroke, Removal};
use ds_core::{Adjustments, CfaPattern, RawImage, Size};
use rayon::prelude::*;

use crate::color::{mat_inv, mat_vec, srgb_decode, srgb_encode};
use crate::geometry::{self, Fit, Geometry};
use crate::inpaint::{Rng, bilinear, patchmatch};
use crate::{ImageKey, PipelineError, RenderRequest};

/// An on-device inpainting model supplied by the app (Core ML `LaMa`). Called from a render
/// thread, never while the pipeline holds a lock.
pub trait Inpainter: Send + Sync {
    /// Identity of the model and its version (part of every fill's cache key).
    fn id(&self) -> String;
    /// Longest edge the model works at; the crop is downscaled to fit.
    fn max_edge(&self) -> u32;
    /// Fill the `mask` pixels (`w × h`, 255 = fill) of the display-encoded RGB image
    /// `rgb` (`w × h × 3`, row-major). Returns `w × h × 3` bytes, or `None` on failure
    /// (the removal then falls back to `PatchMatch`).
    fn inpaint(&self, w: u32, h: u32, rgb: &[u8], mask: &[u8]) -> Option<Vec<u8>>;
}

/// Identity of the built-in fallback (part of the cache key).
pub const PATCHMATCH_ID: &str = "patchmatch-1";
/// Working size of the `PatchMatch` fallback (long edge).
pub const PATCHMATCH_EDGE: u32 = 768;
/// Growth of the hole beyond the brush (lateral CA, anti-aliased object edges), px.
pub const DILATE: f64 = 2.0;
/// Least context around the hole in the inpainting crop, px.
pub const MIN_CONTEXT: u32 = 48;
/// Memory budget of cached patches.
const PATCH_BYTES: u64 = 256 << 20;
/// Derived RAWs kept (one per edited image recently rendered).
const RAWS: usize = 3;
/// Blur (σ, px) taking the engine's pixel-level noise out of the upscaled fill.
const GRAIN_BLUR: f32 = 0.8;
/// Disk patch format version.
const DISK_VERSION: u32 = 2;
/// Budget of the disk cache: least recently used patches beyond it are deleted.
const DISK_BYTES: u64 = 4 << 30;
/// Largest single cache file read back (a 61 MP linear-RGB frame is under 400 MB).
const MAX_PATCH_FILE: u64 = 1 << 30;
/// Edge of the sensor tiles footprints are rasterised in, and of the windows a long
/// removal is filled in, px.
const TILE: u32 = 768;
/// Context around a tile's hole in its window, px (the window then stays near the
/// engine's working size: filled at about native resolution).
const TILE_CONTEXT: u32 = 192;
/// A footprint whose bounding box is at most this long, or whose run is at least
/// [`RUN_THICKNESS`] thick (covered px per px of its length: an object, whose inner tiles
/// would have no context of their own), is filled in one window; longer, thinner ones (a
/// stroke along a wire) tile by tile.
const ONE_WINDOW_EDGE: u32 = 2 * TILE;
const RUN_THICKNESS: f64 = TILE as f64 / 2.0;
/// Longest window crop worked at full resolution. A larger one (a big object) is filled
/// from a reduced image of [`WORK_EDGE`] px (the engines see less anyway) and finished
/// tile by tile at full resolution, so no removal ever holds a full-resolution frame.
/// At this edge a full-resolution window peaks well under the 1 GB budget on 61 MP (a
/// ~3000 px one peaked at ~1.05 GB).
const MAX_FULL_EDGE: u32 = 2048;
const WORK_EDGE: u32 = 1536;
/// Full-resolution context around a tile when finishing a big window, px.
const FINISH_CONTEXT: u32 = 64;

/// The final RAW samples of one removal's sensor rectangle (everything else unchanged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    /// Sensor rectangle x, y, w, h (empty when the strokes cover nothing).
    pub rect: [u32; 4],
    /// Samples per pixel (1 for a CFA mosaic, 3 for linear RGB).
    pub chans: u32,
    pub data: Vec<u16>,
}

impl Patch {
    fn bytes(&self) -> u64 {
        self.data.len() as u64 * 2 + 64
    }

    /// Write the samples into `raw` (rectangles outside the sensor are clipped).
    pub fn apply(&self, raw: &mut RawImage) {
        let [x, y, w, h] = self.rect;
        let ch = self.chans as usize;
        let sw = raw.size.width as usize;
        let rch = if raw.cfa == CfaPattern::LinearRgb {
            3
        } else {
            1
        };
        if ch != rch || self.data.len() as u64 != u64::from(w) * u64::from(h) * ch as u64 {
            return;
        }
        let x1 = x.saturating_add(w).min(raw.size.width) as usize;
        let y1 = y.saturating_add(h).min(raw.size.height) as usize;
        let (x, y, w) = (x as usize, y as usize, w as usize);
        for yy in y..y1 {
            if x >= x1 {
                break;
            }
            let src = &self.data[((yy - y) * w) * ch..((yy - y) * w + (x1 - x)) * ch];
            raw.data[(yy * sw + x) * ch..(yy * sw + x1) * ch].copy_from_slice(src);
        }
    }

    fn encode_into(&self, b: &mut Vec<u8>) {
        for v in self.rect {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.chans.to_le_bytes());
        for v in &self.data {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }

    /// The patch at the start of `b` and the bytes after it.
    fn decode_from(b: &[u8]) -> Option<(Patch, &[u8])> {
        let word = |i: usize| -> Option<u32> {
            Some(u32::from_le_bytes(
                b.get(i * 4..i * 4 + 4)?.try_into().ok()?,
            ))
        };
        let rect = [word(0)?, word(1)?, word(2)?, word(3)?];
        let chans = word(4)?;
        let n = (rect[2] as usize)
            .checked_mul(rect[3] as usize)?
            .checked_mul(chans as usize)?;
        let body = b.get(20..20 + n.checked_mul(2)?)?;
        let data = body
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        Some((Patch { rect, chans, data }, &b[20 + n * 2..]))
    }
}

/// What one removal changes: the patches of the tiles its footprint covers (long thin
/// strokes touch few of a frame's tiles).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patches(pub Vec<Patch>);

impl Patches {
    fn bytes(&self) -> u64 {
        self.0.iter().map(Patch::bytes).sum::<u64>() + 64
    }

    /// Write every patch's samples into `raw`.
    pub fn apply(&self, raw: &mut RawImage) {
        for p in &self.0 {
            p.apply(raw);
        }
    }

    fn encode(&self) -> Vec<u8> {
        let n: usize = self.0.iter().map(|p| 20 + p.data.len() * 2).sum();
        let mut b = Vec::with_capacity(12 + n);
        b.extend_from_slice(b"DSRM");
        b.extend_from_slice(&DISK_VERSION.to_le_bytes());
        b.extend_from_slice(&(self.0.len() as u32).to_le_bytes());
        for p in &self.0 {
            p.encode_into(&mut b);
        }
        b
    }

    fn decode(b: &[u8]) -> Option<Patches> {
        let word = |i: usize| -> Option<u32> {
            Some(u32::from_le_bytes(
                b.get(i * 4..i * 4 + 4)?.try_into().ok()?,
            ))
        };
        if b.get(..4)? != b"DSRM" || word(1)? != DISK_VERSION {
            return None;
        }
        let mut rest = b.get(12..)?;
        let mut out = Vec::new();
        for _ in 0..word(2)? {
            let (p, r) = Patch::decode_from(rest)?;
            out.push(p);
            rest = r;
        }
        rest.is_empty().then_some(Patches(out))
    }
}

// ----------------------------------------------------------------- caches ---

#[derive(Default)]
struct Store {
    /// Most recently used last.
    patches: VecDeque<((u64, u64), Arc<Patches>)>,
    bytes: u64,
    raws: VecDeque<((u64, u64), ImageKey, Arc<RawImage>)>,
    inflight: HashSet<(u64, u64)>,
    /// Derived keys handed out per image (for eviction).
    derived: HashMap<u64, Vec<ImageKey>>,
    dir: Option<PathBuf>,
    /// Patches computed (cache misses everywhere), for tests and diagnostics.
    computed: u64,
}

impl Store {
    fn patch(&mut self, k: (u64, u64)) -> Option<Arc<Patches>> {
        let i = self.patches.iter().position(|e| e.0 == k)?;
        let e = self.patches.remove(i)?;
        let p = Arc::clone(&e.1);
        self.patches.push_back(e);
        Some(p)
    }

    fn put_patch(&mut self, k: (u64, u64), p: Arc<Patches>) {
        self.patches.retain(|e| e.0 != k);
        self.bytes = self.patches.iter().map(|e| e.1.bytes()).sum();
        self.bytes += p.bytes();
        self.patches.push_back((k, p));
        while self.bytes > PATCH_BYTES && self.patches.len() > 1 {
            if let Some(e) = self.patches.pop_front() {
                self.bytes -= e.1.bytes();
            }
        }
    }
}

fn store() -> &'static (Mutex<Store>, Condvar) {
    static S: OnceLock<(Mutex<Store>, Condvar)> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn locked() -> std::sync::MutexGuard<'static, Store> {
    store()
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Keep computed patches in `dir` too (created when needed, trimmed to [`DISK_BYTES`]
/// in the background), or only in memory (`None`).
pub fn set_cache_dir(dir: Option<PathBuf>) {
    locked().dir.clone_from(&dir);
    if let Some(dir) = dir {
        std::thread::spawn(move || prune(&dir, DISK_BYTES));
    }
}

/// Number of patches computed so far (not found in any cache).
pub fn patches_computed() -> u64 {
    locked().computed
}

/// Drop every cached patch and derived RAW of `key` (memory only) and return the derived
/// keys handed out for it, so the renderers can evict those too.
pub fn forget(key: ImageKey) -> Vec<ImageKey> {
    let mut s = locked();
    s.patches.retain(|e| e.0.0 != key.0);
    s.bytes = s.patches.iter().map(|e| e.1.bytes()).sum();
    s.raws.retain(|e| e.0.0 != key.0);
    s.derived.remove(&key.0).unwrap_or_default()
}

fn disk_path(dir: &Path, k: (u64, u64)) -> PathBuf {
    dir.join(format!("{:016x}-{:016x}.dsrm", k.0, k.1))
}

/// The patches of `k` on disk; a hit counts as a use (its time is refreshed for the LRU).
fn disk_read(dir: Option<&Path>, k: (u64, u64)) -> Option<Patches> {
    let path = disk_path(dir?, k);
    // Patches are at most a few sensor frames; a bigger file is not ours.
    if std::fs::metadata(&path).ok()?.len() > MAX_PATCH_FILE {
        return None;
    }
    let p = Patches::decode(&std::fs::read(&path).ok()?)?;
    let _ = std::fs::File::options()
        .append(true)
        .open(&path)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    Some(p)
}

/// Atomic write (temp → fsync → rename); failures only cost a recomputation later.
fn disk_write(dir: Option<&Path>, k: (u64, u64), p: &Patches) {
    let Some(dir) = dir else { return };
    let path = disk_path(dir, k);
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let ok = std::fs::create_dir_all(dir).is_ok()
        && std::fs::File::create(&tmp)
            .and_then(|mut f| {
                f.write_all(&p.encode())?;
                f.sync_all()
            })
            .is_ok()
        && std::fs::rename(&tmp, &path).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Delete the least recently used patch files of `dir` until they total `budget` bytes,
/// and temporary files left by interrupted writes (an hour old: a younger one may be
/// another process's write in flight).
fn prune(dir: &Path, budget: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .filter_map(|e| {
            let e = e.ok()?;
            let path = e.path();
            let ext = path.extension()?.to_str()?;
            let m = e.metadata().ok()?;
            let modified = m.modified().ok()?;
            if ext.starts_with("tmp") {
                if modified < stale {
                    let _ = std::fs::remove_file(&path);
                }
                return None;
            }
            if ext != "dsrm" {
                return None;
            }
            Some((modified, m.len(), path))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

// ------------------------------------------------------------------ hashing ---

/// FNV-1a over `bytes`, continuing from `h` (stable across runs and builds: disk keys).
fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(h, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3))
}

const FNV0: u64 = 0xcbf2_9ce4_8422_2325;

/// The derived image key of `key` with the removal chain `chain` applied.
fn derived_key(key: ImageKey, chain: u64) -> ImageKey {
    let mut z = (key.0 ^ chain.rotate_left(17)).wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    ImageKey(if z == key.0 { z ^ 1 } else { z })
}

/// Whether a removal paints anything.
fn active(r: &Removal) -> bool {
    r.strokes.iter().any(|s| {
        !s.erase
            && s.size.is_finite()
            && s.size > 0.0
            && s.points.iter().any(|p| p.iter().all(|v| v.is_finite()))
    })
}

/// The geometry strokes are mapped with: corrected E frame → sensor.
fn removal_geometry(
    raw: &RawImage,
    adj: &Adjustments,
    req: &RenderRequest,
) -> Result<Geometry, PipelineError> {
    // Only the optics matter (crop and straighten don't move the E frame).
    let optics = Adjustments {
        lens: adj.lens.clone(),
        ..Adjustments::default()
    };
    geometry::plan(
        raw,
        &optics,
        &req.capture,
        None,
        Size::new(1 << 16, 1 << 16),
        Fit::ConstrainToSensorPixels,
    )
}

/// Hash of everything a removal chain's fill depends on besides its own strokes.
fn chain_seed(geo: &Geometry, engine: &str) -> u64 {
    let sig = format!(
        "v{} {engine} {:?} {:?} {:?} {:?}",
        crate::PIPELINE_VERSION,
        geo.e_size,
        geo.keystone,
        geo.lens,
        geo.e_to_sensor
    );
    fnv(FNV0, sig.as_bytes())
}

/// Hash of one removal, continuing the chain from `h`.
fn chain_step(h: u64, r: &Removal) -> u64 {
    fnv(h, format!("{r:?}").as_bytes())
}

// ------------------------------------------------------------------ resolve ---

/// The RAW (and its cache key) a render of `adj` uses: `None` when it has no removals
/// (render the original), else the original with every removal's patch applied —
/// computing the patches not cached yet (see the module docs).
pub fn resolve(
    key: ImageKey,
    raw: &RawImage,
    adj: &Adjustments,
    req: &RenderRequest,
) -> Result<Option<(ImageKey, Arc<RawImage>)>, PipelineError> {
    let removals: Vec<&Removal> = adj.removals.iter().filter(|r| active(r)).collect();
    if removals.is_empty() {
        return Ok(None);
    }
    let geo = removal_geometry(raw, adj, req)?;
    let engine = req
        .inpainter
        .as_ref()
        .map_or_else(|| PATCHMATCH_ID.to_owned(), |i| i.id());
    let mut h = chain_seed(&geo, &engine);
    let chain: Vec<u64> = removals
        .iter()
        .map(|r| {
            h = chain_step(h, r);
            h
        })
        .collect();
    let last = h;
    {
        let mut s = locked();
        if let Some(i) = s.raws.iter().position(|e| e.0 == (key.0, last)) {
            let e = s.raws.remove(i).expect("position is valid");
            let hit = (e.1, Arc::clone(&e.2));
            s.raws.push_back(e);
            return Ok(Some(hit));
        }
    }
    let mut cur = raw.clone();
    for (r, &hc) in removals.iter().zip(&chain) {
        req.check_cancel()?;
        // A computed fill is applied to `cur` as it goes; a cached one here.
        let patch = patch_for((key.0, hc), &mut cur, r, &geo, req)?;
        patch.apply(&mut cur);
    }
    let dk = derived_key(key, last);
    let cur = Arc::new(cur);
    let mut s = locked();
    s.raws.retain(|e| e.0 != (key.0, last));
    s.raws.push_back(((key.0, last), dk, Arc::clone(&cur)));
    while s.raws.len() > RAWS {
        s.raws.pop_front();
    }
    let d = s.derived.entry(key.0).or_default();
    if !d.contains(&dk) {
        d.push(dk);
    }
    Ok(Some((dk, cur)))
}

/// Removes `k` from the in-flight set when dropped (also on a panic or an error).
struct Inflight((u64, u64));

impl Drop for Inflight {
    fn drop(&mut self) {
        locked().inflight.remove(&self.0);
        store().1.notify_all();
    }
}

/// The patches of chain step `k`: cached, being computed by another render (wait for
/// it, or for this render's cancellation), on disk, or computed now (on `raw`, which is
/// left with them applied).
fn patch_for(
    k: (u64, u64),
    raw: &mut RawImage,
    r: &Removal,
    geo: &Geometry,
    req: &RenderRequest,
) -> Result<Arc<Patches>, PipelineError> {
    let dir = {
        let (m, cv) = store();
        let mut s = m.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(p) = s.patch(k) {
                return Ok(p);
            }
            if !s.inflight.contains(&k) {
                break;
            }
            s = cv
                .wait_timeout(s, std::time::Duration::from_millis(50))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
            req.check_cancel()?;
        }
        s.inflight.insert(k);
        s.dir.clone()
    };
    let _flight = Inflight(k);
    let p = if let Some(p) = disk_read(dir.as_deref(), k) {
        p
    } else {
        let p = compute(raw, r, geo, req, k.1)?;
        disk_write(dir.as_deref(), k, &p);
        if let Some(dir) = &dir {
            prune(dir, DISK_BYTES);
        }
        locked().computed += 1;
        p
    };
    let p = Arc::new(p);
    locked().put_patch(k, Arc::clone(&p));
    Ok(p)
}

// ------------------------------------------------------------------ the fill ---

/// One stroke segment in sensor pixels (radius linear along it).
#[derive(Debug, Clone, Copy)]
struct Capsule {
    a: [f64; 2],
    b: [f64; 2],
    ra: f64,
    rb: f64,
    erase: bool,
}

impl Capsule {
    fn contains(&self, p: [f64; 2]) -> bool {
        let (dx, dy) = (self.b[0] - self.a[0], self.b[1] - self.a[1]);
        let l2 = dx * dx + dy * dy;
        let t = if l2 > 0.0 {
            (((p[0] - self.a[0]) * dx + (p[1] - self.a[1]) * dy) / l2).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (cx, cy) = (self.a[0] + t * dx, self.a[1] + t * dy);
        let r = self.ra + (self.rb - self.ra) * t;
        (p[0] - cx).hypot(p[1] - cy) < r
    }

    fn bbox(&self) -> [f64; 4] {
        [
            (self.a[0] - self.ra).min(self.b[0] - self.rb),
            (self.a[1] - self.ra).min(self.b[1] - self.rb),
            (self.a[0] + self.ra).max(self.b[0] + self.rb),
            (self.a[1] + self.ra).max(self.b[1] + self.rb),
        ]
    }
}

/// The strokes of a removal as sensor-space capsules.
fn capsules(strokes: &[BrushStroke], geo: &Geometry) -> Vec<Capsule> {
    let (ew, eh) = (geo.e_size[0], geo.e_size[1]);
    let long = ew.max(eh);
    let mut out = Vec::new();
    for s in strokes {
        if !(s.size.is_finite() && s.size > 0.0) {
            continue;
        }
        let r_e = 0.5 * f64::from(s.size) * long;
        let pts: Vec<([f64; 2], f64)> = s
            .points
            .iter()
            .filter(|p| p.iter().all(|v| v.is_finite()))
            .map(|p| {
                let (ex, ey) = (f64::from(p[0]) * ew, f64::from(p[1]) * eh);
                let c = geo.e_to_sensor_green(ex, ey);
                // Local scale of the optics (keystone, distortion) at this point.
                let dx = geo.e_to_sensor_green(ex + 1.0, ey);
                let dy = geo.e_to_sensor_green(ex, ey + 1.0);
                let det = ((dx.0 - c.0) * (dy.1 - c.1) - (dx.1 - c.1) * (dy.0 - c.0)).abs();
                ([c.0, c.1], r_e * det.sqrt().clamp(0.1, 10.0))
            })
            .collect();
        let segs: Vec<(usize, usize)> = if pts.len() == 1 {
            vec![(0, 0)]
        } else {
            (1..pts.len()).map(|i| (i - 1, i)).collect()
        };
        out.extend(segs.into_iter().map(|(i, j)| Capsule {
            a: pts[i].0,
            b: pts[j].0,
            ra: pts[i].1,
            rb: pts[j].1,
            erase: s.erase,
        }));
    }
    out
}

/// The removal's footprint on the sensor: coverage `alpha` (1 in the grown hole, easing
/// to 0 over the feather) over the sensor rectangle `rect`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Footprint {
    pub rect: [u32; 4],
    /// Quantized to 1/255 (a full-frame object's footprint stays ~60 MB).
    pub alpha: Vec<u8>,
}

/// Rasterise `caps` on the sensor within `area`, grown by [`DILATE`] and feathered by
/// `feather` px, keeping the part inside `clip` (one tile). Each capsule is tested over its
/// own bounding box only, and only the tile (plus the feather's reach) is rasterised.
fn footprint(caps: &[Capsule], feather: f64, area: [u32; 4], clip: [u32; 4]) -> Option<Footprint> {
    let pad = DILATE + feather + 2.0;
    let reach = |r: [u32; 4], pad: f64| {
        [
            f64::from(r[0]) - pad,
            f64::from(r[1]) - pad,
            f64::from(r[0]) + f64::from(r[2]) + pad,
            f64::from(r[1]) + f64::from(r[3]) + pad,
        ]
    };
    let near = reach(clip, pad);
    let meets = |b: [f64; 4]| b[0] < near[2] && b[2] > near[0] && b[1] < near[3] && b[3] > near[1];
    let paint: Vec<(&Capsule, [f64; 4])> = caps
        .iter()
        .filter(|c| !c.erase)
        .map(|c| (c, c.bbox()))
        .filter(|(_, b)| meets(*b))
        .collect();
    let bb = paint.iter().map(|(_, b)| *b).reduce(|a, b| {
        [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ]
    })?;
    let ar = reach(area, 0.0);
    let x0 = (bb[0] - pad).max(near[0]).floor().max(ar[0]);
    let y0 = (bb[1] - pad).max(near[1]).floor().max(ar[1]);
    let x1 = (bb[2] + pad).min(near[2]).ceil().min(ar[2]);
    let y1 = (bb[3] + pad).min(near[3]).ceil().min(ar[3]);
    if !(x1 > x0 && y1 > y0) {
        return None;
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let erase: Vec<(&Capsule, [f64; 4])> = caps
        .iter()
        .filter(|c| c.erase)
        .map(|c| (c, c.bbox()))
        .filter(|(_, b)| meets(*b))
        .collect();
    let mut hole = vec![false; w * h];
    hole.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let py = y0 + y as f64 + 0.5;
        // Paint, then erase, each capsule over its own span of the row.
        for (set, caps) in [(true, &paint), (false, &erase)] {
            for (c, b) in caps {
                if py < b[1] || py > b[3] {
                    continue;
                }
                let xa = (b[0] - x0 - 0.5).floor().max(0.0) as usize;
                let xb = ((b[2] - x0 + 0.5).ceil().max(0.0) as usize).min(w);
                for (x, v) in row.iter_mut().enumerate().take(xb).skip(xa) {
                    if *v != set && c.contains([x0 + x as f64 + 0.5, py]) {
                        *v = set;
                    }
                }
            }
        }
    });
    if !hole.iter().any(|v| *v) {
        return None;
    }
    let dist = chamfer(&hole, w, h);
    drop(hole);
    // Only the clip's part is kept.
    let cx0 = (f64::from(clip[0]) - x0).max(0.0) as usize;
    let cy0 = (f64::from(clip[1]) - y0).max(0.0) as usize;
    let cx1 = ((f64::from(clip[0] + clip[2]) - x0).max(0.0) as usize).min(w);
    let cy1 = ((f64::from(clip[1] + clip[3]) - y0).max(0.0) as usize).min(h);
    let alpha_at = |d: f32| -> f32 {
        let d = f64::from(d);
        if d <= DILATE {
            1.0
        } else if feather <= 0.0 {
            0.0
        } else {
            let t = ((d - DILATE) / feather).clamp(0.0, 1.0);
            (1.0 - t * t * (3.0 - 2.0 * t)) as f32
        }
    };
    let alpha_at = |d: f32| -> u8 { (alpha_at(d) * 255.0).round() as u8 };
    // Shrink to what the feather actually reaches.
    let (mut bx0, mut by0, mut bx1, mut by1) = (cx1, cy1, cx0, cy0);
    for y in cy0..cy1 {
        for x in cx0..cx1 {
            if alpha_at(dist[y * w + x]) > 0 {
                bx0 = bx0.min(x);
                by0 = by0.min(y);
                bx1 = bx1.max(x + 1);
                by1 = by1.max(y + 1);
            }
        }
    }
    if bx1 <= bx0 || by1 <= by0 {
        return None;
    }
    let (rw, rh) = (bx1 - bx0, by1 - by0);
    let mut a = Vec::with_capacity(rw * rh);
    for y in by0..by1 {
        a.extend(dist[y * w + bx0..y * w + bx1].iter().map(|&d| alpha_at(d)));
    }
    Some(Footprint {
        rect: [
            x0 as u32 + bx0 as u32,
            y0 as u32 + by0 as u32,
            rw as u32,
            rh as u32,
        ],
        alpha: a,
    })
}

/// [`footprint`] tile by tile ([`TILE`] px, row-major): only tiles the strokes reach are
/// rasterised, each with the memory of one tile.
fn footprints(caps: &[Capsule], feather: f64, area: [u32; 4]) -> Vec<Footprint> {
    let pad = DILATE + feather + 2.0;
    let Some(bb) = caps
        .iter()
        .filter(|c| !c.erase)
        .map(Capsule::bbox)
        .reduce(|a, b| {
            [
                a[0].min(b[0]),
                a[1].min(b[1]),
                a[2].max(b[2]),
                a[3].max(b[3]),
            ]
        })
    else {
        return Vec::new();
    };
    let clamp = |v: f64, lo: u32, len: u32| v.clamp(f64::from(lo), f64::from(lo + len)) as u32;
    let (x0, x1) = (
        clamp((bb[0] - pad).floor(), area[0], area[2]),
        clamp((bb[2] + pad).ceil(), area[0], area[2]),
    );
    let (y0, y1) = (
        clamp((bb[1] - pad).floor(), area[1], area[3]),
        clamp((bb[3] + pad).ceil(), area[1], area[3]),
    );
    let mut tiles = Vec::new();
    for ty in (y0 / TILE..y1.div_ceil(TILE)).map(|t| t * TILE) {
        for tx in (x0 / TILE..x1.div_ceil(TILE)).map(|t| t * TILE) {
            tiles.push([tx, ty, TILE, TILE]);
        }
    }
    tiles
        .into_par_iter()
        .filter_map(|clip| footprint(caps, feather, area, clip))
        .collect()
}

/// One inpainting window: the footprints filled (and written) together, and the crop
/// the engine sees around them.
struct Window {
    crop: [u32; 4],
    fps: Vec<Footprint>,
}

/// Group a removal's tile footprints into windows, cluster by cluster (footprints touching
/// one another: an object, a stroke, a dust spot apart from them): one window around a
/// cluster that is small or thick (an object), else one per tile (a long stroke: each
/// window close to the engine's size, so it fills at about native resolution and never
/// holds the frame). Each cluster is judged on its own: dust far away doesn't make an
/// object look thin.
fn windows(fps: Vec<Footprint>, area: [u32; 4]) -> Vec<Window> {
    clusters(fps)
        .into_iter()
        .flat_map(|c| cluster_windows(c, area))
        .collect()
}

fn cluster_windows(fps: Vec<Footprint>, area: [u32; 4]) -> Vec<Window> {
    let Some(u) = fps.iter().map(|f| f.rect).reduce(union) else {
        return Vec::new();
    };
    let covered: usize = fps
        .iter()
        .map(|f| f.alpha.iter().filter(|a| **a > 0).count())
        .sum();
    let thick = covered as f64 >= RUN_THICKNESS * f64::from(u[2]).hypot(f64::from(u[3]));
    if fps.len() == 1 || u[2].max(u[3]) <= ONE_WINDOW_EDGE || thick {
        return vec![Window {
            crop: crop_rect(u, area, object_margin(u)),
            fps,
        }];
    }
    fps.into_iter()
        .map(|f| Window {
            crop: crop_rect(f.rect, area, TILE_CONTEXT),
            fps: vec![f],
        })
        .collect()
}

/// The footprints in groups whose rectangles touch (directly or through others), in the
/// order of each group's first footprint.
fn clusters(fps: Vec<Footprint>) -> Vec<Vec<Footprint>> {
    fn find(root: &mut [usize], mut i: usize) -> usize {
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    let n = fps.len();
    let mut root: Vec<usize> = (0..n).collect();
    let grown = |r: [u32; 4]| {
        [
            r[0].saturating_sub(1),
            r[1].saturating_sub(1),
            r[2] + 2,
            r[3] + 2,
        ]
    };
    for i in 0..n {
        for j in i + 1..n {
            if overlaps(grown(fps[i].rect), fps[j].rect) {
                let (a, b) = (find(&mut root, i), find(&mut root, j));
                root[a.max(b)] = a.min(b);
            }
        }
    }
    let mut out: Vec<Vec<Footprint>> = Vec::new();
    let mut slot = vec![usize::MAX; n];
    for (i, f) in fps.into_iter().enumerate() {
        let r = find(&mut root, i);
        if slot[r] == usize::MAX {
            slot[r] = out.len();
            out.push(Vec::new());
        }
        out[slot[r]].push(f);
    }
    out
}

fn union(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    let (x0, y0) = (a[0].min(b[0]), a[1].min(b[1]));
    let (x1, y1) = (
        (a[0] + a[2]).max(b[0] + b[2]),
        (a[1] + a[3]).max(b[1] + b[3]),
    );
    [x0, y0, x1 - x0, y1 - y0]
}

fn overlaps(a: [u32; 4], b: [u32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// Approximate Euclidean distance (px) to the nearest `true` pixel: two-pass 3-4 chamfer.
fn chamfer(set: &[bool], w: usize, h: usize) -> Vec<f32> {
    const BIG: f32 = 1e9;
    let mut d: Vec<f32> = set.iter().map(|&v| if v { 0.0 } else { BIG }).collect();
    let (a, b) = (1.0f32, std::f32::consts::SQRT_2);
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let mut v = d[i];
            if x > 0 {
                v = v.min(d[i - 1] + a);
            }
            if y > 0 {
                v = v.min(d[i - w] + a);
                if x > 0 {
                    v = v.min(d[i - w - 1] + b);
                }
                if x + 1 < w {
                    v = v.min(d[i - w + 1] + b);
                }
            }
            d[i] = v;
        }
    }
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let i = y * w + x;
            let mut v = d[i];
            if x + 1 < w {
                v = v.min(d[i + 1] + a);
            }
            if y + 1 < h {
                v = v.min(d[i + w] + a);
                if x + 1 < w {
                    v = v.min(d[i + w + 1] + b);
                }
                if x > 0 {
                    v = v.min(d[i + w - 1] + b);
                }
            }
            d[i] = v;
        }
    }
    d
}

/// Context around an object's footprint `fp`: half its size, at least [`MIN_CONTEXT`].
fn object_margin(fp: [u32; 4]) -> u32 {
    MIN_CONTEXT.max(fp[2].max(fp[3]) / 2)
}

/// The inpainting crop around `fp`: the footprint plus `margin` of context on every side,
/// as square as the image area allows (models see undistorted content).
fn crop_rect(fp: [u32; 4], area: [u32; 4], margin: u32) -> [u32; 4] {
    let (fw, fh) = (fp[2], fp[3]);
    let side = fw.max(fh) + 2 * margin;
    let fit = |c: i64, len: u32, a0: u32, alen: u32| -> (u32, u32) {
        let len = len.min(alen);
        let start = (c - i64::from(len) / 2).clamp(i64::from(a0), i64::from(a0 + alen - len));
        (start as u32, len)
    };
    let (x, w) = fit(i64::from(fp[0]) + i64::from(fw) / 2, side, area[0], area[2]);
    let (y, h) = fit(i64::from(fp[1]) + i64::from(fh) / 2, side, area[1], area[3]);
    [x, y, w, h]
}

/// Display encoding the inpainter sees: camera (BASE) → working via the camera matrix,
/// desaturated toward luma by `s` so (nearly) every colour of the crop is in gamut, scaled
/// so the crop's bright end sits at 0.85, highlights above [`KNEE`] rolled off (not
/// clipped), sRGB-encoded; and back. Every step is invertible, so a fill of bright or
/// saturated surroundings decodes as bright and saturated as them.
#[derive(Debug, Clone, Copy)]
struct Tone {
    m: crate::color::Mat3,
    inv: crate::color::Mat3,
    k: f32,
    s: f32,
}

/// Where the highlight roll-off starts (encoded linear, 0..1).
const KNEE: f32 = 0.8;

/// Highlight roll-off: identity to [`KNEE`], then approaching 1 (never reaching it).
fn knee(v: f32) -> f32 {
    if v <= KNEE {
        v
    } else {
        KNEE + (1.0 - KNEE) * (1.0 - (-(v - KNEE) / (1.0 - KNEE)).exp())
    }
}

/// Inverse of [`knee`] (values at 1 decode to ~3× the knee's range: the most a fill can
/// brighten to).
fn unknee(e: f32) -> f32 {
    if e <= KNEE {
        e
    } else {
        let t = ((e - KNEE) / (1.0 - KNEE)).min(0.9999);
        KNEE - (1.0 - KNEE) * (1.0 - t).ln()
    }
}

impl Tone {
    fn new(raw: &RawImage, base: &[[f32; 3]]) -> Self {
        let m = crate::plan::cam_matrix(raw);
        let inv = mat_inv(&m).unwrap_or(crate::color::diag([1.0; 3]));
        let mut ys = Vec::new();
        // Per out-of-gamut sample: the desaturation that brings it to the gamut's edge.
        let mut needs = Vec::new();
        for b in base.iter().step_by(7) {
            let w = mat_vec(&m, *b);
            let y = crate::color_edit::luma(w);
            if !y.is_finite() {
                continue;
            }
            ys.push(y);
            let lo = w[0].min(w[1]).min(w[2]);
            if lo < 0.0 && y > 1e-6 {
                needs.push(y / (y - lo));
            }
        }
        let hi = if ys.is_empty() {
            1.0
        } else {
            let n = (ys.len() * 99 / 100).min(ys.len() - 1);
            *ys.select_nth_unstable_by(n, f32::total_cmp).1
        };
        // The strictest need but the 1% most extreme samples (bounded: a crop of
        // out-of-gamut noise must not wash the model's view out).
        let s = if needs.is_empty() {
            1.0
        } else {
            let n = needs.len() / 100;
            (*needs.select_nth_unstable_by(n, f32::total_cmp).1).clamp(0.25, 1.0)
        };
        Self {
            m,
            inv,
            k: (0.85 / hi.max(1e-4)).clamp(0.05, 50.0),
            s,
        }
    }

    fn encode(&self, b: [f32; 3]) -> [f32; 3] {
        let w = mat_vec(&self.m, b);
        let y = crate::color_edit::luma(w);
        w.map(|v| srgb_encode(knee(((y + self.s * (v - y)) * self.k).max(0.0))))
    }

    fn decode(&self, e: [f32; 3]) -> [f32; 3] {
        let lin = e.map(|v| unknee(srgb_decode(v.clamp(0.0, 1.0))) / self.k);
        let y = crate::color_edit::luma(lin);
        let w = lin.map(|v| y + (v - y) / self.s);
        mat_vec(&self.inv, w).map(|v| v.max(0.0))
    }
}

/// Area (box) resample of a `sw × sh` image to `dw × dh`.
fn resize_area(src: &[[f32; 3]], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<[f32; 3]> {
    let (fx, fy) = (sw as f64 / dw as f64, sh as f64 / dh as f64);
    let mut out = vec![[0.0f32; 3]; dw * dh];
    out.par_chunks_mut(dw).enumerate().for_each(|(y, row)| {
        let (sy0, sy1) = (y as f64 * fy, (y + 1) as f64 * fy);
        for (x, o) in row.iter_mut().enumerate() {
            let (sx0, sx1) = (x as f64 * fx, (x + 1) as f64 * fx);
            let mut acc = [0.0f64; 3];
            let mut wsum = 0.0f64;
            let mut yy = sy0.floor() as usize;
            while (yy as f64) < sy1 && yy < sh {
                let wy = (sy1.min(yy as f64 + 1.0) - sy0.max(yy as f64)).max(0.0);
                let mut xx = sx0.floor() as usize;
                while (xx as f64) < sx1 && xx < sw {
                    let wx = (sx1.min(xx as f64 + 1.0) - sx0.max(xx as f64)).max(0.0);
                    let wt = wx * wy;
                    let s = src[yy * sw + xx];
                    for c in 0..3 {
                        acc[c] += f64::from(s[c]) * wt;
                    }
                    wsum += wt;
                    xx += 1;
                }
                yy += 1;
            }
            *o = acc.map(|v| (v / wsum.max(1e-12)) as f32);
        }
    });
    out
}

/// Resample of the mask `src` (`sw × sh`) to `dw × dh`: set where any source pixel under
/// the destination pixel is.
fn resize_any(src: &[bool], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<bool> {
    let (fx, fy) = (sw as f64 / dw as f64, sh as f64 / dh as f64);
    let span = |i: usize, f: f64, n: usize| {
        let (a, b) = (i as f64 * f, (i + 1) as f64 * f);
        (
            a.floor() as usize,
            (b.ceil() as usize).clamp(a.floor() as usize + 1, n),
        )
    };
    let mut out = vec![false; dw * dh];
    out.par_chunks_mut(dw).enumerate().for_each(|(y, row)| {
        let (y0, y1) = span(y, fy, sh);
        for (x, o) in row.iter_mut().enumerate() {
            let (x0, x1) = span(x, fx, sw);
            *o = (y0..y1).any(|yy| src[yy * sw + x0..yy * sw + x1].contains(&true));
        }
    });
    out
}

/// Upright view of a sensor crop: `omap[oriented index] = crop index` for the E-frame
/// orientation `e_to_sensor` (a signed permutation plus a translation).
fn orient_map(e_to_sensor: &geometry::Affine, crop: [u32; 4]) -> (usize, usize, Vec<usize>) {
    let inv = e_to_sensor.inverse().unwrap_or(geometry::Affine::IDENTITY);
    let (cx, cy, cw, ch) = (
        f64::from(crop[0]),
        f64::from(crop[1]),
        f64::from(crop[2]),
        f64::from(crop[3]),
    );
    let corners =
        [(cx, cy), (cx + cw, cy), (cx, cy + ch), (cx + cw, cy + ch)].map(|(x, y)| inv.apply(x, y));
    let ex0 = corners.iter().map(|c| c.0).fold(f64::MAX, f64::min);
    let ey0 = corners.iter().map(|c| c.1).fold(f64::MAX, f64::min);
    let ex1 = corners.iter().map(|c| c.0).fold(f64::MIN, f64::max);
    let ey1 = corners.iter().map(|c| c.1).fold(f64::MIN, f64::max);
    let (ow, oh) = ((ex1 - ex0).round() as usize, (ey1 - ey0).round() as usize);
    let (w, h) = (crop[2] as usize, crop[3] as usize);
    let mut map = vec![0usize; ow * oh];
    for oy in 0..oh {
        for ox in 0..ow {
            let (sx, sy) = e_to_sensor.apply(ex0 + ox as f64 + 0.5, ey0 + oy as f64 + 0.5);
            let x = ((sx - cx).floor().max(0.0) as usize).min(w - 1);
            let y = ((sy - cy).floor().max(0.0) as usize).min(h - 1);
            map[oy * ow + ox] = y * w + x;
        }
    }
    (ow, oh, map)
}

/// Sensor noise model per colour channel in normalized RAW units: `σ² = a·v + b`,
/// fitted on the known samples of `crop` (16 px blocks clear of the `avoid` rectangles,
/// the footprints): per block and channel the robust spread of same-colour neighbour
/// differences.
fn noise_model(raw: &RawImage, crop: [u32; 4], avoid: &[[u32; 4]]) -> [(f32, f32); 3] {
    const B: u32 = 16;
    let sw = raw.size.width as usize;
    let linear = raw.cfa == CfaPattern::LinearRgb;
    let si = crate::demosaic::site_info(raw);
    // (value, colour) of a sample; linear RGB reads channel `c` of the pixel.
    let sample = |x: usize, y: usize, c: usize| -> (f32, usize) {
        if linear {
            (
                (f32::from(raw.data[(y * sw + x) * 3 + c]) - raw.black_level[c])
                    / (raw.white_level - raw.black_level[c]).max(1.0),
                c,
            )
        } else {
            let pos = ((y & 1) << 1) | (x & 1);
            (
                (f32::from(raw.data[y * sw + x]) - si.black[pos]) * si.inv_range[pos],
                si.color[pos],
            )
        }
    };
    let touches = |bx: u32, by: u32| {
        avoid.iter().any(|&[fx, fy, fw, fh]| {
            bx < fx + fw + 2 && bx + B + 2 > fx && by < fy + fh + 2 && by + B + 2 > fy
        })
    };
    let (step, per_px) = if linear { (1, 3) } else { (2, 1) };
    let mut pts: [Vec<(f32, f32)>; 3] = Default::default();
    let mut by = crop[1];
    while by + B <= crop[1] + crop[3] {
        let mut bx = crop[0];
        while bx + B + step as u32 <= crop[0] + crop[2] {
            if !touches(bx, by) {
                let mut d: [Vec<f32>; 3] = Default::default();
                let mut sum = [0.0f32; 3];
                for y in by..by + B {
                    for x in bx..bx + B {
                        let (x, y) = (x as usize, y as usize);
                        for k in 0..per_px {
                            let (a, c) = sample(x, y, k);
                            let (b, _) = sample(x + step, y, k);
                            d[c].push((a - b).abs());
                            sum[c] += a;
                        }
                    }
                }
                for c in 0..3 {
                    let n = d[c].len();
                    if n < 16 {
                        continue;
                    }
                    let mean = sum[c] / n as f32;
                    if mean > 0.0 && mean < 0.9 {
                        let med = *d[c].select_nth_unstable_by(n / 2, f32::total_cmp).1;
                        let sd = 1.4826 * med / std::f32::consts::SQRT_2;
                        pts[c].push((mean, sd * sd));
                    }
                }
            }
            bx += B;
        }
        by += B;
    }
    pts.map(fit_noise)
}

/// Least-squares line `σ² = a·v + b` through the quietest quarter of 8 signal bins.
fn fit_noise(mut pts: Vec<(f32, f32)>) -> (f32, f32) {
    if pts.len() < 4 {
        return (
            0.0,
            pts.iter()
                .map(|p| p.1)
                .fold(f32::MAX, f32::min)
                .clamp(0.0, 1e-4),
        );
    }
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let bins = 8.min(pts.len() / 2).max(1);
    let per = pts.len() / bins;
    let mut xs = Vec::new();
    for k in 0..bins {
        let end = if k + 1 == bins {
            pts.len()
        } else {
            (k + 1) * per
        };
        let chunk = &pts[k * per..end];
        let mean_v = chunk.iter().map(|p| p.0).sum::<f32>() / chunk.len() as f32;
        let q = chunk.len() / 4;
        let mut s2: Vec<f32> = chunk.iter().map(|p| p.1).collect();
        let v = *s2.select_nth_unstable_by(q, f32::total_cmp).1;
        xs.push((mean_v, v));
    }
    let n = xs.len() as f32;
    let (lo, hi) = (xs[0].0, xs[xs.len() - 1].0);
    if hi - lo < 0.5 * f32::midpoint(lo, hi) {
        // Too narrow a signal range for a slope: a constant level.
        let mut s2: Vec<f32> = xs.iter().map(|p| p.1).collect();
        let m = s2.len() / 2;
        return (0.0, *s2.select_nth_unstable_by(m, f32::total_cmp).1);
    }
    let (mx, my) = (
        xs.iter().map(|p| p.0).sum::<f32>() / n,
        xs.iter().map(|p| p.1).sum::<f32>() / n,
    );
    let sxx: f32 = xs.iter().map(|p| (p.0 - mx) * (p.0 - mx)).sum();
    let sxy: f32 = xs.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let a = if sxx > 1e-9 {
        (sxy / sxx).max(0.0)
    } else {
        0.0
    };
    let b = (my - a * mx).max(0.0);
    (a, b)
}

/// What the inpainter is given.
struct Prepared<'a> {
    crop: [u32; 4],
    /// Upright crop size and its map to crop indices.
    ow: usize,
    oh: usize,
    omap: Vec<usize>,
    base: Vec<[f32; 3]>,
    tone: Tone,
    /// The footprints filled and written.
    fps: &'a [Footprint],
    /// Rectangles of every footprint in the crop (these and later windows' ones).
    avoid: Vec<[u32; 4]>,
    /// Upright unknown pixels: coverage (alpha > 0) of the window's footprints and of the
    /// later windows' in the crop (not filled yet: the engine must not copy the object).
    hole: Vec<bool>,
}

/// Stage 1 (parallel): crop, demosaic, tone, orientation, hole.
fn prepare<'a>(
    raw: &RawImage,
    win: &'a Window,
    pending: &[&Footprint],
    geo: &Geometry,
) -> Prepared<'a> {
    let img = crate::demosaic::rcd_rect(raw, &crate::plan::base_params(raw), win.crop);
    let crop = [img.x0, img.y0, img.w, img.h];
    let tone = Tone::new(raw, &img.px);
    let (ow, oh, omap) = orient_map(&geo.e_to_sensor, crop);
    let cw = crop[2] as usize;
    let unknown: Vec<&Footprint> = win
        .fps
        .iter()
        .chain(pending.iter().copied())
        .filter(|f| overlaps(f.rect, crop))
        .collect();
    let hole = omap
        .par_iter()
        .map(|&ci| {
            let (x, y) = ((ci % cw) as u32 + crop[0], (ci / cw) as u32 + crop[1]);
            unknown.iter().any(|f| fp_alpha(f, x, y) > 0.0)
        })
        .collect();
    Prepared {
        crop,
        ow,
        oh,
        omap,
        base: img.px,
        tone,
        fps: &win.fps,
        avoid: unknown.iter().map(|f| f.rect).collect(),
        hole,
    }
}

fn fp_alpha(fp: &Footprint, x: u32, y: u32) -> f32 {
    let [fx, fy, fw, fh] = fp.rect;
    if x < fx || y < fy || x >= fx + fw || y >= fy + fh {
        return 0.0;
    }
    f32::from(fp.alpha[((y - fy) * fw + (x - fx)) as usize]) / 255.0
}

/// The model's (or `PatchMatch`'s) fill at `(mw, mh)`, display-encoded, upright.
struct Fill {
    w: usize,
    h: usize,
    px: Vec<[f32; 3]>,
}

/// Downscale the upright crop for an engine working at `edge`.
fn engine_input(p: &Prepared, edge: u32) -> (usize, usize, Vec<[f32; 3]>, Vec<bool>) {
    let enc: Vec<[f32; 3]> = p
        .omap
        .par_iter()
        .map(|&ci| p.tone.encode(p.base[ci]))
        .collect();
    let s = (f64::from(edge.max(16)) / p.ow.max(p.oh) as f64).min(1.0);
    let (mw, mh) = (
        ((p.ow as f64 * s).round() as usize).max(1),
        ((p.oh as f64 * s).round() as usize).max(1),
    );
    if (mw, mh) == (p.ow, p.oh) {
        return (mw, mh, enc, p.hole.clone());
    }
    let img = resize_area(&enc, p.ow, p.oh, mw, mh);
    drop(enc);
    let hole = resize_any(&p.hole, p.ow, p.oh, mw, mh);
    (mw, mh, img, hole)
}

/// Stage 2 (caller thread for the model, pipeline pool for `PatchMatch`).
fn inpaint(p: &Prepared, req: &RenderRequest, seed: u64) -> Result<Fill, PipelineError> {
    if let Some(model) = &req.inpainter {
        let edge = model.max_edge();
        let (mw, mh, img, hole) = crate::par::run_for(req.priority, || engine_input(p, edge));
        let rgb: Vec<u8> = img
            .iter()
            .flat_map(|v| v.map(|c| (c * 255.0 + 0.5) as u8))
            .collect();
        let mask: Vec<u8> = hole.iter().map(|&v| if v { 255 } else { 0 }).collect();
        if let Some(out) = model.inpaint(mw as u32, mh as u32, &rgb, &mask)
            && out.len() == mw * mh * 3
        {
            let px: Vec<[f32; 3]> = out
                .as_chunks::<3>()
                .0
                .iter()
                .zip(&img)
                .zip(&hole)
                .map(|((o, known), &h)| {
                    if h {
                        [0, 1, 2].map(|c| f32::from(o[c]) / 255.0)
                    } else {
                        *known
                    }
                })
                .collect();
            return Ok(Fill { w: mw, h: mh, px });
        }
    }
    let cancel = req.cancel.clone();
    let (mw, mh, img, hole) =
        crate::par::run_for(req.priority, || engine_input(p, PATCHMATCH_EDGE));
    let px = crate::par::run_for(req.priority, || {
        patchmatch(&img, &hole, mw, mh, seed, &|| {
            cancel
                .as_ref()
                .is_some_and(crate::CancelToken::is_cancelled)
        })
    });
    if let Some(px) = px {
        return Ok(Fill { w: mw, h: mh, px });
    }
    req.check_cancel()?;
    // Nothing known to copy from (a hole covering the whole crop): leave as is.
    Ok(Fill {
        w: mw,
        h: mh,
        px: img,
    })
}

/// Luminance of BASE values.
#[inline]
fn y_of(b: [f32; 3]) -> f32 {
    0.25 * b[0] + 0.5 * b[1] + 0.25 * b[2]
}

/// Separable Gaussian blur of a plane.
fn blur(p: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let r = (3.0 * sigma).ceil() as i32;
    let k: Vec<f32> = (-r..=r)
        .map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let ks: f32 = k.iter().sum();
    let k: Vec<f32> = k.iter().map(|v| v / ks).collect();
    let mut tmp = vec![0.0f32; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (j, kv) in k.iter().enumerate() {
                let xx = (x as i32 + j as i32 - r).clamp(0, w as i32 - 1) as usize;
                acc += kv * p[y * w + xx];
            }
            *o = acc;
        }
    });
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (j, kv) in k.iter().enumerate() {
                let yy = (y as i32 + j as i32 - r).clamp(0, h as i32 - 1) as usize;
                acc += kv * tmp[yy * w + x];
            }
            *o = acc;
        }
    });
    out
}

/// Edge of the texture blocks the detail transfer copies, px.
const DETAIL_BLOCK: usize = 32;
/// Most blocks in a big window's donor pool (4 KB each).
const DONORS: usize = 512;

/// The detail transfer's planes of `base` (`w × h`): the relative luminance band between
/// ~1 px and the model's pixel size (`scale` px), and the coarse luminance it is relative to.
fn detail_planes(base: &[[f32; 3]], w: usize, h: usize, scale: f32) -> (Vec<f32>, Vec<f32>) {
    let y: Vec<f32> = base.iter().map(|b| y_of(*b)).collect();
    let fine = blur(&y, w, h, 0.7);
    let coarse = blur(&y, w, h, coarse_sigma(scale));
    // Relative band: (fine − coarse) / coarse.
    let band = fine
        .iter()
        .zip(&coarse)
        .map(|(f, c)| if *c > 1e-5 { (f - c) / c } else { 0.0 })
        .collect();
    (band, coarse)
}

fn coarse_sigma(scale: f32) -> f32 {
    (0.6 * scale).max(1.5)
}

/// Texture blocks the detail transfer copies: [`DETAIL_BLOCK`] px blocks of a `w`-wide
/// relative-band plane, at `(x, y)`, with their mean coarse luminance.
struct Donors {
    band: Vec<f32>,
    w: usize,
    blocks: Vec<(usize, usize, f32)>,
}

impl Donors {
    /// The known blocks (no hole pixel) of `base`, overlapping by half.
    fn known(base: &[[f32; 3]], hole: &[bool], w: usize, h: usize, scale: f32) -> Self {
        const B: usize = DETAIL_BLOCK;
        let (band, coarse) = detail_planes(base, w, h, scale);
        let mut blocks = Vec::new();
        for by in (0..=h - B).step_by(B / 2) {
            for bx in (0..=w - B).step_by(B / 2) {
                let mut clear = true;
                let mut m = 0.0f32;
                'b: for yy in by..by + B {
                    for xx in bx..bx + B {
                        if hole[yy * w + xx] {
                            clear = false;
                            break 'b;
                        }
                        m += coarse[yy * w + xx];
                    }
                }
                if clear {
                    blocks.push((bx, by, m / (B * B) as f32));
                }
            }
        }
        Self { band, w, blocks }
    }

    /// Up to [`DONORS`] known blocks spread over the whole `crop` (none of `unknown`'s
    /// coverage), each demosaiced on its own with the context its blurs need: a big
    /// window's pool, shared by all its tiles (inner ones have no known blocks nearby). None
    /// when the model worked at about full resolution.
    fn sampled(
        raw: &RawImage,
        crop: [u32; 4],
        unknown: &[&Footprint],
        scale: f32,
        seed: u64,
    ) -> Self {
        const B: usize = DETAIL_BLOCK;
        let b = B as u32;
        let mut sites = Vec::new();
        if scale >= 1.5 && crop[2] >= b && crop[3] >= b {
            let step = ((f64::from(crop[2]) * f64::from(crop[3]) / (4 * DONORS) as f64).sqrt()
                as u32)
                .max(b);
            for y in (crop[1]..=crop[1] + crop[3] - b).step_by(step as usize) {
                for x in (crop[0]..=crop[0] + crop[2] - b).step_by(step as usize) {
                    let r = [x, y, b, b];
                    let clear = unknown.iter().filter(|f| overlaps(f.rect, r)).all(|f| {
                        (y..y + b).all(|yy| (x..x + b).all(|xx| fp_alpha(f, xx, yy) == 0.0))
                    });
                    if clear {
                        sites.push((x, y));
                    }
                }
            }
        }
        // An even random subset when there are more.
        let mut rng = Rng::new(seed);
        for i in 0..sites.len().min(DONORS) {
            let j = i + rng.below(sites.len() - i);
            sites.swap(i, j);
        }
        sites.truncate(DONORS);
        let params = crate::plan::base_params(raw);
        let m = (3.0 * coarse_sigma(scale)).ceil() as u32 + 2;
        let picked: Vec<(Vec<f32>, f32)> = sites
            .par_iter()
            .map(|&(x, y)| {
                let x0 = x.saturating_sub(m);
                let y0 = y.saturating_sub(m);
                let img = crate::demosaic::rcd_rect(raw, &params, [x0, y0, b + 2 * m, b + 2 * m]);
                let (w, h) = (img.w as usize, img.h as usize);
                let (band, coarse) = detail_planes(&img.px, w, h, scale);
                let (ox, oy) = ((x - img.x0) as usize, (y - img.y0) as usize);
                let mut out = Vec::with_capacity(B * B);
                let mut mean = 0.0f32;
                for yy in oy..oy + B {
                    out.extend_from_slice(&band[yy * w + ox..yy * w + ox + B]);
                    mean += coarse[yy * w + ox..yy * w + ox + B].iter().sum::<f32>();
                }
                (out, mean / (B * B) as f32)
            })
            .collect();
        // Stacked in one plane, block i at (0, i·B).
        let blocks = (0..picked.len()).map(|i| (0, i * B, picked[i].1)).collect();
        Self {
            band: picked.into_iter().flat_map(|p| p.0).collect(),
            w: B,
            blocks,
        }
    }
}

/// Texture transfer: the luminance band between ~1 px and the model's pixel size, copied
/// in 32 px blocks (overlapping by half, raised-cosine blended) from known blocks of
/// similar tone, applied as a relative gain to the upscaled fill. Grain (finer) comes
/// from the re-mosaic.
fn transfer_detail(
    fill: &mut [[f32; 3]],
    base: &[[f32; 3]],
    hole: &[bool],
    w: usize,
    h: usize,
    scale: f32,
    seed: u64,
) {
    if scale < 1.5 || w < 2 * DETAIL_BLOCK || h < 2 * DETAIL_BLOCK {
        return;
    }
    apply_detail(
        fill,
        hole,
        w,
        h,
        &Donors::known(base, hole, w, h, scale),
        seed,
    );
}

/// The detail transfer's second half: each block of the fill touching the hole gets the
/// band of the best of a few random donors by tone.
fn apply_detail(fill: &mut [[f32; 3]], hole: &[bool], w: usize, h: usize, d: &Donors, seed: u64) {
    const B: usize = DETAIL_BLOCK;
    const S: usize = B / 2;
    if d.blocks.is_empty() || w < B || h < B {
        return;
    }
    let blocks = &d.blocks;
    let fy: Vec<f32> = fill.iter().map(|b| y_of(*b)).collect();
    let mut acc = vec![0.0f32; w * h];
    let mut wsum = vec![0.0f32; w * h];
    let win: Vec<f32> = (0..B)
        .map(|i| {
            let t = (i as f32 + 0.5) / B as f32;
            (std::f32::consts::PI * t).sin().powi(2)
        })
        .collect();
    let mut rng = Rng::new(seed);
    let mut ty = 0usize;
    while ty + B <= h + S {
        let ty0 = ty.min(h - B);
        let mut tx = 0usize;
        while tx + B <= w + S {
            let tx0 = tx.min(w - B);
            let touches = (ty0..ty0 + B).any(|yy| (tx0..tx0 + B).any(|xx| hole[yy * w + xx]));
            if touches {
                let mut m = 0.0f32;
                for yy in ty0..ty0 + B {
                    for xx in tx0..tx0 + B {
                        m += fy[yy * w + xx];
                    }
                }
                let m = m / (B * B) as f32;
                // Best of a few random candidates by tone.
                let mut best = blocks[rng.below(blocks.len())];
                for _ in 0..12 {
                    let c = blocks[rng.below(blocks.len())];
                    if (c.2 - m).abs() < (best.2 - m).abs() {
                        best = c;
                    }
                }
                for j in 0..B {
                    for i in 0..B {
                        let wt = win[i] * win[j];
                        let t = (ty0 + j) * w + tx0 + i;
                        acc[t] += wt * d.band[(best.1 + j) * d.w + best.0 + i];
                        wsum[t] += wt;
                    }
                }
            }
            tx += S;
        }
        ty += S;
    }
    for i in 0..w * h {
        if hole[i] && wsum[i] > 0.0 {
            let g = (1.0 + acc[i] / wsum[i]).clamp(0.5, 2.0);
            fill[i] = fill[i].map(|v| v * g);
        }
    }
}

/// Stage 3 (parallel): upscale, un-tone, transfer detail, re-mosaic with grain, blend.
fn finish(raw: &RawImage, p: &Prepared, f: &Fill, seed: u64) -> Vec<Patch> {
    let (ow, oh) = (p.ow, p.oh);
    let (sx, sy) = (f.w as f32 / ow as f32, f.h as f32 / oh as f32);
    // Upright fill at crop resolution, back in BASE units.
    let mut up: Vec<[f32; 3]> = (0..ow * oh)
        .into_par_iter()
        .map(|i| {
            let (x, y) = ((i % ow) as f32 + 0.5, (i / ow) as f32 + 0.5);
            p.tone.decode(bilinear(&f.px, f.w, f.h, x * sx, y * sy))
        })
        .collect();
    // The engine's own pixel noise (PatchMatch copies grain, the model invents some) is
    // smoothed away: texture comes back from the detail transfer, grain from the
    // re-mosaic, both at the camera's level. One plane at a time.
    for c in 0..3 {
        let plane = blur(
            &up.iter().map(|v| v[c]).collect::<Vec<_>>(),
            ow,
            oh,
            GRAIN_BLUR,
        );
        for (v, b) in up.iter_mut().zip(plane) {
            v[c] = b;
        }
    }
    let obase: Vec<[f32; 3]> = p.omap.iter().map(|&ci| p.base[ci]).collect();
    transfer_detail(&mut up, &obase, &p.hole, ow, oh, 1.0 / sx.min(sy), seed);
    drop(obase);
    // Crop index → upright index.
    let cw = p.crop[2] as usize;
    let mut cmap = vec![0usize; p.omap.len()];
    for (oi, &ci) in p.omap.iter().enumerate() {
        cmap[ci] = oi;
    }
    let grain = Grain::new(raw, p.crop, &p.avoid, seed);
    p.fps
        .iter()
        .map(|fp| {
            grain.remosaic(raw, fp, |x, y| {
                up[cmap[(y - p.crop[1] as usize) * cw + (x - p.crop[0] as usize)]]
            })
        })
        .collect()
}

/// What re-mosaicing a fill into the RAW needs: the crop's noise model, the frame's cap
/// on it, the white balance and CFA sites.
struct Grain {
    model: [(f32, f32); 3],
    cap: f32,
    mults: [f32; 3],
    si: crate::demosaic::SiteInfo,
    seed: u64,
}

impl Grain {
    fn new(raw: &RawImage, crop: [u32; 4], avoid: &[[u32; 4]], seed: u64) -> Self {
        // The whole frame's measured shot noise (`raw_noise`, robust to texture) caps the
        // local fit, which busy texture (foliage) inflates: σ² ≤ 4·v·σ√² (+ a read floor).
        let s = crate::plan::raw_noise(raw);
        Self {
            model: noise_model(raw, crop, avoid),
            cap: 4.0 * s * s,
            mults: crate::plan::base_params(raw).mults,
            si: crate::demosaic::site_info(raw),
            seed,
        }
    }

    /// The patch of `fp`: the fill (BASE units at sensor pixel `(x, y)`) re-mosaiced with
    /// synthetic sensor noise, blended over the feather.
    fn remosaic(
        &self,
        raw: &RawImage,
        fp: &Footprint,
        fill_at: impl Fn(usize, usize) -> [f32; 3] + Sync,
    ) -> Patch {
        let linear = raw.cfa == CfaPattern::LinearRgb;
        let chans = if linear { 3 } else { 1 };
        let sw = raw.size.width as usize;
        let [fx, fy, fw, fh] = fp.rect;
        let mut data = vec![0u16; (fw * fh) as usize * chans];
        data.par_chunks_mut(fw as usize * chans)
            .enumerate()
            .for_each(|(ry, row)| {
                let y = fy as usize + ry;
                let mut rng = Rng::new(self.seed ^ (y as u64).wrapping_mul(0xd6e8_feb8_6659_fd93));
                for rx in 0..fw as usize {
                    let x = fx as usize + rx;
                    let alpha = f32::from(fp.alpha[ry * fw as usize + rx]) / 255.0;
                    let fill = fill_at(x, y);
                    for c in 0..chans {
                        let (old, black, range, chan) = if linear {
                            (
                                raw.data[(y * sw + x) * 3 + c],
                                raw.black_level[c],
                                (raw.white_level - raw.black_level[c]).max(1.0),
                                c,
                            )
                        } else {
                            let pos = ((y & 1) << 1) | (x & 1);
                            (
                                raw.data[y * sw + x],
                                self.si.black[pos],
                                1.0 / self.si.inv_range[pos],
                                self.si.color[pos],
                            )
                        };
                        let v = fill[chan] / self.mults[chan].max(1e-6);
                        let noise = rng.normal();
                        let (a, b) = self.model[chan];
                        let v0 = v.max(0.0);
                        let sigma = (a * v0 + b).min(self.cap * v0 + 1e-7).max(0.0).sqrt();
                        let new = (black + (v + sigma * noise) * range).clamp(0.0, raw.white_level);
                        let mixed = f32::from(old) + (new - f32::from(old)) * alpha;
                        row[rx * chans + c] = mixed.round().clamp(0.0, 65535.0) as u16;
                    }
                }
            });
        Patch {
            rect: fp.rect,
            chans: chans as u32,
            data,
        }
    }
}

/// Box average of the crop's full-resolution demosaic over `k × k` blocks (`rw × rh`),
/// demosaiced a band of rows at a time (never the whole crop).
fn reduced_base(raw: &RawImage, crop: [u32; 4], k: usize, rw: usize, rh: usize) -> Vec<[f32; 3]> {
    let p = crate::plan::base_params(raw);
    let band = (k * 32) as u32;
    let mut out = vec![[0.0f32; 3]; rw * rh];
    let mut by = 0;
    while by < crop[3] {
        let bh = band.min(crop[3] - by);
        let img = crate::demosaic::rcd_rect(raw, &p, [crop[0], crop[1] + by, crop[2], bh]);
        let (iw, ih) = (img.w as usize, img.h as usize);
        let r0 = by as usize / k;
        let rows = ih.div_ceil(k).min(rh - r0);
        out[r0 * rw..(r0 + rows) * rw]
            .par_chunks_mut(rw)
            .enumerate()
            .for_each(|(ry, row)| {
                let (y0, y1) = (ry * k, ((ry + 1) * k).min(ih));
                for (rx, o) in row.iter_mut().enumerate() {
                    let (x0, x1) = (rx * k, ((rx + 1) * k).min(iw));
                    let mut acc = [0.0f32; 3];
                    for y in y0..y1 {
                        for v in &img.px[y * iw + x0..y * iw + x1] {
                            for c in 0..3 {
                                acc[c] += v[c];
                            }
                        }
                    }
                    let n = ((y1 - y0) * (x1 - x0)).max(1) as f32;
                    *o = acc.map(|a| a / n);
                }
            });
        by += bh;
    }
    out
}

/// A window too large for full resolution (see [`MAX_FULL_EDGE`]): the engine fills a
/// reduced image of it; each footprint tile is then finished at full resolution from the
/// upsampled fill (detail transfer from the window's donor pool, re-mosaic with grain).
fn fill_big(
    raw: &RawImage,
    win: &Window,
    pending: &[&Footprint],
    geo: &Geometry,
    req: &RenderRequest,
    seed: u64,
    work_edge: u32,
) -> Result<Vec<Patch>, PipelineError> {
    let crop = win.crop;
    let k = crop[2].max(crop[3]).div_ceil(work_edge).max(1) as usize;
    let (rw, rh) = (
        (crop[2] as usize).div_ceil(k),
        (crop[3] as usize).div_ceil(k),
    );
    let unknown: Vec<&Footprint> = win
        .fps
        .iter()
        .chain(pending.iter().copied())
        .filter(|f| overlaps(f.rect, crop))
        .collect();
    let prep = crate::par::run_for(req.priority, || {
        let base = reduced_base(raw, crop, k, rw, rh);
        let tone = Tone::new(raw, &base);
        // E/k → reduced crop-local: the orientation at the reduced scale.
        let kf = k as f64;
        let a = geometry::Affine::scale(kf, kf)
            .then(&geo.e_to_sensor)
            .then(&geometry::Affine::translate(
                -f64::from(crop[0]),
                -f64::from(crop[1]),
            ))
            .then(&geometry::Affine::scale(1.0 / kf, 1.0 / kf));
        let (ow, oh, omap) = orient_map(&a, [0, 0, rw as u32, rh as u32]);
        let mut cells = vec![false; rw * rh];
        for f in &unknown {
            let [fx, fy, fw, fh] = f.rect;
            for y in fy.max(crop[1])..(fy + fh).min(crop[1] + crop[3]) {
                for x in fx.max(crop[0])..(fx + fw).min(crop[0] + crop[2]) {
                    if f.alpha[((y - fy) * fw + (x - fx)) as usize] > 0 {
                        let (cx, cy) = ((x - crop[0]) as usize / k, (y - crop[1]) as usize / k);
                        cells[cy * rw + cx] = true;
                    }
                }
            }
        }
        let hole = omap.iter().map(|&ci| cells[ci]).collect();
        Prepared {
            crop,
            ow,
            oh,
            omap,
            base,
            tone,
            fps: &win.fps,
            avoid: unknown.iter().map(|f| f.rect).collect(),
            hole,
        }
    });
    let fill = inpaint(&prep, req, seed)?;
    req.check_cancel()?;
    let (ow, oh) = (prep.ow, prep.oh);
    let (sx, sy) = (fill.w as f32 / ow as f32, fill.h as f32 / oh as f32);
    // The fill back in the reduced crop's (sensor) layout, in BASE units.
    let mut reduced = vec![[0.0f32; 3]; rw * rh];
    for (oi, &ci) in prep.omap.iter().enumerate() {
        let (x, y) = ((oi % ow) as f32 + 0.5, (oi / ow) as f32 + 0.5);
        reduced[ci] = prep
            .tone
            .decode(bilinear(&fill.px, fill.w, fill.h, x * sx, y * sy));
    }
    let scale = k as f32 / sx.min(sy);
    let avoid = prep.avoid.clone();
    drop(prep);
    drop(fill);
    crate::par::run_for(req.priority, || {
        let grain = Grain::new(raw, crop, &avoid, seed);
        // One pool of texture for every tile: inner tiles of a big object have no known
        // blocks of their own, and tiles drawing on different pools would show their grid.
        let donors = Donors::sampled(raw, crop, &unknown, scale, seed);
        let kf = k as f32;
        let mut out = Vec::with_capacity(win.fps.len());
        for (i, fp) in win.fps.iter().enumerate() {
            req.check_cancel()?;
            let [fx, fy, fw, fh] = fp.rect;
            let x0 = fx.saturating_sub(FINISH_CONTEXT).max(crop[0]);
            let y0 = fy.saturating_sub(FINISH_CONTEXT).max(crop[1]);
            let x1 = (fx + fw + FINISH_CONTEXT).min(crop[0] + crop[2]);
            let y1 = (fy + fh + FINISH_CONTEXT).min(crop[1] + crop[3]);
            let (sx0, sy0, w, h) = (
                x0 as usize,
                y0 as usize,
                (x1 - x0) as usize,
                (y1 - y0) as usize,
            );
            let mut up: Vec<[f32; 3]> = (0..w * h)
                .into_par_iter()
                .map(|j| {
                    let x = (sx0 + j % w - crop[0] as usize) as f32 + 0.5;
                    let y = (sy0 + j / w - crop[1] as usize) as f32 + 0.5;
                    bilinear(&reduced, rw, rh, x / kf, y / kf)
                })
                .collect();
            let near: Vec<&&Footprint> = unknown
                .iter()
                .filter(|f| overlaps(f.rect, [x0, y0, x1 - x0, y1 - y0]))
                .collect();
            let hole: Vec<bool> = (0..w * h)
                .into_par_iter()
                .map(|j| {
                    let (x, y) = ((sx0 + j % w) as u32, (sy0 + j / w) as u32);
                    near.iter().any(|f| fp_alpha(f, x, y) > 0.0)
                })
                .collect();
            let tile_seed = seed ^ (i as u64 + 1).wrapping_mul(0xa076_1d64_78bd_642f);
            apply_detail(&mut up, &hole, w, h, &donors, tile_seed);
            out.push(grain.remosaic(raw, fp, |x, y| up[(y - sy0) * w + (x - sx0)]));
        }
        Ok(out)
    })
}

/// The patches of removal `r` on `raw` (see the module docs), applied to `raw` window by
/// window: each later window sees the earlier ones' fill as known content.
fn compute(
    raw: &mut RawImage,
    r: &Removal,
    geo: &Geometry,
    req: &RenderRequest,
    seed: u64,
) -> Result<Patches, PipelineError> {
    let area = geometry::image_area(raw);
    let caps = capsules(&r.strokes, geo);
    let long = geo.e_size[0].max(geo.e_size[1]);
    let feather = f64::from(r.feather.clamp(0.0, ds_core::adjust::MAX_REMOVAL_FEATHER)) * long;
    let fps = crate::par::run_for(req.priority, || footprints(&caps, feather, area));
    let wins = windows(fps, area);
    let mut out = Vec::new();
    for (i, win) in wins.iter().enumerate() {
        req.check_cancel()?;
        let pending: Vec<&Footprint> = wins[i + 1..]
            .iter()
            .flat_map(|w| &w.fps)
            .filter(|f| overlaps(f.rect, win.crop))
            .collect();
        let seed = seed ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let patches = if win.crop[2].max(win.crop[3]) > MAX_FULL_EDGE {
            fill_big(raw, win, &pending, geo, req, seed, WORK_EDGE)?
        } else {
            let prep = crate::par::run_for(req.priority, || prepare(raw, win, &pending, geo));
            let fill = inpaint(&prep, req, seed)?;
            req.check_cancel()?;
            crate::par::run_for(req.priority, || finish(raw, &prep, &fill, seed))
        };
        for p in &patches {
            p.apply(raw);
        }
        out.extend(patches);
    }
    Ok(Patches(out))
}

#[cfg(test)]
#[path = "removal_tests.rs"]
mod tests;
