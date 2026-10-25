//! `ds-plugin`: make, check, package and sign plug-ins.
//!
//! ```text
//! ds-plugin new <name> --kind looks|post-process [--id <reverse-dns>] [--dir <parent>]
//! ds-plugin validate <folder or .dsplugin zip>
//! ds-plugin pack <folder> --out <parent> | --zip <file.dsplugin>
//! ds-plugin keygen <signer name> --out <key.json>
//! ds-plugin sign <folder> --key <key.json>
//! ds-plugin contracts
//! ```

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::host::{MANIFEST_FILE, PLUGIN_EXT, inspect_package};
use crate::manifest::{CONTRACTS, HOST_API, Kind, MANIFEST_VERSION};
use crate::package::Package;
use crate::trust::{self, KeyFile, SIGNATURE_FILE};
use crate::wasm::{Pixels, PostProcessor, WasmModule};
use crate::{darkslide_version, pack};

const USAGE: &str = "usage:
  ds-plugin new <name> --kind looks|post-process [--id <reverse-dns id>] [--dir <parent>]
  ds-plugin validate <folder or .dsplugin zip>
  ds-plugin pack <folder> --out <parent> | --zip <file.dsplugin>
  ds-plugin keygen <signer name> --out <key.json>
  ds-plugin sign <folder> --key <key.json>
  ds-plugin contracts";

const LOOKS_TOML: &str = include_str!("../../../plugins/templates/looks/plugin.toml");
const LOOKS_JSON: &str = include_str!("../../../plugins/templates/looks/looks.json");
const BORDER_TOML: &str = include_str!("../../../plugins/film-border.dsplugin/plugin.toml");
const BORDER_WAT: &str = include_str!("../../../plugins/film-border.dsplugin/border.wat");

/// Run one command; what it prints, or why it failed.
pub fn run(args: &[String]) -> Result<String, String> {
    let (cmd, rest) = args.split_first().ok_or(USAGE)?;
    let mut positional = Vec::new();
    let mut flags = std::collections::BTreeMap::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if let Some(flag) = a.strip_prefix("--") {
            let v = it
                .next()
                .ok_or_else(|| format!("--{flag} needs a value\n{USAGE}"))?;
            flags.insert(flag.to_owned(), v.clone());
        } else {
            positional.push(a.clone());
        }
    }
    let flag = |k: &str| flags.get(k).map(String::as_str);
    let one = || match positional.as_slice() {
        [p] => Ok(p.as_str()),
        _ => Err(USAGE.to_owned()),
    };
    match cmd.as_str() {
        "new" => new(
            one()?,
            flag("kind").ok_or(USAGE)?,
            flag("id"),
            Path::new(flag("dir").unwrap_or(".")),
        ),
        "validate" => validate(Path::new(one()?)),
        "pack" => pack_folder(
            Path::new(one()?),
            flag("out").map(Path::new),
            flag("zip").map(Path::new),
        ),
        "keygen" => keygen(one()?, Path::new(flag("out").ok_or(USAGE)?)),
        "sign" => sign(Path::new(one()?), Path::new(flag("key").ok_or(USAGE)?)),
        "contracts" => Ok(contracts()),
        _ => Err(USAGE.to_owned()),
    }
}

