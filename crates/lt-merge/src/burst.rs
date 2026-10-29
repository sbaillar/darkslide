//! Burst composite: one picture made of the best parts of a burst.
//!
//! 1. **Reference.** The best frame anchors the composite (the caller's cull scores when
//!    every frame has one, else sharpness and open eyes): its geometry, background and
//!    moment are kept wherever nothing better is found.
//! 2. **Alignment.** Every other frame is registered to it with a feature homography
//!    (handheld bursts drift). A frame that cannot be aligned contributes nothing.
//! 3. **Faces.** Each face in the reference is matched to the same face in the other
//!    frames (by overlap once aligned) and scored: eyes open first, then the face's
//!    capture quality and its own sharpness. When another frame's face is clearly better,
//!    and its eyes are no less open, it replaces the reference's: re-aligned on the hair,
//!    neck and background around it, colour-matched on that ring, and blended in two bands
//!    inside a feathered ellipse. A face whose surroundings disagree too much (the head
//!    turned or moved against the background) is left as it is ([`Composite::faces_kept`]).
//! 4. **Sharper regions.** On a tile grid, where the reference is soft and an aligned
//!    frame shows the same content (low frequencies agree) clearly sharper, that frame's
//!    tiles replace it, feathered inside the tiles. Content that moved is never taken, so
//!    nothing ghosts, and faces are left to step 3.
//!
//! Faces and eyes come from the caller (Apple Vision on the Swift side) as plain data, in
//! the frames' own normalized coordinates (top-left origin, y down), so frames and faces
//! must share one orientation. The composite has the reference's size and orientation;
//! outside the patches it is the reference, sample for sample.

use lt_core::NormRect;
use rayon::prelude::*;

use crate::geom::{Mat3, inverse, mul, project};
use crate::hdr::{align_pair, mean_unclipped, upscale};
use crate::image::{Gray, SATURATED, blur, check_analysis, factor_for, luma_image};
use crate::{Frame, MergeError, Progress, Stage, WHITE};

/// Most frames one burst merge takes.
pub const MAX_BURST_FRAMES: usize = 64;
/// Long edge of the analysis images (alignment, scores, masks).
const ANALYSIS_EDGE: u32 = 1200;
/// The patch around a face box: wider for hair and ears, taller for brow and chin.
const GROW_W: f32 = 1.5;
const GROW_H: f32 = 1.6;
/// Ellipse radius (fraction of the patch) inside which a face patch is opaque; it fades
/// to nothing at the patch's edge.
const OPAQUE: f32 = 0.7;
/// The ring (ellipse radii) a face is re-aligned and compared on.
const RING: (f32, f32) = (0.6, 1.15);
/// Least overlap (`IoU`) of two faces, once aligned, to be the same person.
const SAME_FACE: f32 = 0.3;
/// How much higher another frame's face must score to replace the reference's.
const BETTER_FACE: f32 = 0.08;
/// Mean relative ring difference (in `√luma`) above which a face is left alone.
const RING_LIMIT: f32 = 0.08;
/// Score weights of a face: eyes open, capture quality, sharpness.
const FACE_WEIGHTS: [f32; 3] = [0.6, 0.2, 0.2];
/// Tile edge of the sharper-regions grid (analysis px).
const TILE: usize = 32;
/// A tile is taken from a frame at least this much sharper (Laplacian energy ratio)...
const SHARPER: f32 = 1.5;
/// ...whose low frequencies agree with the reference within this (mean relative
/// difference of `√luma` blurred by [`AGREE_SIGMA`]).
const AGREE: f32 = 0.04;
const AGREE_SIGMA: f32 = 3.0;
/// Tiles with less Laplacian energy than this (of `√luma`) have no detail to gain.
const DETAIL_FLOOR: f32 = 2e-5;
/// Most separate sharper regions (the largest are kept).
const MAX_REGIONS: usize = 32;
/// Least valid fraction of a tile in an aligned frame for it to be compared.
const TILE_COVERAGE: f32 = 0.99;

/// A face found in a frame (by Apple Vision on the Swift side).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurstFace {
    /// Normalized to the frame: top-left origin, y down.
    pub bounds: NormRect,
    /// Probability (0..=1) that both eyes are open, if measured.
    pub eyes_open: Option<f32>,
    /// Face capture quality (0..=1: sharp, well lit, facing the camera), if known.
    pub quality: Option<f32>,
}

/// One frame of a burst.
#[derive(Debug, Clone, Copy)]
pub struct BurstInput<'a> {
    pub frame: &'a Frame,
    pub faces: &'a [BurstFace],
    /// The frame's overall quality (lt-ai's cull score, 0..=1), if known.
    pub score: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurstOptions {
    /// The frame to build on; `None`: the best one.
    pub reference: Option<usize>,
    /// Replace faces (closed eyes, soft faces) with better ones from other frames.
    pub faces: bool,
    /// Replace soft regions with sharper ones from other frames.
    pub regions: bool,
}

impl Default for BurstOptions {
    fn default() -> Self {
        Self {
            reference: None,
            faces: true,
            regions: true,
        }
    }
}

