//! GPU renderer: parity with the CPU reference (ΔE2000 mean ≤ 0.05, p99 ≤ 1.0), caching,
//! sharing, cancellation, output formats, error handling.
//! Every test skips when no GPU adapter is available.

#![allow(
    clippy::too_many_lines,
    clippy::field_reassign_with_default,
    clippy::float_cmp
)]

mod support;

use ds_core::adjust::{
    AiTarget, BrushStroke, ColorRange, Crop, Layer, LayerAdjustments, LumaRange, Mask, MaskOp,
    MaskShape, Wheel, WhiteBalance, WhiteBalanceDelta,
};
use ds_core::effects::{BlurMode, BokehShape};
use ds_core::{Adjustments, NormRect, Size};
use ds_pipeline::color::{delta_e2000, srgb8_to_lab};
use ds_pipeline::{
    CancelToken, CaptureInfo, GpuRenderer, ImageKey, MaskBitmap, MaskSource, OutputFormat,
    OutputSpace, PipelineError, Quality, RenderRequest, RenderedImage, Renderer, cpu_renderer,
    gpu_renderer, output_size,
};
use std::sync::Arc;

fn gpu() -> Option<GpuRenderer> {
    match gpu_renderer() {
        Ok(g) => Some(g),
        Err(ds_pipeline::PipelineError::NoAdapter) => {
            eprintln!("skipping GPU test: no adapter");
            None
        }
        // A shader that fails to compile must fail the suite, not skip it.
        Err(e) => panic!("GPU renderer: {e}"),
    }
}

/// A square bound of `n` px, through [`support::dim`] (a quarter under coverage, as the
/// synthetic RAWs it renders).
fn fit(n: u32) -> Size {
    let n = support::dim(n);
    Size::new(n, n)
}

/// A 3200×2400 synthetic RAW; under coverage 2048×1100, still above the 2 MP tiling
/// threshold of full-quality renders, in 2×2 tiles.
fn tiled_raw() -> ds_core::RawImage {
    support::synthetic_raw(
        support::dim_at_least(3200, 2048),
        support::dim_at_least(2400, 1100),
    )
}

/// A 4000×2600 synthetic RAW for drafts; under coverage 2600×1690, still past the
/// draft long edge (2048), so a [`draft_fit`] draft develops small and upscales, and a
/// 0.8-wide 1:1 region is larger than the draft size.
fn draft_raw() -> ds_core::RawImage {
    support::synthetic_raw(
        support::dim_at_least(4000, 2600),
        support::dim_at_least(2600, 1690),
    )
}

/// The view bound of draft tests: 3600 px, under coverage 2300 (past the draft edge).
fn draft_fit() -> Size {
    let n = support::dim_at_least(3600, 2300);
    Size::new(n, n)
}

/// (mean, p99, max) ΔE2000 between two renders.
fn delta_e(a: &RenderedImage, b: &RenderedImage) -> (f32, f32, f32) {
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
    (mean, d[d.len() * 99 / 100], d[d.len() - 1])
}

fn assert_parity(
    label: &str,
    raw: &ds_core::RawImage,
    adj: &Adjustments,
    req: &RenderRequest,
    g: &mut GpuRenderer,
) {
    let c = cpu_renderer()
        .render(ImageKey(1), raw, adj, req)
        .expect("cpu");
    let t = std::time::Instant::now();
    let x = g.render(ImageKey(1), raw, adj, req).expect("gpu");
    let dt = t.elapsed();
    let (mean, p99, max) = delta_e(&c, &x);
    println!(
        "{label}: {:?} ΔE2000 mean {mean:.3} p99 {p99:.3} max {max:.2} (gpu {dt:?})",
        c.size
    );
    assert!(
        mean <= 0.05 && p99 <= 1.0,
        "{label}: parity mean {mean} p99 {p99}"
    );
}

#[derive(Debug)]
struct Center;
impl MaskSource for Center {
    fn ai_mask(&self, _: AiTarget) -> Option<Arc<MaskBitmap>> {
        let (w, h) = (64u32, 48u32);
        let data = (0..w * h)
            .map(|i| {
                if (i % w) > w / 3 && (i % w) < 2 * w / 3 {
                    255
                } else {
                    0
                }
            })
            .collect();
        Some(Arc::new(MaskBitmap {
            size: Size::new(w, h),
            data,
            version: 0,
        }))
    }
}

fn busy_adjustments() -> Adjustments {
    let mut a = Adjustments::default();
    a.white_balance = WhiteBalance::Custom {
        temp_k: 5200.0,
        tint: 8.0,
    };
    a.exposure.exposure_ev = 0.4;
    a.exposure.contrast = 25.0;
    a.exposure.brightness = 10.0;
    a.exposure.saturation = 15.0;
    a.hdr.highlights = 60.0;
    a.hdr.shadows = 40.0;
    a.hdr.whites = 20.0;
    a.hdr.blacks = -10.0;
    a.levels.in_black = 0.02;
    a.levels.gamma = 1.1;
    a.curve.rgb = vec![[0.25, 0.22], [0.75, 0.8]];
    a.curve.luma = vec![[0.5, 0.55]];
    a.curve.blue = vec![[0.5, 0.48]];
    a.color_balance.shadows = Wheel {
        hue: 220.0,
        sat: 0.3,
        lum: 0.0,
    };
    a.color_balance.highlights = Wheel {
        hue: 40.0,
        sat: 0.2,
        lum: 0.05,
    };
    a.color_editor.basic[3].saturation = -40.0;
    a.color_editor.basic[5].hue_shift = 10.0;
    a.color_editor.advanced.push(ColorRange {
        center_hue: 25.0,
        width: 40.0,
        lightness: 20.0,
        ..ColorRange::default()
    });
    a.detail.clarity = 30.0;
    a.detail.structure = 20.0;
    a.detail.sharpen_amount = 200.0;
    a.noise.luminance = 40.0;
    a.noise.color = 60.0;
    a.vignette.amount_ev = -0.8;
    a.presence = ds_core::adjust::Presence {
        vibrance: 25.0,
        texture: 30.0,
        dehaze: 20.0,
    };
    a.crop = Crop {
        rect: NormRect {
            x: 0.05,
            y: 0.05,
            w: 0.9,
            h: 0.85,
        },
        angle_deg: 3.0,
        ..Crop::default()
    };
    let stroke = BrushStroke {
        points: vec![[0.2, 0.3], [0.5, 0.35], [0.7, 0.3]],
        size: 0.1,
        hardness: 0.3,
        flow: 0.8,
        erase: false,
        auto_mask: false,
    };
    a.layers = vec![
        Layer {
            id: 1,
            name: "grad".into(),
            enabled: true,
            opacity: 0.8,
            adjustments: LayerAdjustments {
                white_balance: Some(WhiteBalanceDelta {
                    temp_k: 800.0,
                    tint: -5.0,
                }),
                exposure: ds_core::adjust::Exposure {
                    exposure_ev: -0.7,
                    contrast: 20.0,
                    brightness: 0.0,
                    saturation: -20.0,
                },
                hdr: ds_core::adjust::Hdr {
                    highlights: 30.0,
                    whites: 40.0,
                    blacks: -30.0,
                    ..Default::default()
                },
                clarity: 25.0,
                presence: ds_core::adjust::Presence {
                    vibrance: -30.0,
                    texture: 40.0,
                    dehaze: 35.0,
                },
                sharpening: 60.0,
                ..LayerAdjustments::default()
            },
            mask: Mask {
                shapes: vec![
                    MaskShape::Linear {
                        op: MaskOp::Add,
                        start: [0.5, 0.0],
                        end: [0.5, 0.5],
                    },
                    MaskShape::Radial {
                        op: MaskOp::Subtract,
                        center: [0.3, 0.2],
                        radius: [0.1, 0.05],
                        angle_deg: 20.0,
                        feather: 0.5,
                    },
                ],
                feather: 0.02,
                luma_range: Some(LumaRange {
                    low: 0.1,
                    high: 0.9,
                    low_falloff: 0.1,
                    high_falloff: 0.1,
                }),
                ..Mask::default()
            },
        },
        Layer {
            id: 2,
            name: "brush+ai".into(),
            enabled: true,
            opacity: 1.0,
            adjustments: LayerAdjustments {
                curve: ds_core::adjust::Curve {
                    rgb: vec![[0.5, 0.6]],
                    luma: vec![[0.3, 0.25]],
                    ..Default::default()
                },
                color_balance: ds_core::adjust::ColorBalance {
                    midtones: Wheel {
                        hue: 120.0,
                        sat: 0.3,
                        lum: 0.0,
                    },
                    ..Default::default()
                },
                color_editor: vec![ColorRange {
                    center_hue: 240.0,
                    saturation: 30.0,
                    ..ColorRange::default()
                }],
                presence: ds_core::adjust::Presence {
                    vibrance: 40.0,
                    texture: -20.0,
                    dehaze: -30.0,
                },
                sharpening: -70.0,
                structure: 30.0,
                ..LayerAdjustments::default()
            },
            mask: Mask {
                shapes: vec![
                    MaskShape::Brush {
                        op: MaskOp::Add,
                        strokes: vec![stroke],
                        feather: 0.0,
                    },
                    MaskShape::Ai {
                        op: MaskOp::Add,
                        target: AiTarget::Subject,
                        feather: 0.0,
                        person: None,
                    },
                    MaskShape::ColorRange {
                        op: MaskOp::Intersect,
                        range: ColorRange {
                            center_hue: 0.0,
                            width: 200.0,
                            ..ColorRange::default()
                        },
                        feather: 0.0,
                    },
                    MaskShape::Fill { op: MaskOp::Add },
                    MaskShape::Linear {
                        op: MaskOp::Intersect,
                        start: [0.0, 0.5],
                        end: [1.0, 0.5],
                    },
                ],
                invert: true,
                feather: 0.001,
                ..Mask::default()
            },
        },
        Layer {
            id: 3,
            name: "radial".into(),
            enabled: true,
            opacity: 0.6,
            adjustments: LayerAdjustments {
                exposure: ds_core::adjust::Exposure {
                    brightness: 30.0,
                    ..Default::default()
                },
                ..LayerAdjustments::default()
            },
            mask: Mask {
                shapes: vec![MaskShape::Radial {
                    op: MaskOp::Add,
                    center: [0.6, 0.6],
                    radius: [0.2, 0.1],
                    angle_deg: -15.0,
                    feather: 0.3,
                }],
                ..Mask::default()
            },
        },
    ];
    a
}

#[test]
fn parity_default_synthetic() {
    let Some(mut g) = gpu() else { return };
    println!("adapter: {}", g.adapter_name());
    let raw = support::small_raw(1200, 800);
    assert_parity(
        "default",
        &raw,
        &Adjustments::default(),
        &RenderRequest::new(fit(1200)),
        &mut g,
    );
}

#[test]
fn parity_busy_synthetic() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(1000, 700);
    let mut req = RenderRequest::new(fit(800));
    req.masks = Some(Arc::new(Center));
    req.output = OutputSpace::DisplayP3;
    assert_parity("busy", &raw, &busy_adjustments(), &req, &mut g);
}

#[test]
fn parity_full_quality_and_16_bit() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(640, 480);
    let req = RenderRequest {
        quality: Quality::Full,
        ..RenderRequest::new(fit(640))
    };
    assert_parity("rcd", &raw, &Adjustments::default(), &req, &mut g);
    let c = cpu_renderer()
        .render16(ImageKey(2), &raw, &Adjustments::default(), &req)
        .unwrap();
    let x = g
        .render16(ImageKey(2), &raw, &Adjustments::default(), &req)
        .unwrap();
    assert_eq!(c.size, x.size);
    let mean: f64 = c
        .rgb16
        .iter()
        .zip(&x.rgb16)
        .map(|(a, b)| (f64::from(*a) - f64::from(*b)).abs())
        .sum::<f64>()
        / c.rgb16.len() as f64;
    assert!(mean < 100.0, "16-bit mean abs diff {mean}");
}

#[test]
fn parity_real_sample() {
    let Some(mut g) = gpu() else { return };
    let Some(raw) = support::first_sample() else {
        return;
    };
    assert_parity(
        "A9 default 2560",
        &raw,
        &Adjustments::default(),
        &RenderRequest::new(fit(2560)),
        &mut g,
    );
    let mut req = RenderRequest::new(fit(1600));
    req.masks = Some(Arc::new(Center));
    assert_parity("A9 busy 1600", &raw, &busy_adjustments(), &req, &mut g);
}

