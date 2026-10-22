//! AI Optimize ([`ds_pipeline::optimize`]): analyze a photo and its edit, and apply the
//! chosen changes.
//!
//! Reports cross as JSON (`OptimizeReport`, serde): the app shows their texts, keeps the
//! applied report in the sidecar (`SessionHandle::set_optimize_report`) and hands it back
//! as `prior_report_json` so values an earlier optimize set aren't taken for the user's.
//! The analysis runs off the calling thread at `Priority::Tool`, through the shared
//! renderer (GPU statistics, CPU fallback). The probes render the original RAW, never an
//! AI Denoise blend, so with AI Denoise on they measure the noisier frame (docs/FFI.md).

use std::path::Path;
use std::sync::Arc;

use ds_core::Adjustments;
use ds_pipeline::optimize::{self, OptimizeReport, SceneSignals, SceneTag, SkyMask};
use ds_pipeline::{OutputFormat, RenderRequest, RenderedImage};

use crate::develop::develop;
use crate::error::DsError;
use crate::frame::NormRect;
use crate::jobs::{CancelToken, guard, run_job_on_thread};
use crate::viewer::{DevelopMaskSource, mask_source};

/// What the app's scene analysis found (Apple Vision; the sky model).
#[derive(Debug, Clone, PartialEq, Default, uniffi::Record)]
pub struct OptimizeSignals {
    pub faces: u32,
    /// Total face area, fraction of the frame.
    pub face_area: f32,
    /// Horizon tilt, degrees, positive = rises to the right (E frame).
    pub horizon_deg: Option<f32>,
    /// The salient subject, normalized in the E frame (y down).
    pub subject: Option<NormRect>,
    pub sky_fraction: f32,
    /// Sky mask over the E frame (≥ 128 = sky), row-major; empty = none.
    pub sky_width: u32,
    pub sky_height: u32,
    pub sky_mask: Vec<u8>,
    /// "landscape", "portrait", "night", "snow", "beach", "sunset", "backlit";
    /// others are ignored.
    pub tags: Vec<String>,
    pub ai_denoise_available: bool,
}

/// The adjustments with the chosen changes, and the report marked with what was applied.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct OptimizeApplied {
    pub adjustments_json: String,
    pub report_json: String,
}

fn tag(s: &str) -> Option<SceneTag> {
    Some(match s {
        "landscape" => SceneTag::Landscape,
        "portrait" => SceneTag::Portrait,
        "night" => SceneTag::Night,
        "snow" => SceneTag::Snow,
        "beach" => SceneTag::Beach,
        "sunset" => SceneTag::Sunset,
        "backlit" => SceneTag::Backlit,
        _ => return None,
    })
}

fn scene(s: OptimizeSignals) -> SceneSignals {
    let mask_ok = s.sky_width > 0
        && s.sky_height > 0
        && s.sky_mask.len() == s.sky_width as usize * s.sky_height as usize;
    SceneSignals {
        faces: s.faces,
        face_area: s.face_area,
        horizon_deg: s.horizon_deg,
        subject: s.subject.map(|r| ds_core::NormRect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        }),
        sky_fraction: s.sky_fraction,
        sky_mask: mask_ok.then_some(SkyMask {
            width: s.sky_width,
            height: s.sky_height,
            data: s.sky_mask,
        }),
        tags: s.tags.iter().filter_map(|t| tag(t)).collect(),
        ai_denoise_available: s.ai_denoise_available,
    }
}

fn parse_adj(json: &str) -> Result<Adjustments, DsError> {
    serde_json::from_str(json).map_err(|e| DsError::invalid(format!("adjustments JSON: {e}")))
}

fn parse_report(json: &str) -> Result<OptimizeReport, DsError> {
    serde_json::from_str(json).map_err(|e| DsError::invalid(format!("optimize report JSON: {e}")))
}

fn json<T: serde::Serialize>(v: &T) -> Result<String, DsError> {
    serde_json::to_string(v).map_err(|e| DsError::Internal {
        message: format!("JSON: {e}"),
    })
}

