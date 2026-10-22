//! Canon CR3 (ISO base media file format): the embedded previews, the EXIF
//! orientation, and the sensor sizes bounded before rawler decodes.
//!
//! Layout (Laurent Clévy's CR3 notes): a top-level `uuid` box
//! (`eaf42b5e-…`) holds `PRVW`, a ~1620×1080 JPEG; `moov` holds Canon's
//! `uuid` box (`85c0b687-…`) with `CMT1` (a TIFF whose IFD0 is the main EXIF
//! IFD) and `THMB` (a 160×120 JPEG); each `trak`'s sample description has a
//! `CMP1` box giving the CRX frame and tile size. Box walks are bounded in
//! count and depth, and every offset is checked by the [`Source`].
//!
//! The preview walk is ours and lenient. Before rawler sees a CR3 at all,
//! [`validate`] checks the box tree is one rawler's reader can get through, and
//! before it decodes one, [`check_decodable`] bounds the sizes rawler itself
//! parsed.

#[cfg(feature = "rawler")]
use std::panic::{AssertUnwindSafe, catch_unwind};

use ds_core::Orientation;
#[cfg(feature = "rawler")]
use rawler::formats::bmff::FileBox;
#[cfg(feature = "rawler")]
use rawler::rawsource::RawSource;

use crate::RawError;
use crate::container::Container;
#[cfg(feature = "rawler")]
use crate::guard::{COMPRESSED_PER_BYTE, check};
use crate::preview::{EmbeddedPreview, ifd0_orientation, largest};
use crate::source::{Shifted, Source};

/// Canon's metadata box inside `moov`.
const UUID_CANON: [u8; 16] = [
    0x85, 0xc0, 0xb6, 0x87, 0x82, 0x0f, 0x11, 0xe0, 0x81, 0x11, 0xf4, 0xce, 0x46, 0x2b, 0x6a, 0x48,
];
/// The top-level box holding `PRVW`.
const UUID_PREVIEW: [u8; 16] = [
    0xea, 0xf4, 0x2b, 0x5e, 0x1c, 0x98, 0x4b, 0x88, 0xb9, 0xfb, 0xb7, 0xdc, 0x40, 0x6e, 0x4d, 0x16,
];
/// Most boxes read at one level (real files: under 20).
const MAX_BOXES: usize = 64;

/// One box: its type, and where its payload and the box end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bx {
    typ: [u8; 4],
    body: u64,
    end: u64,
}

