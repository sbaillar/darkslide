//! Compile `Adjustments` + request into the numeric plan both renderers execute.
//! Keeping this in one place is what makes the CPU and GPU paths agree.

use lt_core::RawImage;
use lt_core::adjust::{Adjustments, Exposure, Hdr, Layer, LayerAdjustments, Levels};
use rayon::prelude::*;

use crate::color::{self, Mat3, mat_inv, mat_mul, mat_vec};
use crate::color_edit::{BalanceParams, OkRange, compile_editor, compile_ranges, luma};
use crate::demosaic::BaseParams;
use crate::geometry::{self, Geometry};
use crate::local::{GuidedParams, gaussian_kernel};
use crate::lut::{ENC_N, MonotoneCubic, ToneLuts, ToneSettings};
use crate::tone;
use crate::{OutputSpace, PipelineError, Quality, RenderRequest};

/// Default exposure offset applied in BASE so a normally exposed A9 frame lands with
/// middle gray near display 0.46 (calibrated against the camera's embedded JPEGs).
pub const BASELINE_EV: f32 = 1.18;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DemosaicMode {
    Superpixel,
    Rcd,
}

/// Cache key for the BASE stage (the image key is held by the cache itself). White
/// balance is deliberately not part of it: BASE is camera RGB at the reference WB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BaseKey {
    pub mode: DemosaicMode,
    /// Number of 2× box downsamplings after demosaic.
    pub down: u8,
    /// Export precision (the GPU keeps an f32 BASE instead of f16).
    pub precise: bool,
}

impl BaseKey {
    pub fn scale(self) -> f32 {
        let s0 = match self.mode {
            DemosaicMode::Superpixel => 0.5,
            DemosaicMode::Rcd => 1.0,
        };
        s0 / f32::from(1u16 << self.down)
    }
}

/// Largest output scale (of the sensor) rendered from a superpixel BASE.
pub const SUPERPIXEL_MAX_SCALE: f32 = 0.15;

/// Choose the demosaic and pyramid level for an output scale.
pub fn base_key(quality: Quality, scale: f32) -> BaseKey {
    let mode = match quality {
        // Views demosaic like the export (RCD, then box-downsampled levels): a 2×2
        // superpixel level keeps one R and one B sample per cell, 1.3–2.5× the export's
        // chroma noise at the same size, which chroma NR can't take back out. Only small
        // renders (thumbnails, filmstrip) use the 4× cheaper superpixel BASE. Draft and
        // Preview share their BASE (a drag never triggers a rebuild).
        Quality::Draft | Quality::Preview if scale <= SUPERPIXEL_MAX_SCALE => {
            DemosaicMode::Superpixel
        }
        Quality::Draft | Quality::Preview | Quality::Full => DemosaicMode::Rcd,
    };
    let mut key = BaseKey {
        mode,
        down: 0,
        precise: quality == Quality::Full,
    };
    while key.down < 8 && key.scale() * 0.5 >= scale {
        key.down += 1;
    }
    key
}

#[derive(Debug, Clone, PartialEq)]
pub struct NrPlan {
    /// Luma shrinkage thresholds for the fine and coarse wavelet bands.
    pub t1: f32,
    pub t2: f32,
    /// 0..=1 blend toward the guided chroma estimate.
    pub chroma: f32,
    /// Expected chroma noise (encoded units at output scale), measured from the image
    /// ([`raw_noise`]): chroma varying well beyond it is texture and kept.
    pub chroma_sigma: f32,
    pub gp: GuidedParams,
}

/// Chroma noise (encoded units, 1:1) per unit of measured raw noise ([`raw_noise`]) once
/// the noise is clearly visible; its square is the chroma filter's ε.
pub const CHROMA_NOISE_PER_RAW: f32 = 3.5;

/// Measured raw noise at which chroma NR reaches full strength (≈ an A9 at ISO 3200).
/// Below it the filter's noise estimate falls off with the noise itself: faint chroma
/// noise is below visibility, while the colour texture of a clean frame is not.
pub const CHROMA_NOISE_FULL: f32 = 0.012;

/// The chroma filter's noise estimate for a measured raw noise.
pub fn chroma_noise(raw_noise: f32) -> f32 {
    CHROMA_NOISE_PER_RAW * raw_noise * (raw_noise / CHROMA_NOISE_FULL).clamp(0.0, 1.0)
}

/// A value computed once per image (identity: data address and length plus a sample of
/// the data, since allocations get reused).
type PerRaw<T> = std::sync::Mutex<Vec<(usize, u64, T)>>;

fn raw_id(raw: &RawImage) -> (usize, u64) {
    let n = raw.data.len();
    let sample = (0..64usize).fold(n as u64, |h, i| {
        let v = raw.data.get(i * n / 64).copied().unwrap_or(0);
        h.wrapping_mul(0x100_0000_01b3).wrapping_add(u64::from(v))
    });
    (raw.data.as_ptr() as usize, sample)
}

fn per_raw_get<T: Copy>(cache: &PerRaw<T>, raw: &RawImage) -> Option<T> {
    let id = raw_id(raw);
    cache
        .lock()
        .ok()
        .and_then(|c| c.iter().find(|e| (e.0, e.1) == id).map(|e| e.2))
}

fn per_raw_put<T: Copy>(cache: &PerRaw<T>, raw: &RawImage, v: T) {
    let id = raw_id(raw);
    if let Ok(mut c) = cache.lock() {
        c.retain(|e| (e.0, e.1) != id);
        c.push((id.0, id.1, v));
        if c.len() > 16 {
            c.remove(0);
        }
    }
}

fn per_raw<T: Copy>(cache: &PerRaw<T>, raw: &RawImage, f: impl FnOnce() -> T) -> T {
    if let Some(v) = per_raw_get(cache, raw) {
        return v;
    }
    let v = f();
    per_raw_put(cache, raw, v);
    v
}

