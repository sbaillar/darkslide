//! White balance is applied after the demosaic (the BASE is at the as-shot WB). A large
//! WB change must render like a demosaic at the final WB, clipped highlights included
//! (ported from the review probes `q_wbzoom` / `q_wbhl`). Full-resolution renders of the
//! real samples; skips without samples or a GPU.
//!
//! Measured over highlights (SKB06204, −2000 K): ΔE mean 0.78, p99 3.9, max 27. The
//! residual is RCD's white-balance-dependent directional weighting on noisy texture near
//! highlights, not clipping: switching clipped neighbourhoods to a (WB-commuting)
//! bilinear demosaic changed it by < 0.03 mean and not at all at the max, so it was not
//! adopted. Removing it entirely needs a re-demosaic at the final WB.

mod support;

use lt_core::adjust::WhiteBalance;
use lt_core::{Adjustments, Size};
use lt_pipeline::color::{
    CameraColor, delta_e2000, multipliers_to_wb, srgb8_to_lab, temp_tint_to_multipliers,
};
use lt_pipeline::{ImageKey, RenderRequest, Renderer, gpu_renderer};

/// ΔE2000 (mean, p99, max, share > 5) over highlights: pixels whose reference has a
/// channel ≥ 200 (and their neighbours), where clipping lives. Elsewhere the two paths
/// differ only by the demosaic's response to noise at different channel scalings.
fn stats(a: &[u8], b: &[u8], w: usize) -> (f32, f32, f32, f32) {
    let n = a.len() / 4;
    let bright = |i: usize| b[i * 4..i * 4 + 3].iter().any(|v| *v >= 200);
    let mut d: Vec<f32> = (0..n)
        .filter(|&i| {
            let (x, y) = (i % w, i / w);
            (y.saturating_sub(2)..(y + 3).min(n / w))
                .any(|yy| (x.saturating_sub(2)..(x + 3).min(w)).any(|xx| bright(yy * w + xx)))
        })
        .map(|i| {
            let (p, q) = (&a[i * 4..i * 4 + 3], &b[i * 4..i * 4 + 3]);
            delta_e2000(
                srgb8_to_lab([p[0], p[1], p[2]]),
                srgb8_to_lab([q[0], q[1], q[2]]),
            )
        })
        .collect();
    if d.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let above5 = d.iter().filter(|v| **v > 5.0).count() as f32 / d.len() as f32;
    let mean = d.iter().sum::<f32>() / d.len() as f32;
    d.sort_by(f32::total_cmp);
    (mean, d[d.len() * 99 / 100], d[d.len() - 1], above5)
}

#[test]
fn large_wb_changes_match_a_demosaic_at_the_final_wb() {
    let Ok(mut g) = gpu_renderer() else { return };
    let samples: Vec<_> = lt_testdata::samples_with_ext("arw")
        .into_iter()
        .filter(|p| {
            p.file_stem()
                .is_some_and(|s| s == "SKB06204" || s == "SKB06368")
        })
        .collect();
    let mut key = 0;
    for path in samples {
        let raw = support::load_raw(&path).unwrap();
        let req = RenderRequest {
            capture: support::capture_info(&path),
            ..RenderRequest::new(Size::new(8000, 8000))
        };
        let cc = CameraColor::of(&raw);
        let WhiteBalance::Custom {
            temp_k: t0,
            tint: i0,
        } = multipliers_to_wb(raw.wb_as_shot, &cc)
        else {
            unreachable!()
        };
        let mut plain = Adjustments::default();
        plain.noise.luminance = 0.0;
        plain.noise.color = 0.0;
        plain.detail.sharpen_amount = 0.0;
        for dt in [-2000.0f32, 2000.0] {
            let t = t0 + dt;
            let mut adj = plain.clone();
            adj.white_balance = WhiteBalance::Custom {
                temp_k: t,
                tint: i0,
            };
            key += 2;
            let product = g.render(ImageKey(key), &raw, &adj, &req).unwrap();
            let mut at_final = raw.clone();
            at_final.wb_as_shot = temp_tint_to_multipliers(t, i0, &cc);
            let reference = g
                .render(ImageKey(key + 1), &at_final, &plain, &req)
                .unwrap();
            g.evict(ImageKey(key));
            g.evict(ImageKey(key + 1));
            let (mean, p99, max, above5) = stats(
                &product.rgba8,
                &reference.rgba8,
                product.size.width as usize,
            );
            println!(
                "{:?} {dt:+} K: ΔE mean {mean:.3} p99 {p99:.2} max {max:.1}, > 5: {:.4} %",
                path.file_stem().unwrap(),
                above5 * 100.0
            );
            // Regression guard at the measured level (see the module doc).
            assert!(mean < 1.0 && p99 < 5.0, "{path:?} {dt}: {mean} {p99}");
        }
    }
}
