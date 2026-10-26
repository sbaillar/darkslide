//! Content-aware Remove, the Rust side of a long thin removal: a corner-to-corner stroke
//! (1% brush, 241 points) on a synthetic 61 MP (9504×6336) mosaic, filled tile by tile.
//! `model` uses a stand-in for `LaMa` (800 px, returns gray: the app's Neural Engine time is
//! not counted), `patchmatch` the built-in fallback. Budget: `model` under 1.5 s and
//! 1 GB peak (M4 Pro: ~0.5 s, ~0.47 GB peak RSS for the whole process including the
//! frame and its derived copy; it was ~8 s and ~6.5 GB with one full-frame window).

use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lt_core::adjust::{BrushStroke, Removal};
use lt_core::{Adjustments, CfaPattern, Orientation, RawImage, Size};
use lt_pipeline::removal::{forget, resolve};
use lt_pipeline::{ImageKey, Inpainter, RenderRequest};

fn frame() -> RawImage {
    let (w, h) = (9504u32, 6336u32);
    RawImage {
        size: Size::new(w, h),
        cfa: CfaPattern::Rggb,
        data: (0..w * h)
            .map(|i| (2000 + (i.wrapping_mul(2_654_435_761) >> 20) % 400) as u16)
            .collect(),
        black_level: [512.0; 4],
        white_level: 16383.0,
        wb_as_shot: [2.0, 1.0, 1.5],
        wb_daylight: [2.0, 1.0, 1.5],
        cam_to_xyz: [[0.41, 0.36, 0.18], [0.21, 0.72, 0.07], [0.02, 0.12, 0.95]],
        orientation: Orientation::Normal,
        default_crop: None,
    }
}

struct Gray800;

impl Inpainter for Gray800 {
    fn id(&self) -> String {
        "gray-800".into()
    }
    fn max_edge(&self) -> u32 {
        800
    }
    fn inpaint(&self, _w: u32, _h: u32, rgb: &[u8], _mask: &[u8]) -> Option<Vec<u8>> {
        Some(vec![128; rgb.len()])
    }
}

fn removal(c: &mut Criterion) {
    let raw = frame();
    let adj = Adjustments {
        removals: vec![Removal {
            strokes: vec![BrushStroke {
                points: (0..=240)
                    .map(|i| {
                        let t = 0.02 + 0.96 * i as f32 / 240.0;
                        [t, t]
                    })
                    .collect(),
                size: 0.01,
                ..BrushStroke::default()
            }],
            feather: 0.002,
        }],
        ..Adjustments::default()
    };
    let mut g = c.benchmark_group("removal_long_stroke_61mp");
    g.sample_size(10).measurement_time(Duration::from_secs(20));
    for (name, inpainter) in [
        ("model", Some(Arc::new(Gray800) as Arc<dyn Inpainter>)),
        ("patchmatch", None),
    ] {
        let req = RenderRequest {
            inpainter,
            ..RenderRequest::new(Size::new(1024, 1024))
        };
        g.bench_function(name, |b| {
            b.iter(|| {
                // Uncached every time: memory only, and forgotten first.
                forget(ImageKey(1));
                resolve(ImageKey(1), &raw, &adj, &req).unwrap()
            });
        });
    }
    g.finish();
}

criterion_group!(benches, removal);
criterion_main!(benches);
