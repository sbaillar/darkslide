//! `ds-raw` adapters: real capture metadata for naming, and a cheap
//! structural check of copied RAWs.

use std::path::Path;

use chrono::NaiveDate;

use crate::plan::{CaptureMeta, MetaProvider};

/// [`MetaProvider`] backed by `ds_raw::open(path).metadata()`: camera model
/// and capture time (with sub-seconds when recorded). Unreadable files yield
/// empty metadata, so naming falls back to the file mtime.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawMeta;

impl MetaProvider for RawMeta {
    fn capture_meta(&self, path: &Path) -> CaptureMeta {
        let Ok(m) = ds_raw::open(path).and_then(|r| r.metadata()) else {
            return CaptureMeta::default();
        };
        CaptureMeta {
            captured_at: m.capture_time.and_then(naive),
            camera: Some(m.model).filter(|s| !s.is_empty()),
        }
    }
}

fn naive(t: ds_raw::CaptureTime) -> Option<chrono::NaiveDateTime> {
    NaiveDate::from_ymd_opt(i32::from(t.year), u32::from(t.month), u32::from(t.day))?
        .and_hms_nano_opt(
            u32::from(t.hour),
            u32::from(t.minute),
            u32::from(t.second.min(59)),
            t.nanos,
        )
}

/// The RAW's container, metadata and embedded preview all parse. Cheap: a
/// few small reads plus the preview JPEG.
pub fn structurally_valid(path: &Path) -> bool {
    ds_raw::open(path).is_ok_and(|r| r.metadata().is_ok() && r.preview().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_raw_files_give_empty_meta_and_fail_the_check() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("fake.ARW");
        std::fs::write(&p, b"not a raw").unwrap();
        assert_eq!(RawMeta.capture_meta(&p), CaptureMeta::default());
        assert!(!structurally_valid(&p));
        assert!(!structurally_valid(&t.path().join("missing.ARW")));
    }

    #[test]
    fn capture_time_conversion() {
        let t = ds_raw::CaptureTime {
            year: 2026,
            month: 8,
            day: 6,
            hour: 9,
            minute: 30,
            second: 60,
            nanos: 250_000_000,
            utc_offset_minutes: None,
        };
        let n = naive(t).unwrap();
        assert_eq!(n.to_string(), "2026-08-06 09:30:59.250");
        assert!(naive(ds_raw::CaptureTime { month: 13, ..t }).is_none());
    }

    #[test]
    fn real_sample_parses_and_truncated_copy_does_not() {
        let Some(arw) = ds_testdata::sample_arw() else {
            return;
        };
        let m = RawMeta.capture_meta(&arw);
        assert_eq!(m.camera.as_deref(), Some("ILCE-9"));
        assert!(m.captured_at.is_some());
        assert!(structurally_valid(&arw));
        let t = tempfile::tempdir().unwrap();
        let cut = t.path().join("cut.ARW");
        let bytes = std::fs::read(&arw).unwrap();
        std::fs::write(&cut, &bytes[..4096]).unwrap();
        assert!(!structurally_valid(&cut), "a truncated RAW must not pass");
    }
}
