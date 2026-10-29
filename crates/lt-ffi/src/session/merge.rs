//! Merge to HDR, Merge to Panorama and Merge to Best Shot (see `docs/FFI.md`).
//!
//! The sources are decoded and demosaiced to linear camera RGB (RCD, the export's
//! demosaic, without white balance), merged by `lt-merge`, and written as a **linear
//! DNG** (16-bit `LinearRaw`, lossless JPEG, the reference frame's colour matrix,
//! as-shot neutral and EXIF) next to the first source. The new file is registered as a
//! new photo whose first variant starts at the exposure that matches the reference frame
//! (a panorama also starts with lens corrections off: its geometry is already
//! projected). The sources are only read.
//!
//! Best Shot (a burst composite, [`lt_merge::merge_burst`]) builds on the burst's best
//! frame and takes better faces (open eyes) and sharper regions from the others. Faces
//! come from the app (Vision) per source, normalized to the upright photo, so its frames
//! are turned upright first and the result is written upright, like a panorama. At most
//! [`BURST_MAX_FRAMES`] frames are decoded (the best scored, when there are more).
//!
//! Previews merge 2×2-binned frames ([`lt_merge::quick_frame`]) further box-downsampled
//! to the preview size (panorama frames stay at the merge's 1000 px analysis size, so a
//! preview registers like the merge), cached per file version so changing an option
//! only merges and develops again.
//!
//! Memory: sources are decoded one at a time, only the merge holds every frame, the
//! frames are dropped before the DNG is written, its preview develops from a downsampled
//! copy, and the DNG streams to its temp file (hashed from disk).

use std::collections::VecDeque;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use lt_core::{Adjustments, CfaPattern, FileHash, IptcMeta, Orientation, RawImage, Size};
use lt_merge::{Frame, MergeError, Merged};
use lt_pipeline::{ImageKey, OutputFormat, Priority, RenderRequest};
use lt_session::{ImageId, NewOriginal, VariantId};

use super::{CancelToken, DsError, SessionHandle, lock};
use crate::develop::{Loaded, develop};
use crate::events::{Dispatcher, ProgressEvent, ProgressListener, TaskPhase};
use crate::jobs::{run_job, run_job_on_thread};

/// Progress events report permille.
const PERMILLE: u64 = 1000;
/// Long edge of the preview embedded in the written DNG.
const EMBEDDED_PREVIEW_EDGE: u32 = 1616;
/// The exposure slider's range.
const MAX_EV: f32 = 5.0;
/// Long edge of panorama preview frames (the merge's analysis size).
const PANO_PREVIEW_FRAME: u32 = 1000;
/// Decoded preview sources kept for the next preview.
const PREVIEW_CACHE: usize = 24;
/// Focal lengths further apart than this (ratio) are different zoom settings.
const FOCAL_TOLERANCE: f32 = 1.03;
/// Most frames a Best Shot merge decodes (each 24 MP frame is 144 MB in memory).
pub const BURST_MAX_FRAMES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MergeKind {
    Hdr,
    Panorama,
    /// Best Shot: the best parts of a burst in one picture.
    Burst,
}

/// How aggressively moving objects are taken from a single exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Deghost {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PanoProjection {
    Auto,
    Spherical,
    Cylindrical,
    Perspective,
}

/// What to merge and how. HDR reads `align` and `deghost`; panoramas `projection` and
/// `auto_crop`; Best Shot none of them (it always aligns, takes better faces and sharper
/// regions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct MergeOptions {
    pub kind: MergeKind,
    pub align: bool,
    pub deghost: Deghost,
    pub projection: PanoProjection,
    pub auto_crop: bool,
}

/// A preview of a merge.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct MergePreview {
    /// sRGB JPEG of the result developed with its initial edit.
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The projection used (`Auto` resolved; `Perspective` for HDR).
    pub projection: PanoProjection,
    /// HDR: regions where one exposure was used (0 for panoramas).
    pub ghost_regions: u32,
    /// The initial exposure the new photo gets.
    pub exposure_ev: f32,
    /// HDR: frames that could not be aligned (merged as they are). Best Shot: frames that
    /// could not be aligned (nothing was taken from them).
    pub unaligned_frames: u32,
    /// Best Shot: faces taken from another frame (usually open eyes).
    pub faces_replaced: u32,
    /// Best Shot: regions taken from a sharper frame.
    pub regions_replaced: u32,
    /// Best Shot: better faces left as they were (the head moved too much to blend).
    pub faces_kept: u32,
}

