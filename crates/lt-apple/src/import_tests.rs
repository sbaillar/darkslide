//! A whole library imported into a Luxtable session opened on its `Masters`
//! folder (the session's store outside the library, as the app keeps it).
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::path::PathBuf;

use lt_core::{ColorTag, FileHash, PickState, Rating};
use lt_lr::{Category, ImportOptions, ImportReport};
use lt_session::{ImageMetaLite, Session, VariantId};
use plist::Value;

use super::*;
use crate::fixtures::{Library, bytes, snapshot, write};

fn opts() -> ImportOptions {
    ImportOptions {
        flavor: FLAVOR,
        ..ImportOptions::default()
    }
}

struct Env {
    lib: Library,
    store: PathBuf,
    _store_dir: tempfile::TempDir,
}

const MANAGED: [&str; 2] = ["2012/03/IMG_0001.CR2", "2012/03/IMG_0002.CR2"];

/// An Aperture library and the Luxtable session over its `Masters` folder.
fn env() -> Env {
    let lib = Library::apdb("Library.aplibrary");
    for rel in MANAGED {
        lib.master_file(rel);
    }
    lib.master_file("2012/03/IMG_0009.CR2"); // in the library, not yet in the session
    lib.exec(
        "INSERT INTO RKVolume (modelId, name) VALUES (1, 'Offline Disk');
         INSERT INTO RKMaster (uuid, imagePath, volumeId) VALUES
           ('m1', '2012/03/IMG_0001.CR2', NULL), ('m2', '2012/03/IMG_0002.CR2', NULL),
           ('m9', '2012/03/IMG_0009.CR2', NULL), ('m3', 'Shoot/IMG_0003.NEF', 1);
         INSERT INTO RKFolder (modelId, uuid, name, folderType, parentFolderUuid, isMagic) VALUES
           (1, 'LibraryFolder', 'Library', 1, NULL, 1), (2, 'p1', 'Paris', 2, 'LibraryFolder', 0);
         INSERT INTO RKVersion (modelId, uuid, name, masterUuid, versionNumber, mainRating, isFlagged,
             colorLabelIndex, projectUuid, latitude, longitude) VALUES
           (1, 'v1', 'Eiffel', 'm1', 1, 4, 1, 4, 'p1', 48.85, 2.29),
           (2, 'v1b', 'Black and white', 'm1', 2, 2, 0, 0, 'p1', NULL, NULL),
           (3, 'v2', 'IMG_0002', 'm2', 1, -1, 0, 7, 'p1', NULL, NULL),
           (4, 'v9', NULL, 'm9', 1, 3, 0, 0, NULL, NULL, NULL),
           (5, 'v3', NULL, 'm3', 1, 5, 0, 0, NULL, NULL, NULL);
         INSERT INTO RKKeyword (modelId, name) VALUES (1, 'Paris'), (2, 'Tower');
         INSERT INTO RKKeywordForVersion (versionId, keywordId) VALUES (1, 1), (1, 2), (3, 1);
         INSERT INTO RKAlbum (modelId, albumType, albumSubclass, name, folderUuid) VALUES
           (1, 1, 3, 'Best', 'p1'), (2, 1, 2, 'Five stars', NULL);
         INSERT INTO RKAlbumVersion (versionId, albumId) VALUES (1, 1), (3, 1);",
    );
    lib.adjustment(
        "v1",
        "RKExposureOperation",
        0,
        &[("inputExposure", Value::Real(0.7))],
    );
    lib.adjustment("v1b", "RKBlackAndWhiteOperation", 0, &[]);
    lib.adjustment(
        "v2",
        "RKLevelsOperation",
        0,
        &[("inputBlack", Value::Real(0.1))],
    );
    lib.exec_properties(
        "INSERT INTO RKUniqueString (modelId, stringProperty) VALUES (1, 'From the river');
         INSERT INTO RKIptcProperty (versionId, propertyKey, stringId) VALUES (1, 'Caption/Abstract', 1);",
    );
    let store_dir = tempfile::tempdir().unwrap();
    let store = store_dir.path().join("store");
    let masters = lib.root.join("Masters");
    let mut s = Session::create_external(&store, "Masters", &masters).unwrap();
    for rel in MANAGED {
        let meta = ImageMetaLite {
            width: 6000,
            height: 4000,
            ..ImageMetaLite::default()
        };
        let hash = FileHash(*blake3::hash(&bytes(rel)).as_bytes());
        s.add_original(&format!("Capture/{rel}"), hash, meta)
            .unwrap();
    }
    Env {
        lib,
        store,
        _store_dir: store_dir,
    }
}

