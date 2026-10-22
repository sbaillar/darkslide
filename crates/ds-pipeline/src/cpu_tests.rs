//! Behavioral tests of the CPU reference renderer on synthetic sensor data.
#![allow(clippy::field_reassign_with_default)]

use ds_core::adjust::{
    ColorRange, Crop, Layer, LayerAdjustments, Mask, MaskOp, MaskShape, WhiteBalance,
    WhiteBalanceDelta,
};
use ds_core::{Adjustments, NormRect, Orientation, Size};

use crate::cpu::{build_base, scene_linear};
use crate::plan::{self, DemosaicMode, base_key};
use crate::testutil::{gray_raw, noise, raw_from_fn};
use crate::{
    ImageKey, OutputSpace, PipelineError, Quality, RenderRequest, RenderedImage, Renderer,
    cpu_renderer,
};

fn req(n: u32) -> RenderRequest {
    RenderRequest::new(Size::new(n, n))
}

fn render(raw: &ds_core::RawImage, adj: &Adjustments, n: u32) -> RenderedImage {
    cpu_renderer()
        .render(ImageKey(1), raw, adj, &req(n))
        .expect("render")
}

fn px(img: &RenderedImage, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * img.size.width + x) * 4) as usize;
    [img.rgba8[i], img.rgba8[i + 1], img.rgba8[i + 2]]
}

fn mean_luma(img: &RenderedImage) -> f64 {
    let n = f64::from(img.size.width * img.size.height);
    img.rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]))
        .sum::<f64>()
        / n
}

fn std_luma(img: &RenderedImage) -> f64 {
    let m = mean_luma(img);
    let n = f64::from(img.size.width * img.size.height);
    (img.rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            (0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]) - m)
                .powi(2)
        })
        .sum::<f64>()
        / n)
        .sqrt()
}

#[test]
fn identity_keeps_neutral_gray_neutral() {
    for level in [0.02f32, 0.09, 0.18, 0.5] {
        let raw = gray_raw(64, 48, level);
        let img = render(&raw, &Adjustments::default(), 64);
        assert_eq!(img.size, Size::new(64, 48));
        for p in img.rgba8.as_chunks::<4>().0 {
            let (mx, mn) = (p[0].max(p[1]).max(p[2]), p[0].min(p[1]).min(p[2]));
            assert!(mx - mn <= 1, "level {level}: {p:?}");
            assert_eq!(p[3], 255);
        }
    }
    // Brighter scene → brighter output (monotone tone reproduction).
    let a = mean_luma(&render(
        &gray_raw(32, 32, 0.05),
        &Adjustments::default(),
        32,
    ));
    let b = mean_luma(&render(&gray_raw(32, 32, 0.1), &Adjustments::default(), 32));
    assert!(b > a);
}

#[test]
fn exposure_plus_one_doubles_linear_values_before_tone() {
    let raw = raw_from_fn(40, 30, |x, y| {
        [0.05 + 0.01 * (x % 7) as f32, 0.08 + 0.002 * y as f32, 0.06]
    });
    let mut adj = Adjustments::default();
    let p0 = plan::build(&raw, &adj, &req(40)).unwrap();
    let base = build_base(&raw, &p0, [0, 0, 20, 15]);
    let l0 = scene_linear(&base, &p0);
    adj.exposure.exposure_ev = 1.0;
    let p1 = plan::build(&raw, &adj, &req(40)).unwrap();
    assert_eq!(p0.base_key, p1.base_key, "exposure must not change BASE");
    let l1 = scene_linear(&base, &p1);
    for (a, b) in l0.iter().zip(&l1) {
        for c in 0..3 {
            assert!(
                (b[c] - 2.0 * a[c]).abs() <= 1e-5 * a[c].abs().max(1e-3),
                "{a:?} {b:?}"
            );
        }
    }
    // And the rendered result is brighter.
    assert!(
        mean_luma(&render(&raw, &adj, 40))
            > mean_luma(&render(&raw, &Adjustments::default(), 40)) + 10.0
    );
}

#[test]
fn develop_sliders_do_not_rebuild_base() {
    let raw = gray_raw(80, 60, 0.2);
    let mut r = cpu_renderer();
    let mut adj = Adjustments::default();
    r.render(ImageKey(7), &raw, &adj, &req(40)).unwrap();
    for ev in [0.5, 1.0, -1.0] {
        adj.exposure.exposure_ev = ev;
        adj.hdr.shadows = 30.0;
        adj.detail.clarity = 20.0;
        r.render(ImageKey(7), &raw, &adj, &req(40)).unwrap();
    }
    assert_eq!(r.base_builds(), 1);
    // White balance is applied in DEVELOP too: a WB drag never rebuilds BASE.
    for t in [3500.0, 5200.0, 8000.0] {
        adj.white_balance = WhiteBalance::Custom {
            temp_k: t,
            tint: 5.0,
        };
        r.render(ImageKey(7), &raw, &adj, &req(40)).unwrap();
    }
    assert_eq!(r.base_builds(), 1);
    assert_eq!(r.cached_bases(), 1);
    // Another image gets its own base; evict drops only that key.
    r.render(ImageKey(8), &raw, &adj, &req(40)).unwrap();
    assert_eq!(r.cached_bases(), 2);
    r.evict(ImageKey(7));
    assert_eq!(r.cached_bases(), 1);
    r.render(ImageKey(7), &raw, &adj, &req(40)).unwrap();
    assert_eq!(r.base_builds(), 3);
}

#[test]
fn base_key_selection() {
    // Views demosaic like the export (M1); only thumbnail-sized renders use superpixels.
    let k = base_key(Quality::Preview, 0.43);
    assert_eq!((k.mode, k.down, k.scale()), (DemosaicMode::Rcd, 1, 0.5));
    let k = base_key(Quality::Preview, 0.2);
    assert_eq!((k.mode, k.down), (DemosaicMode::Rcd, 2));
    let k = base_key(Quality::Preview, 0.1);
    assert_eq!(
        (k.mode, k.down, k.scale()),
        (DemosaicMode::Superpixel, 2, 0.125)
    );
    let k = base_key(Quality::Preview, 0.8);
    assert_eq!((k.mode, k.down), (DemosaicMode::Rcd, 0));
    // Draft shares Preview's BASE (a drag never triggers a rebuild).
    assert_eq!(
        base_key(Quality::Draft, 1.0),
        base_key(Quality::Preview, 1.0)
    );
    assert_eq!(
        base_key(Quality::Draft, 0.3),
        base_key(Quality::Preview, 0.3)
    );
    let k = base_key(Quality::Full, 0.3);
    assert_eq!((k.mode, k.down, k.scale()), (DemosaicMode::Rcd, 1, 0.5));
}

#[test]
fn quality_modes_and_pyramid_levels_render() {
    let raw = raw_from_fn(256, 192, |x, _| [0.02 + 0.3 * x as f32 / 256.0; 3]);
    let mut r = cpu_renderer();
    for (q, n) in [
        (Quality::Draft, 256),
        (Quality::Preview, 200),
        (Quality::Full, 256),
        (Quality::Full, 40),
        (Quality::Preview, 40),
    ] {
        let rq = RenderRequest {
            quality: q,
            ..req(n)
        };
        let img = r
            .render(ImageKey(1), &raw, &Adjustments::default(), &rq)
            .unwrap();
        // Left-to-right ramp stays monotone at every level.
        let row: Vec<u8> = (0..img.size.width)
            .map(|x| px(&img, x, img.size.height / 2)[1])
            .collect();
        assert!(
            row.windows(2).all(|w| w[1] + 1 >= w[0]),
            "{q:?} {n}: {row:?}"
        );
    }
}

#[test]
fn render16_matches_render8() {
    let raw = raw_from_fn(48, 32, |x, y| {
        [0.05 + 0.004 * x as f32, 0.1, 0.02 + 0.005 * y as f32]
    });
    let mut r = cpu_renderer();
    let a = r
        .render(ImageKey(1), &raw, &Adjustments::default(), &req(48))
        .unwrap();
    let b = r
        .render16(ImageKey(1), &raw, &Adjustments::default(), &req(48))
        .unwrap();
    assert_eq!(a.size, b.size);
    assert_eq!(b.rgb16.len(), 48 * 32 * 3);
    for (p8, p16) in a
        .rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.rgb16.as_chunks::<3>().0)
    {
        for c in 0..3 {
            let v = f32::from(p16[c]) / 257.0;
            assert!((v - f32::from(p8[c])).abs() <= 0.51, "{p8:?} {p16:?}");
        }
    }
}

#[test]
fn invalid_input_is_an_error() {
    let mut raw = gray_raw(8, 8, 0.2);
    raw.data.pop();
    let e = cpu_renderer().render(ImageKey(1), &raw, &Adjustments::default(), &req(8));
    assert!(matches!(e, Err(PipelineError::InvalidRaw(_))));
    let raw = gray_raw(8, 8, 0.2);
    let e = cpu_renderer().render(
        ImageKey(1),
        &raw,
        &Adjustments::default(),
        &RenderRequest::new(Size::new(0, 0)),
    );
    assert!(matches!(e, Err(PipelineError::InvalidRequest(_))));
    assert!(format!("{}", e.unwrap_err()).contains("invalid render request"));
}

