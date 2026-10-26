//! Layer mask rasterization.
//!
//! Shapes are combined in order (`Add` = union via max, `Subtract` = `m·(1−a)`,
//! `Intersect` = `m·a`), then the Gaussian feather is applied, then `invert`, then the
//! luminance-range refinement (evaluated on the layer's input pixel).
//!
//! Coordinates: shape points are normalized in the E frame (see crate docs); distances
//! (brush size, radial radii) are in units of the E-frame long edge so circles stay round.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use lt_core::Size;
use lt_core::adjust::{AiTarget, BrushStroke, LumaRange, Mask, MaskOp, MaskShape};
use rayon::prelude::*;

use crate::color_edit::{OkRange, encoded_to_oklch, luma};
use crate::geometry::Geometry;
use crate::local::{GuidedParams, Plane, block_means, gaussian_blur, gaussian_kernel, upsample_at};
use crate::tone::smoothstep;
use lt_core::adjust::MAX_PLANE_FEATHER;

/// An 8-bit coverage bitmap in the E frame (e.g. an Apple Vision segmentation).
#[derive(Clone, PartialEq, Eq)]
pub struct MaskBitmap {
    pub size: Size,
    /// Row-major, 0 = not selected, 255 = fully selected.
    pub data: Vec<u8>,
    /// Identity of the content, changed whenever the mask changes (e.g. a counter the
    /// app bumps per regeneration). 0 = unknown: the pipeline hashes the data instead.
    pub version: u64,
}

impl MaskBitmap {
    /// A bitmap whose content the pipeline identifies by hashing it.
    pub fn new(size: Size, data: Vec<u8>) -> Self {
        Self {
            size,
            data,
            version: 0,
        }
    }

    /// What identifies this bitmap's content: its size and version, or a hash of the
    /// data when no version is given.
    pub(crate) fn content_key(&self) -> (u32, u32, bool, u64) {
        let (w, h) = (self.size.width, self.size.height);
        if self.version != 0 {
            return (w, h, true, self.version);
        }
        // 64-bit FNV-style over 8-byte words (a 2 MP mask hashes in ~0.2 ms).
        let (words, rest) = self.data.as_chunks::<8>();
        let mut hsh = 0xcbf2_9ce4_8422_2325u64 ^ self.data.len() as u64;
        for w8 in words {
            hsh = (hsh ^ u64::from_le_bytes(*w8)).wrapping_mul(0x100_0000_01b3);
            hsh ^= hsh >> 29;
        }
        for b in rest {
            hsh = (hsh ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3);
        }
        (w, h, false, hsh)
    }
}

impl fmt::Debug for MaskBitmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaskBitmap")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl MaskBitmap {
    /// Bilinear coverage (0..=1) at E-normalized `(u, v)`; 0 for malformed bitmaps.
    pub fn sample(&self, u: f32, v: f32) -> f32 {
        let (w, h) = (self.size.width as usize, self.size.height as usize);
        if w == 0 || h == 0 || self.data.len() != w * h {
            return 0.0;
        }
        let fx = (u * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
        let fy = (v * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
        let (x0, y0) = (fx as usize, fy as usize);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let p = |x: usize, y: usize| f32::from(self.data[y * w + x]) / 255.0;
        let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * tx;
        let bot = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * tx;
        top + (bot - top) * ty
    }
}

/// Supplies AI mask bitmaps for the image being rendered (implemented on the Swift side).
pub trait MaskSource: Send + Sync {
    /// The target's mask over the whole image (for people targets: all people).
    fn ai_mask(&self, target: AiTarget) -> Option<Arc<MaskBitmap>>;

    /// A people target's mask for one person (`Some(index)`, see
    /// `MaskShape::Ai::person`), or [`MaskSource::ai_mask`] for `None`. Sources that
    /// don't tell people apart have no single-person masks.
    fn ai_person_mask(&self, target: AiTarget, person: Option<u32>) -> Option<Arc<MaskBitmap>> {
        match person {
            None => self.ai_mask(target),
            Some(_) => None,
        }
    }

    /// The photo's depth map for the lens blur, in the E frame: 255 = nearest, 0 =
    /// farthest (relative inverse depth, the subject's edges refined by the app's subject
    /// masks). `None` → the depth lens blur renders nothing.
    fn depth_map(&self) -> Option<Arc<MaskBitmap>> {
        None
    }
}

/// Shape evaluated analytically per pixel (CPU and GPU alike).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShapeKind {
    Fill,
    /// Start and end in isotropic long-edge units.
    Linear {
        s: [f32; 2],
        e: [f32; 2],
    },
    /// Center (iso units), radii (long-edge units), rotation (cos, sin), feather 0..=1.
    Radial {
        c: [f32; 2],
        r: [f32; 2],
        cs: [f32; 2],
        feather: f32,
    },
    ColorRange(OkRange),
    /// Index into [`CompiledMask::rasters`] (brush strokes, AI bitmaps; E frame).
    Raster(usize),
    /// An output-space plane built per render (index into [`CompiledMask::planes`]),
    /// combined (max) with an E-frame raster when given (a brush with plain and
    /// auto-masked strokes).
    Plane {
        plane: usize,
        raster: Option<usize>,
    },
}

/// How an output-space plane of a mask is built (it depends on the image).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlaneSrc {
    /// A colour range, refined edge-aware: its weight guided-filtered over `gp` (see
    /// [`cr_params`]) on a guide that sees both tone and the range's hue
    /// ([`cr_guide`]), which anti-aliases the key and spreads a feather only within
    /// regions of similar tone and colour (no halo onto the background).
    ColorRange { range: OkRange, gp: GuidedParams },
    /// Auto-masked brush strokes: the painted footprint (raster `raster`) keeps only the
    /// tones similar to those under the strokes' centre line (raster `core`): the
    /// reference luma at each pixel is the core-weighted local mean over the block grid
    /// `gp`; `sigma` is the shape's feather.
    Auto {
        raster: usize,
        core: usize,
        gp: GuidedParams,
        sigma: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompiledShape {
    pub op: MaskOp,
    pub kind: ShapeKind,
}

/// A mask ready to evaluate at output resolution.
#[derive(Debug, Clone)]
pub struct CompiledMask {
    pub shapes: Vec<CompiledShape>,
    pub invert: bool,
    /// Feather Gaussian sigma in output pixels (0 = none).
    pub feather_px: f32,
    pub luma: Option<LumaRange>,
    /// Colour-range refinement (after invert, like the luma range).
    pub color: Option<OkRange>,
    /// E-frame coverage rasters referenced by [`ShapeKind::Raster`].
    pub rasters: Vec<Raster>,
    /// Output-space planes referenced by [`ShapeKind::Plane`].
    pub planes: Vec<PlaneSrc>,
}

/// Raster id bits for derived rasters (never collide with a shape index).
const FEATHERED: usize = 1 << 20;
const AUTO: usize = 1 << 21;
const CORE: usize = 1 << 22;

/// Brush rasters live in the E frame at this long-edge resolution (or the E frame's own,
/// if smaller), so pans, zooms and crop drags only resample them.
pub const BRUSH_LONG_EDGE: f64 = 3072.0;

/// Raster size for an E frame of `e_size` sensor pixels.
pub fn raster_dims(e_size: [f64; 2]) -> (usize, usize) {
    let long = e_size[0].max(e_size[1]).max(1.0);
    let f = (BRUSH_LONG_EDGE / long).min(1.0);
    (
        ((e_size[0] * f).round() as usize).max(1),
        ((e_size[1] * f).round() as usize).max(1),
    )
}

/// Identity of a cached raster (for GPU residency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RasterId {
    pub image: u64,
    pub layer: u64,
    pub shape: usize,
}

/// A coverage plane in the E frame, sampled with bilinear filtering at `(u, v)`.
#[derive(Debug, Clone)]
pub struct Raster {
    pub id: RasterId,
    /// Changes whenever the plane is rebuilt or resized (not on incremental strokes).
    pub generation: u64,
    /// Changes whenever the content changes (incremental strokes included).
    pub version: u64,
    pub plane: Arc<Plane>,
    /// Gaussian feather (σ in raster pixels, 0 = none) still to apply to `plane`: the GPU
    /// blurs its resident copy (only the changed rows), the CPU reference blurs with
    /// [`feather`] ([`resolve_feathers`]).
    pub feather: f32,
    /// A brush raster stamped on the GPU (a GPU-resident cache): `plane` has its size but
    /// no data; the GPU keeps combined, base and stroke planes and applies the
    /// [`BrushOp`]s the cache records.
    pub gpu_brush: bool,
}

