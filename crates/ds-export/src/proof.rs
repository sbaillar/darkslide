//! Soft proofing and output ICC profiles, through Little CMS (`lcms2`).
//!
//! **Viewer.** [`proof_lut`] bakes the whole proof into a small 3D table over the
//! develop pipeline's working gamut (Rec.2020, sRGB-encoded, 0…1), so a colour outside
//! Display P3 is proofed (and gamut-checked) as itself, not as its P3 clip. Each texel
//! holds the colour the proof shows (Rec.2020 → proof device with the chosen intent
//! and black point compensation, quantized to 16-bit device values as a file would be,
//! → back relative colorimetric, *linear* Rec.2020) and a signed gamut distance. The
//! app samples it in the viewer's fragment shader (one hardware-filtered 3D texture
//! read per pixel), so proofing costs nothing on the render path.
//!
//! **Gamut.** A colour is out of gamut when its relative-colorimetric round trip
//! through the proof device moves it by more than the profile's threshold (ΔE76,
//! [`IccProfile::gamut_threshold`]): 1 for a matrix/TRC profile, which round-trips
//! exactly, and 5 (Little CMS's own gamut-check threshold) for a profile with LUTs,
//! whose round trip is noisy; a profile with both is evaluated through its LUTs, so it
//! counts as a LUT profile. Alpha is the signed distance to the gamut surface in grid
//! cells (positive outside), found by bisecting every grid edge that crosses it, so
//! the interpolated zero crossing sits on the surface instead of bulging inward.
//!
//! **Export.** [`OutputProfile`] converts rendered pixels (rendered in Rec.2020, see
//! [`crate::OutputSpace::Rec2020`]) to an RGB profile with the same intent and BPC,
//! so the file matches the proof.
//!
//! Little CMS is used rather than `ColorSync` because it is deterministic and testable
//! in `cargo test`, handles every profile kind (matrix/TRC, LUT, CMYK, gray), has
//! black point compensation for every intent, and the export path is in Rust.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use lcms2::{
    CIExyY, ColorSpaceSignature, Flags, GlobalContext, InfoType, Intent, Locale, PixelFormat,
    Profile, Tag, TagSignature, Transform,
};
use rayon::prelude::*;

use crate::ExportError;
use crate::icc;
use crate::process;
use crate::recipe::OutputSpace;

/// Grid points per axis of the viewer's table (33³ × RGBA ≈ 0.6 MB as f32).
pub const LUT_SIZE: usize = 33;

/// Out-of-gamut threshold (ΔE76) for matrix/TRC profiles, which round-trip exactly.
pub const GAMUT_DELTA_E_MATRIX: f32 = 1.0;
/// Out-of-gamut threshold (ΔE76) for profiles with LUTs (Little CMS's own gamut check).
pub const GAMUT_DELTA_E_LUT: f32 = 5.0;

/// Largest profile accepted (printer profiles are a few MB at most).
pub const MAX_PROFILE_BYTES: usize = 32 << 20;

/// Bisection steps per grid edge crossing the gamut surface (1/4096 of a cell).
const SURFACE_STEPS: usize = 12;

/// The table's input space: the working gamut, sRGB-encoded.
const WIDE: OutputSpace = OutputSpace::Rec2020;

/// How colours outside the proof profile's gamut are brought in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ProofIntent {
    /// Compress the whole gamut (the profile's perceptual table).
    #[default]
    Perceptual,
    /// Keep in-gamut colours exact and clip the rest.
    RelativeColorimetric,
}

impl ProofIntent {
    fn lcms(self) -> Intent {
        match self {
            Self::Perceptual => Intent::Perceptual,
            Self::RelativeColorimetric => Intent::RelativeColorimetric,
        }
    }
}

/// Rendering intent and black point compensation of a proof or an output conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ProofSettings {
    pub intent: ProofIntent,
    pub black_point_compensation: bool,
}

impl ProofSettings {
    fn flags(self) -> Flags<lcms2::DisallowCache> {
        let f = Flags::NO_CACHE;
        if self.black_point_compensation {
            f | Flags::BLACKPOINT_COMPENSATION
        } else {
            f
        }
    }
}

const RELATIVE: ProofSettings = ProofSettings {
    intent: ProofIntent::RelativeColorimetric,
    black_point_compensation: false,
};

/// A profile's device colour model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileSpace {
    Rgb,
    Cmyk,
    Gray,
}

impl ProfileSpace {
    /// 16-bit device format for this space.
    fn format16(self) -> PixelFormat {
        match self {
            Self::Rgb => PixelFormat::RGB_16,
            Self::Cmyk => PixelFormat::CMYK_16,
            Self::Gray => PixelFormat::GRAY_16,
        }
    }
}

fn profile_error(m: impl std::fmt::Display) -> ExportError {
    ExportError::Profile(m.to_string())
}

/// Tags that make Little CMS evaluate a profile through LUTs instead of its matrix.
const LUT_TAGS: [TagSignature; 8] = [
    TagSignature::AToB0Tag,
    TagSignature::AToB1Tag,
    TagSignature::AToB2Tag,
    TagSignature::BToA0Tag,
    TagSignature::BToA1Tag,
    TagSignature::BToA2Tag,
    TagSignature::DToB0Tag,
    TagSignature::BToD0Tag,
];

/// An ICC profile the viewer can proof against (and, if RGB, a recipe can export to).
#[derive(Debug, Clone, PartialEq)]
pub struct IccProfile {
    bytes: Arc<[u8]>,
    space: ProfileSpace,
    name: String,
    matrix_only: bool,
    linear: bool,
    /// Linear Rec.2020 → the profile's linear RGB (matrix/TRC RGB profiles only).
    from_wide: Option<[[f32; 3]; 3]>,
}

