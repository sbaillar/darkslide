//! Synthetic Sony-ARW-like files, laid out like a real ILCE-9 ARW, used to
//! exercise every path (fast decode, rawler fallback, metadata, preview,
//! error handling) without the git-ignored sample files.
#![allow(
    clippy::float_cmp,
    clippy::too_many_lines,
    clippy::struct_excessive_bools
)] // exact expected values; one linear file layout

use std::path::{Path, PathBuf};

use ds_core::{CfaPattern, Orientation, Size};

use crate::preview::tests::fake_jpeg;
use crate::sony;
use crate::tiff::build::{Val, header, ifd};
use crate::tiff::{Endian, tag};
use crate::{DecodePath, RawError, RawFile, decode, embedded_preview, read_metadata};
use rawler::rawsource::RawSource;

const LE: Endian = Endian::Little;
const IFD0_AT: u32 = 8;
const RAW_AT: u32 = 0x200;
const EXIF_AT: u32 = 0x400;
const MN_AT: u32 = 0x600;
const GPS_AT: u32 = 0x900;
const SR2P_AT: u32 = 0xA00;
const SR2_AT: u32 = 0xB00;
const JPEG_AT: u32 = 0xC00;
const SR2_KEY: u32 = 0x1234_5678;
/// Optional IFD1 holding the preview as a single JPEG strip.
const IFD1_AT: u32 = 0x180;

#[derive(Debug, Clone)]
pub struct Synth {
    pub make: &'static str,
    pub model: &'static str,
    pub w: u32,
    pub h: u32,
    /// `DefaultCropOrigin`/`DefaultCropSize`.
    pub crop: Option<[u32; 4]>,
    /// `CFAPattern` tag (0 = R, 1 = G, 2 = B).
    pub cfa: [u8; 4],
    /// Black/WB/white directly in the raw IFD (ARW ≥ 2.3.1).
    pub plain_levels: bool,
    /// Encrypted SR2 block with black/WB/white (all ARW; rawler needs it).
    pub sr2: bool,
    pub compression: u16,
    pub orientation: u16,
    pub black: [u16; 4],
    /// DNG `WhiteLevel` (0xC61D) in the raw IFD (with `plain_levels`).
    pub white: u16,
    /// Sony 0x787F white in the SR2 block (15360 on a real A9).
    pub sr2_white: u16,
    pub wb_rggb: [u16; 4],
    pub jpeg: (u16, u16),
    /// Write `PhotometricInterpretation` = CFA on the raw IFD.
    pub photometric: bool,
    /// Store the preview as IFD1's single compression-6 strip instead of
    /// IFD0 `JPEGInterchangeFormat` tags.
    pub preview_in_strip: bool,
    /// A GPS IFD (20°48'12.34"N 156°19'30"W, 30.5 m).
    pub gps: bool,
    /// IFD0 `Artist` and `Copyright`.
    pub credits: Option<(&'static str, &'static str)>,
}

/// Sony's `b³ mod 249` substitution (inverse of `sony::decipher`).
pub fn encipher(b: &[u8]) -> Vec<u8> {
    b.iter()
        .map(|&b| {
            if b >= 249 {
                b
            } else {
                (u32::from(b).pow(3) % 249) as u8
            }
        })
        .collect()
}

impl Default for Synth {
    fn default() -> Self {
        Self {
            make: "SONY",
            model: "ILCE-9",
            w: 20,
            h: 14,
            crop: Some([2, 2, 16, 10]),
            cfa: [0, 1, 1, 2],
            plain_levels: true,
            sr2: true,
            compression: 1,
            orientation: 8,
            black: [512, 510, 514, 516],
            white: 16380,
            sr2_white: 15360,
            wb_rggb: [2568, 1024, 1024, 1620],
            jpeg: (160, 120),
            photometric: true,
            preview_in_strip: false,
            gps: true,
            credits: None,
        }
    }
}

/// Deterministic, position-unique sample value.
pub fn px(x: u32, y: u32) -> u16 {
    (600 + (x * 7 + y * 131) % 15000) as u16
}

fn place(buf: &mut Vec<u8>, at: u32, bytes: &[u8], limit: u32) {
    assert!(
        at as usize + bytes.len() <= limit as usize,
        "synthetic layout overflow at {at:#x}"
    );
    if buf.len() < at as usize {
        buf.resize(at as usize, 0);
    }
    buf.truncate(at as usize);
    buf.extend_from_slice(bytes);
}

