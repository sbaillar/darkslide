//! One C1 variant → what Darkslide stores, and what has no equivalent.
//!
//! Only adjustment-layer (`AL`) keys are the user's edits; default-layer
//! values are C1's own starting point (camera profile, as-shot white
//! balance, EXIF orientation) and are used only as the reference an edit is
//! relative to. See `docs/C1_FORMAT.md` for the evidence behind each rule.

use std::path::Path;

use ds_core::adjust::{Aspect, Crop, CurvePoints, Exposure, Hdr, WhiteBalance};
use ds_core::geom::{NormRect, Orientation};
use ds_core::{Adjustments, ColorTag, Rating};
use ds_session::ImageMetaLite;

use crate::model::{Settings, parse_f64, parse_list};
use crate::state::Tools;

/// Converts a C1 white-balance edit to Darkslide's (the app reads the
/// original's camera matrix; tests use a double).
pub trait WbConverter {
    /// `c1_default`: C1's as-shot multipliers (default layer, R;G;B with
    /// G = 1), when known; `c1_user`: the edited multipliers. `None` when
    /// the original can't be read.
    fn convert(
        &self,
        original: &Path,
        c1_default: Option<[f64; 3]>,
        c1_user: [f64; 3],
    ) -> Option<WhiteBalance>;
}

/// How the import treats a settings key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyClass {
    /// Rating, colour tag, keywords, label.
    Organization,
    /// Translated into Darkslide adjustments.
    Mapped,
    /// Computed by C1 (not a user edit) or consumed with another key.
    Derived,
    /// A user edit with no Darkslide translation (the reason says why).
    Unmapped(&'static str),
}

/// Keys translated into Darkslide adjustments.
const MAPPED: [&str; 33] = [
    "Exposure",
    "Contrast",
    "Brightness",
    "Saturation",
    "HighlightRecoveryEx",
    "ShadowRecovery",
    "WhiteRecovery",
    "BlackRecovery",
    "WhiteBalance",
    "WhiteBalanceTemperature",
    "WhiteBalanceTint",
    "Crop",
    "Rotation",
    "Clarity",
    "ClarityStructure",
    "DehazeAmount",
    "Vignetting",
    "UsmAmount",
    "UsmRadius",
    "UsmThreshold",
    "NrAmount",
    "CnrAmount",
    "NoiseDetailsAmount",
    "Shadow",
    "Highlight",
    "TargetShadow",
    "TargetHighlight",
    "ChromaticAberEnabled",
    "GradationCurve",
    "GradationCurveY",
    "GradationCurveRed",
    "GradationCurveGreen",
    "GradationCurveBlue",
];

/// Classify a `.cos` settings key.
pub fn classify(key: &str) -> KeyClass {
    const LEVELS: &str = "Levels: per-channel levels and the midtone have no confirmed translation";
    const SHARPEN: &str = "Sharpening method / halo suppression: no Darkslide equivalent";
    const NOISE: &str =
        "Noise reduction (surface smoothness, long exposure): no Darkslide equivalent";
    const DEHAZE: &str = "Dehaze colour: Darkslide estimates the haze colour itself";
    const COLOR: &str = "Colour balance / colour editor: not translated";
    const BW: &str = "Black & white: no Darkslide equivalent";
    const LENS: &str = "Lens: Darkslide applies its own lens profile";
    const KEYSTONE: &str = "Keystone: C1's units and sign are unconfirmed (no keystone edits seen)";
    const BASE: &str =
        "Base characteristics (curve, ICC profile): C1 camera profiles have no equivalent";
    const GRAIN: &str = "Film grain: no Darkslide equivalent";
    const MOIRE: &str = "Moiré: no Darkslide equivalent";
    const SPOTS: &str = "Spot / dust removal: not translated";
    const FLIP: &str = "Flip: C1's flip encoding is not confirmed";
    const META: &str = "IPTC / contact metadata: not imported";
    const UNKNOWN: &str = "Not recognized";
    match key {
        "Basic_Rating" | "Color_Tag_Index" | "Content_Keywords" | "Basic_Label" => {
            KeyClass::Organization
        }
        k if MAPPED.contains(&k) => KeyClass::Mapped,
        "CropAspectRatioName"
        | "CalculatedDehazeColor"
        | "DehazeLevelHigh"
        | "DehazeLevelLow"
        | "ClarityMethod"
        | "CanvasSize"
        | "ContainsOrientation"
        | "Enabled"
        | "Name"
        | "LayerSelectPoint"
        | "StyleModified"
        | "StyleSource"
        | "TechnicalProcessingMethod"
        | "CropOutsideImage"
        | "GeometryCanvasClipping"
        | "GeometryKeystoneFocalLength"
        | "LevelsTargetEnable"
        | "ImageDimensions" => KeyClass::Derived,
        "Flip" => KeyClass::Unmapped(FLIP),
        "Midtone" => KeyClass::Unmapped(LEVELS),
        k if k.starts_with("Usm") => KeyClass::Unmapped(SHARPEN),
        "SurfaceSmoothnessAmount" | "CleanLongExposureAmount" | "NoiseReductionAmount" => {
            KeyClass::Unmapped(NOISE)
        }
        "DehazeColor" => KeyClass::Unmapped(DEHAZE),
        k if k.starts_with("ColorBalance")
            || k == "ColorCorrections"
            || k == "ColorCastCorrection" =>
        {
            KeyClass::Unmapped(COLOR)
        }
        k if k.starts_with("Bw") => KeyClass::Unmapped(BW),
        k if k.starts_with("GeometryKeystone") => KeyClass::Unmapped(KEYSTONE),
        k if k.starts_with("Lens") || k.starts_with("Lcc") || k == "PurpleFringingAmount" => {
            KeyClass::Unmapped(LENS)
        }
        "FilmCurve" | "FilmCurveSettings" | "ICCProfile" => KeyClass::Unmapped(BASE),
        k if k.starts_with("FilmGrain") => KeyClass::Unmapped(GRAIN),
        "Moire" => KeyClass::Unmapped(MOIRE),
        "Spots" | "RelativeSpots" | "RelativeDust" | "CorrespondingPoints" => {
            KeyClass::Unmapped(SPOTS)
        }
        k if [
            "Contact_", "Content_", "Image_", "Status_", "Getty_", "Basic_",
        ]
        .iter()
        .any(|p| k.starts_with(p)) =>
        {
            KeyClass::Unmapped(META)
        }
        _ => KeyClass::Unmapped(UNKNOWN),
    }
}