#[test]
fn black_white_and_nan_inputs_stay_finite() {
    // Pure black sensor and fully clipped sensor.
    let black = gray_raw(16, 16, 0.0);
    let img = render(&black, &Adjustments::default(), 16);
    assert!(
        img.rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[0] < 3 && p[1] < 3 && p[2] < 3)
    );
    let white = gray_raw(16, 16, 5.0);
    let img = render(&white, &Adjustments::default(), 16);
    assert!(
        img.rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[0] > 240 && p[0] == p[1] && p[1] == p[2]),
        "{:?}",
        px(&img, 3, 3)
    );
    // Every slider NaN / extreme: no panic, output sane.
    let mut adj = Adjustments::default();
    adj.exposure.exposure_ev = f32::NAN;
    adj.exposure.contrast = f32::INFINITY;
    adj.exposure.saturation = f32::NAN;
    adj.hdr.shadows = f32::NAN;
    adj.levels.gamma = 0.0;
    adj.levels.in_white = adj.levels.in_black;
    adj.noise.luminance = f32::NAN;
    adj.detail.sharpen_radius = f32::NAN;
    adj.vignette.amount_ev = f32::NAN;
    let img = render(&gray_raw(16, 16, 0.18), &adj, 16);
    assert_eq!(img.rgba8.len(), 16 * 16 * 4);
    let mut adj = Adjustments::default();
    adj.exposure.exposure_ev = 5.0;
    adj.hdr.whites = 100.0;
    let img = render(&gray_raw(16, 16, 0.9), &adj, 16);
    assert!(px(&img, 5, 5).iter().all(|v| *v >= 250));
}

#[test]
fn crop_orientation_and_region_sizes() {
    let mut raw = raw_from_fn(60, 40, |x, _| if x < 30 { [0.4; 3] } else { [0.01; 3] });
    let mut adj = Adjustments::default();
    adj.crop = Crop {
        rect: NormRect {
            x: 0.0,
            y: 0.0,
            w: 0.5,
            h: 0.5,
        },
        ..Crop::default()
    };
    assert_eq!(render(&raw, &adj, 100).size, Size::new(30, 20));
    // EXIF Rotate90: the bright left half ends up on top.
    raw.orientation = Orientation::Rotate90;
    let img = render(&raw, &Adjustments::default(), 100);
    assert_eq!(img.size, Size::new(40, 60));
    assert!(px(&img, 20, 5)[1] > 150 && px(&img, 20, 55)[1] < 60);
    // Region: right half of the final frame, 1:1.
    raw.orientation = Orientation::Normal;
    let rq = RenderRequest {
        region: Some(NormRect {
            x: 0.5,
            y: 0.0,
            w: 0.5,
            h: 1.0,
        }),
        ..req(100)
    };
    let img = cpu_renderer()
        .render(ImageKey(1), &raw, &Adjustments::default(), &rq)
        .unwrap();
    assert_eq!(img.size, Size::new(30, 40));
    assert!(px(&img, 15, 20)[1] < 60);
}

#[test]
fn hdr_vignette_clarity_nr_and_sharpen_behave() {
    let scene = |x: u32, y: u32| {
        let v = if x < 64 { 0.01 } else { 0.9 };
        [v * (1.0 + 0.05 * noise(x, y)); 3]
    };
    let raw = raw_from_fn(128, 96, scene);
    let base = Adjustments::default();
    let at = |img: &RenderedImage, x| f64::from(px(img, x, 48)[1]);
    let plain = render(&raw, &base, 128);
    // Shadows lift the dark half, highlights darken the bright half.
    let mut adj = base.clone();
    adj.hdr.shadows = 80.0;
    adj.hdr.highlights = 80.0;
    let hdr = render(&raw, &adj, 128);
    assert!(at(&hdr, 20) > at(&plain, 20) + 5.0);
    assert!(at(&hdr, 110) < at(&plain, 110) - 5.0);
    // Vignette darkens the corners, not the center.
    let flat = gray_raw(64, 64, 0.18);
    let mut adj = base.clone();
    adj.vignette.amount_ev = -1.5;
    let v = render(&flat, &adj, 64);
    let f = render(&flat, &base, 64);
    assert!(px(&v, 1, 1)[1] + 20 < px(&f, 1, 1)[1]);
    assert_eq!(px(&v, 32, 32), px(&f, 32, 32));
    // Clarity and sharpening raise local contrast; NR lowers noise.
    let textured = raw_from_fn(96, 96, |x, y| [0.12 * (1.0 + 0.3 * noise(x / 3, y / 3)); 3]);
    let mut none = base.clone();
    none.noise.luminance = 0.0;
    none.noise.color = 0.0;
    none.detail.sharpen_amount = 0.0;
    let s0 = std_luma(&render(&textured, &none, 96));
    let mut clar = none.clone();
    clar.detail.clarity = 100.0;
    clar.detail.structure = 100.0;
    assert!(std_luma(&render(&textured, &clar, 96)) > s0 * 1.1);
    let mut sharp = none.clone();
    sharp.detail.sharpen_amount = 600.0;
    assert!(std_luma(&render(&textured, &sharp, 96)) > s0 * 1.05);
    let noisy = raw_from_fn(96, 96, |x, y| {
        [
            0.1 * (1.0 + 0.2 * noise(x, y)),
            0.1 * (1.0 + 0.2 * noise(y, x)),
            0.1,
        ]
    });
    let mut nr = none.clone();
    nr.noise.luminance = 100.0;
    nr.noise.color = 100.0;
    assert!(std_luma(&render(&noisy, &nr, 96)) < std_luma(&render(&noisy, &none, 96)) * 0.8);
}

/// Adjustments with NaN/Inf sprinkled through every tool (as a corrupt sidecar might carry).
pub(crate) fn poisoned() -> Adjustments {
    let mut adj = Adjustments::default();
    adj.levels.in_black = f32::NAN;
    adj.levels.gamma = f32::INFINITY;
    adj.color_balance.shadows = ds_core::adjust::Wheel {
        hue: f32::NAN,
        sat: f32::NAN,
        lum: f32::INFINITY,
    };
    adj.color_editor.basic[2].saturation = f32::NAN;
    adj.color_editor.advanced.push(ColorRange {
        center_hue: f32::NAN,
        width: f32::NAN,
        hue_shift: 10.0,
        ..ColorRange::default()
    });
    adj.hdr.whites = f32::NAN;
    adj.exposure.brightness = f32::NEG_INFINITY;
    adj.curve.rgb = vec![[f32::NAN, 0.5], [0.5, 0.6]];
    let bad_stroke = ds_core::adjust::BrushStroke {
        points: vec![[f32::NAN, 0.5], [0.5, f32::INFINITY]],
        size: 0.1,
        hardness: 0.5,
        flow: 1.0,
        erase: false,
        auto_mask: false,
    };
    adj.layers.push(layer(
        LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 1.0,
                ..Default::default()
            },
            ..Default::default()
        },
        Mask {
            shapes: vec![
                MaskShape::Fill { op: MaskOp::Add },
                MaskShape::Linear {
                    op: MaskOp::Subtract,
                    start: [f32::NAN, 0.0],
                    end: [1.0, 1.0],
                },
                MaskShape::Radial {
                    op: MaskOp::Intersect,
                    center: [0.5, 0.5],
                    radius: [f32::NAN, 0.1],
                    angle_deg: 0.0,
                    feather: 0.5,
                },
                MaskShape::ColorRange {
                    op: MaskOp::Intersect,
                    range: ColorRange {
                        width: f32::INFINITY,
                        ..ColorRange::default()
                    },
                    feather: 0.0,
                },
                MaskShape::Brush {
                    op: MaskOp::Add,
                    strokes: vec![bad_stroke],
                    feather: 0.0,
                },
            ],
            feather: f32::NAN,
            ..Mask::default()
        },
    ));
    adj.layers[0].opacity = f32::NAN;
    adj
}

#[test]
fn non_finite_adjustments_are_sanitized() {
    let raw = raw_from_fn(48, 32, |x, y| {
        [0.05 + 0.004 * x as f32, 0.1, 0.02 + 0.005 * y as f32]
    });
    let img = render(&raw, &poisoned(), 48);
    // Output is a real image: not blank, not saturated, varies across the frame.
    let m = mean_luma(&img);
    assert!(m > 20.0 && m < 235.0, "mean {m}");
    assert!(std_luma(&img) > 1.0);
}

/// Max deviation (8-bit codes) from the far plateaus beside a vertical step at `edge`,
/// ignoring `gap` pixels on each side of it.
fn halo(img: &RenderedImage, edge: u32, gap: u32) -> (i32, i32) {
    let y = img.size.height / 2;
    let w = img.size.width;
    let (dark, bright) = (
        i32::from(px(img, 10, y)[1]),
        i32::from(px(img, w - 10, y)[1]),
    );
    let dark_dev = (10..edge - gap)
        .map(|x| (i32::from(px(img, x, y)[1]) - dark).abs())
        .max()
        .unwrap();
    let bright_dev = (edge + gap..w - 10)
        .map(|x| (i32::from(px(img, x, y)[1]) - bright).abs())
        .max()
        .unwrap();
    (dark_dev, bright_dev)
}

