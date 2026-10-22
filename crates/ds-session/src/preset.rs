//! Named partial adjustment stacks ("styles"), stored as JSON files in the
//! app's presets directory.

use std::fs;
use std::path::{Path, PathBuf};

use ds_core::clipboard::Clipboard;
use ds_core::geom::Size;
use ds_core::{Adjustments, GroupSet, PasteOptions};
use serde::{Deserialize, Serialize};

use crate::atomic::{read_json_file, write_atomic};
use crate::error::{IoContext, Result, SessionError, json_err};

pub const PRESET_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    #[serde(default = "preset_version")]
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub adjustments: Adjustments,
    /// Groups this preset sets; everything else on the target is left alone.
    #[serde(default)]
    pub groups: GroupSet,
    /// Groups applied as deltas from the default (e.g. +0.3 EV) instead of absolute.
    #[serde(default)]
    pub relative: GroupSet,
}

fn preset_version() -> u32 {
    PRESET_SCHEMA_VERSION
}

impl Preset {
    pub fn new(name: impl Into<String>, adjustments: Adjustments, groups: GroupSet) -> Self {
        Self {
            schema_version: PRESET_SCHEMA_VERSION,
            name: name.into(),
            adjustments,
            groups,
            relative: GroupSet::new(),
        }
    }

    /// Apply onto `target` (whose oriented size is `target_size`). Relative
    /// groups add the preset's change from the defaults.
    pub fn apply(&self, target: &Adjustments, target_size: Size) -> Adjustments {
        let clip = Clipboard::new(
            self.adjustments.clone(),
            Adjustments::default(),
            target_size,
        );
        let opts = PasteOptions {
            groups: self.groups.clone(),
            relative: self.relative.clone(),
            ..PasteOptions::default()
        };
        clip.apply(target, target_size, &opts)
    }
}

/// Directory of preset files. `~/Library/Application Support/Darkslide/presets/`
/// in the app; injectable for tests.
#[derive(Debug, Clone)]
pub struct PresetStore {
    dir: PathBuf,
}

impl PresetStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The default location for the current user, if `$HOME` is set.
    pub fn default_dir() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(|h| Path::new(&h).join("Library/Application Support/Darkslide/presets"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// File name for a preset: a readable sanitized stem plus a short hash of
    /// the exact name, so distinct names never collide.
    fn file_for(&self, name: &str) -> Result<PathBuf> {
        if name.trim().is_empty() || name.contains('\0') {
            return Err(SessionError::InvalidName(name.to_owned()));
        }
        let stem: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == ' ' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        let hash = blake3::hash(name.as_bytes()).to_hex();
        Ok(self
            .dir
            .join(format!("{}-{}.json", stem.trim(), &hash[..12])))
    }

    pub fn save(&self, preset: &Preset) -> Result<()> {
        let path = self.file_for(&preset.name)?;
        fs::create_dir_all(&self.dir).at(&self.dir)?;
        let bytes = serde_json::to_vec_pretty(preset).map_err(|e| json_err(&path, &e))?;
        write_atomic(&path, &bytes).at(&path)
    }

    pub fn get(&self, name: &str) -> Result<Option<Preset>> {
        let path = self.file_for(name)?;
        match read_json_file(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| json_err(&path, &e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).at(&path),
        }
    }

    /// Delete a preset file. Returns whether it existed.
    pub fn delete(&self, name: &str) -> Result<bool> {
        let path = self.file_for(name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).at(&path),
        }
    }

    /// All readable presets, sorted by name. Unparseable files are skipped
    /// (and returned in the second list) rather than failing the whole list.
    pub fn list(&self) -> Result<(Vec<Preset>, Vec<PathBuf>)> {
        let rd = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(e) => return Err(e).at(&self.dir),
        };
        let (mut ok, mut bad) = (Vec::new(), Vec::new());
        for entry in rd.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            match read_json_file(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Preset>(&b).ok())
            {
                Some(p) => ok.push(p),
                None => bad.push(path),
            }
        }
        ok.sort_by(|a: &Preset, b| a.name.cmp(&b.name));
        bad.sort();
        Ok((ok, bad))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp, clippy::unnecessary_wraps)]

    use ds_core::AdjustGroup;

    use super::*;

    fn warm() -> Preset {
        let mut a = Adjustments::default();
        a.exposure.exposure_ev = 0.5;
        a.detail.sharpen_amount = 300.0;
        Preset::new(
            "Warm / Sharp: A9",
            a,
            [AdjustGroup::Exposure].into_iter().collect(),
        )
    }

    #[test]
    fn save_get_list_delete() {
        let d = tempfile::tempdir().unwrap();
        let store = PresetStore::new(d.path().join("presets"));
        assert_eq!(store.list().unwrap().0, vec![]);
        assert_eq!(store.get("nope").unwrap(), None);
        let p = warm();
        store.save(&p).unwrap();
        let mut other = warm();
        other.name = "Warm _ Sharp_ A9".into(); // sanitizes to the same stem
        store.save(&other).unwrap();
        assert_eq!(store.get(&p.name).unwrap().unwrap(), p);
        assert_eq!(store.get(&other.name).unwrap().unwrap(), other);
        fs::write(store.dir().join("junk.json"), b"{").unwrap();
        fs::write(store.dir().join("notes.txt"), b"x").unwrap();
        let (list, bad) = store.list().unwrap();
        assert_eq!(
            list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["Warm / Sharp: A9", "Warm _ Sharp_ A9"]
        );
        assert_eq!(bad.len(), 1);
        assert!(store.delete(&p.name).unwrap());
        assert!(!store.delete(&p.name).unwrap());
        assert!(matches!(
            store.save(&Preset::new("  ", Adjustments::default(), GroupSet::new())),
            Err(SessionError::InvalidName(_))
        ));
        assert!(
            PresetStore::default_dir()
                .unwrap()
                .ends_with("Darkslide/presets")
        );
    }

    #[test]
    fn corrupt_preset_get_errors() {
        let d = tempfile::tempdir().unwrap();
        let store = PresetStore::new(d.path());
        store.save(&warm()).unwrap();
        let file = fs::read_dir(d.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(&file, b"garbage").unwrap();
        assert!(matches!(
            store.get(&warm().name),
            Err(SessionError::Json { .. })
        ));
    }

    #[test]
    fn apply_only_touches_preset_groups_and_supports_relative() {
        let p = warm();
        let mut target = Adjustments::default();
        target.exposure.exposure_ev = 1.0;
        target.detail.sharpen_amount = 50.0;
        let out = p.apply(&target, Size::new(10, 10));
        assert_eq!(out.exposure.exposure_ev, 0.5);
        assert_eq!(out.detail.sharpen_amount, 50.0, "Detail not in groups");
        let mut rel = p.clone();
        rel.relative.insert(AdjustGroup::Exposure);
        assert_eq!(
            rel.apply(&target, Size::new(10, 10)).exposure.exposure_ev,
            1.5
        );
    }

    #[test]
    fn missing_fields_default() {
        let p: Preset = serde_json::from_str(r#"{"name":"x","extra":1}"#).unwrap();
        assert_eq!(p.schema_version, PRESET_SCHEMA_VERSION);
        assert!(p.groups.is_empty());
    }
}