impl IccProfile {
    /// Parse and check a profile: RGB, CMYK or gray, usable as a transform's output.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ExportError> {
        if bytes.len() > MAX_PROFILE_BYTES {
            return Err(profile_error(format!(
                "profile is {} bytes (limit {MAX_PROFILE_BYTES})",
                bytes.len()
            )));
        }
        let p = Profile::new_icc(bytes).map_err(|_| profile_error("not a readable ICC profile"))?;
        let space = match p.color_space() {
            ColorSpaceSignature::RgbData => ProfileSpace::Rgb,
            ColorSpaceSignature::CmykData => ProfileSpace::Cmyk,
            ColorSpaceSignature::GrayData => ProfileSpace::Gray,
            other => return Err(profile_error(format!("unsupported colour space {other:?}"))),
        };
        let name = p
            .info(InfoType::Description, Locale::none())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Unnamed profile".to_owned());
        let mut out = Self {
            bytes: bytes.into(),
            space,
            name,
            matrix_only: p.is_matrix_shaper() && !LUT_TAGS.iter().any(|t| p.has_tag(*t)),
            linear: false,
            from_wide: None,
        };
        if out.matrix_only && space == ProfileSpace::Rgb {
            out.from_wide = wide_to_colorants(&p);
        }
        // A profile that can't be rendered into (an input-only profile) fails here.
        out.wide_to_device(ProofSettings::default())?;
        out.linear = out.dark_step_lightness()? > LINEAR_DARK_STEP_L;
        Ok(out)
    }

    /// Read a profile file (`.icc` / `.icm`).
    pub fn read(path: &Path) -> Result<Self, ExportError> {
        let len = std::fs::metadata(path)
            .map_err(|source| ExportError::Io {
                path: path.into(),
                source,
            })?
            .len();
        if len > MAX_PROFILE_BYTES as u64 {
            return Err(profile_error(format!(
                "profile is {len} bytes (limit {MAX_PROFILE_BYTES})"
            )));
        }
        // The file may grow (or be a device) after the length check: read one byte
        // past the limit so an oversize file is refused, never fully loaded.
        let io = |source| ExportError::Io {
            path: path.into(),
            source,
        };
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(io)?
            .take(MAX_PROFILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        Self::from_bytes(&bytes)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn space(&self) -> ProfileSpace {
        self.space
    }

    /// The profile's description (`desc`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A matrix/TRC profile without LUTs: every rendering intent maps colours the same
    /// way (Little CMS falls back to colorimetric), so the intent choice does nothing.
    pub fn matrix_only(&self) -> bool {
        self.matrix_only
    }

    /// Linear-light encoding (e.g. ACEScg): 8 bits per channel band visibly in the
    /// shadows, so such a profile needs a 16-bit file.
    pub fn is_linear(&self) -> bool {
        self.linear
    }

    /// Out-of-gamut threshold, ΔE76 (see the module docs).
    pub fn gamut_threshold(&self) -> f32 {
        if self.matrix_only {
            GAMUT_DELTA_E_MATRIX
        } else {
            GAMUT_DELTA_E_LUT
        }
    }

    /// Soft-compress encoded Rec.2020 colours into this profile's gamut, as the develop
    /// pipeline does into sRGB or P3 (`ds_core::gamut`), so Little CMS doesn't clip
    /// them channel by channel (hue shifts). Matrix/TRC RGB profiles only; LUT
    /// profiles map out-of-gamut colours with their own perceptual tables.
    fn compress_wide(&self, px: &mut [[u16; 3]]) {
        let Some(m) = self.from_wide else { return };
        let back = inverse(&m);
        let decode = srgb_decode_table();
        px.par_chunks_mut(4096).for_each(|chunk| {
            for p in chunk {
                let lin = p.map(|v| decode[usize::from(v)]);
                let t = mul3(&m, lin);
                let c = ds_core::gamut::compress(t);
                if c.iter().zip(t).any(|(a, b)| (a - b).abs() > 1e-7) {
                    *p = mul3(&back, c)
                        .map(|v| (linear_to_srgb(v.clamp(0.0, 1.0)) * 65_535.0).round() as u16);
                }
            }
        });
    }

    fn lcms(&self) -> Result<Profile, ExportError> {
        Profile::new_icc(&self.bytes).map_err(|_| profile_error("not a readable ICC profile"))
    }

    /// How much lighter (ΔL*) the first 8-bit step above black is than black: ≈ 0.3
    /// for gamma-encoded profiles, ≈ 3.5 for linear ones. 0 for CMYK (never exported).
    fn dark_step_lightness(&self) -> Result<f64, ExportError> {
        if self.space == ProfileSpace::Cmyk {
            return Ok(0.0);
        }
        let lab = lab_profile()?;
        let dev = self.space.format16();
        let px = |v: u16| -> Vec<u8> {
            std::iter::repeat_n(v, dev.channels())
                .flat_map(u16::to_ne_bytes)
                .collect()
        };
        let t: Transform<u8, [f64; 3]> = Transform::new_flags(
            &self.lcms()?,
            dev,
            &lab,
            PixelFormat::Lab_DBL,
            Intent::RelativeColorimetric,
            Flags::NO_OPTIMIZE,
        )
        .map_err(profile_error)?;
        let mut out = [[0.0; 3]; 2];
        t.transform_pixels(&[px(0), px(257)].concat(), &mut out);
        Ok(out[1][0] - out[0][0])
    }

    /// The working gamut (Rec.2020, sRGB-encoded, 16-bit) → this profile's 16-bit
    /// device values, unoptimized: Little CMS would otherwise resample the transform
    /// into a grid whose cells straddle the gamut surface (≈ 2 ΔE off at the primaries).
    fn wide_to_device(&self, s: ProofSettings) -> Result<Transform<u8, u8>, ExportError> {
        let wide = Profile::new_icc(icc::profile(WIDE)).map_err(profile_error)?;
        Transform::new_flags(
            &wide,
            PixelFormat::RGB_16,
            &self.lcms()?,
            self.space.format16(),
            s.intent.lcms(),
            s.flags() | Flags::NO_OPTIMIZE,
        )
        .map_err(|e| profile_error(format!("{}: can't render into it ({e})", self.name)))
    }
}

/// Linear Rec.2020 → a matrix/TRC profile's linear RGB, from its colorant tags (both
/// adapted to D50, the ICC way).
fn wide_to_colorants(p: &Profile) -> Option<[[f32; 3]; 3]> {
    let col = |t| match p.read_tag(t) {
        Tag::CIEXYZ(xyz) => Some([xyz.X, xyz.Y, xyz.Z]),
        _ => None,
    };
    let (r, g, b) = (
        col(TagSignature::RedColorantTag)?,
        col(TagSignature::GreenColorantTag)?,
        col(TagSignature::BlueColorantTag)?,
    );
    let target: icc::M3 = [0, 1, 2].map(|i| [r[i], g[i], b[i]]);
    let (wide, _) = icc::colorants(icc::REC2020_PRIMARIES);
    let m = icc::mul(&icc::inv(&target), &wide);
    Some(m.map(|row| row.map(|v| v as f32)))
}

fn inverse(m: &[[f32; 3]; 3]) -> [[f32; 3]; 3] {
    icc::inv(&m.map(|r| r.map(f64::from))).map(|r| r.map(|v| v as f32))
}

fn mul3(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    [0, 1, 2].map(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

/// 16-bit sRGB-encoded value → linear.
fn srgb_decode_table() -> &'static [f32] {
    static T: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        (0..=65_535u32)
            .map(|v| srgb_to_linear(v as f32 / 65_535.0))
            .collect()
    })
}

/// [`IccProfile::is_linear`] cut-off: ΔL* of the first 8-bit step above black.
const LINEAR_DARK_STEP_L: f64 = 1.5;

fn lab_profile() -> Result<Profile, ExportError> {
    Profile::new_lab4_context(GlobalContext::new(), CIExyY::d50()).map_err(profile_error)
}

/// The viewer's proof table: `size³` RGBA texels, red fastest, then green, then blue.
/// Input coordinates are Rec.2020, sRGB-encoded; RGB is the proofed colour in *linear*
/// Rec.2020; alpha is the signed distance to the gamut surface in cells (> 0: out).
#[derive(Debug, Clone, PartialEq)]
pub struct ProofLut {
    pub size: usize,
    pub rgba: Vec<f32>,
    /// Gray profiles: their gamut is the neutral axis (thinner than a cell, so no grid
    /// field can hold it); a colour is out when its chroma — max − min of the encoded
    /// Rec.2020 components — exceeds this. `None`: use alpha.
    pub neutral_tolerance: Option<f32>,
}