/// A face Vision found on a Best Shot source, normalized to the upright photo (top-left
/// origin, y down).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct BurstFaceSignal {
    pub bounds: crate::frame::NormRect,
    /// Probability (0..=1) that both eyes are open, if measured.
    pub eyes_open: Option<f32>,
    /// Vision's face capture quality, 0..=1, if known.
    pub quality: Option<f32>,
}

/// What the app knows about one Best Shot source.
#[derive(Debug, Clone, PartialEq, Default, uniffi::Record)]
pub struct BurstSignals {
    pub faces: Vec<BurstFaceSignal>,
    /// Its overall quality (lt-ai's cull score, 0..=1), if known.
    pub score: Option<f32>,
}

/// The photo a merge added.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MergeOutcome {
    pub variant_id: i64,
    pub image_id: i64,
    pub original_path: String,
    pub file_name: String,
}

impl From<Deghost> for lt_merge::Deghost {
    fn from(d: Deghost) -> Self {
        match d {
            Deghost::Off => Self::Off,
            Deghost::Low => Self::Low,
            Deghost::Medium => Self::Medium,
            Deghost::High => Self::High,
        }
    }
}

impl From<PanoProjection> for lt_merge::Projection {
    fn from(p: PanoProjection) -> Self {
        match p {
            PanoProjection::Auto => Self::Auto,
            PanoProjection::Spherical => Self::Spherical,
            PanoProjection::Cylindrical => Self::Cylindrical,
            PanoProjection::Perspective => Self::Perspective,
        }
    }
}

impl From<lt_merge::Projection> for PanoProjection {
    fn from(p: lt_merge::Projection) -> Self {
        match p {
            lt_merge::Projection::Auto => Self::Auto,
            lt_merge::Projection::Spherical => Self::Spherical,
            lt_merge::Projection::Cylindrical => Self::Cylindrical,
            lt_merge::Projection::Perspective => Self::Perspective,
        }
    }
}

fn merge_error(e: MergeError) -> DsError {
    match e {
        MergeError::Cancelled => DsError::Cancelled,
        e => DsError::invalid(e.to_string()),
    }
}

/// `lt-merge` progress → a cancel token and an optional permille callback.
struct Monitor<'a> {
    token: &'a CancelToken,
    report: &'a (dyn Fn(u64) + Sync),
    last: AtomicU64,
}

impl lt_merge::Progress for Monitor<'_> {
    fn set(&self, fraction: f32) {
        let p = (f64::from(fraction.clamp(0.0, 1.0)) * PERMILLE as f64) as u64;
        if self.last.fetch_max(p, Ordering::Relaxed) < p {
            (self.report)(p);
        }
    }

    fn cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

/// A source decoded for merging.
#[derive(Debug, Clone)]
struct Source {
    path: PathBuf,
    frame: Frame,
    meta: Option<lt_raw::ImageMeta>,
    /// The raw's colour (`frame` keeps none of it) and orientation.
    color: RawImage,
}

/// Decodes a RAW: `lt_raw::decode`, or a stand-in (the memory-budget test).
type Decode<'a> = &'a (dyn Fn(&Path) -> Result<RawImage, DsError> + Sync);

fn decode_file(p: &Path) -> Result<RawImage, DsError> {
    Ok(lt_raw::decode(p)?)
}

/// Decode `path`: full resolution (RCD) or, for previews, binned and box-downsampled so
/// the long edge is at most `max_edge`.
fn load(decode: Decode<'_>, path: &Path, max_edge: Option<u32>) -> Result<Source, DsError> {
    let raw = decode(path)?;
    let meta = lt_raw::read_metadata(path).ok();
    let frame = if let Some(edge) = max_edge {
        let quick = lt_merge::quick_frame(&raw).map_err(merge_error)?;
        let f = quick.w.max(quick.h).div_ceil(edge.max(8)).max(1);
        quick.downsampled(f)
    } else {
        let lin = lt_pipeline::denoise::linear_original(&raw, Priority::Background)
            .map_err(crate::develop::pipeline_error)?;
        Frame::new(lin.size.width, lin.size.height, lin.data).map_err(merge_error)?
    };
    let color = RawImage {
        data: Vec::new(),
        ..raw
    };
    Ok(Source {
        path: path.to_owned(),
        frame,
        meta,
        color,
    })
}

/// A file version: path, size and modification time.
type FileKey = (PathBuf, u64, Option<SystemTime>);

fn file_key(path: &Path) -> Result<FileKey, DsError> {
    let md = fs::metadata(path)?;
    Ok((path.to_owned(), md.len(), md.modified().ok()))
}

