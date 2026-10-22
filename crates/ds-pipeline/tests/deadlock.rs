//! Renders never deadlock on the shared pools: a 10 s storm of concurrent brush-mask
//! renders, plain renders and evicts. Its own test binary, so the timing-sensitive tests
//! (`tests/scheduling.rs`, `tests/concurrency.rs`) never run while it loads the machine.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ds_core::{Adjustments, RawImage, Size};
use ds_pipeline::{ImageKey, Priority, RenderRequest, Renderer, gpu_renderer};

/// A brush layer whose raster must be stamped (in parallel) for every new image.
fn brushed() -> Adjustments {
    use ds_core::adjust::{BrushStroke, Layer, Mask, MaskOp, MaskShape};
    let mut l = Layer {
        id: 3,
        ..Layer::default()
    };
    l.adjustments.exposure.exposure_ev = 0.5;
    l.mask = Mask {
        shapes: vec![MaskShape::Brush {
            op: MaskOp::Add,
            strokes: vec![BrushStroke {
                points: (0..40)
                    .map(|i| [0.1 + 0.02 * i as f32, 0.5 + 0.2 * (i as f32 / 5.0).sin()])
                    .collect(),
                size: 0.08,
                ..BrushStroke::default()
            }],
            feather: 0.0,
        }],
        ..Mask::default()
    };
    Adjustments {
        layers: vec![l],
        ..Adjustments::default()
    }
}

/// 8 threads for 10 s on one shared renderer (CPU, then GPU): brush-mask renders of new
/// images (each stamps its raster in parallel), plain renders, and evicts, at both
/// priorities. Nothing deadlocks: a watchdog fails the test instead of hanging.
///
/// Regression: GPU renders compiled masks on the pipeline pool holding the raster cache
/// lock; a worker waiting for its parallel stamping stole another render's job that
/// blocked on the same lock (the app's test suite hung on every run). Debug builds also
/// assert that no ds-pipeline lock is held when entering a thread pool.
#[test]
fn brush_renders_plain_renders_and_evicts_never_deadlock() {
    fn storm<R: Renderer + Clone + Send + Sync>(r: &R, raw: &RawImage, secs: u64) -> u64 {
        let end = Instant::now() + Duration::from_secs(secs);
        let done = AtomicU64::new(0);
        std::thread::scope(|sc| {
            for t in 0..8u64 {
                let (mut own, done) = (r.clone(), &done);
                sc.spawn(move || {
                    let mut i = 0u64;
                    while Instant::now() < end {
                        i += 1;
                        let key = ImageKey(10_000 + t * 1_000_000 + i);
                        let req = RenderRequest {
                            priority: if t % 2 == 0 {
                                Priority::Background
                            } else {
                                Priority::Interactive
                            },
                            ..RenderRequest::new(Size::new(480, 480))
                        };
                        match (t + i) % 3 {
                            0 => {
                                own.render(key, raw, &brushed(), &req).unwrap();
                            }
                            1 => {
                                own.render(key, raw, &Adjustments::default(), &req).unwrap();
                            }
                            _ => {
                                own.render(key, raw, &brushed(), &req).unwrap();
                                own.evict(key);
                            }
                        }
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        done.load(Ordering::Relaxed)
    }
    let secs = if cfg!(debug_assertions) { 4 } else { 10 };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let raw = support::synthetic_raw(1500, 1000);
        let cpu = storm(&ds_pipeline::cpu_renderer(), &raw, secs);
        let gpu = gpu_renderer().ok().map(|g| storm(&g, &raw, secs));
        let _ = tx.send((cpu, gpu));
    });
    let r = rx.recv_timeout(Duration::from_secs(2 * secs + 120));
    let Ok((cpu, gpu)) = r else {
        panic!("deadlock: concurrent brush/plain renders and evicts never finished");
    };
    println!("{secs} s x 8 threads: {cpu} CPU renders, {gpu:?} GPU renders");
    assert!(cpu > 0 && gpu.is_none_or(|n| n > 0));
}
