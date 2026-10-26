#![allow(clippy::float_cmp)]

use lt_core::geom::Orientation;
use lt_session::ImageMetaLite;

use super::*;

fn meta() -> ImageMetaLite {
    ImageMetaLite {
        width: 6000,
        height: 4000,
        orientation: Orientation::Normal,
        ..ImageMetaLite::default()
    }
}

fn dev(pairs: &[(&str, Setting)]) -> Develop {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

fn n(v: f64) -> Setting {
    Setting::Num(v)
}

fn map(d: &Develop) -> Mapped {
    map_raw(d, true)
}

fn map_raw(d: &Develop, is_raw: bool) -> Mapped {
    let m = meta();
    map_adjustments(
        d,
        &MapContext {
            meta: &m,
            is_raw,
            lr_orientation: None,
        },
    )
}

fn mapped(m: &Mapped) -> Vec<&str> {
    m.mapped.iter().map(|(k, _)| k.as_str()).collect()
}

fn dropped<'a>(m: &'a Mapped, k: &str) -> Option<&'a str> {
    m.dropped
        .iter()
        .find(|(n, _)| n == k)
        .map(|(_, r)| r.as_str())
}

#[test]
fn exposure_enhance_and_highlights_shadows_map_onto_luxtable_tone_and_colour() {
    let m = map(&dev(&[
        ("RKExposureOperation.present", Setting::Bool(true)),
        ("RKExposureOperation.inputExposure", n(7.0)),
        ("RKExposureOperation.inputRecovery", n(0.75)),
        ("RKExposureOperation.inputBlackPoint", n(5.0)),
        ("RKExposureOperation.inputBrightness", n(-0.5)),
        ("RKEnhanceOperation.inputContrast", n(0.25)),
        ("RKEnhanceOperation.inputDefinition", n(0.4)),
        ("RKEnhanceOperation.inputSaturation", n(1.5)),
        ("RKEnhanceOperation.inputVibrancy", n(-0.2)),
        ("RKShadowHighlightOperation.inputShadows", n(30.0)),
        ("RKShadowHighlightOperation.inputHighlights", n(150.0)),
    ]));
    let a = &m.adjustments;
    assert_eq!(a.exposure.exposure_ev, 5.0, "clamped");
    assert_eq!(a.exposure.brightness, -25.0);
    assert_eq!(a.exposure.contrast, 25.0);
    assert_eq!(a.exposure.saturation, 50.0);
    assert!((a.presence.vibrance + 20.0).abs() < 1e-4);
    assert!((a.detail.clarity - 40.0).abs() < 1e-4);
    assert_eq!(a.hdr.blacks, -10.0);
    assert_eq!(a.hdr.shadows, 30.0);
    // Highlights & Shadows comes after Exposure (key order) and sets highlights last.
    assert_eq!(a.hdr.highlights, 100.0);
    for k in [
        "Exposure",
        "Recovery",
        "Black Point",
        "Brightness",
        "Contrast",
        "Definition",
        "Saturation",
        "Vibrancy",
        "Highlights",
        "Shadows",
    ] {
        assert!(mapped(&m).contains(&k), "{k}: {m:?}");
    }
    assert!(m.mapped.iter().all(|(_, f)| *f == Fidelity::Approximate));
    for c in [Category::Tone, Category::Color, Category::Detail] {
        assert!(m.categories.contains(&c));
    }
    assert!(m.dropped.is_empty(), "{:?}", m.dropped);
}

#[test]
fn neutral_values_are_not_edits() {
    let m = map(&dev(&[
        ("RKExposureOperation.inputExposure", n(0.0)),
        ("RKEnhanceOperation.inputSaturation", n(1.0)),
    ]));
    assert_eq!(m.mapped, Vec::new());
    assert_eq!(m.categories, std::collections::BTreeSet::new());
    assert_eq!(m.adjustments, lt_core::Adjustments::default().sanitized());
}

