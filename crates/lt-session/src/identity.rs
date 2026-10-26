//! Content identity of originals: what makes a file "the same photo".
//!
//! - `file`: BLAKE3 of the whole file (the import's dedupe key; `Sidecar::hash`).
//! - `content`: BLAKE3 of the sensor data only (TIFF-based RAWs; the byte
//!   ranges come from `lt_raw::image_data_ranges`). Metadata writers
//!   (exiftool, geotaggers) change `file` but not `content`, so a whole-file
//!   mismatch with a content match is a *metadata edit*: the photo keeps its
//!   ratings and edits.
//! - `sample`: BLAKE3 of the first and last 64 KiB plus the size, a cheap
//!   check for volumes whose stat can't be trusted (FAT/exFAT reuse inodes).
//!
//! Hashing reads with plain buffered I/O and `F_NOCACHE` (a background pass
//! over a whole card must not evict what the user is looking at), and can be
//! cancelled between chunks.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::Path;

use lt_core::FileHash;

/// Bytes read per chunk.
const CHUNK: usize = 1 << 20;
/// Size of each end of the sample hash.
pub const SAMPLE_BYTES: u64 = 64 << 10;

/// The hashes of one original, computed in one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hashes {
    pub file: FileHash,
    /// `None` when the file has no locatable sensor data (not a TIFF RAW).
    pub content: Option<FileHash>,
    pub sample: FileHash,
}

/// How a verified file compares with what the session knew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Byte-identical.
    Same,
    /// Only metadata changed (sensor data identical): keep everything.
    MetadataEdited,
    /// Different photo.
    Changed,
}

/// Compare fresh hashes with an image's stored `file` / `content` hashes.
pub fn verdict(file: FileHash, content: Option<FileHash>, now: &Hashes) -> Verdict {
    if now.file == file {
        Verdict::Same
    } else if content.is_some() && now.content == content {
        Verdict::MetadataEdited
    } else {
        Verdict::Changed
    }
}

fn open_uncached(path: &Path) -> io::Result<File> {
    let f = File::open(path)?;
    // Best effort: without it the read still works, it just fills the page cache.
    let _ = rustix::fs::fcntl_nocache(&f, true);
    Ok(f)
}

