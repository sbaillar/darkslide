//! Bounds checks that must pass before a file is handed to rawler.
//!
//! rawler sizes its output buffers from header values before it reads any
//! pixel data, and an allocation failure aborts the process instead of
//! unwinding (so `catch_unwind` cannot help). Every format we pass to rawler
//! therefore has its declared dimensions parsed and bounded here first;
//! formats whose dimensions we do not parse are refused for decoding.
//!
//! Limits, in samples (pixels × components):
//! - never more than [`MAX_SAMPLES`] (1 Gi, a 2 GiB `u16` buffer);
//! - uncompressed / bit-packed data stores ≥ 4 bits per sample, so at most
//!   [`UNPACKED_PER_BYTE`] samples per file byte;
//! - compressed data (lossless Huffman/LJPEG, vendor codecs) at most
//!   [`COMPRESSED_PER_BYTE`] samples per file byte. Lossless dark frames
//!   reach 4–6; 32 leaves a wide margin while still turning a 4 KB file's
//!   claim into at most 128 K samples.

use crate::RawError;
use crate::container::Container;
use crate::preview::jpeg_frame;
use crate::source::Source;
use crate::tiff::tag;

pub(crate) const MAX_SAMPLES: u64 = 1 << 30;
pub(crate) const UNPACKED_PER_BYTE: u64 = 2;
pub(crate) const COMPRESSED_PER_BYTE: u64 = 32;
/// How much of a JPEG/LJPEG stream is scanned for its frame header.
const SOF_SCAN: u64 = 64 * 1024;

/// RAW container families by signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    /// Classic TIFF (`II*\0` / `MM\0*`): ARW, NEF, CR2, DNG, PEF, …
    Tiff,
    /// TIFF structure with another magic: ORF (`IIRO`/`IIRS`/`MMOR`), RW2 (`IIU\0`).
    TiffVariant,
    /// Minolta MRW (`\0MRM`).
    Mrw,
    /// Not TIFF-structured: CR3 and RAF (bounded by `cr3` / `raf` before
    /// decoding), and CRW, X3F, ARRI, IIQ, whose dimensions we do not parse:
    /// metadata only, decoding is refused.
    Unbounded(&'static str),
    NotRaw,
}

impl Format {
    pub fn sniff(h: &[u8]) -> Self {
        let at = |i: usize, s: &[u8]| h.get(i..i + s.len()) == Some(s);
        if at(0, b"II*\0") || at(0, b"MM\0*") {
            Self::Tiff
        } else if at(0, b"IIRO") || at(0, b"IIRS") || at(0, b"MMOR") || at(0, b"IIU\0") {
            Self::TiffVariant
        } else if at(0, b"\0MRM") {
            Self::Mrw
        } else if at(0, b"FUJIFILM") {
            Self::Unbounded("RAF")
        } else if at(4, b"ftyp") && (at(8, b"crx ") || at(8, b"CRX ")) {
            Self::Unbounded("CR3")
        } else if at(0, b"II\x1a\0") && at(6, b"HEAPCCDR") {
            Self::Unbounded("CRW")
        } else if at(0, b"FOVb") {
            Self::Unbounded("X3F")
        } else if at(0, b"ARRI") {
            Self::Unbounded("ARRI")
        } else if at(0, b"IIII") {
            Self::Unbounded("IIQ")
        } else {
            Self::NotRaw
        }
    }

    pub fn is_raw(self) -> bool {
        self != Self::NotRaw
    }

    /// Short name for messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Tiff => "TIFF",
            Self::TiffVariant => "ORF/RW2",
            Self::Mrw => "MRW",
            Self::Unbounded(n) => n,
            Self::NotRaw => "this file",
        }
    }
}

/// `w * h * spp` saturating at `u64::MAX`, so a wrapped product can never
/// slip under the limit (release builds would otherwise wrap silently).
fn samples(w: u64, h: u64, spp: u64) -> u64 {
    w.saturating_mul(h).saturating_mul(spp)
}

pub(crate) fn check(
    what: &str,
    samples: u64,
    per_byte: u64,
    file_len: u64,
) -> Result<(), RawError> {
    let limit = MAX_SAMPLES.min(file_len.saturating_mul(per_byte));
    if samples > limit {
        return Err(RawError::Malformed(format!(
            "{what} claims {samples} samples; a {file_len}-byte file allows at most {limit}"
        )));
    }
    Ok(())
}

