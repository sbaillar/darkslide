//! Moving originals between folders, emptying the session Trash, and
//! relinking files moved behind the session's back.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ds_core::{FileHash, PickState};
use rusqlite::{OptionalExtension, params};

use crate::error::{IoContext, Result, SessionError};
use crate::relpath::{Folder, RelPath, ensure_no_symlinks};
use crate::session::{DELETED_DIR, FileStat, ImageId, Session, UNHASHED, VariantId, record_stat};
use crate::trash::Trasher;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub image: ImageId,
    pub from: RelPath,
    pub to: RelPath,
}

/// Outcome of [`Session::trash_rejected`].
#[derive(Debug)]
pub struct TrashedRejects {
    /// Originals moved to `Trash/` (every variant rejected).
    pub moved: Vec<Moved>,
    /// Rejected variants of photos that have a kept variant: deleted.
    pub hidden: Vec<VariantId>,
    /// Set when it stopped part-way: `moved` and `hidden` are what was done.
    pub error: Option<SessionError>,
}

/// Outcome of [`Session::rescan`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RescanReport {
    /// Originals found at a new path by content hash; sidecars followed them.
    pub relinked: Vec<Moved>,
    /// Originals that are nowhere to be found (includes `replaced` and `conflicts`).
    pub missing: Vec<(ImageId, RelPath)>,
    /// The file at the image's path now has different content (e.g. another
    /// file was renamed onto it). The edits are not applied to it.
    pub replaced: Vec<(ImageId, RelPath)>,
    /// Content found at a new path that could not be relinked there (the path
    /// or its sidecar locations are taken): the user has to sort these out.
    pub conflicts: Vec<(ImageId, RelPath)>,
    /// Files in the image folders that the session does not know.
    pub untracked: Vec<String>,
    /// Files seen by the walk of an external folder (more than it lists when
    /// it hit [`crate::MAX_EXTERNAL_FILES`]).
    pub seen: usize,
    /// Originals whose stat changed but that could not be read (I/O error,
    /// permissions): kept as they are, looked at again on the next rescan.
    pub unreadable: Vec<(ImageId, RelPath)>,
}

/// One planned file rename (absolute paths).
type Op = (PathBuf, PathBuf);

/// Run renames in order, never overwriting; on failure undo the completed
/// ones in reverse and return the error.
///
/// Uses [`ds_fsutil::rename_noreplace`]: `RENAME_EXCL` where the file system
/// has it, check-then-rename where it doesn't (exFAT/FAT session disks).
/// Callers hold the session's write lock ([`Session::write_lock`]), so no
/// other Darkslide writer can take a name between the check and the rename.
pub(crate) fn run_ops(ops: &[Op]) -> Result<()> {
    for (i, (from, to)) in ops.iter().enumerate() {
        let res = (|| {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent).at(parent)?;
            }
            ds_fsutil::rename_noreplace(from, to).at(to)
        })();
        if let Err(e) = res {
            rollback(&ops[..i]);
            return Err(e);
        }
    }
    Ok(())
}

pub(crate) fn rollback(done: &[Op]) {
    for (from, to) in done.iter().rev() {
        // Best effort: the forward rename just succeeded, so the reverse normally does too.
        let _ = ds_fsutil::rename_noreplace(to, from);
    }
}

/// `name.ext` → `name-2.ext`, `name-3.ext`, ... for collision avoidance.
fn numbered(file: &str, n: u32) -> String {
    match file.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{n}.{ext}"),
        _ => format!("{file}-{n}"),
    }
}

pub(crate) fn hash_file(path: &Path) -> std::io::Result<FileHash> {
    let mut h = blake3::Hasher::new();
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(FileHash(*h.finalize().as_bytes()))
}

fn stat_of(path: &Path) -> Option<FileStat> {
    fs::metadata(path).ok().map(|m| FileStat::of(&m))
}

/// An image row as rescan needs it.
#[derive(Debug, Clone)]
struct Tracked {
    id: ImageId,
    rel: RelPath,
    hash: FileHash,
    size: u64,
    /// Stat recorded when the content was last verified.
    verified: Option<FileStat>,
}