/// One step of GPU brush rasterization, as [`BrushRaster`] would compute it on the CPU.
#[derive(Debug, Clone, PartialEq)]
pub enum BrushOp {
    /// Zero all three planes (a new or rebuilt raster).
    Clear,
    /// Commit the stroke in progress: `base = combined`, `stroke = 0` over the box.
    Fold([usize; 4]),
    /// Max-stamp capsules into `stroke` over `bbox` and recombine it:
    /// `combined = erase ? base·(1 − stroke) : base + stroke·(1 − base)`.
    Stamp {
        /// Segments (a.xy, b.xy) in raster pixels.
        segs: Vec<[f32; 4]>,
        /// Per segment: its box (x0, y0, x1, y1).
        boxes: Vec<[usize; 4]>,
        bbox: [usize; 4],
        radius: f32,
        hard: f32,
        flow: f32,
        erase: bool,
    },
}

impl BrushOp {
    /// Rows the op changes.
    pub fn rows(&self, h: usize) -> (usize, usize) {
        match self {
            Self::Clear => (0, h),
            Self::Fold(b) | Self::Stamp { bbox: b, .. } => (b[1], b[3]),
        }
    }
}

impl Raster {
    /// Bilinear coverage at E-normalized `(u, v)`, clamped to the edge.
    #[inline]
    pub fn sample(&self, u: f32, v: f32) -> f32 {
        sample_plane(&self.plane, u, v)
    }
}

