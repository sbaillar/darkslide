//! Content-aware removal on synthetic sensor data: placement through every frame
//! transform, the fill, the grain, caching and the model seam.
#![allow(clippy::float_cmp)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ds_core::adjust::{BrushStroke, Removal};
use ds_core::{Adjustments, CfaPattern, NormRect, Orientation, RawImage, Size};

use super::*;
use crate::testutil::{noise, raw_from_fn};
use crate::{CancelToken, Renderer, cpu_renderer};

/// A smooth, slightly noisy background with a bright red disc (the "object") at sensor
/// `(cx, cy)`, radius `r`.
fn scene(w: u32, h: u32, cx: f32, cy: f32, r: f32, object: bool) -> RawImage {
    raw_from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        if object && (fx - cx).hypot(fy - cy) < r {
            return [0.9, 0.1, 0.1];
        }
        let g = 0.25 + 0.15 * fx / w as f32 + 0.004 * noise(x, y);
        [g, g * 1.05, g * 0.9]
    })
}

/// One removal stroke: a dot at E-normalized `p` with diameter `size` (long-edge units).
fn dot(p: [f32; 2], size: f32) -> Removal {
    Removal {
        strokes: vec![BrushStroke {
            points: vec![p],
            size,
            ..BrushStroke::default()
        }],
        feather: 0.004,
    }
}

fn with(removals: Vec<Removal>) -> Adjustments {
    Adjustments {
        removals,
        ..Adjustments::default()
    }
}

/// E-normalized position of sensor point `(sx, sy)` for `raw`'s orientation.
fn e_norm(raw: &RawImage, sx: f64, sy: f64) -> [f32; 2] {
    let (sw, sh) = (f64::from(raw.size.width), f64::from(raw.size.height));
    let inv = geometry::orientation_to_stored(raw.orientation, sw, sh)
        .inverse()
        .unwrap();
    let (ex, ey) = inv.apply(sx, sy);
    let e = raw.orientation.apply_to(raw.size);
    [
        (ex / f64::from(e.width)) as f32,
        (ey / f64::from(e.height)) as f32,
    ]
}

/// Mean normalized red-site value inside the disc (sensor), and the max change outside
/// a margin around it.
fn disc_stats(a: &RawImage, b: &RawImage, cx: f32, cy: f32, r: f32) -> (f32, u16) {
    let w = a.size.width;
    let (mut sum, mut n, mut far) = (0.0f32, 0.0f32, 0u16);
    for y in 0..a.size.height {
        for x in 0..w {
            let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
            let i = (y * w + x) as usize;
            if d < r && CfaPattern::Rggb.color_at(x, y) == Some(0) {
                sum += (f32::from(b.data[i]) - 256.0) / (16_383.0 - 256.0) * 2.0;
                n += 1.0;
            }
            if d > r * 3.0 {
                far = far.max(a.data[i].abs_diff(b.data[i]));
            }
        }
    }
    (sum / n, far)
}

fn rq() -> RenderRequest {
    RenderRequest::new(Size::new(400, 400))
}

#[test]
fn nothing_to_remove_renders_the_original() {
    let raw = scene(64, 48, 30.0, 20.0, 5.0, true);
    let r = rq();
    assert!(
        resolve(ImageKey(1), &raw, &Adjustments::default(), &r)
            .unwrap()
            .is_none()
    );
    let mut erase_only = dot([0.5, 0.5], 0.1);
    erase_only.strokes[0].erase = true;
    let mut nan = dot([f32::NAN, 0.5], 0.1);
    nan.strokes.push(BrushStroke {
        points: vec![[0.5, 0.5]],
        size: 0.0,
        ..BrushStroke::default()
    });
    let empty = Removal::default();
    assert!(
        resolve(ImageKey(1), &raw, &with(vec![erase_only, nan, empty]), &r)
            .unwrap()
            .is_none()
    );
}

/// The object under the stroke is replaced by background, nothing far from it changes,
/// for every EXIF orientation (strokes are E-frame, the fill is sensor-space).
#[test]
fn the_fill_lands_on_the_object_in_every_orientation() {
    let (cx, cy, r) = (100.0, 80.0, 12.0);
    for (i, o) in [
        Orientation::Normal,
        Orientation::FlipH,
        Orientation::Rotate180,
        Orientation::FlipV,
        Orientation::Transpose,
        Orientation::Rotate90,
        Orientation::Transverse,
        Orientation::Rotate270,
    ]
    .into_iter()
    .enumerate()
    {
        let mut raw = scene(256, 192, cx, cy, r, true);
        raw.orientation = o;
        let p = e_norm(&raw, f64::from(cx), f64::from(cy));
        let adj = with(vec![dot(p, 34.0 / 256.0)]);
        let key = ImageKey(1000 + i as u64);
        let (dk, out) = resolve(key, &raw, &adj, &rq()).unwrap().unwrap();
        assert_ne!(dk, key);
        let (red, far) = disc_stats(&raw, &out, cx, cy, r);
        assert!(red < 0.45, "{o:?}: object still there (red {red})");
        assert_eq!(far, 0, "{o:?}: pixels far from the removal changed");
    }
}

/// A removal renders in place under crop, straighten and a quarter turn: the developed
/// frame matches a frame that never had the object (much closer than with it).
#[test]
fn removal_placement_survives_crop_and_rotation() {
    let (cx, cy, r) = (150.0, 90.0, 10.0);
    let with_obj = scene(240, 160, cx, cy, r, true);
    let clean = scene(240, 160, cx, cy, r, false);
    let mut adj = with(vec![dot(
        e_norm(&with_obj, f64::from(cx), f64::from(cy)),
        30.0 / 240.0,
    )]);
    adj.lens.enabled = false;
    adj.crop.rect = NormRect {
        x: 0.4,
        y: 0.2,
        w: 0.55,
        h: 0.7,
    };
    adj.crop.angle_deg = 8.0;
    adj.crop.quarter_turns = 1;
    let mut plain = adj.clone();
    plain.removals.clear();
    let r = RenderRequest::new(Size::new(200, 200));
    let mut cpu = cpu_renderer();
    let removed = cpu.render(ImageKey(2001), &with_obj, &adj, &r).unwrap();
    let truth = cpu.render(ImageKey(2002), &clean, &plain, &r).unwrap();
    let object = cpu.render(ImageKey(2003), &with_obj, &plain, &r).unwrap();
    let diff = |a: &crate::RenderedImage, b: &crate::RenderedImage| -> f64 {
        a.rgba8
            .iter()
            .zip(&b.rgba8)
            .map(|(x, y)| f64::from(x.abs_diff(*y)))
            .sum::<f64>()
            / a.rgba8.len() as f64
    };
    let (d_removed, d_object) = (diff(&removed, &truth), diff(&object, &truth));
    assert!(d_object > 0.5, "the object is in the crop: {d_object}");
    assert!(d_removed < d_object * 0.25, "{d_removed} vs {d_object}");
}

