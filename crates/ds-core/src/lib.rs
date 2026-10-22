//! Shared domain types for Darkslide.
//!
//! Everything here is plain data: no I/O, no GPU. Other crates agree on these
//! types so the pipeline, session store, and UI can evolve independently.

pub mod adjust;
pub mod clipboard;
pub mod date;
pub mod edit_keys;
pub mod effects;
pub mod embedding;
pub mod gamut;
pub mod geom;
pub mod iptc;
mod lenient;
pub mod limits;
pub mod meta;
pub mod raw;
mod sanitize;

pub use adjust::Adjustments;

/// Version of the rendered output for the same adjustments: bumped by any change to what
/// `ds-pipeline` renders (math, defaults, their interpretation) and by any change to how
/// cached renditions are produced (encoder settings, thumbnail sizes). The single source
/// of truth: `ds_pipeline` and `ds_cache` re-export it, and cached renditions of another
/// version are discarded.
///
/// 2: ISO-adaptive noise reduction, lens-profile selection by crop factor, light-falloff
/// slider spanning the full profile (default 0.5).
/// 3: auto-fitted frames (straighten, keystone, lens) fill the original sensor size.
/// 4: presence (vibrance, texture, dehaze), heal/clone spots, the full layer tool set,
/// range masks and brush auto-mask; resized exports no longer gamut-compress twice.
/// 5: dehaze (patch dark channel, luminance recovery, cloud protection), texture and
/// layer sharpening strength/scale, heal gains, pinned spot sources, auto-mask colour,
/// capped output-space feathers, auto WB from a neutral on the locus.
/// 6: dehaze clears harder (veil capped per neighbourhood, local contrast); colour
/// ranges are refined edge-aware (no feather halo).
/// 7: colour-range keys without speckle (smoothed refinement guide).
/// 8: content-aware removals (`Adjustments::removals`) and the people AI targets.
/// 9: camera profiles (`Adjustments::profile`): the default edit renders with Darkslide
/// Standard (richer curve, colour twist); Neutral renders exactly what 8 rendered.
/// 10: effects (lens blur, glow, grain, halation; `Adjustments::effects`).
pub const PIPELINE_VERSION: u32 = 10;
pub use clipboard::{
    AdjustGroup, GroupSet, PasteMode, PasteOptions, PasteTarget, PasteWarning, Pasted,
};
pub use geom::{NormRect, Orientation, Size};
pub use iptc::{IptcField, IptcMeta};
pub use meta::{ColorTag, FileHash, GeoPoint, PickState, Rating};
pub use raw::{CfaPattern, RawImage};

/// Current sidecar schema version. Bump on any breaking change to [`Adjustments`]
/// and add a migration in `ds-session`.
pub const SIDECAR_SCHEMA_VERSION: u32 = 1;
