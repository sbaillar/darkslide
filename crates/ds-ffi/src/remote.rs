//! The Mac ↔ iPad link (`ds-remote`): the server behind Settings ▸ Devices,
//! and the paired-device list (readable and editable while the server is off;
//! the server re-reads it on every request, so a revoke applies at once).
//!
//! The open session is served through the app's own [`SessionHandle`], so a
//! device's sidecar writes and the app's are serialized on one lock. A variant
//! a device changed is "unseen" until the app has its edits in its model again
//! (`remote_mark_seen`); if
//! the app saves over an unseen device edit (an edit made before it reloaded),
//! the device's edit is first kept as a variant, so neither side's is lost.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use ds_core::Adjustments;
use ds_remote::server::{OwnSessions, SessionAccess};
use ds_remote::{DeviceStore, Fingerprint, Server, ServerConfig, SessionRoot};
use ds_session::{Session, VariantId};

use crate::error::DsError;
use crate::jobs::guard;
use crate::session::SessionHandle;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Variants a device changed that the app hasn't adopted since:
/// (session root, variant id) → (the device's name, the write's generation).
static UNSEEN: Mutex<BTreeMap<(PathBuf, i64), (String, u64)>> = Mutex::new(BTreeMap::new());

/// Device write generation: bumped by every device write (under the
/// session's lock), read with each edits snapshot (under the same lock).
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The generation an edits snapshot taken now includes (call with the
/// session's lock held, as `load_all_adjustments_json` does).
pub(crate) fn device_generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// The app's editor now holds this session's device edits up to
/// `generation` (the snapshot it adopted): later saves over those are
/// ordinary edits. Device writes after the snapshot stay unseen.
fn mark_seen(root: &Path, generation: u64) {
    lock(&UNSEEN).retain(|(r, _), (_, g)| r != root || *g > generation);
}

/// Called by the app once the edits it loaded (`AllAdjustmentsJson`'s
/// `device_generation`) are in its model, not when the store merely read
/// them, so a save queued during the reload, or a push that landed after
/// the snapshot, still keeps the device's edit.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn remote_mark_seen(handle: Arc<SessionHandle>, generation: u64) {
    mark_seen(lock(&handle.session).root(), generation);
}

/// Before the app saves `edits`: a variant a device changed since the app
/// last loaded it keeps the device's adjustments as a new variant
/// ("Edited on <device>") when the app's save would replace them.
pub(crate) fn keep_unseen_device_edits(
    s: &mut Session,
    edits: &[(VariantId, &Adjustments)],
) -> ds_session::Result<()> {
    let root = s.root().to_owned();
    for (v, new) in edits {
        let Some((device, _)) = lock(&UNSEEN).remove(&(root.clone(), v.0)) else {
            continue;
        };
        let current = s.load_adjustments(*v)?;
        if current != **new {
            let image = s.variant(*v)?.image;
            let kept = s.new_variant(image)?;
            s.save_adjustments(kept, &current)?;
            s.rename_variant(kept, &format!("Edited on {device}"))?;
        }
    }
    Ok(())
}

/// [`SessionAccess`] on the app's open session, else sessions of its own.
#[derive(Default)]
struct AppSessions {
    open: Mutex<Option<(PathBuf, Arc<SessionHandle>)>>,
    own: OwnSessions,
}

impl SessionAccess for AppSessions {
    fn with_session(&self, root: &Path, f: &mut dyn FnMut(&mut Session)) -> ds_remote::Result<()> {
        let handle = lock(&self.open)
            .as_ref()
            .filter(|(r, _)| r == root)
            .map(|(_, h)| h.clone());
        match handle {
            Some(h) => {
                f(&mut lock(&h.session));
                Ok(())
            }
            None => self.own.with_session(root, f),
        }
    }

    fn device_wrote(&self, root: &Path, variants: &[VariantId], device: &str) {
        let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        let mut u = lock(&UNSEEN);
        for v in variants {
            u.insert((root.to_owned(), v.0), (device.to_owned(), generation));
        }
    }
}

/// A session the iPad may see.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RemoteSession {
    pub path: String,
    pub name: String,
}

