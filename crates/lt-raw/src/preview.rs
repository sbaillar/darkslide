//! Embedded JPEG preview extraction without decoding anything.

use lt_core::{Orientation, Size};

use crate::RawError;
use crate::container::Container;
use crate::source::{Shifted, Source};
use crate::tiff::{Endian, Ifd, tag};

/// Candidates fully read at most (each is header-checked first).
const MAX_ATTEMPTS: usize = 4;
/// Embedded previews are 1-8 MB; a longer claim is a hostile or broken tag.
const MAX_PREVIEW_BYTES: u64 = 64 << 20;
/// Bytes read to check a candidate's SOI and frame header before reading it whole.
const HEAD_PROBE: u64 = 64 * 1024;
/// The frame header can follow several `APPn` segments of up to 64 KB each
/// (Fujifilm's JPEGs put it ~78 KB in, after two `Exif` blocks): a JPEG whose
/// header isn't in the first probe is read this far once.
const HEAD_PROBE_MAX: u64 = 256 * 1024;
/// Lossless `SOFn` markers (3, 7, 11, 15): sensor data, never a preview.
const LOSSLESS_SOF: [u8; 4] = [0xC3, 0xC7, 0xCB, 0xCF];
/// Olympus maker-note tag pointing at the `CameraSettings` IFD.
const OLYMPUS_CAMERA_SETTINGS: u16 = 0x2020;

/// Largest JPEG preview embedded in a RAW, as stored (not decoded, not rotated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedPreview {
    /// Complete JPEG file bytes (SOI … EOI).
    pub jpeg: Vec<u8>,
    /// Pixel size from the JPEG frame header, as stored (before `orientation`).
    pub size: Size,
    /// Orientation to apply for display; same as the RAW's EXIF orientation.
    pub orientation: Orientation,
}

/// `(offset, length)` of every JPEG the container points at, deduplicated.
/// Unreadable tags are skipped.
fn candidates(src: &dyn Source, c: &Container) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for ifd in c.tiff.chain.iter().chain(&c.subs) {
        let u = |t| ifd.uint(src, t).ok().flatten();
        if let (Some(off), Some(len)) = (u(tag::JPEG_OFFSET), u(tag::JPEG_LENGTH)) {
            out.push((u64::from(off), u64::from(len)));
        } else if (u(tag::COMPRESSION) == Some(6)
            // DNG previews: "new-style" JPEG (7) in YCbCr; raw data is CFA or linear.
            || (u(tag::COMPRESSION) == Some(7) && u(tag::PHOTOMETRIC) == Some(6)))
            && let (Some(off), Some(len)) = (u(tag::STRIP_OFFSETS), u(tag::STRIP_BYTE_COUNTS))
        {
            // A JPEG stored as a single strip (e.g. DNG/NEF preview SubIFDs).
            out.push((u64::from(off), u64::from(len)));
        }
        // Panasonic RW2: a whole JPEG file as an UNDEFINED value in IFD0.
        if let Some(e) = ifd.get(tag::PANA_JPG_FROM_RAW).filter(|e| e.count > 4) {
            out.push((u64::from(ifd.endian.u32(e.field)), u64::from(e.count)));
        }
    }
    out.extend(olympus_preview(src, c));
    out.sort_unstable();
    out.dedup();
    out
}

/// Olympus / OM System ORF: the preview is listed in the maker note's
/// `CameraSettings` IFD (0x2020: `PreviewImageStart` 0x0101, `PreviewImageLength`
/// 0x0102). Offsets are relative to the maker note, except in the old `OLYMP\0`
/// layout where they are file offsets.
fn olympus_preview(src: &dyn Source, c: &Container) -> Option<(u64, u64)> {
    let mn = c.exif.as_ref()?.get(tag::MAKER_NOTE)?;
    if mn.count <= 4 {
        return None;
    }
    let at = u64::from(c.tiff.endian.u32(mn.field));
    let head = src.read(at, 16).ok()?;
    let (ifd_at, delta, order) = if head.starts_with(b"OLYMPUS\0") {
        (12, at, &head[8..10])
    } else if head.starts_with(b"OM SYSTEM\0") {
        (16, at, &head[12..14])
    } else if head.starts_with(b"OLYMP\0") {
        (at + 8, 0, &b"--"[..])
    } else {
        return None;
    };
    let endian = match order {
        b"II" => Endian::Little,
        b"MM" => Endian::Big,
        _ => c.tiff.endian,
    };
    let sh = Shifted { inner: src, delta };
    let root = Ifd::parse(&sh, ifd_at, endian).ok()?;
    let settings = root.uint(&sh, OLYMPUS_CAMERA_SETTINGS).ok()??;
    let settings = Ifd::parse(&sh, u64::from(settings), endian).ok()?;
    let start = settings.uint(&sh, 0x0101).ok()??;
    let len = settings.uint(&sh, 0x0102).ok()??;
    Some((delta + u64::from(start), u64::from(len)))
}

