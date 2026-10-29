#![allow(clippy::float_cmp)] // exact values are intended where asserted

use super::*;
use crate::testutil::{frame_from, texture, vnoise};
use crate::{NoProgress, WHITE};
use std::sync::Mutex;

const W: u32 = 480;
const H: u32 = 320;
/// The face (scene coordinates): an ellipse and Vision's box around it.
const FACE: (f32, f32, f32, f32) = (240.0, 150.0, 40.0, 50.0);
const FACE_BOX: (f32, f32, f32, f32) = (202.0, 104.0, 278.0, 196.0);
const EYES: [(f32, f32); 2] = [(224.0, 138.0), (256.0, 138.0)];

#[derive(Clone, Copy, PartialEq)]
enum Eyes {
    Open,
    Closed,
}

/// A textured background with one face whose eyes are open or closed.
fn scene(x: f32, y: f32, eyes: Eyes) -> [f32; 3] {
    let (fx, fy, rx, ry) = FACE;
    if ((x - fx) / rx).powi(2) + ((y - fy) / ry).powi(2) > 1.0 {
        let v = texture(x, y);
        return [v * 0.8, v, v * 1.15];
    }
    let mut v = 0.5 + 0.1 * vnoise(x, y, 9.0, 5);
    for (cx, cy) in EYES {
        let d = ((x - cx) / 9.0).powi(2) + ((y - cy) / 5.0).powi(2);
        if d <= 1.0 {
            v = match eyes {
                Eyes::Open if d < 0.35 => 0.04,
                Eyes::Open => 0.85,
                Eyes::Closed if (y - cy).abs() < 0.8 => 0.15,
                Eyes::Closed => v,
            };
        }
    }
    [v * 1.2, v, v * 0.8]
}

/// The burst frame of a camera shifted by `(dx, dy)`: pixel `(x, y)` sees the scene at
/// `(x + dx, y + dy)`.
fn shot(shift: (f32, f32), seed: u64, f: impl Fn(f32, f32) -> [f32; 3]) -> Frame {
    frame_from(W, H, 1.0, 0.002, seed, |x, y| f(x + shift.0, y + shift.1))
}

/// Vision's face box in a frame shifted by `shift`.
fn face_in(shift: (f32, f32), eyes_open: f32, quality: f32) -> BurstFace {
    let (x0, y0, x1, y1) = FACE_BOX;
    BurstFace {
        bounds: NormRect {
            x: (x0 - shift.0) / W as f32,
            y: (y0 - shift.1) / H as f32,
            w: (x1 - x0) / W as f32,
            h: (y1 - y0) / H as f32,
        },
        eyes_open: Some(eyes_open),
        quality: Some(quality),
    }
}

fn px(f: &Frame, x: u32, y: u32) -> [f32; 3] {
    let i = ((y * f.w + x) * 3) as usize;
    [0, 1, 2].map(|c| f32::from(f.data[i + c]) / WHITE)
}

/// Mean absolute error of `f` against `want` over a box (all channels).
fn mean_error(
    f: &Frame,
    (x0, y0, x1, y1): (u32, u32, u32, u32),
    want: impl Fn(f32, f32) -> [f32; 3],
) -> f32 {
    let (mut sum, mut n) = (0.0, 0);
    for y in y0..y1 {
        for x in x0..x1 {
            let (got, w) = (px(f, x, y), want(x as f32, y as f32));
            for c in 0..3 {
                sum += (got[c] - w[c]).abs();
                n += 1;
            }
        }
    }
    sum / n as f32
}

/// The eye regions (scene = reference coordinates).
fn eye_box(i: usize) -> (u32, u32, u32, u32) {
    let (cx, cy) = EYES[i];
    (
        (cx - 8.0) as u32,
        (cy - 4.0) as u32,
        (cx + 8.0) as u32,
        (cy + 4.0) as u32,
    )
}

fn opts() -> BurstOptions {
    BurstOptions::default()
}

