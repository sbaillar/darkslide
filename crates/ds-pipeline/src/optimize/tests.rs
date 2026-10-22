#![allow(clippy::field_reassign_with_default)]

use ds_core::adjust::WhiteBalance;
use ds_core::{Adjustments, NormRect, RawImage, Size};

use super::rules::{self, Ctx, Outcome, Owner};
use super::*;
use crate::color::{CameraColor, temp_tint_to_multipliers};
use crate::testutil::{noise, raw_from_fn};

/// A textured scene: `f(u, v)` (white-balanced reflectance × light, fraction of clip)
/// with ±`tex` texture in 4 px cells (detail, not sensor noise).
fn scene(w: u32, h: u32, tex: f32, f: impl Fn(f32, f32) -> [f32; 3]) -> RawImage {
    raw_from_fn(w, h, |x, y| {
        let t = 1.0 + tex * noise(x / 4, y / 4);
        f(x as f32 / w as f32, y as f32 / h as f32).map(|c| c * t)
    })
}

/// A varied, natural scene at `level` (mean fraction of clip).
fn varied(level: f32) -> RawImage {
    scene(256, 192, 0.15, move |u, v| {
        let patch = ((u * 6.0) as usize + (v * 4.0) as usize * 6) % 6;
        let refl = [
            [0.9, 0.9, 0.9],
            [1.6, 0.9, 0.5],
            [0.5, 1.2, 0.6],
            [0.6, 0.8, 1.5],
            [0.25, 0.25, 0.25],
            [1.3, 1.2, 1.0],
        ][patch];
        let ramp = 0.4 + 1.2 * u;
        refl.map(|r| r * level * ramp)
    })
}

fn input<'a>(raw: &'a RawImage, adj: &'a Adjustments, scene: &'a SceneSignals) -> Input<'a> {
    Input {
        raw,
        adj,
        scene,
        prior: None,
        request: RenderRequest::default(),
    }
}

fn run(raw: &RawImage, adj: &Adjustments, scene: &SceneSignals) -> OptimizeReport {
    analyze_cpu(&input(raw, adj, scene)).unwrap()
}

fn change(r: &OptimizeReport, f: Field) -> Option<&Change> {
    r.changes.iter().find(|c| c.field == f)
}

fn num(s: &Setting) -> f32 {
    match *s {
        Setting::Number { value } => value,
        _ => panic!("not a number: {s:?}"),
    }
}

/// Apply every default-on change, then analyze again with the report as `prior`.
fn rerun(
    raw: &RawImage,
    adj: &Adjustments,
    sc: &SceneSignals,
    r: &OptimizeReport,
) -> (Adjustments, OptimizeReport) {
    let ids: Vec<u32> = r
        .changes
        .iter()
        .filter(|c| c.default_on)
        .map(|c| c.id)
        .collect();
    let applied = apply(adj, r, &ids);
    let prior = mark_applied(r, &ids);
    let again = analyze_cpu(&Input {
        prior: Some(&prior),
        ..input(raw, &applied, sc)
    })
    .unwrap();
    (applied, again)
}

// ------------------------------------------------------------------ end to end ---

#[test]
fn underexposed_scene_is_brightened_with_the_reason() {
    let raw = varied(0.03);
    let r = run(&raw, &Adjustments::default(), &SceneSignals::default());
    let c = change(&r, Field::Exposure).expect("exposure change");
    assert!(num(&c.new) > 1.0, "{c:?}");
    let Reason::Exposure {
        midtone, target, ..
    } = c.reason
    else {
        panic!("{:?}", c.reason)
    };
    assert!(
        midtone < target * 0.5 && target == rules::MIDTONE,
        "{midtone} {target}"
    );
    assert!(c.explanation.starts_with("Exposure +"), "{}", c.explanation);
    assert!(c.explanation.contains("dark") && c.explanation.contains("18%"));
    assert!(c.confidence > 0.6 && c.default_on);
    assert!(r.summary.contains("brightens the image"), "{}", r.summary);
    assert!(r.elapsed_ms > 0.0);
}

#[test]
fn overexposed_scene_is_darkened() {
    let raw = varied(0.5);
    let r = run(&raw, &Adjustments::default(), &SceneSignals::default());
    let c = change(&r, Field::Exposure).expect("exposure change");
    assert!(num(&c.new) < -0.3, "{c:?}");
    assert!(c.explanation.contains("bright"), "{}", c.explanation);
    assert!(r.summary.contains("darkens"), "{}", r.summary);
}

/// Top 40% is a blown sky (scene values above the clip), the rest a mid-gray land.
fn clipped_sky() -> (RawImage, SceneSignals) {
    let raw = scene(240, 160, 0.1, |_, v| {
        if v < 0.4 {
            [1.2, 1.2, 1.3]
        } else {
            [0.03, 0.028, 0.025]
        }
    });
    let (mw, mh) = (24u32, 16u32);
    let data = (0..mw * mh)
        .map(|i| {
            if (i / mw) as f32 / (mh as f32) < 0.4 {
                255
            } else {
                0
            }
        })
        .collect();
    let sc = SceneSignals {
        sky_fraction: 0.4,
        sky_mask: Some(SkyMask {
            width: mw,
            height: mh,
            data,
        }),
        tags: vec![SceneTag::Landscape],
        ..SceneSignals::default()
    };
    (raw, sc)
}

#[test]
fn clipped_sky_gets_highlight_recovery_and_a_sky_layer_suggestion() {
    // A dark land the user brightened under a sky at the sensor's clip.
    let (raw, sc) = clipped_sky();
    let mut user = Adjustments::default();
    user.exposure.exposure_ev = 2.0;
    let r = run(&raw, &user, &sc);
    let c = change(&r, Field::Highlights).expect("highlights");
    let Reason::HighlightsClipped {
        sky_clipped_pct: Some(sky),
        ..
    } = c.reason
    else {
        panic!("{:?}", c.reason)
    };
    assert!(sky > 1.0 && num(&c.new) >= 15.0, "{sky} {c:?}");
    assert!(
        c.explanation.contains("of the sky clips"),
        "{}",
        c.explanation
    );
    assert!(
        r.summary.contains("recovers the clipped sky"),
        "{}",
        r.summary
    );
    let s = change(&r, Field::SkyLayer).expect("sky layer");
    assert!(!s.default_on && s.explanation.contains("starts unchecked"));
    // Applying it adds one AI-sky layer; a second run doesn't suggest another.
    let with = apply(&user, &r, &[s.id]);
    assert_eq!(with.layers.len(), 1);
    assert!(matches!(
        with.layers[0].mask.shapes[0],
        MaskShape::Ai {
            target: AiTarget::Sky,
            ..
        }
    ));
    let again = run(&raw, &with, &sc);
    assert!(change(&again, Field::SkyLayer).is_none());
    // Measurements reflect the mask: most of the sky is clipped.
    assert!(r.measurements.current.sky_clipped.unwrap() > 0.5);
}

/// `shot` from auto.rs: reflectances lit by `light_k`, camera WB at `as_shot_k`.
fn shot(light_k: f32, as_shot_k: f32, f: impl Fn(f32, f32) -> [f32; 3]) -> RawImage {
    let mut raw = crate::testutil::gray_raw(240, 160, 0.0);
    let cam = CameraColor::of(&raw);
    let m = temp_tint_to_multipliers(light_k, 0.0, &cam);
    raw.wb_as_shot = temp_tint_to_multipliers(as_shot_k, 0.0, &cam);
    let (black, white) = (raw.black_level[0], raw.white_level);
    for y in 0..160u32 {
        for x in 0..240u32 {
            let r = f(x as f32 / 240.0, y as f32 / 160.0);
            let c = ds_core::CfaPattern::Rggb.color_at(x, y).unwrap() as usize;
            let v = (r[c] / m[c] * 0.5 * (1.0 + 0.01 * noise(x, y))).clamp(0.0, 1.0);
            raw.data[(y * 240 + x) as usize] = (black + v * (white - black)) as u16;
        }
    }
    raw
}

fn gray_card(u: f32, v: f32) -> [f32; 3] {
    if (u - 0.5).abs() < 0.25 && (v - 0.5).abs() < 0.3 {
        [0.45; 3]
    } else {
        [0.45, 0.28, 0.14]
    }
}

#[test]
fn colour_cast_is_corrected_from_a_neutral() {
    let raw = shot(2850.0, 5500.0, gray_card);
    let r = run(&raw, &Adjustments::default(), &SceneSignals::default());
    let c = change(&r, Field::WhiteBalance).expect("wb");
    let Setting::WhiteBalance {
        temp_k, as_shot, ..
    } = c.new
    else {
        panic!()
    };
    assert!(!as_shot && (temp_k - 2850.0).abs() < 400.0, "{temp_k}");
    let Reason::ColorCast {
        evidence,
        current_k,
        ..
    } = c.reason
    else {
        panic!()
    };
    assert_eq!(evidence, Evidence::WhitePatch);
    assert!((current_k - 5500.0).abs() < 100.0);
    assert!(c.from.starts_with("As shot (5,"), "{}", c.from);
    assert!(
        c.explanation.contains("warm (orange) cast"),
        "{}",
        c.explanation
    );
    assert!(c.confidence >= 0.8);
    // A sunset keeps its warmth when the evidence is only gray world.
    let sunset = shot(5500.0, 5500.0, |_, v| {
        [1.2 - 0.3 * v, 0.55 - 0.2 * v, 0.25 - 0.1 * v].map(|x| x * 0.7)
    });
    let sc = SceneSignals {
        tags: vec![SceneTag::Sunset],
        ..SceneSignals::default()
    };
    let r = run(&sunset, &Adjustments::default(), &sc);
    assert!(change(&r, Field::WhiteBalance).is_none());
}

