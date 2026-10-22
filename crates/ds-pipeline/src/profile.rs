//! Camera profiles: the base look a develop starts from (`Adjustments::profile`).
//!
//! A profile is a tone curve (the base curve of the tone stage, see [`crate::lut`]) plus
//! optional hue/saturation/value maps applied to scene-linear colour just before tone,
//! the way a DNG camera profile's `HueSatMap` and `LookTable` are: in linear ROMM
//! (`ProPhoto`) RGB with a D50 white, as HSV, by (tri)linear lookup with hue wrapping. Darkslide's own profiles
//! are generated the same way from a small set of tuned OKLCh "twists" ([`Look`]), so
//! the CPU and GPU only know one map stage. An imported Adobe profile ([`Dcp`]) also
//! brings its own camera matrix (interpolated by the white balance's temperature between
//! its two calibrations), its tone curve and its baseline exposure offset.
//!
//! - **Neutral** is the original look (no map, the Apple-matched curve): sidecars from
//!   before profiles render exactly as before.
//! - **Standard** (the default): ~12 % more midtone contrast with a deeper toe and a
//!   filmic shoulder, skin pulled from yellow/green toward warm, greens a little warmer
//!   and calmer, deep blues kept from turning purple, skies a touch deeper.
//! - **Vivid**: Standard's twist with a steeper curve and +16 % chroma (skin +7 %).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use ds_core::RawImage;
use ds_core::adjust::Profile;

use crate::color::{
    self, D65_XYZ, Mat3, REC2020_TO_XYZ, diag, mat_inv, mat_mul, mat_vec, srgb_encode,
};
use crate::dcp::{Dcp, DcpError};
use crate::lut::{BaseCurve, BaseLuts, neutral_curve};
use crate::tone::smoothstep;

/// CIE XYZ of D50 (Y = 1), the DNG profile connection space white.
pub const D50_XYZ: [f32; 3] = [0.964_22, 1.0, 0.825_21];

/// Linear ROMM RGB (D50) → XYZ D50.
pub const PROPHOTO_TO_XYZ: Mat3 = [
    [0.797_674_9, 0.135_191_7, 0.031_353_4],
    [0.288_040_2, 0.711_874_1, 0.000_085_7],
    [0.0, 0.0, 0.825_210_0],
];

const BRADFORD: Mat3 = [
    [0.8951, 0.2664, -0.1614],
    [-0.7502, 1.7135, 0.0367],
    [0.0389, -0.0685, 1.0296],
];

/// Bradford chromatic adaptation from white `src` to white `dst` (XYZ).
pub fn bradford(src: [f32; 3], dst: [f32; 3]) -> Mat3 {
    let inv = mat_inv(&BRADFORD).expect("bradford invertible");
    let (s, d) = (mat_vec(&BRADFORD, src), mat_vec(&BRADFORD, dst));
    let k = [0, 1, 2].map(|i| if s[i].abs() > 1e-9 { d[i] / s[i] } else { 1.0 });
    mat_mul(&inv, &mat_mul(&diag(k), &BRADFORD))
}

struct Spaces {
    to_pp: Mat3,
    from_pp: Mat3,
    d50_to_working: Mat3,
}

fn spaces() -> &'static Spaces {
    static S: OnceLock<Spaces> = OnceLock::new();
    S.get_or_init(|| {
        let pp_from_xyz = mat_inv(&PROPHOTO_TO_XYZ).expect("prophoto invertible");
        let to_pp = mat_mul(
            &pp_from_xyz,
            &mat_mul(&bradford(D65_XYZ, D50_XYZ), &REC2020_TO_XYZ),
        );
        let from_pp = mat_inv(&to_pp).expect("prophoto invertible");
        let d50_to_working = mat_mul(&color::xyz_to_rec2020(), &bradford(D50_XYZ, D65_XYZ));
        Spaces {
            to_pp,
            from_pp,
            d50_to_working,
        }
    })
}

/// Linear Rec.2020 (working) → linear ROMM RGB, and back.
pub fn prophoto_matrices() -> (Mat3, Mat3) {
    (spaces().to_pp, spaces().from_pp)
}

// ------------------------------------------------------------------ HSV map ---

/// A DNG-style hue/saturation/value map: `dims` = (hue, saturation, value) divisions,
/// entries (hue shift in degrees, saturation scale, value scale) stored value-major,
/// then hue, then saturation. `srgb` looks the value index up on sRGB-encoded values.
/// Maps compare by a content hash taken once at construction (plans holding them are
/// compared per render; a table can have ~86 k entries).
#[derive(Debug, Clone)]
pub struct HsvMap {
    dims: [usize; 3],
    srgb: bool,
    data: Vec<[f32; 3]>,
    hash: u64,
}

impl PartialEq for HsvMap {
    fn eq(&self, o: &Self) -> bool {
        self.hash == o.hash && self.dims == o.dims && self.srgb == o.srgb
    }
}

fn content_hash(dims: [usize; 3], srgb: bool, data: &[[f32; 3]]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    dims.hash(&mut h);
    srgb.hash(&mut h);
    for e in data {
        e.map(f32::to_bits).hash(&mut h);
    }
    h.finish()
}

/// DNG HSV (hue in 0..6) of linear RGB.
#[inline]
pub fn rgb_to_hsv(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb;
    let v = r.max(g).max(b);
    let d = v - r.min(g).min(b);
    if v <= 0.0 || d <= 0.0 {
        return [0.0, 0.0, v];
    }
    let h = if r == v {
        (g - b) / d
    } else if g == v {
        2.0 + (b - r) / d
    } else {
        4.0 + (r - g) / d
    };
    [if h < 0.0 { h + 6.0 } else { h }, d / v, v]
}

