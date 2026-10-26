//! Color science: camera → XYZ → linear Rec.2020 working space, white balance
//! (Planckian locus ↔ multipliers), output transforms, OKLab, gamut compression, ΔE2000.
//!
//! All matrices are row-major `[[f32; 3]; 3]` applied to column vectors.

use std::sync::OnceLock;

use lt_core::RawImage;
use lt_core::adjust::WhiteBalance;

use crate::OutputSpace;

pub type Mat3 = [[f32; 3]; 3];

/// CIE XYZ of D65 (Y = 1).
pub const D65_XYZ: [f32; 3] = [0.950_47, 1.0, 1.088_83];

pub const REC2020_TO_XYZ: Mat3 = [
    [0.636_958_0, 0.144_616_9, 0.168_881_0],
    [0.262_700_2, 0.677_998_1, 0.059_301_7],
    [0.0, 0.028_072_7, 1.060_985_1],
];
pub const SRGB_TO_XYZ: Mat3 = [
    [0.412_456_4, 0.357_576_1, 0.180_437_5],
    [0.212_672_9, 0.715_152_2, 0.072_175_0],
    [0.019_333_9, 0.119_192_0, 0.950_304_1],
];
pub const P3_TO_XYZ: Mat3 = [
    [0.486_570_9, 0.265_667_7, 0.198_217_3],
    [0.228_974_6, 0.691_738_5, 0.079_286_9],
    [0.0, 0.045_113_4, 1.043_944_4],
];

/// Luminance (Y) weights of linear Rec.2020.
pub const REC2020_LUMA: [f32; 3] = [0.262_700_2, 0.677_998_1, 0.059_301_7];

/// Temperature range accepted by [`temp_tint_to_multipliers`] (Kelvin).
pub const TEMP_MIN: f32 = 1667.0;
pub const TEMP_MAX: f32 = 50_000.0;
/// Tint magnitude kept on the multipliers → temperature/tint round trip.
pub const TINT_MAX: f32 = 1000.0;
/// Tint units: one tint step is 1/3000 of a CIE 1960 uv unit off the locus
/// (the DNG SDK convention). Positive = magenta.
pub const TINT_SCALE: f64 = 3000.0;

// ----------------------------------------------------------------- matrices ---

/// The 3×3 identity.
pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub fn mat_mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut o = [[0.0f32; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            o[i][j] = (0..3)
                .map(|k| f64::from(a[i][k]) * f64::from(b[k][j]))
                .sum::<f64>() as f32;
        }
    }
    o
}

#[inline]
pub fn mat_vec(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// Inverse of a 3×3 matrix (f64 internally). `None` if singular.
pub fn mat_inv(m: &Mat3) -> Option<Mat3> {
    let a: [[f64; 3]; 3] = m.map(|r| r.map(f64::from));
    let c00 = a[1][1] * a[2][2] - a[1][2] * a[2][1];
    let c01 = a[1][2] * a[2][0] - a[1][0] * a[2][2];
    let c02 = a[1][0] * a[2][1] - a[1][1] * a[2][0];
    let det = a[0][0] * c00 + a[0][1] * c01 + a[0][2] * c02;
    if !det.is_finite() || det.abs() < 1e-12 {
        return None;
    }
    let inv = [
        [
            c00,
            a[0][2] * a[2][1] - a[0][1] * a[2][2],
            a[0][1] * a[1][2] - a[0][2] * a[1][1],
        ],
        [
            c01,
            a[0][0] * a[2][2] - a[0][2] * a[2][0],
            a[0][2] * a[1][0] - a[0][0] * a[1][2],
        ],
        [
            c02,
            a[0][1] * a[2][0] - a[0][0] * a[2][1],
            a[0][0] * a[1][1] - a[0][1] * a[1][0],
        ],
    ];
    Some(inv.map(|r| r.map(|v| (v / det) as f32)))
}

pub fn diag(v: [f32; 3]) -> Mat3 {
    [[v[0], 0.0, 0.0], [0.0, v[1], 0.0], [0.0, 0.0, v[2]]]
}

struct Consts {
    xyz_to_rec2020: Mat3,
    rec2020_to_srgb: Mat3,
    rec2020_to_p3: Mat3,
    rec2020_to_lms: Mat3,
    lms_to_rec2020: Mat3,
    ok_anchor_hues: [f32; 7],
}

fn consts() -> &'static Consts {
    static C: OnceLock<Consts> = OnceLock::new();
    C.get_or_init(|| {
        let xyz_to_rec2020 = mat_inv(&REC2020_TO_XYZ).expect("rec2020 invertible");
        let rec2020_to_srgb = mat_mul(&mat_inv(&SRGB_TO_XYZ).expect("srgb"), &REC2020_TO_XYZ);
        let rec2020_to_p3 = mat_mul(&mat_inv(&P3_TO_XYZ).expect("p3"), &REC2020_TO_XYZ);
        let rec2020_to_lms = mat_mul(&OK_M1, &REC2020_TO_XYZ);
        let lms_to_rec2020 = mat_inv(&rec2020_to_lms).expect("lms");
        // UI hue anchors: HSV primaries/secondaries of the sRGB display, in OKLCh hue.
        let srgb_to_rec2020 = mat_mul(&xyz_to_rec2020, &SRGB_TO_XYZ);
        let prim = [
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 1.0, 1.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
        ];
        let mut hues = [0.0f32; 7];
        let mut prev = f32::NEG_INFINITY;
        for (i, p) in prim.iter().enumerate() {
            let lab = oklab_with(&rec2020_to_lms, mat_vec(&srgb_to_rec2020, *p));
            let mut h = lab[2].atan2(lab[1]).to_degrees();
            while h < prev {
                h += 360.0;
            }
            hues[i] = h;
            prev = h;
        }
        hues[6] = hues[0] + 360.0;
        Consts {
            xyz_to_rec2020,
            rec2020_to_srgb,
            rec2020_to_p3,
            rec2020_to_lms,
            lms_to_rec2020,
            ok_anchor_hues: hues,
        }
    })
}

