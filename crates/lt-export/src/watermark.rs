//! Watermarks: a line of text or a PNG logo composited onto each output after it
//! has been resized and sharpened. Sizes are fractions of the output, so a
//! watermark keeps its relative size and place at every export size.
//!
//! Text is rasterised by an injected [`TextRasterizer`] (Core Text on the Swift
//! side: the system's fonts, kerning and glyph fallback) as a coverage mask;
//! colour, shadow, opacity and placement are applied here. Colours are worked out
//! in linear light of the output space (the text colour and the logo, both sRGB,
//! are converted to Display P3 for P3 outputs; a logo is resampled premultiplied in
//! linear light), but the "over" itself blends **in the output's encoded (gamma)
//! values**, as Lightroom and Photoshop do: 50% white over black is ~128/255, and
//! text edges look as Core Text draws them. Only the watermark's rectangle is
//! touched, row by row (a logo is resampled one output row at a time), so no
//! image-sized buffer is allocated.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use chrono::Datelike;
use lt_core::Size;
use rayon::prelude::*;

use crate::ExportError;
use crate::batch::ExportItem;
use crate::image::Image16;
use crate::process::{encode, encode_lut, srgb_to_linear, taps};
use crate::recipe::OutputSpace;

/// Where the watermark sits: a corner, an edge's middle or the centre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    #[default]
    BottomRight,
}

impl Anchor {
    /// Column and row in the 3×3 grid: 0 = left/top, 1 = middle, 2 = right/bottom.
    fn grid(self) -> (u8, u8) {
        match self {
            Self::TopLeft => (0, 0),
            Self::Top => (1, 0),
            Self::TopRight => (2, 0),
            Self::Left => (0, 1),
            Self::Center => (1, 1),
            Self::Right => (2, 1),
            Self::BottomLeft => (0, 2),
            Self::Bottom => (1, 2),
            Self::BottomRight => (2, 2),
        }
    }
}

/// What is drawn.
#[derive(Debug, Clone)]
pub enum Mark {
    /// One line of text; tokens ([`expand_tokens`]) are filled in per photo.
    Text {
        text: String,
        /// Font family; empty = the system font.
        font: String,
        /// Font size (em) as a fraction of the output's short edge.
        size: f32,
        /// sRGB colour.
        color: [u8; 3],
        /// A soft dark drop shadow behind the text.
        shadow: bool,
    },
    /// A PNG logo (its alpha kept).
    Logo {
        logo: Arc<Logo>,
        /// Logo width as a fraction of the output's width.
        scale: f32,
    },
}

/// A watermark of one export recipe.
#[derive(Debug, Clone)]
pub struct Watermark {
    pub mark: Mark,
    pub anchor: Anchor,
    /// Margin from the anchored edges, a fraction of the output's short edge.
    pub inset: f32,
    /// 0 = invisible … 1 = opaque.
    pub opacity: f32,
}

/// Largest text size and inset (fractions of the short edge).
pub const MAX_TEXT_SIZE: f32 = 0.5;
pub const MAX_INSET: f32 = 0.25;

impl Watermark {
    /// Why this watermark can't be used, or `Ok`.
    pub fn validate(&self) -> Result<(), String> {
        let unit = |v: f32, max: f32| v.is_finite() && v >= 0.0 && v <= max;
        if !unit(self.opacity, 1.0) {
            return Err(format!("watermark opacity {} not in 0…1", self.opacity));
        }
        if !unit(self.inset, MAX_INSET) {
            return Err(format!(
                "watermark margin {} not in 0…{MAX_INSET}",
                self.inset
            ));
        }
        match &self.mark {
            Mark::Text { text, size, .. } => {
                if text.trim().is_empty() {
                    return Err("watermark text is empty".into());
                }
                if !(unit(*size, MAX_TEXT_SIZE) && *size > 0.0) {
                    return Err(format!(
                        "watermark text size {size} not in 0…{MAX_TEXT_SIZE}"
                    ));
                }
            }
            Mark::Logo { scale, .. } => {
                if !(unit(*scale, 1.0) && *scale > 0.0) {
                    return Err(format!("watermark logo scale {scale} not in 0…1"));
                }
            }
        }
        Ok(())
    }

    /// The line this watermark draws for `item` (tokens filled in, newlines made
    /// spaces), or `None` for a logo. Blank when the tokens left nothing to draw.
    pub fn text_for(&self, item: &ExportItem) -> Option<String> {
        match &self.mark {
            Mark::Text { text, .. } => Some(expand_tokens(text, item).replace(['\n', '\r'], " ")),
            Mark::Logo { .. } => None,
        }
    }

    /// Nothing is drawn for `item`: its text is blank once the tokens are filled in
    /// (e.g. `{copyright}` for a photo without one).
    pub fn skips(&self, item: &ExportItem) -> bool {
        self.text_for(item).is_some_and(|t| t.trim().is_empty())
    }
}

/// One line of text as coverage: `width`×`height` bytes, row-major, 255 = ink.
/// The box is the line's typographic box (advance width × ascent + descent), so
/// the same text sits at the same place whatever its glyphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMask {
    pub width: u32,
    pub height: u32,
    pub coverage: Vec<u8>,
}

/// Rasterises text (the app's Core Text). Called from export workers.
pub trait TextRasterizer: Sync {
    /// `text` (one line) in the family `font` ("" = the system font) at
    /// `pixel_size` px per em.
    fn rasterize(&self, text: &str, font: &str, pixel_size: f32) -> Result<TextMask, String>;
}

/// A decoded logo: 8-bit sRGB RGBA, straight alpha, as the PNG stores it (4 bytes
/// a pixel); it is linearised and premultiplied per filter tap while drawn.
pub struct Logo {
    size: Size,
    px: Vec<[u8; 4]>,
    /// The pixels premultiplied in linear light, made once on first use (every tap of
    /// every output row reads them); `None` for a logo too large to hold twice.
    lin: OnceLock<Option<Vec<[f32; 4]>>>,
}

impl std::fmt::Debug for Logo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Logo({}×{})", self.size.width, self.size.height)
    }
}

impl Logo {
    /// Largest logo edge accepted (a 4096² logo holds 64 MB).
    pub const MAX_EDGE: u32 = 4096;

    /// Largest logo file read (a PNG of a 4096² logo can be far smaller than its pixels,
    /// so this bounds the read, not the decode).
    pub const MAX_FILE_BYTES: u64 = 64 << 20;

    /// Most pixels kept linearised (16 bytes each, 64 MB at the limit): a bigger logo
    /// is linearised per tap instead.
    const LINEAR_CACHE_PIXELS: usize = 1 << 22;

