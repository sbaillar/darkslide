use super::*;
use crate::mock::{API_KEY, MockImmich, SERVER};

const ALBUM: &str = "Darkslide ★★★★★";
const TRIP: &str = "/mnt/photos/Darkslide/Trip";

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

fn hset(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| (*s).to_owned()).collect()
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

fn path(name: &str) -> String {
    format!("{TRIP}/{name}")
}

fn source(album: &str, names: &[&str]) -> Source {
    Source {
        key: "session-trip".into(),
        scope: TRIP.into(),
        wanted: vec![Wanted {
            album: album.into(),
            paths: names.iter().map(|n| path(n)).collect(),
        }],
    }
}

fn opts() -> SyncOptions {
    SyncOptions {
        scan: false,
        album_description: "Kept in sync by Darkslide".into(),
    }
}

fn run(mock: &MockImmich, state: &mut SyncState, sources: &[Source]) -> SyncReport {
    let c = Client::new(mock, SERVER, API_KEY).unwrap();
    sync(&c, state, sources, &opts()).unwrap()
}

/// Every request is a read or one of the writes Darkslide may make.
fn assert_only_allowed_writes(mock: &MockImmich) {
    for (m, p) in mock.log() {
        let seg: Vec<&str> = p.trim_start_matches('/').split('/').collect();
        let ok = matches!(
            (m.as_str(), seg.as_slice()),
            ("GET", _)
                | (
                    "POST",
                    ["search", "metadata"] | ["albums"] | ["libraries", _, "scan"]
                )
                | ("PUT" | "DELETE", ["albums", _, "assets"])
        );
        assert!(ok, "Darkslide must not send {m} {p}");
    }
}

#[test]
fn plan_adds_wanted_and_removes_only_its_own() {
    let by_path: HashMap<String, String> = [
        (path("a.ARW"), "a"),
        (path("b.ARW"), "b"),
        (path("c.ARW"), "c"),
        (path("d.ARW"), "d"),
    ]
    .into_iter()
    .map(|(p, id)| (p, id.to_owned()))
    .collect();
    // a: wanted, ours, member. b: wanted, not a member yet. c: ours, no longer
    // wanted. d: added by hand, not wanted. h: member by hand, path unknown.
    // x: ours but someone took it out of the album.
    let members = hset(&["a", "c", "d", "h"]);
    let ours = set(&["a", "c", "x"]);
    let wanted = vec![
        path("a.ARW"),
        path("b.ARW"),
        path("new.ARW"),
        path("new.ARW"),
    ];
    let p = plan(&wanted, &by_path, &members, &ours);
    assert_eq!(p.add, strings(&["b"]));
    assert_eq!(p.remove, strings(&["c"]));
    assert_eq!(p.missing, vec![path("new.ARW")]);
    assert_eq!(p.keep, set(&["a"]));
}

#[test]
fn plan_never_claims_existing_members_or_touches_unknown_ones() {
    let by_path: HashMap<String, String> = [(path("a.ARW"), "a".to_owned())].into();
    // Wanted but already in the album (added by hand): not added, not ours.
    let p = plan(&[path("a.ARW")], &by_path, &hset(&["a"]), &BTreeSet::new());
    assert_eq!(p, Plan::default());
    // Ours, but its path vanished from the folder in Immich: left alone.
    let p = plan(&[], &by_path, &hset(&["z"]), &set(&["z"]));
    assert_eq!(p.remove, Vec::<String>::new());
    assert_eq!(p.keep, set(&["z"]));
}

#[test]
fn first_sync_creates_and_fills_then_is_idempotent() {
    let mock = MockImmich::new();
    let a = mock.add_asset(&path("a.ARW"));
    let b = mock.add_asset(&path("b.ARW"));
    mock.add_asset(&path("c.ARW"));
    let mut state = SyncState::default();
    let r = run(&mock, &mut state, &[source(ALBUM, &["a.ARW", "b.ARW"])]);
    let album = mock.album_by_name(ALBUM).unwrap();
    assert_eq!(album.description, "Kept in sync by Darkslide");
    assert_eq!(mock.members(&album.id), {
        let mut v = vec![a.clone(), b.clone()];
        v.sort();
        v
    });
    assert_eq!(r.albums.len(), 1);
    assert!(r.albums[0].created);
    assert_eq!(r.albums[0].added, 2);
    assert_eq!(r.albums[0].id, album.id);
    assert_eq!((r.missing, r.scanned), (0, 0));
    assert_eq!(r.failures, Vec::<String>::new());
    assert_eq!(state.server, SERVER);
    assert_eq!(state.albums[ALBUM].id, album.id);
    assert_eq!(
        state.albums[ALBUM].added["session-trip"],
        [a, b].into_iter().collect()
    );

    let before = mock.writes().len();
    let r = run(&mock, &mut state, &[source(ALBUM, &["a.ARW", "b.ARW"])]);
    assert_eq!(mock.writes().len(), before, "a second sync only reads");
    assert!(!r.albums[0].created);
    assert_eq!((r.albums[0].added, r.albums[0].removed), (0, 0));
    assert_only_allowed_writes(&mock);
}

