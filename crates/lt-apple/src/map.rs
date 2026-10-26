//! Aperture / iPhoto adjustments onto Luxtable's. A photo's adjustments are
//! read into a [`Develop`] keyed `<adjustment>.<parameter>` (for example
//! `RKExposureOperation.inputExposure`), a repeated adjustment as
//! `<adjustment>~2.…`, plus `Rotation` (the version's rotation in degrees) and
//! `iPhotoEdit` (an older iPhoto library kept only a rendered edit).
//!
//! Apple never published these formats. The adjustment names come from public
//! readers and Apple's own support pages; the parameter names follow Core
//! Image's `input<Name>` convention, which those readers show. A parameter is
//! looked up by its name without the `input` prefix, case-insensitively, so
//! `inputExposure` and `exposure` both count. When an adjustment is there but
//! none of the parameters it needs is, it is reported as not imported: the
//! mapping never guesses a value.
//!
//! What maps (all approximate: Apple's engine is not Luxtable's):
//!
//! | Aperture / iPhoto | Luxtable |
//! |---|---|
//! | Exposure: Exposure (EV) | Exposure (EV, clamped to ±5) |
//! | Exposure: Recovery (0..1.5) | Highlights (0..100) |
//! | Exposure: Black Point (-5..50) | Blacks (×-2) |
//! | Exposure: Brightness (-2..2) | Brightness (×50) |
//! | Enhance: Contrast (-1..1) | Contrast (×100) |
//! | Enhance: Definition (0..1) | Clarity (×100) |
//! | Enhance: Saturation (0..2, 1 neutral) | Saturation ((v-1)×100) |
//! | Enhance: Vibrancy (-1..1) | Vibrance (×100) |
//! | Highlights & Shadows: Highlights, Shadows (0..100) | Highlights, Shadows |
//! | White Balance: Temperature (K), Tint (RAW files) | Custom white balance |
//! | Straighten: angle (degrees) | Crop angle |
//! | Crop: origin and size (pixels, origin bottom-left) | Crop rectangle |
//! | Black & White | Saturation -100 |
//! | Rotation 180° | Two quarter turns |
//!
//! Everything else (Levels, Curves, Color, sharpening, noise reduction,
//! vignettes, retouching, red-eye, brushed-in adjustments, quarter-turn
//! rotations, the tint wheels and advanced sliders) is listed as not imported.

use lt_core::adjust::WhiteBalance;
use lt_core::geom::NormRect;
use lt_lr::{Category, Develop, Fidelity, MapContext, Mapped, Setting};

/// The develop key of a rotation, in degrees.
pub(crate) const ROTATION: &str = "Rotation";
/// The develop key that says an older iPhoto library kept only a rendered edit.
pub(crate) const IPHOTO_EDIT: &str = "iPhotoEdit";

/// Adjustments that aren't edits: every RAW photo has the decoder's settings.
const IGNORED: [&str; 1] = ["RKRawDecodeOperation"];

/// `RKExposureOperation` → "Exposure"; `RKHighlightsAndShadowsOperation` →
/// "Highlights And Shadows"; any other name as it is.
pub(crate) fn display_name(op: &str) -> String {
    let base = op.split('~').next().unwrap_or(op);
    let core = base
        .strip_prefix("RK")
        .unwrap_or(base)
        .strip_suffix("Operation")
        .unwrap_or_else(|| base.strip_prefix("RK").unwrap_or(base));
    let mut out = String::with_capacity(core.len() + 4);
    let chars: Vec<char> = core.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
        let prev_lower = i > 0 && chars[i - 1].is_lowercase();
        if i > 0 && c.is_uppercase() && (prev_lower || next_lower) && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push(*c);
    }
    if out.is_empty() { op.to_owned() } else { out }
}

