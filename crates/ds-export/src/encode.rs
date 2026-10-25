//! Encoders: JPEG (mozjpeg), 16-bit TIFF (`tiff`), PNG 8/16 (`png`), HEIC via hook.
//!
//! JPEG uses mozjpeg with its "fastest" settings (libjpeg-turbo baseline, NEON
//! SIMD), faster than `jpeg-encoder` at equal quality (see `benches/encode.rs`).
//! Chroma is full resolution (4:4:4) at quality ≥ 90, where subsampling
//! artifacts on saturated edges are the visible loss; below 90 it is 4:2:0.
//!
//! JPEG and PNG are fed in [`STRIP_ROWS`]-row strips converted on the fly, so
//! encoding never holds a second image-sized buffer. JPEG and PNG8 also take an
//! 8-bit RGBA render as is ([`Pixels::Rgba8`]).

use std::io::{Cursor, Seek, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};

use tiff::encoder::{Rational, TiffEncoder, colortype::RGB16};
use tiff::tags::{Tag, Type};

use ds_core::IptcMeta;

use crate::ExportError;
use crate::exif::{self, Entry, ExifDirs, FieldType};
use crate::image::{Image16, Pixels};
use crate::recipe::ExportFormat;

/// HEIC encoding is implemented in Swift (`ImageIO`) and injected.
pub trait HeicEncoder: Send + Sync {
    /// Encode 16-bit RGB (output-encoded) to HEIC bytes, embedding `icc` and,
    /// when given, the TIFF-structured `exif` block, the `keywords` and the
    /// `iptc` fields (IPTC/XMP), and the XMP packet `xmp` (the same fields plus the
    /// rating and hierarchical tags, see [`crate::xmp::packet`]), merged with them.
    #[allow(clippy::too_many_arguments)] // one call per output, mirrored across the FFI
    fn encode(
        &self,
        img: &Image16,
        quality: u8,
        icc: &[u8],
        exif: Option<&[u8]>,
        keywords: &[String],
        iptc: &IptcMeta,
        xmp: Option<&[u8]>,
    ) -> Result<Vec<u8>, String>;
}

/// Rows converted per encoder strip.
pub const STRIP_ROWS: usize = 64;

/// Quality at and above which JPEG uses 4:4:4 chroma.
pub const FULL_CHROMA_QUALITY: u8 = 90;

fn strips(h: usize) -> impl Iterator<Item = std::ops::Range<usize>> {
    (0..h)
        .step_by(STRIP_ROWS)
        .map(move |y| y..(y + STRIP_ROWS).min(h))
}

fn enc_err(e: impl std::fmt::Display) -> ExportError {
    ExportError::Encode(e.to_string())
}

/// Encode `img` into `w`.
pub fn encode_to<W: Write + Seek>(
    w: &mut W,
    img: &Image16,
    format: ExportFormat,
    icc: &[u8],
    exif: Option<&ExifDirs>,
    heic: Option<&dyn HeicEncoder>,
) -> Result<(), ExportError> {
    encode_pixels_to(w, Pixels::Rgb16(img), format, icc, exif, heic).map(drop)
}

/// Encode `px` into `w`: JPEG and PNG8 read 8-bit input directly; the 16-bit
/// formats and the HEIC hook take an 8-bit render converted to 16 bits. Returns notes
/// on metadata left out to fit the format (a JPEG segment holds 64 KB).
pub fn encode_pixels_to<W: Write + Seek>(
    w: &mut W,
    px: Pixels<'_>,
    format: ExportFormat,
    icc: &[u8],
    exif: Option<&ExifDirs>,
    heic: Option<&dyn HeicEncoder>,
) -> Result<Vec<String>, ExportError> {
    if icc.len() > crate::icc::MAX_EMBEDDED_BYTES {
        return Err(ExportError::Profile(format!(
            "profile is {} bytes; a file can embed at most {}",
            icc.len(),
            crate::icc::MAX_EMBEDDED_BYTES
        )));
    }
    match format {
        ExportFormat::Jpeg { quality } => jpeg(w, px, quality, icc, exif),
        ExportFormat::Png8 => png(w, px, false, icc, exif).map(|()| Vec::new()),
        ExportFormat::Png16 => png(w, px, true, icc, exif).map(|()| Vec::new()),
        ExportFormat::Tiff16 => tiff16(w, &*px.to_image16()?, icc, exif).map(|()| Vec::new()),
        ExportFormat::Heic { quality } => {
            let img = px.to_image16()?;
            let img = img.as_ref();
            let enc = heic.ok_or(ExportError::NoHeicEncoder)?;
            let blob = exif.map(exif::to_tiff_bytes);
            let keywords = exif
                .map(|d| crate::xmp::subjects(&d.keywords, &d.organize))
                .unwrap_or_default();
            let none = IptcMeta::default();
            let iptc = exif.map_or(&none, |d| &d.iptc);
            // One JPEG segment's worth (the hook also writes JPEG).
            let room = crate::xmp::JPEG_SEGMENT_MAX - crate::xmp::XMP_JPEG_HEADER.len();
            let xmp = exif
                .and_then(|d| crate::xmp::packet_fit(&d.keywords, &d.iptc, &d.organize, room))
                .map(|(p, _)| p);
            let bytes = ds_fsutil::catch_panic(|| {
                enc.encode(
                    img,
                    quality,
                    icc,
                    blob.as_deref(),
                    &keywords,
                    iptc,
                    xmp.as_deref(),
                )
            })
            .map_err(|p| format!("HEIC encoder panicked: {p}"))
            .and_then(|r| r)
            .map_err(ExportError::Encode)?;
            w.write_all(&bytes).map_err(enc_err)?;
            Ok(Vec::new())
        }
    }
}

