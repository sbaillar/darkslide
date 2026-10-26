//! Auto tools: white balance, exposure/HDR/levels, straighten, and the histogram.

use lt_core::adjust::{Crop, Exposure, Hdr, Levels, WhiteBalance};
use lt_core::{Adjustments, NormRect, RawImage, Size};
use rayon::prelude::*;

use crate::color::{CameraColor, multipliers_to_wb, wb_to_multipliers};
use crate::color_edit::luma;
use crate::demosaic::site_info;
use crate::tone::MID_GRAY;
use crate::{ImageKey, Quality, RenderRequest, RenderedImage, Renderer, cpu_renderer};

/// 256-bin histograms of an 8-bit render plus clipping counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Histogram {
    pub r: [u32; 256],
    pub g: [u32; 256],
    pub b: [u32; 256],
    /// Rec.709 luma of the display values.
    pub l: [u32; 256],
    /// Pixels with at least one channel at 0 / at 255.
    pub clipped_shadows: u64,
    pub clipped_highlights: u64,
    /// Per-channel clip counts (R, G, B).
    pub clipped_low: [u64; 3],
    pub clipped_high: [u64; 3],
    pub total: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            r: [0; 256],
            g: [0; 256],
            b: [0; 256],
            l: [0; 256],
            clipped_shadows: 0,
            clipped_highlights: 0,
            clipped_low: [0; 3],
            clipped_high: [0; 3],
            total: 0,
        }
    }
}

impl Histogram {
    /// Number of `u32` counters in the GPU layout: R, G, B, L bins (256 each), then
    /// clipped-low R/G/B, clipped-high R/G/B, clipped shadows, clipped highlights, total.
    pub(crate) const GPU_WORDS: usize = 4 * 256 + 9;

    /// Count one pixel's 8-bit display codes.
    #[inline]
    pub(crate) fn add(&mut self, r: u8, g: u8, b: u8) {
        self.r[r as usize] += 1;
        self.g[g as usize] += 1;
        self.b[b as usize] += 1;
        // Rec.709 luma in integer arithmetic (bit-identical on the GPU).
        let l = (2126 * u32::from(r) + 7152 * u32::from(g) + 722 * u32::from(b) + 5000) / 10_000;
        self.l[l.min(255) as usize] += 1;
        let mut low = false;
        let mut high = false;
        for (c, v) in [r, g, b].into_iter().enumerate() {
            if v == 0 {
                self.clipped_low[c] += 1;
                low = true;
            }
            if v == 255 {
                self.clipped_high[c] += 1;
                high = true;
            }
        }
        self.clipped_shadows += u64::from(low);
        self.clipped_highlights += u64::from(high);
        self.total += 1;
    }

    /// From the GPU counters (see [`Self::GPU_WORDS`]).
    pub(crate) fn from_gpu(w: &[u32]) -> Self {
        let mut h = Self::default();
        h.r.copy_from_slice(&w[0..256]);
        h.g.copy_from_slice(&w[256..512]);
        h.b.copy_from_slice(&w[512..768]);
        h.l.copy_from_slice(&w[768..1024]);
        let c = &w[1024..];
        h.clipped_low = [c[0], c[1], c[2]].map(u64::from);
        h.clipped_high = [c[3], c[4], c[5]].map(u64::from);
        h.clipped_shadows = u64::from(c[6]);
        h.clipped_highlights = u64::from(c[7]);
        h.total = u64::from(c[8]);
        h
    }

    /// Sum per-chunk histograms (collected on the heap: the 4 KB structs are not passed
    /// through rayon's reduction by value, which overflows worker stacks in debug builds).
    pub(crate) fn sum(parts: &[Histogram]) -> Self {
        parts.iter().fold(Self::default(), Histogram::merge)
    }

    pub(crate) fn merge(mut self, o: &Histogram) -> Self {
        for i in 0..256 {
            self.r[i] += o.r[i];
            self.g[i] += o.g[i];
            self.b[i] += o.b[i];
            self.l[i] += o.l[i];
        }
        self.clipped_shadows += o.clipped_shadows;
        self.clipped_highlights += o.clipped_highlights;
        for c in 0..3 {
            self.clipped_low[c] += o.clipped_low[c];
            self.clipped_high[c] += o.clipped_high[c];
        }
        self.total += o.total;
        self
    }

    /// Value (0..=255) below which `frac` of the luma histogram lies.
    pub fn luma_percentile(&self, frac: f64) -> u8 {
        if self.total == 0 {
            return 0;
        }
        let target = (frac.clamp(0.0, 1.0) * self.total as f64).ceil() as u64;
        let mut acc = 0u64;
        for (i, &n) in self.l.iter().enumerate() {
            acc += u64::from(n);
            if acc >= target.max(1) {
                return i as u8;
            }
        }
        255
    }
}

/// Histogram of a rendered image.
pub fn histogram(img: &RenderedImage) -> Histogram {
    let parts = img
        .rgba8
        .par_chunks(4 * 4096)
        .map(|chunk| {
            let mut h = Histogram::default();
            for px in chunk.as_chunks::<4>().0 {
                h.add(px[0], px[1], px[2]);
            }
            h
        })
        .collect::<Vec<_>>();
    Histogram::sum(&parts)
}

/// Block-averaged camera RGB (black-subtracted, normalized, no WB), skipping blocks that
/// contain clipped samples. Returns `(rgb, max_sample)` per valid block.
/// Block size for the camera-space statistics (~256 blocks along the long edge).
pub(crate) fn block_size(raw: &RawImage) -> usize {
    ((raw.size.width.max(raw.size.height) / 256) as usize).clamp(2, 64) & !1
}

