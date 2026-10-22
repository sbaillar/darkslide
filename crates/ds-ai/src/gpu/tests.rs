//! GPU ↔ CPU parity. Every test that needs a device skips (passes) when no
//! adapter exists.

use ds_core::NormRect;

use super::*;
use crate::testutil::{img, texture};
use crate::{NoiseConfig, SharpnessConfig, analyze_with_subject};

fn gpu(cfg: AnalysisConfig) -> Option<GpuAnalyzer> {
    match GpuAnalyzer::with_config(cfg) {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("skipping GPU test: {e}");
            None
        }
    }
}

fn roi(x: f32, y: f32, w: f32, h: f32) -> SubjectRegion {
    SubjectRegion {
        rect: NormRect { x, y, w, h },
        weight: 1.0,
    }
}

fn subject_of(subjects: &[Vec<SubjectRegion>], i: usize) -> &[SubjectRegion] {
    subjects.get(i).map_or(&[][..], Vec::as_slice)
}

/// The device path itself (no silent CPU fallback), then the public batch
/// call; both must equal the CPU reference exactly.
fn assert_parity(g: &GpuAnalyzer, images: &[LumaImage], subjects: &[Vec<SubjectRegion>]) {
    let cfg = g.config();
    let want: Vec<ImageAnalysis> = images
        .iter()
        .enumerate()
        .map(|(i, im)| analyze_with_subject(im, cfg, subject_of(subjects, i)))
        .collect();
    let direct: Vec<(usize, (&LumaImage, &[SubjectRegion]))> = images
        .iter()
        .enumerate()
        .filter(|(_, i)| !i.is_empty())
        .take(MAX_CHUNK_IMAGES)
        .map(|(i, im)| (i, (im, subject_of(subjects, i))))
        .collect();
    if !direct.is_empty() {
        let dev = g.device().expect("device");
        let tile = cfg.sharpness.tile.clamp(4, MAX_TILE);
        let chunk: Vec<_> = direct.iter().map(|&(_, c)| c).collect();
        let got = run_chunk(&dev, cfg, &chunk, tile).expect("GPU chunk");
        for (a, &(i, _)) in got.iter().zip(&direct) {
            assert_eq!(*a, want[i], "direct: image {i} {:?}", images[i]);
        }
    }
    let got = g.analyze_batch_with_subjects(images, subjects);
    assert_eq!(got.len(), images.len());
    for (i, a) in got.iter().enumerate() {
        assert_eq!(*a, want[i], "image {i}: {:?}", images[i]);
    }
}

/// Noisy texture with flat and clipped areas, so every reduction (including
/// the noise pass and the >1023 residual path) has work to do.
fn scene(w: usize, h: usize, seed: u64) -> LumaImage {
    let t = texture(w, h, seed);
    let mut s = seed | 1;
    img(w, h, |x, y| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        let n = (s % 7) as i32 - 3;
        let v = if y < h / 8 {
            255 // clipped band
        } else if x < w / 5 {
            100 + i32::from(y % 64 == 0) * 150 // flat with rare hard lines
        } else {
            i32::from(t.pixels()[y * w + x])
        };
        (v + n).clamp(0, 255) as u8
    })
}

#[test]
fn batch_matches_cpu_bit_for_bit() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let images: Vec<LumaImage> = (0..6)
        .map(|k| scene(300 + k * 37, 200 + k * 13, k as u64))
        .collect();
    assert_parity(&g, &images, &[]);
    assert!(g.is_available());
    assert_ne!(g.adapter_name().len(), 0);
    assert!(format!("{g:?}").contains("GpuAnalyzer"));
}

#[test]
fn tiny_odd_and_empty_sizes() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let sizes = [
        (0, 0),
        (1, 1),
        (2, 2),
        (3, 3),
        (1, 97),
        (97, 1),
        (5, 3),
        (47, 49),
        (48, 48),
        (49, 47),
        (65, 63),
        (0, 9),
        (1617, 7),
    ];
    let images: Vec<LumaImage> = sizes
        .iter()
        .enumerate()
        .map(|(k, &(w, h))| img(w, h, |x, y| ((x * 31 + y * 17 + k * 7) % 256) as u8))
        .collect();
    let subjects: Vec<Vec<SubjectRegion>> = images
        .iter()
        .map(|_| vec![roi(0.0, 0.0, 1.0, 1.0), roi(0.2, 0.3, 0.5, 0.4)])
        .collect();
    assert_parity(&g, &images, &[]);
    assert_parity(&g, &images, &subjects);
}

