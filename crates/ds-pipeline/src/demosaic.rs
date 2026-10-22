//! BASE stage: black/white normalization → reference WB → demosaic, producing
//! white-balanced **camera RGB** (balanced with a fixed reference, the as-shot
//! multipliers, which guide the demosaic). The user's WB, highlight reconstruction and
//! the camera matrix are applied per pixel in DEVELOP, so a WB change never rebuilds this.
//!
//! Two demosaics: a 2×2 superpixel (half size, used for previews at scale ≤ 0.5) and
//! RCD (Ratio Corrected Demosaicing, L. Sanz Rodríguez) for everything larger. RCD runs
//! on overlapping tiles with mirrored (CFA-phase preserving) borders, so every pixel,
//! including the image edge, gets the full algorithm.

use ds_core::{CfaPattern, RawImage};
use rayon::prelude::*;

use crate::tone::smoothstep;

/// Camera RGB balanced by the reference multipliers, interleaved, at `scale` × sensor
/// resolution. May cover only a sub-rectangle of the level's grid (1:1 region renders):
/// pixel `(x, y)` here is level pixel `(x0 + x, y0 + y)`.
#[derive(Debug, Clone, PartialEq)]
pub struct BaseImage {
    pub w: u32,
    pub h: u32,
    /// Base pixels per sensor pixel (1, 0.5, 0.25, …).
    pub scale: f32,
    pub x0: u32,
    pub y0: u32,
    pub px: Vec<[f32; 3]>,
}

impl BaseImage {
    pub fn bytes(&self) -> usize {
        self.px.len() * 12
    }
}

/// Everything the BASE stage needs besides the raw samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BaseParams {
    /// Reference white balance multipliers (G = 1), normally the as-shot setting.
    pub mults: [f32; 3],
}

/// Per-2×2-position sample info: CFA color, black level, 1/(white − black).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SiteInfo {
    pub color: [usize; 4],
    pub black: [f32; 4],
    pub inv_range: [f32; 4],
}

pub(crate) fn site_info(raw: &RawImage) -> SiteInfo {
    let mut color = [1usize; 4];
    let mut black = [0.0f32; 4];
    let mut inv_range = [1.0f32; 4];
    for pos in 0..4u32 {
        let (x, y) = (pos & 1, pos >> 1);
        let c = raw.cfa.color_at(x, y).unwrap_or(1);
        let bidx = match c {
            0 => 0,
            2 => 2,
            // Green on a red row is G, on a blue row G2.
            _ => {
                if raw.cfa.color_at(x ^ 1, y) == Some(0) {
                    1
                } else {
                    3
                }
            }
        };
        let b = raw.black_level[bidx];
        color[pos as usize] = c as usize;
        black[pos as usize] = b;
        inv_range[pos as usize] = 1.0 / (raw.white_level - b).max(1.0);
    }
    SiteInfo {
        color,
        black,
        inv_range,
    }
}

/// Reconstruct clipped channels (camera WB space; `clip[c]` is where channel c saturates).
///
/// A clipped channel is raised toward the mean of the other two (never lowered), blended by
/// how clipped it is; when all three clip, the pixel blends to neutral at its max. This
/// avoids magenta/pink blown highlights and keeps their brightness for the tone shoulder.
#[inline]
pub fn reconstruct_highlights(v: [f32; 3], clip: [f32; 3]) -> [f32; 3] {
    let t = [0, 1, 2].map(|c| smoothstep(0.95 * clip[c], 0.995 * clip[c], v[c]));
    if t[0] + t[1] + t[2] <= 0.0 {
        return v;
    }
    let sum = v[0] + v[1] + v[2];
    let mut o = v;
    for c in 0..3 {
        if t[c] > 0.0 {
            let est = v[c].max(0.5 * (sum - v[c]));
            o[c] = v[c] + (est - v[c]) * t[c];
        }
    }
    let all = t[0].min(t[1]).min(t[2]);
    if all > 0.0 {
        let mx = o[0].max(o[1]).max(o[2]);
        for c in &mut o {
            *c += (mx - *c) * all;
        }
    }
    o
}

