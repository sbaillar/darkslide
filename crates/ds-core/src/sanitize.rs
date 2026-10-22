//! Bringing an [`Adjustments`] stack back into its documented domain.
//!
//! Every non-finite value becomes that field's default and every value is
//! clamped to the range documented on its field. Call at every write boundary
//! (sidecars, presets) so a stray NaN from arithmetic can never be persisted:
//! JSON cannot represent it, and a `null` would make the file unreadable to a
//! strict reader.

use crate::adjust::{
    Adjustments, BrushStroke, ColorBalance, ColorEditor, ColorRange, Crop, Curve, CurvePoints,
    Detail, Exposure, Hdr, Layer, Levels, LumaRange, Mask, MaskShape, NoiseReduction, Presence,
    Removal, SkinTone, Spot, Vignette, Wheel, WhiteBalance, WhiteBalanceDelta,
};
use crate::limits;

/// Neutral daylight, used when a custom temperature is not a number.
const FALLBACK_TEMP_K: f32 = 5500.0;

/// Accumulates whether anything changed.
struct Fixer(bool);

impl Fixer {
    /// Non-finite → `default`; otherwise clamp into `lo..=hi`.
    fn num(&mut self, v: &mut f32, default: f32, lo: f32, hi: f32) {
        let next = if v.is_finite() {
            v.clamp(lo, hi)
        } else {
            default
        };
        if next.to_bits() != v.to_bits() {
            *v = next;
            self.0 = true;
        }
    }

    /// Angles in degrees wrap into `0..360`.
    fn hue(&mut self, v: &mut f32, default: f32) {
        let next = if v.is_finite() {
            v.rem_euclid(360.0)
        } else {
            default
        };
        // rem_euclid can round up to exactly 360.0 for tiny negatives.
        let next = if next >= 360.0 { 0.0 } else { next };
        if next.to_bits() != v.to_bits() {
            *v = next;
            self.0 = true;
        }
    }

    fn slider(&mut self, v: &mut f32) {
        self.num(v, 0.0, -100.0, 100.0);
    }

    fn unit(&mut self, v: &mut f32, default: f32) {
        self.num(v, default, 0.0, 1.0);
    }

    /// Normalized points: drop points with a non-finite coordinate, clamp the rest.
    /// Drop everything past `max` entries.
    fn cap<T>(&mut self, v: &mut Vec<T>, max: usize) {
        if v.len() > max {
            v.truncate(max);
            self.0 = true;
        }
    }

    fn points(&mut self, pts: &mut CurvePoints) {
        self.cap(pts, limits::MAX_POINTS);
        let before = pts.len();
        pts.retain(|p| p.iter().all(|c| c.is_finite()));
        self.0 |= pts.len() != before;
        for p in pts.iter_mut() {
            for c in p.iter_mut() {
                self.unit(c, 0.0);
            }
        }
    }

    /// A normalized point that has no natural default: non-finite → center.
    fn point(&mut self, p: &mut [f32; 2]) {
        for c in p.iter_mut() {
            self.num(c, 0.5, -10.0, 10.0);
        }
    }

    fn wb(&mut self, wb: &mut WhiteBalance) {
        if let WhiteBalance::Custom { temp_k, tint } = wb {
            self.num(temp_k, FALLBACK_TEMP_K, 2000.0, 50000.0);
            self.num(tint, 0.0, -150.0, 150.0);
        }
    }

    fn exposure(&mut self, e: &mut Exposure) {
        self.num(&mut e.exposure_ev, 0.0, -5.0, 5.0);
        self.slider(&mut e.contrast);
        self.slider(&mut e.brightness);
        self.slider(&mut e.saturation);
    }

    fn presence(&mut self, p: &mut Presence) {
        self.slider(&mut p.vibrance);
        self.slider(&mut p.texture);
        self.slider(&mut p.dehaze);
    }

