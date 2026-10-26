//! Live develop renders for the viewer (and anything else that shows a developed
//! photo): plan a frame, then render it through the shared [`crate::develop`] context
//! straight into caller memory (the app's Metal buffer).
//!
//! - [`plan_develop`] decodes the RAW (cached) and returns the output size plus the map
//!   from the final frame to crop/mask space, so the app sizes its buffer and places
//!   overlays before a pixel is rendered.
//! - [`render_develop`] renders into `dest_ptr..dest_ptr+dest_len` and returns the size
//!   and (optionally) the output's display histogram with clipping counts: the app never
//!   bins pixels on the CPU.
//! - [`prefetch_develop`] warms the decode and the GPU BASE for a neighbour or for the
//!   pan slack around a 1:1 region, at background priority.
//! - `geometry: Oriented` renders the corrected E frame (no crop, straighten, turns or
//!   flips; keystone and lens corrections kept): what the crop and straighten tools edit.
//!
//! Every call goes through [`crate::jobs`]: off the calling thread, cancellable with a
//! [`CancelToken`] (linked into the pipeline, which stops between GPU passes), panic-safe.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lt_core::adjust::AiTarget;
use lt_core::{Adjustments, NormRect as CoreRect, Size};
use lt_pipeline::{MaskBitmap, OutputFormat, OutputSpace, Priority, Quality, RenderRequest};

use crate::develop::{Loaded, develop};
use crate::error::DsError;
use crate::frame::NormRect;
use crate::jobs::{CancelToken, guard, run_job_on_thread};

/// Speed/quality of a develop render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DevelopQuality {
    /// Slider drags: develops at ≤ 2560 px and upscales (no sharpening).
    Draft,
    /// The settled view.
    Preview,
    /// Full sensor demosaic (1:1 refine, export).
    Full,
}

/// Scheduling class on the shared GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DevelopPriority {
    /// The viewer: served before background work.
    Interactive,
    /// Thumbnails, prefetch, preset previews, exports.
    Background,
}

/// Pixel format and color space of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DevelopFormat {
    /// 8-bit RGBA, sRGB.
    Rgba8Srgb,
    /// 8-bit RGBA, Display P3 (sRGB transfer).
    Rgba8DisplayP3,
    /// Half-float RGBA, linear Display P3, extended range (EDR).
    Rgba16fLinearP3,
    /// Half-float RGBA, linear Rec.2020 (the working gamut, nothing clipped to P3), for
    /// the soft-proofing viewer.
    Rgba16fLinearRec2020,
}

impl DevelopFormat {
    fn pipeline(self) -> (OutputFormat, OutputSpace) {
        match self {
            Self::Rgba8Srgb => (OutputFormat::Rgba8, OutputSpace::Srgb),
            Self::Rgba8DisplayP3 => (OutputFormat::Rgba8, OutputSpace::DisplayP3),
            Self::Rgba16fLinearP3 => (OutputFormat::Rgba16fLinearP3, OutputSpace::DisplayP3),
            Self::Rgba16fLinearRec2020 => (OutputFormat::Rgba16fLinearP3, OutputSpace::Rec2020),
        }
    }

    pub fn bytes_per_pixel(self) -> u64 {
        self.pipeline().0.bytes_per_pixel() as u64
    }
}

/// Which frame to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DevelopGeometry {
    /// The final output: cropped, straightened, turned and flipped (what export writes).
    Final,
    /// The corrected E frame (keystone and lens applied, no crop/straighten/turns/flips):
    /// the canvas of the crop and straighten tools.
    Oriented,
}

/// What to render.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DevelopRequest {
    /// Bounding box of the output: the whole frame, or the region when one is set. The
    /// output is fit inside and never upscaled past 1:1 (so a region rendered at 1:1 asks
    /// for `region × DevelopPlan::native_width/height` or more).
    pub max_width: u32,
    pub max_height: u32,
    /// Normalized sub-rectangle of the frame to render (`None` = whole frame).
    pub region: Option<NormRect>,
    pub quality: DevelopQuality,
    pub priority: DevelopPriority,
    pub format: DevelopFormat,
    /// Also return the output's display histogram.
    pub histogram: bool,
    pub geometry: DevelopGeometry,
}

/// Layout of a frame, before rendering it.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DevelopPlan {
    pub width: u32,
    pub height: u32,
    pub bytes_per_row: u64,
    /// Size of the whole frame at 1:1 (one output pixel per corrected-sensor pixel).
    pub native_width: u32,
    pub native_height: u32,
    /// Affine from frame-normalized coordinates (0..1 across the whole frame) to crop/mask
    /// space (E-normalized): `[m00, m01, m02, m10, m11, m12]`.
    pub frame_to_crop_space: Vec<f64>,
    /// Pixel size of the corrected E frame (sensor pixels).
    pub source_width: u32,
    pub source_height: u32,
}

