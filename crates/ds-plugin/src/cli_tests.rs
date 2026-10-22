use super::*;
use std::os::unix::fs::PermissionsExt;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn new_validate_pack_for_looks() {
    let d = tempfile::tempdir().unwrap();
    let out = run(&args(&[
        "new",
        "My Looks",
        "--kind",
        "looks",
        "--dir",
        s(d.path()),
    ]))
    .unwrap();
    assert!(out.contains("created"));
    let folder = d.path().join("My Looks.dsplugin");
    let text = fs::read_to_string(folder.join(MANIFEST_FILE)).unwrap();
    assert!(text.contains("id = \"com.example.my-looks\""));
    assert!(
        fs::read_to_string(folder.join("looks.json"))
            .unwrap()
            .contains("\"warm-fade\"")
    );
    let v = run(&args(&["validate", s(&folder)])).unwrap();
    assert!(v.contains("looks `looks` (contract 1): 1 items OK"), "{v}");
    assert!(v.contains("unsigned"));
    assert!(
        run(&args(&[
            "new",
            "My Looks",
            "--kind",
            "looks",
            "--dir",
            s(d.path())
        ]))
        .unwrap_err()
        .contains("already exists")
    );
}

#[test]
fn new_post_process_validates_runs_and_packs_to_wasm() {
    let d = tempfile::tempdir().unwrap();
    run(&args(&[
        "new",
        "Frame",
        "--kind",
        "post-process",
        "--id",
        "com.me.frame",
        "--dir",
        s(d.path()),
    ]))
    .unwrap();
    let folder = d.path().join("Frame.dsplugin");
    let text = fs::read_to_string(folder.join(MANIFEST_FILE)).unwrap();
    assert!(text.contains("id = \"com.me.frame\"") && text.contains("name = \"Frame\""));
    let v = run(&args(&["validate", s(&folder)])).unwrap();
    assert!(v.contains("runs on 64×64"), "{v}");
    assert!(!v.contains("(0 bytes changed)"), "the border paints: {v}");

    fs::write(folder.join(".DS_Store"), "x").unwrap();
    let out_dir = d.path().join("dist");
    run(&args(&["pack", s(&folder), "--out", s(&out_dir)])).unwrap();
    let packed = out_dir.join("Frame.dsplugin");
    assert!(packed.join("border.wasm").exists());
    assert!(!packed.join("border.wat").exists() && !packed.join(".DS_Store").exists());
    assert!(
        fs::read_to_string(packed.join(MANIFEST_FILE))
            .unwrap()
            .contains("module = \"border.wasm\"")
    );
    assert!(
        run(&args(&["validate", s(&packed)]))
            .unwrap()
            .contains("runs on")
    );
    assert!(
        run(&args(&["pack", s(&folder), "--out", s(&out_dir)]))
            .unwrap_err()
            .contains("already exists")
    );
    // As a zip, which validates and installs like the folder.
    let zip = d.path().join("Frame.dsplugin");
    let zip = zip.with_file_name("Frame-1.1.0.dsplugin");
    run(&args(&["pack", s(&folder), "--zip", s(&zip)])).unwrap();
    assert!(zip.is_file());
    assert!(
        run(&args(&["validate", s(&zip)]))
            .unwrap()
            .contains("runs on")
    );
    assert!(
        run(&args(&["pack", s(&folder), "--zip", s(&zip)]))
            .unwrap_err()
            .contains("already exists")
    );
    assert!(
        run(&args(&["pack", s(&folder)]))
            .unwrap_err()
            .contains("usage")
    );
}

#[test]
fn validate_is_strict_where_loading_is_lenient() {
    let d = tempfile::tempdir().unwrap();
    run(&args(&[
        "new",
        "L",
        "--kind",
        "looks",
        "--dir",
        s(d.path()),
    ]))
    .unwrap();
    let folder = d.path().join("L.dsplugin");
    let toml = fs::read_to_string(folder.join(MANIFEST_FILE)).unwrap();
    fs::write(folder.join(MANIFEST_FILE), format!("colour = 1\n{toml}")).unwrap();
    assert!(
        run(&args(&["validate", s(&folder)]))
            .unwrap_err()
            .contains("colour")
    );
    fs::write(folder.join(MANIFEST_FILE), toml).unwrap();
    fs::write(
        folder.join("looks.json"),
        r#"[{"id":"x","name":"X","values":{"exposure.ev":99}}]"#,
    )
    .unwrap();
    assert!(
        run(&args(&["validate", s(&folder)]))
            .unwrap_err()
            .contains("outside")
    );
}

