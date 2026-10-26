//! AI Optimize: measured, explained edit proposals.
//!
//! [`analyze`] measures a photo (camera-space block statistics, plus small probe
//! renders with their histograms) and runs deterministic rules over the measurements
//! ([`rules`]). Each rule may propose one [`Change`] — a field's old and new value, a
//! machine-readable [`Reason`] carrying the measurements that drove it, a confidence, and
//! a templated plain-English explanation ([`explain`]) — or record why it left a field
//! alone ([`Kept`]). Values come from the rules, never from a language model, so a
//! proposal is reproducible and every number in its explanation is a measurement.
//!
//! **Deliberate choices are kept.** A field differing from its default is the user's
//! choice unless it equals what an earlier, applied optimize set (`prior`): user values
//! are left alone (and the report says so), except that a user exposure clipping more
//! than 1% of the frame with highlights also set by hand is pulled back. The pulled-back
//! exposure stays the user's (an applied pull-back is remembered as theirs, and the
//! baseline is measured at it), so a rerun keeps it rather than auto-exposing.
//!
//! **Idempotent.** Rules measure a *baseline* render (the proposed white balance and
//! exposure, the optimizer-owned look fields at their defaults, user fields as set) and
//! compute absolute targets from it and from the camera data, so running Optimize again
//! on its own result proposes nothing new.
//!
//! **Cost.** One pass over camera blocks (shared with auto tone; the GPU computes them),
//! two renders at ≤ [`ANALYSIS_LONG_EDGE`] px (`Priority::Tool`, histograms binned in the
//! output pass), and a strided single-threaded scan of the 8-bit pixels.

mod explain;
mod measure;
mod rules;
#[cfg(test)]
mod tests;

use std::time::Instant;

use lt_core::adjust::{AiTarget, Layer, LayerAdjustments, Mask, MaskOp, MaskShape};
use lt_core::{Adjustments, NormRect, RawImage, Size};
use serde::{Deserialize, Serialize};

pub use measure::RenderStats;

use crate::auto::{Histogram, ToneStats, WbEvidence};
use crate::{PipelineError, Priority, Quality, RenderRequest, RenderedImage};

/// Version of the [`OptimizeReport`] layout (stored in sidecars; readers ignore unknown
/// fields and default missing ones).
pub const REPORT_VERSION: u32 = 1;

/// Long edge of the analysis renders.
pub const ANALYSIS_LONG_EDGE: u32 = 1024;

/// Name of the layer the sky suggestion adds (also how a second run recognizes it).
pub const SKY_LAYER_NAME: &str = "Sky (AI Optimize)";

/// What the app's scene analysis (Apple Vision, the sky model) found.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SceneSignals {
    /// Detected faces.
    pub faces: u32,
    /// Their total area as a fraction of the frame.
    pub face_area: f32,
    /// Horizon tilt in degrees, positive = rises to the right (Vision), in the E frame.
    pub horizon_deg: Option<f32>,
    /// The most salient subject, normalized in the E frame (y down).
    pub subject: Option<NormRect>,
    /// Share of the frame that is sky.
    pub sky_fraction: f32,
    /// Sky mask in the E frame (never stored in a report).
    #[serde(skip)]
    pub sky_mask: Option<SkyMask>,
    pub tags: Vec<SceneTag>,
    /// The app has the AI Denoise model.
    pub ai_denoise_available: bool,
}

impl SceneSignals {
    pub fn has(&self, tag: SceneTag) -> bool {
        self.tags.contains(&tag)
    }
}

/// An 8-bit sky mask (≥ 128 = sky), row-major, normalized over the E frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SkyMask {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl SkyMask {
    /// Whether normalized E-frame point `(u, v)` is sky (nearest sample).
    pub(crate) fn is_sky(&self, u: f64, v: f64) -> bool {
        if self.width == 0
            || self.height == 0
            || !(0.0..1.0).contains(&u)
            || !(0.0..1.0).contains(&v)
        {
            return false;
        }
        let x = (u * f64::from(self.width)) as usize;
        let y = (v * f64::from(self.height)) as usize;
        self.data
            .get(y * self.width as usize + x)
            .is_some_and(|m| *m >= 128)
    }
}

/// Scene types the rules adapt to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SceneTag {
    Landscape,
    Portrait,
    Night,
    Snow,
    Beach,
    Sunset,
    Backlit,
}

