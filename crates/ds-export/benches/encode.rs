//! Export encode budget: a 6000×4000 (A9 full-res) 16-bit buffer to JPEG q90
//! (4:4:4) and to 16-bit TIFF, q85 (4:2:0), the linear-light resize to 2560 px
//! and in-place sharpening, plus the raw JPEG encoder comparison that chose mozjpeg.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ds_core::Size;
use ds_export::{ExifMeta, ExportFormat, Image16, OutputSpace, encode, icc};

/// Photo-like synthetic content: smooth gradients plus fine noise.
fn synth16(w: u32, h: u32) -> Image16 {
    let mut s = 0x1234_5678_u32;
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let n = s & 0x3ff;
            data.push((x * 60_000 / w + n) as u16);
            data.push((y * 60_000 / h + n) as u16);
            data.push(((x + y) * 30_000 / (w + h) + n) as u16);
        }
    }
    Image16::new(Size::new(w, h), data).unwrap()
}

fn bench(c: &mut Criterion) {
    let img = synth16(6000, 4000);
    let exif = ExifMeta {
        model: Some("ILCE-9".into()),
        ..Default::default()
    }
    .dirs(false);
    let profile = icc::profile(OutputSpace::Srgb);

    let mut g = c.benchmark_group("export_6000x4000_16bit");
    g.sample_size(10);
    g.bench_function("jpeg_q90", |b| {
        b.iter(|| {
            encode(
                black_box(&img),
                ExportFormat::Jpeg { quality: 90 },
                profile,
                Some(&exif),
                None,
            )
            .unwrap()
            .len()
        });
    });
    g.bench_function("tiff16", |b| {
        b.iter(|| {
            encode(
                black_box(&img),
                ExportFormat::Tiff16,
                profile,
                Some(&exif),
                None,
            )
            .unwrap()
            .len()
        });
    });
    g.bench_function("jpeg_q85_420", |b| {
        b.iter(|| {
            encode(
                black_box(&img),
                ExportFormat::Jpeg { quality: 85 },
                profile,
                Some(&exif),
                None,
            )
            .unwrap()
            .len()
        });
    });
    g.bench_function("resize_linear_to_2560", |b| {
        b.iter(|| {
            ds_export::process::resize(black_box(&img), Size::new(2560, 1707))
                .data
                .len()
        });
    });
    g.bench_function("sharpen_in_place", |b| {
        let mut work = img.clone();
        b.iter(|| ds_export::process::sharpen_in_place(black_box(&mut work), 0.5));
    });
    g.finish();

    // Encoder selection: both on the same 8-bit RGB, q90.
    let rgb8 = img.to_rgb8();
    let mut g = c.benchmark_group("jpeg_encoder_choice_6000x4000_q90");
    g.sample_size(10);
    g.bench_function("mozjpeg_fastest", |b| {
        b.iter(|| {
            let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
            comp.set_size(6000, 4000);
            comp.set_fastest_defaults();
            comp.set_quality(90.0);
            let mut s = comp.start_compress(Vec::new()).unwrap();
            s.write_scanlines(black_box(&rgb8)).unwrap();
            s.finish().unwrap().len()
        });
    });
    g.bench_function("jpeg_encoder", |b| {
        b.iter(|| {
            let mut out = Vec::new();
            let enc = jpeg_encoder::Encoder::new(&mut out, 90);
            enc.encode(black_box(&rgb8), 6000, 4000, jpeg_encoder::ColorType::Rgb)
                .unwrap();
            out.len()
        });
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