#[test]
fn tilted_horizon_is_straightened_and_the_crop_fits() {
    let raw = varied(0.15);
    let sc = SceneSignals {
        horizon_deg: Some(1.8),
        ..SceneSignals::default()
    };
    let r = run(&raw, &Adjustments::default(), &sc);
    let c = change(&r, Field::Straighten).expect("straighten");
    assert_eq!(
        c.new,
        Setting::Straighten {
            angle_deg: -1.8,
            crop: None
        }
    );
    assert_eq!(
        apply(&Adjustments::default(), &r, &[c.id]).crop.rect,
        NormRect::FULL,
        "the pipeline fits the frame"
    );
    assert_eq!(c.to, "\u{2212}1.8°");
    assert!(
        c.explanation.contains("tilted 1.8° (rising to the right)"),
        "{}",
        c.explanation
    );
    // Level enough, or too steep to be a horizon: left alone.
    for h in [0.3, 12.0, f32::NAN] {
        let sc = SceneSignals {
            horizon_deg: Some(h),
            ..SceneSignals::default()
        };
        assert!(change(&run(&raw, &Adjustments::default(), &sc), Field::Straighten).is_none());
    }
    // A user straighten stays.
    let mut adj = Adjustments::default();
    adj.crop.angle_deg = 3.0;
    let r = run(&raw, &adj, &sc_h(1.8));
    assert!(change(&r, Field::Straighten).is_none());
    assert!(r.kept.iter().any(|k| k.field == Field::Straighten));
}

fn sc_h(h: f32) -> SceneSignals {
    SceneSignals {
        horizon_deg: Some(h),
        ..SceneSignals::default()
    }
}

#[test]
fn flat_hazy_scene_gets_dehaze_not_contrast() {
    // Lifted blacks, bright veil, low contrast and colour.
    let raw = scene(256, 192, 0.12, |u, v| {
        let base = 0.16 + 0.05 * (u * 7.0).sin() + 0.03 * v;
        [base * 1.02, base, base * 1.05]
    });
    let r = run(&raw, &Adjustments::default(), &SceneSignals::default());
    let b = r.measurements.baseline;
    assert!(b.haze > rules::HAZE_MIN, "{b:?}");
    let d = change(&r, Field::Dehaze).expect("dehaze");
    assert!(num(&d.new) >= 5.0);
    assert!(d.explanation.contains("haze score"), "{}", d.explanation);
    assert!(change(&r, Field::Contrast).is_none());
    assert!(change(&r, Field::Levels).is_some());
    // On snow the bright veil is the scene: no dehaze.
    let sc = SceneSignals {
        tags: vec![SceneTag::Snow],
        ..SceneSignals::default()
    };
    assert!(change(&run(&raw, &Adjustments::default(), &sc), Field::Dehaze).is_none());
}

#[test]
fn high_iso_suggests_ai_denoise_or_noise_reduction() {
    let raw = varied(0.15);
    let adj = Adjustments::default();
    let mut sc = SceneSignals {
        ai_denoise_available: true,
        ..SceneSignals::default()
    };
    let mut inp = input(&raw, &adj, &sc);
    inp.request.capture.iso = Some(6400);
    let r = analyze_cpu(&inp).unwrap();
    let c = change(&r, Field::AiDenoise).expect("ai denoise");
    assert_eq!(c.new, Setting::Toggle { on: true });
    assert!(c.explanation.contains("ISO 6400"), "{}", c.explanation);
    assert!(
        change(&r, Field::Clarity).is_none(),
        "no clarity on noisy images"
    );
    // Without the model: stronger classic noise reduction.
    sc.ai_denoise_available = false;
    let mut inp = input(&raw, &adj, &sc);
    inp.request.capture.iso = Some(6400);
    let r = analyze_cpu(&inp).unwrap();
    let c = change(&r, Field::NoiseReduction).expect("nr");
    assert_eq!(
        c.new,
        Setting::NoiseReduction {
            luminance: 45.0,
            color: 40.0
        }
    );
    assert!(
        c.explanation.contains("isn't available"),
        "{}",
        c.explanation
    );
    assert_eq!(c.from, "luminance 25, color 25");
    // Already on: nothing more.
    let mut on = Adjustments::default();
    on.noise.ai_denoise = true;
    let sc = SceneSignals {
        ai_denoise_available: true,
        ..SceneSignals::default()
    };
    let mut inp = input(&raw, &on, &sc);
    inp.request.capture.iso = Some(6400);
    assert!(change(&analyze_cpu(&inp).unwrap(), Field::AiDenoise).is_none());
    // Low ISO, clean: no noise proposal.
    let r = run(&raw, &adj, &SceneSignals::default());
    assert!(change(&r, Field::NoiseReduction).is_none() && change(&r, Field::AiDenoise).is_none());
}

#[test]
fn user_adjusted_fields_are_kept_and_said_so() {
    let raw = varied(0.03);
    let mut adj = Adjustments::default();
    adj.exposure.exposure_ev = 0.3;
    adj.presence.vibrance = -20.0;
    let r = run(&raw, &adj, &SceneSignals::default());
    assert!(change(&r, Field::Exposure).is_none());
    let k = r.kept.iter().find(|k| k.field == Field::Exposure).unwrap();
    assert_eq!(k.why, KeptWhy::UserSet { clipped_pct: None });
    assert_eq!(k.explanation, "Exposure stays at +0.30 EV: you set it.");
    assert!(
        r.summary
            .contains("left exposure and vibrance alone because you set them"),
        "{}",
        r.summary
    );
}

#[test]
fn a_clipping_user_exposure_is_pulled_back_only_when_highlights_are_set_too() {
    let raw = varied(0.3);
    let mut adj = Adjustments::default();
    adj.exposure.exposure_ev = 2.5;
    let r = run(&raw, &adj, &SceneSignals::default());
    assert!(change(&r, Field::Exposure).is_none());
    let k = r.kept.iter().find(|k| k.field == Field::Exposure).unwrap();
    assert!(
        matches!(k.why, KeptWhy::UserSet { clipped_pct: Some(c) } if c > 1.0),
        "{k:?}"
    );
    assert!(k.explanation.contains("clips to white"));
    adj.hdr.highlights = 10.0;
    let r = run(&raw, &adj, &SceneSignals::default());
    let c = change(&r, Field::Exposure).expect("pulled back");
    assert!(num(&c.new) < 2.5);
    assert!(matches!(
        c.reason,
        Reason::Exposure {
            clipped_pct: Some(_),
            limited: true,
            ..
        }
    ));
    assert!(
        c.explanation.contains("you set +2.50 EV"),
        "{}",
        c.explanation
    );
    assert!(r.kept.iter().any(|k| k.field == Field::Highlights
        && matches!(
            k.why,
            KeptWhy::UserSet {
                clipped_pct: Some(_)
            }
        )));
}

#[test]
fn optimize_twice_proposes_nothing_new() {
    let (sky, sky_sc) = clipped_sky();
    let cast = shot(2850.0, 5500.0, gray_card);
    let flat = scene(256, 192, 0.12, |u, _| [0.16 + 0.05 * (u * 7.0).sin(); 3]);
    for (raw, sc) in [
        (varied(0.03), SceneSignals::default()),
        (varied(0.5), sc_h(2.5)),
        (sky, sky_sc),
        (cast, SceneSignals::default()),
        (flat, SceneSignals::default()),
    ] {
        let adj = Adjustments::default();
        let r = run(&raw, &adj, &sc);
        assert_ne!(r.changes.len(), 0);
        let (applied, again) = rerun(&raw, &adj, &sc, &r);
        let left: Vec<_> = again.changes.iter().filter(|c| c.default_on).collect();
        assert!(
            left.is_empty(),
            "second run: {left:#?}\nfirst: {:#?}",
            r.changes
        );
        // Nothing the optimizer set reads as the user's.
        assert!(
            again
                .kept
                .iter()
                .all(|k| !matches!(k.why, KeptWhy::UserSet { .. })),
            "{:?}",
            again.kept
        );
        assert_ne!(applied, adj);
    }
}

#[test]
fn muted_colours_get_vibrance_moderated_for_faces() {
    let raw = scene(256, 192, 0.1, |u, v| {
        let l = 0.12 + 0.08 * u;
        [l * (1.0 + 0.1 * v), l, l * (1.0 - 0.1 * v)]
    });
    let r = run(&raw, &Adjustments::default(), &SceneSignals::default());
    let c = change(&r, Field::Vibrance).expect("vibrance");
    assert!(num(&c.new) > 15.0, "{c:?}");
    let faces = SceneSignals {
        faces: 2,
        face_area: 0.1,
        ..SceneSignals::default()
    };
    let r = run(&raw, &Adjustments::default(), &faces);
    let c = change(&r, Field::Vibrance).expect("vibrance");
    assert!(num(&c.new) <= 15.0);
    assert!(
        c.explanation.contains("skin stays natural"),
        "{}",
        c.explanation
    );
    assert!(
        change(&r, Field::Clarity).is_none(),
        "portraits get no clarity"
    );
}

