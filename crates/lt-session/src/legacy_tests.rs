//! Sessions from before the rename (`.ds`, `.dsession`) move to `.lt` on open.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lt_core::{Adjustments, FileHash, Rating};
use rusqlite::Connection;

use super::*;
use crate::{GlobalIndex, ImageMetaLite, Session, VariantId};

fn meta() -> ImageMetaLite {
    ImageMetaLite {
        capture_time_ms: Some(1_700_000_000_000),
        camera: None,
        lens: None,
        iso: None,
        focal_length_mm: None,
        width: 600,
        height: 400,
        orientation: lt_core::geom::Orientation::Normal,
        as_shot_wb: None,
    }
}

fn ev(x: f32) -> Adjustments {
    let mut a = Adjustments::default();
    a.exposure.exposure_ev = x;
    a
}

/// Every directory under `root` with this name, deepest first.
fn dirs_named(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
                if e.file_name() == name {
                    out.push(e.path());
                }
            }
        }
    }
    out.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    out
}

/// Turn a closed session into what version 0.1 left: `.ds` folders, a
/// `.dsession` index whose sidecar paths name `.ds`.
fn make_legacy(root: &Path) {
    let db = fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "ltsession"))
        .unwrap();
    let c = Connection::open(&db).unwrap();
    c.execute(
        "UPDATE variants SET sidecar = substr(replace('/' || sidecar, '/.lt/', '/.ds/'), 2)",
        [],
    )
    .unwrap();
    c.close().unwrap();
    fs::rename(&db, db.with_extension("dsession")).unwrap();
    for d in dirs_named(root, ".lt") {
        fs::rename(&d, d.with_file_name(".ds")).unwrap();
    }
}

struct Old {
    _dir: tempfile::TempDir,
    root: PathBuf,
    v1: VariantId,
    v2: VariantId,
}

/// A legacy session with two edited photos (one in a subfolder) and an album.
fn old_session() -> Old {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Hike");
    let mut s = Session::create(&root, "Hike").unwrap();
    let mut add = |rel: &str| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, rel.as_bytes()).unwrap();
        let id = s
            .add_original(
                rel,
                FileHash(*blake3::hash(rel.as_bytes()).as_bytes()),
                meta(),
            )
            .unwrap();
        s.variants(id).unwrap()[0]
    };
    let v1 = add("Capture/a.ARW");
    let v2 = add("Capture/Day 2/b.ARW");
    s.save_adjustments(v1, &ev(1.5)).unwrap();
    s.set_rating(&[v2], Rating::new(4).unwrap()).unwrap();
    let album = s.create_album("Best").unwrap();
    s.add_to_album(album, &[v1]).unwrap();
    drop(s);
    make_legacy(&root);
    assert!(root.join(".ds/settings/session.json").is_file());
    assert!(root.join("Capture/Day 2/.ds/settings").is_dir());
    Old {
        _dir: dir,
        root,
        v1,
        v2,
    }
}

fn mode(p: &Path, m: u32) {
    fs::set_permissions(p, fs::Permissions::from_mode(m)).unwrap();
}

#[test]
fn an_old_session_opens_and_ends_up_in_lt_with_everything_intact() {
    let o = old_session();
    let originals: Vec<Vec<u8>> = ["Capture/a.ARW", "Capture/Day 2/b.ARW"]
        .iter()
        .map(|r| fs::read(o.root.join(r)).unwrap())
        .collect();
    assert!(is_session_dir(&o.root));
    let mut s = Session::open(&o.root).unwrap();
    let report = s.open_report().clone();
    assert!(
        matches!(report.legacy, Some(Legacy::Migrated { folders }) if folders >= 3),
        "{report:?}"
    );
    assert!(report.recovery.is_none(), "the index is kept, not rebuilt");
    assert!(dirs_named(&o.root, ".ds").is_empty(), "no .ds left");
    assert!(o.root.join(".lt/settings/session.json").is_file());
    assert!(o.root.join("Hike.ltsession").is_file() && !o.root.join("Hike.dsession").exists());
    assert!(!o.root.join(".lt").join(MARKER).exists());
    // Originals, edits, ratings and albums all came along; the index points at `.lt`.
    for (r, bytes) in ["Capture/a.ARW", "Capture/Day 2/b.ARW"]
        .iter()
        .zip(&originals)
    {
        assert_eq!(&fs::read(o.root.join(r)).unwrap(), bytes);
    }
    assert_eq!(s.load_adjustments(o.v1).unwrap(), ev(1.5));
    assert_eq!(s.variant(o.v2).unwrap().rating, Rating::new(4).unwrap());
    assert_eq!(s.albums()[0].name, "Best");
    assert!(s.locate(o.v2).unwrap().sidecar.contains("/.lt/settings/"));
    // Edits keep working.
    s.save_adjustments(o.v2, &ev(-1.0)).unwrap();
    drop(s);
    let s = Session::open(&o.root).unwrap();
    assert_eq!(s.open_report().legacy, None, "a second open does nothing");
    assert_eq!(s.load_adjustments(o.v2).unwrap(), ev(-1.0));
    assert!(dirs_named(&o.root, ".ds").is_empty(), "no .ds left");
}

