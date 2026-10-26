//! Plain-English explanations, built from the reasons' measurements by templates
//! (deterministic and always available; no language model writes or rephrases them).

use super::{Change, Evidence, Field, Kept, KeptWhy, Reason, SceneTag, Setting};

/// A signed number with a real minus sign: "+0.45", "−35", "0".
pub(crate) fn signed(v: f32, decimals: usize) -> String {
    let s = format!("{:.*}", decimals, v.abs());
    if s.trim_start_matches(['0', '.']).is_empty() {
        s
    } else if v < 0.0 {
        format!("\u{2212}{s}")
    } else {
        format!("+{s}")
    }
}

/// A fraction as a percentage: "2.4%" below 10%, "18%" above.
pub(crate) fn pct(frac: f32) -> String {
    percent(frac * 100.0)
}

/// A percentage value: "2.4%" below 10, "18%" above.
pub(crate) fn percent(p: f32) -> String {
    if p.abs() < 9.95 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

/// "5,600 K".
pub(crate) fn kelvin(k: f32) -> String {
    let k = (k / 10.0).round() as i64 * 10;
    let digits = k.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    format!("{out} K")
}

/// A field's value as the panel shows it.
pub(crate) fn value(field: Field, s: &Setting) -> String {
    match *s {
        Setting::Number { value } => match field {
            Field::Exposure => format!("{} EV", signed(value, 2)),
            _ => signed(value, 0),
        },
        Setting::WhiteBalance {
            temp_k,
            tint,
            as_shot,
        } => {
            let v = format!("{} / {}", kelvin(temp_k), signed(tint, 0));
            if as_shot { format!("As shot ({v})") } else { v }
        }
        Setting::Levels { black, white } => {
            format!("black {}, white {}", pct(black), pct(white))
        }
        Setting::Toggle { on } => if on { "On" } else { "Off" }.to_owned(),
        Setting::NoiseReduction { luminance, color } => {
            format!("luminance {luminance:.0}, color {color:.0}")
        }
        Setting::Straighten { angle_deg, .. } => format!("{}°", signed(angle_deg, 1)),
        Setting::SkyLayer {
            present,
            exposure_ev,
            highlights,
        } => {
            if present {
                format!(
                    "{} EV, highlights {}",
                    signed(exposure_ev, 1),
                    signed(highlights, 0)
                )
            } else {
                "None".to_owned()
            }
        }
    }
}

/// "a little", "about half a stop", "about a stop", "about 2 stops".
fn stops(s: f32) -> String {
    let s = s.abs();
    if s < 0.4 {
        "a little".to_owned()
    } else if s < 0.75 {
        "about half a stop".to_owned()
    } else if s < 1.25 {
        "about a stop".to_owned()
    } else {
        format!("about {s:.0} stops")
    }
}

fn scene_word(tag: SceneTag) -> &'static str {
    match tag {
        SceneTag::Landscape => "landscape",
        SceneTag::Portrait => "portrait",
        SceneTag::Night => "night scene",
        SceneTag::Snow => "snow scene",
        SceneTag::Beach => "beach scene",
        SceneTag::Sunset => "sunset",
        SceneTag::Backlit => "backlit scene",
    }
}

/// The cast a white balance change removes, from the temperature and tint deltas.
fn cast(dk: f32, dt: f32) -> &'static str {
    if dk.abs() >= 150.0 || dt.abs() < 3.0 {
        // Lowering the setting cools the image: the light was warmer than set.
        if dk < 0.0 {
            "a warm (orange)"
        } else {
            "a cool (blue)"
        }
    } else if dt > 0.0 {
        "a green"
    } else {
        "a magenta"
    }
}

