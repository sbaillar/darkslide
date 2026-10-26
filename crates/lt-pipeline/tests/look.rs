//! Renders every sample through the CPU reference and (when `LT_WRITE_PNG=<dir>` is set)
//! writes PNGs for visual inspection. Always checks basic sanity of real-data renders.

#![allow(clippy::too_many_lines, clippy::float_cmp)]

mod support;

use lt_core::{Adjustments, Size};
use lt_pipeline::{ImageKey, RenderRequest, Renderer, cpu_renderer, histogram};

#[test]
fn render_samples_sanity() {
    // One sample by default (debug builds are slow); `LT_LOOK_ALL=1` renders them all.
    let mut paths = lt_testdata::samples_with_ext("arw");
    if std::env::var_os("LT_LOOK_ALL").is_none() {
        paths.truncate(1);
    }
    let out_dir = std::env::var_os("LT_WRITE_PNG").map(std::path::PathBuf::from);
    let max: u32 = std::env::var("LT_LOOK_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1200);
    let mut r = cpu_renderer();
    for (i, p) in paths.iter().enumerate() {
        let raw = support::load_raw(p).expect("lt_raw decodes every sample");
        let adj = Adjustments::default();
        let t = std::time::Instant::now();
        let img = r
            .render(
                ImageKey(i as u64),
                &raw,
                &adj,
                &RenderRequest::new(Size::new(max, max)),
            )
            .expect("render");
        let dt = t.elapsed();
        let h = histogram(&img);
        let mean: f64 =
            h.l.iter()
                .enumerate()
                .map(|(v, n)| v as f64 * f64::from(*n))
                .sum::<f64>()
                / h.total as f64;
        println!(
            "{:?}: {:?} in {dt:?}, mean luma {mean:.1}, p50 {}, clipped hi {:.3}%",
            p.file_name().unwrap(),
            img.size,
            h.luma_percentile(0.5),
            100.0 * h.clipped_highlights as f64 / h.total as f64
        );
        assert!(mean > 20.0 && mean < 235.0, "implausible mean {mean}");
        // Strong HDR + clarity must still look like a photo (no blow-outs or crushing).
        let mut hdr_adj = Adjustments::default();
        hdr_adj.hdr.shadows = 100.0;
        hdr_adj.hdr.highlights = 100.0;
        hdr_adj.detail.clarity = 60.0;
        let req = RenderRequest::new(Size::new(max, max));
        let hdr = r
            .render(ImageKey(i as u64), &raw, &hdr_adj, &req)
            .expect("render");
        let hh = histogram(&hdr);
        assert!(hh.clipped_highlights <= h.clipped_highlights.max(hh.total / 1000));
        if let Some(dir) = &out_dir {
            let stem = p.file_stem().unwrap().to_string_lossy();
            for (suffix, im) in [("", &img), ("_hdr", &hdr)] {
                support::write_png(
                    &dir.join(format!("{stem}{suffix}.png")),
                    im.size.width,
                    im.size.height,
                    &im.rgba8,
                );
            }
        }
    }
}

/// Switching from as-shot to Custom at the as-shot temperature/tint must not change the
/// picture, and a daylight Custom WB must look neutral-ish (not green) on real files.
#[test]
fn custom_wb_matches_as_shot_on_real_files() {
    let Some(raw) = support::first_sample() else {
        return;
    };
    let (t, tint) = lt_pipeline::color::wb_temp_tint(lt_core::adjust::WhiteBalance::AsShot, &raw);
    assert!(
        (3000.0..9000.0).contains(&t) && tint.abs() < 50.0,
        "as-shot reads {t} K / {tint}"
    );
    let m = lt_pipeline::color::wb_to_multipliers(
        lt_core::adjust::WhiteBalance::Custom { temp_k: t, tint },
        &raw,
    );
    for c in 0..3 {
        let a = raw.wb_as_shot[c] / raw.wb_as_shot[1];
        assert!(
            (m[c] - a).abs() / a < 0.005,
            "{m:?} vs {:?}",
            raw.wb_as_shot
        );
    }
    let adj = Adjustments {
        white_balance: lt_core::adjust::WhiteBalance::Custom {
            temp_k: 5500.0,
            tint: 0.0,
        },
        ..Adjustments::default()
    };
    let img = cpu_renderer()
        .render(
            ImageKey(1),
            &raw,
            &adj,
            &RenderRequest::new(Size::new(600, 600)),
        )
        .unwrap();
    let n = f64::from(img.size.width * img.size.height);
    let mean = |c: usize| {
        img.rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| f64::from(p[c]))
            .sum::<f64>()
            / n
    };
    let (r, g, b) = (mean(0), mean(1), mean(2));
    assert!(
        g < 1.35 * r.max(b),
        "green cast at 5500 K: {r:.0},{g:.0},{b:.0}"
    );
}

