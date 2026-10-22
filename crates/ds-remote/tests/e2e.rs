//! Loopback end-to-end tests: a real server and client over TLS on an
//! ephemeral port, with temp sessions on both sides.

#![allow(clippy::too_many_lines, clippy::float_cmp)] // long scenario tests; exposures are exact

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use ds_core::{FileHash, Rating};
use ds_remote::client::MAX_RETRIES;
use ds_remote::net::allowed_ip;
use ds_remote::server::{Media, ReadFault};
use ds_remote::tls::Pin;
use ds_remote::transfer::Incoming;
use ds_remote::wire::{CHUNK, Secret};
use ds_remote::{
    Client, ErrorCode, FileRef, Identity, MergeOutcome, PullOutcome, RemoteError, Server,
    ServerConfig, SessionRoot,
};
use ds_session::{ImageMetaLite, Session, Sidecar};
use tempfile::TempDir;

/// Previews without RAW decoding: the first 64 KiB of the original, reversed.
struct FakeMedia;

impl Media for FakeMedia {
    fn preview(&self, original: &Path) -> io::Result<Vec<u8>> {
        let mut b = fs::read(original)?;
        b.truncate(64 << 10);
        b.reverse();
        Ok(b)
    }
    fn meta(&self, _original: &Path) -> ImageMetaLite {
        ImageMetaLite {
            width: 6000,
            height: 4000,
            ..ImageMetaLite::default()
        }
    }
}

fn bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn hash(b: &[u8]) -> FileHash {
    FileHash(*blake3::hash(b).as_bytes())
}

struct Mac {
    _t: TempDir,
    state: PathBuf,
    session: PathBuf,
    server: Server,
    id: String,
}

fn mac_with(
    hook: Option<ds_remote::server::ReadHook>,
    policy: Option<ds_remote::net::PeerPolicy>,
) -> Mac {
    mac_cfg(|cfg| {
        cfg.read_hook = hook;
        if let Some(p) = policy {
            cfg.policy = p;
        }
    })
}

/// A session with two photos (one big enough for three chunks) served on loopback.
fn mac_cfg(tweak: impl FnOnce(&mut ServerConfig)) -> Mac {
    let t = tempfile::tempdir().unwrap();
    let session = t.path().join("Trip");
    let mut s = Session::create(&session, "Trip").unwrap();
    for (name, data) in [
        ("A.ARW", bytes(1, 2 * CHUNK + 12_345)),
        ("B.ARW", bytes(2, 1000)),
    ] {
        let rel = format!("Capture/{name}");
        fs::write(session.join(&rel), &data).unwrap();
        s.add_original(&rel, hash(&data), ImageMetaLite::default())
            .unwrap();
    }
    drop(s);
    let state = t.path().join("mac-state");
    let mut cfg = ServerConfig::new("Test Mac", &state, "127.0.0.1:0".parse().unwrap());
    cfg.media = Arc::new(FakeMedia);
    tweak(&mut cfg);
    let server = Server::start(cfg).unwrap();
    let root = SessionRoot {
        path: session.clone(),
        name: "Trip".into(),
    };
    let id = root.id();
    server.set_sessions(vec![root]);
    Mac {
        _t: t,
        state,
        session,
        server,
        id,
    }
}

fn mac() -> Mac {
    mac_with(None, None)
}

fn addr(m: &Mac) -> SocketAddr {
    m.server.addr()
}

/// Pair a fresh device by code; returns its identity and a connected client.
fn paired(m: &Mac, name: &str) -> (Identity, Client) {
    let id = Identity::generate().unwrap();
    let t = m.server.begin_pairing();
    let mut c = Client::connect(addr(m), &id, Pin::AnyForPairing, name).unwrap();
    assert!(!c.paired);
    c.pair(&t.code, Secret::Code).unwrap();
    (id, c)
}

fn reconnect(m: &Mac, id: &Identity) -> ds_remote::Result<Client> {
    Client::connect(addr(m), id, Pin::Key(m.server.fingerprint()), "iPad")
}

fn grant(m: &Mac, id: &Identity) {
    assert!(
        m.server
            .devices()
            .set_write(&id.fingerprint().to_hex(), true)
            .unwrap()
    );
}

#[test]
fn pairing_by_code_pins_both_keys_and_starts_read_only() {
    let m = mac();
    let (id, c) = paired(&m, "Test iPad");
    assert!(c.paired && !c.write);
    assert_eq!(c.server_name, "Test Mac");
    assert_eq!(c.server_fingerprint(), m.server.fingerprint());
    let devs = m.server.devices().list().unwrap();
    assert_eq!(devs.len(), 1);
    assert_eq!(devs[0].fingerprint, id.fingerprint().to_hex());
    assert_eq!(devs[0].name, "Test iPad");
    assert!(!devs[0].write, "read-only until granted");
    assert!(!m.server.pairing_open(), "a code works once");
    // The paired key reconnects without a window; a wrong pin is refused.
    let c = reconnect(&m, &id).unwrap();
    assert!(c.paired);
    let wrong = Client::connect(addr(&m), &id, Pin::Key(id.fingerprint()), "iPad");
    assert!(
        matches!(wrong, Err(RemoteError::Io(_) | RemoteError::Tls(_))),
        "{wrong:?}"
    );
}

#[test]
fn pairing_by_qr_token_and_wrong_codes() {
    let m = mac();
    let t = m.server.begin_pairing();
    let payload = t.qr_payload("localhost", addr(&m).port(), &m.server.fingerprint());
    let q = ds_remote::pairing::QrPayload::parse(&payload).unwrap();
    let id = Identity::generate().unwrap();
    // Wrong code: refused, and the window counts the attempt.
    let mut c = Client::connect(addr(&m), &id, Pin::Key(q.fingerprint), "QR iPad").unwrap();
    let e = c.pair("000000", Secret::Code).unwrap_err();
    let e2 = c.pair("000001", Secret::Code);
    assert!(
        e.code() == Some(ErrorCode::PairingFailed) || matches!(e, RemoteError::Io(_)),
        "{e:?}"
    );
    drop((c, e2));
    let mut c = Client::connect(addr(&m), &id, Pin::Key(q.fingerprint), "QR iPad").unwrap();
    c.pair(&q.token, Secret::Token).unwrap();
    assert_eq!(m.server.devices().list().unwrap()[0].name, "QR iPad");

    // Three wrong confirmations close the window.
    let _t = m.server.begin_pairing();
    for _ in 0..3 {
        let other = Identity::generate().unwrap();
        let mut c = Client::connect(addr(&m), &other, Pin::AnyForPairing, "x").unwrap();
        assert!(c.pair("999999x", Secret::Code).is_err());
    }
    assert!(!m.server.pairing_open());
    // Pairing a second time on the same connection without a window.
    let _t = m.server.begin_pairing();
    let other = Identity::generate().unwrap();
    let mut c = Client::connect(addr(&m), &other, Pin::AnyForPairing, "x").unwrap();
    m.server.cancel_pairing();
    let e = c.pair("123456", Secret::Code).unwrap_err();
    // Refused, or already closed with the window.
    assert!(
        e.code() == Some(ErrorCode::PairingClosed) || matches!(e, RemoteError::Io(_)),
        "{e:?}"
    );
}

