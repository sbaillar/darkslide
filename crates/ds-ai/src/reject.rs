//! Why a frame would be rejected: typed reasons and the note the app shows.
//!
//! Everything here is read from the [`CullScore`]s the culler already
//! computed (no new analysis): the frame's own [`Reason`]s, its components
//! next to the group's best frame, and the score margin [`suggest_picks`]
//! rejects on. [`explain_reject`] lists the reasons most important first;
//! [`reject_note`] joins their `Display` strings into one line.
//!
//! [`suggest_picks`]: crate::suggest_picks

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::cull::{CullScore, CullThresholds, Reason};

/// What the best frame of the group does better (its largest component lead).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Advantage {
    Sharper,
    BetterExposed,
    LessNoisy,
    /// No measured component is ahead (Vision signals decided).
    HigherScore,
}

/// One reason a frame would be rejected. `Display` gives the UI text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RejectReason {
    /// Sharpness components (0..=1) of this frame and of the group's best.
    OutOfFocus {
        sharpness: f32,
        best: Option<f32>,
    },
    MotionBlur {
        confirmed: bool,
        sharpness: f32,
        best: Option<f32>,
    },
    EyesClosed {
        confirmed: bool,
    },
    /// More than the reject margin below the group's best frame.
    Duplicate {
        /// The best frame's name, when the caller knows it.
        of: Option<String>,
        advantage: Advantage,
        score: f32,
        best_score: f32,
    },
    /// Sharpness relative to the group's sharpest frame, 0..=1.
    SofterThanBest {
        relative: f32,
    },
    Underexposed {
        mean: f32,
        stops: Option<f32>,
    },
    Overexposed {
        mean: f32,
        stops: Option<f32>,
    },
    HighlightsClipped {
        fraction: f32,
    },
    ShadowsClipped {
        fraction: f32,
    },
    Noisy {
        sigma: f32,
    },
    PoorFaceCapture,
    UtilityImage,
    LowAesthetics,
    /// Nothing specific: only the overall score is low.
    LowScore {
        score: f32,
    },
}

impl RejectReason {
    /// Stable `snake_case` code (the serde tag).
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutOfFocus { .. } => "out_of_focus",
            Self::MotionBlur { .. } => "motion_blur",
            Self::EyesClosed { .. } => "eyes_closed",
            Self::Duplicate { .. } => "duplicate",
            Self::SofterThanBest { .. } => "softer_than_best",
            Self::Underexposed { .. } => "underexposed",
            Self::Overexposed { .. } => "overexposed",
            Self::HighlightsClipped { .. } => "highlights_clipped",
            Self::ShadowsClipped { .. } => "shadows_clipped",
            Self::Noisy { .. } => "noisy",
            Self::PoorFaceCapture => "poor_face_capture",
            Self::UtilityImage => "utility_image",
            Self::LowAesthetics => "low_aesthetics",
            Self::LowScore { .. } => "low_score",
        }
    }

    /// A failure a pick should never have (see [`Reason::is_hard`]).
    pub fn is_hard(&self) -> bool {
        matches!(
            self,
            Self::OutOfFocus { .. }
                | Self::MotionBlur {
                    confirmed: true,
                    ..
                }
                | Self::EyesClosed { confirmed: true }
        )
    }

    /// Lower ranks first: hard failures, then losing to the best frame, then
    /// the softer signals.
    fn rank(&self) -> u8 {
        match self {
            Self::OutOfFocus { .. } => 0,
            Self::MotionBlur {
                confirmed: true, ..
            } => 1,
            Self::EyesClosed { confirmed: true } => 2,
            Self::Duplicate { .. } => 3,
            Self::SofterThanBest { .. } => 4,
            Self::Underexposed { .. } | Self::Overexposed { .. } => 5,
            Self::HighlightsClipped { .. } => 6,
            Self::ShadowsClipped { .. } => 7,
            Self::EyesClosed { confirmed: false } => 8,
            Self::MotionBlur {
                confirmed: false, ..
            } => 9,
            Self::Noisy { .. } => 10,
            Self::PoorFaceCapture => 11,
            Self::UtilityImage => 12,
            Self::LowAesthetics => 13,
            Self::LowScore { .. } => 14,
        }
    }
}

fn sharpness_detail(f: &mut fmt::Formatter<'_>, sharpness: f32, best: Option<f32>) -> fmt::Result {
    match best {
        Some(b) => write!(f, " (sharpness {sharpness:.2}, best in group {b:.2})"),
        None => write!(f, " (sharpness {sharpness:.2})"),
    }
}