/// What a patch of the composite replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchKind {
    /// A better face (usually open eyes).
    Face,
    /// A sharper region.
    Sharper,
}

/// Part of the composite taken from another frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Patch {
    pub kind: PatchKind,
    /// Index of the input it came from.
    pub frame: usize,
    /// Where, normalized to the composite.
    pub bounds: NormRect,
}

/// A burst merge result.
#[derive(Debug, Clone, PartialEq)]
pub struct Composite {
    /// Same size, orientation and colour space as the inputs.
    pub frame: Frame,
    /// Index of the input the composite is built on (its metadata describes the result).
    pub reference: usize,
    /// Every part taken from another frame, faces last (they were blended last).
    pub patches: Vec<Patch>,
    /// Faces another frame had better but that stayed as they were: their surroundings
    /// differed too much to blend without a visible seam.
    pub faces_kept: u32,
    /// Frames that could not be aligned (they contributed nothing).
    pub unaligned: u32,
}

/// A frame registered to the reference.
struct Aligned {
    /// Reference analysis px → this frame's analysis px.
    h: Mat3,
    /// This frame's `√luma` resampled onto the reference grid in reference units (the
    /// reference's own values where the frame has none).
    view: Gray,
    valid: Vec<bool>,
    /// Linear brightness of this frame relative to the reference.
    ratio: f32,
}

/// A face to replace, on the reference's analysis grid.
struct FacePlan {
    frame: usize,
    center: (f32, f32),
    radii: (f32, f32),
    /// Where the face sits in the other frame relative to the reference's (analysis px,
    /// reference coordinates).
    shift: (f32, f32),
}

/// A sharper region to replace: its frame and feathered mask on the analysis grid.
struct RegionPlan {
    frame: usize,
    mask: Gray,
    /// Bounding box in analysis px (x0, y0, x1, y1), exclusive ends.
    rect: (usize, usize, usize, usize),
}