/// BLAKE3 of `path` (whole file), of its bytes inside `ranges` (sorted,
/// non-overlapping `(offset, len)`), and its sample hash, in one sequential
/// read. `cancelled` is polled between chunks (`ErrorKind::Interrupted`).
pub fn hash_original(
    path: &Path,
    ranges: Option<&[(u64, u64)]>,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Hashes> {
    let mut f = open_uncached(path)?;
    let size = f.metadata()?.len();
    let mut whole = blake3::Hasher::new();
    let mut content = ranges.map(|_| blake3::Hasher::new());
    let ranges = ranges.unwrap_or(&[]);
    let mut buf = vec![0u8; CHUNK];
    let mut pos = 0u64;
    loop {
        if cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let chunk = &buf[..n];
        whole.update(chunk);
        if let Some(h) = content.as_mut() {
            let end = pos + n as u64;
            for &(o, l) in ranges {
                let (a, b) = (o.max(pos), (o + l).min(end));
                if a < b {
                    h.update(&chunk[(a - pos) as usize..(b - pos) as usize]);
                }
            }
        }
        pos += n as u64;
    }
    if pos != size {
        return Err(io::Error::other("the file changed size while being read"));
    }
    Ok(Hashes {
        file: FileHash(*whole.finalize().as_bytes()),
        content: content.map(|h| FileHash(*h.finalize().as_bytes())),
        sample: sample_of(&f, size)?,
    })
}

/// The sample hash of `path` (two small reads).
pub fn sample_hash(path: &Path) -> io::Result<FileHash> {
    let f = open_uncached(path)?;
    let size = f.metadata()?.len();
    sample_of(&f, size)
}

fn sample_of(f: &File, size: u64) -> io::Result<FileHash> {
    let mut h = blake3::Hasher::new();
    let head = size.min(SAMPLE_BYTES);
    let tail_from = size.saturating_sub(SAMPLE_BYTES).max(head);
    let mut buf = vec![0u8; SAMPLE_BYTES as usize];
    for (off, len) in [(0, head), (tail_from, size - tail_from)] {
        let b = &mut buf[..len as usize];
        f.read_exact_at(b, off)?;
        h.update(b);
    }
    h.update(&size.to_le_bytes());
    Ok(FileHash(*h.finalize().as_bytes()))
}

/// Trailing number of a file stem (`DSC01234` → 1234, `Trip_002` → 2).
fn trailing_number(stem: &str) -> Option<u64> {
    let digits: String = stem
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.get(..18).unwrap_or(&digits).parse().ok()
}

fn stem(name: &str) -> String {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.rsplit_once('.')
        .map_or(file, |(s, _)| s)
        .to_lowercase()
}

/// How alike two file names are (higher is more alike): the same trailing
/// number counts most, then the longest common substring of the stems.
pub fn name_similarity(a: &str, b: &str) -> u32 {
    let (a, b) = (stem(a), stem(b));
    let number = match (trailing_number(&a), trailing_number(&b)) {
        (Some(x), Some(y)) if x == y => 1000,
        _ => 0,
    };
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut best = 0usize;
    let mut prev = vec![0usize; b.len() + 1];
    for &ca in a {
        let mut cur = vec![0usize; b.len() + 1];
        for (j, &cb) in b.iter().enumerate() {
            if ca == cb {
                cur[j + 1] = prev[j] + 1;
                best = best.max(cur[j + 1]);
            }
        }
        prev = cur;
    }
    number + best as u32
}

/// Pair `missing` names with `found` names that have the same content (every
/// pair is a possible match). Returns `(missing index, found index)` pairs
/// where the choice is unambiguous: a lone pair, or a name-similarity best
/// that is strictly better than every alternative on both sides. The rest
/// is left for the user (never guessed, never swapped).
pub fn pair_by_name(missing: &[&str], found: &[&str]) -> Vec<(usize, usize)> {
    if missing.len() == 1 && found.len() == 1 {
        return vec![(0, 0)];
    }
    let score: Vec<Vec<u32>> = missing
        .iter()
        .map(|m| found.iter().map(|f| name_similarity(m, f)).collect())
        .collect();
    let mut out = Vec::new();
    for (i, row) in score.iter().enumerate() {
        let Some((j, &best)) = row.iter().enumerate().max_by_key(|(_, s)| **s) else {
            continue;
        };
        let unique_in_row = row.iter().enumerate().all(|(k, &s)| k == j || s < best);
        let unique_in_col = score.iter().enumerate().all(|(k, r)| k == i || r[j] < best);
        if unique_in_row && unique_in_col {
            out.push((i, j));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn b3(b: &[u8]) -> FileHash {
        FileHash(*blake3::hash(b).as_bytes())
    }

    #[test]
    fn one_read_gives_file_content_and_sample_hashes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f");
        let data: Vec<u8> = (0..(3 * CHUNK + 777))
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        fs::write(&p, &data).unwrap();
        // Ranges crossing chunk boundaries.
        let ranges = [
            (10u64, 100u64),
            (CHUNK as u64 - 5, 20),
            (2 * CHUNK as u64, 70_000),
        ];
        let h = hash_original(&p, Some(&ranges), &|| false).unwrap();
        assert_eq!(h.file, b3(&data));
        let mut picked = Vec::new();
        for &(o, l) in &ranges {
            picked.extend_from_slice(&data[o as usize..(o + l) as usize]);
        }
        assert_eq!(h.content, Some(b3(&picked)));
        let mut s = data[..SAMPLE_BYTES as usize].to_vec();
        s.extend_from_slice(&data[data.len() - SAMPLE_BYTES as usize..]);
        s.extend_from_slice(&(data.len() as u64).to_le_bytes());
        assert_eq!(h.sample, b3(&s));
        assert_eq!(sample_hash(&p).unwrap(), h.sample);
        let plain = hash_original(&p, None, &|| false).unwrap();
        assert_eq!((plain.file, plain.content), (h.file, None));
    }

    #[test]
    fn small_files_cancellation_and_errors() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("small");
        fs::write(&p, b"abc").unwrap();
        let mut s = b"abc".to_vec();
        s.extend_from_slice(&3u64.to_le_bytes());
        assert_eq!(
            sample_hash(&p).unwrap(),
            b3(&s),
            "no overlap for small files"
        );
        let e = hash_original(&p, None, &|| true).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
        assert!(hash_original(&d.path().join("nope"), None, &|| false).is_err());
        assert!(sample_hash(&d.path().join("nope")).is_err());
    }

    #[test]
    fn verdicts() {
        let h = |b: u8| FileHash([b; 32]);
        let now = Hashes {
            file: h(1),
            content: Some(h(2)),
            sample: h(3),
        };
        assert_eq!(verdict(h(1), None, &now), Verdict::Same);
        assert_eq!(verdict(h(9), Some(h(2)), &now), Verdict::MetadataEdited);
        assert_eq!(verdict(h(9), Some(h(8)), &now), Verdict::Changed);
        assert_eq!(verdict(h(9), None, &now), Verdict::Changed);
    }

    #[test]
    fn name_pairs_never_guess() {
        assert!(name_similarity("Capture/P1.ARW", "Capture/Trip_001.ARW") >= 1000);
        assert!(
            name_similarity("DSC01234.ARW", "DSC01234 copy.ARW")
                > name_similarity("DSC01234.ARW", "DSC09999.ARW")
        );
        assert_eq!(trailing_number("abc"), None);
        assert_eq!(pair_by_name(&["a"], &["zzz"]), vec![(0, 0)], "lone pair");
        assert_eq!(
            pair_by_name(&["P1.ARW", "P2.ARW"], &["Trip_002.ARW", "Trip_001.ARW"]),
            vec![(0, 1), (1, 0)]
        );
        // Two identical files renamed to names that say nothing: ambiguous, left alone.
        assert_eq!(
            pair_by_name(&["P1.ARW", "P2.ARW"], &["x.ARW", "y.ARW"]),
            [] as [(usize, usize); 0]
        );
        // One clear match among two missing.
        assert_eq!(
            pair_by_name(&["IMG_7.ARW", "other.ARW"], &["IMG_7 (1).ARW"]),
            vec![(0, 0)]
        );
    }
}
