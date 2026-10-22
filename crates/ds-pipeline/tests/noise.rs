//! ISO-adaptive noise reduction on real files (ported from the review probe `q_nr`):
//! base-ISO files keep their detail, high-ISO files lose visible chroma noise at
//! default settings. Skips without samples. `DS_WRITE_PNG=<dir>` saves the crops
//! (default | NR off | max) side by side.

mod support;

use ds_core::{Adjustments, NormRect, Size};
use ds_pipeline::{ImageKey, RenderRequest, RenderedImage, Renderer, cpu_renderer};

/// High-pass (pixel − 3×3 mean) standard deviation of luma and chroma, 8-bit units.
fn noise(img: &RenderedImage) -> (f64, f64) {
    let (w, h) = (img.size.width as usize, img.size.height as usize);
    let px = |x: usize, y: usize, c: usize| f64::from(img.rgba8[(y * w + x) * 4 + c]);
    let (mut sl, mut sc, mut n) = (0.0, 0.0, 0.0);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let mut d = [0.0; 3];
            for (c, dc) in d.iter_mut().enumerate() {
                let m: f64 = (0..9)
                    .map(|k| px(x + k % 3 - 1, y + k / 3 - 1, c))
                    .sum::<f64>()
                    / 9.0;
                *dc = px(x, y, c) - m;
            }
            let l = (d[0] + d[1] + d[2]) / 3.0;
            sl += l * l;
            sc += (d[0] - l).powi(2) + (d[2] - l).powi(2);
            n += 1.0;
        }
    }
    ((sl / n).sqrt(), (sc / n / 2.0).sqrt())
}

fn save_strip(name: &str, imgs: &[RenderedImage]) {
    let Some(dir) = std::env::var_os("DS_WRITE_PNG").map(std::path::PathBuf::from) else {
        return;
    };
    let (w, h) = (imgs[0].size.width, imgs[0].size.height);
    let total = w * imgs.len() as u32 + 4 * (imgs.len() as u32 - 1);
    let mut out = vec![255u8; (total * h * 4) as usize];
    for (k, img) in imgs.iter().enumerate() {
        let x0 = k as u32 * (w + 4);
        for y in 0..h {
            let src = &img.rgba8[(y * w * 4) as usize..((y + 1) * w * 4) as usize];
            let d = ((y * total + x0) * 4) as usize;
            out[d..d + src.len()].copy_from_slice(src);
        }
    }
    std::fs::create_dir_all(&dir).unwrap();
    support::write_png(&dir.join(format!("{name}.png")), total, h, &out);
}

/// (default, off, max) (luma, chroma) noise of a 1:1 crop.
fn measure(path: &std::path::Path, region: NormRect, tag: &str) -> [(f64, f64); 3] {
    measure_iso(path, region, tag, None)
}

/// [`measure`] with the ISO the request claims overridden.
fn measure_iso(
    path: &std::path::Path,
    region: NormRect,
    tag: &str,
    iso: Option<u32>,
) -> [(f64, f64); 3] {
    let raw = support::load_raw(path).expect("decode");
    let mut capture = support::capture_info(path);
    if iso.is_some() {
        capture.iso = iso;
    }
    let req = RenderRequest {
        region: Some(region),
        capture,
        ..RenderRequest::new(Size::new(10000, 10000))
    };
    let mut r = cpu_renderer();
    let mut imgs = Vec::new();
    let mut out = [(0.0, 0.0); 3];
    for (i, (l, c)) in [
        (None, None),
        (Some(0.0), Some(0.0)),
        (Some(100.0), Some(100.0)),
    ]
    .into_iter()
    .enumerate()
    {
        let mut a = Adjustments::default();
        if let (Some(l), Some(c)) = (l, c) {
            a.noise.luminance = l;
            a.noise.color = c;
        }
        let img = r.render(ImageKey(1), &raw, &a, &req).unwrap();
        out[i] = noise(&img);
        imgs.push(img);
    }
    println!(
        "{tag} iso {:?}: default {:.2}/{:.2}, off {:.2}/{:.2}, max {:.2}/{:.2} (luma/chroma sd)",
        req.capture.iso, out[0].0, out[0].1, out[1].0, out[1].1, out[2].0, out[2].1
    );
    save_strip(&format!("nr_{tag}"), &imgs);
    out
}

