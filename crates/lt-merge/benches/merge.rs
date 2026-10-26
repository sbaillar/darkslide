//! Merge timings on A9-sized frames (6000×4000), synthetic so they run without samples:
//! an aligned, deghosted HDR merge of three brackets and a three-frame panorama.

use criterion::{Criterion, criterion_group, criterion_main};
use lt_merge::{
    Deghost, Frame, HdrInput, HdrOptions, NoProgress, PanoInput, PanoOptions, merge_hdr,
    merge_panorama,
};
use rayon::prelude::*;

const W: u32 = 6000;
const H: u32 = 4000;
/// Frames of the scaling bench (6 MP, so 24 of them stay under 1 GB).
const SW: u32 = 3000;
const SH: u32 = 2000;

fn hash(i: i64, j: i64) -> f32 {
    let mut h = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (j as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
    h ^= h >> 31;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (h >> 40) as f32 / (1u64 << 24) as f32
}

fn noise(x: f32, y: f32, cell: f32) -> f32 {
    let (u, v) = (x / cell, y / cell);
    let (i, j) = (u.floor(), v.floor());
    let s = |t: f32| t * t * (3.0 - 2.0 * t);
    let (fx, fy) = (s(u - i), s(v - j));
    let (i, j) = (i as i64, j as i64);
    let top = hash(i, j) + (hash(i + 1, j) - hash(i, j)) * fx;
    let bottom = hash(i, j + 1) + (hash(i + 1, j + 1) - hash(i, j + 1)) * fx;
    top + (bottom - top) * fy
}

fn frame(f: impl Fn(f32, f32) -> f32 + Sync) -> Frame {
    sized_frame(W, H, f)
}

fn sized_frame(w: u32, h: u32, f: impl Fn(f32, f32) -> f32 + Sync) -> Frame {
    let mut data = vec![0u16; (w * h * 3) as usize];
    data.par_chunks_mut(w as usize * 3)
        .enumerate()
        .for_each(|(y, row)| {
            for x in 0..w as usize {
                let v = f(x as f32, y as f32);
                for (k, c) in [0.8f32, 1.0, 1.2].iter().enumerate() {
                    row[x * 3 + k] = ((v * c).clamp(0.0, 1.0) * 65535.0) as u16;
                }
            }
        });
    Frame::new(w, h, data).unwrap()
}

/// `n` frames panned 12° apart (about 78% overlap with the next), 6 MP each.
fn sweep(n: usize) -> Vec<Frame> {
    let f = SW as f32 * 35.0 / 36.0;
    (0..n)
        .map(|k| {
            let (s, cy) = (k as f32 * 12.0).to_radians().sin_cos();
            sized_frame(SW, SH, move |x, y| {
                let (rx, ry) = (
                    (x - (SW - 1) as f32 / 2.0) / f,
                    (y - (SH - 1) as f32 / 2.0) / f,
                );
                let (wx, wz) = (cy * rx + s, -s * rx + cy);
                let lon = wx.atan2(wz);
                let lat = ry.atan2(wx.hypot(wz));
                0.1 + 0.5 * noise(lon * f, lat * f, 14.0)
            })
        })
        .collect()
}

fn bench(c: &mut Criterion) {
    let radiance =
        |x: f32, y: f32| (10.0 * noise(x, y, 900.0) - 7.0).exp2() * (0.2 + 0.8 * noise(x, y, 14.0));
    let shifts = [(4.0f32, -3.0f32), (0.0, 0.0), (-3.0, 2.0)];
    let brackets: Vec<Frame> = [0.25f32, 1.0, 4.0]
        .iter()
        .zip(shifts)
        .map(|(&e, (dx, dy))| frame(|x, y| radiance(x + dx, y + dy) * e))
        .collect();
    let inputs: Vec<HdrInput<'_>> = brackets
        .iter()
        .zip([0.25f32, 1.0, 4.0])
        .map(|(f, e)| HdrInput {
            frame: f,
            exposure: Some(e),
        })
        .collect();
    let opts = HdrOptions {
        align: true,
        deghost: Deghost::Medium,
    };
    // The work is real: the frames align and span ±2 EV.
    let m = merge_hdr(&inputs, &opts, &NoProgress).unwrap();
    assert!((m.exposure_ev - 2.0).abs() < 0.05, "{} EV", m.exposure_ev);
    let mut g = c.benchmark_group("merge");
    g.sample_size(10);
    g.bench_function("hdr_3x24mp_aligned_deghost", |b| {
        b.iter(|| merge_hdr(&inputs, &opts, &NoProgress).unwrap());
    });
    drop(inputs);
    drop(brackets);

    // Three frames panned 25° apart, 35 mm equivalent on a 6000 px frame.
    let f = 6000.0f32 * 35.0 / 36.0;
    let pano: Vec<Frame> = [-25.0f32, 0.0, 25.0]
        .iter()
        .map(|yaw| {
            let (s, cy) = yaw.to_radians().sin_cos();
            frame(move |x, y| {
                let (rx, ry) = ((x - 2999.5) / f, (y - 1999.5) / f);
                let (wx, wz) = (cy * rx + s, -s * rx + cy);
                let lon = wx.atan2(wz);
                let lat = ry.atan2(wx.hypot(wz));
                0.1 + 0.5 * noise(lon * f, lat * f, 14.0)
            })
        })
        .collect();
    let inputs: Vec<PanoInput<'_>> = pano
        .iter()
        .map(|fr| PanoInput {
            frame: fr,
            focal_px: Some(f),
        })
        .collect();
    let m = merge_panorama(&inputs, &PanoOptions::default(), &NoProgress).unwrap();
    assert!(m.frame.w > 3 * W / 2, "{}×{}", m.frame.w, m.frame.h);
    g.bench_function("pano_3x24mp", |b| {
        b.iter(|| merge_panorama(&inputs, &PanoOptions::default(), &NoProgress).unwrap());
    });
    drop(inputs);
    drop(pano);

    // Registration cost against the frame count (every pair used to be matched).
    let f = SW as f32 * 35.0 / 36.0;
    for n in [6, 12, 24] {
        let frames = sweep(n);
        let inputs: Vec<PanoInput<'_>> = frames
            .iter()
            .map(|fr| PanoInput {
                frame: fr,
                focal_px: Some(f),
            })
            .collect();
        let opts = PanoOptions::default();
        let m = merge_panorama(&inputs, &opts, &NoProgress).unwrap();
        assert!(m.frame.w > SW, "{}x{}", m.frame.w, m.frame.h);
        g.bench_function(format!("pano_sweep_{n}x6mp"), |b| {
            b.iter(|| merge_panorama(&inputs, &opts, &NoProgress).unwrap());
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