/// Keystone and lens geometry move the footprint: strokes are mapped with the optics.
#[test]
fn strokes_map_through_the_optics() {
    let raw = scene(300, 200, 0.0, 0.0, 0.0, false);
    let mut adj = with(vec![dot([0.8, 0.25], 0.05)]);
    adj.lens.keystone_vertical = 20.0;
    let geo = removal_geometry(&raw, &adj, &rq()).unwrap();
    assert!(geo.keystone.is_some());
    let caps = capsules(&adj.removals[0].strokes, &geo);
    let want = geo.e_to_sensor_green(f64::from(0.8f32) * 300.0, f64::from(0.25f32) * 200.0);
    assert!((caps[0].a[0] - want.0).abs() < 1e-9 && (caps[0].a[1] - want.1).abs() < 1e-9);
    assert!(
        (want.0 - 240.0).abs() > 1.0 || (want.1 - 50.0).abs() > 1.0,
        "keystone moved it"
    );
    // The radius follows the local scale of the keystone (not 7.5 px exactly).
    assert!((caps[0].ra - 7.5).abs() > 1e-3 && (caps[0].ra - 7.5).abs() < 3.0);
    let fp = footprint(&caps, 0.0, [0, 0, 300, 200], [0, 0, 300, 200]).unwrap();
    let c = (
        f64::from(fp.rect[0]) + f64::from(fp.rect[2]) / 2.0,
        f64::from(fp.rect[1]) + f64::from(fp.rect[3]) / 2.0,
    );
    assert!(
        (c.0 - want.0).abs() < 2.0 && (c.1 - want.1).abs() < 2.0,
        "{c:?} {want:?}"
    );
    // Changing the keystone changes the chain (the fill is recomputed).
    let mut other = adj.clone();
    other.lens.keystone_vertical = 10.0;
    let g2 = removal_geometry(&raw, &other, &rq()).unwrap();
    assert_ne!(chain_seed(&geo, "x"), chain_seed(&g2, "x"));
}

#[test]
fn footprint_feathers_erases_and_clips() {
    let cap = |a: [f64; 2], b: [f64; 2], r: f64, erase: bool| Capsule {
        a,
        b,
        ra: r,
        rb: r,
        erase,
    };
    // A stroke from (20, 20) to (60, 20), radius 5, with an erased middle.
    let caps = [
        cap([20.0, 20.0], [60.0, 20.0], 5.0, false),
        cap([40.0, 20.0], [40.0, 20.0], 12.0, true),
    ];
    let fp = footprint(&caps, 4.0, [0, 0, 100, 100], [0, 0, 100, 100]).unwrap();
    let a = |x: u32, y: u32| fp_alpha(&fp, x, y);
    assert_eq!(a(25, 20), 1.0);
    assert_eq!(a(40, 20), 0.0, "erased");
    assert!(
        a(25, 27) > 0.0 && a(25, 27) < 1.0,
        "feather band: {}",
        a(25, 27)
    );
    assert_eq!(a(25, 40), 0.0);
    assert_eq!(a(99, 99), 0.0, "outside the rect");
    // Clipped to the image area; nothing left when the stroke is outside it.
    let edge = footprint(
        &[cap([2.0, 2.0], [2.0, 2.0], 6.0, false)],
        0.0,
        [0, 0, 50, 50],
        [0, 0, 50, 50],
    )
    .unwrap();
    assert_eq!((edge.rect[0], edge.rect[1]), (0, 0));
    assert!(
        footprint(
            &[cap([200.0, 2.0], [200.0, 2.0], 6.0, false)],
            0.0,
            [0, 0, 50, 50],
            [0, 0, 50, 50]
        )
        .is_none()
    );
    assert!(
        footprint(
            &[cap([20.0, 20.0], [20.0, 20.0], 3.0, true)],
            0.0,
            [0, 0, 50, 50],
            [0, 0, 50, 50]
        )
        .is_none()
    );
    // Fully erased paint has no footprint.
    assert!(
        footprint(
            &[
                cap([20.0, 20.0], [20.0, 20.0], 3.0, false),
                cap([20.0, 20.0], [20.0, 20.0], 9.0, true)
            ],
            0.0,
            [0, 0, 50, 50],
            [0, 0, 50, 50]
        )
        .is_none()
    );
}

#[test]
fn chamfer_approximates_euclidean_distance() {
    let (w, h) = (21, 21);
    let mut set = vec![false; w * h];
    set[10 * w + 10] = true;
    let d = chamfer(&set, w, h);
    assert_eq!(d[10 * w + 10], 0.0);
    assert_eq!(d[10 * w + 15], 5.0);
    let diag = d[15 * w + 15];
    assert!((diag - 50f32.sqrt()).abs() < 0.2, "{diag}");
    let knight = d[12 * w + 15];
    assert!((knight - 29f32.sqrt()).abs() < 0.5, "{knight}");
}

#[test]
fn crops_are_square_with_context_and_inside_the_area() {
    let crop = |fp| crop_rect(fp, [0, 0, 6000, 4000], object_margin(fp));
    let c = crop([500, 400, 40, 20]);
    assert_eq!(c[2], c[3], "square");
    assert!(c[2] >= 40 + 2 * MIN_CONTEXT);
    assert!(c[0] <= 500 && c[0] + c[2] >= 540 && c[1] <= 400 && c[1] + c[3] >= 420);
    // At a corner it shifts inside; a crop larger than the area is clipped to it.
    let corner = crop_rect([0, 0, 30, 30], [10, 10, 200, 100], MIN_CONTEXT);
    assert_eq!((corner[0], corner[1]), (10, 10));
    let big = crop_rect([0, 0, 180, 90], [0, 0, 200, 100], 90);
    assert_eq!(big, [0, 0, 200, 100]);
}

#[test]
fn orientation_maps_are_permutations() {
    let raw = scene(40, 30, 0.0, 0.0, 0.0, false);
    for o in [
        Orientation::Normal,
        Orientation::Rotate90,
        Orientation::Transverse,
        Orientation::FlipV,
    ] {
        let mut r = raw.clone();
        r.orientation = o;
        let geo = removal_geometry(&r, &Adjustments::default(), &rq()).unwrap();
        let crop = [5, 7, 12, 9];
        let (ow, oh, map) = orient_map(&geo.e_to_sensor, crop);
        assert_eq!(ow * oh, 12 * 9);
        let mut seen = map.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), map.len(), "{o:?}: a bijection");
        if matches!(o, Orientation::Rotate90 | Orientation::Transverse) {
            assert_eq!((ow, oh), (9, 12));
        }
    }
}

#[test]
fn the_noise_model_is_fitted_from_the_samples() {
    // Exact points on σ² = 2e-4·v + 1e-6, plus textured outliers above them.
    let mut pts = Vec::new();
    for i in 0..200 {
        let v = 0.05 + 0.8 * i as f32 / 200.0;
        let s2 = 2e-4 * v + 1e-6;
        pts.push((v, s2));
        pts.push((v, s2 * 5.0));
    }
    let (a, b) = fit_noise(pts);
    assert!((a - 2e-4).abs() < 3e-5, "{a}");
    assert!(b < 2e-5, "{b}");
    assert_eq!(fit_noise(vec![]).0, 0.0);
    let (a, b) = fit_noise(vec![(0.2, 4e-6), (0.3, 9e-6)]);
    assert_eq!((a, b), (0.0, 4e-6));
}