impl Env {
    fn session(&self) -> Session {
        Session::open(&self.store).unwrap()
    }

    fn read(&self) -> AppleLibrary {
        read_library_with(&self.lib.root, &self.lib.volumes, &|| false).unwrap()
    }

    fn run(&self, o: &ImportOptions) -> ImportReport {
        let lib = self.read();
        lt_lr::import(&mut self.session(), &lib.lib, o, &|| false).unwrap()
    }

    fn variants(&self, rel: &str) -> Vec<VariantId> {
        let s = self.session();
        let img = s
            .images()
            .unwrap()
            .into_iter()
            .find(|i| i.rel_path.as_str() == format!("Capture/{rel}"))
            .unwrap();
        s.variants(img.id).unwrap()
    }
}

#[test]
fn a_library_imports_maps_and_reports() {
    let e = env();
    let before = snapshot(&e.lib.root);
    let dry = e.run(&ImportOptions {
        dry_run: true,
        ..opts()
    });
    assert_eq!(dry.matched, 2, "{dry:?}");
    assert_eq!(
        e.session()
            .variant(e.variants(MANAGED[0])[0])
            .unwrap()
            .keywords,
        Vec::<String>::new()
    );
    let r = e.run(&opts());
    assert_eq!(
        snapshot(&e.lib.root),
        before,
        "nothing in the library changes"
    );
    assert!(!e.lib.root.join("Masters/.lt").exists());
    assert_eq!((r.photos, r.matched, r.matched_by_path), (5, 2, 2));
    assert_eq!(r.variants_created, 1, "the second version");
    // Not in the session: the managed one under the library's Masters folder,
    // the referenced one under its (disconnected) volume's folder.
    let folders: Vec<&str> = r
        .not_found_folders
        .iter()
        .map(|f| f.folder.as_str())
        .collect();
    let masters = e.lib.root.join("Masters");
    assert!(
        folders.contains(&masters.to_string_lossy().as_ref()),
        "{folders:?}"
    );
    assert!(
        folders.iter().any(|f| f.ends_with("Offline Disk/Shoot")),
        "{folders:?}"
    );
    assert!(r.warnings.iter().any(|w| w.contains("isn't connected")));

    let s = e.session();
    let v = e.variants(MANAGED[0]);
    assert_eq!(v.len(), 2);
    let first = s.variant(v[0]).unwrap();
    assert_eq!(first.rating, Rating::new(4).unwrap());
    assert_eq!(first.pick, PickState::Picked);
    assert_eq!(first.color_tag, ColorTag::Green);
    assert_eq!(first.keywords, ["Paris", "Tower"]);
    assert_eq!(first.iptc.title, "Eiffel");
    assert_eq!(first.iptc.caption, "From the river");
    assert!((first.geotag.unwrap().latitude - 48.85).abs() < 1e-9);
    assert!((s.load_adjustments(v[0]).unwrap().exposure.exposure_ev - 0.7).abs() < 1e-6);
    let copy = s.variant(v[1]).unwrap();
    assert_eq!(copy.rating, Rating::new(2).unwrap());
    assert_eq!(
        s.load_adjustments(v[1]).unwrap().exposure.saturation,
        -100.0
    );
    let rejected = s.variant(e.variants(MANAGED[1])[0]).unwrap();
    assert_eq!(rejected.pick, PickState::Rejected);
    assert_eq!(rejected.iptc.title, "", "the file's own name");
    assert_eq!(r.unmapped_labels.get("Gray"), Some(&1));
    assert_eq!(r.mapped_keys["Exposure"].1, 1);
    assert!(r.dropped.contains_key("Levels"), "{:?}", r.dropped);
    assert!(r.dropped.contains_key("Black & White mix"));
    // The project and the album; the smart album is reported.
    let names: Vec<String> = s.albums().iter().map(|a| a.name.clone()).collect();
    assert!(
        names.contains(&"Paris".to_owned()) && names.contains(&"Paris / Best".to_owned()),
        "{names:?}"
    );
    assert!(r.collections_skipped.iter().any(|c| c.name == "Five stars"));
    assert!(r.changed[&Category::Metadata] >= 2);
    // A Lightroom revert has nothing to do with it.
    assert!(!lt_lr::can_revert(&s));
    assert!(lt_lr::can_revert_as(&s, &FLAVOR));

    // Again: idempotent.
    let again = e.run(&opts());
    assert!(again.changed.is_empty(), "{:?}", again.changed);
    assert_eq!((again.albums_created, again.albums_updated), (0, 0));
    assert_eq!(e.variants(MANAGED[0]).len(), 2);

    // An edit in Luxtable survives a re-import of an unchanged library.
    e.session()
        .set_rating(&[v[0]], Rating::new(1).unwrap())
        .unwrap();
    let kept = e.run(&opts());
    assert_eq!(kept.kept_luxtable, 1);
    assert_eq!(
        e.session().variant(v[0]).unwrap().rating,
        Rating::new(1).unwrap()
    );

    // Revert: the import is taken back, the later Luxtable edit kept.
    let rev = lt_lr::revert_as(&mut e.session(), &FLAVOR)
        .unwrap()
        .unwrap();
    assert_eq!(rev.variants_removed, 1);
    assert!(rev.albums_removed >= 2);
    let s = e.session();
    let back = s.variant(v[0]).unwrap();
    assert_eq!(back.rating, Rating::new(1).unwrap(), "changed since: kept");
    assert!(back.keywords.is_empty() && back.iptc.title.is_empty());
    assert_eq!(back.geotag, None);
    assert_eq!(s.load_adjustments(v[0]).unwrap().exposure.exposure_ev, 0.0);
    assert_eq!(e.variants(MANAGED[0]).len(), 1);
    assert!(!lt_lr::can_revert_as(&s, &FLAVOR));
    assert_eq!(snapshot(&e.lib.root), before);
}