#[test]
fn an_unpaired_key_is_refused() {
    let m = mac();
    let stranger = Identity::generate().unwrap();
    // No pairing window: the TLS handshake fails before any request.
    let r = Client::connect(addr(&m), &stranger, Pin::Key(m.server.fingerprint()), "x");
    assert!(r.is_err(), "unpaired key must be refused");
    // A window is open: it may connect, but only to pair.
    let _t = m.server.begin_pairing();
    let mut c =
        Client::connect(addr(&m), &stranger, Pin::Key(m.server.fingerprint()), "x").unwrap();
    assert!(!c.paired);
    let e = c.sessions().unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::Unpaired));
}

#[test]
fn a_non_private_source_address_is_dropped() {
    // The seam: this policy sees every loopback peer as a public address.
    let policy: ds_remote::net::PeerPolicy = Arc::new(|peer: SocketAddr| {
        let seen = if peer.ip().is_loopback() {
            "203.0.113.9".parse().unwrap()
        } else {
            peer.ip()
        };
        allowed_ip(seen)
    });
    let m = mac_with(None, Some(policy));
    let _t = m.server.begin_pairing();
    let id = Identity::generate().unwrap();
    assert!(Client::connect(addr(&m), &id, Pin::AnyForPairing, "x").is_err());
    assert_eq!(m.server.status().refused_peers, 1);
    assert_eq!(
        m.server.devices().list().unwrap(),
        [] as [ds_remote::Device; 0]
    );
}

#[test]
fn list_sessions_photos_and_pull_everything() {
    let m = mac();
    let (_id, mut c) = paired(&m, "iPad");
    let sessions = c.sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        (sessions[0].id.as_str(), sessions[0].name.as_str()),
        (m.id.as_str(), "Trip")
    );
    let photos = c.photos(&m.id).unwrap();
    assert_eq!(photos.len(), 2);
    let a = photos.iter().find(|p| p.rel == "Capture/A.ARW").unwrap();
    assert_eq!(a.size, (2 * CHUNK + 12_345) as u64);
    assert_eq!(a.variants.len(), 1);
    assert_eq!(a.variants[0].ordinal, 1);
    assert!(a.hash.is_some());

    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("A.ARW");
    let orig = FileRef::Original { rel: a.rel.clone() };
    let r = c.pull(&m.id, &orig, &dest).unwrap();
    assert_eq!(
        r,
        PullOutcome::Fetched {
            bytes: a.size,
            resumed_from: 0,
            retries: 0
        }
    );
    assert_eq!(
        fs::read(&dest).unwrap(),
        fs::read(m.session.join(&a.rel)).unwrap()
    );
    assert_eq!(
        c.pull(&m.id, &orig, &dest).unwrap(),
        PullOutcome::AlreadyHere
    );
    // A different file at the destination is never replaced.
    let b = FileRef::Original {
        rel: "Capture/B.ARW".into(),
    };
    assert!(matches!(
        c.pull(&m.id, &b, &dest),
        Err(RemoteError::Exists(_))
    ));
    assert_eq!(
        fs::read(&dest).unwrap(),
        fs::read(m.session.join(&a.rel)).unwrap()
    );

    let pv = out.path().join("A.jpg");
    c.pull(&m.id, &FileRef::Preview { rel: a.rel.clone() }, &pv)
        .unwrap();
    assert_eq!(
        fs::read(&pv).unwrap(),
        FakeMedia.preview(&m.session.join(&a.rel)).unwrap()
    );
    let sc = c
        .fetch(
            &m.id,
            &FileRef::Sidecar {
                rel: a.rel.clone(),
                ordinal: 1,
            },
        )
        .unwrap();
    assert_eq!(
        blake3::hash(&sc).to_hex().as_str(),
        a.variants[0].sidecar_hash
    );
    assert!(m.server.status().bytes_sent >= a.size);
    assert!(m.server.status().transferring);

    // Bad requests: unknown session, file not in the session, bad paths, stale hash.
    assert_eq!(
        c.photos("nope").unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );
    fs::write(m.session.join("Capture/stray.txt"), b"x").unwrap();
    let stray = FileRef::Original {
        rel: "Capture/stray.txt".into(),
    };
    assert_eq!(
        c.stat(&m.id, &stray).unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );
    let up = FileRef::Original {
        rel: "Capture/../../etc/passwd".into(),
    };
    assert_eq!(
        c.stat(&m.id, &up).unwrap_err().code(),
        Some(ErrorCode::BadRequest)
    );
    let hidden = FileRef::Sidecar {
        rel: ".ds/x".into(),
        ordinal: 1,
    };
    assert_eq!(
        c.stat(&m.id, &hidden).unwrap_err().code(),
        Some(ErrorCode::BadRequest)
    );
    let missing = FileRef::Sidecar {
        rel: a.rel.clone(),
        ordinal: 9,
    };
    assert_eq!(
        c.stat(&m.id, &missing).unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );
}

