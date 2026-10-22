//! Export watermarks (`ds_export::watermark`): the recipe's [`WatermarkSpec`],
//! the app's text renderer ([`WatermarkTextRenderer`], Core Text) and the recipe
//! editor's live preview ([`watermark_preview`]).
//!
//! Logos are decoded once per file version and shared (a small cache keyed by
//! path, size and modification time), so validating recipes on every edit and
//! exporting thousands of photos read each PNG once.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ds_core::Size;
use ds_export::watermark::{self, Canvas};
use ds_export::{Anchor, ExportItem, Logo, Mark, OutputSpace, TextMask, Watermark};

use crate::error::DsError;
use crate::jobs::guard;

/// Where the watermark sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum WatermarkAnchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// What a watermark draws.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum WatermarkContent {
    /// One line of text. Tokens: `{copyright}`, `{artist}`, `{year}`, `{filename}`.
    Text {
        text: String,
        /// Font family; empty = the system font.
        font: String,
        /// Font size as a fraction of the output's short edge (0…0.5].
        size: f32,
        /// sRGB colour, `0xRRGGBB`.
        color: u32,
        shadow: bool,
    },
    /// A PNG logo, `scale` = its width as a fraction of the output's width (0…1].
    Logo { path: String, scale: f32 },
}

/// A recipe's watermark.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct WatermarkSpec {
    pub content: WatermarkContent,
    pub anchor: WatermarkAnchor,
    /// Margin from the anchored edges, a fraction of the short edge (0…0.25).
    pub inset: f32,
    /// 0…1.
    pub opacity: f32,
}

/// One line of text as coverage (see `ds_export::TextMask`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WatermarkTextMask {
    pub width: u32,
    pub height: u32,
    /// `width * height` bytes, row-major from the top, 255 = ink.
    pub coverage: Vec<u8>,
}

/// Implemented in Swift with Core Text. Called on export worker threads (and on
/// the main thread for the preview); must be thread-safe.
#[uniffi::export(with_foreign)]
pub trait WatermarkTextRenderer: Send + Sync {
    /// One line of `text` in the family `font` ("" = the system font) at
    /// `pixel_size` px per em: its typographic box (advance width × ascent +
    /// descent, rounded up) with the glyphs' coverage.
    fn rasterize(
        &self,
        text: String,
        font: String,
        pixel_size: f32,
    ) -> Result<WatermarkTextMask, DsError>;
}

/// `ds_export`'s text seam over the app's renderer.
pub(crate) struct TextHook(pub Arc<dyn WatermarkTextRenderer>);

impl ds_export::TextRasterizer for TextHook {
    fn rasterize(&self, text: &str, font: &str, pixel_size: f32) -> Result<TextMask, String> {
        self.0
            .rasterize(text.to_owned(), font.to_owned(), pixel_size)
            .map(|m| TextMask {
                width: m.width,
                height: m.height,
                coverage: m.coverage,
            })
            .map_err(|e| e.to_string())
    }
}

fn anchor_of(a: WatermarkAnchor) -> Anchor {
    match a {
        WatermarkAnchor::TopLeft => Anchor::TopLeft,
        WatermarkAnchor::Top => Anchor::Top,
        WatermarkAnchor::TopRight => Anchor::TopRight,
        WatermarkAnchor::Left => Anchor::Left,
        WatermarkAnchor::Center => Anchor::Center,
        WatermarkAnchor::Right => Anchor::Right,
        WatermarkAnchor::BottomLeft => Anchor::BottomLeft,
        WatermarkAnchor::Bottom => Anchor::Bottom,
        WatermarkAnchor::BottomRight => Anchor::BottomRight,
    }
}

/// The `ds_export` watermark of `spec` (its logo decoded, or taken from the cache),
/// or why it can't be used.
pub(crate) fn watermark_of(spec: &WatermarkSpec) -> Result<Watermark, String> {
    let mark = match &spec.content {
        WatermarkContent::Text {
            text,
            font,
            size,
            color,
            shadow,
        } => Mark::Text {
            text: text.clone(),
            font: font.clone(),
            size: *size,
            color: [(color >> 16) as u8, (color >> 8) as u8, *color as u8],
            shadow: *shadow,
        },
        WatermarkContent::Logo { path, .. } if path.is_empty() => {
            return Err("choose a PNG file for the watermark logo".into());
        }
        WatermarkContent::Logo { path, scale } => Mark::Logo {
            logo: logo(Path::new(path))?,
            scale: *scale,
        },
    };
    let wm = Watermark {
        mark,
        anchor: anchor_of(spec.anchor),
        inset: spec.inset,
        opacity: spec.opacity,
    };
    wm.validate()?;
    Ok(wm)
}

