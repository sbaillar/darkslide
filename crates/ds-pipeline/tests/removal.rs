//! Content-aware removal on a real A9 frame: four people removed from above the waterfall
//! (`SKB06204.ARW`), `PatchMatch` (no model). Skips without `samples/`.
//!
//! `DS_WRITE_PNG=<dir>` writes the frame before/after and 1:1 crops for inspection.
//! `DS_INPAINT_CMD=<exe>` fills with an external model instead: it is run as
//! `<exe> <in.rgb> <in.mask> <w> <h> <out.rgb>` (raw RGB8 / 8-bit mask files), e.g. a
//! Core ML `LaMa` helper; `DS_INPAINT_EDGE` is its working size (default 800).

#![allow(clippy::too_many_lines)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ds_core::adjust::{BrushStroke, Removal};
use ds_core::{Adjustments, NormRect, Size};
use ds_pipeline::{ImageKey, Inpainter, RenderRequest, Renderer, cpu_renderer};

/// Strokes over the four people above the falls (E-normalized; brush size as a share of
/// the long edge), traced on a 1500 px render. `DS_REMOVAL_STROKES` replaces them.
fn swimmers() -> Vec<Removal> {
    let stroke = |pts: &[[f32; 2]], size: f32| BrushStroke {
        points: pts.to_vec(),
        size,
        hardness: 1.0,
        ..BrushStroke::default()
    };
    let spec = std::env::var("DS_REMOVAL_STROKES").ok();
    if let Some(spec) = spec {
        // Removals split by ';', their strokes by '|': "x,y x,y ...@size|...;..."
        return spec
            .split(';')
            .map(|r| Removal {
                strokes: r
                    .split('|')
                    .map(|s| {
                        let (pts, size) = s.split_once('@').expect("pts@size");
                        let pts: Vec<[f32; 2]> = pts
                            .split_whitespace()
                            .map(|p| {
                                let (x, y) = p.split_once(',').unwrap();
                                [x.parse().unwrap(), y.parse().unwrap()]
                            })
                            .collect();
                        stroke(&pts, size.parse().unwrap())
                    })
                    .collect(),
                feather: 0.002,
            })
            .collect();
    }
    // The man on the grass, then the three on the rocks (one removal, three strokes).
    vec![
        Removal {
            strokes: vec![stroke(
                &[[0.630, 0.383], [0.628, 0.42], [0.632, 0.45], [0.636, 0.462]],
                0.045,
            )],
            feather: 0.002,
        },
        Removal {
            strokes: vec![
                stroke(&[[0.419, 0.456], [0.419, 0.50], [0.419, 0.538]], 0.04),
                stroke(&[[0.455, 0.466], [0.455, 0.50], [0.455, 0.538]], 0.035),
                stroke(&[[0.505, 0.473], [0.515, 0.50], [0.52, 0.538]], 0.05),
            ],
            feather: 0.002,
        },
    ]
}

/// An external inpainting command (see the module docs).
struct External {
    cmd: PathBuf,
    edge: u32,
    dir: PathBuf,
}

impl Inpainter for External {
    fn id(&self) -> String {
        format!("external:{}", self.cmd.display())
    }
    fn max_edge(&self) -> u32 {
        self.edge
    }
    fn inpaint(&self, w: u32, h: u32, rgb: &[u8], mask: &[u8]) -> Option<Vec<u8>> {
        let (i, m, o) = (
            self.dir.join("in.rgb"),
            self.dir.join("in.mask"),
            self.dir.join("out.rgb"),
        );
        std::fs::write(&i, rgb).ok()?;
        std::fs::write(&m, mask).ok()?;
        let t = std::time::Instant::now();
        let ok = std::process::Command::new(&self.cmd)
            .args([&i, &m])
            .arg(w.to_string())
            .arg(h.to_string())
            .arg(&o)
            .status()
            .ok()?
            .success();
        println!("external inpaint {w}x{h}: {:?}", t.elapsed());
        ok.then(|| std::fs::read(&o).ok()).flatten()
    }
}

fn crop_png(dir: &Path, name: &str, img: &ds_pipeline::RenderedImage, r: [u32; 4]) {
    let w = img.size.width;
    let mut out = Vec::with_capacity((r[2] * r[3] * 4) as usize);
    for y in r[1]..r[1] + r[3] {
        let i = ((y * w + r[0]) * 4) as usize;
        out.extend_from_slice(&img.rgba8[i..i + (r[2] * 4) as usize]);
    }
    support::write_png(&dir.join(name), r[2], r[3], &out);
}

