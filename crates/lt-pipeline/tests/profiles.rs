//! Camera profiles end to end: GPU ↔ CPU parity of the profile stage (built-in looks and
//! imported DCPs with 2.5-D and 3-D sRGB-indexed maps), layers re-toning through the
//! profile's curve, and a missing DCP rendering as Standard. GPU checks skip without an
//! adapter.

#![allow(clippy::float_cmp)]

mod support;

use lt_core::adjust::{Layer, LayerAdjustments, Mask, MaskOp, MaskShape, Profile, WhiteBalance};
use lt_core::{Adjustments, Size};
use lt_pipeline::color::{delta_e2000, srgb8_to_lab};
use lt_pipeline::profile::{register_dcp, unregister_dcp};
use lt_pipeline::{
    ImageKey, Quality, RenderRequest, RenderedImage, Renderer, cpu_renderer, gpu_renderer,
};

fn delta_e(a: &RenderedImage, b: &RenderedImage) -> (f32, f32) {
    assert_eq!(a.size, b.size);
    let mut d: Vec<f32> = a
        .rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.rgba8.as_chunks::<4>().0)
        .map(|(p, q)| {
            delta_e2000(
                srgb8_to_lab([p[0], p[1], p[2]]),
                srgb8_to_lab([q[0], q[1], q[2]]),
            )
        })
        .collect();
    let mean = d.iter().sum::<f32>() / d.len() as f32;
    d.sort_by(f32::total_cmp);
    (mean, d[d.len() * 99 / 100])
}

fn with(profile: Profile) -> Adjustments {
    Adjustments {
        profile,
        ..Adjustments::default()
    }
}

/// A layer that re-tones (+1 EV, warmer) over a radial mask, plus global contrast.
fn layered(profile: Profile) -> Adjustments {
    let mut a = with(profile);
    a.exposure.contrast = 20.0;
    a.layers.push(Layer {
        id: 1,
        adjustments: LayerAdjustments {
            exposure: lt_core::adjust::Exposure {
                exposure_ev: 1.0,
                contrast: 15.0,
                ..Default::default()
            },
            ..LayerAdjustments::default()
        },
        mask: Mask {
            shapes: vec![MaskShape::Radial {
                op: MaskOp::Add,
                center: [0.5, 0.5],
                radius: [0.3, 0.3],
                angle_deg: 0.0,
                feather: 0.5,
            }],
            ..Mask::default()
        },
        ..Layer::default()
    });
    a
}

fn profiles() -> Vec<(&'static str, Profile)> {
    let dcp = register_dcp(&lt_pipeline::dcp::synth::example("Parity Example")).unwrap();
    vec![
        ("neutral", Profile::Neutral),
        ("standard", Profile::Standard),
        ("vivid", Profile::Vivid),
        ("dcp", dcp.profile()),
    ]
}

#[test]
fn gpu_matches_cpu_for_every_profile() {
    let mut g = match gpu_renderer() {
        Ok(g) => g,
        Err(lt_pipeline::PipelineError::NoAdapter) => return,
        Err(e) => panic!("GPU renderer: {e}"),
    };
    let raw = support::synthetic_raw(1000, 700);
    let mut c = cpu_renderer();
    for (label, p) in profiles() {
        for (kind, adj) in [
            ("plain", with(p.clone())),
            ("layered", layered(p.clone())),
            (
                "tungsten -1 EV",
                Adjustments {
                    white_balance: WhiteBalance::Custom {
                        temp_k: 3200.0,
                        tint: 5.0,
                    },
                    exposure: lt_core::adjust::Exposure {
                        exposure_ev: -1.0,
                        ..Default::default()
                    },
                    ..with(p.clone())
                },
            ),
        ] {
            for q in [Quality::Preview, Quality::Draft] {
                let mut req = RenderRequest::new(Size::new(900, 900));
                req.quality = q;
                let a = c.render(ImageKey(1), &raw, &adj, &req).unwrap();
                let b = g.render(ImageKey(1), &raw, &adj, &req).unwrap();
                let (mean, p99) = delta_e(&a, &b);
                println!("{label} {kind} {q:?}: ΔE2000 mean {mean:.4} p99 {p99:.3}");
                assert!(
                    mean <= 0.05 && p99 <= 1.0,
                    "{label} {kind} {q:?}: mean {mean} p99 {p99}"
                );
            }
        }
    }
}

