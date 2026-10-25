//! wgpu (Metal) renderer: the same stages as the CPU reference, as WGSL compute kernels.
//!
//! - **Shared:** `GpuRenderer` is `Clone + Send + Sync`; clones share one device, one BASE
//!   cache (inside the process-wide memory budget), one set of uploaded raws and mask
//!   rasters.
//! - **Scheduling:** CPU preparation (plan, mask rasterization, RAW staging) runs outside
//!   any lock; GPU work is recorded and submitted in short *turns*, and the readback wait
//!   happens after the turn is released. Each turn goes to the highest `Priority` class
//!   waiting (interactive, tool, background, bulk); each non-interactive class has one
//!   render in flight (FIFO, the wait cancellable) and it yields between stages (BASE,
//!   then DEVELOP, per tile for large outputs). The interactive image's RAW upload is
//!   pinned. The CPU-parallel parts of a render run on the pipeline's own thread pool
//!   (`crate::par`), so renders waiting from rayon workers never deadlock their pool.
//! - **BASE** (camera RGB at the reference WB) stays resident per (image, level); every
//!   slider, white balance included, only re-runs DEVELOP. Both demosaics run on the GPU;
//!   a 1:1 region demosaics only its sensor rectangle (plus slack). Previews keep an f16
//!   BASE; `Quality::Full` and the extended-range output an f32 one.
//! - **Memory:** frame buffers (LRU-trimmed), RAW uploads, output slots and mask rasters
//!   count against the same budget as BASE levels.
//! - **Errors:** buffer sizes are checked against the device limits, every turn runs in
//!   validation + out-of-memory error scopes and under `catch_unwind`, the device is
//!   polled for loss before each turn and rebuilt after one. Nothing panics on GPU errors.
//! - **Cancellation:** `RenderRequest::cancel` is checked between stages/tiles and while
//!   waiting for the GPU; at most one stage of GPU work is left running.

mod base;
#[cfg(test)]
mod brush_tests;
mod context;
mod develop;
mod effects;
mod export;
mod gate;
mod layers;
mod params;
mod render;
#[cfg(all(test, feature = "stage-timings"))]
mod stage_timings;
mod stats;
#[cfg(test)]
mod turn_tests;
mod upload;

use std::collections::HashMap;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use ds_core::adjust::{Exposure, Hdr, Levels, WhiteBalance};
use ds_core::{Adjustments, CfaPattern, RawImage, Size};
use half::f16;

use crate::cache::{Account, BaseCache, Resident, build_rect};
use crate::cpu::{build_base, level_need};
use crate::local::GuidedParams;
use crate::mask::{self, CompiledMask, RasterCache, RasterId};
use crate::plan::{self, DemosaicMode, Plan};
use crate::{ImageKey, OutputFormat, PipelineError, Priority, RenderRequest, Renderer};

use context::{Context, LANES, OutSlot, Readback};
use develop::gpu_bpp;
use gate::{Admission, Gate, MemGate};
use params::{BaseView, INLINE_SHARPEN_TAPS, Tables};

/// A BASE level resident on the GPU.
#[derive(Debug)]
struct GpuBase {
    buf: wgpu::Buffer,
    w: u32,
    h: u32,
    x0: u32,
    y0: u32,
    precise: bool,
    /// Device generation that built it.
    epoch: u64,
}

impl Resident for GpuBase {
    fn bytes(&self) -> u64 {
        self.buf.size()
    }
    fn rect(&self) -> [u32; 4] {
        [self.x0, self.y0, self.w, self.h]
    }
}

/// A BASE being built band by band (so background builds yield the GPU and cancel
/// between bands).
struct BaseJob {
    key: ImageKey,
    bk: plan::BaseKey,
    base_params: crate::demosaic::BaseParams,
    /// Level-0 rectangle (x, y, w, h) being built into `buf`.
    rect: [u32; 4],
    buf: wgpu::Buffer,
    next_row: u32,
    band: u32,
    /// Built on the CPU already (linear-RGB input).
    linear: bool,
    /// Device generation it was started on; a job never continues on another device.
    epoch: u64,
}

impl BaseJob {
    fn done(&self) -> bool {
        self.next_row >= self.rect[3]
    }
}

/// A layer's E-frame mask rasters, packed in one GPU buffer.
#[derive(Debug)]
struct LayerRasters {
    buf: wgpu::Buffer,
    layout: Vec<RasterEntry>,
    _acct: Account,
}

