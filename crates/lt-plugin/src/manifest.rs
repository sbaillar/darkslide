//! `plugin.toml`: parsing and validation (manifest format 1).
//!
//! Strict where a typo would pass silently (unknown top-level keys are errors), lenient
//! where a newer contract adds optional fields (unknown keys inside a contribution are
//! warnings), so an older host still loads a newer plug-in's data it understands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::Deserialize;

use crate::params::{Param, RawParam};
use crate::{PluginError, text};

/// The manifest format this host reads.
pub const MANIFEST_VERSION: u32 = 1;
/// The host API major (WebAssembly imports and exports, ABI conventions).
pub const HOST_API: u32 = 1;
/// Ids starting with this are reserved for plug-ins bundled with Luxtable.
pub const BUILTIN_PREFIX: &str = "luxtable.";
/// Migration source: the bundled plug-ins' id prefix when the app was called
/// Darkslide. Still reserved (no other plug-in may take those ids); stored state
/// under it moves to [`BUILTIN_PREFIX`] (`State::load`).
pub const LEGACY_BUILTIN_PREFIX: &str = "darkslide.";

/// Ids only the app's own (bundled) plug-ins may have.
#[must_use]
pub fn is_reserved_id(id: &str) -> bool {
    id.starts_with(BUILTIN_PREFIX) || id.starts_with(LEGACY_BUILTIN_PREFIX)
}

/// Contribution kinds and the contract range this host supports for each.
pub const CONTRACTS: &[(Kind, u32, u32)] = &[
    (Kind::Looks, 1, 1),
    (Kind::Styles, 1, 1),
    (Kind::VoicePhrases, 1, 1),
    (Kind::ExportPostProcess, 1, 1),
    (Kind::ExportDestination, 1, 1),
    (Kind::NativeFeature, 1, 1),
];

/// The app's own optional integrations a bundled `native_feature` plug-in switches on
/// and off (see docs/PLUGINS.md, "Native features"). The code is native; the plug-in is
/// the switch and the catalogue entry.
pub const NATIVE_FEATURES: &[&str] = &["archive", "amazon-photos", "immich", "google-photos"];

/// What a contribution adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Looks,
    Styles,
    VoicePhrases,
    ExportPostProcess,
    ExportDestination,
    /// One of the app's own optional integrations (bundled plug-ins only).
    NativeFeature,
}

impl Kind {
    /// The manifest's table name (`[[contributes.<name>]]`).
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Looks => "looks",
            Self::Styles => "styles",
            Self::VoicePhrases => "voice_phrases",
            Self::ExportPostProcess => "export_post_process",
            Self::ExportDestination => "export_destination",
            Self::NativeFeature => "native_feature",
        }
    }

    /// Whether it runs code (WebAssembly), as opposed to a data file.
    #[must_use]
    pub fn is_code(self) -> bool {
        matches!(self, Self::ExportPostProcess | Self::ExportDestination)
    }

    /// Whether it names a file in the package (a data file or a module). A native feature
    /// names none: its code is part of the app.
    #[must_use]
    pub fn has_file(self) -> bool {
        self != Self::NativeFeature
    }

    /// The contract range this host supports.
    #[must_use]
    pub fn contracts(self) -> (u32, u32) {
        CONTRACTS
            .iter()
            .find(|(k, ..)| *k == self)
            .map_or((0, 0), |&(_, lo, hi)| (lo, hi))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Macos,
    Ipados,
}

/// What a plug-in may be granted (see docs/PLUGINS.md, Permissions).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Permission {
    PhotoPixels,
    PhotoMetadata,
    MetadataWrite,
    FilesRead(String),
    FilesWrite(String),
    Network(String),
    Account(String),
    Clock,
}

