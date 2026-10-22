//! GPU brush stamping vs the CPU rasterizer (`mask::BrushRaster`), plane by plane.

use std::sync::Mutex;

use ds_core::adjust::{BrushStroke, Mask, MaskOp, MaskShape};
use ds_core::{CfaPattern, Orientation, RawImage, Size};

use super::*;

fn geo(w: u32, h: u32) -> crate::geometry::Geometry {
    let raw = RawImage {
        size: Size::new(w, h),
        cfa: CfaPattern::Rggb,
        data: vec![0; (w * h) as usize],
        black_level: [0.0; 4],
        white_level: 100.0,
        wb_as_shot: [1.0; 3],
        cam_to_xyz: crate::color::REC2020_TO_XYZ,
        wb_daylight: [1.0; 3],
        orientation: Orientation::Normal,
        default_crop: None,
    };
    crate::geometry::plan(
        &raw,
        &ds_core::Adjustments::default(),
        &crate::geometry::CaptureInfo::default(),
        None,
        Size::new(w, h),
        crate::geometry::Fit::ConstrainToSensorPixels,
    )
    .unwrap()
}

fn brush(strokes: Vec<BrushStroke>) -> Mask {
    Mask {
        shapes: vec![MaskShape::Brush {
            op: MaskOp::Add,
            strokes,
            feather: 0.0,
        }],
        ..Mask::default()
    }
}

/// The GPU raster (combined plane) of `mask` after `layer_rasters`, read back.
fn gpu_raster(
    g: &GpuRenderer,
    rasters: &Mutex<RasterCache>,
    mask: &Mask,
    geo: &crate::geometry::Geometry,
) -> Vec<f32> {
    let cm = mask::compile(mask, 1, geo, None, 9, rasters);
    let shapes = params::shapes(std::slice::from_ref(&cm));
    let sl = shapes.layouts[0][0].clone();
    let n = (sl.raster.plane.w * sl.raster.plane.h) as u64;
    let mut inner = g.lock();
    let mut cmd = inner
        .ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    let buf = inner
        .layer_rasters(&mut cmd, rasters, 9, 1, &shapes.layouts[0])
        .unwrap()
        .unwrap();
    let rb = inner.ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("brush_rb"),
        size: n * 4,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    cmd.copy_buffer_to_buffer(&buf, u64::from(sl.read) * 4, &rb, 0, n * 4);
    inner.ctx.queue.submit([cmd.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    rb.map_async(wgpu::MapMode::Read, .., move |r| {
        let _ = tx.send(r);
    });
    let _ = inner.ctx.device.poll(wgpu::PollType::wait_indefinitely());
    rx.recv().unwrap().unwrap();
    let out = bytemuck::cast_slice::<u8, f32>(&rb.slice(..).get_mapped_range().unwrap()).to_vec();
    rb.unmap();
    out
}

fn cpu_raster(
    cache: &Mutex<RasterCache>,
    mask: &Mask,
    geo: &crate::geometry::Geometry,
) -> Vec<f32> {
    let cm = mask::compile(mask, 1, geo, None, 9, cache);
    cm.rasters[0].plane.data.clone()
}

fn stroke(points: Vec<[f32; 2]>, size: f32, hardness: f32, flow: f32, erase: bool) -> BrushStroke {
    BrushStroke {
        points,
        size,
        hardness,
        flow,
        erase,
        auto_mask: false,
    }
}

/// Mixed add/erase strokes, hardness 0 / 0.5 / 1, a 1-point stroke, an incremental
/// continuation, and an undo (non-incremental rebuild): the GPU planes equal the CPU's.
#[test]
fn gpu_brush_stamping_matches_the_cpu_rasterizer() {
    let Ok(g) = GpuRenderer::new() else { return };
    let geo = geo(900, 600);
    let (gc, cc) = (
        Mutex::new(RasterCache::gpu_resident()),
        Mutex::new(RasterCache::default()),
    );
    let a = stroke(
        vec![[0.1, 0.2], [0.5, 0.25], [0.8, 0.6]],
        0.08,
        0.0,
        1.0,
        false,
    );
    let b = stroke(vec![[0.3, 0.7]], 0.1, 1.0, 0.7, false);
    let e = stroke(vec![[0.2, 0.2], [0.6, 0.3]], 0.05, 0.5, 0.9, true);
    let mut c = stroke(vec![[0.9, 0.1], [0.7, 0.3]], 0.06, 0.5, 0.8, false);
    let steps: Vec<Vec<BrushStroke>> = vec![
        vec![a.clone()],
        vec![a.clone(), b.clone()],
        vec![a.clone(), b.clone(), e.clone()],
        vec![a.clone(), b.clone(), e.clone(), c.clone()],
        {
            // Continue the last stroke (incremental).
            c.points.push([0.5, 0.5]);
            c.points.push([0.4, 0.9]);
            vec![a.clone(), b.clone(), e.clone(), c.clone()]
        },
        // Undo two strokes (rebuild).
        vec![a.clone(), b.clone()],
    ];
    for (i, strokes) in steps.iter().enumerate() {
        let m = brush(strokes.clone());
        let (gp, cp) = (gpu_raster(&g, &gc, &m, &geo), cpu_raster(&cc, &m, &geo));
        assert_eq!(gp.len(), cp.len());
        let worst = gp
            .iter()
            .zip(&cp)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 1e-4, "step {i}: max |Δ| {worst}");
        assert!(cp.iter().any(|v| *v > 0.5), "step {i}: something painted");
    }
    // A fresh GPU copy (e.g. after eviction) is rebuilt from the whole stroke list.
    g.lock().layer_rasters.clear();
    let m = brush(steps[3].clone());
    let (gp, cp) = (gpu_raster(&g, &gc, &m, &geo), cpu_raster(&cc, &m, &geo));
    let worst = gp
        .iter()
        .zip(&cp)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max);
    assert!(worst <= 1e-4, "replay: max |Δ| {worst}");
}

/// A BASE job begun on a device that was replaced (a loss between two bands) must not
/// record its next band on the new device: its buffer belongs to the lost one.
#[test]
fn a_base_job_from_a_lost_device_fails_instead_of_recording() {
    let Ok(g) = GpuRenderer::new() else { return };
    let raw = crate::testutil::raw_from_fn(512, 512, |x, y| [(x ^ y) as f32 / 1024.0; 3]);
    let req = crate::RenderRequest::new(Size::new(512, 512));
    let plan = crate::plan::build(&raw, &ds_core::Adjustments::default(), &req).unwrap();
    let mut inner = g.lock();
    let mut job = inner.begin_base(ImageKey(1), &raw, &plan).unwrap();
    inner.ctx.mark_lost();
    inner.recover().unwrap();
    assert!(matches!(
        inner.base_step(&mut job, &raw),
        Err(PipelineError::Gpu(_))
    ));
    assert!(matches!(
        inner.finish_base(job, &raw),
        Err(PipelineError::Gpu(_))
    ));
}