/// The fill carries the sensor's grain: same-colour neighbour differences inside the
/// fill spread like those around it (not a smooth, plastic patch).
#[test]
fn the_fill_has_the_cameras_grain() {
    let (w, h) = (200u32, 160u32);
    let sigma = 0.01f32;
    let raw = raw_from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32 - 100.0, y as f32 - 80.0);
        if fx.hypot(fy) < 15.0 {
            return [0.8, 0.8, 0.1];
        }
        // Per-site pseudo-Gaussian noise (sum of two uniforms).
        let n = f32::midpoint(noise(x, y), noise(y + 7, x + 3)) * sigma * 2.45;
        [0.4 + n; 3]
    });
    let adj = with(vec![dot([0.5, 0.5], 44.0 / 200.0)]);
    let (_, out) = resolve(ImageKey(3001), &raw, &adj, &rq()).unwrap().unwrap();
    let spread = |r: &RawImage, inside: bool| {
        let mut d = Vec::new();
        for y in (20..140u32).step_by(2) {
            for x in (20..176u32).step_by(2) {
                let rr = (x as f32 - 100.0).hypot(y as f32 - 80.0);
                if (inside && rr < 12.0) || (!inside && rr > 40.0) {
                    let i = (y * w + x) as usize;
                    d.push((f32::from(r.data[i]) - f32::from(r.data[i + 2])).abs());
                }
            }
        }
        d.sort_by(f32::total_cmp);
        d[d.len() / 2]
    };
    let (fill, around) = (spread(&out, true), spread(&raw, false));
    assert!(
        fill > around * 0.6 && fill < around * 1.6,
        "{fill} vs {around}"
    );
}

/// Counts calls; fills with mid gray; optionally returns a wrong size.
struct FakeModel {
    calls: AtomicUsize,
    id: &'static str,
    broken: bool,
}

impl Inpainter for FakeModel {
    fn id(&self) -> String {
        self.id.into()
    }
    fn max_edge(&self) -> u32 {
        32
    }
    fn inpaint(&self, w: u32, h: u32, rgb: &[u8], mask: &[u8]) -> Option<Vec<u8>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(w.max(h) <= 32);
        assert_eq!(
            (rgb.len(), mask.len()),
            ((w * h * 3) as usize, (w * h) as usize)
        );
        assert!(mask.contains(&255) && mask.contains(&0));
        if self.broken {
            return Some(vec![0; 5]);
        }
        Some(vec![128; rgb.len()])
    }
}

fn model(id: &'static str, broken: bool) -> Arc<FakeModel> {
    Arc::new(FakeModel {
        calls: AtomicUsize::new(0),
        id,
        broken,
    })
}

/// The model fills the hole (its gray, back in camera units, at the sensor); concurrent
/// renders of a new removal call it once; a model answer of the wrong size falls back
/// to `PatchMatch`.
#[test]
fn the_model_is_called_once_and_its_fill_is_used() {
    let raw = scene(256, 192, 120.0, 90.0, 12.0, true);
    let adj = with(vec![dot(e_norm(&raw, 120.0, 90.0), 34.0 / 256.0)]);
    let m = model("fake-gray", false);
    let req = RenderRequest {
        inpainter: Some(m.clone()),
        ..rq()
    };
    let outs: Vec<_> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..4)
            .map(|_| s.spawn(|| resolve(ImageKey(4001), &raw, &adj, &req).unwrap().unwrap()))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(m.calls.load(Ordering::SeqCst), 1, "computed once");
    assert!(
        outs.windows(2)
            .all(|p| p[0].0 == p[1].0 && p[0].1.data == p[1].1.data)
    );
    // Gray 128 → the same level on every channel's site in the hole (up to grain).
    let out = &outs[0].1;
    let (red, _) = disc_stats(&raw, out, 120.0, 90.0, 8.0);
    assert!(red < 0.6, "{red}");
    // A model replacing the PatchMatch fallback is a different fill (new chain).
    let plain = resolve(ImageKey(4001), &raw, &adj, &rq()).unwrap().unwrap();
    assert_ne!(plain.0, outs[0].0);
    let bad = model("fake-broken", true);
    let req = RenderRequest {
        inpainter: Some(bad.clone()),
        ..rq()
    };
    let (_, fell_back) = resolve(ImageKey(4001), &raw, &adj, &req).unwrap().unwrap();
    assert_eq!(bad.calls.load(Ordering::SeqCst), 1);
    assert!(disc_stats(&raw, &fell_back, 120.0, 90.0, 8.0).0 < 0.45);
}

/// Fills are cached per (image, removal chain): the same edit reuses the derived RAW,
/// edits elsewhere in the stack don't recompute, a new removal reuses the earlier ones'
/// patches, and `forget` drops it all.
#[test]
fn the_cache_key_follows_the_chain() {
    let raw = scene(256, 192, 100.0, 80.0, 10.0, true);
    let key = ImageKey(5001);
    let m = model("fake-cache", false);
    let req = RenderRequest {
        inpainter: Some(m.clone()),
        ..rq()
    };
    let first = dot(e_norm(&raw, 100.0, 80.0), 30.0 / 256.0);
    let mut adj = with(vec![first.clone()]);
    let (k1, r1) = resolve(key, &raw, &adj, &req).unwrap().unwrap();
    let (k1b, r1b) = resolve(key, &raw, &adj, &req).unwrap().unwrap();
    assert!(k1 == k1b && Arc::ptr_eq(&r1, &r1b));
    // Exposure, crop, layers: same chain.
    adj.exposure.exposure_ev = 1.0;
    adj.crop.angle_deg = 5.0;
    assert_eq!(resolve(key, &raw, &adj, &req).unwrap().unwrap().0, k1);
    assert_eq!(m.calls.load(Ordering::SeqCst), 1);
    // A second removal: only it is computed.
    adj.removals.push(dot([0.2, 0.3], 0.05));
    let (k2, _) = resolve(key, &raw, &adj, &req).unwrap().unwrap();
    assert_ne!(k2, k1);
    assert_eq!(m.calls.load(Ordering::SeqCst), 2);
    // Editing the first removal recomputes both (the second sees the first's fill).
    adj.removals[0].feather = 0.01;
    let (k3, _) = resolve(key, &raw, &adj, &req).unwrap().unwrap();
    assert!(k3 != k2 && k3 != k1);
    assert_eq!(m.calls.load(Ordering::SeqCst), 4);
    // Another image with the same edit is its own fill.
    assert_ne!(
        resolve(ImageKey(5002), &raw, &adj, &req)
            .unwrap()
            .unwrap()
            .0,
        k3
    );
    assert_eq!(m.calls.load(Ordering::SeqCst), 6);
    let derived = forget(key);
    assert!(derived.contains(&k1) && derived.contains(&k2) && derived.contains(&k3));
    assert_eq!(forget(key).len(), 0);
    let _ = resolve(key, &raw, &adj, &req).unwrap();
    assert_eq!(m.calls.load(Ordering::SeqCst), 8, "forgotten: recomputed");
    forget(ImageKey(5002));
}

