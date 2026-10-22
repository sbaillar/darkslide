//! The post-process seam: plug-in steps run in order, then the watermark, on each
//! output's own working copy, on both the 8-bit and the 16-bit paths; a step may
//! enlarge the canvas, and a panic is that file's error.

use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use ds_core::Size;
use ds_export::watermark::{Anchor, Canvas, Mark as WMark, TextMask, TextRasterizer, Watermark};
use ds_export::{
    BatchOptions, CanvasLayout, ExportError, ExportFormat, ExportItem, ExportRecipe, OutputSpace,
    PhotoRect, PostProcess, Rendered, Renderer, Sizing, run_batch,
};

/// Paints pixel (0, 0) with `value` after checking it holds `expect` (the previous
/// step's mark), and counts its calls.
#[derive(Debug)]
struct Mark {
    name: &'static str,
    expect: Option<u8>,
    value: u8,
    calls: Mutex<Vec<&'static str>>,
}

impl PostProcess for Mark {
    fn name(&self) -> &str {
        self.name
    }

    fn apply(
        &self,
        canvas: Canvas<'_>,
        _photo: PhotoRect,
        _space: OutputSpace,
        _item: &ExportItem,
        _cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), ExportError> {
        let (first, kind) = match canvas {
            Canvas::Rgba8 { data, .. } => (&mut data[0], "rgba8"),
            Canvas::Rgb16(img) => {
                let v = (img.data[0] >> 8) as u8;
                if let Some(e) = self.expect
                    && v != e
                {
                    return Err(ExportError::PostProcess(format!("{}: saw {v}", self.name)));
                }
                img.data[0] = u16::from(self.value) * 257;
                self.calls.lock().unwrap().push("rgb16");
                return Ok(());
            }
        };
        if let Some(e) = self.expect
            && *first != e
        {
            return Err(ExportError::PostProcess(format!(
                "{}: saw {first}",
                self.name
            )));
        }
        *first = self.value;
        self.calls.lock().unwrap().push(kind);
        Ok(())
    }
}

fn mark(name: &'static str, expect: Option<u8>, value: u8) -> Arc<Mark> {
    Arc::new(Mark {
        name,
        expect,
        value,
        calls: Mutex::new(Vec::new()),
    })
}

fn gray16(w: u32, h: u32) -> Rendered {
    Rendered::Rgb16 {
        size: Size::new(w, h),
        data: vec![0x8080; (w * h * 3) as usize],
    }
}

/// Renders full size (16-bit), or 8-bit already sized when asked to (`render_for`).
struct Sized8;

impl Renderer for Sized8 {
    fn render(&self, _: &ExportItem, _: OutputSpace) -> Result<Rendered, String> {
        Ok(gray16(16, 8))
    }

    fn render_for(
        &self,
        _: &ExportItem,
        recipe: &ExportRecipe,
    ) -> Option<Result<Rendered, String>> {
        let size = recipe.target_size(Size::new(16, 8)).ok()?;
        Some(Ok(Rendered::Rgba8 {
            size,
            data: vec![128; size.pixels() as usize * 4],
        }))
    }
}

fn first_png_pixel(path: &Path) -> u8 {
    let dec = png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()));
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    buf[0]
}

fn item() -> ExportItem {
    ExportItem {
        id: 1,
        name: "a".into(),
        seq: 1,
        ..ExportItem::default()
    }
}

fn run(recipes: &[ExportRecipe], renderer: &dyn Renderer) -> ds_export::BatchReport {
    run_batch(
        &[item()],
        recipes,
        renderer,
        BatchOptions::default(),
        &|_| {},
        &AtomicBool::new(false),
    )
}

#[test]
fn steps_run_in_order_on_the_16_bit_path_and_only_on_their_recipe() {
    let d = tempfile::tempdir().unwrap();
    let (a, b) = (mark("A", None, 10), mark("B", Some(10), 20));
    let mut with = ExportRecipe::new("with", ExportFormat::Png8, d.path().join("with"));
    with.post_process = vec![a.clone(), b.clone()];
    // Same output space: both recipes share one full render.
    let without = ExportRecipe::new("without", ExportFormat::Png8, d.path().join("without"));
    let render = |_: &ExportItem, _: OutputSpace| Ok(gray16(16, 8));
    let rep = run(&[with, without], &render);
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(*a.calls.lock().unwrap(), ["rgb16"]);
    assert_eq!(*b.calls.lock().unwrap(), ["rgb16"]);
    assert_eq!(first_png_pixel(&d.path().join("with/a.png")), 20);
    assert_eq!(first_png_pixel(&d.path().join("without/a.png")), 128);
}

#[test]
fn steps_run_on_the_8_bit_sized_path() {
    let d = tempfile::tempdir().unwrap();
    let a = mark("A", Some(128), 7);
    let mut r = ExportRecipe::new("web", ExportFormat::Png8, d.path());
    r.sizing = Sizing::LongEdge(8);
    r.post_process = vec![a.clone()];
    let rep = run(&[r], &Sized8);
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(*a.calls.lock().unwrap(), ["rgba8"]);
    assert_eq!(first_png_pixel(&d.path().join("a.png")), 7);
}

