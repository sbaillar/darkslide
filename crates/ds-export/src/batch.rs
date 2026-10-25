//! Parallel batch export with bounded memory, progress and cancellation.
//!
//! Memory bound: each of the `max_in_flight` workers holds at most **one
//! full-resolution render plus one output-sized working copy** (the resized
//! and/or sharpened image; sharpening is in place, resizing streams rows).
//! Encoders convert in strips, so no third image-sized buffer exists. An
//! 8-bit RGBA render already sized for its recipe ([`Renderer::render_for`]) is
//! encoded as is to JPEG (without a hardware JPEG hook) and PNG8; otherwise it is
//! converted to 16-bit once, and during that conversion the 8-bit buffer (2/3 the
//! size of the 16-bit one) takes the working copy's place. `tests/memory.rs`
//! measures the peak with a counting allocator.
//! A batch with AI Upscale runs one worker: it also holds the 2× image (4× the
//! render's pixels), which is the working copy at 200 % or is resized from
//! (`tests/memory_upscale.rs`).
//!
//! All recipes that share an output space are produced from a single render.
//! Injected callbacks (renderer, HEIC encoder, progress) run under
//! `catch_panic`: a panic fails that item, never the batch. Every output is
//! fsync'd; the device caches of all destinations are flushed
//! (`F_FULLFSYNC`) once at the end, before the report is returned.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ds_core::Size;
use ds_template::Vars;

use crate::encode::{HeicEncoder, encode_pixels_to};
use crate::exif::ExifMeta;
use crate::image::{Image16, Pixels, Rendered};
use crate::output::{Namer, write_atomic_sized};
use crate::post::{self, PostProcess, WatermarkStep};
use crate::process::{resize, sharpen_in_place};
use crate::recipe::{ExportFormat, ExportRecipe, OutputSpace};
use crate::watermark::TextRasterizer;
use crate::{ExportError, icc};

/// One image to export.
#[derive(Debug, Clone, Default)]
pub struct ExportItem {
    /// Caller's identifier (e.g. variant id), echoed in the report.
    pub id: u64,
    /// Original file stem for `{name}`.
    pub name: String,
    /// Running number for `{seq}`.
    pub seq: u64,
    pub meta: ExifMeta,
}

/// Renders an item at full resolution in the requested output space.
/// Implemented by the integration layer on top of `ds-pipeline`.
pub trait Renderer: Sync {
    fn render(&self, item: &ExportItem, space: OutputSpace) -> Result<Rendered, String>;

    /// Render `item` already sized for `recipe`: at `recipe.target_size(full develop
    /// size)`, in `recipe.space`, with the recipe's output sharpening applied (the
    /// develop pipeline resizes and sharpens on the GPU). `None` when this renderer can't
    /// (the default): the batch then renders full size once per output space and
    /// resizes and sharpens on the CPU.
    fn render_for(
        &self,
        _item: &ExportItem,
        _recipe: &ExportRecipe,
    ) -> Option<Result<Rendered, String>> {
        None
    }
}

impl<F> Renderer for F
where
    F: Fn(&ExportItem, OutputSpace) -> Result<Rendered, String> + Sync,
{
    fn render(&self, item: &ExportItem, space: OutputSpace) -> Result<Rendered, String> {
        self(item, space)
    }
}

/// Enlarges a rendered image 2× with a learned model (AI Upscale, run by the app).
/// Implemented by the integration layer.
pub trait Upscaler: Sync {
    /// `img` at twice its size, in the same encoding. `Ok(None)`: no model in this
    /// build (the recipe resamples instead).
    fn upscale(&self, img: &Image16) -> Result<Option<Image16>, String>;
}