/// A merge's distinct photos: session paths, files, and each one's first index into the
/// variant ids.
type MergeSources = (Vec<String>, Vec<PathBuf>, Vec<usize>);

/// Recent preview decodes by file version and frame size, oldest first.
type PreviewCache = Mutex<VecDeque<((FileKey, u32), Arc<Source>)>>;

fn preview_cache() -> &'static PreviewCache {
    static C: PreviewCache = Mutex::new(VecDeque::new());
    &C
}

/// [`load`] for previews, reusing the last decodes of the same file version and size.
fn load_preview(path: &Path, edge: u32) -> Result<Arc<Source>, DsError> {
    let key = (file_key(path)?, edge);
    let hit = preview_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, s)| s.clone());
    if let Some(s) = hit {
        return Ok(s);
    }
    let s = Arc::new(load(&decode_file, path, Some(edge))?);
    let mut c = preview_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if c.len() >= PREVIEW_CACHE {
        c.pop_front();
    }
    c.push_back((key, s.clone()));
    Ok(s)
}

/// Whether a preview decode of this version of `path` at `edge` is cached (tests).
#[cfg(test)]
fn preview_cached(path: &Path, edge: u32) -> bool {
    let Ok(k) = file_key(path) else {
        return false;
    };
    preview_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|(key, _)| *key == (k.clone(), edge))
}

/// Relative exposures (time × ISO / N²) from EXIF. Only when every frame records the
/// same fields: a field missing on some frames would be guessed differently per frame.
fn exposures(metas: &[Option<&lt_raw::ImageMeta>]) -> Vec<Option<f32>> {
    let fields = |m: Option<&lt_raw::ImageMeta>| {
        m.map(|m| (m.shutter.is_some(), m.iso.is_some(), m.aperture.is_some()))
    };
    let first = fields(metas.first().copied().flatten());
    if metas.iter().any(|m| fields(*m) != first) {
        return vec![None; metas.len()];
    }
    metas
        .iter()
        .map(|m| {
            let m = (*m)?;
            let t = m.shutter.map(|r| r.num as f32 / r.den as f32)?;
            let iso = m.iso.unwrap_or(100) as f32;
            let n = m.aperture.filter(|a| *a > 0.0).unwrap_or(1.0);
            Some(t * iso / (n * n))
        })
        .collect()
}

/// Focal length in pixels of a frame `long` px across, from the 35 mm equivalent.
fn focal_px(m: Option<&lt_raw::ImageMeta>, long: u32) -> Option<f32> {
    let f35 = m?.focal_length_35mm.filter(|f| *f > 0)?;
    Some(long as f32 * f35 as f32 / 36.0)
}

fn camera(m: Option<&lt_raw::ImageMeta>) -> (String, String) {
    m.map_or_else(Default::default, |m| (m.make.clone(), m.model.clone()))
}

/// One camera and one focal length: a merge models one camera's colour and (for a
/// panorama) one focal length.
fn check_compatible(metas: &[Option<&lt_raw::ImageMeta>]) -> Result<(), DsError> {
    let cams: Vec<(String, String)> = metas.iter().map(|m| camera(*m)).collect();
    if cams.iter().any(|c| *c != cams[0]) {
        return Err(DsError::invalid("Merges need photos from one camera"));
    }
    let focal: Vec<f32> = metas
        .iter()
        .filter_map(|m| m.and_then(|m| m.focal_length).filter(|f| *f > 0.0))
        .collect();
    let (lo, hi) = focal
        .iter()
        .fold((f32::MAX, 0.0f32), |(lo, hi), &f| (lo.min(f), hi.max(f)));
    if focal.len() == metas.len() && hi > lo * FOCAL_TOLERANCE {
        return Err(DsError::invalid(
            "Merges need photos taken at one focal length",
        ));
    }
    Ok(())
}

/// A merge result and, for Best Shot, what it took from where.
#[derive(Debug)]
struct Outcome {
    merged: Merged,
    faces_replaced: u32,
    regions_replaced: u32,
    faces_kept: u32,
}

impl From<Merged> for Outcome {
    fn from(merged: Merged) -> Self {
        Self {
            merged,
            faces_replaced: 0,
            regions_replaced: 0,
            faces_kept: 0,
        }
    }
}

