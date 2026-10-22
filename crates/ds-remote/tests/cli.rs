//! The `ds-remote` CLI against a loopback server: pair, list, pull, resume
//! after a kill, push, push edited sidecars, grant and revoke, and `serve`.

#![allow(clippy::too_many_lines)] // one scenario, start to finish

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use ds_core::FileHash;
use ds_remote::server::Media;
use ds_remote::tls::Pin;
use ds_remote::transfer::Incoming;
use ds_remote::wire::Secret;
use ds_remote::{Client, Identity, Server, ServerConfig, SessionRoot, cli};
use ds_session::{ImageMetaLite, Session};

struct FakeMedia;

impl Media for FakeMedia {
    fn preview(&self, original: &Path) -> io::Result<Vec<u8>> {
        let mut b = fs::read(original)?;
        b.reverse();
        Ok(b)
    }
    fn meta(&self, _original: &Path) -> ImageMetaLite {
        ImageMetaLite::default()
    }
}

fn data(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(13).wrapping_add(seed))
        .collect()
}

fn run(args: &[&str]) -> Result<String, String> {
    let mut out = Vec::new();
    let stop = AtomicBool::new(false);
    cli::run(
        args.iter().map(|s| (*s).to_owned()).collect(),
        &mut out,
        &stop,
    )
    .map(|()| String::from_utf8(out).unwrap())
    .map_err(|e| e.to_string())
}

struct Setup {
    t: tempfile::TempDir,
    session: PathBuf,
    server: Server,
}

impl Setup {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let session = t.path().join("Trip");
        let mut s = Session::create(&session, "Trip").unwrap();
        for (name, d) in [("A.ARW", data(1, 3000)), ("B.ARW", data(2, 5000))] {
            let rel = format!("Capture/{name}");
            fs::write(session.join(&rel), &d).unwrap();
            s.add_original(
                &rel,
                FileHash(*blake3::hash(&d).as_bytes()),
                ImageMetaLite::default(),
            )
            .unwrap();
        }
        let mut cfg = ServerConfig::new(
            "Test Mac",
            &t.path().join("mac"),
            "127.0.0.1:0".parse().unwrap(),
        );
        cfg.media = Arc::new(FakeMedia);
        let server = Server::start(cfg).unwrap();
        server.set_sessions(vec![SessionRoot {
            path: session.clone(),
            name: "Trip".into(),
        }]);
        Self { t, session, server }
    }

    fn path(&self, p: &str) -> String {
        self.t.path().join(p).to_string_lossy().into_owned()
    }

    fn addr(&self) -> String {
        self.server.addr().to_string()
    }
}