#[derive(Clone, Copy)]
pub struct BatchOptions<'a> {
    /// Max full-resolution renders held at once (= worker count). Default 2.
    pub max_in_flight: usize,
    pub heic: Option<&'a dyn HeicEncoder>,
    /// Hardware JPEG (`ImageIO` on the Swift side) used instead of mozjpeg when
    /// set. Same seam as HEIC: 16-bit RGB in, file bytes out, ICC and EXIF
    /// embedded by the encoder.
    pub jpeg: Option<&'a dyn HeicEncoder>,
    /// AI Upscale for recipes that ask for it ([`ExportRecipe::ai_upscale`]).
    pub upscaler: Option<&'a dyn Upscaler>,
    /// Rasterises text watermarks (Core Text on the Swift side); without it a
    /// recipe with a text watermark fails.
    pub text: Option<&'a dyn TextRasterizer>,
    /// The batch's Cancel, for long post-process steps (`run_batch` sets it).
    pub cancel: Option<&'a AtomicBool>,
}

impl<'a> BatchOptions<'a> {
    /// These options with the batch's Cancel.
    #[must_use]
    pub fn with_cancel(self, cancel: &'a AtomicBool) -> Self {
        Self {
            cancel: Some(cancel),
            ..self
        }
    }

    /// Whether the batch was cancelled.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
    }
}

impl std::fmt::Debug for BatchOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchOptions")
            .field("max_in_flight", &self.max_in_flight)
            .field("heic", &self.heic.is_some())
            .field("jpeg", &self.jpeg.is_some())
            .field("upscaler", &self.upscaler.is_some())
            .field("text", &self.text.is_some())
            .field("cancel", &self.cancel.is_some())
            .finish()
    }
}

impl Default for BatchOptions<'_> {
    fn default() -> Self {
        Self {
            max_in_flight: 2,
            heic: None,
            jpeg: None,
            upscaler: None,
            text: None,
            cancel: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportProgress {
    pub images_done: usize,
    pub images_total: usize,
    pub outputs_written: usize,
    pub failures: usize,
    /// Item just finished.
    pub last: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedFile {
    pub item: u64,
    pub recipe: usize,
    pub path: PathBuf,
    pub size: Size,
    pub bytes: u64,
    /// The recipe's text watermark was left out: its text was blank for this photo
    /// (e.g. `{copyright}` and no copyright).
    pub watermark_skipped: bool,
    /// Metadata left out of this file to make it fit the format (e.g. keywords past a
    /// JPEG segment's 64 KB); empty when everything was written.
    pub notes: Vec<String>,
}

/// One file just written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub path: PathBuf,
    pub size: Size,
    pub bytes: u64,
    /// See [`ExportedFile::notes`].
    pub notes: Vec<String>,
}

#[derive(Debug)]
pub struct ExportFailure {
    /// `None` for a recipe that failed validation (reported once).
    pub item: Option<u64>,
    /// `None` when the render itself failed (all recipes of that space).
    pub recipe: Option<usize>,
    pub error: ExportError,
}

#[derive(Debug, Default)]
pub struct BatchReport {
    pub written: Vec<ExportedFile>,
    pub failed: Vec<ExportFailure>,
    pub cancelled: bool,
    /// A destination filled up: the batch stopped there ([`ExportError::is_disk_full`]);
    /// items after it were not attempted.
    pub disk_full: bool,
}

/// Export `items` × `recipes`. Bad recipes, render errors (including a
/// panicking renderer or encoder), bad buffers and I/O errors end up in
/// [`BatchReport::failed`]; a failed final device flush is reported there too
/// (with `item` and `recipe` = `None`).
/// Cancellation stops between outputs; files
/// already written stay, and no temp files remain.
pub fn run_batch(
    items: &[ExportItem],
    recipes: &[ExportRecipe],
    renderer: &dyn Renderer,
    opts: BatchOptions<'_>,
    progress: &(dyn Fn(&ExportProgress) + Sync),
    cancel: &AtomicBool,
) -> BatchReport {
    let opts = opts.with_cancel(cancel);
    let mut report = BatchReport::default();
    let mut valid = Vec::new();
    for (i, r) in recipes.iter().enumerate() {
        match r.validate() {
            Ok(()) => valid.push(i),
            Err(error) => report.failed.push(ExportFailure {
                item: None,
                recipe: Some(i),
                error,
            }),
        }
    }
    // Group recipes by output space: one render per space.
    let mut spaces: Vec<(OutputSpace, Vec<usize>)> = Vec::new();
    for &i in &valid {
        match spaces.iter_mut().find(|(s, _)| *s == recipes[i].space) {
            Some((_, v)) => v.push(i),
            None => spaces.push((recipes[i].space, vec![i])),
        }
    }

    let namer = Namer::default();
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let shared = Mutex::new(report);
    let full = AtomicBool::new(false);
    // AI Upscale holds a 2× image (4× the render's pixels) on top of the render: one
    // worker, so a 60 MP export stays within a few GB.
    let upscaling = opts.upscaler.is_some() && valid.iter().any(|&i| recipes[i].ai_upscale);
    let max = if upscaling { 1 } else { opts.max_in_flight };
    let workers = max.clamp(1, items.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(idx) else { break };
                    if cancel.load(Ordering::Relaxed) || full.load(Ordering::Relaxed) {
                        break;
                    }
                    let mut written = Vec::new();
                    let mut failed = Vec::new();
                    for (space, rs) in &spaces {
                        export_item(
                            item,
                            *space,
                            rs,
                            recipes,
                            renderer,
                            opts,
                            &namer,
                            cancel,
                            &mut written,
                            &mut failed,
                        );
                    }
                    if failed
                        .iter()
                        .any(|f: &ExportFailure| f.error.is_disk_full())
                    {
                        full.store(true, Ordering::Relaxed);
                    }
                    let images_done = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let snapshot = {
                        let mut rep = shared
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        rep.written.extend(written);
                        rep.failed.extend(failed);
                        ExportProgress {
                            images_done,
                            images_total: items.len(),
                            outputs_written: rep.written.len(),
                            failures: rep.failed.len(),
                            last: Some(item.id),
                        }
                    };
                    // A panicking progress callback must not kill the worker.
                    let _ = ds_fsutil::catch_panic(|| progress(&snapshot));
                }
            });
        }
    });
    let mut report = shared
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    report.cancelled = cancel.load(Ordering::Relaxed);
    report.disk_full = full.load(Ordering::Relaxed);
    // One device-wide flush per destination (per-file fsync already done).
    let dests: BTreeSet<PathBuf> = report
        .written
        .iter()
        .filter_map(|w| w.path.parent().map(PathBuf::from))
        .collect();
    for d in dests {
        if let Err(source) = ds_fsutil::flush_device(&d) {
            report.failed.push(ExportFailure {
                item: None,
                recipe: None,
                error: ExportError::Io { path: d, source },
            });
        }
    }
    report
}

