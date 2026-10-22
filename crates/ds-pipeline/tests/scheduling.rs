//! Scheduling classes on a shared GPU renderer: a storm of bulk thumbnails never stalls
//! an auto tool or an export (each class has its own in-flight place and turns go to the
//! highest class waiting), queued thumbnails cancel promptly, and renders waiting from
//! rayon workers never deadlock the pool. Own test binary (the global rayon pool and the
//! process-wide budget); every test takes `SERIAL`, and the long deadlock storm lives in
//! `tests/deadlock.rs` so timing assertions never share the machine with it. Skips
//! without a GPU.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ds_core::{Adjustments, RawImage, Size};
use ds_pipeline::{
    CancelToken, ImageKey, PipelineError, Priority, Quality, RenderRequest, Renderer, gpu_renderer,
};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn raws() -> Arc<Vec<RawImage>> {
    let mut v: Vec<RawImage> = ds_testdata::samples_with_ext("arw")
        .iter()
        .filter_map(|p| support::load_raw(p))
        .collect();
    if v.is_empty() {
        v = (0..4)
            .map(|i| support::synthetic_raw(6000 - 100 * i, 4000))
            .collect();
    }
    Arc::new(v)
}

/// Resident set size of this process (MB), from `ps`.
fn rss_mb() -> f64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<f64>()
                .ok()
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

/// New images rendered in the background (each needs its full-sensor noise scan and
/// RAW staging copy) while the viewer drags another: the viewer's frames never wait for
/// that CPU work (it runs on the low-priority pool; the viewer plans on the main one).
#[test]
fn background_statistics_never_block_the_viewer() {
    let _serial = serial();
    let Ok(g) = gpu_renderer() else { return };
    let viewer_raw = support::synthetic_raw(3000, 2000);
    let big = Arc::new(support::synthetic_raw(6000, 4000));
    let mut v = g.clone();
    let req = RenderRequest::new(Size::new(1200, 1200));
    let mut a = Adjustments::default();
    v.render(ImageKey(1), &viewer_raw, &a, &req).unwrap();
    let mut drag = |v: &mut ds_pipeline::GpuRenderer, n: usize| {
        let mut lat = Vec::new();
        for i in 0..n {
            a.exposure.exposure_ev = 0.05 * (i % 10) as f32;
            let t = Instant::now();
            v.render(ImageKey(1), &viewer_raw, &a, &req).unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
            std::thread::sleep(Duration::from_millis(4));
        }
        lat
    };
    let mut idle = drag(&mut v, 30);
    let stop = Arc::new(AtomicBool::new(false));
    let scans = Arc::new(AtomicU64::new(0));
    let h = {
        let (mut gg, big, stop, scans) = (g.clone(), big.clone(), stop.clone(), scans.clone());
        std::thread::spawn(move || {
            let mut k = 70_000u64;
            while !stop.load(Ordering::Relaxed) {
                // A fresh, different copy: a new image whose statistics aren't known yet
                // (the per-image caches key on the data's address and a content sample).
                let mut raw = (*big).clone();
                raw.data[0] = 600 + (k % 1000) as u16;
                let req = RenderRequest {
                    priority: Priority::Background,
                    ..RenderRequest::new(Size::new(512, 512))
                };
                gg.render(ImageKey(k), &raw, &Adjustments::default(), &req)
                    .unwrap();
                gg.evict(ImageKey(k));
                scans.fetch_add(1, Ordering::Relaxed);
                k += 1;
            }
        })
    };
    std::thread::sleep(Duration::from_millis(100));
    let mut busy = drag(&mut v, 60);
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
    let (i90, b90) = (percentile(&mut idle, 0.9), percentile(&mut busy, 0.9));
    let bmax = busy.iter().copied().fold(0.0, f64::max);
    println!(
        "viewer idle p90 {i90:.1} ms; during {} background new-image renders: p90 {b90:.1} ms, max {bmax:.1} ms",
        scans.load(Ordering::Relaxed)
    );
    if cfg!(debug_assertions) {
        return;
    }
    let f = support::load_factor();
    assert!(
        b90 <= (2.0 * i90 + 10.0) * f && bmax <= (i90 + 150.0) * f,
        "viewer p90 {b90:.1} max {bmax:.1} vs idle p90 {i90:.1} (load ×{f:.1})"
    );
}

