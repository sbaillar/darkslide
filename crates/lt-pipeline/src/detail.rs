//! Clarity/structure (local contrast at two radii), noise reduction (à-trous wavelet
//! shrinkage on luma + luma-guided chroma smoothing) and unsharp-mask sharpening.
//! Per-pixel math lives here; the passes are orchestrated by the renderers.

use crate::color::REC2020_LUMA;

/// Weight that fades local-contrast changes out near black and white (avoids clipping).
#[inline]
pub fn midtone_weight(y: f32) -> f32 {
    let t = (2.0 * y.clamp(0.0, 1.0) - 1.0).abs();
    1.0 - t * t * t * t
}

/// Detail amplitude (encoded luma) above which clarity/structure boosts less: large
/// differences are edges, not texture, and boosting them is what makes halos.
pub const DETAIL_KNEE: f32 = 0.015;

/// Soft limiter on a detail value: linear for texture, rolling off for edge-sized swings.
#[inline]
pub fn soft_detail(d: f32) -> f32 {
    let t = d / DETAIL_KNEE;
    d / (1.0 + t * t)
}

/// Luma change from clarity/structure given the detail layers `y − smooth(y)`.
#[inline]
pub fn local_contrast_delta(
    y: f32,
    det_clarity: f32,
    det_structure: f32,
    clarity: f32,
    structure: f32,
) -> f32 {
    midtone_weight(y)
        * (clarity * soft_detail(det_clarity) + structure * soft_detail(det_structure))
}

/// Texture: detail amplitude (encoded luma) where its soft limiter rolls off. Texture is
/// fine detail of a few percent; clarity's knee is for larger swings.
pub const TEXTURE_KNEE: f32 = 0.05;
/// Texture: the fine band is multiplied by up to `1 + BOOST` at +100.
pub const TEXTURE_BOOST: f32 = 1.75;
/// Texture: guided-filter ε of its plane, ~(10 %)²: texture-sized contrast is detail,
/// only real edges stay in the smooth layer (a near non-edge-preserving band-pass).
pub const TEXTURE_EPS: f32 = 0.01;
/// Layer sharpening: kernel σ as a share of the long edge (~0.9 px on a 6000 px frame),
/// so a fit view shows what the export will have instead of 8× the effect.
pub const FINE_SIGMA: f32 = 0.00015;
/// Detail noise (encoded luma at 1:1) per unit of measured raw noise: texture and layer
/// sharpening boost detail below it progressively less.
pub const DETAIL_NOISE_PER_RAW: f32 = 3.0;

/// Luma change from texture given its (fine) detail layer `y − smooth(y)`: positive
/// boosts it by up to `1 + TEXTURE_BOOST`, leaving detail below the noise `noise`
/// ([`sharpen_threshold`]); negative removes it.
#[inline]
pub fn texture_delta(y: f32, det_texture: f32, texture: f32, noise: f32) -> f32 {
    midtone_weight(y) * texture_gain(det_texture, texture, noise)
}

/// [`texture_delta`] without the midtone weight.
#[inline]
pub fn texture_gain(det: f32, k: f32, noise: f32) -> f32 {
    if k == 0.0 {
        return 0.0;
    }
    let (d, g) = if k > 0.0 {
        (sharpen_threshold(det, noise), k * TEXTURE_BOOST)
    } else {
        (det, k)
    };
    let t = d / TEXTURE_KNEE;
    g * d / (1.0 + t * t)
}

/// Layer sharpening from the fine detail (`y − gauss(y)`): positive up to 2.5×, above
/// the noise `noise`; negative removes it.
#[inline]
pub fn sharpen_delta(fine: f32, k: f32, noise: f32) -> f32 {
    if k > 0.0 {
        1.5 * k * sharpen_threshold(fine, noise)
    } else {
        k * fine
    }
}

/// The smallest kernel σ (output pixels) the 5-tap layer sharpening uses.
pub const FINE_MIN_SIGMA: f32 = 0.5;

