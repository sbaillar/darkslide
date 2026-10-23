//! Combined cull score with explanations, and pick suggestions per group.
//!
//! Each signal becomes a 0–1 component (1 = good); the score is the weighted
//! mean of the components that are **present** (missing Vision signals drop
//! out rather than count as zero), multiplied by `hard_fail_factor` per
//! confirmed hard failure and by `utility_factor` for utility shots.
//!
//! **Judged against the group.** Inside a burst, what matters is how a frame
//! compares with its neighbours, so [`score_group`] derives every reason from
//! the group: sharpness relative to the best frame (same tiles or subject),
//! and robust outliers (median ± max(k·MAD, floor)) for exposure, clipping,
//! noise, directional blur and closed eyes. A white jersey clipped in every
//! frame, a squinting athlete, or a stadium full of parallel lines is then
//! not a defect. A lone frame ([`score`]) only gets loose absolute sanity
//! floors — deliberately: without neighbours there is nothing to be relative
//! to, and a loose floor keeps false rejects (the costly error) rare — and
//! never a hard failure from the motion-blur heuristic.
//!
//! Sharpness verdicts come from [`crate::focus::group_focus`]: a "soft" call
//! is only a hard failure when it is corroborated (subject region, or a
//! consensus of at least three frames); otherwise the group is marked
//! [`Reason::FocusUncertain`] and ranked by each frame's own measure.

use std::fmt;

use serde::{Deserialize, Serialize};

use ds_core::NormRect;

use crate::focus::{GroupFocus, group_focus};
use crate::stats::median;
use crate::{Exposure, ImageAnalysis};

/// Relative importance of each component. Only ratios matter.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CullWeights {
    /// Subject/focus sharpness (with motion-blur penalty). The dominant
    /// signal for sports bursts.
    pub sharpness: f32,
    /// Clipping and brightness.
    pub exposure: f32,
    /// Noise level (only when a flat area was available to measure it).
    pub noise: f32,
    /// Eyes-open probability of the subject's face (Vision).
    pub eyes: f32,
    /// Subject face capture quality (Vision `VNDetectFaceCaptureQualityRequest`).
    pub face_quality: f32,
    /// Vision aesthetics `overallScore`, mapped from -1..1 to 0..1.
    pub aesthetics: f32,
    /// In a group, the share of the sharpness component that is relative to
    /// the group's best frame (the rest is the absolute scale). 0..=1.
    pub group_relative: f32,
    /// Multiplier per confirmed hard failure.
    pub hard_fail_factor: f32,
    /// Multiplier for Vision "utility image" shots.
    pub utility_factor: f32,
}

impl Default for CullWeights {
    fn default() -> Self {
        Self {
            sharpness: 0.45,
            exposure: 0.20,
            noise: 0.10,
            eyes: 0.10,
            face_quality: 0.05,
            aesthetics: 0.10,
            group_relative: 0.8,
            hard_fail_factor: 0.6,
            utility_factor: 0.4,
        }
    }
}

/// Group-relative limits first. The lone-frame floors are deliberately
/// loose (see the module docs): tuned so that only clearly blurred or
/// clipped frames are flagged without a group to compare with.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CullThresholds {
    /// Outlier rule: a value is an outlier when it is further than
    /// `max(outlier_k · 1.4826 · MAD, floor)` from the group median.
    pub outlier_k: f32,
    /// Relative sharpness below this → [`Reason::SofterThanGroup`].
    pub softer_than_group: f32,
    /// Relative sharpness below this (and an outlier) → hard [`Reason::OutOfFocus`].
    pub group_out_of_focus: f32,
    /// Directional-ratio drop (log floor, e.g. 0.6 = 40 % below the group
    /// median) that, together with a sharpness drop below
    /// `motion_sharpness_drop`, confirms motion blur.
    pub motion_ratio_drop: f32,
    pub motion_sharpness_drop: f32,
    /// Exposure outlier floor, stops of mean luma from the group median.
    pub exposure_stops: f32,
    /// Clipped-fraction excess over the group median that is reported.
    pub clip_excess: f32,
    /// Noise σ ratio over the group median that is reported.
    pub noise_ratio: f32,

    /// Absolute sharpness component: the value where the scale reaches
    /// `1 − 1/e`. Contrast-normalized units (see `sharpness`); on the A9
    /// previews in-focus frames measure 1.3–3.6, the same frames under a
    /// 3×3 box blur 0.25–0.5, 5×5 0.07–0.23, 9×9 0.02–0.11.
    pub sharpness_reference: f32,
    /// Lone frame: absolute component below this → hard [`Reason::OutOfFocus`]
    /// (0.2 ≈ a value of 0.11: only clearly blurred frames).
    pub out_of_focus_floor: f32,
    /// Lone frame: directional ratio below this → unconfirmed motion blur.
    pub motion_floor: f32,
    /// Lone frame: loose absolute exposure floors.
    pub highlights_clipped: f32,
    pub shadows_clipped: f32,
    pub underexposed_mean: f32,
    pub overexposed_mean: f32,
    /// Noise σ (DN): component loses points from `noise_low` to `noise_high`;
    /// a lone frame above `noise_high` is reported.
    pub noise_low: f32,
    pub noise_high: f32,

    /// Eyes-open probability below this counts as closed.
    pub eyes_open: f32,
    /// Faces smaller than this fraction of the frame, or less confident than
    /// `min_face_confidence`, are never the subject face.
    pub min_face_area: f32,
    pub min_face_confidence: f32,
    /// A lone frame's closed eyes are a hard failure only on a face at least
    /// this large; smaller or unsized faces give an unconfirmed reason.
    pub prominent_face_area: f32,
    /// Face capture quality below this → [`Reason::PoorFaceCapture`].
    pub face_quality: f32,
    /// Aesthetics `overallScore` below this → [`Reason::LowAesthetics`].
    pub aesthetics: f32,
    /// In [`suggest_picks`], frames scoring this far below the best are rejected.
    pub reject_margin: f32,
}