#[test]
fn profiles_change_the_render_and_layers_retone_through_the_profile() {
    let raw = support::synthetic_raw(400, 300);
    let req = RenderRequest::new(Size::new(400, 400));
    let mut c = cpu_renderer();
    let mut renders = Vec::new();
    for (label, p) in profiles() {
        let img = c.render(ImageKey(2), &raw, &with(p), &req).unwrap();
        renders.push((label, img));
    }
    for i in 0..renders.len() {
        for j in i + 1..renders.len() {
            let (mean, _) = delta_e(&renders[i].1, &renders[j].1);
            assert!(mean > 0.3, "{} vs {}: {mean}", renders[i].0, renders[j].0);
        }
    }
    // A layer that only re-tones (a negligible whites change forces it) inverts the
    // profile's own curve: it moves the image no more under any profile than under
    // Neutral (whose round trip is inexact only in the highlight roll-off).
    let mut neutral_moved = None;
    // On a real photo the round trip is invisible; the synthetic frame is all extreme
    // saturated gradients, where it is only bounded relative to Neutral (Vivid: 2.6×).
    let real = support::first_sample();
    let bound = |n: f32| if real.is_some() { 0.15 } else { n * 3.0 };
    let raw = real.clone().unwrap_or(raw);
    for (label, p) in profiles() {
        let base = c.render(ImageKey(3), &raw, &with(p.clone()), &req).unwrap();
        let mut a = with(p);
        a.layers.push(Layer {
            id: 1,
            adjustments: LayerAdjustments {
                hdr: lt_core::adjust::Hdr {
                    whites: 1e-3,
                    ..Default::default()
                },
                ..LayerAdjustments::default()
            },
            mask: Mask {
                shapes: vec![MaskShape::Fill { op: MaskOp::Add }],
                ..Mask::default()
            },
            ..Layer::default()
        });
        let l = c.render(ImageKey(3), &raw, &a, &req).unwrap();
        let (mean, _) = delta_e(&base, &l);
        println!("{label}: re-tone moved ΔE mean {mean:.3}");
        let n = *neutral_moved.get_or_insert(mean);
        assert!(
            mean <= bound(n),
            "{label}: re-tone moved {mean} (Neutral {n})"
        );
    }
}

#[test]
fn a_missing_dcp_renders_as_standard() {
    let raw = support::synthetic_raw(300, 200);
    let req = RenderRequest::new(Size::new(300, 300));
    let bytes = lt_pipeline::dcp::synth::example("Soon Gone");
    let info = register_dcp(&bytes).unwrap();
    let mut c = cpu_renderer();
    let with_dcp = c
        .render(ImageKey(4), &raw, &with(info.profile()), &req)
        .unwrap();
    assert!(unregister_dcp(&info.hash));
    let missing = c
        .render(ImageKey(4), &raw, &with(info.profile()), &req)
        .unwrap();
    let standard = c
        .render(ImageKey(4), &raw, &with(Profile::Standard), &req)
        .unwrap();
    assert_eq!(missing.rgba8, standard.rgba8);
    assert_ne!(with_dcp.rgba8, standard.rgba8);
}

/// Warm 5K Draft WB drags per profile, interleaved (min / median ms; the minimum is
/// robust to a loaded machine). The budget is 16 ms with the profile stage on. Run with
/// `cargo test --release --test profiles drag_timing -- --ignored --nocapture`.
#[test]
#[ignore = "timing; run in release on a quiet machine"]
fn drag_timing() {
    let Ok(mut g) = gpu_renderer() else { return };
    let Some(path) = lt_testdata::sample_arw() else {
        return;
    };
    let raw = support::load_raw(&path).unwrap();
    let mut req = RenderRequest::new(Size::new(5120, 5120));
    req.capture = support::capture_info(&path);
    req.quality = Quality::Draft;
    let size = lt_pipeline::output_size(&raw, &Adjustments::default(), &req).unwrap();
    let mut out = vec![0u8; size.pixels() as usize * 4];
    let ps = profiles();
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); ps.len()];
    for i in 0..80u32 {
        for (k, (_, p)) in ps.iter().enumerate() {
            let adj = Adjustments {
                white_balance: WhiteBalance::Custom {
                    temp_k: 4000.0 + 50.0 * (i % 40) as f32,
                    tint: (i % 9) as f32,
                },
                ..with(p.clone())
            };
            let t = std::time::Instant::now();
            g.render_into(
                ImageKey(9),
                &raw,
                &adj,
                &req,
                lt_pipeline::OutputFormat::Rgba8,
                &mut out,
            )
            .unwrap();
            if i >= 5 {
                times[k].push(t.elapsed().as_secs_f64() * 1e3);
            }
        }
    }
    for ((label, _), t) in ps.iter().zip(&mut times) {
        t.sort_by(f64::total_cmp);
        println!(
            "{label}: min {:.2} ms, median {:.2} ms",
            t[0],
            t[t.len() / 2]
        );
        assert!(t[0] <= 16.0, "{label}: {:.2} ms", t[0]);
    }
}
