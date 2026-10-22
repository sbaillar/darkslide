use super::*;
use crate::trust::{SIGNATURE_FILE, keygen, sign};

const BORDER: &str = include_str!("../../../plugins/film-border.dsplugin/border.wat");
const LOOKS: &str = r#"[{"id":"t","name":"T","values":{"exposure.contrast":10}}]"#;

struct Env {
    _tmp: tempfile::TempDir,
    builtin: PathBuf,
    user: PathBuf,
    src: PathBuf,
    outside: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (builtin, user, src, outside) = (
        root.join("Darkslide.app/Contents/Resources/Plugins"),
        root.join("support/Plugins"),
        root.join("src"),
        root.join("outside"),
    );
    for d in [&builtin, &src, &outside] {
        fs::create_dir_all(d).unwrap();
    }
    Env {
        _tmp: tmp,
        builtin,
        user,
        src,
        outside,
    }
}

/// The hashes of every bundled folder as it is now (what a build would compile in).
fn hashes_of(dir: &Path) -> BTreeMap<String, FileHashes> {
    list_folders(dir)
        .into_iter()
        .map(|f| {
            (
                f.file_name().unwrap().to_string_lossy().into_owned(),
                Package::read_dir(&f).unwrap().hashes(),
            )
        })
        .collect()
}

impl Env {
    fn host(&self) -> Host {
        Host::open_with(
            Some(self.builtin.clone()),
            hashes_of(&self.builtin),
            self.user.clone(),
            semver::Version::new(0, 2, 0),
            Platform::Macos,
        )
    }
}

fn header(id: &str, version: &str, extra: &str) -> String {
    format!(
        "manifest = 1\nid = \"{id}\"\nname = \"N {id}\"\nversion = \"{version}\"\napi = 1\n{extra}\n"
    )
}

/// A looks pack at `parent/<name>.dsplugin`.
fn data_plugin(parent: &Path, name: &str, id: &str, version: &str) -> PathBuf {
    let d = parent.join(format!("{name}.dsplugin"));
    fs::create_dir_all(&d).unwrap();
    let toml = header(
        id,
        version,
        "[[contributes.looks]]\nid = \"looks\"\nfile = \"looks.json\"\ncontract = 1",
    );
    fs::write(d.join(MANIFEST_FILE), toml).unwrap();
    let looks = if id.starts_with(BUILTIN_PREFIX) {
        r#"[{"id":"look.t","uuid":"00000000-0000-0000-0000-00000000B001","name":"T","values":{"exposure.contrast":10}}]"#
    } else {
        LOOKS
    };
    fs::write(d.join("looks.json"), looks).unwrap();
    d
}

/// The film border as a plug-in asking for `perms`.
fn code_plugin(parent: &Path, id: &str, version: &str, perms: &[&str]) -> PathBuf {
    let d = parent.join(format!("{id}.dsplugin"));
    fs::create_dir_all(&d).unwrap();
    let perms = perms
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = header(
        id,
        version,
        &format!(
            "permissions = [{perms}]\n[[contributes.export_post_process]]\nid = \"border\"\nmodule = \"b.wat\"\ncontract = 1\nparams = [{{ key = \"width\", type = \"float\", max = 0.2, default = 0.05 }}, {{ key = \"color\", type = \"color\", default = \"#000000\" }}]"
        ),
    );
    fs::write(d.join(MANIFEST_FILE), toml).unwrap();
    fs::write(d.join("b.wat"), BORDER).unwrap();
    d
}

fn sign_folder(dir: &Path, key: &trust::KeyFile) {
    let _ = fs::remove_file(dir.join(SIGNATURE_FILE));
    let sig = sign(&Package::read_dir(dir).unwrap().hashes(), key).unwrap();
    fs::write(
        dir.join(SIGNATURE_FILE),
        serde_json::to_string(&sig).unwrap(),
    )
    .unwrap();
}

fn status<'a>(h: &'a Host, id: &str) -> &'a Status {
    &h.plugins()
        .iter()
        .find(|p| p.id() == Some(id) && !matches!(p.status, Status::Shadowed { .. }))
        .unwrap()
        .status
}

fn needs(h: &Host, id: &str) -> Vec<Approval> {
    match status(h, id) {
        Status::NeedsApproval(v) => v.clone(),
        s => panic!("{id}: {s:?}"),
    }
}

fn err(s: &Status) -> &str {
    match s {
        Status::Error(m) => m,
        s => panic!("not an error: {s:?}"),
    }
}

#[test]
fn builtins_are_active_and_can_be_disabled() {
    let e = env();
    data_plugin(&e.builtin, "looks", "darkslide.looks", "1.0.0");
    let mut h = e.host();
    assert_eq!(status(&h, "darkslide.looks"), &Status::Active);
    assert_eq!(h.plugins()[0].trust, Trust::BuiltIn);
    assert_eq!(h.data(Kind::Looks).0.len(), 1);
    h.set_enabled("darkslide.looks", false).unwrap();
    assert_eq!(status(&h, "darkslide.looks"), &Status::Disabled);
    assert_eq!(h.data(Kind::Looks).0.len(), 0);
    assert_eq!(
        status(&e.host(), "darkslide.looks"),
        &Status::Disabled,
        "persisted"
    );
    assert!(
        h.user_folder("darkslide.looks")
            .unwrap_err()
            .to_string()
            .contains("disabled, not removed")
    );
    h.approve_files("darkslide.looks").unwrap();
    assert!(h.set_enabled("com.example.none", true).is_err());
}

#[test]
fn bundled_folders_must_match_this_build() {
    let e = env();
    let d = data_plugin(&e.builtin, "looks", "darkslide.looks", "1.0.0");
    let hashes = hashes_of(&e.builtin);
    fs::write(d.join("looks.json"), "[]").unwrap();
    let h = Host::open_with(
        Some(e.builtin.clone()),
        hashes,
        e.user.clone(),
        semver::Version::new(0, 2, 0),
        Platform::Macos,
    );
    assert!(err(status(&h, "darkslide.looks")).contains("don't match this build"));
    // The real build's list covers the repository's own bundled folder.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/builtin");
    let h = Host::open(
        Some(repo),
        e.user.clone(),
        semver::Version::new(9, 0, 0),
        Platform::Macos,
    );
    assert_eq!(status(&h, "darkslide.looks"), &Status::Active);
}

