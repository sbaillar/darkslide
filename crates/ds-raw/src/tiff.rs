//! Minimal, defensive TIFF/EXIF IFD walker.
//!
//! Only what RAW metadata extraction needs: parse an IFD's entry table and
//! decode entry values on demand. All offsets are validated against the
//! [`Source`] before any allocation, arithmetic is checked, and IFD chains
//! are bounded so loops or hostile counts cannot hang or exhaust memory.
//!
//! Reads are also bounded by what the caller needs, not by the declared
//! count: a scalar read touches one element, arrays at most [`MAX_ARRAY`]
//! elements and strings at most [`MAX_ASCII`] bytes, so a crafted entry
//! claiming a 16 MB value costs no more than a legitimate one.

use std::borrow::Cow;

use crate::RawError;
use crate::source::Source;

/// Maximum entries accepted in one IFD. Real files stay well under 200.
const MAX_ENTRIES: u16 = 1024;
/// Maximum IFDs followed through a `next` chain.
pub(crate) const MAX_CHAIN: usize = 16;
/// Most elements read from an array-valued entry (`SubIFDs`, levels, crop, …).
pub(crate) const MAX_ARRAY: u64 = 256;
/// Most bytes read from an ASCII entry.
pub(crate) const MAX_ASCII: u64 = 4096;

pub(crate) mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 0x00FE;
    pub const IMAGE_WIDTH: u16 = 0x0100;
    pub const IMAGE_LENGTH: u16 = 0x0101;
    pub const BITS_PER_SAMPLE: u16 = 0x0102;
    pub const COMPRESSION: u16 = 0x0103;
    pub const PHOTOMETRIC: u16 = 0x0106;
    pub const MAKE: u16 = 0x010F;
    pub const MODEL: u16 = 0x0110;
    pub const STRIP_OFFSETS: u16 = 0x0111;
    pub const ORIENTATION: u16 = 0x0112;
    pub const ARTIST: u16 = 0x013B;
    pub const COPYRIGHT: u16 = 0x8298;
    pub const SAMPLES_PER_PIXEL: u16 = 0x0115;
    pub const STRIP_BYTE_COUNTS: u16 = 0x0117;
    pub const TILE_OFFSETS: u16 = 0x0144;
    pub const TILE_BYTE_COUNTS: u16 = 0x0145;
    pub const SUB_IFDS: u16 = 0x014A;
    pub const JPEG_OFFSET: u16 = 0x0201;
    pub const JPEG_LENGTH: u16 = 0x0202;
    pub const CFA_PATTERN: u16 = 0x828E;
    pub const EXPOSURE_TIME: u16 = 0x829A;
    pub const F_NUMBER: u16 = 0x829D;
    pub const EXIF_IFD: u16 = 0x8769;
    pub const GPS_IFD: u16 = 0x8825;
    /// GPS IFD tags (EXIF 2.3 §4.6.6).
    pub const GPS_LATITUDE_REF: u16 = 0x0001;
    pub const GPS_LATITUDE: u16 = 0x0002;
    pub const GPS_LONGITUDE_REF: u16 = 0x0003;
    pub const GPS_LONGITUDE: u16 = 0x0004;
    pub const GPS_ALTITUDE_REF: u16 = 0x0005;
    pub const GPS_ALTITUDE: u16 = 0x0006;
    /// `A` (measurement active) or `V` (void: no fix).
    pub const GPS_STATUS: u16 = 0x0009;
    pub const ISO: u16 = 0x8827;
    pub const SENSITIVITY_TYPE: u16 = 0x8830;
    pub const RECOMMENDED_EXPOSURE_INDEX: u16 = 0x8832;
    pub const ISO_SPEED: u16 = 0x8833;
    pub const DATE_TIME_ORIGINAL: u16 = 0x9003;
    pub const OFFSET_TIME_ORIGINAL: u16 = 0x9011;
    pub const FOCAL_LENGTH: u16 = 0x920A;
    pub const MAKER_NOTE: u16 = 0x927C;
    pub const SUB_SEC_TIME_ORIGINAL: u16 = 0x9291;
    pub const BODY_SERIAL: u16 = 0xA431;
    pub const LENS_MODEL: u16 = 0xA434;
    pub const FOCAL_LENGTH_35MM: u16 = 0xA405;
    pub const DNG_PRIVATE_DATA: u16 = 0xC634;
    pub const WHITE_LEVEL: u16 = 0xC61D;
    pub const DEFAULT_CROP_ORIGIN: u16 = 0xC61F;
    pub const DEFAULT_CROP_SIZE: u16 = 0xC620;
    /// Panasonic RW2 raw frame size (IFD0).
    pub const PANA_WIDTH: u16 = 0x0002;
    pub const PANA_LENGTH: u16 = 0x0003;
    /// Panasonic RW2 IFD0: the embedded JPEG (`JpgFromRaw`).
    pub const PANA_JPG_FROM_RAW: u16 = 0x002E;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endian {
    Little,
    Big,
}

