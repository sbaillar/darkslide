#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::fs;

use plist::Value;

use super::*;
use crate::fixtures::{Library, snapshot};

fn never() -> bool {
    false
}

fn read_lib(l: &Library) -> LrLibrary {
    read(&l.root, Format::Aperture, &l.volumes, &never).unwrap()
}

fn photo<'a>(lib: &'a LrLibrary, key: &str) -> &'a LrPhoto {
    lib.photos
        .iter()
        .find(|p| p.key == key)
        .unwrap_or_else(|| panic!("{key}: {:?}", lib.photos))
}

/// A library with one of everything the reader handles.
fn full() -> Library {
    let l = Library::apdb("Library.aplibrary");
    l.master_file("2012/03/15/20120315-101010/IMG_0001.CR2");
    l.master_file("2012/03/15/20120315-101010/IMG_0002.JPG");
    l.volume_file("Photos Disk", "Shoots/IMG_0003.NEF");
    l.exec(
        "INSERT INTO RKVolume (modelId, uuid, name) VALUES (1, 'vol-1', 'Photos Disk'), (2, 'vol-2', 'Old Disk');
         INSERT INTO RKMaster (modelId, uuid, imagePath, volumeId, isInTrash) VALUES
           (1, 'm1', '2012/03/15/20120315-101010/IMG_0001.CR2', NULL, 0),
           (2, 'm2', '2012/03/15/20120315-101010/IMG_0002.JPG', NULL, 0),
           (3, 'm3', 'Shoots/IMG_0003.NEF', 1, 0),
           (4, 'm4', 'Archive/IMG_0004.NEF', 2, 0),
           (5, 'm5', 'x/IMG_0005.NEF', 99, 0),
           (6, 'm6', 'gone/IMG_0006.CR2', NULL, 1),
           (7, 'm7', '../../escape.CR2', NULL, 0),
           (8, NULL, 'none.CR2', NULL, 0);
         INSERT INTO RKFolder (modelId, uuid, name, isMagic, folderType, parentFolderUuid, isInTrash) VALUES
           (1, 'LibraryFolder', 'Library', 1, 1, NULL, 0),
           (2, 'f-travel', 'Travel', 0, 1, 'LibraryFolder', 0),
           (3, 'p-paris', 'Paris 2012', 0, 2, 'f-travel', 0),
           (4, 'p-empty', 'Empty', 0, 2, 'LibraryFolder', 0),
           (5, 'p-trash', 'Binned', 0, 2, 'LibraryFolder', 1),
           (6, 'f-loop-a', 'A', 0, 1, 'f-loop-b', 0),
           (7, 'f-loop-b', 'B', 0, 1, 'f-loop-a', 0);
         INSERT INTO RKVersion (modelId, uuid, name, masterUuid, versionNumber, mainRating, isFlagged,
             colorLabelIndex, isHidden, isInTrash, showInLibrary, projectUuid, latitude, longitude,
             exifLatitude, exifLongitude, imageDate, imageTimeZoneOffsetSeconds, rotation) VALUES
           (10, 'v1', 'Eiffel at dusk', 'm1', 1, 4, 1, 1, 0, 0, 1, 'p-paris', 48.8584, 2.2945, NULL, NULL, 353000000.0, 3600, 0),
           (11, 'v1b', 'Version 2', 'm1', 2, -1, 1, 7, 0, 0, 1, 'p-paris', NULL, NULL, NULL, NULL, NULL, NULL, 180),
           (12, 'v2', 'IMG_0002', 'm2', 1, 0, 0, 0, 1, 0, 1, 'p-paris', 0, 0, 45.0, 6.0, NULL, NULL, 0),
           (13, 'v3', 'img_0003', 'm3', 1, 2, 0, 3, 0, 0, NULL, 'p-empty-not', NULL, NULL, NULL, NULL, NULL, NULL, NULL),
           (14, 'v4', NULL, 'm4', 1, 5, 0, 9, 0, 0, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
           (15, 'v5', NULL, 'm5', 1, 1, 0, NULL, 0, 0, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
           (16, 'v6', NULL, 'm6', 1, 1, 0, NULL, 0, 0, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
           (17, 'v-trash', NULL, 'm2', 3, 1, 0, NULL, 0, 1, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
           (18, 'v-orphan', NULL, 'nope', 1, 1, 0, NULL, 0, 0, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
           (19, NULL, NULL, 'm1', 1, 1, 0, NULL, 0, 0, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
         INSERT INTO RKKeyword (modelId, name) VALUES (1, 'France'), (2, ' night '), (3, '   '), (4, NULL);
         INSERT INTO RKKeywordForVersion (versionId, keywordId) VALUES
           (10, 1), (10, 2), (10, 1), (10, 3), (12, 1), (99, 1), (10, 77);
         INSERT INTO RKAlbum (modelId, uuid, albumType, albumSubclass, name, folderUuid, isMagic, isInTrash) VALUES
           (1, 'a-best', 1, 3, 'Best', 'p-paris', 0, 0),
           (2, 'a-smart', 1, 2, 'Five stars', 'f-travel', 0, 0),
           (3, 'a-book', 6, 3, 'Holiday book', NULL, 0, 0),
           (4, 'a-builtin', 1, 1, 'Flagged', NULL, 0, 0),
           (5, 'a-magic', 1, 3, 'Library', NULL, 1, 0),
           (6, 'a-trash', 1, 3, 'Old', NULL, 0, 1),
           (7, 'a-loop', NULL, NULL, 'Looped', 'f-loop-a', 0, 0);
         INSERT INTO RKAlbumVersion (versionId, albumId) VALUES (12, 1), (10, 1), (10, 1), (99, 1), (10, 2), (13, 7);",
    );
    l.adjustment(
        "v1",
        "RKExposureOperation",
        1,
        &[("inputExposure", Value::Real(0.5))],
    );
    l.adjustment(
        "v1",
        "RKRawDecodeOperation",
        0,
        &[("inputBoost", Value::Real(1.0))],
    );
    l.adjustment(
        "v1",
        "RKExposureOperation",
        2,
        &[("inputExposure", Value::Real(1.0))],
    );
    l.exec(
        "INSERT INTO RKImageAdjustment (versionUuid, name, adjIndex, data) VALUES
           ('v2', 'RKLevelsOperation', 0, x'00'),
           ('v2', 'RKCurvesOperation', 1, NULL),
           ('nobody', 'RKLevelsOperation', 0, NULL),
           ('v2', NULL, 2, NULL);",
    );
    l.exec_properties(
        "INSERT INTO RKUniqueString (modelId, stringProperty) VALUES (1, ' Seen from the river '), (2, 'Other');
         INSERT INTO RKIptcProperty (versionId, propertyKey, stringId) VALUES
           (10, 'Caption/Abstract', 1), (12, 'Headline', 2), (99, 'Caption/Abstract', 2);",
    );
    l
}

#[test]
fn photos_versions_and_their_metadata() {
    let l = full();
    let before = snapshot(&l.root);
    let lib = read_lib(&l);
    assert_eq!(snapshot(&l.root), before, "the library is only read");
    assert_eq!(lib.written_by.as_deref(), Some("Aperture 3"));
    let keys: Vec<&str> = lib.photos.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(keys, ["v1", "v1b", "v2", "v3", "v4"], "{:?}", lib.warnings);

    let v1 = photo(&lib, "v1");
    let masters = l.root.join("Masters");
    assert_eq!(
        v1.path,
        masters.join("2012/03/15/20120315-101010/IMG_0001.CR2")
    );
    assert_eq!(v1.folder.as_deref(), Some(masters.as_path()));
    assert_eq!(v1.rating, Rating::new(4));
    assert_eq!(v1.pick, Some(PickState::Picked));
    assert_eq!(v1.color_tag, Some(ColorTag::Red));
    assert_eq!(v1.title.as_deref(), Some("Eiffel at dusk"));
    assert_eq!(v1.caption.as_deref(), Some("Seen from the river"));
    assert_eq!(
        v1.keywords.as_deref(),
        Some(&["France".to_owned(), "night".to_owned()][..])
    );
    let g = v1.geotag.unwrap();
    assert!((g.latitude - 48.8584).abs() < 1e-9 && (g.longitude - 2.2945).abs() < 1e-9);
    assert_eq!(
        v1.capture_time_ms,
        Some((353_000_000 + APPLE_EPOCH + 3600) * 1000)
    );
    assert!(v1.master.is_none() && v1.order == 0);
    // Two exposure adjustments: the second is numbered; RAW decoding is kept for the mapping to skip.
    let d = v1.develop.as_ref().unwrap();
    assert_eq!(
        d.get("RKExposureOperation.inputExposure"),
        Some(&Setting::Num(0.5))
    );
    assert_eq!(
        d.get("RKExposureOperation~2.inputExposure"),
        Some(&Setting::Num(1.0))
    );
    assert!(d.contains_key("RKRawDecodeOperation.inputBoost"));

    // The second version: a copy of the first, rejected, grey label (no Luxtable colour), turned.
    let v1b = photo(&lib, "v1b");
    assert_eq!(v1b.master.as_deref(), Some("v1"));
    assert_eq!(
        (v1b.order, v1b.copy_name.as_deref()),
        (1, Some("Version 2"))
    );
    assert_eq!(v1b.rating, Rating::new(0));
    assert_eq!(v1b.pick, Some(PickState::Rejected));
    assert_eq!(v1b.color_tag, Some(ColorTag::None));
    assert_eq!(v1b.unmapped_label.as_deref(), Some("Gray"));
    assert_eq!(
        v1b.develop.as_ref().unwrap().get(ROTATION),
        Some(&Setting::Num(180.0))
    );
    assert_eq!(v1b.caption.as_deref(), Some(""), "none in Properties");

    // In iPhoto, Hidden is a photo the user hid: rejected (Aperture's flag is left alone).
    // A name that is the file's is no title. 0,0 is no position: EXIF's is used.
    let v2 = photo(&lib, "v2");
    assert_eq!(v2.pick, Some(PickState::Unflagged));
    let as_iphoto = read(&l.root, Format::IPhoto, &l.volumes, &never).unwrap();
    assert_eq!(photo(&as_iphoto, "v2").pick, Some(PickState::Rejected));
    assert_eq!(v2.title.as_deref(), Some(""));
    assert_eq!(v2.geotag.unwrap().latitude, 45.0);
    assert_eq!(v2.color_tag, Some(ColorTag::None));
    assert_eq!(v2.keywords.as_deref(), Some(&["France".to_owned()][..]));
    let d2 = v2.develop.as_ref().unwrap();
    assert!(d2.contains_key("RKLevelsOperation.present"));
    assert!(d2.contains_key("RKCurvesOperation.present"));

    // Referenced, on a connected volume (case-insensitive stem).
    let v3 = photo(&lib, "v3");
    assert_eq!(v3.path, l.volumes.join("Photos Disk/Shoots/IMG_0003.NEF"));
    assert!(v3.folder.is_none());
    assert_eq!(v3.title.as_deref(), Some(""));
    assert_eq!(v3.color_tag, Some(ColorTag::Yellow));
    assert_eq!(v3.pick, Some(PickState::Unflagged));
    assert_eq!(v3.keywords.as_deref(), Some(&[][..]));
    // Referenced on a volume that isn't connected: still listed (it's missing, not gone).
    let v4 = photo(&lib, "v4");
    assert_eq!(v4.path, l.volumes.join("Old Disk/Archive/IMG_0004.NEF"));
    assert_eq!(v4.unmapped_label.as_deref(), Some("Label 9"));
    assert!(v4.geotag.is_none() && v4.capture_time_ms.is_none());
    let w = lib.warnings.join("\n");
    assert!(
        w.contains("1 referenced original is on \u{201c}Old Disk\u{201d}"),
        "{w}"
    );
    assert!(w.contains("a volume the library doesn't name"), "{w}");
    // The orphan, the one on an unnamed volume and the one in the trash.
    assert!(
        w.contains("3 versions whose original isn't in the library"),
        "{w}"
    );
    assert!(w.contains("1 adjustment couldn't be read"), "{w}");
}

#[test]
fn projects_and_albums_become_collections() {
    let l = full();
    let lib = read_lib(&l);
    let by_name = |n: &str| {
        lib.collections
            .iter()
            .find(|c| c.name == n)
            .unwrap_or_else(|| panic!("{n}: {:?}", lib.collections))
    };
    let paris = by_name("Paris 2012");
    assert_eq!(paris.id, -3);
    assert_eq!(paris.parents, ["Travel"]);
    assert_eq!(paris.members, ["v1", "v1b", "v2"]);
    assert!(
        lib.collections
            .iter()
            .all(|c| c.name != "Empty" && c.name != "Binned")
    );
    let best = by_name("Best");
    assert_eq!(best.parents, ["Travel", "Paris 2012"]);
    assert_eq!(best.members, ["v2", "v1"], "in the order they were added");
    assert!(best.unsupported.is_none() && !best.smart);
    let smart = by_name("Five stars");
    assert!(smart.smart && smart.unsupported.as_deref().unwrap().contains("smart"));
    let book = by_name("Holiday book");
    assert!(book.unsupported.as_deref().unwrap().contains("book"));
    assert_eq!(by_name("Looped").members, ["v3"]);
    for gone in ["Flagged", "Library", "Old"] {
        assert!(lib.collections.iter().all(|c| c.name != gone), "{gone}");
    }
}

#[test]
fn optional_tables_and_columns_may_be_missing() {
    let l = Library::apdb("Lib.photolibrary");
    l.master_file("a.JPG");
    l.exec(
        "DROP TABLE RKFolder; DROP TABLE RKAlbum; DROP TABLE RKKeyword; DROP TABLE RKVolume;
         DROP TABLE RKImageAdjustment;
         CREATE TABLE RKAlbum (modelId integer primary key, name varchar);
         DROP TABLE RKAlbumVersion;
         CREATE TABLE RKAlbumVersion (versionId integer, albumId integer);
         INSERT INTO RKAlbum VALUES (1, 'Kept');
         INSERT INTO RKAlbumVersion VALUES (1, 1), (1, 1);
         DROP TABLE RKVersion;
         CREATE TABLE RKVersion (modelId integer primary key, uuid varchar, masterUuid varchar);
         INSERT INTO RKMaster (uuid, imagePath, volumeId) VALUES ('m', 'a.JPG', 3);
         INSERT INTO RKMaster (uuid, imagePath) VALUES ('m2', 'a.JPG');
         INSERT INTO RKVersion VALUES (1, 'v', 'm2');",
    );
    fs::remove_file(l.root.join("Database/apdb/Properties.apdb")).unwrap();
    let lib = read(&l.root, Format::IPhoto, &l.volumes, &never).unwrap();
    assert_eq!(lib.written_by.as_deref(), Some("iPhoto 9.3 or later"));
    let p = &lib.photos[0];
    assert_eq!(p.rating, None);
    assert_eq!(p.pick, None);
    assert_eq!(p.caption, None, "no captions database: left alone");
    assert_eq!(p.keywords, None, "no keyword table: left alone");
    assert_eq!(lib.collections.len(), 1);
    assert_eq!(lib.collections[0].members, ["v"]);
    // The masters' volume table is gone: a referenced original can't be placed.
    assert!(lib.warnings.iter().any(|w| w.contains("doesn't name")));
}

#[test]
fn adjustment_rows_may_say_disabled_or_brushed() {
    let l = Library::apdb("A.aplibrary");
    l.master_file("a.CR2");
    l.exec(
        "ALTER TABLE RKImageAdjustment ADD COLUMN isEnabled integer;
         ALTER TABLE RKImageAdjustment ADD COLUMN maskUuid varchar;
         INSERT INTO RKMaster (uuid, imagePath) VALUES ('m', 'a.CR2');
         INSERT INTO RKVersion (modelId, uuid, masterUuid) VALUES (1, 'v', 'm');
         INSERT INTO RKImageAdjustment (versionUuid, name, adjIndex, isEnabled, maskUuid) VALUES
           ('v', 'RKExposureOperation', 0, 0, NULL),
           ('v', 'RKEnhanceOperation', 1, 1, 'mask-1'),
           ('v', 'RKColorOperation', 2, 1, '');",
    );
    let lib = read_lib(&l);
    let d = lib.photos[0].develop.as_ref().unwrap();
    assert_eq!(
        d.get("RKExposureOperation.enabled"),
        Some(&Setting::Bool(false))
    );
    assert_eq!(d.get("RKEnhanceOperation.mask"), Some(&Setting::Bool(true)));
    assert!(!d.contains_key("RKColorOperation.mask"));
}

/// A volume name from the library is one folder under `/Volumes`, never a path.
#[test]
fn volume_names_cannot_point_outside_the_volumes_folder() {
    let l = Library::apdb("A.aplibrary");
    l.exec(
        "INSERT INTO RKVolume (modelId, name) VALUES
           (1, ''), (2, '..'), (3, '../..'), (4, '/etc'), (5, 'a/b'), (6, '.'), (7, 'Good');
         INSERT INTO RKMaster (uuid, imagePath, volumeId) VALUES
           ('m1', 'x1.CR2', 1), ('m2', 'x2.CR2', 2), ('m3', 'x3.CR2', 3), ('m4', 'x4.CR2', 4),
           ('m5', 'x5.CR2', 5), ('m6', 'x6.CR2', 6), ('m7', 'x7.CR2', 7);
         INSERT INTO RKVersion (modelId, uuid, masterUuid) VALUES
           (1, 'v1', 'm1'), (2, 'v2', 'm2'), (3, 'v3', 'm3'), (4, 'v4', 'm4'),
           (5, 'v5', 'm5'), (6, 'v6', 'm6'), (7, 'v7', 'm7');",
    );
    let lib = read_lib(&l);
    let keys: Vec<&str> = lib.photos.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(keys, ["v7"], "{:?}", lib.warnings);
    assert_eq!(lib.photos[0].path, l.volumes.join("Good/x7.CR2"));
    assert!(
        lib.warnings
            .iter()
            .any(|w| w.starts_with("6 referenced originals on a volume the library doesn't name")),
        "{:?}",
        lib.warnings
    );
    for bad in ["", "..", "../..", "/etc", "a/b", ".", "a\0b"] {
        assert!(!volume_name_ok(bad), "{bad:?}");
    }
    assert!(volume_name_ok("Photos Disk"));
}

/// An adjustment blob over the size limit is skipped without being read.
#[test]
fn oversized_adjustment_data_is_skipped() {
    let l = Library::apdb("A.aplibrary");
    l.master_file("a.CR2");
    l.exec(&format!(
        "INSERT INTO RKMaster (uuid, imagePath) VALUES ('m', 'a.CR2');
         INSERT INTO RKVersion (modelId, uuid, masterUuid) VALUES (1, 'v', 'm');
         INSERT INTO RKImageAdjustment (versionUuid, name, adjIndex, data) VALUES
           ('v', 'RKHugeOperation', 0, zeroblob({}));",
        crate::limits::MAX_ARCHIVE_BYTES + 1
    ));
    l.adjustment(
        "v",
        "RKExposureOperation",
        1,
        &[("inputExposure", Value::Real(0.5))],
    );
    let lib = read_lib(&l);
    let d = lib.photos[0].develop.as_ref().unwrap();
    assert_eq!(d.get("RKHugeOperation.present"), Some(&Setting::Bool(true)));
    assert_eq!(
        d.get("RKExposureOperation.inputExposure"),
        Some(&Setting::Num(0.5))
    );
    assert!(
        lib.warnings
            .iter()
            .any(|w| w.starts_with("1 adjustment couldn't be read")),
        "{:?}",
        lib.warnings
    );
}

#[test]
fn damaged_locked_and_foreign_databases() {
    // Not SQLite.
    let l = Library::apdb("A.aplibrary");
    fs::write(l.db(), b"this is not a database").unwrap();
    let e = read(&l.root, Format::Aperture, &l.volumes, &never).unwrap_err();
    assert!(matches!(e, AppleError::NotALibrary { .. }), "{e}");
    // SQLite, but not a library.
    let l = Library::apdb("A.aplibrary");
    l.exec("DROP TABLE RKVersion;");
    let e = read(&l.root, Format::Aperture, &l.volumes, &never).unwrap_err();
    assert!(e.to_string().contains("RKVersion"), "{e}");
    // Aperture 1 or 2's own database.
    let l = Library::apdb("A.aplibrary");
    l.exec("DROP TABLE RKVersion; CREATE TABLE ZRKVERSION (Z_PK integer);");
    let e = read(&l.root, Format::Aperture, &l.volumes, &never).unwrap_err();
    assert!(matches!(e, AppleError::Unsupported { .. }));
    assert!(e.to_string().contains("Aperture 1 or 2"));
    // No database at all.
    let d = tempfile::tempdir().unwrap();
    assert!(matches!(
        read(d.path(), Format::Aperture, d.path(), &never),
        Err(AppleError::NotALibrary { .. })
    ));
    // Locked by Aperture (an exclusive transaction): the copy still reads, and
    // the library is left as it was.
    let l = full();
    let lock = rusqlite::Connection::open(l.db()).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE; UPDATE RKVersion SET mainRating = 1;")
        .unwrap();
    let before = snapshot(&l.root);
    let lib = read_lib(&l);
    assert_eq!(
        photo(&lib, "v1").rating,
        Rating::new(4),
        "the committed state"
    );
    assert_eq!(snapshot(&l.root), before);
    drop(lock);
    // A damaged captions database costs the captions only.
    let l = full();
    fs::write(l.root.join("Database/apdb/Properties.apdb"), b"junk").unwrap();
    let lib = read_lib(&l);
    assert!(lib.warnings.iter().any(|w| w.starts_with("Captions:")));
    assert_eq!(photo(&lib, "v1").caption, None);
    // One without the caption tables: nothing to say.
    let l = full();
    rusqlite::Connection::open(l.root.join("Database/apdb/Properties.apdb"))
        .unwrap()
        .execute_batch("DROP TABLE RKUniqueString;")
        .unwrap();
    let lib = read_lib(&l);
    assert_eq!(photo(&lib, "v1").caption, None);
    assert!(!lib.warnings.iter().any(|w| w.starts_with("Captions:")));
}

#[test]
fn cancelling_or_running_out_of_time_stops_the_read() {
    let l = full();
    let e = read(&l.root, Format::Aperture, &l.volumes, &|| true).unwrap_err();
    assert!(matches!(e, AppleError::Cancelled));
    let e = read_within(
        &l.root,
        Format::Aperture,
        &l.volumes,
        &never,
        Duration::ZERO,
    )
    .unwrap_err();
    assert!(e.to_string().contains("too long"), "{e}");
    // Cancelled while reading the captions.
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let late = || calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 40;
    match read(&l.root, Format::Aperture, &l.volumes, &late) {
        Err(AppleError::Cancelled) | Ok(_) => {}
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn ancestry_stops_at_loops_and_unknown_folders() {
    let f = |parent: Option<&str>, name: &str| Folder {
        model_id: 1,
        name: name.into(),
        kind: Some(1),
        parent: parent.map(str::to_owned),
        magic: false,
        trashed: false,
    };
    let folders: HashMap<String, Folder> = [
        ("a".to_owned(), f(Some("b"), "A")),
        ("b".to_owned(), f(Some("a"), "B")),
        ("c".to_owned(), f(Some("missing"), "C")),
    ]
    .into();
    assert_eq!(ancestry(&folders, "a", true), ["B", "A"]);
    assert_eq!(ancestry(&folders, "c", false), Vec::<String>::new());
    assert_eq!(ancestry(&folders, "c", true), ["C"]);
}

#[test]
fn labels_follow_the_menu_order() {
    let tags: Vec<Option<ColorTag>> = (0..=7).map(|i| label(Some(i)).0).collect();
    assert_eq!(
        tags,
        [
            Some(ColorTag::None),
            Some(ColorTag::Red),
            Some(ColorTag::Orange),
            Some(ColorTag::Yellow),
            Some(ColorTag::Green),
            Some(ColorTag::Blue),
            Some(ColorTag::Purple),
            Some(ColorTag::None),
        ]
    );
    assert_eq!(label(None), (Some(ColorTag::None), None));
}
