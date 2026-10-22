//! Geometry primitives shared by crop, masks, and rendering.

use serde::{Deserialize, Serialize};

/// Pixel dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub fn is_portrait(self) -> bool {
        self.height > self.width
    }

    #[must_use]
    /// Largest size with the same aspect ratio that fits within `bound`.
    /// Never upscales and never returns a zero dimension for non-empty input.
    pub fn fit_within(self, bound: Size) -> Size {
        if self.width == 0 || self.height == 0 {
            return self;
        }
        if self.width <= bound.width && self.height <= bound.height {
            return self;
        }
        let scale = (f64::from(bound.width) / f64::from(self.width))
            .min(f64::from(bound.height) / f64::from(self.height));
        Size {
            width: ((f64::from(self.width) * scale).round() as u32).max(1),
            height: ((f64::from(self.height) * scale).round() as u32).max(1),
        }
    }

    pub fn pixels(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// Rectangle in normalized image coordinates (0..=1 on both axes),
/// relative to the oriented (display-upright) image.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormRect {
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub x: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub y: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub w: f32,
    #[serde(deserialize_with = "crate::lenient::f32")]
    pub h: f32,
}

impl Default for NormRect {
    fn default() -> Self {
        Self::FULL
    }
}

impl NormRect {
    pub const FULL: NormRect = NormRect {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };

    #[must_use]
    /// Clamp into the unit square, keeping a minimum extent of `1e-4`.
    /// Non-finite extents become full (1.0) and non-finite positions become 0,
    /// so a corrupt rect never panics or propagates NaN into the pipeline.
    pub fn clamped(self) -> NormRect {
        let finite_or = |v: f32, d: f32| if v.is_finite() { v } else { d };
        let w = finite_or(self.w, 1.0).clamp(1e-4, 1.0);
        let h = finite_or(self.h, 1.0).clamp(1e-4, 1.0);
        NormRect {
            x: finite_or(self.x, 0.0).clamp(0.0, 1.0 - w),
            y: finite_or(self.y, 0.0).clamp(0.0, 1.0 - h),
            w,
            h,
        }
    }

    pub fn is_full(self) -> bool {
        self == Self::FULL
    }
}

/// EXIF orientation, as the eight TIFF values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Orientation {
    #[default]
    Normal,
    FlipH,
    Rotate180,
    FlipV,
    Transpose,
    Rotate90,
    Transverse,
    Rotate270,
}

impl Orientation {
    /// From the EXIF `Orientation` tag value (1..=8). Unknown values map to `Normal`.
    pub fn from_exif(v: u16) -> Self {
        match v {
            2 => Self::FlipH,
            3 => Self::Rotate180,
            4 => Self::FlipV,
            5 => Self::Transpose,
            6 => Self::Rotate90,
            7 => Self::Transverse,
            8 => Self::Rotate270,
            _ => Self::Normal,
        }
    }

    pub fn to_exif(self) -> u16 {
        match self {
            Self::Normal => 1,
            Self::FlipH => 2,
            Self::Rotate180 => 3,
            Self::FlipV => 4,
            Self::Transpose => 5,
            Self::Rotate90 => 6,
            Self::Transverse => 7,
            Self::Rotate270 => 8,
        }
    }

