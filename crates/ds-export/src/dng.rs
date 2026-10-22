//! Linear DNG writer: a new photo made from a RAW (AI Upscale), streamed row by row.
//!
//! The image is 3 × `u16` per pixel, linear camera RGB before white balance (black 0,
//! white 65535 = the sensor's clip), stored uncompressed as `LinearRaw` (DNG 1.4). The
//! colour tags make it decode (`ds-raw`, through rawler's DNG reader) to exactly the
//! source's `cam_to_xyz`, as-shot and daylight white balance, so it develops like the
//! original:
//!
//! - `ColorMatrix1` (D65) = `diag(1 / wb_daylight) · cam_to_xyz⁻¹`: its rows' responses to
//!   D65 white are `1 / wb_daylight`, and normalizing them away leaves `cam_to_xyz⁻¹`
//!   (see `ds_raw::color`).
//! - `AsShotNeutral` = `1 / wb_as_shot`.
//!
//! Layout: the header, then the pixel strips as they arrive (offsets known in advance),
//! then the embedded JPEG preview, the Exif and GPS directories, IFD0 (the image) and
//! IFD1 (the preview), and finally IFD0's offset patched into the header. Written to a
//! hidden temp file in the destination folder, fsync'd, then renamed to a free name: an
//! existing file is never replaced, and an unfinished file is removed.

use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ds_core::Size;

use crate::ExportError;
use crate::exif::{Entry, ExifMeta, FieldType, ifd_bytes, ifd_size, padded};
use crate::output::{Namer, TempGuard, io_err, place};

/// Rows per strip.
const STRIP_ROWS: u32 = 16;
/// Denominator of the colour matrix and neutral rationals.
const DEN: i32 = 1_000_000;
/// EXIF `LightSource` D65.
const D65: u16 = 21;
/// `PhotometricInterpretation` `LinearRaw`.
const LINEAR_RAW: u16 = 34_892;

/// What the DNG says about its pixels.
#[derive(Debug, Clone)]
pub struct DngInfo {
    pub size: Size,
    /// The source RAW's colour (see the module docs).
    pub cam_to_xyz: [[f32; 3]; 3],
    pub wb_as_shot: [f32; 3],
    pub wb_daylight: [f32; 3],
    /// EXIF orientation code (1..=8) of the stored pixels.
    pub orientation: u16,
    /// Recommended crop in stored pixels (x, y, w, h).
    pub default_crop: Option<[u32; 4]>,
    /// Camera, lens, exposure, capture time and GPS of the source. The position is
    /// written whenever `gps` is set: clear it to leave the location out.
    pub exif: ExifMeta,
    /// A JPEG to embed as the preview (the source's), as stored.
    pub preview: Option<Vec<u8>>,
}

/// A rational per value, as `value × 1 000 000 / 1 000 000` (signed or not).
fn rationals(tag: u16, signed: bool, v: &[f64]) -> Entry {
    let data = v
        .iter()
        .flat_map(|x| {
            let n = (x * f64::from(DEN)).round();
            let n = if signed {
                (n.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32).to_le_bytes()
            } else {
                (n.clamp(0.0, f64::from(u32::MAX)) as u32).to_le_bytes()
            };
            n.into_iter().chain(DEN.to_le_bytes())
        })
        .collect();
    Entry {
        tag,
        ty: if signed {
            FieldType::SRational
        } else {
            FieldType::Rational
        },
        data,
    }
}

/// `diag(1 / wb_daylight) · cam_to_xyz⁻¹` (see the module docs), `None` when singular.
fn color_matrix(cam_to_xyz: &[[f32; 3]; 3], wb_daylight: [f32; 3]) -> Option<[f64; 9]> {
    let m = cam_to_xyz.map(|r| r.map(f64::from));
    let c =
        |r0: usize, r1: usize, c0: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    let cof = [
        [c(1, 2, 1, 2), -c(1, 2, 0, 2), c(1, 2, 0, 1)],
        [-c(0, 2, 1, 2), c(0, 2, 0, 2), -c(0, 2, 0, 1)],
        [c(0, 1, 1, 2), -c(0, 1, 0, 2), c(0, 1, 0, 1)],
    ];
    let det = m[0][0] * cof[0][0] + m[0][1] * cof[0][1] + m[0][2] * cof[0][2];
    if !det.is_finite() || det.abs() < 1e-12 || wb_daylight.iter().any(|w| *w <= 0.0) {
        return None;
    }
    Some(std::array::from_fn(|k| {
        let (i, j) = (k / 3, k % 3);
        cof[j][i] / det / f64::from(wb_daylight[i])
    }))
}

