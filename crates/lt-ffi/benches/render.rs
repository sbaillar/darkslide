//! FFI render path: the test pattern rendered into caller memory (what the app
//! does now) vs. the old path (fresh `Vec` returned by value, then copied into
//! a `RustBuffer` and again into Swift `Data`: simulated with two clones).
//! Also the embedded-preview extraction (skipped without `samples/`).

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use futures_executor::block_on;
use lt_ffi::{CancelToken, Engine, FrameFormat, RenderRequest, TestPatternParams, plan_frame};

fn render_into(engine: &Arc<Engine>, w: u32, h: u32, req: RenderRequest, buf: &mut [u8]) {
    block_on(engine.render_test_pattern_into(
        w,
        h,
        TestPatternParams::default(),
        req,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        CancelToken::new(),
    ))
    .unwrap();
}

fn benches(c: &mut Criterion) {
    let engine = Engine::new();
    let mut g = c.benchmark_group("render");
    g.sample_size(20);
    let cases = [
        (
            "fit_2560_rgba8",
            RenderRequest {
                max_long_edge: 2560,
                scale: None,
                region: None,
                format: FrameFormat::Rgba8Srgb,
            },
        ),
        (
            "fit_2560_rgba16f",
            RenderRequest {
                max_long_edge: 2560,
                scale: None,
                region: None,
                format: FrameFormat::Rgba16fLinearP3,
            },
        ),
        (
            "full_6000x4000_rgba8",
            RenderRequest {
                max_long_edge: 6000,
                scale: None,
                region: None,
                format: FrameFormat::Rgba8Srgb,
            },
        ),
    ];
    for (name, req) in cases {
        let info = plan_frame(6000, 4000, req).unwrap();
        let mut buf = vec![0u8; info.byte_len() as usize];
        g.bench_function(format!("{name}_into_caller_buffer"), |b| {
            b.iter(|| render_into(&engine, 6000, 4000, req, black_box(&mut buf)));
        });
        if req.format == FrameFormat::Rgba8Srgb {
            g.bench_function(format!("{name}_legacy_vec_plus_2_copies"), |b| {
                b.iter(|| {
                    let mut v = vec![0u8; info.byte_len() as usize];
                    render_into(&engine, 6000, 4000, req, &mut v);
                    let rust_buffer = v.clone();
                    black_box(rust_buffer.clone())
                });
            });
        }
    }
    g.finish();

    if let Some(path) = lt_testdata::sample_arw() {
        let p = path.to_string_lossy().into_owned();
        c.bench_function("embedded_preview_ffi", |b| {
            b.iter(|| {
                black_box(
                    block_on(lt_ffi::embedded_preview(p.clone(), CancelToken::new())).unwrap(),
                )
            });
        });
        c.bench_function("raw_metadata_ffi", |b| {
            b.iter(|| {
                black_box(block_on(lt_ffi::raw_metadata(p.clone(), CancelToken::new())).unwrap())
            });
        });
    }
}

criterion_group!(group, benches);
criterion_main!(group);
