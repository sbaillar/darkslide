//! Google Photos export destination (`ds-gphotos`): sign-in steps, the app's
//! albums, and uploading exported files.
//!
//! Every request goes through the app's [`GphotosHttp`] (`URLSession`); the app
//! keeps the refresh token in the Keychain and passes it in per call
//! ([`GphotosAccount`]). The PKCE verifier/challenge and the loopback listener
//! are the app's too. Calls run on their own thread; `token` cancels between
//! requests and while waiting to retry.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ds_gphotos::{AlbumChoice, Client, Credentials, GphotosError, Stage, Transport, oauth};

use crate::error::DsError;
use crate::events::{Dispatcher, ProgressEvent, ProgressListener, TaskPhase};
use crate::jobs::{CancelToken, guard, run_job_on_thread};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosHttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<GphotosHeader>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosHttpResponse {
    pub status: u16,
    pub headers: Vec<GphotosHeader>,
    pub body: Vec<u8>,
}

/// Implemented in Swift with `URLSession`. Called on a worker thread (never the
/// main thread), one request at a time per call; blocks until the answer.
#[uniffi::export(with_foreign)]
pub trait GphotosHttp: Send + Sync {
    /// Any HTTP answer is `Ok` (whatever its status); `Err` means no answer
    /// (offline, timeout), which is retried.
    fn send(&self, request: GphotosHttpRequest) -> Result<GphotosHttpResponse, DsError>;
}

struct Bridge(Arc<dyn GphotosHttp>);

fn headers_in(h: Vec<GphotosHeader>) -> Vec<(String, String)> {
    h.into_iter().map(|h| (h.name, h.value)).collect()
}

fn headers_out(h: Vec<(String, String)>) -> Vec<GphotosHeader> {
    h.into_iter()
        .map(|(name, value)| GphotosHeader { name, value })
        .collect()
}

impl Transport for Bridge {
    fn send(&self, r: ds_gphotos::Request) -> Result<ds_gphotos::Response, String> {
        let answer = self
            .0
            .send(GphotosHttpRequest {
                method: r.method,
                url: r.url,
                headers: headers_out(r.headers),
                body: r.body,
            })
            .map_err(|e| e.to_string())?;
        Ok(ds_gphotos::Response {
            status: answer.status,
            headers: headers_in(answer.headers),
            body: answer.body,
        })
    }
}

/// The signed-in account (refresh token from the Keychain).
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosAccount {
    pub client_id: String,
    /// The Desktop client's secret, when it has one.
    pub client_secret: Option<String>,
    pub refresh_token: String,
}

impl std::fmt::Debug for GphotosAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GphotosAccount")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl GphotosAccount {
    fn credentials(&self) -> Credentials {
        Credentials {
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            refresh_token: self.refresh_token.clone(),
        }
    }
}

/// The result of signing in: keep `refresh_token` in the Keychain.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosSignIn {
    pub refresh_token: String,
}

impl std::fmt::Debug for GphotosSignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GphotosSignIn { .. }")
    }
}

/// An album this app created.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosAlbum {
    pub id: String,
    pub title: String,
    pub product_url: String,
    pub item_count: Option<u64>,
}

impl From<ds_gphotos::Album> for GphotosAlbum {
    fn from(a: ds_gphotos::Album) -> Self {
        Self {
            item_count: a.media_items_count.and_then(|c| c.parse().ok()),
            id: a.id,
            title: a.title,
            product_url: a.product_url,
        }
    }
}

/// Where uploads go.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum GphotosAlbumChoice {
    /// An album picked from [`gphotos_albums`].
    Existing { id: String, title: String },
    /// This app's album with this title, created when there is none.
    Named { title: String },
}

