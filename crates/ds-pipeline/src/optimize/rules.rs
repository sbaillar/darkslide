//! The rules: deterministic functions of the measurements, one per field.
//!
//! Every target is absolute (a function of the camera data and of the *baseline* render,
//! never of the field's own current value), so a second run proposes nothing. Small
//! differences inside a dead band are not worth a change.

use ds_core::Adjustments;
use ds_core::adjust::{AiTarget, MaskShape, WhiteBalance};

use super::{
    Change, Evidence, Field, Kept, KeptWhy, Measurements, OptimizeReport, Owned, Reason,
    SKY_LAYER_NAME, SceneSignals, SceneTag, SetBy, Setting, explain,
};
use crate::auto::ToneStats;

/// Exposure changes smaller than this (EV) aren't proposed.
pub(crate) const EV_DEADBAND: f32 = 0.15;
/// White balance changes smaller than both of these aren't proposed.
pub(crate) const WB_DEADBAND_K: f32 = 150.0;
pub(crate) const WB_DEADBAND_TINT: f32 = 3.0;
/// Slider changes smaller than this aren't proposed.
pub(crate) const SLIDER_DEADBAND: f32 = 5.0;
/// Middle gray: the default midtone target (linear luminance).
pub(crate) const MIDTONE: f32 = crate::tone::MID_GRAY;
/// Display luma spread (std) of a well-contrasted photo, and below which it is flat.
pub(crate) const SPREAD_TARGET: f32 = 0.19;
pub(crate) const SPREAD_FLAT: f32 = 0.15;
/// Mean saturation of a natural colour photo, and below which colours read as muted.
pub(crate) const SATURATION_TARGET: f32 = 0.28;
pub(crate) const SATURATION_MUTED: f32 = 0.2;
/// Local contrast (mean |luma − 3×3 mean|, display) of a crisp photo at 1024 px.
pub(crate) const DETAIL_TARGET: f32 = 0.016;
/// Haze score above which (with lifted blacks and low contrast) dehaze is proposed.
pub(crate) const HAZE_MIN: f32 = 0.35;
/// ISO, or sensor noise σ (fraction of white), at which noise reduction is proposed.
pub(crate) const NOISY_ISO: u32 = 3200;
pub(crate) const NOISY_SIGMA: f32 = 0.008;
/// Horizon tilts outside this range (degrees) are left alone: smaller is level enough,
/// larger is probably deliberate (or not a horizon).
pub(crate) const TILT_RANGE: [f32; 2] = [0.5, 8.0];

/// Who set a field's current value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owner {
    /// It is at its default.
    Default,
    /// An applied optimize set it (the prior report).
    Optimizer,
    /// The user set it: left alone.
    User,
}

/// What the rules see.
#[derive(Debug, Clone)]
pub(crate) struct Ctx<'a> {
    pub adj: &'a Adjustments,
    pub prior: Option<&'a OptimizeReport>,
    pub scene: &'a SceneSignals,
    pub m: &'a Measurements,
    /// The gray-world estimate's near-neutral share (1 for stronger evidence).
    pub gray_world_weight: f32,
    /// Scene luminance at the white balance used (`None` without camera data).
    pub tone: Option<ToneStats>,
}

impl Ctx<'_> {
    /// Who set `field`: the prior report's memory when the value is still the one it
    /// remembers; a default value over a remembered one is the user's reset; any
    /// other value off the default is the user's.
    pub fn owner(&self, field: Field) -> Owner {
        let cur = current(self.adj, field, self.m);
        let default = current(&Adjustments::default(), field, self.m);
        let remembered = self.prior.and_then(|p| p.owner_of(field));
        match remembered {
            Some((v, by)) if v.approx_eq(&cur) => match by {
                SetBy::Optimizer if cur.approx_eq(&default) => Owner::Default,
                SetBy::Optimizer => Owner::Optimizer,
                SetBy::User => Owner::User,
            },
            Some((v, _)) if cur.approx_eq(&default) && !v.approx_eq(&default) => Owner::User,
            _ if cur.approx_eq(&default) => Owner::Default,
            _ => Owner::User,
        }
    }

    fn user(&self, field: Field) -> bool {
        self.owner(field) == Owner::User
    }

    fn noisy(&self) -> bool {
        self.m.iso.is_some_and(|i| i >= NOISY_ISO) || self.m.noise_sigma >= NOISY_SIGMA
    }

    fn portrait(&self) -> bool {
        self.scene.faces > 0 || self.scene.has(SceneTag::Portrait)
    }
}