impl Endian {
    pub fn u16(self, b: [u8; 2]) -> u16 {
        match self {
            Self::Little => u16::from_le_bytes(b),
            Self::Big => u16::from_be_bytes(b),
        }
    }

    pub fn u32(self, b: [u8; 4]) -> u32 {
        match self {
            Self::Little => u32::from_le_bytes(b),
            Self::Big => u32::from_be_bytes(b),
        }
    }
}

/// Byte size of one element of a TIFF field type, `None` for unknown types.
pub(crate) fn type_size(typ: u16) -> Option<u64> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,   // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,           // SHORT, SSHORT
        4 | 9 | 11 | 13 => 4, // LONG, SLONG, FLOAT, IFD
        5 | 10 | 12 => 8,     // RATIONAL, SRATIONAL, DOUBLE
        _ => return None,
    })
}

/// A signed or unsigned rational as stored in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawRational {
    pub num: i64,
    pub den: i64,
}

impl RawRational {
    pub fn to_f64(self) -> Option<f64> {
        (self.den != 0).then(|| self.num as f64 / self.den as f64)
    }
}

/// One 12-byte IFD entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub tag: u16,
    pub typ: u16,
    pub count: u32,
    /// Raw value/offset field, in file byte order.
    pub field: [u8; 4],
}

impl Entry {
    /// Declared total byte size of the value (`None` for unknown types).
    pub fn byte_len(&self) -> Option<u64> {
        type_size(self.typ).map(|s| s * u64::from(self.count))
    }
}

/// A parsed IFD. Value offsets are absolute offsets into the owning [`Source`].
#[derive(Debug, Clone)]
pub(crate) struct Ifd {
    /// Where this IFD's table starts.
    pub off: u64,
    pub endian: Endian,
    pub entries: Vec<Entry>,
    /// Offset of the next IFD in the chain (0 = none).
    pub next: u32,
}

impl Ifd {
    /// Parse the IFD at absolute offset `off`.
    pub fn parse(src: &dyn Source, off: u64, endian: Endian) -> Result<Self, RawError> {
        let head = src.read(off, 2)?;
        let n = endian.u16([head[0], head[1]]);
        if n > MAX_ENTRIES {
            return Err(RawError::Malformed(format!(
                "IFD at {off} claims {n} entries"
            )));
        }
        let table = src.read(off + 2, u64::from(n) * 12 + 4)?;
        let entries = table[..usize::from(n) * 12]
            .as_chunks::<12>()
            .0
            .iter()
            .map(|e| Entry {
                tag: endian.u16([e[0], e[1]]),
                typ: endian.u16([e[2], e[3]]),
                count: endian.u32([e[4], e[5], e[6], e[7]]),
                field: [e[8], e[9], e[10], e[11]],
            })
            .collect();
        let t = &table[usize::from(n) * 12..];
        let next = endian.u32([t[0], t[1], t[2], t[3]]);
        Ok(Self {
            off,
            endian,
            entries,
            next,
        })
    }

