//! Pairing: a short-lived secret shown on the Mac (a 6-digit code to type, or
//! a QR code that also carries the Mac's key fingerprint and a long token),
//! and the lists of paired devices (on the Mac) and servers (on the client).
//!
//! The secret is never sent. Both sides run SPAKE2 over it, then confirm the
//! derived key with a MAC over *both TLS key fingerprints*. A device in the
//! middle (which must present its own key to each side) can't produce
//! matching confirmations without the secret, and gets one guess per attempt;
//! the Mac allows [`MAX_ATTEMPTS`] wrong confirmations before it closes the
//! window. A secret works once.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

use crate::identity::Fingerprint;
use crate::wire::Secret;
use crate::{RemoteError, Result};

/// How long a pairing code stays valid.
pub const PAIRING_TTL: Duration = Duration::from_secs(300);
/// Wrong confirmations before the window closes.
pub const MAX_ATTEMPTS: u32 = 3;
/// URL scheme of the QR payload.
pub const QR_SCHEME: &str = "darkslide-pair";

const DEVICES_FILE: &str = "devices.json";
const SERVERS_FILE: &str = "servers.json";

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    SystemRandom::new()
        .fill(&mut b)
        .expect("the system random source works");
    b
}

/// A device allowed to connect to this Mac.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub fingerprint: String,
    pub name: String,
    pub paired_ms: i64,
    /// May push files and edits; off until granted in Settings.
    #[serde(default)]
    pub write: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    devices: Vec<Device>,
}

/// The paired-device list, `devices.json` in the state directory, written
/// atomically. Checked against the file on every request (its mtime, size and
/// inode decide whether the parsed copy is still good), so a revoke from
/// anywhere (Settings, the CLI) applies to the next request.
#[derive(Debug)]
pub struct DeviceStore {
    path: PathBuf,
    write: Mutex<()>,
    cache: Mutex<Option<(FileStamp, Vec<Device>)>>,
}

/// What identifies one version of `devices.json` (writes replace the file).
type FileStamp = (i64, i64, u64, u64);

/// How long [`DeviceStore::update`] waits for another process's write.
const LOCK_WAIT: Duration = Duration::from_secs(5);

impl DeviceStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(DEVICES_FILE),
            write: Mutex::new(()),
            cache: Mutex::new(None),
        }
    }

    pub fn list(&self) -> Result<Vec<Device>> {
        let stamp = fs::metadata(&self.path)
            .ok()
            .map(|m| (m.mtime(), m.mtime_nsec(), m.size(), m.ino()));
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let (Some(stamp), Some((have, devices))) = (stamp, cache.as_ref())
            && *have == stamp
        {
            return Ok(devices.clone());
        }
        // The stamp is taken before the read: a write in between only makes
        // the next check re-read.
        let devices = read_json::<DevicesFile>(&self.path)?.devices;
        *cache = stamp.map(|s| (s, devices.clone()));
        Ok(devices)
    }

    pub fn find(&self, fp: &Fingerprint) -> Result<Option<Device>> {
        let hex = fp.to_hex();
        Ok(self.list()?.into_iter().find(|d| d.fingerprint == hex))
    }

    /// Add or replace (same key) a device.
    pub fn add(&self, device: Device) -> Result<()> {
        self.update(|ds| {
            ds.retain(|d| d.fingerprint != device.fingerprint);
            ds.push(device);
            true
        })
        .map(drop)
    }

    /// Forget a device. Whether it was paired.
    pub fn revoke(&self, fingerprint: &str) -> Result<bool> {
        self.update(|ds| {
            let n = ds.len();
            ds.retain(|d| d.fingerprint != fingerprint);
            ds.len() != n
        })
    }

    /// Grant or withdraw write access. Whether the device is paired.
    pub fn set_write(&self, fingerprint: &str, write: bool) -> Result<bool> {
        self.update(|ds| {
            ds.iter_mut()
                .find(|d| d.fingerprint == fingerprint)
                .map(|d| d.write = write)
                .is_some()
        })
    }

    fn update(&self, f: impl FnOnce(&mut Vec<Device>) -> bool) -> Result<bool> {
        let _g = self.write.lock().expect("device store lock");
        // Another process (the CLI revoking while the app pairs) takes turns.
        let lock_path = self.path.with_extension("lock");
        let started = Instant::now();
        let _flock = loop {
            if let Some(l) = ds_fsutil::DirLock::try_lock(&lock_path)? {
                break l;
            }
            if started.elapsed() > LOCK_WAIT {
                return Err(RemoteError::Protocol(
                    "the device list is being changed by another process".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut file = read_json::<DevicesFile>(&self.path)?;
        let changed = f(&mut file.devices);
        if changed {
            write_json(&self.path, &file)?;
        }
        Ok(changed)
    }
}

/// A Mac this client has paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownServer {
    pub fingerprint: String,
    pub name: String,
    /// `host:port` it was paired at.
    pub addr: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ServersFile {
    #[serde(default)]
    servers: Vec<KnownServer>,
}

/// The client's paired Macs, `servers.json`; the last one paired comes last.
#[derive(Debug)]
pub struct ServerStore {
    path: PathBuf,
}

impl ServerStore {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(SERVERS_FILE),
        }
    }

    pub fn list(&self) -> Result<Vec<KnownServer>> {
        read_json::<ServersFile>(&self.path).map(|f| f.servers)
    }

    pub fn add(&self, server: KnownServer) -> Result<()> {
        let mut file = read_json::<ServersFile>(&self.path)?;
        file.servers.retain(|s| s.fingerprint != server.fingerprint);
        file.servers.push(server);
        write_json(&self.path, &file)
    }
}

fn read_json<T: Default + serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)
            .map_err(|e| RemoteError::Protocol(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(RemoteError::file(path, e)),
    }
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(v).expect("plain data serializes");
    ds_fsutil::write_atomic(path, &bytes).map_err(|e| RemoteError::file(path, e))
}