/// What the next run must remember: values the optimizer owns now, and defaults the
/// user restored over an optimizer value.
pub(crate) fn owned(ctx: &Ctx<'_>) -> Vec<Owned> {
    Field::ALL
        .into_iter()
        .filter_map(|field| {
            let value = current(ctx.adj, field, ctx.m);
            let by = match ctx.owner(field) {
                Owner::Optimizer => SetBy::Optimizer,
                Owner::User if value.approx_eq(&current(&Adjustments::default(), field, ctx.m)) => {
                    SetBy::User
                }
                _ => return None,
            };
            Some(Owned { field, value, by })
        })
        .collect()
}

/// Worse than the baseline: ≥ 0.5% of the frame more, and half again as much.
fn worse(now: f32, before: f32) -> bool {
    now > before + 0.005 && now > before * 1.5
}

fn clips_more(m: &Measurements) -> bool {
    worse(m.proposed_clipped, m.baseline.clipped)
}

fn crushes_more(m: &Measurements) -> bool {
    worse(m.proposed_crushed, m.baseline.crushed)
}

/// Proposals the joint clipping check may withdraw (the ones that add punch or move
/// the end points; exposure, white balance and recovery stay).
pub(crate) const WITHDRAWABLE: [Field; 5] = [
    Field::Contrast,
    Field::Levels,
    Field::Vibrance,
    Field::Clarity,
    Field::Dehaze,
];

/// The proposal to withdraw when the set over-clips: the likeliest cause still
/// proposed, by what got worse (white: contrast, then the levels white point,
/// vibrance, clarity, dehaze; black: the levels black point, contrast, dehaze, clarity).
pub(crate) fn clipping_cause(m: &Measurements, out: &Outcomes) -> Option<Field> {
    let order: &[Field] = if clips_more(m) {
        &[
            Field::Contrast,
            Field::Levels,
            Field::Vibrance,
            Field::Clarity,
            Field::Dehaze,
        ]
    } else if crushes_more(m) {
        &[
            Field::Levels,
            Field::Contrast,
            Field::Dehaze,
            Field::Clarity,
        ]
    } else {
        return None;
    };
    order.iter().copied().find(|f| out.proposes_any(&[*f]))
}

/// One rule's verdict.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    Change(Proposal),
    Kept(Field, KeptWhy, Option<Reason>),
    Nothing,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Proposal {
    pub field: Field,
    pub new: Setting,
    pub reason: Reason,
    pub confidence: f32,
    pub default_on: bool,
}

fn propose(field: Field, new: Setting, reason: Reason, confidence: f32) -> Outcome {
    Outcome::Change(Proposal {
        field,
        new,
        reason,
        confidence: confidence.clamp(0.05, 0.99),
        default_on: true,
    })
}

fn kept_user(field: Field) -> Outcome {
    Outcome::Kept(field, KeptWhy::UserSet { clipped_pct: None }, None)
}

/// The rules' verdicts, in order.
#[derive(Debug, Default)]
pub(crate) struct Outcomes(Vec<Outcome>);

impl Outcomes {
    pub fn push(&mut self, o: Outcome) {
        self.0.push(o);
    }

    /// The value proposed for `field`, if any.
    pub fn new_setting(&self, field: Field) -> Option<Setting> {
        self.0.iter().find_map(|o| match o {
            Outcome::Change(p) if p.field == field => Some(p.new),
            _ => None,
        })
    }

    /// The values the default-on proposals set.
    pub fn default_on_settings(&self) -> Vec<(Field, Setting)> {
        self.0
            .iter()
            .filter_map(|o| match o {
                Outcome::Change(p) if p.default_on => Some((p.field, p.new)),
                _ => None,
            })
            .collect()
    }

    /// Whether a default-on proposal exists for any of `fields`.
    pub fn proposes_any(&self, fields: &[Field]) -> bool {
        self.0
            .iter()
            .any(|o| matches!(o, Outcome::Change(p) if p.default_on && fields.contains(&p.field)))
    }