/// Inverse of [`rgb_to_hsv`].
#[inline]
pub fn hsv_to_rgb(hsv: [f32; 3]) -> [f32; 3] {
    let [h, s, v] = hsv;
    if s <= 0.0 {
        return [v; 3];
    }
    let h = h.rem_euclid(6.0);
    let i = (h as usize).min(5);
    let f = h - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

impl HsvMap {
    /// `None` unless every division is ≥ 1, saturation has ≥ 2 and `data` has one entry
    /// per node. Non-finite entries read as identity.
    pub fn new(dims: [usize; 3], data: Vec<[f32; 3]>, srgb: bool) -> Option<Self> {
        if dims.contains(&0) || dims[1] < 2 || dims[0] * dims[1] * dims[2] != data.len() {
            return None;
        }
        let data = data
            .into_iter()
            .map(|e| {
                if e.iter().all(|v| v.is_finite()) {
                    e
                } else {
                    [0.0, 1.0, 1.0]
                }
            })
            .collect::<Vec<_>>();
        let hash = content_hash(dims, srgb, &data);
        Some(Self {
            dims,
            srgb,
            data,
            hash,
        })
    }

    pub fn dims(&self) -> [usize; 3] {
        self.dims
    }

    pub fn srgb_value(&self) -> bool {
        self.srgb
    }

    /// Entries flattened (hue shift, sat scale, value scale) in storage order.
    pub fn data(&self) -> &[[f32; 3]] {
        &self.data
    }

    /// `a·t + b·(1 − t)` entrywise (`b` ignored unless it has the same dimensions).
    pub fn blend(a: &Self, b: &Self, t: f32) -> Self {
        if a.dims != b.dims || t >= 1.0 {
            return a.clone();
        }
        let data = a
            .data
            .iter()
            .zip(&b.data)
            .map(|(x, y)| [0, 1, 2].map(|c| x[c] * t + y[c] * (1.0 - t)))
            .collect::<Vec<_>>();
        Self {
            dims: a.dims,
            srgb: a.srgb,
            hash: content_hash(a.dims, a.srgb, &data),
            data,
        }
    }

    /// Map linear ROMM `rgb`. `v_scale` scales the value used for the value index
    /// only (the hue/sat map is defined before exposure, applied after it here).
    #[inline]
    pub fn apply(&self, rgb: [f32; 3], v_scale: f32) -> [f32; 3] {
        let [h, s, v] = rgb_to_hsv(rgb);
        if v <= 0.0 {
            return rgb;
        }
        let [hd, sd, vd] = self.dims;
        let hs = h * (hd as f32 / 6.0);
        let mut h0 = hs as usize;
        let hf = hs - h0 as f32;
        let mut h1 = h0 + 1;
        if h0 >= hd - 1 {
            h0 = hd - 1;
            h1 = 0;
        }
        let ss = s.clamp(0.0, 1.0) * (sd - 1) as f32;
        let s0 = (ss as usize).min(sd - 2);
        let sf = ss - s0 as f32;
        let at = |vi: usize, hi: usize, si: usize| self.data[(vi * hd + hi) * sd + si];
        let plane = |vi: usize| {
            let lerp =
                |a: [f32; 3], b: [f32; 3], t: f32| [0, 1, 2].map(|c| a[c] + (b[c] - a[c]) * t);
            let lo = lerp(at(vi, h0, s0), at(vi, h0, s0 + 1), sf);
            let hi = lerp(at(vi, h1, s0), at(vi, h1, s0 + 1), sf);
            lerp(lo, hi, hf)
        };
        let e = if vd > 1 {
            let x = (v * v_scale).clamp(0.0, 1.0);
            let vs = if self.srgb { srgb_encode(x) } else { x } * (vd - 1) as f32;
            let v0 = (vs as usize).min(vd - 2);
            let vf = vs - v0 as f32;
            let (a, b) = (plane(v0), plane(v0 + 1));
            [0, 1, 2].map(|c| a[c] + (b[c] - a[c]) * vf)
        } else {
            plane(0)
        };
        let s2 = (s * e[1]).min(s.max(1.0));
        hsv_to_rgb([h + e[0] * (6.0 / 360.0), s2, v * e[2]])
    }
}

// ------------------------------------------------------------ built-in looks ---

/// A hue-local adjustment of a [`Look`] (OKLCh hue in degrees).
#[derive(Debug, Clone, Copy)]
struct Bump {
    hue: f32,
    /// Gaussian σ in degrees.
    width: f32,
    /// Hue rotation (degrees) at the centre.
    shift: f32,
    /// Fractional chroma change at the centre.
    chroma: f32,
    /// Fractional OKLab lightness change at the centre.
    light: f32,
}

/// A Darkslide profile: its tone curve and colour twist.
#[derive(Debug)]
struct Look {
    curve: &'static [[f32; 2]],
    /// Global chroma factor.
    chroma: f32,
    bumps: &'static [Bump],
}

const fn bump(hue: f32, width: f32, shift: f32, chroma: f32, light: f32) -> Bump {
    Bump {
        hue,
        width,
        shift,
        chroma,
        light,
    }
}

/// The colour character shared by Standard and Vivid (OKLCh hues: skin 45–60°, yellowish
/// skin 75–90°, yellow ~100°, foliage 125–150°, sky 250–258°, deep blue ~267°).
const STANDARD_BUMPS: [Bump; 5] = [
    // Reds a little richer.
    bump(28.0, 14.0, 0.0, 0.03, 0.0),
    // Skin: yellow and green-yellow casts rotate back toward warm, slightly calmer.
    bump(80.0, 12.0, -5.0, -0.03, 0.0),
    // Greens: warmer and less neon.
    bump(150.0, 18.0, -4.0, -0.06, 0.0),
    // Skies: a touch deeper and richer.
    bump(252.0, 16.0, 0.0, 0.05, -0.03),
    // Deep blues stay blue instead of drifting purple.
    bump(275.0, 12.0, -5.0, 0.0, 0.0),
];

const VIVID_BUMPS: [Bump; 6] = [
    STANDARD_BUMPS[0],
    STANDARD_BUMPS[1],
    STANDARD_BUMPS[2],
    STANDARD_BUMPS[3],
    STANDARD_BUMPS[4],
    // Skin gets only about half of the global boost.
    bump(52.0, 14.0, 0.0, -0.08, 0.0),
];

/// Standard: deeper toe, ~12 % steeper midtones, a long filmic shoulder (white at +6).
const STANDARD_CURVE: [[f32; 2]; 18] = [
    [-10.5, 0.0],
    [-9.0, 0.0008],
    [-8.0, 0.002],
    [-7.0, 0.005],
    [-6.0, 0.0095],
    [-5.0, 0.02],
    [-4.0, 0.042],
    [-3.0, 0.085],
    [-2.0, 0.16],
    [-1.0, 0.29],
    [0.0, 0.46],
    [1.0, 0.643],
    [2.0, 0.805],
    [3.0, 0.922],
    [3.5, 0.955],
    [4.0, 0.977],
    [5.0, 0.995],
    [6.0, 1.0],
];

const VIVID_CURVE: [[f32; 2]; 18] = [
    [-10.5, 0.0],
    [-9.0, 0.0006],
    [-8.0, 0.0015],
    [-7.0, 0.004],
    [-6.0, 0.008],
    [-5.0, 0.016],
    [-4.0, 0.035],
    [-3.0, 0.074],
    [-2.0, 0.148],
    [-1.0, 0.28],
    [0.0, 0.46],
    [1.0, 0.655],
    [2.0, 0.822],
    [3.0, 0.935],
    [3.5, 0.963],
    [4.0, 0.981],
    [5.0, 0.996],
    [6.0, 1.0],
];

const STANDARD: Look = Look {
    curve: &STANDARD_CURVE,
    chroma: 1.05,
    bumps: &STANDARD_BUMPS,
};

const VIVID: Look = Look {
    curve: &VIVID_CURVE,
    chroma: 1.16,
    bumps: &VIVID_BUMPS,
};

/// Hue (degrees) of an OKLab colour, and the signed shortest difference of two hues.
fn hue_diff(a: f32, b: f32) -> f32 {
    (a - b + 180.0).rem_euclid(360.0) - 180.0
}

/// The look's twist of one OKLab colour. Neutrals are untouched; chroma boosts fade for
/// colours that are already vivid (so they don't clip), cuts apply fully.
fn twist(lab: [f32; 3], look: &Look) -> [f32; 3] {
    let [l, a, b] = lab;
    let c = a.hypot(b);
    if l <= 1e-6 || c < 1e-6 {
        return lab;
    }
    let h = b.atan2(a).to_degrees();
    let q = c / l;
    let on = smoothstep(0.01, 0.08, q);
    let (mut shift, mut dc, mut dl) = (0.0, look.chroma - 1.0, 0.0);
    for k in look.bumps {
        let x = hue_diff(h, k.hue) / k.width;
        let w = (-0.5 * x * x).exp();
        shift += w * k.shift;
        dc += w * k.chroma;
        dl += w * k.light;
    }
    let dc = if dc > 0.0 {
        dc * (1.0 - smoothstep(0.25, 0.7, q))
    } else {
        dc
    };
    let h2 = (h + shift * on).to_radians();
    let c2 = c * (1.0 + dc * on);
    [l * (1.0 + dl * on), c2 * h2.cos(), c2 * h2.sin()]
}

/// Hue and saturation divisions of the generated maps (Adobe's are 90 × 30 too).
const LOOK_DIMS: [usize; 3] = [90, 30, 1];

/// The look's 2.5-D map: each node's ROMM colour twisted in OKLCh. The twist only
/// depends on hue and C/L (both invariant to scaling linear RGB), so a map without a
/// value axis reproduces it at every brightness.
fn look_map(look: &Look) -> HsvMap {
    let (to_pp, from_pp) = prophoto_matrices();
    let [hd, sd, _] = LOOK_DIMS;
    let mut data = Vec::with_capacity(hd * sd);
    for hi in 0..hd {
        for si in 0..sd {
            let h = hi as f32 * 6.0 / hd as f32;
            let s = si as f32 / (sd - 1) as f32;
            let v = 0.5;
            let w = mat_vec(&from_pp, hsv_to_rgb([h, s, v]));
            let t = color::oklab_to_rec2020(twist(color::rec2020_to_oklab(w), look));
            let [h2, s2, v2] = rgb_to_hsv(mat_vec(&to_pp, t));
            data.push(if s <= 0.0 || s2 <= 0.0 {
                [0.0, 1.0, v2 / v]
            } else {
                [hue_diff(h2 * 60.0, h * 60.0), s2 / s, v2 / v]
            });
        }
    }
    HsvMap::new(LOOK_DIMS, data, false).expect("look map dimensions")
}

/// A resolved curve with its layer tables.
#[derive(Debug)]
struct Tone {
    curve: Arc<BaseCurve>,
    luts: Arc<BaseLuts>,
}

impl Tone {
    fn new(curve: BaseCurve) -> Self {
        let luts = Arc::new(BaseLuts::new(&curve));
        Self {
            curve: Arc::new(curve),
            luts,
        }
    }
}

#[derive(Debug)]
struct BuiltIn {
    tone: Tone,
    map: Option<Arc<HsvMap>>,
}

fn builtin(look: Option<&'static Look>) -> BuiltIn {
    match look {
        None => BuiltIn {
            tone: Tone::new(neutral_curve().clone()),
            map: None,
        },
        Some(l) => BuiltIn {
            tone: Tone::new(BaseCurve::stops(l.curve).expect("look curve")),
            map: Some(Arc::new(look_map(l))),
        },
    }
}

fn neutral() -> &'static BuiltIn {
    static B: OnceLock<BuiltIn> = OnceLock::new();
    B.get_or_init(|| builtin(None))
}

