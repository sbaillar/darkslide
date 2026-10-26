//! Effects (`Adjustments::effects`): lens blur, glow, halation and film grain — the plans
//! both renderers execute and the CPU reference of each stage (the WGSL kernels in
//! `gpu/shaders/bokeh_*.wgsl`, `fx_*.wgsl` and `grain_fn.wgsl` mirror these functions).
//!
//! **Lens blur** runs in scene-linear light right after the scene stage, so bright
//! highlights bloom into bokeh balls the way a lens renders them:
//! 1. A signed circle of confusion per pixel, in developed pixels: from the app's depth map
//!    (relative inverse depth, which is proportional to defocus) around the focus depth,
//!    or from the distance to a planar focus band (tilt-shift). Negative = in front of the
//!    focus.
//! 2. A block-mean image on a grid of `f` px (a power of two keeping the largest blur
//!    within [`RLOW_MAX`] grid cells), anchored to the full frame so tiles match: only
//!    pixels that are out of focus contribute (in-focus pixels have weight 0), so the
//!    sharp subject never enters the blur and can't bleed into the background.
//! 3. An occlusion-aware gather on that grid (scatter-as-gather): every cell whose own
//!    aperture shape (circle, six blades, or a cat's eye toward the corners) covers the
//!    pixel contributes, weighted by `1/r²` and the highlight boost; a cell behind the
//!    pixel spreads no wider than the pixel's own blur, so the background can't spread
//!    over what is in front of it.
//! 4. The composite: pixels within half a pixel of focus keep their exact value; the rest
//!    blend toward the bilinearly upsampled gather by their blur size.
//!
//! **Glow** (Orton: a blurred copy screened over the photo) and **halation** (a red-orange
//! glow around highlights) are block-mean → Gaussian → upsample passes on the display-
//! referred image after the layers. **Grain** is monochrome value noise fixed to the image
//! (E-frame position at output scale, so tiles, regions and zoom levels agree), added
//! after sharpening.

use std::sync::{Arc, Mutex};

use lt_core::adjust::{Adjustments, AiTarget};
use lt_core::effects::{BlurMode, BokehShape};
use rayon::prelude::*;

use crate::color_edit::luma;
use crate::geometry::Geometry;
use crate::local::gaussian_kernel;
use crate::mask::{MaskBitmap, MaskSource};

/// Largest blur radius (amount 100), as a fraction of the long edge.
pub const MAX_BLUR: f32 = 0.025;
/// Largest gather radius on the block grid, in cells.
pub const RLOW_MAX: f32 = 16.0;
/// Depth distance (0..1) over which the blur ramps from the focal range to its full amount.
pub const DEPTH_RAMP: f32 = 0.45;

/// Aperture shape codes (shared with the WGSL kernels).
pub const SHAPE_CIRCLE: u32 = 0;
pub const SHAPE_BLADES: u32 = 1;
pub const SHAPE_CAT_EYE: u32 = 2;

/// Where the circle of confusion comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum CocSource {
    /// The depth map (E frame, 255 = nearest) and the depth in focus (0..1).
    Depth {
        map: Arc<MaskBitmap>,
        focus: f32,
        /// Half-width of the sharp depth zone.
        range: f32,
    },
    /// Distance to a straight band through `center` (isotropic long-edge units) along the
    /// unit `normal`; sharp within `width`, fully blurred `feather` beyond.
    Plane {
        center: [f32; 2],
        normal: [f32; 2],
        width: f32,
        feather: f32,
    },
}

/// The lens blur of one render.
#[derive(Debug, Clone, PartialEq)]
pub struct LensBlurPlan {
    /// Largest blur radius, developed pixels.
    pub rmax: f32,
    /// Block size of the gather grid (a power of two).
    pub f: usize,
    /// Gather radius on the grid, in cells.
    pub rlow: usize,
    /// Grid phase (local pixel `x` lies in cell `(x + off) / f`), anchored to the frame.
    pub off: [usize; 2],
    /// 0..=1 highlight boost.
    pub boost: f32,
    pub shape: u32,
    pub source: CocSource,
}

