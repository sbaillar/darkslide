//! Capture One's preferences (`~/Library/Preferences/com.captureone.captureone<N>.plist`,
//! a binary property list). Only a short list is carried over; everything else
//! there is window state, recent sessions and per-session view settings.

use std::path::Path;

use plist::{Dictionary, Value};

use super::{Note, read_bytes};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrefsImport {
    /// Import naming (`ImportNamingFormatTokensForSession`) as a Luxtable
    /// rename template (`{original}`…), when any of it translates.
    pub import_naming: Option<String>,
    /// Recipes ticked for export (`enabledRecipeNames`).
    pub enabled_recipes: Vec<String>,
    /// The soft-proof profile (`proofProfile143`), by name ("Adobe RGB (1998)").
    pub proof_profile: Option<String>,
    /// The keyboard shortcut set (`KeyboardShortcutSet`).
    pub shortcut_set: Option<String>,
    pub notes: Vec<Note>,
}

/// Preferences that are seen in the file but not carried over, and why.
const NOT_CARRIED: [(&str, &str); 6] = [
    (
        "NewSessionLocation",
        "Where new sessions go: Luxtable's own Sessions folder is set in Settings",
    ),
    (
        "Workspace",
        "The active workspace: workspaces are imported from the Workspaces folder instead",
    ),
    (
        "PreviewSizeInPixels",
        "Preview size: Luxtable sizes previews itself",
    ),
    (
        "viewerBackgroundColor",
        "Viewer background: not carried over",
    ),
    ("BeforeAfterMode", "Before/after mode: not carried over"),
    (
        "CreateDefaultCatalogOnStartup",
        "Catalog startup: Luxtable has no catalogs",
    ),
];

pub(crate) fn read(path: &Path) -> Result<PrefsImport, String> {
    let bytes = read_bytes(path)?;
    let root = super::limits::plist(&bytes).map_err(|e| format!("Invalid preferences: {e}"))?;
    let d = root
        .as_dictionary()
        .ok_or("Invalid preferences: not a dictionary")?;
    Ok(parse(d))
}

