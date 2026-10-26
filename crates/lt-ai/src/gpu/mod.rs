//! Batched GPU (wgpu → Metal) implementation of [`crate::analyze`].
//!
//! **What runs where.** Every full-resolution reduction runs on the device,
//! in integer arithmetic, so its results are *bit-identical* to the CPU's:
//!
//! | reduction | kernel | output |
//! |---|---|---|
//! | stencil sums (Laplacian, Sobel tensor, luma moments, clips) per tile and per subject-region block | `stats.wgsl`, one workgroup per rectangle | 9 × i64 per rectangle |
//! | pHash/dHash box sums (≤ 64 × 64 boxes) + 256-bin luma histogram (exposure) | `boxes.wgsl`, one workgroup per box row | ≤ 4096 + 256 × u32 per image |
//! | noise residual histograms at 1–4 px spacings on the flattest tiles | `noise.wgsl`, one workgroup per (tile, spacing) | 4 × 4081 × u32 per image |
//!
//! The noise pass needs to know which tiles are flattest, which needs the
//! tile sums, so a batch is two submissions with one small readback between
//! them. Everything after the reductions — per-tile ratios, focus-region
//! selection, medians, the area resample and 32×32 DCT of the hashes — is
//! O(tiles) or O(64²) per image and stays on the CPU, running the exact same
//! code as [`crate::analyze`] (in parallel across the batch). Measured on an
//! M4 Pro for a 1616×1080 preview: all of it takes ~54 µs of a ~2.95 ms
//! analysis; the pHash finish (box means, two area resamples, DCT, dHash)
//! ~10 µs, the 32×32 DCT alone ~1.4 µs — less than one dispatch plus
//! readback. The full-resolution box sums it starts from (~146 µs) run on
//! the device.
//!
//! **Memory.** Images are uploaded in chunks of at most [`MAX_CHUNK_BYTES`]
//! of luma, [`MAX_CHUNK_IMAGES`] images and [`MAX_CHUNK_RECTS`] stencil
//! rectangles, so a 1 000-image cull holds one chunk's buffers: ≈ 125 MB
//! for 64 A9 previews, ≤ ~220 MB in the worst case. On unified memory the
//! luma is copied straight into the buffer the kernels read. Buffers are
//! pooled and reused between calls; each concurrent caller uses its own
//! set, at most one idle set is kept, and [`GpuAnalyzer::trim`] drops it.
//!
//! **Failure.** Any device error (validation, out of memory, device lost,
//! timeout, a panic inside wgpu) makes that chunk fall back to the CPU
//! implementation, so [`GpuAnalyzer::analyze_batch`] always returns a
//! complete, correct result. The device is then rebuilt before the next
//! chunk; if the rebuilt device fails before completing a chunk (or cannot
//! be opened), the analyzer stays on the CPU.
//!
//! **Threads.** `GpuAnalyzer` is `Send + Sync`; no lock is held while work
//! runs, so calling it from rayon tasks (or re-entrantly from one) is safe.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rayon::prelude::*;

use crate::analysis::{AnalysisConfig, ImageAnalysis};
use crate::noise::{self, MAX_RESIDUAL, STEPS};
use crate::phash::{self, PerceptualHash};
use crate::sharpness::{self, SubjectRegion};
use crate::tiles::{self, MAX_TILE, TileGrid, TileStats};
use crate::{LumaImage, exposure};

/// Why a [`GpuAnalyzer`] could not be created.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GpuError {
    #[error("no GPU adapter available")]
    NoAdapter,
    #[error("GPU error: {0}")]
    Device(String),
}

fn dev_err(e: impl std::fmt::Display) -> GpuError {
    GpuError::Device(e.to_string())
}

/// Largest luma payload of one device submission (also capped by the
/// device's storage-binding limit).
pub const MAX_CHUNK_BYTES: u64 = 128 << 20;
/// Most images in one device submission (bounds the per-image outputs:
/// 64 KiB of noise histograms each).
pub const MAX_CHUNK_IMAGES: usize = 256;
/// Most stencil rectangles (tiles + subject blocks) in one submission:
/// 72 bytes of output each, so ≤ 36 MiB.
pub const MAX_CHUNK_RECTS: usize = 1 << 19;