impl Synth {
    pub fn build(&self) -> Vec<u8> {
        let jpeg = fake_jpeg(self.jpeg.0, self.jpeg.1, 3);
        let strip_at = (JPEG_AT + jpeg.len() as u32 + 15) & !15;
        let strip_len = self.w * self.h * 2;

        let mut ifd0 = vec![
            (tag::MAKE, Val::Ascii(self.make)),
            (tag::MODEL, Val::Ascii(self.model)),
            (tag::ORIENTATION, Val::Short(vec![self.orientation])),
            (tag::SUB_IFDS, Val::Long(vec![RAW_AT])),
            (tag::EXIF_IFD, Val::Long(vec![EXIF_AT])),
        ];
        if self.gps {
            ifd0.push((tag::GPS_IFD, Val::Long(vec![GPS_AT])));
        }
        if let Some((artist, copyright)) = self.credits {
            ifd0.push((tag::ARTIST, Val::Ascii(artist)));
            ifd0.push((tag::COPYRIGHT, Val::Ascii(copyright)));
        }
        if !self.preview_in_strip {
            ifd0.push((tag::COMPRESSION, Val::Short(vec![6])));
            ifd0.push((tag::JPEG_OFFSET, Val::Long(vec![JPEG_AT])));
            ifd0.push((tag::JPEG_LENGTH, Val::Long(vec![jpeg.len() as u32])));
        }
        if self.sr2 {
            ifd0.push((
                tag::DNG_PRIVATE_DATA,
                Val::Byte(SR2P_AT.to_le_bytes().to_vec()),
            ));
        }

        let mut raw = vec![
            (0x00FE, Val::Long(vec![0])),
            (tag::IMAGE_WIDTH, Val::Short(vec![self.w as u16])),
            (tag::IMAGE_LENGTH, Val::Short(vec![self.h as u16])),
            (0x0102, Val::Short(vec![12])),
            (tag::COMPRESSION, Val::Short(vec![self.compression])),
            (tag::STRIP_OFFSETS, Val::Long(vec![strip_at])),
            (tag::STRIP_BYTE_COUNTS, Val::Long(vec![strip_len])),
            (0x828D, Val::Short(vec![2, 2])),
            (tag::CFA_PATTERN, Val::Byte(self.cfa.to_vec())),
        ];
        if self.photometric {
            raw.push((tag::PHOTOMETRIC, Val::Short(vec![32803])));
        }
        if let Some([x, y, w, h]) = self.crop {
            raw.push((tag::DEFAULT_CROP_ORIGIN, Val::Long(vec![x, y])));
            raw.push((tag::DEFAULT_CROP_SIZE, Val::Long(vec![w, h])));
        }
        let levels = |v: &mut Vec<(u16, Val)>| {
            v.push((0x7310, Val::Short(self.black.to_vec())));
            v.push((0x7313, Val::Short(self.wb_rggb.to_vec())));
        };
        if self.plain_levels {
            levels(&mut raw);
            raw.push((0xC61D, Val::Short(vec![self.white])));
        }

        let mut n9050 = vec![0u8; 0x120];
        n9050[0x3A..0x3E].copy_from_slice(&3418u32.to_le_bytes());
        let n9050 = encipher(&n9050);
        // Focal-plane AF points: 2 used, area 640x428, at (338,214) and (302,234).
        let mut af = vec![1u8, 2];
        for v in [640u16, 428, 338, 214, 302, 234] {
            af.extend_from_slice(&v.to_le_bytes());
        }
        af.resize(66, 0);
        // Tag9400c: ShotNumberSincePowerUp 15583 at 0x0A, SequenceImageNumber 6 (0-based) at 0x12.
        let mut n9400 = vec![0u8; 0x20];
        n9400[0x0A..0x0E].copy_from_slice(&15583u32.to_le_bytes());
        n9400[0x12..0x16].copy_from_slice(&6u32.to_le_bytes());
        let mut n9400 = encipher(&n9400);
        n9400[0] = 0x26;
        let mn = ifd(
            LE,
            MN_AT,
            &[
                (0x9050, Val::Undefined(n9050)),
                (0x202A, Val::Undefined(af)),
                (0x2027, Val::Short(vec![6000, 4000, 3000, 2000])),
                (0x9400, Val::Undefined(n9400)),
                (0xB020, Val::Ascii("Standard")),
            ],
            0,
        );

        let exif = vec![
            (tag::EXPOSURE_TIME, Val::Rational(vec![(1, 5000)])),
            (tag::F_NUMBER, Val::Rational(vec![(28, 10)])),
            (tag::ISO, Val::Short(vec![4000])),
            (tag::DATE_TIME_ORIGINAL, Val::Ascii("2026:08:08 17:25:40")),
            (tag::OFFSET_TIME_ORIGINAL, Val::Ascii("-04:00")),
            (tag::SUB_SEC_TIME_ORIGINAL, Val::Ascii("25")),
            (tag::FOCAL_LENGTH, Val::Rational(vec![(520, 10)])),
            (tag::FOCAL_LENGTH_35MM, Val::Short(vec![52])),
            (tag::LENS_MODEL, Val::Ascii("FE 24-70mm F2.8 GM")),
            (tag::BODY_SERIAL, Val::Ascii("1234567")),
            (
                tag::MAKER_NOTE,
                Val::Raw {
                    typ: 7,
                    count: mn.len() as u32,
                    bytes: MN_AT.to_le_bytes().to_vec(),
                },
            ),
        ];

        let mut sr2_entries = Vec::new();
        levels(&mut sr2_entries);
        sr2_entries.push((0x787F, Val::Short(vec![self.sr2_white])));
        let mut sr2_plain = ifd(LE, SR2_AT, &sr2_entries, 0);
        sr2_plain.resize(sr2_plain.len().next_multiple_of(4), 0);
        let sr2_enc = sony::decrypt(&sr2_plain, SR2_KEY);
        let sr2p = ifd(
            LE,
            SR2P_AT,
            &[
                (0x7200, Val::Long(vec![SR2_AT])),
                (0x7201, Val::Long(vec![sr2_enc.len() as u32])),
                (0x7221, Val::Undefined(SR2_KEY.to_le_bytes().to_vec())),
            ],
            0,
        );

        let mut f = header(LE);
        let ifd0_next = if self.preview_in_strip { IFD1_AT } else { 0 };
        place(
            &mut f,
            IFD0_AT,
            &ifd(LE, IFD0_AT, &ifd0, ifd0_next),
            IFD1_AT,
        );
        if self.preview_in_strip {
            let ifd1 = [
                (tag::COMPRESSION, Val::Short(vec![6])),
                (tag::IMAGE_WIDTH, Val::Short(vec![self.jpeg.0])),
                (tag::IMAGE_LENGTH, Val::Short(vec![self.jpeg.1])),
                (tag::STRIP_OFFSETS, Val::Long(vec![JPEG_AT])),
                (tag::STRIP_BYTE_COUNTS, Val::Long(vec![jpeg.len() as u32])),
            ];
            place(&mut f, IFD1_AT, &ifd(LE, IFD1_AT, &ifd1, 0), RAW_AT);
        }
        place(&mut f, RAW_AT, &ifd(LE, RAW_AT, &raw, 0), EXIF_AT);
        place(&mut f, EXIF_AT, &ifd(LE, EXIF_AT, &exif, 0), MN_AT);
        if self.gps {
            place(&mut f, MN_AT, &mn, GPS_AT);
            let gps = [
                (tag::GPS_LATITUDE_REF, Val::Ascii("N")),
                (
                    tag::GPS_LATITUDE,
                    Val::Rational(vec![(20, 1), (48, 1), (1234, 100)]),
                ),
                (tag::GPS_LONGITUDE_REF, Val::Ascii("W")),
                (
                    tag::GPS_LONGITUDE,
                    Val::Rational(vec![(156, 1), (19, 1), (30, 1)]),
                ),
                (tag::GPS_ALTITUDE_REF, Val::Byte(vec![0])),
                (tag::GPS_ALTITUDE, Val::Rational(vec![(305, 10)])),
            ];
            place(&mut f, GPS_AT, &ifd(LE, GPS_AT, &gps, 0), SR2P_AT);
        } else {
            place(&mut f, MN_AT, &mn, SR2P_AT);
        }
        place(&mut f, SR2P_AT, &sr2p, SR2_AT);
        place(&mut f, SR2_AT, &sr2_enc, JPEG_AT);
        place(&mut f, JPEG_AT, &jpeg, strip_at);
        let mut strip = Vec::with_capacity(strip_len as usize);
        for y in 0..self.h {
            for x in 0..self.w {
                strip.extend_from_slice(&px(x, y).to_le_bytes());
            }
        }
        place(&mut f, strip_at, &strip, u32::MAX);
        f
    }
}

/// Write bytes to a fresh temp file; the dir guard keeps it alive.
pub fn write_tmp(bytes: &[u8], name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    std::fs::write(&p, bytes).unwrap();
    (dir, p)
}