    pub fn get(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// The first `max_elems` elements of an entry's value (inline or at its
    /// offset). Only those bytes are range-checked and read.
    fn value_prefix<'s>(
        &self,
        src: &'s dyn Source,
        e: &Entry,
        max_elems: u64,
    ) -> Result<Cow<'s, [u8]>, RawError> {
        let es = type_size(e.typ).ok_or_else(|| {
            RawError::Malformed(format!("tag {:#06x}: unknown type {}", e.tag, e.typ))
        })?;
        let total = es * u64::from(e.count);
        let size = es * u64::from(e.count).min(max_elems);
        if total <= 4 {
            Ok(Cow::Owned(e.field[..size as usize].to_vec()))
        } else {
            src.read(u64::from(self.endian.u32(e.field)), size)
        }
    }

    /// At most `max_len` leading bytes of an entry's value.
    pub fn bytes_prefix<'s>(
        &self,
        src: &'s dyn Source,
        e: &Entry,
        max_len: u64,
    ) -> Result<Cow<'s, [u8]>, RawError> {
        let es = type_size(e.typ).unwrap_or(1);
        self.value_prefix(src, e, max_len / es)
    }

    /// Up to `max` integer values of a BYTE/SHORT/LONG (or signed) entry.
    pub(crate) fn uints_n(
        &self,
        src: &dyn Source,
        tag: u16,
        max: u64,
    ) -> Result<Option<Vec<u32>>, RawError> {
        let Some(e) = self.get(tag) else {
            return Ok(None);
        };
        if !matches!(e.typ, 1 | 3 | 4 | 6 | 7 | 8 | 9 | 13) {
            return Err(RawError::Malformed(format!(
                "tag {tag:#06x}: type {} is not an integer",
                e.typ
            )));
        }
        let b = self.value_prefix(src, e, max)?;
        let v = match e.typ {
            1 | 6 | 7 => b.iter().map(|&x| u32::from(x)).collect(),
            3 | 8 => b
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u32::from(self.endian.u16(*c)))
                .collect(),
            _ => b
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| self.endian.u32(*c))
                .collect(),
        };
        Ok(Some(v))
    }

    /// Integer values (at most [`MAX_ARRAY`]).
    pub fn uints(&self, src: &dyn Source, tag: u16) -> Result<Option<Vec<u32>>, RawError> {
        self.uints_n(src, tag, MAX_ARRAY)
    }

    /// First integer value of an entry; reads only that element.
    pub fn uint(&self, src: &dyn Source, tag: u16) -> Result<Option<u32>, RawError> {
        Ok(self.uints_n(src, tag, 1)?.and_then(|v| v.first().copied()))
    }

    fn rationals_n(
        &self,
        src: &dyn Source,
        tag: u16,
        max: u64,
    ) -> Result<Option<Vec<RawRational>>, RawError> {
        let Some(e) = self.get(tag) else {
            return Ok(None);
        };
        let v = match e.typ {
            5 | 10 => {
                let b = self.value_prefix(src, e, max)?;
                b.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| {
                        let n = self.endian.u32([c[0], c[1], c[2], c[3]]);
                        let d = self.endian.u32([c[4], c[5], c[6], c[7]]);
                        if e.typ == 10 {
                            RawRational {
                                num: i64::from(n as i32),
                                den: i64::from(d as i32),
                            }
                        } else {
                            RawRational {
                                num: i64::from(n),
                                den: i64::from(d),
                            }
                        }
                    })
                    .collect()
            }
            _ => self
                .uints_n(src, tag, max)?
                .unwrap_or_default()
                .into_iter()
                .map(|n| RawRational {
                    num: i64::from(n),
                    den: 1,
                })
                .collect(),
        };
        Ok(Some(v))
    }

    /// Rational values (at most [`MAX_ARRAY`]); integers are accepted as `n/1`.
    pub fn rationals(
        &self,
        src: &dyn Source,
        tag: u16,
    ) -> Result<Option<Vec<RawRational>>, RawError> {
        self.rationals_n(src, tag, MAX_ARRAY)
    }

    /// First rational value; reads only that element.
    pub fn rational(&self, src: &dyn Source, tag: u16) -> Result<Option<RawRational>, RawError> {
        Ok(self
            .rationals_n(src, tag, 1)?
            .and_then(|v| v.first().copied()))
    }

    /// ASCII value (first [`MAX_ASCII`] bytes), cut at the first NUL and
    /// trimmed. Empty strings become `None`.
    pub fn ascii(&self, src: &dyn Source, tag: u16) -> Result<Option<String>, RawError> {
        let Some(e) = self.get(tag) else {
            return Ok(None);
        };
        let b = self.value_prefix(src, e, MAX_ASCII)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        let s = String::from_utf8_lossy(&b[..end]).trim().to_string();
        Ok((!s.is_empty()).then_some(s))
    }
}