#[test]
fn white_balance_carries_over_for_raw_files_only() {
    let wb = dev(&[
        ("RKWhiteBalanceOperation.inputTemperature", n(5200.0)),
        ("RKWhiteBalanceOperation.inputTint", n(500.0)),
    ]);
    let m = map(&wb);
    assert_eq!(
        m.adjustments.white_balance,
        WhiteBalance::Custom {
            temp_k: 5200.0,
            tint: 150.0
        }
    );
    assert!(m.categories.contains(&Category::WhiteBalance));
    let jpeg = map_raw(&wb, false);
    assert_eq!(jpeg.adjustments.white_balance, WhiteBalance::default());
    assert!(dropped(&jpeg, "White Balance").unwrap().contains("RAW"));
    let relative = map(&dev(&[("RKWhiteBalanceOperation.inputTemp", n(20.0))]));
    assert!(dropped(&relative, "White Balance").is_some());
    // No temperature at all: the settings weren't recognised.
    let none = map(&dev(&[("RKWhiteBalanceOperation.inputFace", n(1.0))]));
    assert!(
        dropped(&none, "White Balance")
            .unwrap()
            .contains("recognised")
    );
}

#[test]
fn straighten_and_crop_become_luxtable_crop() {
    // 600×400 px from (600, 400), origin at the bottom left, on a 6000×4000 photo.
    let crop = [
        ("RKCropOperation.inputXOrigin", n(600.0)),
        ("RKCropOperation.inputYOrigin", n(400.0)),
        ("RKCropOperation.inputWidth", n(3000.0)),
        ("RKCropOperation.inputHeight", n(2000.0)),
    ];
    let mut d = dev(&crop);
    d.insert("RKStraightenCropOperation.inputRotation".into(), n(2.5));
    let m = map(&d);
    let c = &m.adjustments.crop;
    assert!((c.rect.x - 0.1).abs() < 1e-6, "{c:?}");
    assert!(
        (c.rect.y - 0.4).abs() < 1e-6,
        "top = 1 - (400 + 2000) / 4000"
    );
    assert!((c.rect.w - 0.5).abs() < 1e-6 && (c.rect.h - 0.5).abs() < 1e-6);
    assert_eq!(c.angle_deg, 2.5);
    assert!(mapped(&m).contains(&"Crop") && mapped(&m).contains(&"Straighten"));
    // On a photo turned 180°, the rectangle turns with it.
    let mut d = dev(&crop);
    d.insert(ROTATION.into(), n(-180.0));
    let m = map(&d);
    let c = &m.adjustments.crop;
    assert_eq!(c.quarter_turns, 2);
    assert!(
        (c.rect.x - 0.4).abs() < 1e-6 && (c.rect.y - 0.1).abs() < 1e-6,
        "{c:?}"
    );
    // A quarter turn: neither the turn nor the crop is guessed.
    let mut d = dev(&crop);
    d.insert(ROTATION.into(), n(90.0));
    let m = map(&d);
    assert_eq!(m.adjustments.crop.quarter_turns, 0);
    assert!(dropped(&m, "Rotate").is_some());
    assert!(dropped(&m, "Crop").unwrap().contains("quarter-turned"));
    // Outside the photo, or the whole photo.
    let out = map(&dev(&[
        ("RKCropOperation.inputXOrigin", n(5000.0)),
        ("RKCropOperation.inputYOrigin", n(0.0)),
        ("RKCropOperation.inputWidth", n(3000.0)),
        ("RKCropOperation.inputHeight", n(2000.0)),
    ]));
    assert!(dropped(&out, "Crop").unwrap().contains("outside"));
    let whole = map(&dev(&[
        ("RKCropOperation.inputXOrigin", n(0.0)),
        ("RKCropOperation.inputYOrigin", n(0.0)),
        ("RKCropOperation.inputWidth", n(6000.0)),
        ("RKCropOperation.inputHeight", n(4000.0)),
    ]));
    assert!(whole.mapped.is_empty() && whole.dropped.is_empty());
    // Straightening by nothing, and a crop without its numbers.
    let flat = map(&dev(&[("RKStraightenOperation.inputAngle", n(0.0))]));
    assert_eq!(flat.mapped, Vec::new());
    let vague = map(&dev(&[
        ("RKCropOperation.inputXOrigin", n(1.0)),
        (
            "RKStraightenOperation.inputVector",
            Setting::Text("<CIVector>".into()),
        ),
    ]));
    assert!(dropped(&vague, "Crop").unwrap().contains("recognised"));
    assert!(dropped(&vague, "Straighten").is_some());
    // A full turn is no turn.
    let round = map(&dev(&[(ROTATION, n(360.0))]));
    assert!(round.mapped.is_empty() && round.dropped.is_empty());
}

