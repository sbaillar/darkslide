//! The whole folder: synthetic fixtures (`fixtures/settings/`) copied to a
//! temporary folder, plus what can't live in git (empty folders, hidden files).
#![allow(clippy::float_cmp, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use lt_core::AdjustGroup;
use plist::{Dictionary, Value};

use super::*;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/settings")
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            fs::copy(e.path(), dest).unwrap();
        }
    }
}

/// Every file under `dir` (recursively, hidden ones too).
fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(all_files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// Every file under `dir` with its BLAKE3.
fn hashes(dir: &Path) -> BTreeMap<PathBuf, String> {
    all_files(dir)
        .into_iter()
        .map(|p| {
            let h = blake3::hash(&fs::read(&p).unwrap()).to_hex().to_string();
            (p, h)
        })
        .collect()
}

/// The fixtures plus an empty preset folder, reported-only folders, a hidden
/// file, and preferences beside the folder.
fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("Capture One");
    copy_tree(&fixtures(), &folder);
    fs::create_dir_all(folder.join("Presets60/Camera Focus")).unwrap();
    fs::create_dir_all(folder.join("Search Presets")).unwrap();
    fs::create_dir_all(folder.join("StyleBrushes")).unwrap();
    fs::write(folder.join("StyleBrushes/Dodge.costylebrush"), "<SL/>").unwrap();
    fs::write(folder.join("Recipes143/.DS_Store"), "x").unwrap();
    let mut d = Dictionary::new();
    d.insert(
        "enabledRecipeNames".into(),
        Value::Array(vec![Value::String("TIFF - A3 Print (300 dpi)".into())]),
    );
    d.insert(
        "proofProfile143".into(),
        Value::String("1;Adobe RGB (1998).icm".into()),
    );
    let prefs = tmp.path().join("com.captureone.captureone16.plist");
    Value::Dictionary(d).to_file_binary(&prefs).unwrap();
    (tmp, folder, prefs)
}

#[test]
fn reads_every_category_without_writing() {
    let (_tmp, folder, prefs) = setup();
    let before = hashes(folder.parent().unwrap());
    let s = read_app_settings(&folder, Some(&prefs)).unwrap();
    assert_eq!(
        hashes(folder.parent().unwrap()),
        before,
        "nothing written or created"
    );

    // Recipes143 wins over Recipes100; C1's order; the broken one is skipped.
    let names: Vec<(&str, bool)> = s
        .recipes
        .iter()
        .map(|r| (r.name.as_str(), r.enabled))
        .collect();
    assert_eq!(
        names,
        [
            ("JPEG - 2048px for web use", false),
            ("TIFF - A3 Print (300 dpi)", true),
            ("PSD - Logo", false)
        ]
    );
    let web = &s.recipes[0];
    assert_eq!(web.size, RecipeSize::LongEdge(2048));
    assert_eq!(web.subfolder, "Web");
    assert!(!web.include_metadata && web.strip_gps);
    let a3 = &s.recipes[1];
    assert_eq!(a3.format, RecipeFormat::Tiff16);
    assert_eq!(a3.size, RecipeSize::LongEdge(4961));
    assert_eq!(
        a3.space,
        RecipeOutputSpace::Profile("Adobe RGB (1998)".into())
    );
    assert_eq!(
        a3.destination.as_deref(),
        Some("/tmp/Luxtable Fixture Exports")
    );
    let psd = &s.recipes[2];
    assert_eq!(psd.space, RecipeOutputSpace::DisplayP3);
    assert_eq!(psd.watermark.as_ref().unwrap().anchor, "bottomRight");

    let w = &s.workspaces[0];
    assert_eq!(w.name, "Studio (Capture One)");
    let tabs: Vec<Option<&str>> = w.tabs.iter().map(|t| t.builtin).collect();
    assert_eq!(
        tabs,
        [
            Some("library"),
            Some("favorites"),
            Some("exposure"),
            None,
            Some("details"),
            Some("metadata")
        ]
    );
    assert_eq!(
        w.tabs[0].panels,
        ["smartFolders", "sessions", "albums", "filters"]
    );
    assert_eq!(
        w.tabs[2].panels,
        ["exposure", "hdr", "levels", "curve", "clarity", "vignette"]
    );
    assert_eq!(w.tabs[4].panels, ["sharpening", "noise", "effects"]);
    assert_eq!(w.collapsed, ["levels"]);
    assert!(w.unknown_tools.contains(&"Details: Moire".to_owned()));

    let ratios: Vec<(&str, Option<&str>)> = s
        .aspect_ratios
        .iter()
        .map(|a| (a.name.as_str(), a.builtin))
        .collect();
    assert_eq!(ratios, [("Cinema", None), ("Square", Some("1:1"))]);

    let styles: Vec<(&str, StyleKind, Option<&str>)> = s
        .styles
        .iter()
        .map(|st| (st.name.as_str(), st.kind, st.category.as_deref()))
        .collect();
    assert_eq!(
        styles,
        [
            ("Fade", StyleKind::Style, None),
            ("Soft Skin", StyleKind::Style, Some("Portrait")),
            ("Brighter", StyleKind::Preset, Some("Exposure")),
        ]
    );
    assert_eq!(s.styles[0].file, "Styles/Fade.costyle");
    assert_eq!(s.styles[2].groups, [AdjustGroup::Exposure]);
    assert_eq!(s.styles[2].adjustments.exposure.exposure_ev, 0.5);

    assert_eq!(
        s.keyword_libraries[0].keywords,
        ["Places", "Coast", "Harbour", "beach", "sunset"]
    );
    let p = s.prefs.as_ref().unwrap();
    assert_eq!(p.proof_profile.as_deref(), Some("Adobe RGB (1998)"));

    let folders: Vec<(&str, usize)> = s
        .folders
        .iter()
        .map(|f| (f.folder.as_str(), f.files))
        .collect();
    assert_eq!(
        folders,
        [
            ("Recipes143", 4),
            ("Workspaces", 1),
            ("Aspect Ratios 8.2", 1),
            ("Styles", 3),
            ("Presets60", 1),
            ("Keywords", 1),
            ("StyleBrushes", 1),
            ("Search Presets", 0),
        ]
    );
    let skipped: Vec<(&str, &str)> = s
        .skipped
        .iter()
        .map(|k| (k.file.as_str(), k.reason.as_str()))
        .collect();
    assert_eq!(skipped.len(), 4, "{skipped:?}");
    assert_eq!(skipped[0].0, "Recipes143/Broken.xml");
    assert!(skipped[0].1.contains("No output format"));
    assert_eq!(skipped[1].0, "Aspect Ratios 8.2/User Aspect Ratios.xml");
    assert!(skipped[1].1.contains("Entry 3"));
    assert_eq!(skipped[2].0, "Styles/Pack.costylepack");
    assert!(skipped[2].1.contains("style pack"));
    assert_eq!(skipped[3].0, "StyleBrushes/Dodge.costylebrush");
}

