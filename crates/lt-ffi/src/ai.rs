//! lt-ai facade: culling scores, burst/moment grouping, and the offline
//! natural-language edit parser.
//!
//! Culling is stateful so Swift can stream previews one at a time: each
//! [`CullSession::add`] analyses one preview (off-thread, cancellable) and keeps
//! only the compact [`lt_ai::ImageAnalysis`]; [`CullSession::finish`] groups the
//! frames into moments and scores each group against itself. The pixels never
//! stay in memory, so a 2 000-frame cull holds analyses, not previews.
//!
//! Stack building ([`group_burst_items`]) is a separate, synchronous call on
//! metadata alone (capture time, shot/file sequence, camera): without pixels
//! there are no perceptual hashes, so only burst timing links frames.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};

use lt_ai::{
    AnalysisConfig, CullScore, CullThresholds, CullWeights, EditIntent, FaceSignal, GroupConfig,
    GroupItem, GroupKind, ImageAnalysis, LumaImage, PerceptualHash, RuleBasedParser, Straighten,
    SubjectHints, VisionSignals,
};

use crate::error::DsError;
use crate::frame::NormRect;
use crate::jobs::{CancelToken, guard, run_job, run_job_on_thread};

// ------------------------------------------------------------------ records ---

/// dHash + pHash of a preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct PHashRecord {
    pub dhash: u64,
    pub phash: u64,
}

/// One face as Apple Vision saw it. Rects are normalized with a top-left origin.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct FaceSignalRecord {
    /// Probability (0..=1) that both eyes are open.
    pub eyes_open: Option<f32>,
    pub capture_quality: Option<f32>,
    pub area: Option<f32>,
    pub confidence: Option<f32>,
    pub bounds: Option<NormRect>,
}

/// Vision signals and subject hints for one frame.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct VisionSignalsRecord {
    pub faces: Vec<FaceSignalRecord>,
    /// Vision aesthetics `overallScore`, -1..=1.
    pub aesthetics: Option<f32>,
    pub is_utility: Option<bool>,
    /// Camera AF point (normalized, top-left origin), from lt-raw.
    pub af_point: Option<NormPoint>,
    /// Attention-saliency rectangles, most salient first.
    pub saliency: Vec<NormRect>,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct NormPoint {
    pub x: f32,
    pub y: f32,
}

/// One frame to cull: an 8-bit RGBA preview plus what groups it.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct CullInput {
    /// Caller's id (the Swift `PhotoID`); echoed in the results.
    pub id: i64,
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    pub capture_ms: Option<i64>,
    /// Shot number, else the file-name sequence (orders frames in one second).
    pub seq: Option<u64>,
    /// Camera identity (make + model + serial); frames of different bodies never group.
    pub camera: Option<String>,
    pub vision: Option<VisionSignalsRecord>,
    /// File name, for reject notes that point at a group's better frame.
    #[uniffi(default = None)]
    pub name: Option<String>,
}

/// What one analysis measured (for diagnostics and the UI).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct AnalysisSummary {
    pub id: i64,
    /// Lone-frame focus measure (lt-ai `Sharpness::focus`).
    pub focus: f32,
    pub highlights_clipped: f32,
    pub shadows_clipped: f32,
    pub noise_sigma: f32,
    pub hash: PHashRecord,
    /// A subject region (AF point, face, saliency) was measured.
    pub has_subject: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum GroupKindRecord {
    Single,
    Burst,
    NearDuplicate,
}

impl From<GroupKind> for GroupKindRecord {
    fn from(k: GroupKind) -> Self {
        match k {
            GroupKind::Single => Self::Single,
            GroupKind::Burst => Self::Burst,
            GroupKind::NearDuplicate => Self::NearDuplicate,
        }
    }
}

/// A moment: ids in capture order (the first is the anchor).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GroupRecord {
    pub kind: GroupKindRecord,
    pub member_ids: Vec<i64>,
}