#[allow(clippy::too_many_arguments)]
fn export_item(
    item: &ExportItem,
    space: OutputSpace,
    recipe_ids: &[usize],
    recipes: &[ExportRecipe],
    renderer: &dyn Renderer,
    opts: BatchOptions<'_>,
    namer: &Namer,
    cancel: &AtomicBool,
    written: &mut Vec<ExportedFile>,
    failed: &mut Vec<ExportFailure>,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    // The full-size render, made at most once and only for recipes the renderer
    // can't size itself.
    let mut full: Option<Image16> = None;
    for &ri in recipe_ids {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let recipe = &recipes[ri];
        let sized = ds_fsutil::catch_panic(|| renderer.render_for(item, recipe))
            .unwrap_or_else(|p| Some(Err(format!("renderer panicked: {p}"))));
        // Cancelled while rendering: nothing more is written.
        if sized.is_some() && cancel.load(Ordering::Relaxed) {
            return;
        }
        let result = if let Some(r) = sized {
            r.map_err(ExportError::Render)
                .and_then(|r| export_rendered(item, r, recipe, opts, namer))
        } else {
            if full.is_none() {
                let f = ds_fsutil::catch_panic(|| renderer.render(item, space))
                    .map_err(|p| format!("renderer panicked: {p}"))
                    .and_then(|r| r)
                    .map_err(ExportError::Render)
                    .and_then(Image16::from_rendered);
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                match f {
                    Ok(f) => full = Some(f),
                    Err(error) => {
                        failed.push(ExportFailure {
                            item: Some(item.id),
                            recipe: None,
                            error,
                        });
                        return;
                    }
                }
            }
            match &full {
                Some(f) => export_one_with(item, f, recipe, opts, namer),
                None => continue,
            }
        };
        match result {
            Ok(w) => written.push(ExportedFile {
                item: item.id,
                recipe: ri,
                path: w.path,
                size: w.size,
                bytes: w.bytes,
                watermark_skipped: recipe.watermark.as_ref().is_some_and(|w| w.skips(item)),
                notes: w.notes,
            }),
            Err(error) => failed.push(ExportFailure {
                item: Some(item.id),
                recipe: Some(ri),
                error,
            }),
        }
    }
}

