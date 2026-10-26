//! Background renders from rayon's global workers never deadlock the GPU gate. Its own
//! test binary: it sizes rayon's global pool (2 workers, so it saturates) before anything
//! else uses it.
//!
//! Regression (the app's lt-ffi suite hung for hours at full CPU): a render holding its
//! class's background slot converted a linear (already demosaiced) image on rayon's
//! global pool while holding the renderer lock. Every global worker was parked in
//! `Gate::background` waiting for that slot (one of them from inside a stolen job of an
//! unrelated decode), so the slot holder's parallel loop never got a worker. The slot
//! holder's CPU work must only ever use the pipeline's own pools (`par`).

mod support;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lt_core::{Adjustments, CfaPattern, RawImage, Size};
use lt_pipeline::{ImageKey, Priority, Quality, RenderRequest, Renderer, gpu_renderer};
use rayon::prelude::*;

/// An already-demosaiced (linear DNG-like) image: its BASE is converted on the CPU.
fn linear_raw(w: u32, h: u32) -> RawImage {
    let mosaic = support::synthetic_raw(w, h);
    let data = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            let v = f32::from(mosaic.data[i as usize]);
            [
                v,
                v * 0.8 + (x % 7) as f32 * 30.0,
                v * 0.6 + (y % 5) as f32 * 40.0,
            ]
            .map(|c| c.min(15_000.0) as u16)
        })
        .collect();
    RawImage {
        cfa: CfaPattern::LinearRgb,
        data,
        ..mosaic
    }
}

#[test]
fn background_renders_from_global_workers_never_deadlock_the_gate() {
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build_global()
        .expect("the global pool is sized before first use");
    let Ok(gpu) = gpu_renderer() else {
        eprintln!("skipping: no GPU");
        return;
    };
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let linear = linear_raw(2400, 1600);
        let mosaic = support::synthetic_raw(3000, 2000);
        let raws = [&linear, &mosaic];
        let stop = AtomicBool::new(false);
        let renders = AtomicU64::new(0);
        let end = Instant::now() + Duration::from_secs(20);
        std::thread::scope(|s| {
            // Background renders (thumbnails, previews) issued from the global pool's
            // workers, as the app's grid does; new keys, so every one uploads, measures
            // its statistics and builds a BASE.
            s.spawn(|| {
                let mut round = 0u64;
                while Instant::now() < end {
                    round += 1;
                    (0..16u64).into_par_iter().for_each(|i| {
                        let mut g = gpu.clone();
                        let key = ImageKey(1000 + round * 100 + i);
                        let req = RenderRequest {
                            priority: if i % 4 == 0 {
                                Priority::Bulk
                            } else {
                                Priority::Background
                            },
                            ..RenderRequest::new(Size::new(512, 512))
                        };
                        g.render(key, raws[i as usize % 2], &Adjustments::default(), &req)
                            .unwrap();
                        g.evict(key);
                        renders.fetch_add(1, Ordering::Relaxed);
                    });
                }
            });
            // Unrelated parallel work on the global pool (a decode), whose waiting
            // workers steal the renders above.
            s.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    let v: u64 = (0..200_000u64)
                        .into_par_iter()
                        .map(|x| x.wrapping_mul(2_654_435_761) >> 7)
                        .sum();
                    std::hint::black_box(v);
                }
            });
            // Interactive renders and full-size exports of both images, from plain threads.
            for t in 0..2u64 {
                let (mut g, stop, renders) = (gpu.clone(), &stop, &renders);
                s.spawn(move || {
                    let mut i = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        i += 1;
                        let raw = raws[((t + i) % 2) as usize];
                        let req = if t == 0 {
                            RenderRequest::new(Size::new(1200, 1200))
                        } else {
                            RenderRequest {
                                quality: Quality::Full,
                                priority: Priority::Background,
                                ..RenderRequest::new(Size::new(2000, 2000))
                            }
                        };
                        let key = ImageKey(10 + t * 2 + i % 2);
                        g.render(key, raw, &Adjustments::default(), &req).unwrap();
                        if i.is_multiple_of(3) {
                            g.evict(key);
                        }
                        renders.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            while Instant::now() < end {
                std::thread::sleep(Duration::from_millis(50));
            }
            stop.store(true, Ordering::Relaxed);
        });
        let _ = done_tx.send(renders.load(Ordering::Relaxed));
    });
    // Watchdog: a deadlock fails the test instead of hanging the suite.
    let Ok(n) = done_rx.recv_timeout(Duration::from_secs(240)) else {
        // Straight to stderr: the harness's capture would swallow eprintln!.
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "DEADLOCK: renders stopped making progress (gate / pool cycle)"
        );
        std::process::exit(101);
    };
    println!("{n} renders");
    assert!(n > 16);
    worker.join().unwrap();
}