#[test]
fn a_dropped_connection_resumes_from_the_partial() {
    let dropped = Arc::new(AtomicBool::new(false));
    let d = dropped.clone();
    let hook: ds_remote::server::ReadHook = Arc::new(move |f: &FileRef, off: u64| {
        if matches!(f, FileRef::Original { .. })
            && off == CHUNK as u64
            && !d.swap(true, Ordering::SeqCst)
        {
            ReadFault::Drop
        } else {
            ReadFault::None
        }
    });
    let m = mac_with(Some(hook), None);
    let (id, mut c) = paired(&m, "iPad");
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("A.ARW");
    let file = FileRef::Original {
        rel: "Capture/A.ARW".into(),
    };
    assert!(c.pull(&m.id, &file, &dest).is_err(), "the link dropped");
    assert!(!dest.exists());
    let size = fs::metadata(m.session.join("Capture/A.ARW")).unwrap().len();
    let inc = Incoming::for_dest(
        &dest,
        size,
        hash(&fs::read(m.session.join("Capture/A.ARW")).unwrap()),
    );
    assert_eq!(inc.have().unwrap(), CHUNK as u64, "first chunk kept");
    let mut c = reconnect(&m, &id).unwrap();
    let r = c.pull(&m.id, &file, &dest).unwrap();
    assert_eq!(
        r,
        PullOutcome::Fetched {
            bytes: size - CHUNK as u64,
            resumed_from: CHUNK as u64,
            retries: 0
        }
    );
    assert_eq!(
        fs::read(&dest).unwrap(),
        fs::read(m.session.join("Capture/A.ARW")).unwrap()
    );
    assert!(!inc.partial.exists());
}

#[test]
fn a_corrupted_chunk_fails_verification_and_is_retried() {
    let corrupt = Arc::new(AtomicU32::new(1));
    let n = corrupt.clone();
    let hook: ds_remote::server::ReadHook = Arc::new(move |_f: &FileRef, off: u64| {
        if off == 0 && n.load(Ordering::SeqCst) > 0 {
            n.fetch_sub(1, Ordering::SeqCst);
            ReadFault::Corrupt
        } else {
            ReadFault::None
        }
    });
    let m = mac_with(Some(hook), None);
    let (_id, mut c) = paired(&m, "iPad");
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("B.ARW");
    let file = FileRef::Original {
        rel: "Capture/B.ARW".into(),
    };
    let r = c.pull(&m.id, &file, &dest).unwrap();
    assert!(
        matches!(r, PullOutcome::Fetched { retries: 1, .. }),
        "{r:?}"
    );
    assert_eq!(
        fs::read(&dest).unwrap(),
        fs::read(m.session.join("Capture/B.ARW")).unwrap()
    );
    // Always corrupt: gives up after the retries, leaving nothing behind.
    corrupt.store(u32::MAX, Ordering::SeqCst);
    let dest2 = out.path().join("B2.ARW");
    let e = c.pull(&m.id, &file, &dest2).unwrap_err();
    assert!(
        e.to_string().contains(&format!("{MAX_RETRIES} retries")),
        "{e}"
    );
    assert!(!dest2.exists());
    assert_eq!(
        fs::read_dir(out.path()).unwrap().count(),
        1,
        "no partial left"
    );
    // Small files in memory are checked too.
    let sc = FileRef::Sidecar {
        rel: "Capture/B.ARW".into(),
        ordinal: 1,
    };
    assert!(
        c.fetch(&m.id, &sc)
            .unwrap_err()
            .to_string()
            .contains("damaged")
    );
}

#[test]
fn a_file_changing_mid_pull_restarts_with_the_new_content() {
    let path: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let p = path.clone();
    let hook: ds_remote::server::ReadHook = Arc::new(move |_f: &FileRef, off: u64| {
        if off == 0
            && let Some(p) = p.lock().unwrap().take()
        {
            fs::write(&p, bytes(9, 3 * CHUNK)).unwrap();
        }
        ReadFault::None
    });
    let m = mac_with(Some(hook), None);
    *path.lock().unwrap() = Some(m.session.join("Capture/A.ARW"));
    let (_id, mut c) = paired(&m, "iPad");
    let out = tempfile::tempdir().unwrap();
    let dest = out.path().join("A.ARW");
    let r = c
        .pull(
            &m.id,
            &FileRef::Original {
                rel: "Capture/A.ARW".into(),
            },
            &dest,
        )
        .unwrap();
    assert!(
        matches!(r, PullOutcome::Fetched { retries: 1, .. }),
        "{r:?}"
    );
    assert_eq!(fs::read(&dest).unwrap(), bytes(9, 3 * CHUNK));
}

#[test]
fn pushes_need_write_access_land_in_the_session_and_resume() {
    let m = mac();
    let (id, mut c) = paired(&m, "Test iPad");
    let src_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path().join("NEW.ARW");
    let data = bytes(7, CHUNK + 777);
    fs::write(&src, &data).unwrap();
    let e = c.push(&m.id, &src).unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::ReadOnly));

    grant(&m, &id);
    let mut c = reconnect(&m, &id).unwrap();
    assert!(c.write);
    // A push killed after its first chunk left a partial on the Mac.
    let dir = m.session.join("Capture/From Test iPad");
    let inc = Incoming::in_dir(&dir, data.len() as u64, hash(&data));
    inc.write(0, &data[..CHUNK]).unwrap();
    let r = c.push(&m.id, &src).unwrap();
    assert_eq!(r.rel, "Capture/From Test iPad/NEW.ARW");
    assert_eq!((r.duplicate, r.resumed_from), (false, CHUNK as u64));
    assert_eq!(fs::read(m.session.join(&r.rel)).unwrap(), data);
    assert!(!inc.partial.exists());
    assert_eq!(m.server.take_changed(), vec![m.session.clone()]);
    assert_eq!(m.server.take_changed(), [] as [std::path::PathBuf; 0]);
    let s = Session::open(&m.session).unwrap();
    let img = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == r.rel)
        .expect("registered in the session");
    assert_eq!(img.hash, hash(&data));
    assert_eq!(img.meta.width, 6000, "metadata from the Media seam");
    drop(s);

    // The same content again (any name) is a duplicate; a new file with the
    // same name is placed beside it, never over it.
    let again = src_dir.path().join("COPY.ARW");
    fs::write(&again, &data).unwrap();
    let r2 = c.push(&m.id, &again).unwrap();
    assert_eq!((r2.rel.as_str(), r2.duplicate), (r.rel.as_str(), true));
    fs::write(&src, bytes(8, 10)).unwrap();
    let r3 = c.push(&m.id, &src).unwrap();
    assert_eq!(r3.rel, "Capture/From Test iPad/NEW-2.ARW");
    assert_eq!(
        fs::read(m.session.join(&r.rel)).unwrap(),
        data,
        "never replaced"
    );
    // A device can't leave more than MAX_PARTIAL_BYTES of unfinished pushes.
    let hog = dir.join(".0000000000000000.partial");
    fs::File::create(&hog)
        .unwrap()
        .set_len(ds_remote::server::MAX_PARTIAL_BYTES)
        .unwrap();
    let small = src_dir.path().join("SMALL.ARW");
    fs::write(&small, bytes(5, 10)).unwrap();
    let e = c.push(&m.id, &small).unwrap_err();
    assert!(e.to_string().contains("unfinished transfers"), "{e}");
    fs::remove_file(&hog).unwrap();
    // Empty files and bad names.
    let empty = src_dir.path().join("EMPTY.JPG");
    fs::write(&empty, b"").unwrap();
    assert!(!c.push(&m.id, &empty).unwrap().duplicate);
    let hidden = src_dir.path().join(".hidden");
    fs::write(&hidden, b"h").unwrap();
    assert_eq!(
        c.push(&m.id, &hidden).unwrap_err().code(),
        Some(ErrorCode::BadRequest)
    );
}

