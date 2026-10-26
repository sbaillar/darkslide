//! Peak memory of a batch with AI Upscale. Its own test binary so the
//! counting allocator sees only this test.

use std::sync::atomic::AtomicBool;

use lt_core::Size;
use lt_export::{
    BatchOptions, ExportFormat, ExportItem, ExportRecipe, Image16, OutputSpace, Rendered, Sizing,
    Upscaler, run_batch,
};
use peak_alloc::PeakAlloc;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

const W: u32 = 1000;
const H: u32 = 750;
/// One full-resolution 16-bit RGB render.
const FULL: usize = (W * H * 6) as usize;

/// The model double: nearest neighbour 2× (one 4× image, like the real upscaler's output).
struct Nearest;

impl Upscaler for Nearest {
    fn upscale(&self, img: &Image16) -> Result<Option<Image16>, String> {
        let (w, h) = (img.size.width as usize, img.size.height as usize);
        let mut data = vec![0u16; 4 * w * h * 3];
        for (i, px) in data.as_chunks_mut::<3>().0.iter_mut().enumerate() {
            let (x, y) = (i % (2 * w), i / (2 * w));
            let p = ((y / 2) * w + x / 2) * 3;
            px.copy_from_slice(&img.data[p..p + 3]);
        }
        Ok(Some(
            Image16::new(Size::new(2 * w as u32, 2 * h as u32), data).unwrap(),
        ))
    }
}

fn pixels(it: &ExportItem) -> Rendered {
    let n = (W * H * 3) as usize;
    Rendered::Rgb16 {
        size: Size::new(W, H),
        data: (0..n).map(|i| (i as u64 * 7919 + it.id) as u16).collect(),
    }
}

#[test]
fn an_ai_upscale_batch_holds_one_render_and_one_2x_image() {
    let d = tempfile::tempdir().unwrap();
    // 200 % with sharpening (the 2× image is the working copy, never cloned) and 150 %
    // (resized from it), 16-bit TIFF.
    let mut double = ExportRecipe::new("d", ExportFormat::Tiff16, d.path().join("d"));
    double.sizing = Sizing::Percent(200.0);
    double.sharpen = 1.0;
    double.ai_upscale = true;
    let mut more = ExportRecipe::new("m", ExportFormat::Tiff16, d.path().join("m"));
    more.sizing = Sizing::Percent(150.0);
    more.ai_upscale = true;
    let items: Vec<ExportItem> = (0..4)
        .map(|i| ExportItem {
            id: i,
            name: format!("img{i}"),
            ..Default::default()
        })
        .collect();
    let render = |it: &ExportItem, _: OutputSpace| Ok(pixels(it));
    let opts = BatchOptions {
        max_in_flight: 2, // asked for two: AI Upscale runs one
        upscaler: Some(&Nearest),
        ..BatchOptions::default()
    };
    // Warm the static LUTs / rayon pool so they are not attributed to the batch.
    let _ = run_batch(
        &items[..1],
        std::slice::from_ref(&more),
        &render,
        opts,
        &|_| {},
        &AtomicBool::new(false),
    );
    let base = PEAK.current_usage();
    PEAK.reset_peak_usage();
    let rep = run_batch(
        &items,
        &[double, more],
        &render,
        opts,
        &|_| {},
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(rep.written.len(), 8);
    let peak = PEAK.peak_usage().saturating_sub(base);
    // One worker: the render, the 2× image, and the 150 % output resized from it
    // (2.25×), plus encoder strips and per-thread rows.
    let bound = FULL + 4 * FULL + 9 * FULL / 4 + (6 << 20);
    eprintln!(
        "AI Upscale batch peak {:.1} MB, bound {:.1} MB ({:.2} renders)",
        peak as f64 / 1e6,
        bound as f64 / 1e6,
        peak as f64 / FULL as f64
    );
    assert!(peak <= bound, "peak {peak} > bound {bound}");
}