#[test]
fn a_failing_step_fails_that_file_with_its_message() {
    let d = tempfile::tempdir().unwrap();
    let mut r = ExportRecipe::new("web", ExportFormat::Png8, d.path().join("out"));
    r.post_process = vec![mark("Strict", Some(99), 1)];
    let render = |_: &ExportItem, _: OutputSpace| Ok(gray16(4, 4));
    let rep = run(&[r], &render);
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(rep.failed[0].error.to_string(), "Strict: saw 128");
    assert_eq!(rep.written, [] as [ds_export::ExportedFile; 0]);
    let leftovers = fs::read_dir(d.path().join("out")).map_or(0, Iterator::count);
    assert_eq!(leftovers, 0);
}

/// Covers every pixel it is asked for: the watermark text is a solid block.
struct Solid;

impl TextRasterizer for Solid {
    fn rasterize(&self, _: &str, _: &str, px: f32) -> Result<TextMask, String> {
        let n = px.ceil().max(1.0) as u32;
        Ok(TextMask {
            width: n,
            height: n,
            coverage: vec![255; (n * n) as usize],
        })
    }
}

#[test]
fn the_watermark_is_the_last_step() {
    let d = tempfile::tempdir().unwrap();
    // Paints (0, 0) 5; the watermark's red block then covers it.
    let before = mark("Before", None, 5);
    let mut r = ExportRecipe::new("web", ExportFormat::Png8, d.path());
    r.watermark = Some(Watermark {
        mark: WMark::Text {
            text: "X".into(),
            font: String::new(),
            size: 0.25,
            color: [255, 0, 0],
            shadow: false,
        },
        anchor: Anchor::TopLeft,
        inset: 0.0,
        opacity: 1.0,
    });
    r.post_process = vec![before.clone()];
    let render = |_: &ExportItem, _: OutputSpace| Ok(gray16(16, 8));
    let opts = BatchOptions {
        text: Some(&Solid),
        ..BatchOptions::default()
    };
    let rep = run_batch(
        &[item()],
        &[r],
        &render,
        opts,
        &|_| {},
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(*before.calls.lock().unwrap(), ["rgb16"]);
    assert_eq!(first_png_pixel(&d.path().join("a.png")), 255);
}

/// Adds a `m`-pixel margin and checks the photo's place in the new canvas.
#[derive(Debug)]
struct Margin {
    m: u32,
    seen: Mutex<Option<(PhotoRect, Size)>>,
}

impl PostProcess for Margin {
    fn name(&self) -> &'static str {
        "Margin"
    }

    fn layout(
        &self,
        size: Size,
        _: OutputSpace,
        _: &ExportItem,
    ) -> Result<CanvasLayout, ExportError> {
        Ok((
            Size::new(size.width + 2 * self.m, size.height + 2 * self.m),
            (self.m, self.m),
        ))
    }

    fn apply(
        &self,
        canvas: Canvas<'_>,
        photo: PhotoRect,
        _: OutputSpace,
        _: &ExportItem,
        _: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), ExportError> {
        let size = match canvas {
            Canvas::Rgba8 { size, .. } => size,
            Canvas::Rgb16(img) => img.size,
        };
        *self.seen.lock().unwrap() = Some((photo, size));
        Ok(())
    }
}

fn png_info(path: &Path) -> (u32, u32, Vec<u8>) {
    let dec = png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()));
    let mut r = dec.read_info().unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    (info.width, info.height, buf)
}

#[test]
fn a_step_can_enlarge_the_canvas_on_both_paths() {
    for sized in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let margin = Arc::new(Margin {
            m: 2,
            seen: Mutex::new(None),
        });
        let mut r = ExportRecipe::new("web", ExportFormat::Png8, d.path());
        r.sizing = Sizing::LongEdge(16);
        r.post_process = vec![margin.clone()];
        let rep = if sized {
            run(&[r], &Sized8)
        } else {
            run(&[r], &|_: &ExportItem, _: OutputSpace| Ok(gray16(16, 8)))
        };
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        let (w, h, px) = png_info(&d.path().join("a.png"));
        assert_eq!((w, h), (20, 12), "sized {sized}");
        let (photo, canvas) = margin.seen.lock().unwrap().unwrap();
        assert_eq!(
            photo,
            PhotoRect {
                x: 2,
                y: 2,
                width: 16,
                height: 8
            }
        );
        assert_eq!(canvas, Size::new(20, 12));
        assert_eq!(px[0], 0, "the margin starts black");
    }
}

/// Asks for a canvas the photo doesn't fit in, or panics.
#[derive(Debug)]
struct Broken(bool);

impl PostProcess for Broken {
    fn name(&self) -> &'static str {
        "Broken"
    }

    fn layout(
        &self,
        size: Size,
        _: OutputSpace,
        _: &ExportItem,
    ) -> Result<CanvasLayout, ExportError> {
        if self.0 {
            Ok((Size::new(size.width, size.height), (1, 0)))
        } else {
            Ok((size, (0, 0)))
        }
    }

    fn apply(
        &self,
        _: Canvas<'_>,
        _: PhotoRect,
        _: OutputSpace,
        _: &ExportItem,
        _: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), ExportError> {
        panic!("boom")
    }
}