fn cfa_from(p: [u8; 4]) -> CfaPattern {
    crate::decode::cfa_from_tag(&p.map(u32::from)).unwrap()
}

fn assert_matches_source(s: &Synth, img: &ds_core::RawImage) {
    let [cx, cy, w, h] = s.crop.unwrap_or([0, 0, s.w, s.h]);
    assert_eq!(img.size, Size::new(w, h));
    let full = cfa_from(s.cfa);
    assert_eq!(img.cfa, full.shifted(cx, cy));
    for y in 0..h {
        for x in 0..w {
            assert_eq!(
                img.data[(y * w + x) as usize],
                px(x + cx, y + cy),
                "pixel {x},{y}"
            );
            // The shifted pattern must name the same sensor color as the original site.
            assert_eq!(
                img.cfa.color_at(x, y),
                full.color_at(x + cx, y + cy),
                "cfa at {x},{y}"
            );
        }
    }
}

#[test]
fn fast_path_crops_and_shifts_cfa_for_every_parity() {
    for cfa in [[0, 1, 1, 2], [2, 1, 1, 0], [1, 0, 2, 1], [1, 2, 0, 1]] {
        for crop in [
            Some([2, 2, 16, 10]),
            Some([1, 0, 18, 13]),
            Some([0, 1, 19, 12]),
            Some([3, 3, 15, 9]),
            None,
        ] {
            let s = Synth {
                crop,
                cfa,
                ..Synth::default()
            };
            let (_d, p) = write_tmp(&s.build(), "a.ARW");
            let img = decode(&p).unwrap();
            assert_matches_source(&s, &img);
            assert_eq!(img.orientation, Orientation::Rotate270);
            assert_eq!(img.white_level, 16380.0);
            assert_eq!(img.black_level, [512.0, 510.0, 516.0, 514.0]);
            assert!((img.wb_as_shot[0] - 2.507_812_5).abs() < 1e-6);
            assert_eq!(img.wb_as_shot[1], 1.0);
            assert!((img.wb_as_shot[2] - 1.582_031_3).abs() < 1e-6);
            assert_eq!(img.default_crop, None);
        }
    }
}

#[test]
fn fast_path_uses_encrypted_sr2_when_raw_ifd_lacks_levels() {
    let s = Synth {
        plain_levels: false,
        sr2_white: 15361,
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "sr2.ARW");
    let img = decode(&p).unwrap();
    assert_matches_source(&s, &img);
    assert_eq!(img.white_level, 15361.0);
    assert_eq!(img.black_level, [512.0, 510.0, 516.0, 514.0]);
}

#[test]
fn rawler_fallback_agrees_with_fast_path() {
    let s = Synth {
        plain_levels: false,
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "fb.ARW");
    let fast = decode(&p).unwrap();
    let rs = RawSource::new(&p).unwrap();
    let (slow, ill) = crate::fallback::decode(&rs, Some(Orientation::Rotate270), None).unwrap();
    assert_eq!(ill, crate::decode::D65_LIGHT_SOURCE);
    assert_eq!(slow.size, fast.size);
    assert_eq!(slow.cfa, fast.cfa);
    assert_eq!(slow.data, fast.data);
    assert_eq!(slow.white_level, fast.white_level);
    assert_eq!(slow.orientation, fast.orientation);
    for c in 0..3 {
        assert!((slow.wb_as_shot[c] - fast.wb_as_shot[c]).abs() < 1e-5);
    }
    assert_eq!(slow.cam_to_xyz, fast.cam_to_xyz);
    // rawler reads 0x7310 positionally, we read it in R,G1,G2,B color order;
    // for RGGB both give R, G (red rows), B, G2 (blue rows) identically.
    assert_eq!(slow.black_level, fast.black_level);
    assert_eq!(fast.black_level, [512.0, 510.0, 516.0, 514.0]);
    // Orientation lookup through rawler's metadata when not supplied.
    let o = crate::fallback::decode(&rs, None, None)
        .unwrap()
        .0
        .orientation;
    assert_eq!(o, Orientation::Rotate270);
    // The white override replaces rawler's 0x787F value.
    let w = crate::fallback::decode(&rs, None, Some(16383.0)).unwrap().0;
    assert_eq!(w.white_level, 16383.0);
}

#[test]
fn color_info_matches_the_decode_without_reading_pixels() {
    // Fast (tags only) path.
    let (_d, p) = write_tmp(&Synth::default().build(), "ci.ARW");
    let img = decode(&p).unwrap();
    let ci = crate::color_info(&p).unwrap();
    assert_eq!(
        (ci.wb_as_shot, ci.wb_daylight, ci.cam_to_xyz),
        (img.wb_as_shot, img.wb_daylight, img.cam_to_xyz)
    );
    // Not a fast-path file: falls back to a decode, same answer.
    let s = Synth {
        compression: 32767,
        ..Synth::default()
    };
    let (_d2, p2) = write_tmp(&s.build(), "ci2.ARW");
    if let Ok(img) = decode(&p2) {
        let ci = crate::color_info(&p2).unwrap();
        assert_eq!(ci.wb_as_shot, img.wb_as_shot);
    }
    assert!(crate::color_info(&p.with_extension("missing")).is_err());
}

#[test]
fn non_fast_compression_goes_to_rawler() {
    // ARW2 "compressed" claims; our strip is not valid ARW2 data but rawler
    // decodes any bytes, so this must not panic whatever it returns.
    let s = Synth {
        compression: 32767,
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "c.ARW");
    let _ = decode(&p);
    // Unknown compression: rawler reports a decode error.
    let s = Synth {
        compression: 99,
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "u.ARW");
    assert!(matches!(decode(&p), Err(RawError::Decode(_))));
}

#[test]
fn unknown_sony_model_is_unsupported() {
    let s = Synth {
        model: "NOPE-1",
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "n.ARW");
    match decode(&p) {
        Err(RawError::Unsupported { make, model, .. }) => {
            assert_eq!((make.as_str(), model.as_str()), ("SONY", "NOPE-1"));
        }
        other => panic!("{other:?}"),
    }
    // Metadata and preview still work for cameras we cannot decode.
    assert_eq!(read_metadata(&p).unwrap().model, "NOPE-1");
    assert!(embedded_preview(&p).is_ok());
}

#[test]
fn non_sony_tiff_goes_to_rawler() {
    let s = Synth {
        make: "ACME",
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "x.DNG");
    assert!(matches!(
        decode(&p),
        Err(RawError::Unsupported { .. } | RawError::Decode(_))
    ));
}