#[test]
fn renderers_evict_derived_images_with_the_original() {
    let raw = scene(128, 96, 60.0, 40.0, 6.0, true);
    let adj = with(vec![dot(e_norm(&raw, 60.0, 40.0), 0.15)]);
    let mut cpu = cpu_renderer();
    let r = RenderRequest::new(Size::new(64, 64));
    cpu.render(ImageKey(6001), &raw, &adj, &r).unwrap();
    assert_eq!(cpu.cached_bases(), 1);
    cpu.evict(ImageKey(6001));
    assert_eq!(cpu.cached_bases(), 0, "the derived image's BASE went too");
}

#[test]
fn a_cancelled_render_stops_before_filling() {
    let raw = scene(128, 96, 60.0, 40.0, 6.0, true);
    let adj = with(vec![dot(e_norm(&raw, 60.0, 40.0), 0.15)]);
    let c = CancelToken::new();
    c.cancel();
    let req = RenderRequest {
        cancel: Some(c),
        ..rq()
    };
    assert!(matches!(
        resolve(ImageKey(7001), &raw, &adj, &req),
        Err(PipelineError::Cancelled)
    ));
}

/// Linear RGB sensors (three samples per pixel) are filled per channel.
#[test]
fn linear_rgb_raws_are_filled_per_channel() {
    let (w, h) = (160u32, 120u32);
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let obj = (x as f32 - 80.0).hypot(y as f32 - 60.0) < 10.0;
            for c in 0..3 {
                let v = if obj {
                    if c == 0 { 0.9 } else { 0.05 }
                } else {
                    0.3 + 0.003 * noise(x * 3 + c, y)
                };
                data.push((v * 4000.0) as u16);
            }
        }
    }
    let raw = RawImage {
        size: Size::new(w, h),
        cfa: CfaPattern::LinearRgb,
        data,
        black_level: [0.0; 4],
        white_level: 4000.0,
        wb_as_shot: [1.0; 3],
        cam_to_xyz: crate::color::REC2020_TO_XYZ,
        wb_daylight: [1.0; 3],
        orientation: Orientation::Normal,
        default_crop: None,
    };
    let adj = with(vec![dot([0.5, 0.5], 30.0 / 160.0)]);
    let (_, out) = resolve(ImageKey(8001), &raw, &adj, &rq()).unwrap().unwrap();
    let i = ((60 * w + 80) * 3) as usize;
    let px = [0, 1, 2].map(|c| f32::from(out.data[i + c]) / 4000.0);
    assert!(
        (px[0] - 0.3).abs() < 0.06 && (px[1] - 0.3).abs() < 0.06,
        "{px:?}"
    );
}

#[test]
fn patches_apply_clip_and_persist() {
    let mut raw = scene(8, 6, 0.0, 0.0, 0.0, false);
    let p = Patch {
        rect: [6, 4, 3, 3],
        chans: 1,
        data: (0..9).collect(),
    };
    p.apply(&mut raw);
    assert_eq!(raw.data[4 * 8 + 6], 0);
    assert_eq!(raw.data[4 * 8 + 7], 1);
    assert_eq!(raw.data[5 * 8 + 7], 4, "clipped to the sensor");
    let before = raw.data.clone();
    Patch {
        chans: 3,
        ..p.clone()
    }
    .apply(&mut raw);
    Patch {
        data: vec![1],
        ..p.clone()
    }
    .apply(&mut raw);
    assert_eq!(raw.data, before, "malformed patches are ignored");
    let p = Patches(vec![
        p,
        Patch {
            rect: [0, 0, 2, 1],
            chans: 1,
            data: vec![7, 8],
        },
    ]);
    assert_eq!(Patches::decode(&p.encode()), Some(p.clone()));
    let mut bad = p.encode();
    bad.pop();
    assert!(Patches::decode(&bad).is_none(), "short");
    bad = p.encode();
    bad.push(0);
    assert!(Patches::decode(&bad).is_none(), "trailing bytes");
    bad = p.encode();
    bad[4] = 9;
    assert!(Patches::decode(&bad).is_none(), "other version");
    assert!(Patches::decode(b"nope").is_none());
    assert_eq!(
        Patches::decode(&Patches::default().encode()),
        Some(Patches::default())
    );
    let mut two = raw.clone();
    p.apply(&mut two);
    assert_eq!((two.data[0], two.data[1]), (7, 8));

    let t = std::env::temp_dir().join(format!("ds-removal-{}", std::process::id()));
    let k = (1, 2);
    disk_write(Some(&t), k, &p);
    assert_eq!(disk_read(Some(&t), k), Some(p.clone()));
    assert!(disk_read(Some(&t), (1, 3)).is_none());
    assert!(disk_read(None, k).is_none());
    disk_write(None, k, &p);
    // A file in the way of the folder: the write fails quietly.
    let blocked = t.join("file");
    std::fs::write(&blocked, b"x").unwrap();
    disk_write(Some(&blocked), k, &p);
    assert!(disk_read(Some(&blocked), k).is_none());
    let _ = std::fs::remove_dir_all(&t);
}

#[test]
fn patch_store_is_bounded_and_lru() {
    let mut s = Store::default();
    let big = |n: u16| {
        Arc::new(Patches(vec![Patch {
            rect: [0, 0, 1, 1],
            chans: 1,
            data: vec![n; (PATCH_BYTES / 2 / 3) as usize],
        }]))
    };
    s.put_patch((1, 1), big(1));
    s.put_patch((1, 2), big(2));
    assert!(s.patch((1, 1)).is_some(), "touch 1");
    s.put_patch((1, 3), big(3));
    s.put_patch((1, 4), big(4));
    assert!(s.patch((1, 2)).is_none(), "least recently used went first");
    assert!(s.bytes <= PATCH_BYTES);
    s.put_patch((1, 4), big(5));
    assert_eq!(s.patch((1, 4)).unwrap().0[0].data[0], 5, "replaced");
}

#[test]
fn derived_keys_differ_from_their_image() {
    for k in 0..64u64 {
        for c in [0u64, 1, u64::MAX] {
            assert_ne!(derived_key(ImageKey(k), c), ImageKey(k));
        }
    }
    assert_ne!(derived_key(ImageKey(1), 2), derived_key(ImageKey(1), 3));
}

