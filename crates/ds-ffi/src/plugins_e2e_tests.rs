//! The plug-in system end to end, as a user and an author meet it: the `ds-plugin` CLI
//! on the reference plug-ins in `plugins/` (new, validate, pack to a folder and a zip,
//! keygen, sign), then the app's host over FFI: install a zip and a folder, unsigned
//! and signed, trust and pinning, update and downgrade, a development folder, a
//! tampered bundled plug-in, the looks pack, and Film Border changing a real export.

use std::fs;
use std::path::{Path, PathBuf};

use ds_core::Size;
use ds_export::{ExportFormat, Image16, OutputSpace};
use futures_executor::block_on;

use super::*;
use crate::export::{
    ExportColorSpace, ExportFileFormat, ExportRecipeSpec, ExportSize, ExportSource, export_batch,
};
use crate::jobs::CancelToken;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn cli(args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    ds_plugin::cli::run(&args).unwrap_or_else(|e| panic!("ds-plugin {args:?}: {e}"))
}

fn p(path: &Path) -> String {
    path.display().to_string()
}

fn record(h: &PluginHost, id: &str) -> PluginRecord {
    h.list()
        .into_iter()
        .find(|r| r.id == id && r.state != PluginState::Shadowed)
        .unwrap_or_else(|| panic!("no {id}"))
}

fn install(h: &PluginHost, path: &Path, downgrade: bool) -> Result<PluginInstallResult, DsError> {
    let preview = h.preview(p(path))?;
    h.install(preview.token, downgrade)
}

/// A copy of Film Border at `dir` with another version (and optionally another id).
fn border_copy(dir: &Path, id: &str, version: &str) -> PathBuf {
    let src = repo().join("plugins/film-border.dsplugin");
    fs::create_dir_all(dir).unwrap();
    fs::copy(src.join("border.wat"), dir.join("border.wat")).unwrap();
    let toml = fs::read_to_string(src.join("plugin.toml"))
        .unwrap()
        .replace("com.example.film-border", id)
        .replace("version = \"1.1.0\"", &format!("version = \"{version}\""));
    fs::write(dir.join("plugin.toml"), toml).unwrap();
    dir.to_path_buf()
}

/// A 64×32 JPEG "original", mid grey.
fn jpeg(dir: &Path) -> PathBuf {
    let img = Image16::new(Size::new(64, 32), vec![30_000; 64 * 32 * 3]).unwrap();
    let bytes = ds_export::encode(
        &img,
        ExportFormat::Jpeg { quality: 95 },
        ds_export::icc::profile(OutputSpace::Srgb),
        None,
        None,
    )
    .unwrap();
    let path = dir.join("Frame.JPG");
    fs::write(&path, bytes).unwrap();
    path
}

fn png16(path: &Path) -> (u32, u32, Vec<u16>) {
    let mut r = ::png::Decoder::new(std::io::BufReader::new(fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut buf = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut buf).unwrap();
    let px = buf[..info.buffer_size()]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_be_bytes(*b))
        .collect();
    (info.width, info.height, px)
}

fn recipe(name: &str, dest: &Path, steps: Vec<ExportPostProcessStep>) -> ExportRecipeSpec {
    ExportRecipeSpec {
        name: name.into(),
        format: ExportFileFormat::Png16,
        size: ExportSize::Original,
        allow_upscale: false,
        space: ExportColorSpace::Srgb,
        sharpen: 0.0,
        file_name_template: format!("{{name}}_{name}"),
        destination: p(dest),
        include_exif: false,
        strip_gps: false,
        constrain_to_sensor_pixels: false,
        output_profile: None,
        ai_upscale: false,
        watermark: None,
        post_process: steps,
        label_tags: false,
    }
}

