//! The Library through the FFI: sync of the open session, background refresh
//! of the others, queries and pages, edits written through, collections.
#![allow(clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_executor::block_on;

use super::library::*;
use super::people::tests::open;
use super::search::with_global;
use super::tests::{by_name, list};
use super::*;

fn tok() -> Arc<CancelToken> {
    CancelToken::new()
}

fn all() -> LibraryQueryRecord {
    LibraryQueryRecord {
        sessions: Vec::new(),
        from_ms: None,
        to_ms: None,
        min_rating: 0,
        unrated: false,
        labels: Vec::new(),
        pick: LibraryPickFilter::All,
        cameras: Vec::new(),
        keywords: Vec::new(),
        text: String::new(),
        collection: None,
        sort: LibrarySortKey::Captured,
        descending: false,
    }
}

fn page(global: &str, q: LibraryQueryRecord, first: u32) -> LibraryPage {
    block_on(library_query(global.into(), q, first, tok())).unwrap()
}

fn files(p: &LibraryPage) -> Vec<String> {
    p.first
        .iter()
        .map(|i| {
            Path::new(&i.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

struct Fx {
    dir: tempfile::TempDir,
    global: String,
    island: Arc<SessionHandle>,
    island_root: PathBuf,
}

/// An in-place session (two photos) synced into a fresh Library.
fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global.db").to_string_lossy().into_owned();
    let island = open(
        &dir.path().join("Island"),
        &[("a.ARW", &b"a"[..]), ("b.ARW", b"bb")],
    );
    assert!(block_on(island.clone().sync_library(global.clone(), tok())).unwrap());
    assert!(!block_on(island.clone().sync_library(global.clone(), tok())).unwrap());
    let island_root = PathBuf::from(island.in_place_folder().unwrap().unwrap());
    Fx {
        dir,
        global,
        island,
        island_root,
    }
}

#[test]
fn the_open_session_reaches_the_library_with_its_folder_as_root() {
    let f = fx();
    let p = page(&f.global, all(), 1);
    assert_eq!(p.ids.len(), 2);
    assert_eq!(files(&p), ["a.ARW"], "only the first one's details");
    let a = &p.first[0];
    assert_eq!(PathBuf::from(&a.session_root), f.island_root);
    assert_eq!(PathBuf::from(&a.path), f.island_root.join("a.ARW"));
    assert_eq!(
        (a.session_name.as_str(), a.ordinal, a.pick),
        ("Island", 1, PickFlag::Unflagged)
    );
    assert_eq!(a.edit_hash, None);
    let v = list(&f.island);
    assert_eq!(a.variant, by_name(&v, "a.ARW").variant_id);

    let items = block_on(library_items(f.global.clone(), vec![p.ids[1], -1], tok())).unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].path.ends_with("b.ARW"));

    let o = block_on(library_overview(f.global.clone(), tok())).unwrap();
    assert_eq!(o.total, 2);
    assert_eq!(o.sessions.len(), 1);
    assert!(o.sessions[0].available);
    assert_eq!(o.sessions[0].count, 2);
    assert_eq!(o.days.iter().map(|d| d.count).sum::<u64>(), 2);
}

#[test]
fn refresh_syncs_changed_and_never_synced_sessions_and_skips_the_rest() {
    let f = fx();
    let g = Path::new(&f.global);
    let r = refresh(g, &|| false).unwrap();
    assert_eq!((r.synced, r.unchanged, r.unavailable), (0, 1, 0), "{r:?}");

    // A change made by the open session: its index stamp moved.
    let v = list(&f.island);
    block_on(
        f.island
            .clone()
            .set_rating(vec![by_name(&v, "a.ARW").variant_id], 4, tok()),
    )
    .unwrap();
    let r = refresh(g, &|| false).unwrap();
    assert_eq!((r.synced, r.unchanged), (1, 0), "{r:?}");
    let q = LibraryQueryRecord {
        min_rating: 4,
        ..all()
    };
    assert_eq!(files(&page(&f.global, q, 10)), ["a.ARW"]);

    // A plain session registered (by import dedupe) but never synced: found at its root.
    let plain = f.dir.path().join("Plain");
    let h = block_on(create_session(
        plain.to_string_lossy().into(),
        "Plain".into(),
        tok(),
    ))
    .unwrap();
    drop(h);
    // A registered folder whose in-place store is in `.ds/session`.
    let shoot = f.dir.path().join("Shoot");
    let s = open(&shoot, &[("c.ARW", &b"c"[..])]);
    let shoot = PathBuf::from(s.in_place_folder().unwrap().unwrap());
    drop(s);
    with_global(g, |gl| {
        gl.register_session(&plain, "Plain")?;
        gl.register_session(&shoot, "Shoot")?;
        gl.register_session(Path::new("/nowhere/Gone"), "Gone")?;
        Ok(())
    })
    .unwrap();
    let r = refresh(g, &|| false).unwrap();
    assert_eq!((r.synced, r.unchanged, r.unavailable), (1, 2, 1), "{r:?}");
    assert_eq!(r.failed.len(), 0);
    let o = block_on(library_overview(f.global.clone(), tok())).unwrap();
    let counts: Vec<(&str, u64, bool)> = o
        .sessions
        .iter()
        .map(|s| (s.name.as_str(), s.count, s.available))
        .collect();
    assert_eq!(
        counts,
        [
            ("Gone", 0, false),
            ("Island", 2, true),
            ("Plain", 0, true),
            ("Shoot", 1, true)
        ]
    );
    let c = page(
        &f.global,
        LibraryQueryRecord {
            text: "c".into(),
            ..all()
        },
        1,
    );
    assert_eq!(PathBuf::from(&c.first[0].path), shoot.join("c.ARW"));
    // Nothing changed since: all unchanged; a store gone missing is unavailable.
    let r = refresh(g, &|| false).unwrap();
    assert_eq!((r.synced, r.unchanged, r.unavailable), (0, 3, 1), "{r:?}");
    std::fs::rename(&plain, f.dir.path().join("Moved")).unwrap();
    let r = refresh(g, &|| false).unwrap();
    assert_eq!((r.unchanged, r.unavailable), (2, 2), "{r:?}");
    // A store that no longer opens (a newer build's index) is reported, not fatal.
    let db = rusqlite::Connection::open(shoot.join(".ds/session/Shoot.dsession")).unwrap();
    db.execute_batch("PRAGMA user_version=999").unwrap();
    drop(db);
    let r = refresh(g, &|| false).unwrap();
    assert_eq!(r.failed.len(), 1, "{r:?}");
    assert!(r.failed[0].starts_with("Shoot: "), "{r:?}");

    assert_eq!(refresh(g, &|| true), Err(DsError::Cancelled));
    let t = tok();
    t.cancel();
    assert_eq!(
        block_on(library_refresh(f.global.clone(), t)),
        Err(DsError::Cancelled)
    );
}

#[test]
fn edits_are_written_to_the_owning_sessions_sidecars() {
    let f = fx();
    let other = open(&f.dir.path().join("Coast"), &[("m.ARW", &b"m"[..])]);
    block_on(other.clone().sync_library(f.global.clone(), tok())).unwrap();
    let p = page(&f.global, all(), 10);
    assert_eq!(p.ids.len(), 3);
    let r = block_on(library_edit(
        f.global.clone(),
        p.ids.clone(),
        LibraryEditRecord::Rating { stars: 5 },
        tok(),
    ))
    .unwrap();
    assert_eq!(r.changed.len(), 3);
    assert_eq!(r.failed.len(), 0);
    block_on(library_edit(
        f.global.clone(),
        vec![p.ids[0]],
        LibraryEditRecord::Label {
            label: ColorLabel::Purple,
        },
        tok(),
    ))
    .unwrap();
    block_on(library_edit(
        f.global.clone(),
        vec![p.ids[0]],
        LibraryEditRecord::Flag {
            pick: PickFlag::Rejected,
        },
        tok(),
    ))
    .unwrap();
    let p = page(&f.global, all(), 10);
    assert!(p.first.iter().all(|i| i.rating == 5));
    assert_eq!(
        (p.first[0].label, p.first[0].pick),
        (ColorLabel::Purple, PickFlag::Rejected)
    );
    // The sessions' own sidecars say so (seen through a fresh listing).
    let m = list(&other);
    assert_eq!(by_name(&m, "m.ARW").rating, 5);
    assert!(list(&f.island).iter().all(|v| v.rating == 5));

    assert!(matches!(
        block_on(library_edit(
            f.global.clone(),
            p.ids.clone(),
            LibraryEditRecord::Rating { stars: 6 },
            tok()
        )),
        Err(DsError::InvalidArgument { .. })
    ));
    // A session whose store is gone: reported, the others still written.
    let store = PathBuf::from(other.root().unwrap());
    drop(other);
    std::fs::remove_dir_all(&store).unwrap();
    let r = block_on(library_edit(
        f.global.clone(),
        p.ids.clone(),
        LibraryEditRecord::Rating { stars: 2 },
        tok(),
    ))
    .unwrap();
    assert_eq!(r.changed.len(), 2);
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].starts_with("Coast: "), "{:?}", r.failed);
}