/// File identity: a changed logo (new size or time) is decoded again.
type Stamp = (u64, Option<SystemTime>);

/// Decoded logos kept (most recent last) are bounded by their pixel bytes.
const LOGO_CACHE_BYTES: usize = 128 << 20;
const _: () = assert!(LOGO_CACHE_BYTES >= 64 << 20, "one 4096² logo fits");

type CacheEntry = (PathBuf, Stamp, Arc<Logo>);

/// Drop the least recently used logos until the rest hold at most `cap` bytes
/// (the newest is always kept, however large).
fn trim(cache: &mut Vec<CacheEntry>, cap: usize) {
    let mut total: usize = cache.iter().map(|e| e.2.bytes()).sum();
    while cache.len() > 1 && total > cap {
        total -= cache.remove(0).2.bytes();
    }
}

fn logo(path: &Path) -> Result<Arc<Logo>, String> {
    static CACHE: Mutex<Vec<CacheEntry>> = Mutex::new(Vec::new());
    let meta =
        std::fs::metadata(path).map_err(|e| format!("watermark logo {}: {e}", path.display()))?;
    let stamp = (meta.len(), meta.modified().ok());
    {
        let mut c = CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(i) = c.iter().position(|(p, s, _)| p == path && *s == stamp) {
            let hit = c.remove(i);
            let logo = hit.2.clone();
            c.push(hit);
            return Ok(logo);
        }
    }
    let logo = Arc::new(Logo::load(path).map_err(|e| e.to_string())?);
    let mut c = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    c.retain(|(p, _, _)| p != path);
    c.push((path.to_path_buf(), stamp, logo.clone()));
    trim(&mut c, LOGO_CACHE_BYTES);
    Ok(logo)
}

/// What the preview fills the tokens with.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WatermarkSample {
    /// `{filename}`: the original's name without extension.
    pub name: String,
    pub copyright: Option<String>,
    pub artist: Option<String>,
    /// Camera wall-clock time, `YYYY-MM-DDTHH:MM:SS` (`{year}`; `None`: this year).
    pub capture_time: Option<String>,
}

/// [`watermark_preview`]'s result.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WatermarkPreviewImage {
    /// The image with the watermark drawn.
    pub rgba: Vec<u8>,
    /// A text watermark's line for this photo (tokens filled in): blank means the
    /// export leaves it out. `None` for a logo.
    pub text: Option<String>,
}

