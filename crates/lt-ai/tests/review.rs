//! Regression tests for the adversarial reviews (the parser repros live in
//! `intent::rules` tests). Pixel scenarios run on the real A9 previews and skip without
//! `samples/`; the grouping ones are synthetic.
//!
//! Where the reviewer's scenario is undecidable without a subject (a held
//! frame and a background-focused frame are symmetric), the assertion is the
//! design rule instead: *a good frame is never rejected*.

#![allow(clippy::float_cmp)]

mod support;

use lt_ai::{
    AnalysisConfig, CullScore, CullThresholds, CullWeights, GroupConfig, GroupItem, ImageAnalysis,
    LumaImage, PerceptualHash, PickSuggestion, Reason, SubjectRegion, analyze_with_subject,
    group_focus, group_images, score, score_group, suggest_picks,
};
use lt_core::NormRect;
use support::box_blur;

const W: usize = 1616;
const CENTRE: [usize; 4] = [539, 360, 1077, 720];

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

fn shifted(img: &LumaImage, dx: usize) -> LumaImage {
    let px = img.pixels();
    map(img, |x, y, _| px[y * W + (x + dx).min(W - 1)])
}

/// `a` inside the box, `b` outside.
fn mix(a: &LumaImage, b: &LumaImage, bx: [usize; 4]) -> LumaImage {
    let (pa, pb) = (a.pixels(), b.pixels());
    let inside = |x: usize, y: usize| (bx[0]..bx[2]).contains(&x) && (bx[1]..bx[3]).contains(&y);
    map(a, |x, y, _| {
        if inside(x, y) {
            pa[y * W + x]
        } else {
            pb[y * W + x]
        }
    })
}

fn roi(x: f32, y: f32, w: f32, h: f32) -> SubjectRegion {
    SubjectRegion {
        rect: NormRect { x, y, w, h },
        weight: 1.0,
    }
}

fn centre() -> SubjectRegion {
    roi(1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0)
}

fn an(i: &LumaImage, subject: &[SubjectRegion]) -> ImageAnalysis {
    analyze_with_subject(i, &AnalysisConfig::default(), subject)
}

fn judge(frames: &[ImageAnalysis]) -> (Vec<CullScore>, PickSuggestion) {
    let t = CullThresholds::default();
    let s = score_group(frames, &[], &CullWeights::default(), &t);
    let p = suggest_picks(&s, &t);
    (s, p)
}

/// Deterministic Gaussian-ish noise field, optionally box-correlated.
fn noisy(img: &LumaImage, sigma: f32, corr: usize, seed: u64) -> LumaImage {
    let (w, h) = (img.width(), img.height());
    let mut s = seed | 1;
    let mut u = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f32 / (1u64 << 53) as f32
    };
    let white: Vec<f32> = (0..w * h)
        .map(|_| (u() + u() + u() + u() - 2.0) * 3f32.sqrt())
        .collect();
    let k = (2 * corr + 1) as f32;
    let field = |x: usize, y: usize| {
        if corr == 0 || x < corr || y < corr || x + corr >= w || y + corr >= h {
            return white[y * w + x];
        }
        let mut a = 0.0;
        for dy in 0..=2 * corr {
            for dx in 0..=2 * corr {
                a += white[(y + dy - corr) * w + x + dx - corr];
            }
        }
        a / k
    };
    let px = img.pixels();
    map(img, |x, y, _| {
        (f32::from(px[y * w + x]) + sigma * field(x, y))
            .round()
            .clamp(0.0, 255.0) as u8
    })
}