/// Layer sharpening at `sigma` output pixels (the normalized kernel at this scale):
/// the 5-tap weights of the kernel used, and a gain on its detail. Below
/// [`FINE_MIN_SIGMA`] (a downscaled view) the smallest kernel stands in with the gain
/// `σ / σ_min`, so the view shows about what the downsampled export has instead of 8×
/// it (SKB06204 +100: fit +22% high-frequency RMS, export downsampled +17%).
pub fn fine_kernel(sigma: f32) -> ([f32; 3], f32) {
    let s = sigma.max(0.0);
    let eff = s.max(FINE_MIN_SIGMA);
    (fine_weights(eff), s / eff)
}

/// Weights (center, ±1, ±2) of a normalized 5-tap Gaussian of `sigma` pixels (≤ 1.2).
pub fn fine_weights(sigma: f32) -> [f32; 3] {
    let s = sigma.clamp(1e-3, 1.2);
    let g = |i: f32| (-(i * i) / (2.0 * s * s)).exp();
    let (w0, w1, w2) = (1.0, g(1.0), g(2.0));
    let n = w0 + 2.0 * (w1 + w2);
    [w0 / n, w1 / n, w2 / n]
}

/// Dark channel of a linear pixel (or block mean) relative to the atmospheric light `a`.
#[inline]
pub fn dark_channel(p: [f32; 3], a: [f32; 3]) -> f32 {
    (p[0] / a[0])
        .min(p[1] / a[1])
        .min(p[2] / a[2])
        .clamp(0.0, 1.5)
}

/// Luminance relative to the atmospheric light's: the guide of the haze estimate.
#[inline]
pub fn haze_guide(p: [f32; 3], a: [f32; 3]) -> f32 {
    luma3(p) / luma3(a).max(1e-6)
}

#[inline]
fn luma3(p: [f32; 3]) -> f32 {
    REC2020_LUMA[0] * p[0] + REC2020_LUMA[1] * p[1] + REC2020_LUMA[2] * p[2]
}

/// Dehaze: share of the haze removed at +100 (`t = 1 − ω·k·haze`).
pub const DEHAZE_OMEGA: f32 = 0.95;
/// Dehaze: the transmission never drops below `1 − (F0 + F1·haze)·k`: 0.4 at +100
/// where the haze estimate is 0, down to 0.15 where it is 1 (confident haze clears
/// harder), which bounds the amplification of noise and detail.
pub const DEHAZE_FLOOR: [f32; 2] = [0.6, 0.25];
/// Dehaze: the local-contrast part (Lightroom's dehaze has one): luminance relative to
/// its ~1%-of-the-long-edge neighbourhood mean is raised to `1 + LC·k·(0.3 + 0.7·haze)`,
/// so distant, flat areas gain punch, not just depth.
pub const DEHAZE_LC: f32 = 1.0;
/// Dehaze: bright-region protection `t ≥ K / |Y/Y_A − 1|` (He et al.'s sky rule): pixels
/// near the atmospheric light (clouds, white water, sky) move by at most ~`K·Y_A`.
pub const DEHAZE_BRIGHT: f32 = 0.12;
/// Dehaze: luminance (relative to the light's) over which bright neutral pixels become
/// protected.
pub const PROTECT_Y: [f32; 2] = [0.1, 0.4];
/// Dehaze: tint (largest channel deviation from the light's colour) over which that
/// protection fades out.
pub const PROTECT_TINT: [f32; 2] = [0.15, 0.6];
/// Dehaze: largest saturation gain at +100 (the hue is kept).
pub const DEHAZE_SAT: f32 = 0.25;
/// Dehaze: coefficient of variation of the guide (around a block) over which a region
/// stops counting as smooth (cloud-like) for the protection.
pub const PROTECT_CV: [f32; 2] = [0.1, 0.3];
/// Dehaze: OKLab chroma over which the saturation gain fades out.
pub const DEHAZE_VIVID: [f32; 2] = [0.02, 0.1];
/// Dehaze: never darkens a pixel below this share of its luminance (a safety net).
pub const DEHAZE_KEEP: f32 = 0.2;
/// Dehaze: the veil removed, `Y_A·(1 − t)`, is at most this share of the neighbourhood's
/// mean luminance (`t ≥ 1 − VEIL·mean/Y_A`): the neighbourhood keeps a common, bounded
/// subtraction, which raises its contrast, where a per-pixel floor would clip dark land
/// in a bright scene and only scale it down.
pub const DEHAZE_VEIL: f32 = 0.6;

