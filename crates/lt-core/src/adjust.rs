//! The non-destructive adjustment model stored in each variant's sidecar.
//!
//! Every struct's `Default` is the identity (renders the image unchanged apart
//! from the base camera look). Values are in UI units; the pipeline owns the
//! mapping to math. Unknown fields are ignored on read so newer sidecars still
//! load in older builds (`#[serde(default)]` everywhere).

use serde::{Deserialize, Serialize};

use crate::geom::NormRect;

/// Serialization writes the struct as-is (call [`Adjustments::sanitize`] first;
/// non-finite floats would become `null`). Deserialization reads `null` floats
/// leniently and always returns a sanitized value.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, remote = "Self")]
pub struct Adjustments {
    /// The base look the develop starts from. New edits default to
    /// [`Profile::Standard`]; a sidecar written before profiles existed has no
    /// `profile` key and reads as [`Profile::Neutral`] (the look it was edited
    /// under), so old edits render unchanged. Every build that knows the field
    /// writes it, so its absence is unambiguous: no schema bump is needed.
    #[serde(default = "Profile::legacy")]
    pub profile: Profile,
    pub white_balance: WhiteBalance,
    pub exposure: Exposure,
    /// Vibrance, texture and dehaze (Lightroom's "Presence").
    pub presence: Presence,
    pub hdr: Hdr,
    pub levels: Levels,
    pub curve: Curve,
    pub color_balance: ColorBalance,
    pub color_editor: ColorEditor,
    pub detail: Detail,
    pub noise: NoiseReduction,
    pub vignette: Vignette,
    pub lens: LensCorrection,
    pub crop: Crop,
    pub layers: Vec<Layer>,
    /// Heal and clone spots, applied in order before any other develop step.
    pub spots: Vec<Spot>,
    /// Content-aware removals, applied in order to the RAW samples (before the spots and
    /// every other step).
    pub removals: Vec<Removal>,
    /// Where the people that per-person mask shapes (`MaskShape::Ai::person`) refer to
    /// were when the shapes were made: by person index, the normalized `[x0, y0, x1, y1]`
    /// box of that person (oriented frame). The app re-matches the indices with them when
    /// its people order changes (another OS's Vision, another analysis image).
    #[serde(
        skip_serializing_if = "std::collections::BTreeMap::is_empty",
        deserialize_with = "person_boxes"
    )]
    pub person_boxes: std::collections::BTreeMap<u32, [f32; 4]>,
    /// Lens blur, glow, grain and halation.
    pub effects: crate::effects::Effects,
    /// The one-touch look last applied, for its intensity slider (app metadata; the
    /// pipeline ignores it). A malformed record reads as none.
    #[serde(deserialize_with = "crate::effects::look")]
    pub look: Option<crate::effects::AppliedLook>,
}

/// [`Adjustments::person_boxes`] read leniently: `null` coordinates become NaN (the box
/// is then dropped by `sanitize`), and entries that aren't an index and a box (a newer
/// build's) are ignored.
fn person_boxes<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<std::collections::BTreeMap<u32, [f32; 4]>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Box([Option<f32>; 4]),
        Other(serde::de::IgnoredAny),
    }
    let raw = std::collections::BTreeMap::<String, Entry>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(k, e)| match e {
            Entry::Box(b) => Some((k.parse().ok()?, b.map(|v| v.unwrap_or(f32::NAN)))),
            Entry::Other(_) => None,
        })
        .collect())
}

impl Serialize for Adjustments {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        Adjustments::serialize(self, s)
    }
}

impl<'de> Deserialize<'de> for Adjustments {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Adjustments::deserialize(d)?.sanitized())
    }
}

impl Adjustments {
    /// True when nothing differs from default (used to show the "edited" badge). An
    /// untouched sidecar from before profiles (read as [`Profile::Neutral`]) is
    /// unedited too.
    pub fn is_identity(&self) -> bool {
        let neutral = Self {
            profile: Profile::Neutral,
            ..Self::default()
        };
        *self == Self::default() || *self == neutral
    }
}

/// The base look ("camera profile"): the colour twist and tone curve the develop
/// pipeline applies before any user tool.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Profile {
    /// Luxtable's default look: a richer contrast curve with a filmic shoulder, warm
    /// skin, natural blues and greens.
    #[default]
    Standard,
    /// The original look: brightness matched to Apple's RAW engine, a gentle curve and
    /// no colour twist.
    Neutral,
    /// Standard with more contrast and saturation (skin held back).
    Vivid,
    /// An imported Adobe camera profile (.dcp), by the BLAKE3 hex digest of the file and
    /// its profile name (shown when the file is missing).
    Dcp { hash: String, name: String },
    /// A profile this build doesn't know (written by a newer one), kept verbatim so a
    /// save never downgrades it. It renders as Standard, flagged like a missing import.
    Other(serde_json::Value),
}

/// The wire form of the profiles this build knows (`{"kind": …}`).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum KnownProfile {
    Standard,
    Neutral,
    Vivid,
    Dcp { hash: String, name: String },
}

impl Serialize for Profile {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Standard => KnownProfile::Standard.serialize(s),
            Self::Neutral => KnownProfile::Neutral.serialize(s),
            Self::Vivid => KnownProfile::Vivid.serialize(s),
            Self::Dcp { hash, name } => KnownProfile::Dcp {
                hash: hash.clone(),
                name: name.clone(),
            }
            .serialize(s),
            Self::Other(v) => v.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Profile {
    /// A known kind (extra fields ignored, as everywhere in a sidecar); any other
    /// object (a newer kind, a malformed import) is kept verbatim as
    /// [`Profile::Other`]. A non-object is an error, as before.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        if !v.is_object() {
            return Err(serde::de::Error::custom("a profile is an object"));
        }
        Ok(match KnownProfile::deserialize(&v) {
            Ok(KnownProfile::Standard) => Self::Standard,
            Ok(KnownProfile::Neutral) => Self::Neutral,
            Ok(KnownProfile::Vivid) => Self::Vivid,
            Ok(KnownProfile::Dcp { hash, name }) => Self::Dcp { hash, name },
            Err(_) => Self::Other(v),
        })
    }
}