/// Mean linear luminance (Display P3 weights on decoded values; the Apple comparison is
/// in P3) and mean linear R/G, B/G ratios.
fn stats(rgba: &[u8]) -> (f64, f64, f64) {
    let dec = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (mut y, mut r, mut g, mut b) = (0.0, 0.0, 0.0, 0.0);
    let n = (rgba.len() / 4) as f64;
    for p in rgba.as_chunks::<4>().0 {
        let (lr, lg, lb) = (dec(p[0]), dec(p[1]), dec(p[2]));
        y += 0.228_975 * lr + 0.691_739 * lg + 0.079_287 * lb;
        r += lr;
        g += lg;
        b += lb;
    }
    (y / n, r / g, b / g)
}

/// Rotate/flip a stored-orientation RGBA image into display orientation (`sips` writes
/// the RAW's pixels unrotated).
fn to_display(rgba: &[u8], w: usize, h: usize, o: lt_core::Orientation) -> (Vec<u8>, usize, usize) {
    use lt_core::Orientation as O;
    let swap = matches!(o, O::Rotate90 | O::Rotate270 | O::Transpose | O::Transverse);
    let (dw, dh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u8; rgba.len()];
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = match o {
                O::Normal => (x, y),
                O::FlipH => (w - 1 - x, y),
                O::Rotate180 => (w - 1 - x, h - 1 - y),
                O::FlipV => (x, h - 1 - y),
                O::Transpose => (y, x),
                O::Rotate90 => (h - 1 - y, x),
                O::Transverse => (h - 1 - y, w - 1 - x),
                O::Rotate270 => (y, w - 1 - x),
            };
            let (s, d) = ((y * w + x) * 4, (dy * dw + dx) * 4);
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    (out, dw, dh)
}

/// 4×4 box average (in linear light) of an RGBA8 image.
fn shrink4(img: &[u8], w: usize, h: usize) -> Vec<u8> {
    let dec = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let enc = |l: f64| {
        let c = if l <= 0.003_130_8 {
            l * 12.92
        } else {
            1.055 * l.powf(1.0 / 2.4) - 0.055
        };
        (c * 255.0).round().clamp(0.0, 255.0) as u8
    };
    let mut out = Vec::new();
    for by in 0..h / 4 {
        for bx in 0..w / 4 {
            let mut acc = [0.0f64; 3];
            for y in by * 4..by * 4 + 4 {
                for x in bx * 4..bx * 4 + 4 {
                    for c in 0..3 {
                        acc[c] += dec(img[(y * w + x) * 4 + c]);
                    }
                }
            }
            out.extend(acc.map(|v| enc(v / 16.0)));
            out.push(255);
        }
    }
    out
}

/// White balance on near-neutral pixels: mean linear R/G and B/G of `a` and `b` over the
/// pixels where `b` (the reference) is within 12 % of gray and mid-bright. Returns
/// (a R/G, a B/G, b R/G, b B/G, pixel count).
fn neutral_wb(a: &[u8], b: &[u8]) -> (f64, f64, f64, f64, usize) {
    let dec = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (mut s, mut n) = ([0.0f64; 6], 0usize);
    for (p, q) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        let qb = [dec(q[0]), dec(q[1]), dec(q[2])];
        let (lo, hi) = (qb[0].min(qb[1]).min(qb[2]), qb[0].max(qb[1]).max(qb[2]));
        if !(lo > 0.05 && hi < 0.8 && hi < lo * 1.12) {
            continue;
        }
        let pb = [dec(p[0]), dec(p[1]), dec(p[2])];
        for c in 0..3 {
            s[c] += pb[c];
            s[3 + c] += qb[c];
        }
        n += 1;
    }
    (s[0] / s[1], s[2] / s[1], s[3] / s[4], s[5] / s[4], n)
}