/// Merge the best parts of a burst into one picture (see the module docs).
pub fn merge_burst(
    inputs: &[BurstInput<'_>],
    opts: &BurstOptions,
    progress: &dyn Progress,
) -> Result<Composite, MergeError> {
    if inputs.len() < 2 {
        return Err(MergeError::TooFew);
    }
    if inputs.len() > MAX_BURST_FRAMES {
        return Err(MergeError::Invalid(format!(
            "a burst merge takes at most {MAX_BURST_FRAMES} photos"
        )));
    }
    let (w, h) = (inputs[0].frame.w, inputs[0].frame.h);
    if inputs.iter().any(|i| (i.frame.w, i.frame.h) != (w, h)) {
        return Err(MergeError::Mismatch(
            "burst frames must all be the same size (same camera, crop and orientation)".into(),
        ));
    }
    if opts.reference.is_some_and(|r| r >= inputs.len()) {
        return Err(MergeError::Invalid(
            "the reference frame is not in the burst".into(),
        ));
    }
    let n = inputs.len();
    let factor = factor_for(w, h, ANALYSIS_EDGE);
    check_analysis([inputs[0].frame], factor)?;

    let stage = Stage::new(progress, 0.0, 0.15);
    let mut lumas = Vec::with_capacity(n);
    for (i, inp) in inputs.iter().enumerate() {
        stage.check()?;
        lumas.push(luma_image(inp.frame, factor));
        stage.set((i + 1) as f32 / n as f32);
    }
    let roots: Vec<Gray> = lumas.iter().map(root).collect();
    let r = opts.reference.unwrap_or_else(|| best_frame(inputs, &roots));
    let grid = Grid::new(w, h, factor);

    let ref_faces: Vec<(f32, f32, f32, f32)> = inputs[r]
        .faces
        .iter()
        .map(|f| grid.analysis_box(f.bounds))
        .collect();
    let wants_faces = opts.faces && !ref_faces.is_empty();
    let stage = Stage::new(progress, 0.15, 0.6);
    let mut aligned: Vec<Option<Aligned>> = (0..n).map(|_| None).collect();
    let mut unaligned = 0;
    if wants_faces || opts.regions {
        let base = mean_unclipped(&lumas[r]).max(1e-6);
        for f in (0..n).filter(|&f| f != r) {
            stage.check()?;
            let ratio = (mean_unclipped(&lumas[f]) / base).clamp(0.25, 4.0);
            match align_pair(&lumas[r], &lumas[f], ratio) {
                Some(hm) => aligned[f] = Some(resample(&roots[r], &roots[f], hm, ratio)),
                None => unaligned += 1,
            }
            stage.set((f + 1) as f32 / n as f32);
        }
    }
    drop(lumas);
    stage.set(1.0);

    let stage = Stage::new(progress, 0.6, 0.7);
    let (faces, faces_kept) = if wants_faces {
        plan_faces(inputs, r, &roots[r], &aligned, &grid)
    } else {
        (Vec::new(), 0)
    };
    stage.check()?;
    let regions = if opts.regions {
        let guard: Vec<(f32, f32, f32, f32)> = ref_faces.iter().map(|b| grown(*b)).collect();
        plan_regions(&roots[r], &aligned, &guard)
    } else {
        Vec::new()
    };
    stage.set(1.0);

    let stage = Stage::new(progress, 0.7, 1.0);
    let (out, patches) = compose(inputs, r, &aligned, &regions, &faces, &grid, &stage)?;
    stage.set(1.0);
    Ok(Composite {
        frame: Frame { w, h, data: out },
        reference: r,
        patches,
        faces_kept,
        unaligned,
    })
}

/// The reference with the planned regions, then faces, blended in; and what each took.
fn compose(
    inputs: &[BurstInput<'_>],
    r: usize,
    aligned: &[Option<Aligned>],
    regions: &[RegionPlan],
    faces: &[FacePlan],
    grid: &Grid,
    stage: &Stage<'_>,
) -> Result<(Vec<u16>, Vec<Patch>), MergeError> {
    let (up, down) = upscale(grid.factor as u32);
    let full_h = |f: usize| aligned[f].as_ref().map(|a| mul(&up, &mul(&a.h, &down)));
    let mut out = inputs[r].frame.data.clone();
    let mut patches = Vec::with_capacity(regions.len() + faces.len());
    let steps = (regions.len() + faces.len()).max(1) as f32;
    for (k, reg) in regions.iter().enumerate() {
        stage.check()?;
        let (Some(hf), Some(a)) = (full_h(reg.frame), aligned[reg.frame].as_ref()) else {
            continue;
        };
        paste_region(
            &mut out,
            inputs[reg.frame].frame,
            &hf,
            1.0 / a.ratio,
            reg,
            grid,
        );
        let (x0, y0, x1, y1) = reg.rect;
        patches.push(Patch {
            kind: PatchKind::Sharper,
            frame: reg.frame,
            bounds: grid.norm(x0 as f32, y0 as f32, x1 as f32, y1 as f32),
        });
        stage.set((k + 1) as f32 / steps);
    }
    for (k, face) in faces.iter().enumerate() {
        stage.check()?;
        let Some(hf) = full_h(face.frame) else {
            continue;
        };
        paste_face(&mut out, inputs[face.frame].frame, &hf, face, grid);
        let (cx, cy) = face.center;
        let (rx, ry) = face.radii;
        patches.push(Patch {
            kind: PatchKind::Face,
            frame: face.frame,
            bounds: grid.norm(cx - rx, cy - ry, cx + rx, cy + ry),
        });
        stage.set((regions.len() + k + 1) as f32 / steps);
    }
    Ok((out, patches))
}

/// Full-resolution ↔ analysis coordinates.
struct Grid {
    w: f32,
    h: f32,
    factor: f32,
}

impl Grid {
    fn new(w: u32, h: u32, factor: u32) -> Self {
        Self {
            w: w as f32,
            h: h as f32,
            factor: factor as f32,
        }
    }

    /// Full-resolution pixel → analysis pixel (the inverse of [`upscale`]'s `up`).
    fn to_analysis(&self, x: f32, y: f32) -> (f32, f32) {
        let o = (self.factor - 1.0) / 2.0;
        ((x - o) / self.factor, (y - o) / self.factor)
    }

    fn to_full(&self, x: f32, y: f32) -> (f32, f32) {
        let o = (self.factor - 1.0) / 2.0;
        (x * self.factor + o, y * self.factor + o)
    }

    /// A normalized box as `(x0, y0, x1, y1)` analysis px.
    fn analysis_box(&self, b: NormRect) -> (f32, f32, f32, f32) {
        let b = sane(b);
        let (x0, y0) = self.to_analysis(b.x * self.w, b.y * self.h);
        let (x1, y1) = self.to_analysis((b.x + b.w) * self.w, (b.y + b.h) * self.h);
        (x0, y0, x1, y1)
    }

    /// An analysis-px box, normalized to the frame and clipped to it.
    fn norm(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> NormRect {
        let (a, b) = self.to_full(x0, y0);
        let (c, d) = self.to_full(x1, y1);
        let (a, c) = ((a / self.w).clamp(0.0, 1.0), (c / self.w).clamp(0.0, 1.0));
        let (b, d) = ((b / self.h).clamp(0.0, 1.0), (d / self.h).clamp(0.0, 1.0));
        NormRect {
            x: a,
            y: b,
            w: c - a,
            h: d - b,
        }
    }
}

/// A face box with finite, non-negative extents (a corrupt one becomes empty).
fn sane(b: NormRect) -> NormRect {
    let f = |v: f32| if v.is_finite() { v } else { 0.0 };
    NormRect {
        x: f(b.x),
        y: f(b.y),
        w: f(b.w).max(0.0),
        h: f(b.h).max(0.0),
    }
}

/// The patch around a face box (see [`GROW_W`]).
fn grown((x0, y0, x1, y1): (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    let (cx, cy) = (f32::midpoint(x0, x1), f32::midpoint(y0, y1));
    let (rx, ry) = ((x1 - x0) * GROW_W / 2.0, (y1 - y0) * GROW_H / 2.0);
    (cx - rx, cy - ry, cx + rx, cy + ry)
}

fn root(g: &Gray) -> Gray {
    Gray {
        w: g.w,
        h: g.h,
        px: g.px.iter().map(|v| v.max(0.0).sqrt()).collect(),
    }
}

/// Mean squared Laplacian of `g` over `x0..x1` × `y0..y1` (interior pixels only).
fn detail(g: &Gray, (x0, y0, x1, y1): (usize, usize, usize, usize)) -> f32 {
    let (xa, xb) = (x0.max(1), x1.min(g.w.saturating_sub(1)));
    let (ya, yb) = (y0.max(1), y1.min(g.h.saturating_sub(1)));
    if xa >= xb || ya >= yb {
        return 0.0;
    }
    let sum: f64 = (ya..yb)
        .map(|y| {
            let mut s = 0.0f64;
            for x in xa..xb {
                let l = 4.0 * g.at(x, y)
                    - g.at(x - 1, y)
                    - g.at(x + 1, y)
                    - g.at(x, y - 1)
                    - g.at(x, y + 1);
                s += f64::from(l * l);
            }
            s
        })
        .sum();
    (sum / ((xb - xa) * (ya..yb).len()) as f64) as f32
}

/// An analysis box rounded out to whole pixels inside the image.
fn pixels(g: &Gray, (x0, y0, x1, y1): (f32, f32, f32, f32)) -> (usize, usize, usize, usize) {
    let c = |v: f32, hi: usize| (v.max(0.0) as usize).min(hi);
    (
        c(x0.floor(), g.w),
        c(y0.floor(), g.h),
        c(x1.ceil(), g.w),
        c(y1.ceil(), g.h),
    )
}

/// The frame to build on: the best cull score when every frame has one, else the
/// sharpest (with faces: half sharpness, half open eyes). Ties go to the earliest.
fn best_frame(inputs: &[BurstInput<'_>], roots: &[Gray]) -> usize {
    let argmax = |v: &[f32]| {
        v.iter()
            .enumerate()
            .fold(
                (0, f32::NEG_INFINITY),
                |b, (i, &s)| if s > b.1 { (i, s) } else { b },
            )
            .0
    };
    if inputs.iter().all(|i| i.score.is_some_and(f32::is_finite)) {
        let s: Vec<f32> = inputs.iter().map(|i| i.score.unwrap_or(0.0)).collect();
        return argmax(&s);
    }
    let sharp: Vec<f32> = roots
        .par_iter()
        .map(|g| detail(g, (0, 0, g.w, g.h)))
        .collect();
    let top = sharp.iter().copied().fold(0.0f32, f32::max);
    let any_faces = inputs.iter().any(|i| !i.faces.is_empty());
    let rank: Vec<f32> = inputs
        .iter()
        .zip(&sharp)
        .map(|(inp, &s)| {
            let s = if top > 0.0 { s / top } else { 0.0 };
            if !any_faces {
                return s;
            }
            let eyes = if inp.faces.is_empty() {
                0.0
            } else {
                inp.faces
                    .iter()
                    .map(|f| f.eyes_open.unwrap_or(0.5))
                    .sum::<f32>()
                    / inp.faces.len() as f32
            };
            0.5 * s + 0.5 * eyes
        })
        .collect();
    argmax(&rank)
}

/// `root` (a frame's `√luma`) resampled onto the reference grid through `h`, in reference
/// units (`ratio`: the frame's linear brightness over the reference's).
fn resample(reference: &Gray, root: &Gray, h: Mat3, ratio: f32) -> Aligned {
    let k = 1.0 / ratio.sqrt();
    let (w, hh) = (reference.w, reference.h);
    let (xm, ym) = ((root.w - 1) as f64, (root.h - 1) as f64);
    let mut px = vec![0.0f32; w * hh];
    let mut valid = vec![false; w * hh];
    px.par_chunks_mut(w)
        .zip(valid.par_chunks_mut(w))
        .enumerate()
        .for_each(|(y, (row, vrow))| {
            for x in 0..w {
                match project(&h, x as f64, y as f64) {
                    Some((fx, fy)) if fx >= 0.0 && fy >= 0.0 && fx <= xm && fy <= ym => {
                        row[x] = root.sample(fx as f32, fy as f32) * k;
                        vrow[x] = true;
                    }
                    _ => row[x] = reference.at(x, y),
                }
            }
        });
    Aligned {
        h,
        view: Gray { w, h: hh, px },
        valid,
        ratio,
    }
}

fn iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let iw = (a.2.min(b.2) - a.0.max(b.0)).max(0.0);
    let ih = (a.3.min(b.3) - a.1.max(b.1)).max(0.0);
    let inter = iw * ih;
    let area = |r: (f32, f32, f32, f32)| (r.2 - r.0).max(0.0) * (r.3 - r.1).max(0.0);
    let union = area(a) + area(b) - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// A box in a frame's analysis px mapped to the reference's through `inv` (frame →
/// reference): the bounding box of its mapped corners.
fn to_reference(inv: &Mat3, b: (f32, f32, f32, f32)) -> Option<(f32, f32, f32, f32)> {
    let mut out = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (x, y) in [(b.0, b.1), (b.2, b.1), (b.0, b.3), (b.2, b.3)] {
        let (px, py) = project(inv, f64::from(x), f64::from(y))?;
        let (px, py) = (px as f32, py as f32);
        out = (out.0.min(px), out.1.min(py), out.2.max(px), out.3.max(py));
    }
    Some(out)
}

/// A face candidate: its frame, box on the reference grid, eyes, quality and detail.
struct Candidate {
    frame: usize,
    rect: (f32, f32, f32, f32),
    eyes: Option<f32>,
    quality: Option<f32>,
    detail: f32,
}

/// The faces to replace, and how many better ones had to stay (see [`RING_LIMIT`]).
fn plan_faces(
    inputs: &[BurstInput<'_>],
    r: usize,
    reference: &Gray,
    aligned: &[Option<Aligned>],
    grid: &Grid,
) -> (Vec<FacePlan>, u32) {
    let inverses: Vec<Option<Mat3>> = aligned
        .iter()
        .map(|a| a.as_ref().and_then(|a| inverse(&a.h)))
        .collect();
    let mut plans = Vec::new();
    let mut kept = 0;
    for face in inputs[r].faces {
        let rect = grid.analysis_box(face.bounds);
        if rect.2 - rect.0 < 2.0 || rect.3 - rect.1 < 2.0 {
            continue;
        }
        let mut cands = vec![Candidate {
            frame: r,
            rect,
            eyes: face.eyes_open,
            quality: face.quality,
            detail: detail(reference, pixels(reference, rect)),
        }];
        for (f, a) in aligned.iter().enumerate() {
            let (Some(a), Some(inv)) = (a, inverses[f].as_ref()) else {
                continue;
            };
            let best = inputs[f]
                .faces
                .iter()
                .filter_map(|o| {
                    let m = to_reference(inv, grid.analysis_box(o.bounds))?;
                    let v = iou(rect, m);
                    (v >= SAME_FACE).then_some((v, o, m))
                })
                .max_by(|x, y| x.0.total_cmp(&y.0));
            if let Some((_, o, m)) = best {
                cands.push(Candidate {
                    frame: f,
                    rect: m,
                    eyes: o.eyes_open,
                    quality: o.quality,
                    detail: detail(&a.view, pixels(&a.view, m)),
                });
            }
        }
        let top = cands.iter().map(|c| c.detail).fold(0.0f32, f32::max);
        let score = |c: &Candidate| {
            let d = if top > 0.0 { c.detail / top } else { 0.0 };
            FACE_WEIGHTS[0] * c.eyes.unwrap_or(0.5)
                + FACE_WEIGHTS[1] * c.quality.unwrap_or(0.5)
                + FACE_WEIGHTS[2] * d
        };
        let own = score(&cands[0]);
        // Never trade open eyes for closed ones (unknown counts as no better).
        let eyes_ok = |c: &Candidate| match (c.eyes, face.eyes_open) {
            (Some(e), Some(mine)) => e >= mine,
            (Some(_), None) => true,
            (None, mine) => mine.is_none(),
        };
        let Some(best) = cands[1..]
            .iter()
            .filter(|c| eyes_ok(c) && score(c) >= own + BETTER_FACE)
            .max_by(|a, b| score(a).total_cmp(&score(b)))
        else {
            continue;
        };
        let Some(a) = aligned[best.frame].as_ref() else {
            continue;
        };
        let center = (f32::midpoint(rect.0, rect.2), f32::midpoint(rect.1, rect.3));
        let radii = (
            (rect.2 - rect.0) * GROW_W / 2.0,
            (rect.3 - rect.1) * GROW_H / 2.0,
        );
        let start = (
            f32::midpoint(best.rect.0, best.rect.2) - center.0,
            f32::midpoint(best.rect.1, best.rect.3) - center.1,
        );
        match refine(reference, a, center, radii, start) {
            Some(shift) => plans.push(FacePlan {
                frame: best.frame,
                center,
                radii,
                shift,
            }),
            None => kept += 1,
        }
    }
    (plans, kept)
}

/// The shift (analysis px) that best lines up the other frame's view with the reference
/// on the ring around a face, searched around `start`; `None` when even the best leaves
/// a ring difference over [`RING_LIMIT`].
fn refine(
    reference: &Gray,
    a: &Aligned,
    center: (f32, f32),
    radii: (f32, f32),
    start: (f32, f32),
) -> Option<(f32, f32)> {
    let (w, h) = (reference.w as isize, reference.h as isize);
    let (rx, ry) = (radii.0.max(1.0), radii.1.max(1.0));
    let reach = (RING.1 * rx.max(ry)).ceil() as isize;
    let (cx, cy) = (center.0.round() as isize, center.1.round() as isize);
    let ring: Vec<(isize, isize)> = (cy - reach..=cy + reach)
        .flat_map(|y| (cx - reach..=cx + reach).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let r = (((x as f32 - center.0) / rx).powi(2) + ((y as f32 - center.1) / ry).powi(2))
                .sqrt();
            x >= 0 && y >= 0 && x < w && y < h && (RING.0..=RING.1).contains(&r)
        })
        .collect();
    if ring.is_empty() {
        return None;
    }
    let mean_ref = ring
        .iter()
        .map(|&(x, y)| reference.at(x as usize, y as usize))
        .sum::<f32>()
        / ring.len() as f32;
    let radius = (0.25 * (2.0 * rx / GROW_W)).clamp(2.0, 24.0) as isize;
    let (sx, sy) = (start.0.round() as isize, start.1.round() as isize);
    let shifts: Vec<(isize, isize)> = (sy - radius..=sy + radius)
        .flat_map(|dy| (sx - radius..=sx + radius).map(move |dx| (dx, dy)))
        .collect();
    let cost = |&(dx, dy): &(isize, isize)| {
        let (mut sum, mut n) = (0.0f32, 0usize);
        for &(x, y) in &ring {
            let (qx, qy) = (x + dx, y + dy);
            if qx < 0 || qy < 0 || qx >= w || qy >= h {
                continue;
            }
            let i = qy as usize * a.view.w + qx as usize;
            if !a.valid[i] {
                continue;
            }
            sum += (reference.at(x as usize, y as usize) - a.view.px[i]).abs();
            n += 1;
        }
        // Most of the ring must be compared.
        if n * 4 < ring.len() * 3 {
            f32::INFINITY
        } else {
            sum / n as f32
        }
    };
    let (best, c) = shifts
        .par_iter()
        .map(|s| (*s, cost(s)))
        .min_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)))?;
    (c.is_finite() && c <= RING_LIMIT * mean_ref.max(1e-3))
        .then_some((best.0 as f32, best.1 as f32))
}

/// Sharper regions: connected tiles where one aligned frame is clearly sharper with the
/// same content, kept clear of the `guard` boxes (faces), largest first.
fn plan_regions(
    reference: &Gray,
    aligned: &[Option<Aligned>],
    guard: &[(f32, f32, f32, f32)],
) -> Vec<RegionPlan> {
    let (w, h) = (reference.w, reference.h);
    let (tw, th) = (w.div_ceil(TILE), h.div_ceil(TILE));
    let low_ref = blur(reference, AGREE_SIGMA);
    let lows: Vec<Option<Gray>> = aligned
        .par_iter()
        .map(|a| a.as_ref().map(|a| blur(&a.view, AGREE_SIGMA)))
        .collect();
    // Per tile: the frame to take it from (the sharpest qualifying one).
    let choice: Vec<Option<usize>> = (0..tw * th)
        .into_par_iter()
        .map(|t| {
            let rect = (
                (t % tw) * TILE,
                (t / tw) * TILE,
                ((t % tw + 1) * TILE).min(w),
                ((t / tw + 1) * TILE).min(h),
            );
            let fr = (rect.0 as f32, rect.1 as f32, rect.2 as f32, rect.3 as f32);
            if guard.iter().any(|g| iou(*g, fr) > 0.0) {
                return None;
            }
            let own = detail(reference, rect);
            let mut best: Option<(usize, f32)> = None;
            for (f, a) in aligned.iter().enumerate() {
                let (Some(a), Some(low)) = (a, lows[f].as_ref()) else {
                    continue;
                };
                let d = detail(&a.view, rect);
                if d < DETAIL_FLOOR || d < SHARPER * own || best.is_some_and(|b| d <= b.1) {
                    continue;
                }
                if agreement(&low_ref, low, &a.valid, rect).is_some_and(|e| e <= AGREE) {
                    best = Some((f, d));
                }
            }
            best.map(|b| b.0)
        })
        .collect();

    // Connected components of tiles that take the same frame.
    let mut label = vec![usize::MAX; tw * th];
    let mut comps: Vec<(usize, Vec<usize>)> = Vec::new();
    for start in 0..tw * th {
        let Some(f) = choice[start] else { continue };
        if label[start] != usize::MAX {
            continue;
        }
        let id = comps.len();
        let mut tiles = vec![start];
        label[start] = id;
        let mut k = 0;
        while k < tiles.len() {
            let t = tiles[k];
            k += 1;
            let (x, y) = (t % tw, t / tw);
            let next = [
                (x > 0).then(|| t - 1),
                (x + 1 < tw).then(|| t + 1),
                (y > 0).then(|| t - tw),
                (y + 1 < th).then(|| t + tw),
            ];
            for u in next.into_iter().flatten() {
                if label[u] == usize::MAX && choice[u] == Some(f) {
                    label[u] = id;
                    tiles.push(u);
                }
            }
        }
        comps.push((f, tiles));
    }
    comps.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.1[0].cmp(&b.1[0])));
    comps.truncate(MAX_REGIONS);
    comps
        .into_par_iter()
        .map(|(frame, tiles)| region_mask(w, h, tw, frame, &tiles))
        .collect()
}