fn sample(name: &str) -> Option<std::path::PathBuf> {
    ds_testdata::samples_with_ext("arw")
        .into_iter()
        .find(|p| p.file_stem().is_some_and(|s| s == name))
}

#[test]
fn high_iso_chroma_noise_is_gone_at_default_settings() {
    // SKB06204: ISO 4000 (rock and spray crops).
    let Some(p) = sample("SKB06204") else { return };
    for (tag, region) in [
        (
            "spray",
            NormRect {
                x: 0.3,
                y: 0.9,
                w: 0.1,
                h: 0.07,
            },
        ),
        (
            "rock",
            NormRect {
                x: 0.55,
                y: 0.62,
                w: 0.1,
                h: 0.07,
            },
        ),
    ] {
        for iso in [800u32, 3200, 6400] {
            let [d, o, _] = measure_iso(&p, region, tag, Some(iso));
            println!(
                "  {tag} claimed ISO {iso}: default chroma {:.2} (off {:.2})",
                d.1, o.1
            );
        }
        let [def, off, max] = measure(&p, region, tag);
        // Default chroma noise is close to the maximum setting's, well below NR off.
        assert!(
            def.1 <= max.1 + 0.35,
            "{tag}: default chroma {} vs max {}",
            def.1,
            max.1
        );
        assert!(
            def.1 < 0.6 * off.1,
            "{tag}: default chroma {} vs off {}",
            def.1,
            off.1
        );
        // Luma NR is moderate: less noise than off, more texture than max.
        assert!(
            def.0 < off.0 && def.0 > max.0,
            "{tag}: luma {def:?} {off:?} {max:?}"
        );
    }
}

#[test]
fn base_iso_keeps_detail() {
    // SKB08229: ISO 200 — default NR barely touches it.
    let Some(p) = sample("SKB08229") else { return };
    let [def, off, _] = measure(
        &p,
        NormRect {
            x: 0.55,
            y: 0.55,
            w: 0.1,
            h: 0.1,
        },
        "base_iso",
    );
    assert!(
        def.0 > 0.95 * off.0,
        "luma detail kept: {} vs {}",
        def.0,
        off.0
    );
}

/// Clean, subtle red/green checkers of similar luma (ported from the review probe
/// `q_nrsyn`, `SUBTLE`): periods 2–64 px.
fn subtle_checkers() -> ds_core::RawImage {
    let (w, h) = (2400u32, 600u32);
    let wb = [2.4f32, 1.0, 1.6];
    let periods = [2u32, 4, 8, 16, 32, 64];
    let (red, green) = ([0.14f32, 0.11, 0.10], [0.10f32, 0.125, 0.10]);
    let mut raw = support::synthetic_raw(4, 4);
    raw.size = Size::new(w, h);
    raw.cfa = ds_core::CfaPattern::LinearRgb;
    raw.black_level = [0.0; 4];
    raw.white_level = 65535.0;
    raw.wb_as_shot = wb;
    raw.default_crop = None;
    raw.data = vec![0u16; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let half = (periods[(x / 400) as usize] / 2).max(1);
            let c = if ((x / half) + (y / half)).is_multiple_of(2) {
                red
            } else {
                green
            };
            for k in 0..3 {
                raw.data[((y * w + x) * 3 + k) as usize] =
                    (c[k as usize] / wb[k as usize] * 65535.0) as u16;
            }
        }
    }
    raw
}