/// What the Mac shows while pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingTicket {
    /// Six digits to type.
    pub code: String,
    /// 128-bit token, hex, carried by the QR code.
    pub token: String,
    pub expires_in: Duration,
}

impl PairingTicket {
    /// The QR payload: `darkslide-pair://<host>:<port>?fp=<hex>&t=<token>&c=<code>`.
    pub fn qr_payload(&self, host: &str, port: u16, server: &Fingerprint) -> String {
        format!(
            "{QR_SCHEME}://{host}:{port}?fp={}&t={}&c={}",
            server.to_hex(),
            self.token,
            self.code
        )
    }
}

/// A parsed QR payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrPayload {
    pub addr: String,
    pub fingerprint: Fingerprint,
    pub token: String,
}

impl QrPayload {
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix(QR_SCHEME)?.strip_prefix("://")?;
        let (addr, query) = rest.split_once('?')?;
        let mut fp = None;
        let mut token = None;
        for kv in query.split('&') {
            match kv.split_once('=') {
                Some(("fp", v)) => fp = Fingerprint::from_hex(v),
                Some(("t", v)) => token = Some(v.to_owned()),
                _ => {}
            }
        }
        (!addr.is_empty()).then_some(())?;
        Some(Self {
            addr: addr.to_owned(),
            fingerprint: fp?,
            token: token.filter(|t| !t.is_empty())?,
        })
    }
}

/// An open pairing window on the Mac.
#[derive(Debug)]
pub(crate) struct Window {
    /// Ties an exchange to the window it started in (a new window voids it).
    id: u64,
    code: String,
    token: String,
    expires: Instant,
    attempts_left: u32,
}

impl Window {
    pub(crate) fn open(ttl: Duration) -> (Self, PairingTicket) {
        let n = u32::from_be_bytes(random::<4>()) % 1_000_000;
        let code = format!("{n:06}");
        let token = blake3::Hash::from(random::<32>()).to_hex()[..32].to_owned();
        let w = Self {
            id: u64::from_be_bytes(random::<8>()),
            code: code.clone(),
            token: token.clone(),
            expires: Instant::now() + ttl,
            attempts_left: MAX_ATTEMPTS,
        };
        (
            w,
            PairingTicket {
                code,
                token,
                expires_in: ttl,
            },
        )
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn is_open(&self) -> bool {
        Instant::now() < self.expires && self.attempts_left > 0
    }

    pub(crate) fn secret(&self, kind: Secret) -> &str {
        match kind {
            Secret::Code => &self.code,
            Secret::Token => &self.token,
        }
    }

    /// Count a failed attempt; whether the window is still open.
    pub(crate) fn fail(&mut self) -> bool {
        self.attempts_left = self.attempts_left.saturating_sub(1);
        self.is_open()
    }
}

const ID_CLIENT: &[u8] = b"darkslide link client";
const ID_SERVER: &[u8] = b"darkslide link server";

/// One side of the SPAKE2 exchange.
#[derive(Debug)]
pub(crate) struct Exchange(Spake2<Ed25519Group>);

impl Exchange {
    /// Start as the client (A) or server (B); returns the message to send (hex).
    pub(crate) fn start(secret: &str, client: bool) -> (Self, String) {
        let pw = Password::new(secret.as_bytes());
        let (a, b) = (SpakeId::new(ID_CLIENT), SpakeId::new(ID_SERVER));
        let (s, msg) = if client {
            Spake2::<Ed25519Group>::start_a(&pw, &a, &b)
        } else {
            Spake2::<Ed25519Group>::start_b(&pw, &a, &b)
        };
        (Self(s), hex(&msg))
    }

