//! Monotone cubic curves and the per-render lookup tables that fold the whole global
//! tone chain (base filmic curve → whites/blacks → contrast → brightness → levels →
//! RGB curve → per-channel curves) into one table per channel.

use std::sync::{Arc, OnceLock};

use ds_core::adjust::{Curve, Exposure, Hdr, Levels};

use crate::color::{oklab_to_rec2020, rec2020_to_oklab, srgb_decode, srgb_encode};
use crate::tone::{self, MID_GRAY};

/// Monotone piecewise-cubic Hermite interpolant (Fritsch–Carlson). Never overshoots the
/// data, so a monotone set of control points gives a monotone curve.
#[derive(Debug, Clone, PartialEq)]
pub struct MonotoneCubic {
    xs: Vec<f32>,
    ys: Vec<f32>,
    ms: Vec<f32>,
}

impl MonotoneCubic {
    /// Build from points (any order; duplicates in x keep the last). `None` for < 2 points.
    pub fn new(points: &[[f32; 2]]) -> Option<Self> {
        let mut pts: Vec<[f32; 2]> = points
            .iter()
            .copied()
            .filter(|p| p[0].is_finite() && p[1].is_finite())
            .collect();
        pts.sort_by(|a, b| a[0].total_cmp(&b[0]));
        let mut clean: Vec<[f32; 2]> = Vec::with_capacity(pts.len());
        for p in pts {
            match clean.last_mut() {
                Some(last) if (p[0] - last[0]).abs() < 1e-6 => *last = p,
                _ => clean.push(p),
            }
        }
        if clean.len() < 2 {
            return None;
        }
        let n = clean.len();
        let xs: Vec<f32> = clean.iter().map(|p| p[0]).collect();
        let ys: Vec<f32> = clean.iter().map(|p| p[1]).collect();
        let d: Vec<f32> = (0..n - 1)
            .map(|k| (ys[k + 1] - ys[k]) / (xs[k + 1] - xs[k]))
            .collect();
        let mut ms = vec![0.0f32; n];
        ms[0] = d[0];
        ms[n - 1] = d[n - 2];
        for k in 1..n - 1 {
            ms[k] = if d[k - 1] * d[k] > 0.0 {
                f32::midpoint(d[k - 1], d[k])
            } else {
                0.0
            };
        }
        for k in 0..n - 1 {
            if d[k] == 0.0 {
                ms[k] = 0.0;
                ms[k + 1] = 0.0;
                continue;
            }
            let a = ms[k] / d[k];
            let b = ms[k + 1] / d[k];
            if a < 0.0 {
                ms[k] = 0.0;
            }
            if b < 0.0 {
                ms[k + 1] = 0.0;
            }
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                ms[k] = t * a * d[k];
                ms[k + 1] = t * b * d[k];
            }
        }
        Some(Self { xs, ys, ms })
    }

    /// A 0..=1 editing curve: empty = `None` (identity); implicit (0,0)/(1,1) endpoints are
    /// added when the user's points don't reach the edges.
    pub fn from_curve_points(points: &[[f32; 2]]) -> Option<Self> {
        if points.is_empty() {
            return None;
        }
        let mut pts: Vec<[f32; 2]> = points
            .iter()
            .map(|p| [p[0].clamp(0.0, 1.0), p[1].clamp(0.0, 1.0)])
            .collect();
        if !pts.iter().any(|p| p[0] <= 1e-6) {
            pts.push([0.0, 0.0]);
        }
        if !pts.iter().any(|p| p[0] >= 1.0 - 1e-6) {
            pts.push([1.0, 1.0]);
        }
        Self::new(&pts)
    }

    /// Evaluate; flat extension outside the control range.
    pub fn eval(&self, x: f32) -> f32 {
        let n = self.xs.len();
        if x.is_nan() || x <= self.xs[0] {
            return self.ys[0];
        }
        if x >= self.xs[n - 1] {
            return self.ys[n - 1];
        }
        let k = self.xs.partition_point(|&v| v <= x) - 1;
        let h = self.xs[k + 1] - self.xs[k];
        let t = (x - self.xs[k]) / h;
        let (t2, t3) = (t * t, t * t * t);
        let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
        let h10 = t3 - 2.0 * t2 + t;
        let h01 = -2.0 * t3 + 3.0 * t2;
        let h11 = t3 - t2;
        h00 * self.ys[k] + h10 * h * self.ms[k] + h01 * self.ys[k + 1] + h11 * h * self.ms[k + 1]
    }
}

