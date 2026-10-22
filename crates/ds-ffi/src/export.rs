//! Export (`ds-export`): process recipes → files, with progress and cancel.
//!
//! - Recipes ([`ExportRecipeSpec`]) mirror `ds_export::ExportRecipe`: format,
//!   size, sharpening, output space, file-name template (`ds-template`),
//!   destination (the app passes the session's `Output/` by default) and the
//!   EXIF policy (include, strip GPS). Several recipes run per batch; recipes
//!   sharing an output space share one render.
//! - Encoders: JPEG is mozjpeg unless the app hands in its hardware encoder
//!   ([`ImageEncoder`], `ImageIO` on the Swift side); HEIC always comes from
//!   that encoder. The image crosses the FFI once per output as native-endian
//!   16-bit RGB bytes.
//! - Rendering is one seam, [`ExportRenderer`]. [`DevelopRenderer`] develops
//!   RAWs through `ds-pipeline` on the process-wide renderer
//!   ([`crate::develop`]): every edit, crop, straighten and quarter turn applies,
//!   the output is rendered at each recipe's target size (`Quality::Full`, the
//!   recipe's output space) with its output sharpening: 8-bit RGBA for JPEG
//!   (mozjpeg) and PNG8, which `ds-export` encodes as is, 16-bit otherwise; at
//!   `Priority::Background` so the viewer's renders go first. JPEG/HEIF/PNG/TIFF
//!   originals are decoded by the app ([`ImageEncoder::decode`], `ImageIO`), which
//!   also resizes and sharpens them to each recipe's target on the GPU (Core
//!   Image); without the app's decoder (or for an upscale) they are decoded at
//!   full size and resized/sharpened by `ds-export`; videos are refused.
//! - Metadata: RAWs through `ds-raw` (including GPS), other originals through
//!   their EXIF block; `OffsetTimeOriginal` is written when known.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ds_core::adjust::AiTarget;
use ds_core::{Adjustments, IptcMeta, Size};
use ds_export::proof::{IccProfile, OutputProfile};
use ds_export::{
    BatchOptions, ExifPolicy, ExportFormat, ExportItem, ExportRecipe, Gps, HeicEncoder, Image16,
    OutputSpace, Rendered, Renderer, Sizing,
};
use ds_pipeline::{
    Fit, MaskBitmap, OutputFormat, OutputSharpening, OutputSpace as PipelineSpace, Priority,
    Quality, RenderRequest,
};
use ds_template::{Template, Vars};

use crate::develop::Loaded;
use crate::error::DsError;
use crate::events::{Dispatcher, ProgressEvent, ProgressListener, TaskPhase};
use crate::jobs::{CancelToken, guard, run_job_on_thread};
use crate::proof::ProofRenderingIntent;
use crate::session::metadata::PhotoMetadata;
use crate::viewer::{DevelopAiTarget, DevelopMask};
use crate::watermark::{TextHook, WatermarkSpec, WatermarkTextRenderer, watermark_of};

/// Output file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ExportFileFormat {
    /// Quality 1..=100.
    Jpeg {
        quality: u8,
    },
    /// Quality 1..=100 (needs the app's [`ImageEncoder`]).
    Heic {
        quality: u8,
    },
    /// Uncompressed 16-bit RGB TIFF.
    Tiff16,
    Png8,
    Png16,
}

/// How to size the output (of the rendered, oriented image).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Enum)]
pub enum ExportSize {
    Original,
    LongEdge { pixels: u32 },
    ShortEdge { pixels: u32 },
    Megapixels { megapixels: f32 },
    Percent { percent: f32 },
}

/// Output color space (its ICC profile is embedded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ExportColorSpace {
    Srgb,
    DisplayP3,
}

/// One process recipe.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
#[allow(clippy::struct_excessive_bools)] // independent recipe options, flat across the FFI
pub struct ExportRecipeSpec {
    pub name: String,
    pub format: ExportFileFormat,
    pub size: ExportSize,
    pub allow_upscale: bool,
    pub space: ExportColorSpace,
    /// Output sharpening amount, 0 = off, up to 5.
    pub sharpen: f32,
    /// `ds-template` file name, e.g. `{name}_{seq:3}` (`.ext` appended).
    pub file_name_template: String,
    /// Absolute destination directory (created if missing).
    pub destination: String,
    /// Copy capture metadata (time, camera, lens, exposure) into the file.
    pub include_exif: bool,
    /// Leave out GPS even when the photo has a position.
    pub strip_gps: bool,
    /// Export a straightened or lens-corrected frame at its inscribed size (1:1 sensor
    /// pixels) instead of the original size (`ds_pipeline::Fit`).
    #[uniffi(default = false)]
    pub constrain_to_sensor_pixels: bool,
    /// Convert to this ICC profile and embed it (e.g. the soft-proof profile); the
    /// photo is then rendered in Rec.2020 (the working gamut) whatever `space` says.
    #[uniffi(default = None)]
    pub output_profile: Option<ExportOutputProfile>,
    /// AI Upscale: the recipe may enlarge, starting from the image upscaled 2× by the
    /// app's model (`set_upscale_provider`).
    #[uniffi(default = false)]
    pub ai_upscale: bool,
    /// Drawn on each output after resizing and sharpening; `None` = no watermark.
    #[uniffi(default = None)]
    pub watermark: Option<WatermarkSpec>,
    /// Plug-in steps drawn after the watermark, in order (contract 28).
    #[uniffi(default = [])]
    pub post_process: Vec<crate::plugins::ExportPostProcessStep>,
}

/// An export recipe's output ICC profile (RGB), with how colours are mapped into it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ExportOutputProfile {
    /// Absolute path of the `.icc` / `.icm` file.
    pub path: String,
    pub intent: ProofRenderingIntent,
    pub black_point_compensation: bool,
}

/// A position to write when EXIF is included and GPS isn't stripped.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct ExportGps {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: Option<f64>,
}

/// One photo to export.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ExportSource {
    /// Caller's id (variant id), echoed in the report.
    pub id: u64,
    /// The RAW file.
    pub path: String,
    /// `{seq}` for this photo (1-based in the app's order).
    pub seq: u64,
    pub gps: Option<ExportGps>,
    pub artist: Option<String>,
    pub copyright: Option<String>,
    /// The photo's adjustments (`ds_core::Adjustments` JSON, as in its sidecar);
    /// `None` develops with the defaults.
    #[uniffi(default = None)]
    pub adjustments_json: Option<String>,
    /// The photo's keywords (sidecar), written as XMP `dc:subject` / IPTC keywords
    /// when the recipe includes metadata.
    #[uniffi(default)]
    pub keywords: Vec<String>,
    /// The photo's title, caption, credits… (sidecar), written as XMP/IPTC (the
    /// caption also as EXIF `ImageDescription`) when the recipe includes metadata.
    /// Its creator and copyright win over `artist`/`copyright`.
    #[uniffi(default = None)]
    pub metadata: Option<PhotoMetadata>,
}