#[test]
fn parity_stage_toggles_and_pyramid() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(1000, 700);
    let mut luma_only = Adjustments::default();
    luma_only.noise.color = 0.0;
    luma_only.detail.sharpen_amount = 0.0;
    assert_parity(
        "nr luma only",
        &raw,
        &luma_only,
        &RenderRequest::new(fit(1000)),
        &mut g,
    );
    let mut chroma_only = Adjustments::default();
    chroma_only.noise.luminance = 0.0;
    chroma_only.layers = busy_adjustments().layers[2..].to_vec();
    assert_parity(
        "nr chroma only",
        &raw,
        &chroma_only,
        &RenderRequest::new(fit(1000)),
        &mut g,
    );
    // Thumbnail: two BASE pyramid levels below the superpixel.
    let req = RenderRequest {
        quality: Quality::Draft,
        ..RenderRequest::new(fit(120))
    };
    assert_parity("thumbnail", &raw, &Adjustments::default(), &req, &mut g);
    // Straightened crop + quarter turn + flip.
    let mut geo = Adjustments::default();
    geo.crop = Crop {
        rect: NormRect {
            x: 0.1,
            y: 0.2,
            w: 0.6,
            h: 0.5,
        },
        angle_deg: -7.0,
        quarter_turns: 1,
        flip_h: true,
        ..Crop::default()
    };
    assert_parity(
        "geometry",
        &raw,
        &geo,
        &RenderRequest::new(fit(600)),
        &mut g,
    );
}

#[test]
fn parity_non_finite_adjustments() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(400, 300);
    let mut a = busy_adjustments();
    a.levels.gamma = f32::NAN;
    a.color_balance.midtones = Wheel {
        hue: f32::NAN,
        sat: f32::INFINITY,
        lum: f32::NAN,
    };
    a.color_editor.basic[0].width = f32::NAN;
    a.exposure.contrast = f32::NAN;
    a.layers[0].mask.shapes.push(MaskShape::Linear {
        op: MaskOp::Subtract,
        start: [f32::NAN, 0.0],
        end: [1.0, 1.0],
    });
    a.layers[1].opacity = f32::NAN;
    let mut req = RenderRequest::new(fit(400));
    req.masks = Some(Arc::new(Center));
    assert_parity("non-finite", &raw, &a, &req, &mut g);
}

#[test]
fn warm_render_reuses_base() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(800, 600);
    let req = RenderRequest::new(fit(400));
    let mut adj = Adjustments::default();
    g.render(ImageKey(9), &raw, &adj, &req).unwrap();
    adj.exposure.exposure_ev = 1.0;
    g.render(ImageKey(9), &raw, &adj, &req).unwrap();
    assert_eq!(g.base_builds(), 1);
    // A WB drag stays warm: BASE is camera RGB at the reference WB.
    for t in [4000.0, 5000.0, 6500.0] {
        adj.white_balance = WhiteBalance::Custom {
            temp_k: t,
            tint: 0.0,
        };
        g.render(ImageKey(9), &raw, &adj, &req).unwrap();
    }
    assert_eq!(g.base_builds(), 1);
    assert_eq!(g.cached_bases(), 1);
    g.evict(ImageKey(9));
    assert_eq!(g.cached_bases(), 0);
    g.render(ImageKey(9), &raw, &adj, &req).unwrap();
    assert_eq!(g.base_builds(), 2);
}

#[test]
fn unchanged_tables_are_not_uploaded_again() {
    let Some(mut g) = gpu() else { return };
    let raw = support::synthetic_raw(320, 240);
    let req = RenderRequest::new(Size::new(160, 160));
    let mut adj = Adjustments::default();
    let first = g.render(ImageKey(31), &raw, &adj, &req).unwrap();
    let n = g.table_uploads();
    // Exposure isn't in the tables: the same buffer serves the next render.
    adj.exposure.exposure_ev = 0.5;
    g.render(ImageKey(31), &raw, &adj, &req).unwrap();
    assert_eq!(g.table_uploads(), n);
    // A curve change rebuilds them; going back uploads again and renders as before.
    adj.exposure.contrast = 40.0;
    g.render(ImageKey(31), &raw, &adj, &req).unwrap();
    assert_eq!(g.table_uploads(), n + 1);
    adj.exposure.contrast = 0.0;
    adj.exposure.exposure_ev = 0.0;
    assert_eq!(g.render(ImageKey(31), &raw, &adj, &req).unwrap(), first);
    assert_eq!(g.table_uploads(), n + 2);
}

#[test]
fn clones_share_one_device_and_cache_across_threads() {
    let Some(g) = gpu() else { return };
    let raw = Arc::new(support::synthetic_raw(640, 480));
    let req = RenderRequest::new(Size::new(320, 320));
    let mut first = g.clone();
    first
        .render(ImageKey(3), &raw, &Adjustments::default(), &req)
        .unwrap();
    let threads: Vec<_> = (0..4)
        .map(|i| {
            let mut r = g.clone();
            let raw = Arc::clone(&raw);
            let req = req.clone();
            std::thread::spawn(move || {
                let mut adj = Adjustments::default();
                adj.exposure.exposure_ev = i as f32 * 0.2;
                r.render(ImageKey(3), &raw, &adj, &req).unwrap().size
            })
        })
        .collect();
    for t in threads {
        assert_eq!(t.join().unwrap(), Size::new(320, 240));
    }
    assert_eq!(g.base_builds(), 1, "one BASE for all clones");
    assert!(format!("{g:?}").contains("GpuRenderer"));
    assert!(g.pool_bytes() > 0);
}

#[test]
fn cancellation_returns_cancelled_and_leaves_the_renderer_usable() {
    let Some(mut g) = gpu() else { return };
    let raw = support::synthetic_raw(640, 480);
    let token = CancelToken::new();
    token.cancel();
    let mut req = RenderRequest::new(Size::new(320, 320));
    req.cancel = Some(token);
    let e = g.render(ImageKey(4), &raw, &Adjustments::default(), &req);
    assert!(matches!(e, Err(PipelineError::Cancelled)), "{e:?}");
    let e = cpu_renderer().render(ImageKey(4), &raw, &Adjustments::default(), &req);
    assert!(matches!(e, Err(PipelineError::Cancelled)));
    // Cancel from another thread while a large render is in flight.
    let token = CancelToken::new();
    // Tiled (above 2 MP) even under coverage, so it can be cancelled between tiles.
    let big = support::synthetic_raw(
        support::dim_at_least(4000, 2048),
        support::dim_at_least(3000, 1536),
    );
    let full = RenderRequest {
        quality: Quality::Full,
        cancel: Some(token.clone()),
        ..RenderRequest::new(Size::new(4000, 4000))
    };
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2));
        token.cancel();
    });
    let r = g.render(ImageKey(5), &big, &Adjustments::default(), &full);
    t.join().unwrap();
    assert!(matches!(r, Ok(_) | Err(PipelineError::Cancelled)), "{r:?}");
    let ok = g.render(
        ImageKey(4),
        &raw,
        &Adjustments::default(),
        &RenderRequest::new(Size::new(320, 320)),
    );
    assert!(ok.is_ok());
}

#[test]
fn render_into_formats_match_the_cpu() {
    let Some(mut g) = gpu() else { return };
    let raw = support::synthetic_raw(600, 400);
    let adj = Adjustments::default();
    let req = RenderRequest::new(Size::new(300, 300));
    let size = output_size(&raw, &adj, &req).unwrap();
    assert_eq!(size, Size::new(300, 200));
    let n = size.pixels() as usize;
    let mut cpu = cpu_renderer();
    // Linear extended-range P3 half floats.
    let mut a = vec![0u8; n * 8];
    let mut b = vec![0u8; n * 8];
    cpu.render_into(
        ImageKey(1),
        &raw,
        &adj,
        &req,
        OutputFormat::Rgba16fLinearP3,
        &mut a,
    )
    .unwrap();
    g.render_into(
        ImageKey(1),
        &raw,
        &adj,
        &req,
        OutputFormat::Rgba16fLinearP3,
        &mut b,
    )
    .unwrap();
    let f = |v: &[u8]| -> Vec<f32> {
        bytemuck::cast_slice::<u8, u16>(v)
            .iter()
            .map(|h| half::f16::from_bits(*h).to_f32())
            .collect()
    };
    let (fa, fb) = (f(&a), f(&b));
    let max_diff = fa
        .iter()
        .zip(&fb)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max);
    assert!(max_diff < 0.02, "f16 max diff {max_diff}");
    assert!(
        fa.chunks(4)
            .all(|p| p[3] == 1.0 && p[..3].iter().all(|v| *v >= 0.0))
    );
    // Packed 16-bit RGB.
    let mut c16 = vec![0u8; n * 6];
    let mut g16 = vec![0u8; n * 6];
    cpu.render_into(ImageKey(1), &raw, &adj, &req, OutputFormat::Rgb16, &mut c16)
        .unwrap();
    g.render_into(ImageKey(1), &raw, &adj, &req, OutputFormat::Rgb16, &mut g16)
        .unwrap();
    let mean: f64 = bytemuck::cast_slice::<u8, u16>(&c16)
        .iter()
        .zip(bytemuck::cast_slice::<u8, u16>(&g16))
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).abs())
        .sum::<f64>()
        / (n * 3) as f64;
    assert!(mean < 100.0, "16-bit mean diff {mean}");
    // A too-small buffer is an error, not a panic.
    let mut small = vec![0u8; 10];
    let e = g.render_into(
        ImageKey(1),
        &raw,
        &adj,
        &req,
        OutputFormat::Rgba8,
        &mut small,
    );
    assert!(matches!(e, Err(PipelineError::InvalidRequest(_))));
    assert_eq!(OutputFormat::Rgb16.bytes_per_pixel(), 6);
}

/// RGB16 is packed on the GPU (6 bytes per pixel, rows padded to 4): odd widths come
/// out right on every output path (view, tiled full-size export, resized export,
/// region), matching the CPU; and 8-bit output equals the 16-bit output narrowed
/// (±1 LSB, ≥ 99.9% identical) so 8-bit encoders can request it directly.
#[test]
fn packed_rgb16_and_8_bit_outputs_on_every_path() {
    let Some(mut g) = gpu() else { return };
    // Odd sizes; above the 2 MP tiling threshold even under coverage.
    let raw = support::synthetic_raw(
        support::dim_at_least(2401, 1801),
        support::dim_at_least(1601, 1201),
    );
    let adj = busy_adjustments();
    let reqs = [
        ("view", RenderRequest::new(fit(1001))),
        (
            "tiled full size",
            RenderRequest {
                quality: Quality::Full,
                ..RenderRequest::new(Size::new(4000, 4000))
            },
        ),
        (
            "resized export",
            RenderRequest {
                quality: Quality::Full,
                output_sharpening: Some(ds_pipeline::OutputSharpening {
                    amount: 0.5,
                    radius: 1.0,
                }),
                ..RenderRequest::new(fit(1203))
            },
        ),
        (
            "region",
            RenderRequest {
                region: Some(NormRect {
                    x: 0.21,
                    y: 0.33,
                    w: 0.37,
                    h: 0.29,
                }),
                ..RenderRequest::new(Size::new(4000, 4000))
            },
        ),
    ];
    for (label, req) in reqs {
        let size = output_size(&raw, &adj, &req).unwrap();
        let n = size.pixels() as usize;
        let mut g16 = vec![0u8; n * 6];
        let mut c16 = vec![0u8; n * 6];
        let mut g8 = vec![0u8; n * 4];
        g.render_into(ImageKey(9), &raw, &adj, &req, OutputFormat::Rgb16, &mut g16)
            .unwrap();
        cpu_renderer()
            .render_into(ImageKey(9), &raw, &adj, &req, OutputFormat::Rgb16, &mut c16)
            .unwrap();
        g.render_into(ImageKey(9), &raw, &adj, &req, OutputFormat::Rgba8, &mut g8)
            .unwrap();
        let (g16, c16) = (
            bytemuck::cast_slice::<u8, u16>(&g16),
            bytemuck::cast_slice::<u8, u16>(&c16),
        );
        let mean = g16
            .iter()
            .zip(c16)
            .map(|(x, y)| (f64::from(*x) - f64::from(*y)).abs())
            .sum::<f64>()
            / (n * 3) as f64;
        // 8-bit output vs the 16-bit one narrowed.
        let (mut same, mut worst) = (0usize, 0i32);
        for i in 0..n {
            for c in 0..3 {
                let narrowed = ((u32::from(g16[i * 3 + c]) * 255 + 32_767) / 65_535) as i32;
                let d = (i32::from(g8[i * 4 + c]) - narrowed).abs();
                worst = worst.max(d);
                same += usize::from(d == 0);
            }
        }
        let share = same as f64 / (n * 3) as f64;
        println!(
            "{label} {size:?}: RGB16 GPU vs CPU mean {mean:.1}; 8-bit vs narrowed 16-bit max {worst}, {:.3}% identical",
            100.0 * share
        );
        assert!(
            size.width % 2 == 1 || label == "region",
            "{label}: odd width expected"
        );
        assert!(mean < 100.0, "{label}: RGB16 GPU vs CPU {mean}");
        assert!(
            worst <= 1 && share >= 0.999,
            "{label}: 8-bit {worst} {share}"
        );
    }
}