/// Luminance EV difference per cell of a 4×4 grid (images resized to a common grid by
/// cell averaging), so framing differences of a few pixels don't matter.
fn grid_ev(a: &[u8], aw: usize, ah: usize, b: &[u8], bw: usize, bh: usize) -> Vec<f64> {
    let cell = |img: &[u8], w: usize, h: usize, cx: usize, cy: usize| {
        let (x0, x1) = (cx * w / 4, (cx + 1) * w / 4);
        let (y0, y1) = (cy * h / 4, (cy + 1) * h / 4);
        let mut px = Vec::with_capacity((x1 - x0) * (y1 - y0) * 4);
        for y in y0..y1 {
            px.extend_from_slice(&img[(y * w + x0) * 4..(y * w + x1) * 4]);
        }
        stats(&px).0
    };
    (0..16)
        .map(|i| (cell(a, aw, ah, i % 4, i / 4) / cell(b, bw, bh, i % 4, i / 4)).log2())
        .collect()
}

/// Apple's RAW engine (via `sips`) as a reference for the Neutral look. `sips` writes
/// Display P3 with the RAW's pixels unrotated, so ours is rendered in P3 and Apple's is
/// rotated into display orientation. The mean luminance must agree within 0.1 EV on
/// every sample and the white balance (R/G, B/G over near-neutral areas) within 2 %
/// wherever there are enough neutral areas; the whole-image R/G, B/G and a 4×4 grid of
/// luminance differences are reported (they differ by look, not white point).
#[test]
fn default_look_matches_apple_brightness() {
    let paths = lt_testdata::samples_with_ext("arw");
    let take = if std::env::var_os("LT_LOOK_ALL").is_some() {
        paths.len()
    } else {
        3
    };
    let dir = std::env::temp_dir().join(format!("lt-look-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut r = cpu_renderer();
    for (i, p) in paths.iter().take(take).enumerate() {
        let apple = dir.join(format!("apple_{i}.png"));
        let ok = std::process::Command::new("/usr/bin/sips")
            .args(["-s", "format", "png", "-Z", "600"])
            .arg(p)
            .arg("--out")
            .arg(&apple)
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("sips unavailable; skipping Apple comparison");
            return;
        }
        let decoder = png::Decoder::new(std::io::BufReader::new(
            std::fs::File::open(&apple).unwrap(),
        ));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        let rgba: Vec<u8> = match info.color_type {
            png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
            png::ColorType::Rgb => buf[..info.buffer_size()]
                .chunks(3)
                .flat_map(|c| [c[0], c[1], c[2], 255])
                .collect(),
            other => panic!("unexpected sips output {other:?}"),
        };
        let raw = support::load_raw(p).unwrap();
        // `sips` writes Display P3 (tagged): compare P3 with P3.
        let mut req = RenderRequest::new(Size::new(600, 600));
        req.capture = support::capture_info(p);
        req.output = lt_pipeline::OutputSpace::DisplayP3;
        // Neutral is the Apple-matched look (Standard stays within ±0.25 EV of it, see
        // `profile_sheets`).
        let neutral = Adjustments {
            profile: lt_core::adjust::Profile::Neutral,
            ..Adjustments::default()
        };
        let ours = r
            .render(ImageKey(100 + i as u64), &raw, &neutral, &req)
            .unwrap();
        let raw_o = raw.orientation;
        let (rgba, aw, ah) = to_display(&rgba, info.width as usize, info.height as usize, raw_o);
        let (ya, ra, ba) = stats(&rgba);
        let (yo, ro, bo) = stats(&ours.rgba8);
        let ev = (yo / ya).log2();
        let grid = grid_ev(
            &ours.rgba8,
            ours.size.width as usize,
            ours.size.height as usize,
            &rgba,
            aw,
            ah,
        );
        let worst = grid.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        // 4×4 box-averaged (linear) so a pixel or two of framing difference doesn't mix
        // colored pixels into the neutral set.
        let (ow, oh) = (ours.size.width as usize, ours.size.height as usize);
        let (nr, nb, ar, ab, nn) =
            neutral_wb(&shrink4(&ours.rgba8, ow, oh), &shrink4(&rgba, aw, ah));
        println!(
            "  neutral pixels ({nn}): R/G {nr:.3} vs {ar:.3} ({:+.1} %), B/G {nb:.3} vs {ab:.3} ({:+.1} %)",
            (nr / ar - 1.0) * 100.0,
            (nb / ab - 1.0) * 100.0
        );
        println!(
            "  4x4 grid EV: {} (worst {worst:.2})",
            grid.iter()
                .map(|v| format!("{v:+.2}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!(
            "{:?}: ours-apple {ev:+.3} EV, R/G {ro:.3} vs {ra:.3}, B/G {bo:.3} vs {ba:.3} ({:?} vs {aw}x{ah})",
            p.file_name().unwrap(),
            ours.size,
        );
        if let Some(out) = std::env::var_os("LT_WRITE_PNG").map(std::path::PathBuf::from) {
            let stem = p.file_stem().unwrap().to_string_lossy();
            let _ = std::fs::copy(&apple, out.join(format!("apple_{stem}.png")));
            support::write_png(
                &out.join(format!("ours_{stem}.png")),
                ours.size.width,
                ours.size.height,
                &ours.rgba8,
            );
        }
        assert!(
            ev.abs() <= 0.1,
            "{p:?}: mean luminance differs from Apple by {ev:+.3} EV"
        );
        // White balance where there is enough neutral content to measure it.
        if nn >= 500 {
            assert!(
                (nr / ar - 1.0).abs() <= 0.02 && (nb / ab - 1.0).abs() <= 0.02,
                "{p:?}: neutral R/G {nr:.3} vs {ar:.3}, B/G {nb:.3} vs {ab:.3}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hue (HSV degrees) of the mean sRGB color of a normalized region.
fn region_hue(img: &lt_pipeline::RenderedImage, r: [f32; 4]) -> f32 {
    let (w, h) = (img.size.width as usize, img.size.height as usize);
    let (x0, y0) = ((r[0] * w as f32) as usize, (r[1] * h as f32) as usize);
    let (x1, y1) = (
        ((r[0] + r[2]) * w as f32) as usize,
        ((r[1] + r[3]) * h as f32) as usize,
    );
    let mut s = [0.0f32; 3];
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * w + x) * 4;
            for (acc, v) in s.iter_mut().zip(&img.rgba8[i..i + 3]) {
                *acc += f32::from(*v);
            }
            n += 1.0;
        }
    }
    let [r, g, b] = s.map(|v| v / n);
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let d = (mx - mn).max(1e-6);
    let hue = if mx == r {
        60.0 * ((g - b) / d)
    } else if mx == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    hue.rem_euclid(360.0)
}

/// Brightening keeps the sky's hue (the tone curve is hue-preserving): SKB08229's sky
/// shifted 7-9° toward cyan at +2 EV with a per-channel curve.
#[test]
fn sky_hue_holds_when_brightening() {
    let Some(p) = lt_testdata::samples_with_ext("arw")
        .into_iter()
        .find(|p| p.file_stem().is_some_and(|s| s == "SKB08229"))
    else {
        return;
    };
    let raw = support::load_raw(&p).unwrap();
    let mut req = RenderRequest::new(Size::new(900, 900));
    req.capture = support::capture_info(&p);
    let mut r = cpu_renderer();
    let sky = [0.05, 0.1, 0.25, 0.25];
    let hue = |ev: f32, r: &mut lt_pipeline::CpuRenderer| {
        let mut a = Adjustments::default();
        a.exposure.exposure_ev = ev;
        region_hue(&r.render(ImageKey(1), &raw, &a, &req).unwrap(), sky)
    };
    let (h0, h1, h2) = (hue(0.0, &mut r), hue(1.0, &mut r), hue(2.0, &mut r));
    println!("sky hue: 0 EV {h0:.1}°, +1 EV {h1:.1}°, +2 EV {h2:.1}°");
    assert!(
        (h2 - h0).abs() <= 3.0 && (h1 - h0).abs() <= 3.0,
        "{h0} {h1} {h2}"
    );
}

/// Neutral | Standard | Vivid side by side per sample (written as `profiles_<stem>.png`
/// when `LT_WRITE_PNG=<dir>` is set; `LT_LOOK_ALL=1` for every sample), plus 1:1 crops
/// of `LT_CROP=<stem>:<x>,<y>,<w>,<h>` (normalized) as `crop_<stem>.png`. Always checks
/// that Standard keeps the mean brightness near Neutral's (±0.25 EV) and adds contrast.
#[test]
fn profile_sheets() {
    use lt_core::adjust::Profile;
    let mut paths = lt_testdata::samples_with_ext("arw");
    if std::env::var_os("LT_LOOK_ALL").is_none() {
        paths.truncate(1);
    }
    let out = std::env::var_os("LT_WRITE_PNG").map(std::path::PathBuf::from);
    let crop = std::env::var("LT_CROP").ok();
    let size: u32 = std::env::var("LT_LOOK_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900);
    let mut r = cpu_renderer();
    for (i, p) in paths.iter().enumerate() {
        let raw = support::load_raw(p).unwrap();
        let stem = p.file_stem().unwrap().to_string_lossy().to_string();
        let mut req = RenderRequest::new(Size::new(size, size));
        req.capture = support::capture_info(p);
        let render = |r: &mut lt_pipeline::CpuRenderer, prof: Profile, req: &RenderRequest| {
            let adj = Adjustments {
                profile: prof,
                ..Adjustments::default()
            };
            r.render(ImageKey(i as u64), &raw, &adj, req).unwrap()
        };
        let imgs: Vec<_> = [Profile::Neutral, Profile::Standard, Profile::Vivid]
            .into_iter()
            .map(|prof| render(&mut r, prof, &req))
            .collect();
        let (yn, _, _) = stats(&imgs[0].rgba8);
        let (ys, _, _) = stats(&imgs[1].rgba8);
        let spread = |im: &lt_pipeline::RenderedImage| {
            let h = histogram(im);
            f64::from(h.luma_percentile(0.9)) - f64::from(h.luma_percentile(0.1))
        };
        let ev = (ys / yn).log2();
        println!(
            "{stem}: Standard vs Neutral {ev:+.3} EV, p10–p90 spread {:.0} → {:.0} (Vivid {:.0})",
            spread(&imgs[0]),
            spread(&imgs[1]),
            spread(&imgs[2])
        );
        assert!(
            ev.abs() <= 0.25,
            "{stem}: Standard moved brightness {ev:+.3} EV"
        );
        assert!(
            spread(&imgs[1]) >= spread(&imgs[0]),
            "{stem}: Standard is flatter"
        );
        let Some(dir) = &out else { continue };
        let (w, h) = (imgs[0].size.width as usize, imgs[0].size.height as usize);
        let gap = 8;
        let sw = 3 * w + 2 * gap;
        let mut sheet = vec![255u8; sw * h * 4];
        for (k, im) in imgs.iter().enumerate() {
            for y in 0..h {
                let d = (y * sw + k * (w + gap)) * 4;
                sheet[d..d + w * 4].copy_from_slice(&im.rgba8[y * w * 4..(y + 1) * w * 4]);
            }
        }
        support::write_png(
            &dir.join(format!("profiles_{stem}.png")),
            sw as u32,
            h as u32,
            &sheet,
        );
        // 1:1-ish crops of a region (skin), Neutral | Standard | Vivid.
        let Some((cs, rect)) = crop.as_deref().and_then(|c| c.split_once(':')) else {
            continue;
        };
        if cs != stem {
            continue;
        }
        let v: Vec<f32> = rect.split(',').map(|x| x.parse().unwrap()).collect();
        let mut creq = RenderRequest::new(Size::new(6000, 6000));
        creq.capture = req.capture.clone();
        creq.region = Some(lt_core::NormRect {
            x: v[0],
            y: v[1],
            w: v[2],
            h: v[3],
        });
        let crops: Vec<_> = [Profile::Neutral, Profile::Standard, Profile::Vivid]
            .into_iter()
            .map(|prof| render(&mut r, prof, &creq))
            .collect();
        let (cw, ch) = (crops[0].size.width as usize, crops[0].size.height as usize);
        let csw = 3 * cw + 2 * gap;
        let mut cs = vec![255u8; csw * ch * 4];
        for (k, im) in crops.iter().enumerate() {
            for y in 0..ch {
                let d = (y * csw + k * (cw + gap)) * 4;
                cs[d..d + cw * 4].copy_from_slice(&im.rgba8[y * cw * 4..(y + 1) * cw * 4]);
            }
        }
        support::write_png(
            &dir.join(format!("crop_{stem}.png")),
            csw as u32,
            ch as u32,
            &cs,
        );
    }
}