    pub fn size(&self) -> Size {
        self.size
    }

    /// Bytes held by the decoded pixels.
    pub fn bytes(&self) -> usize {
        self.px.len() * 4
    }

    /// Pixel `i` premultiplied in linear sRGB.
    #[inline]
    fn linear(&self, i: usize) -> [f32; 4] {
        let cached = self
            .lin
            .get_or_init(|| {
                (self.px.len() <= Self::LINEAR_CACHE_PIXELS).then(|| {
                    (0..self.px.len())
                        .map(|i| self.linear_uncached(i))
                        .collect()
                })
            })
            .as_deref();
        match cached {
            Some(lin) => lin[i],
            None => self.linear_uncached(i),
        }
    }

    fn linear_uncached(&self, i: usize) -> [f32; 4] {
        let p = self.px[i];
        let a = f32::from(p[3]) / 255.0;
        let lut = srgb8_lut();
        [
            lut[usize::from(p[0])] * a,
            lut[usize::from(p[1])] * a,
            lut[usize::from(p[2])] * a,
            a,
        ]
    }

    /// Read and decode a PNG logo.
    pub fn load(path: &Path) -> Result<Self, ExportError> {
        let io = |source| ExportError::Io {
            path: path.to_path_buf(),
            source,
        };
        // One byte past the limit: an oversize (or endless) file is refused unread.
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(io)?
            .take(Self::MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 > Self::MAX_FILE_BYTES {
            return Err(ExportError::Watermark(format!(
                "logo {} is larger than {} MB",
                path.display(),
                Self::MAX_FILE_BYTES >> 20
            )));
        }
        Self::decode_png(&bytes)
            .map_err(|m| ExportError::Watermark(format!("logo {}: {m}", path.display())))
    }

    /// Decode PNG bytes (grey, grey+alpha, RGB, RGBA, palette; 8 or 16 bits).
    /// Colour is taken as sRGB.
    pub fn decode_png(bytes: &[u8]) -> Result<Self, String> {
        let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
        dec.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = dec.read_info().map_err(|e| e.to_string())?;
        let (w, h) = (reader.info().width, reader.info().height);
        if w > Self::MAX_EDGE || h > Self::MAX_EDGE {
            return Err(format!("{w}×{h} is larger than {0}×{0}", Self::MAX_EDGE));
        }
        let mut buf = vec![0u8; reader.output_buffer_size().ok_or("too large")?];
        let frame = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        let channels = frame.color_type.samples();
        let px = buf[..frame.buffer_size()]
            .chunks_exact(channels)
            .map(|p| match p {
                [g] => [*g, *g, *g, 255],
                [g, a] => [*g, *g, *g, *a],
                [r, g, b] => [*r, *g, *b, 255],
                [r, g, b, a, ..] => [*r, *g, *b, *a],
                [] => unreachable!("chunks are never empty"),
            })
            .collect();
        Ok(Self {
            size: Size::new(w, h),
            px,
            lin: OnceLock::new(),
        })
    }
}

/// `{copyright}`, `{artist}`, `{filename}` (the original's name, without
/// extension) and `{year}` (the capture year, else this year) filled in for `item`.
/// One pass over `text`: a substituted value is never scanned again, so a file named
/// `a{year}b` stays that.
pub fn expand_tokens(text: &str, item: &ExportItem) -> String {
    let year = item
        .meta
        .capture_time
        .map_or_else(|| chrono::Local::now().year(), |t| t.year())
        .to_string();
    let tokens = [
        ("{copyright}", item.meta.copyright.as_deref().unwrap_or("")),
        ("{artist}", item.meta.artist.as_deref().unwrap_or("")),
        ("{filename}", item.name.as_str()),
        ("{year}", year.as_str()),
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some((t, value)) = tokens.iter().find(|(t, _)| rest.starts_with(t)) {
            out.push_str(value);
            rest = &rest[t.len()..];
        } else {
            out.push('{');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// The pixels a watermark is drawn onto, output-encoded in the recipe's space.
#[derive(Debug)]
pub enum Canvas<'a> {
    Rgb16(&'a mut Image16),
    /// 4 bytes per pixel; alpha is left alone.
    Rgba8 {
        size: Size,
        data: &'a mut [u8],
    },
}

impl Canvas<'_> {
    fn size(&self) -> Size {
        match self {
            Self::Rgb16(img) => img.size,
            Self::Rgba8 { size, .. } => *size,
        }
    }
}

/// Draw `wm` onto `canvas` (in `space`), its text tokens filled in from `item`.
/// Text needs `text`; text that is empty once its tokens are filled draws nothing.
pub fn apply(
    canvas: Canvas<'_>,
    space: OutputSpace,
    wm: &Watermark,
    item: &ExportItem,
    text: Option<&dyn TextRasterizer>,
) -> Result<(), ExportError> {
    wm.validate().map_err(ExportError::Watermark)?;
    let size = canvas.size();
    let short = size.width.min(size.height);
    let inset = (wm.inset * short as f32).round() as u32;
    let avail = Size::new(
        size.width.saturating_sub(2 * inset).max(1),
        size.height.saturating_sub(2 * inset).max(1),
    );
    let color = Tint::new(space);
    let overlay = match &wm.mark {
        Mark::Text {
            font,
            size: em,
            color: rgb,
            shadow,
            ..
        } => {
            let line = wm.text_for(item).unwrap_or_default();
            if line.trim().is_empty() {
                return Ok(());
            }
            let rast = text.ok_or_else(|| {
                ExportError::Watermark("text watermarks need the app's text renderer".into())
            })?;
            let mask = fit_text(rast, &line, font, em * short as f32, avail)?;
            Overlay::text(mask, color.rgb(*rgb), *shadow, wm.opacity)
        }
        Mark::Logo { logo, scale } => Overlay::logo(logo, *scale, size, avail, color, wm.opacity),
    };
    let (col, row) = wm.anchor.grid();
    let place = |c: u8, extent: u32, mark: u32| -> i64 {
        let (extent, mark, inset) = (i64::from(extent), i64::from(mark), i64::from(inset));
        match c {
            0 => inset,
            1 => (extent - mark) / 2,
            _ => extent - inset - mark,
        }
    };
    let at = (
        place(col, size.width, overlay.mark.width) - i64::from(overlay.pad),
        place(row, size.height, overlay.mark.height) - i64::from(overlay.pad),
    );
    composite(canvas, &overlay, at);
    Ok(())
}

/// Largest em size asked of the rasterizer (Core Text refuses 20 000 px and up).
const MAX_TEXT_PX: f32 = 16_384.0;

/// Rasterise `text` at `px` per em (at most the frame's height), once more smaller
/// if it doesn't fit `avail`.
fn fit_text(
    rast: &dyn TextRasterizer,
    text: &str,
    font: &str,
    px: f32,
    avail: Size,
) -> Result<TextMask, ExportError> {
    let call = |px: f32| -> Result<TextMask, ExportError> {
        let m = lt_fsutil::catch_panic(|| rast.rasterize(text, font, px.max(1.0)))
            .map_err(|p| format!("text renderer panicked: {p}"))
            .and_then(|r| r)
            .map_err(ExportError::Watermark)?;
        let n = u64::from(m.width) * u64::from(m.height);
        if n == 0 || m.coverage.len() as u64 != n {
            return Err(ExportError::Watermark(format!(
                "text mask has {} bytes for {}×{}",
                m.coverage.len(),
                m.width,
                m.height
            )));
        }
        Ok(m)
    };
    let px = px.min(avail.height as f32).min(MAX_TEXT_PX);
    let mask = call(px)?;
    let k = (avail.width as f32 / mask.width as f32).min(avail.height as f32 / mask.height as f32);
    if k < 1.0 { call(px * k) } else { Ok(mask) }
}

/// Converts sRGB colours into the output space's linear light.
#[derive(Clone, Copy)]
struct Tint(Option<[[f32; 3]; 3]>);

impl Tint {
    fn new(space: OutputSpace) -> Self {
        Self(match space {
            OutputSpace::Srgb => None,
            OutputSpace::DisplayP3 => Some(crate::icc::srgb_to_p3()),
            OutputSpace::Rec2020 => Some(crate::icc::srgb_to_rec2020()),
        })
    }

    fn linear(self, c: [f32; 3]) -> [f32; 3] {
        match self.0 {
            None => c,
            Some(m) => m.map(|r| r[0] * c[0] + r[1] * c[1] + r[2] * c[2]),
        }
    }

    fn rgb(self, rgb: [u8; 3]) -> [f32; 3] {
        let lut = srgb8_lut();
        self.linear(rgb.map(|v| lut[usize::from(v)]))
    }
}

/// 8-bit sRGB-encoded → linear.
fn srgb8_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| std::array::from_fn(|v| srgb_to_linear(v as f64 / 255.0) as f32))
}

/// Shadow: offset and blur radius as a fraction of the text height, and its strength.
const SHADOW_OFFSET: f32 = 0.05;
const SHADOW_ALPHA: f32 = 0.6;

/// A watermark ready to draw: rows of premultiplied linear RGBA, made on demand.
struct Overlay {
    /// The whole drawn area (the mark plus its shadow's reach).
    size: Size,
    /// The mark's own box, placed by the anchor; it starts `pad` px into `size`.
    mark: Size,
    pad: u32,
    kind: OverlayKind,
}

enum OverlayKind {
    Text {
        coverage: Vec<u8>,
        /// Shadow alpha (0…255) over the whole overlay, if any: a byte a pixel, since
        /// the overlay can be as large as the output.
        shadow: Option<Vec<u8>>,
        color: [f32; 3],
        opacity: f32,
    },
    Logo {
        logo: Arc<Logo>,
        h: Vec<(usize, Vec<f32>)>,
        v: Vec<(usize, Vec<f32>)>,
        tint: Tint,
        opacity: f32,
    },
}

impl Overlay {
    fn text(mask: TextMask, color: [f32; 3], shadow: bool, opacity: f32) -> Self {
        let mark = Size::new(mask.width, mask.height);
        let d = if shadow {
            ((mask.height as f32 * SHADOW_OFFSET).round() as u32).max(1)
        } else {
            0
        };
        // Shadow offset d down-right, blurred by radius d: reaches d px up-left
        // and 2d down-right of the text.
        let pad = d;
        let size = Size::new(mask.width + 3 * d, mask.height + 3 * d);
        let shadow = shadow.then(|| {
            let (w, h) = (size.width as usize, size.height as usize);
            let mut a = vec![0u8; w * h];
            for y in 0..mask.height as usize {
                let src = &mask.coverage[y * mask.width as usize..][..mask.width as usize];
                let row = (y + 2 * d as usize) * w + 2 * d as usize;
                for (o, &c) in a[row..row + src.len()].iter_mut().zip(src) {
                    *o = (f32::from(c) * SHADOW_ALPHA).round() as u8;
                }
            }
            box_blur(&mut a, w, h, d as usize);
            a
        });
        Self {
            size,
            mark,
            pad,
            kind: OverlayKind::Text {
                coverage: mask.coverage,
                shadow,
                color,
                opacity,
            },
        }
    }