/// Produce one output file from an already rendered full-size image.
pub fn export_one(
    item: &ExportItem,
    full: &Image16,
    recipe: &ExportRecipe,
    heic: Option<&dyn HeicEncoder>,
    namer: &Namer,
) -> Result<Written, ExportError> {
    let opts = BatchOptions {
        heic,
        ..BatchOptions::default()
    };
    export_one_with(item, full, recipe, opts, namer)
}

/// [`export_one`] with the encoder hooks of `opts` (HEIC, hardware JPEG).
pub fn export_one_with(
    item: &ExportItem,
    full: &Image16,
    recipe: &ExportRecipe,
    opts: BatchOptions<'_>,
    namer: &Namer,
) -> Result<Written, ExportError> {
    recipe.validate()?;
    let target = recipe.target_size(full.size)?;
    // AI Upscale first when the output is larger than the render: the model's 2× image
    // is then resampled to the target (an extra image of 4× the render's pixels).
    let enlarged = match opts.upscaler {
        Some(u) if recipe.ai_upscale && target.pixels() > full.size.pixels() => {
            ds_fsutil::catch_panic(|| u.upscale(full))
                .map_err(|p| format!("upscaler panicked: {p}"))
                .and_then(|r| r)
                .map_err(ExportError::Render)?
                .map(|big| checked_upscale(full, big))
                .transpose()?
        }
        _ => None,
    };
    // At most one output-sized working copy: the resize output, or a copy of
    // the full render when sharpening at original size; sharpen is in place. The 2×
    // image is the working copy itself when it is already the target.
    let mut img: Cow<'_, Image16> = match enlarged {
        Some(big) if big.size == target => Cow::Owned(big),
        Some(big) => Cow::Owned(resize(&big, target)),
        None if target == full.size => Cow::Borrowed(full),
        None => Cow::Owned(resize(full, target)),
    };
    if recipe.sharpen > 0.0 {
        sharpen_in_place(img.to_mut(), recipe.sharpen);
    }
    watermark_and_write(item, img, recipe, opts, namer)
}

/// The model's result, checked before anything resamples from it: an app-side model
/// that returns the wrong size or a short buffer must fail this output, not panic the
/// resize and take the batch down with it.
fn checked_upscale(full: &Image16, big: Image16) -> Result<Image16, ExportError> {
    let want = Size::new(
        full.size.width.saturating_mul(2),
        full.size.height.saturating_mul(2),
    );
    if big.size != want {
        return Err(ExportError::Render(format!(
            "upscaler returned {}×{}, expected {}×{}",
            big.size.width, big.size.height, want.width, want.height
        )));
    }
    Image16::new(big.size, big.data)
        .map_err(|e| ExportError::Render(format!("upscaler result: {e}")))
}

/// Whether `recipe` encodes an 8-bit render as is (JPEG through mozjpeg, PNG8).
/// A recipe with an output profile converts first, so it always renders 16-bit
/// (one quantization, after the conversion).
pub fn encodes_8_bit(recipe: &ExportRecipe, opts: BatchOptions<'_>) -> bool {
    if recipe.output_profile.is_some() {
        return false;
    }
    match recipe.format {
        ExportFormat::Jpeg { .. } => opts.jpeg.is_none(),
        ExportFormat::Png8 => true,
        _ => false,
    }
}