/// The recipe editor's preview: `watermark` drawn onto an sRGB RGBA8 image
/// (`rgba.len() == width * height * 4`) exactly as the export draws it. The
/// sample's credits are what the export would use (the app's, else the photo's).
/// Blocking (may decode the logo): call it off the main thread.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn watermark_preview(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    watermark: WatermarkSpec,
    sample: WatermarkSample,
    text: Option<Arc<dyn WatermarkTextRenderer>>,
) -> Result<WatermarkPreviewImage, DsError> {
    guard(|| {
        let mut rgba = rgba;
        let size = Size::new(width, height);
        if width == 0
            || height == 0
            || rgba.len() as u64 != u64::from(width) * u64::from(height) * 4
        {
            return Err(DsError::invalid(format!(
                "preview buffer has {} bytes for {width}×{height}",
                rgba.len()
            )));
        }
        let wm = watermark_of(&watermark).map_err(DsError::invalid)?;
        let mut item = ExportItem {
            name: sample.name,
            ..ExportItem::default()
        };
        item.meta.copyright = sample.copyright;
        item.meta.artist = sample.artist;
        item.meta.capture_time = sample
            .capture_time
            .as_deref()
            .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok());
        let line = wm.text_for(&item);
        let hook = text.map(TextHook);
        watermark::apply(
            Canvas::Rgba8 {
                size,
                data: &mut rgba,
            },
            OutputSpace::Srgb,
            &wm,
            &item,
            hook.as_ref().map(|h| h as &dyn ds_export::TextRasterizer),
        )
        .map_err(|e| DsError::invalid(e.to_string()))?;
        Ok(WatermarkPreviewImage { rgba, text: line })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Core Text stand-in: a solid box, 0.5 em per character × 1 em.
    struct Blocks;

    impl WatermarkTextRenderer for Blocks {
        fn rasterize(
            &self,
            text: String,
            font: String,
            pixel_size: f32,
        ) -> Result<WatermarkTextMask, DsError> {
            if font == "Missing" {
                return Err(DsError::invalid("no such font"));
            }
            let h = pixel_size.round() as u32;
            let w = (pixel_size * 0.5 * text.chars().count() as f32).round() as u32;
            Ok(WatermarkTextMask {
                width: w,
                height: h,
                coverage: vec![255; (w * h) as usize],
            })
        }
    }

    fn text_spec(text: &str, color: u32) -> WatermarkSpec {
        WatermarkSpec {
            content: WatermarkContent::Text {
                text: text.into(),
                font: String::new(),
                size: 0.1,
                color,
                shadow: false,
            },
            anchor: WatermarkAnchor::TopLeft,
            inset: 0.0,
            opacity: 1.0,
        }
    }

    fn sample() -> WatermarkSample {
        WatermarkSample {
            name: "DSC1".into(),
            copyright: Some("(c) S".into()),
            artist: None,
            capture_time: Some("2023-02-03T04:05:06".into()),
        }
    }

    fn png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
        let f = std::fs::File::create(path).unwrap();
        let mut enc = ::png::Encoder::new(f, w, h);
        enc.set_color(::png::ColorType::Rgba);
        enc.set_depth(::png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(rgba).unwrap();
        wr.finish().unwrap();
    }

    #[test]
    fn preview_draws_text_with_its_colour_at_its_anchor() {
        let rgba = vec![0u8; 100 * 50 * 4];
        let out = watermark_preview(
            100,
            50,
            rgba,
            text_spec("{year}", 0x00_FF_80),
            sample(),
            Some(Arc::new(Blocks)),
        )
        .unwrap();
        assert_eq!(out.text.as_deref(), Some("2023"));
        let out = out.rgba;
        // "2023": 4 × 2.5 px wide, 5 px tall, top-left.
        assert_eq!(&out[..4], &[0, 255, 128, 0]);
        let at = |x: usize, y: usize| &out[(y * 100 + x) * 4..][..4];
        assert_eq!(at(9, 4), &[0, 255, 128, 0]);
        assert_eq!(at(10, 0), &[0, 0, 0, 0]);
        assert_eq!(at(0, 5), &[0, 0, 0, 0]);
    }

    #[test]
    fn preview_errors() {
        let e =
            watermark_preview(2, 2, vec![0; 15], text_spec("x", 0), sample(), None).unwrap_err();
        assert!(e.to_string().contains("15 bytes"), "{e}");
        assert!(watermark_preview(0, 2, vec![], text_spec("x", 0), sample(), None).is_err());
        let e =
            watermark_preview(4, 4, vec![0; 64], text_spec("x", 0), sample(), None).unwrap_err();
        assert!(e.to_string().contains("text renderer"), "{e}");
        let mut bad = text_spec("x", 0);
        if let WatermarkContent::Text { font, .. } = &mut bad.content {
            *font = "Missing".into();
        }
        let e = watermark_preview(4, 4, vec![0; 64], bad, sample(), Some(Arc::new(Blocks)))
            .unwrap_err();
        assert!(e.to_string().contains("no such font"), "{e}");
        let mut bad = text_spec("x", 0);
        bad.opacity = 2.0;
        let e = watermark_preview(4, 4, vec![0; 64], bad, sample(), None).unwrap_err();
        assert!(e.to_string().contains("opacity"), "{e}");
    }

    #[test]
    fn every_anchor_maps() {
        use WatermarkAnchor as W;
        let all = [
            (W::TopLeft, Anchor::TopLeft),
            (W::Top, Anchor::Top),
            (W::TopRight, Anchor::TopRight),
            (W::Left, Anchor::Left),
            (W::Center, Anchor::Center),
            (W::Right, Anchor::Right),
            (W::BottomLeft, Anchor::BottomLeft),
            (W::Bottom, Anchor::Bottom),
            (W::BottomRight, Anchor::BottomRight),
        ];
        for (w, a) in all {
            assert_eq!(anchor_of(w), a);
        }
    }

    #[test]
    fn logos_are_decoded_once_per_file_version() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("logo.png");
        png(&p, 2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]);
        let a = logo(&p).unwrap();
        let b = logo(&p).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "cached");
        // A new version (different size) is decoded again.
        png(&p, 3, 1, &[255; 12]);
        let c = logo(&p).unwrap();
        assert_eq!(c.size(), Size::new(3, 1));
        // Other logos come and go; this one is still right.
        for i in 0..3 {
            let q = d.path().join(format!("l{i}.png"));
            png(&q, 1, 1, &[0, 0, 0, 255]);
            assert_eq!(logo(&q).unwrap().size(), Size::new(1, 1));
        }
        assert_eq!(logo(&p).unwrap().size(), Size::new(3, 1));
        // Missing and broken files are invalid arguments that name the file.
        let e = logo(&d.path().join("none.png")).unwrap_err();
        assert!(e.contains("none.png"), "{e}");
        let junk = d.path().join("junk.png");
        std::fs::write(&junk, b"junk").unwrap();
        let e = logo(&junk).unwrap_err();
        assert!(e.contains("junk.png"), "{e}");
    }

    /// The cache is bounded by decoded bytes, least recently used out first; the
    /// newest logo stays even when it alone is over the cap.
    #[test]
    fn logo_cache_is_bounded_by_bytes() {
        let d = tempfile::tempdir().unwrap();
        let entry = |name: &str, w: u32| -> CacheEntry {
            let p = d.path().join(name);
            png(&p, w, 1, &vec![255; w as usize * 4]);
            (p.clone(), (0, None), Arc::new(Logo::load(&p).unwrap()))
        };
        // 4, 8 and 12 bytes.
        let mut c = vec![entry("a.png", 1), entry("b.png", 2), entry("c.png", 3)];
        trim(&mut c, 24);
        assert_eq!(c.len(), 3, "24 bytes fit");
        trim(&mut c, 20);
        assert_eq!(
            c.iter().map(|e| e.2.size().width).collect::<Vec<_>>(),
            [2, 3]
        );
        trim(&mut c, 1);
        assert_eq!(c.iter().map(|e| e.2.size().width).collect::<Vec<_>>(), [3]);
    }

    /// A text that is blank once filled in (no copyright) is reported, and nothing is drawn.
    #[test]
    fn preview_reports_a_blank_line() {
        let mut s = sample();
        s.copyright = None;
        let out = watermark_preview(
            8,
            8,
            vec![0; 256],
            text_spec("{copyright}", 0xFF_FF_FF),
            s,
            Some(Arc::new(Blocks)),
        )
        .unwrap();
        assert_eq!(out.text.as_deref(), Some(""));
        assert!(out.rgba.iter().all(|&v| v == 0));
    }

    #[test]
    fn logo_preview_and_spec_conversion() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("logo.png");
        png(&p, 2, 2, &[255; 16]);
        let spec = WatermarkSpec {
            content: WatermarkContent::Logo {
                path: p.to_string_lossy().into_owned(),
                scale: 0.5,
            },
            anchor: WatermarkAnchor::BottomRight,
            inset: 0.0,
            opacity: 1.0,
        };
        let wm = watermark_of(&spec).unwrap();
        assert!(matches!(wm.mark, Mark::Logo { scale, .. } if (scale - 0.5).abs() < 1e-6));
        let out = watermark_preview(4, 4, vec![0; 64], spec, sample(), None).unwrap();
        assert_eq!(out.text, None, "a logo has no text");
        let lit: Vec<usize> = out
            .rgba
            .chunks(4)
            .enumerate()
            .filter(|(_, p)| p[0] == 255)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(lit, [10, 11, 14, 15], "a 2×2 white square bottom-right");
        let empty = WatermarkSpec {
            content: WatermarkContent::Logo {
                path: String::new(),
                scale: 0.5,
            },
            ..text_spec("x", 0)
        };
        assert!(watermark_of(&empty).unwrap_err().contains("choose a PNG"));
        let wm = watermark_of(&text_spec("x", 0x12_34_56)).unwrap();
        assert!(matches!(
            wm.mark,
            Mark::Text {
                color: [0x12, 0x34, 0x56],
                ..
            }
        ));
    }

    #[test]
    fn text_hook_passes_through() {
        let hook = TextHook(Arc::new(Blocks));
        let m = ds_export::TextRasterizer::rasterize(&hook, "ab", "", 4.0).unwrap();
        assert_eq!((m.width, m.height, m.coverage.len()), (4, 4, 16));
        let e = ds_export::TextRasterizer::rasterize(&hook, "ab", "Missing", 4.0).unwrap_err();
        assert!(e.contains("no such font"), "{e}");
    }
}