    fn logo(
        logo: &Arc<Logo>,
        scale: f32,
        canvas: Size,
        avail: Size,
        tint: Tint,
        opacity: f32,
    ) -> Self {
        let ls = logo.size;
        let mut w = f64::from(scale) * f64::from(canvas.width);
        let mut h = w * f64::from(ls.height) / f64::from(ls.width);
        let k = (f64::from(avail.width) / w)
            .min(f64::from(avail.height) / h)
            .min(1.0);
        w *= k;
        h *= k;
        let mark = Size::new((w.round() as u32).max(1), (h.round() as u32).max(1));
        Self {
            size: mark,
            mark,
            pad: 0,
            kind: OverlayKind::Logo {
                logo: logo.clone(),
                h: taps(ls.width, mark.width),
                v: taps(ls.height, mark.height),
                tint,
                opacity,
            },
        }
    }

    /// Row `y` into `out` (`size.width` pixels); `tmp` is scratch.
    fn row(&self, y: usize, out: &mut [[f32; 4]], tmp: &mut Vec<[f32; 4]>) {
        match &self.kind {
            OverlayKind::Text {
                coverage,
                shadow,
                color,
                opacity,
            } => {
                let w = self.size.width as usize;
                let (mw, pad) = (self.mark.width as usize, self.pad as usize);
                let text_row = y
                    .checked_sub(pad)
                    .filter(|&r| r < self.mark.height as usize)
                    .map(|r| &coverage[r * mw..(r + 1) * mw]);
                for (x, o) in out.iter_mut().enumerate() {
                    let t = text_row
                        .and_then(|r| x.checked_sub(pad).and_then(|c| r.get(c)))
                        .map_or(0.0, |&c| f32::from(c) / 255.0);
                    let s = shadow
                        .as_ref()
                        .map_or(0.0, |s| f32::from(s[y * w + x]) / 255.0);
                    // Text over its (black) shadow.
                    let a = t + s * (1.0 - t);
                    *o = [
                        color[0] * t * opacity,
                        color[1] * t * opacity,
                        color[2] * t * opacity,
                        a * opacity,
                    ];
                }
            }
            OverlayKind::Logo {
                logo,
                h,
                v,
                tint,
                opacity,
            } => {
                let lw = logo.size.width as usize;
                let (start, wv) = &v[y];
                tmp.clear();
                tmp.resize(lw, [0.0; 4]);
                for (k, &wk) in wv.iter().enumerate() {
                    let row = (start + k) * lw;
                    for (x, t) in tmp.iter_mut().enumerate() {
                        let s = logo.linear(row + x);
                        for c in 0..4 {
                            t[c] += wk * s[c];
                        }
                    }
                }
                for (o, (hs, hw)) in out.iter_mut().zip(h) {
                    let mut acc = [0f32; 4];
                    for (k, &wk) in hw.iter().enumerate() {
                        let p = tmp[hs + k];
                        for c in 0..4 {
                            acc[c] += wk * p[c];
                        }
                    }
                    // Catmull-Rom rings: keep a valid premultiplied colour.
                    let a = acc[3].clamp(0.0, 1.0);
                    let rgb = tint
                        .linear([acc[0], acc[1], acc[2]])
                        .map(|c| c.clamp(0.0, a));
                    *o = [
                        rgb[0] * opacity,
                        rgb[1] * opacity,
                        rgb[2] * opacity,
                        a * opacity,
                    ];
                }
            }
        }
    }
}

/// In-place box blur of radius `r` (horizontal, then vertical; edges clamp to 0), in
/// whole bytes: each pass rounds, which a soft shadow does not show.
fn box_blur(a: &mut [u8], w: usize, h: usize, r: usize) {
    let n = (2 * r + 1) as u32;
    let mut line: Vec<u8> = Vec::new();
    let mut pass = |len: usize, count: usize, at: &dyn Fn(usize, usize) -> usize, a: &mut [u8]| {
        for i in 0..count {
            line.clear();
            line.extend((0..len).map(|j| a[at(i, j)]));
            let mut sum: u32 = line.iter().take(r).map(|&v| u32::from(v)).sum();
            for j in 0..len {
                if j + r < len {
                    sum += u32::from(line[j + r]);
                }
                a[at(i, j)] = ((sum + n / 2) / n) as u8;
                if j >= r {
                    sum -= u32::from(line[j - r]);
                }
            }
        }
    };
    pass(w, h, &|y, x| y * w + x, a);
    pass(h, w, &|x, y| y * w + x, a);
}

/// A premultiplied linear overlay pixel as its encoded (0…65 535) straight colour
/// and alpha, or `None` when it is transparent.
#[inline]
fn encoded(enc: &[u16], p: &[f32; 4]) -> Option<([f32; 3], f32)> {
    let a = p[3].min(1.0);
    if a <= 0.0 {
        return None;
    }
    Some(([p[0], p[1], p[2]].map(|c| f32::from(encode(enc, c / a))), a))
}

/// Blend `ov` over the canvas with its top-left corner at `at` (clipped).
fn composite(canvas: Canvas<'_>, ov: &Overlay, at: (i64, i64)) {
    let size = canvas.size();
    let (cw, ch) = (i64::from(size.width), i64::from(size.height));
    let (ow, oh) = (i64::from(ov.size.width), i64::from(ov.size.height));
    let (x0, x1) = (at.0.max(0), (at.0 + ow).min(cw));
    let (y0, y1) = (at.1.max(0), (at.1 + oh).min(ch));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let skip = (x0 - at.0) as usize;
    let span = (x1 - x0) as usize;
    let enc = encode_lut();
    // Overlay row of each canvas row in the band.
    let rows: Vec<usize> = (y0..y1).map(|y| (y - at.1) as usize).collect();
    let init = || (vec![[0f32; 4]; ov.size.width as usize], Vec::new());
    match canvas {
        Canvas::Rgb16(img) => {
            let stride = size.width as usize * 3;
            let band = &mut img.data[y0 as usize * stride..y1 as usize * stride];
            band.par_chunks_mut(stride)
                .zip(&rows)
                .for_each_init(init, |(px, tmp), (row, &oy)| {
                    ov.row(oy, px, tmp);
                    let dst = &mut row[x0 as usize * 3..(x0 as usize + span) * 3];
                    for (d, p) in dst.as_chunks_mut::<3>().0.iter_mut().zip(&px[skip..]) {
                        let Some((rgb, a)) = encoded(enc, p) else {
                            continue;
                        };
                        for c in 0..3 {
                            let v = rgb[c] * a + f32::from(d[c]) * (1.0 - a);
                            d[c] = v.round().clamp(0.0, 65_535.0) as u16;
                        }
                    }
                });
        }
        Canvas::Rgba8 { data, .. } => {
            let stride = size.width as usize * 4;
            let band = &mut data[y0 as usize * stride..y1 as usize * stride];
            band.par_chunks_mut(stride)
                .zip(&rows)
                .for_each_init(init, |(px, tmp), (row, &oy)| {
                    ov.row(oy, px, tmp);
                    let dst = &mut row[x0 as usize * 4..(x0 as usize + span) * 4];
                    for (d, p) in dst.as_chunks_mut::<4>().0.iter_mut().zip(&px[skip..]) {
                        let Some((rgb, a)) = encoded(enc, p) else {
                            continue;
                        };
                        for c in 0..3 {
                            let v = rgb[c] / 257.0 * a + f32::from(d[c]) * (1.0 - a);
                            d[c] = v.round().clamp(0.0, 255.0) as u8;
                        }
                    }
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::exif::ExifMeta;
    use crate::process::linear_to_srgb;

    /// A test double for Core Text: each character is a solid 0.5 em × 1 em cell
    /// (the line is `0.5 × chars` em wide, 1 em tall); records the sizes asked for.
    #[derive(Default)]
    struct Blocks(Mutex<Vec<f32>>);

    impl TextRasterizer for Blocks {
        fn rasterize(&self, text: &str, _font: &str, px: f32) -> Result<TextMask, String> {
            self.0.lock().unwrap().push(px);
            let h = px.round() as u32;
            let w = (px * 0.5 * text.chars().count() as f32).round() as u32;
            Ok(TextMask {
                width: w,
                height: h,
                coverage: vec![255; (w * h) as usize],
            })
        }
    }

    fn black16(w: u32, h: u32) -> Image16 {
        Image16::new(Size::new(w, h), vec![0; (w * h * 3) as usize]).unwrap()
    }

    fn item() -> ExportItem {
        ExportItem {
            id: 1,
            name: "DSC0042".into(),
            seq: 1,
            meta: ExifMeta {
                copyright: Some("© Alexa".into()),
                artist: Some("Alexa R".into()),
                capture_time: chrono::NaiveDate::from_ymd_opt(2024, 5, 6)
                    .unwrap()
                    .and_hms_opt(7, 8, 9),
                ..Default::default()
            },
        }
    }

    fn text_wm(text: &str, size: f32, anchor: Anchor) -> Watermark {
        Watermark {
            mark: Mark::Text {
                text: text.into(),
                font: String::new(),
                size,
                color: [255, 255, 255],
                shadow: false,
            },
            anchor,
            inset: 0.05,
            opacity: 1.0,
        }
    }

    /// Bounding box (x0, y0, x1, y1) of the pixels that aren't black.
    fn ink(img: &Image16) -> Option<(u32, u32, u32, u32)> {
        let w = img.size.width;
        let mut bb: Option<(u32, u32, u32, u32)> = None;
        for (i, p) in img.data.chunks(3).enumerate() {
            if p.iter().any(|&v| v != 0) {
                let (x, y) = (i as u32 % w, i as u32 / w);
                bb = Some(bb.map_or((x, y, x + 1, y + 1), |(a, b, c, d)| {
                    (a.min(x), b.min(y), c.max(x + 1), d.max(y + 1))
                }));
            }
        }
        bb
    }

    fn draw(img: &mut Image16, wm: &Watermark, space: OutputSpace) {
        apply(
            Canvas::Rgb16(img),
            space,
            wm,
            &item(),
            Some(&Blocks::default()),
        )
        .unwrap();
    }

    #[test]
    fn nine_anchors_place_the_mark_inside_the_margin() {
        // 200×100: short edge 100 → 10 px em (a 4-character mark is 20×10),
        // 5 px margin.
        let want = [
            (Anchor::TopLeft, (5, 5)),
            (Anchor::Top, (90, 5)),
            (Anchor::TopRight, (175, 5)),
            (Anchor::Left, (5, 45)),
            (Anchor::Center, (90, 45)),
            (Anchor::Right, (175, 45)),
            (Anchor::BottomLeft, (5, 85)),
            (Anchor::Bottom, (90, 85)),
            (Anchor::BottomRight, (175, 85)),
        ];
        for (anchor, (x, y)) in want {
            let mut img = black16(200, 100);
            draw(&mut img, &text_wm("ABCD", 0.1, anchor), OutputSpace::Srgb);
            assert_eq!(ink(&img), Some((x, y, x + 20, y + 10)), "{anchor:?}");
            // Opaque white over black is white.
            let i = ((y * 200 + x) * 3) as usize;
            assert_eq!(&img.data[i..i + 3], &[65_535; 3], "{anchor:?}");
        }
        assert_eq!(Anchor::default(), Anchor::BottomRight);
    }

    #[test]
    fn opacity_blends_in_encoded_values_like_lightroom() {
        let mut wm = text_wm("AB", 0.2, Anchor::Center);
        wm.opacity = 0.5;
        // 16-bit: half-transparent white over black is half the encoded range
        // (not 0.5 linear, which would be ~0.735 encoded).
        let mut img = black16(100, 100);
        draw(&mut img, &wm, OutputSpace::Srgb);
        let want = 32_768;
        let c = ((50 * 100 + 50) * 3) as usize;
        assert!(
            (i32::from(img.data[c]) - want).abs() <= 2,
            "{}",
            img.data[c]
        );
        // 8-bit: 128/255 (a linear-light blend would give 188); alpha untouched.
        let mut data = vec![0u8; 100 * 100 * 4];
        let canvas = Canvas::Rgba8 {
            size: Size::new(100, 100),
            data: &mut data,
        };
        apply(
            canvas,
            OutputSpace::Srgb,
            &wm,
            &item(),
            Some(&Blocks::default()),
        )
        .unwrap();
        let c = (50 * 100 + 50) * 4;
        assert_eq!(&data[c..c + 4], &[128, 128, 128, 0]);
        // Opacity 0 changes nothing.
        let mut clear = wm.clone();
        clear.opacity = 0.0;
        let mut img = black16(100, 100);
        draw(&mut img, &clear, OutputSpace::Srgb);
        assert_eq!(ink(&img), None);
        // Over a mid grey (encoded 100/255) a 30% black mark is 70% of it, as in Photoshop.
        wm.opacity = 0.3;
        if let Mark::Text { color, .. } = &mut wm.mark {
            *color = [0; 3];
        }
        let mut grey = vec![100u8; 100 * 100 * 4];
        let canvas = Canvas::Rgba8 {
            size: Size::new(100, 100),
            data: &mut grey,
        };
        apply(
            canvas,
            OutputSpace::Srgb,
            &wm,
            &item(),
            Some(&Blocks::default()),
        )
        .unwrap();
        assert_eq!(grey[c], 70);
        assert_eq!(grey[0], 100, "outside the mark");
    }

    #[test]
    fn the_mark_keeps_its_relative_size_at_every_export_size() {
        let wm = text_wm("Alexa", 0.04, Anchor::BottomRight);
        let rel = |w: u32, h: u32| {
            let mut img = black16(w, h);
            draw(&mut img, &wm, OutputSpace::Srgb);
            let (x0, y0, x1, y1) = ink(&img).unwrap();
            [x0, y0, x1, y1].map(|v| f64::from(v) / f64::from(w))
        };
        let small = rel(600, 400);
        for (w, h) in [(1200, 800), (3000, 2000), (6000, 4000)] {
            let big = rel(w, h);
            for (a, b) in small.iter().zip(big) {
                assert!((a - b).abs() < 1.5 / 600.0, "{small:?} vs {big:?} at {w}");
            }
        }
        // Portrait: sized by the short edge, so the same pixel size as landscape.
        let mut img = black16(400, 600);
        draw(&mut img, &wm, OutputSpace::Srgb);
        let (x0, y0, x1, y1) = ink(&img).unwrap();
        assert_eq!((x1 - x0, y1 - y0), (40, 16));
        assert_eq!((x1, y1), (400 - 20, 600 - 20));
    }

    #[test]
    fn text_too_wide_is_rasterised_again_to_fit() {
        let rast = Blocks::default();
        let mut img = black16(100, 100);
        let wm = text_wm("a long copyright line", 0.2, Anchor::BottomLeft);
        apply(
            Canvas::Rgb16(&mut img),
            OutputSpace::Srgb,
            &wm,
            &item(),
            Some(&rast),
        )
        .unwrap();
        let calls = rast.0.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!((calls[0] - 20.0).abs() < 1e-4);
        assert!(calls[1] < calls[0]);
        let (x0, _, x1, y1) = ink(&img).unwrap();
        assert_eq!(x0, 5);
        assert!((93..=95).contains(&x1), "{x1}");
        assert_eq!(y1, 95);
    }

    #[test]
    fn text_colour_is_converted_into_p3() {
        let mut wm = text_wm("AB", 0.2, Anchor::Center);
        if let Mark::Text { color, .. } = &mut wm.mark {
            *color = [255, 0, 0];
        }
        let mut srgb = black16(100, 100);
        draw(&mut srgb, &wm, OutputSpace::Srgb);
        let mut p3 = black16(100, 100);
        draw(&mut p3, &wm, OutputSpace::DisplayP3);
        let c = ((50 * 100 + 50) * 3) as usize;
        assert_eq!(&srgb.data[c..c + 3], &[65_535, 0, 0]);
        let m = crate::icc::srgb_to_p3();
        let want = [m[0][0], m[1][0], m[2][0]].map(|v| linear_to_srgb(f64::from(v)) * 65_535.0);
        for (got, want) in p3.data[c..c + 3].iter().zip(want) {
            assert!((f64::from(*got) - want).abs() <= 2.0, "{got} vs {want}");
        }
        assert!(p3.data[c] < 61_000, "sRGB red is not P3's full red");
    }

    #[test]
    fn shadow_darkens_down_and_right_of_the_text() {
        let white = Image16::new(Size::new(200, 200), vec![65_535; 200 * 200 * 3]).unwrap();
        let mut wm = text_wm("AB", 0.2, Anchor::Center);
        if let Mark::Text { color, shadow, .. } = &mut wm.mark {
            *color = [255, 255, 255];
            *shadow = true;
        }
        let mut img = white.clone();
        draw(&mut img, &wm, OutputSpace::Srgb);
        // Text 40×40 at (80, 80); shadow offset 2 px, blurred 2 px.
        let px = |x: usize, y: usize| img.data[(y * 200 + x) * 3];
        assert_eq!(px(100, 100), 65_535, "text is white over its shadow");
        assert!(
            px(121, 121) < 60_000,
            "shadow below-right: {}",
            px(121, 121)
        );
        assert!(px(121, 100) < 60_000, "shadow right: {}", px(121, 100));
        assert_eq!(px(78, 78), 65_535, "nothing up-left beyond the blur");
        assert_eq!(px(150, 150), 65_535, "nothing far away");
        // No shadow: only the text box changes (and white on white: nothing).
        if let Mark::Text { shadow, .. } = &mut wm.mark {
            *shadow = false;
        }
        let mut plain = white.clone();
        draw(&mut plain, &wm, OutputSpace::Srgb);
        assert_eq!(plain, white);
    }

    #[test]
    fn marks_partly_outside_are_clipped() {
        let mut wm = text_wm("ABCDEFGH", 0.5, Anchor::Center);
        wm.inset = 0.0;
        // 10×4: 2 px em → 8 chars = 8 px wide, centred.
        let mut img = black16(10, 4);
        draw(&mut img, &wm, OutputSpace::Srgb);
        assert_eq!(ink(&img), Some((1, 1, 9, 3)));
        // Overlay entirely off the canvas draws nothing (and doesn't panic).
        let ov = Overlay::text(
            TextMask {
                width: 2,
                height: 2,
                coverage: vec![255; 4],
            },
            [1.0; 3],
            false,
            1.0,
        );
        let mut img = black16(4, 4);
        composite(Canvas::Rgb16(&mut img), &ov, (-5, 0));
        composite(Canvas::Rgb16(&mut img), &ov, (0, 9));
        assert_eq!(ink(&img), None);
        composite(Canvas::Rgb16(&mut img), &ov, (-1, 3));
        assert_eq!(ink(&img), Some((0, 3, 1, 4)));
    }

    #[test]
    fn tokens_are_filled_in() {
        let it = item();
        assert_eq!(
            expand_tokens("{copyright} {year} · {filename} by {artist}", &it),
            "© Alexa 2024 · DSC0042 by Alexa R"
        );
        let mut bare = it;
        bare.meta = ExifMeta::default();
        let year = chrono::Local::now().year().to_string();
        assert_eq!(
            expand_tokens("{copyright}|{year}|{x}", &bare),
            format!("|{year}|{{x}}")
        );
    }

    #[test]
    fn text_for_and_skips_report_blank_lines() {
        let mut it = item();
        let wm = text_wm("{copyright}\n{year}", 0.1, Anchor::Center);
        assert_eq!(wm.text_for(&it).as_deref(), Some("© Alexa 2024"));
        assert!(!wm.skips(&it));
        it.meta.copyright = None;
        let only = text_wm("{copyright}", 0.1, Anchor::Center);
        assert_eq!(only.text_for(&it).as_deref(), Some(""));
        assert!(only.skips(&it));
        let logo = Watermark {
            mark: Mark::Logo {
                logo: Arc::new(
                    Logo::decode_png(&png_bytes(1, 1, png::ColorType::Rgba, &[0; 4])).unwrap(),
                ),
                scale: 0.5,
            },
            ..only
        };
        assert_eq!(logo.text_for(&it), None);
        assert!(!logo.skips(&it));
    }

    #[test]
    fn first_rasterisation_is_capped() {
        // Records the size asked for; a 1×1 mask always fits.
        struct Dot(Mutex<Vec<f32>>);
        impl TextRasterizer for Dot {
            fn rasterize(&self, _: &str, _: &str, px: f32) -> Result<TextMask, String> {
                self.0.lock().unwrap().push(px);
                Ok(TextMask {
                    width: 1,
                    height: 1,
                    coverage: vec![255],
                })
            }
        }
        let dot = Dot(Mutex::default());
        fit_text(&dot, "x", "", 30_000.0, Size::new(100_000, 100_000)).unwrap();
        fit_text(&dot, "x", "", 500.0, Size::new(1000, 120)).unwrap();
        assert_eq!(*dot.0.lock().unwrap(), [MAX_TEXT_PX, 120.0]);
    }

    #[test]
    fn empty_text_after_tokens_draws_nothing_and_needs_no_rasterizer() {
        let mut it = item();
        it.meta.copyright = None;
        let wm = text_wm("{copyright}", 0.1, Anchor::Center);
        let mut img = black16(20, 20);
        apply(Canvas::Rgb16(&mut img), OutputSpace::Srgb, &wm, &it, None).unwrap();
        assert_eq!(ink(&img), None);
    }

    #[test]
    fn text_errors_are_reported() {
        struct Bad(u8);
        impl TextRasterizer for Bad {
            fn rasterize(&self, _: &str, _: &str, _: f32) -> Result<TextMask, String> {
                match self.0 {
                    0 => Err("no such font".into()),
                    1 => Ok(TextMask {
                        width: 3,
                        height: 3,
                        coverage: vec![0; 8],
                    }),
                    2 => Ok(TextMask {
                        width: 0,
                        height: 3,
                        coverage: vec![],
                    }),
                    _ => panic!("boom"),
                }
            }
        }
        let wm = text_wm("x", 0.1, Anchor::Center);
        let mut img = black16(20, 20);
        let e = apply(
            Canvas::Rgb16(&mut img),
            OutputSpace::Srgb,
            &wm,
            &item(),
            None,
        )
        .unwrap_err();
        assert!(e.to_string().contains("text renderer"), "{e}");

        for (k, msg) in [
            (0, "no such font"),
            (1, "8 bytes for 3×3"),
            (2, "0 bytes for 0×3"),
            (3, "panicked"),
        ] {
            let e = apply(
                Canvas::Rgb16(&mut img),
                OutputSpace::Srgb,
                &wm,
                &item(),
                Some(&Bad(k)),
            )
            .unwrap_err();
            assert!(matches!(e, ExportError::Watermark(_)), "{e}");
            assert!(e.to_string().contains(msg), "{e}");
        }
        assert_eq!(ink(&img), None);
    }

    #[test]
    fn validation() {
        let ok = text_wm("x", 0.1, Anchor::Center);
        assert!(ok.validate().is_ok());
        let bad = |f: &dyn Fn(&mut Watermark), msg: &str| {
            let mut w = ok.clone();
            f(&mut w);
            let e = w.validate().unwrap_err();
            assert!(e.contains(msg), "{e}");
            let mut img = black16(4, 4);
            assert!(
                apply(
                    Canvas::Rgb16(&mut img),
                    OutputSpace::Srgb,
                    &w,
                    &item(),
                    None
                )
                .is_err()
            );
        };
        bad(&|w| w.opacity = 1.5, "opacity");
        bad(&|w| w.opacity = f32::NAN, "opacity");
        bad(&|w| w.inset = 0.3, "margin");
        bad(&|w| w.inset = -0.1, "margin");
        bad(
            &|w| w.mark = text_wm(" ", 0.1, Anchor::Center).mark,
            "empty",
        );
        bad(&|w| w.mark = text_wm("x", 0.0, Anchor::Center).mark, "size");
        bad(&|w| w.mark = text_wm("x", 0.6, Anchor::Center).mark, "size");
        let logo =
            Arc::new(Logo::decode_png(&png_bytes(1, 1, png::ColorType::Rgba, &[0; 4])).unwrap());
        for scale in [0.0, 1.5, f32::INFINITY] {
            let l = Mark::Logo {
                logo: logo.clone(),
                scale,
            };
            bad(&move |w| w.mark = l.clone(), "scale");
        }
    }

    fn png_bytes(w: u32, h: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(data).unwrap();
        wr.finish().unwrap();
        out
    }

    fn logo_wm(logo: Logo, scale: f32, anchor: Anchor) -> Watermark {
        Watermark {
            mark: Mark::Logo {
                logo: Arc::new(logo),
                scale,
            },
            anchor,
            inset: 0.0,
            opacity: 1.0,
        }
    }

    #[test]
    fn logo_alpha_is_kept() {
        // 4×2 logo: left half transparent red, right half opaque green, and one
        // half-transparent white pixel.
        let mut data = Vec::new();
        for y in 0..2 {
            for x in 0..4 {
                data.extend(match (x, y) {
                    (0 | 1, _) => [255, 0, 0, 0],
                    (3, 1) => [255, 255, 255, 128],
                    _ => [0, 255, 0, 255],
                });
            }
        }
        let logo = Logo::decode_png(&png_bytes(4, 2, png::ColorType::Rgba, &data)).unwrap();
        assert_eq!(logo.size(), Size::new(4, 2));
        assert_eq!(format!("{logo:?}"), "Logo(4×2)");
        // Native size (scale × 40 px = 4 px): no resampling.
        let mut img = black16(40, 20);
        draw(
            &mut img,
            &logo_wm(logo, 0.1, Anchor::TopLeft),
            OutputSpace::Srgb,
        );
        let px = |x: usize, y: usize| &img.data[(y * 40 + x) * 3..][..3];
        assert_eq!(px(0, 0), &[0, 0, 0], "transparent: canvas shows through");
        assert_eq!(px(1, 1), &[0, 0, 0]);
        assert_eq!(px(2, 0), &[0, 65_535, 0], "opaque: the logo");
        // Half-transparent white over black: half the encoded range.
        let half = 128 * 65_535 / 255;
        assert!(
            px(3, 1).iter().all(|&v| (i32::from(v) - half).abs() <= 2),
            "{:?}",
            px(3, 1)
        );
        assert_eq!(ink(&img), Some((2, 0, 4, 2)));
    }

    #[test]
    fn logo_scales_with_the_output_and_fits_the_margin() {
        // An opaque 8×4 white logo at a quarter of the width.
        let logo =
            || Logo::decode_png(&png_bytes(8, 4, png::ColorType::Rgb, &[255; 8 * 4 * 3])).unwrap();
        for (w, h) in [(80, 60), (400, 300), (1000, 750)] {
            let mut wm = logo_wm(logo(), 0.25, Anchor::BottomRight);
            wm.inset = 0.1;
            let mut img = black16(w, h);
            draw(&mut img, &wm, OutputSpace::Srgb);
            let m = (h as f32 * 0.1).round() as u32;
            assert_eq!(
                ink(&img),
                Some((w - m - w / 4, h - m - w / 8, w - m, h - m)),
                "{w}×{h}"
            );
            // Upscaled flat white stays exactly white (no ringing inside).
            let c = (((h - m - w / 16) * w + (w - m - w / 8)) * 3) as usize;
            assert_eq!(img.data[c], 65_535);
        }
        // Too big for the frame: fitted inside the margins, aspect kept.
        let mut wm = logo_wm(logo(), 1.0, Anchor::Center);
        wm.inset = 0.1;
        let mut img = black16(100, 20);
        draw(&mut img, &wm, OutputSpace::Srgb);
        assert_eq!(ink(&img), Some((34, 2, 66, 18)));
    }

    #[test]
    fn logo_downscale_keeps_edges_premultiplied() {
        // 64×64 half transparent black, half opaque white, shrunk to 8×8 on red:
        // no dark fringe from the transparent pixels' colour.
        let mut data = Vec::new();
        for _ in 0..64 {
            for x in 0..64 {
                data.extend(if x < 32 {
                    [0, 0, 0, 0]
                } else {
                    [255, 255, 255, 255]
                });
            }
        }
        let logo = Logo::decode_png(&png_bytes(64, 64, png::ColorType::Rgba, &data)).unwrap();
        let red = Image16::new(
            Size::new(80, 80),
            (0..80 * 80).flat_map(|_| [65_535, 0, 0]).collect(),
        )
        .unwrap();
        let mut img = red.clone();
        draw(
            &mut img,
            &logo_wm(logo, 0.1, Anchor::TopLeft),
            OutputSpace::Srgb,
        );
        for x in 0..8 {
            let p = &img.data[(4 * 80 + x) * 3..][..3];
            assert_eq!(p[0], 65_535, "red channel never darkened at x={x}: {p:?}");
        }
    }

    #[test]
    fn logo_formats_decode() {
        let grey =
            Logo::decode_png(&png_bytes(2, 1, png::ColorType::Grayscale, &[0, 255])).unwrap();
        assert_eq!(grey.px, vec![[0, 0, 0, 255], [255, 255, 255, 255]]);
        let near = |a: [f32; 4], b: [f32; 4]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
        assert!(near(grey.linear(0), [0.0, 0.0, 0.0, 1.0]) && near(grey.linear(1), [1.0; 4]));
        assert_eq!(grey.bytes(), 8);
        let ga =
            Logo::decode_png(&png_bytes(1, 1, png::ColorType::GrayscaleAlpha, &[255, 51])).unwrap();
        assert_eq!(ga.px, vec![[255, 255, 255, 51]]);
        let l = ga.linear(0);
        assert!(
            (l[3] - 0.2).abs() < 1e-6 && (l[0] - 0.2).abs() < 1e-6,
            "premultiplied: {l:?}"
        );
        // Palette.
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 2, 1);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_palette(vec![255, 0, 0, 0, 0, 255]);
        enc.set_trns(vec![255, 0]);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&[0, 1]).unwrap();
        wr.finish().unwrap();
        let pal = Logo::decode_png(&out).unwrap();
        assert_eq!(pal.px, vec![[255, 0, 0, 255], [0, 0, 255, 0]]);
        assert!(pal.linear(1).iter().all(|v| v.abs() < 1e-9), "transparent");
        // 16-bit is reduced to 8.
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 1, 1);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Sixteen);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&[255, 255, 0, 0, 0, 0]).unwrap();
        wr.finish().unwrap();
        assert_eq!(Logo::decode_png(&out).unwrap().px, vec![[255, 0, 0, 255]]);
    }

