//! Exposure matching across a set ("Match Total Exposures").
//!
//! Every frame is metered on a small render of its edit, in **linear light**: the
//! [`meter`] is the centre-weighted median luminance of the pixels that are neither
//! clipped (any channel at the output's white) nor black.
//!
//! Sensor clipping needs more care: a blown region renders at whatever level the edit
//! maps the sensor's clip to, which is below the output's white when the frame is
//! darkened. The caller therefore also renders what a *fully clipped* sensor looks
//! like with the same edit (its "clip level"), and [`compare`] meters reference and
//! target below a **common ceiling** a little under the lower of the two clip levels.
//! Both frames then lose the same scene radiances: a bracket pushed into clipping
//! meters like the unclipped reference (the populations match), and a blown window
//! that only one frame has is left out instead of pulling its median up.
//!
//! The render's tone curve is not linear, so one ratio of medians doesn't land on the
//! reference: [`solve_delta`] iterates (render with a delta, meter, secant step) until
//! the target's median is within [`SolveConfig::tolerance_ev`] of the reference's.
//!
//! EXIF (shutter, aperture, ISO) is the prior: when metering is unreliable (too few
//! usable pixels) the delta comes from the difference of the frames' total exposures
//! (camera exposure + exposure slider), as Lightroom's command does; when both exist,
//! a large disagreement is reported (most likely the frames don't show the same scene).
//!
//! Everything here is pure: rendering is the caller's closure.

/// How a frame is metered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterConfig {
    /// A pixel whose largest channel reaches this (linear, 1 = output white) is clipped.
    pub clip: f32,
    /// A pixel whose luminance is at or below this is black (no information).
    pub floor: f32,
    /// Weighted fraction of usable pixels below which the meter is unreliable.
    pub min_valid: f32,
    /// Weight of the frame's corners relative to its centre (1 = flat average).
    pub edge_weight: f32,
    /// Luminance coefficients of the RGB the caller hands in (default: linear Display P3).
    pub luma: [f32; 3],
    /// The common ceiling of [`compare`] is this fraction of the lower clip level.
    pub clip_margin: f32,
}

impl Default for MeterConfig {
    fn default() -> Self {
        Self {
            clip: 0.98,
            floor: 1.0e-4,
            min_valid: 0.2,
            edge_weight: 0.25,
            luma: [0.228_974_6, 0.691_738_5, 0.079_286_9],
            clip_margin: 0.9,
        }
    }
}

/// What [`meter`] measured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Meter {
    /// Weighted median luminance of the usable pixels (linear); 0 when there are none.
    pub median: f32,
    /// Weighted fraction of usable pixels (0..=1).
    pub valid: f32,
}

impl Meter {
    /// Enough usable pixels to trust the median.
    pub fn is_reliable(&self, cfg: &MeterConfig) -> bool {
        self.valid >= cfg.min_valid && self.median.is_finite() && self.median > cfg.floor
    }

    /// `log2(median)` when reliable.
    pub fn log2(&self, cfg: &MeterConfig) -> Option<f32> {
        self.is_reliable(cfg).then(|| self.median.log2())
    }
}

/// Histogram range in stops (linear 2^-20 ..= 2^4) and its resolution.
const LOG_MIN: f32 = -20.0;
const LOG_MAX: f32 = 4.0;
const BINS_PER_EV: f32 = 128.0;
// 24 stops × 128 bins; the range constants are small whole numbers.
const BINS: usize = ((LOG_MAX - LOG_MIN) * BINS_PER_EV) as usize;

/// A frame's luminance, ready to meter below any ceiling. Pixels clipped in the output
/// (any channel at [`MeterConfig::clip`]) are stored as `+∞`, unusable ones as NaN.
#[derive(Debug, Clone, PartialEq)]
pub struct LumaPlane {
    lum: Vec<f32>,
    width: usize,
    height: usize,
}

impl LumaPlane {
    /// Build from a `width`×`height` linear RGB image (row-major).
    ///
    /// # Panics
    /// If `rgb.len() != width × height`.
    pub fn new(rgb: &[[f32; 3]], width: usize, height: usize, cfg: &MeterConfig) -> Self {
        assert_eq!(
            width.checked_mul(height),
            Some(rgb.len()),
            "image size mismatch"
        );
        let lum = rgb
            .iter()
            .map(|px| {
                let l = px[0] * cfg.luma[0] + px[1] * cfg.luma[1] + px[2] * cfg.luma[2];
                let peak = px[0].max(px[1]).max(px[2]);
                if !l.is_finite() || !peak.is_finite() {
                    f32::NAN
                } else if peak >= cfg.clip {
                    f32::INFINITY
                } else {
                    l
                }
            })
            .collect();
        Self { lum, width, height }
    }

