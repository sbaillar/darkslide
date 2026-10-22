//! CPU reference renderer: rayon row-parallel f32 implementation of every stage.

use ds_core::{Adjustments, RawImage};
use rayon::prelude::*;

use std::sync::{Arc, Mutex};

use crate::cache::{BaseCache, Resident, build_rect};
use crate::color::{Mat3, gamut_compress, mat_vec, srgb_decode, srgb_encode};
use crate::color_edit::{apply_balance, apply_ranges, apply_saturation, apply_vibrance, luma};
use crate::demosaic::{self, BaseImage};
use crate::detail::{
    dehaze_px, from_luma_chroma, garrote, haze_guide, haze_local, local_contrast_delta,
    sharpen_delta, sharpen_threshold, texture_delta,
};
use crate::geometry;
use crate::local::{Plane, gaussian_blur, guided_haze, guided_self, upsample_off};
use crate::lut::{lookup_enc, tone_rgb, untone_rgb};
use crate::mask::{self, CompiledMask, RasterCache};
use crate::plan::{self, DemosaicMode, LayerPlan, Plan};
use crate::tone::{self, MID_GRAY};
use crate::{CancelToken, ImageKey, OutputFormat, PipelineError, RenderRequest, Renderer};
use ds_core::Size;

impl Resident for BaseImage {
    fn bytes(&self) -> u64 {
        self.bytes() as u64
    }
    fn rect(&self) -> [u32; 4] {
        [self.x0, self.y0, self.w, self.h]
    }
}

/// The level-grid rectangle the view needs (only meaningful for full-res RCD levels;
/// other levels are always built whole).
pub(crate) fn level_need(plan: &Plan, full: [u32; 2]) -> [u32; 4] {
    if plan.base_key.mode == DemosaicMode::Rcd && plan.base_key.down == 0 {
        plan.sensor_bbox
    } else {
        [0, 0, full[0], full[1]]
    }
}

/// Build the BASE image covering `rect` of its level.
pub(crate) fn build_base(raw: &RawImage, plan: &Plan, rect: [u32; 4]) -> BaseImage {
    let bk = plan.base_key;
    let mut img = match bk.mode {
        DemosaicMode::Superpixel => demosaic::superpixel(raw, &plan.base_params),
        DemosaicMode::Rcd if bk.down == 0 => demosaic::rcd_rect(raw, &plan.base_params, rect),
        DemosaicMode::Rcd => demosaic::rcd(raw, &plan.base_params),
    };
    for _ in 0..bk.down {
        img = demosaic::downsample2(&img);
    }
    img
}

/// The CPU reference renderer. `Clone + Send + Sync`: clones share one BASE and mask
/// raster cache, and render concurrently (the caches are locked only to look up and
/// insert, never while building or developing). Non-interactive renders (exports,
/// thumbnails) run on a pool with half the cores, so a viewer render on the same
/// renderer never waits for them.
#[derive(Debug, Clone, Default)]
pub struct CpuRenderer {
    shared: Arc<CpuShared>,
}

#[derive(Debug, Default)]
struct CpuShared {
    bases: Mutex<BaseCache<BaseImage>>,
    rasters: Mutex<RasterCache>,
}

fn lock<T>(m: &Mutex<T>) -> crate::par::Held<'_, T> {
    crate::par::hold(m)
}

impl CpuRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of BASE builds so far (cache misses).
    pub fn base_builds(&self) -> u64 {
        lock(&self.shared.bases).builds
    }

    /// Number of cached BASE images.
    pub fn cached_bases(&self) -> usize {
        lock(&self.shared.bases).len()
    }

    /// Number of brush-mask rasterizations so far (raster cache misses).
    pub fn raster_misses(&self) -> u64 {
        lock(&self.shared.rasters).misses
    }

    fn develop(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
    ) -> Result<(Plan, Vec<[f32; 3]>), PipelineError> {
        req.check_cancel()?;
        let plan = plan::build(raw, adj, req)?;
        let full = plan.level_size;
        let need = level_need(&plan, full);
        // Built outside the lock: other renders keep using the cache meanwhile.
        let cached = lock(&self.shared.bases).get(key, plan.base_key, need);
        let base = if let Some(b) = cached {
            b
        } else {
            let img = build_base(raw, &plan, build_rect(plan.base_key, need, full));
            lock(&self.shared.bases).insert(key, plan.base_key, img)
        };
        let base = base.get();
        let masks: Vec<CompiledMask> = plan
            .layers
            .iter()
            .map(|lp| {
                let mut cm = mask::compile(
                    &adj.layers[lp.index].mask,
                    adj.layers[lp.index].id,
                    &plan.geo,
                    req.masks.as_ref(),
                    key.0,
                    &self.shared.rasters,
                );
                mask::resolve_feathers(&mut cm, &self.shared.rasters);
                cm
            })
            .collect();
        req.check_cancel()?;
        let out = develop(base, &plan, &masks, req.cancel.as_ref())?;
        Ok((plan, out))
    }
}