#[test]
fn realistic_edges_have_no_halos_at_extreme_settings() {
    // A 2.5:1 luminance step (a ridge against sky) at realistic resolution, with every
    // local-contrast control at 100.
    let raw = raw_from_fn(1600, 40, |x, _| if x < 800 { [0.06; 3] } else { [0.15; 3] });
    let mut adj = Adjustments::default();
    adj.hdr.shadows = 100.0;
    adj.hdr.highlights = 100.0;
    adj.detail.clarity = 100.0;
    adj.detail.structure = 100.0;
    adj.detail.sharpen_amount = 0.0;
    adj.noise.luminance = 0.0;
    adj.noise.color = 0.0;
    let img = render(&raw, &adj, 1600);
    let (d, b) = halo(&img, 800, 4);
    let step = i32::from(px(&img, 1590, 20)[1]) - i32::from(px(&img, 10, 20)[1]);
    println!("halo: dark side {d}, bright side {b}, step {step}");
    assert!(
        d <= 4 && b <= 4,
        "halo: dark side {d}, bright side {b} codes (step {step})"
    );
}

#[test]
fn local_tone_has_no_halos() {
    // A hard dark/bright edge with maximal HDR + clarity: the edge-aware base layer must
    // not create over/undershoot bands next to it (a monotone step stays monotone).
    let raw = raw_from_fn(400, 60, |x, _| if x < 200 { [0.01; 3] } else { [0.8; 3] });
    let mut adj = Adjustments::default();
    adj.hdr.shadows = 100.0;
    adj.hdr.highlights = 100.0;
    adj.detail.clarity = 100.0;
    adj.detail.sharpen_amount = 0.0;
    adj.noise.luminance = 0.0;
    let img = render(&raw, &adj, 400);
    let row: Vec<i32> = (0..400).map(|x| i32::from(px(&img, x, 30)[1])).collect();
    let (dark_far, bright_far) = (row[20], row[380]);
    for (x, v) in row.iter().enumerate() {
        if x < 195 {
            assert!(
                (v - dark_far).abs() <= 3,
                "dark-side halo at {x}: {v} vs {dark_far}"
            );
        } else if x > 205 {
            assert!(
                (v - bright_far).abs() <= 3,
                "bright-side halo at {x}: {v} vs {bright_far}"
            );
        }
    }
}

#[test]
fn color_tools_move_colors() {
    let raw = raw_from_fn(32, 32, |_, _| [0.05, 0.25, 0.05]);
    let base = render(&raw, &Adjustments::default(), 32);
    let g = px(&base, 16, 16);
    assert!(g[1] > g[0] + 30, "green patch {g:?}");
    let mut adj = Adjustments::default();
    adj.color_editor.basic[3].saturation = -100.0;
    let d = px(&render(&raw, &adj, 32), 16, 16);
    assert!(
        i32::from(d[1]) - i32::from(d[0]) < (i32::from(g[1]) - i32::from(g[0])) / 2,
        "{d:?}"
    );
    let mut adj = Adjustments::default();
    adj.exposure.saturation = -100.0;
    let d = px(&render(&raw, &adj, 32), 16, 16);
    assert!(
        d[0].abs_diff(d[1]) <= 2 && d[1].abs_diff(d[2]) <= 2,
        "{d:?}"
    );
    // Display P3 keeps the same neutral axis but encodes saturated colors differently.
    let p3 = cpu_renderer()
        .render(
            ImageKey(1),
            &raw,
            &Adjustments::default(),
            &RenderRequest {
                output: OutputSpace::DisplayP3,
                ..req(32)
            },
        )
        .unwrap();
    assert_ne!(px(&p3, 16, 16), g);
    let gray = gray_raw(8, 8, 0.18);
    let a = render(&gray, &Adjustments::default(), 8);
    let b = cpu_renderer()
        .render(
            ImageKey(1),
            &gray,
            &Adjustments::default(),
            &RenderRequest {
                output: OutputSpace::DisplayP3,
                ..req(8)
            },
        )
        .unwrap();
    assert_eq!(px(&a, 4, 4), px(&b, 4, 4));
    // Curves, levels and color balance all act.
    let mut adj = Adjustments::default();
    adj.curve.luma = vec![[0.5, 0.7]];
    assert!(mean_luma(&render(&gray, &adj, 8)) > mean_luma(&a) + 10.0);
    let mut adj = Adjustments::default();
    adj.levels.out_white = 0.5;
    assert!(mean_luma(&render(&gray, &adj, 8)) < mean_luma(&a) - 10.0);
    let mut adj = Adjustments::default();
    adj.color_balance.midtones = ds_core::adjust::Wheel {
        hue: 240.0,
        sat: 1.0,
        lum: 0.0,
    };
    let c = px(&render(&gray, &adj, 8), 4, 4);
    assert!(c[2] > c[0] + 10, "{c:?}");
}

fn layer(adjustments: LayerAdjustments, mask: Mask) -> Layer {
    Layer {
        id: 1,
        name: "l".into(),
        enabled: true,
        opacity: 1.0,
        adjustments,
        mask,
    }
}

#[test]
fn layers_blend_by_mask_and_opacity() {
    let gray = gray_raw(40, 20, 0.18);
    let base = render(&gray, &Adjustments::default(), 40);
    let bright = LayerAdjustments {
        exposure: ds_core::adjust::Exposure {
            exposure_ev: 1.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let left_half = Mask {
        shapes: vec![MaskShape::Linear {
            op: MaskOp::Add,
            start: [0.49, 0.5],
            end: [0.5, 0.5],
        }],
        ..Mask::default()
    };
    let mut adj = Adjustments::default();
    adj.layers.push(layer(bright.clone(), left_half.clone()));
    let img = render(&gray, &adj, 40);
    assert!(px(&img, 5, 10)[1] > px(&base, 5, 10)[1] + 20);
    assert_eq!(px(&img, 35, 10), px(&base, 35, 10));
    // Half opacity lands in between; disabled / zero opacity layers do nothing.
    adj.layers[0].opacity = 0.5;
    let half = render(&gray, &adj, 40);
    assert!(px(&half, 5, 10)[1] > px(&base, 5, 10)[1] && px(&half, 5, 10)[1] < px(&img, 5, 10)[1]);
    adj.layers[0].enabled = false;
    assert_eq!(render(&gray, &adj, 40), base);
    // A lower temperature setting cools the image (as in the global WB tool); curves,
    // balance, editor, clarity and HDR all run through the layer path.
    let warm = LayerAdjustments {
        white_balance: Some(WhiteBalanceDelta {
            temp_k: -2000.0,
            tint: 0.0,
        }),
        hdr: ds_core::adjust::Hdr {
            shadows: 50.0,
            ..Default::default()
        },
        exposure: ds_core::adjust::Exposure {
            contrast: 30.0,
            brightness: 20.0,
            saturation: 20.0,
            ..Default::default()
        },
        curve: ds_core::adjust::Curve {
            rgb: vec![[0.5, 0.55]],
            luma: vec![[0.4, 0.45]],
            ..Default::default()
        },
        color_balance: ds_core::adjust::ColorBalance {
            highlights: ds_core::adjust::Wheel {
                hue: 30.0,
                sat: 0.5,
                lum: 0.0,
            },
            ..Default::default()
        },
        color_editor: vec![ColorRange {
            center_hue: 30.0,
            saturation: 50.0,
            ..ColorRange::default()
        }],
        clarity: 50.0,
        structure: 50.0,
        presence: ds_core::adjust::Presence::default(),
        sharpening: 0.0,
    };
    let fill = Mask {
        shapes: vec![MaskShape::Fill { op: MaskOp::Add }],
        ..Mask::default()
    };
    let mut adj = Adjustments::default();
    adj.layers.push(layer(warm, fill));
    let w = px(&render(&gray, &adj, 40), 20, 10);
    assert!(w[2] > w[0] + 5, "cooler layer {w:?}");
    adj.layers[0].adjustments.white_balance = Some(WhiteBalanceDelta {
        temp_k: 2000.0,
        tint: 0.0,
    });
    let w = px(&render(&gray, &adj, 40), 20, 10);
    assert!(w[0] > w[2] + 5, "warmer layer {w:?}");
}

#[test]
fn brush_rasters_are_cached_between_renders() {
    let gray = gray_raw(40, 30, 0.18);
    let stroke = ds_core::adjust::BrushStroke {
        points: vec![[0.2, 0.5], [0.8, 0.5]],
        size: 0.1,
        hardness: 0.5,
        flow: 1.0,
        erase: false,
        auto_mask: false,
    };
    let mut adj = Adjustments::default();
    adj.layers.push(layer(
        LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 1.0,
                ..Default::default()
            },
            ..Default::default()
        },
        Mask {
            shapes: vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![stroke],
                feather: 0.0,
            }],
            ..Mask::default()
        },
    ));
    let mut r = cpu_renderer();
    r.render(ImageKey(1), &gray, &adj, &req(40)).unwrap();
    adj.layers[0].adjustments.exposure.exposure_ev = 0.5;
    r.render(ImageKey(1), &gray, &adj, &req(40)).unwrap();
    assert_eq!(r.raster_misses(), 1);
    r.evict(ImageKey(1));
    r.render(ImageKey(1), &gray, &adj, &req(40)).unwrap();
    assert_eq!(r.raster_misses(), 2);
}