/// A paired device.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RemoteDevice {
    pub fingerprint: String,
    /// `ab12 cd34 ef56 7890`, to compare by eye.
    pub short_fingerprint: String,
    pub name: String,
    pub paired_ms: i64,
    /// May push photos and edits.
    pub write: bool,
}

/// What Settings shows while pairing.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RemotePairing {
    /// Six digits to type on the iPad.
    pub code: String,
    /// The QR code's text (host, port, this Mac's key fingerprint, a one-time token).
    pub qr_payload: String,
    pub expires_in_secs: u32,
}

/// For the status indicator.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RemoteStatus {
    pub connections: u32,
    /// A transfer moved data in the last two seconds.
    pub transferring: bool,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub refused_peers: u64,
}

fn err(e: ds_remote::RemoteError) -> DsError {
    match e {
        ds_remote::RemoteError::Io(e) => e.into(),
        e => DsError::Io {
            message: e.to_string(),
        },
    }
}

/// The running link server. Dropping it (or `stop`) closes the port.
#[derive(uniffi::Object)]
pub struct RemoteServer {
    inner: Server,
    sessions: Arc<AppSessions>,
}

impl std::fmt::Debug for RemoteServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteServer").finish_non_exhaustive()
    }
}

/// The port the app asks for first.
#[uniffi::export]
pub fn remote_default_port() -> u16 {
    ds_remote::cli::DEFAULT_PORT
}

fn start_on(
    state_dir: &Path,
    name: &str,
    bind: SocketAddr,
    sessions: &Arc<AppSessions>,
) -> Result<Server, DsError> {
    let mut cfg = ServerConfig::new(name, state_dir, bind);
    cfg.sessions = sessions.clone();
    Server::start(cfg).map_err(err)
}

impl RemoteServer {
    /// [`RemoteServer::start`] on one address (tests bind loopback only).
    fn start_at(state_dir: &str, name: &str, ip: IpAddr, port: u16) -> Result<Arc<Self>, DsError> {
        guard(|| {
            let dir = PathBuf::from(state_dir);
            let sessions = Arc::new(AppSessions::default());
            let inner = match start_on(&dir, name, SocketAddr::new(ip, port), &sessions) {
                Err(DsError::Io { .. }) if port != 0 => {
                    start_on(&dir, name, SocketAddr::new(ip, 0), &sessions)?
                }
                r => r?,
            };
            Ok(Arc::new(Self { inner, sessions }))
        })
    }
}

#[uniffi::export]
impl RemoteServer {
    /// Listen at `port` (0 = any; a busy port falls back to any) on every
    /// interface, or on loopback only when `all_interfaces` is false (tests,
    /// trying it out). Only private, link-local and tailnet peers are
    /// accepted. `state_dir` holds this Mac's key and the paired devices.
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    #[uniffi::constructor(name = "start")]
    pub fn start(
        state_dir: String,
        name: String,
        port: u16,
        all_interfaces: bool,
    ) -> Result<Arc<Self>, DsError> {
        let ip: IpAddr = if all_interfaces {
            Ipv6Addr::UNSPECIFIED.into()
        } else {
            Ipv6Addr::LOCALHOST.into()
        };
        Self::start_at(&state_dir, &name, ip, port)
    }

    pub fn port(&self) -> u16 {
        self.inner.addr().port()
    }

    /// This Mac's key fingerprint, short form.
    pub fn fingerprint(&self) -> String {
        self.inner.fingerprint().short()
    }

    pub fn set_sessions(&self, sessions: Vec<RemoteSession>) {
        self.inner.set_sessions(
            sessions
                .into_iter()
                .map(|s| SessionRoot {
                    path: PathBuf::from(s.path),
                    name: s.name,
                })
                .collect(),
        );
    }

    /// Open a five-minute pairing window; `host` is the name the QR code
    /// tells the iPad to connect to.
    /// Serve the app's open session (`None`: nothing), through its own handle
    /// so the device's writes and the app's share one lock.
    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn set_open_session(&self, handle: Option<Arc<SessionHandle>>, name: String) {
        let open = handle.map(|h| {
            let root = lock(&h.session).root().to_owned();
            (root, h)
        });
        let roots = open
            .iter()
            .map(|(root, _)| SessionRoot {
                path: root.clone(),
                name: name.clone(),
            })
            .collect();
        *lock(&self.sessions.open) = open;
        self.inner.set_sessions(roots);
    }

