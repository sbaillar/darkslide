use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::mock::{API_KEY, MockImmich, SERVER};

/// Answers every request with the same response, and records them.
pub(crate) struct Fixed(pub Response, pub Mutex<Vec<Request>>);

impl Fixed {
    pub(crate) fn new(status: u16, body: &str) -> Self {
        Self(Response::new(status, body), Mutex::new(Vec::new()))
    }
}

impl Transport for Fixed {
    fn send(&self, request: Request) -> Result<Response, String> {
        self.1.lock().unwrap().push(request);
        Ok(self.0.clone())
    }
}

fn client(mock: &MockImmich) -> Client<'_> {
    Client::new(mock, SERVER, API_KEY).unwrap()
}

fn ids(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("id-{i}")).collect()
}

#[test]
fn server_urls_are_normalized() {
    for (given, want) in [
        (
            "  https://immich.example.org/api/ ",
            "https://immich.example.org",
        ),
        ("https://immich.example.org/", "https://immich.example.org"),
        ("http://nas.local:2283", "http://nas.local:2283"),
        (
            "HTTPS://immich.example.org/api",
            "HTTPS://immich.example.org",
        ),
        (
            "https://example.org/immich/api",
            "https://example.org/immich",
        ),
        ("https://api", "https://api"),
    ] {
        assert_eq!(normalize_server(given).unwrap(), want, "{given}");
    }
    for bad in [
        "immich.example.org",
        "ftp://immich.example.org",
        "https://",
        "https:///api",
        "http://:2283",
        "https://immich example.org",
        "",
    ] {
        assert!(
            matches!(normalize_server(bad), Err(ImmichError::InvalidServer(_))),
            "{bad}"
        );
    }
}

#[test]
fn key_is_required_and_never_printed() {
    let mock = MockImmich::new();
    assert!(matches!(
        Client::new(&mock, SERVER, "  "),
        Err(ImmichError::Auth(_))
    ));
    assert!(matches!(
        Client::new(&mock, "nope", API_KEY),
        Err(ImmichError::InvalidServer(_))
    ));
    let c = Client::new(&mock, &format!("{SERVER}/api"), &format!(" {API_KEY} ")).unwrap();
    assert_eq!(c.server(), SERVER);
    let dbg = format!("{c:?}");
    assert!(dbg.contains(SERVER));
    assert!(!dbg.contains(API_KEY));
    c.me().unwrap();
    let req = &mock.state().requests[0];
    assert_eq!(req.header_value("x-api-key"), Some(API_KEY));
    assert_eq!(req.header_value("accept"), Some("application/json"));
    assert_eq!(req.url, format!("{SERVER}/api/users/me"));
}