#[test]
fn rule_no_longer_matching_removes_only_ours() {
    let mock = MockImmich::new();
    let a = mock.add_asset(&path("a.ARW"));
    let b = mock.add_asset(&path("b.ARW"));
    let hand = mock.add_asset(&path("hand.ARW"));
    let mut state = SyncState::default();
    run(&mock, &mut state, &[source(ALBUM, &["a.ARW", "b.ARW"])]);
    let album = mock.album_by_name(ALBUM).unwrap().id;
    // Someone adds a photo by hand in Immich.
    mock.state().albums[0].members.push(hand.clone());

    let r = run(&mock, &mut state, &[source(ALBUM, &["a.ARW"])]);
    assert_eq!(r.albums[0].removed, 1);
    let mut want = vec![a.clone(), hand.clone()];
    want.sort();
    assert_eq!(mock.members(&album), want);
    assert_eq!(state.albums[ALBUM].added["session-trip"], set(&[&a]));

    // Nothing wanted any more: ours go, the hand-added photo stays, and the
    // album is kept.
    let r = run(&mock, &mut state, &[source(ALBUM, &[])]);
    assert_eq!(r.albums[0].removed, 1);
    assert_eq!(mock.members(&album), vec![hand]);
    assert!(state.albums[ALBUM].added.is_empty());
    assert!(
        !mock
            .writes()
            .iter()
            .any(|(m, p)| m == "DELETE" && p == &format!("/albums/{album}"))
    );
    let _ = b;
}

#[test]
fn an_album_the_source_no_longer_lists_loses_its_photos() {
    let mock = MockImmich::new();
    let a = mock.add_asset(&path("a.ARW"));
    let mut state = SyncState::default();
    run(&mock, &mut state, &[source("Old rule", &["a.ARW"])]);
    let old = mock.album_by_name("Old rule").unwrap().id;
    assert_eq!(mock.members(&old), vec![a.clone()]);
    let r = run(&mock, &mut state, &[source(ALBUM, &["a.ARW"])]);
    assert_eq!(mock.members(&old), Vec::<String>::new());
    assert_eq!(
        mock.members(&mock.album_by_name(ALBUM).unwrap().id),
        vec![a]
    );
    assert!(!state.albums.contains_key("Old rule"));
    let old_report = r.albums.iter().find(|x| x.name == "Old rule").unwrap();
    assert_eq!(old_report.removed, 1);
    // Deleted on the server meanwhile: never recreated just to empty it.
    let mut state2 = state.clone();
    run(&mock, &mut state2, &[source("Old rule", &["a.ARW"])]);
    let again = mock.album_by_name("Old rule").unwrap().id;
    mock.delete_album(&again);
    let n = mock.state().albums.len();
    run(&mock, &mut state2, &[source(ALBUM, &["a.ARW"])]);
    assert_eq!(mock.state().albums.len(), n);
    assert!(!state2.albums.contains_key("Old rule"));
}

#[test]
fn hand_added_member_is_never_claimed_or_removed() {
    let mock = MockImmich::new();
    let a = mock.add_asset(&path("a.ARW"));
    let album = mock.add_album(ALBUM, &[&a]);
    let mut state = SyncState::default();
    let r = run(&mock, &mut state, &[source(ALBUM, &["a.ARW"])]);
    // The existing album is adopted, and `a` was already in it.
    assert!(!r.albums[0].created);
    assert_eq!(r.albums[0].id, album);
    assert_eq!(r.albums[0].added, 0);
    assert!(state.albums[ALBUM].added.is_empty());
    run(&mock, &mut state, &[source(ALBUM, &[])]);
    assert_eq!(mock.members(&album), vec![a]);
    assert_eq!(mock.state().albums.len(), 1);
}

