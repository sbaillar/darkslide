//! Heal and clone spots, applied to the camera-linear samples before any other develop
//! step (the GPU mirrors this in `scene_fn.wgsl` and `spot_stats.wgsl`).
//!
//! A spot's destination is a disc in the oriented frame (isotropic E-normalized units);
//! its source is the same disc shifted, sampled through the full geometry (lens,
//! keystone). The source disc is pinned inside the E frame and every source read is
//! clamped to the sensor edge, so a spot never paints black.
//!
//! Clone blends the source in. Heal multiplies the source's texture by a smooth gain
//! taken from rings around destination and source: a luminance ratio per angular sector
//! (8 sectors; trimmed means of the on-image samples; interpolated in log space) times a
//! bounded whole-ring chroma correction, so tone and colour come from the destination's
//! surroundings without casts, and one bright neighbour doesn't bloom into the disc.
//!
//! Sources are always read from the BASE (spots don't chain). A spot outside the render
//! is dropped from the plan. There is no cap on the count: the spots are binned into
//! [`BIN`]-pixel output tiles, so each pixel visits only the spots that can touch it.

use lt_core::adjust::{Spot, SpotKind};

use crate::geometry::Geometry;

/// Angular sectors of the heal rings.
pub const SECTORS: usize = 8;
/// Ring radii (share of the spot radius) sampled for the heal statistics.
pub const RING: [f32; 3] = [1.1, 1.25, 1.4];
/// Samples per sector and ring radius.
pub const RING_ANGLES: usize = 4;
/// Side of the output-pixel tiles the spots are binned into.
pub const BIN: u32 = 32;
/// Bounds of the heal's luminance ratio.
pub const RATIO: [f32; 2] = [0.25, 4.0];
/// Bounds of the heal's per-channel chroma correction (relative to the luminance ratio).
pub const CHROMA: [f32; 2] = [0.87, 1.15];

/// A spot ready for one render.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpotPlan {
    pub heal: bool,
    /// Center and radius in isotropic E-normalized units (`uv × iso`).
    pub c: [f32; 2],
    pub r: f32,
    /// 0..=1 soft edge as a share of the radius.
    pub feather: f32,
    pub opacity: f32,
    /// Source offset in output pixels.
    pub d: [f32; 2],
    /// Conservative output-pixel bounds of the destination disc: x0, y0, x1, y1.
    pub bbox: [f32; 4],
}

/// Spot indices per [`BIN`]-pixel output tile (row-major, `tiles_x` wide), in plan
/// order: tile `t` holds `indices[offsets[t]..offsets[t + 1]]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bins {
    pub tiles_x: u32,
    pub offsets: Vec<u32>,
    pub indices: Vec<u32>,
}

impl Bins {
    /// The spots that can touch output pixel `(x, y)`.
    #[inline]
    pub fn at(&self, x: usize, y: usize) -> &[u32] {
        let b = BIN as usize;
        let t = (y / b) * self.tiles_x as usize + x / b;
        match (self.offsets.get(t), self.offsets.get(t + 1)) {
            (Some(&a), Some(&e)) => &self.indices[a as usize..e as usize],
            _ => &[],
        }
    }
}

/// A source coordinate (isotropic units) pinned so the source disc lies inside the E
/// frame `[0, extent]`; a disc wider than the frame is centered.
fn pin(s: f32, r: f32, extent: f32) -> f32 {
    if 2.0 * r >= extent {
        0.5 * extent
    } else {
        s.clamp(r, extent - r)
    }
}

