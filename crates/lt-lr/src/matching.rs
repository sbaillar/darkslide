//! Which Luxtable photo is a Lightroom photo. In order:
//! 1. **path**: the photo's path is under the session's folder (an in-place session maps
//!    `Capture/<path under its folder>` to the file);
//! 2. **content**: the file is read where Lightroom says it is, and its
//!    BLAKE3 is looked up among the session's photos of the same size;
//! 3. **name and capture time**: the same file name and capture time, when
//!    exactly one photo has both (the catalog came from another volume).
//!
//! A photo several Luxtable photos could be, or that another Lightroom photo
//! already took, is *ambiguous* and not imported.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lt_core::FileHash;
use lt_session::identity::hash_original;
use lt_session::{ImageId, ImageInfo, Session, UNHASHED};

use crate::error::{LrError, Result};
use crate::model::LrPhoto;

/// Most files hashed to find photos by content in one import.
pub(crate) const MAX_HASHED: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum By {
    Path,
    Hash,
    Name,
}

#[derive(Debug, Default)]
pub(crate) struct Matches {
    /// Photo key → its Luxtable photo.
    pub found: HashMap<String, (ImageId, By)>,
    /// Paths of the photos that could be more than one Luxtable photo (or were taken by another).
    pub ambiguous: Vec<String>,
    keys_ambiguous: HashSet<String>,
    pub warnings: Vec<String>,
}

impl Matches {
    fn ambiguous(&mut self, p: &LrPhoto) {
        if self.keys_ambiguous.insert(p.key.clone()) {
            self.ambiguous.push(p.path.to_string_lossy().into_owned());
        }
    }
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_owned())
}

fn file_name_lower(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().to_lowercase())
}

/// `photo`'s path relative to the session's folder, `/`-separated.
fn rel_in_folder(root: &Path, root_c: &Path, photo: &Path) -> Option<String> {
    let rel = photo
        .strip_prefix(root)
        .or_else(|_| photo.strip_prefix(root_c))
        .map(Path::to_path_buf)
        .or_else(|_| canonical(photo).strip_prefix(root_c).map(Path::to_path_buf))
        .ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Match the photos (virtual copies are matched through their master).
#[allow(clippy::too_many_lines)] // the three steps share their state
pub(crate) fn match_photos(
    session: &Session,
    photos: &[LrPhoto],
    images: &[ImageInfo],
    cancelled: &dyn Fn() -> bool,
) -> Result<Matches> {
    let mut m = Matches::default();
    let masters: Vec<&LrPhoto> = photos.iter().filter(|p| p.master.is_none()).collect();
    let mut taken: HashMap<ImageId, String> = HashMap::new();
    let claim = |m: &mut Matches,
                 taken: &mut HashMap<ImageId, String>,
                 p: &LrPhoto,
                 id: ImageId,
                 by: By| {
        match taken.get(&id) {
            Some(other) if *other != p.key => m.ambiguous(p),
            _ => {
                taken.insert(id, p.key.clone());
                m.found.insert(p.key.clone(), (id, by));
            }
        }
    };

    // 1. Path.
    if let Some(root) = session.external_root() {
        let root_c = canonical(root);
        let exact: HashMap<&str, &ImageInfo> =
            images.iter().map(|i| (i.rel_path.as_str(), i)).collect();
        let mut lower: HashMap<String, Vec<&ImageInfo>> = HashMap::new();
        for i in images {
            lower
                .entry(i.rel_path.as_str().to_lowercase())
                .or_default()
                .push(i);
        }
        for p in &masters {
            if cancelled() {
                return Err(LrError::Cancelled);
            }
            let Some(rel) = rel_in_folder(root, &root_c, &p.path) else {
                continue;
            };
            let key = format!("Capture/{rel}");
            if let Some(i) = exact.get(key.as_str()) {
                claim(&mut m, &mut taken, p, i.id, By::Path);
            } else if let Some(c) = lower.get(&key.to_lowercase()) {
                if let [i] = c.as_slice() {
                    claim(&mut m, &mut taken, p, i.id, By::Path);
                } else {
                    m.ambiguous(p);
                }
            }
        }
    }

    // 2. Content.
    let mut by_size: HashMap<u64, Vec<&ImageInfo>> = HashMap::new();
    for i in images {
        by_size.entry(i.file_size).or_default().push(i);
    }
    let mut hashed = 0usize;
    let mut session_hashes: HashMap<ImageId, Option<FileHash>> = HashMap::new();
    for p in &masters {
        if m.found.contains_key(&p.key) || m.keys_ambiguous.contains(&p.key) {
            continue;
        }
        if cancelled() {
            return Err(LrError::Cancelled);
        }
        // A FIFO or device named by a catalog must never be read.
        let Ok(md) = std::fs::metadata(&p.path).and_then(|md| {
            if md.is_file() {
                Ok(md)
            } else {
                Err(std::io::ErrorKind::InvalidInput.into())
            }
        }) else {
            continue;
        };
        let Some(candidates) = by_size.get(&md.len()) else {
            continue;
        };
        if hashed >= MAX_HASHED {
            m.warnings.push(format!(
                "More than {MAX_HASHED} files to compare by content: the rest were matched by name only"
            ));
            break;
        }
        hashed += 1;
        let hash = match hash_original(&p.path, None, cancelled) {
            Ok(h) => h.file,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                return Err(LrError::Cancelled);
            }
            Err(_) => continue,
        };
        let mut hits: Vec<ImageId> = Vec::new();
        for i in candidates {
            let theirs = if i.hash == UNHASHED {
                *session_hashes.entry(i.id).or_insert_with(|| {
                    hash_original(&session.original_path(i.rel_path.as_str()), None, &|| false)
                        .ok()
                        .map(|h| h.file)
                })
            } else {
                Some(i.hash)
            };
            if theirs == Some(hash) {
                hits.push(i.id);
            }
        }
        match hits.as_slice() {
            [] => {}
            [id] => claim(&mut m, &mut taken, p, *id, By::Hash),
            _ => m.ambiguous(p),
        }
    }

    // 3. Name and capture time.
    let mut by_name: HashMap<String, Vec<&ImageInfo>> = HashMap::new();
    for i in images {
        if let Some(n) = file_name_lower(Path::new(i.rel_path.as_str())) {
            by_name.entry(n).or_default().push(i);
        }
    }
    for p in &masters {
        if m.found.contains_key(&p.key) || m.keys_ambiguous.contains(&p.key) {
            continue;
        }
        let (Some(name), Some(t)) = (file_name_lower(&p.path), p.capture_time_ms) else {
            continue;
        };
        let hits: Vec<&&ImageInfo> = by_name
            .get(&name)
            .into_iter()
            .flatten()
            .filter(|i| i.meta.capture_time_ms == Some(t))
            .collect();
        if let [i] = hits.as_slice() {
            claim(&mut m, &mut taken, p, i.id, By::Name);
        } else if hits.len() > 1 {
            m.ambiguous(p);
        }
    }
    Ok(m)
}
