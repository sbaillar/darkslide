use super::*;

fn v(s: &str) -> semver::Version {
    semver::Version::parse(s).unwrap()
}

fn parse(text: &str) -> Result<Manifest, PluginError> {
    Manifest::parse(text, &v("0.2.0"))
}

const BASE: &str =
    "manifest = 1\nid = \"com.example.p\"\nname = \"P\"\nversion = \"1.0.0\"\napi = 1\n";

fn with(extra: &str) -> String {
    format!("{BASE}{extra}")
}

fn err(text: &str) -> String {
    parse(text).unwrap_err().to_string()
}

#[test]
fn parses_a_full_manifest() {
    let m = parse(&with(
        r#"
min_luxtable = "0.1.0"
authors = ["A"]
summary = "s"
platforms = ["macos"]
permissions = ["photo_metadata", "clock"]

[[contributes.looks]]
id = "looks"
file = "looks.json"
contract = 1

[[contributes.export_post_process]]
id = "border"
name = "Border"
module = "b.wat"
contract = 1
params = [{ key = "width", type = "float", max = 0.2, default = 0.03 }]
"#,
    ))
    .unwrap();
    assert_eq!(m.id, "com.example.p");
    assert_eq!(m.version, v("1.0.0"));
    assert_eq!(m.platforms, [Platform::Macos].into_iter().collect());
    assert_eq!(m.contributions.len(), 2);
    let pp = m
        .contributions
        .iter()
        .find(|c| c.kind == Kind::ExportPostProcess)
        .unwrap();
    assert_eq!(pp.entry.as_deref(), Some("lt_post_process"));
    assert_eq!(pp.params[0].key, "width");
    let looks = m
        .contributions
        .iter()
        .find(|c| c.kind == Kind::Looks)
        .unwrap();
    assert_eq!(looks.name, "looks");
    assert!(m.has_code());
    assert_eq!(m.files(), ["b.wat", "looks.json"].into_iter().collect());
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
}

#[test]
fn defaults_to_every_platform_and_no_code() {
    let m = parse(&with(
        "[[contributes.styles]]\nid = \"s\"\nfile = \"s.json\"\ncontract = 1\n",
    ))
    .unwrap();
    assert_eq!(m.platforms.len(), 2);
    assert!(!m.has_code());
}

#[test]
fn top_level_typos_are_errors_but_contribution_extras_are_warnings() {
    // At load an unknown top-level key is a warning (a newer optional key); at author
    // time (`lt-plugin validate`, `pack`) it is an error.
    let typo = with("nmae = \"x\"\n");
    assert!(parse(&typo).unwrap().warnings[0].contains("unknown key `nmae`"));
    let strict = Manifest::parse_with(&typo, &v("0.2.0"), true)
        .unwrap_err()
        .to_string();
    assert!(strict.contains("nmae"), "{strict}");
    let m = parse(&with(
        "[[contributes.looks]]\nid = \"l\"\nfile = \"l.json\"\ncontract = 1\nfuture = true\n",
    ))
    .unwrap();
    assert!(m.warnings[0].contains("unknown key `future`"));
    let m = parse(&with(
        "[[contributes.time_machine]]\nid = \"t\"\ncontract = 1\n",
    ))
    .unwrap();
    assert!(m.warnings[0].contains("not supported"));
    assert_eq!(m.contributions.len(), 0);
}

#[test]
fn versions_and_compatibility() {
    assert!(err(&BASE.replace("manifest = 1", "manifest = 2")).contains("manifest format 2"));
    assert!(err(&BASE.replace("api = 1", "api = 2")).contains("needs a newer Luxtable"));
    assert!(err(&BASE.replace("api = 1", "api = 0")).contains("host API 0"));
    assert!(err(&BASE.replace("1.0.0", "one")).contains("`version`"));
    assert!(err(&with("min_luxtable = \"9.0.0\"\n")).contains("needs Luxtable 9.0.0"));
    assert!(err(&with("min_luxtable = \"x\"\n")).contains("min_luxtable"));
    let c = |n: u32| {
        with(&format!(
            "[[contributes.looks]]\nid = \"l\"\nfile = \"l.json\"\ncontract = {n}\n"
        ))
    };
    assert!(err(&c(2)).contains("needs a newer Luxtable"));
    assert!(err(&c(0)).contains("not supported"));
    assert!(err("not toml at all = = =").contains("plugin.toml"));
}

