//! Develop tools on the shared renderer ([`crate::develop`]): auto white balance and
//! auto tone (GPU statistics), the white-balance picker, the as-shot Kelvin/tint and lens
//! profile readouts, and grid thumbnails of edited photos (rendered at
//! `Priority::Bulk`, cached on disk by ds-cache under `ds_core::PIPELINE_VERSION`).
//!
//! Tools run off the calling thread (`run_job_on_thread`) at `Priority::Tool`, ahead of
//! exports and thumbnails. Thumbnails run on two small fixed pools (cache lookups,
//! renders). Every call decodes the original through the shared decoded-RAW cache, so the
//! tools of the photo on screen don't decode it again.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use ds_cache::{AdjHash, CacheKey, DiskCache, Kind, render_hash};
use ds_core::adjust::WhiteBalance;
use ds_core::{Adjustments, FileHash, NormRect, Size};
use ds_pipeline::color::{
    CameraColor, multipliers_to_wb, normalize_mults, wb_temp_tint, wb_to_multipliers,
};
use ds_pipeline::{OutputFormat, Priority, Quality, RenderRequest};

use crate::develop::{Loaded, develop};
use crate::error::DsError;
use crate::jobs::{CancelToken, WorkerPool, guard, run_job_on_pool, run_job_on_thread};
use crate::viewer::DevelopMaskSource;

/// A white balance as the UI shows it.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct WbValue {
    pub kelvin: f32,
    pub tint: f32,
}

/// What the develop tools show for one original.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DevelopInfo {
    /// The camera's white balance, read through the camera matrix.
    pub as_shot: WbValue,
    /// EXIF lens model.
    pub lens: Option<String>,
    /// The lens-correction profile the pipeline uses, e.g. "Sony FE 24-70mm F2.8 GM".
    pub lens_profile: Option<String>,
    /// ISO (drives noise reduction).
    pub iso: Option<u32>,
    /// "Apple M4 Pro" or "CPU rendering (…)".
    pub renderer: String,
}

fn parse(json: &str) -> Result<Adjustments, DsError> {
    serde_json::from_str(json).map_err(|e| DsError::invalid(format!("adjustments JSON: {e}")))
}

fn to_json(a: &Adjustments) -> Result<String, DsError> {
    serde_json::to_string(a).map_err(|e| DsError::Internal {
        message: format!("adjustments JSON: {e}"),
    })
}

fn wb_value(wb: WhiteBalance, loaded: &Loaded) -> WbValue {
    let (kelvin, tint) = wb_temp_tint(wb, &loaded.raw);
    WbValue { kelvin, tint }
}

/// The lens profile name (maker + model) for a capture, if one is vendored.
pub(crate) fn lens_profile_name(capture: &ds_pipeline::CaptureInfo) -> Option<String> {
    let lens = capture.lens.as_deref()?;
    let p = ds_pipeline::lens::find(lens, capture.crop_factor.unwrap_or(1.0))?;
    Some(if p.model.starts_with(&p.maker) {
        p.model.clone()
    } else {
        format!("{} {}", p.maker, p.model)
    })
}

/// The camera colour of `path` from its tags (no pixel decode for Sony uncompressed
/// files): what as-shot white balance and multiplier conversions need.
pub(crate) fn camera_color(path: &Path) -> Result<(CameraColor, [f32; 3]), DsError> {
    let ci = ds_raw::color_info(path)?;
    Ok((
        CameraColor {
            cam_to_xyz: ci.cam_to_xyz,
            daylight: normalize_mults(ci.wb_daylight),
        },
        ci.wb_as_shot,
    ))
}

pub(crate) fn wb_of(wb: WhiteBalance) -> WbValue {
    match wb {
        WhiteBalance::Custom { temp_k, tint } => WbValue {
            kelvin: temp_k,
            tint,
        },
        WhiteBalance::AsShot => WbValue {
            kelvin: 6500.0,
            tint: 0.0,
        },
    }
}

/// As-shot WB, lens and ISO from the file's tags. Never decodes the sensor (H4: relative
/// paste over a selection loads every target's as-shot value, and decoding them would
/// push the viewer's photo out of the decoded-RAW cache and the GPU).
fn info(path: &Path) -> Result<DevelopInfo, DsError> {
    let meta = ds_raw::read_metadata(path)?;
    let capture = crate::develop::capture_info(&meta);
    let (cam, as_shot) = camera_color(path)?;
    Ok(DevelopInfo {
        as_shot: wb_of(multipliers_to_wb(as_shot, &cam)),
        lens: capture.lens.clone(),
        lens_profile: lens_profile_name(&capture),
        iso: capture.iso,
        renderer: develop().backend(),
    })
}

fn auto_wb(path: &Path) -> Result<WbValue, DsError> {
    let d = develop();
    let loaded = d.load(path)?;
    let wb = match d.gpu() {
        Some(g) => g.auto_white_balance(loaded.key, &loaded.raw),
        None => ds_pipeline::auto::auto_white_balance(&loaded.raw),
    };
    Ok(wb_value(wb, &loaded))
}

fn tone(path: &Path, adj: &Adjustments) -> Result<Adjustments, DsError> {
    let d = develop();
    let loaded = d.load(path)?;
    let (exposure, hdr, levels) = match d.gpu() {
        Some(g) => g.auto_levels(loaded.key, &loaded.raw, adj),
        None => ds_pipeline::auto::auto_levels(&loaded.raw, adj),
    };
    Ok(Adjustments {
        exposure,
        hdr,
        levels,
        ..adj.clone()
    })
}

/// Display P3 (D65) linear → CIE XYZ.
const P3_TO_XYZ: [[f32; 3]; 3] = [
    [0.486_570_9, 0.265_667_7, 0.198_217_3],
    [0.228_974_6, 0.691_738_5, 0.079_286_9],
    [0.0, 0.045_113_4, 1.043_944_4],
];

fn mat_vec(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|r| m[r][0] * v[0] + m[r][1] * v[1] + m[r][2] * v[2])
}