/// One written file.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ExportedFileInfo {
    pub source_id: u64,
    /// Index into the batch's recipes.
    pub recipe: u32,
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    /// The recipe's text watermark was left out: its text was blank for this photo.
    #[uniffi(default = false)]
    pub watermark_skipped: bool,
}

/// What `{name}`, `{date}` and `{camera}` give for one photo, as the export
/// itself reads them (the sheet's file-name example uses it).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ExportNameSample {
    pub name: String,
    pub camera: Option<String>,
    /// Camera wall-clock time, `YYYY-MM-DDTHH:MM:SS`.
    pub capture_time: Option<String>,
    /// The file's own EXIF credits (the export's fallback for `{artist}`/`{copyright}`).
    #[uniffi(default = None)]
    pub artist: Option<String>,
    #[uniffi(default = None)]
    pub copyright: Option<String>,
}

/// Upright pixel size of a non-RAW original (its EXIF orientation applied).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

/// What [`ImageEncoder::decode`] resizes to: exactly `width`×`height` (never
/// larger than the upright original), then the recipe's output sharpening
/// (`sharpen` > 0: the unsharp mask of `ds_export::process::sharpen_in_place`,
/// applied to the output-encoded values).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct DecodeTarget {
    pub width: u32,
    pub height: u32,
    pub sharpen: f32,
}

/// A non-RAW original decoded by the app: upright, `width`×`height`
/// interleaved RGB, 16 bits per sample in native byte order.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgb16: Vec<u8>,
}

/// One failure; `source_id`/`recipe` are `None` when the whole recipe or the
/// render of a photo failed.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ExportFailureInfo {
    pub source_id: Option<u64>,
    pub recipe: Option<u32>,
    pub message: String,
}

/// Outcome of [`export_batch`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ExportReport {
    pub written: Vec<ExportedFileInfo>,
    pub failed: Vec<ExportFailureInfo>,
    pub cancelled: bool,
    /// Which renderer developed the RAWs, e.g. `"develop (Apple M4 Pro)"` or
    /// `"develop (CPU rendering (…))"`.
    pub renderer: String,
    /// JPEG encoder used: `"imageio"` (hardware) or `"mozjpeg"`.
    pub jpeg_encoder: String,
}

/// What the app's encoder produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum EncodedImageFormat {
    Jpeg,
    Heic,
}

/// Implemented in Swift with `ImageIO` (JPEG through `ImageIO`'s encoder, HEIC on
/// the media engine) and Core Image. Called on an export worker thread (never
/// the main thread); must be thread-safe.
#[uniffi::export(with_foreign)]
#[allow(clippy::too_many_arguments)] // one call per output: flat is simplest across the FFI
pub trait ImageEncoder: Send + Sync {
    /// Encode `width`×`height` interleaved RGB, 16 bits per sample in native
    /// byte order (`rgb16.len() == width * height * 6`), output-encoded in the
    /// space of `icc`. Embed `icc`, `exif` (TIFF-structured, as in a JPEG APP1
    /// after `Exif\0\0`) when given, and `keywords` and the set `metadata` fields
    /// (IPTC/XMP). Returns the file bytes.
    fn encode(
        &self,
        format: EncodedImageFormat,
        width: u32,
        height: u32,
        rgb16: Vec<u8>,
        quality: u8,
        icc: Vec<u8>,
        exif: Option<Vec<u8>>,
        keywords: Vec<String>,
        metadata: PhotoMetadata,
    ) -> Result<Vec<u8>, DsError>;

    /// The upright size of a non-RAW original, from its header (no decode).
    fn upright_size(&self, path: String) -> Result<ImageDimensions, DsError>;

    /// Decode a non-RAW original (JPEG, HEIF, PNG, TIFF) upright (its EXIF
    /// orientation applied), color-managed into `space`: at full size, or resized
    /// and sharpened to `target` (see [`DecodeTarget`]).
    fn decode(
        &self,
        path: String,
        space: ExportColorSpace,
        target: Option<DecodeTarget>,
    ) -> Result<DecodedImage, DsError>;
}

/// `ds_export`'s encoder hook over the app's [`ImageEncoder`].
struct Hook {
    encoder: Arc<dyn ImageEncoder>,
    format: EncodedImageFormat,
}

impl HeicEncoder for Hook {
    fn encode(
        &self,
        img: &Image16,
        quality: u8,
        icc: &[u8],
        exif: Option<&[u8]>,
        keywords: &[String],
        iptc: &IptcMeta,
    ) -> Result<Vec<u8>, String> {
        let bytes = ne_bytes(&img.data);
        self.encoder
            .encode(
                self.format,
                img.size.width,
                img.size.height,
                bytes,
                quality,
                icc.to_vec(),
                exif.map(<[u8]>::to_vec),
                keywords.to_vec(),
                iptc.clone().into(),
            )
            .map_err(|e| e.to_string())
    }
}

/// `u16` samples as native-endian bytes (one straight copy).
fn ne_bytes(data: &[u16]) -> Vec<u8> {
    let mut out = vec![0u8; data.len() * 2];
    for (d, v) in out.as_chunks_mut::<2>().0.iter_mut().zip(data) {
        *d = v.to_ne_bytes();
    }
    out
}

// ---------------------------------------------------------------- renderer ---

/// The render seam of an export (see [`DevelopRenderer`]).
pub trait ExportRenderer: Renderer + Send {
    /// Short identifier shown in reports.
    fn name(&self) -> String;
    /// Stop renders in flight (cancel was requested); later renders fail fast.
    fn cancel_now(&self) {}
    /// AI Upscale for recipes that ask for it (`None`: they resample).
    fn upscaler(&self) -> Option<&dyn ds_export::Upscaler> {
        None
    }
}

/// What kind of original a path is, by extension (the file is not opened).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// JPEG, HEIF, PNG, TIFF: decoded at full size.
    Raster,
    Video,
    /// Anything else is treated as a RAW (developed).
    Raw,
}

pub(crate) fn source_kind(path: &Path) -> SourceKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "heic" | "heif" | "hif" | "png" | "tif" | "tiff" => SourceKind::Raster,
        "mp4" | "mov" | "m4v" | "mts" | "m2ts" | "avi" | "mxf" | "3gp" => SourceKind::Video,
        _ => SourceKind::Raw,
    }
}

/// Unsharp-mask radius of the recipe's output sharpening (same ~0.8 px blur as
/// `ds_export::process::sharpen_in_place`).
pub const OUTPUT_SHARPEN_RADIUS: f32 = 0.8;

/// Bounding box of a "full size" develop: never reached (the pipeline doesn't
/// upscale past 1:1), so the output is the whole developed frame.
const FULL: Size = Size::new(1 << 16, 1 << 16);

pub(crate) type Loader = Arc<dyn Fn(&Path) -> Result<Loaded, DsError> + Send + Sync>;

