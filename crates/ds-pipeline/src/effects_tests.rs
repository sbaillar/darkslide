//! Behavioral tests of the effects (lens blur, glow, halation, grain) on synthetic data.
#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use ds_core::adjust::{Adjustments, AiTarget};
use ds_core::effects::{BlurMode, BokehShape};
use ds_core::{NormRect, Size};

use crate::effects::{
    self, CocSource, FxPlan, LensBlurPlan, SHAPE_BLADES, SHAPE_CAT_EYE, SHAPE_CIRCLE,
};
use crate::geometry::{self, Fit, Geometry};
use crate::mask::{MaskBitmap, MaskSource};
use crate::testutil::{noise, raw_from_fn};
use crate::{CaptureInfo, ImageKey, RenderRequest, RenderedImage, Renderer, cpu_renderer};

/// A 256×256 frame: a square subject in the middle (`64..192`), the rest background.
const N: usize = 256;

fn inside(x: usize, y: usize) -> bool {
    (64..192).contains(&x) && (64..192).contains(&y)
}

fn geo(n: u32) -> Geometry {
    let raw = raw_from_fn(n, n, |_, _| [0.2; 3]);
    geometry::plan(
        &raw,
        &Adjustments::default(),
        &CaptureInfo::default(),
        None,
        Size::new(n, n),
        Fit::FillOriginalSize,
    )
    .unwrap()
}

/// Depth (64×64): 255 on the subject square, `far` elsewhere.
fn depth(far: u8) -> Arc<MaskBitmap> {
    let s = 64usize;
    Arc::new(MaskBitmap::new(
        Size::new(s as u32, s as u32),
        (0..s * s)
            .map(|i| {
                if inside(i % s * 4, i / s * 4) {
                    255
                } else {
                    far
                }
            })
            .collect(),
    ))
}

fn depth_plan(map: Arc<MaskBitmap>, rmax: f32, shape: u32, boost: f32) -> LensBlurPlan {
    let mut f = 1;
    while rmax / f as f32 > effects::RLOW_MAX {
        f *= 2;
    }
    LensBlurPlan {
        rmax,
        f,
        rlow: (rmax / f as f32).ceil() as usize,
        off: [0, 0],
        boost,
        shape,
        source: CocSource::Depth {
            map,
            focus: 1.0,
            range: 0.05,
        },
    }
}

/// Red subject, a textured green/blue background.
fn scene() -> Vec<[f32; 3]> {
    (0..N * N)
        .map(|i| {
            let (x, y) = (i % N, i / N);
            if inside(x, y) {
                [0.9, 0.05, 0.05]
            } else {
                let t = 0.15 + 0.1 * noise(x as u32, y as u32);
                [0.02, t, 0.3 - t]
            }
        })
        .collect()
}

#[test]
fn the_in_focus_subject_is_pixel_identical_and_the_background_blurs() {
    let g = geo(N as u32);
    let src = scene();
    for shape in [SHAPE_CIRCLE, SHAPE_BLADES, SHAPE_CAT_EYE] {
        let mut lin = src.clone();
        let lb = depth_plan(depth(0), 20.0, shape, 0.5);
        effects::lens_blur(&mut lin, N, N, &g, &lb);
        for y in 0..N {
            for x in 0..N {
                if inside(x, y) {
                    assert_eq!(lin[y * N + x], src[y * N + x], "shape {shape} at {x},{y}");
                }
            }
        }
        // The background's texture is gone far from the subject.
        let var = |img: &[[f32; 3]]| {
            let v: Vec<f32> = (0..40).map(|x| img[10 * N + x][1]).collect();
            let m = v.iter().sum::<f32>() / v.len() as f32;
            v.iter().map(|a| (a - m) * (a - m)).sum::<f32>() / v.len() as f32
        };
        assert!(
            var(&lin) < 0.1 * var(&src),
            "shape {shape}: background still sharp"
        );
    }
}

#[test]
fn the_subject_never_bleeds_into_the_background() {
    let g = geo(N as u32);
    let src = scene();
    let max_red = src
        .iter()
        .enumerate()
        .filter(|(i, _)| !inside(i % N, i / N))
        .map(|(_, p)| p[0])
        .fold(0.0f32, f32::max);
    let mut lin = src.clone();
    effects::lens_blur(
        &mut lin,
        N,
        N,
        &g,
        &depth_plan(depth(0), 24.0, SHAPE_CIRCLE, 1.0),
    );
    for y in 0..N {
        for x in 0..N {
            if !inside(x, y) {
                let r = lin[y * N + x][0];
                assert!(
                    r <= max_red + 1e-4,
                    "red {r} at {x},{y} next to the subject"
                );
            }
        }
    }
}

