//! Tests on the real Sony A9 embedded previews (1616×1080). They skip when
//! `samples/` is absent; unit and synthetic tests cover the same logic.

#![allow(clippy::float_cmp)]

mod support;

use ds_ai::{
    AnalysisConfig, CullThresholds, CullWeights, GroupConfig, GroupItem, GroupKind, LumaImage,
    PerceptualHash, Reason, analyze, group_focus, group_images, score_group, suggest_picks,
};

fn shifted(img: &LumaImage, dx: usize) -> LumaImage {
    let (w, h) = (img.width(), img.height());
    let px = img.pixels();
    let data = (0..w * h)
        .map(|k| px[(k / w) * w + ((k % w) + dx).min(w - 1)])
        .collect();
    LumaImage::from_gray8(w, h, data).unwrap()
}

fn map(img: &LumaImage, f: impl Fn(usize, usize, u8) -> u8) -> LumaImage {
    let w = img.width();
    let data = img
        .pixels()
        .iter()
        .enumerate()
        .map(|(k, &p)| f(k % w, k / w, p))
        .collect();
    LumaImage::from_gray8(w, img.height(), data).unwrap()
}

#[test]
fn previews_decode_at_a9_size() {
    for (name, img) in support::sample_previews() {
        assert_eq!((img.width(), img.height()), (1616, 1080), "{name}");
    }
}

#[test]
fn sharp_frame_outranks_its_blurred_copies() {
    let cfg = AnalysisConfig::default();
    for (name, img) in support::sample_previews() {
        let vals: Vec<f32> = [
            img.clone(),
            support::box_blur(&img, 1, 1),
            support::box_blur(&img, 2, 2),
        ]
        .iter()
        .map(|i| analyze(i, &cfg).sharpness_value())
        .collect();
        assert!(
            vals[0] > 3.0 * vals[1] && vals[1] > vals[2],
            "{name}: {vals:?}"
        );
    }
}

#[test]
fn flicker_and_underexposure_do_not_read_as_blur() {
    // −2 EV and a +15 % flicker frame keep their relative sharpness.
    // (Before contrast normalization the −2 EV frame measured ~1/16 of the
    // original and was rejected as out of focus.) Scaling 8-bit data by 0.25
    // does destroy 1–3 DN detail, so a partial drop is real, not an artefact.
    let cfg = AnalysisConfig::default();
    for (name, img) in support::sample_previews() {
        let dark = map(&img, |_, _, p| (f32::from(p) * 0.25).round() as u8);
        let flicker = map(&img, |_, _, p| {
            (f32::from(p) * 1.15).round().min(255.0) as u8
        });
        let a: Vec<_> = [&img, &dark, &flicker]
            .iter()
            .map(|i| analyze(i, &cfg))
            .collect();
        let rel = group_focus(&a.iter().map(|a| &a.sharpness).collect::<Vec<_>>()).relative;
        assert!(
            rel.iter().all(|&r| r > 0.6) && rel[2] > 0.9,
            "{name}: {rel:?}"
        );
        let s = score_group(&a, &[], &CullWeights::default(), &CullThresholds::default());
        for s in &s {
            let blur_reason = s.reasons.iter().any(|r| {
                matches!(
                    r,
                    Reason::OutOfFocus | Reason::SofterThanGroup { .. } | Reason::MotionBlur { .. }
                )
            });
            assert!(!blur_reason && !s.has_hard_failure(), "{name}: {s:?}");
        }
    }
}

