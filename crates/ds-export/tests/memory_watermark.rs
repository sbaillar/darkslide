//! Peak memory of a text watermark with a shadow: the shadow is a byte a pixel, not a
//! float. Its own test binary so the counting allocator sees only this test.

use ds_core::Size;
use ds_export::watermark::{Canvas, apply};
use ds_export::{
    Anchor, ExportItem, Image16, Mark, OutputSpace, TextMask, TextRasterizer, Watermark,
};
use peak_alloc::PeakAlloc;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

/// A rasteriser that returns a large fixed mask whatever it is asked.
struct Big;

impl TextRasterizer for Big {
    fn rasterize(&self, _: &str, _: &str, _: f32) -> Result<TextMask, String> {
        Ok(TextMask {
            width: 3000,
            height: 1000,
            coverage: vec![200; 3_000_000],
        })
    }
}

#[test]
fn the_shadow_costs_a_byte_a_pixel() {
    let (w, h) = (3200u32, 1200u32);
    let mut img = Image16::new(Size::new(w, h), vec![30_000; (w * h * 3) as usize]).unwrap();
    let wm = Watermark {
        mark: Mark::Text {
            text: "x".into(),
            font: String::new(),
            size: 0.4,
            color: [255, 255, 255],
            shadow: true,
        },
        anchor: Anchor::Center,
        inset: 0.0,
        opacity: 1.0,
    };
    let item = ExportItem::default();
    let draw = |img: &mut Image16| {
        apply(
            Canvas::Rgb16(img),
            OutputSpace::Srgb,
            &wm,
            &item,
            Some(&Big),
        )
        .unwrap();
    };
    draw(&mut img); // warm the lookup tables and the rayon pool
    let base = PEAK.current_usage();
    PEAK.reset_peak_usage();
    draw(&mut img);
    let peak = PEAK.peak_usage().saturating_sub(base);
    // The mask (3 MB) and its shadow (3.3 MB as bytes, 13 MB as floats), plus rows.
    let bound = 10 << 20;
    eprintln!(
        "peak {:.1} MB, bound {:.1} MB",
        peak as f64 / 1e6,
        bound as f64 / 1e6
    );
    assert!(peak <= bound, "peak {peak} > bound {bound}");
}