/// One resident raster: its source slot (and feathered read slot), offsets in floats.
#[derive(Debug, Clone)]
struct RasterEntry {
    id: RasterId,
    generation: u64,
    w: usize,
    h: usize,
    src: u32,
    read: u32,
    /// (source generation, σ bits) the read slot was feathered for.
    feathered: Option<(u64, u32)>,
}

/// An uploaded RAW mosaic.
struct RawSlot {
    key: ImageKey,
    buf: wgpu::Buffer,
    _acct: Account,
}

/// How DEVELOP ends: the output pass into a readback slot, or (export tiles) storing the
/// capture-sharpened encoded image into a stripe at (x, y) with a row stride.
enum Finish<'a> {
    Output { slot: OutSlot, histogram: bool },
    Store { dst: &'a wgpu::Buffer, at: [u32; 3] },
}

/// Submitted DEVELOP work: a readback, or a completion signal (stored tiles).
enum Pending {
    Frame(Readback),
    Stored(
        std::sync::mpsc::Receiver<()>,
        wgpu::Device,
        Arc<std::sync::atomic::AtomicBool>,
    ),
}

/// Tile size (output pixels) of tiled renders: with the context apron a tile's DEVELOP
/// stays around 10 ms, so an interactive frame never waits longer behind one.
const TILE: u32 = 1024;

/// Error for work that straddles a device loss (never `Ok` with stale pixels).
fn lost_during(what: &str) -> PipelineError {
    PipelineError::Gpu(format!("GPU device lost during the {what}"))
}

struct Inner {
    ctx: Context,
    bases: BaseCache<GpuBase>,
    raws: Vec<RawSlot>,
    /// Retired RAW upload buffers (mappable ones), reused by the next upload of the same
    /// size: already paged in, so an upload is one parallel copy.
    raw_pool: Vec<RawSlot>,
    /// The interactive image: its RAW upload is never evicted by background work.
    pinned: Option<ImageKey>,
    layer_rasters: HashMap<(u64, u64), LayerRasters>,
    /// Test hook: pretend the device allows at most this many bytes per buffer.
    max_buffer_override: Option<u64>,
    /// Device rebuilds after losses (diagnostics).
    rebuilds: u64,
    /// Per lane, the last lookup-table buffer and its contents: a render with the same tables (a
    /// slider that doesn't touch them, or an imported profile's ~1 MB maps under
    /// another edit) reuses it instead of uploading again.
    tables: [Option<(Vec<f32>, wgpu::Buffer)>; LANES],
    /// Lookup-table uploads (diagnostics, tests).
    table_uploads: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    mem: MemGate,
    /// CPU mask rasters (prepared outside GPU turns).
    rasters: Mutex<RasterCache>,
    gate: Gate,
    /// Eviction passes `admit` ran (diagnostics, tests).
    room_passes: std::sync::atomic::AtomicU64,
    /// Images being uploaded now, and RAW uploads done (diagnostics, tests).
    uploading: (
        Mutex<std::collections::HashSet<ImageKey>>,
        std::sync::Condvar,
    ),
    uploads: std::sync::atomic::AtomicU64,
    /// Unmapped output slots ready for reuse.
    outs: Mutex<Vec<OutSlot>>,
}

/// The GPU renderer (cheap to clone; clones share the device and caches).
#[derive(Clone)]
pub struct GpuRenderer {
    shared: Arc<Shared>,
    adapter: String,
}

/// Outputs above this many pixels rendered at `Quality::Full` or in the background are
/// developed in tiles (seamless, see `Plan::tile`) so cancellation and interactive
/// renders only ever wait for one tile.
const TILE_PIXELS: u64 = 2_000_000;

fn lock<T>(m: &Mutex<T>) -> crate::par::Held<'_, T> {
    crate::par::hold(m)
}

impl fmt::Debug for GpuRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GpuRenderer")
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}

/// Frame-buffer pool lane of a request: per class (renders of different classes
/// interleave their turns, so they must not share intermediate buffers); interactive
/// full-quality (tiled) renders get their own.
fn lane_of(req: &RenderRequest) -> u8 {
    match req.priority {
        Priority::Interactive if req.quality == crate::Quality::Full => 4,
        p => p.class() as u8,
    }
}

