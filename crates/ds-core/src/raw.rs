//! Decoded-but-unprocessed sensor data, the hand-off between `ds-raw` and `ds-pipeline`.

use crate::geom::{Orientation, Size};

/// Color filter array layout of the top-left 2x2 block (Bayer), or X-Trans 6x6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CfaPattern {
    Rggb,
    Bggr,
    Grbg,
    Gbrg,
    /// Already demosaiced / linear RGB (e.g. linear DNG). Samples are interleaved RGB.
    LinearRgb,
}

impl CfaPattern {
    /// Channel (0=R, 1=G, 2=B) at pixel `(x, y)` for Bayer patterns.
    /// Returns `None` for [`CfaPattern::LinearRgb`].
    pub fn color_at(self, x: u32, y: u32) -> Option<u8> {
        let idx = ((y & 1) << 1 | (x & 1)) as usize;
        let lut: [u8; 4] = match self {
            Self::Rggb => [0, 1, 1, 2],
            Self::Bggr => [2, 1, 1, 0],
            Self::Grbg => [1, 0, 2, 1],
            Self::Gbrg => [1, 2, 0, 1],
            Self::LinearRgb => return None,
        };
        Some(lut[idx])
    }

    /// Pattern after cropping `dx`,`dy` pixels from the top-left.
    #[must_use]
    pub fn shifted(self, dx: u32, dy: u32) -> Self {
        let Some(_) = self.color_at(0, 0) else {
            return self;
        };
        // Only parity matters; reducing first avoids `+ 1` overflow at u32::MAX.
        let (dx, dy) = (dx & 1, dy & 1);
        let key = [
            self.color_at(dx, dy),
            self.color_at(dx + 1, dy),
            self.color_at(dx, dy + 1),
        ];
        match key {
            [Some(0), _, _] => Self::Rggb,
            [Some(2), _, _] => Self::Bggr,
            [Some(1), Some(0), _] => Self::Grbg,
            _ => Self::Gbrg,
        }
    }
}

/// Sensor data plus everything the pipeline needs to develop it.
///
/// `data` holds the *active area only* (already cropped from the full sensor),
/// row-major, `u16` per sample. For Bayer it is one sample per pixel; for
/// [`CfaPattern::LinearRgb`] three interleaved samples per pixel.
#[derive(Debug, Clone)]
pub struct RawImage {
    pub size: Size,
    pub cfa: CfaPattern,
    pub data: Vec<u16>,
    /// Per-CFA-channel black level, in R, G, B, G2 order. `G` is the green
    /// that shares rows with red, `G2` the green on the blue rows (for
    /// [`CfaPattern::LinearRgb`]: R, G, B, with G repeated as G2).
    pub black_level: [f32; 4],
    /// Raw value at which the sensor clips (same scale as `data`).
    pub white_level: f32,
    /// As-shot white balance multipliers (R, G, B), normalized so G = 1.
    pub wb_as_shot: [f32; 3],
    /// Multipliers (R, G, B; G = 1) that neutralize D65 daylight for this
    /// camera, i.e. `1 / (ColorMatrix · D65)` normalized. `cam_to_xyz` is
    /// row-normalized and so no longer carries the per-channel response; the
    /// pipeline needs this to convert between Kelvin/tint and multipliers:
    /// `mults(T) ∝ wb_daylight ⊘ (N · xyz(T))` with `N = cam_to_xyz⁻¹`.
    pub wb_daylight: [f32; 3],
    /// Camera RGB → XYZ (D65) matrix, row-major, for **white-balanced**
    /// camera values. Derived in `ds-raw` (see `ds_raw::color`) from the Adobe
    /// `ColorMatrix` for D65 (`ColorMatrix2`, XYZ → camera) `M`:
    /// each row of `M` is divided by its response to the D65 white point
    /// `w = (0.95047, 1, 1.08883)`, then the result is inverted, so
    /// `cam_to_xyz · (1, 1, 1) = w`. Per pixel, in this order:
    ///
    /// 1. `v = (raw − black_level[c]) / (white_level − black_level[c])`
    /// 2. `v *= wb[c]` (as-shot or user multipliers, G = 1)
    /// 3. `xyz = cam_to_xyz · v`
    ///
    /// A neutral surface (equal channels after step 2) lands on D65 white;
    /// no further chromatic adaptation is applied for white balance.
    pub cam_to_xyz: [[f32; 3]; 3],
    pub orientation: Orientation,
    /// Default crop from the maker notes, in active-area pixels (x, y, w, h).
    pub default_crop: Option<[u32; 4]>,
}

