//! Spatial building blocks shared by HDR, clarity/structure, NR and mask feathering:
//! block downsampling, box and Gaussian blurs, bilinear upsampling, and the fast guided
//! filter (He & Sun 2015) computed on a subsampled grid so large radii cost O(1)/pixel.
//!
//! The GPU shaders mirror these exactly (same block sizes, same edge handling).

use rayon::prelude::*;

/// A single-channel f32 image.
#[derive(Debug, Clone, PartialEq)]
pub struct Plane {
    pub w: usize,
    pub h: usize,
    pub data: Vec<f32>,
}

impl Plane {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            data: vec![0.0; w * h],
        }
    }

    pub fn from_fn(w: usize, h: usize, f: impl Fn(usize, usize) -> f32 + Sync) -> Self {
        let mut p = Self::new(w, h);
        p.data
            .par_chunks_mut(w.max(1))
            .enumerate()
            .for_each(|(y, row)| {
                for (x, v) in row.iter_mut().enumerate() {
                    *v = f(x, y);
                }
            });
        p
    }

    #[inline]
    pub fn at(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.w + x]
    }
}

/// Parameters of a subsampled guided filter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuidedParams {
    /// Subsampling factor (block size).
    pub s: usize,
    /// Box radius on the low-res grid.
    pub r: usize,
    pub eps: f32,
    /// Block-grid phase: local pixel `x` lies in block `(x + off.0) / s`. Anchors the grid
    /// to the full frame so region (tile) renders use the same blocks as a full render.
    pub off: [usize; 2],
}

impl GuidedParams {
    /// Choose a block size so the low-res radius is about 3–6 samples.
    pub fn for_radius(radius_px: f32, eps: f32) -> Self {
        let radius = radius_px.max(1.0);
        let s = ((radius / 3.0).floor() as usize).clamp(1, 32);
        let r = ((radius / s as f32).round() as usize).max(1);
        Self {
            s,
            r,
            eps,
            off: [0, 0],
        }
    }

    /// Anchor the block grid for a render whose pixel (0, 0) is frame pixel `origin`.
    #[must_use]
    pub fn anchored(mut self, origin: [i64; 2]) -> Self {
        let s = self.s as i64;
        self.off = origin.map(|o| o.rem_euclid(s) as usize);
        self
    }

    /// Output pixels on each side a render needs as context for this filter.
    pub fn reach(&self) -> usize {
        (self.r + 1) * self.s * 2
    }

    /// Force at least block size `min` (keeping the same spatial radius).
    #[must_use]
    pub fn with_min_block(self, min: usize) -> Self {
        if self.s >= min {
            return self;
        }
        let radius = (self.s * self.r) as f32;
        Self {
            s: min,
            r: ((radius / min as f32).round() as usize).max(1),
            eps: self.eps,
            off: self.off,
        }
    }
}

/// Mean of each `s×s` block of several planes at once (partial edge blocks average what exists).
/// `f` maps a full-res pixel index to the per-channel values accumulated.
pub fn block_means<const N: usize>(
    w: usize,
    h: usize,
    s: usize,
    f: impl Fn(usize) -> [f32; N] + Sync,
) -> [Plane; N] {
    block_means_off(w, h, s, [0, 0], f)
}

/// [`block_means`] with the block grid shifted by `off` (see [`GuidedParams::off`]).
pub fn block_means_off<const N: usize>(
    w: usize,
    h: usize,
    s: usize,
    off: [usize; 2],
    f: impl Fn(usize) -> [f32; N] + Sync,
) -> [Plane; N] {
    let (lw, lh) = ((w + off[0]).div_ceil(s), (h + off[1]).div_ceil(s));
    let rows: Vec<Vec<[f32; N]>> = (0..lh)
        .into_par_iter()
        .map(|ly| {
            let y0 = (ly * s).saturating_sub(off[1]);
            let y1 = ((ly + 1) * s - off[1]).min(h);
            (0..lw)
                .map(|lx| {
                    let x0 = (lx * s).saturating_sub(off[0]);
                    let x1 = ((lx + 1) * s - off[0]).min(w);
                    let mut acc = [0.0f32; N];
                    for y in y0..y1 {
                        for x in x0..x1 {
                            let v = f(y * w + x);
                            for c in 0..N {
                                acc[c] += v[c];
                            }
                        }
                    }
                    let n = ((y1 - y0) * (x1 - x0)) as f32;
                    acc.map(|a| a / n)
                })
                .collect()
        })
        .collect();
    std::array::from_fn(|c| Plane {
        w: lw,
        h: lh,
        data: rows.iter().flat_map(|r| r.iter().map(|v| v[c])).collect(),
    })
}

