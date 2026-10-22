#![allow(clippy::float_cmp)] // exact values are intended where asserted

use super::*;
use crate::geom::{axis_angle, rotation_angle};
use crate::testutil::texture;
use crate::{NoProgress, WHITE};

const W: u32 = 480;
const H: u32 = 320;
const F: f64 = 400.0;

/// The world: a textured sphere of view directions (texture px ≈ frame px at the centre).
fn env(r: [f64; 3]) -> [f32; 3] {
    let n = dot3(r, r).sqrt();
    let (u, v) = (r[0].atan2(r[2]), (r[1] / n).asin());
    let t = texture((u * F) as f32, (v * F) as f32) * 0.5;
    [t * 0.9, t, t * 1.1]
}

/// Camera → world rotation for yaw (right), pitch (down) and roll, in degrees.
fn cam(yaw: f64, pitch: f64, roll: f64) -> Mat3 {
    let y = axis_angle([0.0, 1.0, 0.0], yaw.to_radians());
    let p = axis_angle([1.0, 0.0, 0.0], -pitch.to_radians());
    let r = axis_angle([0.0, 0.0, 1.0], roll.to_radians());
    mul(&y, &mul(&p, &r))
}

fn shoot(r: &Mat3, gain: f32) -> Frame {
    shoot_in(r, gain, env)
}

/// A blown sky above 7° of elevation (clipped in every frame), texture below.
fn sky(r: [f64; 3]) -> [f32; 3] {
    let lat = (r[1] / dot3(r, r).sqrt()).asin();
    if lat < -0.12 { [3.0; 3] } else { env(r) }
}

fn shoot_in(r: &Mat3, gain: f32, world: fn([f64; 3]) -> [f32; 3]) -> Frame {
    let mut data = Vec::with_capacity((W * H * 3) as usize);
    for y in 0..H {
        for x in 0..W {
            let c = world(mul_vec(r, ray(f64::from(x), f64::from(y), W, H, F)));
            data.extend(c.map(|v| ((v * gain).min(1.0) * WHITE).round() as u16));
        }
    }
    Frame::new(W, H, data).unwrap()
}

struct Scene {
    truth: Vec<Mat3>,
    gains: Vec<f32>,
    frames: Vec<Frame>,
}

fn scene(cams: &[(f64, f64, f64)], gains: &[f32]) -> Scene {
    let truth: Vec<Mat3> = cams.iter().map(|&(y, p, r)| cam(y, p, r)).collect();
    let frames = truth.iter().zip(gains).map(|(r, &g)| shoot(r, g)).collect();
    Scene {
        truth,
        gains: gains.to_vec(),
        frames,
    }
}

fn three() -> Scene {
    scene(
        &[(-30.0, 1.0, 0.5), (0.0, 0.0, 0.0), (29.0, -1.5, -1.0)],
        &[1.0, 1.25, 0.8],
    )
}

fn refs(s: &Scene) -> Vec<&Frame> {
    s.frames.iter().collect()
}

fn degrees(a: &Mat3, b: &Mat3) -> f64 {
    rotation_angle(&mul(&transpose(a), b)).to_degrees()
}

struct Cancel;

impl Progress for Cancel {
    fn set(&self, _: f32) {}

    fn cancelled(&self) -> bool {
        true
    }
}

#[test]
fn registration_recovers_focal_rotations_gains_and_horizon() {
    let s = three();
    for start in [None, Some(450.0)] {
        let reg = register(&refs(&s), start, &Stage::new(&NoProgress, 0.0, 1.0)).unwrap();
        assert!(
            (reg.focal / F - 1.0).abs() < 0.02,
            "{start:?}: focal {}",
            reg.focal
        );
        assert_eq!(reg.reference, 1);
        for i in 0..3 {
            for j in 0..3 {
                let est = mul(&transpose(&reg.rot[i]), &reg.rot[j]);
                let tru = mul(&transpose(&s.truth[i]), &s.truth[j]);
                assert!(
                    degrees(&est, &tru) < 0.1,
                    "pair {i}{j}: {}°",
                    degrees(&est, &tru)
                );
            }
            // Gains undo the exposure differences, relative to the reference.
            let k = reg.gain[i] * s.gains[i] / s.gains[1];
            assert!((k - 1.0).abs() < 0.02, "frame {i}: gain {}", reg.gain[i]);
        }
        // Levelled: the world's down in the reference camera matches the truth.
        let down = |r: &Mat3| mul_vec(&transpose(r), [0.0, 1.0, 0.0]);
        let tilt = dot3(down(&reg.rot[1]), down(&s.truth[1]))
            .clamp(-1.0, 1.0)
            .acos();
        assert!(
            tilt.to_degrees() < 2.5, // the cameras roll by up to 1°: the level is a compromise
            "horizon off by {}°",
            tilt.to_degrees()
        );
    }
}