/// Half-size 2×2 superpixel demosaic (R, mean of both greens, B per block).
pub fn superpixel(raw: &RawImage, p: &BaseParams) -> BaseImage {
    let (sw, sh) = (raw.size.width as usize, raw.size.height as usize);
    let (w, h) = (sw / 2, sh / 2);
    let si = site_info(raw);
    let mut px = vec![[0.0f32; 3]; w * h];
    if raw.cfa == CfaPattern::LinearRgb {
        px.par_chunks_mut(w.max(1))
            .enumerate()
            .for_each(|(y, row)| {
                for (x, out) in row.iter_mut().enumerate() {
                    let mut acc = [0.0f32; 3];
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let i = ((2 * y + dy) * sw + 2 * x + dx) * 3;
                        for c in 0..3 {
                            acc[c] += linear_sample(raw, i + c, c);
                        }
                    }
                    *out = acc.map(|v| v * 0.25).map(|v| v.max(0.0)).zip3(p.mults);
                }
            });
    } else {
        px.par_chunks_mut(w.max(1))
            .enumerate()
            .for_each(|(y, row)| {
                let r0 = &raw.data[2 * y * sw..(2 * y + 1) * sw];
                let r1 = &raw.data[(2 * y + 1) * sw..(2 * y + 2) * sw];
                for (x, out) in row.iter_mut().enumerate() {
                    let s = [r0[2 * x], r0[2 * x + 1], r1[2 * x], r1[2 * x + 1]];
                    let mut acc = [0.0f32; 3];
                    let mut n = [0.0f32; 3];
                    for pos in 0..4 {
                        let c = si.color[pos];
                        let v = ((f32::from(s[pos]) - si.black[pos]) * si.inv_range[pos])
                            .clamp(0.0, 1.0);
                        acc[c] += v;
                        n[c] += 1.0;
                    }
                    *out = [0, 1, 2].map(|c| {
                        if n[c] > 0.0 {
                            acc[c] / n[c] * p.mults[c]
                        } else {
                            0.0
                        }
                    });
                }
            });
    }
    BaseImage {
        w: w as u32,
        h: h as u32,
        scale: 0.5,
        x0: 0,
        y0: 0,
        px,
    }
}

trait Zip3 {
    fn zip3(self, m: [f32; 3]) -> [f32; 3];
}
impl Zip3 for [f32; 3] {
    fn zip3(self, m: [f32; 3]) -> [f32; 3] {
        [self[0] * m[0], self[1] * m[1], self[2] * m[2]]
    }
}

#[inline]
fn linear_sample(raw: &RawImage, i: usize, c: usize) -> f32 {
    let b = raw.black_level[c];
    ((f32::from(raw.data[i]) - b) / (raw.white_level - b).max(1.0)).clamp(0.0, 1.0)
}

/// 2× box downsample (drops a trailing odd row/column). Only for full-grid bases.
pub fn downsample2(src: &BaseImage) -> BaseImage {
    let (w, h) = ((src.w / 2).max(1) as usize, (src.h / 2).max(1) as usize);
    let sw = src.w as usize;
    let (mx, my) = (src.w as usize - 1, src.h as usize - 1);
    let mut px = vec![[0.0f32; 3]; w * h];
    px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, out) in row.iter_mut().enumerate() {
            let (x0, y0) = ((2 * x).min(mx), (2 * y).min(my));
            let (x1, y1) = ((2 * x + 1).min(mx), (2 * y + 1).min(my));
            let a = src.px[y0 * sw + x0];
            let b = src.px[y0 * sw + x1];
            let c = src.px[y1 * sw + x0];
            let d = src.px[y1 * sw + x1];
            *out = [0, 1, 2].map(|k| 0.25 * (a[k] + b[k] + c[k] + d[k]));
        }
    });
    BaseImage {
        w: w as u32,
        h: h as u32,
        scale: src.scale * 0.5,
        x0: 0,
        y0: 0,
        px,
    }
}

// --------------------------------------------------------------------- RCD ---

const BAND: usize = 128;
const TILE_W: usize = 512;
const BORDER: usize = 12;