pub fn xyz_to_rec2020() -> Mat3 {
    consts().xyz_to_rec2020
}

/// Linear Rec.2020 → linear output primaries.
pub fn working_to_output(space: OutputSpace) -> Mat3 {
    match space {
        OutputSpace::Srgb => consts().rec2020_to_srgb,
        OutputSpace::DisplayP3 => consts().rec2020_to_p3,
        OutputSpace::Rec2020 => IDENTITY,
    }
}

/// White-balanced camera RGB → linear Rec.2020.
///
/// Normalized (as dcraw does) so that a white-balanced neutral `(1,1,1)` maps to
/// Rec.2020 white `(1,1,1)`: `M = XYZ→2020 · cam→XYZ · diag(camera response to D65)`.
pub fn camera_to_working(cam_to_xyz: &Mat3) -> Mat3 {
    // With lt-raw's D65 row-normalized matrices `white` is (1, 1, 1) and this is just
    // XYZ→2020 · cam→XYZ; older/other matrices still get neutral-preserving scaling.
    let Some(xyz_to_cam) = mat_inv(cam_to_xyz) else {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    };
    let white = mat_vec(&xyz_to_cam, D65_XYZ);
    if white.iter().any(|v| !v.is_finite() || *v <= 1e-6) {
        return [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    }
    mat_mul(&mat_mul(&xyz_to_rec2020(), cam_to_xyz), &diag(white))
}

// ------------------------------------------------------ transfer functions ---

/// sRGB / Display P3 OETF (linear → encoded). Odd-extended for negatives.
#[inline]
pub fn srgb_encode(x: f32) -> f32 {
    let a = x.abs();
    let e = if a <= 0.003_130_8 {
        a * 12.92
    } else {
        1.055 * a.powf(1.0 / 2.4) - 0.055
    };
    e.copysign(x)
}

/// sRGB / Display P3 EOTF (encoded → linear). Odd-extended for negatives.
#[inline]
pub fn srgb_decode(y: f32) -> f32 {
    let a = y.abs();
    let l = if a <= 0.040_45 {
        a / 12.92
    } else {
        ((a + 0.055) / 1.055).powf(2.4)
    };
    l.copysign(y)
}

// ------------------------------------------------------------------- OKLab ---

const OK_M1: Mat3 = [
    [0.818_933_010_1, 0.361_866_742_4, -0.128_859_713_7],
    [0.032_984_543_6, 0.929_311_871_5, 0.036_145_638_7],
    [0.048_200_301_8, 0.264_366_269_1, 0.633_851_707_0],
];
const OK_M2: Mat3 = [
    [0.210_454_255_3, 0.793_617_785_0, -0.004_072_046_8],
    [1.977_998_495_1, -2.428_592_205_0, 0.450_593_709_9],
    [0.025_904_037_1, 0.782_771_766_2, -0.808_675_766_0],
];
const OK_M2_INV: Mat3 = [
    [1.0, 0.396_337_777_4, 0.215_803_757_3],
    [1.0, -0.105_561_345_8, -0.063_854_172_8],
    [1.0, -0.089_484_177_5, -1.291_485_548_0],
];

fn oklab_with(to_lms: &Mat3, rgb: [f32; 3]) -> [f32; 3] {
    let lms = mat_vec(to_lms, rgb).map(f32::cbrt);
    mat_vec(&OK_M2, lms)
}

/// Linear Rec.2020 → OKLab.
#[inline]
pub fn rec2020_to_oklab(rgb: [f32; 3]) -> [f32; 3] {
    oklab_with(&consts().rec2020_to_lms, rgb)
}

/// OKLab → linear Rec.2020.
#[inline]
pub fn oklab_to_rec2020(lab: [f32; 3]) -> [f32; 3] {
    let lms = mat_vec(&OK_M2_INV, lab).map(|v| v * v * v);
    mat_vec(&consts().lms_to_rec2020, lms)
}

/// Matrices the GPU needs for OKLab (Rec.2020 → LMS, LMS → Rec.2020).
pub(crate) fn oklab_matrices() -> (Mat3, Mat3, Mat3, Mat3) {
    let c = consts();
    (c.rec2020_to_lms, OK_M2, OK_M2_INV, c.lms_to_rec2020)
}

/// Map a UI hue (HSV degrees of the sRGB display: 0 red, 60 yellow, 120 green, …) to an
/// OKLCh hue angle in degrees (0..360), piecewise-linearly between the six anchors.
pub fn ui_hue_to_ok(h: f32) -> f32 {
    let a = &consts().ok_anchor_hues;
    let h = h.rem_euclid(360.0);
    let seg = ((h / 60.0) as usize).min(5);
    let t = (h - seg as f32 * 60.0) / 60.0;
    (a[seg] + t * (a[seg + 1] - a[seg])).rem_euclid(360.0)
}

/// Center and half-width (OK degrees) of a UI hue range `[center ± width/2]`.
pub fn ui_range_to_ok(center: f32, width: f32) -> (f32, f32) {
    let half = (width.abs() * 0.5).min(180.0);
    let lo = ui_hue_to_ok(center - half);
    let hi = ui_hue_to_ok(center + half);
    let span = if half >= 180.0 {
        360.0
    } else {
        (hi - lo).rem_euclid(360.0)
    };
    ((lo + span * 0.5).rem_euclid(360.0), span * 0.5)
}

// ------------------------------------------------------- gamut compression ---

#[cfg(test)]
use lt_core::gamut::LIM as GAMUT_LIM;
pub use lt_core::gamut::compress as gamut_compress;
pub(crate) use lt_core::gamut::{S as GAMUT_S, THR as GAMUT_THR};

// ------------------------------------------------------------ white balance ---

/// CIE 1960 (u, v) of the Planckian radiator at `t` Kelvin (Krystek 1985).
fn planck_uv(t: f64) -> (f64, f64) {
    let u = (0.860_117_757 + 1.541_182_54e-4 * t + 1.286_412_12e-7 * t * t)
        / (1.0 + 8.424_202_35e-4 * t + 7.081_451_63e-7 * t * t);
    let v = (0.317_398_726 + 4.228_062_45e-5 * t + 4.204_816_91e-8 * t * t)
        / (1.0 - 2.897_418_16e-5 * t + 1.614_560_53e-7 * t * t);
    (u, v)
}

/// Unit normal to the locus at `t` pointing toward green (+v side).
fn locus_normal(t: f64) -> (f64, f64) {
    let h = t * 1e-4;
    let (u0, v0) = planck_uv(t - h);
    let (u1, v1) = planck_uv(t + h);
    let (du, dv) = (u1 - u0, v1 - v0);
    let len = du.hypot(dv).max(1e-12);
    // Perpendicular; pick the one with positive v component.
    let (nu, nv) = (-dv / len, du / len);
    if nv >= 0.0 { (nu, nv) } else { (-nu, -nv) }
}

fn uv_to_xy(u: f64, v: f64) -> (f64, f64) {
    let d = 2.0 * u - 8.0 * v + 4.0;
    (3.0 * u / d, 2.0 * v / d)
}

fn xy_to_uv(x: f64, y: f64) -> (f64, f64) {
    let d = -2.0 * x + 12.0 * y + 3.0;
    (4.0 * x / d, 6.0 * y / d)
}

/// CIE 1931 xy chromaticity of the white point `temp_k` / `tint`.
pub fn temp_tint_to_xy(temp_k: f32, tint: f32) -> [f64; 2] {
    let t = f64::from(temp_k.clamp(TEMP_MIN, TEMP_MAX));
    let (u, v) = planck_uv(t);
    let (nu, nv) = locus_normal(t);
    let off = -f64::from(tint) / TINT_SCALE; // positive tint = magenta = away from green
    let (x, y) = uv_to_xy(u + nu * off, v + nv * off);
    [x, y]
}

/// Nearest Planckian temperature and tint for an xy chromaticity.
pub fn xy_to_temp_tint(xy: [f64; 2]) -> (f32, f32) {
    let (u, v) = xy_to_uv(xy[0], xy[1]);
    let dist = |t: f64| {
        let (lu, lv) = planck_uv(t);
        (u - lu).powi(2) + (v - lv).powi(2)
    };
    // Coarse scan in mired, then golden-section refine.
    let (m_lo, m_hi) = (1e6 / f64::from(TEMP_MAX), 1e6 / f64::from(TEMP_MIN));
    let steps = 200;
    let mut best = (f64::INFINITY, m_lo);
    for i in 0..=steps {
        let m = m_lo + (m_hi - m_lo) * f64::from(i) / f64::from(steps);
        let d = dist(1e6 / m);
        if d < best.0 {
            best = (d, m);
        }
    }
    let step = (m_hi - m_lo) / f64::from(steps);
    let (mut a, mut b) = ((best.1 - step).max(m_lo), (best.1 + step).min(m_hi));
    let g = 0.618_033_988_75;
    for _ in 0..80 {
        let c = b - g * (b - a);
        let d = a + g * (b - a);
        if dist(1e6 / c) < dist(1e6 / d) {
            b = d;
        } else {
            a = c;
        }
    }
    let t = 1e6 / f64::midpoint(a, b);
    let (lu, lv) = planck_uv(t);
    let (nu, nv) = locus_normal(t);
    let off = (u - lu) * nu + (v - lv) * nv;
    (t as f32, (-off * TINT_SCALE) as f32)
}

/// The camera's color description needed for white balance: `cam_to_xyz` for
/// white-balanced values (D65 row-normalized, see `lt_core::RawImage`) and the
/// multipliers that neutralize D65 daylight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraColor {
    pub cam_to_xyz: Mat3,
    pub daylight: [f32; 3],
}