#[test]
fn oversized_buffers_and_device_loss_are_errors_not_panics() {
    let Some(mut g) = gpu() else { return };
    let raw = support::synthetic_raw(800, 600);
    let req = RenderRequest {
        quality: Quality::Full,
        ..RenderRequest::new(Size::new(800, 800))
    };
    g.set_max_buffer_bytes(Some(1 << 20));
    let e = g.render(ImageKey(6), &raw, &Adjustments::default(), &req);
    assert!(
        matches!(e, Err(PipelineError::Gpu(ref m)) if m.contains("buffer")),
        "{e:?}"
    );
    g.set_max_buffer_bytes(None);
    assert!(
        g.render(ImageKey(6), &raw, &Adjustments::default(), &req)
            .is_ok()
    );
    let rebuilds = g.device_rebuilds();
    g.simulate_device_loss();
    assert!(
        g.render(ImageKey(6), &raw, &Adjustments::default(), &req)
            .is_ok()
    );
    assert_eq!(g.cached_bases(), 1, "rebuilt after the loss");
    assert_eq!(g.device_rebuilds(), rebuilds + 1);
}

#[test]
fn region_renders_demosaic_only_their_rectangle() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(2000, 1400);
    let adj = Adjustments::default();
    let region = |x: f32| RenderRequest {
        region: Some(NormRect {
            x,
            y: 0.4,
            w: 0.2,
            h: 0.2,
        }),
        ..RenderRequest::new(Size::new(4000, 4000))
    };
    assert_parity("1:1 region", &raw, &adj, &region(0.4), &mut g);
    let builds = g.base_builds();
    // A small pan stays inside the cached rectangle's slack.
    g.render(ImageKey(1), &raw, &adj, &region(0.41)).unwrap();
    assert_eq!(g.base_builds(), builds);
    // A far pan rebuilds, replacing (not adding) the entry.
    g.render(ImageKey(1), &raw, &adj, &region(0.0)).unwrap();
    assert_eq!(g.base_builds(), builds + 1);
    assert_eq!(g.cached_bases(), 1);
}

#[test]
fn parity_lens_keystone_and_brush_painting() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(1200, 800);
    let mut adj = Adjustments::default();
    adj.lens.keystone_vertical = 6.0;
    adj.lens.keystone_horizontal = -3.0;
    adj.crop.angle_deg = 2.0;
    let mut req = RenderRequest::new(fit(900));
    req.capture = CaptureInfo {
        lens: Some("FE 24-70mm F2.8 GM".into()),
        focal_length: Some(24.0),
        aperture: Some(2.8),
        iso: Some(800),
        crop_factor: None,
        camera: None,
    };
    assert_parity("lens+keystone", &raw, &adj, &req, &mut g);
    // Paint a growing stroke: incremental rasterization, parity on the way.
    let mut stroke = BrushStroke {
        points: vec![[0.2, 0.5]],
        size: 0.08,
        hardness: 0.6,
        flow: 0.9,
        erase: false,
        auto_mask: false,
    };
    adj.layers = vec![Layer {
        id: 11,
        name: "paint".into(),
        enabled: true,
        opacity: 1.0,
        adjustments: LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 1.0,
                ..Default::default()
            },
            ..Default::default()
        },
        mask: Mask {
            shapes: vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![stroke.clone()],
                feather: 0.0,
            }],
            ..Mask::default()
        },
    }];
    for i in 1..20 {
        stroke
            .points
            .push([0.2 + 0.03 * i as f32, 0.5 + 0.01 * (i % 3) as f32]);
        adj.layers[0].mask.shapes[0] = MaskShape::Brush {
            op: MaskOp::Add,
            strokes: vec![stroke.clone()],
            feather: 0.0,
        };
        g.render(ImageKey(8), &raw, &adj, &req).unwrap();
    }
    assert_eq!(g.raster_misses(), 1, "strokes grow incrementally");
    assert_parity("brush", &raw, &adj, &req, &mut g);
    let painted = g.render(ImageKey(8), &raw, &adj, &req).unwrap();
    adj.layers.clear();
    let plain = g.render(ImageKey(8), &raw, &adj, &req).unwrap();
    let changed = painted
        .rgba8
        .iter()
        .zip(&plain.rgba8)
        .filter(|(a, b)| a != b)
        .count();
    assert!(
        changed > 1000,
        "the brush layer brightens its stroke ({changed} bytes changed)"
    );
}

/// Renders the four quadrants as 1:1 regions and compares each to the same pixels of a
/// full-frame render: local tone (HDR), clarity, NR and layers must not seam.
fn tiles_match_full(label: &str, r: &mut dyn Renderer) {
    tiles_match_full_with(label, r, &busy_adjustments());
}

fn tiles_match_full_with(label: &str, r: &mut dyn Renderer, adj: &Adjustments) {
    tiles_match_full_on(label, r, adj, &support::small_raw(1200, 800));
}

fn tiles_match_full_on(
    label: &str,
    r: &mut dyn Renderer,
    adj: &Adjustments,
    raw: &ds_core::RawImage,
) {
    let adj = adj.clone();
    let full_req = RenderRequest::new(Size::new(4000, 4000));
    let full = r.render(ImageKey(7), raw, &adj, &full_req).unwrap();
    let (fw, fh) = (full.size.width as usize, full.size.height as usize);
    let mut worst = (0.0f32, 0.0f32);
    for (x, y) in [(0.0, 0.0), (0.5, 0.0), (0.0, 0.5), (0.5, 0.5)] {
        let req = RenderRequest {
            region: Some(NormRect {
                x,
                y,
                w: 0.5,
                h: 0.5,
            }),
            ..RenderRequest::new(Size::new(4000, 4000))
        };
        let t = r.render(ImageKey(7), raw, &adj, &req).unwrap();
        let (tw, th) = (t.size.width as usize, t.size.height as usize);
        assert_eq!((tw * 2, th * 2), (fw, fh), "{label}");
        let (ox, oy) = ((x * fw as f32) as usize, (y * fh as f32) as usize);
        let crop: Vec<u8> = (0..th)
            .flat_map(|row| {
                let i = ((oy + row) * fw + ox) * 4;
                full.rgba8[i..i + tw * 4].iter().copied()
            })
            .collect();
        let reference = RenderedImage {
            size: t.size,
            rgba8: crop,
        };
        let (mean, p99, max) = delta_e(&reference, &t);
        println!("{label} tile ({x}, {y}): ΔE2000 mean {mean:.3} p99 {p99:.3} max {max:.2}");
        worst = (worst.0.max(mean), worst.1.max(p99));
    }
    assert!(
        worst.0 <= 0.05 && worst.1 <= 1.0,
        "{label}: tiles differ from the full render: {worst:?}"
    );
}

#[derive(Debug)]
struct Sky;
impl MaskSource for Sky {
    fn ai_mask(&self, t: AiTarget) -> Option<Arc<MaskBitmap>> {
        (t == AiTarget::Sky).then(|| {
            let (w, h) = (48u32, 32u32);
            Arc::new(MaskBitmap {
                size: Size::new(w, h),
                data: (0..w * h)
                    .map(|i| if i / w < h / 3 + (i % w) / 8 { 255 } else { 0 })
                    .collect(),
                version: 0,
            })
        })
    }
}

/// Layers using every new mask tool: colour-range refinement, per-shape feather
/// (brush, AI sky, colour range at small and large σ), and brushes mixing plain and
/// auto-masked strokes with an eraser.
fn mask_tool_adjustments() -> Adjustments {
    let layer = |id: u64, shapes: Vec<MaskShape>, color: Option<ColorRange>| Layer {
        id,
        name: format!("m{id}"),
        adjustments: LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 0.6 + 0.1 * id as f32,
                saturation: 20.0,
                ..Default::default()
            },
            ..LayerAdjustments::default()
        },
        mask: Mask {
            shapes,
            color_range: color,
            ..Mask::default()
        },
        ..Layer::default()
    };
    let stroke = |x: f32, size: f32, auto_mask: bool, erase: bool| BrushStroke {
        points: vec![[x, 0.1], [x + 0.05, 0.5], [x, 0.9]],
        size,
        hardness: 0.7,
        flow: 1.0,
        erase,
        auto_mask,
    };
    let mut a = Adjustments::default();
    a.layers = vec![
        layer(
            1,
            vec![MaskShape::Fill { op: MaskOp::Add }],
            Some(ColorRange {
                center_hue: 240.0,
                width: 90.0,
                ..ColorRange::default()
            }),
        ),
        layer(
            2,
            vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![stroke(0.2, 0.1, false, false)],
                feather: 0.02,
            }],
            None,
        ),
        layer(
            3,
            vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![
                    stroke(0.45, 0.12, true, false),
                    stroke(0.6, 0.06, false, false),
                    stroke(0.47, 0.03, false, true),
                ],
                feather: 0.004,
            }],
            None,
        ),
        layer(
            4,
            vec![MaskShape::Ai {
                op: MaskOp::Add,
                target: AiTarget::Sky,
                feather: 0.03,
                person: None,
            }],
            None,
        ),
        layer(
            5,
            vec![
                MaskShape::ColorRange {
                    op: MaskOp::Add,
                    range: ColorRange {
                        center_hue: 30.0,
                        width: 60.0,
                        ..ColorRange::default()
                    },
                    feather: 0.003,
                },
                MaskShape::ColorRange {
                    op: MaskOp::Add,
                    range: ColorRange {
                        center_hue: 120.0,
                        width: 60.0,
                        ..ColorRange::default()
                    },
                    feather: 0.06,
                },
            ],
            None,
        ),
    ];
    a
}

/// Painting a feathered stroke point by point on the GPU (each step re-feathers only
/// the band of changed rows) matches the CPU at every step, and matches a fresh GPU
/// renderer that feathers the whole raster.
#[test]
fn incremental_feathered_painting_matches_a_full_reblur() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(1200, 800);
    let req = RenderRequest::new(fit(900));
    let mut a = Adjustments::default();
    let layer = |pts: Vec<[f32; 2]>, feather: f32| Layer {
        id: 5,
        adjustments: LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 1.0,
                ..Default::default()
            },
            ..LayerAdjustments::default()
        },
        mask: Mask {
            shapes: vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![BrushStroke {
                    points: pts,
                    size: 0.04,
                    hardness: 0.9,
                    ..BrushStroke::default()
                }],
                feather,
            }],
            ..Mask::default()
        },
        ..Layer::default()
    };
    for feather in [0.004f32, 0.03] {
        let mut pts = vec![[0.2, 0.3]];
        for step in 0..6 {
            pts.push([0.2 + 0.1 * step as f32, 0.3 + 0.08 * (step % 3) as f32]);
            a.layers = vec![layer(pts.clone(), feather)];
            assert_parity(
                &format!("feathered paint σ {feather} step {step}"),
                &raw,
                &a,
                &req,
                &mut g,
            );
        }
        let incremental = g.render(ImageKey(1), &raw, &a, &req).unwrap();
        let fresh = gpu().unwrap().render(ImageKey(1), &raw, &a, &req).unwrap();
        let worst = incremental
            .rgba8
            .iter()
            .zip(&fresh.rgba8)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap();
        assert!(worst <= 1, "incremental vs full re-feather: {worst}");
    }
}

