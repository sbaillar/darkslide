//! Color balance (3-way + master), saturation, and the hue-band color editor (OKLCh).
//! Operates on display-encoded Rec.2020 values; the color editor decodes to linear to
//! work in OKLab.

use ds_core::adjust::{ColorBalance, ColorEditor, ColorRange, Wheel};

use crate::color::{
    REC2020_LUMA, SRGB_TO_XYZ, mat_mul, mat_vec, oklab_to_rec2020, rec2020_to_oklab, srgb_decode,
    srgb_encode, ui_range_to_ok, xyz_to_rec2020,
};
use crate::tone::smoothstep;

/// `v` if finite, else `default` (sidecars may carry NaN/Inf from buggy writers).
#[inline]
pub fn fin(v: f32, default: f32) -> f32 {
    if v.is_finite() { v } else { default }
}

#[inline]
pub fn luma(rgb: [f32; 3]) -> f32 {
    REC2020_LUMA[0] * rgb[0] + REC2020_LUMA[1] * rgb[1] + REC2020_LUMA[2] * rgb[2]
}

/// Luma-neutral RGB direction (encoded Rec.2020) for a UI hue, max |component| = 1.
pub fn hue_direction(hue_deg: f32) -> [f32; 3] {
    let h = hue_deg.rem_euclid(360.0) / 60.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let srgb = match h as u32 {
        0 => [1.0, x, 0.0],
        1 => [x, 1.0, 0.0],
        2 => [0.0, 1.0, x],
        3 => [0.0, x, 1.0],
        4 => [x, 0.0, 1.0],
        _ => [1.0, 0.0, x],
    };
    let to2020 = mat_mul(&xyz_to_rec2020(), &SRGB_TO_XYZ);
    let enc = mat_vec(&to2020, srgb.map(srgb_decode)).map(srgb_encode);
    let y = luma(enc);
    let d = enc.map(|v| v - y);
    let m = d.iter().fold(0.0f32, |a, v| a.max(v.abs())).max(1e-6);
    d.map(|v| v / m)
}

/// Compiled 3-way color balance: RGB offsets per zone.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BalanceParams {
    pub master: [f32; 3],
    pub shadows: [f32; 3],
    pub midtones: [f32; 3],
    pub highlights: [f32; 3],
}

