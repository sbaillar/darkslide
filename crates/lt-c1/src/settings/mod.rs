//! Capture One's *application* settings (`~/Library/Application Support/
//! Capture One/` and its preferences file), read-only, mapped onto what
//! Luxtable keeps per user: export recipes, a named tool workspace, crop
//! ratio presets, presets, a keyword list and a few preferences. See
//! `docs/C1_FORMAT.md` (§ Application settings) for the formats.
//!
//! Nothing here writes: [`read_app_settings`] returns what each file maps to
//! (and what doesn't), and the app applies it to its own stores, journaled.
//! Every file is opened read-only; a keyword library database is copied to a
//! temporary directory first, like a session database.

mod aspect;
mod keywords;
mod limits;
mod prefs;
mod recipe;
mod style;
mod tokens;
mod workspace;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use aspect::AspectRatioImport;
pub use keywords::KeywordLibraryImport;
pub use prefs::PrefsImport;
pub use recipe::{RecipeFormat, RecipeImport, RecipeOutputSpace, RecipeSize, RecipeWatermark};
pub use style::{StyleImport, StyleKind};
pub use workspace::{WorkspaceImport, WorkspaceTab};

use crate::error::{C1Error, IoAt, Result};

/// Settings files larger than this are not read (a recipe is ~5 KB, the
/// workspace ~40 KB).
pub const MAX_SETTINGS_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// A setting that maps only approximately, or not at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The Capture One key (or a short label for a group of keys).
    pub key: String,
    pub text: String,
    /// `false`: carried over approximately; `true`: not carried over.
    pub dropped: bool,
}

impl Note {
    pub(crate) fn approx(key: &str, text: impl Into<String>) -> Self {
        Self {
            key: key.to_owned(),
            text: text.into(),
            dropped: false,
        }
    }

    pub(crate) fn dropped(key: &str, text: impl Into<String>) -> Self {
        Self {
            key: key.to_owned(),
            text: text.into(),
            dropped: true,
        }
    }
}

/// A file that was found but not read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    /// Relative to the settings folder, `/`-separated.
    pub file: String,
    pub reason: String,
}

/// One settings category folder and how many files it holds (an empty
/// folder is reported as found with nothing in it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderCount {
    pub folder: String,
    pub files: usize,
}

/// Everything found in a Capture One settings folder (and its preferences).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct C1AppSettings {
    pub recipes: Vec<RecipeImport>,
    pub workspaces: Vec<WorkspaceImport>,
    pub aspect_ratios: Vec<AspectRatioImport>,
    pub styles: Vec<StyleImport>,
    pub keyword_libraries: Vec<KeywordLibraryImport>,
    pub prefs: Option<PrefsImport>,
    pub folders: Vec<FolderCount>,
    pub skipped: Vec<SkippedFile>,
}

/// Category folders, with the newest version suffix C1 used so far
/// (`Recipes143`, `Presets60`, `Aspect Ratios 8.2`). A higher-numbered folder
/// of the same family wins, as with `.cos` settings folders.
const RECIPES: &str = "Recipes";
const PRESETS: &str = "Presets";
const ASPECTS: &str = "Aspect Ratios";
/// Folders that hold user content Luxtable reports but doesn't map.
const REPORTED_ONLY: [(&str, &str); 4] = [
    (
        "StyleBrushes",
        "Style brushes are local adjustments: not imported",
    ),
    (
        "Smart Adjustments",
        "Smart adjustments: no Luxtable equivalent",
    ),
    (
        "Normalizations",
        "Normalize colour targets: no Luxtable equivalent",
    ),
    (
        "Search Presets",
        "Advanced search presets: not imported (Luxtable's smart folders are built in)",
    ),
];

/// Polled while reading: `true` stops the read ([`C1Error::Cancelled`]).
pub type Cancel = Arc<dyn Fn() -> bool + Send + Sync>;

/// Read a Capture One settings folder (and `prefs_file`, the
/// `com.captureone.captureone*.plist` preferences file, when given).
/// Unreadable files are skipped with a reason; only an unreadable folder is
/// an error.
pub fn read_app_settings(folder: &Path, prefs_file: Option<&Path>) -> Result<C1AppSettings> {
    let never: Cancel = Arc::new(|| false);
    read_app_settings_cancellable(folder, prefs_file, &never)
}