/// A new photo's statistics come from the GPU path (dehaze light from GPU block means
/// of the uploaded mosaic, sensor noise measured during the upload): renders of two
/// separate copies of the raw (so each renderer measures its own) agree.
#[test]
fn per_image_statistics_measured_on_the_gpu_match_the_cpu() {
    let Some(mut g) = gpu() else { return };
    let mut a = Adjustments::default();
    a.presence.dehaze = 60.0;
    a.noise.color = 80.0;
    let req = RenderRequest {
        capture: CaptureInfo {
            iso: Some(3200),
            ..CaptureInfo::default()
        },
        ..RenderRequest::new(fit(900))
    };
    let (for_gpu, for_cpu) = (
        support::small_raw(1600, 1000),
        support::small_raw(1600, 1000),
    );
    let gi = g.render(ImageKey(77), &for_gpu, &a, &req).unwrap();
    let ci = cpu_renderer()
        .render(ImageKey(77), &for_cpu, &a, &req)
        .unwrap();
    let (mean, p99, _) = delta_e(&ci, &gi);
    println!("GPU-measured statistics vs CPU: ΔE2000 mean {mean:.3} p99 {p99:.3}");
    assert!(mean <= 0.05 && p99 <= 1.0, "{mean} {p99}");
}

#[test]
fn mask_tools_match_the_cpu() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(1200, 800);
    let req = RenderRequest {
        masks: Some(Arc::new(Sky)),
        ..RenderRequest::new(fit(900))
    };
    assert_parity("mask tools", &raw, &mask_tool_adjustments(), &req, &mut g);
    let full = RenderRequest {
        quality: Quality::Full,
        ..req.clone()
    };
    assert_parity(
        "mask tools, full",
        &raw,
        &mask_tool_adjustments(),
        &full,
        &mut g,
    );
}

#[test]
fn tiles_with_mask_tools_match_a_full_frame_render() {
    let adj = mask_tool_adjustments();
    tiles_match_full_with("cpu masks", &mut cpu_renderer(), &adj);
    if let Some(mut g) = gpu() {
        tiles_match_full_with("gpu masks", &mut g, &adj);
    }
}

/// The busy edit plus heal and clone spots: soft and hard, partial opacity, one in the
/// top-left quadrant whose source lies in the bottom-right one (tiles must build their
/// BASE over it), off-image sources, and 90 small ones.
fn spot_adjustments() -> Adjustments {
    use ds_core::adjust::{Spot, SpotKind};
    let mut a = busy_adjustments();
    a.spots = vec![
        Spot {
            kind: SpotKind::Heal,
            center: [0.3, 0.3],
            radius: 0.05,
            source: [0.7, 0.75],
            feather: 0.5,
            opacity: 1.0,
        },
        Spot {
            kind: SpotKind::Clone,
            center: [0.55, 0.5],
            radius: 0.03,
            source: [0.6, 0.62],
            feather: 0.0,
            opacity: 0.7,
        },
        Spot {
            kind: SpotKind::Heal,
            center: [0.5, 0.52],
            radius: 0.04,
            source: [0.42, 0.52],
            feather: 1.0,
            opacity: 1.0,
        },
        // Sources off the image (pinned inside it), and a ring partly off-image.
        Spot {
            kind: SpotKind::Heal,
            center: [0.97, 0.2],
            radius: 0.05,
            source: [1.2, 0.2],
            feather: 0.3,
            opacity: 1.0,
        },
        Spot {
            kind: SpotKind::Clone,
            center: [0.1, 0.9],
            radius: 0.04,
            source: [-0.5, 0.9],
            feather: 0.3,
            opacity: 1.0,
        },
    ];
    // Many small spots all over the frame (well past the old cap of 64).
    for i in 0..90u8 {
        let (x, y) = (
            0.05 + 0.1 * f32::from(i % 10),
            0.08 + 0.1 * f32::from(i / 10),
        );
        a.spots.push(Spot {
            kind: if i % 2 == 0 {
                SpotKind::Heal
            } else {
                SpotKind::Clone
            },
            center: [x, y],
            radius: 0.006,
            source: [x + 0.03, y],
            feather: 0.5,
            opacity: 1.0,
        });
    }
    a
}

#[test]
fn spots_match_the_cpu() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(1200, 800);
    let req = RenderRequest::new(fit(900));
    assert_parity("spots", &raw, &spot_adjustments(), &req, &mut g);
    let full = RenderRequest {
        quality: Quality::Full,
        ..req.clone()
    };
    assert_parity("spots, full", &raw, &spot_adjustments(), &full, &mut g);
    // With lens correction and keystone (spots sample through the geometry).
    let mut a = spot_adjustments();
    a.lens.keystone_vertical = 8.0;
    let lensed = RenderRequest {
        capture: CaptureInfo {
            lens: Some("FE 24-70mm F2.8 GM".into()),
            focal_length: Some(24.0),
            aperture: Some(4.0),
            ..CaptureInfo::default()
        },
        ..req
    };
    assert_parity("spots, lens + keystone", &raw, &a, &lensed, &mut g);
}

#[test]
fn tiles_with_spots_match_a_full_frame_render() {
    let adj = spot_adjustments();
    tiles_match_full_with("cpu spots", &mut cpu_renderer(), &adj);
    if let Some(mut g) = gpu() {
        tiles_match_full_with("gpu spots", &mut g, &adj);
    }
}

/// A wide colour-range, mask and auto-brush feather on a 4000 px frame: every tile
/// has the context the feather needs (feathers stop at `MAX_PLANE_FEATHER`), so
/// exports have no tile grid.
#[test]
fn tiles_with_wide_output_space_feathers_match_a_full_frame_render() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(4000, 2600);
    let mut a = Adjustments::default();
    a.layers = vec![Layer {
        id: 1,
        adjustments: LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 1.5,
                ..Default::default()
            },
            ..LayerAdjustments::default()
        },
        mask: Mask {
            shapes: vec![
                MaskShape::ColorRange {
                    op: MaskOp::Add,
                    range: ColorRange {
                        center_hue: 100.0,
                        width: 60.0,
                        ..ColorRange::default()
                    },
                    feather: 0.2,
                },
                MaskShape::Brush {
                    op: MaskOp::Add,
                    strokes: vec![BrushStroke {
                        points: vec![[0.1, 0.3], [0.9, 0.35]],
                        size: 0.2,
                        auto_mask: true,
                        ..BrushStroke::default()
                    }],
                    feather: 0.3,
                },
            ],
            feather: 0.25,
            ..Mask::default()
        },
        ..Layer::default()
    }];
    tiles_match_full_on("gpu wide feathers", &mut g, &a, &raw);
}

#[test]
fn tiles_match_a_full_frame_render() {
    tiles_match_full("cpu", &mut cpu_renderer());
    if let Some(mut g) = gpu() {
        tiles_match_full("gpu", &mut g);
    }
}

#[test]
fn parity_tiny_raws() {
    let Some(mut g) = gpu() else { return };
    for (w, h) in [(2, 2), (3, 3), (5, 4), (4, 5), (33, 17)] {
        let raw = support::synthetic_raw(w, h);
        for quality in [Quality::Preview, Quality::Full] {
            let req = RenderRequest {
                quality,
                ..RenderRequest::new(Size::new(512, 512))
            };
            let label = format!("raw {w}x{h} {quality:?}");
            assert_parity(&label, &raw, &Adjustments::default(), &req, &mut g);
        }
        // Each size is a different image under the same key.
        g.evict(ImageKey(1));
    }
}

#[test]
fn draft_drags_develop_small_and_upscale_on_the_same_base() {
    let Some(mut g) = gpu() else { return };
    let raw = draft_raw();
    let adj = busy_adjustments();
    let preview = RenderRequest::new(draft_fit());
    let draft = RenderRequest {
        quality: Quality::Draft,
        ..preview.clone()
    };
    let p = g.render(ImageKey(1), &raw, &adj, &preview).unwrap();
    let builds = g.base_builds();
    assert_parity("draft upscaled", &raw, &adj, &draft, &mut g);
    let d = g.render(ImageKey(1), &raw, &adj, &draft).unwrap();
    assert_eq!(d.size, p.size, "draft returns the requested size");
    assert_eq!(g.base_builds(), builds, "draft reuses the preview BASE");
    // Close to the full-resolution refine (softer, no sharpening).
    let (mean, _, _) = delta_e(&p, &d);
    assert!(mean < 2.5, "draft vs preview mean ΔE {mean}");
    // A 1:1 region larger than the draft size, too.
    let region = RenderRequest {
        region: Some(NormRect {
            x: 0.1,
            y: 0.1,
            w: 0.8,
            h: 0.7,
        }),
        quality: Quality::Draft,
        ..RenderRequest::new(Size::new(4000, 4000))
    };
    assert_parity("draft region", &raw, &adj, &region, &mut g);
}

#[test]
fn a_really_destroyed_device_is_an_error_then_recovers() {
    let Some(mut g) = gpu() else { return };
    let raw = support::synthetic_raw(800, 600);
    let adj = Adjustments::default();
    let req = RenderRequest::new(Size::new(400, 400));
    g.render(ImageKey(1), &raw, &adj, &req).unwrap();
    // Another clone is mid-render on another thread while the device goes away.
    let mut other = g.clone();
    let raw2 = raw.clone();
    let busy = std::thread::spawn(move || {
        let req = RenderRequest {
            quality: Quality::Full,
            ..RenderRequest::new(Size::new(800, 800))
        };
        for i in 0..20 {
            let _ = other.render(ImageKey(100 + i), &raw2, &busy_adjustments(), &req);
        }
    });
    g.destroy_device_for_tests();
    // Every later render returns (Err or, once rebuilt, Ok) instead of panicking, and the
    // renderer recovers.
    let mut ok = false;
    for _ in 0..5 {
        match g.render(ImageKey(1), &raw, &adj, &req) {
            Ok(img) => {
                assert_eq!(img.size, Size::new(400, 300));
                ok = true;
                break;
            }
            Err(PipelineError::Gpu(e)) => println!("after destroy: {e}"),
            Err(e) => panic!("unexpected {e}"),
        }
    }
    busy.join().expect("the other thread never panics");
    assert!(ok, "rebuilt after a real device loss");
    assert_parity("after device loss", &raw, &adj, &req, &mut g);
}

#[test]
fn memory_outside_bases_counts_against_the_budget() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(1600, 1200);
    g.render(
        ImageKey(1),
        &raw,
        &busy_adjustments(),
        &RenderRequest::new(fit(1600)),
    )
    .unwrap();
    let held = g.pool_bytes() + g.raw_bytes();
    assert!(held > 0);
    assert!(
        ds_pipeline::cache::used_bytes() >= held,
        "pool + raw uploads are counted: used {} held {held}",
        ds_pipeline::cache::used_bytes()
    );
}

#[test]
fn tiled_full_exports_match_the_cpu() {
    let Some(mut g) = gpu() else { return };
    let raw = tiled_raw();
    let req = RenderRequest {
        quality: Quality::Full,
        ..RenderRequest::new(Size::new(4000, 4000))
    };
    // Above the tiling threshold: several tiles, seamless.
    assert_parity("tiled export", &raw, &busy_adjustments(), &req, &mut g);
    let bg = RenderRequest {
        priority: ds_pipeline::Priority::Background,
        quality: Quality::Preview,
        ..req
    };
    assert_parity("tiled background", &raw, &busy_adjustments(), &bg, &mut g);
}

