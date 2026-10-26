//! A plug-in's files, read once into memory, with the rules every package follows.
//!
//! - Only regular files: no symbolic links, no hard links (a link count above 1).
//! - Names are printable ASCII without `\`, and unique ignoring case (APFS is
//!   case-insensitive; Unicode name forms are avoided altogether).
//! - Dotfiles (`.DS_Store`, Finder's `._*` files, `.git`) are ignored: never hashed, never read.
//! - Limits: [`MAX_FILES`] files, [`MAX_PACKAGE_BYTES`] in all.
//!
//! Everything after reading (hashing, the signature, parsing, compiling) works on these
//! bytes, so what was verified is what is used: nothing is reopened by path.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

use crate::PluginError;
use crate::trust::FileHashes;

pub const MAX_FILES: usize = 512;
pub const MAX_PACKAGE_BYTES: u64 = 64 << 20;
/// Zip entries are capped like folders, plus this much compressed input.
pub const MAX_ZIP_BYTES: u64 = 64 << 20;

/// The files of one package: relative path (`/`-separated) → bytes, and which are
/// executable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Package {
    files: BTreeMap<String, Vec<u8>>,
    executable: BTreeSet<String>,
}

/// Whether a name is skipped (dotfiles: Finder, SMB and git metadata).
#[must_use]
pub fn ignored(name: &str) -> bool {
    name.starts_with('.')
}

fn check_name(rel: &str) -> Result<(), PluginError> {
    let ok = !rel.is_empty()
        && rel
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
        && rel.chars().all(|c| c.is_ascii_graphic() || c == ' ')
        && !rel.contains('\\');
    if ok {
        Ok(())
    } else {
        Err(PluginError::Invalid(format!(
            "`{rel}`: file names in a plug-in are plain ASCII (letters, digits, punctuation)"
        )))
    }
}

impl Package {
    /// Read every file under `root` (refusing links and odd names), each opened without
    /// following a link and read once.
    pub fn read_dir(root: &Path) -> Result<Self, PluginError> {
        let meta = fs::symlink_metadata(root)?;
        if !meta.is_dir() {
            return Err(PluginError::Invalid(format!(
                "{} is not a folder",
                root.display()
            )));
        }
        let mut p = Self::default();
        let mut total = 0;
        p.walk(root, root, &mut total)?;
        p.check_case()?;
        Ok(p)
    }

