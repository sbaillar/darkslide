//! Crop, straighten, quarter turns, flips and EXIF orientation, folded into affine maps
//! from output pixel centers back to sensor / normalized coordinates, plus the bilinear
//! resampler that pulls the BASE image onto the output grid.

use ds_core::Adjustments;
use ds_core::adjust::{Crop, LensCorrection};
use ds_core::{NormRect, Orientation, RawImage, Size};
use rayon::prelude::*;

use crate::PipelineError;
use crate::demosaic::BaseImage;
use crate::lens::{self, LensModel};

/// 2×3 affine map: `x' = m[0][0]·x + m[0][1]·y + m[0][2]`, `y' = m[1][0]·x + m[1][1]·y + m[1][2]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine(pub [[f64; 3]; 2]);

impl Affine {
    pub const IDENTITY: Affine = Affine([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);

    pub fn translate(tx: f64, ty: f64) -> Self {
        Self([[1.0, 0.0, tx], [0.0, 1.0, ty]])
    }

    pub fn scale(sx: f64, sy: f64) -> Self {
        Self([[sx, 0.0, 0.0], [0.0, sy, 0.0]])
    }

    /// Apply `self` first, then `next`.
    #[must_use]
    pub fn then(&self, next: &Affine) -> Affine {
        let (a, b) = (&self.0, &next.0);
        Affine([
            [
                b[0][0] * a[0][0] + b[0][1] * a[1][0],
                b[0][0] * a[0][1] + b[0][1] * a[1][1],
                b[0][0] * a[0][2] + b[0][1] * a[1][2] + b[0][2],
            ],
            [
                b[1][0] * a[0][0] + b[1][1] * a[1][0],
                b[1][0] * a[0][1] + b[1][1] * a[1][1],
                b[1][0] * a[0][2] + b[1][1] * a[1][2] + b[1][2],
            ],
        ])
    }

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let m = &self.0;
        (
            m[0][0] * x + m[0][1] * y + m[0][2],
            m[1][0] * x + m[1][1] * y + m[1][2],
        )
    }

    pub fn inverse(&self) -> Option<Affine> {
        let m = &self.0;
        let det = m[0][0] * m[1][1] - m[0][1] * m[1][0];
        if det.abs() < 1e-18 {
            return None;
        }
        let (a, b, c, d) = (m[1][1] / det, -m[0][1] / det, -m[1][0] / det, m[0][0] / det);
        Some(Affine([
            [a, b, -(a * m[0][2] + b * m[1][2])],
            [c, d, -(c * m[0][2] + d * m[1][2])],
        ]))
    }

    /// Row-major f32 `[m00, m01, m02, m10, m11, m12]` for shaders and hot loops.
    pub fn to_f32(self) -> [f32; 6] {
        let m = &self.0;
        [
            m[0][0] as f32,
            m[0][1] as f32,
            m[0][2] as f32,
            m[1][0] as f32,
            m[1][1] as f32,
            m[1][2] as f32,
        ]
    }
}

/// Map from E-frame (displayed, EXIF-oriented) continuous coordinates to stored
/// (sensor-area) coordinates. `sw`/`sh` are the stored dimensions.
pub fn orientation_to_stored(o: Orientation, sw: f64, sh: f64) -> Affine {
    // Rows: sx = a·ex + b·ey + c, sy = d·ex + e·ey + f.
    match o {
        Orientation::Normal => Affine::IDENTITY,
        Orientation::FlipH => Affine([[-1.0, 0.0, sw], [0.0, 1.0, 0.0]]),
        Orientation::Rotate180 => Affine([[-1.0, 0.0, sw], [0.0, -1.0, sh]]),
        Orientation::FlipV => Affine([[1.0, 0.0, 0.0], [0.0, -1.0, sh]]),
        Orientation::Transpose => Affine([[0.0, 1.0, 0.0], [1.0, 0.0, 0.0]]),
        Orientation::Rotate90 => Affine([[0.0, 1.0, 0.0], [-1.0, 0.0, sh]]),
        Orientation::Transverse => Affine([[0.0, -1.0, sw], [-1.0, 0.0, sh]]),
        Orientation::Rotate270 => Affine([[0.0, -1.0, sw], [1.0, 0.0, 0.0]]),
    }
}

/// Lens corrections placed in the E frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LensGeo {
    pub model: LensModel,
    /// Optical center (E-frame pixels).
    pub center: [f64; 2],
    /// 1 / (half the short side): lensfun's distortion/TCA radius unit.
    pub r_dist: f64,
    /// 1 / (half diagonal): lensfun's vignetting radius unit.
    pub r_vig: f64,
}

/// Resolved output geometry for one render.
///
/// Output pixel → `to_e` (crop, straighten, turns, flips) → corrected E frame →
/// keystone homography → lens distortion/TCA (per channel) → `e_to_sensor` (EXIF
/// orientation, default crop) → sensor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub out: Size,
    /// Output pixels per sensor pixel (mean of x/y).
    pub scale: f32,
    /// Output pixel center → corrected E-frame pixels.
    pub to_e: Affine,
    /// Corrected E px → pre-keystone E px (row-major 3×3, projective).
    pub keystone: Option<[f64; 9]>,
    pub lens: Option<LensGeo>,
    /// E-frame pixels → sensor pixels.
    pub e_to_sensor: Affine,
    /// Output pixel center → E-frame normalized (u, v) (masks, crop space).
    pub to_norm: Affine,
    /// Output pixel center → final-frame coordinates centered and divided by the half
    /// diagonal (corners at radius 1) — for the creative vignette.
    pub to_vignette: Affine,
    /// E-frame size in sensor pixels.
    pub e_size: [f64; 2],
    /// Sensor size.
    pub sensor: [f64; 2],
    /// Where output pixel (0, 0) sits in the final frame at output scale, and that
    /// frame's size at output scale (a region render is a window into it).
    pub origin_out: [f64; 2],
    pub frame_out: [f64; 2],
}

/// Apply a row-major homography.
#[inline]
pub fn homography(h: &[f64; 9], x: f64, y: f64) -> (f64, f64) {
    let w = h[6] * x + h[7] * y + h[8];
    let w = if w.abs() < 1e-12 { 1e-12 } else { w };
    (
        (h[0] * x + h[1] * y + h[2]) / w,
        (h[3] * x + h[4] * y + h[5]) / w,
    )
}

impl Geometry {
    /// Multiplier from E-normalized to isotropic long-edge units: `(W/L, H/L)`.
    pub fn iso(&self) -> [f32; 2] {
        let l = self.e_size[0].max(self.e_size[1]);
        [(self.e_size[0] / l) as f32, (self.e_size[1] / l) as f32]
    }

    /// E-frame long edge in output pixels.
    pub fn long_edge_out(&self) -> f32 {
        self.e_size[0].max(self.e_size[1]) as f32 * self.scale
    }

    /// E-normalized coordinates of an output position.
    #[inline]
    pub fn norm_at(&self, x: f64, y: f64) -> (f64, f64) {
        self.to_norm.apply(x, y)
    }

    /// Pre-lens (keystoned) E position of an output position.
    #[inline]
    fn optical_e(&self, x: f64, y: f64) -> (f64, f64) {
        let (ex, ey) = self.to_e.apply(x, y);
        match &self.keystone {
            Some(h) => homography(h, ex, ey),
            None => (ex, ey),
        }
    }