impl LensBlurPlan {
    /// Output pixels on each side a region render needs as context.
    pub fn reach(&self) -> usize {
        (self.rlow + 3) * self.f
    }

    /// Anchor the grid for a render whose pixel (0, 0) is frame pixel `origin`.
    #[must_use]
    pub fn anchored(mut self, origin: [i64; 2]) -> Self {
        let f = self.f as i64;
        self.off = origin.map(|o| o.rem_euclid(f) as usize);
        self
    }

    /// Grid size for a `w × h` render.
    pub fn grid(&self, w: usize, h: usize) -> (usize, usize) {
        (
            (w + self.off[0]).div_ceil(self.f),
            (h + self.off[1]).div_ceil(self.f),
        )
    }

    /// Signed circle of confusion (developed px; negative in front of the focus) at
    /// E-normalized `(u, v)` (`iso`: the E frame's aspect, long edge = 1).
    pub fn coc(&self, u: f32, v: f32, iso: [f32; 2]) -> f32 {
        match &self.source {
            CocSource::Depth { map, focus, range } => {
                let d = depth_max4(map, u, v);
                let delta = d - focus;
                let k = ((delta.abs() - range) / DEPTH_RAMP).clamp(0.0, 1.0);
                if delta > 0.0 {
                    -self.rmax * k
                } else {
                    self.rmax * k
                }
            }
            CocSource::Plane {
                center,
                normal,
                width,
                feather,
            } => {
                let p = [u * iso[0] - center[0], v * iso[1] - center[1]];
                let dist = (p[0] * normal[0] + p[1] * normal[1]).abs();
                self.rmax * ((dist - width) / feather).clamp(0.0, 1.0)
            }
        }
    }
}

