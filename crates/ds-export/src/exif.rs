//! Minimal EXIF passthrough: a plain metadata struct (filled from `ds-raw`
//! at integration time) → TIFF-structured EXIF (little-endian).
//!
//! The same entry model feeds the JPEG/PNG/HEIC `Exif` blob and the TIFF
//! encoder's IFD0/Exif/GPS directories. Orientation is always written as 1
//! because exported pixels are already upright.

use chrono::NaiveDateTime;
use ds_core::{IptcField, IptcMeta};

/// Metadata copied from the original.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExifMeta {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens_make: Option<String>,
    pub lens_model: Option<String>,
    pub capture_time: Option<NaiveDateTime>,
    /// Fractional seconds of the capture time as EXIF digits (e.g. `"042"`),
    /// written to `SubSecTimeOriginal`. Supplied by the integration layer from
    /// the maker notes or a burst's shot sequence when known.
    pub subsec_original: Option<String>,
    /// UTC offset of the capture time, minutes (e.g. `-600` for UTC−10),
    /// written to `OffsetTimeOriginal`/`OffsetTimeDigitized` as `"-10:00"`.
    pub offset_original: Option<i16>,
    /// Exposure time as a fraction of a second, e.g. (1, 250).
    pub exposure_time: Option<(u32, u32)>,
    pub f_number: Option<f32>,
    pub iso: Option<u32>,
    pub focal_length_mm: Option<f32>,
    pub artist: Option<String>,
    pub copyright: Option<String>,
    pub gps: Option<Gps>,
    /// The photo's keywords, written as XMP `dc:subject` (and IPTC 2:25 in JPEG).
    pub keywords: Vec<String>,
    /// The photo's title, caption, credits… (sidecar), written as XMP/IPTC; the
    /// caption also as EXIF `ImageDescription`. Its creator and copyright win
    /// over `artist`/`copyright`, which fill them when empty.
    pub iptc: IptcMeta,
}

/// Decimal-degree position; negative latitude = south, negative longitude = west.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gps {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: Option<f64>,
}

/// TIFF field types used here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Byte = 1,
    Ascii = 2,
    Short = 3,
    Long = 4,
    Rational = 5,
    Undefined = 7,
    SRational = 10,
}

impl FieldType {
    fn unit(self) -> usize {
        match self {
            Self::Byte | Self::Ascii | Self::Undefined => 1,
            Self::Short => 2,
            Self::Long => 4,
            Self::Rational | Self::SRational => 8,
        }
    }
}

/// One IFD entry with little-endian value bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub tag: u16,
    pub ty: FieldType,
    pub data: Vec<u8>,
}

impl Entry {
    pub fn count(&self) -> u32 {
        (self.data.len() / self.ty.unit()) as u32
    }

    /// A string cut to [`STRING_MAX`] bytes (on a character boundary).
    pub(crate) fn ascii(tag: u16, s: &str) -> Self {
        Self::ascii_max(tag, s, STRING_MAX)
    }

    /// A string cut to `max` bytes. EXIF ASCII is 7-bit in theory; UTF-8 is what
    /// every modern reader accepts.
    pub(crate) fn ascii_max(tag: u16, s: &str, max: usize) -> Self {
        let mut data: Vec<u8> = crate::xmp::truncate(s, max)
            .bytes()
            .filter(|&b| b != 0)
            .collect();
        data.push(0);
        Self {
            tag,
            ty: FieldType::Ascii,
            data,
        }
    }

    fn short(tag: u16, v: u16) -> Self {
        Self::shorts(tag, &[v])
    }

    pub(crate) fn shorts(tag: u16, v: &[u16]) -> Self {
        Self {
            tag,
            ty: FieldType::Short,
            data: v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        }
    }

    fn long(tag: u16, v: u32) -> Self {
        Self::longs(tag, &[v])
    }

    pub(crate) fn longs(tag: u16, v: &[u32]) -> Self {
        Self {
            tag,
            ty: FieldType::Long,
            data: v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        }
    }

