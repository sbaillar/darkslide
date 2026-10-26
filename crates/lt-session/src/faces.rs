//! Faces and people tags in a session.
//!
//! The app detects faces on each original's embedded preview and embeds an
//! aligned crop of each (Apple Vision); the boxes and vectors are stored here
//! (`faces`, f16 vectors), keyed by content identity like the search
//! embeddings, with `face_scans` recording which originals were looked at (so a
//! photo without faces isn't scanned again). Like every index table these are
//! derived and rebuildable.
//!
//! **Who** is on a photo is the user's answer and lives in the sidecars
//! ([`PersonTag`] in `people`, "not this person" in `people_rejected`), on
//! every variant of the original, mirrored into `variant_people`. A tag's
//! region identifies its face (intersection over union ≥ ½). Same/different
//! person links between faces that aren't named yet (an unnamed merge or
//! split) are kept in `face_links` and mirrored to
//! `.lt/settings/face_links.json`, so a lost or recreated index gets them back
//! on the next open.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lt_core::geom::NormRect;
use lt_core::{FileHash, embedding};
use rusqlite::{OptionalExtension, params};

use crate::atomic::{read_json_file, write_atomic};
use crate::embed::KEY_SQL;
use crate::error::{IoContext, Result};
use crate::relpath::Folder;
use crate::session::{ImageId, SETTINGS_DIR, Session, VariantId};
use crate::sidecar::PersonTag;

/// A face the app found, to store.
#[derive(Debug, Clone, PartialEq)]
pub struct NewFace {
    /// Normalized to the upright photo, origin top left.
    pub rect: NormRect,
    /// 0–1, higher is better (Vision's face capture quality).
    pub quality: f32,
    pub vector: Vec<f32>,
}

/// An original whose faces haven't been looked for with this model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaceJob {
    pub key: FileHash,
    pub path: PathBuf,
}

/// One face: which photo content, which of its faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FaceRef {
    pub key: FileHash,
    pub idx: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    pub at: FaceRef,
    pub rect: NormRect,
    pub quality: f32,
    /// Unit length (from f16).
    pub vector: Vec<f32>,
}

/// How far face scanning got (present originals outside the Trash).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FaceCounts {
    pub scanned: usize,
    pub total: usize,
    /// Not hashed yet: scannable once identified.
    pub waiting: usize,
    pub faces: usize,
}

/// An original's people: its tags and rejections (over all its variants).
#[derive(Debug, Clone, PartialEq)]
pub struct PhotoPeople {
    pub image: ImageId,
    pub key: Option<FileHash>,
    pub variants: Vec<VariantId>,
    pub tags: Vec<PersonTag>,
    pub rejected: Vec<PersonTag>,
}

const FACE_LINKS_FILE: &str = "face_links.json";

/// `[a key (hex), a index, b key, b index, must]` per link.
type LinkRow = (String, u32, String, u32, bool);

#[derive(serde::Serialize, serde::Deserialize)]
struct FaceLinksFile {
    schema_version: u32,
    links: Vec<LinkRow>,
}

fn face_links_path(root: &Path) -> PathBuf {
    root.join(SETTINGS_DIR).join(FACE_LINKS_FILE)
}

/// Write every link of the index to `face_links.json` (inside the caller's
/// write transaction, so the file is never behind the index).
fn save_face_links(conn: &rusqlite::Connection, root: &Path) -> Result<()> {
    let mut st = conn.prepare_cached("SELECT a_key, a_idx, b_key, b_idx, must FROM face_links")?;
    let links = st
        .query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, u32>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?
        .filter_map(|row| {
            let (ak, ai, bk, bi, must) = row.ok()?;
            Some((key_of(ak)?.to_hex(), ai, key_of(bk)?.to_hex(), bi, must))
        })
        .collect();
    let bytes = serde_json::to_vec(&FaceLinksFile {
        schema_version: 1,
        links,
    })
    .expect("links serialize");
    let path = face_links_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).at(dir)?;
    }
    write_atomic(&path, &bytes).at(&path)
}