impl ProofLut {
    /// Whether the viewer marks this encoded Rec.2020 colour as out of gamut (the CPU
    /// reference of the shader's test).
    pub fn out_of_gamut(&self, wide_encoded: [f32; 3]) -> bool {
        match self.neutral_tolerance {
            Some(t) => {
                let e = wide_encoded.map(|v| v.clamp(0.0, 1.0));
                e[0].max(e[1]).max(e[2]) - e[0].min(e[1]).min(e[2]) > t
            }
            None => self.sample(wide_encoded)[3] > 0.0,
        }
    }

    /// Trilinear lookup of an encoded Rec.2020 colour, as the viewer's sampler does
    /// (the CPU reference of the shader).
    pub fn sample(&self, wide_encoded: [f32; 3]) -> [f32; 4] {
        let n = self.size;
        let last = (n - 1) as f32;
        let pos = wide_encoded.map(|v| v.clamp(0.0, 1.0) * last);
        let i0 = pos.map(|p| (p.floor() as usize).min(n - 2));
        let f = [0, 1, 2].map(|k| pos[k] - i0[k] as f32);
        let at = |r: usize, g: usize, b: usize| (r + n * (g + n * b)) * 4;
        let mut out = [0.0; 4];
        for corner in 0..8 {
            let d = [corner & 1, (corner >> 1) & 1, (corner >> 2) & 1];
            let w: f32 = (0..3)
                .map(|k| if d[k] == 1 { f[k] } else { 1.0 - f[k] })
                .product();
            let o = at(i0[0] + d[0], i0[1] + d[1], i0[2] + d[2]);
            for (c, v) in out.iter_mut().enumerate() {
                *v += w * self.rgba[o + c];
            }
        }
        out
    }
}

/// sRGB transfer function, encoded → linear.
pub fn srgb_to_linear(v: f32) -> f32 {
    process::srgb_to_linear(f64::from(v)) as f32
}

/// sRGB transfer function, linear → encoded.
pub fn linear_to_srgb(v: f32) -> f32 {
    process::linear_to_srgb(f64::from(v)) as f32
}

fn delta_e(a: [f64; 3], b: [f64; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt() as f32
}

fn bytes16(px: &[[u16; 3]]) -> Vec<u8> {
    px.iter().flatten().flat_map(|v| v.to_ne_bytes()).collect()
}

/// The gamut test: how far a colour's relative-colorimetric round trip through the
/// proof device moves it (ΔE76), over the profile's threshold.
struct GamutCheck {
    to_device: Transform<u8, u8>,
    device_to_lab: Transform<u8, [f64; 3]>,
    wide_to_lab: Transform<[u16; 3], [f64; 3]>,
    device_bpp: usize,
    threshold: f32,
}

impl GamutCheck {
    fn new(profile: &IccProfile) -> Result<Self, ExportError> {
        let lab = lab_profile()?;
        let wide = Profile::new_icc(icc::profile(WIDE)).map_err(profile_error)?;
        let dev = profile.space.format16();
        Ok(Self {
            to_device: profile.wide_to_device(RELATIVE)?,
            device_to_lab: Transform::new_flags(
                &profile.lcms()?,
                dev,
                &lab,
                PixelFormat::Lab_DBL,
                Intent::RelativeColorimetric,
                Flags::NO_OPTIMIZE,
            )
            .map_err(profile_error)?,
            wide_to_lab: Transform::new_flags(
                &wide,
                PixelFormat::RGB_16,
                &lab,
                PixelFormat::Lab_DBL,
                Intent::RelativeColorimetric,
                Flags::NO_OPTIMIZE,
            )
            .map_err(profile_error)?,
            device_bpp: dev.bytes_per_pixel(),
            threshold: profile.gamut_threshold(),
        })
    }

    /// ΔE / threshold per colour (> 1: out of gamut).
    fn measure(&self, wide: &[[u16; 3]]) -> Vec<f32> {
        let mut device = vec![0u8; wide.len() * self.device_bpp];
        self.to_device.transform_pixels(&bytes16(wide), &mut device);
        let mut round = vec![[0.0f64; 3]; wide.len()];
        self.device_to_lab.transform_pixels(&device, &mut round);
        let mut orig = vec![[0.0f64; 3]; wide.len()];
        self.wide_to_lab.transform_pixels(wide, &mut orig);
        orig.iter()
            .zip(&round)
            .map(|(a, b)| delta_e(*a, *b) / self.threshold)
            .collect()
    }
}

/// Proof a list of Rec.2020 colours (sRGB-encoded, 16-bit): for each, the proofed
/// colour in linear Rec.2020 and the round trip's ΔE / threshold (> 1: out of gamut).
pub fn proof_colors(
    profile: &IccProfile,
    settings: ProofSettings,
    wide: &[[u16; 3]],
) -> Result<Vec<[f32; 4]>, ExportError> {
    let wide_profile = Profile::new_icc(icc::profile(WIDE)).map_err(profile_error)?;
    let dev = profile.space.format16();
    let mut device = vec![0u8; wide.len() * dev.bytes_per_pixel()];
    // The simulation: Rec.2020 → (soft gamut compression, as an export does) → device
    // (intent, BPC) → Rec.2020, relative colorimetric.
    let mut compressed = wide.to_vec();
    profile.compress_wide(&mut compressed);
    profile
        .wide_to_device(settings)?
        .transform_pixels(&bytes16(&compressed), &mut device);
    let back: Transform<u8, [f64; 3]> = Transform::new_flags(
        &profile.lcms()?,
        dev,
        &wide_profile,
        PixelFormat::RGB_DBL,
        Intent::RelativeColorimetric,
        Flags::NO_OPTIMIZE,
    )
    .map_err(profile_error)?;
    let mut shown = vec![[0.0f64; 3]; wide.len()];
    back.transform_pixels(&device, &mut shown);
    let gamut = GamutCheck::new(profile)?.measure(wide);
    Ok(shown
        .iter()
        .zip(gamut)
        .map(|(c, g)| {
            let [r, gr, b] = c.map(|v| srgb_to_linear(v.clamp(0.0, 1.0) as f32));
            [r, gr, b, g]
        })
        .collect())
}

/// Build the viewer's proof table (`size` grid points per axis, 2…65).
pub fn proof_lut(
    profile: &IccProfile,
    settings: ProofSettings,
    size: usize,
) -> Result<ProofLut, ExportError> {
    let n = size.clamp(2, 65);
    let level = |i: usize| ((i * 65_535 + (n - 1) / 2) / (n - 1)) as u16;
    let grid: Vec<[u16; 3]> = (0..n * n * n)
        .map(|i| [level(i % n), level(i / n % n), level(i / (n * n))])
        .collect();
    let colours = proof_colors(profile, settings, &grid)?;
    let outside: Vec<bool> = colours.iter().map(|c| c[3] > 1.0).collect();
    let check = GamutCheck::new(profile)?;
    let alpha = surface_distance(&check, n, &grid, &outside);
    let rgba = colours
        .iter()
        .zip(alpha)
        .flat_map(|(c, a)| [c[0], c[1], c[2], a])
        .collect();
    let neutral_tolerance =
        (profile.space == ProfileSpace::Gray).then(|| neutral_tolerance(&check));
    Ok(ProofLut {
        size: n,
        rgba,
        neutral_tolerance,
    })
}

/// A gray profile's gamut as an encoded chroma (max − min) limit: the smallest chroma
/// at which the exact test fails, bisected along six hue directions at three
/// lightnesses.
fn neutral_tolerance(check: &GamutCheck) -> f32 {
    let dirs: [[f32; 3]; 6] = [
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 1.0, 1.0],
        [1.0, 0.0, 1.0],
        [1.0, 1.0, 0.0],
    ];
    let rays: Vec<([f32; 3], [f32; 3])> = [0.25f32, 0.5, 0.75]
        .iter()
        .flat_map(|&l| dirs.iter().map(move |d| ([l; 3], d.map(|v| v - 0.5))))
        .collect();
    let at = |(base, dir): &([f32; 3], [f32; 3]), t: f32| -> [u16; 3] {
        [0, 1, 2].map(|k| ((base[k] + dir[k] * t).clamp(0.0, 1.0) * 65_535.0).round() as u16)
    };
    let mut lo = vec![0.0f32; rays.len()];
    let mut hi = vec![0.5f32; rays.len()];
    for _ in 0..SURFACE_STEPS {
        let mids: Vec<[u16; 3]> = rays
            .iter()
            .zip(lo.iter().zip(&hi))
            .map(|(r, (l, h))| at(r, f32::midpoint(*l, *h)))
            .collect();
        for (k, m) in check.measure(&mids).into_iter().enumerate() {
            let mid = f32::midpoint(lo[k], hi[k]);
            if m > 1.0 {
                hi[k] = mid;
            } else {
                lo[k] = mid;
            }
        }
    }
    // Every direction is ±0.5 per channel, so chroma (max − min) is t.
    lo.into_iter().fold(f32::INFINITY, f32::min)
}

