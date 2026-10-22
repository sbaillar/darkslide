//! Adobe Lightroom Classic and Camera Raw import (see `docs/LR_FORMAT.md`).
//!
//! Everything here **reads** a Lightroom source and never writes to it:
//! a `.lrcat` catalog is copied into a temporary directory and the copy is
//! opened, XMP sidecars and presets are read with plain `File::open`.

mod catalog;
mod error;
mod groups;
mod import;
mod journal;
mod limits;
mod lua;
mod map;
mod map_local;
mod matching;
mod model;
mod preset;
mod sidecars;
mod state;
mod xmp;

pub use catalog::read_catalog;
pub use error::{LrError, Result};
pub use import::{
    Conflict, FolderCount, ImportOptions, ImportReport, NotFound, PhotoNote, Skipped, import,
};
pub use journal::{JOURNAL_FILE, RevertReport, can_revert, revert};
pub use limits::check_depth;
pub use lua::parse_lua_table;
pub use map::{MapContext, Mapped, map_develop};
pub use model::{
    Category, Develop, Fidelity, LrCollection, LrLibrary, LrPhoto, LrPreset, Setting, SourceKind,
};
pub use preset::read_presets;
pub use sidecars::read_sidecars;
pub use state::STATE_FILE;
pub use xmp::{XmpData, parse_xmp};