#[test]
fn the_cli_pairs_pulls_resumes_pushes_and_merges() {
    let m = Setup::new();
    let ipad = m.path("ipad");
    let mac = m.path("mac");
    let out = m.path("out");
    let st = |extra: &[&str]| {
        let mut v = vec!["--state", ipad.as_str(), "--name", "CLI iPad"];
        v.extend_from_slice(extra);
        run(&v)
    };

    let fp = st(&["id"]).unwrap();
    assert_eq!(fp.trim().len(), 64);
    let e = st(&["ls"]).unwrap_err();
    assert!(e.contains("not paired"), "{e}");

    // Pair by typed code.
    let ticket = m.server.begin_pairing();
    let r = st(&["pair", &m.addr(), &ticket.code]).unwrap();
    assert!(r.starts_with("paired with Test Mac"), "{r}");
    let r = st(&["ls"]).unwrap();
    assert!(r.contains("  Trip"), "{r}");
    let r = st(&["ls", "Trip"]).unwrap();
    assert!(r.contains("Capture/A.ARW  3000 bytes  variants 1"), "{r}");

    // Pull everything.
    let r = st(&["pull", "Trip", &out, "--originals"]).unwrap();
    assert!(
        r.contains("fetched 4, already here 0, local edits kept 0"),
        "{r}"
    );
    let outp = PathBuf::from(&out);
    assert_eq!(fs::read(outp.join("Capture/A.ARW")).unwrap(), data(1, 3000));
    assert!(outp.join("Previews/Capture/B.ARW.jpg").exists());
    let sc = outp.join("Sidecars/Capture/A.ARW.1.json");
    assert_eq!(
        fs::read(&sc).unwrap(),
        fs::read(outp.join("Sidecars/.base/Capture/A.ARW.1.json")).unwrap()
    );

    // A pull killed half-way through B leaves a partial; the next run resumes it.
    let b = outp.join("Capture/B.ARW");
    fs::remove_file(&b).unwrap();
    let full = data(2, 5000);
    let inc = Incoming::for_dest(&b, 5000, FileHash(*blake3::hash(&full).as_bytes()));
    inc.write(0, &full[..2000]).unwrap();
    let r = st(&[
        "pull",
        "Trip",
        &out,
        "--originals",
        "--no-previews",
        "--no-sidecars",
    ])
    .unwrap();
    assert!(r.contains("fetched 1, already here 1"), "{r}");
    assert_eq!(fs::read(&b).unwrap(), full);

    // Read-only until the Mac grants write access.
    let new = m.t.path().join("NEW.ARW");
    fs::write(&new, data(9, 777)).unwrap();
    let e = st(&["push", "Trip", &new.to_string_lossy()]).unwrap_err();
    assert!(e.contains("read-only"), "{e}");
    let r = run(&["--state", &mac, "devices"]).unwrap();
    assert!(r.contains("read-only  CLI iPad"), "{r}");
    let prefix = &fp.trim()[..8];
    assert_eq!(
        run(&["--state", &mac, "grant", prefix]).unwrap().trim(),
        "done"
    );
    let r = st(&["push", "Trip", &new.to_string_lossy()]).unwrap();
    assert!(r.contains("sent as Capture/From CLI iPad/NEW.ARW"), "{r}");
    let r = st(&["push", "Trip", &new.to_string_lossy()]).unwrap();
    assert!(r.contains("already there"), "{r}");
    assert_eq!(
        fs::read(m.session.join("Capture/From CLI iPad/NEW.ARW")).unwrap(),
        data(9, 777)
    );

    // Edit a pulled sidecar and push it: a fast-forward.
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&sc).unwrap()).unwrap();
    v["rating"] = 4.into();
    fs::write(&sc, serde_json::to_vec(&v).unwrap()).unwrap();
    let r = st(&["push-sidecars", "Trip", &out]).unwrap();
    assert!(r.contains("Capture/A.ARW #1: FastForward"), "{r}");
    assert!(r.contains("pushed 1"), "{r}");
    let r = st(&["push-sidecars", "Trip", &out]).unwrap();
    assert!(r.contains("pushed 0"), "nothing left to push: {r}");
    let s = Session::open(&m.session).unwrap();
    let a = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == "Capture/A.ARW")
        .unwrap();
    assert_eq!(
        s.variant(s.variants(a.id).unwrap()[0])
            .unwrap()
            .rating
            .stars(),
        4
    );
    drop(s);

    // A pull never overwrites local edits not pushed yet.
    v["rating"] = 1.into();
    fs::write(&sc, serde_json::to_vec(&v).unwrap()).unwrap();
    let mut s = Session::open(&m.session).unwrap();
    let va = s.variants(a.id).unwrap()[0];
    s.set_rating(&[va], ds_core::Rating::new(2).unwrap())
        .unwrap();
    drop(s);
    let r = st(&["pull", "Trip", &out, "--no-previews"]).unwrap();
    assert!(r.contains("Capture/A.ARW #1: has local edits"), "{r}");
    let kept: serde_json::Value = serde_json::from_slice(&fs::read(&sc).unwrap()).unwrap();
    assert_eq!(kept["rating"], 1);

    // Revoke from the Mac: refused at once.
    assert_eq!(
        run(&["--state", &mac, "readonly", prefix]).unwrap().trim(),
        "done"
    );
    assert_eq!(
        run(&["--state", &mac, "revoke", prefix]).unwrap().trim(),
        "done"
    );
    assert!(
        run(&["--state", &mac, "revoke", prefix])
            .unwrap_err()
            .contains("no paired device")
    );
    assert!(st(&["ls"]).is_err());
}