    /// Replace whatever was decided for `field` with `o`.
    pub fn replace(&mut self, field: Field, o: Outcome) {
        self.0.retain(|x| match x {
            Outcome::Change(p) => p.field != field,
            Outcome::Kept(f, ..) => *f != field,
            Outcome::Nothing => true,
        });
        self.0.push(o);
    }

    /// Withdraw the proposals for `fields`.
    pub fn drop_fields(&mut self, fields: &[Field]) {
        for o in &mut self.0 {
            if matches!(o, Outcome::Change(p) if fields.contains(&p.field)) {
                *o = Outcome::Nothing;
            }
        }
    }

    /// Changes (sorted by field, numbered) and kept notes, with their texts.
    pub fn finish(self, ctx: &Ctx<'_>) -> (Vec<Change>, Vec<Kept>) {
        let mut props: Vec<Proposal> = Vec::new();
        let mut kept = Vec::new();
        for o in self.0 {
            match o {
                Outcome::Change(p) => props.push(p),
                Outcome::Kept(field, why, reason) => {
                    let value = explain::value(field, &current(ctx.adj, field, ctx.m));
                    let explanation = explain::kept(field, &value, why, reason.as_ref());
                    kept.push(Kept {
                        field,
                        label: field.label().to_owned(),
                        value,
                        why,
                        explanation,
                    });
                }
                Outcome::Nothing => {}
            }
        }
        props.sort_by_key(|p| p.field);
        kept.sort_by_key(|k| k.field);
        let changes = props
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let old = current(ctx.adj, p.field, ctx.m);
                let (from, to) = (
                    explain::value(p.field, &old),
                    explain::value(p.field, &p.new),
                );
                let explanation = explain::change(p.field, &old, &p.new, &p.reason, p.default_on);
                Change {
                    id: i as u32,
                    field: p.field,
                    label: p.field.label().to_owned(),
                    old,
                    new: p.new,
                    from,
                    to,
                    reason: p.reason,
                    confidence: (p.confidence * 100.0).round() / 100.0,
                    default_on: p.default_on,
                    applied: false,
                    explanation,
                }
            })
            .collect();
        (changes, kept)
    }
}

/// The current value of `field` in `adj`.
pub(crate) fn current(adj: &Adjustments, field: Field, m: &Measurements) -> Setting {
    let n = |value| Setting::Number { value };
    match field {
        Field::WhiteBalance => match adj.white_balance {
            WhiteBalance::AsShot => Setting::WhiteBalance {
                temp_k: m.as_shot_wb[0],
                tint: m.as_shot_wb[1],
                as_shot: true,
            },
            WhiteBalance::Custom { temp_k, tint } => Setting::WhiteBalance {
                temp_k,
                tint,
                as_shot: false,
            },
        },
        Field::Exposure => n(adj.exposure.exposure_ev),
        Field::Highlights => n(adj.hdr.highlights),
        Field::Shadows => n(adj.hdr.shadows),
        Field::Levels => Setting::Levels {
            black: adj.levels.in_black,
            white: adj.levels.in_white,
        },
        Field::Contrast => n(adj.exposure.contrast),
        Field::Vibrance => n(adj.presence.vibrance),
        Field::Saturation => n(adj.exposure.saturation),
        Field::Clarity => n(adj.detail.clarity),
        Field::Texture => n(adj.presence.texture),
        Field::Dehaze => n(adj.presence.dehaze),
        Field::AiDenoise => Setting::Toggle {
            on: adj.noise.ai_denoise,
        },
        Field::NoiseReduction => Setting::NoiseReduction {
            luminance: adj.noise.luminance,
            color: adj.noise.color,
        },
        Field::Straighten => Setting::Straighten {
            angle_deg: adj.crop.angle_deg,
            crop: None,
        },
        Field::LensCorrection => Setting::Toggle {
            on: adj.lens.enabled,
        },
        Field::SkyLayer => adj.layers.iter().find(|l| l.name == SKY_LAYER_NAME).map_or(
            Setting::SkyLayer {
                present: false,
                exposure_ev: 0.0,
                highlights: 0.0,
            },
            |l| Setting::SkyLayer {
                present: true,
                exposure_ev: l.adjustments.exposure.exposure_ev,
                highlights: l.adjustments.hdr.highlights,
            },
        ),
    }
}

