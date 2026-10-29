//! HDR merge of bracketed exposures in scene-linear light.
//!
//! 1. **Exposure order and ratios.** EXIF exposure (time × ISO / N²) orders the frames
//!    when every frame has it; the ratios are then measured from the data (median ratio
//!    of well-exposed pixels after alignment), which is more exact than nominal shutter
//!    speeds. The middle exposure is the reference.
//! 2. **Alignment.** Each frame is matched to the reference on luminance scaled to the
//!    reference's exposure and clipped at the pair's common clip (so both look alike),
//!    and a RANSAC homography maps reference pixels into it. Without enough matches the
//!    frame is used unaligned.
//! 3. **Merge.** Per pixel, radiance `v · e_ref / e_i` from every frame, weighted by its
//!    exposure (shot-noise SNR) and a taper to zero before the clip. The darkest frame
//!    always counts where everything else is clipped.
//! 4. **Ghosts.** On the analysis grid, frames adjacent in exposure that are both well
//!    exposed but disagree by more than the deghost threshold mark a ghost; marks are
//!    opened, dilated and split into regions, and each region takes **one** exposure
//!    (the brightest one not clipped there), feathered into the merge.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use rayon::prelude::*;

use crate::features::{detect, match_features};
use crate::geom::{IDENTITY, Mat3, Pair, inverse, mul, project, ransac_homography};
use crate::image::{Gray, SATURATED, blur, check_analysis, dither, factor_for, luma_image};
use crate::{Frame, MergeError, Merged, Progress, Projection, Stage, WHITE};

/// Long edge of the analysis images (alignment, exposure ratios, ghost map).
const ANALYSIS_EDGE: u32 = 1200;
/// A sample above this (max channel) is treated as clipped for analysis.
const CLIPPED: f32 = 0.9;
/// Merge weight tapers from 1 here to 0 at [`TAPER_END`] (max channel).
const TAPER_START: f32 = 0.80;
const TAPER_END: f32 = 0.95;
/// Below this an analysis sample is too noisy to compare.
const NOISE_FLOOR: f32 = 0.004;
/// Fewest RANSAC inliers that make an alignment trustworthy.
const MIN_INLIERS: usize = 12;

/// How aggressively moving objects are taken from a single exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Deghost {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl Deghost {
    /// Log-radiance difference that marks a ghost (`None`: off).
    fn threshold(self) -> Option<f32> {
        match self {
            Self::Off => None,
            Self::Low => Some(0.5),
            Self::Medium => Some(0.3),
            Self::High => Some(0.18),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrOptions {
    pub align: bool,
    pub deghost: Deghost,
}

impl Default for HdrOptions {
    fn default() -> Self {
        Self {
            align: true,
            deghost: Deghost::Off,
        }
    }
}

/// One bracketed frame. `exposure`: its relative exposure (time × ISO / N²), if known.
#[derive(Debug, Clone, Copy)]
pub struct HdrInput<'a> {
    pub frame: &'a Frame,
    pub exposure: Option<f32>,
}

/// One frame prepared for the merge.
struct Plan<'a> {
    frame: &'a Frame,
    /// Multiplier to reference units (`e_ref / e_i`).
    k: f32,
    /// Reference pixel → this frame's pixel (`None`: identity).
    h: Option<Mat3>,
}

/// The ghost map on the analysis grid.
struct Ghosts {
    w: usize,
    h: usize,
    factor: f32,
    /// Feathered weight of the single exposure, `0..=1`.
    mask: Vec<f32>,
    /// Index into the sorted plans of the exposure used (255: none).
    frame: Vec<u8>,
    regions: u32,
}