/// [`read_app_settings`], stopping when `cancel` says so (checked between
/// categories and inside a keyword database's query).
pub fn read_app_settings_cancellable(
    folder: &Path,
    prefs_file: Option<&Path>,
    cancel: &Cancel,
) -> Result<C1AppSettings> {
    let check = || {
        if cancel() {
            Err(C1Error::Cancelled)
        } else {
            Ok(())
        }
    };
    let mut out = C1AppSettings::default();
    let entries = list(folder)?;
    match prefs_file.map(prefs::read).transpose() {
        Ok(p) => out.prefs = p,
        Err(reason) => out.skipped.push(SkippedFile {
            file: prefs_file.map(file_name).unwrap_or_default(),
            reason,
        }),
    }
    let has = |name: &str| entries.iter().any(|e| e == name);
    if let Some(dir) = newest(&entries, RECIPES) {
        read_recipes(folder, &dir, &mut out);
    }
    check()?;
    if has("Workspaces") {
        read_workspaces(folder, &mut out);
    }
    check()?;
    if let Some(dir) = newest(&entries, ASPECTS) {
        read_aspect_ratios(folder, &dir, &mut out);
    }
    if has("Styles") {
        read_styles(folder, "Styles", StyleKind::Style, &mut out);
    }
    if let Some(dir) = newest(&entries, PRESETS) {
        read_styles(folder, &dir, StyleKind::Preset, &mut out);
    }
    check()?;
    if has("Keywords") {
        let files = found(folder, "Keywords", true, &[], &mut out);
        // A database's journal files are read with it, not as libraries.
        for f in files.iter().filter(|f| !keywords::is_db_sidecar(f)) {
            match keywords::read(f, cancel) {
                Ok(k) => out.keyword_libraries.push(k),
                Err(reason) => out.skip(folder, f, reason),
            }
            check()?;
        }
    }
    for (dir, reason) in REPORTED_ONLY {
        if has(dir) {
            for f in found(folder, dir, true, &[], &mut out) {
                out.skip(folder, &f, reason.into());
            }
        }
    }
    Ok(out)
}

impl C1AppSettings {
    fn skip(&mut self, root: &Path, file: &Path, reason: String) {
        self.skipped.push(SkippedFile {
            file: rel_path(root, file),
            reason,
        });
    }
}

/// The files of category folder `dir` (with one of `exts`, any when empty;
/// `recursive` descends into sub-folders), counted in `out.folders`. A folder
/// that can't be read is skipped with a reason; the other categories still read.
fn found(
    root: &Path,
    dir: &str,
    recursive: bool,
    exts: &[&str],
    out: &mut C1AppSettings,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.join(dir)];
    let mut readable = false;
    while let Some(d) = stack.pop() {
        let entries = match fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) => {
                out.skip(root, &d, format!("Folder couldn't be read: {e}"));
                continue;
            }
        };
        readable = true;
        for e in entries.filter_map(std::result::Result::ok) {
            let p = e.path();
            // Links are never followed: nothing outside the settings folder is read.
            let Ok(ft) = e.file_type() else { continue };
            if hidden(&p) {
                continue;
            }
            if ft.is_dir() && recursive {
                stack.push(p);
            } else if ft.is_file() && (exts.is_empty() || exts.contains(&extension(&p).as_str())) {
                files.push(p);
            }
        }
    }
    files.sort();
    if readable {
        out.folders.push(FolderCount {
            folder: dir.to_owned(),
            files: files.len(),
        });
    }
    files
}

fn read_recipes(root: &Path, dir: &str, out: &mut C1AppSettings) {
    let enabled = out
        .prefs
        .as_ref()
        .map(|p| p.enabled_recipes.clone())
        .unwrap_or_default();
    for f in found(root, dir, false, &["xml"], out) {
        match read_text(&f).and_then(|t| recipe::parse(&t, &file_stem(&f))) {
            Ok(mut r) => {
                r.enabled = enabled.contains(&r.name);
                out.recipes.push(r);
            }
            Err(reason) => out.skip(root, &f, reason),
        }
    }
    out.recipes
        .sort_by_key(|r| (r.sort_index.unwrap_or(u32::MAX), r.name.clone()));
}

fn read_workspaces(root: &Path, out: &mut C1AppSettings) {
    for f in found(root, "Workspaces", false, &["plist"], out) {
        match read_bytes(&f).and_then(|b| workspace::parse(&b, &file_stem(&f))) {
            Ok(w) => out.workspaces.push(w),
            Err(reason) => out.skip(root, &f, reason),
        }
    }
}