fn standard() -> &'static BuiltIn {
    static B: OnceLock<BuiltIn> = OnceLock::new();
    B.get_or_init(|| builtin(Some(&STANDARD)))
}

fn vivid() -> &'static BuiltIn {
    static B: OnceLock<BuiltIn> = OnceLock::new();
    B.get_or_init(|| builtin(Some(&VIVID)))
}

/// Darkslide's own profiles, in picker order.
pub const BUILT_IN: [Profile; 3] = [Profile::Standard, Profile::Neutral, Profile::Vivid];

/// The name shown for a profile.
pub fn display_name(p: &Profile) -> String {
    match p {
        Profile::Standard => "Darkslide Standard".into(),
        Profile::Neutral => "Darkslide Neutral".into(),
        Profile::Vivid => "Darkslide Vivid".into(),
        Profile::Dcp { name, .. } => name.clone(),
        Profile::Other(_) => "Unknown Profile".into(),
    }
}

// ----------------------------------------------------------- DCP registry ---

/// An imported camera profile as the picker lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DcpInfo {
    /// BLAKE3 of the file, lowercase hex (what `Profile::Dcp` stores).
    pub hash: String,
    pub name: String,
    pub camera: Option<String>,
}

impl DcpInfo {
    pub fn profile(&self) -> Profile {
        Profile::Dcp {
            hash: self.hash.clone(),
            name: self.name.clone(),
        }
    }
}

#[derive(Debug)]
struct Loaded {
    info: DcpInfo,
    dcp: Dcp,
    tone: Tone,
    look: Option<Arc<HsvMap>>,
    /// Recently blended hue/sat maps by calibration weight (bits), newest last.
    blends: Mutex<Vec<(u32, Arc<HsvMap>)>>,
}

/// Blended hue/sat maps kept per imported profile (a viewer and an export or two).
const BLEND_CACHE: usize = 4;