/// The explanation of one change.
pub(crate) fn change(
    field: Field,
    old: &Setting,
    new: &Setting,
    reason: &Reason,
    default_on: bool,
) -> String {
    let to = value(field, new);
    let head = format!("{} {to}", field.label());
    if let Some(less) = easing(field, old, new) {
        let m = measured(reason);
        let body = if less == Easing::Off {
            format!(
                "the earlier {} isn't needed any more ({m})",
                field.label().to_lowercase()
            )
        } else {
            format!("less is needed now ({m})")
        };
        return format!("{head} \u{2014} {body}.");
    }
    let body = match reason {
        Reason::Exposure {
            midtone,
            target,
            limited,
            scene,
            clipped_pct,
        } => {
            if let Some(c) = clipped_pct {
                format!(
                    "you set {}, but {} of the image clips to white and Highlights is set by hand too; this pulls exposure back just enough to keep the brightest areas",
                    value(field, old),
                    percent(*c)
                )
            } else {
                let delta = number(new) - number(old);
                let dark = delta > 0.0;
                // The size the change has: a near-black frame isn't "17 stops dark"
                // when exposure can only move 3.
                let mut s = format!(
                    "the image was {} {} (midtones at {} vs the {} target{})",
                    stops(delta),
                    if dark { "dark" } else { "bright" },
                    pct(*midtone),
                    pct(*target),
                    scene.map_or(String::new(), |t| format!(" for a {}", scene_word(t)))
                );
                if *limited {
                    s.push_str("; held back so the brightest highlights don't clip");
                }
                s
            }
        }
        Reason::HighlightsClipped {
            clipped_pct,
            sky_clipped_pct,
        } => match sky_clipped_pct {
            Some(sky) => format!(
                "{} of the sky clips to white at this exposure; this recovers cloud detail",
                percent(*sky)
            ),
            None => format!(
                "{} of the image clips to white at this exposure; this brings back detail in the brightest areas",
                percent(*clipped_pct)
            ),
        },
        Reason::HighlightRange { stops_over } => format!(
            "the brightest areas sit {:.1} stops into the tone curve's roll-off; this compresses them so they keep detail",
            stops_over.max(0.0)
        ),
        Reason::Shadows {
            dark_pct,
            backlit,
            subject_luma,
            median_luma,
            ..
        } => match (backlit, subject_luma) {
            (true, Some(s)) => format!(
                "the subject is backlit (subject at {} brightness vs {} for the frame); this lifts it without touching the sky",
                pct(*s),
                pct(*median_luma)
            ),
            (true, None) => "the scene is backlit; this lifts the darker foreground".to_owned(),
            _ => format!(
                "{} of the image is close to black; this opens up shadow detail",
                percent(*dark_pct)
            ),
        },
        Reason::ColorCast {
            neutral_k,
            neutral_tint,
            current_k,
            current_tint,
            evidence,
        } => {
            let removes = cast(neutral_k - current_k, neutral_tint - current_tint);
            let found = format!("{} / {}", kelvin(*neutral_k), signed(*neutral_tint, 0));
            let set = format!("{} / {}", kelvin(*current_k), signed(*current_tint, 0));
            match evidence {
                Evidence::NeutralCluster => format!(
                    "several neutral surfaces in the image agree on {found}, against the {set} setting; this removes {removes} cast"
                ),
                Evidence::WhitePatch => format!(
                    "a neutral grey or white area in the image reads {found}, against the {set} setting; this removes {removes} cast"
                ),
                _ => format!(
                    "the image's overall color balance leans away from neutral ({found} vs {set}); this gently removes {removes} cast (capped at 600 K)"
                ),
            }
        }
        Reason::TonalRange { darkest, brightest } => format!(
            "the darkest tones stopped at {} and the brightest at {}; this stretches them to the full range",
            pct(*darkest),
            pct(*brightest)
        ),
        Reason::LowContrast { spread, target } => format!(
            "the tones were flat (tonal spread {} vs about {} in a typical photo)",
            pct(*spread),
            pct(*target)
        ),
        Reason::Muted {
            mean_saturation,
            target,
            faces,
        } => {
            let mut s = format!(
                "colors were muted (average saturation {} vs about {}); vibrance boosts the dullest colors most",
                pct(*mean_saturation),
                pct(*target)
            );
            if *faces > 0 {
                s.push_str(", and it's kept moderate so skin stays natural");
            }
            s
        }
        Reason::Oversaturated { p90_saturation, .. } => format!(
            "the most colorful tenth of the image is at {} saturation or more; this tames it",
            pct(*p90_saturation)
        ),
        Reason::SoftDetail { detail, target, .. } => format!(
            "fine detail was soft (local contrast {} vs about {} in a crisp photo); this adds definition to edges and textures",
            pct(*detail),
            pct(*target)
        ),
        Reason::Haze { haze, .. } => format!(
            "the image shows haze (haze score {haze:.2}, where 0 is clear air); this restores depth and color"
        ),
        Reason::Noise {
            iso,
            sigma,
            ai_available,
        } => {
            let iso = iso.map_or("High noise".to_owned(), |i| format!("ISO {i}"));
            let what = format!("{iso} with measured sensor noise {}", pct(*sigma));
            if field == Field::AiDenoise {
                format!("{what}; AI Denoise removes noise while keeping detail")
            } else if *ai_available {
                format!("{what}; this cleans it up")
            } else {
                format!("{what}; this cleans it up (AI Denoise isn't available in this build)")
            }
        }
        Reason::Tilted { horizon_deg } => format!(
            "the horizon was tilted {:.1}° ({}); {}",
            horizon_deg.abs(),
            if *horizon_deg > 0.0 {
                "rising to the right"
            } else {
                "falling to the right"
            },
            if matches!(new, Setting::Straighten { crop: Some(_), .. }) {
                "your crop shrinks about its centre so no empty corners show"
            } else {
                "the frame is fitted inside the turned image, so it shrinks slightly"
            }
        ),
        Reason::LensOff { profile } => format!(
            "lens corrections are off; the {profile} profile corrects distortion, color fringes and corner shading"
        ),
        Reason::BrightSky {
            sky_pct, sky_luma, ..
        } => format!(
            "the sky covers {} of the frame and is very bright (average {}); a layer masked to the sky darkens it without touching the land",
            percent(*sky_pct),
            pct(*sky_luma)
        ),
    };
    let mut s = format!("{head} \u{2014} {body}.");
    if !default_on {
        s.push_str(if matches!(reason, Reason::LensOff { .. }) {
            " You turned them off, so this is unchecked."
        } else {
            " A suggestion, so it starts unchecked."
        });
    }
    s
}

