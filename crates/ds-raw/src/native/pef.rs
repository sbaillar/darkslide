//! Pentax PEF (TIFF compression 65535): the same two-columns-left Huffman
//! scheme as Nikon's ([`super::nef::two_left`], predictors starting at 0),
//! with the Huffman code stored in the maker note's 0x0220 entry: a 14-byte
//! header whose byte 4 is the code width `b`, then `n` codes (`u16`,
//! left-aligned in `b` bits, maker note byte order) and their `n` lengths,
//! symbol `i` being the difference size `i`. Layout read off the corpus
//! file and checked by decoding it whole against rawler (docs/RAWDEC.md).
//!
//! The maker note (`PENTAX \0`, byte order, then an IFD with offsets from
//! the maker note's start) also gives the black level (0x0200, R G G B),
//! the as-shot WB (0x0201, R G G B) and the white level (0x007E). Files
//! without a code table (older bodies use a fixed one) go to the fallback.

use ds_core::CfaPattern;

use super::ljpeg::Huffman;
use super::nef::two_left;
use super::{Sensor, from_margins, orientation};
use crate::RawError;
use crate::cameras;
use crate::container::Container;
use crate::source::{Shifted, Source};
use crate::tiff::{Endian, Ifd, tag};

const COMPRESSION_PENTAX: u32 = 65535;
/// Maker note tags.
const WHITE_LEVEL: u16 = 0x007E;
const BLACK_POINT: u16 = 0x0200;
const WB_LEVELS: u16 = 0x0201;
const HUFFMAN_TABLE: u16 = 0x0220;

/// The maker note IFD, read through a source whose offset 0 is its start.
fn maker_note<'a>(src: &'a dyn Source, c: &Container) -> Option<(Shifted<'a>, Ifd)> {
    let exif = c.exif.as_ref()?;
    let e = exif.get(tag::MAKER_NOTE)?;
    let off = u64::from(exif.endian.u32(e.field));
    let head = src.read(off, 10).ok()?;
    if &head[..8] != b"PENTAX \0" {
        return None;
    }
    let endian = match &head[8..10] {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => return None,
    };
    let sh = Shifted {
        inner: src,
        delta: off,
    };
    let ifd = Ifd::parse(&sh, 10, endian).ok()?;
    Some((sh, ifd))
}

/// The Huffman code of 0x0220 as a canonical table; `None` for a layout
/// we don't recognize (or codes that aren't canonical).
pub(crate) fn huffman(b: &[u8], endian: Endian) -> Option<Huffman> {
    let n = b.len().checked_sub(14)? / 3;
    let width = u32::from(*b.get(4)?);
    if b.len() != 14 + 3 * n || !(1..=17).contains(&n) || !(1..=16).contains(&width) {
        return None;
    }
    let mut codes = Vec::with_capacity(n);
    for i in 0..n {
        let len = u32::from(b[14 + 2 * n + i]);
        if !(1..=width).contains(&len) {
            return None;
        }
        let raw = u32::from(endian.u16([b[14 + 2 * i], b[15 + 2 * i]]));
        codes.push((len, raw >> (width - len), i as u8));
    }
    codes.sort_unstable();
    // Canonical: each code follows the previous one, shifted to its length.
    let mut next = 0u32;
    let mut prev_len = codes[0].0;
    let mut counts = [0u8; 16];
    for &(len, code, _) in &codes {
        next <<= len - prev_len;
        if code != next {
            return None;
        }
        next += 1;
        prev_len = len;
        counts[len as usize - 1] += 1;
    }
    let values: Vec<u8> = codes.iter().map(|c| c.2).collect();
    Huffman::new(&counts, &values).ok()
}

/// Colour order R, G1, G2, B → by position in the 2×2 repeat.
fn positional(v: [f32; 4], cfa: CfaPattern) -> [f32; 4] {
    std::array::from_fn(|slot| {
        let (x, y) = ((slot % 2) as u32, (slot / 2) as u32);
        v[match cfa.color_at(x, y) {
            Some(0) => 0,
            Some(2) => 3,
            _ if cfa.color_at(0, y) == Some(0) || cfa.color_at(1, y) == Some(0) => 1,
            _ => 2,
        }]
    })
}

