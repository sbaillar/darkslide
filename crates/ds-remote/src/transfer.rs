//! Chunked, resumable receive, shared by the client's pulls and the server's
//! pushed originals. The scheme is `ds-import`'s: chunks go to a hidden
//! `.partial` next to the destination, whose length says where to resume;
//! the finished file is fsynced, re-read with the page cache bypassed and
//! checked against the sender's BLAKE3 ([`ds_import::verify_hash`]), and
//! only then placed with a no-replace rename.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ds_core::FileHash;

use crate::wire::CHUNK;

/// A file being received.
#[derive(Debug)]
pub struct Incoming {
    pub partial: PathBuf,
    pub size: u64,
    pub hash: FileHash,
}

impl Incoming {
    /// The partial for content `hash` in `dir`: named after the content, so
    /// a resumed push of the same file finds it and a different file never does.
    pub fn in_dir(dir: &Path, size: u64, hash: FileHash) -> Self {
        Self {
            partial: ds_import::fsio::partial_path(&dir.join(hash.to_hex())),
            size,
            hash,
        }
    }

    /// The partial for the file that will be placed at `dest`.
    pub fn for_dest(dest: &Path, size: u64, hash: FileHash) -> Self {
        Self {
            partial: ds_import::fsio::partial_path(dest),
            size,
            hash,
        }
    }

    /// Bytes received so far. A partial longer than the file can't be ours
    /// for this content: it is discarded and the transfer starts over.
    pub fn have(&self) -> io::Result<u64> {
        match fs::symlink_metadata(&self.partial) {
            Ok(m) if m.is_file() && m.len() <= self.size => Ok(m.len()),
            Ok(m) if m.is_file() => {
                fs::remove_file(&self.partial)?;
                Ok(0)
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a file", self.partial.display()),
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Append `data`, which must start where the partial ends.
    pub fn write(&self, offset: u64, data: &[u8]) -> io::Result<u64> {
        let have = self.have()?;
        if offset != have || offset + data.len() as u64 > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "chunk at {offset} (+{}) but have {have} of {}",
                    data.len(),
                    self.size
                ),
            ));
        }
        if let Some(dir) = self.partial.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.partial)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(offset + data.len() as u64)
    }

    pub fn complete(&self) -> io::Result<bool> {
        Ok(self.have()? == self.size)
    }

    /// Sync and verify the complete partial. On a mismatch the partial is
    /// removed (it is ours and wrong) and the transfer must start over.
    pub fn verify(&self) -> io::Result<bool> {
        if self.size == 0 && !self.partial.exists() {
            File::create(&self.partial)?;
        }
        ds_fsutil::sync_file(&File::open(&self.partial)?)?;
        let ok = ds_import::verify_hash(&self.partial, self.size, CHUNK)? == self.hash;
        if !ok {
            ds_fsutil::remove_if_exists(&self.partial)?;
        }
        Ok(ok)
    }

    /// Place the verified partial at `dest`, never replacing anything.
    pub fn place(&self, dest: &Path) -> io::Result<()> {
        ds_fsutil::rename_noreplace(&self.partial, dest)
    }
}

/// Whether `path` is a file with content `hash`.
pub fn has_content(path: &Path, hash: &FileHash) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(ds_import::full_hash(path)? == *hash),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(b: &[u8]) -> FileHash {
        FileHash(*blake3::hash(b).as_bytes())
    }

    #[test]
    fn chunks_resume_verify_and_place_without_replacing() {
        let t = tempfile::tempdir().unwrap();
        let data = b"hello resumable world";
        let dest = t.path().join("out.bin");
        let inc = Incoming::for_dest(&dest, data.len() as u64, hash(data));
        assert_eq!(inc.have().unwrap(), 0);
        assert_eq!(inc.write(0, &data[..5]).unwrap(), 5);
        // A dropped connection: a new receiver resumes from the partial.
        let inc = Incoming::for_dest(&dest, data.len() as u64, hash(data));
        assert_eq!(inc.have().unwrap(), 5);
        assert!(
            inc.write(3, b"xx").is_err(),
            "gaps and overlaps are refused"
        );
        assert!(inc.write(5, &[0; 64]).is_err(), "past the end");
        assert!(!inc.complete().unwrap());
        inc.write(5, &data[5..]).unwrap();
        assert!(inc.complete().unwrap());
        assert!(inc.verify().unwrap());
        inc.place(&dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), data);
        assert!(has_content(&dest, &hash(data)).unwrap());
        assert!(!has_content(&dest, &hash(b"other")).unwrap());
        assert!(!has_content(&t.path().join("nope"), &hash(data)).unwrap());
        assert!(!has_content(t.path(), &hash(data)).unwrap(), "a directory");

        // Same name again: never replaced.
        let inc = Incoming::for_dest(&dest, 2, hash(b"zz"));
        inc.write(0, b"zz").unwrap();
        assert!(inc.verify().unwrap());
        assert_eq!(
            inc.place(&dest).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&dest).unwrap(), data);
    }

    #[test]
    fn corrupt_or_oversized_partials_start_over() {
        let t = tempfile::tempdir().unwrap();
        let inc = Incoming::in_dir(t.path(), 4, hash(b"abcd"));
        assert!(
            inc.partial
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(".partial")
        );
        inc.write(0, b"abXd").unwrap();
        assert!(!inc.verify().unwrap(), "hash mismatch");
        assert!(!inc.partial.exists(), "the bad partial is dropped");
        fs::write(&inc.partial, b"too long for it").unwrap();
        assert_eq!(inc.have().unwrap(), 0);
        assert!(!inc.partial.exists());
        fs::create_dir(&inc.partial).unwrap();
        assert!(inc.have().is_err(), "a directory in the way is an error");
        let empty = Incoming::in_dir(t.path(), 0, hash(b""));
        assert!(empty.verify().unwrap(), "an empty file needs no chunks");
    }
}
