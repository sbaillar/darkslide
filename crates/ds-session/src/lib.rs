//! Darkslide sessions: self-contained folders of originals, per-variant JSON
//! sidecars (the source of truth), and a rebuildable SQLite index.
//!
//! - [`Session`]: create/open (with index validation and rebuild), originals and
//!   variants, ratings/tags/picks/keywords (single or batch), adjustments with
//!   undo/redo, copy/apply as one undo step, filters and albums, `move_to`,
//!   `empty_trash`, `rescan` (relink by BLAKE3), `snapshot_to`.
//! - [`PresetStore`] / [`Preset`]: named partial adjustment stacks.
//! - [`GlobalIndex`]: all sessions and original hashes, for dedupe and search;
//!   `find` across sessions (people, places, dates, ratings, description).
//! - The Library: every session's photos in one index ([`GlobalIndex::library_query`]),
//!   collections across sessions, edits written through to the owning session.
//! - The map: variant geotags (sidecar) and [`GlobalIndex::map_points`].
//! - Search embeddings (`embedding_jobs`, `store_embeddings`, `search_embeddings`,
//!   `similar_to`), keyed by content identity so renames never re-embed.

mod album;
mod atomic;
mod db;
mod edit;
mod embed;
mod error;
mod external;
mod faces;
mod files;
mod filter;
mod find;
mod global;
pub mod identity;
mod journal;
mod library;
mod map;
mod people;
mod preset;
mod relpath;
mod session;
mod sidecar;
mod trash;
mod undo;

pub use album::{Album, AlbumId, AlbumKind, builtin_smart_folders};
pub use atomic::write_atomic as write_file_atomic;
pub use db::{INDEX_SCHEMA_VERSION, Recovery};
pub use edit::BatchApplied;
pub use embed::{EmbeddingCounts, EmbeddingJob, SearchHit, embedding_key};
pub use error::{Result, SessionError};
pub use external::{
    ExternalSync, Found, Migrated, RelinkReport, Stat, ToVerify, Verified, original_stat,
};
pub use faces::{Face, FaceCounts, FaceJob, FaceRef, NewFace, PhotoPeople, PhotoPlace};
pub use files::{Moved, RescanReport, TrashedRejects};
pub use filter::{Filter, Sort, SortKey};
pub use find::{
    FindOptions, FindOutcome, FindPhoto, FindRecord, FindSpec, MatchReason, PersonCentroid,
    PersonQuery, PlaceResolution, PlaceSource, SessionMatch, SessionReason,
};
pub use global::{GlobalHit, GlobalIndex, GlobalSearchHit, RegisteredSession};
pub use library::{
    KnownEdits, LibraryCollection, LibraryDay, LibraryEdit, LibraryItem, LibraryOverview,
    LibraryQuery, LibraryRecord, LibrarySession, LibrarySessionCount, LibrarySort, LibrarySource,
    LibraryTargets, PickFilter, apply_library_edit, edit_hash, index_stamp,
};
pub use map::GlobalMapPoint;
pub use people::{Exemplar, MAX_EXEMPLARS, PEOPLE_SCHEMA_VERSION, People, PeopleStore, Person};
pub use preset::{PRESET_SCHEMA_VERSION, Preset, PresetStore};
pub use relpath::{Folder, MAX_PLAIN_NAME, RelPath, original_path, sidecar_file_name};
pub use session::is_capture_one_session;
pub use session::{
    DEVELOPED_DIR, DEVELOPED_MARKER, ImageId, ImageInfo, MAX_EXTERNAL_DEPTH, MAX_EXTERNAL_FILES,
    NewOriginal, Originals, Session, SyncReport, UNHASHED, VariantId, VariantInfo, neutral_default,
};
pub use sidecar::{ImageMetaLite, MIGRATIONS, Migration, PersonTag, Sidecar, iou};
pub use trash::{SystemTrash, Trasher};
pub use undo::{GroupId, UndoConfig};

#[cfg(test)]
mod adv_tests;
#[cfg(test)]
mod embed_tests;
#[cfg(test)]
mod ext_tests;
#[cfg(test)]
mod faces_tests;
#[cfg(test)]
mod find_tests;
#[cfg(test)]
mod iptc_tests;
#[cfg(test)]
mod library_tests;
#[cfg(test)]
mod smart_tests;
#[cfg(test)]
mod tests;