/// An adjustable setting, in the tools' UI units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    WhiteBalance,
    Exposure,
    Highlights,
    Shadows,
    Levels,
    Contrast,
    Vibrance,
    Saturation,
    Clarity,
    Texture,
    Dehaze,
    AiDenoise,
    NoiseReduction,
    Straighten,
    LensCorrection,
    SkyLayer,
}

impl Field {
    /// Every field, in panel order.
    pub const ALL: [Field; 16] = [
        Self::WhiteBalance,
        Self::Exposure,
        Self::Highlights,
        Self::Shadows,
        Self::Levels,
        Self::Contrast,
        Self::Vibrance,
        Self::Saturation,
        Self::Clarity,
        Self::Texture,
        Self::Dehaze,
        Self::AiDenoise,
        Self::NoiseReduction,
        Self::Straighten,
        Self::LensCorrection,
        Self::SkyLayer,
    ];

    /// The tool's name as the panel shows it.
    pub fn label(self) -> &'static str {
        match self {
            Self::WhiteBalance => "White balance",
            Self::Exposure => "Exposure",
            Self::Highlights => "Highlights",
            Self::Shadows => "Shadows",
            Self::Levels => "Levels",
            Self::Contrast => "Contrast",
            Self::Vibrance => "Vibrance",
            Self::Saturation => "Saturation",
            Self::Clarity => "Clarity",
            Self::Texture => "Texture",
            Self::Dehaze => "Dehaze",
            Self::AiDenoise => "AI Denoise",
            Self::NoiseReduction => "Noise reduction",
            Self::Straighten => "Straighten",
            Self::LensCorrection => "Lens corrections",
            Self::SkyLayer => "Sky layer",
        }
    }
}

/// One field's value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Setting {
    Number {
        value: f32,
    },
    WhiteBalance {
        temp_k: f32,
        tint: f32,
        as_shot: bool,
    },
    Levels {
        black: f32,
        white: f32,
    },
    Toggle {
        on: bool,
    },
    NoiseReduction {
        luminance: f32,
        color: f32,
    },
    /// `crop`: the shrunk crop rectangle when the user's crop isn't the full frame (the
    /// pipeline fits only a full frame inside the turned image by itself).
    Straighten {
        angle_deg: f32,
        #[serde(default)]
        crop: Option<NormRect>,
    },
    SkyLayer {
        present: bool,
        exposure_ev: f32,
        highlights: f32,
    },
}

impl Setting {
    /// Equal within display rounding (the app stores values as doubles).
    pub fn approx_eq(&self, o: &Setting) -> bool {
        let eq = |a: f32, b: f32| (a - b).abs() <= 1e-3 * a.abs().max(b.abs()).max(1.0);
        match (*self, *o) {
            (Self::Number { value: a }, Self::Number { value: b })
            | (Self::Straighten { angle_deg: a, .. }, Self::Straighten { angle_deg: b, .. }) => {
                eq(a, b)
            }
            (
                Self::WhiteBalance {
                    temp_k: a,
                    tint: b,
                    as_shot: c,
                },
                Self::WhiteBalance {
                    temp_k: x,
                    tint: y,
                    as_shot: z,
                },
            ) => c == z && (c || (eq(a, x) && eq(b, y))),
            (Self::Levels { black: a, white: b }, Self::Levels { black: x, white: y }) => {
                eq(a, x) && eq(b, y)
            }
            (Self::Toggle { on: a }, Self::Toggle { on: b }) => a == b,
            (
                Self::NoiseReduction {
                    luminance: a,
                    color: b,
                },
                Self::NoiseReduction {
                    luminance: x,
                    color: y,
                },
            ) => eq(a, x) && eq(b, y),
            (
                Self::SkyLayer {
                    present: a,
                    exposure_ev: b,
                    highlights: c,
                },
                Self::SkyLayer {
                    present: x,
                    exposure_ev: y,
                    highlights: z,
                },
            ) => a == x && (!a || (eq(b, y) && eq(c, z))),
            _ => false,
        }
    }
}

