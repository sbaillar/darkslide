//! Soft proofing: building the viewer's 33³ proof table (once per profile, intent or
//! BPC change, off the main thread) for a matrix profile and, when the system has
//! them, a LUT CMYK profile; and converting an A9 frame (6000×4000, 16-bit) from
//! Display P3 to Adobe RGB for an export to a proof profile.

use std::hint::black_box;
use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};
use ds_export::proof::{IccProfile, LUT_SIZE, OutputProfile, ProofSettings, proof_lut};
use ds_export::{OutputSpace, icc};

const SYSTEM: &str = "/System/Library/ColorSync/Profiles";

fn system(name: &str) -> Option<IccProfile> {
    IccProfile::read(&Path::new(SYSTEM).join(name)).ok()
}

fn bench(c: &mut Criterion) {
    let s = ProofSettings::default();
    let srgb = IccProfile::from_bytes(icc::profile(OutputSpace::Srgb)).unwrap();
    c.bench_function("proof_lut_srgb", |b| {
        b.iter(|| proof_lut(black_box(&srgb), s, LUT_SIZE).unwrap());
    });
    if let Some(cmyk) = system("Generic CMYK Profile.icc") {
        c.bench_function("proof_lut_generic_cmyk", |b| {
            b.iter(|| proof_lut(black_box(&cmyk), s, LUT_SIZE).unwrap());
        });
    }
    let adobe = system("AdobeRGB1998.icc")
        .unwrap_or_else(|| IccProfile::from_bytes(icc::profile(OutputSpace::DisplayP3)).unwrap());
    let out = OutputProfile::new(adobe, s).unwrap();
    let (w, h) = (6000usize, 4000usize);
    let src: Vec<u16> = (0..w * h * 3).map(|i| (i * 7919 % 65_536) as u16).collect();
    let mut g = c.benchmark_group("export_convert");
    g.sample_size(10);
    g.bench_function("p3_to_adobe_rgb_6000x4000_16bit", |b| {
        b.iter_batched_ref(
            || src.clone(),
            |px| out.convert_rgb16(OutputSpace::DisplayP3, px, w).unwrap(),
            criterion::BatchSize::LargeInput,
        );
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