#[test]
fn folders_opened_in_place_take_no_pushed_originals() {
    let m = mac();
    let t = tempfile::tempdir().unwrap();
    let originals = t.path().join("Photos");
    fs::create_dir_all(&originals).unwrap();
    let store = t.path().join("Store");
    Session::create_external(&store, "Folder", &originals).unwrap();
    let root = SessionRoot {
        path: store,
        name: "Folder".into(),
    };
    let ext = root.id();
    m.server.set_sessions(vec![root]);
    let (id, _) = paired(&m, "iPad");
    grant(&m, &id);
    let mut c = reconnect(&m, &id).unwrap();
    let f = t.path().join("x.jpg");
    fs::write(&f, b"x").unwrap();
    assert_eq!(
        c.push(&ext, &f).unwrap_err().code(),
        Some(ErrorCode::ReadOnly)
    );
    assert_eq!(
        fs::read_dir(&originals).unwrap().count(),
        0,
        "nothing written"
    );
    // The old session is no longer offered.
    assert_eq!(
        c.photos(&m.id).unwrap_err().code(),
        Some(ErrorCode::NotFound)
    );
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn sidecar_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap()
}

#[test]
fn sidecar_pushes_fast_forward_merge_and_keep_conflicts_as_variants() {
    let m = mac();
    let (id, _) = paired(&m, "Test iPad");
    grant(&m, &id);
    let mut c = reconnect(&m, &id).unwrap();
    let rel = "Capture/B.ARW";
    let file = FileRef::Sidecar {
        rel: rel.into(),
        ordinal: 1,
    };
    let base = c.fetch(&m.id, &file).unwrap();
    let base_hash = blake3::hash(&base).to_hex().to_string();

    // Read-only devices can't push edits.
    let ro = Identity::generate().unwrap();
    {
        let t = m.server.begin_pairing();
        let mut r = Client::connect(addr(&m), &ro, Pin::AnyForPairing, "RO").unwrap();
        r.pair(&t.code, Secret::Code).unwrap();
        let e = r
            .push_sidecar(&m.id, rel, 1, None, None, sidecar_json(&base), 0)
            .unwrap_err();
        assert_eq!(e.code(), Some(ErrorCode::ReadOnly));
    }

    // 1. Fast-forward: the iPad rates it 3 from the current sidecar.
    let mut edited: Sidecar = Sidecar::from_json(&base).unwrap().sidecar;
    edited.rating = Rating::new(3).unwrap();
    let r = c
        .push_sidecar(
            &m.id,
            rel,
            1,
            Some(base_hash.clone()),
            Some(text(&base)),
            sidecar_json(&edited.to_json()),
            1,
        )
        .unwrap();
    assert_eq!(
        (r.outcome, r.kept_ordinal),
        (MergeOutcome::FastForward, None)
    );
    assert_eq!(r.hash, blake3::hash(&r.sidecar).to_hex().as_str());
    let now = Sidecar::from_json(&r.sidecar).unwrap().sidecar;
    assert_eq!(now.rating, Rating::new(3).unwrap());
    assert_eq!(m.server.take_changed(), vec![m.session.clone()]);
    // Pushing the same thing again changes nothing.
    let same = c
        .push_sidecar(
            &m.id,
            rel,
            1,
            Some(r.hash.clone()),
            None,
            sidecar_json(&r.sidecar),
            1,
        )
        .unwrap();
    assert_eq!(same.outcome, MergeOutcome::Unchanged);
    assert_eq!(m.server.take_changed(), [] as [std::path::PathBuf; 0]);

    // 2. The Mac changes the exposure; the iPad, still on the old base,
    //    changes the colour label and keywords: merged field by field.
    let mut s = Session::open(&m.session).unwrap();
    let img = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == rel)
        .unwrap();
    let v = s.variants(img.id).unwrap()[0];
    let mut mac_adj = s.load_adjustments(v).unwrap();
    mac_adj.exposure.exposure_ev = 0.4;
    s.save_adjustments(v, &mac_adj).unwrap();
    let mut ipad = now.clone();
    ipad.color_tag = ds_core::ColorTag::Green;
    ipad.keywords = vec!["ipad".into()];
    let r = c
        .push_sidecar(
            &m.id,
            rel,
            1,
            Some(r.hash.clone()),
            Some(text(&r.sidecar)),
            sidecar_json(&ipad.to_json()),
            1,
        )
        .unwrap();
    assert_eq!(r.outcome, MergeOutcome::Merged);
    let merged = Sidecar::from_json(&r.sidecar).unwrap().sidecar;
    assert_eq!(
        merged.adjustments.exposure.exposure_ev, 0.4,
        "the Mac's edit stays"
    );
    assert_eq!(
        merged.color_tag,
        ds_core::ColorTag::Green,
        "the iPad's label lands"
    );
    assert_eq!(merged.keywords, ["ipad"]);
    assert_eq!(merged.rating, Rating::new(3).unwrap());

    // 3. Both change the exposure: the newer (iPad, edited "now") wins on
    //    the variant, and the Mac's edit is kept as a new variant.
    mac_adj.exposure.exposure_ev = -1.0;
    s.save_adjustments(v, &mac_adj).unwrap();
    let mut ipad2 = merged.clone();
    ipad2.adjustments.exposure.exposure_ev = 1.5;
    let later = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 60_000;
    let r = c
        .push_sidecar(
            &m.id,
            rel,
            1,
            Some(r.hash.clone()),
            Some(text(&r.sidecar)),
            sidecar_json(&ipad2.to_json()),
            later,
        )
        .unwrap();
    assert_eq!(r.outcome, MergeOutcome::Conflict);
    let kept = r.kept_ordinal.expect("a variant for the other edit");
    assert_eq!(kept, 2);
    s.sync_index().unwrap();
    let vs = s.variants(img.id).unwrap();
    assert_eq!(vs.len(), 2);
    assert_eq!(s.load_adjustments(vs[0]).unwrap().exposure.exposure_ev, 1.5);
    let k = vs
        .iter()
        .find(|x| s.variant(**x).unwrap().ordinal == kept)
        .unwrap();
    assert_eq!(
        s.load_adjustments(*k).unwrap().exposure.exposure_ev,
        -1.0,
        "the Mac's edit is kept"
    );
    assert_eq!(s.variant(*k).unwrap().name, "Edited on this Mac");

    // 4. An older iPad edit conflicting with a newer Mac edit: the Mac keeps
    //    its edit and the iPad's is kept, named after the device.
    mac_adj.exposure.exposure_ev = 0.2;
    s.save_adjustments(vs[0], &mac_adj).unwrap();
    let cur = c.fetch(&m.id, &file).unwrap();
    let mut old = Sidecar::from_json(&cur).unwrap().sidecar;
    old.adjustments.exposure.exposure_ev = 2.0;
    let r = c
        .push_sidecar(&m.id, rel, 1, None, None, sidecar_json(&old.to_json()), 1)
        .unwrap();
    assert_eq!(r.outcome, MergeOutcome::Conflict);
    s.sync_index().unwrap();
    let vs = s.variants(img.id).unwrap();
    assert_eq!(vs.len(), 3);
    assert_eq!(s.load_adjustments(vs[0]).unwrap().exposure.exposure_ev, 0.2);
    let names: Vec<String> = vs.iter().map(|v| s.variant(*v).unwrap().name).collect();
    assert!(
        names.contains(&"Edited on Test iPad".to_owned()),
        "{names:?}"
    );

    // A sidecar for another variant, unknown photos and ordinals, bad JSON.
    let mut other = old.clone();
    other.variant_id += 1000;
    let e = c
        .push_sidecar(&m.id, rel, 1, None, None, sidecar_json(&other.to_json()), 1)
        .unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::BadRequest));
    let e = c
        .push_sidecar(
            &m.id,
            "Capture/NOPE.ARW",
            1,
            None,
            None,
            sidecar_json(&cur),
            1,
        )
        .unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::NotFound));
    let e = c
        .push_sidecar(&m.id, rel, 7, None, None, sidecar_json(&cur), 1)
        .unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::NotFound));
    let e = c
        .push_sidecar(&m.id, rel, 1, None, None, serde_json::json!({"x": 1}), 1)
        .unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::BadRequest));
}

