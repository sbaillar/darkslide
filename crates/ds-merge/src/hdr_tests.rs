#![allow(clippy::float_cmp)] // exact values are intended where asserted

use super::*;
use crate::testutil::{frame_from, texture, vnoise};
use crate::{NoProgress, WHITE};
use std::sync::Mutex;

const W: u32 = 320;
const H: u32 = 240;
const EXPOSURES: [f32; 3] = [1.0 / 16.0, 1.0, 16.0];

/// Scene radiance spanning ~13 stops (reference-exposure units: 1 = the clip at e = 1).
fn radiance(x: f32, y: f32) -> [f32; 3] {
    let base = (12.0 * vnoise(x, y, 70.0, 7) - 8.5).exp2() * texture(x, y) * 1.6;
    [base * 0.8, base, base * 1.15]
}

fn brackets(shift: [(f32, f32); 3]) -> Vec<Frame> {
    EXPOSURES
        .iter()
        .zip(shift)
        .enumerate()
        .map(|(i, (&e, (dx, dy)))| {
            frame_from(W, H, e, 0.002, i as u64, |x, y| radiance(x + dx, y + dy))
        })
        .collect()
}

fn inputs(frames: &[Frame], exif: Option<[f32; 3]>) -> Vec<HdrInput<'_>> {
    frames
        .iter()
        .enumerate()
        .map(|(i, f)| HdrInput {
            frame: f,
            exposure: exif.map(|e| e[i]),
        })
        .collect()
}

/// Merged radiance at `(x, y)` in reference units.
fn rad(m: &Merged, x: u32, y: u32) -> [f32; 3] {
    let i = ((y * m.frame.w + x) * 3) as usize;
    let k = m.exposure_ev.exp2();
    [0, 1, 2].map(|c| f32::from(m.frame.data[i + c]) / WHITE * k)
}

/// Worst relative error against the true radiance over measurable pixels (inside a
/// `border`), and how many were checked. Errors within 3 output LSB (16-bit quantization
/// of the result) count as none.
fn worst_error(m: &Merged, border: u32) -> (f32, usize) {
    let lsb = 3.0 * m.exposure_ev.exp2() / WHITE;
    let (mut worst, mut n) = (0.0f32, 0);
    for y in border..H - border {
        for x in border..W - border {
            let want = radiance(x as f32, y as f32);
            // The brightest frame measures down to ~0.002 / 16 of the reference clip.
            if want[0] < 0.004 || want[2] > 12.0 {
                continue;
            }
            let got = rad(m, x, y);
            for c in 0..3 {
                let e = ((got[c] - want[c]).abs() - lsb).max(0.0) / want[c];
                worst = worst.max(e);
            }
            n += 1;
        }
    }
    (worst, n)
}

struct Cancel;

impl Progress for Cancel {
    fn set(&self, _: f32) {}

