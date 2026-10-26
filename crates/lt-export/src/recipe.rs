//! Export recipes: what to produce for each image (Capture One "process recipes").

use std::path::PathBuf;

use lt_core::Size;
use lt_template::Template;

use crate::ExportError;
use crate::post::PostProcessSteps;
use crate::proof::OutputProfile;
use crate::watermark::Watermark;

/// Largest output edge any recipe may produce (JPEG's hard limit is 65 535).
pub const MAX_OUTPUT_EDGE: u32 = 65_500;
/// Largest output pixel count (500 MP ≈ 3 GB as 16-bit RGB): a guard against
/// absurd upscales exhausting memory.
pub const MAX_OUTPUT_PIXELS: u64 = 500_000_000;

/// Output file format.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExportFormat {
    /// Baseline JPEG, quality 1..=100.
    Jpeg {
        quality: u8,
    },
    /// Uncompressed 16-bit RGB TIFF.
    Tiff16,
    Png8,
    Png16,
    /// HEIC via the injected [`crate::HeicEncoder`] (Swift/ImageIO), quality 1..=100.
    Heic {
        quality: u8,
    },
}

impl ExportFormat {
    /// File extension, lowercase, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg { .. } => "jpg",
            Self::Tiff16 => "tif",
            Self::Png8 | Self::Png16 => "png",
            Self::Heic { .. } => "heic",
        }
    }
}

/// How to size the output. Sizes refer to the rendered (oriented, cropped) image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sizing {
    Original,
    LongEdge(u32),
    ShortEdge(u32),
    /// Target pixel count in megapixels (e.g. 12.0).
    Megapixels(f32),
    /// Scale in percent (e.g. 50.0).
    Percent(f32),
}

/// Output color space; its ICC profile is embedded in every file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OutputSpace {
    #[default]
    Srgb,
    DisplayP3,
    /// Rec.2020 primaries, sRGB transfer function: the develop pipeline's working
    /// gamut, unclipped. Only an intermediate for recipes with an output profile.
    Rec2020,
}

/// EXIF passthrough policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExifPolicy {
    /// Copy capture time, camera, lens and exposure from the original.
    pub include: bool,
    /// Drop GPS even when the original has it.
    pub strip_gps: bool,
    /// Write the colour label and pick flag as `Luxtable/…` tags (with the
    /// metadata, see [`crate::xmp::label_pick_tags`]).
    pub label_tags: bool,
}

impl Default for ExifPolicy {
    fn default() -> Self {
        Self {
            include: true,
            strip_gps: false,
            label_tags: true,
        }
    }
}

impl ExifPolicy {
    /// The metadata this policy writes for `meta` (`None`: none at all).
    pub fn dirs(&self, meta: &crate::ExifMeta) -> Option<crate::exif::ExifDirs> {
        self.include.then(|| {
            let mut d = meta.dirs(self.strip_gps);
            if !self.label_tags {
                d.organize
                    .tags
                    .retain(|t| t.first().map(String::as_str) != Some(crate::xmp::TAG_ROOT));
            }
            d
        })
    }
}

/// One output variant of an image.
#[derive(Debug, Clone)]
pub struct ExportRecipe {
    pub name: String,
    pub format: ExportFormat,
    pub sizing: Sizing,
    pub allow_upscale: bool,
    pub space: OutputSpace,
    /// Output sharpening (unsharp mask amount), 0 = off, typical 0.3..1.0, max 5.
    pub sharpen: f32,
    /// File name template (see `lt-template`); `.ext` is appended unless it has `{ext}`.
    pub file_name: Template,
    /// Destination directory (created if missing).
    pub destination: PathBuf,
    pub exif: ExifPolicy,
    /// A straightened or lens-corrected frame is exported at its inscribed size (1:1
    /// sensor pixels) instead of being scaled back up to the original size. Only the
    /// develop renderer reads it ([`crate::Renderer::render_for`]).
    pub constrain_to_sensor_pixels: bool,
    /// Convert to this ICC profile (e.g. the soft-proof profile) and embed it instead
    /// of `space`'s profile; `space` is then what the renderer delivers (lt-ffi asks for
    /// [`OutputSpace::Rec2020`], the working gamut, so nothing is clipped before the
    /// conversion).
    pub output_profile: Option<OutputProfile>,
    /// AI Upscale: the recipe may enlarge (as with `allow_upscale`), and an enlargement
    /// starts from the image upscaled 2× by the app's model
    /// ([`crate::batch::Upscaler`]), resampled to the size; without a model it is
    /// resampled from the render.
    pub ai_upscale: bool,
    /// Drawn on each output after resizing and sharpening.
    pub watermark: Option<Watermark>,
    /// Plug-in steps drawn after the watermark, in order ([`crate::post`]).
    pub post_process: PostProcessSteps,
}