impl Bx {
    /// The payload of a `uuid` box after its 16-byte id, if the id is `id`.
    fn uuid_body(&self, src: &dyn Source, id: &[u8; 16]) -> Option<u64> {
        (&self.typ == b"uuid" && src.read(self.body, 16).ok()?.as_ref() == id)
            .then_some(self.body + 16)
    }
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// The boxes laid end to end in `start..end`. Stops at the first box whose
/// header is unreadable or whose size doesn't fit.
fn boxes(src: &dyn Source, start: u64, end: u64) -> Vec<Bx> {
    let mut out = Vec::new();
    let mut at = start;
    while at + 8 <= end && out.len() < MAX_BOXES {
        let Ok(h) = src.read(at, 8) else { break };
        let (size, typ) = (u64::from(be32(&h, 0)), [h[4], h[5], h[6], h[7]]);
        let (size, head) = match size {
            0 => (end - at, 8),
            1 => match src.read(at + 8, 8) {
                Ok(l) => (
                    u64::from_be_bytes(l.as_ref().try_into().unwrap_or([0; 8])),
                    16,
                ),
                Err(_) => break,
            },
            n => (n, 8),
        };
        if size < head || at.checked_add(size).is_none_or(|e| e > end) {
            break;
        }
        out.push(Bx {
            typ,
            body: at + head,
            end: at + size,
        });
        at += size;
    }
    out
}

/// Canon's `uuid` box inside `moov`: its children.
fn canon_boxes(src: &dyn Source, top: &[Bx]) -> Vec<Bx> {
    top.iter()
        .filter(|b| &b.typ == b"moov")
        .flat_map(|m| boxes(src, m.body, m.end))
        .find_map(|b| Some((b.uuid_body(src, &UUID_CANON)?, b.end)))
        .map(|(body, end)| boxes(src, body, end))
        .unwrap_or_default()
}

/// The JPEG of a `PRVW` / `THMB` box: 16 header bytes, then the JPEG, whose
/// length is at +12 (`PRVW`) or +8 (`THMB`).
fn jpeg_in(src: &dyn Source, b: &Bx) -> Option<(u64, u64)> {
    let h = src.read(b.body, 16).ok()?;
    let len = match &b.typ {
        b"PRVW" => be32(&h, 12),
        b"THMB" => be32(&h, 8),
        _ => return None,
    };
    // Clamped to the box: the JPEG can't run into the next one.
    Some((
        b.body + 16,
        u64::from(len).min(b.end.saturating_sub(b.body + 16)),
    ))
}

/// Orientation from `CMT1` (IFD0 of an embedded TIFF), `Normal` if absent.
fn orientation(src: &dyn Source, canon: &[Bx]) -> Orientation {
    canon
        .iter()
        .find(|b| &b.typ == b"CMT1")
        .and_then(|b| {
            let sh = Shifted {
                inner: src,
                delta: b.body,
            };
            Container::parse(&sh)
                .ok()
                .map(|c| ifd0_orientation(&sh, &c))
        })
        .unwrap_or(Orientation::Normal)
}

/// The largest of `PRVW` and `THMB`.
pub(crate) fn preview(src: &dyn Source) -> Result<EmbeddedPreview, RawError> {
    let top = boxes(src, 0, src.end());
    let canon = canon_boxes(src, &top);
    let mut cands: Vec<(u64, u64)> = canon.iter().filter_map(|b| jpeg_in(src, b)).collect();
    for b in &top {
        if let Some(body) = b.uuid_body(src, &UUID_PREVIEW) {
            // 8 bytes (version, count) precede the child boxes.
            cands.extend(
                boxes(src, body + 8, b.end)
                    .iter()
                    .filter_map(|c| jpeg_in(src, c)),
            );
        }
    }
    largest(src, cands, orientation(src, &canon))
}

/// Where rawler's BMFF reader is: which containers it descends into, and how
/// many payload bytes it reads before their children (`rawler::formats::bmff`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    File,
    Moov,
    Canon,
    Trak,
    Mdia,
    Minf,
    Stbl,
    Stsd,
    Craw,
    Cdi1,
    Cctp,
}

impl Ctx {
    /// The child container `typ` opens here, and the bytes before its children.
    fn child(self, typ: [u8; 4], canon: bool) -> Option<(Self, u64)> {
        Some(match (self, &typ) {
            (Self::File, b"moov") => (Self::Moov, 0),
            (Self::Moov, b"trak") => (Self::Trak, 0),
            (Self::Moov, b"uuid") if canon => (Self::Canon, 0),
            (Self::Canon, b"CCTP") => (Self::Cctp, 12), // version/flags, 2 × u32
            (Self::Trak, b"mdia") => (Self::Mdia, 0),
            (Self::Mdia, b"minf") => (Self::Minf, 0),
            (Self::Minf, b"stbl") => (Self::Stbl, 0),
            (Self::Stbl, b"stsd") => (Self::Stsd, 8), // version/flags, entry count
            (Self::Stsd, b"CRAW") => (Self::Craw, 82), // sample entry fields
            (Self::Craw, b"CDI1") => (Self::Cdi1, 4), // version/flags
            _ => return None,
        })
    }
}

/// Most boxes [`validate`] accepts in a file (real CR3: ~40).
const MAX_WALKED: usize = 1 << 16;
/// Most IFDs in a `CMT` TIFF's chain (real files: 1).
const MAX_CMT_CHAIN: usize = 64;

/// Check the box tree is one rawler's reader gets through, before any rawler
/// call (metadata or decode). Its container loops never check progress: a box
/// whose size is 0, or smaller than its own header, makes them re-read the same
/// header forever while a `Vec` grows, until the process aborts. So every box
/// rawler will read, in every container it descends into, must hold its header
/// and fit in its parent. The `CMT1`–`CMT4` TIFFs are parsed with an unbounded
/// IFD chain, so a chain that loops is refused too.
pub(crate) fn validate(src: &dyn Source) -> Result<(), RawError> {
    let mut walked = 0;
    walk(src, Ctx::File, 0, src.end(), &mut walked)
}