#[test]
fn ids_are_reserved_both_ways() {
    let e = env();
    data_plugin(&e.builtin, "x", "com.example.x", "1.0.0");
    data_plugin(&e.user, "y", "darkslide.y", "1.0.0");
    let mut h = e.host();
    assert!(err(status(&h, "com.example.x")).contains("must start with"));
    assert!(err(status(&h, "darkslide.y")).contains("reserved"));
    let src = data_plugin(&e.src, "z", "darkslide.z", "1.0.0");
    assert!(h.stage(&src).unwrap_err().to_string().contains("reserved"));
    assert!(
        !fs::read_dir(&e.user)
            .unwrap()
            .flatten()
            .any(|d| d.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
    );
}

#[test]
fn installs_go_through_staging_and_leave_the_source() {
    let e = env();
    let src = data_plugin(&e.src, "My Looks", "com.example.looks", "1.0.0");
    let before = Package::read_dir(&src).unwrap();
    let mut h = e.host();
    let p = h.stage(&src).unwrap();
    assert_eq!(
        (p.replaces.clone(), p.trust.clone()),
        (None, Trust::Unsigned)
    );
    // The source changing after the sheet doesn't matter: the staged copy installs.
    fs::write(src.join("looks.json"), "[]").unwrap();
    let out = h
        .install_staged(&p.token, InstallOptions::default())
        .unwrap();
    assert_eq!(
        out,
        InstallOutcome::Installed {
            id: "com.example.looks".into(),
            version: "1.0.0".into()
        }
    );
    assert_eq!(status(&h, "com.example.looks"), &Status::Active);
    let installed = Package::read_dir(&h.user_folder("com.example.looks").unwrap()).unwrap();
    assert_eq!(installed.hashes(), before.hashes());
    assert!(
        h.install_staged(&p.token, InstallOptions::default())
            .is_err(),
        "a token is used once"
    );
    // Cancelling discards the staged copy.
    let p2 = h
        .stage(&data_plugin(&e.src, "Other", "com.example.other", "1.0.0"))
        .unwrap();
    h.discard(&p2.token);
    h.discard("nope");
    assert!(
        !fs::read_dir(&e.user)
            .unwrap()
            .flatten()
            .any(|d| d.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
    );
}

#[test]
fn zips_install_like_folders() {
    let e = env();
    let src = data_plugin(&e.src, "Z", "com.example.zip", "1.0.0");
    let zip = e.src.join("Z.dsplugin.zip");
    fs::write(&zip, Package::read_dir(&src).unwrap().to_zip().unwrap()).unwrap();
    let mut h = e.host();
    assert!(matches!(
        h.install(&zip, InstallOptions::default()).unwrap(),
        InstallOutcome::Installed { .. }
    ));
    assert_eq!(status(&h, "com.example.zip"), &Status::Active);
}

#[test]
fn same_version_is_a_no_op_or_refused() {
    let e = env();
    let src = data_plugin(&e.src, "a", "com.example.a", "1.0.0");
    let mut h = e.host();
    h.install(&src, InstallOptions::default()).unwrap();
    let again = h.install(&src, InstallOptions::default()).unwrap();
    assert_eq!(
        again,
        InstallOutcome::AlreadyInstalled {
            id: "com.example.a".into(),
            version: "1.0.0".into()
        }
    );
    fs::write(src.join("looks.json"), "[]").unwrap();
    let e2 = h
        .install(&src, InstallOptions::default())
        .unwrap_err()
        .to_string();
    assert!(e2.contains("needs a new version number"), "{e2}");
    assert!(
        !fs::read_dir(&e.user)
            .unwrap()
            .flatten()
            .any(|d| d.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
    );
}

#[test]
fn updates_replace_and_downgrades_need_consent() {
    let e = env();
    let mut h = e.host();
    h.install(
        &data_plugin(&e.src, "v2", "com.example.a", "2.0.0"),
        InstallOptions::default(),
    )
    .unwrap();
    let v3 = data_plugin(&e.src, "v3", "com.example.a", "3.0.0");
    let out = h.install(&v3, InstallOptions::default()).unwrap();
    assert_eq!(
        out,
        InstallOutcome::Updated {
            id: "com.example.a".into(),
            from: "2.0.0".into(),
            to: "3.0.0".into()
        }
    );
    assert_eq!(fs::read_dir(e.user.join(".replaced")).unwrap().count(), 0);

    let v1 = data_plugin(&e.src, "v1", "com.example.a", "1.0.0");
    let err = h
        .install(&v1, InstallOptions::default())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("3.0.0 is installed; this is the older 1.0.0"),
        "{err}"
    );
    assert_eq!(
        h.active("com.example.a")
            .unwrap()
            .manifest
            .as_ref()
            .unwrap()
            .version
            .to_string(),
        "3.0.0"
    );
    let p = h.stage(&v1).unwrap();
    assert_eq!(p.replaces.map(|v| v.to_string()).as_deref(), Some("3.0.0"));
    h.discard(&p.token);
    let out = h
        .install(
            &v1,
            InstallOptions {
                allow_downgrade: true,
            },
        )
        .unwrap();
    assert!(matches!(out, InstallOutcome::Updated { ref from, .. } if from == "3.0.0"));
}

#[test]
fn disabled_stays_disabled_across_an_update() {
    let e = env();
    let mut h = e.host();
    h.install(
        &data_plugin(&e.src, "v1", "com.example.a", "1.0.0"),
        InstallOptions::default(),
    )
    .unwrap();
    h.set_enabled("com.example.a", false).unwrap();
    h.install(
        &data_plugin(&e.src, "v2", "com.example.a", "1.1.0"),
        InstallOptions::default(),
    )
    .unwrap();
    assert_eq!(status(&h, "com.example.a"), &Status::Disabled);
}

#[test]
fn hand_placed_and_changed_folders_need_approval() {
    let e = env();
    let dir = data_plugin(&e.user, "com.example.a", "com.example.a", "1.0.0");
    let mut h = e.host();
    assert_eq!(needs(&h, "com.example.a"), [Approval::NotInstalled]);
    h.approve_files("com.example.a").unwrap();
    assert_eq!(status(&h, "com.example.a"), &Status::Active);
    fs::write(dir.join("looks.json"), r#"[{"id":"evil","name":"E"}]"#).unwrap();
    h.reload();
    assert_eq!(needs(&h, "com.example.a"), [Approval::Changed]);
    assert_eq!(h.data(Kind::Looks).0.len(), 0);
    h.approve_files("com.example.a").unwrap();
    assert_eq!(status(&h, "com.example.a"), &Status::Active);
}

#[test]
fn the_signer_is_pinned_per_id() {
    let e = env();
    let mut h = e.host();
    h.set_allow_unsigned_code(true).unwrap();
    let alice = keygen("Alice").unwrap();
    let mallory = keygen("Mallory").unwrap();
    let v1 = code_plugin(
        &e.src.join("1"),
        "com.example.s",
        "1.0.0",
        &["network:api.example.com"],
    );
    sign_folder(&v1, &alice);
    h.install(&v1, InstallOptions::default()).unwrap();
    h.trust_signer(&alice.key, "Alice").unwrap();
    h.grant("com.example.s", "network:api.example.com", None, &[])
        .unwrap();
    assert_eq!(status(&h, "com.example.s"), &Status::Active);

    // A newer version signed by someone else, or unsigned, is refused at install…
    let evil = code_plugin(
        &e.src.join("2"),
        "com.example.s",
        "9.0.0",
        &["network:api.example.com"],
    );
    sign_folder(&evil, &mallory);
    let msg = h
        .install(&evil, InstallOptions::default())
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("different key") && msg.contains("remove the installed one first"),
        "{msg}"
    );
    let unsigned = code_plugin(
        &e.src.join("3"),
        "com.example.s",
        "9.0.0",
        &["network:api.example.com"],
    );
    assert!(
        h.install(&unsigned, InstallOptions::default())
            .unwrap_err()
            .to_string()
            .contains("unsigned")
    );
    // …and when dropped into the folder by hand, it doesn't load (nor inherit grants).
    let dropped = code_plugin(&e.user.join("x.dsplugin"), "com.example.s", "9.0.0", &[]);
    sign_folder(&dropped, &mallory);
    let staged_parent = e.user.join("x.dsplugin");
    fs::rename(&dropped, e.user.join("evil.dsplugin")).unwrap();
    fs::remove_dir_all(staged_parent).unwrap();
    h.reload();
    let evil = h
        .plugins()
        .iter()
        .find(|p| p.dir.ends_with("evil.dsplugin"))
        .unwrap();
    assert!(err(&evil.status).contains("different key"));
    assert_eq!(
        status(&h, "com.example.s"),
        &Status::Active,
        "the real one stays"
    );

    // The same signer updates normally and keeps its grants.
    let v2 = code_plugin(
        &e.src.join("4"),
        "com.example.s",
        "1.1.0",
        &["network:api.example.com"],
    );
    sign_folder(&v2, &alice);
    fs::remove_dir_all(e.user.join("evil.dsplugin")).unwrap();
    h.install(&v2, InstallOptions::default()).unwrap();
    assert_eq!(status(&h, "com.example.s"), &Status::Active);

    // Removal forgets grants and the pin: a re-install starts from scratch.
    fs::remove_dir_all(h.user_folder("com.example.s").unwrap()).unwrap();
    h.forget("com.example.s").unwrap();
    assert!(h.granted("com.example.s").is_empty());
    sign_folder(&evil_again(&e), &mallory);
    h.install(
        &e.src.join("5/com.example.s.dsplugin"),
        InstallOptions::default(),
    )
    .unwrap();
    assert!(needs(&h, "com.example.s").contains(&Approval::Grant(
        Permission::parse("network:api.example.com").unwrap()
    )));
}

fn evil_again(e: &Env) -> PathBuf {
    code_plugin(
        &e.src.join("5"),
        "com.example.s",
        "9.0.0",
        &["network:api.example.com"],
    )
}

#[test]
fn a_state_entry_without_pins_still_needs_approval() {
    let e = env();
    data_plugin(&e.user, "com.example.a", "com.example.a", "1.0.0");
    let mut s = State::default();
    s.plugin("com.example.a").enabled = true;
    s.save(&e.user).unwrap();
    assert_eq!(needs(&e.host(), "com.example.a"), [Approval::NotInstalled]);
}

#[test]
fn unsigned_code_never_gets_files_or_network_silently() {
    let e = env();
    let src = code_plugin(
        &e.src,
        "com.example.b",
        "1.0.0",
        &["network:api.example.com", "photo_metadata"],
    );
    let mut h = e.host();
    h.install(&src, InstallOptions::default()).unwrap();
    let want = needs(&h, "com.example.b");
    assert!(want.contains(&Approval::AllowUnsignedCode));
    assert!(want.contains(&Approval::Grant(
        Permission::parse("network:api.example.com").unwrap()
    )));
    assert!(
        h.granted("com.example.b").is_empty(),
        "install grants nothing"
    );

    let e1 = h
        .grant("com.example.b", "network:api.example.com", None, &[])
        .unwrap_err()
        .to_string();
    assert!(e1.contains("isn't signed"), "{e1}");
    h.grant("com.example.b", "photo_metadata", None, &[])
        .unwrap();
    assert!(
        h.grant("com.example.b", "clock", None, &[])
            .unwrap_err()
            .to_string()
            .contains("doesn't ask for")
    );
    assert!(h.grant("com.example.b", "bogus", None, &[]).is_err());
    assert!(
        h.grant("com.example.b", "photo_metadata", Some(&e.outside), &[])
            .unwrap_err()
            .to_string()
            .contains("takes no folder")
    );

    h.set_allow_unsigned_code(true).unwrap();
    h.grant("com.example.b", "network:api.example.com", None, &[])
        .unwrap();
    assert_eq!(status(&h, "com.example.b"), &Status::Active);
    h.set_allow_unsigned_code(false).unwrap();
    let g = h.granted("com.example.b");
    assert!(
        g.contains(&Permission::PhotoMetadata) && g.len() == 1,
        "{g:?}"
    );
}

#[test]
fn file_grants_never_reach_darkslides_own_folders() {
    let e = env();
    let mut h = e.host();
    h.set_allow_unsigned_code(true).unwrap();
    h.install(
        &code_plugin(&e.src, "com.example.f", "1.0.0", &["files.write:out"]),
        InstallOptions::default(),
    )
    .unwrap();
    let grant = |h: &mut Host, f: &Path, protected: &[PathBuf]| {
        h.grant("com.example.f", "files.write:out", Some(f), protected)
    };
    assert!(
        grant(&mut h, &e.user, &[])
            .unwrap_err()
            .to_string()
            .contains("Darkslide's own files")
    );
    assert!(
        grant(&mut h, e.user.parent().unwrap(), &[]).is_err(),
        "the support root"
    );
    assert!(grant(&mut h, &e.builtin, &[]).is_err(), "the app");
    let app = e
        .builtin
        .ancestors()
        .find(|a| a.extension().is_some_and(|x| x == "app"))
        .unwrap();
    assert!(grant(&mut h, app, &[]).is_err(), "the app bundle");
    let session = e.outside.join("Shoot");
    fs::create_dir_all(session.join("Capture")).unwrap();
    assert!(
        grant(
            &mut h,
            &session.join("Capture"),
            std::slice::from_ref(&session)
        )
        .is_err(),
        "inside a session"
    );
    assert!(
        grant(&mut h, &e.outside, std::slice::from_ref(&session)).is_err(),
        "contains a session"
    );
    assert!(grant(&mut h, Path::new("/"), &[]).is_err());
    assert!(
        h.grant("com.example.f", "files.write:out", None, &[])
            .unwrap_err()
            .to_string()
            .contains("needs a folder")
    );
    let ok = e.outside.join("Exports");
    fs::create_dir_all(&ok).unwrap();
    grant(&mut h, &ok, std::slice::from_ref(&session)).unwrap();
    assert_eq!(
        h.granted_folder("com.example.f", "files.write:out", &[])
            .unwrap(),
        ok
    );
    // A link swapped in later fails the check at use.
    fs::remove_dir(&ok).unwrap();
    std::os::unix::fs::symlink(&e.user, &ok).unwrap();
    assert!(
        h.granted_folder("com.example.f", "files.write:out", &[])
            .is_err()
    );
    assert!(
        h.granted_folder("com.example.f", "files.read:x", &[])
            .is_err()
    );
    h.revoke("com.example.f", "files.write:out").unwrap();
    assert!(
        h.granted_folder("com.example.f", "files.write:out", &[])
            .is_err()
    );
}

#[test]
fn signed_code_needs_a_trusted_signer() {
    let e = env();
    let src = code_plugin(&e.src, "com.example.s", "1.0.0", &["files.write:out"]);
    let key = keygen("Tester").unwrap();
    sign_folder(&src, &key);
    let mut h = e.host();
    let p = h.stage(&src).unwrap();
    assert!(matches!(p.trust, Trust::Untrusted(ref s) if s.name == "Tester"));
    h.install_staged(&p.token, InstallOptions::default())
        .unwrap();
    let want = needs(&h, "com.example.s");
    assert!(
        matches!(&want[0], Approval::TrustSigner(s) if s.key == key.key),
        "{want:?}"
    );
    assert!(want[0].to_string().contains(&trust::fingerprint(&key.key)));
    let out = e.outside.join("o");
    fs::create_dir_all(&out).unwrap();
    assert!(
        h.grant("com.example.s", "files.write:out", Some(&out), &[])
            .is_err(),
        "untrusted is unsigned code"
    );
    assert!(h.trust_signer("zz", "x").is_err());
    h.trust_signer(&key.key, "Tester").unwrap();
    h.grant("com.example.s", "files.write:out", Some(&out), &[])
        .unwrap();
    assert_eq!(status(&h, "com.example.s"), &Status::Active);
    h.untrust_signer(&key.key).unwrap();
    assert!(h.trusted_signers().is_empty());
}

#[test]
fn an_update_keeps_grants_still_asked_for_and_asks_for_new_ones() {
    let e = env();
    let mut h = e.host();
    h.set_allow_unsigned_code(true).unwrap();
    h.install(
        &code_plugin(
            &e.src.join("1"),
            "com.example.c",
            "1.0.0",
            &["clock", "photo_pixels"],
        ),
        InstallOptions::default(),
    )
    .unwrap();
    h.grant("com.example.c", "clock", None, &[]).unwrap();
    h.grant("com.example.c", "photo_pixels", None, &[]).unwrap();
    h.install(
        &code_plugin(
            &e.src.join("2"),
            "com.example.c",
            "1.1.0",
            &["clock", "network:x.example.com"],
        ),
        InstallOptions::default(),
    )
    .unwrap();
    assert_eq!(
        h.granted("com.example.c"),
        [Permission::Clock].into_iter().collect()
    );
    assert_eq!(
        needs(&h, "com.example.c"),
        [Approval::Grant(
            Permission::parse("network:x.example.com").unwrap()
        )]
    );
}

#[test]
fn id_clashes_resolve_to_one_winner() {
    let e = env();
    let old = data_plugin(&e.user, "old", "com.example.a", "1.0.0");
    data_plugin(&e.user, "new", "com.example.a", "1.5.0");
    let mut h = e.host();
    h.approve_files("com.example.a").unwrap();
    assert_eq!(
        h.active("com.example.a")
            .unwrap()
            .manifest
            .as_ref()
            .unwrap()
            .version
            .to_string(),
        "1.5.0"
    );
    let loser = h.plugins().iter().find(|p| p.dir == old).unwrap();
    assert!(matches!(&loser.status, Status::Shadowed { by } if by.ends_with("new.dsplugin")));
    assert_eq!(h.data(Kind::Looks).0.len(), 1);

    let e = env();
    data_plugin(&e.user, "aaa", "com.example.b", "1.0.0");
    data_plugin(&e.user, "com.example.b", "com.example.b", "1.0.0");
    let h = e.host();
    let w = h
        .plugins()
        .iter()
        .find(|p| !matches!(p.status, Status::Shadowed { .. }))
        .unwrap();
    assert!(w.dir.ends_with("com.example.b.dsplugin"));
}

#[test]
fn development_folders_reload_without_approval() {
    let e = env();
    let mut h = e.host();
    let dev = code_plugin(&e.outside, "com.example.dev", "0.1.0", &["clock"]);
    assert!(
        h.add_dev_folder(&dev)
            .unwrap_err()
            .to_string()
            .contains("Allow unsigned code")
    );
    h.set_allow_unsigned_code(true).unwrap();
    h.add_dev_folder(&dev).unwrap();
    h.add_dev_folder(&dev).unwrap();
    assert_eq!(h.dev_folders().len(), 1);
    assert_eq!(
        needs(&h, "com.example.dev"),
        [Approval::Grant(Permission::Clock)]
    );
    // A first decision about a development folder leaves it enabled.
    h.grant("com.example.dev", "clock", None, &[]).unwrap();
    assert_eq!(status(&h, "com.example.dev"), &Status::Active);
    // Edit it: still active, no approval needed.
    let toml = fs::read_to_string(dev.join(MANIFEST_FILE)).unwrap();
    fs::write(dev.join(MANIFEST_FILE), toml.replace("0.1.0", "0.2.0")).unwrap();
    h.reload();
    assert_eq!(status(&h, "com.example.dev"), &Status::Active);
    assert!(h.post_processor("com.example.dev", "border").is_ok());
    // A dev copy wins over an installed one with the same id.
    h.install(
        &code_plugin(&e.src, "com.example.dev", "5.0.0", &["clock"]),
        InstallOptions::default(),
    )
    .unwrap();
    assert_eq!(h.active("com.example.dev").unwrap().origin, Origin::Dev);
    assert!(
        h.user_folder("com.example.dev").is_ok(),
        "the installed copy can still be removed"
    );
    // Unsigned code off: a dev folder stops.
    h.set_allow_unsigned_code(false).unwrap();
    assert!(needs(&h, "com.example.dev").contains(&Approval::AllowUnsignedCode));
    h.set_allow_unsigned_code(true).unwrap();
    h.remove_dev_folder(&h.dev_folders()[0].clone()).unwrap();
    assert!(h.plugins().iter().all(|p| p.origin != Origin::Dev));
    assert!(
        h.add_dev_folder(&e.src)
            .unwrap_err()
            .to_string()
            .contains("no plugin.toml")
    );
    assert!(
        h.add_dev_folder(&h.user_folder("com.example.dev").unwrap())
            .unwrap_err()
            .to_string()
            .contains("Darkslide's own")
    );
}

#[test]
fn data_and_post_processors_come_from_active_plugins() {
    let e = env();
    let mut h = e.host();
    h.set_allow_unsigned_code(true).unwrap();
    h.install(
        &code_plugin(&e.src, "com.example.b", "1.0.0", &[]),
        InstallOptions::default(),
    )
    .unwrap();
    let (pp, params, version) = h.post_processor("com.example.b", "border").unwrap();
    assert_eq!((params.len(), version.as_str()), (2, "1.0.0"));
    let (again, ..) = h.post_processor("com.example.b", "border").unwrap();
    assert!(Arc::ptr_eq(&pp, &again), "compiled once");
    assert!(h.post_processor("com.example.b", "nope").is_err());

    let broken = data_plugin(&e.src, "broken", "com.example.broken", "1.0.0");
    fs::write(
        broken.join("looks.json"),
        r#"[{"id":"ok","name":"Ok"},{"id":"bad","name":"B","values":{"x.y":1}}]"#,
    )
    .unwrap();
    h.install(&broken, InstallOptions::default()).unwrap();
    let (data, errors) = h.data(Kind::Looks);
    assert_eq!(data.len(), 1, "the good item loads");
    assert!(
        errors[0].contains("N com.example.broken (looks.json): look `bad`"),
        "{errors:?}"
    );
    fs::write(
        h.user_folder("com.example.broken")
            .unwrap()
            .join("looks.json"),
        "[{",
    )
    .unwrap();
    h.reload();
    h.approve_files("com.example.broken").unwrap();
    assert!(h.data(Kind::Looks).1[0].contains("line 1"));
}

#[test]
fn missing_files_and_unreadable_manifests_are_load_errors() {
    let e = env();
    let d = data_plugin(&e.user, "a", "com.example.a", "1.0.0");
    fs::remove_file(d.join("looks.json")).unwrap();
    let d2 = data_plugin(&e.user, "b", "com.example.b", "1.0.0");
    fs::write(d2.join(MANIFEST_FILE), "manifest = 1").unwrap();
    let d3 = data_plugin(&e.user, "c", "com.example.c", "1.0.0");
    std::os::unix::fs::symlink("/etc/hosts", d3.join("link")).unwrap();
    let h = e.host();
    assert!(err(status(&h, "com.example.a")).contains("looks.json"));
    assert!(
        h.plugins()
            .iter()
            .any(|p| p.manifest.is_none() && matches!(p.status, Status::Error(_)))
    );
    assert!(
        h.plugins()
            .iter()
            .any(|p| p.dir == d3 && matches!(&p.status, Status::Error(m) if m.contains("link")))
    );
}

#[test]
fn other_platforms_are_listed_not_loaded() {
    let e = env();
    let d = data_plugin(&e.user, "x", "com.example.ipad-only", "1.0.0");
    let t = fs::read_to_string(d.join(MANIFEST_FILE)).unwrap();
    fs::write(
        d.join(MANIFEST_FILE),
        t.replace("api = 1\n", "api = 1\nplatforms = [\"ipados\"]\n"),
    )
    .unwrap();
    let h = e.host();
    assert!(matches!(
        status(&h, "com.example.ipad-only"),
        Status::Unsupported(_)
    ));
}

#[test]
fn leftovers_and_unreadable_settings_are_handled() {
    let e = env();
    fs::create_dir_all(e.user.join(".staging-abc/x")).unwrap();
    let hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 3600);
    fs::File::open(e.user.join(".staging-abc"))
        .unwrap()
        .set_modified(hours_ago)
        .unwrap();
    fs::write(e.user.join("state.json"), "garbage").unwrap();
    let h = e.host();
    assert!(!e.user.join(".staging-abc").exists());
    assert!(h.state_error.as_deref().unwrap().contains("unreadable"));
}

#[test]
fn approvals_read_as_sentences() {
    let s = Signer {
        name: "A".into(),
        key: "k".into(),
    };
    let all = [
        Approval::Changed,
        Approval::NotInstalled,
        Approval::TrustSigner(s),
        Approval::AllowUnsignedCode,
        Approval::Grant(Permission::Clock),
    ];
    let texts: Vec<String> = all.iter().map(ToString::to_string).collect();
    assert!(texts.iter().all(|t| !t.is_empty()));
    assert!(texts[2].contains("key ") && texts[4].contains("clock"));
    assert_eq!(pin_label(UNSIGNED), "unsigned");
}

/// Review probe H1/N4: allowing one unsigned plug-in doesn't flip the global switch,
/// gives it no file, network or account permission, and an unsigned update asks again.
#[test]
fn allowing_one_unsigned_plugin_is_per_plugin() {
    let e = env();
    let mut h = e.host();
    h.install(
        &code_plugin(
            &e.src.join("1"),
            "com.example.u",
            "1.0.0",
            &["network:api.example.com"],
        ),
        InstallOptions::default(),
    )
    .unwrap();
    assert!(needs(&h, "com.example.u").contains(&Approval::AllowUnsignedCode));
    h.allow_unsigned("com.example.u").unwrap();
    assert!(!h.allow_unsigned_code(), "the global switch stays off");
    assert_eq!(
        needs(&h, "com.example.u"),
        [Approval::Grant(
            Permission::parse("network:api.example.com").unwrap()
        )]
    );
    assert!(
        h.grant("com.example.u", "network:api.example.com", None, &[])
            .is_err(),
        "still no network"
    );
    // Another unsigned plug-in is not allowed by that.
    h.install(
        &code_plugin(&e.src.join("2"), "com.example.v", "1.0.0", &[]),
        InstallOptions::default(),
    )
    .unwrap();
    assert_eq!(needs(&h, "com.example.v"), [Approval::AllowUnsignedCode]);
    // An unsigned update (anyone could have made it) asks again.
    h.install(
        &code_plugin(&e.src.join("3"), "com.example.v", "1.0.1", &[]),
        InstallOptions::default(),
    )
    .unwrap();
    h.allow_unsigned("com.example.v").unwrap();
    assert_eq!(status(&h, "com.example.v"), &Status::Active);
    h.install(
        &code_plugin(&e.src.join("4"), "com.example.v", "1.0.2", &[]),
        InstallOptions::default(),
    )
    .unwrap();
    assert_eq!(needs(&h, "com.example.v"), [Approval::AllowUnsignedCode]);
    // Only unsigned plug-ins take it.
    data_plugin(&e.builtin, "b", "darkslide.b", "1.0.0");
    let mut h = e.host();
    assert!(h.allow_unsigned("darkslide.b").is_err());
    assert!(h.allow_unsigned("com.example.none").is_err());
}

/// Review probe H1: an unsigned update of a trusted-signed plug-in never runs with the
/// signed one's grants, whatever is clicked afterwards.
#[test]
fn an_unsigned_update_of_a_signed_plugin_never_inherits_its_grants() {
    let e = env();
    let mut h = e.host();
    let key = keygen("Victim Corp").unwrap();
    let v1 = code_plugin(
        &e.src.join("1"),
        "com.victim.x",
        "1.0.0",
        &["network:api.example.com"],
    );
    sign_folder(&v1, &key);
    h.trust_signer(&key.key, "Victim Corp").unwrap();
    h.install(&v1, InstallOptions::default()).unwrap();
    h.grant("com.victim.x", "network:api.example.com", None, &[])
        .unwrap();
    assert_eq!(status(&h, "com.victim.x"), &Status::Active);
    let v2 = code_plugin(
        &e.src.join("2"),
        "com.victim.x",
        "1.0.1",
        &["network:api.example.com"],
    );
    assert!(h.install(&v2, InstallOptions::default()).is_err());
    h.set_allow_unsigned_code(true).unwrap();
    assert!(
        h.install(&v2, InstallOptions::default()).is_err(),
        "not even with unsigned code allowed"
    );
    assert_eq!(
        h.active("com.victim.x").unwrap().trust,
        Trust::Signed(Signer {
            name: "Victim Corp".into(),
            key: key.key
        })
    );
}

/// Review probe H3: the module that runs is the one verified at load; a file swapped on
/// disk afterwards never runs (and a reload sees the change and waits for approval).
#[test]
fn a_module_swapped_on_disk_never_runs() {
    let e = env();
    let mut h = e.host();
    h.set_allow_unsigned_code(true).unwrap();
    h.install(
        &code_plugin(&e.src, "com.example.x", "1.0.0", &[]),
        InstallOptions::default(),
    )
    .unwrap();
    let dir = h.user_folder("com.example.x").unwrap();
    let swapped = "(module (memory (export \"memory\") 1) (func (export \"ds_api\") (result i32) (i32.const 1)) \
        (func (export \"ds_alloc\") (param i32) (result i32) (i32.const 1024)) \
        (func (export \"ds_post_process\") (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32) (i32.const 9)))";
    fs::write(dir.join("b.wat"), swapped).unwrap();
    let (pp, ..) = h.post_processor("com.example.x", "border").unwrap();
    let mut px = vec![0u8; 4];
    let p = serde_json::json!({"width": 0.0, "color": "#000000"});
    let serde_json::Value::Object(p) = p else {
        unreachable!()
    };
    pp.run(
        crate::wasm::Pixels::Rgba8(&mut px),
        1,
        1,
        [0, 0, 1, 1],
        p,
        &serde_json::json!({}),
        &|| false,
    )
    .unwrap();
    h.reload();
    assert_eq!(needs(&h, "com.example.x"), [Approval::Changed]);
    assert!(h.post_processor("com.example.x", "border").is_err());
}

/// Review probe M5: Finder's files in an installed plug-in change nothing.
#[test]
fn finder_metadata_in_an_installed_plugin_changes_nothing() {
    let e = env();
    let mut h = e.host();
    h.install(
        &data_plugin(&e.src, "a", "com.example.a", "1.0.0"),
        InstallOptions::default(),
    )
    .unwrap();
    let dir = h.user_folder("com.example.a").unwrap();
    fs::write(dir.join(".DS_Store"), b"\0\0\0\x01Bud1").unwrap();
    fs::write(dir.join("._looks.json"), b"junk").unwrap();
    h.reload();
    assert_eq!(status(&h, "com.example.a"), &Status::Active);
}

/// N6: an install that crashed between moving the old version aside and renaming the
/// new one in loses nothing: the old version comes back at the next launch.
#[test]
fn a_crashed_install_restores_the_previous_version() {
    let e = env();
    let mut h = e.host();
    h.install(
        &data_plugin(&e.src, "a", "com.example.a", "1.0.0"),
        InstallOptions::default(),
    )
    .unwrap();
    let dest = h.user_folder("com.example.a").unwrap();
    let aside = e.user.join(".replaced/com.example.a@0123abcd");
    fs::create_dir_all(aside.parent().unwrap()).unwrap();
    fs::rename(&dest, &aside).unwrap(); // the crash point
    let h = e.host();
    assert!(dest.exists() && !aside.exists());
    assert_eq!(status(&h, "com.example.a"), &Status::Active);
}

/// A trusted-signed `com.victim.x` with `network:api.example.com` granted.
fn victim(e: &Env) -> Host {
    let mut h = e.host();
    let key = keygen("Victim Corp").unwrap();
    let v1 = code_plugin(
        &e.src.join("v1"),
        "com.victim.x",
        "1.0.0",
        &["network:api.example.com"],
    );
    sign_folder(&v1, &key);
    h.trust_signer(&key.key, "Victim Corp").unwrap();
    h.install(&v1, InstallOptions::default()).unwrap();
    h.grant("com.victim.x", "network:api.example.com", None, &[])
        .unwrap();
    assert_eq!(status(&h, "com.victim.x"), &Status::Active);
    h
}

/// Round-2 probe (H1 via developer mode): an unsigned development folder with an
/// installed id doesn't run under it, and never gets its grants.
#[test]
fn a_dev_folder_never_takes_an_installed_ids_grants() {
    let e = env();
    let mut h = victim(&e);
    let dev = code_plugin(
        &e.outside,
        "com.victim.x",
        "0.0.1",
        &["network:api.example.com"],
    );
    h.set_allow_unsigned_code(true).unwrap();
    h.add_dev_folder(&dev).unwrap();
    let d = h
        .plugins()
        .iter()
        .find(|p| p.origin == Origin::Dev)
        .unwrap();
    assert!(
        matches!(&d.status, Status::Error(m) if m.contains("different key")),
        "{:?}",
        d.status
    );
    assert!(h.granted_to(d).is_empty());
    let installed = h.active("com.victim.x").unwrap();
    assert_eq!(installed.origin, Origin::User);
    // A dev folder of a new id keeps its own decisions, apart from any later install.
    let mine = code_plugin(
        &e.outside.join("m"),
        "com.example.mine",
        "0.1.0",
        &["network:api.example.com"],
    );
    h.add_dev_folder(&mine).unwrap();
    h.grant("com.example.mine", "network:api.example.com", None, &[])
        .unwrap();
    assert_eq!(status(&h, "com.example.mine"), &Status::Active);
    h.remove_dev_folder(&h.dev_folders()[1].clone()).unwrap();
    h.install(
        &code_plugin(
            &e.src.join("m"),
            "com.example.mine",
            "1.0.0",
            &["network:api.example.com"],
        ),
        InstallOptions::default(),
    )
    .unwrap();
    assert!(
        h.granted("com.example.mine").is_empty(),
        "the installed copy starts without the dev folder's grants"
    );
}

/// Round-2 probe (H1 via a stale token): the pin is checked again at install time.
#[test]
fn a_stale_staging_token_is_checked_again() {
    let e = env();
    let mut h = e.host();
    let evil = code_plugin(
        &e.src.join("evil"),
        "com.victim.x",
        "2.0.0",
        &["network:api.example.com"],
    );
    let token = h.stage(&evil).unwrap().token; // nothing pinned yet
    let key = keygen("Victim Corp").unwrap();
    let v1 = code_plugin(
        &e.src.join("v1"),
        "com.victim.x",
        "1.0.0",
        &["network:api.example.com"],
    );
    sign_folder(&v1, &key);
    h.trust_signer(&key.key, "Victim Corp").unwrap();
    h.install(&v1, InstallOptions::default()).unwrap();
    h.grant("com.victim.x", "network:api.example.com", None, &[])
        .unwrap();
    let r = h.install_staged(&token, InstallOptions::default());
    assert!(r.unwrap_err().to_string().contains("different key"));
    assert_eq!(
        h.active("com.victim.x")
            .unwrap()
            .manifest
            .as_ref()
            .unwrap()
            .version
            .to_string(),
        "1.0.0"
    );
    assert!(
        !fs::read_dir(&e.user)
            .unwrap()
            .flatten()
            .any(|d| d.file_name().to_string_lossy().starts_with(STAGING_PREFIX))
    );
}

/// A staging another host has open (recent) is left alone; an old one is cleaned.
#[test]
fn only_old_stagings_are_cleaned() {
    let e = env();
    let mut h = e.host();
    let p = h
        .stage(&data_plugin(&e.src, "a", "com.example.a", "1.0.0"))
        .unwrap();
    let _other = e.host(); // a second host opening the same folder
    assert!(
        h.install_staged(&p.token, InstallOptions::default())
            .is_ok()
    );
    let old = e.user.join(".staging-old");
    fs::create_dir_all(&old).unwrap();
    let hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 3600);
    fs::File::open(&old)
        .unwrap()
        .set_modified(hours_ago)
        .unwrap();
    let _h2 = e.host();
    assert!(!old.exists());
}

