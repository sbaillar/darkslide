//! What the last import wrote, per C1 variant: the reference for the
//! three-way merge that keeps re-imports idempotent and never overwrites a
//! Darkslide edit silently. Stored in the Darkslide session's store
//! (`.ds/settings/capture-one-import.json`, atomic writes). Losing it is safe:
//! without a record, any Darkslide value that differs from both the default
//! and C1's is treated as the user's edit (a conflict), never overwritten.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ds_core::adjust::{
    Crop, Curve, Detail, Exposure, Hdr, Levels, NoiseReduction, Vignette, WhiteBalance,
};
use ds_core::{Adjustments, ColorTag, PickState, Rating};
use serde::{Deserialize, Serialize};

/// File name under the session store's `.ds/settings/`.
pub const STATE_FILE: &str = "capture-one-import.json";
const VERSION: u32 = 1;

/// The adjustment tools the import maps (compared and written together).
/// Fields added after the first release default to Darkslide's defaults when
/// an older record lacks them, which the merge reads as "Darkslide untouched".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tools {
    pub white_balance: WhiteBalance,
    pub exposure: Exposure,
    pub hdr: Hdr,
    pub crop: Crop,
    pub dehaze: f32,
    pub levels: Levels,
    pub curve: Curve,
    pub detail: Detail,
    /// Luminance, colour and detail only: AI Denoise is Darkslide's own and
    /// stays at its default here (the import never compares or writes it).
    pub noise: NoiseReduction,
    pub vignette: Vignette,
    pub chromatic_aberration: bool,
}

impl Default for Tools {
    fn default() -> Self {
        Self::of(&Adjustments::default())
    }
}

impl Tools {
    pub fn of(a: &Adjustments) -> Self {
        Self {
            white_balance: a.white_balance,
            exposure: a.exposure,
            hdr: a.hdr,
            crop: a.crop,
            dehaze: a.presence.dehaze,
            levels: a.levels,
            curve: a.curve.clone(),
            detail: a.detail,
            noise: NoiseReduction {
                luminance: a.noise.luminance,
                color: a.noise.color,
                detail: a.noise.detail,
                ..NoiseReduction::default()
            },
            vignette: a.vignette,
            chromatic_aberration: a.lens.chromatic_aberration,
        }
    }

    /// `base` with these tools; every other setting keeps `base`'s.
    pub fn applied_to(&self, base: &Adjustments) -> Adjustments {
        let mut a = base.clone();
        a.white_balance = self.white_balance;
        a.exposure = self.exposure;
        a.hdr = self.hdr;
        a.crop = self.crop;
        a.presence.dehaze = self.dehaze;
        a.levels = self.levels;
        a.curve = self.curve.clone();
        a.detail = self.detail;
        a.noise.luminance = self.noise.luminance;
        a.noise.color = self.noise.color;
        a.noise.detail = self.noise.detail;
        a.vignette = self.vignette;
        a.lens.chromatic_aberration = self.chromatic_aberration;
        a.sanitized()
    }
}

/// Values the import last wrote (or found already equal) for one variant.
/// `None`: never imported (C1 had nothing to say, or a conflict was kept).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Record {
    pub variant: i64,
    pub rel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<Rating>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_tag: Option<ColorTag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pick: Option<PickState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Tools>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct State {
    pub version: u32,
    /// The C1 session database's UUID at the last import.
    pub c1_session: Option<String>,
    /// C1 variant UUID → record.
    pub variants: BTreeMap<String, Record>,
    /// `pk:<C1 collection id>` → Darkslide album id.
    pub albums: BTreeMap<String, i64>,
}

pub(crate) fn path(store: &Path) -> PathBuf {
    store.join(".ds/settings").join(STATE_FILE)
}

/// The saved state; a missing file is empty. An unreadable one is reported
/// (`Err(text)`) and treated as empty by the caller, which is safe (see the module docs).
pub(crate) fn load(store: &Path) -> Result<State, String> {
    let p = path(store);
    match std::fs::read(&p) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

pub(crate) fn save(store: &Path, state: &mut State) -> std::io::Result<()> {
    state.version = VERSION;
    let p = path(store);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    ds_session::write_file_atomic(&p, &json)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    #[test]
    fn round_trips_and_tolerates_absence() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), State::default());
        let mut s = State::default();
        s.variants.insert(
            "V".into(),
            Record {
                variant: 7,
                rel: "Capture/a.ARW".into(),
                rating: Rating::new(3),
                ..Record::default()
            },
        );
        s.albums.insert("pk:3".into(), 42);
        save(dir.path(), &mut s).unwrap();
        let back = load(dir.path()).unwrap();
        assert_eq!(back.version, VERSION);
        assert_eq!(back, s);
        let text = std::fs::read_to_string(path(dir.path())).unwrap();
        assert!(
            !text.contains("color_tag"),
            "unset fields are omitted: {text}"
        );
    }

    #[test]
    fn unreadable_state_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(path(dir.path()), b"{not json").unwrap();
        assert!(load(dir.path()).unwrap_err().contains(STATE_FILE));
        // A directory where the file should be.
        let d2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(path(d2.path())).unwrap();
        assert!(load(d2.path()).is_err());
        assert!(save(d2.path(), &mut State::default()).is_err());
    }

    #[test]
    fn tools_are_the_mapped_groups() {
        let mut a = Adjustments::default();
        a.exposure.exposure_ev = 1.0;
        a.presence.vibrance = 50.0;
        a.detail.clarity = 20.0;
        let t = Tools::of(&a);
        assert_eq!(t.exposure.exposure_ev, 1.0);
        assert_eq!(t.detail.clarity, 20.0);
        assert_eq!(Tools::of(&Adjustments::default()), Tools::default());
        assert!(
            Tools::default().chromatic_aberration,
            "Darkslide's default, not bool's"
        );
        // Applying keeps what the import doesn't map.
        let mut base = Adjustments::default();
        base.presence.vibrance = 30.0;
        let back = t.applied_to(&base);
        assert_eq!(back.presence.vibrance, 30.0);
        assert_eq!(Tools::of(&back), t);
        // AI Denoise is Darkslide's own: neither compared nor written.
        let mut ai = Adjustments::default();
        ai.noise.ai_denoise = true;
        ai.noise.ai_strength = 80.0;
        assert_eq!(Tools::of(&ai), Tools::default());
        let kept = t.applied_to(&ai);
        assert!(kept.noise.ai_denoise);
        assert_eq!(kept.noise.ai_strength, 80.0);
        // A record from before the extra tools loads with Darkslide's defaults for them.
        let old: Tools = serde_json::from_str(r#"{"exposure":{"exposure_ev":1.0}}"#).unwrap();
        assert_eq!(old.exposure.exposure_ev, 1.0);
        assert_eq!(old.detail, Detail::default());
        assert!(old.chromatic_aberration);
    }
}