    fn spot(&mut self, sp: &mut Spot) {
        let d = Spot::default();
        self.point(&mut sp.center);
        self.point(&mut sp.source);
        self.num(&mut sp.radius, d.radius, 0.0, 0.5);
        self.unit(&mut sp.feather, d.feather);
        self.unit(&mut sp.opacity, d.opacity);
    }

    fn hdr(&mut self, h: &mut Hdr) {
        self.slider(&mut h.highlights);
        self.slider(&mut h.shadows);
        self.slider(&mut h.whites);
        self.slider(&mut h.blacks);
    }

    fn levels(&mut self, l: &mut Levels) {
        let d = Levels::default();
        self.unit(&mut l.in_black, d.in_black);
        self.unit(&mut l.in_white, d.in_white);
        self.num(&mut l.gamma, d.gamma, 0.1, 10.0);
        self.unit(&mut l.out_black, d.out_black);
        self.unit(&mut l.out_white, d.out_white);
    }

    fn curve(&mut self, c: &mut Curve) {
        for pts in [
            &mut c.rgb,
            &mut c.luma,
            &mut c.red,
            &mut c.green,
            &mut c.blue,
        ] {
            self.points(pts);
        }
    }

    fn wheel(&mut self, w: &mut Wheel) {
        self.hue(&mut w.hue, 0.0);
        self.unit(&mut w.sat, 0.0);
        self.num(&mut w.lum, 0.0, -1.0, 1.0);
    }

    fn color_balance(&mut self, cb: &mut ColorBalance) {
        for w in [
            &mut cb.master,
            &mut cb.shadows,
            &mut cb.midtones,
            &mut cb.highlights,
        ] {
            self.wheel(w);
        }
    }

    fn range(&mut self, r: &mut ColorRange, default_hue: f32) {
        let d = ColorRange::default();
        self.hue(&mut r.center_hue, default_hue);
        self.num(&mut r.width, d.width, 0.0, 360.0);
        self.unit(&mut r.smoothness, d.smoothness);
        self.num(&mut r.hue_shift, 0.0, -30.0, 30.0);
        self.slider(&mut r.saturation);
        self.slider(&mut r.lightness);
    }

    fn skin(&mut self, s: &mut SkinTone) {
        self.range(&mut s.range, 0.0);
        for u in [
            &mut s.hue_uniformity,
            &mut s.saturation_uniformity,
            &mut s.lightness_uniformity,
        ] {
            self.num(u, 0.0, 0.0, 100.0);
        }
    }

    fn color_editor(&mut self, ce: &mut ColorEditor) {
        let defaults = ColorEditor::default();
        for (r, d) in ce.basic.iter_mut().zip(defaults.basic) {
            self.range(r, d.center_hue);
        }
        self.cap(&mut ce.advanced, limits::MAX_SHAPES);
        for r in &mut ce.advanced {
            self.range(r, 0.0);
        }
        if let Some(s) = &mut ce.skin {
            self.skin(s);
        }
    }

    fn detail(&mut self, dt: &mut Detail) {
        let d = Detail::default();
        self.slider(&mut dt.clarity);
        self.slider(&mut dt.structure);
        self.num(&mut dt.sharpen_amount, d.sharpen_amount, 0.0, 1000.0);
        self.num(&mut dt.sharpen_radius, d.sharpen_radius, 0.2, 2.5);
        self.num(&mut dt.sharpen_threshold, d.sharpen_threshold, 0.0, 10.0);
    }

    fn noise(&mut self, n: &mut NoiseReduction) {
        let d = NoiseReduction::default();
        self.num(&mut n.luminance, d.luminance, 0.0, 100.0);
        self.num(&mut n.color, d.color, 0.0, 100.0);
        self.num(&mut n.detail, d.detail, 0.0, 100.0);
        self.num(&mut n.ai_strength, d.ai_strength, 0.0, 100.0);
    }