fn analyze(
    path: &Path,
    adj: &Adjustments,
    scene: &SceneSignals,
    prior: Option<&OptimizeReport>,
    masks: Option<Arc<dyn DevelopMaskSource>>,
    cancel: ds_pipeline::CancelToken,
) -> Result<OptimizeReport, DsError> {
    let d = develop();
    let loaded = d.load(path)?;
    let raw = &loaded.raw;
    let blocks = match d.gpu() {
        Some(g) => g.block_means(loaded.key, raw),
        None => ds_pipeline::auto::block_means(raw),
    };
    let input = optimize::Input {
        raw,
        adj,
        scene,
        prior,
        request: RenderRequest {
            capture: loaded.capture.clone(),
            masks: masks.map(mask_source),
            cancel: Some(cancel),
            ..RenderRequest::default()
        },
    };
    let mut failure = None;
    let report = optimize::analyze(&input, &blocks, &mut |a, req| {
        let size = ds_pipeline::output_size(raw, a, req)?;
        let mut out = vec![0u8; size.pixels() as usize * 4];
        // The original RAW, not an AI Denoise blend: the analysis measures the photo,
        // and a blend at Tool priority would cost a full-resolution mix per probe.
        match d.render_now(&loaded, a, req, OutputFormat::Rgba8, &mut out) {
            Ok(frame) => {
                let img = RenderedImage { size, rgba8: out };
                let h = frame
                    .histogram
                    .unwrap_or_else(|| ds_pipeline::histogram(&img));
                Ok((img, h))
            }
            Err(e) => {
                failure = Some(e);
                Err(ds_pipeline::PipelineError::Cancelled)
            }
        }
    });
    match (report, failure) {
        (_, Some(e)) => Err(e),
        (r, None) => r.map_err(crate::develop::pipeline_error),
    }
}

/// Analyze `path` with its edit `adjustments_json`: the report (JSON) of proposed changes
/// with their reasons and explanations. `prior_report_json` is the last applied report
/// of this photo; `masks` provides AI masks for layers that use them.
#[uniffi::export]
pub async fn optimize_analyze(
    path: String,
    adjustments_json: String,
    signals: OptimizeSignals,
    prior_report_json: Option<String>,
    masks: Option<Arc<dyn DevelopMaskSource>>,
    token: Arc<CancelToken>,
) -> Result<String, DsError> {
    let cancel = token.pipeline_token();
    run_job_on_thread("ds-optimize", token, move |cancelled| {
        let adj = parse_adj(&adjustments_json)?;
        let prior = prior_report_json.as_deref().map(parse_report).transpose()?;
        let scene = scene(signals);
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        json(&analyze(
            Path::new(&path),
            &adj,
            &scene,
            prior.as_ref(),
            masks,
            cancel,
        )?)
    })
    .await
}

/// Apply the changes `selected` (their ids) of `report_json` to `adjustments_json`.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn optimize_apply(
    adjustments_json: String,
    report_json: String,
    selected: Vec<u32>,
) -> Result<OptimizeApplied, DsError> {
    guard(|| {
        let adj = parse_adj(&adjustments_json)?;
        let report = parse_report(&report_json)?;
        Ok(OptimizeApplied {
            adjustments_json: json(&optimize::apply(&adj, &report, &selected).sanitized())?,
            report_json: json(&optimize::mark_applied(&report, &selected))?,
        })
    })
}

