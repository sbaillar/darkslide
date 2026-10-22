//! Copy/Apply metadata. The group list and which groups support relative paste
//! come from `ds_core`, so the Swift sheet can never drift from the Rust model.

use ds_core::AdjustGroup as CoreGroup;
use ds_core::adjust::{Aspect, Crop};
use ds_core::{NormRect, Size};

use crate::error::DsError;
use crate::jobs::guard;

/// Mirror of [`ds_core::AdjustGroup`] (`UniFFI` needs a local enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum AdjustGroup {
    Profile,
    WhiteBalance,
    Exposure,
    Presence,
    Hdr,
    Levels,
    Curve,
    ColorBalance,
    ColorEditor,
    Detail,
    Noise,
    Vignette,
    Effects,
    Lens,
    Crop,
    Layers,
    Spots,
}

impl From<CoreGroup> for AdjustGroup {
    fn from(g: CoreGroup) -> Self {
        match g {
            CoreGroup::Profile => Self::Profile,
            CoreGroup::WhiteBalance => Self::WhiteBalance,
            CoreGroup::Exposure => Self::Exposure,
            CoreGroup::Presence => Self::Presence,
            CoreGroup::Hdr => Self::Hdr,
            CoreGroup::Levels => Self::Levels,
            CoreGroup::Curve => Self::Curve,
            CoreGroup::ColorBalance => Self::ColorBalance,
            CoreGroup::ColorEditor => Self::ColorEditor,
            CoreGroup::Detail => Self::Detail,
            CoreGroup::Noise => Self::Noise,
            CoreGroup::Vignette => Self::Vignette,
            CoreGroup::Effects => Self::Effects,
            CoreGroup::Lens => Self::Lens,
            CoreGroup::Crop => Self::Crop,
            CoreGroup::Layers => Self::Layers,
            CoreGroup::Spots => Self::Spots,
        }
    }
}

impl From<AdjustGroup> for CoreGroup {
    fn from(g: AdjustGroup) -> Self {
        match g {
            AdjustGroup::Profile => Self::Profile,
            AdjustGroup::WhiteBalance => Self::WhiteBalance,
            AdjustGroup::Exposure => Self::Exposure,
            AdjustGroup::Presence => Self::Presence,
            AdjustGroup::Hdr => Self::Hdr,
            AdjustGroup::Levels => Self::Levels,
            AdjustGroup::Curve => Self::Curve,
            AdjustGroup::ColorBalance => Self::ColorBalance,
            AdjustGroup::ColorEditor => Self::ColorEditor,
            AdjustGroup::Detail => Self::Detail,
            AdjustGroup::Noise => Self::Noise,
            AdjustGroup::Vignette => Self::Vignette,
            AdjustGroup::Effects => Self::Effects,
            AdjustGroup::Lens => Self::Lens,
            AdjustGroup::Crop => Self::Crop,
            AdjustGroup::Layers => Self::Layers,
            AdjustGroup::Spots => Self::Spots,
        }
    }
}

/// One group row of the Copy options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct AdjustGroupInfo {
    pub group: AdjustGroup,
    pub supports_relative: bool,
}

/// Every copyable group in canonical order.
#[uniffi::export]
pub fn adjust_groups() -> Result<Vec<AdjustGroupInfo>, DsError> {
    guard(|| {
        Ok(CoreGroup::ALL
            .into_iter()
            .map(|g| AdjustGroupInfo {
                group: g.into(),
                supports_relative: g.supports_relative(),
            })
            .collect())
    })
}

/// The parts of a crop that decide how it lands on another photo (M10).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct PastedCrop {
    /// Normalized rect in the oriented image (`ds_core::adjust::Crop::rect`).
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Aspect lock long:short; 0:0 = unlocked.
    pub aspect_long: u32,
    pub aspect_short: u32,
    pub quarter_turns: u8,
}

