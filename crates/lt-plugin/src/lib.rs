//! Luxtable's plug-in host (see docs/PLUGINS.md).
//!
//! - [`manifest`]: `plugin.toml` (format 1), permissions, contracts per kind.
//! - [`host`]: the registry of bundled and installed plug-ins, the user's decisions
//!   (`state.json`), install with pinned hashes, clashes and downgrades.
//! - [`trust`]: folder hashes and Ed25519 signatures.
//! - [`pack`]: data packs (looks, styles, voice phrases).
//! - [`wasm`]: the WebAssembly runtime (wasmi, fuel and memory limits) and
//!   `export_post_process`.
//! - [`destination`]: the `export_destination` contract (specified; no runner yet).
//! - [`cli`]: the `lt-plugin` command (new, validate, pack, keygen, sign).

pub mod builtin;
pub mod cli;
pub mod destination;
pub mod host;
pub mod manifest;
pub mod pack;
pub mod package;
pub mod params;
pub mod state;
pub mod text;
pub mod trust;
pub mod wasm;

pub use host::{Approval, Host, InstallOptions, InstallOutcome, Origin, Plugin, Status, Trust};
pub use manifest::{Contribution, Kind, Manifest, Permission, Platform};

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("{0}")]
    Manifest(String),
    #[error("{0}")]
    Invalid(String),
    #[error("signature: {0}")]
    Signature(String),
    #[error("{0}")]
    Install(String),
    #[error("{0}")]
    Permission(String),
    /// A plug-in's code failed (the message names it).
    #[error("{0}")]
    Plugin(String),
    #[error("no plug-in {0}")]
    NotFound(String),
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// This build's Luxtable version (for `min_luxtable`).
#[must_use]
pub fn luxtable_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| semver::Version::new(0, 0, 0))
}
