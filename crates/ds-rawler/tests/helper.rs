//! The built `darkslide-rawler` against ds-raw's in-process rawler: the same answers.

use std::path::{Path, PathBuf};
use std::process::Command;

fn helper() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_darkslide-rawler"))
}

/// RAWs to compare: the samples and the compatibility corpus, when present.
fn files() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ds_testdata::sample_arw().into_iter().collect();
    if let Some(dir) = std::env::var_os("DS_RAWCOMPAT_DIR")
        && let Ok(rd) = std::fs::read_dir(dir)
    {
        let mut corpus: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        corpus.sort();
        v.extend(corpus.into_iter().filter(|p| ds_raw::has_raw_extension(p)));
    }
    v
}

#[test]
fn it_answers_who_it_is() {
    let info = ds_raw::helper::check(helper()).unwrap();
    assert_eq!(info.protocol, ds_raw::helper::PROTOCOL);
    assert!(info.rawler.contains('.'), "{info:?}");
}

#[test]
fn it_explains_itself_when_run_by_hand() {
    let out = Command::new(helper()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not run by hand"));
}

#[test]
fn errors_come_back_as_errors() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.raf");
    std::fs::write(&p, [b"FUJIFILMCCD-RAW ".as_slice(), &[0; 4096]].concat()).unwrap();
    let raw = ds_raw::open(&p).unwrap();
    assert!(raw.decode_with_helper(helper()).is_err());
    assert!(raw.metadata_with_helper(helper()).is_err());
}

/// Every file decodes through the helper exactly as rawler decodes it in process.
#[test]
fn decodes_match_the_in_process_decoder() {
    for p in files() {
        let raw = ds_raw::open(&p).unwrap();
        let want = raw.decode_rawler();
        let got = raw.decode_with_helper(helper());
        match (want, got) {
            (Ok((w, wi)), Ok((g, gi))) => {
                assert_eq!(gi, wi, "{}", p.display());
                assert_eq!(g.size, w.size, "{}", p.display());
                assert_eq!(g.cfa, w.cfa);
                assert!(g.data == w.data, "{}: samples differ", p.display());
                assert_eq!(
                    g.black_level.map(f32::to_bits),
                    w.black_level.map(f32::to_bits)
                );
                assert_eq!(g.white_level.to_bits(), w.white_level.to_bits());
                assert_eq!(
                    g.wb_as_shot.map(f32::to_bits),
                    w.wb_as_shot.map(f32::to_bits)
                );
                assert_eq!(g.cam_to_xyz, w.cam_to_xyz);
                assert_eq!(g.orientation, w.orientation);
            }
            (Err(w), Err(g)) => assert_eq!(g.to_string(), w.to_string(), "{}", p.display()),
            (w, g) => panic!(
                "{}: in process {:?}, helper {:?}",
                p.display(),
                w.err(),
                g.err()
            ),
        }
    }
}
