//! Lightroom develop settings → what Luxtable stores, and what has no
//! equivalent. See `docs/LR_FORMAT.md` for the evidence behind each rule.
//!
//! Lightroom writes every setting it knows, defaults included, so only a
//! value that differs from Lightroom's own default is an edit. Each edit lands
//! in one [`Category`]; an unrecognised setting that is not at its default is
//! reported as dropped rather than ignored silently.

use std::collections::BTreeSet;

use lt_core::Adjustments;
use lt_core::adjust::{ColorRange, CurvePoints, WhiteBalance};
use lt_core::geom::NormRect;
use lt_session::ImageMetaLite;

use crate::model::{Category, Develop, Fidelity, Setting};

/// What the mapping needs to know about the photo.
#[derive(Debug, Clone, Copy)]
pub struct MapContext<'a> {
    /// The Luxtable photo: sensor size and EXIF orientation.
    pub meta: &'a ImageMetaLite,
    /// A RAW file (Lightroom's temperature is in Kelvin only for these).
    pub is_raw: bool,
    /// The EXIF orientation (1-8) Lightroom shows the photo with, user rotation included.
    pub lr_orientation: Option<u8>,
}

/// What Luxtable stores for one Lightroom photo's develop settings.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mapped {
    /// `Adjustments::default()` with the mapped edits set (masks are layers named `Lightroom: …`).
    pub adjustments: Adjustments,
    /// Develop categories with at least one mapped setting.
    pub categories: BTreeSet<Category>,
    /// Lightroom settings that were mapped, and how faithfully.
    pub mapped: Vec<(String, Fidelity)>,
    /// Lightroom settings or features with no Luxtable equivalent, and why.
    pub dropped: Vec<(String, String)>,
}

/// Lightroom's hue-slider range (±100) onto Luxtable's ±30°. Adobe publishes
/// the slider range only; this spreads the whole range over Luxtable's.
const HUE_DEGREES_PER_POINT: f64 = 0.3;
/// Lightroom's sharpening amount (default 40) onto Luxtable's (default 140),
/// so Lightroom's default lands on Luxtable's.
const SHARPEN_AMOUNT_SCALE: f64 = 3.5;
/// Likewise for the radius (Lightroom's default 1.0, Luxtable's 0.8).
const SHARPEN_RADIUS_SCALE: f64 = 0.8;
/// Post-crop vignette: ±100 spread over Luxtable's ±3 EV.
const VIGNETTE_EV_PER_POINT: f64 = 0.03;

/// The eight HSL bands in Lightroom's names, in Luxtable's band order
/// (red, orange, yellow, green, cyan, blue, purple, magenta).
const BANDS: [&str; 8] = [
    "Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta",
];

/// Lightroom's value for a setting nobody touched. Anything not listed is 0.
fn lr_default(key: &str) -> f64 {
    match key {
        "Sharpness" => 40.0,
        "SharpenRadius" | "CropRight" | "CropBottom" => 1.0,
        "SharpenDetail" | "ColorNoiseReduction" | "GrainSize" => 25.0,
        "ColorNoiseReductionDetail"
        | "ColorNoiseReductionSmoothness"
        | "LuminanceNoiseReductionDetail"
        | "PostCropVignetteMidpoint"
        | "GrainFrequency"
        | "ColorGradeBlending" => 50.0,
        "PostCropVignetteFeather"
        | "LensProfileDistortionScale"
        | "LensProfileVignettingScale"
        | "LensProfileChromaticAberrationScale" => 100.0,
        _ => 0.0,
    }
}

pub(crate) struct Mapper<'a> {
    pub dev: &'a Develop,
    pub ctx: &'a MapContext<'a>,
    pub out: Mapped,
}

