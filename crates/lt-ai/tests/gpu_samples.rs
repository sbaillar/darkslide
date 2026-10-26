//! The GPU batch path against the CPU reference on real A9 previews
//! (1616×1080), arranged as bursts with blurred, panned and smeared frames
//! and subject regions: every per-tile value, every group, pick and cull
//! reason must match. Skips without `samples/` or without a GPU adapter;
//! `src/gpu/tests.rs` covers the same logic on synthetic frames.

#![allow(clippy::float_cmp)]

mod support;

use lt_ai::{
    AnalysisConfig, CullThresholds, CullWeights, GpuAnalyzer, GroupConfig, GroupItem,
    ImageAnalysis, LumaImage, SubjectRegion, analyze_with_subject, group_images, score_group,
    suggest_picks,
};
use lt_core::NormRect;

fn shifted(img: &LumaImage, dx: usize) -> LumaImage {
    let (w, h) = (img.width(), img.height());
    let px = img.pixels();
    let data = (0..w * h)
        .map(|k| px[(k / w) * w + ((k % w) + dx).min(w - 1)])
        .collect();
    LumaImage::from_gray8(w, h, data).unwrap()
}

/// Largest `|a - b| / max(|a|, |b|)` over paired values (0 when equal).
fn max_rel(a: impl IntoIterator<Item = f32>, b: impl IntoIterator<Item = f32>) -> f64 {
    a.into_iter()
        .zip(b)
        .map(|(a, b)| {
            let (a, b) = (f64::from(a), f64::from(b));
            let m = a.abs().max(b.abs());
            if m == 0.0 { 0.0 } else { (a - b).abs() / m }
        })
        .fold(0.0, f64::max)
}

fn tile_values(a: &ImageAnalysis) -> Vec<f32> {
    let t = &a.sharpness.tiles;
    t.sharpness
        .iter()
        .chain(&t.detail)
        .copied()
        .chain(t.orientation.iter().flatten().copied())
        .collect()
}

/// Groups, then per group the scores (with reasons) and the picks.
fn cull(analyses: &[ImageAnalysis], seqs: &[(i64, u64)]) -> Vec<String> {
    let items: Vec<GroupItem> = analyses
        .iter()
        .zip(seqs)
        .map(|(a, &(t, seq))| GroupItem {
            capture_ms: Some(t),
            seq: Some(seq),
            camera: None,
            hash: a.hash,
        })
        .collect();
    let t = CullThresholds::default();
    group_images(&items, &GroupConfig::default(), None)
        .iter()
        .map(|g| {
            let frames: Vec<ImageAnalysis> =
                g.members.iter().map(|&i| analyses[i].clone()).collect();
            let scores = score_group(&frames, &[], &CullWeights::default(), &t);
            let picks = suggest_picks(&scores, &t);
            format!("{g:?} {scores:?} {picks:?}")
        })
        .collect()
}

#[test]
fn gpu_batch_matches_cpu_on_real_bursts() {
    let previews = support::sample_previews();
    if previews.is_empty() {
        eprintln!("skipping: no samples/");
        return;
    }
    let gpu = match GpuAnalyzer::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let face = SubjectRegion {
        rect: NormRect {
            x: 0.4,
            y: 0.3,
            w: 0.2,
            h: 0.3,
        },
        weight: 1.0,
    };
    let (mut images, mut subjects, mut seqs) = (Vec::new(), Vec::new(), Vec::new());
    for (k, (_, img)) in previews.iter().enumerate() {
        let frames = [
            support::box_blur(img, 2, 2),
            img.clone(),
            shifted(img, 12),
            support::box_blur(img, 5, 0),
        ];
        for (f, frame) in frames.into_iter().enumerate() {
            images.push(frame);
            // Every other frame carries a subject (a Vision face / AF point).
            subjects.push(if f % 2 == 1 { vec![face] } else { vec![] });
            seqs.push((
                k as i64 * 3_000 + (f as i64 / 2) * 1_000,
                (k * 4 + f) as u64,
            ));
        }
    }
    let cfg = AnalysisConfig::default();
    let cpu: Vec<ImageAnalysis> = images
        .iter()
        .zip(&subjects)
        .map(|(i, s)| analyze_with_subject(i, &cfg, s))
        .collect();
    let got = gpu.analyze_batch_with_subjects(&images, &subjects);
    assert_eq!(gpu.gpu_chunks(), 1, "the batch must run on the GPU");

    let tiles = cpu
        .iter()
        .zip(&got)
        .map(|(c, g)| max_rel(tile_values(c), tile_values(g)))
        .fold(0.0, f64::max);
    let scalars = cpu
        .iter()
        .zip(&got)
        .map(|(c, g)| {
            let v = |a: &ImageAnalysis| {
                [
                    a.sharpness_value(),
                    a.sharpness.laplacian_var,
                    a.sharpness.tenengrad,
                    a.sharpness.directional_ratio,
                    a.noise.sigma,
                    a.noise.laplacian_floor,
                    a.exposure.mean,
                    a.exposure.highlights_clipped,
                    a.exposure.dynamic_range_stops,
                ]
            };
            max_rel(v(c), v(g))
        })
        .fold(0.0, f64::max);
    eprintln!(
        "{} frames: max relative difference per tile {tiles:e}, per scalar {scalars:e}",
        images.len()
    );
    assert!(tiles <= 1e-6 && scalars <= 1e-6, "{tiles} {scalars}");
    // The kernels are integer-exact, so the results are in fact identical.
    for (i, (c, g)) in cpu.iter().zip(&got).enumerate() {
        assert_eq!(c, g, "frame {i}");
    }
    assert!(got.iter().any(|a| a.sharpness.subject.is_some()));
    assert_eq!(cull(&cpu, &seqs), cull(&got, &seqs));
}