impl CameraColor {
    pub fn of(raw: &RawImage) -> Self {
        Self {
            cam_to_xyz: raw.cam_to_xyz,
            daylight: normalize_mults(raw.wb_daylight),
        }
    }
}

/// Camera-space multipliers (G = 1) that neutralize an illuminant of `temp_k`/`tint`:
/// `normalize(daylight ⊘ (N · xyz(T)))` with `N = cam_to_xyz⁻¹`. At D65 this is exactly
/// `daylight`.
pub fn temp_tint_to_multipliers(temp_k: f32, tint: f32, cam: &CameraColor) -> [f32; 3] {
    let [x, y] = temp_tint_to_xy(temp_k, tint);
    let xyz = [(x / y) as f32, 1.0, ((1.0 - x - y) / y) as f32];
    let Some(n) = mat_inv(&cam.cam_to_xyz) else {
        return [1.0; 3];
    };
    let r = mat_vec(&n, xyz);
    normalize_mults([0, 1, 2].map(|c| cam.daylight[c] / r[c]))
}

/// Normalize multipliers so G = 1; non-finite or non-positive values fall back to 1.
pub fn normalize_mults(m: [f32; 3]) -> [f32; 3] {
    if m.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return [1.0; 3];
    }
    [m[0] / m[1], 1.0, m[2] / m[1]]
}