/// Display histogram of a rendered frame (8-bit codes in its color space).
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DevelopHistogram {
    pub red: Vec<u32>,
    pub green: Vec<u32>,
    pub blue: Vec<u32>,
    pub luma: Vec<u32>,
    /// Pixels with any channel at 0 / at 255.
    pub clipped_shadows: u64,
    pub clipped_highlights: u64,
    pub total: u64,
}

/// What a render produced.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DevelopFrame {
    pub width: u32,
    pub height: u32,
    pub histogram: Option<DevelopHistogram>,
}

/// The renderer in use.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DevelopBackend {
    /// "Apple M4 Pro" or "CPU rendering (…)".
    pub name: String,
    pub gpu: bool,
}

/// AI mask target (mirrors `lt_core::adjust::AiTarget`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DevelopAiTarget {
    Subject,
    Sky,
    Person,
    Background,
    /// People sub-parts (per person, see [`DevelopMaskSource::ai_person_mask`]).
    Face,
    Skin,
    Eyes,
    Teeth,
    Hair,
    Clothes,
}

impl From<AiTarget> for DevelopAiTarget {
    fn from(t: AiTarget) -> Self {
        match t {
            AiTarget::Subject => Self::Subject,
            AiTarget::Sky => Self::Sky,
            AiTarget::Person => Self::Person,
            AiTarget::Background => Self::Background,
            AiTarget::Face => Self::Face,
            AiTarget::Skin => Self::Skin,
            AiTarget::Eyes => Self::Eyes,
            AiTarget::Teeth => Self::Teeth,
            AiTarget::Hair => Self::Hair,
            AiTarget::Clothes => Self::Clothes,
        }
    }
}

/// An AI mask bitmap in crop/mask space (E-normalized, row-major, 0..255).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DevelopMask {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// Identity of this content, new for every regenerated mask (the pipeline caches
    /// the mask's raster by it); 0 = unknown, the pipeline hashes `data` instead.
    pub version: u64,
}

/// The pipeline's bitmap of an app mask; `None` when malformed (that AI shape is
/// then empty).
pub(crate) fn mask_bitmap(m: DevelopMask) -> Option<Arc<MaskBitmap>> {
    let size = Size::new(m.width, m.height);
    (m.width > 0 && m.height > 0 && m.data.len() == size.pixels() as usize).then(|| {
        Arc::new(MaskBitmap {
            version: m.version,
            ..MaskBitmap::new(size, m.data)
        })
    })
}

/// AI masks for a render, provided by the app (Apple Vision). Called from a render
/// thread; return `None` when the mask isn't available (the AI shape is then empty).
#[uniffi::export(with_foreign)]
pub trait DevelopMaskSource: Send + Sync {
    /// The target over the whole image (people targets: all people).
    fn ai_mask(&self, target: DevelopAiTarget) -> Option<DevelopMask>;
    /// A people target for one person (index in the app's person order, largest first).
    fn ai_person_mask(&self, target: DevelopAiTarget, person: u32) -> Option<DevelopMask>;
    /// The photo's depth map for the lens blur (E frame, 255 = nearest), or `None`
    /// while it isn't computed (the depth lens blur then renders nothing).
    fn depth_map(&self) -> Option<DevelopMask>;
}

/// Adapter from the foreign trait to the pipeline's `MaskSource`.
struct Masks(Arc<dyn DevelopMaskSource>);

impl lt_pipeline::MaskSource for Masks {
    fn ai_mask(&self, target: AiTarget) -> Option<Arc<MaskBitmap>> {
        mask_bitmap(self.0.ai_mask(target.into())?)
    }

    fn ai_person_mask(&self, target: AiTarget, person: Option<u32>) -> Option<Arc<MaskBitmap>> {
        match person {
            None => self.ai_mask(target),
            Some(p) => mask_bitmap(self.0.ai_person_mask(target.into(), p)?),
        }
    }

    fn depth_map(&self) -> Option<Arc<MaskBitmap>> {
        mask_bitmap(self.0.depth_map()?)
    }
}

/// The pipeline's `MaskSource` over the app's provider (viewer, thumbnails, export).
pub(crate) fn mask_source(m: Arc<dyn DevelopMaskSource>) -> Arc<dyn lt_pipeline::MaskSource> {
    Arc::new(Masks(m))
}

/// Validated caller destination (see [`render_develop`]'s contract).
#[derive(Clone, Copy)]
struct Dest {
    addr: usize,
    len: usize,
}