/// Staged copies expire: an old token is refused, and the next drop clears it out.
#[test]
fn staging_tokens_expire() {
    let e = env();
    let mut h = e.host();
    let old = h
        .stage(&data_plugin(&e.src, "a", "com.example.a", "1.0.0"))
        .unwrap()
        .token;
    let long_ago = std::time::Instant::now()
        .checked_sub(STAGING_TTL * 2)
        .unwrap();
    h.staged.get_mut(&old).unwrap().at = long_ago;
    assert!(
        h.install_staged(&old, InstallOptions::default())
            .unwrap_err()
            .to_string()
            .contains("too long")
    );
    let again = h
        .stage(&data_plugin(&e.src, "a", "com.example.a", "1.0.0"))
        .unwrap()
        .token;
    h.staged.get_mut(&again).unwrap().at = long_ago;
    let dir = h.staged[&again].dir.clone();
    h.stage(&data_plugin(
        &e.src.join("b"),
        "b",
        "com.example.b",
        "1.0.0",
    ))
    .unwrap();
    assert!(!h.staged.contains_key(&again) && !dir.exists());
}

/// Round 3: a copy kept aside by an update isn't restored after the user removed it.
#[test]
fn a_removed_plugin_doesnt_come_back_from_replaced() {
    let e = env();
    let mut h = e.host();
    h.install(
        &data_plugin(&e.src, "a", "com.example.a", "1.0.0"),
        InstallOptions::default(),
    )
    .unwrap();
    let dest = h.user_folder("com.example.a").unwrap();
    let aside = e.user.join(".replaced/com.example.a@0123abcd");
    fs::create_dir_all(aside.parent().unwrap()).unwrap();
    fs::rename(&dest, &aside).unwrap(); // left by a failed update, then the user removed it
    h.forget("com.example.a").unwrap();
    assert!(!aside.exists());
    let h = e.host();
    assert!(!dest.exists());
    assert!(h.plugins().iter().all(|p| p.id() != Some("com.example.a")));
}