    #[test]
    fn bad_logos_are_refused() {
        assert!(Logo::decode_png(b"not a png").is_err());
        let big = png_bytes(
            Logo::MAX_EDGE + 1,
            1,
            png::ColorType::Grayscale,
            &vec![0; Logo::MAX_EDGE as usize + 1],
        );
        assert!(Logo::decode_png(&big).unwrap_err().contains("larger than"));
        let d = tempfile::tempdir().unwrap();
        let missing = Logo::load(&d.path().join("logo.png")).unwrap_err();
        assert!(matches!(missing, ExportError::Io { .. }), "{missing}");
        let p = d.path().join("bad.png");
        std::fs::write(&p, b"junk").unwrap();
        let e = Logo::load(&p).unwrap_err();
        assert!(e.to_string().contains("bad.png"), "{e}");
        std::fs::write(&p, png_bytes(3, 2, png::ColorType::Rgb, &[9; 18])).unwrap();
        assert_eq!(Logo::load(&p).unwrap().size(), Size::new(3, 2));
    }

    #[test]
    fn box_blur_spreads_and_keeps_the_sum() {
        let (w, h) = (9, 9);
        let mut a = vec![0u8; w * h];
        a[4 * w + 4] = 180;
        box_blur(&mut a, w, h, 1);
        // 180 / 9 = 20 in each cell of the 3×3 block around the dot, nothing outside.
        for y in 0..h {
            for x in 0..w {
                let near = x.abs_diff(4) <= 1 && y.abs_diff(4) <= 1;
                assert_eq!(a[y * w + x], if near { 20 } else { 0 }, "({x},{y})");
            }
        }
        // Edges clamp to zero and each pass rounds to nearest: 255 / 3 = 85 across, then
        // 85 / 3 = 28 down (a one-row image still divides by three).
        let mut edge = vec![255u8, 0, 0, 0];
        box_blur(&mut edge, 4, 1, 1);
        assert_eq!(edge, [28, 28, 0, 0]);
    }