impl Default for CullThresholds {
    fn default() -> Self {
        Self {
            outlier_k: 3.0,
            softer_than_group: 0.5,
            group_out_of_focus: 0.3,
            motion_ratio_drop: 0.6,
            motion_sharpness_drop: 0.8,
            exposure_stops: 0.7,
            clip_excess: 0.02,
            noise_ratio: 1.5,
            sharpness_reference: 0.5,
            out_of_focus_floor: 0.2,
            motion_floor: 0.05,
            highlights_clipped: 0.05,
            shadows_clipped: 0.25,
            underexposed_mean: 0.06,
            overexposed_mean: 0.92,
            noise_low: 2.0,
            noise_high: 6.0,
            eyes_open: 0.3,
            min_face_area: 0.002,
            min_face_confidence: 0.5,
            prominent_face_area: 0.01,
            face_quality: 0.35,
            aesthetics: -0.3,
            reject_margin: 0.2,
        }
    }
}

/// Per-face Vision results. Every field is optional.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct FaceSignal {
    /// Probability (0..=1) that both eyes are open.
    pub eyes_open: Option<f32>,
    /// `VNFaceObservation.faceCaptureQuality`, 0..=1.
    pub capture_quality: Option<f32>,
    /// Face bounding-box area as a fraction of the frame, 0..=1.
    pub area: Option<f32>,
    /// `VNFaceObservation.confidence`, 0..=1.
    pub confidence: Option<f32>,
    /// Face bounding box (normalized), to find the face under the AF point
    /// or the subject region.
    pub bounds: Option<NormRect>,
}

/// Signals computed by Apple Vision on the Swift side.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct VisionSignals {
    pub faces: Vec<FaceSignal>,
    /// `VNImageAestheticsScoresObservation.overallScore`, -1..=1.
    pub aesthetics: Option<f32>,
    /// `VNImageAestheticsScoresObservation.isUtility`.
    pub is_utility: Option<bool>,
    /// Camera AF point (normalized), from ds-raw `ImageMeta::af_point`: the
    /// face under it is the subject whose eyes count.
    pub af_point: Option<[f32; 2]>,
}

/// Why an image scored the way it did. `Display` gives the UI string.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Reason {
    /// On the subject's face. `confirmed`: a prominent face in a lone frame,
    /// or closed while the rest of the group has them open. Only confirmed
    /// ones are hard.
    EyesClosed {
        confirmed: bool,
    },
    /// No subject region and no consensus on where focus is: the group was
    /// ranked by each frame's own sharpness and nobody was judged soft
    /// unless it is sharp nowhere. Informational.
    FocusUncertain,
    OutOfFocus,
    /// `confirmed`: directional ratio and sharpness both dropped against the
    /// group. Unconfirmed (lone frame) is informational only.
    MotionBlur {
        angle_deg: f32,
        confirmed: bool,
    },
    /// Sharpness relative to the group's sharpest frame, 0..=1.
    SofterThanGroup {
        relative: f32,
    },
    SharpestInGroup,
    HighlightsClipped {
        fraction: f32,
    },
    ShadowsClipped {
        fraction: f32,
    },
    /// `mean`: the frame's mean luma (0..=1, display-referred). `stops`: how
    /// far `log2(mean)` is from the group's median, `None` for a lone frame.
    Underexposed {
        mean: f32,
        stops: Option<f32>,
    },
    Overexposed {
        mean: f32,
        stops: Option<f32>,
    },
    Noisy {
        sigma: f32,
    },
    PoorFaceCapture,
    LowAesthetics,
    UtilityImage,
}

impl Reason {
    /// Hard failures a pick should never have; each costs `hard_fail_factor`.
    pub fn is_hard(&self) -> bool {
        matches!(
            self,
            Self::EyesClosed {
                confirmed: true,
                ..
            } | Self::OutOfFocus
                | Self::MotionBlur {
                    confirmed: true,
                    ..
                }
        )
    }

    pub fn is_negative(&self) -> bool {
        !matches!(self, Self::SharpestInGroup | Self::FocusUncertain)
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pct = |v: f32| (v * 100.0).round();
        match *self {
            Self::EyesClosed { confirmed: true } => f.write_str("eyes closed"),
            Self::EyesClosed { confirmed: false } => f.write_str("eyes may be closed"),
            Self::FocusUncertain => f.write_str("focus uncertain (no subject)"),
            Self::OutOfFocus => f.write_str("out of focus"),
            Self::MotionBlur {
                confirmed: true, ..
            } => f.write_str("motion blur"),
            Self::MotionBlur {
                confirmed: false, ..
            } => f.write_str("possible motion blur"),
            Self::SofterThanGroup { relative } => {
                write!(f, "softer than the best frame ({}%)", pct(relative))
            }
            Self::SharpestInGroup => f.write_str("sharpest in group"),
            Self::HighlightsClipped { fraction } => {
                write!(f, "highlights clipped {}%", pct(fraction))
            }
            Self::ShadowsClipped { fraction } => write!(f, "shadows clipped {}%", pct(fraction)),
            Self::Underexposed { .. } => f.write_str("underexposed"),
            Self::Overexposed { .. } => f.write_str("overexposed"),
            Self::Noisy { sigma } => write!(f, "noisy (σ {sigma:.1})"),
            Self::PoorFaceCapture => f.write_str("poor face capture"),
            Self::LowAesthetics => f.write_str("low aesthetics score"),
            Self::UtilityImage => f.write_str("utility image"),
        }
    }
}