/// Render with a layout and compare every covered output pixel with the scene.
fn errors(s: &Scene, projection: Projection, auto_crop: bool) -> (Vec<f32>, Vec<bool>, Layout) {
    let frames = refs(s);
    let reg = register(&frames, None, &Stage::new(&NoProgress, 0.0, 1.0)).unwrap();
    let opts = PanoOptions {
        projection,
        auto_crop,
        ..PanoOptions::default()
    };
    let layout = Layout::new(&frames, &reg, opts).unwrap();
    let out = render(&frames, &reg, &layout, &Stage::new(&NoProgress, 0.0, 1.0)).unwrap();
    let gmax = reg.gain.iter().copied().fold(1.0f32, f32::max);
    // Our world → the true world, through the reference camera.
    let q = mul(&s.truth[reg.reference], &transpose(&reg.rot[reg.reference]));
    let mut err = Vec::new();
    let mut seam = Vec::new();
    for y in 0..layout.h {
        for x in 0..layout.w {
            let i = (y * layout.w + x) * 3;
            if out.data[i..i + 3].iter().all(|&v| v == 0) {
                continue;
            }
            let r = layout.ray_at(x, y);
            let want = env(mul_vec(&q, r))[1] * s.gains[reg.reference];
            let got = f32::from(out.data[i + 1]) / WHITE * gmax;
            err.push((got - want).abs() / want);
            // The frame whose centre is nearest here and at the next pixel.
            let winner = |x: usize| {
                let r = layout.ray_at(x, y);
                layout
                    .placed
                    .iter()
                    .enumerate()
                    .filter_map(|(k, p)| {
                        p.pixel(r, layout.focal).map(|(a, b)| (k, p.feather(a, b)))
                    })
                    .max_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(k, _)| k)
            };
            seam.push(x + 1 < layout.w && winner(x) != winner(x + 1));
        }
    }
    (err, seam, layout)
}

fn mean(v: impl Iterator<Item = f32>) -> f32 {
    let (s, n) = v.fold((0.0, 0), |a, e| (a.0 + e, a.1 + 1));
    s / n.max(1) as f32
}

#[test]
fn stitches_match_the_scene_without_visible_seams() {
    let s = three();
    for p in [
        Projection::Cylindrical,
        Projection::Spherical,
        Projection::Perspective,
    ] {
        let (err, seam, layout) = errors(&s, p, false);
        assert!(
            layout.w * 10 > 16 * W as usize,
            "{p:?}: {}×{}",
            layout.w,
            layout.h
        );
        let mut sorted = err.clone();
        sorted.sort_by(f32::total_cmp);
        let median = sorted[sorted.len() / 2];
        let all = mean(err.iter().copied());
        assert!(
            median < 0.03 && all < 0.05,
            "{p:?}: median {median}, mean {all}"
        );
        // Seams (where the high band switches frames) are no worse than elsewhere.
        let at_seams = mean(err.iter().zip(&seam).filter(|e| *e.1).map(|e| *e.0));
        assert!(
            seam.iter().filter(|s| **s).count() > 100,
            "{p:?}: seams found"
        );
        assert!(
            at_seams < 1.5 * all + 0.005,
            "{p:?}: seams {at_seams} vs {all}"
        );
    }
}

