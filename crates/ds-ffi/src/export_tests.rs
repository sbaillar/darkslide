use std::fs;
use std::io::Cursor;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

use ds_core::geom::{NormRect, Orientation};
use ds_core::{CfaPattern, RawImage};
use ds_pipeline::{CaptureInfo, ImageKey};
use futures_executor::block_on;

use super::*;

fn recipe(name: &str, format: ExportFileFormat, dest: &Path) -> ExportRecipeSpec {
    ExportRecipeSpec {
        name: name.into(),
        format,
        size: ExportSize::Original,
        allow_upscale: false,
        space: ExportColorSpace::Srgb,
        sharpen: 0.0,
        file_name_template: "{name}".into(),
        destination: dest.to_string_lossy().into_owned(),
        include_exif: true,
        strip_gps: false,
        constrain_to_sensor_pixels: false,
        output_profile: None,
        ai_upscale: false,
        watermark: None,
        post_process: vec![],
        label_tags: true,
    }
}

fn source(id: u64, path: &Path) -> ExportSource {
    ExportSource {
        id,
        path: path.to_string_lossy().into_owned(),
        seq: id,
        gps: None,
        artist: None,
        copyright: None,
        adjustments_json: None,
        keywords: Vec::new(),
        metadata: None,
        rating: None,
        label: None,
        pick: None,
    }
}

#[allow(clippy::unnecessary_wraps)] // fills `ExportSource::adjustments_json` directly
fn adjustments(f: impl FnOnce(&mut Adjustments)) -> Option<String> {
    let mut a = Adjustments::default();
    f(&mut a);
    Some(serde_json::to_string(&a).unwrap())
}

/// One recorded encode: format, width, height, RGB bytes, EXIF given.
type Call = (EncodedImageFormat, u32, u32, usize, bool);

/// Records calls; returns a marker "file" (or an error for quality 13).
#[derive(Default)]
struct FakeEncoder(
    Mutex<Vec<Call>>,
    Mutex<Vec<Vec<String>>>,
    Mutex<Vec<PhotoMetadata>>,
    Mutex<Vec<Option<Vec<u8>>>>,
);

