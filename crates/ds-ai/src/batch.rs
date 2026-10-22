//! Batch analysis with backend selection: the GPU ([`GpuAnalyzer`]) when an
//! adapter is available, else the CPU reference in parallel across images.
//! Both return exactly what [`crate::analyze_with_subject`] returns for each
//! image.

use rayon::prelude::*;

use crate::LumaImage;
use crate::analysis::{AnalysisConfig, ImageAnalysis};
use crate::gpu::GpuAnalyzer;
use crate::sharpness::SubjectRegion;

/// Analyzes batches of previews on the best available backend.
#[derive(Debug)]
pub enum Analyzer {
    Gpu(GpuAnalyzer),
    Cpu(AnalysisConfig),
}

impl Analyzer {
    /// The GPU when an adapter can be opened, else the CPU.
    pub fn new(cfg: AnalysisConfig) -> Self {
        GpuAnalyzer::with_config(cfg).map_or(Self::Cpu(cfg), Self::Gpu)
    }

    /// Always the CPU (parallel across images).
    pub fn cpu(cfg: AnalysisConfig) -> Self {
        Self::Cpu(cfg)
    }

    pub fn is_gpu(&self) -> bool {
        matches!(self, Self::Gpu(_))
    }

    /// `out[i] == analyze(&images[i], cfg)`.
    pub fn analyze_batch(&self, images: &[LumaImage]) -> Vec<ImageAnalysis> {
        self.analyze_batch_with_subjects(images, &[])
    }

    /// `out[i] == analyze_with_subject(&images[i], cfg, &subjects[i])`;
    /// `subjects` is empty or holds one list per image.
    ///
    /// # Panics
    /// When `subjects` is neither empty nor as long as `images`.
    pub fn analyze_batch_with_subjects(
        &self,
        images: &[LumaImage],
        subjects: &[Vec<SubjectRegion>],
    ) -> Vec<ImageAnalysis> {
        match self {
            Self::Gpu(g) => g.analyze_batch_with_subjects(images, subjects),
            Self::Cpu(cfg) => analyze_batch_cpu_with_subjects(images, subjects, cfg),
        }
    }
}

/// The CPU reference over a batch, parallel across images (rayon).
pub fn analyze_batch_cpu(images: &[LumaImage], cfg: &AnalysisConfig) -> Vec<ImageAnalysis> {
    analyze_batch_cpu_with_subjects(images, &[], cfg)
}

/// [`analyze_batch_cpu`] with subject regions (empty, or one list per image).
///
/// # Panics
/// When `subjects` is neither empty nor as long as `images`.
pub fn analyze_batch_cpu_with_subjects(
    images: &[LumaImage],
    subjects: &[Vec<SubjectRegion>],
    cfg: &AnalysisConfig,
) -> Vec<ImageAnalysis> {
    assert!(
        subjects.is_empty() || subjects.len() == images.len(),
        "subjects: {} lists for {} images",
        subjects.len(),
        images.len()
    );
    images
        .par_iter()
        .enumerate()
        .map(|(i, img)| {
            let s = subjects.get(i).map_or(&[][..], Vec::as_slice);
            crate::analyze_with_subject(img, cfg, s)
        })
        .collect()
}
