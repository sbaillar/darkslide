//! Panorama stitching for a rotating camera.
//!
//! **Registration** (Rust feature matcher, see `features`): Harris corners and patch
//! descriptors on a ~1000 px analysis image of each frame, mutual ratio-test matches for
//! the pairs of frames next to each other in capture order (and, only when those leave
//! the pair graph in pieces, for every other pair), and an adaptive RANSAC homography
//! that keeps the geometrically consistent ones (a pure rotation induces a homography,
//! so this filter needs no focal length). Pairs run in parallel. A pair counts when it
//! has `8 + 0.3·matches` inliers (Brown & Lowe's test).
//!
//! **Camera model.** One focal length shared by every frame: EXIF (35 mm equivalent →
//! pixels) as a start, refined by a 1-D search minimizing the pairwise reprojection error
//! of rotation-only fits. Rotations come from a maximum spanning tree of the pair graph
//! (Horn's closed form per edge), then a few sweeps re-fit each camera against every
//! placed neighbour. The horizon is levelled (the camera x axes span the horizontal
//! plane) and the panorama faces the mean viewing direction.
//!
//! **Exposure.** Per-frame gains from the median luminance ratio at matched points,
//! solved in the log domain by least squares with the reference fixed.
//!
//! **Projection and blending.** Perspective, cylindrical or spherical at the source's
//! resolution at the centre (auto: perspective when the panorama is narrow enough,
//! cylindrical while it stays within ±55° of the horizon, else spherical). The output is
//! rendered in bands of rows and tiles of columns, with scratch buffers reused, so the
//! transient memory does not grow with the panorama's width. Each frame is warped into
//! the tile and split into two bands
//! (Brown & Lowe's two-band blending): the low frequencies are cross-faded with
//! centre-weighted feathers, the high frequencies are taken from the frame whose centre
//! is nearest (a seam cut), so overlaps neither ghost fine detail nor show steps. Auto
//! crop keeps the largest rectangle the frames cover.

use std::sync::{Mutex, PoisonError};

use rayon::prelude::*;

use crate::features::{Feature, detect, match_features};
use crate::geom::{
    IDENTITY, Mat3, Pair, cross3, dot3, mul, mul_vec, normalize3, ransac_homography, rotation_fit,
    sym_eigen, transpose,
};
use crate::image::{Gray, SATURATED, blur, check_analysis, dither, factor_for, luma_image};
use crate::{Frame, MergeError, Merged, Progress, Stage, WHITE};

/// Long edge of the analysis images.
const ANALYSIS_EDGE: u32 = 1000;
/// Corners per frame.
const FEATURES: usize = 1500;
/// Longest edge of the panorama written.
const MAX_EDGE: f64 = 65_000.0;
/// Most photos one panorama takes: registration and memory grow with the count.
pub const MAX_FRAMES: usize = 60;
/// Each frame is matched with this many frames after it in capture order (and, around
/// the end, with the first ones, for a full turn).
const NEIGHBOURS: usize = 2;
/// Output rows rendered together.
const BAND: usize = 256;
/// Output columns rendered together (plus the blur margin each side).
const TILE_COLS: usize = 2048;
/// Low/high band split (output px) and the blur margin it needs.
const SIGMA: f32 = 3.0;
const MARGIN: usize = 9;

/// How the sphere of view directions is flattened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Projection {
    #[default]
    Auto,
    Spherical,
    Cylindrical,
    Perspective,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanoOptions {
    pub projection: Projection,
    pub auto_crop: bool,
    /// Largest panorama written, in pixels (the result is scaled down to fit). The caller
    /// derives it from its memory budget; 6 bytes a pixel stay resident.
    pub max_pixels: u64,
}

/// Default for [`PanoOptions::max_pixels`].
pub const DEFAULT_MAX_PIXELS: u64 = 120_000_000;

impl Default for PanoOptions {
    fn default() -> Self {
        Self {
            projection: Projection::Auto,
            auto_crop: true,
            max_pixels: DEFAULT_MAX_PIXELS,
        }
    }
}

/// One frame (upright). `focal_px`: its focal length in pixels, if known.
#[derive(Debug, Clone, Copy)]
pub struct PanoInput<'a> {
    pub frame: &'a Frame,
    pub focal_px: Option<f32>,
}

/// Inlier correspondences between frames `i` and `j` (full-resolution pixels, and the
/// analysis-image pixels for exposure sampling).
#[derive(Debug, Clone)]
pub(crate) struct PairMatch {
    pub i: usize,
    pub j: usize,
    pub px: Vec<Pair>,
    pub ana: Vec<Pair>,
}

/// Cameras of a registered panorama.
#[derive(Debug, Clone)]
pub(crate) struct Registration {
    pub focal: f64,
    /// Camera ray → world ray (levelled; world z faces the panorama's centre, y down).
    pub rot: Vec<Mat3>,
    pub gain: Vec<f32>,
    pub reference: usize,
}

