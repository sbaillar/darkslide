//! What the last import wrote, per Lightroom photo: the reference for the
//! three-way merge that keeps re-imports idempotent and never overwrites a
//! Darkslide edit silently. Stored in the Darkslide session's store
//! (`.ds/settings/lightroom-import.json`, atomic writes). Losing it is safe:
//! without a record, any Darkslide value that differs from both the default
//! and Lightroom's is treated as the user's edit (a conflict), never overwritten.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ds_core::{ColorTag, PickState, Rating};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// File name under the session store's `.ds/settings/`.
pub const STATE_FILE: &str = "lightroom-import.json";
const VERSION: u32 = 1;

/// Values the import last wrote (or found already equal) for one variant.
/// `None`: never imported (the source had nothing to say, or a conflict was kept).
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
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copyright: Option<String>,
    /// Develop category key → the slice written (see `groups::slice`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Value>,
    /// Ids of the layers the import wrote (the masks category): the only
    /// layers it compares, replaces or removes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<u64>,
}

impl Record {
    /// Nothing recorded besides where the variant is.
    pub fn is_blank(&self) -> bool {
        *self
            == Record {
                variant: self.variant,
                rel: self.rel.clone(),
                ..Record::default()
            }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct State {
    pub version: u32,
    /// The source's id at the last import (a catalog's global id).
    pub library: Option<String>,
    /// Photo key → record.
    pub variants: BTreeMap<String, Record>,
    /// `col:<collection id>` → Darkslide album id.
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
    use super::*;

    #[test]
    fn round_trips_and_tolerates_absence() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), State::default());
        let mut s = State::default();
        let mut rec = Record {
            variant: 7,
            rel: "Capture/a.ARW".into(),
            rating: Rating::new(3),
            ..Record::default()
        };
        assert!(!rec.is_blank());
        rec.groups
            .insert("tone".into(), serde_json::json!({"/hdr": 1}));
        s.variants.insert("K".into(), rec);
        s.albums.insert("col:3".into(), 42);
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
    fn blank_records_hold_only_the_location() {
        let rec = Record {
            variant: 1,
            rel: "x".into(),
            ..Record::default()
        };
        assert!(rec.is_blank());
    }

    #[test]
    fn unreadable_state_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(path(dir.path()), b"{not json").unwrap();
        assert!(load(dir.path()).unwrap_err().contains(STATE_FILE));
        let d2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(path(d2.path())).unwrap();
        assert!(load(d2.path()).is_err());
        assert!(save(d2.path(), &mut State::default()).is_err());
    }
}
