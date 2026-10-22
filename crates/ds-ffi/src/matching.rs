//! Matching a set to its focused photo: "Match Total Exposures" and "Match White
//! Balance to Focused Photo".
//!
//! Exposure: every frame is rendered small (≤ [`METER_EDGE`] px, `Priority::Tool`, the
//! shared GPU renderer with the CPU fallback, linear Display P3 output) and metered by
//! `ds_ai::matching` — the centre-weighted median luminance of the unclipped, non-black
//! pixels below a ceiling under both frames' sensor-clip levels (each measured on a
//! tiny fully clipped probe frame rendered with the same edit). Each target's delta
//! is solved by re-rendering it with candidate deltas until its median meets the
//! reference's (the tone curve is not linear); EXIF
//! (shutter/aperture/ISO plus the sliders) is the fallback when metering is unreliable
//! and a sanity check otherwise. The app applies the deltas relatively
//! (`exposure_ev += delta`) as one undo step.
//!
//! White balance: each target gets the reference's Kelvin/tint, computed against the
//! target's own current white balance (As Shot is read through the camera matrix from
//! the file's tags, like the readout), so a target that already matches is left alone.

use std::path::Path;
use std::sync::Arc;

use ds_ai::matching::{
    self, ExifExposure, LumaPlane, MatchConfig, MatchSource as CoreSource, MeterConfig, Metered,
    compare, exif_delta,
};
use ds_core::adjust::WhiteBalance;
use ds_core::{Adjustments, RawImage};
use ds_pipeline::color::multipliers_to_wb;
use ds_pipeline::{ImageKey, OutputFormat, Priority, Quality, RenderRequest};

use crate::develop::{Loaded, develop, pipeline_error};
use crate::error::DsError;
use crate::jobs::{CancelToken, run_job_on_thread};
use crate::tools::{WbValue, camera_color, wb_of};

/// Long edge of the metering renders.
pub(crate) const METER_EDGE: u32 = 512;

/// One photo of a match: its original and its current adjustments.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct MatchPhoto {
    pub path: String,
    pub adjustments_json: String,
}

/// Where an exposure match came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ExposureMatchSource {
    /// Metered on renders.
    Metered,
    /// Metering was unreliable (mostly clipped or black): EXIF exposure difference.
    Exif,
    /// Neither worked: leave the target as it is.
    Unmatched,
}

impl From<CoreSource> for ExposureMatchSource {
    fn from(s: CoreSource) -> Self {
        match s {
            CoreSource::Metered => Self::Metered,
            CoreSource::Exif => Self::Exif,
            CoreSource::Unmatched => Self::Unmatched,
        }
    }
}

/// One target's exposure match.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ExposureMatchResult {
    /// Add this to the target's exposure (stops); keeps the slider within −5..=5.
    pub delta_ev: f32,
    pub source: ExposureMatchSource,
    /// What EXIF alone suggests, when both frames have it.
    pub exif_delta_ev: Option<f32>,
    /// Metered difference left after the last render (metered matches).
    pub residual_ev: Option<f32>,
    /// Metering renders taken for this target.
    pub renders: u32,
    /// Metering and EXIF disagree by more than 3 stops (likely a different scene).
    pub exif_disagrees: bool,
    /// Why this target could not be read or rendered (the match then used EXIF or
    /// nothing).
    pub error: Option<String>,
}

/// One target's white-balance match.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct WbMatchResult {
    /// The new white balance; `None` when the target already matches or failed.
    pub white_balance: Option<WbValue>,
    pub error: Option<String>,
}

/// A frame ready to meter.
#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub loaded: Loaded,
    pub adj: Adjustments,
    pub exif: Option<ExifExposure>,
    /// Decoded for this match only (not in the decoded-RAW cache): its renderer caches
    /// are dropped as soon as it is metered.
    pub transient: bool,
}

fn parse(json: &str) -> Result<Adjustments, DsError> {
    serde_json::from_str(json).map_err(|e| DsError::invalid(format!("adjustments JSON: {e}")))
}

/// Camera exposure from the file's metadata.
fn exif_of(meta: &ds_raw::ImageMeta) -> Option<ExifExposure> {
    Some(ExifExposure {
        shutter_s: meta.shutter?.to_f64() as f32,
        aperture: meta.aperture?,
        iso: meta.iso? as f32,
    })
}

/// Decode (the cached decode when the viewer has one; otherwise a transient one that
/// leaves the viewer's cache alone) and read the EXIF of a photo.
fn load_frame(p: &MatchPhoto) -> Result<Frame, DsError> {
    let adj = parse(&p.adjustments_json)?;
    let path = Path::new(&p.path);
    let (loaded, transient) = develop().load_transient(path)?;
    let exif = ds_raw::read_metadata(path).ok().as_ref().and_then(exif_of);
    Ok(Frame {
        loaded,
        adj,
        exif,
        transient,
    })
}

/// Long edge of the clip-level probe (large enough for any crop to keep pixels).
const PROBE_EDGE: u32 = 64;

