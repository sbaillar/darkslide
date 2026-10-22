//! Render budgets (ARCHITECTURE.md): GPU cold ≤ 80 ms, GPU warm ≤ 16 ms (every slider,
//! white balance included, at every view size), CPU ≤ 300 ms, full-res GPU export ≤ 1 s.
//! Real A9 ARW via `ds_raw::decode`; skips when `samples/` is absent.
#![allow(clippy::too_many_lines, clippy::field_reassign_with_default)]

#[path = "../tests/support/mod.rs"]
mod support;

use std::time::Duration;

use criterion::{
    BenchmarkGroup, Criterion, criterion_group, criterion_main, measurement::WallTime,
};
use ds_core::adjust::{
    BrushStroke, Layer, LayerAdjustments, Mask, MaskOp, MaskShape, Profile, WhiteBalance,
};
use ds_core::{Adjustments, NormRect, RawImage, Size};
use ds_pipeline::{
    ImageKey, OutputFormat, OutputSpace, Quality, RenderRequest, Renderer, cpu_renderer,
    gpu_renderer, output_size,
};

/// A heavy edit: HDR, clarity/structure, color editor, curves and two masked layers.
fn busy() -> Adjustments {
    let mut a = Adjustments::default();
    a.hdr.shadows = 40.0;
    a.hdr.highlights = 50.0;
    a.detail.clarity = 30.0;
    a.detail.structure = 20.0;
    a.curve.rgb = vec![[0.25, 0.22], [0.75, 0.8]];
    a.color_editor.basic[3].saturation = -30.0;
    a.vignette.amount_ev = -0.5;
    let layer = |mask: Mask| Layer {
        id: 1,
        name: "l".into(),
        enabled: true,
        opacity: 1.0,
        adjustments: LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 0.5,
                ..Default::default()
            },
            clarity: 20.0,
            ..Default::default()
        },
        mask,
    };
    a.layers = vec![
        layer(Mask {
            shapes: vec![MaskShape::Linear {
                op: MaskOp::Add,
                start: [0.5, 0.0],
                end: [0.5, 0.6],
            }],
            feather: 0.01,
            ..Mask::default()
        }),
        layer(Mask {
            shapes: vec![MaskShape::Radial {
                op: MaskOp::Add,
                center: [0.5, 0.5],
                radius: [0.2, 0.15],
                angle_deg: 0.0,
                feather: 0.5,
            }],
            ..Mask::default()
        }),
    ];
    a
}

fn stroke(n: usize, y: f32) -> BrushStroke {
    BrushStroke {
        points: (0..n)
            .map(|i| {
                [
                    0.1 + 0.8 * i as f32 / 400.0,
                    y + 0.05 * (i as f32 * 0.1).sin(),
                ]
            })
            .collect(),
        size: 0.05,
        hardness: 0.5,
        flow: 0.8,
        erase: false,
        auto_mask: false,
    }
}

fn brush_layer(id: u64, strokes: Vec<BrushStroke>) -> Layer {
    Layer {
        id,
        name: format!("brush {id}"),
        enabled: true,
        opacity: 1.0,
        adjustments: LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 0.7,
                ..Default::default()
            },
            ..Default::default()
        },
        mask: Mask {
            shapes: vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes,
                feather: 0.0,
            }],
            ..Mask::default()
        },
    }
}

fn group<'a>(c: &'a mut Criterion, name: &str) -> BenchmarkGroup<'a, WallTime> {
    let mut g = c.benchmark_group(name);
    g.sample_size(20).measurement_time(Duration::from_secs(6));
    g
}

fn bench_renderer(
    c: &mut Criterion,
    name: &str,
    r: &mut dyn Renderer,
    raw: &RawImage,
    capture: &ds_pipeline::CaptureInfo,
) {
    let mut req = RenderRequest::new(Size::new(2560, 2560));
    req.capture = capture.clone();
    let key = ImageKey(1);
    let mut g = group(c, name);
    for (label, mut adj) in [("", Adjustments::default()), ("_busy", busy())] {
        g.bench_function(format!("cold_2560{label}"), |b| {
            b.iter(|| {
                r.evict(key);
                r.render(key, raw, &adj, &req).expect("render")
            });
        });
        let mut i = 0u32;
        g.bench_function(format!("warm_2560{label}"), |b| {
            b.iter(|| {
                // A slider drag: exposure changes every frame, BASE stays cached.
                i += 1;
                adj.exposure.exposure_ev = (i % 7) as f32 * 0.1;
                r.render(key, raw, &adj, &req).expect("render")
            });
        });
    }
    g.finish();
}

