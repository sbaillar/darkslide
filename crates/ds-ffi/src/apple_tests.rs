//! Aperture / iPhoto import through the FFI: a synthetic library whose
//! `Masters` folder is opened in place (its store kept outside the library),
//! kind detection, dry run, import, revert, and the library left byte-identical.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ds_core::geom::Orientation;
use ds_lr::Category;
use ds_session::ImageMetaLite;
use futures_executor::block_on;

use super::*;
use crate::raw::RawMetadata;
use crate::session::{Probe, open_in_place};

fn arw(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw"))
}

fn fake_read(p: &Path) -> Option<(ImageMetaLite, RawMetadata)> {
    fs::metadata(p).ok()?;
    let lite = ImageMetaLite {
        width: 6000,
        height: 4000,
        orientation: Orientation::Normal,
        ..ImageMetaLite::default()
    };
    Some((lite, RawMetadata::default()))
}

const FAKE: Probe = Probe {
    is_supported: arw,
    read: fake_read,
    volume_ok: |_| Ok(()),
    ranges: |_| None,
    untrusted_stat: |_| false,
    is_raw_name: arw,
};

/// An Aperture library: two managed originals, the first rated 4 and flagged
/// with a keyword, the second rejected; one project holding both.
fn library(dir: &Path) -> PathBuf {
    let root = dir.join("Library.aplibrary");
    let masters = root.join("Masters/2012/03");
    fs::create_dir_all(&masters).unwrap();
    for f in ["a.ARW", "b.ARW"] {
        fs::write(masters.join(f), format!("raw {f}")).unwrap();
    }
    let apdb = root.join("Database/apdb");
    fs::create_dir_all(&apdb).unwrap();
    rusqlite::Connection::open(apdb.join("Library.apdb"))
        .unwrap()
        .execute_batch(
            "CREATE TABLE RKMaster (modelId integer primary key, uuid varchar, imagePath varchar);
             CREATE TABLE RKVersion (modelId integer primary key, uuid varchar, name varchar,
               masterUuid varchar, versionNumber integer, mainRating integer, isFlagged integer,
               projectUuid varchar);
             CREATE TABLE RKFolder (modelId integer primary key, uuid varchar, name varchar,
               folderType integer, parentFolderUuid varchar, isMagic integer);
             CREATE TABLE RKKeyword (modelId integer primary key, name varchar);
             CREATE TABLE RKKeywordForVersion (versionId integer, keywordId integer);
             CREATE TABLE RKAlbum (modelId integer primary key, name varchar, albumSubclass integer);
             INSERT INTO RKMaster VALUES (1, 'm1', '2012/03/a.ARW'), (2, 'm2', '2012/03/b.ARW');
             INSERT INTO RKFolder VALUES (1, 'p1', 'Spring', 2, NULL, 0);
             INSERT INTO RKVersion VALUES (1, 'v1', 'a', 'm1', 1, 4, 1, 'p1'), (2, 'v2', 'b', 'm2', 1, -1, 0, 'p1');
             INSERT INTO RKKeyword VALUES (1, 'Garden');
             INSERT INTO RKKeywordForVersion VALUES (1, 1);
             INSERT INTO RKAlbum VALUES (1, 'Smart one', 2);",
        )
        .unwrap();
    root
}

fn checksums(dir: &Path) -> BTreeMap<PathBuf, blake3::Hash> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), blake3::hash(&fs::read(e.path()).unwrap()));
            }
        }
    }
    out
}

fn opts(dry_run: bool) -> ImportOptions {
    ImportOptions {
        dry_run,
        flavor: ds_apple::FLAVOR,
        ..ImportOptions::default()
    }
}

#[test]
fn a_library_imports_into_its_masters_session_and_reverts() {
    let t = tempfile::tempdir().unwrap();
    let root = library(t.path());
    let before = checksums(&root);
    let store = t.path().join("support/InPlace/vol/abc");
    let h: Arc<SessionHandle> = open_in_place(
        &root.join("Masters"),
        Some(&store),
        None,
        &FAKE,
        &|| false,
        &mut |_, _, _| {},
    )
    .unwrap()
    .into();
    assert!(
        !root.join("Masters/.ds").exists(),
        "the store is outside the library"
    );
    assert!(store.is_dir(), "it went to the store given");

    let source = AppleLibrarySource::load(root.to_str().unwrap(), &|| false).unwrap();
    let s = source.summary();
    assert_eq!(
        (s.format.as_str(), s.origin.as_str(), s.photos),
        ("aperture", "Library.aplibrary", 2)
    );
    assert_eq!(
        (s.albums, s.skipped_albums),
        (1, 1),
        "the project; the smart album"
    );
    assert_eq!(s.written_by.as_deref(), Some("Aperture 3"));

    assert!(!h.can_revert_apple_import());
    let dry = h
        .import_apple_with(&source, &opts(true), &|| false)
        .unwrap();
    assert_eq!((dry.photos, dry.matched, dry.matched_by_path), (2, 2, 2));
    assert!(
        dry.changed
            .iter()
            .any(|c| c.category == Category::Rating.key() && c.count == 1),
        "{:?}",
        dry.changed
    );
    assert_eq!(dry.albums_created, 1);
    assert!(!h.can_revert_apple_import());

    let real = h
        .import_apple_with(&source, &opts(false), &|| false)
        .unwrap();
    assert_eq!(real.changed, dry.changed);
    assert!(h.can_revert_apple_import());
    assert!(
        !h.can_revert_lightroom_import(),
        "kept apart from Lightroom's"
    );
    let again = h
        .import_apple_with(&source, &opts(false), &|| false)
        .unwrap();
    assert!(again.changed.is_empty(), "idempotent: {:?}", again.changed);
    assert_eq!(checksums(&root), before, "the library is untouched");

    let r = block_on(h.clone().revert_apple_import(CancelToken::new()))
        .unwrap()
        .unwrap();
    assert!(r.restored >= 2);
    assert!(!h.can_revert_apple_import());
    assert!(
        block_on(h.clone().revert_apple_import(CancelToken::new()))
            .unwrap()
            .is_none()
    );
    assert_eq!(checksums(&root), before);
}

