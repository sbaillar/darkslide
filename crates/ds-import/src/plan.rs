//! Turn scanned candidates into a concrete, collision-free copy plan.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::NaiveDateTime;
use ds_template::{Template, Vars, collision_key, sanitize_component, with_suffix};

use crate::ImportError;
use crate::dedupe::DedupeLookup;
use crate::journal::{EntryState, Journal, JournalEntry, SourceKey, VolumeCache, journal_id};
use crate::scan::{Candidate, ScanError, ScanReport, SkippedFile, SourceFile};

/// Default number of files copied at once. Card readers slow down with more.
pub const DEFAULT_CONCURRENCY: usize = 3;
/// Default read/write chunk.
pub const DEFAULT_CHUNK: usize = 4 << 20;

/// Capture metadata for naming. Supplied by `ds-raw` at integration time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureMeta {
    pub captured_at: Option<NaiveDateTime>,
    pub camera: Option<String>,
}

/// Reads capture metadata for a source file. Only called when a template uses
/// `{date}`, `{time}` or `{camera}`. A missing date falls back to the file mtime.
pub trait MetaProvider: Send + Sync {
    fn capture_meta(&self, path: &Path) -> CaptureMeta;
}

/// A [`MetaProvider`] that knows nothing (mtime fallback for dates).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMeta;

impl MetaProvider for NoMeta {
    fn capture_meta(&self, _: &Path) -> CaptureMeta {
        CaptureMeta::default()
    }
}

/// User choices for an import.
#[derive(Clone)]
pub struct PlanOptions {
    /// Destination root, e.g. `<session>/Capture`.
    pub dest: PathBuf,
    /// Optional subfolder template under `dest`, e.g. `{date:%Y-%m-%d}`.
    pub subfolder: Option<String>,
    /// Optional rename template, e.g. `{camera}_{date}_{seq:4}`. `None` keeps names.
    pub rename: Option<String>,
    /// Optional second destination (backup copy) written in the same pass,
    /// mirroring the same relative paths.
    pub backup_dest: Option<PathBuf>,
    /// Skip files the library already holds.
    pub dedupe: Option<Arc<dyn DedupeLookup>>,
    /// Files copied concurrently (default 3).
    pub concurrency: usize,
    /// First `{seq}` value (default 1).
    pub seq_start: u64,
    /// After copying, re-read the *source* with the page cache bypassed and
    /// compare hashes, catching a card reader that returned bad bytes once
    /// (default off: it doubles card reads).
    pub verify_source_reread: bool,
}

impl PlanOptions {
    pub fn new(dest: impl Into<PathBuf>) -> Self {
        Self {
            dest: dest.into(),
            subfolder: None,
            rename: None,
            backup_dest: None,
            dedupe: None,
            concurrency: DEFAULT_CONCURRENCY,
            seq_start: 1,
            verify_source_reread: false,
        }
    }
}

impl fmt::Debug for PlanOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanOptions")
            .field("dest", &self.dest)
            .field("subfolder", &self.subfolder)
            .field("rename", &self.rename)
            .field("backup_dest", &self.backup_dest)
            .field("dedupe", &self.dedupe.is_some())
            .field("concurrency", &self.concurrency)
            .field("seq_start", &self.seq_start)
            .field("verify_source_reread", &self.verify_source_reread)
            .finish()
    }
}

/// One source file and where it goes (relative to the destination roots).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub src: SourceFile,
    pub rel: PathBuf,
    /// Journal identity (volume UUID + volume-relative path + size + mtime).
    pub key: SourceKey,
    /// Mount point of the source volume (to tell "card removed" from a bad file).
    pub mount: PathBuf,
}

/// A photo: the primary file first, then its sidecars, all sharing one new stem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedItem {
    pub files: Vec<PlannedFile>,
}

/// A resolved import. Build with [`ImportPlan::new`], then [`crate::run`].
#[derive(Clone)]
pub struct ImportPlan {
    pub dest: PathBuf,
    /// Journal file id (hash of the source volume UUIDs).
    pub journal_id: String,
    pub backup_dest: Option<PathBuf>,
    pub items: Vec<PlannedItem>,
    pub dedupe: Option<Arc<dyn DedupeLookup>>,
    pub concurrency: usize,
    pub chunk_size: usize,
    pub verify_source_reread: bool,
    /// Unreadable parts of the card (from the scan): the import can never be
    /// complete while this is non-empty.
    pub scan_errors: Vec<ScanError>,
    /// Card files deliberately not imported (from the scan).
    pub skipped: Vec<SkippedFile>,
}

