//! A build without the `rawler` feature (Luxtable's own): files the native decoders
//! don't cover need the rawler helper, and go through it once it is set. The helper
//! is the one `LT_RAWLER_HELPER` names (CI builds `luxtable-rawler` first); without
//! it only the "not installed" half runs.
#![cfg(not(feature = "rawler"))]

use lt_raw::{DecodePath, RawError};
use std::path::PathBuf;

fn corpus(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("LT_RAWCOMPAT_DIR")?).join(name);
    p.exists().then_some(p)
}

/// One test, as the helper setting is process-wide.
#[test]
fn files_need_the_helper_and_go_through_it_once_set() {
    let helper = std::env::var_os("LT_RAWLER_HELPER").map(PathBuf::from);
    lt_raw::set_rawler_helper(None);
    // A non-empty path that isn't there stands in for "none" (the environment would win
    // over an unset one).
    let none = PathBuf::from("");
    lt_raw::set_rawler_helper(Some(none));
    let a9 = lt_testdata::sample_arw();
    let cr3 = corpus("canon-eos-r5.cr3");
    if helper.is_none() {
        for p in a9.iter().chain(&cr3) {
            let e = lt_raw::decode(p).unwrap_err();
            assert!(
                matches!(&e, RawError::Unsupported { reason, .. } if reason.contains("rawler helper")),
                "{}: {e}",
                p.display()
            );
        }
        let dng = tempfile::tempfile().unwrap();
        let img = lt_core::RawImage {
            size: lt_core::Size::new(2, 2),
            cfa: lt_core::CfaPattern::LinearRgb,
            data: vec![0; 12],
            black_level: [0.0; 4],
            white_level: 1.0,
            wb_as_shot: [1.0; 3],
            wb_daylight: [1.0; 3],
            cam_to_xyz: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            orientation: lt_core::Orientation::Normal,
            default_crop: None,
        };
        let e = lt_raw::write_linear_dng(dng, img, &lt_raw::DngMeta::default()).unwrap_err();
        assert!(e.to_string().contains("rawler helper"), "{e}");
        return;
    }
    lt_raw::set_rawler_helper(helper);
    if let Some(p) = &a9 {
        // Native decode, the colour matrix from the helper.
        let (img, info) = lt_raw::open(p).unwrap().decode_with_info().unwrap();
        assert_eq!(info.path, DecodePath::SonyUncompressed);
        assert!(img.cam_to_xyz[1][1] > 0.0);
    }
    if let Some(p) = &cr3 {
        let raw = lt_raw::open(p).unwrap();
        assert_eq!(raw.decode_with_info().unwrap().1.path, DecodePath::Rawler);
        assert_eq!(raw.metadata().unwrap().model, "EOS R5");
    }
}