    /// Sensor position of a corrected-E-frame position (E pixels), as the green channel
    /// samples it (no lateral CA): keystone, lens distortion, orientation, default crop.
    pub fn e_to_sensor_green(&self, ex: f64, ey: f64) -> (f64, f64) {
        let (ex, ey) = match &self.keystone {
            Some(h) => homography(h, ex, ey),
            None => (ex, ey),
        };
        let Some(l) = &self.lens else {
            return self.e_to_sensor.apply(ex, ey);
        };
        let (dx, dy) = (ex - l.center[0], ey - l.center[1]);
        let r = ((dx * dx + dy * dy).sqrt() * l.r_dist) as f32;
        let f = f64::from(l.model.dist_factor(r));
        self.e_to_sensor
            .apply(l.center[0] + dx * f, l.center[1] + dy * f)
    }

    /// Sensor positions sampled for R, G and B at an output position, and the lens
    /// vignetting correction gain there.
    pub fn sensor_at(&self, x: f64, y: f64) -> ([[f64; 2]; 3], f32) {
        let (ex, ey) = self.optical_e(x, y);
        let Some(l) = &self.lens else {
            let s = self.e_to_sensor.apply(ex, ey);
            return ([[s.0, s.1]; 3], 1.0);
        };
        let (dx, dy) = (ex - l.center[0], ey - l.center[1]);
        let r = ((dx * dx + dy * dy).sqrt() * l.r_dist) as f32;
        let f = f64::from(l.model.dist_factor(r));
        let (gx, gy) = (dx * f, dy * f);
        let rd = r * f as f32;
        let at = |k: f64| {
            let s = self
                .e_to_sensor
                .apply(l.center[0] + gx * k, l.center[1] + gy * k);
            [s.0, s.1]
        };
        let kr = f64::from(LensModel::tca_factor(l.model.tca_red, rd));
        let kb = f64::from(LensModel::tca_factor(l.model.tca_blue, rd));
        let rv = ((gx * gx + gy * gy).sqrt() * l.r_vig) as f32;
        let fall = l.model.vig_falloff(rv);
        let gain = if fall > 0.05 { 1.0 / fall } else { 20.0 };
        ([at(kr), at(1.0), at(kb)], gain)
    }

    /// The optics as f32 uniforms (`to_e` rows, keystone rows, lens parameters, flags).
    pub fn optics_f32(&self) -> OpticsF32 {
        let k = self
            .keystone
            .unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
        let l = self.lens.unwrap_or(LensGeo {
            model: LensModel::IDENTITY,
            center: [0.0; 2],
            r_dist: 1.0,
            r_vig: 1.0,
        });
        OpticsF32 {
            to_e: self.to_e.to_f32(),
            keystone: k.map(|v| v as f32),
            has_keystone: self.keystone.is_some(),
            has_lens: self.lens.is_some(),
            center: [l.center[0] as f32, l.center[1] as f32],
            r_dist: l.r_dist as f32,
            r_vig: l.r_vig as f32,
            model: l.model,
        }
    }
}

/// f32 view of the optics for the GPU.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpticsF32 {
    pub to_e: [f32; 6],
    pub keystone: [f32; 9],
    pub has_keystone: bool,
    pub has_lens: bool,
    pub center: [f32; 2],
    pub r_dist: f32,
    pub r_vig: f32,
    pub model: LensModel,
}

/// Sensor sub-rectangle holding the visible image (the maker-note default crop, if sane).
pub fn image_area(raw: &RawImage) -> [u32; 4] {
    let (w, h) = (raw.size.width, raw.size.height);
    match raw.default_crop {
        Some([x, y, cw, ch])
            if cw >= 2 && ch >= 2 && x.saturating_add(cw) <= w && y.saturating_add(ch) <= h =>
        {
            [x, y, cw, ch]
        }
        _ => [0, 0, w, h],
    }
}

fn valid_rect(r: &NormRect) -> bool {
    [r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite()) && r.w > 0.0 && r.h > 0.0
}

/// Capture metadata the geometry needs (lens identification and focal length).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptureInfo {
    /// EXIF lens model, e.g. `FE 24-70mm F2.8 GM`.
    pub lens: Option<String>,
    /// Focal length in mm.
    pub focal_length: Option<f32>,
    /// F-number.
    pub aperture: Option<f32>,
    /// ISO sensitivity (drives ISO-adaptive noise reduction).
    pub iso: Option<u32>,
    /// Body crop factor (selects the lens calibration; `None` = 1.0, full frame, as on
    /// every Sony FE body).
    pub crop_factor: Option<f32>,
    /// Camera make and model (e.g. `SONY ILCE-9`): an imported camera profile made for
    /// another camera renders as Standard (`profile::camera_matches`). `None` = unknown.
    pub camera: Option<String>,
}

/// Output size of a frame that straighten, keystone or lens correction auto-fitted
/// inside the image (a full-frame crop shrunk about its centre so no corner is empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Fit {
    /// Lightroom's rule: the frame keeps the size the crop has on the sensor (a full
    /// frame stays 6000×4000 on the A9) and the inscribed area (a few percent smaller) is
    /// scaled back up in the same linear-light resample. A user crop gives
    /// `crop × sensor size` either way.
    #[default]
    FillOriginalSize,
    /// No scale-back: the output is the inscribed area at 1:1 sensor pixels (smaller).
    ConstrainToSensorPixels,
}

/// Maximum straighten angle, degrees.
pub const MAX_ANGLE: f32 = 45.0;
/// Maximum keystone angle, degrees.
pub const MAX_KEYSTONE: f32 = 30.0;

/// Keystone homography (corrected → source) for vertical/horizontal angles in degrees:
/// a virtual camera rotation about the image center with a pinhole of focal `f` pixels,
/// re-centred so the image center stays put. Positive vertical widens the top (fixes
/// verticals converging upward, as when a camera tilts up at a building); positive
/// horizontal widens the right side.
pub fn keystone_homography(
    vertical_deg: f32,
    horizontal_deg: f32,
    f: f64,
    center: [f64; 2],
) -> [f64; 9] {
    // Source camera tilt is the opposite of the correction: positive widens the top.
    let (sv, cv) = (-f64::from(vertical_deg)).to_radians().sin_cos();
    let (sh, ch) = (-f64::from(horizontal_deg)).to_radians().sin_cos();
    // R = Rx(v) · Ry(h); source ray = R · corrected ray.
    let rx = [[1.0, 0.0, 0.0], [0.0, cv, -sv], [0.0, sv, cv]];
    let ry = [[ch, 0.0, sh], [0.0, 1.0, 0.0], [-sh, 0.0, ch]];
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = (0..3).map(|k| rx[i][k] * ry[k][j]).sum();
        }
    }
    // H = T_c · K · R · K⁻¹ · T_{-c}, then re-centre the image center.
    let k = [[f, 0.0, 0.0], [0.0, f, 0.0], [0.0, 0.0, 1.0]];
    let kinv = [[1.0 / f, 0.0, 0.0], [0.0, 1.0 / f, 0.0], [0.0, 0.0, 1.0]];
    let mul = |a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]| {
        let mut o = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                o[i][j] = (0..3).map(|q| a[i][q] * b[q][j]).sum();
            }
        }
        o
    };
    let m = mul(&mul(&k, &r), &kinv);
    let h0 = [
        m[0][0], m[0][1], m[0][2], m[1][0], m[1][1], m[1][2], m[2][0], m[2][1], m[2][2],
    ];
    let (ox, oy) = homography(&h0, 0.0, 0.0);
    // x_src = H0(x − c) − H0(0) + c.
    let t_in = [
        [1.0, 0.0, -center[0]],
        [0.0, 1.0, -center[1]],
        [0.0, 0.0, 1.0],
    ];
    let t_out = [
        [1.0, 0.0, center[0] - ox],
        [0.0, 1.0, center[1] - oy],
        [0.0, 0.0, 1.0],
    ];
    let full = mul(&mul(&t_out, &m), &t_in);
    [
        full[0][0], full[0][1], full[0][2], full[1][0], full[1][1], full[1][2], full[2][0],
        full[2][1], full[2][2],
    ]
}

