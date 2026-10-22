//! AI upscale: the pipeline side of a learned 2× super-resolution network that runs
//! outside Rust (Core ML in the app, on the Neural Engine and GPU), like
//! [`crate::denoise`].
//!
//! The image is cut into overlapping `TILE`² tiles; the host runs the model on each one
//! ([`UpscaleJob::read_tile`] → model → [`UpscaleJob::write_tile`], in any order, from any
//! thread). The `2·TILE`² outputs are stitched with linear cross-fades over the overlaps
//! (the denoiser's partition of unity, scaled ×2), so no seam shows.
//!
//! Memory stays bounded on any size: the stitched output is never held whole. Rows are
//! accumulated only until every tile covering them is written, then
//! [`UpscaleJob::take_rows`] hands them out, in order, to be streamed to a file. With the
//! tiles run in index order (row by row) at most about two tile rows of output are held
//! (~150 MB for a 24 MP frame, ~230 MB for 60 MP).
//!
//! Two inputs:
//! - [`UpscaleJob::from_raw`]: a RAW's linear BASE (RCD at the reference white balance),
//!   sRGB-encoded, the AI Denoise model's domain. The output is linear camera RGB before
//!   white balance at twice the size, which a linear DNG stores (a new photo that
//!   develops like the original). Pixels the encoding clipped keep the original,
//!   interpolated.
//! - [`UpscaleJob::from_encoded`]: an export's rendered, output-encoded 16-bit RGB. The
//!   output is the same encoding at twice the size.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};

use ds_core::{RawImage, Size};
use half::f16;
use rayon::prelude::*;

use crate::color::srgb_decode;
use crate::denoise::{CLIP_END, CLIP_START, WHITE, encode_base, mirror, positions, ramp_weight};
use crate::{PipelineError, Priority};

/// Output pixels per input pixel along each axis.
pub const SCALE: u32 = 2;
/// Input tile edge the model takes (pixels, square); its output is `SCALE · TILE`.
pub const TILE: u32 = 256;
/// Minimum overlap of neighbouring input tiles.
pub const OVERLAP: u32 = 24;
/// Input pixels at each overlapping tile edge that get no weight (the network's zero
/// padding makes its border pixels the least reliable).
pub const MARGIN: u32 = 4;
/// Version of the encoding and stitching.
pub const VERSION: u32 = 1;
/// Largest output (pixels): 256 MP, a 64 MP original doubled. Its linear DNG is 1.5 GB,
/// well inside classic TIFF's 4 GB.
pub const MAX_OUTPUT_PIXELS: u64 = 256_000_000;

/// What the input holds, and so what the output rows are.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Domain {
    /// A BASE at these white-balance multipliers: rows are linear camera RGB before white
    /// balance, `0..=65535` = the sensor's clip.
    Linear([f32; 3]),
    /// Output-encoded RGB: rows are the same encoding, `0..=65535`.
    Encoded,
}

/// Stitched output rows not handed out yet.
#[derive(Debug)]
struct Band {
    /// First output row held (every row above it was handed out).
    top: u32,
    /// Weighted sum of the model's outputs from `top` down (interleaved RGB, output width).
    acc: Vec<f32>,
    /// Tiles written per tile row.
    done: Vec<u32>,
}

/// Output rows `y0..y0 + rows`, interleaved RGB `u16` (see the module docs for their
/// domain).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rows {
    pub y0: u32,
    pub rows: u32,
    pub data: Vec<u16>,
}

/// One image being upscaled. `Send + Sync`: tiles may be read and written from several
/// threads (the host runs one model instance per compute unit).
#[derive(Debug)]
pub struct UpscaleJob {
    size: Size,
    out: Size,
    xs: Vec<u32>,
    ys: Vec<u32>,
    /// Output-space weights of each tile column / row (`SCALE · TILE` entries each).
    wx: Vec<Vec<f32>>,
    wy: Vec<Vec<f32>>,
    /// Sum of all tiles' weights at each output column / row.
    sx: Vec<f32>,
    sy: Vec<f32>,
    /// sRGB-encoded input (f16 bits, interleaved RGB; a BASE is not clamped). An
    /// encoded input is released once every tile was read (its rows don't need it).
    input: RwLock<Vec<u16>>,
    /// Tiles read (each counted once).
    read: Vec<AtomicBool>,
    reads: AtomicUsize,
    domain: Domain,
    priority: Priority,
    band: Mutex<Band>,
    written: Vec<AtomicBool>,
}