/// Depth at E-normalized `(u, v)`: the nearest (largest) of the four bilinear taps, 0..1.
/// Taking the nearest keeps a subject's edge pixels at the subject's depth, so the subject
/// colour never enters the background's blur.
pub fn depth_max4(map: &MaskBitmap, u: f32, v: f32) -> f32 {
    let (w, h) = (map.size.width as usize, map.size.height as usize);
    let fx = (u * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
    let fy = (v * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (fx as usize, fy as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let m = map.data[y0 * w + x0]
        .max(map.data[y0 * w + x1])
        .max(map.data[y1 * w + x0])
        .max(map.data[y1 * w + x1]);
    f32::from(m) / 255.0
}

/// The depth in focus when no point is picked: the mean depth under the subject mask, or
/// (no subject) the 90th percentile of the map (the nearest things).
pub fn auto_focus(depth: &MaskBitmap, subject: Option<&MaskBitmap>) -> f32 {
    let (w, h) = (depth.size.width as usize, depth.size.height as usize);
    if let Some(m) = subject {
        let (mw, mh) = (m.size.width as usize, m.size.height as usize);
        let (mut acc, mut wsum) = (0.0f64, 0.0f64);
        for y in 0..h {
            let my = ((y * 2 + 1) * mh / (2 * h)).min(mh - 1);
            for x in 0..w {
                let mx = ((x * 2 + 1) * mw / (2 * w)).min(mw - 1);
                let wt = f64::from(m.data[my * mw + mx]);
                acc += wt * f64::from(depth.data[y * w + x]);
                wsum += wt;
            }
        }
        if wsum >= 0.002 * 255.0 * (w * h) as f64 {
            return (acc / wsum / 255.0) as f32;
        }
    }
    let mut hist = [0usize; 256];
    for &d in &depth.data {
        hist[d as usize] += 1;
    }
    let target = (depth.data.len() * 9).div_ceil(10);
    let mut seen = 0;
    for (v, n) in hist.iter().enumerate() {
        seen += n;
        if seen >= target {
            return v as f32 / 255.0;
        }
    }
    1.0
}

/// Auto-focus results by (depth, subject) content, so a drag doesn't rescan the maps.
static AUTO_FOCUS: Mutex<Vec<(MaskKeys, f32)>> = Mutex::new(Vec::new());

type MaskKeys = ((u32, u32, bool, u64), Option<(u32, u32, bool, u64)>);

fn auto_focus_cached(depth: &MaskBitmap, subject: Option<&MaskBitmap>) -> f32 {
    let key = (depth.content_key(), subject.map(MaskBitmap::content_key));
    if let Ok(c) = AUTO_FOCUS.lock()
        && let Some((_, v)) = c.iter().find(|(k, _)| *k == key)
    {
        return *v;
    }
    let v = auto_focus(depth, subject);
    if let Ok(mut c) = AUTO_FOCUS.lock() {
        c.push((key, v));
        if c.len() > 16 {
            c.remove(0);
        }
    }
    v
}

fn finite_or(v: f32, d: f32) -> f32 {
    if v.is_finite() { v } else { d }
}

/// The lens blur plan for a render, or `None` when it renders nothing (amount 0, a blur
/// under half a pixel, or depth mode without a depth map).
pub fn plan_lens_blur(
    adj: &Adjustments,
    geo: &Geometry,
    masks: Option<&Arc<dyn MaskSource>>,
) -> Option<LensBlurPlan> {
    let lb = &adj.effects.lens_blur;
    let amount = (finite_or(lb.amount, 0.0) / 100.0).clamp(0.0, 1.0);
    let rmax = amount * MAX_BLUR * geo.long_edge_out();
    if rmax < 0.5 {
        return None;
    }
    let source = match lb.mode {
        BlurMode::Depth => {
            let map = masks?.depth_map()?;
            if map.size.width == 0 || map.size.height == 0 {
                return None;
            }
            let focus = match lb.focus {
                Some([u, v]) if u.is_finite() && v.is_finite() => {
                    depth_max4(&map, u.clamp(0.0, 1.0), v.clamp(0.0, 1.0))
                }
                _ => {
                    let subject = masks.and_then(|m| m.ai_mask(AiTarget::Subject));
                    auto_focus_cached(&map, subject.as_deref())
                }
            };
            let range = (finite_or(lb.focal_range, 20.0) / 100.0).clamp(0.0, 1.0) * 0.3;
            CocSource::Depth { map, focus, range }
        }
        // A newer build's mode: no blur here (its sidecar still loads and keeps it).
        BlurMode::Other(_) => return None,
        BlurMode::Plane => {
            let b = &lb.plane;
            let iso = geo.iso();
            let a = finite_or(b.angle_deg, 0.0).to_radians();
            CocSource::Plane {
                center: [
                    finite_or(b.center[0], 0.5) * iso[0],
                    finite_or(b.center[1], 0.5) * iso[1],
                ],
                // The band's normal (y down; positive angles turn the band counter-clockwise).
                normal: [a.sin(), a.cos()],
                width: finite_or(b.width, 0.06).clamp(0.0, 1.0),
                feather: finite_or(b.feather, 0.22).clamp(0.005, 1.0),
            }
        }
    };
    let mut f = 1usize;
    while rmax / f as f32 > RLOW_MAX && f < 64 {
        f *= 2;
    }
    Some(LensBlurPlan {
        rmax,
        f,
        rlow: (rmax / f as f32).ceil() as usize,
        off: [0, 0],
        boost: (finite_or(lb.highlight_boost, 0.0) / 100.0).clamp(0.0, 1.0),
        shape: match lb.shape {
            BokehShape::Circle | BokehShape::Other(_) => SHAPE_CIRCLE,
            BokehShape::Blades => SHAPE_BLADES,
            BokehShape::CatEye => SHAPE_CAT_EYE,
        },
        source,
    })
}

/// Weight of an out-of-focus pixel in the gather grid: 0 within half a pixel of focus.
#[inline]
pub fn blur_weight(c: f32) -> f32 {
    (c.abs() - 0.5).clamp(0.0, 1.0)
}

/// Highlight boost of a scene-linear colour (bright highlights dominate the gather, so
/// they bloom into bokeh balls).
#[inline]
pub fn highlight_gain(rgb: [f32; 3], boost: f32) -> f32 {
    let t = ((luma(rgb) - 0.7) / 1.3).clamp(0.0, 1.0);
    1.0 + 6.0 * boost * t * t
}

/// How much a cell at offset `(dx, dy)` (cells, from the cell to the pixel) whose aperture
/// has radius `r` (cells) covers the pixel, anti-aliased over one cell. `vig` is the
/// pixel's position in the frame (centre 0, corners at radius 1) for the cat's eye.
#[inline]
pub fn shape_cover(shape: u32, dx: f32, dy: f32, r: f32, vig: [f32; 2]) -> f32 {
    let disc = |x: f32, y: f32| (r - (x * x + y * y).sqrt() + 0.5).clamp(0.0, 1.0);
    match shape {
        SHAPE_BLADES => {
            // Six blades: a hexagon with circumradius r (inradius r·cos 30°).
            // Edge normals at 15°, 75° and 135° (the WGSL kernel uses the same constants).
            const C30: f32 = 0.866_025_4;
            let m = (dx * 0.965_925_8 + dy * 0.258_819)
                .abs()
                .max((dx * 0.258_819 + dy * 0.965_925_8).abs())
                .max(((dy - dx) * std::f32::consts::FRAC_1_SQRT_2).abs());
            (r - m / C30 + 0.5).clamp(0.0, 1.0)
        }
        SHAPE_CAT_EYE => {
            // The aperture clipped by a second, equal disc shifted toward the frame
            // centre: an eye elongated around the centre, flatter toward the corners.
            let s = 0.7 * r;
            disc(dx, dy).min(disc(dx + s * vig[0], dy + s * vig[1]))
        }
        _ => disc(dx, dy),
    }
}

/// Lens blur of the scene-linear image `lin` (`w × h`, geometry `geo`), in place.
pub fn lens_blur(lin: &mut [[f32; 3]], w: usize, h: usize, geo: &Geometry, lb: &LensBlurPlan) {
    let iso = geo.iso();
    let norm = geo.to_norm.to_f32();
    let coc: Vec<f32> = (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (px, py) = ((i % w) as f32 + 0.5, (i / w) as f32 + 0.5);
            let u = norm[0] * px + norm[1] * py + norm[2];
            let v = norm[3] * px + norm[4] * py + norm[5];
            lb.coc(u, v, iso)
        })
        .collect();
    let (lo, loz) = blur_down(lin, &coc, w, h, lb);
    let vig = geo.to_vignette.to_f32();
    let gathered = gather(&lo, &loz, w, h, lb, vig);
    composite(lin, &coc, &gathered, w, h, lb);
}

/// The gather grid: per cell the mean colour of its out-of-focus pixels with their
/// weight × highlight gain (`w`), and their mean signed blur in cells.
pub fn blur_down(
    lin: &[[f32; 3]],
    coc: &[f32],
    w: usize,
    h: usize,
    lb: &LensBlurPlan,
) -> (Vec<[f32; 4]>, Vec<f32>) {
    let f = lb.f;
    let (lw, lh) = lb.grid(w, h);
    let cells: Vec<([f32; 4], f32)> = (0..lw * lh)
        .into_par_iter()
        .map(|c| {
            let (cx, cy) = (c % lw, c / lw);
            let x0 = (cx * f).saturating_sub(lb.off[0]);
            let y0 = (cy * f).saturating_sub(lb.off[1]);
            let x1 = ((cx + 1) * f - lb.off[0]).min(w);
            let y1 = ((cy + 1) * f - lb.off[1]).min(h);
            let (mut rgb, mut bsum, mut zsum) = ([0.0f32; 3], 0.0f32, 0.0f32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = y * w + x;
                    let b = blur_weight(coc[i]);
                    for k in 0..3 {
                        rgb[k] += b * lin[i][k];
                    }
                    bsum += b;
                    zsum += b * coc[i];
                }
            }
            if bsum <= 0.0 {
                return ([0.0; 4], 0.0);
            }
            let rgb = rgb.map(|v| v / bsum);
            let n = ((x1 - x0) * (y1 - y0)) as f32;
            let a = bsum / n * highlight_gain(rgb, lb.boost);
            ([rgb[0], rgb[1], rgb[2], a], zsum / bsum / f as f32)
        })
        .collect();
    cells.into_iter().unzip()
}

/// Occlusion-aware gather on the grid: per cell the blurred colour and whether anything
/// reached it (alpha 0 or 1).
pub fn gather(
    lo: &[[f32; 4]],
    loz: &[f32],
    w: usize,
    h: usize,
    lb: &LensBlurPlan,
    vig_rows: [f32; 6],
) -> Vec<[f32; 4]> {
    let (lw, lh) = lb.grid(w, h);
    let reach = lb.rlow as i64 + 1;
    let reach2 = (reach as f32 + 0.5) * (reach as f32 + 0.5);
    let f = lb.f as f32;
    (0..lw * lh)
        .into_par_iter()
        .map(|p| {
            let (px, py) = ((p % lw) as i64, (p / lw) as i64);
            let zp = loz[p];
            // The cell centre in developed px, in the frame's vignette coordinates.
            let cx = (px as f32 + 0.5) * f - lb.off[0] as f32;
            let cy = (py as f32 + 0.5) * f - lb.off[1] as f32;
            let vig = [
                vig_rows[0] * cx + vig_rows[1] * cy + vig_rows[2],
                vig_rows[3] * cx + vig_rows[4] * cy + vig_rows[5],
            ];
            let (mut acc, mut wsum) = ([0.0f32; 3], 0.0f32);
            for dy in -reach..=reach {
                let sy = py + dy;
                if sy < 0 || sy >= lh as i64 {
                    continue;
                }
                for dx in -reach..=reach {
                    let sx = px + dx;
                    if sx < 0 || sx >= lw as i64 || (dx * dx + dy * dy) as f32 > reach2 {
                        continue;
                    }
                    let s = sy as usize * lw + sx as usize;
                    let c = lo[s];
                    if c[3] <= 0.0 {
                        continue;
                    }
                    let zs = loz[s];
                    // A cell behind the pixel spreads no wider than the pixel's own blur.
                    let r = if zs < zp {
                        zs.abs()
                    } else {
                        zs.abs().min(zp.abs())
                    };
                    let cover = shape_cover(lb.shape, -(dx as f32), -(dy as f32), r, vig);
                    if cover <= 0.0 {
                        continue;
                    }
                    let wt = cover * c[3] / (r * r).max(0.25);
                    for k in 0..3 {
                        acc[k] += wt * c[k];
                    }
                    wsum += wt;
                }
            }
            if wsum > 0.0 {
                [acc[0] / wsum, acc[1] / wsum, acc[2] / wsum, 1.0]
            } else {
                [0.0; 4]
            }
        })
        .collect()
}

/// Blend each out-of-focus pixel toward the upsampled gather by its blur size; pixels
/// within half a pixel of focus keep their exact value.
pub fn composite(
    lin: &mut [[f32; 3]],
    coc: &[f32],
    out: &[[f32; 4]],
    w: usize,
    h: usize,
    lb: &LensBlurPlan,
) {
    let (lw, lh) = lb.grid(w, h);
    let f = lb.f as f32;
    let full = (0.5 * f).max(1.0);
    lin.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, p) in row.iter_mut().enumerate() {
            let c = coc[y * w + x].abs();
            if c < 0.5 {
                continue;
            }
            let fx = (((x + lb.off[0]) as f32 + 0.5) / f - 0.5).clamp(0.0, (lw - 1) as f32);
            let fy = (((y + lb.off[1]) as f32 + 0.5) / f - 0.5).clamp(0.0, (lh - 1) as f32);
            let (x0, y0) = (fx as usize, fy as usize);
            let (x1, y1) = ((x0 + 1).min(lw - 1), (y0 + 1).min(lh - 1));
            let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
            let taps = [
                (out[y0 * lw + x0], (1.0 - tx) * (1.0 - ty)),
                (out[y0 * lw + x1], tx * (1.0 - ty)),
                (out[y1 * lw + x0], (1.0 - tx) * ty),
                (out[y1 * lw + x1], tx * ty),
            ];
            let (mut acc, mut alpha) = ([0.0f32; 3], 0.0f32);
            for (o, wt) in taps {
                let a = wt * o[3];
                for k in 0..3 {
                    acc[k] += a * o[k];
                }
                alpha += a;
            }
            if alpha <= 1e-6 {
                continue;
            }
            let t = ((c - 0.5) / full).clamp(0.0, 1.0) * alpha.min(1.0);
            for k in 0..3 {
                p[k] += (acc[k] / alpha - p[k]) * t;
            }
        }
    });
}