/// Extract the largest embedded preview. Candidates that are out of range or
/// not JPEG are skipped (after a cheap header probe, so a bogus multi-MB
/// candidate costs one small read); `NoPreview` if none is usable.
pub(crate) fn from_container(src: &dyn Source, c: &Container) -> Result<EmbeddedPreview, RawError> {
    largest(src, candidates(src, c), ifd0_orientation(src, c))
}

/// The EXIF orientation in IFD0, `Normal` when absent.
pub(crate) fn ifd0_orientation(src: &dyn Source, c: &Container) -> Orientation {
    c.ifd0()
        .uint(src, tag::ORIENTATION)
        .ok()
        .flatten()
        .map_or(Orientation::Normal, |v| Orientation::from_exif(v as u16))
}

/// The largest usable JPEG among `cands` (`(offset, length)`, any order).
/// Lossless-JPEG streams (raw data, e.g. CR2's sensor strip) are not previews
/// and are skipped.
pub(crate) fn largest(
    src: &dyn Source,
    mut cands: Vec<(u64, u64)>,
    orientation: Orientation,
) -> Result<EmbeddedPreview, RawError> {
    // Largest first: byte size is a good proxy for pixel count and costs nothing.
    cands.sort_by_key(|&(_, len)| std::cmp::Reverse(len));
    first_valid(src, cands, orientation)
}

/// The first candidate `(offset, len)` that is a real JPEG.
fn first_valid(
    src: &dyn Source,
    cands: Vec<(u64, u64)>,
    orientation: Orientation,
) -> Result<EmbeddedPreview, RawError> {
    let (mut attempts, mut extended) = (0, 0);
    for (off, len) in cands {
        if len > MAX_PREVIEW_BYTES {
            continue;
        }
        let Ok(mut head) = src.read(off, len.min(HEAD_PROBE)) else {
            continue;
        };
        if !head.starts_with(&[0xFF, 0xD8]) {
            continue;
        }
        if jpeg_sof(&head).is_none() && len > HEAD_PROBE && extended < MAX_ATTEMPTS {
            extended += 1;
            let Ok(more) = src.read(off, len.min(HEAD_PROBE_MAX)) else {
                continue;
            };
            head = more;
        }
        if jpeg_sof(&head).is_none_or(|(m, ..)| LOSSLESS_SOF.contains(&m)) {
            continue;
        }
        attempts += 1;
        if attempts > MAX_ATTEMPTS {
            break;
        }
        // The probe may already hold the whole JPEG.
        let bytes = if head.len() as u64 == len {
            head
        } else {
            let Ok(b) = src.read(off, len) else {
                continue;
            };
            b
        };
        if let Some((n, size)) = validate_jpeg(&bytes).map(|(j, size)| (j.len(), size)) {
            let mut jpeg = bytes.into_owned();
            jpeg.truncate(n);
            return Ok(EmbeddedPreview {
                jpeg,
                size,
                orientation,
            });
        }
    }
    Err(RawError::NoPreview)
}

/// Checks SOI, finds the frame size, and trims anything after the last EOI.
pub(crate) fn validate_jpeg(b: &[u8]) -> Option<(&[u8], Size)> {
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let eoi = b.windows(2).rposition(|w| w == [0xFF, 0xD9])?;
    let b = &b[..eoi + 2];
    Some((b, jpeg_size(b)?))
}