    /// Finish with the peer's message; the confirmation key.
    pub(crate) fn finish(self, peer_msg: &str) -> Result<[u8; 32]> {
        let msg = unhex(peer_msg).ok_or_else(|| RemoteError::Pairing("bad message".into()))?;
        let k = self
            .0
            .finish(&msg)
            .map_err(|e| RemoteError::Pairing(format!("{e:?}")))?;
        Ok(blake3::derive_key("darkslide link pairing confirm v1", &k))
    }
}

/// The confirmation `role` ("client"/"server") sends, binding both TLS keys.
pub(crate) fn confirm(
    key: &[u8; 32],
    role: &str,
    client: &Fingerprint,
    server: &Fingerprint,
) -> String {
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(role.as_bytes());
    h.update(&client.0);
    h.update(&server.0);
    h.finalize().to_hex().to_string()
}

/// Constant-time comparison of two hex confirmations.
pub(crate) fn confirms_match(got: &str, want: &str) -> bool {
    match (blake3::Hash::from_hex(got), blake3::Hash::from_hex(want)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    b.iter()
        .fold(String::with_capacity(b.len() * 2), |mut s, x| {
            let _ = write!(s, "{x:02x}");
            s
        })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(b: u8) -> Fingerprint {
        Fingerprint([b; 32])
    }

    #[test]
    fn device_store_adds_grants_and_revokes_atomically() {
        let t = tempfile::tempdir().unwrap();
        let s = DeviceStore::new(t.path());
        assert!(s.list().unwrap().is_empty(), "no file yet = no devices");
        let d = Device {
            fingerprint: fp(1).to_hex(),
            name: "iPad".into(),
            paired_ms: 5,
            write: false,
        };
        s.add(d.clone()).unwrap();
        s.add(d.clone()).unwrap();
        assert_eq!(s.list().unwrap(), vec![d.clone()], "re-pairing replaces");
        assert!(s.set_write(&d.fingerprint, true).unwrap());
        assert!(s.find(&fp(1)).unwrap().unwrap().write);
        assert!(!s.set_write(&fp(2).to_hex(), true).unwrap());
        assert!(s.revoke(&d.fingerprint).unwrap());
        assert!(!s.revoke(&d.fingerprint).unwrap());
        assert_eq!(s.find(&fp(1)).unwrap(), None);
        fs::write(t.path().join(DEVICES_FILE), b"{oops").unwrap();
        assert!(
            s.list().is_err(),
            "a damaged list is an error, not 'no devices'"
        );
        assert!(s.add(d).is_err(), "and is never overwritten");
        assert_eq!(fs::read(t.path().join(DEVICES_FILE)).unwrap(), b"{oops");
    }

    #[test]
    fn a_cached_device_list_follows_another_stores_writes() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (DeviceStore::new(t.path()), DeviceStore::new(t.path()));
        let d = Device {
            fingerprint: fp(3).to_hex(),
            name: "iPad".into(),
            paired_ms: 1,
            write: false,
        };
        a.add(d.clone()).unwrap();
        assert!(a.find(&fp(3)).unwrap().is_some(), "cached");
        assert!(b.revoke(&d.fingerprint).unwrap());
        assert_eq!(
            a.find(&fp(3)).unwrap(),
            None,
            "a revoke from anywhere applies at once"
        );
        b.add(d).unwrap();
        assert!(b.set_write(&fp(3).to_hex(), true).unwrap());
        assert!(
            a.find(&fp(3)).unwrap().unwrap().write,
            "same size, new file"
        );
    }

    #[test]
    fn updates_wait_for_another_process_holding_the_file_lock() {
        let t = tempfile::tempdir().unwrap();
        let s = DeviceStore::new(t.path());
        let lock = ds_fsutil::DirLock::try_lock(&t.path().join("devices.lock"))
            .unwrap()
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(lock);
        });
        let started = Instant::now();
        s.add(Device {
            fingerprint: fp(4).to_hex(),
            name: "iPad".into(),
            paired_ms: 1,
            write: false,
        })
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(100));
        release.join().unwrap();
        assert!(s.find(&fp(4)).unwrap().is_some());
    }

    #[test]
    fn server_store_keeps_the_latest_last() {
        let t = tempfile::tempdir().unwrap();
        let s = ServerStore::new(t.path());
        let k = |b, n: &str| KnownServer {
            fingerprint: fp(b).to_hex(),
            name: n.into(),
            addr: "127.0.0.1:1".into(),
        };
        s.add(k(1, "a")).unwrap();
        s.add(k(2, "b")).unwrap();
        s.add(k(1, "a2")).unwrap();
        let names: Vec<_> = s.list().unwrap().into_iter().map(|k| k.name).collect();
        assert_eq!(names, ["b", "a2"]);
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join(SERVERS_FILE)).unwrap();
        assert!(matches!(
            ServerStore::new(d.path()).list(),
            Err(RemoteError::File { .. })
        ));
    }

    #[test]
    fn qr_payload_round_trips_and_rejects_junk() {
        let (w, t) = Window::open(PAIRING_TTL);
        assert_eq!(t.code.len(), 6);
        assert!(t.code.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(t.token.len(), 32);
        assert_eq!(w.secret(Secret::Code), t.code);
        assert_eq!(w.secret(Secret::Token), t.token);
        let p = t.qr_payload("mac.local", 4242, &fp(7));
        let q = QrPayload::parse(&p).unwrap();
        assert_eq!(q.addr, "mac.local:4242");
        assert_eq!(q.fingerprint, fp(7));
        assert_eq!(q.token, t.token);
        for junk in [
            "http://x?fp=1",
            "darkslide-pair://x",
            "darkslide-pair://?fp=00&t=1",
            &format!("darkslide-pair://h:1?fp={}", fp(1).to_hex()),
            &format!("darkslide-pair://h:1?fp={}&t=", fp(1).to_hex()),
            "darkslide-pair://h:1?fp=zz&t=abc&x",
        ] {
            assert_eq!(QrPayload::parse(junk), None, "{junk}");
        }
    }

    #[test]
    fn window_expires_and_counts_attempts() {
        let (mut w, _) = Window::open(PAIRING_TTL);
        assert!(w.is_open());
        assert!(w.fail());
        assert!(w.fail());
        assert!(!w.fail(), "third wrong confirmation closes it");
        let (w, _) = Window::open(Duration::ZERO);
        assert!(!w.is_open());
        assert_ne!(
            w.id(),
            Window::open(PAIRING_TTL).0.id(),
            "each window has its own id"
        );
    }

    #[test]
    fn spake_agrees_only_on_the_same_secret() {
        let run = |a: &str, b: &str| {
            let (ca, ma) = Exchange::start(a, true);
            let (sb, mb) = Exchange::start(b, false);
            (ca.finish(&mb).unwrap(), sb.finish(&ma).unwrap())
        };
        let (k1, k2) = run("123456", "123456");
        assert_eq!(k1, k2);
        let (c, s) = (fp(1), fp(2));
        assert!(confirms_match(
            &confirm(&k1, "client", &c, &s),
            &confirm(&k2, "client", &c, &s)
        ));
        assert!(
            !confirms_match(
                &confirm(&k1, "client", &c, &s),
                &confirm(&k1, "client", &c, &fp(3))
            ),
            "a substituted server key changes the confirmation"
        );
        assert!(!confirms_match(
            &confirm(&k1, "client", &c, &s),
            &confirm(&k1, "server", &c, &s)
        ));
        let (k3, k4) = run("123456", "654321");
        assert_ne!(k3, k4);
        assert!(!confirms_match("zz", "zz"));
        let (x, _) = Exchange::start("1", true);
        assert!(x.finish("abc").is_err(), "odd hex");
        let (x, _) = Exchange::start("1", true);
        assert!(x.finish("00").is_err(), "bad point");
        assert_eq!(unhex("0g"), None);
        assert_eq!(hex(&[0, 255]), "00ff");
    }
}