/// Frame-buffer bytes DEVELOP of `plan` uses for `px` pixels developed at once.
fn pool_need(plan: &Plan, px: u64) -> u64 {
    let mut per_px = if plan.base_key.precise { 80 } else { 40 };
    if plan.lens_blur.is_some() || plan.fx.is_active() {
        // The lens blur's circle of confusion and grid; the overlays' grids.
        per_px += 8;
    }
    if plan.needs_lin() {
        // The linear plane HDR and dehaze read, and their guided-filter coefficients.
        per_px += 24;
    }
    if !plan.layers.is_empty() {
        // A layer's mask, its feathering and the planes it is built from.
        per_px += 24;
    }
    px * per_px
}

/// Raw uploads kept resident (thumbnail and viewer usually touch the same few images).
const RAW_SLOTS: usize = 3;

/// How long an admission waiter sleeps before re-checking the budget and its cancel token.
const ADMIT_RECHECK: std::time::Duration = std::time::Duration::from_millis(20);

impl GpuRenderer {
    pub fn new() -> Result<Self, PipelineError> {
        let ctx = Context::new()?;
        let adapter = ctx.adapter_name.clone();
        Ok(Self {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    ctx,
                    bases: BaseCache::default(),
                    raws: Vec::new(),
                    raw_pool: Vec::new(),
                    pinned: None,
                    layer_rasters: HashMap::new(),
                    max_buffer_override: None,
                    rebuilds: 0,
                    tables: Default::default(),
                    table_uploads: 0,
                }),
                rasters: Mutex::new(RasterCache::gpu_resident()),
                mem: MemGate::default(),
                gate: Gate::default(),
                room_passes: std::sync::atomic::AtomicU64::new(0),
                uploading: Default::default(),
                uploads: std::sync::atomic::AtomicU64::new(0),
                outs: Mutex::new(Vec::new()),
            }),
            adapter,
        })
    }

    fn lock(&self) -> crate::par::Held<'_, Inner> {
        crate::par::hold(&self.shared.inner)
    }

    /// Name of the GPU adapter in use.
    pub fn adapter_name(&self) -> &str {
        &self.adapter
    }

    /// Number of BASE builds so far (cache misses).
    pub fn base_builds(&self) -> u64 {
        self.lock().bases.builds
    }

    /// Lookup-table uploads so far (a render with unchanged tables reuses the last).
    pub fn table_uploads(&self) -> u64 {
        self.lock().table_uploads
    }

    /// Resident BASE levels.
    pub fn cached_bases(&self) -> usize {
        self.lock().bases.len()
    }

    /// Brush-mask full rasterizations so far.
    pub fn raster_misses(&self) -> u64 {
        lock(&self.shared.rasters).misses
    }

    /// Bytes held by the frame-buffer pool and idle output slots (not counting BASE
    /// levels).
    pub fn pool_bytes(&self) -> u64 {
        let outs: u64 = lock(&self.shared.outs).iter().map(|o| o.gpu.size()).sum();
        self.lock().ctx.pool_bytes() + outs
    }

    /// Bytes of uploaded RAW mosaics.
    pub fn raw_bytes(&self) -> u64 {
        self.lock().raws.iter().map(|r| r.buf.size()).sum()
    }

    /// Test/diagnostic hook: cap the per-buffer size (as a smaller GPU would).
    pub fn set_max_buffer_bytes(&self, bytes: Option<u64>) {
        let mut g = self.lock();
        g.max_buffer_override = bytes;
        g.apply_limit();
    }

    /// Simulate a lost device (tests); the next render rebuilds the context.
    pub fn simulate_device_loss(&self) {
        self.lock().ctx.mark_lost();
    }

    /// Device rebuilds after losses so far (one per loss); for tests and diagnostics.
    #[doc(hidden)]
    pub fn device_rebuilds(&self) -> u64 {
        self.lock().rebuilds
    }

    /// Test hook: really destroy the device (as a GPU reset or eGPU unplug would).
    #[doc(hidden)]
    pub fn destroy_device_for_tests(&self) {
        self.lock().ctx.destroy();
    }

    /// Rebuild the device if it is lost; output slots of the lost generation go too.
    fn recover_if_lost(&self, inner: &mut Inner) -> Result<(), PipelineError> {
        if inner.ctx.poll_lost() {
            inner.recover()?;
            let epoch = inner.ctx.epoch;
            lock(&self.shared.outs).retain(|o| o.epoch == epoch);
        }
        Ok(())
    }

    /// One GPU turn: wait for it (by priority), make sure the device is alive, then run
    /// `f` inside error scopes and `catch_unwind`. A panic marks the device lost.
    fn turn<T>(
        &self,
        req: &RenderRequest,
        f: impl FnOnce(&mut Inner) -> Result<T, PipelineError>,
    ) -> Result<T, PipelineError> {
        let _turn = self.shared.gate.turn(req.priority, req.cancel.as_ref())?;
        let mut inner = self.lock();
        self.recover_if_lost(&mut inner)?;
        inner.ctx.lane = lane_of(req);
        let r = catch_unwind(AssertUnwindSafe(|| {
            let oom = inner
                .ctx
                .device
                .push_error_scope(wgpu::ErrorFilter::OutOfMemory);
            let val = inner
                .ctx
                .device
                .push_error_scope(wgpu::ErrorFilter::Validation);
            let r = f(&mut inner);
            let unknown = inner
                .ctx
                .unknown_kernel
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let v = pollster::block_on(val.pop());
            let o = pollster::block_on(oom.pop());
            match v.or(o) {
                Some(e) => {
                    // Our own work validates; an error here means the device (or its
                    // memory) is gone. Rebuild it on the next turn.
                    inner.ctx.lost.store(true, Ordering::SeqCst);
                    Err(PipelineError::Gpu(e.to_string()))
                }
                None => match unknown {
                    // A programming error in a kernel name: the device is fine.
                    Some(k) => Err(PipelineError::Gpu(format!("unknown GPU kernel `{k}`"))),
                    None => r,
                },
            }
        }));
        r.unwrap_or_else(|_| {
            inner.ctx.lost.store(true, Ordering::SeqCst);
            Err(PipelineError::Gpu("GPU failure (device lost?)".into()))
        })
    }

    /// Wait until a render needing about `need` bytes fits the budget (evicting caches of
    /// other images first); a render alone is always admitted.
    ///
    /// Caches are evicted once on entry and again only after another admission ended
    /// (its notify), never on a timeout: a waiter that polled would take the `Inner` lock
    /// the viewer needs and keep destroying idle output slots.
    fn admit(
        &self,
        key: ImageKey,
        need: u64,
        req: &RenderRequest,
    ) -> Result<Admission<'_>, PipelineError> {
        let gate = &self.shared.mem;
        // Non-interactive renders leave a quarter of the budget free, so the viewer is
        // admitted at once instead of waiting for an export to finish.
        let headroom = if req.priority == Priority::Interactive {
            0
        } else {
            crate::cache::budget_bytes() / 4
        };
        let mut make_room = true;
        loop {
            // Reservations of renders in flight are committed already: only what they
            // have not allocated yet, and what the caches hold, stands in the way.
            if make_room && crate::cache::over_budget_with(need) {
                self.shared.room_passes.fetch_add(1, Ordering::Relaxed);
                self.lock().make_room(need, key);
                if crate::cache::over_budget_with(need) {
                    // Idle output slots are the cheapest to give back, but only
                    // once the caches alone did not make room.
                    lock(&self.shared.outs).clear();
                }
            }
            // A plain lock (condvar-waited, nothing parallel inside).
            let mut n = gate
                .admitted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let limit = (*n > 0).then(|| crate::cache::budget_bytes().saturating_sub(headroom));
            if let Some(r) = crate::cache::Reservation::try_new(need, limit) {
                *n += 1;
                return Ok(Admission(gate, Some(r)));
            }
            req.check_cancel()?;
            // Cancellation has no notify, so the wait is bounded; memory freed elsewhere
            // is seen by the re-check on timeout.
            let (n, timeout) = gate
                .cv
                .wait_timeout(n, ADMIT_RECHECK)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(n);
            make_room = !timeout.timed_out();
        }
    }

    /// Rough working set of a render: the output slot, frame buffers of what is developed
    /// at once (`tile`: the plan of the largest tile, for tiled work), a BASE and a RAW
    /// upload when not resident.
    fn estimate(
        &self,
        key: ImageKey,
        raw: &RawImage,
        plan: &Plan,
        format: OutputFormat,
        tile: Option<&Plan>,
        req: &RenderRequest,
    ) -> u64 {
        let out = plan.out_size().pixels() * gpu_bpp(format);
        let tiled = tile.is_some();
        let dev = tile.unwrap_or(plan);
        let mut inner = self.lock();
        let base = if inner.cached_base(key, plan).is_some() {
            0
        } else {
            let bk = plan.base_key;
            let [w, h] = plan.level_size;
            let px = u64::from(w) * u64::from(h);
            // Tiled work builds a full-resolution RCD level per stripe; other levels are
            // built whole either way.
            let px = if tiled && bk.mode == DemosaicMode::Rcd && bk.down == 0 {
                px.min(u64::from(w) * u64::from(TILE + 256))
            } else {
                px
            };
            // The demosaic's work buffers for one band (at most the sensor's width).
            let scratch = if bk.mode == DemosaicMode::Rcd && raw.cfa != CfaPattern::LinearRgb {
                let (sw, m) = (raw.size.width, plan::BBOX_MARGIN);
                let n = u64::from(sw + 2 * m) * u64::from(base::rcd_band(sw) + 2 * m);
                base::rcd_bufs(n).iter().map(|b| b.1).sum()
            } else {
                0
            };
            px * if bk.precise { 16 } else { 8 } + scratch
        };
        let raw_bytes =
            if raw.cfa == CfaPattern::LinearRgb || inner.raws.iter().any(|r| r.key == key) {
                0
            } else {
                (raw.data.len() * 2) as u64
            };
        // Only the render's own lane's buffers are reused (and if another render trims
        // them meanwhile, DEVELOP makes room for them again, see `Inner::develop`).
        let pooled = inner.ctx.lane_pool_bytes(lane_of(req));
        out + base + raw_bytes + pool_need(dev, dev.geo.out.pixels()).saturating_sub(pooled)
    }
}