    /// Meter the usable pixels darker than `ceiling` (linear luminance; `∞` or NaN =
    /// only the output-clipped ones are out).
    pub fn meter(&self, ceiling: f32, cfg: &MeterConfig) -> Meter {
        let (w, h) = (self.width, self.height);
        let mut bins = vec![0f64; BINS];
        let (mut total, mut usable) = (0f64, 0f64);
        if w == 0 || h == 0 {
            return Meter {
                median: 0.0,
                valid: 0.0,
            };
        }
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let edge = cfg.edge_weight.clamp(0.0, 1.0);
        // Squared distance from the centre along each axis, once per column
        // and per row instead of per pixel.
        let squared = |n: usize, c: f32| -> Vec<f32> {
            (0..n)
                .map(|k| {
                    let d = (k as f32 + 0.5 - c) / c.max(0.5);
                    d * d
                })
                .collect()
        };
        let (sx, sy) = (squared(w, cx), squared(h, cy));
        for (row, &dy2) in self.lum.chunks_exact(w).zip(&sy) {
            for (&l, &dx2) in row.iter().zip(&sx) {
                // 1 at the centre, `edge` at the corners (r² = 2).
                let wt = f64::from(edge + (1.0 - edge) * (1.0 - f32::midpoint(dx2, dy2)).max(0.0));
                total += wt;
                if l.is_nan() || l >= ceiling || l <= cfg.floor {
                    continue;
                }
                usable += wt;
                bins[bin(l.log2())] += wt;
            }
        }
        if usable <= 0.0 {
            return Meter {
                median: 0.0,
                valid: 0.0,
            };
        }
        Meter {
            median: weighted_median(&bins, usable).exp2(),
            valid: (usable / total) as f32,
        }
    }
}

/// Meter a `width`×`height` linear RGB image (row-major) with no ceiling.
///
/// # Panics
/// If `rgb.len() != width × height`.
pub fn meter(rgb: &[[f32; 3]], width: usize, height: usize, cfg: &MeterConfig) -> Meter {
    LumaPlane::new(rgb, width, height, cfg).meter(f32::INFINITY, cfg)
}

/// A metered frame: its luminance and the luminance a fully clipped sensor renders at
/// with the same edit (`∞` when unknown).
#[derive(Debug, Clone, PartialEq)]
pub struct Metered {
    pub plane: LumaPlane,
    pub clip_level: f32,
}

/// How far (stops) `target`'s median lies above `reference`'s, both metered below
/// the common ceiling (`clip_margin` × the lower clip level). `None` when either is
/// unreliable there.
pub fn compare(reference: &Metered, target: &Metered, cfg: &MeterConfig) -> Option<f32> {
    let level = |l: f32| {
        if l.is_finite() && l > 0.0 {
            l
        } else {
            f32::INFINITY
        }
    };
    let ceiling = level(reference.clip_level).min(level(target.clip_level)) * cfg.clip_margin;
    let r = reference.plane.meter(ceiling, cfg).log2(cfg)?;
    let t = target.plane.meter(ceiling, cfg).log2(cfg)?;
    Some(t - r)
}

fn bin(log2: f32) -> usize {
    // Clamped into 0..BINS first, so the cast is in range.
    (((log2 - LOG_MIN) * BINS_PER_EV).clamp(0.0, (BINS - 1) as f32)) as usize
}

/// The log2 value where the cumulative weight reaches half of `total`, interpolated
/// inside the bin.
fn weighted_median(hist: &[f64], total: f64) -> f32 {
    let half = total / 2.0;
    let mut acc = 0.0;
    for (i, &w) in hist.iter().enumerate() {
        if w > 0.0 && acc + w >= half {
            let frac = ((half - acc) / w) as f32;
            return LOG_MIN + (i as f32 + frac) / BINS_PER_EV;
        }
        acc += w;
    }
    LOG_MAX
}