#[test]
fn auto_crop_keeps_only_covered_pixels() {
    let s = three();
    let ins: Vec<PanoInput<'_>> = s
        .frames
        .iter()
        .map(|f| PanoInput {
            frame: f,
            focal_px: Some(400.0),
        })
        .collect();
    let zeros = |m: &Merged| {
        m.frame
            .data
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| p.iter().all(|&v| v == 0))
            .count()
    };
    let full = merge_panorama(
        &ins,
        &PanoOptions {
            projection: Projection::Auto,
            auto_crop: false,
            ..PanoOptions::default()
        },
        &NoProgress,
    )
    .unwrap();
    let cropped = merge_panorama(&ins, &PanoOptions::default(), &NoProgress).unwrap();
    assert_eq!(full.projection, Projection::Cylindrical);
    assert_eq!((full.reference, full.ghost_regions), (1, 0));
    // Gains bring frames to the reference (×1.25): the darkest needs ×1.5625.
    assert!(
        (full.exposure_ev - 1.5625f32.log2()).abs() < 0.05,
        "{}",
        full.exposure_ev
    );
    let px = |m: &Merged| (m.frame.w * m.frame.h) as usize;
    assert!(
        zeros(&full) * 100 > px(&full),
        "uncovered corners without auto crop"
    );
    assert_eq!(zeros(&cropped), 0);
    assert!(
        px(&cropped) * 2 > px(&full),
        "{} of {}",
        px(&cropped),
        px(&full)
    );
    assert!(cropped.frame.w < full.frame.w || cropped.frame.h < full.frame.h);
}

fn reg_of(rot: Vec<Mat3>) -> Registration {
    let n = rot.len();
    Registration {
        focal: F,
        rot,
        gain: vec![1.0; n],
        reference: 0,
    }
}

#[test]
fn projection_choice_and_limits() {
    let blank = Frame::new(W, H, vec![0; (W * H * 3) as usize]).unwrap();
    let frames = [&blank, &blank, &blank];
    let narrow = reg_of(vec![
        cam(-12.0, 0.0, 0.0),
        cam(0.0, 0.0, 0.0),
        cam(12.0, 0.0, 0.0),
    ]);
    assert_eq!(auto_projection(&frames, &narrow), Projection::Perspective);
    let wide = reg_of(vec![
        cam(-30.0, 0.0, 0.0),
        cam(0.0, 0.0, 0.0),
        cam(30.0, 0.0, 0.0),
    ]);
    assert_eq!(auto_projection(&frames, &wide), Projection::Cylindrical);
    let tall = reg_of(vec![
        cam(0.0, -60.0, 0.0),
        cam(0.0, 0.0, 0.0),
        cam(0.0, 60.0, 0.0),
    ]);
    assert_eq!(auto_projection(&frames, &tall), Projection::Spherical);
    let round = reg_of(vec![
        cam(-90.0, 0.0, 0.0),
        cam(0.0, 0.0, 0.0),
        cam(90.0, 0.0, 0.0),
    ]);
    let persp = PanoOptions {
        projection: Projection::Perspective,
        auto_crop: false,
        ..PanoOptions::default()
    };
    assert!(matches!(
        Layout::new(&frames, &round, persp),
        Err(MergeError::Invalid(m)) if m.contains("perspective")
    ));
    // A cylinder can't hold the pole; the message names the projection asked for.
    let pole = reg_of(vec![cam(0.0, 0.0, 0.0), cam(0.0, 90.0, 0.0)]);
    let cyl_err = PanoOptions {
        projection: Projection::Cylindrical,
        auto_crop: false,
        ..PanoOptions::default()
    };
    assert!(matches!(
        Layout::new(&frames[..2], &pole, cyl_err),
        Err(MergeError::Invalid(m)) if m.contains("cylindrical")
    ));
    // A 360° ring wraps: the whole longitude range, frames on the seam span every column.
    let ring: Vec<Mat3> = (0..8).map(|k| cam(f64::from(k) * 45.0, 0.0, 0.0)).collect();
    let ring_frames = [&blank; 8];
    let cyl = PanoOptions {
        projection: Projection::Cylindrical,
        auto_crop: false,
        ..PanoOptions::default()
    };
    let l = Layout::new(&ring_frames, &reg_of(ring), cyl).unwrap();
    assert!((l.w as f64 / (2.0 * std::f64::consts::PI * F) - 1.0).abs() < 0.01);
    assert!(l.placed.iter().any(|p| p.cols == (0, l.w)));
    // A huge panorama is scaled to the pixel budget.
    let big = Registration {
        focal: 100_000.0,
        ..reg_of(vec![cam(-30.0, 0.0, 0.0), cam(30.0, 0.0, 0.0)])
    };
    let l = Layout::new(&frames[..2], &big, cyl).unwrap();
    assert!(
        l.w as f64 <= MAX_EDGE + 1.0 && l.scale < 100_000.0,
        "{} px wide",
        l.w
    );
}

