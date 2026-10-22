#![allow(clippy::float_cmp)] // exact values are intended where asserted

use super::*;
use crate::denoise::linear_original;
use crate::testutil::{noise, raw_from_fn};

const OT: usize = (TILE * SCALE) as usize;

/// The model double: every output pixel is its input pixel (nearest neighbour, ×2), plus
/// `shift(i)` (tiles that disagree).
fn nearest(i: usize, input: &[u16], out: &mut [u16], shift: &dyn Fn(usize) -> f32) {
    let t = TILE as usize;
    for c in 0..3 {
        for y in 0..OT {
            for x in 0..OT {
                let v = f16::from_bits(input[c * t * t + (y / 2) * t + x / 2]).to_f32();
                out[c * OT * OT + y * OT + x] = f16::from_f32(v + shift(i)).to_bits();
            }
        }
    }
}

/// Run the model double over `order`'s tiles, collecting every row handed out (in order).
fn run_in(job: &UpscaleJob, order: &[usize], shift: &dyn Fn(usize) -> f32) -> Vec<u16> {
    let mut input = vec![0u16; job.tile_len()];
    let mut output = vec![0u16; 3 * OT * OT];
    let mut rows = Vec::new();
    let mut next = 0;
    for &i in order {
        job.read_tile(i, &mut input).unwrap();
        nearest(i, &input, &mut output, shift);
        job.write_tile(i, &output, OT, OT * OT).unwrap();
        while let Some(r) = job.take_rows() {
            assert_eq!(r.y0, next, "rows come out in order");
            assert_eq!(
                r.data.len(),
                r.rows as usize * job.output_size().width as usize * 3
            );
            next += r.rows;
            rows.extend(r.data);
        }
    }
    assert_eq!(job.missing(), 0);
    assert_eq!(job.rows_left(), 0);
    assert!(job.take_rows().is_none());
    rows
}

fn run(job: &UpscaleJob, shift: &dyn Fn(usize) -> f32) -> Vec<u16> {
    run_in(job, &(0..job.tile_count()).collect::<Vec<_>>(), shift)
}

/// A smooth colourful encoded image with fine texture.
fn encoded(w: u32, h: u32) -> Vec<u16> {
    (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            let n = 0.03 * noise(x, y);
            [
                0.2 + 0.6 * x as f32 / w as f32 + n,
                0.3 + 0.4 * y as f32 / h as f32 + n,
                0.5 + n,
            ]
            .map(|v| (v.clamp(0.0, 1.0) * WHITE).round() as u16)
        })
        .collect()
}

#[test]
fn tiles_cover_the_image_and_the_output_is_twice_the_size() {
    let job = UpscaleJob::from_encoded(
        Size::new(600, 400),
        &encoded(600, 400),
        Priority::Interactive,
    )
    .unwrap();
    assert_eq!(job.size(), Size::new(600, 400));
    assert_eq!(job.output_size(), Size::new(1200, 800));
    assert_eq!(job.tile_count(), 3 * 2);
    assert_eq!(job.origin(0), (0, 0));
    assert_eq!(
        job.origin(2),
        (600 - TILE, 0),
        "the last tile ends at the edge"
    );
    assert_eq!(job.origin(5), (600 - TILE, 400 - TILE));
    // An A9 frame: 26 × 18 tiles of 256 px overlapping by at least 24 px.
    assert_eq!(positions(6000, TILE, OVERLAP).len(), 26);
    assert_eq!(positions(4000, TILE, OVERLAP).len(), 18);
    // The output weights sum to one away from three-tile corners.
    for s in job.sx.iter().chain(&job.sy) {
        assert!(*s >= 1.0 - 1e-5, "{s}");
    }
}

