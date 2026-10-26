//! AI Denoise, the Rust side of one A9 frame (the model runs in the app): demosaic and
//! encode (`DenoiseJob::new`), reading and writing every tile, stitching
//! (`DenoiseJob::finish`), and the blend at a strength (`denoised_raw`). Budget: these
//! together stay under 1 s, a fifth of the 5 s per photo (M4 Pro: ~0.1 + 0.1 + 0.15 +
//! 0.25 s, blend ~0.03 s). The real A9 frame when `samples/` exists, else a synthetic
//! 6000×4000 mosaic.

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use lt_core::{CfaPattern, Orientation, RawImage, Size};
use lt_pipeline::Priority;
use lt_pipeline::denoise::{DenoiseJob, denoised_raw, linear_original};

fn frame() -> RawImage {
    if let Some(p) = lt_testdata::sample_arw()
        && let Ok(raw) = lt_raw::decode(&p)
    {
        return raw;
    }
    let (w, h) = (6000u32, 4000u32);
    RawImage {
        size: Size::new(w, h),
        cfa: CfaPattern::Rggb,
        data: (0..w * h)
            .map(|i| (600 + (i.wrapping_mul(2_654_435_761) >> 20) % 8000) as u16)
            .collect(),
        black_level: [512.0; 4],
        white_level: 16383.0,
        wb_as_shot: [2.0, 1.0, 1.5],
        wb_daylight: [2.0, 1.0, 1.5],
        cam_to_xyz: [[0.41, 0.36, 0.18], [0.21, 0.72, 0.07], [0.02, 0.12, 0.95]],
        orientation: Orientation::Normal,
        default_crop: None,
    }
}

/// Every tile read and written back unchanged (a pass-through model).
fn run_tiles(job: &DenoiseJob) {
    let mut buf = vec![0u16; job.tile_len()];
    let t = job.grid().tile() as usize;
    for i in 0..job.grid().len() {
        job.read_tile(i, &mut buf).unwrap();
        job.write_tile(i, &buf, t, t * t).unwrap();
    }
}

fn denoise(c: &mut Criterion) {
    let raw = frame();
    let mut g = c.benchmark_group("denoise");
    g.sample_size(10).measurement_time(Duration::from_secs(10));
    g.bench_function("job_new", |b| {
        b.iter(|| DenoiseJob::new(&raw, Priority::Interactive).unwrap());
    });
    g.bench_function("tiles_read_write", |b| {
        b.iter_batched(
            || DenoiseJob::new(&raw, Priority::Interactive).unwrap(),
            |job| run_tiles(&job),
            criterion::BatchSize::PerIteration,
        );
    });
    g.bench_function("finish", |b| {
        b.iter_batched(
            || {
                let job = DenoiseJob::new(&raw, Priority::Interactive).unwrap();
                run_tiles(&job);
                job
            },
            |job| job.finish(Priority::Interactive).unwrap(),
            criterion::BatchSize::PerIteration,
        );
    });
    let job = DenoiseJob::new(&raw, Priority::Interactive).unwrap();
    run_tiles(&job);
    let den = job.finish(Priority::Interactive).unwrap();
    let orig = linear_original(&raw, Priority::Interactive).unwrap();
    g.bench_function("linear_original", |b| {
        b.iter(|| linear_original(&raw, Priority::Interactive).unwrap());
    });
    g.bench_function("denoised_raw_50", |b| {
        b.iter(|| denoised_raw(&raw, &den, Some(&orig), 0.5).unwrap());
    });
    g.finish();
}

criterion_group!(benches, denoise);
criterion_main!(benches);