/// `.cos` key for a session-database column (`ZEXPOSURE` → `Exposure`).
/// Unknown columns keep their name without the leading `Z`.
pub(crate) fn key_for_column(column: &str) -> String {
    const EXCEPTIONS: [(&str, &str); 7] = [
        ("ZHIGHLIGHTRECOVERY", "HighlightRecoveryEx"),
        ("ZLENSVIGNETTING", "Vignetting"),
        ("ZCROPASPECTRATIONAME", "CropAspectRatioName"),
        ("ZLEVELSHIGHLIGHT", "Highlight"),
        ("ZLEVELSSHADOW", "Shadow"),
        ("ZLEVELSTARGETHIGHLIGHT", "TargetHighlight"),
        ("ZLEVELSTARGETSHADOW", "TargetShadow"),
    ];
    const KNOWN: [&str; 35] = [
        "Exposure",
        "Contrast",
        "Brightness",
        "Saturation",
        "ShadowRecovery",
        "WhiteRecovery",
        "BlackRecovery",
        "WhiteBalance",
        "Crop",
        "Rotation",
        "Flip",
        "Clarity",
        "ClarityMethod",
        "ClarityStructure",
        "DehazeAmount",
        "CalculatedDehazeColor",
        "DehazeLevelHigh",
        "DehazeLevelLow",
        "Basic_Rating",
        "Basic_Label",
        "Color_Tag_Index",
        "Content_Keywords",
        "Midtone",
        "Vignetting",
        "NrAmount",
        "CnrAmount",
        "NoiseDetailsAmount",
        "ChromaticAberEnabled",
        "DehazeColor",
        "SurfaceSmoothnessAmount",
        "UsmAmount",
        "UsmRadius",
        "UsmThreshold",
        "UsmMethod",
        "UsmHaloControl",
    ];
    if let Some((_, k)) = EXCEPTIONS.iter().find(|(c, _)| *c == column) {
        return (*k).to_owned();
    }
    let bare = column.strip_prefix('Z').unwrap_or(column);
    if let Some(k) = KNOWN.iter().find(|k| k.eq_ignore_ascii_case(bare)) {
        return (*k).to_owned();
    }
    if let Some(rest) = bare.strip_prefix("GRADATIONCURVE") {
        let color = match rest {
            "RED" => "Red",
            "GREEN" => "Green",
            "BLUE" => "Blue",
            "Y" => "Y",
            _ => "",
        };
        return format!("GradationCurve{color}");
    }
    if let Some(rest) = bare.strip_prefix("USM") {
        return format!("Usm{rest}");
    }
    bare.to_owned()
}

/// What Darkslide stores for one C1 variant.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mapped {
    pub rating: Rating,
    pub color_tag: ColorTag,
    pub keywords: Vec<String>,
    /// C1's values for the tools the import maps; Darkslide's defaults where
    /// C1 has no edit.
    pub tools: Tools,
    /// Mapped edit keys present in the adjustment layer.
    pub mapped: Vec<String>,
    /// Edits with no translation: (key, reason).
    pub unmapped: Vec<(String, &'static str)>,
}

impl Mapped {
    /// `base` with the mapped tools replaced by C1's values; every other
    /// tool keeps `base`'s.
    pub fn apply(&self, base: &Adjustments) -> Adjustments {
        self.tools.applied_to(base)
    }

    /// The mapped tools are all at their defaults.
    pub fn adjustments_are_default(&self) -> bool {
        self.apply(&Adjustments::default()).is_identity()
    }

    /// Report a present key as not translated after all.
    fn unmap(&mut self, key: &str, reason: &'static str) {
        self.mapped.retain(|k| k != key);
        self.unmapped.push((key.to_owned(), reason));
    }
}

/// Split C1's keyword text. Entries are separated by `;` (also `,` unless the
/// keyword library has the entry with its comma); a hierarchical entry
/// (`Places|Iceland`) contributes its leaf.
pub fn split_keywords(text: &str, library: &[String]) -> Vec<String> {
    let known = |s: &str| library.iter().any(|k| k == s);
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let leaf = s
            .rsplit('|')
            .map(str::trim)
            .find(|p| !p.is_empty())
            .unwrap_or_default();
        if !leaf.is_empty() && !out.iter().any(|k| k == leaf) {
            out.push(leaf.to_owned());
        }
    };
    let whole = text.trim();
    if whole.is_empty() {
        return out;
    }
    if known(whole) {
        push(whole);
        return out;
    }
    for part in whole.split([';', '\n']).map(str::trim) {
        if known(part) || !part.contains(',') {
            push(part);
        } else {
            part.split(',').for_each(&mut push);
        }
    }
    out
}

/// C1 colour tag index (0 none, 1 red … 7 purple: the same order as Darkslide's).
fn color_tag(index: i64) -> Option<ColorTag> {
    Some(match index {
        0 => ColorTag::None,
        1 => ColorTag::Red,
        2 => ColorTag::Orange,
        3 => ColorTag::Yellow,
        4 => ColorTag::Green,
        5 => ColorTag::Blue,
        6 => ColorTag::Pink,
        7 => ColorTag::Purple,
        _ => return None,
    })
}