/// One reason behind a score, with the UI string.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ReasonRecord {
    /// Stable `snake_case` kind (`out_of_focus`, `eyes_closed`, …).
    pub kind: String,
    pub text: String,
    /// A hard failure a pick should never have.
    pub hard: bool,
    /// Counts against the frame (everything except "sharpest in group" and
    /// "focus uncertain").
    pub negative: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PickSuggestionKind {
    /// The group's best frame.
    Best,
    /// Clearly worse than the best, or a hard failure.
    Reject,
    /// Neither.
    Keep,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct CullResultRecord {
    pub id: i64,
    /// 0..=1.
    pub score: f32,
    pub sharpness: f32,
    pub exposure: f32,
    pub noise: Option<f32>,
    pub reasons: Vec<ReasonRecord>,
    /// Index into [`CullReport::groups`].
    pub group: u32,
    pub suggestion: PickSuggestionKind,
    /// Why this frame would be rejected (lt-ai `explain_reject`), most important
    /// first; never empty. `kind` is the stable code (`out_of_focus`,
    /// `duplicate`, `low_score`, …).
    #[uniffi(default)]
    pub reject_reasons: Vec<ReasonRecord>,
    /// `reject_reasons` as one line for the app (lt-ai `reject_note`).
    #[uniffi(default = "")]
    pub reject_note: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct CullReport {
    /// In the order frames were added.
    pub results: Vec<CullResultRecord>,
    pub groups: Vec<GroupRecord>,
}

/// Metadata-only item for stack building.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct BurstItem {
    pub id: i64,
    pub capture_ms: Option<i64>,
    pub seq: Option<u64>,
    pub camera: Option<String>,
    /// When known (e.g. after a cull), visual similarity also links frames.
    pub hash: Option<PHashRecord>,
}

/// A typed edit from text (lt-ai `EditIntent`), plus what was not understood.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct EditIntentRecord {
    /// Warmer (+) / cooler (−), -100..=100 slider units (100 = 60 mired).
    pub warmth: f32,
    pub temperature_k: Option<f32>,
    pub temperature_shift_k: f32,
    pub tint: f32,
    pub exposure_ev: f32,
    pub contrast: f32,
    pub saturation: f32,
    /// + lifts shadows.
    pub shadows: f32,
    /// + recovers (darkens) highlights, as in Capture One and lt-core.
    pub highlights: f32,
    pub band_edits: Vec<BandEditRecord>,
    /// Spoken width:height ("4:5" is portrait).
    pub crop_width: Option<u32>,
    pub crop_height: Option<u32>,
    pub straighten_auto: bool,
    /// Counter-clockwise positive.
    pub straighten_degrees: Option<f32>,
    pub black_and_white: Option<bool>,
    pub auto: bool,
    pub auto_white_balance: bool,
    pub unrecognized: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct BandEditRecord {
    /// `red`, `orange`, … `magenta` (lt-core `BASIC_BAND_HUES` order).
    pub band: String,
    pub saturation: f32,
    pub lightness: f32,
}

// ---------------------------------------------------------------- mapping ---

impl From<PerceptualHash> for PHashRecord {
    fn from(h: PerceptualHash) -> Self {
        Self {
            dhash: h.dhash,
            phash: h.phash,
        }
    }
}

impl From<PHashRecord> for PerceptualHash {
    fn from(h: PHashRecord) -> Self {
        Self {
            dhash: h.dhash,
            phash: h.phash,
        }
    }
}

fn core_rect(r: NormRect) -> lt_core::NormRect {
    lt_core::NormRect {
        x: r.x,
        y: r.y,
        w: r.w,
        h: r.h,
    }
}

fn camera_key(camera: Option<&str>) -> Option<u64> {
    camera.map(|c| {
        // DefaultHasher::new() uses fixed keys: stable within and across runs.
        let mut h = DefaultHasher::new();
        c.hash(&mut h);
        h.finish()
    })
}

impl VisionSignalsRecord {
    fn signals(&self) -> VisionSignals {
        VisionSignals {
            faces: self
                .faces
                .iter()
                .map(|f| FaceSignal {
                    eyes_open: f.eyes_open,
                    capture_quality: f.capture_quality,
                    area: f.area,
                    confidence: f.confidence,
                    bounds: f.bounds.map(core_rect),
                })
                .collect(),
            aesthetics: self.aesthetics,
            is_utility: self.is_utility,
            af_point: self.af_point.map(|p| [p.x, p.y]),
        }
    }

    fn hints(&self) -> SubjectHints {
        SubjectHints {
            af_point: self.af_point.map(|p| [p.x, p.y]),
            faces: self
                .faces
                .iter()
                .filter_map(|f| f.bounds)
                .map(core_rect)
                .collect(),
            saliency: self.saliency.iter().copied().map(core_rect).collect(),
        }
    }
}

fn reason_kind(r: lt_ai::Reason) -> String {
    // The serde tag is the stable kind name.
    serde_json::to_value(r)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn reasons(score: &CullScore) -> Vec<ReasonRecord> {
    score
        .reasons
        .iter()
        .map(|r| ReasonRecord {
            kind: reason_kind(*r),
            text: r.to_string(),
            hard: r.is_hard(),
            negative: r.is_negative(),
        })
        .collect()
}

fn reject_reasons(reasons: &[lt_ai::RejectReason]) -> Vec<ReasonRecord> {
    reasons
        .iter()
        .map(|r| ReasonRecord {
            kind: r.code().to_owned(),
            text: r.to_string(),
            hard: r.is_hard(),
            negative: true,
        })
        .collect()
}

fn band_name(b: lt_ai::ColorBand) -> String {
    serde_json::to_value(b)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl EditIntentRecord {
    fn from_intent(i: &EditIntent, unrecognized: Vec<String>) -> Self {
        let (straighten_auto, straighten_degrees) = match i.straighten {
            Some(Straighten::Auto) => (true, None),
            Some(Straighten::Degrees(d)) => (false, Some(d)),
            None => (false, None),
        };
        Self {
            warmth: i.warmth,
            temperature_k: i.temperature_k,
            temperature_shift_k: i.temperature_shift_k,
            tint: i.tint,
            exposure_ev: i.exposure_ev,
            contrast: i.contrast,
            saturation: i.saturation,
            shadows: i.shadows,
            highlights: i.highlights,
            band_edits: i
                .color_bands
                .iter()
                .map(|b| BandEditRecord {
                    band: band_name(b.band),
                    saturation: b.saturation,
                    lightness: b.lightness,
                })
                .collect(),
            crop_width: i.crop_aspect.map(|a| a.width),
            crop_height: i.crop_aspect.map(|a| a.height),
            straighten_auto,
            straighten_degrees,
            black_and_white: i.black_and_white,
            auto: i.auto,
            auto_white_balance: i.auto_white_balance,
            unrecognized,
        }
    }
}

fn luma(width: u32, height: u32, rgba: &[u8]) -> Result<LumaImage, DsError> {
    LumaImage::from_rgba8(width as usize, height as usize, rgba)
        .map_err(|e| DsError::invalid(e.to_string()))
}

// ------------------------------------------------------------ cull session ---

struct Frame {
    id: i64,
    capture_ms: Option<i64>,
    seq: Option<u64>,
    camera: Option<u64>,
    name: Option<String>,
    analysis: ImageAnalysis,
    vision: Option<VisionSignals>,
}

/// Accumulates per-frame analyses, then groups and scores them.
#[derive(Default, uniffi::Object)]
pub struct CullSession {
    frames: Mutex<Vec<Frame>>,
}

impl std::fmt::Debug for CullSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self
            .frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        f.debug_struct("CullSession").field("frames", &n).finish()
    }
}

impl CullSession {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Store one frame's analysis and summarize it.
    fn keep(&self, input: &CullInput, analysis: ImageAnalysis) -> AnalysisSummary {
        let summary = AnalysisSummary {
            id: input.id,
            focus: analysis.sharpness.focus,
            highlights_clipped: analysis.exposure.highlights_clipped,
            shadows_clipped: analysis.exposure.shadows_clipped,
            noise_sigma: analysis.noise.sigma,
            hash: analysis.hash.into(),
            has_subject: analysis.sharpness.subject.is_some(),
        };
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Frame {
                id: input.id,
                capture_ms: input.capture_ms,
                seq: input.seq,
                camera: camera_key(input.camera.as_deref()),
                name: input.name.clone(),
                analysis,
                vision: input.vision.as_ref().map(VisionSignalsRecord::signals),
            });
        summary
    }
}