#[test]
fn a_near_object_out_of_focus_blurs_and_the_focused_background_stays() {
    // Focus on the background (depth 0): the textured square is in front and blurs.
    let g = geo(N as u32);
    let src: Vec<[f32; 3]> = (0..N * N)
        .map(|i| {
            let (x, y) = (i % N, i / N);
            let t = 0.3 + 0.25 * noise(x as u32, y as u32);
            if inside(x, y) {
                [t, 0.05, 0.05]
            } else {
                [0.05, t, 0.1]
            }
        })
        .collect();
    let mut lb = depth_plan(depth(0), 16.0, SHAPE_CIRCLE, 0.0);
    lb.source = CocSource::Depth {
        map: depth(0),
        focus: 0.0,
        range: 0.05,
    };
    assert!(lb.coc(0.5, 0.5, [1.0, 1.0]) < 0.0, "in front is negative");
    let mut lin = src.clone();
    effects::lens_blur(&mut lin, N, N, &g, &lb);
    // The in-focus background keeps its exact values, even right next to the object (an
    // approximation: a blurred foreground keeps its silhouette)...
    for (x, y) in [(10, 10), (58, 128), (200, 100)] {
        assert_eq!(lin[y * N + x], src[y * N + x], "background at {x},{y}");
    }
    // ...while the object's texture is blurred, with no background colour inside it.
    let var = |img: &[[f32; 3]]| {
        let v: Vec<f32> = (100..140).map(|x| img[128 * N + x][0]).collect();
        let m = v.iter().sum::<f32>() / v.len() as f32;
        v.iter().map(|a| (a - m) * (a - m)).sum::<f32>() / v.len() as f32
    };
    assert!(var(&lin) < 0.1 * var(&src));
    assert!((90..166).all(|x| lin[128 * N + x][1] < 0.051));
}

#[test]
fn highlights_bloom_into_brighter_bokeh_with_the_boost() {
    let g = geo(N as u32);
    // A dark far background with a few bright points.
    let src: Vec<[f32; 3]> = (0..N * N)
        .map(|i| {
            let (x, y) = (i % N, i / N);
            if x % 48 == 24 && y % 48 == 24 {
                [8.0; 3]
            } else {
                [0.03; 3]
            }
        })
        .collect();
    let flat = Arc::new(MaskBitmap::new(Size::new(8, 8), vec![0; 64]));
    let peak = |boost: f32| {
        let mut lin = src.clone();
        effects::lens_blur(
            &mut lin,
            N,
            N,
            &g,
            &depth_plan(flat.clone(), 12.0, SHAPE_CIRCLE, boost),
        );
        lin.iter().map(|p| p[1]).fold(0.0f32, f32::max)
    };
    let (plain, boosted) = (peak(0.0), peak(1.0));
    assert!(boosted > 1.5 * plain, "boost {boosted} vs plain {plain}");
}

/// Footprint of a blurred point: the pixels it lights above `t`, and the spread of those
/// pixels along x and y.
fn footprint(shape: u32, at: (usize, usize)) -> (usize, f32, f32) {
    let g = geo(N as u32);
    let src: Vec<[f32; 3]> = (0..N * N)
        .map(|i| {
            if (i % N, i / N) == at {
                [50.0; 3]
            } else {
                [0.0; 3]
            }
        })
        .collect();
    let flat = Arc::new(MaskBitmap::new(Size::new(8, 8), vec![0; 64]));
    let mut lin = src;
    effects::lens_blur(&mut lin, N, N, &g, &depth_plan(flat, 14.0, shape, 0.0));
    let lit: Vec<(f32, f32)> = (0..N * N)
        .filter(|&i| lin[i][0] > 1e-3)
        .map(|i| ((i % N) as f32, (i / N) as f32))
        .collect();
    let mean = |f: &dyn Fn(&(f32, f32)) -> f32| lit.iter().map(f).sum::<f32>() / lit.len() as f32;
    let (mx, my) = (mean(&|p| p.0), mean(&|p| p.1));
    (
        lit.len(),
        mean(&|p| (p.0 - mx).powi(2)),
        mean(&|p| (p.1 - my).powi(2)),
    )
}

