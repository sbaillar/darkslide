//! A 100,000-photo Library for its bench and budget test: 50 sessions of
//! 2,000 photos over five years, four cameras, six lenses, a 200-keyword
//! vocabulary (two or three each), labels, flags, a title on 1 in 10, a
//! position on 1 in 4, an edit on 1 in 5 and no capture time on 1 in 997.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use ds_core::date::days_from_civil;
use ds_core::{ColorTag, PickState};
use ds_session::{GlobalIndex, LibraryQuery, LibraryRecord, LibrarySource, PickFilter};

pub const N: usize = 100_000;
pub const SESSIONS: usize = 50;
pub const PER: usize = N / SESSIONS;
/// The first screen of a big window: 8 columns × 6 rows, plus a row of prefetch.
pub const FIRST_SCREEN: usize = 56;

const CAMERAS: [&str; 4] = ["ILCE-9", "ILCE-7M4", "ILCE-1", "X-T5"];
const LENSES: [&str; 6] = [
    "FE 24-70mm F2.8 GM",
    "FE 70-200mm F2.8 GM OSS",
    "FE 35mm F1.4 GM",
    "FE 85mm F1.4 GM",
    "XF 23mm F1.4",
    "FE 16-35mm F4",
];
const LABELS: [ColorTag; 8] = [
    ColorTag::None,
    ColorTag::Red,
    ColorTag::Orange,
    ColorTag::Yellow,
    ColorTag::Green,
    ColorTag::Blue,
    ColorTag::Pink,
    ColorTag::Purple,
];

pub fn root(s: usize) -> PathBuf {
    PathBuf::from(format!(
        "/library/{:04}-{:02} Shoot {s}",
        2020 + s / 10,
        1 + s % 12
    ))
}

pub fn source(s: usize) -> LibrarySource {
    LibrarySource {
        store: root(s),
        external: None,
        stamp: s as i64,
    }
}

pub fn records(s: usize) -> Vec<LibraryRecord> {
    let start = days_from_civil(2020 + (s / 10) as i64, 1 + (s % 12) as i64, 1) * 86_400_000;
    (0..PER)
        .map(|i| {
            let n = s * PER + i;
            LibraryRecord {
                rel: format!("Capture/DSC{i:05}.ARW"),
                ordinal: 1,
                variant: i as i64 + 1,
                capture_ms: (!n.is_multiple_of(997)).then_some(start + i as i64 * 3_000),
                rating: (n % 6) as u8,
                label: LABELS[n % 8],
                pick: [PickState::Unflagged, PickState::Picked, PickState::Rejected][n % 3],
                camera: CAMERAS[(n / 7) % 4].into(),
                lens: LENSES[(n / 3) % 6].into(),
                keywords: (0..2 + n % 2)
                    .map(|k| format!("keyword {}", (n / 2 * 7 + k * 31) % 200))
                    .collect(),
                title: if n.is_multiple_of(10) {
                    format!("Frame {n} at the lake")
                } else {
                    String::new()
                },
                gps: n.is_multiple_of(4),
                stamp: n as i64,
                edit: (n.is_multiple_of(5)).then_some([(n % 251) as u8; 32]),
            }
        })
        .collect()
}

pub fn build(dir: &Path) -> GlobalIndex {
    let mut g = GlobalIndex::open(&dir.join("global.db")).unwrap();
    for s in 0..SESSIONS {
        let name = root(s).file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            g.sync_library(&root(s), &name, &source(s), &records(s))
                .unwrap()
        );
    }
    g
}

/// The filter and sort changes the budget covers, by name.
pub fn queries() -> Vec<(&'static str, LibraryQuery)> {
    vec![
        ("all_by_capture", LibraryQuery::default()),
        (
            "rating_3_picked",
            LibraryQuery {
                min_rating: 3,
                pick: PickFilter::Picked,
                ..LibraryQuery::default()
            },
        ),
        (
            "label_camera_keyword",
            LibraryQuery {
                labels: vec![ColorTag::Red, ColorTag::Green],
                cameras: vec!["ILCE-9".into()],
                keywords: vec!["Keyword 17".into()],
                ..LibraryQuery::default()
            },
        ),
        (
            "text",
            LibraryQuery {
                text: "lake frame 1".into(),
                ..LibraryQuery::default()
            },
        ),
        (
            "year_by_rating",
            LibraryQuery {
                from_ms: Some(days_from_civil(2022, 1, 1) * 86_400_000),
                to_ms: Some(days_from_civil(2023, 1, 1) * 86_400_000),
                sort: ds_session::LibrarySort::Rating,
                ..LibraryQuery::default()
            },
        ),
        (
            "all_by_name_desc",
            LibraryQuery {
                sort: ds_session::LibrarySort::Name,
                descending: true,
                ..LibraryQuery::default()
            },
        ),
    ]
}