fn read_aspect_ratios(root: &Path, dir: &str, out: &mut C1AppSettings) {
    for f in found(root, dir, false, &["xml", "plist"], out) {
        match read_bytes(&f).and_then(|b| aspect::parse(&b)) {
            Ok((list, bad)) => {
                out.aspect_ratios.extend(list);
                for reason in bad {
                    out.skip(root, &f, reason);
                }
            }
            Err(reason) => out.skip(root, &f, reason),
        }
    }
}

fn read_styles(root: &Path, dir: &str, kind: StyleKind, out: &mut C1AppSettings) {
    for f in found(root, dir, true, &[], out) {
        let ext = extension(&f);
        if !matches!(ext.as_str(), "costyle" | "copreset") {
            let reason = if ext == "costylepack" {
                "A style pack: install it in Capture One, then import its styles"
            } else {
                "Not a style or preset file"
            };
            out.skip(root, &f, reason.into());
            continue;
        }
        // Presets60/<tool>/<name>.copreset: the tool folder is the category.
        let category = f.parent().filter(|p| *p != root.join(dir)).map(file_name);
        let rel = rel_path(root, &f);
        match read_text(&f).and_then(|t| style::parse(&t, &rel, &file_stem(&f), kind, category)) {
            Ok(s) => out.styles.push(s),
            Err(reason) => out.skip(root, &f, reason),
        }
    }
}

/// Names in `folder` (an unreadable folder is an error: nothing could be read).
fn list(folder: &Path) -> Result<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .at(folder)?
        .filter_map(std::result::Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

/// The highest-versioned folder of a family (`Recipes143` over `Recipes100`;
/// `Aspect Ratios 8.2` over `Aspect Ratios 7`).
fn newest(entries: &[String], family: &str) -> Option<String> {
    entries
        .iter()
        .filter_map(|e| {
            let rest = e.strip_prefix(family)?.trim();
            let version: Vec<u32> = if rest.is_empty() {
                Vec::new()
            } else {
                rest.split('.')
                    .map(|p| p.parse().ok())
                    .collect::<Option<_>>()?
            };
            Some((version, e))
        })
        .max()
        .map(|(_, e)| e.clone())
}

fn hidden(p: &Path) -> bool {
    p.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn extension(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn file_stem(p: &Path) -> String {
    p.file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The file's bytes, refusing files over [`MAX_SETTINGS_FILE_BYTES`].
pub(crate) fn read_bytes(p: &Path) -> Result<Vec<u8>, String> {
    let len = fs::metadata(p).map_err(|e| e.to_string())?.len();
    if len > MAX_SETTINGS_FILE_BYTES {
        return Err(format!("Too large to read ({len} bytes)"));
    }
    fs::read(p).map_err(|e| e.to_string())
}

/// UTF-8 text (a byte-order mark is dropped).
pub(crate) fn read_text(p: &Path) -> Result<String, String> {
    let b = read_bytes(p)?;
    let s = String::from_utf8(b).map_err(|_| "Not UTF-8 text".to_owned())?;
    Ok(s.strip_prefix('\u{feff}').unwrap_or(&s).to_owned())
}

/// A short content hash: re-imports compare it to tell whether C1's file
/// changed since the last import.
pub(crate) fn fingerprint(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_owned()
}

/// `<E K=".." V=".."/>` entries of a flat settings document whose root is
/// one of `roots` (`OR` for recipes, `SL` for styles and presets).
pub(crate) fn flat_entries(xml: &str, roots: &[&str]) -> Result<Vec<(String, String)>, String> {
    let xml = xml.trim_start_matches('\u{feff}');
    crate::xml::check_depth(xml)?;
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("Invalid XML: {e}"))?;
    let root = doc.root_element();
    if !roots.contains(&root.tag_name().name()) {
        return Err(format!(
            "Root element is <{}>, not <{}>",
            root.tag_name().name(),
            roots.join("> or <")
        ));
    }
    Ok(root
        .children()
        .filter(|n| n.has_tag_name("E"))
        .filter_map(|n| Some((n.attribute("K")?.to_owned(), n.attribute("V")?.to_owned())))
        .collect())
}

#[cfg(test)]
mod tests;