fn mat_inv(m: &[[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let [a, b, c] = m[0];
    let [d, e, f] = m[1];
    let [g, h, i] = m[2];
    let det = a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g);
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    Some([
        [
            (e * i - f * h) * inv,
            (c * h - b * i) * inv,
            (b * f - c * e) * inv,
        ],
        [
            (f * g - d * i) * inv,
            (a * i - c * g) * inv,
            (c * d - a * f) * inv,
        ],
        [
            (d * h - e * g) * inv,
            (b * g - a * h) * inv,
            (a * e - b * d) * inv,
        ],
    ])
}

/// The largest picker sample, in pixels per side.
pub const MAX_PICK_SAMPLE: u32 = 31;

/// The region of the final frame (whole frame `w`×`h` at 1:1) holding the `n`×`n`
/// pixels centred on the pixel under `(x, y)` (normalized), shifted inside the frame.
fn pixel_box(w: u32, h: u32, x: f32, y: f32, n: u32) -> NormRect {
    let (n_w, n_h) = (n.min(w), n.min(h));
    let corner = |v: f32, len: u32, n: u32| {
        // The pixel under the point, then back by half the box, kept inside the frame.
        let px = ((v * len as f32).floor() as i64).clamp(0, i64::from(len) - 1);
        (px - i64::from((n - 1) / 2)).clamp(0, i64::from(len - n)) as f32
    };
    NormRect {
        x: corner(x, w, n_w) / w as f32,
        y: corner(y, h, n_h) / h as f32,
        w: n_w as f32 / w as f32,
        h: n_h as f32 / h as f32,
    }
}

/// Mean linear Display-P3 RGB of the develop render with `adj` around `(x, y)`
/// (normalized in the final, cropped and turned frame): exactly the `sample`×`sample`
/// full-resolution pixels centred on the pixel under the point (the viewer's loupe
/// outlines the same pixels), or, with `sample == 0`, a patch of 0.8% of the frame.
fn sample_linear(
    loaded: &Loaded,
    adj: &Adjustments,
    x: f32,
    y: f32,
    sample: u32,
) -> Result<[f32; 3], DsError> {
    const HALF: f32 = 0.004;
    let (region, max, quality) = if sample == 0 {
        let r = NormRect {
            x: (x - HALF).clamp(0.0, 1.0 - 2.0 * HALF),
            y: (y - HALF).clamp(0.0, 1.0 - 2.0 * HALF),
            w: 2.0 * HALF,
            h: 2.0 * HALF,
        };
        (r, Size::new(48, 48), Quality::Preview)
    } else {
        // 1:1 (a box larger than the frame is never upscaled): the frame's native size.
        let native = Size::new(1 << 16, 1 << 16);
        let full = ds_pipeline::output_size(&loaded.raw, adj, &RenderRequest::new(native))
            .map_err(crate::develop::pipeline_error)?;
        let r = pixel_box(full.width, full.height, x, y, sample.min(MAX_PICK_SAMPLE));
        (r, native, Quality::Full)
    };
    let req = RenderRequest {
        region: Some(region),
        quality,
        priority: Priority::Tool,
        ..RenderRequest::new(max)
    };
    let size =
        ds_pipeline::output_size(&loaded.raw, adj, &req).map_err(crate::develop::pipeline_error)?;
    let mut out = vec![0u8; size.pixels() as usize * 8];
    develop().render(loaded, adj, &req, OutputFormat::Rgba16fLinearP3, &mut out)?;
    let mut sum = [0f64; 3];
    let mut n = 0f64;
    for px in out.as_chunks::<8>().0 {
        for (c, s) in sum.iter_mut().enumerate() {
            *s += f64::from(half::f16::from_le_bytes([px[c * 2], px[c * 2 + 1]]).to_f32());
        }
        n += 1.0;
    }
    if n == 0.0 {
        return Err(DsError::invalid("empty sample"));
    }
    Ok(sum.map(|s| (s / n) as f32))
}

/// The white balance that makes the sample at `(x, y)` neutral (see [`sample_linear`]).
/// The sample is taken in the develop render (linear P3) back to white-balanced camera
/// space; the multipliers are corrected by its colour and the result re-sampled, a few
/// times, so the stages after white balance (tone, saturation) don't bias it. Those
/// stages also scale how far a correction moves the sample (a profile's colour boost
/// makes a full correction overshoot and oscillate), so after the first step each
/// channel's step is a secant on the last two samples.
fn pick(path: &Path, adj: &Adjustments, x: f32, y: f32, sample: u32) -> Result<WbValue, DsError> {
    let loaded = develop().load(path)?;
    let cam = CameraColor::of(&loaded.raw);
    let xyz_to_cam = mat_inv(&cam.cam_to_xyz).ok_or_else(|| DsError::Unsupported {
        message: "camera matrix is singular".into(),
    })?;
    let mut mults = wb_to_multipliers(adj.white_balance, &loaded.raw);
    // ln(multipliers), ln(cast) of the previous sample.
    let mut prev: Option<([f32; 3], [f32; 3])> = None;
    for _ in 0..6 {
        let wb = multipliers_to_wb(mults, &cam);
        let probe = Adjustments {
            white_balance: wb,
            ..adj.clone()
        };
        let p3 = sample_linear(&loaded, &probe, x, y, sample)?;
        let balanced = mat_vec(&xyz_to_cam, mat_vec(&P3_TO_XYZ, p3));
        if balanced.iter().any(|v| !v.is_finite() || *v <= 1e-6) {
            return Err(DsError::invalid("the picked spot is black or clipped"));
        }
        let cast = normalize_mults(balanced);
        if cast.iter().all(|c| (c - 1.0).abs() < 0.002) {
            break;
        }
        let (lm, lc) = (mults.map(f32::ln), cast.map(f32::ln));
        let step = [0, 1, 2].map(|c| {
            let full = -lc[c];
            match prev {
                Some((pm, pc)) if (lm[c] - pm[c]).abs() > 1e-4 => {
                    let slope = (lc[c] - pc[c]) / (lm[c] - pm[c]);
                    if slope > 0.05 {
                        (full / slope).clamp(-1.0, 1.0)
                    } else {
                        full
                    }
                }
                _ => full,
            }
        });
        prev = Some((lm, lc));
        mults = normalize_mults([0, 1, 2].map(|c| (lm[c] + step[c]).exp()));
    }
    Ok(wb_value(multipliers_to_wb(mults, &cam), &loaded))
}

/// As-shot Kelvin/tint, lens profile and ISO of an original.
#[uniffi::export]
pub async fn develop_info(path: String, token: Arc<CancelToken>) -> Result<DevelopInfo, DsError> {
    run_job_on_thread("ds-tools", token, move |_| info(Path::new(&path))).await
}

/// Auto white balance (grey world + white patch on the camera data, statistics on the GPU).
#[uniffi::export]
pub async fn auto_white_balance(path: String, token: Arc<CancelToken>) -> Result<WbValue, DsError> {
    run_job_on_thread("ds-tools", token, move |_| auto_wb(Path::new(&path))).await
}

/// Auto tone: `adjustments_json` with exposure, HDR and levels set from the image (the
/// probe renders run on the GPU at background priority).
#[uniffi::export]
pub async fn auto_tone(
    path: String,
    adjustments_json: String,
    token: Arc<CancelToken>,
) -> Result<String, DsError> {
    run_job_on_thread("ds-tools", token, move |_| {
        to_json(&tone(Path::new(&path), &parse(&adjustments_json)?)?)
    })
    .await
}

/// White balance picker: the Kelvin/tint that neutralizes the sample at `(x, y)`,
/// normalized in the final (cropped, turned) frame of the develop render. `sample`:
/// the side of the square of full-resolution pixels averaged, centred on the pixel
/// under the point (1, 3, 5, … ≤ [`MAX_PICK_SAMPLE`]); 0 averages 0.8% of the frame.
/// Deterministic: the same photo, edit, point and sample give the same value (the
/// viewer's loupe shows it before the click applies it).
#[uniffi::export]
pub async fn pick_white_balance(
    path: String,
    adjustments_json: String,
    x: f32,
    y: f32,
    sample: u32,
    token: Arc<CancelToken>,
) -> Result<WbValue, DsError> {
    run_job_on_thread("ds-tools", token, move |_| {
        if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
            return Err(DsError::invalid("the point is outside the image"));
        }
        pick(Path::new(&path), &parse(&adjustments_json)?, x, y, sample)
    })
    .await
}

/// Kelvin/tint of camera white-balance multipliers (R, G, B; any scale) for this camera.
#[uniffi::export]
pub async fn white_balance_from_multipliers(
    path: String,
    multipliers: Vec<f32>,
    token: Arc<CancelToken>,
) -> Result<WbValue, DsError> {
    run_job_on_thread("ds-tools", token, move |_| {
        let [r, g, b] = multipliers[..] else {
            return Err(DsError::invalid("three multipliers expected"));
        };
        let (cam, _) = camera_color(Path::new(&path))?;
        Ok(wb_of(multipliers_to_wb([r, g, b], &cam)))
    })
    .await
}

