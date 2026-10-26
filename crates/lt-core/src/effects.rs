//! Stylistic effects (the "Effects" group): lens blur (depth or tilt-shift), glow, film
//! grain and halation, plus the record of a one-touch look the app applied.
//!
//! Every struct's `Default` is the identity (`amount` 0 renders nothing). Values are in UI
//! units; `lt-pipeline` owns the mapping to math. All fields are additive to sidecar
//! schema 1 (`#[serde(default)]` everywhere).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Lens blur, glow, grain and halation.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Effects {
    pub lens_blur: LensBlur,
    pub glow: Glow,
    pub grain: Grain,
    pub halation: Halation,
}

impl Effects {
    /// True when nothing renders.
    pub fn is_identity(&self) -> bool {
        self.lens_blur.amount <= 0.0
            && self.glow.amount <= 0.0
            && self.grain.amount <= 0.0
            && self.halation.amount <= 0.0
    }
}

/// Where the lens blur's sharp zone comes from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BlurMode {
    /// A depth map of the photo (the app's depth model, refined by its subject masks).
    #[default]
    Depth,
    /// A planar focus band across the frame (tilt-shift miniature); no depth needed.
    Plane,
    /// A mode this build doesn't know (a newer build's), kept verbatim so a save never
    /// drops it. The lens blur renders nothing with it.
    Other(serde_json::Value),
}

/// Shape of the out-of-focus highlights (the aperture).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BokehShape {
    #[default]
    Circle,
    /// A six-bladed aperture (hexagonal highlights).
    Blades,
    /// Mechanical vignetting: highlights turn into cat's eyes toward the corners, swirling
    /// around the centre.
    CatEye,
    /// A shape this build doesn't know (a newer build's), kept verbatim. It renders as
    /// [`BokehShape::Circle`].
    Other(serde_json::Value),
}

/// A choice read leniently: a known name, else the value kept as it was (an older build
/// must still load a newer build's sidecar, and write it back unchanged).
macro_rules! lenient_choice {
    ($t:ident { $($name:literal => $v:ident),+ $(,)? }) => {
        impl Serialize for $t {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                match self {
                    $(Self::$v => s.serialize_str($name),)+
                    Self::Other(v) => v.serialize(s),
                }
            }
        }

        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let v = serde_json::Value::deserialize(d)?;
                Ok(match v.as_str() {
                    $(Some($name) => Self::$v,)+
                    _ => Self::Other(v),
                })
            }
        }
    };
}

lenient_choice!(BlurMode { "depth" => Depth, "plane" => Plane });
lenient_choice!(BokehShape { "circle" => Circle, "blades" => Blades, "cat_eye" => CatEye });

/// A depth-dependent disc ("lens") blur with occlusion-aware gathering: the in-focus
/// zone is left untouched and never bleeds into the blurred background.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LensBlur {
    /// 0..=100: the largest blur (the farthest depth), f/16 at 1 to f/1.2 at 100. 0 = off.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub amount: f32,
    pub mode: BlurMode,
    /// The E-normalized point whose depth is in focus; `None` focuses on the subject.
    #[serde(
        default,
        deserialize_with = "focus",
        skip_serializing_if = "Option::is_none"
    )]
    pub focus: Option<[f32; 2]>,
    /// 0..=100: the depth range kept sharp around the focus.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub focal_range: f32,
    /// 0..=100: how much bright out-of-focus highlights bloom into bokeh balls.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub highlight_boost: f32,
    pub shape: BokehShape,
    /// The sharp band of [`BlurMode::Plane`].
    pub plane: FocusBand,
}

impl Default for LensBlur {
    fn default() -> Self {
        Self {
            amount: 0.0,
            mode: BlurMode::Depth,
            focus: None,
            focal_range: 20.0,
            highlight_boost: 30.0,
            shape: BokehShape::Circle,
            plane: FocusBand::default(),
        }
    }
}

/// `null` or a malformed point (not two numbers) is "focus on the subject"; null
/// coordinates become NaN and are dropped by `sanitize`.
fn focus<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<[f32; 2]>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(Option::<[Option<f32>; 2]>::deserialize(&v)
        .ok()
        .flatten()
        .map(|p| p.map(|v| v.unwrap_or(f32::NAN))))
}

/// A malformed look record (not an object of the right shape) is no record: the rest
/// of the edit still loads.
pub(crate) fn look<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<AppliedLook>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(Option::<AppliedLook>::deserialize(&v).ok().flatten())
}