impl Permission {
    /// Parse the manifest's spelling (`network:api.example.com`).
    pub fn parse(s: &str) -> Result<Self, String> {
        let (name, arg) = match s.split_once(':') {
            Some((n, a)) => (n, Some(a)),
            None => (s, None),
        };
        let label = |a: Option<&str>| -> Result<String, String> {
            match a {
                Some(a)
                    if !a.is_empty()
                        && a.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
                {
                    Ok(a.to_owned())
                }
                _ => Err(format!(
                    "`{s}` needs a label of letters, digits, - or _ after the colon"
                )),
            }
        };
        match (name, arg) {
            ("photo_pixels", None) => Ok(Self::PhotoPixels),
            ("photo_metadata", None) => Ok(Self::PhotoMetadata),
            ("metadata_write", None) => Ok(Self::MetadataWrite),
            ("clock", None) => Ok(Self::Clock),
            ("files.read", a) => label(a).map(Self::FilesRead),
            ("files.write", a) => label(a).map(Self::FilesWrite),
            ("account", a) => label(a).map(Self::Account),
            ("network", Some(host)) => valid_host(host)
                .then(|| Self::Network(host.to_ascii_lowercase()))
                .ok_or_else(|| format!("`{s}`: a host name like api.example.com or *.example.com (no IP, port or scheme)")),
            _ => Err(format!("unknown permission `{s}`")),
        }
    }

    /// Whether granting it reaches outside the plug-in's own inputs (files, network,
    /// accounts): never granted silently, and never to unsigned code unless the user
    /// allowed unsigned code plug-ins.
    #[must_use]
    pub fn is_sensitive(&self) -> bool {
        matches!(
            self,
            Self::FilesRead(_) | Self::FilesWrite(_) | Self::Network(_) | Self::Account(_)
        )
    }

    /// Whether `host` (lowercase) is covered by this `network:` permission.
    #[must_use]
    pub fn allows_host(&self, host: &str) -> bool {
        let Self::Network(pattern) = self else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        match pattern.strip_prefix("*.") {
            Some(base) => {
                host.len() > base.len() + 1
                    && host.ends_with(base)
                    && host[..host.len() - base.len()].ends_with('.')
            }
            None => host == *pattern,
        }
    }
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PhotoPixels => f.write_str("photo_pixels"),
            Self::PhotoMetadata => f.write_str("photo_metadata"),
            Self::MetadataWrite => f.write_str("metadata_write"),
            Self::Clock => f.write_str("clock"),
            Self::FilesRead(l) => write!(f, "files.read:{l}"),
            Self::FilesWrite(l) => write!(f, "files.write:{l}"),
            Self::Network(h) => write!(f, "network:{h}"),
            Self::Account(a) => write!(f, "account:{a}"),
        }
    }
}