#[test]
fn black_and_white_desaturates_and_reports_its_mix() {
    let m = map(&dev(&[("RKBlackAndWhiteOperation.inputRed", n(0.3))]));
    assert_eq!(m.adjustments.exposure.saturation, -100.0);
    assert!(mapped(&m).contains(&"Black & White"));
    assert!(dropped(&m, "Black & White mix").is_some());
}

#[test]
fn everything_else_is_reported_not_approximated() {
    let ops = [
        ("RKLevelsOperation", "levels"),
        ("RKCurvesOperation", "curves"),
        ("RKSharpenOperation", "sharpens"),
        ("RKNoiseReductionOperation", "noise"),
        ("RKVignetteOperation", "vignettes"),
        ("RKRetouchOperation", "retouching"),
        ("RKRedEyeOperation", "red-eye"),
        ("DGiOSEditsoperation", "iOS"),
        ("RKColorOperation", "no Luxtable equivalent"),
    ];
    let mut d = Develop::new();
    for (op, _) in ops {
        d.insert(format!("{op}.present"), Setting::Bool(true));
    }
    let m = map(&d);
    for (op, why) in ops {
        let r = dropped(&m, &display_name(op)).unwrap_or_else(|| panic!("{op}: {m:?}"));
        assert!(r.contains(why), "{op}: {r}");
    }
    assert_eq!(m.mapped, Vec::new());
}

#[test]
fn raw_decoding_disabled_masked_and_repeated_adjustments() {
    let m = map(&dev(&[
        ("RKRawDecodeOperation.inputBoost", n(0.5)),
        ("RKExposureOperation.inputExposure", n(1.0)),
        ("RKExposureOperation.enabled", Setting::Bool(false)),
        ("RKEnhanceOperation.inputContrast", n(0.5)),
        ("RKEnhanceOperation.mask", Setting::Bool(true)),
        ("RKShadowHighlightOperation.inputShadows", n(10.0)),
        ("RKShadowHighlightOperation~2.inputShadows", n(50.0)),
    ]));
    assert_eq!(m.adjustments.exposure.exposure_ev, 0.0, "disabled");
    assert_eq!(m.adjustments.exposure.contrast, 0.0, "brushed in");
    assert!(dropped(&m, "Enhance").unwrap().contains("brush"));
    assert_eq!(m.adjustments.hdr.shadows, 10.0, "the first one");
    assert!(
        dropped(&m, "Shadow Highlight")
            .unwrap()
            .contains("repeated")
    );
    assert!(dropped(&m, "Raw Decode").is_none());
}

#[test]
fn an_iphoto_edit_is_reported() {
    let m = map(&dev(&[(IPHOTO_EDIT, Setting::Bool(true))]));
    assert!(dropped(&m, "iPhoto edit").unwrap().contains("unedited"));
    // Keys without an adjustment name are not adjustments.
    let m = map(&dev(&[("loose", n(1.0))]));
    assert_eq!(m.dropped, Vec::new());
}

#[test]
fn adjustment_names_read_as_words() {
    assert_eq!(display_name("RKExposureOperation"), "Exposure");
    assert_eq!(
        display_name("RKHighlightsAndShadowsOperation"),
        "Highlights And Shadows"
    );
    assert_eq!(display_name("RKRedEyeOperation~2"), "Red Eye");
    assert_eq!(display_name("iPhotoEffects"), "i Photo Effects");
    assert_eq!(display_name("RKOperation"), "RKOperation");
    assert_eq!(display_name("RKDGiOS"), "D Gi OS");
}