#[test]
fn album_deleted_on_the_server_is_recreated() {
    let mock = MockImmich::new();
    let a = mock.add_asset(&path("a.ARW"));
    let mut state = SyncState::default();
    run(&mock, &mut state, &[source(ALBUM, &["a.ARW"])]);
    let first = mock.album_by_name(ALBUM).unwrap().id;
    mock.delete_album(&first);
    let r = run(&mock, &mut state, &[source(ALBUM, &["a.ARW"])]);
    let second = mock.album_by_name(ALBUM).unwrap().id;
    assert_ne!(first, second);
    assert!(r.albums[0].created);
    assert_eq!(r.albums[0].added, 1);
    assert_eq!(state.albums[ALBUM].id, second);
    assert_eq!(mock.members(&second), vec![a]);
}

#[test]
fn renamed_album_keeps_its_id() {
    let mock = MockImmich::new();
    mock.add_asset(&path("a.ARW"));
    let mut state = SyncState::default();
    run(&mock, &mut state, &[source(ALBUM, &["a.ARW"])]);
    mock.state().albums[0].name = "Best of".into();
    let r = run(&mock, &mut state, &[source(ALBUM, &["a.ARW"])]);
    assert_eq!(mock.state().albums.len(), 1);
    assert!(!r.albums[0].created);
}

#[test]
fn not_scanned_yet_then_found_on_the_next_sync() {
    let mock = MockImmich::new();
    let lib = mock.add_library("NAS", &["/mnt/photos"]);
    mock.add_library("Other", &["/srv/other"]);
    mock.state().lazy_scan = true;
    let a = mock.add_pending_asset(&path("a.ARW"));
    let c = Client::new(&mock, SERVER, API_KEY).unwrap();
    let mut state = SyncState::default();
    let o = SyncOptions {
        scan: true,
        ..opts()
    };
    let src = [source(ALBUM, &["a.ARW"])];
    let r = sync(&c, &mut state, &src, &o).unwrap();
    assert_eq!(r.scanned, 1);
    assert_eq!(r.missing, 1);
    assert_eq!(r.albums[0].missing, 1);
    assert!(r.scan_note.is_none());
    let album = mock.album_by_name(ALBUM).unwrap().id;
    assert_eq!(mock.members(&album), Vec::<String>::new());
    let r = sync(&c, &mut state, &src, &o).unwrap();
    assert_eq!(r.missing, 0);
    assert_eq!(r.albums[0].added, 1);
    assert_eq!(mock.members(&album), vec![a]);
    let s = mock.state();
    assert_eq!(s.libraries[0].scans, 2);
    assert_eq!(s.libraries[0].id, lib);
    assert_eq!(s.libraries[1].scans, 0, "only libraries holding a source");
    drop(s);
    assert_only_allowed_writes(&mock);
}

#[test]
fn non_admin_keys_get_a_scan_note() {
    let mock = MockImmich::new();
    mock.add_library("NAS", &["/mnt/photos"]);
    mock.state().is_admin = false;
    mock.add_asset(&path("a.ARW"));
    let c = Client::new(&mock, SERVER, API_KEY).unwrap();
    let mut state = SyncState::default();
    let o = SyncOptions {
        scan: true,
        ..opts()
    };
    let r = sync(&c, &mut state, &[source(ALBUM, &["a.ARW"])], &o).unwrap();
    assert_eq!(r.scanned, 0);
    assert_eq!(r.scan_note.as_deref(), Some(SCAN_NOTE));
    assert_eq!(r.albums[0].added, 1);
}

#[test]
fn scan_failures_are_reported_or_fatal() {
    let mock = MockImmich::new();
    let lib = mock.add_library("NAS", &["/mnt/photos"]);
    let c = Client::new(&mock, SERVER, API_KEY).unwrap();
    let o = SyncOptions {
        scan: true,
        ..opts()
    };
    let src = [source(ALBUM, &[])];
    mock.force("POST", &format!("/libraries/{lib}/scan"), 403);
    let r = sync(&c, &mut SyncState::default(), &src, &o).unwrap();
    assert_eq!(r.scan_note.as_deref(), Some(SCAN_NOTE));
    mock.state().forced.clear();
    mock.force("POST", &format!("/libraries/{lib}/scan"), 500);
    let r = sync(&c, &mut SyncState::default(), &src, &o).unwrap();
    assert_eq!(r.scanned, 0);
    assert!(
        r.failures[0].contains("Could not scan library “NAS”"),
        "{:?}",
        r.failures
    );
    mock.state().forced.clear();
    mock.force("GET", "/libraries", 500);
    let r = sync(&c, &mut SyncState::default(), &src, &o).unwrap();
    assert!(r.failures[0].contains("libraries"));
    mock.state().forced.clear();
    mock.force("GET", "/libraries", 401);
    assert!(matches!(
        sync(&c, &mut SyncState::default(), &src, &o),
        Err(ImmichError::Auth(_))
    ));
}