#[test]
fn identity_is_checked() {
    for bad in [
        "nodots",
        "Com.Example",
        "com..x",
        "com.-x",
        &format!("com.{}", "a".repeat(130)),
    ] {
        assert!(
            err(&BASE.replace("com.example.p", bad)).contains("reverse-DNS"),
            "{bad}"
        );
    }
    assert!(err(&BASE.replace("name = \"P\"", "name = \" \"")).contains("`name`"));
    assert!(validate_id("com.example.my-plugin2").is_ok());
}

#[test]
fn contributions_are_checked() {
    let c = |body: &str| {
        with(&format!(
            "[[contributes.export_post_process]]\nid = \"x\"\ncontract = 1\n{body}"
        ))
    };
    assert!(err(&c("")).contains("`module` is missing"));
    assert!(err(&c("module = \"m.wasm\"\nfile = \"f\"\n")).contains("not a `file`"));
    assert!(err(&c("module = \"m.js\"\n")).contains(".wasm"));
    assert!(err(&c("module = \"../m.wasm\"\n")).contains("inside the plug-in folder"));
    assert!(err(&c("module = \"/m.wasm\"\n")).contains("inside the plug-in folder"));
    assert!(err(&c("module = \"m.wasm\"\nentry = \"bad name\"\n")).contains("entry"));
    assert!(err(&c("module = \"m.wasm\"\nformats = [\"jpeg\"]\n")).contains("export destinations"));
    let p = "params = [{ key = \"a\", type = \"bool\", default = true }, { key = \"a\", type = \"bool\", default = false }]\n";
    assert!(err(&c(&format!("module = \"m.wasm\"\n{p}"))).contains("share the key"));
    assert!(
        err(&c(
            "module = \"m.wasm\"\nparams = [{ key = \"a\", type = \"bool\", default = 1 }]\n"
        ))
        .contains("default")
    );
    let d = |body: &str| {
        with(&format!(
            "[[contributes.looks]]\nid = \"x\"\ncontract = 1\n{body}"
        ))
    };
    assert!(err(&d("")).contains("`file` is missing"));
    assert!(err(&d("file = \"a\"\nmodule = \"m.wasm\"\n")).contains("data contributions"));
    assert!(
        err(&with(
            "[[contributes.looks]]\nid = \"X Y\"\nfile = \"a\"\ncontract = 1\n"
        ))
        .contains("lowercase")
    );
    let two = "[[contributes.looks]]\nid = \"a\"\nfile = \"a\"\ncontract = 1\n[[contributes.styles]]\nid = \"a\"\nfile = \"b\"\ncontract = 1\n";
    assert!(err(&with(two)).contains("share the id"));
}

#[test]
fn permissions_parse_and_print() {
    for s in [
        "photo_pixels",
        "photo_metadata",
        "metadata_write",
        "clock",
        "files.read:inbox",
        "files.write:amazon",
        "network:api.example.com",
        "network:*.example.com",
        "account:google",
    ] {
        assert_eq!(Permission::parse(s).unwrap().to_string(), s);
    }
    for bad in [
        "root",
        "files.read",
        "files.read:",
        "files.read:a/b",
        "network:",
        "network:localhost",
        "network:1.2.3.4",
        "network:https://x.com",
        "network:x.com:80",
        "clock:x",
    ] {
        assert!(Permission::parse(bad).is_err(), "{bad}");
    }
    assert!(Permission::parse("network:x.com").unwrap().is_sensitive());
    assert!(Permission::parse("files.read:x").unwrap().is_sensitive());
    assert!(!Permission::parse("photo_pixels").unwrap().is_sensitive());
    let wild = Permission::parse("network:*.example.com").unwrap();
    assert!(wild.allows_host("a.example.com") && wild.allows_host("A.B.Example.com"));
    assert!(!wild.allows_host("example.com") && !wild.allows_host("badexample.com"));
    assert!(!Permission::Clock.allows_host("example.com"));
    assert!(err(&with("permissions = [\"teleport\"]\n")).contains("unknown permission"));
}

const DEST: &str = r#"
permissions = ["network:photoslibrary.googleapis.com", "network:oauth2.googleapis.com", "network:accounts.google.com", "account:google"]

[accounts.google]
api_hosts = ["photoslibrary.googleapis.com"]
auth = "oauth2-pkce"
authorize_url = "https://accounts.google.com/o/oauth2/v2/auth"
token_url = "https://oauth2.googleapis.com/token"
revoke_url = "https://oauth2.googleapis.com/revoke"
client_id = "1234-abc.apps.googleusercontent.com"
scopes = ["https://www.googleapis.com/auth/photoslibrary.appendonly"]

[[contributes.export_destination]]
id = "google-photos"
name = "Google Photos"
module = "gphotos.wasm"
contract = 1
accounts = ["google"]
formats = ["jpeg", "heic"]
max_file_mb = 200
"#;