/// The white balance a [`Setting::WhiteBalance`] stands for.
pub(crate) fn wb_of(s: &Setting) -> WhiteBalance {
    match *s {
        Setting::WhiteBalance {
            as_shot: false,
            temp_k,
            tint,
        } => WhiteBalance::Custom { temp_k, tint },
        _ => WhiteBalance::AsShot,
    }
}

fn number(s: &Setting) -> f32 {
    match *s {
        Setting::Number { value } => value,
        _ => 0.0,
    }
}

/// Round a slider value to whole units.
fn slider(v: f32) -> f32 {
    v.round()
}

/// A slider rule's outcome: propose `target` unless the user set the field or it is
/// within the dead band of the current value.
fn slider_rule(
    ctx: &Ctx<'_>,
    field: Field,
    target: Option<f32>,
    reason: Reason,
    confidence: f32,
) -> Outcome {
    if ctx.user(field) {
        return kept_user(field);
    }
    let cur = number(&current(ctx.adj, field, ctx.m));
    // Optimizer-owned values return to their default when the reason is gone.
    let target = target.map_or(0.0, slider);
    if (target - cur).abs() < SLIDER_DEADBAND {
        return Outcome::Nothing;
    }
    propose(field, Setting::Number { value: target }, reason, confidence)
}

// ---------------------------------------------------------------- white balance ---

pub(crate) fn white_balance(ctx: &Ctx<'_>) -> Outcome {
    let field = Field::WhiteBalance;
    if ctx.user(field) {
        return kept_user(field);
    }
    let evidence = ctx.m.wb_evidence.unwrap_or(Evidence::AsShot);
    if evidence == Evidence::AsShot {
        return Outcome::Nothing;
    }
    // A sunset's or snow's colour is the point; only a clear neutral overrides it.
    if evidence == Evidence::GrayWorld
        && let Some(tag) = [SceneTag::Sunset, SceneTag::Snow, SceneTag::Beach]
            .into_iter()
            .find(|t| ctx.scene.has(*t))
    {
        return Outcome::Kept(field, KeptWhy::SceneMood { tag }, None);
    }
    let Setting::WhiteBalance { temp_k, tint, .. } = current(ctx.adj, field, ctx.m) else {
        return Outcome::Nothing;
    };
    let [nk, nt] = ctx.m.neutral_wb;
    if (nk - temp_k).abs() < WB_DEADBAND_K && (nt - tint).abs() < WB_DEADBAND_TINT {
        return if ctx.owner(field) == Owner::Default {
            Outcome::Kept(field, KeptWhy::OnTarget, None)
        } else {
            Outcome::Nothing
        };
    }
    let confidence = match evidence {
        Evidence::NeutralCluster => 0.9,
        Evidence::WhitePatch => 0.8,
        _ => 0.45 + 0.35 * ctx.gray_world_weight,
    };
    propose(
        field,
        Setting::WhiteBalance {
            temp_k: (nk / 10.0).round() * 10.0,
            tint: (nt * 10.0).round() / 10.0,
            as_shot: false,
        },
        Reason::ColorCast {
            neutral_k: nk,
            neutral_tint: nt,
            current_k: temp_k,
            current_tint: tint,
            evidence,
        },
        confidence,
    )
}

// ---------------------------------------------------------------- tone ---

/// The midtone a scene type wants (linear luminance) and the tag that set it.
pub(crate) fn midtone_target(scene: &SceneSignals) -> (f32, Option<SceneTag>) {
    if scene.has(SceneTag::Night) {
        (0.09, Some(SceneTag::Night))
    } else if scene.has(SceneTag::Snow) {
        (0.24, Some(SceneTag::Snow))
    } else if scene.has(SceneTag::Beach) {
        (0.24, Some(SceneTag::Beach))
    } else if scene.has(SceneTag::Sunset) {
        (0.14, Some(SceneTag::Sunset))
    } else {
        (MIDTONE, None)
    }
}

/// The exposure the analysis continues with: the user's, the current one when the target
/// is within the dead band, else the target.
pub(crate) fn exposure_target(ctx: &Ctx<'_>) -> f32 {
    let cur = ctx.adj.exposure.exposure_ev;
    let Some(t) = ctx.tone.filter(|_| !ctx.user(Field::Exposure)) else {
        return cur;
    };
    let ev = t.exposure_for(midtone_target(ctx.scene).0);
    if (ev - cur).abs() < EV_DEADBAND {
        cur
    } else {
        ev
    }
}

