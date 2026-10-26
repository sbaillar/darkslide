//! The app's people: names that work in every session.
//!
//! One JSON file (`~/Library/Application Support/Luxtable/people.json` in the
//! app), written atomically. Each person has a stable id (sidecar tags carry
//! it, so a rename reaches them) and up to [`MAX_EXEMPLARS`] of their confirmed
//! faces' vectors (f16, hex), from which "Is this …?" suggestions are made in
//! any session. Unlike the indexes this is the user's data: a file that can't
//! be read is an error, never silently replaced.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use lt_core::embedding;
use serde::{Deserialize, Serialize};

use crate::atomic::{read_json_file, write_atomic};
use crate::error::{IoContext, Result, SessionError, json_err};

pub const PEOPLE_SCHEMA_VERSION: u32 = 1;

/// Confirmed faces kept per person (the newest).
pub const MAX_EXEMPLARS: usize = 64;

/// A confirmed face kept for suggestions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exemplar {
    /// Content key (hex) and face index it came from.
    pub key: String,
    pub idx: u32,
    pub model: String,
    /// f16 little-endian, hex.
    pub vec: String,
}

impl Exemplar {
    pub fn new(key: String, idx: u32, model: &str, vector: &[f32]) -> Option<Self> {
        let unit = embedding::normalized(vector)?;
        Some(Self {
            key,
            idx,
            model: model.to_owned(),
            vec: hex(&embedding::encode_f16(&unit)),
        })
    }