#[test]
fn a_bad_layout_or_a_panic_fails_only_that_file() {
    for bad_layout in [true, false] {
        let d = tempfile::tempdir().unwrap();
        let mut r = ExportRecipe::new("web", ExportFormat::Png8, d.path().join("out"));
        r.post_process = vec![Arc::new(Broken(bad_layout))];
        let rep = run(&[r], &|_: &ExportItem, _: OutputSpace| Ok(gray16(4, 4)));
        assert_eq!(rep.failed.len(), 1);
        let msg = rep.failed[0].error.to_string();
        assert!(
            msg.starts_with("Broken:")
                && (msg.contains("doesn't hold") || msg.contains("crashed (boom)")),
            "{msg}"
        );
        assert_eq!(rep.written, [] as [ds_export::ExportedFile; 0]);
    }
}

#[test]
fn arcs_and_the_watermark_step_are_steps() {
    let a = mark("A", None, 1);
    let shared: Arc<dyn PostProcess + Send + Sync> = a;
    assert_eq!(PostProcess::name(&shared), "A");
    let wm = Watermark {
        mark: WMark::Text {
            text: "X".into(),
            font: String::new(),
            size: 0.1,
            color: [0, 0, 0],
            shadow: false,
        },
        anchor: Anchor::default(),
        inset: 0.0,
        opacity: 1.0,
    };
    let step = ds_export::WatermarkStep {
        watermark: &wm,
        text: None,
    };
    assert_eq!(step.name(), "Watermark");
    assert!(format!("{step:?}").contains("text: false"));
}

/// Cancels the batch itself, then records whether its `cancelled` probe says so.
#[derive(Debug, Default)]
struct SeesCancel {
    flag: Arc<AtomicBool>,
    seen: Mutex<Option<bool>>,
}

impl PostProcess for SeesCancel {
    fn name(&self) -> &'static str {
        "SeesCancel"
    }

    fn apply(
        &self,
        _: Canvas<'_>,
        _: PhotoRect,
        _: OutputSpace,
        _: &ExportItem,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), ExportError> {
        let before = cancelled();
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
        *self.seen.lock().unwrap() = Some(!before && cancelled());
        Ok(())
    }
}

#[test]
fn steps_see_the_batch_cancel() {
    let d = tempfile::tempdir().unwrap();
    let seen = Arc::new(SeesCancel::default());
    let mut r = ExportRecipe::new("web", ExportFormat::Png8, d.path());
    r.post_process = vec![seen.clone()];
    let render = |_: &ExportItem, _: OutputSpace| Ok(gray16(4, 4));
    let flag = seen.flag.clone();
    let _ = run_batch(
        &[item()],
        &[r],
        &render,
        BatchOptions::default(),
        &|_| {},
        &flag,
    );
    assert_eq!(*seen.seen.lock().unwrap(), Some(true));
}

/// Round 2 (N13): a step can't grow the canvas past twice the image's area.
#[test]
fn canvas_growth_is_capped() {
    let d = tempfile::tempdir().unwrap();
    let mut r = ExportRecipe::new("web", ExportFormat::Png8, d.path().join("out"));
    // 16×8 + 2×8 margins = 32×24: 6× the area.
    r.post_process = vec![Arc::new(Margin {
        m: 8,
        seen: Mutex::new(None),
    })];
    let rep = run(&[r], &|_: &ExportItem, _: OutputSpace| Ok(gray16(16, 8)));
    assert_eq!(rep.failed.len(), 1);
    assert!(
        rep.failed[0].error.to_string().contains("more than twice"),
        "{}",
        rep.failed[0].error
    );
}

/// Records the output space its `layout` and `apply` are given.
#[derive(Debug, Default)]
struct SpaceSeen(Mutex<Vec<OutputSpace>>);

impl PostProcess for SpaceSeen {
    fn name(&self) -> &'static str {
        "Space"
    }

    fn layout(
        &self,
        size: Size,
        space: OutputSpace,
        _: &ExportItem,
    ) -> Result<CanvasLayout, ExportError> {
        self.0.lock().unwrap().push(space);
        Ok((size, (0, 0)))
    }

    fn apply(
        &self,
        _: Canvas<'_>,
        _: PhotoRect,
        space: OutputSpace,
        _: &ExportItem,
        _: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), ExportError> {
        self.0.lock().unwrap().push(space);
        Ok(())
    }
}

#[test]
fn layout_and_apply_see_the_same_output_space() {
    let seen = Arc::new(SpaceSeen::default());
    let steps: Vec<Box<dyn PostProcess + '_>> = vec![Box::new(Arc::clone(&seen))];
    let mut size = Size::new(2, 2);
    let mut data = vec![0u8; 16];
    ds_export::post::run_rgba8(
        &steps,
        &mut size,
        &mut data,
        OutputSpace::DisplayP3,
        &item(),
        &|| false,
    )
    .unwrap();
    assert_eq!(
        *seen.0.lock().unwrap(),
        [OutputSpace::DisplayP3, OutputSpace::DisplayP3]
    );
}
