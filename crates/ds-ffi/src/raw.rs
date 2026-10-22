//! RAW file access for the browser: capture metadata and the embedded JPEG
//! preview (`ds_raw`), so the app can show an image the instant it's focused.
//!
//! Both calls read only a few small ranges of the file (no sensor decode):
//! see `ds_raw::RawFile::metadata` / `preview`. They run on the rayon pool via
//! [`run_job`] and honour the [`CancelToken`] before starting. The preview JPEG
//! crosses the FFI as `Vec<u8>` → `Data` (one copy of a few hundred KB);
//! decoding happens in Swift with `ImageIO`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::DsError;
use crate::jobs::{CancelToken, guard, run_job};

/// Capture time as recorded (wall clock of the camera) plus its UTC offset if known.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, uniffi::Record, serde::Serialize, serde::Deserialize,
)]
pub struct CaptureTimeData {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub nanos: u32,
    pub utc_offset_minutes: Option<i16>,
}

/// A position, decimal degrees: negative latitude = south, negative
/// longitude = west; altitude in metres (negative = below sea level).
#[derive(Debug, Clone, Copy, PartialEq, uniffi::Record, serde::Serialize, serde::Deserialize)]
pub struct GeoPosition {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: Option<f64>,
}

impl From<ds_core::GeoPoint> for GeoPosition {
    fn from(g: ds_core::GeoPoint) -> Self {
        Self {
            latitude: g.latitude,
            longitude: g.longitude,
            altitude_m: g.altitude_m,
        }
    }
}

impl GeoPosition {
    /// The validated point, or an error naming what is out of range.
    pub(crate) fn to_point(self) -> Result<ds_core::GeoPoint, DsError> {
        ds_core::GeoPoint::new(self.latitude, self.longitude, self.altitude_m).ok_or_else(|| {
            DsError::invalid(format!(
                "position {}, {} is out of range",
                self.latitude, self.longitude
            ))
        })
    }
}

/// What the browser needs about a RAW file. Dimensions are *oriented*
/// (as displayed), `orientation` is the EXIF value (1..=8).
#[derive(
    Debug, Clone, Default, PartialEq, uniffi::Record, serde::Serialize, serde::Deserialize,
)]
pub struct RawMetadata {
    pub make: String,
    pub model: String,
    pub lens: Option<String>,
    pub iso: Option<u32>,
    pub shutter_seconds: Option<f64>,
    /// e.g. `1/5000`, `2"`.
    pub shutter_display: Option<String>,
    pub aperture: Option<f32>,
    pub focal_length: Option<f32>,
    pub focal_length_35mm: Option<u32>,
    pub capture_time: Option<CaptureTimeData>,
    pub orientation: u16,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub serial: Option<String>,
    pub shutter_count: Option<u32>,
    /// Focus point, normalized in the oriented frame.
    pub af_x: Option<f32>,
    pub af_y: Option<f32>,
    /// Sony `ShotNumberSincePowerUp` (orders frames within a burst).
    pub shot_number: Option<u32>,
    pub sequence_image_number: Option<u32>,
    /// EXIF `Artist` and `Copyright` as the camera wrote them.
    #[uniffi(default = None)]
    #[serde(default)]
    pub artist: Option<String>,
    #[uniffi(default = None)]
    #[serde(default)]
    pub copyright: Option<String>,
    /// Where the camera recorded the photo was taken (EXIF GPS IFD).
    #[uniffi(default = None)]
    #[serde(default)]
    pub gps: Option<GeoPosition>,
}

/// The largest embedded JPEG, as stored (not rotated); apply `orientation`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EmbeddedPreviewData {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub orientation: u16,
}

impl From<ds_raw::ImageMeta> for RawMetadata {
    fn from(m: ds_raw::ImageMeta) -> Self {
        let dims = m.dimensions.map(|d| m.orientation.apply_to(d));
        Self {
            make: m.make,
            model: m.model,
            lens: m.lens,
            iso: m.iso,
            shutter_seconds: m.shutter.map(ds_raw::Rational::to_f64),
            shutter_display: m.shutter_display,
            aperture: m.aperture,
            focal_length: m.focal_length,
            focal_length_35mm: m.focal_length_35mm,
            capture_time: m.capture_time.map(|t| CaptureTimeData {
                year: t.year,
                month: t.month,
                day: t.day,
                hour: t.hour,
                minute: t.minute,
                second: t.second,
                nanos: t.nanos,
                utc_offset_minutes: t.utc_offset_minutes,
            }),
            orientation: m.orientation.to_exif(),
            width: dims.map(|d| d.width),
            height: dims.map(|d| d.height),
            serial: m.serial,
            shutter_count: m.shutter_count,
            af_x: m.af_point.map(|p| p.x),
            af_y: m.af_point.map(|p| p.y),
            shot_number: m.shot_number,
            sequence_image_number: m.sequence_image_number,
            artist: m.artist,
            copyright: m.copyright,
            gps: m.gps.map(|g| GeoPosition {
                latitude: g.latitude,
                longitude: g.longitude,
                altitude_m: g.altitude_m,
            }),
        }
    }
}