/// Mirror `i` into `0..n` without repeating the edge, repeatedly (a triangle wave of
/// period 2(n-1)), so even sensors narrower than the tile border get a true mirror
/// extension. The period is even, so CFA parity is preserved.
#[inline]
fn reflect(i: isize, n: isize) -> usize {
    if n <= 1 {
        return 0;
    }
    let r = i.rem_euclid(2 * (n - 1));
    (if r >= n { 2 * (n - 1) - r } else { r }) as usize
}

/// Full-resolution RCD demosaic of the whole sensor.
pub fn rcd(raw: &RawImage, p: &BaseParams) -> BaseImage {
    rcd_rect(raw, p, [0, 0, raw.size.width, raw.size.height])
}

/// RCD demosaic of the sensor sub-rectangle `rect` = (x, y, w, h). Neighbours outside the
/// rectangle come from the real sensor data (mirrored only at the sensor edge), so the
/// result equals the same pixels of a full-frame demosaic.
pub fn rcd_rect(raw: &RawImage, p: &BaseParams, rect: [u32; 4]) -> BaseImage {
    let (sw, sh) = (raw.size.width, raw.size.height);
    let rx = rect[0].min(sw - 1);
    let ry = rect[1].min(sh - 1);
    let (w, h) = (
        rect[2].clamp(1, sw - rx) as usize,
        rect[3].clamp(1, sh - ry) as usize,
    );
    let (rx, ry) = (rx as usize, ry as usize);
    let mut px = vec![[0.0f32; 3]; w * h];
    if raw.cfa == CfaPattern::LinearRgb {
        let fw = sw as usize;
        px.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
            for (x, out) in row.iter_mut().enumerate() {
                let i = ((ry + y) * fw + rx + x) * 3;
                *out = [0, 1, 2].map(|c| linear_sample(raw, i + c, c) * p.mults[c]);
            }
        });
    } else {
        let si = site_info(raw);
        px.par_chunks_mut(BAND * w)
            .enumerate()
            .for_each(|(band, out)| {
                let y0 = ry + band * BAND;
                let bh = out.len() / w;
                let mut t = RcdTile::new(TILE_W + 2 * BORDER, bh + 2 * BORDER);
                let mut xo = 0;
                while xo < w {
                    let tw = TILE_W.min(w - xo);
                    t.resize(tw + 2 * BORDER, bh + 2 * BORDER);
                    t.load(raw, &si, p.mults, rx + xo, y0);
                    t.run();
                    for y in 0..bh {
                        for x in 0..tw {
                            let i = (y + BORDER) * t.w + x + BORDER;
                            out[y * w + xo + x] = [
                                t.rgb[0][i].max(0.0),
                                t.rgb[1][i].max(0.0),
                                t.rgb[2][i].max(0.0),
                            ];
                        }
                    }
                    xo += tw;
                }
            });
    }
    BaseImage {
        w: w as u32,
        h: h as u32,
        scale: 1.0,
        x0: rx as u32,
        y0: ry as u32,
        px,
    }
}

struct RcdTile {
    w: usize,
    h: usize,
    cfa: Vec<f32>,
    col: Vec<u8>,
    rgb: [Vec<f32>; 3],
    buf_a: Vec<f32>,
    buf_b: Vec<f32>,
    vh: Vec<f32>,
    pq: Vec<f32>,
    lpf: Vec<f32>,
}

#[inline]
fn sqr(x: f32) -> f32 {
    x * x
}

impl RcdTile {
    fn new(w: usize, h: usize) -> Self {
        let n = w * h;
        Self {
            w,
            h,
            cfa: vec![0.0; n],
            col: vec![0; n],
            rgb: [vec![0.0; n], vec![0.0; n], vec![0.0; n]],
            buf_a: vec![0.0; n],
            buf_b: vec![0.0; n],
            vh: vec![0.0; n],
            pq: vec![0.0; n],
            lpf: vec![0.0; n],
        }
    }

    fn resize(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        let n = w * h;
        for v in [
            &mut self.cfa,
            &mut self.buf_a,
            &mut self.buf_b,
            &mut self.vh,
            &mut self.pq,
            &mut self.lpf,
        ] {
            v.resize(n, 0.0);
        }
        for v in &mut self.rgb {
            v.resize(n, 0.0);
        }
        self.col.resize(n, 0);
    }

