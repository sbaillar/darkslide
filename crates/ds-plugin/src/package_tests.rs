use super::*;
use std::os::unix::fs::PermissionsExt;

fn folder() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("plugin.toml"), "x").unwrap();
    fs::create_dir(d.path().join("sub")).unwrap();
    fs::write(d.path().join("sub/a.json"), "[]").unwrap();
    d
}

#[test]
fn reads_files_once_and_hashes_them() {
    let d = folder();
    fs::write(d.path().join(".DS_Store"), "junk").unwrap();
    fs::write(d.path().join("._plugin.toml"), "junk").unwrap();
    fs::write(d.path().join(crate::trust::SIGNATURE_FILE), "{}").unwrap();
    fs::set_permissions(
        d.path().join("sub/a.json"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let p = Package::read(d.path()).unwrap();
    assert_eq!(
        p.names().collect::<Vec<_>>(),
        ["SIGNATURE", "plugin.toml", "sub/a.json"]
    );
    assert_eq!(p.get("plugin.toml"), Some(&b"x"[..]));
    let h = p.hashes();
    assert_eq!(
        h.keys().collect::<Vec<_>>(),
        ["plugin.toml", "sub/a.json"],
        "SIGNATURE isn't hashed"
    );
    assert_eq!(h["plugin.toml"], blake3::hash(b"x").to_hex().to_string());
    assert!(h["sub/a.json"].ends_with("+x"));
}

#[test]
fn refuses_links_odd_names_and_case_clashes() {
    let d = folder();
    std::os::unix::fs::symlink("/etc/hosts", d.path().join("link")).unwrap();
    assert!(
        Package::read_dir(d.path())
            .unwrap_err()
            .to_string()
            .contains("symbolic link")
    );

    let d = folder();
    fs::hard_link(d.path().join("plugin.toml"), d.path().join("hard.toml")).unwrap();
    assert!(
        Package::read_dir(d.path())
            .unwrap_err()
            .to_string()
            .contains("hard link")
    );

    let d = folder();
    fs::write(d.path().join("caf\u{e9}.json"), "x").unwrap();
    assert!(
        Package::read_dir(d.path())
            .unwrap_err()
            .to_string()
            .contains("plain ASCII")
    );

    let mut p = Package::default();
    let mut total = 0;
    p.add("A.json", vec![], false, &mut total).unwrap();
    p.add("a.json", vec![], false, &mut total).unwrap();
    assert!(
        p.check_case()
            .unwrap_err()
            .to_string()
            .contains("ignoring case")
    );
    assert!(p.add("a.json", vec![], false, &mut total).is_err());
    assert!(Package::read_dir(&d.path().join("plugin.toml")).is_err());
}

#[test]
fn zips_round_trip_and_strip_one_top_folder() {
    let d = folder();
    let p = Package::read_dir(d.path()).unwrap();
    let z = p.to_zip().unwrap();
    let back = Package::read_zip(&z).unwrap();
    assert_eq!(back.hashes(), p.hashes());

    // A zip made of "Name.dsplugin/…" (Finder's Compress) reads the same.
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default();
    for (name, body) in [
        ("X.dsplugin/plugin.toml", "x"),
        ("X.dsplugin/sub/a.json", "[]"),
        ("__MACOSX/._x", "junk"),
        ("X.dsplugin/.DS_Store", "j"),
    ] {
        w.start_file(name, o).unwrap();
        std::io::Write::write_all(&mut w, body.as_bytes()).unwrap();
    }
    w.add_directory("X.dsplugin/empty/", o).unwrap();
    let z = w.finish().unwrap().into_inner();
    let finder = Package::read_zip(&z).unwrap();
    assert_eq!(
        finder.names().collect::<Vec<_>>(),
        ["plugin.toml", "sub/a.json"]
    );

    let f = tempfile::NamedTempFile::new().unwrap();
    fs::write(f.path(), p.to_zip().unwrap()).unwrap();
    assert_eq!(Package::read(f.path()).unwrap().hashes(), p.hashes());
}

#[test]
fn zips_cannot_escape_or_hold_links() {
    let make = |entries: &[(&str, &str)]| {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            std::io::Write::write_all(&mut w, body.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    };
    assert!(
        Package::read_zip(&make(&[("../evil", "x")]))
            .unwrap_err()
            .to_string()
            .contains("leaves")
    );
    assert!(Package::read_zip(&make(&[("/abs", "x")])).is_err());
    assert!(
        Package::read_zip(&make(&[(".hidden", "x")]))
            .unwrap_err()
            .to_string()
            .contains("no files")
    );
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    w.add_symlink(
        "link",
        "/etc/hosts",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    let z = w.finish().unwrap().into_inner();
    assert!(
        Package::read_zip(&z)
            .unwrap_err()
            .to_string()
            .contains("link")
    );
    assert!(Package::read_zip(b"not a zip").is_err());
    assert!(
        Package::read_zip(&vec![0; (MAX_ZIP_BYTES + 1) as usize])
            .unwrap_err()
            .to_string()
            .contains("too large")
    );
}

#[test]
fn writes_a_copy_with_modes() {
    let d = folder();
    fs::set_permissions(
        d.path().join("sub/a.json"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let p = Package::read_dir(d.path()).unwrap();
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("copy");
    p.write_to(&dest).unwrap();
    assert_eq!(Package::read_dir(&dest).unwrap(), p);
    assert!(p.write_to(&dest).is_err(), "never into an existing folder");
    let mut q = p.clone();
    q.put("new.txt", b"n".to_vec()).unwrap();
    assert!(q.put("../x", vec![]).is_err());
    q.remove("sub/a.json");
    assert_eq!(q.names().collect::<Vec<_>>(), ["new.txt", "plugin.toml"]);
    assert!(ignored(".git") && !ignored("a"));
}