#[test]
fn batch_sizes_zero_one_and_many_chunks() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    assert_eq!(g.analyze_batch(&[]).len(), 0);
    assert_parity(&g, &[scene(160, 120, 9)], &[]);
    // A 1000-frame cull: four chunks of at most MAX_CHUNK_IMAGES.
    let many: Vec<LumaImage> = (0..1000)
        .map(|k| scene(24 + k % 9, 20 + k % 5, k as u64))
        .collect();
    assert_parity(&g, &many, &[]);
}

#[test]
fn large_residuals_on_flat_tiles_match() {
    // Flat, unclipped frames with isolated bright pixels: the flattest tiles
    // hold residuals of 16 · 100 (> 255), which bypass the workgroup bins.
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let images: Vec<LumaImage> = (0..3)
        .map(|k| {
            img(200 + k, 150, |x, y| {
                if (x * 7 + y * 13 + k) % 97 == 0 {
                    200
                } else {
                    100
                }
            })
        })
        .collect();
    let a = g.analyze_batch(&images);
    assert!(a.iter().all(|a| a.noise.sampled_fraction > 0.0));
    assert_parity(&g, &images, &[]);
}

#[test]
fn buffers_grow_geometrically_up_to_the_cap() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let dev = g.device().expect("device");
    let mut s = Scratch {
        bufs: HashMap::new(),
        cap: 1000,
    };
    let usage = STORAGE | DST;
    assert_eq!(s.get(&dev.device, "b", 5, usage).size(), 16);
    assert_eq!(s.get(&dev.device, "b", 16, usage).size(), 16); // reused
    assert_eq!(s.get(&dev.device, "b", 20, usage).size(), 32); // doubled
    assert_eq!(s.get(&dev.device, "b", 600, usage).size(), 608);
    // Doubling 608 would pass the cap: capped instead.
    assert_eq!(s.get(&dev.device, "b", 700, usage).size(), 1000);
    // A request above the cap itself is still honoured.
    assert_eq!(s.get(&dev.device, "b", 1200, usage).size(), 1200);
}

#[test]
fn subject_regions_match_including_wide_and_invalid_ones() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let images = [scene(700, 530, 11), scene(333, 250, 12), scene(90, 60, 13)];
    let subjects = vec![
        // Wider and taller than one 256-px block, a face, an off-frame one.
        vec![
            roi(0.05, 0.05, 0.9, 0.9),
            roi(0.4, 0.4, 0.1, 0.1),
            roi(1.5, 0.0, 0.2, 0.2),
        ],
        // Invalid regions only: no subject.
        vec![SubjectRegion {
            rect: NormRect::FULL,
            weight: 0.0,
        }],
        vec![],
    ];
    assert_parity(&g, &images, &subjects);
    let got = g.analyze_batch_with_subjects(&images, &subjects);
    assert!(got[0].sharpness.subject.is_some());
    assert!(got[1].sharpness.subject.is_none());
}

#[test]
#[should_panic(expected = "subjects: 1 lists for 2 images")]
fn mismatched_subjects_panic() {
    let images = [img(4, 4, |_, _| 0), img(4, 4, |_, _| 0)];
    let _ = subject_lookup(&images, &[vec![]]);
}

#[test]
fn other_configs_match_too() {
    for cfg in [
        AnalysisConfig {
            sharpness: SharpnessConfig {
                tile: 5, // tiles that straddle words
                ..SharpnessConfig::default()
            },
            noise: NoiseConfig { flat_fraction: 1.0 },
            ..AnalysisConfig::default()
        },
        AnalysisConfig {
            sharpness: SharpnessConfig {
                tile: 1000, // clamped to MAX_TILE: worst-case per-thread sums
                ..SharpnessConfig::default()
            },
            ..AnalysisConfig::default()
        },
    ] {
        let Some(g) = gpu(cfg) else {
            return;
        };
        // Checkerboard: maximal |Laplacian| everywhere; 259 px wide puts a
        // 256-px tile across 65 words.
        let checker = img(259, 270, |x, y| if (x + y) % 2 == 0 { 255 } else { 0 });
        let subjects = vec![vec![roi(0.0, 0.0, 1.0, 1.0)], vec![]];
        assert_parity(&g, &[checker, scene(333, 250, 3)], &subjects);
    }
}