#[test]
fn aperture_shapes_change_the_bokeh() {
    let (circle, cx, cy) = footprint(SHAPE_CIRCLE, (128, 128));
    let (blades, ..) = footprint(SHAPE_BLADES, (128, 128));
    assert!(
        (cx / cy - 1.0).abs() < 0.1,
        "a circle is round: {cx} vs {cy}"
    );
    assert!(
        blades < circle,
        "a hexagon covers less than its circumcircle"
    );
    // Toward a side, the cat's eye is squeezed along the radius (x here), not across it.
    let (_, ex, ey) = footprint(SHAPE_CAT_EYE, (240, 128));
    assert!(ex < 0.8 * ey, "cat's eye at the edge: {ex} vs {ey}");
    let (_, mx, my) = footprint(SHAPE_CAT_EYE, (128, 128));
    assert!(
        (mx / my - 1.0).abs() < 0.15,
        "round at the centre: {mx} vs {my}"
    );
}

#[test]
fn a_tilt_shift_band_stays_sharp_and_blurs_with_distance() {
    let g = geo(N as u32);
    let src = scene();
    let lb = LensBlurPlan {
        rmax: 12.0,
        f: 1,
        rlow: 12,
        off: [0, 0],
        boost: 0.0,
        shape: SHAPE_CIRCLE,
        source: CocSource::Plane {
            center: [0.5, 0.5],
            normal: [0.0, 1.0],
            width: 0.1,
            feather: 0.2,
        },
    };
    assert_eq!(lb.coc(0.3, 0.55, [1.0, 1.0]), 0.0);
    assert_eq!(lb.coc(0.3, 0.95, [1.0, 1.0]), 12.0);
    let mut lin = src.clone();
    effects::lens_blur(&mut lin, N, N, &g, &lb);
    for x in 0..N {
        assert_eq!(lin[128 * N + x], src[128 * N + x], "band row at {x}");
    }
    assert_ne!(lin[5 * N + 5], src[5 * N + 5]);
}

#[test]
fn coc_follows_the_depth_ramp_and_focal_range() {
    let lb = depth_plan(depth(0), 10.0, SHAPE_CIRCLE, 0.0);
    let CocSource::Depth { map, .. } = &lb.source else {
        unreachable!()
    };
    // Nearest of the four taps: the subject's edge stays at the subject's depth.
    assert_eq!(effects::depth_max4(map, 63.0 / 256.0, 0.5), 1.0);
    assert_eq!(effects::depth_max4(map, 0.1, 0.1), 0.0);
    let coc = |d: u8, range: f32| {
        let p = LensBlurPlan {
            source: CocSource::Depth {
                map: Arc::new(MaskBitmap::new(Size::new(1, 1), vec![d])),
                focus: 128.0 / 255.0,
                range,
            },
            ..lb.clone()
        };
        p.coc(0.5, 0.5, [1.0, 1.0])
    };
    assert_eq!(coc(128, 0.0), 0.0);
    assert_eq!(coc(133, 0.05), 0.0, "inside the focal range");
    assert!(coc(0, 0.0) > 9.9, "far: full positive blur");
    assert!(coc(255, 0.0) < -9.9, "near: full negative blur");
    let half = coc(198, 0.05);
    assert!((half + 5.0).abs() < 0.3, "linear in depth: {half}");
}

#[test]
fn auto_focus_reads_the_subject_or_the_nearest_depths() {
    let d = depth(40);
    let subject = MaskBitmap::new(
        Size::new(32, 32),
        (0..32 * 32)
            .map(|i| {
                if inside(i % 32 * 8, i / 32 * 8) {
                    255
                } else {
                    0
                }
            })
            .collect(),
    );
    assert!((effects::auto_focus(&d, Some(&subject)) - 1.0).abs() < 1e-6);
    // No subject (or an empty one): the 90th percentile of the map.
    let empty = MaskBitmap::new(Size::new(4, 4), vec![0; 16]);
    let ramp = MaskBitmap::new(Size::new(100, 1), (0..100u8).map(|v| v * 2).collect());
    assert_eq!(effects::auto_focus(&ramp, Some(&empty)), 178.0 / 255.0);
    assert_eq!(effects::auto_focus(&ramp, None), 178.0 / 255.0);
}

#[derive(Debug)]
struct Depth {
    depth: Option<Arc<MaskBitmap>>,
    subject: bool,
}