impl Dest {
    fn new(ptr: u64, len: u64) -> Result<Self, DsError> {
        if ptr == 0 {
            return Err(DsError::invalid("destination pointer is null"));
        }
        let addr = usize::try_from(ptr).map_err(|_| DsError::invalid("pointer out of range"))?;
        let len = usize::try_from(len)
            .ok()
            .filter(|&l| isize::try_from(l).is_ok())
            .ok_or_else(|| DsError::invalid("destination length out of range"))?;
        Ok(Self { addr, len })
    }

    /// # Safety
    /// [`render_develop`]'s contract: `addr..addr+len` is valid writable memory that
    /// nothing else touches until the call's future completes or is dropped (`run_job`
    /// waits for the worker before either happens).
    #[allow(unsafe_code)]
    unsafe fn slice<'a>(self) -> &'a mut [u8] {
        // SAFETY: non-null and len ≤ isize::MAX were checked in `new`; u8 needs no
        // alignment; validity and exclusivity are the caller's documented contract.
        unsafe { std::slice::from_raw_parts_mut(self.addr as *mut u8, self.len) }
    }
}

fn parse(json: &str) -> Result<Adjustments, DsError> {
    serde_json::from_str(json).map_err(|e| DsError::invalid(format!("adjustments JSON: {e}")))
}

/// The adjustments actually rendered for `geometry`.
fn for_geometry(mut adj: Adjustments, geometry: DevelopGeometry) -> Adjustments {
    if geometry == DevelopGeometry::Oriented {
        // Just under full: a full rect with lens/keystone corrections would auto-fit
        // (shrink); the crop tool shows the whole corrected frame instead.
        adj.crop.rect = CoreRect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 0.999_999,
        };
        adj.crop.angle_deg = 0.0;
        adj.crop.quarter_turns = 0;
        adj.crop.flip_h = false;
        adj.crop.flip_v = false;
    }
    adj
}

fn pipeline_request(
    r: &DevelopRequest,
    loaded: &Loaded,
    masks: Option<Arc<dyn DevelopMaskSource>>,
) -> Result<RenderRequest, DsError> {
    if r.max_width == 0 || r.max_height == 0 {
        return Err(DsError::invalid("render size is 0"));
    }
    let region = match r.region {
        Some(g) if !(g.w > 0.0 && g.h > 0.0 && g.x >= 0.0 && g.y >= 0.0) => {
            return Err(DsError::invalid("empty or negative region"));
        }
        Some(g) => Some(CoreRect {
            x: g.x,
            y: g.y,
            w: g.w,
            h: g.h,
        }),
        None => None,
    };
    let (_, output) = r.format.pipeline();
    Ok(RenderRequest {
        max_size: Size::new(r.max_width, r.max_height),
        region,
        quality: match r.quality {
            DevelopQuality::Draft => Quality::Draft,
            DevelopQuality::Preview => Quality::Preview,
            DevelopQuality::Full => Quality::Full,
        },
        output,
        masks: masks.map(mask_source),
        cancel: None,
        capture: loaded.capture.clone(),
        priority: match r.priority {
            DevelopPriority::Interactive => Priority::Interactive,
            DevelopPriority::Background => Priority::Background,
        },
        histogram: r.histogram,
        output_sharpening: None,
        ..RenderRequest::default()
    })
}

fn plan(path: &Path, adj: &Adjustments, r: &DevelopRequest) -> Result<DevelopPlan, DsError> {
    let loaded = develop().load(path)?;
    let adj = for_geometry(adj.clone(), r.geometry);
    let req = pipeline_request(r, &loaded, None)?;
    let size = lt_pipeline::output_size(&loaded.raw, &adj, &req)
        .map_err(crate::develop::pipeline_error)?;
    let (native, m) = lt_pipeline::frame_mapping(
        &loaded.raw,
        &adj,
        &RenderRequest {
            region: None,
            max_size: Size::new(1 << 20, 1 << 20),
            ..req
        },
    )
    .map_err(crate::develop::pipeline_error)?;
    let (w, h) = match loaded.raw.default_crop {
        Some([_, _, cw, ch]) if cw >= 2 && ch >= 2 => (cw, ch),
        _ => (loaded.raw.size.width, loaded.raw.size.height),
    };
    let e = loaded.raw.orientation.apply_to(Size::new(w, h));
    Ok(DevelopPlan {
        width: size.width,
        height: size.height,
        bytes_per_row: u64::from(size.width) * r.format.bytes_per_pixel(),
        native_width: native.width,
        native_height: native.height,
        frame_to_crop_space: m.to_vec(),
        source_width: e.width,
        source_height: e.height,
    })
}