/// Detail transfer adds texture from the known area into a smooth upscaled fill.
#[test]
fn detail_transfer_adds_texture_to_the_fill() {
    let (w, h) = (96, 96);
    let base: Vec<[f32; 3]> = (0..w * h)
        .map(|i| {
            let v = 0.3 + 0.08 * (((i % w) / 3 + (i / w) / 3) % 2) as f32;
            [v; 3]
        })
        .collect();
    let hole: Vec<bool> = (0..w * h)
        .map(|i| (32..64).contains(&(i % w)) && (32..64).contains(&(i / w)))
        .collect();
    let mut fill = vec![[0.34f32; 3]; w * h];
    transfer_detail(&mut fill, &base, &hole, w, h, 4.0, 1);
    let inside: Vec<f32> = (0..w * h)
        .filter(|&i| hole[i])
        .map(|i| fill[i][0])
        .collect();
    let mean = inside.iter().sum::<f32>() / inside.len() as f32;
    let var = inside.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / inside.len() as f32;
    assert!(var.sqrt() > 0.004, "texture added: {}", var.sqrt());
    assert!((mean - 0.34).abs() < 0.02, "tone kept: {mean}");
    assert!((0..w * h).filter(|&i| !hole[i]).all(|i| fill[i][0] == 0.34));
    // No transfer when the model ran at (about) full resolution.
    let mut flat = vec![[0.34f32; 3]; w * h];
    transfer_detail(&mut flat, &base, &hole, w, h, 1.2, 1);
    assert!(flat.iter().all(|v| v[0] == 0.34));
}

/// Fills with mid gray at `LaMa`'s working size (800 px), for budget tests.
struct Gray800;

impl Inpainter for Gray800 {
    fn id(&self) -> String {
        "gray-800".into()
    }
    fn max_edge(&self) -> u32 {
        800
    }
    fn inpaint(&self, _w: u32, _h: u32, rgb: &[u8], _mask: &[u8]) -> Option<Vec<u8>> {
        Some(vec![128; rgb.len()])
    }
}

/// A corner-to-corner stroke (1% brush) on a 61 MP frame, as a drag records it.
fn long_stroke() -> Removal {
    Removal {
        strokes: vec![BrushStroke {
            points: (0..=240)
                .map(|i| {
                    let t = 0.02 + 0.96 * i as f32 / 240.0;
                    [t, t]
                })
                .collect(),
            size: 0.01,
            ..BrushStroke::default()
        }],
        feather: 0.002,
    }
}

/// The budget of a long thin removal (bench: `removal_long_stroke_61mp`): a
/// corner-to-corner stroke on 61 MP is filled in windows near the engine's size (about
/// native resolution, a few MB each, never the frame), stores only the tiles it touches,
/// and takes well under the old ~8 s (~0.5 s idle on an M4 Pro; peak RSS of this test
/// ~0.47 GB, was ~6.5 GB).
#[test]
#[cfg_attr(debug_assertions, ignore = "61 MP: release builds only")]
fn a_long_thin_removal_on_61_mp_stays_in_budget() {
    let raw = raw_from_fn(9504, 6336, |x, y| {
        let g = 0.25 + 0.004 * noise(x, y);
        [g, g * 1.05, g * 0.9]
    });
    let adj = with(vec![long_stroke()]);
    let geo = removal_geometry(&raw, &adj, &rq()).unwrap();
    let caps = capsules(&adj.removals[0].strokes, &geo);
    let wins = windows(
        footprints(&caps, 0.002 * 9504.0, geometry::image_area(&raw)),
        geometry::image_area(&raw),
    );
    assert!(wins.len() > 10, "tiled: {}", wins.len());
    for w in &wins {
        assert!(
            w.crop[2].max(w.crop[3]) <= TILE + 2 * TILE_CONTEXT,
            "{:?}",
            w.crop
        );
    }
    let req = RenderRequest {
        inpainter: Some(Arc::new(Gray800)),
        ..rq()
    };
    let t = std::time::Instant::now();
    let (_, out) = resolve(ImageKey(9001), &raw, &adj, &req).unwrap().unwrap();
    let secs = t.elapsed().as_secs_f64();
    let bytes: u64 = locked().patches.iter().map(|e| e.1.bytes()).sum();
    eprintln!("long stroke: {secs:.2} s, patches {} MB", bytes >> 20);
    assert!(bytes < 32 << 20, "only the touched tiles: {bytes}");
    // ~0.5 s idle (2.8 s CPU); generous here: the gate runs on a shared, loaded machine.
    assert!(secs < 15.0, "{secs} s");
    // The middle of the stroke was filled (gray 128 → a different level than 0.25).
    let (cx, cy) = (4752usize, 3168usize);
    assert_ne!(out.data[cy * 9504 + cx], raw.data[cy * 9504 + cx]);
    assert_eq!(out.data[100 * 9504 + 9000], raw.data[100 * 9504 + 9000]);
    forget(ImageKey(9001));
}

/// Checks that the engine never sees part of the object as known content.
struct NoRedKnown(AtomicUsize);

impl Inpainter for NoRedKnown {
    fn id(&self) -> String {
        "no-red-known".into()
    }
    fn max_edge(&self) -> u32 {
        800
    }
    fn inpaint(&self, _w: u32, _h: u32, rgb: &[u8], mask: &[u8]) -> Option<Vec<u8>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        for (p, m) in rgb.as_chunks::<3>().0.iter().zip(mask) {
            let red = u16::from(p[0]) > 2 * u16::from(p[1]) + 40;
            assert!(!red || *m == 255, "object pixel {p:?} given as known");
        }
        Some(vec![128; rgb.len()])
    }
}

/// A stroke along a long red wire is filled window by window: each window masks the
/// wire's later parts too (not filled yet), and the whole wire goes.
#[test]
fn long_strokes_are_filled_window_by_window() {
    let (w, h) = (3400u32, 300u32);
    let raw = raw_from_fn(w, h, |x, y| {
        if (140..156).contains(&y) && (100..3300).contains(&x) {
            return [0.9, 0.1, 0.1];
        }
        let g = 0.25 + 0.004 * noise(x, y);
        [g, g * 1.05, g * 0.9]
    });
    let m = Arc::new(NoRedKnown(AtomicUsize::new(0)));
    let adj = with(vec![Removal {
        strokes: vec![BrushStroke {
            points: vec![[90.0 / 3400.0, 0.49], [3310.0 / 3400.0, 0.49]],
            size: 40.0 / 3400.0,
            ..BrushStroke::default()
        }],
        feather: 0.001,
    }]);
    let req = RenderRequest {
        inpainter: Some(m.clone()),
        ..rq()
    };
    let (_, out) = resolve(ImageKey(9101), &raw, &adj, &req).unwrap().unwrap();
    assert!(m.0.load(Ordering::SeqCst) >= 4, "one call per window");
    // No red left on the wire.
    let mut worst = 0.0f32;
    for x in (100..3300u32).step_by(2) {
        let i = (148 * w + x) as usize;
        if CfaPattern::Rggb.color_at(x, 148) == Some(0) {
            worst = worst.max((f32::from(out.data[i]) - 256.0) / (16_383.0 - 256.0) * 2.0);
        }
    }
    assert!(worst < 0.6, "red left: {worst}");
    forget(ImageKey(9101));
}

