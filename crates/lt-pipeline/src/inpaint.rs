//! Classical exemplar inpainting: the removal fill when the app has no model.
//!
//! The hole is synthesised from its edge inwards (onion peel, Efros–Leung / Criminisi
//! style): each pixel of the current outer layer takes the centre of the source patch
//! whose known part best matches its already-known 7×7 neighbourhood. The search is
//! `PatchMatch`'s (Barnes et al. 2009): the matches of filled neighbours, shifted
//! (propagation), a random search around the best with a halving radius, and a uniform
//! sample of sources. Sources are whole known patches near the hole (what surrounds an
//! object is the likeliest background, and a far self-similar region — a dark bush, the
//! sky — can't take over the fill).
//!
//! Why not Wexler's multiscale EM: on foliage its votes average mismatched patches and
//! the coarse-to-fine estimate drifts to the flattest region of the image (a smooth,
//! dark, plastic blob); copying exemplars keeps the texture's contrast. Deterministic for
//! a seed.

use rayon::prelude::*;

/// Patch radius (7×7 patches).
const R: i32 = 3;
/// Distance factor of a neighbour's shifted match (coherence bias).
const COHERENCE: f32 = 0.7;
/// Uniform source samples per pixel.
const SAMPLES: usize = 16;

/// `SplitMix64`: small, seedable, good enough for search positions and grain.
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub(crate) fn below(&mut self, n: usize) -> usize {
        ((self.next_u64() >> 11) % n as u64) as usize
    }

    /// Uniform in `[0, 1)`.
    pub(crate) fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Standard normal (Box–Muller).
    pub(crate) fn normal(&mut self) -> f32 {
        let u = self.unit().max(1e-7);
        let v = self.unit();
        (-2.0 * u.ln()).sqrt() * (std::f32::consts::TAU * v).cos()
    }
}

/// Source patch centres: patches wholly inside the image and the known area, within
/// reach of the hole (all of them when too few are near).
fn sources(hole: &[bool], w: usize, h: usize) -> (Vec<bool>, Vec<u32>) {
    let r = R as usize;
    // Integral image of the hole flags.
    let sw = w + 1;
    let mut sum = vec![0u32; sw * (h + 1)];
    for y in 0..h {
        let mut row = 0u32;
        for x in 0..w {
            row += u32::from(hole[y * w + x]);
            sum[(y + 1) * sw + x + 1] = sum[y * sw + x + 1] + row;
        }
    }
    let count = |x0: usize, y0: usize, x1: usize, y1: usize| {
        sum[y1 * sw + x1] + sum[y0 * sw + x0] - sum[y0 * sw + x1] - sum[y1 * sw + x0]
    };
    let (mut hx0, mut hy0, mut hx1, mut hy1) = (w, h, 0, 0);
    for (i, &v) in hole.iter().enumerate() {
        if v {
            let (x, y) = (i % w, i / w);
            (hx0, hy0, hx1, hy1) = (hx0.min(x), hy0.min(y), hx1.max(x + 1), hy1.max(y + 1));
        }
    }
    let reach = (6 * r).max((hx1.saturating_sub(hx0)).max(hy1.saturating_sub(hy0)) * 3 / 4);
    let near = |x: usize, y: usize| {
        let dx = hx0.saturating_sub(x).max(x.saturating_sub(hx1));
        let dy = hy0.saturating_sub(y).max(y.saturating_sub(hy1));
        dx.max(dy) <= reach
    };
    let mut valid = vec![false; w * h];
    let (mut close, mut far) = (Vec::new(), Vec::new());
    for y in r..h.saturating_sub(r) {
        for x in r..w.saturating_sub(r) {
            if count(x - r, y - r, x + r + 1, y + r + 1) == 0 {
                if near(x, y) {
                    close.push((y * w + x) as u32);
                } else {
                    far.push((y * w + x) as u32);
                }
            }
        }
    }
    if close.len() < 64 {
        close.extend(far);
    }
    for &i in &close {
        valid[i as usize] = true;
    }
    (valid, close)
}