/// A 0–1 cull score, its main components, and the reasons behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CullScore {
    pub score: f32,
    pub sharpness: f32,
    pub exposure: f32,
    /// `None` when noise could not be measured.
    pub noise: Option<f32>,
    pub reasons: Vec<Reason>,
}

impl CullScore {
    pub fn has_hard_failure(&self) -> bool {
        self.reasons.iter().any(Reason::is_hard)
    }
}

/// Sharpness component floor for frames an uncertain group cannot judge.
const UNCERTAIN_SHARPNESS: f32 = 0.85;

/// Finite and clamped into `lo..=hi`, else `None` (bad Vision data is ignored).
fn valid(v: Option<f32>, lo: f32, hi: f32) -> Option<f32> {
    v.filter(|v| v.is_finite()).map(|v| v.clamp(lo, hi))
}

fn smoothstep(lo: f32, hi: f32, v: f32) -> f32 {
    let t = ((v - lo) / (hi - lo).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The median and median absolute deviation of one measurement across a
/// group, computed once and used for every frame.
#[derive(Debug, Clone, Copy)]
struct Robust {
    med: f32,
    mad: f32,
}

impl Robust {
    fn of(values: &[f32]) -> Option<Self> {
        let med = median(values.to_vec())?;
        let mad = median(values.iter().map(|v| (v - med).abs()).collect())?;
        Some(Self { med, mad })
    }
}

/// Signed distance beyond the group's robust band (0 inside it, and with no
/// group data): `x − median` shrunk by `max(k · 1.4826 · MAD, floor)`.
fn outlier(x: f32, group: Option<Robust>, k: f32, floor: f32) -> f32 {
    let Some(Robust { med, mad }) = group else {
        return 0.0;
    };
    let band = (k * 1.4826 * mad).max(floor);
    let d = x - med;
    d.signum() * (d.abs() - band).max(0.0)
}

/// Stops of mean luma, with a floor so black frames stay finite.
fn stops(m: f32) -> f32 {
    m.max(1.0 / 255.0).log2()
}

fn overlap(a: &NormRect, b: &NormRect) -> f32 {
    ((a.x + a.w).min(b.x + b.w) - a.x.max(b.x)).max(0.0)
        * ((a.y + a.h).min(b.y + b.h) - a.y.max(b.y)).max(0.0)
}

/// The face that is the subject: the one under the AF point, else the one
/// overlapping the subject regions most, else the largest. Small or unsure
/// faces (spectators) never qualify.
fn subject_face<'v>(
    v: &'v VisionSignals,
    a: &ImageAnalysis,
    t: &CullThresholds,
) -> Option<&'v FaceSignal> {
    let area = |f: &FaceSignal| {
        valid(f.area, 0.0, 1.0)
            .or_else(|| f.bounds.map(|b| b.w * b.h).filter(|x| x.is_finite()))
            .unwrap_or(0.0)
    };
    let faces: Vec<&FaceSignal> = v
        .faces
        .iter()
        .filter(|f| area(f) >= t.min_face_area)
        .filter(|f| valid(f.confidence, 0.0, 1.0).is_none_or(|c| c >= t.min_face_confidence))
        .collect();
    let by = |key: &dyn Fn(&FaceSignal) -> f32| {
        faces
            .iter()
            .copied()
            .filter(|f| key(f) > 0.0)
            .max_by(|x, y| key(x).total_cmp(&key(y)))
    };
    let under_af = v
        .af_point
        .filter(|p| p.iter().all(|c| c.is_finite()))
        .and_then(|p| {
            by(&|f: &FaceSignal| {
                f.bounds.map_or(0.0, |b| {
                    f32::from(u8::from(
                        (b.x..=b.x + b.w).contains(&p[0]) && (b.y..=b.y + b.h).contains(&p[1]),
                    ))
                })
            })
        });
    let on_subject = || {
        by(&|f: &FaceSignal| {
            f.bounds.map_or(0.0, |b| {
                a.sharpness
                    .regions
                    .iter()
                    .map(|r| overlap(&b, &r.rect))
                    .sum()
            })
        })
    };
    under_af.or_else(on_subject).or_else(|| by(&area))
}

/// What the whole group says, worked out once for all its frames.
struct GroupStats {
    focus_relative: Option<Robust>,
    highlights: Option<Robust>,
    shadows: Option<Robust>,
    exposure: Option<Robust>,
    noise: Option<Robust>,
    /// Median log blur-direction ratio of the group's sharp frames (all the
    /// frames when none is sharp): smeared frames must not drag it down.
    motion_reference: f32,
    /// Median of each sharp frame's own ratio, where it is sharpest.
    own_reference: Option<f32>,
    /// Median eyes-open value over the frames that have a subject face.
    eyes: Option<f32>,
}

