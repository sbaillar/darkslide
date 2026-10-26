//! `<support>/Plugins/state.json`: what the user decided (enabled, granted, trusted)
//! and what was installed (pinned file hashes). Only the host writes it, atomically;
//! plug-ins have no way to reach it (no file access, no import that touches it).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::PluginError;
use crate::trust::FileHashes;

pub const STATE_FILE: &str = "state.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "one")]
    pub format: u32,
    /// Per plug-in id. Ids with no folder any more are kept, so a plug-in put back
    /// keeps its settings (its files must still match the pinned hashes).
    #[serde(default)]
    pub plugins: BTreeMap<String, PluginState>,
    /// Ed25519 public key (hex) → the signer's name, as the user trusted it.
    #[serde(default)]
    pub trusted_signers: BTreeMap<String, String>,
    /// Settings ▸ Plugins ▸ "Allow unsigned code plug-ins" (for authors).
    #[serde(default)]
    pub allow_unsigned_code: bool,
    /// Development folders loaded in place (absolute paths).
    #[serde(default)]
    pub dev_folders: Vec<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginState {
    #[serde(default)]
    pub enabled: bool,
    /// Permissions the user granted, in the manifest's spelling.
    #[serde(default)]
    pub granted: BTreeSet<String>,
    /// The files as installed or last approved (user plug-ins only).
    #[serde(default)]
    pub files: Option<FileHashes>,
    /// The version installed, for the downgrade check.
    #[serde(default)]
    pub version: Option<String>,
    /// The signing key first installed under this id (hex), or `unsigned`: a package
    /// signed otherwise is a different plug-in.
    #[serde(default)]
    pub signer: Option<String>,
    /// The user allowed this unsigned code plug-in to run (not its sensitive permissions).
    #[serde(default)]
    pub unsigned_ok: bool,
    /// The folder chosen for each granted `files.*` permission.
    #[serde(default)]
    pub folders: BTreeMap<String, String>,
}

impl State {
    /// Read `dir/state.json`. A missing file is an empty state. An unreadable one is
    /// moved aside (`state.json.unreadable`) and an empty state used: that only ever
    /// takes permissions away, never grants any.
    pub fn load(dir: &Path) -> (Self, Option<String>) {
        let path = dir.join(STATE_FILE);
        match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default_state(), None),
            Err(e) => (
                Self::default_state(),
                Some(format!("couldn't read {}: {e}", path.display())),
            ),
            Ok(bytes) => match serde_json::from_slice::<Self>(&bytes) {
                Ok(mut s) => {
                    s.migrate_legacy_ids();
                    (s, None)
                }
                Err(e) => {
                    let aside = path.with_extension("json.unreadable");
                    let _ = std::fs::rename(&path, &aside);
                    (
                        Self::default_state(),
                        Some(format!(
                            "plug-in settings were unreadable ({e}); kept as {}",
                            aside.display()
                        )),
                    )
                }
            },
        }
    }

    fn default_state() -> Self {
        Self {
            format: 1,
            ..Self::default()
        }
    }

    /// Write `dir/state.json` atomically.
    pub fn save(&self, dir: &Path) -> Result<(), PluginError> {
        std::fs::create_dir_all(dir)?;
        let bytes =
            serde_json::to_vec_pretty(self).map_err(|e| PluginError::Invalid(e.to_string()))?;
        lt_fsutil::write_atomic(&state_path(dir), &bytes)?;
        Ok(())
    }

    /// Decisions recorded for the bundled plug-ins under their ids from before the
    /// rename (`darkslide.immich`) move to the new ids (`luxtable.immich`): enabled,
    /// granted and the rest carry over. A new id already recorded wins.
    fn migrate_legacy_ids(&mut self) {
        use crate::manifest::{BUILTIN_PREFIX, LEGACY_BUILTIN_PREFIX};
        let old: Vec<String> = self
            .plugins
            .keys()
            .filter(|k| k.starts_with(LEGACY_BUILTIN_PREFIX))
            .cloned()
            .collect();
        for id in old {
            if let Some(st) = self.plugins.remove(&id) {
                let new = format!("{BUILTIN_PREFIX}{}", &id[LEGACY_BUILTIN_PREFIX.len()..]);
                self.plugins.entry(new).or_insert(st);
            }
        }
    }

    pub fn plugin(&mut self, id: &str) -> &mut PluginState {
        self.plugins.entry(id.to_owned()).or_default()
    }
}

#[must_use]
pub fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATE_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_keeps_unknown_ids() {
        let d = tempfile::tempdir().unwrap();
        let (mut s, err) = State::load(d.path());
        assert!(err.is_none());
        assert_eq!(s.format, 1);
        s.plugin("com.example.gone").enabled = true;
        s.plugin("com.example.gone").granted.insert("clock".into());
        s.trusted_signers.insert("ab".into(), "A".into());
        s.save(d.path()).unwrap();
        let (back, _) = State::load(d.path());
        assert_eq!(back, s);
    }

    #[test]
    fn bundled_plugin_decisions_move_to_the_new_ids() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(STATE_FILE),
            br#"{"format":1,"plugins":{
                "darkslide.immich":{"enabled":true,"granted":["network"]},
                "darkslide.archive":{"enabled":false},
                "luxtable.archive":{"enabled":true},
                "com.example.x":{"enabled":true}}}"#,
        )
        .unwrap();
        let (s, err) = State::load(d.path());
        assert!(err.is_none());
        let ids: Vec<&str> = s.plugins.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            ["com.example.x", "luxtable.archive", "luxtable.immich"]
        );
        assert!(s.plugins["luxtable.immich"].enabled);
        assert!(s.plugins["luxtable.immich"].granted.contains("network"));
        assert!(
            s.plugins["luxtable.archive"].enabled,
            "a new id already recorded wins"
        );
    }

    #[test]
    fn unreadable_state_is_kept_aside_and_grants_nothing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(STATE_FILE), b"{ not json").unwrap();
        let (s, err) = State::load(d.path());
        assert!(s.plugins.is_empty() && !s.allow_unsigned_code);
        assert!(err.unwrap().contains("unreadable"));
        assert!(d.path().join("state.json.unreadable").exists());
        assert!(!d.path().join(STATE_FILE).exists());
    }

    #[test]
    fn unreadable_path_reports_an_error() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join(STATE_FILE)).unwrap(); // a folder where the file goes
        let (s, err) = State::load(d.path());
        assert!(s.plugins.is_empty());
        assert!(err.unwrap().contains("couldn't read"));
    }
}