impl Inner {
    /// A read-only storage buffer holding `data`: the previous render's of this lane when
    /// it held the same tables (compared bitwise, so a NaN in a table still matches).
    fn tables_buffer(&mut self, data: &[f32]) -> Result<wgpu::Buffer, PipelineError> {
        let lane = usize::from(self.ctx.lane);
        let bytes: &[u8] = bytemuck::cast_slice(data);
        if let Some((d, b)) = &self.tables[lane]
            && bytemuck::cast_slice::<f32, u8>(d) == bytes
        {
            return Ok(b.clone());
        }
        let b = self.ctx.storage_init("tables", bytes)?;
        self.table_uploads += 1;
        self.tables[lane] = Some((data.to_vec(), b.clone()));
        Ok(b)
    }

    fn apply_limit(&mut self) {
        let dev = self.ctx.device.limits();
        let real = dev.max_buffer_size.min(dev.max_storage_buffer_binding_size);
        self.ctx.max_buffer = self.max_buffer_override.map_or(real, |o| o.min(real));
    }

    /// Rebuild the device after a loss; every GPU resource is gone with it.
    fn recover(&mut self) -> Result<(), PipelineError> {
        if !self.ctx.lost.load(Ordering::SeqCst) {
            return Ok(());
        }
        if !self.ctx.really_lost() {
            // A suspicion that didn't hold (e.g. an error from work that straddled an
            // earlier loss): keep the device and its caches.
            self.ctx.lost.store(false, Ordering::SeqCst);
            return Ok(());
        }
        self.ctx = Context::new()?;
        self.rebuilds += 1;
        self.bases = BaseCache::default();
        self.raws.clear();
        self.raw_pool.clear();
        self.layer_rasters.clear();
        self.tables = Default::default();
        self.apply_limit();
        Ok(())
    }