/// Frame size from the first `SOFn` marker.
pub(crate) fn jpeg_size(b: &[u8]) -> Option<Size> {
    jpeg_frame(b).map(|(w, h, _)| Size::new(u32::from(w), u32::from(h)))
}

/// `(width, height, components)` from the first `SOFn` marker (baseline,
/// progressive or lossless), skipping over other segments by length.
pub(crate) fn jpeg_frame(b: &[u8]) -> Option<(u16, u16, u8)> {
    jpeg_sof(b).map(|(_, w, h, nc)| (w, h, nc))
}

/// [`jpeg_frame`] plus the `SOFn` marker byte.
fn jpeg_sof(b: &[u8]) -> Option<(u8, u16, u16, u8)> {
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1; // fill byte
            continue;
        }
        if m == 0xD8 || (0xD0..=0xD7).contains(&m) || m == 0x01 {
            i += 2;
            continue;
        }
        let len = usize::from(u16::from_be_bytes([b[i + 2], b[i + 3]]));
        // SOF0..SOF15 except DHT (C4), JPG (C8), DAC (CC).
        if (0xC0..=0xCF).contains(&m) && ![0xC4, 0xC8, 0xCC].contains(&m) {
            let h = u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]);
            let w = u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]);
            let nc = *b.get(i + 9)?;
            return (w > 0 && h > 0).then_some((m, w, h, nc));
        }
        if m == 0xDA || len < 2 {
            return None; // scan data before any frame header
        }
        i += 2 + len;
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Smallest structurally valid JPEG header + EOI with a given frame size.
    pub fn fake_jpeg(w: u16, h: u16, pad: usize) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x4A, 0x46];
        j.extend_from_slice(&[0xFF, 0xFF]); // fill byte
        j.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        j.extend_from_slice(&h.to_be_bytes());
        j.extend_from_slice(&w.to_be_bytes());
        j.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        j.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34]);
        j.extend_from_slice(&[0xFF, 0xD9]);
        j.extend(std::iter::repeat_n(0, pad));
        j
    }

    use crate::RawFile;
    use crate::source::SliceSource;
    use crate::tiff::build::{Val, header, ifd};

    /// IFD0 with `entries` at 8, then each blob at its offset (ascending).
    fn tiff_with(magic: [u8; 4], entries: &[(u16, Val)], blobs: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut f = header(Endian::Little);
        f[..4].copy_from_slice(&magic);
        f.extend(ifd(Endian::Little, 8, entries, 0));
        for (at, b) in blobs {
            assert!(f.len() <= *at as usize);
            f.resize(*at as usize, 0);
            f.extend_from_slice(b);
        }
        f
    }

    fn strip(at: u32, j: &[u8]) -> [(u16, Val); 2] {
        [
            (tag::STRIP_OFFSETS, Val::Long(vec![at])),
            (tag::STRIP_BYTE_COUNTS, Val::Long(vec![j.len() as u32])),
        ]
    }

    fn lossless(w: u16, h: u16) -> Vec<u8> {
        let mut j = fake_jpeg(w, h, 0);
        j[11] = 0xC3; // SOF0 → SOF3
        j
    }

    fn preview_of(f: &[u8]) -> Result<EmbeddedPreview, RawError> {
        RawFile::from_bytes(f)?.preview()
    }

    /// DNG: previews are "new-style" JPEG (7) in YCbCr; compression-7 CFA
    /// data and lossless streams in any IFD are never taken for a preview.
    #[test]
    fn dng_ycbcr_previews_but_not_raw_data() {
        let small = fake_jpeg(1024, 683, 0);
        let big_baseline_cfa = fake_jpeg(6000, 4000, 4000);
        let big_lossless = lossless(6000, 4000);
        let raw = ifd(
            Endian::Little,
            2048,
            &[
                [
                    (tag::COMPRESSION, Val::Short(vec![7])),
                    (tag::PHOTOMETRIC, Val::Short(vec![32803])),
                ]
                .as_slice(),
                &strip(8192, &big_baseline_cfa),
            ]
            .concat(),
            0,
        );
        let cr2_like = ifd(
            Endian::Little,
            3072,
            &[
                [(tag::COMPRESSION, Val::Short(vec![6]))].as_slice(),
                &strip(16384, &big_lossless),
            ]
            .concat(),
            0,
        );
        let f = tiff_with(
            *b"II*\0",
            &[
                [
                    (tag::NEW_SUBFILE_TYPE, Val::Long(vec![1])),
                    (tag::COMPRESSION, Val::Short(vec![7])),
                    (tag::PHOTOMETRIC, Val::Short(vec![6])),
                    (tag::ORIENTATION, Val::Short(vec![3])),
                    (tag::SUB_IFDS, Val::Long(vec![2048, 3072])),
                ]
                .as_slice(),
                &strip(4096, &small),
            ]
            .concat(),
            &[
                (2048, raw),
                (3072, cr2_like),
                (4096, small),
                (8192, big_baseline_cfa),
                (16384, big_lossless),
            ],
        );
        let p = preview_of(&f).unwrap();
        assert_eq!(p.size, Size::new(1024, 683));
        assert_eq!(p.orientation, Orientation::from_exif(3));
    }

    /// RW2: the preview is IFD0's `JpgFromRaw` (an UNDEFINED value).
    #[test]
    fn rw2_jpg_from_raw() {
        let j = fake_jpeg(1920, 1440, 0);
        let f = tiff_with(
            *b"IIU\0",
            &[
                (tag::PANA_JPG_FROM_RAW, Val::Undefined(j)),
                (tag::ORIENTATION, Val::Short(vec![6])),
            ],
            &[],
        );
        let p = preview_of(&f).unwrap();
        assert_eq!(p.size, Size::new(1920, 1440));
        assert_eq!(p.orientation, Orientation::from_exif(6));
        // A 4-byte (inline) value is no JPEG.
        let f = tiff_with(
            *b"IIU\0",
            &[(
                tag::PANA_JPG_FROM_RAW,
                Val::Undefined(vec![0xFF, 0xD8, 0, 0]),
            )],
            &[],
        );
        assert!(matches!(preview_of(&f), Err(RawError::NoPreview)));
    }

    /// An ORF whose maker note (in the Exif IFD at 1024) starts with `head`;
    /// `absolute`: offsets are file offsets (old `OLYMP` notes) rather than
    /// relative to the note.
    fn orf(head: &[u8], absolute: bool, jpeg: &[u8]) -> Vec<u8> {
        let note_at = 1024 + 2 + 12 + 4; // the note is the Exif IFD's only value
        let base = if absolute { note_at } else { 0 };
        let root = head.len() as u32;
        let mut note = head.to_vec();
        note.extend(ifd(
            Endian::Little,
            base + root,
            &[(OLYMPUS_CAMERA_SETTINGS, Val::Long(vec![base + 64]))],
            0,
        ));
        note.resize(64, 0);
        note.extend(ifd(
            Endian::Little,
            base + 64,
            &[
                (0x0101, Val::Long(vec![base + 128])),
                (0x0102, Val::Long(vec![jpeg.len() as u32])),
            ],
            0,
        ));
        note.resize(128, 0);
        note.extend_from_slice(jpeg);
        let exif = ifd(
            Endian::Little,
            1024,
            &[(tag::MAKER_NOTE, Val::Undefined(note))],
            0,
        );
        tiff_with(
            *b"IIRO",
            &[(tag::EXIF_IFD, Val::Long(vec![1024]))],
            &[(1024, exif)],
        )
    }

    #[test]
    fn orf_maker_note_preview() {
        let j = fake_jpeg(1280, 960, 0);
        for (head, absolute) in [
            (b"OLYMPUS\0II\x03\0".as_slice(), false),
            (b"OM SYSTEM\0\0\0II\x04\0".as_slice(), false),
            (b"OLYMP\0\x01\0".as_slice(), true),
        ] {
            let p = preview_of(&orf(head, absolute, &j)).unwrap();
            assert_eq!(p.size, Size::new(1280, 960), "{head:?}");
        }
        // Unknown maker-note layout, or a note without CameraSettings: nothing.
        for f in [
            orf(b"NIKON\0\x02\0\0\0II*\0", false, &j),
            orf(b"OLYMPUS\0MM\x03\0", false, &j), // big-endian table read from LE bytes
        ] {
            assert!(matches!(preview_of(&f), Err(RawError::NoPreview)));
        }
    }

    /// A frame header past the first 64 KB probe (two full `APP1` blocks, as in
    /// Fujifilm's JPEGs) is found with one longer read.
    #[test]
    fn frame_header_behind_big_app_segments() {
        let mut j = vec![0xFF, 0xD8];
        for _ in 0..2 {
            j.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xF0]);
            j.resize(j.len() + 0xFFF0 - 2, 0);
        }
        j.extend_from_slice(&fake_jpeg(4416, 2944, 0)[2..]);
        assert!(j.len() as u64 > HEAD_PROBE);
        let p = largest(
            &SliceSource::new(&j),
            vec![(0, j.len() as u64)],
            Orientation::Normal,
        )
        .unwrap();
        assert_eq!(p.size, Size::new(4416, 2944));
        // Beyond the longer probe too: not a usable preview.
        let mut far = vec![0xFF, 0xD8];
        for _ in 0..5 {
            far.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xF0]);
            far.resize(far.len() + 0xFFF0 - 2, 0);
        }
        far.extend_from_slice(&fake_jpeg(10, 10, 0)[2..]);
        assert!(matches!(
            largest(
                &SliceSource::new(&far),
                vec![(0, far.len() as u64)],
                Orientation::Normal
            ),
            Err(RawError::NoPreview)
        ));
    }

    #[test]
    fn oversized_candidates_are_skipped_unread() {
        struct Fail(u64);
        impl Source for Fail {
            fn end(&self) -> u64 {
                self.0
            }
            fn read(&self, _: u64, _: u64) -> Result<std::borrow::Cow<'_, [u8]>, RawError> {
                panic!("a candidate over the cap must not be read");
            }
        }
        let huge = vec![(0, MAX_PREVIEW_BYTES + 1)];
        assert!(matches!(
            first_valid(&Fail(1 << 30), huge, Orientation::Normal),
            Err(RawError::NoPreview)
        ));
        // A normal candidate is returned without the trailing padding.
        let j = fake_jpeg(64, 48, 100);
        let src = crate::source::SliceSource::new(&j);
        let p = first_valid(&src, vec![(0, j.len() as u64)], Orientation::Normal).unwrap();
        assert_eq!(p.jpeg.len(), j.len() - 100);
        assert_eq!(p.size, Size::new(64, 48));
    }

    #[test]
    fn jpeg_size_and_trim() {
        let j = fake_jpeg(1616, 1080, 7);
        let (trimmed, size) = validate_jpeg(&j).unwrap();
        assert_eq!(size, Size::new(1616, 1080));
        assert_eq!(trimmed.len(), j.len() - 7);
        assert!(trimmed.ends_with(&[0xFF, 0xD9]));
    }

    #[test]
    fn jpeg_rejects_garbage() {
        assert!(validate_jpeg(b"").is_none());
        assert!(validate_jpeg(&[0xFF, 0xD8, 0x00, 0x00]).is_none()); // no EOI
        assert!(validate_jpeg(&[0x00, 0xD8, 0xFF, 0xD9]).is_none()); // no SOI
        // SOS before SOF.
        assert!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0xFF, 0xD9]).is_none());
        // Non-marker byte where a marker is expected.
        assert!(jpeg_size(&[0xFF, 0xD8, 0x12, 0x34, 0x00, 0x00]).is_none());
        // Segment length < 2.
        assert!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x01, 0x00, 0x00]).is_none());
        // SOF truncated.
        assert!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08]).is_none());
        // Zero width.
        assert!(
            jpeg_size(&[
                0xFF, 0xD8, 0xFF, 0xC2, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x00, 0x01
            ])
            .is_none()
        );
        // RST/TEM markers are skipped, then runs out.
        assert!(jpeg_size(&[0xFF, 0xD8, 0xFF, 0xD0, 0xFF, 0x01, 0xFF]).is_none());
    }
}