fn valid_host(host: &str) -> bool {
    let bare = host.strip_prefix("*.").unwrap_or(host);
    let labels: Vec<&str> = bare.split('.').collect();
    labels.len() >= 2
        && bare.len() <= 253
        && labels.iter().all(|l| {
            !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        // No IP literals: the last label of a name is never all digits.
        && !labels.last().is_some_and(|l| l.chars().all(|c| c.is_ascii_digit()))
}

/// How an account signs in (`[accounts.<name>]`): OAuth 2.0 with PKCE, run by the host.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSpec {
    /// The service's API hosts: the only hosts its token is ever sent to (each must also
    /// be a `network:` permission). Shown at sign-in.
    pub api_hosts: Vec<String>,
    /// `oauth2-pkce` (the only one in contract 1).
    pub auth: String,
    pub authorize_url: String,
    pub token_url: String,
    #[serde(default)]
    pub revoke_url: Option<String>,
    /// A public (installed-app) client id; no client secret is ever stored.
    pub client_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// One contribution, after validation.
#[derive(Debug, Clone, PartialEq)]
pub struct Contribution {
    pub kind: Kind,
    /// Unique inside the plug-in.
    pub id: String,
    pub name: String,
    pub contract: u32,
    /// Data file (data kinds) or WebAssembly module (code kinds), relative to the folder.
    pub file: String,
    /// Code kinds: the exported function to call.
    pub entry: Option<String>,
    pub params: Vec<Param>,
    /// Destinations: the `[accounts]` it signs in with, the formats it takes.
    pub accounts: Vec<String>,
    pub formats: Vec<String>,
    pub max_file_mb: Option<u32>,
    /// Post-processors: rows of context above and below each strip (0–256).
    pub apron_rows: u32,
    /// Native features: what the feature reaches beyond the app, in words, for the
    /// Plugins pane ("Your Immich server").
    pub uses: Vec<String>,
}

/// A validated manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: semver::Version,
    pub api: u32,
    pub min_luxtable: Option<semver::Version>,
    pub authors: Vec<String>,
    pub summary: String,
    pub homepage: Option<String>,
    pub platforms: BTreeSet<Platform>,
    pub permissions: BTreeSet<Permission>,
    pub accounts: BTreeMap<String, AccountSpec>,
    pub contributions: Vec<Contribution>,
    /// Things an older host skipped (unknown keys inside contributions, unknown kinds).
    pub warnings: Vec<String>,
}

impl Manifest {
    /// Whether any contribution runs code.
    #[must_use]
    pub fn has_code(&self) -> bool {
        self.contributions.iter().any(|c| c.kind.is_code())
    }

    /// Every path the manifest names (data files and modules).
    #[must_use]
    pub fn files(&self) -> BTreeSet<&str> {
        self.contributions
            .iter()
            .filter(|c| c.kind.has_file())
            .map(|c| c.file.as_str())
            .collect()
    }

    /// The native features this plug-in switches (`[[contributes.native_feature]]` ids).
    pub fn native_features(&self) -> impl Iterator<Item = &str> {
        self.contributions
            .iter()
            .filter(|c| c.kind == Kind::NativeFeature)
            .map(|c| c.id.as_str())
    }

    /// Parse and validate `plugin.toml`'s text at load: unknown keys are warnings, so a
    /// newer optional key doesn't stop an older Luxtable. `luxtable` is the running
    /// app's version (for `min_luxtable`).
    pub fn parse(text: &str, luxtable: &semver::Version) -> Result<Self, PluginError> {
        Self::parse_with(text, luxtable, false)
    }

    /// As [`Self::parse`]; with `strict` (`lt-plugin validate` and `pack`, author time)
    /// every unknown key and every other warning is an error.
    pub fn parse_with(
        text: &str,
        luxtable: &semver::Version,
        strict: bool,
    ) -> Result<Self, PluginError> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|e| PluginError::Manifest(toml_message(&e)))?;
        let m = raw.validate(luxtable)?;
        if strict && let Some(w) = m.warnings.first() {
            return Err(PluginError::Manifest(w.clone()));
        }
        Ok(m)
    }
}

/// A TOML error as one line with its position (`line 12: …`).
fn toml_message(e: &toml::de::Error) -> String {
    let msg = e.message().trim().to_owned();
    match e.span() {
        Some(span) => format!("plugin.toml at byte {}: {msg}", span.start),
        None => format!("plugin.toml: {msg}"),
    }
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    manifest: u32,
    id: String,
    name: String,
    version: String,
    api: u32,
    /// `min_darkslide` is the same field in manifests written before the rename.
    #[serde(default, alias = "min_darkslide")]
    min_luxtable: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    platforms: Option<Vec<Platform>>,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    accounts: BTreeMap<String, AccountSpec>,
    #[serde(default)]
    contributes: BTreeMap<String, Vec<RawContribution>>,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Deserialize)]