#[test]
fn files_that_cant_be_read_are_skipped_with_a_reason() {
    let (_tmp, folder, _) = setup();
    fs::write(folder.join("Workspaces/Bad.plist"), "not a plist").unwrap();
    fs::write(folder.join("Styles/Bad.costyle"), "<OR/>").unwrap();
    fs::write(folder.join("Styles/notes.txt"), "x").unwrap();
    fs::write(folder.join("Keywords/empty.txt"), "").unwrap();
    fs::write(folder.join("Aspect Ratios 8.2/Other.xml"), "junk").unwrap();
    let missing = folder.join("missing.plist");
    let s = read_app_settings(&folder, Some(&missing)).unwrap();
    assert!(s.prefs.is_none());
    let files: Vec<&str> = s.skipped.iter().map(|k| k.file.as_str()).collect();
    for f in [
        "missing.plist",
        "Workspaces/Bad.plist",
        "Styles/Bad.costyle",
        "Styles/notes.txt",
        "Keywords/empty.txt",
        "Aspect Ratios 8.2/Other.xml",
    ] {
        assert!(files.contains(&f), "{f} not in {files:?}");
    }
    // Without preferences, no recipe is ticked.
    let s = read_app_settings(&folder, None).unwrap();
    assert!(s.recipes.iter().all(|r| !r.enabled));
}

#[test]
fn an_empty_or_missing_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let s = read_app_settings(tmp.path(), None).unwrap();
    assert_eq!(s, C1AppSettings::default());
    assert!(read_app_settings(&tmp.path().join("nope"), None).is_err());
}

#[test]
fn large_files_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("big.xml");
    let f = fs::File::create(&p).unwrap();
    f.set_len(MAX_SETTINGS_FILE_BYTES + 1).unwrap();
    assert!(read_bytes(&p).unwrap_err().contains("Too large"));
    fs::write(&p, [0xff, 0xfe]).unwrap();
    assert!(read_text(&p).unwrap_err().contains("UTF-8"));
}

#[test]
fn newest_folder_of_a_family() {
    let e = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        newest(
            &e(&["Recipes100", "Recipes143", "RecipesX", "Styles"]),
            "Recipes"
        )
        .as_deref(),
        Some("Recipes143")
    );
    assert_eq!(
        newest(
            &e(&["Aspect Ratios 8.2", "Aspect Ratios 10"]),
            "Aspect Ratios"
        )
        .as_deref(),
        Some("Aspect Ratios 10")
    );
    assert_eq!(
        newest(&e(&["Presets"]), "Presets").as_deref(),
        Some("Presets")
    );
    assert_eq!(newest(&e(&["Other"]), "Presets"), None);
}

