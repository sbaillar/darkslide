//! GPU admission under contention: an interactive slider drag while bulk threads queue
//! thumbnail renders against a tight memory budget. The drag's frame time must stay near
//! its uncontended time (ARCHITECTURE.md: GPU warm ≤ 16 ms); background throughput is
//! printed next to it. Real A9 ARW; skips when `samples/` is absent.
#[path = "../tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use lt_core::{Adjustments, Size};
use lt_pipeline::{ImageKey, Priority, RenderRequest, Renderer, gpu_renderer};

const BULK_THREADS: usize = 32;

fn bench_admit(c: &mut Criterion) {
    let Some(path) = lt_testdata::sample_arw() else {
        eprintln!("no samples/ — skipping admit bench");
        return;
    };
    let Ok(mut gpu) = gpu_renderer() else {
        eprintln!("no GPU — skipping admit bench");
        return;
    };
    let raw = Arc::new(support::load_raw(&path).expect("decode"));
    let capture = support::capture_info(&path);
    let viewer = RenderRequest {
        capture: capture.clone(),
        ..RenderRequest::new(Size::new(2560, 2560))
    };
    let thumb = RenderRequest {
        capture,
        priority: Priority::Bulk,
        ..RenderRequest::new(Size::new(256, 256))
    };
    // A budget that fits the viewer's image and a few thumbnails' working sets, not 32.
    lt_pipeline::cache::set_budget_bytes(raw.data.len() as u64 * 2 * 5);

    let mut g = c.benchmark_group("admit");
    g.sample_size(20).measurement_time(Duration::from_secs(8));
    let mut adj = Adjustments::default();
    let mut i = 0u32;
    let key = ImageKey(1);
    let mut drag = |b: &mut criterion::Bencher| {
        b.iter(|| {
            i += 1;
            adj.exposure.exposure_ev = (i % 7) as f32 * 0.1;
            gpu.render(key, &raw, &adj, &viewer).expect("render")
        });
    };
    g.bench_function("drag_alone", &mut drag);

    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicU64::new(0));
    let t0 = Instant::now();
    let workers: Vec<_> = (0..BULK_THREADS)
        .map(|n| {
            let (stop, done, raw, thumb) = (stop.clone(), done.clone(), raw.clone(), thumb.clone());
            let mut r = gpu_renderer().expect("gpu");
            std::thread::spawn(move || {
                let adj = Adjustments::default();
                while !stop.load(Ordering::Relaxed) {
                    if r.render(ImageKey(100 + n as u64), &raw, &adj, &thumb)
                        .is_ok()
                    {
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();
    g.bench_function("drag_vs_32_bulk_threads", &mut drag);
    stop.store(true, Ordering::Relaxed);
    for w in workers {
        w.join().expect("worker");
    }
    println!(
        "admit: {} bulk thumbnails in {:.1} s alongside the drag",
        done.load(Ordering::Relaxed),
        t0.elapsed().as_secs_f64()
    );
    g.finish();
}

criterion_group!(admit, bench_admit);
criterion_main!(admit);
