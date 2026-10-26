//! Natural-language edit seam: text → typed [`EditIntent`] → [`Adjustments`].
//!
//! Backends implement [`IntentBackend`]. The Swift app adds Apple
//! `FoundationModels` (guided generation straight into this struct, which is
//! why it is plain serde data); a `TypeSafe` Jev backend can slot in later.
//! [`RuleBasedParser`] is the deterministic offline fallback.
//!
//! Shifts are **deltas** on the current adjustments, in `lt_core::adjust` UI
//! units. [`apply_intent`] never guesses what it cannot know: without the
//! image's as-shot white balance or oriented size, the edits that need them
//! are returned as [`Unapplied`] instead of being applied to a made-up value.

mod rules;

use lt_core::adjust::{Aspect, WhiteBalance};
use lt_core::{Adjustments, NormRect, Size};
use serde::{Deserialize, Serialize};

pub use rules::{ParseReport, RuleBasedParser};

/// Target crop shape as spoken: "4:5" is 4 wide by 5 tall (portrait).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CropAspect {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "degrees")]
pub enum Straighten {
    /// Level the horizon automatically (needs pixels: Vision
    /// `VNDetectHorizonRequest`); reported as [`Unapplied::AutoStraighten`].
    Auto,
    /// Rotate by this many degrees, counter-clockwise positive.
    Degrees(f32),
}

/// The eight Basic color-editor bands, in `BASIC_BAND_HUES` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorBand {
    Red,
    Orange,
    Yellow,
    Green,
    Cyan,
    Blue,
    Purple,
    Magenta,
}

impl ColorBand {
    fn index(self) -> usize {
        self as usize
    }
}

/// A local color edit on one band ("bluer sky", "brighter greens").
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BandEdit {
    pub band: ColorBand,
    /// -100..=100 deltas.
    pub saturation: f32,
    pub lightness: f32,
}

/// A typed edit request. `Default` is "do nothing".
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EditIntent {
    /// Warmer (+) / cooler (−), -100..=100 slider units; 100 shifts the white
    /// balance by 60 mired (5 500 K → ≈ 8 200 K); 25 ≈ +500 K.
    pub warmth: f32,
    /// Set the white balance to this temperature (K) before other WB shifts.
    pub temperature_k: Option<f32>,
    /// Kelvin delta ("warmer by 500k").
    pub temperature_shift_k: f32,
    /// Magenta (+) / green (−), in `WhiteBalance::Custom::tint` units.
    pub tint: f32,
    /// Exposure change in stops.
    pub exposure_ev: f32,
    /// -100..=100 slider deltas.
    pub contrast: f32,
    pub saturation: f32,
    /// HDR shadows: + lifts.
    pub shadows: f32,
    /// HDR highlights: + recovers (darkens) highlights, as in Capture One.
    pub highlights: f32,
    pub color_bands: Vec<BandEdit>,
    pub crop_aspect: Option<CropAspect>,
    pub straighten: Option<Straighten>,
    /// `Some(true)` = convert to black & white, `Some(false)` = back to color.
    pub black_and_white: Option<bool>,
    /// Auto tone + white balance; needs pixels, reported as [`Unapplied::Auto`].
    pub auto: bool,
    /// Auto white balance only ("remove the color cast"); needs pixels,
    /// reported as [`Unapplied::AutoWhiteBalance`].
    pub auto_white_balance: bool,
}

impl EditIntent {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn touches_wb(&self) -> bool {
        self.warmth != 0.0
            || self.tint != 0.0
            || self.temperature_shift_k != 0.0
            || self.temperature_k.is_some()
    }

    /// Combine two intents as if applied one after the other: shifts add,
    /// options from `other` win when set, band edits accumulate. An absolute
    /// temperature in `other` discards the earlier white-balance shifts, so
    /// "warmer by 500k, then set 3200k" ends at 3 200 K.
    #[must_use]
    pub fn then(mut self, other: EditIntent) -> EditIntent {
        if other.temperature_k.is_some() {
            self.warmth = 0.0;
            self.temperature_shift_k = 0.0;
        }
        let straighten = match (self.straighten, other.straighten) {
            (Some(Straighten::Degrees(a)), Some(Straighten::Degrees(b))) => {
                Some(Straighten::Degrees(a + b))
            }
            (a, b) => b.or(a),
        };
        self.color_bands.extend(other.color_bands);
        EditIntent {
            warmth: self.warmth + other.warmth,
            temperature_k: other.temperature_k.or(self.temperature_k),
            temperature_shift_k: self.temperature_shift_k + other.temperature_shift_k,
            tint: self.tint + other.tint,
            exposure_ev: self.exposure_ev + other.exposure_ev,
            contrast: self.contrast + other.contrast,
            saturation: self.saturation + other.saturation,
            shadows: self.shadows + other.shadows,
            highlights: self.highlights + other.highlights,
            color_bands: self.color_bands,
            crop_aspect: other.crop_aspect.or(self.crop_aspect),
            straighten,
            black_and_white: other.black_and_white.or(self.black_and_white),
            auto: self.auto || other.auto,
            auto_white_balance: self.auto_white_balance || other.auto_white_balance,
        }
    }
}

