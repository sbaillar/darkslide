//! `UniFFI` facade over the Darkslide Rust core.
//!
//! This crate is the *only* surface the Swift app sees. It stays thin: it
//! converts between FFI-friendly records and the domain types of the other
//! crates, offloads blocking work to worker threads so Swift `async` calls never
//! park a cooperative thread, and forwards progress through a callback
//! interface. See `docs/FFI.md` for the module layout and threading rules.
//!
//! Module map (one module per future domain facade):
//! - [`engine`]    the `Engine` object: owns shared state (renderer, caches, progress)
//! - [`frame`]     `RenderRequest` / `FrameFormat` / `FrameInfo` + `plan_frame`
//! - [`render`]    the test pattern (stand-in for `ds-pipeline`), rendered into caller memory
//! - [`jobs`]      `CancelToken` and the panic-safe, cancellable worker helper
//! - [`raw`]       RAW metadata + embedded previews (`ds-raw`)
//! - [`clipboard`] copy/apply group metadata (mirrors `ds_core::AdjustGroup`)
//! - [`profiles`]  camera profiles: built-in looks and imported Adobe `.dcp` files
//! - [`proof`]     soft proofing: the viewer's proof table for an ICC profile
//! - [`denoise`]   AI Denoise: tiles to and from the app's Core ML model, cached results
//! - [`upscale`]   AI Upscale: tiles to and from the app's Core ML model, a new photo's DNG
//! - [`optimize`]  AI Optimize: measured, explained edit proposals, and applying them
//! - [`session`]   sessions (`ds-session`): open / open folder in place, variants, ratings/tags/picks, adjustments as JSON
//!   (`session::search`: natural-language / find-similar search, CLIP tokenizer;
//!   `session::merge`: Merge to HDR / Panorama into a new linear DNG)
//! - [`ai`]        culling scores, burst grouping, offline NL parser (`ds-ai`)
//! - [`matching`]  match total exposures / white balance across a set to the focused photo
//! - [`export`]    process recipes → files (`ds-export`), `ImageEncoder` seam, preview renderer
//! - [`import`]    card import (`ds-import`): scan, verified copy into a session, resume
//! - [`c1`]        Capture One session import into an in-place session (`ds-c1`)
//! - [`c1_settings`] Capture One application settings: recipes, workspaces, styles… (`ds-c1`)
//! - [`lr`]        Lightroom catalog / sidecar / preset import (`ds-lr`)
//! - [`remote`]    the Mac ↔ iPad link server and paired devices (`ds-remote`)
//! - [`events`]    progress events + the `ProgressListener` callback interface
//! - [`error`]     the single `DsError` enum every fallible call returns
//!
//! Planned (slot in as the crates land): `archive`, real `render` (ds-pipeline).

uniffi::setup_scaffolding!();

pub mod ai;
pub mod archive;
pub mod background;
pub mod c1;
pub mod c1_settings;
pub mod clipboard;
pub mod denoise;
pub(crate) mod develop;
pub mod engine;
pub mod error;
pub mod events;
pub mod export;
pub mod frame;
pub mod gphotos;
pub mod import;
pub mod jobs;
pub mod lr;
pub mod matching;
pub mod optimize;
pub mod plugins;
pub mod profiles;
pub mod proof;
pub mod raw;
pub mod remote;
pub mod removal;
pub mod render;
pub mod session;
pub mod tools;
pub mod upscale;
pub mod viewer;
pub mod watermark;

pub use background::note_user_activity;
pub use clipboard::{AdjustGroup, AdjustGroupInfo, adjust_groups};
pub use engine::Engine;
pub use error::DsError;
pub use events::{ProgressEvent, ProgressListener, TaskPhase};
pub use frame::{FrameFormat, FrameInfo, NormRect, RenderRequest, plan_frame};
pub use jobs::CancelToken;
pub use raw::{
    EmbeddedPreviewData, RawMetadata, embedded_preview, filename_sequence, is_supported_raw,
    raw_metadata,
};
pub use render::TestPatternParams;
pub use session::{
    AdjustmentsJson, AllAdjustmentsJson, ColorLabel, OpenReport, PickFlag, SessionHandle,
    VariantRecord, default_adjustments_json, normalize_adjustments_json, open_folder_in_place,
    open_session,
};

/// Crate version plus the FFI contract revision, e.g. `"0.1.0 (ffi 2)"`.
/// The Swift side compares `FFI_CONTRACT` to catch stale generated bindings.
#[uniffi::export]
pub fn version() -> Result<String, DsError> {
    jobs::guard(|| {
        Ok(format!(
            "{} (ffi {FFI_CONTRACT})",
            env!("CARGO_PKG_VERSION")
        ))
    })
}

/// Bump whenever an exported signature changes in a way old bindings can't call.
pub const FFI_CONTRACT: u32 = 38;

/// Numeric contract revision (for a cheap equality check from Swift).
#[uniffi::export]
pub fn ffi_contract() -> Result<u32, DsError> {
    jobs::guard(|| Ok(FFI_CONTRACT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_mentions_crate_version_and_contract() {
        let v = version().unwrap();
        assert!(v.starts_with(env!("CARGO_PKG_VERSION")));
        assert!(v.ends_with(&format!("(ffi {FFI_CONTRACT})")));
        assert_eq!(ffi_contract().unwrap(), FFI_CONTRACT);
    }
}