/// One lens-correction profile the pipeline knows (for the manual profile picker).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LensProfileEntry {
    pub maker: String,
    /// The model name; set it as `LensCorrection::profile` to apply this profile.
    pub model: String,
}

/// Every vendored lens, sorted by maker then model, one entry per lens: calibrations of
/// one lens on several sensors are listed once, under its longest name (usually the most
/// descriptive: "Batis 85mm f/1.8" rather than the camera's "E 85mm F1.8"); the override
/// picks the calibration for the body's crop factor.
#[uniffi::export]
pub fn lens_profiles() -> Vec<LensProfileEntry> {
    let all = ds_pipeline::lens::profiles();
    let mut out: Vec<LensProfileEntry> = all
        .iter()
        .map(|p| {
            let name = all
                .iter()
                .filter(|o| o.same_lens(p))
                .flat_map(|o| &o.aliases)
                .max_by_key(|m| (m.len(), std::cmp::Reverse(m.as_str())))
                .unwrap_or(&p.model);
            LensProfileEntry {
                maker: p.maker.clone(),
                model: name.clone(),
            }
        })
        .collect();
    out.sort_by(|a, b| (&a.maker, &a.model).cmp(&(&b.maker, &b.model)));
    out.dedup();
    out
}

/// A JPEG thumbnail of an edited photo.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DevelopThumbnail {
    pub jpeg: Vec<u8>,
    /// Served from the disk cache (no render).
    pub cached: bool,
}

/// Grid thumbnails of edited photos: pipeline renders at background priority (the
/// viewer always goes first), cached on disk (byte-capped LRU, keyed by variant, file
/// version, adjustments and `PIPELINE_VERSION`).
///
/// Renders are **bounded**: decodes and renders run on [`THUMBNAIL_RENDERS`] fixed
/// threads, newest request first; a request cancelled while queued never starts. They are
/// **cancellable** (the request carries the caller's token into the pipeline) and run at
/// `Priority::Bulk`, behind the viewer, tools, exports and prefetch. A grid scrolled past
/// a cold cache used to start one thread and one full RAW decode per cell (H3: 343
/// threads, 7.8 GB).
#[derive(uniffi::Object)]
pub struct DevelopThumbnails {
    cache: DiskCache,
    /// Adjustments hash most recently requested per variant (A6: a late render of an
    /// older edit must not delete the newer rendition).
    latest: Mutex<HashMap<u64, ds_cache::AdjHash>>,
    /// Decodes a RAW (the shared develop's loader; a synthetic one in tests).
    load: crate::export::Loader,
    /// Renditions that weren't kept on disk (a depth blur rendered before its map), by
    /// their full key: served again instead of rendering on every request (the Styles
    /// panel's tiles of one photo each have theirs), until the map arrives (another key;
    /// the photo's held ones are then dropped) or the edit changes.
    unkept: Mutex<HashMap<CacheKey, Vec<u8>>>,
}

/// At most this many unkept renditions are held (about 60 KB each).
const UNKEPT_LIMIT: usize = 256;

/// Concurrent thumbnail renders.
pub(crate) const THUMBNAIL_RENDERS: usize = 3;

/// Threads that look thumbnails up in the disk cache (hits never wait behind renders).
const THUMBNAIL_LOOKUPS: usize = 2;

fn render_pool() -> &'static WorkerPool {
    static POOL: std::sync::OnceLock<WorkerPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| WorkerPool::new("ds-thumbnail", THUMBNAIL_RENDERS))
}

fn lookup_pool() -> &'static WorkerPool {
    static POOL: std::sync::OnceLock<WorkerPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| WorkerPool::new("ds-thumbnail-cache", THUMBNAIL_LOOKUPS))
}

impl std::fmt::Debug for DevelopThumbnails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevelopThumbnails")
            .field("root", &self.cache.root())
            .finish_non_exhaustive()
    }
}

/// Identity of the original's current version (path, size, mtime, inode, ctime) as a
/// `FileHash`, so a replaced file never serves a stale thumbnail.
fn version_hash(path: &Path) -> Result<FileHash, DsError> {
    let md = std::fs::metadata(path)?;
    let key = crate::develop::image_key(path, &md);
    Ok(FileHash(*blake3::hash(&key.0.to_le_bytes()).as_bytes()))
}

pub(crate) fn encode_jpeg(rgba: &[u8], size: Size, quality: f32) -> Result<Vec<u8>, DsError> {
    let rgb: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect();
    std::panic::catch_unwind(|| -> std::io::Result<Vec<u8>> {
        let mut c = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
        c.set_size(size.width as usize, size.height as usize);
        c.set_fastest_defaults();
        c.set_quality(quality);
        let mut s = c.start_compress(Vec::new())?;
        s.write_scanlines(&rgb)?;
        s.finish()
    })
    .map_err(|_| DsError::Internal {
        message: "jpeg encoder failed".into(),
    })?
    .map_err(DsError::from)
}

/// Everything one thumbnail render needs.
struct ThumbJob<'a> {
    path: &'a Path,
    variant: u64,
    adj: &'a Adjustments,
    /// Its cache identity (`render_hash` with `inpainter`'s engine and the depth map).
    adj_hash: AdjHash,
    /// Keep the rendition: false when its depth lens blur had no depth map yet (it would
    /// otherwise stay blurless on disk after the map lands).
    persist: bool,
    /// It renders with the photo's depth map.
    with_map: bool,
    /// The removal engine the key was made with (`None`: `PatchMatch`).
    inpainter: Option<Arc<dyn ds_pipeline::Inpainter>>,
    masks: Option<Arc<dyn DevelopMaskSource>>,
    cancel: ds_pipeline::CancelToken,
}

impl DevelopThumbnails {
    fn render_thumbnail(
        &self,
        job: ThumbJob<'_>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<DevelopThumbnail, DsError> {
        let adj_hash = job.adj_hash;
        let key = CacheKey::new(job.variant, version_hash(job.path)?, adj_hash, Kind::Thumb);
        if let Some(jpeg) = self.cache.get(&key).map_err(|e| cache_error(&e))? {
            return Ok(DevelopThumbnail { jpeg, cached: true });
        }
        let is_cancelled = || cancelled() || job.cancel.is_cancelled();
        if is_cancelled() {
            return Err(DsError::Cancelled);
        }
        let d = develop();
        let loaded = (self.load)(job.path)?;
        if is_cancelled() {
            return Err(DsError::Cancelled);
        }
        let edge = Kind::Thumb.max_edge();
        let has_ai = job.adj.layers.iter().any(|l| {
            l.mask
                .shapes
                .iter()
                .any(|s| matches!(s, ds_core::adjust::MaskShape::Ai { .. }))
        });
        let req = RenderRequest {
            priority: Priority::Bulk,
            quality: Quality::Preview,
            cancel: Some(job.cancel.clone()),
            inpainter: job.inpainter,
            // M5: AI-mask layers render in the grid too (the app's Vision masks), and so
            // does a depth lens blur (the app's depth map).
            masks: job
                .masks
                .filter(|_| has_ai || needs_depth(job.adj))
                .map(|m| crate::viewer::mask_source(m)),
            ..RenderRequest::new(Size::new(edge, edge))
        };
        let size = ds_pipeline::output_size(&loaded.raw, job.adj, &req)
            .map_err(crate::develop::pipeline_error)?;
        let mut rgba = vec![0u8; size.pixels() as usize * 4];
        d.render(&loaded, job.adj, &req, OutputFormat::Rgba8, &mut rgba)?;
        let jpeg = encode_jpeg(&rgba, size, 85.0)?;
        // Older renditions of this photo (previous edits) are dead: drop them, unless a
        // newer edit was requested meanwhile (its rendition may already be cached).
        let newest = self
            .latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&job.variant)
            .is_none_or(|h| *h == adj_hash);
        let mut unkept = self.unkept.lock().unwrap_or_else(PoisonError::into_inner);
        if !job.persist {
            if unkept.len() >= UNKEPT_LIMIT {
                unkept.clear();
            }
            unkept.insert(key, jpeg.clone());
            return Ok(DevelopThumbnail {
                jpeg,
                cached: false,
            });
        }
        if job.with_map {
            unkept.retain(|k, _| k.variant != job.variant); // the photo's map is here now
        }
        drop(unkept);
        if newest {
            let _ = self.cache.invalidate_variant(job.variant);
        }
        self.cache.put(&key, &jpeg).map_err(|e| cache_error(&e))?;
        Ok(DevelopThumbnail {
            jpeg,
            cached: false,
        })
    }
}

