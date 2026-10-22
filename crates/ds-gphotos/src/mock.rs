//! An in-process mock of Google's token endpoint and the Photos Library API
//! (resumable uploads, albums, `batchCreate`), with failure injection: offline,
//! 429s, 5xx, dropped requests, chunks cut short, expired access tokens, per-item
//! failures. Every request is logged.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use crate::client::API;
use crate::http::{Request, Response, Transport, parse_pairs};
use crate::oauth::{REVOKE_ENDPOINT, SCOPES, TOKEN_ENDPOINT};

pub const SESSION_PREFIX: &str = "https://photoslibrary.googleapis.com/mock-upload/session/";
/// The code the mock's consent page "returns".
pub const GOOD_CODE: &str = "the-code";
/// The refresh token the mock issues and accepts.
pub const REFRESH_TOKEN: &str = "refresh-1";

#[derive(Debug, Default)]
pub struct Session {
    pub size: u64,
    pub received: Vec<u8>,
    pub finalized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItem {
    pub id: String,
    pub file_name: String,
    pub description: Option<String>,
    pub album_id: Option<String>,
    pub bytes: Vec<u8>,
}

/// The server's state; tests set the failure knobs through [`MockGoogle::state`].
#[derive(Debug)]
pub struct MockState {
    /// Every request fails without an answer.
    pub offline: bool,
    pub granted_scope: String,
    pub omit_refresh_token: bool,
    /// The token endpoint answers this status (200: an unparseable body).
    pub token_status: Option<u16>,
    pub revoke_status: Option<u16>,
    pub batch_status: Option<u16>,
    /// The next N API requests get no answer / a 429 / a 503.
    pub drop_requests: u32,
    pub rate_limit: u32,
    pub server_errors: u32,
    /// Sent with each 429.
    pub retry_after: Option<String>,
    /// The next N chunk uploads keep half their bytes, then answer 503.
    pub cut_chunks: u32,
    /// Queries of an upload session report it ended.
    pub end_sessions: bool,
    pub granularity: u64,
    pub album_page_size: usize,
    /// `batchCreate` fails items with these file names.
    pub fail_file_names: HashSet<String>,
    /// `batchCreate` answers with no result for these file names.
    pub drop_results: HashSet<String>,
    pub refresh_tokens: HashSet<String>,
    pub access_tokens: HashSet<String>,
    pub sessions: HashMap<String, Session>,
    pub uploads: HashMap<String, Vec<u8>>,
    pub albums: Vec<(String, String)>,
    pub media: Vec<MediaItem>,
    pub requests: Vec<Request>,
    next: u64,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            offline: false,
            granted_scope: SCOPES.join(" "),
            omit_refresh_token: false,
            token_status: None,
            revoke_status: None,
            batch_status: None,
            drop_requests: 0,
            rate_limit: 0,
            server_errors: 0,
            retry_after: None,
            cut_chunks: 0,
            end_sessions: false,
            granularity: 4,
            album_page_size: 50,
            fail_file_names: HashSet::new(),
            drop_results: HashSet::new(),
            refresh_tokens: HashSet::from([REFRESH_TOKEN.to_owned()]),
            access_tokens: HashSet::new(),
            sessions: HashMap::new(),
            uploads: HashMap::new(),
            albums: Vec::new(),
            media: Vec::new(),
            requests: Vec::new(),
            next: 0,
        }
    }
}

impl MockState {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}-{}", self.next)
    }
}

/// The mock server.
#[derive(Debug, Default)]
pub struct MockGoogle {
    state: Mutex<MockState>,
}

fn json_response(status: u16, v: &Value) -> Response {
    Response::new(status, v.to_string())
}

fn error(status: u16, message: &str) -> Response {
    json_response(
        status,
        &json!({ "error": { "code": status, "message": message } }),
    )
}

fn oauth_error(code: &str) -> Response {
    json_response(400, &json!({ "error": code, "error_description": "mock" }))
}