    fn vignette(&mut self, v: &mut Vignette) {
        let d = Vignette::default();
        self.num(&mut v.amount_ev, d.amount_ev, -3.0, 3.0);
        self.unit(&mut v.midpoint, d.midpoint);
    }

    fn crop(&mut self, c: &mut Crop) {
        let rect = c.rect.clamped();
        if rect != c.rect {
            c.rect = rect;
            self.0 = true;
        }
        self.num(&mut c.angle_deg, 0.0, -45.0, 45.0);
        if c.quarter_turns > 3 {
            c.quarter_turns %= 4;
            self.0 = true;
        }
    }

    fn luma(&mut self, l: &mut LumaRange) {
        let d = LumaRange::default();
        self.unit(&mut l.low, d.low);
        self.unit(&mut l.high, d.high);
        self.unit(&mut l.low_falloff, d.low_falloff);
        self.unit(&mut l.high_falloff, d.high_falloff);
    }

    fn stroke(&mut self, s: &mut BrushStroke) {
        let d = BrushStroke::default();
        self.points(&mut s.points);
        self.num(&mut s.size, d.size, 0.0, 10.0);
        self.unit(&mut s.hardness, d.hardness);
        self.unit(&mut s.flow, d.flow);
    }

    fn removal(&mut self, r: &mut Removal) {
        self.cap(&mut r.strokes, limits::MAX_STROKES);
        for s in &mut r.strokes {
            self.stroke(s);
            // The Remove brush goes up to `MAX_REMOVAL_SIZE` (a wider one is a hand-edited
            // sidecar, and would make the hole most of the frame).
            let d = BrushStroke::default().size;
            self.num(&mut s.size, d, 0.0, crate::adjust::MAX_REMOVAL_SIZE);
        }
        let d = Removal::default();
        self.num(
            &mut r.feather,
            d.feather,
            0.0,
            crate::adjust::MAX_REMOVAL_FEATHER,
        );
    }

    fn mask(&mut self, m: &mut Mask) {
        self.num(&mut m.feather, 0.0, 0.0, crate::adjust::MAX_PLANE_FEATHER);
        if let Some(l) = &mut m.luma_range {
            self.luma(l);
        }
        if let Some(r) = &mut m.color_range {
            self.range(r, 0.0);
        }
        self.cap(&mut m.shapes, limits::MAX_SHAPES);
        for shape in &mut m.shapes {
            match shape {
                MaskShape::Fill { .. } => {}
                MaskShape::Ai { feather, .. } => self.num(feather, 0.0, 0.0, 10.0),
                MaskShape::Brush {
                    strokes, feather, ..
                } => {
                    self.cap(strokes, limits::MAX_STROKES);
                    for s in strokes.iter_mut() {
                        self.stroke(s);
                    }
                    self.num(feather, 0.0, 0.0, crate::adjust::MAX_PLANE_FEATHER);
                }
                MaskShape::Linear { start, end, .. } => {
                    self.point(start);
                    self.point(end);
                }
                MaskShape::Radial {
                    center,
                    radius,
                    angle_deg,
                    feather,
                    ..
                } => {
                    self.point(center);
                    for r in radius.iter_mut() {
                        self.num(r, 0.25, 0.0, 10.0);
                    }
                    self.num(angle_deg, 0.0, -360.0, 360.0);
                    self.num(feather, 0.0, 0.0, 10.0);
                }
                MaskShape::ColorRange { range, feather, .. } => {
                    self.range(range, 0.0);
                    self.num(feather, 0.0, 0.0, crate::adjust::MAX_PLANE_FEATHER);
                }
            }
        }
    }