/// Bring the links back into a recreated index from `face_links.json`; with
/// no file yet (a session from before it existed) the index's links are
/// written out instead. A file that cannot be read is left alone and reported.
pub(crate) fn sync_face_links(conn: &rusqlite::Connection, root: &Path) -> Result<Option<String>> {
    let path = face_links_path(root);
    let bytes = match read_json_file(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let any: bool =
                conn.query_row("SELECT EXISTS(SELECT 1 FROM face_links)", [], |r| r.get(0))?;
            if any {
                save_face_links(conn, root)?;
            }
            return Ok(None);
        }
        Err(e) => return Ok(Some(format!("{}: {e}", path.display()))),
    };
    let file: FaceLinksFile = match serde_json::from_slice(&bytes) {
        Ok(f) => f,
        Err(e) => return Ok(Some(format!("{}: {e}", path.display()))),
    };
    let mut ins = conn.prepare_cached(
        "INSERT OR IGNORE INTO face_links(a_key, a_idx, b_key, b_idx, must) VALUES (?, ?, ?, ?, ?)",
    )?;
    for (ak, ai, bk, bi, must) in file.links {
        if let (Some(a), Some(b)) = (FileHash::from_hex(&ak), FileHash::from_hex(&bk)) {
            ins.execute(params![&a.0[..], ai, &b.0[..], bi, must])?;
        }
    }
    Ok(None)
}

fn key_of(bytes: Vec<u8>) -> Option<FileHash> {
    <[u8; 32]>::try_from(bytes).ok().map(FileHash)
}

/// `list` with `tag` added unless an equal one (person and region) is there.
fn add_unique(list: &mut Vec<PersonTag>, tag: &PersonTag) -> bool {
    if list
        .iter()
        .any(|t| t.same_person(tag) && t.same_region(tag.region))
    {
        return false;
    }
    list.push(tag.clone());
    true
}