#[test]
fn soft_landscape_gets_clarity_and_texture() {
    let raw = scene(256, 192, 0.0, |u, v| {
        let l = 0.05 + 0.25 * u + 0.1 * v;
        [l * 1.4, l, l * 0.6]
    });
    let sc = SceneSignals {
        tags: vec![SceneTag::Landscape],
        ..SceneSignals::default()
    };
    let r = run(&raw, &Adjustments::default(), &sc);
    let c = change(&r, Field::Clarity).expect("clarity");
    assert!((5.0..=15.0).contains(&num(&c.new)));
    let t = change(&r, Field::Texture).expect("texture");
    assert_eq!(num(&t.new), 10.0);
    assert!(r.summary.contains("adds clarity") && r.summary.contains("fine texture"));
}

#[test]
fn night_scenes_stay_dark() {
    let raw = varied(0.03);
    let night = SceneSignals {
        tags: vec![SceneTag::Night],
        ..SceneSignals::default()
    };
    let day = run(&raw, &Adjustments::default(), &SceneSignals::default());
    let r = run(&raw, &Adjustments::default(), &night);
    let (d, n) = (
        num(&change(&day, Field::Exposure).unwrap().new),
        num(&change(&r, Field::Exposure).unwrap().new),
    );
    assert!((d - n - 1.0).abs() < 0.05, "night one stop lower: {d} {n}");
    let c = change(&r, Field::Exposure).unwrap();
    assert!(
        c.explanation.contains("for a night scene"),
        "{}",
        c.explanation
    );
}

#[test]
fn lens_corrections_turned_off_are_suggested_unchecked() {
    let raw = varied(0.15);
    let mut adj = Adjustments::default();
    adj.lens.enabled = false;
    let sc = SceneSignals::default();
    let mut inp = input(&raw, &adj, &sc);
    inp.request.capture.lens = Some("FE 24-70mm F2.8 GM".into());
    let r = analyze_cpu(&inp).unwrap();
    let c = change(&r, Field::LensCorrection).expect("lens");
    assert!(!c.default_on);
    assert!(
        c.explanation
            .ends_with("You turned them off, so this is unchecked."),
        "{}",
        c.explanation
    );
    assert!(apply(&adj, &r, &[c.id]).lens.enabled);
    // Without a profile, or with corrections on: nothing.
    assert!(change(&run(&raw, &adj, &sc), Field::LensCorrection).is_none());
}

#[test]
fn backlit_subject_gets_its_shadows_lifted() {
    // Bright surround, a dark subject in the middle.
    let raw = scene(240, 160, 0.1, |u, v| {
        if (u - 0.5).abs() < 0.15 && (v - 0.5).abs() < 0.25 {
            [0.012; 3]
        } else {
            [0.5; 3]
        }
    });
    let sc = SceneSignals {
        subject: Some(NormRect {
            x: 0.36,
            y: 0.26,
            w: 0.28,
            h: 0.48,
        }),
        ..SceneSignals::default()
    };
    let r = run(&raw, &Adjustments::default(), &sc);
    let c = change(&r, Field::Shadows).expect("shadows");
    assert!(num(&c.new) >= 30.0);
    assert!(matches!(
        c.reason,
        Reason::Shadows {
            backlit: true,
            subject_luma: Some(_),
            ..
        }
    ));
    assert!(c.explanation.contains("backlit"), "{}", c.explanation);
}

#[test]
fn invalid_input_is_an_error() {
    let mut raw = varied(0.1);
    raw.white_level = 0.0;
    assert!(
        analyze_cpu(&input(
            &raw,
            &Adjustments::default(),
            &SceneSignals::default()
        ))
        .is_err()
    );
}

// ------------------------------------------------------------------ rules ---

fn ctx<'a>(adj: &'a Adjustments, m: &'a Measurements, sc: &'a SceneSignals) -> Ctx<'a> {
    Ctx {
        adj,
        prior: None,
        scene: sc,
        m,
        gray_world_weight: 1.0,
        tone: None,
    }
}

#[test]
fn ownership_reads_the_prior_report() {
    let m = Measurements::default();
    let sc = SceneSignals::default();
    let mut adj = Adjustments::default();
    assert_eq!(ctx(&adj, &m, &sc).owner(Field::Vibrance), Owner::Default);
    adj.presence.vibrance = 12.0;
    assert_eq!(ctx(&adj, &m, &sc).owner(Field::Vibrance), Owner::User);
    let mut prior = OptimizeReport::default();
    prior.changes.push(Change {
        id: 0,
        field: Field::Vibrance,
        label: String::new(),
        old: Setting::Number { value: 0.0 },
        new: Setting::Number { value: 12.0 },
        from: String::new(),
        to: String::new(),
        reason: Reason::Haze {
            haze: 0.0,
            darkest: 0.0,
            spread: 0.0,
        },
        confidence: 0.5,
        default_on: true,
        applied: false,
        explanation: String::new(),
    });
    let mut c = ctx(&adj, &m, &sc);
    c.prior = Some(&prior);
    assert_eq!(
        c.owner(Field::Vibrance),
        Owner::User,
        "not applied: still the user's"
    );
    prior.changes[0].applied = true;
    let mut c = ctx(&adj, &m, &sc);
    c.prior = Some(&prior);
    assert_eq!(c.owner(Field::Vibrance), Owner::Optimizer);
    adj.presence.vibrance = 13.0;
    let mut c = ctx(&adj, &m, &sc);
    c.prior = Some(&prior);
    assert_eq!(c.owner(Field::Vibrance), Owner::User, "moved since");
}

#[test]
fn slider_rules_respect_the_dead_band_and_return_to_default() {
    let mut m = Measurements::default();
    m.baseline.mean_saturation = 0.19; // just muted → a small boost
    let sc = SceneSignals::default();
    let adj = Adjustments::default();
    assert!(matches!(
        rules::vibrance(&ctx(&adj, &m, &sc)),
        Outcome::Change(_)
    ));
    m.baseline.mean_saturation = 0.25;
    assert_eq!(rules::vibrance(&ctx(&adj, &m, &sc)), Outcome::Nothing);
    // An optimizer value whose reason is gone goes back to 0.
    let mut a = Adjustments::default();
    a.presence.vibrance = 15.0;
    let mut prior = OptimizeReport::default();
    prior.changes.push(Change {
        id: 0,
        field: Field::Vibrance,
        label: String::new(),
        old: Setting::Number { value: 0.0 },
        new: Setting::Number { value: 15.0 },
        from: String::new(),
        to: String::new(),
        reason: Reason::Haze {
            haze: 0.0,
            darkest: 0.0,
            spread: 0.0,
        },
        confidence: 0.5,
        default_on: true,
        applied: true,
        explanation: String::new(),
    });
    let mut c = ctx(&a, &m, &sc);
    c.prior = Some(&prior);
    let Outcome::Change(p) = rules::vibrance(&c) else {
        panic!()
    };
    assert_eq!(p.new, Setting::Number { value: 0.0 });
}

#[test]
fn oversaturated_images_lose_some_saturation() {
    let mut m = Measurements::default();
    m.baseline.mean_saturation = 0.6;
    m.baseline.p90_saturation = 0.98;
    let (adj, sc) = (Adjustments::default(), SceneSignals::default());
    let Outcome::Change(p) = rules::saturation(&ctx(&adj, &m, &sc)) else {
        panic!()
    };
    assert_eq!(p.new, Setting::Number { value: -10.0 });
    let c = rules_change(&adj, &m, p);
    assert!(
        c.explanation.contains("98% saturation"),
        "{}",
        c.explanation
    );
    assert_eq!(c.from, "0");
    assert_eq!(c.to, "\u{2212}10");
}

fn rules_change(adj: &Adjustments, m: &Measurements, p: rules::Proposal) -> Change {
    let sc = SceneSignals::default();
    let mut o = rules::Outcomes::default();
    o.push(Outcome::Change(p));
    o.finish(&ctx(adj, m, &sc)).0.remove(0)
}

#[test]
fn contrast_is_added_to_flat_images_but_gently_for_portraits() {
    let mut m = Measurements::default();
    m.levels = [0.0, 1.0];
    m.baseline.luma_std = 0.1;
    let adj = Adjustments::default();
    let sc = SceneSignals::default();
    let Outcome::Change(p) = rules::contrast(&ctx(&adj, &m, &sc)) else {
        panic!()
    };
    assert_eq!(p.new, Setting::Number { value: 25.0 });
    let portrait = SceneSignals {
        tags: vec![SceneTag::Portrait],
        ..SceneSignals::default()
    };
    let Outcome::Change(p) = rules::contrast(&ctx(&adj, &m, &portrait)) else {
        panic!()
    };
    assert_eq!(p.new, Setting::Number { value: 10.0 });
    let mut hazy = m.clone();
    (hazy.baseline.haze, hazy.baseline.p001) = (0.5, 0.1);
    assert_eq!(
        rules::contrast(&ctx(&adj, &hazy, &sc)),
        Outcome::Nothing,
        "dehaze handles haze"
    );
    let night = SceneSignals {
        tags: vec![SceneTag::Night],
        ..SceneSignals::default()
    };
    assert_eq!(rules::contrast(&ctx(&adj, &m, &night)), Outcome::Nothing);
}

