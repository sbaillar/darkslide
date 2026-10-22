//! Google Photos export destination: exported JPEG/HEIC files are uploaded into the
//! user's library, into an album this app created (the Library API only lets an app
//! add to its own albums since March 2025).
//!
//! Self-contained behind one narrow seam so it can move into a plugin later:
//! every request goes through [`Transport`] (`URLSession` in the app, the
//! [`mock`] Google server in tests), and the caller supplies [`Credentials`]
//! (the refresh token lives in the macOS Keychain; it is never written here).
//!
//! - [`oauth`]: installed-app OAuth 2.0 with PKCE and a loopback redirect (what
//!   Google allows for Desktop clients): the authorization URL, the redirect's
//!   code, code exchange, refresh, revoke.
//! - [`Client`]: authorized calls with one refresh on 401, exponential backoff on
//!   429/5xx/network errors (at least 30 s after a 429, as Google asks), resumable
//!   chunked uploads that resume where the server says, albums, `batchCreate`.
//! - [`upload`]: the job: files → album (reused by title or created) in groups of
//!   [`BATCH_LIMIT`], with per-file progress and per-file failures.

use std::path::PathBuf;

pub mod client;
pub mod http;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod oauth;
pub mod upload;

pub use client::{Album, Client, Credentials, NewItem, RetryPolicy, Sleeper, ThreadSleeper};
pub use http::{Request, Response, Transport};
pub use oauth::{SCOPES, Tokens};
pub use upload::{AlbumChoice, Progress, Stage, UploadFile, UploadReport, upload};

/// `mediaItems:batchCreate` takes at most this many items per call.
pub const BATCH_LIMIT: usize = 50;
/// Largest photo Google Photos accepts.
pub const MAX_PHOTO_BYTES: u64 = 200 * 1024 * 1024;
/// Longest media item description.
pub const MAX_DESCRIPTION_CHARS: usize = 1000;
/// Longest album title.
pub const MAX_ALBUM_TITLE_CHARS: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum GphotosError {
    /// Google refused the sign-in or the refresh token (revoked, expired, wrong
    /// client): the user has to sign in again.
    #[error("Google sign-in needed: {0}")]
    Auth(String),
    /// A request Google rejected (not worth retrying).
    #[error("Google Photos answered {status}: {message}")]
    Http { status: u16, message: String },
    /// Still rate limited (429) after every retry.
    #[error("Google Photos is rate limiting uploads; try again later")]
    RateLimited,
    /// No connection, or still failing after every retry.
    #[error("network: {0}")]
    Network(String),
    /// An answer this client doesn't understand.
    #[error("unexpected answer from Google: {0}")]
    Protocol(String),
    /// A file Google Photos won't take (format, size), or bad input.
    #[error("{0}")]
    Unsupported(String),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cancelled")]
    Cancelled,
}

impl GphotosError {
    /// Stops the whole job (every later call would fail the same way).
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Auth(_) | Self::Cancelled | Self::RateLimited)
    }
}

/// The MIME type Google Photos is told for `path` (JPEG or HEIC exports only).
pub fn mime_type(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "heic" | "heif" => Some("image/heic"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn mime_types_of_exports() {
        assert_eq!(mime_type(Path::new("a/B.JPG")), Some("image/jpeg"));
        assert_eq!(mime_type(Path::new("b.jpeg")), Some("image/jpeg"));
        assert_eq!(mime_type(Path::new("c.heic")), Some("image/heic"));
        assert_eq!(mime_type(Path::new("c.HEIF")), Some("image/heic"));
        assert_eq!(mime_type(Path::new("d.tif")), None);
        assert_eq!(mime_type(Path::new("noext")), None);
    }

    #[test]
    fn fatal_errors_stop_the_job() {
        assert!(GphotosError::Auth("x".into()).is_fatal());
        assert!(GphotosError::Cancelled.is_fatal());
        assert!(GphotosError::RateLimited.is_fatal());
        assert!(!GphotosError::Network("x".into()).is_fatal());
        assert!(
            !GphotosError::Http {
                status: 400,
                message: "x".into()
            }
            .is_fatal()
        );
        assert_eq!(
            GphotosError::Http {
                status: 400,
                message: "bad".into()
            }
            .to_string(),
            "Google Photos answered 400: bad"
        );
    }
}