struct RawContribution {
    id: String,
    #[serde(default)]
    name: Option<String>,
    contract: u32,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    params: Vec<RawParam>,
    #[serde(default)]
    accounts: Vec<String>,
    #[serde(default)]
    formats: Vec<String>,
    #[serde(default)]
    max_file_mb: Option<u32>,
    #[serde(default)]
    apron_rows: Option<u32>,
    #[serde(default)]
    uses: Vec<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

/// Formats an export destination may accept.
const DESTINATION_FORMATS: &[&str] = &["jpeg", "heic", "tiff", "png", "original"];

impl RawManifest {
    #[allow(clippy::too_many_lines)] // one check after another, each with its message
    fn validate(self, luxtable: &semver::Version) -> Result<Manifest, PluginError> {
        let bad = |m: String| Err(PluginError::Manifest(m));
        if self.manifest != MANIFEST_VERSION {
            return bad(format!(
                "manifest format {} is not supported (this Luxtable reads {MANIFEST_VERSION})",
                self.manifest
            ));
        }
        validate_id(&self.id).map_err(PluginError::Manifest)?;
        let name = text::clean(&self.name, text::NAME_MAX);
        if name.is_empty() || name.ends_with('…') {
            return bad(format!(
                "`name` must be 1–{} characters on one line",
                text::NAME_MAX
            ));
        }
        let builtin = self.id.starts_with(BUILTIN_PREFIX);
        if !builtin && text::reserved_name(&name) {
            return bad(format!(
                "the name “{name}” would pass for part of Luxtable; choose another"
            ));
        }
        let version = semver::Version::parse(&self.version)
            .map_err(|e| PluginError::Manifest(format!("`version` \"{}\": {e}", self.version)))?;
        if self.api != HOST_API {
            return bad(format!(
                "written for host API {}; this Luxtable provides API {HOST_API}{}",
                self.api,
                if self.api > HOST_API {
                    " (it needs a newer Luxtable)"
                } else {
                    ""
                }
            ));
        }
        let min_luxtable = match &self.min_luxtable {
            None => None,
            Some(v) => {
                let v = semver::Version::parse(v)
                    .map_err(|e| PluginError::Manifest(format!("`min_luxtable` \"{v}\": {e}")))?;
                if &v > luxtable {
                    return bad(format!("needs Luxtable {v} or newer (this is {luxtable})"));
                }
                Some(v)
            }
        };
        let mut permissions = BTreeSet::new();
        for p in &self.permissions {
            permissions.insert(Permission::parse(p).map_err(PluginError::Manifest)?);
        }
        for (name, acct) in &self.accounts {
            validate_account(name, acct, &permissions, builtin).map_err(PluginError::Manifest)?;
        }
        for p in &permissions {
            if let Permission::Account(a) = p
                && !self.accounts.contains_key(a)
            {
                return bad(format!(
                    "permission `account:{a}` has no [accounts.{a}] table"
                ));
            }
        }

        let mut warnings: Vec<String> = self
            .extra
            .keys()
            .map(|k| format!("unknown key `{k}` ignored (written for a newer Luxtable?)"))
            .collect();
        let mut contributions = Vec::new();
        let mut ids = BTreeSet::new();
        for (key, list) in self.contributes {
            let Some(kind) = CONTRACTS.iter().map(|c| c.0).find(|k| k.key() == key) else {
                warnings.push(format!(
                    "[[contributes.{key}]] is not supported by this Luxtable and was skipped"
                ));
                continue;
            };
            if kind == Kind::NativeFeature && !builtin {
                return bad(
                    "[[contributes.native_feature]] is only for Luxtable's own bundled plug-ins"
                        .into(),
                );
            }
            for raw in list {
                if kind == Kind::NativeFeature && !NATIVE_FEATURES.contains(&raw.id.as_str()) {
                    warnings.push(format!(
                        "[[contributes.native_feature]] `{}` is not a feature of this Luxtable and was skipped",
                        raw.id
                    ));
                    continue;
                }
                let c = raw.validate(kind, &self.accounts, &permissions, &mut warnings)?;
                if !ids.insert(c.id.clone()) {
                    return bad(format!("two contributions share the id `{}`", c.id));
                }
                contributions.push(c);
            }
        }
        let code = contributions.iter().any(|c| c.kind.is_code());
        let platforms = self.platforms.map_or_else(
            || [Platform::Macos, Platform::Ipados].into_iter().collect(),
            |p| p.into_iter().collect::<BTreeSet<_>>(),
        );
        if platforms.is_empty() {
            return bad("`platforms` is empty".into());
        }
        // A native feature's permissions describe the app's own code (shown, never asked for).
        let native = contributions.iter().any(|c| c.kind == Kind::NativeFeature);
        if !code && !native && permissions.iter().any(|p| p != &Permission::PhotoMetadata) {
            warnings
                .push("a data-only plug-in runs no code, so its permissions are never used".into());
        }
        Ok(Manifest {
            id: self.id,
            name,
            version,
            api: self.api,
            min_luxtable,
            authors: self.authors,
            summary: text::clean(&self.summary, text::SUMMARY_MAX),
            homepage: self.homepage,
            platforms,
            permissions,
            accounts: self.accounts,
            contributions,
            warnings,
        })
    }
}

impl RawContribution {
    #[allow(clippy::too_many_lines)] // one check after another, each with its message
    fn validate(
        self,
        kind: Kind,
        accounts: &BTreeMap<String, AccountSpec>,
        permissions: &BTreeSet<Permission>,
        warnings: &mut Vec<String>,
    ) -> Result<Contribution, PluginError> {
        let at = format!("[[contributes.{}]] `{}`", kind.key(), self.id);
        let bad = |m: String| Err(PluginError::Manifest(format!("{at}: {m}")));
        if !valid_local_id(&self.id) {
            return Err(PluginError::Manifest(format!(
                "[[contributes.{}]]: id `{}` must be lowercase letters, digits and -",
                kind.key(),
                self.id
            )));
        }
        let (lo, hi) = kind.contracts();
        if !(lo..=hi).contains(&self.contract) {
            return bad(format!(
                "contract {} is not supported (this Luxtable supports {lo}–{hi}{})",
                self.contract,
                if self.contract > hi {
                    "; it needs a newer Luxtable"
                } else {
                    ""
                }
            ));
        }
        for key in self.extra.keys() {
            warnings.push(format!("{at}: unknown key `{key}` ignored"));
        }
        let file = if !kind.has_file() {
            if self.file.is_some() || self.module.is_some() || self.entry.is_some() {
                return bad("a native feature names no `file`, `module` or `entry`".into());
            }
            String::new()
        } else if kind.is_code() {
            if self.file.is_some() {
                return bad("code contributions name a `module`, not a `file`".into());
            }
            self.module
                .ok_or_else(|| PluginError::Manifest(format!("{at}: `module` is missing")))?
        } else {
            if self.module.is_some() || self.entry.is_some() {
                return bad("data contributions name a `file`, not a `module` or `entry`".into());
            }
            self.file
                .ok_or_else(|| PluginError::Manifest(format!("{at}: `file` is missing")))?
        };
        if kind.has_file() {
            check_relative(&file).map_err(|m| PluginError::Manifest(format!("{at}: {m}")))?;
        }
        if kind.is_code() && !(has_ext(&file, "wasm") || has_ext(&file, "wat")) {
            return bad(format!("module `{file}` must be a .wasm (or .wat) file"));
        }
        let entry = if kind.is_code() {
            let e = self.entry.unwrap_or_else(|| default_entry(kind).to_owned());
            if e.is_empty() || !e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return bad(format!("entry `{e}` is not a valid export name"));
            }
            Some(e)
        } else {
            None
        };
        if kind == Kind::NativeFeature && !self.params.is_empty() {
            return bad("a native feature has no `params` (its settings are the app's)".into());
        }
        let mut params = Vec::new();
        for p in self.params {
            let p = p
                .validate()
                .map_err(|m| PluginError::Manifest(format!("{at}: {m}")))?;
            if params.iter().any(|q: &Param| q.key == p.key) {
                return bad(format!("two params share the key `{}`", p.key));
            }
            params.push(p);
        }
        if kind != Kind::ExportDestination
            && (!self.accounts.is_empty() || !self.formats.is_empty() || self.max_file_mb.is_some())
        {
            return bad(
                "`accounts`, `formats` and `max_file_mb` are for export destinations".into(),
            );
        }
        if kind == Kind::ExportDestination {
            for a in &self.accounts {
                if !accounts.contains_key(a)
                    || !permissions.contains(&Permission::Account(a.clone()))
                {
                    return bad(format!(
                        "account `{a}` needs an [accounts.{a}] table and the `account:{a}` permission"
                    ));
                }
            }
            if self.formats.is_empty() {
                return bad(format!(
                    "`formats` must list what it accepts ({})",
                    DESTINATION_FORMATS.join(", ")
                ));
            }
            if let Some(f) = self
                .formats
                .iter()
                .find(|f| !DESTINATION_FORMATS.contains(&f.as_str()))
            {
                return bad(format!(
                    "format `{f}` is not one of {}",
                    DESTINATION_FORMATS.join(", ")
                ));
            }
        }
        if kind != Kind::NativeFeature && !self.uses.is_empty() {
            return bad("`uses` is for native features".into());
        }
        let uses: Vec<String> = self
            .uses
            .iter()
            .map(|u| text::clean(u, text::SUMMARY_MAX))
            .filter(|u| !u.is_empty())
            .collect();
        let apron_rows = self.apron_rows.unwrap_or(0);
        if apron_rows > 256 || (apron_rows > 0 && kind != Kind::ExportPostProcess) {
            return bad("`apron_rows` is 0–256, for export post-processors".into());
        }
        Ok(Contribution {
            kind,
            name: text::clean(
                &self.name.unwrap_or_else(|| self.id.clone()),
                text::NAME_MAX,
            ),
            id: self.id,
            contract: self.contract,
            file,
            entry,
            params,
            accounts: self.accounts,
            formats: self.formats,
            max_file_mb: self.max_file_mb,
            apron_rows,
            uses,
        })
    }
}

