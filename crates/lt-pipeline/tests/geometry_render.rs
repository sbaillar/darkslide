//! Rendered geometry conventions (ported from the review probes `q_key`): keystone
//! direction on converging lines and the straighten/turn/crop convention.

#![allow(clippy::float_cmp)]

mod support;

use lt_core::{Adjustments, CfaPattern, NormRect, RawImage, Size};
use lt_pipeline::{CaptureInfo, ImageKey, RenderRequest, RenderedImage, Renderer, cpu_renderer};

/// Linear RGB frame, 0.18 gray with dark pixels where `dark(x, y)`.
fn frame(w: u32, h: u32, dark: impl Fn(f32, f32) -> bool) -> RawImage {
    let mut raw = support::synthetic_raw(64, 64);
    raw.size = Size::new(w, h);
    raw.cfa = CfaPattern::LinearRgb;
    raw.black_level = [0.0; 4];
    raw.white_level = 65535.0;
    raw.wb_as_shot = [1.0; 3];
    raw.default_crop = None;
    raw.data = (0..w * h * 3)
        .map(|i| {
            let p = i / 3;
            let (x, y) = ((p % w) as f32, (p / w) as f32);
            let v = if dark(x, y) { 0.02f32 } else { 0.18 };
            (v * 65535.0) as u16
        })
        .collect();
    raw
}

fn plain() -> Adjustments {
    let mut a = Adjustments::default();
    a.noise.luminance = 0.0;
    a.noise.color = 0.0;
    a.detail.sharpen_amount = 0.0;
    a
}

/// Mean spacing of dark lines crossing row `yf` (fraction of the width).
fn spacing(img: &RenderedImage, yf: f32) -> f32 {
    let (iw, ih) = (img.size.width as usize, img.size.height as usize);
    let y = ((ih as f32 - 1.0) * yf) as usize;
    let g = |x: usize| img.rgba8[(y * iw + x) * 4 + 1];
    let xs: Vec<usize> = (1..iw - 1)
        .filter(|&x| g(x) < 60 && g(x - 1) >= 60)
        .collect();
    assert!(xs.len() >= 2, "lines not found at {yf}");
    (xs[xs.len() - 1] - xs[0]) as f32 / (xs.len() - 1) as f32 / iw as f32
}

#[test]
fn positive_vertical_keystone_straightens_lines_converging_upward() {
    let (w, h) = (1800u32, 1200u32);
    let cx = w as f32 / 2.0;
    // Nine dark verticals converging toward the top (camera tilted up): spacing 0.7 at
    // the top, 1.0 at the bottom.
    let raw = frame(w, h, |x, y| {
        let s = 0.7 + 0.3 * y / h as f32;
        (-4i32..=4).any(|k| (x - (cx + k as f32 * 180.0 * s)).abs() < 3.0)
    });
    let req = RenderRequest {
        capture: CaptureInfo {
            focal_length: Some(24.0),
            ..CaptureInfo::default()
        },
        ..RenderRequest::new(Size::new(900, 900))
    };
    let mut r = cpu_renderer();
    let mut ratio = |v: f32| {
        let mut a = plain();
        a.lens.keystone_vertical = v;
        let img = r.render(ImageKey(1), &raw, &a, &req).unwrap();
        spacing(&img, 0.05) / spacing(&img, 0.95)
    };
    let (r0, r5, r10, rm) = (ratio(0.0), ratio(5.0), ratio(10.0), ratio(-10.0));
    println!("top/bottom spacing: -10 {rm:.3}, 0 {r0:.3}, +5 {r5:.3}, +10 {r10:.3}");
    assert!(r0 < 0.75, "{r0}");
    assert!(
        r5 > r0 + 0.03 && r10 > r5 + 0.03,
        "+v widens the top: {r0} {r5} {r10}"
    );
    assert!(rm < r0 - 0.03, "-v narrows the top: {rm}");
}

#[test]
fn straighten_turn_and_crop_follow_the_documented_convention() {
    // A dark horizontal line through the center, a dark square in the top-left quadrant.
    let (w, h) = (1200u32, 800u32);
    let raw = frame(w, h, |x, y| {
        (y - 400.0).abs() < 3.0 || ((100.0..200.0).contains(&x) && (100.0..200.0).contains(&y))
    });
    let mut r = cpu_renderer();
    let req = RenderRequest::new(Size::new(4000, 4000));
    let dark_rows = |img: &RenderedImage, xf: f32| -> f32 {
        let (iw, ih) = (img.size.width as usize, img.size.height as usize);
        let x = ((iw as f32 - 1.0) * xf) as usize;
        let ys: Vec<usize> = (0..ih)
            .filter(|&y| img.rgba8[(y * iw + x) * 4 + 1] < 60)
            .collect();
        ys.iter().sum::<usize>() as f32 / ys.len().max(1) as f32
    };
    // Positive angle rotates content counter-clockwise: the line rises to the right
    // (smaller y on the right than on the left).
    let mut a = plain();
    a.crop.angle_deg = 5.0;
    a.crop.rect = NormRect {
        x: 0.25,
        y: 0.25,
        w: 0.5,
        h: 0.5,
    };
    let img = r.render(ImageKey(1), &raw, &a, &req).unwrap();
    let (left, right) = (dark_rows(&img, 0.1), dark_rows(&img, 0.9));
    assert!(right < left - 10.0, "CCW: left {left} right {right}");
    // The crop rect is in the oriented, pre-turn frame: a crop of the top-left quarter
    // contains the square; one clockwise quarter turn after the crop puts that
    // quarter's top-left corner at the top-right.
    let mut a = plain();
    a.crop.rect = NormRect {
        x: 0.0,
        y: 0.0,
        w: 0.5,
        h: 0.5,
    };
    a.crop.quarter_turns = 1;
    let img = r.render(ImageKey(1), &raw, &a, &req).unwrap();
    assert_eq!(img.size, Size::new(400, 600), "turned after the crop");
    let px = |x: u32, y: u32| img.rgba8[((y * img.size.width + x) * 4 + 1) as usize];
    // Square occupies source x,y in 100..200 → after CW turn: x' = 400 − y, y' = x.
    assert!(px(250, 150) < 60, "square at top-right after a CW turn");
    assert!(px(150, 150) > 60);
}