/// The frames a Best Shot merge uses: all of them up to [`BURST_MAX_FRAMES`], else the
/// best scored (unscored last, capture order among equals), in their original order.
fn burst_frames(signals: &[BurstSignals], n: usize) -> Vec<usize> {
    if n <= BURST_MAX_FRAMES {
        return (0..n).collect();
    }
    let key = |i: usize| {
        signals
            .get(i)
            .and_then(|s| s.score)
            .filter(|s| s.is_finite())
            .unwrap_or(f32::NEG_INFINITY)
    };
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| key(b).total_cmp(&key(a)).then(a.cmp(&b)));
    order.truncate(BURST_MAX_FRAMES);
    order.sort_unstable();
    order
}

/// Signals for `n` sources: one each, or none at all (no faces, no scores).
fn check_signals(signals: &[BurstSignals], n: usize) -> Result<(), DsError> {
    if signals.is_empty() || signals.len() == n {
        Ok(())
    } else {
        Err(DsError::invalid(format!(
            "{} face lists for {n} photos",
            signals.len()
        )))
    }
}

/// Merge the sources (panorama and Best Shot frames are turned upright first). `signals`:
/// Best Shot's faces and scores per source, or none.
fn run_merge(
    sources: &mut [Source],
    options: MergeOptions,
    signals: &[BurstSignals],
    progress: &dyn lt_merge::Progress,
) -> Result<Outcome, DsError> {
    if sources.len() < 2 {
        return Err(merge_error(MergeError::TooFew));
    }
    let metas: Vec<Option<&lt_raw::ImageMeta>> = sources.iter().map(|s| s.meta.as_ref()).collect();
    check_compatible(&metas)?;
    check_signals(signals, sources.len())?;
    match options.kind {
        MergeKind::Hdr => {
            let expo = exposures(&metas);
            let inputs: Vec<lt_merge::HdrInput<'_>> = sources
                .iter()
                .zip(expo)
                .map(|(s, exposure)| lt_merge::HdrInput {
                    frame: &s.frame,
                    exposure,
                })
                .collect();
            let opts = lt_merge::HdrOptions {
                align: options.align,
                deghost: options.deghost.into(),
            };
            lt_merge::merge_hdr(&inputs, &opts, progress)
                .map(Outcome::from)
                .map_err(merge_error)
        }
        MergeKind::Panorama => {
            for s in sources.iter_mut() {
                s.frame = std::mem::take(&mut s.frame).upright(s.color.orientation);
            }
            let inputs: Vec<lt_merge::PanoInput<'_>> = sources
                .iter()
                .map(|s| lt_merge::PanoInput {
                    frame: &s.frame,
                    focal_px: focal_px(s.meta.as_ref(), s.frame.w.max(s.frame.h)),
                })
                .collect();
            let opts = lt_merge::PanoOptions {
                projection: options.projection.into(),
                auto_crop: options.auto_crop,
                ..lt_merge::PanoOptions::default()
            };
            lt_merge::merge_panorama(&inputs, &opts, progress)
                .map(Outcome::from)
                .map_err(merge_error)
        }
        MergeKind::Burst => run_burst(sources, signals, progress),
    }
}

/// [`run_merge`] for Best Shot: upright frames with their faces and scores.
fn run_burst(
    sources: &mut [Source],
    signals: &[BurstSignals],
    progress: &dyn lt_merge::Progress,
) -> Result<Outcome, DsError> {
    for s in sources.iter_mut() {
        s.frame = std::mem::take(&mut s.frame).upright(s.color.orientation);
    }
    let faces: Vec<Vec<lt_merge::BurstFace>> = (0..sources.len())
        .map(|i| {
            signals.get(i).map_or_else(Vec::new, |s| {
                s.faces
                    .iter()
                    .map(|f| lt_merge::BurstFace {
                        bounds: lt_core::NormRect {
                            x: f.bounds.x,
                            y: f.bounds.y,
                            w: f.bounds.w,
                            h: f.bounds.h,
                        },
                        eyes_open: f.eyes_open,
                        quality: f.quality,
                    })
                    .collect()
            })
        })
        .collect();
    let inputs: Vec<lt_merge::BurstInput<'_>> = sources
        .iter()
        .zip(&faces)
        .enumerate()
        .map(|(i, (s, faces))| lt_merge::BurstInput {
            frame: &s.frame,
            faces,
            score: signals.get(i).and_then(|s| s.score),
        })
        .collect();
    let c = lt_merge::merge_burst(&inputs, &lt_merge::BurstOptions::default(), progress)
        .map_err(merge_error)?;
    let count = |k: lt_merge::PatchKind| c.patches.iter().filter(|p| p.kind == k).count() as u32;
    Ok(Outcome {
        faces_replaced: count(lt_merge::PatchKind::Face),
        regions_replaced: count(lt_merge::PatchKind::Sharper),
        faces_kept: c.faces_kept,
        merged: Merged {
            frame: c.frame,
            exposure_ev: 0.0,
            reference: c.reference,
            ghost_regions: 0,
            projection: lt_merge::Projection::Perspective,
            unaligned: c.unaligned,
        },
    })
}