    fn cancelled(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct Log(Mutex<Vec<f32>>);

impl Progress for Log {
    fn set(&self, f: f32) {
        self.0.lock().unwrap().push(f);
    }

    fn cancelled(&self) -> bool {
        false
    }
}

#[test]
fn radiance_is_recovered_across_the_whole_range() {
    let frames = brackets([(0.0, 0.0); 3]);
    // The scene really needs all three frames.
    let (lo, hi) = (0..H * W).fold((f32::MAX, 0.0f32), |(lo, hi), i| {
        let r = radiance((i % W) as f32, (i / W) as f32)[1];
        (lo.min(r), hi.max(r))
    });
    assert!(lo < 0.02 && hi > 2.0, "scene spans {lo}..{hi}");
    for exif in [Some(EXPOSURES), None, Some([0.07, 1.0, 13.0])] {
        let m = merge_hdr(&inputs(&frames, exif), &HdrOptions::default(), &NoProgress).unwrap();
        assert_eq!(m.reference, 1);
        assert!(
            (m.exposure_ev - 4.0).abs() < 0.03,
            "{exif:?}: {} EV",
            m.exposure_ev
        );
        assert_eq!(
            (m.ghost_regions, m.projection),
            (0, Projection::Perspective)
        );
        let (worst, n) = worst_error(&m, 0);
        assert!(n > 50_000, "{n} pixels checked");
        assert!(worst < 0.04, "{exif:?}: worst error {worst}");
    }
    // Input order does not matter.
    let rev: Vec<Frame> = frames.iter().rev().cloned().collect();
    let m = merge_hdr(&inputs(&rev, None), &HdrOptions::default(), &NoProgress).unwrap();
    assert_eq!(m.reference, 1);
    assert!(worst_error(&m, 0).0 < 0.04);
}

#[test]
fn shifted_brackets_are_aligned() {
    let shift = [(3.4, -2.1), (0.0, 0.0), (-2.6, 1.7)];
    let frames = brackets(shift);
    // The homographies: reference pixel (x, y) is at (x − dx, y − dy) in frame i.
    assert_eq!(factor_for(W, H, ANALYSIS_EDGE), 1);
    let reference = luma_image(&frames[1], 1);
    for (i, ratio) in [(0usize, 1.0 / 16.0), (2, 16.0)] {
        let h = align_pair(&reference, &luma_image(&frames[i], 1), ratio).unwrap();
        let (dx, dy) = shift[i];
        let mut worst = 0.0f64;
        for y in (8..H - 8).step_by(16) {
            for x in (8..W - 8).step_by(16) {
                let (x, y) = (f64::from(x), f64::from(y));
                let (u, v) = project(&h, x, y).unwrap();
                worst = worst.max((u - (x - f64::from(dx))).hypot(v - (y - f64::from(dy))));
            }
        }
        assert!(worst < 0.25, "frame {i}: {worst} px");
    }
    let aligned = merge_hdr(
        &inputs(&frames, Some(EXPOSURES)),
        &HdrOptions::default(),
        &NoProgress,
    )
    .unwrap();
    let (worst, n) = worst_error(&aligned, 8);
    assert!(n > 40_000);
    // Resampling a texture with 4 px features at sub-pixel offsets costs a few percent.
    assert!(worst < 0.12, "aligned worst error {worst}");
    let opts = HdrOptions {
        align: false,
        deghost: Deghost::Off,
    };
    let unaligned = merge_hdr(&inputs(&frames, Some(EXPOSURES)), &opts, &NoProgress).unwrap();
    assert!(
        worst_error(&unaligned, 8).0 > 0.3,
        "misaligned frames should smear"
    );
}

/// An object only the reference frame saw: blended without deghosting, one exposure
/// with it.
#[test]
fn ghosts_take_a_single_exposure() {
    let (cx, cy, r) = (200.0f32, 110.0f32, 22.0f32);
    let object = |x: f32, y: f32| (x - cx).abs() < r && (y - cy).abs() < r;
    // Dark enough that the +2 EV frame never clips: deghosting picks it.
    let scene = |x: f32, y: f32| [0.05 + 0.15 * texture(x, y); 3];
    let frames: Vec<Frame> = [0.25f32, 1.0, 4.0]
        .iter()
        .enumerate()
        .map(|(i, &e)| {
            frame_from(W, H, e, 0.002, i as u64, |x, y| {
                if i == 1 && object(x, y) {
                    [0.6; 3]
                } else {
                    scene(x, y)
                }
            })
        })
        .collect();
    let ins = inputs(&frames, Some([0.25, 1.0, 4.0]));
    let off = merge_hdr(
        &ins,
        &HdrOptions {
            align: false,
            deghost: Deghost::Off,
        },
        &NoProgress,
    )
    .unwrap();
    let on = merge_hdr(
        &ins,
        &HdrOptions {
            align: false,
            deghost: Deghost::High,
        },
        &NoProgress,
    )
    .unwrap();
    assert_eq!(off.ghost_regions, 0);
    assert_eq!(on.ghost_regions, 1);

    // Inside the object: Off is a mixture of object and background; High is one of them.
    let mut mixed = 0;
    let mut checked = 0;
    for y in (cy - r + 6.0) as u32..(cy + r - 6.0) as u32 {
        for x in (cx - r + 6.0) as u32..(cx + r - 6.0) as u32 {
            let bg = scene(x as f32, y as f32)[1];
            let near = |v: f32, want: f32| (v - want).abs() / want < 0.04;
            let g_on = rad(&on, x, y)[1];
            assert!(
                near(g_on, bg) || near(g_on, 0.6),
                "({x},{y}) {g_on} vs {bg} / 0.6"
            );
            let g_off = rad(&off, x, y)[1];
            if !near(g_off, bg) && !near(g_off, 0.6) {
                mixed += 1;
            }
            checked += 1;
        }
    }
    assert!(
        mixed * 2 > checked,
        "{mixed} of {checked} blended without deghosting"
    );
    // The whole region took the same exposure (the +2 EV frame: background).
    let centre = rad(&on, cx as u32, cy as u32)[1];
    for (x, y) in [(cx - 10.0, cy - 10.0), (cx + 10.0, cy + 8.0)] {
        let v = rad(&on, x as u32, y as u32)[1];
        let bg = scene(x, y)[1];
        assert_eq!(
            (v - bg).abs() / bg < 0.04,
            (centre - scene(cx, cy)[1]).abs() / scene(cx, cy)[1] < 0.04
        );
    }
    // Far from the object deghosting changes nothing.
    for (x, y) in [(40, 40), (100, 200), (300, 20)] {
        let (a, b) = (rad(&on, x, y), rad(&off, x, y));
        assert!(
            (0..3).all(|c| (a[c] - b[c]).abs() <= 0.002 * b[c].max(0.01)),
            "{a:?} vs {b:?}"
        );
    }
}

#[test]
fn errors_cancellation_and_progress() {
    let frames = brackets([(0.0, 0.0); 3]);
    assert_eq!(
        merge_hdr(
            &inputs(&frames[..1], None),
            &HdrOptions::default(),
            &NoProgress
        ),
        Err(MergeError::TooFew)
    );
    let small = frames[0].downsampled(2);
    let mixed = [
        HdrInput {
            frame: &frames[0],
            exposure: None,
        },
        HdrInput {
            frame: &small,
            exposure: None,
        },
    ];
    assert!(matches!(
        merge_hdr(&mixed, &HdrOptions::default(), &NoProgress),
        Err(MergeError::Mismatch(_))
    ));

    // A frame that would shrink to nothing at the analysis size is refused, not a panic.
    let sliver = Frame::new(8, 10_000, vec![500; 8 * 10_000 * 3]).unwrap();
    let two = [
        HdrInput {
            frame: &sliver,
            exposure: None,
        },
        HdrInput {
            frame: &sliver,
            exposure: None,
        },
    ];
    assert!(matches!(
        merge_hdr(&two, &HdrOptions::default(), &NoProgress),
        Err(MergeError::Invalid(m)) if m.contains("too elongated")
    ));

    assert_eq!(
        merge_hdr(&inputs(&frames, None), &HdrOptions::default(), &Cancel),
        Err(MergeError::Cancelled)
    );

    let log = Log::default();
    merge_hdr(&inputs(&frames, None), &HdrOptions::default(), &log).unwrap();
    let seen = log.0.into_inner().unwrap();
    assert_eq!(seen.last(), Some(&1.0));
    assert!(seen.iter().all(|f| (0.0..=1.0).contains(f)));
    // Rows finish in any order, but the reported fraction never goes back.
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
}

#[test]
fn helpers() {
    assert_eq!(taper(0.5), 1.0);
    assert_eq!(taper(0.96), 0.0);
    assert!((taper(0.875) - 0.5).abs() < 1e-6);
    for x in 0..50 {
        let d = dither(x, x * 7, x % 3);
        assert!((-1.0..1.0).contains(&d));
    }
    assert_eq!(Deghost::Off.threshold(), None);
    assert!(Deghost::Low.threshold() > Deghost::Medium.threshold());
    assert!(Deghost::Medium.threshold() > Deghost::High.threshold());
    let all_clipped = Gray {
        w: 4,
        h: 4,
        px: vec![1.0; 16],
    };
    assert_eq!(mean_unclipped(&all_clipped), 1.0);
    assert_eq!(measured_ratio(&all_clipped, &all_clipped, None), None);

    // Opening removes a lone mark, keeps a block; components count them.
    let (w, h) = (12, 10);
    let mut m = vec![false; w * h];
    m[w + 1] = true;
    for y in 4..8 {
        for x in 5..9 {
            m[y * w + x] = true;
        }
    }
    let opened = dilate(&erode(&m, w, h, 1), w, h, 1);
    assert!(!opened[w + 1]);
    assert!(opened[5 * w + 6]);
    let (_, n) = components(&m, w, h);
    assert_eq!(n, 2);
    let (labels, n) = components(&opened, w, h);
    assert_eq!(n, 1);
    assert_eq!(labels[0], usize::MAX);
    let (up, down) = upscale(4);
    assert_eq!(project(&up, 0.0, 0.0), Some((1.5, 1.5)));
    let (x, y) = project(&down, 1.5, 1.5).unwrap();
    assert!(x.abs() < 1e-12 && y.abs() < 1e-12);
}

/// A hand-held 5-frame bracket 3 EV apart (±6 EV): the outer frames share no
/// well-exposed pixels with the middle one, so they are aligned and measured through
/// their neighbours.
#[test]
fn wide_brackets_align_through_neighbours() {
    let exposures = [1.0f32 / 64.0, 1.0 / 8.0, 1.0, 8.0, 64.0];
    let shifts = [
        (2.6f32, -1.9f32),
        (-1.4, 2.2),
        (0.0, 0.0),
        (1.8, 1.1),
        (-2.3, -2.7),
    ];
    // ~17 stops of scene, in reference units.
    let wide = |x: f32, y: f32| {
        let b = (16.0 * vnoise(x, y, 90.0, 9) - 11.0).exp2() * texture(x, y) * 1.6;
        [b * 0.8, b, b * 1.15]
    };
    let frames: Vec<Frame> = exposures
        .iter()
        .zip(shifts)
        .enumerate()
        .map(|(i, (&e, (dx, dy)))| {
            frame_from(W, H, e, 0.002, i as u64, |x, y| wide(x + dx, y + dy))
        })
        .collect();
    let ins: Vec<HdrInput<'_>> = frames
        .iter()
        .zip(exposures)
        .map(|(f, e)| HdrInput {
            frame: f,
            exposure: Some(e),
        })
        .collect();
    let m = merge_hdr(&ins, &HdrOptions::default(), &NoProgress).unwrap();
    assert_eq!((m.reference, m.unaligned), (2, 0));
    assert!((m.exposure_ev - 6.0).abs() < 0.05, "{} EV", m.exposure_ev);
    let lsb = 3.0 * m.exposure_ev.exp2() / WHITE;
    let (mut worst, mut n) = (0.0f32, 0);
    for y in 12..H - 12 {
        for x in 12..W - 12 {
            let want = wide(x as f32, y as f32);
            if want[0] < 0.004 || want[2] > 50.0 {
                continue;
            }
            let got = rad(&m, x, y);
            for c in 0..3 {
                worst = worst.max(((got[c] - want[c]).abs() - lsb).max(0.0) / want[c]);
            }
            n += 1;
        }
    }
    assert!(n > 30_000, "{n} pixels");
    assert!(worst < 0.12, "worst error {worst}");
}

/// Without EXIF the brightest frame, mostly clipped, can look darker than the middle
/// one; the measured ratios still put the reference on the middle exposure.
#[test]
fn reference_is_the_middle_exposure_without_exif() {
    // Mostly bright (clipped at +2 EV) with darker patches every frame measures.
    let bright = |x: f32, y: f32| {
        let v = if vnoise(x, y, 40.0, 5) < 0.3 {
            0.06 + 0.1 * texture(x, y)
        } else {
            0.5 + 0.4 * texture(x, y)
        };
        [v; 3]
    };
    let frames: Vec<Frame> = [0.25f32, 1.0, 4.0]
        .iter()
        .enumerate()
        .map(|(i, &e)| frame_from(W, H, e, 0.002, i as u64, bright))
        .collect();
    let lumas: Vec<Gray> = frames.iter().map(|f| luma_image(f, 1)).collect();
    assert!(
        mean_unclipped(&lumas[2]) < mean_unclipped(&lumas[1]),
        "the trap is set"
    );
    let m = merge_hdr(&inputs(&frames, None), &HdrOptions::default(), &NoProgress).unwrap();
    assert_eq!(m.reference, 1);
    assert!((m.exposure_ev - 2.0).abs() < 0.05, "{} EV", m.exposure_ev);
}

#[test]
fn single_exposure_never_pastes_a_clipped_sample() {
    let merged = [0.4, 0.5, 0.6];
    assert_eq!(blend_single(merged, [0.95, 0.5, 0.5], 1.0, 1.0), merged);
    let got = blend_single(merged, [0.2, 0.3, 0.4], 2.0, 0.5);
    assert!((got[0] - 0.4).abs() < 1e-6 && (got[2] - 0.7).abs() < 1e-6);
}