    #[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
    pub fn begin_pairing(&self, host: String) -> RemotePairing {
        let t = self.inner.begin_pairing();
        RemotePairing {
            qr_payload: t.qr_payload(&host, self.port(), &self.inner.fingerprint()),
            code: t.code,
            expires_in_secs: t.expires_in.as_secs() as u32,
        }
    }

    pub fn cancel_pairing(&self) {
        self.inner.cancel_pairing();
    }

    pub fn pairing_open(&self) -> bool {
        self.inner.pairing_open()
    }

    pub fn status(&self) -> RemoteStatus {
        let s = self.inner.status();
        RemoteStatus {
            connections: s.connections,
            transferring: s.transferring,
            bytes_sent: s.bytes_sent,
            bytes_received: s.bytes_received,
            refused_peers: s.refused_peers,
        }
    }

    /// Roots of sessions a device changed since the last call (reload them).
    pub fn take_changed_sessions(&self) -> Vec<String> {
        self.inner
            .take_changed()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    pub fn stop(&self) {
        self.inner.stop();
    }
}

/// The paired devices in `state_dir`.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn remote_devices(state_dir: String) -> Result<Vec<RemoteDevice>, DsError> {
    guard(|| {
        let list = DeviceStore::new(Path::new(&state_dir))
            .list()
            .map_err(err)?;
        Ok(list
            .into_iter()
            .map(|d| RemoteDevice {
                short_fingerprint: Fingerprint::from_hex(&d.fingerprint)
                    .map(|f| f.short())
                    .unwrap_or_default(),
                fingerprint: d.fingerprint,
                name: d.name,
                paired_ms: d.paired_ms,
                write: d.write,
            })
            .collect())
    })
}

/// Forget a device; its next request is refused. Whether it was paired.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn remote_revoke(state_dir: String, fingerprint: String) -> Result<bool, DsError> {
    guard(|| {
        DeviceStore::new(Path::new(&state_dir))
            .revoke(&fingerprint)
            .map_err(err)
    })
}

