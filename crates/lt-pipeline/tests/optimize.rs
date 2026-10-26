//! AI Optimize on the GPU and on real A9 samples (skips without an adapter / samples).

mod support;

use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::Instant;

use lt_core::{Adjustments, RawImage};
use lt_pipeline::optimize::{
    Input, OptimizeReport, SceneSignals, analyze, analyze_cpu, apply, mark_applied,
};
use lt_pipeline::{
    GpuRenderer, ImageKey, OutputFormat, RenderRequest, RenderedImage, Renderer, output_size,
};

fn key(p: &Path) -> ImageKey {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    ImageKey(h.finish())
}

/// Analyze on the GPU: blocks from the resident mosaic, probes with GPU histograms.
fn gpu_analyze(gpu: &GpuRenderer, key: ImageKey, input: &Input<'_>) -> OptimizeReport {
    let blocks = gpu.block_means(key, input.raw);
    let mut g = gpu.clone();
    analyze(input, &blocks, &mut |adj, req| {
        let size = output_size(input.raw, adj, req)?;
        let mut out = vec![0u8; size.pixels() as usize * 4];
        let f = g.render_frame_into(key, input.raw, adj, req, OutputFormat::Rgba8, &mut out)?;
        let h = f.histogram.expect("histogram requested");
        Ok((RenderedImage { size, rgba8: out }, h))
    })
    .unwrap()
}

fn input<'a>(
    raw: &'a RawImage,
    adj: &'a Adjustments,
    scene: &'a SceneSignals,
    path: Option<&Path>,
) -> Input<'a> {
    Input {
        raw,
        adj,
        scene,
        prior: None,
        request: RenderRequest {
            capture: path.map(support::capture_info).unwrap_or_default(),
            ..RenderRequest::default()
        },
    }
}

/// The GPU's statistics give the CPU reference's proposals.
#[test]
fn gpu_analysis_matches_the_cpu() {
    let Ok(gpu) = lt_pipeline::gpu_renderer() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    let raw = support::synthetic_raw(600, 400);
    let (adj, scene) = (Adjustments::default(), SceneSignals::default());
    let inp = input(&raw, &adj, &scene, None);
    let cpu = analyze_cpu(&inp).unwrap();
    let g = gpu_analyze(&gpu, ImageKey(0x0b7), &inp);
    let fields = |r: &OptimizeReport| r.changes.iter().map(|c| c.field).collect::<Vec<_>>();
    assert_eq!(fields(&g), fields(&cpu));
    for (a, b) in g.changes.iter().zip(&cpu.changes) {
        assert_eq!(
            a.reason.clone(),
            b.reason.clone(),
            "{} vs {}",
            a.explanation,
            b.explanation
        );
    }
    assert_eq!(g.measurements.log_avg, cpu.measurements.log_avg);
    // Invalid input → empty blocks, no panic.
    let mut bad = raw.clone();
    bad.white_level = 0.0;
    assert_eq!(gpu.block_means(ImageKey(0x0b8), &bad), [] as [[f32; 3]; 0]);
}

/// Real A9 frames: sensible proposals, explanations for each, idempotent, and the
/// analysis inside its budget once the photo is resident (the viewer has shown it).
#[test]
fn real_samples_are_optimized_sensibly() {
    let samples = lt_testdata::samples_with_ext("arw");
    let Ok(gpu) = lt_pipeline::gpu_renderer() else {
        eprintln!("no GPU adapter; skipping");
        return;
    };
    if samples.is_empty() {
        eprintln!("no samples; skipping");
        return;
    }
    let mut times = Vec::new();
    for p in samples.iter().take(8) {
        let raw = support::load_raw(p).expect("decode");
        let (adj, scene) = (
            Adjustments::default(),
            SceneSignals {
                ai_denoise_available: true,
                ..SceneSignals::default()
            },
        );
        let k = key(p);
        let inp = input(&raw, &adj, &scene, Some(p));
        let _ = gpu_analyze(&gpu, k, &inp); // resident, like the photo on screen
        let t = Instant::now();
        let r = gpu_analyze(&gpu, k, &inp);
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        eprintln!(
            "{}: {:.0} ms\n  {}",
            p.display(),
            times.last().unwrap(),
            r.summary
        );
        for c in &r.changes {
            eprintln!("  + {}", c.explanation);
            assert!(c.explanation.starts_with(&c.label), "{}", c.explanation);
            assert!((0.0..=1.0).contains(&c.confidence));
        }
        if let Some(c) = r.changes.iter().find(|c| c.label == "Exposure") {
            let lt_pipeline::optimize::Setting::Number { value } = c.new else {
                panic!()
            };
            assert!(value.abs() <= 2.0, "{value}");
        }
        let ids: Vec<u32> = r
            .changes
            .iter()
            .filter(|c| c.default_on)
            .map(|c| c.id)
            .collect();
        let applied = apply(&adj, &r, &ids);
        let prior = mark_applied(&r, &ids);
        let again = gpu_analyze(
            &gpu,
            k,
            &Input {
                prior: Some(&prior),
                ..input(&raw, &applied, &scene, Some(p))
            },
        );
        let left: Vec<_> = again
            .changes
            .iter()
            .filter(|c| c.default_on)
            .map(|c| &c.explanation)
            .collect();
        assert!(
            left.is_empty(),
            "{}: second run proposes {left:#?}",
            p.display()
        );
        gpu.clone().evict(k);
    }
    times.sort_by(f64::total_cmp);
    let median = times[times.len() / 2];
    eprintln!(
        "analysis median {median:.1} ms over {} samples",
        times.len()
    );
    // The budget is for release builds (the bench measures it); debug and coverage
    // builds only check the behaviour.
    if !cfg!(debug_assertions) {
        assert!(
            median <= 150.0 * support::load_factor(),
            "median {median:.1} ms"
        );
    }
}