    pub(crate) fn bytes(tag: u16, v: &[u8]) -> Self {
        Self {
            tag,
            ty: FieldType::Byte,
            data: v.to_vec(),
        }
    }

    fn rationals(tag: u16, vals: &[(u32, u32)]) -> Self {
        let data = vals
            .iter()
            .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()])
            .flatten()
            .collect();
        Self {
            tag,
            ty: FieldType::Rational,
            data,
        }
    }
}

/// EXIF split into the three directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExifDirs {
    /// Not EXIF: the keywords and IPTC fields travel with the metadata the
    /// recipe includes (see [`crate::xmp`]).
    pub keywords: Vec<String>,
    pub iptc: IptcMeta,
    pub ifd0: Vec<Entry>,
    pub exif: Vec<Entry>,
    pub gps: Vec<Entry>,
}

pub const TAG_EXIF_IFD: u16 = 0x8769;

/// Bytes of the caption written as EXIF `ImageDescription`.
pub const CAPTION_MAX: usize = 2000;
pub const TAG_GPS_IFD: u16 = 0x8825;

/// Bytes of any other EXIF string (camera, lens, artist, copyright…): longer ones are
/// cut, so a hostile or corrupt tag can't push the block past a JPEG segment.
pub const STRING_MAX: usize = 255;

/// Rational approximation with a fixed denominator (good for f-number, focal length).
fn ratio(v: f64, den: u32) -> (u32, u32) {
    ((v.max(0.0) * f64::from(den)).round() as u32, den)
}

/// Degrees, minutes and seconds (in ten-thousandths) of `deg`, rounded once in whole
/// units of 1/10 000 s so the seconds never reach 60.
fn dms(deg: f64) -> [(u32, u32); 3] {
    const SECOND: u64 = 10_000;
    let total = (deg.abs() * 3600.0 * SECOND as f64).round() as u64;
    let (d, rest) = (total / (3600 * SECOND), total % (3600 * SECOND));
    let (m, s) = (rest / (60 * SECOND), rest % (60 * SECOND));
    [(d as u32, 1), (m as u32, 1), (s as u32, SECOND as u32)]
}

/// `ImageDescription` (the caption, capped like IIM 2:120 so it can't overflow the
/// EXIF segment), `Artist` and `Copyright`, when set.
fn credit_entries(iptc: &IptcMeta) -> Vec<Entry> {
    [
        (0x010E, &iptc.caption, CAPTION_MAX),
        (0x013B, &iptc.creator, STRING_MAX),
        (0x8298, &iptc.copyright, STRING_MAX),
    ]
    .into_iter()
    .filter(|(_, v, _)| !v.is_empty())
    .map(|(tag, v, max)| Entry::ascii_max(tag, v, max))
    .collect()
}

impl ExifMeta {
    /// The IPTC fields as written: the creator and copyright fall back to the
    /// EXIF artist and copyright; `strip_location` ("Remove location") drops the
    /// named place too, not just the GPS position.
    pub fn written_iptc(&self, strip_location: bool) -> IptcMeta {
        let fallback = IptcMeta {
            creator: self.artist.clone().unwrap_or_default(),
            copyright: self.copyright.clone().unwrap_or_default(),
            ..IptcMeta::default()
        };
        let mut m = fallback.overlaid(&self.iptc);
        if strip_location {
            for f in [
                IptcField::Sublocation,
                IptcField::City,
                IptcField::State,
                IptcField::Country,
            ] {
                m.get_mut(f).clear();
            }
        }
        m
    }

    /// Build the directories with `DateTime` = now (export time).
    pub fn dirs(&self, strip_gps: bool) -> ExifDirs {
        self.dirs_at(strip_gps, chrono::Local::now().naive_local())
    }