/// A frame's camera exposure from EXIF.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExifExposure {
    /// Exposure time in seconds.
    pub shutter_s: f32,
    /// F-number.
    pub aperture: f32,
    /// ISO sensitivity.
    pub iso: f32,
}

impl ExifExposure {
    /// How much light the sensor recorded, in stops: `log2(t · ISO/100 / N²)`
    /// (−EV100). `None` for missing or nonsensical values.
    pub fn brightness_ev(&self) -> Option<f32> {
        let ok = |v: f32| v.is_finite() && v > 0.0;
        (ok(self.shutter_s) && ok(self.aperture) && ok(self.iso))
            .then(|| (self.shutter_s * self.iso / 100.0 / (self.aperture * self.aperture)).log2())
    }
}

/// The exposure change that gives `target` the reference's total exposure (camera
/// exposure + exposure slider), assuming both show the same scene.
pub fn exif_delta(
    reference: Option<ExifExposure>,
    reference_ev: f32,
    target: Option<ExifExposure>,
    target_ev: f32,
) -> Option<f32> {
    let r = reference?.brightness_ev()?;
    let t = target?.brightness_ev()?;
    let d = (r + reference_ev) - (t + target_ev);
    d.is_finite().then_some(d)
}

/// How [`solve_delta`] iterates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolveConfig {
    /// Stop once the metered median is this close to the reference (stops).
    pub tolerance_ev: f32,
    /// Most renders per target (the first one is at the current edit).
    pub max_renders: u32,
    /// Slope bounds of the secant (d log2(median) / d exposure).
    pub min_slope: f32,
    pub max_slope: f32,
}

impl Default for SolveConfig {
    fn default() -> Self {
        Self {
            tolerance_ev: 0.02,
            max_renders: 5,
            min_slope: 0.3,
            max_slope: 3.0,
        }
    }
}

/// A converged (or best-effort) delta.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Solved {
    /// Exposure change to apply (stops), within the bounds.
    pub delta_ev: f32,
    /// Remaining difference of the last measurement from the reference (stops; the
    /// measurement at `delta_ev` when converged, else the last one taken).
    pub residual_ev: f32,
    /// Renders taken.
    pub renders: u32,
    /// Whether the last measurement was within tolerance.
    pub converged: bool,
}

/// Find the exposure delta `d` (within `bounds`) at which `measure(d)` — how far (stops)
/// the target rendered with `d` added to its exposure meters above the reference
/// ([`compare`]), `None` when unreliable — is zero. Starts at `d = 0`, then secant
/// steps (unit slope for the first). A step that meters unreliably (pushed into
/// clipping) is halved back toward the last reliable point. `Ok(None)` when the first
/// measurement is unreliable, or the renders run out on an unreliable one.
///
/// # Errors
/// The first error `measure` returns.
pub fn solve_delta<E>(
    bounds: (f32, f32),
    cfg: &SolveConfig,
    mut measure: impl FnMut(f32) -> Result<Option<f32>, E>,
) -> Result<Option<Solved>, E> {
    let (lo, hi) = (bounds.0.min(0.0), bounds.1.max(0.0));
    // The last reliable measurement (d, error).
    let mut prev: Option<(f32, f32)> = None;
    let mut d = 0.0f32;
    let mut renders = 0;
    loop {
        let measured = measure(d)?;
        renders += 1;
        let Some(err) = measured else {
            match prev {
                Some((pd, _)) if renders < cfg.max_renders => {
                    d = f32::midpoint(pd, d);
                    continue;
                }
                _ => return Ok(None),
            }
        };
        if err.abs() <= cfg.tolerance_ev {
            return Ok(Some(Solved {
                delta_ev: d,
                residual_ev: err,
                renders,
                converged: true,
            }));
        }
        let slope = match prev {
            Some((pd, pe)) if (d - pd).abs() > 1e-4 => {
                ((err - pe) / (d - pd)).clamp(cfg.min_slope, cfg.max_slope)
            }
            _ => 1.0,
        };
        let next = (d - err / slope).clamp(lo, hi);
        // Out of renders, or pinned at a bound: report the best estimate unverified.
        if renders >= cfg.max_renders || (next - d).abs() < 1e-4 {
            return Ok(Some(Solved {
                delta_ev: next,
                residual_ev: err,
                renders,
                converged: false,
            }));
        }
        prev = Some((d, err));
        d = next;
    }
}