impl Session {
    /// Present originals outside the Trash, hashed, never scanned with
    /// `model`: one per content, in path order.
    pub fn face_jobs(&self, model: &str) -> Result<Vec<FaceJob>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT i.rel_path, {KEY_SQL} AS k FROM images i
             WHERE i.missing = 0 AND i.folder <> ?1 AND k IS NOT NULL
             AND NOT EXISTS (SELECT 1 FROM face_scans f WHERE f.key = k AND f.model = ?2)
             ORDER BY i.rel_path"
        ))?;
        let rows: Vec<(String, Vec<u8>)> = st
            .query_map(params![Folder::Trash.code(), model], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut seen = HashSet::new();
        Ok(rows
            .into_iter()
            .filter_map(|(rel, k)| {
                let key = key_of(k)?;
                seen.insert(key).then(|| FaceJob {
                    key,
                    path: self.original_path(&rel),
                })
            })
            .collect())
    }

    pub fn face_counts(&self, model: &str) -> Result<FaceCounts> {
        let (all, total, scanned, faces): (i64, i64, i64, i64) = self.conn.query_row(
            &format!(
                "SELECT count(*), count(x.k), count(s.key),
                    coalesce(sum((SELECT count(*) FROM faces f WHERE f.key = x.k AND f.model = ?2)), 0)
                 FROM (SELECT {KEY_SQL} AS k FROM images i WHERE i.missing = 0 AND i.folder <> ?1) x
                 LEFT JOIN face_scans s ON s.key = x.k AND s.model = ?2"
            ),
            params![Folder::Trash.code(), model],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        Ok(FaceCounts {
            scanned: scanned as usize,
            total: total as usize,
            waiting: (all - total) as usize,
            faces: faces as usize,
        })
    }

    /// Replace the faces found on `key` with `faces` (in order: their
    /// indices) and mark it scanned. Faces whose vector can't be normalized
    /// are dropped. Returns how many were stored.
    pub fn store_faces(&mut self, model: &str, key: &FileHash, faces: &[NewFace]) -> Result<usize> {
        let tx = self.write_lock()?;
        tx.execute(
            "DELETE FROM faces WHERE key = ? AND model = ?",
            params![&key.0[..], model],
        )?;
        let mut n = 0u32;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO faces(key, model, idx, x, y, w, h, quality, vec) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?;
            for f in faces {
                if let Some(unit) = embedding::normalized(&f.vector) {
                    let r = f.rect;
                    st.execute(params![
                        &key.0[..],
                        model,
                        n,
                        r.x,
                        r.y,
                        r.w,
                        r.h,
                        f.quality,
                        embedding::encode_f16(&unit)
                    ])?;
                    n += 1;
                }
            }
        }
        tx.execute(
            "INSERT OR IGNORE INTO face_scans(key, model) VALUES (?, ?)",
            params![&key.0[..], model],
        )?;
        tx.commit()?;
        Ok(n as usize)
    }

    /// Faces of the present originals outside the Trash, by key then index.
    pub fn faces(&self, model: &str) -> Result<Vec<Face>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT f.key, f.idx, f.x, f.y, f.w, f.h, f.quality, f.vec FROM faces f
             WHERE f.model = ?2 AND f.key IN (SELECT {KEY_SQL} FROM images i
                WHERE i.missing = 0 AND i.folder <> ?1)
             ORDER BY f.key, f.idx"
        ))?;
        let rows = st.query_map(params![Folder::Trash.code(), model], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, u32>(1)?,
                NormRect {
                    x: r.get(2)?,
                    y: r.get(3)?,
                    w: r.get(4)?,
                    h: r.get(5)?,
                },
                r.get::<_, f32>(6)?,
                r.get::<_, Vec<u8>>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (k, idx, rect, quality, v) = row?;
            if let (Some(key), Some(vector)) = (key_of(k), embedding::decode_f16(&v)) {
                out.push(Face {
                    at: FaceRef { key, idx },
                    rect,
                    quality,
                    vector,
                });
            }
        }
        Ok(out)
    }

    /// Every present original outside the Trash with its people tags and
    /// rejections (the union over its variants), in path order.
    pub fn photo_people(&self) -> Result<Vec<PhotoPeople>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT i.id, {KEY_SQL} AS k, v.id FROM images i JOIN variants v ON v.image_id = i.id
             WHERE i.missing = 0 AND i.folder <> ?1 ORDER BY i.rel_path, v.ordinal"
        ))?;
        let rows: Vec<(i64, Option<Vec<u8>>, i64)> = st
            .query_map([Folder::Trash.code()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut tags: HashMap<i64, Vec<(PersonTag, bool)>> = HashMap::new();
        let mut st = self.conn.prepare_cached(
            "SELECT variant_id, name, person_id, x, y, w, h, rejected FROM variant_people
             ORDER BY variant_id, ord",
        )?;
        let people = st.query_map([], |r| {
            let region = match (r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?) {
                (Some(x), Some(y), Some(w), Some(h)) => Some(NormRect { x, y, w, h }),
                _ => None,
            };
            Ok((
                r.get::<_, i64>(0)?,
                PersonTag {
                    name: r.get(1)?,
                    id: r.get(2)?,
                    region,
                },
                r.get::<_, bool>(7)?,
            ))
        })?;
        for p in people {
            let (v, tag, rejected) = p?;
            tags.entry(v).or_default().push((tag, rejected));
        }
        let mut out: Vec<PhotoPeople> = Vec::new();
        for (image, key, variant) in rows {
            if out.last().is_none_or(|p| p.image.0 != image) {
                out.push(PhotoPeople {
                    image: ImageId(image),
                    key: key.and_then(key_of),
                    variants: Vec::new(),
                    tags: Vec::new(),
                    rejected: Vec::new(),
                });
            }
            let p = out.last_mut().expect("pushed above");
            p.variants.push(VariantId(variant));
            for (tag, rejected) in tags.remove(&variant).unwrap_or_default() {
                add_unique(
                    if rejected {
                        &mut p.rejected
                    } else {
                        &mut p.tags
                    },
                    &tag,
                );
            }
        }
        Ok(out)
    }

    /// Tag `tag`'s person on each image (every variant), at the given face
    /// (`None`: on the photo). An earlier rejection of that person at that
    /// face is dropped; a tag of another person at the same face is replaced.
    /// Not an undo step (People has its own Remove).
    pub fn tag_person(
        &mut self,
        images: &[(ImageId, Option<NormRect>)],
        tag: &PersonTag,
    ) -> Result<()> {
        self.edit_people(images, |sc, region| {
            let t = PersonTag {
                region,
                ..tag.clone()
            };
            let before = (sc.people.len(), sc.people_rejected.len());
            sc.people_rejected
                .retain(|r| !(r.same_person(&t) && r.same_region(region)));
            // One person per face.
            if region.is_some() {
                sc.people
                    .retain(|p| p.same_person(&t) || !p.same_region(region));
            }
            add_unique(&mut sc.people, &t) || before != (sc.people.len(), sc.people_rejected.len())
        })
    }

    /// Remove `person`'s tags at the given face (`None`: wherever on the photo).
    pub fn untag_person(
        &mut self,
        images: &[(ImageId, Option<NormRect>)],
        person: &PersonTag,
    ) -> Result<()> {
        self.edit_people(images, |sc, region| {
            let before = sc.people.len();
            sc.people.retain(|p| {
                !(p.same_person(person) && (region.is_none() || p.same_region(region)))
            });
            sc.people.len() != before
        })
    }

    /// "Not this person": remove any such tag at that face and remember the answer.
    pub fn reject_person(
        &mut self,
        images: &[(ImageId, Option<NormRect>)],
        person: &PersonTag,
    ) -> Result<()> {
        self.edit_people(images, |sc, region| {
            let t = PersonTag {
                region,
                ..person.clone()
            };
            let before = sc.people.len();
            sc.people
                .retain(|p| !(p.same_person(&t) && p.same_region(region)));
            add_unique(&mut sc.people_rejected, &t) || sc.people.len() != before
        })
    }

    /// Give the person with store id `id` a new name on this session's photos.
    /// Returns how many sidecars changed.
    pub fn rename_person(&mut self, id: &str, name: &str) -> Result<usize> {
        let vs: Vec<VariantId> = {
            let mut st = self.conn.prepare_cached(
                "SELECT DISTINCT variant_id FROM variant_people WHERE person_id = ? AND name <> ?",
            )?;
            st.query_map(params![id, name], |r| r.get(0).map(VariantId))?
                .collect::<rusqlite::Result<_>>()?
        };
        self.modify(&vs, |_, sc| {
            let mut changed = false;
            for t in sc.people.iter_mut().chain(sc.people_rejected.iter_mut()) {
                if t.id.as_deref() == Some(id) && t.name != name {
                    name.clone_into(&mut t.name);
                    changed = true;
                }
            }
            changed
        })?;
        Ok(vs.len())
    }

    /// Apply `f(sidecar, region)` to every variant of each image.
    fn edit_people(
        &mut self,
        images: &[(ImageId, Option<NormRect>)],
        mut f: impl FnMut(&mut crate::sidecar::Sidecar, Option<NormRect>) -> bool,
    ) -> Result<()> {
        // An image may be listed with several faces: apply them in order.
        let mut by_variant: Vec<(VariantId, Vec<Option<NormRect>>)> = Vec::new();
        for (image, region) in images {
            for v in self.variants(*image)? {
                match by_variant.iter_mut().find(|(x, _)| *x == v) {
                    Some((_, regions)) => regions.push(*region),
                    None => by_variant.push((v, vec![*region])),
                }
            }
        }
        let regions: HashMap<VariantId, Vec<Option<NormRect>>> =
            by_variant.iter().cloned().collect();
        let vs: Vec<VariantId> = by_variant.into_iter().map(|(v, _)| v).collect();
        self.modify(&vs, |v, sc| {
            let mut changed = false;
            for r in regions.get(&v).into_iter().flatten() {
                changed |= f(sc, *r);
            }
            changed
        })
    }

    /// Same-person (`must`) or different-person links between unnamed faces.
    pub fn face_links(&self) -> Result<Vec<(FaceRef, FaceRef, bool)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT a_key, a_idx, b_key, b_idx, must FROM face_links")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, u32>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (ak, ai, bk, bi, must) = row?;
            if let (Some(a), Some(b)) = (key_of(ak), key_of(bk)) {
                out.push((
                    FaceRef { key: a, idx: ai },
                    FaceRef { key: b, idx: bi },
                    must,
                ));
            }
        }
        Ok(out)
    }

    /// Link each pair (replacing an earlier link between them).
    pub fn link_faces(&mut self, pairs: &[(FaceRef, FaceRef)], must: bool) -> Result<()> {
        let tx = self.write_lock()?;
        {
            let mut del = tx.prepare_cached(
                "DELETE FROM face_links WHERE a_key = ? AND a_idx = ? AND b_key = ? AND b_idx = ?",
            )?;
            let mut ins = tx.prepare_cached(
                "INSERT INTO face_links(a_key, a_idx, b_key, b_idx, must) VALUES (?, ?, ?, ?, ?)",
            )?;
            for &(a, b) in pairs.iter().filter(|(a, b)| a != b) {
                let (a, b) = (a.min(b), a.max(b));
                del.execute(params![&a.key.0[..], a.idx, &b.key.0[..], b.idx])?;
                ins.execute(params![&a.key.0[..], a.idx, &b.key.0[..], b.idx, must])?;
            }
        }
        save_face_links(&tx, &self.root)?;
        tx.commit()?;
        Ok(())
    }

    /// Forget every link of these faces.
    pub fn unlink_faces(&mut self, faces: &[FaceRef]) -> Result<()> {
        let tx = self.write_lock()?;
        {
            let mut st = tx.prepare_cached(
                "DELETE FROM face_links WHERE (a_key = ?1 AND a_idx = ?2) OR (b_key = ?1 AND b_idx = ?2)",
            )?;
            for f in faces {
                st.execute(params![&f.key.0[..], f.idx])?;
            }
        }
        save_face_links(&tx, &self.root)?;
        tx.commit()?;
        Ok(())
    }
}