#[test]
fn bad_levels_are_rejected_or_deferred() {
    // Black above white.
    let s = Synth {
        white: 400,
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "b.ARW");
    assert!(matches!(decode(&p), Err(RawError::Malformed(_))));
    // Zero WB gains.
    let s = Synth {
        wb_rggb: [0, 0, 0, 0],
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "w.ARW");
    assert!(matches!(decode(&p), Err(RawError::Malformed(_))));
    // No levels anywhere → not a fast-path file → rawler, which needs SR2 → error.
    let s = Synth {
        plain_levels: false,
        sr2: false,
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "n.ARW");
    assert!(decode(&p).is_err());
    // Unknown CFA code → rawler path, which takes the pattern from its camera DB.
    let s = Synth {
        cfa: [3, 1, 1, 2],
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "cfa.ARW");
    let img = decode(&p).unwrap();
    assert_eq!(img.cfa, CfaPattern::Rggb);
    assert_eq!(img.size, Size::new(16, 10));
}

#[test]
fn metadata_from_synthetic() {
    let (_d, p) = write_tmp(&Synth::default().build(), "m.ARW");
    let m = read_metadata(&p).unwrap();
    assert_eq!(m.make, "SONY");
    assert_eq!(m.model, "ILCE-9");
    assert_eq!(m.lens.as_deref(), Some("FE 24-70mm F2.8 GM"));
    assert_eq!(m.iso, Some(4000));
    assert_eq!(m.shutter, Some(crate::Rational { num: 1, den: 5000 }));
    assert_eq!(m.shutter_display.as_deref(), Some("1/5000"));
    assert_eq!(m.aperture, Some(2.8));
    assert_eq!(m.focal_length, Some(52.0));
    assert_eq!(m.focal_length_35mm, Some(52));
    let t = m.capture_time.unwrap();
    assert_eq!(t.to_rfc3339(), "2026-08-08T17:25:40.25-04:00");
    assert_eq!(m.orientation, Orientation::Rotate270);
    assert_eq!(m.dimensions, Some(Size::new(16, 10)));
    assert_eq!(m.serial.as_deref(), Some("1234567"));
    assert_eq!(m.shutter_count, Some(3418));
}

#[test]
fn rawler_metadata_agrees() {
    let (_d, p) = write_tmp(&Synth::default().build(), "rm.ARW");
    let ours = read_metadata(&p).unwrap();
    let theirs = crate::fallback::read_metadata(&RawSource::new(&p).unwrap()).unwrap();
    assert_eq!(theirs.iso, ours.iso);
    assert_eq!(theirs.shutter, ours.shutter);
    assert_eq!(theirs.aperture, ours.aperture);
    assert_eq!(theirs.focal_length, ours.focal_length);
    assert_eq!(theirs.orientation, ours.orientation);
    assert_eq!(theirs.capture_time, ours.capture_time);
    assert_eq!(theirs.lens, ours.lens);
}

#[test]
fn preview_from_synthetic() {
    let (_d, p) = write_tmp(
        &Synth {
            jpeg: (1616, 1080),
            ..Synth::default()
        }
        .build(),
        "p.ARW",
    );
    let pv = embedded_preview(&p).unwrap();
    assert_eq!(pv.size, Size::new(1616, 1080));
    assert_eq!(pv.orientation, Orientation::Rotate270);
    assert!(pv.jpeg.starts_with(&[0xFF, 0xD8]) && pv.jpeg.ends_with(&[0xFF, 0xD9]));
}

#[test]
fn preview_missing_or_broken() {
    // JPEG pointer at non-JPEG bytes (the raw strip) → NoPreview.
    let mut f = Synth::default().build();
    let at = JPEG_AT as usize;
    f[at] = 0; // break SOI
    let (_d, p) = write_tmp(&f, "np.ARW");
    assert!(matches!(embedded_preview(&p), Err(RawError::NoPreview)));
}

fn is_supported_ok(bytes: &[u8], name: &str) -> bool {
    let (_d, p) = write_tmp(bytes, name);
    crate::is_supported(&p)
}

#[test]
fn is_supported_checks_extension_and_magic() {
    let arw = Synth::default().build();
    assert!(is_supported_ok(&arw, "a.ARW"));
    assert!(is_supported_ok(&arw, "a.arw"));
    assert!(!is_supported_ok(&arw, "a.jpg"));
    assert!(!is_supported_ok(&arw, "noext"));
    assert!(!is_supported_ok(b"\xFF\xD8\xFF\xE0 jpeg", "fake.ARW"));
    assert!(!is_supported_ok(b"", "empty.ARW"));
    assert!(!crate::is_supported(Path::new("/nonexistent/x.ARW")));
}

#[test]
fn raw_extensions_match_has_raw_extension() {
    let exts = crate::raw_extensions();
    for e in ["arw", "pef", "nef", "cr2", "dng"] {
        assert!(exts.iter().any(|x| x == e), "{e} missing from {exts:?}");
    }
    assert!(!exts.iter().any(|x| x == "jpg"));
    for e in &exts {
        assert_eq!(e, &e.to_ascii_lowercase());
        assert!(crate::has_raw_extension(Path::new(&format!(
            "a.{}",
            e.to_ascii_uppercase()
        ))));
    }
}

#[test]
fn non_raw_files_are_not_raw() {
    for (bytes, name) in [
        (b"hello, not a raw file at all".as_slice(), "t.ARW"),
        (b"\xFF\xD8\xFF\xE0\0\x10JFIF", "j.ARW"),
        (b"", "e.ARW"),
    ] {
        let (_d, p) = write_tmp(bytes, name);
        assert!(
            matches!(read_metadata(&p), Err(RawError::NotRaw(_))),
            "{name}"
        );
        assert!(matches!(embedded_preview(&p), Err(RawError::NotRaw(_))));
        assert!(matches!(decode(&p), Err(RawError::NotRaw(_))));
    }
    assert!(matches!(
        read_metadata(Path::new("/nonexistent/x.ARW")),
        Err(RawError::Io(_))
    ));
    assert!(matches!(
        embedded_preview(Path::new("/nonexistent/x.ARW")),
        Err(RawError::Io(_))
    ));
    assert!(matches!(
        decode(Path::new("/nonexistent/x.ARW")),
        Err(RawError::Io(_))
    ));
}