/// Plan the spots that touch a render with geometry `geo`.
pub fn plan(spots: &[Spot], geo: &Geometry) -> Vec<SpotPlan> {
    let Some(inv) = geo.to_norm.inverse() else {
        return Vec::new();
    };
    let iso = geo.iso();
    let (w, h) = (f64::from(geo.out.width), f64::from(geo.out.height));
    let m = &inv.0;
    spots
        .iter()
        .filter(|s| {
            s.center.iter().chain(&s.source).all(|v| v.is_finite())
                && s.radius.is_finite()
                && s.radius > 0.0
                && s.opacity.is_finite()
                && s.opacity > 0.0
        })
        .filter_map(|s| {
            let src = [0, 1].map(|a| pin(s.source[a] * iso[a], s.radius, iso[a]) / iso[a]);
            let du = [0, 1].map(|a| f64::from(src[a]) - f64::from(s.center[a]));
            let d = [
                (m[0][0] * du[0] + m[0][1] * du[1]) as f32,
                (m[1][0] * du[0] + m[1][1] * du[1]) as f32,
            ];
            // The destination disc's bounding box in output pixels.
            let (rx, ry) = (
                f64::from(s.radius / iso[0].max(1e-6)),
                f64::from(s.radius / iso[1].max(1e-6)),
            );
            let (cu, cv) = (f64::from(s.center[0]), f64::from(s.center[1]));
            let corners = [(-rx, -ry), (rx, -ry), (-rx, ry), (rx, ry)]
                .map(|(a, b)| inv.apply(cu + a, cv + b));
            let (x0, x1) = corners
                .iter()
                .fold((f64::MAX, f64::MIN), |a, c| (a.0.min(c.0), a.1.max(c.0)));
            let (y0, y1) = corners
                .iter()
                .fold((f64::MAX, f64::MIN), |a, c| (a.0.min(c.1), a.1.max(c.1)));
            (x1 >= 0.0 && y1 >= 0.0 && x0 <= w && y0 <= h).then(|| SpotPlan {
                heal: s.kind == SpotKind::Heal,
                c: [s.center[0] * iso[0], s.center[1] * iso[1]],
                r: s.radius,
                feather: if s.feather.is_finite() {
                    s.feather.clamp(0.0, 1.0)
                } else {
                    0.0
                },
                opacity: s.opacity.clamp(0.0, 1.0),
                d,
                bbox: [
                    (x0 - 1.0).max(0.0) as f32,
                    (y0 - 1.0).max(0.0) as f32,
                    (x1 + 1.0).min(w) as f32,
                    (y1 + 1.0).min(h) as f32,
                ],
            })
        })
        .collect()
}

/// Bin `spots` into the [`BIN`]-pixel tiles of a `w × h` output.
pub fn bins(spots: &[SpotPlan], w: u32, h: u32) -> Bins {
    if spots.is_empty() {
        return Bins::default();
    }
    let (tx, ty) = (w.div_ceil(BIN).max(1), h.div_ceil(BIN).max(1));
    let tile = |v: f32, n: u32| ((v.max(0.0) as u32) / BIN).min(n - 1);
    let mut lists = vec![Vec::new(); (tx * ty) as usize];
    for (k, sp) in spots.iter().enumerate() {
        let [x0, y0, x1, y1] = sp.bbox;
        for y in tile(y0, ty)..=tile(y1, ty) {
            for x in tile(x0, tx)..=tile(x1, tx) {
                lists[(y * tx + x) as usize].push(k as u32);
            }
        }
    }
    let mut offsets = Vec::with_capacity(lists.len() + 1);
    offsets.push(0);
    let mut indices = Vec::new();
    for l in lists {
        indices.extend(l);
        offsets.push(indices.len() as u32);
    }
    Bins {
        tiles_x: tx,
        offsets,
        indices,
    }
}