/// Mean squared difference between the known pixels of the patch at `p` and the patch
/// at source `q` (`f32::MAX` when nothing around `p` is known).
#[inline]
fn masked_dist(px: &[[f32; 3]], known: &[bool], w: usize, h: usize, p: usize, q: usize) -> f32 {
    let (wi, hi) = (w as i32, h as i32);
    let (x, y) = ((p % w) as i32, (p / w) as i32);
    let (qx, qy) = ((q % w) as i32, (q / w) as i32);
    let (mut d, mut n) = (0.0f32, 0u32);
    for dy in -R..=R {
        let ty = y + dy;
        if ty < 0 || ty >= hi {
            continue;
        }
        for dx in -R..=R {
            let tx = x + dx;
            if tx < 0 || tx >= wi {
                continue;
            }
            let t = (ty * wi + tx) as usize;
            if !known[t] {
                continue;
            }
            let (a, b) = (px[t], px[((qy + dy) * wi + qx + dx) as usize]);
            d += (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2);
            n += 1;
        }
    }
    if n == 0 { f32::MAX } else { d / n as f32 }
}

/// Fill the `hole` pixels of the `w × h` image `img` from its known area. Returns the
/// whole image (known pixels unchanged), or `None` when cancelled or when nothing is
/// known to copy from.
pub fn patchmatch(
    img: &[[f32; 3]],
    hole: &[bool],
    w: usize,
    h: usize,
    seed: u64,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Option<Vec<[f32; 3]>> {
    if w == 0 || h == 0 || img.len() != w * h || hole.len() != w * h {
        return None;
    }
    if !hole.iter().any(|v| *v) {
        return Some(img.to_vec());
    }
    let (valid, srcs) = sources(hole, w, h);
    if srcs.is_empty() {
        return None;
    }
    let (wi, hi) = (w as i32, h as i32);
    let mut px = img.to_vec();
    let mut known: Vec<bool> = hole.iter().map(|v| !v).collect();
    let mut matched: Vec<Option<u32>> = vec![None; w * h];
    let mut depth = 0usize;
    loop {
        if cancelled() {
            return None;
        }
        let layer: Vec<usize> = (0..w * h)
            .into_par_iter()
            .filter(|&i| {
                !known[i] && {
                    let (x, y) = ((i % w) as i32, (i / w) as i32);
                    [(-1, 0), (1, 0), (0, -1), (0, 1)].iter().any(|(dx, dy)| {
                        let (nx, ny) = (x + dx, y + dy);
                        nx >= 0 && ny >= 0 && nx < wi && ny < hi && known[(ny * wi + nx) as usize]
                    })
                }
            })
            .collect();
        if layer.is_empty() {
            return Some(px);
        }
        // The layer's pixels are matched in parallel against what was known before it
        // (fixed chunks, one random stream each: deterministic), then filled.
        let found: Vec<(usize, usize)> = layer
            .par_chunks(256)
            .enumerate()
            .flat_map_iter(|(ci, chunk)| {
                let mut rng =
                    Rng::new(seed ^ (depth as u64).wrapping_mul(0x9e37_79b9) ^ (ci as u64) << 32);
                let (px, known, matched) = (&px, &known, &matched);
                chunk
                    .iter()
                    .map(|&p| {
                        let best = search(px, known, matched, &valid, &srcs, w, h, p, &mut rng);
                        (p, best)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        for (p, q) in found {
            px[p] = px[q];
            matched[p] = Some(q as u32);
            known[p] = true;
        }
        depth += 1;
    }
}

/// The best source for hole pixel `p` (see the module docs).
#[allow(clippy::too_many_arguments)]
fn search(
    px: &[[f32; 3]],
    known: &[bool],
    matched: &[Option<u32>],
    valid: &[bool],
    srcs: &[u32],
    w: usize,
    h: usize,
    p: usize,
    rng: &mut Rng,
) -> usize {
    let (wi, hi) = (w as i32, h as i32);
    let (x, y) = ((p % w) as i32, (p / w) as i32);
    let first = srcs[rng.below(srcs.len())] as usize;
    let mut best = (masked_dist(px, known, w, h, p, first), first);
    // `k` < 1 favours continuing a neighbour's copy (coherent texture, not speckle).
    let consider = |q: usize, best: &mut (f32, usize), k: f32| {
        let d = masked_dist(px, known, w, h, p, q) * k;
        if d < best.0 {
            *best = (d, q);
        }
    };
    // Propagation: filled neighbours' matches, shifted back to this pixel.
    for dy in -1..=1i32 {
        for dx in -1..=1i32 {
            let (nx, ny) = (x + dx, y + dy);
            if (dx, dy) == (0, 0) || nx < 0 || ny < 0 || nx >= wi || ny >= hi {
                continue;
            }
            if let Some(s) = matched[(ny * wi + nx) as usize] {
                let (sx, sy) = ((s as i32 % wi) - dx, (s as i32 / wi) - dy);
                if sx >= 0 && sy >= 0 && sx < wi && sy < hi {
                    let q = (sy * wi + sx) as usize;
                    if valid[q] {
                        consider(q, &mut best, COHERENCE);
                    }
                }
            }
        }
    }
    for _ in 0..SAMPLES {
        consider(srcs[rng.below(srcs.len())] as usize, &mut best, 1.0);
    }
    // Random search around the best, halving the radius.
    let mut rad = wi.max(hi) / 4;
    while rad >= 1 {
        let (bx, by) = ((best.1 % w) as i32, (best.1 / w) as i32);
        let sx = bx + rng.below(2 * rad as usize + 1) as i32 - rad;
        let sy = by + rng.below(2 * rad as usize + 1) as i32 - rad;
        if sx >= 0 && sy >= 0 && sx < wi && sy < hi && valid[(sy * wi + sx) as usize] {
            consider((sy * wi + sx) as usize, &mut best, 1.0);
        }
        rad /= 2;
    }
    best.1
}

/// Bilinear sample of a `w × h` image at continuous coordinates (pixel centres at
/// k + 0.5), clamped to the edge.
pub(crate) fn bilinear(img: &[[f32; 3]], w: usize, h: usize, x: f32, y: f32) -> [f32; 3] {
    let fx = (x - 0.5).clamp(0.0, (w - 1) as f32);
    let fy = (y - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (fx as usize, fy as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let (a, b, c, d) = (
        img[y0 * w + x0],
        img[y0 * w + x1],
        img[y1 * w + x0],
        img[y1 * w + x1],
    );
    [0, 1, 2].map(|k| {
        let top = a[k] + (b[k] - a[k]) * tx;
        let bot = c[k] + (d[k] - c[k]) * tx;
        top + (bot - top) * ty
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> bool {
        false
    }

    #[test]
    fn rng_is_deterministic_and_roughly_uniform() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let xs: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(Rng::new(8).next_u64(), xs[0]);
        let mut r = Rng::new(1);
        let n = 20_000;
        let mean: f32 = (0..n).map(|_| r.unit()).sum::<f32>() / n as f32;
        assert!((mean - 0.5).abs() < 0.01, "{mean}");
        let g: Vec<f32> = (0..n).map(|_| r.normal()).collect();
        let m = g.iter().sum::<f32>() / n as f32;
        let var = g.iter().map(|v| (v - m) * (v - m)).sum::<f32>() / n as f32;
        assert!(m.abs() < 0.03 && (var - 1.0).abs() < 0.05, "{m} {var}");
        assert!((0..1000).all(|_| r.below(5) < 5));
    }

    /// A smooth ramp with a hole fills with nearby values: an exemplar fill copies what
    /// surrounds the hole (it doesn't extrapolate the gradient, so the error grows to
    /// about the hole radius × slope at the centre).
    #[test]
    fn a_smooth_ramp_fills_with_nearby_values() {
        let (w, h) = (96, 80);
        let f = |x: usize, y: usize| [x as f32 / w as f32, y as f32 / h as f32, 0.4];
        let img: Vec<[f32; 3]> = (0..w * h).map(|i| f(i % w, i / w)).collect();
        let hole: Vec<bool> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32 - 48.0, (i / w) as f32 - 40.0);
                x * x + y * y < 12.0 * 12.0
            })
            .collect();
        let mut broken = img.clone();
        for (p, &hl) in broken.iter_mut().zip(&hole) {
            if hl {
                *p = [1.0, 0.0, 1.0];
            }
        }
        let out = patchmatch(&broken, &hole, w, h, 3, &never).unwrap();
        let (mut worst, mut sum, mut n) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..w * h {
            if hole[i] {
                for c in 0..3 {
                    let e = (out[i][c] - img[i][c]).abs();
                    worst = worst.max(e);
                    sum += e;
                    n += 1.0;
                }
            } else {
                assert_eq!(out[i], broken[i], "known pixels stay");
            }
        }
        assert!(worst < 0.25 && sum / n < 0.06, "{worst} {}", sum / n);
        // Same seed, same fill.
        assert_eq!(out, patchmatch(&broken, &hole, w, h, 3, &never).unwrap());
    }

    /// Stripes keep their period through the hole (texture is copied, not blurred).
    #[test]
    fn stripes_are_copied_not_blurred() {
        let (w, h) = (120, 90);
        let f = |x: usize| if (x / 4).is_multiple_of(2) { 0.9 } else { 0.1 };
        let img: Vec<[f32; 3]> = (0..w * h).map(|i| [f(i % w); 3]).collect();
        let hole: Vec<bool> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                (50..70).contains(&x) && (30..60).contains(&y)
            })
            .collect();
        let out = patchmatch(&img, &hole, w, h, 11, &never).unwrap();
        // Contrast survives (a blur would average to ~0.5) and most pixels match the
        // stripe they should be (a copy can be phase-shifted by whole periods only near
        // seams).
        let mut right = 0;
        let mut n = 0;
        for i in 0..w * h {
            if hole[i] {
                n += 1;
                let v = out[i][0];
                assert!(!(0.3..0.7).contains(&v), "blurred value {v}");
                if (v - img[i][0]).abs() < 0.2 {
                    right += 1;
                }
            }
        }
        assert!(right * 10 >= n * 8, "{right}/{n}");
    }

    #[test]
    fn degenerate_inputs() {
        let img = vec![[0.5f32; 3]; 16];
        assert!(patchmatch(&img, &[false; 15], 4, 4, 0, &never).is_none());
        assert!(patchmatch(&img, &[false; 16], 0, 4, 0, &never).is_none());
        assert_eq!(
            patchmatch(&img, &[false; 16], 4, 4, 0, &never).unwrap(),
            img,
            "nothing to fill"
        );
        // All unknown: nothing to copy from.
        assert!(patchmatch(&img, &[true; 16], 4, 4, 0, &never).is_none());
        // A cancelled fill stops.
        let (w, h) = (64, 64);
        let big = vec![[0.2f32; 3]; w * h];
        let mut hole = vec![false; w * h];
        hole[32 * w + 32] = true;
        assert!(patchmatch(&big, &hole, w, h, 0, &|| true).is_none());
        let filled = patchmatch(&big, &hole, w, h, 0, &never).unwrap();
        assert!((filled[32 * w + 32][0] - 0.2).abs() < 1e-5);
    }

    #[test]
    fn bilinear_clamps_and_interpolates() {
        let img = vec![[0.0; 3], [1.0; 3], [2.0; 3], [3.0; 3]];
        assert_eq!(bilinear(&img, 2, 2, 0.5, 0.5), [0.0; 3]);
        assert_eq!(bilinear(&img, 2, 2, 1.0, 0.5), [0.5; 3]);
        assert_eq!(bilinear(&img, 2, 2, 9.0, 9.0), [3.0; 3]);
    }
}