#[inline]
pub(crate) fn sample_plane(p: &Plane, u: f32, v: f32) -> f32 {
    let fx = (u * p.w as f32 - 0.5).clamp(0.0, (p.w - 1) as f32);
    let fy = (v * p.h as f32 - 0.5).clamp(0.0, (p.h - 1) as f32);
    let (x0, y0) = (fx as usize, fy as usize);
    let (x1, y1) = ((x0 + 1).min(p.w - 1), (y0 + 1).min(p.h - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let a = p.data[y0 * p.w + x0];
    let b = p.data[y0 * p.w + x1];
    let c = p.data[y1 * p.w + x0];
    let d = p.data[y1 * p.w + x1];
    let top = a + (b - a) * tx;
    let bot = c + (d - c) * tx;
    top + (bot - top) * ty
}

/// An incrementally rasterized brush shape. Strokes are appended while painting; only the
/// new segments are stamped, so a painting frame costs O(new dabs), not O(stroke).
#[derive(Debug)]
struct BrushRaster {
    combined: Arc<Plane>,
    /// All completed strokes combined.
    base: Plane,
    /// Coverage (max over dabs) of the stroke in progress.
    stroke: Plane,
    stroke_bbox: Option<[usize; 4]>,
    done: Vec<BrushStroke>,
    generation: u64,
    /// Bumped by every change of `combined`.
    version: u64,
    dirty: Option<(usize, usize)>,
    /// GPU-resident: no CPU planes, the steps are recorded in `ops` instead.
    gpu: bool,
    ops: Vec<BrushOp>,
    /// The three planes count against the memory budget (CPU mode).
    _acct: crate::cache::Account,
}

fn same_brush(a: &BrushStroke, b: &BrushStroke) -> bool {
    a.size.to_bits() == b.size.to_bits()
        && a.hardness.to_bits() == b.hardness.to_bits()
        && a.flow.to_bits() == b.flow.to_bits()
        && a.erase == b.erase
}

fn union_bbox(a: Option<[usize; 4]>, b: [usize; 4]) -> [usize; 4] {
    match a {
        None => b,
        Some(a) => [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ],
    }
}

impl BrushRaster {
    fn with_mode(w: usize, h: usize, generation: u64, gpu: bool) -> Self {
        let plane = |w, h| {
            if gpu {
                // Size only: the GPU holds the data.
                Plane {
                    w,
                    h,
                    data: Vec::new(),
                }
            } else {
                Plane::new(w, h)
            }
        };
        Self {
            combined: Arc::new(plane(w, h)),
            base: plane(w, h),
            stroke: plane(w, h),
            stroke_bbox: None,
            done: Vec::new(),
            generation,
            version: 0,
            dirty: Some((0, h)),
            gpu,
            ops: if gpu {
                vec![BrushOp::Clear]
            } else {
                Vec::new()
            },
            _acct: crate::cache::Account::new(
                crate::cache::Use::MaskRaster,
                if gpu { 0 } else { (3 * w * h * 4) as u64 },
            ),
        }
    }

    fn mark(&mut self, b: [usize; 4]) {
        self.version += 1;
        self.dirty = Some(match self.dirty {
            None => (b[1], b[3]),
            Some((a, z)) => (a.min(b[1]), z.max(b[3])),
        });
    }

    /// Bring the raster up to `strokes`. Returns false when a full rebuild was needed.
    fn update(&mut self, strokes: &[BrushStroke]) -> bool {
        let n = self.done.len();
        let incremental = strokes.len() >= n
            && self
                .done
                .iter()
                .zip(strokes)
                .take(n.saturating_sub(1))
                .all(|(a, b)| a == b)
            && (n == 0 || {
                let (old, new) = (&self.done[n - 1], &strokes[n - 1]);
                same_brush(old, new) && new.points.starts_with(&old.points)
            });
        if !incremental {
            let (w, h) = (self.base.w, self.base.h);
            *self = Self::with_mode(w, h, self.generation + 1, self.gpu);
        }
        let n = self.done.len();
        if n > 0 {
            let old_len = self.done[n - 1].points.len();
            let s = &strokes[n - 1];
            if s.points.len() > old_len {
                // Continue from the last stamped point so the capsule chain is unbroken.
                let from = old_len.saturating_sub(1);
                self.stamp(s, &s.points[from..], s.erase);
                self.done[n - 1] = s.clone();
            }
        }
        for s in &strokes[n..] {
            self.fold();
            self.stamp(s, &s.points, s.erase);
            self.done.push(s.clone());
        }
        incremental
    }

    /// Commit the stroke in progress into `base` and clear it.
    fn fold(&mut self) {
        if self.gpu {
            if let Some(b) = self.stroke_bbox.take() {
                self.ops.push(BrushOp::Fold(b));
            }
            return;
        }
        if let Some(b) = self.stroke_bbox.take() {
            let w = self.base.w;
            for y in b[1]..b[3] {
                let r = y * w + b[0]..y * w + b[2];
                self.base.data[r.clone()].copy_from_slice(&self.combined.data[r.clone()]);
                self.stroke.data[r].fill(0.0);
            }
        }
    }

    /// Stamp the capsules through `pts` into the stroke plane and recombine that area.
    fn stamp(&mut self, s: &BrushStroke, pts: &[[f32; 2]], erase: bool) {
        let (w, h) = (self.base.w, self.base.h);
        if s.size.is_nan() || s.size <= 0.0 {
            return;
        }
        let long = w.max(h) as f32;
        let radius = 0.5 * s.size * long;
        let hard = s.hardness.clamp(0.0, 1.0);
        let flow = s.flow.clamp(0.0, 1.0);
        let pts: Vec<[f32; 2]> = pts
            .iter()
            .filter(|p| p[0].is_finite() && p[1].is_finite())
            .map(|p| [p[0] * w as f32, p[1] * h as f32])
            .collect();
        if pts.is_empty() {
            return;
        }
        let segs: Vec<([f32; 2], [f32; 2])> = if pts.len() == 1 {
            vec![(pts[0], pts[0])]
        } else {
            pts.windows(2).map(|p| (p[0], p[1])).collect()
        };
        // All segments at once: one parallel pass over the union of their boxes (each row
        // only tests the segments whose box it crosses), then one recombine pass.
        let boxed: Vec<([f32; 2], [f32; 2], [usize; 4])> = segs
            .into_iter()
            .filter_map(|(a, b)| {
                let bx0 = (a[0].min(b[0]) - radius).floor().max(0.0) as usize;
                let by0 = (a[1].min(b[1]) - radius).floor().max(0.0) as usize;
                let bx1 = ((a[0].max(b[0]) + radius).ceil().max(0.0) as usize).min(w);
                let by1 = ((a[1].max(b[1]) + radius).ceil().max(0.0) as usize).min(h);
                (bx0 < bx1 && by0 < by1).then_some((a, b, [bx0, by0, bx1, by1]))
            })
            .collect();
        let Some(ub) = boxed
            .iter()
            .map(|s| s.2)
            .reduce(|u, b| union_bbox(Some(u), b))
        else {
            return;
        };
        if self.gpu {
            self.ops.push(BrushOp::Stamp {
                segs: boxed
                    .iter()
                    .map(|(a, b, _)| [a[0], a[1], b[0], b[1]])
                    .collect(),
                boxes: boxed.iter().map(|s| s.2).collect(),
                bbox: ub,
                radius,
                hard,
                flow,
                erase,
            });
            self.stroke_bbox = Some(union_bbox(self.stroke_bbox, ub));
            self.mark(ub);
            return;
        }
        let stroke_rows = &mut self.stroke.data[ub[1] * w..ub[3] * w];
        crate::par::run(|| {
            stroke_rows
                .par_chunks_mut(w)
                .enumerate()
                .for_each(|(ry, row)| {
                    let y = ub[1] + ry;
                    let py = y as f32 + 0.5;
                    for (a, b, bb) in &boxed {
                        if y < bb[1] || y >= bb[3] {
                            continue;
                        }
                        for (x, m) in row.iter_mut().enumerate().take(bb[2]).skip(bb[0]) {
                            let d = seg_dist([x as f32 + 0.5, py], *a, *b);
                            if d < radius {
                                *m = m.max(flow * (1.0 - smoothstep(hard, 1.0, d / radius)));
                            }
                        }
                    }
                });
        });
        self.stroke_bbox = Some(union_bbox(self.stroke_bbox, ub));
        let (base, stroke) = (&self.base, &self.stroke);
        let combined = Arc::make_mut(&mut self.combined);
        let rows = &mut combined.data[ub[1] * w..ub[3] * w];
        crate::par::run(|| {
            rows.par_chunks_mut(w).enumerate().for_each(|(ry, row)| {
                let o = (ub[1] + ry) * w;
                for x in ub[0]..ub[2] {
                    let (m, a) = (base.data[o + x], stroke.data[o + x]);
                    row[x] = if erase {
                        m * (1.0 - a)
                    } else {
                        m + a * (1.0 - m)
                    };
                }
            });
        });
        self.mark(ub);
    }
}

/// Cache of E-frame mask rasters: incremental brush rasters per (image, layer, shape) and
/// converted AI bitmaps. Bounded to `CAP` brush entries (least recently used dropped).
#[derive(Debug, Default)]
pub struct RasterCache {
    /// Brush rasters are stamped on the GPU (the GPU renderer's cache).
    gpu: bool,
    brushes: HashMap<RasterId, (u64, BrushRaster)>,
    ai: HashMap<RasterId, AiEntry>,
    /// Feathered copies: (source generation, source version, σ bits) → plane.
    feathered: HashMap<RasterId, FeatheredRaster>,
    tick: u64,
    /// Full brush rasterizations (not incremental updates), for tests and diagnostics.
    pub misses: u64,
}

impl RasterCache {
    const CAP: usize = 32;

    pub fn clear(&mut self) {
        self.brushes.clear();
        self.ai.clear();
        self.feathered.clear();
    }

    /// Drop everything cached for `image`.
    pub fn evict(&mut self, image: u64) {
        self.brushes.retain(|k, _| k.image != image);
        self.ai.retain(|k, _| k.image != image);
        self.feathered.retain(|k, _| k.image != image);
    }

    /// The cached feathered copy of `src` at `sigma`, if it is current.
    fn feathered_get(&self, src: &Raster, sigma: f32) -> Option<Raster> {
        let id = RasterId {
            shape: src.id.shape | FEATHERED,
            ..src.id
        };
        let f = self.feathered.get(&id)?;
        (f.key == (src.generation, src.version, sigma.to_bits())).then(|| Raster {
            id,
            generation: f.generation,
            version: f.generation,
            plane: Arc::clone(&f.plane),
            feather: 0.0,
            gpu_brush: false,
        })
    }

    /// Store a feathered copy of `src` (blurred by the caller, without the lock).
    fn feathered_put(&mut self, src: &Raster, sigma: f32, plane: Plane) -> Raster {
        let id = RasterId {
            shape: src.id.shape | FEATHERED,
            ..src.id
        };
        let generation = self.feathered.get(&id).map_or(0, |f| f.generation + 1);
        let plane = Arc::new(plane);
        let acct = crate::cache::Account::new(
            crate::cache::Use::MaskRaster,
            (plane.data.len() * 4) as u64,
        );
        self.feathered.insert(
            id,
            FeatheredRaster {
                key: (src.generation, src.version, sigma.to_bits()),
                generation,
                plane: Arc::clone(&plane),
                _acct: acct,
            },
        );
        Raster {
            id,
            generation,
            version: generation,
            plane,
            feather: 0.0,
            gpu_brush: false,
        }
    }

    /// A cache whose brush rasters are stamped on the GPU (see [`BrushOp`]).
    pub fn gpu_resident() -> Self {
        Self {
            gpu: true,
            ..Self::default()
        }
    }

    /// The GPU steps recorded for brush raster `id` since the last call (in order).
    pub fn take_ops(&mut self, id: RasterId) -> Vec<BrushOp> {
        self.brushes
            .get_mut(&id)
            .map(|(_, b)| std::mem::take(&mut b.ops))
            .unwrap_or_default()
    }

    /// All steps that rebuild brush raster `id` from scratch (its GPU copy is new), and
    /// no pending ones.
    pub fn replay_ops(&mut self, id: RasterId) -> Vec<BrushOp> {
        let Some((_, b)) = self.brushes.get_mut(&id) else {
            return Vec::new();
        };
        b.ops.clear();
        let mut fresh = BrushRaster::with_mode(b.base.w, b.base.h, b.generation, true);
        fresh.update(&b.done);
        fresh.ops
    }

    /// Rows of a brush raster changed since the last call (for incremental GPU uploads).
    pub fn take_dirty(&mut self, id: RasterId) -> Option<(usize, usize)> {
        self.brushes.get_mut(&id).and_then(|(_, b)| b.dirty.take())
    }

    /// Take `id`'s brush raster out of the cache (a new one if absent or resized) to
    /// update it without holding the cache's lock; put it back with [`Self::put_brush`].
    fn take_brush(&mut self, id: RasterId, dims: (usize, usize)) -> BrushRaster {
        match self.brushes.remove(&id) {
            Some((_, b)) if (b.base.w, b.base.h) == dims => b,
            Some((_, b)) => BrushRaster::with_mode(dims.0, dims.1, b.generation + 1, self.gpu),
            None => BrushRaster::with_mode(dims.0, dims.1, 0, self.gpu),
        }
    }

    /// Return an updated brush raster (a concurrent update of the same raster may have
    /// been put back meanwhile: the latest wins). Bounded to `CAP` (least recent dropped).
    fn put_brush(&mut self, id: RasterId, b: BrushRaster, rebuilt: bool) {
        self.tick += 1;
        if rebuilt {
            self.misses += 1;
        }
        self.brushes.insert(id, (self.tick, b));
        while self.brushes.len() > Self::CAP {
            let Some(old) = self
                .brushes
                .iter()
                .filter(|(k, _)| **k != id)
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| *k)
            else {
                break;
            };
            self.brushes.remove(&old);
        }
    }

    /// The raster of an AI mask. Cached per shape by content (the bitmap's version, or
    /// a hash of its data), never by address: the cache holds the bitmap it converted,
    /// so an `Arc` handed in again is recognized by pointer, and a new `Arc` (lt-ffi makes
    /// one per render) by its content key.
    fn ai_raster(&mut self, id: RasterId, bmp: Option<&Arc<MaskBitmap>>) -> Raster {
        if let Some(e) = self.ai.get(&id) {
            let same = match (bmp, &e.source) {
                (None, None) => true,
                (Some(b), Some(s)) => Arc::ptr_eq(b, s) || b.content_key() == s.content_key(),
                _ => false,
            };
            if same {
                let (generation, plane) = (e.generation, Arc::clone(&e.plane));
                if let (Some(b), Some(e)) = (bmp, self.ai.get_mut(&id)) {
                    // Hold the latest Arc (cheap pointer hits next time).
                    e.source = Some(Arc::clone(b));
                }
                return Raster {
                    id,
                    generation,
                    version: generation,
                    plane,
                    feather: 0.0,
                    gpu_brush: false,
                };
            }
        }
        let plane = match bmp.map(AsRef::as_ref) {
            Some(b)
                if b.size.width > 0
                    && b.size.height > 0
                    && b.data.len() == (b.size.width * b.size.height) as usize =>
            {
                Plane {
                    w: b.size.width as usize,
                    h: b.size.height as usize,
                    data: b.data.iter().map(|v| f32::from(*v) / 255.0).collect(),
                }
            }
            _ => Plane::new(1, 1),
        };
        let generation = self.ai.get(&id).map_or(0, |e| e.generation + 1);
        let acct = crate::cache::Account::new(
            crate::cache::Use::MaskRaster,
            (plane.data.len() * 4) as u64,
        );
        let plane = Arc::new(plane);
        self.ai.insert(
            id,
            AiEntry {
                source: bmp.cloned(),
                generation,
                plane: Arc::clone(&plane),
                _acct: acct,
            },
        );
        Raster {
            id,
            generation,
            version: generation,
            plane,
            feather: 0.0,
            gpu_brush: false,
        }
    }
}

/// A converted AI mask and the bitmap it came from.
#[derive(Debug)]
struct AiEntry {
    source: Option<Arc<MaskBitmap>>,
    generation: u64,
    plane: Arc<Plane>,
    _acct: crate::cache::Account,
}

#[derive(Debug)]
struct FeatheredRaster {
    key: (u64, u64, u32),
    generation: u64,
    plane: Arc<Plane>,
    _acct: crate::cache::Account,
}

/// Bring brush raster `id` up to `strokes`: taken out of the cache, stamped (in
/// parallel) with no lock held, and put back.
fn brush_raster(
    cache: &std::sync::Mutex<RasterCache>,
    id: RasterId,
    strokes: &[BrushStroke],
    dims: (usize, usize),
) -> Raster {
    let mut b = crate::par::hold(cache).take_brush(id, dims);
    let empty_before = b.done.is_empty();
    let rebuilt = !b.update(strokes) || (empty_before && !strokes.is_empty());
    let r = Raster {
        id,
        generation: b.generation,
        version: b.version,
        gpu_brush: b.gpu,
        plane: Arc::clone(&b.combined),
        feather: 0.0,
    };
    crate::par::hold(cache).put_brush(id, b, rebuilt);
    r
}

/// Apply the rasters' pending feathers on the CPU (the reference; cached per source
/// version, blurred with no lock held). The GPU renderer skips this and blurs on the GPU.
pub fn resolve_feathers(cm: &mut CompiledMask, cache: &std::sync::Mutex<RasterCache>) {
    for r in &mut cm.rasters {
        if r.feather > 0.0 {
            let sigma = r.feather;
            *r = feathered_raster(
                cache,
                &Raster {
                    feather: 0.0,
                    ..r.clone()
                },
                sigma,
            );
        }
    }
}

/// `src` blurred by a Gaussian of `sigma` raster pixels, cached until `src` changes (the
/// blur runs with no lock held).
fn feathered_raster(cache: &std::sync::Mutex<RasterCache>, src: &Raster, sigma: f32) -> Raster {
    if let Some(r) = crate::par::hold(cache).feathered_get(src, sigma) {
        return r;
    }
    let plane = feather(&src.plane, sigma);
    crate::par::hold(cache).feathered_put(src, sigma, plane)
}

/// Shapes with NaN/Inf geometry are ignored rather than poisoning the whole mask.
fn shape_is_finite(shape: &MaskShape) -> bool {
    let ok = |v: &[f32]| v.iter().all(|x| x.is_finite());
    match shape {
        MaskShape::Fill { .. } | MaskShape::Ai { .. } | MaskShape::Brush { .. } => true,
        MaskShape::Linear { start, end, .. } => ok(start) && ok(end),
        MaskShape::Radial {
            center,
            radius,
            angle_deg,
            feather,
            ..
        } => ok(center) && ok(radius) && ok(&[*angle_deg, *feather]),
        MaskShape::ColorRange { range, .. } => {
            ok(&[range.center_hue, range.width, range.smoothness])
        }
    }
}

/// Compile a layer's mask for this geometry. Brush strokes are rasterized incrementally
/// in the E frame and AI bitmaps converted (both cached in `cache`, keyed by
/// `image`/`layer`); analytic shapes are evaluated per output pixel later.
///
/// The cache is locked only to look up and store: brush stamping (parallel, on the
/// pipeline pool) runs with no lock held (see [`crate::par::hold`]).
pub fn compile(
    mask: &Mask,
    layer: u64,
    geo: &Geometry,
    source: Option<&Arc<dyn MaskSource>>,
    image: u64,
    cache: &std::sync::Mutex<RasterCache>,
) -> CompiledMask {
    let iso = geo.iso();
    let to_iso = |p: [f32; 2]| [p[0] * iso[0], p[1] * iso[1]];
    let dims = raster_dims(geo.e_size);
    let long = geo.long_edge_out();
    // Feather in output pixels (at most MAX_PLANE_FEATHER: tiles have that much
    // context) / raster pixels.
    let sigma_out = |f: f32| {
        if f.is_finite() {
            (f.min(MAX_PLANE_FEATHER) * long).max(0.0)
        } else {
            0.0
        }
    };
    let sigma_raster = |f: f32| {
        if f.is_finite() {
            (f * dims.0.max(dims.1) as f32).max(0.0)
        } else {
            0.0
        }
    };
    let origin = geo.origin_out.map(|v| v.round() as i64);
    let mut rasters = Vec::new();
    let mut planes = Vec::new();
    let mut shapes = Vec::with_capacity(mask.shapes.len());
    for (idx, shape) in mask
        .shapes
        .iter()
        .enumerate()
        .filter(|(_, s)| shape_is_finite(s))
    {
        let id = RasterId {
            image,
            layer,
            shape: idx,
        };
        let (op, kind) = match shape {
            MaskShape::Fill { op } => (*op, ShapeKind::Fill),
            MaskShape::Linear { op, start, end } => (
                *op,
                ShapeKind::Linear {
                    s: to_iso(*start),
                    e: to_iso(*end),
                },
            ),
            MaskShape::Radial {
                op,
                center,
                radius,
                angle_deg,
                feather,
            } => {
                let (s, c) = angle_deg.to_radians().sin_cos();
                (
                    *op,
                    ShapeKind::Radial {
                        c: to_iso(*center),
                        r: *radius,
                        cs: [c, s],
                        feather: feather.clamp(0.0, 1.0),
                    },
                )
            }
            MaskShape::ColorRange { op, range, feather } => {
                planes.push(PlaneSrc::ColorRange {
                    range: OkRange::new(range),
                    gp: cr_params(sigma_out(*feather), long).anchored(origin),
                });
                (
                    *op,
                    ShapeKind::Plane {
                        plane: planes.len() - 1,
                        raster: None,
                    },
                )
            }
            MaskShape::Brush {
                op,
                strokes,
                feather,
            } => {
                let sr = sigma_raster(*feather);
                let feathered = |r: Raster| Raster {
                    feather: if sr >= 0.5 { sr } else { 0.0 },
                    ..r
                };
                if strokes.iter().any(|s| s.auto_mask && !s.erase) {
                    let plain: Vec<BrushStroke> =
                        strokes.iter().filter(|s| !s.auto_mask).cloned().collect();
                    let auto: Vec<BrushStroke> = strokes
                        .iter()
                        .filter(|s| s.auto_mask || s.erase)
                        .cloned()
                        .collect();
                    let plain_idx = plain.iter().any(|s| !s.erase).then(|| {
                        rasters.push(feathered(brush_raster(cache, id, &plain, dims)));
                        rasters.len() - 1
                    });
                    let auto_id = RasterId {
                        shape: idx | AUTO,
                        ..id
                    };
                    rasters.push(brush_raster(cache, auto_id, &auto, dims));
                    // The centre line: the same strokes at a quarter of their size.
                    let core: Vec<BrushStroke> = auto
                        .iter()
                        .map(|s| BrushStroke {
                            size: s.size * AUTO_CORE,
                            hardness: 1.0,
                            flow: 1.0,
                            ..s.clone()
                        })
                        .collect();
                    let core_id = RasterId {
                        shape: idx | AUTO | CORE,
                        ..id
                    };
                    rasters.push(brush_raster(cache, core_id, &core, dims));
                    let size = strokes
                        .iter()
                        .filter(|s| s.auto_mask && !s.erase && s.size.is_finite())
                        .map(|s| s.size)
                        .fold(f32::MAX, f32::min);
                    // The reference averages over about the brush radius, in normalized
                    // units (the same neighbourhood at every scale), up to AUTO_REF_MAX.
                    let radius = auto_radius(size, long);
                    planes.push(PlaneSrc::Auto {
                        raster: rasters.len() - 2,
                        core: rasters.len() - 1,
                        gp: GuidedParams::for_radius(radius, 0.0).anchored(origin),
                        sigma: sigma_out(*feather),
                    });
                    (
                        *op,
                        ShapeKind::Plane {
                            plane: planes.len() - 1,
                            raster: plain_idx,
                        },
                    )
                } else {
                    rasters.push(feathered(brush_raster(cache, id, strokes, dims)));
                    (*op, ShapeKind::Raster(rasters.len() - 1))
                }
            }
            MaskShape::Ai {
                op,
                target,
                feather,
                person,
            } => {
                let person = person.filter(|_| target.is_people());
                let bmp = source.and_then(|s| s.ai_person_mask(*target, person));
                let r = crate::par::hold(cache).ai_raster(id, bmp.as_ref());
                let sr = sigma_raster(*feather);
                rasters.push(Raster {
                    feather: if sr >= 0.5 { sr } else { 0.0 },
                    ..r
                });
                (*op, ShapeKind::Raster(rasters.len() - 1))
            }
        };
        shapes.push(CompiledShape { op, kind });
    }
    let feather_px = if mask.feather.is_finite() {
        (mask.feather.min(MAX_PLANE_FEATHER) * geo.long_edge_out()).max(0.0)
    } else {
        0.0
    };
    CompiledMask {
        shapes,
        invert: mask.invert,
        feather_px,
        luma: mask.luma_range,
        color: mask.color_range.as_ref().map(OkRange::new),
        rasters,
        planes,
    }
}

/// Colour-range refinement: the smallest guided-filter radius (share of the long edge),
/// what anti-aliases an unfeathered key.
pub const CR_MIN_RADIUS: f32 = 0.002;
/// Colour-range refinement: guided-filter ε (encoded luma variance): tone steps well
/// above √ε ≈ 0.045 are edges the selection keeps to.
pub const CR_EPS: f32 = 0.002;

/// Colour-range refinement: weight of the range's soft hue proximity in the guide.
pub const CR_GUIDE_HUE: f32 = 0.35;

/// Colour-range refinement: radius (output pixels) of the smoothing of the guide the
/// filter's coefficients are applied to ([`cr_smooth_guide`]). The guide's pixel noise
/// (chroma noise near the hue range's edge, fine texture) would otherwise come back as
/// speckle in the selection.
pub const CR_GUIDE_SMOOTH: usize = 2;

/// Range σ (guide units) of that smoothing: guide steps well above it (a colour or tone
/// edge) are kept, pixel noise below it is averaged away.
pub const CR_GUIDE_SIGMA: f32 = 0.2;

/// The colour-range refinement's guide for an encoded pixel: its luma plus
/// [`CR_GUIDE_HUE`] × a soft version of the range's weight (the full smoothness), so
/// the filter keeps to both tone edges and edges between the selected hue and others.
#[inline]
pub fn cr_guide(range: &OkRange, rgb: [f32; 3]) -> f32 {
    let lch = encoded_to_oklch(rgb);
    let soft = OkRange {
        smooth: 1.0,
        ..*range
    };
    luma(rgb) + CR_GUIDE_HUE * soft.weight(lch[2], lch[1])
}

/// The guide the colour-range filter's coefficients are applied to: a small bilateral
/// smoothing ([`CR_GUIDE_SMOOTH`] radius, [`CR_GUIDE_SIGMA`] range, replicated edges)
/// that removes pixel noise but keeps tone and colour edges.
pub fn cr_smooth_guide(guide: &[f32], w: usize, h: usize) -> Vec<f32> {
    let r = CR_GUIDE_SMOOTH as isize;
    let k = -0.5 / (CR_GUIDE_SIGMA * CR_GUIDE_SIGMA);
    let at = |x: isize, y: isize| {
        guide[y.clamp(0, h as isize - 1) as usize * w + x.clamp(0, w as isize - 1) as usize]
    };
    (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (x, y) = ((i % w) as isize, (i / w) as isize);
            let c = guide[i];
            let (mut sum, mut wsum) = (0.0f32, 0.0f32);
            for dy in -r..=r {
                for dx in -r..=r {
                    let g = at(x + dx, y + dy);
                    let wt = (k * (g - c) * (g - c)).exp();
                    sum += wt * g;
                    wsum += wt;
                }
            }
            sum / wsum
        })
        .collect()
}