#[test]
fn unreadable_folders_are_skipped_and_the_rest_still_reads() {
    use std::os::unix::fs::PermissionsExt;
    let (_tmp, folder, _) = setup();
    fs::remove_dir_all(folder.join("Workspaces")).unwrap();
    fs::write(folder.join("Workspaces"), "a file, not a folder").unwrap();
    let locked = folder.join("Styles/Locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("x.costyle"), "<SL/>").unwrap();
    let keywords = folder.join("Keywords");
    for d in [&locked, &keywords] {
        fs::set_permissions(d, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let s = read_app_settings(&folder, None);
    for d in [&locked, &keywords] {
        fs::set_permissions(d, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let s = s.unwrap();
    assert_eq!(s.recipes.len(), 3);
    assert_eq!(s.styles.len(), 3, "the readable styles still import");
    assert!(s.workspaces.is_empty() && s.keyword_libraries.is_empty());
    let skipped: Vec<&str> = s.skipped.iter().map(|k| k.file.as_str()).collect();
    for f in ["Workspaces", "Styles/Locked", "Keywords"] {
        let k = s.skipped.iter().find(|k| k.file == f);
        assert!(
            k.is_some_and(|k| k.reason.starts_with("Folder couldn't be read")),
            "{f}: {skipped:?}"
        );
    }
    let folders: Vec<&str> = s.folders.iter().map(|f| f.folder.as_str()).collect();
    assert!(!folders.contains(&"Workspaces") && !folders.contains(&"Keywords"));
}

/// The review's probes: files under the size cap whose nesting would overflow the
/// stack (an abort) where they're parsed or dropped. Read on a 2 MiB thread, the
/// size of the FFI's job threads; each is skipped with a reason.
#[test]
fn deeply_nested_files_are_refused_not_crashed_on() {
    use super::limits::tests::nested_binary_plist;
    use crate::xml::tests::nested as nested_xml;
    let (_tmp, folder, _) = setup();
    let recipe = format!(
        "<?xml version=\"1.0\"?>\n<OR>{}</OR>",
        nested_xml(500_000, "E")
    );
    fs::write(folder.join("Recipes143/Deep.xml"), recipe).unwrap();
    let ws = format!(
        "<?xml version=\"1.0\"?><plist version=\"1.0\">{}</plist>",
        nested_xml(200_000, "array")
    );
    fs::write(folder.join("Workspaces/Deep.plist"), ws).unwrap();
    let prefs = folder.parent().unwrap().join("deep.plist");
    fs::write(&prefs, nested_binary_plist(300_000)).unwrap();
    for f in [folder.join("Recipes143/Deep.xml"), prefs.clone()] {
        assert!(fs::metadata(&f).unwrap().len() < MAX_SETTINGS_FILE_BYTES);
    }
    let s = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || read_app_settings(&folder, Some(&prefs)).unwrap())
        .unwrap()
        .join()
        .expect("no stack overflow");
    let reasons: BTreeMap<&str, &str> = s
        .skipped
        .iter()
        .map(|k| (k.file.as_str(), k.reason.as_str()))
        .collect();
    for f in ["Recipes143/Deep.xml", "Workspaces/Deep.plist", "deep.plist"] {
        assert!(
            reasons.get(f).is_some_and(|r| r.contains("Nested")),
            "{f}: {reasons:?}"
        );
    }
    assert_eq!(s.recipes.len(), 3, "the rest still reads");
}

/// The review's round-3 probes: nesting hidden from a hand-written scanner
/// behind a `>` in CDATA or a processing instruction.
#[test]
fn nesting_behind_cdata_or_processing_instructions_is_refused() {
    let (_tmp, folder, _) = setup();
    for (name, unit) in [("Cdata", "<a><![CDATA[></z>]]>"), ("Pi", "<a><?p ></z>?>")] {
        let xml = format!("<?xml version=\"1.0\"?>\n<OR>{}</OR>", unit.repeat(300_000));
        assert!(xml.len() < MAX_SETTINGS_FILE_BYTES as usize);
        fs::write(folder.join(format!("Recipes143/{name}.xml")), xml).unwrap();
    }
    let s = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || read_app_settings(&folder, None).unwrap())
        .unwrap()
        .join()
        .expect("no stack overflow");
    for f in ["Recipes143/Cdata.xml", "Recipes143/Pi.xml"] {
        let k = s.skipped.iter().find(|k| k.file == f);
        assert!(k.is_some_and(|k| k.reason.contains("Nested")), "{f}");
    }
    assert_eq!(s.recipes.len(), 3);
    assert_eq!(s.workspaces.len(), 1);
}

#[test]
fn a_cancelled_read_stops() {
    let (_tmp, folder, _) = setup();
    let stop: Cancel = std::sync::Arc::new(|| true);
    assert!(matches!(
        read_app_settings_cancellable(&folder, None, &stop),
        Err(crate::C1Error::Cancelled)
    ));
}
