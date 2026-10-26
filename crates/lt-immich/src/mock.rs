//! An in-process mock of the Immich API endpoints this crate uses: API-key
//! auth, admin-only libraries and scans (assets added "pending" only become
//! searchable after a scan of their library), albums and their members,
//! `POST /search/metadata` with Immich's case-insensitive substring filters and
//! both paging styles, bulk add/remove result arrays. Failure knobs: offline,
//! forced statuses by path prefix, assets the key may not touch. Every request
//! is logged.

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};

use crate::http::{Request, Response, Transport, decode};
use crate::mapping::immich_under;

/// The mock's server URL (what clients are created with).
pub const SERVER: &str = "https://immich.example.org";
/// The API key the mock accepts.
pub const API_KEY: &str = "mock-api-key";
pub const USER_ID: &str = "user-1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockAsset {
    pub id: String,
    pub original_path: String,
    pub file_name: String,
    pub library_id: Option<String>,
    pub trashed: bool,
    /// Scans of its library still needed before searches find it (0: indexed).
    pub scans_needed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockLibrary {
    pub id: String,
    pub name: String,
    pub import_paths: Vec<String>,
    /// Scan requests received.
    pub scans: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockAlbum {
    pub id: String,
    pub name: String,
    pub description: String,
    pub members: Vec<String>,
}

/// The server's state; tests set the knobs through [`MockImmich::state`].
#[allow(clippy::struct_excessive_bools)] // independent test knobs
#[derive(Debug)]
pub struct MockState {
    /// Every request fails without an answer.
    pub offline: bool,
    pub api_key: String,
    /// Libraries and scans answer 403 to non-admins.
    pub is_admin: bool,
    /// Largest search page the server sends.
    pub page_size: usize,
    /// Page with `nextPage` (Immich before v3.2) instead of `nextCursor`.
    pub legacy_paging: bool,
    /// New pending assets need two scans (the first one is still running).
    pub lazy_scan: bool,
    /// `(method, path prefix under /api, status)`: matching requests get that
    /// status; an empty method matches any.
    pub forced: Vec<(String, String, u16)>,
    /// Asset ids bulk add/remove refuse with `no_permission`.
    pub no_permission: HashSet<String>,
    pub libraries: Vec<MockLibrary>,
    pub assets: Vec<MockAsset>,
    pub albums: Vec<MockAlbum>,
    pub requests: Vec<Request>,
    next: u64,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            offline: false,
            api_key: API_KEY.to_owned(),
            is_admin: true,
            page_size: 1000,
            legacy_paging: false,
            lazy_scan: false,
            forced: Vec::new(),
            no_permission: HashSet::new(),
            libraries: Vec::new(),
            assets: Vec::new(),
            albums: Vec::new(),
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

    fn album_mut(&mut self, id: &str) -> Option<&mut MockAlbum> {
        self.albums.iter_mut().find(|a| a.id == id)
    }
}

/// The mock server.
#[derive(Debug, Default)]
pub struct MockImmich {
    state: Mutex<MockState>,
}

fn json_response(status: u16, v: &Value) -> Response {
    Response::new(status, v.to_string())
}

/// Immich's (`NestJS`) error body.
fn error(status: u16, message: &Value) -> Response {
    let name = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    json_response(
        status,
        &json!({ "message": message, "error": name, "statusCode": status }),
    )
}

fn album_json(a: &MockAlbum) -> Value {
    json!({
        "id": a.id, "albumName": a.name, "description": a.description,
        "assetCount": a.members.len(), "ownerId": USER_ID, "shared": false,
    })
}

fn ilike(value: &str, filter: Option<&str>) -> bool {
    filter.is_none_or(|f| value.to_lowercase().contains(&f.to_lowercase()))
}

impl MockImmich {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An external library indexing `import_paths`; returns its id.
    pub fn add_library(&self, name: &str, import_paths: &[&str]) -> String {
        let mut s = self.state();
        let id = s.id("lib");
        s.libraries.push(MockLibrary {
            id: id.clone(),
            name: name.to_owned(),
            import_paths: import_paths.iter().map(|p| (*p).to_owned()).collect(),
            scans: 0,
        });
        id
    }

    fn push_asset(&self, path: &str, scans_needed: u32) -> String {
        let mut s = self.state();
        let id = s.id("asset");
        let library_id = s
            .libraries
            .iter()
            .find(|l| l.import_paths.iter().any(|ip| immich_under(path, ip)))
            .map(|l| l.id.clone());
        s.assets.push(MockAsset {
            id: id.clone(),
            original_path: path.to_owned(),
            file_name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            library_id,
            trashed: false,
            scans_needed,
        });
        id
    }

    /// An indexed asset; returns its id.
    pub fn add_asset(&self, path: &str) -> String {
        self.push_asset(path, 0)
    }

    /// A file in a library's folder that Immich hasn't indexed yet: searches
    /// find it after a scan of its library (two with `lazy_scan`).
    pub fn add_pending_asset(&self, path: &str) -> String {
        let n = if self.state().lazy_scan { 2 } else { 1 };
        self.push_asset(path, n)
    }

    pub fn trash(&self, asset_id: &str) {
        let mut s = self.state();
        if let Some(a) = s.assets.iter_mut().find(|a| a.id == asset_id) {
            a.trashed = true;
        }
    }

    /// An album with these members, as if made in Immich; returns its id.
    pub fn add_album(&self, name: &str, members: &[&str]) -> String {
        let mut s = self.state();
        let id = s.id("album");
        s.albums.push(MockAlbum {
            id: id.clone(),
            name: name.to_owned(),
            description: String::new(),
            members: members.iter().map(|m| (*m).to_owned()).collect(),
        });
        id
    }

    pub fn delete_album(&self, album_id: &str) {
        self.state().albums.retain(|a| a.id != album_id);
    }

    pub fn album_by_name(&self, name: &str) -> Option<MockAlbum> {
        self.state().albums.iter().find(|a| a.name == name).cloned()
    }

    /// Members of an album, sorted (empty if there's no such album).
    pub fn members(&self, album_id: &str) -> Vec<String> {
        let mut m = self
            .state()
            .album_mut(album_id)
            .map(|a| a.members.clone())
            .unwrap_or_default();
        m.sort();
        m
    }

    /// Requests under `/api` matching `prefix` answer `status`.
    pub fn force(&self, method: &str, prefix: &str, status: u16) {
        self.state()
            .forced
            .push((method.to_owned(), prefix.to_owned(), status));
    }

    /// `(method, path under /api)` of every request, in order.
    pub fn log(&self) -> Vec<(String, String)> {
        self.state()
            .requests
            .iter()
            .map(|r| {
                let path = r
                    .url
                    .strip_prefix(SERVER)
                    .and_then(|p| p.strip_prefix("/api"))
                    .unwrap_or(&r.url);
                (r.method.clone(), path.to_owned())
            })
            .collect()
    }

    /// The logged requests that change something on the server (searches are
    /// reads even though they are POSTs).
    pub fn writes(&self) -> Vec<(String, String)> {
        self.log()
            .into_iter()
            .filter(|(m, p)| m != "GET" && !(m == "POST" && p == "/search/metadata"))
            .collect()
    }

    /// JSON bodies of the logged requests to `path`.
    pub fn bodies(&self, path: &str) -> Vec<Value> {
        let s = self.state();
        s.requests
            .iter()
            .filter(|r| r.url == format!("{SERVER}/api{path}"))
            .map(|r| serde_json::from_slice(&r.body).unwrap_or_default())
            .collect()
    }

    fn route(s: &mut MockState, method: &str, segs: &[&str], body: &Value) -> Response {
        let forbidden = || error(403, &json!("Forbidden resource"));
        match (method, segs) {
            ("GET", ["users", "me"]) => json_response(
                200,
                &json!({
                    "id": USER_ID, "name": "Test User", "email": "user@example.org",
                    "isAdmin": s.is_admin,
                }),
            ),
            ("GET", ["libraries"]) | ("POST", ["libraries", _, "scan"]) if !s.is_admin => {
                forbidden()
            }
            ("GET", ["libraries"]) => json_response(
                200,
                &Value::Array(
                    s.libraries
                        .iter()
                        .map(|l| {
                            json!({
                                "id": l.id, "name": l.name, "ownerId": USER_ID,
                                "importPaths": l.import_paths, "exclusionPatterns": [],
                                "assetCount": 0,
                            })
                        })
                        .collect(),
                ),
            ),
            ("POST", ["libraries", id, "scan"]) => {
                let Some(lib) = s.libraries.iter_mut().find(|l| l.id == *id) else {
                    return error(400, &json!("Library not found"));
                };
                lib.scans += 1;
                for a in &mut s.assets {
                    if a.library_id.as_deref() == Some(*id) && a.scans_needed > 0 {
                        a.scans_needed -= 1;
                    }
                }
                Response::new(204, "")
            }
            ("GET", ["albums"]) => json_response(
                200,
                &Value::Array(s.albums.iter().map(album_json).collect()),
            ),
            ("POST", ["albums"]) => {
                let name = body["albumName"].as_str().unwrap_or("").to_owned();
                if name.is_empty() {
                    return error(400, &json!(["albumName should not be empty"]));
                }
                let id = s.id("album");
                let album = MockAlbum {
                    id,
                    name,
                    description: body["description"].as_str().unwrap_or("").to_owned(),
                    members: Vec::new(),
                };
                let v = album_json(&album);
                s.albums.push(album);
                json_response(201, &v)
            }
            ("PUT" | "DELETE", ["albums", id, "assets"]) => Self::bulk(s, method, id, body),
            ("POST", ["search", "metadata"]) => Self::search(s, body),
            _ => error(
                404,
                &json!(format!("Cannot {method} /api/{}", segs.join("/"))),
            ),
        }
    }

    fn bulk(s: &mut MockState, method: &str, album_id: &str, body: &Value) -> Response {
        let Some(ids) = body["ids"].as_array() else {
            return error(400, &json!(["ids must be an array"]));
        };
        let ids: Vec<String> = ids
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let known: HashSet<String> = s.assets.iter().map(|a| a.id.clone()).collect();
        let denied = s.no_permission.clone();
        let Some(album) = s.album_mut(album_id) else {
            return error(400, &json!("Not found or no album.update access"));
        };
        let mut results = Vec::new();
        for id in ids {
            let member = album.members.contains(&id);
            let err = if !known.contains(&id) {
                Some("not_found")
            } else if denied.contains(&id) {
                Some("no_permission")
            } else if method == "PUT" && member {
                Some("duplicate")
            } else if method == "DELETE" && !member {
                Some("not_found")
            } else {
                None
            };
            if let Some(e) = err {
                results.push(json!({ "id": id, "success": false, "error": e }));
                continue;
            }
            if method == "PUT" {
                album.members.push(id.clone());
            } else {
                album.members.retain(|m| *m != id);
            }
            results.push(json!({ "id": id, "success": true }));
        }
        json_response(200, &Value::Array(results))
    }

    fn search(s: &MockState, body: &Value) -> Response {
        let size = body["size"]
            .as_u64()
            .map_or(250, |n| n as usize)
            .clamp(1, s.page_size.max(1));
        let path = body["originalPath"].as_str();
        let name = body["originalFileName"].as_str();
        let with_deleted = body["withDeleted"].as_bool().unwrap_or(false);
        let albums: Vec<&str> = body["albumIds"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let found: Vec<&MockAsset> = s
            .assets
            .iter()
            .filter(|a| a.scans_needed == 0 && (with_deleted || !a.trashed))
            .filter(|a| ilike(&a.original_path, path) && ilike(&a.file_name, name))
            .filter(|a| {
                albums.iter().all(|id| {
                    s.albums
                        .iter()
                        .any(|al| al.id == *id && al.members.contains(&a.id))
                })
            })
            .collect();
        let page = body["page"].as_u64().unwrap_or(1).max(1) as usize;
        let offset = if s.legacy_paging {
            (page - 1) * size
        } else {
            body["cursor"]
                .as_str()
                .and_then(|c| c.strip_prefix('c'))
                .and_then(|n| n.parse().ok())
                .unwrap_or((page - 1) * size)
        };
        let items: Vec<Value> = found
            .iter()
            .skip(offset)
            .take(size)
            .map(|a| {
                json!({
                    "id": a.id, "originalPath": a.original_path,
                    "originalFileName": a.file_name, "isTrashed": a.trashed,
                    "libraryId": a.library_id, "type": "IMAGE", "ownerId": USER_ID,
                })
            })
            .collect();
        let more = offset + size < found.len();
        let mut assets = json!({
            "total": items.len(), "count": items.len(), "items": items,
            "facets": [], "nextPage": Value::Null,
        });
        if more {
            if s.legacy_paging {
                assets["nextPage"] = json!((page + 1).to_string());
            } else {
                assets["nextCursor"] = json!(format!("c{}", offset + size));
                assets["nextPage"] = json!((offset / size + 2).to_string());
            }
        }
        json_response(
            200,
            &json!({
                "albums": { "total": 0, "count": 0, "items": [], "facets": [], "nextPage": null },
                "assets": assets,
            }),
        )
    }
}

impl Transport for MockImmich {
    fn send(&self, request: Request) -> Result<Response, String> {
        let mut s = self.state();
        s.requests.push(request.clone());
        if s.offline {
            return Err("The Internet connection appears to be offline.".into());
        }
        let Some(path) = request
            .url
            .strip_prefix(SERVER)
            .and_then(|p| p.strip_prefix("/api"))
        else {
            return Ok(error(404, &json!("unknown host")));
        };
        let method = request.method.as_str();
        if method == "GET" && path == "/server/ping" {
            return Ok(json_response(200, &json!({ "res": "pong" })));
        }
        if request.header_value("x-api-key") != Some(s.api_key.as_str()) {
            return Ok(error(401, &json!("Invalid API key")));
        }
        if let Some((_, _, status)) = s
            .forced
            .iter()
            .find(|(m, p, _)| (m.is_empty() || m == method) && path.starts_with(p.as_str()))
        {
            return Ok(error(*status, &json!("forced by the test")));
        }
        let decoded: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .map(decode)
            .collect();
        let segs: Vec<&str> = decoded.iter().map(String::as_str).collect();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or_default();
        Ok(Self::route(&mut s, method, &segs, &body))
    }
}