#[test]
fn non_tiff_raw_magic_goes_to_rawler() {
    // RAF magic with garbage: rawler must fail cleanly.
    let mut f = b"FUJIFILMCCD-RAW 0201FF383501".to_vec();
    f.resize(512, 0xAB);
    let (_d, p) = write_tmp(&f, "x.RAF");
    assert!(read_metadata(&p).is_err());
    assert!(matches!(embedded_preview(&p), Err(RawError::NoPreview)));
    // The guard needs rawler's parse of the JPEG's TIFF, and there is none.
    assert!(matches!(decode(&p), Err(RawError::Malformed(m)) if m.contains("RAF JPEG")));
    let mut f = b"\0\0\0\x18ftypcrx \0\0\0\x01".to_vec();
    f.resize(256, 0);
    // A zero-size box after ftyp: refused before rawler's box reader.
    assert!(matches!(
        crate::decode_bytes(&f),
        Err(RawError::Malformed(m)) if m.starts_with("CR3 box tree")
    ));
    assert!(matches!(
        RawFile::from_bytes(&f).unwrap().preview(),
        Err(RawError::NoPreview)
    ));
    // Formats whose dimensions we do not bound are refused outright.
    let mut f = b"FOVb".to_vec();
    f.resize(256, 0);
    assert!(matches!(
        crate::decode_bytes(&f),
        Err(RawError::Unsupported { reason, .. }) if reason.contains("X3F")
    ));
    assert!(matches!(
        RawFile::from_bytes(&f).unwrap().preview(),
        Err(RawError::Unsupported { reason, .. }) if reason.contains("X3F")
    ));
}

/// CR3 and RAF go through their own preview readers and decode guards, then
/// rawler (which fails cleanly on these header-only files).
#[test]
fn cr3_and_raf_previews_and_guards() {
    let f = crate::cr3::tests::cr3(6, (64, 48), 64 * 48);
    let r = RawFile::from_bytes(&f).unwrap();
    let p = r.preview().unwrap();
    assert_eq!(p.size, Size::new(1620, 1080));
    assert_eq!(p.orientation, Orientation::from_exif(6));
    // Past both guards (rawler then fails on the header-only file).
    assert!(!matches!(r.decode(), Err(RawError::Malformed(_))));
    let _ = r.metadata();
    let f = crate::cr3::tests::cr3(1, (65_535, 65_535), 0);
    assert!(matches!(
        crate::decode_bytes(&f),
        Err(RawError::Malformed(_))
    ));

    let size = (0x100, vec![0, 40, 0, 60]);
    let f = crate::raf::tests::raf(8, std::slice::from_ref(&size), Some((60, 40)), 4800);
    let r = RawFile::from_bytes(&f).unwrap();
    let p = r.preview().unwrap();
    assert_eq!(p.size, Size::new(1920, 1280));
    assert_eq!(p.orientation, Orientation::from_exif(8));
    assert!(!matches!(r.decode(), Err(RawError::Malformed(_))));
    let f = crate::raf::tests::raf(1, &[size, (0x131, vec![1; 36])], None, 4800);
    assert!(matches!(
        crate::decode_bytes(&f),
        Err(RawError::Unsupported { reason, .. }) if reason == crate::raf::XTRANS
    ));
}

/// Truncate at every interesting boundary and a sweep of offsets: every call
/// must return (Ok or Err) without panicking; truncating inside the pixel
/// strip must be reported as an error by the fast path.
#[test]
fn truncated_files_never_panic() {
    let full = Synth::default().build();
    let strip_start = full.len() - 20 * 14 * 2;
    let mut cuts: Vec<usize> = (0..full.len()).step_by(37).collect();
    cuts.extend([
        0,
        1,
        4,
        7,
        8,
        9,
        0x200,
        0x400,
        0x600,
        0xA00,
        0xB00,
        0xC00,
        strip_start,
        full.len() - 1,
    ]);
    let dir = tempfile::tempdir().unwrap();
    for cut in cuts {
        let p = dir.path().join(format!("t{cut}.ARW"));
        std::fs::write(&p, &full[..cut]).unwrap();
        let _ = read_metadata(&p);
        let _ = embedded_preview(&p);
        let r = decode(&p);
        if cut >= strip_start {
            assert!(r.is_err(), "decode of file cut at {cut} must fail");
        }
    }
}

/// Deterministic byte-flipping fuzz over the header region.
#[test]
fn corrupted_headers_never_panic() {
    let full = Synth::default().build();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.ARW");
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..400 {
        let mut f = full.clone();
        for _ in 0..=(next() % 8) {
            let i = (next() % 0xC10) as usize;
            f[i] = next() as u8;
        }
        std::fs::write(&p, &f).unwrap();
        let _ = read_metadata(&p);
        let _ = embedded_preview(&p);
        if let Ok(img) = decode(&p) {
            img.validate().unwrap();
        }
    }
    // Pure garbage with a TIFF header.
    for len in [8usize, 64, 4096] {
        let mut f = b"II*\0\x08\0\0\0".to_vec();
        f.extend((0..len).map(|_| next() as u8));
        std::fs::write(&p, &f).unwrap();
        let _ = read_metadata(&p);
        let _ = embedded_preview(&p);
        let _ = decode(&p);
    }
}

/// Overwrite the inline value field of `tag` in the IFD at `ifd_at` (LE),
/// optionally retyping it to a single value of type `typ`.
fn patch_field(f: &mut [u8], ifd_at: u32, tag: u16, field: [u8; 4], typ: Option<u16>) {
    let at = ifd_at as usize;
    let n = u16::from_le_bytes([f[at], f[at + 1]]) as usize;
    for i in 0..n {
        let e = at + 2 + i * 12;
        if u16::from_le_bytes([f[e], f[e + 1]]) == tag {
            if let Some(t) = typ {
                f[e + 2..e + 4].copy_from_slice(&t.to_le_bytes());
                f[e + 4..e + 8].copy_from_slice(&1u32.to_le_bytes());
            }
            f[e + 8..e + 12].copy_from_slice(&field);
            return;
        }
    }
    panic!("tag {tag:#x} not in IFD at {ifd_at:#x}");
}

#[test]
fn hostile_dimensions_never_reach_rawler() {
    // 60000 x 60000 claimed in a ~4 KB compressed-mode file: rawler would try
    // to allocate 7.2 GB (and abort on failure). Must be rejected up front.
    let mut f = Synth {
        compression: 32767,
        ..Synth::default()
    }
    .build();
    patch_field(&mut f, RAW_AT, tag::IMAGE_WIDTH, [0x60, 0xEA, 0, 0], None);
    patch_field(&mut f, RAW_AT, tag::IMAGE_LENGTH, [0x60, 0xEA, 0, 0], None);
    let (_d, p) = write_tmp(&f, "huge.ARW");
    assert!(matches!(decode(&p), Err(RawError::Malformed(m)) if m.contains("3600000000 samples")));
    // The fast path bounds the same claim against the strip instead.
    let mut f = Synth::default().build();
    patch_field(&mut f, RAW_AT, tag::IMAGE_WIDTH, [0x60, 0xEA, 0, 0], None);
    let (_d, p) = write_tmp(&f, "huge1.ARW");
    assert!(decode(&p).is_err());
}

