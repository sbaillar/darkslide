//! CPU work left in the GPU render path, measured (the audit behind the "move it to the
//! GPU or justify it" rule). Run in release with
//! `cargo test --release -p ds-pipeline --features stage-timings stage_timings -- --ignored --nocapture`.

use std::time::Instant;

use ds_core::adjust::{BrushStroke, Layer, LayerAdjustments, Mask, MaskOp, MaskShape};
use ds_core::{Adjustments, CfaPattern, Size};

use super::params::Tables;
use crate::mask::{self, RasterCache};
use crate::{CaptureInfo, ImageKey, RenderRequest, plan};

fn median_ms(n: usize, mut f: impl FnMut()) -> f64 {
    let mut v: Vec<f64> = (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    v.sort_by(f64::total_cmp);
    v[n / 2]
}

fn brushes() -> Vec<Layer> {
    (0..3)
        .map(|k| Layer {
            id: 10 + k,
            name: "b".into(),
            enabled: true,
            opacity: 1.0,
            adjustments: LayerAdjustments::default(),
            mask: Mask {
                shapes: vec![MaskShape::Brush {
                    op: MaskOp::Add,
                    strokes: vec![BrushStroke {
                        points: (0..400)
                            .map(|i| [0.1 + 0.002 * i as f32, 0.3 + 0.2 * k as f32])
                            .collect(),
                        size: 0.05,
                        hardness: 0.5,
                        flow: 0.8,
                        erase: false,
                        auto_mask: false,
                    }],
                    feather: 0.0,
                }],
                ..Mask::default()
            },
        })
        .collect()
}

#[test]
#[ignore = "timing report, run manually in release"]
fn stage_timings() {
    let (raw, capture) = ds_testdata::sample_arw().map_or_else(
        || {
            (
                crate::testutil::raw_from_fn(6000, 4000, |_, _| [0.2; 3]),
                CaptureInfo::default(),
            )
        },
        |p| {
            let m = ds_raw::read_metadata(&p).unwrap_or_default();
            (
                ds_raw::decode(&p).unwrap(),
                CaptureInfo {
                    lens: m.lens,
                    focal_length: m.focal_length,
                    aperture: m.aperture,
                    iso: m.iso,
                    crop_factor: None,
                    camera: None,
                },
            )
        },
    );
    let req = RenderRequest {
        capture,
        ..RenderRequest::new(Size::new(2560, 2560))
    };
    let adj = Adjustments::default();
    println!(
        "plan::build default (lens on): {:.3} ms",
        median_ms(50, || {
            std::hint::black_box(plan::build(&raw, &adj, &req).unwrap());
        })
    );
    let mut busy = adj.clone();
    busy.hdr.shadows = 40.0;
    busy.curve.rgb = vec![[0.25, 0.22], [0.75, 0.8]];
    busy.crop.angle_deg = 3.0;
    busy.layers = brushes();
    let p = plan::build(&raw, &busy, &req).unwrap();
    println!(
        "plan::build busy + straighten + 3 brush layers: {:.3} ms",
        median_ms(50, || {
            std::hint::black_box(plan::build(&raw, &busy, &req).unwrap());
        })
    );
    println!(
        "gpu Tables::build: {:.3} ms",
        median_ms(50, || {
            std::hint::black_box(Tables::build(&p));
        })
    );
    let cache = std::sync::Mutex::new(RasterCache::default());
    let compile = |cache: &std::sync::Mutex<RasterCache>| {
        for l in &busy.layers {
            std::hint::black_box(mask::compile(&l.mask, l.id, &p.geo, None, 1, cache));
        }
    };
    compile(&cache);
    println!(
        "mask compile, 3 brush layers, warm: {:.3} ms",
        median_ms(50, || compile(&cache))
    );
    println!(
        "mask compile, 3 brush layers, cold: {:.3} ms",
        median_ms(5, || {
            let c = std::sync::Mutex::new(RasterCache::default());
            compile(&c);
        })
    );
    let frame = vec![7u8; 2560 * 1707 * 4];
    let mut dst = vec![0u8; frame.len()];
    println!(
        "output copy, 2560 RGBA8 frame: {:.3} ms",
        median_ms(50, || {
            dst.copy_from_slice(&frame);
            std::hint::black_box(&dst);
        })
    );
    let bytes: &[u8] = bytemuck::cast_slice(&raw.data);
    let mut staged = vec![0u8; bytes.len()];
    println!(
        "RAW staging copy ({} MB): {:.3} ms",
        bytes.len() >> 20,
        median_ms(10, || {
            staged.copy_from_slice(bytes);
            std::hint::black_box(&staged);
        })
    );
    let mut lin = crate::testutil::raw_from_fn(6000, 4000, |x, _| [0.1 + x as f32 * 1e-4; 3]);
    lin.cfa = CfaPattern::LinearRgb;
    lin.data = (0..6000u32 * 4000 * 3)
        .map(|i| (i % 4000) as u16 + 300)
        .collect();
    let lp = plan::build(&lin, &adj, &RenderRequest::new(Size::new(2560, 2560))).unwrap();
    let need = crate::cpu::level_need(&lp, lp.level_size);
    let rect = crate::cache::build_rect(lp.base_key, need, lp.level_size);
    println!(
        "linear-DNG BASE on the CPU (24 Mpx, level {:?}): {:.3} ms",
        lp.level_size,
        median_ms(5, || {
            std::hint::black_box(crate::cpu::build_base(&lin, &lp, rect));
        })
    );
    // Memory held besides BASE levels after views of growing size and an export.
    if let Ok(mut g) = super::GpuRenderer::new() {
        use crate::Renderer;
        let mb = |b: u64| b as f64 / 1_048_576.0;
        for long in [2560u32, 3840, 5120] {
            let r = RenderRequest {
                capture: req.capture.clone(),
                ..RenderRequest::new(Size::new(long, long))
            };
            g.render(ImageKey(1), &raw, &busy, &r).unwrap();
            println!(
                "after a {long} px view: pool {:.0} MB, RAW uploads {:.0} MB, budget used {:.0} MB",
                mb(g.pool_bytes()),
                mb(g.raw_bytes()),
                mb(crate::cache::used_bytes())
            );
        }
        let full = RenderRequest {
            quality: crate::Quality::Full,
            capture: req.capture.clone(),
            ..RenderRequest::new(Size::new(8192, 8192))
        };
        g.render16(ImageKey(2), &raw, &busy, &full).unwrap();
        println!(
            "after a full-resolution export: pool {:.0} MB, budget used {:.0} MB",
            mb(g.pool_bytes()),
            mb(crate::cache::used_bytes())
        );
    }
}