impl fmt::Debug for ImportPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportPlan")
            .field("dest", &self.dest)
            .field("backup_dest", &self.backup_dest)
            .field("items", &self.items.len())
            .field("dedupe", &self.dedupe.is_some())
            .field("concurrency", &self.concurrency)
            .finish_non_exhaustive()
    }
}

impl ImportPlan {
    /// Plan everything a [`crate::scan_volume`] found, carrying its errors and
    /// skipped files into the report so "complete" means the whole card.
    pub fn from_scan(
        scan: &ScanReport,
        opts: PlanOptions,
        meta: &dyn MetaProvider,
    ) -> Result<Self, ImportError> {
        let mut plan = Self::new(&scan.candidates, opts, meta)?;
        plan.scan_errors.clone_from(&scan.errors);
        plan.skipped.clone_from(&scan.skipped_unknown);
        Ok(plan)
    }

    /// Resolve names for every candidate.
    ///
    /// * Names come from the rename template (or the original name), NFC.
    /// * Collisions — with files already on disk in either destination, or
    ///   between two candidates of this plan (compared case- and
    ///   normalization-insensitively, like APFS) — get `-1`, `-2`, … on the
    ///   whole group so a RAW and its JPG keep matching stems. Nothing is ever
    ///   overwritten.
    /// * If `dest` holds an unfinished import journal, files it already
    ///   planned keep their recorded names, so a resumed import lands in the
    ///   same place instead of making `-1` copies.
    pub fn new(
        candidates: &[Candidate],
        opts: PlanOptions,
        meta: &dyn MetaProvider,
    ) -> Result<Self, ImportError> {
        let subfolder = opts.subfolder.as_deref().map(Template::parse).transpose()?;
        let rename = opts.rename.as_deref().map(Template::parse).transpose()?;
        let needs_meta = subfolder.iter().chain(&rename).any(Template::uses_metadata);
        let journals = Journal::load_all(&opts.dest)?;
        let mut vols = VolumeCache::default();

        let mut taken = reserved_names(&journals);
        let mut volume_ids: Vec<String> = Vec::new();
        let roots: Vec<&Path> = std::iter::once(opts.dest.as_path())
            .chain(opts.backup_dest.as_deref())
            .collect();

        let mut items = Vec::with_capacity(candidates.len());
        for (i, cand) in candidates.iter().enumerate() {
            let seq = opts.seq_start + i as u64;
            let meta = if needs_meta {
                meta.capture_meta(&cand.primary.path)
            } else {
                CaptureMeta::default()
            };
            let datetime = meta
                .captured_at
                .or_else(|| Some(local_time(cand.primary.mtime)));
            let vars = |file: &SourceFile| -> (String, String) {
                let stem = cand.stem().to_owned();
                let ext = file
                    .path
                    .extension()
                    .map(|e| e.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (stem, ext)
            };
            let (stem0, ext0) = vars(&cand.primary);
            let v0 = Vars {
                name: &stem0,
                seq,
                datetime,
                camera: meta.camera.as_deref(),
                ext: &ext0,
            };
            let dir = subfolder
                .as_ref()
                .map(|t| t.render_subfolder(&v0))
                .transpose()?
                .unwrap_or_default();

            let mut files = Vec::with_capacity(1 + cand.sidecars.len());
            let mut fresh = Vec::new(); // (index into files, base name) needing collision resolution
            for f in cand.files() {
                let (mount, volume, vrel) = vols.identify(&f.path);
                if !volume_ids.contains(&volume) {
                    volume_ids.push(volume.clone());
                }
                let key = SourceKey::new(&volume, &vrel, f.size, f.mtime);
                // Ours from an earlier run (even if it failed after placing a
                // torn file): keep its name so the run re-verifies it and
                // quarantines anything that does not match.
                if let Some(e) = prior_entry(&journals, &key, &f.path)
                    .filter(|e| !matches!(e.state, EntryState::Duplicate { .. }))
                {
                    files.push(PlannedFile {
                        src: f.clone(),
                        rel: e.dest_rel.clone(),
                        key: e.key.clone(),
                        mount,
                    });
                    continue;
                }
                let (stem, ext) = vars(f);
                let v = Vars {
                    name: &stem,
                    ext: &ext,
                    ..v0
                };
                let name = match &rename {
                    Some(t) => t.render_file_name(&v),
                    None => sanitize_component(
                        &f.path.file_name().unwrap_or_default().to_string_lossy(),
                    ),
                };
                fresh.push((files.len(), name));
                files.push(PlannedFile {
                    src: f.clone(),
                    rel: PathBuf::new(),
                    key,
                    mount,
                });
            }

            let names: Vec<(&str, &SourceFile)> = fresh
                .iter()
                .map(|(i, n)| (n.as_str(), &files[*i].src))
                .collect();
            let resolved = resolve_names(&dir, &names, &taken, &roots);
            for ((idx, _), rel) in fresh.iter().zip(resolved) {
                taken.insert(collision_key(&rel.to_string_lossy()));
                files[*idx].rel = rel;
            }
            items.push(PlannedItem { files });
        }

        Ok(Self::assemble(opts, items, &volume_ids))
    }

    fn assemble(opts: PlanOptions, items: Vec<PlannedItem>, volume_ids: &[String]) -> Self {
        Self {
            journal_id: journal_id(volume_ids.iter().map(String::as_str)),
            dest: opts.dest,
            // Absolute, so the journal can record (and later accept) it.
            backup_dest: opts
                .backup_dest
                .map(|b| std::path::absolute(&b).unwrap_or(b)),
            items,
            dedupe: opts.dedupe,
            concurrency: opts.concurrency.max(1),
            chunk_size: DEFAULT_CHUNK,
            verify_source_reread: opts.verify_source_reread,
            scan_errors: Vec::new(),
            skipped: Vec::new(),
        }
    }

    pub fn file_count(&self) -> usize {
        self.items.iter().map(|i| i.files.len()).sum()
    }

    pub fn total_bytes(&self) -> u64 {
        self.items
            .iter()
            .flat_map(|i| &i.files)
            .map(|f| f.src.size)
            .sum()
    }
}

/// Names that unfinished imports (any card) placed or will place.
/// Failed/duplicate entries hold nothing on disk, so they reserve nothing.
fn reserved_names(journals: &[(PathBuf, Journal)]) -> HashSet<String> {
    journals
        .iter()
        .flat_map(|(_, j)| &j.entries)
        .filter(|e| matches!(e.state, EntryState::Pending | EntryState::Done { .. }))
        .map(|e| collision_key(&e.dest_rel.to_string_lossy()))
        .collect()
}

/// Pick the smallest suffix `n` (none, `-1`, `-2`, …) that makes every name
/// of a group usable: not taken by this plan (case/normalization-insensitive)
/// and, in every destination root, either absent or already holding a
/// byte-identical copy of that very source file (a finished copy whose
/// journal was lost is recognized by hash, not duplicated). One suffix for
/// the whole group.
fn resolve_names(
    dir: &Path,
    names: &[(&str, &SourceFile)],
    taken: &HashSet<String>,
    roots: &[&Path],
) -> Vec<PathBuf> {
    let mut src_hash: Vec<Option<Option<ds_core::FileHash>>> = vec![None; names.len()];
    let mut src_partial: Vec<Option<Option<crate::dedupe::PartialHash>>> = vec![None; names.len()];
    let mut n = 0u32;
    loop {
        let rels: Vec<PathBuf> = names
            .iter()
            .map(|(name, _)| {
                dir.join(if n == 0 {
                    (*name).to_owned()
                } else {
                    with_suffix(name, n)
                })
            })
            .collect();
        let mut keys = HashSet::new();
        let free = rels.iter().enumerate().all(|(i, rel)| {
            let k = collision_key(&rel.to_string_lossy());
            if taken.contains(&k) || !keys.insert(k) {
                return false;
            }
            roots
                .iter()
                .all(|r| match fs::symlink_metadata(r.join(rel)) {
                    Err(_) => true,
                    Ok(m) if m.is_file() && m.len() == names[i].1.size => {
                        // Two small reads rule out almost every same-size
                        // stranger (uncompressed ARWs are all one size)
                        // before any whole file is read from the card.
                        let dest = r.join(rel);
                        let p = *src_partial[i].get_or_insert_with(|| {
                            crate::dedupe::partial_hash(&names[i].1.path).ok()
                        });
                        if p.is_none() || crate::dedupe::partial_hash(&dest).ok() != p {
                            return false;
                        }
                        let h = *src_hash[i]
                            .get_or_insert_with(|| crate::dedupe::full_hash(&names[i].1.path).ok());
                        h.is_some() && crate::dedupe::full_hash(&dest).ok() == h
                    }
                    Ok(_) => false,
                })
        });
        if free {
            return rels;
        }
        n += 1;
    }
}

/// The journal entry for `key`: exact match, else the same file seen with a
/// whole-hour shifted mtime (FAT stores local time, so a time-zone or DST
/// change between mounts shifts every mtime by 1..14 h). A shifted `Done`
/// entry must also match the source's partial hash.
fn prior_entry<'a>(
    journals: &'a [(PathBuf, Journal)],
    key: &SourceKey,
    src: &Path,
) -> Option<&'a JournalEntry> {
    let entries = || journals.iter().flat_map(|(_, j)| &j.entries);
    if let Some(e) = entries().find(|e| &e.key == key) {
        return Some(e);
    }
    let mut partial: Option<Option<String>> = None;
    entries().find(|e| {
        let k = &e.key;
        let shift = k.mtime_secs.abs_diff(key.mtime_secs);
        let shifted = k.volume == key.volume
            && k.rel == key.rel
            && k.size == key.size
            && k.mtime_nanos == key.mtime_nanos
            && shift != 0
            && shift % 3600 == 0
            && shift <= 14 * 3600;
        shifted
            && match &e.state {
                EntryState::Done { partial: p, .. } => {
                    partial
                        .get_or_insert_with(|| {
                            crate::dedupe::partial_hash(src).ok().map(|h| h.to_hex())
                        })
                        .as_deref()
                        == Some(p.as_str())
                }
                _ => true,
            }
    })
}