/// Stitch overlapping frames (see the module docs).
pub fn merge_panorama(
    inputs: &[PanoInput<'_>],
    opts: &PanoOptions,
    progress: &dyn Progress,
) -> Result<Merged, MergeError> {
    if inputs.len() < 2 {
        return Err(MergeError::TooFew);
    }
    if inputs.len() > MAX_FRAMES {
        return Err(MergeError::Invalid(format!(
            "A panorama takes at most {MAX_FRAMES} photos, got {}",
            inputs.len()
        )));
    }
    let frames: Vec<&Frame> = inputs.iter().map(|i| i.frame).collect();
    let focal = {
        let mut f: Vec<f32> = inputs
            .iter()
            .filter_map(|i| i.focal_px)
            .filter(|f| f.is_finite() && *f > 0.0)
            .collect();
        f.sort_by(f32::total_cmp);
        f.get(f.len() / 2).map(|&v| f64::from(v))
    };
    let reg = register(&frames, focal, &Stage::new(progress, 0.0, 0.35))?;
    let layout = Layout::new(&frames, &reg, *opts)?;
    Stage::new(progress, 0.35, 0.4).check()?;
    let frame = render(&frames, &reg, &layout, &Stage::new(progress, 0.4, 1.0))?;
    let gmax = reg.gain.iter().copied().fold(1.0f32, f32::max);
    Ok(Merged {
        frame,
        exposure_ev: gmax.log2(),
        reference: reg.reference,
        ghost_regions: 0,
        unaligned: 0,
        projection: layout.projection,
    })
}

/// Peak memory (bytes) of [`merge_panorama`] for frames of the given `(w, h)`: the frames
/// themselves, their analysis images, the output (at most [`PanoOptions::max_pixels`],
/// and no larger than the frames together) and the render scratch. Lets a caller refuse
/// a merge before decoding anything.
#[must_use]
pub fn estimate_bytes(sizes: &[(u32, u32)], opts: &PanoOptions) -> u64 {
    let long = sizes.iter().map(|&(w, h)| w.max(h)).max().unwrap_or(1);
    let factor = u64::from(factor_for(long, long, ANALYSIS_EDGE));
    let px: u64 = sizes
        .iter()
        .map(|&(w, h)| u64::from(w) * u64::from(h))
        .sum();
    // u16 RGB frames; analysis: luma + blurred luma (f32) at 1/factor².
    let frames = px * 6 + px / (factor * factor) * 8;
    let out = opts.max_pixels.min(px) * 6;
    frames + out + render_scratch_bytes()
}

/// Bytes of the render's per-tile scratch (warp, feather, clip, blur, accumulators).
fn render_scratch_bytes() -> u64 {
    let tile = (TILE_COLS + 2 * MARGIN) as u64 * (BAND + 2 * MARGIN) as u64;
    let acc = (TILE_COLS * BAND) as u64 * std::mem::size_of::<Acc>() as u64;
    tile * (3 * 16 + 4 + 1) + acc
}

/// Features of one frame's analysis image, in analysis pixels.
fn analysis(frame: &Frame, factor: u32) -> (Gray, Vec<Feature>) {
    let l = luma_image(frame, factor);
    let mut sorted: Vec<f32> = l.px.iter().copied().step_by(7).collect();
    let mid = sorted.len() / 2;
    let median = sorted
        .select_nth_unstable_by(mid, f32::total_cmp)
        .1
        .max(1e-4);
    let g = Gray {
        w: l.w,
        h: l.h,
        px: l
            .px
            .iter()
            .map(|v| (v * 0.18 / median).min(1.0).sqrt())
            .collect(),
    };
    let f = detect(&g, FEATURES);
    (blur(&l, 1.5), f)
}

/// Pixel ↔ ray for a frame of `w`×`h` and focal `f`.
#[inline]
fn ray(x: f64, y: f64, w: u32, h: u32, f: f64) -> [f64; 3] {
    normalize3([
        (x - f64::from(w - 1) / 2.0) / f,
        (y - f64::from(h - 1) / 2.0) / f,
        1.0,
    ])
}

/// Register the frames: pairwise inliers, focal length, rotations, levelling and gains.
pub(crate) fn register(
    frames: &[&Frame],
    focal: Option<f64>,
    progress: &Stage<'_>,
) -> Result<Registration, MergeError> {
    let n = frames.len();
    let long = frames.iter().map(|f| f.w.max(f.h)).max().unwrap_or(1);
    let factor = factor_for(long, long, ANALYSIS_EDGE);
    check_analysis(frames.iter().copied(), factor)?;
    let mut ana = Vec::with_capacity(n);
    for (k, f) in frames.iter().enumerate() {
        progress.check()?;
        ana.push(analysis(f, factor));
        progress.set(0.4 * (k + 1) as f32 / n as f32);
    }
    let to_full = |v: f32| (f64::from(v) + 0.5) * f64::from(factor) - 0.5;
    let match_pair = |i: usize, j: usize| -> Option<PairMatch> {
        let (fa, fb) = (&ana[i].1, &ana[j].1);
        let m = match_features(fa, fb, 0.8);
        let cand: Vec<Pair> = m
            .iter()
            .map(|&(a, b)| {
                (
                    (to_full(fa[a].x), to_full(fa[a].y)),
                    (to_full(fb[b].x), to_full(fb[b].y)),
                )
            })
            .collect();
        let thresh = 2.0 * f64::from(factor);
        let (_, inl) = ransac_homography(&cand, thresh, 1000)?;
        (inl.len() >= 15 && inl.len() as f64 >= 8.0 + 0.3 * m.len() as f64).then(|| PairMatch {
            i,
            j,
            px: inl.iter().map(|&k| cand[k]).collect(),
            ana: inl
                .iter()
                .map(|&k| {
                    let (a, b) = m[k];
                    (
                        (f64::from(fa[a].x), f64::from(fa[a].y)),
                        (f64::from(fb[b].x), f64::from(fb[b].y)),
                    )
                })
                .collect(),
        })
    };
    // Neighbours in capture order first; every other pair only if they leave the
    // overlap graph in pieces (photos shot or selected out of order).
    let first = neighbour_pairs(n);
    let mut matches = match_pairs(&first, &match_pair, &Stage::new(progress, 0.4, 0.6))?;
    if !connected(&matches, n) {
        let rest: Vec<(usize, usize)> = (0..n)
            .flat_map(|i| (i + 1..n).map(move |j| (i, j)))
            .filter(|p| !first.contains(p))
            .collect();
        matches.extend(match_pairs(
            &rest,
            &match_pair,
            &Stage::new(progress, 0.6, 0.8),
        )?);
    }
    let sizes: Vec<(u32, u32)> = frames.iter().map(|f| (f.w, f.h)).collect();
    let f0 = focal.unwrap_or(0.85 * f64::from(long));
    let spread = if focal.is_some() { 1.3 } else { 3.0 };
    let focal = refine_focal(&matches, &sizes, f0, spread);
    let (rot, reference) = rotations(&matches, &sizes, focal, n)?;
    let rot = level(&rot, reference);
    let lumas: Vec<&Gray> = ana.iter().map(|a| &a.0).collect();
    let gain = gains(&matches, &lumas, reference, n);
    progress.set(1.0);
    Ok(Registration {
        focal,
        rot,
        gain,
        reference,
    })
}

/// The pairs matched first: each frame with the next [`NEIGHBOURS`] in capture order,
/// wrapping around the end (a full turn closes on the first frames). Sorted, `i < j`.
fn neighbour_pairs(n: usize) -> Vec<(usize, usize)> {
    let mut pairs: Vec<(usize, usize)> = (0..n)
        .flat_map(|i| (1..=NEIGHBOURS).map(move |d| (i, (i + d) % n)))
        .filter(|&(i, j)| i != j)
        .map(|(i, j)| (i.min(j), i.max(j)))
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

/// Match `pairs` in parallel (results in `pairs` order, so the outcome is the same on
/// any thread count), reporting progress and honouring cancellation.
fn match_pairs(
    pairs: &[(usize, usize)],
    match_pair: &(impl Fn(usize, usize) -> Option<PairMatch> + Sync),
    progress: &Stage<'_>,
) -> Result<Vec<PairMatch>, MergeError> {
    let done = Mutex::new(0usize);
    let found: Vec<Option<PairMatch>> = pairs
        .par_iter()
        .map(|&(i, j)| {
            progress.check()?;
            let m = match_pair(i, j);
            // Reported under the lock so the fraction never goes backwards.
            let mut d = done.lock().unwrap_or_else(PoisonError::into_inner);
            *d += 1;
            progress.set(*d as f32 / pairs.len() as f32);
            Ok(m)
        })
        .collect::<Result<_, MergeError>>()?;
    Ok(found.into_iter().flatten().collect())
}

/// Every frame is linked to every other through `matches`.
fn connected(matches: &[PairMatch], n: usize) -> bool {
    fn find(root: &mut [usize], mut x: usize) -> usize {
        while root[x] != x {
            root[x] = root[root[x]];
            x = root[x];
        }
        x
    }
    let mut root: Vec<usize> = (0..n).collect();
    for m in matches {
        let (a, b) = (find(&mut root, m.i), find(&mut root, m.j));
        root[a] = b;
    }
    let r0 = find(&mut root, 0);
    (1..n).all(|k| find(&mut root, k) == r0)
}

/// Rays of a pair's inliers at focal `f`.
fn pair_rays(m: &PairMatch, sizes: &[(u32, u32)], f: f64) -> Vec<([f64; 3], [f64; 3])> {
    let ((wi, hi), (wj, hj)) = (sizes[m.i], sizes[m.j]);
    m.px.iter()
        .map(|&((xa, ya), (xb, yb))| (ray(xa, ya, wi, hi, f), ray(xb, yb, wj, hj, f)))
        .collect()
}

/// Truncated squared reprojection error (px²) of rotation-only fits of every pair.
fn focal_cost(matches: &[PairMatch], sizes: &[(u32, u32)], f: f64) -> f64 {
    matches
        .iter()
        .map(|m| {
            let rays = pair_rays(m, sizes, f);
            let r = rotation_fit(&rays);
            rays.iter()
                .map(|(a, b)| {
                    let d = mul_vec(&r, *a);
                    let e = [d[0] - b[0], d[1] - b[1], d[2] - b[2]];
                    (dot3(e, e) * f * f).min(16.0)
                })
                .sum::<f64>()
        })
        .sum()
}

/// The focal length (px) in `[f0 / spread, f0 · spread]` that best explains the matches.
fn refine_focal(matches: &[PairMatch], sizes: &[(u32, u32)], f0: f64, spread: f64) -> f64 {
    if matches.is_empty() {
        return f0;
    }
    let cost = |t: f64| focal_cost(matches, sizes, t.exp());
    let (lo, hi) = ((f0 / spread).ln(), (f0 * spread).ln());
    // Coarse scan, then golden-section search around the best sample.
    let steps = 24;
    let step = (hi - lo) / f64::from(steps);
    let best = (0..=steps)
        .map(|k| lo + f64::from(k) * step)
        .min_by(|a, b| cost(*a).total_cmp(&cost(*b)))
        .unwrap_or(f0.ln());
    let (mut a, mut b) = (best - step, best + step);
    let g = (5f64.sqrt() - 1.0) / 2.0;
    let (mut c, mut d) = (b - g * (b - a), a + g * (b - a));
    let (mut fc, mut fd) = (cost(c), cost(d));
    for _ in 0..40 {
        if fc < fd {
            b = d;
            d = c;
            fd = fc;
            c = b - g * (b - a);
            fc = cost(c);
        } else {
            a = c;
            c = d;
            fc = fd;
            d = a + g * (b - a);
            fd = cost(d);
        }
    }
    f64::midpoint(a, b).exp()
}

/// Camera → world rotations (the reference is the identity) and the reference index.
fn rotations(
    matches: &[PairMatch],
    sizes: &[(u32, u32)],
    f: f64,
    n: usize,
) -> Result<(Vec<Mat3>, usize), MergeError> {
    let mut weight = vec![0usize; n];
    for m in matches {
        weight[m.i] += m.px.len();
        weight[m.j] += m.px.len();
    }
    let reference = (0..n)
        .max_by_key(|&i| (weight[i], std::cmp::Reverse(i)))
        .unwrap_or(0);
    let mut rot: Vec<Option<Mat3>> = vec![None; n];
    rot[reference] = Some(IDENTITY);
    let mut order = vec![reference];
    // Prim: always attach the unplaced frame with the strongest edge to a placed one.
    while order.len() < n {
        let edge = matches
            .iter()
            .filter(|m| rot[m.i].is_some() != rot[m.j].is_some())
            .max_by_key(|m| m.px.len());
        let Some(m) = edge else {
            return Err(MergeError::NoOverlap(
                "These photos don't overlap enough to stitch a panorama".into(),
            ));
        };
        // b = R_ij · a maps rays of i to rays of j.
        let r_ij = rotation_fit(&pair_rays(m, sizes, f));
        let (placed, new, r) = match (rot[m.i], rot[m.j]) {
            (Some(ri), _) => (ri, m.j, transpose(&r_ij)),
            (_, Some(rj)) => (rj, m.i, r_ij),
            (None, None) => unreachable!("filtered above"),
        };
        rot[new] = Some(mul(&placed, &r));
        order.push(new);
    }
    let mut rot: Vec<Mat3> = rot.into_iter().map(|r| r.unwrap_or(IDENTITY)).collect();
    // Re-fit each camera against all its neighbours (Gauss–Seidel sweeps).
    for _ in 0..4 {
        for &k in order.iter().skip(1) {
            let mut pairs = Vec::new();
            for m in matches.iter().filter(|m| m.i == k || m.j == k) {
                for (a, b) in pair_rays(m, sizes, f) {
                    if m.i == k {
                        pairs.push((a, mul_vec(&rot[m.j], b)));
                    } else {
                        pairs.push((b, mul_vec(&rot[m.i], a)));
                    }
                }
            }
            rot[k] = rotation_fit(&pairs);
        }
    }
    Ok((rot, reference))
}

/// Level the horizon: world y is the direction the camera x axes are all perpendicular
/// to (or the mean camera "down" when they don't span a plane), z the mean view.
pub(crate) fn level(rot: &[Mat3], reference: usize) -> Vec<Mat3> {
    let col = |r: &Mat3, c: usize| [r[0][c], r[1][c], r[2][c]];
    let mut m = [[0.0f64; 3]; 3];
    let (mut down, mut fwd) = ([0.0f64; 3], [0.0f64; 3]);
    for r in rot {
        let x = col(r, 0);
        for i in 0..3 {
            for j in 0..3 {
                m[i][j] += x[i] * x[j];
            }
            down[i] += r[i][1];
            fwd[i] += r[i][2];
        }
    }
    let (vals, vecs) = sym_eigen(m);
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&a, &b| vals[a].total_cmp(&vals[b]));
    let n = rot.len() as f64;
    let mut y = if vals[idx[1]] > 0.02 * n {
        [vecs[0][idx[0]], vecs[1][idx[0]], vecs[2][idx[0]]]
    } else {
        normalize3(down)
    };
    if dot3(y, down) < 0.0 {
        y = y.map(|v| -v);
    }
    let flat = |f: [f64; 3]| {
        let d = dot3(f, y);
        [f[0] - d * y[0], f[1] - d * y[1], f[2] - d * y[2]]
    };
    let mut h = flat(fwd);
    // A full ring's views cancel out: face the reference camera instead.
    if dot3(h, h) < 1e-6 * n * n {
        h = flat(col(&rot[reference], 2));
    }
    let z = normalize3(h);
    let x = cross3(y, z);
    let l = [x, y, z];
    rot.iter().map(|r| mul(&l, r)).collect()
}

/// Per-frame gains bringing every frame to the reference's exposure.
fn gains(matches: &[PairMatch], lumas: &[&Gray], reference: usize, n: usize) -> Vec<f32> {
    // d_ij = ln(L_j / L_i) at the same scene points: ln g_i − ln g_j = d_ij.
    let mut edges: Vec<(usize, usize, f64, f64)> = Vec::new();
    for m in matches {
        let mut logs: Vec<f32> = m
            .ana
            .iter()
            .filter_map(|&((xa, ya), (xb, yb))| {
                let a = lumas[m.i].sample(xa as f32, ya as f32);
                let b = lumas[m.j].sample(xb as f32, yb as f32);
                ((0.01..0.85).contains(&a) && (0.01..0.85).contains(&b)).then(|| (b / a).ln())
            })
            .collect();
        if logs.len() >= 8 {
            let mid = logs.len() / 2;
            let d = *logs.select_nth_unstable_by(mid, f32::total_cmp).1;
            edges.push((m.i, m.j, f64::from(d), logs.len() as f64));
        }
    }
    let mut lg = vec![0.0f64; n];
    for _ in 0..100 {
        for k in (0..n).filter(|&k| k != reference) {
            let (mut num, mut den) = (0.0, 0.0);
            for &(i, j, d, w) in &edges {
                if i == k {
                    num += w * (lg[j] + d);
                    den += w;
                } else if j == k {
                    num += w * (lg[i] - d);
                    den += w;
                }
            }
            if den > 0.0 {
                lg[k] = num / den;
            }
        }
    }
    lg.iter()
        .map(|v| (v.exp() as f32).clamp(1.0 / 16.0, 16.0))
        .collect()
}

/// Flattening of world rays to plane coordinates (radians or tangent units).
#[inline]
fn to_plane(p: Projection, r: [f64; 3]) -> Option<(f64, f64)> {
    match p {
        Projection::Perspective => (r[2] > 1e-3).then(|| (r[0] / r[2], r[1] / r[2])),
        Projection::Cylindrical => {
            let h = r[0].hypot(r[2]);
            (h > 1e-6).then(|| (r[0].atan2(r[2]), r[1] / h))
        }
        Projection::Spherical | Projection::Auto => {
            let n = dot3(r, r).sqrt();
            Some((r[0].atan2(r[2]), (r[1] / n).clamp(-1.0, 1.0).asin()))
        }
    }
}

#[inline]
fn from_plane(p: Projection, u: f64, v: f64) -> [f64; 3] {
    match p {
        Projection::Perspective => [u, v, 1.0],
        Projection::Cylindrical => [u.sin(), v, u.cos()],
        Projection::Spherical | Projection::Auto => {
            let c = v.cos();
            [u.sin() * c, v.sin(), u.cos() * c]
        }
    }
}

/// A frame placed in the output.
#[derive(Debug, Clone)]
struct Placed {
    /// World → camera.
    rt: Mat3,
    w: u32,
    h: u32,
    gain: f32,
    /// Output rows and columns it can touch.
    rows: (usize, usize),
    cols: (usize, usize),
}

impl Placed {
    /// Frame pixel seen along world ray `r`, if inside the frame.
    #[inline]
    fn pixel(&self, r: [f64; 3], f: f64) -> Option<(f32, f32)> {
        let c = mul_vec(&self.rt, r);
        if c[2] <= 1e-6 {
            return None;
        }
        let x = f * c[0] / c[2] + f64::from(self.w - 1) / 2.0;
        let y = f * c[1] / c[2] + f64::from(self.h - 1) / 2.0;
        (x >= 0.0 && y >= 0.0 && x <= f64::from(self.w - 1) && y <= f64::from(self.h - 1))
            .then_some((x as f32, y as f32))
    }

    /// Centre-weighted feather: 1 at the centre, 0 at the edges.
    #[inline]
    fn feather(&self, x: f32, y: f32) -> f32 {
        let (hw, hh) = ((self.w - 1) as f32 / 2.0, (self.h - 1) as f32 / 2.0);
        (1.0 - (x - hw).abs() / hw).max(1e-4) * (1.0 - (y - hh).abs() / hh).max(1e-4)
    }
}

/// Where everything goes in the output.
#[derive(Debug, Clone)]
struct Layout {
    projection: Projection,
    focal: f64,
    /// Plane coordinates of output pixel (0, 0)'s corner and output pixels per unit.
    u0: f64,
    v0: f64,
    scale: f64,
    w: usize,
    h: usize,
    placed: Vec<Placed>,
}

/// Plane outline of a frame (border samples), unwrapped around its centre's longitude.
fn outline(p: Projection, f: &Frame, rot: &Mat3, focal: f64) -> Option<Vec<(f64, f64)>> {
    let (w, h) = (f64::from(f.w - 1), f64::from(f.h - 1));
    let steps = 32;
    let mut pts = Vec::with_capacity(4 * steps + 1);
    let world = |x: f64, y: f64| mul_vec(rot, ray(x, y, f.w, f.h, focal));
    let (uc, _) = to_plane(p, world(w / 2.0, h / 2.0))?;
    for k in 0..steps {
        let t = k as f64 / steps as f64;
        for (x, y) in [(t * w, 0.0), (w, t * h), (w - t * w, h), (0.0, h - t * h)] {
            let (mut u, v) = to_plane(p, world(x, y))?;
            if p != Projection::Perspective {
                let pi = std::f64::consts::PI;
                u = uc + (u - uc + pi).rem_euclid(2.0 * pi) - pi;
            }
            pts.push((u, v));
        }
    }
    Some(pts)
}

impl Layout {
    fn new(frames: &[&Frame], reg: &Registration, opts: PanoOptions) -> Result<Self, MergeError> {
        let projection = match opts.projection {
            Projection::Auto => auto_projection(frames, reg),
            p => p,
        };
        let outlines: Option<Vec<Vec<(f64, f64)>>> = frames
            .iter()
            .zip(&reg.rot)
            .map(|(f, r)| outline(projection, f, r, reg.focal))
            .collect();
        let feasible = projection != Projection::Perspective || perspective_fits(frames, reg);
        let Some(outlines) = outlines.filter(|_| feasible) else {
            return Err(MergeError::Invalid(format!(
                "These photos cover too wide a view for a {} projection",
                match projection {
                    Projection::Perspective => "perspective",
                    Projection::Cylindrical => "cylindrical",
                    Projection::Spherical | Projection::Auto => "spherical",
                }
            )));
        };
        let pi = std::f64::consts::PI;
        let (mut u0, mut u1, mut v0, mut v1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for &(u, v) in outlines.iter().flatten() {
            (u0, u1, v0, v1) = (u0.min(u), u1.max(u), v0.min(v), v1.max(v));
        }
        if projection != Projection::Perspective && u1 - u0 > 2.0 * pi {
            (u0, u1) = (-pi, pi);
        }
        let mut scale = reg.focal;
        let (du, dv) = (u1 - u0, v1 - v0);
        let fit = (opts.max_pixels as f64 / (du * dv * scale * scale))
            .sqrt()
            .min(MAX_EDGE / (du.max(dv) * scale));
        if fit < 1.0 {
            scale *= fit;
        }
        let (w, h) = (
            (du * scale).ceil().max(1.0) as usize,
            (dv * scale).ceil().max(1.0) as usize,
        );
        let placed = outlines
            .iter()
            .zip(&reg.rot)
            .zip(frames.iter().zip(&reg.gain))
            .map(|((o, r), (f, &gain))| {
                let (mut a, mut b, mut c, mut d) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
                for &(u, v) in o {
                    (a, b, c, d) = (a.min(u), b.max(u), c.min(v), d.max(v));
                }
                // A frame across the ±180° seam may land anywhere in a row.
                let cols = if projection != Projection::Perspective && (a < -pi || b > pi) {
                    (0, w)
                } else {
                    px_range(a, b, u0, scale, w)
                };
                Placed {
                    rt: transpose(r),
                    w: f.w,
                    h: f.h,
                    gain,
                    rows: px_range(c, d, v0, scale, h),
                    cols,
                }
            })
            .collect();
        let mut layout = Self {
            projection,
            focal: reg.focal,
            u0,
            v0,
            scale,
            w,
            h,
            placed,
        };
        if opts.auto_crop {
            layout.crop();
        }
        Ok(layout)
    }

    #[inline]
    fn ray_at(&self, x: usize, y: usize) -> [f64; 3] {
        let u = self.u0 + (x as f64 + 0.5) / self.scale;
        let v = self.v0 + (y as f64 + 0.5) / self.scale;
        from_plane(self.projection, u, v)
    }

    fn covered(&self, x: usize, y: usize) -> bool {
        let r = self.ray_at(x, y);
        self.placed.iter().any(|p| p.pixel(r, self.focal).is_some())
    }

    /// Shrink to the largest axis-aligned rectangle the frames cover.
    fn crop(&mut self) {
        let step = (self.w.max(self.h) / 800).max(1);
        let (gw, gh) = (self.w.div_ceil(step), self.h.div_ceil(step));
        let grid: Vec<bool> = (0..gw * gh)
            .into_par_iter()
            .map(|i| {
                self.covered(
                    ((i % gw) * step).min(self.w - 1),
                    ((i / gw) * step).min(self.h - 1),
                )
            })
            .collect();
        let Some((x0, y0, x1, y1)) = largest_rectangle(&grid, gw, gh) else {
            return;
        };
        // Inside the outermost covered grid points (one step in, to stay covered between).
        let px0 = x0 * step + usize::from(x0 > 0) * step;
        let py0 = y0 * step + usize::from(y0 > 0) * step;
        let px1 = (x1 * step)
            .min(self.w - 1)
            .saturating_sub(usize::from(x1 + 1 < gw) * step);
        let py1 = (y1 * step)
            .min(self.h - 1)
            .saturating_sub(usize::from(y1 + 1 < gh) * step);
        if px1 <= px0 + 8 || py1 <= py0 + 8 {
            return;
        }
        self.u0 += px0 as f64 / self.scale;
        self.v0 += py0 as f64 / self.scale;
        let (w, h) = (px1 - px0 + 1, py1 - py0 + 1);
        for p in &mut self.placed {
            p.cols = (
                p.cols.0.saturating_sub(px0).min(w),
                p.cols.1.saturating_sub(px0).min(w),
            );
            p.rows = (
                p.rows.0.saturating_sub(py0).min(h),
                p.rows.1.saturating_sub(py0).min(h),
            );
        }
        (self.w, self.h) = (w, h);
    }
}

/// Output pixel range `[a, b]` of plane coordinates (one pixel of slack, clamped).
fn px_range(a: f64, b: f64, origin: f64, scale: f64, n: usize) -> (usize, usize) {
    let lo = ((a - origin) * scale - 1.0).floor().max(0.0) as usize;
    let hi = (((b - origin) * scale + 2.0).ceil().max(0.0) as usize).min(n);
    (lo.min(n), hi)
}

/// Every frame corner is well in front of the panorama's centre (perspective works).
fn perspective_fits(frames: &[&Frame], reg: &Registration) -> bool {
    frames.iter().zip(&reg.rot).all(|(f, r)| {
        let (w, h) = (f64::from(f.w - 1), f64::from(f.h - 1));
        [
            (0.0, 0.0),
            (w, 0.0),
            (0.0, h),
            (w, h),
            (w / 2.0, 0.0),
            (w / 2.0, h),
        ]
        .iter()
        .all(|&(x, y)| mul_vec(r, ray(x, y, f.w, f.h, reg.focal))[2] > 0.26)
    })
}

/// Auto projection: perspective for narrow panoramas, cylindrical within ±55° of the
/// horizon, spherical otherwise.
fn auto_projection(frames: &[&Frame], reg: &Registration) -> Projection {
    let Some(outlines) = frames
        .iter()
        .zip(&reg.rot)
        .map(|(f, r)| outline(Projection::Spherical, f, r, reg.focal))
        .collect::<Option<Vec<_>>>()
    else {
        return Projection::Spherical;
    };
    let (mut u0, mut u1, mut vmax) = (f64::MAX, f64::MIN, 0.0f64);
    for &(u, v) in outlines.iter().flatten() {
        (u0, u1, vmax) = (u0.min(u), u1.max(u), vmax.max(v.abs()));
    }
    let deg = |r: f64| r.to_degrees();
    if deg(u1 - u0) < 100.0 && deg(vmax) < 50.0 && perspective_fits(frames, reg) {
        Projection::Perspective
    } else if deg(vmax) < 55.0 {
        Projection::Cylindrical
    } else {
        Projection::Spherical
    }
}

/// Largest all-true axis-aligned rectangle of a grid: inclusive `(x0, y0, x1, y1)`.
fn largest_rectangle(grid: &[bool], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let mut heights = vec![0usize; w];
    let mut best: Option<(usize, (usize, usize, usize, usize))> = None;
    let mut stack: Vec<usize> = Vec::with_capacity(w + 1);
    for y in 0..h {
        for x in 0..w {
            heights[x] = if grid[y * w + x] { heights[x] + 1 } else { 0 };
        }
        stack.clear();
        for x in 0..=w {
            let cur = if x < w { heights[x] } else { 0 };
            while let Some(&top) = stack.last() {
                if heights[top] < cur {
                    break;
                }
                stack.pop();
                let hgt = heights[top];
                let left = stack.last().map_or(0, |&l| l + 1);
                let area = hgt * (x - left);
                if hgt > 0 && best.is_none_or(|(a, _)| area > a) {
                    best = Some((area, (left, y + 1 - hgt, x - 1, y)));
                }
            }
            stack.push(x);
        }
    }
    best.map(|(_, r)| r)
}

/// One output pixel's blend accumulators.
#[derive(Debug, Clone, Copy, Default)]
struct Acc {
    low: [f32; 3],
    w: f32,
    high: [f32; 3],
    best: f32,
    /// A frame saw this pixel at its clip.
    clipped: bool,
}

/// Reset `v` to `n` copies of `fill`, keeping its allocation.
fn reset<T: Copy>(v: &mut Vec<T>, n: usize, fill: T) {
    v.clear();
    v.resize(n, fill);
}

/// Separable Gaussian of premultiplied RGBA with zero outside (normalized convolution).
/// `tmp` and `out` are scratch buffers, resized here (the result is `out`).
fn blur4(
    buf: &[[f32; 4]],
    w: usize,
    h: usize,
    kernel: &[f32],
    tmp: &mut Vec<[f32; 4]>,
    out: &mut Vec<[f32; 4]>,
) {
    let r = (kernel.len() / 2) as isize;
    reset(tmp, w * h, [0.0; 4]);
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let src = &buf[y * w..(y + 1) * w];
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = [0.0f32; 4];
            for (k, kv) in kernel.iter().enumerate() {
                let xx = x as isize + k as isize - r;
                if xx >= 0 && (xx as usize) < w {
                    let s = src[xx as usize];
                    for c in 0..4 {
                        acc[c] += kv * s[c];
                    }
                }
            }
            *o = acc;
        }
    });
    reset(out, w * h, [0.0; 4]);
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (k, kv) in kernel.iter().enumerate() {
            let yy = y as isize + k as isize - r;
            if yy < 0 || yy as usize >= h {
                continue;
            }
            for (o, s) in row
                .iter_mut()
                .zip(&tmp[yy as usize * w..(yy as usize + 1) * w])
            {
                for c in 0..4 {
                    o[c] += kv * s[c];
                }
            }
        }
    });
}