/// `frame` as a linear raw with the reference frame's colour.
fn merged_raw(frame: Frame, reference: &Source, orientation: Orientation) -> RawImage {
    RawImage {
        size: Size::new(frame.w, frame.h),
        cfa: CfaPattern::LinearRgb,
        data: frame.data,
        black_level: [0.0; 4],
        white_level: lt_merge::WHITE,
        orientation,
        default_crop: None,
        ..reference.color.clone()
    }
}

/// The result's EXIF orientation: the reference's for HDR (sensor data), none for a
/// panorama or a Best Shot (merged upright).
fn result_orientation(reference: &Source, kind: MergeKind) -> Orientation {
    match kind {
        MergeKind::Hdr => reference.color.orientation,
        MergeKind::Panorama | MergeKind::Burst => Orientation::Normal,
    }
}

/// The new photo's starting edit: `base` at the exposure matching the reference frame.
fn initial_edit(mut base: Adjustments, exposure_ev: f32, kind: MergeKind) -> Adjustments {
    base.exposure.exposure_ev = (base.exposure.exposure_ev + exposure_ev).clamp(-MAX_EV, MAX_EV);
    if kind == MergeKind::Panorama {
        base.lens.enabled = false; // the projection already defines the geometry
    }
    base
}

/// Develop `raw` with `adj` into a JPEG at most `edge` px long.
fn develop_jpeg(raw: RawImage, adj: &Adjustments, edge: u32) -> Result<(Vec<u8>, Size), DsError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // Keys of transient images live in their own range (file keys are hashes).
    let key = ImageKey(0x4d45_5247_0000_0000 | NEXT.fetch_add(1, Ordering::Relaxed));
    let loaded = Loaded {
        key,
        raw: Arc::new(raw),
        capture: lt_pipeline::CaptureInfo::default(),
    };
    let d = develop();
    let req = RenderRequest {
        priority: Priority::Tool,
        ..RenderRequest::new(Size::new(edge, edge))
    };
    let size =
        lt_pipeline::output_size(&loaded.raw, adj, &req).map_err(crate::develop::pipeline_error)?;
    let mut rgba = vec![0u8; size.pixels() as usize * 4];
    let r = d.render_now(&loaded, adj, &req, OutputFormat::Rgba8, &mut rgba);
    d.evict(key);
    r?;
    Ok((crate::tools::encode_jpeg(&rgba, size, 85.0)?, size))
}

/// A merged preview of the RAWs at `paths`, its long edge at most `max_edge` px.
#[uniffi::export]
pub async fn merge_preview(
    paths: Vec<String>,
    options: MergeOptions,
    max_edge: u32,
    token: Arc<CancelToken>,
) -> Result<MergePreview, DsError> {
    preview(paths, options, Vec::new(), max_edge, token).await
}

/// A preview of a Best Shot of the RAWs at `paths` with their `signals` (one per path, or
/// none), its long edge at most `max_edge` px.
#[uniffi::export]
pub async fn burst_preview(
    paths: Vec<String>,
    signals: Vec<BurstSignals>,
    max_edge: u32,
    token: Arc<CancelToken>,
) -> Result<MergePreview, DsError> {
    preview(paths, BURST, signals, max_edge, token).await
}

