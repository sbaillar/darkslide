//! AI Upscale through the FFI: the app runs the model (Core ML), Rust does everything
//! around it (see `lt_pipeline::upscale`).
//!
//! - [`begin_upscale`] decodes the RAW (cached), demosaics it at full resolution and
//!   starts a linear DNG next to it (`<name>-Upscaled.dng`, or a free `-N` name): the app
//!   reads each tile into its model's input ([`UpscaleTask::read_tile`]), runs the model
//!   and hands the output back ([`UpscaleTask::write_tile`]), from as many threads as it
//!   has model instances. Finished rows stream into the DNG as tile rows complete, so
//!   memory stays bounded. [`UpscaleTask::finish`] completes the file, places it and
//!   returns its path; a task dropped unfinished (cancelled) leaves no file behind.
//! - [`SessionHandle::add_derived`] adds that file to the session as a new photo with the
//!   source photo's adjustments.
//! - Export: a recipe with AI Upscale asks the app's [`UpscaleProvider`] to run the model
//!   over the rendered image (an [`UpscaleTask`] whose rows stay in memory), then resizes
//!   that to the recipe's size. With no provider or no model the render is resampled.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use lt_core::{RawImage, Size};
use lt_export::{DngInfo, ExportError, Image16, LinearDngWriter, Namer};
use lt_pipeline::Priority;
use lt_pipeline::upscale::{MAX_OUTPUT_PIXELS, SCALE, TILE, UpscaleJob};
use lt_session::{NewOriginal, VariantId, identity};

use crate::denoise::elements;
use crate::develop::{develop, pipeline_error};
use crate::error::DsError;
use crate::jobs::{CancelToken, guard, run_job, run_job_on_thread};
use crate::session::{SessionHandle, read_meta, sidecar_volume_ok};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn export_error(e: ExportError) -> DsError {
    match e {
        e @ ExportError::Io { .. } => DsError::Io {
            message: e.to_string(),
        },
        e => DsError::Unsupported {
            message: e.to_string(),
        },
    }
}

/// The app's AI Upscale, for export. Called from export workers, never the main thread.
#[uniffi::export(with_foreign)]
pub trait UpscaleProvider: Send + Sync {
    /// Run the app's model over every tile of `task` now (blocking). `Ok(false)`: this
    /// build has no AI Upscale model, so the export resamples instead. An error fails
    /// that photo's export, not the export; `cancel` is cancelled when the export is.
    fn upscale(&self, task: Arc<UpscaleTask>, cancel: Arc<CancelToken>) -> Result<bool, DsError>;
}

fn provider_slot() -> &'static Mutex<Option<Arc<dyn UpscaleProvider>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<dyn UpscaleProvider>>>> = OnceLock::new();
    SLOT.get_or_init(Mutex::default)
}

/// The app's AI Upscale for exports (`None`: exports resample).
#[uniffi::export]
pub fn set_upscale_provider(provider: Option<Arc<dyn UpscaleProvider>>) {
    *lock(provider_slot()) = provider;
}

/// The provider exports use.
pub(crate) fn provider() -> Option<Arc<dyn UpscaleProvider>> {
    lock(provider_slot()).clone()
}

/// Where the stitched rows go.
enum Sink {
    /// A new photo.
    Dng(Box<LinearDngWriter>),
    /// An export's image (interleaved RGB, rows so far).
    Image(Vec<u16>),
    /// Finished (or failed while finishing).
    Done,
}

/// One image being upscaled 2× (see the module docs). Tiles are planar RGB `f16`
/// (`[3][tile_size][tile_size]` in, `[3][2·tile_size][2·tile_size]` out, sRGB-encoded,
/// 0..1).
#[derive(uniffi::Object)]
pub struct UpscaleTask {
    job: Arc<UpscaleJob>,
    sink: Mutex<Sink>,
}

impl std::fmt::Debug for UpscaleTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpscaleTask")
            .field("size", &self.job.size())
            .finish_non_exhaustive()
    }
}