/// A TIFF container: header byte order plus the IFD0 chain.
#[derive(Debug, Clone)]
pub(crate) struct Tiff {
    pub endian: Endian,
    /// IFD0, IFD1, … following `next` links (bounded, loop-safe).
    pub chain: Vec<Ifd>,
}

impl Tiff {
    /// Classic TIFF only (test shorthand).
    #[cfg(test)]
    pub fn parse_std(src: &dyn Source) -> Result<Self, RawError> {
        Self::parse(src, false)
    }

    /// Byte order from a classic TIFF header (`II*\0` / `MM\0*`).
    /// With `variants`, also the TIFF-structured RAW signatures that use a
    /// different magic number: Olympus ORF (`IIRO`, `IIRS`, `MMOR`) and
    /// Panasonic RW2 (`IIU\0`).
    pub fn sniff(head: &[u8], variants: bool) -> Option<Endian> {
        match head.get(..4)? {
            [b'I', b'I', 42, 0] => Some(Endian::Little),
            [b'M', b'M', 0, 42] => Some(Endian::Big),
            [b'I', b'I', b'R', b'O' | b'S'] | [b'I', b'I', b'U', 0] if variants => {
                Some(Endian::Little)
            }
            [b'M', b'M', b'O', b'R'] if variants => Some(Endian::Big),
            _ => None,
        }
    }

    pub fn parse(src: &dyn Source, variants: bool) -> Result<Self, RawError> {
        let head = src
            .read(0, 8)
            .map_err(|_| RawError::NotRaw("file too short for a TIFF header".into()))?;
        let endian = Self::sniff(&head, variants)
            .ok_or_else(|| RawError::NotRaw("no TIFF header".into()))?;
        let first = endian.u32([head[4], head[5], head[6], head[7]]);
        let chain = Self::chain(src, u64::from(first), endian)?;
        if chain.is_empty() {
            return Err(RawError::Malformed("TIFF has no IFD0".into()));
        }
        Ok(Self { endian, chain })
    }

    /// Follow a `next` chain from `off`; stops at 0, a repeated offset, or [`MAX_CHAIN`].
    /// IFD0 must parse; a broken link later in the chain just ends it.
    pub fn chain(src: &dyn Source, mut off: u64, endian: Endian) -> Result<Vec<Ifd>, RawError> {
        let mut out: Vec<Ifd> = Vec::new();
        while off != 0 && out.len() < MAX_CHAIN && !out.iter().any(|i| i.off == off) {
            match Ifd::parse(src, off, endian) {
                Ok(ifd) => {
                    off = u64::from(ifd.next);
                    out.push(ifd);
                }
                Err(e) if out.is_empty() => return Err(e),
                Err(_) => break,
            }
        }
        Ok(out)
    }

    pub fn ifd0(&self) -> &Ifd {
        &self.chain[0]
    }
}

#[cfg(test)]
pub(crate) mod build {
    //! A tiny TIFF writer used by tests to build synthetic files.

    use super::Endian;

    /// A value to store in an entry.
    #[derive(Debug, Clone)]
    pub enum Val {
        Byte(Vec<u8>),
        Ascii(&'static str),
        Short(Vec<u16>),
        Long(Vec<u32>),
        Rational(Vec<(u32, u32)>),
        SRational(Vec<(i32, i32)>),
        Undefined(Vec<u8>),
        /// LONG entry whose value is patched later (offset placeholder).
        Raw {
            typ: u16,
            count: u32,
            bytes: Vec<u8>,
        },
    }