#[test]
fn noise_reduction_follows_sigma_without_iso() {
    let m = Measurements {
        noise_sigma: rules::NOISY_SIGMA * 2.0,
        ..Measurements::default()
    };
    let (adj, sc) = (Adjustments::default(), SceneSignals::default());
    let out = rules::noise(&ctx(&adj, &m, &sc));
    let [Outcome::Change(p)] = out.as_slice() else {
        panic!("{out:?}")
    };
    assert_eq!(
        p.new,
        Setting::NoiseReduction {
            luminance: 45.0,
            color: 40.0
        }
    );
    let mut user = Adjustments::default();
    user.noise.luminance = 10.0;
    assert!(matches!(
        rules::noise(&ctx(&user, &m, &sc)).as_slice(),
        [Outcome::Kept(..)]
    ));
}

#[test]
fn white_balance_without_evidence_or_within_the_dead_band_stays() {
    let mut m = Measurements {
        as_shot_wb: [5200.0, 2.0],
        neutral_wb: [5300.0, 3.0],
        wb_evidence: Some(Evidence::NeutralCluster),
        ..Measurements::default()
    };
    let (adj, sc) = (Adjustments::default(), SceneSignals::default());
    let Outcome::Kept(_, why, _) = rules::white_balance(&ctx(&adj, &m, &sc)) else {
        panic!()
    };
    assert_eq!(why, KeptWhy::OnTarget);
    m.wb_evidence = None;
    assert_eq!(rules::white_balance(&ctx(&adj, &m, &sc)), Outcome::Nothing);
    let mut user = Adjustments::default();
    user.white_balance = WhiteBalance::Custom {
        temp_k: 3000.0,
        tint: 0.0,
    };
    assert!(matches!(
        rules::white_balance(&ctx(&user, &m, &sc)),
        Outcome::Kept(_, KeptWhy::UserSet { .. }, _)
    ));
    // A tint-only cast reads as green or magenta.
    m.wb_evidence = Some(Evidence::GrayWorld);
    m.neutral_wb = [5200.0, 12.0];
    let Outcome::Change(p) = rules::white_balance(&ctx(&adj, &m, &sc)) else {
        panic!()
    };
    let c = rules_change(&adj, &m, p);
    assert!(
        c.explanation.contains("a green cast") && c.explanation.contains("capped at 600 K"),
        "{}",
        c.explanation
    );
    m.neutral_wb = [5200.0, -12.0];
    let Outcome::Change(p) = rules::white_balance(&ctx(&adj, &m, &sc)) else {
        panic!()
    };
    assert!(
        rules_change(&adj, &m, p)
            .explanation
            .contains("a magenta cast")
    );
    m.neutral_wb = [6200.0, 2.0];
    m.wb_evidence = Some(Evidence::NeutralCluster);
    let Outcome::Change(p) = rules::white_balance(&ctx(&adj, &m, &sc)) else {
        panic!()
    };
    let c = rules_change(&adj, &m, p);
    assert!(
        c.explanation.contains("several neutral surfaces") && c.explanation.contains("cool (blue)"),
        "{}",
        c.explanation
    );
}

#[test]
fn optimizer_values_on_target_are_silent() {
    let m = Measurements {
        as_shot_wb: [5200.0, 2.0],
        neutral_wb: [5300.0, 3.0],
        wb_evidence: Some(Evidence::WhitePatch),
        ..Measurements::default()
    };
    let mut adj = Adjustments::default();
    adj.white_balance = WhiteBalance::Custom {
        temp_k: 5300.0,
        tint: 3.0,
    };
    let mut prior = OptimizeReport::default();
    prior.changes.push(Change {
        id: 0,
        field: Field::WhiteBalance,
        label: String::new(),
        old: Setting::Toggle { on: false },
        new: Setting::WhiteBalance {
            temp_k: 5300.0,
            tint: 3.0,
            as_shot: false,
        },
        from: String::new(),
        to: String::new(),
        reason: Reason::Tilted { horizon_deg: 0.0 },
        confidence: 0.5,
        default_on: true,
        applied: true,
        explanation: String::new(),
    });
    let sc = SceneSignals::default();
    let mut c = ctx(&adj, &m, &sc);
    c.prior = Some(&prior);
    assert_eq!(rules::white_balance(&c), Outcome::Nothing);
}

// ------------------------------------------------------------------ explain, apply ---

#[test]
fn number_formatting() {
    use super::explain::{kelvin, pct, percent, signed};
    assert_eq!(signed(0.45, 2), "+0.45");
    assert_eq!(signed(-35.0, 0), "\u{2212}35");
    assert_eq!(signed(-0.001, 2), "0.00");
    assert_eq!(signed(0.0, 0), "0");
    assert_eq!(pct(0.024), "2.4%");
    assert_eq!(pct(0.18), "18%");
    assert_eq!(percent(9.96), "10%");
    assert_eq!(kelvin(5600.0), "5,600 K");
    assert_eq!(kelvin(12_345.0), "12,350 K");
    assert_eq!(kelvin(950.0), "950 K");
}

#[test]
fn values_read_like_the_panel() {
    use super::explain::value;
    let v = |f, s| value(f, &s);
    assert_eq!(
        v(Field::Exposure, Setting::Number { value: 0.45 }),
        "+0.45 EV"
    );
    assert_eq!(
        v(
            Field::Levels,
            Setting::Levels {
                black: 0.03,
                white: 0.94
            }
        ),
        "black 3.0%, white 94%"
    );
    assert_eq!(v(Field::AiDenoise, Setting::Toggle { on: false }), "Off");
    assert_eq!(
        v(
            Field::SkyLayer,
            Setting::SkyLayer {
                present: true,
                exposure_ev: -0.5,
                highlights: 30.0
            }
        ),
        "\u{2212}0.5 EV, highlights +30"
    );
    assert_eq!(
        v(
            Field::SkyLayer,
            Setting::SkyLayer {
                present: false,
                exposure_ev: 0.0,
                highlights: 0.0
            }
        ),
        "None"
    );
    assert_eq!(
        v(
            Field::WhiteBalance,
            Setting::WhiteBalance {
                temp_k: 4150.0,
                tint: 6.0,
                as_shot: false
            }
        ),
        "4,150 K / +6"
    );
}

#[test]
fn explanations_cover_every_reason() {
    use super::explain::change;
    let n = |value| Setting::Number { value };
    let cases: Vec<(Field, Reason, &str)> = vec![
        (
            Field::Highlights,
            Reason::HighlightsClipped {
                clipped_pct: 2.4,
                sky_clipped_pct: None,
            },
            "2.4% of the image clips",
        ),
        (
            Field::Highlights,
            Reason::HighlightRange { stops_over: 0.62 },
            "0.6 stops",
        ),
        (
            Field::Shadows,
            Reason::Shadows {
                dark_pct: 4.0,
                stops_under: 1.0,
                backlit: false,
                subject_luma: None,
                median_luma: 0.4,
            },
            "4.0% of the image is close to black",
        ),
        (
            Field::Shadows,
            Reason::Shadows {
                dark_pct: 4.0,
                stops_under: 1.0,
                backlit: true,
                subject_luma: None,
                median_luma: 0.4,
            },
            "the scene is backlit",
        ),
        (
            Field::Levels,
            Reason::TonalRange {
                darkest: 0.03,
                brightest: 0.94,
            },
            "stopped at 3.0% and the brightest at 94%",
        ),
        (
            Field::Contrast,
            Reason::LowContrast {
                spread: 0.12,
                target: 0.19,
            },
            "tonal spread 12% vs about 19%",
        ),
        (
            Field::Clarity,
            Reason::SoftDetail {
                detail: 0.008,
                target: 0.016,
                landscape: false,
            },
            "local contrast 0.8%",
        ),
        (
            Field::NoiseReduction,
            Reason::Noise {
                iso: None,
                sigma: 0.012,
                ai_available: true,
            },
            "High noise with measured sensor noise 1.2%; this cleans it up.",
        ),
        (
            Field::Straighten,
            Reason::Tilted { horizon_deg: -2.0 },
            "falling to the right",
        ),
        (
            Field::Exposure,
            Reason::Exposure {
                midtone: 0.11,
                target: 0.18,
                limited: true,
                scene: None,
                clipped_pct: None,
            },
            "held back so the brightest highlights don't clip",
        ),
        (
            Field::Exposure,
            Reason::Exposure {
                midtone: 0.04,
                target: 0.18,
                limited: false,
                scene: Some(SceneTag::Snow),
                clipped_pct: None,
            },
            "stops dark (midtones at 4.0%",
        ),
        (
            Field::Shadows,
            Reason::Shadows {
                dark_pct: 4.0,
                stops_under: 1.0,
                backlit: true,
                subject_luma: Some(0.1),
                median_luma: 0.4,
            },
            "subject at 10% brightness vs 40%",
        ),
    ];
    for (f, r, want) in cases {
        let e = change(f, &n(0.0), &n(10.0), &r, true);
        assert!(e.contains(want), "{e}");
        assert!(e.starts_with(f.label()) && e.ends_with('.'));
    }
    // The lead's example, exactly.
    let e = change(
        Field::Exposure,
        &n(0.0),
        &n(0.45),
        &Reason::Exposure {
            midtone: 0.11,
            target: 0.18,
            limited: false,
            scene: None,
            clipped_pct: None,
        },
        true,
    );
    assert_eq!(
        e,
        "Exposure +0.45 EV \u{2014} the image was about half a stop dark (midtones at 11% vs the 18% target)."
    );
}