/// Mean camera RGB (normalized, not white balanced) of each `block × block` mosaic block,
/// skipping blocks with a clipped sample (≥ 0.97). The GPU computes the same blocks.
pub(crate) fn camera_blocks(raw: &RawImage, block: usize) -> Vec<[f32; 3]> {
    if raw.cfa == lt_core::CfaPattern::LinearRgb {
        return Vec::new();
    }
    let (w, h) = (raw.size.width as usize, raw.size.height as usize);
    let block = block.max(2) & !1;
    let si = site_info(raw);
    (0..h / block)
        .into_par_iter()
        .flat_map_iter(|by| {
            let si = &si;
            (0..w / block).filter_map(move |bx| {
                let mut acc = [0.0f32; 3];
                let mut n = [0.0f32; 3];
                for y in by * block..(by + 1) * block {
                    for x in bx * block..(bx + 1) * block {
                        let pos = (y & 1) << 1 | (x & 1);
                        let v =
                            (f32::from(raw.data[y * w + x]) - si.black[pos]) * si.inv_range[pos];
                        if v >= 0.97 {
                            return None;
                        }
                        let c = si.color[pos];
                        acc[c] += v.max(0.0);
                        n[c] += 1.0;
                    }
                }
                Some([0, 1, 2].map(|c| acc[c] / n[c].max(1.0)))
            })
        })
        .collect()
}

/// Camera-space block means for the auto tools and AI Optimize (empty for an invalid
/// raw); `GpuRenderer::block_means` computes the same on the GPU.
pub fn block_means(raw: &RawImage) -> Vec<[f32; 3]> {
    if raw.validate().is_err() {
        return Vec::new();
    }
    camera_blocks(raw, block_size(raw))
}

/// Automatic white balance that trusts the camera's as-shot setting unless the image
/// clearly shows it wrong (see [`wb_from_blocks`]). Always returns
/// [`WhiteBalance::Custom`]; falls back to the as-shot setting.
///
/// Measured on `raw` as given: content-aware removals (`Adjustments::removals`) are not
/// applied first, so a removed object still counts (pass the derived RAW to exclude it).
pub fn auto_white_balance(raw: &RawImage) -> WhiteBalance {
    if raw.validate().is_err() {
        return wb_from_blocks(raw, &[]);
    }
    wb_from_blocks(raw, &camera_blocks(raw, block_size(raw)))
}

/// Log-chroma radius within which a block counts as neutral under an illuminant.
const NEUTRAL_R: f32 = 0.06;
/// Weak-evidence gray world: σ across the Planckian locus and along it from the
/// as-shot's locus point, toward warmer and toward cooler light (blue sky and sea sit on
/// the cool side of the locus: they mustn't read as neutrals), and the "far from
/// as-shot" radius of strong evidence.
const GRAY_SIGMA: f32 = 0.06;
const GRAY_ALONG: [f32; 2] = [0.35, 0.08];
const GRAY_EXCLUDE: f32 = 0.2;
/// A white-patch cluster counts as a neutral surface when its chroma lies within this
/// of the Planckian locus (log-chroma), at a plausible illuminant.
const LOCUS_TOL: f32 = 0.035;
const LOCUS_K: [f32; 2] = [2400.0, 9000.0];
/// Weak evidence never moves the temperature further than this from as-shot (K).
const WEAK_MAX_K: f32 = 600.0;
const WEAK_MAX_TINT: f32 = 10.0;

/// Auto white balance from camera-space block means (see [`camera_blocks`]).
///
/// Blocks are measured as log-chroma relative to the as-shot neutral (so 0 = neutral
/// under the camera's white balance), weighted by luminance. Neutral candidates are the
/// modes of that distribution (tight clusters: several gray surfaces agree on one
/// illuminant); high-chroma regions (sky, sea, foliage) form clusters too, but neither
/// span the luminance range of real gray objects nor hold the white patch:
/// - **strong evidence** (the as-shot is far off): the densest tight cluster lies
///   outside the as-shot neighbourhood (> 0.2 log-chroma), holds ≥ 30% of the weight,
///   spanning ≥ 3× in luminance (gray objects of different reflectance, not one sky) and
///   holding at least half of the brightest blocks (the white patch agrees), is trusted
///   wherever it is (a wrong as-shot under tungsten, say);
/// - **white patch on the locus**: the cluster the brightest blocks form (a gray card,
///   fog, a white wall; one luminance is enough) is trusted when it holds ≥ 8% of the
///   weight and at least half of the brightest blocks, and its chroma lies on the
///   Planckian locus (within 0.035) at 2400–9000 K: coloured objects (a sunset, blue sky,
///   foliage) are off the locus or outside that range;
/// - otherwise a gray world weighted toward the locus (σ 0.06 across it; along it from
///   the as-shot's point 0.35 toward warmer light, so a wrong as-shot's true neutrals
///   under tungsten still count, and 0.08 toward cooler, where blue sky lies) is
///   applied in proportion to the near-neutral share of the image, and the result is
///   clamped to ±600 K / ±10 tint around as-shot.
pub(crate) fn wb_from_blocks(raw: &RawImage, blocks: &[[f32; 3]]) -> WhiteBalance {
    wb_estimate(raw, blocks).0
}

/// Which evidence [`wb_from_blocks`] trusted (AI Optimize explains it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WbEvidence {
    /// Too little usable data: the as-shot setting.
    AsShot,
    /// A dense multi-luminance neutral cluster far from as-shot.
    NeutralCluster,
    /// The white patch's cluster on the Planckian locus (a gray card, fog, a white wall).
    WhitePatch,
    /// Gray world weighted toward the locus; `weight` (0..=1) is the near-neutral share
    /// it was applied with, and the result was clamped to ±600 K / ±10 tint.
    GrayWorld { weight: f32 },
}