#[test]
fn failures_and_cancellation() {
    let s = scene(&[(0.0, 0.0, 0.0), (120.0, 0.0, 0.0)], &[1.0, 1.0]);
    let ins: Vec<PanoInput<'_>> = s
        .frames
        .iter()
        .map(|f| PanoInput {
            frame: f,
            focal_px: None,
        })
        .collect();
    assert!(matches!(
        merge_panorama(&ins, &PanoOptions::default(), &NoProgress),
        Err(MergeError::NoOverlap(_))
    ));
    assert_eq!(
        merge_panorama(&ins[..1], &PanoOptions::default(), &NoProgress).unwrap_err(),
        MergeError::TooFew
    );
    assert_eq!(
        merge_panorama(&ins, &PanoOptions::default(), &Cancel).unwrap_err(),
        MergeError::Cancelled
    );
}

#[test]
fn helpers() {
    // A grid with an L-shaped hole: the largest rectangle avoids it.
    let (w, h) = (6, 5);
    let mut g = vec![true; w * h];
    g[0] = false;
    g[1] = false;
    g[w] = false;
    // Columns 2..=5 over every row (20 cells) beats rows 2..=4 (18).
    assert_eq!(largest_rectangle(&g, w, h), Some((2, 0, 5, 4)));
    assert_eq!(largest_rectangle(&[false; 4], 2, 2), None);
    assert_eq!(px_range(0.1, 0.2, 0.0, 100.0, 1000), (9, 22));
    assert_eq!(px_range(-1.0, 50.0, 0.0, 100.0, 1000), (0, 1000));
    for p in [
        Projection::Perspective,
        Projection::Cylindrical,
        Projection::Spherical,
    ] {
        let r = normalize3([0.3, -0.2, 0.9]);
        let (u, v) = to_plane(p, r).unwrap();
        let back = normalize3(from_plane(p, u, v));
        assert!((dot3(back, r) - 1.0).abs() < 1e-12, "{p:?}");
    }
    assert!(to_plane(Projection::Perspective, [0.0, 0.0, -1.0]).is_none());
    assert!(to_plane(Projection::Cylindrical, [0.0, 1.0, 0.0]).is_none());
    // Nearly identical cameras: the horizon comes from the cameras' own down axis.
    let tilted = vec![cam(0.0, 0.0, 10.0), cam(1.0, 0.0, 10.0)];
    let l = level(&tilted, 0);
    let down = mul_vec(&transpose(&l[0]), [0.0, 1.0, 0.0]);
    assert!((down[1] - 1.0).abs() < 1e-3, "{down:?}");
    // No usable edges: every gain is 1.
    let g = Gray {
        w: 4,
        h: 4,
        px: vec![0.5; 16],
    };
    assert_eq!(gains(&[], &[&g, &g], 0, 2), vec![1.0, 1.0]);
    assert_eq!(refine_focal(&[], &[], 123.0, 2.0), 123.0);
}

/// Frames at different gains with a sky clipped in all of them: the stitched sky is at
/// the clip on every channel (a gain below 1 must not turn it into a dim colour).
#[test]
fn clipped_skies_stay_at_the_clip() {
    let cams = [(-30.0, 1.0, 0.5), (0.0, 0.0, 0.0), (29.0, -1.5, -1.0)];
    let gains = [1.0f32, 1.3, 0.85];
    let truth: Vec<Mat3> = cams.iter().map(|&(y, p, r)| cam(y, p, r)).collect();
    let frames: Vec<Frame> = truth
        .iter()
        .zip(gains)
        .map(|(r, g)| shoot_in(r, g, sky))
        .collect();
    let refs: Vec<&Frame> = frames.iter().collect();
    let reg = register(&refs, None, &Stage::new(&NoProgress, 0.0, 1.0)).unwrap();
    // The gains differ, so before the fix most frames' clip landed below the output's.
    let gmax = reg.gain.iter().copied().fold(1.0f32, f32::max);
    assert!(reg.gain.iter().any(|g| g / gmax < 0.9), "{:?}", reg.gain);
    let opts = PanoOptions {
        projection: Projection::Cylindrical,
        auto_crop: false,
        ..PanoOptions::default()
    };
    let layout = Layout::new(&refs, &reg, opts).unwrap();
    let out = render(&refs, &reg, &layout, &Stage::new(&NoProgress, 0.0, 1.0)).unwrap();
    let q = mul(&truth[reg.reference], &transpose(&reg.rot[reg.reference]));
    let mut checked = 0;
    for y in 0..layout.h {
        for x in 0..layout.w {
            let i = (y * layout.w + x) * 3;
            let px = &out.data[i..i + 3];
            let r = mul_vec(&q, layout.ray_at(x, y));
            let lat = (r[1] / dot3(r, r).sqrt()).asin();
            if px.iter().all(|&v| v == 0) || lat > -0.16 {
                continue;
            }
            assert!(
                px.iter().all(|&v| f32::from(v) >= 0.95 * WHITE),
                "({x},{y}) {px:?}"
            );
            checked += 1;
        }
    }
    assert!(checked > 10_000, "{checked} sky pixels");
}

