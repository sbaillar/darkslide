//! Per-pixel tone functions shared by the LUT builder, the layer path and (mirrored
//! in WGSL) the GPU renderer. All operate on display-encoded values in 0..=1 unless
//! stated otherwise.

use ds_core::adjust::Levels;

/// Scene-linear middle gray.
pub const MID_GRAY: f32 = 0.18;

#[inline]
pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e1 <= e0 {
        return if x < e0 { 0.0 } else { 1.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Contrast exponent for the UI value (-100..=100).
pub fn contrast_k(contrast: f32) -> f32 {
    1.0 + 0.6 * (contrast / 100.0).clamp(-1.0, 1.0)
}

/// Power S-curve about `pivot`: slope `k` at the pivot, endpoints fixed, monotone.
#[inline]
pub fn contrast(y: f32, k: f32, pivot: f32) -> f32 {
    let y = y.clamp(0.0, 1.0);
    if (k - 1.0).abs() < 1e-6 {
        return y;
    }
    if y < pivot {
        pivot * (y / pivot).powf(k)
    } else {
        1.0 - (1.0 - pivot) * ((1.0 - y) / (1.0 - pivot)).powf(k)
    }
}

/// Brightness gamma for the UI value (-100..=100); > 0 brightens midtones.
pub fn brightness_gamma(brightness: f32) -> f32 {
    2f32.powf(-0.5 * (brightness / 100.0).clamp(-1.0, 1.0))
}

#[inline]
pub fn brightness(y: f32, gamma: f32) -> f32 {
    y.clamp(0.0, 1.0).powf(gamma)
}

/// Blacks: a toe that lifts (> 0) or crushes (< 0) the darkest tones. Monotone for |b| ≤ 100.
#[inline]
pub fn blacks_toe(y: f32, blacks: f32) -> f32 {
    let y = y.clamp(0.0, 1.0);
    let t = 1.0 - y;
    let t2 = t * t;
    (y + 0.08 * (blacks / 100.0) * t2 * t2 * t2).clamp(0.0, 1.0)
}

/// Whites: stretch factor applied to stops above middle gray (> 1 brightens highlights).
pub fn whites_factor(whites: f32) -> f32 {
    2f32.powf(0.5 * (whites / 100.0).clamp(-1.0, 1.0))
}

/// Input/output levels with midtone gamma (Photoshop convention: gamma > 1 brightens).
#[inline]
pub fn levels(y: f32, l: &Levels) -> f32 {
    let span = (l.in_white - l.in_black).max(1e-4);
    let t = ((y - l.in_black) / span).clamp(0.0, 1.0);
    let g = l.gamma.clamp(0.1, 10.0);
    let t = if (g - 1.0).abs() < 1e-6 {
        t
    } else {
        t.powf(1.0 / g)
    };
    l.out_black + (l.out_white - l.out_black) * t
}

/// HDR highlights/shadows gain in stops for a smoothed log2 luminance `b_rel`
/// (stops relative to middle gray). `highlights`/`shadows` are -1..=1.
/// Positive highlights darken (recover) bright regions; positive shadows lift dark ones.
#[inline]
pub fn hdr_gain_ev(b_rel: f32, highlights: f32, shadows: f32) -> f32 {
    let w_sh = 1.0 - smoothstep(-6.0, 0.0, b_rel);
    let w_hi = smoothstep(-1.0, 3.0, b_rel);
    1.5 * shadows * w_sh - 1.5 * highlights * w_hi
}

/// Vignette gain (linear) at normalized radius `r` (0 center, 1 corner).
#[inline]
pub fn vignette_gain(r: f32, amount_ev: f32, midpoint: f32) -> f32 {
    let t = smoothstep(midpoint.clamp(0.0, 0.99), 1.0, r);
    2f32.powf(amount_ev * t * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monotone(f: impl Fn(f32) -> f32) -> bool {
        let mut prev = f32::NEG_INFINITY;
        (0..=1000).all(|i| {
            let v = f(i as f32 / 1000.0);
            let ok = v >= prev - 1e-6;
            prev = v;
            ok
        })
    }

    #[test]
    fn smoothstep_edges() {
        assert_eq!(smoothstep(0.0, 1.0, -1.0), 0.0);
        assert_eq!(smoothstep(0.0, 1.0, 2.0), 1.0);
        assert_eq!(smoothstep(0.0, 1.0, 0.5), 0.5);
        assert_eq!(smoothstep(1.0, 1.0, 0.5), 0.0);
        assert_eq!(smoothstep(1.0, 1.0, 1.5), 1.0);
    }

    #[test]
    fn contrast_is_monotone_and_fixes_endpoints() {
        for c in [-100.0, -30.0, 0.0, 50.0, 100.0] {
            let k = contrast_k(c);
            assert!(monotone(|y| contrast(y, k, 0.46)), "c={c}");
            assert!(contrast(0.0, k, 0.46).abs() < 1e-6);
            assert!((contrast(1.0, k, 0.46) - 1.0).abs() < 1e-6);
            assert!((contrast(0.46, k, 0.46) - 0.46).abs() < 1e-5);
        }
        // More contrast darkens below the pivot.
        assert!(contrast(0.2, contrast_k(80.0), 0.46) < 0.2);
    }

    #[test]
    fn brightness_blacks_whites() {
        assert!(brightness(0.5, brightness_gamma(50.0)) > 0.5);
        assert!(brightness(0.5, brightness_gamma(-50.0)) < 0.5);
        assert_eq!(brightness(0.5, brightness_gamma(0.0)), 0.5);
        for b in [-100.0, 100.0] {
            assert!(monotone(|y| blacks_toe(y, b)));
        }
        assert!(blacks_toe(0.02, 100.0) > 0.02);
        assert_eq!(blacks_toe(0.02, -100.0), 0.0);
        assert!((blacks_toe(1.0, 100.0) - 1.0).abs() < 1e-6);
        assert!(whites_factor(100.0) > 1.0 && whites_factor(-100.0) < 1.0);
    }

    #[test]
    fn levels_mapping() {
        let id = Levels::default();
        assert_eq!(levels(0.3, &id), 0.3);
        let l = Levels {
            in_black: 0.1,
            in_white: 0.9,
            gamma: 2.0,
            out_black: 0.05,
            out_white: 0.95,
        };
        assert!((levels(0.1, &l) - 0.05).abs() < 1e-6);
        assert!((levels(0.9, &l) - 0.95).abs() < 1e-6);
        assert!(levels(0.5, &l) > 0.5);
        assert!(monotone(|y| levels(y, &l)));
        // Degenerate span doesn't divide by zero.
        let d = Levels {
            in_black: 0.5,
            in_white: 0.5,
            ..Levels::default()
        };
        assert!(levels(0.7, &d).is_finite());
    }

    #[test]
    fn hdr_and_vignette() {
        assert_eq!(hdr_gain_ev(0.0, 0.0, 0.0), 0.0);
        assert!(hdr_gain_ev(-6.0, 0.0, 1.0) > 1.4);
        assert!(hdr_gain_ev(3.0, 1.0, 0.0) < -1.4);
        assert!(hdr_gain_ev(-1.5, 1.0, 0.0).abs() < 1e-6);
        assert_eq!(vignette_gain(0.2, -1.0, 0.5), 1.0);
        assert!((vignette_gain(1.0, -1.0, 0.5) - 0.5).abs() < 1e-6);
    }
}