/// [`wb_from_blocks`] plus the evidence it used.
pub(crate) fn wb_estimate(raw: &RawImage, blocks: &[[f32; 3]]) -> (WhiteBalance, WbEvidence) {
    let cam = CameraColor::of(raw);
    let as_shot = || (multipliers_to_wb(raw.wb_as_shot, &cam), WbEvidence::AsShot);
    if raw.validate().is_err() {
        return (
            WhiteBalance::Custom {
                temp_k: 5500.0,
                tint: 0.0,
            },
            WbEvidence::AsShot,
        );
    }
    let a = raw.wb_as_shot;
    if !(a.iter().all(|v| v.is_finite() && *v > 0.0)) {
        return as_shot();
    }
    let (ar, ab) = (a[0] / a[1], a[2] / a[1]);
    // (log-chroma relative to the as-shot neutral, luminance weight, green level)
    let samples: Vec<([f32; 2], f32, f32)> = blocks
        .iter()
        .filter(|p| p.iter().all(|v| *v > 0.002) && p[1] > 0.004)
        .map(|p| {
            let c = [(p[0] * ar / p[1]).ln(), (p[2] * ab / p[1]).ln()];
            (c, p[1].sqrt(), p[1])
        })
        .filter(|(c, _, _)| c[0].is_finite() && c[1].is_finite())
        .collect();
    if samples.len() < 16 {
        return as_shot();
    }
    let total: f32 = samples.iter().map(|s| s.1).sum();
    // Strong evidence: the densest tight cluster, if it is big, spans several luminance
    // levels and holds the white patch (the brightest blocks).
    let support = support_map(&samples);
    let best = (0..GRID * GRID)
        .max_by(|a, b| support[*a].total_cmp(&support[*b]))
        .unwrap_or(0);
    let strong = cluster_stats(&samples, [grid_c(best / GRID), grid_c(best % GRID)], total);
    let white = white_patch_share(&samples, strong.center);
    let far = strong.center[0].hypot(strong.center[1]) > GRAY_EXCLUDE;
    let locus = locus(&cam, ar, ab);
    let patch = white_cluster(&samples, total);
    let (patch_k, patch_d) = on_locus(&locus, patch.center);
    let (shift, evidence) = if far && strong.frac >= 0.3 && strong.lum_span >= 3.0 && white >= 0.5 {
        (strong.center, WbEvidence::NeutralCluster)
    } else if patch.frac >= 0.08
        && white_patch_share(&samples, patch.center) >= 0.5
        && patch_d <= LOCUS_TOL
        && (LOCUS_K[0]..=LOCUS_K[1]).contains(&patch_k)
    {
        (patch.center, WbEvidence::WhitePatch)
    } else {
        // Weak evidence: gray world weighted toward the Planckian locus near the
        // as-shot (high-chroma regions count for little), applied in proportion to how
        // much of the image is near-neutral.
        let (k0, _) = on_locus(&locus, [0.0, 0.0]);
        let at = |k: f32| locus_at(&locus, k);
        let home = at(k0);
        let (mut acc, mut wsum) = ([0.0f32; 2], 0.0f32);
        for (c, w, _) in &samples {
            let (k, across) = on_locus(&locus, *c);
            let p = at(k);
            let along = (p[0] - home[0]).hypot(p[1] - home[1]);
            let s_along = if k <= k0 {
                GRAY_ALONG[0]
            } else {
                GRAY_ALONG[1]
            };
            let k = w
                * (-(across * across) / (2.0 * GRAY_SIGMA * GRAY_SIGMA)).exp()
                * (-(along * along) / (2.0 * s_along * s_along)).exp();
            acc[0] += k * c[0];
            acc[1] += k * c[1];
            wsum += k;
        }
        let e = if wsum > 0.0 {
            [acc[0] / wsum, acc[1] / wsum]
        } else {
            [0.0; 2]
        };
        let k = ((wsum / total.max(1e-12) - 0.05) / 0.3).clamp(0.0, 1.0);
        ([e[0] * k, e[1] * k], WbEvidence::GrayWorld { weight: k })
    };
    let clamp = matches!(evidence, WbEvidence::GrayWorld { .. });
    let mults = [ar * (-shift[0]).exp(), 1.0, ab * (-shift[1]).exp()];
    match (multipliers_to_wb(mults, &cam), as_shot().0) {
        (
            WhiteBalance::Custom { temp_k, tint },
            WhiteBalance::Custom {
                temp_k: t0,
                tint: n0,
            },
        ) if temp_k.is_finite() && tint.is_finite() => {
            let wb = if clamp {
                WhiteBalance::Custom {
                    temp_k: temp_k.clamp(t0 - WEAK_MAX_K, t0 + WEAK_MAX_K),
                    tint: tint.clamp(n0 - WEAK_MAX_TINT, n0 + WEAK_MAX_TINT),
                }
            } else {
                WhiteBalance::Custom { temp_k, tint }
            };
            (wb, evidence)
        }
        _ => as_shot(),
    }
}

/// The Planckian locus (tint 0) from 2000 to 12000 K (even in mired) as log-chroma
/// relative to the as-shot neutral `(ar, ab)`: `(K, chroma)`.
fn locus(cam: &CameraColor, ar: f32, ab: f32) -> Vec<(f32, [f32; 2])> {
    const N: usize = 96;
    let (m0, m1) = (1e6 / 2000.0, 1e6 / 12000.0);
    (0..=N)
        .map(|i| {
            let k = 1e6 / (m0 + (m1 - m0) * i as f32 / N as f32);
            let m = crate::color::temp_tint_to_multipliers(k, 0.0, cam);
            (k, [(ar / m[0]).ln(), (ab / m[2]).ln()])
        })
        .collect()
}