    fn walk(&mut self, root: &Path, dir: &Path, total: &mut u64) -> Result<(), PluginError> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if ignored(&name) {
                continue;
            }
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .map_err(|_| PluginError::Invalid("path outside the folder".into()))?
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            check_name(&rel)?;
            let meta = fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                return Err(PluginError::Invalid(format!(
                    "`{rel}` is a symbolic link; plug-ins can't contain links"
                )));
            }
            if meta.is_dir() {
                self.walk(root, &path, total)?;
            } else if meta.is_file() {
                if meta.nlink() > 1 {
                    return Err(PluginError::Invalid(format!(
                        "`{rel}` is a hard link; plug-ins can't contain links"
                    )));
                }
                let mut f = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc_o_nofollow())
                    .open(&path)?;
                let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
                f.by_ref()
                    .take(MAX_PACKAGE_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                let exec = meta.permissions().mode() & 0o111 != 0;
                self.add(&rel, bytes, exec, total)?;
            } else {
                return Err(PluginError::Invalid(format!(
                    "`{rel}` is not a regular file"
                )));
            }
        }
        Ok(())
    }

    fn add(
        &mut self,
        rel: &str,
        bytes: Vec<u8>,
        exec: bool,
        total: &mut u64,
    ) -> Result<(), PluginError> {
        *total += bytes.len() as u64;
        if self.files.len() >= MAX_FILES || *total > MAX_PACKAGE_BYTES {
            return Err(PluginError::Invalid(format!(
                "a plug-in holds at most {MAX_FILES} files and {} MB",
                MAX_PACKAGE_BYTES >> 20
            )));
        }
        if exec {
            self.executable.insert(rel.to_owned());
        }
        if self.files.insert(rel.to_owned(), bytes).is_some() {
            return Err(PluginError::Invalid(format!("`{rel}` appears twice")));
        }
        Ok(())
    }

    fn check_case(&self) -> Result<(), PluginError> {
        let mut seen = BTreeSet::new();
        for name in self.files.keys() {
            if !seen.insert(name.to_ascii_lowercase()) {
                return Err(PluginError::Invalid(format!(
                    "two files are called `{name}` ignoring case (macOS can't tell them apart)"
                )));
            }
        }
        Ok(())
    }

    /// Read a `.ltplugin` zip: one top folder (`Name.ltplugin/…`) or the files at the
    /// root. Entries must be regular files with safe relative names; links, absolute
    /// paths and `..` are refused; sizes are capped before anything is inflated.
    pub fn read_zip(bytes: &[u8]) -> Result<Self, PluginError> {
        if bytes.len() as u64 > MAX_ZIP_BYTES {
            return Err(PluginError::Invalid("the zip is too large".into()));
        }
        let bad = |m: String| PluginError::Invalid(format!("zip: {m}"));
        let mut z =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| bad(e.to_string()))?;
        if z.len() > MAX_FILES * 2 {
            return Err(bad("too many entries".into()));
        }
        let mut names = Vec::new();
        for i in 0..z.len() {
            let f = z.by_index_raw(i).map_err(|e| bad(e.to_string()))?;
            let name = f.name().map_err(|e| bad(e.to_string()))?.into_owned();
            let path = f
                .enclosed_name()
                .ok_or_else(|| bad(format!("`{name}` leaves the package")))?;
            if path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            {
                return Err(bad(format!("`{name}` leaves the package")));
            }
            if f.is_symlink() {
                return Err(bad(format!("`{name}` is a link")));
            }
            names.push((i, name, f.is_dir(), f.size(), f.unix_mode()));
        }
        // A single top folder named *.ltplugin is stripped.
        // (Finder's Compress adds a `__MACOSX` folder of AppleDouble files: ignored.)
        let tops: BTreeSet<&str> = names
            .iter()
            .map(|(_, n, ..)| n.split('/').next().unwrap_or(""))
            .filter(|t| *t != "__MACOSX")
            .collect();
        let strip = match tops.iter().next() {
            Some(t) if tops.len() == 1 && crate::host::is_package_name(Path::new(&**t)) => {
                Some(format!("{t}/"))
            }
            _ => None,
        };
        let mut p = Self::default();
        let mut total = 0u64;
        for (i, name, is_dir, size, mode) in names {
            let rel = match &strip {
                Some(s) => name.strip_prefix(s.as_str()).unwrap_or("").to_owned(),
                None => name.clone(),
            };
            let rel = rel.trim_end_matches('/').to_owned();
            if is_dir
                || rel.is_empty()
                || rel.split('/').any(ignored)
                || rel.starts_with("__MACOSX")
            {
                continue;
            }
            check_name(&rel)?;
            if size > MAX_PACKAGE_BYTES {
                return Err(bad(format!("`{rel}` is too large")));
            }
            let mut f = z.by_index(i).map_err(|e| bad(e.to_string()))?;
            let mut buf = Vec::new();
            f.by_ref()
                .take(MAX_PACKAGE_BYTES + 1)
                .read_to_end(&mut buf)
                .map_err(|e| bad(e.to_string()))?;
            p.add(&rel, buf, mode.is_some_and(|m| m & 0o111 != 0), &mut total)?;
        }
        p.check_case()?;
        if p.files.is_empty() {
            return Err(bad("no files".into()));
        }
        Ok(p)
    }

    /// Read `path`: a folder, or a zip file.
    pub fn read(path: &Path) -> Result<Self, PluginError> {
        let meta = fs::symlink_metadata(path)?;
        if meta.is_file() {
            let mut bytes = Vec::new();
            fs::File::open(path)?
                .take(MAX_ZIP_BYTES + 1)
                .read_to_end(&mut bytes)?;
            Self::read_zip(&bytes)
        } else {
            Self::read_dir(path)
        }
    }

    /// Write the files into the new folder `dest` (each fsync'd).
    pub fn write_to(&self, dest: &Path) -> Result<(), PluginError> {
        fs::create_dir(dest)?;
        for (rel, bytes) in &self.files {
            let path = dest.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            std::io::Write::write_all(&mut f, bytes)?;
            let mode = if self.executable.contains(rel) {
                0o755
            } else {
                0o644
            };
            f.set_permissions(fs::Permissions::from_mode(mode))?;
            lt_fsutil::sync_file(&f)?;
        }
        Ok(())
    }

    /// Replace or add a file (`lt-plugin pack`); names are checked.
    pub fn put(&mut self, rel: &str, bytes: Vec<u8>) -> Result<(), PluginError> {
        check_name(rel)?;
        self.files.insert(rel.to_owned(), bytes);
        Ok(())
    }

    pub fn remove(&mut self, rel: &str) {
        self.files.remove(rel);
        self.executable.remove(rel);
    }

    /// The package as a `.ltplugin` zip (deflated, byte-sorted names, modes kept).
    pub fn to_zip(&self) -> Result<Vec<u8>, PluginError> {
        let bad = |e: zip::result::ZipError| PluginError::Invalid(format!("zip: {e}"));
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (rel, bytes) in &self.files {
            let mode = if self.executable.contains(rel) {
                0o755
            } else {
                0o644
            };
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(mode);
            z.start_file(rel.as_str(), opts).map_err(bad)?;
            std::io::Write::write_all(&mut z, bytes)?;
        }
        Ok(z.finish().map_err(bad)?.into_inner())
    }

    #[must_use]
    pub fn get(&self, rel: &str) -> Option<&[u8]> {
        self.files.get(rel).map(Vec::as_slice)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Path → BLAKE3 hex (`+x` appended for an executable file), every file but
    /// `SIGNATURE`. Byte-sorted paths.
    #[must_use]
    pub fn hashes(&self) -> FileHashes {
        self.files
            .iter()
            .filter(|(rel, _)| rel.as_str() != crate::trust::SIGNATURE_FILE)
            .map(|(rel, bytes)| {
                let mut h = blake3::hash(bytes).to_hex().to_string();
                if self.executable.contains(rel) {
                    h.push_str("+x");
                }
                (rel.clone(), h)
            })
            .collect()
    }
}

/// `O_NOFOLLOW`: never open a file through a link swapped in after the check.
fn libc_o_nofollow() -> i32 {
    // Darwin's value (sys/fcntl.h); the crate links no libc bindings.
    0x0100
}

#[cfg(test)]
#[path = "package_tests.rs"]
mod tests;