impl GroupStats {
    fn new(
        frames: &[ImageAnalysis],
        focus: &GroupFocus,
        subject_eyes: &[Option<f32>],
        t: &CullThresholds,
    ) -> Self {
        let column = |f: &dyn Fn(&ImageAnalysis) -> f32| -> Option<Robust> {
            Robust::of(&frames.iter().map(f).collect::<Vec<_>>())
        };
        let ln_motion = |m: &[f32; 2]| m[0].max(1e-4).ln();
        let sharp: Vec<usize> = (0..focus.motion.len())
            .filter(|&k| focus.own[k] >= t.motion_sharpness_drop)
            .collect();
        let motion_reference = if sharp.is_empty() {
            median(focus.motion.iter().map(ln_motion).collect())
        } else {
            median(sharp.iter().map(|&k| ln_motion(&focus.motion[k])).collect())
        }
        .unwrap_or(0.0);
        let own_reference = median(
            (0..frames.len())
                .filter(|&k| focus.own[k] >= t.motion_sharpness_drop)
                .map(|k| frames[k].sharpness.motion().0.max(1e-4).ln())
                .collect(),
        );
        Self {
            focus_relative: Robust::of(
                &focus
                    .relative
                    .iter()
                    .map(|r| r.max(1e-4).ln())
                    .collect::<Vec<_>>(),
            ),
            highlights: column(&|f| f.exposure.highlights_clipped),
            shadows: column(&|f| f.exposure.shadows_clipped),
            exposure: column(&|f| stops(f.exposure.mean)),
            noise: column(&|f| f.noise.sigma.max(0.1).ln()),
            motion_reference,
            own_reference,
            eyes: median(subject_eyes.iter().flatten().copied().collect()),
        }
    }
}

/// What the group says about one frame (absent for a lone frame).
struct GroupView<'a> {
    i: usize,
    focus: &'a GroupFocus,
    stats: &'a GroupStats,
}