static AIRLIGHT: PerRaw<[f32; 3]> = std::sync::Mutex::new(Vec::new());
static RAW_NOISE: PerRaw<f32> = std::sync::Mutex::new(Vec::new());

/// Atmospheric light for dehaze, in camera RGB (normalized, before white balance):
/// the mean of the 0.1% of ~256×256 blocks with the brightest dark channel (the minimum
/// of the as-shot white-balanced block mean), skipping blocks with clipped samples.
/// Measured on the whole image, so every region and tile of a render agrees.
pub fn airlight_camera(raw: &RawImage) -> [f32; 3] {
    per_raw(&AIRLIGHT, raw, || {
        let blocks = if raw.cfa == lt_core::CfaPattern::LinearRgb {
            linear_blocks(raw)
        } else {
            crate::auto::camera_blocks(raw, crate::auto::block_size(raw))
        };
        airlight_from_blocks(raw, blocks)
    })
}

/// Whether the atmospheric light of `raw` is measured already.
pub fn airlight_known(raw: &RawImage) -> bool {
    per_raw_get(&AIRLIGHT, raw).is_some()
}

/// Measure the atmospheric light from camera block means computed elsewhere (the GPU's
/// [`crate::auto::block_size`] blocks of the resident mosaic) and remember it.
pub fn seed_airlight(raw: &RawImage, blocks: Vec<[f32; 3]>) {
    per_raw_put(&AIRLIGHT, raw, airlight_from_blocks(raw, blocks));
}

/// Whether the sensor noise of `raw` is measured already.
pub fn raw_noise_known(raw: &RawImage) -> bool {
    per_raw_get(&RAW_NOISE, raw).is_some()
}

/// Sensor noise set for images whose noise can't be measured (AI-denoised images), kept
/// until [`unseed`]: unlike the measured values (a bounded memo), a seed is
/// never evicted, so the look of a denoised image can't shift by being re-measured.
static SEEDED_NOISE: PerRaw<f32> = std::sync::Mutex::new(Vec::new());

/// Set the sensor noise of `raw` instead of measuring it (an AI-denoised image: the
/// estimator can't tell its fine texture from noise, see `denoise::denoised_raw`). Call
/// [`unseed`] when `raw` is dropped.
pub fn seed_raw_noise(raw: &RawImage, noise: f32) {
    let id = raw_id(raw);
    if let Ok(mut c) = SEEDED_NOISE.lock() {
        c.retain(|e| (e.0, e.1) != id);
        c.push((id.0, id.1, noise.max(0.0)));
    }
}

/// The identity a seed is kept under: taken while the raw is alive, it forgets the
/// seed after the raw is gone ([`unseed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoiseSeed(usize, u64);

/// The seed identity of `raw`.
pub fn noise_seed_of(raw: &RawImage) -> NoiseSeed {
    let (a, b) = raw_id(raw);
    NoiseSeed(a, b)
}

/// Forget a seed (its raw is dropped).
pub fn unseed(seed: NoiseSeed) {
    if let Ok(mut c) = SEEDED_NOISE.lock() {
        c.retain(|e| (e.0, e.1) != (seed.0, seed.1));
    }
}

fn airlight_from_blocks(raw: &RawImage, blocks: Vec<[f32; 3]>) -> [f32; 3] {
    let wb = raw.wb_as_shot;
    let mut by: Vec<(f32, [f32; 3])> = blocks
        .into_iter()
        .map(|b| ((0..3).map(|c| b[c] * wb[c]).fold(f32::MAX, f32::min), b))
        .filter(|(d, _)| d.is_finite())
        .collect();
    if by.is_empty() {
        // Nothing measurable: a neutral light at the white level.
        return [0, 1, 2].map(|c| 1.0 / wb[c].max(1e-3));
    }
    let n = (by.len() / 1000).max(1);
    by.select_nth_unstable_by(n - 1, |a, b| b.0.total_cmp(&a.0));
    let mut a = [0.0f32; 3];
    for (_, b) in &by[..n] {
        for c in 0..3 {
            a[c] += b[c] / n as f32;
        }
    }
    a
}

/// Block means of a linear RGB mosaic (tests and synthetic images).
fn linear_blocks(raw: &RawImage) -> Vec<[f32; 3]> {
    let (w, h) = (raw.size.width as usize, raw.size.height as usize);
    let b = crate::auto::block_size(raw);
    let range = |c: usize| (raw.white_level - raw.black_level[c]).max(1.0);
    let mut out = Vec::new();
    for by in 0..h / b {
        'blk: for bx in 0..w / b {
            let mut acc = [0.0f32; 3];
            for y in by * b..(by + 1) * b {
                for x in bx * b..(bx + 1) * b {
                    for c in 0..3 {
                        let v = (f32::from(raw.data[(y * w + x) * 3 + c]) - raw.black_level[c])
                            / range(c);
                        if v >= 0.97 {
                            continue 'blk;
                        }
                        acc[c] += v.max(0.0);
                    }
                }
            }
            out.push(acc.map(|v| v / (b * b) as f32));
        }
    }
    out
}

/// Sensor noise of an image, measured on the data: robust (MAD) standard deviation of
/// differences between diagonal same-colour neighbours (the two greens of a Bayer
/// quad; any channel pair of linear RGB) in mid-tones, normalized to the white level.
/// Texture barely moves a median of diagonal differences, so a clean image measures ~0
/// whatever its ISO. Cached per image (identity of the data).
pub fn raw_noise(raw: &RawImage) -> f32 {
    if let Some(v) = per_raw_get(&SEEDED_NOISE, raw) {
        return v;
    }
    per_raw(&RAW_NOISE, raw, || measure_raw_noise(raw))
}

