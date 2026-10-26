//! Duplicate detection: size → partial hash → full BLAKE3, cheapest first.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use lt_core::FileHash;
use serde::{Deserialize, Serialize};

/// Bytes hashed from each end of a file for [`PartialHash`].
pub const PARTIAL_SPAN: u64 = 64 * 1024;

/// BLAKE3 of `size (u64 LE) ‖ first 64 KiB ‖ last 64 KiB` (the tail never
/// overlaps the head). Cheap to compute off a card (two small reads) and a
/// strong pre-filter: RAWs of equal size still differ in their headers
/// (timestamps, counters) and tails (embedded preview).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PartialHash(pub [u8; 32]);

impl PartialHash {
    pub fn to_hex(&self) -> String {
        FileHash(self.0).to_hex()
    }
}

/// Incremental [`PartialHash`] computed from the same stream as the full hash,
/// so the copy pass produces both without extra reads.
#[derive(Debug)]
pub(crate) struct PartialHasher {
    size: u64,
    head: Vec<u8>,
    tail: Vec<u8>,
    tail_start: u64,
}

impl PartialHasher {
    pub(crate) fn new(size: u64) -> Self {
        let head_len = size.min(PARTIAL_SPAN);
        let tail_start = size.saturating_sub(PARTIAL_SPAN).max(head_len);
        Self {
            size,
            head: Vec::with_capacity(head_len as usize),
            tail: Vec::with_capacity((size - tail_start) as usize),
            tail_start,
        }
    }

    /// Feed `chunk`, which starts at byte `offset` of the file.
    pub(crate) fn update(&mut self, offset: u64, chunk: &[u8]) {
        let end = offset + chunk.len() as u64;
        let head_end = self.size.min(PARTIAL_SPAN);
        if offset < head_end {
            let take = (head_end.min(end) - offset) as usize;
            self.head.extend_from_slice(&chunk[..take]);
        }
        if end > self.tail_start {
            let from = self.tail_start.max(offset);
            let to = end.min(self.size);
            if to > from {
                self.tail
                    .extend_from_slice(&chunk[(from - offset) as usize..(to - offset) as usize]);
            }
        }
    }

    pub(crate) fn finish(&self) -> PartialHash {
        partial_from_parts(self.size, &self.head, &self.tail)
    }
}

fn partial_from_parts(size: u64, head: &[u8], tail: &[u8]) -> PartialHash {
    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    h.update(head);
    h.update(tail);
    PartialHash(*h.finalize().as_bytes())
}

/// Compute the [`PartialHash`] of a file with two small reads. Callers that
/// already have the index's copy of a file use this to fill the index.
pub fn partial_hash(path: &Path) -> io::Result<PartialHash> {
    let mut f = crate::fsio::open_source(path)?;
    let size = f.metadata()?.len();
    partial_hash_reader(&mut f, size)
}

pub(crate) fn partial_hash_reader<R: Read + Seek>(r: &mut R, size: u64) -> io::Result<PartialHash> {
    let head_len = size.min(PARTIAL_SPAN);
    let tail_start = size.saturating_sub(PARTIAL_SPAN).max(head_len);
    let mut head = vec![0; head_len as usize];
    r.read_exact(&mut head)?;
    let mut tail = vec![0; (size - tail_start) as usize];
    if !tail.is_empty() {
        r.seek(SeekFrom::Start(tail_start))?;
        r.read_exact(&mut tail)?;
    }
    Ok(partial_from_parts(size, &head, &tail))
}

#[cfg(test)]
thread_local! {
    /// How many whole-file hashes this thread has started (tests assert the
    /// planner avoids them).
    pub(crate) static FULL_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Full BLAKE3 of a file (streamed).
pub fn full_hash(path: &Path) -> io::Result<FileHash> {
    #[cfg(test)]
    FULL_HASHES.with(|c| c.set(c.get() + 1));
    let mut h = blake3::Hasher::new();
    h.update_reader(crate::fsio::open_source(path)?)?;
    Ok(FileHash(*h.finalize().as_bytes()))
}

/// What the library already holds, injected by the session/global index.
///
/// Queried cheapest-first: `has_size` for every primary file, `matches` only
/// when the size is known, and the caller hashes the full file only when
/// `matches` returns candidates.
pub trait DedupeLookup: Send + Sync {
    /// Is any known original exactly `size` bytes?
    fn has_size(&self, size: u64) -> bool;
    /// Known originals with this size and partial hash, with where they live.
    fn matches(&self, size: u64, partial: &PartialHash) -> Vec<KnownCopy>;
}

/// An original the library says it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownCopy {
    pub hash: FileHash,
    /// Where the library's copy is. A copy without a location, or whose file
    /// is missing/changed (session deleted, torn by a crash), does **not**
    /// count as a duplicate: the card file is imported again.
    pub path: Option<PathBuf>,
}