/// Multipliers for a white balance setting on this image.
pub fn wb_to_multipliers(wb: WhiteBalance, raw: &RawImage) -> [f32; 3] {
    match wb {
        WhiteBalance::AsShot => normalize_mults(raw.wb_as_shot),
        WhiteBalance::Custom { temp_k, tint } => {
            temp_tint_to_multipliers(temp_k, tint, &CameraColor::of(raw))
        }
    }
}

/// Inverse of [`temp_tint_to_multipliers`]: the `Custom` WB that yields `mults`
/// (`xyz = cam_to_xyz · (daylight ⊘ mults)` is the illuminant's white).
pub fn multipliers_to_wb(mults: [f32; 3], cam: &CameraColor) -> WhiteBalance {
    let m = normalize_mults(mults);
    let v = [0, 1, 2].map(|c| cam.daylight[c] / m[c]);
    let xyz = mat_vec(&cam.cam_to_xyz, v);
    let s = f64::from(xyz[0] + xyz[1] + xyz[2]);
    if !s.is_finite() || s.abs() < 1e-12 {
        return WhiteBalance::Custom {
            temp_k: 6500.0,
            tint: 0.0,
        };
    }
    let (temp_k, tint) = xy_to_temp_tint([f64::from(xyz[0]) / s, f64::from(xyz[1]) / s]);
    WhiteBalance::Custom {
        temp_k,
        tint: tint.clamp(-TINT_MAX, TINT_MAX),
    }
}

