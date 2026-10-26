//! Lightroom and Camera Raw presets: `.xmp` presets (settings as `crs:`
//! attributes, a name and group as `rdf:Alt`) and the older `.lrtemplate`
//! files (a Lua table with the settings under `value.settings`).

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use crate::error::{IoAt, LrError, Result};
use crate::limits::{MAX_LUA_BYTES, MAX_XMP_BYTES};
use crate::lua::parse_lua_table;
use crate::model::{Develop, LrPreset, Setting};
use crate::xmp::parse_xmp;

/// Most preset files read from a folder.
const MAX_PRESET_FILES: usize = 10_000;

/// Read the preset at `path`, or every preset file (`.xmp`, `.lrtemplate`)
/// under the folder `path`. A file that isn't a readable preset is an error
/// when it was asked for by name and skipped inside a folder.
pub fn read_presets(path: &Path, cancel: &dyn Fn() -> bool) -> Result<Vec<LrPreset>> {
    let meta = fs::metadata(path).at(path)?;
    if !meta.is_dir() {
        return read_one(path);
    }
    let mut out = Vec::new();
    let mut files = 0usize;
    let mut stack = vec![(path.to_owned(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if cancel() {
            return Err(LrError::Cancelled);
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for e in entries {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let p = e.path();
            if ft.is_dir() {
                if depth < lt_session::MAX_EXTERNAL_DEPTH {
                    stack.push((p, depth + 1));
                }
            } else if ft.is_file() && is_preset_name(&p) {
                files += 1;
                if files > MAX_PRESET_FILES {
                    return Ok(out);
                }
                if let Ok(mut v) = read_one(&p) {
                    out.append(&mut v);
                }
            }
        }
    }
    out.sort_by(|a, b| (&a.group, &a.name).cmp(&(&b.group, &b.name)));
    Ok(out)
}

fn is_preset_name(p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.starts_with('.') {
        return false;
    }
    matches!(
        p.extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .as_deref(),
        Some("xmp" | "lrtemplate")
    )
}

fn read_bounded(path: &Path, limit: u64) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .at(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .at(path)?;
    let bad = |message: String| LrError::Preset {
        path: path.to_owned(),
        message,
    };
    if bytes.len() as u64 > limit {
        return Err(bad(format!("More than {limit} bytes: not read")));
    }
    String::from_utf8(bytes).map_err(|_| bad("not UTF-8 text".into()))
}

fn read_one(path: &Path) -> Result<Vec<LrPreset>> {
    let bad = |message: String| LrError::Preset {
        path: path.to_owned(),
        message,
    };
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let is_template = path
        .extension()
        .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("lrtemplate"));
    if is_template {
        let text = read_bounded(path, MAX_LUA_BYTES as u64)?;
        let root = parse_lua_table(&text).map_err(bad)?;
        let t = root.table().ok_or_else(|| bad("not a preset".into()))?;
        let value = t.get("value").and_then(Setting::table);
        let settings = value
            .and_then(|v| v.get("settings"))
            .and_then(Setting::table)
            .ok_or_else(|| bad("no develop settings".into()))?;
        let name = ["title", "internalName"]
            .iter()
            .filter_map(|k| t.get(*k).and_then(Setting::text))
            .find(|n| !n.trim().is_empty())
            .map_or(stem, |n| n.trim().to_owned());
        return Ok(vec![LrPreset {
            name,
            group: value
                .and_then(|v| v.get("group"))
                .and_then(Setting::text)
                .map(str::to_owned),
            uuid: t.get("id").and_then(Setting::text).map(str::to_owned),
            develop: settings.clone(),
        }]);
    }
    let text = read_bounded(path, MAX_XMP_BYTES)?;
    let x = parse_xmp(&text).map_err(bad)?;
    if x.develop.is_empty() {
        return Err(bad("no Camera Raw settings".into()));
    }
    let develop: Develop = x.develop;
    Ok(vec![LrPreset {
        name: x
            .preset_name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(stem),
        group: x.preset_group.filter(|g| !g.trim().is_empty()),
        uuid: x.preset_uuid,
        develop,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no() -> bool {
        false
    }

    const XMP_PRESET: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
      xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
     <rdf:RDF><rdf:Description crs:PresetType="Normal" crs:UUID="U-1" crs:Contrast2012="+25" crs:Vibrance="+10">
      <crs:Name><rdf:Alt><rdf:li xml:lang="x-default">Punchy</rdf:li></rdf:Alt></crs:Name>
      <crs:Group><rdf:Alt><rdf:li xml:lang="x-default">Mine</rdf:li></rdf:Alt></crs:Group>
     </rdf:Description></rdf:RDF></x:xmpmeta>"#;

    const TEMPLATE: &str = "s = {\n\tid = \"T-1\",\n\tinternalName = \"Old Style\",\n\ttitle = \"Old\",\n\
        \tvalue = {\n\t\tgroup = \"Legacy\",\n\t\tsettings = {\n\t\t\tExposure2012 = 0.25,\n\t\t\tToneCurvePV2012 = { 0, 0, 128, 140, 255, 255 },\n\t\t},\n\t},\n\tversion = 0,\n}\n";

    #[test]
    fn reads_xmp_and_lrtemplate_presets() {
        let d = tempfile::tempdir().unwrap();
        let x = d.path().join("Punchy.xmp");
        fs::write(&x, XMP_PRESET).unwrap();
        let v = read_presets(&x, &no).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Punchy");
        assert_eq!(v[0].group.as_deref(), Some("Mine"));
        assert_eq!(v[0].uuid.as_deref(), Some("U-1"));
        assert_eq!(
            v[0].develop.keys().collect::<Vec<_>>(),
            ["Contrast2012", "Vibrance"]
        );
        let t = d.path().join("Old.lrtemplate");
        fs::write(&t, TEMPLATE).unwrap();
        let v = read_presets(&t, &no).unwrap();
        assert_eq!(v[0].name, "Old");
        assert_eq!(v[0].group.as_deref(), Some("Legacy"));
        assert_eq!(v[0].uuid.as_deref(), Some("T-1"));
        assert_eq!(v[0].develop["Exposure2012"], Setting::Num(0.25));
    }

    #[test]
    fn names_fall_back_to_the_file_name() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("Plain.xmp"),
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
              xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"><rdf:RDF><rdf:Description crs:Exposure2012="+1"/></rdf:RDF></x:xmpmeta>"#,
        )
        .unwrap();
        fs::write(
            d.path().join("Bare.lrtemplate"),
            "s = { value = { settings = { A = 1 } } }",
        )
        .unwrap();
        let v = read_presets(d.path(), &no).unwrap();
        let names: Vec<_> = v.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Bare", "Plain"]);
        assert!(v.iter().all(|p| p.group.is_none() && p.uuid.is_none()));
    }

    #[test]
    fn folders_skip_bad_files_and_named_bad_files_are_errors() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("Group");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("Good.xmp"), XMP_PRESET).unwrap();
        fs::write(sub.join("Bad.xmp"), "<broken").unwrap();
        fs::write(
            sub.join("Empty.xmp"),
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>",
        )
        .unwrap();
        fs::write(
            sub.join("Evil.lrtemplate"),
            "s = { value = { settings = os.exit() } }",
        )
        .unwrap();
        fs::write(sub.join("NoSettings.lrtemplate"), "s = { value = {} }").unwrap();
        fs::write(sub.join("List.lrtemplate"), "s = { 1, 2 }").unwrap();
        fs::write(sub.join("Num.lrtemplate"), "s = 5").unwrap();
        fs::write(sub.join("notes.txt"), "x").unwrap();
        fs::write(sub.join("._Good.xmp"), "x").unwrap();
        std::os::unix::fs::symlink(d.path(), sub.join("loop")).unwrap();
        let v = read_presets(d.path(), &no).unwrap();
        assert_eq!(v.len(), 1);
        for bad in [
            "Bad.xmp",
            "Empty.xmp",
            "Evil.lrtemplate",
            "NoSettings.lrtemplate",
            "List.lrtemplate",
            "Num.lrtemplate",
        ] {
            assert!(
                matches!(
                    read_presets(&sub.join(bad), &no),
                    Err(LrError::Preset { .. })
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            read_presets(d.path(), &|| true),
            Err(LrError::Cancelled)
        ));
        assert!(read_presets(&d.path().join("missing.xmp"), &no).is_err());
    }

    #[test]
    fn hostile_preset_files_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let deep = d.path().join("Deep.lrtemplate");
        fs::write(
            &deep,
            format!("s = {}{}", "{".repeat(100_000), "}".repeat(100_000)),
        )
        .unwrap();
        assert!(read_presets(&deep, &no).is_err());
        let big = d.path().join("Big.lrtemplate");
        File::create(&big)
            .unwrap()
            .set_len(MAX_LUA_BYTES as u64 + 1)
            .unwrap();
        assert!(
            read_presets(&big, &no)
                .unwrap_err()
                .to_string()
                .contains("bytes")
        );
        let xml = d.path().join("Deep.xmp");
        fs::write(
            &xml,
            format!("<r>{}</r>", crate::limits::tests::nested(300, "a")),
        )
        .unwrap();
        assert!(
            read_presets(&xml, &no)
                .unwrap_err()
                .to_string()
                .contains("Nested")
        );
        let bin = d.path().join("Bin.xmp");
        fs::write(&bin, [0xff, 0xfe]).unwrap();
        assert!(
            read_presets(&bin, &no)
                .unwrap_err()
                .to_string()
                .contains("UTF-8")
        );
    }
}