/// Dehaze: ε of the self-guided filter giving the local-contrast mean (edge-aware, so a
/// cloud's edge doesn't darken the sky next to it).
pub const DEHAZE_LC_EPS: f32 = 0.03;

/// The neighbourhood of a pixel with guide `g` from the self-guided coefficients `(sa,
/// sb)` of the guide (ε [`DEHAZE_LC_EPS`], box-blurred): its smoothness (cloud-like,
/// 0..=1: the guide's coefficient of variation, hazed land keeps texture, a cloud is flat
/// at the ~1% patch scale) and its edge-aware mean `sa·g + sb`.
#[inline]
pub fn haze_local(sa: f32, sb: f32, g: f32) -> [f32; 2] {
    let sa = sa.clamp(0.0, 1.0);
    // var = ε·sa/(1 − sa), mean = sb/(1 − sa): cv = √(ε·sa·(1 − sa)) / sb.
    let cv = (DEHAZE_LC_EPS * sa * (1.0 - sa)).sqrt() / sb.max(1e-3);
    [
        1.0 - crate::tone::smoothstep(PROTECT_CV[0], PROTECT_CV[1], cv),
        sa * g + sb,
    ]
}

/// Share (0..=1) of a pixel's dehaze withheld because it looks like a cloud: bright
/// (relative to the light, luminance share `yn`), close to the light's colour, and in a
/// smooth neighbourhood (`smooth`, from [`haze_smoothness`]).
#[inline]
fn dehaze_protect(p: [f32; 3], a: [f32; 3], yn: f32, smooth: f32) -> f32 {
    let bright = crate::tone::smoothstep(PROTECT_Y[0], PROTECT_Y[1], yn) * smooth.clamp(0.0, 1.0);
    if bright <= 0.0 {
        return 0.0;
    }
    let tint = [0, 1, 2]
        .map(|c| (p[c] / a[c].max(1e-6) / yn - 1.0).abs())
        .into_iter()
        .fold(0.0f32, f32::max);
    bright * (1.0 - crate::tone::smoothstep(PROTECT_TINT[0], PROTECT_TINT[1], tint))
}