/// Round 3 probe: unlinking a development folder drops its decisions, so another folder
/// linked later with that id starts without them.
#[test]
fn unlinking_a_dev_folder_drops_its_grants() {
    let e = env();
    let mut h = e.host();
    h.set_allow_unsigned_code(true).unwrap();
    let d1 = code_plugin(
        &e.outside.join("1"),
        "com.dev.x",
        "0.1.0",
        &["network:api.example.com"],
    );
    h.add_dev_folder(&d1).unwrap();
    h.grant("com.dev.x", "network:api.example.com", None, &[])
        .unwrap();
    h.allow_unsigned("com.dev.x").unwrap();
    let linked = h.dev_folders()[0].clone();
    h.remove_dev_folder(&linked).unwrap();
    assert!(!h.state.plugins.contains_key("dev:com.dev.x"));
    let d2 = code_plugin(
        &e.outside.join("2"),
        "com.dev.x",
        "0.9.0",
        &["network:api.example.com"],
    );
    h.add_dev_folder(&d2).unwrap();
    assert!(h.granted("com.dev.x").is_empty());
}

/// Audit: an unsigned package claiming an installed unsigned id (nothing ties it to the
/// previous author) never inherits its file, network or account grants, even with
/// unsigned code allowed; a signed update from the pinned key keeps them.
#[test]
fn an_unsigned_update_never_inherits_sensitive_grants() {
    let e = env();
    let mut h = e.host();
    h.set_allow_unsigned_code(true).unwrap();
    let perms = ["files.write:out", "network:api.example.com", "clock"];
    h.install(
        &code_plugin(&e.src.join("1"), "com.example.u", "1.0.0", &perms),
        InstallOptions::default(),
    )
    .unwrap();
    h.grant("com.example.u", "files.write:out", Some(&e.outside), &[])
        .unwrap();
    h.grant("com.example.u", "network:api.example.com", None, &[])
        .unwrap();
    h.grant("com.example.u", "clock", None, &[]).unwrap();
    assert_eq!(status(&h, "com.example.u"), &Status::Active);
    // A look-alike with the same id.
    h.install(
        &code_plugin(&e.src.join("2"), "com.example.u", "1.0.1", &perms),
        InstallOptions::default(),
    )
    .unwrap();
    assert_eq!(
        h.granted("com.example.u"),
        [Permission::Clock].into_iter().collect()
    );
    assert!(
        h.granted_folder("com.example.u", "files.write:out", &[])
            .is_err()
    );
    assert_eq!(
        needs(&h, "com.example.u"),
        [
            Approval::Grant(Permission::parse("files.write:out").unwrap()),
            Approval::Grant(Permission::parse("network:api.example.com").unwrap()),
        ]
    );

    let key = keygen("Jane").unwrap();
    h.trust_signer(&key.key, "Jane").unwrap();
    let s1 = code_plugin(&e.src.join("3"), "com.example.s", "1.0.0", &perms[..2]);
    sign_folder(&s1, &key);
    h.install(&s1, InstallOptions::default()).unwrap();
    h.grant("com.example.s", "files.write:out", Some(&e.outside), &[])
        .unwrap();
    h.grant("com.example.s", "network:api.example.com", None, &[])
        .unwrap();
    let s2 = code_plugin(&e.src.join("4"), "com.example.s", "1.1.0", &perms[..2]);
    sign_folder(&s2, &key);
    h.install(&s2, InstallOptions::default()).unwrap();
    assert_eq!(status(&h, "com.example.s"), &Status::Active);
    assert_eq!(
        h.granted_folder("com.example.s", "files.write:out", &[])
            .unwrap(),
        e.outside
    );
}
