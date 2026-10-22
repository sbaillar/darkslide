//! Test/bench helpers: real A9 ARWs through `ds_raw::decode`, and synthetic mosaics
//! that use the same camera-color convention (D65 row-normalized matrix + daylight WB).
#![allow(dead_code, clippy::too_many_lines)]

use std::path::Path;

use ds_core::{CfaPattern, Orientation, RawImage, Size};

/// How much other processes slow this machine down right now: the 1-minute load
/// average per core, at least 1. Wall-clock budgets in timing tests are multiplied by
/// it, so an idle machine keeps the real budgets and a machine already loaded well past
/// its cores (other agents, llvm-cov at load 100+) doesn't fail them for work it never
/// got the CPU/GPU time to do. Stalls these tests look for (a viewer waiting for a
/// whole export or storm) are far larger than the load's effect.
pub fn load_factor() -> f64 {
    let load = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().find_map(|t| t.parse::<f64>().ok()))
        .or_else(|| {
            std::fs::read_to_string("/proc/loadavg")
                .ok()
                .and_then(|s| s.split_whitespace().next()?.parse().ok())
        })
        .unwrap_or(0.0);
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get) as f64;
    (load / cores).max(1.0)
}

/// A test image dimension: a quarter of `n` under `cargo llvm-cov` (`cfg(coverage)`,
/// where the instrumented CPU reference is ~50× slower), so the same code runs on a
/// smaller picture; `n` otherwise. Odd stays odd.
pub fn dim(n: u32) -> u32 {
    dim_at_least(n, 0)
}

/// [`dim`], but at least `min` (at most `n`): the size a test needs for the path it
/// checks, e.g. an output above the GPU tiling threshold or a draft that upscales.
pub fn dim_at_least(n: u32, min: u32) -> u32 {
    if !cfg!(coverage) {
        return n;
    }
    let d = (n / 4).max(min).min(n);
    if d % 2 == n % 2 { d } else { d + 1 }
}

/// [`synthetic_raw`] with both sides through [`dim`].
pub fn small_raw(w: u32, h: u32) -> RawImage {
    synthetic_raw(dim(w), dim(h))
}

/// Adobe `ColorMatrix2` (XYZ → camera, D65) of the Sony ILCE-9.
pub const A9_XYZ_TO_CAM: [f32; 9] = [
    0.6389, -0.1703, -0.0378, -0.4562, 1.2265, 0.2587, -0.067, 0.1489, 0.655,
];

/// Deterministic per-pixel noise in [-1, 1].
fn hash_noise(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h & 0xFFFF) as f32 / 32767.5 - 1.0
}

/// A synthetic 14-bit RGGB mosaic: gray ramp, saturated color patches, a sharp edge,
/// a clipped highlight disc and mild sensor noise. `f` returns camera-space (un-white-
/// balanced) linear values at a pixel; channel gains mimic daylight on an A9.
pub fn synthetic_raw(w: u32, h: u32) -> RawImage {
    let wb = [2.4f32, 1.0, 1.6];
    let scene = |x: u32, y: u32| -> [f32; 3] {
        let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
        let refl: [f32; 3] = if v < 0.25 {
            // Gray ramp.
            let g = 0.002 + 0.6 * u * u;
            [g; 3]
        } else if v < 0.6 {
            // Color patches.
            let patches = [
                [0.5, 0.1, 0.08],
                [0.1, 0.45, 0.1],
                [0.08, 0.12, 0.5],
                [0.55, 0.45, 0.08],
                [0.45, 0.25, 0.18],
                [0.1, 0.35, 0.45],
                [0.4, 0.1, 0.4],
                [0.18, 0.18, 0.18],
            ];
            patches[((u * 8.0) as usize).min(7)]
        } else if v < 0.8 {
            // Sharp vertical edges.
            if (x / 13).is_multiple_of(2) {
                [0.05; 3]
            } else {
                [0.5; 3]
            }
        } else {
            // Clipped highlight disc on a mid background.
            let d = ((u - 0.5) * (u - 0.5) + (v - 0.9) * (v - 0.9)).sqrt();
            if d < 0.06 { [3.0; 3] } else { [0.12; 3] }
        };
        [0, 1, 2].map(|c| refl[c] / wb[c] * 0.9 + 0.004 * hash_noise(x * 3 + c as u32, y))
    };
    let (black, white) = (512.0f32, 15_360.0f32);
    let mut data = vec![0u16; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            let c = CfaPattern::Rggb.color_at(x, y).unwrap() as usize;
            let v = scene(x, y)[c].clamp(0.0, 1.2);
            data[(y * w + x) as usize] = (black + v * (white - black)).round().min(16_383.0) as u16;
        }
    }
    RawImage {
        size: Size::new(w, h),
        cfa: CfaPattern::Rggb,
        data,
        black_level: [black; 4],
        white_level: white,
        wb_as_shot: wb,
        cam_to_xyz: ds_raw::color::cam_to_xyz(&A9_XYZ_TO_CAM).unwrap(),
        wb_daylight: ds_raw::color::daylight_multipliers(&A9_XYZ_TO_CAM).unwrap(),
        orientation: Orientation::Normal,
        default_crop: None,
    }
}

/// Decode a RAW file with the production decoder. `None` on any failure.
pub fn load_raw(path: &Path) -> Option<RawImage> {
    ds_raw::decode(path).ok()
}

/// Capture metadata (lens, focal length, aperture, ISO) the way the app passes it.
pub fn capture_info(path: &Path) -> ds_pipeline::CaptureInfo {
    let m = ds_raw::read_metadata(path).unwrap_or_default();
    ds_pipeline::CaptureInfo {
        lens: m.lens.clone(),
        focal_length: m.focal_length,
        aperture: m.aperture,
        iso: m.iso,
        crop_factor: None,
        camera: Some(format!("{} {}", m.make, m.model)),
    }
}

/// First sample decoded, if samples are present.
pub fn first_sample() -> Option<RawImage> {
    ds_testdata::sample_arw().and_then(|p| load_raw(&p))
}

/// Write an RGBA8 buffer as PNG (debug aid; used by the look-check test when
/// `DS_WRITE_PNG` is set).
pub fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().expect("png header");
    w.write_image_data(rgba).expect("png data");
}