    /// True when width and height swap on display.
    pub fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Transpose | Self::Rotate90 | Self::Transverse | Self::Rotate270
        )
    }

    pub fn apply_to(self, size: Size) -> Size {
        if self.swaps_axes() {
            Size::new(size.height, size.width)
        } else {
            size
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

    use super::*;

    #[test]
    fn size_orientation_and_pixels() {
        assert!(Size::new(4000, 6000).is_portrait());
        assert!(!Size::new(6000, 4000).is_portrait());
        assert!(!Size::new(10, 10).is_portrait(), "square is not portrait");
        assert_eq!(Size::new(6000, 4000).pixels(), 24_000_000);
        assert_eq!(
            Size::new(u32::MAX, u32::MAX).pixels(),
            u64::from(u32::MAX).pow(2)
        );
    }

    #[test]
    fn fit_within_scales_down_preserving_aspect() {
        assert_eq!(
            Size::new(6000, 4000).fit_within(Size::new(512, 512)),
            Size::new(512, 341)
        );
        assert_eq!(
            Size::new(4000, 6000).fit_within(Size::new(512, 512)),
            Size::new(341, 512)
        );
        assert_eq!(
            Size::new(6000, 4000).fit_within(Size::new(2560, 2560)),
            Size::new(2560, 1707)
        );
        // Limited by height.
        assert_eq!(
            Size::new(1000, 1000).fit_within(Size::new(800, 100)),
            Size::new(100, 100)
        );
    }

    #[test]
    fn fit_within_never_upscales_or_zeroes() {
        assert_eq!(
            Size::new(100, 50).fit_within(Size::new(2560, 2560)),
            Size::new(100, 50)
        );
        assert_eq!(
            Size::new(100, 50).fit_within(Size::new(100, 50)),
            Size::new(100, 50)
        );
        assert_eq!(
            Size::new(0, 50).fit_within(Size::new(10, 10)),
            Size::new(0, 50)
        );
        assert_eq!(
            Size::new(50, 0).fit_within(Size::new(10, 10)),
            Size::new(50, 0)
        );
        // Extreme aspect: the short side rounds to zero but is kept at 1.
        assert_eq!(
            Size::new(10_000, 1).fit_within(Size::new(100, 100)),
            Size::new(100, 1)
        );
        assert_eq!(
            Size::new(100, 100).fit_within(Size::new(0, 0)),
            Size::new(1, 1)
        );
    }

    #[test]
    fn norm_rect_default_is_full() {
        assert_eq!(NormRect::default(), NormRect::FULL);
        assert!(NormRect::default().is_full());
        assert!(
            !NormRect {
                x: 0.1,
                ..NormRect::FULL
            }
            .is_full()
        );
    }

    #[test]
    fn clamped_keeps_in_bounds_rect() {
        let r = NormRect {
            x: 0.1,
            y: 0.2,
            w: 0.5,
            h: 0.6,
        };
        assert_eq!(r.clamped(), r);
    }

    #[test]
    fn clamped_moves_and_shrinks_into_unit_square() {
        let r = NormRect {
            x: 0.8,
            y: -0.3,
            w: 0.5,
            h: 2.0,
        }
        .clamped();
        assert_eq!(
            r,
            NormRect {
                x: 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0
            }
        );
        let tiny = NormRect {
            x: 1.5,
            y: 0.0,
            w: 0.0,
            h: -1.0,
        }
        .clamped();
        assert_eq!(tiny.w, 1e-4);
        assert_eq!(tiny.h, 1e-4);
        assert!((tiny.x - (1.0 - 1e-4)).abs() < 1e-6);
    }

    #[test]
    fn clamped_is_nan_safe() {
        // Used to panic: `x.clamp(0.0, 1.0 - NaN)` asserts min <= max.
        let r = NormRect {
            x: f32::NAN,
            y: f32::INFINITY,
            w: f32::NAN,
            h: f32::NEG_INFINITY,
        }
        .clamped();
        for v in [r.x, r.y, r.w, r.h] {
            assert!(v.is_finite(), "{r:?}");
        }
        assert!(r.x >= 0.0 && r.x + r.w <= 1.0 + 1e-6);
        assert!(r.y >= 0.0 && r.y + r.h <= 1.0 + 1e-6);
        assert_eq!(r.w, 1.0, "non-finite extent falls back to full");
        assert_eq!(r.h, 1.0, "non-finite extent falls back to full");
    }

    #[test]
    fn orientation_exif_round_trip() {
        for v in 1..=8u16 {
            assert_eq!(Orientation::from_exif(v).to_exif(), v);
        }
        assert_eq!(Orientation::from_exif(0), Orientation::Normal);
        assert_eq!(Orientation::from_exif(9), Orientation::Normal);
        assert_eq!(Orientation::default(), Orientation::Normal);
    }

    #[test]
    fn orientation_swaps_axes() {
        let s = Size::new(6000, 4000);
        for v in 1..=8u16 {
            let o = Orientation::from_exif(v);
            let expect_swap = v >= 5;
            assert_eq!(o.swaps_axes(), expect_swap, "{o:?}");
            assert_eq!(
                o.apply_to(s),
                if expect_swap {
                    Size::new(4000, 6000)
                } else {
                    s
                }
            );
        }
    }

    #[test]
    fn serde_round_trip() {
        let r = NormRect {
            x: 0.25,
            y: 0.5,
            w: 0.125,
            h: 0.0625,
        };
        let back: NormRect = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        let s: Size = serde_json::from_str(r#"{"width":3,"height":4}"#).unwrap();
        assert_eq!(s, Size::new(3, 4));
        let o: Orientation =
            serde_json::from_str(&serde_json::to_string(&Orientation::Rotate270).unwrap()).unwrap();
        assert_eq!(o, Orientation::Rotate270);
    }

    proptest::proptest! {
        #[test]
        fn clamped_always_inside_unit_square(
            x in proptest::num::f32::ANY,
            y in proptest::num::f32::ANY,
            w in proptest::num::f32::ANY,
            h in proptest::num::f32::ANY,
        ) {
            let r = NormRect { x, y, w, h }.clamped();
            proptest::prop_assert!(r.w >= 1e-4 && r.w <= 1.0);
            proptest::prop_assert!(r.h >= 1e-4 && r.h <= 1.0);
            proptest::prop_assert!(r.x >= 0.0 && r.x + r.w <= 1.0 + 1e-6);
            proptest::prop_assert!(r.y >= 0.0 && r.y + r.h <= 1.0 + 1e-6);
        }

        #[test]
        fn fit_within_respects_bound(w in 1u32..20_000, h in 1u32..20_000, bw in 1u32..4000, bh in 1u32..4000) {
            let out = Size::new(w, h).fit_within(Size::new(bw, bh));
            proptest::prop_assert!(out.width >= 1 && out.height >= 1);
            proptest::prop_assert!(out.width <= w && out.height <= h);
            proptest::prop_assert!(out.width <= bw && out.height <= bh);
        }
    }
}