fn bench_gpu_views(c: &mut Criterion, raw: &RawImage, capture: &ds_pipeline::CaptureInfo) {
    let Ok(mut r) = gpu_renderer() else { return };
    let key = ImageKey(2);
    let mut g = group(c, "gpu");
    for long in [2560u32, 3840, 5120] {
        let mut req = RenderRequest::new(Size::new(long, long));
        req.capture = capture.clone();
        let adj = Adjustments::default();
        g.bench_function(format!("cold_{long}"), |b| {
            b.iter(|| {
                r.evict(key);
                r.render(key, raw, &adj, &req).expect("render")
            });
        });
        // WB drags into a reused buffer (the app path): Preview (full resolution) and
        // Draft (developed at ≤ 2560 px and upscaled, for interactive drags).
        let mut out = vec![0u8; output_size(raw, &adj, &req).expect("size").pixels() as usize * 4];
        // The default is the Standard profile (colour map on); Neutral has no map; the
        // imported DCP adds a 3-D look table and a hue/sat map.
        let dcp = ds_pipeline::profile::register_dcp(&ds_pipeline::dcp::synth::example("Bench"))
            .expect("dcp")
            .profile();
        for (label, base) in [
            ("", Adjustments::default()),
            ("_busy", busy()),
            (
                "_neutral",
                Adjustments {
                    profile: Profile::Neutral,
                    ..Adjustments::default()
                },
            ),
            (
                "_dcp",
                Adjustments {
                    profile: dcp,
                    ..Adjustments::default()
                },
            ),
        ] {
            for (q, qn) in [
                (Quality::Preview, "wb_drag"),
                (Quality::Draft, "draft_wb_drag"),
            ] {
                let rq = RenderRequest {
                    quality: q,
                    ..req.clone()
                };
                let mut adj = base.clone();
                let mut i = 0u32;
                g.bench_function(format!("{qn}_{long}{label}"), |b| {
                    b.iter(|| {
                        i += 1;
                        adj.white_balance = WhiteBalance::Custom {
                            temp_k: 4000.0 + 50.0 * (i % 40) as f32,
                            tint: (i % 9) as f32,
                        };
                        r.render_into(key, raw, &adj, &rq, OutputFormat::Rgba8, &mut out)
                            .expect("render")
                    });
                });
            }
        }
    }
    // A 1:1 viewport (2560×1440 sensor pixels).
    let region = NormRect {
        x: 0.3,
        y: 0.3,
        w: 2560.0 / 4000.0,
        h: 1440.0 / 6000.0,
    };
    let mut req = RenderRequest {
        region: Some(region),
        ..RenderRequest::new(Size::new(2560, 2560))
    };
    req.capture = capture.clone();
    let mut adj = Adjustments::default();
    g.bench_function("region_1to1_2560x1440_cold", |b| {
        b.iter(|| {
            r.evict(key);
            r.render(key, raw, &adj, &req).expect("render")
        });
    });
    let mut i = 0u32;
    g.bench_function("region_1to1_2560x1440_wb_drag", |b| {
        b.iter(|| {
            i += 1;
            adj.white_balance = WhiteBalance::Custom {
                temp_k: 4000.0 + 50.0 * (i % 40) as f32,
                tint: 0.0,
            };
            r.render(key, raw, &adj, &req).expect("render")
        });
    });
    for long in [2560u32, 5120] {
        // Interactive brush work at 5120 uses Draft (see Quality::Draft).
        let quality = if long > 2560 {
            Quality::Draft
        } else {
            Quality::Preview
        };
        let req = RenderRequest {
            quality,
            capture: capture.clone(),
            ..RenderRequest::new(Size::new(long, long))
        };
        let mut out = vec![0u8; output_size(raw, &adj, &req).expect("size").pixels() as usize * 4];
        // Painting: one stroke growing by a point per frame (400 points max, then restart).
        let mut adj = Adjustments::default();
        let mut n = 1usize;
        g.bench_function(format!("brush_paint_{long}"), |b| {
            b.iter(|| {
                n = n % 400 + 1;
                adj.layers = vec![brush_layer(7, vec![stroke(n, 0.5)])];
                r.render_into(key, raw, &adj, &req, OutputFormat::Rgba8, &mut out)
                    .expect("render")
            });
        });
        // Three finished brush layers: exposure drag and crop drag.
        let mut adj = Adjustments::default();
        adj.layers = (0..3)
            .map(|k| brush_layer(10 + k, vec![stroke(400, 0.3 + 0.2 * k as f32)]))
            .collect();
        r.render_into(key, raw, &adj, &req, OutputFormat::Rgba8, &mut out)
            .expect("warm");
        let mut i = 0u32;
        g.bench_function(format!("brush3_exposure_drag_{long}"), |b| {
            b.iter(|| {
                i += 1;
                adj.exposure.exposure_ev = (i % 7) as f32 * 0.1;
                r.render_into(key, raw, &adj, &req, OutputFormat::Rgba8, &mut out)
                    .expect("render")
            });
        });
        g.bench_function(format!("brush3_crop_drag_{long}"), |b| {
            b.iter(|| {
                i += 1;
                let k = 0.02 * (i % 10) as f32;
                adj.crop.rect = NormRect {
                    x: k,
                    y: k,
                    w: 1.0 - 2.0 * k,
                    h: 1.0 - 2.0 * k,
                };
                r.render_into(key, raw, &adj, &req, OutputFormat::Rgba8, &mut out)
                    .expect("render")
            });
        });
    }
    // Opening a heavily brushed edit: 30 strokes of 100 points rasterized from scratch
    // (a new layer id each time), BASE warm.
    let req = RenderRequest {
        capture: capture.clone(),
        ..RenderRequest::new(Size::new(2560, 2560))
    };
    let strokes: Vec<BrushStroke> = (0..30)
        .map(|s| BrushStroke {
            points: (0..100)
                .map(|i| [0.05 + 0.9 * i as f32 / 100.0, 0.05 + 0.03 * s as f32])
                .collect(),
            size: 0.04,
            hardness: 0.3,
            flow: 0.8,
            erase: false,
            auto_mask: false,
        })
        .collect();
    let mut id = 1000u64;
    let mut adj = Adjustments::default();
    g.bench_function("brush_open_30_strokes_2560", |b| {
        b.iter(|| {
            id += 1;
            adj.layers = vec![brush_layer(id, strokes.clone())];
            r.render(key, raw, &adj, &req).expect("render")
        });
    });
    g.finish();
    let full = RenderRequest {
        quality: Quality::Full,
        capture: capture.clone(),
        ..RenderRequest::new(Size::new(8192, 8192))
    };
    let mut g = c.benchmark_group("gpu");
    g.sample_size(10).measurement_time(Duration::from_secs(8));
    g.bench_function("export_full_res_16bit", |b| {
        b.iter(|| {
            r.evict(ImageKey(3));
            r.render16(ImageKey(3), raw, &Adjustments::default(), &full)
                .expect("render")
        });
    });
    // A resized export (developed at full resolution, resized in linear light).
    let small = RenderRequest {
        max_size: Size::new(2048, 2048),
        ..full.clone()
    };
    g.bench_function("export_2048_16bit", |b| {
        b.iter(|| {
            r.evict(ImageKey(3));
            r.render16(ImageKey(3), raw, &Adjustments::default(), &small)
                .expect("render")
        });
    });
    g.finish();
}