#[test]
fn gpu_histogram_matches_the_output_and_the_cpu() {
    let Some(mut g) = gpu() else { return };
    // Half size (not a quarter) under coverage: GPU-vs-CPU luma-bin differences sit on
    // edges, so their share of the pixels grows as the image shrinks (0.3 % at full
    // size, 0.6 % at half, 1.3 % at a quarter, against the 1 % bound).
    let raw = support::synthetic_raw(
        support::dim_at_least(1200, 600),
        support::dim_at_least(800, 400),
    );
    let adj = busy_adjustments();
    let preview = support::dim_at_least(900, 450);
    for (label, req) in [
        ("preview", RenderRequest::new(Size::new(preview, preview))),
        (
            "tiled export",
            RenderRequest {
                quality: Quality::Full,
                ..RenderRequest::new(Size::new(4000, 4000))
            },
        ),
    ] {
        let req = RenderRequest {
            histogram: true,
            ..req
        };
        let raw = if label == "preview" {
            raw.clone()
        } else {
            tiled_raw()
        };
        let size = output_size(&raw, &adj, &req).unwrap();
        let mut out = vec![0u8; size.pixels() as usize * 4];
        let f = g
            .render_frame_into(ImageKey(9), &raw, &adj, &req, OutputFormat::Rgba8, &mut out)
            .unwrap();
        let gh = f.histogram.expect("requested");
        // Exactly the histogram of the returned pixels.
        let img = RenderedImage { size, rgba8: out };
        assert_eq!(gh, ds_pipeline::histogram(&img), "{label}");
        assert_eq!(gh.total, size.pixels());
        // And close to the CPU reference's (parity-level pixel differences only).
        let mut cout = vec![0u8; size.pixels() as usize * 4];
        let ch = cpu_renderer()
            .render_frame_into(
                ImageKey(9),
                &raw,
                &adj,
                &req,
                OutputFormat::Rgba8,
                &mut cout,
            )
            .unwrap()
            .histogram
            .unwrap();
        let diff: u64 =
            gh.l.iter()
                .zip(ch.l.iter())
                .map(|(a, b)| u64::from(a.abs_diff(*b)))
                .sum();
        assert!(diff * 100 < gh.total, "{label}: luma bins differ by {diff}");
        g.evict(ImageKey(9));
    }
    // Other formats report 8-bit display codes; no histogram unless asked.
    let req = RenderRequest {
        histogram: true,
        ..RenderRequest::new(fit(600))
    };
    let size = output_size(&raw, &adj, &req).unwrap();
    let mut out = vec![0u8; size.pixels() as usize * 8];
    let f = g
        .render_frame_into(
            ImageKey(9),
            &raw,
            &adj,
            &req,
            OutputFormat::Rgba16fLinearP3,
            &mut out,
        )
        .unwrap();
    assert_eq!(f.histogram.unwrap().total, size.pixels());
    let f = g
        .render_frame_into(
            ImageKey(9),
            &raw,
            &adj,
            &RenderRequest::new(fit(600)),
            OutputFormat::Rgb16,
            &mut out,
        )
        .unwrap();
    assert!(f.histogram.is_none());
}

#[test]
fn gpu_auto_tools_match_the_cpu() {
    let Some(g) = gpu() else { return };
    let mut raws = vec![support::small_raw(1600, 1200)];
    // The 24 MP sample's CPU auto tools take minutes instrumented; the synthetic RAW
    // covers the same code there.
    if !cfg!(coverage) {
        raws.extend(support::first_sample());
    }
    for (i, raw) in raws.iter().enumerate() {
        let key = ImageKey(50 + i as u64);
        let (cw, gw) = (
            ds_pipeline::auto::auto_white_balance(raw),
            g.auto_white_balance(key, raw),
        );
        let (
            WhiteBalance::Custom {
                temp_k: ct,
                tint: ci,
            },
            WhiteBalance::Custom {
                temp_k: gt,
                tint: gi,
            },
        ) = (cw, gw)
        else {
            panic!("{cw:?} {gw:?}")
        };
        assert!(
            (ct - gt).abs() < 25.0 && (ci - gi).abs() < 1.0,
            "WB {cw:?} vs {gw:?}"
        );
        let adj = Adjustments::default();
        let (ce, ch, cl) = ds_pipeline::auto::auto_levels(raw, &adj);
        let (ge, gh, gl) = g.auto_levels(key, raw, &adj);
        assert!(
            (ce.exposure_ev - ge.exposure_ev).abs() <= 0.02,
            "{ce:?} {ge:?}"
        );
        assert!(
            (ch.highlights - gh.highlights).abs() <= 1.0 && (ch.shadows - gh.shadows).abs() <= 1.0
        );
        assert!(
            (cl.in_black - gl.in_black).abs() <= 2.0 / 255.0,
            "{cl:?} {gl:?}"
        );
        assert!(
            (cl.in_white - gl.in_white).abs() <= 2.0 / 255.0,
            "{cl:?} {gl:?}"
        );
    }
}

#[test]
fn export_resize_and_output_sharpening_match_the_cpu() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(3200, 2400);
    let adj = busy_adjustments();
    let sharp = Some(ds_pipeline::OutputSharpening {
        amount: 0.8,
        radius: 0.8,
    });
    // Downscaled export: developed at the level size (half), resized to 3/8.
    let export = RenderRequest {
        quality: Quality::Full,
        output_sharpening: sharp,
        ..RenderRequest::new(fit(1200))
    };
    assert_parity("export resized + sharpened", &raw, &adj, &export, &mut g);
    let plain = RenderRequest {
        output_sharpening: None,
        ..export.clone()
    };
    assert_parity("export resized", &raw, &adj, &plain, &mut g);
    // Resized vs rendered straight at the target size: the same picture (the direct
    // render is a bilinear sample of a half-size BASE, so fine stripes alias there).
    let straight = RenderRequest {
        quality: Quality::Preview,
        ..plain.clone()
    };
    let a = g.render(ImageKey(1), &raw, &adj, &plain).unwrap();
    let b = g.render(ImageKey(2), &raw, &adj, &straight).unwrap();
    assert_eq!(a.size, b.size);
    let (mean, _, _) = delta_e(&a, &b);
    assert!(mean < 3.0, "resized export vs direct render: {mean}");
    // Output sharpening at full size (no resize) and in 16 bit.
    let full = RenderRequest {
        quality: Quality::Full,
        output_sharpening: sharp,
        ..RenderRequest::new(fit(1000))
    };
    let small = support::small_raw(1000, 700);
    g.evict(ImageKey(1));
    assert_parity("output sharpening at size", &small, &adj, &full, &mut g);
    let c16 = cpu_renderer()
        .render16(ImageKey(3), &raw, &adj, &export)
        .unwrap();
    let g16 = g.render16(ImageKey(3), &raw, &adj, &export).unwrap();
    let worst = c16
        .rgb16
        .iter()
        .zip(&g16.rgb16)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap();
    assert!(worst < 2048, "16-bit export max diff {worst}");
}

#[test]
fn prefetch_builds_the_next_pan_rectangle_in_the_background() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(4000, 2600);
    let adj = Adjustments::default();
    let region = |x: f32| RenderRequest {
        region: Some(NormRect {
            x,
            y: 0.4,
            w: 0.15,
            h: 0.15,
        }),
        ..RenderRequest::new(Size::new(4000, 4000))
    };
    g.render(ImageKey(1), &raw, &adj, &region(0.5)).unwrap();
    let b0 = g.base_builds();
    // Nothing to do while the view is covered.
    assert!(!g.prefetch(ImageKey(1), &raw, &adj, &region(0.51)).unwrap());
    // Ahead of a pan past the slack: built in the background ...
    assert!(g.prefetch(ImageKey(1), &raw, &adj, &region(0.3)).unwrap());
    assert_eq!(g.base_builds(), b0 + 1);
    // ... so neither the new view nor the old one rebuilds synchronously.
    g.render(ImageKey(1), &raw, &adj, &region(0.3)).unwrap();
    g.render(ImageKey(1), &raw, &adj, &region(0.5)).unwrap();
    assert_eq!(g.base_builds(), b0 + 1);
    assert_eq!(g.cached_bases(), 1);
    assert_parity("after prefetch", &raw, &adj, &region(0.3), &mut g);
}

/// A smooth linear-RGB ramp (ported from the review probe `q_band`).
fn ramp_raw() -> ds_core::RawImage {
    let (w, h) = (4096u32, 4u32);
    let mut raw = support::synthetic_raw(4, 4);
    raw.size = Size::new(w, h);
    raw.cfa = ds_core::CfaPattern::LinearRgb;
    raw.black_level = [0.0; 4];
    raw.white_level = 65535.0;
    raw.default_crop = None;
    raw.data = (0..w * h * 3)
        .map(|i| {
            let x = (i / 3) % w;
            let v = 0.10 + 0.04 * x as f32 / (w - 1) as f32;
            (v * [0.6f32, 0.8, 1.0][(i % 3) as usize] * 65535.0).round() as u16
        })
        .collect();
    raw
}

#[test]
fn smooth_ramps_keep_their_levels_on_the_gpu() {
    let Some(mut g) = gpu() else { return };
    let raw = ramp_raw();
    let mut a = Adjustments::default();
    a.detail.sharpen_amount = 0.0;
    a.noise.luminance = 0.0;
    a.noise.color = 0.0;
    a.exposure.contrast = 60.0;
    // 16-bit export (M3): as many distinct levels as the CPU.
    let req = RenderRequest {
        quality: Quality::Full,
        ..RenderRequest::new(Size::new(8192, 8192))
    };
    let levels = |v: &[u16]| {
        let mut row: Vec<u16> = v.iter().step_by(3).skip(1).take(4096).copied().collect();
        row.dedup();
        row.len()
    };
    let c = levels(
        &cpu_renderer()
            .render16(ImageKey(1), &raw, &a, &req)
            .unwrap()
            .rgb16,
    );
    let gl = levels(&g.render16(ImageKey(1), &raw, &a, &req).unwrap().rgb16);
    println!("16-bit ramp: CPU {c} levels, GPU {gl}");
    assert!(gl * 100 >= c * 97, "GPU {gl} vs CPU {c} levels");
    // Extended-range preview (L2): an f32 BASE, so no f16 steps.
    let req = RenderRequest::new(Size::new(8192, 8192));
    let size = output_size(&raw, &a, &req).unwrap();
    let mut out = vec![0u8; size.pixels() as usize * 8];
    g.render_into(
        ImageKey(2),
        &raw,
        &a,
        &req,
        OutputFormat::Rgba16fLinearP3,
        &mut out,
    )
    .unwrap();
    let green: Vec<f32> = out
        .as_chunks::<8>()
        .0
        .iter()
        .take(4096)
        .map(|p| half::f16::from_ne_bytes([p[2], p[3]]).to_f32())
        .collect();
    let max_step = green
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    println!("EDR ramp: max step {max_step:.5} linear");
    assert!(max_step < 0.0015, "EDR ramp steps {max_step}");
}

#[test]
fn unknown_lenses_are_a_true_no_op() {
    let raw = support::small_raw(800, 600);
    let adj = Adjustments::default();
    let mut off = adj.clone();
    off.lens.enabled = false;
    let mut r = cpu_renderer();
    let base = r
        .render(ImageKey(1), &raw, &off, &RenderRequest::new(fit(800)))
        .unwrap();
    // Lenses lensfun has no calibration for (the database covers many Sigma and Tamron
    // lenses since N11).
    for lens in [
        "Sigma 150-600mm F5-6.3 DG DN OS | Sports 023",
        "Viltrox AF 27mm F1.2 Pro",
        "Samyang AF 35mm F1.4 FE",
        "",
    ] {
        let req = RenderRequest {
            capture: CaptureInfo {
                lens: Some(lens.into()),
                focal_length: Some(35.0),
                aperture: Some(4.0),
                ..CaptureInfo::default()
            },
            ..RenderRequest::new(fit(800))
        };
        let img = r.render(ImageKey(1), &raw, &adj, &req).unwrap();
        assert_eq!(img, base, "{lens}");
    }
}