fn exposure_detail(
    f: &mut fmt::Formatter<'_>,
    word: &str,
    mean: f32,
    stops: Option<f32>,
) -> fmt::Result {
    match stops {
        Some(s) => write!(
            f,
            "{word} by about {:.1} stops against the rest of the group",
            s.abs()
        ),
        None => write!(f, "{word} (average brightness {:.0}%)", mean * 100.0),
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pct = |v: f32| (v * 100.0).round();
        match self {
            Self::OutOfFocus { sharpness, best } => {
                f.write_str("Out of focus")?;
                sharpness_detail(f, *sharpness, *best)
            }
            Self::MotionBlur {
                confirmed,
                sharpness,
                best,
            } => {
                f.write_str(if *confirmed {
                    "Motion blur"
                } else {
                    "Possible motion blur"
                })?;
                sharpness_detail(f, *sharpness, *best)
            }
            Self::EyesClosed { confirmed: true } => f.write_str("Eyes closed"),
            Self::EyesClosed { confirmed: false } => f.write_str("Eyes may be closed"),
            Self::Duplicate {
                of,
                advantage,
                score,
                best_score,
            } => {
                match of {
                    Some(name) => write!(f, "Duplicate of {name}")?,
                    None => f.write_str("Duplicate of a better frame")?,
                }
                f.write_str(match advantage {
                    Advantage::Sharper => " — that frame is sharper",
                    Advantage::BetterExposed => " — that frame is better exposed",
                    Advantage::LessNoisy => " — that frame is less noisy",
                    Advantage::HigherScore => " — that frame scores higher",
                })?;
                write!(f, " (score {score:.2} vs {best_score:.2})")
            }
            Self::SofterThanBest { relative } => write!(
                f,
                "Softer than the best frame ({}% of its sharpness)",
                pct(*relative)
            ),
            Self::Underexposed { mean, stops } => exposure_detail(f, "Underexposed", *mean, *stops),
            Self::Overexposed { mean, stops } => exposure_detail(f, "Overexposed", *mean, *stops),
            Self::HighlightsClipped { fraction } => {
                write!(f, "Highlights clipped ({}% of the frame)", pct(*fraction))
            }
            Self::ShadowsClipped { fraction } => {
                write!(f, "Shadows clipped ({}% of the frame)", pct(*fraction))
            }
            Self::Noisy { sigma } => write!(f, "Noisy (noise σ {sigma:.1})"),
            Self::PoorFaceCapture => f.write_str("Poor face capture"),
            Self::UtilityImage => f.write_str("Utility shot (document, screenshot or similar)"),
            Self::LowAesthetics => f.write_str("Low aesthetics score"),
            Self::LowScore { score } => write!(f, "Low overall score ({score:.2})"),
        }
    }
}

/// What `best` does better than `this`: the component with the largest lead.
fn advantage(this: &CullScore, best: &CullScore) -> Advantage {
    let leads = [
        (best.sharpness - this.sharpness, Advantage::Sharper),
        (best.exposure - this.exposure, Advantage::BetterExposed),
        (
            best.noise.zip(this.noise).map_or(0.0, |(b, t)| b - t),
            Advantage::LessNoisy,
        ),
    ];
    leads
        .into_iter()
        .filter(|(d, _)| d.is_finite() && *d > 0.0)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(Advantage::HigherScore, |(_, a)| a)
}

/// Why `scores[i]` would be rejected, most important first; never empty.
///
/// `best` is the group's best frame (from [`crate::suggest_picks`]) and
/// `best_name` its name; a lone frame passes `best: Some(i)` or `None`.
/// Informational reasons ("sharpest in group", "focus uncertain") are left
/// out; when nothing else applies the low overall score is the reason.
///
/// # Panics
/// When `i` (or `best`) is out of range of `scores`.
pub fn explain_reject(
    scores: &[CullScore],
    i: usize,
    best: Option<usize>,
    best_name: Option<&str>,
    t: &CullThresholds,
) -> Vec<RejectReason> {
    let s = &scores[i];
    let other = best.filter(|&b| b != i).map(|b| &scores[b]);
    let best_sharpness = other.map(|b| b.sharpness);
    let mut out: Vec<RejectReason> = s
        .reasons
        .iter()
        .filter_map(|r| {
            Some(match *r {
                Reason::OutOfFocus => RejectReason::OutOfFocus {
                    sharpness: s.sharpness,
                    best: best_sharpness,
                },
                Reason::MotionBlur { confirmed, .. } => RejectReason::MotionBlur {
                    confirmed,
                    sharpness: s.sharpness,
                    best: best_sharpness,
                },
                Reason::EyesClosed { confirmed } => RejectReason::EyesClosed { confirmed },
                Reason::SofterThanGroup { relative } => RejectReason::SofterThanBest { relative },
                Reason::Underexposed { mean, stops } => RejectReason::Underexposed { mean, stops },
                Reason::Overexposed { mean, stops } => RejectReason::Overexposed { mean, stops },
                Reason::HighlightsClipped { fraction } => {
                    RejectReason::HighlightsClipped { fraction }
                }
                Reason::ShadowsClipped { fraction } => RejectReason::ShadowsClipped { fraction },
                Reason::Noisy { sigma } => RejectReason::Noisy { sigma },
                Reason::PoorFaceCapture => RejectReason::PoorFaceCapture,
                Reason::UtilityImage => RejectReason::UtilityImage,
                Reason::LowAesthetics => RejectReason::LowAesthetics,
                Reason::FocusUncertain | Reason::SharpestInGroup => return None,
            })
        })
        .collect();
    if let Some(b) = other
        && s.score < b.score - t.reject_margin
    {
        out.push(RejectReason::Duplicate {
            of: best_name.map(str::to_owned),
            advantage: advantage(s, b),
            score: s.score,
            best_score: b.score,
        });
    }
    if out.is_empty() {
        out.push(RejectReason::LowScore { score: s.score });
    }
    out.sort_by_key(RejectReason::rank);
    out
}