/// Start upscaling the RAW at `path` 2×: decodes it (cached), demosaics it and opens its
/// DNG next to it, named `name` (from [`SessionHandle::free_derived_name`]; a file
/// taken meanwhile gets the next free `-N`). Fails when the file can't be developed or
/// the result would be larger than 256 MP.
#[uniffi::export]
pub async fn begin_upscale(
    path: String,
    name: String,
    token: Arc<CancelToken>,
) -> Result<Arc<UpscaleTask>, DsError> {
    run_job_on_thread("lt-upscale", token, move |cancelled| {
        let path = PathBuf::from(&path);
        let loaded = develop().load(&path)?;
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        UpscaleTask::for_raw(&path, &name, &loaded.raw).map(Arc::new)
    })
    .await
}

/// The DNG of `raw` (the file at `path`) upscaled to `out`: the RAW's colour and
/// orientation, its default crop scaled, its metadata and its embedded preview.
fn dng_info(path: &Path, raw: &RawImage, out: Size) -> DngInfo {
    DngInfo {
        size: out,
        cam_to_xyz: raw.cam_to_xyz,
        wb_as_shot: raw.wb_as_shot,
        wb_daylight: raw.wb_daylight,
        orientation: raw.orientation.to_exif(),
        default_crop: raw.default_crop.map(|c| c.map(|v| v * SCALE)),
        exif: crate::export::meta_of(path),
        preview: lt_raw::embedded_preview(path).ok().map(|p| p.jpeg),
    }
}

impl UpscaleTask {
    /// The new photo goes next to the original, only where the session writes too: a
    /// local, writable APFS or HFS+ volume (never a memory card).
    pub(crate) fn for_raw(path: &Path, name: &str, raw: &RawImage) -> Result<Self, DsError> {
        let dir = path
            .parent()
            .ok_or_else(|| DsError::invalid(format!("{} has no folder", path.display())))?;
        sidecar_volume_ok(dir).map_err(|why| DsError::Unsupported {
            message: format!("the upscaled photo can't be written next to the original: {why}"),
        })?;
        let job = UpscaleJob::from_raw(raw, Priority::Background).map_err(pipeline_error)?;
        let info = dng_info(path, raw, job.output_size());
        let writer = LinearDngWriter::create(dir, name, info).map_err(export_error)?;
        Ok(Self {
            job: Arc::new(job),
            sink: Mutex::new(Sink::Dng(Box::new(writer))),
        })
    }

    /// An export's rendered image, its rows kept in memory.
    pub(crate) fn for_image(img: &Image16) -> Result<Self, DsError> {
        let job = UpscaleJob::from_encoded(img.size, &img.data, Priority::Background)
            .map_err(pipeline_error)?;
        let out = job.output_size();
        Ok(Self {
            job: Arc::new(job),
            sink: Mutex::new(Sink::Image(Vec::with_capacity(out.pixels() as usize * 3))),
        })
    }

    /// The upscaled image of [`Self::for_image`], once every tile was written.
    pub(crate) fn take_image(&self) -> Option<Image16> {
        let mut sink = lock(&self.sink);
        if self.job.missing() > 0 || self.job.rows_left() > 0 {
            return None;
        }
        match std::mem::replace(&mut *sink, Sink::Done) {
            Sink::Image(data) => Image16::new(self.job.output_size(), data).ok(),
            other => {
                *sink = other;
                None
            }
        }
    }

    /// Hand the rows every written tile completed to the sink.
    fn drain(&self, sink: &mut Sink) -> Result<(), DsError> {
        while let Some(rows) = self.job.take_rows() {
            match sink {
                Sink::Dng(w) => w.write_rows(&rows.data).map_err(export_error)?,
                Sink::Image(v) => v.extend_from_slice(&rows.data),
                Sink::Done => return Err(DsError::invalid("upscale task already finished")),
            }
        }
        Ok(())
    }
}

#[uniffi::export]
impl UpscaleTask {
    /// Number of tiles (indices `0..tile_count`).
    pub fn tile_count(&self) -> u32 {
        self.job.tile_count() as u32
    }

    /// Edge of an input tile in pixels (the output tile is `output_scale` times larger).
    pub fn tile_size(&self) -> u32 {
        TILE
    }

    /// Output pixels per input pixel along each axis.
    pub fn output_scale(&self) -> u32 {
        SCALE
    }