#[test]
fn noise_reduction_is_iso_adaptive() {
    assert_eq!(plan::iso_nr_factors(None), (1.0, 1.0));
    assert_eq!(plan::iso_nr_factors(Some(0)), (1.0, 1.0));
    assert_eq!(plan::iso_nr_factors(Some(100)), (0.0, 0.3));
    let near = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5;
    assert!(near(plan::iso_nr_factors(Some(800)), (0.6, 1.0)));
    assert!(near(plan::iso_nr_factors(Some(3200)), (1.0, 2.0)));
    assert!(near(plan::iso_nr_factors(Some(51200)), (1.4, 2.0)));
    // The chroma filter's noise estimate: none for a clean image, full strength (∝ the
    // noise) from CHROMA_NOISE_FULL, faded below.
    assert_eq!(plan::chroma_noise(0.0), 0.0);
    let full = plan::CHROMA_NOISE_FULL;
    assert!(
        (plan::chroma_noise(2.0 * full) - 2.0 * plan::CHROMA_NOISE_PER_RAW * full).abs() < 1e-6
    );
    assert!(plan::chroma_noise(0.5 * full) < 0.3 * plan::chroma_noise(full));
    let raw = gray_raw(32, 32, 0.2);
    let mut rq = req(32);
    rq.capture.iso = Some(100);
    let p = plan::build(&raw, &Adjustments::default(), &rq).unwrap();
    assert!(!p.nr.luma_active() && p.nr.chroma_active());
    rq.capture.iso = Some(6400);
    let p = plan::build(&raw, &Adjustments::default(), &rq).unwrap();
    assert!(p.nr.luma_active());
    assert!(
        p.nr.chroma_sigma < 1e-3,
        "a flat synthetic frame measures no noise"
    );
    assert_eq!(plan::raw_noise(&gray_raw(2, 2, 0.2)), 0.0);
}

#[test]
fn render_request_helpers() {
    let r = RenderRequest::default();
    assert_eq!(r.max_size, Size::new(2560, 2560));
    assert_eq!(r.quality, Quality::Preview);
    assert!(format!("{r:?}").contains("masks: false"));
    let _ = crate::cpu::CpuRenderer::default();
}

/// A coarse fingerprint of what the pipeline renders (block means of a fixed synthetic
/// frame, lens profile and ISO set) for `adj`.
fn render_fingerprint(adj: &Adjustments) -> u64 {
    let raw = raw_from_fn(192, 128, |x, y| {
        let (u, v) = (x as f32 / 192.0, y as f32 / 128.0);
        [
            0.02 + 0.5 * u * u,
            0.05 + 0.3 * v,
            0.1 + 0.2 * (u * 6.0 + v * 3.0).sin().abs(),
        ]
    });
    let rq = RenderRequest {
        capture: crate::CaptureInfo {
            lens: Some("FE 24-70mm F2.8 GM".into()),
            focal_length: Some(35.0),
            aperture: Some(4.0),
            iso: Some(1600),
            crop_factor: None,
            camera: None,
        },
        ..req(192)
    };
    let img = cpu_renderer().render(ImageKey(1), &raw, adj, &rq).unwrap();
    let (w, h) = (img.size.width as usize, img.size.height as usize);
    let mut fp = 0u64;
    for by in 0..h / 16 {
        for bx in 0..w / 16 {
            for c in 0..3 {
                let mut s = 0u32;
                for y in by * 16..by * 16 + 16 {
                    for x in bx * 16..bx * 16 + 16 {
                        s += u32::from(img.rgba8[(y * w + x) * 4 + c]);
                    }
                }
                // Block mean in codes, rounded: robust to last-bit float differences.
                let m = u64::from((s + 128) / 256);
                fp = fp.wrapping_mul(1_000_003).wrapping_add(m);
            }
        }
    }
    fp
}

/// The default edit's fingerprint per pipeline version. A change to render output changes
/// it: bump `PIPELINE_VERSION` (cached renditions are keyed by it) and update the table.
/// From 9 the default edit uses the Standard profile; Neutral (what sidecars from before
/// profiles read as) must still render exactly what version 8 rendered.
#[test]
fn render_fingerprint_matches_the_pipeline_version() {
    const V7: u64 = 16_723_479_887_789_986_979;
    const FINGERPRINTS: [(u32, u64); 9] = [
        (2, 8_615_863_134_269_144_642),
        (3, V7),
        (4, V7),
        (5, V7),
        (6, V7),
        (7, V7),
        (8, V7),
        (9, 17_118_641_484_853_951_463),
        (10, 17_118_641_484_853_951_463),
    ];
    let fp = render_fingerprint(&Adjustments::default());
    println!("render fingerprint: {fp}");
    let expected = FINGERPRINTS
        .iter()
        .find(|(v, _)| *v == crate::PIPELINE_VERSION)
        .map(|f| f.1);
    assert_eq!(
        expected,
        Some(fp),
        "render output changed: bump ds_core::PIPELINE_VERSION and record the fingerprint"
    );
    let neutral = Adjustments {
        profile: ds_core::adjust::Profile::Neutral,
        ..Adjustments::default()
    };
    assert_eq!(render_fingerprint(&neutral), V7, "the Neutral look changed");
}

#[test]
fn frame_mapping_maps_final_frame_points_to_crop_space() {
    let raw = gray_raw(400, 200, 0.3);
    let map = |adj: &Adjustments, x: f64, y: f64| {
        let (size, m) = crate::frame_mapping(&raw, adj, &req(400)).unwrap();
        (
            size,
            (m[0] * x + m[1] * y + m[2], m[3] * x + m[4] * y + m[5]),
        )
    };
    let mut adj = Adjustments::default();
    adj.lens.enabled = false;
    let (size, p) = map(&adj, 0.25, 0.75);
    assert_eq!(size, Size::new(400, 200));
    assert!(
        (p.0 - 0.25).abs() < 1e-9 && (p.1 - 0.75).abs() < 1e-9,
        "{p:?}"
    );
    // One clockwise turn: the output's top-left shows the source's bottom-left.
    adj.crop.quarter_turns = 1;
    let (size, p) = map(&adj, 0.0, 0.0);
    assert_eq!(size, Size::new(200, 400));
    assert!(p.0.abs() < 1e-9 && (p.1 - 1.0).abs() < 1e-9, "{p:?}");
    // A crop: the final frame's corners are the crop rect's.
    adj.crop.quarter_turns = 0;
    adj.crop.rect = NormRect {
        x: 0.2,
        y: 0.1,
        w: 0.5,
        h: 0.6,
    };
    let (_, a) = map(&adj, 0.0, 0.0);
    let (_, b) = map(&adj, 1.0, 1.0);
    assert!(
        (a.0 - 0.2).abs() < 1e-6 && (a.1 - 0.1).abs() < 1e-6,
        "{a:?}"
    );
    assert!(
        (b.0 - 0.7).abs() < 1e-6 && (b.1 - 0.7).abs() < 1e-6,
        "{b:?}"
    );
    // A straightened full frame auto-fits: its corners are (about) inside the E frame
    // (plan allows half a pixel of slack).
    adj.crop.rect = NormRect::FULL;
    adj.crop.angle_deg = 5.0;
    let (_, c) = map(&adj, 0.0, 0.0);
    assert!(c.0 > 0.02 && c.1 > -0.01 && c.0 < 0.2 && c.1 < 0.2, "{c:?}");
    assert!(crate::frame_mapping(&raw, &adj, &req(0)).is_err());
}

// ------------------------------------------------------------ presence ---

/// Chroma (max − min) of a pixel, 8-bit.
fn chroma8(p: [u8; 3]) -> i32 {
    i32::from(*p.iter().max().unwrap()) - i32::from(*p.iter().min().unwrap())
}

#[test]
fn vibrance_boosts_muted_colours_more_and_protects_skin() {
    // Left: muted blue-grey; middle: saturated green; right: a skin tone.
    let raw = raw_from_fn(96, 32, |x, _| match x / 32 {
        0 => [0.16, 0.17, 0.2],
        1 => [0.05, 0.4, 0.05],
        _ => [0.45, 0.3, 0.22],
    });
    let base = render(&raw, &Adjustments::default(), 96);
    let mut a = Adjustments::default();
    a.presence.vibrance = 100.0;
    let v = render(&raw, &a, 96);
    let mut s = Adjustments::default();
    s.exposure.saturation = 50.0;
    let sat = render(&raw, &s, 96);
    let gain = |img: &RenderedImage, x| {
        f64::from(chroma8(px(img, x, 16))) / f64::from(chroma8(px(&base, x, 16)).max(1))
    };
    let (muted, vivid, skin) = (gain(&v, 16), gain(&v, 48), gain(&v, 80));
    assert!(muted > 1.3, "muted {muted}");
    assert!(muted > vivid + 0.2, "muted {muted} vs vivid {vivid}");
    assert!(skin < gain(&sat, 80), "skin {skin} protected vs saturation");
    // Negative desaturates the muted colour first.
    a.presence.vibrance = -100.0;
    let d = render(&raw, &a, 96);
    assert!(
        gain(&d, 16) < 0.4 && gain(&d, 48) > gain(&d, 16),
        "{} {}",
        gain(&d, 16),
        gain(&d, 48)
    );
}

/// Fine checker detail (period 8 px) on a smooth ramp.
fn textured() -> ds_core::RawImage {
    textured_sized(256, 192)
}

fn textured_sized(w: u32, h: u32) -> ds_core::RawImage {
    raw_from_fn(w, h, |x, y| {
        let base = 0.08 + 0.2 * (x % 256) as f32 / 256.0;
        let t = if ((x / 4) + (y / 4)).is_multiple_of(2) {
            1.05
        } else {
            0.95
        };
        [base * t; 3]
    })
}

/// High-pass (pixel − 5×5 mean) luma energy: fine texture.
fn fine_energy(img: &RenderedImage) -> f64 {
    let (w, h) = (img.size.width, img.size.height);
    let l = |x: u32, y: u32| {
        let p = px(img, x, y);
        0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2])
    };
    let mut e = 0.0;
    for y in 2..h - 2 {
        for x in 2..w - 2 {
            let m: f64 = (0..25)
                .map(|k| l(x + k % 5 - 2, y + k / 5 - 2))
                .sum::<f64>()
                / 25.0;
            e += (l(x, y) - m).powi(2);
        }
    }
    e / f64::from((w - 4) * (h - 4))
}