/// a* standard deviation per checker band.
fn band_a_std(img: &RenderedImage) -> Vec<f64> {
    let w = img.size.width;
    (0..6u32)
        .map(|i| {
            let (mut n, mut m, mut q) = (0.0f64, 0.0f64, 0.0f64);
            for y in 100..500u32 {
                for x in (i * 400 + 50)..(i * 400 + 350) {
                    let o = ((y * w + x) * 4) as usize;
                    let lab = ds_pipeline::color::srgb8_to_lab([
                        img.rgba8[o],
                        img.rgba8[o + 1],
                        img.rgba8[o + 2],
                    ]);
                    let a = f64::from(lab[1]);
                    n += 1.0;
                    m += a;
                    q += a * a;
                }
            }
            (q / n - (m / n).powi(2)).max(0.0).sqrt()
        })
        .collect()
}

/// High-ISO chroma NR keeps real colour texture: clean subtle checkers at ISO 6400 keep
/// ≥ 75 % of their NR-off chroma variance at every period.
#[test]
fn chroma_nr_keeps_subtle_colour_texture_at_high_iso() {
    let raw = subtle_checkers();
    let mut r = cpu_renderer();
    let render = |r: &mut ds_pipeline::CpuRenderer, iso: u32, color: Option<f32>| {
        let mut a = Adjustments::default();
        if let Some(c) = color {
            a.noise.color = c;
        }
        let req = RenderRequest {
            capture: ds_pipeline::CaptureInfo {
                iso: Some(iso),
                ..Default::default()
            },
            ..RenderRequest::new(Size::new(2400, 2400))
        };
        band_a_std(&r.render(ImageKey(1), &raw, &a, &req).unwrap())
    };
    let off = render(&mut r, 6400, Some(0.0));
    for iso in [800u32, 3200, 4000, 6400] {
        let on = render(&mut r, iso, None);
        let kept: Vec<f64> = on.iter().zip(&off).map(|(a, b)| (a / b).powi(2)).collect();
        println!(
            "ISO {iso}: a* std {} (NR off {}), variance kept {}",
            on.iter()
                .map(|v| format!("{v:.1}"))
                .collect::<Vec<_>>()
                .join(" "),
            off.iter()
                .map(|v| format!("{v:.1}"))
                .collect::<Vec<_>>()
                .join(" "),
            kept.iter()
                .map(|v| format!("{:.0}%", v * 100.0))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(kept.iter().all(|k| *k >= 0.75), "ISO {iso}: {kept:?}");
    }
}

#[test]
fn raw_noise_measures_the_sensor_not_the_texture() {
    let clean = subtle_checkers();
    let n = ds_pipeline::raw_noise(&clean);
    println!("clean checkers: {n:.5}");
    assert!(n < 1e-3, "{n}");
    for p in ds_testdata::samples_with_ext("arw") {
        let raw = support::load_raw(&p).unwrap();
        println!(
            "{:?} ISO {:?}: raw noise {:.5}",
            p.file_stem().unwrap(),
            support::capture_info(&p).iso,
            ds_pipeline::raw_noise(&raw)
        );
    }
}

/// Mean chroma (a*, b*) and luma high-pass (pixel − 3×3 mean) in Lab.
fn chroma_hp(img: &RenderedImage) -> f64 {
    let (w, h) = (img.size.width as usize, img.size.height as usize);
    let lab: Vec<[f32; 3]> = img
        .rgba8
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| ds_pipeline::color::srgb8_to_lab([p[0], p[1], p[2]]))
        .collect();
    let (mut c, mut n) = (0.0, 0.0);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let mut m = [0.0f32; 3];
            for dy in 0..3 {
                for dx in 0..3 {
                    let q = lab[(y + dy - 1) * w + x + dx - 1];
                    for k in 1..3 {
                        m[k] += q[k] / 9.0;
                    }
                }
            }
            let p = lab[y * w + x];
            c += f64::from(((p[1] - m[1]).powi(2) + (p[2] - m[2]).powi(2)).sqrt());
            n += 1.0;
        }
    }
    c / n
}