impl BalanceParams {
    pub fn new(cb: &ColorBalance) -> Self {
        let off = |w: &Wheel, k: f32| {
            let d = hue_direction(fin(w.hue, 0.0));
            let (s, l) = (
                fin(w.sat, 0.0).clamp(0.0, 1.0),
                fin(w.lum, 0.0).clamp(-1.0, 1.0),
            );
            d.map(|v| k * (s * v + l))
        };
        Self {
            master: off(&cb.master, 0.1),
            shadows: off(&cb.shadows, 0.15),
            midtones: off(&cb.midtones, 0.15),
            highlights: off(&cb.highlights, 0.15),
        }
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

#[inline]
pub fn apply_balance(rgb: [f32; 3], p: &BalanceParams) -> [f32; 3] {
    let y = luma(rgb).clamp(0.0, 1.0);
    let ws = (1.0 - y) * (1.0 - y);
    let wh = y * y;
    let wm = 2.0 * y * (1.0 - y);
    [0, 1, 2].map(|c| {
        rgb[c] + p.master[c] + ws * p.shadows[c] + wm * p.midtones[c] + wh * p.highlights[c]
    })
}

#[inline]
pub fn apply_saturation(rgb: [f32; 3], factor: f32) -> [f32; 3] {
    let y = luma(rgb);
    rgb.map(|v| y + (v - y) * factor)
}

/// Vibrance (`k` −1..=1): saturation weighted toward muted colours (weight
/// `(1 − s)²` with `s = (max − min)/max` of the encoded pixel); skin tones (R ≥ G ≥ B
/// with G between ~25 and ~65% of the way from B to R) get 30% of the boost.
#[inline]
pub fn apply_vibrance(rgb: [f32; 3], k: f32) -> [f32; 3] {
    if k == 0.0 {
        return rgb;
    }
    let mx = rgb[0].max(rgb[1]).max(rgb[2]);
    let mn = rgb[0].min(rgb[1]).min(rgb[2]);
    let s = if mx > 1e-6 {
        ((mx - mn) / mx).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut w = (1.0 - s) * (1.0 - s);
    if k > 0.0 && rgb[0] >= rgb[1] && rgb[1] >= rgb[2] && rgb[0] > rgb[2] {
        let t = (rgb[1] - rgb[2]) / (rgb[0] - rgb[2]);
        let skin =
            crate::tone::smoothstep(0.1, 0.25, t) * (1.0 - crate::tone::smoothstep(0.55, 0.7, t));
        w *= 1.0 - 0.7 * skin;
    }
    apply_saturation(rgb, 1.0 + k * w)
}

/// One compiled hue range in OKLCh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OkRange {
    /// OK hue center and half-width, degrees.
    pub center: f32,
    pub half: f32,
    /// 0..=1 edge softness.
    pub smooth: f32,
    /// Degrees.
    pub hue_shift: f32,
    /// -1..=1.
    pub sat: f32,
    /// -1..=1.
    pub light: f32,
    /// Pull toward the center hue, 0..=1 (skin-tone uniformity).
    pub hue_pull: f32,
}

impl OkRange {
    pub fn new(r: &ColorRange) -> Self {
        let (center, half) = ui_range_to_ok(fin(r.center_hue, 0.0), fin(r.width, 45.0));
        Self {
            center,
            half,
            smooth: fin(r.smoothness, 0.5).clamp(0.0, 1.0),
            hue_shift: fin(r.hue_shift, 0.0).clamp(-30.0, 30.0),
            sat: (fin(r.saturation, 0.0) / 100.0).clamp(-1.0, 1.0),
            light: (fin(r.lightness, 0.0) / 100.0).clamp(-1.0, 1.0),
            hue_pull: 0.0,
        }
    }

    pub fn is_noop(&self) -> bool {
        self.hue_shift == 0.0 && self.sat == 0.0 && self.light == 0.0 && self.hue_pull == 0.0
    }

    /// Selection weight for an OKLCh hue (degrees) and chroma.
    #[inline]
    pub fn weight(&self, hue: f32, chroma: f32) -> f32 {
        let d = (hue - self.center).rem_euclid(360.0);
        let d = d.min(360.0 - d);
        let lo = self.half * (1.0 - 0.5 * self.smooth);
        let hi = self.half * (1.0 + 0.5 * self.smooth);
        (1.0 - smoothstep(lo, hi, d)) * smoothstep(0.005, 0.03, chroma)
    }
}

/// Compile the color editor's active ranges (basic bands, advanced ranges, skin tone).
pub fn compile_editor(ce: &ColorEditor) -> Vec<OkRange> {
    let mut v: Vec<OkRange> = ce
        .basic
        .iter()
        .chain(ce.advanced.iter())
        .map(OkRange::new)
        .filter(|r| !r.is_noop())
        .collect();
    if let Some(s) = &ce.skin {
        let mut r = OkRange::new(&s.range);
        r.hue_pull = (fin(s.hue_uniformity, 0.0) / 100.0).clamp(0.0, 1.0);
        if !r.is_noop() {
            v.push(r);
        }
    }
    v
}

/// Compile a layer's list of ranges.
pub fn compile_ranges(ranges: &[ColorRange]) -> Vec<OkRange> {
    ranges
        .iter()
        .map(OkRange::new)
        .filter(|r| !r.is_noop())
        .collect()
}

/// OKLCh (L, C, h°) of an encoded Rec.2020 pixel.
#[inline]
pub fn encoded_to_oklch(rgb: [f32; 3]) -> [f32; 3] {
    let lab = rec2020_to_oklab(rgb.map(srgb_decode));
    [
        lab[0],
        lab[1].hypot(lab[2]),
        lab[2].atan2(lab[1]).to_degrees().rem_euclid(360.0),
    ]
}

/// Apply a list of ranges to an encoded Rec.2020 pixel.
pub fn apply_ranges(rgb: [f32; 3], ranges: &[OkRange]) -> [f32; 3] {
    if ranges.is_empty() {
        return rgb;
    }
    let [mut l, mut c, mut h] = encoded_to_oklch(rgb);
    let mut touched = false;
    for r in ranges {
        let w = r.weight(h, c);
        if w <= 0.0 {
            continue;
        }
        touched = true;
        let mut dh = (r.center - h).rem_euclid(360.0);
        if dh > 180.0 {
            dh -= 360.0;
        }
        h += w * (r.hue_shift + dh * r.hue_pull);
        c *= (1.0 + w * r.sat).max(0.0);
        l *= 1.0 + 0.4 * w * r.light;
    }
    if !touched {
        return rgb;
    }
    let (s, co) = h.to_radians().sin_cos();
    oklab_to_rec2020([l, c * co, c * s]).map(srgb_encode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::adjust::SkinTone;

    #[test]
    fn hue_direction_is_luma_neutral() {
        for h in [0.0, 45.0, 120.0, 200.0, 300.0, 359.0] {
            let d = hue_direction(h);
            assert!(luma(d).abs() < 1e-5, "{h} {d:?}");
            assert!(d.iter().any(|v| (v.abs() - 1.0).abs() < 1e-5));
        }
        // Red points red.
        let r = hue_direction(0.0);
        assert!(r[0] > 0.0 && r[1] < 0.0);
    }

    #[test]
    fn balance_zones() {
        assert!(BalanceParams::new(&ColorBalance::default()).is_identity());
        let cb = ColorBalance {
            shadows: Wheel {
                hue: 240.0,
                sat: 1.0,
                lum: 0.0,
            },
            highlights: Wheel {
                hue: 30.0,
                sat: 1.0,
                lum: 0.0,
            },
            ..ColorBalance::default()
        };
        let p = BalanceParams::new(&cb);
        let dark = apply_balance([0.05; 3], &p);
        assert!(dark[2] > dark[0], "shadows go blue {dark:?}");
        let bright = apply_balance([0.95; 3], &p);
        assert!(bright[0] > bright[2], "highlights go warm {bright:?}");
        let lum = BalanceParams::new(&ColorBalance {
            master: Wheel {
                hue: 0.0,
                sat: 0.0,
                lum: 1.0,
            },
            ..ColorBalance::default()
        });
        let o = apply_balance([0.5; 3], &lum);
        assert!(o.iter().all(|v| (*v - 0.6).abs() < 1e-5), "{o:?}");
    }

    #[test]
    fn saturation_scales_chroma_keeps_luma() {
        let rgb = [0.6, 0.4, 0.2];
        let s = apply_saturation(rgb, 1.5);
        assert!((luma(s) - luma(rgb)).abs() < 1e-6);
        assert!(s[0] - s[2] > rgb[0] - rgb[2]);
        let g = apply_saturation(rgb, 0.0);
        assert!((g[0] - g[2]).abs() < 1e-6);
    }

    #[test]
    fn range_weights_and_falloff() {
        let r = OkRange::new(&ColorRange {
            center_hue: 120.0,
            width: 60.0,
            smoothness: 0.5,
            saturation: 50.0,
            ..ColorRange::default()
        });
        assert!(r.weight(r.center, 0.1) > 0.999);
        assert_eq!(r.weight((r.center + 180.0) % 360.0, 0.1), 0.0);
        // Smooth falloff: a point at the nominal edge is partially selected.
        let edge = r.weight(r.center + r.half, 0.1);
        assert!(edge > 0.2 && edge < 0.8, "{edge}");
        // Neutral pixels are never selected.
        assert_eq!(r.weight(r.center, 0.0), 0.0);
        let hard = OkRange { smooth: 0.0, ..r };
        assert_eq!(hard.weight(hard.center + hard.half * 0.9, 0.1), 1.0);
        assert_eq!(hard.weight(hard.center + hard.half * 1.1, 0.1), 0.0);
    }

    #[test]
    fn editor_moves_only_selected_hues() {
        let mut ce = ColorEditor::default();
        assert_eq!(compile_editor(&ce).len(), 0);
        ce.basic[3].saturation = -100.0; // greens
        let ranges = compile_editor(&ce);
        assert_eq!(ranges.len(), 1);
        let green = [0.2, 0.6, 0.2];
        let out = apply_ranges(green, &ranges);
        let spread = |p: [f32; 3]| {
            p.iter().fold(0.0f32, |a, v| a.max(*v)) - p.iter().fold(1.0f32, |a, v| a.min(*v))
        };
        assert!(spread(out) < spread(green) * 0.2, "{out:?}");
        let blue = [0.1, 0.2, 0.7];
        let ob = apply_ranges(blue, &ranges);
        for c in 0..3 {
            assert!((ob[c] - blue[c]).abs() < 1e-3, "{ob:?}");
        }
        // Gray is untouched and round-trips exactly.
        assert_eq!(apply_ranges([0.5; 3], &ranges), [0.5; 3]);
        assert_eq!(apply_ranges(green, &[]), green);
    }

    #[test]
    fn hue_shift_lightness_and_skin_pull() {
        let lch = encoded_to_oklch([0.7, 0.45, 0.35]);
        let r = OkRange {
            center: lch[2],
            half: 30.0,
            smooth: 0.5,
            hue_shift: 20.0,
            sat: 0.0,
            light: 0.5,
            hue_pull: 0.0,
        };
        let out = encoded_to_oklch(apply_ranges([0.7, 0.45, 0.35], &[r]));
        assert!((out[2] - lch[2] - 20.0).abs() < 1.0, "{lch:?} {out:?}");
        assert!(out[0] > lch[0]);
        let pull = OkRange {
            center: lch[2] + 10.0,
            hue_shift: 0.0,
            light: 0.0,
            hue_pull: 1.0,
            ..r
        };
        let pulled = encoded_to_oklch(apply_ranges([0.7, 0.45, 0.35], &[pull]));
        assert!((pulled[2] - (lch[2] + 10.0)).abs() < 1.5, "{pulled:?}");
        let mut ce = ColorEditor {
            skin: Some(SkinTone {
                range: ColorRange {
                    center_hue: 25.0,
                    ..ColorRange::default()
                },
                hue_uniformity: 50.0,
                saturation_uniformity: 0.0,
                lightness_uniformity: 0.0,
            }),
            ..ColorEditor::default()
        };
        assert_eq!(compile_editor(&ce).len(), 1);
        ce.skin.as_mut().unwrap().hue_uniformity = 0.0;
        assert_eq!(compile_editor(&ce).len(), 0);
        assert_eq!(compile_ranges(&[ColorRange::default()]).len(), 0);
    }
}
