//! The single fused gradient pass everything else is derived from.
//!
//! One walk over the interior pixels computes, per tile: the 4-neighbour
//! Laplacian (sum and sum of squares), the Sobel structure tensor
//! (`Σgx²`, `Σgy²`, `Σgx·gy`), the luma sum and sum of squares (local
//! contrast), and the clipped-pixel count.
//! All per-pixel math is `i32` on 8-bit values so LLVM vectorizes the inner
//! loop; tile widths are capped so per-row `i32` sums cannot overflow.

/// Per-tile sums over interior pixels (the 1-pixel border has no stencil).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TileStats {
    pub n: i64,
    pub lap: i64,
    pub lap2: i64,
    pub gxx: i64,
    pub gyy: i64,
    pub gxy: i64,
    pub luma: i64,
    pub luma2: i64,
    /// Pixels at 0 or 255 (either end): such tiles say nothing about noise.
    pub clipped: i64,
}

impl TileStats {
    /// Mean squared Sobel gradient magnitude (Tenengrad).
    pub fn tenengrad(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            (self.gxx + self.gyy) as f64 / self.n as f64
        }
    }

    pub fn add(&mut self, o: &TileStats) {
        self.n += o.n;
        self.lap += o.lap;
        self.lap2 += o.lap2;
        self.gxx += o.gxx;
        self.gyy += o.gyy;
        self.gxy += o.gxy;
        self.luma += o.luma;
        self.luma2 += o.luma2;
        self.clipped += o.clipped;
    }

    /// Luma variance (local contrast) of the tile, DN².
    pub fn luma_var(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let n = self.n as f64;
        let mean = self.luma as f64 / n;
        (self.luma2 as f64 / n - mean * mean).max(0.0)
    }

    /// Variance of the Laplacian over the pooled pixels.
    pub fn laplacian_var(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let n = self.n as f64;
        let mean = self.lap as f64 / n;
        (self.lap2 as f64 / n - mean * mean).max(0.0)
    }
}

/// Largest tile edge; keeps `Σ lap²` over one tile row below `i32::MAX`
/// (`256 · 1020² ≈ 2.7e8`).
pub(crate) const MAX_TILE: usize = 256;

#[derive(Debug, Clone)]
pub(crate) struct TileGrid {
    pub tile: usize,
    pub cols: usize,
    pub rows: usize,
    pub stats: Vec<TileStats>,
}

impl TileGrid {
    /// Tile rectangle in pixels: `(x0, y0, x1, y1)`, exclusive end.
    pub fn bounds(&self, idx: usize, width: usize, height: usize) -> (usize, usize, usize, usize) {
        let (tx, ty) = (idx % self.cols, idx / self.cols);
        let (x0, y0) = (tx * self.tile, ty * self.tile);
        (
            x0,
            y0,
            (x0 + self.tile).min(width),
            (y0 + self.tile).min(height),
        )
    }
}

/// A rectangle whose end is never before its start: an empty one is
/// zero-sized at its start, so callers can subtract its edges (the GPU
/// kernel reads them as unsigned) without wrapping.
fn non_inverted((x0, y0, x1, y1): (usize, usize, usize, usize)) -> (usize, usize, usize, usize) {
    (x0, y0, x1.max(x0), y1.max(y0))
}

/// Pixels (`x0..x1`, `y0..y1`) whose whole 3×3 stencil lies inside the
/// rectangle `x0..x1`, `y0..y1` of a `w × h` frame (zero-sized when a range
/// is empty).
pub(crate) fn region_interior(
    (x0, y0, x1, y1): (usize, usize, usize, usize),
    w: usize,
    h: usize,
) -> (usize, usize, usize, usize) {
    non_inverted((
        x0.saturating_add(1),
        y0.saturating_add(1),
        x1.min(w).saturating_sub(1),
        y1.min(h).saturating_sub(1),
    ))
}

/// Pixels of tile `idx` with a whole 3×3 stencil inside the frame (the
/// stencil may reach into neighbouring tiles).
pub(crate) fn tile_interior(
    grid: &TileGrid,
    idx: usize,
    w: usize,
    h: usize,
) -> (usize, usize, usize, usize) {
    let (x0, y0, x1, y1) = grid.bounds(idx, w, h);
    non_inverted((
        x0.max(1),
        y0.max(1),
        x1.min(w.saturating_sub(1)),
        y1.min(h.saturating_sub(1)),
    ))
}