fn measure_raw_noise(raw: &RawImage) -> f32 {
    const B: usize = 64;
    let (w, h) = (raw.size.width as usize, raw.size.height as usize);
    if w < 4 || h < 4 {
        return 0.0;
    }
    let si = crate::demosaic::site_info(raw);
    let linear = raw.cfa == lt_core::CfaPattern::LinearRgb;
    let norm = |x: usize, y: usize| -> f32 {
        if linear {
            // Green channel of linear RGB.
            (f32::from(raw.data[(y * w + x) * 3 + 1]) - raw.black_level[1])
                / (raw.white_level - raw.black_level[1]).max(1.0)
        } else {
            let pos = (y & 1) << 1 | (x & 1);
            (f32::from(raw.data[y * w + x]) - si.black[pos]) * si.inv_range[pos]
        }
    };
    let green = |x: usize, y: usize| linear || si.color[(y & 1) << 1 | (x & 1)] == 1;
    // Robust noise per 64×64 block (MAD of diagonal same-colour differences), then a low
    // percentile over blocks: the flattest areas show the noise, textured ones don't count.
    let blocks_x = w / B;
    let mut mads: Vec<f32> = (0..h / B)
        .into_par_iter()
        .flat_map_iter(|by| {
            (0..blocks_x).filter_map(move |bx| {
                let mut d: Vec<f32> = Vec::with_capacity(256);
                for y in (by * B..(by + 1) * B - 1).step_by(4) {
                    for x in (bx * B..(bx + 1) * B - 2).step_by(2) {
                        // The green of this 2×2 block and its diagonal partner (green in a
                        // Bayer quad, the same channel in linear RGB).
                        let (x0, y0) = if green(x, y) { (x, y) } else { (x + 1, y) };
                        let (x1, y1) = (x0 + 1, y0 + 1);
                        if !green(x0, y0) || !green(x1, y1) {
                            continue;
                        }
                        let (a, b) = (norm(x0, y0), norm(x1, y1));
                        if (0.02..0.6).contains(&a) && (0.02..0.6).contains(&b) {
                            // Square roots: shot noise becomes level-independent, as it is
                            // (roughly) in the encoded domain chroma NR works in.
                            d.push((a.sqrt() - b.sqrt()).abs());
                        }
                    }
                }
                (d.len() >= 64).then(|| {
                    let mid = d.len() / 2;
                    *d.select_nth_unstable_by(mid, f32::total_cmp).1
                })
            })
        })
        .collect();
    if mads.is_empty() {
        return 0.0;
    }
    let k = mads.len() / 5;
    let med = *mads.select_nth_unstable_by(k, f32::total_cmp).1;
    1.4826 * med / std::f32::consts::SQRT_2
}