/// The exposure a deliberate one is pulled back to: when it clips (in the current
/// render) and highlights are set by hand too (nothing else could recover them).
pub(crate) fn pull_back(ctx: &Ctx<'_>) -> Option<f32> {
    let t = ctx.tone?;
    let limit = (t.ev_max() * 100.0).round() / 100.0;
    (ctx.user(Field::Exposure)
        && ctx.m.current.clipped > 0.01
        && ctx.user(Field::Highlights)
        && ctx.adj.exposure.exposure_ev - limit >= EV_DEADBAND)
        .then_some(limit)
}

pub(crate) fn exposure(ctx: &Ctx<'_>, used: f32) -> Outcome {
    let field = Field::Exposure;
    let cur = ctx.adj.exposure.exposure_ev;
    let Some(t) = ctx.tone else {
        return Outcome::Nothing;
    };
    let (target, tag) = midtone_target(ctx.scene);
    let clipped = ctx.m.current.clipped;
    if ctx.user(field) {
        // A deliberate exposure stays, unless it is pulled back.
        if let Some(limit) = pull_back(ctx) {
            return Outcome::Change(Proposal {
                field,
                new: Setting::Number { value: limit },
                reason: Reason::Exposure {
                    midtone: t.midtone(cur),
                    target,
                    limited: true,
                    scene: tag,
                    clipped_pct: Some(clipped * 100.0),
                },
                confidence: 0.7,
                default_on: true,
            });
        }
        return Outcome::Kept(
            field,
            KeptWhy::UserSet {
                clipped_pct: (clipped > 0.01).then_some(clipped * 100.0),
            },
            None,
        );
    }
    let reason = Reason::Exposure {
        midtone: t.midtone(cur),
        target,
        limited: used < (target.log2() - t.log_avg).clamp(-3.0, 3.0) - 0.02,
        scene: tag,
        clipped_pct: None,
    };
    if (used - cur).abs() < 1e-3 {
        return if ctx.owner(field) == Owner::Default {
            Outcome::Kept(field, KeptWhy::OnTarget, Some(reason))
        } else {
            Outcome::Nothing
        };
    }
    let confidence = (0.6 + 0.25 * (used - cur).abs().min(1.0))
        * if tag == Some(SceneTag::Night) {
            0.8
        } else {
            1.0
        };
    propose(field, Setting::Number { value: used }, reason, confidence)
}

pub(crate) fn highlights(ctx: &Ctx<'_>, ev: f32, hl_auto: f32) -> Outcome {
    let field = Field::Highlights;
    if ctx.user(field) {
        let c = ctx.m.current.clipped;
        return Outcome::Kept(
            field,
            KeptWhy::UserSet {
                clipped_pct: (c > 0.01).then_some(c * 100.0),
            },
            None,
        );
    }
    let b = &ctx.m.baseline;
    let clip = b.clipped * 100.0;
    let sky = b.sky_clipped.map(|s| s * 100.0);
    let mut extra = 0.0f32;
    if clip > 0.3 {
        extra = 10.0 + 8.0 * clip;
    }
    if let Some(s) = sky.filter(|s| *s > 1.0) {
        extra = extra.max(15.0 + 0.5 * s);
    }
    let target = (hl_auto + extra).clamp(0.0, 70.0);
    let reason = if extra > 0.0 {
        Reason::HighlightsClipped {
            clipped_pct: clip,
            sky_clipped_pct: sky.filter(|s| *s > 1.0),
        }
    } else {
        Reason::HighlightRange {
            stops_over: ctx.tone.map_or(0.0, |t| t.highlight_excess(ev)),
        }
    };
    slider_rule(
        ctx,
        field,
        Some(target),
        reason,
        0.6 + (clip * 0.1).min(0.3),
    )
}