/// A wrong `Ok` render in the device-loss storm: what differs and where (the region
/// tells the stage: a whole frame, one export tile or stripe, a band of the BASE), and
/// both images saved under the temp dir.
fn report_wrong(
    img: &RenderedImage,
    reference: &RenderedImage,
    thread: usize,
    iter: usize,
    key: ImageKey,
    gen0: u64,
    gen1: u64,
) {
    let kind = if thread < 2 { "export" } else { "view" };
    println!("WRONG on {:?}", std::thread::current().id());
    if img.size != reference.size {
        println!(
            "WRONG {kind} t{thread} i{iter} {key:?} gen {gen0}->{gen1}: size {:?} vs {:?}",
            img.size, reference.size
        );
        return;
    }
    let (w, h) = (img.size.width, img.size.height);
    let (mut x0, mut y0, mut x1, mut y1, mut n) = (w, h, 0, 0, 0u64);
    let mut rows = std::collections::BTreeMap::new();
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let (p, q) = (&img.rgba8[i..i + 3], &reference.rgba8[i..i + 3]);
            let d = delta_e2000(
                srgb8_to_lab([p[0], p[1], p[2]]),
                srgb8_to_lab([q[0], q[1], q[2]]),
            );
            if d > 2.0 {
                n += 1;
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                *rows.entry(y / 64).or_insert(0u32) += 1;
            }
        }
    }
    let (mean, p99, max) = delta_e(img, reference);
    let black = img
        .rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[..3] == [0, 0, 0])
        .count();
    println!(
        "WRONG {kind} t{thread} i{iter} {key:?} gen {gen0}->{gen1} {:?}: dE mean {mean:.3} p99 {p99:.2} max {max:.1}; {n} px > 2 in x {x0}..={x1} y {y0}..={y1}; black px {black}; bad rows/64: {rows:?}",
        img.size
    );
    let dir = std::env::temp_dir();
    for (name, im) in [("got", img), ("ref", reference)] {
        let path = dir.join(format!("ds-loss-wrong-{kind}-t{thread}-i{iter}-{name}.png"));
        support::write_png(&path, w, h, &im.rgba8);
        println!("  {}", path.display());
    }
}