fn walk(
    src: &dyn Source,
    ctx: Ctx,
    start: u64,
    end: u64,
    walked: &mut usize,
) -> Result<(), RawError> {
    let bad = |what: String| RawError::Malformed(format!("CR3 box tree: {what}"));
    let mut at = start;
    while at < end {
        *walked += 1;
        if *walked > MAX_WALKED {
            return Err(bad(format!("more than {MAX_WALKED} boxes")));
        }
        let h = src
            .read(at, 8)
            .ok()
            .filter(|_| at + 8 <= end)
            .ok_or_else(|| bad(format!("box header at {at} past its parent")))?;
        let typ = [h[4], h[5], h[6], h[7]];
        let (mut size, mut head) = (u64::from(be32(&h, 0)), 8);
        if size == 1 {
            let l = src
                .read(at + 8, 8)
                .map_err(|_| bad(format!("box size at {at}")))?;
            size = u64::from_be_bytes(l.as_ref().try_into().unwrap_or([0; 8]));
            head = 16;
        }
        let mut canon = false;
        if &typ == b"uuid" {
            let id = src
                .read(at + head, 16)
                .map_err(|_| bad(format!("uuid at {at}")))?;
            canon = id.as_ref() == UUID_CANON;
            head += 16;
        }
        if size < head || at.checked_add(size).is_none_or(|e| e > end) {
            return Err(bad(format!(
                "{} box at {at} has size {size}",
                String::from_utf8_lossy(&typ)
            )));
        }
        let (body, box_end) = (at + head, at + size);
        if let Some((inner, skip)) = ctx.child(typ, canon) {
            walk(src, inner, body + skip, box_end, walked)?;
        } else if ctx == Ctx::Canon && matches!(&typ, b"CMT1" | b"CMT2" | b"CMT3" | b"CMT4") {
            let tiff = src.read(body, box_end - body)?;
            if !tiff_chain_ends(&tiff) {
                return Err(bad(format!(
                    "{} IFD chain loops",
                    String::from_utf8_lossy(&typ)
                )));
            }
        }
        at = box_end;
    }
    Ok(())
}