// ------------------------------------------------------------- base curve ---

/// Control points of the default base curve: (stops relative to middle gray,
/// display-encoded value). A gentle film-like toe and a long shoulder that reaches
/// white ~6 stops over middle gray, so reconstructed highlights still have room.
const BASE_CURVE_POINTS: [[f32; 2]; 18] = [
    [-10.5, 0.0],
    [-9.0, 0.001],
    [-8.0, 0.003],
    [-7.0, 0.007],
    [-6.0, 0.012],
    [-5.0, 0.025],
    [-4.0, 0.052],
    [-3.0, 0.1],
    [-2.0, 0.18],
    [-1.0, 0.31],
    [0.0, 0.46],
    [1.0, 0.63],
    [2.0, 0.79],
    [3.0, 0.915],
    [3.5, 0.95],
    [4.0, 0.975],
    [5.0, 0.995],
    [6.0, 1.0],
];

/// A base tone curve: scene stops relative to middle gray → display-encoded 0..=1.
#[derive(Debug, Clone, PartialEq)]
pub enum BaseCurve {
    /// Monotone spline through (stops, encoded) points, flat beyond its ends (the
    /// Darkslide profiles).
    Stops(MonotoneCubic),
    /// A camera profile's tone curve (DCP `ProfileToneCurve`): scene-linear 0..=1 →
    /// linear 0..=1, scene linear 1.0 being the curve's 1.0 (clipped above).
    Linear(MonotoneCubic),
}

impl BaseCurve {
    /// From (stops, encoded) control points; `None` for fewer than two.
    pub fn stops(points: &[[f32; 2]]) -> Option<Self> {
        MonotoneCubic::new(points).map(Self::Stops)
    }

    /// From a linear → linear curve's (x, y) points; `None` for fewer than two.
    pub fn linear(points: &[[f32; 2]]) -> Option<Self> {
        MonotoneCubic::new(points).map(Self::Linear)
    }

    /// Display-encoded value for scene `stops` relative to middle gray.
    pub fn eval(&self, stops: f32) -> f32 {
        match self {
            Self::Stops(c) => c.eval(stops),
            Self::Linear(c) => {
                let x = (MID_GRAY * stops.exp2()).clamp(0.0, 1.0);
                srgb_encode(c.eval(x).clamp(0.0, 1.0))
            }
        }
    }

    /// Display-encoded value of scene middle gray (the contrast pivot).
    pub fn pivot(&self) -> f32 {
        self.eval(0.0)
    }

    /// Stops range over which the curve is not flat (the inverse's search interval).
    fn domain(&self) -> (f32, f32) {
        match self {
            Self::Stops(c) => (c.xs[0], c.xs[c.xs.len() - 1]),
            Self::Linear(_) => (LOG_MIN - MID_GRAY.log2(), -MID_GRAY.log2()),
        }
    }
}

/// The Neutral profile's base curve (the original look).
pub fn neutral_curve() -> &'static BaseCurve {
    static S: OnceLock<BaseCurve> = OnceLock::new();
    S.get_or_init(|| BaseCurve::stops(&BASE_CURVE_POINTS).expect("base curve"))
}

/// The Neutral base tone curve: stops relative to middle gray → display-encoded 0..=1.
pub fn base_curve(stops: f32) -> f32 {
    neutral_curve().eval(stops)
}

/// Display-encoded value of scene middle gray under the Neutral base curve.
pub fn base_pivot() -> f32 {
    base_curve(0.0)
}

// -------------------------------------------------------------------- LUTs ---

/// Scene-linear LUT domain, log2.
pub const LOG_MIN: f32 = -16.0;
pub const LOG_MAX: f32 = 8.0;
/// Entries in a scene-domain (log2) table.
pub const TONE_N: usize = 4096;
/// Entries in an encoded-domain (0..=1) table.
pub const ENC_N: usize = 1024;