impl Mapper<'_> {
    pub(crate) fn get(&self, key: &str) -> Option<&Setting> {
        self.dev.get(key)
    }

    pub(crate) fn num(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(Setting::num)
    }

    /// The value when it is an edit: present and not Lightroom's default.
    pub(crate) fn edit(&self, key: &str) -> Option<f64> {
        self.num(key).filter(|v| (v - lr_default(key)).abs() > 1e-9)
    }

    pub(crate) fn flag(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(Setting::flag)
    }

    fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Setting::text).map(str::trim)
    }

    pub(crate) fn done(&mut self, key: &str, cat: Category, fidelity: Fidelity) {
        self.out.categories.insert(cat);
        if !self.out.mapped.iter().any(|(k, _)| k == key) {
            self.out.mapped.push((key.to_owned(), fidelity));
        }
    }

    pub(crate) fn drop_setting(&mut self, key: &str, reason: &str) {
        if !self.out.dropped.iter().any(|(k, _)| k == key) {
            self.out.dropped.push((key.to_owned(), reason.to_owned()));
        }
    }

    /// A value that changed `key`'s Luxtable setting, clamped by the caller.
    fn scaled(
        &mut self,
        key: &str,
        cat: Category,
        fid: Fidelity,
        f: impl Fn(f64) -> f64,
    ) -> Option<f32> {
        let v = self.edit(key)?;
        self.done(key, cat, fid);
        Some(f(v) as f32)
    }
}

/// Map one photo's develop settings. Only settings that differ from
/// Lightroom's own defaults count as edits.
pub fn map_develop(dev: &Develop, ctx: &MapContext) -> Mapped {
    let mut m = Mapper {
        dev,
        ctx,
        out: Mapped::default(),
    };
    m.crop();
    m.white_balance();
    m.tone();
    m.color();
    m.detail();
    m.effects();
    m.lens();
    crate::map_local::masks(&mut m);
    m.leftovers();
    m.out.adjustments = m.out.adjustments.sanitized();
    m.out
}

/// EXIF orientation value → clockwise quarter turns, for the four rotations.
fn quarter_turns(exif: u8) -> Option<i32> {
    match exif {
        1 => Some(0),
        6 => Some(1),
        3 => Some(2),
        8 => Some(3),
        _ => None,
    }
}

/// The user's rotation in Lightroom, as clockwise quarter turns on top of the
/// file's EXIF orientation. `Err` when a mirrored orientation differs.
pub(crate) fn user_turns(ctx: &MapContext) -> Result<i32, String> {
    let exif = ctx.meta.orientation.to_exif() as u8;
    let Some(lr) = ctx.lr_orientation.filter(|o| (1..=8).contains(o)) else {
        return Ok(0);
    };
    if lr == exif {
        return Ok(0);
    }
    match (quarter_turns(lr), quarter_turns(exif)) {
        (Some(l), Some(e)) => Ok((l - e).rem_euclid(4)),
        _ => Err("Orientation: a mirrored orientation changed in Lightroom".to_owned()),
    }
}

/// A point in Lightroom's frame (the photo as shown, rotated `k` quarter
/// turns clockwise from the EXIF-oriented frame) in the EXIF-oriented frame.
pub(crate) fn to_exif_frame(k: i32, (x, y): (f64, f64)) -> (f64, f64) {
    match k.rem_euclid(4) {
        1 => (y, 1.0 - x),
        2 => (1.0 - x, 1.0 - y),
        3 => (1.0 - y, x),
        _ => (x, y),
    }
}

