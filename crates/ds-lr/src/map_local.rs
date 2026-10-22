//! Lightroom's local adjustments (linear and radial gradients, brushes and
//! the AI masks Darkslide can redo) → Darkslide layers named `Lightroom: …`.
//!
//! Lightroom stores them in four lists: the three classic ones
//! (`GradientBasedCorrections`, `CircularGradientBasedCorrections`,
//! `PaintBasedCorrections`) and the masking model's `MaskGroupBasedCorrections`,
//! whose corrections hold a list of mask components. Coordinates are fractions
//! of the whole (uncropped) photo as Lightroom shows it. Several details are
//! **unconfirmed** (see `docs/LR_FORMAT.md`): they are marked `UNCONFIRMED` here.

use std::collections::BTreeMap;

use ds_core::adjust::{AiTarget, BrushStroke, Layer, LayerAdjustments, Mask, MaskOp, MaskShape};

use crate::map::{Mapper, to_exif_frame, user_turns};
use crate::model::{Category, Fidelity, Setting};

type Table = BTreeMap<String, Setting>;

const MASKS: Category = Category::Masks;

/// The local sliders mapped to a layer's tools.
const MAPPED: [&str; 11] = [
    "LocalExposure2012",
    "LocalContrast2012",
    "LocalHighlights2012",
    "LocalShadows2012",
    "LocalWhites2012",
    "LocalBlacks2012",
    "LocalClarity2012",
    "LocalTexture",
    "LocalDehaze",
    "LocalSaturation",
    "LocalSharpness",
];

/// Most corrections and components read from one photo (a hostile file can list millions).
pub(crate) const MAX_CORRECTIONS: usize = 64;
const MAX_STROKE_POINTS: usize = ds_core::limits::MAX_POINTS;

pub(crate) fn masks(m: &mut Mapper) {
    let k = user_turns(m.ctx).unwrap_or(0);
    let mut layers: Vec<Layer> = Vec::new();
    for (list, kind) in [
        ("GradientBasedCorrections", Kind::Linear),
        ("CircularGradientBasedCorrections", Kind::Radial),
        ("PaintBasedCorrections", Kind::Group),
        ("MaskGroupBasedCorrections", Kind::Group),
    ] {
        let Some(items) = m.get(list).and_then(Setting::list) else {
            continue;
        };
        let items: Vec<Table> = items
            .iter()
            .take(MAX_CORRECTIONS)
            .filter_map(|s| s.table().cloned())
            .collect();
        if m.get(list)
            .and_then(Setting::list)
            .is_some_and(|l| l.len() > MAX_CORRECTIONS)
        {
            m.drop_setting(
                list,
                "More local adjustments than Darkslide reads; the rest are left out",
            );
        }
        for corr in &items {
            if let Some(layer) = correction(m, corr, kind, k) {
                layers.push(layer);
                m.done(list, MASKS, Fidelity::Approximate);
            }
        }
    }
    m.out.adjustments.layers = layers;
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// The correction is itself a linear gradient.
    Linear,
    /// The correction is itself a radial gradient.
    Radial,
    /// The correction lists mask components (`CorrectionMasks`).
    Group,
}

fn num(t: &Table, key: &str) -> Option<f64> {
    t.get(key).and_then(Setting::num)
}

fn flag(t: &Table, key: &str) -> Option<bool> {
    t.get(key).and_then(Setting::flag)
}

fn point(k: i32, x: f64, y: f64) -> Option<[f32; 2]> {
    if !(x.is_finite() && y.is_finite()) {
        return None;
    }
    let (x, y) = to_exif_frame(k, (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)));
    Some([x as f32, y as f32])
}

/// The sliders of one correction. `None`: all at zero (nothing to apply).
fn adjustments(m: &mut Mapper, t: &Table) -> Option<LayerAdjustments> {
    let mut a = LayerAdjustments::default();
    let mut any = false;
    let mut take = |m: &mut Mapper, key: &str, f: &mut dyn FnMut(&mut LayerAdjustments, f32)| {
        if let Some(v) = num(t, key).filter(|v| v.abs() > 1e-9) {
            f(&mut a, v as f32);
            any = true;
            m.done(key, MASKS, Fidelity::Approximate);
        }
    };
    take(m, "LocalExposure2012", &mut |a, v| {
        a.exposure.exposure_ev = v.clamp(-5.0, 5.0);
    });
    take(m, "LocalContrast2012", &mut |a, v| {
        a.exposure.contrast = v.clamp(-100.0, 100.0);
    });
    // Darkslide's positive highlights recover (darken); Lightroom's negative ones do.
    take(m, "LocalHighlights2012", &mut |a, v| {
        a.hdr.highlights = -v.clamp(-100.0, 100.0);
    });
    take(m, "LocalShadows2012", &mut |a, v| {
        a.hdr.shadows = v.clamp(-100.0, 100.0);
    });
    take(m, "LocalWhites2012", &mut |a, v| {
        a.hdr.whites = v.clamp(-100.0, 100.0);
    });
    take(m, "LocalBlacks2012", &mut |a, v| {
        a.hdr.blacks = v.clamp(-100.0, 100.0);
    });
    take(m, "LocalClarity2012", &mut |a, v| {
        a.clarity = v.clamp(-100.0, 100.0);
    });
    take(m, "LocalTexture", &mut |a, v| {
        a.presence.texture = v.clamp(-100.0, 100.0);
    });
    take(m, "LocalDehaze", &mut |a, v| {
        a.presence.dehaze = v.clamp(-100.0, 100.0);
    });
    take(m, "LocalSaturation", &mut |a, v| {
        a.exposure.saturation = v.clamp(-100.0, 100.0);
    });
    take(m, "LocalSharpness", &mut |a, v| {
        a.sharpening = v.clamp(-100.0, 100.0);
    });
    for (key, v) in t {
        if key.starts_with("Local")
            && !MAPPED.contains(&key.as_str())
            && v.num().is_some_and(|n| n.abs() > 1e-9)
        {
            m.drop_setting(
                key,
                "Local adjustment: this slider has no Darkslide equivalent (a relative white balance is not calibrated)",
            );
        }
    }
    any.then_some(a)
}