/// Grant or withdraw write access. Whether the device is paired.
#[uniffi::export]
#[allow(clippy::needless_pass_by_value)] // UniFFI hands exports owned values
pub fn remote_set_write(
    state_dir: String,
    fingerprint: String,
    write: bool,
) -> Result<bool, DsError> {
    guard(|| {
        DeviceStore::new(Path::new(&state_dir))
            .set_write(&fingerprint, write)
            .map_err(err)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_remote::tls::Pin;
    use ds_remote::wire::Secret;
    use ds_remote::{Client, Identity};

    fn adj_json(ev: f32) -> String {
        let mut a = Adjustments::default();
        a.exposure.exposure_ev = ev;
        serde_json::to_string(&a).unwrap()
    }

    fn exposures(h: &SessionHandle) -> Vec<f32> {
        let mut s = lock(&h.session);
        s.sync_index().unwrap();
        let mut out = Vec::new();
        for v in s.all_variants().unwrap() {
            out.push(s.load_adjustments(v).unwrap().exposure.exposure_ev);
        }
        out
    }

    fn has(evs: &[f32], ev: f32) -> bool {
        evs.iter().any(|e| (e - ev).abs() < 1e-4)
    }

    /// The app keeps editing its open session while a device pushes an edit
    /// of the same photo: both edits survive (on the variant or kept as one).
    #[test]
    #[allow(clippy::too_many_lines)] // one scenario, start to finish
    fn app_edits_and_device_pushes_never_lose_either_side() {
        use futures_executor::block_on;
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("S");
        let v = {
            let mut s = Session::create(&dir, "S").unwrap();
            std::fs::write(dir.join("Capture/A.ARW"), b"raw").unwrap();
            let id = s
                .add_original(
                    "Capture/A.ARW",
                    ds_core::FileHash(*blake3::hash(b"raw").as_bytes()),
                    ds_session::ImageMetaLite::default(),
                )
                .unwrap();
            s.variants(id).unwrap()[0]
        };
        let h = block_on(crate::session::open_session(
            dir.to_string_lossy().into_owned(),
            crate::jobs::CancelToken::new(),
        ))
        .unwrap();
        let state = t.path().join("Remote").to_string_lossy().into_owned();
        let server = RemoteServer::start_at(&state, "Mac", [127, 0, 0, 1].into(), 0).unwrap();
        server.set_open_session(Some(h.clone()), "S".into());
        let p = server.begin_pairing("localhost".into());
        let id = Identity::generate().unwrap();
        let addr = SocketAddr::from(([127, 0, 0, 1], server.port()));
        let q = ds_remote::pairing::QrPayload::parse(&p.qr_payload).unwrap();
        let mut c = Client::connect(addr, &id, Pin::Key(q.fingerprint), "Test iPad").unwrap();
        c.pair(&q.token, Secret::Token).unwrap();
        assert!(remote_set_write(state.clone(), id.fingerprint().to_hex(), true).unwrap());
        let mut c = Client::connect(addr, &id, Pin::Key(q.fingerprint), "Test iPad").unwrap();
        let session = c.sessions().unwrap()[0].id.clone();
        let file = ds_remote::FileRef::Sidecar {
            rel: "Capture/A.ARW".into(),
            ordinal: 1,
        };
        let base = c.fetch(&session, &file).unwrap();
        let mut edited = ds_session::Sidecar::from_json(&base).unwrap().sidecar;
        edited.adjustments.exposure.exposure_ev = 1.5;

        let app = {
            let h = h.clone();
            std::thread::spawn(move || {
                for i in 0..20 {
                    let json = adj_json(0.1 + i as f32 * 0.01);
                    block_on(h.clone().save_adjustments_json(
                        v.0,
                        json,
                        crate::jobs::CancelToken::new(),
                    ))
                    .unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            })
        };
        let r = c
            .push_sidecar(
                &session,
                "Capture/A.ARW",
                1,
                Some(blake3::hash(&base).to_hex().to_string()),
                Some(String::from_utf8(base.clone()).unwrap()),
                serde_json::from_slice(&edited.to_json()).unwrap(),
                i64::MAX,
            )
            .unwrap();
        app.join().unwrap();
        assert_ne!(r.outcome, ds_remote::MergeOutcome::Unchanged);
        let evs = exposures(&h);
        assert!(has(&evs, 1.5), "the device's edit survives: {evs:?}");
        assert!(has(&evs, 0.29), "the app's last edit survives: {evs:?}");
        assert_eq!(server.take_changed_sessions().len(), 1);

        let push = |c: &mut Client, ev: f32| {
            let cur = c.fetch(&session, &file).unwrap();
            let mut sc = ds_session::Sidecar::from_json(&cur).unwrap().sidecar;
            sc.adjustments.exposure.exposure_ev = ev;
            c.push_sidecar(
                &session,
                "Capture/A.ARW",
                1,
                Some(blake3::hash(&cur).to_hex().to_string()),
                Some(String::from_utf8(cur).unwrap()),
                serde_json::from_slice(&sc.to_json()).unwrap(),
                i64::MAX,
            )
            .unwrap();
        };
        let save = |ev: f32| {
            block_on(h.clone().save_adjustments_json(
                v.0,
                adj_json(ev),
                crate::jobs::CancelToken::new(),
            ))
            .unwrap();
        };
        // The store reading the edits is not the app adopting them: a save
        // queued during a reload still keeps the device's edit.
        let n = evs.len();
        push(&mut c, -1.0);
        block_on(
            h.clone()
                .load_all_adjustments_json(crate::jobs::CancelToken::new()),
        )
        .unwrap();
        save(0.5);
        let evs = exposures(&h);
        assert_eq!(evs.len(), n + 1);
        assert!(has(&evs, -1.0), "{evs:?}");
        // Once the app has them in its model, its next save is an ordinary edit.
        push(&mut c, -2.0);
        let gen_ = block_on(
            h.clone()
                .load_all_adjustments_json(crate::jobs::CancelToken::new()),
        )
        .unwrap()
        .device_generation;
        remote_mark_seen(h.clone(), gen_);
        save(0.55);
        assert_eq!(exposures(&h).len(), n + 1, "no extra variant once seen");
        // A push landing after the app's snapshot isn't adopted with it: the
        // app's next save still keeps it.
        let snapshot = block_on(
            h.clone()
                .load_all_adjustments_json(crate::jobs::CancelToken::new()),
        )
        .unwrap()
        .device_generation;
        push(&mut c, -3.0);
        remote_mark_seen(h.clone(), snapshot);
        save(0.56);
        let evs = exposures(&h);
        assert_eq!(evs.len(), n + 2, "{evs:?}");
        assert!(
            has(&evs, -3.0),
            "the late push survives as a variant: {evs:?}"
        );
        // A batch save over an unseen device edit keeps it too.
        push(&mut c, 2.0);
        let batch = vec![crate::session::AdjustmentsJson {
            variant_id: v.0,
            json: adj_json(0.6),
        }];
        block_on(
            h.clone()
                .apply_batch_json(batch, crate::jobs::CancelToken::new()),
        )
        .unwrap();
        let evs = exposures(&h);
        assert!(has(&evs, 2.0) && has(&evs, 0.6), "{evs:?}");
        server.set_open_session(None, String::new());
        assert_eq!(c.sessions().unwrap().len(), 0);
    }

    #[test]
    fn server_pairs_lists_grants_and_revokes() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("Remote").to_string_lossy().into_owned();
        assert_eq!(remote_default_port(), 47_474);
        let lo: IpAddr = [127, 0, 0, 1].into();
        let s = RemoteServer::start_at(&state, "Test Mac", lo, 0).unwrap();
        let v6 = RemoteServer::start(state.clone(), "Test Mac".into(), 0, false).unwrap();
        assert!(v6.port() > 0, "loopback-only start");
        v6.stop();
        assert!(s.port() > 0);
        assert_eq!(s.fingerprint().len(), 19);
        let session = t.path().join("S");
        ds_session::Session::create(&session, "S").unwrap();
        s.set_sessions(vec![RemoteSession {
            path: session.to_string_lossy().into_owned(),
            name: "S".into(),
        }]);
        assert!(!s.pairing_open());
        let p = s.begin_pairing("localhost".into());
        assert!(s.pairing_open());
        assert_eq!(p.code.len(), 6);
        assert_eq!(p.expires_in_secs, 300);
        assert!(p.qr_payload.contains(&format!("localhost:{}", s.port())));
        let q = ds_remote::pairing::QrPayload::parse(&p.qr_payload).unwrap();

        let id = Identity::generate().unwrap();
        let addr = SocketAddr::from(([127, 0, 0, 1], s.port()));
        let mut c = Client::connect(addr, &id, Pin::Key(q.fingerprint), "Test iPad").unwrap();
        c.pair(&q.token, Secret::Token).unwrap();
        assert_eq!(c.sessions().unwrap().len(), 1);
        assert_eq!(s.status().connections, 1);
        assert_eq!(s.take_changed_sessions().len(), 0);

        let devs = remote_devices(state.clone()).unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!((devs[0].name.as_str(), devs[0].write), ("Test iPad", false));
        assert!(
            devs[0]
                .fingerprint
                .starts_with(&devs[0].short_fingerprint.replace(' ', ""))
        );
        assert!(remote_set_write(state.clone(), devs[0].fingerprint.clone(), true).unwrap());
        assert!(remote_devices(state.clone()).unwrap()[0].write);
        assert!(!remote_set_write(state.clone(), "00".into(), true).unwrap());
        assert!(remote_revoke(state.clone(), devs[0].fingerprint.clone()).unwrap());
        assert!(c.sessions().is_err(), "revoked at once");
        s.cancel_pairing();
        s.stop();

        // A busy port falls back to any free one.
        let other = RemoteServer::start_at(&state, "Mac", lo, 0).unwrap();
        let again = RemoteServer::start_at(&state, "Mac", lo, other.port()).unwrap();
        assert_ne!(again.port(), other.port());
        assert_eq!(format!("{again:?}"), "RemoteServer { .. }");

        // A damaged device list is an error, never "no devices".
        std::fs::write(t.path().join("Remote/devices.json"), b"{").unwrap();
        assert!(matches!(
            remote_devices(state.clone()),
            Err(DsError::Io { .. })
        ));
        assert!(matches!(
            err(ds_remote::RemoteError::Io(std::io::Error::other("x"))),
            DsError::Io { .. }
        ));
    }
}