fn color_from_label(label: &str) -> Option<ColorTag> {
    let l = label.trim().to_ascii_lowercase();
    [
        "", "red", "orange", "yellow", "green", "blue", "pink", "purple",
    ]
    .iter()
    .position(|n| !n.is_empty() && *n == l)
    .and_then(|i| color_tag(i as i64))
}

/// Map one variant. `original` is the image file (for the white-balance
/// conversion), `meta` its sensor size and EXIF orientation.
pub fn map_variant(
    s: &Settings,
    original: &Path,
    meta: &ImageMetaLite,
    wb: &dyn WbConverter,
    library: &[String],
) -> Mapped {
    let mut m = Mapped {
        rating: s
            .get_f64("Basic_Rating")
            .and_then(|r| Rating::new(r.round().clamp(0.0, 5.0) as u8))
            .unwrap_or_default(),
        color_tag: s
            .get_f64("Color_Tag_Index")
            .and_then(|i| color_tag(i as i64))
            .or_else(|| s.get("Basic_Label").and_then(color_from_label))
            .unwrap_or_default(),
        keywords: s
            .get("Content_Keywords")
            .map(|k| split_keywords(k, library))
            .unwrap_or_default(),
        ..Mapped::default()
    };
    let num = |k: &str, lo: f64, hi: f64| s.edit_f64(k).map_or(0.0, |v| v.clamp(lo, hi) as f32);
    m.tools.exposure = Exposure {
        exposure_ev: num("Exposure", -5.0, 5.0),
        contrast: num("Contrast", -100.0, 100.0),
        brightness: num("Brightness", -100.0, 100.0),
        saturation: num("Saturation", -100.0, 100.0),
    };
    // C1's HDR highlight slider darkens highlights for negative values;
    // Darkslide's darkens them for positive values. The others agree.
    m.tools.hdr = Hdr {
        highlights: -num("HighlightRecoveryEx", -100.0, 100.0),
        shadows: num("ShadowRecovery", -100.0, 100.0),
        whites: num("WhiteRecovery", -100.0, 100.0),
        blacks: num("BlackRecovery", -100.0, 100.0),
    };
    for (key, value) in s.al.iter().filter(|(k, _)| s.is_edit(k)) {
        match classify(key) {
            KeyClass::Unmapped(reason) if !(key == "Flip" && parse_f64(value) == Some(0.0)) => {
                m.unmapped.push((key.clone(), reason));
            }
            KeyClass::Mapped => m.mapped.push(key.clone()),
            _ => {}
        }
    }
    map_tools(s, &mut m);
    if let Some(user) = s.al.get("WhiteBalance") {
        match parse_list(user).and_then(|v| <[f64; 3]>::try_from(v).ok()) {
            Some(user) => {
                let default =
                    s.dl.get("WhiteBalance")
                        .and_then(|d| parse_list(d))
                        .and_then(|v| <[f64; 3]>::try_from(v).ok());
                if default
                    .is_some_and(|d| d.iter().zip(user).all(|(d, u)| (u / d - 1.0).abs() < 1e-4))
                {
                    m.tools.white_balance = WhiteBalance::AsShot;
                } else if let Some(w) = wb.convert(original, default, user) {
                    m.tools.white_balance = w;
                } else {
                    m.unmap(
                        "WhiteBalance",
                        "White balance: the original couldn't be read to convert it",
                    );
                }
            }
            None => m.unmap("WhiteBalance", "White balance: unreadable value"),
        }
    } else if let Some(k) = s.edit_f64("WhiteBalanceTemperature") {
        m.tools.white_balance = WhiteBalance::Custom {
            temp_k: k as f32,
            tint: s.edit_f64("WhiteBalanceTint").unwrap_or(0.0) as f32,
        };
    }
    if s.al.contains_key("WhiteBalance") {
        m.mapped
            .retain(|k| k != "WhiteBalanceTemperature" && k != "WhiteBalanceTint");
    }

    if s.al.contains_key("Crop") || s.al.contains_key("Rotation") {
        match crop(s, meta) {
            Ok(c) => m.tools.crop = c,
            Err(reason) => {
                for k in ["Crop", "Rotation"] {
                    if s.al.contains_key(k) {
                        m.unmap(k, reason);
                    }
                }
            }
        }
    }
    m
}

