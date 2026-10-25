//! Synthetic libraries for the tests: the schema subset the reader uses, with
//! the column names Aperture 3 / iPhoto 9.x use (see `docs/APPLE_FORMAT.md`),
//! and tiny stand-in originals. No real library or photo is in the repository.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// The `Library.apdb` tables and columns the reader looks at.
pub(crate) const LIBRARY_SCHEMA: &str = "
CREATE TABLE RKMaster (modelId integer primary key autoincrement, uuid varchar, name varchar,
  isInTrash integer, fileIsReference integer, isMissing integer, volumeId integer,
  fileSize integer, imagePath varchar, originalFileName varchar, type varchar);
CREATE TABLE RKVersion (modelId integer primary key autoincrement, uuid varchar, name varchar,
  isInTrash integer, isHidden integer, colorLabelIndex integer, fileName varchar,
  imageDate timestamp, imageTimeZoneOffsetSeconds integer, versionNumber integer,
  masterUuid varchar, masterId integer, mainRating integer, isFlagged integer,
  isOriginal integer, projectUuid varchar, latitude decimal, longitude decimal,
  exifLatitude decimal, exifLongitude decimal, showInLibrary integer, rotation integer);
CREATE TABLE RKFolder (modelId integer primary key autoincrement, uuid varchar, name varchar,
  isInTrash integer, isMagic integer, folderType integer, parentFolderUuid varchar,
  folderPath varchar);
CREATE TABLE RKAlbum (modelId integer primary key autoincrement, uuid varchar, albumType integer,
  albumSubclass integer, name varchar, folderUuid varchar, isInTrash integer, isMagic integer);
CREATE TABLE RKAlbumVersion (modelId integer primary key autoincrement, versionId integer,
  albumId integer);
CREATE TABLE RKKeyword (modelId integer primary key autoincrement, uuid varchar, name varchar,
  parentId integer);
CREATE TABLE RKKeywordForVersion (modelId integer primary key autoincrement, versionId integer,
  keywordId integer);
CREATE TABLE RKVolume (modelId integer primary key autoincrement, uuid varchar, name varchar);
CREATE TABLE RKImageAdjustment (modelId integer primary key autoincrement, uuid varchar,
  versionUuid varchar, name varchar, adjIndex integer, data blob);
";

/// The `Properties.apdb` tables captions come from.
pub(crate) const PROPERTIES_SCHEMA: &str = "
CREATE TABLE RKIptcProperty (modelId integer primary key autoincrement, versionId integer,
  modDate timestamp, propertyKey varchar, stringId integer);
CREATE TABLE RKUniqueString (modelId integer primary key autoincrement, stringProperty varchar);
";

/// The bytes of a stand-in original (unique per name, so content matching works).
pub(crate) fn bytes(rel: &str) -> Vec<u8> {
    format!("stand-in original {rel}").into_bytes()
}

/// A library package in a temporary directory, with `/Volumes` stood in by
/// `volumes` for referenced originals.
pub(crate) struct Library {
    pub _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub volumes: PathBuf,
}

impl Library {
    /// An Aperture-style package: `Database/apdb/Library.apdb` (+ `Properties.apdb`).
    pub fn apdb(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(name);
        let apdb = root.join("Database/apdb");
        fs::create_dir_all(&apdb).unwrap();
        fs::create_dir_all(root.join("Masters")).unwrap();
        Connection::open(apdb.join("Library.apdb"))
            .unwrap()
            .execute_batch(LIBRARY_SCHEMA)
            .unwrap();
        Connection::open(apdb.join("Properties.apdb"))
            .unwrap()
            .execute_batch(PROPERTIES_SCHEMA)
            .unwrap();
        let volumes = dir.path().join("Volumes");
        fs::create_dir_all(&volumes).unwrap();
        Self {
            _dir: dir,
            root,
            volumes,
        }
    }

    pub fn db(&self) -> PathBuf {
        self.root.join("Database/apdb/Library.apdb")
    }

    pub fn exec(&self, sql: &str) {
        Connection::open(self.db())
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    pub fn exec_properties(&self, sql: &str) {
        Connection::open(self.root.join("Database/apdb/Properties.apdb"))
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    /// A stand-in original under `Masters/`.
    pub fn master_file(&self, rel: &str) -> PathBuf {
        let p = self.root.join("Masters").join(rel);
        write(&p, &bytes(rel));
        p
    }

    /// A stand-in original on a mounted volume.
    pub fn volume_file(&self, volume: &str, rel: &str) -> PathBuf {
        let p = self.volumes.join(volume).join(rel);
        write(&p, &bytes(rel));
        p
    }

    /// An adjustment of version `uuid`, its parameters archived as Aperture does.
    pub fn adjustment(&self, uuid: &str, name: &str, index: i64, params: &[(&str, plist::Value)]) {
        let data = crate::archive::tests::archive(params);
        Connection::open(self.db())
            .unwrap()
            .execute(
                "INSERT INTO RKImageAdjustment (versionUuid, name, adjIndex, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![uuid, name, index, data],
            )
            .unwrap();
    }
}

pub(crate) fn write(p: &Path, b: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b).unwrap();
}

/// Every file under `dir`: relative path → content, to prove nothing changed.
pub(crate) fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                walk(root, &p, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.push((rel, fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}