#[test]
fn texture_boosts_and_smooths_fine_detail_only() {
    let raw = textured();
    let mut a = Adjustments::default();
    a.noise.luminance = 0.0;
    a.detail.sharpen_amount = 0.0;
    let base = render(&raw, &a, 256);
    a.presence.texture = 100.0;
    let up = render(&raw, &a, 256);
    a.presence.texture = -100.0;
    let down = render(&raw, &a, 256);
    let (e0, eu, ed) = (fine_energy(&base), fine_energy(&up), fine_energy(&down));
    assert!(eu > 2.0 * e0, "texture + {eu} vs {e0}");
    assert!(ed < 0.7 * e0, "texture − {ed} vs {e0}");
    // Clearly visible: most of the textured frame moves by more than 4 levels.
    let moved = base
        .rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .zip(up.rgba8.as_chunks::<4>().0)
        .filter(|(p, q)| p[1].abs_diff(q[1]) > 4)
        .count();
    let n = (base.size.width * base.size.height) as usize;
    assert!(moved * 10 > n * 4, "{moved} of {n} pixels move > 4 levels");
    // Overall tone stays.
    assert!((mean_luma(&up) - mean_luma(&base)).abs() < 1.0);
}

#[test]
fn dehaze_removes_and_adds_haze() {
    // Dark and bright objects (fine texture) under a veil of bluish-white haze.
    let raw = raw_from_fn(256, 192, |x, y| {
        let obj = if ((x / 4) + (y / 4)).is_multiple_of(2) {
            0.05
        } else {
            0.35
        };
        let haze = 0.5;
        let tint = [0.5, 0.55, 0.6];
        [0, 1, 2].map(|c| obj * (1.0 - haze) + haze * tint[c])
    });
    // (Textured: a flat bright grey under haze reads as a cloud and is kept.)
    let base = render(&raw, &Adjustments::default(), 256);
    let mut a = Adjustments::default();
    a.presence.dehaze = 80.0;
    let clear = render(&raw, &a, 256);
    a.presence.dehaze = -80.0;
    let foggy = render(&raw, &a, 256);
    let (s0, s1, s2) = (std_luma(&base), std_luma(&clear), std_luma(&foggy));
    assert!(s1 > 1.3 * s0, "dehaze + contrast {s1} vs {s0}");
    assert!(s2 < 0.8 * s0, "dehaze − contrast {s2} vs {s0}");
    // The veil comes off: the image gets darker, not brighter.
    assert!(mean_luma(&clear) < mean_luma(&base));
    let al = crate::plan::airlight_camera(&raw);
    assert!(al.iter().all(|v| *v > 0.1), "{al:?}");
}

/// A white disc (white water), a grey one (a cloud's underside) and textured land under
/// haze: dehaze +100 clears the land but leaves both discs' tone; −100 veils without
/// whiting out.
#[test]
fn dehaze_keeps_white_objects_white_and_negative_keeps_the_scene_visible() {
    let raw = raw_from_fn(256, 192, |x, y| {
        let d = ((x as f32 - 64.0).powi(2) + (y as f32 - 64.0).powi(2)).sqrt();
        if d < 24.0 {
            return [0.8, 0.82, 0.85];
        }
        let g = ((x as f32 - 190.0).powi(2) + (y as f32 - 60.0).powi(2)).sqrt();
        if g < 24.0 {
            return [0.36, 0.37, 0.39];
        }
        let obj = if ((x / 4) + (y / 4)).is_multiple_of(2) {
            0.04
        } else {
            0.3
        };
        let haze = 0.4 * y as f32 / 192.0 + 0.1;
        let tint = [0.55, 0.6, 0.66];
        let land = [obj * 0.9, obj, obj * 0.7];
        [0, 1, 2].map(|c| land[c] * (1.0 - haze) + haze * tint[c])
    });
    let base = render(&raw, &Adjustments::default(), 256);
    let mut a = Adjustments::default();
    a.presence.dehaze = 100.0;
    let clear = render(&raw, &a, 256);
    a.presence.dehaze = -100.0;
    let foggy = render(&raw, &a, 256);
    let lab = |img: &RenderedImage, x: u32, y: u32| crate::color::srgb8_to_lab(px(img, x, y));
    let disc = |img: &RenderedImage, cx: u32, cy: u32| {
        let mut l = 0.0;
        for (x, y) in [(0, 0), (-8, -4), (6, 6), (-4, 8)] {
            l += lab(
                img,
                cx.saturating_add_signed(x),
                cy.saturating_add_signed(y),
            )[0] / 4.0;
        }
        l
    };
    for (cx, cy) in [(64, 64), (190, 60)] {
        let lost = disc(&base, cx, cy) - disc(&clear, cx, cy);
        assert!(lost <= 5.0, "the disc at ({cx}, {cy}) loses {lost} L");
    }
    // The land gains contrast.
    let spread = |img: &RenderedImage| {
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for x in 130..250 {
            let l = lab(img, x, 150)[0];
            lo = lo.min(l);
            hi = hi.max(l);
        }
        hi - lo
    };
    assert!(
        spread(&clear) > 1.3 * spread(&base),
        "{} vs {}",
        spread(&clear),
        spread(&base)
    );
    // −100: a veil, not a white-out.
    assert!(
        spread(&foggy) > 0.5 * spread(&base),
        "{} vs {}",
        spread(&foggy),
        spread(&base)
    );
}

#[test]
fn layers_carry_presence_and_local_sharpening() {
    let raw = textured();
    let mut a = Adjustments::default();
    a.noise.luminance = 0.0;
    a.detail.sharpen_amount = 0.0;
    let base = render(&raw, &a, 256);
    let fill = |adj: LayerAdjustments| Layer {
        id: 1,
        adjustments: adj,
        mask: Mask {
            shapes: vec![MaskShape::Fill { op: MaskOp::Add }],
            ..Mask::default()
        },
        ..Layer::default()
    };
    let with = |adj: LayerAdjustments| {
        let mut b = a.clone();
        b.layers = vec![fill(adj)];
        render(&raw, &b, 256)
    };
    let soft = with(LayerAdjustments {
        sharpening: -100.0,
        ..LayerAdjustments::default()
    });
    let sharp = LayerAdjustments {
        sharpening: 100.0,
        ..LayerAdjustments::default()
    };
    let crisp = with(sharp.clone());
    let e0 = fine_energy(&base);
    assert!(
        fine_energy(&soft) < 0.5 * e0,
        "softening {}",
        fine_energy(&soft)
    );
    // Sharpening's kernel is ~0.9 px on a 6000 px frame: a 256 px view of the frame
    // shows (nearly) nothing, as its export downsampled would.
    assert!(
        fine_energy(&crisp) < 1.05 * e0,
        "sharpening at 256 px {}",
        fine_energy(&crisp)
    );
    // At 1:1 on a 4800 px frame it sharpens.
    let big = textured_sized(4800, 40);
    let mut b = a.clone();
    let full = render(&big, &b, 4800);
    b.layers = vec![fill(sharp)];
    let sharpened = render(&big, &b, 4800);
    assert!(
        fine_energy(&sharpened) > 1.5 * fine_energy(&full),
        "sharpening at 1:1 {} vs {}",
        fine_energy(&sharpened),
        fine_energy(&full)
    );
    // A full-frame layer with presence equals the same global presence.
    let mut g = a.clone();
    g.presence.texture = 60.0;
    g.presence.vibrance = 40.0;
    let global = render(&raw, &g, 256);
    let local = with(LayerAdjustments {
        presence: ds_core::adjust::Presence {
            vibrance: 40.0,
            texture: 60.0,
            dehaze: 0.0,
        },
        ..LayerAdjustments::default()
    });
    let d = global
        .rgba8
        .iter()
        .zip(&local.rgba8)
        .map(|(x, y)| x.abs_diff(*y))
        .max()
        .unwrap();
    assert!(d <= 2, "layer presence vs global: max diff {d}");
}

/// Layer sharpening is defined in normalized units: a fit view shows what the export
/// has once it's downsampled to the same size, and noise below the measured noise level
/// is left alone.
#[test]
fn layer_sharpening_matches_across_scales_and_spares_noise() {
    let fill = |adj: LayerAdjustments| Layer {
        id: 1,
        adjustments: adj,
        mask: Mask {
            shapes: vec![MaskShape::Fill { op: MaskOp::Add }],
            ..Mask::default()
        },
        ..Layer::default()
    };
    let mut a = Adjustments::default();
    a.noise.luminance = 0.0;
    a.detail.sharpen_amount = 0.0;
    let mut s = a.clone();
    s.layers = vec![fill(LayerAdjustments {
        sharpening: 100.0,
        ..LayerAdjustments::default()
    })];
    // Box-downsample an image by 4.
    let down4 = |img: &RenderedImage| {
        let (w, h) = (img.size.width / 4, img.size.height / 4);
        let mut out = img.clone();
        out.size = Size::new(w, h);
        out.rgba8 = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                (0..4).map(move |c| (x, y, c))
            })
            .map(|(x, y, c)| {
                let mut acc = 0u32;
                for k in 0..16 {
                    let (xx, yy) = (x * 4 + k % 4, y * 4 + k / 4);
                    acc += u32::from(img.rgba8[((yy * img.size.width + xx) * 4 + c) as usize]);
                }
                (acc / 16) as u8
            })
            .collect();
        out
    };
    let raw = textured_sized(4800, 64);
    let gain = |n: u32, down: bool| {
        let r = |adj: &Adjustments| {
            let img = render(&raw, adj, n);
            if down { down4(&img) } else { img }
        };
        fine_energy(&r(&s)) / fine_energy(&r(&a))
    };
    // (A pixel-unit kernel gave the fit view 8× the export's change; this all-edges
    // checker is the hardest case for the low-frequency model.)
    let (fit, export) = (gain(1200, false), gain(4800, true));
    assert!(
        fit > 1.1 && (fit - export).abs() < 0.3,
        "fit {fit} vs export {export}"
    );
    // Flat grey with sensor noise: sharpening leaves the noise (mostly) alone.
    let noisy = raw_from_fn(4800, 64, |x, y| [0.18 * (1.0 + 0.04 * noise(x, y)); 3]);
    let e = |adj: &Adjustments| fine_energy(&render(&noisy, adj, 4800));
    let ratio = e(&s) / e(&a);
    assert!(ratio < 1.3, "noise grows {ratio}×");
}

