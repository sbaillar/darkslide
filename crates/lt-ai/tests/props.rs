//! Property tests: hash metric laws, grouping determinism/partitioning, and
//! robustness of analysis and intent handling against arbitrary input.

#![allow(clippy::float_cmp)]

use lt_ai::{
    AnalysisConfig, CullThresholds, CullWeights, EditIntent, GroupConfig, GroupItem, IntentBackend,
    IntentContext, LumaImage, PerceptualHash, RuleBasedParser, Straighten, analyze, apply_intent,
    group_images, score,
};
use lt_core::Adjustments;
use proptest::prelude::*;

fn hash() -> impl Strategy<Value = PerceptualHash> {
    (any::<u64>(), any::<u64>()).prop_map(|(dhash, phash)| PerceptualHash { dhash, phash })
}

fn items() -> impl Strategy<Value = Vec<GroupItem>> {
    // Small hash alphabet and a short time range so links actually happen.
    let hash = prop_oneof![Just(0u64), Just(0xFF), Just(0xFFFF_FFFF), Just(u64::MAX)]
        .prop_map(|d| PerceptualHash { dhash: d, phash: 0 });
    let t = prop_oneof![3 => (0i64..2_000).prop_map(Some), 1 => Just(None)];
    let seq = prop::option::of(0u64..50);
    let camera = prop::option::of(0u64..2);
    prop::collection::vec(
        (t, seq, camera, hash).prop_map(|(capture_ms, seq, camera, hash)| GroupItem {
            capture_ms,
            seq,
            camera,
            hash,
        }),
        0..40,
    )
}