/// Mean relative difference of two low-passed images over `rect` where the frame is
/// valid; `None` when too little of the tile is.
fn agreement(
    low_ref: &Gray,
    low: &Gray,
    valid: &[bool],
    (x0, y0, x1, y1): (usize, usize, usize, usize),
) -> Option<f32> {
    let (mut diff, mut base, mut n) = (0.0f32, 0.0f32, 0usize);
    for y in y0..y1 {
        for x in x0..x1 {
            let i = y * low_ref.w + x;
            if valid[i] {
                diff += (low_ref.px[i] - low.px[i]).abs();
                base += low_ref.px[i];
                n += 1;
            }
        }
    }
    let area = (x1 - x0) * (y1 - y0);
    (n > 0 && n as f32 >= TILE_COVERAGE * area as f32).then(|| diff / base.max(1e-6))
}

/// A region's mask: its tiles eroded by a quarter tile, then blurred, so the feather stays
/// inside the tiles that qualified.
fn region_mask(w: usize, h: usize, tw: usize, frame: usize, tiles: &[usize]) -> RegionPlan {
    let set: std::collections::HashSet<usize> = tiles.iter().copied().collect();
    let erode = TILE / 4;
    let inside = |x: usize, y: usize| set.contains(&((y / TILE) * tw + x / TILE));
    let mut px = vec![0.0f32; w * h];
    px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let (ya, yb) = (y.saturating_sub(erode), (y + erode).min(h - 1));
        for (x, o) in row.iter_mut().enumerate() {
            let (xa, xb) = (x.saturating_sub(erode), (x + erode).min(w - 1));
            // Every tile the square around (x, y) touches (at most 2×2: erode < TILE).
            if inside(xa, ya) && inside(xb, ya) && inside(xa, yb) && inside(xb, yb) {
                *o = 1.0;
            }
        }
    });
    let mask = blur(&Gray { w, h, px }, erode as f32 / 2.0);
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for &t in tiles {
        let (x, y) = ((t % tw) * TILE, (t / tw) * TILE);
        (x0, y0) = (x0.min(x), y0.min(y));
        (x1, y1) = (x1.max((x + TILE).min(w)), y1.max((y + TILE).min(h)));
    }
    RegionPlan {
        frame,
        mask,
        rect: (x0, y0, x1, y1),
    }
}