/// Reference (best score) with closed eyes, a frame with open eyes, one more closed.
fn blink_burst() -> (Vec<Frame>, Vec<Vec<BurstFace>>) {
    let shifts = [(0.0, 0.0), (3.4, -2.2), (-2.1, 1.6)];
    let eyes = [Eyes::Closed, Eyes::Open, Eyes::Closed];
    let frames = shifts
        .iter()
        .zip(eyes)
        .enumerate()
        .map(|(i, (&s, e))| shot(s, i as u64, move |x, y| scene(x, y, e)))
        .collect();
    let faces = vec![
        vec![face_in(shifts[0], 0.05, 0.7)],
        vec![face_in(shifts[1], 0.95, 0.6)],
        vec![face_in(shifts[2], 0.1, 0.7)],
    ];
    (frames, faces)
}

fn inputs<'a>(
    frames: &'a [Frame],
    faces: &'a [Vec<BurstFace>],
    scores: &[f32],
) -> Vec<BurstInput<'a>> {
    frames
        .iter()
        .enumerate()
        .map(|(i, f)| BurstInput {
            frame: f,
            faces: faces.get(i).map_or(&[], Vec::as_slice),
            score: scores.get(i).copied(),
        })
        .collect()
}

#[test]
fn closed_eyes_take_the_open_eyes_of_another_frame() {
    let (frames, faces) = blink_burst();
    let c = merge_burst(
        &inputs(&frames, &faces, &[0.9, 0.6, 0.5]),
        &opts(),
        &NoProgress,
    )
    .unwrap();
    assert_eq!(c.reference, 0);
    assert_eq!(c.unaligned, 0);
    let face: Vec<&Patch> = c
        .patches
        .iter()
        .filter(|p| p.kind == PatchKind::Face)
        .collect();
    assert_eq!(face.len(), 1, "{:?}", c.patches);
    assert_eq!(face[0].frame, 1);
    // The patch covers both eyes.
    let b = face[0].bounds;
    for (x, y) in EYES {
        assert!(b.x * W as f32 <= x && x <= (b.x + b.w) * W as f32);
        assert!(b.y * H as f32 <= y && y <= (b.y + b.h) * H as f32);
    }
    for i in 0..2 {
        let open = mean_error(&c.frame, eye_box(i), |x, y| scene(x, y, Eyes::Open));
        let closed = mean_error(&c.frame, eye_box(i), |x, y| scene(x, y, Eyes::Closed));
        // Bilinear resampling of the eyes' hard edges alone costs about 0.08 here.
        assert!(open < 0.09, "eye {i}: {open} from open eyes");
        assert!(closed > 3.0 * open, "eye {i}: {closed} vs {open}");
    }
}

#[test]
fn the_swapped_face_blends_without_a_seam_and_the_rest_stays_the_reference() {
    let (frames, faces) = blink_burst();
    let c = merge_burst(
        &inputs(&frames, &faces, &[0.9, 0.6, 0.5]),
        &opts(),
        &NoProgress,
    )
    .unwrap();
    // Around the eyes (all of the patch, its feathered edge included) the composite is the
    // scene with open eyes, in the reference's geometry.
    let patch = (176, 72, 304, 228);
    let e = mean_error(&c.frame, patch, |x, y| scene(x, y, Eyes::Open));
    assert!(e < 0.012, "patch error {e}");
    // A ring just around the patch's edge: no step to the untouched reference.
    let ring = mean_error(&c.frame, (170, 66, 310, 76), |x, y| scene(x, y, Eyes::Open));
    assert!(ring < 0.012, "edge error {ring}");
    // Outside every patch: the reference, sample for sample.
    for (x, y) in [(5, 5), (470, 10), (20, 300), (400, 290), (100, 160)] {
        let i = ((y * W + x) * 3) as usize;
        assert_eq!(
            c.frame.data[i..i + 3],
            frames[0].data[i..i + 3],
            "({x}, {y})"
        );
    }
}