/// Encode to an in-memory buffer.
pub fn encode(
    img: &Image16,
    format: ExportFormat,
    icc: &[u8],
    exif: Option<&ExifDirs>,
    heic: Option<&dyn HeicEncoder>,
) -> Result<Vec<u8>, ExportError> {
    let mut c = Cursor::new(Vec::new());
    encode_to(&mut c, img, format, icc, exif, heic)?;
    Ok(c.into_inner())
}

fn jpeg<W: Write>(
    w: &mut W,
    img: Pixels<'_>,
    quality: u8,
    icc: &[u8],
    exif: Option<&ExifDirs>,
) -> Result<Vec<String>, ExportError> {
    const LIMIT: u32 = 65_500;
    let size = img.size();
    if size.width > LIMIT || size.height > LIMIT {
        return Err(ExportError::TooLarge(size));
    }
    let app1 = exif.map(|d| [&b"Exif\0\0"[..], &exif::to_tiff_bytes(d)].concat());
    if app1.as_ref().is_some_and(|a| a.len() > 65_533) {
        return Err(ExportError::Encode("EXIF block exceeds 64 KB".into()));
    }
    let mut notes = Vec::new();
    // libjpeg reports errors by unwinding; contain them.
    catch_unwind(AssertUnwindSafe(|| -> std::io::Result<()> {
        // An 8-bit RGBA render goes to libjpeg as is (RGBX input: no conversion).
        let input = match img {
            Pixels::Rgb16(_) => mozjpeg::ColorSpace::JCS_RGB,
            Pixels::Rgba8 { .. } => mozjpeg::ColorSpace::JCS_EXT_RGBX,
        };
        let mut c = mozjpeg::Compress::new(input);
        c.set_size(size.width as usize, size.height as usize);
        c.set_fastest_defaults();
        c.set_quality(f32::from(quality));
        if quality >= FULL_CHROMA_QUALITY {
            c.set_chroma_sampling_pixel_sizes((1, 1), (1, 1));
        }
        let mut s = c.start_compress(&mut *w)?;
        if let Some(a) = &app1 {
            s.write_marker(mozjpeg::Marker::APP(1), a);
        }
        if let Some(d) = exif {
            // A packet or IIM block past one segment would corrupt the file, so
            // long text and trailing keywords are cut to fit, and said so in `notes`.
            let room = crate::xmp::JPEG_SEGMENT_MAX - crate::xmp::XMP_JPEG_HEADER.len();
            if let Some((p, note)) =
                crate::xmp::packet_fit(&d.keywords, &d.iptc, &d.organize, room)
            {
                s.write_marker(
                    mozjpeg::Marker::APP(1),
                    &[crate::xmp::XMP_JPEG_HEADER, &p].concat(),
                );
                notes.extend(note);
            }
            let keywords = crate::xmp::subjects(&d.keywords, &d.organize);
            if let Some((a, dropped)) =
                crate::xmp::iptc_app13_fit(&keywords, &d.iptc, crate::xmp::JPEG_SEGMENT_MAX)
            {
                s.write_marker(mozjpeg::Marker::APP(13), &a);
                if dropped > 0 {
                    notes.push(format!(
                        "IPTC: {dropped} of {} keywords left out (they do not fit one JPEG segment)",
                        keywords.len()
                    ));
                }
            }
        }
        s.write_icc_profile(icc);
        match img {
            Pixels::Rgba8 { data, .. } => s.write_scanlines(data)?,
            Pixels::Rgb16(img) => {
                let mut rgb = Vec::new();
                for rows in strips(size.height as usize) {
                    img.rgb8_rows(rows, &mut rgb);
                    s.write_scanlines(&rgb)?;
                }
            }
        }
        s.finish()?;
        Ok(())
    }))
    .map_err(|p| {
        let msg = p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_owned()));
        ExportError::Encode(format!(
            "jpeg: {}",
            msg.unwrap_or_else(|| "libjpeg error".into())
        ))
    })?
    .map_err(enc_err)?;
    Ok(notes)
}

fn png<W: Write>(
    w: &mut W,
    px: Pixels<'_>,
    sixteen: bool,
    icc: &[u8],
    exif: Option<&ExifDirs>,
) -> Result<(), ExportError> {
    let wide = if sixteen {
        Some(px.to_image16()?)
    } else {
        None
    };
    let size = px.size();
    let mut info = png::Info::with_size(size.width, size.height);
    info.color_type = png::ColorType::Rgb;
    info.bit_depth = if sixteen {
        png::BitDepth::Sixteen
    } else {
        png::BitDepth::Eight
    };
    info.icc_profile = Some(icc.into());
    info.exif_metadata = exif.map(|d| exif::to_tiff_bytes(d).into());
    if let Some(p) = exif.and_then(|d| crate::xmp::packet(&d.keywords, &d.iptc, &d.organize)) {
        let xml = String::from_utf8(p).map_err(enc_err)?;
        info.utf8_text.push(png::text_metadata::ITXtChunk::new(
            crate::xmp::XMP_PNG_KEYWORD,
            xml,
        ));
    }
    let mut enc = png::Encoder::with_info(w, info).map_err(enc_err)?;
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().map_err(enc_err)?;
    let mut stream = writer.stream_writer().map_err(enc_err)?;
    let mut buf = Vec::new();
    for rows in strips(size.height as usize) {
        match &wide {
            Some(img) => img.be16_rows(rows, &mut buf),
            None => px.rgb8_rows(rows, &mut buf),
        }
        stream.write_all(&buf).map_err(enc_err)?;
    }
    stream.finish().map_err(enc_err)?;
    writer.finish().map_err(enc_err)
}

