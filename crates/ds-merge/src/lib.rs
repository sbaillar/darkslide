//! HDR merge and panorama stitching.
//!
//! Both take demosaiced frames in **linear camera RGB before white balance**, `0..=65535`
//! = `0..=1` of the sensor's clip (the layout of `ds_pipeline::denoise::linear_original`),
//! and return one frame in the same space, so the result can be written as a linear DNG
//! and developed like any RAW.
//!
//! - [`merge_hdr`]: aligns bracketed exposures (feature homographies), estimates their
//!   exposure ratios from the data, merges radiance with SNR weights and, with deghosting
//!   on, takes one exposure in regions where the frames disagree.
//! - [`merge_panorama`]: matches features between neighbouring frames (every pair when
//!   those leave gaps), fits camera rotations and a
//!   shared focal length, levels the horizon, projects (perspective, cylindrical or
//!   spherical; auto picks one), compensates exposure and blends seams with two bands.
//!
//! The result keeps every value below the clip: highlights that only the darkest frame
//! (or the darkest-gain frame) holds set the scale, and [`Merged::exposure_ev`] says how
//! far to brighten it to look like the reference frame.
//!
//! Heavy loops run on rayon by rows or bands and reuse their scratch, never allocating per
//! pixel. [`estimate_bytes`] gives a panorama's peak memory before anything is decoded.

mod features;
mod geom;
mod hdr;
mod image;
mod pano;

pub use geom::Mat3;
pub use hdr::{Deghost, HdrInput, HdrOptions, merge_hdr};
pub use image::{Frame, quick_frame};
pub use pano::{
    DEFAULT_MAX_PIXELS, MAX_FRAMES, PanoInput, PanoOptions, Projection, estimate_bytes,
    merge_panorama,
};

/// Full scale of a [`Frame`] sample (the sensor's clip).
pub const WHITE: f32 = 65535.0;

/// A merge result.
#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    pub frame: Frame,
    /// Exposure to give the result so it looks like the reference frame (EV, ≥ 0).
    pub exposure_ev: f32,
    /// Index of the input the result is registered to (its metadata describes the result).
    pub reference: usize,
    /// HDR: regions where one exposure was used because the frames disagreed.
    pub ghost_regions: u32,
    /// The projection used ([`Projection::Perspective`] for HDR).
    pub projection: Projection,
    /// HDR: frames that could not be aligned (merged as they are).
    pub unaligned: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MergeError {
    #[error("select at least two photos to merge")]
    TooFew,
    #[error("{0}")]
    Mismatch(String),
    #[error("{0}")]
    NoOverlap(String),
    #[error("{0}")]
    Invalid(String),
    #[error("cancelled")]
    Cancelled,
}

/// Progress and cancellation of a long merge. Called from worker threads.
pub trait Progress: Sync {
    /// Overall completion, `0.0..=1.0`, non-decreasing.
    fn set(&self, fraction: f32);
    fn cancelled(&self) -> bool;
}

/// No progress reporting, never cancelled.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn set(&self, _fraction: f32) {}

    fn cancelled(&self) -> bool {
        false
    }
}

/// A sub-range of a parent [`Progress`]: `set(f)` reports `start + f·(end − start)`.
pub(crate) struct Stage<'a> {
    parent: &'a dyn Progress,
    start: f32,
    end: f32,
}

impl<'a> Stage<'a> {
    pub(crate) fn new(parent: &'a dyn Progress, start: f32, end: f32) -> Self {
        Self { parent, start, end }
    }

    pub(crate) fn check(&self) -> Result<(), MergeError> {
        if self.parent.cancelled() {
            Err(MergeError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Progress for Stage<'_> {
    fn set(&self, fraction: f32) {
        self.parent
            .set(self.start + fraction.clamp(0.0, 1.0) * (self.end - self.start));
    }

    fn cancelled(&self) -> bool {
        self.parent.cancelled()
    }
}

#[cfg(test)]
pub(crate) mod testutil;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Log(Mutex<Vec<f32>>);

    impl Progress for Log {
        fn set(&self, f: f32) {
            self.0.lock().unwrap().push(f);
        }
        fn cancelled(&self) -> bool {
            true
        }
    }

    #[test]
    fn stages_map_into_their_range_and_forward_cancellation() {
        let log = Log::default();
        let s = Stage::new(&log, 0.5, 0.75);
        s.set(0.0);
        s.set(0.5);
        s.set(2.0);
        assert_eq!(*log.0.lock().unwrap(), vec![0.5, 0.625, 0.75]);
        assert!(s.cancelled());
        assert_eq!(s.check(), Err(MergeError::Cancelled));
        assert_eq!(Stage::new(&NoProgress, 0.0, 1.0).check(), Ok(()));
        NoProgress.set(0.3);
        assert!(!NoProgress.cancelled());
    }
}
