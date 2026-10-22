use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::mock::{MockGoogle, REFRESH_TOKEN, SESSION_PREFIX};

#[derive(Default)]
pub(crate) struct RecordingSleeper(pub Mutex<Vec<Duration>>);

impl Sleeper for RecordingSleeper {
    fn sleep(&self, d: Duration, _cancelled: &dyn Fn() -> bool) {
        self.0.lock().unwrap().push(d);
    }
}

impl RecordingSleeper {
    pub(crate) fn waits(&self) -> Vec<Duration> {
        self.0.lock().unwrap().clone()
    }
}

pub(crate) fn creds() -> Credentials {
    Credentials {
        client_id: "cid.apps.googleusercontent.com".into(),
        client_secret: Some("secret".into()),
        refresh_token: REFRESH_TOKEN.into(),
    }
}

pub(crate) fn jpeg(dir: &Path, name: &str, len: usize) -> std::path::PathBuf {
    let p = dir.join(name);
    let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn chunked_upload_reassembles_the_file() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    let dir = tempfile::tempdir().unwrap();
    let p = jpeg(dir.path(), "a.jpg", 1000);
    let mut c = Client::new(&g, creds())
        .with_sleeper(&s)
        .with_chunk_size(130);
    let token = c.upload_file(&p).unwrap();
    // 130 rounds down to the mock's 4-byte granularity: 128-byte chunks.
    let chunks = g.requests_to(SESSION_PREFIX);
    assert_eq!(chunks.len(), 8);
    assert!(chunks.iter().all(|r| r.body.len() <= 128));
    assert_eq!(
        chunks.last().unwrap().header_value("X-Goog-Upload-Command"),
        Some("upload, finalize")
    );
    assert_eq!(chunks[1].header_value("X-Goog-Upload-Offset"), Some("128"));
    assert_eq!(g.state().uploads[&token], std::fs::read(&p).unwrap());
    let start = &g.requests_to(&format!("{API}/uploads"))[0];
    assert_eq!(
        start.header_value("X-Goog-Upload-Content-Type"),
        Some("image/jpeg")
    );
    assert_eq!(start.header_value("X-Goog-Upload-Raw-Size"), Some("1000"));
    assert_eq!(s.waits().len(), 0);
}

#[test]
fn rate_limited_requests_back_off_at_least_30s() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    g.state().rate_limit = 2;
    let mut c = Client::new(&g, creds()).with_sleeper(&s);
    let a = c.create_album("Trip").unwrap();
    assert_eq!(a.title, "Trip");
    assert_eq!(
        s.waits(),
        vec![Duration::from_secs(30), Duration::from_secs(30)]
    );
    // Retry-After longer than the backoff wins.
    g.state().rate_limit = 1;
    g.state().retry_after = Some("45".into());
    c.list_albums().unwrap();
    assert_eq!(s.waits()[2], Duration::from_secs(45));
}

#[test]
fn persistent_rate_limit_gives_up() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    g.state().rate_limit = 100;
    let mut c = Client::new(&g, creds()).with_sleeper(&s);
    assert!(matches!(c.list_albums(), Err(GphotosError::RateLimited)));
    assert_eq!(s.waits().len(), 5, "6 tries");
}

#[test]
fn server_errors_and_dropped_requests_retry_with_exponential_backoff() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    g.state().server_errors = 2;
    g.state().drop_requests = 1;
    let mut c = Client::new(&g, creds()).with_sleeper(&s);
    c.list_albums().unwrap();
    assert_eq!(
        s.waits(),
        vec![
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4)
        ]
    );
    g.state().server_errors = 100;
    assert!(matches!(
        c.list_albums(),
        Err(GphotosError::Http { status: 503, .. })
    ));
}

#[test]
fn expired_access_token_is_refreshed_once() {
    let g = MockGoogle::new();
    let mut c = Client::new(&g, creds());
    c.list_albums().unwrap();
    let refreshes = || g.requests_to(crate::oauth::TOKEN_ENDPOINT).len();
    assert_eq!(refreshes(), 1);
    c.list_albums().unwrap();
    assert_eq!(refreshes(), 1, "the access token is reused");
    g.expire_access_tokens();
    c.list_albums().unwrap();
    assert_eq!(refreshes(), 2);
    // The new token works; the 401 was answered once.
    let albums = g.requests_to(&format!("{API}/albums"));
    assert_eq!(albums.len(), 4);
}