impl Renderer for CpuRenderer {
    fn render_frame_into(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
        format: OutputFormat,
        out: &mut [u8],
    ) -> Result<crate::RenderedFrame, PipelineError> {
        // Content-aware removals: render the RAW with their fills composited in.
        let removed = crate::removal::resolve(key, raw, adj, req)?;
        let (key, raw) = match &removed {
            Some((k, r)) => (*k, r.as_ref()),
            None => (key, raw),
        };
        if req.priority == crate::Priority::Interactive {
            self.render_now(key, raw, adj, req, format, out)
        } else {
            // Exports and thumbnails leave half the cores to the viewer.
            crate::par::run_low(|| self.render_now(key, raw, adj, req, format, out))
        }
    }

    fn evict(&mut self, key: ImageKey) {
        self.evict_key(key);
    }
}

impl CpuRenderer {
    fn render_now(
        &mut self,
        key: ImageKey,
        raw: &RawImage,
        adj: &Adjustments,
        req: &RenderRequest,
        format: OutputFormat,
        out: &mut [u8],
    ) -> Result<crate::RenderedFrame, PipelineError> {
        let size = crate::output_size(raw, adj, req)?;
        crate::check_out(size, format, out)?;
        let (plan, mut enc) = self.develop(key, raw, adj, req)?;
        let exported = plan.resize.is_some() || plan.out_sharpen.is_some();
        if exported && let Some(g) = &plan.grain {
            // Exports: grain at the developed resolution (as the GPU's export tiles add it).
            grain_developed(&mut enc, &plan, g);
        }
        let enc = match plan.window {
            Some([x, y, w, h]) => {
                let full = plan.geo.out.width as usize;
                let mut win = Vec::with_capacity(w as usize * h as usize);
                for r in y as usize..(y + h) as usize {
                    win.extend_from_slice(&enc[r * full + x as usize..r * full + (x + w) as usize]);
                }
                win
            }
            None => enc,
        };
        let mut enc = match plan.upscale {
            Some(to) => {
                let [_, _, w, h] = plan.out_window();
                upscale_bilinear(&enc, w, h, to)
            }
            None => enc,
        };
        if !exported && let Some(g) = &plan.grain {
            grain_output(&mut enc, &plan, g);
        }
        // The format's output transform (matrix into the output space).
        let mut m = match format {
            OutputFormat::Rgba16fLinearP3 => {
                crate::color::working_to_output(f16_space(plan.output))
            }
            _ => plan.output_matrix,
        };
        let mut gamut = Gamut::Compress;
        let enc = if plan.resize.is_some() || plan.out_sharpen.is_some() {
            // Exports: every full-resolution pixel is output-transformed (as the full
            // render shows it), then resized in linear light and output-sharpened in the
            // output space; the final pass only encodes.
            let clamp = format != OutputFormat::Rgba16fLinearP3;
            let lin: Vec<[f32; 3]> = enc
                .par_iter()
                .map(|v| {
                    let l = output_linear_p3(*v, &m);
                    if clamp { l.map(|c| c.min(1.0)) } else { l }
                })
                .collect();
            let [_, _, w, h] = plan.out_window();
            let size = plan.out_size();
            let enc = resize_linear(&lin, w, h, size);
            m = crate::color::IDENTITY;
            gamut = Gamut::Mapped;
            match &plan.out_sharpen {
                Some(sp) => output_sharpen(enc, size.width as usize, size.height as usize, sp),
                None => enc,
            }
        } else {
            enc
        };
        finish_into(&enc, &m, gamut, format, out);
        let histogram = req.histogram.then(|| {
            // The 8-bit display codes of the output, in its output space.
            crate::Histogram::sum(
                &enc.par_chunks(4096)
                    .map(|c| {
                        let mut h = crate::Histogram::default();
                        for v in c {
                            let o = output_pixel_g(*v, &m, gamut).map(quantize8);
                            h.add(o[0], o[1], o[2]);
                        }
                        h
                    })
                    .collect::<Vec<_>>(),
            )
        });
        Ok(crate::RenderedFrame {
            size: plan.out_size(),
            histogram,
        })
    }

    fn evict_key(&self, key: ImageKey) {
        for k in std::iter::once(key).chain(crate::removal::forget(key)) {
            lock(&self.shared.bases).evict(k);
            lock(&self.shared.rasters).evict(k.0);
        }
    }
}

// ------------------------------------------------------------------ output ---

/// Grain on the whole developed image (export path), at developed pixel centres.
fn grain_developed(enc: &mut [[f32; 3]], plan: &Plan, g: &crate::effects::GrainPlan) {
    let geo = &plan.geo;
    let (norm, iso, long) = (geo.to_norm.to_f32(), geo.iso(), geo.long_edge_out());
    let w = geo.out.width as usize;
    enc.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, e) in row.iter_mut().enumerate() {
            let p = crate::effects::grain_pos(&norm, iso, long, x as f32 + 0.5, y as f32 + 0.5);
            *e = crate::effects::grain_px(*e, p, g);
        }
    });
}