/// Bound every size a TIFF-family decoder in rawler may allocate from:
/// `ImageWidth × ImageLength × SamplesPerPixel` of every IFD, Panasonic's
/// sensor size tags, and the frame header of JPEG/LJPEG-compressed strips
/// and tiles (CR2 takes its size from there). Also refuses IFD graphs our
/// walker had to cut short and tag tables declaring far more value bytes
/// than the file holds, since rawler loads all of those eagerly.
pub(crate) fn check_tiff(src: &dyn Source, c: &Container) -> Result<(), RawError> {
    let len = src.end();
    if c.overflow {
        return Err(RawError::Malformed(
            "IFD graph exceeds walker limits".into(),
        ));
    }
    let declared = c.declared_value_bytes();
    if declared > len.saturating_mul(4).saturating_add(1 << 20) {
        return Err(RawError::Malformed(format!(
            "tags declare {declared} value bytes in a {len}-byte file"
        )));
    }
    for ifd in c.all() {
        let u = |t| ifd.uint(src, t).ok().flatten();
        let compression = u(tag::COMPRESSION);
        let per_byte = if compression == Some(1) {
            UNPACKED_PER_BYTE
        } else {
            COMPRESSED_PER_BYTE
        };
        let spp = u64::from(u(tag::SAMPLES_PER_PIXEL).unwrap_or(1).max(1));
        for (wt, ht) in [
            (tag::IMAGE_WIDTH, tag::IMAGE_LENGTH),
            (tag::PANA_WIDTH, tag::PANA_LENGTH),
        ] {
            if let (Some(w), Some(h)) = (u(wt), u(ht)) {
                check("IFD", samples(w.into(), h.into(), spp), per_byte, len)?;
            }
        }
        if matches!(compression, Some(6 | 7 | 99)) {
            let off = u(tag::STRIP_OFFSETS).or_else(|| u(tag::TILE_OFFSETS));
            if let Some(off) = off.map(u64::from).filter(|&o| o < len) {
                let head = src.read(off, SOF_SCAN.min(len - off))?;
                if let Some((w, h, nc)) = jpeg_frame(&head) {
                    let n = samples(w.into(), h.into(), nc.max(1).into());
                    check("JPEG frame", n, COMPRESSED_PER_BYTE, len)?;
                }
            }
        }
    }
    Ok(())
}