async fn preview(
    paths: Vec<String>,
    options: MergeOptions,
    signals: Vec<BurstSignals>,
    max_edge: u32,
    token: Arc<CancelToken>,
) -> Result<MergePreview, DsError> {
    let t = token.clone();
    run_job(token, move |_| {
        let edge = max_edge.clamp(64, 4096);
        // A panorama registers at the merge's analysis size: a smaller preview could
        // fail to stitch what the merge stitches.
        let frame_edge = match options.kind {
            MergeKind::Hdr | MergeKind::Burst => edge,
            MergeKind::Panorama => PANO_PREVIEW_FRAME,
        };
        check_signals(&signals, paths.len())?;
        let keep = kept(options.kind, &signals, paths.len());
        let (paths, signals) = (pick(&paths, &keep), pick(&signals, &keep));
        let mut sources = Vec::with_capacity(paths.len());
        for p in &paths {
            if t.is_cancelled() {
                return Err(DsError::Cancelled);
            }
            sources.push((*load_preview(Path::new(p), frame_edge)?).clone());
        }
        let monitor = Monitor {
            token: &t,
            report: &|_| {},
            last: AtomicU64::new(0),
        };
        let outcome = run_merge(&mut sources, options, &signals, &monitor)?;
        let merged = outcome.merged;
        let (ev, ghosts, projection, unaligned) = (
            merged.exposure_ev,
            merged.ghost_regions,
            merged.projection,
            merged.unaligned,
        );
        let reference = &sources[merged.reference];
        let raw = merged_raw(
            merged.frame,
            reference,
            result_orientation(reference, options.kind),
        );
        let adj = initial_edit(Adjustments::default(), ev, options.kind);
        let (jpeg, size) = develop_jpeg(raw, &adj, edge)?;
        Ok(MergePreview {
            jpeg,
            width: size.width,
            height: size.height,
            projection: projection.into(),
            ghost_regions: ghosts,
            exposure_ev: ev.min(MAX_EV),
            unaligned_frames: unaligned,
            faces_replaced: outcome.faces_replaced,
            regions_replaced: outcome.regions_replaced,
            faces_kept: outcome.faces_kept,
        })
    })
    .await
}

/// The sources a merge of `kind` uses (see [`burst_frames`]).
fn kept(kind: MergeKind, signals: &[BurstSignals], n: usize) -> Vec<usize> {
    if kind == MergeKind::Burst {
        burst_frames(signals, n)
    } else {
        (0..n).collect()
    }
}

/// `items` at `keep`.
fn pick<T: Clone>(items: &[T], keep: &[usize]) -> Vec<T> {
    keep.iter().filter_map(|&i| items.get(i).cloned()).collect()
}

/// The embedded preview of a merged frame: developed from a downsampled copy, and in
/// sensor orientation (readers apply the DNG's `Orientation` to it, as to a camera's).
fn embedded_preview(
    frame: &Frame,
    reference: &Source,
    ev: f32,
    kind: MergeKind,
) -> Result<(Vec<u8>, Size), DsError> {
    let f = frame
        .w
        .max(frame.h)
        .div_ceil(2 * EMBEDDED_PREVIEW_EDGE)
        .max(1);
    let small = merged_raw(frame.downsampled(f), reference, Orientation::Normal);
    develop_jpeg(
        small,
        &initial_edit(Adjustments::default(), ev, kind),
        EMBEDDED_PREVIEW_EDGE,
    )
}

/// `<stem>-HDR.dng`, `<stem>-HDR-2.dng`, … for the `n`th attempt.
fn candidate_name(stem: &str, kind: MergeKind, n: u32) -> String {
    let tag = match kind {
        MergeKind::Hdr => "HDR",
        MergeKind::Panorama => "Pano",
        MergeKind::Burst => "Best",
    };
    if n <= 1 {
        format!("{stem}-{tag}.dng")
    } else {
        format!("{stem}-{tag}-{n}.dng")
    }
}

