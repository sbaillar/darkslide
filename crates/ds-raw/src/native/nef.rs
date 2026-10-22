//! Nikon NEF: lossless compressed 14-bit (TIFF compression 34713 with
//! maker note `NEFCompression` 3) and uncompressed 16-bit data.
//!
//! The compressed stream is MSB-first Huffman-coded differences (no byte
//! stuffing), each a JPEG-style size category followed by that many bits
//! (T.81 F.12 `EXTEND`). Each difference applies to the pixel two columns to
//! the left (the same CFA colour); a row's first two pixels apply to the
//! first two of the row two above, starting from the four predictors in the
//! maker note's decode table (0x0096, after its two version bytes). The
//! Huffman code ([`LOSSLESS_14`]) is not stored in the file: it was worked
//! out from the corpus files against rawler's output (docs/RAWDEC.md), so
//! only 14-bit lossless files use it; 12-bit and lossy files go to the
//! fallback. "High Efficiency" files are unsupported in every build.
//!
//! From the maker note: black level (0x003D), as-shot WB (`WB_RBLevels`,
//! 0x000C) and the crop (0x0045: x, y, w, h). White levels come from the
//! camera table.

use ds_core::CfaPattern;
use rayon::prelude::*;

use super::ljpeg::{Bits, Huffman};
use super::{Sensor, from_margins, orientation};
use crate::RawError;
use crate::cameras;
use crate::container::Container;
use crate::decode::cfa_from_tag;
use crate::source::{Shifted, Source};
use crate::tiff::{Endian, Ifd, Tiff, tag};

const COMPRESSION_NIKON: u32 = 34713;
/// Maker note tags.
const WB_RB_LEVELS: u16 = 0x000C;
const BLACK_LEVEL: u16 = 0x003D;
const CROP_AREA: u16 = 0x0045;
const NEF_COMPRESSION: u16 = 0x0093;
const DECODE_TABLE: u16 = 0x0096;
/// `NEFCompression` values.
const LOSSLESS: u32 = 3;
const HIGH_EFFICIENCY: [u32; 2] = [13, 14];

/// The 14-bit lossless code as a `DHT`-style table: codes per length
/// (1..=16 bits), then the size categories in code order.
pub(crate) const LOSSLESS_14: ([u8; 16], [u8; 15]) = (
    [0, 1, 4, 2, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0],
    [7, 6, 8, 5, 9, 4, 10, 3, 11, 12, 2, 0, 1, 13, 14],
);

/// The maker note: `Nikon\0`, a version, then a TIFF whose offsets are
/// relative to its own header.
pub(crate) struct MakerNote<'a> {
    pub src: Shifted<'a>,
    pub ifd: Ifd,
}

pub(crate) fn maker_note<'a>(src: &'a dyn Source, c: &Container) -> Option<MakerNote<'a>> {
    let exif = c.exif.as_ref()?;
    let e = exif.get(tag::MAKER_NOTE)?;
    let off = u64::from(exif.endian.u32(e.field));
    if src.read(off, 6).ok()?.as_ref() != b"Nikon\0" {
        return None;
    }
    let sh = Shifted {
        inner: src,
        delta: off + 10,
    };
    let t = Tiff::parse(&sh, false).ok()?;
    let ifd = t.ifd0().clone();
    Some(MakerNote { src: sh, ifd })
}

/// Black levels in colour order R, G1, G2, B → by position in the 2×2 repeat.
fn positional(black: [f32; 4], cfa: CfaPattern) -> [f32; 4] {
    std::array::from_fn(|slot| {
        let (x, y) = ((slot % 2) as u32, (slot / 2) as u32);
        black[match cfa.color_at(x, y) {
            Some(0) => 0,
            Some(2) => 3,
            _ if cfa.color_at(0, y) == Some(0) || cfa.color_at(1, y) == Some(0) => 1,
            _ => 2,
        }]
    })
}

/// Decode the lossless stream into `out` (`w × h`).
pub(crate) fn lossless(
    data: &[u8],
    vpred: [i32; 4],
    w: usize,
    out: &mut [u16],
) -> Result<(), RawError> {
    let (counts, values) = LOSSLESS_14;
    two_left(data, &Huffman::new(&counts, &values)?, vpred, w, out, "NEF")
}

