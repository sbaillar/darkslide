//! A 50,000-photo library for the Find bench and budget test: 25 sessions of
//! 2,000 photos over three years, people tagged on 1 in 20 (suggested on 1 in
//! 50), keywords, a location on 1 in 10, and 512-dimension vectors.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lt_core::FileHash;
use lt_core::date::{DateFilter, days_from_civil};
use lt_session::{FindRecord, FindSpec, GlobalIndex, PersonQuery};

pub const N: usize = 50_000;
pub const SESSIONS: usize = 25;
pub const DIM: usize = 512;
pub const MODEL: &str = "mobileclip-s2";

const PLACES: [&str; 5] = ["Iceland", "Home", "Tahoe", "Paris", "Wedding"];

/// Deterministic pseudo-random vector (xorshift).
pub fn vector(seed: u64) -> Vec<f32> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..DIM)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % 2001) as f32 / 1000.0 - 1.0
        })
        .collect()
}

pub fn key(i: usize) -> FileHash {
    FileHash(*blake3::hash(&i.to_le_bytes()).as_bytes())
}

pub fn root(s: usize) -> PathBuf {
    PathBuf::from(format!(
        "/Volumes/pics/{} {}-{:02} {}",
        s,
        2023 + s % 3,
        1 + s % 12,
        PLACES[s % PLACES.len()]
    ))
}

pub fn records(s: usize) -> Vec<FindRecord> {
    let root = root(s);
    let per = N / SESSIONS;
    (s * per..(s + 1) * per)
        .map(|i| {
            let day =
                days_from_civil(2023 + (s % 3) as i64, 1 + (s % 12) as i64, 1) + (i % 20) as i64;
            FindRecord {
                path: root.join(format!("Capture/DSC{i:05}.ARW")),
                key: Some(key(i)),
                capture_ms: Some(day * 86_400_000 + i as i64),
                rating: (i % 6) as u8,
                keywords: if i % 7 == 0 {
                    vec!["beach".into(), "Sicily".into()]
                } else {
                    vec![]
                },
                people: match i % 40 {
                    0 => vec!["Alex".into()],
                    1 => vec!["Sam".into(), "Alex".into()],
                    _ => vec![],
                },
                suggested: if i % 50 == 3 {
                    vec!["Alex".into()]
                } else {
                    vec![]
                },
                location: if i % 10 == 0 {
                    vec!["Vik".into(), "Iceland".into()]
                } else {
                    vec![]
                },
                ..FindRecord::default()
            }
        })
        .collect()
}

pub fn build(dir: &Path) -> GlobalIndex {
    let mut g = GlobalIndex::open(&dir.join("global.db")).unwrap();
    for s in 0..SESSIONS {
        let root = root(s);
        let name = root.file_name().unwrap().to_string_lossy().into_owned();
        g.sync_find_items(&root, &name, &records(s)).unwrap();
    }
    for chunk in (0..N).collect::<Vec<_>>().chunks(5000) {
        let v: Vec<(FileHash, Vec<f32>)> =
            chunk.iter().map(|&i| (key(i), vector(i as u64))).collect();
        g.store_embeddings(MODEL, &v).unwrap();
    }
    g
}

fn alex() -> PersonQuery {
    PersonQuery {
        term: "alex".into(),
        names: vec!["Alex".into(), "Alexander".into()],
    }
}

/// "alex in iceland 2024": people, a resolved place and a date, no text.
pub fn structured() -> FindSpec {
    FindSpec {
        people: vec![alex()],
        places: vec!["iceland".into()],
        date: Some(DateFilter::range(
            days_from_civil(2024, 1, 1) * 86_400_000,
            days_from_civil(2025, 1, 1) * 86_400_000 - 1,
        )),
        model: MODEL.into(),
        ..FindSpec::default()
    }
}

/// "pics of alex at the beach near nowhere": a person, text and a soft place.
pub fn visual() -> FindSpec {
    let mut place_vectors = HashMap::new();
    place_vectors.insert("nowhere".to_owned(), vector(777));
    FindSpec {
        people: vec![alex()],
        places: vec!["nowhere".into()],
        place_vectors,
        text: "beach".into(),
        text_vector: Some(vector(424_242)),
        model: MODEL.into(),
        options: lt_session::FindOptions {
            min_score: -1.0,
            window: 2.0,
            ..lt_session::FindOptions::default()
        },
        ..FindSpec::default()
    }
}

/// Text only, every photo scored.
pub fn text_only() -> FindSpec {
    FindSpec {
        text: "beach".into(),
        text_vector: Some(vector(424_242)),
        model: MODEL.into(),
        options: lt_session::FindOptions {
            min_score: -1.0,
            window: 2.0,
            ..lt_session::FindOptions::default()
        },
        ..FindSpec::default()
    }
}