#[test]
fn c1_missed_focus() {
    for (name, img) in support::sample_previews() {
        let soft2 = box_blur(&img, 2, 2);
        let soft1 = box_blur(&img, 1, 1);
        let held = mix(&img, &soft2, CENTRE);
        let missed2 = mix(&soft2, &img, CENTRE);
        let missed1 = mix(&soft1, &img, CENTRE);
        let h2 = shifted(&held, 4);

        // Subject known: the held frame wins and the missed ones are rejected.
        let r = [centre()];
        let (_, p) = judge(&[an(&held, &r), an(&missed2, &r), an(&missed1, &r)]);
        assert!(
            p.best == Some(0) && p.rejects.contains(&1) && p.rejects.contains(&2),
            "{name}: {p:?}"
        );
        // Vision lost the subject in one frame: its neighbours lend theirs.
        let (_, p) = judge(&[an(&held, &r), an(&missed2, &[]), an(&missed1, &r)]);
        assert_eq!(p.best, Some(0), "{name} mixed ROI: {p:?}");

        // No subject, frames disagree on where focus is: uncertain, and the
        // held frame is never rejected (was: rejected as "out of focus").
        for frames in [
            vec![an(&held, &[]), an(&missed2, &[]), an(&missed1, &[])],
            vec![an(&held, &[]), an(&missed2, &[])],
            vec![an(&held, &[]), an(&missed2, &[]), an(&h2, &[])],
        ] {
            let (s, p) = judge(&frames);
            assert!(
                s.iter()
                    .all(|s| s.reasons.contains(&Reason::FocusUncertain)),
                "{name}: {s:?}"
            );
            let gf = group_focus(&frames.iter().map(|a| &a.sharpness).collect::<Vec<_>>());
            assert!(
                !p.rejects.contains(&0),
                "{name}: held rejected {p:?} {gf:?} {s:?}"
            );
            assert!(s.iter().all(|s| !s.has_hard_failure()), "{name}: {s:?}");
        }
        // Lone frames: no hard verdict either way.
        let (w, t) = (CullWeights::default(), CullThresholds::default());
        for f in [&held, &missed2] {
            assert!(
                !score(&an(f, &[]), None, &w, &t).has_hard_failure(),
                "{name}"
            );
        }

        // Two faces (background + subject), most frames held: the missed frame
        // never wins outright and no held frame is rejected.
        let bg_face = roi(0.05, 0.05, 0.15, 0.2);
        let (s, p) = judge(&[
            an(&held, &[bg_face, centre()]),
            an(&missed2, &[bg_face, centre()]),
            an(&h2, &[bg_face, centre()]),
        ]);
        assert!(
            !p.rejects.contains(&0) && !p.rejects.contains(&2),
            "{name}: {p:?} {s:?}"
        );

        // Moving subject, static camera: a frame sharp nowhere is rejected,
        // and frames sharp somewhere are never rejected.
        let xs = [100usize, 350, 600, 850, 1100];
        let window = |x0: usize| [x0, 300, x0 + 400, 800];
        let mut frames: Vec<_> = xs
            .iter()
            .map(|&x0| an(&mix(&img, &soft2, window(x0)), &[]))
            .collect();
        frames[2] = an(&soft2, &[]);
        let (_, p) = judge(&frames);
        assert!(p.rejects.contains(&2) && p.best != Some(2), "{name}: {p:?}");
        frames[2] = an(&mix(&soft2, &img, window(xs[2])), &[]);
        let (sc, p) = judge(&frames);
        let gf = group_focus(&frames.iter().map(|a| &a.sharpness).collect::<Vec<_>>());
        assert!(
            !p.rejects.iter().any(|&k| k != 2),
            "{name}: sharp frames rejected {p:?} {gf:?} {sc:?}"
        );
    }
}

#[test]
fn identical_subjects_score_alike_whatever_the_background() {
    // Same subject pixels; background sharp, bokeh or
    // flat. The busy background used to be subtracted from the subject.
    for (name, img) in support::sample_previews() {
        let soft = box_blur(&box_blur(&img, 4, 4), 4, 4);
        let lowc = map(&img, |x, y, p| {
            let inside = (CENTRE[0]..CENTRE[2]).contains(&x) && (CENTRE[1]..CENTRE[3]).contains(&y);
            if inside {
                (128.0 + 0.3 * (f32::from(p) - 128.0)).round() as u8
            } else {
                p
            }
        });
        let lsoft = box_blur(&box_blur(&lowc, 4, 4), 4, 4);
        for (base, bsoft) in [(&img, &soft), (&lowc, &lsoft)] {
            let flat = map(base, |x, y, p| {
                let inside =
                    (CENTRE[0]..CENTRE[2]).contains(&x) && (CENTRE[1]..CENTRE[3]).contains(&y);
                if inside { p } else { 120 }
            });
            let frames = [
                an(base, &[centre()]),
                an(&mix(base, bsoft, CENTRE), &[centre()]),
                an(&flat, &[centre()]),
            ];
            let rel =
                group_focus(&frames.iter().map(|a| &a.sharpness).collect::<Vec<_>>()).relative;
            assert!(rel.iter().all(|&r| r > 0.75), "{name}: {rel:?}");
            assert!(judge(&frames).1.rejects.is_empty(), "{name}");
        }
    }
}