/// Linear Display P3 RGB of a render of `loaded` with `adj` at ≤ `edge` px.
fn render_linear(
    loaded: &Loaded,
    adj: &Adjustments,
    edge: u32,
) -> Result<(Vec<[f32; 3]>, usize, usize), DsError> {
    let req = RenderRequest {
        quality: Quality::Preview,
        priority: Priority::Tool,
        ..RenderRequest::new(ds_core::Size::new(edge, edge))
    };
    let size = ds_pipeline::output_size(&loaded.raw, adj, &req).map_err(pipeline_error)?;
    let mut out = vec![0u8; size.pixels() as usize * 8];
    develop().render(loaded, adj, &req, OutputFormat::Rgba16fLinearP3, &mut out)?;
    let f = |px: &[u8; 8], c: usize| half::f16::from_le_bytes([px[c * 2], px[c * 2 + 1]]).to_f32();
    let rgb = out
        .as_chunks::<8>()
        .0
        .iter()
        .map(|px| [f(px, 0), f(px, 1), f(px, 2)])
        .collect();
    Ok((rgb, size.width as usize, size.height as usize))
}

/// A small frame like `raw` (same CFA, levels, white balance and camera) whose every
/// photosite is at the clip.
fn clipped_probe(raw: &RawImage) -> RawImage {
    let per_px = raw.data.len() / (raw.size.pixels() as usize).max(1);
    RawImage {
        size: ds_core::Size::new(PROBE_EDGE, PROBE_EDGE),
        cfa: raw.cfa,
        data: vec![
            raw.white_level.round() as u16;
            (PROBE_EDGE * PROBE_EDGE) as usize * per_px.max(1)
        ],
        black_level: raw.black_level,
        white_level: raw.white_level,
        wb_as_shot: raw.wb_as_shot,
        wb_daylight: raw.wb_daylight,
        cam_to_xyz: raw.cam_to_xyz,
        orientation: raw.orientation,
        default_crop: None,
    }
}

/// The cache key of `frame`'s clip probe: one per frame, so its BASE is reused across
/// the solver's renders.
fn probe_key(frame: &Frame) -> ImageKey {
    ImageKey(frame.loaded.key.0 ^ 0xC11B_C11B_C11B_C11B)
}

/// Drops the clip probes' renderer caches when a match ends (however it ends).
struct Probes(Vec<ImageKey>);

impl Drop for Probes {
    fn drop(&mut self) {
        for &k in &self.0 {
            develop().evict(k);
        }
    }
}

/// Drops a transient frame's renderer caches when the match is done with it (however
/// that ends).
struct Transient(Option<ImageKey>);

impl Transient {
    fn of(frame: &Frame) -> Self {
        Self(frame.transient.then_some(frame.loaded.key))
    }
}

impl Drop for Transient {
    fn drop(&mut self) {
        if let Some(k) = self.0 {
            develop().evict(k);
        }
    }
}

/// The luminance a fully clipped sensor renders at with `adj` (the brightest probe
/// pixel, so a vignette doesn't lower it); `∞` when the probe can't be rendered.
fn clip_level(frame: &Frame, adj: &Adjustments) -> f32 {
    let probe = Loaded {
        key: probe_key(frame),
        raw: Arc::new(clipped_probe(&frame.loaded.raw)),
        // The frame's own capture (lens, camera): the probe renders like the frame.
        capture: frame.loaded.capture.clone(),
    };
    let cfg = MeterConfig::default();
    render_linear(&probe, adj, PROBE_EDGE).map_or(f32::INFINITY, |(rgb, _, _)| {
        rgb.iter()
            .map(|p| p[0] * cfg.luma[0] + p[1] * cfg.luma[1] + p[2] * cfg.luma[2])
            .filter(|l| l.is_finite())
            .fold(f32::NEG_INFINITY, f32::max)
    })
}

/// Meter `frame` rendered with `delta` stops added to its exposure.
pub(crate) fn meter_frame(
    frame: &Frame,
    delta: f32,
    cfg: &MeterConfig,
) -> Result<Metered, DsError> {
    let mut adj = frame.adj.clone();
    adj.exposure.exposure_ev += delta;
    let (rgb, w, h) = render_linear(&frame.loaded, &adj, METER_EDGE)?;
    Ok(Metered {
        plane: LumaPlane::new(&rgb, w, h, cfg),
        clip_level: clip_level(frame, &adj),
    })
}