/// Nearest locus point to chroma `c`: its temperature and the distance.
fn on_locus(locus: &[(f32, [f32; 2])], c: [f32; 2]) -> (f32, f32) {
    locus
        .iter()
        .map(|(k, p)| (*k, (c[0] - p[0]).hypot(c[1] - p[1])))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap_or((0.0, f32::MAX))
}

/// The locus chroma at temperature `k` (its nearest sample).
fn locus_at(locus: &[(f32, [f32; 2])], k: f32) -> [f32; 2] {
    locus
        .iter()
        .min_by(|a, b| (a.0 - k).abs().total_cmp(&(b.0 - k).abs()))
        .map_or([0.0; 2], |p| p.1)
}

/// The cluster the brightest 3% of blocks form: the densest chroma among them, and the
/// stats of all blocks near it.
fn white_cluster(samples: &[([f32; 2], f32, f32)], total: f32) -> Cluster {
    let mut g: Vec<f32> = samples.iter().map(|s| s.2).collect();
    let n_top = (g.len() * 3 / 100).max(1);
    let idx = g.len() - n_top;
    let cut = *g.select_nth_unstable_by(idx, f32::total_cmp).1;
    let top: Vec<_> = samples.iter().filter(|s| s.2 >= cut).copied().collect();
    let support = support_map(&top);
    let best = (0..GRID * GRID)
        .max_by(|a, b| support[*a].total_cmp(&support[*b]))
        .unwrap_or(0);
    cluster_stats(samples, [grid_c(best / GRID), grid_c(best % GRID)], total)
}

/// Support grid over log-chroma: `GRID` bins of `STEP` centered on 0.
const GRID: usize = 121;
const STEP: f32 = 0.02;

fn grid_c(i: usize) -> f32 {
    (i as f32 - (GRID / 2) as f32) * STEP
}

