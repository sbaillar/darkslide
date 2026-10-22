//! Peak-memory regression test for the batch exporter (the memory cap
//! measured 4x over). Its own test binary so the counting allocator sees
//! only this test.

use std::sync::atomic::AtomicBool;

use ds_core::Size;
use ds_export::{
    BatchOptions, ExportFormat, ExportItem, ExportRecipe, OutputSpace, Rendered, Sizing, run_batch,
};
use peak_alloc::PeakAlloc;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

const W: u32 = 1500;
const H: u32 = 1000;
/// One full-resolution 16-bit RGB render.
const FULL: usize = (W * H * 6) as usize;

fn render(kind: u8) -> impl Fn(&ExportItem, OutputSpace) -> Result<Rendered, String> + Sync {
    move |it, _| {
        let size = Size::new(W, H);
        let n = (W * H) as usize;
        Ok(if kind == 16 {
            Rendered::Rgb16 {
                size,
                data: (0..n * 3)
                    .map(|i| (i as u64 * 7919 + it.id) as u16)
                    .collect(),
            }
        } else {
            Rendered::Rgba8 {
                size,
                data: (0..n * 4).map(|i| (i * 31) as u8).collect(),
            }
        })
    }
}

fn peak_for(kind: u8, max_in_flight: usize) -> usize {
    let d = tempfile::tempdir().unwrap();
    // Every buffer-hungry path: original size + sharpen (working copy),
    // resize + sharpen, 16-bit PNG and TIFF, JPEG 4:4:4.
    let mut orig = ExportRecipe::new("o", ExportFormat::Jpeg { quality: 95 }, d.path().join("o"));
    orig.sharpen = 1.0;
    let mut small = ExportRecipe::new("s", ExportFormat::Tiff16, d.path().join("s"));
    small.sizing = Sizing::LongEdge(1200);
    small.sharpen = 0.5;
    let png = ExportRecipe::new("p", ExportFormat::Png16, d.path().join("p"));
    let items: Vec<ExportItem> = (0..4)
        .map(|i| ExportItem {
            id: i,
            name: format!("img{i}"),
            ..Default::default()
        })
        .collect();
    // Warm the static LUTs / rayon pool so they are not attributed to the batch.
    let _ = run_batch(
        &items[..1],
        std::slice::from_ref(&small),
        &render(kind),
        BatchOptions::default(),
        &|_| {},
        &AtomicBool::new(false),
    );
    let base = PEAK.current_usage();
    PEAK.reset_peak_usage();
    let rep = run_batch(
        &items,
        &[orig, small, png],
        &render(kind),
        BatchOptions {
            max_in_flight,
            heic: None,
            jpeg: None,
            upscaler: None,
            text: None,
            cancel: None,
        },
        &|_| {},
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(rep.written.len(), 12);
    PEAK.peak_usage().saturating_sub(base)
}

#[test]
fn peak_memory_is_render_plus_one_working_copy_per_worker() {
    // Slack: encoder/libjpeg/png internals, sharpen halos, per-thread rows.
    let slack = 6 << 20;
    for workers in [1usize, 2] {
        for kind in [16u8, 8] {
            let peak = peak_for(kind, workers);
            let bound = workers * 2 * FULL + slack;
            eprintln!(
                "workers={workers} input={kind}-bit peak={:.1} MB bound={:.1} MB ({:.2} full buffers/worker)",
                peak as f64 / 1e6,
                bound as f64 / 1e6,
                peak as f64 / (workers * FULL) as f64
            );
            assert!(
                peak <= bound,
                "peak {peak} > bound {bound} (workers={workers}, {kind}-bit)"
            );
        }
    }
}
