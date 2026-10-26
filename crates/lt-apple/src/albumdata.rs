//! iPhoto 6 to 9.2: `AlbumData.xml` at the library's top, an XML property
//! list (keys as public readers of it show them):
//!
//! - `Archive Path`: where the library was when iPhoto wrote the file. Image
//!   paths are absolute and start with it; they are moved onto where the
//!   library is now.
//! - `Master Image List`: id → `ImagePath` (the photo as shown: an edited
//!   copy under `Modified/` once edited), `OriginalPath` (the original, when
//!   edited), `Caption` (iPhoto's title), `Comment` (the description),
//!   `Rating` (0-5), `Keywords` (ids into `List of Keywords`), `Roll`,
//!   `latitude` / `longitude` (iPhoto '09 and later).
//! - `List of Rolls`: events (`AlbumId`/`RollID`, `AlbumName`/`RollName`, `KeyList`).
//! - `List of Albums`: albums (`AlbumId`, `AlbumName`, `Album Type`,
//!   `KeyList`, `Parent`); the album of type `Flagged` lists the flagged photos.
//!
//! iPhoto kept an edit only as a rendered JPEG: the original is imported and
//! the photo noted as edited in iPhoto.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use lt_core::{GeoPoint, PickState, Rating};
use lt_lr::{Develop, LrCollection, LrLibrary, LrPhoto, Setting, SourceKind};
use plist::{Dictionary, Value};

use crate::detect::library_id;
use crate::error::{AppleError, IoAt, Result};
use crate::limits::{
    MAX_ALBUMDATA_BYTES, MAX_ALBUMDATA_EVENTS, MAX_KEYWORDS, MAX_PHOTOS, cap_text, plist,
};
use crate::map::IPHOTO_EDIT;

/// Album types that are iPhoto's own lists, not albums the user made.
const BUILT_IN: [&str; 8] = [
    "Master",
    "Special Roll",
    "Special Month",
    "Flagged",
    "Selected Event Album",
    "Event",
    "Folder",
    "Shelf",
];

fn text(d: &Dictionary, k: &str) -> Option<String> {
    d.get(k)
        .and_then(Value::as_string)
        .map(|s| cap_text(s.trim().to_owned()))
}

