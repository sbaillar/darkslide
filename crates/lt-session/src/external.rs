//! Sessions over the user's own folder ([`crate::Originals::External`]).
//!
//! The originals are the user's files; the session only ever writes sidecars
//! and its index (under the session root, which may be on another volume).
//!
//! Identity (see [`crate::identity`]) is tracked by `(size, mtime, inode)`
//! and content hashes, in two steps so opening a folder never waits for
//! hashing:
//!
//! 1. [`Session::sync_external`] (on open, stat only): an unchanged stat is
//!    the same file; a renamed file (same size, mtime and inode at a new
//!    path) is relinked at once; a file whose stat changed is kept
//!    *provisionally* and listed in [`ExternalSync::verify`]; new files that
//!    could be a missing photo moved behind the session's back are *held*
//!    ([`ExternalSync::held`]) instead of registered.
//! 2. In the background the caller hashes those files and reports back:
//!    [`Session::accept_verified`] (same bytes, or the same sensor data with
//!    edited metadata: everything is kept), [`Session::replace_image`] (a
//!    different photo: the old metadata is set aside, restorable with
//!    [`Session::restore_replaced`]), [`Session::relink_found`] (held files
//!    matched to missing photos by content; ambiguous duplicates are reported,
//!    never guessed). A file that can't be read is simply looked at again on
//!    the next open: an I/O error never detaches anything.
//!
//! [`Session::forget_images`] drops images from the session (their sidecars
//! go to `Trash/.lt/deleted/`, recoverable); originals are never touched.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use lt_core::FileHash;
use rusqlite::{OptionalExtension, params};

use crate::error::{IoContext, Result, SessionError};
use crate::files::Moved;
use crate::identity::{Hashes, pair_by_name};
use crate::relpath::RelPath;
use crate::session::{
    DELETED_DIR, FileStat, ImageId, NewOriginal, Originals, Pending, Session, UNHASHED, VariantId,
    record_stat,
};
use crate::sidecar::ImageMetaLite;

/// `(size, mtime_ns, inode)` of an original, as [`original_stat`] returns it.
pub type Stat = (u64, i64, u64);

/// An original whose file is present but whose stat changed since it was
/// verified: kept provisionally until its content is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToVerify {
    pub image: ImageId,
    /// `Capture/<rel>`.
    pub rel: String,
    pub path: PathBuf,
    pub stat: Stat,
    /// Its stored whole-file hash ([`UNHASHED`] if never hashed).
    pub hash: FileHash,
    /// Its stored sensor-data hash, if known.
    pub content: Option<FileHash>,
    /// Only the cheap sample check is needed (FAT/exFAT volumes, where an
    /// unchanged stat does not prove the file is the same).
    pub sample_only: bool,
}

/// What [`Session::sync_external`] found (stat only; nothing was hashed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExternalSync {
    /// Originals renamed or moved inside the folder (same size, mtime and
    /// inode at a new path), followed at once.
    pub relinked: Vec<Moved>,
    /// Originals not found (still in the session, flagged missing). Hashed
    /// ones may still be found among [`Self::held`] files.
    pub missing: Vec<ImageId>,
    /// Present originals to check in the background.
    pub verify: Vec<ToVerify>,
    /// `Capture/<rel>` files not registered yet because they could be a
    /// missing photo (same size): hash them, then [`Session::relink_found`].
    pub held: Vec<String>,
    /// `Capture/<rel>` files in the folder the session doesn't track yet.
    pub untracked: Vec<String>,
    /// Photos seen in the folder (above [`crate::MAX_EXTERNAL_FILES`], the
    /// rest were not listed and nothing was marked missing).
    pub seen: usize,
}

/// A held file after hashing, for [`Session::relink_found`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub rel: String,
    pub stat: Stat,
    pub hashes: Hashes,
}

/// What [`Session::relink_found`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelinkReport {
    pub relinked: Vec<Moved>,
    /// Missing photos with several identical-content candidates (or the
    /// reverse) that names could not tell apart: left for the user.
    pub ambiguous: Vec<String>,
    /// Held files that are no missing photo: register them as new.
    pub unmatched: Vec<String>,
}

/// A verified original for [`Session::accept_verified`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub image: ImageId,
    /// The stat the hashes were computed under.
    pub stat: Stat,
    pub hashes: Hashes,
}