#[test]
fn revoking_a_device_takes_effect_on_its_next_request() {
    let m = mac();
    let (id, mut c) = paired(&m, "iPad");
    assert_eq!(c.sessions().unwrap().len(), 1);
    // Revoked from anywhere (here: another store handle on the same state).
    let store = ds_remote::DeviceStore::new(&m.state);
    assert!(store.revoke(&id.fingerprint().to_hex()).unwrap());
    let e = c.sessions().unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::Unpaired));
    assert!(reconnect(&m, &id).is_err(), "and can't reconnect");
}

#[test]
fn stopping_closes_connections() {
    let m = mac();
    let (_id, mut c) = paired(&m, "iPad");
    assert_eq!(m.server.status().connections, 1);
    m.server.stop();
    assert!(c.sessions().is_err());
    let id = Identity::generate().unwrap();
    assert!(Client::connect(addr(&m), &id, Pin::AnyForPairing, "x").is_err());
}

#[test]
fn a_base_that_is_not_verified_never_fast_forwards() {
    let m = mac();
    let (id, _) = paired(&m, "Test iPad");
    grant(&m, &id);
    let mut c = reconnect(&m, &id).unwrap();
    let rel = "Capture/B.ARW";
    let file = FileRef::Sidecar {
        rel: rel.into(),
        ordinal: 1,
    };
    let base = c.fetch(&m.id, &file).unwrap();
    // The Mac edits the exposure after the device fetched its base.
    let mut s = Session::open(&m.session).unwrap();
    let img = s
        .images()
        .unwrap()
        .into_iter()
        .find(|i| i.rel_path.as_str() == rel)
        .unwrap();
    let v = s.variants(img.id).unwrap()[0];
    let mut adj = s.load_adjustments(v).unwrap();
    adj.exposure.exposure_ev = 0.7;
    s.save_adjustments(v, &adj).unwrap();
    let current = c.fetch(&m.id, &file).unwrap();
    let current_hash = blake3::hash(&current).to_hex().to_string();
    let mut edited = Sidecar::from_json(&base).unwrap().sidecar;
    edited.adjustments.exposure.exposure_ev = -0.5;
    // Claiming the Mac's current hash without the base (or with a base that
    // isn't that sidecar) is not a fast-forward: the Mac's edit is kept.
    for (n, claimed_base) in [(2, None), (3, Some(text(&base)))] {
        let r = c
            .push_sidecar(
                &m.id,
                rel,
                1,
                Some(current_hash.clone()),
                claimed_base,
                sidecar_json(&edited.to_json()),
                i64::MAX,
            )
            .unwrap();
        assert_eq!(r.outcome, MergeOutcome::Conflict, "push {n}");
        s.sync_index().unwrap();
        let vs = s.variants(img.id).unwrap();
        assert_eq!(vs.len(), n);
        let kept = vs
            .iter()
            .find(|x| s.variant(**x).unwrap().ordinal == r.kept_ordinal.unwrap())
            .unwrap();
        assert_eq!(
            s.load_adjustments(*kept).unwrap().exposure.exposure_ev,
            0.7,
            "the Mac's edit survives"
        );
        // The Mac's edit is back on the variant for the next round.
        s.save_adjustments(v, &adj).unwrap();
    }
}

