use std::fs;
use std::path::{Path, PathBuf};

use futures_executor::block_on;
use lt_core::FileHash;
use lt_immich::mock::{API_KEY, MockImmich, SERVER};
use lt_session::{ImageMetaLite, Session};

use super::*;
use crate::archive::ArchiveConfig;
use crate::session::open_session;

/// The app's `URLSession` stand-in: the mock Immich server behind the FFI types.
struct Http(Arc<MockImmich>);

impl ImmichHttp for Http {
    fn send(&self, r: ImmichHttpRequest) -> Result<ImmichHttpResponse, DsError> {
        let answer = lt_immich::Transport::send(
            self.0.as_ref(),
            lt_immich::Request {
                method: r.method,
                url: r.url,
                headers: r.headers.into_iter().map(|h| (h.name, h.value)).collect(),
                body: r.body,
            },
        )
        .map_err(|message| DsError::Io { message })?;
        Ok(ImmichHttpResponse {
            status: answer.status,
            headers: Vec::new(),
            body: answer.body,
        })
    }
}

fn setup() -> (Arc<MockImmich>, Arc<dyn ImmichHttp>) {
    let m = Arc::new(MockImmich::new());
    (m.clone(), Arc::new(Http(m)))
}

fn server() -> ImmichServer {
    ImmichServer {
        url: SERVER.into(),
        api_key: API_KEY.into(),
    }
}

const OPTS: ImmichDevelopOptions = ImmichDevelopOptions {
    quality: 90,
    long_edge: None,
    label_tags: true,
};

/// An imported session `name` under `parent` with these files under `Capture/`.
fn session(parent: &Path, name: &str, files: &[&str]) -> Arc<SessionHandle> {
    let root = parent.join(name);
    let mut s = Session::create(&root, name).unwrap();
    let mut items = Vec::new();
    for f in files {
        let rel = format!("Capture/{f}");
        let p = root.join(&rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, f.as_bytes()).unwrap();
        items.push((
            rel,
            FileHash(*blake3::hash(f.as_bytes()).as_bytes()),
            ImageMetaLite::default(),
        ));
    }
    s.add_originals(items).unwrap();
    drop(s);
    block_on(open_session(
        root.to_string_lossy().into_owned(),
        CancelToken::new(),
    ))
    .unwrap()
}

/// Variant ids in file order.
fn ids(h: &SessionHandle) -> Vec<i64> {
    let s = lock(&h.session);
    let mut v: Vec<(String, i64)> = s
        .all_variants()
        .unwrap()
        .into_iter()
        .map(|id| {
            let info = s.variant(id).unwrap();
            (
                s.image(info.image).unwrap().rel_path.as_str().to_owned(),
                id.0,
            )
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, id)| id).collect()
}

fn rate(h: &SessionHandle, id: i64, stars: u8) {
    lock(&h.session)
        .set_rating(&[VariantId(id)], Rating::new(stars).unwrap())
        .unwrap();
}

fn root(h: &SessionHandle) -> PathBuf {
    PathBuf::from(h.root().unwrap())
}

/// "Render" a job as the app's export would: a file in the staging folder.
fn render(plan: &ImmichDevelopPlan, job: &ImmichDevelopJob, bytes: &[u8]) -> String {
    let p = Path::new(&plan.staging_dir).join(format!("{}.jpg", job.variant_id));
    fs::write(&p, bytes).unwrap();
    p.to_string_lossy().into_owned()
}

fn states(h: &SessionHandle) -> Vec<(i64, ImmichCopyState)> {
    let mut v: Vec<_> = h
        .immich_developed(OPTS)
        .unwrap()
        .into_iter()
        .map(|d| (d.variant_id, d.state))
        .collect();
    v.sort_by_key(|(id, _)| ids(h).iter().position(|i| i == id));
    v
}