// ------------------------------------------------------------ glow, halation ---

/// One blurred overlay (glow or halation): its strength, grid and Gaussian.
#[derive(Debug, Clone, PartialEq)]
pub struct BlurOverlay {
    pub amount: f32,
    /// Block size (a power of two) and the grid phase anchored to the frame.
    pub f: usize,
    pub off: [usize; 2],
    /// Half Gaussian kernel on the grid (centre first).
    pub kernel: Vec<f32>,
}

impl BlurOverlay {
    fn new(amount: f32, sigma_px: f32) -> Self {
        let mut f = 1usize;
        while sigma_px / f as f32 > 4.0 && f < 64 {
            f *= 2;
        }
        Self {
            amount,
            f,
            off: [0, 0],
            kernel: gaussian_kernel(sigma_px / f as f32),
        }
    }

    pub fn reach(&self) -> usize {
        (self.kernel.len() + 3) * self.f
    }

    #[must_use]
    pub fn anchored(mut self, origin: [i64; 2]) -> Self {
        let f = self.f as i64;
        self.off = origin.map(|o| o.rem_euclid(f) as usize);
        self
    }

    pub fn grid(&self, w: usize, h: usize) -> (usize, usize) {
        (
            (w + self.off[0]).div_ceil(self.f),
            (h + self.off[1]).div_ceil(self.f),
        )
    }
}