/// The ids of the applied changes of `report_json` whose values `adjustments_json` still
/// has ("Why these settings?" shows only those).
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)]
pub fn optimize_still_applied(
    adjustments_json: String,
    report_json: String,
) -> Result<Vec<u32>, DsError> {
    guard(|| {
        Ok(optimize::still_applied(
            &parse_adj(&adjustments_json)?,
            &parse_report(&report_json)?,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;

    fn signals() -> OptimizeSignals {
        OptimizeSignals {
            horizon_deg: Some(2.0),
            tags: vec!["landscape".into(), "unknown".into()],
            sky_width: 2,
            sky_height: 2,
            sky_mask: vec![255, 255, 0, 0],
            sky_fraction: 0.5,
            subject: Some(NormRect {
                x: 0.2,
                y: 0.2,
                w: 0.5,
                h: 0.5,
            }),
            ..OptimizeSignals::default()
        }
    }

    #[test]
    fn signals_convert() {
        let s = scene(signals());
        assert_eq!(s.tags, [SceneTag::Landscape]);
        assert!(s.sky_mask.is_some() && s.subject.is_some());
        for t in ["portrait", "night", "snow", "beach", "sunset", "backlit"] {
            assert!(tag(t).is_some(), "{t}");
        }
        // A mask whose size doesn't match is dropped.
        let bad = scene(OptimizeSignals {
            sky_mask: vec![1],
            ..signals()
        });
        assert!(bad.sky_mask.is_none());
    }

    #[test]
    fn analyze_and_apply_on_a_sample() {
        let Some(p) = ds_testdata::sample_arw() else {
            return;
        };
        let adj = json(&Adjustments::default()).unwrap();
        let t = std::time::Instant::now();
        let report = block_on(optimize_analyze(
            p.to_string_lossy().into(),
            adj.clone(),
            signals(),
            None,
            None,
            CancelToken::new(),
        ))
        .unwrap();
        println!(
            "optimize_analyze {:.0} ms ({})",
            t.elapsed().as_secs_f64() * 1e3,
            develop().backend()
        );
        let r = parse_report(&report).unwrap();
        assert!(!r.changes.is_empty() && !r.summary.is_empty());
        let straighten = r
            .changes
            .iter()
            .find(|c| c.field == optimize::Field::Straighten)
            .expect("straighten from the horizon signal");
        let ids: Vec<u32> = r.changes.iter().map(|c| c.id).collect();
        let applied = optimize_apply(adj.clone(), report.clone(), ids.clone()).unwrap();
        let a = parse_adj(&applied.adjustments_json).unwrap();
        assert!((a.crop.angle_deg + 2.0).abs() < 1e-4, "{straighten:?}");
        let marked = parse_report(&applied.report_json).unwrap();
        assert!(marked.changes.iter().all(|c| c.applied));
        let still = optimize_still_applied(
            applied.adjustments_json.clone(),
            applied.report_json.clone(),
        )
        .unwrap();
        assert_eq!(still, ids, "every applied change is in effect");
        assert!(
            optimize_still_applied(adj.clone(), applied.report_json.clone())
                .unwrap()
                .is_empty(),
            "undone"
        );
        assert!(optimize_still_applied("x".into(), applied.report_json.clone()).is_err());
        // A second run with the applied report as prior proposes nothing new.
        let again = block_on(optimize_analyze(
            p.to_string_lossy().into(),
            applied.adjustments_json.clone(),
            signals(),
            Some(applied.report_json.clone()),
            None,
            CancelToken::new(),
        ))
        .unwrap();
        let again = parse_report(&again).unwrap();
        assert!(
            again.changes.iter().all(|c| !c.default_on),
            "{:#?}",
            again.changes
        );
    }

    #[test]
    fn bad_input_is_invalid_argument() {
        assert!(matches!(
            optimize_apply("nope".into(), "{}".into(), vec![]),
            Err(DsError::InvalidArgument { .. })
        ));
        assert!(matches!(
            optimize_apply("{}".into(), "nope".into(), vec![]),
            Err(DsError::InvalidArgument { .. })
        ));
        let r = block_on(optimize_analyze(
            "/nonexistent/x.arw".into(),
            "{}".into(),
            OptimizeSignals::default(),
            Some("nope".into()),
            None,
            CancelToken::new(),
        ));
        assert!(matches!(r, Err(DsError::InvalidArgument { .. })));
        let r = block_on(optimize_analyze(
            "/nonexistent/x.arw".into(),
            "{}".into(),
            OptimizeSignals::default(),
            None,
            None,
            CancelToken::new(),
        ));
        assert!(matches!(r, Err(DsError::Io { .. })), "{r:?}");
        // An empty report applies nothing (`{}` has no `profile` key: Neutral).
        let same = optimize_apply("{}".into(), "{}".into(), vec![0]).unwrap();
        assert_eq!(
            parse_adj(&same.adjustments_json).unwrap(),
            parse_adj("{}").unwrap()
        );
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let r = block_on(optimize_analyze(
            "/nonexistent/x.arw".into(),
            "{}".into(),
            OptimizeSignals::default(),
            None,
            None,
            cancelled,
        ));
        assert!(matches!(r, Err(DsError::Cancelled)), "{r:?}");
    }
}
