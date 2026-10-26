//! The edit vocabulary of presets, looks and plug-in packs: every `EditCodec` key
//! (the app's stable string keys for an edit's fields), its adjustment group, what kind
//! of value it is and its range. One table, read by the plug-in validator (lt-plugin)
//! and checked against the app's own `EditCodec.scalars`/`flags` by a Swift test, so the
//! two can't drift.

use std::sync::OnceLock;

/// What a key's value means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// An amount a look's Intensity scales.
    Amount,
    /// A choice by index (`fx.blur.mode`): Intensity switches it.
    Choice,
    /// A colour-balance wheel's hue or saturation: Intensity moves the two together.
    WheelPolar,
    /// On/off, stored as 1/0. Styles only: a look can't scale it.
    Flag,
    /// An index into a list (`crop.aspect`). Styles only.
    Index,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditKey {
    pub key: String,
    /// The app's `AdjustGroup` raw value (`whiteBalance`, `exposure`, …).
    pub group: &'static str,
    pub kind: KeyKind,
    pub min: f64,
    pub max: f64,
}

impl EditKey {
    /// Whether a one-touch look may set it (scalars Intensity can scale exactly).
    #[must_use]
    pub fn allowed_in_looks(&self) -> bool {
        !matches!(self.kind, KeyKind::Flag | KeyKind::Index) && self.key != "crop.angle"
    }
}

/// Colour-editor bands (`ce.<i>.*`), the app's `ColorEditorEdit.bandHues.count`.
pub const COLOR_EDITOR_BANDS: usize = 8;

/// Every key, in the app's `EditCodec` order (scalars, then flags, then `crop.aspect`).
#[must_use]
pub fn edit_keys() -> &'static [EditKey] {
    static KEYS: OnceLock<Vec<EditKey>> = OnceLock::new();
    KEYS.get_or_init(build)
}

/// The key named `key`.
#[must_use]
pub fn edit_key(key: &str) -> Option<&'static EditKey> {
    edit_keys().iter().find(|k| k.key == key)
}

