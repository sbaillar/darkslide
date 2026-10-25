//! What a path is: an Aperture 3 or iPhoto 9.3+ library (the shared
//! `Database/apdb/Library.apdb` layout), an older iPhoto library
//! (`AlbumData.xml`), a Photos library (not supported), or none of these.
//! Only names are looked at here; nothing is opened.

use std::path::{Path, PathBuf};

/// What kind of library a path holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Aperture 3 (`.aplibrary`): `Database/apdb/Library.apdb`.
    Aperture,
    /// iPhoto 9.3 and later: the same database layout as Aperture 3.
    IPhoto,
    /// iPhoto 6 to 9.2: `AlbumData.xml` at the library's top.
    IPhotoXml,
    /// A Photos library: Darkslide doesn't read these.
    Photos,
    /// An Aperture 1 or 2 library (a different, older database).
    OldAperture,
    Unknown,
}

impl Format {
    /// Stable name (wire form).
    pub fn key(self) -> &'static str {
        match self {
            Self::Aperture => "aperture",
            Self::IPhoto => "iphoto",
            Self::IPhotoXml => "iphoto-xml",
            Self::Photos => "photos",
            Self::OldAperture => "old-aperture",
            Self::Unknown => "unknown",
        }
    }

    /// One the reader can import.
    pub fn is_readable(self) -> bool {
        matches!(self, Self::Aperture | Self::IPhoto | Self::IPhotoXml)
    }
}

/// Package extensions of Apple's photo libraries.
const PACKAGES: [&str; 4] = [
    "aplibrary",
    "photolibrary",
    "migratedphotolibrary",
    "photoslibrary",
];

fn ext_is(p: &Path, ext: &str) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// The library's database, where Aperture 3 and iPhoto 9.3+ keep it.
pub(crate) fn apdb_path(root: &Path) -> Option<PathBuf> {
    [
        root.join("Database").join("apdb").join("Library.apdb"),
        root.join("Database").join("Library.apdb"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// The library a chosen path belongs to: the package itself, or the package
/// around a chosen `AlbumData.xml` or `Library.apdb`.
pub fn library_root(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if path.is_file() {
        if name.eq_ignore_ascii_case("AlbumData.xml") {
            return path.parent().unwrap_or(path).to_owned();
        }
        if name.eq_ignore_ascii_case("Library.apdb") {
            // `<root>/Database/apdb/Library.apdb` or `<root>/Database/Library.apdb`.
            let mut up = path.parent();
            while let Some(dir) = up {
                if dir
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("Database"))
                {
                    return dir.parent().unwrap_or(dir).to_owned();
                }
                up = dir.parent();
            }
        }
    }
    path.to_owned()
}

/// Identifies a library across imports: where it is.
pub(crate) fn library_id(root: &Path) -> String {
    root.canonicalize()
        .unwrap_or_else(|_| root.to_owned())
        .to_string_lossy()
        .into_owned()
}

/// What `path` holds (see [`Format`]).
pub fn detect(path: &Path) -> Format {
    let root = library_root(path);
    if !root.is_dir() {
        return Format::Unknown;
    }
    let photos_db = root.join("database");
    if ext_is(&root, "photoslibrary")
        || photos_db.join("Photos.sqlite").is_file()
        || photos_db.join("photos.db").is_file()
    {
        return Format::Photos;
    }
    if apdb_path(&root).is_some() {
        return if ext_is(&root, "aplibrary") {
            Format::Aperture
        } else {
            Format::IPhoto
        };
    }
    if root.join("AlbumData.xml").is_file() {
        return Format::IPhotoXml;
    }
    if root.join("Aperture.aplib").is_dir() || root.join("Library.apdb").is_file() {
        return Format::OldAperture;
    }
    Format::Unknown
}

/// Whether `path` is inside (or is) an Apple photo library: such a folder is
/// never written to, so a session opened on it keeps its store elsewhere.
pub fn is_inside_library(path: &Path) -> bool {
    path.ancestors().any(|a| {
        PACKAGES.iter().any(|e| ext_is(a, e))
            || a.join("AlbumData.xml").is_file()
            || apdb_path(a).is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn touch(p: &Path) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"x").unwrap();
    }

    #[test]
    fn formats_are_told_apart_by_their_files() {
        let d = tempfile::tempdir().unwrap();
        let ap = d.path().join("Library.aplibrary");
        touch(&ap.join("Database/apdb/Library.apdb"));
        assert_eq!(detect(&ap), Format::Aperture);
        let ip = d.path().join("iPhoto Library.photolibrary");
        touch(&ip.join("Database/Library.apdb"));
        assert_eq!(detect(&ip), Format::IPhoto);
        let old = d.path().join("iPhoto Library");
        touch(&old.join("AlbumData.xml"));
        assert_eq!(detect(&old), Format::IPhotoXml);
        assert_eq!(detect(&old.join("AlbumData.xml")), Format::IPhotoXml);
        assert_eq!(
            detect(&ip.join("Database/Library.apdb")),
            Format::IPhoto,
            "the database file names its library"
        );
        let photos = d.path().join("Photos Library.photoslibrary");
        fs::create_dir_all(&photos).unwrap();
        assert_eq!(detect(&photos), Format::Photos);
        let renamed = d.path().join("Renamed");
        touch(&renamed.join("database/Photos.sqlite"));
        assert_eq!(detect(&renamed), Format::Photos);
        let v4 = d.path().join("Photos4");
        touch(&v4.join("database/photos.db"));
        assert_eq!(detect(&v4), Format::Photos);
        let a2 = d.path().join("Aperture Library.aplibrary");
        fs::create_dir_all(a2.join("Aperture.aplib")).unwrap();
        assert_eq!(detect(&a2), Format::OldAperture);
        let empty = d.path().join("Empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(detect(&empty), Format::Unknown);
        assert_eq!(detect(&d.path().join("missing")), Format::Unknown);
        let stray = d.path().join("x.apdb");
        touch(&stray);
        assert_eq!(detect(&stray), Format::Unknown);
        // A Library.apdb outside any Database folder names only itself.
        let loose = d.path().join("loose/Library.apdb");
        touch(&loose);
        assert_eq!(library_root(&loose), loose);
        for f in [
            Format::Aperture,
            Format::IPhoto,
            Format::IPhotoXml,
            Format::Photos,
            Format::OldAperture,
            Format::Unknown,
        ] {
            assert_ne!(f.key(), "");
            assert_eq!(
                f.is_readable(),
                matches!(f, Format::Aperture | Format::IPhoto | Format::IPhotoXml)
            );
        }
    }

    #[test]
    fn folders_inside_a_library_are_recognised() {
        let d = tempfile::tempdir().unwrap();
        let lib = d.path().join("Old");
        touch(&lib.join("AlbumData.xml"));
        fs::create_dir_all(lib.join("Originals/2008")).unwrap();
        assert!(is_inside_library(&lib.join("Originals/2008")));
        let ap = d.path().join("A");
        touch(&ap.join("Database/apdb/Library.apdb"));
        assert!(is_inside_library(&ap.join("Masters")));
        assert!(is_inside_library(Path::new(
            "/x/Lib.aplibrary/Masters/2012"
        )));
        assert!(is_inside_library(Path::new("/x/P.photoslibrary")));
        assert!(!is_inside_library(d.path()));
    }
}