/// Merge bracketed exposures (see the module docs).
pub fn merge_hdr(
    inputs: &[HdrInput<'_>],
    opts: &HdrOptions,
    progress: &dyn Progress,
) -> Result<Merged, MergeError> {
    if inputs.len() < 2 {
        return Err(MergeError::TooFew);
    }
    if inputs.len() > 255 {
        return Err(MergeError::Invalid("too many frames".into()));
    }
    let (w, h) = (inputs[0].frame.w, inputs[0].frame.h);
    if inputs.iter().any(|i| (i.frame.w, i.frame.h) != (w, h)) {
        return Err(MergeError::Mismatch(
            "HDR frames must all be the same size (same camera and crop)".into(),
        ));
    }
    let stage = Stage::new(progress, 0.0, 0.25);
    let factor = factor_for(w, h, ANALYSIS_EDGE);
    check_analysis([inputs[0].frame], factor)?;
    let lumas: Vec<Gray> = inputs.iter().map(|i| luma_image(i.frame, factor)).collect();
    stage.check()?;

    // Initial exposures: EXIF when every frame has it, else mean brightness.
    let exif = inputs
        .iter()
        .all(|i| i.exposure.is_some_and(|e| e.is_finite() && e > 0.0));
    let expo: Vec<f32> = if exif {
        inputs.iter().map(|i| i.exposure.unwrap_or(1.0)).collect()
    } else {
        lumas.iter().map(|g| mean_unclipped(g).max(1e-6)).collect()
    };
    let reg = register(&lumas, expo, opts.align, &stage)?;
    let (order, r, align, expo) = (reg.order, reg.reference, reg.align, reg.expo);
    stage.set(1.0);

    let scale = upscale(factor);
    let plans: Vec<Plan<'_>> = order
        .iter()
        .map(|&i| Plan {
            frame: inputs[i].frame,
            k: expo[r] / expo[i],
            h: align[i].map(|a| mul(&scale.0, &mul(&a, &scale.1))),
        })
        .collect();

    let ghosts = match opts.deghost.threshold() {
        Some(t) => {
            let order_lumas: Vec<&Gray> = order.iter().map(|&i| &lumas[i]).collect();
            let aligns: Vec<Option<&Mat3>> = order.iter().map(|&i| align[i].as_ref()).collect();
            let ks: Vec<f32> = plans.iter().map(|p| p.k).collect();
            Some(ghost_map(&order_lumas, &aligns, &ks, t, factor as f32))
        }
        None => None,
    };
    Stage::new(progress, 0.25, 0.3).check()?;

    let k_max = plans[0].k.max(1.0);
    let out_scale = WHITE / k_max;
    let frame = merge_rows(
        &plans,
        ghosts.as_ref(),
        out_scale,
        &Stage::new(progress, 0.3, 1.0),
    )?;
    Ok(Merged {
        frame,
        exposure_ev: k_max.log2(),
        reference: r,
        ghost_regions: ghosts.map_or(0, |g| g.regions),
        projection: Projection::Perspective,
        unaligned: reg.unaligned,
    })
}

/// Exposure order, reference, alignment (reference → frame, analysis grid) and
/// exposures of a bracket.
struct Registration {
    /// Frame indices, darkest first.
    order: Vec<usize>,
    reference: usize,
    align: Vec<Option<Mat3>>,
    expo: Vec<f32>,
    /// Frames alignment was asked for but failed on.
    unaligned: u32,
}

/// Align and measure frames **adjacent in exposure** (they always share well-exposed
/// pixels, unlike the ends of a wide bracket and its middle), then chain the links
/// toward the middle exposure. When the measured ratios change which frame is in the
/// middle, the reference moves there and the chain is built again.
fn register(
    lumas: &[Gray],
    mut expo: Vec<f32>,
    align_on: bool,
    stage: &Stage<'_>,
) -> Result<Registration, MergeError> {
    let n = lumas.len();
    let sorted = |expo: &[f32]| {
        let mut o: Vec<usize> = (0..n).collect();
        o.sort_by(|&a, &b| expo[a].total_cmp(&expo[b]));
        o
    };
    let mut order = sorted(&expo);
    let mut tried: Vec<usize> = Vec::new();
    loop {
        let mid = n / 2;
        let r = order[mid];
        tried.push(r);
        // Links between neighbours: `a → b` pixels and the ratio `e_b / e_a`.
        let mut links: Vec<(Option<Mat3>, Option<f32>)> = Vec::with_capacity(n - 1);
        for k in 0..n - 1 {
            stage.set(0.1 + 0.8 * k as f32 / (n - 1) as f32);
            stage.check()?;
            let (a, b) = (order[k], order[k + 1]);
            let h = if align_on {
                align_pair(&lumas[a], &lumas[b], expo[b] / expo[a])
            } else {
                None
            };
            links.push((h, measured_ratio(&lumas[a], &lumas[b], h.as_ref())));
        }
        let mut align: Vec<Option<Mat3>> = vec![None; n];
        align[r] = Some(IDENTITY);
        let mut next = expo.clone();
        for k in mid + 1..n {
            let (prev, f) = (order[k - 1], order[k]);
            let (h, ratio) = &links[k - 1];
            align[f] = align[prev].zip(*h).map(|(p, l)| mul(&l, &p));
            if let Some(q) = ratio {
                next[f] = next[prev] * q;
            }
        }
        for k in (0..mid).rev() {
            let (prev, f) = (order[k + 1], order[k]);
            let (h, ratio) = &links[k];
            align[f] = align[prev]
                .zip(h.and_then(|l| inverse(&l)))
                .map(|(p, l)| mul(&l, &p));
            if let Some(q) = ratio {
                next[f] = next[prev] / q;
            }
        }
        // A broken chain: try the frame against the reference directly.
        let mut unaligned = 0;
        for f in (0..n).filter(|&f| f != r) {
            if align_on && align[f].is_none() {
                align[f] = align_pair(&lumas[r], &lumas[f], next[f] / next[r]);
                unaligned += u32::from(align[f].is_none());
            }
        }
        align[r] = None;
        expo = next;
        let resorted = sorted(&expo);
        if resorted[mid] == r || tried.contains(&resorted[mid]) {
            return Ok(Registration {
                order: resorted,
                reference: r,
                align,
                expo,
                unaligned,
            });
        }
        order = resorted;
    }
}

/// Analysis pixel ↔ full-resolution pixel: `(up, down)` with `up · a · down` mapping a
/// full-resolution reference pixel through an analysis-grid homography `a`.
pub(crate) fn upscale(factor: u32) -> (Mat3, Mat3) {
    let f = f64::from(factor);
    let o = (f - 1.0) / 2.0;
    let up = [[f, 0.0, o], [0.0, f, o], [0.0, 0.0, 1.0]];
    let down = inverse(&up).unwrap_or(IDENTITY);
    (up, down)
}

pub(crate) fn mean_unclipped(g: &Gray) -> f32 {
    let (sum, n) =
        g.px.iter()
            .filter(|&&v| v < CLIPPED)
            .fold((0.0f64, 0usize), |a, &v| (a.0 + f64::from(v), a.1 + 1));
    // All clipped: brighter than anything measurable.
    if n == 0 { 1.0 } else { (sum / n as f64) as f32 }
}

/// Homography from reference to `frame` analysis pixels, `ratio` = `e_frame / e_ref`.
pub(crate) fn align_pair(reference: &Gray, frame: &Gray, ratio: f32) -> Option<Mat3> {
    // Both images in reference units, clipped at the lower of the two clips.
    let clip = CLIPPED * (1.0f32).min(1.0 / ratio);
    let prep = |g: &Gray, k: f32| Gray {
        w: g.w,
        h: g.h,
        px: g
            .px
            .iter()
            .map(|v| (v * k).min(clip).max(0.0).sqrt())
            .collect(),
    };
    let a = detect(&prep(reference, 1.0), 1500);
    let b = detect(&prep(frame, 1.0 / ratio), 1500);
    let m = match_features(&a, &b, 0.8);
    let pairs: Vec<Pair> = m
        .iter()
        .map(|&(i, j)| {
            (
                (f64::from(a[i].x), f64::from(a[i].y)),
                (f64::from(b[j].x), f64::from(b[j].y)),
            )
        })
        .collect();
    let (h, inl) = ransac_homography(&pairs, 1.0, 1000)?;
    (inl.len() >= MIN_INLIERS).then_some(h)
}

/// Median `frame / reference` brightness over pixels well exposed in both (`None` with
/// too few of them).
fn measured_ratio(reference: &Gray, frame: &Gray, h: Option<&Mat3>) -> Option<f32> {
    let mut logs: Vec<f32> = Vec::new();
    for y in (0..reference.h).step_by(2) {
        for x in (0..reference.w).step_by(2) {
            let a = reference.at(x, y);
            let (fx, fy) = match h {
                Some(h) => match project(h, x as f64, y as f64) {
                    Some((u, v)) => (u as f32, v as f32),
                    None => continue,
                },
                None => (x as f32, y as f32),
            };
            if fx < 0.0 || fy < 0.0 || fx > (frame.w - 1) as f32 || fy > (frame.h - 1) as f32 {
                continue;
            }
            let b = frame.sample(fx, fy);
            if (0.02..0.8).contains(&a) && (0.02..0.8).contains(&b) {
                logs.push((b / a).ln());
            }
        }
    }
    if logs.len() < 64 {
        return None;
    }
    let mid = logs.len() / 2;
    let (_, m, _) = logs.select_nth_unstable_by(mid, f32::total_cmp);
    Some(m.exp())
}

/// The ghost map: `lumas`, `aligns` and `ks` are in exposure order (darkest first).
fn ghost_map(
    lumas: &[&Gray],
    aligns: &[Option<&Mat3>],
    ks: &[f32],
    thresh: f32,
    factor: f32,
) -> Ghosts {
    let (w, h) = (lumas[0].w, lumas[0].h);
    let n = lumas.len();
    // Aligned analysis luminance in frame units (`None`: outside the frame).
    let at = |i: usize, x: usize, y: usize| -> Option<f32> {
        let g = lumas[i];
        let (fx, fy) = match aligns[i] {
            Some(a) => {
                let (u, v) = project(a, x as f64, y as f64)?;
                (u as f32, v as f32)
            }
            None => (x as f32, y as f32),
        };
        (fx >= 0.0 && fy >= 0.0 && fx <= (g.w - 1) as f32 && fy <= (g.h - 1) as f32)
            .then(|| g.sample(fx, fy))
    };
    let mut marks = vec![false; w * h];
    marks.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, m) in row.iter_mut().enumerate() {
            for i in 0..n - 1 {
                let (Some(a), Some(b)) = (at(i, x, y), at(i + 1, x, y)) else {
                    continue;
                };
                let ok = |v: f32| (NOISE_FLOOR..CLIPPED).contains(&v);
                if ok(a) && ok(b) && ((a * ks[i]).ln() - (b * ks[i + 1]).ln()).abs() > thresh {
                    *m = true;
                    break;
                }
            }
        }
    });
    // Opening (drop isolated marks), then a generous dilation.
    let marks = dilate(&erode(&marks, w, h, 1), w, h, 4);
    let (labels, count) = components(&marks, w, h);
    // Per region: pixels and clipped pixels per frame.
    let mut area = vec![0usize; count];
    let mut clipped = vec![0usize; count * n];
    for y in 0..h {
        for x in 0..w {
            let l = labels[y * w + x];
            if l == usize::MAX {
                continue;
            }
            area[l] += 1;
            for i in 0..n {
                if at(i, x, y).is_none_or(|v| v >= CLIPPED) {
                    clipped[l * n + i] += 1;
                }
            }
        }
    }
    // The brightest frame clipped on under 2% of the region, else the darkest.
    let choice: Vec<u8> = (0..count)
        .map(|l| {
            (0..n)
                .rev()
                .find(|&i| clipped[l * n + i] * 50 < area[l])
                .unwrap_or(0) as u8
        })
        .collect();
    let binary = Gray {
        w,
        h,
        px: marks.iter().map(|&m| if m { 1.0 } else { 0.0 }).collect(),
    };
    let soft = blur(&binary, 1.5);
    let mask: Vec<f32> = soft.px.iter().map(|v| (v * 2.0).min(1.0)).collect();
    // The frame of each feathered pixel: its region's, or the nearest region's nearby.
    let frame: Vec<u8> = (0..w * h)
        .into_par_iter()
        .map(|i| {
            if mask[i] <= 0.0 {
                return u8::MAX;
            }
            let (x, y) = ((i % w) as isize, (i / w) as isize);
            for r in 0..=5isize {
                for dy in -r..=r {
                    for dx in -r..=r {
                        let (xx, yy) = (x + dx, y + dy);
                        if xx < 0 || yy < 0 || xx >= w as isize || yy >= h as isize {
                            continue;
                        }
                        let l = labels[yy as usize * w + xx as usize];
                        if l != usize::MAX {
                            return choice[l];
                        }
                    }
                }
            }
            u8::MAX
        })
        .collect();
    Ghosts {
        w,
        h,
        factor,
        mask,
        frame,
        regions: count as u32,
    }
}