/// Guided-filter parameters of a colour range with feather `sigma` (output pixels) at
/// long edge `long`: a box radius of about the Gaussian's (√3·σ), at least
/// [`CR_MIN_RADIUS`].
pub fn cr_params(sigma: f32, long: f32) -> GuidedParams {
    GuidedParams::for_radius((1.7 * sigma).max(CR_MIN_RADIUS * long), CR_EPS)
}

/// Largest radius (share of the long edge) of an auto-masked stroke's tone reference:
/// its neighbourhood must fit the context a tile or 1:1 region renders with.
pub const AUTO_REF_MAX: f32 = 0.03;

/// Output-pixel radius of an auto-masked stroke's tone reference for brush `size`
/// (the smallest auto stroke's) at long edge `long`: half the size in normalized units,
/// so a fit view and an export use the same neighbourhood.
pub fn auto_radius(size: f32, long: f32) -> f32 {
    (0.5 * size).min(AUTO_REF_MAX) * long
}

/// Auto-mask core: the share of the brush size around the centre line whose tones
/// are the reference.
pub const AUTO_CORE: f32 = 0.25;

/// Tone similarity (encoded luma) for auto-masked strokes: `exp(−(Δ/AUTO_TOL)² − …)`.
pub const AUTO_TOL: f32 = 0.08;

