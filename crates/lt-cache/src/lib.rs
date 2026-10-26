//! Thumbnail/preview caching and background work scheduling for Luxtable.
//!
//! - [`DiskCache`]: encoded JPEG thumbnails (512 px) and previews (2560 px)
//!   under `<session>/.lt/cache/`, keyed by (variant, original hash, adjustments hash, kind, pipeline version),
//!   with atomic writes and a byte cap enforced by LRU eviction.
//! - [`MemLru`]: byte-bounded in-memory LRU for decoded previews.
//! - [`JobScheduler`]: prioritized (`Visible > Prefetch > Background`),
//!   cancellable, deduplicating worker pool.
//!
//! Decoding, rendering, and JPEG encoding are the caller's job (closures), so
//! this crate has no dependency on `lt-raw` or `lt-pipeline`.

pub mod disk;
pub mod lru;
pub mod scheduler;

pub use disk::{
    AdjHash, CacheError, CacheKey, DiskCache, Kind, PIPELINE_VERSION, adjustments_hash, render_hash,
};
pub use lru::{MemLru, Weighted};
pub use scheduler::{CancelToken, JobResult, JobScheduler, Priority, Submitted, perf_cores};