/// Dehaze one linear pixel. `hz` is the refined dark channel (0..=1) around it and the
/// neighbourhood's smoothness ([`haze_smoothness`]); `k` −1..=1.
///
/// Positive recovers the scene radiance on luminance, `Y_J = (Y − Y_A)/t + Y_A` with
/// `t = max(1 − ω·k·haze, 1 − FLOOR·k, K/|Y/Y_A − 1|)`, keeps the pixel's hue and lets
/// its saturation grow (in OKLab) as the full per-channel recovery would, by at most
/// `1 + SAT·k` and never out of sRGB. Smooth, bright pixels near the light's colour are
/// protected (clouds, white water: the prior can't tell them from haze), and nothing
/// gets darker than `KEEP` of its luminance.
/// Negative adds haze in proportion to its depth: `J = I + (A − I)·|k|·(0.1 + 0.3·haze)`
/// (at most 0.4 of the way to the atmospheric light).
#[inline]
pub fn dehaze_px(p: [f32; 3], hz: [f32; 3], a: [f32; 3], k: f32) -> [f32; 3] {
    let haze = hz[0].clamp(0.0, 1.0);
    if k <= 0.0 {
        let m = -k * (0.1 + 0.3 * haze);
        return [0, 1, 2].map(|c| p[c] + (a[c] - p[c]) * m);
    }
    let (y, ya) = (luma3(p), luma3(a).max(1e-6));
    if y <= 1e-9 {
        return p;
    }
    let yn = y / ya;
    let dy = (yn - 1.0).abs();
    let mean = if hz[2] > 1e-4 { hz[2] } else { yn };
    let t = (1.0 - DEHAZE_OMEGA * k * haze)
        .max(1.0 - (DEHAZE_FLOOR[0] + DEHAZE_FLOOR[1] * haze) * k)
        .max(1.0 - DEHAZE_VEIL * mean)
        .max((DEHAZE_BRIGHT / dy.max(1e-6)).min(1.0));
    // Bright pixels of about the light's colour (clouds, white water, snow) keep their
    // tone: the dark-channel prior can't tell them from haze.
    let protect = dehaze_protect(p, a, yn, hz[1]);
    let t = t + (1.0 - t) * protect;
    let mut yj = ((y - ya) / t + ya).max(DEHAZE_KEEP * y);
    // Local contrast against the neighbourhood mean of the guide (`hz[2]`).
    if hz[2] > 1e-4 {
        let e = DEHAZE_LC * k * (0.3 + 0.7 * haze) * (1.0 - protect);
        yj *= (yn / hz[2]).clamp(0.25, 4.0).powf(e);
    }
    // Saturation: the full per-channel recovery's, relative to the pixel's, bounded.
    let full = [0, 1, 2].map(|c| (p[c] - a[c]) / t + a[c]);
    let yf = luma3(full).max(1e-6);
    let sat = |v: [f32; 3], l: f32| v.iter().map(|x| (x / l - 1.0).abs()).fold(0.0f32, f32::max);
    let (s0, s1) = (sat(p, y), sat(full, yf));
    let g = if s0 > 1e-6 {
        (s1 / s0).clamp(1.0, 1.0 + DEHAZE_SAT * k)
    } else {
        1.0
    };
    // A pure scale: keeps the chromaticity (and any out-of-gamut component's sign).
    let j = p.map(|v| v * yj / y);
    if g <= 1.0 {
        return j;
    }
    // Saturate in OKLab (constant hue), muted colours most: boosting an already vivid
    // one (a deep sky) only pushes it out of the display gamut, where it changes hue.
    let lab = crate::color::rec2020_to_oklab(j);
    let vivid = crate::tone::smoothstep(DEHAZE_VIVID[0], DEHAZE_VIVID[1], lab[1].hypot(lab[2]));
    let g = 1.0 + (g - 1.0) * (1.0 - vivid);
    let boosted = |g: f32| crate::color::oklab_to_rec2020([lab[0], lab[1] * g, lab[2] * g]);
    // Nor out of the sRGB gamut: mapped back, it would change hue. Scale the boost back
    // (linearly in the lowest sRGB channel) to end at the gamut's edge.
    let m = crate::color::working_to_output(crate::OutputSpace::Srgb);
    let lowest = |v: [f32; 3]| {
        crate::color::mat_vec(&m, v)
            .into_iter()
            .fold(f32::MAX, f32::min)
    };
    let out = boosted(g);
    let (v1, vg) = (lowest(j), lowest(out));
    if vg >= 0.0 {
        out
    } else if v1 > 0.0 {
        boosted(1.0 + (g - 1.0) * v1 / (v1 - vg))
    } else {
        j
    }
}

/// Non-negative garrote shrinkage: zero below `t`, nearly unbiased well above it.
#[inline]
pub fn garrote(d: f32, t: f32) -> f32 {
    if t <= 0.0 {
        return d;
    }
    let a = d.abs();
    if a <= t { 0.0 } else { d - t * t / d }
}

/// Soft threshold used by sharpening: detail below `t` is attenuated smoothly.
#[inline]
pub fn sharpen_threshold(d: f32, t: f32) -> f32 {
    if t <= 0.0 {
        return d;
    }
    let a = d.abs();
    let w = (a / t).min(1.0);
    d * w * w
}