/// Subject regions (AF point, faces, saliency) Vision reported for one input.
fn subject_regions(input: &CullInput) -> Vec<lt_ai::SubjectRegion> {
    input
        .vision
        .as_ref()
        .map(|v| v.hints().regions())
        .unwrap_or_default()
}

/// The process-wide batch analyzer (one GPU context for every cull).
fn analyzer() -> &'static Mutex<lt_ai::Analyzer> {
    static A: std::sync::OnceLock<Mutex<lt_ai::Analyzer>> = std::sync::OnceLock::new();
    A.get_or_init(|| Mutex::new(lt_ai::Analyzer::new(AnalysisConfig::default())))
}

#[uniffi::export]
impl CullSession {
    #[uniffi::constructor(name = "new")]
    pub fn ffi_new() -> Result<Arc<Self>, DsError> {
        crate::jobs::guard(|| Ok(Self::new()))
    }

    /// Analyse one preview off the calling thread and keep the result.
    pub async fn add(
        self: Arc<Self>,
        input: CullInput,
        token: Arc<CancelToken>,
    ) -> Result<AnalysisSummary, DsError> {
        let me = self.clone();
        run_job(token, move |cancelled| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            let img = luma(input.width, input.height, &input.rgba)?;
            let regions = subject_regions(&input);
            let analysis = lt_ai::analyze_with_subject(&img, &AnalysisConfig::default(), &regions);
            Ok(me.keep(&input, analysis))
        })
        .await
    }

    /// Analyse a batch of previews (16–32 per call is the sweet spot) on the
    /// process-wide [`lt_ai::Analyzer`]: the GPU when an adapter is available
    /// (bit-identical to [`CullSession::add`]), else the CPU in parallel. Runs on
    /// a thread of its own (it waits for the GPU; never on the rayon pool).
    pub async fn add_batch(
        self: Arc<Self>,
        inputs: Vec<CullInput>,
        token: Arc<CancelToken>,
    ) -> Result<Vec<AnalysisSummary>, DsError> {
        let me = self.clone();
        run_job_on_thread("lt-cull-batch", token, move |cancelled| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            let images = inputs
                .iter()
                .map(|i| luma(i.width, i.height, &i.rgba))
                .collect::<Result<Vec<_>, _>>()?;
            let subjects: Vec<_> = inputs.iter().map(subject_regions).collect();
            let analyses = analyzer()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .analyze_batch_with_subjects(&images, &subjects);
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            Ok(inputs
                .iter()
                .zip(analyses)
                .map(|(input, analysis)| me.keep(input, analysis))
                .collect())
        })
        .await
    }

    /// Number of frames analysed so far.
    pub fn count(&self) -> Result<u32, DsError> {
        guard(|| {
            Ok(self
                .frames
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len() as u32)
        })
    }

    /// Group the frames into moments and score each against its group.
    pub async fn finish(self: Arc<Self>, token: Arc<CancelToken>) -> Result<CullReport, DsError> {
        let me = self.clone();
        run_job(token, move |_| {
            let mut frames = me.frames.lock().unwrap_or_else(PoisonError::into_inner);
            // Frames arrive in completion order (concurrent Vision, batches); the report
            // must not depend on it: same photos, same groups and scores.
            frames.sort_by_key(|f| (f.capture_ms, f.seq, f.id));
            Ok(cull(&frames))
        })
        .await
    }
}