/// Grain on the output window (preview path): each output pixel at the developed
/// position it shows (the draft upscale's bilinear source position), as the GPU output
/// pass computes it.
fn grain_output(out: &mut [[f32; 3]], plan: &Plan, g: &crate::effects::GrainPlan) {
    let geo = &plan.geo;
    let (norm, iso, long) = (geo.to_norm.to_f32(), geo.iso(), geo.long_edge_out());
    let [wx, wy, ww, wh] = plan.out_window();
    let size = plan.out_size();
    let up = plan.upscale;
    out.par_chunks_mut(size.width as usize)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, e) in row.iter_mut().enumerate() {
                let (dx, dy) = match up {
                    Some(to) => (
                        ((x as f32 + 0.5) * ww as f32 / to.width as f32 - 0.5)
                            .clamp(0.0, (ww - 1) as f32),
                        ((y as f32 + 0.5) * wh as f32 / to.height as f32 - 0.5)
                            .clamp(0.0, (wh - 1) as f32),
                    ),
                    None => (x as f32, y as f32),
                };
                let p = crate::effects::grain_pos(
                    &norm,
                    iso,
                    long,
                    dx + wx as f32 + 0.5,
                    dy + wy as f32 + 0.5,
                );
                *e = crate::effects::grain_px(*e, p, g);
            }
        });
}

#[inline]
fn catmull_rom(x: f32) -> f32 {
    let x = x.abs();
    if x < 1.0 {
        1.5 * x * x * x - 2.5 * x * x + 1.0
    } else if x < 2.0 {
        -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0
    } else {
        0.0
    }
}

/// Area-correct Catmull-Rom taps (first source index, normalized weights) of output
/// index `o` for a `src → dst` resize (mirrors the GPU `resize` kernel).
#[inline]
pub(crate) fn resize_taps(src: u32, dst: u32, o: u32) -> (usize, Vec<f32>) {
    let ratio = src as f32 / dst as f32;
    let fscale = ratio.max(1.0);
    let center = (o as f32 + 0.5) * ratio;
    let lo = (center - 2.0 * fscale).floor().max(0.0) as usize;
    let hi = ((center + 2.0 * fscale).ceil() as usize).min(src as usize);
    let mut w: Vec<f32> = (lo..hi)
        .map(|i| catmull_rom((i as f32 + 0.5 - center) / fscale))
        .collect();
    let sum: f32 = w.iter().sum();
    if sum.abs() > f32::EPSILON {
        for v in &mut w {
            *v /= sum;
        }
    }
    (lo, w)
}

/// Resize a linear image (area-correct Catmull-Rom); returns it encoded.
pub(crate) fn resize_linear(lin: &[[f32; 3]], w: u32, h: u32, to: Size) -> Vec<[f32; 3]> {
    // Horizontal, then vertical.
    let (tw, th) = (to.width, to.height);
    let xt: Vec<(usize, Vec<f32>)> = (0..tw).map(|o| resize_taps(w, tw, o)).collect();
    let mut tmp = vec![[0.0f32; 3]; (tw * h) as usize];
    tmp.par_chunks_mut(tw as usize)
        .enumerate()
        .for_each(|(y, row)| {
            let src = &lin[y * w as usize..(y + 1) * w as usize];
            for (o, (lo, wt)) in row.iter_mut().zip(&xt) {
                let mut acc = [0.0f32; 3];
                for (k, wv) in wt.iter().enumerate() {
                    let p = src[lo + k];
                    for c in 0..3 {
                        acc[c] += wv * p[c];
                    }
                }
                *o = acc;
            }
        });
    let mut out = vec![[0.0f32; 3]; to.pixels() as usize];
    out.par_chunks_mut(tw as usize)
        .enumerate()
        .for_each(|(y, row)| {
            let (lo, wt) = resize_taps(h, th, y as u32);
            for (x, o) in row.iter_mut().enumerate() {
                let mut acc = [0.0f32; 3];
                for (k, wv) in wt.iter().enumerate() {
                    let p = tmp[(lo + k) * tw as usize + x];
                    for c in 0..3 {
                        acc[c] += wv * p[c];
                    }
                }
                *o = acc.map(srgb_encode);
            }
        });
    out
}

/// Unsharp mask on (encoded) luma at the output size.
fn output_sharpen(
    mut enc: Vec<[f32; 3]>,
    w: usize,
    h: usize,
    sp: &crate::plan::SharpenPlan,
) -> Vec<[f32; 3]> {
    let y = Plane {
        w,
        h,
        data: enc.iter().map(|p| luma(*p)).collect(),
    };
    let blur = gaussian_blur(&y, &sp.kernel);
    enc.par_iter_mut().enumerate().for_each(|(i, p)| {
        let d = sp.gain * sharpen_threshold(y.data[i] - blur.data[i], sp.threshold);
        *p = p.map(|v| v + d);
    });
    enc
}