/// Why an adjustment Luxtable doesn't map is left out.
fn reason(op: &str) -> &'static str {
    let o = op.to_ascii_lowercase();
    if o.contains("level") {
        "Luxtable's levels work on one range, not Aperture's quarter-tones"
    } else if o.contains("curve") {
        "curves are not carried over"
    } else if o.contains("sharpen") {
        "Luxtable sharpens with its own engine"
    } else if o.contains("noise") {
        "Luxtable reduces noise with its own engine"
    } else if o.contains("vignette") {
        "vignettes are not carried over"
    } else if o.contains("retouch") || o.contains("spot") || o.contains("patch") {
        "retouching is not carried over"
    } else if o.contains("redeye") {
        "red-eye correction is not carried over"
    } else if o.contains("ios") {
        "edits made on iOS are not carried over"
    } else {
        "no Luxtable equivalent"
    }
}

/// The parameters of one adjustment, by name without `input`, lower-cased.
struct Op<'a> {
    name: &'a str,
    params: Vec<(String, &'a Setting)>,
    masked: bool,
}

impl Op<'_> {
    fn num(&self, names: &[&str]) -> Option<f64> {
        names.iter().find_map(|n| {
            self.params
                .iter()
                .find(|(k, _)| k == n)
                .and_then(|(_, s)| s.num())
        })
    }

    fn enabled(&self) -> bool {
        ["enabled", "isenabled"].iter().all(|n| {
            self.params
                .iter()
                .find(|(k, _)| k == n)
                .and_then(|(_, s)| s.flag())
                != Some(false)
        })
    }
}

fn short(param: &str) -> String {
    let p = param.rsplit('.').next().unwrap_or(param);
    let p = p.strip_prefix("input").unwrap_or(p);
    p.to_ascii_lowercase()
}

/// The adjustments in `dev`, in key order (the reader numbers repeats).
fn ops(dev: &Develop) -> Vec<Op<'_>> {
    let mut out: Vec<Op> = Vec::new();
    for (k, v) in dev {
        let Some((op, param)) = k.split_once('.') else {
            continue;
        };
        if out.last().is_none_or(|o| o.name != op) {
            out.push(Op {
                name: op,
                params: Vec::new(),
                masked: false,
            });
        }
        let o = out.last_mut().expect("pushed");
        if param == "mask" {
            o.masked = true;
        } else {
            o.params.push((short(param), v));
        }
    }
    out
}

struct M<'a> {
    ctx: &'a MapContext<'a>,
    out: Mapped,
}