/// Footprints made tile by tile are the footprint made in one piece, cut at the tiles.
#[test]
fn tiled_footprints_match_the_whole() {
    let caps = [
        Capsule {
            a: [100.0, 700.0],
            b: [1700.0, 820.0],
            ra: 9.0,
            rb: 14.0,
            erase: false,
        },
        Capsule {
            a: [760.0, 760.0],
            b: [780.0, 760.0],
            ra: 30.0,
            rb: 30.0,
            erase: true,
        },
    ];
    let area = [0, 0, 2000, 1600];
    let whole = footprint(&caps, 6.0, area, area).unwrap();
    let tiles = footprints(&caps, 6.0, area);
    assert!(tiles.len() >= 3, "{}", tiles.len());
    for t in &tiles {
        assert_eq!(
            t.rect[0] / TILE,
            (t.rect[0] + t.rect[2] - 1) / TILE,
            "inside a tile"
        );
        assert_eq!(t.rect[1] / TILE, (t.rect[1] + t.rect[3] - 1) / TILE);
    }
    for y in (650..880).step_by(3) {
        for x in (60..1760).step_by(3) {
            let a = tiles.iter().map(|t| fp_alpha(t, x, y)).fold(0.0, f32::max);
            assert_eq!(a, fp_alpha(&whole, x, y), "({x}, {y})");
        }
    }
    assert!(footprints(&caps[1..], 6.0, area).is_empty(), "erase only");
}

#[test]
fn objects_fill_in_one_window_long_strokes_per_tile() {
    let fp = |rect: [u32; 4], fill: u8| Footprint {
        rect,
        alpha: vec![fill; (rect[2] * rect[3]) as usize],
    };
    let area = [0, 0, 6000, 4000];
    assert!(windows(Vec::new(), area).is_empty());
    // Small: one window around it, with the object's margin.
    let w = windows(
        vec![fp([708, 700, 60, 60], 255), fp([768, 700, 40, 60], 255)],
        area,
    );
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].fps.len(), 2);
    assert_eq!(w[0].crop[2], 100 + 2 * object_margin([708, 700, 100, 60]));
    // Long and sparse: a window per tile, with the tile context.
    let thin: Vec<Footprint> = (0..5)
        .map(|i| {
            let mut f = fp([i * TILE, 1000, TILE, TILE], 0);
            f.alpha[..(TILE * 40) as usize].fill(255);
            f
        })
        .collect();
    let w = windows(thin, area);
    assert_eq!(w.len(), 5);
    assert!(w.iter().all(|w| w.crop[2] == TILE + 2 * TILE_CONTEXT));
    // Long and dense (a big object): one window.
    let blob: Vec<Footprint> = (0..5)
        .map(|i| fp([i * TILE, 1000, TILE, TILE], 255))
        .collect();
    assert_eq!(windows(blob, area).len(), 1);
    // A big object plus dust far away in the same removal: the dust doesn't dilute the
    // object's thickness (one window for the object, one per speck, in order).
    let mut parts: Vec<Footprint> = (0..3)
        .flat_map(|j| (0..3).map(move |i| fp([1536 + i * TILE, 768 + j * TILE, TILE, TILE], 255)))
        .collect();
    parts.insert(0, fp([10, 10, 20, 20], 255));
    parts.push(fp([5900, 3900, 30, 30], 255));
    let w = windows(parts, area);
    assert_eq!(
        w.len(),
        3,
        "{:?}",
        w.iter().map(|w| w.crop).collect::<Vec<_>>()
    );
    assert_eq!(w.iter().map(|w| w.fps.len()).collect::<Vec<_>>(), [1, 9, 1]);
    assert_eq!(w[0].fps[0].rect, [10, 10, 20, 20]);
    // Footprints meeting only at a corner are one cluster.
    let c = clusters(vec![
        fp([0, 0, 10, 10], 255),
        fp([10, 10, 5, 5], 255),
        fp([16, 0, 4, 4], 255),
    ]);
    assert_eq!(c.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);
}

#[test]
fn mask_resampling_keeps_any_coverage() {
    let mut src = vec![false; 10 * 10];
    src[3 * 10 + 7] = true;
    let d = resize_any(&src, 10, 10, 3, 3);
    // Rows 3.33.. of the source fall in the second row of cells: both cover row 3.
    assert_eq!(d.iter().filter(|v| **v).count(), 2);
    assert!(d[2] && d[3 + 2], "the cells over (7, 3)");
    // Upscaling and odd ratios cover the same place.
    let up = resize_any(&src, 10, 10, 20, 20);
    assert!(up[6 * 20 + 14] && up[7 * 20 + 15] && !up[0]);
    assert!(resize_any(&src, 10, 10, 7, 4).contains(&true));
}

/// Highlights above the crop's bright end and colours outside the working gamut survive
/// the engine's encoding (a fill of them decodes to them, not to a clipped version).
#[test]
fn the_engine_encoding_keeps_highlights_and_wide_gamut() {
    let mut raw = scene(16, 16, 0.0, 0.0, 0.0, false);
    // A camera whose green is wider than the working gamut.
    raw.cam_to_xyz = [[0.41, 0.05, 0.18], [0.21, 0.9, 0.07], [0.02, 0.0, 0.95]];
    let m = crate::plan::cam_matrix(&raw);
    let inv = mat_inv(&m).unwrap();
    let wide = [0.02f32, 0.3, 0.01];
    assert!(mat_vec(&m, wide).iter().any(|v| *v < 0.0), "out of gamut");
    let gray = mat_vec(&inv, [0.2; 3]);
    let mut base = vec![gray; 700];
    for b in base.iter_mut().step_by(10) {
        *b = wide;
    }
    let t = Tone::new(&raw, &base);
    assert!(t.s < 1.0 && t.s >= 0.25, "{}", t.s);
    let back = |b: [f32; 3]| t.decode(t.encode(b));
    let close = |a: [f32; 3], b: [f32; 3]| a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 2e-3);
    assert!(close(back(wide), wide), "{:?}", back(wide));
    assert!(close(back(gray), gray), "{:?}", back(gray));
    // 2.5× the bright end: encoded below 1, decoded back.
    let bright = gray.map(|v| v * 2.5);
    assert!(t.encode(bright).iter().all(|v| *v < 1.0));
    let b = back(bright);
    assert!(
        b.iter().zip(&bright).all(|(x, y)| (x - y).abs() < 0.01 * y),
        "{b:?}"
    );
    assert_eq!(unknee(knee(0.3)), 0.3);
    assert!(knee(1.5) < 1.0 && knee(100.0) <= 1.0 && unknee(1.0) > 2.0);
}

/// A render waiting for another one's fill gives up when it's cancelled.
#[test]
fn waiting_for_another_renders_fill_is_cancellable() {
    let raw = scene(64, 48, 30.0, 20.0, 5.0, true);
    let k = (9201, 1);
    locked().inflight.insert(k);
    let c = CancelToken::new();
    let req = RenderRequest {
        cancel: Some(c.clone()),
        ..rq()
    };
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(80));
        c.cancel();
    });
    let geo = removal_geometry(&raw, &Adjustments::default(), &rq()).unwrap();
    let mut cur = raw.clone();
    let r = patch_for(k, &mut cur, &dot([0.5, 0.5], 0.1), &geo, &req);
    assert!(matches!(r, Err(PipelineError::Cancelled)));
    canceller.join().unwrap();
    locked().inflight.remove(&k);
}

