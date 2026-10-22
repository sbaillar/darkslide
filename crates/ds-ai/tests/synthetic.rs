//! End-to-end on synthetic pixels: grouping + group scoring + picks, and
//! degenerate inputs through the whole pipeline.

#![allow(clippy::float_cmp)]

mod support;

use ds_ai::{
    AnalysisConfig, CullThresholds, CullWeights, GroupConfig, GroupItem, GroupKind, ImageAnalysis,
    LumaImage, Reason, analyze, group_images, score, score_group, suggest_picks,
};
use support::{box_blur, synthetic_scene};

fn img(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> LumaImage {
    LumaImage::from_gray8(w, h, (0..w * h).map(|i| f(i % w, i / w)).collect()).unwrap()
}

fn run(i: &LumaImage) -> ImageAnalysis {
    analyze(i, &AnalysisConfig::default())
}

#[test]
fn degenerate_inputs_are_safe_and_neutral() {
    let cases = [
        img(0, 0, |_, _| 0),
        img(1, 1, |_, _| 255),
        img(1, 400, |_, y| y as u8),
        img(400, 1, |x, _| x as u8),
        img(2, 2, |x, _| x as u8 * 255),
        img(64, 64, |_, _| 0),
        img(64, 64, |_, _| 255),
        LumaImage::from_f32(8, 8, &[f32::NAN; 64]).unwrap(),
    ];
    let (w, t) = (CullWeights::default(), CullThresholds::default());
    let analyses: Vec<ImageAnalysis> = cases.iter().map(run).collect();
    for (c, a) in cases.iter().zip(&analyses) {
        let s = score(a, None, &w, &t);
        assert!((0.0..=1.0).contains(&s.score), "{c:?}: {s:?}");
        assert!(a.sharpness_value().is_finite() && a.exposure.dynamic_range_stops.is_finite());
    }
    // Mixed sizes in one group fall back to plain value ratios; still finite.
    for s in score_group(&analyses, &[], &w, &t) {
        assert!((0.0..=1.0).contains(&s.score), "{s:?}");
    }
    let black = &analyses[5];
    assert_eq!(black.sharpness_value(), 0.0);
    assert_eq!(black.noise.sampled_fraction, 0.0);
    let r = score(black, None, &w, &t).reasons;
    assert!(
        r.contains(&Reason::Underexposed) && r.contains(&Reason::OutOfFocus),
        "{r:?}"
    );
}

#[test]
fn a9_burst_end_to_end() {
    // Twelve 20 fps frames panning across the scene (whole-second stamps, shot
    // numbers for order); frames 3 and 7 are soft, frame 9 is smeared. Then a
    // second burst of a different scene.
    let scene = synthetic_scene(560, 320, 11);
    let frame = |k: usize| {
        let crop = img(480, 320, |x, y| scene.pixels()[y * 560 + x + k * 4]);
        match k {
            3 | 7 => box_blur(&crop, 2, 2),
            9 => box_blur(&crop, 6, 0),
            _ => crop,
        }
    };
    let other = img(480, 320, |x, y| {
        let d = (x as f32 - 140.0).hypot(y as f32 - 220.0);
        if d < 70.0 { 230 } else { (20 + y / 6) as u8 }
    });
    let mut analyses: Vec<ImageAnalysis> = (0..12).map(|k| run(&frame(k))).collect();
    analyses.push(run(&other));
    analyses.push(run(&box_blur(&other, 1, 1)));
    let items: Vec<GroupItem> = analyses
        .iter()
        .enumerate()
        .map(|(k, a)| GroupItem {
            capture_ms: Some(if k < 12 { 1_000 } else { 9_000 }),
            seq: Some(8_000 + k as u64),
            hash: a.hash,
            camera: None,
        })
        .collect();

    let groups = group_images(&items, &GroupConfig::default(), None);
    assert_eq!(groups.len(), 2, "{groups:?}");
    assert_eq!(groups[0].members, (0..12).collect::<Vec<_>>());
    assert_eq!(groups[1].members, vec![12, 13]);
    assert!(groups.iter().all(|g| g.kind == GroupKind::Burst));

    let burst: Vec<ImageAnalysis> = groups[0]
        .members
        .iter()
        .map(|&i| analyses[i].clone())
        .collect();
    let t = CullThresholds::default();
    let scores = score_group(&burst, &[], &CullWeights::default(), &t);
    let picks = suggest_picks(&scores, &t);
    assert!(![3, 7, 9].contains(&picks.best.unwrap()), "{picks:?}");
    for bad in [3, 7, 9] {
        assert!(picks.rejects.contains(&bad), "{bad}: {picks:?}");
    }
    // The heavy 13 px smear leaves too little detail to tell direction from defocus.
    assert!(scores[9].has_hard_failure(), "{:?}", scores[9]);
}