/// Glow and halation of a render.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FxPlan {
    pub glow: Option<BlurOverlay>,
    pub halation: Option<BlurOverlay>,
}

impl FxPlan {
    pub fn is_active(&self) -> bool {
        self.glow.is_some() || self.halation.is_some()
    }

    pub fn reach(&self) -> usize {
        self.glow
            .iter()
            .chain(&self.halation)
            .map(BlurOverlay::reach)
            .max()
            .unwrap_or(0)
    }

    #[must_use]
    pub fn anchored(self, origin: [i64; 2]) -> Self {
        Self {
            glow: self.glow.map(|g| g.anchored(origin)),
            halation: self.halation.map(|g| g.anchored(origin)),
        }
    }
}

/// Halation tint (added light around highlights).
pub const HALATION_TINT: [f32; 3] = [0.6, 0.19, 0.07];

pub fn plan_fx(adj: &Adjustments, long: f32) -> FxPlan {
    let fx = &adj.effects;
    let glow_amount = (finite_or(fx.glow.amount, 0.0) / 100.0).clamp(0.0, 1.0);
    let hal_amount = (finite_or(fx.halation.amount, 0.0) / 100.0).clamp(0.0, 1.0);
    let size = (finite_or(fx.glow.size, 50.0) / 100.0).clamp(0.0, 1.0);
    FxPlan {
        glow: (glow_amount > 0.0)
            .then(|| BlurOverlay::new(0.6 * glow_amount, (0.004 + 0.03 * size) * long)),
        halation: (hal_amount > 0.0).then(|| BlurOverlay::new(hal_amount, 0.004 * long)),
    }
}