/// Longest image side the kernels take: keeps their u32 index math
/// (`64 · side`) from overflowing. Longer images are analyzed on the CPU.
pub const MAX_SIDE: usize = 1 << 24;

/// Workgroups per dispatch row (2-D dispatch past the 65 535 limit).
const ROW_STRIDE: u32 = 32_768;
/// u32 words per rectangle in the `stats` output: 9 × (lo, hi).
const STAT_WORDS: usize = 18;
const BINS: usize = MAX_RESIDUAL + 1;
const NOISE_WORDS: usize = STEPS.len() * BINS;
/// u32 words per image in the `metas` buffer (see `common.wgsl`).
const META_WORDS: usize = 12;
/// Upper bound on one wait for the device (a healthy batch takes ~10 ms).
const WAIT: Duration = Duration::from_secs(20);

const COMMON: &str = include_str!("shaders/common.wgsl");

const STORAGE: wgpu::BufferUsages = wgpu::BufferUsages::STORAGE;
const DST: wgpu::BufferUsages = wgpu::BufferUsages::COPY_DST;
const SRC: wgpu::BufferUsages = wgpu::BufferUsages::COPY_SRC;

/// Buffers of one chunk, grown on demand and reused.
struct Scratch {
    bufs: HashMap<&'static str, wgpu::Buffer>,
    /// Growth never doubles past this (the device's chunk limit), so a
    /// grown buffer always stays bindable.
    cap: u64,
}

impl Scratch {
    fn get(
        &mut self,
        device: &wgpu::Device,
        name: &'static str,
        size: u64,
        usage: wgpu::BufferUsages,
    ) -> wgpu::Buffer {
        let size = size.max(16).next_multiple_of(16);
        if let Some(b) = self.bufs.get(name)
            && b.size() >= size
        {
            return b.clone();
        }
        // Grow geometrically so a slowly growing batch does not reallocate
        // each call, but never past the cap.
        let size = self
            .bufs
            .get(name)
            .map_or(size, |b| size.max((b.size() * 2).min(self.cap)));
        let b = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(name),
            size,
            usage,
            mapped_at_creation: false,
        });
        self.bufs.insert(name, b.clone());
        b
    }
}

/// One device with its pipelines and buffer pool.
struct Dev {
    device: wgpu::Device,
    queue: wgpu::Queue,
    stats: wgpu::ComputePipeline,
    boxes: wgpu::ComputePipeline,
    noise: wgpu::ComputePipeline,
    name: String,
    chunk_bytes: u64,
    /// Storage buffers can be mapped for writing (unified memory): images
    /// are copied straight into the buffer the kernels read.
    mappable: bool,
    /// Set by the device-lost callback or a failed chunk: rebuild before reuse.
    broken: Arc<AtomicBool>,
    /// This device replaced a failed one; if it fails before completing a
    /// chunk, the fault is not transient and the analyzer gives up.
    rebuilt: bool,
    completed: AtomicBool,
    pool: Mutex<Vec<Scratch>>,
    /// Test double for device failures: 1 = the next chunk errors, 2 = it
    /// panics, 3 = the next buffer map times out.
    #[cfg(test)]
    fail: std::sync::atomic::AtomicU8,
}

