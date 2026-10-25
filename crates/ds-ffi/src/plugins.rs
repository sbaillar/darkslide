//! Plug-ins (contract 28): the registry for Settings ▸ Plugins, the data packs the
//! app reads (looks, styles, voice phrases), and plug-in export post-processors.
//!
//! The app opens one [`PluginHost`] at launch (bundled `Plugins` folder + the
//! support root's); export recipes name post-process steps by plug-in and contribution
//! id, resolved through the host that was opened last.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use ds_core::Size;
use ds_export::watermark::Canvas;
use ds_export::{CanvasLayout, ExportError, ExportItem, OutputSpace, PhotoRect, PostProcess};
use ds_plugin::params::{self, Param, ParamType};
use ds_plugin::wasm::{Pixels, PostProcessor};
use ds_plugin::{
    Approval, Host, InstallOptions, InstallOutcome, Kind, Permission, Platform, Status, Trust,
};
use serde_json::{Map, Value, json};

use crate::error::DsError;
use crate::jobs::guard;

fn err(e: ds_plugin::PluginError) -> DsError {
    match e {
        ds_plugin::PluginError::Io(e) => DsError::Io {
            message: e.to_string(),
        },
        ds_plugin::PluginError::Cancelled => DsError::Cancelled,
        e => DsError::invalid(e.to_string()),
    }
}

/// How far a plug-in is trusted.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PluginTrust {
    BuiltIn,
    /// `signer` is the name the key claims; `fingerprint` identifies it.
    Signed {
        signer: String,
        key: String,
        fingerprint: String,
    },
    /// Signed by a key the user hasn't trusted.
    Untrusted {
        signer: String,
        key: String,
        fingerprint: String,
    },
    Unsigned,
}