/// A linear DNG being written (see the module docs). Rows go in with
/// [`LinearDngWriter::write_rows`]; [`LinearDngWriter::finish`] places the file. Dropped
/// unfinished, it removes its temp file.
#[derive(Debug)]
pub struct LinearDngWriter {
    out: BufWriter<File>,
    tmp: PathBuf,
    guard: TempGuard,
    dir: PathBuf,
    name: String,
    info: DngInfo,
    rows: u32,
}

/// Start of the pixel data (right after the header).
const DATA: u64 = 8;

impl LinearDngWriter {
    /// Start a DNG named `name` (or a free `name-N`) in `dir`.
    pub fn create(dir: &Path, name: &str, info: DngInfo) -> Result<Self, ExportError> {
        let px = u64::from(info.size.width) * u64::from(info.size.height);
        if px == 0 || DATA + px * 6 + (64 << 20) > u64::from(u32::MAX) {
            return Err(ExportError::TooLarge(info.size));
        }
        if color_matrix(&info.cam_to_xyz, info.wb_daylight).is_none()
            || info.wb_as_shot.iter().any(|w| !w.is_finite() || *w <= 0.0)
        {
            return Err(ExportError::InvalidImage(
                "the source's colour can't be stored".into(),
            ));
        }
        fs::create_dir_all(dir).map_err(io_err(dir))?;
        let tmp = ds_fsutil::temp_path(dir, "partial");
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(io_err(&tmp))?;
        let guard = TempGuard(Some(tmp.clone()));
        let mut out = BufWriter::with_capacity(4 << 20, file);
        // Little-endian TIFF; IFD0's offset is patched in by `finish`.
        out.write_all(b"II\x2a\x00\0\0\0\0").map_err(io_err(&tmp))?;
        Ok(Self {
            out,
            tmp,
            guard,
            dir: dir.to_owned(),
            name: name.to_owned(),
            info,
            rows: 0,
        })
    }

    /// Rows written so far.
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Append whole rows (interleaved RGB, `width × 3` samples each), top to bottom.
    pub fn write_rows(&mut self, data: &[u16]) -> Result<(), ExportError> {
        let row = self.info.size.width as usize * 3;
        let n = data.len() / row;
        if !data.len().is_multiple_of(row)
            || self.rows as usize + n > self.info.size.height as usize
        {
            return Err(ExportError::InvalidImage(format!(
                "{} samples after row {} of a {:?} image",
                data.len(),
                self.rows,
                self.info.size
            )));
        }
        let mut buf = Vec::with_capacity(data.len() * 2);
        buf.extend(data.iter().flat_map(|v| v.to_le_bytes()));
        self.out.write_all(&buf).map_err(io_err(&self.tmp))?;
        self.rows += n as u32;
        Ok(())
    }

    /// Write the preview and the directories, sync, and place the file under a free
    /// name. Every row must have been written.
    pub fn finish(self, namer: &Namer) -> Result<PathBuf, ExportError> {
        let Self {
            mut out,
            tmp,
            mut guard,
            dir,
            name,
            mut info,
            rows,
        } = self;
        if rows != info.size.height {
            return Err(ExportError::InvalidImage(format!(
                "{rows} of {} rows written",
                info.size.height
            )));
        }
        let preview = info.preview.take().unwrap_or_default();
        let (tail, at_ifd0) = tail(&info, preview)?;
        out.write_all(&tail).map_err(io_err(&tmp))?;
        out.flush().map_err(io_err(&tmp))?;
        let mut file = out.into_inner().map_err(|e| io_err(&tmp)(e.into_error()))?;
        file.seek(SeekFrom::Start(4)).map_err(io_err(&tmp))?;
        file.write_all(&at_ifd0.to_le_bytes())
            .map_err(io_err(&tmp))?;
        ds_fsutil::sync_file(&file).map_err(io_err(&tmp))?;
        drop(file);
        let reserved = namer.reserve(&dir, &name)?;
        let dest = place(namer, &tmp, &dir, &name, reserved)?;
        guard.0 = None;
        Ok(dest)
    }
}

