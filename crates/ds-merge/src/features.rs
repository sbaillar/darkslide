//! Feature points for registration: Harris corners spread over a grid, each described by
//! a bias/gain-normalized 8×8 patch (MOPS-style, without rotation: bracketed and panning
//! frames barely roll), matched by mutual nearest neighbour with a ratio test.

use rayon::prelude::*;

use crate::image::{Gray, blur};

/// Samples per descriptor side and their spacing (px of the analysis image).
const SIDE: usize = 8;
const SPACING: f32 = 2.5;
/// Keep corners this far from the edge so the whole patch is inside.
const MARGIN: usize = 12;
/// Most corners kept per grid cell.
const PER_CELL: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Feature {
    pub x: f32,
    pub y: f32,
    pub desc: [f32; SIDE * SIDE],
}

/// Up to `max` well-spread corners of `g` with their descriptors.
pub(crate) fn detect(g: &Gray, max: usize) -> Vec<Feature> {
    let (w, h) = (g.w, g.h);
    if w <= 2 * MARGIN + 2 || h <= 2 * MARGIN + 2 || max == 0 {
        return Vec::new();
    }
    let s = blur(g, 1.0);
    let mut gxx = Gray {
        w,
        h,
        px: vec![0.0; w * h],
    };
    let mut gyy = gxx.clone();
    let mut gxy = gxx.clone();
    gxx.px
        .par_chunks_mut(w)
        .zip(gyy.px.par_chunks_mut(w))
        .zip(gxy.px.par_chunks_mut(w))
        .enumerate()
        .for_each(|(y, ((xx, yy), xy))| {
            let (ya, yb) = (y.saturating_sub(1), (y + 1).min(h - 1));
            for x in 0..w {
                let (xa, xb) = (x.saturating_sub(1), (x + 1).min(w - 1));
                let ix = (s.at(xb, y) - s.at(xa, y)) * 0.5;
                let iy = (s.at(x, yb) - s.at(x, ya)) * 0.5;
                xx[x] = ix * ix;
                yy[x] = iy * iy;
                xy[x] = ix * iy;
            }
        });
    let (gxx, gyy, gxy) = (blur(&gxx, 1.5), blur(&gyy, 1.5), blur(&gxy, 1.5));
    let resp: Vec<f32> = (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (a, b, c) = (gxx.px[i], gyy.px[i], gxy.px[i]);
            a * b - c * c - 0.04 * (a + b) * (a + b)
        })
        .collect();
    let peak = resp.iter().copied().fold(0.0f32, f32::max);
    let floor = (peak * 1e-3).max(1e-12);

    // Grid cells sized so that PER_CELL corners per cell give about `max` in total.
    let cells = (max / PER_CELL).max(1);
    let cell = (((w * h) as f32 / cells as f32).sqrt().ceil() as usize).max(4);
    let (cw, ch) = (w.div_ceil(cell), h.div_ceil(cell));
    let mut buckets: Vec<Vec<(f32, usize, usize)>> = vec![Vec::new(); cw * ch];
    for y in MARGIN..h - MARGIN {
        for x in MARGIN..w - MARGIN {
            let r = resp[y * w + x];
            if r < floor {
                continue;
            }
            let is_max = (y - 1..=y + 1)
                .all(|yy| (x - 1..=x + 1).all(|xx| (xx == x && yy == y) || resp[yy * w + xx] < r));
            if is_max {
                buckets[(y / cell) * cw + x / cell].push((r, x, y));
            }
        }
    }
    let mut picked: Vec<(f32, usize, usize)> = Vec::new();
    for b in &mut buckets {
        b.sort_by(|p, q| q.0.total_cmp(&p.0));
        picked.extend(b.iter().take(PER_CELL));
    }
    picked.sort_by(|p, q| q.0.total_cmp(&p.0));
    picked.truncate(max);

    let d = blur(g, 2.0);
    // Sub-pixel peaks: a parabola through the response on each axis.
    let peak = |c: f32, a: f32, b: f32| {
        let den = a - 2.0 * c + b;
        if den < 0.0 {
            (0.5 * (a - b) / den).clamp(-0.5, 0.5)
        } else {
            0.0
        }
    };
    picked
        .par_iter()
        .filter_map(|&(r, x, y)| {
            let dx = peak(r, resp[y * w + x - 1], resp[y * w + x + 1]);
            let dy = peak(r, resp[(y - 1) * w + x], resp[(y + 1) * w + x]);
            describe(&d, x as f32 + dx, y as f32 + dy)
        })
        .collect()
}