fn tiff_type(t: FieldType) -> Type {
    match t {
        FieldType::Byte => Type::BYTE,
        FieldType::Ascii => Type::ASCII,
        FieldType::Short => Type::SHORT,
        FieldType::Long => Type::LONG,
        FieldType::Rational => Type::RATIONAL,
        FieldType::Undefined => Type::UNDEFINED,
        FieldType::SRational => Type::SRATIONAL,
    }
}

/// Entry bytes are little-endian; the `tiff` crate writes native order.
fn native(e: &Entry) -> Vec<u8> {
    let unit = match e.ty {
        FieldType::Short => 2,
        FieldType::Long | FieldType::Rational | FieldType::SRational => 4,
        _ => 1,
    };
    let mut d = e.data.clone();
    if cfg!(target_endian = "big") && unit > 1 {
        d.chunks_mut(unit).for_each(<[u8]>::reverse);
    }
    d
}

fn tiff16<W: Write + Seek>(
    w: &mut W,
    img: &Image16,
    icc: &[u8],
    exif: Option<&ExifDirs>,
) -> Result<(), ExportError> {
    if img.size.pixels() * 6 > u64::from(u32::MAX) - (1 << 20) {
        return Err(ExportError::TooLarge(img.size));
    }
    let mut enc = TiffEncoder::new(w).map_err(enc_err)?;
    // Sub-IFDs (Exif, GPS) are written first; IFD0 points at them.
    let mut pointers = Vec::new();
    if let Some(d) = exif {
        for (tag, entries) in [(exif::TAG_EXIF_IFD, &d.exif), (exif::TAG_GPS_IFD, &d.gps)] {
            if entries.is_empty() {
                continue;
            }
            let mut dir = enc.extra_directory().map_err(enc_err)?;
            let mut t = tiff::Directory::empty();
            for e in entries {
                let entry = dir
                    .write_entry_bytes(tiff_type(e.ty), &native(e))
                    .map_err(enc_err)?;
                t.extend([(Tag::from_u16_exhaustive(e.tag), entry)]);
            }
            dir.extend_from(&t);
            pointers.push((tag, dir.finish_with_offsets().map_err(enc_err)?.offset));
        }
    }
    let mut image = enc
        .new_image::<RGB16>(img.size.width, img.size.height)
        .map_err(enc_err)?;
    image.resolution(tiff::tags::ResolutionUnit::Inch, Rational { n: 300, d: 1 });
    let dir = image.encoder();
    let mut t = tiff::Directory::empty();
    let icc_entry = dir
        .write_entry_bytes(Type::UNDEFINED, icc)
        .map_err(enc_err)?;
    t.extend([(Tag::IccProfile, icc_entry)]);
    if let Some(p) = exif.and_then(|d| crate::xmp::packet(&d.keywords, &d.iptc, &d.organize)) {
        let entry = dir.write_entry_bytes(Type::BYTE, &p).map_err(enc_err)?;
        t.extend([(Tag::from_u16_exhaustive(crate::xmp::TAG_XMP), entry)]);
    }
    for e in exif.map(|d| d.ifd0.as_slice()).unwrap_or_default() {
        let entry = dir
            .write_entry_bytes(tiff_type(e.ty), &native(e))
            .map_err(enc_err)?;
        t.extend([(Tag::from_u16_exhaustive(e.tag), entry)]);
    }
    dir.extend_from(&t);
    for (tag, off) in pointers {
        dir.write_tag(Tag::from_u16_exhaustive(tag), off)
            .map_err(enc_err)?;
    }
    image.write_data(&img.data).map_err(enc_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exif::tests::sample;
    use crate::exif::{CAPTION_MAX, ExifMeta};
    use crate::icc;
    use crate::recipe::OutputSpace;
    use ::exif::{In, Tag as ETag};
    use ds_core::Size;

    fn gradient(w: u32, h: u32) -> Image16 {
        let data = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                [(x * 65_535 / w) as u16, (y * 65_535 / h) as u16, 32_768]
            })
            .collect();
        Image16::new(Size::new(w, h), data).unwrap()
    }

    fn icc() -> &'static [u8] {
        icc::profile(OutputSpace::DisplayP3)
    }

    fn exif_of(bytes: &[u8]) -> ::exif::Exif {
        ::exif::Reader::new()
            .read_from_container(&mut Cursor::new(bytes))
            .expect("exif present")
    }

    /// Sampling factors (h<<4 | v) of each component from the SOF0/SOF2 header.
    fn sof_sampling(jpeg: &[u8]) -> Vec<u8> {
        let mut i = 2;
        while i + 4 < jpeg.len() {
            assert_eq!(jpeg[i], 0xFF);
            let marker = jpeg[i + 1];
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            if marker == 0xC0 || marker == 0xC2 {
                let n = usize::from(jpeg[i + 9]);
                return (0..n).map(|c| jpeg[i + 11 + 3 * c]).collect();
            }
            i += 2 + len;
        }
        panic!("no SOF");
    }

    #[test]
    fn full_chroma_at_high_quality_subsampled_below() {
        let img = gradient(64, 48);
        let at = |q| {
            let b = encode(&img, ExportFormat::Jpeg { quality: q }, icc(), None, None).unwrap();
            sof_sampling(&b)
        };
        assert_eq!(at(90), [0x11, 0x11, 0x11], "q90 must be 4:4:4");
        assert_eq!(at(100), [0x11, 0x11, 0x11]);
        assert_eq!(at(89), [0x22, 0x11, 0x11], "q<90 stays 4:2:0");
    }

    #[test]
    fn streamed_strips_cover_odd_heights() {
        // Heights that are not a multiple of the strip size decode fully.
        for h in [1u32, 63, 65, 130] {
            let img = gradient(9, h);
            let b = encode(&img, ExportFormat::Jpeg { quality: 95 }, icc(), None, None).unwrap();
            assert_eq!(u16::from_be_bytes([b[b.len() - 2], b[b.len() - 1]]), 0xFFD9);
            let p = encode(&img, ExportFormat::Png16, icc(), None, None).unwrap();
            let dec = png::Decoder::new(Cursor::new(p)).read_info().unwrap();
            assert_eq!(dec.info().height, h);
        }
    }

    /// The payload of every JPEG marker `marker` in `jpeg` (up to start of scan).
    fn jpeg_segments(jpeg: &[u8], marker: u8) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 2;
        while i + 4 <= jpeg.len() && jpeg[i] == 0xFF && jpeg[i + 1] != 0xDA {
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            if jpeg[i + 1] == marker {
                out.push(jpeg[i + 4..i + 2 + len].to_vec());
            }
            i += 2 + len;
        }
        out
    }

    /// Keywords reach every self-encoded format: JPEG (XMP APP1 + IPTC APP13), PNG
    /// (iTXt XMP) and TIFF (tag 700), with the recipe's metadata; none without it.
    #[test]
    fn keywords_are_written_as_xmp_and_iptc() {
        use crate::xmp::tests::{iim_keywords, subjects};
        let img = gradient(16, 8);
        let dirs = sample().dirs(false);
        let want = ["beach", "Crete & friends"];
        let j = encode(
            &img,
            ExportFormat::Jpeg { quality: 90 },
            icc(),
            Some(&dirs),
            None,
        )
        .unwrap();
        let xmp: Vec<_> = jpeg_segments(&j, 0xE1)
            .into_iter()
            .filter(|s| s.starts_with(crate::xmp::XMP_JPEG_HEADER))
            .collect();
        assert_eq!(xmp.len(), 1);
        assert_eq!(subjects(&xmp[0][crate::xmp::XMP_JPEG_HEADER.len()..]), want);
        let app13 = jpeg_segments(&j, 0xED);
        assert_eq!(iim_keywords(&app13[0]), want);
        let plain = encode(&img, ExportFormat::Jpeg { quality: 90 }, icc(), None, None).unwrap();
        assert_eq!(jpeg_segments(&plain, 0xED).len(), 0);

        let p = encode(&img, ExportFormat::Png8, icc(), Some(&dirs), None).unwrap();
        let r = png::Decoder::new(Cursor::new(&p)).read_info().unwrap();
        let chunk = r
            .info()
            .utf8_text
            .iter()
            .find(|c| c.keyword == crate::xmp::XMP_PNG_KEYWORD)
            .unwrap();
        assert_eq!(subjects(chunk.get_text().unwrap().as_bytes()), want);

        let t = encode(&img, ExportFormat::Tiff16, icc(), Some(&dirs), None).unwrap();
        let mut d = tiff::decoder::Decoder::new(Cursor::new(&t)).unwrap();
        let x = d
            .get_tag_u8_vec(Tag::from_u16_exhaustive(crate::xmp::TAG_XMP))
            .unwrap();
        assert_eq!(subjects(&x), want);
    }

    /// Title, caption, credits, location and rights read back from a written JPEG
    /// (XMP, IPTC-IIM, EXIF) and TIFF (XMP tag 700, EXIF); stripping GPS also drops
    /// the named place.
    #[test]
    fn iptc_fields_read_back_from_jpeg_and_tiff() {
        use crate::xmp::tests::{iim_datasets, iptc, xmp_values};
        let img = gradient(16, 8);
        let meta = ExifMeta {
            iptc: iptc(),
            ..sample()
        };
        let dirs = meta.dirs(true);
        let check_xmp = |x: &[u8]| {
            assert_eq!(xmp_values(x, "dc:title"), ["Morning <Surf>"]);
            assert_eq!(
                xmp_values(x, "dc:description"),
                ["Surfers & friends at dawn"]
            );
            assert_eq!(xmp_values(x, "dc:creator"), ["Alexa"]);
            assert_eq!(xmp_values(x, "dc:rights"), ["© 2026 Alexa"]);
            // GPS stripped: the named place goes too.
            for name in [
                "photoshop:City",
                "photoshop:State",
                "photoshop:Country",
                "Iptc4xmpCore:Location",
            ] {
                assert!(xmp_values(x, name).is_empty(), "{name}");
            }
            assert_eq!(
                xmp_values(x, "xmpRights:UsageTerms"),
                ["Editorial use only"]
            );
            assert_eq!(xmp_values(x, "dc:subject"), ["beach", "Crete & friends"]);
        };
        let check_exif = |e: &::exif::Exif| {
            let a = |t| {
                e.get_field(t, In::PRIMARY)
                    .unwrap()
                    .display_value()
                    .to_string()
            };
            assert_eq!(a(ETag::ImageDescription), "\"Surfers & friends at dawn\"");
            assert_eq!(a(ETag::Artist), "\"Alexa\"");
            assert!(a(ETag::Copyright).contains("2026 Alexa"));
            assert!(e.get_field(ETag::GPSLatitude, In::PRIMARY).is_none());
        };

        let j = encode(
            &img,
            ExportFormat::Jpeg { quality: 90 },
            icc(),
            Some(&dirs),
            None,
        )
        .unwrap();
        let xmp = jpeg_segments(&j, 0xE1)
            .into_iter()
            .find(|s| s.starts_with(crate::xmp::XMP_JPEG_HEADER))
            .unwrap();
        check_xmp(&xmp[crate::xmp::XMP_JPEG_HEADER.len()..]);
        let iim = iim_datasets(&jpeg_segments(&j, 0xED)[0]);
        assert!(
            !iim.iter().any(|d| [90, 92, 95, 101].contains(&d.0)),
            "no location datasets: {iim:?}"
        );
        for (id, want) in [
            (5, "Morning <Surf>"),
            (80, "Alexa"),
            (105, "Big swell"),
            (110, "Darkslide"),
            (116, "© 2026 Alexa"),
            (120, "Surfers & friends at dawn"),
            (40, "No crops"),
        ] {
            assert!(iim.contains(&(id, want.to_owned())), "2:{id}");
        }
        check_exif(&exif_of(&j));

        // With GPS kept, the place is written.
        let kept = encode(
            &img,
            ExportFormat::Jpeg { quality: 90 },
            icc(),
            Some(&meta.dirs(false)),
            None,
        )
        .unwrap();
        let iim = iim_datasets(&jpeg_segments(&kept, 0xED)[0]);
        for (id, want) in [
            (90, "Husavik"),
            (92, "Skjalfandi Bay"),
            (95, "Nordurland"),
            (101, "Iceland"),
        ] {
            assert!(iim.contains(&(id, want.to_owned())), "2:{id}");
        }
        let t = encode(&img, ExportFormat::Tiff16, icc(), Some(&dirs), None).unwrap();
        let mut d = tiff::decoder::Decoder::new(Cursor::new(&t)).unwrap();
        check_xmp(
            &d.get_tag_u8_vec(Tag::from_u16_exhaustive(crate::xmp::TAG_XMP))
                .unwrap(),
        );
        check_exif(&exif_of(&t));
    }

    /// A huge caption exports: EXIF capped, the XMP shortened to fit its segment (not
    /// dropped, and said so in the notes), IIM capped at 2:120's 2000 bytes.
    #[test]
    fn a_huge_caption_still_exports() {
        use crate::xmp::tests::{iim_datasets, xmp_values};
        let meta = ExifMeta {
            iptc: IptcMeta {
                caption: "é".repeat(40_000),
                ..IptcMeta::default()
            },
            ..sample()
        };
        let mut out = Cursor::new(Vec::new());
        let notes = encode_pixels_to(
            &mut out,
            Pixels::Rgb16(&gradient(16, 8)),
            ExportFormat::Jpeg { quality: 90 },
            icc(),
            Some(&meta.dirs(false)),
            None,
        )
        .unwrap();
        let j = out.into_inner();
        let e = exif_of(&j);
        let desc = e.get_field(ETag::ImageDescription, In::PRIMARY).unwrap();
        match &desc.value {
            ::exif::Value::Ascii(v) => assert_eq!(v[0].len(), CAPTION_MAX),
            v => panic!("{v:?}"),
        }
        let xmp = jpeg_segments(&j, 0xE1)
            .into_iter()
            .find(|s| s.starts_with(crate::xmp::XMP_JPEG_HEADER))
            .expect("the XMP is shortened, not dropped");
        assert!(xmp.len() + 2 <= 65_535);
        let caption = &xmp_values(&xmp[crate::xmp::XMP_JPEG_HEADER.len()..], "dc:description")[0];
        assert!(
            caption.starts_with('é') && caption.chars().count() < 40_000,
            "{}",
            caption.len()
        );
        assert_eq!(notes.len(), 1, "the cut is reported, not silent: {notes:?}");
        assert!(
            notes[0].starts_with("XMP: long text shortened"),
            "{notes:?}"
        );
        let iim = iim_datasets(&jpeg_segments(&j, 0xED)[0]);
        assert!(iim.iter().any(|d| d.0 == 120 && d.1 == "é".repeat(1000)));
    }

    /// About 950 long keywords overflowed the APP13 segment and failed the whole JPEG:
    /// trailing keywords are left out instead, the file decodes, and a note says how
    /// many.
    #[test]
    fn too_many_keywords_still_export() {
        use crate::xmp::tests::{iim_keywords, subjects};
        let keywords: Vec<String> = (0..3000)
            .map(|i| format!("{i:04}-{}", "k".repeat(40)))
            .collect();
        let meta = ExifMeta {
            keywords: keywords.clone(),
            ..sample()
        };
        let mut out = Cursor::new(Vec::new());
        let notes = encode_pixels_to(
            &mut out,
            Pixels::Rgb16(&gradient(16, 8)),
            ExportFormat::Jpeg { quality: 90 },
            icc(),
            Some(&meta.dirs(false)),
            None,
        )
        .unwrap();
        let j = out.into_inner();
        mozjpeg::Decompress::new_mem(&j).expect("a decodable JPEG");
        let app13 = &jpeg_segments(&j, 0xED)[0];
        assert!(app13.len() + 2 <= 65_535, "{}", app13.len());
        let kept = iim_keywords(app13);
        assert!(!kept.is_empty() && kept.len() < keywords.len());
        assert_eq!(
            kept[..],
            keywords[..kept.len()],
            "the leading keywords stay"
        );
        let xmp = jpeg_segments(&j, 0xE1)
            .into_iter()
            .find(|s| s.starts_with(crate::xmp::XMP_JPEG_HEADER))
            .unwrap();
        let in_xmp = subjects(&xmp[crate::xmp::XMP_JPEG_HEADER.len()..]);
        assert!(!in_xmp.is_empty() && in_xmp.len() < keywords.len());
        assert_eq!(in_xmp[..], keywords[..in_xmp.len()]);
        assert!(
            notes.iter().any(|n| n.starts_with(&format!(
                "IPTC: {} of 3000 keywords left out",
                keywords.len() - kept.len()
            ))),
            "{notes:?}"
        );
        assert!(notes.iter().any(|n| n.starts_with("XMP: ")), "{notes:?}");
        // Few keywords: nothing is cut and nothing is reported.
        let mut small = Cursor::new(Vec::new());
        let none = encode_pixels_to(
            &mut small,
            Pixels::Rgb16(&gradient(16, 8)),
            ExportFormat::Jpeg { quality: 90 },
            icc(),
            Some(&sample().dirs(false)),
            None,
        )
        .unwrap();
        assert!(none.is_empty(), "{none:?}");
    }

    /// libjpeg counts a profile's APP2 chunks in one byte: past 255 the file is
    /// corrupt, so a profile that big is refused for every format.
    #[test]
    fn a_profile_too_big_to_embed_is_refused() {
        let img = gradient(8, 8);
        let fits = vec![0u8; crate::icc::MAX_EMBEDDED_BYTES];
        let big = vec![0u8; crate::icc::MAX_EMBEDDED_BYTES + 1];
        for fmt in [
            ExportFormat::Jpeg { quality: 90 },
            ExportFormat::Png8,
            ExportFormat::Tiff16,
        ] {
            let e = encode(&img, fmt, &big, None, None).unwrap_err();
            assert!(matches!(e, ExportError::Profile(_)), "{fmt:?}: {e}");
        }
        // The largest allowed profile makes a JPEG whose chunks all read back.
        let j = encode(&img, ExportFormat::Jpeg { quality: 90 }, &fits, None, None).unwrap();
        let d = mozjpeg::Decompress::with_markers(mozjpeg::ALL_MARKERS)
            .from_mem(&j)
            .unwrap();
        let back: usize = d
            .markers()
            .filter(|m| m.data.starts_with(b"ICC_PROFILE\0"))
            .map(|m| m.data.len() - 14)
            .sum();
        assert_eq!(back, fits.len());
    }

    /// A camera or artist string past 255 bytes is cut, so it can't push the EXIF block
    /// past a JPEG segment and fail the photo.
    #[test]
    fn long_exif_strings_are_cut_not_fatal() {
        let meta = ExifMeta {
            model: Some("m".repeat(100_000)),
            artist: Some("é".repeat(50_000)),
            copyright: Some("c".repeat(70_000)),
            ..sample()
        };
        let j = encode(
            &gradient(16, 8),
            ExportFormat::Jpeg { quality: 90 },
            icc(),
            Some(&meta.dirs(false)),
            None,
        )
        .unwrap();
        let e = exif_of(&j);
        for tag in [ETag::Model, ETag::Artist, ETag::Copyright] {
            match &e.get_field(tag, In::PRIMARY).unwrap().value {
                ::exif::Value::Ascii(v) => assert!(v[0].len() <= 255 && v[0].len() > 200, "{tag}"),
                v => panic!("{v:?}"),
            }
        }
    }

    #[test]
    fn jpeg_decodes_with_exif_and_icc() {
        let img = gradient(64, 48);
        let dirs = sample().dirs(false);
        let bytes = encode(
            &img,
            ExportFormat::Jpeg { quality: 90 },
            icc(),
            Some(&dirs),
            None,
        )
        .unwrap();
        let d = mozjpeg::Decompress::with_markers(mozjpeg::ALL_MARKERS)
            .from_mem(&bytes)
            .unwrap();
        assert_eq!((d.width(), d.height()), (64, 48));
        let got_icc: Vec<u8> = d
            .markers()
            .filter(|m| m.data.starts_with(b"ICC_PROFILE\0"))
            .flat_map(|m| m.data[14..].to_vec())
            .collect();
        assert_eq!(got_icc, icc());
        let mut d = d.rgb().unwrap();
        let px: Vec<[u8; 3]> = d.read_scanlines().unwrap();
        // Top-left red≈0, bottom-right red≈high: gradient survived.
        assert!(
            px[0][0] < 10 && px[64 * 48 - 1][0] > 240,
            "{:?} {:?}",
            px[0],
            px[64 * 48 - 1]
        );
        let e = exif_of(&bytes);
        assert!(e.get_field(ETag::Model, In::PRIMARY).is_some());
        assert!(e.get_field(ETag::GPSLatitude, In::PRIMARY).is_some());
    }

    #[test]
    fn jpeg_q90_fastest_mode_quality_is_high() {
        // Gradients plus harsh high-frequency texture (worst case): mozjpeg's fastest
        // mode must match jpeg-encoder's quality at q90.
        let (w, h) = (256u32, 192u32);
        let data = (0..w * h)
            .flat_map(|i| {
                let (x, y) = (i % w, i / w);
                let t = ((x * 7 + y * 13) % 17) * 300;
                [
                    (x * 200 + t) as u16,
                    (y * 300 + t) as u16,
                    ((x + y) * 100) as u16,
                ]
            })
            .collect();
        let img = Image16::new(Size::new(w, h), data).unwrap();
        let bytes = encode(&img, ExportFormat::Jpeg { quality: 90 }, icc(), None, None).unwrap();
        let src = img.to_rgb8();
        let psnr = |jpeg: &[u8]| {
            let mut d = mozjpeg::Decompress::new_mem(jpeg).unwrap().rgb().unwrap();
            let px: Vec<[u8; 3]> = d.read_scanlines().unwrap();
            let mse = px
                .iter()
                .flatten()
                .zip(&src)
                .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
                .sum::<f64>()
                / src.len() as f64;
            10.0 * (255.0f64.powi(2) / mse).log10()
        };
        let mut other = Vec::new();
        jpeg_encoder::Encoder::new(&mut other, 90)
            .encode(&src, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        let (ours, theirs) = (psnr(&bytes), psnr(&other));
        eprintln!(
            "q90 PSNR mozjpeg-fastest {ours:.2} dB ({} B), jpeg-encoder {theirs:.2} dB ({} B)",
            bytes.len(),
            other.len()
        );
        assert!(
            ours > 34.0 && ours > theirs - 0.5,
            "mozjpeg {ours:.1} dB vs jpeg-encoder {theirs:.1} dB"
        );
    }

    #[test]
    fn jpeg_without_exif_and_too_large() {
        let bytes = encode(
            &gradient(8, 8),
            ExportFormat::Jpeg { quality: 50 },
            icc(),
            None,
            None,
        )
        .unwrap();
        assert!(
            ::exif::Reader::new()
                .read_from_container(&mut Cursor::new(&bytes))
                .is_err()
        );
        let huge = Image16 {
            size: Size::new(70_000, 1),
            data: vec![],
        };
        assert!(matches!(
            encode(&huge, ExportFormat::Jpeg { quality: 90 }, icc(), None, None),
            Err(ExportError::TooLarge(_))
        ));
    }

    #[test]
    fn jpeg_libjpeg_error_is_contained() {
        // Size/data mismatch makes libjpeg read past the end → error, not a crash.
        let bad = Image16 {
            size: Size::new(0, 0),
            data: vec![],
        };
        let r = encode(&bad, ExportFormat::Jpeg { quality: 90 }, icc(), None, None);
        assert!(matches!(r, Err(ExportError::Encode(_))), "{r:?}");
    }

    #[test]
    fn png_8_and_16_roundtrip() {
        let img = gradient(20, 10);
        let dirs = sample().dirs(true);
        for sixteen in [false, true] {
            let fmt = if sixteen {
                ExportFormat::Png16
            } else {
                ExportFormat::Png8
            };
            let bytes = encode(&img, fmt, icc(), Some(&dirs), None).unwrap();
            let mut r = png::Decoder::new(Cursor::new(&bytes)).read_info().unwrap();
            assert_eq!(r.info().icc_profile.as_deref(), Some(icc()));
            assert!(r.info().exif_metadata.is_some());
            let mut buf = vec![0; r.output_buffer_size().unwrap()];
            let fi = r.next_frame(&mut buf).unwrap();
            assert_eq!((fi.width, fi.height), (20, 10));
            if sixteen {
                assert_eq!(fi.bit_depth, png::BitDepth::Sixteen);
                let v = u16::from_be_bytes([buf[114], buf[115]]); // sample 57 = pixel 19, red
                assert_eq!(v, img.data[57]);
            } else {
                assert_eq!(buf[57], img.to_rgb8()[57]);
            }
        }
    }

    #[test]
    fn tiff16_roundtrip_with_exif_and_icc() {
        let img = gradient(33, 7);
        let dirs = sample().dirs(false);
        let bytes = encode(&img, ExportFormat::Tiff16, icc(), Some(&dirs), None).unwrap();
        let mut d = tiff::decoder::Decoder::new(Cursor::new(&bytes)).unwrap();
        assert_eq!(d.dimensions().unwrap(), (33, 7));
        assert_eq!(d.get_tag_u8_vec(Tag::IccProfile).unwrap(), icc());
        match d.read_image().unwrap() {
            tiff::decoder::DecodingResult::U16(v) => assert_eq!(v, img.data),
            _ => panic!("expected u16"),
        }
        // A real EXIF parser follows the Exif and GPS pointers.
        let e = ::exif::Reader::new().read_raw(bytes.clone()).unwrap();
        assert_eq!(
            e.get_field(ETag::LensModel, In::PRIMARY)
                .unwrap()
                .display_value()
                .to_string(),
            "\"FE 24-70mm F2.8 GM\""
        );
        assert!(e.get_field(ETag::GPSLongitude, In::PRIMARY).is_some());
        // No EXIF at all is also valid.
        let plain = encode(&img, ExportFormat::Tiff16, icc(), None, None).unwrap();
        assert!(
            tiff::decoder::Decoder::new(Cursor::new(&plain))
                .unwrap()
                .dimensions()
                .is_ok()
        );
    }

    #[test]
    fn tiff_too_large_rejected() {
        let huge = Image16 {
            size: Size::new(40_000, 40_000),
            data: vec![],
        };
        assert!(matches!(
            encode(&huge, ExportFormat::Tiff16, icc(), None, None),
            Err(ExportError::TooLarge(_))
        ));
    }

    struct FakeHeic;
    impl HeicEncoder for FakeHeic {
        fn encode(
            &self,
            img: &Image16,
            q: u8,
            icc: &[u8],
            exif: Option<&[u8]>,
            keywords: &[String],
            iptc: &IptcMeta,
            xmp: Option<&[u8]>,
        ) -> Result<Vec<u8>, String> {
            if q == 13 {
                return Err("encoder refused".into());
            }
            let rating = xmp
                .map(|x| crate::xmp::tests::xmp_values(x, "xmp:Rating").join(","))
                .unwrap_or_default();
            Ok(format!(
                "{}x{} q{q} icc{} exif{} kw{} rating{rating} title{}",
                img.size.width,
                img.size.height,
                icc.len(),
                exif.map_or(0, <[u8]>::len),
                keywords.join("|"),
                iptc.title
            )
            .into_bytes())
        }
    }

    #[test]
    fn the_hook_gets_the_rating_and_tags() {
        let img = gradient(4, 2);
        let meta = ExifMeta {
            organize: crate::xmp::tests::organized(),
            ..sample()
        };
        let out = encode(
            &img,
            ExportFormat::Heic { quality: 80 },
            icc(),
            Some(&meta.dirs(false)),
            Some(&FakeHeic),
        )
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("kwbeach|Crete & friends|Red|Pick rating4 "),
            "{s}"
        );
    }

    #[test]
    fn jpeg_carries_rating_and_tags_in_xmp_and_leaves_in_iptc() {
        use crate::xmp::tests::{iim_keywords, xmp_values};
        let meta = ExifMeta {
            organize: crate::xmp::tests::organized(),
            ..sample()
        };
        let out = encode(
            &gradient(8, 8),
            ExportFormat::Jpeg { quality: 90 },
            icc(),
            Some(&meta.dirs(false)),
            None,
        )
        .unwrap();
        let xmp = jpeg_segments(&out, 0xE1)
            .into_iter()
            .find(|s| s.starts_with(crate::xmp::XMP_JPEG_HEADER))
            .unwrap()[crate::xmp::XMP_JPEG_HEADER.len()..]
            .to_vec();
        let app13 = jpeg_segments(&out, 0xED).remove(0);
        assert_eq!(xmp_values(&xmp, "xmp:Rating"), ["4"]);
        assert_eq!(
            xmp_values(&xmp, "lr:hierarchicalSubject"),
            [
                "beach",
                "Crete & friends",
                "Darkslide|Label|Red",
                "Darkslide|Pick"
            ]
        );
        assert_eq!(
            iim_keywords(&app13),
            ["beach", "Crete & friends", "Red", "Pick"]
        );
        // As exiftool (what Immich reads files with) sees it, where it is installed.
        if std::process::Command::new("exiftool")
            .arg("-ver")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("x.jpg");
            std::fs::write(&path, &out).unwrap();
            let o = std::process::Command::new("exiftool")
                .args(["-j", "-n", "-XMP:Rating", "-XMP:TagsList", "-IPTC:Keywords"])
                .arg(&path)
                .output()
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
            assert_eq!(v[0]["Rating"], 4);
            assert_eq!(
                v[0]["TagsList"],
                serde_json::json!([
                    "beach",
                    "Crete & friends",
                    "Darkslide/Label/Red",
                    "Darkslide/Pick"
                ])
            );
            assert_eq!(
                v[0]["Keywords"],
                serde_json::json!(["beach", "Crete & friends", "Red", "Pick"])
            );
        }
    }

    #[test]
    fn heic_goes_through_the_hook() {
        let img = gradient(4, 2);
        let dirs = sample().dirs(false);
        let out = encode(
            &img,
            ExportFormat::Heic { quality: 80 },
            icc(),
            Some(&dirs),
            Some(&FakeHeic),
        )
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("4x2 q80 icc") && s.ends_with("title"), "{s}");
        assert!(!s.contains("exif0"), "{s}");
        let titled = ExifMeta {
            iptc: IptcMeta {
                title: "Dawn".into(),
                ..IptcMeta::default()
            },
            ..sample()
        }
        .dirs(false);
        let out = encode(
            &img,
            ExportFormat::Heic { quality: 80 },
            icc(),
            Some(&titled),
            Some(&FakeHeic),
        )
        .unwrap();
        assert!(String::from_utf8(out).unwrap().ends_with("titleDawn"));
        assert!(matches!(
            encode(&img, ExportFormat::Heic { quality: 80 }, icc(), None, None),
            Err(ExportError::NoHeicEncoder)
        ));
        let e = encode(
            &img,
            ExportFormat::Heic { quality: 13 },
            icc(),
            None,
            Some(&FakeHeic),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "encode failed: encoder refused");
    }

    #[test]
    fn native_order_helper() {
        let e = Entry {
            tag: 1,
            ty: FieldType::Short,
            data: vec![1, 0],
        };
        assert_eq!(native(&e), 1u16.to_ne_bytes());
    }
}