fn erode(m: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    morph(m, w, h, r, true)
}

fn dilate(m: &[bool], w: usize, h: usize, r: usize) -> Vec<bool> {
    morph(m, w, h, r, false)
}

/// Square-window erosion (`all`) or dilation (`any`); the outside counts as unmarked.
fn morph(m: &[bool], w: usize, h: usize, r: usize, erode: bool) -> Vec<bool> {
    let mut out = vec![false; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut hit = erode;
            'win: for yy in y as isize - r as isize..=(y + r) as isize {
                for xx in x as isize - r as isize..=(x + r) as isize {
                    let v = xx >= 0
                        && yy >= 0
                        && (xx as usize) < w
                        && (yy as usize) < h
                        && m[yy as usize * w + xx as usize];
                    if erode && !v {
                        hit = false;
                        break 'win;
                    }
                    if !erode && v {
                        hit = true;
                        break 'win;
                    }
                }
            }
            *o = hit;
        }
    });
    out
}

/// 4-connected components of `m`: a label per pixel (`usize::MAX` unmarked) and the count.
fn components(m: &[bool], w: usize, h: usize) -> (Vec<usize>, usize) {
    let mut labels = vec![usize::MAX; w * h];
    let mut count = 0;
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !m[start] || labels[start] != usize::MAX {
            continue;
        }
        labels[start] = count;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            let mut visit = |j: usize| {
                if m[j] && labels[j] == usize::MAX {
                    labels[j] = count;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < w {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - w);
            }
            if y + 1 < h {
                visit(i + w);
            }
        }
        count += 1;
    }
    (labels, count)
}