/// Whether rawler's TIFF reader (`GenericTiffReader` over a `CMT` box: no chain
/// limit) reaches the end of the IFD chain. Mirrors where `IFD::new` leaves
/// its reader: after the last entry it got through, or at the end of the data
/// when a value runs past it (the chain then ends).
fn tiff_chain_ends(d: &[u8]) -> bool {
    let little = match d.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return true, // rawler refuses it
    };
    let u16_at = |i: usize| {
        let b = d.get(i..i + 2)?;
        Some(if little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let u32_at = |i: usize| {
        let b: [u8; 4] = d.get(i..i + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    let mut next = u32_at(4).unwrap_or(0);
    let mut seen = Vec::new();
    while next != 0 {
        if seen.contains(&next) || seen.len() >= MAX_CMT_CHAIN {
            return false;
        }
        seen.push(next);
        let off = next as usize;
        let Some(n) = u16_at(off).filter(|&n| n > 0) else {
            return true; // rawler fails on it: the chain ends there
        };
        next = ifd_next(d, off, n, &u16_at, &u32_at);
    }
    true
}

/// The next-IFD pointer rawler reads after the `n` entries of the IFD at `off`.
fn ifd_next(
    d: &[u8],
    off: usize,
    n: u16,
    u16_at: &dyn Fn(usize) -> Option<u16>,
    u32_at: &dyn Fn(usize) -> Option<u32>,
) -> u32 {
    // Bytes per element of each TIFF type (rawler's DATASHIFTS); 0 and > 12 count as bytes.
    const SHIFT: [u8; 13] = [0, 0, 0, 1, 2, 3, 0, 0, 1, 2, 3, 2, 3];
    let mut errors = 0;
    let mut k = 0;
    for i in 0..usize::from(n).min(4096) {
        let p = off + 2 + 12 * i;
        let (Some(_tag), Some(typ), Some(count)) = (u16_at(p), u16_at(p + 2), u32_at(p + 4)) else {
            return 0; // end of the data: the next pointer can't be read either
        };
        let shift = SHIFT
            .get(usize::from(typ))
            .copied()
            .filter(|_| typ != 0)
            .unwrap_or(0);
        let bytes = u64::from(count) << shift;
        let ok = if bytes <= 4 {
            if p + 12 > d.len() {
                return 0;
            }
            true
        } else {
            let Some(v) = u32_at(p + 8) else { return 0 };
            if v == u32::MAX {
                false // rawler's "invalid offset": skipped, reader after the entry
            } else if u64::from(v) + bytes > d.len() as u64 {
                return 0; // the value read hits the end of the data
            } else {
                true
            }
        };
        k = i + 1;
        if ok {
            errors = 0;
        } else {
            errors += 1;
            if errors >= 5 {
                break;
            }
        }
    }
    u32_at(off + 2 + 12 * k).unwrap_or(0)
}

/// Parse the box tree with rawler's own reader (after [`validate`]).
#[cfg(feature = "rawler")]
fn filebox(rs: &RawSource) -> Result<FileBox, RawError> {
    match catch_unwind(AssertUnwindSafe(|| FileBox::parse(rs.reader()))) {
        Ok(Ok(fb)) => Ok(fb),
        Ok(Err(e)) => Err(RawError::Malformed(format!("CR3: {e}"))),
        Err(_) => Err(RawError::Malformed("CR3: box parser panicked".into())),
    }
}

/// Bound every `CMP1` rawler parsed (frame and tile size) before it sizes CRX
/// buffers from them. The values come from rawler's own `FileBox` (which keeps
/// the last `moov`), so no other parse can disagree with them.
#[cfg(feature = "rawler")]
fn bound_cmp1(fb: &FileBox, len: u64) -> Result<(), RawError> {
    let mut found = false;
    for t in &fb.moov.traks {
        let Some(c) = t
            .mdia
            .minf
            .stbl
            .stsd
            .craw
            .as_ref()
            .and_then(|c| c.cmp1.as_ref())
        else {
            continue;
        };
        let frame = u64::from(c.f_width) * u64::from(c.f_height);
        let tile = u64::from(c.tile_width) * u64::from(c.tile_height);
        check("CR3 CMP1 frame", frame, COMPRESSED_PER_BYTE, len)?;
        check("CR3 CMP1 tile", tile, COMPRESSED_PER_BYTE, len)?;
        found = true;
    }
    if found {
        Ok(())
    } else {
        Err(RawError::Malformed("CR3 without a CMP1 box".into()))
    }
}

/// Everything rawler's CR3 decode sizes from the header is bounded. Call
/// after [`validate`].
#[cfg(feature = "rawler")]
pub(crate) fn check_decodable(rs: &RawSource) -> Result<(), RawError> {
    bound_cmp1(&filebox(rs)?, rs.buf().len() as u64)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::preview::tests::fake_jpeg;
    use crate::source::SliceSource;
    use crate::tiff::build::{Val, header, ifd};
    use crate::tiff::{Endian, tag};

    pub fn bx(typ: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(&typ);
        b.extend_from_slice(body);
        b
    }

    fn uuid(id: &[u8; 16], body: &[u8]) -> Vec<u8> {
        bx(*b"uuid", &[id.as_slice(), body].concat())
    }

    /// `PRVW`/`THMB` payload: 16 header bytes then the JPEG.
    fn jpeg_box(typ: [u8; 4], w: u16, h: u16) -> Vec<u8> {
        let j = fake_jpeg(w, h, usize::from(w)); // bigger frames, bigger files
        let mut p = vec![0u8; 16];
        let at = if &typ == b"PRVW" { 12 } else { 8 };
        p[at..at + 4].copy_from_slice(&(j.len() as u32).to_be_bytes());
        p.extend_from_slice(&j);
        bx(typ, &p)
    }

    fn cmp1(w: u32, h: u32, tw: u32, th: u32) -> Vec<u8> {
        let mut p = vec![0xFF, 0x00, 0x00, 0x30, 0x01, 0x00, 0x00, 0x00];
        for v in [w, h, tw, th] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p.extend_from_slice(&[0; 16]);
        bx(*b"CMP1", &p)
    }

    fn tiff_with_orientation(orientation: u16) -> Vec<u8> {
        let mut t = header(Endian::Little);
        t.extend(ifd(
            Endian::Little,
            8,
            &[(tag::ORIENTATION, Val::Short(vec![orientation]))],
            0,
        ));
        t
    }

    /// One `trak` as rawler's reader wants it (`tkhd`, `mdia`/`minf`/`stbl`
    /// with `stsd` → `CRAW` → `CMP1`, and the sample tables), zero-filled.
    fn trak(frame: (u32, u32)) -> Vec<u8> {
        let mut craw = vec![0u8; 82];
        craw.extend(cmp1(frame.0, frame.1, frame.0 / 2, frame.1 / 2));
        let stsd = [[0u8, 0, 0, 0, 0, 0, 0, 1].as_slice(), &bx(*b"CRAW", &craw)].concat();
        let stbl = [
            bx(*b"stsd", &stsd),
            bx(*b"stts", &[0; 8]),
            bx(*b"stsc", &[0; 8]),
            bx(*b"stsz", &[0; 12]),
        ]
        .concat();
        let minf = [bx(*b"dinf", &[]), bx(*b"stbl", &stbl)].concat();
        let mdia = [
            bx(*b"mdhd", &[0; 24]),
            bx(*b"hdlr", &[0; 25]),
            bx(*b"minf", &minf),
        ]
        .concat();
        bx(
            *b"trak",
            &[bx(*b"tkhd", &[0; 84]), bx(*b"mdia", &mdia)].concat(),
        )
    }

    /// A minimal CR3 that rawler's box reader accepts: ftyp; moov (mvhd,
    /// Canon's uuid with CNCV, CCTP, CTBO, CMT1–4 and THMB, one trak per
    /// frame size); the PRVW uuid; then `pad` bytes of mdat.
    pub fn cr3_traks(orientation: u16, frames: &[(u32, u32)], pad: usize) -> Vec<u8> {
        let cmt1 = tiff_with_orientation(orientation);
        let other = tiff_with_orientation(1);
        let canon = uuid(
            &UUID_CANON,
            &[
                bx(*b"CNCV", &[b'0'; 30]),
                bx(*b"CCTP", &[0; 12]),
                bx(*b"CTBO", &[]),
                bx(*b"CMT1", &cmt1),
                bx(*b"CMT2", &other),
                bx(*b"CMT3", &other),
                bx(*b"CMT4", &other),
                jpeg_box(*b"THMB", 160, 120),
            ]
            .concat(),
        );
        let mut moov = [bx(*b"mvhd", &[0; 100]), canon].concat();
        for &fr in frames {
            moov.extend(trak(fr));
        }
        let mut f = bx(*b"ftyp", b"crx \0\0\0\x01crx isom");
        f.extend(bx(*b"moov", &moov));
        f.extend(uuid(
            &UUID_PREVIEW,
            &[[0u8; 8].as_slice(), &jpeg_box(*b"PRVW", 1620, 1080)].concat(),
        ));
        f.extend(bx(*b"mdat", &vec![0; pad]));
        f
    }

    pub fn cr3(orientation: u16, frame: (u32, u32), pad: usize) -> Vec<u8> {
        cr3_traks(orientation, &[frame], pad)
    }

    #[cfg(feature = "rawler")]
    fn decodable(f: &[u8]) -> Result<(), RawError> {
        validate(&SliceSource::new(f))?;
        check_decodable(&RawSource::new_from_slice(f))
    }

    #[test]
    fn preview_is_prvw_with_cmt1_orientation() {
        let f = cr3(8, (64, 48), 64);
        let src = SliceSource::new(&f);
        let p = preview(&src).unwrap();
        assert_eq!(p.size, ds_core::Size::new(1620, 1080));
        assert_eq!(p.orientation, Orientation::from_exif(8));
        assert!(p.jpeg.starts_with(&[0xFF, 0xD8]) && p.jpeg.ends_with(&[0xFF, 0xD9]));
    }

    #[test]
    fn thumbnail_when_prvw_is_missing_or_broken() {
        let mut f = cr3(1, (64, 48), 0);
        // Corrupt PRVW's JPEG (the last SOI in the file).
        let soi = f.windows(2).rposition(|w| w == [0xFF, 0xD8]).unwrap();
        f[soi] = 0;
        let p = preview(&SliceSource::new(&f)).unwrap();
        assert_eq!(p.size, ds_core::Size::new(160, 120));
        assert_eq!(p.orientation, Orientation::Normal);
        // Nothing at all.
        let f = [bx(*b"ftyp", b"crx "), bx(*b"moov", &[])].concat();
        assert!(matches!(
            preview(&SliceSource::new(&f)),
            Err(RawError::NoPreview)
        ));
    }

    #[test]
    fn box_walk_stops_at_bad_sizes() {
        // Size 0 runs to the end; size 1 reads a 64-bit size.
        let mut f = bx(*b"ftyp", b"crx ");
        f.extend_from_slice(&0u32.to_be_bytes());
        f.extend_from_slice(b"mdat\x01\x02");
        let bs = boxes(&SliceSource::new(&f), 0, f.len() as u64);
        assert_eq!(bs.len(), 2);
        assert_eq!(bs[1].end, f.len() as u64);
        let mut f = 1u32.to_be_bytes().to_vec();
        f.extend_from_slice(b"mdat");
        f.extend_from_slice(&20u64.to_be_bytes());
        f.extend_from_slice(&[0; 4]);
        let bs = boxes(&SliceSource::new(&f), 0, f.len() as u64);
        assert_eq!(
            bs,
            [Bx {
                typ: *b"mdat",
                body: 16,
                end: 20
            }]
        );
        // Truncated 64-bit size, a size below the header, a size past the end.
        for f in [
            [1u32.to_be_bytes().as_slice(), b"mdat\0\0"].concat(),
            [4u32.to_be_bytes().as_slice(), b"mdat"].concat(),
            [99u32.to_be_bytes().as_slice(), b"mdat"].concat(),
        ] {
            assert_eq!(boxes(&SliceSource::new(&f), 0, f.len() as u64).len(), 0);
        }
        // A box with a short header body for PRVW yields no candidate.
        let b = Bx {
            typ: *b"PRVW",
            body: 0,
            end: 4,
        };
        assert!(jpeg_in(&SliceSource::new(&[0; 4]), &b).is_none());
        let b = Bx {
            typ: *b"free",
            body: 0,
            end: 16,
        };
        assert!(jpeg_in(&SliceSource::new(&[0; 16]), &b).is_none());
    }

    #[cfg(feature = "rawler")]
    #[test]
    fn cmp1_sizes_come_from_rawlers_parse_and_are_bounded() {
        let ok = cr3(1, (64, 48), 64 * 48);
        assert!(decodable(&ok).is_ok());
        // 65535² claimed in a small file.
        let big = cr3(1, (65_535, 65_535), 0);
        assert!(matches!(
            decodable(&big),
            Err(RawError::Malformed(m)) if m.contains("CMP1 frame")
        ));
        // A tile larger than the file allows.
        let mut fb = filebox(&RawSource::new_from_slice(&ok)).unwrap();
        let c = fb.moov.traks[0].mdia.minf.stbl.stsd.craw.as_mut().unwrap();
        c.cmp1.as_mut().unwrap().tile_width = u32::MAX;
        assert!(matches!(
            bound_cmp1(&fb, 1024),
            Err(RawError::Malformed(m)) if m.contains("CMP1 tile")
        ));
        // No CMP1 at all.
        fb.moov.traks[0].mdia.minf.stbl.stsd.craw = None;
        assert!(matches!(
            bound_cmp1(&fb, 1024),
            Err(RawError::Malformed(m)) if m.contains("without a CMP1")
        ));
        // rawler's reader refuses the tree (no mvhd): so do we.
        let bare = [bx(*b"ftyp", b"crx "), bx(*b"moov", &[]), bx(*b"mdat", &[])].concat();
        assert!(matches!(
            decodable(&bare),
            Err(RawError::Malformed(m)) if m.starts_with("CR3: ")
        ));
    }

    /// The review's case: a benign moov in the first 64 boxes, a second one
    /// with a hostile CMP1 after them. rawler keeps the last moov, and so do
    /// the bounds (they're read from rawler's own parse).
    #[cfg(feature = "rawler")]
    #[test]
    fn a_later_moov_is_bounded() {
        let mut f = cr3(1, (64, 48), 64 * 48);
        let first = cr3(1, (64, 48), 0);
        let mut later = Vec::new();
        for _ in 0..70 {
            later.extend(bx(*b"free", &[]));
        }
        // The second file's moov (after its ftyp).
        let ftyp_len = 8 + 16;
        let moov_len = be32(&first, ftyp_len) as usize;
        let mut moov2 = first[ftyp_len..ftyp_len + moov_len].to_vec();
        let at = moov2.windows(4).position(|w| w == b"CMP1").unwrap() + 4 + 8;
        moov2[at..at + 8].copy_from_slice(&[0, 0, 0xEA, 0x60, 0, 0, 0xEA, 0x60]); // 60000²
        later.extend(moov2);
        f.extend(later);
        assert!(matches!(
            decodable(&f),
            Err(RawError::Malformed(m)) if m.contains("CMP1 frame")
        ));
    }

    /// Fuzz case (5 KB) whose moov holds a zero-size box: rawler's moov loop
    /// re-read it forever, growing a Vec until the process aborted.
    #[test]
    fn non_advancing_boxes_never_reach_rawler() {
        let f = include_bytes!("../tests/fixtures/cr3-moov-loop.bin");
        assert!(matches!(
            validate(&SliceSource::new(f)),
            Err(RawError::Malformed(m)) if m.starts_with("CR3 box tree")
        ));
        assert!(crate::decode_bytes(f).is_err());
        assert!(crate::read_metadata_bytes(f).is_err());
        // The same shape, built: size 0, size below the header, size past the parent.
        for (size, what) in [(0u32, "size 0"), (4, "size 4"), (999, "size 999")] {
            let bad = [size.to_be_bytes().as_slice(), b"free"].concat();
            let f = [bx(*b"ftyp", b"crx "), bx(*b"moov", &bad)].concat();
            assert!(
                matches!(validate(&SliceSource::new(&f)), Err(RawError::Malformed(m)) if m.contains(what)),
                "{what}"
            );
        }
        // A header that doesn't fit its parent; a 64-bit size; a truncated uuid.
        let f = [bx(*b"ftyp", b"crx "), bx(*b"moov", b"\0\0")].concat();
        assert!(validate(&SliceSource::new(&f)).is_err());
        let mut big = 1u32.to_be_bytes().to_vec();
        big.extend_from_slice(b"free");
        big.extend_from_slice(&16u64.to_be_bytes());
        assert!(validate(&SliceSource::new(&big)).is_ok());
        let f = [1u32.to_be_bytes().as_slice(), b"free\0\0"].concat();
        assert!(validate(&SliceSource::new(&f)).is_err());
        let f = [24u32.to_be_bytes().as_slice(), b"uuid\0\0"].concat();
        assert!(validate(&SliceSource::new(&f)).is_err());
        // The real-shaped synthetic file passes.
        assert!(validate(&SliceSource::new(&cr3(1, (64, 48), 0))).is_ok());
    }

    #[test]
    fn box_count_is_capped() {
        let mut f = bx(*b"ftyp", b"crx ");
        for _ in 0..MAX_WALKED {
            f.extend(bx(*b"free", &[]));
        }
        assert!(matches!(
            validate(&SliceSource::new(&f)),
            Err(RawError::Malformed(m)) if m.contains("more than")
        ));
    }

    /// TIFF bytes: header, then IFDs at the given offsets, each with `n` entries
    /// of (`typ`, `count`, `value`) and a next pointer.
    /// `(at, n, entry, next)`: an IFD at `at` with `n` copies of `entry`.
    type TestIfd = (u32, u16, (u16, u32, u32), u32);

    fn tiff(ifds: &[TestIfd]) -> Vec<u8> {
        let mut d = b"II*\0".to_vec();
        d.extend_from_slice(&ifds[0].0.to_le_bytes());
        for &(at, n, (typ, count, value), next) in ifds {
            d.resize(at as usize, 0);
            d.extend_from_slice(&n.to_le_bytes());
            for _ in 0..n {
                d.extend_from_slice(&0x0112u16.to_le_bytes());
                d.extend_from_slice(&typ.to_le_bytes());
                d.extend_from_slice(&count.to_le_bytes());
                d.extend_from_slice(&value.to_le_bytes());
            }
            d.extend_from_slice(&next.to_le_bytes());
        }
        d
    }

    #[test]
    fn cmt_chains_that_loop_are_refused() {
        let short = (3, 1, 1); // SHORT, inline
        // A two-IFD chain that ends; one that loops back; one that loops on itself.
        assert!(tiff_chain_ends(&tiff(&[
            (8, 1, short, 32),
            (32, 1, short, 0)
        ])));
        assert!(!tiff_chain_ends(&tiff(&[
            (8, 1, short, 32),
            (32, 1, short, 8)
        ])));
        assert!(!tiff_chain_ends(&tiff(&[(8, 1, short, 8)])));
        // Big-endian, an unknown marker, an empty or unreadable IFD: rawler stops there.
        let mut be = b"MM\0*\0\0\0\x08\0\x01".to_vec();
        be.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 8]);
        assert!(!tiff_chain_ends(&be));
        assert!(tiff_chain_ends(b"XX*\0\x08\0\0\0"));
        assert!(tiff_chain_ends(&tiff(&[(8, 0, short, 8)])));
        assert!(tiff_chain_ends(b"II*\0\xFF\0\0\0"));
        // A value past the data ends the chain (rawler's read hits the end).
        assert!(tiff_chain_ends(&tiff(&[(8, 1, (4, 2, 4000), 8)])));
        // Invalid offsets (u32::MAX) are skipped, and the next pointer still read:
        // fewer than 5 in a row don't stop the IFD.
        assert!(!tiff_chain_ends(&tiff(&[(8, 3, (4, 2, u32::MAX), 8)])));
        // Five in a row stop it after the fifth: the pointer after that entry.
        let mut d = tiff(&[(8, 6, (4, 2, u32::MAX), 0)]);
        d[8 + 2 + 12 * 5..8 + 2 + 12 * 5 + 4].copy_from_slice(&8u32.to_le_bytes());
        assert!(!tiff_chain_ends(&d));
        // An entry cut short at the end of the data.
        let mut d = tiff(&[(8, 1, short, 0)]);
        d.truncate(8 + 2 + 6);
        assert!(tiff_chain_ends(&d));
        let mut d = tiff(&[(8, 1, (4, 2, 0), 0)]);
        d.truncate(8 + 2 + 10);
        assert!(tiff_chain_ends(&d));
        let mut d = tiff(&[(8, 1, short, 0)]);
        d.truncate(8 + 2 + 10);
        assert!(tiff_chain_ends(&d));
        // A chain longer than any real one.
        let ifds: Vec<_> = (0..=MAX_CMT_CHAIN as u32)
            .map(|k| (8 + 32 * k, 1, short, 8 + 32 * (k + 1)))
            .collect();
        assert!(!tiff_chain_ends(&tiff(&ifds)));

        // Through the box walk.
        let looped = tiff(&[(8, 1, short, 8)]);
        let canon = uuid(&UUID_CANON, &bx(*b"CMT2", &looped));
        let f = [bx(*b"ftyp", b"crx "), bx(*b"moov", &canon)].concat();
        assert!(matches!(
            validate(&SliceSource::new(&f)),
            Err(RawError::Malformed(m)) if m.contains("CMT2 IFD chain loops")
        ));
    }
}
