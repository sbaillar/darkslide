//! Luxtable develop pipeline.
//!
//! Two renderers implement the same staged pipeline (see `docs/ARCHITECTURE.md`):
//!
//! ```text
//! RawImage ─► BASE    black/white normalize → reference (as-shot) WB → demosaic (superpixel
//!                     or RCD) → pyramid level; camera RGB. Cached per ImageKey × level (one
//!                     entry, replaced when a pan leaves its rectangle); never per WB.
//!          ─► DEVELOP geometry resample (crop/rotate/flip, keystone, lens distortion + CA)
//!                     → WB ratio + highlight reconstruction → lens vignetting → camera matrix
//!                     → exposure/vignette → gamut compress → HDR (guided-filter local tone)
//!                     → hue-preserving tone → levels/curves → color balance → saturation
//!                     → color editor → clarity/structure → layers (masked) → glow/halation
//!                     → NR (ISO-adaptive) → sharpen → grain → [export resize + output
//!                     sharpening] → output transform
//!                     (lens blur: right after the scene stage, in scene-linear light)
//! ```
//!
//! The CPU renderer ([`cpu_renderer`]) is the reference. The GPU renderer ([`gpu_renderer`],
//! wgpu → Metal) runs the same math as WGSL compute shaders; parity is enforced by tests.
//! Interactive drags use [`Quality::Draft`] (develop at ≤ [`DRAFT_LONG_EDGE`], upscale);
//! [`Renderer::render_frame_into`] can also return the output's histogram.
//!
//! Geometry is resampled *first* so every develop stage runs at output resolution. All
//! spatial radii are defined in sensor pixels and scaled to the output, so a 2560 px preview
//! and a full-resolution export look the same.
//!
//! Coordinate frames: the **E frame** is the image after EXIF orientation (and after the
//! maker-note default crop). `Crop::rect`, mask shapes and AI mask bitmaps are normalized in
//! the E frame (oriented, before the quarter turns and before straightening). The straighten
//! angle rotates the sampling rectangle about the crop center (positive = image content
//! turns counter-clockwise), then the user's quarter turns (clockwise) and flips are
//! applied to the cropped result. Positive vertical keystone widens the top; positive
//! horizontal keystone widens the right side.

// Numeric image code: exact float comparisons are deliberate identity checks
// (e.g. `saturation != 1.0` skips a no-op stage), constants carry published precision,
// and per-channel index loops mirror the WGSL kernels.
#![allow(
    clippy::float_cmp,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::excessive_precision,
    clippy::unreadable_literal,
    clippy::many_single_char_names,
    clippy::needless_range_loop
)]

pub mod auto;
pub mod cache;
pub mod color;
mod color_edit;
mod cpu;
pub mod dcp;
mod demosaic;
pub mod denoise;
mod detail;
pub mod effects;
mod geometry;
mod gpu;
mod inpaint;
pub mod lens;
mod local;
pub mod lut;
pub mod mask;
pub mod optimize;
mod par;
mod plan;
pub mod profile;
pub mod removal;
mod spots;
mod tone;
pub mod upscale;

#[cfg(test)]
mod cpu_tests;
#[cfg(test)]
mod effects_tests;
#[cfg(test)]
mod testutil;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lt_core::{Adjustments, NormRect, RawImage, Size};

pub use auto::{Histogram, histogram};
pub use cpu::CpuRenderer;
pub use geometry::{CaptureInfo, Fit, inscribed_crop};
pub use gpu::GpuRenderer;
/// Version of the rendered output ([`lt_core::PIPELINE_VERSION`]); bump it with any change
/// to what this crate renders.
pub use lt_core::PIPELINE_VERSION;
pub use mask::{MaskBitmap, MaskSource};
pub use plan::raw_noise;
pub use removal::Inpainter;

/// Identity of a decoded image for caching (e.g. a hash of the file + variant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageKey(pub u64);

