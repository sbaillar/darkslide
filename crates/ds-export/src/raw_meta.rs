//! Adapter: `ds_raw::ImageMeta` (what the camera recorded) → [`ExifMeta`]
//! (what an export carries).

use chrono::NaiveDate;

use crate::exif::ExifMeta;

/// EXIF passthrough fields from a RAW's metadata.
///
/// `subsec_original` (EXIF `SubSecTimeOriginal`):
/// * the recorded fractional second, when the camera wrote one, as up to nine
///   digits without trailing zeros (`0.042 s` → `"042"`);
/// * otherwise, for Sony bodies (whole-second ARW timestamps), the frame's
///   `sequence_image_number` within a continuous-drive burst, as
///   `"{n-1:03}"`. This is an *ordering key*, not a true fraction: frames
///   shot within one second export with distinct, increasing
///   `DateTimeOriginal`+`SubSecTimeOriginal`, so photo apps (Amazon Photos,
///   Lightroom, Photos) keep burst order instead of sorting ties by name.
///   Single shots (`n = 1`) get `"000"`.
pub fn exif_from_raw(m: &ds_raw::ImageMeta) -> ExifMeta {
    let non_empty = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_owned())
    };
    let capture_time = m.capture_time.and_then(|t| {
        NaiveDate::from_ymd_opt(i32::from(t.year), u32::from(t.month), u32::from(t.day))?
            .and_hms_opt(
                u32::from(t.hour),
                u32::from(t.minute),
                u32::from(t.second.min(59)), // EXIF allows a leap second; chrono does not
            )
    });
    let subsec_original = capture_time.and(
        m.capture_time
            .filter(|t| t.nanos > 0)
            .map(|t| {
                let digits = format!("{:09}", t.nanos.min(999_999_999));
                digits.trim_end_matches('0').to_owned()
            })
            .or_else(|| {
                m.sequence_image_number
                    .map(|n| format!("{:03}", n.saturating_sub(1).min(999)))
            }),
    );
    ExifMeta {
        make: non_empty(&m.make),
        model: non_empty(&m.model),
        lens_make: None,
        lens_model: m.lens.as_deref().and_then(non_empty),
        capture_time,
        subsec_original,
        offset_original: capture_time.and(m.capture_time.and_then(|t| t.utc_offset_minutes)),
        exposure_time: m
            .shutter
            .filter(|r| r.den > 0 && r.num > 0)
            .map(|r| (r.num, r.den)),
        f_number: m.aperture.filter(|a| a.is_finite() && *a > 0.0),
        iso: m.iso.filter(|i| *i > 0),
        focal_length_mm: m.focal_length.filter(|f| f.is_finite() && *f > 0.0),
        gps: m.gps.map(|g| crate::exif::Gps {
            latitude: g.latitude,
            longitude: g.longitude,
            altitude_m: g.altitude_m,
        }),
        ..ExifMeta::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_raw::{CaptureTime, ImageMeta, Rational};

    fn when(nanos: u32, second: u8) -> CaptureTime {
        CaptureTime {
            year: 2026,
            month: 8,
            day: 6,
            hour: 14,
            minute: 3,
            second,
            nanos,
            utc_offset_minutes: Some(-600),
        }
    }

    fn a9() -> ImageMeta {
        ImageMeta {
            make: "SONY".into(),
            model: "ILCE-9".into(),
            lens: Some("FE 24-70mm F2.8 GM".into()),
            iso: Some(204_800),
            shutter: Some(Rational { num: 1, den: 8000 }),
            aperture: Some(2.8),
            focal_length: Some(70.0),
            capture_time: Some(when(0, 9)),
            sequence_image_number: Some(7),
            ..ImageMeta::default()
        }
    }

    #[test]
    fn maps_camera_exposure_and_time() {
        let e = exif_from_raw(&a9());
        assert_eq!(e.make.as_deref(), Some("SONY"));
        assert_eq!(e.model.as_deref(), Some("ILCE-9"));
        assert_eq!(e.lens_model.as_deref(), Some("FE 24-70mm F2.8 GM"));
        assert_eq!(e.iso, Some(204_800));
        assert_eq!(e.exposure_time, Some((1, 8000)));
        assert_eq!(e.f_number, Some(2.8));
        assert_eq!(e.focal_length_mm, Some(70.0));
        assert_eq!(
            e.capture_time,
            NaiveDate::from_ymd_opt(2026, 8, 6)
                .unwrap()
                .and_hms_opt(14, 3, 9)
        );
        assert!(e.gps.is_none() && e.artist.is_none());
        assert_eq!(
            e.offset_original,
            Some(-600),
            "the camera's UTC offset is kept"
        );
        let with_gps = exif_from_raw(&ImageMeta {
            gps: Some(ds_raw::GpsPosition {
                latitude: -33.5,
                longitude: 151.25,
                altitude_m: None,
            }),
            ..a9()
        });
        let g = with_gps.gps.expect("gps kept for export");
        assert_eq!(
            (g.latitude, g.longitude, g.altitude_m),
            (-33.5, 151.25, None)
        );
    }

    #[test]
    fn subsec_from_recorded_fraction_else_burst_frame() {
        // Whole-second timestamp: burst frame 7 → "006".
        assert_eq!(exif_from_raw(&a9()).subsec_original.as_deref(), Some("006"));
        let single = ImageMeta {
            sequence_image_number: Some(1),
            ..a9()
        };
        assert_eq!(
            exif_from_raw(&single).subsec_original.as_deref(),
            Some("000")
        );
        // A recorded fraction wins, trailing zeros trimmed.
        let frac = ImageMeta {
            capture_time: Some(when(42_000_000, 9)),
            ..a9()
        };
        assert_eq!(exif_from_raw(&frac).subsec_original.as_deref(), Some("042"));
        let fine = ImageMeta {
            capture_time: Some(when(123_456_789, 9)),
            ..a9()
        };
        assert_eq!(
            exif_from_raw(&fine).subsec_original.as_deref(),
            Some("123456789")
        );
        // Huge sequence numbers clamp to three digits.
        let long = ImageMeta {
            sequence_image_number: Some(5000),
            ..a9()
        };
        assert_eq!(exif_from_raw(&long).subsec_original.as_deref(), Some("999"));
        // Nothing known → none; no capture time → no sub-second either.
        let none = ImageMeta {
            sequence_image_number: None,
            ..a9()
        };
        assert_eq!(exif_from_raw(&none).subsec_original, None);
        let no_time = ImageMeta {
            capture_time: None,
            ..a9()
        };
        assert_eq!(exif_from_raw(&no_time).subsec_original, None);
    }

    #[test]
    fn blanks_invalid_values_and_leap_second() {
        let m = ImageMeta {
            make: "  ".into(),
            model: String::new(),
            lens: Some(" ".into()),
            iso: Some(0),
            shutter: Some(Rational { num: 1, den: 0 }),
            aperture: Some(f32::NAN),
            focal_length: Some(0.0),
            capture_time: Some(when(0, 60)),
            ..ImageMeta::default()
        };
        let e = exif_from_raw(&m);
        assert_eq!((e.make, e.model, e.lens_model), (None, None, None));
        assert_eq!(
            (e.iso, e.exposure_time, e.f_number, e.focal_length_mm),
            (None, None, None, None)
        );
        assert_eq!(e.capture_time.unwrap().format("%S").to_string(), "59");
        let bad_date = ImageMeta {
            capture_time: Some(CaptureTime {
                month: 13,
                ..when(0, 0)
            }),
            ..ImageMeta::default()
        };
        assert_eq!(exif_from_raw(&bad_date).capture_time, None);
    }

    #[test]
    fn real_a9_sample_when_present() {
        let Some(arw) = ds_testdata::sample_arw() else {
            return;
        };
        let m = ds_raw::open(&arw).unwrap().metadata().unwrap();
        let e = exif_from_raw(&m);
        assert_eq!(e.make.as_deref(), Some("SONY"));
        assert_eq!(e.model.as_deref(), Some("ILCE-9"));
        assert!(e.capture_time.is_some() && e.iso.is_some() && e.exposure_time.is_some());
        if m.sequence_image_number.is_some() || m.capture_time.is_some_and(|t| t.nanos > 0) {
            assert!(e.subsec_original.is_some());
        }
    }
}