/// Why a change was proposed, with the measurements that drove it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Reason {
    /// The scene's log-average luminance (`midtone`, linear, at the current exposure)
    /// against the `target`; `limited`: held back so the brightest 0.5% stay below the
    /// tone curve's shoulder. `clipped_pct`: a user exposure that clipped this much.
    Exposure {
        midtone: f32,
        target: f32,
        limited: bool,
        scene: Option<SceneTag>,
        clipped_pct: Option<f32>,
    },
    /// Highlights clipping in the current render (percent of the frame, and of the sky).
    HighlightsClipped {
        clipped_pct: f32,
        sky_clipped_pct: Option<f32>,
    },
    /// The brightest 0.5% sit this many stops into the tone curve's shoulder.
    HighlightRange { stops_over: f32 },
    /// Share of the frame near black (luma < 6%), the 2nd percentile's distance below the
    /// toe, and a backlit subject's brightness against the frame's median.
    Shadows {
        dark_pct: f32,
        stops_under: f32,
        backlit: bool,
        subject_luma: Option<f32>,
        median_luma: f32,
    },
    /// White balance: a neutral read at `neutral_k`/`neutral_tint` against the current
    /// setting (the camera's, when at As Shot).
    ColorCast {
        neutral_k: f32,
        neutral_tint: f32,
        current_k: f32,
        current_tint: f32,
        evidence: Evidence,
    },
    /// Darkest (0.1%) and brightest (99.9%) display tones before the stretch.
    TonalRange { darkest: f32, brightest: f32 },
    /// Display luma standard deviation (after the levels stretch) against a target.
    LowContrast { spread: f32, target: f32 },
    /// Mean saturation of non-dark pixels against a target; faces cap the boost.
    Muted {
        mean_saturation: f32,
        target: f32,
        faces: u32,
    },
    /// The 90th percentile saturation is this high.
    Oversaturated {
        p90_saturation: f32,
        mean_saturation: f32,
    },
    /// Mean local contrast (|luma − 3×3 mean|) against a target.
    SoftDetail {
        detail: f32,
        target: f32,
        landscape: bool,
    },
    /// Dark-channel haze score (0 clear … 1 fog), black floor and tonal spread.
    Haze {
        haze: f32,
        darkest: f32,
        spread: f32,
    },
    /// ISO and the measured sensor noise σ (fraction of white).
    Noise {
        iso: Option<u32>,
        sigma: f32,
        ai_available: bool,
    },
    /// Vision's horizon tilt (degrees, positive rises to the right).
    Tilted { horizon_deg: f32 },
    /// Lens corrections are off though a profile exists.
    LensOff { profile: String },
    /// The sky covers `sky_pct` of the frame at mean display brightness `sky_luma`.
    BrightSky {
        sky_pct: f32,
        sky_luma: f32,
        sky_clipped_pct: f32,
    },
}

/// [`WbEvidence`] as stored.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    AsShot,
    NeutralCluster,
    WhitePatch,
    GrayWorld,
}

impl From<WbEvidence> for Evidence {
    fn from(e: WbEvidence) -> Self {
        match e {
            WbEvidence::AsShot => Self::AsShot,
            WbEvidence::NeutralCluster => Self::NeutralCluster,
            WbEvidence::WhitePatch => Self::WhitePatch,
            WbEvidence::GrayWorld { .. } => Self::GrayWorld,
        }
    }
}

/// One proposed change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// Index in [`OptimizeReport::changes`] (what `apply` selects by).
    pub id: u32,
    pub field: Field,
    pub label: String,
    pub old: Setting,
    pub new: Setting,
    /// `old` and `new` as the panel shows them ("+0.45 EV", "5,600 K / +0").
    pub from: String,
    pub to: String,
    pub reason: Reason,
    /// 0..=1: how strongly the measurements support it.
    pub confidence: f32,
    /// Checked when the panel opens (suggestions that override a user choice, or add a
    /// layer, start unchecked).
    pub default_on: bool,
    /// Set by [`mark_applied`]: this change was applied (a later run treats its value
    /// as the optimizer's, not the user's).
    #[serde(default)]
    pub applied: bool,
    pub explanation: String,
}

/// Why a field was left alone.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum KeptWhy {
    /// The user set it; `clipped_pct` when that value clips the highlights.
    UserSet { clipped_pct: Option<f32> },
    /// The measurement is already on target.
    OnTarget,
    /// The scene's own colour or tone is the point (a sunset's warmth, snow).
    SceneMood { tag: SceneTag },
    /// Proposed, then withdrawn: with the other changes the photo would clip (to white)
    /// or crush (to black) this much of the frame, against the baseline's.
    Withdrawn {
        clipped_pct: f32,
        crushed_pct: f32,
        baseline_clipped_pct: f32,
        baseline_crushed_pct: f32,
    },
}