/// Look up a scene-linear value in a log2-domain table (linear interpolation).
/// Non-positive / NaN / tiny inputs return the first entry.
#[inline]
pub fn lookup_log(table: &[f32], x: f32) -> f32 {
    const MIN_LIN: f32 = 1.525_878_9e-5; // 2^-16
    if x.is_nan() || x <= MIN_LIN {
        return table[0];
    }
    let n = table.len();
    let pos = ((x.log2() - LOG_MIN) * ((n - 1) as f32 / (LOG_MAX - LOG_MIN))).min((n - 1) as f32);
    interp(table, pos)
}

/// Look up an encoded 0..=1 value (clamped) in an encoded-domain table.
#[inline]
pub fn lookup_enc(table: &[f32], y: f32) -> f32 {
    let n = table.len();
    let pos = if y.is_nan() {
        0.0
    } else {
        y.clamp(0.0, 1.0) * (n - 1) as f32
    };
    interp(table, pos)
}

#[inline]
fn interp(table: &[f32], pos: f32) -> f32 {
    let i = (pos as usize).min(table.len() - 2);
    let f = pos - i as f32;
    table[i] + (table[i + 1] - table[i]) * f
}

fn log_domain(i: usize) -> f32 {
    LOG_MIN + (LOG_MAX - LOG_MIN) * i as f32 / (TONE_N - 1) as f32
}

/// Tone settings that feed the global LUTs.
#[derive(Debug, Clone, PartialEq)]
pub struct ToneSettings {
    /// The profile's base curve.
    pub base: Arc<BaseCurve>,
    pub exposure: Exposure,
    pub hdr: Hdr,
    pub levels: Levels,
    pub curve: Curve,
}

/// All per-render tables.
#[derive(Debug, Clone, PartialEq)]
pub struct ToneLuts {
    /// Scene-linear (log2 domain) → display-encoded: base curve with whites/blacks. Applied
    /// to the max-RGB norm with RGB ratios kept (hue preserving), see [`tone_rgb`].
    pub base: Vec<f32>,
    /// Encoded → encoded per channel R, G, B: contrast, brightness, levels, RGB and
    /// channel curves (user edits, per channel as in other editors). `None` = identity.
    pub chan: Option<[Vec<f32>; 3]>,
    /// Encoded luma → encoded luma (luma curve); `None` when identity.
    pub luma: Option<Vec<f32>>,
}

/// Tone mapping of scene-linear RGB with the base table, hue preserved: lightness and
/// chroma come from the per-channel curve (its natural roll-off desaturates bright colors
/// toward white, like film and camera JPEGs), the OKLab hue from the ratio-preserving
/// mapping (curve on max(R, G, B), RGB ratios kept), so skies and skin don't drift toward
/// cyan/yellow as exposure rises. Returns display-encoded RGB.
#[inline]
pub fn tone_rgb(rgb: [f32; 3], base: &[f32]) -> [f32; 3] {
    const MIN_LIN: f32 = 1.525_878_9e-5;
    let pc = rgb.map(|v| lookup_log(base, v));
    let n = rgb[0].max(rgb[1]).max(rgb[2]);
    if n.is_nan() || n <= MIN_LIN {
        return pc;
    }
    let k = srgb_decode(lookup_log(base, n)) / n;
    let rp = rec2020_to_oklab(rgb.map(|v| v.max(0.0) * k));
    let lab = rec2020_to_oklab(pc.map(srgb_decode));
    let (c_pc, c_rp) = (lab[1].hypot(lab[2]), rp[1].hypot(rp[2]));
    if c_pc < 1e-4 || c_rp < 1e-4 {
        return pc;
    }
    let f = c_pc / c_rp;
    oklab_to_rec2020([lab[0], rp[1] * f, rp[2] * f]).map(|v| srgb_encode(v.max(0.0)))
}

/// Inverse of [`tone_rgb`] (per channel through the base curve's inverse table, plus one
/// multiplicative correction step for the hue restoration), used by layers to re-expose.
#[inline]
pub fn untone_rgb(e: [f32; 3], fwd: &[f32], inv: &[f32]) -> [f32; 3] {
    let x0 = e.map(|v| lookup_enc(inv, v));
    let back = tone_rgb(x0, fwd).map(|v| lookup_enc(inv, v));
    [0, 1, 2].map(|c| {
        if back[c] > 1e-9 {
            x0[c] * x0[c] / back[c]
        } else {
            x0[c]
        }
    })
}