    fn layer(&mut self, l: &mut Layer) {
        self.unit(&mut l.opacity, 1.0);
        let a = &mut l.adjustments;
        if let Some(WhiteBalanceDelta { temp_k, tint }) = &mut a.white_balance {
            self.num(temp_k, 0.0, -48000.0, 48000.0);
            self.num(tint, 0.0, -150.0, 150.0);
        }
        self.exposure(&mut a.exposure);
        self.presence(&mut a.presence);
        self.hdr(&mut a.hdr);
        self.curve(&mut a.curve);
        self.color_balance(&mut a.color_balance);
        for r in &mut a.color_editor {
            self.range(r, 0.0);
        }
        self.slider(&mut a.clarity);
        self.slider(&mut a.structure);
        self.slider(&mut a.sharpening);
        self.mask(&mut l.mask);
    }
}

impl Adjustments {
    /// Replace non-finite values with defaults and clamp everything to its
    /// documented range. Returns whether anything changed. Idempotent.
    pub fn sanitize(&mut self) -> bool {
        let mut f = Fixer(false);
        f.wb(&mut self.white_balance);
        f.exposure(&mut self.exposure);
        f.presence(&mut self.presence);
        f.hdr(&mut self.hdr);
        f.levels(&mut self.levels);
        f.curve(&mut self.curve);
        f.color_balance(&mut self.color_balance);
        f.color_editor(&mut self.color_editor);
        f.detail(&mut self.detail);
        f.noise(&mut self.noise);
        f.vignette(&mut self.vignette);
        let lens_default = crate::adjust::LensCorrection::default();
        f.unit(&mut self.lens.distortion, lens_default.distortion);
        f.unit(&mut self.lens.light_falloff, lens_default.light_falloff);
        f.num(&mut self.lens.keystone_vertical, 0.0, -45.0, 45.0);
        f.num(&mut self.lens.keystone_horizontal, 0.0, -45.0, 45.0);
        f.crop(&mut self.crop);
        f.cap(&mut self.layers, limits::MAX_LAYERS);
        f.cap(&mut self.spots, limits::MAX_SPOTS);
        f.cap(&mut self.removals, limits::MAX_SPOTS);
        for l in &mut self.layers {
            f.layer(l);
        }
        for sp in &mut self.spots {
            f.spot(sp);
        }
        for r in &mut self.removals {
            f.removal(r);
        }
        f.0 |= self.effects.sanitize();
        if let Some(look) = &mut self.look {
            f.0 |= look.sanitize();
        }
        while self.person_boxes.len() > limits::MAX_PERSON_BOXES {
            self.person_boxes.pop_last();
            f.0 = true;
        }
        // Person boxes: inside the frame and not empty, else dropped.
        self.person_boxes.retain(|_, b| {
            let ok = b.iter().all(|v| v.is_finite());
            if ok {
                for v in b.iter_mut() {
                    f.num(v, 0.0, 0.0, 1.0);
                }
            }
            let keep = ok && b[2] > b[0] && b[3] > b[1];
            f.0 |= !keep;
            keep
        });
        f.0
    }

