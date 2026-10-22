//! The post-process seam: steps drawn on each output at its final size, after resizing
//! and output sharpening, on the working copy (never a full render shared between
//! recipes). Plug-in steps (ds-ffi adapts WebAssembly ones) run in the recipe's order,
//! then the watermark, last, as its native provider.
//!
//! A step may enlarge the canvas (a border around the photo): `layout` says the new size
//! and where the current canvas goes; the margin starts black (opaque). Every call is
//! isolated: a panic in a step is that file's error, and any error fails that file
//! (nothing is written).

use std::borrow::Cow;
use std::sync::Arc;

use ds_core::Size;

use crate::ExportError;
use crate::batch::ExportItem;
use crate::image::Image16;
use crate::recipe::OutputSpace;
use crate::watermark::{self, Canvas, TextRasterizer, Watermark};

/// Where the photo sits in a step's canvas, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhotoRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// How far steps may grow the canvas, in all: twice the image's area (a border, a
/// letterbox), so a plug-in can't make the host allocate an arbitrary canvas.
pub const MAX_CANVAS_GROWTH: u64 = 2;

/// A step's canvas: its size, and where the current canvas goes in it.
pub type CanvasLayout = (Size, (u32, u32));

/// One post-process step.
pub trait PostProcess: std::fmt::Debug {
    /// Shown in messages ("Film Border").
    fn name(&self) -> &str;

    /// The canvas this step draws on, for a current canvas of `size` in the export's
    /// output `space`: its size and where the current canvas goes in it. The default
    /// keeps the canvas.
    fn layout(
        &self,
        size: Size,
        _space: OutputSpace,
        _item: &ExportItem,
    ) -> Result<CanvasLayout, ExportError> {
        Ok((size, (0, 0)))
    }

    /// Draw on `canvas` in place; `photo` is where the photo is in it. `cancelled` turns
    /// true when the export is cancelled (long steps should stop).
    fn apply(
        &self,
        canvas: Canvas<'_>,
        photo: PhotoRect,
        space: OutputSpace,
        item: &ExportItem,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), ExportError>;
}

impl<T: PostProcess + ?Sized> PostProcess for Arc<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn layout(
        &self,
        size: Size,
        space: OutputSpace,
        item: &ExportItem,
    ) -> Result<CanvasLayout, ExportError> {
        (**self).layout(size, space, item)
    }

    fn apply(
        &self,
        canvas: Canvas<'_>,
        photo: PhotoRect,
        space: OutputSpace,
        item: &ExportItem,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), ExportError> {
        (**self).apply(canvas, photo, space, item, cancelled)
    }
}

/// The recipe's watermark as a step, with the app's text rasterizer.
#[derive(Clone, Copy)]
pub struct WatermarkStep<'a> {
    pub watermark: &'a Watermark,
    pub text: Option<&'a dyn TextRasterizer>,
}

impl std::fmt::Debug for WatermarkStep<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatermarkStep")
            .field("watermark", self.watermark)
            .field("text", &self.text.is_some())
            .finish()
    }
}

impl PostProcess for WatermarkStep<'_> {
    fn name(&self) -> &'static str {
        "Watermark"
    }

    fn apply(
        &self,
        canvas: Canvas<'_>,
        _photo: PhotoRect,
        space: OutputSpace,
        item: &ExportItem,
        _cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), ExportError> {
        watermark::apply(canvas, space, self.watermark, item, self.text)
    }
}

/// A recipe's plug-in steps (`ExportRecipe::post_process`).
pub type PostProcessSteps = Vec<Arc<dyn PostProcess + Send + Sync>>;

/// `f` with a panic turned into this step's error.
fn isolated<T>(
    step: &dyn PostProcess,
    f: impl FnOnce() -> Result<T, ExportError>,
) -> Result<T, ExportError> {
    ds_fsutil::catch_panic(f)
        .map_err(|p| ExportError::PostProcess(format!("{}: crashed ({p})", step.name())))
        .and_then(|r| r)
}