/// Speed/quality trade-off for the BASE stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Quality {
    /// Interactive drags: the same BASE as `Preview` (no rebuild when a drag starts), but
    /// outputs larger than [`DRAFT_LONG_EDGE`] are developed at that size and bilinearly
    /// upscaled to the requested size, without sharpening. Request `Preview` again when
    /// the drag ends for the full-resolution refine.
    ///
    /// A Draft frame's histogram describes the upscaled output: isolated clipped pixels
    /// average out, so its clipped counts read lower than the refined frame's (use the
    /// refined frame for clipping readouts).
    Draft,
    /// Superpixel at scales ≤ 0.5, RCD above.
    #[default]
    Preview,
    /// Always RCD at full sensor resolution, then box-downsampled (export).
    Full,
}

/// Output color space. All use the sRGB transfer function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OutputSpace {
    #[default]
    Srgb,
    DisplayP3,
    /// The working gamut itself: nothing is clipped to a display gamut. For exports
    /// to an ICC profile (converted afterwards) and the soft-proofing viewer.
    Rec2020,
}

/// Pixel format written by [`Renderer::render_into`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OutputFormat {
    /// 8-bit RGBA in `RenderRequest::output` (sRGB or Display P3, sRGB transfer), alpha 255.
    #[default]
    Rgba8,
    /// 16-bit RGB (native-endian `u16`) in `RenderRequest::output`, for export.
    Rgb16,
    /// Half-float RGBA (native-endian `f16`), **linear** Display P3, not clipped at 1.0
    /// (extended range for EDR), alpha 1.0. `RenderRequest::output` is ignored except
    /// `OutputSpace::Rec2020`, which makes it linear Rec.2020 (soft proofing).
    Rgba16fLinearP3,
}

impl OutputFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8 => 4,
            Self::Rgb16 => 6,
            Self::Rgba16fLinearP3 => 8,
        }
    }
}

/// Cooperative cancellation for a render in flight. Clone it, hand one copy to the render
/// (on [`RenderRequest::cancel`]) and call [`CancelToken::cancel`] from any thread; the
/// renderer checks it between passes and while waiting for the GPU, and returns
/// [`PipelineError::Cancelled`].
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Output sharpening for exports, applied after the resize to the target size (an
/// unsharp mask on luma: `amount` × (luma − Gaussian(luma, `radius` px))).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputSharpening {
    /// 0 = off; typical 0.3..1.0.
    pub amount: f32,
    /// Gaussian sigma in output pixels (typical 0.5..1.5).
    pub radius: f32,
}

/// Scheduling class of a render on a shared [`GpuRenderer`], highest first. Each GPU turn
/// goes to the highest class waiting; each non-interactive class has one render in flight
/// at a time (FIFO within the class, the wait cancellable), yielding the GPU between
/// stages (and between tiles of large outputs), so a queue of thumbnails never delays an
/// auto tool or an export by more than a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub enum Priority {
    /// The viewer: always served first, its image's RAW upload is pinned.
    #[default]
    Interactive,
    /// Interactive tools that the user waits on: auto tone/white balance, picker probes.
    Tool,
    /// Exports and prefetch.
    Background,
    /// Bulk work: grid thumbnails.
    Bulk,
}

impl Priority {
    /// 0 (interactive) ..= 3 (bulk).
    pub(crate) fn class(self) -> usize {
        match self {
            Self::Interactive => 0,
            Self::Tool => 1,
            Self::Background => 2,
            Self::Bulk => 3,
        }
    }
}