fn parse(d: &Dictionary) -> PrefsImport {
    let mut out = PrefsImport::default();
    let string = |k: &str| {
        d.get(k)
            .and_then(Value::as_string)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };

    if let Some(tokens) = d
        .get("ImportNamingFormatTokensForSession")
        .and_then(Value::as_array)
    {
        let mut template = String::new();
        let mut dropped = Vec::new();
        for t in tokens.iter().filter_map(Value::as_dictionary) {
            let name = t.get("name").and_then(Value::as_string).unwrap_or_default();
            match t.get("tag").and_then(Value::as_signed_integer) {
                Some(1) => template.push_str("{original}"),
                Some(_) => dropped.push(if name.is_empty() { "a token" } else { name }),
                None => template.push_str(name),
            }
        }
        if !dropped.is_empty() {
            out.notes.push(Note::dropped(
                "ImportNamingFormatTokensForSession",
                format!(
                    "Import naming: {} left out (no Luxtable token)",
                    dropped.join(", ")
                ),
            ));
        }
        if !template.trim().is_empty() {
            out.import_naming = Some(template);
        }
    }

    out.enabled_recipes = d
        .get("enabledRecipeNames")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_string)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    // "1;Adobe RGB (1998).icm": the part after `;` is the profile's file name.
    let proof = d
        .iter()
        .filter(|(k, _)| k.starts_with("proofProfile"))
        .max_by_key(|(k, _)| {
            k.trim_start_matches("proofProfile")
                .parse::<u32>()
                .unwrap_or(0)
        })
        .and_then(|(_, v)| v.as_string());
    if let Some(p) = proof {
        let file = p.split_once(';').map_or(p, |(_, f)| f).trim();
        let lower = file.to_ascii_lowercase();
        let name = [".icm", ".icc"]
            .iter()
            .find_map(|e| lower.ends_with(e).then(|| &file[..file.len() - e.len()]))
            .unwrap_or(file)
            .trim();
        if !name.is_empty() {
            out.proof_profile = Some(name.to_owned());
        }
    }

    out.shortcut_set = string("KeyboardShortcutSet").map(str::to_owned);
    match out.shortcut_set.as_deref() {
        Some("Default") => out.notes.push(Note::approx(
            "KeyboardShortcutSet",
            "Keyboard shortcuts: Capture One's default set; its rating keys (0–5) are Luxtable's defaults too",
        )),
        Some(other) => out.notes.push(Note::dropped(
            "KeyboardShortcutSet",
            format!("Keyboard shortcuts: the custom set \u{201c}{other}\u{201d} isn't read; set Luxtable's keys in Settings"),
        )),
        None => {}
    }

    for (key, text) in NOT_CARRIED {
        if d.contains_key(key) {
            out.notes.push(Note::dropped(key, text));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(name: &str, tag: Option<i64>) -> Value {
        let mut t = Dictionary::new();
        t.insert("name".into(), Value::String(name.into()));
        if let Some(tag) = tag {
            t.insert("tag".into(), Value::Integer(tag.into()));
        }
        Value::Dictionary(t)
    }

    pub(crate) fn sample() -> Dictionary {
        let mut d = Dictionary::new();
        d.insert(
            "ImportNamingFormatTokensForSession".into(),
            Value::Array(vec![
                token("Image Name", Some(1)),
                token("_", None),
                token("Camera Counter", Some(11)),
            ]),
        );
        d.insert(
            "enabledRecipeNames".into(),
            Value::Array(vec![Value::String("JPEG - Full size".into())]),
        );
        d.insert(
            "proofProfile143".into(),
            Value::String("1;Adobe RGB (1998).icm".into()),
        );
        d.insert("proofProfile100".into(), Value::String("1;Old.icc".into()));
        d.insert(
            "KeyboardShortcutSet".into(),
            Value::String("Default".into()),
        );
        d.insert("Workspace".into(), Value::String("Studio".into()));
        d.insert("PreviewSizeInPixels".into(), Value::Integer(5120.into()));
        d
    }

    #[test]
    fn carries_the_short_list() {
        let p = parse(&sample());
        assert_eq!(p.import_naming.as_deref(), Some("{original}_"));
        assert_eq!(p.enabled_recipes, ["JPEG - Full size"]);
        assert_eq!(p.proof_profile.as_deref(), Some("Adobe RGB (1998)"));
        assert_eq!(p.shortcut_set.as_deref(), Some("Default"));
        let keys: Vec<&str> = p.notes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "ImportNamingFormatTokensForSession",
                "KeyboardShortcutSet",
                "Workspace",
                "PreviewSizeInPixels"
            ]
        );
        assert!(p.notes[0].text.contains("Camera Counter"));
        assert!(!p.notes[1].dropped);
    }

    #[test]
    fn custom_shortcuts_and_untranslatable_naming() {
        let mut d = Dictionary::new();
        d.insert(
            "ImportNamingFormatTokensForSession".into(),
            Value::Array(vec![token("", Some(12))]),
        );
        d.insert("KeyboardShortcutSet".into(), Value::String("Mine".into()));
        d.insert("proofProfile143".into(), Value::String("Display".into()));
        let p = parse(&d);
        assert_eq!(p.import_naming, None);
        assert!(p.notes[0].text.contains("a token"));
        assert!(p.notes[1].dropped && p.notes[1].text.contains("Mine"));
        assert_eq!(p.proof_profile.as_deref(), Some("Display"));
        assert_eq!(parse(&Dictionary::new()), PrefsImport::default());
    }

    #[test]
    fn reads_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prefs.plist");
        Value::Dictionary(sample()).to_file_binary(&path).unwrap();
        assert_eq!(
            read(&path).unwrap().proof_profile.as_deref(),
            Some("Adobe RGB (1998)")
        );
        std::fs::write(&path, b"junk").unwrap();
        assert!(read(&path).unwrap_err().contains("Invalid preferences"));
        Value::Array(vec![]).to_file_binary(&path).unwrap();
        assert!(read(&path).unwrap_err().contains("not a dictionary"));
    }
}
