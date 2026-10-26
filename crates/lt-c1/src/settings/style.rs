//! Styles (`Styles/**/*.costyle`) and presets (`Presets60/<tool>/*.copreset`):
//! `<SL>` with `<E K V/>` entries, the same keys as a `.cos` adjustment layer
//! plus `Name`, `UUID` and (presets) `StyleSource`. The keys go through the
//! session import's mapping ([`crate::map_variant`]) so a style's numbers
//! match what the session import makes of the same values.

use std::path::Path;

use lt_core::adjust::WhiteBalance;
use lt_core::{AdjustGroup, Adjustments};
use lt_session::ImageMetaLite;

use super::{Note, fingerprint, flat_entries};
use crate::map::{KeyClass, WbConverter, classify, map_variant};
use crate::model::{Keys, Settings};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    /// A `.costyle` (all tools).
    Style,
    /// A `.copreset` (one tool's preset).
    Preset,
}

/// One style or preset as a Luxtable preset.
#[derive(Debug, Clone, PartialEq)]
pub struct StyleImport {
    /// Path relative to the settings folder (the import's key).
    pub file: String,
    pub name: String,
    pub kind: StyleKind,
    /// A preset's tool folder ("Exposure"), or a style's sub-folder.
    pub category: Option<String>,
    pub uuid: Option<String>,
    /// Luxtable's defaults with the style's values.
    pub adjustments: Adjustments,
    /// The groups the style sets (what the Luxtable preset applies).
    pub groups: Vec<AdjustGroup>,
    /// C1 keys that map.
    pub mapped: Vec<String>,
    pub notes: Vec<Note>,
    pub fingerprint: String,
}

/// A style has no photo, so C1's white-balance multipliers can't be converted.
struct NoPhoto;

impl WbConverter for NoPhoto {
    fn convert(&self, _: &Path, _: Option<[f64; 3]>, _: [f64; 3]) -> Option<WhiteBalance> {
        None
    }
}

/// The Luxtable group a mapped key belongs to.
fn group(key: &str) -> Option<AdjustGroup> {
    Some(match key {
        "Exposure" | "Contrast" | "Brightness" | "Saturation" => AdjustGroup::Exposure,
        "HighlightRecoveryEx" | "ShadowRecovery" | "WhiteRecovery" | "BlackRecovery" => {
            AdjustGroup::Hdr
        }
        "WhiteBalance" | "WhiteBalanceTemperature" | "WhiteBalanceTint" => {
            AdjustGroup::WhiteBalance
        }
        "Crop" | "Rotation" => AdjustGroup::Crop,
        "DehazeAmount" => AdjustGroup::Presence,
        "Vignetting" => AdjustGroup::Vignette,
        "NrAmount" | "CnrAmount" | "NoiseDetailsAmount" => AdjustGroup::Noise,
        "Shadow" | "Highlight" | "TargetShadow" | "TargetHighlight" => AdjustGroup::Levels,
        "ChromaticAberEnabled" => AdjustGroup::Lens,
        k if k.starts_with("GradationCurve") => AdjustGroup::Curve,
        k if k.starts_with("Clarity") || k.starts_with("Usm") => AdjustGroup::Detail,
        _ => return None,
    })
}