#[test]
fn silent_sockets_time_out_and_cant_take_the_paired_slots() {
    let m = mac_cfg(|c| c.handshake_deadline = std::time::Duration::from_millis(300));
    let (id, _c) = paired(&m, "iPad");
    let read_closed = |s: &mut std::net::TcpStream| {
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        matches!(io::Read::read(s, &mut [0u8; 1]), Ok(0) | Err(_))
    };
    let mut silent: Vec<std::net::TcpStream> = (0..2)
        .map(|_| std::net::TcpStream::connect(addr(&m)).unwrap())
        .collect();
    assert!(wait(|| m.server.status().handshaking == 2));
    // A third handshake from the same address drops the oldest, not itself.
    let _third = std::net::TcpStream::connect(addr(&m)).unwrap();
    assert!(
        read_closed(&mut silent[0]),
        "the oldest handshake was dropped"
    );
    assert!(wait(|| m.server.status().refused_peers >= 1));
    assert_eq!(m.server.status().handshaking, 2);
    // So a flood can't keep a real handshake out, even before the deadline.
    let mut c = reconnect(&m, &id).unwrap();
    assert_eq!(c.sessions().unwrap().len(), 1);
    // The deadline is absolute: the rest are closed.
    assert!(wait(|| m.server.status().handshaking == 0));
    assert!(read_closed(&mut silent[1]));
}

#[test]
fn unpaired_connections_are_capped_and_closed_with_the_pairing_window() {
    let m = mac();
    let (id, _) = paired(&m, "iPad");
    let _t = m.server.begin_pairing();
    // The reviewer's probe: strangers connect while pairing is open...
    let mut strangers = Vec::new();
    for _ in 0..8 {
        if let Ok(c) = Client::connect(
            addr(&m),
            &Identity::generate().unwrap(),
            Pin::AnyForPairing,
            "x",
        ) {
            strangers.push(c);
        }
    }
    assert_eq!(
        strangers.len(),
        ds_remote::server::MAX_UNPAIRED_PER_IP,
        "only one from an address may wait to pair"
    );
    // ...then the window closes: they are closed, and paired devices get in.
    m.server.cancel_pairing();
    assert!(wait(|| m.server.status().connections == 0));
    for c in &mut strangers {
        assert!(c.sessions().is_err());
    }
    let mut c = reconnect(&m, &id).unwrap();
    assert_eq!(c.sessions().unwrap().len(), 1);
    // A failed pairing attempt closes the connection too.
    let t = m.server.begin_pairing();
    let mut s = Client::connect(
        addr(&m),
        &Identity::generate().unwrap(),
        Pin::AnyForPairing,
        "x",
    )
    .unwrap();
    assert!(s.pair("bad", Secret::Code).is_err());
    assert!(
        s.pair(&t.code, Secret::Code).is_err(),
        "closed after the failure"
    );
    // A device that pairs leaves the unpaired slots and outlives the window.
    let fresh = Identity::generate().unwrap();
    let mut p = Client::connect(addr(&m), &fresh, Pin::AnyForPairing, "new").unwrap();
    let t = m.server.begin_pairing();
    p.pair(&t.code, Secret::Code).unwrap();
    assert!(!m.server.pairing_open());
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(p.sessions().unwrap().len(), 1);
}

#[test]
fn authenticated_connections_are_capped() {
    let m = mac();
    let (id, first) = paired(&m, "iPad");
    let mut more: Vec<Client> = (1..ds_remote::server::MAX_CONNECTIONS)
        .map(|_| reconnect(&m, &id).unwrap())
        .collect();
    assert!(wait(|| m.server.status().connections == 8));
    assert!(reconnect(&m, &id).is_err(), "a ninth is closed");
    drop(first);
    assert!(wait(|| m.server.status().connections == 7));
    let mut ninth = reconnect(&m, &id).unwrap();
    assert_eq!(ninth.sessions().unwrap().len(), 1);
    assert_eq!(more[0].sessions().unwrap().len(), 1);
}

fn wait(cond: impl Fn() -> bool) -> bool {
    for _ in 0..500 {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// A client that keeps TLS sessions for resumption, as a hostile one would.
#[derive(Debug)]
struct AnyKey;

impl rustls::client::danger::ServerCertVerifier for AnyKey {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("no".into()))
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
}

/// Handshake and say hello with `cfg`; whether the Mac answered.
fn hello_with(cfg: &Arc<rustls::ClientConfig>, a: SocketAddr) -> bool {
    let run = || -> io::Result<bool> {
        let sock = std::net::TcpStream::connect(a)?;
        sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let name = rustls::pki_types::ServerName::try_from("darkslide").unwrap();
        let conn = rustls::ClientConnection::new(cfg.clone(), name).map_err(io::Error::other)?;
        let mut tls = rustls::StreamOwned::new(conn, sock);
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock)?;
        }
        let hello = ds_remote::wire::Request::Hello {
            version: ds_remote::PROTOCOL_VERSION,
            name: "x".into(),
        };
        ds_remote::wire::send(&mut tls, &hello, &[])?;
        let (r, _): (ds_remote::wire::Response, _) = ds_remote::wire::recv(&mut tls)?;
        Ok(matches!(r, ds_remote::wire::Response::Welcome { .. }))
    };
    run().unwrap_or(false)
}

#[test]
fn a_key_let_in_while_pairing_cant_resume_after_the_window_closes() {
    let m = mac();
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let cert = rcgen::CertificateParams::new(vec!["x".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyKey))
            .with_client_auth_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            )
            .unwrap(),
    );
    let _t = m.server.begin_pairing();
    assert!(
        hello_with(&cfg, addr(&m)),
        "let in while the window is open"
    );
    m.server.cancel_pairing();
    assert!(
        !hello_with(&cfg, addr(&m)),
        "no resumed session gets past the closed gate"
    );
}

type Raw = rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>;

/// A TLS connection with a fresh, unpaired key (let in only while pairing is
/// open) that has said hello.
fn stranger(a: SocketAddr) -> io::Result<Raw> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let cert = rcgen::CertificateParams::new(vec!["x".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let cfg = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyKey))
        .with_client_auth_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .unwrap(),
    );
    let sock = std::net::TcpStream::connect(a)?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let name = rustls::pki_types::ServerName::try_from("darkslide").unwrap();
    let mut tls = rustls::StreamOwned::new(rustls::ClientConnection::new(cfg, name).unwrap(), sock);
    match call(&mut tls, &hello())? {
        ds_remote::wire::Response::Welcome { paired: false, .. } => Ok(tls),
        r => Err(io::Error::other(format!("{r:?}"))),
    }
}

fn hello() -> ds_remote::wire::Request {
    ds_remote::wire::Request::Hello {
        version: ds_remote::PROTOCOL_VERSION,
        name: "x".into(),
    }
}