#[test]
fn collections_round_trip() {
    let f = fx();
    let g = f.global.clone();
    let p = page(&g, all(), 10);
    let best = block_on(library_create_collection(
        g.clone(),
        "Best".into(),
        None,
        vec![p.ids[0]],
        tok(),
    ))
    .unwrap();
    assert_eq!(
        block_on(library_collection_members(
            g.clone(),
            best,
            vec![p.ids[1]],
            false,
            tok()
        ))
        .unwrap(),
        1
    );
    assert_eq!(
        block_on(library_collection_members(
            g.clone(),
            best,
            vec![p.ids[0]],
            true,
            tok()
        ))
        .unwrap(),
        1
    );
    let in_best = LibraryQueryRecord {
        collection: Some(best),
        ..all()
    };
    assert_eq!(files(&page(&g, in_best, 10)), ["b.ARW"]);

    let rules = LibraryQueryRecord {
        labels: vec![ColorLabel::Red],
        pick: LibraryPickFilter::Unrejected,
        sort: LibrarySortKey::Name,
        cameras: vec!["ILCE-9".into()],
        keywords: vec!["k".into()],
        text: "x".into(),
        sessions: vec![1],
        from_ms: Some(1),
        to_ms: Some(2),
        min_rating: 1,
        unrated: true,
        descending: true,
        collection: None,
    };
    let smart = block_on(library_create_collection(
        g.clone(),
        "Smart".into(),
        Some(rules.clone()),
        Vec::new(),
        tok(),
    ))
    .unwrap();
    let o = block_on(library_overview(g.clone(), tok())).unwrap();
    let back = o
        .collections
        .iter()
        .find(|c| c.id == smart)
        .unwrap()
        .rules
        .clone()
        .unwrap();
    assert_eq!(
        back,
        LibraryQueryRecord {
            sort: LibrarySortKey::Captured,
            descending: false,
            ..rules
        },
        "the rules round-trip; their order doesn't"
    );
    for (pick, sort) in [
        (LibraryPickFilter::All, LibrarySortKey::Rating),
        (LibraryPickFilter::Picked, LibrarySortKey::Session),
        (LibraryPickFilter::Unflagged, LibrarySortKey::Captured),
        (LibraryPickFilter::Rejected, LibrarySortKey::Name),
    ] {
        let q = LibraryQueryRecord {
            pick,
            sort,
            ..all()
        };
        assert_eq!(
            LibraryQueryRecord::from(ds_session::LibraryQuery::from(q.clone())),
            q
        );
    }
    assert!(
        block_on(library_update_collection(
            g.clone(),
            smart,
            "Renamed".into(),
            Some(all()),
            tok()
        ))
        .unwrap()
    );
    let o = block_on(library_overview(g.clone(), tok())).unwrap();
    let names: Vec<(&str, u64)> = o
        .collections
        .iter()
        .map(|c| (c.name.as_str(), c.count))
        .collect();
    assert_eq!(names, [("Best", 1), ("Renamed", 2)]);
    assert!(matches!(
        block_on(library_collection_members(
            g.clone(),
            smart,
            vec![p.ids[0]],
            false,
            tok()
        )),
        Err(DsError::InvalidArgument { .. })
    ));
    assert!(block_on(library_delete_collection(g.clone(), smart, tok())).unwrap());
    assert!(!block_on(library_delete_collection(g.clone(), smart, tok())).unwrap());
    assert!(!block_on(library_update_collection(g, smart, "x".into(), None, tok())).unwrap());
}
