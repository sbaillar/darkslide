//! Opt-in camera-compatibility run over real RAWs from many makers.
//!
//! Fetch the CC0 samples (`scripts/fetch-rawcompat.sh`, manifest in
//! `tests/compat/samples.json`), then:
//!
//! ```text
//! DS_RAWCOMPAT_DIR=~/Library/Caches/darkslide-rawcompat \
//!   cargo test -p ds-raw --release --test compat -- --nocapture
//! ```
//!
//! Every file must yield metadata and an embedded preview (what import and
//! culling need). A full decode must either succeed with a valid image or
//! fail with [`RawError::Unsupported`] (the app then says it can't develop
//! the file yet); any other failure, or a panic, fails the test. A Markdown
//! table of the results goes to stdout and, with `DS_RAWCOMPAT_REPORT=<file>`,
//! to that file. Without `DS_RAWCOMPAT_DIR` the test does nothing.
#![allow(clippy::cast_precision_loss)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ds_raw::RawError;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| ds_raw::has_raw_extension(p))
        .collect();
    out.sort();
    out
}

/// What culling shows: ISO, shutter, aperture, focal length and capture date
/// ("–" for each one missing).
fn capture(m: &ds_raw::ImageMeta) -> String {
    let dash = || "–".to_string();
    let date = m.capture_time.map_or_else(dash, |t| {
        format!("{:04}-{:02}-{:02}", t.year, t.month, t.day)
    });
    format!(
        "ISO {}, {}, f/{}, {} mm, {date}",
        m.iso.map_or_else(dash, |i| i.to_string()),
        m.shutter_display.clone().unwrap_or_else(dash),
        m.aperture.map_or_else(dash, |a| format!("{a:.1}")),
        m.focal_length.map_or_else(dash, |f| format!("{f:.0}")),
    )
}

/// One file's row, and whether it failed the bar above.
fn check(p: &Path) -> (String, bool) {
    let name = p.file_name().unwrap().to_string_lossy();
    let fmt = p.extension().unwrap().to_string_lossy().to_uppercase();
    let mut ok = true;
    let t = Instant::now();
    let meta = ds_raw::read_metadata(p);
    let meta_ms = ms(t);
    let (camera, capture) = match &meta {
        Ok(m) => (format!("{} {}", m.make, m.model), capture(m)),
        Err(e) => {
            ok = false;
            (String::new(), format!("FAILED: {e}"))
        }
    };
    // Warm read so the timing is the parse, not the first disk read.
    let _ = ds_raw::embedded_preview(p);
    let t = Instant::now();
    let preview = ds_raw::embedded_preview(p);
    let preview_ms = ms(t);
    let preview_col = match &preview {
        Ok(pv) => format!(
            "{}×{} ({} KB), {preview_ms:.1} ms",
            pv.size.width,
            pv.size.height,
            pv.jpeg.len() / 1024
        ),
        Err(e) => {
            ok = false;
            format!("FAILED: {e}")
        }
    };
    let t = Instant::now();
    let decoded = ds_raw::decode(p);
    let decode_ms = ms(t);
    let decode_col = match &decoded {
        Ok(img) => match img.validate() {
            Ok(()) => format!(
                "{}×{} {:?}, {decode_ms:.0} ms",
                img.size.width, img.size.height, img.cfa
            ),
            Err(e) => {
                ok = false;
                format!("INVALID: {e}")
            }
        },
        Err(RawError::Unsupported { reason, .. }) => format!("not yet: {reason}"),
        Err(e) => {
            ok = false;
            format!("FAILED: {e}")
        }
    };
    let pass = match (ok, decoded.is_ok()) {
        (false, _) => "FAIL",
        (true, true) => "full",
        (true, false) => "cull only",
    };
    let row = format!(
        "| {fmt} | {camera} | `{name}` | {capture}, {meta_ms:.1} ms | {preview_col} | {decode_col} | {pass} |"
    );
    (row, ok)
}

#[test]
fn every_format_imports_and_culls() {
    let Some(dir) = std::env::var_os("DS_RAWCOMPAT_DIR").map(PathBuf::from) else {
        return;
    };
    let mut table = String::from(
        "| Format | Camera | File | Metadata | Embedded preview | Full decode | Result |\n\
         |---|---|---|---|---|---|---|\n",
    );
    let mut failed = Vec::new();
    let all = files(&dir);
    assert!(!all.is_empty(), "no RAWs in {}", dir.display());
    for p in &all {
        let (row, ok) = check(p);
        eprintln!("{row}");
        let _ = writeln!(table, "{row}");
        if !ok {
            failed.push(p.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    println!("{table}");
    if let Some(out) = std::env::var_os("DS_RAWCOMPAT_REPORT") {
        std::fs::write(out, &table).unwrap();
    }
    assert!(failed.is_empty(), "failed: {failed:?}");
}
