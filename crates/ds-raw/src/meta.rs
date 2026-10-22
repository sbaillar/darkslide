//! Capture metadata (`ImageMeta`) from EXIF / maker notes.

use std::fmt::{self, Write as _};
use std::path::Path;

use ds_core::{Orientation, Size};
use serde::{Deserialize, Serialize};

use crate::RawError;
use crate::container::{Container, crop_rect};
use crate::sony;
use crate::source::Source;
use crate::tiff::{Ifd, RawRational, tag};

/// An unsigned EXIF rational, kept exact (e.g. exposure time `1/5000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

impl Rational {
    pub fn to_f64(self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            f64::from(self.num) / f64::from(self.den)
        }
    }

    fn from_raw(r: RawRational) -> Option<Self> {
        let num = u32::try_from(r.num).ok()?;
        let den = u32::try_from(r.den).ok()?;
        (den != 0).then_some(Self { num, den })
    }

    /// Exposure time the way cameras print it: `1/5000`, `1/8`, `1/3`, `0.4"`,
    /// `2"`, `30"`. Times of 0.4 s and longer are shown in seconds (one
    /// decimal, trailing `.0` dropped); shorter ones as `1/N`.
    pub fn shutter_display(self) -> String {
        let t = self.to_f64();
        if t <= 0.0 {
            return "0\"".into();
        }
        if t >= 0.4 - 1e-9 {
            let s = format!("{t:.1}");
            let s = s.strip_suffix(".0").unwrap_or(&s);
            return format!("{s}\"");
        }
        format!("1/{}", (1.0 / t).round() as u64)
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// A point in normalized image coordinates (`0..=1` on both axes), relative
/// to the **oriented** (display-upright) image, like [`ds_core::NormRect`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormPoint {
    pub x: f32,
    pub y: f32,
}

impl NormPoint {
    /// Map a point normalized to the *stored* (sensor-orientation) frame into
    /// the frame displayed after applying EXIF `orientation`.
    pub fn from_stored(x: f32, y: f32, orientation: Orientation) -> Self {
        let (x, y) = match orientation {
            Orientation::Normal => (x, y),
            Orientation::FlipH => (1.0 - x, y),
            Orientation::Rotate180 => (1.0 - x, 1.0 - y),
            Orientation::FlipV => (x, 1.0 - y),
            Orientation::Transpose => (y, x),
            // EXIF 6: rotate 90° clockwise to display.
            Orientation::Rotate90 => (1.0 - y, x),
            Orientation::Transverse => (1.0 - y, 1.0 - x),
            // EXIF 8: rotate 90° counter-clockwise to display.
            Orientation::Rotate270 => (y, 1.0 - x),
        };
        Self { x, y }
    }
}

/// Local capture time as recorded by the camera, with optional sub-second
/// precision (`SubSecTimeOriginal`) and UTC offset (`OffsetTimeOriginal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CaptureTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// Fractional second in nanoseconds (0 when the camera did not record it).
    pub nanos: u32,
    /// Offset from UTC in minutes, if recorded.
    pub utc_offset_minutes: Option<i16>,
}