impl Loaded {
    /// The hue/sat map at calibration weight `w`, blended once per weight (a single
    /// map is shared as is).
    fn hue_sat(&self, w: f32) -> Option<Arc<HsvMap>> {
        let (a, b) = match &self.dcp.hue_sat {
            [None, None] => return None,
            [Some(a), Some(b)] => (a, Some(b)),
            [Some(a), None] | [None, Some(a)] => (a, None),
        };
        let key = if b.is_some() { w.to_bits() } else { 0 };
        let mut c = crate::par::hold(&self.blends);
        if let Some(i) = c.iter().position(|(k, _)| *k == key) {
            let hit = c.remove(i);
            c.push(hit.clone());
            return Some(hit.1);
        }
        let m = Arc::new(b.map_or_else(|| a.clone(), |b| HsvMap::blend(a, b, w)));
        if c.len() >= BLEND_CACHE {
            c.remove(0);
        }
        c.push((key, m.clone()));
        Some(m)
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<Loaded>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<Loaded>>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

fn lookup(hash: &str) -> Option<Arc<Loaded>> {
    crate::par::hold(registry()).get(hash).cloned()
}

/// Parse a `.dcp` file's bytes and make it available to renders (idempotent).
pub fn register_dcp(bytes: &[u8]) -> Result<DcpInfo, DcpError> {
    let dcp = Dcp::parse(bytes)?;
    let info = DcpInfo {
        hash: blake3::hash(bytes).to_hex().to_string(),
        name: dcp.name.clone(),
        camera: dcp.camera.clone(),
    };
    let tone = match dcp.tone_curve.as_deref().and_then(BaseCurve::linear) {
        Some(c) => Tone::new(c),
        // No curve of its own: Standard's (Adobe would use its default curve).
        None => Tone {
            curve: standard().tone.curve.clone(),
            luts: standard().tone.luts.clone(),
        },
    };
    let look = dcp.look.clone().map(Arc::new);
    let loaded = Arc::new(Loaded {
        info: info.clone(),
        dcp,
        tone,
        look,
        blends: Mutex::default(),
    });
    crate::par::hold(registry()).insert(info.hash.clone(), loaded);
    Ok(info)
}

/// Forget an imported profile; true if it was registered.
pub fn unregister_dcp(hash: &str) -> bool {
    crate::par::hold(registry()).remove(hash).is_some()
}

/// Every registered camera profile, by name.
pub fn registered_dcps() -> Vec<DcpInfo> {
    let mut v: Vec<DcpInfo> = crate::par::hold(registry())
        .values()
        .map(|l| l.info.clone())
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name).then(a.hash.cmp(&b.hash)));
    v
}

/// Whether a render of `p` uses `p` itself (false: an imported profile that isn't
/// registered, or a profile kind this build doesn't know, rendered as Standard).
pub fn is_available(p: &Profile) -> bool {
    match p {
        Profile::Dcp { hash, .. } => lookup(hash).is_some(),
        Profile::Other(_) => false,
        Profile::Standard | Profile::Neutral | Profile::Vivid => true,
    }
}

/// Lowercase alphanumeric words of a camera name.
fn camera_words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Whether a profile made for `profile_camera` (its `UniqueCameraModel`) fits a photo
/// from `camera` (make and model): the same words ignoring case and punctuation, the
/// make optional on either side ("Sony ILCE-9" ≡ "SONY ILCE-9" ≡ "ILCE-9", never
/// "ILCE-9M2"). A profile without a camera, or an unknown camera, fits.
pub fn camera_matches(profile_camera: Option<&str>, camera: Option<&str>) -> bool {
    let (Some(p), Some(c)) = (profile_camera, camera) else {
        return true;
    };
    let (p, c) = (camera_words(p), camera_words(c));
    if p.is_empty() || c.is_empty() {
        return true;
    }
    let (short, long) = if p.len() <= c.len() { (p, c) } else { (c, p) };
    long.ends_with(&short)
}

/// Whether the imported profile `hash` is registered and made for `camera`.
pub fn fits_camera(hash: &str, camera: &str) -> bool {
    lookup(hash).is_some_and(|l| camera_matches(l.info.camera.as_deref(), Some(camera)))
}

// ------------------------------------------------------------------ the plan ---

/// What the develop needs of the profile for one render.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfilePlan {
    pub curve: Arc<BaseCurve>,
    /// The curve's forward/inverse tables (layer re-toning).
    pub luts: Arc<BaseLuts>,
    /// Display value of middle gray (the contrast pivot).
    pub pivot: f32,
    /// Hue/sat map (before exposure in DNG terms) and the look table.
    pub hue_sat: Option<Arc<HsvMap>>,
    pub look: Option<Arc<HsvMap>>,
    /// Scale of the value the hue/sat map indexes by: 1 / (exposure gain × baseline
    /// gain), so it sees the profile's pre-exposure values.
    pub hue_sat_v: f32,
    /// Working ↔ ROMM matrices the maps run in.
    pub to_pp: Mat3,
    pub from_pp: Mat3,
    /// White-balanced camera RGB → working, replacing the default matrix (imported
    /// profiles), without the baseline gain.
    pub cam: Option<Mat3>,
    /// Stops added to the baseline exposure.
    pub baseline_offset: f32,
    /// An imported profile that isn't registered or was made for another camera, or a
    /// profile kind this build doesn't know (rendered as Standard).
    pub missing: bool,
}

impl ProfilePlan {
    /// Whether any map applies.
    pub fn has_maps(&self) -> bool {
        self.hue_sat.is_some() || self.look.is_some()
    }

    /// The maps on one scene-linear working (Rec.2020) value.
    #[inline]
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        if !self.has_maps() {
            return rgb;
        }
        let mut pp = mat_vec(&self.to_pp, rgb);
        if let Some(m) = &self.hue_sat {
            pp = m.apply(pp, self.hue_sat_v);
        }
        if let Some(m) = &self.look {
            pp = m.apply(pp, 1.0);
        }
        mat_vec(&self.from_pp, pp)
    }

    fn of(b: &BuiltIn) -> Self {
        let (to_pp, from_pp) = prophoto_matrices();
        Self {
            curve: b.tone.curve.clone(),
            luts: b.tone.luts.clone(),
            pivot: b.tone.curve.pivot(),
            hue_sat: None,
            look: b.map.clone(),
            hue_sat_v: 1.0,
            to_pp,
            from_pp,
            cam: None,
            baseline_offset: 0.0,
            missing: false,
        }
    }
}

/// Resolve `profile` for a render: `camera` is the photo's make and model (an imported
/// profile for another camera falls back to Standard), `mults` the user's multipliers
/// (G = 1; they pick between an imported profile's calibrations) and `exposure_gain`
/// the linear exposure.
pub fn resolve(
    profile: &Profile,
    camera: Option<&str>,
    mults: [f32; 3],
    exposure_gain: f32,
) -> ProfilePlan {
    let missing = || ProfilePlan {
        missing: true,
        ..ProfilePlan::of(standard())
    };
    match profile {
        Profile::Neutral => ProfilePlan::of(neutral()),
        Profile::Standard => ProfilePlan::of(standard()),
        Profile::Vivid => ProfilePlan::of(vivid()),
        Profile::Other(_) => missing(),
        Profile::Dcp { hash, .. } => match lookup(hash) {
            Some(l) if camera_matches(l.info.camera.as_deref(), camera) => {
                let d = &l.dcp;
                let w = d.weight_for_neutral(mults);
                let (to_pp, from_pp) = prophoto_matrices();
                ProfilePlan {
                    curve: l.tone.curve.clone(),
                    luts: l.tone.luts.clone(),
                    pivot: l.tone.curve.pivot(),
                    hue_sat: l.hue_sat(w),
                    look: l.look.clone(),
                    hue_sat_v: 1.0 / (exposure_gain * baseline_gain(d.baseline_offset)).max(1e-6),
                    to_pp,
                    from_pp,
                    cam: dcp_camera_to_working(d, w, mults),
                    baseline_offset: d.baseline_offset,
                    missing: false,
                }
            }
            _ => missing(),
        },
    }
}