#[test]
fn open_eyes_are_never_traded_for_closed_ones() {
    let shifts = [(0.0, 0.0), (2.0, 1.0)];
    let frames: Vec<Frame> = shifts
        .iter()
        .enumerate()
        .map(|(i, &s)| shot(s, i as u64, |x, y| scene(x, y, Eyes::Open)))
        .collect();
    // The other face has far better quality but its eyes are less open.
    let faces = vec![
        vec![face_in(shifts[0], 0.9, 0.0)],
        vec![face_in(shifts[1], 0.6, 1.0)],
    ];
    let c = merge_burst(&inputs(&frames, &faces, &[0.9, 0.5]), &opts(), &NoProgress).unwrap();
    assert!(
        c.patches.iter().all(|p| p.kind != PatchKind::Face),
        "{:?}",
        c.patches
    );
    assert_eq!(c.faces_kept, 0);
}

#[test]
fn a_face_whose_surroundings_changed_stays_as_it_is() {
    // In the open-eyes frame the person turned: the head and what is around it differ.
    let frames = vec![
        shot((0.0, 0.0), 0, |x, y| scene(x, y, Eyes::Closed)),
        shot((2.0, -1.0), 1, |x, y| {
            if ((x - FACE.0) / 70.0).powi(2) + ((y - FACE.1) / 85.0).powi(2) <= 1.0 {
                let v = 0.2 + 0.6 * vnoise(x, y, 6.0, 99);
                [v, v, v]
            } else {
                scene(x, y, Eyes::Open)
            }
        }),
    ];
    let faces = vec![
        vec![face_in((0.0, 0.0), 0.05, 0.7)],
        vec![face_in((2.0, -1.0), 0.95, 0.7)],
    ];
    let c = merge_burst(&inputs(&frames, &faces, &[0.9, 0.5]), &opts(), &NoProgress).unwrap();
    assert_eq!(c.faces_kept, 1);
    assert!(c.patches.iter().all(|p| p.kind != PatchKind::Face));
    let e = mean_error(&c.frame, eye_box(0), |x, y| scene(x, y, Eyes::Closed));
    assert!(e < 0.01, "{e}");
}

/// The scene softened by a 7×7 box (a motion-blurred or missed-focus area).
fn soft(x: f32, y: f32) -> [f32; 3] {
    let mut acc = [0.0; 3];
    for dy in -3..=3 {
        for dx in -3..=3 {
            let v = scene(x + dx as f32, y + dy as f32, Eyes::Open);
            for c in 0..3 {
                acc[c] += v[c] / 49.0;
            }
        }
    }
    acc
}

const SOFT: (f32, f32, f32, f32) = (32.0, 32.0, 192.0, 160.0);

fn in_soft(x: f32, y: f32) -> bool {
    x >= SOFT.0 && x < SOFT.2 && y >= SOFT.1 && y < SOFT.3
}

#[test]
fn a_soft_region_takes_the_sharper_frame() {
    let frames = vec![
        shot((0.0, 0.0), 0, |x, y| {
            if in_soft(x, y) {
                soft(x, y)
            } else {
                scene(x, y, Eyes::Open)
            }
        }),
        shot((2.3, 1.4), 1, |x, y| scene(x, y, Eyes::Open)),
    ];
    let c = merge_burst(&inputs(&frames, &[], &[0.9, 0.4]), &opts(), &NoProgress).unwrap();
    assert_eq!(c.reference, 0);
    let regions: Vec<&Patch> = c
        .patches
        .iter()
        .filter(|p| p.kind == PatchKind::Sharper)
        .collect();
    assert_ne!(regions, [] as [&Patch; 0]);
    assert!(regions.iter().all(|p| p.frame == 1));
    // Inside the soft area (away from its edges) the composite is the sharp scene.
    let inner = (64, 64, 160, 128);
    let sharp = mean_error(&c.frame, inner, |x, y| scene(x, y, Eyes::Open));
    let blurred = mean_error(&frames[0], inner, |x, y| scene(x, y, Eyes::Open));
    assert!(sharp < 0.4 * blurred, "{sharp} vs {blurred}");
    // The sharp rest of the reference is untouched.
    for (x, y) in [(400, 250), (300, 40), (460, 300)] {
        let i = ((y * W + x) * 3) as usize;
        assert_eq!(
            c.frame.data[i..i + 3],
            frames[0].data[i..i + 3],
            "({x}, {y})"
        );
    }
}