/// Fixed-width run of ASCII digits (no sign, no spaces).
fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0, |n, &c| n * 10 + u32::from(c - b'0')))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl CaptureTime {
    /// Parse EXIF `YYYY:MM:DD HH:MM:SS` (fixed positions, ASCII digits only,
    /// a real calendar date) plus optional sub-second digits and a `±HH:MM`
    /// offset. Anything else after the 19 characters is ignored.
    pub(crate) fn parse(dt: &str, subsec: Option<&str>, offset: Option<&str>) -> Option<Self> {
        let b = dt.as_bytes().get(..19)?;
        if b[4] != b':' || b[7] != b':' || b[10] != b' ' || b[13] != b':' || b[16] != b':' {
            return None;
        }
        let year = digits(&b[0..4])?;
        let month = digits(&b[5..7])?;
        let day = digits(&b[8..10])?;
        let (hour, minute, second) = (
            digits(&b[11..13])?,
            digits(&b[14..16])?,
            digits(&b[17..19])?,
        );
        if year == 0
            || !(1..=12).contains(&month)
            || day == 0
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return None;
        }
        let nanos = subsec
            .map(str::trim)
            .and_then(|s| {
                let s = s.as_bytes();
                digits(&s[..s.len().min(9)]).map(|v| v * 10u32.pow(9 - s.len().min(9) as u32))
            })
            .unwrap_or(0);
        Some(Self {
            year: year as u16,
            month: month as u8,
            day: day as u8,
            hour: hour as u8,
            minute: minute as u8,
            second: second as u8,
            nanos,
            utc_offset_minutes: offset.and_then(parse_offset),
        })
    }

    /// RFC 3339 / ISO 8601 text, e.g. `2026-08-08T17:25:40.12-04:00`
    /// (no offset suffix when the offset is unknown; fraction only when non-zero).
    pub fn to_rfc3339(&self) -> String {
        let mut s = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        if self.nanos != 0 {
            let frac = format!("{:09}", self.nanos);
            s.push('.');
            s.push_str(frac.trim_end_matches('0'));
        }
        if let Some(off) = self.utc_offset_minutes {
            let sign = if off < 0 { '-' } else { '+' };
            let a = off.unsigned_abs();
            let _ = write!(s, "{sign}{:02}:{:02}", a / 60, a % 60);
        }
        s
    }
}

/// Exactly `±HH:MM`, |offset| ≤ 18:00.
fn parse_offset(s: &str) -> Option<i16> {
    let b = s.trim().as_bytes();
    if b.len() != 6 || b[3] != b':' {
        return None;
    }
    let sign = match b[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (h, m) = (digits(&b[1..3])?, digits(&b[4..6])?);
    (h <= 18 && m < 60 && h * 60 + m <= 18 * 60).then_some(sign * (h * 60 + m) as i16)
}

/// Where the photo was taken (EXIF GPS IFD), decimal degrees: negative
/// latitude = south, negative longitude = west; altitude in metres (negative
/// = below sea level).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GpsPosition {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: Option<f64>,
}

/// Capture metadata for one RAW file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ImageMeta {
    /// Camera make as written in the file (e.g. `SONY`).
    pub make: String,
    /// Camera model as written in the file (e.g. `ILCE-9`).
    pub model: String,
    /// Lens model (EXIF `LensModel`), e.g. `FE 24-70mm F2.8 GM`.
    pub lens: Option<String>,
    /// ISO sensitivity. Uses `RecommendedExposureIndex`/`ISOSpeed` when the
    /// legacy 16-bit `ISOSpeedRatings` is saturated (65535) or superseded
    /// (`SensitivityType` present), so extended ISO above 65535 is exact.
    pub iso: Option<u32>,
    /// Exposure time in seconds, exact.
    pub shutter: Option<Rational>,
    /// Exposure time for display (`1/5000`, `2"`), see [`Rational::shutter_display`].
    pub shutter_display: Option<String>,
    /// F-number, e.g. 2.8.
    pub aperture: Option<f32>,
    /// Focal length in mm.
    pub focal_length: Option<f32>,
    /// 35 mm-equivalent focal length in mm, when recorded.
    pub focal_length_35mm: Option<u32>,
    pub capture_time: Option<CaptureTime>,
    pub orientation: Orientation,
    /// Size of the image `decode` returns (sensor orientation, before `orientation`
    /// is applied). `None` when the container does not describe it cheaply.
    pub dimensions: Option<Size>,
    /// Body serial number (EXIF `BodySerialNumber`), when recorded.
    pub serial: Option<String>,
    /// Mechanical shutter actuations, when the maker note has them (Sony `Tag9050b` bodies).
    pub shutter_count: Option<u32>,
    /// Where the camera focused, in oriented normalized coordinates (Sony:
    /// centroid of the focal-plane AF points used). A subject hint for culling.
    pub af_point: Option<NormPoint>,
    /// Sony `ShotNumberSincePowerUp`: increases by one per frame (bursts
    /// included) until the camera is switched off. With `capture_time` it
    /// orders frames inside a burst, which ARW timestamps (whole seconds) cannot.
    pub shot_number: Option<u32>,
    /// 1-based index of the frame within a continuous-drive burst (1 for single shots).
    pub sequence_image_number: Option<u32>,
    /// Recorded position (GPS IFD), when the camera had one (e.g. from a paired phone).
    #[serde(default)]
    pub gps: Option<GpsPosition>,
    /// IFD0 `Artist` and `Copyright`, when set in the camera.
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub copyright: Option<String>,
}