/// Analysis tools: CPU reference vs GPU (histogram in the output pass, auto tools).
fn bench_tools(c: &mut Criterion, raw: &RawImage, capture: &ds_pipeline::CaptureInfo) {
    let mut g = group(c, "tools");
    let req = RenderRequest {
        capture: capture.clone(),
        ..RenderRequest::new(Size::new(2560, 2560))
    };
    let adj = Adjustments::default();
    let img = cpu_renderer()
        .render(ImageKey(1), raw, &adj, &req)
        .expect("render");
    g.bench_function("cpu_histogram_2560", |b| {
        b.iter(|| ds_pipeline::histogram(&img));
    });
    g.bench_function("cpu_auto_white_balance", |b| {
        b.iter(|| ds_pipeline::auto::auto_white_balance(raw));
    });
    g.bench_function("cpu_auto_levels", |b| {
        b.iter(|| ds_pipeline::auto::auto_levels(raw, &adj));
    });
    if let Ok(mut r) = gpu_renderer() {
        let key = ImageKey(4);
        let mut out = vec![0u8; output_size(raw, &adj, &req).expect("size").pixels() as usize * 4];
        let mut i = 0u32;
        for (name, hist) in [
            ("gpu_warm_2560", false),
            ("gpu_warm_2560_with_histogram", true),
        ] {
            let rq = RenderRequest {
                histogram: hist,
                ..req.clone()
            };
            let mut a = adj.clone();
            g.bench_function(name, |b| {
                b.iter(|| {
                    i += 1;
                    a.exposure.exposure_ev = (i % 7) as f32 * 0.1;
                    r.render_frame_into(key, raw, &a, &rq, OutputFormat::Rgba8, &mut out)
                        .expect("render")
                });
            });
        }
        // The image being edited: its RAW is already on the GPU.
        g.bench_function("gpu_auto_white_balance", |b| {
            b.iter(|| r.auto_white_balance(key, raw));
        });
        g.bench_function("gpu_auto_levels", |b| {
            b.iter(|| r.auto_levels(key, raw, &adj));
        });
    }
    g.finish();
}