/// Bilinear upscale of a `w × h` image to `to` (pixel centers aligned; mirrors the GPU
/// output kernel's draft path).
pub(crate) fn upscale_bilinear(src: &[[f32; 3]], w: u32, h: u32, to: Size) -> Vec<[f32; 3]> {
    let (sx, sy) = (w as f32 / to.width as f32, h as f32 / to.height as f32);
    let (wl, hl) = ((w - 1) as f32, (h - 1) as f32);
    let mut out = vec![[0.0f32; 3]; to.pixels() as usize];
    out.par_chunks_mut(to.width as usize)
        .enumerate()
        .for_each(|(y, row)| {
            let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, hl);
            let (y0, ty) = (fy as u32, fy.fract());
            let y1 = (y0 + 1).min(h - 1);
            for (x, o) in row.iter_mut().enumerate() {
                let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, wl);
                let (x0, tx) = (fx as u32, fx.fract());
                let x1 = (x0 + 1).min(w - 1);
                let at = |xx: u32, yy: u32| src[(yy * w + xx) as usize];
                let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
                *o = [0, 1, 2].map(|k| {
                    let top = a[k] + (b[k] - a[k]) * tx;
                    let bot = c[k] + (d[k] - c[k]) * tx;
                    top + (bot - top) * ty
                });
            }
        });
    out
}

/// The primaries of `OutputFormat::Rgba16fLinearP3`: Display P3, or Rec.2020 when
/// that was asked for (the soft-proofing viewer needs colours outside P3).
pub(crate) fn f16_space(output: crate::OutputSpace) -> crate::OutputSpace {
    match output {
        crate::OutputSpace::Rec2020 => crate::OutputSpace::Rec2020,
        _ => crate::OutputSpace::DisplayP3,
    }
}

/// How the final pass maps values into the output gamut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gamut {
    /// Working-space values: matrix into the output space, then soft gamut compression.
    Compress,
    /// Already output-space and gamut-mapped (resized exports): only negatives from
    /// resampling ringing are cut. `gamut_compress` isn't idempotent, so it must not run
    /// twice.
    Mapped,
}

/// Encoded values → linear output primaries (matrix `m`), mapped into the gamut.
#[inline]
fn output_lin(enc: [f32; 3], m: &Mat3, g: Gamut) -> [f32; 3] {
    let lin = mat_vec(m, enc.map(srgb_decode)).map(|v| if v.is_nan() { 0.0 } else { v });
    match g {
        Gamut::Compress => gamut_compress(lin),
        Gamut::Mapped => lin.map(|v| v.max(0.0)),
    }
}

/// Encoded values → linear output primaries → gamut → clip → encode.
#[inline]
fn output_pixel_g(enc: [f32; 3], m: &Mat3, g: Gamut) -> [f32; 3] {
    output_lin(enc, m, g).map(|v| srgb_encode(v.min(1.0)))
}

/// Encoded Rec.2020 → linear Display P3, extended range (negatives soft-compressed into
/// gamut, nothing clipped above 1).
#[inline]
pub(crate) fn output_linear_p3(enc: [f32; 3], m: &Mat3) -> [f32; 3] {
    output_lin(enc, m, Gamut::Compress)
}