/// Full-resolution pixel bounds `(x0, y0, x1, y1)` of an analysis box, padded by one
/// analysis pixel and clipped to the frame.
fn full_bounds(
    grid: &Grid,
    (x0, y0, x1, y1): (f32, f32, f32, f32),
) -> (usize, usize, usize, usize) {
    let (a, b) = grid.to_full(x0 - 1.0, y0 - 1.0);
    let (c, d) = grid.to_full(x1 + 1.0, y1 + 1.0);
    let cl = |v: f32, hi: f32| v.clamp(0.0, hi) as usize;
    (
        cl(a.floor(), grid.w),
        cl(b.floor(), grid.h),
        cl(c.ceil(), grid.w),
        cl(d.ceil(), grid.h),
    )
}

#[inline]
fn to_u16(v: f32) -> u16 {
    (v * WHITE).round().clamp(0.0, WHITE) as u16
}

/// Blend a sharper region into `out`: `frame` through `hf` (full-resolution reference →
/// frame), brightness-matched by `gain`, under the region's mask.
fn paste_region(
    out: &mut [u16],
    frame: &Frame,
    hf: &Mat3,
    gain: f32,
    reg: &RegionPlan,
    grid: &Grid,
) {
    let w = frame.w as usize;
    let (x0, y0, x1, y1) = reg.rect;
    let (bx0, by0, bx1, by1) = full_bounds(grid, (x0 as f32, y0 as f32, x1 as f32, y1 as f32));
    out.par_chunks_mut(w * 3)
        .enumerate()
        .skip(by0)
        .take(by1 - by0)
        .for_each(|(y, row)| {
            for x in bx0..bx1 {
                let (ax, ay) = grid.to_analysis(x as f32, y as f32);
                let m = reg.mask.sample(ax, ay);
                if m <= 1e-3 {
                    continue;
                }
                let Some((sx, sy)) = project(hf, x as f64, y as f64) else {
                    continue;
                };
                let Some(src) = frame.sample(sx as f32, sy as f32) else {
                    continue;
                };
                let px = &mut row[x * 3..x * 3 + 3];
                for c in 0..3 {
                    let cur = f32::from(px[c]) / WHITE;
                    px[c] = to_u16(cur + (src[c] * gain - cur) * m);
                }
            }
        });
}