/// Match every target to `reference`. A target that can't be loaded or rendered is
/// matched by EXIF when possible and carries its error; a reference that can't be
/// rendered, or a cancel, fails the call.
pub(crate) fn match_frames(
    reference: &Frame,
    targets: impl IntoIterator<Item = Result<Frame, DsError>>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<ExposureMatchResult>, DsError> {
    let cfg = MatchConfig::default();
    let ref_ev = reference.adj.exposure.exposure_ev;
    let mut probes = Probes(vec![probe_key(reference)]);
    let _reference = Transient::of(reference);
    let ref_meter = meter_frame(reference, 0.0, &cfg.meter)?;
    let meterable = ref_meter
        .plane
        .meter(f32::INFINITY, &cfg.meter)
        .is_reliable(&cfg.meter);
    let mut out = Vec::new();
    for target in targets {
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        let frame = match target {
            Ok(f) => f,
            Err(DsError::Cancelled) => return Err(DsError::Cancelled),
            Err(e) => {
                out.push(failed(None, &e));
                continue;
            }
        };
        probes.0.push(probe_key(&frame));
        let _transient = Transient::of(&frame);
        let current = frame.adj.exposure.exposure_ev;
        let exif = exif_delta(reference.exif, ref_ev, frame.exif, current);
        let measured = matching::match_exposure(meterable, current, exif, &cfg, |d| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            Ok(compare(
                &ref_meter,
                &meter_frame(&frame, d, &cfg.meter)?,
                &cfg.meter,
            ))
        });
        out.push(match measured {
            Ok(m) => ExposureMatchResult {
                delta_ev: m.delta_ev,
                source: m.source.into(),
                exif_delta_ev: m.exif_delta_ev,
                residual_ev: m.residual_ev,
                renders: m.renders,
                exif_disagrees: m.exif_disagrees,
                error: None,
            },
            Err(DsError::Cancelled) => return Err(DsError::Cancelled),
            // A render failure: EXIF alone (no metering).
            Err(e) => failed(
                matching::match_exposure(false, current, exif, &cfg, |_| Ok::<_, DsError>(None))
                    .ok(),
                &e,
            ),
        });
    }
    Ok(out)
}

fn failed(fallback: Option<matching::ExposureMatch>, e: &DsError) -> ExposureMatchResult {
    let (delta_ev, source, exif_delta_ev) = fallback
        .map_or((0.0, ExposureMatchSource::Unmatched, None), |m| {
            (m.delta_ev, m.source.into(), m.exif_delta_ev)
        });
    ExposureMatchResult {
        delta_ev,
        source,
        exif_delta_ev,
        residual_ev: None,
        renders: 0,
        exif_disagrees: false,
        error: Some(e.to_string()),
    }
}

/// Kelvin/tint of `adj`'s white balance on `path`: As Shot through the camera matrix
/// (from the file's tags, no decode).
pub(crate) fn resolved_wb(path: &Path, adj: &Adjustments) -> Result<WbValue, DsError> {
    Ok(match adj.white_balance {
        WhiteBalance::Custom { .. } => wb_of(adj.white_balance),
        WhiteBalance::AsShot => {
            let (cam, as_shot) = camera_color(path)?;
            wb_of(multipliers_to_wb(as_shot, &cam))
        }
    })
}

/// The target's white balance after the change that brings it to `reference`,
/// measured against the target's own (resolved) white balance: `None` when that change
/// is below the sliders' rounding (0.5 K, 0.05 tint), else exactly the reference
/// (`target + (reference − target)`, without the float round trip), clamped to the
/// sliders.
pub(crate) fn relative_wb(reference: WbValue, target: WbValue) -> Option<WbValue> {
    let dk = reference.kelvin - target.kelvin;
    let dt = reference.tint - target.tint;
    if dk.abs() < 0.5 && dt.abs() < 0.05 {
        return None;
    }
    Some(WbValue {
        kelvin: reference.kelvin.clamp(2000.0, 50000.0),
        tint: reference.tint.clamp(-150.0, 150.0),
    })
}

fn match_wb(
    reference: &MatchPhoto,
    targets: &[MatchPhoto],
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<WbMatchResult>, DsError> {
    let r = resolved_wb(
        Path::new(&reference.path),
        &parse(&reference.adjustments_json)?,
    )?;
    targets
        .iter()
        .map(|t| {
            if cancelled() {
                return Err(DsError::Cancelled);
            }
            Ok(
                match parse(&t.adjustments_json).and_then(|a| resolved_wb(Path::new(&t.path), &a)) {
                    Ok(cur) => WbMatchResult {
                        white_balance: relative_wb(r, cur),
                        error: None,
                    },
                    Err(e) => WbMatchResult {
                        white_balance: None,
                        error: Some(e.to_string()),
                    },
                },
            )
        })
        .collect()
}

/// Match Total Exposures: per target, the exposure change that matches its metered
/// mid-tones to `reference` (see the module docs). One result per target, in order.
#[uniffi::export]
pub async fn match_exposures(
    reference: MatchPhoto,
    targets: Vec<MatchPhoto>,
    token: Arc<CancelToken>,
) -> Result<Vec<ExposureMatchResult>, DsError> {
    run_job_on_thread("ds-match", token, move |cancelled| {
        let r = load_frame(&reference)?;
        match_frames(&r, targets.iter().map(load_frame), cancelled)
    })
    .await
}

/// Match White Balance to Focused Photo: per target, the Kelvin/tint to set (`None`
/// when it already matches). One result per target, in order.
#[uniffi::export]
pub async fn match_white_balance(
    reference: MatchPhoto,
    targets: Vec<MatchPhoto>,
    token: Arc<CancelToken>,
) -> Result<Vec<WbMatchResult>, DsError> {
    run_job_on_thread("ds-match", token, move |cancelled| {
        match_wb(&reference, &targets, cancelled)
    })
    .await
}

#[cfg(test)]
#[path = "matching_tests.rs"]
mod tests;