/// The step-3 develop tools, each dragged at the 5K fit size (2832 px, Draft, the
/// viewer's drag path): identity (the new stages must cost nothing unused), vibrance,
/// texture, dehaze, a layer with the full tool set, an auto-masked brush, colour-range
/// refinement + feathered shapes, and heal/clone spots.
fn bench_develop_tools(c: &mut Criterion, raw: &RawImage, capture: &ds_pipeline::CaptureInfo) {
    use ds_core::adjust::{ColorRange, Presence, Spot, SpotKind};
    let Ok(mut r) = gpu_renderer() else { return };
    let key = ImageKey(9);
    let req = RenderRequest {
        capture: capture.clone(),
        quality: Quality::Draft,
        histogram: true,
        ..RenderRequest::new(Size::new(2832, 2832))
    };
    let mut out = vec![
        0u8;
        output_size(raw, &Adjustments::default(), &req)
            .expect("size")
            .pixels() as usize
            * 4
    ];
    let presence = |vibrance, texture, dehaze| {
        let mut a = Adjustments::default();
        a.presence = Presence {
            vibrance,
            texture,
            dehaze,
        };
        a
    };
    let full_layer = {
        let mut a = Adjustments::default();
        let mut l = brush_layer(1, vec![stroke(200, 0.5)]);
        let la = &mut l.adjustments;
        la.presence = Presence {
            vibrance: 30.0,
            texture: 20.0,
            dehaze: 15.0,
        };
        la.sharpening = -40.0;
        la.clarity = 20.0;
        la.hdr.shadows = 30.0;
        la.hdr.whites = 20.0;
        la.curve.luma = vec![[0.5, 0.55]];
        a.layers = vec![l];
        a
    };
    let auto_brush = {
        let mut a = Adjustments::default();
        let mut s = stroke(200, 0.4);
        s.auto_mask = true;
        s.size = 0.08;
        a.layers = vec![brush_layer(1, vec![s])];
        a
    };
    let ranges = {
        let mut a = Adjustments::default();
        let mut l = brush_layer(1, vec![stroke(200, 0.5)]);
        l.mask.color_range = Some(ColorRange {
            center_hue: 30.0,
            width: 60.0,
            ..ColorRange::default()
        });
        l.mask.shapes.push(MaskShape::ColorRange {
            op: MaskOp::Add,
            range: ColorRange {
                center_hue: 220.0,
                width: 60.0,
                ..ColorRange::default()
            },
            feather: 0.02,
        });
        a.layers = vec![l];
        a
    };
    let spots = {
        let mut a = Adjustments::default();
        a.spots = (0..12)
            .map(|i| Spot {
                kind: if i % 2 == 0 {
                    SpotKind::Heal
                } else {
                    SpotKind::Clone
                },
                center: [0.1 + 0.07 * i as f32, 0.3],
                radius: 0.015,
                source: [0.1 + 0.07 * i as f32, 0.36],
                feather: 0.5,
                opacity: 1.0,
            })
            .collect();
        a
    };
    let mut g = group(c, "develop_tools_gpu_draft_2832");
    for (name, base) in [
        ("identity", Adjustments::default()),
        ("vibrance", presence(40.0, 0.0, 0.0)),
        ("texture", presence(0.0, 40.0, 0.0)),
        ("dehaze", presence(0.0, 0.0, 40.0)),
        ("layer_full_set", full_layer),
        ("auto_mask_brush", auto_brush),
        ("colour_ranges_feathered", ranges),
        ("spots_12", spots),
    ] {
        let mut adj = base.clone();
        let mut i = 0u32;
        g.bench_function(format!("{name}_exposure_drag"), |b| {
            b.iter(|| {
                i += 1;
                adj.exposure.exposure_ev = (i % 7) as f32 * 0.1;
                r.render_into(key, raw, &adj, &req, OutputFormat::Rgba8, &mut out)
                    .expect("render")
            });
        });
    }
    g.finish();
}