#[test]
fn swimmers_are_removed_from_the_pool() {
    let Some(path) = ds_testdata::samples_with_ext("arw")
        .into_iter()
        .find(|p| p.file_name().is_some_and(|n| n == "SKB06204.ARW"))
    else {
        return;
    };
    let raw = support::load_raw(&path).expect("decodes");
    let capture = support::capture_info(&path);
    let out_dir = std::env::var_os("DS_WRITE_PNG").map(PathBuf::from);
    let inpainter: Option<Arc<dyn Inpainter>> = std::env::var_os("DS_INPAINT_CMD").map(|c| {
        let dir = std::env::temp_dir().join(format!("ds-inpaint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(External {
            cmd: c.into(),
            edge: std::env::var("DS_INPAINT_EDGE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(800),
            dir,
        }) as Arc<dyn Inpainter>
    });
    let mut adj = Adjustments {
        removals: swimmers(),
        ..Adjustments::default()
    };
    let plain = Adjustments::default();
    let edge: u32 = std::env::var("DS_LOOK_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500);
    let req = RenderRequest {
        capture: capture.clone(),
        inpainter: inpainter.clone(),
        ..RenderRequest::new(Size::new(edge, edge))
    };
    let mut r = cpu_renderer();
    let t = std::time::Instant::now();
    let after = r.render(ImageKey(1), &raw, &adj, &req).expect("render");
    println!("first render with removals: {:?}", t.elapsed());
    let t = std::time::Instant::now();
    let again = r.render(ImageKey(1), &raw, &adj, &req).expect("render");
    println!("cached: {:?}", t.elapsed());
    assert_eq!(after.rgba8, again.rgba8, "the cached fill is the same fill");
    let before = r.render(ImageKey(1), &raw, &plain, &req).expect("render");
    // The fill changed the strokes' area and nothing far from it.
    let (w, h) = (after.size.width, after.size.height);
    let mut changed_near = 0u64;
    let mut changed_far = 0u64;
    let strokes = &adj.removals;
    let near = |x: u32, y: u32| {
        let (u, v) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
        strokes.iter().flat_map(|r| &r.strokes).any(|s| {
            s.points.iter().any(|p| {
                ((p[0] - u) * w as f32).hypot((p[1] - v) * h as f32)
                    < (s.size * 0.5 + 0.02) * w.max(h) as f32
            })
        })
    };
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let d = (0..3)
                .map(|c| after.rgba8[i + c].abs_diff(before.rgba8[i + c]))
                .max()
                .unwrap();
            if d > 2 {
                if near(x, y) {
                    changed_near += 1;
                } else {
                    changed_far += 1;
                }
            }
        }
    }
    println!("changed: {changed_near} near, {changed_far} far");
    assert!(changed_near > 50, "the removal shows");
    assert!(
        changed_far * 200 < u64::from(w * h),
        "far pixels changed: {changed_far} (develop context, e.g. NR, may move a few)"
    );
    if let Some(dir) = out_dir {
        std::fs::create_dir_all(&dir).unwrap();
        support::write_png(&dir.join("removal_before.png"), w, h, &before.rgba8);
        support::write_png(&dir.join("removal_after.png"), w, h, &after.rgba8);
        // 1:1 crops around each removal.
        for (k, rm) in adj.removals.iter().enumerate() {
            let p = rm.strokes[0].points[0];
            let full = Size::new(raw.size.width, raw.size.height);
            let long = full.width.max(full.height) as f32;
            let side = 600.0 / long;
            let e = raw.orientation.apply_to(full);
            let (sw, sh) = (side * long / e.width as f32, side * long / e.height as f32);
            let region = NormRect {
                x: (p[0] - sw / 2.0).clamp(0.0, 1.0 - sw),
                y: (p[1] - sh / 2.0).clamp(0.0, 1.0 - sh),
                w: sw,
                h: sh,
            };
            let one = RenderRequest {
                region: Some(region),
                ..RenderRequest::new(Size::new(600, 600))
            };
            let one = RenderRequest {
                capture: capture.clone(),
                inpainter: inpainter.clone(),
                ..one
            };
            let a = r.render(ImageKey(1), &raw, &adj, &one).unwrap();
            let b = r.render(ImageKey(1), &raw, &plain, &one).unwrap();
            let s = a.size;
            crop_png(
                &dir,
                &format!("removal_{k}_after_1to1.png"),
                &a,
                [0, 0, s.width, s.height],
            );
            crop_png(
                &dir,
                &format!("removal_{k}_before_1to1.png"),
                &b,
                [0, 0, s.width, s.height],
            );
        }
    }
    adj.removals.clear();
}