impl UpscaleJob {
    /// Demosaic `raw` at full resolution (on the pipeline's pool for `priority`) and
    /// encode it for the model.
    pub fn from_raw(raw: &RawImage, priority: Priority) -> Result<Self, PipelineError> {
        check_output(raw.size)?;
        let (input, mults) = encode_base(raw, priority)?;
        Ok(Self::new(raw.size, input, Domain::Linear(mults), priority))
    }

    /// An output-encoded 16-bit RGB image (an export's render) of `size`.
    pub fn from_encoded(
        size: Size,
        rgb: &[u16],
        priority: Priority,
    ) -> Result<Self, PipelineError> {
        check_output(size)?;
        if rgb.len() != size.pixels() as usize * 3 {
            return Err(PipelineError::InvalidRequest(format!(
                "{} samples for a {size:?} RGB image",
                rgb.len()
            )));
        }
        let input = crate::par::run_for(priority, || {
            rgb.par_iter()
                .map(|v| f16::from_f32(f32::from(*v) / WHITE).to_bits())
                .collect()
        });
        Ok(Self::new(size, input, Domain::Encoded, priority))
    }

    fn new(size: Size, input: Vec<u16>, domain: Domain, priority: Priority) -> Self {
        let xs = positions(size.width, TILE, OVERLAP);
        let ys = positions(size.height, TILE, OVERLAP);
        let out = Size::new(size.width * SCALE, size.height * SCALE);
        let scaled = |p: &[u32]| p.iter().map(|v| v * SCALE).collect::<Vec<_>>();
        let (oxs, oys) = (scaled(&xs), scaled(&ys));
        let (ot, om) = (TILE * SCALE, MARGIN * SCALE);
        let weights = |pos: &[u32]| -> Vec<Vec<f32>> {
            (0..pos.len())
                .map(|i| {
                    (0..ot)
                        .map(|d| ramp_weight(pos, i, ot, om, pos[i] + d))
                        .collect()
                })
                .collect()
        };
        let sums = |pos: &[u32], len: u32| -> Vec<f32> {
            (0..len)
                .map(|x| (0..pos.len()).map(|i| ramp_weight(pos, i, ot, om, x)).sum())
                .collect()
        };
        let written = (0..xs.len() * ys.len())
            .map(|_| AtomicBool::new(false))
            .collect();
        let read = (0..xs.len() * ys.len())
            .map(|_| AtomicBool::new(false))
            .collect();
        Self {
            size,
            out,
            wx: weights(&oxs),
            wy: weights(&oys),
            sx: sums(&oxs, out.width),
            sy: sums(&oys, out.height),
            band: Mutex::new(Band {
                top: 0,
                acc: Vec::new(),
                done: vec![0; ys.len()],
            }),
            xs,
            ys,
            input: RwLock::new(input),
            read,
            reads: AtomicUsize::new(0),
            domain,
            priority,
            written,
        }
    }

    /// Input size.
    pub fn size(&self) -> Size {
        self.size
    }

    /// Output size: `SCALE` × the input's.
    pub fn output_size(&self) -> Size {
        self.out
    }

    /// Number of tiles (row-major).
    pub fn tile_count(&self) -> usize {
        self.xs.len() * self.ys.len()
    }

    /// Elements of one input tile buffer: 3 planes of `TILE`² (`f16`).
    pub fn tile_len(&self) -> usize {
        3 * (TILE as usize).pow(2)
    }

    /// Top-left of input tile `i`.
    pub fn origin(&self, i: usize) -> (u32, u32) {
        (self.xs[i % self.xs.len()], self.ys[i / self.xs.len()])
    }

    fn check(&self, i: usize) -> Result<(), PipelineError> {
        if i >= self.tile_count() {
            return Err(PipelineError::InvalidRequest(format!(
                "tile {i} of {}",
                self.tile_count()
            )));
        }
        Ok(())
    }