/// Stencil sums over the pixels of a rectangle (`x0..x1`, `y0..y1`) whose
/// whole 3×3 stencil lies inside it, so pixels just outside a subject region
/// (a sharp or flat background) never leak into its measure.
pub(crate) fn region_stats(
    img: &crate::LumaImage,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> TileStats {
    let (xa, ya, xb, yb) = region_interior((x0, y0, x1, y1), img.width(), img.height());
    let mut s = TileStats::default();
    for y in ya..yb {
        // Row segments of at most MAX_TILE pixels keep the i32 sums exact.
        for x in (xa..xb).step_by(MAX_TILE) {
            let end = (x + MAX_TILE).min(xb);
            s.add(&segment(img.row(y - 1), img.row(y), img.row(y + 1), x, end));
        }
    }
    s
}

pub(crate) fn tile_pass(img: &crate::LumaImage, tile: usize) -> TileGrid {
    let (w, h) = (img.width(), img.height());
    let tile = tile.clamp(4, MAX_TILE);
    let cols = w.div_ceil(tile).max(1);
    let rows = h.div_ceil(tile).max(1);
    let mut stats = vec![TileStats::default(); cols * rows];
    if w >= 3 && h >= 3 {
        for y in 1..h - 1 {
            let (up, mid, dn) = (img.row(y - 1), img.row(y), img.row(y + 1));
            let base = (y / tile) * cols;
            for tx in 0..cols {
                let x0 = (tx * tile).max(1);
                let x1 = ((tx + 1) * tile).min(w - 1);
                if x0 < x1 {
                    let seg = segment(up, mid, dn, x0, x1);
                    stats[base + tx].add(&seg);
                }
            }
        }
    }
    TileGrid {
        tile,
        cols,
        rows,
        stats,
    }
}

/// Stencil sums for `x0..x1` of one row. `x0 >= 1`, `x1 <= width - 1`.
#[inline]
fn segment(up: &[u8], mid: &[u8], dn: &[u8], x0: usize, x1: usize) -> TileStats {
    let n = x1 - x0;
    // Pre-sliced, equal-length views let LLVM drop bounds checks and vectorize.
    let (ul, uc, ur) = (&up[x0 - 1..x1 - 1], &up[x0..x1], &up[x0 + 1..=x1]);
    let (ml, mc, mr) = (&mid[x0 - 1..x1 - 1], &mid[x0..x1], &mid[x0 + 1..=x1]);
    let (dl, dc, dr) = (&dn[x0 - 1..x1 - 1], &dn[x0..x1], &dn[x0 + 1..=x1]);
    let (mut s_lap, mut s_lap2, mut s_gxx, mut s_gyy, mut s_gxy) = (0i32, 0i32, 0i32, 0i32, 0i32);
    let (mut s_luma, mut s_luma2, mut s_clip) = (0i32, 0i32, 0i32);
    for i in 0..n {
        let c = i32::from(mc[i]);
        let (l, r) = (i32::from(ml[i]), i32::from(mr[i]));
        let (u, d) = (i32::from(uc[i]), i32::from(dc[i]));
        let (a, b) = (i32::from(ul[i]), i32::from(ur[i]));
        let (e, f) = (i32::from(dl[i]), i32::from(dr[i]));
        let lap = 4 * c - l - r - u - d;
        let gx = (b + 2 * r + f) - (a + 2 * l + e);
        let gy = (e + 2 * d + f) - (a + 2 * u + b);
        s_lap += lap;
        s_lap2 += lap * lap;
        s_gxx += gx * gx;
        s_gyy += gy * gy;
        s_gxy += gx * gy;
        s_luma += c;
        s_luma2 += c * c;
        s_clip += i32::from(c == 0 || c == 255);
    }
    TileStats {
        n: n as i64,
        lap: i64::from(s_lap),
        lap2: i64::from(s_lap2),
        gxx: i64::from(s_gxx),
        gyy: i64::from(s_gyy),
        gxy: i64::from(s_gxy),
        luma: i64::from(s_luma),
        luma2: i64::from(s_luma2),
        clipped: i64::from(s_clip),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LumaImage;

    #[test]
    fn single_edge_has_expected_stencil_sums() {
        // 5x3, left two columns 0, right three 100: one interior row (y = 1).
        let row = [0u8, 0, 100, 100, 100];
        let data: Vec<u8> = row.iter().copied().cycle().take(15).collect();
        let img = LumaImage::from_gray8(5, 3, data).unwrap();
        let g = tile_pass(&img, 64);
        assert_eq!((g.cols, g.rows, g.tile), (1, 1, 64));
        let s = g.stats[0];
        assert_eq!(s.n, 3);
        // Laplacians at x=1..3: -100, +100, 0.
        assert_eq!(s.lap, 0);
        assert_eq!(s.lap2, 20_000);
        // Sobel gx: 400 at x=1 and x=2, 0 at x=3; gy is 0 everywhere.
        assert_eq!(s.gxx, 2 * 400 * 400);
        assert_eq!((s.gyy, s.gxy), (0, 0));
        assert_eq!((s.luma, s.luma2), (200, 20_000));
        assert!((s.luma_var() - (20_000.0 / 3.0 - (200.0f64 / 3.0).powi(2))).abs() < 1e-9);
        assert_eq!(s.clipped, 1);
        assert!((s.laplacian_var() - 20_000.0 / 3.0).abs() < 1e-9);
        assert!((s.tenengrad() - 320_000.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn tiny_images_produce_empty_tiles() {
        for (w, h) in [(0, 0), (1, 1), (1, 50), (50, 2), (2, 2)] {
            let img = LumaImage::from_gray8(w, h, vec![7; w * h]).unwrap();
            let g = tile_pass(&img, 16);
            assert!(g.stats.iter().all(|s| s.n == 0), "{w}x{h}");
            assert_eq!(TileStats::default().laplacian_var(), 0.0);
            assert_eq!(TileStats::default().tenengrad(), 0.0);
            assert_eq!(TileStats::default().luma_var(), 0.0);
        }
    }

    #[test]
    fn tiles_cover_the_interior_exactly_once() {
        let (w, h) = (37, 23);
        let data = (0..w * h).map(|i| (i * 7 % 251) as u8).collect();
        let img = LumaImage::from_gray8(w, h, data).unwrap();
        let fine = tile_pass(&img, 5);
        let coarse = tile_pass(&img, 1000);
        assert_eq!(coarse.tile, MAX_TILE);
        let mut total = TileStats::default();
        fine.stats.iter().for_each(|s| total.add(s));
        assert_eq!(total, coarse.stats[0]);
        assert_eq!(total.n, ((w - 2) * (h - 2)) as i64);
        assert_eq!(fine.bounds(fine.stats.len() - 1, w, h), (35, 20, 37, 23));
        assert_eq!(region_stats(&img, 0, 0, w, h), total);
        assert_eq!(region_stats(&img, 10, 0, 5, h), TileStats::default());
    }

    #[test]
    fn empty_rectangles_are_zero_sized_not_inverted() {
        // A 1-pixel-wide frame has no interior column: the tile's range used
        // to come back as (1, _, 0, _).
        let img = LumaImage::from_gray8(1, 9, vec![7; 9]).unwrap();
        let g = tile_pass(&img, 16);
        let (x0, y0, x1, y1) = tile_interior(&g, 0, 1, 9);
        assert!(x1 >= x0 && y1 >= y0, "{:?}", (x0, y0, x1, y1));
        // A region that ends before it starts, or lies off the frame.
        assert_eq!(region_interior((5, 5, 2, 2), 10, 10), (6, 6, 6, 6));
        assert_eq!(region_interior((50, 0, 60, 4), 10, 10), (51, 1, 51, 3));
        let (_, _, x1, y1) = region_interior((0, 0, 1, 1), 10, 10);
        assert_eq!((x1, y1), (1, 1));
        // The edge of the address space does not overflow.
        assert_eq!(
            region_interior((usize::MAX, usize::MAX, usize::MAX, usize::MAX), 10, 10),
            (usize::MAX, usize::MAX, usize::MAX, usize::MAX)
        );
        assert_eq!(region_stats(&img, 0, 0, 1, 9), TileStats::default());
    }

    #[test]
    fn wide_regions_stay_exact() {
        // A whole-row region of a 3000-px checkerboard: one i32 sum over the
        // row would overflow (2998 · 1020² > 2³¹).
        let (w, h) = (3000, 3);
        let data = (0..w * h)
            .map(|i| if (i % w + i / w) % 2 == 0 { 255 } else { 0 })
            .collect();
        let img = LumaImage::from_gray8(w, h, data).unwrap();
        let s = region_stats(&img, 0, 0, w, h);
        assert_eq!(s.n, (w - 2) as i64);
        assert_eq!(s.lap2, (w - 2) as i64 * 1020 * 1020);
    }

    #[test]
    fn worst_case_tile_does_not_overflow() {
        // Checkerboard of 0/255 maximizes |lap|; a full-width max tile must stay exact.
        let (w, h) = (MAX_TILE + 2, 3);
        let data = (0..w * h)
            .map(|i| if (i % w + i / w) % 2 == 0 { 255 } else { 0 })
            .collect();
        let img = LumaImage::from_gray8(w, h, data).unwrap();
        let g = tile_pass(&img, MAX_TILE);
        let s: i64 = g.stats.iter().map(|s| s.lap2).sum();
        assert_eq!(s, MAX_TILE as i64 * 1020 * 1020);
    }
}