fn correction(m: &mut Mapper, t: &Table, kind: Kind, k: i32) -> Option<Layer> {
    let adj = adjustments(m, t)?;
    let (shapes, invert, label) = match kind {
        Kind::Linear => (
            linear(m, t, k).into_iter().collect::<Vec<_>>(),
            false,
            "Linear gradient",
        ),
        Kind::Radial => {
            // UNCONFIRMED: `Flipped` true means the effect is inside the ellipse.
            let outside = flag(t, "Flipped") == Some(false);
            (
                radial(m, t, k).into_iter().collect::<Vec<_>>(),
                outside,
                "Radial gradient",
            )
        }
        Kind::Group => components(m, t, k),
    };
    if shapes.is_empty() {
        return None;
    }
    let name = t
        .get("CorrectionName")
        .and_then(Setting::text)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(label);
    Some(Layer {
        name: format!("Lightroom: {name}"),
        enabled: flag(t, "CorrectionActive") != Some(false),
        opacity: num(t, "CorrectionAmount").map_or(1.0, |a| a.clamp(0.0, 1.0)) as f32,
        adjustments: adj,
        mask: Mask {
            shapes,
            invert,
            ..Mask::default()
        },
        ..Layer::default()
    })
}

/// The mask components of a correction, whether the mask is inverted, and a name for the layer.
fn components(m: &mut Mapper, t: &Table, k: i32) -> (Vec<MaskShape>, bool, &'static str) {
    let mut shapes = Vec::new();
    let mut label = "Mask";
    let Some(list) = t.get("CorrectionMasks").and_then(Setting::list) else {
        return (shapes, false, label);
    };
    let mut inverted = 0usize;
    for comp in list.iter().take(MAX_CORRECTIONS).filter_map(Setting::table) {
        let what = comp.get("What").and_then(Setting::text).unwrap_or_default();
        let shape = match what {
            "Mask/Gradient" => {
                label = "Linear gradient";
                linear(m, comp, k)
            }
            "Mask/CircularGradient" => {
                label = "Radial gradient";
                radial(m, comp, k)
            }
            "Mask/Image" => ai(m, comp, &mut label),
            "Mask/Brush" | "Mask/PaintBased" | "" if comp.contains_key("Dabs") => {
                label = "Brush";
                brush(m, comp, k)
            }
            other => {
                let other = if other.is_empty() { "unnamed" } else { other };
                m.drop_setting(
                    &format!("Mask {other}"),
                    "Local adjustment mask type with no Darkslide equivalent (range masks are not imported)",
                );
                None
            }
        };
        let Some(mut shape) = shape else { continue };
        inverted += usize::from(flag(comp, "MaskInverted") == Some(true));
        let op = match num(comp, "MaskBlendMode").map(|v| v as i64) {
            Some(1) => MaskOp::Subtract,
            Some(2) => MaskOp::Intersect,
            _ => MaskOp::Add,
        };
        set_op(&mut shape, op);
        shapes.push(shape);
    }
    // One inversion applies to the whole mask: mixed components can't be expressed.
    if inverted > 0 && inverted < shapes.len() {
        m.drop_setting(
            "MaskInverted",
            "Local adjustment: only some mask components are inverted; Darkslide inverts a whole mask",
        );
    }
    let invert = !shapes.is_empty() && inverted == shapes.len();
    (shapes, invert, label)
}

fn set_op(shape: &mut MaskShape, new: MaskOp) {
    match shape {
        MaskShape::Fill { op }
        | MaskShape::Brush { op, .. }
        | MaskShape::Linear { op, .. }
        | MaskShape::Radial { op, .. }
        | MaskShape::ColorRange { op, .. }
        | MaskShape::Ai { op, .. } => *op = new,
    }
}