fn slug(name: &str) -> String {
    let s: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn new(name: &str, kind: &str, id: Option<&str>, parent: &Path) -> Result<String, String> {
    let slug = slug(name);
    if slug.is_empty() {
        return Err("the name needs some letters or digits".into());
    }
    let id = id.map_or_else(|| format!("com.example.{slug}"), str::to_owned);
    crate::manifest::validate_id(&id)?;
    let dir = parent.join(format!("{name}.{PLUGIN_EXT}"));
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    let fill = |t: &str| {
        t.replace("{{id}}", &id)
            .replace("{{name}}", name)
            .replace("{{slug}}", &slug)
    };
    let files: Vec<(&str, String)> = match kind {
        "looks" => vec![
            (MANIFEST_FILE, fill(LOOKS_TOML)),
            ("looks.json", fill(LOOKS_JSON)),
        ],
        "post-process" => {
            // The reference plug-in made the author's own: its id, name, a first version,
            // no borrowed authors or summary (they would say it is Film Border).
            let toml = BORDER_TOML
                .replace(
                    "# Film Border: Darkslide's reference code plug-in",
                    &format!("# {name}: a code plug-in"),
                )
                .replace("com.example.film-border", &id)
                .replace(
                    "name = \"Film Border\"\nversion = \"1.1.0\"",
                    &format!("name = \"{name}\"\nversion = \"1.0.0\""),
                )
                .replace("authors = [\"Darkslide example\"]\n", "")
                .replace(
                    "summary = \"A plain border around each exported photo, like a print's margin.\"",
                    &format!("summary = \"{name}: an export step.\""),
                )
                .replace(
                    "name = \"Film Border\"\nmodule",
                    &format!("name = \"{name}\"\nmodule"),
                );
            vec![(MANIFEST_FILE, toml), ("border.wat", BORDER_WAT.to_owned())]
        }
        other => return Err(format!("unknown kind `{other}` (looks, post-process)")),
    };
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for (f, text) in files {
        fs::write(dir.join(f), text).map_err(|e| e.to_string())?;
    }
    Ok(format!(
        "created {}\nnext: edit it, then `ds-plugin validate \"{}\"`\n",
        dir.display(),
        dir.display()
    ))
}

fn validate(path: &Path) -> Result<String, String> {
    let pkg = Package::read(path).map_err(|e| e.to_string())?;
    let (m, signer) =
        inspect_package(&pkg, &darkslide_version(), true).map_err(|e| e.to_string())?;
    let mut out = format!("{} {} ({})\n", m.name, m.version, m.id);
    let _ = writeln!(
        out,
        "  {} files, {}",
        pkg.hashes().len(),
        signer.map_or_else(
            || "unsigned".to_owned(),
            |s| format!(
                "signed by key {} (claims to be {})",
                s.fingerprint(),
                s.name
            )
        )
    );
    for c in &m.contributions {
        let _ = write!(
            out,
            "  {} `{}` (contract {}): ",
            c.kind.key(),
            c.id,
            c.contract
        );
        let bytes = if c.kind.has_file() {
            pkg.get(&c.file)
                .ok_or_else(|| format!("`{}` is missing", c.file))?
        } else {
            &[]
        };
        match c.kind {
            Kind::Looks | Kind::Styles | Kind::VoicePhrases => {
                let builtin = m.id.starts_with(crate::manifest::BUILTIN_PREFIX);
                let n = pack::normalize(c.kind, &m.id, builtin, bytes)
                    .map_err(|e| format!("{}: {e}", c.file))?;
                let count = serde_json::from_str::<Value>(&n.json).map_or(0, |v| {
                    v.as_array()
                        .map_or_else(|| v["phrases"].as_object().map_or(0, Map::len), Vec::len)
                });
                let _ = writeln!(out, "{count} items OK");
            }
            Kind::ExportPostProcess => {
                let module = WasmModule::compile(bytes).map_err(|e| e.to_string())?;
                let pp = PostProcessor::new(
                    &c.name,
                    module,
                    c.entry.as_deref().unwrap_or("ds_post_process"),
                    c.apron_rows,
                )
                .map_err(|e| e.to_string())?;
                let defaults = crate::params::resolve(&c.params, &Map::new())?;
                let ctx = json!({"output": {"space": "srgb"}});
                let l = pp
                    .layout(64, 64, &defaults, &ctx)
                    .map_err(|e| e.to_string())?;
                let photo = [l.x, l.y, 64, 64];
                let n = (l.width * l.height) as usize;
                let mut a = vec![128u8; n * 4];
                let mut b = vec![32768u16; n * 3];
                for px in [Pixels::Rgba8(&mut a), Pixels::Rgb16(&mut b)] {
                    pp.run(
                        px,
                        l.width,
                        l.height,
                        photo,
                        defaults.clone(),
                        &ctx,
                        &|| false,
                    )
                    .map_err(|e| e.to_string())?;
                }
                let changed = a.iter().filter(|&&v| v != 128).count();
                let _ = writeln!(
                    out,
                    "runs on 64×64 RGBA8 and RGB16 with its defaults (canvas {}×{}, {changed} bytes changed)",
                    l.width, l.height
                );
            }
            Kind::ExportDestination => {
                let _ = writeln!(
                    out,
                    "contract checked (destinations don't run in this version yet)"
                );
            }
            Kind::NativeFeature => {
                let _ = writeln!(out, "switches the app's own {} feature", c.id);
            }
        }
    }
    Ok(out)
}

/// A clean copy for sharing: no dotfiles, `.wat` modules compiled to `.wasm` and the
/// manifest pointed at them, as a folder in `--out` or a `.dsplugin` zip with `--zip`.
/// Validated before and after.
fn pack_folder(
    dir: &Path,
    out_parent: Option<&Path>,
    zip: Option<&Path>,
) -> Result<String, String> {
    let mut pkg = Package::read_dir(dir).map_err(|e| e.to_string())?;
    let (m, _) = inspect_package(&pkg, &darkslide_version(), true).map_err(|e| e.to_string())?;
    // The manifest is rewritten as data (its comments go; the packed copy is generated).
    let mut manifest: toml::Table = std::str::from_utf8(pkg.get(MANIFEST_FILE).unwrap_or_default())
        .map_err(|e| e.to_string())
        .and_then(|t| toml::from_str(t).map_err(|e| e.to_string()))?;
    let wats: Vec<String> = m
        .contributions
        .iter()
        .map(|c| c.file.clone())
        .filter(|f| crate::manifest::has_ext(f, "wat"))
        .collect();
    for wat in &wats {
        let wasm = wat::parse_bytes(pkg.get(wat).unwrap_or_default())
            .map_err(|e| e.to_string())?
            .into_owned();
        let target = format!("{}.wasm", &wat[..wat.len() - 4]);
        pkg.remove(wat);
        pkg.put(&target, wasm).map_err(|e| e.to_string())?;
        let kinds = manifest
            .get_mut("contributes")
            .and_then(toml::Value::as_table_mut);
        for list in kinds
            .into_iter()
            .flat_map(|t| t.iter_mut().map(|(_, v)| v))
            .filter_map(toml::Value::as_array_mut)
        {
            for c in list.iter_mut().filter_map(toml::Value::as_table_mut) {
                if c.get("module").and_then(toml::Value::as_str) == Some(wat.as_str()) {
                    c.insert("module".into(), toml::Value::String(target.clone()));
                }
            }
        }
    }
    let manifest = toml::to_string(&manifest).map_err(|e| e.to_string())?;
    pkg.put(MANIFEST_FILE, manifest.into_bytes())
        .map_err(|e| e.to_string())?;
    pkg.remove(SIGNATURE_FILE); // the files changed: sign the packed copy again
    inspect_package(&pkg, &darkslide_version(), true)
        .map_err(|e| format!("the packed copy doesn't validate: {e}"))?;
    let dest = match (out_parent, zip) {
        (Some(parent), None) => {
            let dest = parent.join(dir.file_name().ok_or("not a folder")?);
            if dest.exists() {
                return Err(format!("{} already exists", dest.display()));
            }
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            pkg.write_to(&dest).map_err(|e| e.to_string())?;
            dest
        }
        (None, Some(zip)) => {
            if zip.exists() {
                return Err(format!("{} already exists", zip.display()));
            }
            ds_fsutil::write_atomic(zip, &pkg.to_zip().map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            zip.to_path_buf()
        }
        _ => return Err(USAGE.to_owned()),
    };
    Ok(format!("packed {} into {}\n", m.name, dest.display()))
}

fn keygen(signer: &str, out: &Path) -> Result<String, String> {
    if out.exists() {
        return Err(format!(
            "{} already exists; keys are never overwritten",
            out.display()
        ));
    }
    let key = trust::keygen(signer).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&key).map_err(|e| e.to_string())?;
    write_private(out, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!(
        "wrote {} (keep it private)\npublic key: {}\n",
        out.display(),
        key.key
    ))
}

/// Write a secret: created owner-only (0600) under a temp name, fsync'd, then renamed
/// into place without replacing anything.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = ds_fsutil::temp_path(dir, "key");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    ds_fsutil::sync_file(&f)?;
    ds_fsutil::rename_noreplace(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

fn sign(dir: &Path, key: &Path) -> Result<String, String> {
    let key: KeyFile = serde_json::from_str(&fs::read_to_string(key).map_err(|e| e.to_string())?)
        .map_err(|e| format!("{}: {e}", key.display()))?;
    // Sign exactly the bytes that were validated.
    let pkg = Package::read_dir(dir).map_err(|e| e.to_string())?;
    inspect_package(&pkg, &darkslide_version(), true).map_err(|e| e.to_string())?;
    let files = pkg.hashes();
    let sig = trust::sign(&files, &key).map_err(|e| e.to_string())?;
    let path: PathBuf = dir.join(SIGNATURE_FILE);
    let text = serde_json::to_string_pretty(&sig).map_err(|e| e.to_string())?;
    ds_fsutil::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(format!(
        "signed {} files as key {}\n",
        files.len(),
        trust::fingerprint(&key.key)
    ))
}

fn contracts() -> String {
    let mut out = format!("manifest format {MANIFEST_VERSION}, host API {HOST_API}\n");
    for (k, lo, hi) in CONTRACTS {
        let _ = writeln!(
            out,
            "  {:<20} contract {lo}–{hi}{}",
            k.key(),
            if k.is_code() { " (code)" } else { "" }
        );
    }
    out
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