    /// Serialize an IFD whose table starts at `at`. Out-of-line values are placed
    /// right after the table. Returns the bytes (table + values).
    pub fn ifd(e: Endian, at: u32, entries: &[(u16, Val)], next: u32) -> Vec<u8> {
        let w16 = |v: u16| match e {
            Endian::Little => v.to_le_bytes(),
            Endian::Big => v.to_be_bytes(),
        };
        let w32 = |v: u32| match e {
            Endian::Little => v.to_le_bytes(),
            Endian::Big => v.to_be_bytes(),
        };
        let mut sorted: Vec<&(u16, Val)> = entries.iter().collect();
        sorted.sort_by_key(|(t, _)| *t);
        let table_len = 2 + sorted.len() * 12 + 4;
        let mut table = Vec::new();
        let mut extra: Vec<u8> = Vec::new();
        table.extend_from_slice(&w16(sorted.len() as u16));
        for (t, v) in sorted {
            let (typ, count, bytes): (u16, u32, Vec<u8>) = match v {
                Val::Byte(b) => (1, b.len() as u32, b.clone()),
                Val::Ascii(s) => {
                    let mut b = s.as_bytes().to_vec();
                    b.push(0);
                    (2, b.len() as u32, b)
                }
                Val::Short(s) => (3, s.len() as u32, s.iter().flat_map(|x| w16(*x)).collect()),
                Val::Long(s) => (4, s.len() as u32, s.iter().flat_map(|x| w32(*x)).collect()),
                Val::Rational(s) => (
                    5,
                    s.len() as u32,
                    s.iter()
                        .flat_map(|(n, d)| [w32(*n), w32(*d)].concat())
                        .collect(),
                ),
                Val::SRational(s) => (
                    10,
                    s.len() as u32,
                    s.iter()
                        .flat_map(|(n, d)| [w32(*n as u32), w32(*d as u32)].concat())
                        .collect(),
                ),
                Val::Undefined(b) => (7, b.len() as u32, b.clone()),
                Val::Raw { typ, count, bytes } => (*typ, *count, bytes.clone()),
            };
            table.extend_from_slice(&w16(*t));
            table.extend_from_slice(&w16(typ));
            table.extend_from_slice(&w32(count));
            if bytes.len() <= 4 {
                let mut f = [0u8; 4];
                f[..bytes.len()].copy_from_slice(&bytes);
                table.extend_from_slice(&f);
            } else {
                let off = at + table_len as u32 + extra.len() as u32;
                table.extend_from_slice(&w32(off));
                extra.extend_from_slice(&bytes);
                if extra.len() % 2 == 1 {
                    extra.push(0);
                }
            }
        }
        table.extend_from_slice(&w32(next));
        table.extend_from_slice(&extra);
        table
    }