pub(crate) fn shadows(ctx: &Ctx<'_>, ev: f32, sh_auto: f32) -> Outcome {
    let field = Field::Shadows;
    let b = &ctx.m.baseline;
    let backlit = ctx.scene.has(SceneTag::Backlit)
        || b.subject_luma
            .is_some_and(|s| s < 0.5 * b.median_luma && s < 0.25);
    let mut target = sh_auto;
    if backlit {
        target = target.max(30.0);
    }
    if ctx.scene.has(SceneTag::Night) {
        target = target.min(15.0);
    }
    let c = &ctx.m.current;
    let reason = Reason::Shadows {
        dark_pct: c.dark * 100.0,
        stops_under: ctx.tone.map_or(0.0, |t| t.shadow_deficit(ev)),
        backlit,
        subject_luma: c.subject_luma,
        median_luma: c.median_luma,
    };
    slider_rule(
        ctx,
        field,
        Some(target),
        reason,
        if backlit { 0.75 } else { 0.6 },
    )
}

pub(crate) fn levels(ctx: &Ctx<'_>) -> Outcome {
    // The dehaze this run proposes (default-on), or the one the edit already has.
    let dehaze = match dehaze(ctx) {
        Outcome::Change(p) if p.default_on => number(&p.new),
        _ => ctx.adj.presence.dehaze,
    };
    levels_with(ctx, dehaze)
}

/// Levels with `dehaze` in effect: dehaze takes the veil (the lifted blacks) out itself,
/// and a black point on top of it would crush the shadows.
pub(crate) fn levels_with(ctx: &Ctx<'_>, dehaze: f32) -> Outcome {
    let field = Field::Levels;
    if ctx.user(field) {
        return kept_user(field);
    }
    let [mut black, white] = ctx.m.levels;
    let l = ctx.adj.levels;
    if dehaze > 0.0 {
        black = l.in_black;
    }
    if (black - l.in_black).abs() < 0.01 && (white - l.in_white).abs() < 0.01 {
        return Outcome::Nothing;
    }
    let r = |v: f32| (v * 100.0).round() / 100.0;
    propose(
        field,
        Setting::Levels {
            black: r(black),
            white: r(white),
        },
        Reason::TonalRange {
            darkest: ctx.m.baseline.p001,
            brightest: ctx.m.baseline.p999,
        },
        0.6,
    )
}

/// The baseline's spread after the proposed levels stretch.
fn spread(ctx: &Ctx<'_>) -> f32 {
    let [black, white] = ctx.m.levels;
    ctx.m.baseline.luma_std / (white - black).max(0.5)
}

pub(crate) fn contrast(ctx: &Ctx<'_>) -> Outcome {
    let field = Field::Contrast;
    let s = spread(ctx);
    // Haze is flat too: dehaze handles it.
    let flat = haze_target(ctx).is_none() && !ctx.scene.has(SceneTag::Night) && s < SPREAD_FLAT;
    let target = flat.then(|| {
        let v = ((SPREAD_TARGET - s) * 300.0).clamp(5.0, 25.0);
        let v = if ctx.portrait() { v.min(10.0) } else { v };
        (v / 5.0).round() * 5.0
    });
    slider_rule(
        ctx,
        field,
        target,
        Reason::LowContrast {
            spread: s,
            target: SPREAD_TARGET,
        },
        0.55,
    )
}

// ---------------------------------------------------------------- colour ---

pub(crate) fn vibrance(ctx: &Ctx<'_>) -> Outcome {
    let field = Field::Vibrance;
    let mean = ctx.m.baseline.mean_saturation;
    let target = (mean < SATURATION_MUTED).then(|| {
        let mut v = ((SATURATION_TARGET - mean) * 150.0).clamp(5.0, 30.0);
        if ctx.portrait() {
            v = v.min(15.0); // skin stays natural
        }
        if ctx.scene.has(SceneTag::Night) || ctx.scene.has(SceneTag::Snow) {
            v = v.min(10.0);
        }
        v
    });
    slider_rule(
        ctx,
        field,
        target,
        Reason::Muted {
            mean_saturation: mean,
            target: SATURATION_TARGET,
            faces: ctx.scene.faces,
        },
        0.55,
    )
}

pub(crate) fn saturation(ctx: &Ctx<'_>) -> Outcome {
    let field = Field::Saturation;
    let b = &ctx.m.baseline;
    let target = (b.p90_saturation > 0.95 && b.mean_saturation > 0.5).then_some(-10.0);
    slider_rule(
        ctx,
        field,
        target,
        Reason::Oversaturated {
            p90_saturation: b.p90_saturation,
            mean_saturation: b.mean_saturation,
        },
        0.5,
    )
}

// ---------------------------------------------------------------- detail ---

