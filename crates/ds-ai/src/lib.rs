//! Culling intelligence for Darkslide.
//!
//! Input is always a plain luma plane ([`LumaImage`]), typically the camera's
//! embedded preview (1616×1080 on the Sony A9) converted once from RGBA. From
//! it ds-ai derives:
//!
//! - **sharpness** (contrast-normalized Laplacian energy per tile, on the
//!   caller's subject region or a focus region, compared tile-for-tile
//!   across a group, plus motion-blur detection) — [`sharpness`];
//! - **exposure quality** (clipping, mean/median, dynamic range) — [`exposure`];
//! - **noise** (MAD of a high-pass on flat tiles) — [`noise`];
//! - **perceptual hashes** (dHash + pHash) — [`phash`];
//!
//! all at once via [`analyze`], or for many previews at once on the GPU via
//! [`GpuAnalyzer`] / [`Analyzer`] (bit-identical results). On top of those:
//!
//! - [`group`]: burst / near-duplicate grouping into "moments" per camera,
//!   from capture times, shot numbers, hash distance, and optional Apple
//!   Vision feature-print distances;
//! - [`cull`]: a 0–1 [`CullScore`] with human-readable [`Reason`]s, merging
//!   optional Vision signals (eyes open, face quality, aesthetics), and
//!   [`suggest_picks`] for a group;
//! - [`matching`]: exposure matching across a set (metering + EXIF prior);
//! - [`intent`]: the natural-language edit seam — typed [`EditIntent`],
//!   [`apply_intent`] (which reports what it could not apply), the
//!   [`IntentBackend`] trait and the offline
//!   [`RuleBasedParser`];
//! - [`faces`]: face clustering (average linkage under the user's must- and
//!   cannot-links), "Is this …?" suggestions and clustering-quality metrics;
//! - [`find`]: the find-query seam — free text ("pics of alex in iceland
//!   last summer") → typed [`FindQuery`] (people, places, dates, rating,
//!   descriptive text) with the offline [`RuleBasedFindParser`].
//!
//! Nothing here calls Vision or an LLM; those signals arrive as plain data
//! from the Swift side.

// Tests compare exactly-representable results on purpose.
#![cfg_attr(test, allow(clippy::float_cmp))]

pub mod analysis;
pub mod batch;
pub mod clip_tokenizer;
pub mod cull;
pub mod exposure;
pub mod faces;
pub mod find;
pub mod focus;
pub mod gpu;
pub mod group;
pub mod image;
pub mod intent;
pub mod matching;
pub mod noise;
pub mod phash;
pub mod sharpness;
mod stats;
#[cfg(test)]
mod testutil;
mod tiles;

pub use analysis::{AnalysisConfig, ImageAnalysis, analyze, analyze_with_subject};
pub use batch::{Analyzer, analyze_batch_cpu, analyze_batch_cpu_with_subjects};
pub use clip_tokenizer::{ClipTokenizer, TokenizerError};
pub use cull::{
    CullScore, CullThresholds, CullWeights, FaceSignal, PickSuggestion, Reason, VisionSignals,
    score, score_group, suggest_picks,
};
pub use exposure::{Exposure, ExposureConfig};
pub use faces::{ClusterConfig, ClusterQuality, Exemplars, Links};
pub use find::{
    CivilDate, DateFilter, DateTerm, FindBackend, FindQuery, FlagQuery, PersonMatch,
    PersonMatchKind, PersonTerm, QueryContext, RatingQuery, RuleBasedFindParser, match_people,
};
pub use focus::{GroupFocus, SubjectHints, group_focus};
pub use gpu::{GpuAnalyzer, GpuError};
pub use group::{Group, GroupConfig, GroupItem, GroupKind, VisionDistance, group_images};
pub use image::LumaImage;
pub use intent::{
    Applied, BandEdit, ColorBand, CropAspect, EditIntent, IntentBackend, IntentContext,
    IntentError, ParseReport, RuleBasedParser, Straighten, Unapplied, apply_intent,
};
pub use noise::{Noise, NoiseConfig};
pub use phash::PerceptualHash;
pub use sharpness::{Sharpness, SharpnessConfig, SubjectRegion, SubjectSharpness, TileMap};

/// Errors from constructing inputs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AiError {
    #[error("buffer of {len} bytes does not match {width}x{height}x{channels}")]
    BadBuffer {
        width: usize,
        height: usize,
        channels: usize,
        len: usize,
    },
}