/// The new canvas for `step`, checked: the old one must fit at the offset.
fn layout(
    step: &dyn PostProcess,
    size: Size,
    space: OutputSpace,
    item: &ExportItem,
    max_pixels: u64,
) -> Result<Option<CanvasLayout>, ExportError> {
    let (new, (x, y)) = isolated(step, || step.layout(size, space, item))?;
    if new == size && (x, y) == (0, 0) {
        return Ok(None);
    }
    let fits = x.checked_add(size.width).is_some_and(|r| r <= new.width)
        && y.checked_add(size.height).is_some_and(|b| b <= new.height);
    if !fits {
        return Err(ExportError::PostProcess(format!(
            "{}: its canvas doesn't hold the photo",
            step.name()
        )));
    }
    if new.pixels() > max_pixels {
        return Err(ExportError::PostProcess(format!(
            "{}: its canvas {}×{} is more than twice the photo's area",
            step.name(),
            new.width,
            new.height
        )));
    }
    Ok(Some((new, (x, y))))
}

/// Run `steps` on a 16-bit working copy, enlarging it when a step asks.
pub fn run_rgb16(
    steps: &[Box<dyn PostProcess + '_>],
    img: &mut Cow<'_, Image16>,
    space: OutputSpace,
    item: &ExportItem,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(), ExportError> {
    let mut photo = PhotoRect {
        x: 0,
        y: 0,
        width: img.size.width,
        height: img.size.height,
    };
    let max = img.size.pixels().saturating_mul(MAX_CANVAS_GROWTH);
    for step in steps {
        if let Some((new, (ox, oy))) = layout(step.as_ref(), img.size, space, item, max)? {
            *img = Cow::Owned(Image16 {
                size: new,
                data: enlarge(img.size, &img.data, 3, new, (ox, oy), 0),
            });
            photo.x += ox;
            photo.y += oy;
        }
        let img = img.to_mut();
        isolated(step.as_ref(), || {
            step.apply(Canvas::Rgb16(img), photo, space, item, cancelled)
        })?;
    }
    Ok(())
}

/// Run `steps` on an 8-bit RGBA working copy, enlarging it when a step asks.
pub fn run_rgba8(
    steps: &[Box<dyn PostProcess + '_>],
    size: &mut Size,
    data: &mut Vec<u8>,
    space: OutputSpace,
    item: &ExportItem,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(), ExportError> {
    let mut photo = PhotoRect {
        x: 0,
        y: 0,
        width: size.width,
        height: size.height,
    };
    let max = size.pixels().saturating_mul(MAX_CANVAS_GROWTH);
    for step in steps {
        if let Some((new, (ox, oy))) = layout(step.as_ref(), *size, space, item, max)? {
            *data = enlarge(*size, data, 4, new, (ox, oy), 255);
            *size = new;
            photo.x += ox;
            photo.y += oy;
        }
        let canvas = Canvas::Rgba8 { size: *size, data };
        isolated(step.as_ref(), || {
            step.apply(canvas, photo, space, item, cancelled)
        })?;
    }
    Ok(())
}

/// A black `new` canvas (alpha `alpha` in a 4-channel one) with `src` copied at `at`.
fn enlarge<T: Copy + Default + From<u8>>(
    size: Size,
    src: &[T],
    channels: usize,
    new: Size,
    at: (u32, u32),
    alpha: u8,
) -> Vec<T> {
    let (w, nw) = (
        size.width as usize * channels,
        new.width as usize * channels,
    );
    let mut data = vec![T::default(); nw * new.height as usize];
    if channels == 4 {
        for px in data.as_chunks_mut::<4>().0 {
            px[3] = T::from(alpha);
        }
    }
    for (r, row) in src.chunks_exact(w).enumerate() {
        let start = (at.1 as usize + r) * nw + at.0 as usize * channels;
        data[start..start + w].copy_from_slice(row);
    }
    data
}