impl ExportRecipe {
    /// A recipe with sensible defaults: original size, sRGB, no sharpening, `{name}`.
    pub fn new(name: &str, format: ExportFormat, destination: impl Into<PathBuf>) -> Self {
        Self {
            name: name.to_owned(),
            format,
            sizing: Sizing::Original,
            allow_upscale: false,
            space: OutputSpace::Srgb,
            sharpen: 0.0,
            file_name: Template::parse("{name}").expect("static template"),
            destination: destination.into(),
            exif: ExifPolicy::default(),
            constrain_to_sensor_pixels: false,
            output_profile: None,
            ai_upscale: false,
            watermark: None,
            post_process: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ExportError> {
        let bad = |m: String| Err(ExportError::InvalidRecipe(format!("{}: {m}", self.name)));
        match self.format {
            ExportFormat::Jpeg { quality } | ExportFormat::Heic { quality }
                if !(1..=100).contains(&quality) =>
            {
                return bad(format!("quality {quality} not in 1..=100"));
            }
            _ => {}
        }
        match self.sizing {
            Sizing::LongEdge(0) | Sizing::ShortEdge(0) => return bad("zero edge".into()),
            Sizing::LongEdge(e) | Sizing::ShortEdge(e) if e > MAX_OUTPUT_EDGE => {
                return bad(format!("edge {e} exceeds {MAX_OUTPUT_EDGE}"));
            }
            Sizing::Megapixels(v) | Sizing::Percent(v) if !(v.is_finite() && v > 0.0) => {
                return bad(format!("size factor {v} must be positive"));
            }
            _ => {}
        }
        if !(self.sharpen.is_finite() && (0.0..=5.0).contains(&self.sharpen)) {
            return bad(format!("sharpen {} not in 0..=5", self.sharpen));
        }
        if let Some(p) = &self.output_profile
            && p.profile.is_linear()
            && matches!(
                self.format,
                ExportFormat::Jpeg { .. } | ExportFormat::Png8 | ExportFormat::Heic { .. }
            )
        {
            return bad(format!(
                "{} is linear-light, which bands in an 8-bit file; use TIFF 16 or PNG 16",
                p.profile.name()
            ));
        }
        if let Some(wm) = &self.watermark {
            wm.validate()
                .map_err(|m| ExportError::InvalidRecipe(format!("{}: {m}", self.name)))?;
        }
        Ok(())
    }

    /// Output size for a rendered image of `src` size.
    pub fn target_size(&self, src: Size) -> Result<Size, ExportError> {
        target_size(src, self.sizing, self.allow_upscale || self.ai_upscale)
    }
}

/// Compute the output size. Never upscales unless `allow_upscale`; never
/// returns a zero dimension; errors when the result exceeds [`MAX_OUTPUT_EDGE`]
/// or [`MAX_OUTPUT_PIXELS`].
pub fn target_size(src: Size, sizing: Sizing, allow_upscale: bool) -> Result<Size, ExportError> {
    if src.width == 0 || src.height == 0 {
        return Err(ExportError::InvalidImage(format!("empty image {src:?}")));
    }
    let (w, h) = (f64::from(src.width), f64::from(src.height));
    let scale = match sizing {
        Sizing::Original => 1.0,
        Sizing::LongEdge(e) => f64::from(e) / w.max(h),
        Sizing::ShortEdge(e) => f64::from(e) / w.min(h),
        Sizing::Megapixels(mp) => (f64::from(mp) * 1e6 / (w * h)).sqrt(),
        Sizing::Percent(p) => f64::from(p) / 100.0,
    };
    let scale = if allow_upscale { scale } else { scale.min(1.0) };
    if (scale - 1.0).abs() < 1e-12 {
        return check_max(src);
    }
    let mut out = Size::new(
        ((w * scale).round() as u32).max(1),
        ((h * scale).round() as u32).max(1),
    );
    // Hit the requested edge exactly (rounding of the scale can be off by one).
    // Reaching here means the scale was not clamped, so `e` is attainable.
    match sizing {
        Sizing::LongEdge(e) if w >= h => out.width = e,
        Sizing::ShortEdge(e) if w <= h => out.width = e,
        Sizing::LongEdge(e) | Sizing::ShortEdge(e) => out.height = e,
        _ => {}
    }
    check_max(out)
}

fn check_max(s: Size) -> Result<Size, ExportError> {
    if s.width > MAX_OUTPUT_EDGE || s.height > MAX_OUTPUT_EDGE || s.pixels() > MAX_OUTPUT_PIXELS {
        return Err(ExportError::TooLarge(s));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sz(w: u32, h: u32) -> Size {
        Size::new(w, h)
    }

    #[test]
    fn exif_policy_writes_label_tags_only_when_asked() {
        let meta = crate::ExifMeta {
            organize: crate::xmp::Organize {
                rating: Some(3),
                tags: vec![
                    vec!["Luxtable".into(), "Pick".into()],
                    vec!["Places".into(), "Coast".into()],
                ],
            },
            ..crate::ExifMeta::default()
        };
        let all = ExifPolicy::default().dirs(&meta).unwrap();
        assert_eq!(all.organize, meta.organize);
        let without = ExifPolicy {
            label_tags: false,
            ..ExifPolicy::default()
        }
        .dirs(&meta)
        .unwrap();
        assert_eq!(without.organize.rating, Some(3), "the rating stays");
        assert_eq!(
            without.organize.tags,
            [vec!["Places".to_owned(), "Coast".to_owned()]]
        );
        let none = ExifPolicy {
            include: false,
            ..ExifPolicy::default()
        };
        assert!(none.dirs(&meta).is_none());
    }

    #[test]
    fn edges_hit_exactly_both_orientations() {
        let land = sz(6000, 4000);
        assert_eq!(
            target_size(land, Sizing::LongEdge(2048), false).unwrap(),
            sz(2048, 1365)
        );
        assert_eq!(
            target_size(land, Sizing::ShortEdge(1000), false).unwrap(),
            sz(1500, 1000)
        );
        let port = sz(4000, 6000);
        assert_eq!(
            target_size(port, Sizing::LongEdge(2048), false).unwrap(),
            sz(1365, 2048)
        );
        assert_eq!(
            target_size(port, Sizing::ShortEdge(1000), false).unwrap(),
            sz(1000, 1500)
        );
    }

    #[test]
    fn megapixels_percent_original() {
        let s = target_size(sz(6000, 4000), Sizing::Megapixels(6.0), false).unwrap();
        assert_eq!(s, sz(3000, 2000));
        assert_eq!(
            target_size(sz(6000, 4000), Sizing::Percent(25.0), false).unwrap(),
            sz(1500, 1000)
        );
        assert_eq!(
            target_size(sz(6000, 4000), Sizing::Original, false).unwrap(),
            sz(6000, 4000)
        );
        // Never zero.
        assert_eq!(
            target_size(sz(1000, 1), Sizing::Percent(10.0), false).unwrap(),
            sz(100, 1)
        );
    }

    #[test]
    fn no_upscale_unless_allowed() {
        let small = sz(800, 600);
        assert_eq!(
            target_size(small, Sizing::LongEdge(2048), false).unwrap(),
            small
        );
        assert_eq!(
            target_size(small, Sizing::ShortEdge(1000), false).unwrap(),
            small
        );
        assert_eq!(
            target_size(small, Sizing::Percent(200.0), false).unwrap(),
            small
        );
        assert_eq!(
            target_size(small, Sizing::LongEdge(1600), true).unwrap(),
            sz(1600, 1200)
        );
        assert_eq!(
            target_size(sz(600, 800), Sizing::ShortEdge(1200), true).unwrap(),
            sz(1200, 1600)
        );
    }

    #[test]
    fn huge_and_empty_rejected() {
        assert!(matches!(
            target_size(sz(6000, 4000), Sizing::Percent(5000.0), true),
            Err(ExportError::TooLarge(_))
        ));
        assert!(matches!(
            target_size(sz(70_000, 10), Sizing::Original, false),
            Err(ExportError::TooLarge(_))
        ));
        assert!(matches!(
            target_size(sz(2, 1), Sizing::LongEdge(65_000), true),
            Err(ExportError::TooLarge(_))
        ));
        assert!(matches!(
            target_size(sz(0, 10), Sizing::Original, false),
            Err(ExportError::InvalidImage(_))
        ));
    }

    #[test]
    fn recipe_validation() {
        let base = ExportRecipe::new("r", ExportFormat::Jpeg { quality: 90 }, "/tmp/x");
        assert!(base.validate().is_ok());
        assert_eq!(base.target_size(sz(10, 10)).unwrap(), sz(10, 10));
        let mut r = base.clone();
        r.format = ExportFormat::Heic { quality: 0 };
        assert!(r.validate().is_err());
        let mut r = base.clone();
        r.sizing = Sizing::LongEdge(0);
        assert!(r.validate().is_err());
        r.sizing = Sizing::ShortEdge(MAX_OUTPUT_EDGE + 1);
        assert!(r.validate().is_err());
        r.sizing = Sizing::Megapixels(f32::NAN);
        assert!(r.validate().is_err());
        r.sizing = Sizing::Percent(-1.0);
        assert!(r.validate().is_err());
        let mut r = base;
        r.sharpen = 6.0;
        let e = r.validate().unwrap_err();
        assert!(e.to_string().contains("sharpen"), "{e}");
    }

    #[test]
    fn extensions() {
        assert_eq!(ExportFormat::Jpeg { quality: 1 }.extension(), "jpg");
        assert_eq!(ExportFormat::Tiff16.extension(), "tif");
        assert_eq!(ExportFormat::Png8.extension(), "png");
        assert_eq!(ExportFormat::Png16.extension(), "png");
        assert_eq!(ExportFormat::Heic { quality: 1 }.extension(), "heic");
    }
}