/// Write the developed image in `format` (native-endian for 16-bit formats).
/// `m` is the format's output matrix (Display P3 for the linear f16 format).
pub(crate) fn finish_into(
    enc: &[[f32; 3]],
    m: &Mat3,
    g: Gamut,
    format: OutputFormat,
    out: &mut [u8],
) {
    let bpp = format.bytes_per_pixel();
    out[..enc.len() * bpp]
        .par_chunks_mut(bpp * 1024)
        .zip(enc.par_chunks(1024))
        .for_each(|(o, e)| {
            for (px, v) in o.chunks_exact_mut(bpp).zip(e) {
                match format {
                    OutputFormat::Rgba8 => {
                        let c = output_pixel_g(*v, m, g);
                        px.copy_from_slice(&[
                            quantize8(c[0]),
                            quantize8(c[1]),
                            quantize8(c[2]),
                            255,
                        ]);
                    }
                    OutputFormat::Rgb16 => {
                        let c = output_pixel_g(*v, m, g);
                        for k in 0..3 {
                            let q = (c[k].clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
                            px[2 * k..2 * k + 2].copy_from_slice(&q.to_ne_bytes());
                        }
                    }
                    OutputFormat::Rgba16fLinearP3 => {
                        let c = output_lin(*v, m, g);
                        for (k, val) in [c[0], c[1], c[2], 1.0].into_iter().enumerate() {
                            px[2 * k..2 * k + 2]
                                .copy_from_slice(&half::f16::from_f32(val).to_ne_bytes());
                        }
                    }
                }
            }
        });
}

#[inline]
fn quantize8(v: f32) -> u8 {
    // NaN clamps to 0 via the saturating cast.
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

// ----------------------------------------------------------------- develop ---

fn luma_plane(px: &[[f32; 3]], w: usize, h: usize) -> Plane {
    Plane::from_fn(w, h, |x, y| luma(px[y * w + x]))
}

/// Scene-linear stage: resample, white balance, highlight reconstruction, camera
/// matrix, exposure, vignette, gamut pre-compression.
pub(crate) fn scene_linear(base: &BaseImage, plan: &Plan) -> Vec<[f32; 3]> {
    let geo = &plan.geo;
    let w = geo.out.width as usize;
    let (mut lin, lens_gain) = geometry::resample(base, geo, plan.level_size, plan.supersample);
    if !plan.spots.is_empty() {
        apply_spots(&mut lin, &lens_gain, base, plan);
    }
    let vig = geo.to_vignette.to_f32();
    let gain = plan.exposure_gain;
    let (ratio, clip, m) = (plan.wb_ratio, plan.clip, plan.cam_matrix);
    lin.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let py = y as f32 + 0.5;
        for (x, p) in row.iter_mut().enumerate() {
            let mut g = gain;
            if let Some((amount, mid)) = plan.vignette {
                let px = x as f32 + 0.5;
                let r =
                    (vig[0] * px + vig[1] * py + vig[2]).hypot(vig[3] * px + vig[4] * py + vig[5]);
                g *= tone::vignette_gain(r, amount, mid);
            }
            let cam = [p[0] * ratio[0], p[1] * ratio[1], p[2] * ratio[2]];
            let lg = lens_gain[y * w + x];
            let rgb = mat_vec(
                &m,
                demosaic::reconstruct_highlights(cam, clip).map(|v| v * lg),
            );
            *p = gamut_compress(rgb.map(|v| v * g));
        }
    });
    lin
}

/// Heal/clone spots on the resampled camera values (see [`crate::spots`]).
fn apply_spots(lin: &mut [[f32; 3]], gain: &[f32], base: &BaseImage, plan: &Plan) {
    use crate::spots::{bins, heal_gain, ring_points, sector_stats, weight};
    let geo = &plan.geo;
    let w = geo.out.width as usize;
    let level = plan.level_size;
    let sample = |p: [f64; 2]| geometry::sample_point_clamped(base, geo, level, p[0], p[1]);
    let stats: Vec<_> = plan
        .spots
        .iter()
        .map(|sp| {
            let ring = |source| {
                let s: Vec<_> = ring_points(sp, geo, source)
                    .into_iter()
                    .map(sample)
                    .collect();
                sector_stats(&s)
            };
            if sp.heal {
                (ring(false), ring(true))
            } else {
                Default::default()
            }
        })
        .collect();
    let bins = bins(&plan.spots, geo.out.width, geo.out.height);
    let iso = geo.iso();
    lin.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let py = y as f64 + 0.5;
        for (x, v) in row.iter_mut().enumerate() {
            let near = bins.at(x, y);
            if near.is_empty() {
                continue;
            }
            let px = x as f64 + 0.5;
            let (u, vv) = geo.norm_at(px, py);
            let p = [u as f32 * iso[0], vv as f32 * iso[1]];
            let g_dst = gain[y * w + x];
            for &k in near {
                let sp = &plan.spots[k as usize];
                let (wt, q, rho) = weight(sp, p);
                if wt <= 0.0 {
                    continue;
                }
                let (s, g, _) = sample([px + f64::from(sp.d[0]), py + f64::from(sp.d[1])]);
                let k_vg = g / g_dst.max(1e-6);
                let corr = if sp.heal {
                    let (dst, src) = &stats[k as usize];
                    heal_gain(dst, src, q, rho)
                } else {
                    [1.0; 3]
                };
                for c in 0..3 {
                    let val = s[c] * k_vg * corr[c];
                    v[c] += (val - v[c]) * wt;
                }
            }
        }
    });
}

/// log2 luminance clamped to the tone LUT floor.
#[inline]
pub(crate) fn log_luma(p: [f32; 3]) -> f32 {
    luma(p).max(1.525_878_9e-5).log2()
}