proptest! {
    #[test]
    fn hash_distance_is_a_metric(a in hash(), b in hash(), c in hash()) {
        prop_assert_eq!(a.distance(&b), b.distance(&a));
        prop_assert_eq!(a.distance(&a), 0.0);
        prop_assert!((0.0..=1.0).contains(&a.distance(&b)));
        prop_assert!(a.distance(&c) <= a.distance(&b) + b.distance(&c) + 1e-6);
    }

    #[test]
    fn grouping_is_a_deterministic_partition(items in items()) {
        let cfg = GroupConfig::default();
        let g1 = group_images(&items, &cfg, None);
        let g2 = group_images(&items, &cfg, None);
        prop_assert_eq!(&g1, &g2);
        let mut seen: Vec<usize> = g1.iter().flat_map(|g| g.members.iter().copied()).collect();
        seen.sort_unstable();
        prop_assert_eq!(seen, (0..items.len()).collect::<Vec<_>>());
    }

    #[test]
    fn grouping_ignores_input_order_when_times_are_distinct(
        items in items(),
        seed in any::<u64>(),
    ) {
        // Whole-second stamps (ties within a second) with distinct shot
        // numbers, as an A9 writes them; then shuffle.
        let items: Vec<GroupItem> = items
            .into_iter()
            .enumerate()
            .map(|(k, it)| GroupItem { capture_ms: Some(k as i64 / 20 * 1000), seq: Some(k as u64), ..it })
            .collect();
        let mut perm: Vec<usize> = (0..items.len()).collect();
        let mut s = seed | 1;
        for i in (1..perm.len()).rev() {
            s ^= s << 13; s ^= s >> 7; s ^= s << 17;
            perm.swap(i, (s % (i as u64 + 1)) as usize);
        }
        let shuffled: Vec<GroupItem> = perm.iter().map(|&i| items[i]).collect();
        let cfg = GroupConfig::default();
        let canon = |groups: Vec<lt_ai::Group>, map: &dyn Fn(usize) -> usize| {
            let mut sets: Vec<Vec<usize>> = groups
                .into_iter()
                .map(|g| { let mut m: Vec<usize> = g.members.into_iter().map(map).collect(); m.sort_unstable(); m })
                .collect();
            sets.sort();
            sets
        };
        let a = canon(group_images(&items, &cfg, None), &|i| i);
        let b = canon(group_images(&shuffled, &cfg, None), &|i| perm[i]);
        prop_assert_eq!(a, b);
    }

    #[test]
    fn analysis_is_total_on_arbitrary_small_images(
        (w, h, data) in (0usize..24, 0usize..24).prop_flat_map(|(w, h)| {
            (Just(w), Just(h), prop::collection::vec(any::<u8>(), w * h))
        })
    ) {
        let img = LumaImage::from_gray8(w, h, data).unwrap();
        let a = analyze(&img, &AnalysisConfig::default());
        prop_assert!(a.sharpness_value().is_finite() && a.sharpness_value() >= 0.0);
        prop_assert!((0.0..=1.0).contains(&a.sharpness.directional_ratio));
        prop_assert!(a.noise.sigma.is_finite() && a.exposure.dynamic_range_stops.is_finite());
        let s = score(&a, None, &CullWeights::default(), &CullThresholds::default());
        prop_assert!((0.0..=1.0).contains(&s.score));
    }

    #[test]
    fn parser_never_panics_and_results_stay_in_range(text in "\\PC{0,60}") {
        if let Ok(intent) = RuleBasedParser.parse(&text) {
            check_ranges(&apply_intent(&Adjustments::default(), &intent.intent, &ctx()).adjustments)?;
        }
    }

    #[test]
    fn parser_handles_arbitrary_keyword_soup(words in prop::collection::vec(prop::sample::select(vec![
        "warmer", "cooler", "too", "less", "more", "a", "bit", "lot", "brighter", "darker", "shadows",
        "highlights", "contrast", "saturation", "crop", "4:5", "16x9", "by", "straighten", "rotate", "-3",
        "+0.5", "1/0", "stops", "degrees", "cw", "and", ",", ".", "black and white", "color", "back", "auto",
        "square", "portrait", "magenta", "green", "tint", "one", "stop", "half", "99999", "nan", "inf",
    ]), 0..12)) {
        let text = words.join(" ");
        let report = RuleBasedParser.parse_detailed(&text);
        check_ranges(&apply_intent(&Adjustments::default(), &report.intent, &ctx()).adjustments)?;
        prop_assert_eq!(RuleBasedParser.parse(&text).is_ok(), !report.intent.is_empty());
    }

    #[test]
    fn apply_intent_clamps_any_numbers(v in prop::array::uniform8(prop_oneof![
        any::<f32>(), Just(f32::NAN), Just(f32::INFINITY), -1e6f32..1e6
    ])) {
        let intent = EditIntent {
            warmth: v[0], tint: v[1], exposure_ev: v[2], contrast: v[3],
            saturation: v[4], shadows: v[5], highlights: v[6],
            straighten: Some(Straighten::Degrees(v[7])),
            ..EditIntent::default()
        };
        check_ranges(&apply_intent(&Adjustments::default(), &intent, &ctx()).adjustments)?;
    }
}

fn ctx() -> IntentContext {
    IntentContext {
        image_size: Some(lt_core::Size::new(6000, 4000)),
        as_shot_wb: Some((5200.0, 3.0)),
    }
}

fn check_ranges(a: &Adjustments) -> Result<(), TestCaseError> {
    use lt_core::adjust::WhiteBalance;
    let e = &a.exposure;
    prop_assert!((-5.0..=5.0).contains(&e.exposure_ev));
    for v in [e.contrast, e.saturation, a.hdr.shadows, a.hdr.highlights] {
        prop_assert!((-100.0..=100.0).contains(&v), "{v}");
    }
    prop_assert!((-45.0..=45.0).contains(&a.crop.angle_deg));
    if let WhiteBalance::Custom { temp_k, tint } = a.white_balance {
        prop_assert!((2000.0..=50_000.0).contains(&temp_k) && (-150.0..=150.0).contains(&tint));
    }
    Ok(())
}