    /// The model input of tile `i`: planar RGB (`[3][TILE][TILE]`, `f16` bits), encoded,
    /// clamped to 0..1, mirrored past the image edge (images smaller than a tile).
    pub fn read_tile(&self, i: usize, out: &mut [u16]) -> Result<(), PipelineError> {
        self.check(i)?;
        if out.len() < self.tile_len() {
            return Err(PipelineError::InvalidRequest(format!(
                "tile buffer of {} elements, {} needed",
                out.len(),
                self.tile_len()
            )));
        }
        let t = TILE as usize;
        let (x0, y0) = self.origin(i);
        let (w, h) = (self.size.width, self.size.height);
        let one = f16::ONE.to_bits();
        let input = self.input.read().unwrap_or_else(PoisonError::into_inner);
        if input.is_empty() {
            return Err(PipelineError::InvalidRequest(format!(
                "tile {i} read after every tile was"
            )));
        }
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
                let s = &input[(y * w as usize + x) * 3..][..3];
                // f16 bit patterns of non-negative values order like the values.
                rr[tx] = s[0].min(one);
                gr[tx] = s[1].min(one);
                br[tx] = s[2].min(one);
            }
        }
        drop(input);
        let first = !self.read[i].swap(true, Ordering::AcqRel);
        if first
            && self.reads.fetch_add(1, Ordering::AcqRel) + 1 == self.tile_count()
            && self.domain == Domain::Encoded
        {
            // An export's 2× pass: the render's f16 copy is no longer needed.
            *self.input.write().unwrap_or_else(PoisonError::into_inner) = Vec::new();
        }
        Ok(())
    }

    /// Bytes of input held (an encoded input is released once every tile was read).
    pub fn input_bytes(&self) -> usize {
        self.input
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
            * 2
    }

    /// The model output of tile `i`: `[3][2·TILE][2·TILE]` `f16` bits, element `(c, y, x)`
    /// at `c·channel_stride + y·row_stride + x` (Core ML may pad rows). Each tile is taken
    /// once; writing it again is an error (its first output is kept).
    pub fn write_tile(
        &self,
        i: usize,
        data: &[u16],
        row_stride: usize,
        channel_stride: usize,
    ) -> Result<(), PipelineError> {
        self.check(i)?;
        let t = (TILE * SCALE) as usize;
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
        let (xi, yi) = (i % self.xs.len(), i / self.xs.len());
        let (ox, oy) = (self.xs[xi] * SCALE, self.ys[yi] * SCALE);
        let (wx, wy) = (&self.wx[xi], &self.wy[yi]);
        let ow = self.out.width as usize;
        // Only the part of the tile inside the image (a mirrored tile's rest is padding).
        let tw = (self.out.width - ox).min(t as u32) as usize;
        let bottom = (oy + t as u32).min(self.out.height);
        let mut band = self.band.lock().unwrap_or_else(PoisonError::into_inner);
        // Every row above `top` was handed out, so all tiles covering it were written.
        debug_assert!(oy >= band.top);
        let held = (bottom - band.top) as usize * ow * 3;
        if band.acc.len() < held {
            band.acc.resize(held, 0.0);
        }
        let top = band.top as usize;
        for ty in 0..(bottom - oy) as usize {
            let row = (oy as usize + ty - top) * ow + ox as usize;
            let dst = &mut band.acc[row * 3..(row + tw) * 3];
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
        band.done[yi] += 1;
        Ok(())
    }

    /// Tiles not written yet.
    pub fn missing(&self) -> usize {
        self.written
            .iter()
            .filter(|w| !w.load(Ordering::Acquire))
            .count()
    }

    /// Output rows not handed out yet.
    pub fn rows_left(&self) -> u32 {
        self.out.height - self.band.lock().unwrap_or_else(PoisonError::into_inner).top
    }

    /// The next stitched output rows: every row whose tiles are all written and that
    /// wasn't handed out yet, in order (`None` when there are none right now). Callers
    /// that write the rows somewhere serialize their `take_rows` + write.
    pub fn take_rows(&self) -> Option<Rows> {
        let (y0, acc) = {
            let mut band = self.band.lock().unwrap_or_else(PoisonError::into_inner);
            let n = self.xs.len() as u32;
            let ready = band
                .done
                .iter()
                .position(|&d| d < n)
                .map_or(self.out.height, |r| self.ys[r] * SCALE);
            if ready <= band.top {
                return None;
            }
            let y0 = band.top;
            let len = (ready - y0) as usize * self.out.width as usize * 3;
            band.top = ready;
            (y0, band.acc.drain(..len).collect::<Vec<f32>>())
        };
        let ow = self.out.width as usize;
        let mut data = vec![0u16; acc.len()];
        let input = self.input.read().unwrap_or_else(PoisonError::into_inner);
        crate::par::run_for(self.priority, || {
            data.par_chunks_mut(ow * 3)
                .zip(acc.par_chunks(ow * 3))
                .enumerate()
                .for_each(|(r, (out, sums))| self.finish_row(&input, y0 + r as u32, sums, out));
        });
        drop(input);
        Some(Rows {
            y0,
            rows: (acc.len() / (ow * 3)) as u32,
            data,
        })
    }

    /// Normalize one stitched output row and convert it to the output domain.
    fn finish_row(&self, input: &[u16], y: u32, sums: &[f32], out: &mut [u16]) {
        let sy = self.sy[y as usize];
        let px = out
            .as_chunks_mut::<3>()
            .0
            .iter_mut()
            .zip(sums.as_chunks::<3>().0)
            .enumerate();
        match self.domain {
            Domain::Encoded => {
                for (x, (o, a)) in px {
                    let norm = 1.0 / (self.sx[x] * sy).max(f32::MIN_POSITIVE);
                    *o = a.map(|v| (v * norm * WHITE).clamp(0.0, WHITE).round() as u16);
                }
            }
            Domain::Linear(mults) => {
                let inv = mults.map(|m| WHITE / m);
                for (x, (o, a)) in px {
                    let norm = 1.0 / (self.sx[x] * sy).max(f32::MIN_POSITIVE);
                    // Clipped input: the original, interpolated (the model only saw 1).
                    let orig = self.sample(input, x as u32, y);
                    let peak = orig[0].max(orig[1]).max(orig[2]);
                    let t = ((peak - CLIP_START) / (CLIP_END - CLIP_START)).clamp(0.0, 1.0);
                    let model = 1.0 - t * t * (3.0 - 2.0 * t);
                    for c in 0..3 {
                        let v =
                            model * srgb_decode(a[c] * norm) + (1.0 - model) * srgb_decode(orig[c]);
                        o[c] = (v * inv[c]).clamp(0.0, WHITE).round() as u16;
                    }
                }
            }
        }
    }

    /// The input, bilinearly interpolated at the centre of output pixel `(x, y)`.
    fn sample(&self, input: &[u16], x: u32, y: u32) -> [f32; 3] {
        let coord = |p: u32, n: u32| -> (usize, usize, f32) {
            let f = ((p as f32 + 0.5) / SCALE as f32 - 0.5).clamp(0.0, (n - 1) as f32);
            let i = f as usize;
            (i, (i + 1).min(n as usize - 1), f - i as f32)
        };
        let (x0, x1, fx) = coord(x, self.size.width);
        let (y0, y1, fy) = coord(y, self.size.height);
        let w = self.size.width as usize;
        let at =
            |px: usize, py: usize, c: usize| f16::from_bits(input[(py * w + px) * 3 + c]).to_f32();
        [0, 1, 2].map(|c| {
            let top = at(x0, y0, c) + (at(x1, y0, c) - at(x0, y0, c)) * fx;
            let bottom = at(x0, y1, c) + (at(x1, y1, c) - at(x0, y1, c)) * fx;
            top + (bottom - top) * fy
        })
    }
}

fn check_output(size: Size) -> Result<(), PipelineError> {
    let out = size.pixels() * u64::from(SCALE * SCALE);
    if size.width == 0 || size.height == 0 {
        return Err(PipelineError::InvalidRequest(format!(
            "empty image {size:?}"
        )));
    }
    if out > MAX_OUTPUT_PIXELS {
        return Err(PipelineError::InvalidRequest(format!(
            "a {:.0} MP upscale is larger than the {} MP limit",
            out as f64 / 1e6,
            MAX_OUTPUT_PIXELS / 1_000_000
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "upscale_tests.rs"]
mod tests;