fn sharpness_part(
    a: &ImageAnalysis,
    g: Option<&GroupView<'_>>,
    w: &CullWeights,
    t: &CullThresholds,
    reasons: &mut Vec<Reason>,
) -> f32 {
    let abs = 1.0 - (-a.sharpness_value() / t.sharpness_reference.max(1e-6)).exp();
    let abs = if abs.is_finite() {
        abs.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (ratio, angle) = a.sharpness.motion();
    let Some(g) = g else {
        if abs < t.out_of_focus_floor {
            reasons.push(Reason::OutOfFocus);
        } else if ratio < t.motion_floor {
            reasons.push(Reason::MotionBlur {
                angle_deg: angle,
                confirmed: false,
            });
        }
        return abs;
    };
    let (rel, rels, confident) = (
        g.focus.relative[g.i],
        &g.focus.relative,
        g.focus.confident[g.i],
    );
    if g.focus.uncertain {
        reasons.push(Reason::FocusUncertain);
    }
    let low = outlier(
        rel.max(1e-4).ln(),
        g.stats.focus_relative,
        t.outlier_k,
        (1.0 / t.softer_than_group).ln(),
    ) < 0.0;
    let spread = rels.iter().copied().fold(1.0f32, f32::min) < t.softer_than_group;
    if rel >= 1.0 && spread {
        reasons.push(Reason::SharpestInGroup);
    } else if rel < t.group_out_of_focus && low && confident {
        reasons.push(Reason::OutOfFocus);
    } else if rel < t.softer_than_group {
        reasons.push(Reason::SofterThanGroup { relative: rel });
    }
    let gr = w.group_relative.clamp(0.0, 1.0);
    let mut sharp = if g.focus.uncertain && !confident {
        // Nobody can tell whether this frame missed focus or followed the
        // subject: keep it within reach of the best (ranked by its own
        // sharpness, never rejected for it).
        UNCERTAIN_SHARPNESS + (1.0 - UNCERTAIN_SHARPNESS) * g.focus.own[g.i]
    } else {
        (1.0 - gr) * abs + gr * rel
    };
    // Blur direction on the same tiles in every frame (see `GroupFocus::motion`).
    let [ratio, angle] = g.focus.motion[g.i];
    let reference = g.stats.motion_reference;
    let drop_of = |r: f32, reference: f32| {
        (r.max(1e-4).ln() - reference + (1.0 / t.motion_ratio_drop).ln()).min(0.0)
    };
    let drop = drop_of(ratio, reference);
    // …and where the frame itself is sharpest: a panning shot's sharp subject
    // stays isotropic while its background streaks.
    let own_ref = g.stats.own_reference.unwrap_or(reference);
    let drop = if drop_of(a.sharpness.motion().0, own_ref) < 0.0 {
        drop
    } else {
        0.0
    };
    // Motion blur is its own evidence (a direction, and a drop in the frame's
    // own sharpness): it is confirmed without knowing where focus was when
    // the frame lost most of its sharpness.
    let own = g.focus.own[g.i];
    if drop < 0.0 && own.min(rel) < t.motion_sharpness_drop {
        let confirmed = confident || own < 0.5;
        reasons.push(Reason::MotionBlur {
            angle_deg: angle,
            confirmed,
        });
        sharp *= 0.5 + 0.5 * drop.exp();
    }
    sharp
}

fn exposure_part(
    a: &ImageAnalysis,
    g: Option<&GroupView<'_>>,
    t: &CullThresholds,
    reasons: &mut Vec<Reason>,
) -> f32 {
    let e: &Exposure = &a.exposure;
    let (hi_x, lo_x, ev) = match g {
        Some(g) => (
            outlier(
                e.highlights_clipped,
                g.stats.highlights,
                t.outlier_k,
                t.clip_excess,
            ),
            outlier(
                e.shadows_clipped,
                g.stats.shadows,
                t.outlier_k,
                t.clip_excess,
            ),
            outlier(
                stops(e.mean),
                g.stats.exposure,
                t.outlier_k,
                t.exposure_stops,
            ),
        ),
        None => (
            e.highlights_clipped - t.highlights_clipped,
            e.shadows_clipped - t.shadows_clipped,
            if e.mean < t.underexposed_mean {
                stops(e.mean) - stops(t.underexposed_mean)
            } else {
                (stops(e.mean) - stops(t.overexposed_mean)).max(0.0)
            },
        ),
    };
    if hi_x > 0.0 {
        reasons.push(Reason::HighlightsClipped {
            fraction: e.highlights_clipped,
        });
    }
    if lo_x > 0.0 {
        reasons.push(Reason::ShadowsClipped {
            fraction: e.shadows_clipped,
        });
    }
    let deviation = g
        .and_then(|g| g.stats.exposure)
        .map(|r| stops(e.mean) - r.med);
    if ev < 0.0 {
        reasons.push(Reason::Underexposed {
            mean: e.mean,
            stops: deviation,
        });
    } else if ev > 0.0 {
        reasons.push(Reason::Overexposed {
            mean: e.mean,
            stops: deviation,
        });
    }
    (1.0 - 6.0 * hi_x.max(0.0) - 2.0 * lo_x.max(0.0) - 0.4 * ev.abs()).clamp(0.0, 1.0)
}

fn noise_part(
    a: &ImageAnalysis,
    g: Option<&GroupView<'_>>,
    t: &CullThresholds,
    reasons: &mut Vec<Reason>,
) -> Option<f32> {
    if a.noise.sampled_fraction <= 0.0 {
        return None;
    }
    let sigma = a.noise.sigma;
    let noisy = match g {
        Some(g) => {
            sigma > t.noise_low
                && outlier(
                    sigma.max(0.1).ln(),
                    g.stats.noise,
                    t.outlier_k,
                    t.noise_ratio.ln(),
                ) > 0.0
        }
        None => sigma > t.noise_high,
    };
    if noisy {
        reasons.push(Reason::Noisy { sigma });
    }
    Some(1.0 - smoothstep(t.noise_low, t.noise_high, sigma))
}

/// Adds the Vision components to `parts`; returns true for a utility shot.
fn vision_parts(
    v: &VisionSignals,
    a: &ImageAnalysis,
    g: Option<&GroupView<'_>>,
    w: &CullWeights,
    t: &CullThresholds,
    parts: &mut Vec<(f32, f32)>,
    reasons: &mut Vec<Reason>,
) -> bool {
    if let Some(face) = subject_face(v, a, t) {
        if let Some(open) = valid(face.eyes_open, 0.0, 1.0) {
            if open < t.eyes_open {
                let confirmed = match g {
                    // Closed while most of the group has them open.
                    Some(g) => g.stats.eyes.is_some_and(|m| m >= t.eyes_open),
                    None => valid(face.area, 0.0, 1.0)
                        .or_else(|| face.bounds.map(|b| b.w * b.h))
                        .is_some_and(|x| x >= t.prominent_face_area),
                };
                reasons.push(Reason::EyesClosed { confirmed });
            }
            parts.push((w.eyes, open));
        }
        if let Some(q) = valid(face.capture_quality, 0.0, 1.0) {
            if q < t.face_quality {
                reasons.push(Reason::PoorFaceCapture);
            }
            parts.push((w.face_quality, q));
        }
    }
    if let Some(s) = valid(v.aesthetics, -1.0, 1.0) {
        if s < t.aesthetics {
            reasons.push(Reason::LowAesthetics);
        }
        parts.push((w.aesthetics, f32::midpoint(s, 1.0)));
    }
    let utility = v.is_utility == Some(true);
    if utility {
        reasons.push(Reason::UtilityImage);
    }
    utility
}

fn score_one(
    a: &ImageAnalysis,
    vision: Option<&VisionSignals>,
    g: Option<&GroupView<'_>>,
    w: &CullWeights,
    t: &CullThresholds,
) -> CullScore {
    let mut reasons = Vec::new();
    let sharp = sharpness_part(a, g, w, t, &mut reasons);
    let exposure = exposure_part(a, g, t, &mut reasons);
    let noise = noise_part(a, g, t, &mut reasons);
    let mut parts = vec![(w.sharpness, sharp), (w.exposure, exposure)];
    parts.extend(noise.map(|n| (w.noise, n)));
    let utility = vision.is_some_and(|v| vision_parts(v, a, g, w, t, &mut parts, &mut reasons));

    let (num, den) = parts
        .iter()
        .filter(|(wt, _)| wt.is_finite() && *wt > 0.0)
        .fold((0.0f32, 0.0f32), |(n, d), &(wt, v)| (n + wt * v, d + wt));
    let mut total = if den > 0.0 { num / den } else { 0.0 };
    for _ in reasons.iter().filter(|r| r.is_hard()) {
        total *= w.hard_fail_factor;
    }
    if utility {
        total *= w.utility_factor;
    }
    let score = if total.is_finite() {
        total.clamp(0.0, 1.0)
    } else {
        0.0
    };
    CullScore {
        score,
        sharpness: sharp,
        exposure,
        noise,
        reasons,
    }
}

/// Score a lone frame: absolute sanity floors only.
pub fn score(
    a: &ImageAnalysis,
    vision: Option<&VisionSignals>,
    w: &CullWeights,
    t: &CullThresholds,
) -> CullScore {
    score_one(a, vision, None, w, t)
}

/// Score the frames of one group (burst / moment) against each other.
/// `vision[i]` belongs to `frames[i]`; a shorter slice means "no Vision data"
/// for the rest. A one-frame group is scored like [`score`].
pub fn score_group(
    frames: &[ImageAnalysis],
    vision: &[Option<VisionSignals>],
    w: &CullWeights,
    t: &CullThresholds,
) -> Vec<CullScore> {
    let vis = |i: usize| vision.get(i).and_then(Option::as_ref);
    if frames.len() < 2 {
        return frames
            .iter()
            .enumerate()
            .map(|(i, a)| score(a, vis(i), w, t))
            .collect();
    }
    let refs: Vec<_> = frames.iter().map(|f| &f.sharpness).collect();
    let focus = group_focus(&refs);
    let subject_eyes: Vec<Option<f32>> = (0..frames.len())
        .map(|i| {
            vis(i)
                .and_then(|v| subject_face(v, &frames[i], t))
                .and_then(|f| valid(f.eyes_open, 0.0, 1.0))
        })
        .collect();
    let stats = GroupStats::new(frames, &focus, &subject_eyes, t);
    (0..frames.len())
        .map(|i| {
            let g = GroupView {
                i,
                focus: &focus,
                stats: &stats,
            };
            score_one(&frames[i], vis(i), Some(&g), w, t)
        })
        .collect()
}

/// Suggested outcome for a group.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PickSuggestion {
    /// Highest score (lowest index on ties); `None` only for an empty group.
    pub best: Option<usize>,
    /// Frames more than `reject_margin` below the best, or with a hard
    /// failure. Never contains `best`. Ascending.
    pub rejects: Vec<usize>,
}