/// One photo to develop: its file and its adjustments (or why they're unusable).
type Source = (PathBuf, Result<Adjustments, String>);

/// AI mask bitmaps for the photos of an export (Apple Vision on the app side, the
/// viewer's [`DevelopMask`] format). Called from an export thread; `None` when a mask
/// isn't available (that AI shape is then empty, as in the viewer).
#[uniffi::export(with_foreign)]
pub trait ExportMaskSource: Send + Sync {
    fn ai_mask(&self, source_id: u64, target: DevelopAiTarget) -> Option<DevelopMask>;
    /// A people target for one person of the photo (see `DevelopMaskSource`).
    fn ai_person_mask(
        &self,
        source_id: u64,
        target: DevelopAiTarget,
        person: u32,
    ) -> Option<DevelopMask>;
    /// The photo's depth map for the lens blur (see `DevelopMaskSource::depth_map`).
    fn depth_map(&self, source_id: u64) -> Option<DevelopMask>;
}

/// One photo's view of an [`ExportMaskSource`], as the pipeline's `MaskSource`.
struct PhotoMasks {
    source: Arc<dyn ExportMaskSource>,
    id: u64,
}

impl ds_pipeline::MaskSource for PhotoMasks {
    fn ai_mask(&self, target: AiTarget) -> Option<Arc<MaskBitmap>> {
        crate::viewer::mask_bitmap(self.source.ai_mask(self.id, target.into())?)
    }

    fn ai_person_mask(&self, target: AiTarget, person: Option<u32>) -> Option<Arc<MaskBitmap>> {
        match person {
            None => self.ai_mask(target),
            Some(p) => {
                crate::viewer::mask_bitmap(self.source.ai_person_mask(self.id, target.into(), p)?)
            }
        }
    }

    fn depth_map(&self) -> Option<Arc<MaskBitmap>> {
        crate::viewer::mask_bitmap(self.source.depth_map(self.id)?)
    }
}

/// Develops RAWs through `ds-pipeline` (see the module docs); other originals
/// are decoded at full size.
pub struct DevelopRenderer {
    sources: HashMap<u64, Source>,
    cancel: Arc<AtomicBool>,
    pipeline_cancel: ds_pipeline::CancelToken,
    /// Cancels AI Denoise the export is waiting for (`denoise::for_export`).
    denoise_cancel: Arc<CancelToken>,
    decoder: Option<Arc<dyn ImageEncoder>>,
    loader: Loader,
    masks: Option<Arc<dyn ExportMaskSource>>,
    /// JPEG is encoded by mozjpeg from an 8-bit render (not the app's 16-bit hook).
    eight_bit_jpeg: bool,
    upscaler: crate::upscale::ExportUpscaler,
}

impl std::fmt::Debug for DevelopRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevelopRenderer")
            .field("sources", &self.sources.len())
            .field("decoder", &self.decoder.is_some())
            .finish_non_exhaustive()
    }
}

impl DevelopRenderer {
    /// `sources`: photo id → (file, adjustments JSON or `None` for the defaults).
    pub fn new(
        sources: HashMap<u64, (PathBuf, Option<String>)>,
        cancel: Arc<AtomicBool>,
        decoder: Option<Arc<dyn ImageEncoder>>,
    ) -> Self {
        Self::with_loader(
            sources,
            cancel,
            decoder,
            Arc::new(|p: &Path| crate::develop::develop().load(p)),
        )
    }

    pub(crate) fn with_loader(
        sources: HashMap<u64, (PathBuf, Option<String>)>,
        cancel: Arc<AtomicBool>,
        decoder: Option<Arc<dyn ImageEncoder>>,
        loader: Loader,
    ) -> Self {
        let sources = sources
            .into_iter()
            .map(|(id, (path, json))| {
                let adj = json.map_or_else(
                    || Ok(Adjustments::default()),
                    |j| crate::session::parse_adjustments(&j).map_err(|e| e.to_string()),
                );
                (id, (path, adj))
            })
            .collect();
        Self {
            sources,
            cancel,
            pipeline_cancel: ds_pipeline::CancelToken::new(),
            denoise_cancel: CancelToken::new(),
            decoder,
            loader,
            masks: None,
            eight_bit_jpeg: true,
            upscaler: crate::upscale::ExportUpscaler::new(crate::upscale::provider()),
        }
    }

    /// JPEG goes through the app's (16-bit) encoder hook: render it at 16 bits.
    #[must_use]
    pub fn with_hardware_jpeg(mut self, hardware_jpeg: bool) -> Self {
        self.eight_bit_jpeg = !hardware_jpeg;
        self
    }

    /// AI masks for layers with AI shapes (without: those shapes are empty).
    #[must_use]
    pub fn with_masks(mut self, masks: Option<Arc<dyn ExportMaskSource>>) -> Self {
        self.masks = masks;
        self
    }

    /// A depth lens blur exports only with the photo's depth map: without one the photo
    /// fails, and says why, instead of exporting unblurred (asking makes the map now).
    fn depth_ready(&self, id: u64, adj: &Adjustments) -> Result<(), String> {
        if !crate::tools::needs_depth(adj)
            || self
                .masks
                .as_ref()
                .is_some_and(|m| m.depth_map(id).is_some())
        {
            return Ok(());
        }
        Err("not exported: its lens blur's depth map couldn't be made on this Mac".into())
    }

    fn masks_for(&self, id: u64) -> Option<Arc<dyn ds_pipeline::MaskSource>> {
        self.masks
            .clone()
            .map(|source| Arc::new(PhotoMasks { source, id }) as Arc<dyn ds_pipeline::MaskSource>)
    }

