//! Sessions from before the rename (`.ds`, `.dsession`) through the FFI layer:
//! plain and in-place sessions move to `.lt` on open and say so in the report.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_executor::block_on;

use super::tests::{by_name, folder, list, open};
use super::*;

/// Every path under `root` named `name` (directories), deepest first.
fn named(root: &Path, name: &str, dirs: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let is_dir = e.file_type().unwrap().is_dir();
            let n = e.file_name().to_string_lossy().into_owned();
            if is_dir {
                stack.push(e.path());
            }
            if is_dir == dirs && (n == name || (!dirs && n.ends_with(name))) {
                out.push(e.path());
            }
        }
    }
    out.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    out
}

/// Turn closed sessions under `root` into what Darkslide 0.1 left: every `.lt`
/// folder named `.ds`, every index `.dsession` with its sidecar paths in `.ds`.
pub(crate) fn make_legacy(root: &Path) {
    for db in named(root, ".ltsession", false) {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute(
            "UPDATE variants SET sidecar = substr(replace('/' || sidecar, '/.lt/', '/.ds/'), 2)",
            [],
        )
        .unwrap();
        c.close().unwrap();
        fs::rename(&db, db.with_extension("dsession")).unwrap();
    }
    for d in named(root, ".lt", true) {
        fs::rename(&d, d.with_file_name(".ds")).unwrap();
    }
}

#[test]
fn an_old_in_place_folder_moves_to_lt_and_keeps_its_edits() {
    let d = folder(&[("a.ARW", b"aaaa"), ("trip/b.ARW", b"bb")]);
    let h = open(d.path());
    assert_eq!(h.report.legacy, "");
    let a = by_name(&list(&h), "a.ARW").variant_id;
    block_on(Arc::new(h).set_rating(vec![a], 4, CancelToken::new())).unwrap();
    make_legacy(d.path());
    assert!(
        d.path()
            .join(".ds/session/.ds/settings/session.json")
            .is_file()
    );
    let h = open(d.path());
    assert_eq!(h.report.legacy, "migrated");
    assert!(h.report.store.ends_with(".lt/session"));
    assert!(named(d.path(), ".ds", true).is_empty(), "no .ds left");
    assert_eq!((h.report.added, h.report.existing), (0, 2));
    assert_eq!(by_name(&list(&h), "a.ARW").rating, 4);
    assert_eq!(fs::read(d.path().join("trip/b.ARW")).unwrap(), b"bb");
    drop(h);
    assert_eq!(
        open(d.path()).report.legacy,
        "",
        "a second open does nothing"
    );
}

#[test]
fn an_old_plain_session_opens_and_reports_the_move() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("Shoot");
    drop(Session::create(&root, "Shoot").unwrap());
    make_legacy(&root);
    assert!(lt_session::is_session_dir(&root));
    let h = block_on(open_session(
        root.to_string_lossy().into_owned(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(h.report.legacy, "migrated");
    assert!(root.join("Shoot.ltsession").is_file() && !root.join(".ds").exists());
    // Both: `.lt` is used, `.ds` stays as it was.
    fs::create_dir_all(root.join(".ds/keep")).unwrap();
    drop(h);
    let h = block_on(open_session(
        root.to_string_lossy().into_owned(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(h.report.legacy, "both");
    assert!(root.join(".ds/keep").is_dir());
    assert_eq!(legacy_text(None), "");
}

#[test]
fn the_global_index_follows_the_copied_support_folder() {
    let t = tempfile::tempdir().unwrap();
    let index = t.path().join("global.db");
    let s = |p: &Path| p.to_string_lossy().into_owned();
    assert_eq!(
        crate::import::relocate_global_index(s(&index), "/Old".into(), "/New".into()).unwrap(),
        0,
        "no index yet: nothing to do"
    );
    let mut g = lt_session::GlobalIndex::open(&index).unwrap();
    g.register_session(Path::new("/Old/InPlace/a"), "a")
        .unwrap();
    drop(g);
    assert_eq!(
        crate::import::relocate_global_index(s(&index), "/Old".into(), "/New".into()).unwrap(),
        1
    );
    let g = lt_session::GlobalIndex::open(&index).unwrap();
    assert_eq!(
        g.sessions().unwrap()[0].root,
        PathBuf::from("/New/InPlace/a")
    );
}