/// The full DEVELOP stage on an already-built base. Returns encoded Rec.2020.
pub(crate) fn develop(
    base: &BaseImage,
    plan: &Plan,
    masks: &[CompiledMask],
    cancel: Option<&CancelToken>,
) -> Result<Vec<[f32; 3]>, PipelineError> {
    let check = || match cancel {
        Some(c) if c.is_cancelled() => Err(PipelineError::Cancelled),
        _ => Ok(()),
    };
    let geo = &plan.geo;
    let (w, h) = (geo.out.width as usize, geo.out.height as usize);
    let mut lin = scene_linear(base, plan);
    if let Some(lb) = &plan.lens_blur {
        crate::effects::lens_blur(&mut lin, w, h, geo, lb);
        check()?;
    }

    // HDR: smoothed log luminance via guided filter; global shadows/highlights gain.
    let hdr_b: Option<Plane> = plan.needs_hdr().then(|| {
        let ly: Vec<f32> = lin.par_iter().map(|p| log_luma(*p)).collect();
        let gp = plan.hdr_gp;
        let (a, b) = guided_self(&ly, w, h, gp);
        Plane::from_fn(w, h, |x, y| {
            upsample_off(&a, gp.s, gp.off, x, y) * ly[y * w + x]
                + upsample_off(&b, gp.s, gp.off, x, y)
        })
    });
    // Dehaze: a patch dark channel (relative to the atmospheric light), refined by a
    // luminance-guided filter, estimates the haze at each pixel.
    let haze: Option<Vec<[f32; 3]>> = plan.needs_dehaze().then(|| {
        let a = plan.airlight;
        let gp = plan.dehaze_gp;
        let (ga, gb, gs, gm) = guided_haze(&lin, w, h, a, gp);
        (0..w * h)
            .into_par_iter()
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let up = |p: &Plane| upsample_off(p, gp.s, gp.off, x, y);
                let g = haze_guide(lin[i], a);
                let [smooth, mean] = haze_local(up(&gs), up(&gm), g);
                [(up(&ga) * g + up(&gb)).clamp(0.0, 1.0), smooth, mean]
            })
            .collect()
    });
    if let Some(hz) = &haze
        && plan.dehaze != 0.0
    {
        lin.par_iter_mut()
            .zip(hz.par_iter())
            .for_each(|(p, d)| *p = dehaze_px(*p, *d, plan.airlight, plan.dehaze));
    }
    let mid = MID_GRAY.log2();
    if let Some(bp) = &hdr_b
        && (plan.highlights != 0.0 || plan.shadows != 0.0)
    {
        lin.par_iter_mut()
            .zip(bp.data.par_iter())
            .for_each(|(p, b)| {
                let g = tone::hdr_gain_ev(b - mid, plan.highlights, plan.shadows).exp2();
                *p = p.map(|v| v * g);
            });
    }

    check()?;
    // Global tone + color.
    let luts = &plan.luts;
    let mut graded: Vec<[f32; 3]> = lin
        .par_iter()
        .map(|p| {
            let mut e = tone_rgb(plan.profile.apply(*p), &luts.base);
            if let Some([r, g, b]) = &luts.chan {
                e = [
                    lookup_enc(r, e[0]),
                    lookup_enc(g, e[1]),
                    lookup_enc(b, e[2]),
                ];
            }
            if let Some(lt) = &luts.luma {
                let y = luma(e);
                let d = lookup_enc(lt, y) - y;
                e = e.map(|v| v + d);
            }
            e = apply_balance(e, &plan.balance);
            if plan.saturation != 1.0 {
                e = apply_saturation(e, plan.saturation);
            }
            e = apply_vibrance(e, plan.vibrance);
            apply_ranges(e, &plan.ranges)
        })
        .collect();
    drop(lin);

    check()?;
    // Clarity / structure / texture detail layers (luma − guided smooth).
    // Each plane only when something uses it.
    let detail: Option<[Option<Plane>; 3]> = plan.needs_detail().then(|| {
        let y = luma_plane(&graded, w, h);
        let smooth = |gp: crate::local::GuidedParams| {
            let (a, b) = guided_self(&y.data, w, h, gp);
            Plane::from_fn(w, h, |xx, yy| {
                let v = y.data[yy * w + xx];
                v - (upsample_off(&a, gp.s, gp.off, xx, yy) * v
                    + upsample_off(&b, gp.s, gp.off, xx, yy))
            })
        };
        [
            plan.needs_clarity_plane().then(|| smooth(plan.clarity_gp)),
            plan.needs_structure_plane()
                .then(|| smooth(plan.structure_gp)),
            plan.needs_texture_plane().then(|| smooth(plan.texture_gp)),
        ]
    });
    let at = |k: usize, i: usize| {
        detail
            .as_ref()
            .and_then(|d| d[k].as_ref())
            .map_or(0.0, |p| p.data[i])
    };
    // Without a detail pass or layers `enc` is `graded` itself (no 12 B/px copy).
    let mut enc = if detail.is_none() && plan.layers.is_empty() {
        std::mem::take(&mut graded)
    } else {
        graded.clone()
    };
    if detail.is_some() && (plan.clarity != 0.0 || plan.structure != 0.0 || plan.texture != 0.0) {
        enc.par_iter_mut().enumerate().for_each(|(i, p)| {
            let y = luma(*p);
            let d = local_contrast_delta(y, at(0, i), at(1, i), plan.clarity, plan.structure)
                + texture_delta(y, at(2, i), plan.texture, plan.detail_noise);
            *p = p.map(|v| v + d);
        });
    }
    // Fine detail of the graded luma (5×5 Gaussian high-pass, σ in normalized units) for
    // layer sharpening.
    let fine: Option<Plane> = plan.needs_layer_sharpen().then(|| {
        let y = luma_plane(&graded, w, h);
        let (k, gain) = plan.fine;
        let b = blur5(&y, k);
        Plane::from_fn(w, h, |x, yy| {
            (y.data[yy * w + x] - b.data[yy * w + x]) * gain
        })
    });

    check()?;
    // Layers.
    for (lp, cm) in plan.layers.iter().zip(masks) {
        let cov = mask::evaluate(cm, geo, &graded);
        enc.par_iter_mut().enumerate().for_each(|(i, p)| {
            let m = mask::finalize(cov.data[i], cm, graded[i]) * lp.opacity;
            if m <= 0.0 {
                return;
            }
            let b = hdr_b.as_ref().map_or(0.0, |bp| bp.data[i]);
            let (dc, ds, dt) = (at(0, i), at(1, i), at(2, i));
            let local = LayerLocal {
                b_rel: b - mid,
                dc,
                ds,
                dt,
                haze: haze.as_ref().map_or([0.0; 3], |hz| hz[i]),
                airlight: plan.airlight,
                fine: fine.as_ref().map_or(0.0, |f| f.data[i]),
                noise: plan.detail_noise,
            };
            let o = apply_layer(*p, lp, &local, &plan.profile);
            *p = [0, 1, 2].map(|c| p[c] + (o[c] - p[c]) * m);
        });
    }
    drop(graded);
    if plan.fx.is_active() {
        crate::effects::apply_fx(&mut enc, w, h, &plan.fx);
    }

    check()?;
    // Noise reduction.
    let nr = &plan.nr;
    if nr.luma_active() || nr.chroma_active() {
        let y = luma_plane(&enc, w, h);
        let (b1, b2) = if nr.luma_active() {
            let b1 = blur121(&y, 1);
            let b2 = blur121(&b1, 2);
            (Some(b1), Some(b2))
        } else {
            (None, None)
        };
        let chroma = nr.chroma_active().then(|| {
            let cb: Vec<f32> = enc
                .par_iter()
                .zip(y.data.par_iter())
                .map(|(p, l)| p[2] - l)
                .collect();
            let cr: Vec<f32> = enc
                .par_iter()
                .zip(y.data.par_iter())
                .map(|(p, l)| p[0] - l)
                .collect();
            // Self-guided per chroma channel with ε = (expected chroma noise)²:
            // windows whose chroma varies well beyond the noise (texture, colour edges)
            // are kept, windows that only hold noise are flattened.
            let mut gp = nr.gp;
            gp.eps = (nr.chroma_sigma * nr.chroma_sigma).max(1e-10);
            (
                [guided_self(&cb, w, h, gp), guided_self(&cr, w, h, gp)],
                cb,
                cr,
            )
        });
        enc.par_chunks_mut(w).enumerate().for_each(|(yy, row)| {
            for (xx, p) in row.iter_mut().enumerate() {
                let i = yy * w + xx;
                let l = y.data[i];
                let nl = match (&b1, &b2) {
                    (Some(b1), Some(b2)) => {
                        let (v1, v2) = (b1.data[i], b2.data[i]);
                        v2 + garrote(v1 - v2, nr.t2) + garrote(l - v1, nr.t1)
                    }
                    _ => l,
                };
                let (mut cb, mut cr) = (p[2] - l, p[0] - l);
                if let Some(([(a1, bb1), (a2, bb2)], _, _)) = &chroma {
                    let s = nr.gp.s;
                    let o = nr.gp.off;
                    let qb = upsample_off(a1, s, o, xx, yy) * cb + upsample_off(bb1, s, o, xx, yy);
                    let qr = upsample_off(a2, s, o, xx, yy) * cr + upsample_off(bb2, s, o, xx, yy);
                    cb += (qb - cb) * nr.chroma;
                    cr += (qr - cr) * nr.chroma;
                }
                *p = from_luma_chroma(nl, cb, cr);
            }
        });
    }

    check()?;
    // Sharpening (luma unsharp mask with threshold).
    if let Some(sp) = &plan.sharpen {
        let y = luma_plane(&enc, w, h);
        let blur = gaussian_blur(&y, &sp.kernel);
        enc.par_iter_mut().enumerate().for_each(|(i, p)| {
            let d = sp.gain * sharpen_threshold(y.data[i] - blur.data[i], sp.threshold);
            *p = p.map(|v| v + d);
        });
    }
    Ok(enc)
}