/// Clarity, dehaze, sharpening, noise reduction, vignetting, curves, levels
/// and chromatic aberration. Each value is C1's when the adjustment layer
/// edits it, else Darkslide's default (C1's own defaults differ for noise
/// reduction). The scales are approximate; `docs/C1_FORMAT.md` lists them.
fn map_tools(s: &Settings, m: &mut Mapped) {
    let edit = |k: &str| s.al.get(k).filter(|_| s.is_edit(k));
    let scaled = |k: &str, factor: f64, lo: f64, hi: f64| {
        edit(k)
            .and_then(|v| parse_f64(v))
            .map(|v| (v * factor).clamp(lo, hi) as f32)
    };
    let t = &mut m.tools;
    let d = t.detail;
    t.detail.clarity = scaled("Clarity", 1.0, -100.0, 100.0).unwrap_or(d.clarity);
    t.detail.structure = scaled("ClarityStructure", 1.0, -100.0, 100.0).unwrap_or(d.structure);
    t.detail.sharpen_amount = scaled("UsmAmount", 1.0, 0.0, 1000.0).unwrap_or(d.sharpen_amount);
    t.detail.sharpen_radius = scaled("UsmRadius", 1.0, 0.2, 2.5).unwrap_or(d.sharpen_radius);
    t.detail.sharpen_threshold =
        scaled("UsmThreshold", 1.0, 0.0, 10.0).unwrap_or(d.sharpen_threshold);
    t.dehaze = scaled("DehazeAmount", 1.0, -100.0, 100.0).unwrap_or(t.dehaze);
    // C1's noise defaults are 50/50/50 where Darkslide's are 25/25/50.
    let n = t.noise;
    t.noise.luminance = scaled("NrAmount", 0.5, 0.0, 100.0).unwrap_or(n.luminance);
    t.noise.color = scaled("CnrAmount", 0.5, 0.0, 100.0).unwrap_or(n.color);
    t.noise.detail = scaled("NoiseDetailsAmount", 1.0, 0.0, 100.0).unwrap_or(n.detail);
    if let Some(v) = edit("ChromaticAberEnabled").and_then(|v| parse_f64(v)) {
        t.chromatic_aberration = v != 0.0;
    }
    let mut bad: Vec<(&str, &'static str)> = Vec::new();
    // `amount|method|…`: the amount is in EV at the corners, like Darkslide's.
    if let Some(v) = edit("Vignetting") {
        match v.split('|').next().and_then(parse_f64) {
            Some(ev) => t.vignette.amount_ev = ev.clamp(-3.0, 3.0) as f32,
            None => bad.push(("Vignetting", "Vignetting: unreadable value")),
        }
    }
    for (key, slot) in [
        ("GradationCurve", &mut t.curve.rgb),
        ("GradationCurveY", &mut t.curve.luma),
        ("GradationCurveRed", &mut t.curve.red),
        ("GradationCurveGreen", &mut t.curve.green),
        ("GradationCurveBlue", &mut t.curve.blue),
    ] {
        if let Some(v) = edit(key) {
            match curve_points(v) {
                Some(p) => *slot = p,
                None => bad.push((key, "Curves: unreadable value")),
            }
        }
    }
    // Levels hold four values (master and the three channels); only levels
    // that are the same on every channel translate.
    for (key, slot) in [
        ("Shadow", &mut t.levels.in_black),
        ("Highlight", &mut t.levels.in_white),
        ("TargetShadow", &mut t.levels.out_black),
        ("TargetHighlight", &mut t.levels.out_white),
    ] {
        if let Some(v) = edit(key) {
            match parse_list(v)
                .filter(|l| !l.is_empty() && l.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-6))
            {
                Some(l) => *slot = l[0].clamp(0.0, 1.0) as f32,
                None => bad.push((
                    key,
                    "Levels: per-channel levels and the midtone have no confirmed translation",
                )),
            }
        }
    }
    for (key, reason) in bad {
        m.unmap(key, reason);
    }
}

/// A C1 curve (`x,y;x,y;…` on 0..=1) as Darkslide points; the identity
/// curve is Darkslide's empty one.
fn curve_points(text: &str) -> Option<CurvePoints> {
    let mut pts = text
        .split(';')
        .map(|p| {
            let (x, y) = p.split_once(',')?;
            Some([
                parse_f64(x)?.clamp(0.0, 1.0) as f32,
                parse_f64(y)?.clamp(0.0, 1.0) as f32,
            ])
        })
        .collect::<Option<CurvePoints>>()?;
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]));
    Some(if pts == [[0.0, 0.0], [1.0, 1.0]] {
        Vec::new()
    } else {
        pts
    })
}

/// C1's rotation for an EXIF orientation (positive = clockwise, as C1 stores it).
fn exif_quarter(o: Orientation) -> Option<i64> {
    match o {
        Orientation::Normal => Some(0),
        Orientation::Rotate90 => Some(1),
        Orientation::Rotate180 => Some(2),
        Orientation::Rotate270 => Some(-1),
        _ => None,
    }
}

/// A sensor-normalized point in the EXIF-oriented frame.
fn orient_point(o: Orientation, u: f64, v: f64) -> (f64, f64) {
    match o {
        Orientation::Normal => (u, v),
        Orientation::FlipH => (1.0 - u, v),
        Orientation::Rotate180 => (1.0 - u, 1.0 - v),
        Orientation::FlipV => (u, 1.0 - v),
        Orientation::Transpose => (v, u),
        Orientation::Rotate90 => (1.0 - v, u),
        Orientation::Transverse => (1.0 - v, 1.0 - u),
        Orientation::Rotate270 => (v, 1.0 - u),
    }
}

/// C1 `Crop` (`cx;cy;w;h`: centre in sensor pixels, size in pixels of C1's
/// displayed orientation) and `Rotation` (degrees clockwise, including the
/// EXIF orientation and user quarter turns) → Darkslide's crop.
fn crop(s: &Settings, meta: &ImageMetaLite) -> Result<Crop, &'static str> {
    let (w, h) = (f64::from(meta.width), f64::from(meta.height));
    let rotation = s.get_f64("Rotation").unwrap_or(0.0);
    let base = match exif_quarter(meta.orientation) {
        Some(q) => q,
        None => s
            .default_f64("Rotation")
            .map_or(0, |r| (r / 90.0).round() as i64),
    };
    let quarters = (rotation / 90.0).round();
    let straighten = rotation - quarters * 90.0;
    let extra = (quarters as i64 - base).rem_euclid(4);
    let mut out = Crop {
        angle_deg: (-straighten).clamp(-45.0, 45.0) as f32,
        quarter_turns: extra as u8,
        ..Crop::default()
    };
    if let Some(text) = s.al.get("Crop") {
        if w < 1.0 || h < 1.0 {
            return Err("Crop: the image size is unknown");
        }
        let [cx, cy, cw, ch] = parse_list(text)
            .and_then(|v| <[f64; 4]>::try_from(v).ok())
            .ok_or("Crop: unreadable value")?;
        if cw <= 0.0 || ch <= 0.0 {
            return Err("Crop: empty crop");
        }
        let (u, v) = orient_point(meta.orientation, cx / w, cy / h);
        let (ow, oh) = if meta.orientation.swaps_axes() {
            (h, w)
        } else {
            (w, h)
        };
        // The size is in C1's displayed orientation: odd user quarter turns swap it back.
        let (pw, ph) = if extra % 2 == 1 { (ch, cw) } else { (cw, ch) };
        out.rect = fit_rect(u, v, pw / ow, ph / oh);
        out.aspect =
            s.al.get("CropAspectRatioName")
                .and_then(|n| aspect(n, w, h));
    }
    Ok(out)
}