#[test]
fn exposure_motion_and_noise() {
    let (w, t) = (CullWeights::default(), CullThresholds::default());
    for (name, img) in support::sample_previews() {
        // Exposure changes are not blur (−3 EV linear in 8 bits is left
        // out: it keeps 5 bits and really loses fine detail).
        let lin = |k: f32| {
            map(&img, |_, _, p| {
                (f32::from(p) * k).round().clamp(0.0, 255.0) as u8
            })
        };
        let gamma = |ev: f32| {
            map(&img, |_, _, p| {
                let l = (f32::from(p) / 255.0).powf(2.2) * 2f32.powf(ev);
                (l.min(1.0).powf(1.0 / 2.2) * 255.0).round() as u8
            })
        };
        let frames: Vec<_> = [img.clone(), lin(0.5), lin(0.25), gamma(1.0), gamma(-2.0)]
            .iter()
            .map(|i| an(i, &[]))
            .collect();
        let (s, _) = judge(&frames);
        for s in &s {
            let blurish = s.reasons.iter().any(|r| {
                matches!(
                    r,
                    Reason::OutOfFocus | Reason::SofterThanGroup { .. } | Reason::MotionBlur { .. }
                )
            });
            assert!(!blurish, "{name}: {s:?}");
        }
        let (_, p) = judge(&[an(&gamma(-2.0), &[]), an(&box_blur(&img, 1, 1), &[])]);
        assert_eq!(p.best, Some(0), "{name}: −2 EV sharp must beat 0 EV soft");

        // Smears rejected, panning and sharp frames kept, with and without a subject.
        let streak = box_blur(&box_blur(&img, 6, 0), 6, 0);
        let imgs = [
            img.clone(),
            mix(&img, &streak, CENTRE),
            streak.clone(),
            box_blur(&img, 3, 0),
            box_blur(&img, 0, 3),
            shifted(&img, 8),
        ];
        for subject in [vec![], vec![centre()]] {
            let frames: Vec<_> = imgs.iter().map(|i| an(i, &subject)).collect();
            let (s, p) = judge(&frames);
            let gf = group_focus(&frames.iter().map(|a| &a.sharpness).collect::<Vec<_>>());
            assert!(
                [2, 3, 4].iter().all(|k| p.rejects.contains(k)),
                "{name}: {p:?} {gf:?} {s:?}"
            );
            assert!(
                !s[1].has_hard_failure() && !p.rejects.contains(&5),
                "{name} {}: {p:?} {gf:?} {s:?}",
                subject.len()
            );
            if !subject.is_empty() {
                assert!(!p.rejects.contains(&1), "{name}: panning rejected {p:?}");
            }
        }
        for k in [0, 5] {
            let lone = score(&an(&imgs[k], &[]), None, &w, &t);
            assert!(
                !lone
                    .reasons
                    .iter()
                    .any(|r| matches!(r, Reason::MotionBlur { .. })),
                "{name}"
            );
        }

        // Noise. Soft frames lose even when noise fakes detail; a burst
        // with uniform noise is not "noisy" against itself; heavy correlated
        // noise on a lone frame is reported.
        let b1 = box_blur(&img, 1, 1);
        let mixed = [
            img.clone(),
            b1.clone(),
            noisy(&b1, 8.0, 0, 3),
            noisy(&b1, 8.0, 1, 4),
            noisy(&img, 8.0, 0, 5),
            noisy(&img, 8.0, 1, 6),
        ];
        let (_, p) = judge(&mixed.iter().map(|i| an(i, &[])).collect::<Vec<_>>());
        assert!(
            p.rejects.contains(&2) && p.rejects.contains(&3),
            "{name}: {p:?}"
        );
        let high_iso = [
            noisy(&img, 8.0, 1, 11),
            noisy(&shifted(&img, 4), 8.0, 1, 12),
            noisy(&b1, 8.0, 1, 13),
            noisy(&shifted(&img, 8), 8.0, 1, 14),
        ];
        let (s, p) = judge(&high_iso.iter().map(|i| an(i, &[])).collect::<Vec<_>>());
        assert!(p.rejects.contains(&2) && p.best != Some(2), "{name}: {p:?}");
        assert!(
            !s.iter()
                .any(|s| s.reasons.iter().any(|r| matches!(r, Reason::Noisy { .. }))),
            "{name}"
        );
        let lone = score(&an(&noisy(&img, 12.0, 1, 22), &[]), None, &w, &t);
        assert!(
            lone.reasons
                .iter()
                .any(|r| matches!(r, Reason::Noisy { .. })),
            "{name}: {lone:?}"
        );
    }
}