fn percentile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// H3: 64 threads keep requesting thumbnails (distinct images, `Priority::Bulk`). Auto
/// tone (`Priority::Tool`) and a 2000 px export (`Priority::Background`) take at most
/// ~100 ms longer than on an idle GPU.
#[test]
fn a_thumbnail_storm_never_stalls_auto_tools_or_exports() {
    let _s = serial();
    let Ok(g) = gpu_renderer() else { return };
    let raws = raws();
    // A miss is re-measured once (another process can load the machine for a moment).
    if let Err(first) = thumbnail_storm(&g, &raws) {
        println!("re-measuring after: {first}");
        if let Err(e) = thumbnail_storm(&g, &raws) {
            panic!("{e}");
        }
    }
}

fn thumbnail_storm(g: &ds_pipeline::GpuRenderer, raws: &Arc<Vec<RawImage>>) -> Result<(), String> {
    let adj = Adjustments::default();
    let export_req = RenderRequest {
        quality: Quality::Full,
        priority: Priority::Background,
        ..RenderRequest::new(Size::new(2000, 2000))
    };
    let measure = |g: &ds_pipeline::GpuRenderer, round: u64| -> (f64, f64) {
        let t = Instant::now();
        let _ = g.auto_levels(ImageKey(90_000 + round), &raws[0], &adj);
        let tone = t.elapsed().as_secs_f64() * 1e3;
        let mut gg = g.clone();
        let t = Instant::now();
        gg.render16(ImageKey(91_000 + round), &raws[0], &adj, &export_req)
            .unwrap();
        let export = t.elapsed().as_secs_f64() * 1e3;
        gg.evict(ImageKey(91_000 + round));
        (tone, export)
    };
    // Idle baseline (warm: pipelines compiled, the RAW of image 0 uploaded once).
    let _ = measure(g, 0);
    let (mut tone0, mut export0): (Vec<f64>, Vec<f64>) = (1..6).map(|r| measure(g, r)).unzip();
    let (tone0, export0) = (median(&mut tone0), median(&mut export0));

    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicU64::new(0));
    let ctr = Arc::new(AtomicU64::new(1_000_000));
    let storm: Vec<_> = (0..64)
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
                    let req = RenderRequest {
                        priority: Priority::Bulk,
                        ..RenderRequest::new(Size::new(512, 512))
                    };
                    let raw = &raws[(k % raws.len() as u64) as usize];
                    gg.render(ImageKey(k), raw, &Adjustments::default(), &req)
                        .unwrap();
                    gg.evict(ImageKey(k));
                    done.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let peak = Arc::new(AtomicU64::new(0));
    let sampler = {
        let (stop, peak) = (stop.clone(), peak.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(rss_mb() as u64, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(500));
    let rounds: Vec<(f64, f64)> = (10..16).map(|r| measure(g, r)).collect();
    let thumbs = done.load(Ordering::Relaxed);
    stop.store(true, Ordering::Relaxed);
    for h in storm {
        h.join().unwrap();
    }
    sampler.join().unwrap();
    let tone_max = rounds.iter().map(|r| r.0).fold(0.0, f64::max);
    let export_max = rounds.iter().map(|r| r.1).fold(0.0, f64::max);
    println!(
        "idle: auto tone {tone0:.1} ms, export {export0:.1} ms; during a 64-thread thumbnail \
         storm: auto tone max {tone_max:.1} ms, export max {export_max:.1} ms \
         ({thumbs} thumbnails meanwhile); peak RSS {} MB",
        peak.load(Ordering::Relaxed)
    );
    assert!(thumbs > 0, "thumbnails still progress");
    if cfg!(debug_assertions) {
        return Ok(());
    }
    let f = support::load_factor();
    if tone_max > tone0 + 100.0 * f {
        return Err(format!(
            "auto tone {tone_max:.1} vs {tone0:.1} (load ×{f:.1})"
        ));
    }
    if export_max > export0 + 100.0 * f {
        return Err(format!(
            "export {export_max:.1} vs {export0:.1} (load ×{f:.1})"
        ));
    }
    Ok(())
}

/// Queued background renders give up when cancelled: of 40 thumbnails requested at once
/// and cancelled 30 ms later, the queued ones return `Cancelled` promptly (at most the
/// ones already in flight finish).
#[test]
fn queued_thumbnails_cancel_promptly() {
    let _s = serial();
    let Ok(g) = gpu_renderer() else { return };
    let raws = raws();
    let cancel = CancelToken::new();
    let start = Arc::new(std::sync::Barrier::new(41));
    let hs: Vec<_> = (0..40u64)
        .map(|i| {
            let (mut gg, raws, cancel, start) =
                (g.clone(), raws.clone(), cancel.clone(), start.clone());
            std::thread::spawn(move || {
                let req = RenderRequest {
                    priority: Priority::Bulk,
                    cancel: Some(cancel),
                    ..RenderRequest::new(Size::new(512, 512))
                };
                start.wait();
                let r = gg.render(
                    ImageKey(200_000 + i),
                    &raws[i as usize % raws.len()],
                    &Adjustments::default(),
                    &req,
                );
                (r.map(|_| ()), Instant::now())
            })
        })
        .collect();
    start.wait();
    std::thread::sleep(Duration::from_millis(30));
    let at = Instant::now();
    cancel.cancel();
    let mut cancelled = 0;
    let mut last = Duration::ZERO;
    for h in hs {
        let (r, t) = h.join().unwrap();
        match r {
            Err(PipelineError::Cancelled) => cancelled += 1,
            Ok(()) => {}
            Err(e) => panic!("{e}"),
        }
        last = last.max(t.saturating_duration_since(at));
    }
    println!(
        "40 queued thumbnails cancelled after 30 ms: {cancelled} returned Cancelled, all back {:.1} ms after the cancel",
        last.as_secs_f64() * 1e3
    );
    assert!(cancelled >= 36, "{cancelled} of 40 cancelled");
    if !cfg!(debug_assertions) {
        let f = support::load_factor();
        assert!(last.as_secs_f64() <= 0.1 * f, "{last:?} (load ×{f:.1})");
    }
}

/// M3: every worker of the global rayon pool waits for the background slot while plain
/// threads hold it and need the pool (`par_iter` in planning and staging). The waiting
/// workers run pool jobs meanwhile, so everything completes.
#[test]
fn background_renders_waiting_on_every_rayon_worker_never_deadlock() {
    let _s = serial();
    let Ok(g) = gpu_renderer() else { return };
    let n = rayon::current_num_threads();
    let (tx, rx) = std::sync::mpsc::channel();
    let g2 = g.clone();
    std::thread::spawn(move || {
        // Fresh images each: planning measures their noise with par_iter on the pool.
        let fresh = |i: u32| Arc::new(support::synthetic_raw(900 + 2 * i, 600));
        std::thread::scope(|sc| {
            for i in 0..4u32 {
                let mut gg = g2.clone();
                sc.spawn(move || {
                    for j in 0..3u32 {
                        let req = RenderRequest {
                            priority: Priority::Background,
                            ..RenderRequest::new(Size::new(600, 600))
                        };
                        let raw = fresh(100 + i * 10 + j);
                        gg.render(
                            ImageKey(u64::from(300_000 + i * 10 + j)),
                            &raw,
                            &Adjustments::default(),
                            &req,
                        )
                        .unwrap();
                    }
                });
            }
            rayon::scope(|rs| {
                for i in 0..(2 * n) as u32 {
                    let mut gg = g2.clone();
                    rs.spawn(move |_| {
                        let req = RenderRequest {
                            priority: Priority::Background,
                            ..RenderRequest::new(Size::new(600, 600))
                        };
                        let raw = fresh(i);
                        gg.render(
                            ImageKey(u64::from(400_000 + i)),
                            &raw,
                            &Adjustments::default(),
                            &req,
                        )
                        .unwrap();
                    });
                }
            });
        });
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(120)).is_ok(),
        "deadlock: background renders on rayon workers never finished"
    );
}

/// M7: the CPU renderer is shared (clones render concurrently, the caches are locked
/// only for lookups): a viewer render on it never waits for an export in progress, and
/// the export's half-core pool leaves it cores. Viewer latency during a full-size export
/// stays within 3× its idle median (and far below the export's duration).
#[test]
fn the_cpu_viewer_does_not_wait_for_an_export() {
    let _s = serial();
    let raw = Arc::new(support::synthetic_raw(6000, 4000));
    // A miss is re-measured once (another process can load the machine for a moment).
    if let Err(first) = cpu_viewer_during_export(&raw) {
        println!("re-measuring after: {first}");
        if let Err(e) = cpu_viewer_during_export(&raw) {
            panic!("{e}");
        }
    }
}

fn cpu_viewer_during_export(raw: &Arc<RawImage>) -> Result<(), String> {
    let cpu = ds_pipeline::cpu_renderer();
    let view = RenderRequest::new(Size::new(800, 800));
    let mut v = cpu.clone();
    v.render(ImageKey(1), raw, &Adjustments::default(), &view)
        .unwrap();
    let mut a = Adjustments::default();
    let mut idle = Vec::new();
    for i in 0..8u8 {
        a.exposure.exposure_ev = 0.05 * f32::from(i);
        let t = Instant::now();
        v.render(ImageKey(1), raw, &a, &view).unwrap();
        idle.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let idle = median(&mut idle);
    let stop = Arc::new(AtomicBool::new(false));
    let export_ms = Arc::new(AtomicU64::new(0));
    let h = {
        let (mut e, raw, stop, export_ms) =
            (cpu.clone(), raw.clone(), stop.clone(), export_ms.clone());
        std::thread::spawn(move || {
            let req = RenderRequest {
                quality: Quality::Full,
                priority: Priority::Background,
                ..RenderRequest::new(Size::new(6000, 6000))
            };
            let mut k = 10;
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                e.render16(ImageKey(k), &raw, &Adjustments::default(), &req)
                    .unwrap();
                export_ms.fetch_max(t.elapsed().as_millis() as u64, Ordering::Relaxed);
                e.evict(ImageKey(k));
                k += 1;
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let mut busy = Vec::new();
    for i in 0..12u8 {
        a.exposure.exposure_ev = 0.05 * f32::from(i);
        let t = Instant::now();
        v.render(ImageKey(1), raw, &a, &view).unwrap();
        busy.push(t.elapsed().as_secs_f64() * 1e3);
    }
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
    let worst = busy.iter().copied().fold(0.0, f64::max);
    let export = export_ms.load(Ordering::Relaxed) as f64;
    println!(
        "CPU viewer 800 px: idle median {idle:.1} ms, during a full-size export median {:.1} ms, max {worst:.1} ms (export {export:.0} ms)",
        median(&mut busy)
    );
    if cfg!(debug_assertions) {
        return Ok(());
    }
    if worst > 3.0 * idle.max(10.0) {
        return Err(format!("viewer {worst:.1} ms vs idle {idle:.1} ms"));
    }
    if worst >= export {
        return Err(format!(
            "viewer {worst:.1} ms waited a whole export ({export:.0} ms)"
        ));
    }
    Ok(())
}
