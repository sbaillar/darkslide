//! Turn, admission and per-lane table-cache behaviour that needs a device.

use std::sync::atomic::Ordering;
use std::time::Duration;

use lt_core::Size;

use super::*;

fn renderer() -> Option<GpuRenderer> {
    GpuRenderer::new().ok()
}

fn req(p: Priority) -> RenderRequest {
    RenderRequest {
        priority: p,
        ..RenderRequest::new(Size::new(64, 64))
    }
}

#[test]
fn an_unknown_kernel_is_an_error_not_a_device_loss() {
    let Some(g) = renderer() else { return };
    let r = g.turn(&req(Priority::Interactive), |inner| {
        let mut enc = inner
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let buf = inner.ctx.new_buffer("scratch", 64)?;
        inner
            .ctx
            .dispatch(&mut enc, "no_such_kernel", &[&buf], 1, 1);
        Ok(())
    });
    let Err(PipelineError::Gpu(msg)) = r else {
        panic!("expected a GPU error, got {r:?}");
    };
    assert!(msg.contains("no_such_kernel"), "{msg}");
    assert_eq!(g.device_rebuilds(), 0, "the device was not rebuilt");
    // The next turn runs on the same device and sees no stale error.
    g.turn(&req(Priority::Interactive), |_| Ok(()))
        .expect("device still fine");
    assert_eq!(g.device_rebuilds(), 0);
}

#[test]
fn table_uploads_are_cached_per_lane() {
    let Some(g) = renderer() else { return };
    let a = vec![1.0f32; 64];
    let b = vec![2.0f32; 64];
    let nan = vec![f32::NAN; 64];
    // The viewer and a bulk render alternate their turns with different tables.
    for _ in 0..3 {
        g.turn(&req(Priority::Interactive), |i| {
            i.tables_buffer(&a).map(drop)
        })
        .expect("viewer");
        g.turn(&req(Priority::Bulk), |i| i.tables_buffer(&b).map(drop))
            .expect("bulk");
    }
    assert_eq!(g.table_uploads(), 2, "one upload per lane, not per turn");
    // NaN != NaN as floats, but identical bits are the same tables.
    let before = g.table_uploads();
    for _ in 0..3 {
        g.turn(&req(Priority::Tool), |i| i.tables_buffer(&nan).map(drop))
            .expect("nan tables");
    }
    assert_eq!(g.table_uploads(), before + 1);
}

#[test]
fn a_waiting_admission_does_not_poll_the_caches() {
    let Some(g) = renderer() else { return };
    let r = req(Priority::Bulk);
    // Bigger than any budget; the first is admitted because nothing else renders.
    let huge = 1u64 << 62;
    let first = g.admit(ImageKey(1), huge, &r).expect("alone");
    let passes0 = g.shared.room_passes.load(Ordering::Relaxed);
    std::thread::scope(|s| {
        let g2 = g.clone();
        let waiter = s.spawn(move || g2.admit(ImageKey(2), huge, &req(Priority::Bulk)).map(drop));
        std::thread::sleep(Duration::from_millis(300));
        let passes = g.shared.room_passes.load(Ordering::Relaxed) - passes0;
        assert!(passes <= 2, "{passes} eviction passes while waiting 300 ms");
        drop(first);
        waiter
            .join()
            .expect("waiter")
            .expect("admitted after notify");
    });
}

#[test]
fn a_waiting_admission_can_be_cancelled() {
    let Some(g) = renderer() else { return };
    let huge = 1u64 << 62;
    let first = g
        .admit(ImageKey(1), huge, &req(Priority::Bulk))
        .expect("alone");
    let token = crate::CancelToken::new();
    let r = RenderRequest {
        cancel: Some(token.clone()),
        ..req(Priority::Bulk)
    };
    std::thread::scope(|s| {
        let waiter = s.spawn(|| g.admit(ImageKey(2), huge, &r).map(drop));
        std::thread::sleep(Duration::from_millis(50));
        token.cancel();
        assert!(matches!(
            waiter.join().expect("waiter"),
            Err(PipelineError::Cancelled)
        ));
    });
    drop(first);
}

#[test]
fn a_cancelled_turn_wait_returns_cancelled() {
    let Some(g) = renderer() else { return };
    let token = crate::CancelToken::new();
    let r = RenderRequest {
        cancel: Some(token.clone()),
        ..req(Priority::Bulk)
    };
    let (started, release) = std::sync::mpsc::channel::<()>();
    std::thread::scope(|s| {
        let g2 = g.clone();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let holder = s.spawn(move || {
            g2.turn(&req(Priority::Bulk), |_| {
                started.send(()).ok();
                go_rx.recv().ok();
                Ok(())
            })
        });
        release.recv().expect("holder started");
        let waiter = s.spawn(|| g.turn(&r, |_| Ok(())));
        std::thread::sleep(Duration::from_millis(30));
        token.cancel();
        assert!(matches!(
            waiter.join().expect("waiter"),
            Err(PipelineError::Cancelled)
        ));
        go_tx.send(()).ok();
        holder.join().expect("holder").expect("holder turn");
    });
}

#[test]
fn racing_stagings_of_one_image_upload_it_once() {
    let Some(g) = renderer() else { return };
    let raw = lt_core::RawImage {
        size: Size::new(256, 256),
        cfa: lt_core::CfaPattern::Rggb,
        data: vec![1000; 256 * 256],
        black_level: [0.0; 4],
        white_level: 4000.0,
        wb_as_shot: [1.0; 3],
        cam_to_xyz: crate::color::REC2020_TO_XYZ,
        wb_daylight: [1.0; 3],
        orientation: lt_core::Orientation::Normal,
        default_crop: None,
    };
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                g.stage_raw(ImageKey(9), &raw, Priority::Bulk)
                    .expect("stage");
            });
        }
    });
    assert_eq!(g.shared.uploads.load(Ordering::Relaxed), 1);
    assert!(g.raw_bytes() > 0);
}