fn h(bits: u32) -> PerceptualHash {
    PerceptualHash {
        dhash: if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        },
        phash: 0,
    }
}

fn it(t: i64, seq: Option<u64>, camera: Option<u64>, bits: u32) -> GroupItem {
    GroupItem {
        capture_ms: Some(t),
        seq,
        camera,
        hash: h(bits),
    }
}

fn sizes(items: &[GroupItem], cfg: &GroupConfig) -> Vec<usize> {
    group_images(items, cfg, None)
        .iter()
        .map(|g| g.members.len())
        .collect()
}

#[test]
fn grouping_round_two() {
    let d = GroupConfig::default();
    let off = GroupConfig {
        near_dup_window_ms: Some(0),
        ..d
    };
    // 60 whole-second frames → one moment; two bodies → one each.
    let items: Vec<_> = (0..60u64)
        .map(|k| it(1000 * (k as i64 / 20), Some(7000 + k), None, 0))
        .collect();
    assert_eq!(sizes(&items, &d), [60]);
    let bodies: Vec<_> = (0..40)
        .map(|k| {
            it(
                1000 * (k / 40),
                Some((k / 2) as u64),
                Some((k % 2) as u64),
                0,
            )
        })
        .collect();
    assert_eq!(sizes(&bodies, &d), [20, 20]);
    // An ARW burst plus one millisecond-stamped phone photo.
    let mut mixed: Vec<_> = (0..40u64)
        .map(|k| it(10_000 + 1000 * (k as i64 / 20), Some(k), None, 0))
        .collect();
    mixed.push(it(500_123, None, None, 40));
    assert_eq!(sizes(&mixed, &off), [40, 1]);
    // A static 15 s burst is several ≤ 3 s moments with defaults too.
    let still: Vec<_> = (0..300)
        .map(|k| it(k * 50, Some(k as u64), None, 0))
        .collect();
    assert_eq!(sizes(&still, &d), [61, 61, 61, 61, 56]);
    // A drifting burst never chains past the anchor limit.
    let drift: Vec<_> = (0..300)
        .map(|k| it(1000 * (k / 20), Some(k as u64), None, (k / 5) as u32))
        .collect();
    for g in group_images(&drift, &d, None) {
        let a = drift[g.members[0]].hash;
        assert!(
            g.members
                .iter()
                .all(|&m| a.distance(&drift[m].hash) <= d.moment_max_hash_distance)
        );
    }
    // Counter wrap inside one second keeps shot order.
    let mut wrap: Vec<_> = (0..10u64)
        .map(|k| it(7000, Some(9990 + k), None, 2 * k as u32))
        .collect();
    wrap.extend((0..10u64).map(|k| it(7000, Some(k), None, 20 + 2 * k as u32)));
    let order: Vec<usize> = group_images(&wrap, &d, None)
        .iter()
        .flat_map(|g| g.members.clone())
        .collect();
    assert_eq!(order, (0..20).collect::<Vec<_>>());
}

#[test]
fn scoring_and_grouping_stay_fast() {
    // 300-frame group scoring, and 20 000 frames sharing (or
    // lacking) a timestamp — grouping was 2.9 s, is now linear.
    if let Some((_, img)) = support::sample_previews().into_iter().next() {
        let base = an(&img, &[]);
        let soft = an(&box_blur(&img, 1, 1), &[]);
        let frames: Vec<_> = (0..300)
            .map(|k| {
                if k % 7 == 0 {
                    soft.clone()
                } else {
                    base.clone()
                }
            })
            .collect();
        let start = std::time::Instant::now();
        let (_, p) = judge(&frames);
        assert!(start.elapsed().as_secs_f32() < 1.0, "{:?}", start.elapsed());
        assert_eq!(p.rejects.len(), 43);
    }
    for t in [Some(1_000), None] {
        let items: Vec<_> = (0..20_000u64)
            .map(|k| GroupItem {
                capture_ms: t,
                seq: None,
                camera: None,
                hash: PerceptualHash {
                    dhash: k.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                    phash: k,
                },
            })
            .collect();
        let start = std::time::Instant::now();
        let g = group_images(&items, &GroupConfig::default(), None);
        assert!(start.elapsed().as_secs_f32() < 1.0, "{:?}", start.elapsed());
        assert_eq!(g.iter().map(|g| g.members.len()).sum::<usize>(), 20_000);
    }
}