fn cull(frames: &[Frame]) -> CullReport {
    let items: Vec<GroupItem> = frames
        .iter()
        .map(|f| GroupItem {
            capture_ms: f.capture_ms,
            seq: f.seq,
            camera: f.camera,
            hash: f.analysis.hash,
        })
        .collect();
    let groups = lt_ai::group_images(&items, &GroupConfig::default(), None);
    let (w, t) = (CullWeights::default(), CullThresholds::default());
    let mut results: Vec<Option<CullResultRecord>> = vec![None; frames.len()];
    for (gi, g) in groups.iter().enumerate() {
        let analyses: Vec<ImageAnalysis> = g
            .members
            .iter()
            .map(|&i| frames[i].analysis.clone())
            .collect();
        let vision: Vec<Option<VisionSignals>> = g
            .members
            .iter()
            .map(|&i| frames[i].vision.clone())
            .collect();
        let scores = lt_ai::score_group(&analyses, &vision, &w, &t);
        let pick = lt_ai::suggest_picks(&scores, &t);
        let best_name = pick.best.and_then(|b| frames[g.members[b]].name.as_deref());
        for (k, (&i, s)) in g.members.iter().zip(&scores).enumerate() {
            let why = lt_ai::explain_reject(&scores, k, pick.best, best_name, &t);
            let suggestion = if pick.best == Some(k) {
                PickSuggestionKind::Best
            } else if pick.rejects.contains(&k) {
                PickSuggestionKind::Reject
            } else {
                PickSuggestionKind::Keep
            };
            results[i] = Some(CullResultRecord {
                id: frames[i].id,
                score: s.score,
                sharpness: s.sharpness,
                exposure: s.exposure,
                noise: s.noise,
                reasons: reasons(s),
                group: gi as u32,
                suggestion,
                reject_reasons: reject_reasons(&why),
                reject_note: lt_ai::reject_note(&why),
            });
        }
    }
    CullReport {
        // group_images covers every index exactly once.
        results: results.into_iter().flatten().collect(),
        groups: groups
            .iter()
            .map(|g| GroupRecord {
                kind: g.kind.into(),
                member_ids: g.members.iter().map(|&i| frames[i].id).collect(),
            })
            .collect(),
    }
}

