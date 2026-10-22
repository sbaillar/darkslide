//! Regression tests from the adversarial review of copy/apply (L9, H1).
#![allow(clippy::float_cmp)]

use ds_core::adjust::{Aspect, Crop, WhiteBalance};
use ds_core::clipboard::{Clipboard, adapt_crop};
use ds_core::geom::{NormRect, Size};
use ds_core::{AdjustGroup, Adjustments, PasteOptions, PasteTarget, PasteWarning};

const LAND: Size = Size::new(6000, 4000);
const PORT: Size = Size::new(4000, 6000);

fn px(r: NormRect, s: Size) -> f64 {
    (f64::from(r.w) * f64::from(s.width)) / (f64::from(r.h) * f64::from(s.height))
}

fn crop(rect: NormRect, aspect: Option<Aspect>) -> Crop {
    Crop {
        rect,
        aspect,
        ..Crop::default()
    }
}

/// A 4:5 portrait crop out of a landscape frame.
fn portrait_45() -> Crop {
    let h = 0.9f32;
    let w = h * 4000.0 * 0.8 / 6000.0;
    crop(
        NormRect {
            x: 0.5 - w / 2.0,
            y: 0.05,
            w,
            h,
        },
        Some(Aspect { long: 5, short: 4 }),
    )
}

fn wb_opts() -> PasteOptions {
    PasteOptions {
        groups: [AdjustGroup::WhiteBalance].into(),
        relative: [AdjustGroup::WhiteBalance].into(),
        ..PasteOptions::default()
    }
}

fn custom(temp_k: f32, tint: f32) -> WhiteBalance {
    WhiteBalance::Custom { temp_k, tint }
}

fn with_wb(wb: WhiteBalance) -> Adjustments {
    Adjustments {
        white_balance: wb,
        ..Adjustments::default()
    }
}

#[test]
fn landscape_32_to_portrait_becomes_23() {
    let c = crop(
        NormRect {
            x: 0.1,
            y: 0.1,
            w: 0.8,
            h: 0.8,
        },
        Some(Aspect { long: 3, short: 2 }),
    );
    let out = adapt_crop(&c, LAND, PORT);
    assert!((px(out.rect, PORT) - 2.0 / 3.0).abs() < 1e-3);
}

#[test]
fn portrait_45_on_landscape_to_landscape_stays_45() {
    let target = Size::new(6048, 4024);
    let out = adapt_crop(&portrait_45(), LAND, target);
    assert!((px(out.rect, target) - 0.8).abs() < 1e-3);
}

/// Instagram 4:5 portrait crop made on a landscape frame, pasted to a
/// portrait frame: it stays 4:5 portrait.
#[test]
fn portrait_45_on_landscape_to_portrait_target() {
    let out = adapt_crop(&portrait_45(), LAND, PORT);
    let r = px(out.rect, PORT);
    assert!(
        (r - 0.8).abs() < 1e-3,
        "4:5 portrait crop became {r:.3} on a portrait target"
    );
}

/// Free (unlocked) crop, same orientation, different sensor aspect: the
/// normalized rect is kept (documented behavior).
#[test]
fn free_crop_same_orientation_other_aspect_keeps_normalized_rect() {
    let rect = NormRect {
        x: 0.0,
        y: 0.0,
        w: 0.5,
        h: 0.5,
    };
    let out = adapt_crop(&crop(rect, None), LAND, Size::new(5184, 3888));
    assert_eq!(out.rect, rect);
}

/// Quarter turns travel with the crop and orientation is judged on the
/// displayed frame: a landscape image turned to portrait with a matching
/// (displayed-portrait) 2:3 crop, pasted onto a portrait image that the same
/// turn shows as landscape, becomes a displayed-landscape 3:2 crop.
#[test]
fn quarter_turns_are_honored() {
    // Oriented rect is landscape 3:2 in LAND pixels; displayed (turned) it is 2:3 portrait.
    let mut c = crop(
        NormRect {
            x: 0.1,
            y: 0.1,
            w: 0.8,
            h: 0.8,
        },
        Some(Aspect { long: 3, short: 2 }),
    );
    c.quarter_turns = 1;
    let out = adapt_crop(&c, LAND, PORT);
    assert_eq!(out.quarter_turns, 1);
    // Displayed target is 6000x4000 landscape → displayed crop landscape → oriented rect portrait 2:3.
    assert!(
        (px(out.rect, PORT) - 2.0 / 3.0).abs() < 1e-3,
        "{:?}",
        out.rect
    );

    // A deliberate crop against the displayed frame keeps its displayed orientation:
    // displayed frame portrait (LAND turned), displayed crop landscape (oriented rect portrait).
    let mut d = portrait_45();
    d.quarter_turns = 3;
    for target in [LAND, PORT] {
        let out = adapt_crop(&d, LAND, target);
        assert!(
            (px(out.rect, target) - 0.8).abs() < 1e-3,
            "target {target:?}: {:?}",
            out.rect
        );
    }
}

