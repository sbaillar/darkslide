#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::collections::BTreeMap;

use ds_core::Adjustments;
use ds_core::adjust::{AiTarget, MaskOp, MaskShape, WhiteBalance};
use ds_core::geom::Orientation;
use ds_session::ImageMetaLite;

use super::*;

fn meta(orientation: Orientation) -> ImageMetaLite {
    ImageMetaLite {
        capture_time_ms: None,
        camera: None,
        lens: None,
        iso: None,
        focal_length_mm: None,
        width: 6000,
        height: 4000,
        orientation,
        as_shot_wb: None,
    }
}

fn n(v: f64) -> Setting {
    Setting::Num(v)
}

fn t(v: &str) -> Setting {
    Setting::Text(v.to_owned())
}

fn dev(items: &[(&str, Setting)]) -> Develop {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn table(items: &[(&str, Setting)]) -> Setting {
    Setting::Table(
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    )
}

fn run(d: &Develop) -> Mapped {
    let m = meta(Orientation::Normal);
    map_develop(
        d,
        &MapContext {
            meta: &m,
            is_raw: true,
            lr_orientation: None,
        },
    )
}

fn run_with(d: &Develop, m: &ImageMetaLite, is_raw: bool, lr: Option<u8>) -> Mapped {
    map_develop(
        d,
        &MapContext {
            meta: m,
            is_raw,
            lr_orientation: lr,
        },
    )
}

fn fid(m: &Mapped, key: &str) -> Option<Fidelity> {
    m.mapped.iter().find(|(k, _)| k == key).map(|(_, f)| *f)
}

fn dropped(m: &Mapped, key: &str) -> bool {
    m.dropped.iter().any(|(k, _)| k == key)
}

#[test]
fn lightroom_defaults_are_not_edits() {
    let d = dev(&[
        ("Exposure2012", t("+0.00")),
        ("Contrast2012", n(0.0)),
        ("Sharpness", n(40.0)),
        ("SharpenRadius", t("+1.0")),
        ("SharpenDetail", n(25.0)),
        ("ColorNoiseReduction", n(25.0)),
        ("LuminanceNoiseReductionDetail", n(50.0)),
        ("PostCropVignetteMidpoint", n(50.0)),
        ("GrainSize", n(25.0)),
        ("LensProfileDistortionScale", n(100.0)),
        ("CropRight", n(1.0)),
        ("CropBottom", n(1.0)),
        ("CropAngle", n(0.0)),
        ("WhiteBalance", t("As Shot")),
        ("Temperature", n(5500.0)),
        (
            "ToneCurvePV2012",
            Setting::List(vec![t("0, 0"), t("255, 255")]),
        ),
        ("LensProfileEnable", n(1.0)),
        ("AutoLateralCA", n(1.0)),
    ]);
    let m = run(&d);
    assert_eq!(m.adjustments, Adjustments::default());
    assert!(m.categories.is_empty(), "{:?}", m.categories);
    assert!(m.mapped.is_empty(), "{:?}", m.mapped);
    assert!(m.dropped.is_empty(), "{:?}", m.dropped);
}

#[test]
fn tone_sliders_map_with_darkslides_signs() {
    let d = dev(&[
        ("Exposure2012", t("+0.75")),
        ("Contrast2012", n(20.0)),
        ("Highlights2012", n(-60.0)),
        ("Shadows2012", n(40.0)),
        ("Whites2012", n(10.0)),
        ("Blacks2012", n(-30.0)),
    ]);
    let m = run(&d);
    let a = &m.adjustments;
    assert_eq!(a.exposure.exposure_ev, 0.75);
    assert_eq!(a.exposure.contrast, 20.0);
    // Lightroom's negative highlights recover; Darkslide's positive ones do.
    assert_eq!(a.hdr.highlights, 60.0);
    assert_eq!(a.hdr.shadows, 40.0);
    assert_eq!(a.hdr.whites, 10.0);
    assert_eq!(a.hdr.blacks, -30.0);
    assert_eq!(m.categories, BTreeSet::from([Category::Tone]));
    assert_eq!(fid(&m, "Exposure2012"), Some(Fidelity::Approximate));
    assert_eq!(m.mapped.len(), 6);
    // Out-of-range values are clamped, not rejected.
    let m = run(&dev(&[
        ("Exposure2012", n(9.0)),
        ("Highlights2012", n(-250.0)),
    ]));
    assert_eq!(m.adjustments.exposure.exposure_ev, 5.0);
    assert_eq!(m.adjustments.hdr.highlights, 100.0);
}

#[test]
fn tone_curves_from_xmp_text_and_catalog_lists() {
    let xmp = Setting::List(vec![t("0, 0"), t("64, 80"), t("255, 255")]);
    let m = run(&dev(&[("ToneCurvePV2012", xmp)]));
    let rgb = &m.adjustments.curve.rgb;
    assert_eq!(rgb.len(), 3);
    assert!((rgb[1][0] - 64.0 / 255.0).abs() < 1e-6 && (rgb[1][1] - 80.0 / 255.0).abs() < 1e-6);
    // Catalogs hold a flat list, and the channel curves are separate.
    let flat = Setting::List([0.0, 0.0, 128.0, 100.0, 255.0, 255.0].map(n).to_vec());
    let m = run(&dev(&[
        ("ToneCurvePV2012Red", flat.clone()),
        ("ToneCurvePV2012Green", flat.clone()),
        ("ToneCurvePV2012Blue", flat),
    ]));
    for c in [
        &m.adjustments.curve.red,
        &m.adjustments.curve.green,
        &m.adjustments.curve.blue,
    ] {
        assert_eq!(c.len(), 3);
    }
    assert_eq!(m.adjustments.curve.rgb.len(), 0);
    // Unsorted points are sorted; the identity is no edit; junk is dropped with a note.
    let unsorted = Setting::List(vec![t("255, 255"), t("0, 0"), t("100, 120")]);
    let m = run(&dev(&[("ToneCurvePV2012", unsorted)]));
    assert_eq!(m.adjustments.curve.rgb[0], [0.0, 0.0]);
    for bad in [
        Setting::List(vec![t("0, 0")]),
        Setting::List(vec![t("0, 0"), t("0, 9"), t("255, 255")]),
        Setting::List(vec![t("a, b"), t("1, 2")]),
        Setting::Text("0,0,255,255".into()),
        Setting::List(vec![n(1.0), n(2.0), n(3.0)]),
        Setting::List((0..200).map(|i| n(f64::from(i))).collect()),
    ] {
        let m = run(&dev(&[("ToneCurvePV2012", bad)]));
        assert!(dropped(&m, "ToneCurvePV2012"));
        assert_eq!(m.adjustments.curve.rgb.len(), 0);
    }
    let m = run(&dev(&[("ParametricShadows", n(20.0))]));
    assert!(dropped(&m, "ParametricShadows"));
}

#[test]
fn white_balance_kelvin_for_raw_only() {
    let custom = dev(&[
        ("WhiteBalance", t("Custom")),
        ("Temperature", n(4200.0)),
        ("Tint", t("+300")),
    ]);
    let m = run(&custom);
    assert_eq!(
        m.adjustments.white_balance,
        WhiteBalance::Custom {
            temp_k: 4200.0,
            tint: 150.0
        }
    );
    assert!(m.categories.contains(&Category::WhiteBalance));
    // Without a WhiteBalance key a temperature still counts.
    assert!(fid(&run(&dev(&[("Temperature", n(6500.0))])), "Temperature").is_some());
    // The camera's own white balance is not an edit.
    let mut shot = meta(Orientation::Normal);
    shot.as_shot_wb = Some((4200.0, 0.0));
    let d = dev(&[("Temperature", n(4200.4)), ("Tint", n(0.0))]);
    assert_eq!(
        run_with(&d, &shot, true, None).adjustments.white_balance,
        WhiteBalance::AsShot
    );
    shot.as_shot_wb = Some((4200.0, 20.0));
    assert!(matches!(
        run_with(&d, &shot, true, None).adjustments.white_balance,
        WhiteBalance::Custom { .. }
    ));
    // A non-raw file's -100..100 scale (or a missing Kelvin value) is dropped.
    let m = run_with(&custom, &meta(Orientation::Normal), false, None);
    assert_eq!(m.adjustments.white_balance, WhiteBalance::AsShot);
    assert!(dropped(&m, "Temperature"));
    let m = run(&dev(&[("Temperature", n(25.0))]));
    assert!(dropped(&m, "Temperature"));
    assert_eq!(run(&dev(&[("Temperature", n(0.0))])).dropped.len(), 0);
}

#[test]
fn colour_saturation_hsl_and_grading() {
    let m = run(&dev(&[
        ("Saturation", n(-20.0)),
        ("Vibrance", n(35.0)),
        ("HueAdjustmentOrange", n(-50.0)),
        ("SaturationAdjustmentAqua", n(40.0)),
        ("LuminanceAdjustmentMagenta", n(-15.0)),
        ("HueAdjustmentRed", n(500.0)),
    ]));
    let a = &m.adjustments;
    assert_eq!(a.exposure.saturation, -20.0);
    assert_eq!(a.presence.vibrance, 35.0);
    let basic = &a.color_editor.basic;
    assert_eq!(basic[1].hue_shift, -15.0, "orange: ±100 spread over ±30°");
    assert_eq!(basic[4].saturation, 40.0, "aqua is Darkslide's cyan band");
    assert_eq!(basic[7].lightness, -15.0);
    assert_eq!(basic[0].hue_shift, 30.0, "clamped");
    assert_eq!(basic[2].saturation, 0.0);
    assert_eq!(m.categories, BTreeSet::from([Category::Color]));

    // Colour grading, with the settings that have no equivalent reported.
    let m = run(&dev(&[
        ("ColorGradeShadowHue", n(210.0)),
        ("ColorGradeShadowSat", n(40.0)),
        ("ColorGradeShadowLum", n(-10.0)),
        ("ColorGradeGlobalSat", n(20.0)),
        ("ColorGradeBlending", n(80.0)),
        ("SplitToningShadowSaturation", n(90.0)),
    ]));
    let cb = &m.adjustments.color_balance;
    assert_eq!(
        (cb.shadows.hue, cb.shadows.sat, cb.shadows.lum),
        (210.0, 0.4, -0.1)
    );
    assert_eq!(cb.master.sat, 0.2);
    assert_eq!(cb.highlights.sat, 0.0, "grading wins over split toning");
    assert!(dropped(&m, "ColorGradeBlending"));

    // Older split toning.
    let m = run(&dev(&[
        ("SplitToningShadowHue", n(220.0)),
        ("SplitToningShadowSaturation", n(30.0)),
        ("SplitToningHighlightSaturation", n(10.0)),
        ("SplitToningBalance", n(25.0)),
    ]));
    let cb = &m.adjustments.color_balance;
    assert_eq!((cb.shadows.hue, cb.shadows.sat), (220.0, 0.3));
    assert_eq!(cb.highlights.sat, 0.1);
    assert!(dropped(&m, "SplitToningBalance"));

    // Black and white.
    let m = run(&dev(&[
        ("ConvertToGrayscale", t("True")),
        ("Saturation", n(30.0)),
    ]));
    assert_eq!(m.adjustments.exposure.saturation, -100.0);
    assert!(dropped(&m, "GrayMixer"));
}

#[test]
fn detail_calibration_lands_lightrooms_defaults_on_darkslides() {
    let m = run(&dev(&[
        ("Texture", n(25.0)),
        ("Clarity2012", n(-10.0)),
        ("Dehaze", n(30.0)),
        ("Sharpness", n(80.0)),
        ("SharpenRadius", n(1.5)),
        ("SharpenEdgeMasking", n(50.0)),
        ("SharpenDetail", n(60.0)),
        ("LuminanceSmoothing", n(35.0)),
        ("ColorNoiseReduction", n(50.0)),
        ("LuminanceNoiseReductionDetail", n(70.0)),
        ("LuminanceNoiseReductionContrast", n(30.0)),
        ("ColorNoiseReductionDetail", n(10.0)),
    ]));
    let a = &m.adjustments;
    assert_eq!(a.presence.texture, 25.0);
    assert_eq!(a.detail.clarity, -10.0);
    assert_eq!(a.presence.dehaze, 30.0);
    // 40 → 140 and 1.0 → 0.8 are the two defaults.
    assert_eq!(a.detail.sharpen_amount, 280.0);
    assert!((a.detail.sharpen_radius - 1.2).abs() < 1e-6);
    assert!((a.detail.sharpen_threshold - 5.5).abs() < 1e-5);
    assert_eq!(
        (a.noise.luminance, a.noise.color, a.noise.detail),
        (35.0, 50.0, 70.0)
    );
    assert!(dropped(&m, "SharpenDetail"));
    assert!(dropped(&m, "LuminanceNoiseReductionContrast"));
    assert!(dropped(&m, "ColorNoiseReductionDetail"));
    assert_eq!(m.categories, BTreeSet::from([Category::Detail]));
    let m = run(&dev(&[("Sharpness", n(40.0 * 100.0))]));
    assert_eq!(m.adjustments.detail.sharpen_amount, 1000.0, "clamped");
}

#[test]
fn vignette_and_grain() {
    let m = run(&dev(&[
        ("PostCropVignetteAmount", n(-50.0)),
        ("PostCropVignetteMidpoint", n(30.0)),
        ("PostCropVignetteFeather", n(40.0)),
        ("PostCropVignetteRoundness", n(10.0)),
        ("GrainAmount", n(35.0)),
        ("GrainSize", n(40.0)),
        ("GrainFrequency", n(70.0)),
    ]));
    let a = &m.adjustments;
    assert!((a.vignette.amount_ev + 1.5).abs() < 1e-6);
    assert!((a.vignette.midpoint - 0.3).abs() < 1e-6);
    assert!(dropped(&m, "PostCropVignetteFeather"));
    assert!(dropped(&m, "PostCropVignetteRoundness"));
    let g = a.effects.grain;
    assert_eq!((g.amount, g.size, g.roughness), (35.0, 40.0, 70.0));
    assert_eq!(m.categories, BTreeSet::from([Category::Effects]));
    // Settings of an unused vignette or grain say nothing.
    let m = run(&dev(&[
        ("PostCropVignetteFeather", n(10.0)),
        ("GrainSize", n(60.0)),
    ]));
    assert_eq!(m.dropped.len(), 0);
    assert_eq!(m.adjustments, Adjustments::default());
}

#[test]
fn lens_corrections() {
    let m = run(&dev(&[
        ("LensProfileEnable", n(0.0)),
        ("AutoLateralCA", n(0.0)),
        ("LensProfileDistortionScale", n(60.0)),
        ("LensProfileVignettingScale", n(150.0)),
    ]));
    let l = &m.adjustments.lens;
    assert!(!l.enabled && !l.chromatic_aberration);
    assert_eq!(l.distortion, 0.6);
    assert_eq!(l.light_falloff, 0.75);
    assert_eq!(m.categories, BTreeSet::from([Category::Lens]));
    assert_eq!(fid(&m, "LensProfileEnable"), Some(Fidelity::Exact));
    let m = run(&dev(&[
        ("LensProfileEnable", t("1")),
        ("LensProfileName", t("Adobe (Some 24mm F2.8)")),
    ]));
    assert!(m.adjustments.lens.enabled);
    assert!(dropped(&m, "LensProfileName"));
    assert!(m.categories.is_empty());
}

#[test]
fn quarter_turn_frames() {
    // E → LR (a, b) ↦ (1-b, a) for one turn; the helper goes the other way.
    assert_eq!(to_exif_frame(0, (0.2, 0.3)), (0.2, 0.3));
    let (x, y) = to_exif_frame(1, (1.0 - 0.3, 0.2));
    assert!((x - 0.2).abs() < 1e-9 && (y - 0.3).abs() < 1e-9);
    let (x, y) = to_exif_frame(2, (1.0 - 0.2, 1.0 - 0.3));
    assert!((x - 0.2).abs() < 1e-9 && (y - 0.3).abs() < 1e-9);
    let (x, y) = to_exif_frame(3, (0.3, 1.0 - 0.2));
    assert!((x - 0.2).abs() < 1e-9 && (y - 0.3).abs() < 1e-9);
    assert_eq!(to_exif_frame(5, (0.1, 0.9)), to_exif_frame(1, (0.1, 0.9)));
}

#[test]
fn crop_rectangles_and_angles() {
    let rect = dev(&[
        ("HasCrop", t("True")),
        ("CropLeft", t("0.1")),
        ("CropTop", t("0.2")),
        ("CropRight", t("0.9")),
        ("CropBottom", t("0.7")),
        ("CropAngle", t("0")),
    ]);
    let m = run(&rect);
    let c = &m.adjustments.crop;
    assert!((c.rect.x - 0.1).abs() < 1e-6 && (c.rect.y - 0.2).abs() < 1e-6);
    assert!((c.rect.w - 0.8).abs() < 1e-6 && (c.rect.h - 0.5).abs() < 1e-6);
    assert_eq!(c.angle_deg, 0.0);
    assert_eq!(fid(&m, "CropLeft"), Some(Fidelity::Exact));
    assert_eq!(m.categories, BTreeSet::from([Category::Crop]));

    // A straightened crop is approximate.
    let mut tilted = rect.clone();
    tilted.insert("CropAngle".into(), t("-2.5"));
    let m = run(&tilted);
    assert_eq!(m.adjustments.crop.angle_deg, -2.5);
    assert_eq!(fid(&m, "CropLeft"), Some(Fidelity::Approximate));
    assert_eq!(fid(&m, "CropAngle"), Some(Fidelity::Approximate));
    // Straightening alone, no box.
    let m = run(&dev(&[("CropAngle", n(1.0))]));
    assert_eq!(m.adjustments.crop.angle_deg, 1.0);
    assert!(m.adjustments.crop.rect.is_full());

    // HasCrop false, a full frame and an empty box are no crop.
    let mut off = rect.clone();
    off.insert("HasCrop".into(), t("False"));
    assert_eq!(run(&off).adjustments.crop, ds_core::adjust::Crop::default());
    let full = dev(&[("CropLeft", n(0.0)), ("CropRight", n(1.0))]);
    assert_eq!(run(&full).mapped.len(), 0);
    let empty = dev(&[("CropLeft", n(0.6)), ("CropRight", n(0.4))]);
    let m = run(&empty);
    assert!(dropped(&m, "CropLeft"));
    assert!(m.adjustments.crop.rect.is_full());
    // Out-of-range fractions are ignored as if absent.
    assert_eq!(run(&dev(&[("CropLeft", n(7.0))])).mapped.len(), 0);
}

#[test]
fn user_rotation_turns_the_frame() {
    // The file is landscape (EXIF normal); Lightroom shows it rotated 90° clockwise (6).
    let m0 = meta(Orientation::Normal);
    let d = dev(&[
        ("CropLeft", n(0.1)),
        ("CropTop", n(0.2)),
        ("CropRight", n(0.6)),
        ("CropBottom", n(0.9)),
    ]);
    let m = run_with(&d, &m0, true, Some(6));
    let c = &m.adjustments.crop;
    assert_eq!(c.quarter_turns, 1);
    // LR (x, y) = (1 - b, a): the E-frame box is x in [t, bottom], y in [1 - right, 1 - left].
    assert!((c.rect.x - 0.2).abs() < 1e-6 && (c.rect.y - 0.4).abs() < 1e-6);
    assert!((c.rect.w - 0.7).abs() < 1e-6 && (c.rect.h - 0.5).abs() < 1e-6);
    assert_eq!(fid(&m, "Orientation"), Some(Fidelity::Exact));
    // 180° and 270°.
    let c = run_with(&d, &m0, true, Some(3)).adjustments.crop;
    assert_eq!(c.quarter_turns, 2);
    assert!((c.rect.x - 0.4).abs() < 1e-6 && (c.rect.y - 0.1).abs() < 1e-6);
    let c = run_with(&d, &m0, true, Some(8)).adjustments.crop;
    assert_eq!(c.quarter_turns, 3);
    assert!((c.rect.x - 0.1).abs() < 1e-6 && (c.rect.y - 0.1).abs() < 1e-6);
    // The file's own EXIF rotation is not a user rotation.
    let portrait = meta(Orientation::Rotate90);
    let m = run_with(&d, &portrait, true, Some(6));
    assert_eq!(m.adjustments.crop.quarter_turns, 0);
    assert!(!m.mapped.iter().any(|(k, _)| k == "Orientation"));
    // …and a rotation relative to it is.
    assert_eq!(
        run_with(&dev(&[]), &portrait, true, Some(1))
            .adjustments
            .crop
            .quarter_turns,
        3
    );
    // Rotation with no crop at all.
    assert_eq!(
        run_with(&dev(&[]), &m0, true, Some(6))
            .adjustments
            .crop
            .quarter_turns,
        1
    );
    // Mirrored orientations that differ can't be expressed; unknown values are ignored.
    let m = run_with(&d, &m0, true, Some(2));
    assert!(dropped(&m, "Orientation"));
    assert_eq!(m.adjustments.crop.quarter_turns, 0);
    assert_eq!(run_with(&dev(&[]), &m0, true, Some(9)).mapped.len(), 0);
}

#[test]
fn old_process_versions_and_unmapped_sliders_are_reported() {
    let m = run(&dev(&[
        ("Exposure", n(0.5)),
        ("Brightness", n(20.0)),
        ("FillLight", n(10.0)),
        ("Clarity", n(30.0)),
        ("PerspectiveVertical", n(12.0)),
        ("DefringeGreenAmount", n(4.0)),
        ("CameraProfile", t("Camera Vivid")),
    ]));
    for k in [
        "Exposure",
        "Brightness",
        "FillLight",
        "Clarity",
        "PerspectiveVertical",
        "DefringeGreenAmount",
        "CameraProfile",
    ] {
        assert!(dropped(&m, k), "{k}");
    }
    assert_eq!(m.adjustments, Adjustments::default());
    // The 2012 key next to the old one wins, and Adobe Standard is the default.
    let m = run(&dev(&[
        ("Exposure", n(0.5)),
        ("Exposure2012", n(0.5)),
        ("CameraProfile", t("Adobe Standard")),
    ]));
    assert!(!dropped(&m, "Exposure"));
    assert!(!dropped(&m, "CameraProfile"));
}

fn gradient_correction() -> Setting {
    table(&[
        ("LocalExposure2012", n(-0.8)),
        ("LocalHighlights2012", n(-40.0)),
        ("LocalTemperature", n(20.0)),
        ("ZeroX", n(0.5)),
        ("ZeroY", n(0.6)),
        ("FullX", n(0.5)),
        ("FullY", n(0.1)),
    ])
}

#[test]
fn linear_and_radial_gradients_become_layers() {
    let d = dev(&[
        (
            "GradientBasedCorrections",
            Setting::List(vec![gradient_correction()]),
        ),
        (
            "CircularGradientBasedCorrections",
            Setting::List(vec![table(&[
                ("LocalContrast2012", n(25.0)),
                ("Top", n(0.25)),
                ("Left", n(0.25)),
                ("Bottom", n(0.75)),
                ("Right", n(0.75)),
                ("Angle", n(30.0)),
                ("Feather", n(60.0)),
                ("Flipped", t("false")),
            ])]),
        ),
    ]);
    let m = run(&d);
    assert_eq!(m.categories, BTreeSet::from([Category::Masks]));
    let layers = &m.adjustments.layers;
    assert_eq!(layers.len(), 2);
    let l = &layers[0];
    assert!(l.name.starts_with("Lightroom"));
    assert_eq!(l.adjustments.exposure.exposure_ev, -0.8);
    assert_eq!(l.adjustments.hdr.highlights, 40.0);
    let MaskShape::Linear { start, end, op } = &l.mask.shapes[0] else {
        panic!("{:?}", l.mask.shapes)
    };
    assert_eq!((*op, *start, *end), (MaskOp::Add, [0.5, 0.1], [0.5, 0.6]));
    assert!(dropped(&m, "LocalTemperature"));
    let r = &layers[1];
    let MaskShape::Radial {
        center,
        radius,
        angle_deg,
        feather,
        ..
    } = &r.mask.shapes[0]
    else {
        panic!("{:?}", r.mask.shapes)
    };
    assert_eq!(*center, [0.5, 0.5]);
    // Half the width, relative to the long edge; half the height likewise.
    assert!((radius[0] - 0.25).abs() < 1e-6 && (radius[1] - 1000.0 / 6000.0).abs() < 1e-6);
    assert_eq!(*angle_deg, 30.0);
    assert_eq!(*feather, 0.6);
    assert!(
        r.mask.invert,
        "UNCONFIRMED polarity: Flipped=false is outside"
    );
    assert!(fid(&m, "GradientBasedCorrections").is_some());
}

#[test]
fn radial_gradients_follow_a_rotated_frame() {
    let m0 = meta(Orientation::Normal);
    let d = dev(&[(
        "CircularGradientBasedCorrections",
        Setting::List(vec![table(&[
            ("LocalExposure2012", n(0.5)),
            ("Top", n(0.1)),
            ("Left", n(0.2)),
            ("Bottom", n(0.5)),
            ("Right", n(0.8)),
            ("Angle", n(10.0)),
            ("Flipped", t("true")),
        ])]),
    )]);
    let m = run_with(&d, &m0, true, Some(6));
    let MaskShape::Radial {
        center,
        radius,
        angle_deg,
        ..
    } = &m.adjustments.layers[0].mask.shapes[0]
    else {
        panic!()
    };
    // Lightroom's centre (0.5, 0.3) is E (0.3, 0.5); the angle loses the 90° turn.
    assert!((center[0] - 0.3).abs() < 1e-6 && (center[1] - 0.5).abs() < 1e-6);
    assert!((angle_deg + 80.0).abs() < 1e-4);
    // LR frame is 4000×6000: semi-axes 0.3·4000 and 0.2·6000 px over the 6000 px long edge.
    assert!((radius[0] - 0.2).abs() < 1e-5 && (radius[1] - 0.2).abs() < 1e-5);
    assert!(!m.adjustments.layers[0].mask.invert);
}

#[test]
fn masking_model_components() {
    let comps = Setting::List(vec![
        table(&[
            ("What", t("Mask/Gradient")),
            ("ZeroX", n(0.1)),
            ("ZeroY", n(0.9)),
            ("FullX", n(0.1)),
            ("FullY", n(0.2)),
            ("MaskInverted", t("true")),
        ]),
        table(&[
            ("What", t("Mask/Image")),
            ("MaskSubType", n(1.0)),
            ("MaskBlendMode", n(1.0)),
            ("MaskInverted", t("true")),
        ]),
    ]);
    let d = dev(&[(
        "MaskGroupBasedCorrections",
        Setting::List(vec![
            table(&[
                ("CorrectionName", t("Sky darken")),
                ("CorrectionAmount", t("0.5")),
                ("LocalExposure2012", n(-1.0)),
                ("LocalSaturation", n(10.0)),
                ("CorrectionMasks", comps),
            ]),
            // Disabled, sliders at zero (skipped), AI types that can't be recreated.
            table(&[
                ("CorrectionActive", t("false")),
                ("LocalClarity2012", n(30.0)),
                (
                    "CorrectionMasks",
                    Setting::List(vec![
                        table(&[("What", t("Mask/Image")), ("MaskSubType", n(2.0))]),
                        table(&[("What", t("Mask/Image")), ("MaskSubType", n(3.0))]),
                    ]),
                ),
            ]),
            table(&[
                ("LocalExposure2012", n(0.0)),
                (
                    "CorrectionMasks",
                    Setting::List(vec![table(&[("What", t("Mask/Gradient"))])]),
                ),
            ]),
            table(&[
                ("LocalExposure2012", n(0.3)),
                (
                    "CorrectionMasks",
                    Setting::List(vec![
                        table(&[("What", t("Mask/Image")), ("MaskSubType", n(7.0))]),
                        table(&[("What", t("Mask/Image"))]),
                        table(&[("What", t("Mask/RangeMask"))]),
                        table(&[("What", t(""))]),
                    ]),
                ),
            ]),
        ]),
    )]);
    let m = run(&d);
    let layers = &m.adjustments.layers;
    assert_eq!(layers.len(), 2, "{layers:?}");
    let l = &layers[0];
    assert_eq!(l.name, "Lightroom: Sky darken");
    assert_eq!(l.opacity, 0.5);
    assert_eq!(l.adjustments.exposure.saturation, 10.0);
    assert!(l.mask.invert, "both components are inverted");
    assert!(matches!(l.mask.shapes[0], MaskShape::Linear { .. }));
    let MaskShape::Ai { op, target, .. } = &l.mask.shapes[1] else {
        panic!()
    };
    assert_eq!((*op, *target), (MaskOp::Subtract, AiTarget::Subject));
    let off = &layers[1];
    assert!(!off.enabled);
    assert_eq!(off.name, "Lightroom: Background");
    assert_eq!(off.mask.shapes.len(), 2);
    assert!(matches!(
        off.mask.shapes[0],
        MaskShape::Ai {
            target: AiTarget::Sky,
            ..
        }
    ));
    for k in [
        "Mask/Image type 7",
        "Mask/Image bitmap",
        "Mask Mask/RangeMask",
        "Mask unnamed",
    ] {
        assert!(dropped(&m, k), "{k}: {:?}", m.dropped);
    }
}

#[test]
fn mixed_inversion_and_empty_masks() {
    let comps = Setting::List(vec![
        table(&[
            ("What", t("Mask/Gradient")),
            ("ZeroX", n(0.0)),
            ("ZeroY", n(0.0)),
            ("FullX", n(1.0)),
            ("FullY", n(1.0)),
            ("MaskInverted", t("true")),
        ]),
        table(&[
            ("What", t("Mask/CircularGradient")),
            ("Top", n(0.2)),
            ("Left", n(0.2)),
            ("Bottom", n(0.6)),
            ("Right", n(0.6)),
        ]),
    ]);
    let m = run(&dev(&[(
        "MaskGroupBasedCorrections",
        Setting::List(vec![table(&[
            ("LocalDehaze", n(20.0)),
            ("CorrectionMasks", comps),
        ])]),
    )]));
    let l = &m.adjustments.layers[0];
    assert!(!l.mask.invert);
    assert!(dropped(&m, "MaskInverted"));
    assert_eq!(l.adjustments.presence.dehaze, 20.0);
    // Degenerate shapes produce no layer at all.
    let degenerate = |what: &str, items: &[(&str, Setting)]| {
        let mut all = vec![("What", t(what))];
        all.extend(items.iter().cloned());
        run(&dev(&[(
            "MaskGroupBasedCorrections",
            Setting::List(vec![table(&[
                ("LocalDehaze", n(20.0)),
                ("CorrectionMasks", Setting::List(vec![table(&all)])),
            ])]),
        )]))
    };
    let m = degenerate(
        "Mask/Gradient",
        &[
            ("ZeroX", n(0.5)),
            ("ZeroY", n(0.5)),
            ("FullX", n(0.5)),
            ("FullY", n(0.5)),
        ],
    );
    assert_eq!(m.adjustments.layers.len(), 0);
    assert!(dropped(&m, "GradientBasedCorrections"));
    let m = degenerate(
        "Mask/CircularGradient",
        &[
            ("Top", n(0.5)),
            ("Left", n(0.5)),
            ("Bottom", n(0.4)),
            ("Right", n(0.9)),
        ],
    );
    assert_eq!(m.adjustments.layers.len(), 0);
    assert!(dropped(&m, "CircularGradientBasedCorrections"));
    assert_eq!(
        degenerate("Mask/Gradient", &[("ZeroX", t("x"))])
            .adjustments
            .layers
            .len(),
        0
    );
    // No sensor size, no ellipse.
    let mut tiny = meta(Orientation::Normal);
    tiny.width = 0;
    let circle = dev(&[(
        "CircularGradientBasedCorrections",
        Setting::List(vec![table(&[
            ("LocalDehaze", n(20.0)),
            ("Top", n(0.2)),
            ("Left", n(0.2)),
            ("Bottom", n(0.6)),
            ("Right", n(0.6)),
        ])]),
    )]);
    assert_eq!(
        run_with(&circle, &tiny, true, None)
            .adjustments
            .layers
            .len(),
        0
    );
}

#[test]
fn brush_strokes() {
    let dabs = Setting::List(vec![
        t("m 0.1 0.2"),
        t("d 0.2 0.25"),
        t("d 0.3 0.3"),
        t("m 0.8 0.8"),
        t("d 0.85 0.8"),
        t("garbage"),
        t("d x y"),
        Setting::Num(3.0),
        t(""),
    ]);
    let d = dev(&[(
        "PaintBasedCorrections",
        Setting::List(vec![table(&[
            ("LocalExposure2012", n(0.4)),
            (
                "CorrectionMasks",
                Setting::List(vec![table(&[
                    ("What", t("Mask/Brush")),
                    ("Dabs", dabs),
                    ("Radius", n(0.02)),
                    ("Flow", n(0.5)),
                    ("CenterWeight", n(0.8)),
                ])]),
            ),
        ])]),
    )]);
    let m = run(&d);
    let l = &m.adjustments.layers[0];
    assert_eq!(l.name, "Lightroom: Brush");
    let MaskShape::Brush { strokes, .. } = &l.mask.shapes[0] else {
        panic!()
    };
    assert_eq!(strokes.len(), 2);
    assert_eq!(strokes[0].points.len(), 3);
    assert_eq!(strokes[1].points, vec![[0.8, 0.8], [0.85, 0.8]]);
    assert!((strokes[0].size - 0.04).abs() < 1e-6);
    assert_eq!((strokes[0].flow, strokes[0].hardness), (0.5, 0.8));
    // A brush with no readable dabs is no layer.
    let none = dev(&[(
        "PaintBasedCorrections",
        Setting::List(vec![table(&[
            ("LocalExposure2012", n(0.4)),
            (
                "CorrectionMasks",
                Setting::List(vec![table(&[("Dabs", Setting::List(vec![t("nonsense")]))])]),
            ),
        ])]),
    )]);
    assert_eq!(run(&none).adjustments.layers.len(), 0);
}

#[test]
fn hostile_mask_lists_are_bounded() {
    let many = Setting::List((0..500).map(|_| gradient_correction()).collect());
    let m = run(&dev(&[("GradientBasedCorrections", many)]));
    assert_eq!(
        m.adjustments.layers.len(),
        crate::map_local::MAX_CORRECTIONS
    );
    assert!(dropped(&m, "GradientBasedCorrections"));
    // Not a list, or a list of non-tables: nothing.
    let m = run(&dev(&[
        ("MaskGroupBasedCorrections", t("x")),
        ("PaintBasedCorrections", Setting::List(vec![n(1.0)])),
    ]));
    assert_eq!(m.adjustments.layers.len(), 0);
    // A stroke with a million dabs stops at the cap.
    let dabs = Setting::List((0..30_000).map(|_| t("d 0.5 0.5")).collect());
    let m = run(&dev(&[(
        "PaintBasedCorrections",
        Setting::List(vec![table(&[
            ("LocalExposure2012", n(0.4)),
            (
                "CorrectionMasks",
                Setting::List(vec![table(&[("Dabs", dabs)])]),
            ),
        ])]),
    )]));
    let MaskShape::Brush { strokes, .. } = &m.adjustments.layers[0].mask.shapes[0] else {
        panic!()
    };
    assert_eq!(strokes[0].points.len(), ds_core::limits::MAX_POINTS);
    assert!(dropped(&m, "Dabs"));
}

#[test]
fn wrong_types_never_panic() {
    let weird: BTreeMap<String, Setting> = [
        "Exposure2012",
        "CropLeft",
        "Temperature",
        "HasCrop",
        "Sharpness",
    ]
    .iter()
    .map(|k| {
        (
            (*k).to_owned(),
            Setting::List(vec![Setting::Table(BTreeMap::new())]),
        )
    })
    .collect();
    let m = run(&weird);
    assert_eq!(m.adjustments, Adjustments::default());
    let nan = dev(&[("Exposure2012", n(f64::NAN)), ("CropAngle", t("inf"))]);
    assert_eq!(run(&nan).mapped.len(), 0);
}