/// MSB-first Huffman differences (no byte stuffing) against the pixel two
/// to the left; the first two of a row against the first two of the row
/// two above, seeded with `vpred` (row parity × column). Nikon and Pentax
/// both store their data this way.
pub(crate) fn two_left(
    data: &[u8],
    huff: &Huffman,
    vpred: [i32; 4],
    w: usize,
    out: &mut [u16],
    what: &str,
) -> Result<(), RawError> {
    let mut padded = Vec::with_capacity(data.len() + 8);
    padded.extend_from_slice(data);
    padded.extend_from_slice(&[0; 8]);
    let mut br = Bits::over(&padded);
    let mut vp = vpred;
    for (y, row) in out.chunks_exact_mut(w).enumerate() {
        let mut left = [0i32; 2];
        for (x, px) in row.iter_mut().enumerate() {
            br.need32();
            let d = huff.diff(&mut br);
            let v = if x < 2 {
                let slot = (y & 1) * 2 + x;
                vp[slot] = vp[slot].wrapping_add(d);
                left[x] = vp[slot];
                vp[slot]
            } else {
                left[x & 1] = left[x & 1].wrapping_add(d);
                left[x & 1]
            };
            *px = v as u16;
        }
        if br.bad_code() {
            return Err(RawError::Malformed(format!("{what}: invalid Huffman code")));
        }
    }
    if br.overrun() > 64 {
        return Err(RawError::Truncated(format!("{what} data ends early")));
    }
    Ok(())
}

/// Decode a NEF; `None` for layouts we don't read.
pub(super) fn decode(src: &dyn Source, c: &Container) -> Result<Option<Sensor>, RawError> {
    let Some(raw) = c.raw_ifd(src)? else {
        return Ok(None);
    };
    let Some(mn) = maker_note(src, c) else {
        return Ok(None);
    };
    let (msrc, mifd) = (&mn.src, &mn.ifd);
    let make = c.make(src)?;
    let model = c.model(src)?;
    let kind = mifd.uint(msrc, NEF_COMPRESSION)?;
    if kind.is_some_and(|k| HIGH_EFFICIENCY.contains(&k)) {
        return Err(RawError::Unsupported {
            make,
            model,
            reason: "High Efficiency (HE/HE*) compression isn't supported by the RAW decoder yet"
                .into(),
        });
    }
    let u = |t| raw.uint(src, t);
    let (Some(w), Some(h), Some(off), Some(len), Some(bps)) = (
        u(tag::IMAGE_WIDTH)?,
        u(tag::IMAGE_LENGTH)?,
        u(tag::STRIP_OFFSETS)?,
        u(tag::STRIP_BYTE_COUNTS)?,
        u(tag::BITS_PER_SAMPLE)?,
    ) else {
        return Ok(None);
    };
    let Some(cfa) = raw
        .uints(src, tag::CFA_PATTERN)?
        .as_deref()
        .and_then(cfa_from_tag)
    else {
        return Ok(None);
    };
    let (w, h) = (w as usize, h as usize);
    let (off, len) = (u64::from(off), u64::from(len));
    crate::source::check_range(off, len, 0, src.end())?;
    let mut data = vec![0u16; w * h];
    match u(tag::COMPRESSION)? {
        Some(COMPRESSION_NIKON) if kind == Some(LOSSLESS) && bps == 14 => {
            let Some(table) = mifd.get(DECODE_TABLE) else {
                return Ok(None);
            };
            let t = mifd.bytes_prefix(msrc, table, 10)?;
            // Version 0x46: the 14-bit lossless layout.
            if t.len() < 10 || t[0] != 0x46 {
                return Ok(None);
            }
            let vpred =
                std::array::from_fn(|i| i32::from(mifd.endian.u16([t[2 + 2 * i], t[3 + 2 * i]])));
            lossless(&src.read(off, len)?, vpred, w, &mut data)?;
        }
        Some(1) if len == (w * h * 2) as u64 => {
            let bytes = src.read(off, len)?;
            data.par_chunks_mut(w)
                .zip(bytes.par_chunks_exact(w * 2))
                .for_each(|(d, s)| {
                    for (v, b) in d.iter_mut().zip(s.as_chunks::<2>().0) {
                        *v = match c.tiff.endian {
                            Endian::Little => u16::from_le_bytes(*b),
                            Endian::Big => u16::from_be_bytes(*b),
                        };
                    }
                });
        }
        _ => return Ok(None),
    }

    let cam = cameras::lookup(&make, &model, &format!("{bps}bit"));
    let black = match mifd.uints(msrc, BLACK_LEVEL)?.as_deref() {
        Some(&[r, g1, g2, b, ..]) => positional([r, g1, g2, b].map(|v| v as f32), cfa),
        _ => [0.0; 4],
    };
    let wb = match mifd.rationals(msrc, WB_RB_LEVELS)?.as_deref() {
        Some([r, b, ..]) => [r, b].map(|v| v.to_f64().unwrap_or(f64::NAN) as f32),
        _ => [f32::NAN; 2],
    };
    let crop = match mifd.uints(msrc, CROP_AREA)?.as_deref() {
        Some(&[x, y, cw, ch, ..]) if cw > 0 && ch > 0 => {
            Some([x as usize, y as usize, cw as usize, ch as usize])
        }
        _ => cam.crop.and_then(|m| from_margins(m, w, h)),
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
        white: cam
            .white
            .map_or(((1u32 << bps.min(16)) - 1) as f32, |v| v as f32),
        crop,
        wb: [wb[0], 1.0, wb[1]],
        matrix: None,
        orientation: orientation(src, c),
    }))
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
pub(crate) mod tests {
    use super::super::testfile::{Link, Node, build, sample};
    use super::*;
    use crate::source::SliceSource;
    use crate::tiff::build::{Val, header, ifd};

