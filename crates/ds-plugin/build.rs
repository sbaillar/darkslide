//! Embeds the BLAKE3 list of every bundled plug-in (`plugins/builtin/*.dsplugin`) in
//! the binary, so the app trusts exactly the plug-ins it was built with: a bundled
//! folder whose files differ from this list doesn't load. No signing key is involved.
//!
//! The hashing mirrors `Package::hashes` (dotfiles skipped, `+x` for executables).

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("read plugins/builtin")
        .flatten()
        .collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        let meta = fs::symlink_metadata(&path).expect("stat");
        if meta.is_dir() {
            walk(root, &path, out);
        } else if meta.is_file() && name != "SIGNATURE" {
            let rel = path
                .strip_prefix(root)
                .expect("inside")
                .to_string_lossy()
                .replace('\\', "/");
            let mut h = blake3::hash(&fs::read(&path).expect("read"))
                .to_hex()
                .to_string();
            if meta.permissions().mode() & 0o111 != 0 {
                h.push_str("+x");
            }
            out.push((rel, h));
        }
    }
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/builtin");
    println!("cargo::rerun-if-changed={}", dir.display());
    let mut code = String::from(
        "/// Bundled plug-in folder name → (path, hash) of each file.\npub static BUILTIN_HASHES: &[(&str, &[(&str, &str)])] = &[\n",
    );
    let mut folders: Vec<_> = fs::read_dir(&dir)
        .map(|r| r.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    folders.sort();
    for f in folders
        .iter()
        .filter(|p| p.is_dir() && p.extension().is_some_and(|e| e == "dsplugin"))
    {
        let mut files = Vec::new();
        walk(f, f, &mut files);
        let name = f.file_name().expect("name").to_string_lossy();
        let _ = writeln!(code, "    ({name:?}, &[");
        for (rel, h) in files {
            let _ = writeln!(code, "        ({rel:?}, {h:?}),");
        }
        code.push_str("    ]),\n");
    }
    code.push_str("];\n");
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("builtin_hashes.rs");
    fs::write(out, code).expect("write builtin_hashes.rs");
}