#[test]
fn kept_texts() {
    use super::explain::kept;
    assert_eq!(
        kept(
            Field::WhiteBalance,
            "As shot (5,200 K / +2)",
            KeptWhy::OnTarget,
            None
        ),
        "White balance already looks right (As shot (5,200 K / +2))."
    );
    assert!(
        kept(
            Field::WhiteBalance,
            "x",
            KeptWhy::SceneMood {
                tag: SceneTag::Sunset
            },
            None
        )
        .contains("sunset's own color")
    );
}

#[test]
fn apply_sets_exactly_the_selected_fields() {
    let mut r = OptimizeReport::default();
    let mk = |id, field, new| Change {
        id,
        field,
        label: String::new(),
        old: Setting::Toggle { on: false },
        new,
        from: String::new(),
        to: String::new(),
        reason: Reason::Tilted { horizon_deg: 0.0 },
        confidence: 0.5,
        default_on: true,
        applied: false,
        explanation: String::new(),
    };
    let n = |value| Setting::Number { value };
    r.changes = vec![
        mk(0, Field::Exposure, n(0.5)),
        mk(1, Field::Highlights, n(30.0)),
        mk(2, Field::Shadows, n(20.0)),
        mk(
            3,
            Field::Levels,
            Setting::Levels {
                black: 0.03,
                white: 0.95,
            },
        ),
        mk(4, Field::Contrast, n(10.0)),
        mk(5, Field::Vibrance, n(15.0)),
        mk(6, Field::Saturation, n(-10.0)),
        mk(7, Field::Clarity, n(12.0)),
        mk(8, Field::Texture, n(10.0)),
        mk(9, Field::Dehaze, n(20.0)),
        mk(10, Field::AiDenoise, Setting::Toggle { on: true }),
        mk(
            11,
            Field::NoiseReduction,
            Setting::NoiseReduction {
                luminance: 45.0,
                color: 40.0,
            },
        ),
        mk(
            12,
            Field::Straighten,
            Setting::Straighten {
                angle_deg: -1.8,
                crop: None,
            },
        ),
        mk(
            13,
            Field::WhiteBalance,
            Setting::WhiteBalance {
                temp_k: 4000.0,
                tint: 5.0,
                as_shot: false,
            },
        ),
        mk(14, Field::Exposure, Setting::Toggle { on: true }), // mismatched kind: ignored
    ];
    let base = Adjustments::default();
    let all: Vec<u32> = (0..15).collect();
    let a = apply(&base, &r, &all);
    assert_eq!(a.exposure.exposure_ev, 0.5);
    assert_eq!((a.hdr.highlights, a.hdr.shadows), (30.0, 20.0));
    assert_eq!((a.levels.in_black, a.levels.in_white), (0.03, 0.95));
    assert_eq!((a.exposure.contrast, a.exposure.saturation), (10.0, -10.0));
    assert_eq!(
        (a.presence.vibrance, a.presence.texture, a.presence.dehaze),
        (15.0, 10.0, 20.0)
    );
    assert_eq!(a.detail.clarity, 12.0);
    assert!(a.noise.ai_denoise);
    assert_eq!((a.noise.luminance, a.noise.color), (45.0, 40.0));
    assert_eq!(a.crop.angle_deg, -1.8);
    assert_eq!(
        a.white_balance,
        WhiteBalance::Custom {
            temp_k: 4000.0,
            tint: 5.0
        }
    );
    let some = apply(&base, &r, &[1, 5]);
    assert_eq!(some.exposure, base.exposure);
    assert_eq!((some.hdr.highlights, some.presence.vibrance), (30.0, 15.0));
    let marked = mark_applied(&r, &[1, 5]);
    assert!(
        marked
            .changes
            .iter()
            .all(|c| c.applied == [1, 5].contains(&c.id))
    );
    assert_eq!(
        marked.owner_of(Field::Highlights),
        Some((n(30.0), SetBy::Optimizer))
    );
    assert_eq!(marked.owner_of(Field::Exposure), None);
    // A sky layer can be removed again.
    let mut with = base.clone();
    set(
        &mut with,
        Field::SkyLayer,
        &Setting::SkyLayer {
            present: true,
            exposure_ev: -0.5,
            highlights: 30.0,
        },
    );
    assert_eq!(with.layers.len(), 1);
    set(
        &mut with,
        Field::SkyLayer,
        &Setting::SkyLayer {
            present: false,
            exposure_ev: 0.0,
            highlights: 0.0,
        },
    );
    assert_eq!(with.layers.len(), 0);
}

#[test]
fn settings_compare_within_rounding() {
    let wb = |k, t, a| Setting::WhiteBalance {
        temp_k: k,
        tint: t,
        as_shot: a,
    };
    assert!(
        wb(5000.0, 1.0, true).approx_eq(&wb(1.0, 2.0, true)),
        "as shot ignores the values"
    );
    assert!(wb(5000.0, 1.0, false).approx_eq(&wb(5000.4, 1.0, false)));
    assert!(!wb(5000.0, 1.0, false).approx_eq(&wb(5100.0, 1.0, false)));
    assert!(!wb(5000.0, 1.0, false).approx_eq(&Setting::Toggle { on: true }));
    let nr = |l, c| Setting::NoiseReduction {
        luminance: l,
        color: c,
    };
    assert!(
        nr(25.0, 25.0).approx_eq(&nr(25.0, 25.0)) && !nr(25.0, 25.0).approx_eq(&nr(30.0, 25.0))
    );
    let lv = |b, w| Setting::Levels { black: b, white: w };
    assert!(lv(0.0, 1.0).approx_eq(&lv(0.0, 1.0)) && !lv(0.0, 1.0).approx_eq(&lv(0.05, 1.0)));
    let sky = |p, e| Setting::SkyLayer {
        present: p,
        exposure_ev: e,
        highlights: 0.0,
    };
    assert!(
        sky(false, 1.0).approx_eq(&sky(false, 0.0)) && !sky(true, 1.0).approx_eq(&sky(true, 0.0))
    );
}

#[test]
fn sky_mask_lookup() {
    let m = SkyMask {
        width: 2,
        height: 2,
        data: vec![255, 0, 0, 200],
    };
    assert!(m.is_sky(0.1, 0.1) && !m.is_sky(0.9, 0.1) && m.is_sky(0.9, 0.9));
    assert!(!m.is_sky(-0.1, 0.5) && !m.is_sky(0.5, 1.0));
    assert!(!SkyMask::default().is_sky(0.5, 0.5));
}