impl Session {
    /// Ordinal and sidecar path of every variant of `image`.
    pub(crate) fn image_sidecars(&self, image: ImageId) -> Result<Vec<(i64, u32, String)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT id, ordinal, sidecar FROM variants WHERE image_id = ?")?;
        Ok(st
            .query_map([image.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Plan relocating an image's sidecars (and, if `with_original`, the
    /// original) to `to`.
    pub(crate) fn relocation_ops(
        &self,
        from: &RelPath,
        to: &RelPath,
        sidecars: &[(i64, u32, String)],
        with_original: bool,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        if with_original {
            ops.push((from.to_path(&self.root), to.to_path(&self.root)));
        }
        for (_, ord, rel) in sidecars {
            let src = self.root.join(rel);
            if src.exists() {
                ops.push((src, self.root.join(to.sidecar_rel(*ord))));
            }
        }
        ops
    }

    /// Whether `to` can receive an image with these sidecars: the path is
    /// free on disk and in the index, every sidecar location is free, and
    /// nothing on the way is a symlink.
    pub(crate) fn destination_free(
        &self,
        to: &RelPath,
        sidecars: &[(i64, u32, String)],
    ) -> Result<bool> {
        if ensure_no_symlinks(&self.root, to.as_str()).is_err() || self.rel_in_index(to)? {
            return Ok(false);
        }
        for (_, ord, _) in sidecars {
            let rel = to.sidecar_rel(*ord);
            if ensure_no_symlinks(&self.root, &rel).is_err()
                || self.root.join(&rel).symlink_metadata().is_ok()
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn record_relocation(
        tx: &rusqlite::Transaction<'_>,
        image: ImageId,
        to: &RelPath,
        sidecars: &[(i64, u32, String)],
    ) -> Result<()> {
        tx.execute(
            "UPDATE images SET rel_path = ?, folder = ?, file_name_lc = ?, missing = 0 WHERE id = ?",
            params![to.as_str(), to.folder().code(), to.file_name().to_lowercase(), image.0],
        )?;
        for (vid, ord, _) in sidecars {
            tx.execute(
                "UPDATE variants SET sidecar = ? WHERE id = ?",
                params![to.sidecar_rel(*ord), vid],
            )?;
        }
        Ok(())
    }

    /// Move the originals behind `vs` (with all their variants' sidecars) into
    /// `folder`, keeping sub-directories. Name clashes (original or any
    /// sidecar location) get a `-2`, `-3` suffix. Files are never overwritten
    /// or deleted; on any failure every rename done so far is rolled back.
    pub fn move_to(&mut self, vs: &[VariantId], folder: Folder) -> Result<Vec<Moved>> {
        self.refuse_external("moving originals")?;
        let mut targets: Vec<(ImageId, RelPath)> = Vec::new();
        for &v in vs {
            let image = self.locate(v)?.image;
            if !targets.iter().any(|(i, _)| *i == image) {
                let from = self.image(image)?.rel_path;
                if from.folder() != folder {
                    targets.push((image, from.with_folder(folder)));
                }
            }
        }
        self.relocate(targets)
    }

    /// Undo of [`Self::move_to`]: put each original (named by any of its
    /// variants) back at its path, or at a free `-2`, `-3` name next to it
    /// when that was taken meanwhile. Same guarantees as [`Self::move_to`].
    pub fn move_back(&mut self, moves: &[(VariantId, RelPath)]) -> Result<Vec<Moved>> {
        self.refuse_external("moving originals")?;
        let mut targets: Vec<(ImageId, RelPath)> = Vec::new();
        for (v, to) in moves {
            let image = self.locate(*v)?.image;
            if self.image(image)?.rel_path != *to && !targets.iter().any(|(i, _)| *i == image) {
                targets.push((image, to.clone()));
            }
        }
        self.relocate(targets)
    }

    /// Move each image (with its sidecars) to its base path, or the first free
    /// numbered name next to it; all or nothing.
    fn relocate(&mut self, targets: Vec<(ImageId, RelPath)>) -> Result<Vec<Moved>> {
        let tx = self.write_lock()?;
        let mut taken: HashSet<RelPath> = HashSet::new();
        let mut plan = Vec::new();
        for (image, base) in targets {
            let from = self.image(image)?.rel_path;
            let sidecars = self.image_sidecars(image)?;
            let mut to = base.clone();
            let mut n = 2;
            while taken.contains(&to)
                || to.to_path(&self.root).symlink_metadata().is_ok()
                || !self.destination_free(&to, &sidecars)?
            {
                if n > 10_000 {
                    return Err(SessionError::InvalidPath {
                        path: base.as_str().to_owned(),
                        reason: "no free destination name",
                    });
                }
                to = base.with_file_name(&numbered(base.file_name(), n))?;
                n += 1;
            }
            taken.insert(to.clone());
            plan.push((Moved { image, from, to }, sidecars));
        }
        let ops: Vec<Op> = plan
            .iter()
            .flat_map(|(m, sc)| self.relocation_ops(&m.from, &m.to, sc, true))
            .collect();
        run_ops(&ops)?;
        let db = plan
            .iter()
            .try_for_each(|(m, sc)| Self::record_relocation(&tx, m.image, &m.to, sc))
            .and_then(|()| Ok(tx.commit()?));
        if let Err(e) = db {
            rollback(&ops);
            return Err(e);
        }
        Ok(plan.into_iter().map(|(m, _)| m).collect())
    }

    /// Variants of `image` whose pick is rejected.
    fn rejected_variants(&self, image: ImageId) -> Result<(Vec<VariantId>, usize)> {
        let vs = self.variants(image)?;
        let mut rejected = Vec::new();
        for &v in &vs {
            if self.variant(v)?.pick == PickState::Rejected {
                rejected.push(v);
            }
        }
        Ok((rejected, vs.len()))
    }

    /// A rejected photo: in the session's `Trash/`, or every variant rejected
    /// (what "Move Rejected to Trash" moves, and what the archive leaves out).
    pub fn is_rejected(&self, image: &crate::ImageInfo) -> Result<bool> {
        if image.rel_path.folder() == Folder::Trash {
            return Ok(true);
        }
        let (rejected, all) = self.rejected_variants(image.id)?;
        Ok(all > 0 && rejected.len() == all)
    }

    /// "Move Rejected to Trash": every original outside `Trash/` whose variants
    /// are all rejected moves to `Trash/` ([`Self::move_to`]). A photo with a
    /// kept variant stays where it is: only its rejected variants are deleted
    /// ([`Self::delete_variant`]: sidecars set aside, back with
    /// [`Self::restore_variants`]).
    pub fn trash_rejected(&mut self) -> Result<TrashedRejects> {
        self.refuse_external("moving originals")?;
        let mut whole = Vec::new();
        let mut hide = Vec::new();
        for img in self.images()? {
            if img.rel_path.folder() == Folder::Trash {
                continue;
            }
            let (rejected, all) = self.rejected_variants(img.id)?;
            if rejected.len() == all {
                whole.extend(rejected.first().copied());
            } else {
                hide.extend(rejected);
            }
        }
        let moved = self.move_to(&whole, Folder::Trash)?;
        let mut hidden = Vec::new();
        for v in hide {
            if let Err(e) = self.delete_variant(v) {
                // What was done stays done (and listed): the caller can undo it.
                return Ok(TrashedRejects {
                    moved,
                    hidden,
                    error: Some(e),
                });
            }
            hidden.push(v);
        }
        Ok(TrashedRejects {
            moved,
            hidden,
            error: None,
        })
    }

    /// Bring deleted variants back (undo of [`Self::delete_variant`]): each
    /// sidecar returns from `Trash/.ds/deleted/` next to its original, wherever
    /// that is now. One whose original is gone, or whose location a newer
    /// variant took meanwhile, stays set aside. Returns how many came back.
    pub fn restore_variants(&mut self, vs: &[VariantId]) -> Result<usize> {
        let lock = self.write_lock()?;
        let mut ops = Vec::new();
        for v in vs {
            let rel = format!("{DELETED_DIR}/{}.json", v.0);
            if !self.root.join(&rel).is_file() || self.variant(*v).is_ok() {
                continue;
            }
            let sc = self.read_sidecar(&rel)?.sidecar;
            let Ok(img) = self.image(ImageId(sc.image_id)) else {
                continue;
            };
            let dest = img.rel_path.sidecar_rel(sc.ordinal);
            if ensure_no_symlinks(&self.root, &dest).is_err()
                || self.root.join(&dest).symlink_metadata().is_ok()
            {
                continue;
            }
            ops.push((self.root.join(&rel), self.root.join(&dest)));
        }
        if ops.is_empty() {
            return Ok(0);
        }
        run_ops(&ops)?;
        // The index update takes the lock itself.
        drop(lock);
        if let Err(e) = self.sync_index() {
            rollback(&ops);
            return Err(e);
        }
        let back: Vec<VariantId> = vs
            .iter()
            .copied()
            .filter(|v| self.variant(*v).is_ok())
            .collect();
        self.restore_album_memberships(&back)?;
        Ok(ops.len())
    }

    /// Every variant set aside in `Trash/.ds/deleted/` (by "Move Rejected to
    /// Trash" or Delete Variant), found on disk so it survives a relaunch.
    /// Hand them to [`Self::restore_variants`].
    pub fn set_aside_variants(&self) -> Result<Vec<VariantId>> {
        let dir = self.root.join(DELETED_DIR);
        let rd = match fs::read_dir(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            rd => rd.at(&dir)?,
        };
        let mut out: Vec<VariantId> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let id = name.to_str()?.strip_suffix(".json")?.parse().ok()?;
                Some(VariantId(id))
            })
            .collect();
        out.sort_by_key(|v| v.0);
        Ok(out)
    }

    fn rel_in_index(&self, rel: &RelPath) -> Result<bool> {
        let mut st = self
            .conn
            .prepare_cached("SELECT 1 FROM images WHERE rel_path = ?")?;
        Ok(st.exists([rel.as_str()])?)
    }

    /// Hand everything in the session Trash (originals, their sidecars, and
    /// deleted variants' sidecars) to `trasher` (the macOS Trash in the app)
    /// and drop it from the index. Returns the number of originals removed.
    /// This is the only way files leave a session.
    ///
    /// One photo at a time: when the trasher refuses (a volume without a
    /// Trash), the photos already handed over leave the index and the rest
    /// stay as they are, so the index always matches the disk.
    pub fn empty_trash(&mut self, trasher: &dyn Trasher) -> Result<usize> {
        self.refuse_external("emptying the Trash")?;
        let tx = self.write_lock()?;
        let mut st = self
            .conn
            .prepare("SELECT id, rel_path FROM images WHERE folder = ?")?;
        let rows: Vec<(i64, String)> = st
            .query_map([Folder::Trash.code()], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        drop(st);
        let mut variants = Vec::new();
        let mut removed = 0;
        let mut refused = None;
        for (id, rel) in &rows {
            // The original first: a refusing volume refuses before any sidecar goes.
            let mut paths = vec![self.root.join(rel)];
            let sidecars = self.image_sidecars(ImageId(*id))?;
            paths.extend(sidecars.iter().map(|(_, _, sc)| self.root.join(sc)));
            paths.retain(|p| p.exists());
            if let Err(e) = trasher.trash(&paths) {
                refused = Some(e);
                break;
            }
            tx.execute("DELETE FROM variants WHERE image_id = ?", [id])?;
            tx.execute("DELETE FROM images WHERE id = ?", [id])?;
            variants.extend(sidecars.iter().map(|(vid, _, _)| VariantId(*vid)));
            removed += 1;
        }
        if refused.is_none() {
            let deleted = self.root.join(DELETED_DIR);
            if let Ok(rd) = fs::read_dir(&deleted) {
                let paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                refused = trasher.trash(&paths).err();
            }
        }
        tx.commit()?;
        for v in &variants {
            self.undo.forget(v.0);
        }
        if !variants.is_empty() {
            self.drop_from_albums(&variants)?;
        }
        match refused {
            Some(e) => Err(SessionError::Trash(e)),
            None => Ok(removed),
        }
    }

    fn tracked(&self) -> Result<Vec<Tracked>> {
        let mut st = self.conn.prepare_cached(
            "SELECT id, rel_path, hash, file_size, file_mtime, file_ino FROM images",
        )?;
        let rows = st.query_map([], |r| {
            let rel: String = r.get(1)?;
            let hash: Vec<u8> = r.get(2)?;
            let size = r.get::<_, i64>(3)? as u64;
            let mtime: Option<i64> = r.get(4)?;
            let ino: Option<i64> = r.get(5)?;
            Ok(Tracked {
                id: ImageId(r.get(0)?),
                rel: RelPath::new(&rel).map_err(|_| rusqlite::Error::InvalidQuery)?,
                hash: FileHash(hash.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?),
                size,
                verified: mtime.zip(ino).map(|(mtime, ino)| FileStat {
                    size,
                    mtime,
                    ino: ino as u64,
                }),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Phase 1 of [`Self::rescan`] (unlocked): tracked images whose stat
    /// changed, with their content hash now, and images not found on disk.
    #[allow(clippy::type_complexity)]
    fn classify<'a>(
        &self,
        images: &'a [Tracked],
        originals: &HashMap<String, FileStat>,
    ) -> (
        Vec<(&'a Tracked, FileStat, Option<FileHash>)>,
        Vec<&'a Tracked>,
    ) {
        let mut verify = Vec::new();
        let mut missing = Vec::new();
        for img in images {
            match originals.get(img.rel.as_str()) {
                Some(st) if img.verified == Some(*st) => {}
                // Not hashed yet (registered by stat): same size and mtime is
                // the same file (a copied folder, or a card whose inode numbers
                // changed on remount); anything else can't be proven the same.
                Some(st)
                    if img.hash == UNHASHED
                        && img
                            .verified
                            .is_some_and(|v| v.size == st.size && v.mtime == st.mtime) =>
                {
                    verify.push((img, *st, Some(UNHASHED)));
                }
                Some(st) => verify.push((
                    img,
                    *st,
                    hash_file(&self.original_path(img.rel.as_str())).ok(),
                )),
                None => missing.push(img),
            }
        }
        (verify, missing)
    }

    /// Re-sync sidecars, then check every original against the index:
    ///
    /// - a file whose size/mtime/inode changed since its content was last
    ///   verified is re-hashed; different content means the image is
    ///   `replaced` (its edits must not apply to someone else's pixels);
    /// - an image not at its path is searched for among untracked files of
    ///   the same size by BLAKE3 and relinked (sidecars follow), unless the
    ///   destination is taken (`conflicts`).
    ///
    /// Hashing happens without the write lock (other handles keep saving);
    /// every decision is re-checked under the lock before it is applied.
    #[allow(clippy::too_many_lines)] // two phases, each re-checked under the lock
    pub fn rescan(&mut self) -> Result<RescanReport> {
        self.sync_index()?;
        let disk = self.scan_disk(true)?;
        let images = self.tracked()?;
        let known: HashSet<&str> = images.iter().map(|i| i.rel.as_str()).collect();
        let mut untracked: BTreeMap<String, FileStat> = disk
            .originals
            .iter()
            .filter(|(rel, _)| !known.contains(rel.as_str()))
            .map(|(r, s)| (r.clone(), *s))
            .collect();

        // Phase 1, unlocked: hash what needs hashing.
        let (verify, mut missing) = self.classify(&images, &disk.originals);
        missing.retain(|i| !disk.unknown(i.rel.as_str()));
        let wanted: HashSet<u64> = missing
            .iter()
            .filter(|i| i.hash != UNHASHED)
            .map(|i| i.size)
            .collect();
        let mut by_hash: HashMap<FileHash, Vec<(String, FileStat)>> = HashMap::new();
        for (rel, st) in &untracked {
            if wanted.contains(&st.size)
                && let Ok(h) = hash_file(&self.original_path(rel))
            {
                by_hash.entry(h).or_default().push((rel.clone(), *st));
            }
        }

        // Phase 2, locked: re-check and apply.
        let tx = self.write_lock()?;
        let mut report = RescanReport {
            seen: disk.seen,
            ..RescanReport::default()
        };
        let unchanged =
            |rel: &str, st: &FileStat| stat_of(&self.original_path(rel)).as_ref() == Some(st);
        let current_rel = |id: ImageId| -> Result<Option<String>> {
            Ok(tx
                .query_row("SELECT rel_path FROM images WHERE id = ?", [id.0], |r| {
                    r.get(0)
                })
                .optional()?)
        };
        for (img, st, hash) in verify {
            if !unchanged(img.rel.as_str(), &st)
                || current_rel(img.id)?.as_deref() != Some(img.rel.as_str())
            {
                continue; // changed under us; the next rescan looks again
            }
            match hash {
                Some(h) if h == img.hash => record_stat(&tx, img.id.0, st)?,
                Some(h) => {
                    tx.execute("UPDATE images SET missing = 1 WHERE id = ?", [img.id.0])?;
                    report.replaced.push((img.id, img.rel.clone()));
                    report.missing.push((img.id, img.rel.clone()));
                    // The foreign content may be another image's original.
                    by_hash
                        .entry(h)
                        .or_default()
                        .push((img.rel.as_str().to_owned(), st));
                }
                // Unreadable is not "different": keep everything, look again later.
                None => report.unreadable.push((img.id, img.rel.clone())),
            }
        }
        // Identical-content candidates are paired by name only when that is
        // unambiguous; otherwise the images stay missing (never swapped).
        let mut chosen: HashMap<ImageId, (String, FileStat)> = HashMap::new();
        let mut hashed_missing: HashMap<FileHash, Vec<&Tracked>> = HashMap::new();
        for img in missing.iter().filter(|i| i.hash != UNHASHED) {
            hashed_missing.entry(img.hash).or_default().push(img);
        }
        for (h, imgs) in &hashed_missing {
            let Some(cands) = by_hash.get(h) else {
                continue;
            };
            let m: Vec<&str> = imgs.iter().map(|i| i.rel.as_str()).collect();
            let c: Vec<&str> = cands.iter().map(|x| x.0.as_str()).collect();
            for (i, j) in crate::identity::pair_by_name(&m, &c) {
                chosen.insert(imgs[i].id, cands[j].clone());
            }
        }
        for img in missing {
            if current_rel(img.id)?.as_deref() != Some(img.rel.as_str()) {
                continue;
            }
            // An unhashed original can only be followed by its exact stat
            // (a rename keeps size, mtime and inode).
            let candidate = if img.hash == UNHASHED {
                img.verified.and_then(|v| {
                    untracked
                        .iter()
                        .find(|(_, s)| **s == v)
                        .map(|(r, s)| (r.clone(), *s))
                })
            } else {
                chosen.remove(&img.id)
            };
            let found = candidate
                .filter(|(rel, st)| unchanged(rel, st))
                .and_then(|(rel, st)| RelPath::new(&rel).ok().map(|r| (r, st)));
            let Some((to, st)) = found else {
                tx.execute("UPDATE images SET missing = 1 WHERE id = ?", [img.id.0])?;
                report.missing.push((img.id, img.rel.clone()));
                continue;
            };
            let sidecars = self.image_sidecars(img.id)?;
            let ops = self.relocation_ops(&img.rel, &to, &sidecars, false);
            if !self.destination_free(&to, &sidecars)? || run_ops(&ops).is_err() {
                tx.execute("UPDATE images SET missing = 1 WHERE id = ?", [img.id.0])?;
                report.conflicts.push((img.id, to));
                report.missing.push((img.id, img.rel.clone()));
                continue;
            }
            Self::record_relocation(&tx, img.id, &to, &sidecars)?;
            record_stat(&tx, img.id.0, st)?;
            untracked.remove(to.as_str());
            report.relinked.push(Moved {
                image: img.id,
                from: img.rel.clone(),
                to,
            });
        }
        tx.commit()?;
        report.missing.sort_by(|a, b| a.1.cmp(&b.1));
        report.untracked = untracked.into_keys().collect();
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_names() {
        assert_eq!(numbered("a.ARW", 2), "a-2.ARW");
        assert_eq!(numbered("a.b.ARW", 3), "a.b-3.ARW");
        assert_eq!(numbered("noext", 2), "noext-2");
        assert_eq!(numbered(".hidden", 2), ".hidden-2");
    }

    #[test]
    fn run_ops_rolls_back_and_never_overwrites() {
        let d = tempfile::tempdir().unwrap();
        let p = |n: &str| d.path().join(n);
        fs::write(p("a"), b"a").unwrap();
        fs::write(p("b"), b"b").unwrap();
        fs::write(p("taken"), b"keep").unwrap();
        let ops = vec![(p("a"), p("sub/a2")), (p("b"), p("taken"))];
        assert!(run_ops(&ops).is_err());
        assert_eq!(fs::read(p("a")).unwrap(), b"a", "first rename rolled back");
        assert_eq!(fs::read(p("taken")).unwrap(), b"keep");
        assert!(!p("sub/a2").exists());
        run_ops(&[(p("a"), p("x/y/a"))]).unwrap();
        assert!(p("x/y/a").exists());
    }

    #[test]
    fn hashes_match_blake3() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("f");
        let data = vec![7u8; 3 << 20];
        fs::write(&f, &data).unwrap();
        assert_eq!(
            hash_file(&f).unwrap(),
            FileHash(*blake3::hash(&data).as_bytes())
        );
        assert!(hash_file(&d.path().join("nope")).is_err());
    }
}