/// Linear gain of the baseline exposure with an imported profile's offset.
///
/// Adobe's rule is the raw's `BaselineExposure` + the profile's `BaselineExposureOffset`.
/// Sony ARWs carry no `BaselineExposure` (Adobe keeps a per-camera value of its own), so
/// Darkslide's calibrated [`crate::plan::BASELINE_EV`] stands in for it: the same role,
/// the value that lands a normal exposure on middle gray for this pipeline.
fn baseline_gain(offset: f32) -> f32 {
    (crate::plan::BASELINE_EV + offset).exp2()
}

/// `a·w + b·(1 − w)` (just `a` without `b`).
pub(crate) fn blend3(a: &Mat3, b: Option<&Mat3>, w: f32) -> Mat3 {
    match b {
        Some(b) => [0, 1, 2].map(|i| [0, 1, 2].map(|j| a[i][j] * w + b[i][j] * (1.0 - w))),
        None => *a,
    }
}

/// White-balanced camera RGB → working for an imported profile (DNG "mapping camera
/// color space to CIE XYZ"): the forward matrix when the profile has one for each
/// calibration (it maps white-balanced camera RGB to XYZ D50 directly), else the
/// inverse colour matrix on native camera RGB (`wb ⊘ mults`), adapted (Bradford) from
/// the white it gives to D50. Both map a white-balanced neutral to working white.
pub fn dcp_camera_to_working(d: &Dcp, w: f32, mults: [f32; 3]) -> Option<Mat3> {
    let s = spaces();
    let two = d.color_matrix[1].is_some();
    if let Some(f1) = &d.forward_matrix[0]
        && (!two || d.forward_matrix[1].is_some())
    {
        let fm = blend3(f1, d.forward_matrix[1].as_ref(), w);
        let y = fm[1][0] + fm[1][1] + fm[1][2];
        if !(y.is_finite() && y > 1e-6) {
            return None;
        }
        return Some(mat_mul(&s.d50_to_working, &fm.map(|r| r.map(|v| v / y))));
    }
    let cm = blend3(d.color_matrix[0].as_ref()?, d.color_matrix[1].as_ref(), w);
    let cam_to_xyz = mat_inv(&cm)?;
    let native = diag(mults.map(|m| if m > 0.0 { 1.0 / m } else { 1.0 }));
    let m = mat_mul(&cam_to_xyz, &native);
    let white = mat_vec(&m, [1.0; 3]);
    if white.iter().any(|v| !v.is_finite()) || white[1] <= 1e-9 {
        return None;
    }
    let adapt = bradford(white, D50_XYZ);
    Some(mat_mul(&s.d50_to_working, &mat_mul(&adapt, &m)))
}

