//! Viewer latency on a shared GPU renderer while thumbnail threads render in the
//! background (ported from the review probe `q_conc`). Budget (release builds, real
//! samples): with 8 thumbnail threads the viewer's exposure drag stays at median ≤ 16 ms
//! and p90 ≤ 20 ms. Skips without samples or a GPU.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lt_core::{Adjustments, Size};
use lt_pipeline::{ImageKey, Priority, RenderRequest, Renderer, gpu_renderer};

/// The timing tests in this file must not overlap each other.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

#[test]
fn background_thumbnails_do_not_stall_the_viewer() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let paths = lt_testdata::samples_with_ext("arw");
    if paths.len() < 2 {
        return;
    }
    let Ok(g) = gpu_renderer() else { return };
    let raws: Arc<Vec<_>> = Arc::new(
        paths
            .iter()
            .map(|p| (support::load_raw(p).unwrap(), support::capture_info(p)))
            .collect(),
    );
    let mut results = Vec::new();
    for threads in [0usize, 8, 32] {
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicU64::new(0));
        let ctr = Arc::new(AtomicU64::new(1_000_000 * (threads as u64 + 1)));
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (mut gg, raws, stop, done, ctr) = (
                    g.clone(),
                    raws.clone(),
                    stop.clone(),
                    done.clone(),
                    ctr.clone(),
                );
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let k = ctr.fetch_add(1, Ordering::Relaxed);
                        let (r, c) = &raws[(k % raws.len() as u64) as usize];
                        let req = RenderRequest {
                            capture: c.clone(),
                            priority: Priority::Background,
                            ..RenderRequest::new(Size::new(512, 512))
                        };
                        gg.render(ImageKey(k), r, &Adjustments::default(), &req)
                            .unwrap();
                        gg.evict(ImageKey(k));
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(300));
        let mut v = g.clone();
        let (r, c) = &raws[0];
        let req = RenderRequest {
            capture: c.clone(),
            ..RenderRequest::new(Size::new(2560, 2560))
        };
        let mut a = Adjustments::default();
        v.render(ImageKey(1), r, &a, &req).unwrap();
        let (t0, d0) = (Instant::now(), done.load(Ordering::Relaxed));
        let mut lat = Vec::new();
        for i in 0..60 {
            a.exposure.exposure_ev = 0.05 * (i % 10) as f32;
            let t = Instant::now();
            v.render(ImageKey(1), r, &a, &req).unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
            std::thread::sleep(Duration::from_millis(4));
        }
        let thumbs = (done.load(Ordering::Relaxed) - d0) as f64 / t0.elapsed().as_secs_f64();
        stop.store(true, Ordering::Relaxed);
        for w in workers {
            w.join().unwrap();
        }
        let (med, p90, max) = (pct(&mut lat, 0.5), pct(&mut lat, 0.9), pct(&mut lat, 1.0));
        println!(
            "{threads} thumbnail threads: viewer median {med:.1} ms, p90 {p90:.1}, max {max:.1}; {thumbs:.0} thumbnails/s"
        );
        results.push((threads, med, p90));
    }
    if !cfg!(debug_assertions) {
        let f = support::load_factor();
        for &(threads, med, p90) in &results[1..] {
            assert!(
                med <= 16.0 * f && p90 <= 20.0 * f,
                "viewer with {threads} thumbnail threads: median {med} p90 {p90} (load ×{f:.1})"
            );
        }
    }
}

fn busy() -> Adjustments {
    let mut a = Adjustments::default();
    a.hdr.shadows = 40.0;
    a.hdr.highlights = 50.0;
    a.detail.clarity = 30.0;
    a.detail.structure = 20.0;
    a
}

/// Wall-clock budgets are asserted in release builds only, and a miss is re-measured once
/// (another process can hold the GPU for a moment).
fn timed(label: &str, mut run: impl FnMut() -> Result<(), String>) {
    if cfg!(debug_assertions) {
        let _ = run();
        return;
    }
    if let Err(first) = run() {
        println!("{label}: retrying after: {first}");
        if let Err(e) = run() {
            panic!("{label}: {e}");
        }
    }
}

fn sample() -> (Arc<lt_core::RawImage>, lt_pipeline::CaptureInfo) {
    let (raw, cap) = lt_testdata::sample_arw().map_or_else(
        || {
            (
                support::synthetic_raw(6000, 4000),
                lt_pipeline::CaptureInfo::default(),
            )
        },
        |p| (support::load_raw(&p).unwrap(), support::capture_info(&p)),
    );
    (Arc::new(raw), cap)
}