impl Dev {
    /// Open a high-performance adapter (`force_fallback` asks for a software
    /// adapter instead, which the Metal-only build never has: the test seam
    /// for "no GPU").
    fn open(force_fallback: bool, rebuilt: bool) -> Result<Dev, GpuError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: force_fallback,
            ..Default::default()
        }))
        .map_err(|_| GpuError::NoAdapter)?;
        let limits = adapter.limits();
        let mappable = adapter.get_info().device_type == wgpu::DeviceType::IntegratedGpu
            && adapter
                .features()
                .contains(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("lt-ai"),
            required_features: if mappable {
                wgpu::Features::MAPPABLE_PRIMARY_BUFFERS
            } else {
                wgpu::Features::empty()
            },
            required_limits: limits.clone(),
            ..Default::default()
        }))
        .map_err(dev_err)?;
        let broken = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&broken);
        device.set_device_lost_callback(move |_, _| flag.store(true, Ordering::SeqCst));
        // Errors outside an error scope must not panic; chunks run inside
        // scopes and turn errors into a CPU fallback.
        device.on_uncaptured_error(Arc::new(|e: wgpu::Error| {
            eprintln!("lt-ai: uncaptured wgpu error: {e}");
        }));
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pipe = |name: &str, src: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(name),
                source: wgpu::ShaderSource::Wgsl(src.replace("//INCLUDE_COMMON", COMMON).into()),
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(name),
                layout: None,
                module: &module,
                entry_point: Some("main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        let stats = pipe("stats", include_str!("shaders/stats.wgsl"));
        let boxes = pipe("boxes", include_str!("shaders/boxes.wgsl"));
        let noise = pipe("noise", include_str!("shaders/noise.wgsl"));
        if let Some(e) = pollster::block_on(scope.pop()) {
            return Err(dev_err(e));
        }
        let chunk_bytes = MAX_CHUNK_BYTES
            .min(limits.max_storage_buffer_binding_size)
            .min(limits.max_buffer_size);
        Ok(Dev {
            device,
            queue,
            stats,
            boxes,
            noise,
            name: adapter.get_info().name,
            chunk_bytes,
            mappable,
            broken,
            rebuilt,
            completed: AtomicBool::new(false),
            pool: Mutex::new(Vec::new()),
            #[cfg(test)]
            fail: std::sync::atomic::AtomicU8::new(0),
        })
    }
}

/// Batched GPU analysis; see the module docs. Results equal
/// [`crate::analyze_with_subject`] with the same config, bit for bit.
pub struct GpuAnalyzer {
    cfg: AnalysisConfig,
    /// `None` once a rebuild after a failure could not open a device.
    dev: Mutex<Option<Arc<Dev>>>,
    /// [`MAX_CHUNK_RECTS`]; lowered by tests.
    rect_limit: usize,
    /// Chunks completed on the device (see [`GpuAnalyzer::gpu_chunks`]).
    gpu_chunks: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for GpuAnalyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuAnalyzer")
            .field("adapter", &self.adapter_name())
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

/// Per-image placement in a chunk.
struct Slot {
    /// Byte offset of row 0 in the luma buffer.
    off: usize,
    cols: usize,
    rows: usize,
    /// Stencil rectangles: the tiles, then each valid subject region's blocks.
    tiles: Range<usize>,
    regions: Vec<Range<usize>>,
    box_base: usize,
    gw: usize,
    gh: usize,
}

impl GpuAnalyzer {
    /// A high-performance adapter with the default [`AnalysisConfig`].
    pub fn new() -> Result<Self, GpuError> {
        Self::with_config(AnalysisConfig::default())
    }