/// A straight sharp band (tilt-shift): through `center` at `angle_deg` (0 = horizontal,
/// counter-clockwise positive); `width` is the half-height of the sharp zone and
/// `feather` the distance over which the blur ramps to its full amount, both normalized
/// to the long edge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FocusBand {
    #[serde(deserialize_with = "crate::lenient::pair")]
    pub center: [f32; 2],
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub angle_deg: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub width: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub feather: f32,
}

impl Default for FocusBand {
    fn default() -> Self {
        Self {
            center: [0.5, 0.55],
            angle_deg: 0.0,
            width: 0.06,
            feather: 0.22,
        }
    }
}

/// Soft glow (Orton): a blurred copy screened over the photo.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Glow {
    /// 0..=100; 0 = off.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub amount: f32,
    /// 0..=100: the glow's radius (0.4% to 3.4% of the long edge).
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub size: f32,
}

impl Default for Glow {
    fn default() -> Self {
        Self {
            amount: 0.0,
            size: 50.0,
        }
    }
}

/// Film grain: monochrome, strongest in the midtones, fixed to the image (the same at
/// every zoom and in the export).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Grain {
    /// 0..=100; 0 = off.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub amount: f32,
    /// 0..=100: grain size (about 0.7 to 3.7 px on a 24 MP frame).
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub size: f32,
    /// 0..=100: share of the finer, rougher octave.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub roughness: f32,
}

impl Default for Grain {
    fn default() -> Self {
        Self {
            amount: 0.0,
            size: 25.0,
            roughness: 50.0,
        }
    }
}

/// Halation: the warm red glow film shows around bright highlights.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Halation {
    /// 0..=100; 0 = off.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub amount: f32,
}

/// The one-touch look last applied (the app's Looks), so its intensity can be changed
/// afterwards. The pipeline ignores it: the look's values are in the other fields.
///
/// `from`/`to` hold, by the app's edit keys, each field the look changed: its value
/// before the look and at full intensity. At `intensity` t the field was set to
/// `from + t·(to − from)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppliedLook {
    pub id: String,
    /// 0..=1.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub intensity: f32,
    #[serde(deserialize_with = "values")]
    pub from: BTreeMap<String, f32>,
    #[serde(deserialize_with = "values")]
    pub to: BTreeMap<String, f32>,
}

/// A key → value map read leniently: `null` becomes NaN (dropped by `sanitize`) and
/// entries that aren't numbers (a newer build's) are ignored.
fn values<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, f32>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Num(Option<f32>),
        Other(serde::de::IgnoredAny),
    }
    let raw = BTreeMap::<String, Entry>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(k, e)| match e {
            Entry::Num(v) => Some((k, v.unwrap_or(f32::NAN))),
            Entry::Other(_) => None,
        })
        .collect())
}

impl Default for AppliedLook {
    fn default() -> Self {
        Self {
            id: String::new(),
            intensity: 1.0,
            from: BTreeMap::new(),
            to: BTreeMap::new(),
        }
    }
}

fn fix(changed: &mut bool, v: &mut f32, default: f32, lo: f32, hi: f32) {
    let next = if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        default
    };
    if next.to_bits() != v.to_bits() {
        *v = next;
        *changed = true;
    }
}

impl Effects {
    /// Non-finite values → defaults, everything clamped to its documented range. Returns
    /// whether anything changed.
    pub(crate) fn sanitize(&mut self) -> bool {
        let mut c = false;
        let lb = &mut self.lens_blur;
        let d = LensBlur::default();
        fix(&mut c, &mut lb.amount, 0.0, 0.0, 100.0);
        fix(&mut c, &mut lb.focal_range, d.focal_range, 0.0, 100.0);
        fix(
            &mut c,
            &mut lb.highlight_boost,
            d.highlight_boost,
            0.0,
            100.0,
        );
        if let Some(p) = lb.focus {
            if p.iter().all(|v| v.is_finite()) {
                let q = p.map(|v| v.clamp(0.0, 1.0));
                c |= q.map(f32::to_bits) != p.map(f32::to_bits);
                lb.focus = Some(q);
            } else {
                lb.focus = None;
                c = true;
            }
        }
        let band = &mut lb.plane;
        let db = FocusBand::default();
        for (v, dv) in band.center.iter_mut().zip(db.center) {
            fix(&mut c, v, dv, -1.0, 2.0);
        }
        fix(&mut c, &mut band.angle_deg, 0.0, -180.0, 180.0);
        fix(&mut c, &mut band.width, db.width, 0.0, 1.0);
        fix(&mut c, &mut band.feather, db.feather, 0.005, 1.0);
        let g = Glow::default();
        fix(&mut c, &mut self.glow.amount, 0.0, 0.0, 100.0);
        fix(&mut c, &mut self.glow.size, g.size, 0.0, 100.0);
        let gr = Grain::default();
        fix(&mut c, &mut self.grain.amount, 0.0, 0.0, 100.0);
        fix(&mut c, &mut self.grain.size, gr.size, 0.0, 100.0);
        fix(&mut c, &mut self.grain.roughness, gr.roughness, 0.0, 100.0);
        fix(&mut c, &mut self.halation.amount, 0.0, 0.0, 100.0);
        c
    }
}