    /// Encode a `w`-wide image the way the decoder reads it.
    pub fn encode(img: &[u16], w: usize, vpred: [i32; 4]) -> Vec<u8> {
        let (counts, values) = LOSSLESS_14;
        encode_with(img, w, vpred, &counts, &values)
    }

    /// [`encode`] with any canonical code (counts per length, symbols).
    pub fn encode_with(
        img: &[u16],
        w: usize,
        vpred: [i32; 4],
        counts: &[u8; 16],
        values: &[u8],
    ) -> Vec<u8> {
        let mut codes = [(0u32, 0u32); 17];
        let (mut code, mut k) = (0u32, 0usize);
        for len in 1..=16u32 {
            for _ in 0..counts[len as usize - 1] {
                codes[usize::from(values[k])] = (code, len);
                code += 1;
                k += 1;
            }
            code <<= 1;
        }
        let (mut out, mut acc, mut n) = (Vec::new(), 0u64, 0u32);
        let mut put = |v: u32, len: u32| {
            acc = (acc << len) | u64::from(v & ((1u32 << len) - 1));
            n += len;
            while n >= 8 {
                n -= 8;
                out.push((acc >> n) as u8);
            }
        };
        let mut vp = vpred;
        for (y, row) in img.chunks_exact(w).enumerate() {
            for (x, &v) in row.iter().enumerate() {
                let pred = if x < 2 {
                    vp[(y & 1) * 2 + x]
                } else {
                    i32::from(row[x - 2])
                };
                let d = i32::from(v) - pred;
                if x < 2 {
                    vp[(y & 1) * 2 + x] = i32::from(v);
                }
                let s = 32 - d.unsigned_abs().leading_zeros();
                let (c, len) = codes[s as usize];
                put(c, len);
                if s > 0 {
                    put((if d < 0 { d - 1 } else { d }) as u32, s);
                }
            }
        }
        put(0, 7);
        out
    }

    #[test]
    fn lossless_round_trips() {
        let (w, h) = (18, 6);
        let img: Vec<u16> = (0..w * h)
            .map(|i| sample(i % w, i / w, 0, 1 << 14))
            .collect();
        let vpred = [2048, 1024, 4096, 1];
        let mut out = vec![0u16; w * h];
        lossless(&encode(&img, w, vpred), vpred, w, &mut out).unwrap();
        assert_eq!(out, img);
        // Cut short: an error, not garbage.
        let bits = encode(&img, w, vpred);
        assert!(matches!(
            lossless(&bits[..bits.len() / 2], vpred, w, &mut out),
            Err(RawError::Truncated(_))
        ));
        // All-ones bits: no code is that long.
        assert!(lossless(&[0xFF; 64], vpred, w, &mut out).is_err());
    }

