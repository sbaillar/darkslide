//! The registry: every plug-in found (bundled, installed and development folders), what
//! the user decided about each, and the contributions of the ones that are active.
//!
//! A plug-in is **active** when its package validates for this platform, it is
//! enabled, and:
//! - bundled: its files are exactly the ones compiled into this build (`builtin.rs`);
//! - installed: its files match what was installed or approved, it is signed by the
//!   key first installed under its id (or still unsigned), its code is trusted (a
//!   trusted signer, or unsigned with "Allow unsigned code plug-ins"), and every
//!   permission it asks for is granted;
//! - development: as installed, but read from the author's folder and never pinned,
//!   so it reloads without re-approval (shown as DEV; needs unsigned code allowed).
//!
//! Anything else leaves it listed with the reason, contributing nothing.
//!
//! Threat model: a plug-in's own code (sandboxed WebAssembly) and its package (signed or
//! not) are untrusted. Other software running as the same user is out of scope: it can
//! rewrite `state.json` and the plug-in folders like any of the user's files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::manifest::{BUILTIN_PREFIX, Contribution, Kind, Manifest, Permission, Platform};
use crate::package::Package;
use crate::state::{PluginState, State};
use crate::trust::{self, FileHashes, Signer};
use crate::wasm::{PostProcessor, WasmModule};
use crate::{PluginError, pack, params};

pub const MANIFEST_FILE: &str = "plugin.toml";
pub const PLUGIN_EXT: &str = "dsplugin";
const REPLACED_DIR: &str = ".replaced";
const STAGING_PREFIX: &str = ".staging-";
/// The pinned signer of an unsigned plug-in.
const UNSIGNED: &str = "unsigned";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// In the app bundle, verified against the hashes compiled into the app.
    BuiltIn,
    /// In `<support>/Plugins`.
    User,
    /// A development folder, loaded in place.
    Dev,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trust {
    BuiltIn,
    Signed(Signer),
    /// Signed by a key the user hasn't trusted (yet).
    Untrusted(Signer),
    Unsigned,
}

impl Trust {
    fn from_signer(signer: Option<Signer>, trusted: &BTreeMap<String, String>) -> Self {
        match signer {
            Some(s) if trusted.contains_key(&s.key) => Self::Signed(s),
            Some(s) => Self::Untrusted(s),
            None => Self::Unsigned,
        }
    }

    /// The key pinned per id: the signer's key, or "unsigned".
    fn pin(&self) -> String {
        match self {
            Self::Signed(s) | Self::Untrusted(s) => s.key.clone(),
            Self::BuiltIn | Self::Unsigned => UNSIGNED.into(),
        }
    }

    fn is_trusted_code(&self) -> bool {
        matches!(self, Self::BuiltIn | Self::Signed(_))
    }
}

/// Why a plug-in isn't active (or that it is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Active,
    Disabled,
    /// Waiting for the user: each entry is one decision.
    NeedsApproval(Vec<Approval>),
    /// Another folder with the same id (and signer) wins: a newer version, the
    /// development copy, or the managed copy.
    Shadowed {
        by: PathBuf,
    },
    /// Not for this platform.
    Unsupported(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// Its files changed since they were installed or approved.
    Changed,
    /// Put in the folder by hand: never installed or approved.
    NotInstalled,
    TrustSigner(Signer),
    AllowUnsignedCode,
    Grant(Permission),
}

impl std::fmt::Display for Approval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Changed => f.write_str("its files changed since it was installed"),
            Self::NotInstalled => f.write_str("it was added to the Plugins folder by hand"),
            Self::TrustSigner(s) => {
                write!(f, "its signer (key {}) isn't trusted yet", s.fingerprint())
            }
            Self::AllowUnsignedCode => {
                f.write_str("it runs code and isn't signed by someone you trust")
            }
            Self::Grant(p) => write!(f, "permission {p} isn't granted"),
        }
    }
}

/// One plug-in package.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub dir: PathBuf,
    pub origin: Origin,
    /// `None` when the manifest couldn't be read (see `status`).
    pub manifest: Option<Manifest>,
    /// The files, read once (what was verified is what is used).
    pub package: Arc<Package>,
    pub files: FileHashes,
    pub trust: Trust,
    pub status: Status,
}

impl Plugin {
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.manifest.as_ref().map(|m| m.id.as_str())
    }
}