/// Write a new file into `dir` under the first free candidate name: `write` streams it
/// to a temp file, which is fsynced, hashed from disk and renamed without ever
/// replacing a file (then the directory is synced). Returns the name and the hash.
fn write_new(
    dir: &Path,
    stem: &str,
    kind: MergeKind,
    write: impl FnOnce(&mut BufWriter<fs::File>) -> Result<(), DsError>,
) -> Result<(String, FileHash), DsError> {
    let tmp = lt_fsutil::temp_path(dir, "dng");
    let res = (|| {
        let f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let mut out = BufWriter::new(f);
        write(&mut out)?;
        out.flush()?;
        out.get_ref().sync_all()?;
        drop(out);
        let hash = FileHash(
            *blake3::Hasher::new()
                .update_mmap(&tmp)?
                .finalize()
                .as_bytes(),
        );
        for n in 1..=999 {
            let name = candidate_name(stem, kind, n);
            match lt_fsutil::rename_noreplace(&tmp, &dir.join(&name)) {
                Ok(()) => {
                    fs::File::open(dir)?.sync_all()?;
                    return Ok((name, hash));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(DsError::Io {
            message: "no free file name".into(),
        })
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

/// Remove `path` if it still holds exactly what we wrote (`hash`): never a user's file.
fn remove_if_ours(path: &Path, hash: FileHash) -> bool {
    let ours = blake3::Hasher::new()
        .update_mmap(path)
        .is_ok_and(|h| *h.finalize().as_bytes() == hash.0);
    ours && fs::remove_file(path).is_ok()
}

#[uniffi::export]
impl SessionHandle {
    /// Merge the photos of `variant_ids` into a new photo next to the first one (see the
    /// module docs). Progress events: kind `"merge"`, permille.
    pub async fn merge_photos(
        self: Arc<Self>,
        variant_ids: Vec<i64>,
        options: MergeOptions,
        listener: Option<Arc<dyn ProgressListener>>,
        token: Arc<CancelToken>,
    ) -> Result<MergeOutcome, DsError> {
        let events = Arc::new(Dispatcher::new());
        events.set_listener(listener);
        let ev = events.clone();
        let t = token.clone();
        let r = run_job_on_thread("lt-merge", token, move |_| {
            let report = move |p: u64| {
                ev.emit(ProgressEvent {
                    task_id: 1,
                    kind: "merge".into(),
                    phase: TaskPhase::Running,
                    completed: p.min(PERMILLE),
                    total: PERMILLE,
                });
            };
            self.merge_now(&decode_file, &variant_ids, options, &t, &report)
        })
        .await;
        events.flush(std::time::Duration::from_secs(5));
        r
    }

    /// Merge the burst of `variant_ids` into a Best Shot next to the first one, with the
    /// app's `signals` (faces and scores, one per variant, or none). Progress events: kind
    /// `"merge"`, permille.
    pub async fn merge_burst(
        self: Arc<Self>,
        variant_ids: Vec<i64>,
        signals: Vec<BurstSignals>,
        listener: Option<Arc<dyn ProgressListener>>,
        token: Arc<CancelToken>,
    ) -> Result<MergeOutcome, DsError> {
        let events = Arc::new(Dispatcher::new());
        events.set_listener(listener);
        let ev = events.clone();
        let t = token.clone();
        let r = run_job_on_thread("lt-merge", token, move |_| {
            let report = move |p: u64| {
                ev.emit(ProgressEvent {
                    task_id: 1,
                    kind: "merge".into(),
                    phase: TaskPhase::Running,
                    completed: p.min(PERMILLE),
                    total: PERMILLE,
                });
            };
            self.merge_with(&decode_file, &variant_ids, BURST, &signals, &t, &report)
        })
        .await;
        events.flush(std::time::Duration::from_secs(5));
        r
    }
}

/// The options of a Best Shot merge (it reads none of them but the kind).
const BURST: MergeOptions = MergeOptions {
    kind: MergeKind::Burst,
    align: true,
    deghost: Deghost::Off,
    projection: PanoProjection::Perspective,
    auto_crop: false,
};

impl SessionHandle {
    fn merge_now(
        &self,
        decode: Decode<'_>,
        variant_ids: &[i64],
        options: MergeOptions,
        token: &CancelToken,
        report: &(dyn Fn(u64) + Sync),
    ) -> Result<MergeOutcome, DsError> {
        self.merge_with(decode, variant_ids, options, &[], token, report)
    }

    /// [`Self::merge_now`] with Best Shot's `signals` (one per variant id, or none).
    fn merge_with(
        &self,
        decode: Decode<'_>,
        variant_ids: &[i64],
        options: MergeOptions,
        signals: &[BurstSignals],
        token: &CancelToken,
        report: &(dyn Fn(u64) + Sync),
    ) -> Result<MergeOutcome, DsError> {
        self.check_folder()?;
        check_signals(signals, variant_ids.len())?;
        let (rels, paths, firsts) = self.merge_sources(variant_ids)?;
        // One signal per distinct photo (its first variant's), then the frames used.
        let signals: Vec<BurstSignals> = if signals.is_empty() {
            Vec::new()
        } else {
            firsts.iter().map(|&i| signals[i].clone()).collect()
        };
        let keep = kept(options.kind, &signals, paths.len());
        let (rels, paths) = (pick(&rels, &keep), pick(&paths, &keep));
        let signals = pick(&signals, &keep);
        // The new photo carries the first selected photo's title, caption and credits.
        let iptc = lock(&self.session).variant(VariantId(variant_ids[0]))?.iptc;
        let monitor = Monitor {
            token,
            report,
            last: AtomicU64::new(0),
        };
        let step = |a: f32, b: f32| lt_merge_stage(&monitor, a, b);
        // One decode at a time: each runs in parallel inside, and only one demosaic
        // buffer exists at once.
        let mut sources = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            if token.is_cancelled() {
                return Err(DsError::Cancelled);
            }
            sources.push(load(decode, p, None)?);
            lt_merge::Progress::set(&step(0.0, 0.3), (i + 1) as f32 / paths.len() as f32);
        }
        let merged = run_merge(&mut sources, options, &signals, &step(0.3, 0.9))?.merged;
        let (ev, frame) = (merged.exposure_ev, merged.frame);
        // Only the reference's description is needed from here on: drop every frame.
        let mut reference = sources.swap_remove(merged.reference);
        drop(sources);
        reference.frame = Frame::default();
        let (preview, psize) = embedded_preview(&frame, &reference, ev, options.kind)?;
        lt_merge::Progress::set(&monitor, 0.9);
        let raw = merged_raw(
            frame,
            &reference,
            result_orientation(&reference, options.kind),
        );
        if token.is_cancelled() {
            return Err(DsError::Cancelled);
        }
        // Next to the first photo selected.
        let first = Path::new(&rels[0]);
        let rel_dir = first.parent().unwrap_or(Path::new(""));
        let stem = first
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Merged");
        let dir = paths[0].parent().unwrap_or(Path::new("."));
        let (make, model) = camera(reference.meta.as_ref());
        let (name, hash) = write_new(dir, stem, options.kind, |out| {
            let meta = lt_raw::DngMeta {
                make: &make,
                model: &model,
                exif_from: Some(&reference.path),
                preview: Some((&preview, psize.width, psize.height)),
                software: "Luxtable",
            };
            Ok(lt_raw::write_linear_dng(out, raw, &meta)?)
        })?;
        let rel = rel_dir.join(&name).to_string_lossy().into_owned();
        let dest = dir.join(&name);
        lt_merge::Progress::set(&monitor, 0.98);
        let (image, variant) = match self.register(rel, &dest, hash, ev, options.kind, iptc) {
            Ok(r) => r,
            Err(e) => {
                // Not in the session: take back the file this merge just made.
                remove_if_ours(&dest, hash);
                return Err(e);
            }
        };
        lt_merge::Progress::set(&monitor, 1.0);
        Ok(MergeOutcome {
            variant_id: variant.0,
            image_id: image.0,
            original_path: dest.to_string_lossy().into_owned(),
            file_name: name,
        })
    }

    /// The distinct photos of `variant_ids`: their session paths and files, and the index
    /// into `variant_ids` of each one's first variant.
    fn merge_sources(&self, variant_ids: &[i64]) -> Result<MergeSources, DsError> {
        let s = lock(&self.session);
        let mut images: Vec<ImageId> = Vec::new();
        let mut firsts = Vec::new();
        for (i, &v) in variant_ids.iter().enumerate() {
            if let Ok(info) = s.variant(VariantId(v))
                && !images.contains(&info.image)
            {
                images.push(info.image);
                firsts.push(i);
            }
        }
        if images.len() < 2 {
            return Err(merge_error(MergeError::TooFew));
        }
        let mut rels = Vec::new();
        let mut paths = Vec::new();
        for id in &images {
            let info = s.image(*id)?;
            paths.push(s.original_path(info.rel_path.as_str()));
            rels.push(info.rel_path.as_str().to_owned());
        }
        Ok((rels, paths, firsts))
    }

    /// Add the written file at `rel` as a new photo starting at `ev` (see [`initial_edit`])
    /// with `iptc` (the first selected photo's).
    fn register(
        &self,
        rel: String,
        dest: &Path,
        hash: FileHash,
        ev: f32,
        kind: MergeKind,
        iptc: IptcMeta,
    ) -> Result<(ImageId, VariantId), DsError> {
        let meta = super::read_meta(dest)
            .map(|(lite, _)| lite)
            .unwrap_or_default();
        let mut s = lock(&self.session);
        let ids = s.add_new_originals(vec![NewOriginal {
            rel,
            hash,
            content_hash: None,
            meta,
            replaced: Vec::new(),
            iptc,
        }])?;
        let image = ids[0];
        let variant = s.variants(image)?[0];
        let start = initial_edit(s.load_adjustments(variant)?, ev, kind);
        s.save_adjustments(variant, &start)?;
        Ok((image, variant))
    }
}

/// A sub-range of the monitor's progress.
fn lt_merge_stage<'a>(m: &'a Monitor<'a>, a: f32, b: f32) -> SubRange<'a> {
    SubRange { parent: m, a, b }
}

struct SubRange<'a> {
    parent: &'a Monitor<'a>,
    a: f32,
    b: f32,
}

impl lt_merge::Progress for SubRange<'_> {
    fn set(&self, f: f32) {
        lt_merge::Progress::set(self.parent, self.a + f.clamp(0.0, 1.0) * (self.b - self.a));
    }

    fn cancelled(&self) -> bool {
        self.parent.token.is_cancelled()
    }
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