    fn source(&self, id: u64) -> Result<(&Path, &Adjustments), String> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let (path, adj) = self
            .sources
            .get(&id)
            .ok_or_else(|| format!("no file for photo {id}"))?;
        let adj = adj.as_ref().map_err(Clone::clone)?;
        Ok((path, adj))
    }

    fn request(
        &self,
        loaded: &Loaded,
        space: OutputSpace,
        max: Size,
        sharpen: f32,
        fit: Fit,
    ) -> RenderRequest {
        RenderRequest {
            fit,
            quality: Quality::Full,
            output: match space {
                OutputSpace::Srgb => PipelineSpace::Srgb,
                OutputSpace::DisplayP3 => PipelineSpace::DisplayP3,
                OutputSpace::Rec2020 => PipelineSpace::Rec2020,
            },
            cancel: Some(self.pipeline_cancel.clone()),
            capture: loaded.capture.clone(),
            priority: Priority::Background,
            output_sharpening: (sharpen > 0.0).then_some(OutputSharpening {
                amount: sharpen,
                radius: OUTPUT_SHARPEN_RADIUS,
            }),
            ..RenderRequest::new(max)
        }
    }

    /// Develop into a buffer the export encodes directly (no copy): 8-bit RGBA
    /// (packed on the GPU) when `eight_bit`, else 16-bit RGB.
    fn develop(
        loaded: &Loaded,
        adj: &Adjustments,
        req: &RenderRequest,
        eight_bit: bool,
    ) -> Result<Rendered, String> {
        let size = ds_pipeline::output_size(&loaded.raw, adj, req).map_err(|e| e.to_string())?;
        let develop = crate::develop::develop();
        if eight_bit {
            let mut data = vec![0u8; size.pixels() as usize * 4];
            let frame = develop
                .render(loaded, adj, req, OutputFormat::Rgba8, &mut data)
                .map_err(|e| e.to_string())?;
            return Ok(Rendered::Rgba8 {
                size: frame.size,
                data,
            });
        }
        let mut data = vec![0u16; size.pixels() as usize * 3];
        let frame = develop
            .render(
                loaded,
                adj,
                req,
                OutputFormat::Rgb16,
                bytemuck::cast_slice_mut(&mut data),
            )
            .map_err(|e| e.to_string())?;
        Ok(Rendered::Rgb16 {
            size: frame.size,
            data,
        })
    }

    /// A non-RAW original at `recipe`'s size and sharpening, decoded, resized and
    /// sharpened by the app's decoder (Core Image). `None` (the full-size path, the
    /// batch resizes on the CPU) without a decoder, for an upscale, when the decoder
    /// can't size it (its size is unreadable, or past the GPU's limits), or when the
    /// sized decode fails (the full-size decode then reports why, if it fails too).
    fn raster_for(&self, path: &Path, recipe: &ExportRecipe) -> Option<Result<Rendered, String>> {
        let d = self.decoder.as_ref()?;
        let dims = d.upright_size(path.to_string_lossy().into_owned()).ok()?;
        let full = Size::new(dims.width, dims.height);
        let target = recipe.target_size(full).ok()?;
        if target.width > full.width || target.height > full.height {
            return None;
        }
        self.raster(
            path,
            recipe.space,
            Some(DecodeTarget {
                width: target.width,
                height: target.height,
                sharpen: recipe.sharpen,
            }),
        )
        .ok()
        .map(Ok)
    }

    fn raster(
        &self,
        path: &Path,
        space: OutputSpace,
        target: Option<DecodeTarget>,
    ) -> Result<Rendered, String> {
        if let Some(d) = &self.decoder {
            // The app decodes into sRGB or P3; a Rec.2020 recipe takes P3 (originals
            // aren't wider) converted below.
            let img = d
                .decode(
                    path.to_string_lossy().into_owned(),
                    match space {
                        OutputSpace::Srgb => ExportColorSpace::Srgb,
                        OutputSpace::DisplayP3 | OutputSpace::Rec2020 => {
                            ExportColorSpace::DisplayP3
                        }
                    },
                    target,
                )
                .map_err(|e| e.to_string())?;
            let n = img.width as usize * img.height as usize * 3;
            if img.rgb16.len() != n * 2 || n == 0 {
                return Err("decoder returned a short image".into());
            }
            if let Some(t) = target
                && (img.width, img.height) != (t.width, t.height)
            {
                return Err(format!(
                    "decoder returned {}x{}, not {}x{}",
                    img.width, img.height, t.width, t.height
                ));
            }
            let mut data: Vec<u16> = img
                .rgb16
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_ne_bytes(*b))
                .collect();
            if space == OutputSpace::Rec2020 {
                ds_export::proof::convert_space(
                    OutputSpace::DisplayP3,
                    space,
                    &mut data,
                    img.width as usize,
                )
                .map_err(|e| e.to_string())?;
            }
            return Ok(Rendered::Rgb16 {
                size: Size::new(img.width, img.height),
                data,
            });
        }
        // Without the app's decoder: JPEG only (treated as sRGB).
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        if !bytes.starts_with(&[0xFF, 0xD8]) {
            return Err("this image type needs the app's decoder".into());
        }
        let (size, rgb) = decode_jpeg_rgb(&bytes)?;
        let orientation = exif_block(path).map_or(1, |e| e.orientation);
        let (size, rgb) = orient(size, &rgb, orientation);
        let mut data = to_space(&rgb, space);
        if space == OutputSpace::Rec2020 {
            ds_export::proof::convert_space(
                OutputSpace::Srgb,
                space,
                &mut data,
                size.width as usize,
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(Rendered::Rgb16 { size, data })
    }
}

impl ExportRenderer for DevelopRenderer {
    fn name(&self) -> String {
        format!("develop ({})", crate::develop::develop().backend())
    }

    fn cancel_now(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.pipeline_cancel.cancel();
        self.denoise_cancel.cancel();
        self.upscaler.cancel();
    }

    fn upscaler(&self) -> Option<&dyn ds_export::Upscaler> {
        Some(&self.upscaler)
    }
}

impl Renderer for DevelopRenderer {
    /// Full size: RAWs developed at 1:1 (the batch resizes); other originals decoded.
    fn render(&self, item: &ExportItem, space: OutputSpace) -> Result<Rendered, String> {
        let (path, adj) = self.source(item.id)?;
        match source_kind(path) {
            SourceKind::Video => Err("videos aren't exported".into()),
            SourceKind::Raster => self.raster(path, space, None),
            SourceKind::Raw => {
                self.depth_ready(item.id, adj)?;
                let loaded = (self.loader)(path).map_err(|e| e.to_string())?;
                // AI Denoise: made now if missing, exactly what is rendered (or an error).
                let denoised =
                    crate::denoise::for_export(path, &loaded, adj, &self.denoise_cancel)?;
                let (loaded, adj) = denoised
                    .as_ref()
                    .map_or((loaded.clone(), adj), |(l, a)| (l.clone(), a));
                let _blend = crate::denoise::ExportBlend(denoised.as_ref().map(|(l, _)| l.key));
                let req = RenderRequest {
                    masks: self.masks_for(item.id),
                    ..self.request(&loaded, space, FULL, 0.0, Fit::FillOriginalSize)
                };
                Self::develop(&loaded, adj, &req, false)
            }
        }
    }

