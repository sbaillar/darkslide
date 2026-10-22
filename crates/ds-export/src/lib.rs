//! Export: recipes → sized, sharpened, color-tagged files with EXIF passthrough, and the
//! linear DNG of a new photo made from a RAW ([`dng`], AI Upscale).
//!
//! Rendering is injected ([`Renderer`]) so this crate does not depend on
//! `ds-pipeline`; HEIC is injected ([`HeicEncoder`]) because it is encoded by
//! `ImageIO` on the Swift side. Outputs are written atomically and never
//! overwrite an existing file (a `-N` suffix is added instead). Text watermarks are
//! rasterised by an injected [`TextRasterizer`] (Core Text on the Swift side).

pub mod batch;
pub mod dng;
pub mod encode;
pub mod exif;
pub mod icc;
pub mod image;
pub mod output;
pub mod post;
pub mod process;
pub mod proof;
pub mod raw_meta;
pub mod recipe;
pub mod watermark;
pub mod xmp;

use std::path::PathBuf;

use ds_core::Size;

pub use batch::{
    BatchOptions, BatchReport, ExportFailure, ExportItem, ExportProgress, ExportedFile, Renderer,
    Upscaler, Written, encodes_8_bit, export_one, export_one_with, export_prepared, run_batch,
};
pub use dng::{DngInfo, LinearDngWriter};
pub use encode::{HeicEncoder, encode, encode_pixels_to, encode_to};
pub use exif::{ExifMeta, Gps};
pub use image::{Image16, Pixels, Rendered};
pub use output::Namer;
pub use post::{CanvasLayout, PhotoRect, PostProcess, PostProcessSteps, WatermarkStep};
pub use raw_meta::exif_from_raw;
pub use recipe::{ExifPolicy, ExportFormat, ExportRecipe, OutputSpace, Sizing, target_size};
pub use watermark::{Anchor, Logo, Mark, TextMask, TextRasterizer, Watermark};

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("invalid recipe {0}")]
    InvalidRecipe(String),
    #[error("invalid image: {0}")]
    InvalidImage(String),
    #[error("output too large: {}x{}", .0.width, .0.height)]
    TooLarge(Size),
    #[error("render failed: {0}")]
    Render(String),
    #[error("encode failed: {0}")]
    Encode(String),
    #[error("ICC profile: {0}")]
    Profile(String),
    #[error("watermark: {0}")]
    Watermark(String),
    /// A post-process step (a plug-in) failed; the message names it.
    #[error("{0}")]
    PostProcess(String),
    #[error("HEIC requested but no HEIC encoder is available")]
    NoHeicEncoder,
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl ExportError {
    /// The destination ran out of space (ENOSPC from a write, or libjpeg's own
    /// "out of disk space"): the batch stops instead of failing every later file.
    pub fn is_disk_full(&self) -> bool {
        match self {
            Self::Io { source, .. } => {
                source.kind() == std::io::ErrorKind::StorageFull
                    || source.raw_os_error() == Some(28)
            }
            Self::Encode(m) => {
                m.contains("out of disk space")
                    || m.contains("No space left on device")
                    || m.contains("os error 28")
            }
            _ => false,
        }
    }
}