/// What to render.
#[derive(Clone)]
pub struct RenderRequest {
    /// Bounding box for the output; the image is fit inside and never upscaled past its
    /// native size (1:1, or the [`Fit::FillOriginalSize`] size of an auto-fitted frame).
    pub max_size: Size,
    /// Sub-rectangle of the final (cropped, rotated) frame to render, normalized. `None` = all.
    pub region: Option<NormRect>,
    pub quality: Quality,
    pub output: OutputSpace,
    /// Provider for AI mask bitmaps (Apple Vision on the Swift side). `None` → AI shapes are empty.
    pub masks: Option<Arc<dyn MaskSource>>,
    /// Checked between passes; see [`CancelToken`].
    pub cancel: Option<CancelToken>,
    /// Capture metadata (from `lt_raw::read_metadata`): the lens and focal length select
    /// the lens-correction profile; the ISO makes noise reduction ISO-adaptive.
    pub capture: CaptureInfo,
    /// Scheduling class on a shared GPU renderer (ignored by the CPU renderer).
    pub priority: Priority,
    /// Also compute the output's display histogram ([`Renderer::render_frame_into`]); the
    /// GPU renderer does it in the output pass.
    pub histogram: bool,
    /// Sharpening after the export resize. `Quality::Full` renders smaller than the
    /// sensor develop at full resolution and resize to `max_size` in linear light
    /// (area-correct Catmull-Rom) on the GPU, so exporters never resize on the CPU; this
    /// is then applied at the target size.
    pub output_sharpening: Option<OutputSharpening>,
    /// Native size of a frame auto-fitted by straighten, keystone or lens correction.
    pub fit: Fit,
    /// The app's inpainting model for content-aware removals (`None` → `PatchMatch`).
    pub inpainter: Option<Arc<dyn Inpainter>>,
}

impl RenderRequest {
    /// A preview request fitting `max_size`, sRGB, no region.
    pub fn new(max_size: Size) -> Self {
        Self {
            max_size,
            region: None,
            quality: Quality::Preview,
            output: OutputSpace::Srgb,
            masks: None,
            cancel: None,
            capture: CaptureInfo::default(),
            priority: Priority::Interactive,
            histogram: false,
            output_sharpening: None,
            fit: Fit::FillOriginalSize,
            inpainter: None,
        }
    }

    pub(crate) fn check_cancel(&self) -> Result<(), PipelineError> {
        match &self.cancel {
            Some(c) if c.is_cancelled() => Err(PipelineError::Cancelled),
            _ => Ok(()),
        }
    }
}

impl Default for RenderRequest {
    fn default() -> Self {
        Self::new(Size::new(2560, 2560))
    }
}

impl fmt::Debug for RenderRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderRequest")
            .field("max_size", &self.max_size)
            .field("region", &self.region)
            .field("quality", &self.quality)
            .field("output", &self.output)
            .field("masks", &self.masks.is_some())
            .field("cancel", &self.cancel)
            .field("capture", &self.capture)
            .field("priority", &self.priority)
            .field("histogram", &self.histogram)
            .field("output_sharpening", &self.output_sharpening)
            .field("fit", &self.fit)
            .field("inpainter", &self.inpainter.as_ref().map(|i| i.id()))
            .finish()
    }
}

/// 8-bit RGBA output, row-major, alpha = 255.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedImage {
    pub size: Size,
    pub rgba8: Vec<u8>,
}

/// What [`Renderer::render_frame_into`] produced besides the pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFrame {
    pub size: Size,
    /// Present when `RenderRequest::histogram` was set.
    pub histogram: Option<Histogram>,
}

/// 16-bit RGB output for export, row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedImage16 {
    pub size: Size,
    pub rgb16: Vec<u16>,
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    InvalidRaw(#[from] lt_core::raw::RawError),
    #[error("invalid render request: {0}")]
    InvalidRequest(String),
    #[error("no GPU adapter available")]
    NoAdapter,
    #[error("GPU error: {0}")]
    Gpu(String),
    #[error("render cancelled")]
    Cancelled,
}

/// Output size a render of this image/adjustments/request produces (to size the buffer
/// for [`Renderer::render_into`]).
pub fn output_size(
    raw: &RawImage,
    adj: &Adjustments,
    req: &RenderRequest,
) -> Result<Size, PipelineError> {
    raw.validate()?;
    Ok(geometry::plan(raw, adj, &req.capture, req.region, req.max_size, req.fit)?.out)
}

