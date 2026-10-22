//! What the imports since the last revert changed in Darkslide, so the user
//! can take them back with one command ("Revert Capture One Import").
//!
//! Every real import records, before it writes anything:
//! - per variant and organization field (rating, tag, pick, keywords), the
//!   value before and after;
//! - per adjustment *setting* it changed (a JSON pointer into the
//!   adjustments, e.g. `/exposure/exposure_ev`), the value before and after.
//!   Settings the import didn't change are never recorded, so a revert never
//!   touches them (HSL, masks, a vibrance set between two imports…);
//! - each variant it creates, as the import left it (all fields), recorded
//!   right after the variant is made;
//! - the albums it creates and the album members it adds.
//!
//! Later imports fold into the same journal (the oldest "before" wins, the
//! newest "after" wins), so a revert returns to the state before the first
//! import that hasn't been reverted. A revert only restores a field or
//! setting that still holds what the import wrote, and only deletes a created
//! variant that still equals what the import left: anything changed in
//! Darkslide since is kept. It also restores the import record
//! ([`crate::state`]) from before, so a later import treats the photos as
//! never imported.
//!
//! Stored next to the import record, in the Darkslide store
//! (`.ds/settings/capture-one-import-undo.json`, atomic writes).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ds_core::{Adjustments, ColorTag, PickState, Rating};
use ds_session::{AlbumId, Session, VariantId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{C1Error, IoAt, Result};
use crate::import::{Current, Writes, apply};
use crate::state::{self, State};

/// File name under the session store's `.ds/settings/`.
pub const JOURNAL_FILE: &str = "capture-one-import-undo.json";
const VERSION: u32 = 2;

/// Organization fields of one variant; `None`: not written by the import.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Values {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<Rating>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_tag: Option<ColorTag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pick: Option<PickState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keywords: Option<Vec<String>>,
}

impl Values {
    /// Fill the fields `self` lacks from `other`.
    fn or(&mut self, other: Values) {
        self.rating = self.rating.or(other.rating);
        self.color_tag = self.color_tag.or(other.color_tag);
        self.pick = self.pick.or(other.pick);
        if self.keywords.is_none() {
            self.keywords = other.keywords;
        }
    }
}

/// One adjustment setting before and after.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Patch {
    pub before: Value,
    pub after: Value,
}

/// One existing variant's fields before and after the imports.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Change {
    pub before: Values,
    pub after: Values,
    /// JSON pointer into the adjustments → the setting before and after.
    pub adjustments: BTreeMap<String, Patch>,
}

/// A variant an import created, as the import left it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Created {
    pub rating: Rating,
    pub color_tag: ColorTag,
    pub pick: PickState,
    pub keywords: Vec<String>,
    pub adjustments: Adjustments,
}

impl Created {
    pub fn of(cur: &Current) -> Self {
        Self {
            rating: cur.rating,
            color_tag: cur.color_tag,
            pick: cur.pick,
            keywords: cur.keywords.clone(),
            adjustments: cur.adjustments.clone(),
        }
    }

    fn matches(&self, cur: &Current) -> bool {
        cur.rating == self.rating
            && cur.color_tag == self.color_tag
            && cur.pick == self.pick
            && cur.keywords == self.keywords
            && cur.adjustments == self.adjustments
    }