/// What [`apply_intent`] needs to know about the image. Nothing is assumed:
/// leave a field `None` when it is not known.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct IntentContext {
    /// Oriented image size (after EXIF orientation and quarter turns).
    pub image_size: Option<Size>,
    /// As-shot white balance `(kelvin, tint)`, from `lt-pipeline::color`'s
    /// multipliers → CCT conversion. Needed when the current WB is `AsShot`.
    pub as_shot_wb: Option<(f32, f32)>,
}

/// Parts of an intent that were not applied, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unapplied {
    /// White balance is `AsShot` and the as-shot values were not supplied.
    WhiteBalance,
    /// The oriented image size was not supplied.
    Crop,
    /// Auto tone needs pixels (lt-pipeline `auto`).
    Auto,
    /// Horizon detection needs pixels (Vision).
    AutoStraighten,
    /// Auto white balance needs pixels (lt-pipeline `auto`).
    AutoWhiteBalance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub adjustments: Adjustments,
    pub unapplied: Vec<Unapplied>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntentError {
    #[error("could not understand {0:?}")]
    NotUnderstood(String),
    #[error("{backend} is unavailable: {reason}")]
    Unavailable { backend: String, reason: String },
}

/// Turns text into an [`EditIntent`]. Object-safe so backends can be chained
/// or swapped at runtime (`FoundationModels` → Jev → rules).
///
/// Returns the whole [`ParseReport`]: whatever was not understood (or was
/// refused as ambiguous) must reach the UI, never be dropped silently. `Err`
/// only when nothing at all could be applied.
pub trait IntentBackend: Send + Sync {
    /// Short identifier for logs and UI ("rules", "foundation-models", "jev").
    fn name(&self) -> &str;
    fn parse(&self, text: &str) -> Result<ParseReport, IntentError>;
}

fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// Add a finite, non-zero `delta` and clamp to `±limit`. Fields the intent
/// does not mention are never rewritten (not even clamped).
fn shift(v: &mut f32, delta: f32, limit: f32) {
    let d = finite(delta);
    if d != 0.0 {
        *v = (*v + d).clamp(-limit, limit);
    }
}

/// Mired shift per warmth slider unit.
const MIRED_PER_WARMTH: f32 = 0.6;
const KELVIN: std::ops::RangeInclusive<f32> = 2000.0..=50_000.0;

/// Apply `intent` on top of `adj`. Non-finite values in the intent are
/// ignored and every result is clamped to the `lt_core` slider ranges.
pub fn apply_intent(adj: &Adjustments, intent: &EditIntent, ctx: &IntentContext) -> Applied {
    let mut out = adj.clone();
    let mut unapplied = Vec::new();

    if intent.touches_wb() {
        let base = match adj.white_balance {
            WhiteBalance::Custom { temp_k, tint } => Some((temp_k, tint)),
            WhiteBalance::AsShot => ctx.as_shot_wb,
        };
        match base {
            Some((k, t)) => out.white_balance = shifted_wb(finite(k), finite(t), intent),
            None => unapplied.push(Unapplied::WhiteBalance),
        }
    }

    let e = &mut out.exposure;
    shift(&mut e.exposure_ev, intent.exposure_ev, 5.0);
    shift(&mut e.contrast, intent.contrast, 100.0);
    shift(&mut e.saturation, intent.saturation, 100.0);
    match intent.black_and_white {
        Some(true) => e.saturation = -100.0,
        Some(false) if e.saturation <= -100.0 => e.saturation = 0.0,
        _ => {}
    }
    shift(&mut out.hdr.shadows, intent.shadows, 100.0);
    shift(&mut out.hdr.highlights, intent.highlights, 100.0);
    for b in &intent.color_bands {
        let r = &mut out.color_editor.basic[b.band.index()];
        shift(&mut r.saturation, b.saturation, 100.0);
        shift(&mut r.lightness, b.lightness, 100.0);
    }

    match intent.straighten {
        Some(Straighten::Degrees(d)) => shift(&mut out.crop.angle_deg, d, 45.0),
        Some(Straighten::Auto) => unapplied.push(Unapplied::AutoStraighten),
        None => {}
    }
    if let Some(a) = intent.crop_aspect.filter(|a| a.width > 0 && a.height > 0) {
        match ctx.image_size.filter(|s| s.width > 0 && s.height > 0) {
            Some(size) => {
                out.crop.rect = fit_aspect(out.crop.rect, a, size);
                out.crop.aspect = Some(Aspect {
                    long: a.width.max(a.height),
                    short: a.width.min(a.height),
                });
            }
            None => unapplied.push(Unapplied::Crop),
        }
    }
    if intent.auto {
        unapplied.push(Unapplied::Auto);
    }
    if intent.auto_white_balance {
        unapplied.push(Unapplied::AutoWhiteBalance);
    }
    Applied {
        adjustments: out,
        unapplied,
    }
}

