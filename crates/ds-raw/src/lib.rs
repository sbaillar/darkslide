//! RAW decoding for Darkslide: metadata, embedded previews, and full sensor
//! decode into [`ds_core::RawImage`].
//!
//! Sony ARW (the reference camera is the ILCE-9) takes hand-written fast
//! paths: a bounded TIFF/EXIF walker for metadata and previews, and a
//! parallel positional-read unpack for uncompressed sensor data. Everything
//! else goes through [`rawler`], after its declared dimensions have been
//! bounded (see `guard`). All header values are validated against the file
//! size before use, and malformed input yields a [`RawError`]; the rawler
//! paths additionally rely on `catch_unwind` (so the workspace must keep
//! `panic = "unwind"`).
//!
//! Open a file once with [`open`] (or [`RawFile::from_bytes`] when the bytes
//! are already in memory, e.g. while hashing on import) and ask it for
//! metadata, the preview and the decode from the same parse. The free
//! functions ([`read_metadata`], [`embedded_preview`], [`decode`], …) are
//! one-shot shorthands.
//!
//! See [`color`] for the exact `cam_to_xyz` / white-balance convention.

pub mod color;
mod container;
mod cr3;
mod decode;
mod dng;
mod error;
mod fallback;
mod guard;
mod meta;
mod preview;
mod raf;
mod sony;
mod source;
#[cfg(test)]
mod synth;
mod tiff;

use std::io::Read;
use std::path::Path;

use ds_core::{Orientation, RawImage};
use rawler::rawsource::RawSource;

pub use dng::{DngMeta, write_linear_dng};
pub use error::RawError;
pub use meta::{CaptureTime, GpsPosition, ImageMeta, NormPoint, Rational, filename_sequence};
pub use preview::EmbeddedPreview;

use container::Container;
use guard::Format;
use source::{FileSource, SliceSource, Source};

/// Most strips/tiles [`RawFile::image_data_ranges`] reads (large tiled DNGs
/// stay well below this).
const MAX_DATA_BLOCKS: u64 = 1 << 16;

/// Which decoder produced a [`RawImage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodePath {
    /// Built-in parallel unpack of Sony uncompressed ARW.
    SonyUncompressed,
    /// rawler (every other format / compression).
    Rawler,
}

/// How a decode was done; see [`RawFile::decode_with_info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeInfo {
    pub path: DecodePath,
    /// EXIF `LightSource` code of the camera `ColorMatrix` behind
    /// `cam_to_xyz`: 21 = D65 (the normal case). Any other value means the
    /// camera DB had no D65 matrix and one calibrated under that illuminant
    /// was normalized to D65 white instead (the dcraw-style approximation,
    /// see [`color`]); colors are then less accurate away from that illuminant.
    pub matrix_illuminant: u16,
}

/// The camera colour a white balance is read through, without the pixels: as-shot
/// multipliers (G = 1), daylight multipliers and `cam_to_xyz` (see [`RawFile::color_info`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorInfo {
    pub wb_as_shot: [f32; 3],
    pub wb_daylight: [f32; 3],
    pub cam_to_xyz: [[f32; 3]; 3],
}

enum Backing<'a> {
    File(FileSource),
    Bytes(SliceSource<'a>),
}

/// A RAW file opened and parsed once. Borrowed bytes (`'a`) or an owned
/// file handle (`'static`).
pub struct RawFile<'a> {
    backing: Backing<'a>,
    format: Format,
    /// Parsed IFD graph for classic TIFF containers, and for ORF/RW2 when
    /// their TIFF structure parses (used for previews and the decode guard).
    container: Option<Container>,
}

impl std::fmt::Debug for RawFile<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawFile")
            .field("format", &self.format)
            .field("len", &self.src().end())
            .finish_non_exhaustive()
    }
}

/// Largest file read whole for rawler.
const MAX_RAWLER_BYTES: u64 = 2 << 30;

/// Read the whole source once with bounded positional reads and hand rawler
/// the bytes. rawler's own `RawSource::new(path)` memory-maps the file and
/// re-opens it by path: a card pulled or a share dropped mid-decode would
/// then raise SIGBUS (uncatchable), and a file swapped after our guard ran
/// would be decoded unchecked.
pub(crate) fn rawler_source_from(src: &dyn Source) -> Result<RawSource, RawError> {
    let len = src.end();
    if len > MAX_RAWLER_BYTES {
        return Err(RawError::Malformed(format!(
            "{len}-byte file is too large to decode"
        )));
    }
    Ok(RawSource::new_from_shared_vec(std::sync::Arc::new(
        src.read(0, len)?.into_owned(),
    )))
}