    /// A later import's writes to this variant.
    fn absorb(&mut self, ch: &Change) {
        let a = &ch.after;
        self.rating = a.rating.unwrap_or(self.rating);
        self.color_tag = a.color_tag.unwrap_or(self.color_tag);
        self.pick = a.pick.unwrap_or(self.pick);
        if let Some(k) = &a.keywords {
            self.keywords.clone_from(k);
        }
        if !ch.adjustments.is_empty() {
            let mut v = to_value(&self.adjustments);
            for (path, p) in &ch.adjustments {
                if let Some(slot) = v.pointer_mut(path) {
                    *slot = p.after.clone();
                }
            }
            if let Ok(a) = serde_json::from_value(v) {
                self.adjustments = a;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Journal {
    pub version: u32,
    /// The import record before the first journaled import.
    pub state_before: State,
    /// Existing variants the imports wrote to.
    pub variants: BTreeMap<i64, Change>,
    /// Variants the imports created (for C1's extra variants), as the
    /// imports left them.
    pub created: BTreeMap<i64, Created>,
    /// Created variants whose fields an import had yet to write, as made
    /// (an import that failed part-way).
    pub created_unwritten: BTreeMap<i64, Created>,
    pub albums_created: Vec<i64>,
    /// Album → the members the imports added.
    pub members_added: BTreeMap<i64, Vec<i64>>,
}

fn to_value(a: &Adjustments) -> Value {
    serde_json::to_value(a).unwrap_or(Value::Null)
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The settings `after` changes from `before`, as JSON pointers. Objects with
/// the same keys are compared key by key; anything else (numbers, curves,
/// an enum that changed variant) is one setting.
pub(crate) fn diff(before: &Adjustments, after: &Adjustments) -> BTreeMap<String, Patch> {
    fn walk(path: &str, b: &Value, a: &Value, out: &mut BTreeMap<String, Patch>) {
        match (b, a) {
            (Value::Object(bo), Value::Object(ao))
                if bo.len() == ao.len() && bo.keys().all(|k| ao.contains_key(k)) =>
            {
                for (k, bv) in bo {
                    walk(&format!("{path}/{}", escape(k)), bv, &ao[k], out);
                }
            }
            _ if b != a => {
                out.insert(
                    path.to_owned(),
                    Patch {
                        before: b.clone(),
                        after: a.clone(),
                    },
                );
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    walk("", &to_value(before), &to_value(after), &mut out);
    out
}

/// Fold a newer import's setting patches into older ones.
fn fold_patches(old: &mut BTreeMap<String, Patch>, newer: BTreeMap<String, Patch>) {
    for (path, p) in newer {
        if let Some(o) = old.get_mut(&path) {
            o.after = p.after;
            continue;
        }
        // Inside a setting already recorded (an enum both imports changed).
        if let Some((parent, o)) = old
            .iter_mut()
            .find(|(k, _)| path.starts_with(&format!("{k}/")))
        {
            if let Some(slot) = o.after.pointer_mut(&path[parent.len()..]) {
                *slot = p.after;
            }
            continue;
        }
        // Around settings already recorded: their older "before" values win.
        let inner: Vec<String> = old
            .keys()
            .filter(|k| k.starts_with(&format!("{path}/")))
            .cloned()
            .collect();
        let mut before = p.before;
        for k in inner {
            let o = old.remove(&k).expect("listed");
            if let Some(slot) = before.pointer_mut(&k[path.len()..]) {
                *slot = o.before;
            }
        }
        old.insert(
            path,
            Patch {
                before,
                after: p.after,
            },
        );
    }
}

impl Journal {
    pub fn new(state_before: State) -> Self {
        Self {
            state_before,
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.variants.is_empty()
            && self.created.is_empty()
            && self.created_unwritten.is_empty()
            && self.albums_created.is_empty()
            && self.members_added.is_empty()
    }

    pub fn added_members(&mut self, album: AlbumId, vs: &[VariantId]) {
        let m = self.members_added.entry(album.0).or_default();
        for v in vs {
            if !m.contains(&v.0) {
                m.push(v.0);
            }
        }
    }

    /// Fold a newer import into this journal: the older "before" values and
    /// import record stay, the newer "after" values replace.
    fn absorb(&mut self, newer: Journal) {
        for (v, mut ch) in newer.variants {
            if let Some(c) = self.created.get_mut(&v) {
                c.absorb(&ch);
                continue;
            }
            match self.variants.get_mut(&v) {
                Some(old) => {
                    old.before.or(ch.before);
                    ch.after.or(std::mem::take(&mut old.after));
                    old.after = ch.after;
                    fold_patches(&mut old.adjustments, ch.adjustments);
                }
                None => {
                    self.variants.insert(v, ch);
                }
            }
        }
        self.created.extend(newer.created);
        self.created_unwritten.extend(newer.created_unwritten);
        for a in newer.albums_created {
            if !self.albums_created.contains(&a) {
                self.albums_created.push(a);
            }
        }
        for (a, vs) in newer.members_added {
            let ids: Vec<VariantId> = vs.into_iter().map(VariantId).collect();
            self.added_members(AlbumId(a), &ids);
        }
    }
}

pub(crate) fn path(store: &Path) -> PathBuf {
    store.join(".ds/settings").join(JOURNAL_FILE)
}

fn load(store: &Path) -> Result<Option<Journal>> {
    let p = path(store);
    match std::fs::read(&p) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| C1Error::Io {
                path: p,
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).at(&p),
    }
}

fn save(store: &Path, j: &mut Journal) -> Result<()> {
    j.version = VERSION;
    let p = path(store);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).at(dir)?;
    }
    let json = serde_json::to_vec_pretty(j).map_err(|e| C1Error::Io {
        path: p.clone(),
        source: std::io::Error::other(e),
    })?;
    ds_session::write_file_atomic(&p, &json).at(&p)
}

/// The journal of a real import in progress, saved before (or, for what only
/// gets an id when made, right after) each change to the session. A dry run
/// keeps it in memory only.
pub(crate) struct Writer {
    store: Option<PathBuf>,
    pub j: Journal,
}

impl Writer {
    /// A dry run's: never saved.
    pub fn dry() -> Self {
        Self {
            store: None,
            j: Journal::default(),
        }
    }

    /// Fold `planned` into the store's journal and save it before anything is
    /// written. An unreadable journal is replaced (this import alone becomes
    /// revertible).
    pub fn begin(store: &Path, planned: Journal) -> Result<Self> {
        let j = match load(store) {
            Ok(Some(mut old)) => {
                old.absorb(planned);
                old
            }
            Ok(None) | Err(_) => planned,
        };
        let mut w = Self {
            store: Some(store.to_owned()),
            j,
        };
        if !w.j.is_empty() {
            w.save()?;
        }
        Ok(w)
    }

    pub fn save(&mut self) -> Result<()> {
        match &self.store {
            Some(store) => save(store, &mut self.j),
            None => Ok(()),
        }
    }
}

/// Add one import's changes to the store's journal.
#[cfg(test)]
pub(crate) fn record(store: &Path, newer: Journal) -> Result<()> {
    Writer::begin(store, newer).map(drop)
}

/// Whether there is an import to revert.
pub fn can_revert(session: &Session) -> bool {
    path(session.root()).is_file()
}

/// What a revert did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RevertReport {
    /// Variants with at least one field put back.
    pub restored: usize,
    /// Fields and settings (and created variants and albums) left alone
    /// because they changed in Darkslide after the import.
    pub kept: usize,
    pub variants_removed: usize,
    pub albums_removed: usize,
    /// Albums that existed before the import and lost the members it added.
    pub albums_trimmed: usize,
}

/// `cur` with the import's settings put back where they still hold what it
/// wrote; `None` when none does. Counts the settings kept.
fn revert_settings(
    cur: &Adjustments,
    patches: &BTreeMap<String, Patch>,
    kept: &mut usize,
) -> Option<Adjustments> {
    let mut v = to_value(cur);
    let mut any = false;
    for (path, p) in patches {
        match v.pointer_mut(path) {
            Some(slot) if *slot == p.after => {
                *slot = p.before.clone();
                any = true;
            }
            _ => *kept += 1,
        }
    }
    if !any {
        return None;
    }
    // Unreadable when a setting kept no longer fits one put back: keep all.
    serde_json::from_value(v).ok().or_else(|| {
        *kept += 1;
        None
    })
}

/// Take back the journaled imports. `Ok(None)`: nothing to revert.
pub fn revert(session: &mut Session) -> Result<Option<RevertReport>> {
    let store = session.root().to_owned();
    let Some(j) = load(&store)? else {
        return Ok(None);
    };
    let mut report = RevertReport::default();
    let mut writes = Writes::default();
    for (&id, ch) in &j.variants {
        let v = VariantId(id);
        let Ok(cur) = Current::of(session, v) else {
            continue; // deleted since
        };
        let before = &ch.before;
        let mut any = false;
        let mut put = |same: bool, queue: &mut dyn FnMut()| {
            if same {
                queue();
                any = true;
            } else {
                report.kept += 1;
            }
        };
        if let (Some(a), Some(b)) = (ch.after.rating, before.rating) {
            put(cur.rating == a, &mut || {
                writes.ratings.entry(b).or_default().push(v);
            });
        }
        if let (Some(a), Some(b)) = (ch.after.color_tag, before.color_tag) {
            put(cur.color_tag == a, &mut || {
                writes.tags.entry(b).or_default().push(v);
            });
        }
        if let (Some(a), Some(b)) = (ch.after.pick, before.pick) {
            put(cur.pick == a, &mut || {
                writes.picks.entry(b).or_default().push(v);
            });
        }
        if let (Some(a), Some(b)) = (&ch.after.keywords, &before.keywords) {
            put(cur.keywords == *a, &mut || {
                writes.keywords.entry(b.clone()).or_default().push(v);
            });
        }
        if let Some(a) = revert_settings(&cur.adjustments, &ch.adjustments, &mut report.kept) {
            writes.adjustments.push((v, a));
            any = true;
        }
        report.restored += usize::from(any);
    }
    for (&id, created) in &j.created {
        let v = VariantId(id);
        let Ok(cur) = Current::of(session, v) else {
            continue; // deleted since
        };
        let as_made = j.created_unwritten.get(&id);
        if created.matches(&cur) || as_made.is_some_and(|m| m.matches(&cur)) {
            session.delete_variant(v)?;
            report.variants_removed += 1;
        } else {
            report.kept += 1;
        }
    }
    apply(session, &writes)?;
    for (&album, added) in &j.members_added {
        let id = AlbumId(album);
        let Ok(members) = session.album_variants(id) else {
            continue; // deleted since
        };
        let added: Vec<VariantId> = added.iter().map(|v| VariantId(*v)).collect();
        if j.albums_created.contains(&album) && members.iter().all(|m| added.contains(m)) {
            session.delete_album(id)?;
            report.albums_removed += 1;
        } else {
            session.remove_from_album(id, &added)?;
            if j.albums_created.contains(&album) {
                report.kept += 1;
            } else {
                report.albums_trimmed += 1;
            }
        }
    }
    let mut before = j.state_before;
    state::save(&store, &mut before).at(&state::path(&store))?;
    let p = path(&store);
    std::fs::remove_file(&p).at(&p)?;
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;
    use ds_core::adjust::WhiteBalance;

    fn adj(f: impl FnOnce(&mut Adjustments)) -> Adjustments {
        let mut a = Adjustments::default();
        f(&mut a);
        a
    }

    #[test]
    fn diff_is_per_setting() {
        let b = adj(|a| a.presence.vibrance = 30.0);
        let a = adj(|a| {
            a.presence.vibrance = 30.0;
            a.exposure.exposure_ev = 0.5;
            a.white_balance = WhiteBalance::Custom {
                temp_k: 5000.0,
                tint: 0.0,
            };
        });
        let d = diff(&b, &a);
        assert!(d.contains_key("/exposure/exposure_ev"), "{d:?}");
        assert!(!d.keys().any(|k| k.contains("vibrance")));
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(diff(&a, &a).is_empty());
        assert_eq!(escape("a/b~c"), "a~1b~0c");
    }

    #[test]
    fn patches_fold_oldest_before_newest_after() {
        let base = Adjustments::default();
        let wb = |k: f32| {
            adj(|a| {
                a.white_balance = WhiteBalance::Custom {
                    temp_k: k,
                    tint: 0.0,
                }
            })
        };
        // As shot → 5000 K (whole setting), then 5000 → 6000 (inside it).
        let mut old = diff(&base, &wb(5000.0));
        fold_patches(&mut old, diff(&wb(5000.0), &wb(6000.0)));
        assert_eq!(old.len(), 1, "{old:?}");
        let mut kept = 0;
        let back = revert_settings(&wb(6000.0), &old, &mut kept).unwrap();
        assert_eq!(back, base);
        // 5000 → 6000 (inside), then 6000 K → as shot (around it).
        let mut old = diff(&wb(5000.0), &wb(6000.0));
        fold_patches(&mut old, diff(&wb(6000.0), &base));
        assert_eq!(old.len(), 1, "{old:?}");
        assert_eq!(revert_settings(&base, &old, &mut kept).unwrap(), wb(5000.0));
        // The same setting twice.
        let e = |v: f32| adj(|a| a.exposure.exposure_ev = v);
        let mut old = diff(&e(0.0), &e(0.5));
        fold_patches(&mut old, diff(&e(0.5), &e(0.9)));
        assert_eq!(revert_settings(&e(0.9), &old, &mut kept).unwrap(), e(0.0));
        assert_eq!(kept, 0);
        // Changed since: kept, and nothing to write.
        assert_eq!(revert_settings(&e(0.3), &old, &mut kept), None);
        assert_eq!(kept, 1);
    }

    #[test]
    fn created_variants_take_later_writes() {
        let mut c = Created::default();
        let ch = Change {
            after: Values {
                rating: Rating::new(3),
                keywords: Some(vec!["x".into()]),
                ..Values::default()
            },
            adjustments: diff(
                &Adjustments::default(),
                &adj(|a| a.exposure.exposure_ev = 1.0),
            ),
            ..Change::default()
        };
        c.absorb(&ch);
        assert_eq!(c.rating.stars(), 3);
        assert_eq!(c.keywords, vec!["x"]);
        assert_eq!(c.adjustments.exposure.exposure_ev, 1.0);
        let mut j = Journal::default();
        j.created.insert(7, Created::default());
        j.absorb(Journal {
            variants: [(7, ch)].into(),
            ..Journal::default()
        });
        assert!(j.variants.is_empty());
        assert_eq!(j.created[&7].rating.stars(), 3);
    }
}