#[test]
fn developed_copies_follow_edits_and_survive_unmarking() {
    let t = tempfile::tempdir().unwrap();
    let h = session(
        t.path(),
        "Shoot",
        &["Day/IMG_1.ARW", "Day/IMG_1.JPG", "IMG_2.ARW"],
    );
    let v = ids(&h);
    let r = h.immich_mark_developed(vec![v[0], v[2]], true).unwrap();
    assert_eq!(r.changed, 2);
    assert!(root(&h).join("Developed/.luxtable-developed").is_file());
    assert_eq!(
        states(&h),
        [
            (v[0], ImmichCopyState::Pending),
            (v[2], ImmichCopyState::Pending)
        ]
    );
    let plan = h.immich_develop_plan(OPTS).unwrap();
    let mut rels: Vec<&str> = plan.jobs.iter().map(|j| j.rel.as_str()).collect();
    rels.sort_unstable();
    assert_eq!(
        rels,
        [
            "Developed/Capture/Day/IMG_1_ARW.jpg",
            "Developed/Capture/IMG_2.jpg"
        ],
        "named after the original, disambiguated when two share a stem"
    );
    for j in &plan.jobs {
        let staged = render(&plan, j, b"v1");
        let placed = h
            .immich_developed_rendered(j.clone(), staged)
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(&placed).unwrap(), b"v1");
    }
    assert!(
        h.immich_develop_plan(OPTS).unwrap().jobs.is_empty(),
        "all current"
    );
    assert_eq!(
        states(&h),
        [
            (v[0], ImmichCopyState::Current),
            (v[2], ImmichCopyState::Current)
        ]
    );
    // Re-rated: out of date, rendered again in place.
    rate(&h, v[0], 4);
    assert_eq!(states(&h)[0], (v[0], ImmichCopyState::OutOfDate));
    let plan = h.immich_develop_plan(OPTS).unwrap();
    assert_eq!(plan.jobs.len(), 1);
    // An edit changes it too.
    {
        let s = lock(&h.session);
        let mut a = s.load_adjustments(VariantId(v[2])).unwrap();
        a.exposure.exposure_ev = 1.0;
        drop(s);
        lock(&h.session)
            .save_adjustments(VariantId(v[2]), &a)
            .unwrap();
    }
    let plan = h.immich_develop_plan(OPTS).unwrap();
    assert_eq!(plan.jobs.len(), 2);
    let job = plan
        .jobs
        .iter()
        .find(|j| j.variant_id == v[0])
        .unwrap()
        .clone();
    let staged = render(&plan, &job, b"v2");
    let placed = h.immich_developed_rendered(job, staged).unwrap().unwrap();
    assert_eq!(
        fs::read(&placed).unwrap(),
        b"v2",
        "the old copy is replaced"
    );
    // Other options make every copy out of date.
    let q = ImmichDevelopOptions {
        quality: 80,
        ..OPTS
    };
    assert_eq!(h.immich_develop_plan(q).unwrap().jobs.len(), 2);
    // Stop developing: the file stays, and is reported.
    let r = h.immich_mark_developed(vec![v[0]], false).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(r.kept_files, std::slice::from_ref(&placed));
    assert_eq!(fs::read(&placed).unwrap(), b"v2");
    assert_eq!(states(&h).len(), 1);
}

