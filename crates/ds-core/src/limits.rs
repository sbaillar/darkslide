//! Limits for data that may come from someone else's folder (a shared or
//! downloaded session, a Capture One import): one place, used by the sidecar,
//! albums, people and preset readers and by [`crate::adjust::Adjustments::sanitize`].

/// Largest JSON file read into memory (sidecars, albums, people, presets).
pub const MAX_JSON_BYTES: u64 = 8 << 20;
/// Most layers in one sidecar.
pub const MAX_LAYERS: usize = 64;
/// Most spots, and most removals, in one sidecar.
pub const MAX_SPOTS: usize = 1_000;
/// Most brush strokes in one mask shape or removal.
pub const MAX_STROKES: usize = 1_000;
/// Most points in one brush stroke or curve.
pub const MAX_POINTS: usize = 10_000;
/// Most shapes in one mask, and most advanced color ranges.
pub const MAX_SHAPES: usize = 64;
/// Most people boxes kept per image.
pub const MAX_PERSON_BOXES: usize = 1_000;
/// Most keywords on one image, and the longest one in bytes.
pub const MAX_KEYWORDS: usize = 500;
pub const MAX_KEYWORD_BYTES: usize = 256;