impl NrPlan {
    pub fn luma_active(&self) -> bool {
        self.t1 > 0.0
    }
    pub fn chroma_active(&self) -> bool {
        self.chroma > 0.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SharpenPlan {
    pub gain: f32,
    pub threshold: f32,
    /// Half Gaussian kernel (center first).
    pub kernel: Vec<f32>,
}

/// Per-layer tone tables on the encoded domain.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerCurves {
    /// Encoded → encoded per channel (RGB curve then channel curve).
    pub chan: [Vec<f32>; 3],
    pub luma: Option<Vec<f32>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LayerPlan {
    pub index: usize,
    pub opacity: f32,
    /// Re-tone through the base curve (WB, exposure or HDR present).
    pub retone: bool,
    pub wb: Mat3,
    pub ev: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub contrast_k: f32,
    pub brightness_g: f32,
    pub saturation: f32,
    pub curves: Option<LayerCurves>,
    pub balance: BalanceParams,
    pub ranges: Vec<OkRange>,
    pub clarity: f32,
    pub structure: f32,
    /// Presence, −1..=1 each.
    pub vibrance: f32,
    pub texture: f32,
    pub dehaze: f32,
    /// Local sharpening, −1..=1 (negative softens: fine detail and texture removed).
    pub sharpening: f32,
    /// Whites stretch factor for stops above middle gray (1 = none) and blacks toe
    /// (−100..=100), as the global tone curve applies them.
    pub whites_f: f32,
    pub blacks: f32,
}

impl LayerPlan {
    pub fn uses_hdr(&self) -> bool {
        self.highlights != 0.0 || self.shadows != 0.0
    }
    pub fn uses_detail(&self) -> bool {
        self.clarity != 0.0 || self.structure != 0.0 || self.texture_amount() != 0.0
    }
    /// Texture including the part negative sharpening (softening) takes away.
    pub fn texture_amount(&self) -> f32 {
        (self.texture + 0.5 * self.sharpening.min(0.0)).clamp(-1.0, 1.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub geo: Geometry,
    pub base_key: BaseKey,
    pub base_params: BaseParams,
    /// Sensor rectangle (x, y, w, h) the view samples, with a demosaic margin.
    pub sensor_bbox: [u32; 4],
    /// Pixel size of the BASE level `base_key` selects.
    pub level_size: [u32; 2],
    /// Develop-time white balance: base values × `wb_ratio` are the user's WB'd camera
    /// RGB, which clips at `clip` (= the user's multipliers).
    pub wb_ratio: [f32; 3],
    pub clip: [f32; 3],
    /// WB'd camera RGB → linear Rec.2020, including the baseline exposure gain.
    pub cam_matrix: Mat3,
    /// The base look: curve, colour maps (applied to scene-linear colour right before
    /// tone) and, for imported profiles, the camera matrix above.
    pub profile: crate::profile::ProfilePlan,
    pub exposure_gain: f32,
    /// (amount EV, midpoint) when active.
    pub vignette: Option<(f32, f32)>,
    /// Global HDR highlights/shadows (−1..=1).
    pub highlights: f32,
    pub shadows: f32,
    pub hdr_gp: GuidedParams,
    pub luts: ToneLuts,
    pub balance: BalanceParams,
    pub saturation: f32,
    pub ranges: Vec<OkRange>,
    pub clarity: f32,
    pub structure: f32,
    pub clarity_gp: GuidedParams,
    pub structure_gp: GuidedParams,
    /// Presence, −1..=1 each.
    pub vibrance: f32,
    pub texture: f32,
    pub texture_gp: GuidedParams,
    /// Luma noise (encoded, at output scale) texture and layer sharpening stay above.
    pub detail_noise: f32,
    /// Layer sharpening's 5-tap Gaussian (center, ±1, ±2) and detail gain
    /// ([`crate::detail::fine_kernel`]; σ in normalized units).
    pub fine: ([f32; 3], f32),
    pub dehaze: f32,
    /// Haze estimate filter (self-guided on the dark channel).
    pub dehaze_gp: GuidedParams,
    /// Atmospheric light (linear Rec.2020 after exposure), measured on the whole image.
    pub airlight: [f32; 3],
    pub layers: Vec<LayerPlan>,
    pub nr: NrPlan,
    pub sharpen: Option<SharpenPlan>,
    pub output: OutputSpace,
    pub output_matrix: Mat3,
    /// Region renders develop with an apron of context around the requested window
    /// (x, y, w, h inside `geo.out`); only the window is output.
    pub window: Option<[u32; 4]>,
    /// Draft renders: the output size the (smaller) developed window is bilinearly
    /// upscaled to.
    pub upscale: Option<lt_core::Size>,
    /// Preview renders that sample their BASE level at a step ≥ 1.25 level pixels per
    /// output pixel average 2×2 geometry samples (±¼ output pixel): a plain bilinear
    /// sample there keeps most of the noise (and aliases fine texture), so a settled view
    /// looked noisier and crisper than the export of the same size, which averages the
    /// whole footprint.
    pub supersample: bool,
    /// Full-quality downscaled renders: developed at the BASE level's resolution, then
    /// resized (linear light, area-correct Catmull-Rom) to this size.
    pub resize: Option<lt_core::Size>,
    /// Output sharpening at the final size (gain = amount, no threshold).
    pub out_sharpen: Option<SharpenPlan>,
    /// Heal/clone spots touching this render.
    pub spots: Vec<crate::spots::SpotPlan>,
    /// Lens blur (scene-linear, after the scene stage).
    pub lens_blur: Option<crate::effects::LensBlurPlan>,
    /// Glow and halation (after the layers).
    pub fx: crate::effects::FxPlan,
    /// Film grain (after sharpening).
    pub grain: Option<crate::effects::GrainPlan>,
}

impl Plan {
    /// The smoothed log-luminance plane is needed (global or any layer HDR).
    pub fn needs_hdr(&self) -> bool {
        self.highlights != 0.0 || self.shadows != 0.0 || self.layers.iter().any(LayerPlan::uses_hdr)
    }
    pub fn needs_detail(&self) -> bool {
        self.clarity != 0.0
            || self.structure != 0.0
            || self.texture != 0.0
            || self.layers.iter().any(LayerPlan::uses_detail)
    }
    /// Each detail plane is built only when something uses it (global or a layer).
    pub fn needs_clarity_plane(&self) -> bool {
        self.clarity != 0.0 || self.layers.iter().any(|l| l.clarity != 0.0)
    }
    pub fn needs_structure_plane(&self) -> bool {
        self.structure != 0.0 || self.layers.iter().any(|l| l.structure != 0.0)
    }
    pub fn needs_texture_plane(&self) -> bool {
        self.texture != 0.0 || self.layers.iter().any(|l| l.texture_amount() != 0.0)
    }
    /// The haze estimate is needed (global or any layer dehaze).
    pub fn needs_dehaze(&self) -> bool {
        self.dehaze != 0.0 || self.layers.iter().any(|l| l.dehaze != 0.0)
    }
    /// The scene-linear image is kept (HDR, dehaze or the lens blur need its
    /// neighbourhood).
    pub fn needs_lin(&self) -> bool {
        self.needs_hdr() || self.needs_dehaze() || self.lens_blur.is_some()
    }
    /// A layer uses the fine-detail (5×5) sharpening plane.
    pub fn needs_layer_sharpen(&self) -> bool {
        self.layers.iter().any(|l| l.sharpening != 0.0)
    }
}

fn finite_or(v: f32, d: f32) -> f32 {
    if v.is_finite() { v } else { d }
}

/// BASE parameters for an image: the reference (as-shot) white balance.
pub fn base_params(raw: &RawImage) -> BaseParams {
    BaseParams {
        mults: color::normalize_mults(raw.wb_as_shot),
    }
}

/// Camera → working matrix including the baseline exposure gain.
pub fn cam_matrix(raw: &RawImage) -> Mat3 {
    let gain = BASELINE_EV.exp2();
    color::camera_to_working(&raw.cam_to_xyz).map(|r| r.map(|v| v * gain))
}

/// Long edge a `Quality::Draft` render develops at: [`crate::DRAFT_LONG_EDGE`], or
/// [`crate::DRAFT_LONG_EDGE_HEAVY`] for heavy edits. The cost counts, per enabled layer,
/// 1 + its output-space mask planes (colour ranges, auto-masked brushes, a mask
/// feather) + 1 if it uses neighbourhood tools (clarity, structure, texture, dehaze,
/// sharpening, highlights/shadows); + 1 for global neighbourhood presence (texture,
/// dehaze, clarity, structure) and + 1 for spots. Heavy from 5.
pub fn draft_long_edge(adj: &Adjustments) -> u32 {
    use lt_core::adjust::MaskShape;
    let on = |v: f32| v.is_finite() && v != 0.0;
    let mut cost = 0usize;
    for l in adj.layers.iter().filter(|l| l.enabled && l.opacity > 0.0) {
        let a = &l.adjustments;
        let planes = l
            .mask
            .shapes
            .iter()
            .filter(|s| match s {
                MaskShape::ColorRange { .. } => true,
                MaskShape::Brush { strokes, .. } => strokes.iter().any(|s| s.auto_mask),
                _ => false,
            })
            .count()
            + usize::from(on(l.mask.feather));
        let tools = [
            a.clarity,
            a.structure,
            a.presence.texture,
            a.presence.dehaze,
            a.sharpening,
            a.hdr.highlights,
            a.hdr.shadows,
        ]
        .into_iter()
        .any(on);
        cost += 1 + planes + usize::from(tools);
    }
    let p = &adj.presence;
    cost += usize::from(
        [
            p.texture,
            p.dehaze,
            adj.detail.clarity,
            adj.detail.structure,
        ]
        .into_iter()
        .any(on),
    );
    cost += usize::from(!adj.spots.is_empty());
    // Effects with a neighbourhood: the lens blur's gather, glow and halation.
    let fx = &adj.effects;
    cost += usize::from(on(fx.lens_blur.amount))
        + usize::from(on(fx.glow.amount) || on(fx.halation.amount));
    if cost >= 5 {
        crate::DRAFT_LONG_EDGE_HEAVY
    } else {
        crate::DRAFT_LONG_EDGE
    }
}

/// Margin around the sampled sensor area so demosaic neighbourhoods are real data.
pub const BBOX_MARGIN: u32 = 16;

/// `bbox` grown to cover what the spots sample outside the output: their sources and
/// the heal rings around both discs.
fn spot_bbox(
    bbox: [u32; 4],
    geo: &Geometry,
    spots: &[crate::spots::SpotPlan],
    raw: &RawImage,
) -> [u32; 4] {
    if spots.is_empty() {
        return bbox;
    }
    let (mut x0, mut y0) = (f64::from(bbox[0]), f64::from(bbox[1]));
    let (mut x1, mut y1) = (x0 + f64::from(bbox[2]), y0 + f64::from(bbox[3]));
    for sp in spots {
        for source in [false, true] {
            for p in crate::spots::ring_points(sp, geo, source) {
                for s in geo.sensor_at(p[0], p[1]).0 {
                    x0 = x0.min(s[0]);
                    y0 = y0.min(s[1]);
                    x1 = x1.max(s[0]);
                    y1 = y1.max(s[1]);
                }
            }
        }
    }
    let m = f64::from(BBOX_MARGIN);
    let (sw, sh) = (f64::from(raw.size.width), f64::from(raw.size.height));
    let (ax, ay) = (
        (x0 - m).clamp(0.0, sw) as u32 & !1,
        (y0 - m).clamp(0.0, sh) as u32 & !1,
    );
    let (bx, by) = (
        (x1 + m + 1.0).clamp(0.0, sw) as u32,
        (y1 + m + 1.0).clamp(0.0, sh) as u32,
    );
    [
        ax,
        ay,
        bx.saturating_sub(ax).max(2),
        by.saturating_sub(ay).max(2),
    ]
}

/// Sensor bounding box of everything the output samples (plus a margin), even-aligned.
pub fn sensor_bbox(geo: &Geometry, raw: &RawImage) -> [u32; 4] {
    const N: usize = 32;
    let (w, h) = (f64::from(geo.out.width), f64::from(geo.out.height));
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for i in 0..=N {
        let t = i as f64 / N as f64;
        for (x, y) in [(t * w, 0.0), (t * w, h), (0.0, t * h), (w, t * h)] {
            for p in geo.sensor_at(x, y).0 {
                x0 = x0.min(p[0]);
                y0 = y0.min(p[1]);
                x1 = x1.max(p[0]);
                y1 = y1.max(p[1]);
            }
        }
    }
    let m = f64::from(BBOX_MARGIN);
    let (sw, sh) = (f64::from(raw.size.width), f64::from(raw.size.height));
    let clampx = |v: f64| v.clamp(0.0, sw) as u32;
    let clampy = |v: f64| v.clamp(0.0, sh) as u32;
    let (ax, ay) = (clampx(x0 - m) & !1, clampy(y0 - m) & !1);
    let (bx, by) = (clampx(x1 + m + 1.0), clampy(y1 + m + 1.0));
    [
        ax,
        ay,
        bx.saturating_sub(ax).max(2),
        by.saturating_sub(ay).max(2),
    ]
}

/// Pixel size of a BASE level.
pub fn level_size(raw: &RawImage, bk: BaseKey) -> [u32; 2] {
    let (mut w, mut h) = match bk.mode {
        DemosaicMode::Superpixel => (raw.size.width / 2, raw.size.height / 2),
        DemosaicMode::Rcd => (raw.size.width, raw.size.height),
    };
    for _ in 0..bk.down {
        (w, h) = ((w / 2).max(1), (h / 2).max(1));
    }
    [w, h]
}

fn curve_tables(
    rgb: &[[f32; 2]],
    per: [&Vec<[f32; 2]>; 3],
    luma_pts: &[[f32; 2]],
) -> Option<LayerCurves> {
    let rgb = MonotoneCubic::from_curve_points(rgb);
    let per = per.map(|p| MonotoneCubic::from_curve_points(p));
    let luma = MonotoneCubic::from_curve_points(luma_pts).map(|c| table(|y| c.eval(y)));
    if rgb.is_none() && per.iter().all(Option::is_none) && luma.is_none() {
        return None;
    }
    let chan = per.map(|c| {
        table(|y| {
            let y = rgb.as_ref().map_or(y, |r| r.eval(y));
            c.as_ref().map_or(y, |c| c.eval(y))
        })
    });
    Some(LayerCurves { chan, luma })
}

fn table(f: impl Fn(f32) -> f32) -> Vec<f32> {
    (0..ENC_N)
        .map(|i| f(i as f32 / (ENC_N - 1) as f32))
        .collect()
}

/// Working-space matrix for a layer's relative WB shift.
fn layer_wb(raw: &RawImage, adj: &Adjustments, la: &LayerAdjustments) -> Mat3 {
    const ID: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let Some(d) = la.white_balance else { return ID };
    if d.temp_k == 0.0 && d.tint == 0.0 {
        return ID;
    }
    let (t0, i0) = color::wb_temp_tint(adj.white_balance, raw);
    let cc = color::CameraColor::of(raw);
    let m0 = color::temp_tint_to_multipliers(t0, i0, &cc);
    let m1 = color::temp_tint_to_multipliers(t0 + d.temp_k, i0 + d.tint, &cc);
    let g = [m1[0] / m0[0], m1[1] / m0[1], m1[2] / m0[2]];
    let cam = color::camera_to_working(&raw.cam_to_xyz);
    let Some(inv) = mat_inv(&cam) else { return ID };
    let w = mat_mul(&mat_mul(&cam, &color::diag(g)), &inv);
    let y = luma(mat_vec(&w, [1.0; 3]));
    if !(y.is_finite() && y > 1e-6) {
        return ID;
    }
    w.map(|r| r.map(|v| v / y))
}

fn layer_plan(index: usize, layer: &Layer, raw: &RawImage, adj: &Adjustments) -> LayerPlan {
    let la = &layer.adjustments;
    let wb = layer_wb(raw, adj, la);
    let ev = finite_or(la.exposure.exposure_ev, 0.0).clamp(-5.0, 5.0);
    let highlights = (finite_or(la.hdr.highlights, 0.0) / 100.0).clamp(-1.0, 1.0);
    let shadows = (finite_or(la.hdr.shadows, 0.0) / 100.0).clamp(-1.0, 1.0);
    let wb_active = la
        .white_balance
        .is_some_and(|d| d.temp_k != 0.0 || d.tint != 0.0);
    let pres = |v: f32| (finite_or(v, 0.0) / 100.0).clamp(-1.0, 1.0);
    let dehaze = pres(la.presence.dehaze);
    let whites = finite_or(la.hdr.whites, 0.0).clamp(-100.0, 100.0);
    let blacks = finite_or(la.hdr.blacks, 0.0).clamp(-100.0, 100.0);
    LayerPlan {
        index,
        opacity: finite_or(layer.opacity, 1.0).clamp(0.0, 1.0),
        retone: wb_active
            || ev != 0.0
            || highlights != 0.0
            || shadows != 0.0
            || dehaze != 0.0
            || whites != 0.0
            || blacks != 0.0,
        wb,
        ev,
        highlights,
        shadows,
        contrast_k: tone::contrast_k(finite_or(la.exposure.contrast, 0.0)),
        brightness_g: tone::brightness_gamma(finite_or(la.exposure.brightness, 0.0)),
        saturation: 1.0 + (finite_or(la.exposure.saturation, 0.0) / 100.0).clamp(-1.0, 1.0),
        curves: curve_tables(
            &la.curve.rgb,
            [&la.curve.red, &la.curve.green, &la.curve.blue],
            &la.curve.luma,
        ),
        balance: BalanceParams::new(&la.color_balance),
        ranges: compile_ranges(&la.color_editor),
        clarity: (finite_or(la.clarity, 0.0) / 100.0).clamp(-1.0, 1.0),
        structure: (finite_or(la.structure, 0.0) / 100.0).clamp(-1.0, 1.0),
        vibrance: pres(la.presence.vibrance),
        texture: pres(la.presence.texture),
        dehaze,
        sharpening: pres(la.sharpening),
        whites_f: tone::whites_factor(whites),
        blacks,
    }
}

/// ISO-adaptive noise-reduction scaling of the sidecar sliders (linear in stops):
/// - luminance: off at base ISO (100), the slider's value at ISO 3200, up to 1.4× by
///   ISO 12 800;
/// - chroma: 0.3× at ISO 100 rising to 1× at ISO 800, then up to 2× at ISO 3200 and
///   above (the default 25 becomes 50); the chroma filter itself is noise-adaptive
///   (ε from the measured noise), so texture above the noise survives.
///
/// The scaled values are capped at the sliders' maximum. Unknown ISO leaves the sliders
/// as they are.
pub fn iso_nr_factors(iso: Option<u32>) -> (f32, f32) {
    match iso {
        Some(iso) if iso > 0 => {
            let stops = (iso as f32 / 100.0).log2();
            let luma = (stops / 5.0).clamp(0.0, 1.4);
            let chroma = if stops <= 3.0 {
                0.3 + 0.7 * (stops / 3.0).max(0.0)
            } else {
                1.0 + 0.5 * (stops - 3.0).min(2.0)
            };
            (luma, chroma)
        }
        _ => (1.0, 1.0),
    }
}

/// Build the plan for one render.
pub fn build(
    raw: &RawImage,
    adj: &Adjustments,
    req: &RenderRequest,
) -> Result<Plan, PipelineError> {
    raw.validate()?;
    let full = geometry::plan(raw, adj, &req.capture, req.region, req.max_size, req.fit)?;
    let long_out = full.out.width.max(full.out.height);
    // Full-quality renders smaller than the sensor (exports) develop at full resolution
    // and resize at the end, area-correct in linear light: tone is applied before any
    // averaging and no box pyramid aliases fine texture.
    let resized =
        req.quality == Quality::Full && req.region.is_none() && f64::from(full.scale) * 1.02 < 1.0;
    let key = base_key(req.quality, if resized { 1.0 } else { full.scale });
    let (geo, upscale, resize) = if req.quality == Quality::Draft && long_out > draft_long_edge(adj)
    {
        let k = f64::from(draft_long_edge(adj)) / f64::from(long_out);
        let small = lt_core::Size::new(
            ((f64::from(full.out.width) * k).round() as u32).max(1),
            ((f64::from(full.out.height) * k).round() as u32).max(1),
        );
        let g = geometry::plan(raw, adj, &req.capture, req.region, small, req.fit)?;
        (g, Some(full.out), None)
    } else if resized {
        let big = lt_core::Size::new(u32::MAX / 2, u32::MAX / 2);
        let g = geometry::plan(raw, adj, &req.capture, None, big, req.fit)?;
        (g, None, Some(full.out))
    } else {
        (full, None, None)
    };
    let bp = base_params(raw);
    let user = color::wb_to_multipliers(adj.white_balance, raw);
    let long = geo.long_edge_out();
    let e = &adj.exposure;
    let v = &adj.vignette;
    let vignette = (finite_or(v.amount_ev, 0.0) != 0.0)
        .then(|| (v.amount_ev.clamp(-3.0, 3.0), finite_or(v.midpoint, 0.5)));
    let lv = &adj.levels;
    let exposure_gain = finite_or(e.exposure_ev, 0.0).clamp(-5.0, 5.0).exp2();
    let profile = crate::profile::resolve(
        &adj.profile,
        req.capture.camera.as_deref(),
        user,
        exposure_gain,
    );
    let cam = crate::profile::camera_matrix(raw, &profile);
    let luts = ToneLuts::build(&ToneSettings {
        base: profile.curve.clone(),
        exposure: Exposure {
            exposure_ev: 0.0,
            contrast: finite_or(e.contrast, 0.0),
            brightness: finite_or(e.brightness, 0.0),
            saturation: 0.0,
        },
        hdr: Hdr {
            whites: finite_or(adj.hdr.whites, 0.0),
            blacks: finite_or(adj.hdr.blacks, 0.0),
            ..Hdr::default()
        },
        levels: Levels {
            in_black: finite_or(lv.in_black, 0.0),
            in_white: finite_or(lv.in_white, 1.0),
            gamma: finite_or(lv.gamma, 1.0),
            out_black: finite_or(lv.out_black, 0.0),
            out_white: finite_or(lv.out_white, 1.0),
        },
        curve: adj.curve.clone(),
    });
    let d = &adj.detail;
    let n = &adj.noise;
    let scale = geo.scale;
    let detail_factor = 1.5 - (finite_or(n.detail, 50.0) / 100.0).clamp(0.0, 1.0);
    let (luma_iso, chroma_iso) = iso_nr_factors(req.capture.iso);
    let t1 = ((finite_or(n.luminance, 0.0) / 100.0).clamp(0.0, 1.0) * luma_iso).min(1.0)
        * 0.03
        * scale.sqrt().min(1.0)
        * detail_factor;
    let nr = NrPlan {
        t1,
        t2: 0.5 * t1,
        chroma: {
            // Eased: the filter's protection of texture makes a firm blend safe.
            let s = ((finite_or(n.color, 0.0) / 100.0).clamp(0.0, 1.0) * chroma_iso).min(1.0);
            1.0 - (1.0 - s).powi(3)
        },
        chroma_sigma: chroma_noise(raw_noise(raw)) * scale.min(1.0),
        // Chroma noise is low-frequency: the guided estimate runs on a coarse grid (at
        // least 2×2, 4×4 once the radius allows), which keeps big views cheap.
        gp: {
            let radius = 8.0 * scale.max(0.25);
            GuidedParams::for_radius(radius, 1e-4).with_min_block(if radius >= 6.0 { 4 } else { 2 })
        },
    };
    let sigma = finite_or(d.sharpen_radius, 0.8).clamp(0.2, 2.5) * scale;
    let amount = finite_or(d.sharpen_amount, 0.0).clamp(0.0, 1000.0);
    let sharpen = (amount > 0.0 && sigma >= 0.3 && upscale.is_none()).then(|| SharpenPlan {
        gain: amount / 200.0,
        threshold: finite_or(d.sharpen_threshold, 0.0).clamp(0.0, 10.0) / 255.0,
        kernel: gaussian_kernel(sigma),
    });
    Ok(Plan {
        base_key: key,
        base_params: bp,
        sensor_bbox: sensor_bbox(&geo, raw),
        level_size: level_size(raw, key),
        wb_ratio: [0, 1, 2].map(|c| user[c] / bp.mults[c]),
        clip: user,
        cam_matrix: cam,
        exposure_gain,
        vignette,
        highlights: (finite_or(adj.hdr.highlights, 0.0) / 100.0).clamp(-1.0, 1.0),
        shadows: (finite_or(adj.hdr.shadows, 0.0) / 100.0).clamp(-1.0, 1.0),
        hdr_gp: GuidedParams::for_radius(0.025 * long, 0.02),
        luts,
        profile,
        balance: BalanceParams::new(&adj.color_balance),
        saturation: 1.0 + (finite_or(e.saturation, 0.0) / 100.0).clamp(-1.0, 1.0),
        ranges: compile_editor(&adj.color_editor),
        clarity: (finite_or(d.clarity, 0.0) / 100.0).clamp(-1.0, 1.0),
        structure: (finite_or(d.structure, 0.0) / 100.0).clamp(-1.0, 1.0),
        clarity_gp: GuidedParams::for_radius(0.012 * long, 0.0015),
        structure_gp: GuidedParams::for_radius(0.0025 * long, 0.0005),
        vibrance: (finite_or(adj.presence.vibrance, 0.0) / 100.0).clamp(-1.0, 1.0),
        texture: (finite_or(adj.presence.texture, 0.0) / 100.0).clamp(-1.0, 1.0),
        // A fine radius: at least 2×2 blocks keeps it cheap at full resolution.
        texture_gp: GuidedParams::for_radius(0.0008 * long, crate::detail::TEXTURE_EPS)
            .with_min_block(2),
        detail_noise: crate::detail::DETAIL_NOISE_PER_RAW * raw_noise(raw) * scale.min(1.0),
        fine: crate::detail::fine_kernel(crate::detail::FINE_SIGMA * long),
        dehaze: (finite_or(adj.presence.dehaze, 0.0) / 100.0).clamp(-1.0, 1.0),
        dehaze_gp: GuidedParams::for_radius(0.01 * long, 0.001),
        airlight: {
            let needs = adj.presence.dehaze != 0.0
                || adj
                    .layers
                    .iter()
                    .any(|l| l.enabled && l.adjustments.presence.dehaze != 0.0);
            if needs {
                let a = airlight_camera(raw);
                let w = color::mat_vec(&cam, [0, 1, 2].map(|c| a[c] * user[c]));
                w.map(|v| (v * exposure_gain).max(0.02))
            } else {
                [1.0; 3]
            }
        },
        layers: adj
            .layers
            .iter()
            .enumerate()
            .filter(|(_, l)| l.enabled && l.opacity > 0.0)
            .map(|(i, l)| layer_plan(i, l, raw, adj))
            .collect(),
        nr,
        sharpen,
        output: req.output,
        output_matrix: color::working_to_output(req.output),
        window: None,
        upscale,
        supersample: req.quality == Quality::Preview
            && upscale.is_none()
            && key.scale() >= 1.25 * geo.scale,
        resize,
        out_sharpen: req
            .output_sharpening
            .filter(|s| s.amount.is_finite() && s.amount > 0.0 && s.radius.is_finite())
            .map(|s| SharpenPlan {
                gain: s.amount.min(10.0),
                threshold: 0.0,
                kernel: gaussian_kernel(s.radius.clamp(0.3, 4.0)),
            }),
        spots: crate::spots::plan(&adj.spots, &geo),
        lens_blur: crate::effects::plan_lens_blur(adj, &geo, req.masks.as_ref()),
        fx: crate::effects::plan_fx(adj, long),
        grain: crate::effects::plan_grain(adj, long),
        geo,
    }
    .with_region_context(raw, req.region.is_some(), adj))
}

impl Plan {
    /// Output size (the window for region renders).
    pub fn out_size(&self) -> lt_core::Size {
        self.upscale.or(self.resize).unwrap_or_else(|| {
            self.window
                .map_or(self.geo.out, |w| lt_core::Size::new(w[2], w[3]))
        })
    }

    /// The developed pixels that are output: (x, y, w, h) inside `geo.out`.
    pub fn out_window(&self) -> [u32; 4] {
        self.window
            .unwrap_or([0, 0, self.geo.out.width, self.geo.out.height])
    }

    /// One tile `r` = (x, y, w, h) of this (full, un-windowed) plan's output, developed
    /// with the context it needs so tiles assemble into exactly the full render.
    pub fn tile(&self, raw: &RawImage, adj: &Adjustments, r: [u32; 4]) -> Plan {
        let mut p = self.clone();
        p.geo = self.geo.window(r);
        p.window = None;
        p.with_region_context(raw, true, adj)
    }

    /// For region (tile) renders: grow the render by the context the neighbourhood filters
    /// need and anchor their block grids to the full frame, so tiles match a full render
    /// and have no seams.
    fn with_region_context(mut self, raw: &RawImage, region: bool, adj: &Adjustments) -> Self {
        if region {
            let mut apron = 8usize;
            if self.needs_hdr() {
                apron = apron.max(self.hdr_gp.reach());
            }
            if self.needs_detail() {
                apron = apron
                    .max(self.clarity_gp.reach())
                    .max(self.structure_gp.reach())
                    .max(self.texture_gp.reach());
            }
            if self.needs_dehaze() {
                apron = apron.max(self.dehaze_gp.reach());
            }
            if self.nr.chroma_active() {
                apron = apron.max(self.nr.gp.reach());
            }
            if let Some(sp) = &self.sharpen {
                apron = apron.max(sp.kernel.len() + 2);
            }
            if let Some(lb) = &self.lens_blur {
                apron = apron.max(lb.reach());
            }
            apron = apron.max(self.fx.reach());
            let long = self.geo.long_edge_out();
            let reach = |f: f32| {
                if f.is_finite() && f > 0.0 {
                    (3.0 * f.min(lt_core::adjust::MAX_PLANE_FEATHER) * long) as usize + 2
                } else {
                    0
                }
            };
            for lp in &self.layers {
                let mask = &adj.layers[lp.index].mask;
                // Output-space shape planes: a feathered colour range, auto-masked
                // strokes (their reference averages over about the brush radius); the
                // whole-mask feather blurs on top of them.
                let mut shapes = 0usize;
                for shape in &mask.shapes {
                    match shape {
                        lt_core::adjust::MaskShape::ColorRange { feather, .. } => {
                            let f = if feather.is_finite() {
                                feather.clamp(0.0, lt_core::adjust::MAX_PLANE_FEATHER)
                            } else {
                                0.0
                            };
                            shapes = shapes.max(
                                crate::mask::cr_params(f * long, long).reach()
                                    + crate::mask::CR_GUIDE_SMOOTH,
                            );
                        }
                        lt_core::adjust::MaskShape::Brush {
                            strokes, feather, ..
                        } if strokes.iter().any(|s| s.auto_mask && !s.erase) => {
                            let size = strokes
                                .iter()
                                .filter(|s| s.auto_mask && s.size.is_finite())
                                .map(|s| s.size)
                                .fold(0.0f32, f32::max);
                            let radius = crate::mask::auto_radius(size, long);
                            shapes = shapes.max((3.0 * radius) as usize + 8 + reach(*feather));
                        }
                        _ => {}
                    }
                }
                apron = apron.max(shapes + reach(mask.feather));
            }
            // Bounded by the normalized limits (MAX_PLANE_FEATHER, AUTO_REF_MAX).
            let cap = (0.4 * long).max(1024.0) as usize;
            let (geo, window) = self.geo.with_apron(apron.min(cap) as u32);
            self.geo = geo;
            self.window = Some(window);
            self.spots = crate::spots::plan(&adj.spots, &self.geo);
        }
        self.sensor_bbox = spot_bbox(sensor_bbox(&self.geo, raw), &self.geo, &self.spots, raw);
        let origin = self.geo.origin_out.map(|v| v.round() as i64);
        self.hdr_gp = self.hdr_gp.anchored(origin);
        self.clarity_gp = self.clarity_gp.anchored(origin);
        self.structure_gp = self.structure_gp.anchored(origin);
        self.texture_gp = self.texture_gp.anchored(origin);
        self.dehaze_gp = self.dehaze_gp.anchored(origin);
        self.nr.gp = self.nr.gp.anchored(origin);
        self.lens_blur = self.lens_blur.map(|lb| lb.anchored(origin));
        self.fx = std::mem::take(&mut self.fx).anchored(origin);
        self
    }
}