#[test]
fn revoked_refresh_token_needs_sign_in() {
    let g = MockGoogle::new();
    g.state().refresh_tokens.clear();
    let mut c = Client::new(&g, creds());
    let e = c.list_albums().unwrap_err();
    assert!(matches!(e, GphotosError::Auth(_)), "{e}");
    assert!(e.is_fatal());
}

#[test]
fn a_401_after_refresh_is_an_auth_error() {
    // A token endpoint that hands out tokens the API never accepts.
    struct Rejecting;
    impl Transport for Rejecting {
        fn send(&self, r: Request) -> Result<Response, String> {
            if r.url == crate::oauth::TOKEN_ENDPOINT {
                Ok(Response::new(
                    200,
                    r#"{"access_token":"a","expires_in":10}"#,
                ))
            } else {
                Ok(Response::new(401, r#"{"error":{"message":"nope"}}"#))
            }
        }
    }
    let mut c = Client::new(&Rejecting, creds());
    assert!(matches!(c.list_albums(), Err(GphotosError::Auth(m)) if m == "nope"));
}

#[test]
fn cut_chunk_resumes_at_the_reported_offset() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    g.state().cut_chunks = 2;
    let dir = tempfile::tempdir().unwrap();
    let p = jpeg(dir.path(), "b.heic", 600);
    let mut c = Client::new(&g, creds())
        .with_sleeper(&s)
        .with_chunk_size(256);
    let token = c.upload_file(&p).unwrap();
    assert_eq!(g.state().uploads[&token], std::fs::read(&p).unwrap());
    let queries = g
        .requests_to(SESSION_PREFIX)
        .iter()
        .filter(|r| r.header_value("X-Goog-Upload-Command") == Some("query"))
        .count();
    assert_eq!(queries, 2);
    // Resumed mid-chunk (half of the first 256 bytes arrived).
    assert!(
        g.requests_to(SESSION_PREFIX)
            .iter()
            .any(|r| r.header_value("X-Goog-Upload-Offset") == Some("128"))
    );
    assert_eq!(s.waits().len(), 2);
    assert_eq!(
        g.requests_to(&format!("{API}/uploads"))[0].header_value("X-Goog-Upload-Content-Type"),
        Some("image/heic")
    );
}

#[test]
fn ended_session_starts_over() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    g.state().cut_chunks = 1;
    g.state().end_sessions = true;
    let dir = tempfile::tempdir().unwrap();
    let p = jpeg(dir.path(), "c.jpg", 100);
    let mut c = Client::new(&g, creds()).with_sleeper(&s);
    let token = c.upload_file(&p).unwrap();
    assert_eq!(g.state().uploads[&token].len(), 100);
    assert_eq!(g.requests_to(&format!("{API}/uploads")).len(), 2);
}

#[test]
fn chunks_that_keep_failing_give_up() {
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    g.state().cut_chunks = 100;
    let dir = tempfile::tempdir().unwrap();
    let p = jpeg(dir.path(), "d.jpg", 100);
    let mut c = Client::new(&g, creds()).with_sleeper(&s);
    assert!(matches!(c.upload_file(&p), Err(GphotosError::Network(_))));
    assert_eq!(s.waits().len(), 5);
}