/// How much of each end of a big file a metadata read hands rawler. Header
/// parsers of the non-TIFF containers look at the start (CR3 `moov`, RAF,
/// ARRI, MRW) or the end (X3F, CRW directories).
const METADATA_WINDOW: u64 = 8 << 20;

/// Like [`rawler_source_from`] but only the first and last `window` bytes
/// are read; the middle stays zero-filled (lazily allocated, never touched).
/// Files that fit in two windows are read whole. Only for header parsing:
/// rawler sees zeros, not file contents, in the middle.
pub(crate) fn rawler_source_window(src: &dyn Source, window: u64) -> Result<RawSource, RawError> {
    let len = src.end();
    if len <= window.saturating_mul(2) {
        return rawler_source_from(src);
    }
    if len > MAX_RAWLER_BYTES {
        return Err(RawError::Malformed(format!(
            "{len}-byte file is too large to decode"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    let w = window as usize;
    src.read_into(0, &mut buf[..w])?;
    src.read_into(len - window, &mut buf[len as usize - w..])?;
    Ok(RawSource::new_from_shared_vec(std::sync::Arc::new(buf)))
}

/// [`rawler_source_from`] for the file at `path`.
pub(crate) fn rawler_source_at(path: &Path) -> Result<RawSource, RawError> {
    rawler_source_from(&FileSource::open(path)?)
}

/// Open and parse the container of the RAW at `path` (a few small reads).
pub fn open(path: &Path) -> Result<RawFile<'static>, RawError> {
    RawFile::new(Backing::File(FileSource::open(path)?))
}

impl<'a> RawFile<'a> {
    /// Parse a RAW already in memory. Nothing is copied except by the rawler
    /// fallback (non-Sony-uncompressed decodes), which needs its own buffer.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self, RawError> {
        Self::new(Backing::Bytes(SliceSource::new(bytes)))
    }

    fn new(backing: Backing<'a>) -> Result<Self, RawError> {
        let src: &dyn Source = match &backing {
            Backing::File(src) => src,
            Backing::Bytes(s) => s,
        };
        let head = src
            .read(0, src.end().min(16))
            .map(|b| b.to_vec())
            .unwrap_or_default();
        let format = Format::sniff(&head);
        let container = match format {
            Format::NotRaw => {
                return Err(RawError::NotRaw("unrecognized file signature".into()));
            }
            Format::Tiff => Some(Container::parse(src)?),
            Format::TiffVariant => Container::parse_with(src, true).ok(),
            _ => None,
        };
        Ok(Self {
            backing,
            format,
            container,
        })
    }

    fn src(&self) -> &dyn Source {
        match &self.backing {
            Backing::File(src) => src,
            Backing::Bytes(s) => s,
        }
    }

    fn rawler_source(&self) -> Result<RawSource, RawError> {
        Ok(match &self.backing {
            Backing::File(src) => rawler_source_from(src)?,
            Backing::Bytes(s) => RawSource::new_from_slice(s.data),
        })
    }

    /// Header-only rawler read: both ends of the file first; the whole file
    /// only when rawler could not parse what the windows hold.
    fn rawler_metadata(&self) -> Result<ImageMeta, RawError> {
        let src = self.src();
        let windowed = rawler_source_window(src, METADATA_WINDOW)?;
        match fallback::read_metadata(&windowed) {
            Err(_) if src.end() > METADATA_WINDOW * 2 => {
                fallback::read_metadata(&rawler_source_from(src)?)
            }
            r => r,
        }
    }

    /// Capture metadata: make/model/lens/ISO/shutter/aperture/focal length,
    /// capture time, orientation, dimensions, and for Sony the shutter
    /// count, AF point and shot number.
    ///
    /// Classic TIFF RAWs are read with the built-in IFD walker (no pixel data
    /// touched); other containers (CR3, RAF, ORF, RW2, …) go through rawler's
    /// header parsers.
    pub fn metadata(&self) -> Result<ImageMeta, RawError> {
        if let (Some(c), Format::Tiff) = (&self.container, self.format) {
            return Ok(meta::from_container(self.src(), c));
        }
        self.check_header()?;
        self.rawler_metadata()
    }

    /// Bound what rawler's header parsers load before they see the file (the
    /// same guards [`RawFile::decode`] runs); formats whose headers we do not
    /// bound are refused.
    pub(crate) fn check_header(&self) -> Result<(), RawError> {
        let src = self.src();
        match (&self.container, self.format) {
            (Some(c), Format::Tiff | Format::TiffVariant) => guard::check_tiff(src, c),
            (None, Format::TiffVariant) => {
                guard::check_tiff(src, &Container::parse_with(src, true)?)
            }
            (_, Format::Mrw) => guard::check_mrw(src),
            (_, Format::Unbounded("CR3")) => cr3::validate(src),
            (_, Format::Unbounded("RAF")) => raf::check_header(src),
            (_, fmt) => Err(RawError::Unsupported {
                make: String::new(),
                model: String::new(),
                reason: format!(
                    "{} is not read: its header is not bounds-checked before rawler parses it",
                    fmt.name()
                ),
            }),
        }
    }

    /// Byte ranges `(offset, len)` of the full-resolution sensor data (the raw
    /// IFD's strips or tiles), sorted and merged. `None` when the container
    /// is not TIFF-based or its sensor data can't be located exactly.
    ///
    /// Metadata writers (exiftool, geotaggers, "write metadata to file")
    /// rewrite tags and maker notes but leave these bytes alone, so a hash
    /// over them identifies the photo across metadata-only edits.
    pub fn image_data_ranges(&self) -> Result<Option<Vec<(u64, u64)>>, RawError> {
        let (Some(c), Format::Tiff) = (&self.container, self.format) else {
            return Ok(None);
        };
        let src = self.src();
        let Some(raw) = c.raw_ifd(src)? else {
            return Ok(None);
        };
        let strips = raw.get(tiff::tag::STRIP_OFFSETS);
        let (offs_tag, lens_tag, declared) = match strips {
            Some(e) => (
                tiff::tag::STRIP_OFFSETS,
                tiff::tag::STRIP_BYTE_COUNTS,
                u64::from(e.count),
            ),
            None => (
                tiff::tag::TILE_OFFSETS,
                tiff::tag::TILE_BYTE_COUNTS,
                raw.get(tiff::tag::TILE_OFFSETS)
                    .map_or(0, |e| u64::from(e.count)),
            ),
        };
        let (Some(offs), Some(lens)) = (
            raw.uints_n(src, offs_tag, MAX_DATA_BLOCKS)?,
            raw.uints_n(src, lens_tag, MAX_DATA_BLOCKS)?,
        ) else {
            return Ok(None);
        };
        if offs.is_empty() || offs.len() != lens.len() || offs.len() as u64 != declared {
            return Ok(None);
        }
        let end = src.end();
        let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(offs.len());
        for (&o, &l) in offs.iter().zip(&lens) {
            let (o, l) = (u64::from(o), u64::from(l));
            if l == 0 || o.checked_add(l).is_none_or(|e| e > end) {
                return Ok(None);
            }
            ranges.push((o, l));
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for (o, l) in ranges {
            match merged.last_mut() {
                Some((mo, ml)) if o <= *mo + *ml => *ml = (*ml).max(o + l - *mo),
                _ => merged.push((o, l)),
            }
        }
        Ok(Some(merged))
    }

    /// The largest embedded JPEG preview, as stored in the file (not
    /// decoded), plus its pixel size and the orientation to display it with.
    /// TIFF-based RAWs (including ORF's maker-note preview and RW2's
    /// `JpgFromRaw`), CR3 and RAF.
    pub fn preview(&self) -> Result<EmbeddedPreview, RawError> {
        match (&self.container, self.format) {
            (Some(c), _) => preview::from_container(self.src(), c),
            (None, Format::Unbounded("CR3")) => cr3::preview(self.src()),
            (None, Format::Unbounded("RAF")) => raf::preview(self.src()),
            (None, fmt) => Err(RawError::Unsupported {
                make: String::new(),
                model: String::new(),
                reason: format!("no embedded preview reader for {}", fmt.name()),
            }),
        }
    }

    /// The white-balance multipliers and colour matrix [`RawFile::decode`] would
    /// return. Sony uncompressed files read them from the tags only (no pixel data,
    /// a few small reads); other formats fall back to a full decode.
    pub fn color_info(&self) -> Result<ColorInfo, RawError> {
        if let (Some(c), Format::Tiff) = (&self.container, self.format)
            && let Some(plan) = decode::sony_plan(self.src(), c)?
        {
            return Ok(ColorInfo {
                wb_as_shot: plan.wb,
                wb_daylight: plan.wb_daylight,
                cam_to_xyz: plan.cam_to_xyz,
            });
        }
        let img = self.decode()?;
        Ok(ColorInfo {
            wb_as_shot: img.wb_as_shot,
            wb_daylight: img.wb_daylight,
            cam_to_xyz: img.cam_to_xyz,
        })
    }

    /// Decode the full sensor into a [`RawImage`]: active area only (the
    /// camera's recommended crop, e.g. 6000×4000 of the A9's 6048×4024
    /// frame), CFA pattern shifted to match, per-channel black level, white
    /// level, as-shot WB (G = 1), and `cam_to_xyz` per the [`color`]
    /// convention.
    pub fn decode(&self) -> Result<RawImage, RawError> {
        self.decode_with_info().map(|(img, _)| img)
    }

    /// [`RawFile::decode`] plus which decoder ran and which color matrix it used.
    pub fn decode_with_info(&self) -> Result<(RawImage, DecodeInfo), RawError> {
        let src = self.src();
        let (orientation, white) = match (&self.container, self.format) {
            (Some(c), Format::Tiff) => {
                if let Some(plan) = decode::sony_plan(src, c)? {
                    let img = decode::run_plan(src, &plan)?;
                    let info = DecodeInfo {
                        path: DecodePath::SonyUncompressed,
                        matrix_illuminant: plan.illuminant,
                    };
                    return Ok((img, info));
                }
                guard::check_tiff(src, c)?;
                (
                    ifd0_orientation(src, c),
                    decode::sony_white_override(src, c),
                )
            }
            (Some(c), Format::TiffVariant) => {
                guard::check_tiff(src, c)?;
                (ifd0_orientation(src, c), None)
            }
            (None, Format::TiffVariant) => {
                // Didn't parse at open: this reports why (or the file changed since).
                let c = Container::parse_with(src, true)?;
                guard::check_tiff(src, &c)?;
                (ifd0_orientation(src, &c), None)
            }
            (None, Format::Mrw) => {
                guard::check_mrw(src)?;
                (None, None)
            }
            (None, Format::Unbounded("CR3")) => {
                cr3::validate(src)?;
                let rs = self.rawler_source()?;
                cr3::check_decodable(&rs)?;
                return rawler_decode(&rs, None, None);
            }
            (None, Format::Unbounded("RAF")) => {
                let rs = self.rawler_source()?;
                raf::check_decodable(&rs)?;
                return rawler_decode(&rs, None, None);
            }
            (_, fmt) => {
                let name = match fmt {
                    Format::Unbounded(n) => n,
                    _ => "this format",
                };
                return Err(RawError::Unsupported {
                    make: String::new(),
                    model: String::new(),
                    reason: format!(
                        "{name} decoding is disabled: its declared dimensions are not \
                         bounds-checked before rawler allocates"
                    ),
                });
            }
        };
        rawler_decode(&self.rawler_source()?, orientation, white)
    }
}

/// [`fallback::decode`] with its [`DecodeInfo`].
fn rawler_decode(
    rs: &RawSource,
    orientation: Option<Orientation>,
    white: Option<f32>,
) -> Result<(RawImage, DecodeInfo), RawError> {
    let (img, ill) = fallback::decode(rs, orientation, white)?;
    let info = DecodeInfo {
        path: DecodePath::Rawler,
        matrix_illuminant: ill,
    };
    Ok((img, info))
}

fn ifd0_orientation(src: &dyn Source, c: &Container) -> Option<Orientation> {
    c.ifd0()
        .uint(src, tiff::tag::ORIENTATION)
        .ok()
        .flatten()
        .map(|v| Orientation::from_exif(v as u16))
}

/// Capture metadata of the RAW at `path`; see [`RawFile::metadata`].
pub fn read_metadata(path: &Path) -> Result<ImageMeta, RawError> {
    open(path)?.metadata()
}

/// Largest embedded JPEG preview of the RAW at `path`; see [`RawFile::preview`].
pub fn embedded_preview(path: &Path) -> Result<EmbeddedPreview, RawError> {
    open(path)?.preview()
}

/// Sensor-data byte ranges of the RAW at `path`; see [`RawFile::image_data_ranges`].
/// `Ok(None)` for files that aren't TIFF-based RAWs (hash the whole file instead).
pub fn image_data_ranges(path: &Path) -> Result<Option<Vec<(u64, u64)>>, RawError> {
    match open(path) {
        Ok(f) => f.image_data_ranges(),
        Err(RawError::NotRaw(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// White-balance multipliers and colour matrix of the RAW at `path`; see
/// [`RawFile::color_info`].
pub fn color_info(path: &Path) -> Result<ColorInfo, RawError> {
    open(path)?.color_info()
}

/// Full sensor decode of the RAW at `path`; see [`RawFile::decode`].
pub fn decode(path: &Path) -> Result<RawImage, RawError> {
    open(path)?.decode()
}

/// [`read_metadata`] for a RAW already in memory.
pub fn read_metadata_bytes(bytes: &[u8]) -> Result<ImageMeta, RawError> {
    RawFile::from_bytes(bytes)?.metadata()
}

/// [`decode`] for a RAW already in memory.
pub fn decode_bytes(bytes: &[u8]) -> Result<RawImage, RawError> {
    RawFile::from_bytes(bytes)?.decode()
}

/// `true` when the file name has a RAW extension rawler knows (the file is
/// not opened).
pub fn has_raw_extension(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        rawler::decoders::supported_extensions()
            .iter()
            .any(|s| s.eq_ignore_ascii_case(e))
    })
}

/// The RAW extensions rawler knows, lowercase (what [`has_raw_extension`] accepts).
pub fn raw_extensions() -> Vec<String> {
    rawler::decoders::supported_extensions()
        .iter()
        .map(|e| e.to_ascii_lowercase())
        .collect()
}

/// `true` when the extension is a RAW type rawler knows *and* the file
/// starts with a matching container signature. Never errors.
pub fn is_supported(path: &Path) -> bool {
    if !has_raw_extension(path) {
        return false;
    }
    let mut h = [0u8; 16];
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let n = f.read(&mut h).unwrap_or(0);
    Format::sniff(&h[..n]).is_raw()
}

#[cfg(test)]
mod source_tests {
    use super::*;

    struct Huge;
    impl Source for Huge {
        fn end(&self) -> u64 {
            MAX_RAWLER_BYTES + 1
        }
        fn read(&self, _: u64, _: u64) -> Result<std::borrow::Cow<'_, [u8]>, RawError> {
            unreachable!("must refuse before reading")
        }
    }

    /// Records every byte range read.
    struct Counting(u64, std::sync::Mutex<u64>);
    impl Source for Counting {
        fn end(&self) -> u64 {
            self.0
        }
        fn read(&self, off: u64, len: u64) -> Result<std::borrow::Cow<'_, [u8]>, RawError> {
            *self.1.lock().unwrap() += len;
            source::check_range(off, len, 0, self.0)?;
            Ok(std::borrow::Cow::Owned(vec![1; len as usize]))
        }
    }

    #[test]
    fn metadata_window_reads_only_both_ends_of_a_big_file() {
        let big = Counting(64 << 20, std::sync::Mutex::default());
        rawler_source_window(&big, 1 << 20).unwrap();
        assert_eq!(*big.1.lock().unwrap(), 2 << 20);
        // Small files are read whole.
        let small = Counting(1 << 20, std::sync::Mutex::default());
        rawler_source_window(&small, 1 << 20).unwrap();
        assert_eq!(*small.1.lock().unwrap(), 1 << 20);
    }

    #[test]
    fn metadata_of_a_huge_non_tiff_file_does_not_read_it_whole() {
        // A RAF-signature file far bigger than the windows: the header read
        // fails (it is not a real RAF) and falls back to the whole file, but
        // the answer is an error, not a crash or a hang.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.raf");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(METADATA_WINDOW * 2 + 4096).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&f, b"FUJIFILMCCD-RAW ", 0).unwrap();
        assert!(read_metadata(&p).is_err());
    }

    #[test]
    fn rawler_reads_the_file_it_opened_not_whatever_is_at_the_path_now() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.raf");
        std::fs::write(&p, [b"FUJIFILMCCD-RAW ".as_slice(), &[0; 4096]].concat()).unwrap();
        let raw = open(&p).unwrap();
        // Swap in an empty file: re-opening by path would now fail with Io.
        let other = dir.path().join("other");
        std::fs::write(&other, b"").unwrap();
        std::fs::rename(&other, &p).unwrap();
        assert!(!matches!(raw.metadata(), Err(RawError::Io(_))));
    }

    #[test]
    fn rawler_source_is_read_through_the_bounded_source() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        std::fs::write(&p, b"hello").unwrap();
        // Bytes come from a positional read, not a map of the path.
        assert!(rawler_source_at(&p).is_ok());
        assert!(matches!(
            rawler_source_at(&dir.path().join("gone")),
            Err(RawError::Io(_))
        ));
        assert!(matches!(
            rawler_source_from(&Huge),
            Err(RawError::Malformed(_))
        ));
    }
}
