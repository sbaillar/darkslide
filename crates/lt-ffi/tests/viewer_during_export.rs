//! A batch export must not hurt the viewer. Viewer-style renders (a slider drag:
//! `Draft`, `Interactive`, a new exposure every frame) run on the shared GPU
//! renderer while a full-size export of the samples develops at `Background`
//! priority; the drag's frame times must stay at p90 ≤ 20 ms.
//!
//! This is its own test binary so no other test competes for the GPU.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_executor::block_on;
use lt_core::Adjustments;
use lt_ffi::export::{
    ExportColorSpace, ExportFileFormat, ExportRecipeSpec, ExportSize, ExportSource, export_batch,
};
use lt_ffi::jobs::CancelToken;
use lt_ffi::viewer::{
    DevelopFormat, DevelopGeometry, DevelopPriority, DevelopQuality, DevelopRequest, plan_develop,
    render_develop,
};

/// The 1-minute load average per core, at least 1: wall-clock budgets are multiplied by
/// it so a machine loaded well past its cores (other agents, llvm-cov) doesn't fail them
/// (as lt-pipeline's `tests/support::load_factor`).
fn load_factor() -> f64 {
    let load = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().find_map(|t| t.parse::<f64>().ok()))
        .unwrap_or(0.0);
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f64;
    (load / cores).max(1.0)
}

fn drag_request(long_edge: u32) -> DevelopRequest {
    DevelopRequest {
        max_width: long_edge,
        max_height: long_edge,
        region: None,
        quality: DevelopQuality::Draft,
        priority: DevelopPriority::Interactive,
        format: DevelopFormat::Rgba8DisplayP3,
        histogram: true,
        geometry: DevelopGeometry::Final,
    }
}

/// Render `n` drag frames of `path` (each with a new exposure) and return the
/// per-frame times in ms, sorted.
fn drag(path: &str, req: &DevelopRequest, n: usize, stop: &AtomicBool) -> Vec<f64> {
    let plan = block_on(plan_develop(
        path.into(),
        serde_json::to_string(&Adjustments::default()).unwrap(),
        req.clone(),
        CancelToken::new(),
    ))
    .unwrap();
    let mut buf = vec![0u8; (plan.bytes_per_row * u64::from(plan.height)) as usize];
    let mut times = Vec::with_capacity(n);
    for i in 0..n {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let mut adj = Adjustments::default();
        adj.exposure.exposure_ev = (i % 40) as f32 * 0.05 - 1.0;
        let json = serde_json::to_string(&adj).unwrap();
        let t = Instant::now();
        let f = block_on(render_develop(
            path.into(),
            json,
            req.clone(),
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            None,
            CancelToken::new(),
        ))
        .unwrap();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        assert!(f.histogram.is_some());
    }
    times.sort_by(f64::total_cmp);
    times
}

fn pct(v: &[f64], p: usize) -> f64 {
    v[(v.len() * p / 100).min(v.len() - 1)]
}

#[test]
#[allow(clippy::too_many_lines)] // one scenario, start to finish
fn viewer_drag_stays_fast_while_a_full_size_export_runs() {
    let samples = lt_testdata::samples_with_ext("ARW");
    if samples.len() < 4 {
        return;
    }
    let viewer = samples[0].to_string_lossy().into_owned();
    let t = tempfile::tempdir().unwrap();
    for long in [2560u32, 4096] {
        let req = drag_request(long);
        // Warm: decode, BASE, pipelines.
        let _ = drag(&viewer, &req, 5, &AtomicBool::new(false));
        let idle = drag(&viewer, &req, 60, &AtomicBool::new(false));

        let out = t.path().join(format!("out-{long}"));
        let sources: Vec<ExportSource> = samples[1..]
            .iter()
            .enumerate()
            .map(|(i, p)| ExportSource {
                keywords: Vec::new(),
                metadata: None,
                id: i as u64 + 1,
                path: p.to_string_lossy().into_owned(),
                seq: i as u64 + 1,
                gps: None,
                artist: None,
                copyright: None,
                adjustments_json: None,
                rating: None,
                label: None,
                pick: None,
            })
            .collect();
        let exported = sources.len();
        let recipe = ExportRecipeSpec {
            name: "full".into(),
            format: ExportFileFormat::Jpeg { quality: 90 },
            size: ExportSize::Original,
            allow_upscale: false,
            space: ExportColorSpace::Srgb,
            sharpen: 0.5,
            file_name_template: "{name}".into(),
            destination: out.to_string_lossy().into_owned(),
            include_exif: true,
            strip_gps: false,
            constrain_to_sensor_pixels: false,
            output_profile: None,
            ai_upscale: false,
            watermark: None,
            post_process: vec![],
            label_tags: true,
        };
        let done = Arc::new(AtomicBool::new(false));
        let export = {
            let done = done.clone();
            std::thread::spawn(move || {
                let t0 = Instant::now();
                let rep = block_on(export_batch(
                    sources,
                    vec![recipe],
                    None,
                    false,
                    None,
                    CancelToken::new(),
                    None,
                    None,
                ))
                .unwrap();
                done.store(true, Ordering::Relaxed);
                (rep, t0.elapsed())
            })
        };
        // Let the export start its first develop, then drag until it finishes.
        std::thread::sleep(Duration::from_millis(300));
        let busy = drag(&viewer, &req, 100_000, &done);
        let (rep, took) = export.join().unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert_eq!(rep.written.len(), exported);
        println!(
            "VIEWERBENCH drag Draft {long}: idle p50 {:.1} p90 {:.1} p99 {:.1} ms | during export \
             p50 {:.1} p90 {:.1} p99 {:.1} max {:.1} ms ({} frames) | export {} photos full size \
             in {:.2} s ({:.2} s/photo, {})",
            pct(&idle, 50),
            pct(&idle, 90),
            pct(&idle, 99),
            pct(&busy, 50),
            pct(&busy, 90),
            pct(&busy, 99),
            busy.last().copied().unwrap_or(0.0),
            busy.len(),
            exported,
            took.as_secs_f64(),
            took.as_secs_f64() / exported as f64,
            rep.renderer,
        );
        assert!(
            busy.len() >= 20,
            "the viewer kept rendering: {}",
            busy.len()
        );
        // Gated at both sizes (M6). 20 ms when the idle drag is fast (≤ 15 ms p90, a
        // normal build on an idle machine); on a slower build (instrumented coverage,
        // debug) the export may cost the drag at most 40 % over its own idle p90, never
        // more than 40 ms. Either way scaled by how loaded the machine is (`load_factor`).
        let budget = if pct(&idle, 90) <= 15.0 {
            20.0
        } else {
            (pct(&idle, 90) * 1.4).min(40.0)
        } * load_factor();
        assert!(
            pct(&busy, 90) <= budget,
            "viewer p90 {:.1} ms during export at {long} (budget {budget:.1} ms)",
            pct(&busy, 90)
        );
    }
}