/// Separable [1 2 1]/4 blur with dilation `step` (à-trous), replicated edges.
pub(crate) fn blur121(p: &Plane, step: usize) -> Plane {
    let (w, h) = (p.w, p.h);
    let tmp = Plane::from_fn(w, h, |x, y| {
        let row = &p.data[y * w..(y + 1) * w];
        0.25 * (row[x.saturating_sub(step)] + 2.0 * row[x] + row[(x + step).min(w - 1)])
    });
    Plane::from_fn(w, h, |x, y| {
        0.25 * (tmp.data[y.saturating_sub(step) * w + x]
            + 2.0 * tmp.data[y * w + x]
            + tmp.data[(y + step).min(h - 1) * w + x])
    })
}

/// Separable 5-tap blur (weights center, ±1, ±2), replicated edges.
fn blur5(p: &Plane, k: [f32; 3]) -> Plane {
    let (w, h) = (p.w, p.h);
    let tap = |n: usize, i: usize, d: isize| (i as isize + d).clamp(0, n as isize - 1) as usize;
    let tmp = Plane::from_fn(w, h, |x, y| {
        let row = &p.data[y * w..(y + 1) * w];
        k[0] * row[x]
            + k[1] * (row[tap(w, x, -1)] + row[tap(w, x, 1)])
            + k[2] * (row[tap(w, x, -2)] + row[tap(w, x, 2)])
    });
    Plane::from_fn(w, h, |x, y| {
        let at = |yy: usize| tmp.data[yy * w + x];
        k[0] * at(y)
            + k[1] * (at(tap(h, y, -1)) + at(tap(h, y, 1)))
            + k[2] * (at(tap(h, y, -2)) + at(tap(h, y, 2)))
    })
}