/// One exported file and its description (the photo's caption or title).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosFile {
    pub path: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosUploaded {
    pub path: String,
    pub media_item_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosFailed {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GphotosUploadReport {
    pub album: Option<GphotosAlbum>,
    pub uploaded: Vec<GphotosUploaded>,
    pub failed: Vec<GphotosFailed>,
    pub cancelled: bool,
    /// Nothing was uploaded: the album couldn't be found or created (why).
    pub error: Option<String>,
    /// Google refused the account: sign in again in Settings.
    pub sign_in_needed: bool,
}

/// `ds-gphotos` errors as the app sees them; a refused sign-in is
/// `InvalidArgument` with a message starting "Google sign-in needed".
fn ds_error(e: GphotosError) -> DsError {
    match e {
        GphotosError::Cancelled => DsError::Cancelled,
        GphotosError::Unsupported(message) => DsError::Unsupported { message },
        GphotosError::Auth(_) => DsError::InvalidArgument {
            message: e.to_string(),
        },
        e => DsError::Io {
            message: e.to_string(),
        },
    }
}

/// The consent page to open for a loopback `redirect_uri`
/// (`http://127.0.0.1:<port>/`) and a PKCE S256 `code_challenge`.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn gphotos_authorization_url(
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    state: String,
) -> Result<String, DsError> {
    guard(|| {
        if client_id.trim().is_empty() {
            return Err(DsError::invalid("no Google OAuth client ID"));
        }
        Ok(oauth::authorization_url(
            client_id.trim(),
            &redirect_uri,
            &code_challenge,
            &state,
        ))
    })
}

/// The authorization code in the request the browser sent to the loopback
/// listener (its request target, e.g. `/?code=…&state=…`).
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn gphotos_code_from_redirect(target: String, state: String) -> Result<String, DsError> {
    guard(|| oauth::code_from_redirect(&target, &state).map_err(ds_error))
}

/// Exchange the code for a refresh token (keep it in the Keychain).
#[uniffi::export]
#[allow(clippy::too_many_arguments)] // one flat call across the FFI
pub async fn gphotos_exchange_code(
    http: Arc<dyn GphotosHttp>,
    client_id: String,
    client_secret: Option<String>,
    code: String,
    code_verifier: String,
    redirect_uri: String,
    token: Arc<CancelToken>,
) -> Result<GphotosSignIn, DsError> {
    run_job_on_thread("ds-gphotos", token, move |_| {
        let t = oauth::exchange_code(
            &Bridge(http),
            client_id.trim(),
            client_secret.as_deref().map(str::trim),
            &code,
            &code_verifier,
            &redirect_uri,
        )
        .map_err(ds_error)?;
        Ok(GphotosSignIn {
            refresh_token: t.refresh_token.unwrap_or_default(),
        })
    })
    .await
}

/// Revoke the refresh token (sign out). The app forgets it either way.
#[uniffi::export]
pub async fn gphotos_revoke(
    http: Arc<dyn GphotosHttp>,
    refresh_token: String,
    token: Arc<CancelToken>,
) -> Result<(), DsError> {
    run_job_on_thread("ds-gphotos", token, move |_| {
        oauth::revoke(&Bridge(http), &refresh_token).map_err(ds_error)
    })
    .await
}

/// The albums this app created, for the album picker.
#[uniffi::export]
pub async fn gphotos_albums(
    http: Arc<dyn GphotosHttp>,
    account: GphotosAccount,
    token: Arc<CancelToken>,
) -> Result<Vec<GphotosAlbum>, DsError> {
    run_job_on_thread("ds-gphotos", token, move |cancelled| {
        let bridge = Bridge(http);
        let mut c = Client::new(&bridge, account.credentials()).with_cancel(cancelled);
        let albums = c.list_albums().map_err(ds_error)?;
        Ok(albums.into_iter().map(GphotosAlbum::from).collect())
    })
    .await
}

/// Upload `files` (exported JPEG/HEIC) into `album`. Progress per photo goes to
/// `listener`: kind `"gphotos.upload"` while files are sent (`completed` =
/// files finished), `"gphotos.add"` while they are added to the album. Files that
/// fail are listed with why; the rest go on.
#[uniffi::export]
pub async fn gphotos_upload(
    http: Arc<dyn GphotosHttp>,
    account: GphotosAccount,
    files: Vec<GphotosFile>,
    album: GphotosAlbumChoice,
    listener: Option<Arc<dyn ProgressListener>>,
    token: Arc<CancelToken>,
) -> Result<GphotosUploadReport, DsError> {
    let events = Arc::new(Dispatcher::new());
    events.set_listener(listener);
    let ev = events.clone();
    let r = run_job_on_thread("ds-gphotos", token, move |cancelled| {
        let bridge = Bridge(http);
        let mut c = Client::new(&bridge, account.credentials()).with_cancel(cancelled);
        Ok(upload_with(&mut c, &files, album, &ev))
    })
    .await;
    events.flush(Duration::from_secs(5));
    r
}

fn upload_with(
    client: &mut Client<'_>,
    files: &[GphotosFile],
    album: GphotosAlbumChoice,
    events: &Dispatcher,
) -> GphotosUploadReport {
    let jobs: Vec<_> = files
        .iter()
        .map(|f| ds_gphotos::UploadFile {
            path: PathBuf::from(&f.path),
            description: f.description.clone(),
        })
        .collect();
    let choice = match album {
        GphotosAlbumChoice::Existing { id, title } => AlbumChoice::Existing { id, title },
        GphotosAlbumChoice::Named { title } => AlbumChoice::Named(title),
    };
    let total = files.len() as u64;
    let emit = |kind: &str, phase, completed: u64| {
        events.emit(ProgressEvent {
            task_id: 0,
            kind: kind.into(),
            phase,
            completed,
            total,
        });
    };
    let path = |p: PathBuf| p.to_string_lossy().into_owned();
    match ds_gphotos::upload(client, &jobs, &choice, &mut |p| {
        let kind = if p.stage == Stage::Adding {
            "gphotos.add"
        } else {
            "gphotos.upload"
        };
        emit(kind, TaskPhase::Running, p.done as u64);
    }) {
        Ok(r) => {
            let sign_in_needed = r
                .failed
                .iter()
                .any(|(_, why)| why.contains("sign-in needed"));
            let done = (r.uploaded.len() + r.failed.len()) as u64;
            emit(
                "gphotos.upload",
                if r.cancelled {
                    TaskPhase::Cancelled
                } else {
                    TaskPhase::Finished
                },
                done,
            );
            GphotosUploadReport {
                album: r.album.map(GphotosAlbum::from),
                uploaded: r
                    .uploaded
                    .into_iter()
                    .map(|(p, id)| GphotosUploaded {
                        path: path(p),
                        media_item_id: id,
                    })
                    .collect(),
                failed: r
                    .failed
                    .into_iter()
                    .map(|(p, reason)| GphotosFailed {
                        path: path(p),
                        reason,
                    })
                    .collect(),
                cancelled: r.cancelled,
                error: None,
                sign_in_needed,
            }
        }
        Err(e) => {
            emit("gphotos.upload", TaskPhase::Failed, 0);
            GphotosUploadReport {
                album: None,
                uploaded: Vec::new(),
                failed: Vec::new(),
                cancelled: matches!(e, GphotosError::Cancelled),
                sign_in_needed: matches!(e, GphotosError::Auth(_)),
                error: Some(e.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests;