/// The explanation of a field left alone.
pub(crate) fn kept(field: Field, value: &str, why: KeptWhy, reason: Option<&Reason>) -> String {
    let label = field.label();
    match why {
        KeptWhy::UserSet { clipped_pct: None } => format!("{label} stays at {value}: you set it."),
        KeptWhy::UserSet {
            clipped_pct: Some(c),
        } => format!(
            "{label} stays at {value}: you set it, though {} of the image clips to white.",
            percent(c)
        ),
        KeptWhy::Withdrawn {
            clipped_pct,
            crushed_pct,
            baseline_clipped_pct,
            baseline_crushed_pct,
        } => {
            let (what, now, before) =
                if clipped_pct - baseline_clipped_pct >= crushed_pct - baseline_crushed_pct {
                    ("clip to white", clipped_pct, baseline_clipped_pct)
                } else {
                    ("crush to black", crushed_pct, baseline_crushed_pct)
                };
            format!(
                "{label} stays at {value}: together with the other changes, {} of the image would {what} ({} without them), so it was left out.",
                percent(now),
                percent(before)
            )
        }
        KeptWhy::SceneMood { tag } => format!(
            "{label} stays at {value} to keep the {}'s own color.",
            scene_word(tag)
        ),
        KeptWhy::OnTarget => match reason {
            Some(Reason::Exposure {
                midtone, target, ..
            }) => format!(
                "{label} is already right (midtones at {} vs the {} target).",
                pct(*midtone),
                pct(*target)
            ),
            _ => format!("{label} already looks right ({value})."),
        },
    }
}

/// How a one-way slider (more recovery, more punch) moves back toward its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Easing {
    Less,
    Off,
}

/// `Some` when `new` is closer to the default than `old` on a slider whose reason only
/// ever asks for more of it: the explanation then says less is needed, not why more is.
fn easing(field: Field, old: &Setting, new: &Setting) -> Option<Easing> {
    let one_way = matches!(
        field,
        Field::Highlights
            | Field::Shadows
            | Field::Contrast
            | Field::Vibrance
            | Field::Saturation
            | Field::Clarity
            | Field::Texture
            | Field::Dehaze
    );
    let (Setting::Number { value: o }, Setting::Number { value: n }) = (*old, *new) else {
        return None;
    };
    // A sign change is a new direction: its own reason explains it.
    if !one_way || n.abs() >= o.abs() || (n != 0.0 && n.signum() != o.signum()) {
        None
    } else if n == 0.0 {
        Some(Easing::Off)
    } else {
        Some(Easing::Less)
    }
}

/// The measurement behind a reason, stated neutrally (for easing a change back).
fn measured(reason: &Reason) -> String {
    match reason {
        Reason::HighlightsClipped { clipped_pct, .. } => {
            format!(
                "{} of the image clips at this exposure",
                percent(*clipped_pct)
            )
        }
        Reason::HighlightRange { stops_over } if *stops_over > 0.0 => {
            format!("the brightest areas sit {stops_over:.1} stops into the tone curve's roll-off")
        }
        Reason::HighlightRange { .. } => {
            "the brightest areas sit below the tone curve's roll-off".to_owned()
        }
        Reason::Shadows { dark_pct, .. } => {
            format!("{} of the image is close to black", percent(*dark_pct))
        }
        Reason::LowContrast { spread, target } => format!(
            "tonal spread {} vs about {} in a typical photo",
            pct(*spread),
            pct(*target)
        ),
        Reason::Muted {
            mean_saturation,
            target,
            ..
        } => format!(
            "average saturation {} vs about {}",
            pct(*mean_saturation),
            pct(*target)
        ),
        Reason::Oversaturated { p90_saturation, .. } => format!(
            "the most colorful tenth of the image is at {} saturation",
            pct(*p90_saturation)
        ),
        Reason::SoftDetail { detail, target, .. } => format!(
            "local contrast {} vs about {} in a crisp photo",
            pct(*detail),
            pct(*target)
        ),
        Reason::Haze { haze, .. } => format!("haze score {haze:.2}, where 0 is clear air"),
        _ => "the photo measures differently now".to_owned(),
    }
}