/// What a layer needs from the neighbourhood of a pixel.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct LayerLocal {
    /// Smoothed log2 luminance relative to middle gray.
    pub b_rel: f32,
    /// Clarity / structure / texture detail.
    pub dc: f32,
    pub ds: f32,
    pub dt: f32,
    /// Haze estimate and smoothness, and the atmospheric light.
    pub haze: [f32; 3],
    pub airlight: [f32; 3],
    /// 5×5 high-pass of the graded luma (layer sharpening), and the detail noise.
    pub fine: f32,
    pub noise: f32,
}

/// A layer's adjustments on one encoded pixel.
pub(crate) fn apply_layer(
    p: [f32; 3],
    lp: &LayerPlan,
    l: &LayerLocal,
    profile: &crate::profile::ProfilePlan,
) -> [f32; 3] {
    let mut e = p;
    if lp.retone {
        let bl = &*profile.luts;
        let mut lin = untone_rgb(e, &bl.fwd, &bl.inv);
        if lp.dehaze != 0.0 {
            lin = dehaze_px(lin, l.haze, l.airlight, lp.dehaze);
        }
        let g = (lp.ev + tone::hdr_gain_ev(l.b_rel, lp.highlights, lp.shadows)).exp2();
        let mut lin = mat_vec(&lp.wb, lin).map(|v| v * g);
        if lp.whites_f != 1.0 {
            // Stops above middle gray stretched by the whites factor (as the global
            // curve does), applied to luminance so hues stay.
            let y = luma(lin).max(1e-9);
            if y > MID_GRAY {
                let k = (y / MID_GRAY).powf(lp.whites_f - 1.0);
                lin = lin.map(|v| v * k);
            }
        }
        e = tone_rgb(gamut_compress(lin), &bl.fwd);
        if lp.blacks != 0.0 {
            e = e.map(|v| tone::blacks_toe(v, lp.blacks));
        }
    }
    if lp.contrast_k != 1.0 {
        e = e.map(|v| tone::contrast(v, lp.contrast_k, profile.pivot));
    }
    if lp.brightness_g != 1.0 {
        e = e.map(|v| tone::brightness(v, lp.brightness_g));
    }
    if let Some(c) = &lp.curves {
        e = [
            lookup_enc(&c.chan[0], e[0]),
            lookup_enc(&c.chan[1], e[1]),
            lookup_enc(&c.chan[2], e[2]),
        ];
        if let Some(lt) = &c.luma {
            let y = luma(e);
            let d = lookup_enc(lt, y) - y;
            e = e.map(|v| v + d);
        }
    }
    if !lp.balance.is_identity() {
        e = apply_balance(e, &lp.balance);
    }
    if lp.saturation != 1.0 {
        e = apply_saturation(e, lp.saturation);
    }
    e = apply_vibrance(e, lp.vibrance);
    e = apply_ranges(e, &lp.ranges);
    if lp.uses_detail() {
        let y = luma(e);
        let d = local_contrast_delta(y, l.dc, l.ds, lp.clarity, lp.structure)
            + texture_delta(y, l.dt, lp.texture_amount(), l.noise);
        e = e.map(|v| v + d);
    }
    if lp.sharpening != 0.0 {
        let d = sharpen_delta(l.fine, lp.sharpening, l.noise);
        e = e.map(|v| v + d);
    }
    e
}
