use std::sync::Mutex;

use ds_gphotos::mock::{GOOD_CODE, MockGoogle, REFRESH_TOKEN};
use futures_executor::block_on;

use super::*;

/// The app's `URLSession` stand-in: the mock Google server behind the FFI types.
struct Http(Arc<MockGoogle>);

impl GphotosHttp for Http {
    fn send(&self, r: GphotosHttpRequest) -> Result<GphotosHttpResponse, DsError> {
        let answer = ds_gphotos::Transport::send(
            self.0.as_ref(),
            ds_gphotos::Request {
                method: r.method,
                url: r.url,
                headers: headers_in(r.headers),
                body: r.body,
            },
        )
        .map_err(|message| DsError::Io { message })?;
        Ok(GphotosHttpResponse {
            status: answer.status,
            headers: headers_out(answer.headers),
            body: answer.body,
        })
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<ProgressEvent>>);

impl ProgressListener for Recorder {
    fn on_progress(&self, event: ProgressEvent) -> Result<(), DsError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

fn setup() -> (Arc<MockGoogle>, Arc<dyn GphotosHttp>) {
    let g = Arc::new(MockGoogle::new());
    (g.clone(), Arc::new(Http(g)))
}

fn account() -> GphotosAccount {
    GphotosAccount {
        client_id: "cid".into(),
        client_secret: None,
        refresh_token: REFRESH_TOKEN.into(),
    }
}

#[test]
fn sign_in_steps() {
    let (g, http) = setup();
    let url = gphotos_authorization_url(
        " cid ".into(),
        "http://127.0.0.1:4/".into(),
        "ch".into(),
        "st".into(),
    )
    .unwrap();
    assert!(url.contains("client_id=cid&") && url.contains("code_challenge=ch"));
    assert!(matches!(
        gphotos_authorization_url(" ".into(), "r".into(), "c".into(), "s".into()),
        Err(DsError::InvalidArgument { .. })
    ));
    let code =
        gphotos_code_from_redirect(format!("/?code={GOOD_CODE}&state=st"), "st".into()).unwrap();
    assert_eq!(code, GOOD_CODE);
    let forged = gphotos_code_from_redirect("/?code=x&state=no".into(), "st".into()).unwrap_err();
    assert!(forged.to_string().contains("sign-in needed"), "{forged}");
    let signed = block_on(gphotos_exchange_code(
        http.clone(),
        "cid".into(),
        Some(" sec ".into()),
        code,
        "verifier".into(),
        "http://127.0.0.1:4/".into(),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(signed.refresh_token, REFRESH_TOKEN);
    assert!(!format!("{signed:?}").contains(REFRESH_TOKEN));
    let body = String::from_utf8(g.requests_to(oauth::TOKEN_ENDPOINT)[0].body.clone()).unwrap();
    assert!(
        body.contains("client_secret=sec&") || body.ends_with("client_secret=sec"),
        "{body}"
    );
    block_on(gphotos_revoke(
        http.clone(),
        REFRESH_TOKEN.into(),
        CancelToken::new(),
    ))
    .unwrap();
    // Revoked: listing albums now needs a new sign-in.
    let e = block_on(gphotos_albums(http, account(), CancelToken::new())).unwrap_err();
    assert!(
        matches!(e, DsError::InvalidArgument { ref message } if message.starts_with("Google sign-in needed"))
    );
}

#[test]
fn albums_are_listed() {
    let (g, http) = setup();
    g.add_album("Iceland");
    let albums = block_on(gphotos_albums(http.clone(), account(), CancelToken::new())).unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].title, "Iceland");
    assert_eq!(albums[0].item_count, Some(0));
    g.state().offline = true;
    let e = block_on(gphotos_albums(http, account(), CancelToken::new())).unwrap_err();
    assert!(matches!(e, DsError::Io { .. }), "{e}");
}

#[test]
fn upload_reports_per_file_with_progress() {
    let (g, http) = setup();
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for n in ["a.jpg", "b.heic", "c.png"] {
        let p = dir.path().join(n);
        std::fs::write(&p, b"0123456789").unwrap();
        files.push(GphotosFile {
            path: p.to_string_lossy().into_owned(),
            description: Some("Caption".into()),
        });
    }
    let rec = Arc::new(Recorder::default());
    let r = block_on(gphotos_upload(
        http,
        account(),
        files,
        GphotosAlbumChoice::Named {
            title: "Trip".into(),
        },
        Some(rec.clone()),
        CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(r.album.as_ref().unwrap().title, "Trip");
    assert_eq!(r.uploaded.len(), 2);
    assert_eq!(r.failed.len(), 1);
    assert!(r.failed[0].path.ends_with("c.png"));
    assert!(!r.sign_in_needed && !r.cancelled && r.error.is_none());
    assert_eq!(g.media()[0].description.as_deref(), Some("Caption"));
    let events = rec.0.lock().unwrap().clone();
    assert!(events.iter().any(|e| e.kind == "gphotos.add"));
    let last = events.last().unwrap();
    assert_eq!(
        (last.phase, last.completed, last.total),
        (TaskPhase::Finished, 3, 3)
    );
}

#[test]
fn upload_without_sign_in_or_album() {
    let (g, http) = setup();
    g.state().refresh_tokens.clear();
    let rec = Arc::new(Recorder::default());
    let r = block_on(gphotos_upload(
        http.clone(),
        account(),
        vec![],
        GphotosAlbumChoice::Existing {
            id: "x".into(),
            title: "X".into(),
        },
        Some(rec.clone()),
        CancelToken::new(),
    ))
    .unwrap();
    // An existing album isn't looked up; with no files nothing is sent.
    assert!(r.error.is_none());
    let r = block_on(gphotos_upload(
        http.clone(),
        account(),
        vec![],
        GphotosAlbumChoice::Named { title: "X".into() },
        Some(rec.clone()),
        CancelToken::new(),
    ))
    .unwrap();
    assert!(r.sign_in_needed);
    assert!(r.error.unwrap().contains("sign-in needed"));
    assert_eq!(
        rec.0.lock().unwrap().last().unwrap().phase,
        TaskPhase::Failed
    );
    // Cancelled before it starts.
    let token = CancelToken::new();
    token.cancel();
    let r = block_on(gphotos_upload(
        http,
        account(),
        vec![],
        GphotosAlbumChoice::Named { title: "X".into() },
        None,
        token,
    ));
    assert!(matches!(r, Err(DsError::Cancelled)) || r.unwrap().cancelled);
}

#[test]
fn sign_in_lost_mid_upload_is_flagged() {
    let (g, http) = setup();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("a.jpg");
    std::fs::write(&p, b"0123").unwrap();
    let id = g.add_album("A");
    g.state().refresh_tokens.clear();
    let r = block_on(gphotos_upload(
        http,
        account(),
        vec![GphotosFile {
            path: p.to_string_lossy().into_owned(),
            description: None,
        }],
        GphotosAlbumChoice::Existing {
            id,
            title: "A".into(),
        },
        None,
        CancelToken::new(),
    ))
    .unwrap();
    assert!(r.sign_in_needed);
    assert_eq!(r.failed.len(), 1);
}

#[test]
fn errors_map_to_ds_errors() {
    assert_eq!(ds_error(GphotosError::Cancelled), DsError::Cancelled);
    assert!(matches!(
        ds_error(GphotosError::Unsupported("x".into())),
        DsError::Unsupported { .. }
    ));
    assert!(matches!(
        ds_error(GphotosError::RateLimited),
        DsError::Io { .. }
    ));
    assert!(!format!("{:?}", account()).contains(REFRESH_TOKEN));
}