#[test]
fn renders_are_checked_before_they_are_placed() {
    let t = tempfile::tempdir().unwrap();
    let h = session(t.path(), "Shoot", &["a.ARW"]);
    let v = ids(&h);
    h.immich_mark_developed(v.clone(), true).unwrap();
    let plan = h.immich_develop_plan(OPTS).unwrap();
    let job = plan.jobs[0].clone();
    // Only files from the staging folder are taken.
    let outside = t.path().join("x.jpg");
    fs::write(&outside, b"x").unwrap();
    assert!(matches!(
        h.immich_developed_rendered(job.clone(), outside.to_string_lossy().into_owned()),
        Err(DsError::InvalidArgument { .. })
    ));
    assert!(outside.exists());
    // Nor one that only looks like it is, through `..`, even when unmarked
    // (which would otherwise remove it).
    let up = Path::new(&plan.staging_dir)
        .strip_prefix(t.path())
        .unwrap()
        .components()
        .count();
    let sneaky = format!("{}/{}x.jpg", plan.staging_dir, "../".repeat(up));
    assert_eq!(
        Path::new(&sneaky).canonicalize().ok(),
        outside.canonicalize().ok()
    );
    h.immich_mark_developed(v.clone(), false).unwrap();
    assert!(matches!(
        h.immich_developed_rendered(job.clone(), sneaky),
        Err(DsError::InvalidArgument { .. })
    ));
    assert!(outside.exists());
    h.immich_mark_developed(v.clone(), true).unwrap();
    // The copy's path must be a file under Developed/: absolute paths, `..`
    // and other folders are refused, and the render is left where it is.
    let staged = render(&plan, &job, b"r");
    let elsewhere = t.path().join("elsewhere.jpg");
    for rel in [
        elsewhere.to_string_lossy().into_owned(),
        "Developed/../a.ARW".into(),
        "Capture/x.jpg".into(),
        "Developed".into(),
        String::new(),
    ] {
        let bad = ImmichDevelopJob {
            rel: rel.clone(),
            ..job.clone()
        };
        assert!(
            matches!(
                h.immich_developed_rendered(bad, staged.clone()),
                Err(DsError::InvalidArgument { .. })
            ),
            "{rel}"
        );
        assert!(Path::new(&staged).exists(), "{rel}");
    }
    assert!(!elsewhere.exists());
    // Unmarked while rendering: the render is dropped, nothing placed.
    let staged = render(&plan, &job, b"r");
    h.immich_mark_developed(v.clone(), false).unwrap();
    assert_eq!(
        h.immich_developed_rendered(job.clone(), staged.clone())
            .unwrap(),
        None
    );
    assert!(!Path::new(&staged).exists());
    assert!(!root(&h).join(&job.rel).exists());
    // A file Luxtable didn't make at the copy's path is never replaced.
    h.immich_mark_developed(v, true).unwrap();
    fs::create_dir_all(root(&h).join("Developed/Capture")).unwrap();
    fs::write(root(&h).join(&job.rel), b"mine").unwrap();
    let staged = render(&plan, &job, b"r");
    assert!(matches!(
        h.immich_developed_rendered(job.clone(), staged),
        Err(DsError::Unsupported { .. })
    ));
    assert_eq!(fs::read(root(&h).join(&job.rel)).unwrap(), b"mine");
}

#[test]
fn a_foreign_developed_folder_is_refused() {
    let t = tempfile::tempdir().unwrap();
    let h = session(t.path(), "Shoot", &["a.ARW"]);
    fs::create_dir_all(root(&h).join("Developed")).unwrap();
    fs::write(root(&h).join("Developed/mine.jpg"), b"m").unwrap();
    let e = h.immich_mark_developed(ids(&h), true).unwrap_err();
    assert!(e.to_string().contains("isn't Luxtable's"), "{e}");
    // Nothing marked: no plan, no states.
    assert_eq!(h.immich_develop_plan(OPTS).unwrap().jobs, []);
    assert_eq!(h.immich_developed(OPTS).unwrap(), []);
}

#[test]
fn sidecars_are_written_and_foreign_ones_left_alone() {
    let t = tempfile::tempdir().unwrap();
    let h = session(t.path(), "Shoot", &["a.ARW", "b.ARW"]);
    let v = ids(&h);
    rate(&h, v[0], 5);
    lock(&h.session)
        .set_color_tag(&[VariantId(v[0])], ColorTag::Red)
        .unwrap();
    let lr = root(&h).join("Capture/b.ARW.xmp");
    fs::write(&lr, b"<x:xmpmeta>Lightroom</x:xmpmeta>").unwrap();
    let r = h.immich_refresh_sidecars(true).unwrap();
    assert_eq!(r.written, 1);
    assert_eq!(r.foreign, ["Capture/b.ARW.xmp"]);
    assert_eq!(fs::read(&lr).unwrap(), b"<x:xmpmeta>Lightroom</x:xmpmeta>");
    let ours = fs::read_to_string(root(&h).join("Capture/a.ARW.xmp")).unwrap();
    assert!(ours.contains("<xmp:Rating>5</xmp:Rating>"), "{ours}");
    assert!(ours.contains("Luxtable/Label/Red"));
    // Unchanged: nothing written; changed: rewritten.
    let r = h.immich_refresh_sidecars(true).unwrap();
    assert_eq!((r.written, r.unchanged), (0, 1));
    rate(&h, v[0], 2);
    assert_eq!(h.immich_refresh_sidecars(false).unwrap().written, 1);
    let ours = fs::read_to_string(root(&h).join("Capture/a.ARW.xmp")).unwrap();
    assert!(ours.contains("<xmp:Rating>2</xmp:Rating>") && !ours.contains("Label/Red"));
    // Edited by another app since: left alone from now on.
    fs::write(
        root(&h).join("Capture/a.ARW.xmp"),
        ours.replace("Rating>2", "Rating>3"),
    )
    .unwrap();
    rate(&h, v[0], 1);
    let r = h.immich_refresh_sidecars(false).unwrap();
    assert_eq!(r.foreign.len(), 2);
    assert!(
        fs::read_to_string(root(&h).join("Capture/a.ARW.xmp"))
            .unwrap()
            .contains("Rating>3")
    );
    // The archive hook writes them only when asked.
    let p = ArchivePublish {
        xmp_sidecars: false,
        label_tags: true,
    };
    assert_eq!(before_archive(&h, &p), Vec::<String>::new());
}