/// A field the rules looked at and left alone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub field: Field,
    pub label: String,
    pub value: String,
    pub why: KeptWhy,
    pub explanation: String,
}

/// Everything the rules measured.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Measurements {
    /// The camera's white balance and the neutral auto white balance found.
    pub as_shot_wb: [f32; 2],
    pub neutral_wb: [f32; 2],
    pub wb_evidence: Option<Evidence>,
    /// Scene luminance (log2, linear, exposure 0, at the white balance used): mean,
    /// 2nd and 99.5th percentiles; `None` without enough camera data.
    pub log_avg: Option<f32>,
    pub log_p02: f32,
    pub log_p995: f32,
    /// What the user sees now, and the baseline the rules measure (see the module docs).
    pub current: RenderStats,
    pub baseline: RenderStats,
    /// Auto levels of the baseline (black, white).
    pub levels: [f32; 2],
    /// Sensor noise σ (fraction of white) and ISO.
    pub noise_sigma: f32,
    pub iso: Option<u32>,
    /// The lens profile the pipeline would use.
    pub lens_profile: Option<String>,
    /// E-frame size (straighten fits a user crop to it).
    pub frame: [u32; 2],
    /// Clipped and crushed share of the frame with every default-on change applied.
    pub proposed_clipped: f32,
    pub proposed_crushed: f32,
}

/// Who set a field's value, as a report remembers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetBy {
    Optimizer,
    User,
}

/// A field whose value a report remembers the owner of: values an earlier optimize set
/// (carried forward while unchanged), and values the user put back to the default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Owned {
    pub field: Field,
    pub value: Setting,
    pub by: SetBy,
}

/// AI Optimize's result for one photo.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OptimizeReport {
    pub version: u32,
    pub changes: Vec<Change>,
    pub kept: Vec<Kept>,
    /// The templated summary paragraph.
    pub summary: String,
    pub scene: SceneSignals,
    pub measurements: Measurements,
    /// Analysis wall time (ms), renders included.
    pub elapsed_ms: f32,
    /// Owners remembered across runs (see [`Owned`]); a later run's applied changes
    /// take precedence.
    pub owned: Vec<Owned>,
}

impl OptimizeReport {
    /// The value and owner this report remembers for `field`: an applied change's value
    /// (the optimizer's, except a clip pull-back, which leaves the user's exposure the
    /// user's), else a carried-forward entry.
    pub(crate) fn owner_of(&self, field: Field) -> Option<(Setting, SetBy)> {
        self.changes
            .iter()
            .find(|c| c.applied && c.field == field)
            .map(|c| {
                let pull_back = matches!(
                    c.reason,
                    Reason::Exposure {
                        clipped_pct: Some(_),
                        ..
                    }
                );
                (
                    c.new,
                    if pull_back {
                        SetBy::User
                    } else {
                        SetBy::Optimizer
                    },
                )
            })
            .or_else(|| {
                self.owned
                    .iter()
                    .find(|o| o.field == field)
                    .map(|o| (o.value, o.by))
            })
    }
}

/// What to analyze.
#[derive(Debug, Clone)]
pub struct Input<'a> {
    pub raw: &'a RawImage,
    pub adj: &'a Adjustments,
    pub scene: &'a SceneSignals,
    /// The last applied report of this photo (its values are the optimizer's).
    pub prior: Option<&'a OptimizeReport>,
    /// Capture info (ISO, lens) and AI masks for the probe renders; size, quality,
    /// priority and histogram are set by [`analyze`].
    pub request: RenderRequest,
}

/// A probe render: the 8-bit sRGB image and its histogram.
pub type Probe = (RenderedImage, Histogram);