/// What [`Session::migrate_to_external`] did to an old hard-link session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Migrated {
    /// The session was converted (false: it already was external).
    pub converted: bool,
    /// Hard links removed (each verified to be the same inode as the original).
    pub links_removed: usize,
    /// Plain copies removed because the folder holds the identical file at
    /// the same path (a copied round-3 folder lost its hard links).
    pub duplicates_removed: usize,
    /// Files in the old `Capture/` that are the only copy (e.g. the original
    /// was deleted in Finder): left in place; see [`Session::restore_kept_copy`].
    pub kept: Vec<String>,
}

/// Missing photos by whole-file and sensor-data hash, so a found file is
/// matched in O(1) instead of scanning every missing photo.
struct HashIndex {
    by_file: HashMap<FileHash, usize>,
    by_content: HashMap<FileHash, usize>,
}

impl HashIndex {
    /// `(file hash, content hash)` per row; the earliest row wins a shared hash.
    fn new(rows: impl Iterator<Item = (FileHash, Option<FileHash>)>) -> Self {
        let mut index = Self {
            by_file: HashMap::new(),
            by_content: HashMap::new(),
        };
        for (i, (file, content)) in rows.enumerate() {
            index.by_file.entry(file).or_insert(i);
            if let Some(c) = content {
                index.by_content.entry(c).or_insert(i);
            }
        }
        index
    }

    /// The earliest row whose file hash is `file` or whose content hash is `content`.
    fn first(&self, file: FileHash, content: Option<FileHash>) -> Option<usize> {
        let by_file = self.by_file.get(&file);
        let by_content = content.and_then(|c| self.by_content.get(&c));
        by_file.into_iter().chain(by_content).min().copied()
    }
}

/// An image row as the external sync needs it.
struct Row {
    id: ImageId,
    rel: RelPath,
    hash: FileHash,
    content: Option<FileHash>,
    missing: bool,
    verified: Option<FileStat>,
}

fn stat_tuple(st: FileStat) -> Stat {
    (st.size, st.mtime, st.ino)
}

fn file_stat((size, mtime, ino): Stat) -> FileStat {
    FileStat { size, mtime, ino }
}

