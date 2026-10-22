//! Auto white balance on the real samples (sky, sea, cliffs, a boat, a
//! waterfall, portraits): the camera's as-shot setting is right for all of them, so Auto
//! must stay within ±600 K / ±10 tint of it (blue-dominant frames used to go to
//! 8,400–8,600 K). CPU and GPU statistics give the same answer. Skips without samples.
//!
//! `DS_MONTAGE=<file.png>` writes an as-shot | Auto montage of every sample.

mod support;

use ds_core::adjust::WhiteBalance;
use ds_core::{Adjustments, Size};
use ds_pipeline::color::{CameraColor, multipliers_to_wb};
use ds_pipeline::{ImageKey, RenderRequest, Renderer, cpu_renderer, gpu_renderer};

fn kt(wb: WhiteBalance) -> (f32, f32) {
    match wb {
        WhiteBalance::Custom { temp_k, tint } => (temp_k, tint),
        WhiteBalance::AsShot => panic!("auto returns Custom"),
    }
}

#[test]
fn auto_white_balance_stays_near_as_shot_on_the_samples() {
    let paths = ds_testdata::samples_with_ext("arw");
    if paths.is_empty() {
        return;
    }
    let gpu = gpu_renderer().ok();
    let tile = 360u32;
    let montage = std::env::var_os("DS_MONTAGE");
    let mut sheet: Vec<(Vec<u8>, u32, u32)> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let raw = support::load_raw(p).unwrap();
        let (k0, n0) = kt(multipliers_to_wb(raw.wb_as_shot, &CameraColor::of(&raw)));
        let (kc, nc) = kt(ds_pipeline::auto::auto_white_balance(&raw));
        let name = p.file_stem().unwrap().to_string_lossy();
        print!("{name}: as-shot {k0:.0} K / {n0:.1}, auto {kc:.0} K / {nc:.1}");
        if let Some(g) = &gpu {
            let (kg, ng) = kt(g.auto_white_balance(ImageKey(500 + i as u64), &raw));
            print!(" (GPU {kg:.0} K / {ng:.1})");
            assert!(
                (kg - kc).abs() <= 50.0 && (ng - nc).abs() <= 1.0,
                "{name}: GPU {kg}/{ng} vs CPU {kc}/{nc}"
            );
        }
        println!();
        assert!(
            (kc - k0).abs() <= 600.0 && (nc - n0).abs() <= 10.0,
            "{name}: auto {kc} K / {nc} vs as-shot {k0} K / {n0}"
        );
        if montage.is_some() {
            let req = RenderRequest::new(Size::new(tile, tile));
            let mut row = Vec::new();
            for wb in [
                WhiteBalance::AsShot,
                WhiteBalance::Custom {
                    temp_k: kc,
                    tint: nc,
                },
            ] {
                let adj = Adjustments {
                    white_balance: wb,
                    ..Adjustments::default()
                };
                let img = cpu_renderer()
                    .render(ImageKey(600 + i as u64), &raw, &adj, &req)
                    .unwrap();
                row.push(img);
            }
            // Side by side, padded to tile × tile each.
            let mut buf = vec![0u8; (2 * tile * tile * 4) as usize];
            for (k, img) in row.iter().enumerate() {
                for y in 0..img.size.height.min(tile) {
                    for x in 0..img.size.width.min(tile) {
                        let s = ((y * img.size.width + x) * 4) as usize;
                        let d = ((y * 2 * tile + k as u32 * tile + x) * 4) as usize;
                        buf[d..d + 4].copy_from_slice(&img.rgba8[s..s + 4]);
                    }
                }
            }
            sheet.push((buf, 2 * tile, tile));
        }
    }
    if let Some(out) = montage {
        let (w, h) = (2 * tile, tile * sheet.len() as u32);
        let all: Vec<u8> = sheet.into_iter().flat_map(|r| r.0).collect();
        support::write_png(std::path::Path::new(&out), w, h, &all);
    }
}