/// Cancelling a background export (tiled 8192, resized 4000 and 1080) leaves at most one
/// short stage queued: the next preview is within 16 ms of a normal one.
#[test]
fn a_cancelled_export_leaves_at_most_one_stage_of_gpu_work() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok(g) = gpu_renderer() else { return };
    let (raw, cap) = sample();
    let preview = RenderRequest {
        capture: cap.clone(),
        ..RenderRequest::new(Size::new(2560, 2560))
    };
    let mut v = g.clone();
    v.render(ImageKey(1), &raw, &Adjustments::default(), &preview)
        .unwrap();
    let mut k = 100u64;
    for px in [8192u32, 4000, 1080] {
        timed(&format!("cancel {px}"), || {
            let mut worst: f64 = 0.0;
            for delay in [0u64, 10, 30, 60, 100] {
                k += 1;
                let tok = lt_pipeline::CancelToken::new();
                let req = RenderRequest {
                    quality: lt_pipeline::Quality::Full,
                    cancel: Some(tok.clone()),
                    capture: cap.clone(),
                    priority: Priority::Background,
                    ..RenderRequest::new(Size::new(px, px))
                };
                let (mut gg, raw2) = (g.clone(), raw.clone());
                let key = ImageKey(k);
                let h = std::thread::spawn(move || {
                    gg.render16(key, &raw2, &busy(), &req).map(|i| i.size)
                });
                std::thread::sleep(Duration::from_millis(delay));
                tok.cancel();
                let r = h.join().unwrap();
                assert!(
                    matches!(r, Err(lt_pipeline::PipelineError::Cancelled) | Ok(_)),
                    "{r:?}"
                );
                let t = Instant::now();
                v.render(ImageKey(1), &raw, &Adjustments::default(), &preview)
                    .unwrap();
                let next = t.elapsed().as_secs_f64() * 1e3;
                let t = Instant::now();
                v.render(ImageKey(1), &raw, &Adjustments::default(), &preview)
                    .unwrap();
                let normal = t.elapsed().as_secs_f64() * 1e3;
                println!(
                    "export {px}, cancel after {delay} ms: next preview {next:.1} ms (normal {normal:.1})"
                );
                worst = worst.max(next - normal);
                v.evict(key);
            }
            let f = support::load_factor();
            if worst <= 16.0 * f {
                Ok(())
            } else {
                Err(format!("cancel residue {worst:.1} ms (load ×{f:.1})"))
            }
        });
    }
}

/// A background export (tiled or resized, busy edit) running while the viewer drags:
/// the viewer's p90 stays ≤ 20 ms.
#[test]
fn background_exports_do_not_stall_the_viewer() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Ok(g) = gpu_renderer() else { return };
    let (raw, cap) = sample();
    for px in [4000u32, 2048, 1080] {
        timed(&format!("export {px}"), || {
            let stop = Arc::new(AtomicBool::new(false));
            let exports = Arc::new(AtomicU64::new(0));
            let h = {
                let (mut gg, raw, cap, stop, exports) = (
                    g.clone(),
                    raw.clone(),
                    cap.clone(),
                    stop.clone(),
                    exports.clone(),
                );
                std::thread::spawn(move || {
                    let mut k = 5000u64;
                    while !stop.load(Ordering::Relaxed) {
                        let req = RenderRequest {
                            capture: cap.clone(),
                            quality: lt_pipeline::Quality::Full,
                            priority: Priority::Background,
                            ..RenderRequest::new(Size::new(px, px))
                        };
                        gg.render16(ImageKey(k), &raw, &busy(), &req).unwrap();
                        gg.evict(ImageKey(k));
                        exports.fetch_add(1, Ordering::Relaxed);
                        k += 1;
                    }
                })
            };
            std::thread::sleep(Duration::from_millis(200));
            let mut v = g.clone();
            let req = RenderRequest {
                capture: cap.clone(),
                ..RenderRequest::new(Size::new(2560, 2560))
            };
            let mut a = Adjustments::default();
            v.render(ImageKey(1), &raw, &a, &req).unwrap();
            let mut lat = Vec::new();
            for i in 0..80 {
                a.exposure.exposure_ev = 0.05 * (i % 10) as f32;
                let t = Instant::now();
                v.render(ImageKey(1), &raw, &a, &req).unwrap();
                lat.push(t.elapsed().as_secs_f64() * 1e3);
                std::thread::sleep(Duration::from_millis(4));
            }
            stop.store(true, Ordering::Relaxed);
            h.join().unwrap();
            let (med, p90, max) = (pct(&mut lat, 0.5), pct(&mut lat, 0.9), pct(&mut lat, 1.0));
            println!(
                "background {px} px exports ({} done): viewer median {med:.1} ms, p90 {p90:.1}, max {max:.1}",
                exports.load(Ordering::Relaxed)
            );
            let f = support::load_factor();
            if p90 <= 20.0 * f {
                Ok(())
            } else {
                Err(format!("viewer p90 {p90:.1} ms (load ×{f:.1})"))
            }
        });
    }
}
