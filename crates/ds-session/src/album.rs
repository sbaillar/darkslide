//! Manual and smart albums, plus filtered queries.
//!
//! Albums live in `.ds/settings/albums.json` (source of truth) and manual
//! membership is mirrored into the index for SQL filtering.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::atomic::{read_json_file, write_atomic};
use crate::error::{IoContext, Result, SessionError, json_err};
use ds_core::{ColorTag, PickState, Rating};

use crate::filter::{Filter, Sort, tag_from_code};
use crate::session::{SETTINGS_DIR, Session, VariantId, random_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AlbumId(pub i64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AlbumKind {
    /// Hand-picked variants, in the user's order.
    Manual { members: Vec<VariantId> },
    /// A saved filter, evaluated live.
    Smart { filter: Filter, sort: Sort },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Album {
    pub id: AlbumId,
    pub name: String,
    #[serde(flatten)]
    pub kind: AlbumKind,
    /// Stable key of a built-in smart folder (`"rating:5"`, `"unrated"`,
    /// `"picked"`, `"rejected"`, `"tag:red"`); `None` for user albums.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<String>,
    /// Mirror membership into a Finder folder of symlinks (the app does the
    /// mirroring; ds-session only stores the choice).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mirror: bool,
}

/// The built-in smart folders every session has: exact ratings 5…1,
/// unrated, picked, rejected, plus one per color tag in use.
pub fn builtin_smart_folders(tags_in_use: &[ColorTag]) -> Vec<(String, String, Filter)> {
    let mut out = Vec::new();
    for n in (1..=5u8).rev() {
        let r = Rating::new(n);
        let name = if n == 1 {
            "1 star".to_owned()
        } else {
            format!("{n} stars")
        };
        out.push((
            format!("rating:{n}"),
            name,
            Filter {
                min_rating: r,
                max_rating: r,
                ..Filter::default()
            },
        ));
    }
    out.push((
        "unrated".into(),
        "Unrated".into(),
        Filter {
            max_rating: Rating::new(0),
            ..Filter::default()
        },
    ));
    for (key, name, pick) in [
        ("picked", "Picked", PickState::Picked),
        ("rejected", "Rejected", PickState::Rejected),
    ] {
        out.push((
            key.into(),
            name.into(),
            Filter {
                pick: Some(pick),
                ..Filter::default()
            },
        ));
    }
    for &t in tags_in_use {
        if t == ColorTag::None {
            continue;
        }
        let name = format!("{t:?}");
        out.push((
            format!("tag:{}", name.to_lowercase()),
            name,
            Filter {
                tags: vec![t],
                ..Filter::default()
            },
        ));
    }
    out
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AlbumsFile {
    #[serde(default)]
    albums: Vec<Album>,
}

const ALBUMS_FILE: &str = "albums.json";

impl Session {
    /// Variants matching `filter`, sorted.
    pub fn query(&self, filter: &Filter, sort: Sort) -> Result<Vec<VariantId>> {
        self.run_query(filter, sort, None)
    }

    fn run_query(
        &self,
        filter: &Filter,
        sort: Sort,
        album: Option<AlbumId>,
    ) -> Result<Vec<VariantId>> {
        let (sql, params) = filter.to_sql(sort, album.map(|a| a.0));
        let mut st = self.conn.prepare_cached(&sql)?;
        Ok(st
            .query_map(params_from_iter(params), |r| r.get(0).map(VariantId))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn albums(&self) -> &[Album] {
        &self.albums
    }

    pub fn album(&self, id: AlbumId) -> Result<&Album> {
        self.albums
            .iter()
            .find(|a| a.id == id)
            .ok_or(SessionError::NotFound {
                kind: "album",
                id: id.0,
            })
    }

    /// The album's variants: a manual album in its stored order (dropping ids
    /// that no longer exist), a smart album by evaluating its filter.
    pub fn album_variants(&self, id: AlbumId) -> Result<Vec<VariantId>> {
        match &self.album(id)?.kind {
            AlbumKind::Smart { filter, sort } => self.query(filter, *sort),
            AlbumKind::Manual { .. } => {
                let mut st = self.conn.prepare_cached(
                    "SELECT m.variant_id FROM album_members m JOIN variants v ON v.id = m.variant_id
                     WHERE m.album_id = ? ORDER BY m.position",
                )?;
                Ok(st
                    .query_map([id.0], |r| r.get(0).map(VariantId))?
                    .collect::<rusqlite::Result<_>>()?)
            }
        }
    }

    /// Filter within an album (a smart album's own filter is ignored; pass it
    /// merged into `filter` if wanted).
    pub fn query_album(&self, id: AlbumId, filter: &Filter, sort: Sort) -> Result<Vec<VariantId>> {
        match &self.album(id)?.kind {
            AlbumKind::Manual { .. } => self.run_query(filter, sort, Some(id)),
            AlbumKind::Smart { .. } => self.query(filter, sort),
        }
    }

    pub fn create_album(&mut self, name: &str) -> Result<AlbumId> {
        self.push_album(
            name,
            AlbumKind::Manual {
                members: Vec::new(),
            },
        )
    }

    pub fn create_smart_album(
        &mut self,
        name: &str,
        filter: Filter,
        sort: Sort,
    ) -> Result<AlbumId> {
        self.push_album(name, AlbumKind::Smart { filter, sort })
    }

    fn push_album(&mut self, name: &str, kind: AlbumKind) -> Result<AlbumId> {
        let name = valid_name(name)?;
        self.update_albums(|albums| {
            // Random, so two handles creating albums at once never collide.
            let id = loop {
                let id = AlbumId(random_id());
                if albums.iter().all(|a| a.id != id) {
                    break id;
                }
            };
            albums.push(Album {
                id,
                name,
                kind,
                builtin: None,
                mirror: false,
            });
            Ok(id)
        })
    }

    /// Color tags used by any variant outside the Trash.
    pub fn tags_in_use(&self) -> Result<Vec<ColorTag>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT DISTINCT v.color_tag FROM variants v JOIN images i ON i.id = v.image_id
             WHERE v.color_tag <> 0 AND i.folder <> {} ORDER BY v.color_tag",
            crate::relpath::Folder::Trash.code()
        ))?;
        Ok(st
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(tag_from_code)
            .collect())
    }

    /// Create any missing built-in smart folders (see [`builtin_smart_folders`]):
    /// run when a session is opened, and again after tag changes. Built-ins the
    /// user deleted through another handle come back. Returns how many were added.
    pub fn ensure_builtin_albums(&mut self) -> Result<usize> {
        let wanted = builtin_smart_folders(&self.tags_in_use()?);
        self.update_albums(|albums| {
            let mut added = 0;
            for (key, name, filter) in wanted {
                if albums.iter().any(|a| a.builtin.as_deref() == Some(&key)) {
                    continue;
                }
                let id = loop {
                    let id = AlbumId(random_id());
                    if albums.iter().all(|a| a.id != id) {
                        break id;
                    }
                };
                albums.push(Album {
                    id,
                    name,
                    kind: AlbumKind::Smart {
                        filter,
                        sort: Sort::default(),
                    },
                    builtin: Some(key),
                    mirror: false,
                });
                added += 1;
            }
            Ok(added)
        })
    }

    /// Replace a user smart album's name and filter (built-ins can't be redefined).
    pub fn update_smart_album(&mut self, id: AlbumId, name: &str, filter: Filter) -> Result<()> {
        let name = valid_name(name)?;
        self.update_albums(|albums| {
            let a = find(albums, id)?;
            if a.builtin.is_some() {
                return Err(SessionError::BuiltinAlbum(id.0));
            }
            match &mut a.kind {
                AlbumKind::Smart { filter: f, .. } => *f = filter,
                AlbumKind::Manual { .. } => return Err(SessionError::InvalidName(name)),
            }
            a.name = name;
            Ok(())
        })
    }

    /// Turn "Mirror to Finder" on or off for an album.
    pub fn set_album_mirror(&mut self, id: AlbumId, on: bool) -> Result<()> {
        self.update_albums(|albums| {
            find(albums, id)?.mirror = on;
            Ok(())
        })
    }

    pub fn rename_album(&mut self, id: AlbumId, name: &str) -> Result<()> {
        let name = valid_name(name)?;
        self.update_albums(|albums| {
            find(albums, id)?.name = name;
            Ok(())
        })
    }

    pub fn delete_album(&mut self, id: AlbumId) -> Result<()> {
        self.update_albums(|albums| {
            if find(albums, id)?.builtin.is_some() {
                return Err(SessionError::BuiltinAlbum(id.0));
            }
            albums.retain(|a| a.id != id);
            Ok(())
        })
    }

    /// Append to a manual album (already-present variants keep their place).
    pub fn add_to_album(&mut self, id: AlbumId, vs: &[VariantId]) -> Result<()> {
        for &v in vs {
            self.locate(v)?;
        }
        self.edit_members(id, |m| {
            let mut present: HashSet<VariantId> = m.iter().copied().collect();
            for v in vs {
                if present.insert(*v) {
                    m.push(*v);
                }
            }
        })
    }

    pub fn remove_from_album(&mut self, id: AlbumId, vs: &[VariantId]) -> Result<()> {
        let drop: HashSet<VariantId> = vs.iter().copied().collect();
        self.edit_members(id, |m| m.retain(|v| !drop.contains(v)))
    }

    fn edit_members(&mut self, id: AlbumId, f: impl FnOnce(&mut Vec<VariantId>)) -> Result<()> {
        self.update_albums(|albums| match &mut find(albums, id)?.kind {
            AlbumKind::Manual { members } => {
                f(members);
                Ok(())
            }
            AlbumKind::Smart { .. } => Err(SessionError::SmartAlbum(id.0)),
        })
    }

    /// Where a variant set aside by `delete_variant` sits in manual albums:
    /// `Trash/.ds/deleted/<id>.albums.json` beside its sidecar, so
    /// `restore_variants` puts it back in the same albums at the same places.
    pub(crate) fn album_memberships_path(&self, v: VariantId) -> PathBuf {
        self.root
            .join(crate::session::DELETED_DIR)
            .join(format!("{}.albums.json", v.0))
    }

    /// Record `v`'s manual album memberships (album id, position) before it is
    /// set aside; nothing is written when it is in none.
    pub(crate) fn record_album_memberships(&self, v: VariantId) -> Result<()> {
        let pos: Vec<(i64, usize)> = read_albums(&self.root)?
            .iter()
            .filter_map(|a| match &a.kind {
                AlbumKind::Manual { members } => {
                    members.iter().position(|m| *m == v).map(|p| (a.id.0, p))
                }
                AlbumKind::Smart { .. } => None,
            })
            .collect();
        if pos.is_empty() {
            return Ok(());
        }
        let path = self.album_memberships_path(v);
        let bytes = serde_json::to_vec(&pos).map_err(|e| json_err(&path, &e))?;
        write_atomic(&path, &bytes).at(&path)
    }

    /// Put restored variants back in the manual albums they were in (albums
    /// deleted meanwhile are skipped), then drop the records.
    pub(crate) fn restore_album_memberships(&mut self, vs: &[VariantId]) -> Result<()> {
        let mut back: Vec<(VariantId, Vec<(i64, usize)>)> = Vec::new();
        for &v in vs {
            let path = self.album_memberships_path(v);
            if let Ok(b) = read_json_file(&path) {
                back.push((
                    v,
                    serde_json::from_slice(&b).map_err(|e| json_err(&path, &e))?,
                ));
            }
        }
        if back.is_empty() {
            return Ok(());
        }
        self.update_albums(|albums| {
            for (v, pos) in &back {
                for (id, p) in pos {
                    if let Some(AlbumKind::Manual { members }) = albums
                        .iter_mut()
                        .find(|a| a.id.0 == *id)
                        .map(|a| &mut a.kind)
                        && !members.contains(v)
                    {
                        members.insert((*p).min(members.len()), *v);
                    }
                }
            }
            Ok(())
        })?;
        for (v, _) in back {
            let _ = fs::remove_file(self.album_memberships_path(v));
        }
        Ok(())
    }

    pub(crate) fn drop_from_albums(&mut self, vs: &[VariantId]) -> Result<()> {
        let gone: HashSet<VariantId> = vs.iter().copied().collect();
        self.update_albums(|albums| {
            for a in albums {
                if let AlbumKind::Manual { members } = &mut a.kind {
                    members.retain(|m| !gone.contains(m));
                }
            }
            Ok(())
        })
    }

    /// Read-modify-write of `albums.json` under the index write lock, starting
    /// from the file (not our possibly stale copy), so edits made through
    /// another handle are never lost.
    fn update_albums<T>(&mut self, f: impl FnOnce(&mut Vec<Album>) -> Result<T>) -> Result<T> {
        let tx = self.write_lock()?;
        let mut albums = read_albums(&self.root)?;
        let before = albums.clone();
        let out = f(&mut albums)?;
        if albums != before {
            let path = self.root.join(SETTINGS_DIR).join(ALBUMS_FILE);
            let file = AlbumsFile { albums };
            let bytes = serde_json::to_vec_pretty(&file).map_err(|e| json_err(&path, &e))?;
            write_atomic(&path, &bytes).at(&path)?;
            albums = file.albums;
        }
        mirror_albums(&tx, &albums)?;
        tx.commit()?;
        self.albums = albums;
        Ok(out)
    }
}

fn valid_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(SessionError::InvalidName(name.to_owned()));
    }
    Ok(name.to_owned())
}

fn find(albums: &mut [Album], id: AlbumId) -> Result<&mut Album> {
    albums
        .iter_mut()
        .find(|a| a.id == id)
        .ok_or(SessionError::NotFound {
            kind: "album",
            id: id.0,
        })
}

/// Albums as stored in `.ds/settings/albums.json` (empty if absent).
pub(crate) fn read_albums(root: &Path) -> Result<Vec<Album>> {
    let path = root.join(SETTINGS_DIR).join(ALBUMS_FILE);
    match read_json_file(&path) {
        Ok(b) => Ok(serde_json::from_slice::<AlbumsFile>(&b)
            .map_err(|e| json_err(&path, &e))?
            .albums),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).at(&path),
    }
}

