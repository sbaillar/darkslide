//! The Mac side of the Darkslide link (Mac ↔ iPad over the LAN or a tailnet):
//! a small server the app runs while it is open, the client library the iPad
//! app's protocol mirrors, and the `ds-remote` CLI that stands in for the iPad
//! in tests. The protocol is described in `docs/REMOTE.md`.
//!
//! - [`identity`]: each device's Ed25519 key and self-signed certificate; the
//!   key's BLAKE3 [`Fingerprint`] is what gets pinned.
//! - [`tls`]: TLS 1.3 only, mutual authentication, both sides checking the
//!   other's key against their pins (no CA, no passwords).
//! - [`net`]: the peer address filter (loopback, private LAN, tailnet).
//! - [`pairing`]: the one-time pairing code and QR payload (SPAKE2 over the
//!   code, bound to both TLS keys), and the paired-device list.
//! - [`wire`]: versioned, length-prefixed JSON messages with binary bodies.
//! - [`transfer`]: chunked, resumable receive into `.partial` files, verified
//!   with `ds-import`'s BLAKE3 re-read before an atomic no-replace rename.
//! - [`merge`]: sidecar merge rules (fast-forward, field merge, newer wins with
//!   the other edit kept as a variant).
//! - [`server`] / [`client`]: the two ends; [`cli`]: the test client.
//!
//! Safety properties:
//! - Nothing is ever written outside a served session's root, and pushed
//!   originals are placed without replacing anything.
//! - A device is read-only until write access is granted; a revoked device is
//!   refused on its next request.
//! - The server binds only when asked to ([`server::Server::start`]); the app
//!   starts it from Settings and stops it when it quits.

use std::io;
use std::path::PathBuf;

pub mod cli;
pub mod client;
pub mod identity;
pub mod merge;
pub mod net;
pub mod pairing;
pub mod server;
pub mod tls;
pub mod transfer;
pub mod wire;

pub use client::{Client, PullOutcome, PushOutcome, SidecarPushed};
pub use identity::{Fingerprint, Identity};
pub use merge::{Edit, MergeOutcome};
pub use pairing::{Device, DeviceStore, KnownServer, PairingTicket, ServerStore};
pub use server::{Media, RawMedia, ReadFault, Server, ServerConfig, ServerStatus, SessionRoot};
pub use wire::{ErrorCode, FileRef, PROTOCOL_VERSION};

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{}: {source}", path.display())]
    File { path: PathBuf, source: io::Error },
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("the Mac refused: {message} ({code:?})")]
    Refused { code: ErrorCode, message: String },
    #[error("pairing failed: {0}")]
    Pairing(String),
    #[error("{} already exists with different content", .0.display())]
    Exists(PathBuf),
    #[error("{0}")]
    Session(#[from] ds_session::SessionError),
    #[error("identity: {0}")]
    Identity(String),
}

impl RemoteError {
    pub(crate) fn file(path: &std::path::Path, source: io::Error) -> Self {
        Self::File {
            path: path.to_path_buf(),
            source,
        }
    }

    /// The server's error code, if the server refused the request.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Refused { code, .. } => Some(*code),
            _ => None,
        }
    }
}

pub type Result<T, E = RemoteError> = std::result::Result<T, E>;