    /// Write tile `index`'s model input into caller memory: `dest_len` ≥ 3 × `tile_size`²
    /// `f16` elements at `dest_ptr`, valid and untouched by the caller during the call
    /// (a model's input buffer). Call it from the model's worker thread.
    pub fn read_tile(&self, index: u32, dest_ptr: u64, dest_len: u64) -> Result<(), DsError> {
        guard(|| {
            let (addr, len) = elements(dest_ptr, dest_len)?;
            // SAFETY: non-null, aligned for u16 and len·2 ≤ isize::MAX were checked in
            // `elements`; the caller guarantees the memory is valid, writable and not
            // accessed elsewhere for the duration of this call.
            #[allow(unsafe_code)]
            let out = unsafe { std::slice::from_raw_parts_mut(addr as *mut u16, len) };
            self.job
                .read_tile(index as usize, out)
                .map_err(pipeline_error)
        })
    }

    /// Take tile `index`'s model output from caller memory: `src_len` `f16` elements at
    /// `src_ptr`, element `(c, y, x)` at `c·channel_stride + y·row_stride + x`. Each tile
    /// once. The rows it completes are written out before this returns.
    pub fn write_tile(
        &self,
        index: u32,
        src_ptr: u64,
        src_len: u64,
        row_stride: u64,
        channel_stride: u64,
    ) -> Result<(), DsError> {
        guard(|| {
            let (addr, len) = elements(src_ptr, src_len)?;
            let stride = |s: u64| usize::try_from(s).map_err(|_| DsError::invalid("stride"));
            // SAFETY: as in `read_tile`; the memory is only read.
            #[allow(unsafe_code)]
            let data = unsafe { std::slice::from_raw_parts(addr as *const u16, len) };
            let mut sink = lock(&self.sink);
            if matches!(*sink, Sink::Done) {
                return Err(DsError::invalid("upscale task already finished"));
            }
            self.job
                .write_tile(
                    index as usize,
                    data,
                    stride(row_stride)?,
                    stride(channel_stride)?,
                )
                .map_err(pipeline_error)?;
            self.drain(&mut sink)
        })
    }

    /// Complete the photo's DNG and place it; returns its path. Every tile must have
    /// been written; the task can't be used afterwards (a failure leaves no file).
    pub async fn finish(self: Arc<Self>, token: Arc<CancelToken>) -> Result<String, DsError> {
        run_job_on_thread("lt-upscale-finish", token, move |cancelled| {
            self.finish_with(cancelled)
        })
        .await
    }
}

impl UpscaleTask {
    /// [`UpscaleTask::finish`] on this thread. Cancelled before the file is placed,
    /// nothing is left (the writer's temp goes with it); once placed, the path is
    /// returned whatever happens next, so the app adds the photo instead of leaving the
    /// file behind.
    pub(crate) fn finish_with(&self, cancelled: &dyn Fn() -> bool) -> Result<String, DsError> {
        let mut sink = std::mem::replace(&mut *lock(&self.sink), Sink::Done);
        let missing = self.job.missing();
        if missing > 0 {
            return Err(DsError::invalid(format!(
                "{missing} of {} tiles not upscaled",
                self.job.tile_count()
            )));
        }
        self.drain(&mut sink)?;
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        match sink {
            Sink::Dng(writer) => {
                let path = (*writer).finish(&Namer::default()).map_err(export_error)?;
                Ok(path.to_string_lossy().into_owned())
            }
            _ => Err(DsError::invalid("not a photo upscale, or already finished")),
        }
    }
}

/// AI Upscale for the export batch: the app's provider over an [`UpscaleTask`].
pub(crate) struct ExportUpscaler {
    provider: Option<Arc<dyn UpscaleProvider>>,
    cancel: Arc<CancelToken>,
}

impl ExportUpscaler {
    pub(crate) fn new(provider: Option<Arc<dyn UpscaleProvider>>) -> Self {
        Self {
            provider,
            cancel: CancelToken::new(),
        }
    }

    /// Stop the upscale an export waits for (the export was cancelled).
    pub(crate) fn cancel(&self) {
        self.cancel.cancel();
    }

    #[cfg(test)]
    pub(crate) fn cancel_token(&self) -> &CancelToken {
        &self.cancel
    }
}

