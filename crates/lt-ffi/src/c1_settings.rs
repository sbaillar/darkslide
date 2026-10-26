//! Capture One application settings (`lt_c1::settings`): what a Capture One
//! settings folder holds and what each item maps to. Read-only; the app
//! applies the result to its own stores (recipes, workspaces, presets, crop
//! ratios, keyword list, preferences) and journals what it wrote.

use std::path::Path;
use std::sync::Arc;

use lt_c1::settings::{
    self, AspectRatioImport, C1AppSettings, KeywordLibraryImport, Note, PrefsImport, RecipeFormat,
    RecipeImport, RecipeOutputSpace, RecipeSize, RecipeWatermark, StyleImport, StyleKind,
    WorkspaceImport,
};

use crate::clipboard::AdjustGroup;
use crate::error::DsError;
use crate::jobs::{CancelToken, run_job_on_thread};
use crate::session::SessionHandle;

/// A setting carried over approximately (`dropped == false`) or not at all.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1NoteRecord {
    pub key: String,
    pub text: String,
    pub dropped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum C1RecipeFormat {
    Jpeg,
    Tiff16,
}

#[derive(Debug, Clone, Copy, PartialEq, uniffi::Enum)]
pub enum C1RecipeSize {
    Original,
    LongEdge { px: u32 },
    ShortEdge { px: u32 },
    Percent { percent: f32 },
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct C1WatermarkRecord {
    pub text: Option<String>,
    pub logo: Option<String>,
    pub font: String,
    /// sRGB `0xRRGGBB`.
    pub color: u32,
    pub opacity: f32,
    /// `topLeft` … `bottomRight` (the app's `ExportWatermark.Anchor`).
    pub anchor: String,
}

/// One C1 process recipe as a Luxtable export recipe.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
#[allow(clippy::struct_excessive_bools)] // the app's recipe has these switches
pub struct C1RecipeRecord {
    pub file: String,
    pub name: String,
    pub enabled: bool,
    pub format: C1RecipeFormat,
    pub quality: u8,
    pub size: C1RecipeSize,
    pub allow_upscale: bool,
    /// `true`: Display P3; `false`: sRGB (or `profile_name`).
    pub display_p3: bool,
    /// An ICC profile to convert to, by name (looked up among installed profiles).
    pub profile_name: Option<String>,
    pub sharpen: f32,
    pub file_name_template: String,
    pub destination: Option<String>,
    pub subfolder: String,
    pub include_metadata: bool,
    pub strip_gps: bool,
    pub watermark: Option<C1WatermarkRecord>,
    pub notes: Vec<C1NoteRecord>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1WorkspaceTabRecord {
    pub c1_id: String,
    /// The app's built-in `ToolTab` raw value, or `None` for a custom tab.
    pub builtin: Option<String>,
    /// A custom tab's title.
    pub title: String,
    /// `ToolPanelID` raw values, in order.
    pub panels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1WorkspaceRecord {
    pub file: String,
    pub name: String,
    pub tabs: Vec<C1WorkspaceTabRecord>,
    pub collapsed: Vec<String>,
    pub unknown_tools: Vec<String>,
    pub notes: Vec<C1NoteRecord>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct C1AspectRatioRecord {
    pub key: String,
    pub name: String,
    pub width: f64,
    pub height: f64,
    /// The app's `CropPreset` with the same ratio, if any.
    pub builtin: Option<String>,
    pub notes: Vec<C1NoteRecord>,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct C1StyleRecord {
    pub file: String,
    pub name: String,
    /// A `.copreset` (one tool) rather than a `.costyle`.
    pub is_preset: bool,
    pub category: Option<String>,
    /// `lt_core::Adjustments` JSON (Luxtable's defaults with the style's values).
    pub adjustments_json: String,
    pub groups: Vec<AdjustGroup>,
    pub mapped: Vec<String>,
    pub notes: Vec<C1NoteRecord>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1KeywordLibraryRecord {
    pub file: String,
    pub name: String,
    pub keywords: Vec<String>,
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1PrefsRecord {
    /// A rename template for imports (`{original}`…).
    pub import_naming: Option<String>,
    pub enabled_recipes: Vec<String>,
    pub proof_profile: Option<String>,
    pub shortcut_set: Option<String>,
    pub notes: Vec<C1NoteRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1FolderRecord {
    pub folder: String,
    pub files: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct C1SkippedFileRecord {
    pub file: String,
    pub reason: String,
}

/// Everything found in a Capture One settings folder.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct C1AppSettingsRecord {
    pub recipes: Vec<C1RecipeRecord>,
    pub workspaces: Vec<C1WorkspaceRecord>,
    pub aspect_ratios: Vec<C1AspectRatioRecord>,
    pub styles: Vec<C1StyleRecord>,
    pub keyword_libraries: Vec<C1KeywordLibraryRecord>,
    pub prefs: Option<C1PrefsRecord>,
    pub folders: Vec<C1FolderRecord>,
    pub skipped: Vec<C1SkippedFileRecord>,
}

fn notes(n: Vec<Note>) -> Vec<C1NoteRecord> {
    n.into_iter()
        .map(|n| C1NoteRecord {
            key: n.key,
            text: n.text,
            dropped: n.dropped,
        })
        .collect()
}

fn strings(v: Vec<&'static str>) -> Vec<String> {
    v.into_iter().map(str::to_owned).collect()
}

impl From<RecipeImport> for C1RecipeRecord {
    fn from(r: RecipeImport) -> Self {
        let (display_p3, profile_name) = match r.space {
            RecipeOutputSpace::Srgb => (false, None),
            RecipeOutputSpace::DisplayP3 => (true, None),
            RecipeOutputSpace::Profile(p) => (false, Some(p)),
        };
        Self {
            file: r.file,
            name: r.name,
            enabled: r.enabled,
            format: match r.format {
                RecipeFormat::Jpeg => C1RecipeFormat::Jpeg,
                RecipeFormat::Tiff16 => C1RecipeFormat::Tiff16,
            },
            quality: r.quality,
            size: match r.size {
                RecipeSize::Original => C1RecipeSize::Original,
                RecipeSize::LongEdge(px) => C1RecipeSize::LongEdge { px },
                RecipeSize::ShortEdge(px) => C1RecipeSize::ShortEdge { px },
                RecipeSize::Percent(percent) => C1RecipeSize::Percent { percent },
            },
            allow_upscale: r.allow_upscale,
            display_p3,
            profile_name,
            sharpen: r.sharpen,
            file_name_template: r.file_name_template,
            destination: r.destination,
            subfolder: r.subfolder,
            include_metadata: r.include_metadata,
            strip_gps: r.strip_gps,
            watermark: r.watermark.map(|w: RecipeWatermark| C1WatermarkRecord {
                text: w.text,
                logo: w.logo,
                font: w.font,
                color: w.color,
                opacity: w.opacity,
                anchor: w.anchor.to_owned(),
            }),
            notes: notes(r.notes),
            fingerprint: r.fingerprint,
        }
    }
}

impl From<WorkspaceImport> for C1WorkspaceRecord {
    fn from(w: WorkspaceImport) -> Self {
        Self {
            file: w.file,
            name: w.name,
            tabs: w
                .tabs
                .into_iter()
                .map(|t| C1WorkspaceTabRecord {
                    c1_id: t.c1_id,
                    builtin: t.builtin.map(str::to_owned),
                    title: t.title,
                    panels: strings(t.panels),
                })
                .collect(),
            collapsed: strings(w.collapsed),
            unknown_tools: w.unknown_tools,
            notes: notes(w.notes),
            fingerprint: w.fingerprint,
        }
    }
}

impl From<AspectRatioImport> for C1AspectRatioRecord {
    fn from(a: AspectRatioImport) -> Self {
        Self {
            key: a.key,
            name: a.name,
            width: a.width,
            height: a.height,
            builtin: a.builtin.map(str::to_owned),
            notes: notes(a.notes),
        }
    }
}

impl C1StyleRecord {
    fn from(s: StyleImport) -> Result<Self, DsError> {
        Ok(Self {
            file: s.file,
            name: s.name,
            is_preset: s.kind == StyleKind::Preset,
            category: s.category,
            adjustments_json: SessionHandle::to_json(&s.adjustments)?,
            groups: s.groups.into_iter().map(AdjustGroup::from).collect(),
            mapped: s.mapped,
            notes: notes(s.notes),
            fingerprint: s.fingerprint,
        })
    }
}

impl From<KeywordLibraryImport> for C1KeywordLibraryRecord {
    fn from(k: KeywordLibraryImport) -> Self {
        Self {
            file: k.file,
            name: k.name,
            keywords: k.keywords,
            fingerprint: k.fingerprint,
        }
    }
}

impl From<PrefsImport> for C1PrefsRecord {
    fn from(p: PrefsImport) -> Self {
        Self {
            import_naming: p.import_naming,
            enabled_recipes: p.enabled_recipes,
            proof_profile: p.proof_profile,
            shortcut_set: p.shortcut_set,
            notes: notes(p.notes),
        }
    }
}

impl C1AppSettingsRecord {
    fn from(s: C1AppSettings) -> Result<Self, DsError> {
        Ok(Self {
            recipes: s.recipes.into_iter().map(Into::into).collect(),
            workspaces: s.workspaces.into_iter().map(Into::into).collect(),
            aspect_ratios: s.aspect_ratios.into_iter().map(Into::into).collect(),
            styles: s
                .styles
                .into_iter()
                .map(C1StyleRecord::from)
                .collect::<Result<_, _>>()?,
            keyword_libraries: s.keyword_libraries.into_iter().map(Into::into).collect(),
            prefs: s.prefs.map(Into::into),
            folders: s
                .folders
                .into_iter()
                .map(|f| C1FolderRecord {
                    folder: f.folder,
                    files: u32::try_from(f.files).unwrap_or(u32::MAX),
                })
                .collect(),
            skipped: s
                .skipped
                .into_iter()
                .map(|k| C1SkippedFileRecord {
                    file: k.file,
                    reason: k.reason,
                })
                .collect(),
        })
    }
}

/// [`read_capture_one_settings`]'s work, synchronous (tests).
pub(crate) fn read_settings(
    folder: &str,
    prefs: Option<&str>,
    cancel: &settings::Cancel,
) -> Result<C1AppSettingsRecord, DsError> {
    let s =
        settings::read_app_settings_cancellable(Path::new(folder), prefs.map(Path::new), cancel)?;
    C1AppSettingsRecord::from(s)
}

/// Read a Capture One settings folder (`~/Library/Application Support/Capture One`)
/// and, when given, its preferences file: every recipe, workspace, crop ratio,
/// style, preset and keyword library, what each maps to in Luxtable, and the
/// files that couldn't be read. Nothing is written anywhere.
#[uniffi::export]
pub async fn read_capture_one_settings(
    folder: String,
    prefs: Option<String>,
    token: Arc<CancelToken>,
) -> Result<C1AppSettingsRecord, DsError> {
    let probe = token.clone();
    let cancel: settings::Cancel = Arc::new(move || probe.is_cancelled());
    run_job_on_thread("lt-c1-settings", token, move |cancelled| {
        if cancelled() {
            return Err(DsError::Cancelled);
        }
        read_settings(&folder, prefs.as_deref(), &cancel)
    })
    .await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use std::path::PathBuf;

    use super::*;

    fn no() -> settings::Cancel {
        Arc::new(|| false)
    }

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../lt-c1/fixtures/settings")
    }

    #[test]
    fn records_carry_every_category() {
        let s = read_settings(fixtures().to_str().unwrap(), None, &no()).unwrap();
        assert_eq!(s.recipes.len(), 3);
        let web = &s.recipes[0];
        assert_eq!(web.size, C1RecipeSize::LongEdge { px: 2048 });
        assert_eq!((web.display_p3, web.profile_name.as_deref()), (false, None));
        assert_eq!(web.format, C1RecipeFormat::Jpeg);
        let a3 = &s.recipes[1];
        assert_eq!(a3.profile_name.as_deref(), Some("Adobe RGB (1998)"));
        assert_eq!(a3.format, C1RecipeFormat::Tiff16);
        let psd = &s.recipes[2];
        assert!(psd.display_p3);
        assert_eq!(psd.size, C1RecipeSize::Original);
        let wm = psd.watermark.as_ref().unwrap();
        assert_eq!(
            (wm.anchor.as_str(), wm.text.as_deref()),
            ("bottomRight", Some("© Luxtable"))
        );
        assert!(
            psd.notes
                .iter()
                .any(|n| n.text.contains("PSD") && !n.dropped)
        );

        let w = &s.workspaces[0];
        assert_eq!(w.tabs[1].builtin.as_deref(), Some("favorites"));
        assert_eq!(w.tabs[3].title, "Capture");
        assert_eq!(w.collapsed, ["levels"]);

        assert_eq!(s.aspect_ratios[1].builtin.as_deref(), Some("1:1"));
        let brighter = s.styles.iter().find(|st| st.name == "Brighter").unwrap();
        assert!(brighter.is_preset);
        assert_eq!(brighter.groups, [AdjustGroup::Exposure]);
        let a: lt_core::Adjustments = serde_json::from_str(&brighter.adjustments_json).unwrap();
        assert_eq!(a.exposure.exposure_ev, 0.5);
        assert_eq!(s.keyword_libraries[0].name, "Travel");
        assert!(s.prefs.is_none());
        assert_eq!(s.folders[0].folder, "Recipes143");
        assert!(s.skipped.iter().any(|k| k.file == "Recipes143/Broken.xml"));
    }

    #[test]
    fn preferences_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = dir.path().join("p.plist");
        std::fs::write(
            &prefs,
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
<key>enabledRecipeNames</key><array><string>PSD - Logo</string></array>
<key>KeyboardShortcutSet</key><string>Default</string></dict></plist>"#,
        )
        .unwrap();
        let s = read_settings(fixtures().to_str().unwrap(), prefs.to_str(), &no()).unwrap();
        let p = s.prefs.unwrap();
        assert_eq!(p.enabled_recipes, ["PSD - Logo"]);
        assert_eq!(p.shortcut_set.as_deref(), Some("Default"));
        assert_eq!(p.notes.len(), 1);
        assert!(s.recipes[2].enabled);
        let missing = dir.path().join("none");
        assert!(matches!(
            read_settings(missing.to_str().unwrap(), None, &no()),
            Err(DsError::Io { .. })
        ));
        let stop: settings::Cancel = Arc::new(|| true);
        assert!(matches!(
            read_settings(fixtures().to_str().unwrap(), None, &stop),
            Err(DsError::Cancelled)
        ));
    }

    #[test]
    fn async_read_and_cancel() {
        let folder = fixtures().to_string_lossy().into_owned();
        let s = futures_executor::block_on(read_capture_one_settings(
            folder.clone(),
            None,
            CancelToken::new(),
        ))
        .unwrap();
        assert_eq!(s.recipes.len(), 3);
        let token = CancelToken::new();
        token.cancel();
        assert!(matches!(
            futures_executor::block_on(read_capture_one_settings(folder, None, token)),
            Err(DsError::Cancelled)
        ));
    }
}