/// Where a photo was taken, as its metadata says: location names (XMP/IPTC
/// sublocation, city, state, country) and the GPS position.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PhotoPlace {
    pub names: Vec<String>,
    /// Decimal degrees (south and west negative).
    pub gps: Option<(f64, f64)>,
    /// What it was read from (the XMP sidecars' sizes and times): read again
    /// when that changes.
    pub stamp: String,
}

impl Session {
    /// Present originals outside the Trash, hashed, whose place hasn't been
    /// read yet: one per content, in path order.
    pub fn location_jobs(&self) -> Result<Vec<FaceJob>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT i.rel_path, {KEY_SQL} AS k FROM images i
             WHERE i.missing = 0 AND i.folder <> ?1 AND k IS NOT NULL
             AND NOT EXISTS (SELECT 1 FROM locations l WHERE l.key = k)
             ORDER BY i.rel_path"
        ))?;
        let rows: Vec<(String, Vec<u8>)> = st
            .query_map([Folder::Trash.code()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut seen = HashSet::new();
        Ok(rows
            .into_iter()
            .filter_map(|(rel, k)| {
                let key = key_of(k)?;
                seen.insert(key).then(|| FaceJob {
                    key,
                    path: self.original_path(&rel),
                })
            })
            .collect())
    }

    /// Record what was read (an empty place too: it isn't read again).
    pub fn store_locations(&mut self, places: &[(FileHash, PhotoPlace)]) -> Result<()> {
        let tx = self.write_lock()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT OR REPLACE INTO locations(key, names, lat, lon, stamp) VALUES (?, ?, ?, ?, ?)",
            )?;
            for (key, p) in places {
                let names: Vec<String> = p.names.iter().map(|n| n.replace('\n', " ")).collect();
                st.execute(params![
                    &key.0[..],
                    names.join("\n"),
                    p.gps.map(|g| g.0),
                    p.gps.map(|g| g.1),
                    p.stamp
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The place of the photo with content `key`, if it was read.
    pub fn location(&self, key: &FileHash) -> Result<Option<PhotoPlace>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT names, lat, lon, stamp FROM locations WHERE key = ?")?;
        let row: Option<(String, Option<f64>, Option<f64>, String)> = st
            .query_row([&key.0[..]], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        Ok(row.map(|(names, lat, lon, stamp)| PhotoPlace {
            names: names
                .split('\n')
                .filter(|n| !n.is_empty())
                .map(str::to_owned)
                .collect(),
            gps: lat.zip(lon),
            stamp,
        }))
    }

    /// Present originals outside the Trash whose place was read, with the
    /// stamp it was read under (to notice a new or changed XMP sidecar).
    pub fn location_stamps(&self) -> Result<Vec<(FaceJob, String)>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT i.rel_path, {KEY_SQL} AS k, l.stamp FROM images i JOIN locations l ON l.key = k
             WHERE i.missing = 0 AND i.folder <> ?1 ORDER BY i.rel_path"
        ))?;
        let rows: Vec<(String, Vec<u8>, String)> = st
            .query_map([Folder::Trash.code()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(rel, k, stamp)| {
                Some((
                    FaceJob {
                        key: key_of(k)?,
                        path: self.original_path(&rel),
                    },
                    stamp,
                ))
            })
            .collect())
    }
}