// ------------------------------------------------------------ free functions ---

/// Perceptual hash of an RGBA preview or thumbnail.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn perceptual_hash(width: u32, height: u32, rgba: Vec<u8>) -> Result<PHashRecord, DsError> {
    guard(|| Ok(PerceptualHash::of(&luma(width, height, &rgba)?).into()))
}

/// Group frames into bursts/moments from metadata (and hashes when known).
///
/// Without a hash for every item there is no visual evidence, so only burst
/// timing links frames: the look-alike checks are made permissive and
/// near-duplicate linking (which needs hashes) is switched off.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn group_burst_items(items: Vec<BurstItem>) -> Result<Vec<GroupRecord>, DsError> {
    guard(|| Ok(group_bursts(&items)))
}

fn group_bursts(items: &[BurstItem]) -> Vec<GroupRecord> {
    let hashed = items.iter().all(|i| i.hash.is_some());
    let mut cfg = GroupConfig::default();
    if !hashed {
        cfg.burst_max_hash_distance = 1.0;
        cfg.moment_max_hash_distance = 1.0;
        cfg.near_dup_max_hash_distance = -1.0;
    }
    let gi: Vec<GroupItem> = items
        .iter()
        .map(|i| GroupItem {
            capture_ms: i.capture_ms,
            seq: i.seq,
            camera: camera_key(i.camera.as_deref()),
            hash: i.hash.map(Into::into).unwrap_or_default(),
        })
        .collect();
    lt_ai::group_images(&gi, &cfg, None)
        .into_iter()
        .map(|g| GroupRecord {
            kind: g.kind.into(),
            member_ids: g.members.iter().map(|&i| items[i].id).collect(),
        })
        .collect()
}

