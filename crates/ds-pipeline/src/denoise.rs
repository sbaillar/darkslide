//! AI denoise: the pipeline side of a learned denoiser that runs outside Rust (Core ML
//! in the app, on the Neural Engine and GPU).
//!
//! The network sees the **linear demosaiced image**, the pipeline's full-resolution BASE
//! (RCD at the reference white balance), encoded with the sRGB transfer curve. That is
//! what a simple camera pipeline without denoising produces, the domain real-noise
//! denoisers are trained on (SIDD). It is cut into overlapping `TILE`² tiles; the host
//! runs the model on each one ([`DenoiseJob::read_tile`] → model →
//! [`DenoiseJob::write_tile`], in any order, from any thread) and [`DenoiseJob::finish`]
//! stitches them with linear cross-fades over the overlaps (a partition of unity whose
//! ramps skip `MARGIN` pixels at each tile edge, where the network sees zero padding), so
//! no seam shows.
//!
//! The result, [`Denoised`], is camera RGB before white balance, like a demosaiced RAW:
//! [`denoised_raw`] turns it (blended with the original at a strength) into a
//! [`CfaPattern::LinearRgb`] [`RawImage`] the renderers develop like any other, so every
//! slider stays a DEVELOP edit on a cached BASE. Pixels the encoding clipped (a channel
//! at or past the clip level after white balance) keep the original values.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ds_core::{CfaPattern, RawImage, Size};
use half::f16;
use rayon::prelude::*;

use crate::color::{srgb_decode, srgb_encode};
use crate::demosaic::rcd;
use crate::plan::base_params;
use crate::{PipelineError, Priority};

/// Tile edge the model takes (pixels, square).
pub const TILE: u32 = 512;
/// Minimum overlap of neighbouring tiles.
pub const OVERLAP: u32 = 48;
/// Pixels at each overlapping tile edge that get no weight (the network's zero padding
/// makes its border pixels the least reliable).
pub const MARGIN: u32 = 8;
/// Version of the encoding and stitching: part of every cache key of denoised results.
pub const VERSION: u32 = 1;

/// Encoded values at or above this (any channel) start falling back to the original:
/// the model input is clamped to 1, so it can't see how far past the clip they are.
pub(crate) const CLIP_START: f32 = 0.92;
/// ... and at this, the original is used alone.
pub(crate) const CLIP_END: f32 = 0.99;

/// Sensor noise a denoised image is treated as keeping (share of the original's). The
/// pipeline's estimator can't measure it: on a denoised frame it reads fine texture as
/// noise (on SKB06204, ISO 4000, it still finds 63% while the residual the model removed
/// has the full noise's level). Set as the noise-adaptive tools' input, a lower value
/// lets sharpening and texture work above the (now low) noise again.
const RESIDUAL_NOISE: f32 = 0.2;

/// Largest value of the linear output (`u16`): normalized 1.0 (the sensor's clip).
pub(crate) const WHITE: f32 = 65_535.0;

/// Tile origins along one axis: one tile when `len ≤ tile`, else evenly spread tiles that
/// cover `0..len` with neighbours overlapping by at least `overlap`.
pub(crate) fn positions(len: u32, tile: u32, overlap: u32) -> Vec<u32> {
    if len <= tile {
        return vec![0];
    }
    let step = tile - overlap;
    let n = (len - overlap).div_ceil(step);
    let span = f64::from(len - tile);
    (0..n)
        .map(|i| (span * f64::from(i) / f64::from(n - 1)).round() as u32)
        .collect()
}

/// Weight of the tile at `pos[i]` for coordinate `x` along one axis: 1 inside, a linear
/// ramp over each overlap with a neighbour (skipping `margin` at the tile's edge), 0
/// outside the tile.
pub(crate) fn ramp_weight(pos: &[u32], i: usize, tile: u32, margin: u32, x: u32) -> f32 {
    let p = pos[i];
    if x < p || x >= p + tile {
        return 0.0;
    }
    let ramp = |d: u32, ov: u32| -> f32 {
        let m = margin.min(ov.saturating_sub(1) / 2);
        let len = (ov - 2 * m) as f32;
        ((d as f32 + 0.5 - m as f32) / len).clamp(0.0, 1.0)
    };
    let mut w = 1.0f32;
    if i > 0 {
        let ov = pos[i - 1] + tile - p;
        if ov > 0 && x - p < ov {
            w = w.min(ramp(x - p, ov));
        }
    }
    if i + 1 < pos.len() {
        let next = pos[i + 1];
        let ov = p + tile - next;
        if x >= next {
            w = w.min(ramp(p + tile - 1 - x, ov));
        }
    }
    w
}

/// The overlapping tiles an image is denoised in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileGrid {
    size: Size,
    tile: u32,
    xs: Vec<u32>,
    ys: Vec<u32>,
}

impl TileGrid {
    /// Tiles of `TILE` px over an image of `size`, overlapping by at least `OVERLAP`.
    pub fn new(size: Size) -> Self {
        Self::with_tile(size, TILE, OVERLAP)
    }