/// Decode a PEF; `None` for layouts we don't read.
pub(super) fn decode(src: &dyn Source, c: &Container) -> Result<Option<Sensor>, RawError> {
    let Some(raw) = c.raw_ifd(src)? else {
        return Ok(None);
    };
    let u = |t| raw.uint(src, t);
    if u(tag::COMPRESSION)? != Some(COMPRESSION_PENTAX) {
        return Ok(None);
    }
    let (Some(w), Some(h), Some(off), Some(len)) = (
        u(tag::IMAGE_WIDTH)?,
        u(tag::IMAGE_LENGTH)?,
        u(tag::STRIP_OFFSETS)?,
        u(tag::STRIP_BYTE_COUNTS)?,
    ) else {
        return Ok(None);
    };
    let Some((msrc, mn)) = maker_note(src, c) else {
        return Ok(None);
    };
    let Some(huff) = mn
        .get(HUFFMAN_TABLE)
        .map(|e| mn.bytes_prefix(&msrc, e, 256))
        .transpose()?
        .and_then(|b| huffman(&b, mn.endian))
    else {
        return Ok(None);
    };
    let (w, h) = (w as usize, h as usize);
    let (off, len) = (u64::from(off), u64::from(len));
    crate::source::check_range(off, len, 0, src.end())?;
    let mut data = vec![0u16; w * h];
    two_left(&src.read(off, len)?, &huff, [0; 4], w, &mut data, "PEF")?;

    let make = c.make(src)?;
    let model = c.model(src)?;
    let cam = cameras::lookup(&make, &model, "");
    let cfa = cam.cfa.unwrap_or(CfaPattern::Rggb);
    let four = |t| -> Result<Option<[f32; 4]>, RawError> {
        Ok(match mn.uints(&msrc, t)?.as_deref() {
            Some(&[a, b, c, d, ..]) => Some([a, b, c, d].map(|v| v as f32)),
            _ => None,
        })
    };
    let black = four(BLACK_POINT)?.map_or([0.0; 4], |b| positional(b, cfa));
    let wb = four(WB_LEVELS)?.map_or([f32::NAN; 3], |[r, g1, g2, b]| [r, g1.midpoint(g2), b]);
    let white = match mn.uint(&msrc, WHITE_LEVEL)? {
        Some(v) if v > 0 => v as f32,
        _ => cam.white.map_or(16383.0, |v| v as f32),
    };
    Ok(Some(Sensor {
        make,
        model,
        width: w,
        height: h,
        cpp: 1,
        data,
        cfa,
        black,
        white,
        crop: cam.crop.and_then(|m| from_margins(m, w, h)),
        wb,
        matrix: None,
        orientation: orientation(src, c),
    }))
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::super::nef::tests::encode_with;
    use super::super::testfile::{Link, Node, build, sample};
    use super::*;
    use crate::source::SliceSource;
    use crate::tiff::build::Val;

    /// A 0x0220 table: 14-byte header (code width 12 at byte 4), codes, lengths.
    fn table(codes: &[(u16, u8)], endian: Endian) -> Vec<u8> {
        let mut b = vec![3, 0, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        for &(c, len) in codes {
            let v = if len <= 12 { c << (12 - len) } else { c };
            b.extend_from_slice(&match endian {
                Endian::Little => v.to_le_bytes(),
                Endian::Big => v.to_be_bytes(),
            });
        }
        b.extend(codes.iter().map(|c| c.1));
        b
    }

    /// The K-3 III's code (symbol = index), as (code, length).
    const K3III: [(u16, u8); 15] = [
        (0xFE, 8),
        (0x7E, 7),
        (0x3E, 6),
        (0x1E, 5),
        (0xE, 4),
        (0x5, 3),
        (0x6, 3),
        (0x4, 3),
        (0x0, 2),
        (0x1, 2),
        (0x1FE, 9),
        (0x3FE, 10),
        (0x7FE, 11),
        (0xFFE, 12),
        (0xFFF, 12),
    ];

    /// The canonical (counts, values) of [`K3III`], for the encoder.
    fn canonical() -> ([u8; 16], Vec<u8>) {
        let mut v: Vec<(u8, u16, u8)> = K3III
            .iter()
            .enumerate()
            .map(|(i, &(c, l))| (l, c, i as u8))
            .collect();
        v.sort_unstable();
        let mut counts = [0u8; 16];
        for &(l, ..) in &v {
            counts[usize::from(l) - 1] += 1;
        }
        (counts, v.iter().map(|x| x.2).collect())
    }

    #[test]
    fn code_tables_parse_only_when_canonical() {
        for e in [Endian::Little, Endian::Big] {
            assert!(huffman(&table(&K3III, e), e).is_some());
        }
        let mut gap = K3III;
        gap[9].0 = 2; // 10 at length 2 leaves a hole after 00
        assert!(huffman(&table(&gap, Endian::Little), Endian::Little).is_none());
        let mut long = K3III;
        long[0].1 = 13; // longer than the code width
        assert!(huffman(&table(&long, Endian::Little), Endian::Little).is_none());
        assert!(huffman(&[0; 13], Endian::Little).is_none());
        assert!(huffman(&table(&K3III, Endian::Little)[..58], Endian::Little).is_none());
    }

    fn pef(strip: Vec<u8>, with_table: bool, compression: u16) -> Vec<u8> {
        let mut entries = vec![
            (WHITE_LEVEL, Val::Short(vec![16379])),
            (BLACK_POINT, Val::Short(vec![64, 65, 66, 67])),
            (WB_LEVELS, Val::Short(vec![20160, 8192, 8192, 13952])),
        ];
        if with_table {
            entries.push((HUFFMAN_TABLE, Val::Undefined(table(&K3III, Endian::Little))));
        }
        // The maker note IFD sits 10 bytes in; its offsets count from the
        // maker note's start.
        let mut mn = b"PENTAX \0II".to_vec();
        mn.extend(crate::tiff::build::ifd(Endian::Little, 10, &entries, 0));
        build(
            Endian::Little,
            &[
                Node::new(
                    Link::Chain,
                    vec![
                        (tag::MAKE, Val::Ascii("RICOH IMAGING COMPANY, LTD.")),
                        (tag::MODEL, Val::Ascii("PENTAX K-3 Mark III")),
                        (tag::IMAGE_WIDTH, Val::Long(vec![120])),
                        (tag::IMAGE_LENGTH, Val::Long(vec![100])),
                        (tag::COMPRESSION, Val::Short(vec![compression])),
                    ],
                )
                .with_blocks(vec![strip], false),
                Node::new(Link::Exif(0), vec![(tag::MAKER_NOTE, Val::Undefined(mn))]),
            ],
        )
    }

    fn sensor(f: &[u8]) -> Option<Sensor> {
        let src = SliceSource::new(f);
        let c = Container::parse(&src).unwrap();
        decode(&src, &c).unwrap()
    }

    #[test]
    fn pentax_file_decodes_with_its_own_code() {
        let img: Vec<u16> = (0..120 * 100)
            .map(|i| sample(i % 120, i / 120, 0, 1 << 14))
            .collect();
        let (counts, values) = canonical();
        let strip = encode_with(&img, 120, [0; 4], &counts, &values);
        let s = sensor(&pef(strip.clone(), true, 65535)).unwrap();
        assert_eq!(s.data, img);
        assert_eq!(s.white, 16379.0);
        assert_eq!(s.black, [64.0, 65.0, 66.0, 67.0]);
        assert_eq!(s.wb, [20160.0, 8192.0, 13952.0]);
        // Margins 54/58/58/38 from the K-3 III row of the camera table.
        assert_eq!(s.crop, Some([54, 58, 8, 4]));
        assert!(sensor(&pef(strip.clone(), false, 65535)).is_none());
        assert!(sensor(&pef(strip, true, 1)).is_none());
    }
}
