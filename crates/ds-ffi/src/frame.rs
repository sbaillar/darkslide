//! Render request / frame layout records shared by every renderer.
//!
//! These are the shapes `ds-pipeline` will serve (see docs/FFI.md): a request
//! names the output scale (fit to a long edge, or an explicit pixels-per-sensor-
//! pixel scale), an optional normalized region (a tile), and the pixel format.
//! [`plan`] turns a request plus the source size into the exact frame geometry,
//! so the caller can allocate the destination buffer *before* rendering.

use crate::error::DsError;

/// Largest edge of an output frame (or of the virtual full image a tile is cut from).
pub const MAX_EDGE: u32 = 16_384;
/// Largest full image a tile may be cut from (e.g. 6000×4000 at 2.0× = 12000×8000).
pub const MAX_FULL_EDGE: u32 = 65_536;

/// Pixel layout of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum FrameFormat {
    /// 8-bit sRGB-encoded RGBA, alpha 255 (SDR display).
    Rgba8Srgb,
    /// IEEE half-float RGBA in *linear* extended Display P3; values above 1.0
    /// are EDR headroom. Alpha is 1.0.
    Rgba16fLinearP3,
}

impl FrameFormat {
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Rgba8Srgb => 4,
            Self::Rgba16fLinearP3 => 8,
        }
    }
}

/// Normalized rectangle (0..=1) in the oriented image.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct NormRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl NormRect {
    pub const FULL: NormRect = NormRect {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };
}

/// What to render.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct RenderRequest {
    /// Long edge of the whole output image when `scale` is `None` (fit renders).
    /// Never upscales past the source.
    pub max_long_edge: u32,
    /// Output pixels per source pixel (1.0 = true 100%). Overrides `max_long_edge`.
    pub scale: Option<f32>,
    /// Region of the image to render; `None` = whole image.
    pub region: Option<NormRect>,
    pub format: FrameFormat,
}

/// Exact geometry of a rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    /// Row stride of the destination buffer (≥ `width * bytes_per_pixel`).
    pub bytes_per_row: u32,
    pub format: FrameFormat,
    /// Size of the *whole* image at this scale (== width/height when untiled).
    pub full_width: u32,
    pub full_height: u32,
    /// Pixel origin of the frame inside the full image.
    pub origin_x: u32,
    pub origin_y: u32,
    /// Normalized region actually covered (snapped to whole pixels); `None` = whole.
    pub region: Option<NormRect>,
}

impl FrameInfo {
    /// Bytes the destination buffer must hold.
    pub fn byte_len(&self) -> u64 {
        u64::from(self.bytes_per_row) * u64::from(self.height)
    }
}

/// Resolve a request against a source of `src_w`×`src_h` pixels.
pub fn plan(src_w: u32, src_h: u32, req: &RenderRequest) -> Result<FrameInfo, DsError> {
    if src_w == 0 || src_h == 0 || src_w > MAX_FULL_EDGE || src_h > MAX_FULL_EDGE {
        return Err(DsError::invalid(format!(
            "source size {src_w}x{src_h} out of range"
        )));
    }
    let (full_w, full_h) = if let Some(s) = req.scale {
        if !(s.is_finite() && s > 0.0 && s <= 8.0) {
            return Err(DsError::invalid(format!("scale {s} outside (0, 8]")));
        }
        let w = (f64::from(src_w) * f64::from(s)).round().max(1.0);
        let h = (f64::from(src_h) * f64::from(s)).round().max(1.0);
        if w > f64::from(MAX_FULL_EDGE) || h > f64::from(MAX_FULL_EDGE) {
            return Err(DsError::invalid("scaled image too large"));
        }
        (w as u32, h as u32)
    } else {
        if req.max_long_edge == 0 {
            return Err(DsError::invalid("max_long_edge is 0"));
        }
        let long = src_w.max(src_h);
        let edge = req.max_long_edge.min(long).min(MAX_EDGE);
        let k = f64::from(edge) / f64::from(long);
        (
            ((f64::from(src_w) * k).round() as u32).max(1),
            ((f64::from(src_h) * k).round() as u32).max(1),
        )
    };
    let (x0, y0, x1, y1) = match req.region {
        None => (0, 0, full_w, full_h),
        Some(r) => {
            let ok = [r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite()) && r.w > 0.0 && r.h > 0.0;
            if !ok {
                return Err(DsError::invalid("region must be finite with positive size"));
            }
            let px = |v: f32, n: u32| (f64::from(v.clamp(0.0, 1.0)) * f64::from(n)) as u32;
            let x0 = px(r.x, full_w).min(full_w - 1);
            let y0 = px(r.y, full_h).min(full_h - 1);
            // Tolerate f32 noise (0.25 + 0.3 = 0.55000001) so edges don't grow a pixel.
            let ceil = |v: f32, n: u32| {
                ((f64::from(v.clamp(0.0, 1.0)) * f64::from(n) - 1e-3).ceil() as u32).min(n)
            };
            let x1 = ceil(r.x + r.w, full_w).max(x0 + 1);
            let y1 = ceil(r.y + r.h, full_h).max(y0 + 1);
            (x0, y0, x1, y1)
        }
    };
    let (width, height) = (x1 - x0, y1 - y0);
    if width > MAX_EDGE || height > MAX_EDGE {
        return Err(DsError::invalid(format!(
            "frame {width}x{height} exceeds {MAX_EDGE} px per edge; request a region"
        )));
    }
    let region = req.region.map(|_| NormRect {
        x: x0 as f32 / full_w as f32,
        y: y0 as f32 / full_h as f32,
        w: width as f32 / full_w as f32,
        h: height as f32 / full_h as f32,
    });
    // 256-byte row alignment: what Metal wants for buffer-backed textures.
    let row = width * req.format.bytes_per_pixel();
    let bytes_per_row = row.div_ceil(256) * 256;
    Ok(FrameInfo {
        width,
        height,
        bytes_per_row,
        format: req.format,
        full_width: full_w,
        full_height: full_h,
        origin_x: x0,
        origin_y: y0,
        region,
    })
}