#[test]
fn offline_fails_and_leaves_state_alone() {
    let mock = MockImmich::new();
    mock.add_asset(&path("a.ARW"));
    let mut state = SyncState::default();
    run(&mock, &mut state, &[source(ALBUM, &["a.ARW"])]);
    let saved = state.clone();
    mock.state().offline = true;
    let c = Client::new(&mock, SERVER, API_KEY).unwrap();
    let e = sync(&c, &mut state, &[source(ALBUM, &[])], &opts()).unwrap_err();
    assert!(e.is_offline());
    assert_eq!(state, saved);
}

#[test]
fn wrong_key_and_cancel_abort() {
    let mock = MockImmich::new();
    mock.add_asset(&path("a.ARW"));
    let c = Client::new(&mock, SERVER, "revoked").unwrap();
    let e = sync(
        &c,
        &mut SyncState::default(),
        &[source(ALBUM, &["a.ARW"])],
        &opts(),
    );
    assert!(matches!(e, Err(ImmichError::Auth(_))));
    let cancelled = || true;
    let c = Client::new(&mock, SERVER, API_KEY)
        .unwrap()
        .with_cancel(&cancelled);
    let e = sync(
        &c,
        &mut SyncState::default(),
        &[source(ALBUM, &["a.ARW"])],
        &opts(),
    );
    assert!(matches!(e, Err(ImmichError::Cancelled)));
    assert_eq!(mock.writes(), Vec::<(String, String)>::new());
}

#[test]
fn partial_failures_are_recorded_and_the_run_goes_on() {
    let mock = MockImmich::new();
    let a = mock.add_asset(&path("a.ARW"));
    let b = mock.add_asset(&path("b.ARW"));
    mock.state().no_permission.insert(b.clone());
    let mut state = SyncState::default();
    let two = [
        source(ALBUM, &["a.ARW", "b.ARW"]),
        Source {
            key: "session-two".into(),
            scope: TRIP.into(),
            wanted: vec![Wanted {
                album: "Second".into(),
                paths: vec![path("a.ARW")],
            }],
        },
    ];
    let r = run(&mock, &mut state, &two);
    assert_eq!(r.failures.len(), 1, "{:?}", r.failures);
    assert!(r.failures[0].contains("could not be added (no_permission)"));
    assert_eq!(state.albums[ALBUM].added["session-trip"], set(&[&a]));
    assert_eq!(state.albums["Second"].added["session-two"], set(&[&a]));

    // A whole request refused: reported, the other album still synced.
    let album = state.albums[ALBUM].id.clone();
    mock.state().no_permission.clear();
    mock.force("PUT", &format!("/albums/{album}/assets"), 500);
    let r = run(&mock, &mut state, &two);
    assert!(r.failures[0].contains("adding photos"), "{:?}", r.failures);
    assert_eq!(state.albums[ALBUM].added["session-trip"], set(&[&a]));
    mock.force("DELETE", &format!("/albums/{album}/assets"), 500);
    let r = run(&mock, &mut state, &[source(ALBUM, &[])]);
    assert!(
        r.failures[0].contains("removing photos"),
        "{:?}",
        r.failures
    );
    // Still ours: the next sync tries again.
    assert_eq!(state.albums[ALBUM].added["session-trip"], set(&[&a]));
    mock.state().forced.clear();
    mock.state().no_permission.insert(a.clone());
    let r = run(&mock, &mut state, &[source(ALBUM, &[])]);
    assert!(r.failures[0].contains("could not be removed (no_permission)"));
    assert_eq!(state.albums[ALBUM].added["session-trip"], set(&[&a]));
    mock.state().no_permission.clear();
    let r = run(&mock, &mut state, &[source(ALBUM, &[])]);
    assert_eq!(r.failures, Vec::<String>::new());
    assert!(state.albums[ALBUM].added.is_empty());
    assert_only_allowed_writes(&mock);
}