/// The crop that straightens by `angle_deg` and keeps the largest rectangle of the full
/// frame's aspect ratio that has no empty corners (for the app's straighten tool).
pub fn inscribed_crop(angle_deg: f32, size: Size) -> Crop {
    let angle = if angle_deg.is_finite() {
        angle_deg.clamp(-MAX_ANGLE, MAX_ANGLE)
    } else {
        0.0
    };
    let mut c = Crop {
        angle_deg: angle,
        ..Crop::default()
    };
    if size.width == 0 || size.height == 0 {
        return c;
    }
    let (w, h) = (f64::from(size.width), f64::from(size.height));
    let (s, co) = f64::from(angle).to_radians().sin_cos();
    let (a, b) = (w * 0.5, h * 0.5);
    let (ex, ey) = (a * co.abs() + b * s.abs(), a * s.abs() + b * co.abs());
    let k = (a / ex).min(b / ey).min(1.0);
    c.rect = NormRect {
        x: (0.5 - 0.5 * k) as f32,
        y: (0.5 - 0.5 * k) as f32,
        w: k as f32,
        h: k as f32,
    }
    .clamped();
    c
}

/// Lens corrections for this capture, if a profile applies.
pub fn lens_geo(lc: &LensCorrection, capture: &CaptureInfo, e_size: [f64; 2]) -> Option<LensGeo> {
    if !lc.enabled {
        return None;
    }
    let name = lc.profile.as_deref().or(capture.lens.as_deref())?;
    let crop = capture.crop_factor.unwrap_or(1.0);
    let profile = lens::find(name, crop)?;
    let focal = capture.focal_length.filter(|f| f.is_finite() && *f > 0.0)?;
    let model = profile.model(focal, capture.aperture).with_strength(
        lc.distortion,
        lc.chromatic_aberration,
        lc.light_falloff,
    );
    if model.is_identity() {
        return None;
    }
    let (w, h) = (e_size[0], e_size[1]);
    // Coefficients are in the calibration sensor's units: a point at our radius r lies at
    // r · crop_calibration / crop_body there.
    let crop = if crop.is_finite() && crop > 0.0 {
        crop
    } else {
        1.0
    };
    let k = f64::from(profile.crop_factor / crop);
    // Distortion and TCA radii are in units of half the calibration sensor's short side;
    // for a given diagonal (crop factor) that side depends on its aspect ratio. Ours is
    // taken as 3:2 (lensfun's default), so 3:2 calibrations need no change and a 4:3
    // one (Micro Four Thirds) is scaled by √(a²+1)/√(1.5²+1).
    let aspect = f64::from(profile.aspect);
    let k_dist = k * (aspect * aspect + 1.0).sqrt() / 3.25f64.sqrt();
    Some(LensGeo {
        model,
        center: [w * 0.5, h * 0.5],
        r_dist: 2.0 / w.min(h) * k_dist,
        r_vig: 2.0 / w.hypot(h) * k,
    })
}

/// Does the output rectangle, mapped through `f`, stay inside `[0,w]×[0,h]`?
fn fits(corners: impl Fn(f64, f64) -> (f64, f64), w: f64, h: f64) -> bool {
    const N: usize = 16;
    (0..=N).all(|i| {
        let t = i as f64 / N as f64;
        [(t, 0.0), (t, 1.0), (0.0, t), (1.0, t)]
            .iter()
            .all(|&(u, v)| {
                let (x, y) = corners(u, v);
                x >= -0.5 && y >= -0.5 && x <= w + 0.5 && y <= h + 0.5
            })
    })
}

/// Resolve the output size and coordinate maps.
///
/// `fit` only changes the output *scale* of an auto-fitted frame (see [`Fit`]): what is
/// framed, and so every normalized mapping (`to_norm`, `frame_to_crop_space`), is the same.
pub fn plan(
    raw: &RawImage,
    adj: &Adjustments,
    capture: &CaptureInfo,
    region: Option<NormRect>,
    max: Size,
    fit: Fit,
) -> Result<Geometry, PipelineError> {
    let crop = &adj.crop;
    if max.width == 0 || max.height == 0 {
        return Err(PipelineError::InvalidRequest(format!(
            "max_size {max:?} is empty"
        )));
    }
    if !valid_rect(&crop.rect) || !crop.angle_deg.is_finite() {
        return Err(PipelineError::InvalidRequest("non-finite crop".into()));
    }
    let [ox, oy, aw, ah] = image_area(raw);
    let e = raw.orientation.apply_to(Size::new(aw, ah));
    let (ew, eh) = (f64::from(e.width), f64::from(e.height));
    let angle = f64::from(crop.angle_deg.clamp(-MAX_ANGLE, MAX_ANGLE));
    let (s, c) = angle.to_radians().sin_cos();

    // Optics between the corrected E frame and the sensor.
    let lens = lens_geo(&adj.lens, capture, [ew, eh]);
    let kv = finite_or(adj.lens.keystone_vertical).clamp(-MAX_KEYSTONE, MAX_KEYSTONE);
    let kh = finite_or(adj.lens.keystone_horizontal).clamp(-MAX_KEYSTONE, MAX_KEYSTONE);
    let keystone = (kv != 0.0 || kh != 0.0).then(|| {
        let focal = capture
            .focal_length
            .filter(|f| f.is_finite() && *f > 0.0)
            .unwrap_or(35.0);
        keystone_homography(
            kv,
            kh,
            f64::from(focal) / 36.0 * ew.max(eh),
            [ew * 0.5, eh * 0.5],
        )
    });
    let to_optical = |x: f64, y: f64| -> (f64, f64) {
        let (x, y) = keystone.as_ref().map_or((x, y), |h| homography(h, x, y));
        match &lens {
            None => (x, y),
            Some(l) => {
                let (dx, dy) = (x - l.center[0], y - l.center[1]);
                let r = ((dx * dx + dy * dy).sqrt() * l.r_dist) as f32;
                let f = f64::from(l.model.dist_factor(r));
                (l.center[0] + dx * f, l.center[1] + dy * f)
            }
        }
    };

    // A full-frame crop shrinks (about the center, same aspect) until straighten, keystone
    // and lens corrections leave no empty corners.
    let mut r = crop.rect.clamped();
    // Native pixels per framed E pixel: > 1 when an auto-fitted frame is scaled back up to
    // the size the user's rect has on the sensor (`Fit::FillOriginalSize`).
    let mut fill = 1.0;
    if r.is_full() && (angle != 0.0 || keystone.is_some() || lens.is_some()) {
        let rect_fits = |k: f64| {
            let (hx, hy) = (ew * 0.5 * k, eh * 0.5 * k);
            fits(
                |u, v| {
                    let (lx, ly) = ((2.0 * u - 1.0) * hx, (2.0 * v - 1.0) * hy);
                    to_optical(ew * 0.5 + c * lx - s * ly, eh * 0.5 + s * lx + c * ly)
                },
                ew,
                eh,
            )
        };
        if !rect_fits(1.0) {
            let (mut lo, mut hi) = (0.0, 1.0);
            for _ in 0..30 {
                let m = f64::midpoint(lo, hi);
                if rect_fits(m) {
                    lo = m;
                } else {
                    hi = m;
                }
            }
            let k = lo.max(1e-3) as f32;
            if fit == Fit::FillOriginalSize {
                fill = 1.0 / f64::from(k);
            }
            r = NormRect {
                x: 0.5 - 0.5 * k,
                y: 0.5 - 0.5 * k,
                w: k,
                h: k,
            };
        }
    }
    let (cx0, cy0) = (f64::from(r.x) * ew, f64::from(r.y) * eh);
    let (cw, ch) = (f64::from(r.w) * ew, f64::from(r.h) * eh);
    let turns = crop.quarter_turns % 4;
    let (fw, fh) = if turns % 2 == 1 { (ch, cw) } else { (cw, ch) };

    let reg = match region {
        Some(g) if valid_rect(&g) => g.clamped(),
        Some(_) => return Err(PipelineError::InvalidRequest("non-finite region".into())),
        None => NormRect::FULL,
    };
    let (rx, ry) = (f64::from(reg.x) * fw, f64::from(reg.y) * fh);
    let (rw, rh) = (f64::from(reg.w) * fw, f64::from(reg.h) * fh);
    let native = Size::new(
        ((rw * fill).round() as u32).max(1),
        ((rh * fill).round() as u32).max(1),
    );
    let out = native.fit_within(max);
    let (sx, sy) = (f64::from(out.width) / rw, f64::from(out.height) / rh);

    // Output pixel center → final frame F.
    let out_to_f = Affine::scale(1.0 / sx, 1.0 / sy).then(&Affine::translate(rx, ry));
    // Undo flips (applied last, in F).
    let unflip = Affine([
        [
            if crop.flip_h { -1.0 } else { 1.0 },
            0.0,
            if crop.flip_h { fw } else { 0.0 },
        ],
        [
            0.0,
            if crop.flip_v { -1.0 } else { 1.0 },
            if crop.flip_v { fh } else { 0.0 },
        ],
    ]);
    // Undo `turns` clockwise quarter turns. One CW turn maps local (qx,qy) in a (w,h) frame to
    // (h − qy, qx); its inverse maps (fx, fy) → (fy, h − fx) where h is the pre-turn height.
    let mut unturn = Affine::IDENTITY;
    let mut dims = (fw, fh);
    for _ in 0..turns {
        let pre_h = dims.0;
        unturn = unturn.then(&Affine([[0.0, 1.0, 0.0], [-1.0, 0.0, pre_h]]));
        dims = (dims.1, dims.0);
    }
    // Crop-local → E: rotate about the crop center by the straighten angle.
    let (hx, hy) = (cw * 0.5, ch * 0.5);
    let local_to_e = Affine::translate(-hx, -hy)
        .then(&Affine([[c, -s, 0.0], [s, c, 0.0]]))
        .then(&Affine::translate(cx0 + hx, cy0 + hy));
    let to_e = out_to_f.then(&unflip).then(&unturn).then(&local_to_e);
    let to_norm = to_e.then(&Affine::scale(1.0 / ew, 1.0 / eh));
    let e_to_sensor = orientation_to_stored(raw.orientation, f64::from(aw), f64::from(ah))
        .then(&Affine::translate(f64::from(ox), f64::from(oy)));
    let half_diag = 0.5 * fw.hypot(fh);
    let to_vignette = out_to_f
        .then(&Affine::translate(-fw * 0.5, -fh * 0.5))
        .then(&Affine::scale(1.0 / half_diag, 1.0 / half_diag));
    Ok(Geometry {
        out,
        scale: f64::midpoint(sx, sy) as f32,
        to_e,
        keystone,
        lens,
        e_to_sensor,
        to_norm,
        to_vignette,
        e_size: [ew, eh],
        sensor: [f64::from(raw.size.width), f64::from(raw.size.height)],
        origin_out: [rx * sx, ry * sy],
        frame_out: [fw * sx, fh * sy],
    })
}