#[test]
#[allow(clippy::too_many_lines)] // one walk through the whole system
fn the_plugin_system_end_to_end() {
    let _g = super::tests::serial();
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let (work, user) = (root.join("work"), root.join("support/Plugins"));
    fs::create_dir_all(&work).unwrap();

    // --- Author side: the CLI on the reference plug-ins.
    let v = cli(&["validate", &p(&repo().join("plugins/film-border.dsplugin"))]);
    assert!(
        v.contains("Film Border 1.1.0") && v.contains("bytes changed"),
        "{v}"
    );
    for f in ["archive", "amazon-photos", "immich", "google-photos"] {
        let v = cli(&[
            "validate",
            &p(&repo().join(format!("plugins/builtin/darkslide-{f}.dsplugin"))),
        ]);
        assert!(
            v.contains(&format!("switches the app's own {f} feature")),
            "{v}"
        );
    }
    let zip = work.join("Film Border.dsplugin");
    cli(&[
        "pack",
        &p(&repo().join("plugins/film-border.dsplugin")),
        "--zip",
        &p(&zip),
    ]);
    let packed = work.join("packed");
    cli(&[
        "pack",
        &p(&repo().join("plugins/film-border.dsplugin")),
        "--out",
        &p(&packed),
    ]);
    let folder = packed.join("film-border.dsplugin");
    assert!(folder.join("border.wasm").is_file() && !folder.join("border.wat").exists());
    // A key (in the temporary folder, never the repository), and a signed plug-in.
    let key = work.join("tester.key.json");
    cli(&["keygen", "Tester", "--out", &p(&key)]);
    let signed = border_copy(
        &work.join("signed.dsplugin"),
        "com.example.signed-border",
        "1.0.0",
    );
    assert!(cli(&["sign", &p(&signed), "--key", &p(&key)]).contains("signed"));
    let looks = cli(&["new", "Trip Looks", "--kind", "looks", "--dir", &p(&work)]);
    assert!(looks.contains("created"), "{looks}");
    let looks = work.join("Trip Looks.dsplugin");
    cli(&["validate", &p(&looks)]);

    // --- The app's host, with the bundled plug-ins of this build.
    let h = PluginHost::open(Some(p(&repo().join("plugins/builtin"))), p(&user));
    assert_eq!(record(&h, "darkslide.looks").state, PluginState::Active);
    let bundled = h.data_packs(PluginDataKind::Looks);
    assert!(bundled.errors.is_empty(), "{:?}", bundled.errors);
    assert!(
        bundled
            .packs
            .iter()
            .any(|p| p.plugin_id == "darkslide.looks")
    );
    assert_eq!(h.native_features().len(), 0, "integrations start off");

    // A zip, unsigned code: installed, then waits for its own Allow.
    assert!(matches!(
        install(&h, &zip, false).unwrap(),
        PluginInstallResult::Installed { .. }
    ));
    let id = "com.example.film-border";
    assert_eq!(
        record(&h, id).approvals,
        [PluginApproval::AllowUnsignedCode]
    );
    h.allow_unsigned(id.into()).unwrap();
    assert_eq!(record(&h, id).state, PluginState::Active);
    // The same files again: nothing to do. The packed folder of the same version is the same plug-in.
    assert!(matches!(
        install(&h, &folder, false).unwrap(),
        PluginInstallResult::AlreadyInstalled { .. }
    ));

    // Film Border changes a real export: the canvas grows and the margin is red.
    let out = work.join("Out");
    let src = jpeg(&work);
    let step = ExportPostProcessStep {
        plugin_id: id.into(),
        contribution_id: "border".into(),
        params_json: r##"{"width": 0.125, "color": "#FF0000", "outside": true}"##.into(),
        plugin_version: Some("1.1.0".into()),
    };
    let source = ExportSource {
        id: 1,
        path: p(&src),
        seq: 1,
        gps: None,
        artist: None,
        copyright: None,
        adjustments_json: None,
        keywords: Vec::new(),
        metadata: None,
        rating: None,
        label: None,
        pick: None,
    };
    let rep = block_on(export_batch(
        vec![source.clone()],
        vec![
            recipe("border", &out, vec![step.clone()]),
            recipe("plain", &out, vec![]),
        ],
        None,
        false,
        None,
        CancelToken::new(),
        None,
        None,
    ))
    .unwrap();
    assert!(rep.failed.is_empty(), "{:?}", rep.failed);
    let (bw, bh, b) = png16(&out.join("Frame_border.png"));
    let (pw, ph, plain) = png16(&out.join("Frame_plain.png"));
    assert_eq!((pw, ph), (64, 32));
    assert!(bw > pw && bh > ph, "the border grew the canvas: {bw}×{bh}");
    assert_eq!(&b[..3], &[65535, 0, 0], "a red margin");
    let mid = (((bh / 2) * bw + bw / 2) * 3) as usize;
    let pmid = (((ph / 2) * pw + pw / 2) * 3) as usize;
    assert_eq!(
        &b[mid..mid + 3],
        &plain[pmid..pmid + 3],
        "the photo itself is untouched"
    );

    // Signed by a key the user hasn't trusted: waits for Trust Signer, by fingerprint.
    install(&h, &signed, false).unwrap();
    let r = record(&h, "com.example.signed-border");
    let Some(PluginApproval::TrustSigner { key: k, .. }) = r.approvals.first().cloned() else {
        panic!("{r:?}");
    };
    h.trust_signer(k.clone(), "Tester".into()).unwrap();
    assert_eq!(
        record(&h, "com.example.signed-border").state,
        PluginState::Active
    );
    // Pinned per id: the same id unsigned (or with another key) is a different plug-in.
    let unsigned_twin = border_copy(
        &work.join("twin.dsplugin"),
        "com.example.signed-border",
        "2.0.0",
    );
    let e = install(&h, &unsigned_twin, false).unwrap_err().to_string();
    assert!(e.contains("different key"), "{e}");

    // Update and downgrade: a newer version replaces it (an unsigned update asks again);
    // an older one needs consent; the same version with other files is refused.
    let newer = border_copy(&work.join("newer.dsplugin"), id, "1.2.0");
    assert!(matches!(
        install(&h, &newer, false).unwrap(),
        PluginInstallResult::Updated { .. }
    ));
    assert_eq!(
        record(&h, id).approvals,
        [PluginApproval::AllowUnsignedCode]
    );
    h.allow_unsigned(id.into()).unwrap();
    let e = install(&h, &zip, false).unwrap_err().to_string();
    assert!(e.contains("older"), "{e}");
    assert!(h.preview(p(&zip)).unwrap().downgrade);
    assert!(matches!(
        install(&h, &zip, true).unwrap(),
        PluginInstallResult::Updated { .. }
    ));
    let changed = border_copy(&work.join("changed.dsplugin"), id, "1.1.0");
    fs::write(
        changed.join("border.wat"),
        ";; changed\n".to_owned() + &fs::read_to_string(changed.join("border.wat")).unwrap(),
    )
    .unwrap();
    let e = install(&h, &changed, false).unwrap_err().to_string();
    assert!(e.contains("new version number"), "{e}");

    // A development folder: needs "Allow unsigned code plug-ins", loads in place as DEV
    // and wins over the installed copy.
    let dev = border_copy(&work.join("dev.dsplugin"), id, "1.1.0");
    assert!(h.add_dev_folder(p(&dev)).is_err());
    h.set_allow_unsigned_code(true).unwrap();
    h.add_dev_folder(p(&dev)).unwrap();
    let r = record(&h, id);
    assert!(r.dev && r.state == PluginState::Active, "{r:?}");
    h.remove_dev_folder(p(&dev.canonicalize().unwrap()))
        .unwrap();
    assert!(!record(&h, id).dev);

    // The looks pack from `ds-plugin new`: a data pack, installed after one confirmation.
    install(&h, &looks, false).unwrap();
    let all = h.data_packs(PluginDataKind::Looks);
    assert!(
        all.packs
            .iter()
            .any(|p| p.plugin_id == "com.example.trip-looks")
    );
    assert!(all.packs.iter().any(|p| p.plugin_id == "darkslide.looks"));

    // A tampered bundled plug-in doesn't load, and its feature stays off.
    let bundle = root.join("Darkslide.app/Contents/Resources/Plugins");
    fs::create_dir_all(&bundle).unwrap();
    for f in ["darkslide-immich.dsplugin", "darkslide-looks.dsplugin"] {
        let to = bundle.join(f);
        fs::create_dir_all(&to).unwrap();
        for e in fs::read_dir(repo().join("plugins/builtin").join(f))
            .unwrap()
            .flatten()
        {
            fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
    let manifest = bundle.join("darkslide-immich.dsplugin/plugin.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    fs::write(
        &manifest,
        text.replace("name = \"Immich\"", "name = \"Immich+\""),
    )
    .unwrap();
    let tampered = PluginHost::open(Some(p(&bundle)), p(&root.join("other/Plugins")));
    let r = record(&tampered, "darkslide.immich");
    assert_eq!(r.state, PluginState::Error);
    assert!(r.message.unwrap().contains("don't match"));
    assert!(
        tampered
            .set_enabled("darkslide.immich".into(), true)
            .is_err()
    );
    assert!(!tampered.feature_enabled("immich".into()));
    assert_eq!(
        record(&tampered, "darkslide.looks").state,
        PluginState::Active
    );
}