/// The disk cache keeps the most recently used patches within its budget.
#[test]
fn the_disk_cache_is_trimmed_least_recently_used_first() {
    let t = std::env::temp_dir().join(format!("ds-removal-lru-{}", std::process::id()));
    let p = Patches(vec![Patch {
        rect: [0, 0, 100, 10],
        chans: 1,
        data: vec![1; 1000],
    }]);
    let now = std::time::SystemTime::now();
    for i in 0..4u64 {
        disk_write(Some(&t), (1, i), &p);
        let age = std::time::Duration::from_secs(100 * (4 - i));
        std::fs::File::options()
            .append(true)
            .open(disk_path(&t, (1, i)))
            .unwrap()
            .set_modified(now - age)
            .unwrap();
    }
    std::fs::write(t.join("other.txt"), vec![0; 50_000]).unwrap();
    // Leftovers of interrupted writes go after an hour; a younger one (another process's
    // write in flight, or this one's) stays.
    std::fs::write(t.join("a.dsrm.tmp1"), b"x").unwrap();
    std::fs::File::options()
        .append(true)
        .open(t.join("a.dsrm.tmp1"))
        .unwrap()
        .set_modified(now - std::time::Duration::from_secs(7200))
        .unwrap();
    let other = t.join("c.tmp1");
    std::fs::write(&other, b"x").unwrap();
    let own = t.join(format!("b.tmp{}", std::process::id()));
    std::fs::write(&own, b"x").unwrap();
    // Reading the oldest makes it the newest.
    assert!(disk_read(Some(&t), (1, 0)).is_some());
    let size = std::fs::metadata(disk_path(&t, (1, 0))).unwrap().len();
    prune(&t, size * 2);
    let left: Vec<bool> = (0..4).map(|i| disk_path(&t, (1, i)).exists()).collect();
    assert_eq!(left, [true, false, false, true]);
    assert!(t.join("other.txt").exists(), "only patch files");
    assert!(!t.join("a.dsrm.tmp1").exists() && own.exists() && other.exists());
    prune(&t.join("missing"), 0);
    // Setting the folder trims it in the background.
    set_cache_dir(Some(t.clone()));
    set_cache_dir(None);
    let _ = std::fs::remove_dir_all(&t);
}

/// A window too big for full resolution fills from a reduced image and finishes each tile
/// at full resolution: the object goes, nothing far from it changes, in every
/// orientation the reduced map must follow.
#[test]
fn big_windows_fill_from_a_reduced_image() {
    let (cx, cy, r) = (300.0, 180.0, 40.0);
    for (i, o) in [
        Orientation::Normal,
        Orientation::Rotate90,
        Orientation::Transverse,
    ]
    .into_iter()
    .enumerate()
    {
        let mut raw = scene(640, 400, cx, cy, r, true);
        raw.orientation = o;
        let p = e_norm(&raw, f64::from(cx), f64::from(cy));
        let rem = dot(p, 100.0 / 640.0);
        let geo = removal_geometry(&raw, &with(vec![rem.clone()]), &rq()).unwrap();
        let area = geometry::image_area(&raw);
        let fps = footprints(&capsules(&rem.strokes, &geo), 0.004 * 640.0, area);
        let win = Window {
            crop: crop_rect(
                fps.iter().map(|f| f.rect).reduce(union).unwrap(),
                area,
                MIN_CONTEXT,
            ),
            fps,
        };
        let m = Arc::new(NoRedKnown(AtomicUsize::new(0)));
        let req = RenderRequest {
            inpainter: Some(m.clone()),
            ..rq()
        };
        // Working at 64 px: the crop is reduced ~3×.
        let patches = fill_big(&raw, &win, &[], &geo, &req, 7 + i as u64, 64).unwrap();
        assert_eq!(m.0.load(Ordering::SeqCst), 1);
        assert_eq!(patches.len(), win.fps.len());
        let mut out = raw.clone();
        Patches(patches).apply(&mut out);
        let (red, far) = disc_stats(&raw, &out, cx, cy, r - 4.0);
        assert!(red < 0.45, "{o:?}: object still there (red {red})");
        assert_eq!(far, 0, "{o:?}: pixels far from the removal changed");
    }
    // The reduced demosaic is the box average of the full one.
    let raw = scene(64, 48, 0.0, 0.0, 0.0, false);
    let full = crate::demosaic::rcd_rect(&raw, &crate::plan::base_params(&raw), [0, 0, 64, 48]);
    let red = reduced_base(&raw, [0, 0, 64, 48], 4, 16, 12);
    let mut want = [0.0f32; 3];
    for y in 4..8 {
        for x in 8..12 {
            for c in 0..3 {
                want[c] += full.px[y * 64 + x][c] / 16.0;
            }
        }
    }
    let got = red[16 + 2];
    assert!(
        (0..3).all(|c| (got[c] - want[c]).abs() < 1e-5),
        "{got:?} {want:?}"
    );
}

/// Fills each hole pixel by interpolating its column between the nearest known pixels
/// above and below (real context, like a model, not a constant).
struct ColumnInterp;

impl Inpainter for ColumnInterp {
    fn id(&self) -> String {
        "column-interp".into()
    }
    fn max_edge(&self) -> u32 {
        800
    }
    fn inpaint(&self, w: u32, h: u32, rgb: &[u8], mask: &[u8]) -> Option<Vec<u8>> {
        let (w, h) = (w as usize, h as usize);
        let mut out = rgb.to_vec();
        for x in 0..w {
            let known = |y: usize| mask[y * w + x] == 0;
            for y in 0..h {
                if known(y) {
                    continue;
                }
                let up = (0..y).rev().find(|&yy| known(yy));
                let down = (y + 1..h).find(|&yy| known(yy));
                for c in 0..3 {
                    let v = |yy: usize| f32::from(rgb[(yy * w + x) * 3 + c]);
                    out[(y * w + x) * 3 + c] = match (up, down) {
                        (Some(a), Some(b)) => {
                            let t = (y - a) as f32 / (b - a) as f32;
                            (v(a) + (v(b) - v(a)) * t).round() as u8
                        }
                        (Some(a), None) => rgb[(a * w + x) * 3 + c],
                        (None, Some(b)) => rgb[(b * w + x) * 3 + c],
                        (None, None) => 128,
                    };
                }
            }
        }
        Some(out)
    }
}