/// Analyze `input` with camera `blocks` ([`crate::auto::camera_blocks`]; the GPU's
/// `GpuRenderer::block_means`) and a renderer for the probes.
pub fn analyze(
    input: &Input<'_>,
    blocks: &[[f32; 3]],
    render: &mut dyn FnMut(&Adjustments, &RenderRequest) -> Result<Probe, PipelineError>,
) -> Result<OptimizeReport, PipelineError> {
    let t0 = Instant::now();
    input.raw.validate()?;
    let (raw, adj) = (input.raw, input.adj);
    let mut m = Measurements {
        iso: input.request.capture.iso,
        noise_sigma: crate::par::run_low(|| crate::plan::raw_noise(raw)),
        lens_profile: lens_profile(&input.request.capture),
        frame: e_frame(raw),
        ..Measurements::default()
    };
    let (neutral, evidence) = crate::auto::wb_estimate(raw, blocks);
    let cam = crate::color::CameraColor::of(raw);
    m.as_shot_wb = wb_pair(crate::color::multipliers_to_wb(raw.wb_as_shot, &cam));
    m.neutral_wb = wb_pair(neutral);
    m.wb_evidence = Some(evidence.into());
    let gray_world_weight = match evidence {
        WbEvidence::GrayWorld { weight } => weight,
        _ => 1.0,
    };

    // Phase 1: white balance and exposure from the camera data.
    let mut out = rules::Outcomes::default();
    let (tone, exposure) = {
        let mut ctx = rules::Ctx {
            adj,
            prior: input.prior,
            scene: input.scene,
            m: &m,
            gray_world_weight,
            tone: None,
        };
        out.push(rules::white_balance(&ctx));
        // Geometry is decided from the signals alone, and measured with: what a
        // straightened or lens-corrected frame shows is what the other rules judge.
        out.push(rules::straighten(&ctx));
        out.push(rules::lens(&ctx));
        let wb = out
            .new_setting(Field::WhiteBalance)
            .map_or(adj.white_balance, |s| rules::wb_of(&s));
        ctx.tone = crate::auto::tone_stats(raw, wb, blocks);
        (ctx.tone, (wb, rules::exposure_target(&ctx)))
    };
    let (wb, exposure) = exposure;
    if let Some(t) = tone {
        m.log_avg = Some(t.log_avg);
        m.log_p02 = t.p02;
        m.log_p995 = t.p995;
    }

    // Phase 2: the probes.
    let req = RenderRequest {
        max_size: Size::new(ANALYSIS_LONG_EDGE, ANALYSIS_LONG_EDGE),
        region: None,
        quality: Quality::Preview,
        priority: Priority::Tool,
        histogram: true,
        output_sharpening: None,
        ..input.request.clone()
    };
    let current = render(adj, &req)?;
    if req
        .cancel
        .as_ref()
        .is_some_and(crate::CancelToken::is_cancelled)
    {
        return Err(PipelineError::Cancelled);
    }
    let current_map = crate::frame_mapping(raw, adj, &req).ok().map(|m| m.1);
    m.current = crate::par::run_low(|| {
        measure::render_stats(&current.0, &current.1, current_map, input.scene)
    });
    let ctx = rules::Ctx {
        adj,
        prior: input.prior,
        scene: input.scene,
        m: &m,
        gray_world_weight,
        tone,
    };
    // A pulled-back exposure is measured at the pull-back, as a rerun on it would be.
    let exposure = rules::pull_back(&ctx).unwrap_or(exposure);
    let (hl_auto, sh_auto) = tone.map_or((0.0, 0.0), |t| t.hdr_for(exposure));
    let mut baseline_adj = rules::baseline(&ctx, wb, exposure, hl_auto, sh_auto);
    for f in [Field::Straighten, Field::LensCorrection] {
        if let Some(v) = out.new_setting(f) {
            set(&mut baseline_adj, f, &v);
        }
    }
    let baseline = render(&baseline_adj, &req)?;
    let levels = crate::auto::levels_for(&baseline.1);
    let baseline_map = crate::frame_mapping(raw, &baseline_adj, &req)
        .ok()
        .map(|m| m.1);
    m.baseline = crate::par::run_low(|| {
        measure::render_stats(&baseline.0, &baseline.1, baseline_map, input.scene)
    });
    m.levels = [levels.0, levels.1];

    // Phase 3: everything else, on the completed measurements.
    let ctx = rules::Ctx {
        adj,
        prior: input.prior,
        scene: input.scene,
        m: &m,
        gray_world_weight,
        tone,
    };
    out.push(rules::exposure(&ctx, exposure));
    out.push(rules::highlights(&ctx, exposure, hl_auto));
    out.push(rules::shadows(&ctx, exposure, sh_auto));
    out.push(rules::levels(&ctx));
    out.push(rules::contrast(&ctx));
    out.push(rules::vibrance(&ctx));
    out.push(rules::saturation(&ctx));
    out.push(rules::clarity(&ctx));
    out.push(rules::texture(&ctx));
    out.push(rules::dehaze(&ctx));
    for o in rules::noise(&ctx) {
        out.push(o);
    }
    out.push(rules::sky_layer(&ctx));

    // Phase 4: the rules judged one field each; check the clipping of all of them
    // together, and back off the "punch" ones (contrast, levels, vibrance, clarity,
    // dehaze) if the proposal clips or crushes noticeably more than the baseline.
    if req
        .cancel
        .as_ref()
        .is_some_and(crate::CancelToken::is_cancelled)
    {
        return Err(PipelineError::Cancelled);
    }
    // Withdraw the likeliest cause first, render again, and stop once the set is fine
    // (skipped when nothing could be withdrawn).
    let mut withdrawn = Vec::new();
    while out.proposes_any(&rules::WITHDRAWABLE) {
        let mut proposed = adj.clone();
        for (f, v) in out.default_on_settings() {
            set(&mut proposed, f, &v);
        }
        // Measured like the baseline: with the geometry proposals, checked or not.
        for f in [Field::Straighten, Field::LensCorrection] {
            if let Some(v) = out.new_setting(f) {
                set(&mut proposed, f, &v);
            }
        }
        let p = render(&proposed, &req)?;
        let s = measure::render_stats(&p.0, &p.1, None, &SceneSignals::default());
        (m.proposed_clipped, m.proposed_crushed) = (s.clipped, s.crushed);
        let Some(cause) = rules::clipping_cause(&m, &out) else {
            break;
        };
        out.drop_fields(&[cause]);
        // Without the dehaze, the black point it held moves after all (checked on the
        // next pass), unless levels were withdrawn already.
        if cause == Field::Dehaze && withdrawn.iter().all(|(f, _)| *f != Field::Levels) {
            let ctx = rules::Ctx {
                adj,
                prior: input.prior,
                scene: input.scene,
                m: &m,
                gray_world_weight,
                tone,
            };
            out.replace(Field::Levels, rules::levels_with(&ctx, adj.presence.dehaze));
        }
        withdrawn.push((
            cause,
            KeptWhy::Withdrawn {
                clipped_pct: m.proposed_clipped * 100.0,
                crushed_pct: m.proposed_crushed * 100.0,
                baseline_clipped_pct: m.baseline.clipped * 100.0,
                baseline_crushed_pct: m.baseline.crushed * 100.0,
            },
        ));
    }
    for (f, why) in withdrawn {
        out.push(rules::Outcome::Kept(f, why, None));
    }
    let ctx = rules::Ctx {
        adj,
        prior: input.prior,
        scene: input.scene,
        m: &m,
        gray_world_weight,
        tone,
    };
    let (changes, kept) = out.finish(&ctx);
    let owned = rules::owned(&ctx);
    let summary = explain::summary(&changes, &kept);
    Ok(OptimizeReport {
        version: REPORT_VERSION,
        changes,
        kept,
        summary,
        scene: input.scene.clone(),
        elapsed_ms: t0.elapsed().as_secs_f32() * 1000.0,
        owned,
        measurements: m,
    })
}