/// Write a render already sized for `recipe`: an 8-bit render straight to an 8-bit
/// encoder, anything else through a 16-bit image.
fn export_rendered(
    item: &ExportItem,
    r: Rendered,
    recipe: &ExportRecipe,
    opts: BatchOptions<'_>,
    namer: &Namer,
) -> Result<Written, ExportError> {
    match r {
        Rendered::Rgba8 { size, mut data } if encodes_8_bit(recipe, opts) => {
            Pixels::rgba8(size, &data)?;
            let mut size = size;
            post::run_rgba8(
                &post_process_steps(recipe, opts),
                &mut size,
                &mut data,
                recipe.space,
                item,
                &|| opts.cancelled(),
            )?;
            let px = Pixels::rgba8(size, &data)?;
            write_output(item, px, icc::profile(recipe.space), recipe, opts, namer)
        }
        r => watermark_and_write(
            item,
            Cow::Owned(Image16::from_rendered(r)?),
            recipe,
            opts,
            namer,
        ),
    }
}

/// Write `img` as `recipe`'s output: it is already at the recipe's size and
/// sharpened (see [`Renderer::render_for`]); the recipe's watermark is drawn on a copy.
pub fn export_prepared(
    item: &ExportItem,
    img: &Image16,
    recipe: &ExportRecipe,
    opts: BatchOptions<'_>,
    namer: &Namer,
) -> Result<Written, ExportError> {
    recipe.validate()?;
    watermark_and_write(item, Cow::Borrowed(img), recipe, opts, namer)
}

/// The steps drawn on each output at its final size, in order: the recipe's plug-in
/// post-processors, then its watermark (the native provider of the seam).
fn post_process_steps<'a>(
    recipe: &'a ExportRecipe,
    opts: BatchOptions<'a>,
) -> Vec<Box<dyn PostProcess + 'a>> {
    let mut steps: Vec<Box<dyn PostProcess + 'a>> = Vec::new();
    for p in &recipe.post_process {
        steps.push(Box::new(Arc::clone(p)));
    }
    // The watermark last, so a border a plug-in adds never covers it.
    if let Some(watermark) = &recipe.watermark {
        steps.push(Box::new(WatermarkStep {
            watermark,
            text: opts.text,
        }));
    }
    steps
}

/// Draw the recipe's post-process steps (last, at output size; on the working copy,
/// never on a shared full render), convert to the recipe's output profile, then write.
fn watermark_and_write(
    item: &ExportItem,
    mut img: Cow<'_, Image16>,
    recipe: &ExportRecipe,
    opts: BatchOptions<'_>,
    namer: &Namer,
) -> Result<Written, ExportError> {
    post::run_rgb16(
        &post_process_steps(recipe, opts),
        &mut img,
        recipe.space,
        item,
        &|| opts.cancelled(),
    )?;
    let icc = match &recipe.output_profile {
        None => icc::profile(recipe.space),
        Some(out) => {
            // In place on the working copy; only a borrowed image (a shared render)
            // is copied, once, by `to_mut`.
            let img = img.to_mut();
            out.convert_rgb16(recipe.space, &mut img.data, img.size.width as usize)?;
            out.profile.bytes()
        }
    };
    write_output(item, Pixels::Rgb16(&img), icc, recipe, opts, namer)
}

fn write_output(
    item: &ExportItem,
    img: Pixels<'_>,
    icc: &[u8],
    recipe: &ExportRecipe,
    opts: BatchOptions<'_>,
    namer: &Namer,
) -> Result<Written, ExportError> {
    let size = img.size();
    let dirs = recipe.exif.dirs(&item.meta);
    let vars = Vars {
        name: &item.name,
        seq: item.seq,
        datetime: item.meta.capture_time,
        camera: item.meta.model.as_deref(),
        ext: recipe.format.extension(),
    };
    let file_name = recipe.file_name.render_file_name(&vars);
    let mut notes = Vec::new();
    let (path, bytes) = write_atomic_sized(namer, &recipe.destination, &file_name, |w| {
        // A hardware JPEG encoder takes the HEIC path (an injected encoder).
        let (format, hook) = match (recipe.format, opts.jpeg) {
            (ExportFormat::Jpeg { quality }, Some(hw)) => {
                (ExportFormat::Heic { quality }, Some(hw))
            }
            (f, _) => (f, opts.heic),
        };
        notes = encode_pixels_to(w, img, format, icc, dirs.as_ref(), hook)?;
        Ok(())
    })?;
    Ok(Written {
        path,
        size,
        bytes,
        notes,
    })
}