pub(crate) fn clarity(ctx: &Ctx<'_>) -> Outcome {
    let field = Field::Clarity;
    let d = ctx.m.baseline.detail;
    let ok = !ctx.portrait() && !ctx.scene.has(SceneTag::Night) && !ctx.noisy();
    let target = (ok && d < 0.5 * DETAIL_TARGET)
        .then(|| ((1.0 - d / DETAIL_TARGET) * 25.0).clamp(5.0, 15.0));
    slider_rule(
        ctx,
        field,
        target,
        Reason::SoftDetail {
            detail: d,
            target: DETAIL_TARGET,
            landscape: false,
        },
        0.5,
    )
}

pub(crate) fn texture(ctx: &Ctx<'_>) -> Outcome {
    let field = Field::Texture;
    let d = ctx.m.baseline.detail;
    let target = (ctx.scene.has(SceneTag::Landscape)
        && !ctx.portrait()
        && !ctx.noisy()
        && d < 0.8 * DETAIL_TARGET)
        .then_some(10.0);
    slider_rule(
        ctx,
        field,
        target,
        Reason::SoftDetail {
            detail: d,
            target: DETAIL_TARGET,
            landscape: true,
        },
        0.5,
    )
}

/// The dehaze a hazy baseline wants: a high dark channel with lifted blacks and low
/// contrast, outside scenes that are bright by nature.
fn haze_target(ctx: &Ctx<'_>) -> Option<f32> {
    let b = &ctx.m.baseline;
    let bright_scene = [SceneTag::Snow, SceneTag::Beach, SceneTag::Night]
        .into_iter()
        .any(|t| ctx.scene.has(t));
    (!bright_scene && b.haze > HAZE_MIN && b.p001 > 0.06 && b.luma_std < 0.17)
        .then(|| ((b.haze - 0.3) * 80.0).clamp(5.0, 30.0))
}

pub(crate) fn dehaze(ctx: &Ctx<'_>) -> Outcome {
    let b = &ctx.m.baseline;
    slider_rule(
        ctx,
        Field::Dehaze,
        haze_target(ctx),
        Reason::Haze {
            haze: b.haze,
            darkest: b.p001,
            spread: b.luma_std,
        },
        0.6,
    )
}

// ---------------------------------------------------------------- noise ---

/// AI Denoise when the model is there, else stronger noise reduction.
pub(crate) fn noise(ctx: &Ctx<'_>) -> Vec<Outcome> {
    if !ctx.noisy() {
        return Vec::new();
    }
    let reason = Reason::Noise {
        iso: ctx.m.iso,
        sigma: ctx.m.noise_sigma,
        ai_available: ctx.scene.ai_denoise_available,
    };
    if ctx.scene.ai_denoise_available {
        if ctx.user(Field::AiDenoise) {
            return vec![kept_user(Field::AiDenoise)];
        }
        if ctx.adj.noise.ai_denoise {
            return Vec::new();
        }
        return vec![propose(
            Field::AiDenoise,
            Setting::Toggle { on: true },
            reason,
            0.8,
        )];
    }
    let field = Field::NoiseReduction;
    if ctx.user(field) {
        return vec![kept_user(field)];
    }
    // ~+10 luminance per stop above ISO 1600 (σ stands in when ISO is unknown).
    let stops = ctx.m.iso.map_or_else(
        || (ctx.m.noise_sigma / NOISY_SIGMA).log2() + 1.0,
        |i| (i as f32 / 1600.0).log2(),
    );
    let luminance = ((25.0 + 10.0 * stops).clamp(30.0, 60.0) / 5.0).round() * 5.0;
    let new = Setting::NoiseReduction {
        luminance,
        color: 40.0,
    };
    if new.approx_eq(&current(ctx.adj, field, ctx.m)) {
        return Vec::new();
    }
    vec![propose(field, new, reason, 0.7)]
}

// ---------------------------------------------------------------- geometry ---