/// [`analyze`] on the CPU reference renderer.
pub fn analyze_cpu(input: &Input<'_>) -> Result<OptimizeReport, PipelineError> {
    input.raw.validate()?;
    let blocks = crate::auto::block_means(input.raw);
    let mut cpu = crate::cpu_renderer();
    analyze(input, &blocks, &mut |adj, req| {
        use crate::Renderer;
        let img = cpu.render(crate::ImageKey(0), input.raw, adj, req)?;
        let h = crate::histogram(&img);
        Ok((img, h))
    })
}

/// `adj` with the changes of `report` whose ids are in `selected` applied.
pub fn apply(adj: &Adjustments, report: &OptimizeReport, selected: &[u32]) -> Adjustments {
    let mut out = adj.clone();
    for c in report.changes.iter().filter(|c| selected.contains(&c.id)) {
        set(&mut out, c.field, &c.new);
    }
    out
}

/// The ids of `report`'s applied changes whose value `adj` still has (an undo, a paste
/// or a slider may have changed it since).
pub fn still_applied(adj: &Adjustments, report: &OptimizeReport) -> Vec<u32> {
    report
        .changes
        .iter()
        .filter(|c| {
            c.applied && rules::current(adj, c.field, &report.measurements).approx_eq(&c.new)
        })
        .map(|c| c.id)
        .collect()
}

