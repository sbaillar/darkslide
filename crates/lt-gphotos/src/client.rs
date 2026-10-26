//! Authorized Library API calls: refresh, backoff, resumable uploads, albums,
//! `batchCreate`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::http::{Request, Response, Transport, encode};
use crate::oauth::{self, http_error};
use crate::{BATCH_LIMIT, GphotosError, MAX_ALBUM_TITLE_CHARS, MAX_PHOTO_BYTES, mime_type};

pub const API: &str = "https://photoslibrary.googleapis.com/v1";
/// Upload chunk size (rounded down to the server's granularity).
pub const DEFAULT_CHUNK: usize = 8 * 1024 * 1024;
/// Granularity assumed when the server doesn't say (Google's is 256 KiB).
const DEFAULT_GRANULARITY: u64 = 256 * 1024;

/// The signed-in account, as the app keeps it (the refresh token comes from the
/// Keychain for each run).
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub client_id: String,
    /// A Desktop client's secret (not confidential for installed apps), if any.
    pub client_secret: Option<String>,
    pub refresh_token: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

/// Waits between retries. A seam so tests don't sleep.
pub trait Sleeper: Send + Sync {
    /// Wait `d`, returning early once `cancelled()`.
    fn sleep(&self, d: Duration, cancelled: &dyn Fn() -> bool);
}

/// Sleeps on the calling thread, checking for cancel every 100 ms.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, d: Duration, cancelled: &dyn Fn() -> bool) {
        let step = Duration::from_millis(100);
        let mut left = d;
        while !left.is_zero() && !cancelled() {
            let s = left.min(step);
            std::thread::sleep(s);
            left -= s;
        }
    }
}

/// How often and how long to retry 429, 5xx and unanswered requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Tries per request, the first included.
    pub attempts: u32,
    /// First wait; doubled each retry.
    pub base: Duration,
    pub max: Duration,
    /// Least wait after a 429 (Google asks for at least 30 s).
    pub rate_limit_min: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 6,
            base: Duration::from_secs(1),
            max: Duration::from_secs(120),
            rate_limit_min: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Wait before retry number `retry` (0-based).
    pub fn delay(&self, retry: u32, rate_limited: bool, retry_after: Option<Duration>) -> Duration {
        let exp = self
            .base
            .saturating_mul(1u32 << retry.min(16))
            .min(self.max);
        let d = if rate_limited {
            exp.max(self.rate_limit_min)
        } else {
            exp
        };
        retry_after.map_or(d, |r| d.max(r.min(self.max.max(self.rate_limit_min))))
    }
}

/// An album this app created.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub product_url: String,
    /// Google sends it as a string.
    #[serde(default)]
    pub media_items_count: Option<String>,
}

/// One uploaded file to add to the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewItem {
    pub upload_token: String,
    pub file_name: String,
    pub description: Option<String>,
}

static NEVER: fn() -> bool = || false;

/// Library API client for one job. Not shared between threads: Google asks for
/// `batchCreate` calls of one user to run one at a time.
pub struct Client<'a> {
    transport: &'a dyn Transport,
    creds: Credentials,
    access: Option<String>,
    retry: RetryPolicy,
    sleeper: &'a dyn Sleeper,
    cancelled: &'a dyn Fn() -> bool,
    chunk_size: usize,
}

impl std::fmt::Debug for Client<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("creds", &self.creds)
            .field("retry", &self.retry)
            .field("chunk_size", &self.chunk_size)
            .finish_non_exhaustive()
    }
}