impl Geometry {
    /// The sub-rectangle `r` = (x, y, w, h) of this render's output as a render of its own.
    pub fn window(&self, r: [u32; 4]) -> Geometry {
        let shift = Affine::translate(f64::from(r[0]), f64::from(r[1]));
        let mut g = *self;
        g.out = Size::new(r[2].max(1), r[3].max(1));
        g.to_e = shift.then(&self.to_e);
        g.to_norm = shift.then(&self.to_norm);
        g.to_vignette = shift.then(&self.to_vignette);
        g.origin_out = [
            self.origin_out[0] + f64::from(r[0]),
            self.origin_out[1] + f64::from(r[1]),
        ];
        g
    }

    /// Grow a region render by up to `apron` output pixels on each side (within the
    /// frame), so neighbourhood filters see the same context a full-frame render would.
    /// Returns the grown geometry and where the requested window sits inside it.
    pub fn with_apron(&self, apron: u32) -> (Geometry, [u32; 4]) {
        let a = f64::from(apron);
        let left = self.origin_out[0].max(0.0).min(a).floor();
        let top = self.origin_out[1].max(0.0).min(a).floor();
        let (w, h) = (f64::from(self.out.width), f64::from(self.out.height));
        let right = (self.frame_out[0] - self.origin_out[0] - w)
            .max(0.0)
            .min(a)
            .floor();
        let bottom = (self.frame_out[1] - self.origin_out[1] - h)
            .max(0.0)
            .min(a)
            .floor();
        let shift = Affine::translate(-left, -top);
        let mut g = *self;
        g.out = Size::new((w + left + right) as u32, (h + top + bottom) as u32);
        g.to_e = shift.then(&self.to_e);
        g.to_norm = shift.then(&self.to_norm);
        g.to_vignette = shift.then(&self.to_vignette);
        g.origin_out = [self.origin_out[0] - left, self.origin_out[1] - top];
        (
            g,
            [left as u32, top as u32, self.out.width, self.out.height],
        )
    }
}