/// Bound Minolta MRW: the `PRD` block's sensor height/width (big-endian u16
/// at +16/+18 of the block, which is how rawler reads them). MRW data is
/// packed or unpacked 12-bit.
pub(crate) fn check_mrw(src: &dyn Source) -> Result<(), RawError> {
    let len = src.end();
    let be32 = |b: &[u8], o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let head = src.read(0, 8.min(len))?;
    if head.len() < 8 {
        return Err(RawError::Truncated("MRW header".into()));
    }
    let data_off = (u64::from(be32(&head, 4)) + 8).min(len);
    let mut pos = 8u64;
    for _ in 0..64 {
        if pos + 20 > data_off {
            break;
        }
        let blk = src.read(pos, 20)?;
        let (tag, blen) = (be32(&blk, 0), be32(&blk, 4));
        if tag == 0x0050_5244 {
            // "\0PRD"
            let h = u16::from_be_bytes([blk[16], blk[17]]);
            let w = u16::from_be_bytes([blk[18], blk[19]]);
            return check(
                "MRW PRD",
                u64::from(w) * u64::from(h),
                UNPACKED_PER_BYTE,
                len,
            );
        }
        pos = pos.saturating_add(8).saturating_add(u64::from(blen));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SliceSource;

    #[test]
    fn sniff_formats() {
        assert_eq!(Format::sniff(b"II*\0\x08\0\0\0"), Format::Tiff);
        assert_eq!(Format::sniff(b"MM\0*\0\0\0\x08"), Format::Tiff);
        assert_eq!(Format::sniff(b"IIRO\x08\0\0\0"), Format::TiffVariant);
        assert_eq!(Format::sniff(b"IIRS\x08\0\0\0"), Format::TiffVariant);
        assert_eq!(Format::sniff(b"MMOR\0\0\0\x08"), Format::TiffVariant);
        assert_eq!(Format::sniff(b"IIU\0\x08\0\0\0"), Format::TiffVariant);
        assert_eq!(Format::sniff(b"\0MRM\0\0\0\x10"), Format::Mrw);
        assert_eq!(Format::sniff(b"FUJIFILMCCD-RAW "), Format::Unbounded("RAF"));
        assert_eq!(
            Format::sniff(b"\0\0\0\x18ftypcrx \0\0\0\x01"),
            Format::Unbounded("CR3")
        );
        assert_eq!(
            Format::sniff(b"II\x1a\0\0\0HEAPCCDR"),
            Format::Unbounded("CRW")
        );
        assert_eq!(Format::sniff(b"FOVb\0\0\0\0"), Format::Unbounded("X3F"));
        assert_eq!(
            Format::sniff(b"ARRI\x12\x34\x56\x78"),
            Format::Unbounded("ARRI")
        );
        assert_eq!(Format::sniff(b"IIII\x01\0\0\0"), Format::Unbounded("IIQ"));
        assert_eq!(Format::sniff(b"II\x1a\0\0\0NOTCANON"), Format::NotRaw);
        assert_eq!(Format::sniff(b"\0\0\0\x18ftypheic"), Format::NotRaw);
        assert_eq!(Format::sniff(b"\xFF\xD8\xFF\xE0"), Format::NotRaw);
        assert_eq!(Format::sniff(b""), Format::NotRaw);
        assert!(!Format::NotRaw.is_raw());
        for (f, n) in [
            (Format::Tiff, "TIFF"),
            (Format::TiffVariant, "ORF/RW2"),
            (Format::Mrw, "MRW"),
            (Format::Unbounded("X3F"), "X3F"),
            (Format::NotRaw, "this file"),
        ] {
            assert_eq!(f.name(), n);
        }
        assert!(Format::Mrw.is_raw());
    }

    #[test]
    fn check_limits() {
        assert!(check("x", 100, 2, 50).is_ok());
        assert!(matches!(
            check("x", 101, 2, 50),
            Err(RawError::Malformed(_))
        ));
        // Absolute cap regardless of file size.
        assert!(check("x", MAX_SAMPLES + 1, 32, u64::MAX / 64).is_err());
        assert!(check("x", MAX_SAMPLES, 32, u64::MAX / 64).is_ok());
    }

    fn mrw(h: u16, w: u16, data_off: u32) -> Vec<u8> {
        let mut f = b"\0MRM".to_vec();
        f.extend_from_slice(&data_off.to_be_bytes());
        // A non-PRD block first, then PRD.
        f.extend_from_slice(b"\0TTW");
        f.extend_from_slice(&4u32.to_be_bytes());
        f.extend_from_slice(&[0; 4]);
        f.extend_from_slice(b"\0PRD");
        f.extend_from_slice(&24u32.to_be_bytes());
        f.extend_from_slice(b"21860002"); // version
        f.extend_from_slice(&h.to_be_bytes());
        f.extend_from_slice(&w.to_be_bytes());
        f.extend_from_slice(&[0; 16]);
        f
    }

    #[test]
    fn mrw_prd_is_bounded() {
        let mut f = mrw(64, 64, 60);
        f.resize(64 * 64 * 2, 0);
        assert!(check_mrw(&SliceSource::new(&f)).is_ok());
        // The reviewer's case: 65535² claimed in a 4 KB file.
        let mut f = mrw(65535, 65535, 60);
        f.resize(4096, 0);
        assert!(matches!(
            check_mrw(&SliceSource::new(&f)),
            Err(RawError::Malformed(m)) if m.contains("MRW PRD")
        ));
        // No PRD before data offset: nothing to bound here (rawler uses its DB size).
        let f = mrw(65535, 65535, 20);
        assert!(check_mrw(&SliceSource::new(&f)).is_ok());
        // Short file.
        assert!(matches!(
            check_mrw(&SliceSource::new(b"\0MRM")),
            Err(RawError::Truncated(_))
        ));
        // Block length that would overflow the cursor just ends the walk.
        let mut f = b"\0MRM\xFF\xFF\xFF\xF0".to_vec();
        f.extend_from_slice(b"\0XXX\xFF\xFF\xFF\xFF");
        f.resize(64, 0);
        assert!(check_mrw(&SliceSource::new(&f)).is_ok());
    }

    /// Little-endian TIFF with one IFD holding the given `(tag, type, value)`
    /// entries (type 3 = SHORT, 4 = LONG), padded to `total` bytes.
    fn tiff(entries: &[(u16, u16, u32)], total: usize) -> Vec<u8> {
        let mut f = b"II*\0\x08\0\0\0".to_vec();
        f.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for &(t, ty, v) in entries {
            f.extend_from_slice(&t.to_le_bytes());
            f.extend_from_slice(&ty.to_le_bytes());
            f.extend_from_slice(&1u32.to_le_bytes());
            f.extend_from_slice(&v.to_le_bytes());
        }
        f.extend_from_slice(&0u32.to_le_bytes());
        f.resize(total, 0);
        f
    }

    fn guard(f: &[u8]) -> Result<(), RawError> {
        let src = SliceSource::new(f);
        check_tiff(&src, &Container::parse(&src)?)
    }

    #[test]
    fn dimension_product_cannot_wrap() {
        // 2^31 x 2^31 x 4 = 2^64 wraps to 0 in an unchecked u64 product.
        let f = tiff(
            &[
                (tag::IMAGE_WIDTH, 4, 0x8000_0000),
                (tag::IMAGE_LENGTH, 4, 0x8000_0000),
                (tag::SAMPLES_PER_PIXEL, 3, 4),
                (tag::COMPRESSION, 3, 1),
            ],
            4096,
        );
        assert!(matches!(guard(&f), Err(RawError::Malformed(_))));
        assert_eq!(samples(u64::MAX, 2, 2), u64::MAX);
        // An honest small image still passes.
        let f = tiff(
            &[
                (tag::IMAGE_WIDTH, 4, 32),
                (tag::IMAGE_LENGTH, 4, 32),
                (tag::COMPRESSION, 3, 1),
            ],
            4096,
        );
        assert!(guard(&f).is_ok());
    }

    /// rawler's header parsers see only what the decode guards bounded,
    /// for metadata too.
    #[test]
    fn metadata_is_guarded_like_decoding() {
        // ORF magic over a TIFF whose first tag claims 2^30 ASCII bytes.
        let mut f = tiff(&[(tag::IMAGE_WIDTH, 4, 32)], 4096);
        f[..4].copy_from_slice(b"IIRO");
        f[14..18].copy_from_slice(&0x4000_0000u32.to_le_bytes());
        f[12..14].copy_from_slice(&2u16.to_le_bytes());
        assert!(matches!(
            crate::read_metadata_bytes(&f),
            Err(RawError::Malformed(m)) if m.contains("value bytes")
        ));
        // One our walker couldn't parse at open is not handed to rawler either.
        let mut f = b"IIRO\xff\xff\xff\x7f".to_vec();
        f.resize(64, 0);
        assert!(crate::RawFile::from_bytes(&f).unwrap().container.is_none());
        assert!(crate::read_metadata_bytes(&f).is_err());
        // Formats whose headers we do not bound are not parsed.
        for magic in [
            &b"FOVb"[..],
            b"ARRI\x12\x34\x56\x78",
            b"IIII\x01\0\0\0",
            b"II\x1a\0\0\0HEAPCCDR",
        ] {
            let mut f = magic.to_vec();
            f.resize(256, 0);
            assert!(matches!(
                crate::read_metadata_bytes(&f),
                Err(RawError::Unsupported { reason, .. }) if reason.contains("not bounds-checked")
            ));
        }
    }

    mod fuzz {
        use super::*;
        use proptest::prelude::*;

        /// Fixed seed and no persistence file: the same inputs on every run.
        fn config() -> ProptestConfig {
            ProptestConfig {
                cases: 512,
                rng_seed: proptest::test_runner::RngSeed::Fixed(1_592_590_337),
                failure_persistence: None,
                ..ProptestConfig::default()
            }
        }

        proptest! {
            #![proptest_config(config())]
            /// Any bytes behind a TIFF header: parse and guard never panic,
            /// and a pass means every IFD's claim fits the limits (u128 math).
            #[test]
            fn check_tiff_total_and_sound(
                body in proptest::collection::vec(any::<u8>(), 0..512),
                le in any::<bool>(),
            ) {
                let mut f = if le { b"II*\0".to_vec() } else { b"MM\0*".to_vec() };
                f.extend_from_slice(&body);
                let src = SliceSource::new(&f);
                if let Ok(c) = Container::parse(&src)
                    && check_tiff(&src, &c).is_ok()
                {
                    for ifd in c.all() {
                        let u = |t| ifd.uint(&src, t).ok().flatten();
                        if let (Some(w), Some(h)) = (u(tag::IMAGE_WIDTH), u(tag::IMAGE_LENGTH)) {
                            let spp = u128::from(u(tag::SAMPLES_PER_PIXEL).unwrap_or(1).max(1));
                            prop_assert!(u128::from(w) * u128::from(h) * spp <= u128::from(MAX_SAMPLES));
                        }
                    }
                }
            }

            /// Hostile dimensions in a well-formed IFD never wrap past the guard.
            #[test]
            fn huge_dimensions_rejected(w in any::<u32>(), h in any::<u32>(), spp in 1u32..=8) {
                let f = tiff(
                    &[
                        (tag::IMAGE_WIDTH, 4, w),
                        (tag::IMAGE_LENGTH, 4, h),
                        (tag::SAMPLES_PER_PIXEL, 3, spp),
                        (tag::COMPRESSION, 3, 1),
                    ],
                    4096,
                );
                let want = u128::from(w) * u128::from(h) * u128::from(spp);
                if want > u128::from(2 * 4096u64) {
                    prop_assert!(guard(&f).is_err());
                }
            }

            #[test]
            fn container_parse_is_total(body in proptest::collection::vec(any::<u8>(), 0..2048)) {
                let _ = Container::parse(&SliceSource::new(&body));
                let mut f = b"II*\0\x08\0\0\0".to_vec();
                f.extend_from_slice(&body);
                let _ = Container::parse(&SliceSource::new(&f));
            }
        }
    }
}