impl Session {
    fn external_rows(&self) -> Result<Vec<Row>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, rel_path, hash, content_hash, missing, file_size, file_mtime, file_ino
             FROM images",
        )?;
        let rows = st.query_map([], |r| {
            let rel: String = r.get(1)?;
            let hash: Vec<u8> = r.get(2)?;
            let size = r.get::<_, i64>(5)? as u64;
            let mtime: Option<i64> = r.get(6)?;
            let ino: Option<i64> = r.get(7)?;
            Ok(Row {
                id: ImageId(r.get(0)?),
                rel: RelPath::new(&rel).map_err(|_| rusqlite::Error::InvalidQuery)?,
                hash: FileHash(hash.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?),
                content: r
                    .get::<_, Option<Vec<u8>>>(3)?
                    .and_then(|b| b.try_into().ok())
                    .map(FileHash),
                missing: r.get(4)?,
                verified: mtime.zip(ino).map(|(mtime, ino)| FileStat {
                    size,
                    mtime,
                    ino: ino as u64,
                }),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// [`Self::sync_external_with`] for volumes whose stat can be trusted.
    pub fn sync_external(&mut self) -> Result<ExternalSync> {
        self.sync_external_with(false)
    }

    /// Re-check every original of an external session against the folder,
    /// by stat only (see the module docs). With `sample_check` (FAT/exFAT),
    /// hashed originals with an unchanged stat are listed for a sample check.
    pub fn sync_external_with(&mut self, sample_check: bool) -> Result<ExternalSync> {
        if !self.is_external() {
            return Err(SessionError::ExternalOnly("sync_external"));
        }
        self.sync_index()?;
        let disk = self.scan_disk(true)?;
        let rows = self.external_rows()?;
        let known: HashSet<&str> = rows.iter().map(|r| r.rel.as_str()).collect();
        let mut untracked: BTreeMap<String, FileStat> = disk
            .originals
            .iter()
            .filter(|(rel, _)| !known.contains(rel.as_str()))
            .map(|(r, s)| (r.clone(), *s))
            .collect();
        let mut report = ExternalSync {
            seen: disk.seen,
            ..ExternalSync::default()
        };
        let tx = self.write_lock()?;
        let mut gone = Vec::new();
        for row in &rows {
            let Some(&st) = disk.originals.get(row.rel.as_str()) else {
                // Under a folder the walk couldn't read: unknown, left as it was.
                if !disk.unknown(row.rel.as_str()) {
                    gone.push(row);
                }
                continue;
            };
            let unchanged = row.verified == Some(st);
            // Registered by stat, never hashed: the same size with the same
            // mtime (a copied folder, a card whose inodes changed on remount)
            // or the same inode (a `touch`) is the same file.
            let unhashed_same = row.hash == UNHASHED
                && row
                    .verified
                    .is_none_or(|v| v.size == st.size && (v.mtime == st.mtime || v.ino == st.ino));
            if unchanged || unhashed_same {
                if !unchanged || row.missing {
                    record_stat(&tx, row.id.0, st)?;
                }
                if unchanged && sample_check && row.hash != UNHASHED {
                    report.verify.push(self.to_verify(row, st, true));
                }
                continue;
            }
            if row.missing {
                tx.execute("UPDATE images SET missing = 0 WHERE id = ?", [row.id.0])?;
            }
            report.verify.push(self.to_verify(row, st, false));
        }
        // A rename keeps size, mtime and inode: follow it now.
        let mut by_stat: HashMap<FileStat, Vec<String>> = HashMap::new();
        for (rel, st) in &untracked {
            by_stat.entry(*st).or_default().push(rel.clone());
        }
        let mut still_missing = Vec::new();
        for row in gone {
            let candidates = row.verified.and_then(|v| by_stat.get(&v)).cloned();
            let pick = candidates.and_then(|c| {
                let names: Vec<&str> = c.iter().map(String::as_str).collect();
                match pair_by_name(&[row.rel.as_str()], &names).as_slice() {
                    [(_, j)] => Some(c[*j].clone()),
                    _ => None,
                }
            });
            let moved = match pick {
                Some(to) if untracked.contains_key(&to) => {
                    let st = untracked[&to];
                    self.relink_one(&tx, row.id, &row.rel, &to, st)?
                }
                _ => None,
            };
            if let Some(m) = moved {
                untracked.remove(m.to.as_str());
                report.relinked.push(m);
            } else {
                tx.execute("UPDATE images SET missing = 1 WHERE id = ?", [row.id.0])?;
                still_missing.push(row);
            }
        }
        tx.commit()?;
        // Files that could be a hashed missing photo are held until hashed.
        let wanted: HashSet<u64> = still_missing
            .iter()
            .filter(|r| r.hash != UNHASHED)
            .filter_map(|r| r.verified.map(|v| v.size))
            .collect();
        for (rel, st) in untracked {
            if wanted.contains(&st.size) {
                report.held.push(rel);
            } else {
                report.untracked.push(rel);
            }
        }
        report.missing = still_missing.iter().map(|r| r.id).collect();
        Ok(report)
    }

    fn to_verify(&self, row: &Row, st: FileStat, sample_only: bool) -> ToVerify {
        ToVerify {
            image: row.id,
            rel: row.rel.as_str().to_owned(),
            path: self.original_path(row.rel.as_str()),
            stat: stat_tuple(st),
            hash: row.hash,
            content: row.content,
            sample_only,
        }
    }

    /// Move an image's sidecars to follow its original from `from` to `to`
    /// (inside the caller's transaction). `None` when `to` is taken or the
    /// file changed meanwhile.
    fn relink_one(
        &self,
        tx: &rusqlite::Transaction<'_>,
        image: ImageId,
        from: &RelPath,
        to: &str,
        st: FileStat,
    ) -> Result<Option<Moved>> {
        let Ok(to) = RelPath::new(to) else {
            return Ok(None);
        };
        if original_stat(&self.original_path(to.as_str())) != Some(stat_tuple(st)) {
            return Ok(None);
        }
        let sidecars = self.image_sidecars(image)?;
        if !self.destination_free(&to, &sidecars)? {
            return Ok(None);
        }
        let ops = self.relocation_ops(from, &to, &sidecars, false);
        if crate::files::run_ops(&ops).is_err() {
            return Ok(None);
        }
        Self::record_relocation(tx, image, &to, &sidecars)?;
        record_stat(tx, image.0, st)?;
        Ok(Some(Moved {
            image,
            from: from.clone(),
            to,
        }))
    }

    /// Match held files (hashed) to missing photos by content: the whole-file
    /// hash, or the sensor-data hash (renamed *and* re-tagged). Several
    /// identical candidates are paired by name only when that is unambiguous
    /// ([`crate::identity::pair_by_name`]); ids are never swapped.
    pub fn relink_found(&mut self, found: &[Found]) -> Result<RelinkReport> {
        let rows: Vec<Row> = self
            .external_rows()?
            .into_iter()
            .filter(|r| r.missing && r.hash != UNHASHED)
            .collect();
        // Group missing photos and found files by the content they share.
        let key_of = |r: &Row| r.content.unwrap_or(r.hash);
        let mut groups: BTreeMap<[u8; 32], (Vec<&Row>, Vec<&Found>)> = BTreeMap::new();
        for r in &rows {
            groups.entry(key_of(r).0).or_default().0.push(r);
        }
        let mut report = RelinkReport::default();
        let index = HashIndex::new(rows.iter().map(|r| (r.hash, r.content)));
        for f in found {
            match index
                .first(f.hashes.file, f.hashes.content)
                .map(|i| &rows[i])
            {
                Some(r) => groups.entry(key_of(r).0).or_default().1.push(f),
                None => report.unmatched.push(f.rel.clone()),
            }
        }
        let mut edited = Vec::new();
        let tx = self.write_lock()?;
        for (missing, files) in groups.values() {
            if files.is_empty() {
                continue;
            }
            let m_names: Vec<&str> = missing.iter().map(|r| r.rel.as_str()).collect();
            let f_names: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
            let pairs = pair_by_name(&m_names, &f_names);
            let mut used = HashSet::new();
            for &(i, j) in &pairs {
                let (row, f) = (missing[i], files[j]);
                match self.relink_one(&tx, row.id, &row.rel, &f.rel, file_stat(f.stat))? {
                    Some(m) => {
                        used.insert(j);
                        if f.hashes.file != row.hash || f.hashes.content != row.content {
                            edited.push(Verified {
                                image: row.id,
                                stat: f.stat,
                                hashes: f.hashes,
                            });
                        }
                        report.relinked.push(m);
                    }
                    // Its place is taken: the photo stays missing, the file is new.
                    None => report.ambiguous.push(row.rel.as_str().to_owned()),
                }
            }
            if pairs.len() < missing.len().min(files.len()) {
                for (i, r) in missing.iter().enumerate() {
                    if !pairs.iter().any(|p| p.0 == i) {
                        report.ambiguous.push(r.rel.as_str().to_owned());
                    }
                }
            }
            for (j, f) in files.iter().enumerate() {
                if !used.contains(&j) {
                    report.unmatched.push(f.rel.clone());
                }
            }
        }
        tx.commit()?;
        if !edited.is_empty() {
            self.accept_verified(&edited)?;
        }
        report.ambiguous.sort();
        report.unmatched.sort();
        Ok(report)
    }

    /// Record the hashes of originals checked in the background: the same
    /// bytes, a metadata-only edit (same sensor data: the new whole-file hash
    /// is stored and everything else kept), or a first hash of a file
    /// registered by stat. Items whose file changed since they were hashed
    /// are skipped (the next sync looks again). Sidecar hash updates are
    /// written as one batch. Returns the images recorded.
    pub fn accept_verified(&mut self, items: &[Verified]) -> Result<Vec<ImageId>> {
        let mut ok = Vec::new();
        let mut want: HashMap<i64, Hashes> = HashMap::new();
        let mut vs = Vec::new();
        for it in items {
            let Ok(info) = self.image(it.image) else {
                continue;
            };
            if original_stat(&self.original_path(info.rel_path.as_str())) != Some(it.stat) {
                continue;
            }
            if info.hash != it.hashes.file || info.content_hash != it.hashes.content {
                for v in self.variants(it.image)? {
                    if !self.variant(v)?.read_only {
                        want.insert(v.0, it.hashes);
                        vs.push(v);
                    }
                }
            }
            ok.push(it);
        }
        let (_, err) = self.modify_collect(&vs, |v, sc| {
            let h = want[&v.0];
            let changed = sc.hash != h.file || sc.content_hash != h.content;
            sc.hash = h.file;
            sc.content_hash = h.content;
            changed
        })?;
        if let Some(e) = err {
            return Err(e);
        }
        let tx = self.write_lock()?;
        for it in &ok {
            tx.execute(
                "UPDATE images SET hash = ?, content_hash = ?, sample_hash = ? WHERE id = ?",
                params![
                    &it.hashes.file.0[..],
                    it.hashes.content.as_ref().map(|h| &h.0[..]),
                    &it.hashes.sample.0[..],
                    it.image.0
                ],
            )?;
            record_stat(&tx, it.image.0, file_stat(it.stat))?;
        }
        tx.commit()?;
        Ok(ok.iter().map(|it| it.image).collect())
    }

    /// Compare sample hashes (FAT/exFAT sample checks). An image without a
    /// stored sample gets this one; returns the images whose sample differs
    /// (their file must be verified in full).
    pub fn check_samples(&mut self, items: &[(ImageId, FileHash)]) -> Result<Vec<ImageId>> {
        let tx = self.write_lock()?;
        let mut differ = Vec::new();
        for (id, sample) in items {
            let stored: Option<Option<Vec<u8>>> = tx
                .query_row("SELECT sample_hash FROM images WHERE id = ?", [id.0], |r| {
                    r.get(0)
                })
                .optional()?;
            match stored {
                Some(Some(s)) if s.as_slice() != sample.0 => differ.push(*id),
                None | Some(Some(_)) => {}
                Some(None) => {
                    tx.execute(
                        "UPDATE images SET sample_hash = ? WHERE id = ?",
                        params![&sample.0[..], id.0],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(differ)
    }

    /// The file of `old` is a different photo now: set its metadata aside
    /// (sidecars to `Trash/.lt/deleted/`, restorable with
    /// [`Self::restore_replaced`]) and register the file as a new photo
    /// flagged "file changed". `None` if the file changed again meanwhile.
    pub fn replace_image(
        &mut self,
        old: ImageId,
        stat: Stat,
        hashes: Option<Hashes>,
        meta: ImageMetaLite,
    ) -> Result<Option<ImageId>> {
        let info = self.image(old)?;
        if original_stat(&self.original_path(info.rel_path.as_str())) != Some(stat) {
            return Ok(None);
        }
        let previous: Vec<i64> = self.variants(old)?.iter().map(|v| v.0).collect();
        self.forget_images(&[old])?;
        let ids = self.add_new_originals(vec![NewOriginal {
            rel: info.rel_path.as_str().to_owned(),
            hash: hashes.map_or(UNHASHED, |h| h.file),
            content_hash: hashes.and_then(|h| h.content),
            meta,
            replaced: previous,
            iptc: lt_core::IptcMeta::default(),
        }])?;
        if let Some(h) = hashes {
            self.conn.execute(
                "UPDATE images SET sample_hash = ? WHERE id = ?",
                params![&h.sample.0[..], ids[0].0],
            )?;
        }
        Ok(Some(ids[0]))
    }

    /// Put back the metadata of the photo a "file changed" image replaced:
    /// its sidecars return from `Trash/.lt/deleted/` (their ids, ratings and
    /// edits) and now describe this file; the fresh metadata goes to the
    /// Trash instead. Returns the restored image id.
    pub fn restore_replaced(&mut self, image: ImageId) -> Result<ImageId> {
        let info = self.image(image)?;
        let current = self.variants(image)?;
        let first = self.locate(current[0])?;
        let loaded = self.read_sidecar(&first.sidecar)?;
        let previous = loaded.sidecar.replaced.clone();
        if previous.is_empty() {
            return Err(SessionError::NotFound {
                kind: "replaced photo of image",
                id: image.0,
            });
        }
        let deleted = self.root.join(DELETED_DIR);
        let mut old = Vec::new();
        for vid in &previous {
            let rel = format!("{DELETED_DIR}/{vid}.json");
            if !deleted.join(format!("{vid}.json")).is_file() {
                return Err(SessionError::NotFound {
                    kind: "set-aside metadata of variant",
                    id: *vid,
                });
            }
            old.push((rel.clone(), self.read_sidecar(&rel)?));
        }
        let tx = self.write_lock()?;
        let mut ops = Vec::new();
        for (vid, _, sc) in self.image_sidecars(image)? {
            ops.push((self.root.join(sc), deleted.join(format!("{vid}.json"))));
        }
        let mut writes = Vec::new();
        for (rel, l) in &old {
            let dest = info.rel_path.sidecar_rel(l.sidecar.ordinal);
            ops.push((self.root.join(rel), self.root.join(&dest)));
            let mut sc = l.sidecar.clone();
            sc.hash = info.hash;
            sc.content_hash = info.content_hash;
            sc.replaced.clear();
            writes.push(Pending {
                rel: dest,
                sidecar: sc,
                base: l.raw.clone(),
            });
        }
        crate::files::run_ops(&ops)?;
        let (_, err) = self.write_sidecars(writes);
        if let Some(e) = err {
            crate::files::rollback(&ops);
            return Err(e);
        }
        tx.execute("DELETE FROM variants WHERE image_id = ?", [image.0])?;
        tx.execute("DELETE FROM images WHERE id = ?", [image.0])?;
        tx.commit()?;
        for v in &current {
            self.undo.forget(v.0);
        }
        self.drop_from_albums(&current)?;
        self.sync_index()?;
        let restored = ImageId(old[0].1.sidecar.image_id);
        if let Some(st) = original_stat(&self.original_path(info.rel_path.as_str())) {
            let tx = self.write_lock()?;
            record_stat(&tx, restored.0, file_stat(st))?;
            tx.commit()?;
        }
        Ok(restored)
    }

    /// Keep a "file changed" photo as the new photo it is: the flag goes
    /// (the previous metadata stays in the session Trash until emptied).
    pub fn dismiss_file_changed(&mut self, image: ImageId) -> Result<()> {
        let vs = self.variants(image)?;
        self.modify(&vs, |_, sc| {
            let changed = !sc.replaced.is_empty();
            sc.replaced.clear();
            changed
        })?;
        self.conn
            .execute("UPDATE images SET file_changed = 0 WHERE id = ?", [image.0])?;
        Ok(())
    }

    /// Point a (missing) image of an external session at `to`, a file in the
    /// folder whose content hash is the image's (the caller verifies it; see
    /// "Locate…" in the app). Only sidecars move, never the original.
    pub fn relink_external(&mut self, image: ImageId, to: &crate::RelPath) -> Result<()> {
        if !self.is_external() {
            return Err(SessionError::ExternalOnly("relink_external"));
        }
        if to.folder() != crate::Folder::Capture {
            return Err(SessionError::ExternalOriginals("relink outside Capture"));
        }
        let from = self.image(image)?.rel_path;
        if &from == to {
            return Ok(());
        }
        let sidecars = self.image_sidecars(image)?;
        if !self.destination_free(to, &sidecars)? {
            return Err(SessionError::DuplicatePath(to.as_str().to_owned()));
        }
        let stat = original_stat(&self.original_path(to.as_str()))
            .ok_or_else(|| SessionError::OriginalMissing(to.as_str().to_owned()))?;
        let tx = self.write_lock()?;
        let ops = self.relocation_ops(&from, to, &sidecars, false);
        crate::files::run_ops(&ops)?;
        let db = Self::record_relocation(&tx, image, to, &sidecars).and_then(|()| {
            record_stat(&tx, image.0, file_stat(stat))?;
            Ok(tx.commit()?)
        });
        if let Err(e) = db {
            crate::files::rollback(&ops);
            return Err(e);
        }
        Ok(())
    }

    /// Drop images (all their variants) from the session. Their sidecars are
    /// moved to `Trash/.lt/deleted/` (recoverable, never removed); the
    /// originals are never touched. Returns how many images were dropped.
    pub fn forget_images(&mut self, ids: &[ImageId]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let tx = self.write_lock()?;
        let mut ops = Vec::new();
        let mut variants = Vec::new();
        let mut dropped = 0;
        for &id in ids {
            let Ok(_) = self.image(id) else { continue };
            let mut st = self
                .conn
                .prepare_cached("SELECT id, sidecar FROM variants WHERE image_id = ?")?;
            let rows: Vec<(i64, String)> = st
                .query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (vid, sc) in rows {
                let src = self.root.join(&sc);
                if src.exists() {
                    let dest = self.root.join(DELETED_DIR).join(format!("{vid}.json"));
                    ops.push((src, dest));
                }
                variants.push(VariantId(vid));
            }
            dropped += 1;
        }
        crate::files::run_ops(&ops)?;
        for &id in ids {
            tx.execute("DELETE FROM variants WHERE image_id = ?", [id.0])?;
            tx.execute("DELETE FROM images WHERE id = ?", [id.0])?;
        }
        tx.commit()?;
        for v in &variants {
            self.undo.forget(v.0);
        }
        if !variants.is_empty() {
            self.drop_from_albums(&variants)?;
        }
        Ok(dropped)
    }

    /// [`Self::forget_images`] for photos the user removes as missing: an
    /// image whose original is back on disk (the listing was stale) is kept.
    ///
    /// Fails closed: the originals' folder must be readable (an unmounted or
    /// unreachable drive is an error, nothing is forgotten), and a photo is
    /// gone only when its file reports "not found" (an unreadable one is kept). The folder
    /// is checked again after the files, so a drive gone mid-check forgets nothing.
    pub fn forget_missing(&mut self, ids: &[ImageId]) -> Result<usize> {
        let base = self.external_root().unwrap_or(&self.root).to_path_buf();
        fs::read_dir(&base).at(&base)?;
        let gone: Vec<ImageId> = ids
            .iter()
            .copied()
            .filter(|&id| {
                self.image(id).is_ok_and(|i| {
                    fs::symlink_metadata(self.original_path(i.rel_path.as_str()))
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                })
            })
            .collect();
        // Unmounted while the files were checked: their "not found" means nothing.
        fs::read_dir(&base).at(&base)?;
        self.forget_images(&gone)
    }

    /// Images registered by stat whose content hasn't been hashed yet,
    /// with the path of their original.
    pub fn unhashed_images(&self) -> Result<Vec<(ImageId, std::path::PathBuf)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id, rel_path FROM images WHERE hash = ? AND missing = 0")?;
        let rows: Vec<(i64, String)> = st
            .query_map([&UNHASHED.0[..]], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows
            .into_iter()
            .map(|(id, rel)| (ImageId(id), self.original_path(&rel)))
            .collect())
    }

    /// Record the whole-file hash of an image's original, computed while its
    /// stat was `stat`. If the file changed since, nothing is recorded and
    /// `false` is returned (the next sync looks again). See
    /// [`Self::accept_verified`] for the batch form with content hashes.
    pub fn set_verified_hash(
        &mut self,
        image: ImageId,
        hash: FileHash,
        stat: Stat,
    ) -> Result<bool> {
        let path = self.original_path(self.image(image)?.rel_path.as_str());
        let Ok(sample) = crate::identity::sample_hash(&path) else {
            return Ok(false);
        };
        let done = self.accept_verified(&[Verified {
            image,
            stat,
            hashes: Hashes {
                file: hash,
                content: None,
                sample,
            },
        }])?;
        Ok(!done.is_empty())
    }

    /// Where the only copy of a missing photo waits after a migration
    /// (`<store>/Capture/<rel>`), if it is there.
    pub fn kept_copy(&self, image: ImageId) -> Option<PathBuf> {
        let info = self.image(image).ok()?;
        if !self.is_external() {
            return None;
        }
        let p = self.root.join(info.rel_path.as_str());
        p.symlink_metadata()
            .is_ok_and(|m| m.file_type().is_file())
            .then_some(p)
    }

    /// Move the only copy of a missing photo (kept in the store by a
    /// migration) back into the user's folder at its path. Never overwrites:
    /// fails if a file is there.
    pub fn restore_kept_copy(&mut self, image: ImageId) -> Result<()> {
        let from = self.kept_copy(image).ok_or(SessionError::NotFound {
            kind: "kept copy of image",
            id: image.0,
        })?;
        let rel = self.image(image)?.rel_path;
        let to = self.original_path(rel.as_str());
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).at(parent)?;
        }
        crate::files::run_ops(&[(from, to.clone())])?;
        if let Some(st) = original_stat(&to) {
            let tx = self.write_lock()?;
            record_stat(&tx, image.0, file_stat(st))?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Convert an old in-place session whose `Capture/` holds hard links to
    /// the files in `originals` into an external session over `originals`.
    /// Ids, sidecars, ratings and edits are kept. A link is removed only when
    /// it is the same inode as a regular (non-symlink) original outside the
    /// store, with a link count above one, re-checked right before removal;
    /// a plain copy is removed only when the folder has the identical file at
    /// the same path; anything else is the only copy and is kept.
    pub fn migrate_to_external(dir: &Path, originals: &Path) -> Result<Migrated> {
        let settings = crate::session::read_settings(dir)?;
        if matches!(settings.originals, Originals::External(_)) {
            return Ok(Migrated::default());
        }
        let capture = dir.join("Capture");
        let mut report = Migrated {
            converted: true,
            ..Migrated::default()
        };
        let mut stack = vec![capture.clone()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                let Ok(ft) = e.file_type() else { continue };
                if name.starts_with('.') {
                    continue; // .lt/settings sidecars stay
                }
                if ft.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let rel = path.strip_prefix(&capture).unwrap_or(&path).to_owned();
                let original = originals.join(&rel);
                match same_original(&path, &original, dir) {
                    Some(Twin::Link(id)) => {
                        // Re-check just before removing: still that inode, still linked.
                        if fs::symlink_metadata(&path)
                            .is_ok_and(|m| (m.dev(), m.ino()) == id && m.nlink() > 1)
                        {
                            fs::remove_file(&path).at(&path)?;
                            report.links_removed += 1;
                            continue;
                        }
                    }
                    // The folder's copy must still be the one compared.
                    Some(Twin::Copy(stat)) if original_stat(&original) == Some(stat) => {
                        fs::remove_file(&path).at(&path)?;
                        report.duplicates_removed += 1;
                        continue;
                    }
                    Some(Twin::Copy(_)) | None => {}
                }
                report.kept.push(rel.to_string_lossy().into_owned());
            }
        }
        report.kept.sort();
        let mut next = settings;
        next.originals = Originals::External(originals.to_owned());
        crate::session::write_settings(dir, &next)?;
        Ok(report)
    }
}

/// How a file in an old store relates to the user's file at the same path.
enum Twin {
    /// The same inode `(dev, ino)`: a hard link.
    Link((u64, u64)),
    /// A separate file with identical bytes (the original's stat when compared).
    Copy(Stat),
}

fn same_original(link: &Path, original: &Path, store: &Path) -> Option<Twin> {
    let l = fs::symlink_metadata(link).ok()?;
    let o = fs::symlink_metadata(original).ok()?;
    // Only a regular file outside the store counts as the user's original
    // (a symlink could point back into the store).
    if !l.file_type().is_file() || !o.file_type().is_file() || original.starts_with(store) {
        return None;
    }
    if l.dev() == o.dev() && l.ino() == o.ino() {
        return (l.nlink() > 1).then_some(Twin::Link((l.dev(), l.ino())));
    }
    if l.len() != o.len() {
        return None;
    }
    let stat = original_stat(original)?;
    let never = || false;
    let a = crate::identity::hash_original(link, None, &never).ok()?;
    let b = crate::identity::hash_original(original, None, &never).ok()?;
    (a.file == b.file).then_some(Twin::Copy(stat))
}

/// `(size, mtime_ns, inode)` of a file, as [`Session::set_verified_hash`] expects.
pub fn original_stat(path: &Path) -> Option<Stat> {
    let md = fs::metadata(path).ok()?;
    let st = FileStat::of(&md);
    Some((st.size, st.mtime, st.ino))
}

#[cfg(test)]
mod hash_index_tests {
    use super::*;

    fn h(n: u32) -> FileHash {
        FileHash(*blake3::hash(&n.to_le_bytes()).as_bytes())
    }

    #[test]
    fn matches_by_file_or_content_and_prefers_the_earliest_row() {
        let rows = [(h(1), None), (h(2), Some(h(20))), (h(3), Some(h(20)))];
        let index = HashIndex::new(rows.into_iter());
        assert_eq!(index.first(h(1), None), Some(0));
        assert_eq!(index.first(h(3), None), Some(2), "by file hash");
        assert_eq!(
            index.first(h(99), Some(h(20))),
            Some(1),
            "by content, earliest"
        );
        assert_eq!(
            index.first(h(3), Some(h(20))),
            Some(1),
            "either way, earliest"
        );
        assert_eq!(index.first(h(99), Some(h(98))), None);
        assert_eq!(index.first(h(99), None), None);
    }

    #[test]
    fn a_large_library_is_matched_without_scanning() {
        let n = 200_000;
        let index = HashIndex::new((0..n).map(|i| (h(i), None)));
        let start = std::time::Instant::now();
        for i in (0..n).rev() {
            assert_eq!(index.first(h(i), None), Some(i as usize));
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }
}