impl ImageEncoder for FakeEncoder {
    fn encode(
        &self,
        format: EncodedImageFormat,
        width: u32,
        height: u32,
        rgb16: Vec<u8>,
        quality: u8,
        icc: Vec<u8>,
        exif: Option<Vec<u8>>,
        keywords: Vec<String>,
        metadata: PhotoMetadata,
        xmp: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, DsError> {
        assert_ne!(icc.len(), 0);
        self.1.lock().unwrap().push(keywords);
        self.2.lock().unwrap().push(metadata);
        self.3.lock().unwrap().push(xmp);
        if quality == 13 {
            return Err(DsError::Io {
                message: "encoder refused".into(),
            });
        }
        self.0
            .lock()
            .unwrap()
            .push((format, width, height, rgb16.len(), exif.is_some()));
        Ok(b"FAKE".to_vec())
    }

    /// 4×2, or an error for paths containing "nosize".
    fn upright_size(&self, path: String) -> Result<ImageDimensions, DsError> {
        if path.contains("nosize") {
            return Err(DsError::Io {
                message: "no header".into(),
            });
        }
        Ok(ImageDimensions {
            width: 4,
            height: 2,
        })
    }

    /// A 4×2 mid-grey image (at `target`'s size when given; brighter when
    /// sharpened), an error for paths containing "bad", a wrong size for "wrong".
    fn decode(
        &self,
        path: String,
        _space: ExportColorSpace,
        target: Option<DecodeTarget>,
    ) -> Result<DecodedImage, DsError> {
        if path.contains("bad") {
            return Err(DsError::Io {
                message: "undecodable".into(),
            });
        }
        if path.contains("short") {
            return Ok(DecodedImage {
                width: 4,
                height: 2,
                rgb16: vec![0; 3],
            });
        }
        let (w, h, v) = match target {
            Some(t) if !path.contains("wrong") => (
                t.width,
                t.height,
                if t.sharpen > 0.0 { 0x9000u16 } else { 0x8000 },
            ),
            Some(t) => (t.width + 1, t.height, 0x8000),
            None => (4, 2, 0x8000),
        };
        Ok(DecodedImage {
            width: w,
            height: h,
            rgb16: std::iter::repeat_n(v, (w * h * 3) as usize)
                .flat_map(u16::to_ne_bytes)
                .collect(),
        })
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<ProgressEvent>>);

impl ProgressListener for Events {
    fn on_progress(&self, event: ProgressEvent) -> Result<(), DsError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

/// A renderer with a fixed gradient image (no files needed).
struct Gradient {
    size: Size,
    delay: Duration,
}

impl Renderer for Gradient {
    fn render(&self, _: &ExportItem, _: OutputSpace) -> Result<Rendered, String> {
        std::thread::sleep(self.delay);
        let n = (self.size.width * self.size.height) as usize;
        Ok(Rendered::Rgb16 {
            size: self.size,
            data: (0..n * 3).map(|i| (i * 97 % 65_536) as u16).collect(),
        })
    }
}

impl ExportRenderer for Gradient {
    fn name(&self) -> String {
        "gradient".into()
    }
}

fn items(n: u64) -> Vec<ExportItem> {
    (1..=n)
        .map(|i| ExportItem {
            id: i,
            name: format!("IMG{i}"),
            seq: i,
            ..ExportItem::default()
        })
        .collect()
}

/// A minimal TIFF-container "RAW" (Make/Model/Orientation + a real JPEG
/// preview of `w`×`h` as stored): the embedded-preview path without samples.
fn tiny_raw(dir: &Path, name: &str, w: u32, h: u32, orientation: u16) -> PathBuf {
    let n = (w * h * 3) as usize;
    // Left half dark, right half bright: orientation shows in the output.
    let data = (0..n)
        .map(|i| {
            if (i / 3) as u32 % w < w / 2 {
                8_000
            } else {
                60_000
            }
        })
        .collect();
    let img = Image16::new(Size::new(w, h), data).unwrap();
    let jpeg = ds_export::encode(
        &img,
        ExportFormat::Jpeg { quality: 95 },
        ds_export::icc::profile(OutputSpace::Srgb),
        None,
        None,
    )
    .unwrap();
    let (make, model): (&[u8], &[u8]) = (b"SONY\0", b"ILCE-9\0");
    let entries = 5u32;
    let make_at = 8 + 2 + entries * 12 + 4;
    let model_at = make_at + make.len() as u32;
    let jpeg_at = (model_at + model.len() as u32 + 15) & !15;
    let mut f = b"II*\0".to_vec();
    f.extend(8u32.to_le_bytes());
    f.extend((entries as u16).to_le_bytes());
    for (tag, ty, count, value) in [
        (0x010F_u16, 2_u16, make.len() as u32, make_at),
        (0x0110, 2, model.len() as u32, model_at),
        (0x0112, 3, 1, u32::from(orientation)),
        (0x0201, 4, 1, jpeg_at),
        (0x0202, 4, 1, jpeg.len() as u32),
    ] {
        f.extend(tag.to_le_bytes());
        f.extend(ty.to_le_bytes());
        f.extend(count.to_le_bytes());
        f.extend(value.to_le_bytes());
    }
    f.extend(0u32.to_le_bytes());
    f.extend(make);
    f.extend(model);
    f.resize(jpeg_at as usize, 0);
    f.extend(&jpeg);
    let p = dir.join(name);
    fs::write(&p, f).unwrap();
    p
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(0x5EED_0000);

/// A synthetic linear-RGB "RAW" (`w`×`h`, sRGB primaries): left half dark,
/// right half bright, with a one-pixel checker in the bright bottom-right
/// quadrant (sharpening shows there). Each call gets its own cache key.
pub(crate) fn synth_raw(w: u32, h: u32) -> Loaded {
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let base = if x < w / 2 { 3_000 } else { 16_000 };
            let v = if x >= w / 2 && y >= h / 2 && (x + y) % 2 == 0 {
                base + 12_000
            } else {
                base
            };
            data.extend([v, v, v]);
        }
    }
    Loaded {
        key: ImageKey(NEXT_KEY.fetch_add(1, Ordering::Relaxed)),
        raw: Arc::new(RawImage {
            size: Size::new(w, h),
            cfa: CfaPattern::LinearRgb,
            data,
            black_level: [0.0; 4],
            white_level: 65_535.0,
            wb_as_shot: [1.0; 3],
            wb_daylight: [1.0; 3],
            // sRGB (D65) → XYZ: rows sum to the D65 white point.
            cam_to_xyz: [
                [0.412_4, 0.357_6, 0.180_5],
                [0.212_6, 0.715_2, 0.072_2],
                [0.019_3, 0.119_2, 0.950_5],
            ],
            orientation: Orientation::Normal,
            default_crop: None,
        }),
        capture: CaptureInfo::default(),
    }
}

/// A develop renderer whose RAWs all decode to `synth_raw(96, 64)` (no samples).
fn synth_renderer(
    sources: Vec<(u64, PathBuf, Option<String>)>,
    decoder: Option<Arc<dyn ImageEncoder>>,
) -> DevelopRenderer {
    let raw = synth_raw(96, 64);
    DevelopRenderer::with_loader(
        sources.into_iter().map(|(id, p, j)| (id, (p, j))).collect(),
        Arc::new(AtomicBool::new(false)),
        decoder,
        Arc::new(move |_: &Path| Ok(raw.clone())),
    )
}

fn rgb16(r: Rendered) -> (Size, Vec<u16>) {
    match r {
        Rendered::Rgb16 { size, data } => (size, data),
        Rendered::Rgba8 { .. } => panic!("expected 16-bit"),
    }
}

fn mean(data: &[u16]) -> f64 {
    data.iter().map(|&v| f64::from(v)).sum::<f64>() / data.len() as f64
}

fn item(id: u64) -> ExportItem {
    ExportItem {
        id,
        name: format!("S{id}"),
        seq: id,
        ..ExportItem::default()
    }
}

/// The develop renderer (synthetic RAW, real pipeline): edits apply, crop and
/// quarter turns shape the output, the recipe's size and output sharpening are
/// rendered by the pipeline, upscales and non-RAWs take the full-size path,
/// bad adjustments and cancel fail cleanly.
#[test]
#[allow(clippy::too_many_lines)]
fn develop_renderer_applies_edits_geometry_size_and_sharpening() {
    let t = tempfile::tempdir().unwrap();
    let raw = t.path().join("S.ARW");
    fs::write(&raw, b"synthetic").unwrap();
    let jpg = jpeg_original(t.path(), "J.JPG", 16, 8, false);
    let video = t.path().join("C.MP4");
    fs::write(&video, b"....ftyp").unwrap();
    let r = synth_renderer(
        vec![
            (1, raw.clone(), None),
            (
                2,
                raw.clone(),
                adjustments(|a| a.exposure.exposure_ev = 1.0),
            ),
            (
                3,
                raw.clone(),
                adjustments(|a| {
                    a.crop.rect = NormRect {
                        x: 0.0,
                        y: 0.0,
                        w: 0.5,
                        h: 1.0,
                    };
                    a.crop.quarter_turns = 1;
                }),
            ),
            (4, raw.clone(), Some("{not json".into())),
            (5, jpg, None),
            (6, video, None),
        ],
        None,
    );
    assert!(r.name().starts_with("develop ("), "{}", r.name());
    let out = t.path().join("Out");
    let full = recipe_of(&recipe("full", ExportFileFormat::Tiff16, &out)).unwrap();
    // Defaults: the whole frame at 1:1.
    let (s1, base) = rgb16(r.render_for(&item(1), &full).unwrap().unwrap());
    assert_eq!(s1, Size::new(96, 64));
    // +1 EV is brighter.
    let (s2, bright) = rgb16(r.render_for(&item(2), &full).unwrap().unwrap());
    assert_eq!(s2, s1);
    assert!(
        mean(&bright) > mean(&base) * 1.25,
        "{} vs {}",
        mean(&bright),
        mean(&base)
    );
    // Crop to the left (dark) half, then one clockwise turn: 64×48, all dark.
    let (s3, cropped) = rgb16(r.render_for(&item(3), &full).unwrap().unwrap());
    assert_eq!(s3, Size::new(64, 48));
    assert!(mean(&cropped) < mean(&base), "only the dark half is left");
    // The recipe's size, rendered by the pipeline.
    let mut small = full.clone();
    small.sizing = Sizing::LongEdge(48);
    let (s4, _) = rgb16(r.render_for(&item(1), &small).unwrap().unwrap());
    assert_eq!(s4.width.max(s4.height), 48);
    // Output sharpening raises local contrast in the checker.
    let mut sharp = full.clone();
    sharp.sharpen = 2.0;
    let (_, sharpened) = rgb16(r.render_for(&item(1), &sharp).unwrap().unwrap());
    let contrast = |d: &[u16]| {
        // Bright quadrant, horizontal neighbour differences.
        let mut sum = 0.0;
        for y in 40..60 {
            for x in 60..90 {
                let i = (y * 96 + x) * 3;
                sum += f64::from(d[i].abs_diff(d[i + 3]));
            }
        }
        sum
    };
    assert!(
        contrast(&sharpened) > contrast(&base) * 1.2,
        "{} vs {}",
        contrast(&sharpened),
        contrast(&base)
    );
    // Upscaling recipes and non-RAWs take the full-size path.
    let mut up = full.clone();
    up.sizing = Sizing::LongEdge(200);
    up.allow_upscale = true;
    assert!(r.render_for(&item(1), &up).is_none());
    assert_eq!(rgb16(r.render(&item(1), OutputSpace::Srgb).unwrap()).0, s1);
    assert!(r.render_for(&item(5), &full).is_none(), "JPEG original");
    assert_eq!(
        rgb16(r.render(&item(5), OutputSpace::Srgb).unwrap()).0,
        Size::new(16, 8)
    );
    assert!(
        r.render(&item(6), OutputSpace::Srgb)
            .unwrap_err()
            .contains("videos")
    );
    // Bad adjustments and unknown photos fail that photo.
    assert!(
        r.render_for(&item(4), &full)
            .unwrap()
            .unwrap_err()
            .contains("adjustments JSON")
    );
    assert!(
        r.render(&item(9), OutputSpace::Srgb)
            .unwrap_err()
            .contains("no file")
    );
    // Display P3 output differs from sRGB for colored content only; neutral stays.
    let mut p3 = full.clone();
    p3.space = OutputSpace::DisplayP3;
    let (_, p3px) = rgb16(r.render_for(&item(1), &p3).unwrap().unwrap());
    assert!(p3px.chunks(3).all(|c| c[0].abs_diff(c[1]) < 600));
    // Cancel: later renders fail fast with "cancelled".
    r.cancel_now();
    assert_eq!(
        r.render_for(&item(1), &full).unwrap().unwrap_err(),
        "cancelled"
    );
    assert_eq!(
        r.render(&item(1), OutputSpace::Srgb).unwrap_err(),
        "cancelled"
    );
}

/// An output-profile recipe develops 16-bit Rec.2020 even for an 8-bit format, and
/// raster originals are handed over in Rec.2020 too.
#[test]
fn output_profile_recipes_render_16_bit_rec2020() {
    let t = tempfile::tempdir().unwrap();
    let raw = t.path().join("S.ARW");
    fs::write(&raw, b"synthetic").unwrap();
    let jpg = jpeg_original(t.path(), "J.JPG", 16, 8, false);
    let r = synth_renderer(vec![(1, raw, None), (5, jpg, None)], None);
    let icc = t.path().join("srgb.icc");
    fs::write(&icc, ds_export::icc::profile(OutputSpace::Srgb)).unwrap();
    let mut spec = recipe("proofed", ExportFileFormat::Png8, t.path());
    spec.output_profile = Some(ExportOutputProfile {
        path: icc.to_string_lossy().into_owned(),
        intent: ProofRenderingIntent::Perceptual,
        black_point_compensation: true,
    });
    let proofed = recipe_of(&spec).unwrap();
    // 16-bit, not the 8-bit render a plain PNG8 recipe gets.
    let (size, wide) = rgb16(r.render_for(&item(1), &proofed).unwrap().unwrap());
    assert_eq!(size, Size::new(96, 64));
    let plain = recipe_of(&recipe("plain", ExportFileFormat::Png8, t.path())).unwrap();
    assert!(matches!(
        r.render_for(&item(1), &plain).unwrap().unwrap(),
        Rendered::Rgba8 { .. }
    ));
    // The synthetic frame is neutral: Rec.2020 and sRGB encode neutrals alike.
    let mut srgb = proofed.clone();
    srgb.space = OutputSpace::Srgb;
    let (_, narrow) = rgb16(r.render_for(&item(1), &srgb).unwrap().unwrap());
    assert!(wide.iter().zip(&narrow).all(|(a, b)| a.abs_diff(*b) < 400));
    // A JPEG original (no app decoder): converted into Rec.2020, neutrals kept.
    let (_, j_wide) = rgb16(r.render(&item(5), OutputSpace::Rec2020).unwrap());
    let (_, j_srgb) = rgb16(r.render(&item(5), OutputSpace::Srgb).unwrap());
    assert!(
        j_wide
            .iter()
            .zip(&j_srgb)
            .all(|(a, b)| a.abs_diff(*b) < 400)
    );
}

/// Left half white for photo 1, a malformed bitmap for photo 3, nothing otherwise.
struct LeftHalfSubject;

impl ExportMaskSource for LeftHalfSubject {
    fn ai_mask(&self, source_id: u64, target: DevelopAiTarget) -> Option<DevelopMask> {
        match (source_id, target) {
            (1, DevelopAiTarget::Subject) => Some(DevelopMask {
                width: 8,
                height: 4,
                data: (0..32).map(|i| if i % 8 < 4 { 255 } else { 0 }).collect(),
                version: 0,
            }),
            (3, _) => Some(DevelopMask {
                width: 8,
                height: 4,
                data: vec![255; 3],
                version: 0,
            }),
            _ => None,
        }
    }

    /// Person 1's hair is the right half of photo 1; nobody else has any.
    fn ai_person_mask(
        &self,
        source_id: u64,
        target: DevelopAiTarget,
        person: u32,
    ) -> Option<DevelopMask> {
        (source_id == 1 && target == DevelopAiTarget::Hair && person == 1).then(|| DevelopMask {
            width: 8,
            height: 4,
            data: (0..32).map(|i| if i % 8 >= 4 { 255 } else { 0 }).collect(),
            version: 0,
        })
    }

    /// Photo 1 is near on its left half and far on its right.
    fn depth_map(&self, source_id: u64) -> Option<DevelopMask> {
        (source_id == 1).then(|| DevelopMask {
            width: 8,
            height: 4,
            data: (0..32).map(|i| if i % 8 < 4 { 255 } else { 0 }).collect(),
            version: 3,
        })
    }
}

/// Med 9: a depth lens blur whose map can't be made fails that photo with a reason
/// (never an unblurred export); with the map, or without a depth blur, it exports.
#[test]
fn a_depth_blur_without_its_map_is_not_exported() {
    let t = tempfile::tempdir().unwrap();
    let raw = t.path().join("S.ARW");
    fs::write(&raw, b"synthetic").unwrap();
    let blur = adjustments(|a| a.effects.lens_blur.amount = 60.0);
    let mut plane = Adjustments::default();
    plane.effects.lens_blur.amount = 60.0;
    plane.effects.lens_blur.mode = ds_core::effects::BlurMode::Plane;
    let plane = Some(serde_json::to_string(&plane).unwrap());
    let sources = || {
        vec![
            (1, raw.clone(), blur.clone()),
            (2, raw.clone(), blur.clone()),
            (3, raw.clone(), plane.clone()),
        ]
    };
    let full = recipe_of(&recipe("f", ExportFileFormat::Tiff16, t.path())).unwrap();
    let r = synth_renderer(sources(), None).with_masks(Some(Arc::new(LeftHalfSubject)));
    assert!(
        r.render_for(&item(1), &full).unwrap().is_ok(),
        "photo 1 has its map"
    );
    let err = r.render_for(&item(2), &full).unwrap().unwrap_err();
    assert!(err.contains("depth map"), "{err}");
    assert!(
        r.render(&item(2), OutputSpace::Srgb)
            .unwrap_err()
            .contains("depth map")
    );
    assert!(
        r.render_for(&item(3), &full).unwrap().is_ok(),
        "tilt-shift needs no map"
    );
    let bare = synth_renderer(sources(), None);
    assert!(
        bare.render_for(&item(1), &full).unwrap().is_err(),
        "no mask source: no map"
    );
    // Through the batch: photo 2 is a per-photo failure with the reason; 1 and 3 write.
    let rep = run(
        &items(3),
        &[full],
        &r,
        None,
        false,
        &Dispatcher::new(),
        &|| false,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.written.len(), 2, "{:?}", rep.failed);
    assert_eq!(rep.failed.len(), 1, "{:?}", rep.failed);
    assert_eq!(rep.failed[0].source_id, Some(2));
    assert!(
        rep.failed[0].message.contains("depth map"),
        "{:?}",
        rep.failed
    );
}

/// A one-person people mask reaches the export render through the photo's source: the
/// layer applies where that person's mask is, and another person's index is empty.
#[test]
fn person_masks_reach_export_renders() {
    use ds_core::adjust::{Layer, Mask, MaskOp, MaskShape};
    let t = tempfile::tempdir().unwrap();
    let raw = t.path().join("S.ARW");
    fs::write(&raw, b"synthetic").unwrap();
    let hair = |person| {
        adjustments(move |a| {
            a.layers.push(Layer {
                adjustments: ds_core::adjust::LayerAdjustments {
                    exposure: ds_core::adjust::Exposure {
                        exposure_ev: -2.0,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                mask: Mask {
                    shapes: vec![MaskShape::Ai {
                        op: MaskOp::Add,
                        target: AiTarget::Hair,
                        feather: 0.0,
                        person,
                    }],
                    ..Mask::default()
                },
                ..Layer::default()
            });
        })
    };
    let r = synth_renderer(
        vec![
            (1, raw.clone(), hair(Some(1))),
            (2, raw.clone(), hair(Some(0))),
            (3, raw.clone(), None),
        ],
        None,
    )
    .with_masks(Some(Arc::new(LeftHalfSubject)));
    let full = recipe_of(&recipe("f", ExportFileFormat::Tiff16, t.path())).unwrap();
    let px = |id| rgb16(r.render_for(&item(id), &full).unwrap().unwrap()).1;
    let (one, other, base) = (px(1), px(2), px(3));
    let band = |d: &[u16], x0: usize, x1: usize| {
        let mut s = 0.0;
        for y in 0..64 {
            for x in x0..x1 {
                s += f64::from(d[(y * 96 + x) * 3 + 1]);
            }
        }
        s / (64.0 * (x1 - x0) as f64)
    };
    assert!(
        band(&one, 56, 92) < band(&base, 56, 92) * 0.6,
        "right half darkened (the synthetic frame's bright half: lifting it would clip)"
    );
    assert!((band(&one, 4, 40) - band(&base, 4, 40)).abs() < band(&base, 4, 40) * 0.05);
    assert_eq!(other, base, "person 0 has no hair mask: the layer is empty");
}

/// AI-mask layers export with the app's masks (the viewer's `DevelopMask`s, per
/// photo): the layer applies where the mask is, and a missing or malformed mask is an
/// empty shape (as in the viewer).
#[test]
fn ai_masks_reach_export_renders() {
    use ds_core::adjust::{Layer, Mask, MaskOp, MaskShape};
    let t = tempfile::tempdir().unwrap();
    let raw = t.path().join("S.ARW");
    fs::write(&raw, b"synthetic").unwrap();
    let layered = adjustments(|a| {
        a.layers.push(Layer {
            adjustments: ds_core::adjust::LayerAdjustments {
                exposure: ds_core::adjust::Exposure {
                    exposure_ev: 2.0,
                    ..Default::default()
                },
                ..Default::default()
            },
            mask: Mask {
                shapes: vec![MaskShape::Ai {
                    op: MaskOp::Add,
                    target: AiTarget::Subject,
                    feather: 0.0,
                    person: None,
                }],
                ..Mask::default()
            },
            ..Layer::default()
        });
    });
    let r = synth_renderer(
        vec![
            (1, raw.clone(), layered.clone()),
            (2, raw.clone(), layered.clone()),
            (3, raw.clone(), layered),
            (4, raw.clone(), None),
        ],
        None,
    )
    .with_masks(Some(Arc::new(LeftHalfSubject)));
    let full = recipe_of(&recipe("f", ExportFileFormat::Tiff16, t.path())).unwrap();
    let px = |id| rgb16(r.render_for(&item(id), &full).unwrap().unwrap()).1;
    let (masked, unmasked, malformed, base) = (px(1), px(2), px(3), px(4));
    // Mean of a column band (the synthetic frame is 96 wide).
    let band = |d: &[u16], x0: usize, x1: usize| {
        let mut s = 0.0;
        let mut n = 0.0;
        for y in 0..64 {
            for x in x0..x1 {
                s += f64::from(d[(y * 96 + x) * 3 + 1]);
                n += 1.0;
            }
        }
        s / n
    };
    assert!(
        band(&masked, 4, 40) > band(&base, 4, 40) * 1.5,
        "left (masked) brighter: {} vs {}",
        band(&masked, 4, 40),
        band(&base, 4, 40)
    );
    assert!(
        (band(&masked, 56, 92) - band(&base, 56, 92)).abs() < band(&base, 56, 92) * 0.02,
        "right (unmasked) unchanged"
    );
    assert_eq!(unmasked, base, "no mask: the AI shape is empty");
    assert_eq!(malformed, base, "malformed mask: ignored");
    // The full-size path takes the masks too.
    let (_, whole) = rgb16(r.render(&item(1), OutputSpace::Srgb).unwrap());
    assert_eq!(whole, masked);
}

/// The batch on the develop renderer: every recipe from the pipeline, HEIC
/// through the app's encoder, camera from the file's metadata.
#[test]
fn develop_batch_writes_every_recipe() {
    let t = tempfile::tempdir().unwrap();
    let raw = tiny_raw(t.path(), "S1.ARW", 16, 8, 1);
    let out = t.path().join("Output");
    let mut jpg = recipe("jpg", ExportFileFormat::Jpeg { quality: 95 }, &out);
    jpg.file_name_template = "{camera}_{name}_{seq:2}".into();
    let mut heic = recipe("heic", ExportFileFormat::Heic { quality: 80 }, &out);
    heic.space = ExportColorSpace::DisplayP3;
    heic.size = ExportSize::LongEdge { pixels: 48 };
    let recipes: Vec<ExportRecipe> = [jpg, heic].iter().map(|r| recipe_of(r).unwrap()).collect();
    let enc = Arc::new(FakeEncoder::default());
    let r = synth_renderer(vec![(3, raw.clone(), None)], Some(enc.clone()));
    let items = vec![item_of(&source(3, &raw))];
    let events = Dispatcher::new();
    let seen = Arc::new(Events::default());
    events.set_listener(Some(seen.clone()));
    let rep = run(
        &items,
        &recipes,
        &r,
        Some(enc.clone()),
        false,
        &events,
        &|| false,
        &AtomicBool::new(false),
    );
    events.flush(Duration::from_secs(5));
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(rep.renderer.starts_with("develop ("), "{}", rep.renderer);
    assert_eq!(listing(&out), ["ILCE-9_S1_03.jpg", "S1.heic"]);
    let w = rep.written.iter().find(|w| w.recipe == 0).unwrap();
    assert_eq!((w.width, w.height), (96, 64), "the whole develop at 1:1");
    let (size, _) = decode_jpeg_rgb(&fs::read(&w.path).unwrap()).unwrap();
    assert_eq!((size.width, size.height), (96, 64));
    let calls = enc.0.lock().unwrap().clone();
    assert_eq!(
        (calls[0].1, calls[0].2),
        (48, 32),
        "HEIC at its recipe's size"
    );
    assert_eq!(
        seen.0.lock().unwrap().last().unwrap().phase,
        TaskPhase::Finished
    );
}

fn listing(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn orientation_matches_exif_for_all_eight_values() {
    // 3×2 image, pixel value = its index: [0 1 2 / 3 4 5].
    let src: Vec<u8> = (0..6u8).flat_map(|i| [i, i, i]).collect();
    let got = |o| {
        let (s, px) = orient(Size::new(3, 2), &src, o);
        (s, px.chunks(3).map(|p| p[0]).collect::<Vec<_>>())
    };
    let want: [(u16, (u32, u32), [u8; 6]); 8] = [
        (1, (3, 2), [0, 1, 2, 3, 4, 5]),
        (2, (3, 2), [2, 1, 0, 5, 4, 3]),
        (3, (3, 2), [5, 4, 3, 2, 1, 0]),
        (4, (3, 2), [3, 4, 5, 0, 1, 2]),
        (5, (2, 3), [0, 3, 1, 4, 2, 5]),
        (6, (2, 3), [3, 0, 4, 1, 5, 2]), // 90° clockwise: bottom-left goes top-left
        (7, (2, 3), [5, 2, 4, 1, 3, 0]),
        (8, (2, 3), [2, 5, 1, 4, 0, 3]), // 90° counter-clockwise
    ];
    for (o, (w, h), px) in want {
        let (s, v) = got(o);
        assert_eq!((s.width, s.height), (w, h), "orientation {o}");
        assert_eq!(v, px, "orientation {o}");
    }
    assert_eq!(got(0).1, [0, 1, 2, 3, 4, 5], "unknown = as stored");
}

#[test]
fn display_p3_conversion_keeps_neutrals_and_desaturates_srgb_primaries() {
    let px = [255u8, 255, 255, 0, 0, 0, 255, 0, 0, 128, 128, 128];
    assert_eq!(
        to_space(&px, OutputSpace::Srgb),
        px.iter().map(|&v| u16::from(v) * 257).collect::<Vec<_>>()
    );
    let p3 = to_space(&px, OutputSpace::DisplayP3);
    assert!(p3[..3].iter().all(|&v| v >= 65_530), "white {:?}", &p3[..3]);
    assert_eq!(&p3[3..6], &[0, 0, 0]);
    // sRGB red is inside P3: less than full red, some green and blue.
    assert!(
        p3[6] < 62_000 && p3[7] > 10_000 && p3[8] > 5_000,
        "{:?}",
        &p3[6..9]
    );
    let g = &p3[9..12];
    assert!(
        g.iter().all(|&v| v.abs_diff(g[0]) < 40),
        "grey stays grey {g:?}"
    );
}

#[test]
fn a_jpeg_claiming_an_enormous_size_is_refused_before_decoding() {
    let mut jpeg = Vec::new();
    {
        let mut c = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
        c.set_size(16, 16);
        let mut c = c.start_compress(&mut jpeg).unwrap();
        c.write_scanlines(&[128u8; 16 * 16 * 3]).unwrap();
        c.finish().unwrap();
    }
    assert!(decode_jpeg_rgb(&jpeg).is_ok());
    // Patch the frame header (SOF: marker, length, precision, height, width)
    // to 60000 x 60000: within libjpeg's 65500 limit, so our check refuses it.
    let sof = jpeg
        .windows(2)
        .position(|w| w == [0xFF, 0xC0] || w == [0xFF, 0xC2])
        .expect("a start-of-frame marker");
    jpeg[sof + 5..sof + 9].copy_from_slice(&[0xEA, 0x60, 0xEA, 0x60]);
    let e = decode_jpeg_rgb(&jpeg).unwrap_err();
    assert!(e.contains("too large"), "{e}");
}

#[test]
fn undecodable_files_fail_cleanly() {
    assert!(decode_jpeg_rgb(b"not a jpeg").is_err());
    assert!(decode_jpeg_rgb(&[0xFF, 0xD8, 0xFF]).is_err());
    let d = tempfile::tempdir().unwrap();
    let bad = d.path().join("x.ARW");
    fs::write(&bad, b"nope").unwrap();
    // The real loader: not a RAW.
    let r = DevelopRenderer::new(
        HashMap::from([(9, (bad, None))]),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    assert!(r.render(&item(9), OutputSpace::Srgb).is_err());
    let full = recipe_of(&recipe("f", ExportFileFormat::Png8, d.path())).unwrap();
    assert!(r.render_for(&item(9), &full).unwrap().is_err());
    let r = DevelopRenderer::new(HashMap::new(), Arc::new(AtomicBool::new(true)), None);
    assert_eq!(
        r.render(&item(9), OutputSpace::Srgb).unwrap_err(),
        "cancelled"
    );
}

/// A Fujifilm X-Trans RAF reduced to what ds-raw reads: the header, an EXIF
/// JPEG (the embedded preview), the record list (raw size, X-Trans layout).
fn xtrans_raf() -> Vec<u8> {
    let mut tiff = b"II*\0\x08\0\0\0\x01\0".to_vec();
    tiff.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]); // Orientation 1
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1];
    jpeg.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
    jpeg.extend_from_slice(b"Exif\0\0");
    jpeg.extend(tiff);
    jpeg.extend_from_slice(&[0xFF, 0xC0, 0, 11, 8, 0, 40, 0, 60, 1, 1, 0x11, 0]); // 60×40
    jpeg.extend_from_slice(&[0xFF, 0xDA, 0, 2, 0xFF, 0xD9]);
    let mut rec = 2u32.to_be_bytes().to_vec();
    rec.extend_from_slice(&[0x01, 0x00, 0, 4, 0, 40, 0, 60]);
    rec.extend_from_slice(&[0x01, 0x31, 0, 36]);
    rec.extend_from_slice(&[1; 36]);
    let data = [0u8; 64];
    let mut f = b"FUJIFILMCCD-RAW 0201FF159505X-T4".to_vec();
    f.resize(84, 0);
    let (j_at, r_at) = (108, 108 + jpeg.len());
    for v in [
        j_at,
        jpeg.len(),
        r_at,
        rec.len(),
        r_at + rec.len(),
        data.len(),
    ] {
        f.extend_from_slice(&(v as u32).to_be_bytes());
    }
    f.extend(jpeg);
    f.extend(rec);
    f.extend_from_slice(&data);
    f
}

/// A RAW Darkslide culls but can't develop yet (X-Trans): it has its embedded
/// preview, and export fails that photo with the reason, never "damaged".
#[test]
fn cull_only_raws_fail_export_with_the_reason() {
    let d = tempfile::tempdir().unwrap();
    let raf = d.path().join("x.RAF");
    fs::write(&raf, xtrans_raf()).unwrap();
    assert!(ds_raw::embedded_preview(&raf).is_ok());
    let err = ds_raw::decode(&raf).unwrap_err();
    assert!(matches!(DsError::from(err), DsError::Unsupported { .. }));
    let r = DevelopRenderer::new(
        HashMap::from([(9, (raf, None))]),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    let e = r.render(&item(9), OutputSpace::Srgb).unwrap_err();
    assert!(e.contains("X-Trans") && !e.contains("damaged"), "{e}");
}

#[test]
fn recipes_are_validated_before_running() {
    let d = tempfile::tempdir().unwrap();
    let ok = recipe("ok", ExportFileFormat::Jpeg { quality: 90 }, d.path());
    assert_eq!(validate_export_recipes(vec![ok.clone()]).unwrap(), None);
    let mut rel = ok.clone();
    rel.destination = "relative/out".into();
    let mut tpl = ok.clone();
    tpl.file_name_template = "{nope}".into();
    let mut q = ok.clone();
    q.format = ExportFileFormat::Heic { quality: 0 };
    let mut sz = ok.clone();
    sz.size = ExportSize::LongEdge { pixels: 0 };
    for bad in [rel, tpl, q, sz] {
        let msg = validate_export_recipes(vec![ok.clone(), bad])
            .unwrap()
            .unwrap();
        assert!(msg.starts_with("invalid argument"), "{msg}");
    }
    let all_sizes = [
        ExportSize::ShortEdge { pixels: 100 },
        ExportSize::Megapixels { megapixels: 2.0 },
        ExportSize::Percent { percent: 50.0 },
    ];
    for (s, f) in all_sizes.into_iter().zip([
        ExportFileFormat::Tiff16,
        ExportFileFormat::Png8,
        ExportFileFormat::Png16,
    ]) {
        let mut r = ok.clone();
        r.size = s;
        r.format = f;
        r.space = ExportColorSpace::DisplayP3;
        assert!(recipe_of(&r).is_ok());
    }
    // Relative destination is refused by the async entry too, before any work.
    let mut rel = ok.clone();
    rel.destination = "out".into();
    let err = block_on(export_batch(
        vec![],
        vec![rel],
        None,
        false,
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap_err();
    assert!(matches!(err, DsError::InvalidArgument { .. }));
}

#[test]
fn output_profiles_are_read_and_force_a_rec2020_render() {
    let d = tempfile::tempdir().unwrap();
    let icc = d.path().join("srgb.icc");
    fs::write(&icc, ds_export::icc::profile(OutputSpace::Srgb)).unwrap();
    let mut r = recipe("proofed", ExportFileFormat::Png8, d.path());
    r.output_profile = Some(ExportOutputProfile {
        path: icc.to_string_lossy().into_owned(),
        intent: ProofRenderingIntent::RelativeColorimetric,
        black_point_compensation: true,
    });
    let out = recipe_of(&r).unwrap();
    assert_eq!(out.space, OutputSpace::Rec2020);
    let p = out.output_profile.unwrap();
    assert_eq!(p.profile.name(), "sRGB (built-in)");
    assert!(p.settings.black_point_compensation);
    // A missing or non-RGB profile names the recipe and the reason.
    let mut missing = r.clone();
    missing.output_profile.as_mut().unwrap().path =
        d.path().join("gone.icc").to_string_lossy().into_owned();
    let msg = validate_export_recipes(vec![missing]).unwrap().unwrap();
    assert!(msg.contains("proofed:"), "{msg}");
}

#[test]
fn file_name_preview_renders_the_template() {
    assert_eq!(
        export_file_name_preview(
            "{name}_{seq:3}".into(),
            "DSC0001".into(),
            7,
            None,
            None,
            "jpg".into()
        )
        .unwrap(),
        "DSC0001_007.jpg"
    );
    assert_eq!(
        export_file_name_preview(
            "{camera}-{name}".into(),
            "a".into(),
            1,
            Some("ILCE-9".into()),
            None,
            "heic".into()
        )
        .unwrap(),
        "ILCE-9-a.heic"
    );
    assert!(
        export_file_name_preview("{bad".into(), "a".into(), 1, None, None, "jpg".into()).is_err()
    );
    // E-L2: the photo's real capture time fills {date}.
    assert_eq!(
        export_file_name_preview(
            "{date}_{seq:3}".into(),
            "a".into(),
            1,
            None,
            Some("2026-08-08T17:25:40".into()),
            "jpg".into()
        )
        .unwrap(),
        "20260808_001.jpg"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn multiple_recipes_encoders_progress_and_confinement() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("Output");
    let sub = t.path().join("Output/web");
    let mut recipes = [
        recipe("full", ExportFileFormat::Jpeg { quality: 90 }, &out),
        recipe("heic", ExportFileFormat::Heic { quality: 80 }, &out),
        recipe("tiff", ExportFileFormat::Tiff16, &out),
        recipe("web", ExportFileFormat::Jpeg { quality: 70 }, &sub),
    ];
    recipes[3].size = ExportSize::LongEdge { pixels: 20 };
    recipes[3].file_name_template = "web_{seq:2}".into();
    let recipes: Vec<ExportRecipe> = recipes.iter().map(|r| recipe_of(r).unwrap()).collect();
    let enc = Arc::new(FakeEncoder::default());
    let events = Dispatcher::new();
    let seen = Arc::new(Events::default());
    events.set_listener(Some(seen.clone()));
    let r = Gradient {
        size: Size::new(40, 30),
        delay: Duration::ZERO,
    };
    let rep = run(
        &items(2),
        &recipes,
        &r,
        Some(enc.clone()),
        false,
        &events,
        &|| false,
        &AtomicBool::new(false),
    );
    events.flush(Duration::from_secs(5));
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(rep.written.len(), 8);
    assert_eq!(rep.jpeg_encoder, "mozjpeg");
    assert_eq!(rep.renderer, "gradient");
    assert_eq!(
        listing(&out),
        [
            "IMG1.heic",
            "IMG1.jpg",
            "IMG1.tif",
            "IMG2.heic",
            "IMG2.jpg",
            "IMG2.tif",
            "web"
        ]
    );
    assert_eq!(listing(&sub), ["web_01.jpg", "web_02.jpg"]);
    let web = rep.written.iter().find(|w| w.recipe == 3).unwrap();
    assert_eq!((web.width, web.height), (20, 15));
    // mozjpeg wrote a real JPEG; HEIC went to the app's encoder (16-bit RGB bytes, EXIF blob).
    assert_eq!(&fs::read(out.join("IMG1.jpg")).unwrap()[..2], &[0xFF, 0xD8]);
    assert_eq!(fs::read(out.join("IMG1.heic")).unwrap(), b"FAKE");
    let calls = enc.0.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert!(
        calls.iter().all(|c| c.0 == EncodedImageFormat::Heic
            && (c.1, c.2, c.3, c.4) == (40, 30, 40 * 30 * 6, true))
    );
    // Nothing outside the destinations; no temp files left.
    assert_eq!(listing(t.path()), ["Output"]);
    let ev = seen.0.lock().unwrap().clone();
    assert_eq!(ev.first().unwrap().phase, TaskPhase::Started);
    assert_eq!(ev.last().unwrap().phase, TaskPhase::Finished);
    assert!(
        ev.iter()
            .any(|e| e.phase == TaskPhase::Running && e.completed == 2 && e.total == 2)
    );
    assert!(ev.iter().all(|e| e.kind == "export"));

    // Hardware JPEG: JPEG goes to the encoder too; its errors are per output.
    let hw = tempfile::tempdir().unwrap();
    let mut specs = [
        recipe("hw", ExportFileFormat::Jpeg { quality: 90 }, hw.path()),
        recipe("refused", ExportFileFormat::Jpeg { quality: 13 }, hw.path()),
    ];
    specs[1].file_name_template = "{name}-refused".into();
    let recipes: Vec<ExportRecipe> = specs.iter().map(|r| recipe_of(r).unwrap()).collect();
    let enc = Arc::new(FakeEncoder::default());
    let rep = run(
        &items(1),
        &recipes,
        &r,
        Some(enc.clone()),
        true,
        &Dispatcher::new(),
        &|| false,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.jpeg_encoder, "imageio");
    assert_eq!(rep.written.len(), 1);
    assert_eq!(rep.failed.len(), 1);
    assert!(
        rep.failed[0].message.contains("encoder refused"),
        "{:?}",
        rep.failed
    );
    assert_eq!(enc.0.lock().unwrap()[0].0, EncodedImageFormat::Jpeg);
    assert_eq!(listing(hw.path()), ["IMG1.jpg"]);

    // HEIC without the app's encoder: a per-output failure, nothing written.
    let none = tempfile::tempdir().unwrap();
    let recipes = vec![
        recipe_of(&recipe(
            "h",
            ExportFileFormat::Heic { quality: 80 },
            none.path(),
        ))
        .unwrap(),
    ];
    let rep = run(
        &items(1),
        &recipes,
        &r,
        None,
        true,
        &Dispatcher::new(),
        &|| false,
        &AtomicBool::new(false),
    );
    assert_eq!(rep.jpeg_encoder, "mozjpeg", "no encoder, no hardware JPEG");
    assert!(rep.failed[0].message.contains("HEIC"));
    assert_eq!(listing(none.path()).len(), 0);
}

#[test]
fn cancel_stops_between_photos() {
    let t = tempfile::tempdir().unwrap();
    let recipes = vec![recipe_of(&recipe("j", ExportFileFormat::Png8, t.path())).unwrap()];
    let r = Gradient {
        size: Size::new(8, 8),
        delay: Duration::from_millis(30),
    };
    let start = std::time::Instant::now();
    let events = Dispatcher::new();
    let seen = Arc::new(Events::default());
    events.set_listener(Some(seen.clone()));
    let rep = run(
        &items(200),
        &recipes,
        &r,
        None,
        false,
        &events,
        &|| start.elapsed() > Duration::from_millis(100),
        &AtomicBool::new(false),
    );
    events.flush(Duration::from_secs(5));
    assert!(rep.cancelled);
    assert!(rep.written.len() < 50, "{}", rep.written.len());
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(
        listing(t.path()).len(),
        rep.written.len(),
        "no temp files left"
    );
    // E-L5: the last event of a cancelled run is "Cancelled" at the count
    // actually done, never N of N.
    let ev = seen.0.lock().unwrap().clone();
    let last = ev.last().unwrap();
    assert_eq!(last.phase, TaskPhase::Cancelled);
    assert!(last.completed < last.total, "{last:?}");
    assert!(
        !ev.iter().any(|e| e.completed == e.total && e.total == 200),
        "never reports 200 of 200"
    );
}

/// Full-size develop and mozjpeg on a real sample (the Swift side times `ImageIO`
/// JPEG and HEIC on the same develops): `cargo test --release -p ds-ffi
/// mozjpeg_encode_cost -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn mozjpeg_encode_cost() {
    let Some(path) = ds_testdata::sample_arw() else {
        return;
    };
    let r = DevelopRenderer::new(
        HashMap::from([(1, (path, None))]),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    let full = recipe_of(&recipe(
        "f",
        ExportFileFormat::Tiff16,
        &std::env::temp_dir(),
    ))
    .unwrap();
    let _ = r.render_for(&item(1), &full).unwrap().unwrap(); // decode + warm
    let t = std::time::Instant::now();
    let (size, data) = rgb16(r.render_for(&item(1), &full).unwrap().unwrap());
    println!(
        "EXPORTBENCH full develop {}x{} (Quality::Full, Rgb16): {:.0} ms",
        size.width,
        size.height,
        t.elapsed().as_secs_f64() * 1000.0
    );
    let img = Image16::new(size, data).unwrap();
    let icc = ds_export::icc::profile(OutputSpace::Srgb);
    for q in [80u8, 90] {
        let fmt = ExportFormat::Jpeg { quality: q };
        let t = std::time::Instant::now();
        let bytes = ds_export::encode(&img, fmt, icc, None, None).unwrap().len();
        println!(
            "EXPORTBENCH encoder only mozjpeg {}x{} q{q}: {:.0} ms ({} KB)",
            size.width,
            size.height,
            t.elapsed().as_secs_f64() * 1000.0,
            bytes / 1024
        );
    }
}

/// Real FFI path on copies of the sample RAWs through the develop pipeline:
/// full-size output (6000×4000 upright), recipe sizes, edits (+1 EV) and crops
/// visible in the files, EXIF include / strip GPS, template, confinement,
/// cancel by token.
#[test]
#[allow(clippy::too_many_lines)]
fn exports_real_samples_through_the_develop_pipeline() {
    let samples = ds_testdata::samples_with_ext("ARW");
    let [a, b, ..] = samples.as_slice() else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("src");
    fs::create_dir(&src).unwrap();
    let (pa, pb) = (src.join("A.ARW"), src.join("B.ARW"));
    fs::copy(a, &pa).unwrap();
    fs::copy(b, &pb).unwrap();
    // The sensor frame, upright (6000×4000 on the A9).
    let upright = |p: &Path| {
        let m = ds_raw::read_metadata(p).unwrap();
        m.orientation.apply_to(Size::new(6000, 4000))
    };
    // The inscribed size of a full develop with default adjustments: the lens
    // profile's distortion correction (on by default) auto-fits the frame inside the
    // sensor. "Constrain to sensor pixels" exports that; the default fills it back up
    // to the sensor size.
    let inscribed = |p: &Path| {
        let l = crate::develop::develop().load(p).unwrap();
        let req = RenderRequest {
            capture: l.capture.clone(),
            quality: Quality::Full,
            fit: Fit::ConstrainToSensorPixels,
            ..RenderRequest::new(Size::new(1 << 16, 1 << 16))
        };
        ds_pipeline::output_size(&l.raw, &Adjustments::default(), &req).unwrap()
    };
    let out = t.path().join("Output");
    let mut keep = recipe("keep", ExportFileFormat::Jpeg { quality: 90 }, &out);
    keep.file_name_template = "{camera}_{name}_{seq:3}".into();
    let mut strip = recipe("strip", ExportFileFormat::Jpeg { quality: 80 }, &out);
    strip.file_name_template = "{name}_nogps".into();
    strip.strip_gps = true;
    strip.size = ExportSize::LongEdge { pixels: 800 };
    strip.sharpen = 0.6;
    let mut none = recipe("none", ExportFileFormat::Png8, &out);
    none.include_exif = false;
    none.space = ExportColorSpace::DisplayP3;
    none.size = ExportSize::LongEdge { pixels: 400 };
    let mut sa = source(1, &pa);
    sa.gps = Some(ExportGps {
        latitude: 20.8,
        longitude: -156.3,
        altitude_m: Some(12.0),
    });
    sa.artist = Some("Alexa".into());
    let started = std::time::Instant::now();
    let rep = block_on(export_batch(
        vec![sa, source(2, &pb)],
        vec![keep, strip, none],
        None,
        true, // no encoder given: falls back to mozjpeg
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap();
    println!(
        "EXPORTBENCH real samples, 2 photos x (full JPEG + 800 JPEG + 400 PNG): {:.2} s ({})",
        started.elapsed().as_secs_f64(),
        rep.renderer
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(rep.renderer.starts_with("develop (") && rep.jpeg_encoder == "mozjpeg");
    assert_eq!(rep.written.len(), 6);
    assert_eq!(
        listing(t.path()),
        ["Output", "src"],
        "nothing outside Output/"
    );
    assert_eq!(listing(&src), ["A.ARW", "B.ARW"]);
    for w in &rep.written {
        let bytes = fs::read(&w.path).unwrap();
        assert_eq!(bytes.len() as u64, w.bytes);
        let src = if w.source_id == 1 { &pa } else { &pb };
        let up = upright(src);
        match w.recipe {
            0 => assert_eq!(
                (w.width, w.height),
                (up.width, up.height),
                "full develop: the sensor size"
            ),
            1 => {
                assert_eq!(w.width.max(w.height), 800);
                assert_eq!(w.width > w.height, up.width > up.height, "orientation kept");
            }
            _ => assert_eq!(w.width.max(w.height), 400),
        }
        let exif = exif::Reader::new().read_from_container(&mut Cursor::new(&bytes));
        match w.recipe {
            0 => {
                let e = exif.unwrap();
                let model = e.get_field(exif::Tag::Model, exif::In::PRIMARY).unwrap();
                assert!(model.display_value().to_string().contains("ILCE-9"));
                let gps = e.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY);
                assert_eq!(
                    gps.is_some(),
                    w.source_id == 1,
                    "GPS only where the photo has one"
                );
                assert!(w.path.contains("ILCE-9_"), "{}", w.path);
                let (size, _) = decode_jpeg_rgb(&bytes).unwrap();
                assert_eq!((size.width, size.height), (up.width, up.height));
            }
            1 => {
                let e = exif.unwrap();
                assert!(
                    e.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY)
                        .is_none(),
                    "stripped"
                );
                assert!(
                    e.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
                        .is_some()
                );
                assert!(w.path.ends_with("_nogps.jpg"));
            }
            _ => assert!(
                exif.is_err() || exif.unwrap().fields().next().is_none(),
                "no EXIF"
            ),
        }
    }
    assert!(
        rep.written
            .iter()
            .any(|w| w.path.ends_with("/ILCE-9_A_001.jpg"))
    );
    assert!(rep.written.iter().any(|w| w.path.ends_with("/B_nogps.jpg")));

    // Edits show in the file: +1 EV is brighter; a centred half crop is half the size.
    let edits = t.path().join("Edits");
    let mut web = recipe("web", ExportFileFormat::Jpeg { quality: 90 }, &edits);
    web.size = ExportSize::LongEdge { pixels: 600 };
    web.file_name_template = "{seq}".into();
    let mut full = recipe("crop", ExportFileFormat::Jpeg { quality: 90 }, &edits);
    full.file_name_template = "{seq}_full".into();
    let mut constrained = recipe("constrained", ExportFileFormat::Png8, &edits);
    constrained.file_name_template = "{seq}_constrained".into();
    constrained.constrain_to_sensor_pixels = true;
    let mut plain = source(1, &pa);
    plain.seq = 1;
    let mut ev = source(2, &pa);
    ev.seq = 2;
    ev.adjustments_json = adjustments(|a| a.exposure.exposure_ev = 1.0);
    let mut straight = source(4, &pa);
    straight.seq = 4;
    straight.adjustments_json = adjustments(|a| a.crop.angle_deg = 3.0);
    let mut cropped = source(3, &pa);
    cropped.seq = 3;
    cropped.adjustments_json = adjustments(|a| {
        a.crop.rect = NormRect {
            x: 0.25,
            y: 0.25,
            w: 0.5,
            h: 0.5,
        };
    });
    let rep = block_on(export_batch(
        vec![plain, ev, cropped, straight],
        vec![web, full, constrained],
        None,
        false,
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap();
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let luma = |name: &str| {
        let (_, rgb) = decode_jpeg_rgb(&fs::read(edits.join(name)).unwrap()).unwrap();
        rgb.iter().map(|&v| f64::from(v)).sum::<f64>() / rgb.len() as f64
    };
    let (l0, l1) = (luma("1.jpg"), luma("2.jpg"));
    println!("EXPORTBENCH +1 EV mean: {l0:.1} -> {l1:.1}");
    assert!(l1 > l0 * 1.2, "+1 EV brighter: {l0} -> {l1}");
    let size_in = |id: u64, recipe: u32| {
        let w = rep
            .written
            .iter()
            .find(|w| w.source_id == id && w.recipe == recipe)
            .unwrap();
        (w.width, w.height)
    };
    let size_of = |id: u64| size_in(id, 1);
    let up = upright(&pa);
    let ins = inscribed(&pa);
    assert_eq!(size_of(1), (up.width, up.height), "lens-corrected, filled");
    assert_eq!(size_of(4), (up.width, up.height), "straightened, filled");
    assert_eq!(size_in(1, 2), (ins.width, ins.height), "constrained");
    assert!(ins.width < up.width && ins.height < up.height, "{ins:?}");
    let (sw, sh) = size_in(4, 2);
    assert!(
        sw < ins.width && sh < ins.height,
        "straightened, constrained: {sw}x{sh}"
    );
    println!(
        "EXPORTBENCH full develop {}x{} (filled), {}x{} (constrained to sensor pixels)",
        up.width, up.height, ins.width, ins.height
    );
    let (cw, ch) = size_of(3);
    assert!(
        cw.abs_diff(up.width / 2) <= 2 && ch.abs_diff(up.height / 2) <= 2,
        "crop: {cw}x{ch}"
    );

    // A token cancelled up front: nothing runs.
    let token = CancelToken::new();
    token.cancel();
    let r = block_on(export_batch(
        vec![source(1, &pa)],
        vec![recipe("x", ExportFileFormat::Png8, &t.path().join("never"))],
        None,
        false,
        None,
        token,
        None,
        None,
    ));
    assert!(matches!(r, Err(DsError::Cancelled)));
    assert!(!t.path().join("never").exists());
}

/// Cancel while a full-size develop is running stops the batch and leaves no
/// partial files.
#[test]
fn cancel_during_real_develops_leaves_no_partial_files() {
    let Some(sample) = ds_testdata::sample_arw() else {
        return;
    };
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("Output");
    // One file twelve times: one decode in the shared cache (other tests use it too).
    let sources: Vec<_> = (1..=12).map(|i| source(i, &sample)).collect();
    let n = sources.len();
    let token = CancelToken::new();
    let events = Arc::new(Events::default());
    let cancel_after_first = {
        let token = token.clone();
        let events = events.clone();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            while t0.elapsed() < Duration::from_secs(60) {
                let done = events
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e.kind == "export" && e.completed >= 1);
                if done {
                    token.cancel();
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let rep = block_on(export_batch(
        sources,
        vec![recipe("t", ExportFileFormat::Tiff16, &out)],
        None,
        false,
        Some(events.clone()),
        token,
        None,
        None,
    ))
    .unwrap();
    cancel_after_first.join().unwrap();
    assert!(rep.cancelled);
    assert!(rep.written.len() < n, "{} of {n}", rep.written.len());
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(listing(&out).len(), rep.written.len(), "no partial files");
}

/// A real JPEG "original" (`w`×`h`, left dark / right bright) whose EXIF block
/// has camera, capture time with UTC offset, and (optionally) a position.
fn jpeg_original(dir: &Path, name: &str, w: u32, h: u32, gps: bool) -> PathBuf {
    let n = (w * h * 3) as usize;
    let data = (0..n)
        .map(|i| {
            if (i / 3) as u32 % w < w / 2 {
                8_000
            } else {
                60_000
            }
        })
        .collect();
    let img = Image16::new(Size::new(w, h), data).unwrap();
    let meta = ds_export::ExifMeta {
        make: Some("Apple".into()),
        model: Some("iPhone 17 Pro".into()),
        capture_time: chrono::NaiveDate::from_ymd_opt(2026, 8, 9)
            .unwrap()
            .and_hms_opt(7, 30, 5),
        offset_original: Some(-600),
        iso: Some(80),
        gps: gps.then_some(Gps {
            latitude: 20.75,
            longitude: -156.5,
            altitude_m: Some(3.0),
        }),
        ..ds_export::ExifMeta::default()
    };
    let bytes = ds_export::encode(
        &img,
        ExportFormat::Jpeg { quality: 95 },
        ds_export::icc::profile(OutputSpace::Srgb),
        Some(&meta.dirs(false)),
        None,
    )
    .unwrap();
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn exif_of(path: &str) -> Option<exif::Exif> {
    exif::Reader::new()
        .read_from_container(&mut Cursor::new(fs::read(path).unwrap()))
        .ok()
}

fn ascii_of(e: &exif::Exif, t: exif::Tag) -> Option<String> {
    match &e.get_field(t, exif::In::PRIMARY)?.value {
        exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

/// E-M2: JPEG/HEIF originals are decoded at full size (the app's decoder, or
/// mozjpeg for JPEG without one) and keep their EXIF, GPS and UTC offset
/// (E-L1, E-L6); videos and undecodable files fail per photo with a clear
/// message.
#[test]
#[allow(clippy::too_many_lines)]
fn raster_originals_export_at_full_size_and_videos_are_refused() {
    let t = tempfile::tempdir().unwrap();
    let jpg = jpeg_original(t.path(), "IMG_0001.JPG", 64, 32, true);
    let heic = t.path().join("IMG_0002.HEIC");
    fs::write(&heic, b"not decodable without the app").unwrap();
    let video = t.path().join("C0001.MP4");
    fs::write(&video, b"....ftyp").unwrap();
    let out = t.path().join("Output");
    let mut keep = recipe("keep", ExportFileFormat::Jpeg { quality: 90 }, &out);
    keep.file_name_template = "{date}_{camera}_{name}".into();
    let mut strip = recipe("strip", ExportFileFormat::Png8, &out);
    strip.strip_gps = true;
    // No app decoder: JPEG through mozjpeg at its full 64×32.
    let rep = block_on(export_batch(
        vec![source(1, &jpg), source(2, &heic), source(3, &video)],
        vec![keep.clone(), strip.clone()],
        None,
        false,
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap();
    let written: Vec<_> = rep.written.iter().filter(|w| w.source_id == 1).collect();
    assert_eq!(written.len(), 2, "{:?}", rep.failed);
    for w in &written {
        assert_eq!((w.width, w.height), (64, 32), "full size");
    }
    let kept = written.iter().find(|w| w.recipe == 0).unwrap();
    assert!(
        kept.path.ends_with("20260809_iPhone 17 Pro_IMG_0001.jpg"),
        "{}",
        kept.path
    );
    let e = exif_of(&kept.path).unwrap();
    assert_eq!(
        ascii_of(&e, exif::Tag::Model).as_deref(),
        Some("iPhone 17 Pro")
    );
    assert_eq!(
        ascii_of(&e, exif::Tag::OffsetTimeOriginal).as_deref(),
        Some("-10:00")
    );
    assert!(
        e.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY)
            .is_some(),
        "the original's GPS is exported"
    );
    let stripped = written.iter().find(|w| w.recipe == 1).unwrap();
    let e = exif_of(&stripped.path).unwrap();
    assert!(
        e.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY)
            .is_none(),
        "Remove location strips it"
    );
    let msg = |id| {
        rep.failed
            .iter()
            .filter(|f| f.source_id == Some(id))
            .map(|f| f.message.clone())
            .collect::<Vec<_>>()
            .join("; ")
    };
    assert!(msg(2).contains("needs the app's decoder"), "{}", msg(2));
    assert!(msg(3).contains("videos aren't exported"), "{}", msg(3));

    // With the app's decoder: its pixels (4×2 here), for JPEG and HEIC alike;
    // decoder errors and short buffers fail that photo only.
    let bad = t.path().join("bad.HEIC");
    let short = t.path().join("short.PNG");
    fs::write(&bad, b"x").unwrap();
    fs::write(&short, b"x").unwrap();
    let out2 = t.path().join("Out2");
    keep.destination = out2.to_string_lossy().into_owned();
    keep.file_name_template = "{name}".into();
    let enc = Arc::new(FakeEncoder::default());
    let rep = block_on(export_batch(
        vec![
            source(1, &jpg),
            source(2, &heic),
            source(3, &bad),
            source(4, &short),
        ],
        vec![keep],
        Some(enc),
        false,
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap();
    let mut sizes: Vec<_> = rep
        .written
        .iter()
        .map(|w| (w.source_id, w.width, w.height))
        .collect();
    sizes.sort_unstable();
    assert_eq!(sizes, [(1, 4, 2), (2, 4, 2)]);
    assert_eq!(rep.failed.len(), 2, "{:?}", rep.failed);
    assert!(rep.failed.iter().any(|f| f.message.contains("undecodable")));
    assert!(rep.failed.iter().any(|f| f.message.contains("short image")));
}

/// Little-endian IFD bytes placed at `at`; out-of-line values follow the table.
fn le_ifd(at: u32, entries: &[(u16, u16, u32, Vec<u8>)]) -> Vec<u8> {
    let table_len = 2 + entries.len() as u32 * 12 + 4;
    let mut f = Vec::new();
    let mut extra: Vec<u8> = Vec::new();
    f.extend((entries.len() as u16).to_le_bytes());
    for (tag, ty, count, bytes) in entries {
        f.extend(tag.to_le_bytes());
        f.extend(ty.to_le_bytes());
        f.extend(count.to_le_bytes());
        if bytes.len() <= 4 {
            let mut v = bytes.clone();
            v.resize(4, 0);
            f.extend(v);
        } else {
            f.extend((at + table_len + extra.len() as u32).to_le_bytes());
            extra.extend(bytes);
        }
    }
    f.extend(0u32.to_le_bytes());
    f.extend(extra);
    f
}

/// `tiny_raw` plus a GPS IFD (20°45'S 156°30'E): a new IFD0 with a `GPSInfo`
/// pointer is appended and the header re-pointed at it.
pub(crate) fn tiny_raw_with_gps(dir: &Path, name: &str) -> PathBuf {
    let mut f = fs::read(tiny_raw(dir, "base.ARW", 32, 24, 1)).unwrap();
    let dms = |d: u32, m: u32| -> Vec<u8> {
        [(d, 1u32), (m, 1), (0, 1)]
            .iter()
            .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
            .collect()
    };
    let gps_at = f.len().next_multiple_of(4) as u32;
    f.resize(gps_at as usize, 0);
    f.extend(le_ifd(
        gps_at,
        &[
            (1, 2, 2, b"S\0".to_vec()),
            (2, 5, 3, dms(20, 45)),
            (3, 2, 2, b"E\0".to_vec()),
            (4, 5, 3, dms(156, 30)),
        ],
    ));
    let old_n = u16::from_le_bytes([f[8], f[9]]) as usize;
    let mut table: Vec<[u8; 12]> = (0..old_n)
        .map(|i| f[10 + i * 12..22 + i * 12].try_into().unwrap())
        .collect();
    let mut gps_entry = [0u8; 12];
    gps_entry[..2].copy_from_slice(&0x8825u16.to_le_bytes());
    gps_entry[2..4].copy_from_slice(&4u16.to_le_bytes());
    gps_entry[4..8].copy_from_slice(&1u32.to_le_bytes());
    gps_entry[8..].copy_from_slice(&gps_at.to_le_bytes());
    table.push(gps_entry);
    table.sort_by_key(|e| u16::from_le_bytes([e[0], e[1]]));
    let ifd0_at = f.len().next_multiple_of(4) as u32;
    f.resize(ifd0_at as usize, 0);
    f.extend((table.len() as u16).to_le_bytes());
    for e in &table {
        f.extend(e);
    }
    f.extend(0u32.to_le_bytes());
    f[4..8].copy_from_slice(&ifd0_at.to_le_bytes());
    let p = dir.join(name);
    fs::write(&p, f).unwrap();
    p
}

/// E-L1: a RAW's recorded position (ds-raw GPS IFD) is exported, and "Remove
/// location" strips it.
#[test]
fn raw_gps_is_exported_unless_stripped() {
    let t = tempfile::tempdir().unwrap();
    let raw = tiny_raw_with_gps(t.path(), "G.ARW");
    let g = ds_raw::read_metadata(&raw)
        .unwrap()
        .gps
        .expect("gps parsed");
    assert!((g.latitude + 20.75).abs() < 1e-9 && (g.longitude - 156.5).abs() < 1e-9);
    let out = t.path().join("Output");
    let keep = recipe("keep", ExportFileFormat::Jpeg { quality: 90 }, &out);
    let mut strip = recipe("strip", ExportFileFormat::Jpeg { quality: 90 }, &out);
    strip.strip_gps = true;
    strip.file_name_template = "{name}-nogps".into();
    let recipes: Vec<ExportRecipe> = [keep, strip]
        .iter()
        .map(|r| recipe_of(r).unwrap())
        .collect();
    let rep = run(
        &[item_of(&source(1, &raw))],
        &recipes,
        &synth_renderer(vec![(1, raw.clone(), None)], None),
        None,
        false,
        &Dispatcher::new(),
        &|| false,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    for w in &rep.written {
        let e = exif_of(&w.path).unwrap();
        let has = e
            .get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY)
            .is_some();
        assert_eq!(has, w.recipe == 0, "{}", w.path);
        if has {
            assert_eq!(
                ascii_of(&e, exif::Tag::GPSLatitudeRef).as_deref(),
                Some("S")
            );
        }
    }
}

/// Map: a geotag (passed as `ExportSource.gps`) replaces the RAW's own
/// position in the export, and "Remove location" still strips it, in every
/// format Rust encodes (HEIC goes through the app's encoder: `MapTests`).
#[test]
fn a_geotag_replaces_the_raw_position_unless_stripped() {
    let t = tempfile::tempdir().unwrap();
    let raw = tiny_raw_with_gps(t.path(), "G.ARW");
    let out = t.path().join("Output");
    let mut specs = Vec::new();
    for (i, f) in [
        ExportFileFormat::Jpeg { quality: 90 },
        ExportFileFormat::Tiff16,
        ExportFileFormat::Png8,
        ExportFileFormat::Png16,
    ]
    .into_iter()
    .enumerate()
    {
        let mut keep = recipe("keep", f, &out);
        keep.file_name_template = format!("{{name}}-{i}-keep");
        let mut strip = recipe("strip", f, &out);
        strip.strip_gps = true;
        strip.file_name_template = format!("{{name}}-{i}-nogps");
        specs.extend([keep, strip]);
    }
    let recipes: Vec<ExportRecipe> = specs.iter().map(|r| recipe_of(r).unwrap()).collect();
    let mut src = source(1, &raw);
    src.gps = Some(ExportGps {
        latitude: 22.25,
        longitude: -159.5,
        altitude_m: Some(-3.0),
    });
    let item = item_of(&src);
    let g = item.meta.gps.unwrap();
    assert_eq!(
        (g.latitude, g.longitude, g.altitude_m),
        (22.25, -159.5, Some(-3.0))
    );
    let rep = run(
        &[item],
        &recipes,
        &synth_renderer(vec![(1, raw.clone(), None)], None),
        None,
        false,
        &Dispatcher::new(),
        &|| false,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(rep.written.len(), 8);
    for w in &rep.written {
        let e = exif_of(&w.path).unwrap_or_else(|| panic!("no EXIF in {}", w.path));
        if w.recipe % 2 == 0 {
            assert_eq!(
                ascii_of(&e, exif::Tag::GPSLatitudeRef).as_deref(),
                Some("N")
            );
            assert_eq!(
                ascii_of(&e, exif::Tag::GPSLongitudeRef).as_deref(),
                Some("W")
            );
            let below = e
                .get_field(exif::Tag::GPSAltitudeRef, exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0));
            assert_eq!(below, Some(1), "below sea level");
        } else {
            assert!(
                e.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY)
                    .is_none(),
                "{}",
                w.path
            );
        }
    }
    // The RAW itself is untouched.
    let own = ds_raw::read_metadata(&raw).unwrap().gps.unwrap();
    assert!((own.latitude + 20.75).abs() < 1e-9);
}

/// E-L2: the name sample the sheet's example uses (same metadata as the export).
#[test]
fn name_sample_and_source_kinds() {
    let t = tempfile::tempdir().unwrap();
    let raw = tiny_raw(t.path(), "R.ARW", 64, 48, 6);
    let jpg = jpeg_original(t.path(), "IMG_1.JPG", 16, 8, false);
    let s = export_name_sample(jpg.to_string_lossy().into_owned()).unwrap();
    assert_eq!(
        s,
        ExportNameSample {
            name: "IMG_1".into(),
            camera: Some("iPhone 17 Pro".into()),
            capture_time: Some("2026-08-09T07:30:05".into()),
            artist: None,
            copyright: None,
        }
    );
    let s = export_name_sample(raw.to_string_lossy().into_owned()).unwrap();
    assert_eq!(
        (s.name.as_str(), s.camera.as_deref(), s.capture_time),
        ("R", Some("ILCE-9"), None)
    );
    // The example matches what the export writes.
    assert_eq!(
        export_file_name_preview(
            "{date}_{camera}".into(),
            "IMG_1".into(),
            1,
            Some("iPhone 17 Pro".into()),
            Some("2026-08-09T07:30:05".into()),
            "jpg".into()
        )
        .unwrap(),
        "20260809_iPhone 17 Pro.jpg"
    );
    assert_eq!(source_kind(Path::new("a.mov")), SourceKind::Video);
    assert_eq!(source_kind(Path::new("a.HIF")), SourceKind::Raster);
    assert_eq!(source_kind(Path::new("a.dng")), SourceKind::Raw);
}

/// E-L4: reading metadata honours cancel and reports progress; a batch
/// cancelled before rendering writes nothing and never says "N of N".
#[test]
fn metadata_phase_reports_progress_and_cancels() {
    let t = tempfile::tempdir().unwrap();
    let raw = tiny_raw(t.path(), "M.ARW", 16, 8, 1);
    let sources: Vec<_> = (1..=5).map(|i| source(i, &raw)).collect();
    let events = Dispatcher::new();
    let seen = Arc::new(Events::default());
    events.set_listener(Some(seen.clone()));
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let r = read_items(&sources, &events, &|| {
        calls.fetch_add(1, Ordering::Relaxed) >= 2
    });
    events.flush(Duration::from_secs(5));
    assert!(r.is_none());
    let ev = seen.0.lock().unwrap().clone();
    assert!(
        ev.iter()
            .all(|e| e.kind == "export.metadata" && e.total == 5)
    );
    assert_eq!(
        ev.iter()
            .map(|e| (e.phase, e.completed))
            .collect::<Vec<_>>(),
        [
            (TaskPhase::Running, 1),
            (TaskPhase::Running, 2),
            (TaskPhase::Cancelled, 2)
        ]
    );
    // The report of a run cancelled while reading metadata.
    let events = Dispatcher::new();
    let seen = Arc::new(Events::default());
    events.set_listener(Some(seen.clone()));
    let rep = cancelled_report(&events, 5, 2);
    events.flush(Duration::from_secs(5));
    assert!(rep.cancelled && rep.written.is_empty() && rep.failed.is_empty());
    let last = seen.0.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (last.kind.as_str(), last.phase, last.completed, last.total),
        ("export", TaskPhase::Cancelled, 2, 5)
    );
}

/// gpu-audit #4: non-RAW originals are sized and sharpened by the app's decoder
/// (Core Image) instead of a full-size decode plus the CPU resize; upscales, a
/// missing decoder, an unreadable header and a failed sized decode (a decoder that
/// returns the wrong size, a panorama past the GPU's limits) take the full-size path.
#[test]
fn raster_originals_are_sized_by_the_apps_decoder() {
    let t = tempfile::tempdir().unwrap();
    let path = |n: &str| {
        let p = t.path().join(n);
        fs::write(&p, b"x").unwrap();
        p
    };
    let enc: Arc<dyn ImageEncoder> = Arc::new(FakeEncoder::default());
    let sources = vec![
        (1, path("A.HEIC"), None),
        (2, path("nosize.JPG"), None),
        (3, path("wrong.PNG"), None),
    ];
    let r = synth_renderer(sources.clone(), Some(enc));
    let mut small = recipe_of(&recipe("s", ExportFileFormat::Png8, t.path())).unwrap();
    small.sizing = Sizing::LongEdge(2);
    let (size, data) = rgb16(r.render_for(&item(1), &small).unwrap().unwrap());
    assert_eq!((size, data[0]), (Size::new(2, 1), 0x8000));
    small.sharpen = 0.5;
    assert_eq!(
        rgb16(r.render_for(&item(1), &small).unwrap().unwrap()).1[0],
        0x9000
    );
    // Upscale, unreadable size: the full-size path.
    let mut up = small.clone();
    up.sizing = Sizing::LongEdge(8);
    up.allow_upscale = true;
    assert!(r.render_for(&item(1), &up).is_none());
    assert!(r.render_for(&item(2), &small).is_none());
    let err = r
        .raster(
            &t.path().join("wrong.PNG"),
            OutputSpace::Srgb,
            Some(DecodeTarget {
                width: 2,
                height: 1,
                sharpen: 0.0,
            }),
        )
        .unwrap_err();
    assert!(err.contains("not 2x1"), "{err}");
    assert!(r.render_for(&item(3), &small).is_none());
    // Without the app's decoder: the full-size path.
    let bare = synth_renderer(sources, None);
    assert!(bare.render_for(&item(1), &small).is_none());
}

/// Batch export of the sample RAWs to JPEG (full size and 2048 px + sharpening) and
/// PNG8 (1024 px), mozjpeg: ms per photo per recipe set. `cargo test --release -p
/// ds-ffi batch_8bit_export_cost -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn batch_8bit_export_cost() {
    let samples = ds_testdata::samples_with_ext("ARW");
    if samples.is_empty() {
        return;
    }
    let t = tempfile::tempdir().unwrap();
    let run = |label: &str, specs: Vec<ExportRecipeSpec>| {
        let sources = samples
            .iter()
            .enumerate()
            .map(|(i, p)| source(i as u64 + 1, p))
            .collect();
        let started = std::time::Instant::now();
        let rep = block_on(export_batch(
            sources,
            specs,
            None,
            false,
            None,
            CancelToken::new(),
            None,
            None,
        ))
        .unwrap();
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        println!(
            "EXPORTBENCH8 {label}: {} photos, {:.0} ms/photo",
            samples.len(),
            started.elapsed().as_secs_f64() * 1000.0 / samples.len() as f64
        );
    };
    let dest = |n: &str| t.path().join(n);
    run(
        "warm",
        vec![recipe(
            "w",
            ExportFileFormat::Jpeg { quality: 90 },
            &dest("w"),
        )],
    );
    for i in 0..2 {
        run(
            &format!("JPEG q90 full size #{i}"),
            vec![recipe(
                "f",
                ExportFileFormat::Jpeg { quality: 90 },
                &dest(&format!("f{i}")),
            )],
        );
        let mut web = recipe(
            "s",
            ExportFileFormat::Jpeg { quality: 85 },
            &dest(&format!("s{i}")),
        );
        web.size = ExportSize::LongEdge { pixels: 2048 };
        web.sharpen = 0.5;
        let mut png = recipe("p", ExportFileFormat::Png8, &dest(&format!("p{i}")));
        png.size = ExportSize::LongEdge { pixels: 1024 };
        run(&format!("JPEG 2048 + PNG8 1024 #{i}"), vec![web, png]);
    }
}

/// Per-stage cost of one full-size export: develop to 16-bit RGB vs 8-bit RGBA, and
/// mozjpeg q90 from each. `cargo test --release -p ds-ffi export_8bit_stage_cost --
/// --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn export_8bit_stage_cost() {
    let Some(path) = ds_testdata::sample_arw() else {
        return;
    };
    let r = DevelopRenderer::new(
        HashMap::from([(1, (path, None))]),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    let dir = std::env::temp_dir();
    let tiff = recipe_of(&recipe("t", ExportFileFormat::Tiff16, &dir)).unwrap();
    let jpeg = recipe_of(&recipe("j", ExportFileFormat::Jpeg { quality: 90 }, &dir)).unwrap();
    let _ = r.render_for(&item(1), &tiff).unwrap().unwrap();
    let _ = r.render_for(&item(1), &jpeg).unwrap().unwrap();
    let icc = ds_export::icc::profile(OutputSpace::Srgb);
    for _ in 0..3 {
        let t = std::time::Instant::now();
        let wide = r.render_for(&item(1), &tiff).unwrap().unwrap();
        let t16 = t.elapsed().as_secs_f64() * 1000.0;
        let t = std::time::Instant::now();
        let narrow = r.render_for(&item(1), &jpeg).unwrap().unwrap();
        let t8 = t.elapsed().as_secs_f64() * 1000.0;
        let Rendered::Rgba8 { size, data } = narrow else {
            panic!("JPEG renders 8-bit")
        };
        let img = Image16::from_rendered(wide).unwrap();
        let fmt = ExportFormat::Jpeg { quality: 90 };
        let t = std::time::Instant::now();
        let mut c = std::io::Cursor::new(Vec::new());
        ds_export::encode_to(&mut c, &img, fmt, icc, None, None).unwrap();
        let e16 = t.elapsed().as_secs_f64() * 1000.0;
        let t = std::time::Instant::now();
        let mut c = std::io::Cursor::new(Vec::new());
        let px = ds_export::Pixels::rgba8(size, &data).unwrap();
        ds_export::encode_pixels_to(&mut c, px, fmt, icc, None, None).unwrap();
        let e8 = t.elapsed().as_secs_f64() * 1000.0;
        println!(
            "EXPORTSTAGE {}x{}: develop Rgb16 {t16:.0} ms / Rgba8 {t8:.0} ms; mozjpeg q90 from 16-bit {e16:.0} ms / from 8-bit {e8:.0} ms",
            size.width, size.height
        );
    }
}

/// Keywords from the source reach the file (XMP + IPTC through mozjpeg) and the app's
/// encoder hook, and are left out when the recipe excludes metadata.
#[test]
fn keywords_are_exported_with_the_metadata() {
    let t = tempfile::tempdir().unwrap();
    let jpg = jpeg_original(t.path(), "K.JPG", 32, 16, false);
    let out = t.path().join("Out");
    let mut with = recipe("with", ExportFileFormat::Jpeg { quality: 90 }, &out);
    with.file_name_template = "{name}_with".into();
    let mut without = recipe("without", ExportFileFormat::Jpeg { quality: 90 }, &out);
    without.file_name_template = "{name}_without".into();
    without.include_exif = false;
    let mut s = source(1, &jpg);
    s.keywords = vec!["beach".into(), "Crete".into()];
    s.artist = Some("Settings Artist".into());
    s.metadata = Some(PhotoMetadata {
        title: "Morning Surf".into(),
        caption: "Dawn patrol".into(),
        creator: "Kai".into(),
        city: "Akureyri".into(),
        ..PhotoMetadata::default()
    });
    let rep = block_on(export_batch(
        vec![s.clone()],
        vec![with.clone(), without],
        None,
        false,
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap();
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let bytes = |suffix: &str| fs::read(out.join(format!("K_{suffix}.jpg"))).unwrap();
    let has = |b: &[u8], needle: &[u8]| b.windows(needle.len()).any(|w| w == needle);
    let a = bytes("with");
    assert!(has(&a, b"<rdf:li>beach</rdf:li>") && has(&a, b"<rdf:li>Crete</rdf:li>"));
    assert!(has(&a, b"Photoshop 3.0\0"));
    assert!(has(&a, b"Morning Surf") && has(&a, b"<photoshop:City>Akureyri</photoshop:City>"));
    assert!(
        has(&a, b"Kai") && !has(&a, b"Settings Artist"),
        "the photo's creator wins"
    );
    let w = bytes("without");
    assert!(!has(&w, b"<rdf:li>") && !has(&w, b"Morning Surf") && !has(&w, b"Dawn patrol"));
    // The app's encoder (HEIC / hardware JPEG) is handed them.
    let enc = Arc::new(FakeEncoder::default());
    with.format = ExportFileFormat::Heic { quality: 80 };
    with.file_name_template = "{name}_heic".into();
    let rep = block_on(export_batch(
        vec![s],
        vec![with],
        Some(enc.clone()),
        false,
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap();
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(
        *enc.1.lock().unwrap(),
        [vec!["beach".to_string(), "Crete".into()]]
    );
    let m = enc.2.lock().unwrap();
    assert_eq!(
        (m[0].title.as_str(), m[0].creator.as_str()),
        ("Morning Surf", "Kai")
    );
    let xmp = enc.3.lock().unwrap();
    let packet = String::from_utf8(xmp[0].clone().unwrap()).unwrap();
    assert!(packet.contains("<rdf:li>beach</rdf:li>"), "{packet}");
}

/// The rating, colour label and pick reach the file as `xmp:Rating` and
/// `Darkslide/…` tags (and the app's encoder as an XMP packet); a recipe with
/// `label_tags` off keeps the rating but leaves the tags out.
#[test]
fn rating_label_and_pick_are_exported() {
    let t = tempfile::tempdir().unwrap();
    let jpg = jpeg_original(t.path(), "R.JPG", 32, 16, false);
    let out = t.path().join("Out");
    let mut tags = recipe("tags", ExportFileFormat::Jpeg { quality: 90 }, &out);
    tags.file_name_template = "{name}_tags".into();
    let mut plain = recipe("plain", ExportFileFormat::Jpeg { quality: 90 }, &out);
    plain.file_name_template = "{name}_plain".into();
    plain.label_tags = false;
    let mut s = source(1, &jpg);
    s.keywords = vec!["beach".into()];
    s.rating = Some(5);
    s.label = Some(crate::session::ColorLabel::Green);
    s.pick = Some(crate::session::PickFlag::Picked);
    let rep = block_on(export_batch(
        vec![s.clone()],
        vec![tags.clone(), plain],
        None,
        false,
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap();
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let has = |b: &[u8], needle: &[u8]| b.windows(needle.len()).any(|w| w == needle);
    let a = fs::read(out.join("R_tags.jpg")).unwrap();
    assert!(has(&a, b"<xmp:Rating>5</xmp:Rating>"));
    assert!(has(&a, b"<rdf:li>Darkslide/Label/Green</rdf:li>"));
    assert!(has(&a, b"<rdf:li>Darkslide|Pick</rdf:li>"));
    let p = fs::read(out.join("R_plain.jpg")).unwrap();
    assert!(has(&p, b"<xmp:Rating>5</xmp:Rating>"));
    assert!(!has(&p, b"Darkslide/Label") && !has(&p, b"Darkslide|Pick"));
    // The app's encoder gets the leaves as keywords and the packet.
    let enc = Arc::new(FakeEncoder::default());
    tags.format = ExportFileFormat::Heic { quality: 80 };
    tags.file_name_template = "{name}_heic".into();
    let rep = block_on(export_batch(
        vec![s],
        vec![tags],
        Some(enc.clone()),
        false,
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap();
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert_eq!(
        *enc.1.lock().unwrap(),
        [vec!["beach".to_string(), "Green".into(), "Pick".into()]]
    );
    let packet = String::from_utf8(enc.3.lock().unwrap()[0].clone().unwrap()).unwrap();
    assert!(packet.contains("<xmp:Rating>5</xmp:Rating>"), "{packet}");
}

/// Final review M5: a destination that is (almost) full refuses the export up front
/// with one plain line; a run that fills it up reports one line, not one per file.
#[test]
fn a_full_export_disk_is_refused_up_front_and_reported_once() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("Out/nested");
    let r = recipe_of(&recipe("j", ExportFileFormat::Jpeg { quality: 90 }, &out)).unwrap();
    let f = preflight_space(std::slice::from_ref(&r), &|_| Some(10 << 20)).unwrap();
    assert!(
        f.message.starts_with("The export disk is full (10 MB free"),
        "{}",
        f.message
    );
    assert_eq!((f.source_id, f.recipe), (None, Some(0)));
    assert!(preflight_space(std::slice::from_ref(&r), &|_| Some(5 << 30)).is_none());
    assert!(preflight_space(std::slice::from_ref(&r), &|_| None).is_none());
    // The real volume (the nearest existing ancestor of a folder not made yet).
    assert!(free_bytes(&out).is_some_and(|b| b > 0));
    let one = disk_full_failure(1, 1, Some(2_500_000_000));
    assert_eq!(
        one.message,
        "The export disk is full (2.5 GB free): stopped after 1 file; 1 photo was not exported."
    );
    assert!(
        disk_full_failure(3, 7, None)
            .message
            .contains("3 files; 7 photos were not")
    );
}

/// A recipe with AI Upscale enlarges through the app's provider (the export's own
/// upscaler is taken when the renderer is made).
#[test]
fn an_export_recipe_with_ai_upscale_runs_the_apps_provider() {
    let t = tempfile::tempdir().unwrap();
    let raw = tiny_raw(t.path(), "S1.ARW", 16, 8, 1);
    let out = t.path().join("Output");
    let mut big = recipe("big", ExportFileFormat::Png16, &out);
    big.size = ExportSize::Percent { percent: 200.0 };
    big.ai_upscale = true;
    let mut same = recipe("same", ExportFileFormat::Png16, &out);
    same.ai_upscale = true;
    let recipes: Vec<ExportRecipe> = [big, same].iter().map(|r| recipe_of(r).unwrap()).collect();
    let p = crate::upscale::tests::provider(true, || Ok(true));
    crate::upscale::set_upscale_provider(Some(
        p.clone() as Arc<dyn crate::upscale::UpscaleProvider>
    ));
    let r = synth_renderer(vec![(3, raw.clone(), None)], None);
    crate::upscale::set_upscale_provider(None);
    let rep = run(
        &[item_of(&source(3, &raw))],
        &recipes,
        &r,
        None,
        false,
        &Dispatcher::new(),
        &|| false,
        &AtomicBool::new(false),
    );
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let size = |i: u32| {
        let w = rep.written.iter().find(|w| w.recipe == i).unwrap();
        (w.width, w.height)
    };
    assert_eq!(size(0), (192, 128));
    assert_eq!(size(1), (96, 64), "original size: nothing to enlarge");
    assert_eq!(p.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    // Cancelling the export cancels the upscale it waits for.
    r.cancel_now();
    assert!(r.upscaler.cancel_token().is_cancelled());
}

/// Core Text stand-in: a solid box, 0.5 em per character × 1 em.
struct BlockText;

impl WatermarkTextRenderer for BlockText {
    fn rasterize(
        &self,
        text: String,
        _font: String,
        pixel_size: f32,
    ) -> Result<crate::watermark::WatermarkTextMask, DsError> {
        let h = pixel_size.round() as u32;
        let w = (pixel_size * 0.5 * text.chars().count() as f32).round() as u32;
        Ok(crate::watermark::WatermarkTextMask {
            width: w,
            height: h,
            coverage: vec![255; (w * h) as usize],
        })
    }
}

/// 16-bit RGB samples of a PNG16 export.
fn png16_pixels(path: &Path) -> (u32, Vec<u16>) {
    let mut r = ::png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    let px = buf[..info.buffer_size()]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_be_bytes(*b))
        .collect();
    (info.width, px)
}

/// A recipe's watermark is drawn on its outputs (text through the app's renderer,
/// tokens from the photo), recipes without one are untouched, and a text watermark
/// without the app's renderer fails that recipe only.
#[test]
fn watermarks_are_drawn_on_exports() {
    use crate::watermark::{WatermarkAnchor, WatermarkContent};
    let t = tempfile::tempdir().unwrap();
    let jpg = jpeg_original(t.path(), "W.JPG", 64, 32, false);
    let out = t.path().join("Out");
    let mut marked = recipe("marked", ExportFileFormat::Png16, &out);
    marked.file_name_template = "{name}_marked".into();
    marked.watermark = Some(WatermarkSpec {
        content: WatermarkContent::Text {
            text: "{year}".into(),
            font: String::new(),
            size: 0.25,
            color: 0,
            shadow: false,
        },
        anchor: WatermarkAnchor::BottomRight,
        inset: 0.0,
        opacity: 1.0,
    });
    let mut plain = recipe("plain", ExportFileFormat::Png16, &out);
    plain.file_name_template = "{name}_plain".into();
    let run = |text: Option<Arc<dyn WatermarkTextRenderer>>| {
        block_on(export_batch(
            vec![source(1, &jpg)],
            vec![marked.clone(), plain.clone()],
            None,
            false,
            None,
            CancelToken::new(),
            None,
            text,
        ))
        .unwrap()
    };
    let rep = run(Some(Arc::new(BlockText)));
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    assert!(rep.written.iter().all(|w| !w.watermark_skipped));
    // `{copyright}` on a photo without one: written without the mark, and reported.
    let mut blank = marked.clone();
    blank.file_name_template = "{name}_blank".into();
    if let Some(WatermarkSpec {
        content: WatermarkContent::Text { text, .. },
        ..
    }) = &mut blank.watermark
    {
        *text = "{copyright}".into();
    }
    let rep2 = block_on(export_batch(
        vec![source(1, &jpg)],
        vec![blank],
        None,
        false,
        None,
        CancelToken::new(),
        None,
        Some(Arc::new(BlockText)),
    ))
    .unwrap();
    assert_eq!(rep2.written.len(), 1);
    assert!(rep2.written[0].watermark_skipped);
    let (w, a) = png16_pixels(&out.join("W_marked.png"));
    let (_, b) = png16_pixels(&out.join("W_plain.png"));
    let at = |px: &[u16], x: u32, y: u32| px[((y * w + x) * 3) as usize];
    // "2026" (the capture year): 4 × 4 px wide, 8 px tall, in the bottom-right corner.
    for (x, y) in [(48, 24), (63, 31), (55, 28)] {
        assert_eq!(at(&a, x, y), 0, "({x},{y})");
        assert!(at(&b, x, y) > 50_000, "({x},{y})");
    }
    for (x, y) in [(47, 28), (55, 23), (10, 5)] {
        assert_eq!(at(&a, x, y), at(&b, x, y), "({x},{y}) untouched");
    }
    // Without the app's text renderer the watermarked recipe fails, the other is written.
    fs::remove_dir_all(&out).unwrap();
    let rep = run(None);
    assert_eq!(rep.written.len(), 1);
    assert_eq!(rep.written[0].recipe, 1);
    assert_eq!(rep.failed.len(), 1);
    assert_eq!(rep.failed[0].recipe, Some(0));
    assert!(
        rep.failed[0].message.contains("text renderer"),
        "{:?}",
        rep.failed
    );
}

#[test]
fn watermark_recipes_are_validated() {
    use crate::watermark::{WatermarkAnchor, WatermarkContent};
    let t = tempfile::tempdir().unwrap();
    let mut r = recipe("Web", ExportFileFormat::Png8, t.path());
    r.watermark = Some(WatermarkSpec {
        content: WatermarkContent::Logo {
            path: t.path().join("gone.png").to_string_lossy().into_owned(),
            scale: 0.2,
        },
        anchor: WatermarkAnchor::BottomRight,
        inset: 0.02,
        opacity: 0.8,
    });
    let msg = validate_export_recipes(vec![r.clone()]).unwrap().unwrap();
    assert!(
        msg.contains("Web: watermark logo") && msg.contains("gone.png"),
        "{msg}"
    );
    r.watermark = Some(WatermarkSpec {
        content: WatermarkContent::Text {
            text: "  ".into(),
            font: String::new(),
            size: 0.05,
            color: 0xFF_FF_FF,
            shadow: true,
        },
        anchor: WatermarkAnchor::Center,
        inset: 0.0,
        opacity: 1.0,
    });
    let msg = validate_export_recipes(vec![r.clone()]).unwrap().unwrap();
    assert!(msg.contains("Web: watermark text is empty"), "{msg}");
    r.watermark = None;
    assert_eq!(validate_export_recipes(vec![r]).unwrap(), None);
}