impl AppliedLook {
    /// Intensity into 0..=1 (non-finite → 1), non-finite recorded values dropped.
    pub(crate) fn sanitize(&mut self) -> bool {
        let mut c = false;
        fix(&mut c, &mut self.intensity, 1.0, 0.0, 1.0);
        for m in [&mut self.from, &mut self.to] {
            let before = m.len();
            m.retain(|_, v| v.is_finite());
            c |= m.len() != before;
        }
        c
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use serde_json::json;

    use super::*;
    use crate::Adjustments;

    #[test]
    fn defaults_are_the_identity_and_older_sidecars_read_them() {
        let a: Adjustments = serde_json::from_value(json!({})).unwrap();
        assert_eq!(a.effects, Effects::default());
        assert!(a.effects.is_identity());
        assert_eq!(a.look, None);
        assert!(a.is_identity());
        // A sidecar without the new fields round-trips without adding a `focus` key.
        let v = serde_json::to_value(&a).unwrap();
        assert!(v["effects"]["lens_blur"].get("focus").is_none());
        assert!(v["look"].is_null());
    }

    #[test]
    fn effects_round_trip_and_unknown_fields_are_ignored() {
        let mut a = Adjustments::default();
        a.effects.lens_blur = LensBlur {
            amount: 60.0,
            mode: BlurMode::Plane,
            focus: Some([0.25, 0.75]),
            focal_range: 5.0,
            highlight_boost: 80.0,
            shape: BokehShape::CatEye,
            plane: FocusBand {
                center: [0.4, 0.6],
                angle_deg: 15.0,
                width: 0.1,
                feather: 0.3,
            },
        };
        a.effects.glow = Glow {
            amount: 40.0,
            size: 70.0,
        };
        a.effects.grain = Grain {
            amount: 30.0,
            size: 60.0,
            roughness: 10.0,
        };
        a.effects.halation.amount = 25.0;
        a.look = Some(AppliedLook {
            id: "look.cinematic".into(),
            intensity: 0.5,
            from: [("exposure.contrast".to_string(), 0.0)].into(),
            to: [("exposure.contrast".to_string(), 20.0)].into(),
        });
        assert!(!a.effects.is_identity());
        let mut v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["effects"]["lens_blur"]["shape"], "cat_eye");
        assert_eq!(v["effects"]["lens_blur"]["mode"], "plane");
        v["effects"]["future"] = json!(1);
        v["effects"]["grain"]["future"] = json!("x");
        let back: Adjustments = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn sanitize_clamps_and_drops_non_finite_values() {
        let v = json!({
            "effects": {
                "lens_blur": {"amount": 500.0, "focus": [null, 0.5], "focal_range": null,
                              "highlight_boost": -3.0, "plane": {"center": [null, 9.0], "width": 2.0,
                              "feather": 0.0, "angle_deg": 720.0}},
                "glow": {"amount": null, "size": 1000.0},
                "grain": {"amount": -1.0, "size": null, "roughness": 101.0},
                "halation": {"amount": 1e9}
            },
            "look": {"id": "x", "intensity": 3.0, "from": {"a": null, "b": 1.0, "c": {"x": 1}},
                     "to": {"b": 2.0}}
        });
        let a: Adjustments = serde_json::from_value(v).unwrap();
        let lb = &a.effects.lens_blur;
        assert_eq!(lb.amount, 100.0);
        assert_eq!(lb.focus, None);
        assert_eq!(lb.focal_range, LensBlur::default().focal_range);
        assert_eq!(lb.highlight_boost, 0.0);
        assert_eq!(lb.plane.center, [FocusBand::default().center[0], 2.0]);
        assert_eq!(lb.plane.width, 1.0);
        assert_eq!(lb.plane.feather, 0.005);
        assert_eq!(lb.plane.angle_deg, 180.0);
        assert_eq!(a.effects.glow.amount, 0.0);
        assert_eq!(a.effects.glow.size, 100.0);
        assert_eq!(a.effects.grain.amount, 0.0);
        assert_eq!(a.effects.grain.size, Grain::default().size);
        assert_eq!(a.effects.grain.roughness, 100.0);
        assert_eq!(a.effects.halation.amount, 100.0);
        let look = a.look.as_ref().unwrap();
        assert_eq!(look.intensity, 1.0);
        assert_eq!(look.from.len(), 1);
        assert_eq!(look.to["b"], 2.0);
        // Idempotent: a sanitized value has nothing left to fix.
        let mut again = a.clone();
        assert!(!again.sanitize());
        assert_eq!(again, a);
    }

    #[test]
    fn a_focus_point_is_clamped_into_the_frame() {
        let mut a = Adjustments::default();
        a.effects.lens_blur.focus = Some([-0.5, 1.5]);
        assert!(a.sanitize());
        assert_eq!(a.effects.lens_blur.focus, Some([0.0, 1.0]));
        a.effects.lens_blur.focus = Some([f32::NAN, 0.5]);
        assert!(a.sanitize());
        assert_eq!(a.effects.lens_blur.focus, None);
    }
}

/// The final review's parse cases, as the spec: a newer build's blur mode or shape and
/// a malformed focus or look must not fail the sidecar (that would drop every edit on
/// the photo); unknown choices are written back unchanged.
#[cfg(test)]
mod review_tests {
    use crate::Adjustments;
    use crate::effects::{BlurMode, BokehShape};
    use serde_json::json;

