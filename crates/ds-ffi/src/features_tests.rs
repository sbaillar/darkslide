use std::path::{Path, PathBuf};

use super::*;

fn builtin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/builtin")
}

#[test]
fn the_bundled_integrations_are_listed_and_start_off() {
    let t = tempfile::tempdir().unwrap();
    let h = PluginHost::unshared(Some(builtin()), t.path().join("Plugins"));
    let records = h.list();
    for (id, feature) in [
        ("darkslide.archive", ARCHIVE),
        ("darkslide.amazon-photos", AMAZON_PHOTOS),
        ("darkslide.immich", IMMICH),
        ("darkslide.google-photos", GOOGLE_PHOTOS),
    ] {
        let r = records.iter().find(|r| r.id == id).unwrap();
        assert!(r.built_in && !r.has_code && !r.summary.is_empty(), "{r:?}");
        assert_eq!(r.contributions[0].kind, "native_feature");
        assert_eq!(r.contributions[0].id, feature);
        assert!(
            !r.contributions[0].uses.is_empty(),
            "{id} says what it reaches"
        );
        assert_eq!(r.state, crate::plugins::PluginState::Disabled, "{id}");
        assert!(
            r.permissions.iter().all(|p| p.granted),
            "built-ins need no grant"
        );
        assert!(!h.feature_enabled(feature.into()));
    }
    // The looks pack is still on by default.
    let looks = records.iter().find(|r| r.id == "darkslide.looks").unwrap();
    assert_eq!(looks.state, crate::plugins::PluginState::Active);
    assert_eq!(h.native_features().len(), 0);
    let mut undecided = h.undecided_features();
    undecided.sort();
    assert_eq!(undecided, [AMAZON_PHOTOS, ARCHIVE, GOOGLE_PHOTOS, IMMICH]);
}

#[test]
fn migration_and_switching_go_through_the_registry() {
    let t = tempfile::tempdir().unwrap();
    let user = t.path().join("Plugins");
    let h = PluginHost::unshared(Some(builtin()), user.clone());
    let configured = HashMap::from([(ARCHIVE.to_owned(), true), (IMMICH.to_owned(), false)]);
    assert_eq!(h.seed_features(configured.clone()).unwrap().len(), 4);
    assert_eq!(h.native_features(), [ARCHIVE]);
    assert_eq!(h.undecided_features().len(), 0);
    assert_eq!(h.seed_features(configured).unwrap().len(), 0, "once");
    h.set_enabled("darkslide.immich".into(), true).unwrap();
    let mut on = h.native_features();
    on.sort();
    assert_eq!(on, [ARCHIVE, IMMICH]);
    // Another launch reads the same decisions.
    let again = PluginHost::unshared(Some(builtin()), user);
    assert!(again.feature_enabled(IMMICH.into()) && !again.feature_enabled(GOOGLE_PHOTOS.into()));
}

#[test]
fn a_gate_without_a_registry_allows_everything() {
    let gate = FeatureGate::default();
    assert!(gate.allows(ARCHIVE));
    let t = tempfile::tempdir().unwrap();
    let h = PluginHost::unshared(Some(builtin()), t.path().join("Plugins"));
    gate.set(Some(h.clone()));
    assert!(!gate.allows(ARCHIVE));
    h.set_enabled("darkslide.archive".into(), true).unwrap();
    assert!(gate.allows(ARCHIVE), "a change applies at once");
    assert!(!gate.allows(IMMICH));
    gate.set(None);
    assert!(gate.allows(IMMICH));
}

#[test]
fn the_bundled_plugins_of_this_build_load() {
    let ok = check_bundled_plugins(builtin().display().to_string());
    assert_eq!(ok.problems.len(), 0, "{:?}", ok.problems);
    assert!(ok.loaded.contains(&"darkslide.looks".to_owned()));
    let mut f = ok.features.clone();
    f.sort();
    assert_eq!(f, [AMAZON_PHOTOS, ARCHIVE, GOOGLE_PHOTOS, IMMICH]);
    // A copy with a changed file is reported, not loaded.
    let t = tempfile::tempdir().unwrap();
    let copy = t.path().join("darkslide-archive.dsplugin");
    std::fs::create_dir_all(&copy).unwrap();
    let toml =
        std::fs::read_to_string(builtin().join("darkslide-archive.dsplugin/plugin.toml")).unwrap();
    std::fs::write(copy.join("plugin.toml"), toml + "\n").unwrap();
    let bad = check_bundled_plugins(t.path().display().to_string());
    assert_eq!(bad.loaded.len(), 0);
    assert!(
        bad.problems[0].contains("don't match this build"),
        "{:?}",
        bad.problems
    );
    // A folder that is no plug-in at all.
    std::fs::write(copy.join("plugin.toml"), "nope").unwrap();
    let none = check_bundled_plugins(t.path().display().to_string());
    assert_eq!((none.loaded.len(), none.problems.len()), (0, 1), "{none:?}");
}