impl From<&Trust> for PluginTrust {
    fn from(t: &Trust) -> Self {
        match t {
            Trust::BuiltIn => Self::BuiltIn,
            Trust::Signed(s) => Self::Signed {
                signer: s.name.clone(),
                key: s.key.clone(),
                fingerprint: s.fingerprint(),
            },
            Trust::Untrusted(s) => Self::Untrusted {
                signer: s.name.clone(),
                key: s.key.clone(),
                fingerprint: s.fingerprint(),
            },
            Trust::Unsigned => Self::Unsigned,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PluginState {
    Active,
    Disabled,
    NeedsApproval,
    Shadowed,
    Unsupported,
    Error,
}

/// One decision a plug-in waits for.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PluginApproval {
    /// Its files changed since they were installed or approved.
    Changed,
    /// It was put in the Plugins folder by hand.
    NotInstalled,
    TrustSigner {
        signer: String,
        key: String,
        fingerprint: String,
    },
    AllowUnsignedCode,
    Grant {
        permission: String,
    },
}

impl From<&Approval> for PluginApproval {
    fn from(a: &Approval) -> Self {
        match a {
            Approval::Changed => Self::Changed,
            Approval::NotInstalled => Self::NotInstalled,
            Approval::TrustSigner(s) => Self::TrustSigner {
                signer: s.name.clone(),
                key: s.key.clone(),
                fingerprint: s.fingerprint(),
            },
            Approval::AllowUnsignedCode => Self::AllowUnsignedCode,
            Approval::Grant(p) => Self::Grant {
                permission: p.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PluginPermissionInfo {
    /// The manifest's spelling (`network:api.example.com`): pass it to `grant`.
    pub permission: String,
    /// What it allows, in words.
    pub label: String,
    /// Reaches outside the plug-in's inputs (files, network, accounts).
    pub sensitive: bool,
    pub granted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PluginContributionInfo {
    /// `looks`, `styles`, `voice_phrases`, `export_post_process`, `export_destination`,
    /// `native_feature`.
    pub kind: String,
    pub id: String,
    pub name: String,
    /// A native feature: what it reaches beyond the app, in words ("Your Immich server").
    pub uses: Vec<String>,
}

/// One plug-in folder for Settings ▸ Plugins.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PluginRecord {
    /// The manifest's id, or the folder's name when the manifest can't be read.
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub summary: String,
    pub authors: Vec<String>,
    pub path: String,
    pub built_in: bool,
    /// A development folder loaded in place (shown as DEV).
    pub dev: bool,
    pub trust: PluginTrust,
    pub state: PluginState,
    /// The load error, or the folder that shadows this one.
    pub message: Option<String>,
    pub approvals: Vec<PluginApproval>,
    pub permissions: Vec<PluginPermissionInfo>,
    pub contributions: Vec<PluginContributionInfo>,
    pub warnings: Vec<String>,
    pub has_code: bool,
}

/// What installing a folder would do (the confirmation sheet).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PluginInstallPreview {
    /// The staged copy: pass to `install` or `discard`.
    pub token: String,
    pub id: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub trust: PluginTrust,
    pub permissions: Vec<PluginPermissionInfo>,
    pub contributions: Vec<PluginContributionInfo>,
    pub has_code: bool,
    /// The installed version this replaces.
    pub replaces: Option<String>,
    /// It is older than the installed version.
    pub downgrade: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PluginInstallResult {
    Installed {
        id: String,
        version: String,
    },
    Updated {
        id: String,
        from: String,
        to: String,
    },
    AlreadyInstalled {
        id: String,
        version: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PluginDataKind {
    Looks,
    Styles,
    VoicePhrases,
}

/// One data file of an active plug-in: its normalized JSON (see `ds_plugin::pack`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PluginDataPack {
    pub plugin_id: String,
    pub plugin_name: String,
    pub contribution_id: String,
    pub json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PluginDataPacks {
    pub packs: Vec<PluginDataPack>,
    /// Files that didn't load ("Name (looks.json): line 3: …").
    pub errors: Vec<String>,
}

/// A parameter the host draws for a contribution.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PluginParamInfo {
    pub key: String,
    pub label: String,
    /// `float`, `int`, `bool`, `choice`, `color` or `text`.
    pub kind: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub choices: Vec<String>,
    pub max_len: Option<u32>,
    /// The default, as JSON (`0.03`, `"#000000"`, `true`).
    pub default_json: String,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct PostProcessorInfo {
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub contribution_id: String,
    pub name: String,
    pub params: Vec<PluginParamInfo>,
}

fn param_info(p: &Param) -> PluginParamInfo {
    let (kind, min, max, choices, max_len) = match &p.ty {
        ParamType::Float { min, max } => ("float", Some(*min), Some(*max), vec![], None),
        ParamType::Int { min, max } => ("int", Some(*min as f64), Some(*max as f64), vec![], None),
        ParamType::Bool => ("bool", None, None, vec![], None),
        ParamType::Choice(c) => ("choice", None, None, c.clone(), None),
        ParamType::Color => ("color", None, None, vec![], None),
        ParamType::Text { max_len } => ("text", None, None, vec![], u32::try_from(*max_len).ok()),
    };
    PluginParamInfo {
        key: p.key.clone(),
        label: p.label.clone(),
        kind: kind.into(),
        min,
        max,
        choices,
        max_len,
        default_json: p.default.to_string(),
    }
}

/// A permission in words, for the install sheet and the Plugins pane.
#[must_use]
pub fn permission_label(p: &Permission) -> String {
    match p {
        Permission::PhotoPixels => "See the photo's pixels".into(),
        Permission::PhotoMetadata => {
            "Read the photo's metadata (camera, keywords, location)".into()
        }
        Permission::MetadataWrite => "Suggest metadata changes".into(),
        Permission::FilesRead(l) => format!("Read files in a folder you choose ({l})"),
        Permission::FilesWrite(l) => format!("Write files in a folder you choose ({l})"),
        Permission::Network(h) => format!("Connect to {h}"),
        Permission::Account(a) => format!("Sign in to your {a} account"),
        Permission::Clock => "Read the date and time".into(),
    }
}

fn permissions(
    m: &ds_plugin::Manifest,
    granted: &std::collections::BTreeSet<Permission>,
    built_in: bool,
) -> Vec<PluginPermissionInfo> {
    m.permissions
        .iter()
        .map(|p| PluginPermissionInfo {
            permission: p.to_string(),
            label: permission_label(p),
            sensitive: p.is_sensitive(),
            granted: built_in || granted.contains(p),
        })
        .collect()
}

fn contributions(m: &ds_plugin::Manifest) -> Vec<PluginContributionInfo> {
    m.contributions
        .iter()
        .map(|c| PluginContributionInfo {
            kind: c.kind.key().into(),
            id: c.id.clone(),
            name: c.name.clone(),
            uses: c.uses.clone(),
        })
        .collect()
}

/// The plug-in registry, shared with export (the last one opened).
#[derive(Debug, uniffi::Object)]
pub struct PluginHost {
    host: Mutex<Host>,
}

fn current() -> &'static Mutex<Option<Arc<PluginHost>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<PluginHost>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

impl PluginHost {
    /// A host export never resolves through (tests running beside the export tests).
    #[cfg(test)]
    pub(crate) fn unshared(builtin_dir: Option<PathBuf>, user_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            host: Mutex::new(Host::open(
                builtin_dir,
                user_dir,
                ds_plugin::darkslide_version(),
                Platform::Macos,
            )),
        })
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Host> {
        self.host.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn with<T>(
        &self,
        f: impl FnOnce(&mut Host) -> Result<T, ds_plugin::PluginError>,
    ) -> Result<T, DsError> {
        guard(|| f(&mut self.lock()).map_err(err))
    }
}

#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
impl PluginHost {
    /// Load every plug-in: `builtin_dir` is the app bundle's `Plugins` folder (none in
    /// tests), `user_dir` `<support>/Plugins`. This host serves export recipes from now on.
    #[uniffi::constructor]
    pub fn open(builtin_dir: Option<String>, user_dir: String) -> Arc<Self> {
        let host = Host::open(
            builtin_dir.map(PathBuf::from),
            PathBuf::from(user_dir),
            ds_plugin::darkslide_version(),
            Platform::Macos,
        );
        let me = Arc::new(Self {
            host: Mutex::new(host),
        });
        *current().lock().unwrap_or_else(PoisonError::into_inner) = Some(me.clone());
        me
    }

    /// Every plug-in folder, bundled first.
    pub fn list(&self) -> Vec<PluginRecord> {
        let h = self.lock();
        h.plugins()
            .iter()
            .map(|p| {
                let built_in = p.origin == ds_plugin::Origin::BuiltIn;
                let folder = p
                    .dir
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (state, message, approvals) = match &p.status {
                    Status::Active => (PluginState::Active, None, vec![]),
                    Status::Disabled => (PluginState::Disabled, None, vec![]),
                    Status::NeedsApproval(a) => (
                        PluginState::NeedsApproval,
                        None,
                        a.iter().map(Into::into).collect(),
                    ),
                    Status::Shadowed { by } => (
                        PluginState::Shadowed,
                        Some(by.display().to_string()),
                        vec![],
                    ),
                    Status::Unsupported(m) => (PluginState::Unsupported, Some(m.clone()), vec![]),
                    Status::Error(m) => (PluginState::Error, Some(m.clone()), vec![]),
                };
                let m = p.manifest.as_ref();
                let granted = m.map(|m| h.granted(&m.id)).unwrap_or_default();
                PluginRecord {
                    id: m.map_or_else(|| folder.clone(), |m| m.id.clone()),
                    name: m.map_or(folder, |m| m.name.clone()),
                    version: m.map(|m| m.version.to_string()),
                    summary: m.map(|m| m.summary.clone()).unwrap_or_default(),
                    authors: m.map(|m| m.authors.clone()).unwrap_or_default(),
                    path: p.dir.display().to_string(),
                    built_in,
                    dev: p.origin == ds_plugin::Origin::Dev,
                    trust: (&p.trust).into(),
                    state,
                    message,
                    approvals,
                    permissions: m
                        .map(|m| permissions(m, &granted, built_in))
                        .unwrap_or_default(),
                    contributions: m.map(contributions).unwrap_or_default(),
                    warnings: m.map(|m| m.warnings.clone()).unwrap_or_default(),
                    has_code: m.is_some_and(ds_plugin::Manifest::has_code),
                }
            })
            .collect()
    }

    /// A problem with the settings file, if there was one at launch.
    pub fn state_error(&self) -> Option<String> {
        self.lock().state_error.clone()
    }

    pub fn allow_unsigned_code(&self) -> bool {
        self.lock().allow_unsigned_code()
    }

    pub fn set_allow_unsigned_code(&self, allow: bool) -> Result<(), DsError> {
        self.with(|h| h.set_allow_unsigned_code(allow))
    }

    /// Copy the package at `path` (a folder or a `.dsplugin` zip) into staging,
    /// validate that copy and say what installing it would do.
    pub fn preview(&self, path: String) -> Result<PluginInstallPreview, DsError> {
        self.with(|h| {
            let p = h.stage(Path::new(&path))?;
            let m = &p.manifest;
            Ok(PluginInstallPreview {
                token: p.token.clone(),
                id: m.id.clone(),
                name: m.name.clone(),
                version: m.version.to_string(),
                summary: m.summary.clone(),
                trust: (&p.trust).into(),
                permissions: permissions(m, &std::collections::BTreeSet::new(), false),
                contributions: contributions(m),
                has_code: m.has_code(),
                downgrade: p.replaces.as_ref().is_some_and(|r| *r > m.version),
                replaces: p.replaces.map(|v| v.to_string()),
            })
        })
    }

    /// Install the staged copy `token` (the user's own folder is never touched).
    pub fn install(
        &self,
        token: String,
        allow_downgrade: bool,
    ) -> Result<PluginInstallResult, DsError> {
        self.with(|h| {
            Ok(
                match h.install_staged(&token, InstallOptions { allow_downgrade })? {
                    InstallOutcome::Installed { id, version } => {
                        PluginInstallResult::Installed { id, version }
                    }
                    InstallOutcome::Updated { id, from, to } => {
                        PluginInstallResult::Updated { id, from, to }
                    }
                    InstallOutcome::AlreadyInstalled { id, version } => {
                        PluginInstallResult::AlreadyInstalled { id, version }
                    }
                },
            )
        })
    }

    /// Allow this one unsigned code plug-in to run (not the global switch).
    pub fn allow_unsigned(&self, id: String) -> Result<(), DsError> {
        self.with(|h| h.allow_unsigned(&id))
    }

    /// The values a recipe step will use, and a note for each it had to change (an
    /// update narrowed a range): the recipe editor shows them.
    pub fn step_notes(
        &self,
        plugin_id: String,
        contribution_id: String,
        params_json: String,
    ) -> Vec<String> {
        let mut h = self.lock();
        let Ok((_, declared, _)) = h.post_processor(&plugin_id, &contribution_id) else {
            return Vec::new();
        };
        let given: Map<String, Value> = serde_json::from_str(&params_json).unwrap_or_default();
        params::resolve_lenient(&declared, &given).1
    }

    /// Drop a staged copy (the install sheet was cancelled).
    pub fn discard(&self, token: String) {
        self.lock().discard(&token);
    }

    /// After the app moved a plug-in to the Trash: forget its grants and pins.
    pub fn forget(&self, id: String) -> Result<(), DsError> {
        self.with(|h| h.forget(&id))
    }

    /// Load the plug-in folder at `path` in place, as a development plug-in.
    pub fn add_dev_folder(&self, path: String) -> Result<(), DsError> {
        self.with(|h| h.add_dev_folder(Path::new(&path)))
    }

    pub fn remove_dev_folder(&self, path: String) -> Result<(), DsError> {
        self.with(|h| h.remove_dev_folder(&path))
    }

    pub fn dev_folders(&self) -> Vec<String> {
        self.lock().dev_folders().to_vec()
    }

    pub fn set_enabled(&self, id: String, enabled: bool) -> Result<(), DsError> {
        self.with(|h| h.set_enabled(&id, enabled))
    }

    /// Accept a changed or hand-placed plug-in's files as they are now.
    pub fn approve_files(&self, id: String) -> Result<(), DsError> {
        self.with(|h| h.approve_files(&id))
    }

    /// Grant a permission. `folder` is the folder the user chose for `files.*`;
    /// `protected` are folders no grant may reach (the app passes its session roots).
    pub fn grant(
        &self,
        id: String,
        permission: String,
        folder: Option<String>,
        protected: Vec<String>,
    ) -> Result<(), DsError> {
        let protected: Vec<PathBuf> = protected.into_iter().map(PathBuf::from).collect();
        self.with(|h| {
            h.grant(
                &id,
                &permission,
                folder.as_deref().map(Path::new),
                &protected,
            )
        })
    }

    pub fn revoke(&self, id: String, permission: String) -> Result<(), DsError> {
        self.with(|h| h.revoke(&id, &permission))
    }

    pub fn trust_signer(&self, key: String, name: String) -> Result<(), DsError> {
        self.with(|h| h.trust_signer(&key, &name))
    }

    pub fn untrust_signer(&self, key: String) -> Result<(), DsError> {
        self.with(|h| h.untrust_signer(&key))
    }

    /// The installed plug-in's folder, for the app to move to the Trash (then `forget`).
    pub fn user_folder(&self, id: String) -> Result<String, DsError> {
        self.with(|h| h.user_folder(&id).map(|p| p.display().to_string()))
    }

    /// Look at the folders again (after a removal, or a change on disk).
    pub fn reload(&self) {
        self.lock().reload();
    }

    pub fn data_packs(&self, kind: PluginDataKind) -> PluginDataPacks {
        let kind = match kind {
            PluginDataKind::Looks => Kind::Looks,
            PluginDataKind::Styles => Kind::Styles,
            PluginDataKind::VoicePhrases => Kind::VoicePhrases,
        };
        let h = self.lock();
        let (data, errors) = h.data(kind);
        let packs = data
            .into_iter()
            .map(|(plugin_id, contribution_id, json)| PluginDataPack {
                plugin_name: h
                    .active(&plugin_id)
                    .and_then(|p| p.manifest.as_ref().map(|m| m.name.clone()))
                    .unwrap_or_else(|| plugin_id.clone()),
                plugin_id,
                contribution_id,
                json,
            })
            .collect();
        PluginDataPacks { packs, errors }
    }

    /// The active export post-processors, for the recipe editor.
    pub fn post_processors(&self) -> Vec<PostProcessorInfo> {
        let h = self.lock();
        h.contributions(Kind::ExportPostProcess)
            .map(|(p, c)| PostProcessorInfo {
                plugin_id: p.id().unwrap_or_default().to_owned(),
                plugin_version: p
                    .manifest
                    .as_ref()
                    .map(|m| m.version.to_string())
                    .unwrap_or_default(),
                plugin_name: p
                    .manifest
                    .as_ref()
                    .map(|m| m.name.clone())
                    .unwrap_or_default(),
                contribution_id: c.id.clone(),
                name: c.name.clone(),
                params: c.params.iter().map(param_info).collect(),
            })
            .collect()
    }
}

/// One post-process step of an export recipe.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ExportPostProcessStep {
    pub plugin_id: String,
    pub contribution_id: String,
    /// The recipe's values by param key, as a JSON object (missing ones take defaults).
    pub params_json: String,
    /// The plug-in version the recipe was set up with (the editor warns after an update).
    #[uniffi(default = None)]
    pub plugin_version: Option<String>,
}

/// A plug-in post-processor bound to one recipe's values.
#[derive(Debug)]
pub(crate) struct PluginStep {
    runner: Arc<PostProcessor>,
    params: Map<String, Value>,
    /// The photo's metadata goes along only with `photo_metadata`.
    metadata: bool,
}

/// The recipe's steps, resolved through the current host; a missing or inactive
/// plug-in, or a bad value, is an error naming it.
pub(crate) fn resolve_steps(
    steps: &[ExportPostProcessStep],
) -> Result<Vec<Arc<dyn PostProcess + Send + Sync>>, String> {
    if steps.is_empty() {
        return Ok(Vec::new());
    }
    let host = current()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let host = host.ok_or("plug-ins aren't loaded")?;
    let mut h = host.lock();
    let mut out: Vec<Arc<dyn PostProcess + Send + Sync>> = Vec::new();
    for s in steps {
        let (runner, declared, _version) = h
            .post_processor(&s.plugin_id, &s.contribution_id)
            .map_err(|_| {
                format!(
                    "the plug-in step {}/{} isn't available (disabled or removed)",
                    s.plugin_id, s.contribution_id
                )
            })?;
        let given: Map<String, Value> = if s.params_json.trim().is_empty() {
            Map::new()
        } else {
            serde_json::from_str(&s.params_json).map_err(|e| format!("{}: {e}", runner.name))?
        };
        // A value a plug-in update no longer allows is clamped, not refused (the recipe
        // editor shows the same note).
        let (params, _notes) = params::resolve_lenient(&declared, &given);
        let metadata = h.granted(&s.plugin_id).contains(&Permission::PhotoMetadata);
        out.push(Arc::new(PluginStep {
            runner,
            params,
            metadata,
        }));
    }
    Ok(out)
}

fn space_name(s: OutputSpace) -> &'static str {
    match s {
        OutputSpace::Srgb => "srgb",
        OutputSpace::DisplayP3 => "display-p3",
        OutputSpace::Rec2020 => "rec2020",
    }
}

impl PluginStep {
    /// The call's JSON besides the params: the output, and the photo's metadata with
    /// `photo_metadata`.
    fn context(&self, space: OutputSpace, item: &ExportItem) -> Value {
        let mut ctx = json!({"output": {"space": space_name(space)}});
        if self.metadata {
            let m = &item.meta;
            ctx["photo"] = json!({
                "name": item.name, "make": m.make, "model": m.model, "lens": m.lens_model,
                "iso": m.iso, "keywords": m.keywords,
            });
        }
        ctx
    }
}

impl PostProcess for PluginStep {
    fn name(&self) -> &str {
        &self.runner.name
    }

    fn layout(
        &self,
        size: Size,
        space: OutputSpace,
        item: &ExportItem,
    ) -> Result<CanvasLayout, ExportError> {
        let l = self
            .runner
            .layout(
                size.width,
                size.height,
                &self.params,
                &self.context(space, item),
            )
            .map_err(|e| ExportError::PostProcess(e.to_string()))?;
        Ok((Size::new(l.width, l.height), (l.x, l.y)))
    }

    fn apply(
        &self,
        canvas: Canvas<'_>,
        photo: PhotoRect,
        space: OutputSpace,
        item: &ExportItem,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), ExportError> {
        let ctx = self.context(space, item);
        let (pixels, size) = match canvas {
            Canvas::Rgba8 { size, data } => (Pixels::Rgba8(data), size),
            Canvas::Rgb16(img) => (Pixels::Rgb16(&mut img.data), img.size),
        };
        self.runner
            .run(
                pixels,
                size.width,
                size.height,
                [photo.x, photo.y, photo.width, photo.height],
                self.params.clone(),
                &ctx,
                cancelled,
            )
            .map(drop)
            .map_err(|e| ExportError::PostProcess(e.to_string()))
    }
}

/// One key of the edit vocabulary (`ds_core::edit_keys`), for the app's drift test.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct EditKeyInfo {
    pub key: String,
    /// The app's `AdjustGroup` raw value.
    pub group: String,
    /// `amount`, `choice`, `wheel`, `flag` or `index`.
    pub kind: String,
    pub min: f64,
    pub max: f64,
}

/// Every key presets, looks and plug-in packs may use, with its group, kind and range.
#[uniffi::export]
pub fn edit_keys() -> Vec<EditKeyInfo> {
    use ds_core::edit_keys::KeyKind;
    ds_core::edit_keys::edit_keys()
        .iter()
        .map(|k| EditKeyInfo {
            key: k.key.clone(),
            group: k.group.into(),
            kind: match k.kind {
                KeyKind::Amount => "amount",
                KeyKind::Choice => "choice",
                KeyKind::WheelPolar => "wheel",
                KeyKind::Flag => "flag",
                KeyKind::Index => "index",
            }
            .into(),
            min: k.min,
            max: k.max,
        })
        .collect()
}

#[cfg(test)]
#[path = "plugins_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "plugins_e2e_tests.rs"]
mod e2e_tests;