#[test]
fn an_identity_double_gives_the_nearest_neighbour_upscale_exactly() {
    // Every tile agrees, so wherever the cross-fades meet, the stitched output is the
    // double's own output: the input, each pixel repeated 2×2 (up to f16 rounding).
    let (w, h) = (700u32, 530u32);
    let src = encoded(w, h);
    let job = UpscaleJob::from_encoded(Size::new(w, h), &src, Priority::Interactive).unwrap();
    let out = run(&job, &|_| 0.0);
    assert_eq!(out.len(), (4 * w * h * 3) as usize);
    let ow = (2 * w) as usize;
    let mut worst = 0u16;
    for y in 0..(2 * h) as usize {
        for x in 0..ow {
            for c in 0..3 {
                let want = src[((y / 2) * w as usize + x / 2) * 3 + c];
                worst = worst.max(out[(y * ow + x) * 3 + c].abs_diff(want));
            }
        }
    }
    // f16 keeps 11 bits: ≤ 1/2048 of full scale, about 32 of 65535.
    assert!(worst <= 32, "worst difference {worst}");
}

#[test]
fn tile_seams_never_show_even_when_tiles_disagree() {
    // Each tile is brightened by a different amount (a model whose output depends on the
    // whole tile). The cross-fades turn the differences into gradients over the overlaps.
    let (w, h) = (640u32, 600u32);
    let flat = vec![(0.4 * WHITE) as u16; (w * h * 3) as usize];
    let job = UpscaleJob::from_encoded(Size::new(w, h), &flat, Priority::Interactive).unwrap();
    let out = run(&job, &|i| 0.03 * (i % 3) as f32);
    let ow = (2 * w) as usize;
    let g = |x: usize, y: usize| f32::from(out[(y * ow + x) * 3 + 1]);
    let mut step = 0.0f32;
    for y in (0..(2 * h) as usize).step_by(5) {
        for x in 0..ow - 1 {
            step = step.max((g(x + 1, y) - g(x, y)).abs());
        }
    }
    for x in (0..ow).step_by(5) {
        for y in 0..(2 * h) as usize - 1 {
            step = step.max((g(x, y + 1) - g(x, y)).abs());
        }
    }
    // A 0.06 jump spread over at least 2·(OVERLAP − 2·MARGIN) = 32 output pixels.
    let jump = 0.06 * WHITE;
    assert!(
        step <= jump / 32.0 * 1.1 + 1.0,
        "step {step} for a jump of {jump}"
    );
    assert!(
        step > 0.0,
        "the overlaps carry the disagreement as a gradient"
    );
}

#[test]
fn rows_stream_out_as_tile_rows_complete_and_memory_stays_bounded() {
    let (w, h) = (900u32, 1000u32);
    let job =
        UpscaleJob::from_encoded(Size::new(w, h), &encoded(w, h), Priority::Interactive).unwrap();
    let cols = 4;
    assert_eq!(job.tile_count(), cols * 5);
    let mut input = vec![0u16; job.tile_len()];
    let mut output = vec![0u16; 3 * OT * OT];
    let mut write = |i: usize| {
        job.read_tile(i, &mut input).unwrap();
        nearest(i, &input, &mut output, &|_| 0.0);
        job.write_tile(i, &output, OT, OT * OT).unwrap();
    };
    // Nothing is final until the whole first tile row is in.
    for i in 0..cols - 1 {
        write(i);
        assert!(job.take_rows().is_none());
    }
    write(cols - 1);
    let first = job.take_rows().unwrap();
    assert_eq!(first.y0, 0);
    // Rows above the second tile row's top are final; the overlap waits for it.
    assert_eq!(first.rows, job.origin(cols).1 * SCALE);
    assert!(job.take_rows().is_none());
    // While tile row k is written, at most the rows from the last hand-out down to the
    // bottom of row k are held: about two tiles' height, never the image.
    for i in cols..job.tile_count() {
        write(i);
        let held = job.band.lock().unwrap().acc.len() / (2 * w as usize * 3);
        assert!(held <= 2 * OT, "{held} rows held");
        let _ = job.take_rows();
    }
    assert_eq!(job.rows_left(), 0);
}