fn histogram(h: &lt_pipeline::Histogram) -> DevelopHistogram {
    DevelopHistogram {
        red: h.r.to_vec(),
        green: h.g.to_vec(),
        blue: h.b.to_vec(),
        luma: h.l.to_vec(),
        clipped_shadows: h.clipped_shadows,
        clipped_highlights: h.clipped_highlights,
        total: h.total,
    }
}

/// Plan a render of `path` with `adjustments_json` (decodes the RAW on first use).
#[uniffi::export]
pub async fn plan_develop(
    path: String,
    adjustments_json: String,
    request: DevelopRequest,
    token: Arc<CancelToken>,
) -> Result<DevelopPlan, DsError> {
    let adj = parse(&adjustments_json)?;
    run_job_on_thread("lt-develop-plan", token, move |_| {
        plan(Path::new(&path), &adj, &request)
    })
    .await
}

/// Render `path` with `adjustments_json` into caller memory at `dest_ptr`
/// (`dest_len` bytes, at least `bytes_per_row × height` of [`plan_develop`] for the same
/// arguments), row-major and tightly packed.
///
/// **Contract:** the memory stays valid and untouched by the caller until this call
/// returns. Cancel with `token`: the render stops between GPU passes and the call
/// returns `DsError::Cancelled`.
#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub async fn render_develop(
    path: String,
    adjustments_json: String,
    request: DevelopRequest,
    dest_ptr: u64,
    dest_len: u64,
    masks: Option<Arc<dyn DevelopMaskSource>>,
    token: Arc<CancelToken>,
) -> Result<DevelopFrame, DsError> {
    let adj = parse(&adjustments_json)?;
    let dest = Dest::new(dest_ptr, dest_len)?;
    let cancel = token.pipeline_token();
    run_job_on_thread("lt-develop", token, move |_| {
        let loaded = develop().load(Path::new(&path))?;
        let adj = for_geometry(adj, request.geometry);
        let mut req = pipeline_request(&request, &loaded, masks)?;
        req.cancel = Some(cancel);
        let (format, _) = request.format.pipeline();
        // SAFETY: see `Dest::slice`; `run_job_on_thread` does not return (or drop)
        // before this closure has finished with the slice.
        #[allow(unsafe_code)]
        let out = unsafe { dest.slice() };
        let f = develop().render(&loaded, &adj, &req, format, out)?;
        Ok(DevelopFrame {
            width: f.size.width,
            height: f.size.height,
            histogram: f.histogram.as_ref().map(histogram),
        })
    })
    .await
}

/// Warm the decode and the GPU BASE a render of `path` will need (a neighbour on
/// navigation, or the slack around a 1:1 region while panning), in the background.
/// Returns at once; errors are ignored (the real render reports them).
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI passes owned strings
pub fn prefetch_develop(
    path: String,
    adjustments_json: String,
    request: DevelopRequest,
) -> Result<(), DsError> {
    guard(|| {
        let adj = parse(&adjustments_json)?;
        let path = PathBuf::from(path);
        let (generation, cancel) = register_prefetch(&path);
        std::thread::Builder::new()
            .name("lt-develop-prefetch".into())
            .spawn(move || {
                let _ = prefetch(&path, &adj, &request, &cancel);
                finish_prefetch(&path, generation);
            })
            .map(|_| ())
            .map_err(|e| DsError::Internal {
                message: e.to_string(),
            })
    })
}

/// Cancel every prefetch except those of `keep` (the new neighbourhood after the
/// user navigated): queued ones never start, running ones stop between GPU passes.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI passes owned vectors
pub fn cancel_develop_prefetches(keep: Vec<String>) {
    let keep: std::collections::HashSet<PathBuf> = keep.into_iter().map(PathBuf::from).collect();
    lock_prefetching().retain(|p, (_, token)| {
        let kept = keep.contains(p);
        if !kept {
            token.cancel();
        }
        kept
    });
}

/// Keep `path`'s decode cached whatever else is decoded (the photo on screen: a long
/// export or a thumbnail storm must not push it out), or release the pin with `None`.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI passes owned values
pub fn pin_develop(path: Option<String>) {
    develop().pin(path.as_deref().map(Path::new));
}

/// Prefetches in flight: path → (generation, cancel token).
type Prefetches = std::collections::HashMap<PathBuf, (u64, lt_pipeline::CancelToken)>;

/// At most this many prefetches are outstanding; older ones are cancelled.
const MAX_PREFETCHES: usize = 6;