pub fn suggest_picks(scores: &[CullScore], t: &CullThresholds) -> PickSuggestion {
    let key = |s: &CullScore| {
        if s.score.is_finite() {
            s.score
        } else {
            f32::NEG_INFINITY
        }
    };
    let Some(best) = (0..scores.len()).reduce(|b, i| {
        if key(&scores[i]) > key(&scores[b]) {
            i
        } else {
            b
        }
    }) else {
        return PickSuggestion::default();
    };
    let floor = key(&scores[best]) - t.reject_margin;
    let rejects = (0..scores.len())
        .filter(|&i| i != best && (key(&scores[i]) < floor || scores[i].has_hard_failure()))
        .collect();
    PickSuggestion {
        best: Some(best),
        rejects,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{blur, img, scale, texture};
    use crate::{AnalysisConfig, LumaImage, analyze};

    fn run(i: &LumaImage) -> ImageAnalysis {
        analyze(i, &AnalysisConfig::default())
    }

    fn lone(a: &ImageAnalysis, v: Option<&VisionSignals>) -> CullScore {
        score(a, v, &CullWeights::default(), &CullThresholds::default())
    }

    fn group(frames: &[ImageAnalysis], v: &[Option<VisionSignals>]) -> Vec<CullScore> {
        score_group(
            frames,
            v,
            &CullWeights::default(),
            &CullThresholds::default(),
        )
    }

    fn texts(s: &CullScore) -> Vec<String> {
        s.reasons.iter().map(ToString::to_string).collect()
    }

    fn scene(seed: u64) -> LumaImage {
        texture(320, 240, seed)
    }

    fn face(eyes: f32, area: f32) -> FaceSignal {
        FaceSignal {
            eyes_open: Some(eyes),
            capture_quality: Some(0.8),
            area: Some(area),
            confidence: Some(0.9),
            bounds: None,
        }
    }

    fn at(f: FaceSignal, x: f32, y: f32, side: f32) -> FaceSignal {
        FaceSignal {
            bounds: Some(NormRect {
                x,
                y,
                w: side,
                h: side,
            }),
            area: Some(side * side),
            ..f
        }
    }

    #[test]
    fn robust_band_is_computed_once_from_the_middle_two() {
        // Even count: median 2.5 (not 3), MAD 1.0.
        let r = Robust::of(&[4.0, 1.0, 3.0, 2.0]).expect("non-empty");
        assert_eq!((r.med, r.mad), (2.5, 1.0));
        // Beyond the band 3 · 1.4826 · 1.0 = 4.4478 from the median.
        let d = outlier(10.0, Some(r), 3.0, 0.1);
        assert!((d - (7.5 - 4.4478)).abs() < 1e-4, "{d}");
        assert_eq!(outlier(-10.0, Some(r), 3.0, 0.1).signum(), -1.0);
        // Inside the band, and with no group data: no outlier.
        assert_eq!(outlier(3.0, Some(r), 3.0, 0.1), 0.0);
        assert_eq!(outlier(99.0, None, 3.0, 0.1), 0.0);
        assert!(Robust::of(&[]).is_none());
    }

    #[test]
    fn a_large_group_scores_every_frame_the_same_way() {
        // 1 200 identical frames: the group statistics are shared, so every
        // frame gets the same verdict, and none is an outlier of its group.
        let a = run(&texture(96, 96, 3));
        let frames = vec![a; 1_200];
        let scores = group(&frames, &[]);
        assert_eq!(scores.len(), 1_200);
        assert!(scores.iter().all(|s| s == &scores[0]), "{:?}", scores[0]);
        assert!(!scores[0].has_hard_failure());
    }

    #[test]
    fn lone_frames_use_loose_floors() {
        let s = lone(&run(&scene(1)), None);
        assert!(s.score > 0.8 && s.reasons.is_empty(), "{s:?}");
        let soft = lone(&run(&blur(&scene(1), 4, 4)), None);
        assert!(
            soft.reasons.contains(&Reason::OutOfFocus) && soft.score < s.score,
            "{soft:?}"
        );
        let sc = scene(1);
        let blown = lone(
            &run(&img(320, 240, |x, y| {
                sc.pixels()[y * 320 + x].saturating_add(140)
            })),
            None,
        );
        assert!(
            texts(&blown)
                .iter()
                .any(|t| t.starts_with("highlights clipped")),
            "{blown:?}"
        );
        let dark = lone(&run(&scale(&scene(1), 0.05)), None);
        assert!(
            dark.reasons
                .iter()
                .any(|r| matches!(r, Reason::Underexposed { stops: None, .. })),
            "{dark:?}"
        );
        let bright = lone(&run(&img(320, 240, |_, y| (240 + y / 60) as u8)), None);
        assert!(
            bright
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::Overexposed { stops: None, .. })),
            "{bright:?}"
        );
    }

    #[test]
    fn line_heavy_lone_frame_is_never_a_hard_motion_failure() {
        // Bleachers — strong horizontal lines, perfectly sharp.
        let bleachers = img(320, 240, |x, y| {
            if (y / 3) % 2 == 0 {
                60 + (x % 7) as u8
            } else {
                190
            }
        });
        let s = lone(&run(&bleachers), None);
        assert!(!s.has_hard_failure(), "{s:?}");
    }

    #[test]
    fn burst_rejects_soft_and_smeared_but_not_darker_frames() {
        let base = scene(2);
        let frames = [
            run(&base),
            run(&blur(&base, 2, 2)),
            run(&scene(2)),
            run(&blur(&base, 5, 0)),
            run(&scale(&base, 0.25)), // −2 EV
        ];
        let s = group(&frames, &[]);
        let picks = suggest_picks(&s, &CullThresholds::default());
        assert!(matches!(picks.best, Some(0 | 2 | 4)), "{picks:?}");
        assert!(
            picks.rejects.contains(&1) && picks.rejects.contains(&3),
            "{picks:?} {s:?}"
        );
        assert!(
            s[3].reasons.iter().any(|r| matches!(
                r,
                Reason::MotionBlur {
                    confirmed: true,
                    ..
                }
            )),
            "{s:?}"
        );
        assert!(s[4].sharpness > 0.85 * s[0].sharpness, "{s:?}");
        assert!(
            s[4].reasons
                .iter()
                .any(|r| matches!(r, Reason::Underexposed { stops: Some(x), .. } if *x < -1.0))
        );
        assert!(
            !s[4]
                .reasons
                .iter()
                .any(|r| matches!(r, Reason::OutOfFocus | Reason::SofterThanGroup { .. }))
        );
    }

    #[test]
    fn consistent_anisotropy_and_clipping_are_not_defects_in_a_group() {
        let lines = |k: usize| {
            img(320, 240, |x, y| {
                if ((y + k) / 3).is_multiple_of(2) {
                    60 + (x % 7) as u8
                } else {
                    255
                }
            })
        };
        let frames: Vec<_> = (0..4).map(|k| run(&lines(k))).collect();
        for s in group(&frames, &[]) {
            assert!(
                !s.reasons.iter().any(|r| matches!(
                    r,
                    Reason::MotionBlur { .. } | Reason::HighlightsClipped { .. }
                )),
                "{s:?}"
            );
        }
    }

    #[test]
    fn group_outliers_in_clipping_and_noise_are_reported() {
        let base = scene(3);
        let noisy = {
            let mut s = 7u64;
            img(320, 240, |x, y| {
                s = s
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let n = ((s >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 40.0;
                (f32::from(base.pixels()[y * 320 + x]) + n).clamp(0.0, 255.0) as u8
            })
        };
        let blown = img(320, 240, |x, y| {
            base.pixels()[y * 320 + x].saturating_add(120)
        });
        let frames = [run(&base), run(&base), run(&noisy), run(&blown)];
        let s = group(&frames, &[]);
        assert!(
            s[2].reasons
                .iter()
                .any(|r| matches!(r, Reason::Noisy { .. })),
            "{:?}",
            s[2]
        );
        assert!(
            s[3].reasons
                .iter()
                .any(|r| matches!(r, Reason::HighlightsClipped { .. })),
            "{:?}",
            s[3]
        );
        assert!(s[0].reasons.iter().all(|r| !r.is_negative()), "{:?}", s[0]);
    }

    #[test]
    fn eyes_are_judged_on_the_subject_face_and_against_the_group() {
        let a = run(&scene(4));
        let frames = vec![a.clone(), a.clone(), a.clone()];
        let v = |e: f32| {
            Some(VisionSignals {
                faces: vec![face(e, 0.05)],
                ..VisionSignals::default()
            })
        };
        // One blink in a burst of open eyes → confirmed and rejected.
        let s = group(&frames, &[v(0.95), v(0.05), v(0.9)]);
        assert_eq!(texts(&s[1]), ["eyes closed"]);
        assert_eq!(
            suggest_picks(&s, &CullThresholds::default()).rejects,
            vec![1]
        );
        // Squinting in every frame → noted, never hard.
        let s = group(&frames, &[v(0.1), v(0.05), v(0.2)]);
        assert!(s.iter().all(|s| !s.has_hard_failure()) && texts(&s[0]) == ["eyes may be closed"]);
        // A spectator blinking behind an open-eyed subject is
        // not the subject — nothing is reported, nobody is rejected.
        let spect = |e: f32| {
            Some(VisionSignals {
                faces: vec![face(0.95, 0.06), face(e, 0.004)],
                ..VisionSignals::default()
            })
        };
        let s = group(&frames, &[spect(0.9), spect(0.05), spect(0.9)]);
        assert!(s.iter().all(|s| s.reasons.is_empty()), "{s:?}");
        assert_eq!(
            suggest_picks(&s, &CullThresholds::default()).rejects.len(),
            0
        );
        // Tiny or unsure faces are never the subject.
        let crowd = VisionSignals {
            faces: vec![
                face(0.0, 0.0005),
                FaceSignal {
                    confidence: Some(0.1),
                    ..face(0.0, 0.05)
                },
            ],
            ..VisionSignals::default()
        };
        assert_eq!(lone(&a, Some(&crowd)).reasons.len(), 0);
        // Lone frame: hard only for a prominent face.
        assert!(lone(&a, v(0.05).as_ref()).has_hard_failure());
        let small = VisionSignals {
            faces: vec![face(0.05, 0.005)],
            ..VisionSignals::default()
        };
        assert_eq!(texts(&lone(&a, Some(&small))), ["eyes may be closed"]);
    }

    #[test]
    fn the_face_under_the_af_point_or_subject_region_is_the_subject() {
        let a = run(&scene(9));
        // Big face with closed eyes elsewhere; the AF point is on a small open-eyed face.
        let faces = vec![
            at(face(0.02, 0.0), 0.0, 0.0, 0.4),
            at(face(0.95, 0.0), 0.6, 0.6, 0.12),
        ];
        let with_af = VisionSignals {
            faces: faces.clone(),
            af_point: Some([0.65, 0.65]),
            ..VisionSignals::default()
        };
        assert_eq!(lone(&a, Some(&with_af)).reasons.len(), 0);
        let without = VisionSignals {
            faces: faces.clone(),
            ..VisionSignals::default()
        };
        assert!(
            lone(&a, Some(&without)).has_hard_failure(),
            "largest face decides"
        );
        // The analysis' subject region selects the face just like the AF point.
        let roi = [crate::SubjectRegion {
            rect: NormRect {
                x: 0.6,
                y: 0.6,
                w: 0.12,
                h: 0.12,
            },
            weight: 1.0,
        }];
        let b = crate::analyze_with_subject(&scene(9), &AnalysisConfig::default(), &roi);
        assert_eq!(lone(&b, Some(&without)).reasons.len(), 0);
    }

    #[test]
    fn other_vision_signals_and_garbage() {
        let a = run(&scene(5));
        let base = lone(&a, None).score;
        let poor = VisionSignals {
            faces: vec![FaceSignal {
                capture_quality: Some(0.1),
                area: Some(0.05),
                ..FaceSignal::default()
            }],
            aesthetics: Some(-0.9),
            is_utility: Some(true),
            af_point: None,
        };
        let s = lone(&a, Some(&poor));
        assert_eq!(
            texts(&s),
            ["poor face capture", "low aesthetics score", "utility image"]
        );
        assert!(s.score < 0.4 * base + 1e-6);
        let garbage = VisionSignals {
            faces: vec![FaceSignal {
                eyes_open: Some(f32::NAN),
                capture_quality: Some(f32::INFINITY),
                area: Some(f32::NAN),
                confidence: None,
                bounds: Some(NormRect {
                    x: f32::NAN,
                    y: 0.0,
                    w: 0.5,
                    h: 0.5,
                }),
            }],
            aesthetics: Some(f32::NAN),
            is_utility: None,
            af_point: Some([f32::NAN, 0.5]),
        };
        let s = lone(&a, Some(&garbage));
        assert!(
            (s.score - base).abs() < 1e-6 && s.reasons.is_empty(),
            "{s:?}"
        );
    }

    #[test]
    fn weights_that_cancel_out_give_zero() {
        let zero = CullWeights {
            sharpness: 0.0,
            exposure: f32::NAN,
            noise: 0.0,
            ..CullWeights::default()
        };
        assert_eq!(
            score(&run(&scene(6)), None, &zero, &CullThresholds::default()).score,
            0.0
        );
    }

    #[test]
    fn picks_edge_cases() {
        let t = CullThresholds::default();
        assert_eq!(suggest_picks(&[], &t), PickSuggestion::default());
        let mk = |score| CullScore {
            score,
            sharpness: 0.0,
            exposure: 0.0,
            noise: None,
            reasons: vec![],
        };
        let p = suggest_picks(&[mk(f32::NAN), mk(0.5), mk(0.5)], &t);
        assert_eq!((p.best, p.rejects), (Some(1), vec![0]));
        // A lone frame is its own best, even when out of focus.
        let s = score_group(
            &[run(&blur(&scene(7), 5, 5))],
            &[],
            &CullWeights::default(),
            &t,
        );
        assert!(s[0].has_hard_failure());
        assert_eq!(
            suggest_picks(&s, &t),
            PickSuggestion {
                best: Some(0),
                rejects: vec![]
            }
        );
    }
}
