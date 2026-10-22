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
// Levels, WB and matrices are compared exactly: both decoders compute them
// from the same integers.
#![allow(clippy::float_cmp)]

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

/// FNV-1a over the samples: a cheap fingerprint for the parity table.
#[cfg(feature = "rawler")]
fn fingerprint(data: &[u16]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &v| {
        (h ^ u64::from(v)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Native/rawler differences that are understood: the file, why, and the
/// largest sample difference it explains.
#[cfg(feature = "rawler")]
const KNOWN: &[(&str, &str, u16)] = &[
    (
        "apple-iphone-12-pro-proraw.dng",
        "rawler dithers LinearizationTable output by up to ±½ table step, and wraps two \
         65535 samples to 0; native maps exactly (DNG 1.7 LinearizationTable)",
        32,
    ),
    ("sony-ilce-7m3.arw", CRAW_DITHER, 8),
    ("sony-ilce-7m4.arw", CRAW_DITHER, 8),
    ("nikon-d850.nef", NEF_DITHER, 1),
    ("nikon-z-6-2.nef", NEF_DITHER, 1),
    (
        "pentax-k-3-mark-iii.pef",
        "white level: native reads the camera's own value from the maker note (0x7E, 16379, \
         as in its DNG); rawler takes 16375 from its camera database",
        0,
    ),
];

#[cfg(feature = "rawler")]
const NEF_DITHER: &str = "rawler dithers decoded NEF values down by 0 or 1 (its linearization \
     step); native returns the decoded values";

#[cfg(feature = "rawler")]
const CRAW_DITHER: &str = "rawler dithers cRAW values across each SonyToneCurve step (up to \
     ±8); native returns the curve value itself";

/// Samples that differ only as `known` explains (or not at all).
#[cfg(feature = "rawler")]
fn explained(n: &[u16], r: &[u16], tol: u16) -> bool {
    n.iter()
        .zip(r)
        .all(|(&a, &b)| a.abs_diff(b) <= tol || (a == u16::MAX && b == 0))
}

/// How a native decode differs from rawler's, `None` when the sensor data
/// is identical: size (the crop), CFA, black and white levels, every sample.
#[cfg(feature = "rawler")]
fn sensor_diff(n: &ds_core::RawImage, r: &ds_core::RawImage) -> Option<String> {
    let mut out = Vec::new();
    if n.size != r.size {
        out.push(format!("size {:?} vs {:?}", n.size, r.size));
    }
    if n.cfa != r.cfa {
        out.push(format!("CFA {:?} vs {:?}", n.cfa, r.cfa));
    }
    if n.black_level != r.black_level {
        out.push(format!("black {:?} vs {:?}", n.black_level, r.black_level));
    }
    if n.white_level != r.white_level {
        out.push(format!("white {} vs {}", n.white_level, r.white_level));
    }
    if n.data.len() == r.data.len() && n.data != r.data {
        let diff: Vec<usize> = (0..n.data.len())
            .filter(|&i| n.data[i] != r.data[i])
            .collect();
        let max = diff
            .iter()
            .map(|&i| (i32::from(n.data[i]) - i32::from(r.data[i])).abs())
            .max()
            .unwrap_or(0);
        out.push(format!(
            "{} samples differ (first at {}, max |Δ| {max})",
            diff.len(),
            diff[0]
        ));
    }
    (!out.is_empty()).then(|| out.join("; "))
}

/// Native decoders against rawler on every corpus file (and the A9 samples)
/// the native decoders cover: the sensor data must be identical. Colour
/// differences (matrix source, WB) are reported, not failed.
#[cfg(feature = "rawler")]
#[test]
fn native_decoders_match_rawler() {
    let Some(dir) = std::env::var_os("DS_RAWCOMPAT_DIR").map(PathBuf::from) else {
        return;
    };
    let mut all = files(&dir);
    all.extend(ds_testdata::samples_with_ext("arw").into_iter().take(2));
    let mut table =
        String::from("| File | Native | rawler | Sensor data | Colour |\n|---|---|---|---|---|\n");
    let mut failed = Vec::new();
    for p in &all {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let f = ds_raw::open(p).unwrap();
        let t = Instant::now();
        let native = f.decode_native();
        let native_ms = ms(t);
        let t = Instant::now();
        let rawler = f.decode_rawler();
        let rawler_ms = ms(t);
        let (verdict, colour) = match (&native, &rawler) {
            (Ok(None), _) => ("not native".to_string(), String::new()),
            (Err(RawError::Unsupported { reason, .. }), Err(_)) => {
                (format!("both unsupported: {reason}"), String::new())
            }
            (Err(e), _) => {
                failed.push(name.clone());
                (format!("NATIVE FAILED: {e}"), String::new())
            }
            (Ok(Some(_)), Err(e)) => (format!("rawler failed: {e}"), String::new()),
            (Ok(Some((n, _))), Ok((r, _))) => {
                let colour = if n.wb_as_shot == r.wb_as_shot && n.cam_to_xyz == r.cam_to_xyz {
                    "same".to_string()
                } else {
                    format!(
                        "WB {:?} vs {:?}{}",
                        n.wb_as_shot,
                        r.wb_as_shot,
                        if n.cam_to_xyz == r.cam_to_xyz {
                            ""
                        } else {
                            "; matrix differs"
                        }
                    )
                };
                let known = KNOWN.iter().find(|k| k.0 == name);
                // A known level deviation: compare the rest with rawler's level.
                let mut n_cmp = n.clone();
                if known.is_some_and(|k| k.1.starts_with("white level")) {
                    n_cmp.white_level = r.white_level;
                }
                let n = &n_cmp;
                match sensor_diff(n, r) {
                    Some(d)
                        if known.is_some_and(|k| {
                            n.size == r.size && explained(&n.data, &r.data, k.2)
                        }) =>
                    {
                        (
                            format!("known deviation: {d} ({})", known.unwrap().1),
                            colour,
                        )
                    }
                    None if known.is_some() => {
                        (format!("known deviation ({})", known.unwrap().1), colour)
                    }
                    None => (
                        format!(
                            "identical, {}×{} {:?}, {:016x}",
                            n.size.width,
                            n.size.height,
                            n.cfa,
                            fingerprint(&n.data)
                        ),
                        colour,
                    ),
                    Some(d) => {
                        failed.push(name.clone());
                        (format!("DIFFERS: {d}"), colour)
                    }
                }
            }
        };
        let row =
            format!("| `{name}` | {native_ms:.0} ms | {rawler_ms:.0} ms | {verdict} | {colour} |");
        eprintln!("{row}");
        let _ = writeln!(table, "{row}");
    }
    println!("{table}");
    if let Some(out) = std::env::var_os("DS_RAWPARITY_REPORT") {
        std::fs::write(out, &table).unwrap();
    }
    assert!(
        failed.is_empty(),
        "native decode differs or fails: {failed:?}"
    );
}