    pub fn with_config(cfg: AnalysisConfig) -> Result<Self, GpuError> {
        Ok(Self {
            cfg,
            dev: Mutex::new(Some(Arc::new(Dev::open(false, false)?))),
            rect_limit: MAX_CHUNK_RECTS,
            gpu_chunks: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Adapter name (e.g. "Apple M4 Pro"); empty once the analyzer has
    /// fallen back to the CPU for good.
    pub fn adapter_name(&self) -> String {
        self.lock()
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_default()
    }

    /// False once a lost device could not be rebuilt: every call then runs
    /// on the CPU.
    pub fn is_available(&self) -> bool {
        self.lock().is_some()
    }

    /// Chunks completed on the GPU so far (the rest ran on the CPU): lets a
    /// caller or test tell a GPU run from a silent fallback.
    pub fn gpu_chunks(&self) -> u64 {
        self.gpu_chunks.load(Ordering::Relaxed)
    }

    pub fn config(&self) -> &AnalysisConfig {
        &self.cfg
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<Dev>>> {
        self.dev.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current device, rebuilt first if the last one failed (unless it
    /// was itself a rebuild that never completed a chunk).
    fn device(&self) -> Option<Arc<Dev>> {
        let mut dev = self.lock();
        if let Some(d) = dev.as_ref()
            && d.broken.load(Ordering::SeqCst)
        {
            let retry = !d.rebuilt || d.completed.load(Ordering::SeqCst);
            *dev = retry
                .then(|| Dev::open(false, true).ok().map(Arc::new))
                .flatten();
        }
        dev.clone()
    }

    /// Release the pooled chunk buffers (up to ~220 MB after a large
    /// batch); the next batch allocates them again.
    pub fn trim(&self) {
        if let Some(d) = self.lock().as_ref() {
            d.pool
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        }
    }

    /// Analyze every image; `out[i]` belongs to `images[i]` and equals
    /// `analyze(&images[i], cfg)`.
    pub fn analyze_batch(&self, images: &[LumaImage]) -> Vec<ImageAnalysis> {
        self.analyze_batch_with_subjects(images, &[])
    }

    /// Like [`Self::analyze_batch`] with subject regions: `subjects` is
    /// empty (no subjects) or holds one list per image, and `out[i]` equals
    /// `analyze_with_subject(&images[i], cfg, &subjects[i])`.
    ///
    /// Empty images, images too large for one chunk, and chunks that hit a
    /// device error are analyzed on the CPU.
    ///
    /// # Panics
    /// When `subjects` is neither empty nor as long as `images`.
    pub fn analyze_batch_with_subjects(
        &self,
        images: &[LumaImage],
        subjects: &[Vec<SubjectRegion>],
    ) -> Vec<ImageAnalysis> {
        let subject = subject_lookup(images, subjects);
        let tile = self.cfg.sharpness.tile.clamp(4, MAX_TILE);
        let mut out: Vec<Option<ImageAnalysis>> = vec![None; images.len()];
        let mut chunk: Vec<usize> = Vec::new();
        let (mut bytes, mut rects) = (0u64, 0usize);
        let flush = |chunk: &mut Vec<usize>, out: &mut [Option<ImageAnalysis>]| {
            if chunk.is_empty() {
                return;
            }
            let imgs: Vec<(&LumaImage, &[SubjectRegion])> =
                chunk.iter().map(|&i| (&images[i], subject(i))).collect();
            if let Some(res) = self.device().and_then(|d| {
                // A panic inside wgpu is a device failure like any other.
                let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    run_chunk(&d, &self.cfg, &imgs, tile)
                }))
                .unwrap_or_else(|_| Err(GpuError::Device("panic in a GPU chunk".into())));
                match &r {
                    Ok(_) => {
                        d.completed.store(true, Ordering::SeqCst);
                        self.gpu_chunks.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => d.broken.store(true, Ordering::SeqCst),
                }
                r.ok()
            }) {
                for (&i, a) in chunk.iter().zip(res) {
                    out[i] = Some(a);
                }
            }
            chunk.clear();
        };
        let limit = self.lock().as_ref().map_or(0, |d| d.chunk_bytes);
        for (i, img) in images.iter().enumerate() {
            // Image starts are 256-byte aligned in the chunk buffer.
            let len = padded_len(img).next_multiple_of(256);
            let n = rect_count(img, subject(i), tile);
            let (w, h) = (img.width(), img.height());
            if img.is_empty() || w.max(h) > MAX_SIDE || len > limit || n > self.rect_limit {
                continue; // CPU below
            }
            if bytes + len > limit || rects + n > self.rect_limit || chunk.len() == MAX_CHUNK_IMAGES
            {
                flush(&mut chunk, &mut out);
                (bytes, rects) = (0, 0);
            }
            chunk.push(i);
            bytes += len;
            rects += n;
        }
        flush(&mut chunk, &mut out);
        out.into_par_iter()
            .enumerate()
            .map(|(i, a)| {
                a.unwrap_or_else(|| crate::analyze_with_subject(&images[i], &self.cfg, subject(i)))
            })
            .collect()
    }

    /// Simulate a lost device (tests the CPU fallback and the rebuild).
    #[cfg(test)]
    fn destroy_device(&self) {
        if let Some(d) = self.lock().as_ref() {
            d.device.destroy();
            let _ = d.device.poll(wgpu::PollType::Poll);
        }
    }
}

/// `subject(i)`: the regions of image `i` (none when `subjects` is empty).
fn subject_lookup<'a>(
    images: &[LumaImage],
    subjects: &'a [Vec<SubjectRegion>],
) -> impl Fn(usize) -> &'a [SubjectRegion] + Sync {
    assert!(
        subjects.is_empty() || subjects.len() == images.len(),
        "subjects: {} lists for {} images",
        subjects.len(),
        images.len()
    );
    move |i| subjects.get(i).map_or(&[][..], Vec::as_slice)
}

/// The rectangles (`x0, y0, x1, y1` of pixels with a whole stencil) one
/// image contributes: its tiles, then each valid subject region split into
/// blocks of at most `MAX_TILE` × `MAX_TILE` (which bounds the kernel's
/// per-thread i32 sums). Returns the per-region block ranges, relative.
fn rects_of(
    img: &LumaImage,
    subject: &[SubjectRegion],
    grid: &TileGrid,
    out: &mut Vec<[u32; 4]>,
) -> Vec<Range<usize>> {
    let (w, h) = (img.width(), img.height());
    let base = out.len();
    for t in 0..grid.cols * grid.rows {
        let (x0, y0, x1, y1) = tiles::tile_interior(grid, t, w, h);
        out.push([x0, y0, x1, y1].map(|v| v as u32));
    }
    sharpness::subject_rects(subject, w, h)
        .into_iter()
        .map(|r| {
            let start = out.len();
            let (x0, y0, x1, y1) = tiles::region_interior(r, w, h);
            for y in (y0..y1).step_by(MAX_TILE) {
                for x in (x0..x1).step_by(MAX_TILE) {
                    let (xe, ye) = ((x + MAX_TILE).min(x1), (y + MAX_TILE).min(y1));
                    out.push([x, y, xe, ye].map(|v| v as u32));
                }
            }
            start - base..out.len() - base
        })
        .collect()
}

/// Number of rectangles [`rects_of`] produces (for chunk planning).
fn rect_count(img: &LumaImage, subject: &[SubjectRegion], tile: usize) -> usize {
    let (w, h) = (img.width(), img.height());
    let blocks = |a: usize, b: usize| b.saturating_sub(a).div_ceil(MAX_TILE);
    let regions: usize = sharpness::subject_rects(subject, w, h)
        .into_iter()
        .map(|r| {
            let (x0, y0, x1, y1) = tiles::region_interior(r, w, h);
            blocks(x0, x1) * blocks(y0, y1)
        })
        .sum();
    w.div_ceil(tile) * h.div_ceil(tile) + regions
}

fn empty_grid(tile: usize, cols: usize, rows: usize) -> TileGrid {
    TileGrid {
        tile,
        cols,
        rows,
        stats: Vec::new(),
    }
}

/// One chunk on the device. All images are non-empty.
fn run_chunk(
    dev: &Dev,
    cfg: &AnalysisConfig,
    imgs: &[(&LumaImage, &[SubjectRegion])],
    tile: usize,
) -> Result<Vec<ImageAnalysis>, GpuError> {
    #[cfg(test)]
    match dev.fail.swap(0, Ordering::SeqCst) {
        1 => return Err(GpuError::Device("injected".into())),
        2 => panic!("injected"),
        _ => {}
    }
    let mut scratch = dev
        .pool
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop()
        .unwrap_or_else(|| Scratch {
            bufs: HashMap::new(),
            cap: dev.chunk_bytes,
        });
    let oom = dev.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let val = dev.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let res = run_chunk_scoped(dev, cfg, &mut scratch, imgs, tile);
    let val = pollster::block_on(val.pop());
    let oom = pollster::block_on(oom.pop());
    if let Some(e) = val.or(oom) {
        return Err(dev_err(e));
    }
    // A failed chunk may leave buffers mapped or half-written: drop them.
    if res.is_ok() {
        let mut pool = dev.pool.lock().unwrap_or_else(PoisonError::into_inner);
        if pool.is_empty() {
            pool.push(scratch);
        }
    }
    res
}

#[allow(clippy::too_many_lines)]
fn run_chunk_scoped(
    dev: &Dev,
    cfg: &AnalysisConfig,
    scratch: &mut Scratch,
    imgs: &[(&LumaImage, &[SubjectRegion])],
    tile: usize,
) -> Result<Vec<ImageAnalysis>, GpuError> {
    let device = &dev.device;
    let n = imgs.len();
    let (mut off, mut box_base, mut box_rows) = (0, 0, 0);
    let mut slots = Vec::with_capacity(n);
    let mut meta: Vec<u32> = Vec::with_capacity(n * META_WORDS);
    let mut rects: Vec<[u32; 4]> = Vec::new();
    let mut rect_img: Vec<u32> = Vec::new();
    for (k, (img, subject)) in imgs.iter().enumerate() {
        let (w, h) = (img.width(), img.height());
        let (cols, rows) = (w.div_ceil(tile), h.div_ceil(tile));
        let (gw, gh) = phash::grid_size(w, h);
        let start = rects.len();
        let regions = rects_of(img, subject, &empty_grid(tile, cols, rows), &mut rects);
        rect_img.resize(rects.len(), k as u32);
        meta.extend(
            [
                w,
                h,
                off / 4,
                w.div_ceil(4),
                tile,
                cols,
                start,
                box_base,
                box_rows,
                gw,
                gh,
                0,
            ]
            .map(|v| v as u32),
        );
        slots.push(Slot {
            off,
            cols,
            rows,
            tiles: start..start + cols * rows,
            regions: regions
                .into_iter()
                .map(|r| start + r.start..start + r.end)
                .collect(),
            box_base,
            gw,
            gh,
        });
        // Map-aligned image starts (see `wgpu::MAP_ALIGNMENT`).
        off = (off + padded_len(img) as usize).next_multiple_of(256);
        box_base += gw * gh;
        box_rows += gh;
    }
    let n_rects = rects.len();

    // Upload, one image per task: straight into the storage buffer where it
    // can be mapped (unified memory), else through staging memory.
    let luma = if dev.mappable {
        let usage = STORAGE | wgpu::BufferUsages::MAP_WRITE;
        let luma = scratch.get(device, "luma", off as u64, usage);
        map(dev, &luma, wgpu::MapMode::Write, off as u64, None)?;
        let views = imgs
            .iter()
            .zip(&slots)
            .map(|((img, _), s)| {
                let at = s.off as u64;
                luma.get_mapped_range_mut(at..at + padded_len(img))
                    .map_err(|e| dev_err(format!("{e:?}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        views
            .into_par_iter()
            .zip(imgs.par_iter())
            .for_each(|(mut view, (img, _))| fill(view.slice(..), img));
        luma.unmap();
        luma
    } else {
        let luma = scratch.get(device, "luma", off as u64, STORAGE | DST);
        let views = imgs
            .iter()
            .zip(&slots)
            .map(|((img, _), s)| {
                let size = NonZeroU64::new(padded_len(img)).expect("images are non-empty");
                dev.queue
                    .write_buffer_with(&luma, s.off as u64, size)
                    .ok_or_else(|| GpuError::Device("staging allocation failed".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut views = views;
        views
            .par_iter_mut()
            .zip(imgs.par_iter())
            .for_each(|(view, (img, _))| fill(view.slice(..), img));
        // Dropping a view schedules its copy: on this thread, inside the
        // chunk's error scopes.
        drop(views);
        luma
    };
    let upload = |scratch: &mut Scratch, name: &'static str, words: &[u32]| {
        let usage = STORAGE | wgpu::BufferUsages::UNIFORM | DST;
        let b = scratch.get(device, name, (words.len() * 4) as u64, usage);
        dev.queue.write_buffer(&b, 0, bytemuck::cast_slice(words));
        b
    };
    let metas = upload(scratch, "metas", &meta);
    let jobs: Vec<u32> = rects
        .iter()
        .zip(&rect_img)
        .flat_map(|(r, &k)| [k, r[0], r[1], r[2], r[3]])
        .collect();
    let jobs = upload(scratch, "rects", &jobs);
    let params = |scratch: &mut Scratch, name: &'static str, items: usize| {
        upload(scratch, name, &[n as u32, items as u32, ROW_STRIDE, 0])
    };
    let p_stats = params(scratch, "p_stats", n_rects);
    let p_boxes = params(scratch, "p_boxes", box_rows);

    // Pass 1: stencil sums; box sums + histograms.
    let stat_bytes = (n_rects * STAT_WORDS * 4) as u64;
    let hist_bytes = (n * 256 * 4) as u64;
    let box_bytes = (box_base * 4) as u64;
    let stats_buf = scratch.get(device, "stats", stat_bytes, STORAGE | SRC);
    let hist_buf = scratch.get(device, "hist", hist_bytes, STORAGE | SRC | DST);
    let boxes_buf = scratch.get(device, "boxes", box_bytes, STORAGE | SRC);
    let rb_bytes = stat_bytes + hist_bytes + box_bytes;
    let rb = scratch.get(
        device,
        "readback",
        rb_bytes.max((n * NOISE_WORDS * 4) as u64),
        wgpu::BufferUsages::MAP_READ | DST,
    );
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    enc.clear_buffer(&hist_buf, 0, Some(hist_bytes));
    dispatch(
        dev,
        &mut enc,
        &dev.stats,
        &[&luma, &metas, &p_stats, &jobs, &stats_buf],
        n_rects,
    );
    dispatch(
        dev,
        &mut enc,
        &dev.boxes,
        &[&luma, &metas, &p_boxes, &boxes_buf, &hist_buf],
        box_rows,
    );
    enc.copy_buffer_to_buffer(&stats_buf, 0, &rb, 0, stat_bytes);
    enc.copy_buffer_to_buffer(&hist_buf, 0, &rb, stat_bytes, hist_bytes);
    enc.copy_buffer_to_buffer(&boxes_buf, 0, &rb, stat_bytes + hist_bytes, box_bytes);
    let words = readback(dev, enc, &rb, rb_bytes)?;
    let (stat_words, rest) = words.split_at(n_rects * STAT_WORDS);
    let (hist_words, box_words) = rest.split_at(n * 256);
    let stats: Vec<TileStats> = stat_words
        .as_chunks::<STAT_WORDS>()
        .0
        .iter()
        .map(tile_stats)
        .collect();

    // CPU: tile grids and the flattest tiles per image.
    let grids: Vec<(TileGrid, Vec<usize>)> = slots
        .par_iter()
        .map(|s| {
            let grid = TileGrid {
                stats: stats[s.tiles.clone()].to_vec(),
                ..empty_grid(tile, s.cols, s.rows)
            };
            let flat = noise::flat_tiles(&grid, cfg.noise);
            (grid, flat)
        })
        .collect();

    // Pass 2: noise residual histograms on the flat tiles.
    let mut noise_jobs: Vec<u32> = Vec::new();
    for (i, (_, flat)) in grids.iter().enumerate() {
        for step in STEPS {
            for &t in flat {
                noise_jobs.extend([i as u32, t as u32, step as u32]);
            }
        }
    }
    let noise_bytes = (n * NOISE_WORDS * 4) as u64;
    let noise_words = if noise_jobs.is_empty() {
        vec![0; n * NOISE_WORDS]
    } else {
        let jobs = upload(scratch, "noise_jobs", &noise_jobs);
        let nh = scratch.get(device, "noise", noise_bytes, STORAGE | SRC | DST);
        let p_noise = params(scratch, "p_noise", noise_jobs.len() / 3);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        enc.clear_buffer(&nh, 0, Some(noise_bytes));
        dispatch(
            dev,
            &mut enc,
            &dev.noise,
            &[&luma, &metas, &p_noise, &jobs, &nh],
            noise_jobs.len() / 3,
        );
        enc.copy_buffer_to_buffer(&nh, 0, &rb, 0, noise_bytes);
        readback(dev, enc, &rb, noise_bytes)?
    };

    // CPU: the same finishing code as `analyze_with_subject`.
    Ok(imgs
        .par_iter()
        .zip(slots.par_iter())
        .zip(grids.into_par_iter())
        .enumerate()
        .map(|(i, (((img, subject), s), (grid, flat)))| {
            let (w, h) = (img.width(), img.height());
            let noise = noise::from_hists(
                &grid,
                &flat,
                &noise_words[i * NOISE_WORDS..(i + 1) * NOISE_WORDS],
                w,
                h,
            );
            let subject_stats: Vec<TileStats> = s
                .regions
                .iter()
                .map(|r| {
                    let mut sum = TileStats::default();
                    stats[r.clone()].iter().for_each(|b| sum.add(b));
                    sum
                })
                .collect();
            let hist: &[u32; 256] = hist_words[i * 256..(i + 1) * 256]
                .try_into()
                .expect("256 bins");
            ImageAnalysis {
                width: w,
                height: h,
                sharpness: sharpness::measure_with(
                    &grid,
                    w,
                    h,
                    &cfg.sharpness,
                    noise.laplacian_floor,
                    subject,
                    &subject_stats,
                ),
                exposure: exposure::measure(hist, cfg.exposure),
                noise,
                hash: PerceptualHash::from_box_sums(
                    &box_words[s.box_base..s.box_base + s.gw * s.gh],
                    w,
                    h,
                ),
            }
        })
        .collect())
}

/// Bind `bufs` to bindings `0..` of `pipe` and dispatch `items` workgroups
/// in a pass of their own.
fn dispatch(
    dev: &Dev,
    enc: &mut wgpu::CommandEncoder,
    pipe: &wgpu::ComputePipeline,
    bufs: &[&wgpu::Buffer],
    items: usize,
) {
    if items == 0 {
        return;
    }
    let entries: Vec<wgpu::BindGroupEntry> = bufs
        .iter()
        .enumerate()
        .map(|(i, b)| wgpu::BindGroupEntry {
            binding: i as u32,
            resource: b.as_entire_binding(),
        })
        .collect();
    let group = dev.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipe.get_bind_group_layout(0),
        entries: &entries,
    });
    let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
    pass.set_pipeline(pipe);
    pass.set_bind_group(0, &group, &[]);
    let (x, y) = grid2d(items);
    pass.dispatch_workgroups(x, y, 1);
}

/// Map the first `bytes` of `buf` once `after` (or all submitted work) is done.
/// A map that fails or times out is cancelled, so the buffer is never left
/// with a pending map for the next user to trip over.
fn map(
    dev: &Dev,
    buf: &wgpu::Buffer,
    mode: wgpu::MapMode,
    bytes: u64,
    after: Option<wgpu::SubmissionIndex>,
) -> Result<(), GpuError> {
    let (tx, rx) = mpsc::channel();
    buf.map_async(mode, ..bytes, move |r| {
        let _ = tx.send(r);
    });
    #[cfg(test)]
    let injected = dev
        .fail
        .compare_exchange(3, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    #[cfg(not(test))]
    let injected = false;
    let waited = if injected {
        Err(GpuError::Device("injected map timeout".into()))
    } else {
        dev.device
            .poll(wgpu::PollType::Wait {
                submission_index: after,
                timeout: Some(WAIT),
            })
            .map_err(dev_err)
            .and_then(|_| rx.recv_timeout(WAIT).map_err(dev_err)?.map_err(dev_err))
    };
    if waited.is_err() {
        buf.unmap();
    }
    waited
}

/// Submit `enc`, wait for it, and return the first `bytes` of `rb`.
fn readback(
    dev: &Dev,
    enc: wgpu::CommandEncoder,
    rb: &wgpu::Buffer,
    bytes: u64,
) -> Result<Vec<u32>, GpuError> {
    let index = dev.queue.submit([enc.finish()]);
    map(dev, rb, wgpu::MapMode::Read, bytes, Some(index))?;
    let words = {
        let view = rb
            .get_mapped_range(..bytes)
            .map_err(|e| dev_err(format!("{e:?}")))?;
        bytemuck::try_cast_slice::<u8, u32>(&view)
            .map_err(dev_err)?
            .to_vec()
    };
    rb.unmap();
    Ok(words)
}

/// Copy `img` into its slot, rows padded with zeros to whole words.
fn fill(mut dst: wgpu::WriteOnly<'_, [u8]>, img: &LumaImage) {
    let (w, px) = (img.width(), img.pixels());
    let stride = w.next_multiple_of(4);
    if stride == w {
        dst.copy_from_slice(px);
    } else {
        for (y, row) in px.chunks_exact(w).enumerate() {
            let mut d = dst.slice(y * stride..(y + 1) * stride);
            d.slice(..w).copy_from_slice(row);
            d.slice(w..).fill(0);
        }
    }
}

/// Bytes an image occupies in the batch buffer: rows padded to whole words.
fn padded_len(img: &LumaImage) -> u64 {
    (img.width().next_multiple_of(4) * img.height()) as u64
}

/// `(x, y)` workgroup counts for `n` linear workgroups.
fn grid2d(n: usize) -> (u32, u32) {
    let n = n as u32;
    (n.min(ROW_STRIDE), n.div_ceil(ROW_STRIDE))
}

fn tile_stats(w: &[u32; STAT_WORDS]) -> TileStats {
    let v = |k: usize| (u64::from(w[2 * k]) | (u64::from(w[2 * k + 1]) << 32)) as i64;
    TileStats {
        n: v(0),
        lap: v(1),
        lap2: v(2),
        gxx: v(3),
        gyy: v(4),
        gxy: v(5),
        luma: v(6),
        luma2: v(7),
        clipped: v(8),
    }
}

#[cfg(test)]
mod tests;