/// Everything after the pixels: the preview, the Exif and GPS directories, IFD0 (the
/// image) and IFD1 (the preview). Returns it and IFD0's offset. Every offset is a
/// 32-bit TIFF offset: a file that would need more is refused, not wrapped.
fn tail(info: &DngInfo, preview: Vec<u8>) -> Result<(Vec<u8>, u32), ExportError> {
    let too_big = || ExportError::TooLarge(info.size);
    let off = |n: usize| u32::try_from(n).map_err(|_| too_big());
    let dirs = info.exif.dirs(false);
    let exif = dirs.exif;
    let gps = dirs.gps;
    let at_preview = DATA as usize + info.size.pixels() as usize * 6;
    let at_exif = padded(at_preview + preview.len());
    let at_gps = (!gps.is_empty()).then(|| at_exif + ifd_size(&exif));
    let at_ifd0 = at_gps.map_or(at_exif + ifd_size(&exif), |g| g + ifd_size(&gps));
    let mut ifd0: Vec<Entry> = dirs
        .ifd0
        .into_iter()
        .filter(|e| e.tag != 0x0112) // the stored pixels' orientation instead
        .collect();
    ifd0.extend(image_fields(
        info,
        off(at_exif)?,
        at_gps.map(off).transpose()?,
    ));
    let at_ifd1 = at_ifd0 + ifd_size(&ifd0);
    let ifd1 = vec![
        Entry::longs(0x00FE, &[1]),
        Entry::shorts(0x0103, &[6]),
        Entry::longs(0x0201, &[off(at_preview)?]),
        Entry::longs(0x0202, &[off(at_exif - at_preview)?]),
    ];
    // The end of the file is the largest offset any directory can need.
    off(at_ifd1 + ifd_size(&ifd1))?;
    let at_ifd0_32 = off(at_ifd0)?;
    let has_preview = !preview.is_empty();
    let mut out = preview;
    out.resize(at_exif - at_preview, 0);
    out.extend(ifd_bytes(exif, at_exif, 0));
    if let Some(at) = at_gps {
        out.extend(ifd_bytes(gps, at, 0));
    }
    out.extend(ifd_bytes(
        ifd0,
        at_ifd0,
        if has_preview { off(at_ifd1)? } else { 0 },
    ));
    if has_preview {
        out.extend(ifd_bytes(ifd1, at_ifd1, 0));
    }
    Ok((out, at_ifd0_32))
}

/// IFD0's image, DNG and colour tags (strips laid out from the header on), pointing at
/// the Exif (and GPS) directories.
fn image_fields(info: &DngInfo, at_exif: u32, at_gps: Option<u32>) -> Vec<Entry> {
    let Size { width, height } = info.size;
    let row_bytes = width * 6;
    let strips = height.div_ceil(STRIP_ROWS);
    let offsets: Vec<u32> = (0..strips)
        .map(|s| DATA as u32 + s * STRIP_ROWS * row_bytes)
        .collect();
    let counts: Vec<u32> = (0..strips)
        .map(|s| (height - s * STRIP_ROWS).min(STRIP_ROWS) * row_bytes)
        .collect();
    let neutral = info.wb_as_shot.map(|w| 1.0 / f64::from(w));
    let matrix = color_matrix(&info.cam_to_xyz, info.wb_daylight).expect("checked in create");
    let model = info.exif.model.clone().unwrap_or_default();
    let camera = match &info.exif.make {
        Some(make) if !model.starts_with(make.as_str()) => format!("{make} {model}"),
        _ => model,
    };
    let mut f = vec![
        Entry::longs(0x00FE, &[0]),
        Entry::longs(0x0100, &[width]),
        Entry::longs(0x0101, &[height]),
        Entry::shorts(0x0102, &[16, 16, 16]),
        Entry::shorts(0x0103, &[1]),
        Entry::shorts(0x0106, &[LINEAR_RAW]),
        Entry::longs(0x0111, &offsets),
        Entry::shorts(0x0112, &[info.orientation.clamp(1, 8)]),
        Entry::shorts(0x0115, &[3]),
        Entry::longs(0x0116, &[STRIP_ROWS]),
        Entry::longs(0x0117, &counts),
        Entry::shorts(0x011C, &[1]),
        Entry::longs(0x8769, &[at_exif]),
        Entry::bytes(0xC612, &[1, 4, 0, 0]),
        Entry::bytes(0xC613, &[1, 1, 0, 0]),
        Entry::ascii(0xC614, camera.trim()),
        Entry::shorts(0xC61A, &[0, 0, 0]),
        Entry::longs(0xC61D, &[65_535; 3]),
        rationals(0xC621, true, &matrix),
        rationals(0xC628, false, &neutral),
        Entry::shorts(0xC65A, &[D65]),
    ];
    if let Some(at) = at_gps {
        f.push(Entry::longs(0x8825, &[at]));
    }
    if let Some([x, y, w, h]) = info.default_crop {
        f.push(Entry::longs(0xC61F, &[x, y]));
        f.push(Entry::longs(0xC620, &[w, h]));
    }
    f
}

#[cfg(test)]
#[path = "dng_tests.rs"]
mod tests;