#[allow(clippy::too_many_lines)] // the table, one key per line
fn build() -> Vec<EditKey> {
    use KeyKind::{Amount, Choice, Flag, Index, WheelPolar};
    let mut v = Vec::new();
    let mut add = |key: &str, group: &'static str, kind: KeyKind, min: f64, max: f64| {
        v.push(EditKey {
            key: key.to_owned(),
            group,
            kind,
            min,
            max,
        });
    };
    let s = 100.0; // the ±100 sliders
    add("wb.temperature", "whiteBalance", Amount, 2000.0, 50000.0);
    add("wb.tint", "whiteBalance", Amount, -150.0, 150.0);
    add("exposure.ev", "exposure", Amount, -5.0, 5.0);
    for k in [
        "exposure.contrast",
        "exposure.brightness",
        "exposure.saturation",
    ] {
        add(k, "exposure", Amount, -s, s);
    }
    for k in ["presence.vibrance", "presence.texture", "presence.dehaze"] {
        add(k, "presence", Amount, -s, s);
    }
    for k in ["hdr.highlights", "hdr.shadows", "hdr.whites", "hdr.blacks"] {
        add(k, "hdr", Amount, -s, s);
    }
    add("levels.inBlack", "levels", Amount, 0.0, 1.0);
    add("levels.inWhite", "levels", Amount, 0.0, 1.0);
    add("levels.gamma", "levels", Amount, 0.1, 10.0);
    add("levels.outBlack", "levels", Amount, 0.0, 1.0);
    add("levels.outWhite", "levels", Amount, 0.0, 1.0);
    add("detail.clarity", "detail", Amount, -s, s);
    add("detail.structure", "detail", Amount, -s, s);
    add("detail.sharpenAmount", "detail", Amount, 0.0, 1000.0);
    add("detail.sharpenRadius", "detail", Amount, 0.2, 2.5);
    add("detail.sharpenThreshold", "detail", Amount, 0.0, 10.0);
    for k in [
        "noise.luminance",
        "noise.color",
        "noise.detail",
        "noise.aiStrength",
    ] {
        add(k, "noise", Amount, 0.0, s);
    }
    add("vignette.amountEV", "vignette", Amount, -3.0, 3.0);
    add("vignette.midpoint", "vignette", Amount, 0.0, 1.0);
    add("lens.distortion", "lens", Amount, 0.0, 1.0);
    add("lens.lightFalloff", "lens", Amount, 0.0, 1.0);
    add("lens.keystoneVertical", "lens", Amount, -30.0, 30.0);
    add("lens.keystoneHorizontal", "lens", Amount, -30.0, 30.0);
    add("crop.angle", "crop", Amount, -45.0, 45.0);
    add("fx.blur.amount", "effects", Amount, 0.0, s);
    add("fx.blur.mode", "effects", Choice, 0.0, 1.0);
    add("fx.blur.focalRange", "effects", Amount, 0.0, s);
    add("fx.blur.highlightBoost", "effects", Amount, 0.0, s);
    add("fx.blur.shape", "effects", Choice, 0.0, 2.0);
    add("fx.band.x", "effects", Amount, 0.0, 1.0);
    add("fx.band.y", "effects", Amount, 0.0, 1.0);
    add("fx.band.angle", "effects", Amount, -90.0, 90.0);
    add("fx.band.width", "effects", Amount, 0.0, 0.5);
    add("fx.band.feather", "effects", Amount, 0.005, 1.0);
    for k in [
        "fx.glow.amount",
        "fx.glow.size",
        "fx.grain.amount",
        "fx.grain.size",
        "fx.grain.roughness",
        "fx.halation.amount",
    ] {
        add(k, "effects", Amount, 0.0, s);
    }
    for wheel in ["master", "shadows", "midtones", "highlights"] {
        add(
            &format!("cb.{wheel}.hue"),
            "colorBalance",
            WheelPolar,
            0.0,
            360.0,
        );
        add(
            &format!("cb.{wheel}.saturation"),
            "colorBalance",
            WheelPolar,
            0.0,
            1.0,
        );
        add(
            &format!("cb.{wheel}.lightness"),
            "colorBalance",
            Amount,
            -1.0,
            1.0,
        );
    }
    for i in 0..COLOR_EDITOR_BANDS {
        add(
            &format!("ce.{i}.hueShift"),
            "colorEditor",
            Amount,
            -30.0,
            30.0,
        );
        add(&format!("ce.{i}.saturation"), "colorEditor", Amount, -s, s);
        add(&format!("ce.{i}.lightness"), "colorEditor", Amount, -s, s);
    }
    add("wb.asShot", "whiteBalance", Flag, 0.0, 1.0);
    add("lens.enabled", "lens", Flag, 0.0, 1.0);
    add("lens.chromaticAberration", "lens", Flag, 0.0, 1.0);
    add("noise.aiDenoise", "noise", Flag, 0.0, 1.0);
    add("crop.aspect", "crop", Index, 0.0, 5.0);
    v
}

/// Why `value` can't be stored under `key` (`None`: it can).
#[must_use]
pub fn check_value(key: &str, value: f64) -> Option<String> {
    let Some(k) = edit_key(key) else {
        return Some(format!("unknown key `{key}`"));
    };
    if !value.is_finite() || value < k.min || value > k.max {
        return Some(format!("`{key}` = {value} is outside {}–{}", k.min, k.max));
    }
    if matches!(k.kind, KeyKind::Choice | KeyKind::Index | KeyKind::Flag) && value.fract() != 0.0 {
        return Some(format!("`{key}` must be a whole number"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_grouped() {
        let keys = edit_keys();
        let mut names: Vec<&str> = keys.iter().map(|k| k.key.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), keys.len());
        assert_eq!(keys.len(), 50 + 12 + 24 + 5);
        assert!(keys.iter().all(|k| k.min < k.max));
        assert_eq!(
            edit_key("cb.shadows.hue").unwrap().kind,
            KeyKind::WheelPolar
        );
        assert_eq!(edit_key("fx.grain.amount").unwrap().group, "effects");
        assert!(edit_key("nope").is_none());
    }

    #[test]
    fn looks_take_scalars_only() {
        assert!(edit_key("exposure.contrast").unwrap().allowed_in_looks());
        assert!(edit_key("fx.blur.mode").unwrap().allowed_in_looks());
        for k in ["wb.asShot", "crop.aspect", "crop.angle"] {
            assert!(!edit_key(k).unwrap().allowed_in_looks(), "{k}");
        }
    }

    #[test]
    fn values_are_checked() {
        assert_eq!(check_value("exposure.ev", 1.5), None);
        assert!(check_value("exposure.ev", 9.0).unwrap().contains("outside"));
        assert!(check_value("exposure.ev", f64::NAN).is_some());
        assert!(check_value("fx.blur.shape", 1.5).unwrap().contains("whole"));
        assert!(check_value("bogus", 0.0).unwrap().contains("unknown"));
    }
}
