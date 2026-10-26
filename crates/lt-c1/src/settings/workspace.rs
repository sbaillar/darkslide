//! Workspaces (`Workspaces/<name>.plist`): per window, the tool tabs. The main
//! window's `com.phaseone.captureone.sessionwindow.tools` holds
//! `tabConfiguration` (the tabs shown, in order) and, per tab id,
//! `[[tool ids], [collapsed tool indices], [per-tool options]]`.
//! The second array's meaning (collapsed, not expanded) is inferred: it lists
//! tools C1 shows folded by default (Annotations, Overlay, Moiré).

use plist::Value;

use super::{Note, fingerprint};

const SESSION_WINDOW: &str = "com.phaseone.captureone.sessionwindow.tools";
const OTHER_WINDOWS: [&str; 2] = [
    "com.phaseone.captureone.viewerwindow.tools",
    "com.phaseone.captureone.livepreviewwindow.tools",
];

/// One rail tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTab {
    /// Capture One's tab id (`ExposureToolTab`).
    pub c1_id: String,
    /// The Luxtable built-in tab (`ToolTab` raw value) it becomes, or
    /// `None` for a custom tab.
    pub builtin: Option<&'static str>,
    /// A custom tab's title ("Capture"); empty for a built-in tab.
    pub title: String,
    /// Luxtable tools (`ToolPanelID` raw values), in C1's order.
    pub panels: Vec<&'static str>,
}

/// One C1 workspace as a named Luxtable workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceImport {
    /// The file's name without `.plist` (the import's key).
    pub file: String,
    /// The saved workspace's name in Luxtable.
    pub name: String,
    pub tabs: Vec<WorkspaceTab>,
    /// Tools C1 shows collapsed.
    pub collapsed: Vec<&'static str>,
    /// C1 tools with no Luxtable tool: "Tab: Tool".
    pub unknown_tools: Vec<String>,
    pub notes: Vec<Note>,
    pub fingerprint: String,
}

/// C1 tab id → Luxtable built-in tab.
fn builtin_tab(id: &str) -> Option<&'static str> {
    Some(match id {
        "QuickToolTab" => "favorites",
        "OrganizeToolTab" | "LibraryToolTab" => "library",
        "ColorToolTab" => "color",
        "ExposureToolTab" => "exposure",
        "LensToolTab" => "lens",
        "CompositionToolTab" => "composition",
        "DetailsToolTab" => "details",
        "LocalAdjustmentsToolTab" | "LayersToolTab" => "layers",
        "MetaDataToolTab" | "MetadataToolTab" => "metadata",
        "OutputToolTab" | "ExportToolTab" => "output",
        _ => return None,
    })
}

/// C1 tool id → Luxtable tools (seen in the real workspace, or C1's tool
/// names where they're plain). The same tool may land in one panel twice
/// (Crop and Rotation are one Luxtable panel); duplicates are dropped.
fn panels(tool: &str) -> &'static [&'static str] {
    match tool {
        "Library" => &["smartFolders", "sessions", "albums"],
        "MetadataFilters" | "Filters" => &["filters"],
        "Styles" => &["styles"],
        "WhiteBalance" => &["whiteBalance"],
        "Exposure" => &["exposure"],
        "ShadowHighlight" | "HighDynamicRange" => &["hdr"],
        "Levels" => &["levels"],
        "Curves" | "Curve" => &["curve"],
        "Clarity" => &["clarity"],
        "Dehaze" => &["presence"],
        "Vignetting" => &["vignette"],
        "Sharpening" => &["sharpening"],
        "Noise" | "NoiseReduction" => &["noise"],
        "Film Grain" | "FilmGrain" => &["effects"],
        "ColorBalance" => &["colorBalance"],
        "SelectiveColorControl" | "ColorEditor" => &["colorEditor"],
        "LocalAdjustments" | "Layers" => &["layers"],
        "Lens" | "LensCorrection" | "Perspective" | "Keystone" => &["lensCorrection"],
        "Crop" | "Rotation" => &["crop"],
        "SpotRemoval" | "Spot Removal" => &["spots"],
        "Metadata" => &["metadata"],
        "Keywords" | "KeywordLibrary" => &["keywords"],
        "ExportDialogRecipeList"
        | "ProcessRecipe"
        | "ProcessRecipes"
        | "ProcessSummary"
        | "OutputLocation"
        | "OutputNaming"
        | "OutputCrossRecipeTokensInspectorTool" => &["export"],
        _ => &[],
    }
}