/// A long stroke over a gradient, filled window by window by a model that uses the
/// context: the fill follows the gradient through every window seam (no steps where one
/// tile's window ends and the next begins).
#[test]
fn tiled_fills_are_continuous_across_window_seams() {
    let (w, h) = (3400u32, 300u32);
    let clean = |x: u32| 0.12 + 0.3 * x as f32 / w as f32;
    let raw = raw_from_fn(w, h, |x, y| {
        if (144..152).contains(&y) && (100..3300).contains(&x) {
            return [0.9, 0.1, 0.1];
        }
        let g = clean(x);
        [g, g, g]
    });
    let adj = with(vec![Removal {
        strokes: vec![BrushStroke {
            points: vec![[90.0 / 3400.0, 0.49], [3310.0 / 3400.0, 0.49]],
            size: 30.0 / 3400.0,
            ..BrushStroke::default()
        }],
        feather: 0.001,
    }]);
    let req = RenderRequest {
        inpainter: Some(Arc::new(ColumnInterp)),
        ..rq()
    };
    let (_, out) = resolve(ImageKey(9301), &raw, &adj, &req).unwrap().unwrap();
    // Per column, the mean green-site level over the stroke's rows (grain averages out).
    let level = |x: u32| {
        let (mut s, mut n) = (0.0f32, 0.0f32);
        for y in 140..156u32 {
            for xx in [x, x + 1] {
                if CfaPattern::Rggb.color_at(xx, y) == Some(1) {
                    s += (f32::from(out.data[(y * w + xx) as usize]) - 256.0) / (16_383.0 - 256.0);
                    n += 1.0;
                }
            }
        }
        s / n
    };
    let mut worst_step = 0.0f32;
    let mut worst_err = 0.0f32;
    let mut prev = level(200);
    for x in (202..3200u32).step_by(2) {
        let v = level(x);
        worst_step = worst_step.max((v - prev).abs());
        worst_err = worst_err.max((v - clean(x)).abs());
        prev = v;
    }
    assert!(worst_err < 0.03, "follows the gradient: {worst_err}");
    assert!(worst_step < 0.02, "no seams: {worst_step}");
    forget(ImageKey(9301));
}

/// A big object on fine texture, filled from a reduced image (a smooth model fill): every
/// finishing tile draws texture from the whole window's pool, so the inner tiles (no known
/// pixel within their reach) are as textured as the edge ones, and there is no step in the
/// texture where one tile meets the next.
#[test]
fn big_fills_are_textured_evenly_across_tiles() {
    let (w, h) = (2304u32, 2304u32);
    let (c, r) = (1152.0f32, 700.0f32);
    // A checker of 4 px squares: a band the reduced fill can't carry.
    let raw = raw_from_fn(w, h, |x, y| {
        if (x as f32 + 0.5 - c).hypot(y as f32 + 0.5 - c) < r {
            return [0.9, 0.1, 0.1];
        }
        let g = 0.25 * (1.0 + 0.25 * ((x / 4 + y / 4) % 2) as f32);
        [g, g, g]
    });
    let rem = dot(e_norm(&raw, f64::from(c), f64::from(c)), 1420.0 / 2304.0);
    let geo = removal_geometry(&raw, &with(vec![rem.clone()]), &rq()).unwrap();
    let area = geometry::image_area(&raw);
    let fps = footprints(&capsules(&rem.strokes, &geo), 0.004 * 2304.0, area);
    let wins = windows(fps, area);
    assert_eq!(wins.len(), 1);
    let req = RenderRequest {
        inpainter: Some(Arc::new(ColumnInterp)),
        ..rq()
    };
    let patches = fill_big(&raw, &wins[0], &[], &geo, &req, 11, 256).unwrap();
    let mut out = raw.clone();
    Patches(patches).apply(&mut out);
    // Texture energy over half-resolution green (2×2 quads): the squared deviation from the
    // 3×3 quad mean, averaged over the quads of `rect` (sensor px) inside the object.
    let lum = |img: &RawImage, qx: u32, qy: u32| {
        let i = |x: u32, y: u32| f32::from(img.data[(y * w + x) as usize]) - 256.0;
        f32::midpoint(i(2 * qx + 1, 2 * qy), i(2 * qx, 2 * qy + 1))
    };
    let energy = |img: &RawImage, rect: [u32; 4], inside: bool| {
        let (mut s, mut n) = (0.0f64, 0.0f64);
        let end = [rect[0] + rect[2], rect[1] + rect[3]].map(|v| v / 2);
        for qy in rect[1] / 2..end[1] {
            for qx in rect[0] / 2..end[0] {
                let d = (qx as f32 * 2.0 + 1.0 - c).hypot(qy as f32 * 2.0 + 1.0 - c);
                if (d < r - 60.0) != inside || d < r + 60.0 && !inside {
                    continue;
                }
                let mut m = 0.0;
                for (dx, dy) in (0..9).map(|k| (k % 3, k / 3)) {
                    m += lum(img, qx + dx - 1, qy + dy - 1) / 9.0;
                }
                s += f64::from((lum(img, qx, qy) - m).powi(2));
                n += 1.0;
            }
        }
        s / n.max(1.0)
    };
    let original = energy(&raw, [8, 8, w - 16, h - 16], false);
    let tiles: Vec<f64> = (0..9)
        .map(|t| energy(&out, [(t % 3) * TILE, (t / 3) * TILE, TILE, TILE], true))
        .collect();
    eprintln!("texture energy: original {original:.0}, tiles {tiles:.0?}");
    let inner = tiles[4];
    let edge = (tiles[1] + tiles[3] + tiles[5] + tiles[7]) / 4.0;
    assert!(
        inner > 0.15 * original,
        "inner tile textured: {inner} vs {original}"
    );
    assert!(
        inner / edge > 0.5 && inner / edge < 2.0,
        "inner {inner}, edge {edge}"
    );
    // Either side of each tile boundary, through the middle of the object.
    for b in [TILE, 2 * TILE] {
        for (a, z) in [
            ([b - 64, 900, 64, 500], [b, 900, 64, 500]),
            ([900, b - 64, 500, 64], [900, b, 500, 64]),
        ] {
            let (ea, ez) = (energy(&out, a, true), energy(&out, z, true));
            assert!(ea / ez > 0.6 && ea / ez < 1.6, "step at {b}: {ea} vs {ez}");
        }
    }
}

#[test]
fn a_patch_with_an_overflowing_rect_is_ignored() {
    let mut raw = raw_from_fn(8, 8, |_, _| [0.5; 3]);
    let before = raw.data.clone();
    // 65536 x 65536 samples wrap to 0 in u32 arithmetic: empty data must not match.
    let wrapped = Patch {
        rect: [0, 0, 1 << 16, 1 << 16],
        chans: 1,
        data: Vec::new(),
    };
    wrapped.apply(&mut raw);
    // A rectangle whose far corner overflows u32 is clipped, not wrapped.
    let far = Patch {
        rect: [u32::MAX - 1, 0, 4, 1],
        chans: 1,
        data: vec![7; 4],
    };
    far.apply(&mut raw);
    assert_eq!(raw.data, before);
}

#[test]
fn an_oversized_cache_file_is_not_read() {
    let t = std::env::temp_dir().join(format!("ds-removal-big-{}", std::process::id()));
    std::fs::create_dir_all(&t).unwrap();
    let k = (7, 9);
    // A sparse file one byte past the cap: refused from its length alone.
    let f = std::fs::File::create(disk_path(&t, k)).unwrap();
    f.set_len(MAX_PATCH_FILE + 1).unwrap();
    assert!(disk_read(Some(&t), k).is_none());
    std::fs::remove_dir_all(&t).unwrap();
}