/// Trailing digits of a file name's stem, e.g. `DSC07062.ARW` → 7062,
/// `DSC00012.ARW` → 12. Camera file numbers wrap (9999 → 0001), so use this
/// only as a tie-breaker after capture time and shot number.
pub fn filename_sequence(path: &Path) -> Option<u64> {
    let stem = path.file_stem()?.to_str()?.as_bytes();
    let start = stem.len() - stem.iter().rev().take_while(|c| c.is_ascii_digit()).count();
    let tail = &stem[start..];
    // Up to 19 digits always fit in u64; keep the least-significant ones.
    let tail = &tail[tail.len().saturating_sub(19)..];
    if tail.is_empty() {
        return None;
    }
    tail.iter().try_fold(0u64, |n, &c| {
        n.checked_mul(10)?.checked_add(u64::from(c - b'0'))
    })
}

/// A field that is unreadable (bad offset, wrong type) is treated as absent:
/// one corrupt tag must not hide the rest of the metadata.
fn opt<T>(r: Result<Option<T>, RawError>) -> Option<T> {
    r.ok().flatten()
}

/// Choose the ISO value: the 16-bit `ISOSpeedRatings` unless it is saturated
/// or the file declares a `SensitivityType` (then the 32-bit tags win).
pub(crate) fn pick_iso(
    iso: Option<u32>,
    sensitivity_type_present: bool,
    rei: Option<u32>,
    iso_speed: Option<u32>,
) -> Option<u32> {
    let iso = iso.filter(|&v| v > 0);
    let ext = rei.filter(|&v| v > 0).or(iso_speed.filter(|&v| v > 0));
    if iso == Some(65535) || sensitivity_type_present {
        ext.or(iso)
    } else {
        iso.or(ext)
    }
}

/// Extract metadata from a parsed TIFF container. Only the container itself
/// must be valid; individual unreadable fields become `None`.
pub(crate) fn from_container(src: &dyn Source, c: &Container) -> ImageMeta {
    let ifd0 = c.ifd0();
    let make = opt(ifd0.ascii(src, tag::MAKE)).unwrap_or_default();
    let model = opt(ifd0.ascii(src, tag::MODEL)).unwrap_or_default();
    let orientation = opt(ifd0.uint(src, tag::ORIENTATION))
        .map_or(Orientation::Normal, |v| Orientation::from_exif(v as u16));
    let dimensions = opt(c.raw_ifd(src)).and_then(|raw| raw_dims(src, raw));
    let mut m = ImageMeta {
        make,
        model,
        orientation,
        dimensions,
        ..ImageMeta::default()
    };
    m.gps = c.gps.as_ref().and_then(|g| gps_position(src, g));
    m.artist = opt(ifd0.ascii(src, tag::ARTIST));
    m.copyright = opt(ifd0.ascii(src, tag::COPYRIGHT));
    if let Some(exif) = &c.exif {
        fill_exif(src, exif, &mut m);
        if m.make.eq_ignore_ascii_case("SONY")
            && let Some(mn) = opt(sony::makernote(src, exif))
        {
            m.shutter_count = opt(sony::shutter_count(src, &m.model, &mn));
            m.af_point =
                sony::af_point(src, &mn).map(|(x, y)| NormPoint::from_stored(x, y, orientation));
            (m.shot_number, m.sequence_image_number) = sony::shot_numbers(src, &mn);
        }
    }
    m
}