/// Colour similarity (OKLab a, b of the encoded pixel) for auto-masked strokes, so a
/// sky stroke doesn't take foliage of the same tone.
pub const AUTO_TOL_AB: f32 = 0.06;

/// OKLab (a, b) of an encoded pixel: the auto mask's colour.
#[inline]
pub fn auto_ab(rgb: [f32; 3]) -> [f32; 2] {
    let lch = encoded_to_oklch(rgb);
    let h = lch[2].to_radians();
    [lch[1] * h.cos(), lch[1] * h.sin()]
}

/// Auto-mask value: painted coverage `cov` times the similarity of luma `l` and colour
/// `ab` to the core-weighted local reference `m` = means of (core, core·luma, core·a,
/// core·b).
#[inline]
pub fn auto_value(cov: f32, l: f32, ab: [f32; 2], m: [f32; 4]) -> f32 {
    if m[0] <= 1e-4 {
        return 0.0;
    }
    let d = (l - m[1] / m[0]) / AUTO_TOL;
    let da = (ab[0] - m[2] / m[0]) / AUTO_TOL_AB;
    let db = (ab[1] - m[3] / m[0]) / AUTO_TOL_AB;
    cov * (-(d * d + da * da + db * db)).exp()
}

/// Build the output-space planes of a mask for `input` (the layer's graded image).
pub fn build_planes(cm: &CompiledMask, geo: &Geometry, input: &[[f32; 3]]) -> Vec<Plane> {
    let (w, h) = (geo.out.width as usize, geo.out.height as usize);
    cm.planes
        .iter()
        .map(|src| match src {
            PlaneSrc::ColorRange { range, gp } => {
                let weight: Vec<f32> = input
                    .par_iter()
                    .map(|p| {
                        let lch = encoded_to_oklch(*p);
                        range.weight(lch[2], lch[1])
                    })
                    .collect();
                let guide: Vec<f32> = input.par_iter().map(|p| cr_guide(range, *p)).collect();
                let (a, b) = crate::local::guided_cross(&guide, &weight, w, h, *gp);
                // Applied to a slightly smoothed guide: its per-pixel noise would come
                // back as speckle in the selection.
                let smooth = cr_smooth_guide(&guide, w, h);
                Plane::from_fn(w, h, |x, y| {
                    (crate::local::upsample_off(&a, gp.s, gp.off, x, y) * smooth[y * w + x]
                        + crate::local::upsample_off(&b, gp.s, gp.off, x, y))
                    .clamp(0.0, 1.0)
                })
            }
            PlaneSrc::Auto {
                raster,
                core,
                gp,
                sigma,
            } => {
                let (r, c) = (&cm.rasters[*raster], &cm.rasters[*core]);
                let sample = |x: usize, y: usize| {
                    let (u, v) = geo.norm_at(x as f64 + 0.5, y as f64 + 0.5);
                    (r.sample(u as f32, v as f32), c.sample(u as f32, v as f32))
                };
                let ab: Vec<[f32; 2]> = input.par_iter().map(|p| auto_ab(*p)).collect();
                let m = crate::local::block_means_off(w, h, gp.s, gp.off, |i| {
                    let k = sample(i % w, i / w).1;
                    [k, k * luma(input[i]), k * ab[i][0], k * ab[i][1]]
                })
                .map(|p| crate::local::box_blur(&p, gp.r));
                let p = Plane::from_fn(w, h, |x, y| {
                    let r =
                        [0, 1, 2, 3].map(|c| crate::local::upsample_off(&m[c], gp.s, gp.off, x, y));
                    let i = y * w + x;
                    auto_value(sample(x, y).0, luma(input[i]), ab[i], r)
                });
                feather(&p, *sigma)
            }
        })
        .collect()
}