/// `BlackWhiteToolTab` → "Black & White", `CaptureToolTab` → "Capture".
fn humanize(id: &str) -> String {
    let stem = id.strip_suffix("ToolTab").unwrap_or(id);
    if stem == "BlackWhite" {
        return "Black & White".into();
    }
    let mut out = String::new();
    for (i, c) in stem.chars().enumerate() {
        if i > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.push(c);
    }
    if out.trim().is_empty() {
        "Custom".into()
    } else {
        out
    }
}

/// Add C1 tab `id` (`[[tool ids], [collapsed indices], …]`) to `out`. A
/// tool's first appearance (`seen`) decides whether it starts collapsed.
fn add_tab(id: &str, entry: &[Value], out: &mut WorkspaceImport, seen: &mut Vec<&'static str>) {
    let names: Vec<&str> = entry
        .first()
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_string).collect())
        .unwrap_or_default();
    let folded: Vec<usize> = entry
        .get(1)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_unsigned_integer)
                .map(|i| i as usize)
                .collect()
        })
        .unwrap_or_default();
    let builtin = builtin_tab(id).filter(|b| !out.tabs.iter().any(|t| t.builtin == Some(*b)));
    let mut tab = WorkspaceTab {
        title: if builtin.is_some() {
            String::new()
        } else {
            humanize(id)
        },
        c1_id: id.to_owned(),
        builtin,
        panels: Vec::new(),
    };
    for (i, name) in names.iter().enumerate() {
        let mapped = panels(name);
        if mapped.is_empty() {
            let label = format!("{}: {name}", humanize(id));
            if !out.unknown_tools.contains(&label) {
                out.unknown_tools.push(label);
            }
        }
        for p in mapped {
            if !tab.panels.contains(p) {
                tab.panels.push(p);
            }
            if !seen.contains(p) {
                seen.push(p);
                if folded.contains(&i) {
                    out.collapsed.push(p);
                }
            }
        }
    }
    out.tabs.push(tab);
}

