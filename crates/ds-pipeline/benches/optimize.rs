//! AI Optimize budget (ARCHITECTURE.md): < 150 ms for the analysis of a 24 MP A9 frame
//! once the viewer has shown it (RAW resident on the GPU), two ≤ 1024 px Tool-priority
//! probes with GPU histograms. Plus the CPU reference on a synthetic frame and the rules
//! alone. Real A9 ARW via `ds_raw::decode`; the sample bench skips without `samples/`.

#[path = "../tests/support/mod.rs"]
mod support;

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use ds_core::Adjustments;
use ds_pipeline::optimize::{Input, SceneSignals, analyze, analyze_cpu};
use ds_pipeline::{ImageKey, OutputFormat, RenderRequest, RenderedImage, Renderer, output_size};

fn benches(c: &mut Criterion) {
    let mut g = c.benchmark_group("optimize");
    g.sample_size(20).measurement_time(Duration::from_secs(8));
    let (adj, scene) = (Adjustments::default(), SceneSignals::default());

    let synthetic = support::synthetic_raw(1200, 800);
    let inp = Input {
        raw: &synthetic,
        adj: &adj,
        scene: &scene,
        prior: None,
        request: RenderRequest::default(),
    };
    g.bench_function("analyze_cpu_synthetic_1200", |b| {
        b.iter(|| analyze_cpu(&inp).unwrap());
    });

    let (Some(path), Ok(gpu)) = (ds_testdata::sample_arw(), ds_pipeline::gpu_renderer()) else {
        eprintln!("no samples or no GPU; skipping the A9 bench");
        return;
    };
    let raw = support::load_raw(&path).expect("decode");
    let inp = Input {
        raw: &raw,
        adj: &adj,
        scene: &scene,
        prior: None,
        request: RenderRequest {
            capture: support::capture_info(&path),
            ..RenderRequest::default()
        },
    };
    let key = ImageKey(0xA9);
    let mut r = gpu.clone();
    let mut run = || {
        let blocks = gpu.block_means(key, &raw);
        analyze(&inp, &blocks, &mut |a, req| {
            let size = output_size(&raw, a, req)?;
            let mut out = vec![0u8; size.pixels() as usize * 4];
            let f = r.render_frame_into(key, &raw, a, req, OutputFormat::Rgba8, &mut out)?;
            Ok((
                RenderedImage { size, rgba8: out },
                f.histogram.unwrap_or_default(),
            ))
        })
        .unwrap()
    };
    run(); // resident, as when the viewer shows the photo
    g.bench_function("analyze_gpu_a9_resident", |b| b.iter(&mut run));
    g.finish();
}

criterion_group!(optimize, benches);
criterion_main!(optimize);