/// Degrees/minutes/seconds rationals + `N`/`S` (`E`/`W`) reference → signed
/// decimal degrees. Missing, malformed or out-of-range values give `None`, as
/// do a void fix (`GPSStatus` `V`) and exactly 0, 0 ("null island": what
/// receivers without a fix write).
fn gps_position(src: &dyn Source, g: &Ifd) -> Option<GpsPosition> {
    if opt(g.ascii(src, tag::GPS_STATUS)).is_some_and(|s| s.trim().eq_ignore_ascii_case("V")) {
        return None;
    }
    let coord = |val: u16, reference: u16, neg: &str, max: f64| -> Option<f64> {
        let dms = opt(g.rationals(src, val))?;
        let [d, m, s] = dms.get(..3)? else {
            return None;
        };
        let (d, m, s) = (d.to_f64()?, m.to_f64()?, s.to_f64()?);
        let v = d + m / 60.0 + s / 3600.0;
        if !v.is_finite() || !(0.0..=max).contains(&v) {
            return None;
        }
        let r = opt(g.ascii(src, reference))?;
        Some(if r.eq_ignore_ascii_case(neg) { -v } else { v })
    };
    let latitude = coord(tag::GPS_LATITUDE, tag::GPS_LATITUDE_REF, "S", 90.0)?;
    let longitude = coord(tag::GPS_LONGITUDE, tag::GPS_LONGITUDE_REF, "W", 180.0)?;
    if latitude == 0.0 && longitude == 0.0 {
        return None;
    }
    let altitude_m = opt(g.rational(src, tag::GPS_ALTITUDE))
        .and_then(RawRational::to_f64)
        .filter(|a| a.is_finite())
        .map(|a| {
            if opt(g.uint(src, tag::GPS_ALTITUDE_REF)) == Some(1) {
                -a
            } else {
                a
            }
        });
    Some(GpsPosition {
        latitude,
        longitude,
        altitude_m,
    })
}

fn raw_dims(src: &dyn Source, raw: &Ifd) -> Option<Size> {
    let w = opt(raw.uint(src, tag::IMAGE_WIDTH))?;
    let h = opt(raw.uint(src, tag::IMAGE_LENGTH))?;
    let [_, _, cw, ch] = crop_rect(src, raw, w, h).ok()?;
    Some(Size::new(cw, ch))
}

fn fill_exif(src: &dyn Source, exif: &Ifd, m: &mut ImageMeta) {
    m.lens = opt(exif.ascii(src, tag::LENS_MODEL));
    m.iso = pick_iso(
        opt(exif.uint(src, tag::ISO)),
        exif.get(tag::SENSITIVITY_TYPE).is_some(),
        opt(exif.uint(src, tag::RECOMMENDED_EXPOSURE_INDEX)),
        opt(exif.uint(src, tag::ISO_SPEED)),
    );
    m.shutter = opt(exif.rational(src, tag::EXPOSURE_TIME))
        .and_then(Rational::from_raw)
        .filter(|r| r.num > 0);
    m.shutter_display = m.shutter.map(Rational::shutter_display);
    m.aperture = positive(opt(exif.rational(src, tag::F_NUMBER)));
    m.focal_length = positive(opt(exif.rational(src, tag::FOCAL_LENGTH)));
    m.focal_length_35mm = opt(exif.uint(src, tag::FOCAL_LENGTH_35MM)).filter(|&v| v > 0);
    m.serial = opt(exif.ascii(src, tag::BODY_SERIAL));
    if let Some(dt) = opt(exif.ascii(src, tag::DATE_TIME_ORIGINAL)) {
        let sub = opt(exif.ascii(src, tag::SUB_SEC_TIME_ORIGINAL));
        let off = opt(exif.ascii(src, tag::OFFSET_TIME_ORIGINAL));
        m.capture_time = CaptureTime::parse(&dt, sub.as_deref(), off.as_deref());
    }
}