#[test]
fn one_corrupt_tag_does_not_hide_other_metadata() {
    let mut f = Synth::default().build();
    // LensModel (out-of-line ASCII) pointed past EOF.
    patch_field(
        &mut f,
        EXIF_AT,
        tag::LENS_MODEL,
        [0xF0, 0xFF, 0xFF, 0x7F],
        None,
    );
    // Orientation retyped to RATIONAL (not an integer).
    patch_field(&mut f, IFD0_AT, tag::ORIENTATION, [8, 0, 0, 0], Some(5));
    let (_d, p) = write_tmp(&f, "lens.ARW");
    let m = read_metadata(&p).unwrap();
    assert_eq!(m.lens, None);
    assert_eq!(m.orientation, Orientation::Normal);
    assert_eq!(m.iso, Some(4000));
    assert_eq!(m.model, "ILCE-9");
    assert_eq!(m.shutter_count, Some(3418));
}

#[test]
fn sony_rawler_path_uses_dng_white_level() {
    // No PhotometricInterpretation → not the fast-path layout → rawler.
    // rawler would report the SR2 0x787F value (15360); our 0xC61D (16380) wins.
    let s = Synth {
        photometric: false,
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "rw.ARW");
    let (img, info) = crate::open(&p).unwrap().decode_with_info().unwrap();
    assert_eq!(info.path, DecodePath::Rawler);
    assert_eq!(info.matrix_illuminant, crate::decode::D65_LIGHT_SOURCE);
    assert_eq!(img.white_level, 16380.0);
    assert_matches_source(&s, &img);
    // Without 0xC61D rawler's own value is kept.
    let s = Synth {
        photometric: false,
        plain_levels: false,
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "rw2.ARW");
    assert_eq!(decode(&p).unwrap().white_level, 15360.0);
    // The fast path reports itself.
    let (_d, p) = write_tmp(&Synth::default().build(), "fp.ARW");
    let (_, info) = crate::open(&p).unwrap().decode_with_info().unwrap();
    assert_eq!(info.path, DecodePath::SonyUncompressed);
}

#[test]
fn fast_path_layout_requirements() {
    // Big-endian Sony TIFF, wrong spp, non-CFA photometric, odd bits and a
    // strip that is not exactly w*h*2 all fall back instead of misreading.
    let base = Synth::default().build();
    let src = crate::source::SliceSource::new(&base);
    let c = crate::container::Container::parse(&src).unwrap();
    assert!(crate::decode::sony_plan(&src, &c).unwrap().is_some());
    for (tag_id, field) in [
        (tag::SAMPLES_PER_PIXEL, None),
        (tag::BITS_PER_SAMPLE, Some([20, 0, 0, 0])),
        (tag::PHOTOMETRIC, Some([2, 0, 0, 0])),
        (tag::STRIP_BYTE_COUNTS, Some([0x31, 0x02, 0, 0])),
    ] {
        let mut f = base.clone();
        if let Some(field) = field {
            patch_field(&mut f, RAW_AT, tag_id, field, None);
        } else {
            // Retag the CFA repeat dim entry as SamplesPerPixel = 3.
            patch_field(&mut f, RAW_AT, 0x828D, [3, 0, 0, 0], Some(3));
            let at = RAW_AT as usize;
            let n = u16::from_le_bytes([f[at], f[at + 1]]) as usize;
            for k in 0..n {
                let e = at + 2 + k * 12;
                if u16::from_le_bytes([f[e], f[e + 1]]) == 0x828D {
                    f[e..e + 2].copy_from_slice(&tag::SAMPLES_PER_PIXEL.to_le_bytes());
                }
            }
        }
        let src = crate::source::SliceSource::new(&f);
        let c = crate::container::Container::parse(&src).unwrap();
        assert!(
            crate::decode::sony_plan(&src, &c).unwrap().is_none(),
            "tag {tag_id:#x}"
        );
    }
    // Big-endian container.
    let mut f = base.clone();
    f[..4].copy_from_slice(b"MM\0*");
    let src = crate::source::SliceSource::new(&f);
    if let Ok(c) = crate::container::Container::parse(&src) {
        assert!(matches!(
            crate::decode::sony_plan(&src, &c),
            Ok(None) | Err(_)
        ));
    }
    // Camera missing from the DB: deferred to rawler, which names it.
    let s = Synth {
        model: "NOPE-1",
        ..Synth::default()
    };
    let f = s.build();
    let src = crate::source::SliceSource::new(&f);
    let c = crate::container::Container::parse(&src).unwrap();
    assert!(crate::decode::sony_plan(&src, &c).unwrap().is_none());
}

#[test]
fn preview_from_single_jpeg_strip() {
    let s = Synth {
        preview_in_strip: true,
        jpeg: (1616, 1080),
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "strip.ARW");
    let pv = embedded_preview(&p).unwrap();
    assert_eq!(pv.size, Size::new(1616, 1080));
    // The strip-JPEG IFD (compression 6) is not mistaken for sensor data.
    assert_eq!(decode(&p).unwrap().size, Size::new(16, 10));
}

#[test]
fn sony_af_point_and_shot_numbers() {
    let (_d, p) = write_tmp(&Synth::default().build(), "af.ARW");
    let m = read_metadata(&p).unwrap();
    // Centroid (320, 224) of a 640x428 grid, stored frame → (0.5, 0.5234);
    // orientation 8 (rotate 270) maps (x, y) → (y, 1 - x).
    let af = m.af_point.unwrap();
    assert!((af.x - 224.0 / 428.0).abs() < 1e-6, "{af:?}");
    assert!((af.y - 0.5).abs() < 1e-6, "{af:?}");
    assert_eq!(m.shot_number, Some(15583));
    assert_eq!(m.sequence_image_number, Some(7));
    // Same file with normal orientation: stored coordinates pass through.
    let s = Synth {
        orientation: 1,
        ..Synth::default()
    };
    let (_d, p) = write_tmp(&s.build(), "af1.ARW");
    let af = read_metadata(&p).unwrap().af_point.unwrap();
    assert!((af.x - 0.5).abs() < 1e-6 && (af.y - 224.0 / 428.0).abs() < 1e-6);
}

