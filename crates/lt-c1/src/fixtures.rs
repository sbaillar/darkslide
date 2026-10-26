//! Synthetic Capture One fixtures (see `fixtures/session.sql`).

use std::fs;
use std::path::Path;

use lt_core::adjust::WhiteBalance;

use crate::map::WbConverter;

pub const COS_A: &str = include_str!("../fixtures/a.ARW.cos");
pub const COS_C: &str = include_str!("../fixtures/c.ARW.cos");
pub const COS_D: &str = include_str!("../fixtures/d.ARW.cos");
pub const SQL: &str = include_str!("../fixtures/session.sql");

/// Build the synthetic session database at `path`.
pub fn build_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(SQL).unwrap();
}

/// A C1 session folder at `dir` ("Shoot"): database, `.cos` files, and the
/// image files (`image(rel)` gives each one's bytes).
pub fn build_session(dir: &Path, image: &dyn Fn(&str) -> Vec<u8>) {
    for d in [
        "Capture",
        "Output",
        "CaptureOne/Cache/Thumbnails",
        "Selects",
        "Trash",
    ] {
        fs::create_dir_all(dir.join(d)).unwrap();
    }
    build_db(&dir.join("Shoot.cosessiondb"));
    for rel in ["a.ARW", "b.ARW", "Selects/c.ARW", "Trash/d.ARW"] {
        fs::write(dir.join(rel), image(rel)).unwrap();
    }
    fs::write(dir.join("Output/a.jpg"), b"jpeg").unwrap();
    fs::write(dir.join("CaptureOne/Cache/Thumbnails/a.cot"), b"thumb").unwrap();
    for (sub, name, xml) in [
        ("", "a.ARW", COS_A),
        ("Selects/", "c.ARW", COS_C),
        ("Trash/", "d.ARW", COS_D),
    ] {
        let settings = dir.join(format!("{sub}CaptureOne/Settings153"));
        fs::create_dir_all(&settings).unwrap();
        fs::write(settings.join(format!("{name}.cos")), xml).unwrap();
    }
}

/// White balance double: K = 5000 + 1000 × (user R / default R − 1);
/// `FakeWb(false)` can't read originals.
pub struct FakeWb(pub bool);

impl WbConverter for FakeWb {
    fn convert(&self, _: &Path, d: Option<[f64; 3]>, u: [f64; 3]) -> Option<WhiteBalance> {
        self.0.then(|| WhiteBalance::Custom {
            temp_k: (5000.0 + 1000.0 * (u[0] / d.map_or(1.0, |d| d[0]) - 1.0)) as f32,
            tint: 0.0,
        })
    }
}