#[test]
fn chunk_rate_limit_and_dropped_chunk_resume() {
    struct Always<'a>(&'a MockGoogle);
    impl Transport for Always<'_> {
        fn send(&self, r: Request) -> Result<Response, String> {
            if r.url.starts_with(SESSION_PREFIX)
                && r.header_value("X-Goog-Upload-Command") != Some("query")
            {
                return Ok(Response::new(429, ""));
            }
            self.0.send(r)
        }
    }
    struct Flaky<'a>(&'a MockGoogle, Mutex<u32>);
    impl Transport for Flaky<'_> {
        fn send(&self, r: Request) -> Result<Response, String> {
            if r.url.starts_with(SESSION_PREFIX)
                && r.header_value("X-Goog-Upload-Command") != Some("query")
            {
                let mut n = self.1.lock().unwrap();
                *n += 1;
                match *n {
                    1 => return Ok(Response::new(429, "")),
                    3 => return Err("lost".into()),
                    _ => {}
                }
            }
            self.0.send(r)
        }
    }
    let g = MockGoogle::new();
    let s = RecordingSleeper::default();
    let dir = tempfile::tempdir().unwrap();
    let p = jpeg(dir.path(), "e.jpg", 300);
    // Start goes through, then the first chunk is rate limited and the second dropped.
    let f = Flaky(&g, Mutex::new(0));
    let mut c = Client::new(&f, creds())
        .with_sleeper(&s)
        .with_chunk_size(100);
    let token = c.upload_file(&p).unwrap();
    assert_eq!(g.state().uploads[&token].len(), 300);
    assert_eq!(s.waits()[0], Duration::from_secs(30));
    // Always rate limited: gives up as rate limited.
    g.state().rate_limit = 0;
    let a = Always(&g);
    let mut c = Client::new(&a, creds()).with_sleeper(&s);
    assert!(matches!(c.upload_file(&p), Err(GphotosError::RateLimited)));
}

#[test]
fn upload_refusals() {
    let g = MockGoogle::new();
    let dir = tempfile::tempdir().unwrap();
    let mut c = Client::new(&g, creds());
    let tif = jpeg(dir.path(), "x.tif", 10);
    assert!(matches!(
        c.upload_file(&tif),
        Err(GphotosError::Unsupported(_))
    ));
    let empty = jpeg(dir.path(), "x.jpg", 0);
    assert!(matches!(
        c.upload_file(&empty),
        Err(GphotosError::Unsupported(_))
    ));
    let missing = dir.path().join("gone.jpg");
    assert!(matches!(
        c.upload_file(&missing),
        Err(GphotosError::Io { .. })
    ));
    let big = dir.path().join("big.jpg");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(MAX_PHOTO_BYTES + 1)
        .unwrap();
    let e = c.upload_file(&big).unwrap_err();
    assert!(e.to_string().contains("200 MB"), "{e}");
    assert_eq!(g.requests_to(SESSION_PREFIX).len(), 0);
}