/// Current temperature/tint for any WB setting (as-shot is converted through the matrix).
pub fn wb_temp_tint(wb: WhiteBalance, raw: &RawImage) -> (f32, f32) {
    match wb {
        WhiteBalance::Custom { temp_k, tint } => (temp_k, tint),
        WhiteBalance::AsShot => match multipliers_to_wb(raw.wb_as_shot, &CameraColor::of(raw)) {
            WhiteBalance::Custom { temp_k, tint } => (temp_k, tint),
            WhiteBalance::AsShot => (6500.0, 0.0),
        },
    }
}

// ------------------------------------------------------------------ ΔE 2000 ---

/// CIE L*a*b* (D65) of an 8-bit sRGB pixel.
pub fn srgb8_to_lab(rgb: [u8; 3]) -> [f32; 3] {
    let lin = rgb.map(|c| srgb_decode(f32::from(c) / 255.0));
    let xyz = mat_vec(&SRGB_TO_XYZ, lin);
    let f = |t: f32| {
        if t > 0.008_856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(xyz[0] / D65_XYZ[0]), f(xyz[1]), f(xyz[2] / D65_XYZ[2]));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// CIEDE2000 color difference.
pub fn delta_e2000(lab1: [f32; 3], lab2: [f32; 3]) -> f32 {
    let (l1, a1, b1) = (f64::from(lab1[0]), f64::from(lab1[1]), f64::from(lab1[2]));
    let (l2, a2, b2) = (f64::from(lab2[0]), f64::from(lab2[1]), f64::from(lab2[2]));
    let c1 = a1.hypot(b1);
    let c2 = a2.hypot(b2);
    let cm = f64::midpoint(c1, c2);
    let g = 0.5 * (1.0 - (cm.powi(7) / (cm.powi(7) + 25f64.powi(7))).sqrt());
    let (a1p, a2p) = ((1.0 + g) * a1, (1.0 + g) * a2);
    let (c1p, c2p) = (a1p.hypot(b1), a2p.hypot(b2));
    let hp = |b: f64, a: f64| {
        if a == 0.0 && b == 0.0 {
            0.0
        } else {
            b.atan2(a).to_degrees().rem_euclid(360.0)
        }
    };
    let (h1p, h2p) = (hp(b1, a1p), hp(b2, a2p));
    let dl = l2 - l1;
    let dc = c2p - c1p;
    let dh = if c1p * c2p == 0.0 {
        0.0
    } else if (h2p - h1p).abs() <= 180.0 {
        h2p - h1p
    } else if h2p - h1p > 180.0 {
        h2p - h1p - 360.0
    } else {
        h2p - h1p + 360.0
    };
    let dhh = 2.0 * (c1p * c2p).sqrt() * (dh.to_radians() * 0.5).sin();
    let lm = f64::midpoint(l1, l2);
    let cmp = f64::midpoint(c1p, c2p);
    let hm = if c1p * c2p == 0.0 {
        h1p + h2p
    } else if (h1p - h2p).abs() <= 180.0 {
        f64::midpoint(h1p, h2p)
    } else if h1p + h2p < 360.0 {
        0.5 * (h1p + h2p + 360.0)
    } else {
        0.5 * (h1p + h2p - 360.0)
    };
    let t = 1.0 - 0.17 * (hm - 30.0).to_radians().cos()
        + 0.24 * (2.0 * hm).to_radians().cos()
        + 0.32 * (3.0 * hm + 6.0).to_radians().cos()
        - 0.20 * (4.0 * hm - 63.0).to_radians().cos();
    let dtheta = 30.0 * (-((hm - 275.0) / 25.0).powi(2)).exp();
    let rc = 2.0 * (cmp.powi(7) / (cmp.powi(7) + 25f64.powi(7))).sqrt();
    let sl = 1.0 + 0.015 * (lm - 50.0).powi(2) / (20.0 + (lm - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * cmp;
    let sh = 1.0 + 0.015 * cmp * t;
    let rt = -(2.0 * dtheta.to_radians()).sin() * rc;
    let v =
        (dl / sl).powi(2) + (dc / sc).powi(2) + (dhh / sh).powi(2) + rt * (dc / sc) * (dhh / sh);
    v.max(0.0).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Adobe `ColorMatrix2` (XYZ → camera, D65) for the Sony ILCE-9.
    const A9_XYZ_TO_CAM: [f32; 9] = [
        0.6389, -0.1703, -0.0378, -0.4562, 1.2265, 0.2587, -0.067, 0.1489, 0.655,
    ];

    /// The A9 as lt-raw describes it (row-normalized matrix + daylight multipliers).
    fn a9() -> CameraColor {
        CameraColor {
            cam_to_xyz: lt_raw::color::cam_to_xyz(&A9_XYZ_TO_CAM).unwrap(),
            daylight: lt_raw::color::daylight_multipliers(&A9_XYZ_TO_CAM).unwrap(),
        }
    }

    /// Raw (un-balanced) camera response to a white of chromaticity `xy`, from the
    /// unnormalized Adobe matrix — independent of the functions under test.
    fn camera_response(xy: [f64; 2]) -> [f32; 3] {
        let xyz = [
            (xy[0] / xy[1]) as f32,
            1.0,
            ((1.0 - xy[0] - xy[1]) / xy[1]) as f32,
        ];
        let m = A9_XYZ_TO_CAM;
        [0, 1, 2].map(|i| m[i * 3] * xyz[0] + m[i * 3 + 1] * xyz[1] + m[i * 3 + 2] * xyz[2])
    }

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn matrix_inverse_roundtrip_and_singular() {
        let inv = mat_inv(&REC2020_TO_XYZ).unwrap();
        let id = mat_mul(&REC2020_TO_XYZ, &inv);
        for i in 0..3 {
            for j in 0..3 {
                assert!(close(id[i][j], if i == j { 1.0 } else { 0.0 }, 1e-5));
            }
        }
        assert!(mat_inv(&[[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [0.0, 0.0, 1.0]]).is_none());
        assert_eq!(diag([1.0, 2.0, 3.0])[1][1], 2.0);
    }

    #[test]
    fn working_white_maps_to_output_white() {
        for space in [
            OutputSpace::Srgb,
            OutputSpace::DisplayP3,
            OutputSpace::Rec2020,
        ] {
            let w = mat_vec(&working_to_output(space), [1.0, 1.0, 1.0]);
            for c in w {
                assert!(close(c, 1.0, 1e-3), "{space:?} {w:?}");
            }
        }
    }

    #[test]
    fn camera_matrix_maps_balanced_neutral_to_white() {
        let m = camera_to_working(&a9().cam_to_xyz);
        let w = mat_vec(&m, [1.0, 1.0, 1.0]);
        for c in w {
            assert!(close(c, 1.0, 1e-3), "{w:?}");
        }
        // Degenerate matrices fall back to identity instead of producing NaN.
        let id = camera_to_working(&[[0.0; 3]; 3]);
        assert_eq!(id[0][0], 1.0);
        let neg = camera_to_working(&[[-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
        assert_eq!(neg[0][0], 1.0);
    }

    #[test]
    fn transfer_functions_roundtrip() {
        for i in 0..=100 {
            let x = i as f32 / 100.0;
            assert!(close(srgb_decode(srgb_encode(x)), x, 1e-5));
            assert!(close(srgb_encode(-x), -srgb_encode(x), 1e-7));
        }
        assert!(close(srgb_encode(0.18), 0.4613, 1e-3));
        assert!(close(srgb_decode(0.5), 0.2140, 1e-3));
    }

    #[test]
    fn oklab_roundtrip_and_white() {
        let lab = rec2020_to_oklab([1.0, 1.0, 1.0]);
        assert!(
            close(lab[0], 1.0, 1e-3) && lab[1].abs() < 1e-3 && lab[2].abs() < 1e-3,
            "{lab:?}"
        );
        for rgb in [[0.2, 0.5, 0.9], [0.9, 0.1, 0.05], [0.01, 0.02, 0.0]] {
            let back = oklab_to_rec2020(rec2020_to_oklab(rgb));
            for c in 0..3 {
                assert!(close(back[c], rgb[c], 1e-4), "{rgb:?} {back:?}");
            }
        }
        let (a, b, c, d) = oklab_matrices();
        assert!(a[0][0] > 0.0 && b[0][0] > 0.0 && c[0][0] == 1.0 && d[0][0] > 0.0);
    }

    #[test]
    fn ui_hue_mapping_is_monotone_and_anchored() {
        let red = ui_hue_to_ok(0.0);
        assert!((20.0..40.0).contains(&red), "{red}");
        let mut prev = red;
        let mut unwrap = 0.0;
        for i in 1..360 {
            let mut h = ui_hue_to_ok(i as f32) + unwrap;
            if h < prev - 180.0 {
                unwrap += 360.0;
                h += 360.0;
            }
            assert!(h >= prev, "not monotone at {i}");
            prev = h;
        }
        let (c, hw) = ui_range_to_ok(0.0, 45.0);
        assert!(hw > 5.0 && hw < 60.0 && (0.0..360.0).contains(&c));
        let (_, all) = ui_range_to_ok(0.0, 400.0);
        assert_eq!(all, 180.0);
    }

    #[test]
    fn gamut_compress_is_soft_and_bounded() {
        // In gamut: untouched.
        assert_eq!(gamut_compress([0.5, 0.4, 0.3]), [0.5, 0.4, 0.3]);
        // Out of gamut: non-negative, and monotone in the input distance (no banding).
        let mut prev = f32::INFINITY;
        for i in 0..100 {
            let b = 0.2 - i as f32 * 0.01;
            let o = gamut_compress([1.0, 0.5, b]);
            assert!(o.iter().all(|v| *v >= 0.0));
            assert!(o[2] <= prev + 1e-6, "not monotone at {b}");
            prev = o[2];
        }
        // Continuity at the threshold.
        let below = gamut_compress([1.0, 0.5, 1.0 - GAMUT_THR + 1e-4]);
        let above = gamut_compress([1.0, 0.5, 1.0 - GAMUT_THR - 1e-4]);
        assert!(close(below[2], above[2], 1e-3));
        // Black / negative achromatic doesn't produce NaN.
        assert_eq!(gamut_compress([0.0, 0.0, 0.0]), [0.0; 3]);
        assert_eq!(gamut_compress([-1.0, -2.0, -0.5]), [0.0; 3]);
        // The limit distance lands on the boundary.
        let edge = gamut_compress([1.0, 0.5, 1.0 - GAMUT_LIM]);
        assert!(edge[2].abs() < 1e-5, "{edge:?}");
    }

    #[test]
    fn planckian_locus_known_points() {
        // D65 is ~6504 K, slightly above the locus (green side).
        let (t, tint) = xy_to_temp_tint([0.312_71, 0.329_02]);
        assert!((6400.0..6600.0).contains(&t), "{t}");
        assert!(tint.abs() < 15.0, "{tint}");
        // Illuminant A (2856 K) sits on the locus.
        let (t, tint) = xy_to_temp_tint([0.447_57, 0.407_45]);
        assert!((2800.0..2900.0).contains(&t), "{t}");
        assert!(tint.abs() < 3.0, "{tint}");
        // Positive tint moves toward magenta (lower y).
        let n = temp_tint_to_xy(5000.0, 0.0);
        let m = temp_tint_to_xy(5000.0, 50.0);
        assert!(m[1] < n[1]);
    }

    #[test]
    fn wb_roundtrip_within_one_percent() {
        for &t in &[
            2500.0f32, 3200.0, 4300.0, 5000.0, 5600.0, 6500.0, 7500.0, 9000.0, 12000.0,
        ] {
            for &tint in &[-40.0f32, 0.0, 25.0] {
                let m = temp_tint_to_multipliers(t, tint, &a9());
                let WhiteBalance::Custom { temp_k, tint: tt } = multipliers_to_wb(m, &a9()) else {
                    panic!()
                };
                assert!((temp_k - t).abs() / t < 0.01, "{t} → {temp_k}");
                assert!((tt - tint).abs() < 1.0, "{tint} → {tt}");
            }
        }
    }

    #[test]
    fn daylight_is_d65_and_as_shot_roundtrips() {
        let cam = a9();
        // D65 (≈6504 K, small positive-v tint) gives the daylight multipliers.
        let (t, tint) = xy_to_temp_tint([0.312_71, 0.329_02]);
        let m = temp_tint_to_multipliers(t, tint, &cam);
        for c in 0..3 {
            assert!(
                (m[c] - cam.daylight[c]).abs() / cam.daylight[c] < 0.005,
                "{m:?} vs {:?}",
                cam.daylight
            );
        }
        // A real A9 as-shot setting (SKB06204) → K/tint → back to the same multipliers.
        for as_shot in [
            [2.507_812_5f32, 1.0, 1.582_031_3],
            [2.375, 1.0, 1.632_812_5],
            [2.578_125, 1.0, 1.472_656_3],
        ] {
            let WhiteBalance::Custom { temp_k, tint } = multipliers_to_wb(as_shot, &cam) else {
                panic!()
            };
            assert!(
                (3500.0..7500.0).contains(&temp_k) && tint.abs() < 60.0,
                "{temp_k} {tint}"
            );
            let back = temp_tint_to_multipliers(temp_k, tint, &cam);
            for c in 0..3 {
                assert!(
                    (back[c] - as_shot[c]).abs() / as_shot[c] < 0.005,
                    "{as_shot:?} → {back:?}"
                );
            }
        }
    }

    #[test]
    fn gray_card_under_tungsten_reads_3000k() {
        let cam = a9();
        for (t, want) in [(3000.0f32, 3000.0f32), (4500.0, 4500.0), (6500.0, 6500.0)] {
            let xy = temp_tint_to_xy(t, 0.0);
            let r = camera_response(xy);
            let WhiteBalance::Custom { temp_k, tint } =
                multipliers_to_wb([1.0 / r[0], 1.0 / r[1], 1.0 / r[2]], &cam)
            else {
                panic!()
            };
            assert!(
                (temp_k - want).abs() <= 100.0 * want / 3000.0,
                "{t}: {temp_k}"
            );
            assert!(tint.abs() < 2.0, "tint {tint}");
        }
    }

    #[test]
    fn warmer_light_needs_more_blue_gain() {
        let warm = temp_tint_to_multipliers(3000.0, 0.0, &a9());
        let cool = temp_tint_to_multipliers(8000.0, 0.0, &a9());
        assert!(warm[2] > cool[2] && warm[0] < cool[0], "{warm:?} {cool:?}");
        assert_eq!(
            temp_tint_to_multipliers(
                5000.0,
                0.0,
                &CameraColor {
                    cam_to_xyz: [[0.0; 3]; 3],
                    daylight: [1.0; 3]
                }
            ),
            [1.0; 3]
        );
    }

    #[test]
    fn multiplier_helpers() {
        assert_eq!(normalize_mults([2.0, 2.0, 3.0]), [1.0, 1.0, 1.5]);
        assert_eq!(normalize_mults([f32::NAN, 1.0, 1.0]), [1.0; 3]);
        assert_eq!(normalize_mults([0.0, 1.0, 1.0]), [1.0; 3]);
        assert_eq!(
            multipliers_to_wb(
                [1.0, 1.0, 1.0],
                &CameraColor {
                    cam_to_xyz: [[0.0; 3]; 3],
                    daylight: [1.0; 3]
                }
            ),
            WhiteBalance::Custom {
                temp_k: 6500.0,
                tint: 0.0
            }
        );
        let raw = RawImage {
            size: lt_core::Size::new(2, 2),
            cfa: lt_core::CfaPattern::Rggb,
            data: vec![0; 4],
            black_level: [0.0; 4],
            white_level: 1.0,
            wb_as_shot: [2.0, 1.0, 1.5],
            cam_to_xyz: a9().cam_to_xyz,
            wb_daylight: a9().daylight,
            orientation: lt_core::Orientation::Normal,
            default_crop: None,
        };
        assert_eq!(
            wb_to_multipliers(WhiteBalance::AsShot, &raw),
            [2.0, 1.0, 1.5]
        );
        let c = wb_to_multipliers(
            WhiteBalance::Custom {
                temp_k: 5000.0,
                tint: 0.0,
            },
            &raw,
        );
        assert!(c[1] == 1.0 && c[0] > 1.0);
        let (t, _) = wb_temp_tint(WhiteBalance::AsShot, &raw);
        assert!((2000.0..12000.0).contains(&t));
        assert_eq!(
            wb_temp_tint(
                WhiteBalance::Custom {
                    temp_k: 4000.0,
                    tint: 3.0
                },
                &raw
            ),
            (4000.0, 3.0)
        );
    }

    #[test]
    fn delta_e2000_reference_pairs() {
        // Sharma et al. (2005) test data, pairs 1, 7 and 17.
        let cases = [
            ([50.0, 2.6772, -79.7751], [50.0, 0.0, -82.7485], 2.0425),
            ([50.0, 0.0, 0.0], [50.0, -1.0, 2.0], 2.3669),
            ([50.0, 2.5, 0.0], [73.0, 25.0, -18.0], 27.1492),
        ];
        for (a, b, want) in cases {
            let got = delta_e2000(a, b);
            assert!(close(got, want, 1e-3), "{got} vs {want}");
        }
        assert_eq!(delta_e2000([50.0, 0.0, 0.0], [50.0, 0.0, 0.0]), 0.0);
        let white = srgb8_to_lab([255, 255, 255]);
        assert!(close(white[0], 100.0, 0.01) && white[1].abs() < 0.01);
        let black = srgb8_to_lab([0, 0, 0]);
        assert!(black[0].abs() < 1e-3);
    }
}