    /// RAWs: developed at the recipe's size with its sharpening on the GPU; other
    /// originals: resized and sharpened by the app's decoder (see `raster_for`). An
    /// upscaling recipe (and videos) take the full-size path.
    fn render_for(
        &self,
        item: &ExportItem,
        recipe: &ExportRecipe,
    ) -> Option<Result<Rendered, String>> {
        let (path, adj) = match self.source(item.id) {
            Ok(s) => s,
            Err(e) => return Some(Err(e)),
        };
        match source_kind(path) {
            SourceKind::Raw => {
                if let Err(e) = self.depth_ready(item.id, adj) {
                    return Some(Err(e));
                }
            }
            SourceKind::Raster => return self.raster_for(path, recipe),
            SourceKind::Video => return None,
        }
        let fit = if recipe.constrain_to_sensor_pixels {
            Fit::ConstrainToSensorPixels
        } else {
            Fit::FillOriginalSize
        };
        let run = || -> Result<Option<Rendered>, String> {
            let loaded = (self.loader)(path).map_err(|e| e.to_string())?;
            // AI Denoise: made now if missing, exactly what is rendered (or an error).
            let denoised = crate::denoise::for_export(path, &loaded, adj, &self.denoise_cancel)?;
            let (loaded, adj) = denoised
                .as_ref()
                .map_or((loaded.clone(), adj), |(l, a)| (l.clone(), a));
            let _blend = crate::denoise::ExportBlend(denoised.as_ref().map(|(l, _)| l.key));
            let full_req = self.request(&loaded, recipe.space, FULL, 0.0, fit);
            let full =
                ds_pipeline::output_size(&loaded.raw, adj, &full_req).map_err(|e| e.to_string())?;
            let target = recipe.target_size(full).map_err(|e| e.to_string())?;
            if target.width > full.width || target.height > full.height {
                return Ok(None); // upscale: the batch resizes the 1:1 develop
            }
            // The pipeline fits the frame inside a box; relax the dimension the recipe
            // didn't fix by a pixel so the one it did (e.g. "800 px long edge") comes out
            // exact (the box rounding can otherwise lose a pixel).
            let fits = |b: Size| {
                ds_pipeline::output_size(
                    &loaded.raw,
                    adj,
                    &self.request(&loaded, recipe.space, b, 0.0, fit),
                )
                .is_ok_and(|s| s == target)
            };
            let box_ = [
                Size::new(target.width, target.height + 1),
                Size::new(target.width + 1, target.height),
            ]
            .into_iter()
            .find(|b| fits(*b))
            .unwrap_or(target);
            let req = RenderRequest {
                masks: self.masks_for(item.id),
                ..self.request(&loaded, recipe.space, box_, recipe.sharpen, fit)
            };
            // An output profile converts 16-bit pixels (one quantization, after it).
            let eight_bit = recipe.output_profile.is_none()
                && match recipe.format {
                    ExportFormat::Png8 => true,
                    ExportFormat::Jpeg { .. } => self.eight_bit_jpeg,
                    _ => false,
                };
            Self::develop(&loaded, adj, &req, eight_bit).map(Some)
        };
        run().transpose()
    }
}

/// Largest JPEG source decoded for an export (a hostile header can claim
/// 65535 x 65535, which is 12 GB of RGB).
const MAX_JPEG_PIXELS: usize = 300_000_000;

/// Decode a baseline/progressive JPEG to interleaved RGB8.
fn decode_jpeg_rgb(jpeg: &[u8]) -> Result<(Size, Vec<u8>), String> {
    // libjpeg reports errors by unwinding; contain them.
    let r = std::panic::catch_unwind(|| -> std::io::Result<(Size, Vec<u8>)> {
        let mut img = mozjpeg::Decompress::new_mem(jpeg)?.rgb()?;
        let (w, h) = (img.width(), img.height());
        // The header's size is the file's claim: refuse before allocating for it.
        if w * h > MAX_JPEG_PIXELS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{w}x{h} is too large to export"),
            ));
        }
        let data: Vec<u8> = img.read_scanlines()?;
        img.finish()?;
        Ok((Size::new(w as u32, h as u32), data))
    });
    match r {
        Ok(Ok((size, data))) if data.len() == size.width as usize * size.height as usize * 3 => {
            Ok((size, data))
        }
        Ok(Ok(_)) => Err("embedded preview: short decode".into()),
        Ok(Err(e)) => Err(format!("embedded preview: {e}")),
        Err(_) => Err("embedded preview: undecodable JPEG".into()),
    }
}

/// Apply EXIF orientation (1..=8) to interleaved RGB8.
fn orient(size: Size, rgb: &[u8], exif: u16) -> (Size, Vec<u8>) {
    let (w, h) = (size.width as usize, size.height as usize);
    if !(2..=8).contains(&exif) {
        return (size, rgb.to_vec());
    }
    let swap = exif >= 5;
    let (ow, oh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u8; rgb.len()];
    for dy in 0..oh {
        for dx in 0..ow {
            // Destination pixel → source pixel for each EXIF orientation.
            let (sx, sy) = match exif {
                2 => (w - 1 - dx, dy),
                3 => (w - 1 - dx, h - 1 - dy),
                4 => (dx, h - 1 - dy),
                5 => (dy, dx),
                6 => (dy, h - 1 - dx),
                7 => (w - 1 - dy, h - 1 - dx),
                _ => (w - 1 - dy, dx), // 8
            };
            let (s, d) = ((sy * w + sx) * 3, (dy * ow + dx) * 3);
            out[d..d + 3].copy_from_slice(&rgb[s..s + 3]);
        }
    }
    (Size::new(ow as u32, oh as u32), out)
}

/// sRGB-encoded RGB8 → 16-bit samples in `space` (Display P3 shares the sRGB
/// transfer curve; primaries are converted in linear light, D65).
/// Rec.2020 is left in sRGB for the caller to convert.
fn to_space(rgb: &[u8], space: OutputSpace) -> Vec<u16> {
    match space {
        OutputSpace::Srgb | OutputSpace::Rec2020 => {
            rgb.iter().map(|&v| u16::from(v) * 257).collect()
        }
        OutputSpace::DisplayP3 => {
            const M: [[f64; 3]; 3] = [
                [0.822_462, 0.177_538, 0.0],
                [0.033_194, 0.966_806, 0.0],
                [0.017_083, 0.072_397, 0.910_520],
            ];
            let lin: Vec<f64> = (0..=255u8)
                .map(|v| ds_export::process::srgb_to_linear(f64::from(v) / 255.0))
                .collect();
            let mut out = Vec::with_capacity(rgb.len());
            for &[r, g, b] in rgb.as_chunks::<3>().0 {
                let l = [lin[r as usize], lin[g as usize], lin[b as usize]];
                for row in &M {
                    let v = row[0] * l[0] + row[1] * l[1] + row[2] * l[2];
                    let e = ds_export::process::linear_to_srgb(v.clamp(0.0, 1.0));
                    out.push((e * 65535.0).round() as u16);
                }
            }
            out
        }
    }
}

// ------------------------------------------------------------------ recipes ---