#[test]
fn relative_wb_from_as_shot_without_context_falls_back_with_warning() {
    let clip = Clipboard::new(with_wb(custom(5600.0, 5.0)), Adjustments::default(), LAND);
    let target = with_wb(custom(4000.0, 0.0));
    let pasted = clip.apply_to(
        &target,
        &PasteTarget {
            size: LAND,
            as_shot: None,
        },
        &wb_opts(),
    );
    assert_eq!(pasted.adjustments.white_balance, custom(5600.0, 5.0));
    assert_eq!(
        pasted.warnings,
        vec![PasteWarning::WhiteBalanceAbsoluteFallback]
    );
    // The plain `apply` gives the same adjustments.
    assert_eq!(clip.apply(&target, LAND, &wb_opts()), pasted.adjustments);
}

#[test]
fn relative_wb_uses_as_shot_context() {
    // Source was shot at 5000 K, user warmed it to 6000 K (+20%), tint +4.
    let mut clip = Clipboard::new(with_wb(custom(6000.0, 4.0)), Adjustments::default(), LAND);
    clip.source_as_shot = Some((5000.0, 0.0));
    // Target still as-shot at 4000 K, tint 2 → 4800 K, tint 6.
    let t = PasteTarget {
        size: LAND,
        as_shot: Some((4000.0, 2.0)),
    };
    let pasted = clip.apply_to(&Adjustments::default(), &t, &wb_opts());
    assert_eq!(pasted.warnings, [] as [ds_core::PasteWarning; 0]);
    let WhiteBalance::Custom { temp_k, tint } = pasted.adjustments.white_balance else {
        panic!()
    };
    assert!(
        (temp_k - 4800.0).abs() < 1e-2 && (tint - 6.0).abs() < 1e-4,
        "{temp_k} {tint}"
    );

    // Target custom, base custom: context only needed for AsShot sides.
    let clip2 = Clipboard::new(
        with_wb(custom(6000.0, 0.0)),
        with_wb(custom(5000.0, 0.0)),
        LAND,
    );
    let out = clip2.apply_to(
        &with_wb(custom(3000.0, 1.0)),
        &PasteTarget {
            size: LAND,
            as_shot: None,
        },
        &wb_opts(),
    );
    assert_eq!(out.warnings, [] as [ds_core::PasteWarning; 0]);
    assert_eq!(out.adjustments.white_balance, custom(3600.0, 1.0));

    // AsShot target without its context → fallback.
    let out = clip.apply_to(
        &Adjustments::default(),
        &PasteTarget {
            size: LAND,
            as_shot: None,
        },
        &wb_opts(),
    );
    assert_eq!(
        out.warnings,
        vec![PasteWarning::WhiteBalanceAbsoluteFallback]
    );
    assert_eq!(out.adjustments.white_balance, custom(6000.0, 4.0));
}

#[test]
fn relative_wb_without_a_change_leaves_target_alone() {
    let clip = Clipboard::new(Adjustments::default(), Adjustments::default(), LAND);
    let target = with_wb(custom(3200.0, -3.0));
    let out = clip.apply_to(
        &target,
        &PasteTarget {
            size: LAND,
            as_shot: None,
        },
        &wb_opts(),
    );
    assert_eq!(out.adjustments.white_balance, target.white_balance);
    assert_eq!(out.warnings, [] as [ds_core::PasteWarning; 0]);
}

#[test]
fn relative_exposure_with_nan_source_keeps_target() {
    let mut src = Adjustments::default();
    src.exposure.exposure_ev = f32::NAN;
    src.exposure.contrast = 10.0;
    let opts = PasteOptions {
        groups: [AdjustGroup::Exposure, AdjustGroup::Hdr].into(),
        relative: [AdjustGroup::Exposure, AdjustGroup::Hdr].into(),
        ..PasteOptions::default()
    };
    let mut target = Adjustments::default();
    target.exposure.exposure_ev = 1.0;
    let out = Clipboard::new(src, Adjustments::default(), LAND).apply(&target, LAND, &opts);
    assert_eq!(out.exposure.exposure_ev, 1.0);
    assert_eq!(out.exposure.contrast, 10.0);
}

#[test]
fn absolute_paste_of_non_finite_values_is_sanitized() {
    let mut src = Adjustments::default();
    src.exposure.exposure_ev = f32::INFINITY;
    src.levels.gamma = f32::NAN;
    src.white_balance = custom(f32::NAN, 0.0);
    let opts = PasteOptions::all();
    let out = Clipboard::new(src, Adjustments::default(), LAND).apply(
        &Adjustments::default(),
        LAND,
        &opts,
    );
    assert_eq!(out.exposure.exposure_ev, 0.0);
    assert_eq!(out.levels.gamma, 1.0);
    assert_eq!(out.white_balance, custom(5500.0, 0.0));
    let json = serde_json::to_string(&out).unwrap();
    assert_eq!(
        serde_json::from_str::<Adjustments>(&json).unwrap(),
        out,
        "persists losslessly"
    );
}

#[test]
fn clipboard_serde_defaults_as_shot() {
    let clip = Clipboard::new(Adjustments::default(), Adjustments::default(), LAND);
    let mut json: serde_json::Value = serde_json::to_value(&clip).unwrap();
    json.as_object_mut().unwrap().remove("source_as_shot");
    let back: Clipboard = serde_json::from_value(json).unwrap();
    assert_eq!(back, clip);
}
