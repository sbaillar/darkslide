use super::*;
use std::fs;

use lt_export::Image16;

/// Tests share the "current host" slot: one at a time.
pub(super) fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn repo_plugins() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins")
}

struct Env {
    _tmp: tempfile::TempDir,
    user: PathBuf,
    src: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (user, src) = (root.join("support/Plugins"), root.join("src"));
    fs::create_dir_all(&src).unwrap();
    Env {
        _tmp: tmp,
        user,
        src,
    }
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

/// The film border, copied to `parent` with `perms` (and a new id).
fn border(parent: &Path, id: &str, perms: &str) -> PathBuf {
    let d = parent.join(format!("{id}.ltplugin"));
    fs::create_dir_all(&d).unwrap();
    let src = repo_plugins().join("film-border.ltplugin");
    fs::copy(src.join("border.wat"), d.join("border.wat")).unwrap();
    let toml = fs::read_to_string(src.join("plugin.toml")).unwrap();
    let toml = toml
        .replace("com.example.film-border", id)
        .replace("api = 1\n", &format!("api = 1\npermissions = [{perms}]\n"));
    fs::write(d.join("plugin.toml"), toml).unwrap();
    d
}

fn looks(parent: &Path, id: &str, version: &str) -> PathBuf {
    let d = parent.join(format!("{id}.ltplugin"));
    fs::create_dir_all(&d).unwrap();
    fs::write(
        d.join("plugin.toml"),
        format!(
            "manifest = 1\nid = \"{id}\"\nname = \"Looks\"\nversion = \"{version}\"\napi = 1\nsummary = \"s\"\n\
             [[contributes.looks]]\nid = \"l\"\nfile = \"l.json\"\ncontract = 1\n\
             [[contributes.voice_phrases]]\nid = \"v\"\nfile = \"v.json\"\ncontract = 1\n"
        ),
    )
    .unwrap();
    fs::write(
        d.join("l.json"),
        r#"[{"id":"x","name":"X","values":{"exposure.contrast":5}}]"#,
    )
    .unwrap();
    fs::write(
        d.join("v.json"),
        r#"{"language":"en","phrases":{"keeper":"key-pick"}}"#,
    )
    .unwrap();
    d
}

fn record(h: &PluginHost, id: &str) -> PluginRecord {
    h.list().into_iter().find(|r| r.id == id).unwrap()
}

fn install(h: &PluginHost, path: &Path, downgrade: bool) -> Result<PluginInstallResult, DsError> {
    let p = h.preview(s(path))?;
    h.install(p.token, downgrade)
}

#[test]
fn bundled_looks_reach_the_app() {
    let _g = serial();
    let e = env();
    let h = PluginHost::open(Some(s(&repo_plugins().join("builtin"))), s(&e.user));
    let r = record(&h, "luxtable.looks");
    assert_eq!(
        (r.state, r.built_in, r.dev, r.trust.clone()),
        (PluginState::Active, true, false, PluginTrust::BuiltIn)
    );
    assert_eq!(r.contributions.len(), 2);
    let looks = h.data_packs(PluginDataKind::Looks);
    assert_eq!(looks.packs.len(), 1);
    assert!(looks.errors.is_empty(), "{:?}", looks.errors);
    assert!(looks.packs[0].json.contains("look.portrait"));
    assert_eq!(h.data_packs(PluginDataKind::Styles).packs.len(), 1);
    h.set_enabled("luxtable.looks".into(), false).unwrap();
    assert_eq!(record(&h, "luxtable.looks").state, PluginState::Disabled);
    assert!(h.user_folder("luxtable.looks".into()).is_err());
    assert_eq!(h.state_error(), None);
}

#[test]
fn installs_stage_then_wait_for_approvals() {
    let _g = serial();
    let e = env();
    let src = border(
        &e.src,
        "com.example.border",
        "\"network:api.example.com\", \"photo_metadata\"",
    );
    let h = PluginHost::open(None, s(&e.user));
    let p = h.preview(s(&src)).unwrap();
    assert_eq!(
        (
            p.id.as_str(),
            p.trust.clone(),
            p.has_code,
            p.replaces.clone()
        ),
        ("com.example.border", PluginTrust::Unsigned, true, None)
    );
    assert!(
        p.permissions
            .iter()
            .any(|q| q.sensitive && q.label == "Connect to api.example.com")
    );
    h.discard(p.token);
    assert!(h.install("gone".into(), false).is_err());

    let out = install(&h, &src, false).unwrap();
    assert_eq!(
        out,
        PluginInstallResult::Installed {
            id: "com.example.border".into(),
            version: "1.1.0".into()
        }
    );
    assert_eq!(
        install(&h, &src, false).unwrap(),
        PluginInstallResult::AlreadyInstalled {
            id: "com.example.border".into(),
            version: "1.1.0".into()
        }
    );
    let r = record(&h, "com.example.border");
    assert_eq!(r.state, PluginState::NeedsApproval);
    assert!(r.approvals.contains(&PluginApproval::AllowUnsignedCode));
    assert!(r.permissions.iter().all(|p| !p.granted));
    assert!(
        h.grant(
            "com.example.border".into(),
            "network:api.example.com".into(),
            None,
            vec![]
        )
        .is_err()
    );
    // Allowing this plug-in doesn't turn on unsigned code for all.
    h.allow_unsigned("com.example.border".into()).unwrap();
    assert!(!h.allow_unsigned_code());
    assert!(
        !record(&h, "com.example.border")
            .approvals
            .contains(&PluginApproval::AllowUnsignedCode)
    );
    h.set_allow_unsigned_code(true).unwrap();
    assert!(h.allow_unsigned_code());
    h.grant(
        "com.example.border".into(),
        "network:api.example.com".into(),
        None,
        vec![],
    )
    .unwrap();
    h.grant(
        "com.example.border".into(),
        "photo_metadata".into(),
        None,
        vec![],
    )
    .unwrap();
    assert_eq!(record(&h, "com.example.border").state, PluginState::Active);
    let pps = h.post_processors();
    assert_eq!(
        pps[0]
            .params
            .iter()
            .map(|p| p.kind.as_str())
            .collect::<Vec<_>>(),
        ["float", "color", "bool"]
    );
    h.revoke("com.example.border".into(), "photo_metadata".into())
        .unwrap();
    assert_eq!(
        record(&h, "com.example.border").state,
        PluginState::NeedsApproval
    );
    // Removal: the app trashes the folder, then the host forgets the grants.
    fs::remove_dir_all(h.user_folder("com.example.border".into()).unwrap()).unwrap();
    h.forget("com.example.border".into()).unwrap();
    assert!(h.list().iter().all(|r| r.id != "com.example.border"));
}

#[test]
fn file_grants_take_a_folder_outside_protected_roots() {
    let _g = serial();
    let e = env();
    let h = PluginHost::open(None, s(&e.user));
    h.set_allow_unsigned_code(true).unwrap();
    install(
        &h,
        &border(&e.src, "com.example.f", "\"files.write:out\""),
        false,
    )
    .unwrap();
    let session = e.src.join("Shoot");
    fs::create_dir_all(&session).unwrap();
    let grant = |f: &Path| {
        h.grant(
            "com.example.f".into(),
            "files.write:out".into(),
            Some(s(f)),
            vec![s(&session)],
        )
    };
    assert!(grant(&session).is_err());
    assert!(grant(&e.user).is_err());
    let ok = e.src.join("Out");
    fs::create_dir_all(&ok).unwrap();
    grant(&ok).unwrap();
}

#[test]
fn downgrades_need_consent_and_are_flagged() {
    let _g = serial();
    let e = env();
    let h = PluginHost::open(None, s(&e.user));
    install(
        &h,
        &looks(&e.src.join("2"), "com.example.l", "2.0.0"),
        false,
    )
    .unwrap();
    let v1 = looks(&e.src.join("1"), "com.example.l", "1.0.0");
    let p = h.preview(s(&v1)).unwrap();
    assert!(p.downgrade && p.replaces.as_deref() == Some("2.0.0"));
    assert!(
        h.install(p.token.clone(), false)
            .unwrap_err()
            .to_string()
            .contains("older")
    );
    assert!(matches!(
        install(&h, &v1, true).unwrap(),
        PluginInstallResult::Updated { .. }
    ));
}

#[test]
fn changes_shadowing_errors_and_dev_folders_are_listed() {
    let _g = serial();
    let e = env();
    let h = PluginHost::open(None, s(&e.user));
    install(&h, &looks(&e.src, "com.example.l", "1.0.0"), false).unwrap();
    let dir = PathBuf::from(h.user_folder("com.example.l".into()).unwrap());
    fs::write(dir.join("l.json"), "[]").unwrap();
    let old = looks(&e.user, "old", "0.9.0");
    let t = fs::read_to_string(old.join("plugin.toml")).unwrap();
    fs::write(
        old.join("plugin.toml"),
        t.replace("id = \"old\"", "id = \"com.example.l\""),
    )
    .unwrap();
    fs::create_dir_all(e.user.join("broken.ltplugin")).unwrap();
    h.reload();
    let list = h.list();
    let main = list
        .iter()
        .find(|r| r.path.ends_with("com.example.l.ltplugin"))
        .unwrap();
    assert_eq!(main.approvals, [PluginApproval::Changed]);
    assert!(list.iter().any(|r| r.state == PluginState::Shadowed));
    assert_eq!(
        list.iter().find(|r| r.id == "broken").unwrap().state,
        PluginState::Error
    );
    h.approve_files("com.example.l".into()).unwrap();
    assert_eq!(record(&h, "com.example.l").state, PluginState::Active);

    let dev = border(&e.src.join("dev"), "com.example.dev", "");
    assert!(
        h.add_dev_folder(s(&dev)).is_err(),
        "needs unsigned code allowed"
    );
    h.set_allow_unsigned_code(true).unwrap();
    h.add_dev_folder(s(&dev)).unwrap();
    let r = record(&h, "com.example.dev");
    assert!(r.dev && r.state == PluginState::Active);
    assert_eq!(h.dev_folders().len(), 1);
    h.remove_dev_folder(h.dev_folders()[0].clone()).unwrap();
    assert!(h.list().iter().all(|r| !r.dev));
}

#[test]
fn signers_show_their_fingerprint() {
    let _g = serial();
    let e = env();
    let src = border(&e.src, "com.example.signed", "");
    let key = lt_plugin::trust::keygen("Tester").unwrap();
    let pkg = lt_plugin::package::Package::read_dir(&src).unwrap();
    let sig = lt_plugin::trust::sign(&pkg.hashes(), &key).unwrap();
    fs::write(src.join("SIGNATURE"), serde_json::to_string(&sig).unwrap()).unwrap();
    let h = PluginHost::open(None, s(&e.user));
    install(&h, &src, false).unwrap();
    let fp = lt_plugin::trust::fingerprint(&key.key);
    assert_eq!(
        record(&h, "com.example.signed").approvals[0],
        PluginApproval::TrustSigner {
            signer: "Tester".into(),
            key: key.key.clone(),
            fingerprint: fp.clone()
        }
    );
    h.trust_signer(key.key.clone(), "Tester".into()).unwrap();
    assert_eq!(
        record(&h, "com.example.signed").trust,
        PluginTrust::Signed {
            signer: "Tester".into(),
            key: key.key.clone(),
            fingerprint: fp
        }
    );
    h.untrust_signer(key.key).unwrap();
    assert!(matches!(
        record(&h, "com.example.signed").trust,
        PluginTrust::Untrusted { .. }
    ));
}

fn border_step(params: &str) -> ExportPostProcessStep {
    ExportPostProcessStep {
        plugin_id: "com.example.border".into(),
        contribution_id: "border".into(),
        params_json: params.into(),
        plugin_version: None,
    }
}

#[test]
#[allow(clippy::too_many_lines)] // one export step end to end
fn export_steps_resolve_through_the_current_host_and_paint() {
    let _g = serial();
    let e = env();
    let h = PluginHost::open(None, s(&e.user));
    h.set_allow_unsigned_code(true).unwrap();
    install(
        &h,
        &border(&e.src, "com.example.border", "\"photo_metadata\""),
        false,
    )
    .unwrap();
    h.grant(
        "com.example.border".into(),
        "photo_metadata".into(),
        None,
        vec![],
    )
    .unwrap();

    assert!(resolve_steps(&[]).unwrap().is_empty());
    let item = ExportItem {
        name: "a".into(),
        ..ExportItem::default()
    };
    // Inside: a 1 px red border on a 10×10 frame, the canvas unchanged.
    let inside = resolve_steps(&[border_step(
        r##"{"width": 0.1, "color": "#FF0000", "outside": false}"##,
    )])
    .unwrap();
    assert_eq!(inside[0].name(), "Film Border");
    assert_eq!(
        inside[0]
            .layout(Size::new(10, 10), OutputSpace::Srgb, &item)
            .unwrap(),
        (Size::new(10, 10), (0, 0))
    );
    let mut img = Image16 {
        size: Size::new(10, 10),
        data: vec![0; 300],
    };
    let full = PhotoRect {
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    };
    inside[0]
        .apply(
            Canvas::Rgb16(&mut img),
            full,
            OutputSpace::DisplayP3,
            &item,
            &|| false,
        )
        .unwrap();
    assert_eq!(&img.data[..3], &[65535, 0, 0]);
    assert_eq!(&img.data[33..36], &[0, 0, 0]);
    // Outside (the default): the canvas grows by the border.
    let outside = resolve_steps(&[border_step(r#"{"width": 0.1}"#)]).unwrap();
    assert_eq!(
        outside[0]
            .layout(Size::new(40, 20), OutputSpace::Srgb, &item)
            .unwrap(),
        (Size::new(44, 24), (2, 2))
    );
    // A failure is the export's error.
    let mut bad = Image16 {
        size: Size::new(2, 2),
        data: vec![0; 3],
    };
    let err = inside[0]
        .apply(
            Canvas::Rgb16(&mut bad),
            full,
            OutputSpace::Srgb,
            &item,
            &|| false,
        )
        .unwrap_err();
    assert!(err.to_string().contains("doesn't match"), "{err}");

    // A value out of the (narrowed) range is clamped: the recipe still exports.
    let clamped = resolve_steps(&[border_step(r#"{"width": 5, "outside": false}"#)]).unwrap();
    let mut tiny = Image16 {
        size: Size::new(10, 10),
        data: vec![1000; 300],
    };
    let full = PhotoRect {
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    };
    clamped[0]
        .apply(
            Canvas::Rgb16(&mut tiny),
            full,
            OutputSpace::Srgb,
            &item,
            &|| false,
        )
        .unwrap();
    assert_eq!(
        &tiny.data[..3],
        &[0, 0, 0],
        "0.2 of 10 px: a 2 px black border"
    );
    assert_eq!(
        &tiny.data[(2 * 10 + 2) * 3..(2 * 10 + 2) * 3 + 3],
        &[1000, 1000, 1000]
    );
    assert!(resolve_steps(&[border_step("{nope")]).is_err());
    let notes = h.step_notes(
        "com.example.border".into(),
        "border".into(),
        r#"{"width": 5}"#.into(),
    );
    assert!(notes[0].starts_with("Width:"), "{notes:?}");
    assert_eq!(
        h.step_notes("com.example.none".into(), "x".into(), "{}".into())
            .len(),
        0
    );
    let missing = ExportPostProcessStep {
        contribution_id: "nope".into(),
        ..border_step("")
    };
    assert!(
        resolve_steps(&[missing])
            .unwrap_err()
            .contains("isn't available")
    );
    h.set_enabled("com.example.border".into(), false).unwrap();
    assert!(
        resolve_steps(&[border_step("")]).is_err(),
        "a disabled plug-in's recipe can't start"
    );
}

#[test]
fn export_recipes_carry_their_steps() {
    let _g = serial();
    let e = env();
    let h = PluginHost::open(None, s(&e.user));
    h.set_allow_unsigned_code(true).unwrap();
    install(&h, &border(&e.src, "com.example.border", ""), false).unwrap();
    let mut spec = crate::export::ExportRecipeSpec {
        name: "web".into(),
        format: crate::export::ExportFileFormat::Png8,
        size: crate::export::ExportSize::Original,
        allow_upscale: false,
        space: crate::export::ExportColorSpace::Srgb,
        sharpen: 0.0,
        file_name_template: "{name}".into(),
        destination: s(&e.src),
        include_exif: false,
        strip_gps: false,
        constrain_to_sensor_pixels: false,
        output_profile: None,
        ai_upscale: false,
        watermark: None,
        post_process: vec![border_step("")],
        label_tags: true,
    };
    assert_eq!(
        crate::export::recipe_of(&spec).unwrap().post_process.len(),
        1
    );
    spec.post_process = vec![ExportPostProcessStep {
        plugin_id: "com.example.gone".into(),
        ..border_step("")
    }];
    let err = crate::export::recipe_of(&spec).unwrap_err().to_string();
    assert!(err.contains("isn't available"), "{err}");
    assert!(
        crate::export::validate_export_recipes(vec![spec])
            .unwrap()
            .unwrap()
            .contains("isn't available")
    );
}

#[test]
fn permissions_read_as_words() {
    for (p, want) in [
        ("photo_pixels", "pixels"),
        ("photo_metadata", "metadata"),
        ("metadata_write", "Suggest"),
        ("files.read:in", "Read files"),
        ("files.write:out", "Write files"),
        ("network:a.example.com", "Connect to a.example.com"),
        ("account:google", "google account"),
        ("clock", "date"),
    ] {
        assert!(
            permission_label(&Permission::parse(p).unwrap()).contains(want),
            "{p}"
        );
    }
}

#[test]
fn params_describe_every_type() {
    let toml = r#"manifest = 1
id = "com.example.p"
name = "P"
version = "1.0.0"
api = 1
[[contributes.export_post_process]]
id = "x"
module = "m.wat"
contract = 1
params = [
  { key = "i", type = "int", min = 1, max = 3, default = 2 },
  { key = "b", type = "bool", default = true },
  { key = "c", type = "choice", choices = ["a", "b"], default = "a" },
  { key = "t", type = "text", max_len = 9, default = "x" },
]
"#;
    let m = lt_plugin::Manifest::parse(toml, &lt_plugin::luxtable_version()).unwrap();
    let infos: Vec<_> = m.contributions[0].params.iter().map(param_info).collect();
    assert_eq!(infos[0].kind, "int");
    assert_eq!((infos[0].min, infos[0].max), (Some(1.0), Some(3.0)));
    assert_eq!(infos[1].default_json, "true");
    assert_eq!(infos[2].choices, ["a", "b"]);
    assert_eq!(infos[3].max_len, Some(9));
    assert_eq!(contributions(&m)[0].name, "x");
}

#[test]
fn the_edit_vocabulary_is_exported() {
    let keys = edit_keys();
    assert!(
        keys.iter()
            .any(|k| k.key == "exposure.ev" && k.group == "exposure" && k.kind == "amount")
    );
    for kind in ["choice", "wheel", "flag", "index"] {
        assert!(keys.iter().any(|k| k.kind == kind), "{kind}");
    }
}

#[test]
fn errors_map_to_lt_errors() {
    assert!(matches!(
        err(lt_plugin::PluginError::Cancelled),
        DsError::Cancelled
    ));
    assert!(matches!(
        err(std::io::Error::other("x").into()),
        DsError::Io { .. }
    ));
    assert!(matches!(
        err(lt_plugin::PluginError::NotFound("x".into())),
        DsError::InvalidArgument { .. }
    ));
    assert_eq!(space_name(OutputSpace::Rec2020), "rec2020");
}