#[test]
fn content_that_moved_is_never_taken() {
    // The other frame is sharp where the reference is soft, but something else is there.
    let frames = vec![
        shot((0.0, 0.0), 0, |x, y| {
            if in_soft(x, y) {
                soft(x, y)
            } else {
                scene(x, y, Eyes::Open)
            }
        }),
        shot((2.3, 1.4), 1, |x, y| {
            if in_soft(x, y) {
                let v = 0.1 + 0.8 * vnoise(x, y, 5.0, 42);
                [v, v, v]
            } else {
                scene(x, y, Eyes::Open)
            }
        }),
    ];
    let c = merge_burst(&inputs(&frames, &[], &[0.9, 0.4]), &opts(), &NoProgress).unwrap();
    assert!(c.patches.is_empty(), "{:?}", c.patches);
    assert_eq!(c.frame, frames[0]);
}

#[test]
fn a_frame_that_cannot_be_aligned_contributes_nothing() {
    let (mut frames, mut faces) = blink_burst();
    frames.truncate(1);
    faces.truncate(1);
    frames.push(frame_from(W, H, 1.0, 0.0, 7, |_, _| [0.3, 0.3, 0.3]));
    faces.push(vec![face_in((0.0, 0.0), 1.0, 1.0)]);
    let c = merge_burst(&inputs(&frames, &faces, &[0.9, 0.1]), &opts(), &NoProgress).unwrap();
    assert_eq!(c.unaligned, 1);
    assert_eq!(c.patches, [] as [Patch; 0]);
    assert_eq!(c.frame, frames[0]);
}

#[test]
fn with_everything_off_the_reference_comes_back_unchanged() {
    let (frames, faces) = blink_burst();
    let o = BurstOptions {
        reference: Some(2),
        faces: false,
        regions: false,
    };
    let c = merge_burst(&inputs(&frames, &faces, &[]), &o, &NoProgress).unwrap();
    assert_eq!((c.reference, c.unaligned, c.faces_kept), (2, 0, 0));
    assert_eq!(c.patches, [] as [Patch; 0]);
    assert_eq!(c.frame, frames[2]);
}

#[test]
fn the_reference_is_the_best_scored_then_the_sharpest_then_the_open_eyes() {
    let sharp = shot((0.0, 0.0), 0, |x, y| scene(x, y, Eyes::Open));
    let blurred = shot((1.0, 0.0), 1, soft);
    let off = BurstOptions {
        faces: false,
        regions: false,
        ..opts()
    };
    let frames = vec![blurred.clone(), sharp, blurred];
    let scored = merge_burst(&inputs(&frames, &[], &[0.2, 0.3, 0.8]), &off, &NoProgress).unwrap();
    assert_eq!(scored.reference, 2, "every frame scored: the best score");
    let partly = merge_burst(&inputs(&frames, &[], &[0.2, 0.3]), &off, &NoProgress).unwrap();
    assert_eq!(partly.reference, 1, "not every frame scored: the sharpest");

    // Equally sharp: the frame with open eyes.
    let (frames, faces) = blink_burst();
    let c = merge_burst(&inputs(&frames, &faces, &[]), &off, &NoProgress).unwrap();
    assert_eq!(c.reference, 1);
}

fn one(fr: &Frame) -> BurstInput<'_> {
    BurstInput {
        frame: fr,
        faces: &[],
        score: None,
    }
}