impl lt_export::Upscaler for ExportUpscaler {
    fn upscale(&self, img: &Image16) -> Result<Option<Image16>, String> {
        // A 2× image past the upscaler's limit: resampled, as without AI Upscale.
        let Some(provider) = self
            .provider
            .as_ref()
            .filter(|_| img.size.pixels() * u64::from(SCALE * SCALE) <= MAX_OUTPUT_PIXELS)
        else {
            return Ok(None);
        };
        let task = Arc::new(UpscaleTask::for_image(img).map_err(|e| e.to_string())?);
        match provider.upscale(task.clone(), self.cancel.clone()) {
            Ok(false) => Ok(None),
            Ok(true) => task
                .take_image()
                .map(Some)
                .ok_or_else(|| "AI Upscale didn't run over every tile".into()),
            Err(DsError::Cancelled) => Err("cancelled".into()),
            Err(e) => Err(format!("AI Upscale: {e}")),
        }
    }
}

impl SessionHandle {
    /// `path` (canonical) as a path of this session: `Capture/<rest>` in a folder opened
    /// in place, relative to the session folder otherwise.
    fn session_rel(&self, path: &Path) -> Result<String, DsError> {
        let (base, prefix) = match self.in_place.as_ref() {
            Some(folder) => (folder.canonicalize()?, "Capture/"),
            None => (lock(&self.session).root().canonicalize()?, ""),
        };
        let rest = path.strip_prefix(&base).map_err(|_| {
            DsError::invalid(format!(
                "{} is not inside {}",
                path.display(),
                base.display()
            ))
        })?;
        Ok(format!("{prefix}{}", rest.to_string_lossy()))
    }
}

#[uniffi::export]
impl SessionHandle {
    /// A file name for a photo made from the one at `source_path` (AI Upscale: `suffix`
    /// "Upscaled"): `<stem>-<suffix>.dng`, or the first free `-N`, free both on disk and in
    /// this session (a photo whose file is missing, or a sidecar left behind, keeps its
    /// name), so the new file can always be added.
    pub async fn free_derived_name(
        self: Arc<Self>,
        source_path: String,
        suffix: String,
        token: Arc<CancelToken>,
    ) -> Result<String, DsError> {
        run_job(token, move |_| {
            let src = Path::new(&source_path).canonicalize()?;
            let dir = src
                .parent()
                .ok_or_else(|| DsError::invalid(format!("{} has no folder", src.display())))?;
            let stem = src.file_stem().unwrap_or_default().to_string_lossy();
            let desired = format!("{stem}-{suffix}.dng");
            for n in 0..10_000 {
                let name = if n == 0 {
                    desired.clone()
                } else {
                    lt_template::with_suffix(&desired, n)
                };
                let path = dir.join(&name);
                if path.symlink_metadata().is_ok() {
                    continue;
                }
                let rel = self.session_rel(&path)?;
                if !lock(&self.session).path_taken(&rel)? {
                    return Ok(name);
                }
            }
            Err(DsError::invalid(format!("no free name for {desired}")))
        })
        .await
    }

    /// Add `path`, a file made from photo `source_variant_id` (AI Upscale's DNG) inside
    /// this session's folder, as a new photo starting with the source's adjustments.
    /// Returns the new photo's variant id.
    pub async fn add_derived(
        self: Arc<Self>,
        path: String,
        source_variant_id: i64,
        token: Arc<CancelToken>,
    ) -> Result<i64, DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            let path = Path::new(&path).canonicalize()?;
            let rel = self.session_rel(&path)?;
            let (meta, _) = read_meta(&path).ok_or_else(|| DsError::Unsupported {
                message: format!("{} can't be read", path.display()),
            })?;
            let ranges = lt_raw::image_data_ranges(&path).ok().flatten();
            let hashes = identity::hash_original(&path, ranges.as_deref(), &|| false)?;
            let mut s = lock(&self.session);
            let adjustments = s.load_adjustments(VariantId(source_variant_id))?;
            let item = NewOriginal {
                rel,
                hash: hashes.file,
                content_hash: hashes.content,
                meta,
                replaced: Vec::new(),
                // Title, caption, credits and rights travel with the photo.
                iptc: s.variant(VariantId(source_variant_id))?.iptc,
            };
            Ok(s.add_derived(item, &adjustments)?.0)
        })
        .await
    }
}

#[cfg(test)]
#[path = "upscale_tests.rs"]
pub(crate) mod tests;