/// How an install went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed {
        id: String,
        version: String,
    },
    Updated {
        id: String,
        from: String,
        to: String,
    },
    /// The same version with the same files is already there.
    AlreadyInstalled {
        id: String,
        version: String,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct InstallOptions {
    /// Replace a newer installed version with this older one.
    pub allow_downgrade: bool,
}

/// A package copied into staging and validated there, for the confirmation sheet.
#[derive(Debug, Clone)]
pub struct Preview {
    /// Pass to `install_staged` or `discard`.
    pub token: String,
    pub manifest: Manifest,
    pub trust: Trust,
    /// The installed version this replaces.
    pub replaces: Option<semver::Version>,
}

#[derive(Debug)]
struct Staged {
    dir: PathBuf,
    package: Package,
    manifest: Manifest,
    trust: Trust,
    at: std::time::Instant,
}

/// How long a staged copy waits for the install sheet's answer.
pub const STAGING_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

#[derive(Debug)]
pub struct Host {
    builtin_dir: Option<PathBuf>,
    builtin_hashes: BTreeMap<String, FileHashes>,
    user_dir: PathBuf,
    darkslide: semver::Version,
    platform: Platform,
    state: State,
    plugins: Vec<Plugin>,
    staged: BTreeMap<String, Staged>,
    post_processors: BTreeMap<(String, String), Arc<PostProcessor>>,
    /// A problem with the settings file, reported once in the Plugins pane.
    pub state_error: Option<String>,
}

impl Host {
    /// Load every plug-in. `builtin_dir` is the app bundle's `Plugins` folder,
    /// `user_dir` `<support>/Plugins`. Bundled folders are checked against this build.
    pub fn open(
        builtin_dir: Option<PathBuf>,
        user_dir: PathBuf,
        darkslide: semver::Version,
        platform: Platform,
    ) -> Self {
        Self::open_with(
            builtin_dir,
            crate::builtin::embedded(),
            user_dir,
            darkslide,
            platform,
        )
    }

    /// As [`Self::open`], trusting `builtin_hashes` for the bundled folders.
    pub fn open_with(
        builtin_dir: Option<PathBuf>,
        builtin_hashes: BTreeMap<String, FileHashes>,
        user_dir: PathBuf,
        darkslide: semver::Version,
        platform: Platform,
    ) -> Self {
        let (state, state_error) = State::load(&user_dir);
        let mut h = Self {
            builtin_dir,
            builtin_hashes,
            user_dir,
            darkslide,
            platform,
            state,
            plugins: Vec::new(),
            staged: BTreeMap::new(),
            post_processors: BTreeMap::new(),
            state_error,
        };
        h.clean_leftovers();
        h.reload();
        h
    }

    #[must_use]
    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    #[must_use]
    pub fn allow_unsigned_code(&self) -> bool {
        self.state.allow_unsigned_code
    }

    #[must_use]
    pub fn trusted_signers(&self) -> &BTreeMap<String, String> {
        &self.state.trusted_signers
    }

    #[must_use]
    pub fn dev_folders(&self) -> &[String] {
        &self.state.dev_folders
    }

    /// Where a plug-in's decisions are kept: its id, or `dev:<id>` for a development
    /// folder, which never shares an installed plug-in's grants or approvals.
    fn state_key(p: &Plugin, id: &str) -> String {
        if p.origin == Origin::Dev {
            format!("dev:{id}")
        } else {
            id.to_owned()
        }
    }

    /// The state key of the plug-in `id` that is in effect (the clash winner).
    fn key(&self, id: &str) -> String {
        self.installed(id)
            .map_or_else(|_| id.to_owned(), |p| Self::state_key(p, id))
    }

    /// The permissions granted to the plug-in `id` in effect.
    #[must_use]
    pub fn granted(&self, id: &str) -> BTreeSet<Permission> {
        self.granted_at(&self.key(id))
    }

    /// The permissions granted to this package (an installed copy and a development
    /// folder with one id are kept apart).
    #[must_use]
    pub fn granted_to(&self, p: &Plugin) -> BTreeSet<Permission> {
        p.id()
            .map(|id| self.granted_at(&Self::state_key(p, id)))
            .unwrap_or_default()
    }

    fn granted_at(&self, key: &str) -> BTreeSet<Permission> {
        self.state
            .plugins
            .get(key)
            .map(|p| {
                p.granted
                    .iter()
                    .filter_map(|g| Permission::parse(g).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The active plug-in with this id.
    #[must_use]
    pub fn active(&self, id: &str) -> Option<&Plugin> {
        self.plugins
            .iter()
            .find(|p| p.id() == Some(id) && p.status == Status::Active)
    }

    /// Look at every folder again (after an install, a removal, a decision, or a change
    /// in a development folder).
    pub fn reload(&mut self) {
        let mut found = Vec::new();
        if let Some(dir) = &self.builtin_dir {
            found.extend(list_folders(dir).into_iter().map(|d| (d, Origin::BuiltIn)));
        }
        found.extend(
            list_folders(&self.user_dir)
                .into_iter()
                .map(|d| (d, Origin::User)),
        );
        found.extend(
            self.state
                .dev_folders
                .iter()
                .map(|d| (PathBuf::from(d), Origin::Dev)),
        );
        let mut plugins: Vec<Plugin> = found
            .into_iter()
            .map(|(d, o)| self.inspect_folder(d, o))
            .collect();
        resolve_clashes(&mut plugins);
        for p in &mut plugins {
            if matches!(p.status, Status::Active) {
                p.status = self.decide(p);
            }
        }
        self.plugins = plugins;
        self.post_processors.clear();
        self.purge_replaced();
    }

    /// Read and check one folder. `status` stays `Active` when nothing is wrong with
    /// the package itself (the user's decisions come in `decide`).
    fn inspect_folder(&self, dir: PathBuf, origin: Origin) -> Plugin {
        let mut p = Plugin {
            dir,
            origin,
            manifest: None,
            package: Arc::new(Package::default()),
            files: FileHashes::new(),
            trust: Trust::Unsigned,
            status: Status::Active,
        };
        let pkg = match Package::read_dir(&p.dir) {
            Ok(pkg) => pkg,
            Err(e) => {
                p.status = Status::Error(e.to_string());
                return p;
            }
        };
        p.files = pkg.hashes();
        match inspect_package(&pkg, &self.darkslide, false) {
            Err(e) => {
                p.status = Status::Error(e.to_string());
                // Name it in the list when at least the manifest reads.
                p.manifest = pkg
                    .get(MANIFEST_FILE)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .and_then(|t| Manifest::parse(t, &self.darkslide).ok());
            }
            Ok((m, signer)) => {
                p.trust = if origin == Origin::BuiltIn {
                    Trust::BuiltIn
                } else {
                    Trust::from_signer(signer, &self.state.trusted_signers)
                };
                p.status = self.package_problem(&p, &m).unwrap_or(Status::Active);
                p.manifest = Some(m);
            }
        }
        p.package = Arc::new(pkg);
        p
    }

    /// Problems of a package that validated: reserved ids, a bundled folder that isn't
    /// this build's, a different signer than the one pinned for the id, the platform.
    fn package_problem(&self, p: &Plugin, m: &Manifest) -> Option<Status> {
        let builtin_id = m.id.starts_with(BUILTIN_PREFIX);
        if builtin_id != (p.origin == Origin::BuiltIn) {
            return Some(Status::Error(if builtin_id {
                format!(
                    "ids starting with `{BUILTIN_PREFIX}` are reserved for Darkslide's own plug-ins"
                )
            } else {
                format!("a bundled plug-in's id must start with `{BUILTIN_PREFIX}`")
            }));
        }
        if p.origin == Origin::BuiltIn {
            let folder = p
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if self.builtin_hashes.get(&folder) != Some(&p.files) {
                return Some(Status::Error(
                    "its files don't match this build of Darkslide".into(),
                ));
            }
        }
        // Installed copies and development folders alike: a dev folder can't run under an
        // installed id with another signer.
        if p.origin != Origin::BuiltIn
            && let Some(pinned) = self
                .state
                .plugins
                .get(&m.id)
                .and_then(|s| s.signer.as_ref())
            && *pinned != p.trust.pin()
        {
            return Some(Status::Error(format!(
                "signed by a different key ({}) than the installed {} ({}); remove that one first",
                pin_label(&p.trust.pin()),
                m.name,
                pin_label(pinned)
            )));
        }
        if !m.platforms.contains(&self.platform) {
            return Some(Status::Unsupported(format!(
                "not available on {:?}",
                self.platform
            )));
        }
        None
    }

    /// The user's decisions for a plug-in whose package is fine.
    fn decide(&self, p: &Plugin) -> Status {
        let Some(m) = &p.manifest else {
            return Status::Error("no manifest".into());
        };
        if p.origin == Origin::BuiltIn {
            let enabled = self.state.plugins.get(&m.id).is_none_or(|s| s.enabled);
            return if enabled {
                Status::Active
            } else {
                Status::Disabled
            };
        }
        let st = self.state.plugins.get(&Self::state_key(p, &m.id));
        if st.is_some_and(|s| !s.enabled) {
            return Status::Disabled;
        }
        let mut needs = Vec::new();
        if p.origin == Origin::User {
            match st.and_then(|s| s.files.as_ref()) {
                None => needs.push(Approval::NotInstalled),
                Some(f) if *f != p.files => needs.push(Approval::Changed),
                Some(_) => {}
            }
        }
        if (m.has_code() || p.origin == Origin::Dev) && !p.trust.is_trusted_code() {
            // Signed by an untrusted key: trust the key. Unsigned: allow this plug-in
            // (or every one, with the authors' switch). Never one click for all.
            match &p.trust {
                Trust::Untrusted(s) => needs.push(Approval::TrustSigner(s.clone())),
                _ if self.state.allow_unsigned_code || st.is_some_and(|s| s.unsigned_ok) => {}
                _ => needs.push(Approval::AllowUnsignedCode),
            }
        }
        for perm in &m.permissions {
            if !st.is_some_and(|s| s.granted.contains(&perm.to_string())) {
                needs.push(Approval::Grant(perm.clone()));
            }
        }
        if needs.is_empty() {
            Status::Active
        } else {
            Status::NeedsApproval(needs)
        }
    }

    fn save(&self) -> Result<(), PluginError> {
        self.state.save(&self.user_dir)
    }

    fn installed(&self, id: &str) -> Result<&Plugin, PluginError> {
        self.plugins
            .iter()
            .find(|p| {
                p.id() == Some(id)
                    && !matches!(p.status, Status::Shadowed { .. } | Status::Error(_))
            })
            .ok_or_else(|| PluginError::NotFound(id.to_owned()))
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<(), PluginError> {
        self.installed(id)?;
        let key = self.key(id);
        self.state.plugin(&key).enabled = enabled;
        self.save()?;
        self.reload();
        Ok(())
    }

    /// Accept a plug-in's files as they are now (after a change, or one put in the
    /// folder by hand), pinning its signer. Grants nothing.
    pub fn approve_files(&mut self, id: &str) -> Result<(), PluginError> {
        let p = self.installed(id)?;
        if p.origin != Origin::User {
            return Ok(());
        }
        let (files, version, pin) = (
            p.files.clone(),
            p.manifest.as_ref().map(|m| m.version.to_string()),
            p.trust.pin(),
        );
        let st = self.state.plugin(id);
        st.files = Some(files);
        st.version = version;
        st.signer = Some(pin);
        st.enabled = true;
        self.save()?;
        self.reload();
        Ok(())
    }

    /// Grant one permission the plug-in asks for, never implicitly.
    /// - `files.read`/`files.write` need the folder the user chose: refused when it is,
    ///   contains or lies inside the support folder, the app, a bundled or development
    ///   plug-in folder, or one of `protected` (the app passes its session roots).
    /// - Files, network and accounts are refused to code that isn't signed by a trusted
    ///   key unless "Allow unsigned code plug-ins" is on.
    pub fn grant(
        &mut self,
        id: &str,
        permission: &str,
        folder: Option<&Path>,
        protected: &[PathBuf],
    ) -> Result<(), PluginError> {
        let perm = Permission::parse(permission).map_err(PluginError::Permission)?;
        let p = self.installed(id)?;
        let m = p
            .manifest
            .as_ref()
            .ok_or_else(|| PluginError::NotFound(id.to_owned()))?;
        if !m.permissions.contains(&perm) {
            return Err(PluginError::Permission(format!(
                "{} doesn't ask for {perm}",
                m.name
            )));
        }
        if perm.is_sensitive()
            && !p.trust.is_trusted_code()
            && m.has_code()
            && !self.state.allow_unsigned_code
        {
            return Err(PluginError::Permission(format!(
                "{} isn't signed by someone you trust, so it can't be given {perm} (Settings ▸ Plugins ▸ Allow unsigned code plug-ins)",
                m.name
            )));
        }
        let folder = match (&perm, folder) {
            (Permission::FilesRead(_) | Permission::FilesWrite(_), Some(f)) => {
                Some(check_folder(f, &self.protected_roots(protected))?)
            }
            (Permission::FilesRead(_) | Permission::FilesWrite(_), None) => {
                return Err(PluginError::Permission(format!(
                    "{perm} needs a folder you choose"
                )));
            }
            (_, Some(_)) => return Err(PluginError::Permission(format!("{perm} takes no folder"))),
            (_, None) => None,
        };
        // A first decision about a plug-in (a development folder) leaves it enabled.
        let st = self
            .state
            .plugins
            .entry(self.key(id))
            .or_insert_with(|| PluginState {
                enabled: true,
                ..PluginState::default()
            });
        st.granted.insert(perm.to_string());
        if let Some(f) = folder {
            st.folders.insert(perm.to_string(), f.display().to_string());
        }
        self.save()?;
        self.reload();
        Ok(())
    }

    /// The folder granted for a `files.*` permission, checked again now (a link swapped
    /// in since, or a folder moved under a protected root, is refused).
    pub fn granted_folder(
        &self,
        id: &str,
        permission: &str,
        protected: &[PathBuf],
    ) -> Result<PathBuf, PluginError> {
        let f = self
            .state
            .plugins
            .get(&self.key(id))
            .filter(|s| s.granted.contains(permission))
            .and_then(|s| s.folders.get(permission))
            .ok_or_else(|| PluginError::Permission(format!("{permission} isn't granted")))?;
        let checked = check_folder(Path::new(f), &self.protected_roots(protected))?;
        if checked != Path::new(f) {
            return Err(PluginError::Permission(format!(
                "{permission}: the folder changed since it was granted"
            )));
        }
        Ok(checked)
    }

    fn protected_roots(&self, extra: &[PathBuf]) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = extra.to_vec();
        roots.push(self.user_dir.clone());
        if let Some(support) = self.user_dir.parent() {
            roots.push(support.to_path_buf());
        }
        if let Some(b) = &self.builtin_dir {
            roots.push(b.clone());
            // The app bundle around `Contents/Resources/Plugins`.
            if let Some(app) = b
                .ancestors()
                .find(|a| a.extension().is_some_and(|e| e == "app"))
            {
                roots.push(app.to_path_buf());
            }
        }
        roots.extend(self.state.dev_folders.iter().map(PathBuf::from));
        roots
    }

    pub fn revoke(&mut self, id: &str, permission: &str) -> Result<(), PluginError> {
        let key = self.key(id);
        if let Some(st) = self.state.plugins.get_mut(&key) {
            st.granted.remove(permission);
            st.folders.remove(permission);
        }
        self.save()?;
        self.reload();
        Ok(())
    }

    /// Trust plug-ins signed with this key (hex). The key, not the name it claims, is
    /// the identity; the app shows its fingerprint.
    pub fn trust_signer(&mut self, key: &str, name: &str) -> Result<(), PluginError> {
        if trust::unhex(key).is_none_or(|b| b.len() != 32) {
            return Err(PluginError::Invalid("not an Ed25519 public key".into()));
        }
        self.state
            .trusted_signers
            .insert(key.to_owned(), crate::text::clean(name, 60));
        self.save()?;
        self.reload();
        Ok(())
    }

    pub fn untrust_signer(&mut self, key: &str) -> Result<(), PluginError> {
        self.state.trusted_signers.remove(key);
        self.save()?;
        self.reload();
        Ok(())
    }

    /// Allow this one unsigned code plug-in to run (its Allow button). Its file, network
    /// and account permissions still need "Allow unsigned code plug-ins"; an update
    /// (which could come from anyone, being unsigned) asks again.
    pub fn allow_unsigned(&mut self, id: &str) -> Result<(), PluginError> {
        let p = self.installed(id)?;
        if !matches!(p.trust, Trust::Unsigned) || p.origin == Origin::BuiltIn {
            return Err(PluginError::Permission(format!(
                "{id} isn't an unsigned plug-in"
            )));
        }
        let key = self.key(id);
        self.state.plugin(&key).unsigned_ok = true;
        self.save()?;
        self.reload();
        Ok(())
    }

    /// "Allow unsigned code plug-ins". Turning it off takes every sensitive grant back
    /// from code that isn't signed by a trusted key.
    pub fn set_allow_unsigned_code(&mut self, allow: bool) -> Result<(), PluginError> {
        self.state.allow_unsigned_code = allow;
        if !allow {
            for p in &self.plugins {
                let Some(m) = &p.manifest else { continue };
                if m.has_code()
                    && !p.trust.is_trusted_code()
                    && let Some(st) = self.state.plugins.get_mut(&Self::state_key(p, &m.id))
                {
                    st.granted
                        .retain(|g| Permission::parse(g).is_ok_and(|p| !p.is_sensitive()));
                    st.folders.retain(|g, _| st.granted.contains(g));
                }
            }
        }
        self.save()?;
        self.reload();
        Ok(())
    }

    /// Load a development folder in place (needs "Allow unsigned code plug-ins").
    pub fn add_dev_folder(&mut self, folder: &Path) -> Result<(), PluginError> {
        if !self.state.allow_unsigned_code {
            return Err(PluginError::Permission(
                "development folders need “Allow unsigned code plug-ins”".into(),
            ));
        }
        let dir = folder.canonicalize()?;
        if !dir.join(MANIFEST_FILE).is_file() {
            return Err(PluginError::Invalid(format!(
                "{} has no {MANIFEST_FILE}",
                dir.display()
            )));
        }
        if self
            .protected_roots(&[])
            .iter()
            .filter(|r| !self.state.dev_folders.contains(&r.display().to_string()))
            .any(|r| {
                r.canonicalize()
                    .is_ok_and(|r| dir.starts_with(&r) || r.starts_with(&dir))
            })
        {
            return Err(PluginError::Invalid(
                "a development folder can't be inside Darkslide's own folders".into(),
            ));
        }
        let d = dir.display().to_string();
        if !self.state.dev_folders.contains(&d) {
            self.state.dev_folders.push(d);
        }
        self.save()?;
        self.reload();
        Ok(())
    }

    pub fn remove_dev_folder(&mut self, folder: &str) -> Result<(), PluginError> {
        self.state.dev_folders.retain(|d| d != folder);
        // Its decisions go with it: another folder later linked with that id starts afresh.
        let ids: Vec<String> = self
            .plugins
            .iter()
            .filter(|p| p.origin == Origin::Dev && p.dir == Path::new(folder))
            .filter_map(|p| p.id().map(str::to_owned))
            .collect();
        for id in ids {
            self.state.plugins.remove(&format!("dev:{id}"));
        }
        self.save()?;
        self.reload();
        Ok(())
    }

    /// Copy the package at `src` (a folder or a `.dsplugin` zip) into staging, read it
    /// back and validate *that* copy: what the sheet shows is exactly what installs.
    pub fn stage(&mut self, src: &Path) -> Result<Preview, PluginError> {
        // Tokens expire: staged copies nobody answered are dropped.
        let expired: Vec<String> = self
            .staged
            .iter()
            .filter(|(_, st)| st.at.elapsed() > STAGING_TTL)
            .map(|(t, _)| t.clone())
            .collect();
        for t in expired {
            self.discard(&t);
        }
        let pkg = Package::read(src)?;
        fs::create_dir_all(&self.user_dir)?;
        let token = trust::hex(&rand_bytes()?);
        let dir = self.user_dir.join(format!("{STAGING_PREFIX}{token}"));
        let staged = (|| {
            pkg.write_to(&dir)?;
            let copy = Package::read_dir(&dir)?;
            if copy.hashes() != pkg.hashes() {
                return Err(PluginError::Install(
                    "the plug-in changed while it was being copied".into(),
                ));
            }
            let (manifest, signer) = inspect_package(&copy, &self.darkslide, false)?;
            if manifest.id.starts_with(BUILTIN_PREFIX) {
                return Err(PluginError::Install(format!(
                    "ids starting with `{BUILTIN_PREFIX}` are reserved for Darkslide's own plug-ins"
                )));
            }
            let trust = Trust::from_signer(signer, &self.state.trusted_signers);
            if let Some(pinned) = self
                .state
                .plugins
                .get(&manifest.id)
                .and_then(|s| s.signer.as_ref())
                && *pinned != trust.pin()
            {
                return Err(PluginError::Install(format!(
                    "{} is signed by a different key ({}) than the installed version ({}); remove the installed one first",
                    manifest.name,
                    pin_label(&trust.pin()),
                    pin_label(pinned)
                )));
            }
            Ok(Staged {
                at: std::time::Instant::now(),
                dir: dir.clone(),
                package: copy,
                manifest,
                trust,
            })
        })();
        let staged = staged.inspect_err(|_| {
            let _ = fs::remove_dir_all(&dir);
        })?;
        let replaces = self
            .plugins
            .iter()
            .filter(|p| p.origin == Origin::User && p.id() == Some(&staged.manifest.id))
            .filter_map(|p| p.manifest.as_ref().map(|m| m.version.clone()))
            .max();
        let preview = Preview {
            token: token.clone(),
            manifest: staged.manifest.clone(),
            trust: staged.trust.clone(),
            replaces,
        };
        self.staged.insert(token, staged);
        Ok(preview)
    }

    /// Drop a staged package (the sheet was cancelled).
    pub fn discard(&mut self, token: &str) {
        if let Some(s) = self.staged.remove(token) {
            let _ = fs::remove_dir_all(s.dir);
        }
    }

    /// Install a staged package as `<support>/Plugins/<id>.dsplugin`.
    ///
    /// - A newer installed version is kept unless `allow_downgrade`.
    /// - The same version is a no-op when the files match, refused when they don't
    ///   (bump the version).
    /// - The old folder is moved to `.replaced/` and removed once the new one loaded.
    /// - New plug-ins start enabled with nothing granted; an update keeps the grants
    ///   the new version still asks for (file, network and account ones only when signed
    ///   by the pinned key), and pins the same signer.
    pub fn install_staged(
        &mut self,
        token: &str,
        opts: InstallOptions,
    ) -> Result<InstallOutcome, PluginError> {
        let staged = self
            .staged
            .remove(token)
            .ok_or_else(|| PluginError::NotFound("that staged plug-in".into()))?;
        let r = self.install_staged_inner(&staged, opts);
        if !matches!(
            r,
            Ok(InstallOutcome::Installed { .. } | InstallOutcome::Updated { .. })
        ) {
            let _ = fs::remove_dir_all(&staged.dir);
        }
        r
    }

    /// A staged copy can still be installed: not expired, and the pinned signer is checked
    /// again (something may have been installed under its id since it was staged).
    fn still_valid(&self, s: &Staged) -> Result<(), PluginError> {
        let m = &s.manifest;
        if s.at.elapsed() > STAGING_TTL {
            return Err(PluginError::Install(
                "the install sheet waited too long; add the plug-in again".into(),
            ));
        }
        if let Some(pinned) = self
            .state
            .plugins
            .get(&m.id)
            .and_then(|st| st.signer.as_ref())
            && *pinned != s.trust.pin()
        {
            return Err(PluginError::Install(format!(
                "{} is signed by a different key ({}) than the installed version ({}); remove the installed one first",
                m.name,
                pin_label(&s.trust.pin()),
                pin_label(pinned)
            )));
        }
        Ok(())
    }

    fn install_staged_inner(
        &mut self,
        s: &Staged,
        opts: InstallOptions,
    ) -> Result<InstallOutcome, PluginError> {
        let m = &s.manifest;
        self.still_valid(s)?;
        let new_files = s.package.hashes();
        let dest = self.user_dir.join(format!("{}.{PLUGIN_EXT}", m.id));
        let existing = self
            .plugins
            .iter()
            .find(|p| {
                p.origin == Origin::User
                    && p.id() == Some(&m.id)
                    && !matches!(p.status, Status::Shadowed { .. })
            })
            .and_then(|p| {
                p.manifest
                    .as_ref()
                    .map(|em| (em.version.clone(), p.files.clone()))
            });
        if let Some((old, files)) = &existing {
            if *old > m.version && !opts.allow_downgrade {
                return Err(PluginError::Install(format!(
                    "{} {old} is installed; this is the older {} (allow the downgrade to replace it)",
                    m.name, m.version
                )));
            }
            if *old == m.version {
                if *files == new_files {
                    return Ok(InstallOutcome::AlreadyInstalled {
                        id: m.id.clone(),
                        version: m.version.to_string(),
                    });
                }
                return Err(PluginError::Install(format!(
                    "{} {old} is installed with different files; a changed plug-in needs a new version number",
                    m.name
                )));
            }
        }
        let mut aside = None;
        if dest.exists() {
            let replaced = self.user_dir.join(REPLACED_DIR);
            fs::create_dir_all(&replaced)?;
            let to = replaced.join(format!("{}@{}", m.id, trust::hex(&rand_bytes()?)));
            fs::rename(&dest, &to)?;
            aside = Some(to);
        }
        if let Err(e) = ds_fsutil::rename_noreplace(&s.dir, &dest) {
            if let Some(a) = &aside {
                let _ = fs::rename(a, &dest);
            }
            return Err(e.into());
        }
        let prev = self
            .state
            .plugins
            .get(&m.id)
            .cloned()
            .unwrap_or(PluginState {
                enabled: true,
                ..PluginState::default()
            });
        let mut asked: BTreeSet<String> = m.permissions.iter().map(ToString::to_string).collect();
        // File, network and account grants carry over only from the same pinned key: an
        // unsigned package with this id could have been made by anyone.
        if s.trust.pin() == UNSIGNED || prev.signer.as_ref() != Some(&s.trust.pin()) {
            asked.retain(|g| Permission::parse(g).is_ok_and(|p| !p.is_sensitive()));
        }
        let st = self.state.plugin(&m.id);
        st.files = Some(new_files);
        st.version = Some(m.version.to_string());
        st.signer = Some(s.trust.pin());
        st.enabled = prev.enabled || existing.is_none();
        // New unsigned code is approved again (nothing ties it to the previous author).
        st.unsigned_ok = false;
        st.granted = prev.granted.intersection(&asked).cloned().collect();
        st.folders = prev
            .folders
            .into_iter()
            .filter(|(k, _)| asked.contains(k))
            .collect();
        self.save()?;
        self.reload();
        Ok(match existing {
            None => InstallOutcome::Installed {
                id: m.id.clone(),
                version: m.version.to_string(),
            },
            Some((old, _)) => InstallOutcome::Updated {
                id: m.id.clone(),
                from: old.to_string(),
                to: m.version.to_string(),
            },
        })
    }

    /// Stage and install in one step (the CLI and tests; the app confirms in between).
    pub fn install(
        &mut self,
        src: &Path,
        opts: InstallOptions,
    ) -> Result<InstallOutcome, PluginError> {
        let p = self.stage(src)?;
        self.install_staged(&p.token, opts)
    }

    /// The folder of the installed plug-in `id`, for the app to move to the Trash
    /// (then `forget`). Bundled plug-ins can only be disabled.
    pub fn user_folder(&self, id: &str) -> Result<PathBuf, PluginError> {
        // The installed copy (even when a development folder shadows it), else a bundled one.
        let p = self
            .plugins
            .iter()
            .filter(|p| p.id() == Some(id) && p.origin != Origin::Dev)
            .min_by_key(|p| {
                (
                    p.origin != Origin::User,
                    matches!(p.status, Status::Shadowed { .. } | Status::Error(_)),
                )
            })
            .ok_or_else(|| PluginError::NotFound(id.to_owned()))?;
        if p.origin == Origin::BuiltIn {
            return Err(PluginError::Install(
                "Darkslide's own plug-ins can be disabled, not removed".into(),
            ));
        }
        Ok(p.dir.clone())
    }

    /// After a removal: drop the id's grants, pinned files and pinned signer (a package
    /// put back later is a new install); keep whether it was enabled.
    pub fn forget(&mut self, id: &str) -> Result<(), PluginError> {
        // A copy kept aside by an earlier update must not come back at the next open.
        let prefix = format!("{id}@");
        for d in fs::read_dir(self.user_dir.join(REPLACED_DIR))
            .into_iter()
            .flatten()
            .flatten()
        {
            if d.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = fs::remove_dir_all(d.path());
            }
        }
        if let Some(st) = self.state.plugins.get_mut(id) {
            let enabled = st.enabled;
            *st = PluginState {
                enabled,
                ..PluginState::default()
            };
        }
        self.save()?;
        self.reload();
        Ok(())
    }

    /// Active contributions of `kind`, with their plug-in.
    pub fn contributions(&self, kind: Kind) -> impl Iterator<Item = (&Plugin, &Contribution)> {
        self.plugins
            .iter()
            .filter(|p| p.status == Status::Active)
            .flat_map(move |p| {
                p.manifest
                    .iter()
                    .flat_map(|m| &m.contributions)
                    .filter(move |c| c.kind == kind)
                    .map(move |c| (p, c))
            })
    }

    /// The normalized data of every active contribution of a data `kind`: (plug-in,
    /// contribution, JSON), and the problems (a file that didn't load, items left out).
    #[must_use]
    pub fn data(&self, kind: Kind) -> (Vec<(String, String, String)>, Vec<String>) {
        let (mut out, mut errors) = (Vec::new(), Vec::new());
        for (p, c) in self.contributions(kind) {
            let id = p.id().unwrap_or_default();
            let name = p.manifest.as_ref().map_or(id, |m| &m.name);
            let builtin = p.origin == Origin::BuiltIn;
            match p
                .package
                .get(&c.file)
                .map(|b| pack::normalize(kind, id, builtin, b))
            {
                Some(Ok(n)) => {
                    errors.extend(
                        n.skipped
                            .iter()
                            .map(|s| format!("{name} ({}): {s}", c.file)),
                    );
                    out.push((id.to_owned(), c.id.clone(), n.json));
                }
                Some(Err(e)) => errors.push(format!("{name} ({}): {e}", c.file)),
                None => errors.push(format!("{name}: `{}` is missing", c.file)),
            }
        }
        (out, errors)
    }

    /// The post-processor `plugin`/`contribution` and its declared params, with the
    /// plug-in's version; compiled once per load.
    pub fn post_processor(
        &mut self,
        plugin: &str,
        contribution: &str,
    ) -> Result<(Arc<PostProcessor>, Vec<params::Param>, String), PluginError> {
        let (p, c) = self
            .contributions(Kind::ExportPostProcess)
            .find(|(p, c)| p.id() == Some(plugin) && c.id == contribution)
            .ok_or_else(|| PluginError::NotFound(format!("{plugin}/{contribution}")))?;
        let key = (plugin.to_owned(), contribution.to_owned());
        let params = c.params.clone();
        let version = p
            .manifest
            .as_ref()
            .map(|m| m.version.to_string())
            .unwrap_or_default();
        if let Some(pp) = self.post_processors.get(&key) {
            return Ok((pp.clone(), params, version));
        }
        let bytes = p
            .package
            .get(&c.file)
            .ok_or_else(|| PluginError::NotFound(c.file.clone()))?;
        let module = WasmModule::compile(bytes)?;
        let pp = Arc::new(PostProcessor::new(
            &c.name,
            module,
            c.entry.as_deref().unwrap_or("ds_post_process"),
            c.apron_rows,
        )?);
        self.post_processors.insert(key, pp.clone());
        Ok((pp, params, version))
    }

    /// Remove our own leftovers: interrupted stagings,
    /// and a version moved aside by an install that crashed before the new one was in
    /// place is put back.
    fn clean_leftovers(&self) {
        for d in fs::read_dir(&self.user_dir).into_iter().flatten().flatten() {
            // Only stagings older than any sheet could be: another host on this folder
            // may have one open.
            let stale = d
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age > STAGING_TTL);
            if d.file_name().to_string_lossy().starts_with(STAGING_PREFIX) && stale {
                let _ = fs::remove_dir_all(d.path());
            }
        }
        for d in fs::read_dir(self.user_dir.join(REPLACED_DIR))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = d.file_name().to_string_lossy().into_owned();
            let Some((id, _)) = name.split_once('@') else {
                continue;
            };
            let dest = self.user_dir.join(format!("{id}.{PLUGIN_EXT}"));
            if !dest.exists() {
                let _ = ds_fsutil::rename_noreplace(&d.path(), &dest);
            }
        }
    }

    /// Old versions kept in `.replaced/` go once their id loads again.
    fn purge_replaced(&self) {
        let dir = self.user_dir.join(REPLACED_DIR);
        for d in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = d.file_name().to_string_lossy().into_owned();
            let loaded = self.plugins.iter().any(|p| {
                p.origin == Origin::User
                    && p.id()
                        .is_some_and(|id| name.strip_prefix(id).is_some_and(|r| r.starts_with('@')))
                    && !matches!(p.status, Status::Error(_))
            });
            if loaded {
                let _ = fs::remove_dir_all(d.path());
            }
        }
    }
}

/// A pinned signer for messages: "unsigned", or the key's fingerprint.
fn pin_label(pin: &str) -> String {
    if pin == UNSIGNED {
        "unsigned".into()
    } else {
        format!("key {}", trust::fingerprint(pin))
    }
}

/// The real folder `f`, refused when it is, contains or lies inside any of `protected`
/// (each resolved too), or is the file system root or the home folder.
pub fn check_folder(f: &Path, protected: &[PathBuf]) -> Result<PathBuf, PluginError> {
    let real = f
        .canonicalize()
        .map_err(|e| PluginError::Permission(format!("{}: {e}", f.display())))?;
    if !real.is_dir() {
        return Err(PluginError::Permission(format!(
            "{} is not a folder",
            real.display()
        )));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|h| h.canonicalize().ok());
    if real.parent().is_none() || home.as_deref() == Some(real.as_path()) {
        return Err(PluginError::Permission(
            "choose a specific folder, not the whole disk or home folder".into(),
        ));
    }
    for r in protected {
        let r = r.canonicalize().unwrap_or_else(|_| r.clone());
        if real.starts_with(&r) || r.starts_with(&real) {
            return Err(PluginError::Permission(format!(
                "{} holds Darkslide's own files or a session; choose another folder",
                real.display()
            )));
        }
    }
    Ok(real)
}

/// Validate a package: its manifest, every file it names, every code module (compiles
/// against host API 1), its signature. `strict` (author time) also refuses unknown
/// manifest keys and any data item that would be left out.
pub fn inspect_package(
    pkg: &Package,
    darkslide: &semver::Version,
    strict: bool,
) -> Result<(Manifest, Option<Signer>), PluginError> {
    let text = pkg
        .get(MANIFEST_FILE)
        .ok_or_else(|| PluginError::Manifest(format!("{MANIFEST_FILE} is missing")))?;
    let text = std::str::from_utf8(text)
        .map_err(|_| PluginError::Manifest(format!("{MANIFEST_FILE} is not UTF-8")))?;
    let m = Manifest::parse_with(text, darkslide, strict)?;
    for c in &m.contributions {
        let bytes = pkg
            .get(&c.file)
            .ok_or_else(|| PluginError::Invalid(format!("`{}` is missing", c.file)))?;
        if c.kind.is_code() {
            let module = WasmModule::compile(bytes)?;
            if c.kind == Kind::ExportPostProcess {
                PostProcessor::new(
                    &c.name,
                    module,
                    c.entry.as_deref().unwrap_or("ds_post_process"),
                    c.apron_rows,
                )?;
            }
        } else if strict {
            let builtin = m.id.starts_with(BUILTIN_PREFIX);
            let n = pack::normalize(c.kind, &m.id, builtin, bytes)
                .map_err(|e| PluginError::Invalid(format!("{}: {e}", c.file)))?;
            if let Some(s) = n.skipped.first() {
                return Err(PluginError::Invalid(format!("{}: {s}", c.file)));
            }
        }
    }
    let signer = trust::read_signature(pkg)?;
    Ok((m, signer))
}

fn list_folders(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().is_some_and(|e| e == PLUGIN_EXT))
        .collect();
    v.sort();
    v
}