/// Coverage of one shape at E-normalized `uv` (isotropic point `p`) for pixel color `rgb`.
#[inline]
pub fn shape_value(
    kind: &ShapeKind,
    uv: [f32; 2],
    p: [f32; 2],
    rgb: [f32; 3],
    rasters: &[Raster],
    planes: &[Plane],
    idx: usize,
) -> f32 {
    match kind {
        ShapeKind::Fill => 1.0,
        ShapeKind::Linear { s, e } => {
            let d = [e[0] - s[0], e[1] - s[1]];
            let len2 = d[0] * d[0] + d[1] * d[1];
            if len2 < 1e-12 {
                return 1.0;
            }
            let t = ((p[0] - s[0]) * d[0] + (p[1] - s[1]) * d[1]) / len2;
            1.0 - smoothstep(0.0, 1.0, t)
        }
        ShapeKind::Radial { c, r, cs, feather } => {
            if r[0] <= 0.0 || r[1] <= 0.0 {
                return 0.0;
            }
            let (dx, dy) = (p[0] - c[0], p[1] - c[1]);
            let qx = cs[0] * dx + cs[1] * dy;
            let qy = -cs[1] * dx + cs[0] * dy;
            let d = (qx / r[0]).hypot(qy / r[1]);
            1.0 - smoothstep(1.0 - feather, 1.0, d)
        }
        ShapeKind::ColorRange(range) => {
            let lch = encoded_to_oklch(rgb);
            range.weight(lch[2], lch[1])
        }
        ShapeKind::Raster(i) => rasters[*i].sample(uv[0], uv[1]),
        ShapeKind::Plane { plane, raster } => {
            let v = planes[*plane].data[idx];
            raster.map_or(v, |r| v.max(rasters[r].sample(uv[0], uv[1])))
        }
    }
}

#[inline]
pub fn combine(m: f32, a: f32, op: MaskOp) -> f32 {
    match op {
        MaskOp::Add => m.max(a),
        MaskOp::Subtract => m * (1.0 - a),
        MaskOp::Intersect => m * a,
    }
}

/// Luminance-range refinement weight for an encoded pixel.
#[inline]
pub fn luma_weight(lr: &LumaRange, rgb: [f32; 3]) -> f32 {
    let y = luma(rgb);
    smoothstep(lr.low - lr.low_falloff.max(0.0), lr.low, y)
        * (1.0 - smoothstep(lr.high, lr.high + lr.high_falloff.max(0.0), y))
}

/// Evaluate shapes (combined + feathered, before invert/luma) over the output grid.
/// `input` is the layer's input image (encoded Rec.2020) used by color-range shapes.
pub fn evaluate(cm: &CompiledMask, geo: &Geometry, input: &[[f32; 3]]) -> Plane {
    let (w, h) = (geo.out.width as usize, geo.out.height as usize);
    let iso = geo.iso();
    let planes = build_planes(cm, geo, input);
    let blurred: Vec<Raster>;
    let rasters = if cm.rasters.iter().any(|r| r.feather > 0.0) {
        blurred = cm
            .rasters
            .iter()
            .map(|r| {
                if r.feather > 0.0 {
                    Raster {
                        plane: Arc::new(feather(&r.plane, r.feather)),
                        feather: 0.0,
                        ..r.clone()
                    }
                } else {
                    r.clone()
                }
            })
            .collect();
        &blurred
    } else {
        &cm.rasters
    };
    let plane = Plane::from_fn(w, h, |x, y| {
        let (u, v) = geo.norm_at(x as f64 + 0.5, y as f64 + 0.5);
        let (u, v) = (u as f32, v as f32);
        let p = [u * iso[0], v * iso[1]];
        let idx = y * w + x;
        let mut acc = 0.0;
        for s in &cm.shapes {
            acc = combine(
                acc,
                shape_value(&s.kind, [u, v], p, input[idx], rasters, &planes, idx),
                s.op,
            );
        }
        acc
    });
    feather(&plane, cm.feather_px)
}

/// Block size of [`feather`]'s grid for `sigma` (1 = full resolution).
pub fn feather_block(sigma: f32) -> usize {
    ((sigma / 4.0).floor() as usize).max(1)
}

/// Rows (or columns) beyond which [`feather`] no longer sees a pixel: the kernel's radius
/// on its grid plus the block-mean and bilinear-upsample neighbourhood.
pub fn feather_reach(sigma: f32) -> usize {
    let f = feather_block(sigma);
    (gaussian_kernel(sigma / f as f32).len() + 2) * f
}

/// Gaussian feather; large sigmas run on a block-downsampled grid.
pub fn feather(p: &Plane, sigma: f32) -> Plane {
    if sigma < 0.5 {
        return p.clone();
    }
    let f = feather_block(sigma);
    let k = gaussian_kernel(sigma / f as f32);
    if f == 1 {
        return gaussian_blur(p, &k);
    }
    let [low] = block_means(p.w, p.h, f, |i| [p.data[i]]);
    let low = gaussian_blur(&low, &k);
    Plane::from_fn(p.w, p.h, |x, y| upsample_at(&low, f, x, y))
}

/// Final per-pixel mask value from the feathered shape coverage.
#[inline]
pub fn finalize(cov: f32, cm: &CompiledMask, rgb: [f32; 3]) -> f32 {
    let mut m = if cm.invert { 1.0 - cov } else { cov };
    if let Some(lr) = &cm.luma {
        m *= luma_weight(lr, rgb);
    }
    if let Some(cr) = &cm.color {
        let lch = encoded_to_oklch(rgb);
        m *= cr.weight(lch[2], lch[1]);
    }
    m.clamp(0.0, 1.0)
}

