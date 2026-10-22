//! Budgets: all scoring for one 1616×1080 preview ≤ 8 ms single-threaded;
//! grouping 2 000 images ≤ 50 ms; a 64-preview batch on the GPU ≥ 5× the
//! single-thread CPU and faster than the rayon CPU batch. Uses real A9
//! previews when `samples/` is present, synthetic scenes of the same size
//! otherwise.

#[path = "../tests/support/mod.rs"]
mod support;

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ds_ai::{
    AnalysisConfig, CullThresholds, CullWeights, GpuAnalyzer, GroupConfig, GroupItem, LumaImage,
    PerceptualHash, RuleBasedParser, SubjectRegion, analyze, analyze_batch_cpu,
    analyze_with_subject, group_images, score_group, suggest_picks,
};

fn preview() -> (String, LumaImage) {
    support::sample_previews()
        .into_iter()
        .next()
        .unwrap_or_else(|| ("synthetic".into(), support::synthetic_scene(1616, 1080, 1)))
}

fn bench_analyze(c: &mut Criterion) {
    let (name, img) = preview();
    let cfg = AnalysisConfig::default();
    c.bench_function(&format!("analyze_1616x1080/{name}"), |b| {
        b.iter(|| analyze(black_box(&img), &cfg));
    });
    c.bench_function("phash_1616x1080", |b| {
        b.iter(|| PerceptualHash::of(black_box(&img)));
    });
    let rgba: Vec<u8> = img.pixels().iter().flat_map(|&p| [p, p, p, 255]).collect();
    c.bench_function("rgba_to_luma_1616x1080", |b| {
        b.iter(|| LumaImage::from_rgba8(1616, 1080, black_box(&rgba)).unwrap());
    });
}

/// `n` A9-sized previews: the samples repeated, or synthetic scenes.
fn batch(samples: &[LumaImage], n: usize) -> Vec<LumaImage> {
    (0..n)
        .map(|k| {
            samples
                .get(k % samples.len().max(1))
                .cloned()
                .unwrap_or_else(|| support::synthetic_scene(1616, 1080, k as u64))
        })
        .collect()
}

/// Whole-batch analysis on each backend. Throughput is images/s.
fn bench_batch(c: &mut Criterion) {
    let samples: Vec<LumaImage> = support::sample_previews()
        .into_iter()
        .map(|(_, i)| i)
        .collect();
    let cfg = AnalysisConfig::default();
    let gpu = GpuAnalyzer::new()
        .inspect_err(|e| eprintln!("no GPU: {e}"))
        .ok();
    for n in [1, 8, 64, 256] {
        let imgs = batch(&samples, n);
        let mut g = c.benchmark_group(format!("analyze_batch_{n}x1616x1080"));
        g.sample_size(10);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_function("cpu_single_thread", |b| {
            b.iter(|| {
                imgs.iter()
                    .map(|i| analyze(black_box(i), &cfg))
                    .collect::<Vec<_>>()
            });
        });
        g.bench_function("cpu_rayon", |b| {
            b.iter(|| analyze_batch_cpu(black_box(&imgs), &cfg));
        });
        if let Some(gpu) = &gpu {
            g.bench_function("gpu", |b| {
                b.iter(|| gpu.analyze_batch(black_box(&imgs)));
            });
        }
        g.finish();
    }
}

fn bench_group(c: &mut Criterion) {
    // 2 000 frames: bursts of 1–20 frames at 50 ms, bursts 2 s apart,
    // hashes drifting within a burst and jumping between bursts.
    let mut items = Vec::with_capacity(2000);
    let (mut t, mut s) = (0i64, 0x1234_5678_9abc_def1u64);
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    while items.len() < 2000 {
        let len = 1 + (next() % 20) as usize;
        let base = PerceptualHash {
            dhash: next(),
            phash: next(),
        };
        for k in 0..len.min(2000 - items.len()) {
            let hash = PerceptualHash {
                dhash: base.dhash ^ (1 << (k % 64)),
                phash: base.phash,
            };
            // Whole-second stamps (ARW) with shot numbers, as the A9 writes them.
            items.push(GroupItem {
                capture_ms: Some(t / 1000 * 1000),
                seq: Some(items.len() as u64),
                camera: Some(1),
                hash,
            });
            t += 50;
        }
        t += 2000;
    }
    let cfg = GroupConfig::default();
    c.bench_function("group_2000", |b| {
        b.iter(|| group_images(black_box(&items), &cfg, None));
    });
    let vision = |a: usize, b: usize| Some(((a ^ b) % 7) as f32 / 7.0);
    c.bench_function("group_2000_with_vision", |b| {
        b.iter(|| group_images(black_box(&items), &cfg, Some(&vision)));
    });
    // Worst case for look-back: 20 000 frames sharing one timestamp.
    let same: Vec<GroupItem> = (0..20_000u64)
        .map(|k| GroupItem {
            capture_ms: Some(1_000),
            seq: None,
            camera: None,
            hash: PerceptualHash {
                dhash: k.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                phash: k,
            },
        })
        .collect();
    c.bench_function("group_20000_same_timestamp", |b| {
        b.iter(|| group_images(black_box(&same), &cfg, None));
    });
}

fn bench_score(c: &mut Criterion) {
    let (_, img) = preview();
    let a = analyze(&img, &AnalysisConfig::default());
    let burst = vec![a; 20];
    let (w, t) = (CullWeights::default(), CullThresholds::default());
    c.bench_function("score_and_pick_burst_20", |b| {
        b.iter(|| suggest_picks(&score_group(black_box(&burst), &[], &w, &t), &t));
    });
    let region = [SubjectRegion::from_af_point([0.5, 0.5], 0.1)];
    c.bench_function("analyze_1616x1080_with_subject", |b| {
        b.iter(|| analyze_with_subject(black_box(&img), &AnalysisConfig::default(), &region));
    });
    let text = "a bit warmer, crop to 4:5, lift the shadows and more contrast";
    c.bench_function("parse_rules", |b| {
        b.iter(|| RuleBasedParser.parse_detailed(black_box(text)));
    });
}

criterion_group!(
    benches,
    bench_analyze,
    bench_batch,
    bench_group,
    bench_score
);
criterion_main!(benches);