/// Weight of a face patch at ellipse radius `r`: 1 inside [`OPAQUE`], easing to 0 at 1.
#[inline]
fn face_weight(r: f32) -> f32 {
    if r <= OPAQUE {
        1.0
    } else if r >= 1.0 {
        0.0
    } else {
        let t = (r - OPAQUE) / (1.0 - OPAQUE);
        1.0 - t * t * (3.0 - 2.0 * t)
    }
}

#[inline]
fn smoothstep(a: f32, b: f32, v: f32) -> f32 {
    let t = ((v - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Running box sum of `n` samples read through `at` (edges clamped), `radius` each side.
#[inline]
fn slide(at: impl Fn(usize) -> f32, n: usize, radius: usize, out: &mut [f32]) {
    let r = radius as isize;
    let get = |i: isize| at(i.clamp(0, n as isize - 1) as usize);
    let norm = 1.0 / (2 * radius + 1) as f32;
    let mut acc: f32 = (-r..=r).map(get).sum();
    for (i, o) in out.iter_mut().enumerate().take(n) {
        *o = acc * norm;
        let i = i as isize;
        acc += get(i + r + 1) - get(i - r);
    }
}

/// Separable box blur of an interleaved RGB buffer (edges clamped), `radius` px; both
/// passes are running sums (the vertical one on a transposed copy).
fn box_blur3(src: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    let stride = w * 3;
    let mut rows = vec![0.0f32; src.len()];
    rows.par_chunks_mut(stride)
        .enumerate()
        .for_each(|(y, row)| {
            let line = &src[y * stride..(y + 1) * stride];
            let mut buf = vec![0.0f32; w];
            for c in 0..3 {
                slide(|x| line[x * 3 + c], w, radius, &mut buf);
                for (x, v) in buf.iter().enumerate() {
                    row[x * 3 + c] = *v;
                }
            }
        });
    // One row per sample column (x, channel).
    let mut cols = vec![0.0f32; src.len()];
    cols.par_chunks_mut(h).enumerate().for_each(|(k, col)| {
        slide(|y| rows[y * stride + k], h, radius, col);
    });
    let mut out = vec![0.0f32; src.len()];
    out.par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
        for (k, o) in row.iter_mut().enumerate() {
            *o = cols[k * h + y];
        }
    });
    out
}

/// Blend a better face into `out`: `frame` through `hf` (full-resolution reference →
/// frame) at the plan's shift, colour-matched on the patch's feathered edge, low
/// frequencies under the soft mask and detail under a steeper one, so the seam takes
/// neither a halo nor a hard line.
fn paste_face(out: &mut [u16], frame: &Frame, hf: &Mat3, face: &FacePlan, grid: &Grid) {
    let fw = frame.w as usize;
    let (cx, cy) = face.center;
    let (rx, ry) = (face.radii.0.max(0.5), face.radii.1.max(0.5));
    let (x0, y0, x1, y1) = full_bounds(grid, (cx - rx, cy - ry, cx + rx, cy + ry));
    let (bw, bh) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if bw < 2 || bh < 2 {
        return;
    }
    let (sx, sy) = (face.shift.0 * grid.factor, face.shift.1 * grid.factor);
    let mut cur = vec![0.0f32; bw * bh * 3];
    let mut cand = vec![0.0f32; bw * bh * 3];
    let mut mask = vec![0.0f32; bw * bh];
    cur.par_chunks_mut(bw * 3)
        .zip(cand.par_chunks_mut(bw * 3))
        .zip(mask.par_chunks_mut(bw))
        .enumerate()
        .for_each(|(j, ((crow, drow), mrow))| {
            let y = y0 + j;
            for i in 0..bw {
                let x = x0 + i;
                let o = (y * fw + x) * 3;
                let now = [0, 1, 2].map(|c| f32::from(out[o + c]) / WHITE);
                crow[i * 3..i * 3 + 3].copy_from_slice(&now);
                let (ax, ay) = grid.to_analysis(x as f32, y as f32);
                let r = (((ax - cx) / rx).powi(2) + ((ay - cy) / ry).powi(2)).sqrt();
                let src = project(hf, f64::from(x as f32 + sx), f64::from(y as f32 + sy))
                    .and_then(|(px, py)| frame.sample(px as f32, py as f32));
                match src {
                    Some(s) => {
                        drow[i * 3..i * 3 + 3].copy_from_slice(&s);
                        mrow[i] = face_weight(r);
                    }
                    None => drow[i * 3..i * 3 + 3].copy_from_slice(&now),
                }
            }
        });
    // Colour match on the feathered edge, where both are seen side by side.
    let (mut a, mut b) = ([0.0f64; 3], [0.0f64; 3]);
    for (k, &m) in mask.iter().enumerate() {
        if m > 0.02 && m < 0.98 {
            let (p, q) = (&cur[k * 3..k * 3 + 3], &cand[k * 3..k * 3 + 3]);
            if p.iter().chain(q).all(|&v| v < SATURATED) {
                for c in 0..3 {
                    a[c] += f64::from(p[c]);
                    b[c] += f64::from(q[c]);
                }
            }
        }
    }
    let gain = [0, 1, 2].map(|c| {
        if b[c] > 0.0 {
            ((a[c] / b[c]) as f32).clamp(0.8, 1.25)
        } else {
            1.0
        }
    });
    for (k, v) in cand.iter_mut().enumerate() {
        *v *= gain[k % 3];
    }
    let radius = ((rx.min(ry) * grid.factor / 6.0) as usize).max(2);
    let low_cur = box_blur3(&box_blur3(&cur, bw, bh, radius), bw, bh, radius);
    let low_cand = box_blur3(&box_blur3(&cand, bw, bh, radius), bw, bh, radius);
    for j in 0..bh {
        for i in 0..bw {
            let k = j * bw + i;
            let m = mask[k];
            if m <= 0.0 {
                continue;
            }
            let hi_m = smoothstep(0.3, 0.7, m);
            let o = ((y0 + j) * fw + x0 + i) * 3;
            for c in 0..3 {
                let q = k * 3 + c;
                let low = low_cur[q] + (low_cand[q] - low_cur[q]) * m;
                let (hc, hd) = (cur[q] - low_cur[q], cand[q] - low_cand[q]);
                out[o + c] = to_u16(low + hc + (hd - hc) * hi_m);
            }
        }
    }
}

#[cfg(test)]
#[path = "burst_tests.rs"]
mod tests;