impl Profile {
    /// What a sidecar without a `profile` key means: the look before profiles existed.
    pub fn legacy() -> Self {
        Self::Neutral
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum WhiteBalance {
    /// Camera as-shot multipliers.
    #[default]
    AsShot,
    /// Correlated color temperature in Kelvin plus green/magenta tint (-150..=150).
    Custom {
        #[serde(deserialize_with = "crate::lenient::f32")]
        temp_k: f32,
        #[serde(deserialize_with = "crate::lenient::f32")]
        tint: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Exposure {
    /// Stops, -5..=5.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub exposure_ev: f32,
    /// -100..=100 for the rest.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub contrast: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub brightness: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub saturation: f32,
}

impl Default for Exposure {
    fn default() -> Self {
        Self {
            exposure_ev: 0.0,
            contrast: 0.0,
            brightness: 0.0,
            saturation: 0.0,
        }
    }
}

/// Presence: all -100..=100, 0 = identity.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Presence {
    /// Saturation weighted toward low-saturation colours, with skin tones protected.
    /// Negative desaturates the muted colours first.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub vibrance: f32,
    /// Mid-frequency detail (between sharpening and clarity); negative smooths it.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub texture: f32,
    /// Haze removal (dark-channel prior); negative adds haze.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub dehaze: f32,
}

/// Capture One "High Dynamic Range" tool. All -100..=100.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Hdr {
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub highlights: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub shadows: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub whites: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub blacks: f32,
}

/// Input/output levels on 0..=1 display-referred values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Levels {
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub in_black: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub in_white: f32,
    /// Midtone gamma, 0.1..=10.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub gamma: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub out_black: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub out_white: f32,
}

impl Default for Levels {
    fn default() -> Self {
        Self {
            in_black: 0.0,
            in_white: 1.0,
            gamma: 1.0,
            out_black: 0.0,
            out_white: 1.0,
        }
    }
}

/// Control points on 0..=1, sorted by x. Empty = identity. Interpolated as a
/// monotone cubic spline in the pipeline.
pub type CurvePoints = Vec<[f32; 2]>;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Curve {
    #[serde(deserialize_with = "crate::lenient::pairs")]
    pub rgb: CurvePoints,
    #[serde(deserialize_with = "crate::lenient::pairs")]
    pub luma: CurvePoints,
    #[serde(deserialize_with = "crate::lenient::pairs")]
    pub red: CurvePoints,
    #[serde(deserialize_with = "crate::lenient::pairs")]
    pub green: CurvePoints,
    #[serde(deserialize_with = "crate::lenient::pairs")]
    pub blue: CurvePoints,
}

/// One color wheel: hue in degrees, saturation 0..=1, lightness -1..=1.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Wheel {
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub hue: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub sat: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub lum: f32,
}

/// Capture One 3-way color balance plus a master wheel.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorBalance {
    pub master: Wheel,
    pub shadows: Wheel,
    pub midtones: Wheel,
    pub highlights: Wheel,
}

/// A hue-range edit: center hue (degrees) and width (degrees) select the range,
/// with `smoothness` (0..=1) feathering the edge. Shifts are -100..=100 except
/// `hue_shift` which is degrees (-30..=30).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorRange {
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub center_hue: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub width: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub smoothness: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub hue_shift: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub saturation: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub lightness: f32,
}

impl Default for ColorRange {
    fn default() -> Self {
        Self {
            center_hue: 0.0,
            width: 45.0,
            smoothness: 0.5,
            hue_shift: 0.0,
            saturation: 0.0,
            lightness: 0.0,
        }
    }
}

/// The eight Basic-mode bands: red, orange, yellow, green, cyan, blue, purple, magenta.
pub const BASIC_BAND_HUES: [f32; 8] = [0.0, 30.0, 60.0, 120.0, 180.0, 240.0, 270.0, 300.0];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorEditor {
    pub basic: [ColorRange; 8],
    /// Advanced mode: user-picked ranges.
    pub advanced: Vec<ColorRange>,
    /// Skin-tone uniformity: pulls hue/sat/lightness toward the picked tone.
    pub skin: Option<SkinTone>,
}