pub(crate) fn recipe_of(r: &ExportRecipeSpec) -> Result<ExportRecipe, DsError> {
    let dest = PathBuf::from(&r.destination);
    if !dest.is_absolute() {
        return Err(DsError::invalid(format!(
            "{}: destination must be an absolute path",
            r.name
        )));
    }
    let format = match r.format {
        ExportFileFormat::Jpeg { quality } => ExportFormat::Jpeg { quality },
        ExportFileFormat::Heic { quality } => ExportFormat::Heic { quality },
        ExportFileFormat::Tiff16 => ExportFormat::Tiff16,
        ExportFileFormat::Png8 => ExportFormat::Png8,
        ExportFileFormat::Png16 => ExportFormat::Png16,
    };
    let mut out = ExportRecipe::new(&r.name, format, dest);
    out.sizing = match r.size {
        ExportSize::Original => Sizing::Original,
        ExportSize::LongEdge { pixels } => Sizing::LongEdge(pixels),
        ExportSize::ShortEdge { pixels } => Sizing::ShortEdge(pixels),
        ExportSize::Megapixels { megapixels } => Sizing::Megapixels(megapixels),
        ExportSize::Percent { percent } => Sizing::Percent(percent),
    };
    out.allow_upscale = r.allow_upscale;
    out.space = match r.space {
        ExportColorSpace::Srgb => OutputSpace::Srgb,
        ExportColorSpace::DisplayP3 => OutputSpace::DisplayP3,
    };
    out.sharpen = r.sharpen;
    out.file_name = Template::parse(&r.file_name_template)
        .map_err(|e| DsError::invalid(format!("{}: file name: {e}", r.name)))?;
    out.exif = ExifPolicy {
        include: r.include_exif,
        strip_gps: r.strip_gps,
    };
    out.constrain_to_sensor_pixels = r.constrain_to_sensor_pixels;
    if let Some(p) = &r.output_profile {
        let profile = IccProfile::read(Path::new(&p.path))
            .and_then(|icc| OutputProfile::new(icc, p.settings()))
            .map_err(|e| DsError::invalid(format!("{}: {e}", r.name)))?;
        out.output_profile = Some(profile);
        // Rendered in the working gamut, so nothing outside P3 is clipped before the
        // conversion to a wider profile.
        out.space = OutputSpace::Rec2020;
    }
    out.ai_upscale = r.ai_upscale;
    out.watermark = r
        .watermark
        .as_ref()
        .map(watermark_of)
        .transpose()
        .map_err(|e| DsError::invalid(format!("{}: {e}", r.name)))?;
    out.post_process = crate::plugins::resolve_steps(&r.post_process)
        .map_err(|e| DsError::invalid(format!("{}: {e}", r.name)))?;
    out.validate()
        .map_err(|e| DsError::invalid(e.to_string()))?;
    Ok(out)
}

/// What an EXIF block (JPEG, HEIF, PNG, TIFF) says, for export.
pub(crate) struct ExifBlock {
    pub meta: ds_export::ExifMeta,
    pub orientation: u16,
}

/// Read the EXIF block of a non-RAW original with `kamadak-exif`.
pub(crate) fn exif_block(path: &Path) -> Option<ExifBlock> {
    use exif::{In, Tag, Value};
    let f = std::fs::File::open(path).ok()?;
    let e = exif::Reader::new()
        .read_from_container(&mut std::io::BufReader::new(f))
        .ok()?;
    let field = |t| e.get_field(t, In::PRIMARY);
    let ascii = |t| match field(t).map(|f| &f.value) {
        Some(Value::Ascii(v)) => v
            .first()
            .map(|b| String::from_utf8_lossy(b).trim().to_owned())
            .filter(|s| !s.is_empty()),
        _ => None,
    };
    let rationals = |t| match field(t).map(|f| &f.value) {
        Some(Value::Rational(v)) => Some(v.iter().map(|r| (r.num, r.denom)).collect::<Vec<_>>()),
        _ => None,
    };
    let real = |t| {
        rationals(t)
            .and_then(|v| v.first().copied())
            .filter(|(_, d)| *d != 0)
            .map(|(n, d)| f64::from(n) / f64::from(d))
    };
    let uint = |t| field(t).and_then(|f| f.value.get_uint(0));
    let capture_time = ascii(Tag::DateTimeOriginal).and_then(|s| {
        chrono::NaiveDateTime::parse_from_str(s.get(..19)?, "%Y:%m:%d %H:%M:%S").ok()
    });
    let offset_original = ascii(Tag::OffsetTimeOriginal).and_then(|s| {
        let b = s.as_bytes();
        if b.len() != 6 || b[3] != b':' {
            return None;
        }
        let sign = match b[0] {
            b'+' => 1i16,
            b'-' => -1,
            _ => return None,
        };
        let h: i16 = s.get(1..3)?.parse().ok()?;
        let m: i16 = s.get(4..6)?.parse().ok()?;
        (h <= 14 && m < 60).then_some(sign * (h * 60 + m))
    });
    let coord = |val, reference, neg: &str| -> Option<f64> {
        let v = rationals(val)?;
        let [d, m, s] = v.get(..3)? else { return None };
        let part = |(n, d): (u32, u32)| (d != 0).then(|| f64::from(n) / f64::from(d));
        let deg = part(*d)? + part(*m)? / 60.0 + part(*s)? / 3600.0;
        let r = ascii(reference)?;
        Some(if r.eq_ignore_ascii_case(neg) {
            -deg
        } else {
            deg
        })
    };
    let gps = coord(Tag::GPSLatitude, Tag::GPSLatitudeRef, "S")
        .zip(coord(Tag::GPSLongitude, Tag::GPSLongitudeRef, "W"))
        .filter(|(la, lo)| la.abs() <= 90.0 && lo.abs() <= 180.0)
        .map(|(latitude, longitude)| Gps {
            latitude,
            longitude,
            altitude_m: real(Tag::GPSAltitude).map(|a| {
                if uint(Tag::GPSAltitudeRef) == Some(1) {
                    -a
                } else {
                    a
                }
            }),
        });
    let meta = ds_export::ExifMeta {
        make: ascii(Tag::Make),
        model: ascii(Tag::Model),
        lens_make: ascii(Tag::LensMake),
        lens_model: ascii(Tag::LensModel),
        subsec_original: capture_time
            .and(ascii(Tag::SubSecTimeOriginal))
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit())),
        capture_time,
        offset_original: capture_time.and(offset_original),
        exposure_time: rationals(Tag::ExposureTime)
            .and_then(|v| v.first().copied())
            .filter(|(n, d)| *n > 0 && *d > 0),
        f_number: real(Tag::FNumber).map(|v| v as f32).filter(|v| *v > 0.0),
        iso: uint(Tag::PhotographicSensitivity).filter(|v| *v > 0),
        focal_length_mm: real(Tag::FocalLength)
            .map(|v| v as f32)
            .filter(|v| *v > 0.0),
        artist: ascii(Tag::Artist),
        copyright: ascii(Tag::Copyright),
        gps,
        keywords: Vec::new(),
        iptc: IptcMeta::default(),
    };
    Some(ExifBlock {
        meta,
        orientation: uint(Tag::Orientation).map_or(1, |v| v as u16),
    })
}

/// Capture metadata of one original, as the export writes it.
pub(crate) fn meta_of(path: &Path) -> ds_export::ExifMeta {
    match source_kind(path) {
        SourceKind::Raw => ds_raw::read_metadata(path)
            .map(|m| ds_export::exif_from_raw(&m))
            .unwrap_or_default(),
        SourceKind::Raster => exif_block(path).map(|e| e.meta).unwrap_or_default(),
        SourceKind::Video => ds_export::ExifMeta::default(),
    }
}

