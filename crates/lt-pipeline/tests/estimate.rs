//! A render's admission estimate covers what it allocates: renders of every kind (cold and
//! warm views, masked layers, a tiled full-quality background render, a resizing export)
//! allocate nothing beyond their reservations. Own test binary: the ledger is
//! process-wide. Skips without a GPU.
#![allow(clippy::field_reassign_with_default)]

mod support;

use lt_core::adjust::{Layer, LayerAdjustments, Mask, MaskOp, MaskShape};
use lt_core::{Adjustments, NormRect, Size};
use lt_pipeline::{ImageKey, Priority, Quality, RenderRequest, Renderer, cache, gpu_renderer};

/// HDR, clarity, curves and two masked layers.
fn busy() -> Adjustments {
    let mut a = Adjustments::default();
    a.hdr.shadows = 40.0;
    a.detail.clarity = 30.0;
    a.curve.rgb = vec![[0.25, 0.22], [0.75, 0.8]];
    let layer = |id, mask| Layer {
        id,
        name: "l".into(),
        enabled: true,
        opacity: 1.0,
        adjustments: LayerAdjustments {
            clarity: 20.0,
            ..Default::default()
        },
        mask,
    };
    a.layers = vec![
        layer(
            1,
            Mask {
                shapes: vec![MaskShape::Linear {
                    op: MaskOp::Add,
                    start: [0.5, 0.0],
                    end: [0.5, 0.6],
                }],
                feather: 0.01,
                ..Mask::default()
            },
        ),
        layer(
            2,
            Mask {
                shapes: vec![MaskShape::Radial {
                    op: MaskOp::Add,
                    center: [0.5, 0.5],
                    radius: [0.2, 0.15],
                    angle_deg: 0.0,
                    feather: 0.5,
                }],
                ..Mask::default()
            },
        ),
    ];
    a
}

#[test]
fn renders_allocate_within_their_reservations() {
    if gpu_renderer().is_err() {
        return;
    }
    let raw = support::synthetic_raw(6000, 4000);
    let plain = Adjustments::default();
    let busy = busy();
    let mut hdr = busy.clone();
    hdr.layers.clear();
    let view = RenderRequest::new(Size::new(2560, 2560));
    let region = RenderRequest {
        region: Some(NormRect {
            x: 0.3,
            y: 0.3,
            w: 0.4,
            h: 0.3,
        }),
        ..RenderRequest::new(Size::new(6000, 6000))
    };
    let tiled_view = RenderRequest {
        quality: Quality::Full,
        ..RenderRequest::new(Size::new(6000, 6000))
    };
    let background = RenderRequest {
        quality: Quality::Full,
        priority: Priority::Background,
        ..RenderRequest::new(Size::new(1600, 1600))
    };
    let full = RenderRequest {
        priority: Priority::Background,
        ..tiled_view.clone()
    };
    let cases = [
        ("view", &plain, &view),
        ("HDR view", &hdr, &view),
        ("masked view", &busy, &view),
        ("1:1 region", &busy, &region),
        ("full-quality view", &busy, &tiled_view),
        ("resized background", &plain, &background),
        ("masked resized background", &busy, &background),
        ("full resolution", &busy, &full),
    ];
    for (what, adj, req) in cases {
        // A fresh renderer: nothing pooled or resident, the whole working set is new.
        let mut g = gpu_renderer().expect("GPU");
        let before = cache::overdrawn_bytes();
        g.render(ImageKey(1), &raw, adj, req).expect("cold");
        g.render(ImageKey(1), &raw, adj, req).expect("warm");
        let over = cache::overdrawn_bytes() - before;
        assert_eq!(over, 0, "{what}: {over} bytes beyond the reservation");
    }
}