#[test]
fn an_old_iphoto_library_imports_its_originals() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("iPhoto Library");
    let rel = "2008/Roll 1/IMG_1.JPG";
    write(&root.join("Originals").join(rel), &bytes(rel));
    write(&root.join("Modified").join(rel), b"rendered edit");
    let recorded = "/Users/someone/Pictures/iPhoto Library";
    let xml = format!(
        r#"<?xml version="1.0"?><plist version="1.0"><dict>
        <key>Archive Path</key><string>{recorded}</string>
        <key>Master Image List</key><dict>
          <key>5</key><dict>
            <key>ImagePath</key><string>{recorded}/Modified/{rel}</string>
            <key>OriginalPath</key><string>{recorded}/Originals/{rel}</string>
            <key>Rating</key><integer>3</integer>
            <key>Comment</key><string>Grandma's garden</string>
          </dict>
        </dict>
        <key>List of Rolls</key><array><dict><key>RollID</key><integer>1</integer>
          <key>RollName</key><string>Roll 1</string><key>KeyList</key><array><string>5</string></array></dict></array>
        </dict></plist>"#
    );
    write(&root.join("AlbumData.xml"), xml.as_bytes());
    let store = d.path().join("store");
    let mut s = Session::create_external(&store, "Originals", &root.join("Originals")).unwrap();
    let hash = FileHash(*blake3::hash(&bytes(rel)).as_bytes());
    s.add_original(&format!("Capture/{rel}"), hash, ImageMetaLite::default())
        .unwrap();
    let lib = read_library(&root.join("AlbumData.xml"), &|| false).unwrap();
    assert_eq!(lib.format, Format::IPhotoXml);
    let r = lt_lr::import(&mut s, &lib.lib, &opts(), &|| false).unwrap();
    assert_eq!(r.matched, 1);
    assert!(r.dropped["iPhoto edit"].0.contains("unedited"));
    let v = s.variants(s.images().unwrap()[0].id).unwrap()[0];
    let info = s.variant(v).unwrap();
    assert_eq!(info.rating, Rating::new(3).unwrap());
    assert_eq!(info.iptc.caption, "Grandma's garden");
    assert!(s.albums().iter().any(|a| a.name == "Roll 1"));
}

#[test]
fn libraries_luxtable_cant_read_say_why() {
    let d = tempfile::tempdir().unwrap();
    let photos = d.path().join("Photos Library.photoslibrary");
    std::fs::create_dir_all(photos.join("database")).unwrap();
    let e = read_library(&photos, &|| false).unwrap_err();
    assert!(matches!(e, AppleError::Unsupported { .. }));
    assert!(e.to_string().contains("is a Photos library"), "{e}");
    let old = d.path().join("Old.aplibrary");
    std::fs::create_dir_all(old.join("Aperture.aplib")).unwrap();
    let e = read_library(&old, &|| false).unwrap_err();
    assert!(e.to_string().contains("Aperture 1 or 2"), "{e}");
    let e = read_library(d.path(), &|| false).unwrap_err();
    assert!(matches!(e, AppleError::NotALibrary { .. }));
    assert!(e.to_string().contains("AlbumData.xml"));
    // An Aperture library through the public entry point (its referenced
    // originals looked for under /Volumes).
    let l = Library::apdb("Lib.aplibrary");
    let lib = read_library(&l.root, &|| false).unwrap();
    assert_eq!(lib.format, Format::Aperture);
    assert_eq!(lib.lib.photos, Vec::new());
}