/// Where a match came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchSource {
    /// Metered on renders (the primary path).
    Metered,
    /// Metering was unreliable: the EXIF exposure difference.
    Exif,
    /// Neither: the target is left as it is.
    Unmatched,
}

/// One target's result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExposureMatch {
    /// Add to the target's exposure slider (stops; 0 when unmatched). Keeps the slider
    /// within −5..=5.
    pub delta_ev: f32,
    pub source: MatchSource,
    /// What EXIF alone suggests, when known.
    pub exif_delta_ev: Option<f32>,
    /// Metered difference left over (metered matches).
    pub residual_ev: Option<f32>,
    /// Renders taken.
    pub renders: u32,
    /// Metering and EXIF disagree by more than [`MatchConfig::sanity_ev`].
    pub exif_disagrees: bool,
}

/// Configuration of [`match_exposure`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchConfig {
    pub meter: MeterConfig,
    pub solve: SolveConfig,
    /// Metered and EXIF deltas further apart than this (stops) are flagged.
    pub sanity_ev: f32,
    /// The exposure slider's range.
    pub ev_range: (f32, f32),
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            meter: MeterConfig::default(),
            solve: SolveConfig::default(),
            sanity_ev: 3.0,
            ev_range: (-5.0, 5.0),
        }
    }
}