impl ToneLuts {
    pub fn build(s: &ToneSettings) -> Self {
        let rgb = MonotoneCubic::from_curve_points(&s.curve.rgb);
        let per = [&s.curve.red, &s.curve.green, &s.curve.blue]
            .map(|p| MonotoneCubic::from_curve_points(p));
        let wf = tone::whites_factor(s.hdr.whites);
        let k = tone::contrast_k(s.exposure.contrast);
        let pivot = s.base.pivot();
        let g = tone::brightness_gamma(s.exposure.brightness);
        let base: Vec<f32> = (0..TONE_N)
            .map(|i| {
                let st = log_domain(i) - MID_GRAY.log2();
                let st = if st > 0.0 { st * wf } else { st };
                tone::blacks_toe(s.base.eval(st), s.hdr.blacks)
            })
            .collect();
        let identity = (k - 1.0).abs() < 1e-6
            && (g - 1.0).abs() < 1e-6
            && s.levels == Levels::default()
            && rgb.is_none()
            && per.iter().all(Option::is_none);
        let chan = (!identity).then(|| {
            let shared: Vec<f32> = (0..ENC_N)
                .map(|i| {
                    let mut y = i as f32 / (ENC_N - 1) as f32;
                    y = tone::contrast(y, k, pivot);
                    y = tone::brightness(y, g);
                    y = tone::levels(y, &s.levels);
                    if let Some(c) = &rgb {
                        y = c.eval(y);
                    }
                    y
                })
                .collect();
            per.map(|c| match c {
                Some(c) => shared.iter().map(|&y| c.eval(y)).collect(),
                None => shared.clone(),
            })
        });
        let luma = MonotoneCubic::from_curve_points(&s.curve.luma).map(|c| {
            (0..ENC_N)
                .map(|i| c.eval(i as f32 / (ENC_N - 1) as f32))
                .collect()
        });
        Self { base, chan, luma }
    }
}

/// Tables for a layer's re-toning: the base curve forward (scene log2 domain → encoded)
/// and its inverse (encoded → scene linear).
#[derive(Debug, Clone, PartialEq)]
pub struct BaseLuts {
    pub fwd: Vec<f32>,
    pub inv: Vec<f32>,
}

impl BaseLuts {
    /// Forward and inverse tables of `curve`.
    pub fn new(curve: &BaseCurve) -> Self {
        let fwd: Vec<f32> = (0..TONE_N)
            .map(|i| curve.eval(log_domain(i) - MID_GRAY.log2()))
            .collect();
        let (lo, hi) = curve.domain();
        let inv = (0..ENC_N)
            .map(|i| {
                let y = i as f32 / (ENC_N - 1) as f32;
                // Bisection on the monotone base curve.
                let (mut a, mut b) = (lo, hi);
                for _ in 0..40 {
                    let m = f32::midpoint(a, b);
                    if curve.eval(m) < y {
                        a = m;
                    } else {
                        b = m;
                    }
                }
                MID_GRAY * f32::midpoint(a, b).exp2()
            })
            .collect();
        Self { fwd, inv }
    }
}

