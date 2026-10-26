//! Content-aware Remove: the app's inpainting model and the fill cache.
//!
//! The app registers its model once it is downloaded and loaded ([`set_inpainter`]);
//! every render from then on (viewer, thumbnails, export) fills new removals with it.
//! Without one, removals are filled by the pipeline's `PatchMatch`. Fills are cached per
//! image and removal chain in memory and, once the app names a folder
//! ([`set_removal_cache_dir`]), on disk; the model's identity is part of the key, so a
//! model that arrives later refills what `PatchMatch` did.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// The app's inpainting model (Core ML `LaMa`). Called from render threads.
#[uniffi::export(with_foreign)]
pub trait DevelopInpainter: Send + Sync {
    /// Identity of the model and its revision (part of every fill's cache key).
    fn model_id(&self) -> String;
    /// Longest edge the model works at (the crop is downscaled to fit).
    fn max_edge(&self) -> u32;
    /// Fill the `mask` pixels (`width × height`, 255 = fill) of the display-encoded image
    /// `rgb` (`width × height × 3`, row-major). Returns `width × height × 3` bytes, or
    /// `None` when inference failed (`PatchMatch` then fills it).
    fn inpaint(&self, width: u32, height: u32, rgb: Vec<u8>, mask: Vec<u8>) -> Option<Vec<u8>>;
}

/// Adapter from the foreign trait to the pipeline's `Inpainter`.
struct Model(Arc<dyn DevelopInpainter>);

impl lt_pipeline::Inpainter for Model {
    fn id(&self) -> String {
        self.0.model_id()
    }

    fn max_edge(&self) -> u32 {
        self.0.max_edge()
    }

    fn inpaint(&self, w: u32, h: u32, rgb: &[u8], mask: &[u8]) -> Option<Vec<u8>> {
        self.0.inpaint(w, h, rgb.to_vec(), mask.to_vec())
    }
}

fn slot() -> &'static Mutex<Option<Arc<dyn lt_pipeline::Inpainter>>> {
    static S: Mutex<Option<Arc<dyn lt_pipeline::Inpainter>>> = Mutex::new(None);
    &S
}

/// Use `inpainter` for new removal fills (`None`: the built-in `PatchMatch`).
#[uniffi::export]
pub fn set_inpainter(inpainter: Option<Arc<dyn DevelopInpainter>>) {
    *slot().lock().unwrap_or_else(PoisonError::into_inner) =
        inpainter.map(|m| Arc::new(Model(m)) as Arc<dyn lt_pipeline::Inpainter>);
}

/// What fills new removals: the model's identity, or the `PatchMatch` fallback's.
#[uniffi::export]
pub fn removal_engine() -> String {
    inpainter().map_or_else(
        || lt_pipeline::removal::PATCHMATCH_ID.to_owned(),
        |m| m.id(),
    )
}

/// Keep removal fills in `dir` across launches (`None`: memory only).
#[uniffi::export]
pub fn set_removal_cache_dir(dir: Option<String>) {
    lt_pipeline::removal::set_cache_dir(dir.map(PathBuf::from));
}

/// The registered model, for render requests.
pub(crate) fn inpainter() -> Option<Arc<dyn lt_pipeline::Inpainter>> {
    slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    impl DevelopInpainter for Fake {
        fn model_id(&self) -> String {
            "fake-1".into()
        }
        fn max_edge(&self) -> u32 {
            64
        }
        fn inpaint(&self, w: u32, h: u32, rgb: Vec<u8>, mask: Vec<u8>) -> Option<Vec<u8>> {
            assert_eq!(rgb.len(), (w * h * 3) as usize);
            assert_eq!(mask.len(), (w * h) as usize);
            Some(vec![7; rgb.len()])
        }
    }

    #[test]
    fn the_registered_model_reaches_the_pipeline() {
        // Tests share the process-wide slot: this is the only one that sets it.
        set_inpainter(Some(Arc::new(Fake)));
        assert_eq!(removal_engine(), "fake-1");
        let m = inpainter().unwrap();
        assert_eq!((m.id(), m.max_edge()), ("fake-1".to_owned(), 64));
        assert_eq!(m.inpaint(2, 1, &[0; 6], &[255, 0]), Some(vec![7; 6]));
        set_inpainter(None);
        assert_eq!(removal_engine(), lt_pipeline::removal::PATCHMATCH_ID);
        assert!(inpainter().is_none());
        set_removal_cache_dir(None);
    }
}