/// Device losses while several threads render (viewer frames and tiled exports): every
/// `Ok` image is correct, and each loss costs exactly one rebuild (ported from the
/// review probe `q_loss3`).
#[test]
fn every_ok_render_is_correct_across_real_device_losses() {
    let Some(g) = gpu() else { return };
    // Full-size mosaics: the RAW staging copy (outside any lock) is long enough to
    // straddle losses.
    let raws = [
        support::synthetic_raw(6000, 4000),
        support::synthetic_raw(5000, 3400),
    ];
    let adj = busy_adjustments();
    let view = RenderRequest::new(Size::new(800, 800));
    let export = RenderRequest {
        quality: Quality::Full,
        priority: ds_pipeline::Priority::Background,
        ..RenderRequest::new(Size::new(4000, 4000))
    };
    let refs: Vec<(RenderedImage, RenderedImage)> = raws
        .iter()
        .map(|r| {
            let mut c = g.clone();
            let a = c.render(ImageKey(900), r, &adj, &view).unwrap();
            let b = c.render(ImageKey(901), r, &adj, &export).unwrap();
            c.evict(ImageKey(900));
            c.evict(ImageKey(901));
            (a, b)
        })
        .collect();
    let refs = Arc::new(refs);
    let raws = Arc::new(raws);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Shared count of Ok renders, so the post-storm phase can wait for real
    // recovery instead of assuming renders are fast (the instrumented coverage
    // build is ~50x slower and can see every render straddle a loss).
    let oks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Shared count of lost renders: the storm goes on until one render really straddled
    // a loss (instrumented builds are so slow that 16 losses could all land before any
    // render reached the GPU, so neither outcome was guaranteed).
    let errs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers: Vec<_> = (0..8)
        .map(|t| {
            let (mut c, raws, refs, stop) = (g.clone(), raws.clone(), refs.clone(), stop.clone());
            let (oks, errs) = (oks.clone(), errs.clone());
            let (adj, view, export) = (adj.clone(), view.clone(), export.clone());
            std::thread::spawn(move || {
                let (mut ok, mut err, mut wrong) = (0, 0, 0);
                let mut i = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    i += 1;
                    let k = (t + i) % 2;
                    let (req, reference) = if t < 2 {
                        (&export, &refs[k].1)
                    } else {
                        (&view, &refs[k].0)
                    };
                    let key = ImageKey(k as u64 + if t < 2 { 20 } else { 10 });
                    let gen0 = c.device_rebuilds();
                    match c.render(key, &raws[k], &adj, req) {
                        Ok(img) => {
                            ok += 1;
                            oks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if img.size != reference.size || delta_e(&img, reference).0 > 0.05 {
                                wrong += 1;
                                report_wrong(&img, reference, t, i, key, gen0, c.device_rebuilds());
                                // Persistent (cached state) or transient? Render again, same
                                // key, then after evicting it.
                                for (label, evict) in [("again", false), ("evicted", true)] {
                                    if evict {
                                        c.evict(key);
                                    }
                                    match c.render(key, &raws[k], &adj, req) {
                                        Ok(im2) => println!(
                                            "WRONG {label}: dE {:?}",
                                            delta_e(&im2, reference)
                                        ),
                                        Err(e) => println!("WRONG {label}: {e}"),
                                    }
                                }
                            }
                        }
                        Err(PipelineError::Gpu(_)) => {
                            err += 1;
                            errs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
                (ok, err, wrong)
            })
        })
        .collect();
    let g0 = g.device_rebuilds();
    // Start the storm once renders flow (bounded), and keep it going until a loss hit a
    // render in flight: at least 16 losses, at most 400 or two minutes.
    let wait = |cond: &dyn Fn() -> bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while !cond() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    wait(&|| oks.load(std::sync::atomic::Ordering::Relaxed) > 0);
    let storm_end = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut destroys = 0;
    let mut i = 0;
    while i < 16
        || (errs.load(std::sync::atomic::Ordering::Relaxed) == 0
            && i < 400
            && std::time::Instant::now() < storm_end)
    {
        std::thread::sleep(std::time::Duration::from_millis(25 + 23 * (i % 5)));
        g.destroy_device_for_tests();
        destroys += 1;
        i += 1;
    }
    // Keep rendering after the last loss until Ok renders resume (bounded).
    let after_storm = oks.load(std::sync::atomic::Ordering::Relaxed);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while oks.load(std::sync::atomic::Ordering::Relaxed) <= after_storm
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (mut ok, mut err, mut wrong) = (0, 0, 0);
    for w in workers {
        let (o, e, x) = w.join().expect("no panics");
        ok += o;
        err += e;
        wrong += x;
    }
    let rebuilds = g.device_rebuilds() - g0;
    println!("{destroys} losses: {ok} Ok ({wrong} wrong), {err} errors, {rebuilds} rebuilds");
    assert_eq!(wrong, 0, "an Ok render returned wrong pixels");
    assert!(ok > 0 && err > 0);
    assert!(
        rebuilds >= 1 && rebuilds <= destroys,
        "rebuild storm: {rebuilds} for {destroys}"
    );
    // Afterwards everything renders correctly again. The last loss can still be reported
    // to this render (Metal's lost callback is asynchronous and very late in the
    // instrumented build): that one fails, and the next one runs on the rebuilt device.
    let mut c = g.clone();
    let mut attempt = || c.render(ImageKey(10), &raws[0], &adj, &view);
    let img = match attempt() {
        Err(PipelineError::Gpu(e)) if e == "device lost" => attempt().unwrap(),
        r => r.unwrap(),
    };
    assert!(delta_e(&img, &refs[0].0).0 <= 0.05);
}

/// A loss that the next render first sees while uploading a new RAW (outside any GPU
/// turn, so before the device is polled): that render may fail, the one after it must
/// run on a rebuilt device. (The upload used to fail with "device lost" without flagging
/// the loss, so every later render failed the same way and nothing recovered.)
#[test]
fn a_loss_seen_while_uploading_a_raw_is_recovered_from() {
    let Some(g) = gpu() else { return };
    let raw = support::small_raw(1200, 800);
    let view = RenderRequest::new(fit(600));
    let mut c = g.clone();
    let reference = c
        .render(ImageKey(960), &raw, &Adjustments::default(), &view)
        .unwrap();
    g.destroy_device_for_tests();
    let mut last = None;
    for _ in 0..3 {
        last = Some(c.render(ImageKey(961), &raw, &Adjustments::default(), &view));
        if matches!(last, Some(Ok(_))) {
            break;
        }
    }
    let img = last.unwrap().expect("renders again after a loss");
    assert!(delta_e(&img, &reference).0 <= 0.05);
}

/// One real loss under 8 rendering threads: only the frames in flight fail, the device
/// is rebuilt exactly once, and every thread renders again (a destroyed device still
/// hands out invalid buffers, so the loss check must not be fooled by that).
#[test]
fn one_device_loss_under_load_costs_only_the_frames_in_flight() {
    let Some(g) = gpu() else { return };
    let raw = Arc::new(support::small_raw(3000, 2000));
    let view = RenderRequest::new(fit(1600));
    let reference = g
        .clone()
        .render(ImageKey(950), &raw, &Adjustments::default(), &view)
        .unwrap();
    let reference = Arc::new(reference);
    let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let workers: Vec<_> = (0..8u64)
        .map(|t| {
            let (mut c, raw, view, done, reference) = (
                g.clone(),
                raw.clone(),
                view.clone(),
                done.clone(),
                reference.clone(),
            );
            std::thread::spawn(move || {
                let (mut err, mut last_ok) = (0, false);
                for i in 0..40u64 {
                    let key = ImageKey(951 + (t + i) % 2);
                    let r = c.render(key, &raw, &Adjustments::default(), &view);
                    done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    match r {
                        Ok(img) => {
                            assert!(delta_e(&img, &reference).0 <= 0.05, "wrong pixels");
                            last_ok = true;
                        }
                        Err(PipelineError::Gpu(_)) => {
                            err += 1;
                            last_ok = false;
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
                (err, last_ok)
            })
        })
        .collect();
    let g0 = g.device_rebuilds();
    // Lose the device in steady state, with renders of every thread in flight.
    while done.load(std::sync::atomic::Ordering::SeqCst) < 16 {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    g.destroy_device_for_tests();
    let mut errors = 0;
    for w in workers {
        let (e, last_ok) = w.join().expect("no panics");
        errors += e;
        assert!(last_ok, "a thread still fails after the rebuild");
    }
    let rebuilds = g.device_rebuilds() - g0;
    println!("1 loss, 8 threads: {errors} failed frames, {rebuilds} rebuilds");
    assert_eq!(rebuilds, 1);
    assert!(errors <= 16, "{errors} failed frames for one loss");
}

/// Resize-quality chart (ported from the review probe `q_resize`): a zone plate on the
/// left; a bright square, 1-px lines and 3-px bars on the right. Linear RGB, 6000×4000
/// (a quarter, `CHART`× smaller, under coverage: the same frequencies, 1-px lines and
/// 3-px bars over a smaller field).
fn resize_chart() -> ds_core::RawImage {
    let (w, h) = (6000 / CHART, 4000 / CHART);
    let r0 = 1500.0 / f64::from(CHART);
    let wb = [2.4f32, 1.0, 1.6];
    let k = 0.5 / r0;
    let mut raw = support::synthetic_raw(4, 4);
    raw.size = Size::new(w, h);
    raw.cfa = ds_core::CfaPattern::LinearRgb;
    raw.black_level = [0.0; 4];
    raw.white_level = 65535.0;
    raw.wb_as_shot = wb;
    raw.default_crop = None;
    raw.data = vec![0u16; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let v: f64 = if x < w / 2 {
                let r2 = (f64::from(x) + 0.5 - r0).powi(2)
                    + (f64::from(y) + 0.5 - f64::from(h) / 2.0).powi(2);
                if r2.sqrt() <= r0 {
                    0.18 * (1.0 + 0.8 * (std::f64::consts::PI * k * r2).cos())
                } else {
                    0.18
                }
            } else {
                // In 6000×4000 chart coordinates (u, vv), but 1 px wide lines and 3 px bars.
                let (u, vv, c) = ((x - w / 2) * CHART, y * CHART, CHART);
                // A bright square, 1-px lines every 200 px, 3-px bars.
                let square = (500..1500).contains(&u) && (500..1500).contains(&vv);
                let lines = (2000..2400).contains(&vv) && (x - w / 2) % (200 / c) == 100 / c;
                let bars = (2800..3600).contains(&vv)
                    && ((x - w / 2) / 3) % 2 == 0
                    && (500..2500).contains(&u);
                if square || lines || bars { 0.9 } else { 0.01 }
            };
            for c in 0..3u32 {
                raw.data[((y * w + x) * 3 + c) as usize] =
                    (v / f64::from(wb[c as usize]) * 0.9 * 65535.0).round() as u16;
            }
        }
    }
    raw
}

fn srgb_dec(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn srgb_enc(v: f64) -> f64 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

fn lanczos3(x: f64) -> f64 {
    let x = x.abs();
    if x < 1e-9 {
        1.0
    } else if x < 3.0 {
        let p = std::f64::consts::PI * x;
        3.0 * p.sin() * (p / 3.0).sin() / (p * p)
    } else {
        0.0
    }
}

/// Reference downscale: Lanczos3, area-correct, separable, in linear light (RGB16 in and
/// out).
fn lanczos_resize(src: &[u16], w: usize, h: usize, tw: usize, th: usize) -> Vec<u16> {
    let lin: Vec<f64> = src
        .iter()
        .map(|v| srgb_dec(f64::from(*v) / 65535.0))
        .collect();
    let taps = |s: usize, d: usize, o: usize| -> (usize, Vec<f64>) {
        let ratio = s as f64 / d as f64;
        let fs = ratio.max(1.0);
        let c = (o as f64 + 0.5) * ratio;
        let lo = (c - 3.0 * fs).floor().max(0.0) as usize;
        let hi = ((c + 3.0 * fs).ceil() as usize).min(s);
        let mut wv: Vec<f64> = (lo..hi)
            .map(|i| lanczos3((i as f64 + 0.5 - c) / fs))
            .collect();
        let sum: f64 = wv.iter().sum();
        for v in &mut wv {
            *v /= sum;
        }
        (lo, wv)
    };
    let mut tmp = vec![0.0; tw * h * 3];
    for o in 0..tw {
        let (lo, wv) = taps(w, tw, o);
        for y in 0..h {
            for c in 0..3 {
                let a: f64 = wv
                    .iter()
                    .enumerate()
                    .map(|(k, q)| q * lin[(y * w + lo + k) * 3 + c])
                    .sum();
                tmp[(y * tw + o) * 3 + c] = a;
            }
        }
    }
    let mut out = vec![0u16; tw * th * 3];
    for o in 0..th {
        let (lo, wv) = taps(h, th, o);
        for x in 0..tw {
            for c in 0..3 {
                let a: f64 = wv
                    .iter()
                    .enumerate()
                    .map(|(k, q)| q * tmp[((lo + k) * tw + x) * 3 + c])
                    .sum();
                out[(o * tw + x) * 3 + c] = (srgb_enc(a) * 65535.0).round() as u16;
            }
        }
    }
    out
}

fn to_rgba8(img: &[u16], size: Size) -> RenderedImage {
    RenderedImage {
        size,
        rgba8: img
            .chunks(3)
            .flat_map(|p| {
                p.iter()
                    .map(|v| (f64::from(*v) / 257.0).round() as u8)
                    .chain([255])
                    .collect::<Vec<_>>()
            })
            .collect(),
    }
}

/// How much smaller the resize chart is: 4 under coverage (the Lanczos reference runs
/// instrumented), else 1.
const CHART: u32 = if cfg!(coverage) { 4 } else { 1 };

/// Exports smaller than the sensor match a proper downscale of the full-resolution
/// render (tone before averaging, no pyramid aliasing).
#[test]
fn resized_exports_match_a_downscaled_full_render() {
    let Some(mut g) = gpu() else { return };
    let raw = resize_chart();
    let adj = Adjustments::default();
    let full = RenderRequest {
        quality: Quality::Full,
        ..RenderRequest::new(Size::new(8192, 8192))
    };
    let full16 = g.render16(ImageKey(1), &raw, &adj, &full).unwrap();
    let exp = RenderRequest {
        quality: Quality::Full,
        ..RenderRequest::new(Size::new(1080 / CHART, 1080 / CHART))
    };
    let e16 = g.render16(ImageKey(1), &raw, &adj, &exp).unwrap();
    let (fw, fh) = (full16.size.width as usize, full16.size.height as usize);
    let (tw, th) = (e16.size.width as usize, e16.size.height as usize);
    let reference = lanczos_resize(&full16.rgb16, fw, fh, tw, th);
    let (mean, p99, _) = delta_e(
        &to_rgba8(&e16.rgb16, e16.size),
        &to_rgba8(&reference, e16.size),
    );
    // 3-px bars (period 6 source px, far above the output Nyquist): flat, and as bright
    // as the reference.
    let luma = |img: &[u16], x: usize, y: usize| {
        let p = &img[(y * tw + x) * 3..(y * tw + x) * 3 + 3];
        0.2126 * srgb_dec(f64::from(p[0]) / 65535.0)
            + 0.7152 * srgb_dec(f64::from(p[1]) / 65535.0)
            + 0.0722 * srgb_dec(f64::from(p[2]) / 65535.0)
    };
    let s = fw as f64 / tw as f64;
    // Chart coordinates (6000×4000) to output pixels.
    let o = |v: f64| (v / f64::from(CHART) / s) as usize;
    let bars = |img: &[u16]| {
        let (mut n, mut sm, mut sq) = (0.0, 0.0, 0.0);
        for y in o(2900.0)..o(3500.0) {
            for x in o(3600.0)..o(5400.0) {
                let v = luma(img, x, y);
                n += 1.0;
                sm += v;
                sq += v * v;
            }
        }
        let m = sm / n;
        (m, (sq / n - m * m).max(0.0).sqrt() / m)
    };
    // Zone plate between 0.12 and 0.45 cycles/px of the source: moiré energy.
    let zone = |img: &[u16]| {
        let (mut n, mut sm, mut sq) = (0.0, 0.0, 0.0);
        for y in 0..th {
            for x in 0..tw {
                let c = f64::from(CHART);
                let (sx, sy) = ((x as f64 + 0.5) * s, (y as f64 + 0.5) * s);
                let r = ((sx - 1500.0 / c).powi(2) + (sy - 2000.0 / c).powi(2)).sqrt();
                let f = r * 0.5 / (1500.0 / c);
                if (0.12..=0.45).contains(&f) && r < 1480.0 / c {
                    let v = luma(img, x, y);
                    n += 1.0;
                    sm += v;
                    sq += v * v;
                }
            }
        }
        let m = sm / n;
        (sq / n - m * m).max(0.0).sqrt() / m
    };
    let ((bm, bs), (rm, rs)) = (bars(&e16.rgb16), bars(&reference));
    let (ze, zr) = (zone(&e16.rgb16), zone(&reference));
    println!(
        "resized export vs Lanczos3 of the full render: ΔE mean {mean:.3} p99 {p99:.2}; 3-px bars mean {bm:.3} (ref {rm:.3}) rel.std {bs:.4} (ref {rs:.4}); zone-plate moiré {ze:.4} (ref {zr:.4})"
    );
    assert!(mean <= 0.3, "tone/texture vs reference: ΔE mean {mean}");
    assert!(
        (bm / rm - 1.0).abs() <= 0.03 && bs <= 0.03,
        "bars {bm} {bs} vs {rm}"
    );
    assert!(ze <= zr * 1.25 + 0.005, "zone-plate moiré {ze} vs {zr}");
}

/// Saturated flat patches (camera RGB, linear) on a grey field: deep blue water, sky,
/// red, green, cyan, magenta.
fn saturated_patches() -> ds_core::RawImage {
    const PATCHES: [[f32; 3]; 6] = [
        [0.010, 0.060, 0.420],
        [0.050, 0.160, 0.520],
        [0.450, 0.030, 0.020],
        [0.030, 0.350, 0.030],
        [0.020, 0.250, 0.300],
        [0.300, 0.020, 0.250],
    ];
    let (w, h) = (support::dim(2400), support::dim(1600));
    let mut raw = support::synthetic_raw(4, 4);
    raw.size = Size::new(w, h);
    raw.cfa = ds_core::CfaPattern::LinearRgb;
    raw.black_level = [0.0; 4];
    raw.white_level = 65535.0;
    raw.default_crop = None;
    let wb = raw.wb_as_shot;
    raw.data = vec![0u16; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let (col, row) = (x / (w / 3), y / (h / 2));
            let c = PATCHES[(row * 3 + col) as usize];
            for k in 0..3 {
                // Camera values before white balance (the pipeline multiplies by wb).
                let v = c[k] / wb[k].max(1e-3);
                raw.data[((y * w + x) * 3 + k as u32) as usize] =
                    (v.clamp(0.0, 1.0) * 65535.0) as u16;
            }
        }
    }
    raw
}

/// Mean colour (sRGB codes) of each patch's inner half, in `img`'s coordinates.
fn patch_means(img: &RenderedImage) -> Vec<[u8; 3]> {
    let (w, h) = (img.size.width as usize, img.size.height as usize);
    let (pw, ph) = (w / 3, h / 2);
    let mut out = Vec::new();
    for row in 0..2 {
        for col in 0..3 {
            let mut s = [0f64; 3];
            let mut n = 0.0;
            for y in row * ph + ph / 4..row * ph + 3 * ph / 4 {
                for x in col * pw + pw / 4..col * pw + 3 * pw / 4 {
                    let p = &img.rgba8[(y * w + x) * 4..(y * w + x) * 4 + 3];
                    for k in 0..3 {
                        s[k] += f64::from(p[k]);
                    }
                    n += 1.0;
                }
            }
            out.push(s.map(|v| (v / n).round() as u8));
        }
    }
    out
}

/// C1 regression: resized and output-sharpened exports show saturated colours as the
/// viewer does (the output gamut compression runs once, not again after the resize), on
/// both renderers and in both output spaces.
#[test]
fn resized_exports_keep_saturated_colours_of_the_viewer() {
    let raw = saturated_patches();
    let mut adj = Adjustments::default();
    adj.exposure.saturation = 30.0;
    adj.white_balance = WhiteBalance::Custom {
        temp_k: 4200.0,
        tint: 0.0,
    };
    let mut renderers: Vec<(&str, Box<dyn Renderer>)> = vec![("cpu", Box::new(cpu_renderer()))];
    if let Some(g) = gpu() {
        renderers.push(("gpu", Box::new(g)));
    }
    for (name, mut r) in renderers {
        for space in [OutputSpace::Srgb, OutputSpace::DisplayP3] {
            let view = RenderRequest {
                output: space,
                ..RenderRequest::new(fit(1200))
            };
            let viewer = r.render(ImageKey(70), &raw, &adj, &view).unwrap();
            for sharpen in [None, Some(0.6)] {
                let exp = RenderRequest {
                    quality: Quality::Full,
                    output: space,
                    output_sharpening: sharpen.map(|amount| ds_pipeline::OutputSharpening {
                        amount,
                        radius: 1.0,
                    }),
                    ..RenderRequest::new(fit(1200))
                };
                let export = r.render(ImageKey(71), &raw, &adj, &exp).unwrap();
                assert_eq!(export.size, viewer.size);
                let (a, b) = (patch_means(&viewer), patch_means(&export));
                let worst = a
                    .iter()
                    .zip(&b)
                    .map(|(p, q)| delta_e2000(srgb8_to_lab(*p), srgb8_to_lab(*q)))
                    .fold(0.0f32, f32::max);
                println!(
                    "{name} {space:?} sharpen {sharpen:?}: patch ΔE max {worst:.2} (viewer {a:?} export {b:?})"
                );
                assert!(worst <= 1.0, "{name} {space:?} {sharpen:?}: ΔE {worst}");
            }
        }
    }
}

/// `Fit::FillOriginalSize` (the default): a straightened, lens-corrected or keystoned
/// full frame keeps the sensor size and is scaled back up from the inscribed area in the
/// same resample, on both renderers. Framing is the same as `ConstrainToSensorPixels`
/// (only the output scale differs), so overlays and masks map identically.
#[test]
fn fill_original_size_keeps_the_sensor_size_and_matches_the_cpu() {
    let Some(mut g) = gpu() else { return };
    // 360×240 under coverage: at 300×200 the constrained size (274×182) is not exactly
    // 3:2, so filling to it rounds to 273 px.
    let (w, h) = (
        support::dim_at_least(1200, 360),
        support::dim_at_least(800, 240),
    );
    let raw = support::synthetic_raw(w, h);
    let mut adj = busy_adjustments();
    adj.crop = Crop {
        angle_deg: 4.0,
        ..Crop::default()
    };
    let fill = RenderRequest {
        quality: Quality::Full,
        ..RenderRequest::new(Size::new(1 << 16, 1 << 16))
    };
    let constrain = RenderRequest {
        fit: ds_pipeline::Fit::ConstrainToSensorPixels,
        ..fill.clone()
    };
    assert_eq!(fill.fit, ds_pipeline::Fit::FillOriginalSize, "the default");
    let (fs, fm) = ds_pipeline::frame_mapping(&raw, &adj, &fill).unwrap();
    let (cs, cm) = ds_pipeline::frame_mapping(&raw, &adj, &constrain).unwrap();
    assert_eq!(fs, Size::new(w, h));
    assert!(cs.width * 24 < w * 23 && cs.height * 80 < h * 77, "{cs:?}");
    for (a, b) in fm.iter().zip(&cm) {
        assert!((a - b).abs() < 1e-9, "same framing: {fm:?} vs {cm:?}");
    }
    assert_parity(
        "fill original size (straightened)",
        &raw,
        &adj,
        &fill,
        &mut g,
    );
    g.evict(ImageKey(1));
    assert_parity("constrain to sensor pixels", &raw, &adj, &constrain, &mut g);
    // Same picture: the constrained render and the filled one brought to its size.
    let a = g.render(ImageKey(2), &raw, &adj, &constrain).unwrap();
    let small = RenderRequest {
        max_size: cs,
        ..fill.clone()
    };
    let b = g.render(ImageKey(2), &raw, &adj, &small).unwrap();
    assert_eq!(a.size, b.size);
    let (mean, _, _) = delta_e(&a, &b);
    assert!(mean < 1.0, "fill vs constrain content: {mean}");
    // A quarter turn swaps the filled size.
    adj.crop.quarter_turns = 1;
    assert_eq!(output_size(&raw, &adj, &fill).unwrap(), Size::new(h, w));
}

/// Content-aware removals render the same on the GPU as on the CPU (both develop the RAW
/// with the fill composited in), under crop, straighten and a quarter turn, and a
/// one-person AI mask reaches the GPU through `ai_person_mask`.
#[test]
fn parity_removals_and_person_masks() {
    struct OnePerson;
    impl MaskSource for OnePerson {
        fn ai_mask(&self, _: AiTarget) -> Option<Arc<MaskBitmap>> {
            None
        }
        fn ai_person_mask(&self, t: AiTarget, p: Option<u32>) -> Option<Arc<MaskBitmap>> {
            (t == AiTarget::Hair && p == Some(1))
                .then(|| Center.ai_mask(t))
                .flatten()
        }
    }
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(900, 600);
    let mut adj = Adjustments::default();
    adj.removals.push(ds_core::adjust::Removal {
        strokes: vec![BrushStroke {
            points: vec![[0.3, 0.4], [0.45, 0.5]],
            size: 0.06,
            ..BrushStroke::default()
        }],
        feather: 0.004,
    });
    adj.crop = Crop {
        rect: NormRect {
            x: 0.1,
            y: 0.1,
            w: 0.8,
            h: 0.8,
        },
        angle_deg: 6.0,
        quarter_turns: 1,
        ..Crop::default()
    };
    adj.layers.push(Layer {
        adjustments: LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 1.0,
                ..Default::default()
            },
            ..Default::default()
        },
        mask: Mask {
            shapes: vec![MaskShape::Ai {
                op: MaskOp::Add,
                target: AiTarget::Hair,
                feather: 0.0,
                person: Some(1),
            }],
            ..Mask::default()
        },
        ..Layer::default()
    });
    let mut req = RenderRequest::new(fit(700));
    req.masks = Some(Arc::new(OnePerson));
    let c = cpu_renderer()
        .render(ImageKey(9101), &raw, &adj, &req)
        .unwrap();
    let x = g.render(ImageKey(9101), &raw, &adj, &req).unwrap();
    let (mean, p99, max) = delta_e(&c, &x);
    println!("removals + person mask: ΔE2000 mean {mean:.3} p99 {p99:.3} max {max:.2}");
    assert!(mean <= 0.05 && p99 <= 1.0, "parity mean {mean} p99 {p99}");
    // Both actually changed something (the fill and the masked layer).
    let mut plain = adj.clone();
    plain.removals.clear();
    plain.layers.clear();
    let p = g.render(ImageKey(9101), &raw, &plain, &req).unwrap();
    assert!(delta_e(&p, &x).0 > 0.2);
}

/// A depth map (far at the top, nearer toward the bottom, a near "subject" block left of
/// centre) and the subject's mask.
#[derive(Debug)]
struct Scene3d;
impl MaskSource for Scene3d {
    fn ai_mask(&self, t: AiTarget) -> Option<Arc<MaskBitmap>> {
        (t == AiTarget::Subject).then(|| {
            let (w, h) = (96u32, 64u32);
            Arc::new(MaskBitmap::new(
                Size::new(w, h),
                (0..w * h)
                    .map(|i| {
                        let (x, y) = (i % w, i / w);
                        if (20..44).contains(&x) && (18..50).contains(&y) {
                            255
                        } else {
                            0
                        }
                    })
                    .collect(),
            ))
        })
    }
    fn depth_map(&self) -> Option<Arc<MaskBitmap>> {
        let (w, h) = (192u32, 128u32);
        Some(Arc::new(MaskBitmap {
            size: Size::new(w, h),
            data: (0..w * h)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    if (40..88).contains(&x) && (36..100).contains(&y) {
                        240
                    } else {
                        (20 + y * 150 / h) as u8
                    }
                })
                .collect(),
            version: 11,
        }))
    }
}

fn effects_adjustments() -> Adjustments {
    let mut a = busy_adjustments();
    let fx = &mut a.effects;
    fx.lens_blur.amount = 70.0;
    fx.lens_blur.highlight_boost = 80.0;
    fx.glow.amount = 35.0;
    fx.glow.size = 60.0;
    fx.halation.amount = 50.0;
    fx.grain.amount = 40.0;
    fx.grain.size = 60.0;
    a
}

fn effects_request(n: u32) -> RenderRequest {
    let mut req = RenderRequest::new(Size::new(n, n));
    req.masks = Some(Arc::new(Scene3d));
    req
}

#[test]
fn effects_match_the_cpu() {
    let Some(mut g) = gpu() else { return };
    let raw = support::small_raw(1200, 800);
    let req = effects_request(support::dim(1200));
    assert_parity("effects", &raw, &effects_adjustments(), &req, &mut g);
    // Each aperture, a picked focus point, and the tilt-shift band.
    for (shape, mode) in [
        (BokehShape::Blades, BlurMode::Depth),
        (BokehShape::CatEye, BlurMode::Plane),
    ] {
        let mut a = effects_adjustments();
        a.effects.lens_blur.shape = shape.clone();
        a.effects.lens_blur.mode = mode.clone();
        a.effects.lens_blur.focus = Some([0.8, 0.2]);
        a.effects.lens_blur.amount = 100.0;
        assert_parity(&format!("{shape:?} {mode:?}"), &raw, &a, &req, &mut g);
    }
    // A plain edit with only the lens blur: the scene-linear path is taken for it.
    let mut only = Adjustments::default();
    only.effects.lens_blur.amount = 60.0;
    assert_parity("lens blur alone", &raw, &only, &req, &mut g);
    let mut grain = Adjustments::default();
    grain.effects.grain.amount = 100.0;
    grain.effects.grain.roughness = 100.0;
    assert_parity("grain alone", &raw, &grain, &req, &mut g);
}

#[test]
fn effects_in_drafts_tiles_and_exports_match_the_cpu() {
    let Some(mut g) = gpu() else { return };
    let adj = effects_adjustments();
    // A draft drag (developed small, upscaled, grain at the source position).
    let raw = draft_raw();
    let draft = RenderRequest {
        quality: Quality::Draft,
        ..effects_request(draft_fit().width)
    };
    assert_parity("effects draft", &raw, &adj, &draft, &mut g);
    // Another image under the same key.
    g.evict(ImageKey(1));
    // A downscaled export (developed in tiles at full resolution, resized).
    let raw = tiled_raw();
    let export = RenderRequest {
        quality: Quality::Full,
        ..effects_request(support::dim_at_least(1200, 768))
    };
    assert_parity("effects export", &raw, &adj, &export, &mut g);
}

#[test]
fn tiles_with_effects_match_a_full_frame_render() {
    let mut adj = effects_adjustments();
    adj.layers.clear();
    let raw = support::small_raw(1200, 800);
    let full_req = effects_request(4000);
    let check = |label: &str, r: &mut dyn Renderer| {
        let full = r.render(ImageKey(9), &raw, &adj, &full_req).unwrap();
        let fw = full.size.width as usize;
        for (x, y) in [(0.0, 0.0), (0.5, 0.0), (0.0, 0.5), (0.5, 0.5)] {
            let req = RenderRequest {
                region: Some(NormRect {
                    x,
                    y,
                    w: 0.5,
                    h: 0.5,
                }),
                ..full_req.clone()
            };
            let t = r.render(ImageKey(9), &raw, &adj, &req).unwrap();
            let (tw, th) = (t.size.width as usize, t.size.height as usize);
            let (ox, oy) = (
                (x * full.size.width as f32) as usize,
                (y * full.size.height as f32) as usize,
            );
            let crop: Vec<u8> = (0..th)
                .flat_map(|row| {
                    let i = ((oy + row) * fw + ox) * 4;
                    full.rgba8[i..i + tw * 4].iter().copied()
                })
                .collect();
            let (mean, p99, _) = delta_e(
                &RenderedImage {
                    size: t.size,
                    rgba8: crop,
                },
                &t,
            );
            assert!(
                mean <= 0.05 && p99 <= 1.0,
                "{label} tile ({x}, {y}): mean {mean} p99 {p99}"
            );
        }
    };
    check("cpu", &mut cpu_renderer());
    if let Some(mut g) = gpu() {
        check("gpu", &mut g);
    }
}

/// `OutputSpace::Rec2020` on the GPU (what the soft-proofing viewer and output-profile
/// exports use) matches the CPU reference, and keeps colours outside Display P3.
#[test]
fn rec2020_output_matches_the_cpu_and_leaves_p3() {
    let Some(mut g) = gpu() else { return };
    let raw = support::synthetic_raw(600, 400);
    let mut adj = Adjustments::default();
    adj.exposure.saturation = 100.0;
    let req = RenderRequest {
        output: OutputSpace::Rec2020,
        ..RenderRequest::new(Size::new(300, 300))
    };
    let n = output_size(&raw, &adj, &req).unwrap().pixels() as usize;
    let mut cpu = cpu_renderer();
    let mut both = |format: OutputFormat, bpp: usize| {
        let (mut a, mut b) = (vec![0u8; n * bpp], vec![0u8; n * bpp]);
        cpu.render_into(ImageKey(7), &raw, &adj, &req, format, &mut a)
            .unwrap();
        g.render_into(ImageKey(7), &raw, &adj, &req, format, &mut b)
            .unwrap();
        (a, b)
    };
    // Linear Rec.2020 half floats.
    let (a, b) = both(OutputFormat::Rgba16fLinearP3, 8);
    let f = |v: &[u8]| -> Vec<f32> {
        bytemuck::cast_slice::<u8, u16>(v)
            .iter()
            .map(|h| half::f16::from_bits(*h).to_f32())
            .collect()
    };
    let (fa, fb) = (f(&a), f(&b));
    let max_diff = fa
        .iter()
        .zip(&fb)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max);
    assert!(max_diff < 0.02, "f16 max diff {max_diff}");
    let to_p3 = ds_pipeline::color::working_to_output(OutputSpace::DisplayP3);
    let outside = fb
        .chunks(4)
        .filter(|p| {
            ds_pipeline::color::mat_vec(&to_p3, [p[0], p[1], p[2]])
                .iter()
                .any(|v| *v < -0.01)
        })
        .count();
    assert!(outside > 0, "the saturated patches leave P3 on the GPU");
    // Encoded 16-bit Rec.2020.
    let (c16, g16) = both(OutputFormat::Rgb16, 6);
    let mean: f64 = bytemuck::cast_slice::<u8, u16>(&c16)
        .iter()
        .zip(bytemuck::cast_slice::<u8, u16>(&g16))
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).abs())
        .sum::<f64>()
        / (n * 3) as f64;
    assert!(mean < 100.0, "16-bit mean diff {mean}");
}