/// Distance from `p` to segment `a`–`b`.
#[inline]
fn seg_dist(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let d = [b[0] - a[0], b[1] - a[1]];
    let len2 = d[0] * d[0] + d[1] * d[1];
    let t = if len2 > 0.0 {
        (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p[0] - a[0] - t * d[0]).hypot(p[1] - a[1] - t * d[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::plan;
    use lt_core::adjust::ColorRange;
    use lt_core::{CfaPattern, Orientation, RawImage};

    fn geo(w: u32, h: u32) -> Geometry {
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
        plan(
            &raw,
            &lt_core::Adjustments::default(),
            &crate::geometry::CaptureInfo::default(),
            None,
            Size::new(w, h),
            crate::geometry::Fit::ConstrainToSensorPixels,
        )
        .unwrap()
    }

    fn eval_mask(
        mask: &Mask,
        g: &Geometry,
        input: &[[f32; 3]],
        src: Option<&Arc<dyn MaskSource>>,
    ) -> Vec<f32> {
        let cm = compile(
            mask,
            1,
            g,
            src,
            7,
            &std::sync::Mutex::new(RasterCache::default()),
        );
        let cov = evaluate(&cm, g, input);
        cov.data
            .iter()
            .zip(input)
            .map(|(c, rgb)| finalize(*c, &cm, *rgb))
            .collect()
    }

    #[derive(Debug)]
    struct Half;
    impl MaskSource for Half {
        fn ai_mask(&self, target: AiTarget) -> Option<Arc<MaskBitmap>> {
            (target == AiTarget::Subject).then(|| {
                Arc::new(MaskBitmap {
                    size: Size::new(2, 1),
                    data: vec![0, 255],
                    version: 0,
                })
            })
        }
    }

    /// AI rasters are cached by content, never by address: a changed mask handed in as a
    /// new Arc (lt-ffi makes one per render, often at a reused address) is converted
    /// again; the same content in a new Arc, or the same version, is a hit.
    #[test]
    fn ai_rasters_are_keyed_by_content_not_address() {
        let id = RasterId {
            image: 1,
            layer: 2,
            shape: 0,
        };
        let bmp = |data: Vec<u8>, version| {
            Arc::new(MaskBitmap {
                size: Size::new(2, 2),
                data,
                version,
            })
        };
        let mut c = RasterCache::default();
        let a = c.ai_raster(id, Some(&bmp(vec![0, 0, 255, 255], 0)));
        // Same content, new Arc: hit (same generation, same plane).
        let b = c.ai_raster(id, Some(&bmp(vec![0, 0, 255, 255], 0)));
        assert_eq!(
            (a.generation, Arc::ptr_eq(&a.plane, &b.plane)),
            (b.generation, true)
        );
        // New content (whatever address the new Arc gets): converted again.
        for _ in 0..8 {
            let changed = c.ai_raster(id, Some(&bmp(vec![255, 255, 0, 0], 0)));
            assert!(changed.plane.data[0] > 0.99, "stale mask served");
            let back = c.ai_raster(id, Some(&bmp(vec![0, 0, 255, 255], 0)));
            assert_eq!(back.plane.data[0], 0.0, "stale mask served");
        }
        // With versions the app supplies: same version = hit, new version = rebuild.
        let v1 = c.ai_raster(id, Some(&bmp(vec![9, 9, 9, 9], 5)));
        let again = c.ai_raster(id, Some(&bmp(vec![9, 9, 9, 9], 5)));
        assert_eq!(v1.generation, again.generation);
        let v2 = c.ai_raster(id, Some(&bmp(vec![9, 9, 9, 9], 6)));
        assert_eq!(v2.generation, v1.generation + 1);
        // No bitmap after one: empty (and cached as such).
        let none = c.ai_raster(id, None);
        assert_eq!((none.plane.w, none.plane.data[0]), (1, 0.0));
        assert_eq!(c.ai_raster(id, None).generation, none.generation);
        // Hashing distinguishes sizes with the same bytes.
        let wide = MaskBitmap::new(Size::new(4, 1), vec![1, 2, 3, 4]);
        let tall = MaskBitmap::new(Size::new(1, 4), vec![1, 2, 3, 4]);
        assert_ne!(wide.content_key(), tall.content_key());
        let odd = MaskBitmap::new(Size::new(3, 3), vec![7; 9]);
        assert_ne!(
            odd.content_key(),
            MaskBitmap::new(Size::new(3, 3), vec![8; 9]).content_key()
        );
    }

    /// The GPU re-feathers only a band of rows: rows `a..b` widened by
    /// [`feather_reach`] and aligned to [`feather_block`], blurred with that much context,
    /// equal the same rows of a full blur (the band logic of `gpu::feather_raster`).
    #[test]
    fn a_feathered_band_equals_the_full_blur() {
        let (w, h) = (97usize, 311usize);
        let p = Plane::from_fn(w, h, |x, y| {
            let v = ((x * 7919 + y * 104_729) % 1000) as f32 / 1000.0;
            if (x / 13 + y / 17) % 3 == 0 {
                v
            } else {
                0.2 * v
            }
        });
        for sigma in [0.6f32, 3.0, 16.0, 61.0] {
            let full = feather(&p, sigma);
            let (f, reach) = (feather_block(sigma), feather_reach(sigma));
            for (a, b) in [(0usize, 5usize), (140, 150), (300, 311), (0, 311)] {
                let y0 = a.saturating_sub(reach) / f * f;
                let y1 = ((b + reach).div_ceil(f) * f).min(h);
                let s0 = y0.saturating_sub(reach) / f * f;
                let s1 = ((y1 + reach).div_ceil(f) * f).min(h);
                let band = Plane {
                    w,
                    h: s1 - s0,
                    data: p.data[s0 * w..s1 * w].to_vec(),
                };
                let fb = feather(&band, sigma);
                for y in y0..y1 {
                    for x in 0..w {
                        let (u, v) = (fb.data[(y - s0) * w + x], full.data[y * w + x]);
                        assert!(
                            (u - v).abs() < 1e-5,
                            "σ {sigma} rows {a}..{b}: ({x}, {y}) {u} vs {v}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn fill_invert_and_empty() {
        let g = geo(20, 10);
        let input = vec![[0.5; 3]; 200];
        let fill = Mask {
            shapes: vec![MaskShape::Fill { op: MaskOp::Add }],
            ..Mask::default()
        };
        assert!(eval_mask(&fill, &g, &input, None).iter().all(|v| *v == 1.0));
        let empty = Mask::default();
        assert!(
            eval_mask(&empty, &g, &input, None)
                .iter()
                .all(|v| *v == 0.0)
        );
        let inv = Mask {
            invert: true,
            ..Mask::default()
        };
        assert!(eval_mask(&inv, &g, &input, None).iter().all(|v| *v == 1.0));
    }

    #[test]
    fn linear_gradient_values() {
        let g = geo(100, 100);
        let input = vec![[0.5; 3]; 10000];
        let m = Mask {
            shapes: vec![MaskShape::Linear {
                op: MaskOp::Add,
                start: [0.2, 0.5],
                end: [0.8, 0.5],
            }],
            ..Mask::default()
        };
        let v = eval_mask(&m, &g, &input, None);
        assert_eq!(v[50 * 100 + 10], 1.0);
        assert!(
            (v[50 * 100 + 49] - 0.5).abs() < 0.03,
            "{}",
            v[50 * 100 + 49]
        );
        assert_eq!(v[50 * 100 + 90], 0.0);
        let degenerate = Mask {
            shapes: vec![MaskShape::Linear {
                op: MaskOp::Add,
                start: [0.5, 0.5],
                end: [0.5, 0.5],
            }],
            ..Mask::default()
        };
        assert!(
            eval_mask(&degenerate, &g, &input, None)
                .iter()
                .all(|v| *v == 1.0)
        );
    }

    #[test]
    fn radial_is_round_on_non_square_images() {
        let g = geo(200, 100);
        let input = vec![[0.5; 3]; 20000];
        let m = Mask {
            shapes: vec![MaskShape::Radial {
                op: MaskOp::Add,
                center: [0.5, 0.5],
                radius: [0.1, 0.1],
                angle_deg: 30.0,
                feather: 0.2,
            }],
            ..Mask::default()
        };
        let v = eval_mask(&m, &g, &input, None);
        let at = |x: usize, y: usize| v[y * 200 + x];
        assert_eq!(at(100, 50), 1.0);
        // Radius 0.1 of the 200 px long edge = 20 px, in both directions.
        assert_eq!(at(125, 50), 0.0);
        assert_eq!(at(100, 75), 0.0);
        assert!(at(118, 50) > 0.0 && at(118, 50) < 1.0);
        assert!((at(118, 50) - at(100, 68)).abs() < 0.05);
        let zero = Mask {
            shapes: vec![MaskShape::Radial {
                op: MaskOp::Add,
                center: [0.5, 0.5],
                radius: [0.0, 0.1],
                angle_deg: 0.0,
                feather: 0.2,
            }],
            ..Mask::default()
        };
        assert!(eval_mask(&zero, &g, &input, None).iter().all(|v| *v == 0.0));
    }

    #[test]
    fn ops_subtract_and_intersect() {
        let g = geo(100, 10);
        let input = vec![[0.5; 3]; 1000];
        // A hard-edged "left of x = 0.4" shape.
        let left = |op| MaskShape::Linear {
            op,
            start: [0.4, 0.5],
            end: [0.41, 0.5],
        };
        let m = Mask {
            shapes: vec![MaskShape::Fill { op: MaskOp::Add }, left(MaskOp::Subtract)],
            ..Mask::default()
        };
        let v = eval_mask(&m, &g, &input, None);
        assert_eq!(v[5 * 100 + 10], 0.0);
        assert_eq!(v[5 * 100 + 80], 1.0);
        let m = Mask {
            shapes: vec![MaskShape::Fill { op: MaskOp::Add }, left(MaskOp::Intersect)],
            ..Mask::default()
        };
        let v = eval_mask(&m, &g, &input, None);
        assert_eq!(v[5 * 100 + 10], 1.0);
        assert_eq!(v[5 * 100 + 80], 0.0);
    }

    #[test]
    fn color_range_and_luma_refinement() {
        // Two 8 px halves: the smoothed guide keeps the colour edge.
        let g = geo(16, 1);
        let input: Vec<[f32; 3]> = (0..16)
            .map(|x| {
                if x < 8 {
                    [0.8, 0.2, 0.1]
                } else {
                    [0.1, 0.3, 0.8]
                }
            })
            .collect();
        let red = ColorRange {
            center_hue: 5.0,
            width: 60.0,
            ..ColorRange::default()
        };
        let m = Mask {
            shapes: vec![MaskShape::ColorRange {
                op: MaskOp::Add,
                range: red,
                feather: 0.0,
            }],
            ..Mask::default()
        };
        let v = eval_mask(&m, &g, &input, None);
        // (Guided refinement: a hair of spill across the colour edge.)
        assert!(v[..7].iter().all(|v| *v > 0.9), "{v:?}");
        assert!(v[9..].iter().all(|v| *v < 0.03), "{v:?}");
        let lr = LumaRange {
            low: 0.5,
            high: 1.0,
            low_falloff: 0.0,
            high_falloff: 0.0,
        };
        let bright = vec![[0.9; 3], [0.1; 3]];
        let m = Mask {
            shapes: vec![MaskShape::Fill { op: MaskOp::Add }],
            luma_range: Some(lr),
            ..Mask::default()
        };
        let v = eval_mask(&m, &geo(2, 1), &bright, None);
        assert_eq!(v, vec![1.0, 0.0]);
        let soft = LumaRange {
            low: 0.5,
            high: 0.6,
            low_falloff: 0.2,
            high_falloff: 0.2,
        };
        assert!((luma_weight(&soft, [0.4; 3]) - 0.5).abs() < 1e-5);
        assert_eq!(luma_weight(&soft, [0.55; 3]), 1.0);
    }

    /// Review L3: a surface whose hue sits at the range's edge, with per-pixel chroma
    /// noise, gets a smooth selection, not the raw weight's speckle.
    #[test]
    fn a_noisy_surface_at_the_range_edge_is_not_speckled() {
        let (w, h) = (48usize, 48usize);
        let g = geo(w as u32, h as u32);
        let mut seed = 12345u32;
        let input: Vec<[f32; 3]> = (0..w * h)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let n = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                [0.6, 0.35 + 0.12 * n, 0.25 - 0.12 * n]
            })
            .collect();
        let range = ColorRange {
            center_hue: 25.0,
            width: 40.0,
            ..ColorRange::default()
        };
        let m = Mask {
            shapes: vec![MaskShape::ColorRange {
                op: MaskOp::Add,
                range,
                feather: 0.0,
            }],
            ..Mask::default()
        };
        let v = eval_mask(&m, &g, &input, None);
        let ok = OkRange::new(&range);
        let raw: Vec<f32> = input
            .iter()
            .map(|p| {
                let lch = encoded_to_oklch(*p);
                ok.weight(lch[2], lch[1])
            })
            .collect();
        let jitter = |p: &[f32]| {
            let d: f32 = (0..h)
                .flat_map(|y| (1..w).map(move |x| (y, x)))
                .map(|(y, x)| (p[y * w + x] - p[y * w + x - 1]).abs())
                .sum();
            d / (h * (w - 1)) as f32
        };
        let (jr, jv) = (jitter(&raw), jitter(&v));
        assert!(jr > 0.1, "the raw weight is speckled: {jr}");
        // Without the guide smoothing the refinement kept almost all of it (0.46 vs 0.49).
        assert!(jv < 0.6 * jr, "refined {jv} vs raw {jr}");
    }

    #[test]
    fn brush_strokes_flow_hardness_erase() {
        let g = geo(100, 100);
        let input = vec![[0.5; 3]; 10000];
        let stroke = BrushStroke {
            points: vec![[0.2, 0.5], [0.8, 0.5]],
            size: 0.2,
            hardness: 0.5,
            flow: 0.8,
            erase: false,
            auto_mask: false,
        };
        let m = Mask {
            shapes: vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![stroke.clone()],
                feather: 0.0,
            }],
            ..Mask::default()
        };
        let v = eval_mask(&m, &g, &input, None);
        let at = |x: usize, y: usize| v[y * 100 + x];
        // Uniform flow along the stroke's core (no dab build-up).
        assert!((at(30, 50) - 0.8).abs() < 1e-6 && (at(60, 50) - 0.8).abs() < 1e-6);
        assert_eq!(at(50, 80), 0.0);
        assert!(at(50, 57) > 0.0 && at(50, 57) < 0.8);
        // A second stroke builds up; an eraser removes.
        let two = Mask {
            shapes: vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![stroke.clone(), stroke.clone()],
                feather: 0.0,
            }],
            ..Mask::default()
        };
        assert!((eval_mask(&two, &g, &input, None)[50 * 100 + 50] - 0.96).abs() < 1e-5);
        let eraser = BrushStroke {
            erase: true,
            flow: 1.0,
            hardness: 1.0,
            ..stroke.clone()
        };
        let erased = Mask {
            shapes: vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![stroke.clone(), eraser],
                feather: 0.0,
            }],
            ..Mask::default()
        };
        assert_eq!(eval_mask(&erased, &g, &input, None)[50 * 100 + 50], 0.0);
        // Single-point dab, degenerate and off-canvas strokes.
        let dot = BrushStroke {
            points: vec![[0.5, 0.5]],
            ..stroke.clone()
        };
        let bad = BrushStroke {
            size: 0.0,
            ..stroke.clone()
        };
        let off = BrushStroke {
            points: vec![[5.0, 5.0]],
            ..stroke
        };
        let m = Mask {
            shapes: vec![MaskShape::Brush {
                op: MaskOp::Add,
                strokes: vec![dot, bad, off],
                feather: 0.0,
            }],
            ..Mask::default()
        };
        let v = eval_mask(&m, &g, &input, None);
        assert!((v[50 * 100 + 50] - 0.8).abs() < 1e-6);
        assert_eq!(v[0], 0.0);
    }

    #[test]
    fn ai_masks_from_source_or_empty() {
        let g = geo(10, 4);
        let input = vec![[0.5; 3]; 40];
        let src: Arc<dyn MaskSource> = Arc::new(Half);
        let m = Mask {
            shapes: vec![MaskShape::Ai {
                op: MaskOp::Add,
                target: AiTarget::Subject,
                feather: 0.0,
                person: None,
            }],
            ..Mask::default()
        };
        let v = eval_mask(&m, &g, &input, Some(&src));
        assert_eq!(v[0], 0.0);
        assert_eq!(v[9], 1.0);
        // Missing source, or target not provided → empty.
        assert!(eval_mask(&m, &g, &input, None).iter().all(|v| *v == 0.0));
        let sky = Mask {
            shapes: vec![MaskShape::Ai {
                op: MaskOp::Add,
                target: AiTarget::Sky,
                feather: 0.0,
                person: None,
            }],
            ..Mask::default()
        };
        assert!(
            eval_mask(&sky, &g, &input, Some(&src))
                .iter()
                .all(|v| *v == 0.0)
        );
        let broken = MaskBitmap {
            size: Size::new(3, 3),
            data: vec![1],
            version: 0,
        };
        assert_eq!(broken.sample(0.5, 0.5), 0.0);
        assert!(format!("{broken:?}").contains("MaskBitmap"));
    }

    #[test]
    fn feather_softens_edges() {
        let g = geo(200, 50);
        let input = vec![[0.5; 3]; 10000];
        let hard = MaskShape::Linear {
            op: MaskOp::Add,
            start: [0.5, 0.5],
            end: [0.501, 0.5],
        };
        for feather in [0.01f32, 0.05] {
            let m = Mask {
                shapes: vec![hard.clone()],
                feather,
                ..Mask::default()
            };
            let v = eval_mask(&m, &g, &input, None);
            let mid = v[25 * 200 + 100];
            assert!(mid > 0.3 && mid < 0.7, "{feather} {mid}");
            assert!(v[25 * 200 + 5] > 0.99 && v[25 * 200 + 195] < 0.01);
        }
        let p = Plane::from_fn(4, 4, |x, _| x as f32);
        assert_eq!(feather(&p, 0.1), p);
    }
}
