//! One call that computes every per-image signal from a luma preview.

use serde::{Deserialize, Serialize};

use crate::exposure::{self, Exposure, ExposureConfig};
use crate::noise::{self, Noise, NoiseConfig};
use crate::phash::PerceptualHash;
use crate::sharpness::{self, Sharpness, SharpnessConfig, SubjectRegion};
use crate::{LumaImage, tiles};

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AnalysisConfig {
    pub sharpness: SharpnessConfig,
    pub exposure: ExposureConfig,
    pub noise: NoiseConfig,
}

/// Everything lt-ai knows about one image from its pixels (plus an optional
/// subject region).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ImageAnalysis {
    pub width: usize,
    pub height: usize,
    pub sharpness: Sharpness,
    pub exposure: Exposure,
    pub noise: Noise,
    pub hash: PerceptualHash,
}

impl ImageAnalysis {
    /// Contrast-normalized, noise-corrected sharpness of the subject (when a
    /// region was given) or the focus region.
    pub fn sharpness_value(&self) -> f32 {
        self.sharpness.value()
    }
}

/// Analyze one preview without subject information. Single-threaded;
/// callers parallelize across images.
pub fn analyze(img: &LumaImage, cfg: &AnalysisConfig) -> ImageAnalysis {
    analyze_with_subject(img, cfg, &[])
}

/// Analyze one preview, measuring sharpness on the given subject regions
/// (Vision faces/saliency, AF point) when they cover any detail.
pub fn analyze_with_subject(
    img: &LumaImage,
    cfg: &AnalysisConfig,
    subject: &[SubjectRegion],
) -> ImageAnalysis {
    let grid = tiles::tile_pass(img, cfg.sharpness.tile);
    let noise = noise::measure(img, &grid, cfg.noise);
    ImageAnalysis {
        width: img.width(),
        height: img.height(),
        sharpness: sharpness::measure(img, &grid, &cfg.sharpness, noise.laplacian_floor, subject),
        exposure: exposure::measure(&exposure::histogram(img), cfg.exposure),
        noise,
        hash: PerceptualHash::of(img),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_image_analyzes_to_neutral_values() {
        let img = LumaImage::from_gray8(0, 0, Vec::new()).unwrap();
        let a = analyze(&img, &AnalysisConfig::default());
        assert_eq!(a.sharpness_value(), 0.0);
        assert_eq!(a.exposure, Exposure::default());
        assert_eq!(a.noise, Noise::default());
        assert_eq!(a.hash, PerceptualHash::default());
    }
}