    /// Build the directories (pointer tags are added by the writers).
    ///
    /// * Orientation = 1 (exported pixels are upright).
    /// * `DateTime` (0x0132) = `processed` (when this file was made, per EXIF);
    ///   `DateTimeOriginal`/`DateTimeDigitized` = capture time.
    /// * ISO: `PhotographicSensitivity` (0x8827, SHORT) saturates at 65535, so
    ///   per EXIF 2.3 we also write `SensitivityType` = 3 ("ISO speed") and
    ///   `ISOSpeed` (0x8833, LONG) with the exact value (e.g. ISO 204800).
    pub fn dirs_at(&self, strip_gps: bool, processed: NaiveDateTime) -> ExifDirs {
        const FMT: &str = "%Y:%m:%d %H:%M:%S";
        let dt = self.capture_time.map(|t| t.format(FMT).to_string());
        let processed = Some(processed.format(FMT).to_string());
        let mut ifd0 = Vec::new();
        let push_s = |v: &mut Vec<Entry>, tag, s: &Option<String>| {
            if let Some(s) = s.as_deref().filter(|s| !s.is_empty()) {
                v.push(Entry::ascii(tag, s));
            }
        };
        push_s(&mut ifd0, 0x010F, &self.make);
        push_s(&mut ifd0, 0x0110, &self.model);
        ifd0.push(Entry::short(0x0112, 1));
        ifd0.push(Entry::ascii(0x0131, "Darkslide"));
        push_s(&mut ifd0, 0x0132, &processed);
        let iptc = self.written_iptc(strip_gps);
        ifd0.extend(credit_entries(&iptc));

        let mut exif = Vec::new();
        if let Some((n, d)) = self.exposure_time.filter(|e| e.1 != 0) {
            exif.push(Entry::rationals(0x829A, &[(n, d)]));
        }
        if let Some(f) = self.f_number {
            exif.push(Entry::rationals(0x829D, &[ratio(f64::from(f), 10)]));
        }
        if let Some(iso) = self.iso {
            exif.push(Entry::short(0x8827, iso.min(65_535) as u16));
            exif.push(Entry::short(0x8830, 3));
            exif.push(Entry::long(0x8833, iso));
        }
        exif.push(Entry {
            tag: 0x9000,
            ty: FieldType::Undefined,
            data: b"0232".to_vec(),
        });
        push_s(&mut exif, 0x9003, &dt);
        push_s(&mut exif, 0x9004, &dt);
        let subsec = self
            .subsec_original
            .clone()
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
        if dt.is_some() {
            push_s(&mut exif, 0x9291, &subsec);
            // EXIF 2.31 OffsetTimeOriginal / OffsetTimeDigitized: "±HH:MM".
            let off = self
                .offset_original
                .filter(|m| m.abs() <= 14 * 60)
                .map(|m| {
                    let a = m.unsigned_abs();
                    format!(
                        "{}{:02}:{:02}",
                        if m < 0 { '-' } else { '+' },
                        a / 60,
                        a % 60
                    )
                });
            push_s(&mut exif, 0x9011, &off);
            push_s(&mut exif, 0x9012, &off);
        }
        if let Some(fl) = self.focal_length_mm {
            exif.push(Entry::rationals(0x920A, &[ratio(f64::from(fl), 10)]));
        }
        push_s(&mut exif, 0xA433, &self.lens_make);
        push_s(&mut exif, 0xA434, &self.lens_model);

        let mut gps = Vec::new();
        if let Some(g) = self.gps.filter(|g| {
            !strip_gps
                && (-90.0..=90.0).contains(&g.latitude)
                && (-180.0..=180.0).contains(&g.longitude)
        }) {
            gps.push(Entry {
                tag: 0x0000,
                ty: FieldType::Byte,
                data: vec![2, 3, 0, 0],
            });
            gps.push(Entry::ascii(
                0x0001,
                if g.latitude < 0.0 { "S" } else { "N" },
            ));
            gps.push(Entry::rationals(0x0002, &dms(g.latitude)));
            gps.push(Entry::ascii(
                0x0003,
                if g.longitude < 0.0 { "W" } else { "E" },
            ));
            gps.push(Entry::rationals(0x0004, &dms(g.longitude)));
            if let Some(alt) = g.altitude_m.filter(|a| a.is_finite()) {
                gps.push(Entry {
                    tag: 0x0005,
                    ty: FieldType::Byte,
                    data: vec![u8::from(alt < 0.0)],
                });
                gps.push(Entry::rationals(0x0006, &[ratio(alt.abs(), 100)]));
            }
        }
        ExifDirs {
            keywords: self.keywords.clone(),
            iptc,
            ifd0,
            exif,
            gps,
        }
    }
}