#[test]
fn a_session_that_cant_be_renamed_is_left_exactly_as_it_was() {
    let o = old_session();
    mode(&o.root, 0o555);
    let err = Session::open(&o.root).unwrap_err();
    mode(&o.root, 0o755);
    assert!(matches!(err, SessionError::LegacyMigration { .. }), "{err}");
    assert!(err.to_string().contains("Nothing was changed"));
    assert!(o.root.join(".ds/settings/session.json").is_file() && !o.root.join(".lt").exists());
    assert!(o.root.join("Hike.dsession").is_file());
}

#[test]
fn a_failure_part_way_puts_every_folder_back() {
    let o = old_session();
    let day2 = o.root.join("Capture/Day 2");
    mode(&day2, 0o555);
    let err = migrate(&o.root).unwrap_err();
    mode(&day2, 0o755);
    assert!(matches!(err, SessionError::LegacyMigration { .. }), "{err}");
    assert!(dirs_named(&o.root, ".lt").is_empty(), "every rename undone");
    assert!(o.root.join(".ds/settings/session.json").is_file());
    assert!(day2.join(".ds/settings").is_dir() && o.root.join("Capture/.ds/settings").is_dir());
    assert!(o.root.join("Hike.dsession").is_file());
    // Once it can, it migrates.
    assert!(matches!(
        migrate(&o.root).unwrap(),
        Some(Legacy::Migrated { .. })
    ));
    assert!(dirs_named(&o.root, ".ds").is_empty(), "no .ds left");
}

#[test]
fn with_both_folders_lt_wins_and_ds_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Both");
    drop(Session::create(&root, "Both").unwrap());
    fs::create_dir_all(root.join(".ds/settings")).unwrap();
    fs::write(
        root.join(".ds/settings/session.json"),
        b"{\"name\":\"Old\"}",
    )
    .unwrap();
    let s = Session::open(&root).unwrap();
    assert_eq!(s.open_report().legacy, Some(Legacy::BothPresent));
    assert_eq!(s.name(), "Both");
    assert_eq!(
        fs::read(root.join(".ds/settings/session.json")).unwrap(),
        b"{\"name\":\"Old\"}"
    );
}

#[test]
fn an_interrupted_migration_is_finished_on_the_next_open() {
    let o = old_session();
    // A crash right after the first rename: the marker is there, the rest isn't.
    fs::rename(o.root.join(".ds"), o.root.join(".lt")).unwrap();
    fs::write(o.root.join(".lt").join(MARKER), b"").unwrap();
    // A Capture folder that already has both keeps its old one.
    fs::create_dir_all(o.root.join("Capture/Day 2/.lt")).unwrap();
    let s = Session::open(&o.root).unwrap();
    assert!(matches!(
        s.open_report().legacy,
        Some(Legacy::Migrated { .. })
    ));
    assert!(!o.root.join(".lt").join(MARKER).exists());
    assert!(o.root.join("Capture/.lt/settings").is_dir() && !o.root.join("Capture/.ds").exists());
    assert!(
        o.root.join("Capture/Day 2/.ds/settings").is_dir(),
        "both: the old one stays"
    );
    assert!(o.root.join("Hike.ltsession").is_file());
    assert_eq!(s.load_adjustments(o.v1).unwrap(), ev(1.5));
}

#[test]
fn a_journal_from_before_the_migration_still_rolls_forward() {
    let o = old_session();
    // The sidecar of v1 as version 0.1 named it, with a batch that crashed mid-way.
    let db = Connection::open(o.root.join("Hike.dsession")).unwrap();
    let rel: String = db
        .query_row("SELECT sidecar FROM variants WHERE id = ?", [o.v1.0], |r| {
            r.get(0)
        })
        .unwrap();
    db.close().unwrap();
    assert!(rel.contains("/.ds/"));
    let json = String::from_utf8(fs::read(o.root.join(&rel)).unwrap()).unwrap();
    let edited = json.replacen("\"rating\": 0", "\"rating\": 5", 1);
    assert_ne!(edited, json, "the sidecar has a rating field");
    let began = SystemTime::now() + Duration::from_secs(60);
    let nanos = began.duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let journal = o.root.join(format!(".ds/journal/{nanos}-x.json"));
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    let entries = serde_json::json!([{ "rel": rel, "json": edited, "created": false }]);
    fs::write(&journal, serde_json::to_vec(&entries).unwrap()).unwrap();
    let s = Session::open(&o.root).unwrap();
    assert_eq!(s.variant(o.v1).unwrap().rating, Rating::new(5).unwrap());
    assert!(!o.root.join(".ds").exists());
}