#[test]
fn layer_whites_and_blacks_move_the_ends_of_the_range() {
    // A ramp from deep shadow to bright highlight.
    let raw = raw_from_fn(256, 16, |x, _| {
        [0.002 + 0.9 * (x as f32 / 256.0).powi(2); 3]
    });
    let base = render(&raw, &Adjustments::default(), 256);
    let with = |whites: f32, blacks: f32| {
        let mut a = Adjustments::default();
        a.layers = vec![Layer {
            id: 1,
            adjustments: LayerAdjustments {
                hdr: ds_core::adjust::Hdr {
                    whites,
                    blacks,
                    ..Default::default()
                },
                ..LayerAdjustments::default()
            },
            mask: Mask {
                shapes: vec![MaskShape::Fill { op: MaskOp::Add }],
                ..Mask::default()
            },
            ..Layer::default()
        }];
        render(&raw, &a, 256)
    };
    let y = |img: &RenderedImage, x| i32::from(px(img, x, 8)[1]);
    let bright = with(100.0, 0.0);
    assert!(
        y(&bright, 230) > y(&base, 230) + 3,
        "whites lift highlights"
    );
    assert_eq!(y(&bright, 20), y(&base, 20), "whites leave shadows");
    let crushed = with(0.0, -100.0);
    assert!(y(&crushed, 20) < y(&base, 20), "blacks crush shadows");
    assert!(
        (y(&crushed, 230) - y(&base, 230)).abs() <= 1,
        "blacks leave highlights"
    );
}

#[test]
fn presence_at_identity_changes_nothing() {
    let raw = textured();
    let p = crate::plan::build(&raw, &Adjustments::default(), &req(256)).unwrap();
    assert!(!p.needs_dehaze() && !p.needs_detail() && !p.needs_layer_sharpen());
    assert_eq!(p.airlight, [1.0; 3], "no airlight measured when unused");
}

// ---------------------------------------------------------------- masks ---

/// A +1.5 EV layer with `mask` on `raw`, and the render without it.
fn masked(
    raw: &ds_core::RawImage,
    mask: Mask,
    source: Option<std::sync::Arc<dyn crate::MaskSource>>,
) -> (RenderedImage, RenderedImage) {
    let mut a = Adjustments::default();
    a.detail.sharpen_amount = 0.0;
    let r = RenderRequest {
        masks: source,
        ..req(256)
    };
    let base = cpu_renderer().render(ImageKey(1), raw, &a, &r).unwrap();
    a.layers = vec![Layer {
        id: 1,
        adjustments: LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 1.5,
                ..Default::default()
            },
            ..LayerAdjustments::default()
        },
        mask,
        ..Layer::default()
    }];
    let img = cpu_renderer().render(ImageKey(2), raw, &a, &r).unwrap();
    (base, img)
}

/// How much the layer brightened pixel `(x, y)` (green channel, 8-bit).
fn lift(pair: &(RenderedImage, RenderedImage), x: u32, y: u32) -> i32 {
    i32::from(px(&pair.1, x, y)[1]) - i32::from(px(&pair.0, x, y)[1])
}

fn brush(points: Vec<[f32; 2]>, size: f32, auto_mask: bool) -> ds_core::adjust::BrushStroke {
    ds_core::adjust::BrushStroke {
        points,
        size,
        hardness: 0.8,
        auto_mask,
        ..ds_core::adjust::BrushStroke::default()
    }
}

#[test]
fn colour_range_refinement_limits_a_layer_to_a_hue() {
    // Left half red, right half blue.
    let raw = raw_from_fn(256, 128, |x, _| {
        if x < 128 {
            [0.3, 0.06, 0.05]
        } else {
            [0.05, 0.08, 0.3]
        }
    });
    let mask = Mask {
        shapes: vec![MaskShape::Fill { op: MaskOp::Add }],
        color_range: Some(ColorRange {
            center_hue: 0.0,
            width: 60.0,
            ..ColorRange::default()
        }),
        ..Mask::default()
    };
    let r = masked(&raw, mask, None);
    assert!(lift(&r, 64, 64) > 20, "red lifted: {}", lift(&r, 64, 64));
    assert!(
        lift(&r, 192, 64).abs() <= 1,
        "blue untouched: {}",
        lift(&r, 192, 64)
    );
}

#[test]
fn shape_feather_softens_brush_and_colour_range_edges() {
    let raw = gray_raw(256, 128, 0.12);
    let stroke = |feather: f32| Mask {
        shapes: vec![MaskShape::Brush {
            op: MaskOp::Add,
            strokes: vec![brush(vec![[0.5, 0.0], [0.5, 1.0]], 0.2, false)],
            feather,
        }],
        ..Mask::default()
    };
    // Partially lifted pixels across the stroke's edge (x 80..176).
    let soft_width = |r: &(RenderedImage, RenderedImage)| {
        (80..176)
            .filter(|x| {
                let l = lift(r, *x, 64);
                l > 2 && l < lift(r, 128, 64) - 2
            })
            .count()
    };
    let (hard, soft) = (
        masked(&raw, stroke(0.0), None),
        masked(&raw, stroke(0.03), None),
    );
    assert!(
        soft_width(&soft) > soft_width(&hard) + 8,
        "feathered edge {} vs {}",
        soft_width(&soft),
        soft_width(&hard)
    );
    // A colour range with a spatial feather: across a hard colour edge the selection
    // stays put (no halo onto the other colour)...
    let raw = raw_from_fn(256, 128, |x, _| {
        if x < 128 {
            [0.3, 0.06, 0.05]
        } else {
            [0.05, 0.08, 0.3]
        }
    });
    let cr = |feather: f32| Mask {
        shapes: vec![MaskShape::ColorRange {
            op: MaskOp::Add,
            range: ColorRange {
                center_hue: 0.0,
                width: 60.0,
                ..ColorRange::default()
            },
            feather,
        }],
        ..Mask::default()
    };
    let (sharp, bled) = (masked(&raw, cr(0.0), None), masked(&raw, cr(0.03), None));
    assert!(lift(&sharp, 134, 64).abs() <= 1);
    assert!(
        lift(&bled, 134, 64).abs() <= 2,
        "halo {}",
        lift(&bled, 134, 64)
    );
    assert!(
        lift(&bled, 120, 64) >= lift(&bled, 60, 64) - 2,
        "selected side kept"
    );
    // ...while within one surface whose hue drifts, the feather widens the transition.
    let ramp = raw_from_fn(256, 128, |x, _| {
        let t = x as f32 / 256.0;
        [0.3 - 0.25 * t, 0.07, 0.05 + 0.25 * t]
    });
    let partial = |r: &(RenderedImage, RenderedImage)| {
        let full = lift(r, 8, 64).max(1);
        (0..256)
            .filter(|x| {
                let l = lift(r, *x, 64);
                l > 2 && l < full - 2
            })
            .count()
    };
    let (rs, rb) = (masked(&ramp, cr(0.0), None), masked(&ramp, cr(0.03), None));
    assert!(
        partial(&rb) > partial(&rs),
        "{} vs {}",
        partial(&rb),
        partial(&rs)
    );
}

/// A sky over foliage of the same tone, with a bright cloud: an auto stroke along the
/// sky takes the sky, not the foliage (colour counts, not only tone), and its
/// neighbourhood is the same at every scale.
fn sky_and_foliage(w: u32, h: u32) -> ds_core::RawImage {
    raw_from_fn(w, h, |x, y| {
        let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
        let t = 1.0 + 0.03 * noise(x, y);
        if v < 0.5 {
            let cloud = ((u - 0.6).powi(2) + (v - 0.25).powi(2)).sqrt() < 0.08;
            if cloud {
                [0.6 * t; 3]
            } else {
                [0.08 * t, 0.16 * t, 0.42 * t]
            }
        } else {
            [0.12 * t, 0.24 * t, 0.05 * t]
        }
    })
}