/// Glow's source: the colour, clamped to the display range.
#[inline]
pub fn glow_src(e: [f32; 3]) -> [f32; 3] {
    e.map(|v| v.clamp(0.0, 1.0))
}

/// Halation's source: how far a pixel's luma is into the highlights.
#[inline]
pub fn halation_src(e: [f32; 3]) -> f32 {
    ((luma(e) - 0.7) / 0.3).clamp(0.0, 1.5)
}

/// Screen `g` over `e` (each clamped to 0..1) with strength `a`.
#[inline]
pub fn glow_px(e: [f32; 3], g: [f32; 3], a: f32) -> [f32; 3] {
    [0, 1, 2].map(|k| {
        let ec = e[k].clamp(0.0, 1.0);
        let s = 1.0 - (1.0 - ec) * (1.0 - g[k].clamp(0.0, 1.0));
        e[k] + a * (s - ec)
    })
}

/// Block means of `src` (4 channels) on `ov`'s grid, Gaussian-blurred there.
fn overlay_plane(
    w: usize,
    h: usize,
    ov: &BlurOverlay,
    src: impl Fn(usize) -> [f32; 4] + Sync,
) -> Vec<[f32; 4]> {
    let (lw, lh) = ov.grid(w, h);
    let f = ov.f;
    let low: Vec<[f32; 4]> = (0..lw * lh)
        .into_par_iter()
        .map(|c| {
            let (cx, cy) = (c % lw, c / lw);
            let x0 = (cx * f).saturating_sub(ov.off[0]);
            let y0 = (cy * f).saturating_sub(ov.off[1]);
            let x1 = ((cx + 1) * f - ov.off[0]).min(w);
            let y1 = ((cy + 1) * f - ov.off[1]).min(h);
            let mut a = [0.0f32; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let v = src(y * w + x);
                    for k in 0..4 {
                        a[k] += v[k];
                    }
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as f32;
            a.map(|v| v / n)
        })
        .collect();
    let k = &ov.kernel;
    let pass = |src: &[[f32; 4]], vertical: bool| -> Vec<[f32; 4]> {
        (0..lw * lh)
            .into_par_iter()
            .map(|i| {
                let (x, y) = (i % lw, i / lw);
                let at = |d: isize| {
                    if vertical {
                        src[(y as isize + d).clamp(0, lh as isize - 1) as usize * lw + x]
                    } else {
                        src[y * lw + (x as isize + d).clamp(0, lw as isize - 1) as usize]
                    }
                };
                let mut acc = at(0).map(|v| v * k[0]);
                for (d, kv) in k.iter().enumerate().skip(1) {
                    let (a, b) = (at(-(d as isize)), at(d as isize));
                    for c in 0..4 {
                        acc[c] += kv * (a[c] + b[c]);
                    }
                }
                acc
            })
            .collect()
    };
    let tmp = pass(&low, false);
    pass(&tmp, true)
}

/// Bilinear sample of a grid plane at local pixel `(x, y)`.
#[inline]
pub fn upsample4(
    p: &[[f32; 4]],
    ov: &BlurOverlay,
    lw: usize,
    lh: usize,
    x: usize,
    y: usize,
) -> [f32; 4] {
    let f = ov.f as f32;
    let fx = (((x + ov.off[0]) as f32 + 0.5) / f - 0.5).clamp(0.0, (lw - 1) as f32);
    let fy = (((y + ov.off[1]) as f32 + 0.5) / f - 0.5).clamp(0.0, (lh - 1) as f32);
    let (x0, y0) = (fx as usize, fy as usize);
    let (x1, y1) = ((x0 + 1).min(lw - 1), (y0 + 1).min(lh - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let (a, b, c, d) = (
        p[y0 * lw + x0],
        p[y0 * lw + x1],
        p[y1 * lw + x0],
        p[y1 * lw + x1],
    );
    [0, 1, 2, 3].map(|k| {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bot = c[k] + (d[k] - c[k]) * tx;
        top + (bot - top) * ty
    })
}

/// Glow and halation on the display-referred image `enc` (`w × h`), in place.
pub fn apply_fx(enc: &mut [[f32; 3]], w: usize, h: usize, fx: &FxPlan) {
    let glow = fx.glow.as_ref().map(|ov| {
        let src = |i: usize| {
            let g = glow_src(enc[i]);
            [g[0], g[1], g[2], 0.0]
        };
        (ov, overlay_plane(w, h, ov, src), ov.grid(w, h))
    });
    let hal = fx.halation.as_ref().map(|ov| {
        let src = |i: usize| [halation_src(enc[i]), 0.0, 0.0, 0.0];
        (ov, overlay_plane(w, h, ov, src), ov.grid(w, h))
    });
    enc.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, e) in row.iter_mut().enumerate() {
            if let Some((ov, p, (lw, lh))) = &glow {
                let g = upsample4(p, ov, *lw, *lh, x, y);
                *e = glow_px(*e, [g[0], g[1], g[2]], ov.amount);
            }
            if let Some((ov, p, (lw, lh))) = &hal {
                let v = upsample4(p, ov, *lw, *lh, x, y)[0] * ov.amount;
                for k in 0..3 {
                    e[k] += v * HALATION_TINT[k];
                }
            }
        }
    });
}