#[test]
fn ping_and_me() {
    let mock = MockImmich::new();
    let c = client(&mock);
    c.ping().unwrap();
    let me = c.me().unwrap();
    assert_eq!(me.id, crate::mock::USER_ID);
    assert!(me.is_admin);
    assert_eq!(me.email, "user@example.org");
    let odd = Fixed::new(200, r#"{"res":"nope"}"#);
    let c = Client::new(&odd, SERVER, "k").unwrap();
    assert!(matches!(c.ping(), Err(ImmichError::Protocol(_))));
}

#[test]
fn errors_map_to_what_the_user_can_do() {
    let mock = MockImmich::new();
    let wrong = Client::new(&mock, SERVER, "wrong").unwrap();
    // Ping needs no key.
    wrong.ping().unwrap();
    match wrong.me() {
        Err(ImmichError::Auth(m)) => assert_eq!(m, "Invalid API key"),
        other => panic!("{other:?}"),
    }
    let c = client(&mock);
    match c.create_album("", "") {
        Err(ImmichError::Http { status, message }) => {
            assert_eq!(status, 400);
            assert_eq!(message, "albumName should not be empty");
        }
        other => panic!("{other:?}"),
    }
    mock.force("GET", "/albums", 404);
    assert!(matches!(c.albums(), Err(ImmichError::NotFound(_))));
    mock.force("", "/users", 503);
    assert!(matches!(c.me(), Err(ImmichError::Http { status: 503, .. })));
    mock.state().is_admin = false;
    assert!(matches!(c.libraries(), Err(ImmichError::Forbidden(_))));
    assert!(matches!(
        c.scan_library("lib-1"),
        Err(ImmichError::Forbidden(_))
    ));
    mock.state().offline = true;
    let e = c.ping().unwrap_err();
    assert!(e.is_offline());
    assert!(e.to_string().contains("offline"));
    let garbage = Fixed::new(200, "<html>");
    let c = Client::new(&garbage, SERVER, "k").unwrap();
    assert!(matches!(c.albums(), Err(ImmichError::Protocol(_))));
    assert!(matches!(
        c.assets_under("/x"),
        Err(ImmichError::Protocol(_))
    ));
}

#[test]
fn cancel_stops_before_sending() {
    let mock = MockImmich::new();
    let flag = AtomicBool::new(false);
    let probe = || flag.load(Ordering::Relaxed);
    let c = client(&mock).with_cancel(&probe);
    c.ping().unwrap();
    flag.store(true, Ordering::Relaxed);
    assert!(matches!(c.albums(), Err(ImmichError::Cancelled)));
    assert_eq!(mock.log().len(), 1);
}

#[test]
fn libraries_scans_and_albums() {
    let mock = MockImmich::new();
    let lib = mock.add_library("NAS", &["/mnt/photos"]);
    let c = client(&mock);
    let libs = c.libraries().unwrap();
    assert_eq!(libs.len(), 1);
    assert_eq!(libs[0].import_paths, vec!["/mnt/photos".to_owned()]);
    assert_eq!(libs[0].exclusion_patterns, Vec::<String>::new());
    c.scan_library(&lib).unwrap();
    assert_eq!(mock.state().libraries[0].scans, 1);
    assert!(matches!(
        c.scan_library("nope"),
        Err(ImmichError::Http { status: 400, .. })
    ));
    let a = c
        .create_album("Darkslide ★★★★★", "made by Darkslide")
        .unwrap();
    assert_eq!(a.name, "Darkslide ★★★★★");
    assert_eq!(a.asset_count, 0);
    assert_eq!(c.albums().unwrap(), vec![a.clone()]);
    assert_eq!(
        mock.album_by_name("Darkslide ★★★★★").unwrap().description,
        "made by Darkslide"
    );
}

#[test]
fn search_follows_cursors() {
    let mock = MockImmich::new();
    mock.state().page_size = 2;
    for i in 0..5 {
        mock.add_asset(&format!("/mnt/photos/Trip/IMG_{i}.ARW"));
    }
    let c = client(&mock);
    let found = c.assets_under("/mnt/photos/Trip/").unwrap();
    assert_eq!(found.len(), 5);
    assert_eq!(found[4].original_path, "/mnt/photos/Trip/IMG_4.ARW");
    assert_eq!(found[4].file_name, "IMG_4.ARW");
    let bodies = mock.bodies("/search/metadata");
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[0]["originalPath"], "/mnt/photos/Trip");
    assert_eq!(bodies[0]["size"], SEARCH_PAGE_SIZE);
    assert!(bodies[0].get("cursor").is_none());
    assert_eq!(bodies[1]["cursor"], "c2");
    assert_eq!(bodies[2]["cursor"], "c4");
    // The cursor wins over the page number the server also sends.
    assert!(bodies.iter().all(|b| b.get("page").is_none()));
}

#[test]
fn search_follows_legacy_pages() {
    let mock = MockImmich::new();
    {
        let mut s = mock.state();
        s.page_size = 2;
        s.legacy_paging = true;
    }
    for i in 0..5 {
        mock.add_asset(&format!("/mnt/photos/Trip/IMG_{i}.ARW"));
    }
    let c = client(&mock);
    assert_eq!(c.assets_under("/mnt/photos/Trip").unwrap().len(), 5);
    let pages: Vec<Value> = mock
        .bodies("/search/metadata")
        .iter()
        .map(|b| b["page"].clone())
        .collect();
    assert_eq!(pages, vec![Value::Null, json!(2), json!(3)]);
    let bad = Fixed::new(200, r#"{"assets":{"items":[],"nextPage":"two"}}"#);
    let c = Client::new(&bad, SERVER, "k").unwrap();
    assert!(matches!(
        c.album_asset_ids("a"),
        Err(ImmichError::Protocol(_))
    ));
}

#[test]
fn endless_paging_is_capped() {
    let mock = MockImmich::new();
    mock.state().page_size = 1;
    for i in 0..5 {
        mock.add_asset(&format!("/mnt/photos/IMG_{i}.ARW"));
    }
    let c = client(&mock).with_max_pages(3);
    let e = c.assets_under("/mnt/photos").unwrap_err();
    assert!(matches!(e, ImmichError::Protocol(_)), "{e}");
    assert_eq!(mock.log().len(), 3);
    // `find_by_file_name` reads one page only.
    mock.add_asset("/mnt/other/IMG_1.ARW");
    assert_eq!(c.find_by_file_name("IMG_1.ARW").unwrap().len(), 1);
}

#[test]
fn assets_under_matches_whole_components() {
    let mock = MockImmich::new();
    let a = mock.add_asset("/mnt/photos/Trip/a.ARW");
    let b = mock.add_asset("/mnt/photos/trip/b.ARW");
    mock.add_asset("/mnt/photos/Trip2/c.ARW");
    mock.add_asset("/mnt/old/mnt/photos/Trip/d.ARW");
    let t = mock.add_asset("/mnt/photos/Trip/e.ARW");
    mock.trash(&t);
    let c = client(&mock);
    let found: Vec<String> = c
        .assets_under("/mnt/photos/Trip")
        .unwrap()
        .into_iter()
        .map(|x| x.id)
        .collect();
    assert_eq!(found, vec![a]);
    // Immich's filter is case-insensitive; ours is exact.
    assert!(!found.contains(&b));
}

#[test]
fn finds_files_by_exact_name() {
    let mock = MockImmich::new();
    let a = mock.add_asset("/mnt/photos/A/IMG_1.ARW");
    mock.add_asset("/mnt/photos/A/IMG_10.ARW");
    mock.add_asset("/mnt/photos/B/img_1.arw");
    let t = mock.add_asset("/mnt/photos/C/IMG_1.ARW");
    mock.trash(&t);
    let c = client(&mock);
    let found = c.find_by_file_name("IMG_1.ARW").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, a);
    assert_eq!(found[0].original_path, "/mnt/photos/A/IMG_1.ARW");
}