pub(crate) fn straighten(ctx: &Ctx<'_>) -> Outcome {
    let field = Field::Straighten;
    let Some(h) = ctx.scene.horizon_deg.filter(|h| h.is_finite()) else {
        return Outcome::Nothing;
    };
    if ctx.user(field) {
        return kept_user(field);
    }
    if !(TILT_RANGE[0]..=TILT_RANGE[1]).contains(&h.abs()) {
        return Outcome::Nothing;
    }
    let angle = (-h * 10.0).round() / 10.0;
    if (angle - ctx.adj.crop.angle_deg).abs() < 0.1 {
        return Outcome::Nothing;
    }
    // A full frame: the angle alone (the pipeline fits it inside the turned and
    // lens-corrected image, as for the straighten tool). A user crop is shrunk about its
    // centre so the turned rectangle stays inside the image.
    let crop = (ctx.adj.crop.rect != ds_core::NormRect::FULL).then(|| {
        let size = ds_core::Size::new(ctx.m.frame[0], ctx.m.frame[1]);
        crate::auto::auto_straighten_from_horizon(-angle, &ctx.adj.crop, size).rect
    });
    propose(
        field,
        Setting::Straighten {
            angle_deg: angle,
            crop,
        },
        Reason::Tilted { horizon_deg: h },
        0.7,
    )
}

pub(crate) fn lens(ctx: &Ctx<'_>) -> Outcome {
    let Some(profile) = ctx.m.lens_profile.clone().filter(|_| !ctx.adj.lens.enabled) else {
        return Outcome::Nothing;
    };
    // Off is the user's choice (corrections are on by default): suggest, unchecked.
    Outcome::Change(Proposal {
        field: Field::LensCorrection,
        new: Setting::Toggle { on: true },
        reason: Reason::LensOff { profile },
        confidence: 0.6,
        default_on: false,
    })
}

pub(crate) fn sky_layer(ctx: &Ctx<'_>) -> Outcome {
    let has_sky_layer = ctx.adj.layers.iter().any(|l| {
        l.name == SKY_LAYER_NAME
            || l.mask.shapes.iter().any(|s| {
                matches!(
                    s,
                    MaskShape::Ai {
                        target: AiTarget::Sky,
                        ..
                    }
                )
            })
    });
    let b = &ctx.m.baseline;
    let (Some(luma), Some(clipped)) = (b.sky_luma, b.sky_clipped) else {
        return Outcome::Nothing;
    };
    if has_sky_layer || ctx.scene.sky_fraction < 0.15 || !(luma > 0.8 || clipped > 0.02) {
        return Outcome::Nothing;
    }
    Outcome::Change(Proposal {
        field: Field::SkyLayer,
        new: Setting::SkyLayer {
            present: true,
            exposure_ev: -0.5,
            highlights: 30.0,
        },
        reason: Reason::BrightSky {
            sky_pct: ctx.scene.sky_fraction * 100.0,
            sky_luma: luma,
            sky_clipped_pct: clipped * 100.0,
        },
        confidence: 0.5,
        default_on: false,
    })
}

// ---------------------------------------------------------------- baseline ---

/// The adjustments the rules measure: the white balance and exposure the analysis
/// continues with, auto tone's highlights/shadows, and every other look field the
/// optimizer may set back at its default (user values stay).
pub(crate) fn baseline(ctx: &Ctx<'_>, wb: WhiteBalance, ev: f32, hl: f32, sh: f32) -> Adjustments {
    let d = Adjustments::default();
    let mut b = ctx.adj.clone();
    b.white_balance = wb;
    b.exposure.exposure_ev = ev;
    let mine = |f: Field| !ctx.user(f);
    if mine(Field::Highlights) {
        b.hdr.highlights = hl;
    }
    if mine(Field::Shadows) {
        b.hdr.shadows = sh;
    }
    if mine(Field::Levels) {
        b.levels.in_black = d.levels.in_black;
        b.levels.in_white = d.levels.in_white;
    }
    if mine(Field::Contrast) {
        b.exposure.contrast = d.exposure.contrast;
    }
    if mine(Field::Saturation) {
        b.exposure.saturation = d.exposure.saturation;
    }
    if mine(Field::Vibrance) {
        b.presence.vibrance = d.presence.vibrance;
    }
    if mine(Field::Texture) {
        b.presence.texture = d.presence.texture;
    }
    if mine(Field::Dehaze) {
        b.presence.dehaze = d.presence.dehaze;
    }
    if mine(Field::Clarity) {
        b.detail.clarity = d.detail.clarity;
    }
    if mine(Field::SkyLayer) {
        b.layers.retain(|l| l.name != SKY_LAYER_NAME);
    }
    b
}