/// Frame geometry for a request, so the caller can size its buffer first.
#[uniffi::export]
pub fn plan_frame(
    source_width: u32,
    source_height: u32,
    request: RenderRequest,
) -> Result<FrameInfo, DsError> {
    crate::jobs::guard(|| plan(source_width, source_height, &request))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fit(edge: u32) -> RenderRequest {
        RenderRequest {
            max_long_edge: edge,
            scale: None,
            region: None,
            format: FrameFormat::Rgba8Srgb,
        }
    }

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        let f = plan(6000, 4000, &fit(2560)).unwrap();
        assert_eq!(
            (f.width, f.height, f.full_width, f.full_height),
            (2560, 1707, 2560, 1707)
        );
        assert_eq!(f.region, None);
        assert_eq!(f.bytes_per_row % 256, 0);
        assert!(f.bytes_per_row >= 2560 * 4);
        let small = plan(300, 200, &fit(2560)).unwrap();
        assert_eq!((small.width, small.height), (300, 200));
        assert_eq!(f.byte_len(), u64::from(f.bytes_per_row) * 1707);
    }

    #[test]
    fn one_to_one_tile_is_real_pixels() {
        let req = RenderRequest {
            scale: Some(1.0),
            region: Some(NormRect {
                x: 0.5,
                y: 0.25,
                w: 0.2,
                h: 0.3,
            }),
            ..fit(1)
        };
        let f = plan(6000, 4000, &req).unwrap();
        assert_eq!((f.full_width, f.full_height), (6000, 4000));
        assert_eq!((f.origin_x, f.origin_y), (3000, 1000));
        assert_eq!((f.width, f.height), (1200, 1200));
        let r = f.region.unwrap();
        assert!((r.x - 0.5).abs() < 1e-6 && (r.h - 0.3).abs() < 1e-6);
    }

    #[test]
    fn half_float_rows_are_wider() {
        let req = RenderRequest {
            format: FrameFormat::Rgba16fLinearP3,
            ..fit(100)
        };
        let f = plan(100, 50, &req).unwrap();
        assert_eq!(f.bytes_per_row, 1024);
        assert_eq!(FrameFormat::Rgba16fLinearP3.bytes_per_pixel(), 8);
    }

    #[test]
    fn region_is_clamped_and_at_least_one_pixel() {
        let req = RenderRequest {
            scale: Some(1.0),
            region: Some(NormRect {
                x: 1.5,
                y: -1.0,
                w: 0.0001,
                h: 0.0001,
            }),
            ..fit(1)
        };
        let f = plan(100, 100, &req).unwrap();
        assert_eq!((f.origin_x, f.origin_y, f.width, f.height), (99, 0, 1, 1));
    }

    #[test]
    fn rejects_bad_requests() {
        let bad = |w, h, r: RenderRequest| {
            assert!(
                matches!(plan(w, h, &r), Err(DsError::InvalidArgument { .. })),
                "{r:?}"
            );
        };
        bad(0, 10, fit(10));
        bad(MAX_FULL_EDGE + 1, 10, fit(10));
        bad(10, 10, fit(0));
        bad(
            10,
            10,
            RenderRequest {
                scale: Some(0.0),
                ..fit(1)
            },
        );
        bad(
            10,
            10,
            RenderRequest {
                scale: Some(f32::NAN),
                ..fit(1)
            },
        );
        bad(
            60_000,
            10,
            RenderRequest {
                scale: Some(2.0),
                ..fit(1)
            },
        );
        bad(
            20_000,
            100,
            RenderRequest {
                scale: Some(1.0),
                ..fit(1)
            },
        );
        bad(
            10,
            10,
            RenderRequest {
                region: Some(NormRect {
                    x: 0.0,
                    y: 0.0,
                    w: -1.0,
                    h: 1.0,
                }),
                ..fit(10)
            },
        );
        bad(
            10,
            10,
            RenderRequest {
                region: Some(NormRect {
                    x: f32::NAN,
                    y: 0.0,
                    w: 1.0,
                    h: 1.0,
                }),
                ..fit(10)
            },
        );
        assert!(plan_frame(10, 10, fit(10)).is_ok());
        assert!((NormRect::FULL.w - 1.0).abs() < f32::EPSILON);
    }
}