/// Bias/gain-normalized patch around `(x, y)`; `None` for a flat patch.
fn describe(d: &Gray, x: f32, y: f32) -> Option<Feature> {
    let mut desc = [0.0f32; SIDE * SIDE];
    let half = (SIDE as f32 - 1.0) / 2.0;
    for (i, v) in desc.iter_mut().enumerate() {
        let (u, w) = ((i % SIDE) as f32 - half, (i / SIDE) as f32 - half);
        *v = d.sample(x + u * SPACING, y + w * SPACING);
    }
    let n = desc.len() as f32;
    let mean = desc.iter().sum::<f32>() / n;
    let var = desc.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / n;
    if var < 1e-10 {
        return None;
    }
    let inv = 1.0 / var.sqrt();
    for v in &mut desc {
        *v = (*v - mean) * inv;
    }
    Some(Feature { x, y, desc })
}

fn dist(a: &[f32; SIDE * SIDE], b: &[f32; SIDE * SIDE]) -> f32 {
    a.iter().zip(b).map(|(p, q)| (p - q) * (p - q)).sum()
}

/// Nearest neighbour of each of `a` in `b`, if it passes the ratio test.
fn nearest(a: &[Feature], b: &[Feature], ratio: f32) -> Vec<Option<usize>> {
    a.par_iter()
        .map(|f| {
            let (mut best, mut second, mut bi) = (f32::MAX, f32::MAX, 0);
            for (j, g) in b.iter().enumerate() {
                let d = dist(&f.desc, &g.desc);
                if d < best {
                    second = best;
                    best = d;
                    bi = j;
                } else if d < second {
                    second = d;
                }
            }
            (best < ratio * ratio * second).then_some(bi)
        })
        .collect()
}

/// Mutual nearest-neighbour matches `(index in a, index in b)` passing the ratio test.
pub(crate) fn match_features(a: &[Feature], b: &[Feature], ratio: f32) -> Vec<(usize, usize)> {
    if a.len() < 2 || b.len() < 2 {
        return Vec::new();
    }
    let ab = nearest(a, b, ratio);
    let ba = nearest(b, a, ratio);
    ab.iter()
        .enumerate()
        .filter_map(|(i, j)| j.filter(|&j| ba[j] == Some(i)).map(|j| (i, j)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::texture;

    fn image(w: usize, h: usize, dx: f32, dy: f32) -> Gray {
        let px = (0..w * h)
            .map(|i| texture((i % w) as f32 + dx, (i / w) as f32 + dy))
            .collect();
        Gray { w, h, px }
    }

    #[test]
    fn corners_are_found_spread_and_matched_under_a_shift() {
        let a = image(240, 180, 0.0, 0.0);
        let b = image(240, 180, 9.0, -4.0);
        let fa = detect(&a, 300);
        let fb = detect(&b, 300);
        assert!(fa.len() > 100, "{} corners", fa.len());
        assert!(fa.len() <= 300);
        // Spread: every quadrant has corners.
        for (qx, qy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            assert!(
                fa.iter()
                    .any(|f| (f.x >= 120.0) == (qx == 1) && (f.y >= 90.0) == (qy == 1))
            );
        }
        let m = match_features(&fa, &fb, 0.8);
        assert!(m.len() > 40, "{} matches", m.len());
        // b(x, y) = a(x + 9, y − 4): a point at (x, y) in a is at (x − 9, y + 4) in b.
        let good = m
            .iter()
            .filter(|&&(i, j)| {
                (fa[i].x - 9.0 - fb[j].x).abs() < 0.6 && (fa[i].y + 4.0 - fb[j].y).abs() < 0.6
            })
            .count();
        assert!(good * 10 >= m.len() * 9, "{good} of {} correct", m.len());
    }

    #[test]
    fn degenerate_inputs_give_nothing() {
        let flat = Gray {
            w: 100,
            h: 100,
            px: vec![0.5; 10_000],
        };
        assert_eq!(detect(&flat, 100).len(), 0);
        assert_eq!(detect(&image(20, 20, 0.0, 0.0), 100).len(), 0);
        assert_eq!(detect(&image(100, 100, 0.0, 0.0), 0).len(), 0);
        let f = detect(&image(100, 100, 0.0, 0.0), 50);
        assert_eq!(match_features(&f, &f[..1], 0.8).len(), 0);
        assert!(describe(&flat, 50.0, 50.0).is_none());
    }
}