// ------------------------------------------------------------------- grain ---

/// Film grain of a render.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrainPlan {
    /// Amplitude (encoded units) before the midtone response.
    pub amp: f32,
    /// Grain cell, developed pixels.
    pub cell: f32,
    /// 0..=1 share of the finer octave.
    pub rough: f32,
}

pub fn plan_grain(adj: &Adjustments, long: f32) -> Option<GrainPlan> {
    let g = &adj.effects.grain;
    let amount = (finite_or(g.amount, 0.0) / 100.0).clamp(0.0, 1.0);
    if amount <= 0.0 {
        return None;
    }
    let size = (finite_or(g.size, 25.0) / 100.0).clamp(0.0, 1.0);
    let cell = (0.7 + 3.0 * size) / 6000.0 * long;
    Some(GrainPlan {
        // Grain finer than a pixel averages out, as it does when the export is downsized.
        amp: 0.12 * amount * cell.min(1.0),
        cell: cell.max(1e-3),
        rough: (finite_or(g.roughness, 50.0) / 100.0).clamp(0.0, 1.0),
    })
}

/// A lattice value in [-1, 1] (triangular distribution), the same in WGSL.
#[inline]
pub fn lattice(ix: i32, iy: i32, seed: u32) -> f32 {
    let mut h = (ix as u32).wrapping_mul(0x8da6_b343)
        ^ (iy as u32).wrapping_mul(0xd816_3841)
        ^ seed.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^= h >> 16;
    let a = (h & 0xffff) as f32 / 65535.0;
    let b = (h >> 16) as f32 / 65535.0;
    a + b - 1.0
}