/// `report` with `applied` set on exactly the `selected` changes.
pub fn mark_applied(report: &OptimizeReport, selected: &[u32]) -> OptimizeReport {
    let mut r = report.clone();
    for c in &mut r.changes {
        c.applied = selected.contains(&c.id);
    }
    r
}

/// Write `s` into `field` of `adj`.
fn set(adj: &mut Adjustments, field: Field, s: &Setting) {
    match (field, *s) {
        (Field::WhiteBalance, Setting::WhiteBalance { .. }) => adj.white_balance = rules::wb_of(s),
        (Field::Exposure, Setting::Number { value }) => adj.exposure.exposure_ev = value,
        (Field::Highlights, Setting::Number { value }) => adj.hdr.highlights = value,
        (Field::Shadows, Setting::Number { value }) => adj.hdr.shadows = value,
        (Field::Levels, Setting::Levels { black, white }) => {
            adj.levels.in_black = black;
            adj.levels.in_white = white;
        }
        (Field::Contrast, Setting::Number { value }) => adj.exposure.contrast = value,
        (Field::Vibrance, Setting::Number { value }) => adj.presence.vibrance = value,
        (Field::Saturation, Setting::Number { value }) => adj.exposure.saturation = value,
        (Field::Clarity, Setting::Number { value }) => adj.detail.clarity = value,
        (Field::Texture, Setting::Number { value }) => adj.presence.texture = value,
        (Field::Dehaze, Setting::Number { value }) => adj.presence.dehaze = value,
        (Field::AiDenoise, Setting::Toggle { on }) => adj.noise.ai_denoise = on,
        (Field::NoiseReduction, Setting::NoiseReduction { luminance, color }) => {
            adj.noise.luminance = luminance;
            adj.noise.color = color;
        }
        (Field::Straighten, Setting::Straighten { angle_deg, crop }) => {
            adj.crop.angle_deg = angle_deg;
            if let Some(r) = crop {
                adj.crop.rect = r;
            }
        }
        (Field::LensCorrection, Setting::Toggle { on }) => adj.lens.enabled = on,
        (
            Field::SkyLayer,
            Setting::SkyLayer {
                present,
                exposure_ev,
                highlights,
            },
        ) => {
            adj.layers.retain(|l| l.name != SKY_LAYER_NAME);
            if present {
                let id = adj.layers.iter().map(|l| l.id).max().unwrap_or(0) + 1;
                let mut a = LayerAdjustments::default();
                a.exposure.exposure_ev = exposure_ev;
                a.hdr.highlights = highlights;
                adj.layers.push(Layer {
                    id,
                    name: SKY_LAYER_NAME.to_owned(),
                    adjustments: a,
                    mask: Mask {
                        shapes: vec![MaskShape::Ai {
                            op: MaskOp::Add,
                            target: AiTarget::Sky,
                            feather: 0.0,
                            person: None,
                        }],
                        ..Mask::default()
                    },
                    ..Layer::default()
                });
            }
        }
        _ => {}
    }
}

fn wb_pair(wb: lt_core::adjust::WhiteBalance) -> [f32; 2] {
    match wb {
        lt_core::adjust::WhiteBalance::Custom { temp_k, tint } => [temp_k, tint],
        lt_core::adjust::WhiteBalance::AsShot => [5500.0, 0.0],
    }
}

/// The E-frame (oriented image area) size.
fn e_frame(raw: &RawImage) -> [u32; 2] {
    let [_, _, w, h] = crate::geometry::image_area(raw);
    let s = raw.orientation.apply_to(Size::new(w, h));
    [s.width, s.height]
}

fn lens_profile(capture: &crate::CaptureInfo) -> Option<String> {
    let lens = capture.lens.as_deref()?;
    let p = crate::lens::find(lens, capture.crop_factor.unwrap_or(1.0))?;
    Some(if p.model.starts_with(&p.maker) {
        p.model.clone()
    } else {
        format!("{} {}", p.maker, p.model)
    })
}

impl ToneStats {
    /// Linear scene midtone at exposure `ev`.
    pub(crate) fn midtone(&self, ev: f32) -> f32 {
        (self.log_avg + ev).exp2()
    }
}