pub(crate) fn parse(
    xml: &str,
    file: &str,
    stem: &str,
    kind: StyleKind,
    category: Option<String>,
) -> Result<StyleImport, String> {
    let mut al: Keys = flat_entries(xml, &["SL"])?.into_iter().collect();
    let name = al
        .remove("Name")
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| stem.to_owned());
    let uuid = al
        .remove("UUID")
        .map(|u| u.trim().trim_matches(['{', '}']).to_owned())
        .filter(|u| !u.is_empty());
    let mut notes = Vec::new();
    let organization: Vec<&str> = al
        .keys()
        .filter(|k| classify(k) == KeyClass::Organization)
        .map(String::as_str)
        .collect();
    if !organization.is_empty() {
        notes.push(Note::dropped(
            &organization.join(", "),
            "Rating, colour tag and keywords in a style aren't carried over",
        ));
    }
    let settings = Settings {
        dl: Keys::new(),
        al,
    };
    let m = map_variant(
        &settings,
        Path::new(""),
        &ImageMetaLite::default(),
        &NoPhoto,
        &[],
    );
    for (key, reason) in &m.unmapped {
        let reason = if key == "WhiteBalance" {
            "White balance: C1's multipliers depend on the camera, and a style has no photo to convert them against"
        } else {
            reason
        };
        notes.push(Note::dropped(key, reason));
    }
    let mut groups: Vec<AdjustGroup> = Vec::new();
    for g in m.mapped.iter().filter_map(|k| group(k)) {
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    groups.sort_by_key(|g| AdjustGroup::ALL.iter().position(|a| a == g));
    Ok(StyleImport {
        file: file.to_owned(),
        name,
        kind,
        category,
        uuid,
        adjustments: m.apply(&Adjustments::default()),
        groups,
        mapped: m.mapped,
        notes,
        fingerprint: fingerprint(xml.as_bytes()),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use std::fmt::Write as _;

    use super::*;

    fn sl(pairs: &[(&str, &str)]) -> String {
        let mut s = String::from("\u{feff}<?xml version=\"1.0\"?>\n<SL Engine=\"1100\">\n");
        for (k, v) in pairs {
            let _ = writeln!(s, "  <E K=\"{k}\" V=\"{}\" />", v.replace('&', "&amp;"));
        }
        s + "</SL>\n"
    }

    #[test]
    fn style_numbers_match_the_session_mapping() {
        let x = sl(&[
            ("Contrast", "9.5"),
            ("Saturation", "-10.25"),
            ("HighlightRecoveryEx", "-30"),
            ("GradationCurve", "0,0;0.5,0.6;1,1"),
            ("ClarityStructure", "12"),
            ("ColorBalanceMidtone", "1;0.98;0.96"),
            ("Name", "Warm & Soft"),
            ("UUID", "{BFCE8278-7814-4021-8307-2549208E7C86}"),
        ]);
        let s = parse(
            x.trim_start_matches('\u{feff}'),
            "Styles/Warm.costyle",
            "Warm",
            StyleKind::Style,
            None,
        )
        .unwrap();
        assert_eq!(s.name, "Warm & Soft");
        assert_eq!(
            s.uuid.as_deref(),
            Some("BFCE8278-7814-4021-8307-2549208E7C86")
        );
        assert_eq!(s.file, "Styles/Warm.costyle");
        let a = &s.adjustments;
        assert_eq!(a.exposure.contrast, 9.5);
        assert_eq!(a.exposure.saturation, -10.25);
        assert_eq!(a.exposure.exposure_ev, 0.0);
        // C1's negative highlight recovers: Luxtable's positive.
        assert_eq!(a.hdr.highlights, 30.0);
        assert_eq!(a.curve.rgb, vec![[0.0, 0.0], [0.5, 0.6], [1.0, 1.0]]);
        assert_eq!(a.detail.structure, 12.0);
        assert_eq!(
            s.groups,
            [
                AdjustGroup::Exposure,
                AdjustGroup::Hdr,
                AdjustGroup::Curve,
                AdjustGroup::Detail
            ]
        );
        assert_eq!(s.notes.len(), 1);
        assert_eq!(s.notes[0].key, "ColorBalanceMidtone");
        assert!(s.notes[0].dropped);
    }

    #[test]
    fn white_balance_and_metadata_are_reported() {
        let x = sl(&[
            ("WhiteBalance", "2.1;1;1.4"),
            ("Basic_Rating", "5"),
            ("Exposure", "1"),
            ("StyleSource", "Exposure"),
        ]);
        let s = parse(
            &x,
            "Presets60/Exposure/p.copreset",
            "p",
            StyleKind::Preset,
            Some("Exposure".into()),
        )
        .unwrap();
        assert_eq!(s.name, "p", "no Name entry: the file name");
        assert_eq!(s.category.as_deref(), Some("Exposure"));
        assert_eq!(s.groups, [AdjustGroup::Exposure]);
        assert_eq!(s.adjustments.exposure.exposure_ev, 1.0);
        let keys: Vec<&str> = s.notes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(keys, ["Basic_Rating", "WhiteBalance"]);
        assert!(s.notes[1].text.contains("no photo"));
        // Temperature and tint need no photo.
        let x = sl(&[
            ("WhiteBalanceTemperature", "5200"),
            ("WhiteBalanceTint", "4"),
        ]);
        let s = parse(&x, "f", "f", StyleKind::Style, None).unwrap();
        assert_eq!(
            s.adjustments.white_balance,
            WhiteBalance::Custom {
                temp_k: 5200.0,
                tint: 4.0
            }
        );
        assert_eq!(s.groups, [AdjustGroup::WhiteBalance]);
    }

    #[test]
    fn invalid() {
        assert!(
            parse("<OR/>", "f", "f", StyleKind::Style, None)
                .unwrap_err()
                .contains("<SL>")
        );
        // Nothing that maps: no groups.
        let s = parse(
            &sl(&[("FilmGrainAmount", "20")]),
            "f",
            "f",
            StyleKind::Style,
            None,
        )
        .unwrap();
        assert_eq!(s.groups.len(), 0);
        assert!(s.adjustments.is_identity());
        assert_eq!(group("Unknown"), None);
        assert_eq!(group("UsmRadius"), Some(AdjustGroup::Detail));
    }
}
