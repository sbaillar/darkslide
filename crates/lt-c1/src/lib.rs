//! Capture One session import (see `docs/C1_FORMAT.md` for the formats).
//!
//! Everything here **reads** a Capture One session and never writes to it:
//! the `.cosessiondb` is copied into a temporary directory and opened there,
//! and `.cos` files are read with plain `File::open`.
//!
//! - [`read_session`]: a C1 session folder → [`C1Session`] (images, variants
//!   with their default and adjustment layers, albums, smart albums).
//! - [`map_variant`]: one C1 variant → what Luxtable would store ([`Mapped`]),
//!   plus the settings that have no Luxtable equivalent.
//! - [`import`]: apply a [`C1Session`] to the Luxtable session of the same
//!   folder; idempotent, three-way merged against what the last import wrote,
//!   with a dry run.
//! - [`revert`]: take back the imports since the last revert (journaled).
//! - [`settings::read_app_settings`]: Capture One's application settings
//!   (recipes, workspaces, crop ratios, styles, keyword libraries, a few
//!   preferences) and what each maps to in Luxtable.

mod cos;
mod criteria;
mod db;
mod error;
mod import;
mod journal;
mod map;
mod model;
mod read;
pub mod settings;
mod state;
mod xml;

pub use cos::{CosFile, CosVariant, parse_cos};
pub use criteria::{Condition, Criteria, parse_criteria};
pub use db::{DbImage, DbLocation, DbSession, DbVariant, read_db, read_db_cancellable};
pub use error::{C1Error, Result};
pub use import::{
    Conflict, Field, ImportOptions, ImportReport, Skipped, import, import_cancellable,
};
pub use journal::{JOURNAL_FILE, RevertReport, can_revert, revert};
pub use map::{KeyClass, Mapped, WbConverter, classify, map_variant, split_keywords};
pub use model::{
    C1Collection, C1Folder, C1Image, C1Session, C1Variant, CollectionKind, Keys, Settings, Source,
};
pub use read::{find_session_db, read_session, read_session_cancellable};
pub use state::{STATE_FILE, Tools};

#[cfg(test)]
mod fixtures;