/// Parse an editing request with the offline rule parser (the fallback when
/// Apple's on-device model is unavailable). Never fails: an empty intent with
/// the text in `unrecognized` means nothing was understood.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn parse_edit_intent(text: String) -> Result<EditIntentRecord, DsError> {
    guard(|| {
        let report = RuleBasedParser.parse_detailed(&text);
        Ok(EditIntentRecord::from_intent(
            &report.intent,
            report.unrecognized,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;

    /// A textured RGBA frame; `blur` > 0 box-blurs it (a soft frame).
    fn frame(w: u32, h: u32, seed: u32, blur: usize) -> Vec<u8> {
        let (wu, hu) = (w as usize, h as usize);
        let mut l = vec![0u8; wu * hu];
        for y in 0..hu {
            for x in 0..wu {
                let v = ((x / 4 + y / 4 + seed as usize) % 2) * 200 + 20;
                l[y * wu + x] = v as u8;
            }
        }
        for _ in 0..blur {
            let src = l.clone();
            for y in 1..hu - 1 {
                for x in 1..wu - 1 {
                    let mut s = 0u32;
                    for dy in 0..3 {
                        for dx in 0..3 {
                            s += u32::from(src[(y + dy - 1) * wu + x + dx - 1]);
                        }
                    }
                    l[y * wu + x] = (s / 9) as u8;
                }
            }
        }
        l.iter().flat_map(|&v| [v, v, v, 255]).collect()
    }

    fn input(id: i64, ms: i64, seq: u64, blur: usize) -> CullInput {
        CullInput {
            id,
            width: 192,
            height: 128,
            rgba: frame(192, 128, 0, blur),
            capture_ms: Some(ms),
            seq: Some(seq),
            camera: Some("SONY ILCE-9 123".into()),
            vision: None,
            name: Some(format!("DSC{id:05}.ARW")),
        }
    }

    /// The report doesn't depend on the order frames were added in.
    #[test]
    fn report_is_independent_of_arrival_order() {
        let frames = || {
            vec![
                input(1, 1_000, 10, 0),
                input(2, 1_000, 11, 6),
                input(3, 1_000, 12, 0),
                input(4, 1_000, 13, 3),
            ]
        };
        let forward = CullSession::new();
        for i in frames() {
            block_on(forward.clone().add(i, CancelToken::new())).unwrap();
        }
        let backward = CullSession::new();
        for i in frames().into_iter().rev() {
            block_on(backward.clone().add(i, CancelToken::new())).unwrap();
        }
        let a = block_on(forward.finish(CancelToken::new())).unwrap();
        let b = block_on(backward.finish(CancelToken::new())).unwrap();
        assert_eq!(a, b);
    }

    /// `add_batch` (the process-wide GPU/CPU analyzer) gives the same summaries and
    /// the same cull report as one `add` per frame, and honours cancellation.
    #[test]
    fn add_batch_matches_single_adds() {
        let inputs = || {
            vec![
                input(1, 1_000, 10, 0),
                input(2, 1_050, 11, 6),
                input(3, 1_100, 12, 0),
                input(4, 9_000, 1, 2),
            ]
        };
        let one = CullSession::new();
        let singles: Vec<_> = inputs()
            .into_iter()
            .map(|i| block_on(one.clone().add(i, CancelToken::new())).unwrap())
            .collect();
        let batch = CullSession::new();
        let summaries = block_on(batch.clone().add_batch(inputs(), CancelToken::new())).unwrap();
        assert_eq!(summaries, singles);
        assert_eq!(batch.count().unwrap(), 4);
        let a = block_on(one.finish(CancelToken::new())).unwrap();
        let b = block_on(batch.clone().finish(CancelToken::new())).unwrap();
        assert_eq!(a, b);
        // Cancelled up front: nothing analysed; a bad input fails the whole batch.
        let t = CancelToken::new();
        t.cancel();
        let s = CullSession::new();
        assert!(matches!(
            block_on(s.clone().add_batch(inputs(), t)),
            Err(DsError::Cancelled)
        ));
        let mut bad = input(9, 0, 0, 0);
        bad.width = 7;
        assert!(block_on(s.clone().add_batch(vec![bad], CancelToken::new())).is_err());
        assert_eq!(s.count().unwrap(), 0);
        assert_eq!(
            block_on(s.clone().add_batch(Vec::new(), CancelToken::new()))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn burst_is_grouped_and_the_soft_frame_ranks_below_the_sharp_one() {
        let s = CullSession::new();
        for inp in [
            input(1, 1_000, 10, 0),
            input(2, 1_050, 11, 6),
            input(3, 1_100, 12, 0),
        ] {
            let sum = block_on(s.clone().add(inp, CancelToken::new())).unwrap();
            assert!(sum.focus.is_finite() && !sum.has_subject);
        }
        assert_eq!(s.count().unwrap(), 3);
        assert!(format!("{s:?}").contains("frames: 3"));
        let r = block_on(s.clone().finish(CancelToken::new())).unwrap();
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.groups[0].kind, GroupKindRecord::Burst);
        assert_eq!(r.groups[0].member_ids, vec![1, 2, 3]);
        let by = |id| r.results.iter().find(|x| x.id == id).unwrap();
        assert!(
            by(1).score > by(2).score,
            "{} vs {}",
            by(1).score,
            by(2).score
        );
        assert!(
            r.results
                .iter()
                .all(|x| (0.0..=1.0).contains(&x.score) && x.group == 0)
        );
        assert_eq!(
            r.results
                .iter()
                .filter(|x| x.suggestion == PickSuggestionKind::Best)
                .count(),
            1
        );
        assert_ne!(by(2).suggestion, PickSuggestionKind::Best);
        assert!(
            by(2)
                .reasons
                .iter()
                .any(|x| x.negative && !x.text.is_empty())
        );
        // The soft frame's reject note: its sharpness failure first, named
        // against the group's best frame, kinds and text in the same order.
        let soft = by(2);
        let best = r
            .results
            .iter()
            .find(|x| x.suggestion == PickSuggestionKind::Best)
            .unwrap();
        assert!(
            matches!(
                soft.reject_reasons[0].kind.as_str(),
                "out_of_focus" | "motion_blur" | "softer_than_best"
            ),
            "{:?}",
            soft.reject_reasons
        );
        let texts: Vec<_> = soft
            .reject_reasons
            .iter()
            .map(|x| x.text.as_str())
            .collect();
        assert_eq!(soft.reject_note, texts.join("; "));
        assert!(
            soft.reject_note.contains(&format!("{:.2}", soft.sharpness)),
            "{}",
            soft.reject_note
        );
        if soft.score < best.score - CullThresholds::default().reject_margin {
            assert!(
                soft.reject_note
                    .contains(&format!("Duplicate of DSC{:05}.ARW", best.id)),
                "{}",
                soft.reject_note
            );
        }
    }

    #[test]
    fn distinct_moments_and_cameras_stay_apart() {
        let s = CullSession::new();
        let mut far = input(2, 90_000, 20, 0);
        far.rgba = frame(192, 128, 3, 0);
        let mut other_cam = input(3, 1_000, 11, 0);
        other_cam.camera = Some("CANON R5".into());
        for inp in [input(1, 1_000, 10, 0), far, other_cam] {
            block_on(s.clone().add(inp, CancelToken::new())).unwrap();
        }
        let r = block_on(s.finish(CancelToken::new())).unwrap();
        assert_eq!(r.groups.len(), 3);
        assert!(r.groups.iter().all(|g| g.kind == GroupKindRecord::Single));
        assert!(
            r.results
                .iter()
                .all(|x| x.suggestion == PickSuggestionKind::Best)
        );
    }

    #[test]
    fn vision_signals_reach_the_score() {
        let face = FaceSignalRecord {
            eyes_open: Some(0.02),
            capture_quality: Some(0.8),
            area: Some(0.2),
            confidence: Some(0.99),
            bounds: Some(NormRect {
                x: 0.3,
                y: 0.2,
                w: 0.4,
                h: 0.5,
            }),
        };
        let vision = VisionSignalsRecord {
            faces: vec![face],
            aesthetics: Some(-0.9),
            is_utility: Some(true),
            af_point: Some(NormPoint { x: 0.5, y: 0.4 }),
            saliency: vec![NormRect {
                x: 0.2,
                y: 0.2,
                w: 0.5,
                h: 0.5,
            }],
        };
        let s = CullSession::new();
        let mut a = input(1, 1_000, 1, 0);
        a.vision = Some(vision);
        let sum = block_on(s.clone().add(a, CancelToken::new())).unwrap();
        assert!(sum.has_subject, "AF point / face become the subject region");
        let r = block_on(s.finish(CancelToken::new())).unwrap();
        let kinds: Vec<_> = r.results[0]
            .reasons
            .iter()
            .map(|x| x.kind.as_str())
            .collect();
        assert!(kinds.contains(&"utility_image"), "{kinds:?}");
        assert!(kinds.contains(&"low_aesthetics"), "{kinds:?}");
        assert!(kinds.contains(&"eyes_closed"), "{kinds:?}");
        // A lone frame: its own hard failure leads the note.
        let why: Vec<_> = r.results[0]
            .reject_reasons
            .iter()
            .map(|x| (x.kind.as_str(), x.hard))
            .collect();
        assert_eq!(why.first(), Some(&("eyes_closed", true)), "{why:?}");
        assert!(why.contains(&("utility_image", false)), "{why:?}");
        assert!(r.results[0].reject_note.starts_with("Eyes closed"));
    }

    #[test]
    fn bad_buffers_and_cancellation_error_cleanly() {
        let s = CullSession::new();
        let mut bad = input(1, 0, 0, 0);
        bad.rgba.truncate(10);
        assert!(matches!(
            block_on(s.clone().add(bad, CancelToken::new())),
            Err(DsError::InvalidArgument { .. })
        ));
        let t = CancelToken::new();
        t.cancel();
        assert_eq!(
            block_on(s.clone().add(input(2, 0, 0, 0), t)),
            Err(DsError::Cancelled)
        );
        assert_eq!(s.count().unwrap(), 0);
        let empty = block_on(s.finish(CancelToken::new())).unwrap();
        assert!(empty.results.is_empty() && empty.groups.is_empty());
        assert!(matches!(
            perceptual_hash(4, 4, vec![0; 3]),
            Err(DsError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn hashes_round_trip_and_differ_for_different_images() {
        let a = perceptual_hash(192, 128, frame(192, 128, 0, 0)).unwrap();
        let b = perceptual_hash(192, 128, frame(192, 128, 0, 0)).unwrap();
        let c = perceptual_hash(192, 128, frame(192, 128, 2, 3)).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        let back: PerceptualHash = a.into();
        assert_eq!(PHashRecord::from(back), a);
    }

    fn item(id: i64, ms: i64, seq: u64) -> BurstItem {
        BurstItem {
            id,
            capture_ms: Some(ms),
            seq: Some(seq),
            camera: Some("A9".into()),
            hash: None,
        }
    }

    #[test]
    fn metadata_bursts_use_whole_second_stamps_and_sequence() {
        // A9 ARWs stamp whole seconds: 20 fps frames share a second, ordered by shot number.
        let items = vec![
            item(3, 5_000, 102),
            item(1, 5_000, 100),
            item(2, 5_000, 101),
            item(4, 9_000, 200),
            item(5, 9_000, 201),
            item(6, 60_000, 300),
        ];
        let g = group_burst_items(items).unwrap();
        let members: Vec<_> = g.iter().map(|g| g.member_ids.clone()).collect();
        assert_eq!(members, vec![vec![1, 2, 3], vec![4, 5], vec![6]]);
        assert_eq!(g[0].kind, GroupKindRecord::Burst);
        assert_eq!(g[2].kind, GroupKindRecord::Single);
        assert_eq!(group_burst_items(Vec::new()).unwrap().len(), 0);
    }

    #[test]
    fn hashes_when_known_split_unrelated_frames_in_one_second() {
        let h1 = perceptual_hash(192, 128, frame(192, 128, 0, 0)).unwrap();
        let h2 = PHashRecord {
            dhash: !h1.dhash,
            phash: !h1.phash,
        };
        let mut a = item(1, 5_000, 1);
        let mut b = item(2, 5_000, 2);
        a.hash = Some(h1);
        b.hash = Some(h2);
        assert_eq!(group_burst_items(vec![a, b]).unwrap().len(), 2);
    }

    #[test]
    fn parses_edit_requests_offline() {
        let r = parse_edit_intent(
            "much warmer, +0.5 ev, lift the shadows, crop 4:5, black and white".into(),
        )
        .unwrap();
        assert!(r.warmth > 0.0 || r.temperature_shift_k > 0.0, "{r:?}");
        assert!((r.exposure_ev - 0.5).abs() < 1e-6, "{r:?}");
        assert!(r.shadows > 0.0);
        assert_eq!((r.crop_width, r.crop_height), (Some(4), Some(5)));
        assert_eq!(r.black_and_white, Some(true));
        let s = parse_edit_intent("straighten by 3 degrees clockwise".into()).unwrap();
        assert!(s.straighten_degrees.is_some() || s.straighten_auto, "{s:?}");
        let auto = parse_edit_intent("level the horizon, auto white balance".into()).unwrap();
        assert!(auto.straighten_auto && auto.auto_white_balance, "{auto:?}");
        let bands = parse_edit_intent("bluer sky, more saturated greens".into()).unwrap();
        assert!(!bands.band_edits.is_empty(), "{bands:?}");
        assert!(bands.band_edits.iter().all(|b| !b.band.is_empty()));
        let none = parse_edit_intent("zorble the flurbs".into()).unwrap();
        assert!(!none.unrecognized.is_empty(), "{none:?}");
    }

    #[test]
    fn real_sample_previews_analyse_and_group() {
        let samples = lt_testdata::samples_with_ext("arw");
        if samples.is_empty() {
            return;
        }
        let s = CullSession::new();
        let mut burst = Vec::new();
        for (i, p) in samples.iter().enumerate() {
            let file = lt_raw::open(p).unwrap();
            let meta = file.metadata().unwrap();
            let prev = file.preview().unwrap();
            // Decode-free check: hash a synthetic frame sized like the preview.
            let (w, h) = (prev.size.width.min(256), prev.size.height.min(256));
            let inp = CullInput {
                id: i as i64,
                width: w,
                height: h,
                rgba: frame(w, h, i as u32, 0),
                capture_ms: None,
                seq: meta.shot_number.map(u64::from),
                camera: Some(meta.model.clone()),
                vision: None,
                name: None,
            };
            block_on(s.clone().add(inp, CancelToken::new())).unwrap();
            burst.push(BurstItem {
                id: i as i64,
                capture_ms: None,
                seq: meta
                    .shot_number
                    .map(u64::from)
                    .or_else(|| lt_raw::filename_sequence(p)),
                camera: Some(meta.model),
                hash: None,
            });
        }
        let r = block_on(s.finish(CancelToken::new())).unwrap();
        assert_eq!(r.results.len(), samples.len());
        let g = group_burst_items(burst).unwrap();
        assert_eq!(
            g.iter().map(|g| g.member_ids.len()).sum::<usize>(),
            samples.len()
        );
    }
}