    #[test]
    fn tokens_are_expanded_in_one_pass() {
        // A value that looks like a token is not expanded again.
        let mut it = item();
        it.name = "a{year}b".into();
        it.meta.artist = Some("{filename}".into());
        it.meta.copyright = Some("{{artist}".into());
        assert_eq!(
            expand_tokens("{filename} / {artist} / {copyright} / {year}", &it),
            "a{year}b / {filename} / {{artist} / 2024"
        );
        // Unknown tokens, stray braces and non-ASCII text pass through.
        assert_eq!(expand_tokens("{ {x} } é{", &it), "{ {x} } é{");
        assert_eq!(expand_tokens("", &it), "");
    }

    /// A logo file is read up to a limit: an endless file is refused, not loaded.
    #[cfg(unix)]
    #[test]
    fn an_oversize_logo_file_is_refused_unread() {
        let e = Logo::load(Path::new("/dev/zero")).unwrap_err();
        assert!(
            matches!(e, ExportError::Watermark(_)) && e.to_string().contains("larger than"),
            "{e}"
        );
    }

    /// The linearised pixels are made once and match the per-tap computation.
    #[test]
    fn logo_pixels_are_linearised_once() {
        let bits = |p: [f32; 4]| p.map(f32::to_bits);
        let data: Vec<u8> = (0..4 * 3 * 4).map(|i| (i * 37 + 11) as u8).collect();
        let logo = Logo::decode_png(&png_bytes(4, 3, png::ColorType::Rgba, &data)).unwrap();
        assert!(logo.lin.get().is_none(), "not made until used");
        for i in 0..12 {
            assert_eq!(bits(logo.linear(i)), bits(logo.linear_uncached(i)));
        }
        let lin = logo
            .lin
            .get()
            .unwrap()
            .as_ref()
            .expect("a small logo is cached");
        assert_eq!(lin.len(), 12);
        // A logo past the cache limit falls back to computing each tap.
        let over = Logo {
            size: Size::new(2, 1),
            px: vec![[1, 2, 3, 4]; Logo::LINEAR_CACHE_PIXELS + 1],
            lin: OnceLock::new(),
        };
        assert_eq!(bits(over.linear(5)), bits(over.linear_uncached(5)));
        assert!(over.lin.get().unwrap().is_none(), "too large to hold twice");
    }
}