#[test]
fn bytes_api_matches_path_api() {
    let f = Synth::default().build();
    let (_d, p) = write_tmp(&f, "b.ARW");
    assert_eq!(
        crate::read_metadata_bytes(&f).unwrap(),
        read_metadata(&p).unwrap()
    );
    assert_eq!(
        crate::decode_bytes(&f).unwrap().data,
        decode(&p).unwrap().data
    );
    let rf = RawFile::from_bytes(&f).unwrap();
    assert_eq!(rf.preview().unwrap(), embedded_preview(&p).unwrap());
    assert!(format!("{rf:?}").contains("Tiff"));
    // The rawler path from bytes.
    let s = Synth {
        photometric: false,
        ..Synth::default()
    };
    let f = s.build();
    let (img, info) = RawFile::from_bytes(&f).unwrap().decode_with_info().unwrap();
    assert_eq!(info.path, DecodePath::Rawler);
    assert_matches_source(&s, &img);
    assert!(matches!(
        crate::decode_bytes(b"nope"),
        Err(RawError::NotRaw(_))
    ));
}

/// A 4 KB MRW declaring a 65535x65535 sensor must be refused before rawler
/// allocates (the reviewer measured 1.47 GB resident without the guard).
#[test]
fn mrw_with_hostile_dimensions_is_refused() {
    let mut f = b"\0MRM".to_vec();
    f.extend_from_slice(&40u32.to_be_bytes());
    f.extend_from_slice(b"\0PRD");
    f.extend_from_slice(&24u32.to_be_bytes());
    f.extend_from_slice(b"21860002");
    f.extend_from_slice(&65535u16.to_be_bytes());
    f.extend_from_slice(&65535u16.to_be_bytes());
    f.resize(4096, 0);
    let (_d, p) = write_tmp(&f, "x.MRW");
    assert!(matches!(decode(&p), Err(RawError::Malformed(m)) if m.contains("MRW PRD")));
    assert!(matches!(read_metadata(&p), Err(RawError::Malformed(_))));
    // A plausible PRD passes the guard and reaches rawler (which then fails
    // on this fake file without panicking).
    let mut f = f.clone();
    f[24..28].copy_from_slice(&[0, 16, 0, 16]);
    let (_d, p) = write_tmp(&f, "y.MRW");
    assert!(!matches!(decode(&p), Err(RawError::Malformed(m)) if m.contains("MRW PRD")));
    let _ = read_metadata(&p);
}

/// ORF/RW2 signatures are TIFF-structured: their tags are bounded too.
#[test]
fn tiff_variants_are_bounded() {
    let mut f = Synth {
        compression: 7,
        ..Synth::default()
    }
    .build();
    patch_field(&mut f, RAW_AT, tag::IMAGE_WIDTH, [0x60, 0xEA, 0, 0], None);
    patch_field(&mut f, RAW_AT, tag::IMAGE_LENGTH, [0x60, 0xEA, 0, 0], None);
    f[..4].copy_from_slice(b"IIRO");
    let (_d, p) = write_tmp(&f, "x.ORF");
    assert!(matches!(decode(&p), Err(RawError::Malformed(_))));
    // A sane ORF-magic file passes the guard and fails inside rawler cleanly.
    let mut f = Synth::default().build();
    f[..4].copy_from_slice(b"IIU\0");
    let (_d, p) = write_tmp(&f, "x.RW2");
    assert!(!matches!(decode(&p), Err(RawError::Malformed(_))));
    let _ = read_metadata(&p);
    // Its JPEG is found like a TIFF's; the sensor data isn't hashed by range
    // (ORF/RW2 identities stay whole-file).
    assert!(embedded_preview(&p).is_ok());
    assert_eq!(crate::image_data_ranges(&p).unwrap(), None);
    // An ORF whose IFD0 doesn't parse still opens (rawler reads its
    // metadata); decoding reports the broken structure.
    let mut f = b"IIRO\xFF\xFF\0\0".to_vec();
    f.resize(64, 0);
    let (_d, p) = write_tmp(&f, "bad.ORF");
    assert!(matches!(decode(&p), Err(RawError::Truncated(_))));
    assert!(matches!(
        embedded_preview(&p),
        Err(RawError::Unsupported { reason, .. }) if reason.contains("ORF/RW2")
    ));
}

/// LJPEG/JPEG strips are bounded by their frame header, not just IFD tags.
#[test]
fn jpeg_frame_dimensions_are_bounded() {
    // Point the raw IFD (compression 7) at a JPEG claiming 65535x65535x4.
    let mut f = Synth {
        compression: 7,
        ..Synth::default()
    }
    .build();
    let mut j = vec![0xFF, 0xD8, 0xFF, 0xC3, 0x00, 0x14, 0x0E];
    j.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 4]);
    let strip_at = f.len() - 20 * 14 * 2;
    f[strip_at..strip_at + j.len()].copy_from_slice(&j);
    let (_d, p) = write_tmp(&f, "lj.ARW");
    assert!(matches!(decode(&p), Err(RawError::Malformed(m)) if m.contains("JPEG frame")));
}

/// Crafted IFD graphs: many IFDs, each entry claiming a 16 MB value. The
/// walker reads only what it needs and refuses to hand the graph to rawler.
#[test]
fn crafted_ifd_graph_is_cheap() {
    const LEN: usize = 16 << 20;
    let e = LE;
    let n_children = 200u32;
    let child_len = 2 + 64 * 12 + 4;
    let ifd0 = |offs: Vec<u32>, exif: u32| {
        ifd(
            e,
            8,
            &[
                (tag::MAKE, Val::Ascii("SONY")),
                (tag::SUB_IFDS, Val::Long(offs)),
                (tag::EXIF_IFD, Val::Long(vec![exif])),
            ],
            0,
        )
    };
    let first = 8 + ifd0(vec![0; n_children as usize], 0).len() as u32;
    let offs: Vec<u32> = (0..n_children).map(|k| first + k * child_len).collect();
    let mut f = header(e);
    f.extend(ifd0(offs, first));
    assert_eq!(f.len() as u32, first);
    for _ in 0..n_children {
        let entries: Vec<(u16, Val)> = (0..64u16)
            .map(|t| {
                (
                    0x0100 + t,
                    Val::Raw {
                        typ: 4,
                        count: (LEN / 4) as u32 - 4,
                        bytes: 8u32.to_le_bytes().to_vec(),
                    },
                )
            })
            .collect();
        let at = f.len() as u32;
        f.extend(ifd(e, at, &entries, 0));
    }
    f.resize(LEN, 0);
    let t = std::time::Instant::now();
    let rf = RawFile::from_bytes(&f).unwrap();
    let _ = rf.metadata();
    let _ = rf.preview();
    let r = rf.decode();
    let dt = t.elapsed();
    assert!(matches!(r, Err(RawError::Malformed(_))), "{r:?}");
    assert!(dt.as_millis() < 500, "took {dt:?}");
}