#[test]
fn reports_round_trip_through_json() {
    let (raw, sc) = clipped_sky();
    let r = run(&raw, &Adjustments::default(), &sc);
    let json = serde_json::to_string(&r).unwrap();
    let back: OptimizeReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.changes, r.changes);
    assert_eq!(back.summary, r.summary);
    assert!(back.scene.sky_mask.is_none(), "the mask isn't stored");
    // Older or partial reports load with defaults.
    let old: OptimizeReport = serde_json::from_str(r#"{"version":1,"changes":[]}"#).unwrap();
    assert!(old.kept.is_empty() && old.summary.is_empty());
    assert!(json.contains(r#""rule":"highlights_clipped""#), "{json}");
}

#[test]
fn render_stats_of_small_and_plain_images() {
    let sc = SceneSignals::default();
    let tiny = RenderedImage {
        size: Size::new(2, 2),
        rgba8: vec![128; 16],
    };
    let h = crate::histogram(&tiny);
    let s = super::measure::render_stats(&tiny, &h, None, &sc);
    assert_eq!(s.detail, 0.0);
    assert!((s.median_luma - 128.0 / 255.0).abs() < 1e-6);
    // A gray image: no saturation, no detail, a hazy (all-veil) dark channel.
    let gray = RenderedImage {
        size: Size::new(64, 64),
        rgba8: vec![160; 64 * 64 * 4],
    };
    let s = super::measure::render_stats(&gray, &crate::histogram(&gray), None, &sc);
    assert_eq!((s.mean_saturation, s.detail, s.luma_std), (0.0, 0.0, 0.0));
    assert!(s.haze > 0.9, "{}", s.haze);
    // Pure red: saturated.
    let red: Vec<u8> = (0..64 * 64).flat_map(|_| [200, 0, 0, 255]).collect();
    let red = RenderedImage {
        size: Size::new(64, 64),
        rgba8: red,
    };
    let s = super::measure::render_stats(&red, &crate::histogram(&red), None, &sc);
    assert_eq!((s.mean_saturation, s.p90_saturation), (1.0, 1.0));
    assert!(s.haze < 0.05);
}

// ------------------------------------------------------------------ review regressions ---

fn default_on(r: &OptimizeReport) -> Vec<u32> {
    r.changes
        .iter()
        .filter(|c| c.default_on)
        .map(|c| c.id)
        .collect()
}

/// Apply `ids` of `r` and analyze again with the marked report as prior.
fn next(
    raw: &RawImage,
    adj: &Adjustments,
    sc: &SceneSignals,
    r: &OptimizeReport,
    ids: &[u32],
) -> (Adjustments, OptimizeReport, OptimizeReport) {
    let a = apply(adj, r, ids);
    let p = mark_applied(r, ids);
    let again = analyze_cpu(&Input {
        prior: Some(&p),
        ..input(raw, &a, sc)
    })
    .unwrap();
    (a, p, again)
}

/// H1: three runs with a partial selection in between: the optimizer keeps owning
/// what it set in run 1, so run 3 neither re-proposes nor calls it the user's.
#[test]
fn review_ownership_survives_three_runs_with_a_partial_selection() {
    let raw = varied(0.03);
    let sc = SceneSignals::default();
    let r1 = run(&raw, &Adjustments::default(), &sc);
    let all = default_on(&r1);
    assert!(all.len() >= 2, "{:?}", r1.changes);
    let (held, first) = all.split_last().unwrap();
    let (a1, _, r2) = next(&raw, &Adjustments::default(), &sc, &r1, first);
    // Run 2 still offers what was held back, and only that.
    let held_field = r1.changes[*held as usize].field;
    assert_eq!(
        r2.changes
            .iter()
            .filter(|c| c.default_on)
            .map(|c| c.field)
            .collect::<Vec<_>>(),
        [held_field]
    );
    let (a2, _, r3) = next(&raw, &a1, &sc, &r2, &default_on(&r2));
    assert!(
        default_on(&r3).is_empty(),
        "run 3 proposes {:?}",
        r3.changes
    );
    assert!(
        r3.kept
            .iter()
            .all(|k| !matches!(k.why, KeptWhy::UserSet { .. })),
        "run 3 calls optimizer values the user's: {:?}",
        r3.kept
    );
    // And it stays that way.
    let (_, _, r4) = next(&raw, &a2, &sc, &r3, &[]);
    assert!(
        default_on(&r4).is_empty()
            && r4
                .kept
                .iter()
                .all(|k| !matches!(k.why, KeptWhy::UserSet { .. }))
    );
    assert!(r3.owned.iter().any(|o| o.by == SetBy::Optimizer));
}

/// M3: a value the optimizer set and the user reset to its default is the user's: not
/// re-proposed, then or on later runs.
#[test]
fn review_a_reset_to_default_is_the_users() {
    let raw = varied(0.03);
    let sc = SceneSignals::default();
    let r1 = run(&raw, &Adjustments::default(), &sc);
    let ids = default_on(&r1);
    let mut a = apply(&Adjustments::default(), &r1, &ids);
    let p1 = mark_applied(&r1, &ids);
    assert!(a.exposure.exposure_ev > 1.0);
    a.exposure.exposure_ev = 0.0; // the user puts it back
    let r2 = analyze_cpu(&Input {
        prior: Some(&p1),
        ..input(&raw, &a, &sc)
    })
    .unwrap();
    assert!(change(&r2, Field::Exposure).is_none(), "{:?}", r2.changes);
    assert!(
        r2.kept
            .iter()
            .any(|k| k.field == Field::Exposure && matches!(k.why, KeptWhy::UserSet { .. }))
    );
    assert!(
        r2.owned
            .iter()
            .any(|o| o.field == Field::Exposure && o.by == SetBy::User)
    );
    let p2 = mark_applied(&r2, &[]);
    let r3 = analyze_cpu(&Input {
        prior: Some(&p2),
        ..input(&raw, &a, &sc)
    })
    .unwrap();
    assert!(
        change(&r3, Field::Exposure).is_none(),
        "remembered on run 3"
    );
}

/// Strong per-pixel noise at ISO 12800 (the reviewer's case): idempotent too.
#[test]
fn review_noisy_high_iso_is_idempotent() {
    let raw = raw_from_fn(256, 192, |x, y| {
        let (u, v) = (x as f32 / 256.0, y as f32 / 192.0);
        let patch = ((u * 6.0) as usize + (v * 4.0) as usize * 6) % 6;
        let refl = [
            [0.9, 0.9, 0.9],
            [1.6, 0.9, 0.5],
            [0.5, 1.2, 0.6],
            [0.6, 0.8, 1.5],
            [0.25, 0.25, 0.25],
            [1.3, 1.2, 1.0],
        ][patch];
        let t = (1.0 + 0.15 * noise(x / 4, y / 4)) * (1.0 + 0.35 * noise(x, y));
        refl.map(|r| r * 0.06 * (0.4 + 1.2 * u) * t)
    });
    let sc = SceneSignals::default();
    let adj = Adjustments::default();
    let mut inp = input(&raw, &adj, &sc);
    inp.request.capture.iso = Some(12800);
    let r = analyze_cpu(&inp).unwrap();
    let ids = default_on(&r);
    let a = apply(&adj, &r, &ids);
    let p = mark_applied(&r, &ids);
    let mut inp2 = input(&raw, &a, &sc);
    inp2.prior = Some(&p);
    inp2.request.capture.iso = Some(12800);
    let r2 = analyze_cpu(&inp2).unwrap();
    assert!(default_on(&r2).is_empty(), "{:?}", r2.changes);
}

/// M1: a change back toward the default says why less is needed now, not why more was.
#[test]
fn review_easing_back_is_explained_as_such() {
    use super::explain::change as text;
    let grey = scene(256, 192, 0.15, |u, _| [0.2 * (0.4 + u); 3]);
    let sc = SceneSignals::default();
    let r1 = run(&grey, &Adjustments::default(), &sc);
    let v1 = change(&r1, Field::Vibrance).expect("grey is muted");
    let ids = default_on(&r1);
    let a = apply(&Adjustments::default(), &r1, &ids);
    let p = mark_applied(&r1, &ids);
    // The same edit on a colourful photo (a paste): the boost isn't wanted any more.
    let colourful = varied(0.03);
    let r2 = analyze_cpu(&Input {
        prior: Some(&p),
        ..input(&colourful, &a, &sc)
    })
    .unwrap();
    let v2 = change(&r2, Field::Vibrance).expect("vibrance returns to 0");
    assert_eq!(v2.new, Setting::Number { value: 0.0 });
    assert!(
        v2.explanation.contains("isn't needed any more")
            && v2.explanation.contains("average saturation"),
        "{}",
        v2.explanation
    );
    assert!(!v2.explanation.contains("muted"), "{}", v2.explanation);
    assert!(
        r2.summary.contains("removes the earlier vibrance"),
        "{}",
        r2.summary
    );
    assert!(v1.explanation.contains("muted"));
    // Less, not off.
    let e = text(
        Field::Highlights,
        &Setting::Number { value: 40.0 },
        &Setting::Number { value: 20.0 },
        &Reason::HighlightRange { stops_over: -0.2 },
        true,
    );
    assert!(
        e.contains("less is needed now") && e.contains("below the tone curve's roll-off"),
        "{e}"
    );
    for r in [
        Reason::HighlightsClipped {
            clipped_pct: 1.0,
            sky_clipped_pct: None,
        },
        Reason::HighlightRange { stops_over: 0.4 },
        Reason::Shadows {
            dark_pct: 1.0,
            stops_under: 0.0,
            backlit: false,
            subject_luma: None,
            median_luma: 0.4,
        },
        Reason::LowContrast {
            spread: 0.2,
            target: 0.19,
        },
        Reason::Oversaturated {
            p90_saturation: 0.5,
            mean_saturation: 0.3,
        },
        Reason::SoftDetail {
            detail: 0.02,
            target: 0.016,
            landscape: false,
        },
        Reason::Haze {
            haze: 0.1,
            darkest: 0.0,
            spread: 0.2,
        },
        Reason::Tilted { horizon_deg: 0.0 },
    ] {
        let e = text(
            Field::Dehaze,
            &Setting::Number { value: 10.0 },
            &Setting::Number { value: 0.0 },
            &r,
            true,
        );
        assert!(e.contains("isn't needed any more ("), "{e}");
    }
}

/// M2: straightening a user crop shrinks it so no empty corners show, and says so.
#[test]
fn review_straighten_shrinks_a_user_crop() {
    let raw = varied(0.15);
    let mut adj = Adjustments::default();
    adj.crop.rect = NormRect {
        x: 0.0,
        y: 0.0,
        w: 0.9,
        h: 0.9,
    };
    let r = run(&raw, &adj, &sc_h(4.0));
    let c = change(&r, Field::Straighten).unwrap();
    let Setting::Straighten {
        angle_deg,
        crop: Some(rect),
    } = c.new
    else {
        panic!("{:?}", c.new)
    };
    assert_eq!(angle_deg, -4.0);
    assert!(rect.w < 0.9 && rect.h < 0.9);
    // Centre kept, and the turned rectangle fits inside the frame.
    assert!(
        (rect.x + rect.w / 2.0 - 0.45).abs() < 1e-3 && (rect.y + rect.h / 2.0 - 0.45).abs() < 1e-3
    );
    let [w, h] = r.measurements.frame.map(f64::from);
    let (s, co) = 4f64.to_radians().sin_cos();
    let (a, b) = (f64::from(rect.w) * w / 2.0, f64::from(rect.h) * h / 2.0);
    let (cx, cy) = (
        f64::from(rect.x + rect.w / 2.0) * w,
        f64::from(rect.y + rect.h / 2.0) * h,
    );
    assert!(a * co + b * s <= cx.min(w - cx) + 1e-3 && a * s + b * co <= cy.min(h - cy) + 1e-3);
    assert!(
        c.explanation.contains("your crop shrinks"),
        "{}",
        c.explanation
    );
    let applied = apply(&adj, &r, &[c.id]);
    assert_eq!(applied.crop.rect, rect);
    // A second run leaves it.
    let p = mark_applied(&r, &[c.id]);
    let again = analyze_cpu(&Input {
        prior: Some(&p),
        ..input(&raw, &applied, &sc_h(4.0))
    })
    .unwrap();
    assert!(change(&again, Field::Straighten).is_none());
}

/// M4: only applied changes whose value the edit still has count as in effect.
#[test]
fn review_still_applied_follows_the_edit() {
    let raw = varied(0.03);
    let sc = sc_h(2.0);
    let r = run(&raw, &Adjustments::default(), &sc);
    let ids = default_on(&r);
    let a = apply(&Adjustments::default(), &r, &ids);
    let p = mark_applied(&r, &ids);
    assert_eq!(still_applied(&a, &p), ids);
    let mut undone = a.clone();
    undone.exposure.exposure_ev = 0.0;
    let left = still_applied(&undone, &p);
    assert!(
        !left.contains(&change(&r, Field::Exposure).unwrap().id) && left.len() == ids.len() - 1
    );
    assert_eq!(still_applied(&Adjustments::default(), &p).len(), 0);
    assert!(
        still_applied(&a, &r).is_empty(),
        "nothing applied in an unmarked report"
    );
}

/// M5: with dehaze proposed, levels leave the black point alone (dehaze removes the veil).
#[test]
fn review_dehaze_and_black_point_dont_stack() {
    let raw = scene(256, 192, 0.12, |u, v| {
        let base = 0.16 + 0.05 * (u * 7.0).sin() + 0.03 * v;
        [base * 1.02, base, base * 1.05]
    });
    let r = run(&raw, &Adjustments::default(), &SceneSignals::default());
    assert!(change(&r, Field::Dehaze).is_some());
    if let Some(l) = change(&r, Field::Levels) {
        let Setting::Levels { black, .. } = l.new else {
            panic!()
        };
        assert_eq!(black, 0.0, "{l:?}");
    }
}

fn proposal(field: Field, value: f32) -> Outcome {
    Outcome::Change(rules::Proposal {
        field,
        new: Setting::Number { value },
        reason: Reason::LowContrast {
            spread: 0.1,
            target: 0.19,
        },
        confidence: 0.5,
        default_on: true,
    })
}

/// NEW-4: the joint check withdraws the likeliest cause of what got worse, one at a
/// time, and says why.
#[test]
fn review_joint_clipping_withdraws_the_cause() {
    let mut o = rules::Outcomes::default();
    for (f, v) in [
        (Field::Exposure, 0.5),
        (Field::Contrast, 20.0),
        (Field::Vibrance, 15.0),
        (Field::Dehaze, 10.0),
    ] {
        o.push(proposal(f, v));
    }
    o.push(Outcome::Change(rules::Proposal {
        field: Field::Levels,
        new: Setting::Levels {
            black: 0.06,
            white: 0.95,
        },
        reason: Reason::TonalRange {
            darkest: 0.06,
            brightest: 0.95,
        },
        confidence: 0.5,
        default_on: true,
    }));
    assert!(o.proposes_any(&rules::WITHDRAWABLE));
    let mut m = Measurements::default();
    m.baseline.clipped = 0.002;
    m.baseline.crushed = 0.001;
    // Nothing worse: nothing withdrawn.
    m.proposed_clipped = 0.004;
    assert_eq!(rules::clipping_cause(&m, &o), None);
    // White clipping: contrast first, then the levels white point, then vibrance.
    m.proposed_clipped = 0.03;
    assert_eq!(rules::clipping_cause(&m, &o), Some(Field::Contrast));
    o.drop_fields(&[Field::Contrast]);
    assert_eq!(rules::clipping_cause(&m, &o), Some(Field::Levels));
    o.drop_fields(&[Field::Levels]);
    assert_eq!(rules::clipping_cause(&m, &o), Some(Field::Vibrance));
    // Black crushing: dehaze before clarity.
    m.proposed_clipped = 0.004;
    m.proposed_crushed = 0.02;
    assert_eq!(rules::clipping_cause(&m, &o), Some(Field::Dehaze));
    o.drop_fields(&[Field::Dehaze, Field::Vibrance]);
    assert_eq!(
        rules::clipping_cause(&m, &o),
        None,
        "only exposure left: not withdrawable"
    );
    assert!(!o.proposes_any(&rules::WITHDRAWABLE));
    assert_eq!(
        o.default_on_settings(),
        [(Field::Exposure, Setting::Number { value: 0.5 })]
    );
    // The kept note says what would have happened.
    let e = super::explain::kept(
        Field::Contrast,
        "0",
        KeptWhy::Withdrawn {
            clipped_pct: 3.0,
            crushed_pct: 0.1,
            baseline_clipped_pct: 0.2,
            baseline_crushed_pct: 0.1,
        },
        None,
    );
    assert_eq!(
        e,
        "Contrast stays at 0: together with the other changes, 3.0% of the image would clip to white (0.2% without them), so it was left out."
    );
    let e = super::explain::kept(
        Field::Levels,
        "x",
        KeptWhy::Withdrawn {
            clipped_pct: 0.2,
            crushed_pct: 2.0,
            baseline_clipped_pct: 0.2,
            baseline_crushed_pct: 0.1,
        },
        None,
    );
    assert!(e.contains("crush to black"), "{e}");
}

/// NEW-4 end to end: a renderer whose output blows out whenever vibrance is up. The
/// joint probe withdraws it (and anything ahead of it in the order), re-renders until the
/// set is fine, keeps the rest and reports what it left out.
#[test]
fn review_withdrawn_proposals_are_reported() {
    let raw = scene(256, 192, 0.1, |u, v| {
        let l = 0.12 + 0.08 * u;
        [l * (1.0 + 0.1 * v), l, l * (1.0 - 0.1 * v)]
    });
    let (adj, sc) = (Adjustments::default(), SceneSignals::default());
    let inp = input(&raw, &adj, &sc);
    let blocks = crate::auto::block_means(&raw);
    let mut cpu = crate::cpu_renderer();
    let mut renders = 0;
    let r = analyze(&inp, &blocks, &mut |a, req| {
        use crate::Renderer;
        renders += 1;
        let mut img = cpu.render(crate::ImageKey(0), &raw, a, req)?;
        if a.presence.vibrance > 0.0 {
            let n = img.rgba8.len() / 10;
            img.rgba8[..n].fill(255);
        }
        let h = crate::histogram(&img);
        Ok((img, h))
    })
    .unwrap();
    assert!(change(&r, Field::Vibrance).is_none(), "{:?}", r.changes);
    let k = r
        .kept
        .iter()
        .find(|k| k.field == Field::Vibrance)
        .expect("kept note");
    assert!(
        matches!(k.why, KeptWhy::Withdrawn { clipped_pct, .. } if clipped_pct > 5.0),
        "{k:?}"
    );
    assert!(
        k.explanation.contains("would clip to white"),
        "{}",
        k.explanation
    );
    assert!(change(&r, Field::Exposure).is_some(), "the rest stays");
    assert!(renders >= 4, "re-checked after withdrawing: {renders}");
    assert!(r.measurements.proposed_clipped < 0.05);
}

/// M1 follow-up: a sign change explains itself with its own reason.
#[test]
fn review_a_sign_change_uses_its_reason() {
    let e = super::explain::change(
        Field::Saturation,
        &Setting::Number { value: 10.0 },
        &Setting::Number { value: -10.0 },
        &Reason::Oversaturated {
            p90_saturation: 0.98,
            mean_saturation: 0.6,
        },
        true,
    );
    assert!(
        e.contains("98% saturation or more") && !e.contains("less is needed"),
        "{e}"
    );
}

/// M5 follow-up: a dehaze the edit already has keeps the black point too; without any
/// dehaze the black point moves.
#[test]
fn review_black_point_follows_the_dehaze_in_effect() {
    let mut m = Measurements {
        levels: [0.06, 0.95],
        ..Measurements::default()
    };
    m.baseline.p001 = 0.06;
    let sc = SceneSignals::default();
    let mut adj = Adjustments::default();
    let Outcome::Change(p) = rules::levels(&ctx(&adj, &m, &sc)) else {
        panic!()
    };
    assert!(matches!(p.new, Setting::Levels { black, .. } if black == 0.06));
    adj.presence.dehaze = 15.0;
    let Outcome::Change(p) = rules::levels(&ctx(&adj, &m, &sc)) else {
        panic!()
    };
    assert!(matches!(p.new, Setting::Levels { black, .. } if black == 0.0));
}

/// The exposure text sizes the darkness by the change made, and the roll-off text
/// never reads negative.
#[test]
fn review_exposure_text_matches_the_change() {
    use super::explain::change as text;
    let e = text(
        Field::Exposure,
        &Setting::Number { value: 0.0 },
        &Setting::Number { value: 3.0 },
        &Reason::Exposure {
            midtone: 0.000_001,
            target: 0.18,
            limited: true,
            scene: None,
            clipped_pct: None,
        },
        true,
    );
    assert!(e.contains("about 3 stops dark"), "{e}");
    let e = text(
        Field::Highlights,
        &Setting::Number { value: 0.0 },
        &Setting::Number { value: 10.0 },
        &Reason::HighlightRange { stops_over: -0.3 },
        true,
    );
    assert!(e.contains("0.0 stops"), "{e}");
}

/// Cancelling stops the analysis between probes.
#[test]
fn review_cancel_stops_between_renders() {
    let raw = varied(0.1);
    let (adj, sc) = (Adjustments::default(), SceneSignals::default());
    let token = crate::CancelToken::new();
    let mut inp = input(&raw, &adj, &sc);
    inp.request.cancel = Some(token.clone());
    let blocks = crate::auto::block_means(&raw);
    let mut cpu = crate::cpu_renderer();
    let mut n = 0;
    let r = analyze(&inp, &blocks, &mut |a, req| {
        use crate::Renderer;
        n += 1;
        token.cancel();
        let img = cpu.render(
            crate::ImageKey(0),
            &raw,
            a,
            &RenderRequest {
                cancel: None,
                ..req.clone()
            },
        )?;
        let h = crate::histogram(&img);
        Ok((img, h))
    });
    assert!(matches!(r, Err(PipelineError::Cancelled)));
    assert_eq!(n, 1, "stopped after the first probe");
}

// ------------------------------------------------------------------ code review (optimize merge) ---

/// CR-1: AI Denoise the optimizer turned on and the user turned off stays off.
#[test]
fn cr_ai_denoise_turned_off_by_the_user_stays_off() {
    let raw = varied(0.15);
    let sc = SceneSignals {
        ai_denoise_available: true,
        ..SceneSignals::default()
    };
    let adj = Adjustments::default();
    let mut inp = input(&raw, &adj, &sc);
    inp.request.capture.iso = Some(6400);
    let r1 = analyze_cpu(&inp).unwrap();
    let ids = default_on(&r1);
    assert!(change(&r1, Field::AiDenoise).is_some_and(|c| c.default_on));
    let mut a = apply(&adj, &r1, &ids);
    let p1 = mark_applied(&r1, &ids);
    assert!(a.noise.ai_denoise);
    a.noise.ai_denoise = false; // the user turns it off
    let mut inp = input(&raw, &a, &sc);
    inp.prior = Some(&p1);
    inp.request.capture.iso = Some(6400);
    let r2 = analyze_cpu(&inp).unwrap();
    assert!(
        change(&r2, Field::AiDenoise).is_none(),
        "re-proposed: {:?}",
        r2.changes
    );
    assert!(
        r2.kept
            .iter()
            .any(|k| k.field == Field::AiDenoise && matches!(k.why, KeptWhy::UserSet { .. }))
    );
    // Remembered on a third run too.
    let p2 = mark_applied(&r2, &[]);
    let mut inp = input(&raw, &a, &sc);
    inp.prior = Some(&p2);
    inp.request.capture.iso = Some(6400);
    assert!(change(&analyze_cpu(&inp).unwrap(), Field::AiDenoise).is_none());
}

/// CR-2: a clip pull-back of the user's exposure is still the user's exposure: a rerun
/// on the result neither swaps it for the auto midtone exposure nor proposes anything.
#[test]
fn cr_exposure_pull_back_is_idempotent() {
    let raw = varied(0.3);
    let sc = SceneSignals::default();
    let mut adj = Adjustments::default();
    adj.exposure.exposure_ev = 2.5;
    adj.hdr.highlights = 10.0;
    let r1 = run(&raw, &adj, &sc);
    let pulled = num(&change(&r1, Field::Exposure).expect("pulled back").new);
    assert!(pulled < 2.5);
    let (a1, p1, r2) = next(&raw, &adj, &sc, &r1, &default_on(&r1));
    assert!(
        default_on(&r2).is_empty(),
        "run 2 proposes {:?}",
        r2.changes
    );
    assert!(
        r2.kept
            .iter()
            .any(|k| k.field == Field::Exposure && matches!(k.why, KeptWhy::UserSet { .. })),
        "{:?}",
        r2.kept
    );
    // And on a third run, with nothing applied in between.
    let (_, _, r3) = next(&raw, &a1, &sc, &r2, &[]);
    assert!(
        default_on(&r3).is_empty(),
        "run 3 proposes {:?}",
        r3.changes
    );
    // Put back to the default by the user: theirs too.
    let mut reset = a1.clone();
    reset.exposure.exposure_ev = 0.0;
    let r = analyze_cpu(&Input {
        prior: Some(&p1),
        ..input(&raw, &reset, &sc)
    })
    .unwrap();
    assert!(change(&r, Field::Exposure).is_none(), "{:?}", r.changes);
}

/// A hazy frame (lifted blacks, low contrast) with a bright highlight reaching white,
/// analyzed by a renderer that clips a tenth of the frame whenever dehaze is up.
fn hazy_with_dehaze_clipping(
    adj: &Adjustments,
    capture: crate::CaptureInfo,
    prior: Option<&OptimizeReport>,
) -> (OptimizeReport, Vec<Adjustments>) {
    let raw = scene(256, 192, 0.12, |u, v| {
        if u > 0.985 {
            return [1.0; 3];
        }
        let base = 0.04 + 0.0125 * (u * 7.0).sin() + 0.0075 * v;
        [base * 1.02, base, base * 1.05]
    });
    let sc = SceneSignals::default();
    let mut inp = input(&raw, adj, &sc);
    inp.request.capture = capture;
    inp.prior = prior;
    let blocks = crate::auto::block_means(&raw);
    let mut cpu = crate::cpu_renderer();
    let mut seen = Vec::new();
    let r = analyze(&inp, &blocks, &mut |a, req| {
        use crate::Renderer;
        seen.push(a.clone());
        let mut img = cpu.render(crate::ImageKey(0), &raw, a, req)?;
        if a.presence.dehaze > 0.0 {
            let n = img.rgba8.len() / 10;
            img.rgba8[..n].fill(255);
        }
        let h = crate::histogram(&img);
        Ok((img, h))
    })
    .unwrap();
    (r, seen)
}

/// CR-4: dehaze withdrawn by the joint check no longer holds the levels black point.
#[test]
fn cr_withdrawn_dehaze_releases_the_black_point() {
    let (r, _) =
        hazy_with_dehaze_clipping(&Adjustments::default(), crate::CaptureInfo::default(), None);
    assert!(change(&r, Field::Dehaze).is_none(), "{:?}", r.changes);
    assert!(
        r.kept
            .iter()
            .any(|k| k.field == Field::Dehaze && matches!(k.why, KeptWhy::Withdrawn { .. }))
    );
    let l = change(&r, Field::Levels).expect("levels");
    assert!(
        matches!(l.new, Setting::Levels { black, .. } if black > 0.05),
        "{l:?}"
    );
}

/// CR-5: the joint check renders with the geometry the baseline was measured with,
/// the unchecked lens correction included.
#[test]
fn cr_joint_check_measures_like_the_baseline() {
    let mut adj = Adjustments::default();
    adj.lens.enabled = false;
    let capture = crate::CaptureInfo {
        lens: Some("FE 24-70mm F2.8 GM".into()),
        ..crate::CaptureInfo::default()
    };
    let (r, seen) = hazy_with_dehaze_clipping(&adj, capture, None);
    assert!(change(&r, Field::LensCorrection).is_some_and(|c| !c.default_on));
    assert!(seen.len() >= 3, "{}", seen.len());
    assert!(!seen[0].lens.enabled, "the current render");
    assert!(
        seen[1..].iter().all(|a| a.lens.enabled),
        "{:?}",
        seen.iter().map(|a| a.lens.enabled).collect::<Vec<_>>()
    );
}

/// CR-4 follow-up: the released black point is idempotent. Run 1 withdraws dehaze and
/// moves the black point; runs 2 and 3 propose nothing and still report the withdrawals.
#[test]
fn cr_released_black_point_is_idempotent_over_three_runs() {
    let cap = crate::CaptureInfo::default;
    let (r1, _) = hazy_with_dehaze_clipping(&Adjustments::default(), cap(), None);
    let l = change(&r1, Field::Levels).expect("levels");
    assert!(
        matches!(l.new, Setting::Levels { black, .. } if (black - 0.08).abs() < 1e-6),
        "{l:?}"
    );
    let withdrawn = |r: &OptimizeReport, f: Field| {
        r.kept
            .iter()
            .any(|k| k.field == f && matches!(k.why, KeptWhy::Withdrawn { .. }))
    };
    assert!(change(&r1, Field::Dehaze).is_none() && withdrawn(&r1, Field::Dehaze));
    let mut adj = Adjustments::default();
    let mut prior = r1;
    for run in 2..=3 {
        let ids = default_on(&prior);
        adj = apply(&adj, &prior, &ids);
        let marked = mark_applied(&prior, &ids);
        let (r, _) = hazy_with_dehaze_clipping(&adj, cap(), Some(&marked));
        assert!(
            default_on(&r).is_empty(),
            "run {run} proposes {:?}",
            r.changes
        );
        for f in [Field::Dehaze, Field::Vibrance, Field::Clarity] {
            assert!(
                withdrawn(&r, f),
                "run {run}: {f:?} not withdrawn: {:?}",
                r.kept
            );
        }
        prior = r;
    }
}