fn call(tls: &mut Raw, r: &ds_remote::wire::Request) -> io::Result<ds_remote::wire::Response> {
    ds_remote::wire::send(tls, r, &[])?;
    ds_remote::wire::recv(tls).map(|(r, _)| r)
}

#[test]
fn an_unpaired_connection_gets_one_pairing_attempt() {
    use ds_remote::wire::{Request, Response};
    let m = mac();
    let _t = m.server.begin_pairing();
    let mut tls = stranger(addr(&m)).unwrap();
    // A valid SPAKE2 message (any password): the start succeeds...
    let (_, msg) = spake2::Spake2::<spake2::Ed25519Group>::start_a(
        &spake2::Password::new(b"guess"),
        &spake2::Identity::new(b"darkslide link client"),
        &spake2::Identity::new(b"darkslide link server"),
    );
    let hex = msg.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    let start = Request::PairStart {
        secret: Secret::Code,
        spake: hex,
    };
    assert!(matches!(
        call(&mut tls, &start).unwrap(),
        Response::PairChallenge { .. }
    ));
    // ...but starting over to keep the slot is refused, and the connection closed.
    let again = call(&mut tls, &start).unwrap();
    assert!(
        matches!(&again, Response::Error { code: ErrorCode::BadRequest, message } if message.contains("one pairing attempt")),
        "{again:?}"
    );
    assert!(call(&mut tls, &hello()).is_err(), "closed");
}

#[test]
fn pair_finish_before_pair_start_closes_the_connection() {
    use ds_remote::wire::{Request, Response};
    let m = mac();
    let _t = m.server.begin_pairing();
    let mut s = stranger(addr(&m)).unwrap();
    let finish = Request::PairFinish {
        confirm: "00".into(),
    };
    // Out of order: refused and hung up on, so it can't be repeated to hold a slot.
    let r = call(&mut s, &finish).unwrap();
    assert!(
        matches!(&r, Response::Error { code: ErrorCode::BadRequest, message } if message.contains("pair_start first")),
        "{r:?}"
    );
    assert!(call(&mut s, &finish).is_err(), "closed");
    assert!(wait(|| m.server.status().connections == 0));
}

#[test]
fn an_unpaired_connection_must_pair_within_its_deadline() {
    let m = mac_cfg(|c| c.unpaired_deadline = std::time::Duration::from_millis(300));
    let _t = m.server.begin_pairing();
    let mut s = stranger(addr(&m)).unwrap();
    assert_eq!(m.server.status().connections, 1);
    // Closed well before the idle timeout, with the window still open.
    assert!(wait(|| m.server.status().connections == 0));
    assert!(m.server.pairing_open());
    assert!(call(&mut s, &hello()).is_err(), "closed");
}

#[test]
fn unpaired_connections_are_capped_per_address() {
    // Listening on both stacks: 127.0.0.1 and ::1 are two addresses.
    let m = mac_cfg(|c| c.bind = "[::]:0".parse().unwrap());
    let port = addr(&m).port();
    let v4 = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let v6 = SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port));
    let _t = m.server.begin_pairing();
    let _a = stranger(v4).unwrap();
    assert!(stranger(v4).is_err(), "one per address");
    let b = stranger(v6).unwrap();
    assert!(stranger(v6).is_err(), "one per /64");
    assert_eq!(m.server.status().connections, 2);
    drop(b);
    assert!(wait(|| m.server.status().connections == 1));
    assert!(stranger(v6).is_ok(), "a closed one frees its address");
}

#[test]
fn malformed_pairing_messages_close_the_connection_but_dont_use_up_attempts() {
    use ds_remote::wire::{Request, Response};
    let m = mac();
    let t = m.server.begin_pairing();
    for _ in 0..=ds_remote::pairing::MAX_ATTEMPTS {
        assert!(wait(|| m.server.status().connections == 0));
        let mut s = stranger(addr(&m)).unwrap();
        let start = Request::PairStart {
            secret: Secret::Code,
            spake: "zz".into(),
        };
        let r = call(&mut s, &start).unwrap();
        assert!(
            matches!(
                r,
                Response::Error {
                    code: ErrorCode::PairingFailed,
                    ..
                }
            ),
            "{r:?}"
        );
        assert!(call(&mut s, &hello()).is_err(), "closed");
    }
    // No guess was made: the window is still open and the code still works.
    assert!(m.server.pairing_open());
    assert!(wait(|| m.server.status().connections == 0));
    let mut c = Client::connect(
        addr(&m),
        &Identity::generate().unwrap(),
        Pin::AnyForPairing,
        "x",
    )
    .unwrap();
    c.pair(&t.code, Secret::Code).unwrap();
}

/// A session of `n` empty photos, each with a long path, served on loopback.
fn mac_n(n: usize) -> Mac {
    let t = tempfile::tempdir().unwrap();
    let session = t.path().join("Big");
    let mut s = Session::create(&session, "Big").unwrap();
    let items = (0..n)
        .map(|i| {
            let rel = format!("Capture/2026-09-21/DSC{i:06}-a-long-file-name.ARW");
            fs::create_dir_all(session.join("Capture/2026-09-21")).unwrap();
            fs::write(session.join(&rel), b"").unwrap();
            ds_session::NewOriginal {
                rel,
                hash: hash(&(i as u64).to_le_bytes()),
                content_hash: None,
                meta: ImageMetaLite::default(),
                replaced: Vec::new(),
                iptc: ds_core::IptcMeta::default(),
            }
        })
        .collect();
    s.add_new_originals(items).unwrap();
    drop(s);
    let state = t.path().join("mac-state");
    let mut cfg = ServerConfig::new("Test Mac", &state, "127.0.0.1:0".parse().unwrap());
    cfg.media = Arc::new(FakeMedia);
    let server = Server::start(cfg).unwrap();
    let root = SessionRoot {
        path: session.clone(),
        name: "Big".into(),
    };
    let id = root.id();
    server.set_sessions(vec![root]);
    Mac {
        _t: t,
        state,
        session,
        server,
        id,
    }
}

#[test]
fn big_sessions_list_in_pages_that_fit_a_frame() {
    for n in [1_000, 5_000, 20_000] {
        let m = mac_n(n);
        let (_id, mut c) = paired(&m, "iPad");
        let photos = c.photos(&m.id).unwrap();
        assert_eq!(photos.len(), n);
        assert!(
            photos.windows(2).all(|w| w[0].rel < w[1].rel),
            "sorted, once each"
        );
        assert!(photos.iter().all(|p| p.variants.len() == 1));
    }
}

