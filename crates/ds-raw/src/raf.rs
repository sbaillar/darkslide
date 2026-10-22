//! Fujifilm RAF: the embedded JPEG, and the sensor sizes bounded before
//! rawler decodes.
//!
//! The 108-byte big-endian header points at three blocks: the JPEG (a whole
//! EXIF JPEG; its TIFF, 12 bytes in, is where rawler reads the metadata), a
//! record list (`count`, then `tag u16, len u16, data`; 0x100 is the raw size
//! as height, width; 0x131 the X-Trans layout) and the sensor data, which
//! starts with a TIFF-style `FujiIFD` (0xF000) holding the raw width and
//! height (0xF001 / 0xF002). Offsets inside each block are relative to it.
//!
//! The decode guard bounds the sizes rawler itself parsed from these blocks
//! (see [`check_decodable`]), not a re-parse of ours.

use ds_core::Orientation;

use std::panic::{AssertUnwindSafe, catch_unwind};

use rawler::decoders::raf::parse_raf_format;
use rawler::formats::tiff::IFD;
use rawler::rawsource::RawSource;

use crate::RawError;
use crate::container::Container;
use crate::guard::{COMPRESSED_PER_BYTE, check, check_tiff};
use crate::preview::{EmbeddedPreview, ifd0_orientation, largest};
use crate::source::{Shifted, SliceSource, Source};

const REC_RAW_SIZE: u16 = 0x100;
const REC_XTRANS_LAYOUT: u16 = 0x131;
const FUJI_IFD: u16 = 0xF000;
const FUJI_WIDTH: u16 = 0xF001;
const FUJI_HEIGHT: u16 = 0xF002;

/// Why an X-Trans file isn't developed.
pub(crate) const XTRANS: &str =
    "X-Trans sensor (6×6 colour filter); Darkslide demosaics 2×2 Bayer sensors only";

/// The header fields: `(offset, length)` of the JPEG, the record list and the
/// sensor data, and the camera name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Header {
    jpeg: (u64, u64),
    records: u64,
    data: u64,
    model: String,
}

impl Header {
    fn parse(src: &dyn Source) -> Result<Self, RawError> {
        let h = src
            .read(0, 108)
            .map_err(|_| RawError::Truncated("RAF header".into()))?;
        let be = |i: usize| u64::from(u32::from_be_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]));
        let name = &h[28..60];
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        Ok(Self {
            jpeg: (be(84), be(88)),
            records: be(92),
            data: be(100),
            model: String::from_utf8_lossy(&name[..end]).trim().to_string(),
        })
    }
}

/// The TIFF inside the JPEG's APP1 `Exif` block, rebased to its header.
fn jpeg_tiff<'a>(src: &'a dyn Source, h: &Header) -> Option<(Shifted<'a>, Container)> {
    let at = h.jpeg.0;
    if src.read(at + 6, 6).ok()?.as_ref() != b"Exif\0\0" {
        return None;
    }
    let sh = Shifted {
        inner: src,
        delta: at + 12,
    };
    let c = Container::parse(&sh).ok()?;
    Some((sh, c))
}

/// The embedded JPEG, with the orientation from its own EXIF.
pub(crate) fn preview(src: &dyn Source) -> Result<EmbeddedPreview, RawError> {
    let h = Header::parse(src)?;
    let orientation =
        jpeg_tiff(src, &h).map_or(Orientation::Normal, |(sh, c)| ifd0_orientation(&sh, &c));
    largest(src, vec![h.jpeg], orientation)
}

/// Bound the TIFF 12 bytes into the JPEG (where rawler reads the metadata,
/// Exif marker or not) before rawler's header parser loads it: see
/// [`crate::guard::check_tiff`]. One our walker can't parse is refused.
pub(crate) fn check_header(src: &dyn Source) -> Result<(), RawError> {
    let h = Header::parse(src)?;
    let sh = Shifted {
        inner: src,
        delta: h.jpeg.0 + 12,
    };
    let c =
        Container::parse(&sh).map_err(|e| RawError::Malformed(format!("RAF JPEG TIFF: {e}")))?;
    check_tiff(&sh, &c)
}

/// Every IFD of `ifd`'s tree: itself, its sub-IFDs and its chain (what
/// rawler's `find_first_ifd_with_tag` searches).
fn each_ifd<'a>(ifd: &'a IFD, out: &mut Vec<&'a IFD>) {
    out.push(ifd);
    for subs in ifd.sub_ifds().values() {
        for s in subs {
            each_ifd(s, out);
        }
    }
    for c in &ifd.chain {
        each_ifd(c, out);
    }
}