#[test]
fn album_and_folder_failures_skip_just_that_part() {
    let mock = MockImmich::new();
    mock.add_asset(&path("a.ARW"));
    mock.add_asset("/mnt/photos/Darkslide/Other/b.ARW");
    let other = Source {
        key: "session-other".into(),
        scope: "/mnt/photos/Darkslide/Other".into(),
        wanted: vec![Wanted {
            album: "Other".into(),
            paths: vec!["/mnt/photos/Darkslide/Other/b.ARW".into()],
        }],
    };
    let empty_scope = Source {
        key: "session-nowhere".into(),
        scope: " / ".into(),
        wanted: vec![],
    };
    mock.force("POST", "/albums", 500);
    let mut state = SyncState::default();
    let r = run(
        &mock,
        &mut state,
        &[source(ALBUM, &["a.ARW"]), other.clone(), empty_scope],
    );
    assert_eq!(r.failures.len(), 3, "{:?}", r.failures);
    assert!(r.failures.iter().any(|f| f.contains("session-nowhere")));
    assert_eq!(r.albums, Vec::<AlbumReport>::new());
    mock.state().forced.clear();
    run(&mock, &mut state, std::slice::from_ref(&other));
    let id = state.albums["Other"].id.clone();
    // Members can't be read: that album is skipped.
    mock.force("POST", "/search/metadata", 500);
    let r = run(&mock, &mut state, &[other]);
    assert!(
        r.failures[0].starts_with("Album “Other”"),
        "{:?}",
        r.failures
    );
    assert_eq!(state.albums["Other"].id, id);
}

#[test]
fn folder_search_failure_skips_that_source() {
    let mock = MockImmich::new();
    mock.add_asset(&path("a.ARW"));
    let mut state = SyncState::default();
    let c = Client::new(&mock, SERVER, API_KEY)
        .unwrap()
        .with_max_pages(1);
    mock.state().page_size = 1;
    mock.add_asset(&path("b.ARW"));
    let r = sync(&c, &mut state, &[source(ALBUM, &["a.ARW"])], &opts()).unwrap();
    assert!(
        r.failures[0].starts_with("Session session-trip"),
        "{:?}",
        r.failures
    );
    assert_eq!(r.albums[0].added, 0);
}

#[test]
fn an_asset_another_source_wants_stays() {
    let mock = MockImmich::new();
    let a = mock.add_asset(&path("a.ARW"));
    let mut state = SyncState::default();
    run(&mock, &mut state, &[source(ALBUM, &["a.ARW"])]);
    let other = Source {
        key: "session-parent".into(),
        scope: "/mnt/photos/Darkslide".into(),
        wanted: vec![Wanted {
            album: ALBUM.into(),
            paths: vec![path("a.ARW")],
        }],
    };
    run(&mock, &mut state, &[source(ALBUM, &[]), other]);
    let album = state.albums[ALBUM].id.clone();
    assert_eq!(mock.members(&album), vec![a.clone()]);
    assert_eq!(state.albums[ALBUM].added["session-trip"], set(&[&a]));
}

#[test]
fn state_round_trips_and_survives_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("immich/state.json");
    assert_eq!(SyncState::load(&file).unwrap(), SyncState::default());
    let mut s = SyncState {
        server: SERVER.into(),
        ..SyncState::default()
    };
    s.albums.insert(
        ALBUM.into(),
        AlbumRecord {
            id: "album-1".into(),
            added: [("session-trip".to_owned(), set(&["a", "b"]))].into(),
        },
    );
    s.save(&file).unwrap();
    assert_eq!(SyncState::load(&file).unwrap(), s);
    std::fs::write(&file, b"{ not json").unwrap();
    assert_eq!(SyncState::load(&file).unwrap(), SyncState::default());
    // Real I/O errors are errors.
    assert!(matches!(
        SyncState::load(dir.path()),
        Err(ImmichError::Io { .. })
    ));
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, b"x").unwrap();
    assert!(matches!(
        s.save(&blocker.join("state.json")),
        Err(ImmichError::Io { .. })
    ));
}

#[test]
fn another_server_resets_the_state() {
    let mut s = SyncState {
        server: SERVER.into(),
        ..SyncState::default()
    };
    s.albums.insert(ALBUM.into(), AlbumRecord::default());
    s.reset_if_server_changed(SERVER);
    assert_eq!(s.albums.len(), 1);
    s.reset_if_server_changed("https://other.example.org");
    assert!(s.albums.is_empty());
    assert_eq!(s.server, "https://other.example.org");

    // A sync against another server never removes by the old ids.
    let mock = MockImmich::new();
    let a = mock.add_asset(&path("a.ARW"));
    let album = mock.add_album(ALBUM, &[&a]);
    let mut stale = SyncState {
        server: "https://old.example.org".into(),
        ..SyncState::default()
    };
    stale.albums.insert(
        ALBUM.into(),
        AlbumRecord {
            id: album.clone(),
            added: [("session-trip".to_owned(), set(&[&a]))].into(),
        },
    );
    run(&mock, &mut stale, &[source(ALBUM, &[])]);
    assert_eq!(mock.members(&album), vec![a]);
    assert_eq!(stale.server, SERVER);
}