fn int(d: &Dictionary, k: &str) -> Option<i64> {
    match d.get(k)? {
        Value::Integer(i) => i.as_signed(),
        Value::Real(r) if r.is_finite() && r.abs() < 9e18 => Some(*r as i64),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn real(d: &Dictionary, k: &str) -> Option<f64> {
    match d.get(k)? {
        Value::Real(r) => Some(*r).filter(|r| r.is_finite()),
        Value::Integer(i) => i.as_signed().map(|n| n as f64),
        Value::String(s) => s.trim().parse().ok().filter(|r: &f64| r.is_finite()),
        _ => None,
    }
}

/// `KeyList`: photo ids as strings (or numbers).
fn keys(d: &Dictionary) -> Vec<String> {
    d.get("KeyList")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.trim().to_owned()),
                    Value::Integer(i) => Some(i.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A recorded path moved onto where the library is now; `None` when it would
/// leave the library through `..`. A path outside the recorded library (an
/// original iPhoto only referenced) stays as it is.
fn rebase(recorded: &str, archive: Option<&str>, root: &Path) -> Option<PathBuf> {
    let p = Path::new(recorded);
    let inside = archive
        .map(|a| a.trim_end_matches('/'))
        .filter(|a| !a.is_empty())
        .and_then(|a| p.strip_prefix(a).ok());
    let rel = match inside {
        Some(rel) => rel,
        None if p.is_relative() => p,
        None => return Some(p.to_owned()),
    };
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    Some(root.join(rel))
}

/// Read the `AlbumData.xml` of the library at `root`.
pub(crate) fn read(root: &Path, cancel: &dyn Fn() -> bool) -> Result<LrLibrary> {
    let path = root.join("AlbumData.xml");
    let len = fs::metadata(&path).at(&path)?.len();
    let xml_err = |message: String| AppleError::Xml {
        path: path.clone(),
        message,
    };
    if len > MAX_ALBUMDATA_BYTES {
        return Err(xml_err(format!("too large to read ({len} bytes)")));
    }
    let bytes = fs::read(&path).at(&path)?;
    if cancel() {
        return Err(AppleError::Cancelled);
    }
    let doc = plist(&bytes, MAX_ALBUMDATA_EVENTS).map_err(xml_err)?;
    drop(bytes);
    let top = doc.as_dictionary().ok_or_else(|| AppleError::Xml {
        path: path.clone(),
        message: "not a dictionary".into(),
    })?;
    let images = top
        .get("Master Image List")
        .and_then(Value::as_dictionary)
        .ok_or_else(|| AppleError::NotALibrary {
            path: path.clone(),
            message: "no Master Image List".into(),
        })?;
    if images.len() > MAX_PHOTOS {
        return Err(AppleError::Xml {
            path,
            message: format!("more than {MAX_PHOTOS} photos: not read"),
        });
    }
    let archive = top.get("Archive Path").and_then(Value::as_string);
    let words: HashMap<String, String> = top
        .get("List of Keywords")
        .and_then(Value::as_dictionary)
        .map(|d| {
            d.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_string()?.trim().to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let albums: Vec<&Dictionary> = list(top, "List of Albums");
    let flagged: HashSet<String> = albums
        .iter()
        .filter(|a| text(a, "Album Type").as_deref() == Some("Flagged"))
        .flat_map(|a| keys(a))
        .collect();
    let ctx = Photos {
        root,
        archive,
        words: &words,
        flagged: &flagged,
    };
    let mut warnings = Vec::new();
    let mut photos = Vec::with_capacity(images.len());
    let (mut outside, mut edited) = (0usize, 0usize);
    for (n, (id, v)) in images.iter().enumerate() {
        if n % 1000 == 0 && cancel() {
            return Err(AppleError::Cancelled);
        }
        match v.as_dictionary().map(|d| ctx.photo(id, d)) {
            Some(Some(Ok(p))) => {
                edited += usize::from(p.develop.is_some());
                photos.push(p);
            }
            Some(Some(Err(Outside))) => outside += 1,
            _ => {}
        }
    }
    if outside > 0 {
        warnings.push(format!(
            "{outside} photo path{} leaving the library through \u{201c}..\u{201d}: not imported",
            if outside == 1 { "" } else { "s" }
        ));
    }
    if edited > 0 {
        warnings.push(format!(
            "{edited} photo{} edited in iPhoto: the originals are imported, without the edits \
             (iPhoto kept them only as rendered copies)",
            if edited == 1 { " was" } else { "s were" }
        ));
    }
    let collections = collections(top, &albums, &photos);
    Ok(LrLibrary {
        kind: SourceKind::Catalog,
        origin: root.to_owned(),
        id: Some(library_id(root)),
        written_by: Some(
            text(top, "Application Version")
                .map_or_else(|| "iPhoto".to_owned(), |v| format!("iPhoto {v}")),
        ),
        photos,
        collections,
        warnings,
    })
}

/// A photo path that would leave the library.
struct Outside;

/// What every photo is read against.
struct Photos<'a> {
    root: &'a Path,
    archive: Option<&'a str>,
    words: &'a HashMap<String, String>,
    flagged: &'a HashSet<String>,
}

impl Photos<'_> {
    /// One `Master Image List` entry; `None` when it names no file.
    fn photo(&self, id: &str, d: &Dictionary) -> Option<Result<LrPhoto, Outside>> {
        let root = self.root;
        let original = text(d, "OriginalPath").filter(|s| !s.is_empty());
        let recorded = original.clone().or_else(|| text(d, "ImagePath"))?;
        let Some(path) = rebase(&recorded, self.archive, root) else {
            return Some(Err(Outside));
        };
        // The library's own folder of originals (`Originals`, or `Masters` in 9.x).
        let folder = path
            .strip_prefix(root)
            .ok()
            .and_then(|rel| rel.components().next())
            .map(|c| root.join(c));
        let develop = original
            .is_some()
            .then(|| Develop::from([(IPHOTO_EDIT.to_owned(), Setting::Bool(true))]));
        let mut kw: Vec<String> = Vec::new();
        for k in d
            .get("Keywords")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = match k {
                Value::String(s) => s.trim().to_owned(),
                Value::Integer(i) => i.to_string(),
                _ => continue,
            };
            if let Some(w) = self.words.get(&id).filter(|w| !w.is_empty())
                && !kw.contains(w)
                && kw.len() < MAX_KEYWORDS
            {
                kw.push(w.clone());
            }
        }
        let place = match (real(d, "latitude"), real(d, "longitude")) {
            (Some(lat), Some(lon)) if lat != 0.0 || lon != 0.0 => GeoPoint::new(lat, lon, None),
            _ => None,
        };
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let title = text(d, "Caption").filter(|t| !t.eq_ignore_ascii_case(&stem));
        Some(Ok(LrPhoto {
            key: id.to_owned(),
            path,
            rating: int(d, "Rating")
                .and_then(|r| u8::try_from(r).ok())
                .and_then(Rating::new),
            pick: Some(if self.flagged.contains(id) {
                PickState::Picked
            } else {
                PickState::Unflagged
            }),
            keywords: Some(kw),
            title: Some(title.unwrap_or_default()),
            caption: Some(text(d, "Comment").unwrap_or_default()),
            geotag: place,
            folder,
            develop,
            ..LrPhoto::default()
        }))
    }
}

/// Events (rolls) and albums, with the photos read.
fn collections(top: &Dictionary, albums: &[&Dictionary], photos: &[LrPhoto]) -> Vec<LrCollection> {
    let known: HashSet<&str> = photos.iter().map(|p| p.key.as_str()).collect();
    let members = |d: &Dictionary| -> Vec<String> {
        let mut seen = HashSet::new();
        keys(d)
            .into_iter()
            .filter(|k| known.contains(k.as_str()) && seen.insert(k.clone()))
            .collect()
    };
    let mut out = Vec::new();
    for r in list(top, "List of Rolls") {
        let Some(id) = int(r, "RollID").or_else(|| int(r, "AlbumId")) else {
            continue;
        };
        out.push(LrCollection {
            // Events apart from albums: negative ids.
            id: -id - 1,
            name: text(r, "RollName")
                .or_else(|| text(r, "AlbumName"))
                .unwrap_or_default(),
            parents: Vec::new(),
            smart: false,
            members: members(r),
            unsupported: None,
        });
    }
    let names: HashMap<i64, (String, Option<i64>)> = albums
        .iter()
        .filter_map(|a| {
            Some((
                int(a, "AlbumId")?,
                (text(a, "AlbumName").unwrap_or_default(), int(a, "Parent")),
            ))
        })
        .collect();
    for a in albums {
        let Some(id) = int(a, "AlbumId") else {
            continue;
        };
        let kind = text(a, "Album Type").unwrap_or_else(|| "Regular".into());
        if BUILT_IN.contains(&kind.as_str())
            || a.get("Master").and_then(Value::as_boolean) == Some(true)
        {
            continue;
        }
        let (smart, unsupported) = match kind.as_str() {
            "Regular" => (false, None),
            "Smart" => (
                true,
                Some("a smart album (a saved search): Luxtable can't read its rules".to_owned()),
            ),
            other => (
                false,
                Some(format!(
                    "an iPhoto {}: Luxtable has no album for it",
                    other.to_lowercase()
                )),
            ),
        };
        out.push(LrCollection {
            id,
            name: text(a, "AlbumName").unwrap_or_default(),
            parents: parents(&names, int(a, "Parent")),
            smart,
            members: members(a),
            unsupported,
        });
    }
    out
}

fn list<'a>(top: &'a Dictionary, key: &str) -> Vec<&'a Dictionary> {
    top.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_dictionary).collect())
        .unwrap_or_default()
}

/// Folder names around an album, outermost first.
fn parents(names: &HashMap<i64, (String, Option<i64>)>, mut at: Option<i64>) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    while let Some(id) = at {
        if out.len() >= 64 || !seen.insert(id) {
            break;
        }
        let Some((name, parent)) = names.get(&id) else {
            break;
        };
        if !name.is_empty() {
            out.push(name.clone());
        }
        at = *parent;
    }
    out.reverse();
    out
}

#[cfg(test)]
#[path = "albumdata_tests.rs"]
mod tests;