impl<'a> Client<'a> {
    pub fn new(transport: &'a dyn Transport, creds: Credentials) -> Self {
        static SLEEPER: ThreadSleeper = ThreadSleeper;
        Self {
            transport,
            creds,
            access: None,
            retry: RetryPolicy::default(),
            sleeper: &SLEEPER,
            cancelled: &NEVER,
            chunk_size: DEFAULT_CHUNK,
        }
    }

    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    #[must_use]
    pub fn with_sleeper(mut self, sleeper: &'a dyn Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// Checked before every request and while waiting to retry.
    #[must_use]
    pub fn with_cancel(mut self, cancelled: &'a dyn Fn() -> bool) -> Self {
        self.cancelled = cancelled;
        self
    }

    #[must_use]
    pub fn with_chunk_size(mut self, bytes: usize) -> Self {
        self.chunk_size = bytes.max(1);
        self
    }

    /// An access token: the current one, else refreshed.
    fn access_token(&mut self) -> Result<String, GphotosError> {
        if let Some(t) = &self.access {
            return Ok(t.clone());
        }
        let t = oauth::refresh(
            self.transport,
            &self.creds.client_id,
            self.creds.client_secret.as_deref(),
            &self.creds.refresh_token,
        )?;
        self.access = Some(t.access_token.clone());
        Ok(t.access_token)
    }

    fn wait(&self, retry: u32, rate_limited: bool, retry_after: Option<Duration>) {
        self.sleeper.sleep(
            self.retry.delay(retry, rate_limited, retry_after),
            self.cancelled,
        );
    }

    /// Send an authorized request (`build` gets the access token). A 401 refreshes
    /// the token once and resends. With `retry`, a 429, 5xx or unanswered request is
    /// retried with backoff; without, it comes back as an error (`RateLimited`,
    /// `Http`, `Network`). Any other answer is returned as is.
    fn send(
        &mut self,
        build: &dyn Fn(&str) -> Request,
        retry: bool,
    ) -> Result<Response, GphotosError> {
        let mut refreshed = false;
        let mut tries = 0;
        loop {
            if (self.cancelled)() {
                return Err(GphotosError::Cancelled);
            }
            let answer = self.access_token().and_then(|tok| {
                self.transport
                    .send(build(&tok))
                    .map_err(GphotosError::Network)
            });
            let (err, rate_limited, after) = match answer {
                Ok(r) if r.status == 401 && !refreshed => {
                    self.access = None;
                    refreshed = true;
                    continue;
                }
                Ok(r) if r.status == 401 => return Err(GphotosError::Auth(r.error_message())),
                Ok(r) if r.status == 429 => (GphotosError::RateLimited, true, retry_after(&r)),
                Ok(r) if r.status >= 500 => (http_error(&r), false, None),
                Ok(r) => return Ok(r),
                Err(e @ GphotosError::Network(_)) => (e, false, None),
                Err(e) => return Err(e),
            };
            tries += 1;
            if !retry || tries >= self.retry.attempts {
                return Err(err);
            }
            self.wait(tries - 1, rate_limited, after);
        }
    }

    /// Upload one JPEG/HEIC with the resumable protocol, in chunks. A chunk that
    /// fails (429, 5xx, no answer) is retried with backoff from the offset the
    /// server reports; a session that ended is started again. Returns the upload
    /// token (valid for a day).
    pub fn upload_file(&mut self, path: &Path) -> Result<String, GphotosError> {
        let io = |e| GphotosError::Io {
            path: path.to_path_buf(),
            source: e,
        };
        let Some(mime) = mime_type(path) else {
            return Err(GphotosError::Unsupported(
                "only JPEG and HEIC exports go to Google Photos".into(),
            ));
        };
        let mut file = File::open(path).map_err(io)?;
        let size = file.metadata().map_err(io)?.len();
        if size == 0 {
            return Err(GphotosError::Unsupported("the file is empty".into()));
        }
        if size > MAX_PHOTO_BYTES {
            return Err(GphotosError::Unsupported(
                "Google Photos takes photos up to 200 MB".into(),
            ));
        }
        let mut failures = 0;
        'session: loop {
            let (session, granularity) = self.start_session(mime, size)?;
            let chunk = (self.chunk_size as u64 / granularity).max(1) * granularity;
            let mut offset = 0u64;
            loop {
                let len = chunk.min(size - offset);
                let last = offset + len == size;
                let mut buf = vec![0u8; len as usize];
                file.seek(SeekFrom::Start(offset)).map_err(io)?;
                file.read_exact(&mut buf).map_err(io)?;
                let command = if last { "upload, finalize" } else { "upload" };
                let answer = self.send(
                    &|tok| {
                        Request::new("POST", session.clone())
                            .header("Authorization", format!("Bearer {tok}"))
                            .header("X-Goog-Upload-Command", command)
                            .header("X-Goog-Upload-Offset", offset.to_string())
                            .body(buf.clone())
                    },
                    false,
                );
                let rate_limited = match answer {
                    Ok(r) if r.is_success() => {
                        if !last {
                            offset += len;
                            continue;
                        }
                        let token = r.text().trim().to_owned();
                        if token.is_empty() {
                            return Err(GphotosError::Protocol("empty upload token".into()));
                        }
                        return Ok(token);
                    }
                    Ok(r) => return Err(http_error(&r)),
                    Err(GphotosError::RateLimited) => true,
                    Err(GphotosError::Network(_) | GphotosError::Http { .. }) => false,
                    Err(e) => return Err(e),
                };
                failures += 1;
                if failures >= self.retry.attempts {
                    return Err(if rate_limited {
                        GphotosError::RateLimited
                    } else {
                        GphotosError::Network("the upload kept failing".into())
                    });
                }
                self.wait(failures - 1, rate_limited, None);
                let received = self.resume_offset(&session)?;
                match received {
                    Some(r) if r <= size => offset = r,
                    // Ended (or an answer we can't use): start a new session.
                    _ => continue 'session,
                }
            }
        }
    }