/// Two folders with one id (and one signer: a different signer is already an error):
/// a development folder wins, then the higher version, then the managed copy
/// (`<id>.dsplugin`), then the bundled one, then the first by path.
fn resolve_clashes(plugins: &mut [Plugin]) {
    let rank = |q: &Plugin| {
        let managed = q
            .dir
            .file_stem()
            .is_some_and(|s| Some(s.to_string_lossy().as_ref()) == q.id());
        (
            q.origin == Origin::Dev,
            q.manifest.as_ref().map(|m| m.version.clone()),
            managed,
            q.origin == Origin::BuiltIn,
        )
    };
    let mut best: BTreeMap<String, usize> = BTreeMap::new();
    for (i, p) in plugins.iter().enumerate() {
        let (Some(m), Status::Active | Status::Unsupported(_)) = (&p.manifest, &p.status) else {
            continue;
        };
        match best.get(&m.id) {
            Some(&j) if rank(&plugins[j]) >= rank(p) => {}
            _ => {
                best.insert(m.id.clone(), i);
            }
        }
    }
    let winners: BTreeMap<String, PathBuf> = best
        .iter()
        .map(|(id, &i)| (id.clone(), plugins[i].dir.clone()))
        .collect();
    for p in plugins.iter_mut() {
        if let (Some(id), Status::Active | Status::Unsupported(_)) = (p.id(), &p.status)
            && let Some(w) = winners.get(id)
            && *w != p.dir
        {
            p.status = Status::Shadowed { by: w.clone() };
        }
    }
}

fn rand_bytes() -> Result<[u8; 8], PluginError> {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).map_err(|e| PluginError::Invalid(format!("no random source: {e}")))?;
    Ok(b)
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