/// In-memory [`DedupeLookup`], for tests and for de-duplicating within one run.
#[derive(Debug, Default)]
pub struct MemoryDedupe {
    map: RwLock<HashMap<u64, Vec<(PartialHash, KnownCopy)>>>,
}

impl MemoryDedupe {
    pub fn insert(&self, size: u64, partial: PartialHash, copy: KnownCopy) {
        self.map
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(size)
            .or_default()
            .push((partial, copy));
    }
}

impl DedupeLookup for MemoryDedupe {
    fn has_size(&self, size: u64) -> bool {
        self.map
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&size)
    }

    fn matches(&self, size: u64, partial: &PartialHash) -> Vec<KnownCopy> {
        let map = self
            .map
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(&size)
            .map(|v| {
                v.iter()
                    .filter(|(p, _)| p == partial)
                    .map(|(_, c)| c.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Can `copy` stand in for the card file at `src` (on device `src_dev`)?
/// Only a copy in the library proper counts: not the card file itself, not
/// anything else on the card's device, not on removable media (another card,
/// a USB stick), and not in a Trash (`.Trash`, `.Trashes`, a session's
/// `Trash/`).
fn trusted_copy(copy: &Path, src: &Path, src_dev: u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let c = canon(copy);
    if c == canon(src) {
        return false;
    }
    if c.components().any(|x| {
        let s = x.as_os_str();
        s == ".Trash" || s == ".Trashes" || s == "Trash"
    }) {
        return false;
    }
    match std::fs::metadata(&c) {
        Ok(m) if m.dev() == src_dev => false,
        Ok(_) => !lt_fsutil::is_removable(&c),
        Err(_) => false,
    }
}

/// Is the file at `path` (of `size` bytes) already safely in the library?
///
/// Cheapest first: nothing is read when the size is unknown, 128 KiB for the
/// partial hash, and the full file only for real candidates. A match counts
/// only if the library's copy exists, is not one of `ignore` (this import's
/// own destinations), has the same size, **and re-hashes to the same BLAKE3**
/// — so a zero-filled or deleted "existing copy" never hides a card file —
/// and only if it is a trusted library location (see `trusted_copy`).
pub fn find_duplicate(
    path: &Path,
    size: u64,
    lookup: &dyn DedupeLookup,
    ignore: &[PathBuf],
) -> io::Result<Option<FileHash>> {
    if !lookup.has_size(size) {
        return Ok(None);
    }
    let mut f = crate::fsio::open_source(path)?;
    let src_dev = {
        use std::os::unix::fs::MetadataExt;
        f.metadata()?.dev()
    };
    let partial = partial_hash_reader(&mut f, size)?;
    let known: Vec<KnownCopy> = lookup
        .matches(size, &partial)
        .into_iter()
        .filter(|k| {
            k.path
                .as_ref()
                .is_some_and(|p| !ignore.contains(p) && trusted_copy(p, path, src_dev))
        })
        .collect();
    if known.is_empty() {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(0))?;
    let mut h = blake3::Hasher::new();
    h.update_reader(f)?;
    let full = FileHash(*h.finalize().as_bytes());
    for k in known.iter().filter(|k| k.hash == full) {
        let Some(p) = &k.path else { continue };
        let present = std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() == size);
        if present && full_hash(p).is_ok_and(|h| h == full) {
            return Ok(Some(full));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    #[test]
    fn streaming_partial_matches_seeking_partial_for_all_shapes() {
        let span = PARTIAL_SPAN as usize;
        for n in [
            0,
            1,
            100,
            span - 1,
            span,
            span + 1,
            2 * span - 1,
            2 * span,
            2 * span + 7,
            5 * span + 3,
        ] {
            let d = data(n);
            let want = partial_hash_reader(&mut Cursor::new(&d), n as u64).unwrap();
            for chunk in [1usize, 4096, 65_537, 1 << 20] {
                let mut p = PartialHasher::new(n as u64);
                let mut off = 0;
                for c in d.chunks(chunk) {
                    p.update(off, c);
                    off += c.len() as u64;
                }
                assert_eq!(p.finish(), want, "n={n} chunk={chunk}");
            }
        }
    }

    #[test]
    fn partial_distinguishes_head_and_tail_but_not_middle() {
        let n = 3 * PARTIAL_SPAN as usize;
        let a = data(n);
        let mut head = a.clone();
        head[0] ^= 1;
        let mut tail = a.clone();
        tail[n - 1] ^= 1;
        let mut mid = a.clone();
        mid[n / 2] ^= 1;
        let h = |d: &Vec<u8>| partial_hash_reader(&mut Cursor::new(d), n as u64).unwrap();
        assert_ne!(h(&a), h(&head));
        assert_ne!(h(&a), h(&tail));
        assert_eq!(
            h(&a),
            h(&mid),
            "middle bytes are not sampled (full hash settles it)"
        );
        assert_eq!(h(&a).to_hex().len(), 64);
    }

    #[test]
    fn untrusted_copies_never_count() {
        // Card and "library" on one device (as a copy on the card itself, or
        // a folder imported from the same disk): not trusted.
        let t = tempfile::tempdir().unwrap();
        let n = 3 * PARTIAL_SPAN as usize;
        let card = t.path().join("card.ARW");
        std::fs::write(&card, data(n)).unwrap();
        let (pa, ha) = (partial_hash(&card).unwrap(), full_hash(&card).unwrap());
        let dev = std::os::unix::fs::MetadataExt::dev(&std::fs::metadata(&card).unwrap());
        let same_dev = t.path().join("lib/copy.ARW");
        std::fs::create_dir_all(same_dev.parent().unwrap()).unwrap();
        std::fs::write(&same_dev, data(n)).unwrap();
        for (p, why) in [
            (card.clone(), "the card file itself"),
            (same_dev.clone(), "same device as the card"),
            (t.path().join("x/.Trash/copy.ARW"), ".Trash"),
            (t.path().join("Sess/Trash/copy.ARW"), "session Trash"),
            (t.path().join("gone.ARW"), "missing"),
        ] {
            assert!(!trusted_copy(&p, &card, dev), "{why}");
        }
        // A different device whose kind is unknown (/dev is devfs, which
        // `diskutil` cannot identify) might be a card: not trusted.
        assert!(!trusted_copy(Path::new("/dev/null"), &card, dev));
        // A fixed disk other than the card's (the card pretends to be on
        // another device here) passes the location rules.
        assert!(trusted_copy(&same_dev, &card, dev ^ 1));
        let lookup = MemoryDedupe::default();
        lookup.insert(
            n as u64,
            pa,
            KnownCopy {
                hash: ha,
                path: Some(same_dev),
            },
        );
        lookup.insert(
            n as u64,
            pa,
            KnownCopy {
                hash: ha,
                path: Some(card.clone()),
            },
        );
        assert_eq!(find_duplicate(&card, n as u64, &lookup, &[]).unwrap(), None);
    }

    /// The positive path needs the card on another device: a FAT32 image.
    #[test]
    fn find_duplicate_escalates_and_verifies_the_existing_copy() {
        let t = tempfile::tempdir().unwrap();
        let n = 3 * PARTIAL_SPAN as usize;
        let lookup = MemoryDedupe::default();
        assert_eq!(
            find_duplicate(&t.path().join("gone"), n as u64, &lookup, &[]).unwrap(),
            None,
            "unknown size reads nothing"
        );
        let Some(img) =
            lt_fsutil::testvol::require(t.path(), lt_fsutil::testvol::Fs::Fat32, 64, "CARD")
        else {
            return;
        };
        let a = img.mount_point().join("a.ARW");
        std::fs::write(&a, data(n)).unwrap();
        let lib = t.path().join("library-copy");
        std::fs::write(&lib, data(n)).unwrap();
        let mut middle = data(n);
        middle[n / 2] ^= 1;
        let b = img.mount_point().join("b.ARW");
        std::fs::write(&b, &middle).unwrap();
        let (pa, ha) = (partial_hash(&a).unwrap(), full_hash(&a).unwrap());
        let copy = |p: Option<&Path>| KnownCopy {
            hash: ha,
            path: p.map(Path::to_path_buf),
        };

        assert_eq!(find_duplicate(&a, n as u64, &lookup, &[]).unwrap(), None);
        lookup.insert(n as u64, pa, copy(Some(&lib)));
        assert_eq!(
            find_duplicate(&a, n as u64, &lookup, &[]).unwrap(),
            Some(ha)
        );
        // Same size + partial, different middle: the full hash rejects it.
        assert_eq!(find_duplicate(&b, n as u64, &lookup, &[]).unwrap(), None);
        // Our own destination never counts as "already imported".
        assert_eq!(
            find_duplicate(&a, n as u64, &lookup, std::slice::from_ref(&lib)).unwrap(),
            None
        );
        // Existing copy zero-filled (power loss) → not a duplicate.
        std::fs::write(&lib, vec![0u8; n]).unwrap();
        assert_eq!(find_duplicate(&a, n as u64, &lookup, &[]).unwrap(), None);
        // Existing copy gone (session deleted) → not a duplicate.
        std::fs::remove_file(&lib).unwrap();
        assert_eq!(find_duplicate(&a, n as u64, &lookup, &[]).unwrap(), None);
        // A copy with no known location cannot be verified → not a duplicate.
        let lk = MemoryDedupe::default();
        lk.insert(n as u64, pa, copy(None));
        assert_eq!(find_duplicate(&a, n as u64, &lk, &[]).unwrap(), None);
        // Different partial with a known size.
        let c = img.mount_point().join("c.ARW");
        std::fs::write(&c, vec![0u8; n]).unwrap();
        assert_eq!(find_duplicate(&c, n as u64, &lookup, &[]).unwrap(), None);
        // Vanished card file with a known size is an I/O error.
        assert!(find_duplicate(&img.mount_point().join("gone"), n as u64, &lookup, &[]).is_err());
    }
}