#[test]
fn list_photos_pages_follow_next_and_cap_the_limit() {
    use ds_remote::wire::{Request, Response};
    let m = mac_n(30);
    let (_id, mut c) = paired(&m, "iPad");
    let page = |c: &mut Client, after: Option<String>, limit| {
        let req = Request::ListPhotos {
            session: m.id.clone(),
            after,
            limit,
        };
        match c.call(&req, &[]).unwrap() {
            (Response::Photos { photos, next }, _) => (photos, next),
            (r, _) => panic!("{r:?}"),
        }
    };
    let (first, next) = page(&mut c, None, 10);
    assert_eq!(first.len(), 10);
    assert_eq!(next.as_deref(), Some(first[9].rel.as_str()));
    let (second, next) = page(&mut c, next, 10);
    assert_eq!(
        second[0].rel,
        format!("Capture/2026-09-21/DSC{:06}-a-long-file-name.ARW", 10)
    );
    let (third, next) = page(&mut c, next, 10);
    assert_eq!((third.len(), next), (10, None), "the last page has no next");
    let (all, next) = page(&mut c, None, 0);
    assert_eq!(
        (all.len(), next),
        (30, None),
        "limit 0 is the server's default"
    );
}

/// Media that can panic or stall, to prove a bad file costs one request.
#[derive(Default)]
struct FlakyMedia {
    panic_meta: AtomicBool,
    preview_ms: AtomicU32,
}

impl Media for FlakyMedia {
    fn preview(&self, original: &Path) -> io::Result<Vec<u8>> {
        std::thread::sleep(std::time::Duration::from_millis(
            self.preview_ms.load(Ordering::SeqCst).into(),
        ));
        FakeMedia.preview(original)
    }
    fn meta(&self, original: &Path) -> ImageMetaLite {
        assert!(!self.panic_meta.load(Ordering::SeqCst), "hostile RAW");
        FakeMedia.meta(original)
    }
}

fn flaky_mac(media: &Arc<FlakyMedia>, idle: Option<std::time::Duration>) -> Mac {
    let media = media.clone();
    mac_cfg(move |cfg| {
        cfg.media = media;
        if let Some(i) = idle {
            cfg.idle = i;
        }
    })
}

#[test]
fn a_panicking_handler_costs_a_request_not_a_connection_slot() {
    let media = Arc::new(FlakyMedia::default());
    media.panic_meta.store(true, Ordering::SeqCst);
    let m = flaky_mac(&media, None);
    let (id, c) = paired(&m, "iPad");
    drop(c);
    grant(&m, &id);
    let dir = tempfile::tempdir().unwrap();
    // More failed pushes than MAX_CONNECTIONS: leaked slots would lock the device out.
    for i in 0..(ds_remote::server::MAX_CONNECTIONS + 3) {
        let mut c = reconnect(&m, &id).unwrap();
        let f = dir.path().join(format!("BAD{i}.ARW"));
        fs::write(&f, bytes(i as u8, 100 + i)).unwrap();
        let e = c.push(&m.id, &f).unwrap_err();
        assert_eq!(e.code(), Some(ErrorCode::Internal), "{e}");
    }
    assert!(wait(|| m.server.status().connections == 0));
    let mut c = reconnect(&m, &id).unwrap();
    assert_eq!(c.sessions().unwrap().len(), 1);
}

#[test]
fn a_pushed_original_that_failed_to_index_is_indexed_on_retry() {
    let media = Arc::new(FlakyMedia::default());
    media.panic_meta.store(true, Ordering::SeqCst);
    let m = flaky_mac(&media, None);
    let (id, c) = paired(&m, "iPad");
    drop(c);
    grant(&m, &id);
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("LOST.ARW");
    fs::write(&f, bytes(9, 500)).unwrap();
    let mut c = reconnect(&m, &id).unwrap();
    assert!(c.push(&m.id, &f).is_err());
    let mut c = reconnect(&m, &id).unwrap(); // the failed request closed the link
    let placed = m.session.join("Capture/From iPad/LOST.ARW");
    assert!(placed.is_file(), "placed on disk, not indexed");
    assert!(
        !c.photos(&m.id)
            .unwrap()
            .iter()
            .any(|p| p.rel.ends_with("LOST.ARW"))
    );

    media.panic_meta.store(false, Ordering::SeqCst);
    let mut c = reconnect(&m, &id).unwrap();
    let r = c.push(&m.id, &f).unwrap();
    assert!(r.duplicate);
    assert!(
        c.photos(&m.id).unwrap().iter().any(|p| p.rel == r.rel),
        "the retry registered the file the first try left behind"
    );
    assert_eq!(m.server.take_changed(), vec![m.session.clone()]);
}

#[test]
fn the_idle_sweep_leaves_a_request_in_progress_alone() {
    let media = Arc::new(FlakyMedia::default());
    media.preview_ms.store(1500, Ordering::SeqCst);
    let m = flaky_mac(&media, Some(std::time::Duration::from_millis(400)));
    let (_id, mut c) = paired(&m, "iPad");
    let file = FileRef::Preview {
        rel: "Capture/B.ARW".into(),
    };
    c.stat(&m.id, &file)
        .expect("a slow request outlives the idle limit");
    // Once it is answered, the connection is idle and swept as usual.
    assert!(wait(|| m.server.status().connections == 0));
}

#[test]
fn device_names_are_bounded_and_clean() {
    let m = mac();
    let id = Identity::generate().unwrap();
    let t = m.server.begin_pairing();
    let name = format!("Pad\n{}", "x".repeat(5000));
    let mut c = Client::connect(addr(&m), &id, Pin::AnyForPairing, &name).unwrap();
    c.pair(&t.code, Secret::Code).unwrap();
    let stored = m.server.devices().list().unwrap();
    assert_eq!(stored.len(), 1);
    assert!(
        stored[0].name.chars().count() <= 64,
        "{}",
        stored[0].name.len()
    );
    assert!(!stored[0].name.contains('\n'));
}

#[test]
fn sidecars_of_photos_not_in_the_session_are_not_served() {
    let m = mac();
    let (_id, mut c) = paired(&m, "iPad");
    let e = c
        .stat(
            &m.id,
            &FileRef::Sidecar {
                rel: "Capture/NOPE.ARW".into(),
                ordinal: 1,
            },
        )
        .unwrap_err();
    assert_eq!(e.code(), Some(ErrorCode::NotFound));
}