#[test]
fn an_online_destination_fits_the_contract() {
    let m = parse(&with(DEST)).unwrap();
    let d = &m.contributions[0];
    assert_eq!(d.kind, Kind::ExportDestination);
    assert_eq!(d.entry.as_deref(), Some("lt_destination"));
    assert_eq!(d.accounts, ["google"]);
    assert_eq!(d.max_file_mb, Some(200));
    assert_eq!(m.accounts["google"].scopes.len(), 1);
}

#[test]
fn a_local_destination_fits_the_contract() {
    let amazon = r#"
permissions = ["files.write:amazon-photos"]
[[contributes.export_destination]]
id = "amazon-photos"
module = "amazon.wasm"
contract = 1
formats = ["original", "jpeg"]
"#;
    let m = parse(&with(amazon)).unwrap();
    assert_eq!(m.contributions[0].accounts.len(), 0);
}

#[test]
fn destination_accounts_and_urls_are_checked() {
    let e = |from: &str, to: &str| err(&with(&DEST.replace(from, to)));
    assert!(e("\"network:oauth2.googleapis.com\", ", "").contains("network:oauth2.googleapis.com"));
    assert!(
        e(
            "https://oauth2.googleapis.com/token",
            "http://oauth2.googleapis.com/token"
        )
        .contains("https://")
    );
    assert!(e("oauth2-pkce", "password").contains("not supported"));
    assert!(e("1234-abc.apps.googleusercontent.com", " ").contains("client_id"));
    assert!(e("accounts = [\"google\"]", "accounts = [\"dropbox\"]").contains("account `dropbox`"));
    assert!(e("formats = [\"jpeg\", \"heic\"]", "formats = []").contains("`formats`"));
    assert!(e("formats = [\"jpeg\", \"heic\"]", "formats = [\"gif\"]").contains("format `gif`"));
    assert!(e(", \"account:google\"", "").contains("account `google`"));
    assert!(e("[accounts.google]", "[accounts.Google]").contains("lowercase"));
    let no_table = with("permissions = [\"account:google\"]\n");
    assert!(err(&no_table).contains("no [accounts.google]"));
}

#[test]
fn https_hosts() {
    assert_eq!(
        https_host("https://a.example.com/x?y"),
        Some("a.example.com")
    );
    assert_eq!(https_host("https://a.example.com"), Some("a.example.com"));
    assert_eq!(https_host("http://a.example.com/"), None);
    assert_eq!(https_host("https://u@a.example.com/"), None);
    assert_eq!(https_host("https:///x"), None);
}

#[test]
fn data_only_permissions_warn() {
    let m = parse(&with("permissions = [\"clock\"]\n[[contributes.looks]]\nid = \"l\"\nfile = \"l.json\"\ncontract = 1\n")).unwrap();
    assert!(m.warnings.iter().any(|w| w.contains("never used")));
    assert!(err(&with("platforms = []\n")).contains("empty"));
}

#[test]
fn kinds_know_their_contracts() {
    for (k, lo, hi) in CONTRACTS {
        assert_eq!(k.contracts(), (*lo, *hi));
        assert_ne!(k.key().len(), 0);
    }
    assert!(Kind::ExportPostProcess.is_code() && !Kind::Looks.is_code());
}

#[test]
fn accounts_tie_tokens_to_their_api_hosts_and_known_names_to_real_sign_ins() {
    let e = |from: &str, to: &str| err(&with(&DEST.replace(from, to)));
    assert!(
        e(
            "api_hosts = [\"photoslibrary.googleapis.com\"]",
            "api_hosts = []"
        )
        .contains("api_hosts")
    );
    assert!(
        e(
            "api_hosts = [\"photoslibrary.googleapis.com\"]",
            "api_hosts = [\"collector.example\"]"
        )
        .contains("needs the permission")
    );
    assert!(
        e(
            "api_hosts = [\"photoslibrary.googleapis.com\"]",
            "api_hosts = [\"*.googleapis.com\"]"
        )
        .contains("plain host")
    );
    // A look-alike "google" account with its own sign-in is refused…
    let fake = DEST
        .replace(
            "https://accounts.google.com/o/oauth2/v2/auth",
            "https://login.example.com/auth",
        )
        .replace(
            "\"network:accounts.google.com\"",
            "\"network:login.example.com\"",
        );
    assert!(err(&with(&fake)).contains("not google's sign-in"));
    // …but fine under its own name.
    let own = fake
        .replace("account:google", "account:example")
        .replace("[accounts.google]", "[accounts.example]")
        .replace("accounts = [\"google\"]", "accounts = [\"example\"]");
    assert!(parse(&with(&own)).is_ok());
}