/// Luminance-weighted histogram of the samples' log-chroma, blurred to the neutral
/// tolerance: `support[i * GRID + j]` ≈ weight within ~`NEUTRAL_R` of that chroma.
fn support_map(samples: &[([f32; 2], f32, f32)]) -> Vec<f32> {
    let mut h = vec![0.0f32; GRID * GRID];
    let half = (GRID / 2) as f32;
    for (c, w, _) in samples {
        let (i, j) = ((c[0] / STEP + half).round(), (c[1] / STEP + half).round());
        if (0.0..GRID as f32).contains(&i) && (0.0..GRID as f32).contains(&j) {
            h[i as usize * GRID + j as usize] += w;
        }
    }
    let sigma = NEUTRAL_R / STEP / 2.0;
    let r = (sigma * 3.0).ceil() as isize;
    let k: Vec<f32> = (-r..=r)
        .map(|d| (-(d * d) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let blur = |src: &[f32], vertical: bool| {
        let mut out = vec![0.0f32; GRID * GRID];
        for i in 0..GRID {
            for j in 0..GRID {
                let mut acc = 0.0;
                for (t, kv) in k.iter().enumerate() {
                    let d = t as isize - r;
                    let (ii, jj) = if vertical {
                        (i as isize + d, j as isize)
                    } else {
                        (i as isize, j as isize + d)
                    };
                    if (0..GRID as isize).contains(&ii) && (0..GRID as isize).contains(&jj) {
                        acc += kv * src[ii as usize * GRID + jj as usize];
                    }
                }
                out[i * GRID + j] = acc;
            }
        }
        out
    };
    blur(&blur(&h, false), true)
}

/// Share of the brightest 3% of blocks within `2 × NEUTRAL_R` of chroma `c`.
fn white_patch_share(samples: &[([f32; 2], f32, f32)], c: [f32; 2]) -> f32 {
    let mut g: Vec<f32> = samples.iter().map(|s| s.2).collect();
    let n_top = (g.len() * 3 / 100).max(1);
    let idx = g.len() - n_top;
    let cut = *g.select_nth_unstable_by(idx, f32::total_cmp).1;
    let top: Vec<_> = samples.iter().filter(|s| s.2 >= cut).collect();
    let near = top
        .iter()
        .filter(|s| (s.0[0] - c[0]).hypot(s.0[1] - c[1]) <= 2.0 * NEUTRAL_R)
        .count();
    near as f32 / top.len().max(1) as f32
}

struct Cluster {
    /// Weighted mean log-chroma of the blocks within `NEUTRAL_R`.
    center: [f32; 2],
    /// Their share of the total weight.
    frac: f32,
    /// 90th / 10th percentile of their green level.
    lum_span: f32,
}

fn cluster_stats(samples: &[([f32; 2], f32, f32)], c: [f32; 2], total: f32) -> Cluster {
    let near: Vec<&([f32; 2], f32, f32)> = samples
        .iter()
        .filter(|s| (s.0[0] - c[0]).hypot(s.0[1] - c[1]) <= NEUTRAL_R)
        .collect();
    let w: f32 = near.iter().map(|s| s.1).sum();
    if near.is_empty() || w <= 0.0 {
        return Cluster {
            center: c,
            frac: 0.0,
            lum_span: 1.0,
        };
    }
    let center = [
        near.iter().map(|s| s.0[0] * s.1).sum::<f32>() / w,
        near.iter().map(|s| s.0[1] * s.1).sum::<f32>() / w,
    ];
    let mut g: Vec<f32> = near.iter().map(|s| s.2).collect();
    g.sort_by(f32::total_cmp);
    let lum_span = percentile(&g, 0.9) / percentile(&g, 0.1).max(1e-6);
    Cluster {
        center,
        frac: w / total.max(1e-12),
        lum_span,
    }
}

fn percentile(sorted: &[f32], frac: f32) -> f32 {
    let i = ((sorted.len() - 1) as f32 * frac.clamp(0.0, 1.0)).round() as usize;
    sorted[i]
}

/// Automatic exposure, HDR and levels from histogram percentiles.
///
/// Exposure moves the log-average scene luminance toward middle gray without pushing the
/// 99.5th percentile past the curve's shoulder; highlights/shadows engage when the
/// extremes still fall outside the tonal range; levels then stretch the rendered
/// 0.1%/99.9% luma percentiles (bounded, so it never becomes a harsh auto-contrast).
/// Other fields of the returned structs are copied from `adj`.
///
/// Like [`auto_white_balance`], it measures `raw` without `adj.removals` applied.
pub fn auto_levels(raw: &RawImage, adj: &Adjustments) -> (Exposure, Hdr, Levels) {
    if raw.validate().is_err() {
        return (adj.exposure, adj.hdr, adj.levels);
    }
    let blocks = camera_blocks(raw, block_size(raw));
    levels_from(raw, adj, &blocks, |probe, req| {
        cpu_renderer()
            .render(ImageKey(0), raw, probe, req)
            .ok()
            .map(|img| histogram(&img))
    })
}

/// The probe render [`levels_from`] histograms: a 512 px `Draft` render.
pub(crate) fn levels_probe_request() -> RenderRequest {
    RenderRequest {
        quality: Quality::Draft,
        ..RenderRequest::new(Size::new(512, 512))
    }
}

/// Auto levels from camera-space block means and a histogram of a probe render
/// (`probe_histogram(adjustments, request)`), so the GPU can supply both.
pub(crate) fn levels_from(
    raw: &RawImage,
    adj: &Adjustments,
    blocks: &[[f32; 3]],
    probe_histogram: impl FnOnce(&Adjustments, &RenderRequest) -> Option<Histogram>,
) -> (Exposure, Hdr, Levels) {
    let mut exposure = adj.exposure;
    let mut hdr = adj.hdr;
    let mut levels = adj.levels;
    if raw.validate().is_err() {
        return (exposure, hdr, levels);
    }
    let Some(stats) = tone_stats(raw, adj.white_balance, blocks) else {
        return (exposure, hdr, levels);
    };
    exposure.exposure_ev = stats.exposure_for(MID_GRAY);
    (hdr.highlights, hdr.shadows) = stats.hdr_for(exposure.exposure_ev);

    let mut probe = adj.clone();
    probe.exposure = exposure;
    probe.hdr = hdr;
    probe.levels = Levels::default();
    probe.layers.clear();
    if let Some(h) = probe_histogram(&probe, &levels_probe_request()) {
        (levels.in_black, levels.in_white) = levels_for(&h);
    }
    (exposure, hdr, levels)
}

/// Scene luminance statistics of camera blocks under a white balance, in log2 of
/// linear luminance at exposure 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ToneStats {
    pub log_avg: f32,
    pub p02: f32,
    pub p995: f32,
}

/// [`ToneStats`] of `blocks` white balanced with `wb`; `None` with fewer than 16 blocks.
pub(crate) fn tone_stats(
    raw: &RawImage,
    wb: WhiteBalance,
    blocks: &[[f32; 3]],
) -> Option<ToneStats> {
    let mults = wb_to_multipliers(wb, raw);
    let matrix = crate::plan::cam_matrix(raw);
    let mut logs: Vec<f32> = blocks
        .iter()
        .map(|p| {
            let w =
                crate::color::mat_vec(&matrix, [p[0] * mults[0], p[1] * mults[1], p[2] * mults[2]]);
            luma(w).max(1e-6).log2()
        })
        .collect();
    if logs.len() < 16 {
        return None;
    }
    logs.sort_by(f32::total_cmp);
    Some(ToneStats {
        log_avg: logs.iter().sum::<f32>() / logs.len() as f32,
        p02: percentile(&logs, 0.02),
        p995: percentile(&logs, 0.995),
    })
}

impl ToneStats {
    /// The largest exposure (EV) that keeps the 99.5th percentile below the curve's
    /// shoulder (4 stops over middle gray).
    pub fn ev_max(&self) -> f32 {
        MID_GRAY.log2() + 4.0 - self.p995
    }

    /// Exposure (EV, 0.01 steps) that moves the log-average toward `target` (linear
    /// luminance; middle gray for auto tone), within ±3 EV and [`Self::ev_max`].
    pub fn exposure_for(&self, target: f32) -> f32 {
        let ev = (target.log2() - self.log_avg).clamp(-3.0, 3.0);
        (ev.min(self.ev_max()).max(-3.0) * 100.0).round() / 100.0
    }

    /// Stops the 99.5th percentile sits above the shoulder's start (2.5 stops over
    /// middle gray) at exposure `ev` (positive: highlights need compressing).
    pub fn highlight_excess(&self, ev: f32) -> f32 {
        self.p995 + ev - (MID_GRAY.log2() + 2.5)
    }

    /// Stops the 2nd percentile sits below the toe (6 stops under middle gray) at `ev`.
    pub fn shadow_deficit(&self, ev: f32) -> f32 {
        (MID_GRAY.log2() - 6.0) - (self.p02 + ev)
    }

    /// Auto tone's HDR highlights and shadows at exposure `ev`.
    pub fn hdr_for(&self, ev: f32) -> (f32, f32) {
        let over = self.highlight_excess(ev);
        let under = self.shadow_deficit(ev);
        (
            if over > 0.0 {
                (over * 30.0).clamp(0.0, 60.0).round()
            } else {
                0.0
            },
            if under > 0.0 {
                (under * 15.0).clamp(0.0, 50.0).round()
            } else {
                0.0
            },
        )
    }
}

/// Auto levels from a probe histogram: the 0.1% / 99.9% luma percentiles, bounded (black
/// ≤ 0.08, white ≥ 0.92, at least half the range kept).
pub(crate) fn levels_for(h: &Histogram) -> (f32, f32) {
    let lo = f32::from(h.luma_percentile(0.001)) / 255.0;
    let hi = f32::from(h.luma_percentile(0.999)) / 255.0;
    let (black, white) = (lo.min(0.08), hi.max(0.92));
    if white - black < 0.5 {
        (0.0, 1.0)
    } else {
        (black, white)
    }
}

/// Straighten from a detected horizon angle (degrees, counter-clockwise positive, as Apple
/// Vision reports it) and shrink the crop about its center so no empty corners show.
/// `size` is the E-frame (oriented) image size.
pub fn auto_straighten_from_horizon(horizon_deg: f32, crop: &Crop, size: Size) -> Crop {
    let mut out = *crop;
    if !horizon_deg.is_finite() || size.width == 0 || size.height == 0 {
        return out;
    }
    let angle = (-horizon_deg).clamp(-45.0, 45.0);
    out.angle_deg = angle;
    let (w, h) = (f64::from(size.width), f64::from(size.height));
    let r = crop.rect.clamped();
    let (cx, cy) = (
        (f64::from(r.x) + f64::from(r.w) * 0.5) * w,
        (f64::from(r.y) + f64::from(r.h) * 0.5) * h,
    );
    let (a, b) = (f64::from(r.w) * w * 0.5, f64::from(r.h) * h * 0.5);
    let (s, c) = f64::from(angle).to_radians().sin_cos();
    let (ex, ey) = (a * c.abs() + b * s.abs(), a * s.abs() + b * c.abs());
    let k = (cx.min(w - cx) / ex)
        .min(cy.min(h - cy) / ey)
        .clamp(0.0, 1.0);
    let (na, nb) = (a * k, b * k);
    out.rect = NormRect {
        x: ((cx - na) / w) as f32,
        y: ((cy - nb) / h) as f32,
        w: (2.0 * na / w) as f32,
        h: (2.0 * nb / h) as f32,
    }
    .clamped();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::temp_tint_to_multipliers;
    use crate::testutil::{gray_raw, noise, raw_from_fn};

    /// A varied scene (gray world holds on average) lit by a `temp_k` illuminant.
    fn lit_scene(temp_k: f32) -> RawImage {
        let mut raw = gray_raw(256, 192, 0.0);
        let m = temp_tint_to_multipliers(temp_k, 0.0, &CameraColor::of(&raw));
        let illum = [1.0 / m[0], 1.0 / m[1], 1.0 / m[2]];
        let k = 0.8 / illum.iter().fold(0.0f32, |a, v| a.max(*v));
        let (black, white) = (raw.black_level[0], raw.white_level);
        for y in 0..192u32 {
            for x in 0..256u32 {
                let patch = (x / 32 + (y / 32) * 8) as usize;
                let refl = [
                    [0.2, 0.2, 0.2],
                    [0.5, 0.3, 0.2],
                    [0.2, 0.4, 0.25],
                    [0.25, 0.3, 0.5],
                    [0.6, 0.6, 0.6],
                    [0.1, 0.1, 0.1],
                    [0.4, 0.35, 0.3],
                    [0.3, 0.35, 0.4],
                ][patch % 8];
                let c = lt_core::CfaPattern::Rggb.color_at(x, y).unwrap() as usize;
                let v = (refl[c] * illum[c] * k * (1.0 + 0.02 * noise(x, y))).clamp(0.0, 1.0);
                raw.data[(y * 256 + x) as usize] = (black + v * (white - black)) as u16;
            }
        }
        raw
    }

    #[test]
    fn auto_wb_recovers_the_illuminant() {
        for t in [3200.0f32, 5000.0, 7500.0] {
            let WhiteBalance::Custom { temp_k, tint } = auto_white_balance(&lit_scene(t)) else {
                panic!()
            };
            assert!((temp_k - t).abs() / t < 0.08, "{t} → {temp_k}");
            assert!(tint.abs() < 10.0, "tint {tint}");
        }
    }

    /// Daylight scene, camera as-shot right: 60% deep blue sea and sky (the classic
    /// gray-world trap), a white boat, gray rocks. Auto stays at as-shot.
    #[test]
    fn auto_wb_keeps_as_shot_on_blue_dominant_scenes() {
        let mut raw = gray_raw(256, 192, 0.0);
        let cam = CameraColor::of(&raw);
        let m = raw.wb_as_shot;
        let (black, white) = (raw.black_level[0], raw.white_level);
        for y in 0..192u32 {
            for x in 0..256u32 {
                // Reflectances as seen after the as-shot white balance.
                let refl: [f32; 3] = if y < 115 {
                    // Sky (lighter at the horizon) and sea.
                    let t = y as f32 / 115.0;
                    [0.08 + 0.1 * t, 0.2 + 0.1 * t, 0.55]
                } else if x < 60 {
                    [0.85, 0.85, 0.85] // white boat
                } else if x < 150 {
                    [0.12, 0.12, 0.12] // dark rock
                } else {
                    [0.35, 0.33, 0.3] // sand
                };
                let c = lt_core::CfaPattern::Rggb.color_at(x, y).unwrap() as usize;
                let v = (refl[c] / m[c] * 0.9 * (1.0 + 0.02 * noise(x, y))).clamp(0.0, 1.0);
                raw.data[(y * 256 + x) as usize] = (black + v * (white - black)) as u16;
            }
        }
        let WhiteBalance::Custom {
            temp_k: k0,
            tint: n0,
        } = multipliers_to_wb(m, &cam)
        else {
            panic!()
        };
        let WhiteBalance::Custom { temp_k, tint } = auto_white_balance(&raw) else {
            panic!()
        };
        assert!((temp_k - k0).abs() <= 150.0, "{k0} → {temp_k}");
        assert!((tint - n0).abs() <= 3.0, "{n0} → {tint}");
    }

    /// No neutral content at all (saturated patches only) and an as-shot a little off:
    /// the change stays within ±600 K / ±10 tint of as-shot.
    #[test]
    fn weak_evidence_is_clamped_toward_as_shot() {
        let mut raw = gray_raw(256, 192, 0.0);
        let cam = CameraColor::of(&raw);
        let (black, white) = (raw.black_level[0], raw.white_level);
        for y in 0..192u32 {
            for x in 0..256u32 {
                let refl: [f32; 3] = [
                    [0.6, 0.1, 0.05],
                    [0.05, 0.5, 0.1],
                    [0.1, 0.2, 0.7],
                    [0.6, 0.5, 0.05],
                ][((x / 64) % 4) as usize];
                let c = lt_core::CfaPattern::Rggb.color_at(x, y).unwrap() as usize;
                let v = (refl[c] * 0.5).clamp(0.0, 1.0);
                raw.data[(y * 256 + x) as usize] = (black + v * (white - black)) as u16;
            }
        }
        let WhiteBalance::Custom {
            temp_k: k0,
            tint: n0,
        } = multipliers_to_wb(raw.wb_as_shot, &cam)
        else {
            panic!()
        };
        let WhiteBalance::Custom { temp_k, tint } = auto_white_balance(&raw) else {
            panic!()
        };
        assert!((temp_k - k0).abs() <= 600.5, "{k0} → {temp_k}");
        assert!((tint - n0).abs() <= 10.01, "{n0} → {tint}");
        // Degenerate as-shot multipliers: as-shot comes back.
        raw.wb_as_shot = [f32::NAN, 1.0, 1.0];
        assert!(matches!(
            auto_white_balance(&raw),
            WhiteBalance::Custom { .. } | WhiteBalance::AsShot
        ));
    }

    /// A 240 × 160 scene of reflectances `scene(u, v)` lit by `light_k`, shot with the
    /// camera's white balance at `as_shot_k`.
    fn shot(light_k: f32, as_shot_k: f32, scene: impl Fn(f32, f32) -> [f32; 3]) -> RawImage {
        let mut raw = gray_raw(240, 160, 0.0);
        let cam = CameraColor::of(&raw);
        let m = temp_tint_to_multipliers(light_k, 0.0, &cam);
        raw.wb_as_shot = temp_tint_to_multipliers(as_shot_k, 0.0, &cam);
        let (black, white) = (raw.black_level[0], raw.white_level);
        for y in 0..160u32 {
            for x in 0..240u32 {
                let r = scene(x as f32 / 240.0, y as f32 / 160.0);
                let c = lt_core::CfaPattern::Rggb.color_at(x, y).unwrap() as usize;
                let v = (r[c] / m[c] * 0.5 * (1.0 + 0.01 * noise(x, y))).clamp(0.0, 1.0);
                raw.data[(y * 240 + x) as usize] = (black + v * (white - black)) as u16;
            }
        }
        raw
    }

    fn auto_k(raw: &RawImage) -> f32 {
        let WhiteBalance::Custom { temp_k, .. } = auto_white_balance(raw) else {
            panic!()
        };
        temp_k
    }

    /// Scenes where the as-shot is really wrong but only one luminance is neutral: a gray
    /// card on a wooden table and a warm room with a few grays under tungsten, fog in
    /// the shade. The white patch on the Planckian locus corrects them; a sunset and a
    /// blue sea keep their colour.
    #[test]
    fn auto_wb_corrects_a_wrong_as_shot_from_one_neutral() {
        let card = |u: f32, v: f32| {
            if (u - 0.5).abs() < 0.25 && (v - 0.5).abs() < 0.3 {
                [0.45; 3]
            } else {
                [0.45, 0.28, 0.14]
            }
        };
        let k = auto_k(&shot(2850.0, 5500.0, card));
        assert!((k - 2850.0).abs() <= 300.0, "gray card under tungsten: {k}");
        let room = |u: f32, v: f32| {
            if v < 0.15 {
                [0.05 + 0.8 * u; 3]
            } else {
                [
                    [0.7, 0.35, 0.15],
                    [0.5, 0.2, 0.1],
                    [0.6, 0.45, 0.25],
                    [0.3, 0.35, 0.15],
                ][((u * 4.0 + v * 3.0) as usize) % 4]
            }
        };
        let k = auto_k(&shot(2850.0, 5500.0, room));
        assert!(k < 5500.0 - 1000.0, "warm room under tungsten: {k}");
        let fog = |u: f32, v: f32| [0.45 + 0.15 * u + 0.05 * v; 3];
        let k = auto_k(&shot(7000.0, 5000.0, fog));
        assert!((k - 7000.0).abs() <= 500.0, "fog in the shade: {k}");
        let sunset = |u: f32, v: f32| {
            if v < 0.65 {
                let t = v / 0.65;
                [1.2 - 0.3 * t, 0.55 - 0.2 * t + 0.05 * u, 0.25 - 0.1 * t].map(|x| x * 0.7)
            } else {
                [0.03, 0.025, 0.02]
            }
        };
        let k = auto_k(&shot(5500.0, 5500.0, sunset));
        assert!((k - 5500.0).abs() <= 150.0, "sunset: {k}");
        let sea = |u: f32, v: f32| {
            if v < 0.4 {
                [0.35, 0.55, 0.85].map(|x| x * (0.8 + 0.2 * v))
            } else {
                [0.05, 0.18, 0.35].map(|x| x * (0.7 + 0.6 * u))
            }
        };
        let k = auto_k(&shot(6000.0, 6000.0, sea));
        assert!((k - 6000.0).abs() <= 150.0, "blue sea and sky: {k}");
    }

    #[test]
    fn auto_wb_fallbacks() {
        // Too few usable blocks (all clipped) → as-shot, expressed as Custom.
        let clipped = gray_raw(64, 64, 5.0);
        assert!(matches!(
            auto_white_balance(&clipped),
            WhiteBalance::Custom { .. }
        ));
        let mut bad = gray_raw(8, 8, 0.2);
        bad.data.clear();
        assert_eq!(
            auto_white_balance(&bad),
            WhiteBalance::Custom {
                temp_k: 5500.0,
                tint: 0.0
            }
        );
        let mut linear = gray_raw(8, 8, 0.2);
        linear.cfa = lt_core::CfaPattern::LinearRgb;
        linear.data = vec![1000; 8 * 8 * 3];
        assert_eq!(camera_blocks(&linear, 4).len(), 0);
    }

    #[test]
    fn auto_levels_targets_middle_gray() {
        let adj = Adjustments::default();
        let (dark, _, _) = auto_levels(&gray_raw(128, 96, 0.01), &adj);
        assert!(dark.exposure_ev > 1.5, "{}", dark.exposure_ev);
        let (bright, _, _) = auto_levels(&gray_raw(128, 96, 0.7), &adj);
        assert!(bright.exposure_ev < -0.5, "{}", bright.exposure_ev);
        // High dynamic range: deep shadows and near-clipped highlights engage HDR.
        let hdr = raw_from_fn(128, 96, |x, _| if x < 64 { [0.0004; 3] } else { [0.95; 3] });
        let (_, h, levels) = auto_levels(&hdr, &adj);
        assert!(h.shadows > 0.0 || h.highlights > 0.0, "{h:?}");
        assert!(levels.in_black <= 0.08 && levels.in_white >= 0.92);
        // Non-exposure fields pass through; invalid input returns the input unchanged.
        let mut custom = Adjustments::default();
        custom.exposure.contrast = 33.0;
        let (e, _, _) = auto_levels(&gray_raw(64, 48, 0.1), &custom);
        assert_eq!(e.contrast, 33.0);
        let mut bad = gray_raw(8, 8, 0.2);
        bad.white_level = 0.0;
        assert_eq!(
            auto_levels(&bad, &custom),
            (custom.exposure, custom.hdr, custom.levels)
        );
        let tiny = gray_raw(4, 4, 0.2);
        assert_eq!(auto_levels(&tiny, &custom).0, custom.exposure);
    }

    #[test]
    fn straighten_counter_rotates_and_fits() {
        let size = Size::new(6000, 4000);
        let c = auto_straighten_from_horizon(5.0, &Crop::default(), size);
        assert_eq!(c.angle_deg, -5.0);
        let r = c.rect;
        // Centered, same aspect, smaller, and the rotated rectangle stays inside the image.
        assert!((r.x + r.w / 2.0 - 0.5).abs() < 1e-4 && (r.y + r.h / 2.0 - 0.5).abs() < 1e-4);
        assert!(((r.w * 6000.0) / (r.h * 4000.0) - 1.5).abs() < 1e-3);
        assert!(r.w < 1.0);
        let (s, co) = 5f64.to_radians().sin_cos();
        let (a, b) = (f64::from(r.w) * 3000.0, f64::from(r.h) * 2000.0);
        assert!(a * co + b * s <= 3000.0 + 1e-3 && a * s + b * co <= 2000.0 + 1e-3);
        // Degenerate inputs leave the crop alone.
        assert_eq!(
            auto_straighten_from_horizon(f32::NAN, &Crop::default(), size),
            Crop::default()
        );
        assert_eq!(
            auto_straighten_from_horizon(3.0, &Crop::default(), Size::new(0, 10)),
            Crop::default()
        );
    }

    #[test]
    fn histogram_counts_and_clipping() {
        let px: Vec<u8> = [
            [0u8, 0, 0, 255],
            [255, 255, 255, 255],
            [255, 10, 10, 255],
            [100, 100, 100, 255],
        ]
        .iter()
        .flatten()
        .copied()
        .collect();
        let h = histogram(&RenderedImage {
            size: Size::new(2, 2),
            rgba8: px,
        });
        assert_eq!(h.total, 4);
        assert_eq!(h.r[255], 2);
        assert_eq!(h.g[10], 1);
        assert_eq!(h.l[100], 1);
        assert_eq!(h.l[0], 1);
        assert_eq!(h.clipped_shadows, 1);
        assert_eq!(h.clipped_highlights, 2);
        assert_eq!(h.clipped_high, [2, 1, 1]);
        assert_eq!(h.clipped_low, [1, 1, 1]);
        assert_eq!(h.luma_percentile(0.0), 0);
        assert_eq!(h.luma_percentile(1.0), 255);
        assert_eq!(Histogram::default().luma_percentile(0.5), 0);
        // Large images reduce across chunks.
        let big = RenderedImage {
            size: Size::new(100, 100),
            rgba8: vec![128; 100 * 100 * 4],
        };
        let h = histogram(&big);
        assert_eq!(
            (h.total, h.r[128], h.clipped_highlights),
            (10_000, 10_000, 0)
        );
    }
}