fn positive(r: Option<RawRational>) -> Option<f32> {
    r.and_then(RawRational::to_f64)
        .filter(|v| *v > 0.0 && v.is_finite())
        .map(|v| v as f32)
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // tests compare exactly-representable values
mod tests {
    use super::*;

    /// South/west, below sea level, and malformed GPS IFDs.
    #[test]
    fn gps_position_signs_and_rejects_bad_values() {
        use crate::source::SliceSource;
        use crate::tiff::Endian;
        use crate::tiff::build::{Val, ifd};
        let at = 8u32;
        let parse = |entries: &[(u16, Val)]| {
            let mut f = vec![0u8; at as usize];
            f.extend(ifd(Endian::Little, at, entries, 0));
            let src = SliceSource::new(&f);
            let g = Ifd::parse(&src, u64::from(at), Endian::Little).unwrap();
            gps_position(&src, &g)
        };
        let dms = |d| Val::Rational(vec![(d, 1), (30, 1), (0, 1)]);
        let g = parse(&[
            (tag::GPS_LATITUDE_REF, Val::Ascii("S")),
            (tag::GPS_LATITUDE, dms(33)),
            (tag::GPS_LONGITUDE_REF, Val::Ascii("W")),
            (tag::GPS_LONGITUDE, dms(70)),
            (tag::GPS_ALTITUDE_REF, Val::Byte(vec![1])),
            (tag::GPS_ALTITUDE, Val::Rational(vec![(12, 1)])),
        ])
        .unwrap();
        assert_eq!(
            (g.latitude, g.longitude, g.altitude_m),
            (-33.5, -70.5, Some(-12.0))
        );
        // No altitude is fine; missing refs, short DMS or out-of-range values are not.
        let g = parse(&[
            (tag::GPS_LATITUDE_REF, Val::Ascii("N")),
            (tag::GPS_LATITUDE, dms(1)),
            (tag::GPS_LONGITUDE_REF, Val::Ascii("E")),
            (tag::GPS_LONGITUDE, dms(2)),
        ])
        .unwrap();
        assert_eq!((g.latitude, g.longitude, g.altitude_m), (1.5, 2.5, None));
        assert!(parse(&[(tag::GPS_LATITUDE, dms(1)), (tag::GPS_LONGITUDE, dms(2))]).is_none());
        assert!(
            parse(&[
                (tag::GPS_LATITUDE_REF, Val::Ascii("N")),
                (tag::GPS_LATITUDE, Val::Rational(vec![(1, 1)])),
                (tag::GPS_LONGITUDE_REF, Val::Ascii("E")),
                (tag::GPS_LONGITUDE, dms(2)),
            ])
            .is_none()
        );
        assert!(
            parse(&[
                (tag::GPS_LATITUDE_REF, Val::Ascii("N")),
                (tag::GPS_LATITUDE, dms(91)),
                (tag::GPS_LONGITUDE_REF, Val::Ascii("E")),
                (tag::GPS_LONGITUDE, dms(2)),
            ])
            .is_none()
        );
        // No fix: a void status, or exactly 0, 0; an active status is fine.
        let fix = |status: &'static str| {
            parse(&[
                (tag::GPS_LATITUDE_REF, Val::Ascii("N")),
                (tag::GPS_LATITUDE, dms(1)),
                (tag::GPS_LONGITUDE_REF, Val::Ascii("E")),
                (tag::GPS_LONGITUDE, dms(2)),
                (tag::GPS_STATUS, Val::Ascii(status)),
            ])
        };
        assert!(fix("V").is_none());
        assert!(fix("A").is_some());
        let zero = Val::Rational(vec![(0, 1), (0, 1), (0, 1)]);
        assert!(
            parse(&[
                (tag::GPS_LATITUDE_REF, Val::Ascii("N")),
                (tag::GPS_LATITUDE, zero.clone()),
                (tag::GPS_LONGITUDE_REF, Val::Ascii("E")),
                (tag::GPS_LONGITUDE, zero),
            ])
            .is_none()
        );
    }

    #[test]
    fn shutter_display_formats() {
        let r = |num, den| Rational { num, den }.shutter_display();
        assert_eq!(r(1, 5000), "1/5000");
        assert_eq!(r(10, 1250), "1/125");
        assert_eq!(r(1, 8), "1/8");
        assert_eq!(r(1, 4), "1/4");
        assert_eq!(r(1, 3), "1/3");
        assert_eq!(r(3, 10), "1/3");
        assert_eq!(r(4, 10), "0.4\"");
        assert_eq!(r(1, 2), "0.5\"");
        assert_eq!(r(2, 1), "2\"");
        assert_eq!(r(13, 10), "1.3\"");
        assert_eq!(r(30, 1), "30\"");
        assert_eq!(r(0, 1), "0\"");
        assert_eq!(Rational { num: 1, den: 0 }.to_f64(), 0.0);
        assert_eq!(Rational { num: 1, den: 400 }.to_string(), "1/400");
    }

    #[test]
    fn rational_from_raw_rejects_negative_and_zero_den() {
        assert_eq!(
            Rational::from_raw(RawRational { num: 1, den: 4 }),
            Some(Rational { num: 1, den: 4 })
        );
        assert_eq!(Rational::from_raw(RawRational { num: -1, den: 4 }), None);
        assert_eq!(Rational::from_raw(RawRational { num: 1, den: 0 }), None);
    }

    #[test]
    fn capture_time_parsing() {
        let t = CaptureTime::parse("2026:08:08 17:25:40", Some("12"), Some("-04:00")).unwrap();
        assert_eq!(
            (t.year, t.month, t.day, t.hour, t.minute, t.second),
            (2026, 8, 8, 17, 25, 40)
        );
        assert_eq!(t.nanos, 120_000_000);
        assert_eq!(t.utc_offset_minutes, Some(-240));
        assert_eq!(t.to_rfc3339(), "2026-08-08T17:25:40.12-04:00");

        let t = CaptureTime::parse("2017:06:25 16:04:05", None, Some("+05:30")).unwrap();
        assert_eq!(t.nanos, 0);
        assert_eq!(t.to_rfc3339(), "2017-06-25T16:04:05+05:30");
        let t = CaptureTime::parse("2017:06:25 16:04:05", Some(" "), Some("garbage")).unwrap();
        assert_eq!(t.to_rfc3339(), "2017-06-25T16:04:05");
        let p = |sub| {
            CaptureTime::parse("2017:06:25 16:04:05", Some(sub), None)
                .unwrap()
                .nanos
        };
        assert_eq!(p("12a"), 0);
        assert_eq!(p("+5"), 0);
        assert_eq!(p("1234567891"), 123_456_789);
        assert_eq!(p("000001"), 1_000);

        // Calendar validation.
        assert!(CaptureTime::parse("2024:02:29 00:00:00", None, None).is_some());
        assert!(CaptureTime::parse("2000:02:29 00:00:00", None, None).is_some());
        assert!(CaptureTime::parse("2023:02:29 00:00:00", None, None).is_none());
        assert!(CaptureTime::parse("1900:02:29 00:00:00", None, None).is_none());
        assert!(CaptureTime::parse("2026:02:31 00:00:00", None, None).is_none());
        assert!(CaptureTime::parse("2026:04:31 00:00:00", None, None).is_none());
        assert!(CaptureTime::parse("2026:12:31 23:59:60", None, None).is_some());
        assert!(CaptureTime::parse("0000:01:01 00:00:00", None, None).is_none());
        assert!(CaptureTime::parse("2017:13:25 16:04:05", None, None).is_none());
        assert!(CaptureTime::parse("2017:06:00 16:04:05", None, None).is_none());
        assert!(CaptureTime::parse("2017:06:25 25:04:05", None, None).is_none());
        assert!(CaptureTime::parse("2017:06:25 16:60:05", None, None).is_none());
        assert!(CaptureTime::parse("2017:06:25 16:04:61", None, None).is_none());
        // Strict field syntax: no signs, spaces or other separators.
        assert!(CaptureTime::parse("+017:06:25 16:04:05", None, None).is_none());
        assert!(CaptureTime::parse("2017:+6:25 16:04:05", None, None).is_none());
        assert!(CaptureTime::parse("2017: 6:25 16:04:05", None, None).is_none());
        assert!(CaptureTime::parse("2017-06-25 16:04:05", None, None).is_none());
        assert!(CaptureTime::parse("2017:06:25T16:04:05", None, None).is_none());
        assert!(CaptureTime::parse("2017:06:25", None, None).is_none());
        assert!(CaptureTime::parse("abcd:06:25 16:04:05", None, None).is_none());
        assert!(CaptureTime::parse("2017:06:25 16:\u{e9}:05xx", None, None).is_none());
        // Trailing text after the 19 characters is ignored.
        assert!(CaptureTime::parse("2017:06:25 16:04:05Z", None, None).is_some());
    }

    #[test]
    fn offset_parsing() {
        assert_eq!(parse_offset("+00:00"), Some(0));
        assert_eq!(parse_offset("-09:30"), Some(-570));
        assert_eq!(parse_offset(" +18:00 "), Some(1080));
        assert_eq!(parse_offset("04:00"), None);
        assert_eq!(parse_offset("+4"), None);
        assert_eq!(parse_offset("+19:00"), None);
        assert_eq!(parse_offset("+18:01"), None);
        assert_eq!(parse_offset("+01:60"), None);
        assert_eq!(parse_offset("--05:00"), None);
        assert_eq!(parse_offset("-+5:00"), None);
        assert_eq!(parse_offset("+05-00"), None);
        assert_eq!(parse_offset("+05:0a"), None);
        assert_eq!(parse_offset(""), None);
    }

    #[test]
    fn iso_selection() {
        // Normal: legacy tag.
        assert_eq!(pick_iso(Some(400), false, Some(400), None), Some(400));
        assert_eq!(pick_iso(Some(400), false, None, None), Some(400));
        // Saturated legacy tag: extended value wins.
        assert_eq!(
            pick_iso(Some(65535), false, Some(102_400), None),
            Some(102_400)
        );
        assert_eq!(
            pick_iso(Some(65535), false, None, Some(204_800)),
            Some(204_800)
        );
        assert_eq!(pick_iso(Some(65535), false, None, None), Some(65535));
        // SensitivityType present: REI preferred.
        assert_eq!(pick_iso(Some(4000), true, Some(4000), None), Some(4000));
        assert_eq!(pick_iso(Some(100), true, Some(125), None), Some(125));
        // Missing / zero legacy tag.
        assert_eq!(pick_iso(None, false, Some(800), None), Some(800));
        assert_eq!(pick_iso(Some(0), false, None, None), None);
    }

    #[test]
    fn norm_point_orientation_mapping() {
        use Orientation::*;
        let p = |o| {
            let n = NormPoint::from_stored(0.25, 0.1, o);
            (n.x, n.y)
        };
        assert_eq!(p(Normal), (0.25, 0.1));
        assert_eq!(p(FlipH), (0.75, 0.1));
        assert_eq!(p(Rotate180), (0.75, 0.9));
        assert_eq!(p(FlipV), (0.25, 0.9));
        assert_eq!(p(Transpose), (0.1, 0.25));
        assert_eq!(p(Rotate90), (0.9, 0.25));
        assert_eq!(p(Transverse), (0.9, 0.75));
        assert_eq!(p(Rotate270), (0.1, 0.75));
        // Corners: the stored top-left goes to top-right under a clockwise turn,
        // to bottom-left under a counter-clockwise one.
        let tl = |o| {
            let n = NormPoint::from_stored(0.0, 0.0, o);
            (n.x, n.y)
        };
        assert_eq!(tl(Rotate90), (1.0, 0.0));
        assert_eq!(tl(Rotate270), (0.0, 1.0));
    }

    #[test]
    fn filename_sequence_parses_trailing_digits() {
        let f = |s: &str| filename_sequence(Path::new(s));
        assert_eq!(f("/a/DSC07062.ARW"), Some(7062));
        assert_eq!(f("DSC00012.arw"), Some(12));
        assert_eq!(f("IMG_0001"), Some(1));
        assert_eq!(f("20260808_172540_123.ARW"), Some(123));
        assert_eq!(f("no-digits.ARW"), None);
        assert_eq!(f("12x.ARW"), None);
        assert_eq!(f(""), None);
        // Absurdly long runs keep the last 19 digits and never overflow.
        assert_eq!(
            f("A12345678901234567890123.ARW"),
            Some(5_678_901_234_567_890_123)
        );
        assert_eq!(
            f("A99999999999999999999.ARW"),
            Some(9_999_999_999_999_999_999)
        );
    }
}