/// Signed distance (in cells, clamped to ±1) from each grid point to the gamut
/// surface, positive outside: every grid edge whose ends disagree is bisected for the
/// crossing, and each point keeps its nearest crossing.
fn surface_distance(check: &GamutCheck, n: usize, grid: &[[u16; 3]], outside: &[bool]) -> Vec<f32> {
    let mut dist: Vec<f32> = outside
        .iter()
        .map(|&o| if o { 1.0 } else { -1.0 })
        .collect();
    // (inside point, outside point) of every edge crossing the surface.
    let mut edges = Vec::new();
    for i in 0..grid.len() {
        let coord = [i % n, i / n % n, i / (n * n)];
        for (axis, step) in [1, n, n * n].into_iter().enumerate() {
            let j = i + step;
            if coord[axis] + 1 < n && outside[i] != outside[j] {
                edges.push(if outside[i] { (j, i) } else { (i, j) });
            }
        }
    }
    let lerp = |a: [u16; 3], b: [u16; 3], t: f32| {
        [0, 1, 2]
            .map(|k| (f32::from(a[k]) + (f32::from(b[k]) - f32::from(a[k])) * t).round() as u16)
    };
    let mut lo = vec![0.0f32; edges.len()];
    let mut hi = vec![1.0f32; edges.len()];
    for _ in 0..SURFACE_STEPS {
        let mids: Vec<[u16; 3]> = edges
            .iter()
            .zip(lo.iter().zip(&hi))
            .map(|(&(a, b), (l, h))| lerp(grid[a], grid[b], f32::midpoint(*l, *h)))
            .collect();
        for (k, m) in check.measure(&mids).into_iter().enumerate() {
            let mid = f32::midpoint(lo[k], hi[k]);
            if m > 1.0 {
                hi[k] = mid;
            } else {
                lo[k] = mid;
            }
        }
    }
    for (k, &(a, b)) in edges.iter().enumerate() {
        let t = f32::midpoint(lo[k], hi[k]);
        dist[a] = dist[a].max(-t);
        dist[b] = dist[b].min(1.0 - t);
    }
    dist
}

/// An export recipe's output profile: rendered pixels are converted to it and it is
/// embedded instead of the recipe space's profile. RGB profiles only.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputProfile {
    pub profile: IccProfile,
    pub settings: ProofSettings,
}

/// Rows converted per parallel chunk.
const CONVERT_ROWS: usize = 64;

impl OutputProfile {
    pub fn new(profile: IccProfile, settings: ProofSettings) -> Result<Self, ExportError> {
        // A proof may use a larger profile than a file can embed (JPEG's ICC chunks).
        if profile.bytes.len() > icc::MAX_EMBEDDED_BYTES {
            return Err(profile_error(format!(
                "{} is {} bytes; a file can embed at most {}",
                profile.name,
                profile.bytes.len(),
                icc::MAX_EMBEDDED_BYTES
            )));
        }
        if profile.space != ProfileSpace::Rgb {
            return Err(profile_error(format!(
                "{} is a {:?} profile; export needs an RGB profile",
                profile.name, profile.space
            )));
        }
        Ok(Self { profile, settings })
    }

    /// Precalculated at Little CMS's high resolution: fast for full-size exports, and
    /// within interpolation error of the proof's exact transform (most near the gamut
    /// surface; see ARCHITECTURE.md for the measured difference).
    fn convert<P: lcms2::Pod + Send>(
        &self,
        from: OutputSpace,
        fmt: PixelFormat,
        pixels: &mut [P],
        chunk: usize,
    ) -> Result<(), ExportError> {
        let src = Profile::new_icc(icc::profile(from)).map_err(profile_error)?;
        let t: Transform<P, P, GlobalContext, lcms2::DisallowCache> = Transform::new_flags_context(
            GlobalContext::new(),
            &src,
            fmt,
            &self.profile.lcms()?,
            fmt,
            self.settings.intent.lcms(),
            self.settings.flags() | Flags::HIGHRES_PRECALC,
        )
        .map_err(profile_error)?;
        pixels
            .par_chunks_mut(chunk.max(1))
            .for_each(|c| t.transform_in_place(c));
        Ok(())
    }

    /// Convert interleaved 16-bit RGB rows of `width` pixels in `from` to this
    /// profile, in place.
    /// A Rec.2020 render is first soft-compressed into a matrix profile's gamut (see
    /// `IccProfile::compress_wide`), as a plain sRGB or P3 render is.
    pub fn convert_rgb16(
        &self,
        from: OutputSpace,
        data: &mut [u16],
        width: usize,
    ) -> Result<(), ExportError> {
        let (px, _) = data.as_chunks_mut::<3>();
        if from == OutputSpace::Rec2020 {
            self.profile.compress_wide(px);
        }
        self.convert(from, PixelFormat::RGB_16, px, width * CONVERT_ROWS)
    }
}

/// Convert interleaved 16-bit RGB from one built-in space to another (relative
/// colorimetric), in place: e.g. a Display P3 decode handed over as Rec.2020.
pub fn convert_space(
    from: OutputSpace,
    to: OutputSpace,
    data: &mut [u16],
    width: usize,
) -> Result<(), ExportError> {
    if from == to {
        return Ok(());
    }
    OutputProfile::new(IccProfile::from_bytes(icc::profile(to))?, RELATIVE)?
        .convert_rgb16(from, data, width)
}

#[cfg(test)]
mod tests {
    use lcms2::{CIExyYTRIPLE, Tag, ToneCurve};

    use super::*;

    /// Adobe RGB (1998) primaries; built with the sRGB TRC, enough for gamut tests.
    pub(crate) const ADOBE_PRIMARIES: [(f64, f64); 3] = [(0.64, 0.33), (0.21, 0.71), (0.15, 0.06)];
    const SYSTEM_CMYK: &str = "/System/Library/ColorSync/Profiles/Generic CMYK Profile.icc";