    /// Start a resumable upload: the session URL and the chunk granularity.
    fn start_session(&mut self, mime: &str, size: u64) -> Result<(String, u64), GphotosError> {
        let start = self.send(
            &|tok| {
                Request::new("POST", format!("{API}/uploads"))
                    .header("Authorization", format!("Bearer {tok}"))
                    .header("X-Goog-Upload-Command", "start")
                    .header("X-Goog-Upload-Protocol", "resumable")
                    .header("X-Goog-Upload-Content-Type", mime)
                    .header("X-Goog-Upload-Raw-Size", size.to_string())
            },
            true,
        )?;
        if !start.is_success() {
            return Err(http_error(&start));
        }
        let Some(session) = start.header_value("X-Goog-Upload-URL").map(str::to_owned) else {
            return Err(GphotosError::Protocol("no upload URL".into()));
        };
        // Every chunk carries the access token: send it to Google only.
        if !is_google_url(&session) {
            return Err(GphotosError::Protocol(
                "an upload URL outside Google".into(),
            ));
        }
        let granularity = start
            .header_value("X-Goog-Upload-Chunk-Granularity")
            .and_then(|g| g.trim().parse::<u64>().ok())
            .filter(|&g| g > 0)
            .unwrap_or(DEFAULT_GRANULARITY);
        Ok((session, granularity))
    }

