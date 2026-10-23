//! Bounded random-access byte sources.
//!
//! Every read is checked against the source length *before* anything is
//! allocated, so a hostile header value (offset or count) can never make us
//! allocate more than the file actually holds.

use std::borrow::Cow;
use std::fs::File;
use std::path::Path;

use crate::RawError;

/// Random access to a byte range. Offsets are absolute (file) offsets.
pub(crate) trait Source: Sync {
    /// Total length of the addressable range, as an absolute end offset.
    fn end(&self) -> u64;

    /// `len` bytes starting at absolute offset `off`.
    /// Fails with [`RawError::Truncated`] if the range is not fully inside the source.
    fn read(&self, off: u64, len: u64) -> Result<Cow<'_, [u8]>, RawError>;

    /// Fill `buf` from absolute offset `off` (same bounds rules as [`Source::read`]).
    fn read_into(&self, off: u64, buf: &mut [u8]) -> Result<(), RawError> {
        buf.copy_from_slice(&self.read(off, buf.len() as u64)?);
        Ok(())
    }
}

/// Checks `off..off+len` lies inside `base..end` without overflowing.
pub(crate) fn check_range(off: u64, len: u64, base: u64, end: u64) -> Result<(), RawError> {
    match off.checked_add(len) {
        Some(stop) if off >= base && stop <= end => Ok(()),
        _ => Err(RawError::Truncated(format!(
            "range {off}+{len} outside {base}..{end}"
        ))),
    }
}

/// An in-memory buffer whose first byte sits at absolute offset `base`.
///
/// `base` lets a decrypted or extracted block keep resolving the absolute
/// offsets its IFD entries contain (Sony SR2 sub-IFD).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SliceSource<'a> {
    pub data: &'a [u8],
    pub base: u64,
}

impl<'a> SliceSource<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, base: 0 }
    }
}

impl Source for SliceSource<'_> {
    fn end(&self) -> u64 {
        self.base + self.data.len() as u64
    }

    fn read(&self, off: u64, len: u64) -> Result<Cow<'_, [u8]>, RawError> {
        check_range(off, len, self.base, self.end())?;
        let start = (off - self.base) as usize;
        Ok(Cow::Borrowed(&self.data[start..start + len as usize]))
    }
}

/// A file read with positional reads (`pread`): no seeking, no locking,
/// safe to share across rayon workers. (mmap would need `unsafe`, which the
/// workspace denies outside `ds-ffi` / `ds-pipeline::gpu`.)
#[derive(Debug)]
pub(crate) struct FileSource {
    file: File,
    len: u64,
}

impl FileSource {
    pub fn open(path: &Path) -> Result<Self, RawError> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Self { file, len })
    }

    /// The open file (read positionally only, so its offset is free for others).
    #[cfg_attr(feature = "rawler", allow(dead_code))]
    pub fn file(&self) -> &File {
        &self.file
    }
}

impl Source for FileSource {
    fn end(&self) -> u64 {
        self.len
    }

    fn read(&self, off: u64, len: u64) -> Result<Cow<'_, [u8]>, RawError> {
        check_range(off, len, 0, self.len)?;
        let mut buf = vec![0u8; len as usize];
        self.read_into(off, &mut buf)?;
        Ok(Cow::Owned(buf))
    }

    fn read_into(&self, off: u64, buf: &mut [u8]) -> Result<(), RawError> {
        check_range(off, buf.len() as u64, 0, self.len)?;
        std::os::unix::fs::FileExt::read_exact_at(&self.file, buf, off)?;
        Ok(())
    }
}

/// Another source seen from `delta` on: offset 0 here is `delta` there.
/// Offsets stored relative to an embedded structure (an Olympus maker note,
/// the TIFF inside a JPEG's EXIF block, a CR3 box) then resolve unchanged.
pub(crate) struct Shifted<'a> {
    pub inner: &'a dyn Source,
    pub delta: u64,
}

impl Source for Shifted<'_> {
    fn end(&self) -> u64 {
        self.inner.end().saturating_sub(self.delta)
    }

    fn read(&self, off: u64, len: u64) -> Result<Cow<'_, [u8]>, RawError> {
        let abs = off
            .checked_add(self.delta)
            .ok_or_else(|| RawError::Truncated(format!("offset {off}+{}", self.delta)))?;
        self.inner.read(abs, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifted_source_rebases_offsets() {
        let data = [1u8, 2, 3, 4, 5];
        let s = SliceSource::new(&data);
        let sh = Shifted {
            inner: &s,
            delta: 2,
        };
        assert_eq!(sh.end(), 3);
        assert_eq!(&*sh.read(0, 3).unwrap(), &[3, 4, 5]);
        assert!(matches!(sh.read(1, 3), Err(RawError::Truncated(_))));
        assert!(matches!(sh.read(u64::MAX, 1), Err(RawError::Truncated(_))));
        let past = Shifted {
            inner: &s,
            delta: 9,
        };
        assert_eq!(past.end(), 0);
    }

    #[test]
    fn slice_source_respects_base_and_bounds() {
        let data = [1u8, 2, 3, 4, 5];
        let s = SliceSource {
            data: &data,
            base: 100,
        };
        assert_eq!(s.end(), 105);
        assert_eq!(&*s.read(101, 3).unwrap(), &[2, 3, 4]);
        assert_eq!(&*s.read(105, 0).unwrap(), &[] as &[u8]);
        assert!(matches!(s.read(99, 1), Err(RawError::Truncated(_))));
        assert!(matches!(s.read(104, 2), Err(RawError::Truncated(_))));
        assert!(matches!(s.read(u64::MAX, 2), Err(RawError::Truncated(_))));
        let mut b = [0u8; 2];
        s.read_into(103, &mut b).unwrap();
        assert_eq!(b, [4, 5]);
        assert!(s.read_into(104, &mut b).is_err());
    }

    #[test]
    fn file_source_reads_and_rejects_out_of_range() {
        let dir = std::env::temp_dir().join(format!("ds-raw-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.bin");
        std::fs::write(&p, b"hello world").unwrap();
        let f = FileSource::open(&p).unwrap();
        assert_eq!(f.end(), 11);
        assert_eq!(&*f.read(6, 5).unwrap(), b"world");
        // Huge length must fail before allocating.
        assert!(matches!(
            f.read(0, u64::MAX / 2),
            Err(RawError::Truncated(_))
        ));
        let mut b = [0u8; 4];
        assert!(f.read_into(9, &mut b).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(FileSource::open(&p), Err(RawError::Io(_))));
    }
}