/// Soft proofing on an SDR display: the viewer drags at the 5K fit size in 8-bit sRGB
/// normally, and in 16-bit float linear Rec.2020 (twice the bytes) while proofing.
fn bench_proof_drag(c: &mut Criterion, raw: &RawImage, capture: &ds_pipeline::CaptureInfo) {
    let Ok(mut r) = gpu_renderer() else { return };
    let key = ImageKey(11);
    let mut g = group(c, "proof_drag_gpu_draft_2832");
    for (name, format, output, bpp) in [
        ("sdr_rgba8_srgb", OutputFormat::Rgba8, OutputSpace::Srgb, 4),
        (
            "proof_f16_rec2020",
            OutputFormat::Rgba16fLinearP3,
            OutputSpace::Rec2020,
            8,
        ),
    ] {
        let req = RenderRequest {
            capture: capture.clone(),
            quality: Quality::Draft,
            histogram: true,
            output,
            ..RenderRequest::new(Size::new(2832, 2832))
        };
        let size = output_size(raw, &Adjustments::default(), &req).expect("size");
        let mut out = vec![0u8; size.pixels() as usize * bpp];
        let mut adj = Adjustments::default();
        let mut i = 0u32;
        g.bench_function(format!("{name}_exposure_drag"), |b| {
            b.iter(|| {
                i += 1;
                adj.exposure.exposure_ev = (i % 7) as f32 * 0.1;
                r.render_frame_into(key, raw, &adj, &req, format, &mut out)
                    .expect("render")
            });
        });
    }
    g.finish();
}

fn benches(c: &mut Criterion) {
    let Some(path) = ds_testdata::sample_arw() else {
        eprintln!("no samples/ — skipping render benches");
        return;
    };
    let raw = support::load_raw(&path).expect("decode");
    let capture = support::capture_info(&path);
    let mut cpu = cpu_renderer();
    bench_renderer(c, "cpu", &mut cpu, &raw, &capture);
    match gpu_renderer() {
        Ok(mut gpu) => bench_renderer(c, "gpu", &mut gpu, &raw, &capture),
        Err(e) => eprintln!("no GPU ({e}) — skipping GPU benches"),
    }
    bench_gpu_views(c, &raw, &capture);
    bench_tools(c, &raw, &capture);
    bench_develop_tools(c, &raw, &capture);
    bench_proof_drag(c, &raw, &capture);
}

criterion_group!(render, benches);
criterion_main!(render);
