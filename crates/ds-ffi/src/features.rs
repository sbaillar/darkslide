//! The app's own optional integrations as bundled plug-ins (contract 43): Settings ▸
//! Plugins turns each on or off, and both sides ask the registry.
//!
//! - The app asks [`PluginHost::native_features`] after each plug-in change and hides
//!   or quiets a feature that is off (its settings, menus, badges, jobs).
//! - Rust code that runs on its own asks through a [`FeatureGate`]: the archive driver
//!   starts no rsync run, Amazon hand-off or Immich sidecar/album work while its
//!   feature is off (a run already going finishes; nothing is cut off half-written).
//! - [`PluginHost::seed_features`] is the one-time migration: a feature the user had
//!   already set up starts on.
//!
//! Turning a feature off never deletes its settings, credentials or data.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use crate::archive::ArchiveManager;
use crate::error::DsError;
use crate::jobs::guard;
use crate::plugins::PluginHost;

/// rsync to the archive target (`darkslide.archive`).
pub const ARCHIVE: &str = "archive";
/// The Amazon Photos hand-off (`darkslide.amazon-photos`).
pub const AMAZON_PHOTOS: &str = "amazon-photos";
/// Immich developed copies, sidecars and albums (`darkslide.immich`).
pub const IMMICH: &str = "immich";
/// The Google Photos export destination (`darkslide.google-photos`).
pub const GOOGLE_PHOTOS: &str = "google-photos";

/// What Rust jobs ask before doing a feature's work. Without a registry (tests, tools
/// that never open one) nothing is gated.
#[derive(Debug, Default)]
pub struct FeatureGate(Mutex<Option<Arc<PluginHost>>>);

impl FeatureGate {
    pub fn set(&self, host: Option<Arc<PluginHost>>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = host;
    }

    /// Whether `feature` may run now.
    pub fn allows(&self, feature: &str) -> bool {
        let host = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        host.is_none_or(|h| h.lock().feature_enabled(feature))
    }
}

#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
impl PluginHost {
    /// The native features that are on (`archive`, `amazon-photos`, `immich`,
    /// `google-photos`).
    pub fn native_features(&self) -> Vec<String> {
        self.lock().native_features().into_iter().collect()
    }

    pub fn feature_enabled(&self, feature: String) -> bool {
        self.lock().feature_enabled(&feature)
    }

    /// Features whose plug-in nobody has decided about yet (empty after the first launch).
    pub fn undecided_features(&self) -> Vec<String> {
        self.lock().undecided_features()
    }

    /// Decide, once, whether each undecided feature plug-in starts on: on when the user
    /// had already set the feature up (`configured`, by feature id). Returns the plug-in
    /// ids decided now.
    pub fn seed_features(&self, configured: HashMap<String, bool>) -> Result<Vec<String>, DsError> {
        let configured: BTreeMap<String, bool> = configured.into_iter().collect();
        self.with(|h| h.seed_features(&configured))
    }
}

#[uniffi::export]
impl ArchiveManager {
    /// Ask `host` before each archive run, Amazon hand-off and Immich step (the app's
    /// one registry). `None`: ungated.
    pub fn set_plugin_host(&self, host: Option<Arc<PluginHost>>) -> Result<(), DsError> {
        guard(|| {
            self.feature_gate().set(host);
            Ok(())
        })
    }
}

/// How the bundled plug-ins of a build load (`Darkslide --self-test` checks the app bundle).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BundledPluginsCheck {
    /// Ids of the bundled plug-ins that loaded (on or off).
    pub loaded: Vec<String>,
    /// The native features they declare.
    pub features: Vec<String>,
    /// "folder: why" for each that didn't load (files that don't match this build…).
    pub problems: Vec<String>,
}

/// Load the bundled plug-ins in `builtin_dir` as the app would, without any user state
/// (nothing is read from or written to the support folder) and without becoming the
/// host export steps resolve through.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn check_bundled_plugins(builtin_dir: String) -> BundledPluginsCheck {
    // A user folder that doesn't exist: no state, nothing installed, nothing written.
    let nowhere = std::env::temp_dir().join(format!(
        "ds-bundled-check-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
    ));
    let h = ds_plugin::Host::open(
        Some(builtin_dir.into()),
        nowhere,
        ds_plugin::darkslide_version(),
        ds_plugin::Platform::Macos,
    );
    let mut out = BundledPluginsCheck {
        loaded: Vec::new(),
        features: Vec::new(),
        problems: Vec::new(),
    };
    for p in h.plugins() {
        let folder = p
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match (&p.status, &p.manifest) {
            (ds_plugin::Status::Active | ds_plugin::Status::Disabled, Some(m)) => {
                out.loaded.push(m.id.clone());
                out.features.extend(m.native_features().map(str::to_owned));
            }
            (status, _) => {
                let why = match status {
                    ds_plugin::Status::Error(e) => e.clone(),
                    other => format!("{other:?}"),
                };
                out.problems.push(format!("{folder}: {why}"));
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "features_tests.rs"]
mod tests;