/// A short phrase for the summary: what a change does.
fn phrase(c: &Change) -> String {
    match easing(c.field, &c.old, &c.new) {
        Some(Easing::Off) => return format!("removes the earlier {}", c.label.to_lowercase()),
        Some(Easing::Less) => return format!("eases {}", c.label.to_lowercase()),
        None => {}
    }
    match (&c.reason, c.new) {
        (Reason::Exposure { .. }, Setting::Number { value }) => {
            let d = value - number(&c.old);
            format!(
                "{} the image by {:.2} EV",
                if d > 0.0 { "brightens" } else { "darkens" },
                d.abs()
            )
        }
        (
            Reason::HighlightsClipped {
                sky_clipped_pct: Some(_),
                ..
            },
            _,
        ) => "recovers the clipped sky".to_owned(),
        (Reason::HighlightsClipped { .. }, _) => "recovers clipped highlights".to_owned(),
        (Reason::HighlightRange { .. }, _) => "softens the brightest highlights".to_owned(),
        (Reason::Shadows { backlit: true, .. }, _) => "lifts the backlit subject".to_owned(),
        (Reason::Shadows { .. }, _) => "opens up the shadows".to_owned(),
        (
            Reason::ColorCast {
                neutral_k,
                neutral_tint,
                current_k,
                current_tint,
                ..
            },
            _,
        ) => format!(
            "removes {} cast",
            cast(neutral_k - current_k, neutral_tint - current_tint)
        ),
        (Reason::TonalRange { .. }, _) => "stretches the tonal range".to_owned(),
        (Reason::LowContrast { .. }, _) => "adds contrast".to_owned(),
        (Reason::Muted { .. }, _) => "boosts muted colors".to_owned(),
        (Reason::Oversaturated { .. }, _) => "tames oversaturated colors".to_owned(),
        (
            Reason::SoftDetail {
                landscape: true, ..
            },
            _,
        ) => "brings out fine texture".to_owned(),
        (Reason::SoftDetail { .. }, _) => "adds clarity".to_owned(),
        (Reason::Haze { .. }, _) => "cuts through haze".to_owned(),
        (Reason::Noise { .. }, _) if c.field == Field::AiDenoise => {
            "turns on AI Denoise".to_owned()
        }
        (Reason::Noise { .. }, _) => "strengthens noise reduction".to_owned(),
        (Reason::Tilted { horizon_deg }, _) => {
            format!("levels a {:.1}° tilt", horizon_deg.abs())
        }
        (Reason::LensOff { .. }, _) => "suggests turning lens corrections back on".to_owned(),
        (Reason::BrightSky { .. }, _) => "suggests a sky layer".to_owned(),
        (_, _) => format!("changes {}", c.label.to_lowercase()),
    }
}

fn number(s: &Setting) -> f32 {
    match *s {
        Setting::Number { value } => value,
        _ => 0.0,
    }
}

/// "a", "a and b", "a, b and c".
fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The summary paragraph.
pub(crate) fn summary(changes: &[Change], kept: &[Kept]) -> String {
    let mut s = if changes.is_empty() {
        "This photo already looks balanced, so AI Optimize suggests no changes.".to_owned()
    } else {
        let phrases: Vec<String> = changes.iter().map(phrase).collect();
        format!(
            "AI Optimize suggests {} change{}: it {}.",
            changes.len(),
            if changes.len() == 1 { "" } else { "s" },
            list(&phrases)
        )
    };
    let user: Vec<String> = kept
        .iter()
        .filter(|k| matches!(k.why, KeptWhy::UserSet { .. }))
        .map(|k| k.label.to_lowercase())
        .collect();
    if !user.is_empty() {
        let them = if user.len() == 1 { "it" } else { "them" };
        s = format!("{s} It left {} alone because you set {them}.", list(&user));
    }
    s
}