impl From<ds_raw::EmbeddedPreview> for EmbeddedPreviewData {
    fn from(p: ds_raw::EmbeddedPreview) -> Self {
        Self {
            jpeg: p.jpeg,
            width: p.size.width,
            height: p.size.height,
            orientation: p.orientation.to_exif(),
        }
    }
}

/// Capture metadata of the RAW at `path`.
#[uniffi::export]
pub async fn raw_metadata(path: String, token: Arc<CancelToken>) -> Result<RawMetadata, DsError> {
    run_job(token, move |_| {
        Ok(ds_raw::read_metadata(&PathBuf::from(path))?.into())
    })
    .await
}

/// Largest embedded JPEG preview of the RAW at `path`.
#[uniffi::export]
pub async fn embedded_preview(
    path: String,
    token: Arc<CancelToken>,
) -> Result<EmbeddedPreviewData, DsError> {
    run_job(token, move |_| {
        Ok(ds_raw::embedded_preview(&PathBuf::from(path))?.into())
    })
    .await
}

/// Whether `path` looks like a RAW we can read (extension + magic bytes).
/// Throws only if the check itself panicked (C3: no `try!` in Swift).
#[uniffi::export]
pub fn is_supported_raw(path: String) -> Result<bool, DsError> {
    guard(|| Ok(ds_raw::is_supported(&PathBuf::from(path))))
}

/// The RAW extensions `ds-raw` reads, lowercase: what a folder walk counts as a
/// photo (the same list a session open uses).
#[uniffi::export]
pub fn raw_extensions() -> Vec<String> {
    ds_raw::raw_extensions()
}