impl MockGoogle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Access tokens issued so far stop working (they "expired").
    pub fn expire_access_tokens(&self) {
        self.state().access_tokens.clear();
    }

    /// Requests whose URL starts with `prefix`.
    pub fn requests_to(&self, prefix: &str) -> Vec<Request> {
        self.state()
            .requests
            .iter()
            .filter(|r| r.url.starts_with(prefix))
            .cloned()
            .collect()
    }

    pub fn media(&self) -> Vec<MediaItem> {
        self.state().media.clone()
    }

    /// Add an album as if this app had created it earlier; returns its id.
    pub fn add_album(&self, title: &str) -> String {
        let mut s = self.state();
        let id = s.id("album");
        s.albums.push((id.clone(), title.to_owned()));
        id
    }

    fn token(s: &mut MockState, req: &Request) -> Response {
        if let Some(st) = s.token_status {
            return if st == 200 {
                Response::new(200, "not json")
            } else {
                error(st, "token endpoint down")
            };
        }
        let form = parse_pairs(&String::from_utf8_lossy(&req.body));
        let get = |k: &str| {
            form.iter()
                .find(|(n, _)| n == k)
                .map_or("", |(_, v)| v.as_str())
                .to_owned()
        };
        let access = s.id("access");
        match get("grant_type").as_str() {
            "authorization_code"
                if get("code") == GOOD_CODE && !get("code_verifier").is_empty() =>
            {
                s.access_tokens.insert(access.clone());
                let mut v = json!({
                    "access_token": access, "expires_in": 3599,
                    "scope": s.granted_scope, "token_type": "Bearer"
                });
                if !s.omit_refresh_token {
                    v["refresh_token"] = json!(REFRESH_TOKEN);
                }
                json_response(200, &v)
            }
            "refresh_token" if s.refresh_tokens.contains(&get("refresh_token")) => {
                s.access_tokens.insert(access.clone());
                json_response(
                    200,
                    &json!({ "access_token": access, "expires_in": 3599, "token_type": "Bearer" }),
                )
            }
            _ => oauth_error("invalid_grant"),
        }
    }

    fn revoke(s: &mut MockState, req: &Request) -> Response {
        if let Some(st) = s.revoke_status {
            return error(st, "revoke down");
        }
        let form = parse_pairs(&String::from_utf8_lossy(&req.body));
        let token = form
            .iter()
            .find(|(n, _)| n == "token")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if s.refresh_tokens.remove(&token) || s.access_tokens.remove(&token) {
            Response::new(200, "")
        } else {
            oauth_error("invalid_token")
        }
    }

    fn api(s: &mut MockState, req: &Request) -> Result<Response, String> {
        let bearer = req
            .header_value("Authorization")
            .and_then(|a| a.strip_prefix("Bearer "))
            .unwrap_or("");
        if !s.access_tokens.contains(bearer) {
            return Ok(error(
                401,
                "Request had invalid authentication credentials.",
            ));
        }
        if s.drop_requests > 0 {
            s.drop_requests -= 1;
            return Err("The network connection was lost.".into());
        }
        if s.rate_limit > 0 {
            s.rate_limit -= 1;
            let mut r = error(429, "Quota exceeded");
            if let Some(a) = &s.retry_after {
                r = r.header("Retry-After", a.clone());
            }
            return Ok(r);
        }
        if s.server_errors > 0 {
            s.server_errors -= 1;
            return Ok(error(503, "Backend unavailable"));
        }
        if let Some(id) = req.url.strip_prefix(SESSION_PREFIX) {
            return Ok(Self::session(s, id, req));
        }
        let path = req.url.strip_prefix(API).unwrap_or("");
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        Ok(match (req.method.as_str(), path) {
            ("POST", "/uploads") => Self::start(s, req),
            ("GET", "/albums") => Self::list(s, query),
            ("POST", "/albums") => {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
                let title = body["album"]["title"].as_str().unwrap_or("").to_owned();
                let id = s.id("album");
                s.albums.push((id.clone(), title.clone()));
                json_response(
                    200,
                    &json!({ "id": id, "title": title, "productUrl": "https://photos/x" }),
                )
            }
            ("POST", "/mediaItems:batchCreate") => Self::batch(s, req),
            _ => error(404, "not found"),
        })
    }

    fn start(s: &mut MockState, req: &Request) -> Response {
        if req.header_value("X-Goog-Upload-Protocol") != Some("resumable")
            || req.header_value("X-Goog-Upload-Command") != Some("start")
        {
            return error(400, "bad start");
        }
        let Some(size) = req
            .header_value("X-Goog-Upload-Raw-Size")
            .and_then(|v| v.parse().ok())
        else {
            return error(400, "no size");
        };
        let id = s.id("s");
        s.sessions.insert(
            id.clone(),
            Session {
                size,
                ..Session::default()
            },
        );
        Response::new(200, "")
            .header("X-Goog-Upload-URL", format!("{SESSION_PREFIX}{id}"))
            .header("X-Goog-Upload-Chunk-Granularity", s.granularity.to_string())
    }

    fn session(s: &mut MockState, id: &str, req: &Request) -> Response {
        let command = req.header_value("X-Goog-Upload-Command").unwrap_or("");
        let end = s.end_sessions;
        let cut = if command.starts_with("upload") && s.cut_chunks > 0 {
            s.cut_chunks -= 1;
            true
        } else {
            false
        };
        let Some(sess) = s.sessions.get_mut(id) else {
            return error(404, "no such session");
        };
        if command == "query" {
            let status = if end || sess.finalized {
                "final"
            } else {
                "active"
            };
            return Response::new(200, "")
                .header("X-Goog-Upload-Status", status)
                .header(
                    "X-Goog-Upload-Size-Received",
                    sess.received.len().to_string(),
                );
        }
        let offset: usize = req
            .header_value("X-Goog-Upload-Offset")
            .and_then(|o| o.parse().ok())
            .unwrap_or(usize::MAX);
        if sess.finalized || offset != sess.received.len() {
            return error(400, "bad offset");
        }
        if cut {
            sess.received
                .extend_from_slice(&req.body[..req.body.len() / 2]);
            return error(503, "connection reset");
        }
        sess.received.extend_from_slice(&req.body);
        if command != "upload, finalize" {
            return Response::new(200, "");
        }
        if sess.received.len() as u64 != sess.size {
            return error(400, "size mismatch");
        }
        sess.finalized = true;
        let bytes = sess.received.clone();
        let token = s.id("upload-token");
        s.uploads.insert(token.clone(), bytes);
        Response::new(200, token)
    }

    fn list(s: &MockState, query: &str) -> Response {
        let q = parse_pairs(query);
        let start: usize = q
            .iter()
            .find(|(k, _)| k == "pageToken")
            .and_then(|(_, v)| v.strip_prefix("p").and_then(|n| n.parse().ok()))
            .unwrap_or(0);
        let page: Vec<Value> = s
            .albums
            .iter()
            .skip(start)
            .take(s.album_page_size)
            .map(|(id, t)| json!({ "id": id, "title": t, "mediaItemsCount": "0" }))
            .collect();
        let mut v = json!({ "albums": page });
        if start + s.album_page_size < s.albums.len() {
            v["nextPageToken"] = json!(format!("p{}", start + s.album_page_size));
        }
        json_response(200, &v)
    }

    fn batch(s: &mut MockState, req: &Request) -> Response {
        if let Some(st) = s.batch_status {
            return error(st, "batchCreate refused");
        }
        let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let items = body["newMediaItems"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if items.len() > 50 {
            return error(400, "Request must have at most 50 items");
        }
        let album = body["albumId"].as_str().map(str::to_owned);
        if let Some(a) = &album
            && !s.albums.iter().any(|(id, _)| id == a)
        {
            return error(400, "Invalid album");
        }
        let mut results = Vec::new();
        let mut all_ok = true;
        for it in items {
            let token = it["simpleMediaItem"]["uploadToken"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let file_name = it["simpleMediaItem"]["fileName"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            if s.drop_results.contains(&file_name) {
                all_ok = false;
                continue;
            }
            let bytes = s.uploads.remove(&token);
            if bytes.is_none() || s.fail_file_names.contains(&file_name) {
                all_ok = false;
                results.push(json!({
                    "uploadToken": token,
                    "status": { "code": 3, "message": "Failed: There was an error while trying to create this media item." }
                }));
                continue;
            }
            let id = s.id("media");
            s.media.push(MediaItem {
                id: id.clone(),
                file_name: file_name.clone(),
                description: it["description"].as_str().map(str::to_owned),
                album_id: album.clone(),
                bytes: bytes.unwrap_or_default(),
            });
            results.push(json!({
                "uploadToken": token,
                "status": { "message": "Success" },
                "mediaItem": { "id": id, "filename": file_name }
            }));
        }
        json_response(
            if all_ok { 200 } else { 207 },
            &json!({ "newMediaItemResults": results }),
        )
    }
}

impl Transport for MockGoogle {
    fn send(&self, request: Request) -> Result<Response, String> {
        let mut s = self.state();
        s.requests.push(request.clone());
        if s.offline {
            return Err("The Internet connection appears to be offline.".into());
        }
        if request.url == TOKEN_ENDPOINT {
            return Ok(Self::token(&mut s, &request));
        }
        if request.url == REVOKE_ENDPOINT {
            return Ok(Self::revoke(&mut s, &request));
        }
        if request.url.starts_with(API) || request.url.starts_with(SESSION_PREFIX) {
            return Self::api(&mut s, &request);
        }
        Ok(error(404, "unknown host"))
    }
}