pub(crate) fn padded(n: usize) -> usize {
    n + (n & 1)
}

/// Bytes of an IFD with `entries`: the entries, the next pointer and the values that
/// don't fit in an entry.
pub(crate) fn ifd_size(entries: &[Entry]) -> usize {
    2 + 12 * entries.len()
        + 4
        + entries
            .iter()
            .filter(|e| e.data.len() > 4)
            .map(|e| padded(e.data.len()))
            .sum::<usize>()
}

/// Serialize an IFD that starts at offset `at` of its TIFF, pointing at the next IFD
/// at `next` (0: none). Entries are sorted by tag; offsets are little-endian.
pub(crate) fn ifd_bytes(mut entries: Vec<Entry>, at: usize, next: u32) -> Vec<u8> {
    entries.sort_by_key(|e| e.tag);
    let mut out = Vec::with_capacity(ifd_size(&entries));
    let mut overflow = at + 2 + 12 * entries.len() + 4;
    out.extend((entries.len() as u16).to_le_bytes());
    for e in &entries {
        out.extend(e.tag.to_le_bytes());
        out.extend((e.ty as u16).to_le_bytes());
        out.extend(e.count().to_le_bytes());
        if e.data.len() <= 4 {
            let mut v = e.data.clone();
            v.resize(4, 0);
            out.extend(v);
        } else {
            out.extend((overflow as u32).to_le_bytes());
            overflow += padded(e.data.len());
        }
    }
    out.extend(next.to_le_bytes());
    for e in entries.iter().filter(|e| e.data.len() > 4) {
        out.extend(&e.data);
        if e.data.len() % 2 == 1 {
            out.push(0);
        }
    }
    out
}