impl Default for ColorEditor {
    fn default() -> Self {
        let basic = BASIC_BAND_HUES.map(|h| ColorRange {
            center_hue: h,
            ..ColorRange::default()
        });
        Self {
            basic,
            advanced: Vec::new(),
            skin: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkinTone {
    pub range: ColorRange,
    /// 0..=100 each.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub hue_uniformity: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub saturation_uniformity: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub lightness_uniformity: f32,
}

/// Clarity/structure and output sharpening.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Detail {
    /// -100..=100.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub clarity: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub structure: f32,
    /// 0..=1000, like Capture One.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub sharpen_amount: f32,
    /// Pixels, 0.2..=2.5.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub sharpen_radius: f32,
    /// 0..=10.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub sharpen_threshold: f32,
}

impl Default for Detail {
    fn default() -> Self {
        Self {
            clarity: 0.0,
            structure: 0.0,
            sharpen_amount: 140.0,
            sharpen_radius: 0.8,
            sharpen_threshold: 1.0,
        }
    }
}

/// 0..=100 each.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NoiseReduction {
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub luminance: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub color: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub detail: f32,
    /// AI Denoise: develop a learned denoiser's output (computed once per photo by the
    /// app, see `lt_pipeline::denoise`) instead of the RAW's own demosaic.
    pub ai_denoise: bool,
    /// AI Denoise strength, 0..=100: the share of the denoised image blended over the
    /// original. It also takes over the luminance slider in proportion.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub ai_strength: f32,
}

impl Default for NoiseReduction {
    fn default() -> Self {
        Self {
            luminance: 25.0,
            color: 25.0,
            detail: 50.0,
            ai_denoise: false,
            ai_strength: 50.0,
        }
    }
}

impl NoiseReduction {
    /// AI Denoise strength as a fraction, `0` when AI Denoise is off.
    pub fn ai_fraction(&self) -> f32 {
        if self.ai_denoise && self.ai_strength.is_finite() {
            (self.ai_strength / 100.0).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Vignette {
    /// Stops at the corner, -3..=3.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub amount_ev: f32,
    /// 0..=1, where falloff begins.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub midpoint: f32,
}

impl Default for Vignette {
    fn default() -> Self {
        Self {
            amount_ev: 0.0,
            midpoint: 0.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LensCorrection {
    /// Lensfun lens identifier; `None` = auto-detect from EXIF.
    pub profile: Option<String>,
    pub enabled: bool,
    /// 0..=1 strength of the profile corrections.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub distortion: f32,
    pub chromatic_aberration: bool,
    /// 0..=1 share of the profiled vignetting removed (1 = fully flat corners). The
    /// default 0.5 matches in-camera shading compensation.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub light_falloff: f32,
    /// Manual keystone, degrees.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub keystone_vertical: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub keystone_horizontal: f32,
}

impl Default for LensCorrection {
    fn default() -> Self {
        Self {
            profile: None,
            enabled: true,
            distortion: 1.0,
            chromatic_aberration: true,
            light_falloff: 0.5,
            keystone_vertical: 0.0,
            keystone_horizontal: 0.0,
        }
    }
}

/// Aspect ratio lock, stored landscape-first (e.g. 3:2 not 2:3). Orientation
/// follows the crop rectangle so pasting onto a portrait image stays sensible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aspect {
    pub long: u32,
    pub short: u32,
}

/// Crop, straighten, quarter turns and flips. **This is the contract lt-pipeline
/// renders and every writer stores**; keep them identical.
///
/// Stages, in order (all normalized coordinates are y-down, 0..=1):
/// 1. **Oriented frame**: the image after EXIF orientation (what embedded previews show).
/// 2. **Crop + straighten**: `rect` is an axis-aligned rectangle *in the oriented frame*
///    (before quarter turns and before straighten). The output samples that rectangle
///    rotated about its own center by `angle_deg`; seen on screen, the **content turns
///    counter-clockwise for positive angles** (lt-pipeline `geometry::plan`, test
///    `straighten_rotates_about_crop_center`). To level a horizon that rises to the right
///    by `t` degrees, store `angle_deg = -t`.
/// 3. **Quarter turns**: `quarter_turns` clockwise turns of the cropped, straightened result.
/// 4. **Flips**: horizontal / vertical mirror of the final frame.
///
/// So quarter turns or a new straighten angle never change which content the crop keeps
/// (the rect's center stays on the same image content).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Crop {
    /// Oriented (EXIF-applied), pre-quarter-turn, pre-straighten normalized coordinates.
    pub rect: NormRect,
    /// Straighten angle, degrees, -45..=45; positive turns the content counter-clockwise.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub angle_deg: f32,
    pub aspect: Option<Aspect>,
    /// Clockwise quarter turns (0..=3), applied after crop and straighten.
    pub quarter_turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
}

// ---------------------------------------------------------------- layers ---

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layer {
    /// Stable id within the variant; survives reordering. Used for per-layer paste.
    pub id: u64,
    pub name: String,
    pub enabled: bool,
    /// 0..=1.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub opacity: f32,
    pub adjustments: LayerAdjustments,
    pub mask: Mask,
}

impl Default for Layer {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            enabled: true,
            opacity: 1.0,
            adjustments: LayerAdjustments::default(),
            mask: Mask::default(),
        }
    }
}

/// The tools of a local-adjustment layer: everything global has except crop, lens
/// correction and noise reduction. All default to identity.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LayerAdjustments {
    /// White balance shift relative to the global white balance.
    pub white_balance: Option<WhiteBalanceDelta>,
    /// Exposure (stops), contrast, brightness, saturation.
    pub exposure: Exposure,
    /// Vibrance, texture, dehaze.
    pub presence: Presence,
    pub hdr: Hdr,
    pub curve: Curve,
    pub color_balance: ColorBalance,
    /// Colour-editor ranges applied under the mask.
    pub color_editor: Vec<ColorRange>,
    /// -100..=100.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub clarity: f32,
    /// -100..=100.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub structure: f32,
    /// Local sharpening, -100..=100: positive sharpens on top of the global
    /// sharpening, negative softens (skin).
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub sharpening: f32,
}

/// Relative WB shift on a layer.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WhiteBalanceDelta {
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub temp_k: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub tint: f32,
}

/// Largest Gaussian feather (normalized to the long edge) of the masks that feather in
/// output space: the whole-mask feather, a colour range's and a brush shape's (auto-masked
/// strokes). A tile or 1:1 region only has ~3σ of context around it at full resolution
/// up to this, so a wider feather would render differently in exports and zoomed views
/// than in the fit view. The UI's feather sliders stop here.
pub const MAX_PLANE_FEATHER: f32 = 0.05;

/// A mask is an ordered list of shapes combined into one alpha channel.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Mask {
    pub shapes: Vec<MaskShape>,
    pub invert: bool,
    /// Gaussian feather in normalized units of the long edge.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub feather: f32,
    /// Luminance range refinement applied on top of shapes.
    pub luma_range: Option<LumaRange>,
    /// Colour range refinement applied on top of shapes (and the luma range).
    pub color_range: Option<ColorRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaskOp {
    #[default]
    Add,
    Subtract,
    Intersect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MaskShape {
    /// Full-image fill (the "fill mask" action).
    Fill { op: MaskOp },
    Brush {
        op: MaskOp,
        strokes: Vec<BrushStroke>,
        /// Extra Gaussian feather, normalized to the long edge (0 = strokes as painted).
        #[serde(default, deserialize_with = "crate::lenient::f32")]
        feather: f32,
    },
    /// Linear gradient: fully on at `start`, fully off at `end` (the two handles). The
    /// start→end distance is its feather.
    Linear {
        op: MaskOp,
        #[serde(deserialize_with = "crate::lenient::pair")]
        start: [f32; 2],
        #[serde(deserialize_with = "crate::lenient::pair")]
        end: [f32; 2],
    },
    /// Elliptical gradient: `center`, `radius` and `angle_deg` are its handles;
    /// `feather` (0..=1 of the radius) is the soft edge width.
    ///
    /// Units: `center` is normalized per axis (0..=1 of width and height, like every
    /// point). `radius` is the ellipse's two semi-axes (before the turn), both in
    /// units of the oriented image's **long edge**, so `[r, r]` is a circle on any aspect
    /// ratio. `angle_deg` turns the ellipse clockwise (y down) in pixel space.
    Radial {
        op: MaskOp,
        #[serde(deserialize_with = "crate::lenient::pair")]
        center: [f32; 2],
        #[serde(deserialize_with = "crate::lenient::pair")]
        radius: [f32; 2],
        #[serde(deserialize_with = "crate::lenient::f32")]
        angle_deg: f32,
        #[serde(deserialize_with = "crate::lenient::f32")]
        feather: f32,
    },
    ColorRange {
        op: MaskOp,
        range: ColorRange,
        /// Gaussian feather, normalized to the long edge.
        #[serde(default, deserialize_with = "crate::lenient::f32")]
        feather: f32,
    },
    /// On-device segmentation (Apple Vision, from the app's `MaskSource`); bitmap
    /// cached, re-generated on demand.
    Ai {
        op: MaskOp,
        target: AiTarget,
        /// Gaussian feather, normalized to the long edge.
        #[serde(default, deserialize_with = "crate::lenient::f32")]
        feather: f32,
        /// Which person a people target ([`AiTarget::is_people`]) covers: the index in the
        /// app's person order (largest first); `None` = all people. Ignored by the other
        /// targets.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        person: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiTarget {
    Subject,
    Sky,
    Person,
    Background,
    /// People sub-parts (face/human parsing in the app): the whole face (skin, brows,
    /// eyes, nose, lips; not hair or ears).
    Face,
    /// Visible skin: face, neck, ears, arms, legs.
    Skin,
    /// Eyes (iris and sclera).
    Eyes,
    /// Teeth (the inner mouth's bright part).
    Teeth,
    Hair,
    /// Clothes, hats and accessories worn by the person.
    Clothes,
}

impl AiTarget {
    /// Every target, in UI order.
    pub const ALL: [AiTarget; 10] = [
        Self::Subject,
        Self::Sky,
        Self::Person,
        Self::Background,
        Self::Face,
        Self::Skin,
        Self::Eyes,
        Self::Teeth,
        Self::Hair,
        Self::Clothes,
    ];

    /// Targets that exist once per person (`MaskShape::Ai::person` selects one).
    pub fn is_people(self) -> bool {
        !matches!(self, Self::Subject | Self::Sky | Self::Background)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrushStroke {
    /// Normalized points.
    #[serde(deserialize_with = "crate::lenient::pairs")]
    pub points: Vec<[f32; 2]>,
    /// Normalized to the long edge.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub size: f32,
    /// 0..=1.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub hardness: f32,
    /// 0..=1.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub flow: f32,
    pub erase: bool,
    /// Edge-aware: the stroke is refined to the image edges under its footprint.
    pub auto_mask: bool,
}

impl Default for BrushStroke {
    fn default() -> Self {
        Self {
            points: Vec::new(),
            size: 0.05,
            hardness: 0.5,
            flow: 1.0,
            erase: false,
            auto_mask: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LumaRange {
    /// 0..=1 bounds with soft falloff widths.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub low: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub high: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub low_falloff: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub high_falloff: f32,
}

impl Default for LumaRange {
    fn default() -> Self {
        Self {
            low: 0.0,
            high: 1.0,
            low_falloff: 0.1,
            high_falloff: 0.1,
        }
    }
}

// ----------------------------------------------------------------- spots ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpotKind {
    /// Texture from the source, colour and tone from the destination's surroundings.
    #[default]
    Heal,
    /// Copy source to destination.
    Clone,
}

/// A heal or clone spot. Points are normalized in the **oriented frame** (EXIF applied,
/// before crop/straighten, like [`Crop::rect`]), y-down; `radius` is normalized to the
/// long edge.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spot {
    pub kind: SpotKind,
    /// Destination center.
    #[serde(deserialize_with = "crate::lenient::pair")]
    pub center: [f32; 2],
    /// 0..=0.5 of the long edge.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub radius: f32,
    /// Source center (same radius).
    #[serde(deserialize_with = "crate::lenient::pair")]
    pub source: [f32; 2],
    /// 0..=1: soft edge width as a share of the radius.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub feather: f32,
    /// 0..=1.
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub opacity: f32,
}

impl Default for Spot {
    fn default() -> Self {
        Self {
            kind: SpotKind::Heal,
            center: [0.5, 0.5],
            radius: 0.02,
            source: [0.55, 0.5],
            feather: 0.5,
            opacity: 1.0,
        }
    }
}

// -------------------------------------------------------------- removals ---

/// A content-aware removal (Lightroom's "Remove"): the brushed region is replaced with
/// plausible background, inpainted on device (the app's model, else a classical
/// `PatchMatch` fill) and composited into the RAW samples, so every later step (crop,
/// straighten, develop) treats it like the photo itself.
///
/// Strokes are normalized in the corrected E frame like brush mask strokes (y-down;
/// `size` normalized to the long edge); `erase` strokes take area back out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Removal {
    pub strokes: Vec<BrushStroke>,
    /// Soft edge of the fill, normalized to the long edge (0..=`MAX_REMOVAL_FEATHER`).
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub feather: f32,
}

/// Widest removal feather (normalized to the long edge).
pub const MAX_REMOVAL_FEATHER: f32 = 0.02;

/// Largest Remove brush size (normalized to the long edge; the app's slider maximum).
pub const MAX_REMOVAL_SIZE: f32 = 0.3;

impl Default for Removal {
    fn default() -> Self {
        Self {
            strokes: Vec::new(),
            feather: 0.002,
        }
    }
}

#[cfg(test)]
mod tests {
    // Exact float equality is intended: defaults and clamps produce exact values.
    #![allow(
        clippy::float_cmp,
        clippy::too_many_lines,
        clippy::field_reassign_with_default
    )]

    use serde_json::{Value, json};

    use super::*;

    /// An `Adjustments` with every group and every mask shape non-default.
    fn everything() -> Adjustments {
        let range = ColorRange {
            center_hue: 25.0,
            width: 30.0,
            smoothness: 0.2,
            hue_shift: 4.0,
            saturation: -10.0,
            lightness: 5.0,
        };
        let mut color_editor = ColorEditor::default();
        color_editor.basic[3].saturation = 20.0;
        color_editor.advanced.push(range);
        color_editor.skin = Some(SkinTone {
            range,
            hue_uniformity: 30.0,
            saturation_uniformity: 20.0,
            lightness_uniformity: 10.0,
        });
        let stroke = BrushStroke {
            points: vec![[0.1, 0.2], [0.3, 0.4]],
            size: 0.02,
            hardness: 0.8,
            flow: 0.5,
            erase: true,
            auto_mask: true,
        };
        let mask = Mask {
            shapes: vec![
                MaskShape::Fill { op: MaskOp::Add },
                MaskShape::Brush {
                    op: MaskOp::Subtract,
                    strokes: vec![stroke],
                    feather: 0.01,
                },
                MaskShape::Linear {
                    op: MaskOp::Intersect,
                    start: [0.0, 0.0],
                    end: [1.0, 1.0],
                },
                MaskShape::Radial {
                    op: MaskOp::Add,
                    center: [0.5, 0.5],
                    radius: [0.2, 0.3],
                    angle_deg: 15.0,
                    feather: 0.4,
                },
                MaskShape::ColorRange {
                    op: MaskOp::Add,
                    range,
                    feather: 0.02,
                },
                MaskShape::Ai {
                    op: MaskOp::Add,
                    target: AiTarget::Sky,
                    feather: 0.25,
                    person: None,
                },
                MaskShape::Ai {
                    op: MaskOp::Subtract,
                    target: AiTarget::Person,
                    feather: 0.0,
                    person: None,
                },
                MaskShape::Ai {
                    op: MaskOp::Add,
                    target: AiTarget::Subject,
                    feather: 0.0,
                    person: None,
                },
                MaskShape::Ai {
                    op: MaskOp::Add,
                    target: AiTarget::Background,
                    feather: 0.0,
                    person: None,
                },
            ],
            invert: true,
            feather: 0.05,
            luma_range: Some(LumaRange {
                low: 0.2,
                high: 0.8,
                low_falloff: 0.05,
                high_falloff: 0.15,
            }),
            color_range: Some(range),
        };
        Adjustments {
            profile: Profile::Dcp {
                hash: "ab".repeat(32),
                name: "Camera Standard".into(),
            },
            white_balance: WhiteBalance::Custom {
                temp_k: 5200.0,
                tint: 7.0,
            },
            exposure: Exposure {
                exposure_ev: 0.7,
                contrast: 12.0,
                brightness: -3.0,
                saturation: 8.0,
            },
            presence: Presence {
                vibrance: 20.0,
                texture: 10.0,
                dehaze: -5.0,
            },
            hdr: Hdr {
                highlights: -40.0,
                shadows: 30.0,
                whites: 5.0,
                blacks: -6.0,
            },
            levels: Levels {
                in_black: 0.02,
                in_white: 0.97,
                gamma: 1.1,
                out_black: 0.01,
                out_white: 0.99,
            },
            curve: Curve {
                rgb: vec![[0.0, 0.0], [0.5, 0.55], [1.0, 1.0]],
                luma: vec![[0.25, 0.2]],
                red: vec![[0.5, 0.52]],
                green: vec![],
                blue: vec![[0.5, 0.48]],
            },
            color_balance: ColorBalance {
                master: Wheel {
                    hue: 30.0,
                    sat: 0.1,
                    lum: 0.0,
                },
                shadows: Wheel {
                    hue: 210.0,
                    sat: 0.2,
                    lum: -0.1,
                },
                midtones: Wheel::default(),
                highlights: Wheel {
                    hue: 45.0,
                    sat: 0.15,
                    lum: 0.05,
                },
            },
            color_editor,
            detail: Detail {
                clarity: 10.0,
                structure: 15.0,
                sharpen_amount: 200.0,
                sharpen_radius: 1.0,
                sharpen_threshold: 2.0,
            },
            noise: NoiseReduction {
                luminance: 40.0,
                color: 30.0,
                detail: 60.0,
                ai_denoise: true,
                ai_strength: 70.0,
            },
            vignette: Vignette {
                amount_ev: -0.5,
                midpoint: 0.4,
            },
            lens: LensCorrection {
                profile: Some("Sony FE 24-70mm F2.8 GM".into()),
                enabled: true,
                distortion: 0.8,
                chromatic_aberration: false,
                light_falloff: 0.5,
                keystone_vertical: 3.0,
                keystone_horizontal: -1.0,
            },
            crop: Crop {
                rect: NormRect {
                    x: 0.1,
                    y: 0.1,
                    w: 0.8,
                    h: 0.7,
                },
                angle_deg: 1.5,
                aspect: Some(Aspect { long: 3, short: 2 }),
                quarter_turns: 1,
                flip_h: true,
                flip_v: false,
            },
            layers: vec![Layer {
                id: 3,
                name: "Sky".into(),
                enabled: false,
                opacity: 0.6,
                adjustments: LayerAdjustments {
                    white_balance: Some(WhiteBalanceDelta {
                        temp_k: -300.0,
                        tint: 2.0,
                    }),
                    exposure: Exposure {
                        exposure_ev: -0.3,
                        ..Exposure::default()
                    },
                    hdr: Hdr {
                        highlights: -20.0,
                        ..Hdr::default()
                    },
                    curve: Curve {
                        luma: vec![[0.5, 0.45]],
                        ..Curve::default()
                    },
                    color_balance: ColorBalance::default(),
                    color_editor: vec![range],
                    clarity: 5.0,
                    structure: -5.0,
                    presence: Presence {
                        vibrance: 10.0,
                        texture: -20.0,
                        dehaze: 15.0,
                    },
                    sharpening: -40.0,
                },
                mask,
            }],
            spots: vec![Spot {
                kind: SpotKind::Clone,
                center: [0.3, 0.4],
                radius: 0.01,
                source: [0.35, 0.4],
                feather: 0.3,
                opacity: 0.9,
            }],
            removals: vec![Removal {
                strokes: vec![BrushStroke {
                    points: vec![[0.6, 0.6], [0.62, 0.65]],
                    size: 0.03,
                    hardness: 0.8,
                    ..BrushStroke::default()
                }],
                feather: 0.004,
            }],
            person_boxes: [(1, [0.2, 0.1, 0.6, 0.9])].into(),
            effects: crate::effects::Effects {
                lens_blur: crate::effects::LensBlur {
                    amount: 45.0,
                    mode: crate::effects::BlurMode::Plane,
                    focus: Some([0.4, 0.6]),
                    focal_range: 12.0,
                    highlight_boost: 70.0,
                    shape: crate::effects::BokehShape::Blades,
                    plane: crate::effects::FocusBand {
                        center: [0.5, 0.6],
                        angle_deg: 10.0,
                        width: 0.08,
                        feather: 0.2,
                    },
                },
                glow: crate::effects::Glow {
                    amount: 20.0,
                    size: 40.0,
                },
                grain: crate::effects::Grain {
                    amount: 15.0,
                    size: 30.0,
                    roughness: 60.0,
                },
                halation: crate::effects::Halation { amount: 10.0 },
            },
            look: Some(crate::effects::AppliedLook {
                id: "look.portrait".into(),
                intensity: 0.8,
                from: [("exposure.contrast".to_string(), 0.0)].into(),
                to: [("exposure.contrast".to_string(), 10.0)].into(),
            }),
        }
    }

    /// Recursively add an unknown key to every JSON object.
    fn add_unknown_everywhere(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for child in m.values_mut() {
                    add_unknown_everywhere(child);
                }
                m.insert("zz_future_field".into(), json!({"nested": [1, 2, 3]}));
            }
            Value::Array(a) => a.iter_mut().for_each(add_unknown_everywhere),
            _ => {}
        }
    }

    /// Every JSON object path (as keys/indices) in `v`.
    fn object_paths(v: &Value, path: &mut Vec<Value>, out: &mut Vec<Vec<Value>>) {
        match v {
            Value::Object(m) => {
                out.push(path.clone());
                for (k, child) in m {
                    path.push(Value::String(k.clone()));
                    object_paths(child, path, out);
                    path.pop();
                }
            }
            Value::Array(a) => {
                for (i, child) in a.iter().enumerate() {
                    path.push(json!(i));
                    object_paths(child, path, out);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    fn at_mut<'a>(v: &'a mut Value, path: &[Value]) -> &'a mut Value {
        path.iter().fold(v, |n, k| match k {
            Value::String(s) => &mut n[s.as_str()],
            Value::Number(i) => &mut n[i.as_u64().unwrap() as usize],
            _ => unreachable!(),
        })
    }

    #[test]
    fn default_is_identity() {
        assert!(Adjustments::default().is_identity());
        let mut a = Adjustments::default();
        a.noise.detail = 49.0;
        assert!(!a.is_identity());
        assert!(!everything().is_identity());
    }

    #[test]
    fn defaults_are_identity_values() {
        let d = Adjustments::default();
        assert_eq!(d.white_balance, WhiteBalance::AsShot);
        assert_eq!(d.levels.in_white, 1.0);
        assert_eq!(d.levels.gamma, 1.0);
        assert!(d.crop.rect.is_full());
        let hues: Vec<f32> = d.color_editor.basic.iter().map(|r| r.center_hue).collect();
        assert_eq!(hues, BASIC_BAND_HUES);
        assert!(
            d.color_editor
                .basic
                .iter()
                .all(|r| r.width == 45.0 && r.saturation == 0.0)
        );
        let l = Layer::default();
        assert!(l.enabled);
        assert_eq!(l.opacity, 1.0);
        assert_eq!(l.mask.shapes.len(), 0);
        assert_eq!(LumaRange::default().high, 1.0);
        assert_eq!(BrushStroke::default().flow, 1.0);
        assert_eq!(SkinTone::default().range, ColorRange::default());
    }

    #[test]
    fn round_trip_everything() {
        let a = everything();
        let json = serde_json::to_string_pretty(&a).unwrap();
        let back: Adjustments = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
        // Canonical: re-serializing gives identical bytes (cache keys hash this).
        assert_eq!(serde_json::to_string_pretty(&back).unwrap(), json);
    }

    #[test]
    fn profile_defaults_and_legacy_rule() {
        // New edits start on Standard and write it explicitly.
        let d = Adjustments::default();
        assert_eq!(d.profile, Profile::Standard);
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["profile"], json!({"kind": "standard"}));
        // No key = written before profiles = Neutral; an explicit key always wins.
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("profile");
        let a: Adjustments = serde_json::from_value(old).unwrap();
        assert_eq!(a.profile, Profile::Neutral);
        let a: Adjustments = serde_json::from_value(v).unwrap();
        assert_eq!(a.profile, Profile::Standard);
        let a: Adjustments = serde_json::from_value(json!({"profile": {"kind": "vivid"}})).unwrap();
        assert_eq!(a.profile, Profile::Vivid);
        assert!(!a.is_identity(), "a chosen look is an edit");
        let dcp = json!({"profile": {"kind": "dcp", "hash": "00", "name": "X"}});
        let a: Adjustments = serde_json::from_value(dcp).unwrap();
        assert_eq!(
            a.profile,
            Profile::Dcp {
                hash: "00".into(),
                name: "X".into()
            }
        );
        assert_eq!(Profile::legacy(), Profile::Neutral);
        // Untouched old and new edits are both unedited; an edit on either is not.
        assert!(d.is_identity());
        let mut e = d.clone();
        e.exposure.exposure_ev = 1.0;
        assert!(!e.is_identity());
        e.profile = Profile::Neutral;
        assert!(!e.is_identity());
    }

    #[test]
    fn unknown_profiles_round_trip_unchanged() {
        // A newer kind and a malformed import load (the rest of the edit with them) and
        // are written back verbatim.
        for p in [
            json!({"kind": "film", "stock": "Portra 400", "grain": 0.2}),
            json!({"kind": "dcp", "hash": 7}),
        ] {
            let a: Adjustments =
                serde_json::from_value(json!({"profile": p, "exposure": {"exposure_ev": 1.0}}))
                    .unwrap();
            assert_eq!(a.profile, Profile::Other(p.clone()), "{p}");
            assert_eq!(a.exposure.exposure_ev, 1.0);
            assert_eq!(serde_json::to_value(&a).unwrap()["profile"], p);
            assert!(!a.is_identity(), "an unknown look is not the default");
        }
        // Not an object: not a profile.
        assert!(serde_json::from_value::<Profile>(json!("cinematic")).is_err());
        assert!(serde_json::from_value::<Profile>(json!(null)).is_err());
        // Known kinds still read as themselves.
        for (k, p) in [
            ("standard", Profile::Standard),
            ("neutral", Profile::Neutral),
            ("vivid", Profile::Vivid),
        ] {
            let v = json!({ "kind": k });
            assert_eq!(serde_json::from_value::<Profile>(v.clone()).unwrap(), p);
            assert_eq!(serde_json::to_value(&p).unwrap(), v);
        }
    }

    #[test]
    fn round_trip_default() {
        let d = Adjustments::default();
        let back: Adjustments = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn tagged_enum_wire_format() {
        let v = serde_json::to_value(everything()).unwrap();
        assert_eq!(
            v["white_balance"],
            json!({"mode": "custom", "temp_k": 5200.0, "tint": 7.0})
        );
        assert_eq!(
            serde_json::to_value(WhiteBalance::AsShot).unwrap(),
            json!({"mode": "as_shot"})
        );
        let shapes = &v["layers"][0]["mask"]["shapes"];
        assert_eq!(shapes[0], json!({"kind": "fill", "op": "add"}));
        assert_eq!(shapes[4]["kind"], "color_range");
        assert_eq!(
            shapes[5],
            json!({"kind": "ai", "op": "add", "target": "sky", "feather": 0.25})
        );
        // Sidecars written before per-shape feathers still read (feather 0).
        let old: MaskShape =
            serde_json::from_value(json!({"kind": "ai", "op": "add", "target": "sky"})).unwrap();
        assert_eq!(
            old,
            MaskShape::Ai {
                op: MaskOp::Add,
                target: AiTarget::Sky,
                feather: 0.0,
                person: None,
            }
        );
        assert_eq!(v["spots"][0]["kind"], "clone");
        assert_eq!(v["removals"][0]["feather"], json!(0.004f32));
    }

    #[test]
    fn ai_targets_keep_their_wire_names_and_read_old_shapes() {
        // The four original targets keep their names, so sidecars written before the
        // people targets existed read unchanged; the new ones are additive.
        let names: Vec<Value> = AiTarget::ALL
            .iter()
            .map(|t| serde_json::to_value(t).unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "subject",
                "sky",
                "person",
                "background",
                "face",
                "skin",
                "eyes",
                "teeth",
                "hair",
                "clothes"
            ]
            .map(Value::from)
        );
        for t in AiTarget::ALL {
            let back: AiTarget = serde_json::from_value(serde_json::to_value(t).unwrap()).unwrap();
            assert_eq!(back, t);
        }
        let people: Vec<AiTarget> = AiTarget::ALL
            .into_iter()
            .filter(|t| t.is_people())
            .collect();
        assert_eq!(
            people,
            [
                AiTarget::Person,
                AiTarget::Face,
                AiTarget::Skin,
                AiTarget::Eyes,
                AiTarget::Teeth,
                AiTarget::Hair,
                AiTarget::Clothes
            ]
        );
        // `person` is written only when a single person is chosen, so an all-people
        // shape serializes exactly as before.
        let all = MaskShape::Ai {
            op: MaskOp::Add,
            target: AiTarget::Hair,
            feather: 0.0,
            person: None,
        };
        assert_eq!(
            serde_json::to_value(&all).unwrap(),
            json!({"kind": "ai", "op": "add", "target": "hair", "feather": 0.0})
        );
        let one = MaskShape::Ai {
            op: MaskOp::Add,
            target: AiTarget::Hair,
            feather: 0.0,
            person: Some(2),
        };
        let v = serde_json::to_value(&one).unwrap();
        assert_eq!(v["person"], 2);
        assert_eq!(serde_json::from_value::<MaskShape>(v).unwrap(), one);
        // An unknown target is an error for the shape (callers fall back per sidecar).
        assert!(
            serde_json::from_value::<MaskShape>(
                json!({"kind": "ai", "op": "add", "target": "elbows"})
            )
            .is_err()
        );
    }

    #[test]
    fn person_boxes_round_trip_and_are_sanitized() {
        // Sidecars from before load with none, and none are written.
        let a: Adjustments = serde_json::from_str(r#"{"spots": []}"#).unwrap();
        assert!(a.person_boxes.is_empty());
        assert!(!serde_json::to_string(&a).unwrap().contains("person_boxes"));
        let a: Adjustments = serde_json::from_str(
            r#"{"person_boxes": {"0": [0.1, 0.2, 0.5, 0.9], "1": [null, 0, 1, 1],
                "2": [0.6, 0.5, 0.4, 0.9], "3": [-0.5, 0.2, 1.5, 0.4], "x": [0, 0, 1, 1],
                "4": {"future": 1}}}"#,
        )
        .unwrap();
        assert_eq!(
            a.person_boxes.len(),
            2,
            "non-finite, empty and unknown entries dropped"
        );
        assert_eq!(a.person_boxes[&0], [0.1, 0.2, 0.5, 0.9]);
        assert_eq!(
            a.person_boxes[&3],
            [0.0, 0.2, 1.0, 0.4],
            "clamped to the frame"
        );
        assert!(!a.is_identity());
        let back: Adjustments = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(back, a);
        let mut b = a.clone();
        assert!(!b.sanitize(), "idempotent");
    }

    #[test]
    fn removals_are_additive_and_sanitized() {
        // Sidecars from before removals existed load with none.
        let a: Adjustments = serde_json::from_str(r#"{"spots": []}"#).unwrap();
        assert_eq!(a.removals.len(), 0);
        let r: Removal =
            serde_json::from_str(r#"{"strokes": [{"points": [[0.5, 0.5]]}]}"#).unwrap();
        assert_eq!(r.feather, Removal::default().feather);
        assert_eq!(r.strokes[0].size, BrushStroke::default().size);
        let mut a = Adjustments {
            removals: vec![Removal {
                strokes: vec![BrushStroke {
                    points: vec![[f32::NAN, 0.5], [0.4, 2.0]],
                    size: 50.0,
                    ..BrushStroke::default()
                }],
                feather: 1.0,
            }],
            ..Adjustments::default()
        };
        assert!(a.sanitize());
        let r = &a.removals[0];
        assert_eq!(r.feather, MAX_REMOVAL_FEATHER);
        assert_eq!(r.strokes[0].points, vec![[0.4, 1.0]]);
        assert_eq!(r.strokes[0].size, MAX_REMOVAL_SIZE);
        assert!(!a.sanitize(), "idempotent");
        assert!(!a.is_identity());
    }

    #[test]
    fn unknown_fields_ignored_at_every_level() {
        let a = everything();
        let mut v = serde_json::to_value(&a).unwrap();
        add_unknown_everywhere(&mut v);
        assert!(v["layers"][0]["mask"]["shapes"][1]["strokes"][0]["zz_future_field"].is_object());
        let back: Adjustments = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn missing_fields_default_at_top_level() {
        // A sidecar from before profiles: everything default except the profile, which
        // is the look it was edited under.
        let a: Adjustments = serde_json::from_str("{}").unwrap();
        assert_eq!(
            a,
            Adjustments {
                profile: Profile::Neutral,
                ..Adjustments::default()
            }
        );
        assert!(a.is_identity());
        let a: Adjustments = serde_json::from_str(r#"{"exposure": {"exposure_ev": 1.0}}"#).unwrap();
        assert_eq!(
            a.exposure,
            Exposure {
                exposure_ev: 1.0,
                ..Exposure::default()
            }
        );
        assert_eq!(a.detail, Detail::default());
        let a: Adjustments =
            serde_json::from_str(r#"{"levels": {"gamma": 2.0}, "detail": {}}"#).unwrap();
        assert_eq!(
            a.levels,
            Levels {
                gamma: 2.0,
                ..Levels::default()
            }
        );
        assert_eq!(a.detail.sharpen_amount, 140.0);
    }

    #[test]
    fn missing_fields_default_in_every_struct() {
        // Old sidecars written before a field existed must still load: drop each
        // field of each struct object, one at a time, and deserialize. Enum
        // variant payloads (tagged by `mode`/`kind`) are genuinely required and skipped.
        let root = serde_json::to_value(everything()).unwrap();
        let mut paths = Vec::new();
        object_paths(&root, &mut Vec::new(), &mut paths);
        let mut tried = 0;
        for p in paths {
            let mut probe = root.clone();
            let obj = at_mut(&mut probe, &p);
            // Atomic geometric values (`NormRect`, `Aspect`) are always written whole.
            let atomic =
                obj.get("long").is_some() || (obj.get("x").is_some() && obj.get("w").is_some());
            if obj.get("mode").is_some() || obj.get("kind").is_some() || atomic {
                continue;
            }
            let keys: Vec<String> = obj.as_object().unwrap().keys().cloned().collect();
            for k in keys {
                let mut dropped = root.clone();
                at_mut(&mut dropped, &p).as_object_mut().unwrap().remove(&k);
                tried += 1;
                if let Err(e) = serde_json::from_value::<Adjustments>(dropped) {
                    panic!("failed to load with {p:?}.{k} missing: {e}");
                }
            }
        }
        assert!(tried > 100, "{tried}");
    }

    #[test]
    fn missing_layer_fields_get_sane_defaults() {
        let a: Adjustments = serde_json::from_str(r#"{"layers": [{"id": 7}]}"#).unwrap();
        let l = &a.layers[0];
        assert_eq!(l.id, 7);
        assert!(
            l.enabled,
            "a layer from an old sidecar must not silently turn off"
        );
        assert_eq!(l.opacity, 1.0);
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(
            serde_json::from_str::<Adjustments>(r#"{"exposure": {"exposure_ev": "high"}}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<Adjustments>(r#"{"white_balance": {"mode": "tungsten"}}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<Adjustments>(r#"{"white_balance": {"mode": "custom"}}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<Adjustments>(r#"{"crop": {"quarter_turns": 300}}"#).is_err()
        );
    }

    #[test]
    fn ai_denoise_is_off_by_default_and_old_sidecars_load() {
        let n: NoiseReduction =
            serde_json::from_str(r#"{"luminance": 10, "color": 20, "detail": 30}"#).unwrap();
        assert!(!n.ai_denoise);
        assert_eq!(n.ai_strength, 50.0);
        assert_eq!(
            n.ai_fraction(),
            0.0,
            "off: no AI denoise whatever the strength"
        );
        let on = NoiseReduction {
            ai_denoise: true,
            ai_strength: 70.0,
            ..n
        };
        assert!((on.ai_fraction() - 0.7).abs() < 1e-6);
        let over = NoiseReduction {
            ai_strength: 250.0,
            ..on
        };
        assert_eq!(over.ai_fraction(), 1.0);
        let nan = NoiseReduction {
            ai_strength: f32::NAN,
            ..on
        };
        assert_eq!(nan.ai_fraction(), 0.0);
        let a: Adjustments =
            serde_json::from_str(r#"{"noise": {"ai_denoise": true, "ai_strength": 400}}"#).unwrap();
        assert!(a.noise.ai_denoise);
        assert_eq!(a.noise.ai_strength, 100.0, "sanitized into 0..=100");
    }

    proptest::proptest! {
        #[test]
        fn exposure_and_crop_round_trip(
            ev in -5.0f32..5.0,
            c in -100.0f32..100.0,
            x in 0.0f32..0.5,
            w in 0.1f32..0.5,
            turns in 0u8..4,
            flip in proptest::bool::ANY,
        ) {
            let mut a = Adjustments::default();
            a.exposure.exposure_ev = ev;
            a.exposure.contrast = c;
            a.crop = Crop {
                rect: NormRect { x, y: x, w, h: w },
                quarter_turns: turns,
                flip_h: flip,
                ..Crop::default()
            };
            let back: Adjustments = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
            proptest::prop_assert_eq!(back, a);
        }
    }
}