/// Output-pixel positions of the ring samples of `sp` (`source`: shifted by `d`), in
/// the order the statistics read them: sector, then angle, then radius.
pub fn ring_points(sp: &SpotPlan, geo: &Geometry, source: bool) -> Vec<[f64; 2]> {
    let Some(inv) = geo.to_norm.inverse() else {
        return Vec::new();
    };
    let iso = geo.iso();
    let mut out = Vec::with_capacity(SECTORS * RING_ANGLES * RING.len());
    for j in 0..SECTORS {
        for a in 0..RING_ANGLES {
            let t = (j as f32 + (a as f32 + 0.5) / RING_ANGLES as f32) * std::f32::consts::TAU
                / SECTORS as f32;
            for rho in RING {
                let (s, c) = t.sin_cos();
                let u = (sp.c[0] + sp.r * rho * c) / iso[0];
                let v = (sp.c[1] + sp.r * rho * s) / iso[1];
                let (x, y) = inv.apply(f64::from(u), f64::from(v));
                out.push(if source {
                    [x + f64::from(sp.d[0]), y + f64::from(sp.d[1])]
                } else {
                    [x, y]
                });
            }
        }
    }
    out
}

/// Camera-space luminance proxy of the heal statistics.
#[inline]
pub fn ring_luma(v: [f32; 3]) -> f32 {
    0.25 * v[0] + 0.5 * v[1] + 0.25 * v[2]
}

/// Robust flat-field statistic per sector of one ring from its [`ring_points`] samples
/// (camera value, vignetting gain, on-image): the mean of the middle half (by
/// luminance) of the on-image samples, so a few bright or dark neighbours (a white
/// splash, a dark crack) don't pull it. `[3]` is 1 when the sector had on-image
/// samples, else 0.
pub fn sector_stats(samples: &[([f32; 3], f32, bool)]) -> [[f32; 4]; SECTORS] {
    let n = RING_ANGLES * RING.len();
    let mut out = [[0.0f32; 4]; SECTORS];
    for (j, o) in out.iter_mut().enumerate() {
        let mut v: Vec<[f32; 3]> = samples[j * n..(j + 1) * n]
            .iter()
            .filter(|s| s.2)
            .map(|(s, g, _)| s.map(|c| c * g))
            .collect();
        if v.is_empty() {
            continue;
        }
        // Stable insertion order by luminance (the GPU sorts the same way).
        v.sort_by(|a, b| ring_luma(*a).total_cmp(&ring_luma(*b)));
        let k = v.len() / 4;
        let kept = &v[k..v.len() - k];
        let mut acc = [0.0f32; 3];
        for s in kept {
            for c in 0..3 {
                acc[c] += s[c];
            }
        }
        let m = kept.len() as f32;
        *o = [acc[0] / m, acc[1] / m, acc[2] / m, 1.0];
    }
    out
}

/// Weight of the spot at isotropic position `p` (0 outside the disc).
#[inline]
pub fn weight(sp: &SpotPlan, p: [f32; 2]) -> (f32, [f32; 2], f32) {
    let q = [(p[0] - sp.c[0]) / sp.r, (p[1] - sp.c[1]) / sp.r];
    let rho = q[0].hypot(q[1]);
    if rho >= 1.0 {
        return (0.0, q, rho);
    }
    let edge = if sp.feather > 0.0 {
        1.0 - crate::tone::smoothstep(1.0 - sp.feather, 1.0, rho)
    } else {
        1.0
    };
    (sp.opacity * edge, q, rho)
}

/// Log2 heal gain of one destination/source statistic pair: the luminance ratio
/// (clamped to [`RATIO`]) plus a per-channel chroma term (clamped to [`CHROMA`]).
#[inline]
fn log_gain(d: [f32; 3], s: [f32; 3]) -> [f32; 3] {
    let (yd, ys) = (ring_luma(d).max(1e-6), ring_luma(s).max(1e-6));
    let l = (yd / ys).clamp(RATIO[0], RATIO[1]).log2();
    [0, 1, 2].map(|c| {
        let chroma = (d[c].max(1e-6) / yd) / (s[c].max(1e-6) / ys);
        l + chroma.clamp(CHROMA[0], CHROMA[1]).log2()
    })
}