/// Merge weight of a sample by its brightest channel.
#[inline]
fn taper(m: f32) -> f32 {
    if m <= TAPER_START {
        1.0
    } else if m >= TAPER_END {
        0.0
    } else {
        let t = (TAPER_END - m) / (TAPER_END - TAPER_START);
        t * t * (3.0 - 2.0 * t)
    }
}

fn sample(p: &Plan<'_>, x: usize, y: usize) -> Option<[f32; 3]> {
    match &p.h {
        None => Some(p.frame.px(x, y)),
        Some(h) => {
            let (u, v) = project(h, x as f64, y as f64)?;
            p.frame.sample(u as f32, v as f32)
        }
    }
}

/// `rad` moved toward the single exposure `single` (frame units, × `k`) by `m`; a
/// clipped sample keeps the merge (it would paste a dim highlight).
#[inline]
fn blend_single(rad: [f32; 3], single: [f32; 3], k: f32, m: f32) -> [f32; 3] {
    if single.iter().any(|&c| c >= CLIPPED) {
        return rad;
    }
    std::array::from_fn(|c| rad[c] + m * (single[c] * k - rad[c]))
}

fn merge_rows(
    plans: &[Plan<'_>],
    ghosts: Option<&Ghosts>,
    out_scale: f32,
    progress: &Stage<'_>,
) -> Result<Frame, MergeError> {
    let (w, h) = (plans[0].frame.w as usize, plans[0].frame.h as usize);
    let mut data = vec![0u16; w * h * 3];
    let done = AtomicUsize::new(0);
    // Rows finish in any order: report under a lock so the fraction never goes back.
    let reported = Mutex::new(0usize);
    data.par_chunks_mut(w * 3)
        .enumerate()
        .try_for_each(|(y, row)| {
            if progress.cancelled() {
                return Err(MergeError::Cancelled);
            }
            for x in 0..w {
                let mut acc = [0.0f32; 3];
                let mut wsum = 0.0f32;
                let mut fallback: Option<([f32; 3], bool)> = None;
                for p in plans {
                    let Some(v) = sample(p, x, y) else {
                        continue;
                    };
                    let rad = v.map(|c| c * p.k);
                    let top = v[0].max(v[1]).max(v[2]);
                    fallback.get_or_insert((rad, top >= SATURATED));
                    let wt = taper(top) / p.k;
                    if wt > 0.0 {
                        for c in 0..3 {
                            acc[c] += wt * rad[c];
                        }
                        wsum += wt;
                    }
                }
                let mut rad = match fallback {
                    _ if wsum > 1e-12 => acc.map(|c| c / wsum),
                    // Clipped in every frame that sees it: the clip, on every channel, so
                    // the pipeline reconstructs it as a highlight (not a dim colour).
                    Some((_, true)) => [WHITE / out_scale; 3],
                    Some((v, false)) => v,
                    None => [0.0; 3],
                };
                if let Some(g) = ghosts
                    && let Some((m, i)) = g.at(x, y)
                    && let Some(single) = sample(&plans[i], x, y)
                {
                    rad = blend_single(rad, single, plans[i].k, m);
                }
                for c in 0..3 {
                    let v = rad[c] * out_scale + dither(x, y, c);
                    row[x * 3 + c] = v.clamp(0.0, WHITE).round() as u16;
                }
            }
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(64) {
                let mut last = reported.lock().unwrap_or_else(PoisonError::into_inner);
                if n > *last {
                    *last = n;
                    progress.set(n as f32 / h as f32);
                }
            }
            Ok(())
        })?;
    progress.set(1.0);
    Frame::new(w as u32, h as u32, data)
}

impl Ghosts {
    /// Single-exposure weight and frame at full-resolution `(x, y)`, when it applies.
    #[inline]
    fn at(&self, x: usize, y: usize) -> Option<(f32, usize)> {
        let gx = ((x as f32 + 0.5) / self.factor - 0.5).clamp(0.0, (self.w - 1) as f32);
        let gy = ((y as f32 + 0.5) / self.factor - 0.5).clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (gx as usize, gy as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (fx, fy) = (gx - x0 as f32, gy - y0 as f32);
        let m = |xx: usize, yy: usize| self.mask[yy * self.w + xx];
        let top = m(x0, y0) + (m(x1, y0) - m(x0, y0)) * fx;
        let bottom = m(x0, y1) + (m(x1, y1) - m(x0, y1)) * fx;
        let v = top + (bottom - top) * fy;
        if v <= 0.0 {
            return None;
        }
        let near = (gy.round() as usize) * self.w + gx.round() as usize;
        let f = self.frame[near];
        (f != u8::MAX).then_some((v, usize::from(f)))
    }
}

#[cfg(test)]
#[path = "hdr_tests.rs"]
mod tests;