/// Trailing digits of the file name (`DSC07062.ARW` → 7062): a burst
/// tie-breaker after capture time and shot number (ds-ai `GroupItem.seq`).
#[uniffi::export]
pub fn filename_sequence(path: String) -> Result<Option<u64>, DsError> {
    guard(|| Ok(ds_raw::filename_sequence(&PathBuf::from(path))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;

    #[test]
    fn missing_and_non_raw_files_error_cleanly() {
        let r = block_on(raw_metadata(
            "/nonexistent/x.ARW".into(),
            CancelToken::new(),
        ));
        assert!(matches!(r, Err(DsError::Io { .. })), "{r:?}");
        let dir = std::env::temp_dir().join(format!("ds-ffi-raw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("note.ARW");
        std::fs::write(&txt, b"hello, not a raw file at all").unwrap();
        let p = txt.to_string_lossy().into_owned();
        let r = block_on(embedded_preview(p.clone(), CancelToken::new()));
        assert!(matches!(r, Err(DsError::Unsupported { .. })), "{r:?}");
        assert_eq!(is_supported_raw(p), Ok(false));
        assert!(raw_extensions().iter().any(|e| e == "pef"));
        let t = CancelToken::new();
        t.cancel();
        assert_eq!(
            block_on(raw_metadata("/x".into(), t)),
            Err(DsError::Cancelled)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn metadata_mapping_orients_dimensions_and_converts_units() {
        let m = ds_raw::ImageMeta {
            make: "SONY".into(),
            model: "ILCE-9".into(),
            lens: Some("FE 85mm F1.4 GM".into()),
            iso: Some(800),
            shutter: Some(ds_raw::Rational { num: 1, den: 250 }),
            shutter_display: Some("1/250".into()),
            aperture: Some(1.4),
            focal_length: Some(85.0),
            orientation: ds_core::Orientation::Rotate90,
            dimensions: Some(ds_core::Size::new(6000, 4000)),
            af_point: Some(ds_raw::NormPoint { x: 0.25, y: 0.75 }),
            shot_number: Some(12),
            ..Default::default()
        };
        let r = RawMetadata::from(m);
        assert_eq!((r.width, r.height), (Some(4000), Some(6000)));
        assert_eq!(r.orientation, 6);
        assert!((r.shutter_seconds.unwrap() - 0.004).abs() < 1e-12);
        assert_eq!((r.af_x, r.af_y), (Some(0.25), Some(0.75)));
        assert_eq!(r.shot_number, Some(12));
        assert_eq!(r.capture_time, None);

        let t = ds_raw::CaptureTime {
            year: 2026,
            month: 8,
            day: 8,
            hour: 17,
            minute: 25,
            second: 40,
            nanos: 120_000_000,
            utc_offset_minutes: Some(-240),
        };
        let with_time = RawMetadata::from(ds_raw::ImageMeta {
            capture_time: Some(t),
            ..Default::default()
        });
        let c = with_time.capture_time.unwrap();
        assert_eq!(
            (c.year, c.month, c.second, c.nanos),
            (2026, 8, 40, 120_000_000)
        );
        assert_eq!(c.utc_offset_minutes, Some(-240));
        assert_eq!(with_time.width, None);
    }

    /// The EXIF position crosses with its signs (south, west, below sea level);
    /// a geotag position is validated before it reaches a sidecar.
    #[test]
    fn gps_crosses_signed_and_positions_are_validated() {
        let r = RawMetadata::from(ds_raw::ImageMeta {
            gps: Some(ds_raw::GpsPosition {
                latitude: -33.5,
                longitude: -70.25,
                altitude_m: Some(-12.0),
            }),
            ..Default::default()
        });
        assert_eq!(
            r.gps,
            Some(GeoPosition {
                latitude: -33.5,
                longitude: -70.25,
                altitude_m: Some(-12.0)
            })
        );
        assert_eq!(RawMetadata::from(ds_raw::ImageMeta::default()).gps, None);
        // A cache entry from before GPS reads as none.
        let old: RawMetadata = serde_json::from_str(r#"{"make":"SONY","model":"ILCE-9","lens":null,
            "iso":null,"shutter_seconds":null,"shutter_display":null,"aperture":null,"focal_length":null,
            "focal_length_35mm":null,"capture_time":null,"orientation":1,"width":null,"height":null,
            "serial":null,"shutter_count":null,"af_x":null,"af_y":null,"shot_number":null,
            "sequence_image_number":null}"#)
        .unwrap();
        assert_eq!(old.gps, None);

        let p = GeoPosition {
            latitude: 22.2,
            longitude: -159.5,
            altitude_m: None,
        };
        let g = p.to_point().unwrap();
        assert_eq!(GeoPosition::from(g), p);
        let bad = GeoPosition {
            latitude: 95.0,
            ..p
        };
        assert!(
            matches!(bad.to_point(), Err(DsError::InvalidArgument { .. })),
            "{:?}",
            bad.to_point()
        );
    }

    #[test]
    fn filename_sequences() {
        assert_eq!(filename_sequence("/x/DSC07062.ARW".into()), Ok(Some(7062)));
        assert_eq!(filename_sequence("/x/photo.ARW".into()), Ok(None));
    }

    #[test]
    fn preview_mapping_keeps_bytes_and_orientation() {
        let p = EmbeddedPreviewData::from(ds_raw::EmbeddedPreview {
            jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
            size: ds_core::Size::new(1616, 1080),
            orientation: ds_core::Orientation::Rotate270,
        });
        assert_eq!((p.width, p.height, p.orientation), (1616, 1080, 8));
        assert_eq!(p.jpeg.len(), 4);
    }

    #[test]
    fn real_samples_have_metadata_and_a_preview() {
        let Some(path) = ds_testdata::sample_arw() else {
            eprintln!("no samples/ — skipping");
            return;
        };
        let p = path.to_string_lossy().into_owned();
        assert_eq!(is_supported_raw(p.clone()), Ok(true));
        let m = block_on(raw_metadata(p.clone(), CancelToken::new())).unwrap();
        assert_eq!(m.model, "ILCE-9");
        assert!(m.capture_time.is_some());
        let (w, h) = (m.width.unwrap(), m.height.unwrap());
        assert_eq!(w.max(h), 6000);
        let t = std::time::Instant::now();
        let pv = block_on(embedded_preview(p, CancelToken::new())).unwrap();
        eprintln!(
            "embedded preview {}x{} {} KB in {:?}",
            pv.width,
            pv.height,
            pv.jpeg.len() / 1024,
            t.elapsed()
        );
        assert_eq!(&pv.jpeg[..2], &[0xff, 0xd8]);
        assert!(pv.width >= 1000);
    }
}