#[test]
fn upload_protocol_errors() {
    struct Fixed(fn(&Request) -> Response);
    impl Transport for Fixed {
        fn send(&self, r: Request) -> Result<Response, String> {
            if r.url == crate::oauth::TOKEN_ENDPOINT {
                return Ok(Response::new(200, r#"{"access_token":"a"}"#));
            }
            Ok((self.0)(&r))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let p = jpeg(dir.path(), "a.jpg", 10);
    // Each answers the token endpoint, then `api` for everything else.
    let no_url = Fixed(|_| Response::new(200, ""));
    assert!(matches!(
        Client::new(&no_url, creds()).upload_file(&p),
        Err(GphotosError::Protocol(_))
    ));
    // The access token only ever goes to Google: a session URL elsewhere is refused
    // before any chunk is sent.
    for elsewhere in [
        "https://evil.example/upload",
        "http://photoslibrary.googleapis.com/s",
        "https://googleapis.com.evil.example/s",
        "https://user@photoslibrary.googleapis.com/s",
        "https://photoslibrary.googleapis.com:444/s",
    ] {
        struct Elsewhere(&'static str);
        impl Transport for Elsewhere {
            fn send(&self, r: Request) -> Result<Response, String> {
                if r.url == crate::oauth::TOKEN_ENDPOINT {
                    return Ok(Response::new(200, r#"{"access_token":"a"}"#));
                }
                assert!(r.url.ends_with("/uploads"), "a chunk went to {}", r.url);
                Ok(Response::new(200, "").header("X-Goog-Upload-URL", self.0))
            }
        }
        let t = Elsewhere(elsewhere);
        assert!(
            matches!(
                Client::new(&t, creds()).upload_file(&p),
                Err(GphotosError::Protocol(m)) if m.contains("upload URL")
            ),
            "{elsewhere}"
        );
    }
    let refused = Fixed(|_| Response::new(403, r#"{"error":{"message":"quota"}}"#));
    assert!(matches!(
        Client::new(&refused, creds()).upload_file(&p),
        Err(GphotosError::Http { status: 403, .. })
    ));
    let empty_token = Fixed(|r| {
        Response::new(200, "").header(
            "X-Goog-Upload-URL",
            if r.url.contains("uploads") {
                "https://photoslibrary.googleapis.com/s"
            } else {
                ""
            },
        )
    });
    assert!(matches!(
        Client::new(&empty_token, creds()).upload_file(&p),
        Err(GphotosError::Protocol(m)) if m.contains("token")
    ));
    let chunk_refused = Fixed(|r| {
        if r.url.ends_with("/uploads") {
            Response::new(200, "").header(
                "X-Goog-Upload-URL",
                "https://photoslibrary.googleapis.com/s",
            )
        } else {
            Response::new(400, "bad offset")
        }
    });
    assert!(matches!(
        Client::new(&chunk_refused, creds()).upload_file(&p),
        Err(GphotosError::Http { status: 400, .. })
    ));
}

#[test]
fn cancel_stops_before_the_next_request() {
    let g = MockGoogle::new();
    let flag = AtomicBool::new(true);
    let cancelled = || flag.load(Ordering::Relaxed);
    let mut c = Client::new(&g, creds()).with_cancel(&cancelled);
    assert!(matches!(c.list_albums(), Err(GphotosError::Cancelled)));
    assert_eq!(g.state().requests.len(), 0);
}

#[test]
fn thread_sleeper_returns_early_when_cancelled() {
    let t = std::time::Instant::now();
    ThreadSleeper.sleep(Duration::from_secs(30), &|| true);
    ThreadSleeper.sleep(Duration::from_millis(30), &|| false);
    assert!(t.elapsed() < Duration::from_secs(2));
    assert!(t.elapsed() >= Duration::from_millis(30));
}

#[test]
fn backoff_doubles_up_to_the_cap() {
    let p = RetryPolicy::default();
    assert_eq!(p.delay(0, false, None), Duration::from_secs(1));
    assert_eq!(p.delay(3, false, None), Duration::from_secs(8));
    assert_eq!(p.delay(20, false, None), Duration::from_secs(120));
    assert_eq!(p.delay(0, true, None), Duration::from_secs(30));
    assert_eq!(p.delay(6, true, None), Duration::from_secs(64));
    assert_eq!(
        p.delay(0, true, Some(Duration::from_secs(9999))),
        Duration::from_secs(120)
    );
}

#[test]
fn albums_paginate_find_and_create() {
    let g = MockGoogle::new();
    g.state().album_page_size = 2;
    for t in ["A", "B", "C", "Trip"] {
        g.add_album(t);
    }
    let mut c = Client::new(&g, creds());
    let all = c.list_albums().unwrap();
    assert_eq!(
        all.iter().map(|a| a.title.as_str()).collect::<Vec<_>>(),
        ["A", "B", "C", "Trip"]
    );
    assert_eq!(g.requests_to(&format!("{API}/albums")).len(), 2);
    let trip = c.find_or_create_album("  Trip ").unwrap();
    assert_eq!(trip.id, all[3].id, "reused by title");
    let new = c.find_or_create_album("New").unwrap();
    assert_eq!(new.title, "New");
    assert_eq!(g.state().albums.len(), 5);
    assert!(matches!(
        c.create_album("   "),
        Err(GphotosError::Unsupported(_))
    ));
    let long = "x".repeat(600);
    assert_eq!(
        c.create_album(&long).unwrap().title.len(),
        MAX_ALBUM_TITLE_CHARS
    );
}

#[test]
fn album_errors() {
    struct Fixed(u16, &'static str);
    impl Transport for Fixed {
        fn send(&self, r: Request) -> Result<Response, String> {
            if r.url == crate::oauth::TOKEN_ENDPOINT {
                return Ok(Response::new(200, r#"{"access_token":"a"}"#));
            }
            Ok(Response::new(self.0, self.1))
        }
    }
    assert!(matches!(
        Client::new(&Fixed(403, "no"), creds()).list_albums(),
        Err(GphotosError::Http { status: 403, .. })
    ));
    assert!(matches!(
        Client::new(&Fixed(200, "nope"), creds()).list_albums(),
        Err(GphotosError::Protocol(_))
    ));
    assert!(matches!(
        Client::new(&Fixed(403, "no"), creds()).create_album("x"),
        Err(GphotosError::Http { status: 403, .. })
    ));
    assert!(matches!(
        Client::new(&Fixed(200, "{}"), creds()).create_album("x"),
        Err(GphotosError::Protocol(_))
    ));
    // A server that always has another page.
    assert!(matches!(
        Client::new(&Fixed(200, r#"{"nextPageToken":"again"}"#), creds()).list_albums(),
        Err(GphotosError::Protocol(m)) if m.contains("pages")
    ));
}

#[test]
fn batch_create_reports_each_item() {
    let g = MockGoogle::new();
    let album = g.add_album("Trip");
    let dir = tempfile::tempdir().unwrap();
    let mut c = Client::new(&g, creds());
    let mut items = Vec::new();
    for n in ["ok.jpg", "bad.jpg", "gone.jpg"] {
        let token = c.upload_file(&jpeg(dir.path(), n, 20)).unwrap();
        items.push(NewItem {
            upload_token: token,
            file_name: n.into(),
            description: (n == "ok.jpg").then(|| "Sunset".to_owned()),
        });
    }
    g.state().fail_file_names.insert("bad.jpg".into());
    g.state().drop_results.insert("gone.jpg".into());
    let r = c.batch_create(Some(&album), &items).unwrap();
    assert!(r[0].is_ok());
    assert!(r[1].as_ref().unwrap_err().starts_with("Failed"));
    assert!(r[2].as_ref().unwrap_err().contains("didn't report"));
    let media = g.media();
    assert_eq!(media.len(), 1);
    assert_eq!(media[0].description.as_deref(), Some("Sunset"));
    assert_eq!(media[0].album_id.as_deref(), Some(album.as_str()));
    let body: serde_json::Value = serde_json::from_slice(
        &g.requests_to(&format!("{API}/mediaItems"))
            .pop()
            .unwrap()
            .body,
    )
    .unwrap();
    assert!(body["newMediaItems"][1].get("description").is_none());
    assert_eq!(
        c.batch_create(None, &[]).unwrap(),
        Vec::<Result<String, String>>::new()
    );
    let too_many = vec![items[0].clone(); BATCH_LIMIT + 1];
    assert!(matches!(
        c.batch_create(None, &too_many),
        Err(GphotosError::Unsupported(_))
    ));
}

#[test]
fn batch_create_whole_call_failures_and_odd_answers() {
    struct Fixed(&'static str);
    impl Transport for Fixed {
        fn send(&self, r: Request) -> Result<Response, String> {
            if r.url == crate::oauth::TOKEN_ENDPOINT {
                return Ok(Response::new(200, r#"{"access_token":"a"}"#));
            }
            Ok(Response::new(200, self.0))
        }
    }
    let g = MockGoogle::new();
    let item = NewItem {
        upload_token: "t".into(),
        file_name: "a.jpg".into(),
        description: None,
    };
    let mut c = Client::new(&g, creds());
    // An album this app didn't create (or that was deleted): every item fails.
    let r = c
        .batch_create(Some("not-ours"), &[item.clone(), item.clone()])
        .unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[0].as_ref().unwrap_err().contains("Invalid album"));

    assert!(matches!(
        Client::new(&Fixed("nope"), creds()).batch_create(None, std::slice::from_ref(&item)),
        Err(GphotosError::Protocol(_))
    ));
    // Results without upload tokens are matched by position; odd statuses.
    let odd = Fixed(
        r#"{"newMediaItemResults":[{"status":{"code":13}},{"status":{"message":"OK"}},{"mediaItem":{"id":"m3"}}]}"#,
    );
    let r = Client::new(&odd, creds())
        .batch_create(None, &[item.clone(), item.clone(), item])
        .unwrap();
    assert_eq!(r[0], Err("error 13".into()));
    assert_eq!(r[1], Err("no media item created".into()));
    assert_eq!(r[2], Ok("m3".into()));
}

#[test]
fn credentials_debug_hides_tokens() {
    let d = format!("{:?}", creds());
    assert!(!d.contains("secret") && !d.contains(REFRESH_TOKEN), "{d}");
    let g = MockGoogle::new();
    let c = Client::new(&g, creds());
    assert!(!format!("{c:?}").contains(REFRESH_TOKEN));
}