impl Mapper<'_> {
    fn crop(&mut self) {
        let k = match user_turns(self.ctx) {
            Ok(k) => k,
            Err(why) => {
                self.drop_setting("Orientation", &why);
                0
            }
        };
        let crop = &mut self.out.adjustments.crop;
        if k != 0 {
            crop.quarter_turns = k as u8;
            self.done("Orientation", Category::Crop, Fidelity::Exact);
        }
        let side = |key: &str| self.num(key).filter(|v| (0.0..=1.0).contains(v));
        let (l, t) = (
            side("CropLeft").unwrap_or(0.0),
            side("CropTop").unwrap_or(0.0),
        );
        let (r, b) = (
            side("CropRight").unwrap_or(1.0),
            side("CropBottom").unwrap_or(1.0),
        );
        let angle = self.num("CropAngle").unwrap_or(0.0).clamp(-45.0, 45.0);
        let has_crop = self.flag("HasCrop") != Some(false);
        let boxed = l > 1e-4 || t > 1e-4 || r < 1.0 - 1e-4 || b < 1.0 - 1e-4;
        if !has_crop || !(boxed || angle.abs() > 1e-6) {
            return;
        }
        if boxed && (r <= l || b <= t) {
            self.drop_setting("CropLeft", "Crop: the rectangle is empty");
            return;
        }
        // The rectangle is in Lightroom's frame; Luxtable's is the EXIF-oriented one.
        let (a, c) = (to_exif_frame(k, (l, t)), to_exif_frame(k, (r, b)));
        let (x0, x1) = (a.0.min(c.0), a.0.max(c.0));
        let (y0, y1) = (a.1.min(c.1), a.1.max(c.1));
        let crop = &mut self.out.adjustments.crop;
        crop.rect = NormRect {
            x: x0 as f32,
            y: y0 as f32,
            w: (x1 - x0) as f32,
            h: (y1 - y0) as f32,
        };
        // Positive angles turn the crop clockwise: the content counter-clockwise, as Luxtable's do.
        crop.angle_deg = angle as f32;
        let fid = if angle.abs() > 1e-6 {
            Fidelity::Approximate
        } else {
            Fidelity::Exact
        };
        for key in ["CropLeft", "CropTop", "CropRight", "CropBottom"] {
            if self.get(key).is_some() {
                self.done(key, Category::Crop, fid);
            }
        }
        if angle.abs() > 1e-6 {
            self.done("CropAngle", Category::Crop, Fidelity::Approximate);
        }
    }

    fn white_balance(&mut self) {
        let wb = self.text("WhiteBalance").map(str::to_ascii_lowercase);
        if wb.as_deref() == Some("as shot") {
            return;
        }
        let Some(temp) = self.num("Temperature") else {
            return;
        };
        let tint = self.num("Tint").unwrap_or(0.0);
        if !self.ctx.is_raw || temp < 1000.0 {
            // Non-raw files use a relative -100..100 scale; Kelvin is only meaningful for raw.
            if temp != 0.0 || tint != 0.0 {
                self.drop_setting(
                    "Temperature",
                    "White balance: only a raw file's Kelvin temperature is meaningful",
                );
            }
            return;
        }
        // The same as the camera's own: nothing to do.
        if let Some((k, t)) = self.ctx.meta.as_shot_wb
            && (f64::from(k) - temp).abs() < 1.0
            && (f64::from(t) - tint).abs() < 1.0
        {
            return;
        }
        self.out.adjustments.white_balance = WhiteBalance::Custom {
            temp_k: temp as f32,
            tint: tint.clamp(-150.0, 150.0) as f32,
        };
        self.done("Temperature", Category::WhiteBalance, Fidelity::Approximate);
        if self.get("Tint").is_some() {
            self.done("Tint", Category::WhiteBalance, Fidelity::Approximate);
        }
    }

    fn tone(&mut self) {
        let approx = Fidelity::Approximate;
        let a = &mut self.out.adjustments;
        let (mut ev, mut contrast) = (a.exposure.exposure_ev, a.exposure.contrast);
        let (mut hi, mut sh, mut wh, mut bl) =
            (a.hdr.highlights, a.hdr.shadows, a.hdr.whites, a.hdr.blacks);
        let t = Category::Tone;
        ev = self
            .scaled("Exposure2012", t, approx, |v| v.clamp(-5.0, 5.0))
            .unwrap_or(ev);
        contrast = self
            .scaled("Contrast2012", t, approx, |v| v.clamp(-100.0, 100.0))
            .unwrap_or(contrast);
        // Luxtable's positive highlights recover (darken); Lightroom's negative ones do.
        hi = self
            .scaled("Highlights2012", t, approx, |v| -v.clamp(-100.0, 100.0))
            .unwrap_or(hi);
        sh = self
            .scaled("Shadows2012", t, approx, |v| v.clamp(-100.0, 100.0))
            .unwrap_or(sh);
        wh = self
            .scaled("Whites2012", t, approx, |v| v.clamp(-100.0, 100.0))
            .unwrap_or(wh);
        bl = self
            .scaled("Blacks2012", t, approx, |v| v.clamp(-100.0, 100.0))
            .unwrap_or(bl);
        let a = &mut self.out.adjustments;
        (a.exposure.exposure_ev, a.exposure.contrast) = (ev, contrast);
        (a.hdr.highlights, a.hdr.shadows, a.hdr.whites, a.hdr.blacks) = (hi, sh, wh, bl);

        for (key, pick) in [
            ("ToneCurvePV2012", 0usize),
            ("ToneCurvePV2012Red", 1),
            ("ToneCurvePV2012Green", 2),
            ("ToneCurvePV2012Blue", 3),
        ] {
            let Some(setting) = self.get(key) else {
                continue;
            };
            match curve_points(setting) {
                Ok(None) => {}
                Ok(Some(points)) => {
                    let c = &mut self.out.adjustments.curve;
                    match pick {
                        0 => c.rgb = points,
                        1 => c.red = points,
                        2 => c.green = points,
                        _ => c.blue = points,
                    }
                    self.done(key, t, approx);
                }
                Err(why) => self.drop_setting(key, &why),
            }
        }
        for key in [
            "ParametricShadows",
            "ParametricDarks",
            "ParametricLights",
            "ParametricHighlights",
        ] {
            if self.edit(key).is_some() {
                self.drop_setting(
                    key,
                    "Parametric tone curve: Luxtable's curve has points only",
                );
            }
        }
    }

    fn color(&mut self) {
        let approx = Fidelity::Approximate;
        let c = Category::Color;
        if let Some(v) = self.scaled("Saturation", c, approx, |v| v.clamp(-100.0, 100.0)) {
            self.out.adjustments.exposure.saturation = v;
        }
        if let Some(v) = self.scaled("Vibrance", c, approx, |v| v.clamp(-100.0, 100.0)) {
            self.out.adjustments.presence.vibrance = v;
        }
        for (i, band) in BANDS.iter().enumerate() {
            let hue = self.scaled(&format!("HueAdjustment{band}"), c, approx, |v| {
                (v * HUE_DEGREES_PER_POINT).clamp(-30.0, 30.0)
            });
            let sat = self.scaled(&format!("SaturationAdjustment{band}"), c, approx, |v| {
                v.clamp(-100.0, 100.0)
            });
            let lum = self.scaled(&format!("LuminanceAdjustment{band}"), c, approx, |v| {
                v.clamp(-100.0, 100.0)
            });
            let r: &mut ColorRange = &mut self.out.adjustments.color_editor.basic[i];
            r.hue_shift = hue.unwrap_or(r.hue_shift);
            r.saturation = sat.unwrap_or(r.saturation);
            r.lightness = lum.unwrap_or(r.lightness);
        }
        self.color_grading();
        if self.flag("ConvertToGrayscale") == Some(true) {
            self.out.adjustments.exposure.saturation = -100.0;
            self.done("ConvertToGrayscale", c, approx);
            self.drop_setting(
                "GrayMixer",
                "Black & white: the per-colour mix has no equivalent (saturation is taken to -100)",
            );
        }
    }

    /// Colour grading (Lightroom 10+) or the older split toning, onto the colour wheels.
    fn color_grading(&mut self) {
        let approx = Fidelity::Approximate;
        let c = Category::Color;
        let wheels = [
            ("Shadow", 0usize),
            ("Midtone", 1),
            ("Highlight", 2),
            ("Global", 3),
        ];
        let mut graded = false;
        for (name, w) in wheels {
            let hue = self.num(&format!("ColorGrade{name}Hue"));
            let sat = self.edit(&format!("ColorGrade{name}Sat"));
            let lum = self.edit(&format!("ColorGrade{name}Lum"));
            if sat.is_none() && lum.is_none() {
                continue;
            }
            graded = true;
            let wheel = self.wheel(w);
            wheel.hue = hue.unwrap_or(0.0).rem_euclid(360.0) as f32;
            wheel.sat = (sat.unwrap_or(0.0) / 100.0).clamp(0.0, 1.0) as f32;
            wheel.lum = (lum.unwrap_or(0.0) / 100.0).clamp(-1.0, 1.0) as f32;
            for part in ["Hue", "Sat", "Lum"] {
                let key = format!("ColorGrade{name}{part}");
                if self.get(&key).is_some() {
                    self.done(&key, c, approx);
                }
            }
        }
        if graded {
            for key in ["ColorGradeBlending", "ColorGradeBalance"] {
                if self.edit(key).is_some() {
                    self.drop_setting(
                        key,
                        "Colour grading: blending and balance have no equivalent",
                    );
                }
            }
            return;
        }
        // Split toning: shadows and highlights only.
        for (name, w) in [("Shadow", 0usize), ("Highlight", 2)] {
            let Some(sat) = self.edit(&format!("SplitToning{name}Saturation")) else {
                continue;
            };
            let hue = self.num(&format!("SplitToning{name}Hue")).unwrap_or(0.0);
            let wheel = self.wheel(w);
            wheel.hue = hue.rem_euclid(360.0) as f32;
            wheel.sat = (sat / 100.0).clamp(0.0, 1.0) as f32;
            self.done(&format!("SplitToning{name}Saturation"), c, approx);
            if self.get(&format!("SplitToning{name}Hue")).is_some() {
                self.done(&format!("SplitToning{name}Hue"), c, approx);
            }
        }
        if self.edit("SplitToningBalance").is_some() {
            self.drop_setting(
                "SplitToningBalance",
                "Split toning: the balance slider has no equivalent",
            );
        }
    }

    fn wheel(&mut self, i: usize) -> &mut lt_core::adjust::Wheel {
        let cb = &mut self.out.adjustments.color_balance;
        match i {
            0 => &mut cb.shadows,
            1 => &mut cb.midtones,
            2 => &mut cb.highlights,
            _ => &mut cb.master,
        }
    }

    fn detail(&mut self) {
        let approx = Fidelity::Approximate;
        let d = Category::Detail;
        let a = &mut self.out.adjustments;
        let (mut texture, mut clarity, mut dehaze) =
            (a.presence.texture, a.detail.clarity, a.presence.dehaze);
        let (mut amount, mut radius, mut threshold) = (
            a.detail.sharpen_amount,
            a.detail.sharpen_radius,
            a.detail.sharpen_threshold,
        );
        let (mut lum, mut color, mut detail) = (a.noise.luminance, a.noise.color, a.noise.detail);
        texture = self
            .scaled("Texture", d, approx, |v| v.clamp(-100.0, 100.0))
            .unwrap_or(texture);
        clarity = self
            .scaled("Clarity2012", d, approx, |v| v.clamp(-100.0, 100.0))
            .unwrap_or(clarity);
        dehaze = self
            .scaled("Dehaze", d, approx, |v| v.clamp(-100.0, 100.0))
            .unwrap_or(dehaze);
        amount = self
            .scaled("Sharpness", d, approx, |v| {
                (v * SHARPEN_AMOUNT_SCALE).clamp(0.0, 1000.0)
            })
            .unwrap_or(amount);
        radius = self
            .scaled("SharpenRadius", d, approx, |v| {
                (v * SHARPEN_RADIUS_SCALE).clamp(0.2, 2.5)
            })
            .unwrap_or(radius);
        // Edge masking hides sharpening in flat areas, as Luxtable's threshold does: 0..100 onto 1..10.
        threshold = self
            .scaled("SharpenEdgeMasking", d, approx, |v| {
                (1.0 + v * 0.09).clamp(0.0, 10.0)
            })
            .unwrap_or(threshold);
        lum = self
            .scaled("LuminanceSmoothing", d, approx, |v| v.clamp(0.0, 100.0))
            .unwrap_or(lum);
        color = self
            .scaled("ColorNoiseReduction", d, approx, |v| v.clamp(0.0, 100.0))
            .unwrap_or(color);
        detail = self
            .scaled("LuminanceNoiseReductionDetail", d, approx, |v| {
                v.clamp(0.0, 100.0)
            })
            .unwrap_or(detail);
        let a = &mut self.out.adjustments;
        (a.presence.texture, a.detail.clarity, a.presence.dehaze) = (texture, clarity, dehaze);
        (
            a.detail.sharpen_amount,
            a.detail.sharpen_radius,
            a.detail.sharpen_threshold,
        ) = (amount, radius, threshold);
        (a.noise.luminance, a.noise.color, a.noise.detail) = (lum, color, detail);
        for (key, why) in [
            ("SharpenDetail", "Sharpening: Detail has no equivalent"),
            (
                "LuminanceNoiseReductionContrast",
                "Noise reduction: luminance contrast has no equivalent",
            ),
            (
                "ColorNoiseReductionDetail",
                "Noise reduction: colour detail has no equivalent",
            ),
            (
                "ColorNoiseReductionSmoothness",
                "Noise reduction: colour smoothness has no equivalent",
            ),
        ] {
            if self.edit(key).is_some() {
                self.drop_setting(key, why);
            }
        }
    }

    fn effects(&mut self) {
        let approx = Fidelity::Approximate;
        let e = Category::Effects;
        if let Some(v) = self.scaled("PostCropVignetteAmount", e, approx, |v| {
            (v * VIGNETTE_EV_PER_POINT).clamp(-3.0, 3.0)
        }) {
            let vig = &mut self.out.adjustments.vignette;
            vig.amount_ev = v;
            if let Some(mid) = self.scaled("PostCropVignetteMidpoint", e, approx, |v| {
                (v / 100.0).clamp(0.0, 1.0)
            }) {
                self.out.adjustments.vignette.midpoint = mid;
            }
            for key in [
                "PostCropVignetteFeather",
                "PostCropVignetteRoundness",
                "PostCropVignetteStyle",
                "PostCropVignetteHighlightContrast",
            ] {
                if self.edit(key).is_some() {
                    self.drop_setting(
                        key,
                        "Vignette: Luxtable's vignette has an amount and a midpoint only",
                    );
                }
            }
        }
        if self.edit("GrainAmount").is_some() {
            let amount = self.scaled("GrainAmount", e, approx, |v| v.clamp(0.0, 100.0));
            let size = self.scaled("GrainSize", e, approx, |v| v.clamp(0.0, 100.0));
            let rough = self.scaled("GrainFrequency", e, approx, |v| v.clamp(0.0, 100.0));
            let g = &mut self.out.adjustments.effects.grain;
            g.amount = amount.unwrap_or(g.amount);
            g.size = size.unwrap_or(g.size);
            g.roughness = rough.unwrap_or(g.roughness);
        }
    }

    fn lens(&mut self) {
        let approx = Fidelity::Approximate;
        let l = Category::Lens;
        if let Some(on) = self.flag("LensProfileEnable") {
            self.out.adjustments.lens.enabled = on;
            if !on {
                self.done("LensProfileEnable", l, Fidelity::Exact);
            }
        }
        if let Some(on) = self.flag("AutoLateralCA") {
            self.out.adjustments.lens.chromatic_aberration = on;
            if !on {
                self.done("AutoLateralCA", l, Fidelity::Exact);
            }
        }
        // 100 is a full correction; Luxtable's vignetting default (0.5) matches Lightroom's 100.
        if let Some(v) = self.scaled("LensProfileDistortionScale", l, approx, |v| {
            (v / 100.0).clamp(0.0, 1.0)
        }) {
            self.out.adjustments.lens.distortion = v;
        }
        if let Some(v) = self.scaled("LensProfileVignettingScale", l, approx, |v| {
            (v / 200.0).clamp(0.0, 1.0)
        }) {
            self.out.adjustments.lens.light_falloff = v;
        }
        if self.text("LensProfileName").is_some_and(|n| !n.is_empty())
            && self.flag("LensProfileEnable") == Some(true)
        {
            self.drop_setting(
                "LensProfileName",
                "Lens profile: Luxtable chooses its own profile from the photo's lens",
            );
        }
    }

    /// Settings Lightroom writes that nothing above handled, when they are not at their default.
    fn leftovers(&mut self) {
        for (key, why) in [
            (
                "PerspectiveVertical",
                "Transform: Upright and perspective corrections have no calibrated equivalent",
            ),
            (
                "PerspectiveHorizontal",
                "Transform: Upright and perspective corrections have no calibrated equivalent",
            ),
            (
                "PerspectiveRotate",
                "Transform: Upright and perspective corrections have no calibrated equivalent",
            ),
            (
                "PerspectiveScale",
                "Transform: Upright and perspective corrections have no calibrated equivalent",
            ),
            (
                "PerspectiveAspect",
                "Transform: Upright and perspective corrections have no calibrated equivalent",
            ),
            (
                "LensManualDistortionAmount",
                "Lens: manual distortion has no calibrated equivalent",
            ),
            (
                "DefringePurpleAmount",
                "Defringe: Luxtable corrects lateral aberration only",
            ),
            (
                "DefringeGreenAmount",
                "Defringe: Luxtable corrects lateral aberration only",
            ),
            (
                "VignetteAmount",
                "Lens vignetting (manual): Luxtable applies its own lens profile",
            ),
        ] {
            if self.edit(key).is_some() {
                self.drop_setting(key, why);
            }
        }
        if self.edit("Clarity").is_some() && self.get("Clarity2012").is_none() {
            self.drop_setting(
                "Clarity",
                "Clarity from an older process version (before 2012)",
            );
        }
        self.legacy_tone();
        if self.dev.contains_key("Look") || self.dev.contains_key("CameraProfile") {
            let named = self.text("CameraProfile").unwrap_or_default();
            if !named.is_empty() && named != "Adobe Standard" {
                self.drop_setting(
                    "CameraProfile",
                    "Camera profile: Luxtable keeps its own base look (import a .dcp to use a profile)",
                );
            }
        }
    }

    /// The tone sliders of process versions before 2012 mean something else.
    fn legacy_tone(&mut self) {
        for (old, new) in [
            ("Exposure", "Exposure2012"),
            ("Contrast", "Contrast2012"),
            ("Shadows", "Shadows2012"),
            ("Brightness", ""),
            ("HighlightRecovery", ""),
            ("FillLight", ""),
        ] {
            if self.edit(old).is_some() && (new.is_empty() || self.get(new).is_none()) {
                self.drop_setting(
                    old,
                    "Tone: settings from process version 2010 or older mean something else",
                );
            }
        }
    }
}