fn item_of(s: &ExportSource) -> ExportItem {
    let path = Path::new(&s.path);
    let mut meta = meta_of(path);
    // The caller's position/credits win over what the file says.
    if let Some(g) = s.gps {
        meta.gps = Some(Gps {
            latitude: g.latitude,
            longitude: g.longitude,
            altitude_m: g.altitude_m,
        });
    }
    if s.artist.is_some() {
        meta.artist.clone_from(&s.artist);
    }
    if s.copyright.is_some() {
        meta.copyright.clone_from(&s.copyright);
    }
    meta.keywords.clone_from(&s.keywords);
    if let Some(m) = &s.metadata {
        meta.iptc = m.clone().into();
    }
    ExportItem {
        id: s.id,
        name: path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        seq: s.seq,
        meta,
    }
}

/// Check recipes before a run (bad template, relative destination, quality or
/// size out of range): the message of the first problem, or `None`.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn validate_export_recipes(recipes: Vec<ExportRecipeSpec>) -> Result<Option<String>, DsError> {
    guard(|| {
        Ok(recipes
            .iter()
            .map(recipe_of)
            .find_map(Result::err)
            .map(|e| e.to_string()))
    })
}

const SAMPLE_TIME: &str = "%Y-%m-%dT%H:%M:%S";

/// The file name `template` gives a photo (for the sheet's live preview).
/// `capture_time` is [`ExportNameSample::capture_time`] (`None`: no date).
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn export_file_name_preview(
    template: String,
    name: String,
    seq: u64,
    camera: Option<String>,
    capture_time: Option<String>,
    extension: String,
) -> Result<String, DsError> {
    guard(|| {
        let t = Template::parse(&template).map_err(|e| DsError::invalid(e.to_string()))?;
        let datetime = capture_time
            .as_deref()
            .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, SAMPLE_TIME).ok());
        Ok(t.render_file_name(&Vars {
            name: &name,
            seq,
            datetime,
            camera: camera.as_deref(),
            ext: &extension,
        }))
    })
}

/// `{name}`, `{camera}` and the capture time of the original at `path`,
/// read the way the export reads them.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn export_name_sample(path: String) -> Result<ExportNameSample, DsError> {
    guard(|| {
        let item = item_of(&ExportSource {
            id: 0,
            path,
            seq: 1,
            gps: None,
            artist: None,
            copyright: None,
            adjustments_json: None,
            keywords: Vec::new(),
            metadata: None,
        });
        Ok(ExportNameSample {
            name: item.name,
            camera: item.meta.model,
            capture_time: item
                .meta
                .capture_time
                .map(|t| t.format(SAMPLE_TIME).to_string()),
            artist: item.meta.artist,
            copyright: item.meta.copyright,
        })
    })
}

// -------------------------------------------------------------------- run ---