#[test]
fn image_data_ranges_cover_exactly_the_sensor_strip() {
    let s = Synth::default();
    let f = s.build();
    let (_d, p) = write_tmp(&f, "r.ARW");
    let ranges = crate::image_data_ranges(&p).unwrap().unwrap();
    let strip_len = u64::from(s.w * s.h * 2);
    assert_eq!(ranges.len(), 1);
    let (off, len) = ranges[0];
    assert_eq!(len, strip_len);
    assert_eq!(off + len, f.len() as u64, "the strip is the file's tail");
    // The bytes are the sensor samples, not metadata.
    let first = u16::from_le_bytes([f[off as usize], f[off as usize + 1]]);
    assert_eq!(first, px(0, 0));
    // A metadata edit (lens name in the Exif IFD) leaves the ranges' bytes alone.
    let mut edited = f.clone();
    let lens = b"FE 24-70mm F2.8 GM";
    let at = edited.windows(lens.len()).position(|w| w == lens).unwrap();
    edited[at] = b'X';
    let (_d2, p2) = write_tmp(&edited, "r2.ARW");
    assert_eq!(crate::image_data_ranges(&p2).unwrap().unwrap(), ranges);
    assert_eq!(
        &edited[off as usize..(off + len) as usize],
        &f[off as usize..(off + len) as usize]
    );
}

#[test]
fn image_data_ranges_merge_tiles_and_reject_bad_layouts() {
    let tiled = |offs: Vec<u32>, lens: Vec<u32>, total: usize| {
        let mut f = header(LE);
        f.extend(ifd(
            LE,
            8,
            &[
                (tag::IMAGE_WIDTH, Val::Long(vec![64])),
                (tag::IMAGE_LENGTH, Val::Long(vec![64])),
                (tag::COMPRESSION, Val::Short(vec![1])),
                (tag::TILE_OFFSETS, Val::Long(offs)),
                (tag::TILE_BYTE_COUNTS, Val::Long(lens)),
            ],
            0,
        ));
        f.resize(total, 7);
        RawFile::from_bytes(&f)
            .unwrap()
            .image_data_ranges()
            .unwrap()
    };
    // Out of order, adjacent and overlapping tiles are sorted and merged.
    assert_eq!(
        tiled(vec![600, 200, 300, 900], vec![100, 100, 50, 10], 1000),
        Some(vec![(200, 150), (600, 100), (900, 10)])
    );
    assert_eq!(
        tiled(vec![200, 250], vec![100, 10], 1000),
        Some(vec![(200, 100)])
    );
    // Past the end, empty, or mismatched counts: can't be located exactly.
    assert_eq!(tiled(vec![900], vec![200], 1000), None);
    assert_eq!(tiled(vec![300], vec![0], 1000), None);
    assert_eq!(tiled(vec![300, 400], vec![10], 1000), None);
    // No sensor IFD at all.
    let mut f = header(LE);
    f.extend(ifd(LE, 8, &[(tag::MAKE, Val::Ascii("X"))], 0));
    assert_eq!(
        RawFile::from_bytes(&f)
            .unwrap()
            .image_data_ranges()
            .unwrap(),
        None
    );
    // Not a TIFF container (and not a RAW at all): hash the whole file instead.
    let (_d, p) = write_tmp(b"just some text", "a.txt");
    assert_eq!(crate::image_data_ranges(&p).unwrap(), None);
    assert!(crate::image_data_ranges(Path::new("/nonexistent/x.ARW")).is_err());
}

#[test]
fn image_data_ranges_of_real_samples_are_the_sensor_data() {
    for p in ds_testdata::samples_with_ext("ARW").into_iter().take(2) {
        let len = std::fs::metadata(&p).unwrap().len();
        let ranges = crate::image_data_ranges(&p).unwrap().unwrap();
        let total: u64 = ranges.iter().map(|r| r.1).sum();
        assert!(total > len / 2, "{}: {total} of {len}", p.display());
        assert!(ranges.iter().all(|&(o, l)| o + l <= len));
    }
}

/// The GPS IFD becomes `ImageMeta::gps` (signed decimal degrees, altitude);
/// no GPS IFD gives `None`.
#[test]
fn gps_position_is_read_from_the_gps_ifd() {
    let (_d, p) = write_tmp(&Synth::default().build(), "gps.ARW");
    let g = read_metadata(&p).unwrap().gps.expect("gps");
    assert!(
        (g.latitude - (20.0 + 48.0 / 60.0 + 12.34 / 3600.0)).abs() < 1e-9,
        "{g:?}"
    );
    assert!(
        (g.longitude + (156.0 + 19.0 / 60.0 + 30.0 / 3600.0)).abs() < 1e-9,
        "{g:?}"
    );
    assert_eq!(g.altitude_m, Some(30.5));
    let s = Synth {
        gps: false,
        ..Synth::default()
    };
    let (_d2, p2) = write_tmp(&s.build(), "nogps.ARW");
    assert_eq!(read_metadata(&p2).unwrap().gps, None);
}

#[test]
fn artist_and_copyright_are_read_trimmed() {
    let m = crate::read_metadata_bytes(&Synth::default().build()).unwrap();
    assert_eq!((m.artist, m.copyright), (None, None));
    let s = Synth {
        credits: Some(("Alexa R  ", "  ")),
        ..Synth::default()
    };
    let m = crate::read_metadata_bytes(&s.build()).unwrap();
    assert_eq!(m.artist.as_deref(), Some("Alexa R"));
    assert_eq!(m.copyright, None, "blank is not a copyright");
}

/// Deterministic mutation fuzz over the CR3 and RAF seeds (the review's probe,
/// without its allocator cap): every call returns, Ok or Err. A CR3 or RAF that
/// makes rawler loop or abort ends the test run instead.
#[test]
fn mutated_cr3_and_raf_never_hang_or_abort() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let seeds = [
        crate::cr3::tests::cr3(6, (64, 48), 64 * 48),
        crate::raf::tests::raf(1, &[(0x100, vec![0, 40, 0, 60])], Some((60, 40)), 4800),
    ];
    for base in seeds {
        for _ in 0..3000 {
            let mut f = base.clone();
            for _ in 0..=next() % 4 {
                let at = (next() as usize) % f.len();
                match next() % 4 {
                    0 => f[at] = next() as u8,
                    1 => {
                        let end = (at + 4).min(f.len());
                        let v = [[0u8; 4], [0xFF; 4], [0, 0, 0, 1], [0, 0, 0, 8]]
                            [(next() % 4) as usize];
                        f[at..end].copy_from_slice(&v[..end - at]);
                    }
                    2 => f.truncate(at.max(16)),
                    _ => f[at] ^= 1 << (next() % 8),
                }
            }
            let Ok(r) = RawFile::from_bytes(&f) else {
                continue;
            };
            let _ = r.metadata();
            let _ = r.preview();
            let _ = r.decode();
        }
    }
}