/// Match one target to the reference (`meterable`: whether the reference meters
/// reliably). `current_ev` is the target's exposure slider; `exif` the EXIF delta
/// ([`exif_delta`]); `measure` as in [`solve_delta`]. Metering wins when it works;
/// EXIF is the fallback.
///
/// # Errors
/// The first error `measure` returns.
pub fn match_exposure<E>(
    meterable: bool,
    current_ev: f32,
    exif: Option<f32>,
    cfg: &MatchConfig,
    measure: impl FnMut(f32) -> Result<Option<f32>, E>,
) -> Result<ExposureMatch, E> {
    let current = if current_ev.is_finite() {
        current_ev.clamp(cfg.ev_range.0, cfg.ev_range.1)
    } else {
        0.0
    };
    let bounds = (cfg.ev_range.0 - current, cfg.ev_range.1 - current);
    let exif_delta_ev = exif.filter(|d| d.is_finite());
    let metered = if meterable {
        solve_delta(bounds, &cfg.solve, measure)?
    } else {
        None
    };
    let clamp = |d: f32| d.clamp(bounds.0.min(0.0), bounds.1.max(0.0));
    Ok(match (metered, exif_delta_ev) {
        (Some(s), _) => ExposureMatch {
            delta_ev: s.delta_ev,
            source: MatchSource::Metered,
            exif_delta_ev,
            residual_ev: Some(s.residual_ev),
            renders: s.renders,
            exif_disagrees: exif_delta_ev.is_some_and(|e| (e - s.delta_ev).abs() > cfg.sanity_ev),
        },
        (None, Some(e)) => ExposureMatch {
            delta_ev: clamp(e),
            source: MatchSource::Exif,
            exif_delta_ev,
            residual_ev: None,
            renders: 0,
            exif_disagrees: false,
        },
        (None, None) => ExposureMatch {
            delta_ev: 0.0,
            source: MatchSource::Unmatched,
            exif_delta_ev: None,
            residual_ev: None,
            renders: 0,
            exif_disagrees: false,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gray ramp (log-uniform luminance 2^-8..2^0 left to right) times `gain`.
    fn ramp(w: usize, h: usize, gain: f32) -> Vec<[f32; 3]> {
        (0..w * h)
            .map(|i| {
                let x = (i % w) as f32 / (w - 1) as f32;
                let v = ((x * 8.0 - 8.0).exp2() * gain).min(1.0);
                [v; 3]
            })
            .collect()
    }

    /// A "tone curve": x / (1 + x) scaled, then the image clipped at 1.
    fn toned(lin: f32) -> f32 {
        (1.6 * lin / (1.0 + lin)).min(1.0)
    }

    #[test]
    fn meter_of_a_flat_gray_is_its_luminance() {
        let img = vec![[0.18f32; 3]; 64 * 48];
        let m = meter(&img, 64, 48, &MeterConfig::default());
        assert!((m.median.log2() - 0.18f32.log2()).abs() < 0.01, "{m:?}");
        assert!((m.valid - 1.0).abs() < 1e-6);
        assert!(m.is_reliable(&MeterConfig::default()));
        assert!(m.log2(&MeterConfig::default()).is_some());
    }

    #[test]
    fn meter_scales_with_exposure() {
        let cfg = MeterConfig::default();
        let a = meter(&ramp(256, 64, 0.5), 256, 64, &cfg);
        let b = meter(&ramp(256, 64, 0.25), 256, 64, &cfg);
        assert!(
            ((a.median / b.median).log2() - 1.0).abs() < 0.02,
            "{a:?} {b:?}"
        );
    }

    #[test]
    fn meter_leaves_clipped_and_black_pixels_out() {
        let cfg = MeterConfig::default();
        // Homogeneous texture (two tones) with a blown block and a black block.
        let (w, h) = (64usize, 64usize);
        let mut img: Vec<[f32; 3]> = (0..w * h)
            .map(|i| {
                if (i % w + i / w) % 2 == 0 {
                    [0.1; 3]
                } else {
                    [0.3; 3]
                }
            })
            .collect();
        let base = meter(&img, w, h, &cfg);
        assert!((base.valid - 1.0).abs() < 1e-6);
        for y in 0..24 {
            for x in 0..24 {
                img[y * w + x] = [1.0, 1.0, 1.0];
                img[(y + 40) * w + x + 40] = [0.0; 3];
            }
        }
        let blown = meter(&img, w, h, &cfg);
        assert!(blown.valid < base.valid);
        // Same texture remains: the median is between the two tones as before.
        assert!(blown.median > 0.09 && blown.median < 0.31, "{blown:?}");
        // A clipped channel alone excludes the pixel; NaN is ignored.
        let img = [[2.0, 0.1, 0.1], [f32::NAN, 0.1, 0.1], [0.2, 0.2, 0.2]];
        let m = meter(&img, 3, 1, &cfg);
        assert!((m.median - 0.2).abs() < 0.01, "{m:?}");
    }

    #[test]
    fn meter_weights_the_centre() {
        let cfg = MeterConfig::default();
        let (w, h) = (101usize, 101usize);
        // Bright centre disc (r < 30 px), dark surround: centre weighting pulls the
        // median to the centre's tone even though the surround has more pixels.
        let img: Vec<[f32; 3]> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w) as f32 - 50.0, (i / w) as f32 - 50.0);
                if x * x + y * y < 900.0 {
                    [0.4; 3]
                } else {
                    [0.05; 3]
                }
            })
            .collect();
        let centre = meter(&img, w, h, &cfg);
        let flat = meter(
            &img,
            w,
            h,
            &MeterConfig {
                edge_weight: 1.0,
                ..cfg
            },
        );
        assert!(centre.median > flat.median, "{centre:?} {flat:?}");
    }

    #[test]
    fn empty_or_unusable_images_are_unreliable() {
        let cfg = MeterConfig::default();
        let m = meter(&[], 0, 0, &cfg);
        assert_eq!(
            m,
            Meter {
                median: 0.0,
                valid: 0.0
            }
        );
        let all_white = vec![[1.0f32; 3]; 16];
        let m = meter(&all_white, 4, 4, &cfg);
        assert!(!m.is_reliable(&cfg) && m.log2(&cfg).is_none());
        // Mostly clipped: few usable pixels.
        let mut img = vec![[1.0f32; 3]; 100];
        img[50] = [0.2; 3];
        assert!(!meter(&img, 10, 10, &cfg).is_reliable(&cfg));
    }

    #[test]
    fn median_saturates_at_the_histogram_ends() {
        let cfg = MeterConfig {
            clip: f32::INFINITY,
            ..MeterConfig::default()
        };
        let m = meter(&[[1000.0; 3]], 1, 1, &cfg);
        assert!((m.median.log2() - LOG_MAX).abs() < 0.01, "{m:?}");
        assert_eq!(weighted_median(&[0.0; 4], 1.0), LOG_MAX);
    }

    #[test]
    fn histogram_meters_below_a_ceiling() {
        let cfg = MeterConfig::default();
        // Half the frame at 0.05, half at 0.5 (the right half).
        let img: Vec<[f32; 3]> = (0..64 * 32)
            .map(|i| if i % 64 < 32 { [0.05; 3] } else { [0.5; 3] })
            .collect();
        let h = LumaPlane::new(&img, 64, 32, &cfg);
        let all = h.meter(f32::INFINITY, &cfg);
        assert_eq!(all, h.meter(f32::NAN, &cfg));
        assert!((all.valid - 1.0).abs() < 1e-6);
        let low = h.meter(0.3, &cfg);
        assert!((low.median.log2() - 0.05f32.log2()).abs() < 0.01, "{low:?}");
        assert!((low.valid - 0.5).abs() < 0.01, "{low:?}");
        assert_eq!(
            h.meter(0.0, &cfg),
            Meter {
                median: 0.0,
                valid: 0.0
            }
        );
        assert_eq!(h.meter(100.0, &cfg), all);
    }

    fn metered(level: f32, k: f32, clip_level: f32, cfg: &MeterConfig) -> Metered {
        let img: Vec<[f32; 3]> = ramp(256, 16, level * k.exp2())
            .into_iter()
            .map(|p| p.map(|v| v.min(clip_level)))
            .collect();
        Metered {
            plane: LumaPlane::new(&img, 256, 16, cfg),
            clip_level,
        }
    }

    #[test]
    fn compare_meters_both_frames_below_the_lower_clip_level() {
        let cfg = MeterConfig::default();
        // Reference clips at the output white (1.0), the target's sensor at 0.4: without
        // the common ceiling its plateau at 0.4 would count, with it both lose the same
        // tones and the difference is the exposure difference (here none).
        let r = metered(1.0, 0.0, 1.0, &cfg);
        let t = metered(1.0, 0.0, 0.4, &cfg);
        assert!(
            compare(&r, &t, &cfg).unwrap().abs() < 0.02,
            "{:?}",
            compare(&r, &t, &cfg)
        );
        // A stop darker: negative (less than a stop, as both are cut at the same
        // ceiling; zero only when matched, which is where the solver stops).
        let t = metered(1.0, -1.0, 0.4, &cfg);
        let d = compare(&r, &t, &cfg).unwrap();
        assert!(d < -0.3 && d > -1.05, "{d}");
        // Unknown clip levels count as no ceiling.
        let t = Metered {
            clip_level: f32::NAN,
            ..metered(1.0, 0.0, 1.0, &cfg)
        };
        assert!(compare(&r, &t, &cfg).unwrap().abs() < 0.02);
        // A ceiling below everything: unreliable.
        let t = metered(1.0, 0.0, 1e-4, &cfg);
        assert_eq!(compare(&r, &t, &cfg), None);
        assert_eq!(compare(&t, &r, &cfg), None);
    }

    #[test]
    #[should_panic(expected = "image size mismatch")]
    fn meter_rejects_a_wrong_size() {
        let _ = meter(&[[0.1; 3]; 3], 2, 2, &MeterConfig::default());
    }

    #[test]
    #[should_panic(expected = "image size mismatch")]
    fn a_size_that_overflows_is_a_mismatch_not_a_wrapped_match() {
        // usize::MAX × 2 wraps to usize::MAX - 1, never 0; 2^32 × 2^32 wraps to 0.
        let _ = LumaPlane::new(&[], 1 << 32, 1 << 32, &MeterConfig::default());
    }

    #[test]
    fn empty_planes_meter_as_nothing() {
        let cfg = MeterConfig::default();
        for (w, h) in [(0, 0), (0, 5), (5, 0)] {
            let m = LumaPlane::new(&[], w, h, &cfg).meter(f32::INFINITY, &cfg);
            assert_eq!((m.median, m.valid), (0.0, 0.0));
        }
    }

    /// The per-pixel formula the separable weights replace.
    fn slow_meter(p: &LumaPlane, ceiling: f32, cfg: &MeterConfig) -> Meter {
        let (w, h) = (p.width, p.height);
        let mut bins = vec![0f64; BINS];
        let (mut total, mut usable) = (0f64, 0f64);
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let edge = cfg.edge_weight.clamp(0.0, 1.0);
        for (i, &l) in p.lum.iter().enumerate() {
            let dx = ((i % w) as f32 + 0.5 - cx) / cx.max(0.5);
            let dy = ((i / w) as f32 + 0.5 - cy) / cy.max(0.5);
            let wt =
                f64::from(edge + (1.0 - edge) * (1.0 - f32::midpoint(dx * dx, dy * dy)).max(0.0));
            total += wt;
            if l.is_nan() || l >= ceiling || l <= cfg.floor {
                continue;
            }
            usable += wt;
            bins[bin(l.log2())] += wt;
        }
        Meter {
            median: weighted_median(&bins, usable).exp2(),
            valid: (usable / total) as f32,
        }
    }

    #[test]
    fn separable_weights_match_the_per_pixel_formula_exactly() {
        let cfg = MeterConfig::default();
        for (w, h, gain) in [(64, 48, 0.7), (257, 31, 0.2), (2, 40, 0.5), (40, 1, 0.5)] {
            let mut img = ramp(w, h, gain);
            if let Some(px) = img.get_mut(w / 2) {
                *px = [1.0; 3];
            }
            let p = LumaPlane::new(&img, w, h, &cfg);
            for ceiling in [f32::INFINITY, 0.3] {
                let (fast, slow) = (p.meter(ceiling, &cfg), slow_meter(&p, ceiling, &cfg));
                assert_eq!(fast, slow, "{w}x{h} ceiling {ceiling}");
            }
        }
    }

    #[test]
    fn exif_brightness_and_delta() {
        let e = |t: f32, n: f32, iso: f32| ExifExposure {
            shutter_s: t,
            aperture: n,
            iso,
        };
        // 1 s, f/1, ISO 100 = 0; one stop less light each way.
        assert_eq!(e(1.0, 1.0, 100.0).brightness_ev(), Some(0.0));
        let a = e(1.0 / 250.0, 4.0, 400.0);
        let b = e(1.0 / 500.0, 4.0, 400.0);
        let d = exif_delta(Some(a), 0.0, Some(b), 0.0).unwrap();
        assert!((d - 1.0).abs() < 1e-5, "{d}");
        // The sliders count: reference at +0.5, target at +0.25.
        let d = exif_delta(Some(a), 0.5, Some(b), 0.25).unwrap();
        assert!((d - 1.25).abs() < 1e-5, "{d}");
        // Aperture and ISO.
        let d = exif_delta(
            Some(e(0.01, 2.8, 100.0)),
            0.0,
            Some(e(0.01, 4.0, 200.0)),
            0.0,
        );
        // The target gains a stop of ISO and loses (4/2.8)² of light at f/4.
        assert!((d.unwrap() - ((4.0f32 / 2.8).powi(2).log2() - 1.0)).abs() < 1e-4);
        // Missing or nonsensical values.
        assert_eq!(e(0.0, 4.0, 100.0).brightness_ev(), None);
        assert_eq!(e(1.0, f32::NAN, 100.0).brightness_ev(), None);
        assert_eq!(e(1.0, 4.0, -1.0).brightness_ev(), None);
        assert_eq!(exif_delta(None, 0.0, Some(b), 0.0), None);
        assert_eq!(exif_delta(Some(a), 0.0, None, 0.0), None);
        assert_eq!(exif_delta(Some(a), f32::INFINITY, Some(b), 0.0), None);
    }

    /// `measure` for a target `k` stops darker than the reference, through `toned`.
    fn toned_measure(k: f32, scene: f32) -> impl FnMut(f32) -> Result<Option<f32>, ()> {
        move |d| {
            Ok(Some(
                toned(scene * (d - k).exp2()).log2() - toned(scene).log2(),
            ))
        }
    }

    #[test]
    fn solver_converges_through_a_tone_curve() {
        let scene = 0.3f32;
        for k in [-2.5f32, -1.3, -0.4, 0.0, 0.7, 1.5] {
            let s = solve_delta(
                (-5.0, 5.0),
                &SolveConfig::default(),
                toned_measure(k, scene),
            )
            .unwrap()
            .unwrap();
            assert!(s.converged, "{k}: {s:?}");
            assert!((s.delta_ev - k).abs() < 0.05, "{k}: {s:?}");
            assert!(s.renders <= 5);
        }
    }

    #[test]
    fn solver_stops_at_the_bounds_and_after_max_renders() {
        // Needs +3 but only +1 is allowed: pinned at the bound, unconverged.
        let mut calls = 0;
        let s = solve_delta((-5.0, 1.0), &SolveConfig::default(), |d| {
            calls += 1;
            Ok::<_, ()>(Some(d - 3.0))
        })
        .unwrap()
        .unwrap();
        assert_eq!(s.delta_ev, 1.0);
        assert!(!s.converged && (s.residual_ev + 2.0).abs() < 1e-5, "{s:?}");
        assert_eq!(calls, 2);
        // A meter that never responds (flat): slope clamped, renders capped.
        let cfg = SolveConfig {
            max_renders: 3,
            ..SolveConfig::default()
        };
        let s = solve_delta((-5.0, 5.0), &cfg, |_| Ok::<_, ()>(Some(-1.0)))
            .unwrap()
            .unwrap();
        assert_eq!(s.renders, 3);
        assert!(!s.converged);
    }

    #[test]
    fn solver_reports_unreliable_meters_and_errors() {
        let r = solve_delta((-5.0, 5.0), &SolveConfig::default(), |_| Ok::<_, ()>(None));
        assert_eq!(r, Ok(None));
        let r = solve_delta((-5.0, 5.0), &SolveConfig::default(), |_| {
            Err::<Option<f32>, _>("gpu")
        });
        assert_eq!(r, Err("gpu"));
        // Unreliable after a first good measurement, for good: renders run out.
        let mut n = 0;
        let r = solve_delta((-5.0, 5.0), &SolveConfig::default(), |_| {
            n += 1;
            Ok::<_, ()>((n == 1).then_some(-1.0))
        });
        assert_eq!((r, n), (Ok(None), 5));
        // Unreliable beyond +1.5 (clipped): the unit-slope overshoot to +1.8 is halved
        // back to +0.9, the root.
        let mut tried = Vec::new();
        let s = solve_delta((-5.0, 5.0), &SolveConfig::default(), |d| {
            tried.push(d);
            Ok::<_, ()>((d < 1.5).then_some(2.0 * d - 1.8))
        })
        .unwrap()
        .unwrap();
        assert_eq!(tried, [0.0, 1.8, 0.9]);
        assert!(s.converged && (s.delta_ev - 0.9).abs() <= 0.02, "{s:?}");
    }

    #[test]
    fn match_prefers_metering_and_falls_back_to_exif() {
        let cfg = MatchConfig::default();
        let scene = 0.3f32;
        let r = true;
        let m = match_exposure(r, 0.0, Some(1.2), &cfg, toned_measure(1.3, scene)).unwrap();
        assert_eq!(m.source, MatchSource::Metered);
        assert!((m.delta_ev - 1.3).abs() < 0.05, "{m:?}");
        assert!(!m.exif_disagrees && m.residual_ev.is_some() && m.renders > 0);
        assert_eq!(m.exif_delta_ev, Some(1.2));

        // EXIF far off: flagged, metering still wins.
        let m = match_exposure(r, 0.0, Some(-4.0), &cfg, toned_measure(1.3, scene)).unwrap();
        assert!(m.exif_disagrees && m.source == MatchSource::Metered);

        // Unreliable meter → EXIF, clamped into the slider range.
        let m = match_exposure(r, 4.5, Some(1.2), &cfg, |_| Ok::<_, ()>(None)).unwrap();
        assert_eq!(m.source, MatchSource::Exif);
        assert!((m.delta_ev - 0.5).abs() < 1e-6, "{m:?}");
        // Unreliable reference → EXIF without rendering.
        let m = match_exposure(
            false,
            0.0,
            Some(-0.7),
            &cfg,
            |_| -> Result<Option<f32>, ()> { panic!("no render without a reference meter") },
        )
        .unwrap();
        assert_eq!(
            (m.source, m.delta_ev, m.renders),
            (MatchSource::Exif, -0.7, 0)
        );
        // Nothing to go on.
        let m =
            match_exposure(false, f32::NAN, Some(f32::NAN), &cfg, |_| Ok::<_, ()>(None)).unwrap();
        assert_eq!(m.source, MatchSource::Unmatched);
        assert_eq!((m.delta_ev, m.exif_delta_ev), (0.0, None));
        // Errors propagate.
        assert_eq!(
            match_exposure(r, 0.0, None, &cfg, |_| Err::<Option<f32>, _>(7)),
            Err(7)
        );
    }

    #[test]
    fn metered_delta_respects_the_slider_range() {
        let cfg = MatchConfig::default();
        // Target at +4.8 needs +1.3: capped at +0.2 (slider at 5).
        let m = match_exposure(true, 4.8, None, &cfg, toned_measure(1.3, 0.3)).unwrap();
        assert!((m.delta_ev - 0.2).abs() < 1e-5, "{m:?}");
    }
}