/// Heal gain (per channel) at offset `q` / radius share `rho` inside the disc, from the
/// [`sector_stats`] of the destination and source rings. Only sectors on-image on both
/// rings count (the others take the whole-ring gain). Each gain is a bounded luminance
/// ratio times a bounded chroma correction ([`log_gain`]); the nearest sectors' gains
/// are interpolated in log space and eased by `rho²` toward the whole-ring gain at the
/// center. 1 when no sector is usable.
#[inline]
pub fn heal_gain(
    dst: &[[f32; 4]; SECTORS],
    src: &[[f32; 4]; SECTORS],
    q: [f32; 2],
    rho: f32,
) -> [f32; 3] {
    let usable = |j: usize| dst[j][3] > 0.0 && src[j][3] > 0.0;
    let rgb = |v: &[f32; 4]| [v[0], v[1], v[2]];
    let (mut ds, mut ss) = ([0.0f32; 3], [0.0f32; 3]);
    let mut n = 0;
    for j in (0..SECTORS).filter(|&j| usable(j)) {
        for c in 0..3 {
            ds[c] += dst[j][c];
            ss[c] += src[j][c];
        }
        n += 1;
    }
    if n == 0 {
        return [1.0; 3];
    }
    let avg = log_gain(ds, ss);
    let sector = |j: usize| {
        if usable(j) {
            log_gain(rgb(&dst[j]), rgb(&src[j]))
        } else {
            avg
        }
    };
    let t = q[1].atan2(q[0]).rem_euclid(std::f32::consts::TAU) / std::f32::consts::TAU
        * SECTORS as f32
        - 0.5;
    let t0 = t.floor();
    let f = t - t0;
    let j0 = (t0 as i32).rem_euclid(SECTORS as i32) as usize;
    let j1 = (j0 + 1) % SECTORS;
    let (a, b) = (sector(j0), sector(j1));
    let e = rho * rho;
    [0, 1, 2].map(|c| {
        let local = a[c] + (b[c] - a[c]) * f;
        (avg[c] + (local - avg[c]) * e).exp2()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(feather: f32) -> SpotPlan {
        SpotPlan {
            heal: true,
            c: [0.5, 0.5],
            r: 0.1,
            feather,
            opacity: 0.8,
            d: [3.0, 0.0],
            bbox: [0.0; 4],
        }
    }

    #[test]
    fn weight_is_the_feathered_disc() {
        let s = sp(0.5);
        assert_eq!(weight(&s, [0.5, 0.5]).0, 0.8);
        assert_eq!(weight(&s, [0.7, 0.5]).0, 0.0);
        let mid = weight(&s, [0.575, 0.5]).0;
        assert!(mid > 0.0 && mid < 0.8, "{mid}");
        assert_eq!(weight(&sp(0.0), [0.59, 0.5]).0, 0.8, "hard edge");
    }

    fn ring(v: [f32; 3]) -> [[f32; 4]; SECTORS] {
        [[v[0], v[1], v[2], 1.0]; SECTORS]
    }

    #[test]
    fn heal_gain_matches_the_rings() {
        let src = ring([0.1; 3]);
        let mut dst = ring([0.2; 3]);
        // Center: whole-ring ratio.
        let g = heal_gain(&dst, &src, [0.0, 0.0], 0.0);
        assert!((g[0] - 2.0).abs() < 1e-5, "{g:?}");
        // One brighter sector pulls the rim toward its own ratio...
        dst[0] = [0.4, 0.4, 0.4, 1.0];
        let t = (0.5f32 * std::f32::consts::TAU / SECTORS as f32).sin_cos();
        let edge = heal_gain(&dst, &src, [t.1, t.0], 1.0);
        assert!((edge[1] - 4.0).abs() < 1e-3, "{edge:?}");
        // ...but only a little halfway in (rho² easing, log-space).
        let mid = heal_gain(&dst, &src, [0.5 * t.1, 0.5 * t.0], 0.5);
        assert!(mid[1] > 2.25 && mid[1] < 2.65, "{mid:?}");
        // Extreme ratios are clamped.
        let g = heal_gain(&ring([1.0; 3]), &ring([0.0; 3]), [0.0, 0.0], 0.0);
        assert!((g[1] - 4.0).abs() < 1e-4, "{g:?}");
    }

    #[test]
    fn heal_chroma_is_bounded_and_off_image_sectors_are_ignored() {
        // A strongly blue destination against a neutral source: bounded cast.
        let g = heal_gain(&ring([0.1, 0.2, 0.6]), &ring([0.2; 3]), [0.0, 0.0], 0.0);
        let y = 0.275 / 0.2;
        assert!((g[2] / y - CHROMA[1]).abs() < 1e-4, "{g:?}");
        assert!((g[0] / y - CHROMA[0]).abs() < 1e-4, "{g:?}");
        // Off-image sectors (flag 0, black) don't darken the gain.
        let mut dst = ring([0.2; 3]);
        for s in dst.iter_mut().take(4) {
            *s = [0.0; 4];
        }
        let g = heal_gain(&dst, &ring([0.2; 3]), [0.0, 0.0], 0.0);
        assert!((g[1] - 1.0).abs() < 1e-5, "{g:?}");
        let t = (0.5f32 * std::f32::consts::TAU / SECTORS as f32).sin_cos();
        let g = heal_gain(&dst, &ring([0.2; 3]), [t.1, t.0], 1.0);
        assert!((g[1] - 1.0).abs() < 1e-5, "{g:?}");
        // Nothing usable: identity.
        assert_eq!(
            heal_gain(&[[0.0; 4]; SECTORS], &dst, [0.0, 0.0], 0.5),
            [1.0; 3]
        );
    }

    #[test]
    fn sector_stats_trim_outliers_and_skip_off_image_samples() {
        let n = RING_ANGLES * RING.len();
        let mut samples: Vec<([f32; 3], f32, bool)> = (0..SECTORS * n)
            .map(|i| ([(i / n) as f32; 3], 0.5, true))
            .collect();
        let m = sector_stats(&samples);
        assert_eq!(m[3], [1.5, 1.5, 1.5, 1.0]);
        // Two blown samples in sector 2 are trimmed away.
        samples[2 * n].0 = [100.0; 3];
        samples[2 * n + 5].0 = [100.0; 3];
        assert_eq!(sector_stats(&samples)[2], [1.0, 1.0, 1.0, 1.0]);
        // Off-image samples don't count; a sector without any is flagged.
        for s in &mut samples[5 * n..6 * n] {
            s.2 = false;
        }
        samples[4 * n].2 = false;
        samples[4 * n].0 = [0.0; 3];
        let m = sector_stats(&samples);
        assert_eq!(m[5], [0.0; 4]);
        assert_eq!(m[4], [2.0, 2.0, 2.0, 1.0]);
    }

    #[test]
    fn sources_are_pinned_inside_the_frame() {
        assert_eq!(pin(0.99, 0.04, 1.0), 0.96);
        assert_eq!(pin(-0.5, 0.04, 1.0), 0.04);
        assert_eq!(pin(0.5, 0.4, 0.66), 0.33);
    }

    #[test]
    fn bins_list_each_spot_in_order_in_the_tiles_it_touches() {
        let mut a = sp(0.0);
        a.bbox = [10.0, 10.0, 40.0, 20.0];
        let mut b = sp(0.0);
        b.bbox = [0.0, 0.0, 100.0, 100.0];
        let bins = bins(&[a, b], 64, 64);
        assert_eq!(bins.tiles_x, 2);
        assert_eq!(bins.at(5, 5), &[0, 1]);
        assert_eq!(bins.at(40, 5), &[0, 1]);
        assert_eq!(bins.at(5, 40), &[1]);
        assert_eq!(bins.at(63, 63), &[1]);
        assert_eq!(bins.at(64, 64), &[] as &[u32]);
        assert_eq!(super::bins(&[], 64, 64), Bins::default());
    }
}