    /// [`Adjustments::sanitize`] by value.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.sanitize();
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::too_many_lines)]

    use super::*;
    use crate::adjust::{AiTarget, LayerAdjustments, MaskOp};
    use crate::geom::NormRect;

    const NAN: f32 = f32::NAN;
    const INF: f32 = f32::INFINITY;

    /// Every float in the stack poisoned with `v`.
    fn poisoned(v: f32) -> Adjustments {
        let range = ColorRange {
            center_hue: v,
            width: v,
            smoothness: v,
            hue_shift: v,
            saturation: v,
            lightness: v,
        };
        let wheel = Wheel {
            hue: v,
            sat: v,
            lum: v,
        };
        let curve = Curve {
            rgb: vec![[v, 0.5], [0.2, 0.3]],
            luma: vec![[v, v]],
            red: vec![],
            green: vec![[0.5, v]],
            blue: vec![[1.0, 1.0]],
        };
        let exposure = Exposure {
            exposure_ev: v,
            contrast: v,
            brightness: v,
            saturation: v,
        };
        let hdr = Hdr {
            highlights: v,
            shadows: v,
            whites: v,
            blacks: v,
        };
        let cb = ColorBalance {
            master: wheel,
            shadows: wheel,
            midtones: wheel,
            highlights: wheel,
        };
        Adjustments {
            profile: crate::adjust::Profile::Vivid,
            white_balance: WhiteBalance::Custom { temp_k: v, tint: v },
            exposure,
            presence: Presence {
                vibrance: v,
                texture: v,
                dehaze: v,
            },
            hdr,
            levels: Levels {
                in_black: v,
                in_white: v,
                gamma: v,
                out_black: v,
                out_white: v,
            },
            curve: curve.clone(),
            color_balance: cb,
            color_editor: ColorEditor {
                basic: [range; 8],
                advanced: vec![range],
                skin: Some(SkinTone {
                    range,
                    hue_uniformity: v,
                    saturation_uniformity: v,
                    lightness_uniformity: v,
                }),
            },
            detail: Detail {
                clarity: v,
                structure: v,
                sharpen_amount: v,
                sharpen_radius: v,
                sharpen_threshold: v,
            },
            noise: NoiseReduction {
                luminance: v,
                color: v,
                detail: v,
                ai_denoise: false,
                ai_strength: v,
            },
            vignette: Vignette {
                amount_ev: v,
                midpoint: v,
            },
            lens: crate::adjust::LensCorrection {
                distortion: v,
                light_falloff: v,
                keystone_vertical: v,
                keystone_horizontal: v,
                profile: Some("lens".into()),
                ..Default::default()
            },
            crop: Crop {
                rect: NormRect {
                    x: v,
                    y: v,
                    w: v,
                    h: v,
                },
                angle_deg: v,
                quarter_turns: 7,
                aspect: Some(crate::adjust::Aspect { long: 3, short: 2 }),
                ..Crop::default()
            },
            layers: vec![Layer {
                opacity: v,
                adjustments: LayerAdjustments {
                    white_balance: Some(WhiteBalanceDelta { temp_k: v, tint: v }),
                    exposure,
                    hdr,
                    curve,
                    color_balance: cb,
                    color_editor: vec![range],
                    clarity: v,
                    structure: v,
                    presence: Presence {
                        vibrance: v,
                        texture: v,
                        dehaze: v,
                    },
                    sharpening: v,
                },
                mask: Mask {
                    shapes: vec![
                        MaskShape::Fill { op: MaskOp::Add },
                        MaskShape::Ai {
                            op: MaskOp::Add,
                            target: AiTarget::Sky,
                            feather: v,
                            person: None,
                        },
                        MaskShape::Brush {
                            op: MaskOp::Add,
                            strokes: vec![BrushStroke {
                                points: vec![[v, 0.1], [0.2, 0.2]],
                                size: v,
                                hardness: v,
                                flow: v,
                                erase: false,
                                auto_mask: true,
                            }],
                            feather: v,
                        },
                        MaskShape::Linear {
                            op: MaskOp::Add,
                            start: [v, v],
                            end: [v, 0.0],
                        },
                        MaskShape::Radial {
                            op: MaskOp::Add,
                            center: [v, v],
                            radius: [v, v],
                            angle_deg: v,
                            feather: v,
                        },
                        MaskShape::ColorRange {
                            op: MaskOp::Add,
                            range,
                            feather: v,
                        },
                    ],
                    invert: false,
                    feather: v,
                    luma_range: Some(LumaRange {
                        low: v,
                        high: v,
                        low_falloff: v,
                        high_falloff: v,
                    }),
                    color_range: Some(range),
                },
                ..Layer::default()
            }],
            spots: vec![Spot {
                kind: crate::adjust::SpotKind::Clone,
                center: [v, v],
                radius: v,
                source: [v, v],
                feather: v,
                opacity: v,
            }],
            removals: vec![crate::adjust::Removal {
                strokes: vec![BrushStroke {
                    points: vec![[v, v], [0.2, 0.2]],
                    size: v,
                    hardness: v,
                    flow: v,
                    ..BrushStroke::default()
                }],
                feather: v,
            }],
            person_boxes: [(0, [v, 0.0, 1.0, 1.0])].into(),
            effects: crate::effects::Effects {
                lens_blur: crate::effects::LensBlur {
                    amount: v,
                    focus: Some([v, 0.5]),
                    focal_range: v,
                    highlight_boost: v,
                    plane: crate::effects::FocusBand {
                        center: [v, v],
                        angle_deg: v,
                        width: v,
                        feather: v,
                    },
                    ..Default::default()
                },
                glow: crate::effects::Glow { amount: v, size: v },
                grain: crate::effects::Grain {
                    amount: v,
                    size: v,
                    roughness: v,
                },
                halation: crate::effects::Halation { amount: v },
            },
            look: Some(crate::effects::AppliedLook {
                id: "look".into(),
                intensity: v,
                from: [("k".to_string(), v)].into(),
                to: [("k".to_string(), 1.0)].into(),
            }),
        }
    }

    /// Collect every float by serializing (sanitized values are all finite).
    fn all_finite(a: &Adjustments) -> bool {
        let json = serde_json::to_string(a).unwrap();
        !json.contains("null")
    }

    #[test]
    fn nan_becomes_field_defaults() {
        let mut a = poisoned(NAN);
        assert!(a.sanitize());
        assert!(all_finite(&a));
        let d = Adjustments::default();
        assert_eq!(
            a.white_balance,
            WhiteBalance::Custom {
                temp_k: 5500.0,
                tint: 0.0
            }
        );
        assert_eq!(a.exposure, d.exposure);
        assert_eq!(a.hdr, d.hdr);
        assert_eq!(a.levels, d.levels);
        assert_eq!(a.color_balance, d.color_balance);
        assert_eq!(
            a.color_editor.basic, d.color_editor.basic,
            "band hues restored per band"
        );
        assert_eq!(a.detail, d.detail);
        assert_eq!(a.noise, d.noise);
        assert_eq!(a.vignette, d.vignette);
        assert_eq!(
            a.lens,
            crate::adjust::LensCorrection {
                profile: Some("lens".into()),
                ..d.lens
            }
        );
        assert_eq!(a.crop.rect, NormRect::FULL);
        assert_eq!(a.crop.angle_deg, 0.0);
        assert_eq!(a.crop.quarter_turns, 3);
        assert_eq!(a.curve.rgb, vec![[0.2, 0.3]], "points with NaN dropped");
        assert_eq!(a.curve.luma.len(), 0);
        let l = &a.layers[0];
        assert_eq!(l.opacity, 1.0);
        assert_eq!(l.mask.luma_range, Some(LumaRange::default()));
        let MaskShape::Brush { strokes, .. } = &l.mask.shapes[2] else {
            panic!()
        };
        assert_eq!(strokes[0].points, vec![[0.2, 0.2]]);
        assert_eq!(
            (strokes[0].size, strokes[0].hardness, strokes[0].flow),
            (0.05, 0.5, 1.0)
        );
        let MaskShape::Radial { center, radius, .. } = l.mask.shapes[4] else {
            panic!()
        };
        assert_eq!((center, radius), ([0.5, 0.5], [0.25, 0.25]));
        let skin = a.color_editor.skin.unwrap();
        assert_eq!(skin.range, ColorRange::default());
        assert_eq!(a.presence, d.presence);
        let la = &l.adjustments;
        assert_eq!((la.presence, la.sharpening), (Presence::default(), 0.0));
        assert_eq!(l.mask.color_range, Some(ColorRange::default()));
        for shape in &l.mask.shapes {
            if let MaskShape::Ai { feather, .. }
            | MaskShape::Brush { feather, .. }
            | MaskShape::ColorRange { feather, .. } = shape
            {
                assert_eq!(*feather, 0.0);
            }
        }
        let sp = Spot::default();
        assert_eq!(
            a.spots[0],
            Spot {
                kind: crate::adjust::SpotKind::Clone,
                center: [0.5, 0.5],
                source: [0.5, 0.5],
                ..sp
            }
        );
        let r = &a.removals[0];
        assert_eq!(r.feather, crate::adjust::Removal::default().feather);
        assert_eq!(r.strokes[0].points, vec![[0.2, 0.2]]);
        assert_eq!(r.strokes[0].size, 0.05);
        let fx = crate::effects::Effects::default();
        assert_eq!(
            a.effects,
            crate::effects::Effects {
                lens_blur: crate::effects::LensBlur {
                    focus: None,
                    ..fx.lens_blur
                },
                ..fx
            }
        );
        let look = a.look.as_ref().unwrap();
        assert_eq!((look.intensity, look.from.len()), (1.0, 0));
        assert!(!a.sanitize(), "idempotent");
    }

    #[test]
    fn infinities_clamp_to_range_ends() {
        let a = poisoned(INF).sanitized();
        assert!(all_finite(&a));
        assert_eq!(
            a.exposure.exposure_ev, 0.0,
            "non-finite is replaced, not clamped"
        );
        let mut b = Adjustments::default();
        b.exposure.exposure_ev = 9.0;
        b.exposure.contrast = -300.0;
        b.levels.gamma = 0.0;
        b.detail.sharpen_radius = 7.0;
        b.white_balance = WhiteBalance::Custom {
            temp_k: 100.0,
            tint: 999.0,
        };
        b.color_balance.master.hue = -30.0;
        b.color_editor.advanced.push(ColorRange {
            center_hue: 725.0,
            ..ColorRange::default()
        });
        b.crop.angle_deg = 90.0;
        b.presence.vibrance = 250.0;
        b.spots.push(Spot {
            radius: 3.0,
            opacity: -1.0,
            ..Spot::default()
        });
        assert!(b.sanitize());
        assert_eq!(b.presence.vibrance, 100.0);
        assert_eq!((b.spots[0].radius, b.spots[0].opacity), (0.5, 0.0));
        assert_eq!(b.exposure.exposure_ev, 5.0);
        assert_eq!(b.exposure.contrast, -100.0);
        assert_eq!(b.levels.gamma, 0.1);
        assert_eq!(b.detail.sharpen_radius, 2.5);
        assert_eq!(
            b.white_balance,
            WhiteBalance::Custom {
                temp_k: 2000.0,
                tint: 150.0
            }
        );
        assert_eq!(b.color_balance.master.hue, 330.0);
        assert_eq!(b.color_editor.advanced[0].center_hue, 5.0);
        assert_eq!(b.crop.angle_deg, 45.0);
        let mut h = 0.0f32;
        let mut f = Fixer(false);
        f.hue(&mut h, 0.0);
        assert!(!f.0);
        let mut tiny = -1e-9f32;
        f.hue(&mut tiny, 0.0);
        assert!((0.0..360.0).contains(&tiny));
    }

    #[test]
    fn defaults_and_valid_values_are_untouched() {
        let mut d = Adjustments::default();
        assert!(!d.sanitize());
        assert_eq!(d, Adjustments::default());
        let mut a = poisoned(0.3);
        a.white_balance = WhiteBalance::Custom {
            temp_k: 5000.0,
            tint: 0.3,
        };
        // Output-space feathers stop at MAX_PLANE_FEATHER.
        let m = &mut a.layers[0].mask;
        m.feather = 0.05;
        for shape in &mut m.shapes {
            if let MaskShape::Brush { feather, .. } | MaskShape::ColorRange { feather, .. } = shape
            {
                *feather = 0.05;
            }
        }
        a.removals[0].feather = crate::adjust::MAX_REMOVAL_FEATHER;
        let before = a.clone();
        a.crop.quarter_turns = 1;
        let mut b = a.clone();
        assert!(!b.sanitize());
        assert_eq!(b, a);
        assert_ne!(before.crop.quarter_turns, b.crop.quarter_turns);
    }

    #[test]
    fn output_space_feathers_stop_at_the_plane_limit() {
        use crate::adjust::MAX_PLANE_FEATHER;
        let mut a = poisoned(0.3);
        assert!(a.sanitize());
        let m = &a.layers[0].mask;
        assert_eq!(m.feather, MAX_PLANE_FEATHER);
        for shape in &m.shapes {
            match shape {
                MaskShape::Brush { feather, .. } | MaskShape::ColorRange { feather, .. } => {
                    assert_eq!(*feather, MAX_PLANE_FEATHER);
                }
                // E-frame rasters and a radial's edge: any width renders the same.
                MaskShape::Ai { feather, .. } | MaskShape::Radial { feather, .. } => {
                    assert_eq!(*feather, 0.3);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn null_floats_in_json_read_as_defaults() {
        let json = r#"{
            "exposure": {"exposure_ev": null, "contrast": 12},
            "white_balance": {"mode": "custom", "temp_k": null, "tint": 3},
            "levels": {"gamma": null},
            "curve": {"rgb": [[null, 0.5], [0.25, 0.75]]},
            "crop": {"rect": {"x": null, "y": 0, "w": null, "h": 0.5}},
            "layers": [{"opacity": null, "mask": {"shapes": [
                {"kind": "linear", "op": "add", "start": [null, 0.1], "end": [1, 1]}
            ]}}]
        }"#;
        let a: Adjustments = serde_json::from_str(json).unwrap();
        assert_eq!(a.exposure.exposure_ev, 0.0);
        assert_eq!(a.exposure.contrast, 12.0);
        assert_eq!(
            a.white_balance,
            WhiteBalance::Custom {
                temp_k: 5500.0,
                tint: 3.0
            }
        );
        assert_eq!(a.levels.gamma, 1.0);
        assert_eq!(a.curve.rgb, vec![[0.25, 0.75]]);
        assert_eq!(
            a.crop.rect,
            NormRect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 0.5
            }
        );
        assert_eq!(a.layers[0].opacity, 1.0);
        let MaskShape::Linear { start, .. } = a.layers[0].mask.shapes[0] else {
            panic!()
        };
        assert_eq!(start, [0.5, 0.1]);
        // A NaN written by serde_json round-trips to the default instead of failing.
        let mut bad = Adjustments::default();
        bad.exposure.exposure_ev = NAN;
        let back: Adjustments =
            serde_json::from_str(&serde_json::to_string(&bad).unwrap()).unwrap();
        assert_eq!(back, Adjustments::default());
    }

    #[test]
    fn hostile_counts_are_capped() {
        let mut a = Adjustments {
            layers: vec![Layer::default(); limits::MAX_LAYERS + 5],
            spots: vec![Spot::default(); limits::MAX_SPOTS + 1],
            removals: vec![
                Removal {
                    strokes: vec![BrushStroke::default(); limits::MAX_STROKES + 1],
                    ..Removal::default()
                };
                limits::MAX_SPOTS + 1
            ],
            ..Adjustments::default()
        };
        a.curve.rgb = vec![[0.5, 0.5]; limits::MAX_POINTS + 1];
        for i in 0..=limits::MAX_PERSON_BOXES as u32 {
            a.person_boxes.insert(i, [0.1, 0.1, 0.5, 0.5]);
        }
        assert!(a.sanitize());
        assert_eq!(a.layers.len(), limits::MAX_LAYERS);
        assert_eq!(a.spots.len(), limits::MAX_SPOTS);
        assert_eq!(a.removals.len(), limits::MAX_SPOTS);
        assert_eq!(a.removals[0].strokes.len(), limits::MAX_STROKES);
        assert_eq!(a.curve.rgb.len(), limits::MAX_POINTS);
        assert_eq!(a.person_boxes.len(), limits::MAX_PERSON_BOXES);
        assert!(!a.sanitize(), "idempotent");
    }
}