#[test]
fn smeared_frame_is_rejected_and_sharp_frames_are_not() {
    // A real 3 px horizontal smear among sharp frames always ranks below
    // them; it is rejected unless the scene is lines running along the smear
    // (SKB06577: the smear keeps 80 % of the sharpness — keeping it is the
    // cheap error).
    let cfg = AnalysisConfig::default();
    let t = CullThresholds::default();
    for (name, img) in support::sample_previews() {
        let frames: Vec<_> = [
            img.clone(),
            shifted(&img, 6),
            support::box_blur(&img, 1, 0),
            shifted(&img, 12),
        ]
        .iter()
        .map(|i| analyze(i, &cfg))
        .collect();
        let s = score_group(&frames, &[], &CullWeights::default(), &t);
        let picks = suggest_picks(&s, &t);
        assert!(
            [0, 1, 3].iter().all(|&k| s[2].score < s[k].score),
            "{name}: {s:?}"
        );
        let rel = ds_ai::group_focus(&frames.iter().map(|a| &a.sharpness).collect::<Vec<_>>())
            .relative[2];
        assert!(
            picks.rejects.contains(&2) || rel > 0.75,
            "{name}: {picks:?} {s:?}"
        );
        for k in [0, 1, 3] {
            assert!(!s[k].has_hard_failure(), "{name} frame {k}: {:?}", s[k]);
        }
    }
}

#[test]
fn hashes_survive_blur_and_small_pans_but_separate_scenes() {
    let previews = support::sample_previews();
    let hashes: Vec<PerceptualHash> = previews
        .iter()
        .map(|(_, i)| PerceptualHash::of(i))
        .collect();
    for (k, (name, img)) in previews.iter().enumerate() {
        let h = hashes[k];
        assert!(
            h.distance(&PerceptualHash::of(&support::box_blur(img, 2, 2))) < 0.05,
            "{name}"
        );
        assert!(
            h.distance(&PerceptualHash::of(&shifted(img, 30))) < 0.15,
            "{name}"
        );
        for (j, other) in hashes.iter().enumerate() {
            if j != k {
                assert!(
                    h.distance(other) > GroupConfig::default().near_dup_max_hash_distance,
                    "{name} vs {j}"
                );
            }
        }
    }
}

#[test]
fn simulated_bursts_group_and_pick_the_sharp_frame() {
    let previews = support::sample_previews();
    let cfg = AnalysisConfig::default();
    let mut items = Vec::new();
    let mut analyses = Vec::new();
    for (k, (_, img)) in previews.iter().enumerate() {
        // Whole-second stamps like a real ARW; order comes from the shot number.
        let frames = [
            support::box_blur(img, 2, 2),
            img.clone(),
            shifted(img, 12),
            support::box_blur(img, 5, 0),
        ];
        for (f, frame) in frames.iter().enumerate() {
            let a = analyze(frame, &cfg);
            items.push(GroupItem {
                capture_ms: Some(k as i64 * 3_000 + if f < 2 { 0 } else { 1_000 }),
                seq: Some((k * 4 + f) as u64),
                camera: None,
                hash: a.hash,
            });
            analyses.push(a);
        }
    }
    let groups = group_images(&items, &GroupConfig::default(), None);
    assert_eq!(groups.len(), previews.len());
    for (k, g) in groups.iter().enumerate() {
        assert_eq!(g.kind, GroupKind::Burst);
        assert_eq!(g.members, (k * 4..k * 4 + 4).collect::<Vec<_>>());
        let burst: Vec<_> = g.members.iter().map(|&i| analyses[i].clone()).collect();
        let scores = score_group(
            &burst,
            &[],
            &CullWeights::default(),
            &CullThresholds::default(),
        );
        let picks = suggest_picks(&scores, &CullThresholds::default());
        assert!(
            matches!(picks.best, Some(1 | 2)),
            "burst {k}: {picks:?} {scores:?}"
        );
        assert!(
            picks.rejects.contains(&0) && picks.rejects.contains(&3),
            "burst {k}: {picks:?}"
        );
    }
}

#[test]
fn brightening_clips_highlights() {
    let cfg = AnalysisConfig::default();
    for (name, img) in support::sample_previews() {
        let a = analyze(&img, &cfg).exposure;
        let b = analyze(&map(&img, |_, _, p| p.saturating_add(90)), &cfg).exposure;
        assert!(
            b.highlights_clipped > a.highlights_clipped + 0.01,
            "{name}: {a:?} {b:?}"
        );
        assert!(
            a.dynamic_range_stops > 3.0 && a.dynamic_range_stops < 13.0,
            "{name}: {a:?}"
        );
    }
}