    /// Classic TIFF header pointing at IFD0 offset 8.
    pub fn header(e: Endian) -> Vec<u8> {
        match e {
            Endian::Little => vec![b'I', b'I', 42, 0, 8, 0, 0, 0],
            Endian::Big => vec![b'M', b'M', 0, 42, 0, 0, 0, 8],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::build::{Val, header, ifd};
    use super::*;
    use crate::source::SliceSource;

    fn file(e: Endian, entries: &[(u16, Val)], next: u32) -> Vec<u8> {
        let mut f = header(e);
        f.extend(ifd(e, 8, entries, next));
        f
    }

    #[test]
    fn parses_both_byte_orders_and_all_value_kinds() {
        for e in [Endian::Little, Endian::Big] {
            let f = file(
                e,
                &[
                    (tag::MAKE, Val::Ascii("SONY")),
                    (tag::MODEL, Val::Ascii("ILCE-9  ")),
                    (tag::ORIENTATION, Val::Short(vec![8])),
                    (tag::IMAGE_WIDTH, Val::Long(vec![6048])),
                    (tag::CFA_PATTERN, Val::Byte(vec![0, 1, 1, 2])),
                    (tag::EXPOSURE_TIME, Val::Rational(vec![(1, 5000)])),
                    (0x9204, Val::SRational(vec![(-7, 10)])),
                    (tag::DEFAULT_CROP_SIZE, Val::Short(vec![6000, 4000])),
                ],
                0,
            );
            let src = SliceSource::new(&f);
            let t = Tiff::parse(&src, false).unwrap();
            assert_eq!(t.endian, e);
            let i = t.ifd0();
            assert_eq!(i.ascii(&src, tag::MAKE).unwrap().as_deref(), Some("SONY"));
            assert_eq!(
                i.ascii(&src, tag::MODEL).unwrap().as_deref(),
                Some("ILCE-9")
            );
            assert_eq!(i.uint(&src, tag::ORIENTATION).unwrap(), Some(8));
            assert_eq!(i.uint(&src, tag::IMAGE_WIDTH).unwrap(), Some(6048));
            assert_eq!(
                i.uints(&src, tag::CFA_PATTERN).unwrap(),
                Some(vec![0, 1, 1, 2])
            );
            assert_eq!(
                i.uints(&src, tag::DEFAULT_CROP_SIZE).unwrap(),
                Some(vec![6000, 4000])
            );
            let r = i.rational(&src, tag::EXPOSURE_TIME).unwrap().unwrap();
            assert_eq!((r.num, r.den), (1, 5000));
            let s = i.rational(&src, 0x9204).unwrap().unwrap();
            assert_eq!((s.num, s.den), (-7, 10));
            assert!((s.to_f64().unwrap() + 0.7).abs() < 1e-9);
            // Integers read as rationals n/1.
            let w = i.rational(&src, tag::IMAGE_WIDTH).unwrap().unwrap();
            assert_eq!((w.num, w.den), (6048, 1));
            // Missing tags are None, not errors.
            assert_eq!(i.uint(&src, 0x1234).unwrap(), None);
            assert_eq!(i.ascii(&src, 0x1234).unwrap(), None);
            assert_eq!(i.rational(&src, 0x1234).unwrap(), None);
            // Rational read as integer is a type error.
            assert!(matches!(
                i.uints(&src, tag::EXPOSURE_TIME),
                Err(RawError::Malformed(_))
            ));
        }
    }

    #[test]
    fn empty_ascii_is_none_and_zero_denominator_is_none() {
        let f = file(
            Endian::Little,
            &[
                (tag::MAKE, Val::Ascii("  ")),
                (tag::F_NUMBER, Val::Rational(vec![(28, 0)])),
            ],
            0,
        );
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        assert_eq!(t.ifd0().ascii(&src, tag::MAKE).unwrap(), None);
        assert_eq!(
            t.ifd0()
                .rational(&src, tag::F_NUMBER)
                .unwrap()
                .unwrap()
                .to_f64(),
            None
        );
    }

    #[test]
    fn rejects_non_tiff_and_short_input() {
        assert!(matches!(
            Tiff::parse_std(&SliceSource::new(b"")),
            Err(RawError::NotRaw(_))
        ));
        assert!(matches!(
            Tiff::parse_std(&SliceSource::new(b"\xFF\xD8\xFF\xE0garbage")),
            Err(RawError::NotRaw(_))
        ));
        // Valid header, IFD0 offset past EOF.
        let f = [b'I', b'I', 42, 0, 0xFF, 0xFF, 0, 0];
        assert!(matches!(
            Tiff::parse_std(&SliceSource::new(&f)),
            Err(RawError::Truncated(_))
        ));
        // IFD0 offset 0.
        let f = [b'I', b'I', 42, 0, 0, 0, 0, 0];
        assert!(matches!(
            Tiff::parse_std(&SliceSource::new(&f)),
            Err(RawError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_hostile_entry_counts_and_value_offsets() {
        // Entry count far above MAX_ENTRIES.
        let mut f = header(Endian::Little);
        f.extend_from_slice(&[0xFF, 0xFF]);
        assert!(matches!(
            Tiff::parse_std(&SliceSource::new(&f)),
            Err(RawError::Malformed(_))
        ));
        // Count that fits the cap but not the file.
        let mut f = header(Endian::Little);
        f.extend_from_slice(&[0x00, 0x02]);
        assert!(matches!(
            Tiff::parse_std(&SliceSource::new(&f)),
            Err(RawError::Truncated(_))
        ));
        // Out-of-line value with offset/count pointing way past EOF.
        let f = file(
            Endian::Little,
            &[(
                tag::MAKE,
                Val::Raw {
                    typ: 2,
                    count: u32::MAX,
                    bytes: vec![0xF0, 0xFF, 0xFF, 0xFF],
                },
            )],
            0,
        );
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        assert!(matches!(
            t.ifd0().ascii(&src, tag::MAKE),
            Err(RawError::Truncated(_))
        ));
        // Unknown field type.
        let f = file(
            Endian::Little,
            &[(
                tag::MAKE,
                Val::Raw {
                    typ: 99,
                    count: 1,
                    bytes: vec![0; 4],
                },
            )],
            0,
        );
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        assert!(matches!(
            t.ifd0().ascii(&src, tag::MAKE),
            Err(RawError::Malformed(_))
        ));
    }

    #[test]
    fn chain_is_loop_safe_and_tolerates_broken_links() {
        // IFD0 whose next points to itself.
        let f = file(
            Endian::Little,
            &[(tag::ORIENTATION, Val::Short(vec![1]))],
            8,
        );
        let src = SliceSource::new(&f);
        assert_eq!(Tiff::parse(&src, false).unwrap().chain.len(), 1);
        // IFD0 → garbage offset: chain just ends.
        let f = file(
            Endian::Little,
            &[(tag::ORIENTATION, Val::Short(vec![1]))],
            0xFFFF_FF00,
        );
        let src = SliceSource::new(&f);
        assert_eq!(Tiff::parse(&src, false).unwrap().chain.len(), 1);
        // A long legit chain is capped at MAX_CHAIN.
        let mut f = header(Endian::Big);
        for _ in 0..(MAX_CHAIN + 4) {
            let at = f.len() as u32;
            let next = at + 18;
            f.extend(ifd(
                Endian::Big,
                at,
                &[(tag::ORIENTATION, Val::Short(vec![1]))],
                next,
            ));
        }
        f.extend_from_slice(&[0, 0]);
        let src = SliceSource::new(&f);
        assert_eq!(Tiff::parse(&src, false).unwrap().chain.len(), MAX_CHAIN);
    }

    #[test]
    fn variant_magics_only_when_allowed() {
        for (magic, e) in [
            (*b"IIRO", Endian::Little),
            (*b"IIRS", Endian::Little),
            (*b"IIU\0", Endian::Little),
            (*b"MMOR", Endian::Big),
        ] {
            assert_eq!(Tiff::sniff(&magic, true), Some(e));
            assert_eq!(Tiff::sniff(&magic, false), None);
        }
        assert_eq!(Tiff::sniff(b"II*\0", false), Some(Endian::Little));
        assert_eq!(Tiff::sniff(b"II", true), None);
    }

    #[test]
    fn reads_are_bounded_by_need_not_by_declared_count() {
        // A LONG entry claiming 2^32-1 elements whose offset points inside a
        // small file: the scalar read touches one element and succeeds, the
        // array read is capped at MAX_ARRAY (and here runs past EOF).
        let mut f = file(
            Endian::Little,
            &[(
                tag::IMAGE_WIDTH,
                Val::Raw {
                    typ: 4,
                    count: u32::MAX,
                    bytes: 8u32.to_le_bytes().to_vec(),
                },
            )],
            0,
        );
        f.resize(64, 0);
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        // Element 0 at offset 8 is the entry count (1) as LE u16 + first bytes of the entry.
        assert!(t.ifd0().uint(&src, tag::IMAGE_WIDTH).unwrap().is_some());
        assert!(matches!(
            t.ifd0().uints(&src, tag::IMAGE_WIDTH),
            Err(RawError::Truncated(_))
        ));
        let e = t.ifd0().get(tag::IMAGE_WIDTH).unwrap();
        assert_eq!(e.byte_len(), Some(4 * u64::from(u32::MAX)));
        assert_eq!(t.ifd0().bytes_prefix(&src, e, 8).unwrap().len(), 8);
        // Long ASCII values are cut at MAX_ASCII.
        let long: &'static str = Box::leak("x".repeat(10_000).into_boxed_str());
        let f = file(Endian::Little, &[(tag::MAKE, Val::Ascii(long))], 0);
        let src = SliceSource::new(&f);
        let t = Tiff::parse(&src, false).unwrap();
        assert_eq!(
            t.ifd0().ascii(&src, tag::MAKE).unwrap().unwrap().len(),
            MAX_ASCII as usize
        );
        // Unknown types have no byte length.
        let bad = Entry {
            tag: 1,
            typ: 99,
            count: 1,
            field: [0; 4],
        };
        assert_eq!(bad.byte_len(), None);
    }
}