#[test]
fn chunks_split_on_rectangle_count_and_oversize_images_use_the_cpu() {
    let Some(mut g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    g.rect_limit = 75;
    let im = scene(200, 150, 21); // 5 × 4 tiles
    // 30 regions given, only the first MAX_SUBJECT_REGIONS count: 16 blocks.
    let many = vec![roi(0.0, 0.0, 1.0, 1.0); 30];
    assert_eq!(rect_count(&im, &many, 48), 36);
    assert_eq!(rect_count(&im, &many[..15], 48), 35);
    // Two frames fill a chunk; the third starts the next; the last has
    // more rectangles (13 × 10 tiles) than any chunk and is analyzed on the CPU.
    let big = scene(600, 450, 22);
    let images = [im.clone(), im.clone(), im, big];
    let subjects = vec![
        many.clone(),
        many[..15].to_vec(),
        many.clone(),
        vec![roi(0.0, 0.0, 1.0, 1.0)],
    ];
    assert_parity(&g, &images[..3], &subjects[..3]);
    let got = g.analyze_batch_with_subjects(&images, &subjects);
    for ((a, im), s) in got.iter().zip(&images).zip(&subjects) {
        assert_eq!(*a, analyze_with_subject(im, g.config(), s));
    }
}

#[test]
fn lost_device_falls_back_to_cpu_then_rebuilds() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let images = [scene(200, 150, 4), scene(90, 91, 5)];
    assert_parity(&g, &images, &[]);
    let before = Arc::as_ptr(&g.device().expect("device"));
    g.destroy_device();
    // The chunk on the dead device fails (or the lost callback already
    // fired and the device is rebuilt first): either way the results are
    // the CPU's.
    let got = g.analyze_batch(&images);
    for (a, im) in got.iter().zip(&images) {
        assert_eq!(*a, crate::analyze(im, g.config()));
    }
    // The next call runs on a fresh device.
    let after = g.device().expect("rebuilt device");
    assert_ne!(Arc::as_ptr(&after), before);
    assert_parity(&g, &images, &[]);
    assert!(after.completed.load(Ordering::SeqCst));
}

#[test]
fn a_rebuilt_device_that_fails_at_once_means_cpu_for_good() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let images = [scene(64, 48, 7)];
    let want = vec![crate::analyze(&images[0], g.config())];
    // A failure: the next use rebuilds the device.
    let first = g.device().expect("device");
    assert!(!first.rebuilt);
    first.broken.store(true, Ordering::SeqCst);
    let rebuilt = g.device().expect("rebuilt device");
    assert!(rebuilt.rebuilt && !rebuilt.completed.load(Ordering::SeqCst));
    // Lost again before completing anything: give up.
    g.destroy_device();
    assert_eq!(g.analyze_batch(&images), want);
    assert_eq!(g.analyze_batch(&images), want);
    assert!(!g.is_available());
    g.trim(); // nothing to trim without a device
}

#[test]
fn over_long_images_and_trim() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    // Longer than MAX_SIDE: analyzed on the CPU.
    let long = img(MAX_SIDE + 4, 1, |x, _| (x % 251) as u8);
    let small = scene(80, 60, 8);
    let images = [long, small];
    let got = g.analyze_batch(&images);
    for (a, im) in got.iter().zip(&images) {
        assert_eq!(*a, crate::analyze(im, g.config()));
    }
    let dev = g.device().expect("device");
    assert_eq!(dev.pool.lock().unwrap().len(), 1);
    g.trim();
    assert!(dev.pool.lock().unwrap().is_empty());
    assert_parity(&g, &images[1..], &[]);
}

#[test]
fn failed_or_panicking_chunks_fall_back_and_rebuild() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let images = [scene(100, 80, 14), scene(70, 50, 15)];
    let want = crate::analyze_batch_cpu(&images, g.config());
    for mode in [1, 2] {
        let dev = g.device().expect("device");
        dev.fail.store(mode, Ordering::SeqCst);
        assert_eq!(g.analyze_batch(&images), want, "mode {mode}");
        assert!(dev.broken.load(Ordering::SeqCst));
        // The next call rebuilds and runs on the device again.
        let next = g.device().expect("rebuilt");
        assert!(!Arc::ptr_eq(&next, &dev));
        assert_parity(&g, &images, &[]);
    }
}

#[test]
fn a_timed_out_map_leaves_the_buffer_mappable_again() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let dev = g.device().expect("device");
    let buf = dev.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test"),
        size: 256,
        usage: wgpu::BufferUsages::MAP_READ | DST,
        mapped_at_creation: false,
    });
    let scope = dev.device.push_error_scope(wgpu::ErrorFilter::Validation);
    dev.fail.store(3, Ordering::SeqCst);
    assert!(map(&dev, &buf, wgpu::MapMode::Read, 256, None).is_err());
    // Mapping again is a validation error while the first map is still pending.
    map(&dev, &buf, wgpu::MapMode::Read, 256, None).expect("mappable again");
    buf.unmap();
    assert_eq!(pollster::block_on(scope.pop()).map(|e| e.to_string()), None);
}

