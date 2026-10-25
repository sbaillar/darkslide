//! Blocking calls to the Immich REST API (`<server>/api`), authenticated with
//! an API key. No retries: a sync that fails is simply tried again next time.

use std::collections::HashMap;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::ImmichError;
use crate::http::{Request, Response, Transport, encode};
use crate::mapping::immich_under;

/// Asset ids per bulk add/remove request.
pub const BULK_LIMIT: usize = 500;
/// Assets asked for per search page (Immich's maximum).
pub const SEARCH_PAGE_SIZE: usize = 1000;
/// Search pages read before giving up on a server that never stops paging
/// (a million assets at full pages).
pub const MAX_SEARCH_PAGES: usize = 1000;

/// The user the API key belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub is_admin: bool,
}

/// A library; external libraries index `import_paths` on the server.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub import_paths: Vec<String>,
    #[serde(default)]
    pub exclusion_patterns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub id: String,
    #[serde(rename = "albumName", default)]
    pub name: String,
    #[serde(default)]
    pub asset_count: u64,
}

/// An asset, located by the path Immich indexed it under.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    pub id: String,
    pub original_path: String,
    #[serde(rename = "originalFileName", default)]
    pub file_name: String,
}

/// What a bulk add/remove did to each asset id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BulkOutcome {
    pub done: Vec<String>,
    /// Already as asked: a duplicate on add, not in the album on remove.
    pub unchanged: Vec<String>,
    /// `(asset id, Immich's reason)`.
    pub failed: Vec<(String, String)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FoundAsset {
    #[serde(flatten)]
    asset: Asset,
    #[serde(default)]
    is_trashed: bool,
}

#[derive(Deserialize)]
struct SearchResponse {
    assets: SearchPage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchPage {
    #[serde(default)]
    items: Vec<FoundAsset>,
    #[serde(default)]
    next_page: Option<String>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct BulkItem {
    id: String,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    error: Option<String>,
}

/// `server_url` as the client uses it: trimmed, `http(s)://host[:port][/path]`
/// without a trailing `/` or `/api`.
pub fn normalize_server(server_url: &str) -> Result<String, ImmichError> {
    let invalid = || ImmichError::InvalidServer(server_url.trim().to_owned());
    let mut url = server_url.trim();
    let scheme_len = ["https://", "http://"]
        .iter()
        .find(|s| url.len() >= s.len() && url[..s.len()].eq_ignore_ascii_case(s))
        .map(|s| s.len())
        .ok_or_else(invalid)?;
    url = url.trim_end_matches('/');
    if url.len() > scheme_len + 4 && url.ends_with("/api") {
        url = url[..url.len() - 4].trim_end_matches('/');
    }
    let host = url
        .get(scheme_len..)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("");
    if host.is_empty() || host.starts_with(':') || url.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    Ok(url.to_owned())
}

static NEVER: fn() -> bool = || false;

/// Immich API client for one sync run.
pub struct Client<'a> {
    transport: &'a dyn Transport,
    server: String,
    api_key: String,
    cancelled: &'a dyn Fn() -> bool,
    max_pages: usize,
}

impl std::fmt::Debug for Client<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("server", &self.server)
            .field("max_pages", &self.max_pages)
            .finish_non_exhaustive()
    }
}