/// The Neutral curve's tables.
pub fn base_luts() -> &'static BaseLuts {
    static B: OnceLock<BaseLuts> = OnceLock::new();
    B.get_or_init(|| BaseLuts::new(neutral_curve()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_monotone(v: &[f32]) -> bool {
        v.windows(2).all(|w| w[1] >= w[0] - 1e-6)
    }

    #[test]
    fn monotone_cubic_interpolates_and_never_overshoots() {
        let c = MonotoneCubic::new(&[[0.0, 0.0], [0.3, 0.1], [0.5, 0.9], [1.0, 1.0]]).unwrap();
        assert!((c.eval(0.3) - 0.1).abs() < 1e-6);
        assert!((c.eval(0.5) - 0.9).abs() < 1e-6);
        let samples: Vec<f32> = (0..=1000).map(|i| c.eval(i as f32 / 1000.0)).collect();
        assert!(is_monotone(&samples));
        assert!(samples.iter().all(|v| (0.0..=1.0).contains(v)));
        // Flat extension and NaN.
        assert_eq!(c.eval(-1.0), 0.0);
        assert_eq!(c.eval(2.0), 1.0);
        assert_eq!(c.eval(f32::NAN), 0.0);
    }

    #[test]
    fn monotone_cubic_edge_cases() {
        assert!(MonotoneCubic::new(&[[0.5, 0.5]]).is_none());
        assert!(MonotoneCubic::new(&[]).is_none());
        // Duplicates collapse; flat segments stay flat; NaN points are dropped.
        let c = MonotoneCubic::new(&[
            [0.0, 0.2],
            [0.5, 0.2],
            [0.5, 0.2],
            [1.0, 0.8],
            [0.7, f32::NAN],
        ])
        .unwrap();
        assert!((c.eval(0.25) - 0.2).abs() < 1e-6);
        // Decreasing curves are handled too.
        let d = MonotoneCubic::new(&[[0.0, 1.0], [1.0, 0.0]]).unwrap();
        assert!((d.eval(0.5) - 0.5).abs() < 1e-6);
        // Steep data triggers the Fritsch–Carlson limiter and stays monotone.
        let s = MonotoneCubic::new(&[[0.0, 0.0], [0.1, 0.0], [0.2, 1.0], [0.3, 1.0], [1.0, 1.0]])
            .unwrap();
        let v: Vec<f32> = (0..=300).map(|i| s.eval(i as f32 / 300.0)).collect();
        assert!(is_monotone(&v));
        // Non-monotone data: sign change sets a zero tangent (no overshoot above the peak).
        let p = MonotoneCubic::new(&[[0.0, 0.0], [0.5, 1.0], [1.0, 0.0]]).unwrap();
        assert!((0..=100).all(|i| p.eval(i as f32 / 100.0) <= 1.0 + 1e-6));
    }

    #[test]
    fn curve_points_get_implicit_endpoints() {
        assert!(MonotoneCubic::from_curve_points(&[]).is_none());
        let c = MonotoneCubic::from_curve_points(&[[0.5, 0.7]]).unwrap();
        assert!(c.eval(0.0).abs() < 1e-6 && (c.eval(1.0) - 1.0).abs() < 1e-6);
        assert!((c.eval(0.5) - 0.7).abs() < 1e-6);
        let v: Vec<f32> = (0..=100).map(|i| c.eval(i as f32 / 100.0)).collect();
        assert!(is_monotone(&v));
    }

    #[test]
    fn base_curve_shape() {
        assert!((base_pivot() - 0.46).abs() < 1e-3);
        assert_eq!(base_curve(-20.0), 0.0);
        assert_eq!(base_curve(20.0), 1.0);
        let v: Vec<f32> = (-110..=70).map(|i| base_curve(i as f32 / 10.0)).collect();
        assert!(is_monotone(&v));
    }

    #[test]
    fn lookups() {
        let t: Vec<f32> = (0..TONE_N).map(|i| i as f32).collect();
        assert_eq!(lookup_log(&t, 0.0), 0.0);
        assert_eq!(lookup_log(&t, -1.0), 0.0);
        assert_eq!(lookup_log(&t, f32::NAN), 0.0);
        assert!((lookup_log(&t, 1.0) - 16.0 / 24.0 * 4095.0).abs() < 0.01);
        assert_eq!(lookup_log(&t, 1e30), 4095.0);
        let e: Vec<f32> = (0..ENC_N).map(|i| i as f32 / (ENC_N - 1) as f32).collect();
        assert!((lookup_enc(&e, 0.25) - 0.25).abs() < 1e-6);
        assert_eq!(lookup_enc(&e, 2.0), 1.0);
        assert_eq!(lookup_enc(&e, f32::NAN), 0.0);
    }

    fn settings() -> ToneSettings {
        ToneSettings {
            base: Arc::new(neutral_curve().clone()),
            exposure: Exposure::default(),
            hdr: Hdr::default(),
            levels: Levels::default(),
            curve: Curve::default(),
        }
    }

    #[test]
    fn identity_luts_follow_base_curve() {
        let l = ToneLuts::build(&settings());
        assert!(l.luma.is_none());
        assert!(l.chan.is_none(), "identity user tone has no channel tables");
        assert!((lookup_log(&l.base, MID_GRAY) - base_pivot()).abs() < 1e-3);
        assert!(is_monotone(&l.base));
    }

    #[test]
    fn all_settings_keep_luts_monotone() {
        let mut s = settings();
        s.exposure.contrast = 60.0;
        s.exposure.brightness = -30.0;
        s.hdr.whites = 50.0;
        s.hdr.blacks = -40.0;
        s.levels = Levels {
            in_black: 0.05,
            in_white: 0.95,
            gamma: 1.2,
            out_black: 0.0,
            out_white: 1.0,
        };
        s.curve.rgb = vec![[0.25, 0.2], [0.75, 0.8]];
        s.curve.red = vec![[0.5, 0.55]];
        s.curve.luma = vec![[0.5, 0.6]];
        let l = ToneLuts::build(&s);
        let chan = l.chan.as_ref().unwrap();
        for c in chan {
            assert!(is_monotone(c));
        }
        assert_ne!(chan[0], chan[1]);
        assert!(is_monotone(&l.base));
        assert!(is_monotone(l.luma.as_ref().unwrap()));
        // Whites > 0 brightens highlights.
        let mut w = settings();
        w.hdr.whites = 50.0;
        let bright = lookup_log(&ToneLuts::build(&w).base, 1.0);
        let plain = lookup_log(&ToneLuts::build(&settings()).base, 1.0);
        assert!(bright > plain);
    }

    #[test]
    fn tone_rgb_preserves_hue_and_rolls_off_highlights() {
        use crate::color_edit::encoded_to_oklch;
        let l = ToneLuts::build(&settings());
        // Gray maps like the curve.
        let g = tone_rgb([MID_GRAY; 3], &l.base);
        assert!((g[0] - base_pivot()).abs() < 1e-3 && g[0] == g[2]);
        // A saturated sky blue keeps its hue at 0 and +2 EV (per-channel curves shift it).
        let sky = [0.05f32, 0.12, 0.3];
        let h0 = encoded_to_oklch(tone_rgb(sky, &l.base))[2];
        let h2 = encoded_to_oklch(tone_rgb(sky.map(|v| v * 4.0), &l.base))[2];
        assert!((h0 - h2).abs() < 3.0, "{h0} vs {h2}");
        // Very bright colors desaturate toward white instead of clipping one channel.
        let hot = tone_rgb([40.0, 10.0, 5.0], &l.base);
        assert!(hot.iter().all(|v| *v <= 1.005) && hot[2] > 0.9, "{hot:?}");
        // Black / NaN input: the curve's floor.
        assert_eq!(tone_rgb([0.0; 3], &l.base), [l.base[0]; 3]);
        assert_eq!(tone_rgb([f32::NAN; 3], &l.base), [l.base[0]; 3]);
        // Neutral stays neutral exactly (no chroma to borrow a hue from).
        let n = tone_rgb([0.3; 3], &l.base);
        assert!(n[0] == n[1] && n[1] == n[2]);
        // untone ∘ tone ≈ identity away from the highlight roll-off.
        let b = base_luts();
        let x = [0.1f32, 0.2, 0.05];
        let back = untone_rgb(tone_rgb(x, &b.fwd), &b.fwd, &b.inv);
        for c in 0..3 {
            assert!((back[c] - x[c]).abs() / x[c] < 0.01, "{x:?} → {back:?}");
        }
        assert!(
            untone_rgb([0.0; 3], &b.fwd, &b.inv)
                .iter()
                .all(|v| *v < 1e-3)
        );
    }

    #[test]
    fn base_luts_invert_each_other() {
        let b = base_luts();
        for y in [0.05f32, 0.2, 0.46, 0.7, 0.9] {
            let x = lookup_enc(&b.inv, y);
            let back = lookup_log(&b.fwd, x);
            assert!((back - y).abs() < 2e-3, "{y} → {x} → {back}");
        }
    }
}