    type M3 = [[f32; 3]; 3];
    const SRGB_TO_2020: M3 = [
        [0.627_404, 0.329_283, 0.043_313],
        [0.069_097, 0.919_54, 0.011_362],
        [0.016_391, 0.088_013, 0.895_595],
    ];
    const P3_TO_2020: M3 = [
        [0.753_833, 0.198_597, 0.047_57],
        [0.045_744, 0.941_777, 0.012_479],
        [-0.001_21, 0.017_602, 0.983_609],
    ];
    const REC2020_TO_SRGB: M3 = [
        [1.660_491, -0.587_641, -0.072_85],
        [-0.124_55, 1.1329, -0.008_349],
        [-0.018_151, -0.100_579, 1.118_73],
    ];

    fn mat(m: &M3, c: [f32; 3]) -> [f32; 3] {
        [0, 1, 2].map(|i| m[i][0] * c[0] + m[i][1] * c[1] + m[i][2] * c[2])
    }

    fn srgb() -> IccProfile {
        IccProfile::from_bytes(icc::profile(OutputSpace::Srgb)).unwrap()
    }

    fn rel(bpc: bool) -> ProofSettings {
        ProofSettings {
            intent: ProofIntent::RelativeColorimetric,
            black_point_compensation: bpc,
        }
    }

    /// Linear Rec.2020 → the table's 16-bit encoded input.
    fn enc16(lin: [f32; 3]) -> [u16; 3] {
        lin.map(|v| (linear_to_srgb(v.clamp(0.0, 1.0)) * 65_535.0).round() as u16)
    }