impl RawImage {
    /// Number of `u16` samples expected in `data` for this size/pattern.
    pub fn expected_len(&self) -> usize {
        let px = self.size.pixels() as usize;
        if self.cfa == CfaPattern::LinearRgb {
            px * 3
        } else {
            px
        }
    }

    /// Structural sanity check: the pipeline assumes this holds.
    pub fn validate(&self) -> Result<(), RawError> {
        if self.size.width < 2 || self.size.height < 2 {
            return Err(RawError::Invalid(format!("size {:?} too small", self.size)));
        }
        if self.data.len() != self.expected_len() {
            return Err(RawError::Invalid(format!(
                "data len {} != expected {}",
                self.data.len(),
                self.expected_len()
            )));
        }
        if self.black_level.iter().any(|b| !b.is_finite()) {
            return Err(RawError::Invalid(format!(
                "non-finite black level {:?}",
                self.black_level
            )));
        }
        let max_black = self.black_level.iter().copied().fold(0.0, f32::max);
        if !self.white_level.is_finite() || self.white_level <= max_black {
            return Err(RawError::Invalid(format!(
                "white level {} not above black {max_black}",
                self.white_level
            )));
        }
        if self.wb_as_shot.iter().any(|m| !m.is_finite() || *m <= 0.0) {
            return Err(RawError::Invalid(format!(
                "bad as-shot WB {:?}",
                self.wb_as_shot
            )));
        }
        if self.wb_daylight.iter().any(|m| !m.is_finite() || *m <= 0.0) {
            return Err(RawError::Invalid(format!(
                "bad daylight WB {:?}",
                self.wb_daylight
            )));
        }
        if self.cam_to_xyz.iter().flatten().any(|v| !v.is_finite()) {
            return Err(RawError::Invalid("non-finite color matrix".into()));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RawError {
    #[error("invalid raw image: {0}")]
    Invalid(String),
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

    const BAYER: [CfaPattern; 4] = [
        CfaPattern::Rggb,
        CfaPattern::Bggr,
        CfaPattern::Grbg,
        CfaPattern::Gbrg,
    ];

    /// Reference: the 2x2 block written out as letters.
    fn block(p: CfaPattern) -> [u8; 4] {
        match p {
            CfaPattern::Rggb => [0, 1, 1, 2],
            CfaPattern::Bggr => [2, 1, 1, 0],
            CfaPattern::Grbg => [1, 0, 2, 1],
            CfaPattern::Gbrg => [1, 2, 0, 1],
            CfaPattern::LinearRgb => unreachable!(),
        }
    }

    #[test]
    fn color_at_tiles_the_2x2_block() {
        for p in BAYER {
            let b = block(p);
            for y in 0..6u32 {
                for x in 0..6u32 {
                    let expect = b[((y % 2) * 2 + x % 2) as usize];
                    assert_eq!(p.color_at(x, y), Some(expect), "{p:?} at ({x},{y})");
                }
            }
            // Large coordinates still work (only parity matters).
            assert_eq!(p.color_at(u32::MAX, u32::MAX), Some(b[3]));
        }
        assert_eq!(CfaPattern::LinearRgb.color_at(0, 0), None);
    }

    #[test]
    fn shifted_matches_color_at_for_all_offsets() {
        for p in BAYER {
            assert_eq!(p.shifted(0, 0), p);
            assert_eq!(p.shifted(2, 4), p);
            for dy in 0..4u32 {
                for dx in 0..4u32 {
                    let s = p.shifted(dx, dy);
                    for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        assert_eq!(
                            s.color_at(x, y),
                            p.color_at(x + dx, y + dy),
                            "{p:?} shifted by ({dx},{dy})"
                        );
                    }
                }
            }
        }
        assert_eq!(CfaPattern::Rggb.shifted(1, 0), CfaPattern::Grbg);
        assert_eq!(CfaPattern::Rggb.shifted(0, 1), CfaPattern::Gbrg);
        assert_eq!(CfaPattern::Rggb.shifted(1, 1), CfaPattern::Bggr);
        assert_eq!(CfaPattern::LinearRgb.shifted(1, 1), CfaPattern::LinearRgb);
    }

    #[test]
    fn shifted_does_not_overflow_at_max_offset() {
        // `dx + 1` used to overflow (panic in debug) for dx = u32::MAX.
        for p in BAYER {
            assert_eq!(p.shifted(u32::MAX, u32::MAX), p.shifted(1, 1));
            assert_eq!(p.shifted(u32::MAX - 1, 0), p);
        }
    }

    fn valid(cfa: CfaPattern) -> RawImage {
        let size = Size::new(4, 2);
        let len = if cfa == CfaPattern::LinearRgb { 24 } else { 8 };
        RawImage {
            size,
            cfa,
            data: vec![512; len],
            black_level: [512.0; 4],
            white_level: 16383.0,
            wb_as_shot: [2.0, 1.0, 1.5],
            wb_daylight: [2.4, 1.0, 1.4],
            cam_to_xyz: [[0.6, 0.3, 0.1], [0.2, 0.7, 0.1], [0.0, 0.1, 0.9]],
            orientation: Orientation::Normal,
            default_crop: None,
        }
    }

    fn err(img: &RawImage) -> String {
        match img.validate() {
            Err(RawError::Invalid(m)) => m,
            Ok(()) => panic!("expected invalid: {img:?}"),
        }
    }

    #[test]
    fn validate_accepts_good_images() {
        assert_eq!(valid(CfaPattern::Rggb).validate(), Ok(()));
        let rgb = valid(CfaPattern::LinearRgb);
        assert_eq!(rgb.expected_len(), 24);
        assert_eq!(rgb.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_each_problem() {
        let mut img = valid(CfaPattern::Rggb);
        img.size = Size::new(1, 8);
        assert!(err(&img).contains("too small"));
        img.size = Size::new(8, 1);
        assert!(err(&img).contains("too small"));

        let mut img = valid(CfaPattern::Rggb);
        img.data.pop();
        assert!(err(&img).contains("data len 7 != expected 8"));

        let mut img = valid(CfaPattern::Rggb);
        img.white_level = 512.0;
        assert!(err(&img).contains("white level"));
        img.white_level = f32::NAN;
        assert!(err(&img).contains("white level"));

        let mut img = valid(CfaPattern::Rggb);
        img.wb_as_shot[1] = 0.0;
        assert!(err(&img).contains("as-shot WB"));
        img.wb_as_shot[1] = f32::INFINITY;
        assert!(err(&img).contains("as-shot WB"));
        img.wb_as_shot[1] = -1.0;
        assert!(err(&img).contains("as-shot WB"));

        let mut img = valid(CfaPattern::Rggb);
        img.cam_to_xyz[2][2] = f32::NAN;
        assert!(err(&img).contains("color matrix"));
    }

    #[test]
    fn validate_rejects_non_finite_levels() {
        // A NaN black level used to be skipped by `f32::max` and pass.
        let mut img = valid(CfaPattern::Rggb);
        img.black_level[2] = f32::NAN;
        assert!(err(&img).contains("black"));
        let mut img = valid(CfaPattern::Rggb);
        img.black_level[0] = f32::NEG_INFINITY;
        assert!(err(&img).contains("black"));
        // An infinite white level would make normalization divide to zero.
        let mut img = valid(CfaPattern::Rggb);
        img.white_level = f32::INFINITY;
        assert!(err(&img).contains("white level"));
    }

    #[test]
    fn error_display() {
        assert_eq!(
            RawError::Invalid("x".into()).to_string(),
            "invalid raw image: x"
        );
    }
}