/// Whether `adj` renders a depth lens blur (it reads the photo's depth map).
pub(crate) fn needs_depth(adj: &Adjustments) -> bool {
    let lb = &adj.effects.lens_blur;
    lb.amount > 0.0 && lb.mode == ds_core::effects::BlurMode::Depth
}

/// The depth map a thumbnail renders with.
#[derive(Debug, PartialEq)]
enum ThumbDepth {
    /// Its edit has no depth lens blur.
    NotNeeded,
    /// It has one, but the app hasn't computed the photo's map yet.
    Missing,
    /// The map's bytes.
    Map(Vec<u8>),
}

fn thumb_depth(adj: &Adjustments, masks: Option<&Arc<dyn DevelopMaskSource>>) -> ThumbDepth {
    if !needs_depth(adj) {
        return ThumbDepth::NotNeeded;
    }
    masks
        .and_then(|m| m.depth_map())
        .map_or(ThumbDepth::Missing, |d| ThumbDepth::Map(d.data))
}

fn cache_error(e: &ds_cache::CacheError) -> DsError {
    DsError::Io {
        message: e.to_string(),
    }
}

#[uniffi::export]
impl DevelopThumbnails {
    /// Open (or create) the cache at `root`, capped at `cap_bytes`.
    #[uniffi::constructor]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(root: String, cap_bytes: u64) -> Result<Arc<Self>, DsError> {
        guard(|| {
            Ok(Arc::new(Self {
                cache: DiskCache::open(&root, cap_bytes).map_err(|e| cache_error(&e))?,
                latest: Mutex::default(),
                load: Arc::new(|p: &Path| develop().load(p)),
                unkept: Mutex::default(),
            }))
        })
    }

    /// The thumbnail of `path` developed with `adjustments_json` (512 px long edge).
    /// `masks` provides AI mask bitmaps when the edit has AI-mask layers.
    pub async fn thumbnail(
        self: Arc<Self>,
        path: String,
        variant: u64,
        adjustments_json: String,
        masks: Option<Arc<dyn DevelopMaskSource>>,
        token: Arc<CancelToken>,
    ) -> Result<DevelopThumbnail, DsError> {
        let adj = parse(&adjustments_json)?;
        // One snapshot of the removal engine for the key and the render.
        let inpainter = crate::removal::inpainter();
        let engine = inpainter.as_ref().map_or_else(
            || ds_pipeline::removal::PATCHMATCH_ID.to_owned(),
            |m| m.id(),
        );
        let depth = thumb_depth(&adj, masks.as_ref());
        let persist = depth != ThumbDepth::Missing;
        let with_map = matches!(depth, ThumbDepth::Map(_));
        let map = match &depth {
            ThumbDepth::Map(d) => Some(d.as_slice()),
            _ => None,
        };
        let adj_hash = render_hash(&adj, &engine, map);
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(variant, adj_hash);
        let key = CacheKey::new(
            variant,
            version_hash(Path::new(&path))?,
            adj_hash,
            Kind::Thumb,
        );
        let me = self.clone();
        // Its own token: a token is spent when its job ends (the lookup is a small read).
        let hit = run_job_on_pool(lookup_pool(), CancelToken::new(), move |_| {
            me.cache.get(&key).map_err(|e| cache_error(&e))
        })
        .await?;
        if let Some(jpeg) = hit {
            return Ok(DevelopThumbnail { jpeg, cached: true });
        }
        if !persist {
            let held = self.unkept.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(jpeg) = held.get(&key) {
                return Ok(DevelopThumbnail {
                    jpeg: jpeg.clone(),
                    cached: true,
                });
            }
        }
        let cancel = token.pipeline_token();
        run_job_on_pool(render_pool(), token, move |cancelled| {
            self.render_thumbnail(
                ThumbJob {
                    path: Path::new(&path),
                    variant,
                    adj: &adj,
                    adj_hash,
                    persist,
                    with_map,
                    inpainter,
                    masks,
                    cancel,
                },
                cancelled,
            )
        })
        .await
    }

    /// [library] The thumbnail of `path` developed with the edit whose hash is
    /// `edit_hash` (hex, as the Library lists it), when the disk cache has it.
    /// Never renders: `None` means the caller shows the embedded preview.
    pub async fn cached_thumbnail(
        self: Arc<Self>,
        path: String,
        variant: u64,
        edit_hash: String,
        token: Arc<CancelToken>,
    ) -> Result<Option<Vec<u8>>, DsError> {
        let adj = ds_cache::AdjHash::from_hex(&edit_hash)
            .ok_or_else(|| DsError::invalid(format!("edit hash {edit_hash:?}")))?;
        run_job_on_pool(lookup_pool(), token, move |_| {
            // An original offline or gone has no current version to match.
            let Ok(original) = version_hash(Path::new(&path)) else {
                return Ok(None);
            };
            let key = CacheKey::new(variant, original, adj, Kind::Thumb);
            self.cache.get(&key).map_err(|e| cache_error(&e))
        })
        .await
    }

    /// Drop every cached rendition of `variant` (e.g. its original was replaced).
    pub fn invalidate(&self, variant: u64) -> Result<u32, DsError> {
        guard(|| {
            Ok(u32::try_from(
                self.cache
                    .invalidate_variant(variant)
                    .map_err(|e| cache_error(&e))?,
            )
            .unwrap_or(u32::MAX))
        })
    }

    /// Bytes on disk.
    pub fn total_bytes(&self) -> u64 {
        self.cache.total_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;

    fn samples() -> Vec<std::path::PathBuf> {
        ds_testdata::samples_with_ext("ARW")
    }

    fn token() -> Arc<CancelToken> {
        CancelToken::new()
    }

    fn render_mean(path: &Path, adj: &Adjustments, x: f32, y: f32) -> [f32; 3] {
        let loaded = develop().load(path).unwrap();
        sample_linear(&loaded, adj, x, y, 0).unwrap()
    }

    #[test]
    fn info_reads_as_shot_lens_profile_and_iso() {
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let i = block_on(develop_info(p.to_string_lossy().into(), token())).unwrap();
        assert!((2500.0..9000.0).contains(&i.as_shot.kelvin), "{i:?}");
        assert!(i.as_shot.tint.abs() < 60.0, "{i:?}");
        assert!(
            i.iso.is_some() && i.lens.is_some() && !i.renderer.is_empty(),
            "{i:?}"
        );
        let profile = i.lens_profile.expect("the A9 samples' lens has a profile");
        assert!(
            profile.contains("24-70") || profile.contains("FE"),
            "{profile}"
        );
    }

    #[test]
    fn auto_white_balance_and_tone_give_sensible_values() {
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let t = std::time::Instant::now();
        let wb = block_on(auto_white_balance(p.to_string_lossy().into(), token())).unwrap();
        let wb_ms = t.elapsed().as_secs_f64() * 1e3;
        assert!(
            (2000.0..15000.0).contains(&wb.kelvin) && wb.tint.abs() <= 150.0,
            "{wb:?}"
        );
        let t = std::time::Instant::now();
        let json = block_on(auto_tone(
            p.to_string_lossy().into(),
            to_json(&Adjustments::default()).unwrap(),
            token(),
        ))
        .unwrap();
        let tone_ms = t.elapsed().as_secs_f64() * 1e3;
        let a = parse(&json).unwrap();
        assert!(a.exposure.exposure_ev.abs() <= 5.0);
        assert_ne!(
            (a.exposure, a.hdr, a.levels),
            (
                Adjustments::default().exposure,
                Adjustments::default().hdr,
                Adjustments::default().levels
            ),
            "auto tone changed something"
        );
        println!(
            "TL8 auto wb {wb_ms:.1} ms, auto tone {tone_ms:.1} ms ({})",
            develop().backend()
        );
    }

    #[test]
    fn picker_makes_the_patch_neutral() {
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let adj = Adjustments::default();
        let (x, y) = (0.5, 0.35);
        let before = render_mean(&p, &adj, x, y);
        let wb = block_on(pick_white_balance(
            p.to_string_lossy().into(),
            to_json(&adj).unwrap(),
            x,
            y,
            0,
            token(),
        ))
        .unwrap();
        let picked = Adjustments {
            white_balance: WhiteBalance::Custom {
                temp_k: wb.kelvin,
                tint: wb.tint,
            },
            ..adj
        };
        let after = render_mean(&p, &picked, x, y);
        let spread = |c: [f32; 3]| {
            let m = (c[0] + c[1] + c[2]) / 3.0;
            c.iter().map(|v| (v - m).abs() / m).fold(0.0f32, f32::max)
        };
        assert!(
            spread(after) < 0.02,
            "neutral after picking: {after:?} (before {before:?}, {wb:?})"
        );
        assert!(spread(after) <= spread(before));
        // Out-of-range point.
        assert!(
            block_on(pick_white_balance(
                p.to_string_lossy().into(),
                "{}".into(),
                2.0,
                0.5,
                5,
                token()
            ))
            .is_err()
        );
    }

    #[test]
    fn pixel_boxes_hold_the_pixels_around_the_point_inside_the_frame() {
        let b = pixel_box(100, 50, 0.505, 0.5, 5);
        // Pixel (50, 25) is under the point: the box is 48..53 × 23..28.
        assert!((b.x * 100.0 - 48.0).abs() < 1e-4 && (b.y * 50.0 - 23.0).abs() < 1e-4);
        assert!((b.w * 100.0 - 5.0).abs() < 1e-4 && (b.h * 50.0 - 5.0).abs() < 1e-4);
        let one = pixel_box(100, 50, 0.999, 0.0, 1);
        assert!((one.x * 100.0 - 99.0).abs() < 1e-4 && one.y == 0.0);
        // At the edges the box shifts inside instead of shrinking.
        let corner = pixel_box(100, 50, 1.0, 1.0, 5);
        assert!((corner.x * 100.0 - 95.0).abs() < 1e-4 && (corner.y * 50.0 - 45.0).abs() < 1e-4);
        let tiny = pixel_box(3, 2, 0.5, 0.5, 5);
        assert!(
            tiny.x == 0.0
                && tiny.y == 0.0
                && (tiny.w - 1.0).abs() < 1e-6
                && (tiny.h - 1.0).abs() < 1e-6
        );
    }

    /// The sampled picker reads exactly the full-resolution pixels it names (the same
    /// pixels as a larger 1:1 render), neutralizes them, and is deterministic, so the
    /// value a loupe shows before the click is the value the click applies.
    #[test]
    fn sampled_picker_reads_the_exact_pixels_and_neutralizes_them() {
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let loaded = develop().load(&p).unwrap();
        let adj = Adjustments::default();
        let (x, y) = (0.5, 0.35);
        // Reference: a 1:1 render of the 31×31 box around the point, read at its centre.
        let native = Size::new(1 << 16, 1 << 16);
        let full =
            ds_pipeline::output_size(&loaded.raw, &adj, &RenderRequest::new(native)).unwrap();
        let req = RenderRequest {
            region: Some(pixel_box(full.width, full.height, x, y, 31)),
            quality: Quality::Full,
            priority: Priority::Tool,
            ..RenderRequest::new(native)
        };
        let size = ds_pipeline::output_size(&loaded.raw, &adj, &req).unwrap();
        assert_eq!((size.width, size.height), (31, 31), "1:1");
        let mut out = vec![0u8; size.pixels() as usize * 8];
        develop()
            .render(&loaded, &adj, &req, OutputFormat::Rgba16fLinearP3, &mut out)
            .unwrap();
        let at = |i: usize, c: usize| {
            half::f16::from_le_bytes([out[i * 8 + c * 2], out[i * 8 + c * 2 + 1]]).to_f32()
        };
        let centre = 15 * 31 + 15;
        let one = sample_linear(&loaded, &adj, x, y, 1).unwrap();
        for c in 0..3 {
            assert!(
                (one[c] - at(centre, c)).abs() <= 1e-3 * at(centre, c).max(1e-3),
                "pixel {c}: {one:?} vs {}",
                at(centre, c)
            );
        }
        let five = sample_linear(&loaded, &adj, x, y, 5).unwrap();
        for c in 0..3 {
            let mean = (13..18)
                .flat_map(|r| (13..18).map(move |col| r * 31 + col))
                .map(|i| at(i, c))
                .sum::<f32>()
                / 25.0;
            assert!(
                (five[c] - mean).abs() <= 2e-3 * mean.max(1e-3),
                "{five:?} vs {mean}"
            );
        }

        let json = to_json(&adj).unwrap();
        let path: String = p.to_string_lossy().into();
        let wb = block_on(pick_white_balance(
            path.clone(),
            json.clone(),
            x,
            y,
            5,
            token(),
        ))
        .unwrap();
        let again = block_on(pick_white_balance(path, json, x, y, 5, token())).unwrap();
        assert_eq!(wb, again, "deterministic");
        let picked = Adjustments {
            white_balance: WhiteBalance::Custom {
                temp_k: wb.kelvin,
                tint: wb.tint,
            },
            ..adj
        };
        let after = sample_linear(&loaded, &picked, x, y, 5).unwrap();
        let m = (after[0] + after[1] + after[2]) / 3.0;
        assert!(
            after.iter().all(|v| (v - m).abs() / m < 0.02),
            "neutral: {after:?} ({wb:?})"
        );
    }

    #[test]
    fn multipliers_round_trip_through_the_camera_matrix() {
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let path: String = p.to_string_lossy().into();
        let loaded = develop().load(&p).unwrap();
        let as_shot = block_on(white_balance_from_multipliers(
            path.clone(),
            loaded.raw.wb_as_shot.to_vec(),
            token(),
        ))
        .unwrap();
        let info = block_on(develop_info(path.clone(), token())).unwrap();
        assert!(
            (as_shot.kelvin - info.as_shot.kelvin).abs() < 1.0
                && (as_shot.tint - info.as_shot.tint).abs() < 0.1
        );
        assert!(block_on(white_balance_from_multipliers(path, vec![1.0], token())).is_err());
    }

    #[test]
    fn edited_thumbnails_render_then_come_from_the_disk_cache() {
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let thumbs =
            DevelopThumbnails::new(t.path().join("cache").to_string_lossy().into(), 64 << 20)
                .unwrap();
        let mut adj = Adjustments::default();
        adj.exposure.exposure_ev = 1.0;
        let json = to_json(&adj).unwrap();
        let path: String = p.to_string_lossy().into();
        let t0 = std::time::Instant::now();
        let a = block_on(
            thumbs
                .clone()
                .thumbnail(path.clone(), 7, json.clone(), None, token()),
        )
        .unwrap();
        let render_ms = t0.elapsed().as_secs_f64() * 1e3;
        let t1 = std::time::Instant::now();
        let b = block_on(
            thumbs
                .clone()
                .thumbnail(path.clone(), 7, json, None, token()),
        )
        .unwrap();
        let hit_ms = t1.elapsed().as_secs_f64() * 1e3;
        assert!(!a.cached && b.cached && a.jpeg == b.jpeg);
        assert!(a.jpeg.starts_with(&[0xFF, 0xD8]));
        assert!(thumbs.total_bytes() > 0);
        // A new edit is a new rendition and replaces the old one.
        adj.exposure.exposure_ev = -1.0;
        let c = block_on(thumbs.clone().thumbnail(
            path.clone(),
            7,
            to_json(&adj).unwrap(),
            None,
            token(),
        ))
        .unwrap();
        assert!(!c.cached && c.jpeg != a.jpeg);
        assert_eq!(thumbs.invalidate(7).unwrap(), 1);
        println!("TL8 edited thumbnail: render {render_ms:.1} ms, cache hit {hit_ms:.2} ms");
        // Errors.
        assert!(
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path, 7, "nope".into(), None, token())
            )
            .is_err()
        );
        assert!(DevelopThumbnails::new("/dev/null/x".into(), 1).is_err());
    }

    #[test]
    fn math_and_encoder_edges() {
        assert!(mat_inv(&[[0.0; 3]; 3]).is_none());
        let id = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        assert_eq!(mat_inv(&id).unwrap(), id);
        let v = mat_vec(&id, [1.0, 2.0, 3.0]);
        assert!(
            v.iter()
                .zip([1.0, 2.0, 3.0])
                .all(|(a, b)| (a - b).abs() < 1e-6)
        );
        let jpeg = encode_jpeg(&[128u8; 4 * 16], Size::new(4, 4), 80.0).unwrap();
        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
        assert!(parse("{").is_err());
        assert!(lens_profile_name(&ds_pipeline::CaptureInfo::default()).is_none());
    }

    /// H4: the as-shot readout comes from the tags (no decode, the viewer's decode and
    /// GPU BASE stay put) and equals what the decode-based path computes.
    #[test]
    fn as_shot_info_reads_tags_and_matches_the_decode() {
        let samples = samples();
        let Some(p) = samples.last() else {
            return;
        };
        let before = develop().is_cached(p);
        let i = info(p).unwrap();
        assert_eq!(develop().is_cached(p), before, "info never decodes");
        let loaded = develop().load(p).unwrap();
        let reference = wb_value(WhiteBalance::AsShot, &loaded);
        assert!(
            (i.as_shot.kelvin - reference.kelvin).abs() < 0.5
                && (i.as_shot.tint - reference.tint).abs() < 0.05,
            "{i:?} vs {reference:?}"
        );
        let t = std::time::Instant::now();
        for p in &samples {
            info(p).unwrap();
        }
        println!(
            "RND9 as-shot info from tags: {:.2} ms/photo",
            t.elapsed().as_secs_f64() * 1e3 / samples.len() as f64
        );
        assert!((wb_of(WhiteBalance::AsShot).kelvin - 6500.0).abs() < f32::EPSILON);
        assert!(info(Path::new("/nonexistent.ARW")).is_err());
    }

    /// H3: a storm of requests runs on the fixed pools (no thread per request) and a
    /// cancelled storm leaves nothing queued.
    #[test]
    fn thumbnail_storms_use_the_fixed_pool_and_cancel() {
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let thumbs =
            DevelopThumbnails::new(t.path().join("c").to_string_lossy().into(), 64 << 20).unwrap();
        let path: String = p.to_string_lossy().into();
        let tokens: Vec<_> = (0..24).map(|_| CancelToken::new()).collect();
        let futs: Vec<_> = tokens
            .iter()
            .enumerate()
            .map(|(i, tk)| {
                let mut a = Adjustments::default();
                a.exposure.exposure_ev = 0.01 * (i as f32 + 1.0);
                thumbs.clone().thumbnail(
                    path.clone(),
                    100 + i as u64,
                    to_json(&a).unwrap(),
                    None,
                    tk.clone(),
                )
            })
            .collect();
        for tk in &tokens[4..] {
            tk.cancel();
        }
        let results: Vec<_> = futs.into_iter().map(block_on).collect();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        let cancelled = results
            .iter()
            .filter(|r| matches!(r, Err(DsError::Cancelled)))
            .count();
        assert_eq!(ok + cancelled, 24, "{results:?}");
        assert!(
            cancelled >= 18,
            "cancelled requests don't render: {cancelled}"
        );
        assert_eq!(render_pool().queued(), 0);
    }

    /// H3: a cancelled request returns Cancelled (no render, nothing cached); M5: an
    /// AI-mask layer shows in the edited thumbnail when the app provides the mask;
    /// A6: a late render of an older edit doesn't delete the newer rendition.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn thumbnails_cancel_render_ai_masks_and_keep_the_newest_rendition() {
        struct Everything;
        impl DevelopMaskSource for Everything {
            fn ai_mask(
                &self,
                _: crate::viewer::DevelopAiTarget,
            ) -> Option<crate::viewer::DevelopMask> {
                Some(crate::viewer::DevelopMask {
                    width: 4,
                    height: 4,
                    data: vec![255; 16],
                    version: 0,
                })
            }
            fn ai_person_mask(
                &self,
                t: crate::viewer::DevelopAiTarget,
                _: u32,
            ) -> Option<crate::viewer::DevelopMask> {
                self.ai_mask(t)
            }
            fn depth_map(&self) -> Option<crate::viewer::DevelopMask> {
                None
            }
        }
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let thumbs =
            DevelopThumbnails::new(t.path().join("c").to_string_lossy().into(), 64 << 20).unwrap();
        let path: String = p.to_string_lossy().into();
        let dead = CancelToken::new();
        dead.cancel();
        assert!(matches!(
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path.clone(), 1, "{}".into(), None, dead)
            ),
            Err(DsError::Cancelled)
        ));
        assert_eq!(thumbs.total_bytes(), 0, "nothing rendered or cached");
        // AI layer: +2 EV on the subject.
        let mut adj = Adjustments::default();
        let mut layer = ds_core::adjust::Layer::default();
        layer.adjustments.exposure.exposure_ev = 2.0;
        layer.mask.shapes.push(ds_core::adjust::MaskShape::Ai {
            op: ds_core::adjust::MaskOp::Add,
            target: ds_core::adjust::AiTarget::Subject,
            feather: 0.0,
            person: None,
        });
        adj.layers.push(layer);
        let json = to_json(&adj).unwrap();
        let plain =
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path.clone(), 2, json.clone(), None, token()),
            )
            .unwrap();
        let t2 = tempfile::tempdir().unwrap();
        let other =
            DevelopThumbnails::new(t2.path().join("c").to_string_lossy().into(), 64 << 20).unwrap();
        let masked = block_on(other.clone().thumbnail(
            path.clone(),
            2,
            json,
            Some(Arc::new(Everything)),
            token(),
        ))
        .unwrap();
        let mean = |jpeg: &[u8]| {
            let mut d = mozjpeg::Decompress::new_mem(jpeg).unwrap().rgb().unwrap();
            let px: Vec<[u8; 3]> = d.read_scanlines().unwrap();
            px.iter().map(|p| f64::from(p[1])).sum::<f64>() / px.len() as f64
        };
        assert!(
            mean(&masked.jpeg) > mean(&plain.jpeg) + 10.0,
            "the AI-mask layer brightens the thumbnail: {} vs {}",
            mean(&masked.jpeg),
            mean(&plain.jpeg)
        );
        // A6: request edit B (newest), then a late render of edit A must not remove B.
        let mut a = Adjustments::default();
        a.exposure.exposure_ev = 0.5;
        let mut b = Adjustments::default();
        b.exposure.exposure_ev = -0.5;
        let got_b = block_on(thumbs.clone().thumbnail(
            path.clone(),
            3,
            to_json(&b).unwrap(),
            None,
            token(),
        ))
        .unwrap();
        // The older edit A rendered late (as if its request had been in flight).
        let late = thumbs
            .render_thumbnail(
                ThumbJob {
                    path: &p,
                    variant: 3,
                    adj: &a,
                    adj_hash: ds_cache::adjustments_hash(&a),
                    persist: true,
                    with_map: false,
                    inpainter: None,
                    masks: None,
                    cancel: ds_pipeline::CancelToken::new(),
                },
                &|| false,
            )
            .unwrap();
        assert!(!late.cached);
        let again =
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path, 3, to_json(&b).unwrap(), None, token()),
            )
            .unwrap();
        assert!(
            again.cached && again.jpeg == got_b.jpeg,
            "B's rendition survived"
        );
    }

    /// A mask source with only a depth map (`None`: not computed yet).
    struct DepthOnly(Option<Vec<u8>>);
    impl DevelopMaskSource for DepthOnly {
        fn ai_mask(&self, _: crate::viewer::DevelopAiTarget) -> Option<crate::viewer::DevelopMask> {
            None
        }
        fn ai_person_mask(
            &self,
            _: crate::viewer::DevelopAiTarget,
            _: u32,
        ) -> Option<crate::viewer::DevelopMask> {
            None
        }
        fn depth_map(&self) -> Option<crate::viewer::DevelopMask> {
            self.0.clone().map(|data| crate::viewer::DevelopMask {
                width: 2,
                height: 1,
                data,
                version: 0,
            })
        }
    }

    fn depth_blur() -> Adjustments {
        let mut adj = Adjustments::default();
        adj.effects.lens_blur.amount = 80.0;
        adj
    }

    #[test]
    fn a_thumbnail_reads_the_depth_map_only_for_a_depth_lens_blur() {
        let ready: Arc<dyn DevelopMaskSource> = Arc::new(DepthOnly(Some(vec![0, 255])));
        let pending: Arc<dyn DevelopMaskSource> = Arc::new(DepthOnly(None));
        let blur = depth_blur();
        assert!(needs_depth(&blur));
        assert_eq!(
            thumb_depth(&blur, Some(&ready)),
            ThumbDepth::Map(vec![0, 255])
        );
        assert_eq!(
            thumb_depth(&blur, Some(&pending)),
            ThumbDepth::Missing,
            "needed, not there yet"
        );
        assert_eq!(thumb_depth(&blur, None), ThumbDepth::Missing);
        let mut plane = depth_blur();
        plane.effects.lens_blur.mode = ds_core::effects::BlurMode::Plane;
        assert!(!needs_depth(&plane));
        assert_eq!(
            thumb_depth(&plane, Some(&ready)),
            ThumbDepth::NotNeeded,
            "tilt-shift needs no depth"
        );
        assert_eq!(
            thumb_depth(&Adjustments::default(), Some(&ready)),
            ThumbDepth::NotNeeded,
            "no blur"
        );
    }

    /// Thumbnails whose RAWs all decode to a synthetic frame (no samples needed).
    fn synth_thumbnails(root: &Path) -> Arc<DevelopThumbnails> {
        let raw = crate::export::tests::synth_raw(96, 64);
        Arc::new(DevelopThumbnails {
            cache: DiskCache::open(root, 64 << 20).unwrap(),
            latest: Mutex::default(),
            load: Arc::new(move |_: &Path| Ok(raw.clone())),
            unkept: Mutex::default(),
        })
    }

    /// [library] A developed thumbnail is found again by the Library's edit hash
    /// (`ds_session::edit_hash`, the cache's `adjustments_hash`) without rendering.
    #[test]
    fn the_library_finds_developed_thumbnails_by_edit_hash() {
        let t = tempfile::tempdir().unwrap();
        let thumbs = synth_thumbnails(&t.path().join("c"));
        let raw = t.path().join("L.ARW");
        std::fs::write(&raw, b"synthetic").unwrap();
        let path: String = raw.to_string_lossy().into();
        let json = r#"{"exposure":{"exposure_ev":0.5}}"#.to_string();
        let adj = parse(&json).unwrap();
        let hash = blake3::Hash::from_bytes(ds_session::edit_hash(&adj))
            .to_hex()
            .to_string();
        assert_eq!(hash, ds_cache::adjustments_hash(&adj).to_hex());
        let lookup = |variant: u64, hash: &str, p: &str| {
            block_on(
                thumbs
                    .clone()
                    .cached_thumbnail(p.into(), variant, hash.into(), token()),
            )
        };
        assert_eq!(lookup(7, &hash, &path).unwrap(), None, "not rendered yet");
        let rendered = block_on(
            thumbs
                .clone()
                .thumbnail(path.clone(), 7, json, None, token()),
        )
        .unwrap();
        assert_eq!(lookup(7, &hash, &path).unwrap(), Some(rendered.jpeg));
        assert_eq!(lookup(8, &hash, &path).unwrap(), None, "another variant");
        assert_eq!(
            lookup(7, &"0".repeat(64), &path).unwrap(),
            None,
            "another edit"
        );
        let gone = t.path().join("gone.ARW").to_string_lossy().into_owned();
        assert_eq!(lookup(7, &hash, &gone).unwrap(), None, "offline original");
        assert!(matches!(
            lookup(7, "zz", &path),
            Err(DsError::InvalidArgument { .. })
        ));
    }

    /// A mask source whose depth map is near in the middle, far around it.
    struct CenterNear;
    impl DevelopMaskSource for CenterNear {
        fn ai_mask(&self, _: crate::viewer::DevelopAiTarget) -> Option<crate::viewer::DevelopMask> {
            None
        }
        fn ai_person_mask(
            &self,
            _: crate::viewer::DevelopAiTarget,
            _: u32,
        ) -> Option<crate::viewer::DevelopMask> {
            None
        }
        fn depth_map(&self) -> Option<crate::viewer::DevelopMask> {
            Some(crate::viewer::DevelopMask {
                width: 8,
                height: 8,
                data: (0..64)
                    .map(|i| {
                        if (2..6).contains(&(i % 8)) && (2..6).contains(&(i / 8)) {
                            255
                        } else {
                            0
                        }
                    })
                    .collect(),
                version: 42,
            })
        }
    }

    /// The review's H1 repro, on a synthetic frame: a depth blur thumbnail asked for
    /// before the map existed is not served from the cache once the map is there.
    #[test]
    fn a_thumbnail_without_depth_is_not_reused_once_depth_exists_synthetic() {
        let t = tempfile::tempdir().unwrap();
        let thumbs = synth_thumbnails(&t.path().join("c"));
        let raw = t.path().join("S.ARW");
        std::fs::write(&raw, b"synthetic").unwrap();
        let path: String = raw.to_string_lossy().into();
        let json = r#"{"effects":{"lens_blur":{"amount":100}}}"#.to_string();
        let before =
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path.clone(), 1, json.clone(), None, token()),
            )
            .unwrap();
        assert_eq!(thumbs.total_bytes(), 0, "not kept without the map");
        // Review 6: asking again before the map doesn't render again (held in memory).
        let again_before =
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path.clone(), 1, json.clone(), None, token()),
            )
            .unwrap();
        assert!(again_before.cached && again_before.jpeg == before.jpeg);
        let other_edit = r#"{"effects":{"lens_blur":{"amount":90}}}"#.to_string();
        assert!(
            !block_on(
                thumbs
                    .clone()
                    .thumbnail(path.clone(), 1, other_edit, None, token())
            )
            .unwrap()
            .cached,
            "another edit renders"
        );
        // Low 4: held by the full key, so the photo's tiles (other edits) don't evict
        // each other: both are served again without rendering.
        let other_edit = r#"{"effects":{"lens_blur":{"amount":90}}}"#.to_string();
        assert!(
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path.clone(), 1, other_edit, None, token())
            )
            .unwrap()
            .cached
        );
        assert!(
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path.clone(), 1, json.clone(), None, token())
            )
            .unwrap()
            .cached,
            "the first tile is still held"
        );
        let after = block_on(thumbs.clone().thumbnail(
            path.clone(),
            1,
            json.clone(),
            Some(Arc::new(CenterNear)),
            token(),
        ))
        .unwrap();
        assert!(
            !after.cached,
            "the sharp thumbnail isn't served after the map arrived"
        );
        assert_ne!(before.jpeg, after.jpeg, "the map blurs it");
        let again =
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path, 1, json, Some(Arc::new(CenterNear)), token()),
            )
            .unwrap();
        assert!(
            again.cached && again.jpeg == after.jpeg,
            "kept with the map"
        );
        assert!(
            thumbs.unkept.lock().unwrap().keys().all(|k| k.variant != 1),
            "the held blurless one is gone once the map came"
        );
    }

    /// The review's H1 repro on a real ARW.
    #[test]
    fn review_thumbnail_cached_without_depth_is_reused_once_depth_exists() {
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let thumbs =
            DevelopThumbnails::new(t.path().join("c").to_string_lossy().into(), 64 << 20).unwrap();
        let path: String = p.to_string_lossy().into();
        let json = r#"{"effects":{"lens_blur":{"amount":100}}}"#.to_string();
        let before =
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path.clone(), 1, json.clone(), None, token()),
            )
            .unwrap();
        let after =
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path, 1, json, Some(Arc::new(CenterNear)), token()),
            )
            .unwrap();
        assert!(
            !after.cached,
            "the depth-less (sharp) thumbnail was served from cache after the depth map arrived"
        );
        assert_ne!(before.jpeg, after.jpeg);
    }

    /// H1: a depth lens blur rendered before its depth map exists isn't kept (it would
    /// stay blurless for good); with the map it is, keyed by the map.
    #[test]
    fn a_depth_blur_thumbnail_without_its_map_is_not_cached() {
        let Some(p) = samples().into_iter().next() else {
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let thumbs =
            DevelopThumbnails::new(t.path().join("c").to_string_lossy().into(), 64 << 20).unwrap();
        let path: String = p.to_string_lossy().into();
        let json = to_json(&depth_blur()).unwrap();
        let get = |masks: Option<Arc<dyn DevelopMaskSource>>| {
            block_on(
                thumbs
                    .clone()
                    .thumbnail(path.clone(), 7, json.clone(), masks, token()),
            )
            .unwrap()
        };
        let pending = get(Some(Arc::new(DepthOnly(None))));
        assert!(!pending.cached);
        assert_eq!(thumbs.total_bytes(), 0, "not kept without the map");
        let held = get(None);
        assert!(
            held.cached && held.jpeg == pending.jpeg && thumbs.total_bytes() == 0,
            "held in memory, still not on disk"
        );
        let near_far = || -> Option<Arc<dyn DevelopMaskSource>> {
            Some(Arc::new(DepthOnly(Some(vec![255, 0]))))
        };
        let with_map = get(near_far());
        assert!(!with_map.cached && thumbs.total_bytes() > 0);
        assert!(get(near_far()).cached, "kept with the map");
        let other = get(Some(Arc::new(DepthOnly(Some(vec![0, 255])))));
        assert!(
            !other.cached,
            "another map (the model replacing the fallback) renders again"
        );
        assert_ne!(with_map.jpeg, pending.jpeg, "the map blurs the thumbnail");
    }

    /// The manual picker lists every vendored profile once, sorted, and a listed model
    /// name is accepted back as the profile override.
    #[test]
    fn lens_profiles_are_listed_sorted_and_usable_as_overrides() {
        let all = lens_profiles();
        assert_ne!(all.len(), 0);
        assert!(
            all.windows(2)
                .all(|w| (&w[0].maker, &w[0].model) < (&w[1].maker, &w[1].model)),
            "sorted and de-duplicated"
        );
        assert!(all.iter().any(|p| p.model.contains("24-70")), "{all:?}");
        // Third-party E-mount lenses are listed under their makers (review N11).
        let makers: std::collections::BTreeSet<&str> =
            all.iter().map(|p| p.maker.as_str()).collect();
        assert_eq!(
            makers.into_iter().collect::<Vec<_>>(),
            ["Samyang", "Sigma", "Sony", "Tamron", "Tokina", "Zeiss"]
        );
        assert!(
            all.iter()
                .any(|p| p.maker == "Sigma" && p.model == "24-70mm F2.8 DG DN | Art 019")
        );
        // One entry per lens: the Sigma 30mm F2.8 DN, calibrated on APS-C (EXIF name
        // "E 30mm f/2.8") and on Micro Four Thirds, is listed once.
        assert!(!all.iter().any(|p| p.model == "E 30mm f/2.8"));
        assert!(all.iter().any(|p| p.model == "Sigma 30mm f/2.8 EX DN"));
        assert!(all.iter().any(|p| p.model == "Batis 85mm f/1.8"));
        // Every listed name resolves back to that lens, at the calibration nearest the
        // body's crop factor.
        let profiles = ds_pipeline::lens::profiles();
        for p in &all {
            for crop in [1.0, 1.5, 2.0] {
                let found = ds_pipeline::lens::find(&p.model, crop).expect(&p.model);
                assert_eq!(found.maker, p.maker, "{}", p.model);
                let calibrations: Vec<_> = profiles.iter().filter(|o| o.same_lens(found)).collect();
                assert!(
                    calibrations.iter().any(|o| o.aliases.contains(&p.model)),
                    "{} is that lens",
                    p.model
                );
                assert!(
                    calibrations
                        .iter()
                        .all(|o| (o.crop_factor - crop).abs() >= (found.crop_factor - crop).abs()),
                    "{} at {crop}",
                    p.model
                );
            }
        }
    }
}