    fn close(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tol)
    }

    fn system_cmyk() -> Option<IccProfile> {
        IccProfile::read(Path::new(SYSTEM_CMYK)).ok()
    }

    /// An RGB profile with sRGB primaries, a 2.2 gamma raised to a black of `black`
    /// (relative luminance), and `gamma` (1.0: linear-light).
    fn rgb_profile(black: f64, gamma: f64) -> IccProfile {
        let a = (1.0 - black).powf(1.0 / gamma);
        // Parametric type 5: Y = (aX + b)^g + e for X ≥ d.
        let curve = ToneCurve::new_parametric(5, &[gamma, a, 0.0, 0.0, 0.0, black, 0.0]).unwrap();
        let xy = |x, y| CIExyY { x, y, Y: 1.0 };
        let p = Profile::new_rgb(
            &xy(0.3127, 0.3290),
            &CIExyYTRIPLE {
                Red: xy(0.64, 0.33),
                Green: xy(0.30, 0.60),
                Blue: xy(0.15, 0.06),
            },
            &[&curve, &curve, &curve],
        )
        .unwrap();
        IccProfile::from_bytes(&p.icc().unwrap()).unwrap()
    }

    /// A gray profile (gamma 2.2) whose black is raised to 5% luminance.
    fn raised_black_gray() -> IccProfile {
        let curve =
            ToneCurve::new_parametric(5, &[2.2, 0.95f64.powf(1.0 / 2.2), 0.0, 0.0, 0.0, 0.05, 0.0])
                .unwrap();
        let p = Profile::new_gray(CIExyY::d50(), &curve).unwrap();
        IccProfile::from_bytes(&p.icc().unwrap()).unwrap()
    }

    /// sRGB's matrix/TRC tags plus A2B0/B2A0 LUTs (Little CMS's resampled sRGB ↔ Lab),
    /// as calibrators write "table + matrix" display profiles. With `perceptual_gamma`,
    /// a B2A1 (relative colorimetric) table is the sRGB one and B2A0 (perceptual) encodes
    /// with that gamma instead, so the two intents render differently.
    fn hybrid() -> IccProfile {
        hybrid_with(None)
    }

    fn hybrid_with(perceptual_gamma: Option<f64>) -> IccProfile {
        let srgb = Profile::new_icc(icc::profile(OutputSpace::Srgb)).unwrap();
        let lab = lab_profile().unwrap();
        let link = |from: &Profile, f, to: &Profile, t| {
            let tr: Transform<u8, u8> = Transform::new_flags(
                from,
                f,
                to,
                t,
                Intent::RelativeColorimetric,
                Flags::FORCE_CLUT | Flags::LOWRES_PRECALC,
            )
            .unwrap();
            Profile::new_device_link(&tr, 4.3, Flags::default()).unwrap()
        };
        let a2b = link(&srgb, PixelFormat::RGB_16, &lab, PixelFormat::Lab_16);
        let b2a = link(&lab, PixelFormat::Lab_16, &srgb, PixelFormat::RGB_16);
        let other = perceptual_gamma.map(|g| {
            let curve = ToneCurve::new(g);
            let xy = |x, y| CIExyY { x, y, Y: 1.0 };
            let p = Profile::new_rgb(
                &xy(0.3127, 0.3290),
                &CIExyYTRIPLE {
                    Red: xy(0.64, 0.33),
                    Green: xy(0.30, 0.60),
                    Blue: xy(0.15, 0.06),
                },
                &[&curve, &curve, &curve],
            )
            .unwrap();
            link(&lab, PixelFormat::Lab_16, &p, PixelFormat::RGB_16)
        });
        let mut p = Profile::new_icc(icc::profile(OutputSpace::Srgb)).unwrap();
        p.set_pcs(ColorSpaceSignature::LabData);
        let Tag::Pipeline(fwd) = a2b.read_tag(TagSignature::AToB0Tag) else {
            panic!("no A2B0")
        };
        assert!(p.write_tag(TagSignature::AToB0Tag, Tag::Pipeline(fwd)));
        let Tag::Pipeline(back) = b2a.read_tag(TagSignature::AToB0Tag) else {
            panic!("no B2A0")
        };
        if let Some(other) = &other {
            assert!(p.write_tag(TagSignature::BToA1Tag, Tag::Pipeline(back)));
            let Tag::Pipeline(perc) = other.read_tag(TagSignature::AToB0Tag) else {
                panic!("no perceptual B2A0")
            };
            assert!(p.write_tag(TagSignature::BToA0Tag, Tag::Pipeline(perc)));
        } else {
            assert!(p.write_tag(TagSignature::BToA0Tag, Tag::Pipeline(back)));
        }
        IccProfile::from_bytes(&p.icc().unwrap()).unwrap()
    }

    #[test]
    fn srgb_proof_of_srgb_colours_is_identity() {
        let p = srgb();
        assert_eq!(p.space(), ProfileSpace::Rgb);
        assert_eq!(p.name(), "sRGB (built-in)");
        assert!(p.matrix_only() && !p.is_linear());
        assert!((p.gamut_threshold() - GAMUT_DELTA_E_MATRIX).abs() < f32::EPSILON);
        let colours = [
            [0.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.18, 0.18, 0.18],
            // Saturated, but within the compression's untouched zone (see
            // `matrix_targets_compress_like_the_pipeline` for beyond it).
            [0.9, 0.25, 0.2],
            [0.25, 0.9, 0.2],
            [0.2, 0.25, 0.9],
            [0.6, 0.3, 0.1],
        ];
        for intent in [ProofIntent::Perceptual, ProofIntent::RelativeColorimetric] {
            for bpc in [false, true] {
                let s = ProofSettings {
                    intent,
                    black_point_compensation: bpc,
                };
                let wide: Vec<[f32; 3]> = colours.iter().map(|c| mat(&SRGB_TO_2020, *c)).collect();
                let input: Vec<[u16; 3]> = wide.iter().map(|c| enc16(*c)).collect();
                let out = proof_colors(&p, s, &input).unwrap();
                for (c, o) in wide.iter().zip(&out) {
                    assert!(close(*c, [o[0], o[1], o[2]], 2e-3), "{s:?}: {c:?} → {o:?}");
                    assert!(o[3] < 1.0, "{s:?}: {c:?} flagged out of gamut ({})", o[3]);
                }
            }
        }
    }

    #[test]
    fn colours_outside_p3_are_judged_as_themselves() {
        // P3's green is out of sRGB and is brought in as a plain sRGB render would be.
        let p3_green = enc16(mat(&P3_TO_2020, [0.0, 1.0, 0.0]));
        let out = proof_colors(&srgb(), rel(false), &[p3_green]).unwrap()[0];
        assert!(out[3] > 10.0, "P3 green ΔE/threshold {}", out[3]);
        let in_srgb =
            ds_core::gamut::compress(mat(&REC2020_TO_SRGB, mat(&P3_TO_2020, [0.0, 1.0, 0.0])));
        let green = mat(&SRGB_TO_2020, in_srgb.map(|v| v.min(1.0)));
        assert!(
            close([out[0], out[1], out[2]], green, 0.02),
            "{out:?} vs {green:?}"
        );
        // Rec.2020's green is outside Display P3 itself: a P3 proof flags it, though its
        // P3 clip (what an 8-bit P3 frame would carry) is in gamut.
        let p3 = IccProfile::from_bytes(icc::profile(OutputSpace::DisplayP3)).unwrap();
        let r = proof_colors(&p3, rel(false), &[[0, 65_535, 0], p3_green]).unwrap();
        assert!(r[0][3] > 1.0 && r[1][3] < 1.0, "{r:?}");
        // Adobe RGB's wider green is closer to a Rec.2020 green than sRGB's.
        let adobe =
            IccProfile::from_bytes(&icc::build_profile("Adobe-like", ADOBE_PRIMARIES)).unwrap();
        let a = proof_colors(&adobe, rel(false), &[[0, 65_535, 0]]).unwrap()[0];
        let s = proof_colors(&srgb(), rel(false), &[[0, 65_535, 0]]).unwrap()[0];
        assert!(a[3] < s[3], "adobe {} vs srgb {}", a[3], s[3]);
    }

    #[test]
    /// Everything is in gamut; colours are shown as themselves, saturated ones softly
    /// compressed as into any matrix profile.
    fn a_rec2020_proof_is_identity_everywhere() {
        let wide = IccProfile::from_bytes(icc::profile(OutputSpace::Rec2020)).unwrap();
        let lut = proof_lut(&wide, rel(false), 9).unwrap();
        assert_eq!((lut.size, lut.rgba.len()), (9, 9 * 9 * 9 * 4));
        for (i, t) in lut.rgba.as_chunks::<4>().0.iter().enumerate() {
            let enc = [i % 9, i / 9 % 9, i / 81].map(|v| v as f32 / 8.0);
            let want = ds_core::gamut::compress(enc.map(srgb_to_linear));
            assert!(close([t[0], t[1], t[2]], want, 2e-3), "{enc:?} → {t:?}");
            assert!(
                (t[3] + 1.0).abs() < f32::EPSILON,
                "{enc:?}: no surface, alpha {}",
                t[3]
            );
        }
    }

    #[test]
    fn lut_sampling_interpolates_and_marks_the_gamut_edge() {
        let lut = proof_lut(&srgb(), rel(false), LUT_SIZE).unwrap();
        assert_eq!(lut.rgba.len(), LUT_SIZE.pow(3) * 4);
        let black = lut.sample([0.0, 0.0, 0.0]);
        let white = lut.sample([1.0, 1.0, 1.0]);
        assert!(black[..3].iter().all(|v| v.abs() < 1e-3), "{black:?}");
        assert!(
            white[..3].iter().all(|v| (v - 1.0).abs() < 2e-3),
            "{white:?}"
        );
        // An off-grid sRGB colour comes back unchanged and in gamut.
        let c = mat(&SRGB_TO_2020, [0.3, 0.5, 0.2]);
        let s = lut.sample(c.map(linear_to_srgb));
        assert!(close([s[0], s[1], s[2]], c, 0.01), "{s:?} vs {c:?}");
        assert!(s[3] < 0.0);
        // Rec.2020's primaries are out; out-of-range input clamps.
        for p in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.5]] {
            assert!(lut.sample(p)[3] > 0.0, "{p:?}");
        }
        let (a, b) = (lut.sample([-1.0, 2.0, 0.0]), lut.sample([0.0, 1.0, 0.0]));
        assert!(a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6));
    }

    /// The mask the viewer draws (the table's interpolated sign) against the exact gamut
    /// test, over a 25³ sweep of the whole working gamut, plus a one-sided check: no
    /// colour comfortably inside sRGB is ever flagged (the unsigned ΔE measure bulged
    /// about half a cell inward).
    #[test]
    fn gamut_mask_matches_the_exact_test() {
        let p = srgb();
        let lut = proof_lut(&p, rel(false), LUT_SIZE).unwrap();
        let k = 25;
        let sweep: Vec<[u16; 3]> = (0..k * k * k)
            .map(|i| [i % k, i / k % k, i / (k * k)].map(|v| (v * 65_535 / (k - 1)) as u16))
            .collect();
        let exact = GamutCheck::new(&p).unwrap().measure(&sweep);
        let (mut agree, mut inside, mut inside_flagged) = (0, 0, 0);
        for (c, e) in sweep.iter().zip(&exact) {
            let enc = c.map(|v| f32::from(v) / 65_535.0);
            let flagged = lut.sample(enc)[3] > 0.0;
            if flagged == (*e > 1.0) {
                agree += 1;
            }
            let srgb_enc = mat(&REC2020_TO_SRGB, enc.map(srgb_to_linear)).map(linear_to_srgb);
            if srgb_enc.iter().all(|v| (0.005..=0.995).contains(v)) {
                inside += 1;
                inside_flagged += usize::from(flagged);
            }
        }
        assert!(inside > 500, "{inside}");
        assert_eq!(
            inside_flagged, 0,
            "{inside_flagged} of {inside} colours inside sRGB flagged"
        );
        assert!(
            agree * 1000 >= sweep.len() * 995,
            "mask agrees on {agree} of {}",
            sweep.len()
        );
    }

    #[test]
    fn profiles_with_luts_get_the_lut_threshold() {
        let h = hybrid();
        assert!(!h.matrix_only(), "LUTs present");
        assert!((h.gamut_threshold() - GAMUT_DELTA_E_LUT).abs() < f32::EPSILON);
        // Its in-gamut colours aren't flagged despite the LUT round trip's noise.
        let colours: Vec<[u16; 3]> = [
            [0.5, 0.4, 0.3],
            [0.2, 0.25, 0.3],
            [0.05, 0.05, 0.06],
            [0.8, 0.7, 0.6],
        ]
        .iter()
        .map(|c| enc16(mat(&SRGB_TO_2020, *c)))
        .collect();
        for o in proof_colors(&h, rel(false), &colours).unwrap() {
            assert!(o[3] < 1.0, "{o:?}");
        }
    }

    #[test]
    fn intent_and_bpc_reach_the_transform() {
        // A profile whose perceptual table differs from its colorimetric one: the
        // intent picks the table the proof goes through.
        let two = hybrid_with(Some(1.4));
        let mid = [enc16(mat(&SRGB_TO_2020, [0.3, 0.2, 0.1]))];
        let pr = proof_colors(&two, ProofSettings::default(), &mid).unwrap()[0];
        let rc = proof_colors(&two, rel(false), &mid).unwrap()[0];
        assert!(
            close(
                [rc[0], rc[1], rc[2]],
                mat(&SRGB_TO_2020, [0.3, 0.2, 0.1]),
                0.01
            ),
            "{rc:?}"
        );
        assert!(
            !close([pr[0], pr[1], pr[2]], [rc[0], rc[1], rc[2]], 0.01),
            "{pr:?} {rc:?}"
        );
        // Raised blacks: without BPC, everything below the profile's black clips to it;
        // with BPC, shadows are scaled into its range and stay apart.
        let g = raised_black_gray();
        let dark = [enc16([0.0; 3]), enc16([0.02; 3])];
        let off = proof_colors(&g, rel(false), &dark).unwrap();
        let on = proof_colors(&g, rel(true), &dark).unwrap();
        assert!(
            (off[1][1] - off[0][1]).abs() < 1e-3,
            "no BPC: both at the black {off:?}"
        );
        assert!(
            on[1][1] > on[0][1] + 5e-3,
            "BPC keeps the shadow apart {on:?}"
        );
        assert!(
            on[0][1] >= off[0][1] - 1e-4 && on[0][1] < 0.06,
            "black is the profile's black {on:?}"
        );
        // A gray profile shows gray, and only neutrals are in its gamut.
        let out = proof_colors(&g, rel(true), &[[40_000; 3], [65_535, 0, 0]]).unwrap();
        assert!((out[0][0] - out[0][1]).abs() < 1e-3 && (out[0][1] - out[0][2]).abs() < 1e-3);
        assert!(out[0][3] < 1.0 && out[1][3] > 1.0, "{out:?}");
        assert_eq!(
            proof_lut(&g, ProofSettings::default(), 500).unwrap().size,
            65
        );
        // The same in an export: a dark gray lands above the device black only with BPC.
        let raised = rgb_profile(0.05, 2.2);
        assert!(!raised.is_linear());
        let convert = |bpc: bool| {
            let mut px = vec![1_200; 3];
            OutputProfile::new(raised.clone(), rel(bpc))
                .unwrap()
                .convert_rgb16(OutputSpace::Srgb, &mut px, 1)
                .unwrap();
            px[1]
        };
        assert!(
            convert(false) < 50 && convert(true) > 500,
            "{} {}",
            convert(false),
            convert(true)
        );
    }

    /// Generic CMYK (macOS): skipped where the system profile is missing.
    #[test]
    fn cmyk_proofing() {
        let Some(cmyk) = system_cmyk() else { return };
        assert_eq!(cmyk.space(), ProfileSpace::Cmyk);
        assert!(!cmyk.matrix_only() && !cmyk.is_linear());
        assert!((cmyk.gamut_threshold() - GAMUT_DELTA_E_LUT).abs() < f32::EPSILON);
        let input: Vec<[u16; 3]> = [
            [0.18, 0.18, 0.18], // mid gray
            [0.45, 0.30, 0.22], // skin
            [1.0, 0.0, 0.0],    // P3 red (below), green, blue
        ]
        .iter()
        .map(|c| enc16(mat(&SRGB_TO_2020, *c)))
        .chain(
            [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]].map(|c| enc16(mat(&P3_TO_2020, c))),
        )
        .collect();
        let r = proof_colors(&cmyk, rel(true), &input).unwrap();
        assert!(r[0][3] < 1.0 && r[1][3] < 1.0, "gray and skin print: {r:?}");
        assert!(
            r[3..].iter().all(|c| c[3] > 1.0),
            "P3 primaries don't: {r:?}"
        );
        // The proof shows paper: mid gray stays neutral within a few percent.
        assert!((r[0][0] - r[0][2]).abs() < 0.03, "{:?}", r[0]);
        // BPC maps black to the press's black: the shown black moves (by ~0.0025 linear
        // for Generic CMYK).
        let black = [enc16([0.0; 3])];
        let off = proof_colors(&cmyk, rel(false), &black).unwrap()[0];
        let on = proof_colors(&cmyk, rel(true), &black).unwrap()[0];
        assert!((on[1] - off[1]).abs() > 1e-3, "{off:?} {on:?}");
        // The table builds and flags the same colours.
        let lut = proof_lut(&cmyk, ProofSettings::default(), 17).unwrap();
        assert!(lut.sample([0.0, 1.0, 0.0])[3] > 0.0);
        let gray = enc16(mat(&SRGB_TO_2020, [0.18; 3])).map(|v| f32::from(v) / 65_535.0);
        assert!(lut.sample(gray)[3] < 0.0);
        // Export needs RGB.
        assert!(OutputProfile::new(cmyk, rel(false)).is_err());
    }

    /// A gray profile's gamut is a line: the table can't hold it, so the viewer tests
    /// chroma instead; off-grid neutrals are never flagged.
    #[test]
    fn gray_profiles_flag_chroma_not_neutrals() {
        let g = raised_black_gray();
        let lut = proof_lut(&g, rel(true), LUT_SIZE).unwrap();
        let t = lut.neutral_tolerance.unwrap();
        assert!((1e-3..0.05).contains(&t), "{t}");
        let neutrals: Vec<[f32; 3]> = (0..100).map(|i| [(i as f32 + 0.37) / 100.5; 3]).collect();
        assert!(neutrals.iter().all(|n| !lut.out_of_gamut(*n)));
        // The interpolated alpha alone would flag most of them.
        assert!(neutrals.iter().filter(|n| lut.sample(**n)[3] > 0.0).count() > 50);
        let tinted = |c: f32| [0.5 + c / 2.0, 0.5 - c / 2.0, 0.5];
        assert!(!lut.out_of_gamut(tinted(t * 0.5)) && lut.out_of_gamut(tinted(t * 2.0)));
        assert!(lut.out_of_gamut([1.0, 0.0, 0.0]));
        // The tolerance agrees with the exact test.
        let check = GamutCheck::new(&g).unwrap();
        let m = check.measure(
            &[tinted(t * 0.8), tinted(t * 1.5)].map(|c| c.map(|v| (v * 65_535.0).round() as u16)),
        );
        assert!(m[0] <= 1.0 && m[1] > 1.0, "{m:?}");
        assert_eq!(
            proof_lut(&srgb(), rel(false), 5).unwrap().neutral_tolerance,
            None
        );
    }

    /// Colours outside a matrix profile are soft-compressed into it like the develop
    /// pipeline does for a plain sRGB render, not clipped channel by channel, in the
    /// export and in the proof.
    #[test]
    fn matrix_targets_compress_like_the_pipeline() {
        let blue = [0.05f32, 0.05, 0.9]; // linear Rec.2020, far outside sRGB
        let pipeline = ds_core::gamut::compress(mat(&REC2020_TO_SRGB, blue));
        let expect = pipeline.map(|v| (linear_to_srgb(v.min(1.0)) * 65_535.0).round() as u16);
        let clipped = mat(&REC2020_TO_SRGB, blue)
            .map(|v| (linear_to_srgb(v.clamp(0.0, 1.0)) * 65_535.0).round() as u16);
        assert!(
            expect
                .iter()
                .zip(clipped)
                .any(|(a, b)| a.abs_diff(b) > 3_000),
            "{expect:?} {clipped:?}"
        );
        let mut px = enc16(blue).to_vec();
        OutputProfile::new(srgb(), ProofSettings::default())
            .unwrap()
            .convert_rgb16(OutputSpace::Rec2020, &mut px, 1)
            .unwrap();
        assert!(
            px.iter().zip(expect).all(|(a, b)| a.abs_diff(b) < 400),
            "{px:?} vs {expect:?}"
        );
        // The proof shows the same colour.
        let shown = proof_colors(&srgb(), ProofSettings::default(), &[enc16(blue)]).unwrap()[0];
        let want = mat(&SRGB_TO_2020, pipeline);
        assert!(
            close([shown[0], shown[1], shown[2]], want, 0.01),
            "{shown:?} vs {want:?}"
        );
        assert!(shown[3] > 1.0, "still flagged out of gamut");
    }

    #[test]
    fn linear_profiles_are_detected() {
        assert!(rgb_profile(0.0, 1.0).is_linear());
        assert!(!rgb_profile(0.0, 1.8).is_linear() && !srgb().is_linear());
    }

    #[test]
    fn bad_profiles_are_refused_with_a_reason() {
        let e = IccProfile::from_bytes(b"not a profile").unwrap_err();
        assert!(e.to_string().contains("not a readable ICC profile"), "{e}");
        let big = vec![0u8; MAX_PROFILE_BYTES + 1];
        assert!(
            IccProfile::from_bytes(&big)
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
        // Lab profiles aren't device profiles.
        let lab = lab_profile().unwrap().icc().unwrap();
        let e = IccProfile::from_bytes(&lab).unwrap_err();
        assert!(e.to_string().contains("unsupported colour space"), "{e}");
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            IccProfile::read(&dir.path().join("missing.icc")),
            Err(ExportError::Io { .. })
        ));
        let f = dir.path().join("srgb.icc");
        std::fs::write(&f, icc::profile(OutputSpace::Srgb)).unwrap();
        assert_eq!(IccProfile::read(&f).unwrap(), srgb());
        // Export needs RGB.
        let e = OutputProfile::new(raised_black_gray(), rel(false)).unwrap_err();
        assert!(e.to_string().contains("needs an RGB profile"), "{e}");
    }

    #[test]
    fn output_conversion_matches_the_profile() {
        let to_srgb = OutputProfile::new(srgb(), rel(false)).unwrap();
        assert_eq!(to_srgb.profile.bytes(), icc::profile(OutputSpace::Srgb));
        // sRGB → sRGB is the identity.
        let mut px: Vec<u16> = (0..300u16).map(|i| i * 218).collect();
        let orig = px.clone();
        to_srgb
            .convert_rgb16(OutputSpace::Srgb, &mut px, 10)
            .unwrap();
        assert!(
            px.iter().zip(&orig).all(|(a, b)| a.abs_diff(*b) <= 2),
            "{px:?}"
        );
        // P3 green → sRGB: clipped to sRGB green (relative colorimetric).
        let mut g = vec![0, 65_535, 0];
        to_srgb
            .convert_rgb16(OutputSpace::DisplayP3, &mut g, 1)
            .unwrap();
        assert!(g[0] < 300 && g[1] > 65_000 && g[2] < 300, "{g:?}");
        // sRGB → Adobe-like: neutral stays neutral; sRGB's green sits inside Adobe RGB's
        // wider green: (0, 1, 0) → (0.56, 1, 0.23).
        let adobe = OutputProfile::new(
            IccProfile::from_bytes(&icc::build_profile("Adobe-like", ADOBE_PRIMARIES)).unwrap(),
            ProofSettings::default(),
        )
        .unwrap();
        let mut c = vec![30_000, 30_000, 30_000, 0, 50_000, 0];
        adobe.convert_rgb16(OutputSpace::Srgb, &mut c, 2).unwrap();
        assert!(
            c[0].abs_diff(30_000) < 40 && c[0].abs_diff(c[2]) < 40,
            "{c:?}"
        );
        assert!(
            c[4].abs_diff(50_000) < 300 && c[3] > 20_000 && c[5] > 5_000,
            "{c:?}"
        );
        // Built-in spaces convert into each other; the same space is untouched.
        let mut p3 = vec![0, 65_535, 0];
        convert_space(OutputSpace::DisplayP3, OutputSpace::Rec2020, &mut p3, 1).unwrap();
        let expect = enc16(mat(&P3_TO_2020, [0.0, 1.0, 0.0]));
        assert!(
            p3.iter().zip(expect).all(|(a, b)| a.abs_diff(b) < 400),
            "{p3:?} vs {expect:?}"
        );
        let mut same = vec![1, 2, 3];
        convert_space(OutputSpace::Srgb, OutputSpace::Srgb, &mut same, 1).unwrap();
        assert_eq!(same, [1, 2, 3]);
    }

    #[test]
    fn transfer_functions_round_trip() {
        for i in 0..=100 {
            let v = i as f32 / 100.0;
            assert!((linear_to_srgb(srgb_to_linear(v)) - v).abs() < 1e-5);
        }
        assert!((srgb_to_linear(0.5) - 0.214_041).abs() < 1e-5);
    }

    /// An sRGB profile padded with `extra` zero bytes after its data (the size field
    /// grows to match): a valid profile of any length.
    fn padded_srgb(extra: usize) -> Vec<u8> {
        let mut b = icc::profile(OutputSpace::Srgb).to_vec();
        b.resize(b.len() + extra, 0);
        let n = b.len() as u32;
        b[..4].copy_from_slice(&n.to_be_bytes());
        b
    }

    /// A profile a JPEG can't carry (its ICC chunk count is one byte) can still be
    /// proofed against, but not exported to.
    #[test]
    fn an_output_profile_must_fit_in_a_file() {
        let fits = IccProfile::from_bytes(&padded_srgb(icc::MAX_EMBEDDED_BYTES - 4000)).unwrap();
        assert!(OutputProfile::new(fits, rel(false)).is_ok());
        let big = IccProfile::from_bytes(&padded_srgb(icc::MAX_EMBEDDED_BYTES + 1)).unwrap();
        let e = OutputProfile::new(big.clone(), rel(false)).unwrap_err();
        assert!(
            matches!(e, ExportError::Profile(_)) && e.to_string().contains("embed"),
            "{e}"
        );
        assert!(
            proof_colors(&big, rel(false), &[[0, 0, 0]]).is_ok(),
            "proofing is fine"
        );
    }

    /// A file whose length is not known up front (here an endless device) is cut off
    /// at the limit while reading, never loaded whole.
    #[cfg(unix)]
    #[test]
    fn reading_stops_at_the_limit() {
        let e = IccProfile::read(Path::new("/dev/zero")).unwrap_err();
        assert!(e.to_string().contains("limit"), "{e}");
    }
}