/// A new prefetch of `path` supersedes the one in flight for it (a wider pan slack is
/// never dropped: the older, narrower one is cancelled instead).
fn register_prefetch(path: &Path) -> (u64, lt_pipeline::CancelToken) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let generation = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let token = lt_pipeline::CancelToken::new();
    let mut map = lock_prefetching();
    if let Some((_, old)) = map.insert(path.to_path_buf(), (generation, token.clone())) {
        old.cancel();
    }
    while map.len() > MAX_PREFETCHES {
        let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (g, _))| *g)
            .map(|(p, _)| p.clone())
        else {
            break;
        };
        if let Some((_, t)) = map.remove(&oldest) {
            t.cancel();
        }
    }
    (generation, token)
}

fn finish_prefetch(path: &Path, generation: u64) {
    let mut map = lock_prefetching();
    if map.get(path).is_some_and(|(g, _)| *g == generation) {
        map.remove(path);
    }
}

fn lock_prefetching() -> std::sync::MutexGuard<'static, Prefetches> {
    static IN_FLIGHT: std::sync::OnceLock<std::sync::Mutex<Prefetches>> =
        std::sync::OnceLock::new();
    IN_FLIGHT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn prefetch(
    path: &Path,
    adj: &Adjustments,
    r: &DevelopRequest,
    cancel: &lt_pipeline::CancelToken,
) -> Result<bool, DsError> {
    if cancel.is_cancelled() {
        return Err(DsError::Cancelled);
    }
    let loaded = develop().load(path)?;
    if cancel.is_cancelled() {
        return Err(DsError::Cancelled);
    }
    let Some(gpu) = develop().gpu() else {
        return Ok(false); // the decode is what the CPU renderer can warm
    };
    let adj = for_geometry(adj.clone(), r.geometry);
    let mut req = pipeline_request(r, &loaded, None)?;
    req.cancel = Some(cancel.clone());
    req.inpainter = crate::removal::inpainter();
    if crate::denoise::wanted(&adj) {
        // AI Denoise renders a blend made on first use (only the photo on screen's are
        // kept): warm its denoised result instead, so moving there skips the disk read.
        return Ok(crate::denoise::warm(&loaded, &adj));
    }
    gpu.prefetch(loaded.key, &loaded.raw, &adj, &req)
        .map_err(crate::develop::pipeline_error)
}