    fn load(&mut self, raw: &RawImage, si: &SiteInfo, mults: [f32; 3], x0: usize, y0: usize) {
        let (sw, sh) = (raw.size.width as isize, raw.size.height as isize);
        for y in 0..self.h {
            let gy = reflect(y0 as isize + y as isize - BORDER as isize, sh);
            let row = &raw.data[gy * sw as usize..(gy + 1) * sw as usize];
            for x in 0..self.w {
                let gx = reflect(x0 as isize + x as isize - BORDER as isize, sw);
                // Mirroring preserves CFA parity, so the source pixel's site is the site.
                let pos = (gy & 1) << 1 | (gx & 1);
                let c = si.color[pos];
                let v = ((f32::from(row[gx]) - si.black[pos]) * si.inv_range[pos]).clamp(0.0, 1.0)
                    * mults[c];
                let i = y * self.w + x;
                self.cfa[i] = v;
                self.col[i] = c as u8;
                self.rgb[0][i] = 0.0;
                self.rgb[1][i] = 0.0;
                self.rgb[2][i] = 0.0;
                self.rgb[c][i] = v;
            }
        }
    }

    fn run(&mut self) {
        const EPS: f32 = 1e-5;
        const EPSSQ: f32 = 1e-10;
        let (w, h) = (self.w, self.h);
        let (w1, w2, w3, w4) = (w, 2 * w, 3 * w, 4 * w);
        let cfa = &self.cfa;
        // Step 1.1: vertical / horizontal high-pass energy.
        for y in 3..h - 3 {
            for x in 3..w - 3 {
                let i = y * w + x;
                self.buf_a[i] = sqr((cfa[i - w3] - cfa[i - w1] - cfa[i + w1] + cfa[i + w3])
                    - 3.0 * (cfa[i - w2] + cfa[i + w2])
                    + 6.0 * cfa[i]);
                self.buf_b[i] = sqr((cfa[i - 3] - cfa[i - 1] - cfa[i + 1] + cfa[i + 3])
                    - 3.0 * (cfa[i - 2] + cfa[i + 2])
                    + 6.0 * cfa[i]);
            }
        }
        // Step 1.2: V/H discrimination.
        for y in 4..h - 4 {
            for x in 4..w - 4 {
                let i = y * w + x;
                let v = (self.buf_a[i - w1] + self.buf_a[i] + self.buf_a[i + w1]).max(EPSSQ);
                let hs = (self.buf_b[i - 1] + self.buf_b[i] + self.buf_b[i + 1]).max(EPSSQ);
                self.vh[i] = v / (v + hs);
            }
        }
        // Step 2: low-pass at R/B sites.
        for y in 2..h - 2 {
            for x in 2..w - 2 {
                let i = y * w + x;
                if self.col[i] != 1 {
                    self.lpf[i] = cfa[i]
                        + 0.5 * (cfa[i - w1] + cfa[i + w1] + cfa[i - 1] + cfa[i + 1])
                        + 0.25
                            * (cfa[i - w1 - 1]
                                + cfa[i - w1 + 1]
                                + cfa[i + w1 - 1]
                                + cfa[i + w1 + 1]);
                }
            }
        }
        // Step 3: green at R/B.
        for y in 5..h - 5 {
            for x in 5..w - 5 {
                let i = y * w + x;
                if self.col[i] == 1 {
                    continue;
                }
                let n_grad = EPS
                    + (cfa[i - w1] - cfa[i + w1]).abs()
                    + (cfa[i] - cfa[i - w2]).abs()
                    + (cfa[i - w1] - cfa[i - w3]).abs()
                    + (cfa[i - w2] - cfa[i - w4]).abs();
                let s_grad = EPS
                    + (cfa[i - w1] - cfa[i + w1]).abs()
                    + (cfa[i] - cfa[i + w2]).abs()
                    + (cfa[i + w1] - cfa[i + w3]).abs()
                    + (cfa[i + w2] - cfa[i + w4]).abs();
                let w_grad = EPS
                    + (cfa[i - 1] - cfa[i + 1]).abs()
                    + (cfa[i] - cfa[i - 2]).abs()
                    + (cfa[i - 1] - cfa[i - 3]).abs()
                    + (cfa[i - 2] - cfa[i - 4]).abs();
                let e_grad = EPS
                    + (cfa[i - 1] - cfa[i + 1]).abs()
                    + (cfa[i] - cfa[i + 2]).abs()
                    + (cfa[i + 1] - cfa[i + 3]).abs()
                    + (cfa[i + 2] - cfa[i + 4]).abs();
                let lpfi = self.lpf[i];
                let n_est = cfa[i - w1] * (lpfi + lpfi) / (EPS + lpfi + self.lpf[i - w2]);
                let s_est = cfa[i + w1] * (lpfi + lpfi) / (EPS + lpfi + self.lpf[i + w2]);
                let w_est = cfa[i - 1] * (lpfi + lpfi) / (EPS + lpfi + self.lpf[i - 2]);
                let e_est = cfa[i + 1] * (lpfi + lpfi) / (EPS + lpfi + self.lpf[i + 2]);
                let v_est = (s_grad * n_est + n_grad * s_est) / (n_grad + s_grad);
                let h_est = (w_grad * e_est + e_grad * w_est) / (e_grad + w_grad);
                let c = self.vh[i];
                let nb = 0.25
                    * (self.vh[i - w1 - 1]
                        + self.vh[i - w1 + 1]
                        + self.vh[i + w1 - 1]
                        + self.vh[i + w1 + 1]);
                let disc = if (0.5 - c).abs() < (0.5 - nb).abs() {
                    nb
                } else {
                    c
                };
                self.rgb[1][i] = disc * (h_est - v_est) + v_est;
            }
        }
        // Step 4.0: diagonal high-pass energy.
        for y in 3..h - 3 {
            for x in 3..w - 3 {
                let i = y * w + x;
                self.buf_a[i] = sqr((cfa[i - w3 - 3] - cfa[i - w1 - 1] - cfa[i + w1 + 1]
                    + cfa[i + w3 + 3])
                    - 3.0 * (cfa[i - w2 - 2] + cfa[i + w2 + 2])
                    + 6.0 * cfa[i]);
                self.buf_b[i] = sqr((cfa[i - w3 + 3] - cfa[i - w1 + 1] - cfa[i + w1 - 1]
                    + cfa[i + w3 - 3])
                    - 3.0 * (cfa[i - w2 + 2] + cfa[i + w2 - 2])
                    + 6.0 * cfa[i]);
            }
        }
        // Step 4.1: P/Q discrimination.
        for y in 4..h - 4 {
            for x in 4..w - 4 {
                let i = y * w + x;
                let p =
                    (self.buf_a[i - w1 - 1] + self.buf_a[i] + self.buf_a[i + w1 + 1]).max(EPSSQ);
                let q =
                    (self.buf_b[i - w1 + 1] + self.buf_b[i] + self.buf_b[i + w1 - 1]).max(EPSSQ);
                self.pq[i] = p / (p + q);
            }
        }
        // Step 4.2: R at B and B at R.
        for y in 7..h - 7 {
            for x in 7..w - 7 {
                let i = y * w + x;
                let own = self.col[i] as usize;
                if own == 1 {
                    continue;
                }
                let c = 2 - own;
                let pc = self.pq[i];
                let nb = 0.25
                    * (self.pq[i - w1 - 1]
                        + self.pq[i - w1 + 1]
                        + self.pq[i + w1 - 1]
                        + self.pq[i + w1 + 1]);
                let disc = if (0.5 - pc).abs() < (0.5 - nb).abs() {
                    nb
                } else {
                    pc
                };
                let (rc, g) = (&self.rgb[c], &self.rgb[1]);
                let nw = EPS
                    + (rc[i - w1 - 1] - rc[i + w1 + 1]).abs()
                    + (rc[i - w1 - 1] - rc[i - w3 - 3]).abs()
                    + (g[i] - g[i - w2 - 2]).abs();
                let ne = EPS
                    + (rc[i - w1 + 1] - rc[i + w1 - 1]).abs()
                    + (rc[i - w1 + 1] - rc[i - w3 + 3]).abs()
                    + (g[i] - g[i - w2 + 2]).abs();
                let sw = EPS
                    + (rc[i - w1 + 1] - rc[i + w1 - 1]).abs()
                    + (rc[i + w1 - 1] - rc[i + w3 - 3]).abs()
                    + (g[i] - g[i + w2 - 2]).abs();
                let se = EPS
                    + (rc[i - w1 - 1] - rc[i + w1 + 1]).abs()
                    + (rc[i + w1 + 1] - rc[i + w3 + 3]).abs()
                    + (g[i] - g[i + w2 + 2]).abs();
                let nw_e = rc[i - w1 - 1] - g[i - w1 - 1];
                let ne_e = rc[i - w1 + 1] - g[i - w1 + 1];
                let sw_e = rc[i + w1 - 1] - g[i + w1 - 1];
                let se_e = rc[i + w1 + 1] - g[i + w1 + 1];
                let p_est = (nw * se_e + se * nw_e) / (nw + se);
                let q_est = (ne * sw_e + sw * ne_e) / (ne + sw);
                let v = g[i] + disc * (q_est - p_est) + p_est;
                self.rgb[c][i] = v;
            }
        }
        // Step 4.3: R and B at G.
        for y in 10..h - 10 {
            for x in 10..w - 10 {
                let i = y * w + x;
                if self.col[i] != 1 {
                    continue;
                }
                let c0 = self.vh[i];
                let nb = 0.25
                    * (self.vh[i - w1 - 1]
                        + self.vh[i - w1 + 1]
                        + self.vh[i + w1 - 1]
                        + self.vh[i + w1 + 1]);
                let disc = if (0.5 - c0).abs() < (0.5 - nb).abs() {
                    nb
                } else {
                    c0
                };
                let g = &self.rgb[1];
                let g0 = g[i];
                let n1 = EPS + (g0 - g[i - w2]).abs();
                let s1 = EPS + (g0 - g[i + w2]).abs();
                let w1g = EPS + (g0 - g[i - 2]).abs();
                let e1 = EPS + (g0 - g[i + 2]).abs();
                let (gn, gs, gw, ge) = (g[i - w1], g[i + w1], g[i - 1], g[i + 1]);
                for c in [0usize, 2] {
                    let r = &self.rgb[c];
                    let sn = (r[i - w1] - r[i + w1]).abs();
                    let ew = (r[i - 1] - r[i + 1]).abs();
                    let n_grad = n1 + sn + (r[i - w1] - r[i - w3]).abs();
                    let s_grad = s1 + sn + (r[i + w1] - r[i + w3]).abs();
                    let w_grad = w1g + ew + (r[i - 1] - r[i - 3]).abs();
                    let e_grad = e1 + ew + (r[i + 1] - r[i + 3]).abs();
                    let n_est = r[i - w1] - gn;
                    let s_est = r[i + w1] - gs;
                    let w_est = r[i - 1] - gw;
                    let e_est = r[i + 1] - ge;
                    let v_est = (n_grad * s_est + s_grad * n_est) / (n_grad + s_grad);
                    let h_est = (e_grad * w_est + w_grad * e_est) / (e_grad + w_grad);
                    let v = g0 + disc * (h_est - v_est) + v_est;
                    self.rgb[c][i] = v;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_core::{Orientation, Size};

    pub(crate) fn raw_from_fn(
        w: u32,
        h: u32,
        cfa: CfaPattern,
        f: impl Fn(u32, u32) -> [f32; 3],
    ) -> RawImage {
        let mut data = vec![0u16; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let c = cfa.color_at(x, y).unwrap() as usize;
                let v = f(x, y)[c].clamp(0.0, 1.0);
                data[(y * w + x) as usize] = (64.0 + v * (4095.0 - 64.0)).round() as u16;
            }
        }
        RawImage {
            size: Size::new(w, h),
            cfa,
            data,
            black_level: [64.0; 4],
            white_level: 4095.0,
            wb_as_shot: [1.0, 1.0, 1.0],
            cam_to_xyz: crate::color::REC2020_TO_XYZ,
            wb_daylight: [1.0; 3],
            orientation: Orientation::Normal,
            default_crop: None,
        }
    }

    fn ident() -> BaseParams {
        BaseParams { mults: [1.0; 3] }
    }

    #[test]
    fn site_info_for_all_patterns() {
        for (cfa, colors) in [
            (CfaPattern::Rggb, [0, 1, 1, 2]),
            (CfaPattern::Bggr, [2, 1, 1, 0]),
            (CfaPattern::Grbg, [1, 0, 2, 1]),
            (CfaPattern::Gbrg, [1, 2, 0, 1]),
        ] {
            let mut raw = raw_from_fn(4, 4, cfa, |_, _| [0.5; 3]);
            raw.black_level = [10.0, 20.0, 30.0, 40.0];
            let si = site_info(&raw);
            assert_eq!(si.color, colors);
            for pos in 0..4 {
                let want = match colors[pos] {
                    0 => 10.0,
                    2 => 30.0,
                    _ => {
                        // G on the red row → G (20), on the blue row → G2 (40).
                        let row = pos >> 1;
                        if colors[row * 2] == 0 || colors[row * 2 + 1] == 0 {
                            20.0
                        } else {
                            40.0
                        }
                    }
                };
                assert_eq!(si.black[pos], want, "{cfa:?} pos {pos}");
            }
        }
    }

    #[test]
    fn superpixel_flat_gray_is_exact() {
        for cfa in [
            CfaPattern::Rggb,
            CfaPattern::Bggr,
            CfaPattern::Grbg,
            CfaPattern::Gbrg,
        ] {
            let raw = raw_from_fn(8, 6, cfa, |_, _| [0.2, 0.4, 0.6]);
            let b = superpixel(&raw, &ident());
            assert_eq!((b.w, b.h, b.scale), (4, 3, 0.5));
            for p in &b.px {
                assert!(
                    (p[0] - 0.2).abs() < 2e-3
                        && (p[1] - 0.4).abs() < 2e-3
                        && (p[2] - 0.6).abs() < 2e-3,
                    "{p:?}"
                );
            }
        }
    }

    #[test]
    fn rcd_reproduces_flat_and_smooth_fields() {
        let raw = raw_from_fn(64, 48, CfaPattern::Rggb, |x, y| {
            let g = 0.2 + 0.4 * (x as f32 / 64.0) + 0.1 * (y as f32 / 48.0);
            [g * 0.8, g, g * 0.6]
        });
        let b = rcd(&raw, &ident());
        assert_eq!((b.w, b.h, b.scale), (64, 48, 1.0));
        let mut max_err = 0.0f32;
        for y in 0..48u32 {
            for x in 0..64u32 {
                let g = 0.2 + 0.4 * (x as f32 / 64.0) + 0.1 * (y as f32 / 48.0);
                let want = [g * 0.8, g, g * 0.6];
                let got = b.px[(y * 64 + x) as usize];
                for c in 0..3 {
                    max_err = max_err.max((got[c] - want[c]).abs());
                }
            }
        }
        assert!(max_err < 0.01, "max err {max_err}");
    }

    #[test]
    fn rcd_resolves_edges_without_zipper() {
        // Vertical stripes (top half) and horizontal stripes (bottom half) in a neutral
        // scene: direction discrimination must keep them free of false color.
        let raw = raw_from_fn(96, 80, CfaPattern::Grbg, |x, y| {
            let v = if y < 40 { (x / 7) % 2 } else { (y / 5) % 2 };
            [if v == 0 { 0.15 } else { 0.7 }; 3]
        });
        let b = rcd(&raw, &ident());
        let mut worst = 0.0f32;
        for (i, p) in b.px.iter().enumerate() {
            if (36..44).contains(&(i / 96)) {
                continue; // the seam between the two halves has corners
            }
            let chroma = (p[0] - p[1]).abs().max((p[2] - p[1]).abs());
            worst = worst.max(chroma);
        }
        assert!(worst < 0.01, "false color {worst}");
        // Bands and tiles: an image larger than one tile/band is seamless.
        let big = raw_from_fn(600, 300, CfaPattern::Rggb, |_, _| [0.3, 0.5, 0.4]);
        let bb = rcd(&big, &ident());
        assert!(
            bb.px
                .iter()
                .all(|p| (p[1] - 0.5).abs() < 2e-3 && (p[0] - 0.3).abs() < 2e-3)
        );
    }

    #[test]
    fn rcd_rect_equals_the_same_pixels_of_a_full_demosaic() {
        let raw = raw_from_fn(700, 300, CfaPattern::Rggb, |x, y| {
            let v = if (x / 9 + y / 7) % 2 == 0 { 0.1 } else { 0.6 };
            [v, v * 0.8 + 0.001 * x as f32, v * 0.5]
        });
        let full = rcd(&raw, &ident());
        for rect in [
            [3u32, 5u32, 100u32, 60u32],
            [513, 130, 187, 170],
            [0, 0, 700, 300],
        ] {
            let part = rcd_rect(&raw, &ident(), rect);
            assert_eq!(
                (part.x0, part.y0, part.w, part.h),
                (rect[0], rect[1], rect[2], rect[3])
            );
            for y in 0..rect[3] {
                for x in 0..rect[2] {
                    let a = part.px[(y * rect[2] + x) as usize];
                    let b = full.px[((rect[1] + y) * 700 + rect[0] + x) as usize];
                    assert_eq!(a, b, "{rect:?} at {x},{y}");
                }
            }
        }
        // Out-of-range rectangles are clamped to the sensor.
        let c = rcd_rect(&raw, &ident(), [690, 290, 50, 50]);
        assert_eq!((c.w, c.h), (10, 10));
    }

    #[test]
    fn linear_rgb_input_paths() {
        let mut raw = raw_from_fn(4, 4, CfaPattern::Rggb, |_, _| [0.0; 3]);
        raw.cfa = CfaPattern::LinearRgb;
        raw.data = (0..48).map(|i| [1000u16, 2000, 3000][i % 3]).collect();
        let b = superpixel(&raw, &ident());
        assert_eq!((b.w, b.h), (2, 2));
        let want = [
            (1000.0 - 64.0) / 4031.0,
            (2000.0 - 64.0) / 4031.0,
            (3000.0 - 64.0) / 4031.0,
        ];
        for c in 0..3 {
            assert!((b.px[0][c] - want[c]).abs() < 1e-4);
        }
        let f = rcd(&raw, &ident());
        assert_eq!((f.w, f.h), (4, 4));
        assert!((f.px[5][1] - want[1]).abs() < 1e-4);
    }

    #[test]
    fn highlight_reconstruction() {
        let clip = [2.0, 1.0, 1.5];
        // Unclipped: untouched.
        assert_eq!(
            reconstruct_highlights([0.5, 0.4, 0.3], clip),
            [0.5, 0.4, 0.3]
        );
        // Green clipped only, R/B high: green raised toward them (no magenta).
        let o = reconstruct_highlights([1.8, 1.0, 1.4], clip);
        assert!(o[1] > 1.5 && o[0] == 1.8 && o[2] == 1.4, "{o:?}");
        // All clipped: neutral at the max.
        let o = reconstruct_highlights([2.0, 1.0, 1.5], clip);
        assert!(
            (o[0] - o[1]).abs() < 1e-6 && (o[1] - o[2]).abs() < 1e-6 && o[0] == 2.0,
            "{o:?}"
        );
        // Clipped channel with low neighbours is never lowered.
        let o = reconstruct_highlights([0.1, 1.0, 0.1], clip);
        assert_eq!(o, [0.1, 1.0, 0.1]);
    }

    #[test]
    fn downsample_and_reflect() {
        let b = BaseImage {
            w: 5,
            h: 3,
            scale: 1.0,
            x0: 0,
            y0: 0,
            px: (0..15).map(|i| [i as f32; 3]).collect(),
        };
        let d = downsample2(&b);
        assert_eq!((d.w, d.h, d.scale), (2, 1, 0.5));
        assert_eq!(d.px[0][0], (0.0 + 1.0 + 5.0 + 6.0) / 4.0);
        assert_eq!(b.bytes(), 180);
        let one = BaseImage {
            w: 1,
            h: 1,
            scale: 1.0,
            x0: 0,
            y0: 0,
            px: vec![[2.0; 3]],
        };
        assert_eq!(downsample2(&one).px, vec![[2.0; 3]]);
        assert_eq!(reflect(-2, 10), 2);
        assert_eq!(reflect(10, 10), 8);
        assert_eq!(reflect(4, 10), 4);
    }
}
