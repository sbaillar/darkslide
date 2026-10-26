//! Peak memory of an export with an output profile: the pixels are converted in place
//! on the working copy, not in a second image-sized buffer. Its own test binary so the
//! counting allocator sees only this test.

use std::sync::atomic::AtomicBool;

use lt_core::Size;
use lt_export::proof::{IccProfile, OutputProfile, ProofSettings};
use lt_export::{
    BatchOptions, ExportFormat, ExportItem, ExportRecipe, OutputSpace, Rendered, Renderer, icc,
    run_batch,
};
use peak_alloc::PeakAlloc;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

const W: u32 = 1500;
const H: u32 = 1000;
/// One full-resolution 16-bit RGB image.
const FULL: usize = (W * H * 6) as usize;

/// A renderer that sizes its own output (the develop pipeline's path): the batch owns
/// the buffer it gets.
struct SelfSized;

impl Renderer for SelfSized {
    fn render(&self, _: &ExportItem, _: OutputSpace) -> Result<Rendered, String> {
        unreachable!("render_for answers")
    }

    fn render_for(&self, _: &ExportItem, _: &ExportRecipe) -> Option<Result<Rendered, String>> {
        Some(Ok(Rendered::Rgb16 {
            size: Size::new(W, H),
            data: (0..(W * H * 3) as usize)
                .map(|i| (i * 7919) as u16)
                .collect(),
        }))
    }
}

#[test]
fn an_output_profile_converts_the_working_copy_in_place() {
    let d = tempfile::tempdir().unwrap();
    let p3 = IccProfile::from_bytes(icc::profile(OutputSpace::DisplayP3)).unwrap();
    let mut r = ExportRecipe::new("p", ExportFormat::Png16, d.path());
    r.space = OutputSpace::Rec2020;
    r.output_profile = Some(OutputProfile::new(p3, ProofSettings::default()).unwrap());
    let items = [ExportItem {
        id: 1,
        name: "a".into(),
        ..Default::default()
    }];
    let go = |name: &str| {
        let mut r = r.clone();
        r.name = name.into();
        run_batch(
            &items,
            &[r],
            &SelfSized,
            BatchOptions {
                max_in_flight: 1,
                ..BatchOptions::default()
            },
            &|_| {},
            &AtomicBool::new(false),
        )
    };
    // Warm the colour tables, LUTs and rayon pool so they are not charged to the run.
    assert!(go("warm").failed.is_empty());
    let base = PEAK.current_usage();
    PEAK.reset_peak_usage();
    let rep = go("measured");
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let peak = PEAK.peak_usage().saturating_sub(base);
    // The render, plus slack for the transform's tables and the encoder; a copy of the
    // image for the conversion would add a whole second FULL.
    let bound = FULL + (4 << 20);
    eprintln!(
        "peak {:.1} MB, bound {:.1} MB",
        peak as f64 / 1e6,
        bound as f64 / 1e6
    );
    assert!(peak <= bound, "peak {peak} > bound {bound}");
}