/// The reasons as one line for the app, e.g. "Out of focus (sharpness 0.12,
/// best in group 0.71); Eyes closed".
pub fn reject_note(reasons: &[RejectReason]) -> String {
    reasons
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(score: f32, sharpness: f32, reasons: Vec<Reason>) -> CullScore {
        CullScore {
            score,
            sharpness,
            exposure: 1.0,
            noise: None,
            reasons,
        }
    }

    fn t() -> CullThresholds {
        CullThresholds::default()
    }

    fn texts(r: &[RejectReason]) -> Vec<String> {
        r.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn out_of_focus_cites_both_sharpness_values() {
        let s = [
            sc(0.9, 0.71, vec![Reason::SharpestInGroup]),
            sc(0.2, 0.12, vec![Reason::OutOfFocus]),
        ];
        let r = explain_reject(&s, 1, Some(0), Some("IMG_0123.ARW"), &t());
        assert_eq!(
            texts(&r),
            [
                "Out of focus (sharpness 0.12, best in group 0.71)",
                "Duplicate of IMG_0123.ARW — that frame is sharper (score 0.20 vs 0.90)",
            ]
        );
        assert_eq!(
            r.iter().map(RejectReason::code).collect::<Vec<_>>(),
            ["out_of_focus", "duplicate"]
        );
        assert!(r[0].is_hard() && !r[1].is_hard());
        // A lone frame has nothing to compare with.
        let lone = explain_reject(&s[1..], 0, Some(0), None, &t());
        assert_eq!(texts(&lone), ["Out of focus (sharpness 0.12)"]);
    }

    #[test]
    fn motion_blur_and_eyes() {
        let s = [
            sc(0.9, 0.8, vec![]),
            sc(
                0.75,
                0.3,
                vec![
                    Reason::EyesClosed { confirmed: true },
                    Reason::MotionBlur {
                        angle_deg: 10.0,
                        confirmed: true,
                    },
                ],
            ),
            sc(
                0.85,
                0.7,
                vec![
                    Reason::MotionBlur {
                        angle_deg: 0.0,
                        confirmed: false,
                    },
                    Reason::EyesClosed { confirmed: false },
                ],
            ),
        ];
        let r = explain_reject(&s, 1, Some(0), None, &t());
        assert_eq!(
            texts(&r),
            [
                "Motion blur (sharpness 0.30, best in group 0.80)",
                "Eyes closed"
            ]
        );
        assert!(r.iter().all(RejectReason::is_hard));
        let r = explain_reject(&s, 2, Some(0), None, &t());
        assert_eq!(
            texts(&r),
            [
                "Eyes may be closed",
                "Possible motion blur (sharpness 0.70, best in group 0.80)"
            ]
        );
        assert!(!r.iter().any(RejectReason::is_hard));
    }

    #[test]
    fn duplicates_name_what_the_best_frame_does_better() {
        let best = CullScore {
            score: 0.9,
            sharpness: 0.9,
            exposure: 1.0,
            noise: Some(1.0),
            reasons: vec![],
        };
        let weaker = |sharpness, exposure, noise| CullScore {
            score: 0.5,
            sharpness,
            exposure,
            noise,
            reasons: vec![],
        };
        let note = |w: CullScore, name| {
            reject_note(&explain_reject(&[best.clone(), w], 1, Some(0), name, &t()))
        };
        assert_eq!(
            note(weaker(0.9, 0.6, Some(1.0)), Some("A")),
            "Duplicate of A — that frame is better exposed (score 0.50 vs 0.90)"
        );
        assert_eq!(
            note(weaker(0.9, 1.0, Some(0.2)), Some("A")),
            "Duplicate of A — that frame is less noisy (score 0.50 vs 0.90)"
        );
        assert_eq!(
            note(weaker(0.95, 1.0, None), None),
            "Duplicate of a better frame — that frame scores higher (score 0.50 vs 0.90)"
        );
        // Within the margin: not a duplicate, only the low score is left.
        let close = sc(0.8, 0.9, vec![]);
        let r = explain_reject(&[best, close], 1, Some(0), Some("A"), &t());
        assert_eq!(texts(&r), ["Low overall score (0.80)"]);
        assert_eq!(r[0].code(), "low_score");
    }

    #[test]
    fn exposure_and_the_rest_in_order() {
        let s = [sc(
            0.25,
            0.9,
            vec![
                Reason::FocusUncertain,
                Reason::LowAesthetics,
                Reason::UtilityImage,
                Reason::PoorFaceCapture,
                Reason::Noisy { sigma: 7.25 },
                Reason::ShadowsClipped { fraction: 0.3 },
                Reason::HighlightsClipped { fraction: 0.12 },
                Reason::Underexposed {
                    mean: 0.04,
                    stops: None,
                },
                Reason::SofterThanGroup { relative: 0.45 },
            ],
        )];
        let r = explain_reject(&s, 0, None, None, &t());
        assert_eq!(
            texts(&r),
            [
                "Softer than the best frame (45% of its sharpness)",
                "Underexposed (average brightness 4%)",
                "Highlights clipped (12% of the frame)",
                "Shadows clipped (30% of the frame)",
                "Noisy (noise σ 7.2)",
                "Poor face capture",
                "Utility shot (document, screenshot or similar)",
                "Low aesthetics score",
            ]
        );
        let codes: Vec<_> = r.iter().map(RejectReason::code).collect();
        assert_eq!(
            codes,
            [
                "softer_than_best",
                "underexposed",
                "highlights_clipped",
                "shadows_clipped",
                "noisy",
                "poor_face_capture",
                "utility_image",
                "low_aesthetics"
            ]
        );
        let group = |r: Reason| {
            texts(&explain_reject(
                &[sc(0.3, 0.9, vec![r])],
                0,
                None,
                None,
                &t(),
            ))
        };
        assert_eq!(
            group(Reason::Underexposed {
                mean: 0.1,
                stops: Some(-2.04)
            }),
            ["Underexposed by about 2.0 stops against the rest of the group"]
        );
        assert_eq!(
            group(Reason::Overexposed {
                mean: 0.9,
                stops: Some(1.3)
            }),
            ["Overexposed by about 1.3 stops against the rest of the group"]
        );
        assert_eq!(
            group(Reason::Overexposed {
                mean: 0.95,
                stops: None
            }),
            ["Overexposed (average brightness 95%)"]
        );
        assert_eq!(
            reject_note(&explain_reject(
                &[sc(0.1, 0.0, vec![Reason::SharpestInGroup])],
                0,
                None,
                None,
                &t()
            )),
            "Low overall score (0.10)"
        );
    }

    #[test]
    fn codes_match_the_serde_tag() {
        let all = [
            RejectReason::OutOfFocus {
                sharpness: 0.1,
                best: None,
            },
            RejectReason::MotionBlur {
                confirmed: true,
                sharpness: 0.1,
                best: None,
            },
            RejectReason::EyesClosed { confirmed: true },
            RejectReason::Duplicate {
                of: None,
                advantage: Advantage::Sharper,
                score: 0.1,
                best_score: 0.9,
            },
            RejectReason::SofterThanBest { relative: 0.4 },
            RejectReason::Underexposed {
                mean: 0.1,
                stops: None,
            },
            RejectReason::Overexposed {
                mean: 0.9,
                stops: None,
            },
            RejectReason::HighlightsClipped { fraction: 0.1 },
            RejectReason::ShadowsClipped { fraction: 0.1 },
            RejectReason::Noisy { sigma: 8.0 },
            RejectReason::PoorFaceCapture,
            RejectReason::UtilityImage,
            RejectReason::LowAesthetics,
            RejectReason::LowScore { score: 0.1 },
        ];
        for r in all {
            let v = serde_json::to_value(&r).unwrap();
            assert_eq!(v["kind"], r.code(), "{r:?}");
            assert_eq!(serde_json::from_value::<RejectReason>(v).unwrap(), r);
        }
    }
}