fn linear(m: &mut Mapper, t: &Table, k: i32) -> Option<MaskShape> {
    let get = |key| num(t, key);
    let (zx, zy, fx, fy) = (get("ZeroX")?, get("ZeroY")?, get("FullX")?, get("FullY")?);
    let (start, end) = (point(k, fx, fy)?, point(k, zx, zy)?);
    if (zx - fx).abs() < 1e-9 && (zy - fy).abs() < 1e-9 {
        m.drop_setting(
            "GradientBasedCorrections",
            "Linear gradient: its two handles coincide",
        );
        return None;
    }
    Some(MaskShape::Linear {
        op: MaskOp::Add,
        start,
        end,
    })
}

fn radial(m: &mut Mapper, t: &Table, k: i32) -> Option<MaskShape> {
    let get = |key| num(t, key);
    let (top, left, bottom, right) = (get("Top")?, get("Left")?, get("Bottom")?, get("Right")?);
    if !(bottom > top && right > left) {
        m.drop_setting(
            "CircularGradientBasedCorrections",
            "Radial gradient: empty ellipse",
        );
        return None;
    }
    let (w, h) = {
        let (w, h) = (f64::from(m.ctx.meta.width), f64::from(m.ctx.meta.height));
        // Lightroom's frame is the EXIF frame turned `k` quarter turns.
        let (w, h) = if m.ctx.meta.orientation.swaps_axes() {
            (h, w)
        } else {
            (w, h)
        };
        if k % 2 == 1 { (h, w) } else { (w, h) }
    };
    if w < 1.0 || h < 1.0 {
        m.drop_setting(
            "CircularGradientBasedCorrections",
            "Radial gradient: the photo's size is unknown",
        );
        return None;
    }
    let long = w.max(h);
    let center = point(k, f64::midpoint(left, right), f64::midpoint(top, bottom))?;
    let radius = [
        ((right - left) / 2.0 * w / long).clamp(0.0, 1.0) as f32,
        ((bottom - top) / 2.0 * h / long).clamp(0.0, 1.0) as f32,
    ];
    // Positive angles turn the ellipse clockwise; the frame's quarter turns add to it.
    let angle = (num(t, "Angle").unwrap_or(0.0) - 90.0 * f64::from(k)).rem_euclid(360.0);
    let angle_deg = if angle > 180.0 { angle - 360.0 } else { angle };
    Some(MaskShape::Radial {
        op: MaskOp::Add,
        center,
        radius,
        angle_deg: angle_deg as f32,
        feather: (num(t, "Feather").unwrap_or(50.0) / 100.0).clamp(0.0, 1.0) as f32,
    })
}

/// AI masks: UNCONFIRMED subtype numbers (`1` subject, `2` sky, `3` background).
fn ai(m: &mut Mapper, comp: &Table, label: &mut &'static str) -> Option<MaskShape> {
    let (target, name) = match num(comp, "MaskSubType").map(|v| v as i64) {
        Some(1) => (AiTarget::Subject, "Subject"),
        Some(2) => (AiTarget::Sky, "Sky"),
        Some(3) => (AiTarget::Background, "Background"),
        other => {
            let kind = other.map_or_else(|| "bitmap".to_owned(), |n| format!("type {n}"));
            m.drop_setting(
                &format!("Mask/Image {kind}"),
                "Local adjustment: this AI or brush-bitmap mask can't be recreated",
            );
            return None;
        }
    };
    *label = name;
    Some(MaskShape::Ai {
        op: MaskOp::Add,
        target,
        feather: 0.0,
        person: None,
    })
}

/// A brush as strokes of `m x y` (start) and `d x y` (continue) dabs. UNCONFIRMED format.
fn brush(m: &mut Mapper, comp: &Table, k: i32) -> Option<MaskShape> {
    let dabs = comp.get("Dabs").and_then(Setting::list)?;
    let size = (num(comp, "Radius").unwrap_or(0.05) * 2.0).clamp(0.001, 0.3) as f32;
    let flow = num(comp, "Flow").unwrap_or(1.0).clamp(0.0, 1.0) as f32;
    let hardness = num(comp, "CenterWeight").unwrap_or(0.5).clamp(0.0, 1.0) as f32;
    let mut strokes: Vec<BrushStroke> = Vec::new();
    let mut total = 0usize;
    for dab in dabs {
        let Some(text) = dab.text() else { continue };
        let mut parts = text.split_whitespace();
        let Some(kind) = parts.next() else { continue };
        let xy: Vec<f64> = parts.filter_map(crate::model::parse_num).collect();
        let (Some(&x), Some(&y)) = (xy.first(), xy.get(1)) else {
            continue;
        };
        let Some(p) = point(k, x, y) else { continue };
        total += 1;
        if total > MAX_STROKE_POINTS {
            m.drop_setting(
                "Dabs",
                "Brush: more points than Darkslide reads; the rest are left out",
            );
            break;
        }
        if kind == "m" || strokes.is_empty() {
            strokes.push(BrushStroke {
                points: vec![p],
                size,
                hardness,
                flow,
                ..BrushStroke::default()
            });
        } else if let Some(s) = strokes.last_mut() {
            s.points.push(p);
        }
    }
    if strokes.is_empty() {
        return None;
    }
    Some(MaskShape::Brush {
        op: MaskOp::Add,
        strokes,
        feather: 0.0,
    })
}