    /// Keep the process under its memory budget before allocating `bytes`: drop pooled
    /// buffers the current render hasn't used, then RAW uploads, mask rasters and BASE
    /// levels of other images (never `keep`'s or the pinned interactive image's upload).
    fn make_room(&mut self, bytes: u64, keep: ImageKey) {
        if !crate::cache::over_budget_with(bytes) {
            return;
        }
        self.ctx.trim_stale(0);
        self.raw_pool.clear();
        while crate::cache::over_budget_with(bytes) {
            let pinned = self.pinned;
            if let Some(i) = self
                .raws
                .iter()
                .position(|r| r.key != keep && Some(r.key) != pinned)
            {
                self.raws.remove(i);
                continue;
            }
            let before = self.layer_rasters.len();
            self.layer_rasters.retain(|(img, _), _| *img == keep.0);
            if self.layer_rasters.len() != before {
                continue;
            }
            if !self.bases.evict_oldest_except(keep) {
                break;
            }
        }
    }

    fn evict(&mut self, key: ImageKey) {
        self.bases.evict(key);
        if let Some(i) = self.raws.iter().position(|r| r.key == key) {
            let old = self.raws.remove(i);
            self.retire_raw(old);
        }
        if self.pinned == Some(key) {
            self.pinned = None;
        }
        self.layer_rasters.retain(|(img, _), _| *img != key.0);
    }
}