pub(crate) fn parse(bytes: &[u8], file: &str) -> Result<WorkspaceImport, String> {
    let root = super::limits::plist(bytes).map_err(|e| format!("Invalid workspace: {e}"))?;
    let root = root
        .as_dictionary()
        .ok_or("Invalid workspace: not a dictionary")?;
    let tools = root
        .get(SESSION_WINDOW)
        .and_then(Value::as_dictionary)
        .ok_or("No main-window tool layout in this workspace")?;
    let order: Vec<String> = match tools.get("tabConfiguration").and_then(Value::as_array) {
        Some(a) => a
            .iter()
            .filter_map(Value::as_string)
            .map(str::to_owned)
            .collect(),
        None => tools
            .keys()
            .filter(|k| k.ends_with("ToolTab"))
            .cloned()
            .collect(),
    };

    let mut out = WorkspaceImport {
        file: file.to_owned(),
        name: format!("{file} (Capture One)"),
        tabs: Vec::new(),
        collapsed: Vec::new(),
        unknown_tools: Vec::new(),
        notes: Vec::new(),
        fingerprint: fingerprint(bytes),
    };
    let mut seen: Vec<&'static str> = Vec::new();
    for id in order {
        if let Some(entry) = tools.get(&id).and_then(Value::as_array) {
            add_tab(&id, entry, &mut out, &mut seen);
        }
    }
    if out.tabs.is_empty() {
        return Err("The workspace has no tool tabs".into());
    }
    if out.unknown_tools.iter().any(|t| t.ends_with(": Histogram")) {
        out.notes.push(Note::approx(
            "Histogram",
            "Histogram: Luxtable shows it above the tools, in every tab",
        ));
    }
    if OTHER_WINDOWS.iter().any(|w| root.contains_key(w)) {
        out.notes.push(Note::dropped(
            "windows",
            "Only the main window's tool tabs are imported (not the viewer or live-view windows)",
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A main-window workspace plist with `tabs` = (id, tools, collapsed).
    pub(crate) fn plist(
        tabs: &[(&str, &[&str], &[u64])],
        order: Option<&[&str]>,
        extra_window: bool,
    ) -> Vec<u8> {
        let mut tools = plist::Dictionary::new();
        for (id, names, folded) in tabs {
            tools.insert(
                (*id).to_owned(),
                Value::Array(vec![
                    Value::Array(names.iter().map(|n| Value::String((*n).into())).collect()),
                    Value::Array(folded.iter().map(|i| Value::Integer((*i).into())).collect()),
                    Value::Array(vec![]),
                ]),
            );
        }
        if let Some(order) = order {
            tools.insert(
                "tabConfiguration".into(),
                Value::Array(order.iter().map(|n| Value::String((*n).into())).collect()),
            );
        }
        let mut root = plist::Dictionary::new();
        root.insert(SESSION_WINDOW.into(), Value::Dictionary(tools));
        if extra_window {
            root.insert(
                OTHER_WINDOWS[0].into(),
                Value::Dictionary(plist::Dictionary::new()),
            );
        }
        let mut out = Vec::new();
        Value::Dictionary(root).to_writer_xml(&mut out).unwrap();
        out
    }

    #[test]
    fn tabs_tools_and_collapsed() {
        let bytes = plist(
            &[
                (
                    "QuickToolTab",
                    &[
                        "Histogram",
                        "Styles",
                        "WhiteBalance",
                        "Exposure",
                        "ShadowHighlight",
                    ],
                    &[],
                ),
                (
                    "CompositionToolTab",
                    &["Crop", "Rotation", "Perspective", "Overlay"],
                    &[1, 3],
                ),
                ("CaptureToolTab", &["Camera", "CameraFocus"], &[]),
                ("BlackWhiteToolTab", &["BlackAndWhite", "Curves"], &[]),
                ("HiddenToolTab", &["Levels"], &[]),
            ],
            Some(&[
                "QuickToolTab",
                "CompositionToolTab",
                "CaptureToolTab",
                "BlackWhiteToolTab",
                "MissingToolTab",
            ]),
            true,
        );
        let w = parse(&bytes, "Customized").unwrap();
        assert_eq!(w.name, "Customized (Capture One)");
        let summary: Vec<(Option<&str>, &str, Vec<&str>)> = w
            .tabs
            .iter()
            .map(|t| (t.builtin, t.title.as_str(), t.panels.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    Some("favorites"),
                    "",
                    vec!["styles", "whiteBalance", "exposure", "hdr"]
                ),
                (Some("composition"), "", vec!["crop", "lensCorrection"]),
                (None, "Capture", vec![]),
                (None, "Black & White", vec!["curve"]),
            ]
        );
        // Rotation (index 1) is the crop panel, which first appeared expanded.
        assert_eq!(w.collapsed, Vec::<&str>::new());
        assert_eq!(
            w.unknown_tools,
            vec![
                "Quick: Histogram",
                "Composition: Overlay",
                "Capture: Camera",
                "Capture: CameraFocus",
                "Black & White: BlackAndWhite"
            ]
        );
        let notes: Vec<&str> = w.notes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(notes, ["Histogram", "windows"]);
        assert_eq!(w.fingerprint.len(), 16);
    }

    #[test]
    fn collapsed_and_duplicate_builtins() {
        let bytes = plist(
            &[
                (
                    "MetaDataToolTab",
                    &["Metadata", "Keywords", "KeywordLibrary", "Annotations"],
                    &[1],
                ),
                ("MetadataToolTab", &["Metadata"], &[]),
            ],
            None,
            false,
        );
        let w = parse(&bytes, "W").unwrap();
        assert_eq!(w.tabs.len(), 2);
        assert_eq!(w.tabs[0].builtin, Some("metadata"));
        assert_eq!(w.tabs[0].panels, ["metadata", "keywords"]);
        // The second tab that would also be Metadata becomes a custom tab.
        assert_eq!(
            (w.tabs[1].builtin, w.tabs[1].title.as_str()),
            (None, "Metadata")
        );
        assert_eq!(w.collapsed, ["keywords"]);
        assert_eq!(w.notes.len(), 0);
    }

    #[test]
    fn errors() {
        assert!(
            parse(b"nope", "x")
                .unwrap_err()
                .contains("Invalid workspace")
        );
        let mut out = Vec::new();
        Value::Array(vec![]).to_writer_xml(&mut out).unwrap();
        assert!(parse(&out, "x").unwrap_err().contains("not a dictionary"));
        let mut out = Vec::new();
        Value::Dictionary(plist::Dictionary::new())
            .to_writer_xml(&mut out)
            .unwrap();
        assert!(parse(&out, "x").unwrap_err().contains("No main-window"));
        let empty = plist(&[], Some(&["GoneToolTab"]), false);
        assert!(parse(&empty, "x").unwrap_err().contains("no tool tabs"));
    }

    #[test]
    fn names() {
        assert_eq!(humanize("LocalAdjustmentsToolTab"), "Local Adjustments");
        assert_eq!(humanize("ToolTab"), "Custom");
        assert_eq!(builtin_tab("ExportToolTab"), Some("output"));
        assert_eq!(panels("Spot Removal"), ["spots"]);
    }
}