#[allow(clippy::float_cmp)] // exact: x values are parsed integers on 0..255
/// A tone curve as Luxtable points. `Ok(None)`: the identity curve.
/// Lightroom writes `x, y` pairs on 0..255 (text in XMP, a flat number list in catalogs).
fn curve_points(s: &Setting) -> Result<Option<CurvePoints>, String> {
    let bad = || "Tone curve: unreadable points".to_owned();
    let items = s.list().ok_or_else(bad)?;
    let mut flat: Vec<f64> = Vec::new();
    for item in items {
        match item {
            Setting::Text(t) if t.contains(',') => {
                for part in t.split(',') {
                    flat.push(crate::model::parse_num(part).ok_or_else(bad)?);
                }
            }
            other => flat.push(other.num().ok_or_else(bad)?),
        }
    }
    if flat.len() < 4 || !flat.len().is_multiple_of(2) || flat.len() > 2 * 64 {
        return Err(bad());
    }
    let mut points: CurvePoints = flat
        .chunks(2)
        .map(|p| {
            [
                (p[0] / 255.0).clamp(0.0, 1.0) as f32,
                (p[1] / 255.0).clamp(0.0, 1.0) as f32,
            ]
        })
        .collect();
    points.sort_by(|a, b| a[0].total_cmp(&b[0]));
    if points.windows(2).any(|w| w[0][0] == w[1][0]) {
        return Err(bad());
    }
    let identity = points.len() == 2 && points[0] == [0.0, 0.0] && points[1] == [1.0, 1.0];
    Ok((!identity).then_some(points))
}

#[cfg(test)]
#[path = "map_tests.rs"]
mod tests;