impl<'a> Client<'a> {
    pub fn new(
        transport: &'a dyn Transport,
        server_url: &str,
        api_key: &str,
    ) -> Result<Self, ImmichError> {
        let server = normalize_server(server_url)?;
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(ImmichError::Auth("no API key was given".into()));
        }
        Ok(Self {
            transport,
            server,
            api_key: api_key.to_owned(),
            cancelled: &NEVER,
            max_pages: MAX_SEARCH_PAGES,
        })
    }

    /// Called before every request: once it answers `true`, calls fail with
    /// `Cancelled`.
    #[must_use]
    pub fn with_cancel(mut self, cancelled: &'a dyn Fn() -> bool) -> Self {
        self.cancelled = cancelled;
        self
    }

    /// Search pages read before a search fails with `Protocol`.
    #[must_use]
    pub fn with_max_pages(mut self, pages: usize) -> Self {
        self.max_pages = pages.max(1);
        self
    }

    /// The normalized server URL (what sync bookkeeping is keyed by).
    pub fn server(&self) -> &str {
        &self.server
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Response, ImmichError> {
        if (self.cancelled)() {
            return Err(ImmichError::Cancelled);
        }
        let mut req = Request::new(method, format!("{}/api{path}", self.server))
            .header("x-api-key", self.api_key.clone())
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = self.transport.send(req).map_err(ImmichError::Network)?;
        if resp.is_success() {
            return Ok(resp);
        }
        let message = resp.error_message();
        Err(match resp.status {
            401 => ImmichError::Auth(message),
            403 => ImmichError::Forbidden(message),
            404 => ImmichError::NotFound(message),
            status => ImmichError::Http { status, message },
        })
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ImmichError> {
        parse(&self.call("GET", path, None)?)
    }

    /// `GET /server/ping`: the server answers and is an Immich server.
    pub fn ping(&self) -> Result<(), ImmichError> {
        let v: Value = self.get("/server/ping")?;
        if v["res"] == "pong" {
            Ok(())
        } else {
            Err(ImmichError::Protocol(format!("ping answered {v}")))
        }
    }

    /// The user the API key belongs to.
    pub fn me(&self) -> Result<User, ImmichError> {
        self.get("/users/me")
    }

    /// Every library (admin keys only: `Forbidden` otherwise).
    pub fn libraries(&self) -> Result<Vec<Library>, ImmichError> {
        self.get("/libraries")
    }

    /// Ask Immich to rescan a library (admin keys only). The scan runs in the
    /// background; new files show up in searches once it is done.
    pub fn scan_library(&self, library_id: &str) -> Result<(), ImmichError> {
        self.call(
            "POST",
            &format!("/libraries/{}/scan", encode(library_id)),
            None,
        )
        .map(drop)
    }

    pub fn albums(&self) -> Result<Vec<Album>, ImmichError> {
        self.get("/albums")
    }

    pub fn create_album(&self, name: &str, description: &str) -> Result<Album, ImmichError> {
        parse(&self.call(
            "POST",
            "/albums",
            Some(&json!({ "albumName": name, "description": description })),
        )?)
    }

    /// Ids of every asset in the album.
    pub fn album_asset_ids(&self, album_id: &str) -> Result<Vec<String>, ImmichError> {
        Ok(self
            .search(json!({ "albumIds": [album_id] }), true)?
            .into_iter()
            .map(|a| a.asset.id)
            .collect())
    }

    /// Assets (not trashed) whose path is `root` or below it, component-wise
    /// (Immich's own filter is a case-insensitive substring match).
    pub fn assets_under(&self, root: &str) -> Result<Vec<Asset>, ImmichError> {
        let root = root.trim_end_matches('/');
        Ok(self
            .search(json!({ "originalPath": root }), true)?
            .into_iter()
            .filter(|a| !a.is_trashed && immich_under(&a.asset.original_path, root))
            .map(|a| a.asset)
            .collect())
    }

    /// Assets (not trashed) named exactly `name`, from the first search page.
    pub fn find_by_file_name(&self, name: &str) -> Result<Vec<Asset>, ImmichError> {
        Ok(self
            .search(json!({ "originalFileName": name }), false)?
            .into_iter()
            .filter(|a| !a.is_trashed && a.asset.file_name == name)
            .map(|a| a.asset)
            .collect())
    }

    /// `POST /search/metadata` with `filter`, following `nextCursor` (v3.2+)
    /// or `nextPage` (older servers) when `all`.
    fn search(&self, filter: Value, all: bool) -> Result<Vec<FoundAsset>, ImmichError> {
        let mut body = filter;
        body["size"] = json!(SEARCH_PAGE_SIZE);
        let mut out = Vec::new();
        for _ in 0..self.max_pages {
            let page: SearchResponse =
                parse(&self.call("POST", "/search/metadata", Some(&body))?)?;
            out.extend(page.assets.items);
            if !all {
                return Ok(out);
            }
            if let Some(cursor) = page.assets.next_cursor {
                body["cursor"] = json!(cursor);
            } else if let Some(next) = page.assets.next_page {
                let n: u64 = next
                    .parse()
                    .map_err(|_| ImmichError::Protocol(format!("next page {next:?}")))?;
                body["page"] = json!(n);
            } else {
                return Ok(out);
            }
        }
        Err(ImmichError::Protocol(format!(
            "search still had more results after {} pages",
            self.max_pages
        )))
    }

    /// Add assets to an album, [`BULK_LIMIT`] per request. A request that
    /// fails ends the call (earlier requests stay applied).
    pub fn add_to_album(&self, album_id: &str, ids: &[String]) -> Result<BulkOutcome, ImmichError> {
        self.bulk("PUT", album_id, ids, "duplicate")
    }

    /// Remove assets from an album (never from Immich), [`BULK_LIMIT`] per
    /// request.
    pub fn remove_from_album(
        &self,
        album_id: &str,
        ids: &[String],
    ) -> Result<BulkOutcome, ImmichError> {
        self.bulk("DELETE", album_id, ids, "not_found")
    }

    fn bulk(
        &self,
        method: &str,
        album_id: &str,
        ids: &[String],
        unchanged: &str,
    ) -> Result<BulkOutcome, ImmichError> {
        let path = format!("/albums/{}/assets", encode(album_id));
        let mut out = BulkOutcome::default();
        for chunk in ids.chunks(BULK_LIMIT) {
            let items: Vec<BulkItem> =
                parse(&self.call(method, &path, Some(&json!({ "ids": chunk })))?)?;
            let by_id: HashMap<&str, &BulkItem> =
                items.iter().map(|i| (i.id.as_str(), i)).collect();
            for id in chunk {
                match by_id.get(id.as_str()) {
                    Some(i) if i.success => out.done.push(id.clone()),
                    Some(i) if i.error.as_deref() == Some(unchanged) => {
                        out.unchanged.push(id.clone());
                    }
                    Some(i) => out.failed.push((
                        id.clone(),
                        i.error.clone().unwrap_or_else(|| "unknown".into()),
                    )),
                    None => out.failed.push((id.clone(), "no answer".into())),
                }
            }
        }
        Ok(out)
    }
}

fn parse<T: DeserializeOwned>(r: &Response) -> Result<T, ImmichError> {
    serde_json::from_slice(&r.body).map_err(|e| ImmichError::Protocol(e.to_string()))
}

#[cfg(test)]
pub(crate) mod tests;