#[test]
fn bad_input_is_refused_with_a_clear_error() {
    let f = frame_from(16, 16, 1.0, 0.0, 0, |_, _| [0.5; 3]);
    let g = frame_from(16, 12, 1.0, 0.0, 0, |_, _| [0.5; 3]);
    assert_eq!(
        merge_burst(&[one(&f)], &opts(), &NoProgress),
        Err(MergeError::TooFew)
    );
    assert!(matches!(
        merge_burst(&[one(&f), one(&g)], &opts(), &NoProgress),
        Err(MergeError::Mismatch(_))
    ));
    let o = BurstOptions {
        reference: Some(2),
        ..opts()
    };
    assert!(matches!(
        merge_burst(&[one(&f), one(&f)], &o, &NoProgress),
        Err(MergeError::Invalid(_))
    ));
    let many = vec![one(&f); MAX_BURST_FRAMES + 1];
    assert!(matches!(
        merge_burst(&many, &opts(), &NoProgress),
        Err(MergeError::Invalid(_))
    ));
}

#[test]
fn corrupt_face_boxes_are_ignored() {
    let (frames, mut faces) = blink_burst();
    faces[0].push(BurstFace {
        bounds: NormRect {
            x: f32::NAN,
            y: 0.2,
            w: f32::INFINITY,
            h: -1.0,
        },
        eyes_open: None,
        quality: None,
    });
    let c = merge_burst(
        &inputs(&frames, &faces, &[0.9, 0.6, 0.5]),
        &opts(),
        &NoProgress,
    )
    .unwrap();
    assert_eq!(
        c.patches
            .iter()
            .filter(|p| p.kind == PatchKind::Face)
            .count(),
        1
    );
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

struct Cancel;

impl Progress for Cancel {
    fn set(&self, _: f32) {}

    fn cancelled(&self) -> bool {
        true
    }
}

#[test]
fn progress_grows_to_one_cancel_stops_and_results_repeat() {
    let (frames, faces) = blink_burst();
    let inp = inputs(&frames, &faces, &[0.9, 0.6, 0.5]);
    let log = Log::default();
    let a = merge_burst(&inp, &opts(), &log).unwrap();
    let steps = log.0.into_inner().unwrap();
    assert!(steps.windows(2).all(|w| w[0] <= w[1]), "{steps:?}");
    assert_eq!(steps.last(), Some(&1.0));
    assert_eq!(
        merge_burst(&inp, &opts(), &Cancel),
        Err(MergeError::Cancelled)
    );
    assert_eq!(merge_burst(&inp, &opts(), &NoProgress).unwrap(), a);
}

#[test]
fn helpers_handle_their_edge_cases() {
    assert_eq!(iou((0.0, 0.0, 1.0, 1.0), (2.0, 2.0, 3.0, 3.0)), 0.0);
    assert_eq!(iou((0.0, 0.0, 0.0, 0.0), (0.0, 0.0, 0.0, 0.0)), 0.0);
    assert!((iou((0.0, 0.0, 2.0, 2.0), (1.0, 0.0, 3.0, 2.0)) - 1.0 / 3.0).abs() < 1e-6);
    assert_eq!(face_weight(0.5), 1.0);
    assert_eq!(face_weight(1.0), 0.0);
    assert!((face_weight(0.85) - 0.5).abs() < 1e-6);
    let g = Gray {
        w: 2,
        h: 2,
        px: vec![1.0; 4],
    };
    assert_eq!(detail(&g, (0, 0, 2, 2)), 0.0, "no interior");
    let flat = vec![0.5f32; 5 * 4 * 3];
    assert!(
        box_blur3(&flat, 5, 4, 2)
            .iter()
            .all(|v| (v - 0.5).abs() < 1e-6)
    );
    let grid = Grid::new(400, 200, 2);
    let n = grid.norm(-10.0, 10.0, 300.0, 50.0);
    assert_eq!((n.x, n.w), (0.0, 1.0));
    assert!((n.y - (20.5 / 200.0)).abs() < 1e-6);
}