impl MaskSource for Depth {
    fn ai_mask(&self, t: AiTarget) -> Option<Arc<MaskBitmap>> {
        (self.subject && t == AiTarget::Subject).then(|| {
            Arc::new(MaskBitmap::new(
                Size::new(16, 16),
                (0..256)
                    .map(|i| {
                        if inside(i % 16 * 16, i / 16 * 16) {
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
        self.depth.clone()
    }
}

#[allow(clippy::unnecessary_wraps)] // the request field's type
fn masks(far: u8, subject: bool) -> Option<Arc<dyn MaskSource>> {
    Some(Arc::new(Depth {
        depth: Some(depth(far)),
        subject,
    }))
}

#[test]
fn plans_render_nothing_without_an_amount_or_a_depth_map() {
    let g = geo(1000);
    let mut adj = Adjustments::default();
    assert!(effects::plan_lens_blur(&adj, &g, masks(0, true).as_ref()).is_none());
    adj.effects.lens_blur.amount = 50.0;
    assert!(
        effects::plan_lens_blur(&adj, &g, None).is_none(),
        "depth mode, no masks"
    );
    let no_depth: Option<Arc<dyn MaskSource>> = Some(Arc::new(Depth {
        depth: None,
        subject: true,
    }));
    assert!(effects::plan_lens_blur(&adj, &g, no_depth.as_ref()).is_none());
    let p = effects::plan_lens_blur(&adj, &g, masks(0, true).as_ref()).unwrap();
    // 50% of 2.5% of 1000 px = 12.5 px: gathered at full resolution.
    assert_eq!((p.rmax, p.f, p.rlow), (12.5, 1, 13));
    let CocSource::Depth { focus, range, .. } = p.source else {
        unreachable!()
    };
    assert_eq!(focus, 1.0, "auto focus on the subject");
    assert!((range - 0.06).abs() < 1e-6);
    // A picked point focuses on the depth there; the plane needs no depth at all.
    adj.effects.lens_blur.focus = Some([0.02, 0.02]);
    adj.effects.lens_blur.amount = 100.0;
    let p = effects::plan_lens_blur(&adj, &g, masks(30, false).as_ref()).unwrap();
    assert!(matches!(p.source, CocSource::Depth { focus, .. } if focus == 30.0 / 255.0));
    assert_eq!((p.f, p.rlow), (2, 13));
    adj.effects.lens_blur.mode = BlurMode::Plane;
    adj.effects.lens_blur.shape = BokehShape::CatEye;
    let p = effects::plan_lens_blur(&adj, &g, None).unwrap();
    assert!(matches!(p.source, CocSource::Plane { .. }));
    assert_eq!(p.shape, SHAPE_CAT_EYE);
    // Under half a pixel: nothing.
    adj.effects.lens_blur.amount = 1.0;
    assert!(effects::plan_lens_blur(&adj, &geo(16), None).is_none());
}

fn blur_adj() -> Adjustments {
    let mut a = Adjustments::default();
    a.effects.lens_blur.amount = 70.0;
    a.effects.lens_blur.highlight_boost = 60.0;
    // No sharpening or noise reduction: they reach across the subject's edge.
    a.detail.sharpen_amount = 0.0;
    a.noise.luminance = 0.0;
    a.noise.color = 0.0;
    a
}

fn subject_raw() -> ds_core::RawImage {
    raw_from_fn(N as u32, N as u32, |x, y| {
        if inside(x as usize, y as usize) {
            [0.6, 0.08, 0.08]
        } else {
            let t = 0.2 + 0.12 * noise(x, y);
            [0.05, t, 0.3 - t]
        }
    })
}

fn render(adj: &Adjustments, req: &RenderRequest) -> RenderedImage {
    let raw = subject_raw();
    cpu_renderer().render(ImageKey(3), &raw, adj, req).unwrap()
}

#[test]
fn a_rendered_portrait_keeps_the_subject_and_blurs_the_rest() {
    let mut req = RenderRequest::new(Size::new(N as u32, N as u32));
    req.masks = masks(0, true);
    let blurred = render(&blur_adj(), &req);
    let mut off = blur_adj();
    off.effects.lens_blur.amount = 0.0;
    let sharp = render(&off, &req);
    let (w, _) = (sharp.size.width as usize, sharp.size.height as usize);
    let at = |img: &RenderedImage, x: usize, y: usize| {
        let i = (y * w + x) * 4;
        [img.rgba8[i], img.rgba8[i + 1], img.rgba8[i + 2]]
    };
    for y in 70..186 {
        for x in 70..186 {
            assert_eq!(at(&blurred, x, y), at(&sharp, x, y), "subject at {x},{y}");
        }
    }
    let rough = |img: &RenderedImage| {
        (1..60)
            .map(|x| (i32::from(at(img, x, 20)[1]) - i32::from(at(img, x - 1, 20)[1])).abs())
            .sum::<i32>()
    };
    assert!(rough(&blurred) * 4 < rough(&sharp), "background blurred");
    // Without a depth map nothing changes (the blur waits for the app's map).
    req.masks = None;
    assert_eq!(render(&blur_adj(), &req).rgba8, sharp.rgba8);
}

#[test]
fn region_renders_of_effects_match_the_full_frame() {
    let mut adj = blur_adj();
    adj.effects.glow.amount = 40.0;
    adj.effects.halation.amount = 60.0;
    adj.effects.grain.amount = 50.0;
    adj.detail.sharpen_amount = 140.0;
    let mut req = RenderRequest::new(Size::new(N as u32, N as u32));
    req.masks = masks(0, true);
    let full = render(&adj, &req);
    for (x, y) in [(0.0, 0.0), (0.5, 0.0), (0.0, 0.5), (0.5, 0.5)] {
        let mut r = req.clone();
        r.region = Some(NormRect {
            x,
            y,
            w: 0.5,
            h: 0.5,
        });
        let t = render(&adj, &r);
        let (tw, th) = (t.size.width as usize, t.size.height as usize);
        let (ox, oy) = ((x * N as f32) as usize, (y * N as f32) as usize);
        let mut worst = 0u8;
        for yy in 0..th {
            for xx in 0..tw {
                for c in 0..3 {
                    let a = t.rgba8[(yy * tw + xx) * 4 + c];
                    let b = full.rgba8[((oy + yy) * N + ox + xx) * 4 + c];
                    worst = worst.max(a.abs_diff(b));
                }
            }
        }
        assert!(worst <= 1, "tile ({x}, {y}) differs by {worst}");
    }
}

#[test]
fn glow_lifts_and_halation_warms_around_highlights() {
    let src: Vec<[f32; 3]> = (0..64 * 64)
        .map(|i| if (i % 64) < 32 { [0.95; 3] } else { [0.2; 3] })
        .collect();
    let off = FxPlan::default();
    let mut same = src.clone();
    effects::apply_fx(&mut same, 64, 64, &off);
    assert_eq!(same, src, "no overlay, no change");
    let mut adj = Adjustments::default();
    adj.effects.glow.amount = 80.0;
    let fx = effects::plan_fx(&adj, 64.0);
    let mut glowing = src.clone();
    effects::apply_fx(&mut glowing, 64, 64, &fx);
    // Screening never darkens; the dark side next to the bright one lifts most.
    assert!(glowing.iter().zip(&src).all(|(a, b)| a[1] >= b[1] - 1e-6));
    assert!(glowing[10 * 64 + 34][1] > glowing[10 * 64 + 60][1]);
    assert!(glowing[10 * 64 + 34][1] > 0.21);
    let mut adj = Adjustments::default();
    adj.effects.halation.amount = 100.0;
    let fx = effects::plan_fx(&adj, 1000.0);
    let mut warm = src.clone();
    effects::apply_fx(&mut warm, 64, 64, &fx);
    let p = warm[10 * 64 + 33];
    let d = [p[0] - 0.2, p[1] - 0.2, p[2] - 0.2];
    assert!(
        d[0] > 0.01 && d[0] > 2.0 * d[1] && d[1] > d[2],
        "red-orange: {d:?}"
    );
    assert_eq!(warm[10 * 64 + 63], src[10 * 64 + 63], "far from highlights");
}

#[test]
fn grain_is_fixed_to_the_image_and_scales_with_its_settings() {
    let mut adj = Adjustments::default();
    assert!(effects::plan_grain(&adj, 6000.0).is_none());
    adj.effects.grain.amount = 100.0;
    adj.effects.grain.size = 100.0;
    let big = effects::plan_grain(&adj, 6000.0).unwrap();
    assert!((big.cell - 3.7).abs() < 1e-4 && (big.amp - 0.12).abs() < 1e-6);
    // Finer than a pixel at a small size: weaker, as a downsized export shows it.
    let small = effects::plan_grain(&adj, 600.0).unwrap();
    assert!(small.amp < 0.5 * big.amp);
    let gray = [0.5f32; 3];
    let a = effects::grain_px(gray, [10.3, 20.7], &big);
    assert_eq!(
        a,
        effects::grain_px(gray, [10.3, 20.7], &big),
        "deterministic"
    );
    assert_eq!(a[0], a[1], "monochrome");
    // Zero mean, bounded, and stronger in the midtones than near black.
    let samples: Vec<f32> = (0..4000)
        .map(|i| effects::grain_px(gray, [i as f32 * 1.37, (i / 61) as f32 * 2.11], &big)[0] - 0.5)
        .collect();
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let sd = (samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32).sqrt();
    assert!(mean.abs() < 0.01 && sd > 0.02 && samples.iter().all(|v| v.abs() < 0.25));
    let dark: f32 = (0..4000)
        .map(|i| (effects::grain_px([0.01; 3], [i as f32 * 1.37, 3.0], &big)[0] - 0.01).abs())
        .sum();
    let mid: f32 = (0..4000)
        .map(|i| (effects::grain_px(gray, [i as f32 * 1.37, 3.0], &big)[0] - 0.5).abs())
        .sum();
    assert!(dark < 0.6 * mid);
    // Hash and noise agree with the lattice at integer positions.
    assert_eq!(effects::value_noise(3.0, 4.0, 1), effects::lattice(3, 4, 1));
    assert!((-1.0..=1.0).contains(&effects::lattice(-7, 9, 2)));
}

#[test]
fn draft_renders_with_effects_upscale_with_grain_on_the_image() {
    let mut adj = blur_adj();
    adj.effects.grain.amount = 60.0;
    let mut req = RenderRequest::new(Size::new(N as u32, N as u32));
    req.masks = masks(0, true);
    let preview = render(&adj, &req);
    req.quality = crate::Quality::Draft;
    let draft = render(&adj, &req);
    assert_eq!(draft.size, preview.size);
    // The heavy-edit draft size counts the effects.
    let mut busy = adj.clone();
    busy.effects.glow.amount = 10.0;
    busy.presence.texture = 10.0;
    busy.spots.push(ds_core::adjust::Spot::default());
    busy.layers.push(ds_core::adjust::Layer {
        id: 1,
        name: "l".into(),
        enabled: true,
        opacity: 1.0,
        adjustments: ds_core::adjust::LayerAdjustments::default(),
        mask: ds_core::adjust::Mask::default(),
    });
    assert_eq!(
        crate::plan::draft_long_edge(&busy),
        crate::DRAFT_LONG_EDGE_HEAVY
    );
}

/// The final review's tile check: region renders match the full frame at the largest
/// blur, glow and halation radii (tiles must read their whole neighbourhood).
#[test]
fn review_tiles_match_full_frame_at_large_radii() {
    let n = 512u32;
    let raw = raw_from_fn(n, n, |x, y| {
        let t = 0.2 + 0.6 * noise(x / 3, y / 3);
        if (x / 64 + y / 64) % 2 == 0 {
            [t, t * 0.8, 0.3]
        } else {
            [1.4, 1.3, 1.1]
        }
    });
    let mut adj = Adjustments::default();
    adj.effects.lens_blur.amount = 100.0;
    adj.effects.lens_blur.mode = BlurMode::Plane;
    adj.effects.lens_blur.highlight_boost = 100.0;
    adj.effects.glow.amount = 100.0;
    adj.effects.glow.size = 100.0;
    adj.effects.halation.amount = 100.0;
    let req = RenderRequest::new(Size::new(n, n));
    let full = cpu_renderer()
        .render(ImageKey(9), &raw, &adj, &req)
        .unwrap();
    let mut worst_all = 0u8;
    for (x, y) in [(0.25, 0.25), (0.5, 0.25), (0.25, 0.5), (0.375, 0.375)] {
        let mut r = req.clone();
        r.region = Some(NormRect {
            x,
            y,
            w: 0.25,
            h: 0.25,
        });
        let t = cpu_renderer().render(ImageKey(9), &raw, &adj, &r).unwrap();
        let (tw, th) = (t.size.width as usize, t.size.height as usize);
        let (ox, oy) = ((x * n as f32) as usize, (y * n as f32) as usize);
        let mut worst = 0u8;
        for yy in 0..th {
            for xx in 0..tw {
                for c in 0..3 {
                    let a = t.rgba8[(yy * tw + xx) * 4 + c];
                    let b = full.rgba8[((oy + yy) * n as usize + ox + xx) * 4 + c];
                    worst = worst.max(a.abs_diff(b));
                }
            }
        }
        eprintln!("tile ({x},{y}) {tw}x{th} worst {worst}");
        worst_all = worst_all.max(worst);
    }
    assert!(worst_all <= 1, "worst {worst_all}");
}