    fn with_tile(size: Size, tile: u32, overlap: u32) -> Self {
        Self {
            size,
            tile,
            xs: positions(size.width, tile, overlap),
            ys: positions(size.height, tile, overlap),
        }
    }

    pub fn len(&self) -> usize {
        self.xs.len() * self.ys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Tile edge in pixels.
    pub fn tile(&self) -> u32 {
        self.tile
    }

    /// Top-left of tile `i` (row-major). Images smaller than a tile are mirrored to fill it.
    pub fn origin(&self, i: usize) -> (u32, u32) {
        (self.xs[i % self.xs.len()], self.ys[i / self.xs.len()])
    }

    /// Per-coordinate weights of tile `i` along x and y (`tile` entries each).
    fn weights(&self, i: usize) -> (Vec<f32>, Vec<f32>) {
        let (xi, yi) = (i % self.xs.len(), i / self.xs.len());
        let (x0, y0) = (self.xs[xi], self.ys[yi]);
        (
            (0..self.tile)
                .map(|d| ramp_weight(&self.xs, xi, self.tile, MARGIN, x0 + d))
                .collect(),
            (0..self.tile)
                .map(|d| ramp_weight(&self.ys, yi, self.tile, MARGIN, y0 + d))
                .collect(),
        )
    }

    /// Sum of all tiles' weights at each coordinate along one axis (weights are separable,
    /// so the 2-D sum is the product of the two).
    fn sums(pos: &[u32], tile: u32, len: u32) -> Vec<f32> {
        (0..len)
            .map(|x| {
                (0..pos.len())
                    .map(|i| ramp_weight(pos, i, tile, MARGIN, x))
                    .sum()
            })
            .collect()
    }
}

/// Mirror `i` into `0..n` (no edge repeat).
pub(crate) fn mirror(i: u32, n: u32) -> u32 {
    if n <= 1 {
        return 0;
    }
    let period = 2 * (n - 1);
    let r = i % period;
    if r >= n { period - r } else { r }
}

/// `raw`'s full-resolution BASE (RCD at the reference white balance) sRGB-encoded as `f16`
/// bits, interleaved RGB, not clamped (values past 1 are past the clip), and the white
/// balance it is at. On the pipeline's pool for `priority`.
pub(crate) fn encode_base(
    raw: &RawImage,
    priority: Priority,
) -> Result<(Vec<u16>, [f32; 3]), PipelineError> {
    raw.validate()?;
    let p = base_params(raw);
    let encoded = crate::par::run_for(priority, || {
        let base = rcd(raw, &p);
        let mut enc = vec![0u16; base.px.len() * 3];
        enc.par_chunks_mut(3)
            .zip(base.px.par_iter())
            .for_each(|(e, v)| {
                for c in 0..3 {
                    e[c] = f16::from_f32(srgb_encode(v[c].max(0.0))).to_bits();
                }
            });
        enc
    });
    Ok((encoded, p.mults))
}

/// One image being denoised: its encoded BASE, the tile grid and the stitching
/// accumulator. `Send + Sync`: tiles may be read and written from several threads (the
/// host runs one model instance per compute unit).
#[derive(Debug)]
pub struct DenoiseJob {
    size: Size,
    grid: TileGrid,
    /// Reference white balance the BASE is at.
    mults: [f32; 3],
    /// sRGB-encoded BASE (f16 bits, interleaved RGB, not clamped: values past 1 are
    /// past the clip, kept for the fallback).
    encoded: Vec<u16>,
    /// Weighted sum of the model's outputs (interleaved RGB, encoded).
    acc: Mutex<Vec<f32>>,
    written: Vec<AtomicBool>,
}

impl DenoiseJob {
    /// Demosaic `raw` at full resolution (on the pipeline's pool for `priority`) and
    /// encode it for the model.
    pub fn new(raw: &RawImage, priority: Priority) -> Result<Self, PipelineError> {
        let (encoded, mults) = encode_base(raw, priority)?;
        let size = raw.size;
        let grid = TileGrid::new(size);
        let written = (0..grid.len()).map(|_| AtomicBool::new(false)).collect();
        Ok(Self {
            size,
            mults,
            acc: Mutex::new(vec![0.0; encoded.len()]),
            encoded,
            grid,
            written,
        })
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn grid(&self) -> &TileGrid {
        &self.grid
    }

    /// Elements of one tile buffer: 3 planes of `TILE`² (`f16`).
    pub fn tile_len(&self) -> usize {
        3 * (self.grid.tile as usize).pow(2)
    }

    fn check(&self, i: usize, len: usize) -> Result<(), PipelineError> {
        if i >= self.grid.len() {
            return Err(PipelineError::InvalidRequest(format!(
                "tile {i} of {}",
                self.grid.len()
            )));
        }
        if len < self.tile_len() {
            return Err(PipelineError::InvalidRequest(format!(
                "tile buffer of {len} elements, {} needed",
                self.tile_len()
            )));
        }
        Ok(())
    }

    /// The model input of tile `i`: planar RGB (`[3][TILE][TILE]`, `f16` bits), encoded,
    /// clamped to 0..1, mirrored past the image edge (images smaller than a tile).
    pub fn read_tile(&self, i: usize, out: &mut [u16]) -> Result<(), PipelineError> {
        self.check(i, out.len())?;
        let t = self.grid.tile as usize;
        let (x0, y0) = self.grid.origin(i);
        let (w, h) = (self.size.width, self.size.height);
        let one = f16::ONE.to_bits();
        let (r, rest) = out.split_at_mut(t * t);
        let (g, b) = rest.split_at_mut(t * t);
        for (ty, ((rr, gr), br)) in r
            .chunks_mut(t)
            .zip(g.chunks_mut(t))
            .zip(b.chunks_mut(t))
            .enumerate()
        {
            let y = mirror(y0 + ty as u32, h) as usize;
            for tx in 0..t {
                let x = mirror(x0 + tx as u32, w) as usize;
                let s = &self.encoded[(y * w as usize + x) * 3..][..3];
                // f16 bit patterns of non-negative values order like the values.
                rr[tx] = s[0].min(one);
                gr[tx] = s[1].min(one);
                br[tx] = s[2].min(one);
            }
        }
        Ok(())
    }

    /// The model output of tile `i`: `[3][TILE][TILE]` `f16` bits, element `(c, y, x)` at
    /// `c·channel_stride + y·row_stride + x` (Core ML may pad rows). Each tile is taken
    /// once; writing it again is an error (its first output is kept).
    pub fn write_tile(
        &self,
        i: usize,
        data: &[u16],
        row_stride: usize,
        channel_stride: usize,
    ) -> Result<(), PipelineError> {
        self.check(i, usize::MAX)?;
        let t = self.grid.tile as usize;
        if row_stride < t || channel_stride < row_stride * t {
            return Err(PipelineError::InvalidRequest(format!(
                "tile strides {row_stride}/{channel_stride} for a {t} px tile"
            )));
        }
        let need = 2 * channel_stride + (t - 1) * row_stride + t;
        if data.len() < need {
            return Err(PipelineError::InvalidRequest(format!(
                "tile output of {} elements, {need} needed",
                data.len()
            )));
        }
        if self.written[i].swap(true, Ordering::AcqRel) {
            return Err(PipelineError::InvalidRequest(format!(
                "tile {i} written twice"
            )));
        }
        let (x0, y0) = self.grid.origin(i);
        let (wx, wy) = self.grid.weights(i);
        let w = self.size.width as usize;
        // Only the part of the tile inside the image (a mirrored tile's rest is padding).
        let tw = (self.size.width - x0).min(self.grid.tile) as usize;
        let th = (self.size.height - y0).min(self.grid.tile) as usize;
        let mut acc = self
            .acc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for ty in 0..th {
            let row = (y0 as usize + ty) * w + x0 as usize;
            let dst = &mut acc[row * 3..(row + tw) * 3];
            for (tx, px) in dst.as_chunks_mut::<3>().0.iter_mut().enumerate() {
                let k = wx[tx] * wy[ty];
                if k == 0.0 {
                    continue;
                }
                for (c, v) in px.iter_mut().enumerate() {
                    let s = f16::from_bits(data[c * channel_stride + ty * row_stride + tx]);
                    *v += k * s.to_f32();
                }
            }
        }
        Ok(())
    }

    /// Tiles not written yet.
    pub fn missing(&self) -> usize {
        self.written
            .iter()
            .filter(|w| !w.load(Ordering::Acquire))
            .count()
    }

    /// Stitch the tiles into the denoised linear image. Every tile must have been written.
    pub fn finish(self, priority: Priority) -> Result<Denoised, PipelineError> {
        let missing = self.missing();
        if missing > 0 {
            return Err(PipelineError::InvalidRequest(format!(
                "{missing} of {} tiles not denoised",
                self.grid.len()
            )));
        }
        let Self {
            size,
            grid,
            mults,
            mut encoded,
            acc,
            ..
        } = self;
        let acc = acc
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sx = TileGrid::sums(&grid.xs, grid.tile, size.width);
        let sy = TileGrid::sums(&grid.ys, grid.tile, size.height);
        let w = size.width as usize;
        let inv = mults.map(|m| WHITE / m);
        let original = |p: usize| -> [f32; 3] {
            [0, 1, 2].map(|c| srgb_decode(f16::from_bits(encoded[p * 3 + c]).to_f32()) * inv[c])
        };
        let orig_means = crate::par::run_for(priority, || block_means(size, original));
        // Share of the model in each pixel (the rest is the original: clipped pixels).
        let mut weight = vec![0u8; w * size.height as usize];
        // The output replaces the encoded input in place (both 3 × u16 per pixel).
        crate::par::run_for(priority, || {
            encoded
                .par_chunks_mut(w * 3)
                .zip(acc.par_chunks(w * 3))
                .zip(weight.par_chunks_mut(w))
                .enumerate()
                .for_each(|(y, ((row, arow), wrow))| {
                    for (x, ((px, a), wt)) in row
                        .as_chunks_mut::<3>()
                        .0
                        .iter_mut()
                        .zip(arow.as_chunks::<3>().0)
                        .zip(wrow.iter_mut())
                        .enumerate()
                    {
                        let input = [0, 1, 2].map(|c| f16::from_bits(px[c]).to_f32());
                        let peak = input[0].max(input[1]).max(input[2]);
                        let t = ((peak - CLIP_START) / (CLIP_END - CLIP_START)).clamp(0.0, 1.0);
                        let model = 1.0 - t * t * (3.0 - 2.0 * t);
                        *wt = (model * 255.0).round() as u8;
                        let norm = 1.0 / (sx[x] * sy[y]).max(f32::MIN_POSITIVE);
                        for c in 0..3 {
                            let den = srgb_decode(a[c] * norm);
                            let orig = srgb_decode(input[c]);
                            let v = model * den + (1.0 - model) * orig;
                            px[c] = (v * inv[c]).clamp(0.0, WHITE).round() as u16;
                        }
                    }
                });
            // Low-frequency tone match: the model's output has the original's local mean
            // per channel (its own small level-dependent bias, and that of averaging
            // in the encoded domain, would otherwise tint and darken the shadows).
            let den_means = block_means(size, |p| [0, 1, 2].map(|c| f32::from(encoded[p * 3 + c])));
            let diff: Vec<[f32; 3]> = orig_means
                .iter()
                .zip(&den_means)
                .map(|(o, d)| [0, 1, 2].map(|c| o[c] - d[c]))
                .collect();
            let blocks = (size.width as usize).div_ceil(MATCH_BLOCK);
            encoded
                .par_chunks_mut(w * 3)
                .zip(weight.par_chunks(w))
                .enumerate()
                .for_each(|(y, (row, wrow))| {
                    for (x, (px, wt)) in row.as_chunks_mut::<3>().0.iter_mut().zip(wrow).enumerate()
                    {
                        if *wt == 0 {
                            continue;
                        }
                        let k = f32::from(*wt) / 255.0;
                        let d = sample_blocks(&diff, blocks, x, y);
                        for c in 0..3 {
                            px[c] = (f32::from(px[c]) + k * d[c]).clamp(0.0, WHITE).round() as u16;
                        }
                    }
                });
        });
        Ok(Denoised {
            size,
            data: encoded,
        })
    }
}

/// Block edge of the low-frequency tone match in [`DenoiseJob::finish`].
const MATCH_BLOCK: usize = 64;

/// Mean of `f(pixel index)` over each `MATCH_BLOCK`² block (row-major; edge blocks
/// partial).
fn block_means(size: Size, f: impl Fn(usize) -> [f32; 3] + Sync) -> Vec<[f32; 3]> {
    let (w, h) = (size.width as usize, size.height as usize);
    let bw = w.div_ceil(MATCH_BLOCK);
    (0..h.div_ceil(MATCH_BLOCK))
        .into_par_iter()
        .flat_map_iter(|by| {
            let mut sums = vec![[0f64; 3]; bw];
            let mut counts = vec![0u32; bw];
            for y in by * MATCH_BLOCK..((by + 1) * MATCH_BLOCK).min(h) {
                for x in 0..w {
                    let v = f(y * w + x);
                    let b = x / MATCH_BLOCK;
                    for c in 0..3 {
                        sums[b][c] += f64::from(v[c]);
                    }
                    counts[b] += 1;
                }
            }
            sums.into_iter()
                .zip(counts)
                .map(|(s, n)| s.map(|v| (v / f64::from(n)) as f32))
        })
        .collect()
}

/// Bilinear sample at pixel `(x, y)` of a block grid `blocks` wide (values at block
/// centres; clamped at the edges).
fn sample_blocks(grid: &[[f32; 3]], blocks: usize, x: usize, y: usize) -> [f32; 3] {
    let rows = grid.len() / blocks;
    let coord = |p: usize, n: usize| -> (usize, usize, f32) {
        let f = ((p as f32 + 0.5) / MATCH_BLOCK as f32 - 0.5).clamp(0.0, (n - 1) as f32);
        let i = (f as usize).min(n - 1);
        (i, (i + 1).min(n - 1), f - i as f32)
    };
    let (x0, x1, fx) = coord(x, blocks);
    let (y0, y1, fy) = coord(y, rows);
    let at = |bx: usize, by: usize| grid[by * blocks + bx];
    let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
    [0, 1, 2].map(|k| {
        let top = a[k] + (b[k] - a[k]) * fx;
        let bottom = c[k] + (d[k] - c[k]) * fx;
        top + (bottom - top) * fy
    })
}

/// A denoised image: linear camera RGB before white balance, interleaved, `0..=65535` =
/// normalized `0..=1` (the sensor's clip), the size of the RAW's active area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denoised {
    pub size: Size,
    pub data: Vec<u16>,
}

impl Denoised {
    /// Bytes held.
    pub fn bytes(&self) -> u64 {
        std::mem::size_of_val(self.data.as_slice()) as u64
    }
}

/// The original's linear image in [`Denoised`]'s layout (the full-resolution RCD BASE
/// without its white balance), for blending at a strength below 1.
pub fn linear_original(raw: &RawImage, priority: Priority) -> Result<Denoised, PipelineError> {
    raw.validate()?;
    let p = base_params(raw);
    let inv = p.mults.map(|m| WHITE / m);
    let data = crate::par::run_for(priority, || {
        let base = rcd(raw, &p);
        let mut out = vec![0u16; base.px.len() * 3];
        out.par_chunks_mut(3)
            .zip(base.px.par_iter())
            .for_each(|(o, v)| {
                for c in 0..3 {
                    o[c] = (v[c] * inv[c]).clamp(0.0, WHITE).round() as u16;
                }
            });
        out
    });
    Ok(Denoised {
        size: raw.size,
        data,
    })
}

/// The image to develop for AI denoise at `strength` (0..=1): the original's linear image
/// blended toward `denoised` (`original` from [`linear_original`]; it may be `None` at
/// strength 1), as a [`CfaPattern::LinearRgb`] raw with `raw`'s colour, orientation and
/// crop. The pipeline's measured sensor noise of the result is set to the original's,
/// scaled by how much of the noise the blend keeps, so the noise-adaptive tools (chroma
/// NR, sharpening and texture thresholds) follow the strength.
///
/// Strength 0 is not handled here: render the original `raw` itself (bit-identical).
pub fn denoised_raw(
    raw: &RawImage,
    denoised: &Denoised,
    original: Option<&Denoised>,
    strength: f32,
) -> Result<RawImage, PipelineError> {
    raw.validate()?;
    let s = if strength.is_finite() {
        strength.clamp(0.0, 1.0)
    } else {
        1.0
    };
    if denoised.size != raw.size || denoised.data.len() != raw.size.pixels() as usize * 3 {
        return Err(PipelineError::InvalidRequest(format!(
            "denoised image {:?} for a {:?} raw",
            denoised.size, raw.size
        )));
    }
    let data = if s >= 1.0 {
        denoised.data.clone()
    } else {
        let Some(orig) = original.filter(|o| o.data.len() == denoised.data.len()) else {
            return Err(PipelineError::InvalidRequest(
                "a partial strength needs the original's linear image".into(),
            ));
        };
        let mut out = vec![0u16; denoised.data.len()];
        crate::par::run_low(|| {
            out.par_chunks_mut(1 << 16)
                .zip(denoised.data.par_chunks(1 << 16))
                .zip(orig.data.par_chunks(1 << 16))
                .for_each(|((o, d), a)| {
                    for ((o, d), a) in o.iter_mut().zip(d).zip(a) {
                        let v = f32::from(*a) + s * (f32::from(*d) - f32::from(*a));
                        *o = v.round() as u16;
                    }
                });
        });
        out
    };
    let out = RawImage {
        size: raw.size,
        cfa: CfaPattern::LinearRgb,
        data,
        black_level: [0.0; 4],
        white_level: WHITE,
        wb_as_shot: raw.wb_as_shot,
        wb_daylight: raw.wb_daylight,
        cam_to_xyz: raw.cam_to_xyz,
        orientation: raw.orientation,
        default_crop: raw.default_crop,
    };
    let kept = (1.0 - s) + s * RESIDUAL_NOISE;
    crate::plan::seed_raw_noise(&out, crate::plan::raw_noise(raw) * kept);
    Ok(out)
}

pub use crate::plan::NoiseSeed;

/// The identity of `raw`'s seeded noise (take it while the raw is alive).
pub fn seed_of(raw: &RawImage) -> NoiseSeed {
    crate::plan::noise_seed_of(raw)
}

/// A raw from [`denoised_raw`] was dropped: forget its seeded noise.
pub fn release(seed: NoiseSeed) {
    crate::plan::unseed(seed);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)] // exact values are intended where asserted

    use super::*;
    use crate::testutil::{noise, raw_from_fn};
    use crate::{ImageKey, Quality, RenderRequest, Renderer, cpu_renderer};
    use ds_core::{Adjustments, Orientation};

    /// A smooth colourful scene with sensor-like noise, `w`×`h`.
    fn scene(w: u32, h: u32) -> RawImage {
        raw_from_fn(w, h, |x, y| {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let n = 0.02 * noise(x, y);
            [
                0.1 + 0.5 * fx + n,
                0.2 + 0.3 * fy + n,
                0.4 - 0.3 * fx * fy + n,
            ]
        })
    }

    /// Run `model` (planar tile in → planar tile out) over every tile of `job`.
    fn run(job: &DenoiseJob, model: impl Fn(usize, &mut [u16])) {
        let mut buf = vec![0u16; job.tile_len()];
        let t = job.grid().tile() as usize;
        for i in 0..job.grid().len() {
            job.read_tile(i, &mut buf).unwrap();
            model(i, &mut buf);
            job.write_tile(i, &buf, t, t * t).unwrap();
        }
    }

    fn max_diff(a: &[u16], b: &[u16]) -> u16 {
        a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap()
    }

    #[test]
    fn tiles_cover_the_image_with_the_minimum_overlap() {
        for len in [1, 100, 512, 513, 960, 1000, 4000, 6000] {
            let p = positions(len, TILE, OVERLAP);
            assert_eq!(p[0], 0);
            if len <= TILE {
                assert_eq!(p, vec![0], "one tile for {len}");
                continue;
            }
            assert_eq!(
                *p.last().unwrap() + TILE,
                len,
                "the last tile ends at the edge"
            );
            for w in p.windows(2) {
                assert!(w[0] + TILE - w[1] >= OVERLAP, "{len}: {p:?}");
            }
        }
        assert_eq!(TileGrid::new(Size::new(6000, 4000)).len(), 13 * 9);
        assert!(!TileGrid::new(Size::new(10, 10)).is_empty());
    }

    #[test]
    fn tile_weights_are_a_partition_of_unity_without_the_tile_edges() {
        for len in [300, 512, 700, 1000, 4000, 6000] {
            let p = positions(len, TILE, OVERLAP);
            let sums = TileGrid::sums(&p, TILE, len);
            if p.windows(2).all(|w| w[0] + TILE - w[1] <= TILE / 2) {
                assert!(sums.iter().all(|s| (s - 1.0).abs() < 1e-5), "{len}");
            } else {
                // Three tiles meet somewhere (images just over a tile): normalized by the
                // sums in `finish`, which only need to be positive.
                assert!(sums.iter().all(|s| *s >= 1.0 - 1e-5), "{len}");
            }
            // Pixels within MARGIN of an inner tile edge get no weight from that tile.
            for i in 1..p.len() {
                for d in 0..MARGIN {
                    assert_eq!(ramp_weight(&p, i, TILE, MARGIN, p[i] + d), 0.0);
                    assert_eq!(
                        ramp_weight(&p, i - 1, TILE, MARGIN, p[i - 1] + TILE - 1 - d),
                        0.0
                    );
                }
            }
        }
        // Narrow overlaps (a tiny tile grid) still sum to one.
        let p = positions(20, 8, 3);
        let sums = TileGrid::sums(&p, 8, 20);
        assert!(
            sums.iter().all(|s| (s - 1.0).abs() < 1e-5),
            "{p:?} {sums:?}"
        );
    }

    #[test]
    fn an_identity_model_returns_the_original_exactly_up_to_f16() {
        let raw = scene(1100, 700);
        let job = DenoiseJob::new(&raw, Priority::Interactive).unwrap();
        assert_eq!(job.size(), raw.size);
        assert_eq!(job.grid().len(), 3 * 2);
        run(&job, |_, _| {});
        assert_eq!(job.missing(), 0);
        let den = job.finish(Priority::Background).unwrap();
        let orig = linear_original(&raw, Priority::Background).unwrap();
        assert_eq!(den.bytes(), 1100 * 700 * 3 * 2);
        // f16 keeps 11 bits: ~0.05% of the value, on a 16-bit scale.
        let worst = den
            .data
            .iter()
            .zip(&orig.data)
            .map(|(a, b)| f32::from(a.abs_diff(*b)) / f32::from(*b).max(256.0))
            .fold(0.0f32, f32::max);
        assert!(worst < 2e-3, "worst relative error {worst}");
    }

    #[test]
    fn tile_seams_never_show_even_when_tiles_disagree() {
        // A model whose output depends on the whole tile (like NAFNet's channel attention):
        // each tile is brightened by a different amount. The cross-fades must turn the
        // differences into gradients spread over the overlaps, never steps.
        let raw = raw_from_fn(1400, 1000, |_, _| [0.3; 3]);
        let job = DenoiseJob::new(&raw, Priority::Interactive).unwrap();
        let shift = |i: usize| 0.02 * (i % 3) as f32;
        run(&job, |i, buf| {
            for v in buf.iter_mut() {
                *v = f16::from_f32(f16::from_bits(*v).to_f32() + shift(i)).to_bits();
            }
        });
        let den = job.finish(Priority::Interactive).unwrap();
        let w = 1400usize;
        let g = |x: usize, y: usize| f32::from(den.data[(y * w + x) * 3 + 1]);
        // Largest step between neighbours, against the largest tile disagreement.
        let mut step = 0.0f32;
        for y in (0..1000).step_by(7) {
            for x in 0..w - 1 {
                step = step.max((g(x + 1, y) - g(x, y)).abs());
            }
        }
        for x in (0..w).step_by(7) {
            for y in 0..999 {
                step = step.max((g(x, y + 1) - g(x, y)).abs());
            }
        }
        // 0.04 encoded at 0.3 linear is ~0.024 linear (~1600 of 65535 at G); spread over at
        // least OVERLAP - 2·MARGIN = 32 px, a step is ≤ 1/32 of it.
        let total = srgb_decode(srgb_encode(0.3) + 0.04) - 0.3;
        assert!(
            step <= total * WHITE / 32.0 * 1.1,
            "step {step} for a jump of {}",
            total * WHITE
        );
        // (The low-frequency tone match then removes most of the tiles' offsets, but only
        // at a 64 px block scale: a hard seam inside a block would remain a full step.)
        assert!(
            step > 0.0,
            "the overlaps carry the tiles' disagreement as a gradient"
        );
    }

    #[test]
    fn the_tone_match_keeps_local_means_when_the_model_is_biased() {
        let raw = scene(900, 600);
        let job = DenoiseJob::new(&raw, Priority::Interactive).unwrap();
        // A model that darkens and tints everything (a biased denoiser).
        run(&job, |_, buf| {
            let n = buf.len() / 3;
            for (k, v) in buf.iter_mut().enumerate() {
                let gain = [0.9, 0.97, 0.93][k / n];
                *v = f16::from_f32(f16::from_bits(*v).to_f32() * gain).to_bits();
            }
        });
        let den = job.finish(Priority::Interactive).unwrap();
        let orig = linear_original(&raw, Priority::Interactive).unwrap();
        for c in 0..3 {
            let mean = |d: &[u16]| {
                d.iter()
                    .skip(c)
                    .step_by(3)
                    .map(|v| f64::from(*v))
                    .sum::<f64>()
                    / (900.0 * 600.0)
            };
            let (a, b) = (mean(&den.data), mean(&orig.data));
            assert!((a / b - 1.0).abs() < 2e-3, "channel {c}: {a} vs {b}");
        }
    }

    #[test]
    fn clipped_pixels_keep_the_original() {
        // A bright patch past the clip after white balance (the encoding can't show the
        // model how far past it is): a model that zeroes everything leaves it untouched.
        let raw = raw_from_fn(
            600,
            600,
            |x, _| if x < 300 { [1.4, 1.2, 1.3] } else { [0.2; 3] },
        );
        let job = DenoiseJob::new(&raw, Priority::Interactive).unwrap();
        run(&job, |_, buf| buf.fill(0));
        let den = job.finish(Priority::Interactive).unwrap();
        let orig = linear_original(&raw, Priority::Interactive).unwrap();
        let w = 600usize;
        for y in [10usize, 300, 590] {
            for x in [10usize, 150, 280] {
                let i = (y * w + x) * 3;
                assert!(
                    max_diff(&den.data[i..i + 3], &orig.data[i..i + 3]) <= 40,
                    "({x},{y})"
                );
            }
        }
    }

    #[test]
    fn images_smaller_than_a_tile_are_mirrored_into_it() {
        let raw = scene(100, 60);
        let job = DenoiseJob::new(&raw, Priority::Interactive).unwrap();
        assert_eq!(job.grid().len(), 1);
        let mut buf = vec![0u16; job.tile_len()];
        job.read_tile(0, &mut buf).unwrap();
        let t = TILE as usize;
        // Column 100 mirrors column 98, row 60 mirrors row 58.
        assert_eq!(buf[5 * t + 100], buf[5 * t + 98]);
        assert_eq!(buf[60 * t + 7], buf[58 * t + 7]);
        assert!(
            buf.iter().all(|v| f16::from_bits(*v).to_f32() <= 1.0),
            "clamped to 1"
        );
        job.write_tile(0, &buf, t, t * t).unwrap();
        let den = job.finish(Priority::Interactive).unwrap();
        assert_eq!(den.size, raw.size);
        assert_eq!(mirror(5, 1), 0);
        assert_eq!(mirror(7, 4), 1);
    }

    #[test]
    fn tile_calls_reject_bad_arguments() {
        let raw = scene(600, 520);
        let job = DenoiseJob::new(&raw, Priority::Interactive).unwrap();
        let n = job.grid().len();
        let t = TILE as usize;
        let mut buf = vec![0u16; job.tile_len()];
        assert!(job.read_tile(n, &mut buf).is_err(), "index past the grid");
        assert!(job.read_tile(0, &mut buf[..10]).is_err(), "short buffer");
        assert!(job.write_tile(n, &buf, t, t * t).is_err());
        assert!(job.write_tile(0, &buf, t - 1, t * t).is_err(), "row stride");
        assert!(
            job.write_tile(0, &buf, t, t * t - 1).is_err(),
            "channel stride"
        );
        assert!(
            job.write_tile(0, &buf[..100], t, t * t).is_err(),
            "short output"
        );
        // Padded rows (Core ML may align them) are read through the strides.
        let (rs, cs) = (t + 8, (t + 8) * t);
        let mut padded = vec![0u16; 3 * cs];
        job.read_tile(0, &mut buf).unwrap();
        for c in 0..3 {
            for y in 0..t {
                padded[c * cs + y * rs..][..t].copy_from_slice(&buf[c * t * t + y * t..][..t]);
            }
        }
        job.write_tile(0, &padded, rs, cs).unwrap();
        assert!(job.write_tile(0, &buf, t, t * t).is_err(), "written twice");
        assert_eq!(job.missing(), n - 1);
        let err = DenoiseJob::new(&raw, Priority::Interactive)
            .unwrap()
            .finish(Priority::Interactive);
        assert!(matches!(err, Err(PipelineError::InvalidRequest(m)) if m.contains("not denoised")));
        let mut bad = raw.clone();
        bad.data.truncate(3);
        assert!(DenoiseJob::new(&bad, Priority::Interactive).is_err());
        assert!(linear_original(&bad, Priority::Interactive).is_err());
    }

    #[test]
    fn strength_blends_toward_the_denoised_image() {
        let raw = scene(64, 48);
        let orig = linear_original(&raw, Priority::Interactive).unwrap();
        let den = Denoised {
            size: raw.size,
            data: orig.data.iter().map(|v| v / 2).collect(),
        };
        let full = denoised_raw(&raw, &den, None, 1.0).unwrap();
        assert_eq!(full.cfa, CfaPattern::LinearRgb);
        assert_eq!(full.data, den.data);
        assert_eq!((full.white_level, full.black_level), (WHITE, [0.0; 4]));
        assert_eq!(full.cam_to_xyz, raw.cam_to_xyz);
        assert_eq!(full.wb_as_shot, raw.wb_as_shot);
        full.validate().unwrap();
        let half = denoised_raw(&raw, &den, Some(&orig), 0.5).unwrap();
        for ((h, o), d) in half.data.iter().zip(&orig.data).zip(&den.data) {
            assert!(
                f32::from(*h)
                    .sub(0.5 * f32::from(*o) + 0.5 * f32::from(*d))
                    .abs()
                    <= 0.5
            );
        }
        // NaN means full strength; out of range clamps.
        assert_eq!(
            denoised_raw(&raw, &den, None, f32::NAN).unwrap().data,
            den.data
        );
        assert_eq!(denoised_raw(&raw, &den, None, 7.0).unwrap().data, den.data);
        assert!(
            denoised_raw(&raw, &den, None, 0.5).is_err(),
            "needs the original"
        );
        let small = Denoised {
            size: Size::new(2, 2),
            data: vec![0; 12],
        };
        assert!(
            denoised_raw(&raw, &small, None, 1.0).is_err(),
            "size mismatch"
        );
        assert!(
            denoised_raw(&raw, &den, Some(&small), 0.5).is_err(),
            "original size mismatch"
        );
    }

    #[test]
    fn a_seed_outlives_the_measured_memo_until_released() {
        let raw = scene(128, 128);
        let orig = linear_original(&raw, Priority::Interactive).unwrap();
        let blend = denoised_raw(&raw, &orig, None, 1.0).unwrap();
        let seeded = crate::plan::raw_noise(&blend);
        // Measure 40 other images: the bounded memo of measured values turns over.
        let others: Vec<_> = (0..40).map(|i| scene(64 + i, 64)).collect();
        for o in &others {
            crate::plan::raw_noise(o);
        }
        assert_eq!(crate::plan::raw_noise(&blend), seeded, "still the seed");
        release(seed_of(&blend));
        assert_ne!(
            crate::plan::raw_noise(&blend),
            seeded,
            "measured once released"
        );
    }

    #[test]
    fn the_seeded_noise_follows_the_strength() {
        let raw = scene(256, 256);
        let orig = linear_original(&raw, Priority::Interactive).unwrap();
        let measured = crate::plan::raw_noise(&raw);
        assert!(measured > 0.0);
        let den = orig.clone();
        let one = denoised_raw(&raw, &den, Some(&orig), 1.0).unwrap();
        assert!((crate::plan::raw_noise(&one) - measured * RESIDUAL_NOISE).abs() < 1e-6);
        let half = denoised_raw(&raw, &den, Some(&orig), 0.5).unwrap();
        let kept = 0.5 + 0.5 * RESIDUAL_NOISE;
        assert!((crate::plan::raw_noise(&half) - measured * kept).abs() < 1e-6);
    }

    #[test]
    fn the_linear_image_develops_like_the_raw_it_came_from() {
        // The seam's premise: a LinearRgb raw holding the original's linear image renders
        // like the mosaic (same BASE up to 16-bit rounding), so AI denoise changes only
        // the noise. Rendered at 1:1, where any demosaic difference would show.
        let raw = scene(320, 240);
        let orig = linear_original(&raw, Priority::Interactive).unwrap();
        let lin = denoised_raw(&raw, &orig, None, 1.0).unwrap();
        // Same noise-adaptive inputs as the original (otherwise sharpening differs).
        crate::plan::seed_raw_noise(&lin, crate::plan::raw_noise(&raw));
        let adj = Adjustments::default();
        let req = RenderRequest {
            quality: Quality::Full,
            ..RenderRequest::new(Size::new(320, 240))
        };
        let a = cpu_renderer()
            .render(ImageKey(1), &raw, &adj, &req)
            .unwrap();
        let b = cpu_renderer()
            .render(ImageKey(2), &lin, &adj, &req)
            .unwrap();
        let worst = a
            .rgba8
            .iter()
            .zip(&b.rgba8)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap();
        assert!(worst <= 2, "8-bit difference {worst}");
        assert_eq!(lin.orientation, Orientation::Normal);
    }

    trait Sub {
        fn sub(self, o: f32) -> f32;
    }
    impl Sub for f32 {
        fn sub(self, o: f32) -> f32 {
            self - o
        }
    }
}