impl M<'_> {
    fn done(&mut self, key: &str, cat: Category) {
        self.out.categories.insert(cat);
        if !self.out.mapped.iter().any(|(k, _)| k == key) {
            self.out
                .mapped
                .push((key.to_owned(), Fidelity::Approximate));
        }
    }

    fn drop_op(&mut self, key: &str, why: &str) {
        if !self.out.dropped.iter().any(|(k, _)| k == key) {
            self.out.dropped.push((key.to_owned(), why.to_owned()));
        }
    }

    /// Set `slot` from `v` when it is an edit (not 0 after scaling).
    fn set(&mut self, key: &str, cat: Category, v: Option<f64>, slot: fn(&mut Self) -> &mut f32) {
        if let Some(v) = v.filter(|v| v.abs() > 1e-6) {
            *slot(self) = v as f32;
            self.done(key, cat);
        }
    }

    fn exposure(&mut self, op: &Op) -> bool {
        let t = Category::Tone;
        let ev = op.num(&["exposure"]).map(|v| v.clamp(-5.0, 5.0));
        let rec = op
            .num(&["recovery"])
            .map(|v| (v / 1.5 * 100.0).clamp(0.0, 100.0));
        let bp = op
            .num(&["blackpoint"])
            .map(|v| (-v * 2.0).clamp(-100.0, 100.0));
        let br = op
            .num(&["brightness"])
            .map(|v| (v * 50.0).clamp(-100.0, 100.0));
        self.set("Exposure", t, ev, |m| {
            &mut m.out.adjustments.exposure.exposure_ev
        });
        self.set("Recovery", t, rec, |m| {
            &mut m.out.adjustments.hdr.highlights
        });
        self.set("Black Point", t, bp, |m| &mut m.out.adjustments.hdr.blacks);
        self.set("Brightness", t, br, |m| {
            &mut m.out.adjustments.exposure.brightness
        });
        [ev, rec, bp, br].iter().any(Option::is_some)
    }

    fn enhance(&mut self, op: &Op) -> bool {
        let contrast = op
            .num(&["contrast"])
            .map(|v| (v * 100.0).clamp(-100.0, 100.0));
        let def = op
            .num(&["definition"])
            .map(|v| (v * 100.0).clamp(0.0, 100.0));
        let sat = op
            .num(&["saturation"])
            .map(|v| ((v - 1.0) * 100.0).clamp(-100.0, 100.0));
        let vib = op
            .num(&["vibrancy", "vibrance"])
            .map(|v| (v * 100.0).clamp(-100.0, 100.0));
        self.set("Contrast", Category::Tone, contrast, |m| {
            &mut m.out.adjustments.exposure.contrast
        });
        self.set("Definition", Category::Detail, def, |m| {
            &mut m.out.adjustments.detail.clarity
        });
        self.set("Saturation", Category::Color, sat, |m| {
            &mut m.out.adjustments.exposure.saturation
        });
        self.set("Vibrancy", Category::Color, vib, |m| {
            &mut m.out.adjustments.presence.vibrance
        });
        [contrast, def, sat, vib].iter().any(Option::is_some)
    }

    fn highlights_shadows(&mut self, op: &Op) -> bool {
        let t = Category::Tone;
        let hi = op
            .num(&["highlights", "highlightamount"])
            .map(|v| v.clamp(0.0, 100.0));
        let sh = op
            .num(&["shadows", "shadowamount"])
            .map(|v| v.clamp(0.0, 100.0));
        self.set("Highlights", t, hi, |m| {
            &mut m.out.adjustments.hdr.highlights
        });
        self.set("Shadows", t, sh, |m| &mut m.out.adjustments.hdr.shadows);
        hi.is_some() || sh.is_some()
    }

    fn white_balance(&mut self, op: &Op) -> bool {
        let Some(temp) = op.num(&["temperature", "temp", "neutraltemperature"]) else {
            return false;
        };
        let tint = op
            .num(&["tint", "neutraltint"])
            .unwrap_or(0.0)
            .clamp(-150.0, 150.0);
        if !self.ctx.is_raw || temp < 1000.0 {
            self.drop_op(
                "White Balance",
                "only a RAW file's temperature in Kelvin carries over",
            );
            return true;
        }
        self.out.adjustments.white_balance = WhiteBalance::Custom {
            temp_k: temp as f32,
            tint: tint as f32,
        };
        self.done("White Balance", Category::WhiteBalance);
        true
    }

    fn straighten(&mut self, op: &Op) -> bool {
        let Some(a) = op.num(&["rotation", "angle", "straightenangle"]) else {
            return false;
        };
        if a.abs() > 1e-6 {
            // Positive turns the picture counter-clockwise, as Luxtable's angle does.
            self.out.adjustments.crop.angle_deg = a.clamp(-45.0, 45.0) as f32;
            self.done("Straighten", Category::Crop);
        }
        true
    }

    /// A pixel rectangle with its origin at the bottom left (as public readers
    /// describe Aperture's crop) as a normalised top-left one. `turns`: the
    /// photo's quarter turns, `None` when they weren't carried over.
    fn crop(&mut self, op: &Op, turns: Option<u8>) -> bool {
        let (Some(x), Some(y), Some(w), Some(h)) = (
            op.num(&["xorigin", "x", "originx", "xstart"]),
            op.num(&["yorigin", "y", "originy", "ystart"]),
            op.num(&["width", "xsize"]),
            op.num(&["height", "ysize"]),
        ) else {
            return false;
        };
        let size = self.ctx.meta.oriented_size();
        let (fw, fh) = (f64::from(size.width), f64::from(size.height));
        let inside = fw > 0.0
            && fh > 0.0
            && x >= 0.0
            && y >= 0.0
            && w > 0.0
            && h > 0.0
            && x + w <= fw + 0.5
            && y + h <= fh + 0.5;
        if !inside {
            self.drop_op("Crop", "the crop is outside the photo");
            return true;
        }
        let Some(turns) = turns else {
            self.drop_op(
                "Crop",
                "a crop on a quarter-turned photo is not carried over",
            );
            return true;
        };
        let (nw, nh) = ((w / fw).min(1.0), (h / fh).min(1.0));
        let (mut nx, mut ny) = (x / fw, ((fh - y - h) / fh).max(0.0));
        if turns == 2 {
            // Drawn on the photo as shown; Luxtable crops before turning it.
            (nx, ny) = ((1.0 - nx - nw).max(0.0), (1.0 - ny - nh).max(0.0));
        }
        let rect = NormRect {
            x: nx as f32,
            y: ny as f32,
            w: nw as f32,
            h: nh as f32,
        };
        if rect.w < 0.9999 || rect.h < 0.9999 {
            self.out.adjustments.crop.rect = rect;
            self.done("Crop", Category::Crop);
        }
        true
    }

    /// The quarter turns carried over, `None` when a quarter turn can't be.
    fn rotation(&mut self, degrees: f64) -> Option<u8> {
        let d = degrees.rem_euclid(360.0).round();
        if d.abs() < 0.5 || (d - 360.0).abs() < 0.5 {
            return Some(0);
        }
        if (d - 180.0).abs() < 0.5 {
            self.out.adjustments.crop.quarter_turns = 2;
            self.done("Rotate", Category::Crop);
            return Some(2);
        }
        self.drop_op(
            "Rotate",
            "which way a quarter turn goes isn't recorded in a form Luxtable can read",
        );
        None
    }
}