fn shifted_wb(k: f32, t: f32, intent: &EditIntent) -> WhiteBalance {
    let mut k = k.clamp(*KELVIN.start(), *KELVIN.end());
    if let Some(abs) = intent.temperature_k.filter(|v| v.is_finite()) {
        k = abs.clamp(*KELVIN.start(), *KELVIN.end());
    }
    k = (k + finite(intent.temperature_shift_k)).clamp(*KELVIN.start(), *KELVIN.end());
    let warmth = finite(intent.warmth);
    if warmth != 0.0 {
        let mired = 1.0e6 / k - warmth * MIRED_PER_WARMTH;
        k = (1.0e6 / mired.clamp(20.0, 500.0)).clamp(*KELVIN.start(), *KELVIN.end());
    }
    WhiteBalance::Custom {
        temp_k: k,
        tint: (t + finite(intent.tint)).clamp(-150.0, 150.0),
    }
}

/// Largest rect of aspect `a` (in pixels of `size`) inside the current crop,
/// centred on it.
fn fit_aspect(current: NormRect, a: CropAspect, size: Size) -> NormRect {
    let (iw, ih) = (f64::from(size.width), f64::from(size.height));
    let ratio = f64::from(a.width) / f64::from(a.height);
    let (cw, ch) = (f64::from(current.w) * iw, f64::from(current.h) * ih);
    let (pw, ph) = if cw / ch > ratio {
        (ch * ratio, ch)
    } else {
        (cw, cw / ratio)
    };
    let (w, h) = ((pw / iw) as f32, (ph / ih) as f32);
    let (cx, cy) = (current.x + current.w / 2.0, current.y + current.h / 2.0);
    NormRect {
        x: cx - w / 2.0,
        y: cy - h / 2.0,
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A9: IntentContext = IntentContext {
        image_size: Some(Size::new(6000, 4000)),
        as_shot_wb: None,
    };

    fn apply(adj: &Adjustments, i: &EditIntent, ctx: &IntentContext) -> Adjustments {
        apply_intent(adj, i, ctx).adjustments
    }

    fn wb(adj: &Adjustments) -> (f32, f32) {
        let WhiteBalance::Custom { temp_k, tint } = adj.white_balance else {
            panic!("{adj:?}")
        };
        (temp_k, tint)
    }

    #[test]
    fn empty_intent_is_identity() {
        let adj = Adjustments::default();
        let r = apply_intent(&adj, &EditIntent::default(), &IntentContext::default());
        assert_eq!(
            r,
            Applied {
                adjustments: adj,
                unapplied: vec![]
            }
        );
    }

    #[test]
    fn as_shot_wb_is_never_guessed() {
        // Tungsten as-shot 3200 K. "Cooler" must lower it, not jump to ~5 000 K.
        let adj = Adjustments::default();
        let cooler = EditIntent {
            warmth: -25.0,
            ..EditIntent::default()
        };
        let tungsten = IntentContext {
            as_shot_wb: Some((3200.0, 5.0)),
            ..IntentContext::default()
        };
        let (k, t) = wb(&apply(&adj, &cooler, &tungsten));
        assert!(k < 3200.0 && k > 2900.0 && t == 5.0, "{k} {t}");
        // Unknown as-shot: nothing applied, and it says so.
        let r = apply_intent(&adj, &cooler, &IntentContext::default());
        assert_eq!(
            (r.adjustments, r.unapplied),
            (adj.clone(), vec![Unapplied::WhiteBalance])
        );
        // Tint-only keeps the temperature exactly.
        let tint = EditIntent {
            tint: 10.0,
            ..EditIntent::default()
        };
        assert_eq!(wb(&apply(&adj, &tint, &tungsten)), (3200.0, 15.0));
        // A custom WB needs no context.
        let custom = Adjustments {
            white_balance: WhiteBalance::Custom {
                temp_k: 5000.0,
                tint: 0.0,
            },
            ..adj.clone()
        };
        assert_eq!(
            wb(&apply(&custom, &tint, &IntentContext::default())),
            (5000.0, 10.0)
        );
    }

    #[test]
    fn kelvin_absolute_and_shift() {
        let adj = Adjustments {
            white_balance: WhiteBalance::Custom {
                temp_k: 5000.0,
                tint: 0.0,
            },
            ..Adjustments::default()
        };
        let ctx = IntentContext::default();
        let set = EditIntent {
            temperature_k: Some(3200.0),
            ..EditIntent::default()
        };
        assert_eq!(wb(&apply(&adj, &set, &ctx)).0, 3200.0);
        let by = EditIntent {
            temperature_shift_k: 500.0,
            ..EditIntent::default()
        };
        assert_eq!(wb(&apply(&adj, &by, &ctx)).0, 5500.0);
        let wild = EditIntent {
            warmth: -1e9,
            tint: -1e9,
            temperature_k: Some(f32::NAN),
            ..EditIntent::default()
        };
        assert_eq!(wb(&apply(&adj, &wild, &ctx)), (2000.0, -150.0));
    }

    #[test]
    fn tone_and_band_shifts_add_clamp_and_leave_the_rest_alone() {
        let mut adj = Adjustments::default();
        adj.exposure.contrast = 90.0;
        adj.hdr.blacks = 150.0; // out of range, untouched by the intent
        let i = EditIntent {
            exposure_ev: 0.5,
            contrast: 20.0,
            saturation: -10.0,
            shadows: 25.0,
            highlights: 30.0,
            color_bands: vec![BandEdit {
                band: ColorBand::Blue,
                saturation: 20.0,
                lightness: -10.0,
            }],
            ..EditIntent::default()
        };
        let out = apply(&adj, &i, &A9);
        assert_eq!(out.exposure.exposure_ev, 0.5);
        assert_eq!(out.exposure.contrast, 100.0);
        assert_eq!(out.exposure.saturation, -10.0);
        assert_eq!(
            (out.hdr.shadows, out.hdr.highlights, out.hdr.blacks),
            (25.0, 30.0, 150.0)
        );
        let blue = out.color_editor.basic[5];
        assert_eq!(
            (blue.center_hue, blue.saturation, blue.lightness),
            (240.0, 20.0, -10.0)
        );
        let nan = EditIntent {
            exposure_ev: f32::NAN,
            straighten: Some(Straighten::Degrees(f32::NAN)),
            ..EditIntent::default()
        };
        assert_eq!(apply(&adj, &nan, &A9), adj);
    }

    #[test]
    fn black_and_white_round_trip() {
        let adj = Adjustments::default();
        let bw = apply(
            &adj,
            &EditIntent {
                black_and_white: Some(true),
                ..EditIntent::default()
            },
            &A9,
        );
        assert_eq!(bw.exposure.saturation, -100.0);
        let color = apply(
            &bw,
            &EditIntent {
                black_and_white: Some(false),
                ..EditIntent::default()
            },
            &A9,
        );
        assert_eq!(color.exposure.saturation, 0.0);
        let mut muted = adj.clone();
        muted.exposure.saturation = -30.0;
        let same = apply(
            &muted,
            &EditIntent {
                black_and_white: Some(false),
                ..EditIntent::default()
            },
            &A9,
        );
        assert_eq!(same.exposure.saturation, -30.0);
    }

    #[test]
    fn crop_needs_the_real_size_and_fits_inside_the_current_crop() {
        let adj = Adjustments::default();
        let i = EditIntent {
            crop_aspect: Some(CropAspect {
                width: 4,
                height: 5,
            }),
            ..EditIntent::default()
        };
        // No size → not applied.
        let r = apply_intent(&adj, &i, &IntentContext::default());
        assert_eq!(
            (r.adjustments, r.unapplied),
            (adj.clone(), vec![Unapplied::Crop])
        );
        let out = apply(&adj, &i, &A9);
        let r = out.crop.rect;
        assert!(
            ((r.w * 6000.0) / (r.h * 4000.0) - 0.8).abs() < 1e-4 && (r.h - 1.0).abs() < 1e-6,
            "{r:?}"
        );
        assert_eq!(out.crop.aspect, Some(Aspect { long: 5, short: 4 }));
        // Inside an existing off-centre crop: never grows past it.
        let mut cropped = adj.clone();
        cropped.crop.rect = NormRect {
            x: 0.6,
            y: 0.1,
            w: 0.4,
            h: 0.5,
        };
        let sq = EditIntent {
            crop_aspect: Some(CropAspect {
                width: 1,
                height: 1,
            }),
            ..EditIntent::default()
        };
        let r = apply(&cropped, &sq, &A9).crop.rect;
        assert!(
            r.x >= 0.6 - 1e-6
                && r.x + r.w <= 1.0 + 1e-6
                && r.y >= 0.1 - 1e-6
                && r.y + r.h <= 0.6 + 1e-6,
            "{r:?}"
        );
        assert!(((r.w * 6000.0) / (r.h * 4000.0) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn straighten_and_auto() {
        let adj = Adjustments::default();
        let d = EditIntent {
            straighten: Some(Straighten::Degrees(90.0)),
            ..EditIntent::default()
        };
        assert_eq!(apply(&adj, &d, &A9).crop.angle_deg, 45.0);
        let auto = EditIntent {
            straighten: Some(Straighten::Auto),
            auto: true,
            auto_white_balance: true,
            ..EditIntent::default()
        };
        let r = apply_intent(&adj, &auto, &A9);
        let expected = vec![
            Unapplied::AutoStraighten,
            Unapplied::Auto,
            Unapplied::AutoWhiteBalance,
        ];
        assert_eq!((r.adjustments, r.unapplied), (adj, expected));
    }

    #[test]
    fn then_combines() {
        let a = EditIntent {
            warmth: 10.0,
            straighten: Some(Straighten::Degrees(1.0)),
            crop_aspect: Some(CropAspect {
                width: 1,
                height: 1,
            }),
            color_bands: vec![BandEdit {
                band: ColorBand::Green,
                saturation: 5.0,
                lightness: 0.0,
            }],
            ..EditIntent::default()
        };
        let b = EditIntent {
            warmth: 5.0,
            temperature_k: Some(4000.0),
            straighten: Some(Straighten::Degrees(2.0)),
            black_and_white: Some(true),
            auto: true,
            color_bands: vec![BandEdit {
                band: ColorBand::Red,
                saturation: 5.0,
                lightness: 0.0,
            }],
            ..EditIntent::default()
        };
        let c = a.clone().then(b);
        // The later absolute temperature discards the earlier warmth.
        assert_eq!((c.warmth, c.temperature_k), (5.0, Some(4000.0)));
        assert_eq!(c.straighten, Some(Straighten::Degrees(3.0)));
        assert_eq!(c.crop_aspect, a.crop_aspect);
        assert_eq!(
            (c.black_and_white, c.auto, c.color_bands.len()),
            (Some(true), true, 2)
        );
        let d = c.clone().then(EditIntent {
            straighten: Some(Straighten::Auto),
            ..EditIntent::default()
        });
        assert_eq!(d.straighten, Some(Straighten::Auto));
        // An absolute temperature later in the text wins over earlier shifts…
        let set = EditIntent {
            temperature_k: Some(3200.0),
            ..EditIntent::default()
        };
        let shift = EditIntent {
            temperature_shift_k: 500.0,
            ..EditIntent::default()
        };
        let later = shift.clone().then(set.clone());
        assert_eq!(
            (later.temperature_k, later.temperature_shift_k, later.warmth),
            (Some(3200.0), 0.0, 0.0)
        );
        // …while a shift after it still applies on top.
        let after = set.then(shift);
        assert_eq!(
            (after.temperature_k, after.temperature_shift_k),
            (Some(3200.0), 500.0)
        );
    }

    #[test]
    fn intent_serializes_for_llm_guided_generation() {
        let i = EditIntent {
            exposure_ev: 0.5,
            straighten: Some(Straighten::Degrees(-1.5)),
            ..EditIntent::default()
        };
        let json = serde_json::to_string(&i).unwrap();
        assert!(
            json.contains(r#""straighten":{"mode":"degrees","degrees":-1.5}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<EditIntent>(&json).unwrap(), i);
        let partial: EditIntent = serde_json::from_str(r#"{"warmth": 20}"#).unwrap();
        assert_eq!(
            partial,
            EditIntent {
                warmth: 20.0,
                ..EditIntent::default()
            }
        );
    }
}