/// An evenly spaced full ring: the mean view is zero, so the reference camera sets the
/// heading (no NaN rotations).
#[test]
fn a_full_ring_faces_the_reference() {
    let ring: Vec<Mat3> = (0..4).map(|k| cam(f64::from(k) * 90.0, 0.0, 0.0)).collect();
    let l = level(&ring, 2);
    assert!(l.iter().flatten().flatten().all(|v| v.is_finite()));
    let fwd = mul_vec(&l[2], [0.0, 0.0, 1.0]);
    assert!((fwd[2] - 1.0).abs() < 1e-9, "{fwd:?}");
}

/// Frames shot in the given yaw order (degrees), 20° steps overlap, 80°+ do not.
fn ordered(yaws: &[f64]) -> Scene {
    let cams: Vec<(f64, f64, f64)> = yaws.iter().map(|&y| (y, 0.0, 0.0)).collect();
    scene(&cams, &vec![1.0; yaws.len()])
}

#[test]
fn neighbours_come_first_and_wrap_around() {
    assert_eq!(neighbour_pairs(2), vec![(0, 1)]);
    assert_eq!(neighbour_pairs(3), vec![(0, 1), (0, 2), (1, 2)]);
    let p = neighbour_pairs(6);
    // Next two in capture order, plus the wrap: (5,0), (5,1) and (4,0).
    assert_eq!(
        p,
        vec![
            (0, 1),
            (0, 2),
            (0, 4),
            (0, 5),
            (1, 2),
            (1, 3),
            (1, 5),
            (2, 3),
            (2, 4),
            (3, 4),
            (3, 5),
            (4, 5)
        ]
    );
    // Linear in the frame count, not quadratic.
    assert_eq!(neighbour_pairs(24).len(), 48);
    let m = |i, j| PairMatch {
        i,
        j,
        px: vec![],
        ana: vec![],
    };
    assert!(connected(&[m(0, 1), m(1, 2)], 3));
    assert!(!connected(&[m(0, 1)], 3));
    assert!(!connected(&[m(0, 2), m(1, 3)], 4));
}

#[test]
fn registration_falls_back_to_all_pairs_for_frames_out_of_order() {
    // Capture-order neighbours here never overlap: {0,2,4} and {1,3,5} are separate
    // islands, joined only by the pair (4,1).
    let s = ordered(&[0.0, 80.0, 20.0, 100.0, 40.0, 120.0]);
    let reg = register(&refs(&s), None, &Stage::new(&NoProgress, 0.0, 1.0)).unwrap();
    for i in 0..6 {
        let est = mul(&transpose(&reg.rot[reg.reference]), &reg.rot[i]);
        let tru = mul(&transpose(&s.truth[reg.reference]), &s.truth[i]);
        assert!(degrees(&est, &tru) < 0.5, "frame {i}");
    }
}

#[test]
fn registration_is_deterministic_and_reports_monotonic_progress() {
    #[derive(Default)]
    struct Seen(Mutex<Vec<f32>>);
    impl Progress for Seen {
        fn set(&self, f: f32) {
            self.0.lock().unwrap().push(f);
        }
        fn cancelled(&self) -> bool {
            false
        }
    }
    let s = ordered(&[0.0, 20.0, 40.0, 60.0]);
    let seen = Seen::default();
    let a = register(&refs(&s), None, &Stage::new(&seen, 0.0, 1.0)).unwrap();
    let b = register(&refs(&s), None, &Stage::new(&NoProgress, 0.0, 1.0)).unwrap();
    assert_eq!(a.rot, b.rot);
    assert_eq!(a.focal, b.focal);
    let v = seen.0.lock().unwrap();
    assert!(v.windows(2).all(|w| w[0] <= w[1]), "{v:?}");
    assert_eq!(v.last(), Some(&1.0));
}