#[test]
fn any_tile_order_gives_the_same_rows() {
    let (w, h) = (620u32, 560u32);
    let src = encoded(w, h);
    let a = UpscaleJob::from_encoded(Size::new(w, h), &src, Priority::Interactive).unwrap();
    let b = UpscaleJob::from_encoded(Size::new(w, h), &src, Priority::Interactive).unwrap();
    let n = a.tile_count();
    let shift = |i: usize| 0.01 * i as f32;
    let forward = run(&a, &shift);
    let backward = run_in(&b, &(0..n).rev().collect::<Vec<_>>(), &shift);
    assert_eq!(forward.len(), backward.len());
    // Equal up to the order of the float additions.
    let worst = forward
        .iter()
        .zip(&backward)
        .map(|(a, b)| a.abs_diff(*b))
        .max();
    assert!(worst <= Some(1), "{worst:?}");
}

#[test]
fn a_raw_upscales_to_its_linear_image_and_clipped_pixels_keep_the_original() {
    // Left: a bright patch past the clip after white balance. A model that returns
    // black must not darken it (the model never saw how bright it is).
    let raw = raw_from_fn(300, 280, |x, y| {
        if x < 100 {
            [1.4, 1.2, 1.3]
        } else {
            [0.1 + 0.002 * x as f32, 0.2 + 0.001 * y as f32, 0.3]
        }
    });
    let lin = linear_original(&raw, Priority::Interactive).unwrap();
    let w = 300usize;
    // With the identity double, every pixel is its original linear value (nearest).
    let job = UpscaleJob::from_raw(&raw, Priority::Interactive).unwrap();
    assert_eq!(job.output_size(), Size::new(600, 560));
    let out = run(&job, &|_| 0.0);
    for (x, y) in [(250usize, 10usize), (400, 300), (599, 559), (300, 0)] {
        for c in 0..3 {
            let got = f32::from(out[(y * 600 + x) * 3 + c]);
            let want = f32::from(lin.data[((y / 2) * w + x / 2) * 3 + c]);
            assert!(
                (got - want).abs() <= want * 2e-3 + 2.0,
                "({x},{y}) c{c}: {got} vs {want}"
            );
        }
    }
    // With a model that returns black, the clipped patch is untouched.
    let job = UpscaleJob::from_raw(&raw, Priority::Interactive).unwrap();
    let out = run(&job, &|_| -2.0);
    for (x, y) in [(20usize, 20usize), (100, 300), (180, 550)] {
        for c in 0..3 {
            let got = f32::from(out[(y * 600 + x) * 3 + c]);
            let want = f32::from(lin.data[((y / 2) * w + x / 2) * 3 + c]);
            assert!((got - want).abs() <= want * 2e-3 + 2.0, "({x},{y}) c{c}");
        }
    }
    // ... and the rest follows the model.
    assert_eq!(out[(300 * 600 + 500) * 3 + 1], 0);
}

#[test]
fn images_smaller_than_a_tile_are_mirrored_into_it() {
    let (w, h) = (100u32, 60u32);
    let src = encoded(w, h);
    let job = UpscaleJob::from_encoded(Size::new(w, h), &src, Priority::Interactive).unwrap();
    assert_eq!(job.tile_count(), 1);
    let mut buf = vec![0u16; job.tile_len()];
    job.read_tile(0, &mut buf).unwrap();
    let t = TILE as usize;
    assert_eq!(buf[5 * t + 100], buf[5 * t + 98]);
    assert_eq!(buf[60 * t + 7], buf[58 * t + 7]);
    // (Every tile was read: a fresh job for the run.)
    let job = UpscaleJob::from_encoded(Size::new(w, h), &src, Priority::Interactive).unwrap();
    let out = run(&job, &|_| 0.0);
    assert_eq!(out.len(), (4 * w * h * 3) as usize);
    assert!(
        out[..6]
            .iter()
            .zip(&src[..3].repeat(2))
            .all(|(a, b)| a.abs_diff(*b) <= 32)
    );
}