/// The trees rawler's RAF decoder builds (`decoders::raf::parse_raf`), parsed
/// by rawler itself: the JPEG's TIFF (12 bytes into the JPEG), the `FujiIFD`
/// tree at the sensor data (it fakes one when that doesn't parse) and the
/// record list (ignored when it doesn't parse).
struct Parsed {
    main: IFD,
    fuji: Option<IFD>,
    records: Option<IFD>,
}

fn parse(rs: &RawSource, h: &Header) -> Result<Parsed, RawError> {
    let u32_of = |v: u64, what: &str| {
        u32::try_from(v).map_err(|_| RawError::Malformed(format!("RAF {what} offset {v}")))
    };
    let main_at = u32_of(h.jpeg.0 + 12, "JPEG")?;
    let (data_at, rec_at) = (u32_of(h.data, "data")?, u32_of(h.records, "record")?);
    let parsed = catch_unwind(AssertUnwindSafe(|| {
        let mut r = rs.reader();
        let main = IFD::new_root(&mut r, main_at);
        let fuji = IFD::new_root_with_correction(&mut r, 0, data_at, 0, 10, &[FUJI_IFD]).ok();
        let records = parse_raf_format(rs, rec_at).ok();
        (main, fuji, records)
    }));
    match parsed {
        Ok((Ok(main), fuji, records)) => Ok(Parsed {
            main,
            fuji,
            records,
        }),
        // rawler can't open the file either: nothing to decode.
        Ok((Err(e), ..)) => Err(RawError::Malformed(format!("RAF JPEG TIFF: {e}"))),
        Err(_) => Err(RawError::Malformed("RAF header parser panicked".into())),
    }
}