#[test]
fn names_that_pass_for_luxtable_are_refused() {
    assert!(
        err(&BASE.replace("name = \"P\"", "name = \"Luxtable Helper\"")).contains("would pass for")
    );
    let builtin = BASE
        .replace("com.example.p", "luxtable.p")
        .replace("name = \"P\"", "name = \"Luxtable Looks\"");
    assert!(parse(&builtin).is_ok());
    let m = parse(&BASE.replace("name = \"P\"", "name = \"Film\\nBorder\"")).unwrap();
    assert_eq!(m.name, "Film Border");
    assert!(
        err(&BASE.replace("name = \"P\"", &format!("name = \"{}\"", "x".repeat(60))))
            .contains("characters")
    );
}

#[test]
fn aprons_are_for_post_processors() {
    let pp = |a: u32| {
        with(&format!(
            "[[contributes.export_post_process]]\nid = \"x\"\nmodule = \"m.wat\"\ncontract = 1\napron_rows = {a}\n"
        ))
    };
    assert_eq!(parse(&pp(8)).unwrap().contributions[0].apron_rows, 8);
    assert!(err(&pp(300)).contains("apron_rows"));
    assert!(
        err(&with(
            "[[contributes.looks]]\nid = \"l\"\nfile = \"l.json\"\ncontract = 1\napron_rows = 2\n"
        ))
        .contains("apron_rows")
    );
}

const FEATURE: &str = "[[contributes.native_feature]]\nid = \"immich\"\ncontract = 1\nuses = [\"Your Immich server\"]\n";

fn builtin(extra: &str) -> String {
    format!(
        "manifest = 1\nid = \"luxtable.immich\"\nname = \"Immich\"\nversion = \"1.0.0\"\napi = 1\n{extra}"
    )
}

#[test]
fn a_native_feature_is_a_switch_without_files() {
    let m = parse(&builtin(&format!(
        "permissions = [\"network:immich.example.org\"]\n{FEATURE}"
    )))
    .unwrap();
    let c = &m.contributions[0];
    assert_eq!(
        (c.kind, c.id.as_str(), c.contract),
        (Kind::NativeFeature, "immich", 1)
    );
    assert_eq!(c.uses, ["Your Immich server"]);
    assert!(m.files().is_empty() && !m.has_code());
    assert_eq!(m.native_features().collect::<Vec<_>>(), ["immich"]);
    // Its permissions describe the app's own code: no "never used" warning.
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
    assert!(!Kind::NativeFeature.has_file() && Kind::Looks.has_file());
    assert_eq!(Kind::NativeFeature.key(), "native_feature");
}

#[test]
fn only_bundled_plugins_declare_native_features() {
    // A third-party package is refused (at validation, so at install and at load).
    assert!(err(&with(FEATURE)).contains("only for Luxtable's own"));
    // Strict (author time) as well.
    assert!(
        Manifest::parse_with(&with(FEATURE), &v("0.2.0"), true)
            .unwrap_err()
            .to_string()
            .contains("only for Luxtable's own")
    );
}

#[test]
fn native_features_are_checked() {
    let e = |extra: &str| err(&builtin(extra));
    assert!(
        e(&FEATURE.replace("contract = 1", "contract = 1\nfile = \"x.json\"")).contains("names no")
    );
    assert!(
        e(&FEATURE.replace("contract = 1", "contract = 1\nmodule = \"x.wasm\""))
            .contains("names no")
    );
    assert!(
        e(&FEATURE.replace(
            "contract = 1",
            "contract = 1\nparams = [{ key = \"a\", type = \"bool\", default = true }]"
        ))
        .contains("no `params`")
    );
    assert!(e(&FEATURE.replace("contract = 1", "contract = 2")).contains("newer Luxtable"));
    // `uses` belongs to native features only.
    assert!(
        err(&with(
            "[[contributes.looks]]\nid = \"l\"\nfile = \"l.json\"\ncontract = 1\nuses = [\"x\"]\n"
        ))
        .contains("`uses` is for native features")
    );
    // A feature this Luxtable doesn't have is skipped with a warning (an error at author time).
    let newer = builtin(&FEATURE.replace("\"immich\"", "\"teleport\""));
    let m = parse(&newer).unwrap();
    assert_eq!(m.contributions.len(), 0);
    assert!(m.warnings[0].contains("not a feature of this Luxtable"));
    assert!(Manifest::parse_with(&newer, &v("0.2.0"), true).is_err());
}