#[test]
fn an_index_in_use_by_another_app_stops_the_migration() {
    let o = old_session();
    let db = o.root.join("Hike.dsession");
    let c = Connection::open(&db).unwrap();
    let _: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    c.execute("UPDATE variants SET name = name", []).unwrap();
    let err = Session::open(&o.root).unwrap_err();
    assert!(err.to_string().contains("in use"), "{err}");
    assert!(o.root.join(".ds/settings/session.json").is_file() && !o.root.join(".lt").exists());
    drop(c);
    assert!(Session::open(&o.root).is_ok());
}

#[test]
fn a_folder_around_an_in_place_store_moves_its_top_folder_only() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("Photos");
    fs::create_dir_all(folder.join("trip/.ds")).unwrap();
    fs::create_dir_all(folder.join(".ds/session")).unwrap();
    fs::write(folder.join(".ds/session/keep"), b"k").unwrap();
    assert_eq!(
        migrate(&folder).unwrap(),
        Some(Legacy::Migrated { folders: 1 })
    );
    assert_eq!(fs::read(folder.join(".lt/session/keep")).unwrap(), b"k");
    assert!(
        folder.join("trip/.ds").is_dir(),
        "the user's own folders aren't walked"
    );
    assert_eq!(migrate(&folder).unwrap(), None);
    // Nothing to do without `.ds`; a plain file named `.ds` isn't ours.
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    fs::write(plain.join(".ds"), b"").unwrap();
    assert_eq!(migrate(&plain).unwrap(), None);
    assert!(!is_session_dir(&plain));
}

#[test]
fn the_global_index_follows_in_place_stores_to_lt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("global.db");
    let mut g = GlobalIndex::open(&path).unwrap();
    g.register_session(Path::new("/p/A/.ds/session"), "A")
        .unwrap();
    g.register_session(Path::new("/p/B/.ds/session"), "B")
        .unwrap();
    g.register_session(Path::new("/p/B/.lt/session"), "B")
        .unwrap();
    g.register_session(Path::new("/p/C"), "C").unwrap();
    drop(g);
    let g = GlobalIndex::open(&path).unwrap();
    let mut roots: Vec<PathBuf> = g.sessions().unwrap().into_iter().map(|s| s.root).collect();
    roots.sort();
    assert_eq!(
        roots,
        [
            "/p/A/.lt/session",
            "/p/B/.ds/session",
            "/p/B/.lt/session",
            "/p/C"
        ]
        .map(PathBuf::from)
    );
}

#[test]
fn stores_in_the_copied_support_folder_are_relocated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("global.db");
    let mut g = GlobalIndex::open(&path).unwrap();
    let old = Path::new("/S/Old");
    g.register_session(Path::new("/S/Old/InPlace/v1/a"), "a")
        .unwrap();
    g.register_session(Path::new("/S/Old/InPlace/v1/b"), "b")
        .unwrap();
    g.register_session(Path::new("/S/New/InPlace/v1/b"), "b")
        .unwrap();
    g.register_session(Path::new("/S/Older/x"), "x").unwrap();
    g.register_session(Path::new("/elsewhere/c"), "c").unwrap();
    assert_eq!(g.relocate_roots(old, Path::new("/S/New/")).unwrap(), 1);
    let mut roots: Vec<PathBuf> = g.sessions().unwrap().into_iter().map(|s| s.root).collect();
    roots.sort();
    assert_eq!(
        roots,
        [
            "/S/New/InPlace/v1/a",
            "/S/New/InPlace/v1/b",
            "/S/Old/InPlace/v1/b",
            "/S/Older/x",
            "/elsewhere/c"
        ]
        .map(PathBuf::from)
    );
    assert_eq!(g.relocate_roots(old, old).unwrap(), 0);
    assert_eq!(
        g.relocate_roots(Path::new("/"), Path::new("/x")).unwrap(),
        0
    );
}

#[test]
fn old_names_map_to_new_ones() {
    assert_eq!(
        current_rel("Capture/x/.ds/settings/a.json"),
        "Capture/x/.lt/settings/a.json"
    );
    assert_eq!(current_rel("Capture/.dsx/a.json"), "Capture/.dsx/a.json");
    assert!(crate::atomic::is_temp_name(".a.json.dstmp-1-2"));
    assert!(crate::atomic::is_temp_name(".a.json.lttmp-1-2"));
    assert!(!crate::atomic::is_temp_name("a.json.dstmp-1-2"));
}