/// [`read_albums`] for opening a session: an `albums.json` that cannot be
/// read (truncated, hand-edited, written by a newer build, too large) is moved
/// aside to `albums.json.corrupt-<seconds>` instead of locking the user out of
/// every photo. Returns the albums (none then) and what happened, if anything.
pub(crate) fn read_albums_or_quarantine(root: &Path) -> Result<(Vec<Album>, Option<String>)> {
    let err = match read_albums(root) {
        Ok(a) => return Ok((a, None)),
        Err(e) => e,
    };
    let unreadable = match &err {
        SessionError::Json { .. } => true,
        SessionError::Io { source, .. } => source.kind() == std::io::ErrorKind::InvalidData,
        _ => false,
    };
    if !unreadable {
        return Err(err);
    }
    let path = root.join(SETTINGS_DIR).join(ALBUMS_FILE);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut aside = path.with_file_name(format!("{ALBUMS_FILE}.corrupt-{stamp}"));
    let mut n = 1;
    while aside.symlink_metadata().is_ok() {
        aside = path.with_file_name(format!("{ALBUMS_FILE}.corrupt-{stamp}-{n}"));
        n += 1;
    }
    ds_fsutil::rename_noreplace(&path, &aside).at(&path)?;
    Ok((
        Vec::new(),
        Some(format!("{err}; moved aside to {}", aside.display())),
    ))
}

/// Mirror manual album membership into the index (caller holds the write lock).
pub(crate) fn mirror_albums(conn: &Connection, albums: &[Album]) -> Result<()> {
    conn.execute("DELETE FROM album_members", [])?;
    let mut st = conn.prepare_cached(
        "INSERT OR IGNORE INTO album_members(album_id, variant_id, position) VALUES (?,?,?)",
    )?;
    for a in albums {
        if let AlbumKind::Manual { members } = &a.kind {
            for (pos, v) in members.iter().enumerate() {
                st.execute([a.id.0, v.0, pos as i64])?;
            }
        }
    }
    Ok(())
}