#[test]
fn the_cli_pairs_by_qr_payload_and_picks_a_server() {
    let m = Setup::new();
    let ipad = m.path("ipad2");
    let t = m.server.begin_pairing();
    let payload = t.qr_payload("127.0.0.1", m.server.addr().port(), &m.server.fingerprint());
    let r = run(&["--state", &ipad, "pair", &payload]).unwrap();
    assert!(r.contains("paired with Test Mac"), "{r}");
    let fp = m.server.fingerprint().to_hex();
    let r = run(&[
        "--state",
        &ipad,
        "ls",
        "--server",
        &fp[..6],
        "--addr",
        &m.addr(),
    ])
    .unwrap();
    assert!(r.contains("Trip"));
    let e = run(&["--state", &ipad, "ls", "--server", "ffffffff"]).unwrap_err();
    assert!(e.contains("not paired with that Mac"), "{e}");
    let e = run(&["--state", &ipad, "ls", "Nope"]).unwrap_err();
    assert!(e.contains("no session Nope"), "{e}");
}

#[test]
fn usage_errors_are_explained() {
    let t = tempfile::tempdir().unwrap();
    let s = t.path().to_string_lossy().into_owned();
    for (args, why) in [
        (vec!["--state", &s], "missing command"),
        (vec!["--state", &s, "frobnicate"], "unknown command"),
        (vec!["--state"], "--state needs a value"),
        (vec!["--state", &s, "pair"], "missing QR payload"),
        (
            vec!["--state", &s, "pair", "127.0.0.1:1"],
            "missing pairing code",
        ),
        (vec!["--state", &s, "serve"], "at least one --session"),
        (
            vec!["--state", &s, "serve", "--session", "x", "--bind", "nope"],
            "bad --bind",
        ),
        (vec!["--state", &s, "grant", "ab"], "no paired device"),
        (vec!["--state", &s, "grant"], "missing device fingerprint"),
    ] {
        let e = run(&args).unwrap_err();
        assert!(e.contains(why), "{args:?}: {e}");
        assert!(e.contains("usage: ds-remote"), "{e}");
    }
    assert_eq!(run(&["--state", &s, "devices"]).unwrap(), "");
}

/// Output shared with the test while `serve` runs on another thread.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn serve_runs_until_stopped_and_pairs() {
    let t = tempfile::tempdir().unwrap();
    let session = t.path().join("Shoot");
    Session::create(&session, "Shoot").unwrap();
    let state = t.path().join("mac").to_string_lossy().into_owned();
    let out = Shared::default();
    let stop = Arc::new(AtomicBool::new(false));
    let (o, s2) = (out.clone(), stop.clone());
    let args: Vec<String> = [
        "--state",
        &state,
        "--name",
        "CLI Mac",
        "serve",
        "--session",
        &session.to_string_lossy(),
        "--bind",
        "127.0.0.1:0",
        "--pair",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let h = std::thread::spawn(move || {
        let mut o = o;
        cli::run(args, &mut o, &s2).map_err(|e| e.to_string())
    });
    let text = loop {
        let s = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        if s.contains("\nqr ") {
            break s;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let addr = text
        .lines()
        .find_map(|l| l.strip_prefix("listening on "))
        .unwrap()
        .to_owned();
    let code = text
        .lines()
        .find_map(|l| l.strip_prefix("pairing code "))
        .unwrap()
        .to_owned();
    assert!(text.contains("fingerprint "));
    let id = Identity::generate().unwrap();
    let mut c = Client::connect(addr.as_str(), &id, Pin::AnyForPairing, "x").unwrap();
    c.pair(&code, Secret::Code).unwrap();
    assert_eq!(c.server_name, "CLI Mac");
    let sessions = c.sessions().unwrap();
    assert_eq!(sessions[0].name, "Shoot");
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    h.join().unwrap().unwrap();
    assert!(c.sessions().is_err(), "stopped");
}