#[test]
fn auto_masks_keep_to_the_colour_and_scale() {
    let stroke = |size: f32| Layer {
        id: 1,
        adjustments: LayerAdjustments {
            exposure: ds_core::adjust::Exposure {
                exposure_ev: 2.0,
                ..Default::default()
            },
            ..LayerAdjustments::default()
        },
        mask: Mask {
            shapes: vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![brush(vec![[0.05, 0.45], [0.95, 0.45]], size, true)],
                feather: 0.0,
            }],
            ..Mask::default()
        },
        ..Layer::default()
    };
    let mut a = Adjustments::default();
    a.noise.luminance = 0.0;
    let raw = sky_and_foliage(256, 128);
    let base = render(&raw, &a, 256);
    a.layers = vec![stroke(0.3)];
    let img = render(&raw, &a, 256);
    // Sky under the stroke brightens; the foliage it also covers (same tone) doesn't.
    let lift = |x: u32, y: u32| i32::from(px(&img, x, y)[1]) - i32::from(px(&base, x, y)[1]);
    assert!(lift(40, 50) > 20, "sky {}", lift(40, 50));
    assert!(lift(40, 80) < 5, "foliage {}", lift(40, 80));
    // Scale: a 2400 px render boxed down to 600 matches a 600 px render in the stroke.
    let raw = sky_and_foliage(2400, 1200);
    a.layers = vec![stroke(0.3)];
    let fit = render(&raw, &a, 600);
    let big = render(&raw, &a, 2400);
    let mut worst = 0.0f64;
    let mut sum = 0.0f64;
    let mut n = 0.0;
    for y in 100..170u32 {
        for x in 20..580u32 {
            let mut acc = 0u32;
            for k in 0..16 {
                acc += u32::from(px(&big, x * 4 + k % 4, y * 4 + k / 4)[1]);
            }
            let d = (f64::from(acc) / 16.0 - f64::from(px(&fit, x, y)[1])).abs();
            worst = worst.max(d);
            sum += d;
            n += 1.0;
        }
    }
    assert!(
        sum / n < 2.0 && worst < 25.0,
        "mean {} max {worst}",
        sum / n
    );
}

/// Heavy edits (masked layers with local tools and output-space planes) drag at a
/// smaller Draft size; light ones keep the full one.
#[test]
fn heavy_edits_drag_at_a_smaller_draft() {
    use crate::plan::draft_long_edge;
    let mut a = Adjustments::default();
    assert_eq!(draft_long_edge(&a), crate::DRAFT_LONG_EDGE);
    let layer = |shapes: Vec<MaskShape>, clarity: f32| Layer {
        id: 1,
        adjustments: LayerAdjustments {
            clarity,
            ..LayerAdjustments::default()
        },
        mask: Mask {
            shapes,
            ..Mask::default()
        },
        ..Layer::default()
    };
    let range = || MaskShape::ColorRange {
        op: MaskOp::Add,
        range: ColorRange::default(),
        feather: 0.02,
    };
    a.layers = vec![layer(vec![range()], 30.0)];
    a.presence.dehaze = 40.0;
    assert_eq!(draft_long_edge(&a), crate::DRAFT_LONG_EDGE, "one layer");
    a.layers.push(layer(vec![range()], 0.0));
    assert_eq!(draft_long_edge(&a), crate::DRAFT_LONG_EDGE_HEAVY);
    // A disabled layer costs nothing.
    a.layers[1].enabled = false;
    assert_eq!(draft_long_edge(&a), crate::DRAFT_LONG_EDGE);
    a.layers[1].enabled = true;
    let raw = gray_raw(4000, 3000, 0.2);
    let req = RenderRequest {
        quality: crate::Quality::Draft,
        ..req(4000)
    };
    let p = crate::plan::build(&raw, &a, &req).unwrap();
    assert_eq!(p.geo.out.width, crate::DRAFT_LONG_EDGE_HEAVY);
    assert_eq!(p.upscale, Some(Size::new(4000, 3000)));
}

#[test]
fn auto_masked_strokes_stop_at_image_edges() {
    // Dark left, bright right: a stroke on the dark side whose brush overlaps the edge.
    let raw = raw_from_fn(256, 128, |x, y| {
        let v = if x < 128 { 0.01 } else { 0.08 };
        [v * (1.0 + 0.02 * noise(x, y)); 3]
    });
    let paint = |auto: bool| Mask {
        shapes: vec![MaskShape::Brush {
            op: MaskOp::Add,
            strokes: vec![brush(vec![[0.46, 0.0], [0.46, 1.0]], 0.2, auto)],
            feather: 0.0,
        }],
        ..Mask::default()
    };
    let (plain, auto) = (
        masked(&raw, paint(false), None),
        masked(&raw, paint(true), None),
    );
    // Inside the dark side both paint; across the edge only the plain stroke does.
    assert!(
        lift(&plain, 110, 64) > 3 && lift(&auto, 110, 64) > 3,
        "{} {}",
        lift(&plain, 110, 64),
        lift(&auto, 110, 64)
    );
    let (p, a) = (lift(&plain, 134, 64), lift(&auto, 134, 64));
    assert!(p > 10, "plain stroke crosses the edge: {p}");
    assert!(a < p / 4, "auto-mask stops at the edge: {a} vs {p}");
}

#[derive(Debug)]
struct SkyOnly;
impl crate::MaskSource for SkyOnly {
    fn ai_mask(&self, t: ds_core::adjust::AiTarget) -> Option<std::sync::Arc<crate::MaskBitmap>> {
        (t == ds_core::adjust::AiTarget::Sky).then(|| {
            // Top half.
            let (w, h) = (32u32, 16u32);
            std::sync::Arc::new(crate::MaskBitmap {
                size: Size::new(w, h),
                data: (0..w * h)
                    .map(|i| if i / w < h / 2 { 255 } else { 0 })
                    .collect(),
                version: 0,
            })
        })
    }
}

#[test]
fn sky_masks_from_the_app_are_composited() {
    let raw = gray_raw(256, 128, 0.12);
    let sky = |feather: f32| Mask {
        shapes: vec![MaskShape::Ai {
            op: MaskOp::Add,
            target: ds_core::adjust::AiTarget::Sky,
            feather,
            person: None,
        }],
        ..Mask::default()
    };
    let src: std::sync::Arc<dyn crate::MaskSource> = std::sync::Arc::new(SkyOnly);
    let r = masked(&raw, sky(0.0), Some(src.clone()));
    assert!(lift(&r, 128, 20) > 20, "sky lifted");
    assert!(lift(&r, 128, 110).abs() <= 1, "ground untouched");
    // Feathered: the boundary (row 64) is soft over more rows.
    let soft = masked(&raw, sky(0.05), Some(src));
    let partial = |r: &(RenderedImage, RenderedImage)| {
        (0..128)
            .filter(|y| {
                let l = lift(r, 128, *y);
                l > 2 && l < lift(r, 128, 10) - 2
            })
            .count()
    };
    assert!(
        partial(&soft) > partial(&r) + 8,
        "{} vs {}",
        partial(&soft),
        partial(&r)
    );
    // No source: the shape is empty.
    let none = masked(&raw, sky(0.0), None);
    assert!(lift(&none, 128, 20).abs() <= 1);
}

// ---------------------------------------------------------------- spots ---

/// A horizontal brightness gradient with fine texture and a dark blemish (radius 12 px)
/// at (64, 64) of 256 × 128.
fn blemished() -> ds_core::RawImage {
    raw_from_fn(256, 128, |x, y| {
        let g = 0.05 + 0.25 * x as f32 / 256.0;
        let t = 1.0 + 0.06 * noise(x, y);
        let d = ((x as f32 - 64.0).powi(2) + (y as f32 - 64.0).powi(2)).sqrt();
        let blemish = if d < 12.0 { 0.2 } else { 1.0 };
        [g * t * blemish; 3]
    })
}

fn spot(kind: ds_core::adjust::SpotKind, source: [f32; 2]) -> ds_core::adjust::Spot {
    ds_core::adjust::Spot {
        kind,
        center: [0.25, 0.5],
        radius: 16.0 / 256.0,
        source,
        feather: 0.0,
        opacity: 1.0,
    }
}

fn mean_disc(img: &RenderedImage, cx: u32, cy: u32, r: u32) -> f64 {
    let mut s = 0.0;
    let mut n = 0.0;
    for y in cy - r..=cy + r {
        for x in cx - r..=cx + r {
            if (f64::from(x) - f64::from(cx)).hypot(f64::from(y) - f64::from(cy)) <= f64::from(r) {
                s += f64::from(px(img, x, y)[1]);
                n += 1.0;
            }
        }
    }
    s / n
}

#[test]
fn clone_copies_the_source_and_heal_keeps_the_destination_tone() {
    use ds_core::adjust::SpotKind;
    let raw = blemished();
    let mut a = Adjustments::default();
    a.noise.luminance = 0.0;
    let base = render(&raw, &a, 256);
    // Surroundings of the blemish (a ring just outside it) and the clean source.
    let around =
        |img: &RenderedImage| f64::midpoint(mean_disc(img, 64, 44, 3), mean_disc(img, 64, 84, 3));
    let blem = mean_disc(&base, 64, 64, 8);
    assert!(blem < around(&base) - 20.0, "the blemish is dark");
    // Source far right: much brighter there.
    let src = [0.75, 0.5];
    a.spots = vec![spot(SpotKind::Clone, src)];
    let cloned = render(&raw, &a, 256);
    let far = mean_disc(&base, 192, 64, 8);
    assert!(
        (mean_disc(&cloned, 64, 64, 8) - far).abs() < 3.0,
        "clone brings the source's tone: {} vs {far}",
        mean_disc(&cloned, 64, 64, 8)
    );
    a.spots = vec![spot(SpotKind::Heal, src)];
    let healed = render(&raw, &a, 256);
    let h = mean_disc(&healed, 64, 64, 8);
    assert!(
        (h - around(&base)).abs() < 4.0,
        "heal matches the surroundings: {h} vs {}",
        around(&base)
    );
    // Outside the spot nothing changes.
    assert_eq!(px(&healed, 200, 20), px(&base, 200, 20));
    // Half opacity: halfway (in linear light, roughly) between blemish and heal.
    a.spots[0].opacity = 0.5;
    let half = mean_disc(&render(&raw, &a, 256), 64, 64, 8);
    assert!(half > blem + 5.0 && half < h - 3.0, "{blem} < {half} < {h}");
    // A soft edge blends into the surroundings instead of a hard circle.
    a.spots[0].opacity = 1.0;
    a.spots[0].feather = 1.0;
    let soft = render(&raw, &a, 256);
    assert!(mean_disc(&soft, 64, 64, 2) > blem + 10.0);
}