/// A centred rectangle moved (then clipped) to lie inside the unit square;
/// one covering (almost) everything is the full frame.
fn fit_rect(cx: f64, cy: f64, w: f64, h: f64) -> NormRect {
    let (w, h) = (w.min(1.0), h.min(1.0));
    if w >= 0.995 && h >= 0.995 {
        return NormRect::FULL;
    }
    let x = (cx - w / 2.0).clamp(0.0, 1.0 - w);
    let y = (cy - h / 2.0).clamp(0.0, 1.0 - h);
    NormRect {
        x: x as f32,
        y: y as f32,
        w: w as f32,
        h: h as f32,
    }
}

/// `CropAspectRatioName`: "Original" (the sensor's ratio) or "a:b"/"a x b".
fn aspect(name: &str, w: f64, h: f64) -> Option<Aspect> {
    let (a, b) = if name.eq_ignore_ascii_case("Original") {
        (w.round() as u32, h.round() as u32)
    } else {
        let (a, b) = name.split_once([':', 'x', 'X'])?;
        (a.trim().parse().ok()?, b.trim().parse().ok()?)
    };
    if a == 0 || b == 0 {
        return None;
    }
    let g = gcd(a, b);
    let (a, b) = (a / g, b / g);
    Some(Aspect {
        long: a.max(b),
        short: a.min(b),
    })
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;
    use crate::cos::parse_cos;
    use crate::fixtures::{self, FakeWb};
    use ds_core::adjust::{Detail, NoiseReduction};

    fn meta(o: Orientation) -> ImageMetaLite {
        ImageMetaLite {
            width: 6000,
            height: 4000,
            orientation: o,
            ..ImageMetaLite::default()
        }
    }

    fn settings(dl: &[(&str, &str)], al: &[(&str, &str)]) -> Settings {
        let keys = |kv: &[(&str, &str)]| {
            kv.iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        Settings {
            dl: keys(dl),
            al: keys(al),
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn maps_the_fixture_variant() {
        let f = parse_cos(fixtures::COS_A).unwrap();
        let m = map_variant(
            &f.variants[0].settings,
            Path::new("a.ARW"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        assert_eq!(m.rating.stars(), 4);
        assert_eq!(m.color_tag, ColorTag::Red);
        assert_eq!(m.keywords, vec!["Beach & Sun", "Family"]);
        assert_eq!(
            m.tools.exposure,
            Exposure {
                exposure_ev: 0.5,
                contrast: 10.0,
                brightness: -5.0,
                saturation: 20.0
            }
        );
        assert_eq!(
            m.tools.hdr,
            Hdr {
                highlights: 20.0,
                shadows: 30.0,
                whites: -10.0,
                blacks: 5.0
            }
        );
        let WhiteBalance::Custom { temp_k, .. } = m.tools.white_balance else {
            panic!("{:?}", m.tools.white_balance)
        };
        assert!((temp_k - 5083.333).abs() < 0.01, "{temp_k}");
        assert_eq!(
            m.tools.crop.rect,
            NormRect {
                x: 0.25,
                y: 0.25,
                w: 0.5,
                h: 0.5
            }
        );
        assert_eq!(
            m.tools.crop.angle_deg, -2.0,
            "C1 turns clockwise for positive angles"
        );
        assert_eq!(m.tools.crop.quarter_turns, 0);
        assert_eq!(m.tools.crop.aspect, Some(Aspect { long: 3, short: 2 }));
        let mut unmapped: Vec<_> = m.unmapped.iter().map(|(k, _)| k.as_str()).collect();
        unmapped.sort_unstable();
        assert_eq!(unmapped, vec!["FilmGrainAmount", "Shadow"]);
        assert_eq!(m.mapped.len(), 16, "{:?}", m.mapped);
        let t = &m.tools;
        assert_eq!(t.detail.clarity, 20.0);
        assert_eq!(t.detail.sharpen_amount, 200.0);
        assert_eq!(t.detail.sharpen_radius, Detail::default().sharpen_radius);
        assert_eq!(t.vignette.amount_ev, -1.0);
        assert_eq!(t.curve.rgb, vec![[0.0, 0.0], [0.4, 0.5], [1.0, 1.0]]);
        assert_eq!(t.curve.red.len(), 0);
        assert!(close(t.levels.in_white, 0.9));
        assert_eq!(t.levels.in_black, 0.0, "per-channel shadow: not translated");
        assert!(!m.adjustments_are_default());
        // The second variant only has a colour tag.
        let m2 = map_variant(
            &f.variants[1].settings,
            Path::new("a.ARW"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        assert_eq!(m2.color_tag, ColorTag::Green);
        assert!(m2.adjustments_are_default());
        assert!(m2.mapped.is_empty() && m2.unmapped.is_empty());
    }

    #[test]
    fn detail_noise_vignette_curves_and_levels() {
        let s = settings(
            &[
                ("NrAmount", "50"),
                ("CnrAmount", "50"),
                ("NoiseDetailsAmount", "50"),
                ("UsmAmount", "140"),
                ("UsmRadius", "0.8"),
                ("Vignetting", "0|1|0|1"),
                ("GradationCurveBlue", "0,0;1,1"),
                ("ChromaticAberEnabled", "1"),
            ],
            &[
                ("ClarityStructure", "-30"),
                ("DehazeAmount", "-20.8"),
                ("UsmAmount", "468.9"),
                ("UsmRadius", "9"),
                ("UsmThreshold", "2"),
                ("NrAmount", "80"),
                ("CnrAmount", "50"),
                ("NoiseDetailsAmount", "70"),
                ("Vignetting", "-1.6|1|0|1"),
                ("GradationCurveY", "1,1;0,0.1;0.5,0.6"),
                ("GradationCurveBlue", "0,0;1,1"),
                ("TargetShadow", "0.05;0.05;0.05;0.05"),
                ("TargetHighlight", "0.95"),
                ("ChromaticAberEnabled", "0"),
            ],
        );
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        let t = &m.tools;
        assert_eq!(t.detail.structure, -30.0);
        assert_eq!(t.detail.clarity, 0.0);
        assert!(close(t.dehaze, -20.8));
        assert!(close(t.detail.sharpen_amount, 468.9));
        assert_eq!(t.detail.sharpen_radius, 2.5, "clamped");
        assert_eq!(t.detail.sharpen_threshold, 2.0);
        assert_eq!(
            t.noise,
            NoiseReduction {
                luminance: 40.0,
                color: 25.0,
                detail: 70.0,
                ..NoiseReduction::default()
            },
            "C1's 50 is Darkslide's 25; an unchanged amount keeps Darkslide's default"
        );
        assert!(close(t.vignette.amount_ev, -1.6));
        assert_eq!(t.vignette.midpoint, 0.5);
        assert_eq!(
            t.curve.luma,
            vec![[0.0, 0.1], [0.5, 0.6], [1.0, 1.0]],
            "sorted"
        );
        assert!(t.curve.blue.is_empty(), "a reset curve is no edit");
        assert!(close(t.levels.out_black, 0.05) && close(t.levels.out_white, 0.95));
        assert!(!t.chromatic_aberration);
        assert!(m.unmapped.is_empty(), "{:?}", m.unmapped);
        let a = m.apply(&Adjustments::default());
        assert_eq!(a.presence.dehaze, t.dehaze);
        assert!(!a.lens.chromatic_aberration);
        assert_eq!(a.levels, t.levels);
        // Unreadable values are reported, not guessed.
        let s = settings(
            &[],
            &[
                ("Vignetting", "x|1"),
                ("GradationCurve", "0,0;1"),
                ("TargetHighlight", "0.9;1;1;1"),
                ("Midtone", "0.2;0.2;0.2;0.2"),
            ],
        );
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        let mut keys: Vec<_> = m.unmapped.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["GradationCurve", "Midtone", "TargetHighlight", "Vignetting"]
        );
        assert!(m.mapped.is_empty(), "{:?}", m.mapped);
        assert!(m.adjustments_are_default());
        assert_eq!(curve_points("0,0;1,1"), Some(Vec::new()));
        assert_eq!(curve_points("a,b"), None);
    }

    #[test]
    fn apply_replaces_only_the_mapped_tools() {
        let f = parse_cos(fixtures::COS_A).unwrap();
        let m = map_variant(
            &f.variants[0].settings,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        let mut base = Adjustments::default();
        base.presence.vibrance = 30.0;
        base.exposure.exposure_ev = -2.0;
        let a = m.apply(&base);
        assert_eq!(a.presence.vibrance, 30.0);
        assert_eq!(a.exposure.exposure_ev, 0.5);
        assert_eq!(a.hdr.highlights, 20.0);
    }

    #[test]
    fn unchanged_or_unreadable_white_balance() {
        let same = settings(
            &[("WhiteBalance", "2.4;1;1.6")],
            &[("WhiteBalance", "2.4;1.0;1.6")],
        );
        let m = map_variant(
            &same,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(false),
            &[],
        );
        assert_eq!(m.tools.white_balance, WhiteBalance::AsShot);
        assert!(
            m.mapped.is_empty(),
            "same as the default layer: not an edit"
        );
        let changed = settings(
            &[("WhiteBalance", "2.4;1;1.6")],
            &[("WhiteBalance", "2.0;1;1.6")],
        );
        let m = map_variant(
            &changed,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(false),
            &[],
        );
        assert_eq!(m.tools.white_balance, WhiteBalance::AsShot);
        assert_eq!(m.mapped.len(), 0);
        assert_eq!(m.unmapped[0].0, "WhiteBalance");
        assert!(m.unmapped[0].1.contains("couldn't be read"));
        let junk = settings(&[], &[("WhiteBalance", "2;1")]);
        let m = map_variant(
            &junk,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        assert!(m.unmapped[0].1.contains("unreadable"));
        // No default layer value: the converter gets the user multipliers alone.
        let no_default = settings(&[], &[("WhiteBalance", "3;1;1.5")]);
        let m = map_variant(
            &no_default,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        assert!(
            matches!(m.tools.white_balance, WhiteBalance::Custom { temp_k, .. } if close(temp_k, 7000.0))
        );
    }

    #[test]
    fn kelvin_and_tint_keys_when_there_are_no_multipliers() {
        let s = settings(
            &[],
            &[
                ("WhiteBalanceTemperature", "5600"),
                ("WhiteBalanceTint", "-3"),
            ],
        );
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(false),
            &[],
        );
        assert_eq!(
            m.tools.white_balance,
            WhiteBalance::Custom {
                temp_k: 5600.0,
                tint: -3.0
            }
        );
        assert_eq!(m.mapped.len(), 2);
        let both = settings(
            &[("WhiteBalance", "2;1;2")],
            &[
                ("WhiteBalance", "2;1;2"),
                ("WhiteBalanceTemperature", "5600"),
            ],
        );
        let m = map_variant(
            &both,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(false),
            &[],
        );
        assert_eq!(
            m.tools.white_balance,
            WhiteBalance::AsShot,
            "multipliers win"
        );
        assert_eq!(m.mapped.len(), 0);
    }

    #[test]
    fn portrait_crop_uses_the_exif_orientation() {
        // C1 -90 = EXIF 8 (Rotate270). Centre in sensor pixels, size as displayed.
        let s = settings(&[("Rotation", "-90")], &[("Crop", "2400;2000;2000;3000")]);
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Rotate270),
            &FakeWb(true),
            &[],
        );
        // Sensor (0.4, 0.5) → oriented (0.5, 0.6); size 2000/4000 × 3000/6000.
        let r = m.tools.crop.rect;
        assert!(
            close(r.x, 0.25) && close(r.y, 0.35) && close(r.w, 0.5) && close(r.h, 0.5),
            "{r:?}"
        );
        assert_eq!(m.tools.crop.quarter_turns, 0);
        assert_eq!(m.tools.crop.angle_deg, 0.0);
        // EXIF 6: C1 +90, straightened by -3° (C1 87).
        let s = settings(
            &[("Rotation", "90")],
            &[("Rotation", "87"), ("Crop", "3000;1000;2000;3000")],
        );
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Rotate90),
            &FakeWb(true),
            &[],
        );
        let r = m.tools.crop.rect;
        // Sensor (0.5, 0.25) → (0.75, 0.5).
        assert!(
            close(r.x + r.w / 2.0, 0.75) && close(r.y + r.h / 2.0, 0.5),
            "{r:?}"
        );
        assert_eq!(m.tools.crop.angle_deg, 3.0);
        assert_eq!(m.tools.crop.quarter_turns, 0);
        assert_eq!(m.tools.crop.aspect, None);
    }

    #[test]
    fn user_quarter_turns_swap_the_crop_size() {
        // A landscape image the user turned once clockwise in C1 (0 → 90).
        let s = settings(
            &[("Rotation", "0")],
            &[("Rotation", "90"), ("Crop", "3000;2000;2000;3000")],
        );
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        assert_eq!(m.tools.crop.quarter_turns, 1);
        let r = m.tools.crop.rect;
        assert!(close(r.w, 0.5) && close(r.h, 0.5), "{r:?}");
        // Turned the other way: three clockwise quarter turns.
        let s = settings(&[("Rotation", "0")], &[("Rotation", "-90")]);
        assert_eq!(
            map_variant(
                &s,
                Path::new("a"),
                &meta(Orientation::Normal),
                &FakeWb(true),
                &[]
            )
            .tools
            .crop
            .quarter_turns,
            3
        );
        // Without a known EXIF quarter (a flipped orientation) the default layer decides.
        let s = settings(&[("Rotation", "-90")], &[("Rotation", "-92")]);
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Transpose),
            &FakeWb(true),
            &[],
        );
        assert_eq!(
            (m.tools.crop.quarter_turns, m.tools.crop.angle_deg),
            (0, 2.0)
        );
    }

    #[test]
    fn crops_are_kept_inside_and_full_crops_are_none() {
        let s = settings(&[], &[("Crop", "2885;1943;6000;4000")]);
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        assert_eq!(m.tools.crop.rect, NormRect::FULL);
        let s = settings(&[], &[("Crop", "300;200;3000;2000")]);
        let r = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        )
        .tools
        .crop
        .rect;
        assert_eq!((r.x, r.y, r.w, r.h), (0.0, 0.0, 0.5, 0.5), "moved inside");
        for (bad, why) in [("1;2;3", "unreadable"), ("1;2;0;5", "empty")] {
            let s = settings(&[], &[("Crop", bad), ("Rotation", "3")]);
            let m = map_variant(
                &s,
                Path::new("a"),
                &meta(Orientation::Normal),
                &FakeWb(true),
                &[],
            );
            assert_eq!(m.tools.crop, Crop::default());
            assert_eq!(m.unmapped.len(), 2, "{:?}", m.unmapped);
            assert!(m.unmapped.iter().all(|(_, r)| r.contains(why)));
            assert_eq!(m.mapped.len(), 0);
        }
        let s = settings(&[], &[("Crop", "1;1;1;1")]);
        let m = map_variant(
            &s,
            Path::new("a"),
            &ImageMetaLite::default(),
            &FakeWb(true),
            &[],
        );
        assert!(m.unmapped[0].1.contains("size is unknown"));
    }

    #[test]
    fn aspect_names() {
        assert_eq!(
            aspect("Original", 6000.0, 4000.0),
            Some(Aspect { long: 3, short: 2 })
        );
        assert_eq!(
            aspect("4:5", 6000.0, 4000.0),
            Some(Aspect { long: 5, short: 4 })
        );
        assert_eq!(
            aspect("16 x 9", 1.0, 1.0),
            Some(Aspect { long: 16, short: 9 })
        );
        assert_eq!(aspect("None", 1.0, 1.0), None);
        assert_eq!(aspect("0:1", 1.0, 1.0), None);
    }

    #[test]
    fn every_orientation_maps_points() {
        use Orientation::*;
        let cases = [
            (Normal, (0.2, 0.3)),
            (FlipH, (0.8, 0.3)),
            (Rotate180, (0.8, 0.7)),
            (FlipV, (0.2, 0.7)),
            (Transpose, (0.3, 0.2)),
            (Rotate90, (0.7, 0.2)),
            (Transverse, (0.7, 0.8)),
            (Rotate270, (0.3, 0.8)),
        ];
        for (o, (x, y)) in cases {
            let (a, b) = orient_point(o, 0.2, 0.3);
            assert!(
                (a - x).abs() < 1e-9 && (b - y).abs() < 1e-9,
                "{o:?}: {a},{b}"
            );
        }
        assert_eq!(exif_quarter(Rotate180), Some(2));
        assert_eq!(exif_quarter(FlipH), None);
    }

    #[test]
    fn ratings_tags_and_labels() {
        let s = settings(
            &[("Basic_Rating", "0")],
            &[
                ("Basic_Rating", "7"),
                ("Color_Tag_Index", "9"),
                ("Basic_Label", "Purple"),
            ],
        );
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        assert_eq!(m.rating.stars(), 5, "clamped");
        assert_eq!(
            m.color_tag,
            ColorTag::Purple,
            "an unknown index falls back to the label"
        );
        let s = settings(&[], &[("Basic_Label", "Approved")]);
        let m = map_variant(
            &s,
            Path::new("a"),
            &meta(Orientation::Normal),
            &FakeWb(true),
            &[],
        );
        assert_eq!(m.color_tag, ColorTag::None);
        assert!(m.unmapped.is_empty(), "labels are organization, not edits");
        for (i, t) in [
            ColorTag::Orange,
            ColorTag::Yellow,
            ColorTag::Blue,
            ColorTag::Pink,
        ]
        .into_iter()
        .enumerate()
        {
            let idx = [2, 3, 5, 6][i].to_string();
            let s = settings(&[], &[("Color_Tag_Index", idx.as_str())]);
            assert_eq!(
                map_variant(
                    &s,
                    Path::new("a"),
                    &meta(Orientation::Normal),
                    &FakeWb(true),
                    &[]
                )
                .color_tag,
                t
            );
        }
        // Flip 0 in the adjustment layer is not an edit; any other value is unmapped.
        let s = settings(&[], &[("Flip", "0")]);
        assert_eq!(
            map_variant(
                &s,
                Path::new("a"),
                &meta(Orientation::Normal),
                &FakeWb(true),
                &[]
            )
            .unmapped
            .len(),
            0
        );
        let s = settings(&[], &[("Flip", "1")]);
        assert_eq!(
            map_variant(
                &s,
                Path::new("a"),
                &meta(Orientation::Normal),
                &FakeWb(true),
                &[]
            )
            .unmapped
            .len(),
            1
        );
    }

    #[test]
    fn keyword_splitting() {
        let lib = vec!["Smith, John".to_owned()];
        assert_eq!(
            split_keywords("Beach;Smith, John", &lib),
            vec!["Beach", "Smith, John"]
        );
        assert_eq!(split_keywords("Smith, John", &lib), vec!["Smith, John"]);
        assert_eq!(split_keywords("a, b; c", &[]), vec!["a", "b", "c"]);
        assert_eq!(
            split_keywords("Places|Iceland;Places|Iceland|;Beach\nBeach", &[]),
            vec!["Iceland", "Beach"]
        );
        assert_eq!(split_keywords("  ", &[]).len(), 0);
        assert_eq!(split_keywords(";|;", &[]).len(), 0);
    }

    #[test]
    fn classification_covers_the_vocabulary() {
        for k in [
            "Exposure",
            "Crop",
            "WhiteBalanceTint",
            "Clarity",
            "ClarityStructure",
            "DehazeAmount",
            "Vignetting",
            "UsmAmount",
            "UsmRadius",
            "UsmThreshold",
            "NrAmount",
            "CnrAmount",
            "NoiseDetailsAmount",
            "GradationCurve",
            "GradationCurveRed",
            "Highlight",
            "TargetShadow",
            "ChromaticAberEnabled",
        ] {
            assert_eq!(classify(k), KeyClass::Mapped, "{k}");
        }
        for k in [
            "CalculatedDehazeColor",
            "CropAspectRatioName",
            "ClarityMethod",
        ] {
            assert_eq!(classify(k), KeyClass::Derived);
        }
        let reason = |k| match classify(k) {
            KeyClass::Unmapped(r) => r,
            other => panic!("{k}: {other:?}"),
        };
        for (k, word) in [
            ("Midtone", "Levels"),
            ("UsmHaloControl", "Sharpening"),
            ("SurfaceSmoothnessAmount", "Noise"),
            ("DehazeColor", "Dehaze"),
            ("ColorBalanceShadow", "Colour"),
            ("ColorCorrections", "Colour"),
            ("BwRed", "Black"),
            ("LensGeoAbr", "Lens"),
            ("GeometryKeystoneTiltX", "Keystone"),
            ("FilmCurveSettings", "Base"),
            ("PurpleFringingAmount", "Lens"),
            ("ICCProfile", "Base"),
            ("FilmGrainAmount", "grain"),
            ("Moire", "Moiré"),
            ("Spots", "Spot"),
            ("Contact_Creator", "IPTC"),
            ("Flip", "Flip"),
            ("SomethingNew", "Not recognized"),
        ] {
            assert!(reason(k).contains(word), "{k}: {}", reason(k));
        }
        assert_eq!(classify("Basic_Label"), KeyClass::Organization);
    }

    #[test]
    fn database_columns_become_cos_keys() {
        for (c, k) in [
            ("ZEXPOSURE", "Exposure"),
            ("ZHIGHLIGHTRECOVERY", "HighlightRecoveryEx"),
            ("ZBASIC_RATING", "Basic_Rating"),
            ("ZCOLOR_TAG_INDEX", "Color_Tag_Index"),
            ("ZCONTENT_KEYWORDS", "Content_Keywords"),
            ("ZCROPASPECTRATIONAME", "CropAspectRatioName"),
            ("ZLENSVIGNETTING", "Vignetting"),
            ("ZGRADATIONCURVERED", "GradationCurveRed"),
            ("ZGRADATIONCURVE", "GradationCurve"),
            ("ZGRADATIONCURVEX", "GradationCurve"),
            ("ZUSMAMOUNT", "UsmAmount"),
            ("ZUSMHALOCONTROL", "UsmHaloControl"),
            ("ZUSMFOO", "UsmFOO"),
            ("ZNRAMOUNT", "NrAmount"),
            ("ZFOO", "FOO"),
            ("BARE", "BARE"),
        ] {
            assert_eq!(key_for_column(c), k);
        }
        assert!(
            matches!(classify(&key_for_column("ZUSMHALOCONTROL")), KeyClass::Unmapped(r) if r.contains("halo"))
        );
    }
}