#[test]
fn tile_calls_reject_bad_arguments() {
    let (w, h) = (600u32, 520u32);
    let job =
        UpscaleJob::from_encoded(Size::new(w, h), &encoded(w, h), Priority::Interactive).unwrap();
    let n = job.tile_count();
    let mut buf = vec![0u16; job.tile_len()];
    let out = vec![0u16; 3 * OT * OT];
    assert!(job.read_tile(n, &mut buf).is_err(), "index past the grid");
    assert!(job.read_tile(0, &mut buf[..10]).is_err(), "short buffer");
    assert!(job.write_tile(n, &out, OT, OT * OT).is_err());
    assert!(
        job.write_tile(0, &out, OT - 1, OT * OT).is_err(),
        "row stride"
    );
    assert!(
        job.write_tile(0, &out, OT, OT * OT - 1).is_err(),
        "channel stride"
    );
    assert!(
        job.write_tile(0, &out[..100], OT, OT * OT).is_err(),
        "short output"
    );
    // Padded rows (Core ML may align them) are read through the strides.
    let (rs, cs) = (OT + 8, (OT + 8) * OT);
    job.write_tile(0, &vec![0u16; 3 * cs], rs, cs).unwrap();
    assert!(
        job.write_tile(0, &out, OT, OT * OT).is_err(),
        "written twice"
    );
    assert_eq!(job.missing(), n - 1);

    let err = |r: Result<UpscaleJob, PipelineError>| match r {
        Err(PipelineError::InvalidRequest(m)) => m,
        other => panic!("{other:?}"),
    };
    assert!(
        err(UpscaleJob::from_encoded(
            Size::new(4, 4),
            &[0; 5],
            Priority::Interactive
        ))
        .contains("samples")
    );
    assert!(
        err(UpscaleJob::from_encoded(
            Size::new(0, 4),
            &[],
            Priority::Interactive
        ))
        .contains("empty")
    );
    // 65 MP doubled is past the limit (checked before anything is allocated).
    let big = Size::new(9_900, 6_600);
    assert!(err(UpscaleJob::from_encoded(big, &[], Priority::Interactive)).contains("limit"));
    let mut bad = raw_from_fn(8, 8, |_, _| [0.5; 3]);
    bad.data.truncate(3);
    assert!(UpscaleJob::from_raw(&bad, Priority::Interactive).is_err());
}

#[test]
fn an_exports_input_is_released_once_every_tile_was_read() {
    let (w, h) = (600u32, 400u32);
    let job =
        UpscaleJob::from_encoded(Size::new(w, h), &encoded(w, h), Priority::Interactive).unwrap();
    assert_eq!(job.input_bytes(), (w * h * 3 * 2) as usize);
    let mut buf = vec![0u16; job.tile_len()];
    // Reading a tile twice counts once.
    job.read_tile(0, &mut buf).unwrap();
    job.read_tile(0, &mut buf).unwrap();
    for i in 1..job.tile_count() - 1 {
        job.read_tile(i, &mut buf).unwrap();
        assert!(job.input_bytes() > 0);
    }
    job.read_tile(job.tile_count() - 1, &mut buf).unwrap();
    assert_eq!(job.input_bytes(), 0, "released after the last read");
    assert!(job.read_tile(0, &mut buf).is_err(), "nothing left to read");
    // The rows still come out (they don't need the input).
    let mut out = vec![0u16; 3 * OT * OT];
    for i in 0..job.tile_count() {
        nearest(i, &buf, &mut out, &|_| 0.0);
        job.write_tile(i, &out, OT, OT * OT).unwrap();
    }
    let mut rows = 0;
    while let Some(r) = job.take_rows() {
        rows += r.rows;
    }
    assert_eq!(rows, 2 * h);
    // A RAW's input stays: its rows fall back to it for clipped pixels.
    let raw = raw_from_fn(300, 260, |_, _| [0.3; 3]);
    let job = UpscaleJob::from_raw(&raw, Priority::Interactive).unwrap();
    for i in 0..job.tile_count() {
        job.read_tile(i, &mut buf).unwrap();
    }
    assert!(job.input_bytes() > 0);
}