fn finite_or(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

/// Bilinear sample of the base image at level coordinates (pixel centers at k+0.5).
/// Positions more than half a pixel outside the level are black (no edge streaks);
/// inside, the stored (sub-)rectangle is clamped to its edge.
#[inline]
pub fn sample_bilinear(base: &BaseImage, lw: f32, lh: f32, lx: f32, ly: f32) -> [f32; 3] {
    if !(lx >= -0.5 && ly >= -0.5 && lx <= lw + 0.5 && ly <= lh + 0.5) {
        return [0.0; 3];
    }
    bilinear_in(base, lx, ly)
}

/// [`sample_bilinear`] with the position clamped to the level edge instead of black
/// outside it; `false` when it had to be clamped (or is not finite: black).
#[inline]
pub fn sample_bilinear_clamped(
    base: &BaseImage,
    lw: f32,
    lh: f32,
    lx: f32,
    ly: f32,
) -> ([f32; 3], bool) {
    if !(lx.is_finite() && ly.is_finite()) {
        return ([0.0; 3], false);
    }
    let inside = lx >= -0.5 && ly >= -0.5 && lx <= lw + 0.5 && ly <= lh + 0.5;
    (
        bilinear_in(base, lx.clamp(-0.5, lw + 0.5), ly.clamp(-0.5, lh + 0.5)),
        inside,
    )
}

#[inline]
fn bilinear_in(base: &BaseImage, lx: f32, ly: f32) -> [f32; 3] {
    let (w, h) = (base.w as i32, base.h as i32);
    let fx = (lx - base.x0 as f32 - 0.5).clamp(0.0, (w - 1) as f32);
    let fy = (ly - base.y0 as f32 - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (fx as i32, fy as i32);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let p = |x: i32, y: i32| base.px[(y * w + x) as usize];
    let (a, b, c, d) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
    [0, 1, 2].map(|k| {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bot = c[k] + (d[k] - c[k]) * tx;
        top + (bot - top) * ty
    })
}

/// Camera value (bilinear from the BASE, per channel for lateral CA) and lens vignetting
/// gain at output position `(px, py)`.
#[inline]
pub fn sample_point(
    base: &BaseImage,
    geo: &Geometry,
    level: [u32; 2],
    px: f64,
    py: f64,
) -> ([f32; 3], f32) {
    let (lw, lh) = (level[0] as f32, level[1] as f32);
    let sc = f64::from(base.scale);
    let (pos, vg) = geo.sensor_at(px, py);
    let mut o = [0.0f32; 3];
    if pos[0] == pos[1] && pos[2] == pos[1] {
        o = sample_bilinear(
            base,
            lw,
            lh,
            (pos[1][0] * sc) as f32,
            (pos[1][1] * sc) as f32,
        );
    } else {
        for c in 0..3 {
            o[c] = sample_bilinear(
                base,
                lw,
                lh,
                (pos[c][0] * sc) as f32,
                (pos[c][1] * sc) as f32,
            )[c];
        }
    }
    (o, vg)
}

/// [`sample_point`] with every read clamped to the level edge (spot sources and rings:
/// never black), and whether all of it was on the sensor.
#[inline]
pub fn sample_point_clamped(
    base: &BaseImage,
    geo: &Geometry,
    level: [u32; 2],
    px: f64,
    py: f64,
) -> ([f32; 3], f32, bool) {
    let (lw, lh) = (level[0] as f32, level[1] as f32);
    let sc = f64::from(base.scale);
    let (pos, vg) = geo.sensor_at(px, py);
    let at = |c: usize| {
        sample_bilinear_clamped(
            base,
            lw,
            lh,
            (pos[c][0] * sc) as f32,
            (pos[c][1] * sc) as f32,
        )
    };
    if pos[0] == pos[1] && pos[2] == pos[1] {
        let (o, ok) = at(1);
        (o, vg, ok)
    } else {
        let s = [at(0), at(1), at(2)];
        ([s[0].0[0], s[1].0[1], s[2].0[2]], vg, s.iter().all(|v| v.1))
    }
}

/// Resample the base onto the output grid (per-channel for lateral CA), returning camera
/// values and the lens vignetting gain per pixel. `supersample`: average 2×2 samples at
/// ±¼ output pixel (see `Plan::supersample`); the vignetting gain is the center's.
pub fn resample(
    base: &BaseImage,
    geo: &Geometry,
    level: [u32; 2],
    supersample: bool,
) -> (Vec<[f32; 3]>, Vec<f32>) {
    let (w, h) = (geo.out.width as usize, geo.out.height as usize);
    let mut out = vec![[0.0f32; 3]; w * h];
    let mut gain = vec![1.0f32; w * h];
    out.par_chunks_mut(w)
        .zip(gain.par_chunks_mut(w))
        .enumerate()
        .for_each(|(y, (row, grow))| {
            let py = y as f64 + 0.5;
            let sample = |px: f64, py: f64| sample_point(base, geo, level, px, py);
            for (x, (o, g)) in row.iter_mut().zip(grow.iter_mut()).enumerate() {
                let px = x as f64 + 0.5;
                let (c, vg) = sample(px, py);
                *g = vg;
                *o = if supersample {
                    let mut acc = [0.0f32; 3];
                    for (dx, dy) in [(-0.25, -0.25), (0.25, -0.25), (-0.25, 0.25), (0.25, 0.25)] {
                        let s = sample(px + dx, py + dy).0;
                        for k in 0..3 {
                            acc[k] += 0.25 * s[k];
                        }
                    }
                    acc
                } else {
                    c
                };
            }
        });
    (out, gain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::CfaPattern;

    fn raw(w: u32, h: u32, o: Orientation) -> RawImage {
        RawImage {
            size: Size::new(w, h),
            cfa: CfaPattern::Rggb,
            data: vec![0; (w * h) as usize],
            black_level: [0.0; 4],
            white_level: 1000.0,
            wb_as_shot: [1.0; 3],
            cam_to_xyz: crate::color::REC2020_TO_XYZ,
            wb_daylight: [1.0; 3],
            orientation: o,
            default_crop: None,
        }
    }

    fn plan_crop(
        r: &RawImage,
        crop: &Crop,
        region: Option<NormRect>,
        max: Size,
    ) -> Result<Geometry, PipelineError> {
        let adj = Adjustments {
            crop: *crop,
            ..Adjustments::default()
        };
        plan(
            r,
            &adj,
            &CaptureInfo::default(),
            region,
            max,
            Fit::ConstrainToSensorPixels,
        )
    }

    /// Green-channel sensor position of an output position.
    fn sensor(g: &Geometry, x: f64, y: f64) -> (f64, f64) {
        let p = g.sensor_at(x, y).0[1];
        (p[0], p[1])
    }

    fn near(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6
    }

    #[test]
    fn affine_compose_and_invert() {
        let a = Affine::scale(2.0, 3.0).then(&Affine::translate(1.0, -1.0));
        assert!(near(a.apply(1.0, 1.0), (3.0, 2.0)));
        let inv = a.inverse().unwrap();
        assert!(near(inv.apply(3.0, 2.0), (1.0, 1.0)));
        assert!(Affine::scale(0.0, 1.0).inverse().is_none());
        assert_eq!(Affine::IDENTITY.to_f32(), [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn orientation_mapping_matches_exif() {
        // Stored 4×2 image. Check where the displayed top-left pixel center comes from.
        let (sw, sh) = (4.0, 2.0);
        let tl = |o| orientation_to_stored(o, sw, sh).apply(0.5, 0.5);
        assert!(near(tl(Orientation::Normal), (0.5, 0.5)));
        assert!(near(tl(Orientation::FlipH), (3.5, 0.5)));
        assert!(near(tl(Orientation::Rotate180), (3.5, 1.5)));
        assert!(near(tl(Orientation::FlipV), (0.5, 1.5)));
        assert!(near(tl(Orientation::Transpose), (0.5, 0.5)));
        // Rotate90 (EXIF 6): displayed top-left is the stored bottom-left.
        assert!(near(tl(Orientation::Rotate90), (0.5, 1.5)));
        assert!(near(tl(Orientation::Transverse), (3.5, 1.5)));
        // Rotate270 (EXIF 8): displayed top-left is the stored top-right.
        assert!(near(tl(Orientation::Rotate270), (3.5, 0.5)));
        // Every mapping is a bijection of the pixel grid.
        for o in [
            Orientation::Normal,
            Orientation::FlipH,
            Orientation::Rotate180,
            Orientation::FlipV,
            Orientation::Transpose,
            Orientation::Rotate90,
            Orientation::Transverse,
            Orientation::Rotate270,
        ] {
            let e = o.apply_to(Size::new(4, 2));
            let m = orientation_to_stored(o, sw, sh);
            let mut seen = std::collections::HashSet::new();
            for y in 0..e.height {
                for x in 0..e.width {
                    let (sx, sy) = m.apply(f64::from(x) + 0.5, f64::from(y) + 0.5);
                    assert!(
                        (0.0..4.0).contains(&sx) && (0.0..2.0).contains(&sy),
                        "{o:?}"
                    );
                    seen.insert(((sx - 0.5) as i32, (sy - 0.5) as i32));
                }
            }
            assert_eq!(seen.len(), 8, "{o:?}");
        }
    }

    #[test]
    fn full_frame_plan_and_fit() {
        let r = raw(6000, 4000, Orientation::Normal);
        let g = plan_crop(&r, &Crop::default(), None, Size::new(2560, 2560)).unwrap();
        assert_eq!(g.out, Size::new(2560, 1707));
        assert!((g.scale - 2560.0 / 6000.0).abs() < 1e-3);
        // Output corners map to sensor corners.
        let (x, y) = sensor(&g, 0.0, 0.0);
        assert!(x.abs() < 1e-6 && y.abs() < 1e-6);
        let (x, y) = sensor(&g, 2560.0, 1707.0);
        assert!((x - 6000.0).abs() < 1e-3 && (y - 4000.0).abs() < 1e-3);
        let (u, v) = g.to_norm.apply(1280.0, 853.5);
        assert!((u - 0.5).abs() < 1e-6 && (v - 0.5).abs() < 1e-6);
        let (vx, vy) = g.to_vignette.apply(2560.0, 1707.0);
        assert!((vx.hypot(vy) - 1.0).abs() < 1e-6);
        assert_eq!(g.iso(), [1.0, 4000.0 / 6000.0]);
        assert!((g.long_edge_out() - 2560.0).abs() < 1.0);
        // Never upscales.
        let g = plan_crop(&r, &Crop::default(), None, Size::new(10000, 10000)).unwrap();
        assert_eq!(g.out, Size::new(6000, 4000));
    }

    #[test]
    fn crop_dimensions_and_offsets() {
        let r = raw(6000, 4000, Orientation::Normal);
        let crop = Crop {
            rect: NormRect {
                x: 0.25,
                y: 0.5,
                w: 0.5,
                h: 0.25,
            },
            ..Crop::default()
        };
        let g = plan_crop(&r, &crop, None, Size::new(10000, 10000)).unwrap();
        assert_eq!(g.out, Size::new(3000, 1000));
        // First output pixel center samples the first cropped sensor pixel center.
        let (x, y) = sensor(&g, 0.5, 0.5);
        assert!((x - 1500.5).abs() < 1e-6 && (y - 2000.5).abs() < 1e-6);
        let (x, y) = sensor(&g, 2999.5, 999.5);
        assert!((x - 4499.5).abs() < 1e-6 && (y - 2999.5).abs() < 1e-6);
    }

    #[test]
    fn default_crop_is_the_image_area() {
        let mut r = raw(6048, 4024, Orientation::Normal);
        r.default_crop = Some([12, 12, 6000, 4000]);
        assert_eq!(image_area(&r), [12, 12, 6000, 4000]);
        let g = plan_crop(&r, &Crop::default(), None, Size::new(6000, 6000)).unwrap();
        assert_eq!(g.out, Size::new(6000, 4000));
        let (x, y) = sensor(&g, 0.5, 0.5);
        assert!((x - 12.5).abs() < 1e-6 && (y - 12.5).abs() < 1e-6);
        r.default_crop = Some([6000, 0, 100, 100]);
        assert_eq!(image_area(&r), [0, 0, 6048, 4024]);
    }

    #[test]
    fn quarter_turns_and_flips() {
        let r = raw(400, 200, Orientation::Normal);
        let max = Size::new(1000, 1000);
        let one = Crop {
            quarter_turns: 1,
            ..Crop::default()
        };
        let g = plan_crop(&r, &one, None, max).unwrap();
        assert_eq!(g.out, Size::new(200, 400));
        // After one clockwise turn, the output top-left shows the source bottom-left.
        let (x, y) = sensor(&g, 0.5, 0.5);
        assert!(
            (x - 0.5).abs() < 1e-6 && (y - 199.5).abs() < 1e-6,
            "{x} {y}"
        );
        let three = Crop {
            quarter_turns: 3,
            ..Crop::default()
        };
        let (x, y) = sensor(&plan_crop(&r, &three, None, max).unwrap(), 0.5, 0.5);
        assert!(
            (x - 399.5).abs() < 1e-6 && (y - 0.5).abs() < 1e-6,
            "{x} {y}"
        );
        let two = Crop {
            quarter_turns: 2,
            flip_h: true,
            ..Crop::default()
        };
        let (x, y) = sensor(&plan_crop(&r, &two, None, max).unwrap(), 0.5, 0.5);
        assert!(
            (x - 0.5).abs() < 1e-6 && (y - 199.5).abs() < 1e-6,
            "{x} {y}"
        );
        let fv = Crop {
            flip_v: true,
            ..Crop::default()
        };
        let (x, y) = sensor(&plan_crop(&r, &fv, None, max).unwrap(), 0.5, 0.5);
        assert!((x - 0.5).abs() < 1e-6 && (y - 199.5).abs() < 1e-6);
        // EXIF orientation composes: a Rotate90 file displays portrait.
        let p = raw(400, 200, Orientation::Rotate90);
        assert_eq!(
            plan_crop(&p, &Crop::default(), None, max).unwrap().out,
            Size::new(200, 400)
        );
    }

    #[test]
    fn straighten_rotates_about_crop_center() {
        let r = raw(1000, 1000, Orientation::Normal);
        let crop = Crop {
            angle_deg: 90.0,
            rect: NormRect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 0.999_999,
            },
            ..Crop::default()
        };
        let g = plan_crop(&r, &crop, None, Size::new(1000, 1000)).unwrap();
        // Center stays put.
        let (x, y) = sensor(&g, 500.0, 500.0);
        assert!((x - 500.0).abs() < 1e-2 && (y - 500.0).abs() < 1e-2);
        // Clamped to 45°: a point right of center samples from below-right of center
        // (content turns counter-clockwise).
        let (x, y) = sensor(&g, 600.0, 500.0);
        let d = 100.0 * std::f64::consts::FRAC_1_SQRT_2;
        assert!(
            (x - (500.0 + d)).abs() < 1e-3 && (y - (500.0 + d)).abs() < 1e-3,
            "{x} {y}"
        );
    }

    #[test]
    fn region_zoom() {
        let r = raw(6000, 4000, Orientation::Normal);
        let reg = NormRect {
            x: 0.5,
            y: 0.5,
            w: 0.25,
            h: 0.25,
        };
        let g = plan_crop(&r, &Crop::default(), Some(reg), Size::new(4000, 4000)).unwrap();
        assert_eq!(g.out, Size::new(1500, 1000));
        assert!((g.scale - 1.0).abs() < 1e-6);
        let (x, y) = sensor(&g, 0.5, 0.5);
        assert!((x - 3000.5).abs() < 1e-6 && (y - 2000.5).abs() < 1e-6);
    }

    #[test]
    fn invalid_requests() {
        let r = raw(100, 100, Orientation::Normal);
        assert!(plan_crop(&r, &Crop::default(), None, Size::new(0, 10)).is_err());
        let bad = Crop {
            angle_deg: f32::NAN,
            ..Crop::default()
        };
        assert!(plan_crop(&r, &bad, None, Size::new(10, 10)).is_err());
        let bad_rect = Crop {
            rect: NormRect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 1.0,
            },
            ..Crop::default()
        };
        assert!(plan_crop(&r, &bad_rect, None, Size::new(10, 10)).is_err());
        let reg = NormRect {
            x: f32::NAN,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        };
        assert!(plan_crop(&r, &Crop::default(), Some(reg), Size::new(10, 10)).is_err());
    }

    #[test]
    fn resample_identity_edges_black_outside() {
        let base = BaseImage {
            w: 4,
            h: 2,
            scale: 1.0,
            x0: 0,
            y0: 0,
            px: (0..8).map(|i| [i as f32, 0.0, 1.0]).collect(),
        };
        let r = raw(4, 2, Orientation::Normal);
        let g = plan_crop(&r, &Crop::default(), None, Size::new(4, 2)).unwrap();
        let (out, gain) = resample(&base, &g, [4, 2], false);
        assert_eq!(
            out.iter().map(|p| p[0]).collect::<Vec<_>>(),
            (0..8).map(|i| i as f32).collect::<Vec<_>>()
        );
        assert!(gain.iter().all(|g| *g == 1.0));
        // Supersampled: each pixel is the mean of the four samples at ±¼ px.
        let (ss, ssg) = resample(&base, &g, [4, 2], true);
        let expect = [(1.25, 0.25), (1.75, 0.25), (1.25, 0.75), (1.75, 0.75)]
            .iter()
            .map(|(x, y)| sample_bilinear(&base, 4.0, 2.0, *x, *y)[0])
            .sum::<f32>()
            / 4.0;
        assert!((ss[1][0] - expect).abs() < 1e-5, "{} vs {expect}", ss[1][0]);
        assert_ne!(ss[1][0], out[1][0]);
        assert_eq!(gain, ssg);
        // Within half a pixel of the edge: clamped; beyond: black (no streaks).
        assert_eq!(sample_bilinear(&base, 4.0, 2.0, 0.2, 0.2), [0.0, 0.0, 1.0]);
        assert_eq!(sample_bilinear(&base, 4.0, 2.0, -10.0, -10.0), [0.0; 3]);
        assert_eq!(sample_bilinear(&base, 4.0, 2.0, 100.0, 1.0), [0.0; 3]);
        assert_eq!(sample_bilinear(&base, 4.0, 2.0, f32::NAN, 1.0), [0.0; 3]);
        // Midway between two pixels.
        assert_eq!(sample_bilinear(&base, 4.0, 2.0, 1.0, 0.5)[0], 0.5);
        // A sub-rectangle base is addressed in level coordinates.
        let sub = BaseImage {
            x0: 2,
            y0: 0,
            w: 2,
            h: 2,
            px: vec![[5.0; 3]; 4],
            ..base.clone()
        };
        assert_eq!(sample_bilinear(&sub, 4.0, 2.0, 3.0, 1.0), [5.0; 3]);
    }

    /// The size rule of [`Fit`]: an auto-fitted full frame keeps the sensor size (turns
    /// swap it) with `FillOriginalSize`, is the inscribed area with
    /// `ConstrainToSensorPixels`; a user crop and an uncorrected frame are `rect × E`
    /// either way; and the framing (normalized maps) never depends on it.
    #[test]
    fn fit_fills_auto_fitted_frames_to_the_original_size() {
        let r = raw(3000, 2000, Orientation::Normal);
        let at = |crop: Crop, fit, max| {
            let adj = Adjustments {
                crop,
                ..Adjustments::default()
            };
            plan(&r, &adj, &CaptureInfo::default(), None, max, fit).unwrap()
        };
        let big = Size::new(100_000, 100_000);
        let (fill, con) = (Fit::FillOriginalSize, Fit::ConstrainToSensorPixels);
        let tilted = Crop {
            angle_deg: 7.0,
            ..Crop::default()
        };
        let f = at(tilted, fill, big);
        let c = at(tilted, con, big);
        assert_eq!(f.out, Size::new(3000, 2000));
        assert!(c.out.width < 2700 && c.out.height < 1800, "{:?}", c.out);
        assert!(
            f.scale > 1.05 && (c.scale - 1.0).abs() < 1e-3,
            "{} {}",
            f.scale,
            c.scale
        );
        // Same framing: the output corners land on the same E positions.
        for (u, v) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.5, 0.5)] {
            let a = f.to_norm.apply(u * 3000.0, v * 2000.0);
            let b = c
                .to_norm
                .apply(u * f64::from(c.out.width), v * f64::from(c.out.height));
            assert!(
                (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3,
                "{a:?} {b:?}"
            );
        }
        // Turned: the sensor size, swapped.
        let turned = Crop {
            quarter_turns: 3,
            ..tilted
        };
        assert_eq!(at(turned, fill, big).out, Size::new(2000, 3000));
        // Bounded outputs keep the aspect and the box.
        assert_eq!(
            at(tilted, fill, Size::new(1500, 1500)).out,
            Size::new(1500, 1000)
        );
        // A user crop is rect × E either way.
        let user = Crop {
            angle_deg: 7.0,
            rect: NormRect {
                x: 0.1,
                y: 0.1,
                w: 0.5,
                h: 0.5,
            },
            ..Crop::default()
        };
        assert_eq!(at(user, fill, big).out, Size::new(1500, 1000));
        assert_eq!(at(user, con, big).out, Size::new(1500, 1000));
        // Nothing to fit: identical.
        assert_eq!(
            at(Crop::default(), fill, big),
            at(Crop::default(), con, big)
        );
        assert_eq!(Fit::default(), fill);
    }

    #[test]
    fn straighten_auto_fits_a_full_crop_and_clamps_the_angle() {
        let r = raw(3000, 2000, Orientation::Normal);
        let crop = Crop {
            angle_deg: 10.0,
            ..Crop::default()
        };
        let g = plan_crop(&r, &crop, None, Size::new(10000, 10000)).unwrap();
        // Shrunk (no empty corners) but the aspect is kept.
        assert!(g.out.width < 3000 && g.out.height < 2000);
        assert!((f64::from(g.out.width) / f64::from(g.out.height) - 1.5).abs() < 0.01);
        for (x, y) in [
            (0.0, 0.0),
            (f64::from(g.out.width), 0.0),
            (0.0, f64::from(g.out.height)),
        ] {
            let (sx, sy) = sensor(&g, x, y);
            assert!(
                (-0.6..=3000.6).contains(&sx) && (-0.6..=2000.6).contains(&sy),
                "{sx} {sy}"
            );
        }
        // The helper agrees with the auto-fit for a pure rotation.
        let ins = inscribed_crop(10.0, Size::new(3000, 2000));
        assert!(
            (f64::from(ins.rect.w) * 3000.0 - f64::from(g.out.width)).abs() < 3.0,
            "{:?} {:?}",
            ins.rect,
            g.out
        );
        assert_eq!(inscribed_crop(1e9, Size::new(10, 10)).angle_deg, MAX_ANGLE);
        assert_eq!(inscribed_crop(f32::NAN, Size::new(10, 10)).angle_deg, 0.0);
        assert!(inscribed_crop(5.0, Size::new(0, 10)).rect.is_full());
        // A user crop is respected as is (outside samples are black instead).
        let user = Crop {
            angle_deg: 10.0,
            rect: NormRect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 0.5,
            },
            ..Crop::default()
        };
        let g = plan_crop(&r, &user, None, Size::new(10000, 10000)).unwrap();
        assert_eq!(g.out, Size::new(3000, 1000));
    }

    #[test]
    fn apron_grows_region_renders_within_the_frame() {
        let r = raw(1000, 800, Orientation::Normal);
        let reg = NormRect {
            x: 0.1,
            y: 0.0,
            w: 0.5,
            h: 0.5,
        };
        let g = plan_crop(&r, &Crop::default(), Some(reg), Size::new(5000, 5000)).unwrap();
        assert!((g.origin_out[0] - 100.0).abs() < 1e-3 && g.origin_out[1] == 0.0);
        assert!((g.frame_out[0] - 1000.0).abs() < 1e-3 && (g.frame_out[1] - 800.0).abs() < 1e-3);
        let (e, win) = g.with_apron(50);
        // 50 px left and right and below; none above (frame edge).
        assert_eq!(win, [50, 0, 500, 400]);
        assert_eq!(e.out, Size::new(600, 450));
        // The window's first pixel maps where the original first pixel did.
        let a = sensor(&e, 50.5, 0.5);
        let b = sensor(&g, 0.5, 0.5);
        assert!((a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9);
        assert!((e.origin_out[0] - 50.0).abs() < 1e-3);
    }

    #[test]
    fn keystone_direction_widens_the_top_and_the_right() {
        let r = raw(3000, 2000, Orientation::Normal);
        let cap = CaptureInfo {
            focal_length: Some(24.0),
            ..CaptureInfo::default()
        };
        let geo = |v: f32, h: f32| {
            let mut adj = Adjustments::default();
            adj.lens.keystone_vertical = v;
            adj.lens.keystone_horizontal = h;
            adj.lens.enabled = false;
            plan(
                &r,
                &adj,
                &cap,
                None,
                Size::new(10000, 10000),
                Fit::ConstrainToSensorPixels,
            )
            .unwrap()
        };
        // Sensor span sampled by an output row / column: a narrower source span shown at
        // the same output width means that side is widened.
        let row_span = |g: &Geometry, yf: f64| {
            let (w, y) = (f64::from(g.out.width), yf * f64::from(g.out.height));
            sensor(g, w * 0.9, y).0 - sensor(g, w * 0.1, y).0
        };
        let col_span = |g: &Geometry, xf: f64| {
            let (h, x) = (f64::from(g.out.height), xf * f64::from(g.out.width));
            sensor(g, x, h * 0.9).1 - sensor(g, x, h * 0.1).1
        };
        for v in [5.0, 15.0] {
            let g = geo(v, 0.0);
            let (top, bot) = (row_span(&g, 0.05), row_span(&g, 0.95));
            assert!(
                top < bot * 0.98,
                "+{v} vertical must widen the top: {top} {bot}"
            );
            let g = geo(-v, 0.0);
            assert!(
                row_span(&g, 0.05) > row_span(&g, 0.95) * 1.02,
                "-{v} narrows the top"
            );
            let g = geo(0.0, v);
            let (left, right) = (col_span(&g, 0.05), col_span(&g, 0.95));
            assert!(
                right < left * 0.98,
                "+{v} horizontal must widen the right: {left} {right}"
            );
        }
    }

    #[test]
    fn keystone_is_projective_and_centered() {
        let r = raw(3000, 2000, Orientation::Normal);
        let mut adj = Adjustments::default();
        adj.lens.keystone_vertical = 10.0;
        let g = plan(
            &r,
            &adj,
            &CaptureInfo {
                focal_length: Some(24.0),
                ..CaptureInfo::default()
            },
            None,
            Size::new(10000, 10000),
            Fit::ConstrainToSensorPixels,
        )
        .unwrap();
        assert!(g.keystone.is_some());
        // The image center stays at the center.
        let (cx, cy) = (f64::from(g.out.width) / 2.0, f64::from(g.out.height) / 2.0);
        let (sx, sy) = sensor(&g, cx, cy);
        assert!(
            (sx - 1500.0).abs() < 1.0 && (sy - 1000.0).abs() < 1.0,
            "{sx} {sy}"
        );
        // Top and bottom rows are scaled differently (perspective).
        let top = sensor(&g, f64::from(g.out.width), 0.0).0 - sensor(&g, 0.0, 0.0).0;
        let bot = sensor(&g, f64::from(g.out.width), f64::from(g.out.height)).0
            - sensor(&g, 0.0, f64::from(g.out.height)).0;
        assert!((top - bot).abs() > 10.0, "{top} {bot}");
        let h = keystone_homography(0.0, 0.0, 1000.0, [0.0, 0.0]);
        let (x, y) = homography(&h, 12.0, -7.0);
        assert!((x - 12.0).abs() < 1e-9 && (y + 7.0).abs() < 1e-9);
        let degenerate = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
        assert!(homography(&degenerate, 1.0, 1.0).0.is_finite());
    }

    #[test]
    fn lens_profile_distortion_ca_and_vignetting() {
        let r = raw(6000, 4000, Orientation::Normal);
        let cap = CaptureInfo {
            lens: Some("FE 24-70mm F2.8 GM".into()),
            focal_length: Some(24.0),
            aperture: Some(2.8),
            iso: Some(100),
            crop_factor: None,
            camera: None,
        };
        let adj = Adjustments::default();
        let g = plan(
            &r,
            &adj,
            &cap,
            None,
            Size::new(10000, 10000),
            Fit::ConstrainToSensorPixels,
        )
        .unwrap();
        let l = g.lens.expect("profile found");
        assert!((l.r_dist - 2.0 / 4000.0).abs() < 1e-12);
        // On a 1.5× body a full-frame calibration is evaluated at 1/1.5 the radius.
        let apsc = CaptureInfo {
            crop_factor: Some(1.5),
            ..cap.clone()
        };
        let l2 = plan(
            &r,
            &adj,
            &apsc,
            None,
            Size::new(10000, 10000),
            Fit::ConstrainToSensorPixels,
        )
        .unwrap()
        .lens
        .unwrap();
        assert!((l2.r_dist * 1.5 / l.r_dist - 1.0).abs() < 1e-6);
        // A calibration on a 4:3 crop-2 sensor (Sigma DN on Micro Four Thirds): its short
        // side is 3/5 of the diagonal, ours (3:2) 2/√13, so the radius shrinks by
        // (2/√13)/(3/5) on top of the crop ratio; vignetting (diagonal units) doesn't.
        let mft = CaptureInfo {
            lens: Some("Sigma 60mm f/2.8 DN".into()),
            focal_length: Some(60.0),
            crop_factor: Some(2.0),
            ..cap.clone()
        };
        let p = crate::lens::find("Sigma 60mm f/2.8 DN", 2.0).unwrap();
        assert!((p.aspect - 4.0 / 3.0).abs() < 1e-6 && p.crop_factor == 2.0);
        let l3 = plan(
            &r,
            &adj,
            &mft,
            None,
            Size::new(10000, 10000),
            Fit::ConstrainToSensorPixels,
        )
        .unwrap()
        .lens
        .unwrap();
        let want = 2.0 / 4000.0 * (2.0 / 13f64.sqrt()) / 0.6;
        assert!(
            (l3.r_dist / want - 1.0).abs() < 1e-6,
            "{} vs {want}",
            l3.r_dist
        );
        assert!((l3.r_vig / (2.0 / 6000f64.hypot(4000.0)) - 1.0).abs() < 1e-9);
        // Center: no displacement, no vignetting gain.
        let (cx, cy) = (f64::from(g.out.width) / 2.0, f64::from(g.out.height) / 2.0);
        let (pos, gain) = g.sensor_at(cx, cy);
        assert!((pos[1][0] - 3000.0).abs() < 2.0 && (pos[1][1] - 2000.0).abs() < 2.0);
        assert!((gain - 1.0).abs() < 1e-3);
        // Corners: vignetting correction brightens, red and blue sample slightly apart.
        let (pos, gain) = g.sensor_at(1.0, 1.0);
        assert!(gain > 1.2, "{gain}");
        assert_ne!(pos[0], pos[2]);
        // Strength 0 / disabled / unknown lens / no focal → no lens stage.
        let mut off = Adjustments::default();
        off.lens.enabled = false;
        assert!(
            plan(
                &r,
                &off,
                &cap,
                None,
                Size::new(100, 100),
                Fit::ConstrainToSensorPixels
            )
            .unwrap()
            .lens
            .is_none()
        );
        let unknown = CaptureInfo {
            lens: Some("Nope 1mm".into()),
            ..cap.clone()
        };
        assert!(
            plan(
                &r,
                &adj,
                &unknown,
                None,
                Size::new(100, 100),
                Fit::ConstrainToSensorPixels
            )
            .unwrap()
            .lens
            .is_none()
        );
        let nofocal = CaptureInfo {
            focal_length: None,
            ..cap.clone()
        };
        assert!(
            plan(
                &r,
                &adj,
                &nofocal,
                None,
                Size::new(100, 100),
                Fit::ConstrainToSensorPixels
            )
            .unwrap()
            .lens
            .is_none()
        );
        let mut zero = Adjustments::default();
        zero.lens.distortion = 0.0;
        zero.lens.light_falloff = 0.0;
        zero.lens.chromatic_aberration = false;
        assert!(
            plan(
                &r,
                &zero,
                &cap,
                None,
                Size::new(100, 100),
                Fit::ConstrainToSensorPixels
            )
            .unwrap()
            .lens
            .is_none()
        );
        // A profile override wins over EXIF.
        let mut ov = Adjustments::default();
        ov.lens.profile = Some("FE 24-70mm f/2.8 GM".into());
        let anon = CaptureInfo { lens: None, ..cap };
        assert!(
            plan(
                &r,
                &ov,
                &anon,
                None,
                Size::new(100, 100),
                Fit::ConstrainToSensorPixels
            )
            .unwrap()
            .lens
            .is_some()
        );
        let o = g.optics_f32();
        assert!(o.has_lens && !o.has_keystone);
    }
}