fn render(
    frames: &[&Frame],
    reg: &Registration,
    layout: &Layout,
    progress: &Stage<'_>,
) -> Result<Frame, MergeError> {
    render_tiled(frames, reg, layout, progress, TILE_COLS)
}

/// [`render`] with `tile_cols` output columns per tile (the result does not depend on it).
fn render_tiled(
    frames: &[&Frame],
    reg: &Registration,
    layout: &Layout,
    progress: &Stage<'_>,
    tile_cols: usize,
) -> Result<Frame, MergeError> {
    let (w, h) = (layout.w, layout.h);
    let kernel: Vec<f32> = {
        let r = MARGIN as isize;
        let k: Vec<f32> = (-r..=r)
            .map(|i| (-(i * i) as f32 / (2.0 * SIGMA * SIGMA)).exp())
            .collect();
        let s: f32 = k.iter().sum();
        k.iter().map(|v| v / s).collect()
    };
    let out_scale = WHITE / reg.gain.iter().copied().fold(1.0f32, f32::max);
    let mut data = vec![0u16; w * h * 3];
    let bands = h.div_ceil(BAND);
    // Scratch shared by every band, tile and frame.
    let mut acc: Vec<Acc> = Vec::new();
    let mut img: Vec<[f32; 4]> = Vec::new();
    let mut feather: Vec<f32> = Vec::new();
    let mut clip: Vec<bool> = Vec::new();
    let (mut tmp, mut low): (Vec<[f32; 4]>, Vec<[f32; 4]>) = (Vec::new(), Vec::new());
    for (band, out) in data.chunks_mut(BAND * w * 3).enumerate() {
        progress.check()?;
        let (y0, y1) = (band * BAND, (band * BAND + BAND).min(h));
        let (ey0, ey1) = (y0.saturating_sub(MARGIN), (y1 + MARGIN).min(h));
        for t0 in (0..w).step_by(tile_cols) {
            let t1 = (t0 + tile_cols).min(w);
            let tw = t1 - t0;
            reset(&mut acc, (y1 - y0) * tw, Acc::default());
            for (p, frame) in layout.placed.iter().zip(frames) {
                if p.rows.1 <= ey0 || p.rows.0 >= ey1 || p.cols.1 <= t0 || p.cols.0 >= t1 {
                    continue;
                }
                // The frame's columns in this tile, plus the blur margin each side.
                let c0 = p.cols.0.max(t0.saturating_sub(MARGIN));
                let c1 = p.cols.1.min(t1 + MARGIN);
                let (bw, bh) = (c1 - c0, ey1 - ey0);
                // Warp: premultiplied RGB + coverage, and the feather weight.
                reset(&mut img, bw * bh, [0.0; 4]);
                reset(&mut feather, bw * bh, 0.0);
                // Clipped samples stay out of the blend (scaled by a gain below 1 they
                // would read as dim unclipped colour); they only mark the pixel as clipped.
                reset(&mut clip, bw * bh, false);
                img.par_chunks_mut(bw)
                    .zip(feather.par_chunks_mut(bw))
                    .zip(clip.par_chunks_mut(bw))
                    .enumerate()
                    .for_each(|(r, ((row, fr), cr))| {
                        for (k, o) in row.iter_mut().enumerate() {
                            let ray = layout.ray_at(c0 + k, ey0 + r);
                            if let Some((x, y)) = p.pixel(ray, layout.focal)
                                && let Some(v) = frame.sample(x, y)
                            {
                                if v[0].max(v[1]).max(v[2]) >= SATURATED {
                                    cr[k] = true;
                                } else {
                                    *o = [v[0] * p.gain, v[1] * p.gain, v[2] * p.gain, 1.0];
                                    fr[k] = p.feather(x, y);
                                }
                            }
                        }
                    });
                blur4(&img, bw, bh, &kernel, &mut tmp, &mut low);
                // Only the tile's own columns are accumulated; the margin was context.
                let (k0, k1) = (t0.max(c0) - c0, t1.min(c1) - c0);
                acc.par_chunks_mut(tw).enumerate().for_each(|(r, arow)| {
                    let br = y0 + r - ey0;
                    for k in k0..k1 {
                        let i = br * bw + k;
                        let (s, l, fw) = (img[i], low[i], feather[i]);
                        let a = &mut arow[c0 + k - t0];
                        a.clipped |= clip[i];
                        if s[3] <= 0.0 || l[3] <= 1e-4 {
                            continue;
                        }
                        for c in 0..3 {
                            let lo = l[c] / l[3];
                            a.low[c] += fw * lo;
                            if fw > a.best {
                                a.high[c] = s[c] - lo;
                            }
                        }
                        a.w += fw;
                        a.best = a.best.max(fw);
                    }
                });
            }
            out.par_chunks_mut(w * 3).enumerate().for_each(|(r, row)| {
                for x in t0..t1 {
                    let a = acc[r * tw + x - t0];
                    if a.w <= 0.0 {
                        if a.clipped {
                            // Only clipped samples here: the clip on every channel, so the
                            // pipeline reconstructs a highlight.
                            row[x * 3..x * 3 + 3].fill(u16::MAX);
                        }
                        continue;
                    }
                    for c in 0..3 {
                        let v = (a.low[c] / a.w + a.high[c]).max(0.0) * out_scale
                            + dither(x, y0 + r, c);
                        row[x * 3 + c] = v.clamp(0.0, WHITE).round() as u16;
                    }
                }
            });
        }
        progress.set((band + 1) as f32 / bands as f32);
    }
    Frame::new(w as u32, h as u32, data)
}

#[cfg(test)]
#[path = "pano_tests.rs"]
mod tests;