/// Rebuild RGB from a new luma `y` and chroma `(cb, cr) = (B − Y, R − Y)`.
#[inline]
pub fn from_luma_chroma(y: f32, cb: f32, cr: f32) -> [f32; 3] {
    let r = y + cr;
    let b = y + cb;
    let g = (y - REC2020_LUMA[0] * r - REC2020_LUMA[2] * b) / REC2020_LUMA[1];
    [r, g, b]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color_edit::luma;

    #[test]
    fn midtone_weight_shape() {
        assert_eq!(midtone_weight(0.5), 1.0);
        assert_eq!(midtone_weight(0.0), 0.0);
        assert_eq!(midtone_weight(1.0), 0.0);
        assert!(midtone_weight(0.25) > 0.9);
        // Small (texture) detail passes nearly linearly, edge-sized swings are limited.
        assert!((local_contrast_delta(0.5, 0.002, 0.0, 1.0, 0.0) - 0.002).abs() < 1e-4);
        assert!(local_contrast_delta(0.5, 0.1, 0.0, 1.0, 0.0) < 0.01);
        assert_eq!(local_contrast_delta(0.5, 0.0, 0.2, 1.0, 0.0), 0.0);
        assert!(soft_detail(-0.1) < 0.0 && soft_detail(0.1) > 0.0);
    }

    #[test]
    fn shrinkage_functions() {
        assert_eq!(garrote(0.01, 0.02), 0.0);
        assert!((garrote(0.1, 0.02) - (0.1 - 0.004)).abs() < 1e-6);
        assert!((garrote(-0.1, 0.02) + (0.1 - 0.004)).abs() < 1e-6);
        assert_eq!(garrote(0.3, 0.0), 0.3);
        assert_eq!(sharpen_threshold(0.5, 0.0), 0.5);
        assert_eq!(sharpen_threshold(0.5, 0.1), 0.5);
        assert!((sharpen_threshold(0.05, 0.1) - 0.0125).abs() < 1e-6);
    }

    fn hue(p: [f32; 3]) -> f32 {
        let l = crate::color::rec2020_to_oklab(p);
        l[2].atan2(l[1]).to_degrees()
    }

    #[test]
    fn dehaze_recovers_luminance_and_keeps_hue() {
        let a = [0.8, 0.85, 0.9];
        // Green land under a 50% veil, in a textured neighbourhood: darker, more
        // saturated, same hue.
        let p = [0.45, 0.575, 0.475];
        let j = dehaze_px(p, [0.5, 0.0, 0.0], a, 1.0);
        assert!(luma(j) < 0.8 * luma(p), "{j:?}");
        assert!((hue(j) - hue(p)).abs() < 1.0, "{} vs {}", hue(j), hue(p));
        let c = |v: [f32; 3]| {
            let l = crate::color::rec2020_to_oklab(v);
            l[1].hypot(l[2]) / l[0]
        };
        assert!(c(j) > c(p) * 1.1, "{} vs {}", c(j), c(p));
        // A dark pixel alone (its own neighbourhood): the veil is capped at VEIL of it,
        // never darker than DEHAZE_KEEP of the luminance.
        let dark = dehaze_px([0.05, 0.05, 0.05], [1.0, 0.0, 0.0], a, 1.0);
        let r = luma(dark) / 0.05;
        assert!(
            (DEHAZE_KEEP..1.0 - DEHAZE_VEIL * 0.9).contains(&r),
            "{dark:?}"
        );
        // In a brighter neighbourhood more veil comes off it (more contrast).
        let darker = dehaze_px([0.05, 0.05, 0.05], [1.0, 0.0, 0.3], a, 1.0);
        assert!(luma(darker) < luma(dark), "{darker:?}");
        // Zero haze, zero strength: identity.
        assert_eq!(dehaze_px(p, [0.0; 3], a, 0.0), p);
        let near = dehaze_px(p, [0.0; 3], a, 1e-6);
        assert!((0..3).all(|c| (near[c] - p[c]).abs() < 1e-4), "{near:?}");
        // Black stays black.
        assert_eq!(dehaze_px([0.0; 3], [1.0, 0.0, 0.0], a, 1.0), [0.0; 3]);
    }

    #[test]
    fn dehaze_protects_smooth_bright_neutrals_and_near_light_pixels() {
        let a = [0.8, 0.85, 0.9];
        // A grey cloud (half the light's luminance, its colour, smooth): kept...
        let cloud = a.map(|v| v * 0.5);
        let j = dehaze_px(cloud, [0.8, 1.0, 0.0], a, 1.0);
        assert!((luma(j) / luma(cloud) - 1.0).abs() < 0.02, "{j:?}");
        // ...but the same tone in a textured neighbourhood is hazed land: cleared.
        let j = dehaze_px(cloud, [0.8, 0.0, 0.0], a, 1.0);
        assert!(luma(j) < 0.8 * luma(cloud), "{j:?}");
        // Close to the light (white water): moves at most ~K of the light.
        let white = a.map(|v| v * 0.93);
        let j = dehaze_px(white, [1.0, 0.0, 0.0], a, 1.0);
        assert!(
            (luma(j) - luma(white)).abs() < DEHAZE_BRIGHT * luma(a),
            "{j:?}"
        );
        // A saturated blue of cloud luminance is not a cloud: it darkens, same hue.
        let blue = [0.25, 0.4, 0.95];
        let j = dehaze_px(blue, [0.5, 1.0, 0.0], a, 1.0);
        assert!(luma(j) < 0.85 * luma(blue), "{j:?}");
        assert!((hue(j) - hue(blue)).abs() < 1.0);
        // Smoothness from the coefficient of variation.
        assert_eq!(haze_local(0.0, 0.5, 0.7)[0], 1.0);
        assert_eq!(haze_local(0.5, 0.05, 0.7)[0], 0.0);
        // Edge-aware mean: at an edge (sa → 1) the pixel's own guide, in flat areas the
        // box mean.
        assert!((haze_local(0.5, 0.1, 0.4)[1] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn dehaze_saturation_stays_inside_srgb() {
        let a = [0.8, 0.85, 0.9];
        let m = crate::color::working_to_output(crate::OutputSpace::Srgb);
        // Hazed sRGB colours are boosted at most to the sRGB edge.
        let to_2020 = |s: [f32; 3]| {
            crate::color::mat_vec(
                &crate::color::xyz_to_rec2020(),
                crate::color::mat_vec(&crate::color::SRGB_TO_XYZ, s),
            )
        };
        for srgb in [[0.02, 0.3, 0.28], [0.3, 0.05, 0.4], [0.35, 0.3, 0.02]] {
            let p = to_2020(srgb).map(|v| v * 0.5 + 0.2);
            let j = dehaze_px(p, [0.6, 0.0, 0.0], a, 1.0);
            let s = crate::color::mat_vec(&m, j);
            assert!(s.iter().all(|v| *v >= -2e-3), "{srgb:?}: {s:?}");
        }
        // Already outside sRGB: no boost at all (a pure luminance scale).
        let out = [0.0, 0.3, 0.3];
        let j = dehaze_px(out, [0.6, 0.0, 0.0], a, 1.0);
        let r = j[1] / out[1];
        assert!(
            (j[2] / out[2] - r).abs() < 1e-4 && j[0].abs() < 1e-6,
            "{j:?}"
        );
    }

    /// Two pixels of one hazy neighbourhood (textured land): their contrast grows by
    /// more than the transmission alone would give (the local-contrast part).
    #[test]
    fn dehaze_raises_local_contrast() {
        let a = [0.8, 0.85, 0.9];
        let (hi, lo) = (a.map(|v| v * 0.3), a.map(|v| v * 0.2));
        let tint = |p: [f32; 3]| [p[0] * 1.1, p[1], p[2] * 0.8];
        let (hi, lo) = (tint(hi), tint(lo));
        let hz = [0.4, 0.0, 0.25];
        let r = |k: f32| luma(dehaze_px(hi, hz, a, k)) / luma(dehaze_px(lo, hz, a, k));
        let r0 = luma(hi) / luma(lo);
        assert!(
            r(0.5) > r0 * 1.2 && r(1.0) > r(0.5) * 1.1,
            "{r0} {} {}",
            r(0.5),
            r(1.0)
        );
        // Without a neighbourhood mean (0) the local-contrast part is off.
        let flat = [0.4, 0.0, 0.0];
        let rf = luma(dehaze_px(hi, flat, a, 1.0)) / luma(dehaze_px(lo, flat, a, 1.0));
        assert!(rf < r(1.0), "{rf}");
    }

    #[test]
    fn negative_dehaze_adds_a_depth_scaled_veil() {
        let a = [0.8, 0.85, 0.9];
        let p = [0.1, 0.2, 0.05];
        // Near (no haze): 10% of the way to the light; far: 40%, never more.
        let near = dehaze_px(p, [0.0, 1.0, 0.0], a, -1.0);
        let far = dehaze_px(p, [1.0, 1.0, 0.0], a, -1.0);
        assert!((near[0] - (0.1 + 0.7 * 0.1)).abs() < 1e-6, "{near:?}");
        assert!((far[0] - (0.1 + 0.7 * 0.4)).abs() < 1e-6, "{far:?}");
        assert!((haze_guide(a, a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn luma_chroma_roundtrip() {
        let rgb = [0.7, 0.4, 0.2];
        let y = luma(rgb);
        let back = from_luma_chroma(y, rgb[2] - y, rgb[0] - y);
        for c in 0..3 {
            assert!((back[c] - rgb[c]).abs() < 1e-5);
        }
    }
}