fn default_entry(kind: Kind) -> &'static str {
    match kind {
        Kind::ExportDestination => "lt_destination",
        _ => "lt_post_process",
    }
}

/// Account names that stand for a well-known service: a third-party plug-in may use one
/// only with that service's real sign-in hosts (else a look-alike could run its own OAuth
/// client against the user's account under a trusted name).
const KNOWN_PROVIDERS: &[(&str, &[&str])] = &[
    ("google", &["accounts.google.com", "oauth2.googleapis.com"]),
    ("amazon", &["www.amazon.com", "api.amazon.com"]),
    ("apple", &[]),
    ("microsoft", &["login.microsoftonline.com"]),
    ("dropbox", &["www.dropbox.com", "api.dropboxapi.com"]),
];

fn validate_account(
    name: &str,
    a: &AccountSpec,
    permissions: &BTreeSet<Permission>,
    builtin: bool,
) -> Result<(), String> {
    if !valid_local_id(name) {
        return Err(format!(
            "[accounts.{name}]: the name must be lowercase letters, digits and -"
        ));
    }
    if a.auth != "oauth2-pkce" {
        return Err(format!(
            "[accounts.{name}]: auth `{}` is not supported (use oauth2-pkce)",
            a.auth
        ));
    }
    if a.client_id.trim().is_empty() {
        return Err(format!("[accounts.{name}]: `client_id` is empty"));
    }
    let urls = [
        Some(&a.authorize_url),
        Some(&a.token_url),
        a.revoke_url.as_ref(),
    ];
    for url in urls.into_iter().flatten() {
        let host = https_host(url)
            .ok_or_else(|| format!("[accounts.{name}]: `{url}` must be an https:// URL"))?;
        if !permissions.iter().any(|p| p.allows_host(host)) {
            return Err(format!(
                "[accounts.{name}]: `{url}` needs the permission `network:{host}`"
            ));
        }
        if let Some((_, hosts)) = KNOWN_PROVIDERS.iter().find(|(p, _)| *p == name)
            && !builtin
            && !hosts.contains(&host)
        {
            return Err(format!(
                "[accounts.{name}]: `{host}` is not {name}'s sign-in; use another account name"
            ));
        }
    }
    if a.api_hosts.is_empty() {
        return Err(format!(
            "[accounts.{name}]: `api_hosts` must list the hosts its token is for"
        ));
    }
    for h in &a.api_hosts {
        if !valid_host(h) || h.starts_with("*.") {
            return Err(format!(
                "[accounts.{name}]: api host `{h}` must be a plain host name"
            ));
        }
        if !permissions.iter().any(|p| p.allows_host(h)) {
            return Err(format!(
                "[accounts.{name}]: api host `{h}` needs the permission `network:{h}`"
            ));
        }
    }
    Ok(())
}

/// The host of an `https://` URL (no userinfo, no port games).
#[must_use]
pub fn https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty() && !host.contains('@') && !host.contains(':') && valid_host(host))
        .then_some(host)
}

/// A reverse-DNS id: lowercase labels of letters, digits and -, at least two, ≤ 128 chars.
pub fn validate_id(id: &str) -> Result<(), String> {
    let labels: Vec<&str> = id.split('.').collect();
    let ok = id.len() <= 128
        && labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && l.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
    if ok {
        Ok(())
    } else {
        Err(format!(
            "`id` \"{id}\" must be reverse-DNS: lowercase words of letters, digits and - separated by dots (com.example.my-plugin)"
        ))
    }
}

fn valid_local_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Whether `path` ends in `.ext` (any case).
#[must_use]
pub fn has_ext(path: &str, ext: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// A manifest path: relative, normal components only (no `..`, no root, no `.`).
pub fn check_relative(path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if path.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!(
            "path `{path}` must stay inside the plug-in folder (relative, no ..)"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod tests;