/// Serialize to a TIFF-structured EXIF block (`II*\0` …), without the
/// `Exif\0\0` JPEG prefix. Offsets are relative to the start of the block.
pub fn to_tiff_bytes(dirs: &ExifDirs) -> Vec<u8> {
    let mut ifd0 = dirs.ifd0.clone();
    let exif = dirs.exif.clone();
    let gps = dirs.gps.clone();
    // Pointer entries are inline LONGs, so sizes are known before offsets.
    let extra = 1 + usize::from(!gps.is_empty());
    let ifd0_size = ifd_size(&ifd0) + 12 * extra;
    let exif_off = 8 + ifd0_size;
    ifd0.push(Entry::long(TAG_EXIF_IFD, exif_off as u32));
    if !gps.is_empty() {
        ifd0.push(Entry::long(
            TAG_GPS_IFD,
            (exif_off + ifd_size(&exif)) as u32,
        ));
    }
    let mut out = b"II*\0".to_vec();
    out.extend(8u32.to_le_bytes());
    out.extend(ifd_bytes(ifd0, out.len(), 0));
    debug_assert_eq!(out.len(), exif_off);
    out.extend(ifd_bytes(exif, out.len(), 0));
    if !gps.is_empty() {
        out.extend(ifd_bytes(gps, out.len(), 0));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ::exif::{In, Reader, Tag, Value};
    use chrono::NaiveDate;

    pub(crate) fn sample() -> ExifMeta {
        ExifMeta {
            make: Some("SONY".into()),
            model: Some("ILCE-9".into()),
            lens_make: Some("Sony".into()),
            lens_model: Some("FE 24-70mm F2.8 GM".into()),
            capture_time: Some(
                NaiveDate::from_ymd_opt(2026, 8, 6)
                    .unwrap()
                    .and_hms_opt(14, 3, 9)
                    .unwrap(),
            ),
            subsec_original: Some("042".into()),
            offset_original: Some(-600),
            exposure_time: Some((1, 250)),
            f_number: Some(2.8),
            iso: Some(400),
            focal_length_mm: Some(35.0),
            artist: Some("Alexa".into()),
            copyright: None,
            gps: Some(Gps {
                latitude: 19.5,
                longitude: -155.25,
                altitude_m: Some(-3.5),
            }),
            keywords: vec!["beach".into(), "Crete & friends".into()],
            iptc: IptcMeta::default(),
        }
    }

    fn read(bytes: &[u8]) -> ::exif::Exif {
        Reader::new()
            .read_raw(bytes.to_vec())
            .expect("parseable EXIF")
    }

    fn ascii(e: &exif::Exif, t: Tag) -> String {
        match &e.get_field(t, In::PRIMARY).unwrap().value {
            Value::Ascii(v) => String::from_utf8(v[0].clone()).unwrap(),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn roundtrips_through_a_real_parser() {
        let e = read(&to_tiff_bytes(&sample().dirs(false)));
        assert_eq!(ascii(&e, Tag::Make), "SONY");
        assert_eq!(ascii(&e, Tag::Model), "ILCE-9");
        assert_eq!(ascii(&e, Tag::LensModel), "FE 24-70mm F2.8 GM");
        assert_eq!(ascii(&e, Tag::DateTimeOriginal), "2026:08:06 14:03:09");
        assert_eq!(ascii(&e, Tag::Software), "Darkslide");
        assert_eq!(ascii(&e, Tag::SubSecTimeOriginal), "042");
        assert_eq!(
            e.get_field(Tag::Orientation, In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(1)
        );
        assert_eq!(
            e.get_field(Tag::PhotographicSensitivity, In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(400)
        );
        let r = |t| match &e.get_field(t, In::PRIMARY).unwrap().value {
            Value::Rational(v) => v[0].to_f64(),
            v => panic!("{v:?}"),
        };
        assert!((r(Tag::ExposureTime) - 0.004).abs() < 1e-9);
        assert!((r(Tag::FNumber) - 2.8).abs() < 1e-6);
        assert!((r(Tag::FocalLength) - 35.0).abs() < 1e-6);
        assert!((r(Tag::GPSAltitude) - 3.5).abs() < 1e-6);
        assert_eq!(ascii(&e, Tag::GPSLatitudeRef), "N");
        assert_eq!(ascii(&e, Tag::GPSLongitudeRef), "W");
        match &e.get_field(Tag::GPSLongitude, In::PRIMARY).unwrap().value {
            Value::Rational(v) => {
                assert_eq!((v[0].to_f64(), v[1].to_f64()), (155.0, 15.0));
                assert!(v[2].to_f64().abs() < 1e-6);
            }
            v => panic!("{v:?}"),
        }
        assert_eq!(
            e.get_field(Tag::GPSAltitudeRef, In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(1)
        );
    }

    #[test]
    fn caption_creator_and_copyright_reach_exif() {
        // The settings' artist fills an empty creator…
        let dirs = sample().dirs(false);
        assert_eq!(dirs.iptc.creator, "Alexa");
        let e = read(&to_tiff_bytes(&dirs));
        assert_eq!(ascii(&e, Tag::Artist), "Alexa");
        assert!(e.get_field(Tag::Copyright, In::PRIMARY).is_none());
        assert!(e.get_field(Tag::ImageDescription, In::PRIMARY).is_none());
        // …the photo's own values win.
        let m = ExifMeta {
            copyright: Some("© camera".into()),
            iptc: IptcMeta {
                caption: "Dawn patrol".into(),
                creator: "Kai".into(),
                ..IptcMeta::default()
            },
            ..sample()
        };
        let dirs = m.dirs(false);
        let e = read(&to_tiff_bytes(&dirs));
        assert_eq!(ascii(&e, Tag::ImageDescription), "Dawn patrol");
        assert_eq!(ascii(&e, Tag::Artist), "Kai");
        assert_eq!(ascii(&e, Tag::Copyright), "© camera");
        assert_eq!(
            (dirs.iptc.creator.as_str(), dirs.iptc.copyright.as_str()),
            ("Kai", "© camera")
        );
    }

    #[test]
    fn strip_gps_removes_the_directory() {
        let dirs = sample().dirs(true);
        assert_eq!(dirs.gps.len(), 0);
        let e = read(&to_tiff_bytes(&dirs));
        assert!(e.get_field(Tag::GPSLatitude, In::PRIMARY).is_none());
        assert!(e.get_field(Tag::GPSInfoIFDPointer, In::PRIMARY).is_none());
        assert_eq!(ascii(&e, Tag::Make), "SONY");
    }

    #[test]
    fn empty_meta_still_valid_and_upright() {
        let m = ExifMeta {
            gps: Some(Gps {
                latitude: f64::NAN,
                longitude: 0.0,
                altitude_m: None,
            }),
            ..Default::default()
        };
        let dirs = m.dirs(false);
        assert_eq!(dirs.gps.len(), 0);
        let e = read(&to_tiff_bytes(&dirs));
        assert!(e.get_field(Tag::Make, In::PRIMARY).is_none());
        assert_eq!(
            e.get_field(Tag::Orientation, In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(1)
        );
    }

    #[test]
    fn edge_values() {
        let m = ExifMeta {
            iso: Some(204_800),
            exposure_time: Some((1, 0)),
            make: Some(String::new()),
            model: Some("odd\0name".into()),
            gps: Some(Gps {
                latitude: -33.8568,
                longitude: 151.2153,
                altitude_m: None,
            }),
            ..Default::default()
        };
        let e = read(&to_tiff_bytes(&m.dirs(false)));
        assert_eq!(
            e.get_field(Tag::PhotographicSensitivity, In::PRIMARY)
                .unwrap()
                .value
                .get_uint(0),
            Some(65_535)
        );
        // The exact value survives via SensitivityType=3 + ISOSpeed (LONG).
        let uint = |t| e.get_field(t, In::PRIMARY).unwrap().value.get_uint(0);
        assert_eq!(uint(Tag::SensitivityType), Some(3));
        assert_eq!(uint(Tag::ISOSpeed), Some(204_800));
        assert!(e.get_field(Tag::ExposureTime, In::PRIMARY).is_none());
        assert!(e.get_field(Tag::Make, In::PRIMARY).is_none());
        assert_eq!(ascii(&e, Tag::Model), "oddname");
        assert_eq!(ascii(&e, Tag::GPSLatitudeRef), "S");
        assert_eq!(ascii(&e, Tag::GPSLongitudeRef), "E");
        assert!(e.get_field(Tag::GPSAltitude, In::PRIMARY).is_none());
    }

    #[test]
    fn datetime_is_processing_time_and_original_is_capture_time() {
        let processed = NaiveDate::from_ymd_opt(2026, 9, 25)
            .unwrap()
            .and_hms_opt(18, 0, 1)
            .unwrap();
        let e = read(&to_tiff_bytes(&sample().dirs_at(false, processed)));
        assert_eq!(ascii(&e, Tag::DateTime), "2026:09:25 18:00:01");
        assert_eq!(ascii(&e, Tag::DateTimeOriginal), "2026:08:06 14:03:09");
        assert_eq!(ascii(&e, Tag::DateTimeDigitized), "2026:08:06 14:03:09");
        // `dirs` stamps the current time.
        let now = read(&to_tiff_bytes(&sample().dirs(false)));
        assert!(ascii(&now, Tag::DateTime).starts_with("20"));
        assert_ne!(ascii(&now, Tag::DateTime), "2026:08:06 14:03:09");
    }

    #[test]
    fn subsec_is_validated_and_needs_a_capture_time() {
        let bad = ExifMeta {
            subsec_original: Some("4x".into()),
            ..sample()
        };
        let e = read(&to_tiff_bytes(&bad.dirs(false)));
        assert!(e.get_field(Tag::SubSecTimeOriginal, In::PRIMARY).is_none());
        let no_time = ExifMeta {
            capture_time: None,
            ..sample()
        };
        let e = read(&to_tiff_bytes(&no_time.dirs(false)));
        assert!(e.get_field(Tag::SubSecTimeOriginal, In::PRIMARY).is_none());
        assert!(e.get_field(Tag::DateTimeOriginal, In::PRIMARY).is_none());
        assert!(e.get_field(Tag::OffsetTimeOriginal, In::PRIMARY).is_none());
    }

    #[test]
    fn offset_time_original_is_written_when_known() {
        let e = read(&to_tiff_bytes(&sample().dirs(false)));
        assert_eq!(ascii(&e, Tag::OffsetTimeOriginal), "-10:00");
        assert_eq!(ascii(&e, Tag::OffsetTimeDigitized), "-10:00");
        for (m, s) in [(0, "+00:00"), (330, "+05:30"), (-45, "-00:45")] {
            let e = read(&to_tiff_bytes(
                &ExifMeta {
                    offset_original: Some(m),
                    ..sample()
                }
                .dirs(false),
            ));
            assert_eq!(ascii(&e, Tag::OffsetTimeOriginal), s);
        }
        // Unknown or nonsense offsets are left out.
        for m in [None, Some(15 * 60)] {
            let e = read(&to_tiff_bytes(
                &ExifMeta {
                    offset_original: m,
                    ..sample()
                }
                .dirs(false),
            ));
            assert!(e.get_field(Tag::OffsetTimeOriginal, In::PRIMARY).is_none());
        }
    }

    #[test]
    fn seconds_never_round_up_to_sixty() {
        // 12°59'59.99996" rounded the seconds to 60.0000 and left the minute alone.
        assert_eq!(dms(12.999_999_999), [(13, 1), (0, 1), (0, 10_000)]);
        assert_eq!(dms(-45.5), [(45, 1), (30, 1), (0, 10_000)]);
        assert_eq!(dms(0.0), [(0, 1), (0, 1), (0, 10_000)]);
        let [_, m, s] = dms(33.999_999_9);
        assert!(m.0 <= 59 && s.0 < 60 * 10_000, "{m:?} {s:?}");
        assert_eq!(dms(90.0), [(90, 1), (0, 1), (0, 10_000)]);
    }

    #[test]
    fn positions_outside_the_globe_are_left_out() {
        let at = |latitude, longitude| {
            ExifMeta {
                gps: Some(Gps {
                    latitude,
                    longitude,
                    altitude_m: None,
                }),
                ..Default::default()
            }
            .dirs(false)
            .gps
            .is_empty()
        };
        assert!(at(95.0, 10.0), "latitude past the pole");
        assert!(at(-90.5, 10.0));
        assert!(at(10.0, 181.0), "longitude past the antimeridian");
        assert!(at(10.0, -200.0));
        assert!(
            !at(90.0, 180.0) && !at(-90.0, -180.0),
            "the limits are valid"
        );
    }

    #[test]
    fn long_strings_are_cut_on_a_character_boundary() {
        let e = Entry::ascii(0x0110, &"é".repeat(300));
        assert_eq!(e.data.len(), 255, "254 bytes of text and the NUL");
        assert!(std::str::from_utf8(&e.data[..254]).is_ok());
        let caption = Entry::ascii_max(0x010E, &"x".repeat(5000), CAPTION_MAX);
        assert_eq!(caption.data.len(), CAPTION_MAX + 1);
        let m = ExifMeta {
            model: Some("m".repeat(70_000)),
            ..Default::default()
        };
        assert!(to_tiff_bytes(&m.dirs(false)).len() < 4096);
    }
}