/// Where a whole-frame render's pixels come from, for the host's overlays and input
/// (crop handles, brushes, masks, color pickers): the output size, and the affine map from
/// **final-frame normalized** coordinates (0..1 across the whole cropped, straightened,
/// turned and flipped output) to **E-normalized** coordinates (the space of `Crop::rect`,
/// mask shapes and AI mask bitmaps). Row-major `[m00, m01, m02, m10, m11, m12]`.
///
/// Keystone and lens distortion don't appear: crop rects and masks live in the corrected
/// E frame, after both. Region renders (`req.region`) are windows into the same final frame.
pub fn frame_mapping(
    raw: &RawImage,
    adj: &Adjustments,
    req: &RenderRequest,
) -> Result<(Size, [f64; 6]), PipelineError> {
    raw.validate()?;
    let g = geometry::plan(raw, adj, &req.capture, None, req.max_size, req.fit)?;
    let m = geometry::Affine::scale(g.frame_out[0], g.frame_out[1])
        .then(&g.to_norm)
        .0;
    Ok((
        g.out,
        [m[0][0], m[0][1], m[0][2], m[1][0], m[1][1], m[1][2]],
    ))
}

/// Long edge (pixels) at which `Quality::Draft` develops before upscaling. 2048 keeps a
/// 5K drag frame (2832 px) at p90 6.4–9.8 ms in the pipeline (2560: 9.0–11.3 ms), leaving
/// the host its share of a 16 ms frame; the settled `Preview` refines at full size.
pub const DRAFT_LONG_EDGE: u32 = 2048;

/// Draft long edge for heavy edits (see [`plan::draft_long_edge`]): several masked
/// layers with local tools and output-space mask planes over a busy global edit missed
/// the 16 ms drag budget at 2048 (5K, 18.8–19 ms median).
pub const DRAFT_LONG_EDGE_HEAVY: u32 = 1536;

/// A develop renderer. Implementations cache the BASE stage per image so that
/// DEVELOP-only changes — every slider, including white balance — are cheap.
pub trait Renderer: Send {
    /// Render into a caller-owned buffer (e.g. a Metal shared buffer mapped by the host),
    /// row-major, tightly packed, `format.bytes_per_pixel()` per pixel. `out` must hold at
    /// least `size.pixels() × bytes_per_pixel` bytes, where `size` is [`output_size`].
    /// Returns that size and, when `req.histogram` is set, the display histogram of the
    /// output (8-bit display codes in the output space).
    fn render_frame_into(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
        format: OutputFormat,
        out: &mut [u8],
    ) -> Result<RenderedFrame, PipelineError>;

    /// [`Renderer::render_frame_into`] returning only the size.
    fn render_into(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
        format: OutputFormat,
        out: &mut [u8],
    ) -> Result<Size, PipelineError> {
        self.render_frame_into(key, raw, adj, req, format, out)
            .map(|f| f.size)
    }

    /// Drop every cached resource for `key`.
    fn evict(&mut self, key: ImageKey);

    fn render(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
    ) -> Result<RenderedImage, PipelineError> {
        let size = output_size(raw, adj, req)?;
        let mut rgba8 = vec![0u8; size.pixels() as usize * 4];
        self.render_into(key, raw, adj, req, OutputFormat::Rgba8, &mut rgba8)?;
        Ok(RenderedImage { size, rgba8 })
    }

    fn render16(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
    ) -> Result<RenderedImage16, PipelineError> {
        let size = output_size(raw, adj, req)?;
        let mut rgb16 = vec![0u16; size.pixels() as usize * 3];
        self.render_into(
            key,
            raw,
            adj,
            req,
            OutputFormat::Rgb16,
            bytemuck::cast_slice_mut(&mut rgb16),
        )?;
        Ok(RenderedImage16 { size, rgb16 })
    }
}

/// Check a caller buffer against the output size and format.
pub(crate) fn check_out(
    size: Size,
    format: OutputFormat,
    out: &[u8],
) -> Result<usize, PipelineError> {
    let need = size.pixels() as usize * format.bytes_per_pixel();
    if out.len() < need {
        return Err(PipelineError::InvalidRequest(format!(
            "output buffer holds {} bytes, {need} needed for {size:?} {format:?}",
            out.len()
        )));
    }
    Ok(need)
}

/// The CPU reference renderer (rayon, f32).
pub fn cpu_renderer() -> CpuRenderer {
    CpuRenderer::new()
}

/// The GPU renderer (wgpu → Metal). Errors when no adapter is available.
pub fn gpu_renderer() -> Result<GpuRenderer, PipelineError> {
    GpuRenderer::new()
}
