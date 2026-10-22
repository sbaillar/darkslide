//! The memory budget is a ceiling: with many threads thrashing the caches (views of many
//! images plus a background export) the memory counted against the budget stays at it.
//! Own test binary: the budget is process-wide. Skips without a GPU.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ds_core::{Adjustments, Size};
use ds_pipeline::{ImageKey, Priority, Quality, RenderRequest, Renderer, cache, gpu_renderer};

#[test]
fn the_budget_holds_under_cache_thrash() {
    let Ok(g) = gpu_renderer() else { return };
    let budget = 512u64 << 20;
    cache::set_budget_bytes(budget);
    let raws: Arc<Vec<_>> = Arc::new(
        (0..4)
            .map(|i| support::synthetic_raw(6000 - 200 * i, 4000))
            .collect(),
    );
    cache::reset_peak();
    let stop = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicU64::new(0));
    let mut workers: Vec<_> = (0..8u64)
        .map(|t| {
            let (mut r, raws, stop, count) = (g.clone(), raws.clone(), stop.clone(), count.clone());
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let k = (t * 7 + n) % 24;
                    let req = RenderRequest::new(Size::new(2560, 2560));
                    r.render(
                        ImageKey(k),
                        &raws[(k % 4) as usize],
                        &Adjustments::default(),
                        &req,
                    )
                    .unwrap();
                    n += 1;
                    count.fetch_add(1, Ordering::Relaxed);
                }
                n
            })
        })
        .collect();
    {
        let (mut r, raws, stop, count) = (g.clone(), raws.clone(), stop.clone(), count.clone());
        workers.push(std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let req = RenderRequest {
                    quality: Quality::Full,
                    priority: Priority::Background,
                    ..RenderRequest::new(Size::new(1600, 1600))
                };
                r.render(
                    ImageKey(100 + n % 4),
                    &raws[(n % 4) as usize],
                    &Adjustments::default(),
                    &req,
                )
                .unwrap();
                n += 1;
                count.fetch_add(1, Ordering::Relaxed);
            }
            n
        }));
    }
    // At least 4 s and 40 renders of thrash (slow instrumented builds run longer).
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) || count.load(Ordering::Relaxed) < 40 {
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::Relaxed);
    let frames: u64 = workers.into_iter().map(|w| w.join().unwrap()).sum();
    let peak = cache::peak_bytes();
    println!(
        "{frames} renders under a {} MB budget: peak {} MB, now {} MB (pool {} MB, RAWs {} MB); by use now {:?} MB",
        budget >> 20,
        peak >> 20,
        cache::used_bytes() >> 20,
        g.pool_bytes() >> 20,
        g.raw_bytes() >> 20,
        cache::used_by_use().map(|v| v >> 20)
    );
    assert!(frames >= 40);
    // A hard ceiling: renders wait for room instead of exceeding it.
    assert!(peak <= budget, "peak {} MB", peak >> 20);
    assert!(cache::used_bytes() <= budget);
    cache::set_budget_bytes(0);
}