#[test]
fn staging_upload_matches_too() {
    // The path for GPUs whose storage buffers cannot be mapped.
    let Ok(g) = GpuAnalyzer::new() else {
        return;
    };
    let mut dev = Dev::open(false, false).expect("device");
    dev.mappable = false;
    *g.lock() = Some(Arc::new(dev));
    let images: Vec<LumaImage> = [(301, 200), (64, 64), (5, 7)]
        .iter()
        .enumerate()
        .map(|(k, &(w, h))| scene(w, h, 40 + k as u64))
        .collect();
    assert_parity(&g, &images, &[]);
}

#[test]
fn unavailable_device_means_cpu() {
    assert_eq!(Dev::open(true, false).err(), Some(GpuError::NoAdapter));
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    *g.lock() = None; // a rebuild that failed
    assert!(!g.is_available());
    assert_eq!(g.adapter_name(), "");
    let images = [scene(120, 90, 6)];
    assert_eq!(
        g.analyze_batch(&images),
        vec![crate::analyze(&images[0], g.config())]
    );
}

#[test]
fn concurrent_and_reentrant_callers() {
    let Some(g) = gpu(AnalysisConfig::default()) else {
        return;
    };
    let images: Vec<LumaImage> = (0..8).map(|k| scene(150, 100, 30 + k)).collect();
    let want = crate::analyze_batch_cpu(&images, g.config());
    let before = g.gpu_chunks();
    // Plain threads.
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| assert_eq!(g.analyze_batch(&images), want));
        }
    });
    // From rayon tasks, nested inside the analyzer's own rayon work.
    let all: Vec<Vec<ImageAnalysis>> = (0..8)
        .into_par_iter()
        .map(|_| g.analyze_batch(&images))
        .collect();
    assert!(all.iter().all(|a| *a == want));
    // Every one of those 12 calls ran on the device, none fell back.
    assert_eq!(g.gpu_chunks() - before, 12);
    // At most one idle buffer set is kept.
    let dev = g.device().expect("device");
    assert!(dev.pool.lock().unwrap().len() <= 1);
}

#[test]
fn analyzer_selects_a_backend() {
    let cpu = crate::Analyzer::cpu(AnalysisConfig::default());
    assert!(!cpu.is_gpu());
    let images = [scene(120, 90, 6), img(0, 0, |_, _| 0)];
    let subjects = vec![vec![roi(0.1, 0.1, 0.5, 0.5)], vec![]];
    let cfg = AnalysisConfig::default();
    let want: Vec<ImageAnalysis> = images
        .iter()
        .zip(&subjects)
        .map(|(i, s)| analyze_with_subject(i, &cfg, s))
        .collect();
    assert_eq!(cpu.analyze_batch_with_subjects(&images, &subjects), want);
    assert_eq!(
        cpu.analyze_batch(&images),
        crate::analyze_batch_cpu(&images, &cfg)
    );
    let auto = crate::Analyzer::new(cfg);
    assert_eq!(auto.is_gpu(), gpu(cfg).is_some());
    assert_eq!(auto.analyze_batch_with_subjects(&images, &subjects), want);
    assert_eq!(
        auto.analyze_batch(&images),
        crate::analyze_batch_cpu(&images, &cfg)
    );
}

#[test]
fn helpers() {
    assert_eq!(grid2d(0), (0, 0));
    assert_eq!(grid2d(5), (5, 1));
    assert_eq!(grid2d(ROW_STRIDE as usize + 1), (ROW_STRIDE, 2));
    let mut w = [0u32; STAT_WORDS];
    w[2] = u32::MAX; // lap = -1
    w[3] = u32::MAX;
    w[4] = 1; // lap2 = 2^32 + 1
    w[5] = 1;
    let s = tile_stats(&w);
    assert_eq!((s.lap, s.lap2), (-1, (1 << 32) + 1));
    assert_eq!(dev_err("x"), GpuError::Device("x".into()));
    assert_eq!(GpuError::NoAdapter.to_string(), "no GPU adapter available");
    // A 600-px-wide region splits into three column blocks per 256 rows.
    let im = img(600, 300, |_, _| 0);
    let grid = empty_grid(48, 13, 7);
    let mut rects = Vec::new();
    let regions = rects_of(&im, &[roi(0.0, 0.0, 1.0, 1.0)], &grid, &mut rects);
    assert_eq!(regions, vec![91..97]);
    assert_eq!(rects.len(), rect_count(&im, &[roi(0.0, 0.0, 1.0, 1.0)], 48));
    assert_eq!(rects[91], [1, 1, 257, 257]);
    assert_eq!(rects[96], [513, 257, 599, 299]);
}