#[test]
fn too_many_frames_are_refused_with_a_clear_error() {
    let tiny = Frame::new(8, 8, vec![1000; 8 * 8 * 3]).unwrap();
    let ins: Vec<PanoInput<'_>> = (0..=MAX_FRAMES)
        .map(|_| PanoInput {
            frame: &tiny,
            focal_px: None,
        })
        .collect();
    let Err(MergeError::Invalid(msg)) = merge_panorama(&ins, &PanoOptions::default(), &NoProgress)
    else {
        panic!("not refused");
    };
    assert!(msg.contains("at most 60") && msg.contains("61"), "{msg}");
}

#[test]
fn extreme_aspect_frames_are_refused_not_panicked_on() {
    // 8 px wide, 10000 tall: the analysis image would be 0 px wide.
    let sliver = Frame::new(8, 10_000, vec![500; 8 * 10_000 * 3]).unwrap();
    let ins = [
        PanoInput {
            frame: &sliver,
            focal_px: None,
        },
        PanoInput {
            frame: &sliver,
            focal_px: None,
        },
    ];
    assert!(matches!(
        merge_panorama(&ins, &PanoOptions::default(), &NoProgress),
        Err(MergeError::Invalid(m)) if m.contains("too elongated")
    ));
}

#[test]
fn tiling_the_columns_changes_nothing() {
    let s = three();
    let frames = refs(&s);
    let reg = register(&frames, None, &Stage::new(&NoProgress, 0.0, 1.0)).unwrap();
    let layout = Layout::new(&frames, &reg, PanoOptions::default()).unwrap();
    let whole = render_tiled(
        &frames,
        &reg,
        &layout,
        &Stage::new(&NoProgress, 0.0, 1.0),
        100_000,
    )
    .unwrap();
    assert!(layout.w > 700, "{}", layout.w);
    for tile in [97, 256] {
        let tiled = render_tiled(
            &frames,
            &reg,
            &layout,
            &Stage::new(&NoProgress, 0.0, 1.0),
            tile,
        )
        .unwrap();
        assert_eq!(tiled, whole, "tile of {tile} columns");
    }
}

#[test]
fn the_pixel_limit_comes_from_the_options() {
    let s = three();
    let frames = refs(&s);
    let reg = register(&frames, None, &Stage::new(&NoProgress, 0.0, 1.0)).unwrap();
    let area = |max_pixels| {
        let opts = PanoOptions {
            max_pixels,
            auto_crop: false,
            ..PanoOptions::default()
        };
        let l = Layout::new(&frames, &reg, opts).unwrap();
        l.w as u64 * l.h as u64
    };
    let full = area(DEFAULT_MAX_PIXELS);
    assert!(full > 200_000, "{full}");
    let capped = area(100_000);
    assert!(capped <= 101_000 && capped > 60_000, "{capped}");
}

#[test]
fn estimate_covers_frames_output_and_scratch_and_respects_the_limit() {
    let sizes = [(6000, 4000); 12];
    let opts = PanoOptions::default();
    let est = estimate_bytes(&sizes, &opts);
    let frames = 12 * 6000 * 4000 * 6;
    assert!(est > frames + 120_000_000 * 6 / 2, "{est}");
    assert!(est < frames + 120_000_000 * 6 + 300_000_000, "{est}");
    // A smaller pixel limit shrinks the output part.
    let small = estimate_bytes(
        &sizes,
        &PanoOptions {
            max_pixels: 10_000_000,
            ..opts
        },
    );
    assert!(small + 600_000_000 < est, "{small} vs {est}");
    // The output never exceeds the frames together.
    assert!(
        estimate_bytes(&[(100, 100); 2], &opts) < 2_000_000 + render_scratch_bytes() + 1_000_000
    );
    assert_eq!(estimate_bytes(&[], &opts), render_scratch_bytes());
}