/// Which renderer the develop renders run on.
#[uniffi::export]
pub fn develop_backend() -> Result<DevelopBackend, DsError> {
    guard(|| {
        let d = develop();
        Ok(DevelopBackend {
            name: d.backend(),
            gpu: d.gpu().is_some(),
        })
    })
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    fn request(n: u32) -> DevelopRequest {
        DevelopRequest {
            max_width: n,
            max_height: n,
            region: None,
            quality: DevelopQuality::Preview,
            priority: DevelopPriority::Interactive,
            format: DevelopFormat::Rgba8Srgb,
            histogram: true,
            geometry: DevelopGeometry::Final,
        }
    }

    fn render(
        path: &str,
        adj: &str,
        r: &DevelopRequest,
    ) -> Result<(DevelopFrame, Vec<u8>), DsError> {
        let p = futures_executor::block_on(plan_develop(
            path.into(),
            adj.into(),
            r.clone(),
            CancelToken::new(),
        ))?;
        let mut buf = vec![0u8; (p.bytes_per_row * u64::from(p.height)) as usize];
        let f = futures_executor::block_on(render_develop(
            path.into(),
            adj.into(),
            r.clone(),
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            None,
            CancelToken::new(),
        ))?;
        assert_eq!((f.width, f.height), (p.width, p.height));
        Ok((f, buf))
    }

    fn mean(buf: &[u8]) -> [f64; 3] {
        let mut s = [0f64; 3];
        let n = (buf.len() / 4) as f64;
        for p in buf.chunks(4) {
            for c in 0..3 {
                s[c] += f64::from(p[c]);
            }
        }
        s.map(|v| v / n)
    }

    #[test]
    fn warmer_white_balance_raises_red_and_lowers_blue() {
        let Some(path) = lt_testdata::sample_arw() else {
            return;
        };
        let path = path.to_string_lossy().into_owned();
        let wb = |k: f32| {
            let mut a = Adjustments::default();
            a.white_balance = lt_core::adjust::WhiteBalance::Custom {
                temp_k: k,
                tint: 0.0,
            };
            serde_json::to_string(&a).unwrap()
        };
        // Telling the pipeline the light was bluer (higher K) renders warmer.
        let (neutral, warm) = (wb(4500.0), wb(8000.0));
        let (f, a) = render(&path, &neutral, &request(800)).unwrap();
        let (_, b) = render(&path, &warm, &request(800)).unwrap();
        let (ma, mb) = (mean(&a), mean(&b));
        assert!(
            mb[0] > ma[0] + 2.0 && mb[2] < ma[2] - 2.0,
            "{ma:?} → {mb:?}"
        );
        let h = f.histogram.unwrap();
        assert_eq!(h.red.len(), 256);
        assert_eq!(h.total, u64::from(f.width) * u64::from(f.height));
    }

    #[test]
    fn plans_final_and_oriented_geometry_and_region_renders() {
        let Some(path) = lt_testdata::sample_arw() else {
            return;
        };
        let path = path.to_string_lossy().into_owned();
        let mut adj = Adjustments::default();
        adj.crop.quarter_turns = 1;
        adj.crop.rect = CoreRect {
            x: 0.1,
            y: 0.1,
            w: 0.5,
            h: 0.5,
        };
        let json = serde_json::to_string(&adj).unwrap();
        let fin = futures_executor::block_on(plan_develop(
            path.clone(),
            json.clone(),
            request(1000),
            CancelToken::new(),
        ))
        .unwrap();
        let ori = futures_executor::block_on(plan_develop(
            path.clone(),
            json.clone(),
            DevelopRequest {
                geometry: DevelopGeometry::Oriented,
                ..request(1000)
            },
            CancelToken::new(),
        ))
        .unwrap();
        // The oriented frame is the whole corrected E frame; its map is (about) identity.
        let m = &ori.frame_to_crop_space;
        assert!(
            (m[0] - 1.0).abs() < 1e-3 && m[1].abs() < 1e-9 && (m[4] - 1.0).abs() < 1e-3,
            "{m:?}"
        );
        assert!(ori.source_width > 0 && ori.source_height > 0);
        // The final frame's corner (0,0) after one clockwise turn is the crop's bottom-left.
        let f = &fin.frame_to_crop_space;
        assert!(
            (f[2] - 0.1).abs() < 1e-3 && (f[5] - 0.6).abs() < 1e-3,
            "{f:?}"
        );
        // A region render: a window of the whole frame at the same scale.
        let r = DevelopRequest {
            region: Some(NormRect {
                x: 0.25,
                y: 0.25,
                w: 0.5,
                h: 0.5,
            }),
            ..request(1000)
        };
        // max_size bounds the region's output: it fills the box at a higher density.
        let (rf, buf) = render(&path, &json, &r).unwrap();
        assert_eq!(
            (rf.width, rf.height),
            (fin.width, fin.height),
            "{rf:?} vs {fin:?}"
        );
        // At 1:1 the region is its native size (never upscaled past it).
        let one = DevelopRequest {
            max_width: 1 << 16,
            max_height: 1 << 16,
            ..r.clone()
        };
        let (native, _) = render(&path, &json, &one).unwrap();
        assert!(
            (i64::from(native.width) - i64::from(fin.native_width) / 2).abs() <= 2,
            "{native:?} vs {fin:?}"
        );
        assert!(buf.chunks(4).any(|p| p[0] > 10));
    }

    #[test]
    fn half_float_frames_and_errors() {
        let Some(path) = lt_testdata::sample_arw() else {
            return;
        };
        let path = path.to_string_lossy().into_owned();
        let adj = serde_json::to_string(&Adjustments::default()).unwrap();
        let r = DevelopRequest {
            format: DevelopFormat::Rgba16fLinearP3,
            quality: DevelopQuality::Draft,
            histogram: false,
            ..request(300)
        };
        let (f, buf) = render(&path, &adj, &r).unwrap();
        assert!(f.histogram.is_none());
        assert_eq!(
            buf.len() as u64,
            u64::from(f.width) * u64::from(f.height) * 8
        );
        assert_eq!(DevelopFormat::Rgba8DisplayP3.bytes_per_pixel(), 4);
        assert_eq!(DevelopFormat::Rgba16fLinearRec2020.bytes_per_pixel(), 8);
        // Too small a destination, null pointer, bad JSON, zero size, empty region, cancel.
        let mut small = [0u8; 16];
        let e = futures_executor::block_on(render_develop(
            path.clone(),
            adj.clone(),
            r.clone(),
            small.as_mut_ptr() as u64,
            16,
            None,
            CancelToken::new(),
        ));
        assert!(matches!(e, Err(DsError::InvalidArgument { .. })), "{e:?}");
        let e = futures_executor::block_on(render_develop(
            path.clone(),
            adj.clone(),
            r.clone(),
            0,
            16,
            None,
            CancelToken::new(),
        ));
        assert!(matches!(e, Err(DsError::InvalidArgument { .. })));
        assert!(matches!(
            futures_executor::block_on(plan_develop(
                path.clone(),
                "{".into(),
                r.clone(),
                CancelToken::new()
            )),
            Err(DsError::InvalidArgument { .. })
        ));
        assert!(render(&path, &adj, &request(0)).is_err());
        let empty = DevelopRequest {
            region: Some(NormRect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.5,
            }),
            ..request(100)
        };
        assert!(render(&path, &adj, &empty).is_err());
        let t = CancelToken::new();
        t.cancel();
        let mut buf = vec![0u8; 400 * 400 * 4];
        let e = futures_executor::block_on(render_develop(
            path.clone(),
            adj.clone(),
            request(400),
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            None,
            t,
        ));
        assert!(matches!(e, Err(DsError::Cancelled)), "{e:?}");
        assert!(render("/nonexistent/x.ARW", &adj, &request(100)).is_err());
    }

    struct Everything;
    impl DevelopMaskSource for Everything {
        fn ai_mask(&self, _: DevelopAiTarget) -> Option<DevelopMask> {
            Some(DevelopMask {
                width: 4,
                height: 4,
                data: vec![255; 16],
                version: 0,
            })
        }
        /// Person `p`'s mask is `p` wide (versioned by the person).
        fn ai_person_mask(&self, _: DevelopAiTarget, p: u32) -> Option<DevelopMask> {
            (p > 0).then(|| DevelopMask {
                width: p,
                height: 1,
                data: vec![9; p as usize],
                version: u64::from(p),
            })
        }
        fn depth_map(&self) -> Option<DevelopMask> {
            Some(DevelopMask {
                width: 2,
                height: 1,
                data: vec![0, 255],
                version: 7,
            })
        }
    }

    struct Broken;
    impl DevelopMaskSource for Broken {
        fn ai_mask(&self, _: DevelopAiTarget) -> Option<DevelopMask> {
            Some(DevelopMask {
                width: 4,
                height: 4,
                data: vec![255; 3],
                version: 0,
            })
        }
        fn ai_person_mask(&self, t: DevelopAiTarget, _: u32) -> Option<DevelopMask> {
            self.ai_mask(t)
        }
        fn depth_map(&self) -> Option<DevelopMask> {
            self.ai_mask(DevelopAiTarget::Subject)
        }
    }

    /// People targets map one to one, and a person index reaches the app's per-person
    /// call (all people stay on `ai_mask`).
    #[test]
    fn person_masks_route_to_the_apps_per_person_call() {
        for t in AiTarget::ALL {
            let d = DevelopAiTarget::from(t);
            assert_eq!(format!("{d:?}"), format!("{t:?}"));
        }
        let src = mask_source(Arc::new(Everything));
        let all = src.ai_person_mask(AiTarget::Hair, None).unwrap();
        assert_eq!(all.size, Size::new(4, 4));
        let two = src.ai_person_mask(AiTarget::Hair, Some(2)).unwrap();
        assert_eq!((two.size, two.version), (Size::new(2, 1), 2));
        assert!(src.ai_person_mask(AiTarget::Hair, Some(0)).is_none());
        let broken = mask_source(Arc::new(Broken));
        assert!(broken.ai_person_mask(AiTarget::Teeth, Some(1)).is_none());
        // The depth map crosses with its version; a malformed one is no depth.
        let depth = src.depth_map().unwrap();
        assert_eq!(
            (depth.size, depth.version, &depth.data[..]),
            (Size::new(2, 1), 7, &[0, 255][..])
        );
        assert!(broken.depth_map().is_none());
    }

    /// The app's mask version reaches the pipeline's bitmap (its raster cache key);
    /// malformed bitmaps are no mask.
    #[test]
    fn mask_versions_reach_the_pipeline() {
        let m = |w, h, n, version| DevelopMask {
            width: w,
            height: h,
            data: vec![7; n],
            version,
        };
        let b = mask_bitmap(m(3, 2, 6, 42)).unwrap();
        assert_eq!((b.size, b.version, b.data.len()), (Size::new(3, 2), 42, 6));
        assert_eq!(mask_bitmap(m(3, 2, 6, 0)).unwrap().version, 0);
        assert!(mask_bitmap(m(3, 2, 5, 1)).is_none());
        assert!(mask_bitmap(m(0, 2, 0, 1)).is_none());
    }

    #[test]
    fn ai_masks_come_from_the_app_and_brighten_their_layer() {
        let Some(path) = lt_testdata::sample_arw() else {
            return;
        };
        let path = path.to_string_lossy().into_owned();
        let mut adj = Adjustments::default();
        let mut layer = lt_core::adjust::Layer::default();
        layer.adjustments.exposure.exposure_ev = 1.5;
        layer.mask.shapes.push(lt_core::adjust::MaskShape::Ai {
            op: lt_core::adjust::MaskOp::Add,
            target: AiTarget::Subject,
            feather: 0.0,
            person: None,
        });
        adj.layers.push(layer);
        let json = serde_json::to_string(&adj).unwrap();
        let r = request(300);
        let run = |m: Option<Arc<dyn DevelopMaskSource>>| {
            let p = futures_executor::block_on(plan_develop(
                path.clone(),
                json.clone(),
                r.clone(),
                CancelToken::new(),
            ))
            .unwrap();
            let mut buf = vec![0u8; (p.bytes_per_row * u64::from(p.height)) as usize];
            futures_executor::block_on(render_develop(
                path.clone(),
                json.clone(),
                r.clone(),
                buf.as_mut_ptr() as u64,
                buf.len() as u64,
                m,
                CancelToken::new(),
            ))
            .unwrap();
            mean(&buf)
        };
        let none = run(None);
        let all = run(Some(Arc::new(Everything)));
        let broken = run(Some(Arc::new(Broken)));
        assert!(all[1] > none[1] + 5.0, "{none:?} → {all:?}");
        assert!(
            (broken[1] - none[1]).abs() < 0.5,
            "a malformed mask is ignored: {broken:?}"
        );
        assert_eq!(DevelopAiTarget::from(AiTarget::Sky), DevelopAiTarget::Sky);
        assert_eq!(
            DevelopAiTarget::from(AiTarget::Person),
            DevelopAiTarget::Person
        );
        assert_eq!(
            DevelopAiTarget::from(AiTarget::Background),
            DevelopAiTarget::Background
        );
    }

    #[test]
    fn prefetch_and_backend() {
        let b = develop_backend().unwrap();
        assert_ne!(b.name.len(), 0);
        let Some(path) = lt_testdata::sample_arw() else {
            return;
        };
        let adj = serde_json::to_string(&Adjustments::default()).unwrap();
        prefetch_develop(
            path.to_string_lossy().into_owned(),
            adj.clone(),
            request(500),
        )
        .unwrap();
        let live = lt_pipeline::CancelToken::new();
        assert!(prefetch(&path, &Adjustments::default(), &request(500), &live).is_ok());
        let dead = lt_pipeline::CancelToken::new();
        dead.cancel();
        assert!(matches!(
            prefetch(&path, &Adjustments::default(), &request(500), &dead),
            Err(DsError::Cancelled)
        ));
        assert!(
            prefetch_develop(
                path.to_string_lossy().into_owned(),
                "{".into(),
                request(500)
            )
            .is_err()
        );
        assert!(
            prefetch(
                Path::new("/nonexistent/x.ARW"),
                &Adjustments::default(),
                &request(500),
                &live
            )
            .is_err()
        );
        pin_develop(Some(path.to_string_lossy().into_owned()));
        pin_develop(None);
    }

    /// A1: a newer prefetch of the same file supersedes (cancels) the older one instead
    /// of being dropped; navigation cancels everything outside the new neighbourhood;
    /// at most `MAX_PREFETCHES` stay outstanding.
    #[test]
    fn prefetches_are_superseded_cancelled_and_bounded() {
        let a = PathBuf::from("/rnd9/prefetch-a.ARW");
        let (g1, t1) = register_prefetch(&a);
        let (g2, t2) = register_prefetch(&a);
        assert!(
            g2 > g1 && t1.is_cancelled() && !t2.is_cancelled(),
            "the newer one wins"
        );
        finish_prefetch(&a, g1); // the old one finishing doesn't drop the new entry
        assert!(lock_prefetching().contains_key(&a));
        let others: Vec<_> = (0..MAX_PREFETCHES + 2)
            .map(|i| register_prefetch(&PathBuf::from(format!("/rnd9/prefetch-{i}.ARW"))).1)
            .collect();
        assert!(t2.is_cancelled(), "oldest cancelled past the bound");
        assert!(lock_prefetching().len() <= MAX_PREFETCHES);
        let keep = format!("/rnd9/prefetch-{}.ARW", MAX_PREFETCHES + 1);
        cancel_develop_prefetches(vec![keep.clone()]);
        assert!(
            others[..=MAX_PREFETCHES]
                .iter()
                .all(lt_pipeline::CancelToken::is_cancelled)
        );
        assert!(!others[MAX_PREFETCHES + 1].is_cancelled());
        let rnd9: Vec<PathBuf> = lock_prefetching()
            .keys()
            .filter(|p| p.starts_with("/rnd9"))
            .cloned()
            .collect();
        assert_eq!(rnd9, vec![PathBuf::from(&keep)]);
        cancel_develop_prefetches(vec![]);
    }
}