/// The default camera matrix for `raw` with an imported profile's override applied.
pub(crate) fn camera_matrix(raw: &RawImage, p: &ProfilePlan) -> Mat3 {
    let gain = baseline_gain(p.baseline_offset);
    match &p.cam {
        Some(m) => m.map(|r| r.map(|v| v * gain)),
        None => crate::plan::cam_matrix(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::rec2020_to_oklab;
    use crate::dcp::synth::{Val, build};

    fn ok_lch(rgb: [f32; 3]) -> [f32; 3] {
        let l = rec2020_to_oklab(rgb);
        [
            l[0],
            l[1].hypot(l[2]),
            l[2].atan2(l[1]).to_degrees().rem_euclid(360.0),
        ]
    }

    #[test]
    fn hsv_round_trips() {
        for rgb in [
            [0.2, 0.5, 0.1],
            [0.9, 0.1, 0.3],
            [0.1, 0.2, 0.8],
            [0.5, 0.5, 0.2],
            [0.3, 0.3, 0.3],
            [0.0, 0.0, 0.0],
            [0.4, 0.1, 0.4],
            [0.1, 0.4, 0.4],
        ] {
            let back = hsv_to_rgb(rgb_to_hsv(rgb));
            for c in 0..3 {
                assert!((back[c] - rgb[c]).abs() < 1e-6, "{rgb:?} → {back:?}");
            }
        }
        assert_eq!(rgb_to_hsv([0.0, 0.0, 0.0]), [0.0, 0.0, 0.0]);
        // Hue sectors: red 0, green 2, blue 4.
        assert_eq!(rgb_to_hsv([1.0, 0.0, 0.0])[0], 0.0);
        assert_eq!(rgb_to_hsv([0.0, 1.0, 0.0])[0], 2.0);
        assert_eq!(rgb_to_hsv([0.0, 0.0, 1.0])[0], 4.0);
        assert_eq!(rgb_to_hsv([1.0, 0.0, 0.5])[0], 5.5);
    }

    fn identity(dims: [usize; 3]) -> HsvMap {
        HsvMap::new(
            dims,
            vec![[0.0, 1.0, 1.0]; dims[0] * dims[1] * dims[2]],
            false,
        )
        .unwrap()
    }

    #[test]
    fn identity_maps_change_nothing() {
        for dims in [[1, 2, 1], [6, 4, 1], [6, 4, 3]] {
            let m = identity(dims);
            for rgb in [
                [0.2f32, 0.5, 0.1],
                [0.9, 0.1, 0.3],
                [0.3, 0.3, 0.3],
                [2.0, 0.5, 0.1],
            ] {
                let o = m.apply(rgb, 1.0);
                for c in 0..3 {
                    assert!((o[c] - rgb[c]).abs() < 1e-5, "{dims:?} {rgb:?} → {o:?}");
                }
            }
        }
        assert_eq!(identity([4, 2, 1]).apply([0.0; 3], 1.0), [0.0; 3]);
        assert_eq!(
            identity([4, 2, 1]).apply([-1.0, -2.0, -3.0], 1.0),
            [-1.0, -2.0, -3.0]
        );
    }

    #[test]
    fn map_validation() {
        assert!(HsvMap::new([0, 2, 1], vec![], false).is_none());
        assert!(HsvMap::new([4, 1, 1], vec![[0.0, 1.0, 1.0]; 4], false).is_none());
        assert!(HsvMap::new([4, 2, 1], vec![[0.0, 1.0, 1.0]; 7], false).is_none());
        let m = HsvMap::new([1, 2, 1], vec![[f32::NAN, 1.0, 1.0], [0.0, 2.0, 1.0]], true).unwrap();
        assert_eq!(m.data()[0], [0.0, 1.0, 1.0]);
        assert!(m.srgb_value());
    }

    #[test]
    fn map_interpolates_hue_saturation_and_value() {
        // 2.5-D: +30° everywhere at full saturation, none at zero; value scale 0.5.
        let data = (0..6)
            .flat_map(|_| [[0.0, 1.0, 0.5], [30.0, 1.0, 0.5]])
            .collect();
        let m = HsvMap::new([6, 2, 1], data, false).unwrap();
        let [h, s, v] = rgb_to_hsv(m.apply([0.8, 0.2, 0.2], 1.0));
        // s = 0.75 → three quarters of the shift.
        assert!((h - 22.5 / 60.0).abs() < 1e-4, "{h}");
        assert!((s - 0.75).abs() < 1e-5 && (v - 0.4).abs() < 1e-6);
        // Hue wraps: just below 6 interpolates between the last node and node 0.
        let o = m.apply(hsv_to_rgb([5.9, 1.0, 0.5]), 1.0);
        assert!((rgb_to_hsv(o)[0] - (5.9 + 0.5f32).rem_euclid(6.0)).abs() < 1e-4);
        // Saturation is capped at 1 (a 2× scale on a saturated colour).
        let sat: Vec<[f32; 3]> = (0..4).flat_map(|_| [[0.0, 2.0, 1.0]; 2]).collect();
        let m = HsvMap::new([4, 2, 1], sat, false).unwrap();
        assert!((rgb_to_hsv(m.apply([0.5, 0.25, 0.25], 1.0))[1] - 1.0).abs() < 1e-6);
        // 3-D: value scale 1 at v = 0, 0.5 at v = 1; v_scale and sRGB indexing.
        let vol: Vec<[f32; 3]> = (0..2)
            .flat_map(|vi| vec![[0.0, 1.0, 1.0 - 0.5 * vi as f32]; 2 * 2])
            .collect();
        let lin = HsvMap::new([2, 2, 2], vol.clone(), false).unwrap();
        let v = |m: &HsvMap, x: f32, k: f32| m.apply([x, x * 0.5, x * 0.5], k)[0] / x;
        assert!((v(&lin, 0.5, 1.0) - 0.75).abs() < 1e-5);
        assert!(
            (v(&lin, 0.5, 2.0) - 0.5).abs() < 1e-5,
            "v_scale moves the index"
        );
        let enc = HsvMap::new([2, 2, 2], vol, true).unwrap();
        let e = srgb_encode(0.5);
        assert!((v(&enc, 0.5, 1.0) - (1.0 - 0.5 * e)).abs() < 1e-5);
        // Blending.
        let a = identity([2, 2, 1]);
        let b = HsvMap::new([2, 2, 1], vec![[10.0, 2.0, 0.5]; 4], false).unwrap();
        let mid = HsvMap::blend(&a, &b, 0.25);
        assert_eq!(mid.data()[0], [7.5, 1.75, 0.625]);
        assert_eq!(HsvMap::blend(&a, &b, 1.0), a);
        assert_eq!(HsvMap::blend(&a, &identity([3, 2, 1]), 0.5), a);
        assert_eq!(mid.dims(), [2, 2, 1]);
    }

    #[test]
    fn neutral_is_the_original_look() {
        let p = resolve(&Profile::Neutral, None, [2.0, 1.0, 1.5], 1.0);
        assert!(!p.has_maps() && !p.missing && p.cam.is_none());
        assert_eq!(*p.curve, *neutral_curve());
        assert_eq!(*p.luts, *crate::lut::base_luts());
        assert!((p.pivot - crate::lut::base_pivot()).abs() < 1e-7);
        assert_eq!(p.apply([0.3, 0.2, 0.1]), [0.3, 0.2, 0.1]);
    }

    /// Hue shift (degrees) and chroma ratio the Standard map gives a colour.
    fn effect(p: &ProfilePlan, rgb: [f32; 3]) -> (f32, f32, f32) {
        let (a, b) = (ok_lch(rgb), ok_lch(p.apply(rgb)));
        (hue_diff(b[2], a[2]), b[1] / a[1], b[0] / a[0])
    }

    fn srgb_lin(c: [u8; 3]) -> [f32; 3] {
        let to2020 = mat_mul(&color::xyz_to_rec2020(), &color::SRGB_TO_XYZ);
        mat_vec(&to2020, c.map(|v| color::srgb_decode(f32::from(v) / 255.0)))
    }

    #[test]
    fn standard_twist_does_what_it_claims() {
        let p = resolve(&Profile::Standard, None, [2.0, 1.0, 1.5], 1.0);
        assert!(p.look.is_some() && p.hue_sat.is_none());
        // Neutrals stay neutral (and keep their level) at any brightness.
        for g in [0.01f32, 0.18, 0.9, 4.0] {
            let o = p.apply([g; 3]);
            assert!(
                o.iter().all(|v| (v - g).abs() < 1e-4 * g.max(1.0)),
                "{g}: {o:?}"
            );
        }
        // Yellowish skin rotates toward warm (lower OK hue); normal skin barely moves.
        let (dh, _, _) = effect(&p, srgb_lin([214, 180, 130]));
        assert!((-6.0..-2.0).contains(&dh), "yellowish skin {dh}");
        let (dh, dc, _) = effect(&p, srgb_lin([224, 172, 140]));
        assert!(
            dh.abs() < 1.5 && (0.98..1.08).contains(&dc),
            "skin {dh} {dc}"
        );
        // Greens warmer and calmer; deep blue rotates away from purple; sky richer.
        let (dh, dc, _) = effect(&p, srgb_lin([40, 160, 60]));
        assert!(dh < -2.0 && dc < 1.0, "green {dh} {dc}");
        let (dh, _, _) = effect(&p, srgb_lin([30, 60, 200]));
        assert!(dh < -2.0, "deep blue {dh}");
        let (_, dc, dl) = effect(&p, srgb_lin([100, 150, 220]));
        assert!(dc > 1.04 && dl < 1.0, "sky {dc} {dl}");
        // Scale invariance: the same colour twice as bright gets the same twist.
        let c = srgb_lin([80, 140, 50]);
        let (a, b) = (effect(&p, c), effect(&p, c.map(|v| v * 4.0)));
        assert!(
            (a.0 - b.0).abs() < 0.2 && (a.1 - b.1).abs() < 0.01,
            "{a:?} {b:?}"
        );
        // Vivid is more saturated than Standard, with skin held back.
        let v = resolve(&Profile::Vivid, None, [2.0, 1.0, 1.5], 1.0);
        let grass = srgb_lin([80, 140, 50]);
        assert!(effect(&v, grass).1 > effect(&p, grass).1 + 0.05);
        let skin = srgb_lin([198, 134, 103]);
        assert!(effect(&v, skin).1 < effect(&v, grass).1);
    }

    #[test]
    fn curves_order_by_contrast() {
        let slope = |p: &Profile| {
            let c = resolve(p, None, [1.0; 3], 1.0).curve;
            c.eval(1.0) - c.eval(-1.0)
        };
        let (n, s, v) = (
            slope(&Profile::Neutral),
            slope(&Profile::Standard),
            slope(&Profile::Vivid),
        );
        assert!(n < s && s < v, "{n} {s} {v}");
        for p in BUILT_IN {
            let r = resolve(&p, None, [1.0; 3], 1.0);
            assert!((r.pivot - 0.46).abs() < 0.01, "{p:?} pivot {}", r.pivot);
            // Monotone, black to white.
            let v: Vec<f32> = (-110..=70).map(|i| r.curve.eval(i as f32 / 10.0)).collect();
            assert!(v.windows(2).all(|w| w[1] >= w[0]));
            assert_eq!(v[0], 0.0);
            assert_eq!(*v.last().unwrap(), 1.0);
        }
        assert_eq!(display_name(&Profile::Standard), "Darkslide Standard");
        assert_eq!(display_name(&Profile::Neutral), "Darkslide Neutral");
        assert_eq!(display_name(&Profile::Vivid), "Darkslide Vivid");
        let d = Profile::Dcp {
            hash: "h".into(),
            name: "Adobe Standard".into(),
        };
        assert_eq!(display_name(&d), "Adobe Standard");
    }

    const CM_A: [f32; 9] = [0.7, -0.15, -0.05, -0.45, 1.25, 0.2, -0.1, 0.2, 0.75];
    const CM_D65: [f32; 9] = [0.75, -0.2, -0.05, -0.4, 1.2, 0.2, -0.05, 0.15, 0.6];

    pub(crate) fn test_dcp(name: &str, forward: bool, tone: bool) -> Vec<u8> {
        let hs: Vec<f32> = (0..6 * 2)
            .flat_map(|i| {
                if i % 2 == 1 {
                    [12.0, 1.2, 1.0]
                } else {
                    [0.0, 1.0, 1.0]
                }
            })
            .collect();
        let mut tags = vec![
            (50936, Val::Ascii(name.into())),
            (50708, Val::Ascii("Sony ILCE-9".into())),
            (50778, Val::Short(vec![17])),
            (50779, Val::Short(vec![21])),
            (50721, Val::SRational(CM_A.to_vec())),
            (50722, Val::SRational(CM_D65.to_vec())),
            (50937, Val::Long(vec![6, 2, 1])),
            (50938, Val::Float(hs.clone())),
            (50939, Val::Float(hs)),
        ];
        if forward {
            let fm = vec![0.6, 0.3, 0.0642, 0.25, 0.8, -0.05, 0.0, 0.05, 0.7752];
            tags.push((50964, Val::SRational(fm.clone())));
            tags.push((50965, Val::SRational(fm)));
        }
        if tone {
            tags.push((50940, Val::Float(vec![0.0, 0.0, 0.18, 0.25, 1.0, 1.0])));
        }
        build(&tags, false)
    }

    #[test]
    fn registry_resolves_imported_profiles() {
        let bytes = test_dcp("Synthetic Standard", true, true);
        let info = register_dcp(&bytes).unwrap();
        assert_eq!(info.hash.len(), 64);
        assert_eq!(info.name, "Synthetic Standard");
        assert_eq!(info.camera.as_deref(), Some("Sony ILCE-9"));
        assert_eq!(register_dcp(&bytes).unwrap(), info, "idempotent");
        assert!(registered_dcps().contains(&info));
        let prof = info.profile();
        assert!(is_available(&prof) && is_available(&Profile::Vivid));
        let p = resolve(&prof, Some("SONY ILCE-9"), [2.0, 1.0, 1.6], 2.0);
        assert!(!p.missing && p.hue_sat.is_some() && p.look.is_none());
        // The hue/sat map indexes pre-exposure values: exposure and baseline removed.
        let gain = 2.0 * crate::plan::BASELINE_EV.exp2();
        assert!((p.hue_sat_v * gain - 1.0).abs() < 1e-6, "{}", p.hue_sat_v);
        // Resolving again shares the blended map (cached per weight).
        let again = resolve(&prof, None, [2.0, 1.0, 1.6], 1.0);
        assert!(Arc::ptr_eq(
            p.hue_sat.as_ref().unwrap(),
            again.hue_sat.as_ref().unwrap()
        ));
        // Made for another camera: Standard, flagged.
        let other = resolve(&prof, Some("SONY ILCE-9M2"), [2.0, 1.0, 1.6], 1.0);
        assert!(other.missing && other.cam.is_none());
        assert_eq!(
            other,
            resolve(
                &Profile::Other(serde_json::json!({"kind": "x"})),
                None,
                [1.0; 3],
                1.0
            )
        );
        assert!(fits_camera(&info.hash, "Sony ILCE-9") && !fits_camera(&info.hash, "Nikon D850"));
        // The profile's own curve: 0.18 → 0.25 linear.
        assert!((p.pivot - srgb_encode(0.25)).abs() < 1e-3, "{}", p.pivot);
        // Forward matrix path: a white-balanced neutral maps to working white.
        let w = mat_vec(p.cam.as_ref().unwrap(), [1.0; 3]);
        assert!(w.iter().all(|v| (v - 1.0).abs() < 2e-3), "{w:?}");
        // The map rotates saturated colours by up to 12°.
        let o = p.apply([0.4, 0.1, 0.1]);
        assert!(
            (rgb_to_hsv(mat_vec(&p.to_pp, o))[0]
                - rgb_to_hsv(mat_vec(&p.to_pp, [0.4, 0.1, 0.1]))[0])
                .abs()
                > 0.05
        );
        assert!(unregister_dcp(&info.hash));
        assert!(!unregister_dcp(&info.hash));
        assert!(!is_available(&prof));
        assert!(!fits_camera(&info.hash, "Sony ILCE-9"), "unregistered");
        let gone = resolve(&prof, None, [2.0, 1.0, 1.6], 1.0);
        assert!(gone.missing);
        assert_eq!(
            gone.curve,
            resolve(&Profile::Standard, None, [1.0; 3], 1.0).curve
        );
        assert!(register_dcp(b"nope").is_err());
    }

    #[test]
    fn colour_matrix_path_and_defaults() {
        // No forward matrix, no tone curve: the colour matrix, Standard's curve.
        let info = register_dcp(&test_dcp("CM only", false, false)).unwrap();
        let mults = [2.1, 1.0, 1.4];
        let p = resolve(&info.profile(), None, mults, 1.0);
        assert_eq!(
            p.curve,
            resolve(&Profile::Standard, None, [1.0; 3], 1.0).curve
        );
        let m = p.cam.unwrap();
        let w = mat_vec(&m, [1.0; 3]);
        assert!(w.iter().all(|v| (v - 1.0).abs() < 1e-3), "{w:?}");
        // A non-neutral stays non-neutral.
        let c = mat_vec(&m, [1.5, 1.0, 0.6]);
        assert!(c[0] > c[2]);
        // Calibration weight (from the white balance) moves the matrix.
        let q = resolve(&info.profile(), None, [1.6, 1.0, 2.2], 1.0);
        assert_ne!(q.cam, p.cam);
        unregister_dcp(&info.hash);
    }

    #[test]
    fn camera_names_match_tolerantly() {
        let m = |p, c| camera_matches(Some(p), Some(c));
        assert!(m("Sony ILCE-9", "Sony ILCE-9"));
        assert!(m("Sony ILCE-9", "SONY ILCE-9"));
        assert!(m("Sony ILCE-9", "ILCE-9"));
        assert!(m("ILCE-9", "Sony ILCE-9"));
        assert!(m("Nikon D850", "Nikon Corporation NIKON D850"));
        assert!(m("Canon EOS 5D Mark IV", "canon eos-5d mark iv"));
        assert!(!m("Sony ILCE-9", "Sony ILCE-9M2"));
        assert!(!m("Sony ILCE-7RM3", "Sony ILCE-7RM3A"));
        assert!(!m("Sony ILCE-9", "Nikon D850"));
        // Unknown on either side fits.
        assert!(camera_matches(None, Some("Sony ILCE-9")));
        assert!(camera_matches(Some("Sony ILCE-9"), None));
        assert!(m("--", "Sony ILCE-9") && m("Sony ILCE-9", ""));
    }

    #[test]
    fn unknown_profiles_render_as_standard_flagged() {
        let p = Profile::Other(serde_json::json!({"kind": "film"}));
        assert!(!is_available(&p));
        assert_eq!(display_name(&p), "Unknown Profile");
        let r = resolve(&p, Some("Sony ILCE-9"), [2.0, 1.0, 1.5], 1.0);
        assert!(r.missing);
        let s = resolve(&Profile::Standard, None, [2.0, 1.0, 1.5], 1.0);
        assert_eq!(r.curve, s.curve);
        assert_eq!(r.look, s.look);
    }

    #[test]
    fn maps_compare_by_content() {
        let a = identity([4, 2, 1]);
        let b = HsvMap::new([4, 2, 1], vec![[0.0, 1.0, 1.0]; 8], false).unwrap();
        assert_eq!(a, b);
        let mut d = vec![[0.0, 1.0, 1.0]; 8];
        d[5][1] = 1.01;
        assert_ne!(a, HsvMap::new([4, 2, 1], d, false).unwrap());
        assert_ne!(
            a,
            HsvMap::new([4, 2, 1], vec![[0.0, 1.0, 1.0]; 8], true).unwrap()
        );
        assert_ne!(a, identity([2, 4, 1]));
    }

    #[test]
    fn single_map_and_blend_cache_bounds() {
        // One calibration's map is shared, not copied, across renders.
        let bytes = crate::dcp::synth::build(
            &[
                (50721, Val::SRational(CM_A.to_vec())),
                (50937, Val::Long(vec![2, 2, 1])),
                (50938, Val::Float([1.0, 1.0, 1.0].repeat(4))),
            ],
            false,
        );
        let info = register_dcp(&bytes).unwrap();
        let (a, b) = (
            resolve(&info.profile(), None, [2.0, 1.0, 1.5], 1.0),
            resolve(&info.profile(), None, [1.5, 1.0, 2.0], 1.0),
        );
        assert!(Arc::ptr_eq(
            a.hue_sat.as_ref().unwrap(),
            b.hue_sat.as_ref().unwrap()
        ));
        unregister_dcp(&info.hash);
        // Two maps: one blend per weight, at most BLEND_CACHE kept.
        let info = register_dcp(&test_dcp("Cache", false, false)).unwrap();
        let l = lookup(&info.hash).unwrap();
        let first = l.hue_sat(0.1).unwrap();
        for i in 0..BLEND_CACHE {
            l.hue_sat(0.2 + i as f32 * 0.1);
        }
        assert_eq!(crate::par::hold(&l.blends).len(), BLEND_CACHE);
        assert!(!Arc::ptr_eq(&first, &l.hue_sat(0.1).unwrap()), "evicted");
        let last = l.hue_sat(0.1).unwrap();
        assert!(Arc::ptr_eq(&last, &l.hue_sat(0.1).unwrap()));
        unregister_dcp(&info.hash);
    }

    #[test]
    fn camera_to_working_edge_cases() {
        let mut d = Dcp::parse(&test_dcp("x", true, false)).unwrap();
        // A forward matrix only for the first calibration of two: colour matrices.
        d.forward_matrix[1] = None;
        let cm = dcp_camera_to_working(&d, 0.5, [2.0, 1.0, 1.5]).unwrap();
        d.forward_matrix = [None, None];
        assert_eq!(dcp_camera_to_working(&d, 0.5, [2.0, 1.0, 1.5]), Some(cm));
        // Degenerate matrices.
        d.color_matrix = [Some([[0.0; 3]; 3]), None];
        assert_eq!(dcp_camera_to_working(&d, 1.0, [1.0; 3]), None);
        d.forward_matrix = [Some([[0.0; 3]; 3]), None];
        assert_eq!(dcp_camera_to_working(&d, 1.0, [1.0; 3]), None);
        // A white with no luminance.
        d.forward_matrix = [None, None];
        d.color_matrix = [
            Some([[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]]),
            None,
        ];
        assert!(dcp_camera_to_working(&d, 1.0, [0.0, 1.0, 1.0]).is_some());
        d.color_matrix = [
            Some([[1.0, 0.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0]]),
            None,
        ];
        assert_eq!(dcp_camera_to_working(&d, 1.0, [1.0; 3]), None);
    }

    #[test]
    fn bradford_maps_white_to_white() {
        let m = bradford(D65_XYZ, D50_XYZ);
        let w = mat_vec(&m, D65_XYZ);
        for c in 0..3 {
            assert!((w[c] - D50_XYZ[c]).abs() < 1e-4);
        }
        // Working white ↔ ProPhoto white.
        let (to, from) = prophoto_matrices();
        let pp = mat_vec(&to, [1.0; 3]);
        assert!(pp.iter().all(|v| (v - 1.0).abs() < 1e-3), "{pp:?}");
        let back = mat_vec(&from, pp);
        assert!(back.iter().all(|v| (v - 1.0).abs() < 1e-5));
        let d50w = mat_vec(&spaces().d50_to_working, D50_XYZ);
        assert!(d50w.iter().all(|v| (v - 1.0).abs() < 1e-3), "{d50w:?}");
    }
}