/// Clean (base-ISO) frames keep their fine colour detail whatever ISO the request claims
/// (ported from the review probe `q_nr3`): chroma high-pass energy ≥ 87 % (variance
/// ≥ 75 %) of NR off at ISO 800, 3200, 4000 and 6400.
#[test]
fn clean_frames_keep_colour_detail_at_any_iso_setting() {
    for (name, cx, cy) in [("SKB08229", 0.6f32, 0.6f32), ("SKB08351", 0.5, 0.45)] {
        let Some(p) = sample(name) else { continue };
        let raw = support::load_raw(&p).unwrap();
        let cap = support::capture_info(&p);
        let mut r = cpu_renderer();
        let mut hp = |iso: u32, colour: Option<f32>| {
            let mut adj = Adjustments::default();
            if let Some(c) = colour {
                adj.noise.color = c;
            }
            let req = RenderRequest {
                capture: ds_pipeline::CaptureInfo {
                    iso: Some(iso),
                    ..cap.clone()
                },
                region: Some(NormRect {
                    x: cx - 0.05,
                    y: cy - 0.05,
                    w: 0.1,
                    h: 0.1,
                }),
                ..RenderRequest::new(Size::new(8000, 8000))
            };
            chroma_hp(&r.render(ImageKey(1), &raw, &adj, &req).unwrap())
        };
        let off = hp(6400, Some(0.0));
        for iso in [800u32, 3200, 4000, 6400] {
            let on = hp(iso, None);
            println!(
                "{name} ISO {iso}: chroma HP {on:.3} of {off:.3} NR off ({:.0}%)",
                100.0 * on / off
            );
            assert!(on >= 0.87 * off, "{name} ISO {iso}: {on} vs {off}");
        }
    }
}

/// M1: a settled view (Preview) and an export (Full) of the same size show the same
/// noise (flat ISO-4000-like field, 2400 px frame at 800 px, CPU and GPU). Before, the
/// view came from a superpixel level (one R and one B sample per 2×2 cell) sampled at a
/// step of 1.5 level pixels without averaging: chroma noise 4.90 against the export's
/// 2.81. Now it demosaics like the export and supersamples: 2.80.
#[test]
fn a_settled_view_is_as_noisy_as_the_export_of_its_size() {
    let (w, h) = (2400u32, 1600u32);
    let mut raw = support::synthetic_raw(w, h);
    let (black, white) = (raw.black_level[0], raw.white_level);
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut gauss = || {
        // Sum of four uniforms: roughly normal, σ ≈ 1.
        let mut acc = 0.0f32;
        for _ in 0..4 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            acc += (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
        }
        acc * 1.732
    };
    for v in &mut raw.data {
        let x = (0.12 + 0.02 * gauss()).clamp(0.0, 1.0);
        *v = (black + x * (white - black)) as u16;
    }
    let mut renderers: Vec<(&str, Box<dyn Renderer>)> = vec![("cpu", Box::new(cpu_renderer()))];
    if let Ok(g) = ds_pipeline::gpu_renderer() {
        renderers.push(("gpu", Box::new(g)));
    }
    for (name, mut r) in renderers {
        let view = RenderRequest::new(Size::new(800, 800));
        let export = RenderRequest {
            quality: ds_pipeline::Quality::Full,
            ..RenderRequest::new(Size::new(800, 800))
        };
        let adj = Adjustments::default();
        let v = noise(&r.render(ImageKey(1), &raw, &adj, &view).unwrap());
        let e = noise(&r.render(ImageKey(2), &raw, &adj, &export).unwrap());
        println!(
            "{name}: view luma/chroma noise {:.2}/{:.2}, export {:.2}/{:.2}",
            v.0, v.1, e.0, e.1
        );
        assert!(v.0 <= 1.15 * e.0 + 0.1, "{name} luma {} vs {}", v.0, e.0);
        assert!(v.1 <= 1.15 * e.1 + 0.1, "{name} chroma {} vs {}", v.1, e.1);
    }
}