    pub fn vector(&self) -> Option<Vec<f32>> {
        embedding::decode_f16(&unhex(&self.vec)?)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    pub id: String,
    pub name: String,
    /// Earlier names: photos of sessions not reopened since a rename still
    /// carry them, so Find matches them too.
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub exemplars: Vec<Exemplar>,
}

impl Person {
    /// This person's face vectors for `model`.
    pub fn vectors(&self, model: &str) -> Vec<Vec<f32>> {
        self.exemplars
            .iter()
            .filter(|e| e.model == model)
            .filter_map(Exemplar::vector)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct People {
    #[serde(default = "version")]
    pub schema_version: u32,
    #[serde(default)]
    pub people: Vec<Person>,
}

fn version() -> u32 {
    PEOPLE_SCHEMA_VERSION
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn new_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("p{t:x}{:x}", N.fetch_add(1, Ordering::Relaxed))
}

impl People {
    pub fn by_id(&self, id: &str) -> Option<&Person> {
        self.people.iter().find(|p| p.id == id)
    }

    /// The person called `name` (case and surrounding space ignored).
    pub fn by_name(&self, name: &str) -> Option<&Person> {
        let n = name.trim().to_lowercase();
        self.people.iter().find(|p| p.name.to_lowercase() == n)
    }

    /// The person called `name`, added if there is none. `None` for a blank name.
    pub fn ensure(&mut self, name: &str) -> Option<&Person> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        if self.by_name(name).is_none() {
            self.people.push(Person {
                id: new_id(),
                name: name.to_owned(),
                aliases: Vec::new(),
                exemplars: Vec::new(),
            });
        }
        self.by_name(name)
    }

    /// Rename; `false` when there's no such person, the name is blank, or
    /// another person already has it.
    pub fn rename(&mut self, id: &str, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.by_name(name).is_some_and(|p| p.id != id) {
            return false;
        }
        match self.people.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                if p.name != name {
                    let old = std::mem::replace(&mut p.name, name.to_owned());
                    if !p.aliases.iter().any(|a| a.eq_ignore_ascii_case(&old)) {
                        p.aliases.push(old);
                    }
                    p.aliases.retain(|a| !a.eq_ignore_ascii_case(name));
                }
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.people.len();
        self.people.retain(|p| p.id != id);
        self.people.len() != before
    }

    /// Add confirmed faces to a person (the same face again replaces its old
    /// entry), keeping the newest [`MAX_EXEMPLARS`].
    pub fn add_exemplars(&mut self, id: &str, faces: Vec<Exemplar>) -> bool {
        let Some(p) = self.people.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        for f in faces {
            p.exemplars
                .retain(|e| !(e.key == f.key && e.idx == f.idx && e.model == f.model));
            p.exemplars.push(f);
        }
        let extra = p.exemplars.len().saturating_sub(MAX_EXEMPLARS);
        p.exemplars.drain(..extra);
        true
    }

    /// Drop faces (`(key, idx)`) from every person: they were untagged.
    pub fn remove_exemplars(&mut self, faces: &[(String, u32)]) {
        for p in &mut self.people {
            p.exemplars
                .retain(|e| !faces.iter().any(|(k, i)| *k == e.key && *i == e.idx));
        }
    }
}

/// The people file.
#[derive(Debug, Clone)]
pub struct PeopleStore {
    path: PathBuf,
}

impl PeopleStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `~/Library/Application Support/Luxtable/people.json`.
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(|h| Path::new(&h).join("Library/Application Support/Luxtable/people.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The people; none yet when the file doesn't exist.
    pub fn load(&self) -> Result<People> {
        let bytes = match read_json_file(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(People::default_v()),
            Err(e) => return Err(e).at(&self.path),
        };
        let people: People =
            serde_json::from_slice(&bytes).map_err(|e| json_err(&self.path, &e))?;
        if people.schema_version > PEOPLE_SCHEMA_VERSION {
            return Err(SessionError::Json {
                path: self.path.clone(),
                message: format!(
                    "people schema v{} is newer than this build's v{PEOPLE_SCHEMA_VERSION}",
                    people.schema_version
                ),
            });
        }
        Ok(people)
    }

    pub fn save(&self, people: &People) -> Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).at(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(people).map_err(|e| json_err(&self.path, &e))?;
        write_atomic(&self.path, &bytes).at(&self.path)
    }

    /// Load, change, save (only when `f` says it changed something).
    pub fn update<T>(&self, f: impl FnOnce(&mut People) -> (T, bool)) -> Result<T> {
        let mut people = self.load()?;
        let (out, changed) = f(&mut people);
        if changed {
            self.save(&people)?;
        }
        Ok(out)
    }
}

impl People {
    fn default_v() -> Self {
        Self {
            schema_version: PEOPLE_SCHEMA_VERSION,
            people: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(key: &str, idx: u32, v: &[f32]) -> Exemplar {
        Exemplar::new(key.into(), idx, "fp", v).unwrap()
    }

    #[test]
    fn names_ids_and_renames() {
        let mut p = People::default_v();
        assert!(p.ensure("  ").is_none());
        let alex = p.ensure(" Alex ").unwrap().id.clone();
        assert_eq!(p.ensure("alex").unwrap().id, alex, "same person, any case");
        let sam = p.ensure("Sam").unwrap().id.clone();
        assert_ne!(alex, sam);
        assert_eq!(p.by_id(&sam).unwrap().name, "Sam");
        assert!(!p.rename(&sam, "ALEX"), "taken");
        assert!(!p.rename(&sam, " "));
        assert!(!p.rename("nobody", "X"));
        assert!(p.rename(&sam, "Samantha"));
        assert!(p.rename(&sam, "samantha"), "own name, new case");
        assert_eq!(p.by_name("SAMANTHA").unwrap().id, sam);
        assert_eq!(
            p.by_id(&sam).unwrap().aliases,
            ["Sam"],
            "case changes aren't aliases"
        );
        assert!(p.rename(&sam, "Sam"));
        assert_eq!(p.by_id(&sam).unwrap().aliases, ["samantha"]);
        assert!(p.rename(&sam, "Sam"), "unchanged");
        assert!(p.remove(&sam));
        assert!(!p.remove(&sam));
        assert!(p.by_id(&sam).is_none());
    }

    #[test]
    fn exemplars_are_capped_and_replaced() {
        let mut p = People::default_v();
        let id = p.ensure("Alex").unwrap().id.clone();
        assert!(!p.add_exemplars("nobody", vec![]));
        let faces: Vec<Exemplar> = (0..70)
            .map(|i| ex(&format!("{i:02}"), 0, &[1.0, i as f32]))
            .collect();
        assert!(p.add_exemplars(&id, faces));
        let person = p.by_id(&id).unwrap();
        assert_eq!(person.exemplars.len(), MAX_EXEMPLARS);
        assert_eq!(person.exemplars[0].key, "06", "oldest dropped");
        // The same face again replaces it (moves to newest).
        p.add_exemplars(&id, vec![ex("06", 0, &[0.0, 1.0])]);
        let person = p.by_id(&id).unwrap();
        assert_eq!(person.exemplars.len(), MAX_EXEMPLARS);
        assert_eq!(person.exemplars.last().unwrap().key, "06");
        let v = person.vectors("fp");
        assert_eq!(v.len(), MAX_EXEMPLARS);
        assert!((v.last().unwrap()[1] - 1.0).abs() < 1e-3);
        assert_eq!(person.vectors("other"), [] as [std::vec::Vec<f32>; 0]);
        p.remove_exemplars(&[("06".into(), 0), ("07".into(), 1)]);
        assert_eq!(p.by_id(&id).unwrap().exemplars.len(), MAX_EXEMPLARS - 1);
        assert!(Exemplar::new("k".into(), 0, "fp", &[0.0, 0.0]).is_none());
        let bad = Exemplar {
            key: "k".into(),
            idx: 0,
            model: "fp".into(),
            vec: "zz".into(),
        };
        assert!(bad.vector().is_none());
        assert!(unhex("abc").is_none());
        assert!(unhex("é1").is_none());
    }

    #[test]
    fn the_file_round_trips_and_errors_are_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        let store = PeopleStore::new(d.path().join("sub/people.json"));
        assert!(store.load().unwrap().people.is_empty(), "no file yet");
        let id = store
            .update(|p| (p.ensure("Alex").unwrap().id.clone(), true))
            .unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.by_id(&id).unwrap().name, "Alex");
        assert_eq!(loaded.schema_version, PEOPLE_SCHEMA_VERSION);
        // No change: not written.
        let mtime = fs::metadata(store.path()).unwrap().modified().unwrap();
        store.update(|_| ((), false)).unwrap();
        assert_eq!(
            fs::metadata(store.path()).unwrap().modified().unwrap(),
            mtime
        );
        // Damaged or newer: an error, and the file is left alone.
        fs::write(store.path(), b"{not json").unwrap();
        assert!(matches!(store.load(), Err(SessionError::Json { .. })));
        assert!(store.update(|_| ((), true)).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), b"{not json");
        fs::write(store.path(), br#"{"schema_version": 9, "people": []}"#).unwrap();
        assert!(store.load().is_err());
        // Older files without a version read as v1.
        fs::write(store.path(), br#"{"people": [{"id": "x", "name": "Old"}]}"#).unwrap();
        assert_eq!(store.load().unwrap().by_id("x").unwrap().name, "Old");
        // A directory in the way is an I/O error.
        let dir = PeopleStore::new(d.path());
        assert!(matches!(dir.load(), Err(SessionError::Io { .. })));
        assert!(PeopleStore::default_path().is_some());
    }
}