#[test]
fn spots_outside_the_render_are_dropped() {
    use ds_core::adjust::SpotKind;
    let raw = blemished();
    let mut a = Adjustments::default();
    a.spots = vec![
        spot(SpotKind::Heal, [0.75, 0.5]),
        ds_core::adjust::Spot {
            center: [3.0, 3.0],
            ..spot(SpotKind::Clone, [0.5, 0.5])
        },
        ds_core::adjust::Spot {
            radius: f32::NAN,
            ..spot(SpotKind::Clone, [0.5, 0.5])
        },
    ];
    let p = crate::plan::build(&raw, &a, &req(256)).unwrap();
    assert_eq!(p.spots.len(), 1);
    // A 1:1 region away from the spot plans none; one around it does.
    let region = |x: f32| RenderRequest {
        region: Some(NormRect {
            x,
            y: 0.0,
            w: 0.2,
            h: 1.0,
        }),
        ..req(256)
    };
    assert_eq!(
        crate::plan::build(&raw, &a, &region(0.7))
            .unwrap()
            .spots
            .len(),
        0
    );
    let near = crate::plan::build(&raw, &a, &region(0.15)).unwrap();
    assert_eq!(near.spots.len(), 1);
    // Its BASE covers the source (x ≈ 0.75 of the sensor), far outside the region.
    let b = near.sensor_bbox;
    assert!(b[0] + b[2] >= 200, "{b:?}");
}

#[test]
fn off_image_sources_never_paint_black() {
    use ds_core::adjust::SpotKind;
    let raw = blemished();
    let mut a = Adjustments::default();
    a.noise.luminance = 0.0;
    let base = render(&raw, &a, 256);
    // The darkest non-blemish tone (the left edge of the gradient).
    let floor = (0..128u32)
        .flat_map(|y| (0..20u32).map(move |x| (x, y)))
        .map(|(x, y)| px(&base, x, y)[1])
        .min()
        .unwrap();
    // Inside the disc (clear of the edge, where output sharpening rings on a hard edge).
    let disc_min = |img: &RenderedImage| {
        (55..74u32)
            .flat_map(|y| (145..163u32).map(move |x| (x, y)))
            .map(|(x, y)| px(img, x, y)[1])
            .min()
            .unwrap()
    };
    for kind in [SpotKind::Heal, SpotKind::Clone] {
        for sx in [0.99, 1.2, -0.5, 1e30] {
            a.spots = vec![ds_core::adjust::Spot {
                center: [0.6, 0.5],
                ..spot(kind, [sx, 0.5])
            }];
            let img = render(&raw, &a, 256);
            let m = disc_min(&img);
            assert!(m + 3 >= floor, "{kind:?} from x {sx}: {m} < {floor}");
        }
    }
    // A spot at the right edge heals from on-image ring samples only.
    a.spots = vec![ds_core::adjust::Spot {
        center: [0.99, 0.5],
        ..spot(SpotKind::Heal, [0.9, 0.5])
    }];
    let edge = render(&raw, &a, 256);
    for x in 240..256 {
        let (v, b) = (px(&edge, x, 64)[1], px(&base, x, 64)[1]);
        assert!(v.abs_diff(b) < 12, "x {x}: {v} vs {b}");
    }
    // Radius 0.5 (the whole frame): the source is pinned to the destination, a no-op.
    a.spots = vec![ds_core::adjust::Spot {
        center: [0.5, 0.5],
        radius: 0.5,
        ..spot(SpotKind::Heal, [1.2, 0.5])
    }];
    let whole = render(&raw, &a, 256);
    for (x, y) in [(10, 10), (64, 64), (128, 64), (250, 120)] {
        let (v, b) = (px(&whole, x, y), px(&base, x, y));
        assert!(
            v.iter().zip(b).all(|(p, q)| p.abs_diff(q) <= 1),
            "({x}, {y}): {v:?} vs {b:?}"
        );
    }
}

#[test]
fn every_spot_applies_past_64() {
    use ds_core::adjust::SpotKind;
    let raw = blemished();
    let mut a = Adjustments::default();
    a.noise.luminance = 0.0;
    let base = render(&raw, &a, 256);
    // 100 clones on a grid, each from the bright right edge of its row.
    let grid: Vec<(u32, u32)> = (0..100u32)
        .map(|i| (20 + 20 * (i % 10), 10 + 12 * (i / 10)))
        .collect();
    a.spots = grid
        .iter()
        .map(|&(x, y)| ds_core::adjust::Spot {
            center: [x as f32 / 256.0, y as f32 / 128.0],
            radius: 3.0 / 256.0,
            ..spot(SpotKind::Clone, [0.93, y as f32 / 128.0])
        })
        .collect();
    let img = render(&raw, &a, 256);
    for &(x, y) in &grid {
        let (v, want) = (px(&img, x, y)[1], px(&base, 238, y)[1]);
        assert!(v.abs_diff(want) < 12, "spot at ({x}, {y}): {v} vs {want}");
    }
}

#[test]
fn heal_across_light_and_shade_takes_the_destination_colour() {
    use ds_core::adjust::SpotKind;
    // A textured warm surface, sunlit on the left and in bluish shade (4× darker) on the
    // right, with a white streak through the destination's ring; heal a shaded spot
    // from a sunlit source.
    let raw = raw_from_fn(256, 128, |x, y| {
        let t = 1.0 + 0.15 * noise(x, y);
        if x < 128 {
            [0.40 * t, 0.30 * t, 0.16 * t]
        } else if (210..213).contains(&x) {
            // A thin white streak (spray) crossing the destination ring.
            [0.9; 3]
        } else {
            [0.095 * t, 0.075 * t, 0.046 * t]
        }
    });
    let mut a = Adjustments::default();
    a.noise.luminance = 0.0;
    a.spots = vec![ds_core::adjust::Spot {
        center: [0.75, 0.5],
        ..spot(SpotKind::Heal, [0.25, 0.5])
    }];
    let img = render(&raw, &a, 256);
    let lab = |x0: u32, x1: u32| {
        let mut s = [0.0f32; 3];
        let mut n = 0.0;
        for y in 54..74 {
            for x in x0..x1 {
                let l = crate::color::srgb8_to_lab(px(&img, x, y));
                for c in 0..3 {
                    s[c] += l[c];
                }
                n += 1.0;
            }
        }
        s.map(|v| v / n)
    };
    let (disc, around) = (lab(182, 202), lab(220, 240));
    let chroma = |l: [f32; 3]| l[1].hypot(l[2]);
    assert!(
        (chroma(disc) - chroma(around)).abs() < 3.0 && (disc[0] - around[0]).abs() < 3.0,
        "{disc:?} vs {around:?}"
    );
}

/// `OutputSpace::Rec2020` keeps colours outside Display P3 (exports to an ICC profile,
/// the soft-proofing viewer); P3 output compresses them into P3.
#[test]
fn rec2020_output_keeps_colours_outside_p3() {
    use crate::OutputFormat;
    let raw = raw_from_fn(
        16,
        16,
        |x, _| if x < 8 { [0.0, 0.3, 0.0] } else { [0.18; 3] },
    );
    let mut adj = Adjustments::default();
    adj.exposure.saturation = 100.0;
    let f16 = |output: OutputSpace| -> Vec<[f32; 3]> {
        let mut out = vec![0u8; 16 * 16 * 8];
        cpu_renderer()
            .render_into(
                ImageKey(9),
                &raw,
                &adj,
                &RenderRequest { output, ..req(16) },
                OutputFormat::Rgba16fLinearP3,
                &mut out,
            )
            .unwrap();
        out.as_chunks::<8>()
            .0
            .iter()
            .map(|p| [0, 2, 4].map(|k| half::f16::from_ne_bytes([p[k], p[k + 1]]).to_f32()))
            .collect()
    };
    let (p3, wide) = (f16(OutputSpace::DisplayP3), f16(OutputSpace::Rec2020));
    let to_p3 = crate::color::working_to_output(OutputSpace::DisplayP3);
    let green = crate::color::mat_vec(&to_p3, wide[8 * 16 + 2]);
    assert!(
        green.iter().any(|v| *v < -0.01),
        "the green leaves P3: {green:?}"
    );
    assert!(
        p3[8 * 16 + 2].iter().all(|v| *v >= 0.0),
        "{:?}",
        p3[8 * 16 + 2]
    );
    // An in-gamut gray is the same colour either way.
    let gray = crate::color::mat_vec(&to_p3, wide[8 * 16 + 12]);
    assert!(
        gray.iter()
            .zip(p3[8 * 16 + 12])
            .all(|(a, b)| (a - b).abs() < 2e-3),
        "{gray:?}"
    );
}