#[test]
fn connection_test_and_mapping_detection() {
    let (m, http) = setup();
    m.add_library("NAS", &["/mnt/photos"]);
    let c = block_on(immich_test_connection(
        http.clone(),
        server(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(c.libraries[0].import_paths, ["/mnt/photos"]);
    assert!(c.libraries_note.is_none());
    m.state().is_admin = false;
    let c = block_on(immich_test_connection(
        http.clone(),
        server(),
        CancelToken::new(),
    ))
    .unwrap();
    assert!(c.libraries.is_empty() && c.libraries_note.is_some());
    // From an archived file Immich indexed.
    m.add_asset("/mnt/photos/Luxtable/Shoot/Capture/a.ARW");
    let d = block_on(immich_detect_mapping(
        http.clone(),
        server(),
        "/Volumes/photos/Luxtable".into(),
        Some("/Volumes/photos/Luxtable/Shoot/Capture/a.ARW".into()),
        CancelToken::new(),
    ))
    .unwrap()
    .unwrap();
    assert_eq!(d.how, "asset");
    let found = d.mapping.mapping().unwrap();
    assert_eq!(
        found
            .to_immich(Path::new("/Volumes/photos/Luxtable/Shoot/x.jpg"))
            .as_deref(),
        Some("/mnt/photos/Luxtable/Shoot/x.jpg")
    );
    // A wrong key, a bad address, no server.
    let bad = ImmichServer {
        api_key: "nope".into(),
        ..server()
    };
    let e = block_on(immich_test_connection(
        http.clone(),
        bad,
        CancelToken::new(),
    ))
    .unwrap_err();
    assert!(matches!(e, DsError::InvalidArgument { .. }), "{e}");
    let bad = ImmichServer {
        url: "immich".into(),
        ..server()
    };
    assert!(
        block_on(immich_test_connection(
            http.clone(),
            bad,
            CancelToken::new()
        ))
        .is_err()
    );
    m.state().offline = true;
    let e = block_on(immich_test_connection(http, server(), CancelToken::new())).unwrap_err();
    assert!(e.to_string().contains("not reachable"), "{e}");
    assert!(
        !format!("{:?}", server()).contains(API_KEY),
        "the key is never logged"
    );
}

fn manager(target: &Path) -> Arc<ArchiveManager> {
    ArchiveManager::ffi_new(ArchiveConfig {
        enabled: false,
        target: target.to_string_lossy().into_owned(),
        interval_secs: 3600,
        debounce_secs: 0,
        lock_dir: None,
        check_timeout_ms: 2000,
        rsync_path: None,
        archive_rejected: true,
        amazon_rejected: false,
        target_volume: None,
        object_store: None,
    })
    .unwrap()
}

/// As if an archive run claimed `folder` for the session.
fn archived(h: &SessionHandle, folder: &str) {
    fs::write(
        root(h).join(".lt/archive.json"),
        format!(r#"{{"id":"abc123","folder":"{folder}"}}"#),
    )
    .unwrap();
}

fn mapping(target: &Path) -> ImmichPathMapping {
    ImmichPathMapping {
        immich_root: "/mnt/photos".into(),
        local_root: target.to_string_lossy().into_owned(),
    }
}

fn five_stars() -> ImmichRule {
    ImmichRule {
        album: "Luxtable ★★★★★".into(),
        min_rating: 5,
        pick: None,
        labels: vec![],
        keywords: vec![],
    }
}

fn sync(
    http: &Arc<dyn ImmichHttp>,
    m: &Arc<ArchiveManager>,
    target: &Path,
    state: &Path,
    rules: Vec<ImmichRule>,
) -> ImmichSyncReport {
    block_on(immich_sync_albums(
        http.clone(),
        server(),
        mapping(target),
        rules,
        m.clone(),
        state.to_string_lossy().into_owned(),
        true,
        true,
        CancelToken::new(),
    ))
    .unwrap()
}

#[test]
fn albums_follow_the_rules_with_developed_copies() {
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("archive");
    fs::create_dir_all(&target).unwrap();
    let state = t.path().join("immich-albums.json");
    let h = session(t.path(), "Shoot", &["a.ARW", "b.ARW", "c.ARW"]);
    let v = ids(&h);
    rate(&h, v[0], 5);
    rate(&h, v[1], 5);
    archived(&h, "Shoot");
    let m = manager(&target);
    m.register_session(h.clone()).unwrap();
    let (mock, http) = setup();
    mock.add_library("NAS", &["/mnt/photos"]);
    let a = mock.add_asset("/mnt/photos/Shoot/Capture/a.ARW");
    mock.add_asset("/mnt/photos/Shoot/Capture/b.ARW");
    let c = mock.add_asset("/mnt/photos/Shoot/Capture/c.ARW");
    // b has a developed copy: the album gets it instead of the RAW.
    h.immich_mark_developed(vec![v[1]], true).unwrap();
    let plan = h.immich_develop_plan(OPTS).unwrap();
    let staged = render(&plan, &plan.jobs[0], b"jpeg");
    h.immich_developed_rendered(plan.jobs[0].clone(), staged)
        .unwrap();
    let bj = mock.add_asset("/mnt/photos/Shoot/Developed/Capture/b.jpg");
    // The user's own photo in the album, by hand.
    let album = mock.add_album("Luxtable ★★★★★", &[c.as_str()]);
    let mut empty = five_stars();
    empty.min_rating = 0;
    let r = sync(&http, &m, &target, &state, vec![five_stars(), empty]);
    assert!(r.error.is_none(), "{r:?}");
    assert_eq!(r.sessions, 1);
    assert!(
        r.failures.iter().any(|f| f.contains("no criterion")),
        "{r:?}"
    );
    assert_eq!(r.albums[0].added, 2);
    let mut members = mock.members(&album);
    members.sort();
    let mut want = vec![a.clone(), bj.clone(), c.clone()];
    want.sort();
    assert_eq!(members, want);
    // a drops to 3 stars: removed; the hand-added c stays.
    rate(&h, v[0], 3);
    let r = sync(&http, &m, &target, &state, vec![five_stars()]);
    assert_eq!(r.albums[0].removed, 1);
    let mut members = mock.members(&album);
    members.sort();
    let mut want = vec![bj, c];
    want.sort();
    assert_eq!(members, want);
    // Nothing but album membership and scans was ever written.
    for (method, path) in mock.writes() {
        assert!(
            path.contains("/albums")
                || path.ends_with("/scan")
                || path.ends_with("/search/metadata"),
            "{method} {path}"
        );
        if method == "DELETE" {
            assert!(
                path.ends_with("/assets") && path.contains("/albums/"),
                "{path}"
            );
        }
    }
}

#[test]
fn photos_not_in_immich_yet_and_offline_are_retried() {
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("archive");
    fs::create_dir_all(&target).unwrap();
    let state = t.path().join("immich-albums.json");
    let h = session(t.path(), "Shoot", &["a.ARW"]);
    rate(&h, ids(&h)[0], 5);
    let m = manager(&target);
    m.set_immich(Some(ImmichPublishConfig {
        xmp_sidecars: true,
        label_tags: true,
    }))
    .unwrap();
    assert_eq!(m.immich_pending().unwrap(), 0);
    m.register_session(h.clone()).unwrap();
    let (mock, http) = setup();
    mock.add_library("NAS", &["/mnt/photos"]);
    // Not archived yet: said so, nothing sent.
    let r = sync(&http, &m, &target, &state, vec![five_stars()]);
    assert!(
        r.failures.iter().any(|f| f.contains("not archived yet")),
        "{r:?}"
    );
    assert_eq!(mock.log(), []);
    archived(&h, "Shoot");
    // Archived but not scanned yet: missing, added on a later sync.
    let r = sync(&http, &m, &target, &state, vec![five_stars()]);
    assert_eq!(r.missing, 1);
    let a = mock.add_asset("/mnt/photos/Shoot/Capture/a.ARW");
    let r = sync(&http, &m, &target, &state, vec![five_stars()]);
    assert_eq!((r.missing, r.albums[0].added), (0, 1));
    let album = mock.album_by_name("Luxtable ★★★★★").unwrap();
    assert_eq!(album.members, [a]);
    // Offline: reported, and the session is queued for the next sync.
    mock.state().offline = true;
    let r = sync(&http, &m, &target, &state, vec![five_stars()]);
    assert!(r.offline && r.error.is_some(), "{r:?}");
    assert_eq!(m.immich_pending().unwrap(), 1);
    // A refused key.
    mock.state().offline = false;
    mock.state().api_key = "other".into();
    let r = sync(&http, &m, &target, &state, vec![five_stars()]);
    assert!(r.key_refused, "{r:?}");
    // A mapping that doesn't cover the archive.
    let bad = ImmichPathMapping {
        immich_root: "/mnt/photos".into(),
        local_root: "/Volumes/elsewhere".into(),
    };
    let r = block_on(immich_sync_albums(
        http.clone(),
        server(),
        bad,
        vec![five_stars()],
        m.clone(),
        state.to_string_lossy().into_owned(),
        false,
        true,
        CancelToken::new(),
    ))
    .unwrap();
    assert!(
        r.failures.iter().any(|f| f.contains("path mapping")),
        "{r:?}"
    );
    let relative = ImmichPathMapping {
        immich_root: "mnt".into(),
        local_root: "x".into(),
    };
    assert!(
        block_on(immich_sync_albums(
            http,
            server(),
            relative,
            vec![],
            m.clone(),
            String::new(),
            false,
            true,
            CancelToken::new(),
        ))
        .is_err()
    );
    m.set_immich(None).unwrap();
    assert_eq!(m.immich_pending().unwrap(), 0);
}

#[test]
fn rules_become_filters() {
    let mut r = five_stars();
    r.min_rating = 0;
    assert!(r.filter().is_none());
    r.keywords = vec![" ".into()];
    r.labels = vec![ColorLabel::None];
    assert!(r.filter().is_none(), "blank criteria don't count");
    r.pick = Some(PickFlag::Picked);
    r.labels = vec![ColorLabel::Green];
    r.keywords = vec![" beach ".into()];
    r.min_rating = 9;
    let f = r.filter().unwrap();
    assert_eq!(f.min_rating.unwrap().stars(), 5);
    assert_eq!(f.pick, Some(PickState::Picked));
    assert_eq!(f.tags, [ColorTag::Green]);
    assert_eq!(f.keywords, ["beach"]);
}

#[test]
fn rejected_photos_keep_their_copies_out_of_the_archive() {
    let t = tempfile::tempdir().unwrap();
    let h = session(t.path(), "Shoot", &["a.ARW"]);
    let v = ids(&h);
    h.immich_mark_developed(v.clone(), true).unwrap();
    let plan = h.immich_develop_plan(OPTS).unwrap();
    let staged = render(&plan, &plan.jobs[0], b"j");
    h.immich_developed_rendered(plan.jobs[0].clone(), staged)
        .unwrap();
    h.immich_refresh_sidecars(true).unwrap();
    lock(&h.session)
        .set_pick(&[VariantId(v[0])], PickState::Rejected)
        .unwrap();
    let ex = h.reject_excludes().unwrap();
    assert!(ex.contains(&"/Capture/a.ARW".to_owned()), "{ex:?}");
    assert!(ex.contains(&"/Capture/a.ARW.xmp".to_owned()), "{ex:?}");
    assert!(
        ex.contains(&"/Developed/Capture/a.jpg".to_owned()),
        "{ex:?}"
    );
}

#[test]
fn developed_folder_names_agree_with_the_session_walk() {
    assert_eq!(publish::DEVELOPED_DIR, lt_session::DEVELOPED_DIR);
    assert_eq!(publish::DEVELOPED_MARKER, lt_session::DEVELOPED_MARKER);
}