    #[test]
    fn review_unknown_shape_from_newer_build() {
        let r = serde_json::from_value::<Adjustments>(
            json!({"exposure": {"ev": 1.0}, "effects": {"lens_blur": {"amount": 50.0, "shape": "anamorphic"}}}),
        );
        assert!(r.is_ok(), "unknown shape: {:?}", r.err());
        let a = r.unwrap();
        assert_eq!(
            a.effects.lens_blur.shape,
            BokehShape::Other(json!("anamorphic"))
        );
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(
            v["effects"]["lens_blur"]["shape"],
            json!("anamorphic"),
            "kept on save"
        );
    }

    #[test]
    fn review_unknown_mode_from_newer_build() {
        let r = serde_json::from_value::<Adjustments>(
            json!({"effects": {"lens_blur": {"mode": "radial"}}}),
        );
        assert!(r.is_ok(), "unknown mode: {:?}", r.err());
        let a = r.unwrap();
        assert_eq!(a.effects.lens_blur.mode, BlurMode::Other(json!("radial")));
        assert_eq!(
            serde_json::to_value(&a).unwrap()["effects"]["lens_blur"]["mode"],
            json!("radial")
        );
        // Not a string at all: kept too.
        let odd: Adjustments =
            serde_json::from_value(json!({"effects": {"lens_blur": {"mode": 7}}})).unwrap();
        assert_eq!(odd.effects.lens_blur.mode, BlurMode::Other(json!(7)));
        // Known names still read as themselves.
        let p: Adjustments = serde_json::from_value(
            json!({"effects": {"lens_blur": {"mode": "plane", "shape": "cat_eye"}}}),
        )
        .unwrap();
        assert_eq!(
            (p.effects.lens_blur.mode, p.effects.lens_blur.shape),
            (BlurMode::Plane, BokehShape::CatEye)
        );
    }

    #[test]
    fn review_malformed_focus() {
        let r = serde_json::from_value::<Adjustments>(
            json!({"effects": {"lens_blur": {"focus": [0.5]}}}),
        );
        assert!(r.is_ok(), "focus [0.5]: {:?}", r.err());
        assert_eq!(r.unwrap().effects.lens_blur.focus, None, "no focus");
        let r = serde_json::from_value::<Adjustments>(
            json!({"effects": {"lens_blur": {"focus": "x"}}}),
        );
        assert!(r.is_ok(), "focus x: {:?}", r.err());
        let ok: Adjustments =
            serde_json::from_value(json!({"effects": {"lens_blur": {"focus": [0.25, 0.75]}}}))
                .unwrap();
        assert_eq!(ok.effects.lens_blur.focus, Some([0.25, 0.75]));
    }

    #[test]
    fn review_malformed_look() {
        let r = serde_json::from_value::<Adjustments>(
            json!({"look": {"id": 3, "from": [1]}, "exposure": {"exposure_ev": 1.0}}),
        );
        assert!(r.is_ok(), "look: {:?}", r.err());
        let a = r.unwrap();
        assert_eq!(a.look, None, "no look record");
        assert!(
            (a.exposure.exposure_ev - 1.0).abs() < 1e-6,
            "the rest of the edit loads"
        );
        let good: Adjustments =
            serde_json::from_value(json!({"look": {"id": "look.pop", "intensity": 0.5}})).unwrap();
        assert_eq!(good.look.map(|l| l.id), Some("look.pop".into()));
    }
}