/// A crop copied from a `source_width`×`source_height` photo, pasted onto a
/// `target_width`×`target_height` one (oriented pixels): the rect after
/// [`ds_core::clipboard::adapt_crop`] (the orientation follows the frame or is kept,
/// and a locked aspect is re-fitted in the target's pixels). Only the rect changes.
#[uniffi::export]
pub fn adapt_pasted_crop(
    crop: PastedCrop,
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> PastedCrop {
    let src = Crop {
        rect: NormRect {
            x: crop.x,
            y: crop.y,
            w: crop.w,
            h: crop.h,
        },
        aspect: (crop.aspect_long > 0 && crop.aspect_short > 0).then_some(Aspect {
            long: crop.aspect_long,
            short: crop.aspect_short,
        }),
        quarter_turns: crop.quarter_turns,
        ..Crop::default()
    };
    let r = ds_core::clipboard::adapt_crop(
        &src,
        Size::new(source_width, source_height),
        Size::new(target_width, target_height),
    )
    .rect;
    PastedCrop {
        x: r.x,
        y: r.y,
        w: r.w,
        h: r.h,
        ..crop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pasted(x: f32, y: f32, w: f32, h: f32, long: u32, short: u32) -> PastedCrop {
        PastedCrop {
            x,
            y,
            w,
            h,
            aspect_long: long,
            aspect_short: short,
            quarter_turns: 0,
        }
    }

    /// Pixel width / height of `c` on a `w`×`h` image.
    fn pixel_ratio(c: PastedCrop, w: f64, h: f64) -> f64 {
        f64::from(c.w) * w / (f64::from(c.h) * h)
    }

    #[test]
    fn a_4x5_crop_of_a_portrait_photo_stays_4x5_in_pixels_on_a_landscape_one() {
        // 4:5 filling the width of a 4000x6000 frame: 4000x5000 px.
        let h = 5000.0 / 6000.0;
        let src = pasted(0.0, (1.0 - h) / 2.0, 1.0, h, 5, 4);
        assert!((pixel_ratio(src, 4000.0, 6000.0) - 0.8).abs() < 1e-3);
        let out = adapt_pasted_crop(src, 4000, 6000, 6000, 4000);
        // The crop followed its frame, so on a landscape frame it is 5:4 in pixels.
        let r = pixel_ratio(out, 6000.0, 4000.0);
        assert!((r - 1.25).abs() < 1e-3, "{out:?} ratio {r}");
        assert!(
            out.x >= 0.0
                && out.y >= 0.0
                && out.x + out.w <= 1.0 + 1e-6
                && out.y + out.h <= 1.0 + 1e-6
        );
        assert_eq!(
            (out.aspect_long, out.aspect_short, out.quarter_turns),
            (5, 4, 0)
        );
    }

    #[test]
    fn a_deliberate_portrait_crop_of_a_landscape_frame_keeps_its_orientation() {
        // 4:5 portrait out of a 6000x4000 frame: 3200x4000 px.
        let w = 3200.0 / 6000.0;
        let out = adapt_pasted_crop(pasted(0.2, 0.0, w, 1.0, 5, 4), 6000, 4000, 4000, 6000);
        let r = pixel_ratio(out, 4000.0, 6000.0);
        assert!((r - 0.8).abs() < 1e-3, "{out:?} ratio {r}");
    }

    #[test]
    fn unlocked_and_full_crops_follow_ds_core() {
        let full = pasted(0.0, 0.0, 1.0, 1.0, 0, 0);
        assert_eq!(adapt_pasted_crop(full, 4000, 6000, 6000, 4000), full);
        // Unlocked, the orientation differs: transposed about its center.
        let out = adapt_pasted_crop(pasted(0.1, 0.2, 0.4, 0.2, 0, 0), 6000, 4000, 4000, 6000);
        assert!(
            (out.w - 0.2).abs() < 1e-6 && (out.h - 0.4).abs() < 1e-6,
            "{out:?}"
        );
    }

    #[test]
    fn mirrors_core_order_and_round_trips() {
        let groups = adjust_groups().unwrap();
        assert_eq!(groups.len(), CoreGroup::ALL.len());
        for (info, core) in groups.iter().zip(CoreGroup::ALL) {
            assert_eq!(CoreGroup::from(info.group), core);
            assert_eq!(info.supports_relative, core.supports_relative());
        }
    }

    #[test]
    fn relative_is_limited_to_tone_groups() {
        let rel: Vec<_> = adjust_groups()
            .unwrap()
            .into_iter()
            .filter(|g| g.supports_relative)
            .map(|g| g.group)
            .collect();
        assert_eq!(
            rel,
            vec![
                AdjustGroup::WhiteBalance,
                AdjustGroup::Exposure,
                AdjustGroup::Presence,
                AdjustGroup::Hdr
            ]
        );
    }
}