/// Bound every raw size rawler may allocate from, as rawler parsed it: the
/// record list's 0x100 (height, width) and every 0xF001 × 0xF002 pair in the
/// JPEG's TIFF and the `FujiIFD` tree (rawler takes the first IFD there with
/// `StripOffsets`). Taking the values from rawler's own structures leaves no
/// room for a second parser to disagree. Then refuse X-Trans files (record
/// 0x131), whose 6×6 filter Darkslide can't demosaic, before a full decode
/// is spent on them.
pub(crate) fn check_decodable(rs: &RawSource) -> Result<(), RawError> {
    let buf = rs.buf();
    let len = buf.len() as u64;
    let h = Header::parse(&SliceSource::new(buf))?;
    let p = parse(rs, &h)?;
    let mut ifds = Vec::new();
    each_ifd(&p.main, &mut ifds);
    if let Some(f) = &p.fuji {
        each_ifd(f, &mut ifds);
    }
    for ifd in ifds {
        if let (Some(w), Some(hh)) = (ifd.get_entry(FUJI_WIDTH), ifd.get_entry(FUJI_HEIGHT)) {
            let samples =
                (w.value.force_usize(0) as u64).saturating_mul(hh.value.force_usize(0) as u64);
            check("RAF FujiIFD", samples, COMPRESSED_PER_BYTE, len)?;
        }
    }
    if let Some(rec) = &p.records {
        if let Some(e) = rec.get_entry(REC_RAW_SIZE) {
            let samples =
                (e.value.force_usize(0) as u64).saturating_mul(e.value.force_usize(1) as u64);
            check("RAF raw size", samples, COMPRESSED_PER_BYTE, len)?;
        }
        if rec.get_entry(REC_XTRANS_LAYOUT).is_some() {
            return Err(RawError::Unsupported {
                make: "FUJIFILM".into(),
                model: h.model,
                reason: XTRANS.into(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::preview::tests::fake_jpeg;
    use crate::source::SliceSource;
    use crate::tiff::build::{Val, header, ifd};
    use crate::tiff::{Endian, tag};
    use ds_core::Size;

    /// An EXIF JPEG whose TIFF IFD0 has `entries`.
    fn exif_jpeg(entries: &[(u16, Val)]) -> Vec<u8> {
        let mut tiff = header(Endian::Little);
        tiff.extend(ifd(Endian::Little, 8, entries, 0));
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE1];
        j.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        j.extend_from_slice(b"Exif\0\0");
        j.extend(tiff);
        j.extend_from_slice(&fake_jpeg(1920, 1280, 0)[2..]);
        j
    }

    /// A RAF: header, the JPEG, the record list, then a `FujiIFD` block and
    /// `pad` bytes of sensor data.
    pub fn raf(
        orientation: u16,
        records: &[(u16, Vec<u8>)],
        fuji: Option<(u32, u32)>,
        pad: usize,
    ) -> Vec<u8> {
        let exif = [(tag::ORIENTATION, Val::Short(vec![orientation]))];
        raf_with(&exif, records, fuji, pad)
    }

    fn raf_with(
        exif: &[(u16, Val)],
        records: &[(u16, Vec<u8>)],
        fuji: Option<(u32, u32)>,
        pad: usize,
    ) -> Vec<u8> {
        let mut rec = (records.len() as u32).to_be_bytes().to_vec();
        for (t, d) in records {
            rec.extend_from_slice(&t.to_be_bytes());
            rec.extend_from_slice(&(d.len() as u16).to_be_bytes());
            rec.extend_from_slice(d);
        }
        let mut data = header(Endian::Little);
        if let Some((w, h)) = fuji {
            let sub_at = 8 + 2 + 12 + 4;
            data.extend(ifd(
                Endian::Little,
                8,
                &[(FUJI_IFD, Val::Long(vec![sub_at]))],
                0,
            ));
            data.extend(ifd(
                Endian::Little,
                sub_at,
                &[
                    (FUJI_WIDTH, Val::Long(vec![w])),
                    (FUJI_HEIGHT, Val::Long(vec![h])),
                ],
                0,
            ));
        }
        data.resize(data.len() + pad, 0);
        raf_raw(&exif_jpeg(exif), &rec, &data)
    }

    /// Header, then the three blocks as given.
    fn raf_raw(jpeg: &[u8], rec: &[u8], data: &[u8]) -> Vec<u8> {
        let (j_at, r_at) = (108, 108 + jpeg.len());
        let d_at = r_at + rec.len();
        let mut f = b"FUJIFILMCCD-RAW 0201FF159505X-T4".to_vec();
        f.resize(84, 0);
        for v in [j_at, jpeg.len(), r_at, rec.len(), d_at, data.len()] {
            f.extend_from_slice(&(v as u32).to_be_bytes());
        }
        f.extend_from_slice(jpeg);
        f.extend_from_slice(rec);
        f.extend_from_slice(data);
        f
    }

    fn decodable(f: &[u8]) -> Result<(), RawError> {
        check_decodable(&RawSource::new_from_slice(f))
    }

    fn refused(f: &[u8], what: &str) -> bool {
        matches!(decodable(f), Err(RawError::Malformed(m)) if m.contains(what))
    }

    fn size(h: u16, w: u16) -> (u16, Vec<u8>) {
        (REC_RAW_SIZE, [h.to_be_bytes(), w.to_be_bytes()].concat())
    }

    /// The JPEG's TIFF is bounded before rawler reads metadata from it.
    #[test]
    fn header_guard_bounds_the_jpeg_tiff() {
        let f = raf(1, &[size(40, 60)], None, 0);
        assert!(check_header(&SliceSource::new(&f)).is_ok());
        // IFD0's first entry (at 108 + 12 + 8 + 2) claims 2^30 SHORTs.
        let mut big = f.clone();
        big[130 + 4..130 + 8].copy_from_slice(&0x4000_0000u32.to_le_bytes());
        assert!(matches!(
            check_header(&SliceSource::new(&big)),
            Err(RawError::Malformed(m)) if m.contains("value bytes")
        ));
        assert!(matches!(
            crate::read_metadata_bytes(&big),
            Err(RawError::Malformed(_))
        ));
        // No TIFF there at all.
        let mut none = f;
        none[120..124].copy_from_slice(b"XXXX");
        assert!(matches!(
            check_header(&SliceSource::new(&none)),
            Err(RawError::Malformed(m)) if m.starts_with("RAF JPEG TIFF")
        ));
    }

    #[test]
    fn preview_is_the_jpeg_with_its_exif_orientation() {
        let f = raf(6, &[size(40, 60)], None, 0);
        let src = SliceSource::new(&f);
        let p = preview(&src).unwrap();
        assert_eq!(p.size, Size::new(1920, 1280));
        assert_eq!(p.orientation, Orientation::from_exif(6));
        assert_eq!(Header::parse(&src).unwrap().model, "X-T4");
        // A JPEG without an Exif block: Normal.
        let mut f = f;
        f[108 + 6] = b'X';
        assert_eq!(
            preview(&SliceSource::new(&f)).unwrap().orientation,
            Orientation::Normal
        );
        assert!(matches!(
            preview(&SliceSource::new(&f[..100])),
            Err(RawError::Truncated(_))
        ));
    }

    #[test]
    fn sizes_are_bounded_and_xtrans_refused() {
        let ok = raf(
            1,
            &[size(40, 60), (0x9650, vec![0; 4])],
            Some((60, 40)),
            4800,
        );
        assert!(decodable(&ok).is_ok());
        assert!(refused(
            &raf(1, &[size(65_535, 65_535)], None, 0),
            "RAF raw size"
        ));
        assert!(refused(
            &raf(1, &[size(40, 60)], Some((65_535, 65_535)), 0),
            "RAF FujiIFD"
        ));
        let x = raf(
            1,
            &[size(40, 60), (REC_XTRANS_LAYOUT, vec![1; 36])],
            None,
            4800,
        );
        assert!(matches!(
            decodable(&x),
            Err(RawError::Unsupported { model, reason, .. })
                if model == "X-T4" && reason == XTRANS
        ));
    }

    #[test]
    fn jpeg_tiff_sizes_are_bounded_too() {
        // The JPEG's IFD0 carries a FujiIFD-style pair claiming 65535².
        let exif = [
            (FUJI_WIDTH, Val::Long(vec![65_535])),
            (FUJI_HEIGHT, Val::Long(vec![65_535])),
        ];
        assert!(refused(&raf_with(&exif, &[], None, 0), "FujiIFD"));
    }

    /// Review H1: an odd-length record shifts rawler's walk of the record list
    /// by a byte (it reads `len / 2` shorts and doesn't skip the rest), so it
    /// sees a 0x100 of 65535² that a walk of our own never would.
    #[test]
    fn record_desync_is_bounded_as_rawler_reads_it() {
        let mut rec = 2u32.to_be_bytes().to_vec();
        rec.extend_from_slice(&[0x01, 0x10, 0x00, 0x03, 0x00, 0x00, 0x01]); // 0x110, len 3
        rec.extend_from_slice(&[0x00, 0x00, 0x04, 0xFF, 0xFF, 0xFF, 0xFF]);
        let jpeg = exif_jpeg(&[(tag::ORIENTATION, Val::Short(vec![1]))]);
        assert!(refused(&raf_raw(&jpeg, &rec, &[0u8; 64]), "RAF raw size"));
    }

    /// Review H2: a `FujiIFD` (0xF000) listing two IFDs; rawler parses both and
    /// uses whichever has `StripOffsets`.
    #[test]
    fn every_fuji_ifd_is_bounded() {
        let mut data = header(Endian::Little);
        data.extend(ifd(
            Endian::Little,
            8,
            &[(FUJI_IFD, Val::Long(vec![40, 80]))],
            0,
        ));
        data.resize(40, 0);
        data.extend(ifd(
            Endian::Little,
            40,
            &[
                (FUJI_WIDTH, Val::Long(vec![60])),
                (FUJI_HEIGHT, Val::Long(vec![40])),
            ],
            0,
        ));
        data.resize(80, 0);
        data.extend(ifd(
            Endian::Little,
            80,
            &[
                (FUJI_WIDTH, Val::Long(vec![60_000])),
                (FUJI_HEIGHT, Val::Long(vec![60_000])),
                (0xF007, Val::Long(vec![0])),
            ],
            0,
        ));
        data.resize(256, 0);
        let jpeg = exif_jpeg(&[(tag::ORIENTATION, Val::Short(vec![1]))]);
        assert!(refused(
            &raf_raw(&jpeg, &0u32.to_be_bytes(), &data),
            "RAF FujiIFD"
        ));
    }

    /// Review H3: without the `Exif` magic rawler still parses the TIFF 12 bytes
    /// into the JPEG; when that TIFF doesn't parse, rawler can't open the file.
    #[test]
    fn the_jpeg_tiff_is_bounded_however_it_looks() {
        let mut j = exif_jpeg(&[
            (FUJI_WIDTH, Val::Long(vec![60_000])),
            (FUJI_HEIGHT, Val::Long(vec![60_000])),
            (0xF007, Val::Long(vec![0])),
        ]);
        j[6..12].copy_from_slice(b"Exxx\0\0");
        assert!(refused(
            &raf_raw(&j, &0u32.to_be_bytes(), &[0u8; 64]),
            "RAF FujiIFD"
        ));
        let mut j = exif_jpeg(&[(tag::ORIENTATION, Val::Short(vec![1]))]);
        j[12..14].copy_from_slice(b"XX");
        assert!(refused(
            &raf_raw(&j, &0u32.to_be_bytes(), &[0u8; 64]),
            "RAF JPEG TIFF"
        ));
        // Offsets rawler adds in 32 bits.
        let mut f = raf(1, &[], None, 0);
        f[84..88].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(refused(&f, "JPEG offset"));
        assert!(matches!(decodable(&f[..100]), Err(RawError::Truncated(_))));
    }

    #[test]
    fn missing_blocks_are_what_rawler_makes_of_them() {
        // No record list, no FujiIFD: nothing to bound, rawler fails later.
        let mut f = raf(1, &[], None, 0);
        f[92..96].copy_from_slice(&u32::MAX.to_be_bytes());
        f[100..104].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decodable(&f).is_ok());
        let mut ifds = Vec::new();
        let p = parse(
            &RawSource::new_from_slice(&f),
            &Header::parse(&SliceSource::new(&f)).unwrap(),
        )
        .unwrap();
        assert!(p.fuji.is_none() && p.records.is_none());
        each_ifd(&p.main, &mut ifds);
        assert_eq!(ifds.len(), 1);
    }
}