#[test]
fn album_members_add_and_remove() {
    let mock = MockImmich::new();
    let a = mock.add_asset("/mnt/photos/a.ARW");
    let b = mock.add_asset("/mnt/photos/b.ARW");
    let d = mock.add_asset("/mnt/photos/d.ARW");
    let album = mock.add_album("Picks", &[&a]);
    mock.state().no_permission.insert(d.clone());
    let c = client(&mock);
    let o = c
        .add_to_album(&album, &[a.clone(), b.clone(), "ghost".into(), d.clone()])
        .unwrap();
    assert_eq!(o.done, vec![b.clone()]);
    assert_eq!(o.unchanged, vec![a.clone()]);
    assert_eq!(
        o.failed,
        vec![
            ("ghost".to_owned(), "not_found".to_owned()),
            (d.clone(), "no_permission".to_owned())
        ]
    );
    let mut members = c.album_asset_ids(&album).unwrap();
    members.sort();
    assert_eq!(members, vec![a.clone(), b.clone()]);
    let o = c
        .remove_from_album(&album, &[b.clone(), d.clone()])
        .unwrap();
    assert_eq!(o.done, vec![b.clone()]);
    assert_eq!(o.failed, vec![(d.clone(), "no_permission".to_owned())]);
    let o = c
        .remove_from_album(&album, std::slice::from_ref(&b))
        .unwrap();
    assert_eq!(o.unchanged, vec![b]);
    assert_eq!(c.add_to_album(&album, &[]).unwrap(), BulkOutcome::default());
    assert!(matches!(
        c.add_to_album("gone", &[a]),
        Err(ImmichError::Http { status: 400, .. })
    ));
}

#[test]
fn bulk_requests_are_chunked() {
    let mock = MockImmich::new();
    let album = mock.add_album("Big", &[]);
    let c = client(&mock);
    let o = c.add_to_album(&album, &ids(1200)).unwrap();
    // Unknown ids: each is answered, one request per 500.
    assert_eq!(o.failed.len(), 1200);
    let sizes: Vec<usize> = mock
        .bodies(&format!("/albums/{album}/assets"))
        .iter()
        .map(|b| b["ids"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, vec![500, 500, 200]);
}

#[test]
fn bulk_answers_without_a_result_or_reason() {
    let odd = Fixed::new(200, r#"[{"id":"a","success":false}]"#);
    let c = Client::new(&odd, SERVER, "k").unwrap();
    let o = c.add_to_album("al/1", &["a".into(), "b".into()]).unwrap();
    assert_eq!(
        o.failed,
        vec![
            ("a".to_owned(), "unknown".to_owned()),
            ("b".to_owned(), "no answer".to_owned())
        ]
    );
    let sent = odd.1.lock().unwrap();
    // Ids in paths are percent-encoded.
    assert_eq!(sent[0].url, format!("{SERVER}/api/albums/al%2F1/assets"));
    assert_eq!(sent[0].method, "PUT");
    assert_eq!(
        sent[0].header_value("content-type"),
        Some("application/json")
    );
}