#[test]
fn async_entry_points_check_categories_and_cancel() {
    let t = tempfile::tempdir().unwrap();
    let root = library(t.path());
    let h: Arc<SessionHandle> = open_in_place(
        &root.join("Masters"),
        Some(&t.path().join("store")),
        None,
        &FAKE,
        &|| false,
        &mut |_, _, _| {},
    )
    .unwrap()
    .into();
    let source = block_on(load_apple_library(
        root.to_string_lossy().into_owned(),
        CancelToken::new(),
    ))
    .unwrap();
    let r = block_on(h.clone().import_apple_library(
        source.clone(),
        vec!["rating".into()],
        true,
        false,
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(r.changed.len(), 1);
    let bad = block_on(h.clone().import_apple_library(
        source.clone(),
        vec!["nope".into()],
        true,
        false,
        CancelToken::new(),
    ));
    assert!(matches!(bad, Err(DsError::InvalidArgument { .. })));
    let token = CancelToken::new();
    token.cancel();
    let cancelled = block_on(
        h.clone()
            .import_apple_library(source, vec![], true, false, token),
    );
    assert!(matches!(cancelled, Err(DsError::Cancelled)));
    let token = CancelToken::new();
    token.cancel();
    assert!(matches!(
        block_on(load_apple_library(
            root.to_string_lossy().into_owned(),
            token
        )),
        Err(DsError::Cancelled)
    ));
    let token = CancelToken::new();
    token.cancel();
    assert!(matches!(
        block_on(h.clone().revert_apple_import(token)),
        Err(DsError::Cancelled)
    ));
}

#[test]
fn kinds_and_refusals() {
    let t = tempfile::tempdir().unwrap();
    let root = library(t.path());
    let kind = |p: &Path| apple_library_kind(p.to_string_lossy().into_owned()).unwrap();
    assert_eq!(kind(&root), "aperture");
    assert_eq!(kind(t.path()), "unknown");
    let photos = t.path().join("Photos Library.photoslibrary");
    fs::create_dir_all(&photos).unwrap();
    assert_eq!(kind(&photos), "photos");
    let e = AppleLibrarySource::load(photos.to_str().unwrap(), &|| false).unwrap_err();
    assert!(
        matches!(&e, DsError::Unsupported { message } if message.contains("Photos library")),
        "{e}"
    );
    let e = AppleLibrarySource::load(t.path().to_str().unwrap(), &|| false).unwrap_err();
    assert!(matches!(e, DsError::InvalidArgument { .. }), "{e}");
    fs::remove_file(root.join("Database/apdb/Library.apdb")).unwrap();
    fs::create_dir(root.join("Database/apdb/Library.apdb")).unwrap();
    // A folder where the database should be: not read.
    assert!(AppleLibrarySource::load(root.to_str().unwrap(), &|| false).is_err());
    let errors = [
        AppleError::Io {
            path: "x".into(),
            source: std::io::Error::other("io"),
        },
        AppleError::Db {
            path: "x".into(),
            message: "db".into(),
        },
        AppleError::Xml {
            path: "x".into(),
            message: "xml".into(),
        },
    ];
    for e in errors {
        assert!(matches!(DsError::from(e), DsError::Io { .. }));
    }
    assert!(matches!(
        DsError::from(AppleError::Cancelled),
        DsError::Cancelled
    ));
}

#[test]
fn a_folder_inside_a_library_never_holds_the_store() {
    let t = tempfile::tempdir().unwrap();
    let root = library(t.path());
    // No fallback store: opening refuses rather than write into the library.
    let e = open_in_place(
        &root.join("Masters"),
        None,
        None,
        &FAKE,
        &|| false,
        &mut |_, _, _| {},
    )
    .unwrap_err();
    assert!(e.to_string().contains("never written to"), "{e}");
    assert!(!root.join("Masters/.ds").exists());
    // A plain folder still keeps its store beside the photos.
    let plain = t.path().join("Shoot");
    fs::create_dir_all(&plain).unwrap();
    fs::write(plain.join("a.ARW"), b"raw").unwrap();
    open_in_place(&plain, None, None, &FAKE, &|| false, &mut |_, _, _| {}).unwrap();
    assert!(plain.join(".ds").is_dir());
}