/// Export `sources` × `recipes`. Progress (`kind` `"export"`, per photo) goes
/// to `listener`; `token` cancels between outputs (files already written stay,
/// no temp files remain). HEIC recipes need `encoder`; with
/// `hardware_jpeg` JPEG also goes through it instead of mozjpeg. `masks` provides
/// the AI mask bitmaps of layers with AI shapes; `text` draws text watermarks.
#[uniffi::export(default(masks = None, text = None))]
#[allow(clippy::too_many_arguments)] // one flat call across the FFI
pub async fn export_batch(
    sources: Vec<ExportSource>,
    recipes: Vec<ExportRecipeSpec>,
    encoder: Option<Arc<dyn ImageEncoder>>,
    hardware_jpeg: bool,
    listener: Option<Arc<dyn ProgressListener>>,
    token: Arc<CancelToken>,
    masks: Option<Arc<dyn ExportMaskSource>>,
    text: Option<Arc<dyn WatermarkTextRenderer>>,
) -> Result<ExportReport, DsError> {
    let recipes = recipes
        .iter()
        .map(recipe_of)
        .collect::<Result<Vec<_>, _>>()?;
    let events = Arc::new(Dispatcher::new());
    events.set_listener(listener);
    let ev = events.clone();
    let watch = token.clone();
    let r = run_job_on_thread("ds-export", token, move |_| {
        let Some(items) = read_items(&sources, &ev, &|| watch.is_cancelled()) else {
            return Ok(cancelled_report(&ev, sources.len() as u64, 0));
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let renderer = DevelopRenderer::new(
            sources
                .iter()
                .map(|s| (s.id, (PathBuf::from(&s.path), s.adjustments_json.clone())))
                .collect(),
            cancel.clone(),
            encoder.clone(),
        )
        .with_masks(masks)
        .with_hardware_jpeg(hardware_jpeg && encoder.is_some());
        let text = text.map(TextHook);
        Ok(run_with_text(
            &items,
            &recipes,
            &renderer,
            encoder,
            hardware_jpeg,
            text.as_ref().map(|t| t as &dyn ds_export::TextRasterizer),
            &ev,
            &|| watch.is_cancelled(),
            &cancel,
        ))
    })
    .await;
    events.flush(Duration::from_secs(5));
    r
}

/// Read every source's metadata (kind `"export.metadata"` progress per
/// photo). `None` when cancelled first.
fn read_items(
    sources: &[ExportSource],
    events: &Dispatcher,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<ExportItem>> {
    let total = sources.len() as u64;
    let emit = |phase, done| {
        events.emit(ProgressEvent {
            task_id: 0,
            kind: "export.metadata".into(),
            phase,
            completed: done,
            total,
        });
    };
    let mut items = Vec::with_capacity(sources.len());
    for (i, s) in sources.iter().enumerate() {
        if cancelled() {
            emit(TaskPhase::Cancelled, i as u64);
            return None;
        }
        items.push(item_of(s));
        emit(TaskPhase::Running, i as u64 + 1);
    }
    Some(items)
}

/// The report of a run cancelled before any photo was rendered.
fn cancelled_report(events: &Dispatcher, total: u64, done: u64) -> ExportReport {
    events.emit(ProgressEvent {
        task_id: 0,
        kind: "export".into(),
        phase: TaskPhase::Cancelled,
        completed: done,
        total,
    });
    ExportReport {
        written: Vec::new(),
        failed: Vec::new(),
        cancelled: true,
        renderer: String::new(),
        jpeg_encoder: String::new(),
    }
}

/// Below this much free space at a destination an export doesn't start.
const MIN_FREE_BYTES: u64 = 64 << 20;

/// Free bytes on the volume holding `dir` (its nearest existing ancestor).
fn free_bytes(dir: &Path) -> Option<u64> {
    let existing = dir.ancestors().find(|p| p.exists())?;
    rustix::fs::statvfs(existing)
        .ok()
        .map(|s| s.f_bavail.saturating_mul(s.f_frsize))
}

fn bytes_text(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let v = n as f64;
    if v >= 1e9 {
        format!("{:.1} GB", v / 1e9)
    } else {
        format!("{:.0} MB", v / 1e6)
    }
}

/// Preflight: a destination with almost no space left refuses the
/// export up front, with one plain message, instead of failing every file.
fn preflight_space(
    recipes: &[ExportRecipe],
    free: &dyn Fn(&Path) -> Option<u64>,
) -> Option<ExportFailureInfo> {
    recipes.iter().enumerate().find_map(|(i, r)| {
        let left = free(&r.destination)?;
        (left < MIN_FREE_BYTES).then(|| ExportFailureInfo {
            source_id: None,
            recipe: Some(i as u32),
            message: format!(
                "The export disk is full ({} free at {}): nothing was exported. Free some space or choose another folder.",
                bytes_text(left),
                r.destination.display()
            ),
        })
    })
}

/// One plain line for a run that stopped on a full disk, in place of the raw
/// per-file errors.
fn disk_full_failure(written: usize, not_exported: usize, free: Option<u64>) -> ExportFailureInfo {
    let free = free.map_or(String::new(), |b| format!(" ({} free)", bytes_text(b)));
    ExportFailureInfo {
        source_id: None,
        recipe: None,
        message: format!(
            "The export disk is full{free}: stopped after {written} file{}; {not_exported} photo{} not exported.",
            if written == 1 { "" } else { "s" },
            if not_exported == 1 { " was" } else { "s were" },
        ),
    }
}

/// The batch itself (synchronous; the tests drive it directly), without a text renderer.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn run(
    items: &[ExportItem],
    recipes: &[ExportRecipe],
    renderer: &(dyn ExportRenderer + Sync),
    encoder: Option<Arc<dyn ImageEncoder>>,
    hardware_jpeg: bool,
    events: &Dispatcher,
    cancelled: &(dyn Fn() -> bool + Sync),
    cancel: &AtomicBool,
) -> ExportReport {
    run_with_text(
        items,
        recipes,
        renderer,
        encoder,
        hardware_jpeg,
        None,
        events,
        cancelled,
        cancel,
    )
}

/// The batch itself (synchronous); `text` draws text watermarks.
#[allow(clippy::too_many_arguments)]
fn run_with_text(
    items: &[ExportItem],
    recipes: &[ExportRecipe],
    renderer: &(dyn ExportRenderer + Sync),
    encoder: Option<Arc<dyn ImageEncoder>>,
    hardware_jpeg: bool,
    text: Option<&dyn ds_export::TextRasterizer>,
    events: &Dispatcher,
    cancelled: &(dyn Fn() -> bool + Sync),
    cancel: &AtomicBool,
) -> ExportReport {
    let heic = encoder.clone().map(|encoder| Hook {
        encoder,
        format: EncodedImageFormat::Heic,
    });
    let jpeg = encoder.filter(|_| hardware_jpeg).map(|encoder| Hook {
        encoder,
        format: EncodedImageFormat::Jpeg,
    });
    let opts = BatchOptions {
        max_in_flight: 2,
        heic: heic.as_ref().map(|h| h as &dyn HeicEncoder),
        jpeg: jpeg.as_ref().map(|h| h as &dyn HeicEncoder),
        upscaler: renderer.upscaler(),
        text,
        cancel: None,
    };
    let total = items.len() as u64;
    let emit = |phase, done| {
        events.emit(ProgressEvent {
            task_id: 0,
            kind: "export".into(),
            phase,
            completed: done,
            total,
        });
    };
    emit(TaskPhase::Started, 0);
    if let Some(full) = preflight_space(recipes, &free_bytes) {
        emit(TaskPhase::Finished, 0);
        return ExportReport {
            written: Vec::new(),
            failed: vec![full],
            cancelled: false,
            renderer: renderer.name(),
            jpeg_encoder: String::new(),
        };
    }
    let images_done = std::sync::atomic::AtomicU64::new(0);
    let finished = AtomicBool::new(false);
    let report = std::thread::scope(|s| {
        // The token → the batch's flag (checked between outputs).
        s.spawn(|| {
            while !finished.load(Ordering::Acquire) {
                if cancelled() {
                    cancel.store(true, Ordering::Release);
                    renderer.cancel_now();
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let r = ds_export::run_batch(
            items,
            recipes,
            renderer,
            opts,
            &|p| {
                images_done.fetch_max(p.images_done as u64, Ordering::Relaxed);
                emit(TaskPhase::Running, p.images_done as u64);
            },
            cancel,
        );
        finished.store(true, Ordering::Release);
        r
    });
    let cancelled = report.cancelled;
    // A cancelled run reports how far it got, never "N of N".
    if cancelled {
        emit(TaskPhase::Cancelled, images_done.load(Ordering::Relaxed));
    } else {
        emit(TaskPhase::Finished, total);
    }
    report_of(
        report,
        items.len(),
        recipes,
        renderer.name(),
        opts.jpeg.is_some(),
    )
}

/// The FFI report of a finished batch: a render cut short by cancel is not a
/// failure, and a full disk is one plain line instead of a raw error per file.
fn report_of(
    report: ds_export::BatchReport,
    items: usize,
    recipes: &[ExportRecipe],
    renderer: String,
    hardware_jpeg: bool,
) -> ExportReport {
    let cancelled = report.cancelled;
    let disk_full = report.disk_full.then(|| {
        let done: HashSet<u64> = report.written.iter().map(|w| w.item).collect();
        let dest = report
            .failed
            .iter()
            .find(|f| f.error.is_disk_full())
            .and_then(|f| f.recipe)
            .and_then(|r| recipes.get(r))
            .and_then(|r| free_bytes(&r.destination));
        disk_full_failure(report.written.len(), items - done.len(), dest)
    });
    ExportReport {
        written: report
            .written
            .into_iter()
            .map(|w| ExportedFileInfo {
                source_id: w.item,
                recipe: w.recipe as u32,
                path: w.path.to_string_lossy().into_owned(),
                width: w.size.width,
                height: w.size.height,
                bytes: w.bytes,
                watermark_skipped: w.watermark_skipped,
            })
            .collect(),
        failed: report
            .failed
            .into_iter()
            // A render cut short by cancel is not a failure.
            .filter(|f| {
                !(cancelled
                    && matches!(&f.error, ds_export::ExportError::Render(m) if m == "cancelled"))
            })
            // A full disk is one plain line, not a raw error per file.
            .filter(|f| !report.disk_full || !f.error.is_disk_full())
            .map(|f| ExportFailureInfo {
                source_id: f.item,
                recipe: f.recipe.map(|r| r as u32),
                message: f.error.to_string(),
            })
            .chain(disk_full)
            .collect(),
        cancelled,
        renderer,
        jpeg_encoder: if hardware_jpeg { "imageio" } else { "mozjpeg" }.into(),
    }
}

#[cfg(test)]
#[path = "export_tests.rs"]
pub(crate) mod tests;