/// Map one photo's Aperture / iPhoto adjustments (see the module docs).
pub fn map_adjustments(dev: &Develop, ctx: &MapContext) -> Mapped {
    let mut m = M {
        ctx,
        out: Mapped::default(),
    };
    let turns = match dev.get(ROTATION).and_then(Setting::num) {
        Some(r) => m.rotation(r),
        None => Some(0),
    };
    if dev.contains_key(IPHOTO_EDIT) {
        m.drop_op(
            "iPhoto edit",
            "iPhoto kept the edit only as a rendered copy: the original is imported unedited",
        );
    }
    let mut seen: Vec<&str> = Vec::new();
    for op in ops(dev) {
        if IGNORED.contains(&op.name) || !op.enabled() {
            continue;
        }
        let name = display_name(op.name);
        let base = op.name.split('~').next().unwrap_or(op.name);
        if op.masked {
            m.drop_op(
                &name,
                "applied with a brush: brushed adjustments are not carried over",
            );
            continue;
        }
        if seen.contains(&base) {
            m.drop_op(
                &name,
                "only the first of a repeated adjustment is carried over",
            );
            continue;
        }
        seen.push(base);
        let lower = base.to_ascii_lowercase();
        let read = match lower.as_str() {
            "rkexposureoperation" => m.exposure(&op),
            "rkenhanceoperation" => m.enhance(&op),
            "rkshadowhighlightoperation" | "rkhighlightsandshadowsoperation" => {
                m.highlights_shadows(&op)
            }
            "rkwhitebalanceoperation" => m.white_balance(&op),
            "rkstraightenoperation" | "rkstraightencropoperation" => m.straighten(&op),
            "rkcropoperation" => m.crop(&op, turns),
            "rkblackandwhiteoperation" => {
                m.out.adjustments.exposure.saturation = -100.0;
                m.done("Black & White", Category::Color);
                m.drop_op(
                    "Black & White mix",
                    "the red, green and blue mix has no equivalent",
                );
                true
            }
            _ => {
                m.drop_op(&name, reason(base));
                true
            }
        };
        if !read {
            m.drop_op(&name, "its settings weren't recognised");
        }
    }
    m.out.adjustments = m.out.adjustments.sanitized();
    m.out
}

#[cfg(test)]
#[path = "map_tests.rs"]
mod tests;