    /// Where an active upload session goes on (bytes the server has), or None
    /// when the session ended (or answered something unusable).
    fn resume_offset(&mut self, session: &str) -> Result<Option<u64>, GphotosError> {
        let q = self.send(
            &|tok| {
                Request::new("POST", session.to_owned())
                    .header("Authorization", format!("Bearer {tok}"))
                    .header("X-Goog-Upload-Command", "query")
            },
            true,
        )?;
        let active = q.is_success() && q.header_value("X-Goog-Upload-Status") == Some("active");
        Ok(q.header_value("X-Goog-Upload-Size-Received")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|_| active))
    }

    /// Albums this app created (all pages).
    pub fn list_albums(&mut self) -> Result<Vec<Album>, GphotosError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Page {
            #[serde(default)]
            albums: Vec<Album>,
            #[serde(default)]
            next_page_token: Option<String>,
        }
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..200 {
            let url = match &page_token {
                Some(t) => format!("{API}/albums?pageSize=50&pageToken={}", encode(t)),
                None => format!("{API}/albums?pageSize=50"),
            };
            let r = self.send(
                &|tok| {
                    Request::new("GET", url.clone())
                        .header("Authorization", format!("Bearer {tok}"))
                },
                true,
            )?;
            if !r.is_success() {
                return Err(http_error(&r));
            }
            let page: Page = serde_json::from_slice(&r.body)
                .map_err(|e| GphotosError::Protocol(format!("albums: {e}")))?;
            out.extend(page.albums);
            match page.next_page_token.filter(|t| !t.is_empty()) {
                Some(t) => page_token = Some(t),
                None => return Ok(out),
            }
        }
        Err(GphotosError::Protocol("too many album pages".into()))
    }

    /// Create an album (owned by this app, so it can be added to).
    pub fn create_album(&mut self, title: &str) -> Result<Album, GphotosError> {
        let title = album_title(title)?;
        let body = json!({ "album": { "title": title } });
        let r = self.send(
            &|tok| {
                Request::new("POST", format!("{API}/albums"))
                    .header("Authorization", format!("Bearer {tok}"))
                    .json(&body)
            },
            true,
        )?;
        if !r.is_success() {
            return Err(http_error(&r));
        }
        serde_json::from_slice(&r.body).map_err(|e| GphotosError::Protocol(format!("album: {e}")))
    }

    /// This app's album called `title` (the first, if several), else a new one.
    pub fn find_or_create_album(&mut self, title: &str) -> Result<Album, GphotosError> {
        let wanted = album_title(title)?;
        if let Some(a) = self.list_albums()?.into_iter().find(|a| a.title == wanted) {
            return Ok(a);
        }
        self.create_album(&wanted)
    }

    /// Add up to [`BATCH_LIMIT`] uploaded files to the library (and to `album_id`).
    /// One result per item, in order: the new media item's id, or why it failed
    /// (a whole-call failure fails every item).
    pub fn batch_create(
        &mut self,
        album_id: Option<&str>,
        items: &[NewItem],
    ) -> Result<Vec<Result<String, String>>, GphotosError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Status {
            #[serde(default)]
            code: i32,
            #[serde(default)]
            message: String,
        }
        #[derive(Deserialize)]
        struct Item {
            id: String,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ItemResult {
            #[serde(default)]
            upload_token: Option<String>,
            #[serde(default)]
            status: Option<Status>,
            #[serde(default)]
            media_item: Option<Item>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Answer {
            #[serde(default)]
            new_media_item_results: Vec<ItemResult>,
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if items.len() > BATCH_LIMIT {
            return Err(GphotosError::Unsupported(format!(
                "at most {BATCH_LIMIT} items per batch"
            )));
        }
        let new_items: Vec<_> = items
            .iter()
            .map(|i| {
                let mut v = json!({
                    "simpleMediaItem": { "fileName": i.file_name, "uploadToken": i.upload_token }
                });
                if let Some(d) = &i.description {
                    v["description"] = json!(d);
                }
                v
            })
            .collect();
        let mut body = json!({ "newMediaItems": new_items });
        if let Some(a) = album_id {
            body["albumId"] = json!(a);
        }
        let r = self.send(
            &|tok| {
                Request::new("POST", format!("{API}/mediaItems:batchCreate"))
                    .header("Authorization", format!("Bearer {tok}"))
                    .json(&body)
            },
            true,
        )?;
        if !r.is_success() {
            let why = http_error(&r).to_string();
            return Ok(items.iter().map(|_| Err(why.clone())).collect());
        }
        let answer: Answer = serde_json::from_slice(&r.body)
            .map_err(|e| GphotosError::Protocol(format!("batchCreate: {e}")))?;
        let results = answer.new_media_item_results;
        Ok(items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let res = results
                    .iter()
                    .find(|r| r.upload_token.as_deref() == Some(item.upload_token.as_str()))
                    .or_else(|| results.get(i).filter(|r| r.upload_token.is_none()));
                match res {
                    None => Err("Google Photos didn't report this file".to_owned()),
                    Some(r) => match (&r.status, &r.media_item) {
                        (Some(s), _) if s.code != 0 => Err(if s.message.is_empty() {
                            format!("error {}", s.code)
                        } else {
                            s.message.clone()
                        }),
                        (_, Some(m)) => Ok(m.id.clone()),
                        (_, None) => Err("no media item created".to_owned()),
                    },
                }
            })
            .collect())
    }
}

/// `Retry-After` in seconds, if given.
fn retry_after(r: &Response) -> Option<Duration> {
    r.header_value("Retry-After")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn album_title(title: &str) -> Result<String, GphotosError> {
    let t = title.trim();
    if t.is_empty() {
        return Err(GphotosError::Unsupported("the album needs a name".into()));
    }
    Ok(t.chars().take(MAX_ALBUM_TITLE_CHARS).collect())
}

#[cfg(test)]
pub(crate) mod tests;

/// `https://` on `googleapis.com` or a subdomain, with no user info or port.
fn is_google_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.contains(['@', ':']) && (host == "googleapis.com" || host.ends_with(".googleapis.com"))
}