fn local_time(t: std::time::SystemTime) -> NaiveDateTime {
    chrono::DateTime::<chrono::Local>::from(t).naive_local()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::FileKind;
    use chrono::NaiveDate;
    use std::time::{Duration, SystemTime};

    fn sf(path: &str, kind: FileKind) -> SourceFile {
        SourceFile {
            path: PathBuf::from(path),
            size: 10,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_780_000_000),
            kind,
        }
    }

    fn cand(stem: &str) -> Candidate {
        Candidate {
            primary: sf(&format!("/card/DCIM/100MSDCF/{stem}.ARW"), FileKind::Raw),
            sidecars: vec![sf(
                &format!("/card/DCIM/100MSDCF/{stem}.JPG"),
                FileKind::Jpeg,
            )],
        }
    }

    struct FixedMeta;
    impl MetaProvider for FixedMeta {
        fn capture_meta(&self, _: &Path) -> CaptureMeta {
            CaptureMeta {
                captured_at: NaiveDate::from_ymd_opt(2026, 8, 6)
                    .unwrap()
                    .and_hms_opt(9, 30, 0),
                camera: Some("ILCE-9".into()),
            }
        }
    }

    fn rels(p: &ImportPlan) -> Vec<String> {
        p.items
            .iter()
            .flat_map(|i| &i.files)
            .map(|f| f.rel.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn keeps_names_by_default() {
        let t = tempfile::tempdir().unwrap();
        let p = ImportPlan::new(
            &[cand("DSC1"), cand("DSC2")],
            PlanOptions::new(t.path()),
            &NoMeta,
        )
        .unwrap();
        assert_eq!(rels(&p), ["DSC1.ARW", "DSC1.JPG", "DSC2.ARW", "DSC2.JPG"]);
        assert_eq!(p.file_count(), 4);
        assert_eq!(p.total_bytes(), 40);
        assert_eq!(p.concurrency, 3);
        assert!(format!("{p:?}").contains("items: 2"));
    }

    #[test]
    fn templates_with_meta_and_seq() {
        let t = tempfile::tempdir().unwrap();
        let mut o = PlanOptions::new(t.path());
        o.subfolder = Some("{date:%Y-%m-%d}".into());
        o.rename = Some("{camera}_{date}_{time}_{seq:4}".into());
        o.seq_start = 41;
        assert!(format!("{o:?}").contains("seq_start: 41"));
        let p = ImportPlan::new(&[cand("DSC1"), cand("DSC2")], o, &FixedMeta).unwrap();
        assert_eq!(
            rels(&p),
            [
                "2026-08-06/ILCE-9_20260806_093000_0041.ARW",
                "2026-08-06/ILCE-9_20260806_093000_0041.JPG",
                "2026-08-06/ILCE-9_20260806_093000_0042.ARW",
                "2026-08-06/ILCE-9_20260806_093000_0042.JPG",
            ]
        );
    }

    #[test]
    fn mtime_fallback_when_no_capture_date() {
        let t = tempfile::tempdir().unwrap();
        let mut o = PlanOptions::new(t.path());
        o.rename = Some("{date:%Y}_{camera}_{name}".into());
        let p = ImportPlan::new(&[cand("DSC1")], o, &NoMeta).unwrap();
        assert_eq!(rels(&p)[0], "2026_unknown_DSC1.ARW");
    }

    #[test]
    fn collisions_with_disk_and_within_plan_suffix_whole_group() {
        let t = tempfile::tempdir().unwrap();
        let backup = tempfile::tempdir().unwrap();
        // Existing file in the *backup* only still counts.
        fs::write(backup.path().join("dsc1.jpg"), b"x").unwrap();
        let mut o = PlanOptions::new(t.path());
        o.backup_dest = Some(backup.path().to_path_buf());
        // Two cards folders with the same file name (100MSDCF, 101MSDCF).
        let mut c2 = cand("DSC1");
        c2.primary.path = PathBuf::from("/card/DCIM/101MSDCF/DSC1.ARW");
        c2.sidecars[0].path = PathBuf::from("/card/DCIM/101MSDCF/DSC1.JPG");
        let p = ImportPlan::new(&[cand("DSC1"), c2], o, &NoMeta).unwrap();
        assert_eq!(
            rels(&p),
            ["DSC1-1.ARW", "DSC1-1.JPG", "DSC1-2.ARW", "DSC1-2.JPG"]
        );
    }

    #[test]
    fn same_size_strangers_are_told_apart_by_partial_then_full_hash() {
        let span = crate::dedupe::PARTIAL_SPAN as usize;
        let card = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let big = |mid: u8, head: u8| {
            let mut v = vec![7u8; span * 4];
            v[0] = head;
            v[span * 2] = mid;
            v
        };
        let src = |name: &str, data: &[u8]| {
            let path = card.path().join(name);
            fs::write(&path, data).unwrap();
            SourceFile {
                path,
                size: data.len() as u64,
                mtime: SystemTime::UNIX_EPOCH,
                kind: FileKind::Raw,
            }
        };
        let resolve = |sf: &SourceFile| {
            let taken = HashSet::new();
            resolve_names(Path::new(""), &[("A.ARW", sf)], &taken, &[dest.path()])
        };
        let a = src("A.ARW", &big(1, 1));
        assert_eq!(resolve(&a), [PathBuf::from("A.ARW")], "nothing there yet");
        // Identical bytes already at the destination: the name is reused.
        fs::write(dest.path().join("A.ARW"), big(1, 1)).unwrap();
        assert_eq!(resolve(&a), [PathBuf::from("A.ARW")]);
        let full = || crate::dedupe::FULL_HASHES.with(std::cell::Cell::get);
        // Same size, different head: caught by the partial hash alone, so no
        // whole file (least of all the one on the card) is read.
        fs::write(dest.path().join("A.ARW"), big(1, 9)).unwrap();
        let before = full();
        assert_eq!(resolve(&a), [PathBuf::from("A-1.ARW")]);
        assert_eq!(full(), before, "partial hashes differ: no full hash");
        // Same size, same head and tail, different middle: needs the full hash.
        fs::write(dest.path().join("A.ARW"), big(9, 1)).unwrap();
        assert_eq!(resolve(&a), [PathBuf::from("A-1.ARW")]);
        assert_eq!(full(), before + 2, "source and destination, once each");
        // Unreadable source: never treated as identical.
        let gone = SourceFile {
            path: card.path().join("missing"),
            ..a.clone()
        };
        assert_eq!(resolve(&gone), [PathBuf::from("A-1.ARW")]);
    }

    #[test]
    fn rename_template_collisions_and_unicode_normalization() {
        let t = tempfile::tempdir().unwrap();
        // NFD name on disk; the plan renders NFC. They must collide.
        fs::write(t.path().join("Cafe\u{301}.ARW"), b"x").unwrap();
        let mut o = PlanOptions::new(t.path());
        o.rename = Some("Caf\u{e9}".into());
        let p = ImportPlan::new(&[cand("A"), cand("B")], o, &NoMeta).unwrap();
        let r = rels(&p);
        // On a normalization-insensitive FS (APFS) the NFD file blocks the plain name.
        assert!(
            r[0] == "Caf\u{e9}-1.ARW" || r[0] == "Caf\u{e9}.ARW",
            "{r:?}"
        );
        assert_ne!(
            collision_key(&r[0]),
            collision_key(&r[2]),
            "two candidates never share a name: {r:?}"
        );
        assert!(r.iter().all(|n| !n.contains('\u{301}')));
    }

    /// The journal only accepts an absolute backup root, so a relative one is
    /// made absolute at plan time (else the journal of a backed-up import
    /// would be set aside as unsafe on the next load).
    #[test]
    fn relative_backup_root_is_made_absolute() {
        let t = tempfile::tempdir().unwrap();
        let mut o = PlanOptions::new(t.path());
        o.backup_dest = Some(PathBuf::from("rel/backup"));
        let p = ImportPlan::new(&[cand("A")], o, &NoMeta).unwrap();
        let b = p.backup_dest.clone().unwrap();
        assert!(b.is_absolute() && b.ends_with("rel/backup"), "{b:?}");
        let jp = crate::journal::journal_path(t.path(), &p.journal_id);
        crate::journal::Journal {
            backup_dest: p.backup_dest,
            ..Default::default()
        }
        .save(&jp)
        .unwrap();
        assert!(crate::journal::Journal::load(&jp).is_ok());
    }

    #[test]
    fn bad_template_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let mut o = PlanOptions::new(t.path());
        o.rename = Some("{bogus}".into());
        assert!(matches!(
            ImportPlan::new(&[cand("A")], o, &NoMeta),
            Err(ImportError::Template(_))
        ));
        let mut o = PlanOptions::new(t.path());
        o.subfolder = Some("../x".into());
        assert!(matches!(
            ImportPlan::new(&[cand("A")], o, &NoMeta),
            Err(ImportError::Template(_))
        ));
    }

    #[test]
    fn journal_names_are_reused_on_resume_and_failed_reserve_nothing() {
        let t = tempfile::tempdir().unwrap();
        let c = cand("DSC1");
        let key_of = |f: &SourceFile| {
            let (_, vol, rel) = VolumeCache::default().identify(&f.path);
            SourceKey::new(&vol, &rel, f.size, f.mtime)
        };
        let entry = |key, rel: &str, state| crate::journal::JournalEntry {
            key,
            dest_rel: PathBuf::from(rel),
            state,
        };
        let j = Journal {
            backup_dest: None,
            version: crate::journal::JOURNAL_VERSION,
            entries: vec![
                entry(
                    key_of(&c.primary),
                    "DSC1.ARW",
                    EntryState::Done {
                        hash: String::new(),
                        partial: String::new(),
                        backup_ok: true,
                    },
                ),
                // A failed file from another card: its name is not held.
                entry(
                    SourceKey::new("OTHER", Path::new("x"), 1, SystemTime::UNIX_EPOCH),
                    "DSC7.ARW",
                    EntryState::Failed {
                        reason: "card removed".into(),
                    },
                ),
            ],
        };
        j.save(&crate::journal::journal_path(t.path(), "old"))
            .unwrap();
        fs::write(t.path().join("DSC1.ARW"), b"done").unwrap();
        let mut c_new = cand("DSC9");
        c_new.primary.path = PathBuf::from("/other/DSC1.ARW"); // different source, same name
        c_new.sidecars.clear();
        let mut c7 = cand("DSC7");
        c7.sidecars.clear();
        let p = ImportPlan::new(&[c, c_new, c7], PlanOptions::new(t.path()), &NoMeta).unwrap();
        assert_eq!(rels(&p), ["DSC1.ARW", "DSC1.JPG", "DSC1-1.ARW", "DSC7.ARW"]);
        assert_eq!(p.items[0].files[0].key, key_of(&cand("DSC1").primary));
        assert_eq!(p.journal_id.len(), 16);
    }
}