/// Separable box blur of radius `r` with replicated edges.
pub fn box_blur(p: &Plane, r: usize) -> Plane {
    let norm = 1.0 / (2 * r + 1) as f32;
    let (w, h) = (p.w, p.h);
    let mut tmp = Plane::new(w, h);
    tmp.data.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let src = &p.data[y * w..(y + 1) * w];
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            for d in 0..=2 * r {
                let xx = (x + d).saturating_sub(r).min(w - 1);
                acc += src[xx];
            }
            *o = acc * norm;
        }
    });
    let mut out = Plane::new(w, h);
    out.data.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            for d in 0..=2 * r {
                let yy = (y + d).saturating_sub(r).min(h - 1);
                acc += tmp.data[yy * w + x];
            }
            *o = acc * norm;
        }
    });
    out
}

/// Bilinear sample of a low-res plane (block size `s`) at full-res pixel `(x, y)`.
#[inline]
pub fn upsample_at(p: &Plane, s: usize, x: usize, y: usize) -> f32 {
    upsample_off(p, s, [0, 0], x, y)
}

/// [`upsample_at`] for a grid shifted by `off`.
#[inline]
pub fn upsample_off(p: &Plane, s: usize, off: [usize; 2], x: usize, y: usize) -> f32 {
    let inv = 1.0 / s as f32;
    let fx = (((x + off[0]) as f32 + 0.5) * inv - 0.5).clamp(0.0, (p.w - 1) as f32);
    let fy = (((y + off[1]) as f32 + 0.5) * inv - 0.5).clamp(0.0, (p.h - 1) as f32);
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

/// Low-res coefficients `(mean_a, mean_b)` of a guided filter of `src` with guide
/// `guide` (`q = mean_a·guide + mean_b`, evaluate with [`upsample_off`]): `src` smoothed
/// within regions of similar guide, following its edges.
pub fn guided_cross(
    guide: &[f32],
    src: &[f32],
    w: usize,
    h: usize,
    gp: GuidedParams,
) -> (Plane, Plane) {
    let [mg, mgg, mp, mgp] = block_means_off(w, h, gp.s, gp.off, |i| {
        let (g, p) = (guide[i], src[i]);
        [g, g * g, p, g * p]
    })
    .map(|p| box_blur(&p, gp.r));
    let mut a = Plane::new(mg.w, mg.h);
    let mut b = Plane::new(mg.w, mg.h);
    for i in 0..mg.data.len() {
        let var = (mgg.data[i] - mg.data[i] * mg.data[i]).max(0.0);
        let ai = (mgp.data[i] - mg.data[i] * mp.data[i]) / (var + gp.eps);
        a.data[i] = ai;
        b.data[i] = mp.data[i] - ai * mg.data[i];
    }
    (box_blur(&a, gp.r), box_blur(&b, gp.r))
}

/// Low-res coefficients `(mean_a, mean_b)` of a self-guided filter over `input`
/// (`q = mean_a·I + mean_b`, evaluate with [`upsample_at`]).
pub fn guided_self(input: &[f32], w: usize, h: usize, gp: GuidedParams) -> (Plane, Plane) {
    let [m1, m2] = block_means_off(w, h, gp.s, gp.off, |i| {
        let v = input[i];
        [v, v * v]
    });
    let mi = box_blur(&m1, gp.r);
    let mii = box_blur(&m2, gp.r);
    let mut a = Plane::new(mi.w, mi.h);
    let mut b = Plane::new(mi.w, mi.h);
    for i in 0..mi.data.len() {
        let var = (mii.data[i] - mi.data[i] * mi.data[i]).max(0.0);
        let ai = var / (var + gp.eps);
        a.data[i] = ai;
        b.data[i] = mi.data[i] - ai * mi.data[i];
    }
    (box_blur(&a, gp.r), box_blur(&b, gp.r))
}

/// Guided-filter coefficients of the haze estimate on the `gp` block grid: the input is
/// the dark channel of each block's mean (relative to `a`), min-filtered over 3×3 blocks
/// (a patch of ~1% of the long edge); the guide is the luminance relative to `a`
/// ([`crate::detail::haze_guide`]). A pixel's haze is `a·guide + b` (upsampled
/// coefficients), so it follows luminance edges at full resolution while the dark
/// channel itself stays a smooth, scale-independent patch statistic. The third and
/// fourth planes are the guide's self-guided coefficients (see
/// [`crate::detail::haze_local`]: smoothness and edge-aware mean for dehaze's cloud
/// protection and local contrast).
pub fn guided_haze(
    lin: &[[f32; 3]],
    w: usize,
    h: usize,
    a: [f32; 3],
    gp: GuidedParams,
) -> (Plane, Plane, Plane, Plane) {
    let [my, myy, mr, mg, mb] = block_means_off(w, h, gp.s, gp.off, |i| {
        let p = lin[i];
        let y = crate::detail::haze_guide(p, a);
        [y, y * y, p[0], p[1], p[2]]
    });
    let (lw, lh) = (my.w, my.h);
    let dark: Vec<f32> = (0..lw * lh)
        .map(|i| crate::detail::dark_channel([mr.data[i], mg.data[i], mb.data[i]], a))
        .collect();
    let patch = Plane::from_fn(lw, lh, |x, y| {
        let mut m = f32::MAX;
        for yy in y.saturating_sub(1)..(y + 2).min(lh) {
            for xx in x.saturating_sub(1)..(x + 2).min(lw) {
                m = m.min(dark[yy * lw + xx]);
            }
        }
        m
    });
    let ip = Plane {
        w: lw,
        h: lh,
        data: my
            .data
            .iter()
            .zip(&patch.data)
            .map(|(i, p)| i * p)
            .collect(),
    };
    let (mi, mii, mp, mip) = (
        box_blur(&my, gp.r),
        box_blur(&myy, gp.r),
        box_blur(&patch, gp.r),
        box_blur(&ip, gp.r),
    );
    let mut ca = Plane::new(lw, lh);
    let mut cb = Plane::new(lw, lh);
    let mut cs = Plane::new(lw, lh);
    let mut cm = Plane::new(lw, lh);
    let eps = crate::detail::DEHAZE_LC_EPS;
    for i in 0..mi.data.len() {
        let var = (mii.data[i] - mi.data[i] * mi.data[i]).max(0.0);
        let cov = mip.data[i] - mi.data[i] * mp.data[i];
        let ai = cov / (var + gp.eps);
        ca.data[i] = ai;
        cb.data[i] = mp.data[i] - ai * mi.data[i];
        let sa = var / (var + eps);
        cs.data[i] = sa;
        cm.data[i] = mi.data[i] * (1.0 - sa);
    }
    (
        box_blur(&ca, gp.r),
        box_blur(&cb, gp.r),
        box_blur(&cs, gp.r),
        box_blur(&cm, gp.r),
    )
}

/// Normalized Gaussian kernel (half: index 0 is the center) for `sigma`, radius ceil(3σ), ≤ 16.
pub fn gaussian_kernel(sigma: f32) -> Vec<f32> {
    let sigma = sigma.max(1e-3);
    let r = ((3.0 * sigma).ceil() as usize).clamp(1, 16);
    let mut k: Vec<f32> = (0..=r)
        .map(|i| (-((i * i) as f32) / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = k[0] + 2.0 * k[1..].iter().sum::<f32>();
    for v in &mut k {
        *v /= sum;
    }
    k
}

/// Separable Gaussian blur with a half-kernel from [`gaussian_kernel`], replicated edges.
pub fn gaussian_blur(p: &Plane, k: &[f32]) -> Plane {
    let (w, h) = (p.w, p.h);
    let mut tmp = Plane::new(w, h);
    tmp.data.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let src = &p.data[y * w..(y + 1) * w];
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = k[0] * src[x];
            for (d, kv) in k.iter().enumerate().skip(1) {
                acc += kv * (src[x.saturating_sub(d)] + src[(x + d).min(w - 1)]);
            }
            *o = acc;
        }
    });
    let mut out = Plane::new(w, h);
    out.data.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut acc = k[0] * tmp.data[y * w + x];
            for (d, kv) in k.iter().enumerate().skip(1) {
                acc += kv
                    * (tmp.data[y.saturating_sub(d) * w + x]
                        + tmp.data[(y + d).min(h - 1) * w + x]);
            }
            *o = acc;
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guided_params_choice() {
        let g = GuidedParams::for_radius(60.0, 0.01);
        assert_eq!((g.s, g.r), (20, 3));
        let g = GuidedParams::for_radius(3.4, 0.01);
        assert_eq!((g.s, g.r), (1, 3));
        assert_eq!(g.with_min_block(2).s, 2);
        assert_eq!(g.with_min_block(2).r, 2);
        assert_eq!(g.with_min_block(1), g);
        let g = GuidedParams::for_radius(0.2, 0.01);
        assert_eq!((g.s, g.r), (1, 1));
        let g = GuidedParams::for_radius(1e5, 0.01);
        assert_eq!(g.s, 32);
    }

    #[test]
    fn block_means_partial_blocks() {
        let data: Vec<f32> = (0..15).map(|i| i as f32).collect(); // 5×3
        let [m] = block_means(5, 3, 2, |i| [data[i]]);
        assert_eq!((m.w, m.h), (3, 2));
        assert_eq!(m.data[0], (0.0 + 1.0 + 5.0 + 6.0) / 4.0);
        assert_eq!(m.data[2], 6.5);
        assert_eq!(m.data[5], 14.0);
    }

    #[test]
    fn box_blur_preserves_constants_and_mean() {
        let p = Plane::from_fn(7, 5, |_, _| 3.0);
        assert!(box_blur(&p, 2).data.iter().all(|v| (v - 3.0).abs() < 1e-6));
        let mut imp = Plane::new(9, 9);
        imp.data[4 * 9 + 4] = 9.0;
        let b = box_blur(&imp, 1);
        assert!(
            (b.at(4, 4) - 1.0).abs() < 1e-6 && (b.at(3, 3) - 1.0).abs() < 1e-6 && b.at(2, 2) == 0.0
        );
    }

    #[test]
    fn anchored_grids_match_the_full_frame() {
        // Blocks of a window starting at frame pixel 5 line up with the full frame's.
        let full: Vec<f32> = (0..40).map(|i| (i * i % 17) as f32).collect();
        let [fm] = block_means(40, 1, 4, |i| [full[i]]);
        let gp = GuidedParams {
            s: 4,
            r: 1,
            eps: 0.1,
            off: [0, 0],
        }
        .anchored([5, 0]);
        assert_eq!(gp.off, [1, 0]);
        let win = &full[5..33];
        let [wm] = block_means_off(28, 1, 4, gp.off, |i| [win[i]]);
        // Window block 1 is frame block 2 (interior blocks are identical).
        assert_eq!(wm.data[1], fm.data[2]);
        assert_eq!(wm.data[3], fm.data[4]);
        // Upsampling at the same frame pixel reads the same low-res position.
        let frame = upsample_off(&fm, 4, [0, 0], 17, 0);
        let window = upsample_off(&wm, 4, gp.off, 12, 0);
        assert!(
            (frame - window).abs() < 1e-5 || wm.data.len() < 3,
            "{frame} {window}"
        );
        assert_eq!(GuidedParams::for_radius(9.0, 0.1).reach(), 2 * (3 + 1) * 3);
        assert_eq!(
            GuidedParams::for_radius(9.0, 0.1).anchored([-1, 7]).off,
            [2, 1]
        );
    }

    #[test]
    fn upsample_is_exact_for_s1_and_smooth_otherwise() {
        let p = Plane::from_fn(4, 4, |x, y| (x + 4 * y) as f32);
        assert_eq!(upsample_at(&p, 1, 2, 3), 14.0);
        // s = 2: full-res pixel 2 (center 2.5) sits 3/4 of the way from low-res center 0 (at
        // 1.0) to center 1 (at 3.0).
        let v = upsample_at(&p, 2, 2, 0);
        assert!((v - 0.75).abs() < 1e-6, "{v}");
        assert_eq!(upsample_at(&p, 2, 0, 0), 0.0);
        assert_eq!(upsample_at(&p, 2, 7, 7), 15.0);
    }

    #[test]
    fn guided_filter_preserves_edges_and_smooths_noise() {
        let (w, h) = (64, 32);
        let input: Vec<f32> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let base = if x < 32 { 0.2 } else { 0.8 };
                base + if (x * 7 + y * 13) % 5 == 0 {
                    0.02
                } else {
                    -0.005
                }
            })
            .collect();
        let gp = GuidedParams {
            s: 2,
            r: 3,
            eps: 0.01,
            off: [0, 0],
        };
        let (a, b) = guided_self(&input, w, h, gp);
        let q = |x, y| upsample_at(&a, gp.s, x, y) * input[y * w + x] + upsample_at(&b, gp.s, x, y);
        // Far from the edge: noise smoothed toward the local mean.
        assert!((q(8, 16) - 0.2).abs() < 0.01);
        assert!((q(56, 16) - 0.8).abs() < 0.01);
        // At the edge: the step survives (no halo / wide blur).
        assert!(
            q(30, 16) < 0.3 && q(34, 16) > 0.7,
            "{} {}",
            q(30, 16),
            q(34, 16)
        );
    }

    #[test]
    fn gaussian_kernel_and_blur() {
        let k = gaussian_kernel(1.0);
        assert_eq!(k.len(), 4);
        let sum = k[0] + 2.0 * k[1..].iter().sum::<f32>();
        assert!((sum - 1.0).abs() < 1e-6);
        assert_eq!(gaussian_kernel(100.0).len(), 17);
        let p = Plane::from_fn(10, 10, |_, _| 2.0);
        assert!(
            gaussian_blur(&p, &k)
                .data
                .iter()
                .all(|v| (v - 2.0).abs() < 1e-5)
        );
        let mut imp = Plane::new(11, 11);
        imp.data[5 * 11 + 5] = 1.0;
        let b = gaussian_blur(&imp, &k);
        assert!((b.at(5, 5) - k[0] * k[0]).abs() < 1e-6);
        assert!((b.data.iter().sum::<f32>() - 1.0).abs() < 1e-5);
    }
}
