//! Immich album sync: Luxtable archives sessions to a NAS folder that a
//! self-hosted Immich server indexes as a read-only *external library*; this
//! crate keeps Immich albums in step with Luxtable's rules ("all 5-star
//! photos" → album "Luxtable ★★★★★").
//!
//! Luxtable's only writes to Immich are: creating an album, adding and
//! removing assets in albums, and asking for a library scan. It never uploads
//! or deletes assets and never edits their metadata.
//!
//! Self-contained behind one narrow seam: every request goes through
//! [`Transport`] (`URLSession` in the app, the [`mock`] Immich server in tests).
//!
//! - [`Client`]: blocking, API-key authenticated calls (no retries: the next
//!   sync simply tries again).
//! - [`mapping`]: the archive folder as this Mac sees it ↔ the path Immich
//!   indexes it under, and detecting that pairing.
//! - [`sync`]: the bookkeeping of which album members Luxtable added, the pure
//!   diff, and the orchestration of one sync run.

use std::path::PathBuf;

pub mod client;
pub mod http;
pub mod mapping;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod publish;
pub mod sync;

pub use client::{Album, Asset, BULK_LIMIT, BulkOutcome, Client, Library, User, normalize_server};
pub use http::{Request, Response, Transport};
pub use mapping::{Detected, Detection, PathMapping, detect, guess_from_import_paths};
pub use sync::{
    AlbumRecord, AlbumReport, Plan, Source, SyncOptions, SyncReport, SyncState, Wanted, plan, sync,
};

#[derive(Debug, thiserror::Error)]
pub enum ImmichError {
    /// The server address isn't a usable `http(s)://host` URL.
    #[error("not an Immich server address: {0}")]
    InvalidServer(String),
    /// 401, or no key at all: the API key is wrong or was revoked.
    #[error("Immich refused the API key: {0}")]
    Auth(String),
    /// 403: the key's user may not do this (e.g. libraries need an admin).
    #[error("Immich did not allow this: {0}")]
    Forbidden(String),
    /// 404.
    #[error("Immich could not find it: {0}")]
    NotFound(String),
    /// Any other answer outside 2xx.
    #[error("Immich answered {status}: {message}")]
    Http { status: u16, message: String },
    /// No answer at all: offline, DNS, timeout.
    #[error("Immich server not reachable: {0}")]
    Network(String),
    /// An answer this client doesn't understand.
    #[error("unexpected answer from Immich: {0}")]
    Protocol(String),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cancelled")]
    Cancelled,
}

impl ImmichError {
    /// The server could not be reached (try again later, nothing is wrong
    /// with the settings).
    pub fn is_offline(&self) -> bool {
        matches!(self, Self::Network(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_read_as_plain_words() {
        assert!(ImmichError::Network("timed out".into()).is_offline());
        assert!(!ImmichError::Auth("x".into()).is_offline());
        assert_eq!(
            ImmichError::Auth("Invalid API key".into()).to_string(),
            "Immich refused the API key: Invalid API key"
        );
        assert_eq!(
            ImmichError::Network("timed out".into()).to_string(),
            "Immich server not reachable: timed out"
        );
        assert_eq!(
            ImmichError::Http {
                status: 500,
                message: "boom".into()
            }
            .to_string(),
            "Immich answered 500: boom"
        );
        let io = ImmichError::Io {
            path: PathBuf::from("/x/state.json"),
            source: std::io::Error::other("disk full"),
        };
        assert_eq!(io.to_string(), "/x/state.json: disk full");
    }
}