/// Smooth value noise at lattice position `(x, y)`.
#[inline]
pub fn value_noise(x: f32, y: f32, seed: u32) -> f32 {
    let (fx, fy) = (x.floor(), y.floor());
    let (tx, ty) = (x - fx, y - fy);
    let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let (ix, iy) = (fx as i32, fy as i32);
    let a = lattice(ix, iy, seed);
    let b = lattice(ix.wrapping_add(1), iy, seed);
    let c = lattice(ix, iy.wrapping_add(1), seed);
    let d = lattice(ix.wrapping_add(1), iy.wrapping_add(1), seed);
    let top = a + (b - a) * sx;
    let bot = c + (d - c) * sx;
    top + (bot - top) * sy
}

/// The grain added to an encoded pixel at frame position `p` (E-normalized × aspect ×
/// long edge in developed px: the same spot of the image at any zoom or tile).
#[inline]
pub fn grain_px(e: [f32; 3], p: [f32; 2], g: &GrainPlan) -> [f32; 3] {
    let (x, y) = (p[0] / g.cell, p[1] / g.cell);
    let n = (1.0 - g.rough) * value_noise(x, y, 1)
        + g.rough * value_noise(x * 2.17 + 13.1, y * 2.17 + 7.3, 2);
    let l = luma(e).clamp(0.0, 1.0);
    let d = g.amp * n * (0.3 + 2.8 * l * (1.0 - l));
    e.map(|v| v + d)
}

/// Frame position of developed pixel position `(x, y)` (pixel centres at +0.5).
#[inline]
pub fn grain_pos(norm: &[f32; 6], iso: [f32; 2], long: f32, x: f32, y: f32) -> [f32; 2] {
    let u = norm[0] * x + norm[1] * y + norm[2];
    let v = norm[3] * x + norm[4] * y + norm[5];
    [u * iso[0] * long, v * iso[1] * long]
}
