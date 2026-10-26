//! Memory and time budget of large removals on a synthetic 61 MP (9504×6336) frame, in its
//! own process so the peak RSS is this test's: wide strokes corner to corner (10% and 30%
//! brush), a 10% horizontal stroke, a person-sized object (~1500×5900 px), a round object
//! whose window (~3030 px) is filled from a reduced image and one just small enough
//! (~2020 px) to be worked at full resolution. Each stays under 1 GB peak for the whole
//! process (the frame and its derived copy included). Release builds only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lt_core::adjust::{BrushStroke, Removal};
use lt_core::{Adjustments, CfaPattern, Orientation, RawImage, Size};
use lt_pipeline::removal::{forget, resolve};
use lt_pipeline::{ImageKey, Inpainter, RenderRequest};

fn frame() -> RawImage {
    let (w, h) = (9504u32, 6336u32);
    RawImage {
        size: Size::new(w, h),
        cfa: CfaPattern::Rggb,
        data: (0..w * h)
            .map(|i| (2000 + (i.wrapping_mul(2_654_435_761) >> 20) % 400) as u16)
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

/// `LaMa`'s working size, returning gray (the Neural Engine's time is not counted).
struct Gray800;

impl Inpainter for Gray800 {
    fn id(&self) -> String {
        "gray-800".into()
    }
    fn max_edge(&self) -> u32 {
        800
    }
    fn inpaint(&self, w: u32, h: u32, rgb: &[u8], _mask: &[u8]) -> Option<Vec<u8>> {
        assert!(w.max(h) <= 800);
        Some(vec![128; rgb.len()])
    }
}

fn rss_bytes() -> u64 {
    std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

/// Run `f`, sampling the process's RSS every few ms: its result, the peak and the time.
fn measured<T>(f: impl FnOnce() -> T) -> (T, u64, Duration) {
    let done = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicU64::new(rss_bytes()));
    let sampler = {
        let (done, peak) = (done.clone(), peak.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                peak.fetch_max(rss_bytes(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let t = Instant::now();
    let v = f();
    let secs = t.elapsed();
    peak.fetch_max(rss_bytes(), Ordering::Relaxed);
    done.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    (v, peak.load(Ordering::Relaxed), secs)
}

fn stroke(points: Vec<[f32; 2]>, size: f32) -> Adjustments {
    Adjustments {
        removals: vec![Removal {
            strokes: vec![BrushStroke {
                points,
                size,
                ..BrushStroke::default()
            }],
            feather: 0.002,
        }],
        ..Adjustments::default()
    }
}

fn line(a: [f32; 2], b: [f32; 2]) -> Vec<[f32; 2]> {
    (0..=240)
        .map(|i| {
            let t = i as f32 / 240.0;
            [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
        })
        .collect()
}

#[test]
#[cfg_attr(debug_assertions, ignore = "61 MP: release builds only")]
fn large_removals_on_61_mp_stay_under_1_gb() {
    let raw = frame();
    let req = RenderRequest {
        inpainter: Some(Arc::new(Gray800)),
        ..RenderRequest::new(Size::new(1024, 1024))
    };
    let cases = [
        (
            "diagonal 10%",
            stroke(line([0.02, 0.02], [0.98, 0.98]), 0.1),
        ),
        (
            "diagonal 30%",
            stroke(line([0.05, 0.05], [0.95, 0.95]), 0.3),
        ),
        (
            "horizontal 10%",
            stroke(line([0.02, 0.5], [0.98, 0.5]), 0.1),
        ),
        ("person", stroke(line([0.3, 0.15], [0.3, 0.85]), 0.158)),
        ("dot 15.5%", stroke(vec![[0.5, 0.5]], 0.155)),
        (
            "dot 10.2% (full resolution)",
            stroke(vec![[0.5, 0.5]], 0.102),
        ),
    ];
    for (i, (name, adj)) in cases.into_iter().enumerate() {
        let key = ImageKey(100 + i as u64);
        let (out, peak, secs) = measured(|| resolve(key, &raw, &adj, &req).unwrap().unwrap());
        eprintln!(
            "{name}: {:.2} s, peak RSS {} MB",
            secs.as_secs_f64(),
            peak >> 20
        );
        assert!(peak < 1 << 30, "{name}: peak RSS {} MB", peak >> 20);
        assert_ne!(out.1.data, raw.data, "{name}: filled");
        drop(out);
        forget(key);
    }
}