    /// A NEF: IFD0 → Exif → `Nikon` maker note (its own TIFF); raw `SubIFD`.
    fn nef(compression: u16, kind: u16, version: u8, strip: Vec<u8>, w: u32, h: u32) -> Vec<u8> {
        let mut table = vec![version, 0x30];
        for v in [2048u16, 2048, 2048, 2048] {
            table.extend_from_slice(&v.to_le_bytes());
        }
        table.resize(46, 0);
        let mut inner = header(Endian::Little);
        inner.extend(ifd(
            Endian::Little,
            8,
            &[
                (
                    WB_RB_LEVELS,
                    Val::Rational(vec![(3892, 2048), (2817, 2048), (1, 1), (1, 1)]),
                ),
                (BLACK_LEVEL, Val::Short(vec![400, 401, 402, 403])),
                (
                    CROP_AREA,
                    Val::Short(vec![2, 1, w as u16 - 4, h as u16 - 2]),
                ),
                (NEF_COMPRESSION, Val::Short(vec![kind])),
                (DECODE_TABLE, Val::Undefined(table)),
            ],
            0,
        ));
        let mut mn = b"Nikon\0\x02\x11\0\0".to_vec();
        mn.extend(inner);
        build(
            Endian::Little,
            &[
                Node::new(
                    Link::Chain,
                    vec![
                        (tag::MAKE, Val::Ascii("NIKON CORPORATION")),
                        (tag::MODEL, Val::Ascii("NIKON TEST")),
                    ],
                ),
                Node::new(
                    Link::Sub(0),
                    vec![
                        (tag::IMAGE_WIDTH, Val::Long(vec![w])),
                        (tag::IMAGE_LENGTH, Val::Long(vec![h])),
                        (tag::BITS_PER_SAMPLE, Val::Short(vec![14])),
                        (tag::COMPRESSION, Val::Short(vec![compression])),
                        (tag::CFA_PATTERN, Val::Byte(vec![1, 2, 0, 1])),
                    ],
                )
                .with_blocks(vec![strip], false),
                Node::new(Link::Exif(0), vec![(tag::MAKER_NOTE, Val::Undefined(mn))]),
            ],
        )
    }

    fn sensor(f: &[u8]) -> Result<Option<Sensor>, RawError> {
        let src = SliceSource::new(f);
        let c = Container::parse(&src).unwrap();
        decode(&src, &c)
    }

    #[test]
    fn lossless_file_with_maker_note_levels() {
        let (w, h) = (16usize, 6usize);
        let img: Vec<u16> = (0..w * h)
            .map(|i| sample(i % w, i / w, 0, 1 << 14))
            .collect();
        let f = nef(
            34713,
            LOSSLESS as u16,
            0x46,
            encode(&img, w, [2048; 4]),
            16,
            6,
        );
        let s = sensor(&f).unwrap().unwrap();
        assert_eq!(s.data, img);
        assert_eq!(s.cfa, CfaPattern::Gbrg);
        // R, G1, G2, B = 400, 401, 402, 403 placed on GBRG: G2 B / R G1.
        assert_eq!(s.black, [402.0, 403.0, 400.0, 401.0]);
        assert_eq!(s.crop, Some([2, 1, 12, 4]));
        assert!((s.wb[0] - 1.900_390_6).abs() < 1e-6 && s.wb[1] == 1.0);
        assert_eq!(s.white, 16383.0, "unknown camera: the full 14-bit range");
        // Uncompressed 16-bit.
        let raw: Vec<u8> = img.iter().flat_map(|v| v.to_le_bytes()).collect();
        let s = sensor(&nef(1, 2, 0x46, raw, 16, 6)).unwrap().unwrap();
        assert_eq!(s.data, img);
    }

    #[test]
    fn other_codecs_are_not_ours_and_he_is_unsupported() {
        let strip = vec![0u8; 64];
        // Lossy, or a decode table version we haven't checked: the fallback.
        assert!(
            sensor(&nef(34713, 1, 0x46, strip.clone(), 16, 6))
                .unwrap()
                .is_none()
        );
        assert!(
            sensor(&nef(34713, 3, 0x44, strip.clone(), 16, 6))
                .unwrap()
                .is_none()
        );
        // Uncompressed of the wrong size.
        assert!(
            sensor(&nef(1, 2, 0x46, strip.clone(), 16, 6))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            sensor(&nef(34713, 13, 0x46, strip, 16, 6)),
            Err(RawError::Unsupported { reason, .. }) if reason.contains("HE/HE*")
        ));
    }
}