#[test]
fn keygen_and_sign() {
    let d = tempfile::tempdir().unwrap();
    run(&args(&[
        "new",
        "L",
        "--kind",
        "looks",
        "--dir",
        s(d.path()),
    ]))
    .unwrap();
    let folder = d.path().join("L.dsplugin");
    let key = d.path().join("key.json");
    assert!(
        run(&args(&["keygen", "Jane", "--out", s(&key)]))
            .unwrap()
            .contains("public key")
    );
    assert_eq!(
        fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600,
        "the secret key is owner-only"
    );
    assert!(
        run(&args(&["keygen", "Jane", "--out", s(&key)]))
            .unwrap_err()
            .contains("never overwritten")
    );
    assert!(
        run(&args(&["sign", s(&folder), "--key", s(&key)]))
            .unwrap()
            .contains("as key ")
    );
    assert!(
        run(&args(&["validate", s(&folder)]))
            .unwrap()
            .contains("(claims to be Jane)")
    );
    fs::write(d.path().join("bad.json"), "{}").unwrap();
    assert!(
        run(&args(&[
            "sign",
            s(&folder),
            "--key",
            s(&d.path().join("bad.json"))
        ]))
        .is_err()
    );
}

#[test]
fn usage_errors_and_contracts() {
    assert!(run(&[]).unwrap_err().contains("usage"));
    assert!(run(&args(&["frobnicate"])).unwrap_err().contains("usage"));
    assert!(run(&args(&["validate"])).unwrap_err().contains("usage"));
    assert!(
        run(&args(&["new", "x", "--kind"]))
            .unwrap_err()
            .contains("needs a value")
    );
    assert!(
        run(&args(&["new", "x", "--kind", "lens"]))
            .unwrap_err()
            .contains("unknown kind")
    );
    assert!(
        run(&args(&["new", "!!!", "--kind", "looks"]))
            .unwrap_err()
            .contains("letters")
    );
    assert!(
        run(&args(&["new", "x", "--kind", "looks", "--id", "BAD"]))
            .unwrap_err()
            .contains("reverse-DNS")
    );
    let c = run(&args(&["contracts"])).unwrap();
    assert!(c.contains("export_post_process") && c.contains("(code)") && c.contains("host API 1"));
    assert!(run(&args(&["validate", "/nonexistent/x.dsplugin"])).is_err());
}

#[test]
fn the_bundled_reference_plugins_validate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
    let v = run(&args(&["validate", s(&root.join("film-border.dsplugin"))])).unwrap();
    assert!(
        v.contains("Film Border 1.1.0") && v.contains("canvas 66×66"),
        "{v}"
    );
    let looks = root.join("builtin/darkslide-looks.dsplugin");
    if looks.exists() {
        let v = run(&args(&["validate", s(&looks)])).unwrap();
        assert!(v.contains("items OK"), "{v}");
    }
}

#[test]
fn validate_reports_destinations_and_phrases() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("D.dsplugin");
    fs::create_dir(&f).unwrap();
    let wat = include_str!("../../../plugins/film-border.dsplugin/border.wat");
    fs::write(f.join("d.wat"), wat).unwrap();
    fs::write(
        f.join("p.json"),
        r#"{"language":"en","phrases":{"keeper":"key-pick","bin it":"key-reject"}}"#,
    )
    .unwrap();
    fs::write(
        f.join(MANIFEST_FILE),
        "manifest = 1\nid = \"com.example.d\"\nname = \"D\"\nversion = \"1.0.0\"\napi = 1\npermissions = [\"files.write:out\"]\n\
         [[contributes.export_destination]]\nid = \"d\"\nmodule = \"d.wat\"\ncontract = 1\nformats = [\"jpeg\"]\n\
         [[contributes.voice_phrases]]\nid = \"p\"\nfile = \"p.json\"\ncontract = 1\n",
    )
    .unwrap();
    let v = run(&args(&["validate", s(&f)])).unwrap();
    assert!(v.contains("don't run in this version"), "{v}");
    assert!(
        v.contains("voice_phrases `p` (contract 1): 2 items OK"),
        "{v}"
    );
}
