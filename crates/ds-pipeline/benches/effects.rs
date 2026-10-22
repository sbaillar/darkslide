//! Effects budgets: an interactive lens-blur slider drag ≤ 33 ms at preview size on the
//! GPU (`Quality::Draft`, 5K fit), plus the settled preview and the CPU reference.
//! Real A9 ARW via `ds_raw::decode` when `samples/` exists, else a synthetic 24 MP frame.
#![allow(clippy::field_reassign_with_default)]

#[path = "../tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use ds_core::adjust::AiTarget;
use ds_core::effects::BokehShape;
use ds_core::{Adjustments, Size};
use ds_pipeline::{
    ImageKey, MaskBitmap, MaskSource, OutputFormat, Quality, RenderRequest, Renderer, cpu_renderer,
    gpu_renderer, output_size,
};

/// A 1024×683 depth map (the app's size): far at the top, nearer toward the bottom, a
/// near subject in the middle; and that subject's mask.
#[derive(Debug)]
struct Depth;
impl MaskSource for Depth {
    fn ai_mask(&self, t: AiTarget) -> Option<Arc<MaskBitmap>> {
        (t == AiTarget::Subject).then(|| Arc::new(subject(255, 0)))
    }
    fn depth_map(&self) -> Option<Arc<MaskBitmap>> {
        let s = subject(240, 1);
        Some(Arc::new(MaskBitmap { version: 5, ..s }))
    }
}

fn subject(near: u8, ramp: u32) -> MaskBitmap {
    let (w, h) = (1024u32, 683u32);
    MaskBitmap::new(
        Size::new(w, h),
        (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let (dx, dy) = (x as f32 / w as f32 - 0.5, y as f32 / h as f32 - 0.55);
                if dx * dx / 0.04 + dy * dy / 0.09 < 1.0 {
                    near
                } else {
                    (ramp * (30 + y * 120 / h)) as u8
                }
            })
            .collect(),
    )
}

fn benches(c: &mut Criterion) {
    let (raw, capture) = if let Some(p) = ds_testdata::sample_arw() {
        (
            support::load_raw(&p).expect("decode"),
            support::capture_info(&p),
        )
    } else {
        eprintln!("no samples/: a synthetic 6000×4000 frame");
        (
            support::synthetic_raw(6000, 4000),
            ds_pipeline::CaptureInfo::default(),
        )
    };
    let mut g = c.benchmark_group("effects");
    g.sample_size(20).measurement_time(Duration::from_secs(6));
    let masks: Option<Arc<dyn MaskSource>> = Some(Arc::new(Depth));
    let req = |q: Quality, long: u32| RenderRequest {
        capture: capture.clone(),
        quality: q,
        masks: masks.clone(),
        histogram: true,
        ..RenderRequest::new(Size::new(long, long))
    };
    let mut portrait = Adjustments::default();
    portrait.effects.lens_blur.amount = 80.0;
    portrait.effects.lens_blur.highlight_boost = 60.0;
    let mut all = portrait.clone();
    all.effects.lens_blur.shape = BokehShape::Blades;
    all.effects.glow.amount = 30.0;
    all.effects.halation.amount = 40.0;
    all.effects.grain.amount = 30.0;
    if let Ok(mut r) = gpu_renderer() {
        let key = ImageKey(1);
        for (name, base, q, long) in [
            (
                "gpu_lens_blur_amount_drag_draft_2832",
                &portrait,
                Quality::Draft,
                2832,
            ),
            (
                "gpu_lens_blur_amount_drag_draft_2560",
                &portrait,
                Quality::Draft,
                2560,
            ),
            (
                "gpu_all_effects_amount_drag_draft_2832",
                &all,
                Quality::Draft,
                2832,
            ),
            (
                "gpu_lens_blur_settled_preview_2560",
                &portrait,
                Quality::Preview,
                2560,
            ),
        ] {
            let rq = req(q, long);
            let mut adj = base.clone();
            let mut out =
                vec![0u8; output_size(&raw, &adj, &rq).expect("size").pixels() as usize * 4];
            let mut i = 0u32;
            g.bench_function(name, |b| {
                b.iter(|| {
                    // The Amount slider moving (every frame a new blur size).
                    i += 1;
                    adj.effects.lens_blur.amount = 40.0 + (i % 40) as f32;
                    r.render_into(key, &raw, &adj, &rq, OutputFormat::Rgba8, &mut out)
                        .expect("render")
                });
            });
        }
    } else {
        eprintln!("no GPU: skipping the GPU effects benches");
    }
    let mut cpu = cpu_renderer();
    let rq = req(Quality::Preview, 2560);
    g.bench_function("cpu_lens_blur_preview_2560", |b| {
        b.iter(|| {
            cpu.render(ImageKey(2), &raw, &portrait, &rq)
                .expect("render")
        });
    });
    g.finish();
}

criterion_group!(effects, benches);
criterion_main!(effects);
