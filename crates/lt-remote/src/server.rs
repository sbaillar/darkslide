//! The link server: one listener, a thread per connection, the session
//! handlers. Only runs between [`Server::start`] and [`Server::stop`] (or
//! drop); the app starts it when "Allow iPad connections" is on.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::IpAddr;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, UNIX_EPOCH};

use lt_core::{FileHash, IptcMeta};
use lt_session::{ImageMetaLite, NewOriginal, RelPath, Session, SessionError, Sidecar, VariantId};
use rustls::{ServerConnection, StreamOwned};

use crate::identity::{Fingerprint, Identity};
use crate::merge::{self, Edit};
use crate::net::{self, PeerPolicy, private_peers};
use crate::pairing::{self, Device, DeviceStore, Exchange, PairingTicket, Window};
use crate::transfer::{self, Incoming};
use crate::wire::{
    self, CHUNK, ErrorCode, FileRef, PROTOCOL_VERSION, PhotoInfo, Request, Response, SessionInfo,
    VariantEntry,
};
use crate::{RemoteError, Result, tls};

/// Connections past the TLS handshake served at once; more are closed.
pub const MAX_CONNECTIONS: usize = 8;
/// Handshakes in progress at once, and from one address (an IPv6 /64 counts
/// as one, see [`crate::net::prefix`]): kept apart from the authenticated
/// slots so silent sockets can't lock a paired device out.
pub const MAX_PENDING: usize = 8;
pub const MAX_PENDING_PER_IP: usize = 2;
/// Connections of unpaired keys (let in while a pairing window is open, to
/// pair), at once and from one address: apart from the paired slots, and
/// closed when the window closes or [`UNPAIRED_DEADLINE`] passes.
pub const MAX_UNPAIRED: usize = 2;
pub const MAX_UNPAIRED_PER_IP: usize = 1;
/// An unpaired connection must pair within this of its handshake, however
/// many requests it makes.
pub const UNPAIRED_DEADLINE: Duration = Duration::from_secs(30);
/// Unfinished pushes one device may keep on the Mac, in bytes.
pub const MAX_PARTIAL_BYTES: u64 = 2 * MAX_PUSH_BYTES;
/// Handshakes dropped to make room for newcomers: a burst of this many, then
/// this many a second, so a flood can't keep dropping a real handshake
/// before it finishes (past the budget, the newcomer is turned away).
const EVICT_BURST: f64 = 8.0;
const EVICT_PER_SEC: f64 = 2.0;
/// A TLS handshake must finish within this, however slowly bytes trickle in.
pub const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(10);
/// A connection without a complete request for this long is closed.
pub const IDLE: Duration = Duration::from_secs(120);
/// Largest original a device may push.
pub const MAX_PUSH_BYTES: u64 = 4 << 30;
/// A pushed file's `.partial` untouched this long was abandoned (removed when
/// the session is served again).
const STALE_PARTIAL: Duration = Duration::from_secs(24 * 3600);
/// Per-connection file hash memo: cleared when it grows past this.
const MAX_MEMO: usize = 4096;
/// Refused peers are logged at most this often.
const REFUSED_LOG_EVERY: Duration = Duration::from_secs(60);
/// "Transferring" stays on this long after the last chunk.
const ACTIVITY: Duration = Duration::from_secs(2);
/// Photos in a `ListPhotos` page when the device doesn't say, and the most it
/// may ask for.
const PAGE_DEFAULT: usize = 200;
const PAGE_MAX: usize = 500;
/// A page's photos stop at about this many bytes of header (well under
/// [`wire::MAX_HEADER`], so a photo with many variants can't overflow it).
const PAGE_BYTES: usize = 512 << 10;
/// Longest device name kept from `Hello`.
const MAX_DEVICE_NAME: usize = 64;
/// Sidecar hash memo: cleared when it grows past this.
const MAX_SIDECAR_MEMO: usize = 100_000;
/// Pushed originals go to `Capture/From <device>/`.
const PUSH_FOLDER: &str = "Capture";

/// A session the server may serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoot {
    pub path: PathBuf,
    pub name: String,
}

impl SessionRoot {
    /// Stable id: BLAKE3 of the root path, 16 hex digits.
    pub fn id(&self) -> String {
        blake3::hash(self.path.as_os_str().as_encoded_bytes()).to_hex()[..16].to_owned()
    }
}

/// Where the server gets the session it reads and writes.
///
/// The app passes its own open session, so a device's writes and the app's
/// own are serialized on one lock and one `Session`; elsewhere (the CLI,
/// tests) [`OwnSessions`] opens each session once for the whole server.
pub trait SessionAccess: Send + Sync {
    /// Run `f` with the session at `root`, holding its lock for the call.
    fn with_session(&self, root: &Path, f: &mut dyn FnMut(&mut Session)) -> Result<()>;

    /// A device changed these variants' sidecars (the app keeps an edit of
    /// its own that hadn't seen them; see `lt-ffi`'s `remote`). Called inside
    /// [`SessionAccess::with_session`], with the session's lock held.
    fn device_wrote(&self, _root: &Path, _variants: &[VariantId], _device: &str) {}
}

/// [`SessionAccess`] that opens each session itself, once.
#[derive(Default)]
pub struct OwnSessions {
    open: Mutex<HashMap<PathBuf, Arc<Mutex<Session>>>>,
}

impl std::fmt::Debug for OwnSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnSessions").finish_non_exhaustive()
    }
}

impl SessionAccess for OwnSessions {
    fn with_session(&self, root: &Path, f: &mut dyn FnMut(&mut Session)) -> Result<()> {
        let s = {
            let mut open = lock(&self.open);
            if let Some(s) = open.get(root) {
                s.clone()
            } else {
                let s = Arc::new(Mutex::new(Session::open(root)?));
                open.insert(root.to_owned(), s.clone());
                s
            }
        };
        f(&mut lock(&s));
        Ok(())
    }
}

/// Previews and metadata of originals: the seam over `lt-raw`.
pub trait Media: Send + Sync {
    /// The preview JPEG of an original.
    fn preview(&self, original: &Path) -> io::Result<Vec<u8>>;
    /// Metadata for a newly pushed original.
    fn meta(&self, original: &Path) -> ImageMetaLite;
}

/// [`Media`] from the RAW's embedded preview and EXIF (`lt-raw`).
#[derive(Debug, Clone, Copy, Default)]
pub struct RawMedia;

impl Media for RawMedia {
    fn preview(&self, original: &Path) -> io::Result<Vec<u8>> {
        lt_raw::embedded_preview(original)
            .map(|p| p.jpeg)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    fn meta(&self, original: &Path) -> ImageMetaLite {
        let Ok(m) = lt_raw::read_metadata(original) else {
            return ImageMetaLite::default();
        };
        let size = m.dimensions.unwrap_or(lt_core::Size::new(0, 0));
        ImageMetaLite {
            capture_time_ms: m.capture_time.as_ref().map(|t| {
                let days = lt_core::date::days_from_civil(
                    i64::from(t.year),
                    i64::from(t.month),
                    i64::from(t.day),
                );
                let secs = days * 86_400
                    + i64::from(t.hour) * 3600
                    + i64::from(t.minute) * 60
                    + i64::from(t.second);
                secs * 1000 + i64::from(t.nanos / 1_000_000)
            }),
            camera: Some(format!("{} {}", m.make, m.model).trim().to_owned())
                .filter(|s| !s.is_empty()),
            lens: m.lens.clone(),
            iso: m.iso,
            focal_length_mm: m.focal_length,
            width: size.width,
            height: size.height,
            orientation: m.orientation,
            as_shot_wb: None,
        }
    }
}

/// A test's fault for one read before it is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFault {
    None,
    /// Flip a byte of the chunk (a corrupted transfer).
    Corrupt,
    /// Close the connection instead of answering (a dropped link).
    Drop,
}

/// Called with each read's file and offset.
pub type ReadHook = Arc<dyn Fn(&FileRef, u64) -> ReadFault + Send + Sync>;

pub struct ServerConfig {
    /// Shown to clients (the Mac's name).
    pub name: String,
    /// Identity, paired devices.
    pub state_dir: PathBuf,
    pub bind: SocketAddr,
    pub policy: PeerPolicy,
    pub media: Arc<dyn Media>,
    pub read_hook: Option<ReadHook>,
    pub sessions: Arc<dyn SessionAccess>,
    /// [`HANDSHAKE_DEADLINE`]; tests shorten it.
    pub handshake_deadline: Duration,
    /// [`IDLE`]; tests shorten it.
    pub idle: Duration,
    /// [`UNPAIRED_DEADLINE`]; tests shorten it.
    pub unpaired_deadline: Duration,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("name", &self.name)
            .field("bind", &self.bind)
            .finish_non_exhaustive()
    }
}

impl ServerConfig {
    pub fn new(name: &str, state_dir: &Path, bind: SocketAddr) -> Self {
        Self {
            name: name.to_owned(),
            state_dir: state_dir.to_owned(),
            bind,
            policy: private_peers(),
            media: Arc::new(RawMedia),
            read_hook: None,
            sessions: Arc::new(OwnSessions::default()),
            handshake_deadline: HANDSHAKE_DEADLINE,
            idle: IDLE,
            unpaired_deadline: UNPAIRED_DEADLINE,
        }
    }
}

/// What the app's status indicator shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerStatus {
    pub connections: u32,
    /// A chunk moved in the last two seconds.
    pub transferring: bool,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    /// Connections dropped by the address filter or the handshake limits.
    pub refused_peers: u64,
    /// TLS handshakes in progress.
    pub handshaking: u32,
}

/// Where a connection is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// TLS handshake in progress.
    Handshake,
    /// An unpaired key let in while pairing is open: it may only pair.
    Unpaired,
    Paired,
}

/// A live socket: its peer, its stage, and when it last made progress (the
/// deadlines are enforced from it; an unpaired one's stays at its handshake).
struct Slot {
    sock: TcpStream,
    /// The peer's [`net::prefix`].
    ip: IpAddr,
    stage: Stage,
    since: Instant,
    /// A request is being handled: the idle sweep leaves it alone.
    busy: bool,
}

/// Removes a connection's slot when its thread ends, however it ends.
struct SlotGuard {
    shared: Arc<Shared>,
    id: u64,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        lock(&self.shared.conns).remove(&self.id);
    }
}

struct Shared {
    name: String,
    identity: Identity,
    devices: Arc<DeviceStore>,
    window: Arc<Mutex<Option<Window>>>,
    sessions: RwLock<Vec<SessionRoot>>,
    tls: Arc<rustls::ServerConfig>,
    policy: PeerPolicy,
    media: Arc<dyn Media>,
    read_hook: Option<ReadHook>,
    access: Arc<dyn SessionAccess>,
    handshake_deadline: Duration,
    idle: Duration,
    unpaired_deadline: Duration,
    stop: AtomicBool,
    conns: Mutex<HashMap<u64, Slot>>,
    last_refused_log: Mutex<Option<Instant>>,
    /// Locked inside `conns`.
    evictions: Mutex<Bucket>,
    next_conn: AtomicU64,
    sent: AtomicU64,
    received: AtomicU64,
    refused: AtomicU64,
    last_transfer: Mutex<Option<Instant>>,
    changed: Mutex<BTreeSet<PathBuf>>,
    /// Sidecar hashes by path, valid while the file's size and mtime hold.
    sidecar_hashes: Mutex<SidecarMemo>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A running server.
pub struct Server {
    shared: Arc<Shared>,
    addr: SocketAddr,
    accept: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Bind and start serving. Nothing listens until this is called.
    pub fn start(cfg: ServerConfig) -> Result<Self> {
        let identity = Identity::load_or_create(&cfg.state_dir)?;
        let devices = Arc::new(DeviceStore::new(&cfg.state_dir));
        let window: Arc<Mutex<Option<Window>>> = Arc::new(Mutex::new(None));
        let gate: tls::ClientGate = {
            let (devices, window) = (devices.clone(), window.clone());
            Arc::new(move |fp: &Fingerprint| {
                matches!(devices.find(fp), Ok(Some(_)))
                    || lock(&window).as_ref().is_some_and(Window::is_open)
            })
        };
        let tls = tls::server_config(&identity, gate)?;
        let listener = TcpListener::bind(cfg.bind)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            name: cfg.name,
            identity,
            devices,
            window,
            sessions: RwLock::new(Vec::new()),
            tls,
            policy: cfg.policy,
            media: cfg.media,
            read_hook: cfg.read_hook,
            access: cfg.sessions,
            handshake_deadline: cfg.handshake_deadline,
            idle: cfg.idle,
            unpaired_deadline: cfg.unpaired_deadline,
            stop: AtomicBool::new(false),
            conns: Mutex::new(HashMap::new()),
            last_refused_log: Mutex::new(None),
            evictions: Mutex::new(Bucket::full(Instant::now())),
            next_conn: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            last_transfer: Mutex::new(None),
            changed: Mutex::new(BTreeSet::new()),
            sidecar_hashes: Mutex::new(HashMap::new()),
        });
        let s = shared.clone();
        let accept = std::thread::Builder::new()
            .name("lt-remote accept".into())
            .spawn(move || accept_loop(&s, &listener))?;
        Ok(Self {
            shared,
            addr,
            accept: Mutex::new(Some(accept)),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.shared.identity.fingerprint()
    }

    pub fn devices(&self) -> &DeviceStore {
        &self.shared.devices
    }

    /// The sessions clients may list and use. Push partials abandoned in
    /// them for a day are removed.
    pub fn set_sessions(&self, sessions: Vec<SessionRoot>) {
        for r in &sessions {
            remove_stale_partials(&r.path.join(PUSH_FOLDER), STALE_PARTIAL);
        }
        *self.shared.sessions.write().expect("sessions lock") = sessions;
    }

    /// Open a pairing window (replacing any open one).
    pub fn begin_pairing(&self) -> PairingTicket {
        self.begin_pairing_for(pairing::PAIRING_TTL)
    }

    pub fn begin_pairing_for(&self, ttl: Duration) -> PairingTicket {
        let (w, ticket) = Window::open(ttl);
        *lock(&self.shared.window) = Some(w);
        ticket
    }

    pub fn cancel_pairing(&self) {
        *lock(&self.shared.window) = None;
    }

    pub fn pairing_open(&self) -> bool {
        lock(&self.shared.window)
            .as_ref()
            .is_some_and(Window::is_open)
    }

    pub fn status(&self) -> ServerStatus {
        let s = &self.shared;
        // One lock of `conns` for both counts (two in one expression deadlock).
        let (authed, handshaking) = lock(&s.conns).values().fold((0, 0), |(a, h), c| {
            if c.stage == Stage::Handshake {
                (a, h + 1)
            } else {
                (a + 1, h)
            }
        });
        ServerStatus {
            connections: authed,
            transferring: lock(&s.last_transfer).is_some_and(|t| t.elapsed() < ACTIVITY),
            bytes_sent: s.sent.load(Ordering::Relaxed),
            bytes_received: s.received.load(Ordering::Relaxed),
            refused_peers: s.refused.load(Ordering::Relaxed),
            handshaking,
        }
    }

    /// Session roots whose files or sidecars a client changed since the
    /// last call (the app reloads them).
    pub fn take_changed(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *lock(&self.shared.changed))
            .into_iter()
            .collect()
    }

    /// Stop listening and close every connection.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for c in lock(&self.shared.conns).values() {
            let _ = c.sock.shutdown(Shutdown::Both);
        }
        if let Some(h) = lock(&self.accept).take() {
            let _ = h.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(shared: &Arc<Shared>, listener: &TcpListener) {
    while !shared.stop.load(Ordering::SeqCst) {
        sweep(shared);
        match listener.accept() {
            Ok((sock, peer)) => admit(shared, sock, peer),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                tracing::warn!("link: accept: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Enforce the deadlines: a handshake must finish within
/// [`HANDSHAKE_DEADLINE`], an unpaired connection lasts only as long as the
/// pairing window and at most [`UNPAIRED_DEADLINE`], and a paired one must
/// make a request within [`IDLE`]. Absolute, so trickled bytes don't extend
/// them.
fn sweep(shared: &Shared) {
    let pairing = lock(&shared.window).as_ref().is_some_and(Window::is_open);
    for c in lock(&shared.conns).values() {
        let expired = match c.stage {
            Stage::Handshake => c.since.elapsed() > shared.handshake_deadline,
            Stage::Unpaired => !pairing || c.since.elapsed() > shared.unpaired_deadline,
            Stage::Paired => !c.busy && c.since.elapsed() > shared.idle,
        };
        if expired {
            let _ = c.sock.shutdown(Shutdown::Both);
        }
    }
}

fn refuse(shared: &Shared, peer: SocketAddr, why: &str) {
    let n = shared.refused.fetch_add(1, Ordering::Relaxed) + 1;
    let mut last = lock(&shared.last_refused_log);
    if last.is_none_or(|t| t.elapsed() >= REFUSED_LOG_EVERY) {
        *last = Some(Instant::now());
        tracing::info!(%peer, refused = n, "link: refused a peer ({why})");
    }
}

/// Check the address, take a slot (on this thread, so a burst of connects
/// can't overshoot), and serve on a new thread. When the handshake limits
/// are reached a pending handshake is dropped instead of the newcomer (see
/// [`victim`]), within the [`EVICT_BURST`] budget: a flood can delay a real
/// handshake but not shut it out.
fn admit(shared: &Arc<Shared>, sock: TcpStream, peer: SocketAddr) {
    if !(shared.policy)(peer) {
        return refuse(shared, peer, "outside the private ranges");
    }
    let from = net::prefix(peer.ip());
    let id = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let admitted = {
        let mut conns = lock(&shared.conns);
        let pending: Vec<_> = conns
            .iter()
            .filter(|(_, c)| c.stage == Stage::Handshake)
            .map(|(k, c)| (*k, c.ip, c.since))
            .collect();
        let victim = victim(&pending, from);
        if victim.is_some() && !lock(&shared.evictions).take(Instant::now()) {
            None
        } else {
            if let Some(old) = victim.and_then(|k| conns.remove(&k)) {
                let _ = old.sock.shutdown(Shutdown::Both);
            }
            let Ok(clone) = sock.try_clone() else { return };
            conns.insert(
                id,
                Slot {
                    sock: clone,
                    ip: from,
                    stage: Stage::Handshake,
                    since: Instant::now(),
                    busy: false,
                },
            );
            Some(victim.is_some())
        }
    };
    match admitted {
        None => return refuse(shared, peer, "too many handshakes"),
        Some(true) => refuse(shared, peer, "too many handshakes; dropped one"),
        Some(false) => {}
    }
    let s = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("lt-remote conn".into())
        .spawn(move || {
            let _slot = SlotGuard {
                shared: s.clone(),
                id,
            };
            if let Err(e) = serve(&s, sock, id) {
                tracing::debug!(%peer, "link: connection ended: {e}");
            }
        });
    if let Err(e) = spawned {
        lock(&shared.conns).remove(&id);
        tracing::warn!("link: no thread for a connection: {e}");
    }
}

fn serve(shared: &Arc<Shared>, sock: TcpStream, id: u64) -> Result<()> {
    sock.set_nonblocking(false)?;
    sock.set_read_timeout(Some(shared.handshake_deadline))?;
    sock.set_write_timeout(Some(shared.handshake_deadline))?;
    if shared.stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut tls = StreamOwned::new(ServerConnection::new(shared.tls.clone())?, sock);
    while tls.conn.is_handshaking() {
        tls.conn.complete_io(&mut tls.sock)?;
    }
    let peer = tls
        .conn
        .peer_certificates()
        .and_then(|c| c.first())
        .and_then(|c| Fingerprint::of_cert(c))
        .ok_or_else(|| RemoteError::Protocol("no client key".into()))?;
    let stage = if matches!(shared.devices.find(&peer), Ok(Some(_))) {
        Stage::Paired
    } else {
        Stage::Unpaired
    };
    {
        let mut conns = lock(&shared.conns);
        let Some(ip) = conns.get(&id).map(|c| c.ip) else {
            return Ok(()); // stopped meanwhile
        };
        let (cap, per_ip) = if stage == Stage::Paired {
            (MAX_CONNECTIONS, MAX_CONNECTIONS)
        } else {
            (MAX_UNPAIRED, MAX_UNPAIRED_PER_IP)
        };
        let same = conns.values().filter(|c| c.stage == stage);
        if same.clone().count() >= cap || same.filter(|c| c.ip == ip).count() >= per_ip {
            return Err(RemoteError::Protocol("too many connections".into()));
        }
        let Some(slot) = conns.get_mut(&id) else {
            return Ok(());
        };
        slot.stage = stage;
        slot.since = Instant::now();
    }
    tls.sock.set_read_timeout(Some(shared.idle))?;
    tls.sock.set_write_timeout(Some(shared.idle))?;
    let mut conn = Conn {
        shared: shared.clone(),
        id,
        peer,
        name: None,
        exchange: None,
        pair_started: false,
        memo: HashMap::new(),
        bytes: None,
    };
    loop {
        let (req, body) = match wire::recv::<Request>(&mut tls) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        };
        // An unpaired connection's deadline runs from its handshake.
        if let Some(slot) = lock(&shared.conns).get_mut(&id) {
            if slot.stage != Stage::Unpaired {
                slot.since = Instant::now();
            }
            slot.busy = true;
        }
        // A panic in a handler (the RAW parser on a hostile file) ends this
        // connection with an error, not the thread's slot.
        let reply = lt_fsutil::catch_panic(|| conn.handle(req, &body)).unwrap_or_else(|why| {
            tracing::warn!("link: a request panicked: {why}");
            Reply::refusal(ErrorCode::Internal, "internal error", true)
        });
        if reply.drop {
            let _ = tls.sock.shutdown(Shutdown::Both);
            return Ok(());
        }
        if let Err(e) = wire::send(&mut tls, &reply.response, &reply.body) {
            if e.kind() != io::ErrorKind::InvalidData {
                return Err(e.into());
            }
            // Too big to frame: say so rather than hang up silently.
            let r = Reply::refusal(ErrorCode::Internal, e.to_string(), true);
            wire::send(&mut tls, &r.response, &r.body)?;
            break;
        }
        // The idle clock starts again once the answer is out (a long request
        // isn't cut off mid-reply).
        if let Some(slot) = lock(&shared.conns).get_mut(&id) {
            if slot.stage != Stage::Unpaired {
                slot.since = Instant::now();
            }
            slot.busy = false;
        }
        if reply.close {
            break;
        }
    }
    tls.conn.send_close_notify();
    let _ = tls.flush();
    Ok(())
}

struct Reply {
    response: Response,
    body: Vec<u8>,
    close: bool,
    drop: bool,
}

impl Reply {
    fn ok(response: Response) -> Self {
        Self::with_body(response, Vec::new())
    }

    fn refusal(code: ErrorCode, message: impl Into<String>, close: bool) -> Self {
        Self {
            response: Response::Error {
                code,
                message: message.into(),
            },
            body: Vec::new(),
            close,
            drop: false,
        }
    }

    fn with_body(response: Response, body: Vec<u8>) -> Self {
        Self {
            response,
            body,
            close: false,
            drop: false,
        }
    }
}

/// A refusal: the error sent back, and whether to hang up after it.
#[derive(Debug)]
struct Fail {
    code: ErrorCode,
    message: String,
    close: bool,
}

fn fail(code: ErrorCode, message: impl Into<String>) -> Fail {
    Fail {
        code,
        message: message.into(),
        close: false,
    }
}

impl From<SessionError> for Fail {
    fn from(e: SessionError) -> Self {
        let code = match e {
            SessionError::InvalidPath { .. } => ErrorCode::BadRequest,
            SessionError::NotFound { .. } => ErrorCode::NotFound,
            _ => ErrorCode::Internal,
        };
        fail(code, e.to_string())
    }
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        let code = match e.kind() {
            io::ErrorKind::NotFound => ErrorCode::NotFound,
            io::ErrorKind::InvalidInput => ErrorCode::BadRequest,
            _ => ErrorCode::Internal,
        };
        fail(code, e.to_string())
    }
}

impl From<RemoteError> for Fail {
    fn from(e: RemoteError) -> Self {
        match e {
            RemoteError::Session(s) => s.into(),
            RemoteError::Io(io) => io.into(),
            e => fail(ErrorCode::Internal, e.to_string()),
        }
    }
}

type Handled = std::result::Result<Reply, Fail>;

/// A file as it is now: where it is, and its size and hash (memoized per
/// change of the file's size and modification time).
#[derive(Clone)]
struct Current {
    stamp: (u64, i128),
    path: PathBuf,
    size: u64,
    hash: FileHash,
}

type FileKey = (String, FileRef);
/// A file's bytes as of its (size, modification time).
type Held = (FileKey, (u64, i128), Arc<Vec<u8>>);
/// Sidecar path → (size and mtime when hashed, BLAKE3).
type SidecarMemo = HashMap<PathBuf, ((u64, i128), String)>;

struct Conn {
    shared: Arc<Shared>,
    /// This connection's slot.
    id: u64,
    peer: Fingerprint,
    /// From `Hello`; `None` until then.
    name: Option<String>,
    /// The SPAKE2 key and the pairing window it was made in.
    exchange: Option<([u8; 32], u64)>,
    /// One pairing attempt per connection (an unpaired one can't hold its
    /// slot for the whole window by starting over).
    pair_started: bool,
    memo: HashMap<FileKey, Current>,
    /// The bytes of the last preview or sidecar served, for its next chunk
    /// (only one is kept: a pull of thousands of previews stays small).
    bytes: Option<Held>,
}

impl Conn {
    fn handle(&mut self, req: Request, body: &[u8]) -> Reply {
        match self.dispatch(req, body) {
            Ok(r) => r,
            Err(f) => Reply::refusal(f.code, f.message, f.close),
        }
    }

    fn dispatch(&mut self, req: Request, body: &[u8]) -> Handled {
        if let Request::Hello { version, name } = req {
            return self.hello(version, &name);
        }
        if self.name.is_none() {
            return Err(Fail {
                close: true,
                ..fail(ErrorCode::BadRequest, "say hello first")
            });
        }
        match req {
            Request::PairStart { secret, spake } => self.pair_start(secret, &spake),
            Request::PairFinish { confirm } => self.pair_finish(&confirm),
            req => {
                let device = self.device()?;
                self.paired(req, body, &device)
            }
        }
    }

    fn paired(&mut self, req: Request, body: &[u8], device: &Device) -> Handled {
        let write = || {
            if device.write {
                Ok(())
            } else {
                Err(fail(
                    ErrorCode::ReadOnly,
                    "this device is read-only; grant write access in Settings ▸ Devices",
                ))
            }
        };
        match req {
            Request::ListSessions => Ok(self.list_sessions()),
            Request::ListPhotos {
                session,
                after,
                limit,
            } => self.list_photos(&session, after.as_deref(), limit),
            Request::Stat { session, file } => {
                let c = self.current(&session, &file)?;
                Ok(Reply::ok(Response::File {
                    size: c.size,
                    hash: c.hash.to_hex(),
                }))
            }
            Request::Read {
                session,
                file,
                hash,
                offset,
                len,
            } => self.read(&session, &file, &hash, offset, len),
            Request::PushStatus {
                session,
                name,
                size,
                hash,
            } => {
                write()?;
                self.push(&session, &name, size, &hash, &device.name, None)
            }
            Request::PushChunk {
                session,
                name,
                size,
                hash,
                offset,
            } => {
                write()?;
                self.push(
                    &session,
                    &name,
                    size,
                    &hash,
                    &device.name,
                    Some((offset, body)),
                )
            }
            Request::PushSidecar {
                session,
                rel,
                ordinal,
                based_on,
                base,
                sidecar,
                modified_ms,
            } => {
                write()?;
                let push = SidecarPush {
                    based_on,
                    base,
                    sidecar,
                    modified_ms,
                };
                self.push_sidecar(&session, &rel, ordinal, &push, &device.name)
            }
            Request::Hello { .. } | Request::PairStart { .. } | Request::PairFinish { .. } => {
                Err(Fail {
                    close: true,
                    ..fail(ErrorCode::BadRequest, "not valid here")
                })
            }
        }
    }

    fn hello(&mut self, version: u32, name: &str) -> Handled {
        if version != PROTOCOL_VERSION {
            return Err(Fail {
                close: true,
                ..fail(
                    ErrorCode::Version,
                    format!("this Mac speaks link protocol {PROTOCOL_VERSION}, not {version}"),
                )
            });
        }
        if self.name.is_some() {
            // Once per connection: no keep-alive for a connection that may only pair.
            return Err(Fail {
                close: true,
                ..fail(ErrorCode::BadRequest, "hello once")
            });
        }
        let device = self.shared.devices.find(&self.peer)?;
        self.name = Some(device_name(name));
        Ok(Reply::ok(Response::Welcome {
            version: PROTOCOL_VERSION,
            name: self.shared.name.clone(),
            paired: device.is_some(),
            write: device.is_some_and(|d| d.write),
        }))
    }

    /// The paired device on the other end, read now: a revoke applies at once.
    fn device(&self) -> std::result::Result<Device, Fail> {
        self.shared.devices.find(&self.peer)?.ok_or(Fail {
            close: true,
            ..fail(
                ErrorCode::Unpaired,
                "this device is not paired with this Mac",
            )
        })
    }

    fn pair_start(&mut self, secret: wire::Secret, spake: &str) -> Handled {
        if std::mem::replace(&mut self.pair_started, true) {
            return Err(Fail {
                close: true,
                ..fail(ErrorCode::BadRequest, "one pairing attempt per connection")
            });
        }
        let w = lock(&self.shared.window);
        let Some(win) = w.as_ref().filter(|w| w.is_open()) else {
            return Err(Fail {
                close: true,
                ..fail(ErrorCode::PairingClosed, "pairing is not open on this Mac")
            });
        };
        let (ex, msg) = Exchange::start(win.secret(secret), false);
        // A malformed message is no guess at the secret (only a wrong
        // confirmation is), so it closes the connection without using up one
        // of the window's attempts.
        let key = ex.finish(spake).map_err(|e| Fail {
            close: true,
            ..fail(ErrorCode::PairingFailed, e.to_string())
        })?;
        self.exchange = Some((key, win.id()));
        Ok(Reply::ok(Response::PairChallenge { spake: msg }))
    }

    fn pair_finish(&mut self, got: &str) -> Handled {
        let (key, window) = self.exchange.take().ok_or_else(|| Fail {
            close: true,
            ..fail(ErrorCode::BadRequest, "pair_start first")
        })?;
        let server = self.shared.identity.fingerprint();
        let mut w = lock(&self.shared.window);
        // The window the exchange started in: a new code voids the old one.
        let Some(win) = w.as_mut().filter(|w| w.is_open() && w.id() == window) else {
            return Err(Fail {
                close: true,
                ..fail(ErrorCode::PairingClosed, "pairing is not open on this Mac")
            });
        };
        if !pairing::confirms_match(got, &pairing::confirm(&key, "client", &self.peer, &server)) {
            if !win.fail() {
                *w = None;
            }
            return Err(Fail {
                close: true,
                ..fail(ErrorCode::PairingFailed, "wrong code")
            });
        }
        // Leave the unpaired slots before the window closes (the sweeper
        // closes unpaired connections once it has). Lock order: window, conns.
        if let Some(slot) = lock(&self.shared.conns).get_mut(&self.id) {
            slot.stage = Stage::Paired;
        }
        *w = None;
        drop(w);
        let name = self.name.clone().unwrap_or_default();
        self.shared.devices.add(Device {
            fingerprint: self.peer.to_hex(),
            name: name.clone(),
            paired_ms: pairing::now_ms(),
            write: false,
        })?;
        tracing::info!(device = %name, "link: paired");
        Ok(Reply::ok(Response::Paired {
            confirm: pairing::confirm(&key, "server", &self.peer, &server),
            name: self.shared.name.clone(),
        }))
    }

    fn list_sessions(&self) -> Reply {
        let sessions = self
            .shared
            .sessions
            .read()
            .expect("sessions lock")
            .iter()
            .map(|r| SessionInfo {
                id: r.id(),
                name: r.name.clone(),
            })
            .collect();
        Reply::ok(Response::Sessions { sessions })
    }

    /// A served session's root; a session no longer offered is refused.
    fn root(&self, id: &str) -> std::result::Result<SessionRoot, Fail> {
        self.shared
            .sessions
            .read()
            .expect("sessions lock")
            .iter()
            .find(|r| r.id() == id)
            .cloned()
            .ok_or_else(|| fail(ErrorCode::NotFound, format!("no session {id}")))
    }

    /// Run `f` on session `id` under its lock (shared with the app's writes).
    fn with<T>(
        &self,
        id: &str,
        f: impl FnOnce(&SessionRoot, &mut Session) -> std::result::Result<T, Fail>,
    ) -> std::result::Result<T, Fail> {
        let root = self.root(id)?;
        let mut f = Some(f);
        let mut out = None;
        self.shared.access.with_session(&root.path, &mut |s| {
            if let Some(f) = f.take() {
                out = Some(f(&root, s));
            }
        })?;
        out.unwrap_or_else(|| Err(fail(ErrorCode::Internal, "the session was not available")))
    }

    /// One page of the session's photos and variants, after path `after`.
    /// Only the index is read under the session lock (the app keeps it
    /// current; no sync here); the sidecars are read and hashed after it is
    /// released, so a device listing in a loop can't hold up the app's own
    /// work. A page ends early at [`PAGE_BYTES`] of header, so the reply
    /// always fits a frame; `next` says where the following page starts.
    fn list_photos(&mut self, id: &str, after: Option<&str>, limit: u32) -> Handled {
        type Listed = (lt_session::ImageInfo, Vec<(u32, i64, String, PathBuf)>);
        let after = after.map(RelPath::new).transpose()?;
        let want = match limit as usize {
            0 => PAGE_DEFAULT,
            n => n.min(PAGE_MAX),
        };
        let listed: Vec<Listed> = self.with(id, |root, s| {
            let mut out = Vec::new();
            for img in s.images_after(after.as_ref(), want + 1)? {
                let mut variants = Vec::new();
                for v in s.variants(img.id)? {
                    let info = s.variant(v)?;
                    let sc = root.path.join(img.rel_path.sidecar_rel(info.ordinal));
                    variants.push((info.ordinal, info.id.0, info.name, sc));
                }
                out.push((img, variants));
            }
            Ok(out)
        })?;
        let mut more = listed.len() > want;
        let mut photos = Vec::new();
        let mut bytes = 0;
        for (img, variants) in listed.into_iter().take(want) {
            let photo = PhotoInfo {
                rel: img.rel_path.as_str().to_owned(),
                size: img.file_size,
                hash: (img.hash != lt_session::UNHASHED).then(|| img.hash.to_hex()),
                missing: img.missing,
                variants: variants
                    .into_iter()
                    .filter_map(|(ordinal, variant_id, name, sc)| {
                        let (sidecar_hash, modified_ms) = self.sidecar_stamp(&sc)?;
                        Some(VariantEntry {
                            ordinal,
                            variant_id,
                            name,
                            sidecar_hash,
                            modified_ms,
                        })
                    })
                    .collect(),
            };
            bytes += serde_json::to_vec(&photo).map_or(0, |v| v.len() + 1);
            if bytes > PAGE_BYTES && !photos.is_empty() {
                more = true;
                break;
            }
            photos.push(photo);
        }
        let next = more.then(|| photos.last().map(|p| p.rel.clone())).flatten();
        Ok(Reply::ok(Response::Photos { photos, next }))
    }

    /// A sidecar's BLAKE3 and modification time, hashed again only when its
    /// size or mtime changed. `None` (logged) when it can't be read.
    fn sidecar_stamp(&self, path: &Path) -> Option<(String, i64)> {
        let stamp = stat_stamp(path).ok()?;
        if let Some((s, h)) = lock(&self.shared.sidecar_hashes).get(path)
            && *s == stamp
        {
            return Some((h.clone(), mtime_ms(path)));
        }
        let hash = match fs::read(path) {
            Ok(b) => blake3::hash(&b).to_hex().to_string(),
            Err(e) => {
                tracing::debug!("link: sidecar {} unreadable: {e}", path.display());
                return None;
            }
        };
        let mut memo = lock(&self.shared.sidecar_hashes);
        if memo.len() >= MAX_SIDECAR_MEMO {
            memo.clear();
        }
        memo.insert(path.to_owned(), (stamp, hash.clone()));
        Some((hash, mtime_ms(path)))
    }

    /// Where a served file is: only for indexed photos.
    /// Only this takes the session lock; reading and hashing happen outside it.
    fn locate(&self, id: &str, file: &FileRef) -> std::result::Result<PathBuf, Fail> {
        let rel = RelPath::new(file.rel())?;
        self.with(id, |root, s| {
            if s.image_by_rel(&rel)?.is_none() {
                return Err(fail(
                    ErrorCode::NotFound,
                    format!("{rel:?} is not in the session"),
                ));
            }
            Ok(match file {
                FileRef::Sidecar { ordinal, .. } => root.path.join(rel.sidecar_rel(*ordinal)),
                FileRef::Original { .. } | FileRef::Preview { .. } => s.original_path(rel.as_str()),
            })
        })
    }

    /// The bytes of a preview or sidecar as they are now.
    fn load_bytes(&self, file: &FileRef, path: &Path) -> io::Result<Vec<u8>> {
        match file {
            FileRef::Preview { .. } => self.shared.media.preview(path),
            _ => fs::read(path),
        }
    }

    /// Resolve a file of a served photo, hashing it once per change.
    fn current(&mut self, id: &str, file: &FileRef) -> std::result::Result<Current, Fail> {
        let key = (id.to_owned(), file.clone());
        if let Some(c) = self.memo.get(&key)
            && stat_stamp(&c.path).ok() == Some(c.stamp)
        {
            return Ok(c.clone());
        }
        let path = self.locate(id, file)?;
        let stamp = stat_stamp(&path)?;
        let (size, hash) = match file {
            FileRef::Original { .. } => (stamp.0, lt_import::full_hash(&path)?),
            FileRef::Preview { .. } | FileRef::Sidecar { .. } => {
                let b = self.load_bytes(file, &path)?;
                let h = (b.len() as u64, FileHash(*blake3::hash(&b).as_bytes()));
                self.bytes = Some((key.clone(), stamp, Arc::new(b)));
                h
            }
        };
        let c = Current {
            stamp,
            path,
            size,
            hash,
        };
        if self.memo.len() >= MAX_MEMO {
            self.memo.clear();
        }
        self.memo.insert(key, c.clone());
        Ok(c)
    }

    fn read(&mut self, id: &str, file: &FileRef, hash: &str, offset: u64, len: u32) -> Handled {
        let c = self.current(id, file)?;
        if c.hash.to_hex() != hash {
            return Err(fail(
                ErrorCode::Changed,
                "the file changed; ask for it again",
            ));
        }
        if offset > c.size {
            return Err(fail(ErrorCode::BadRequest, "read past the end"));
        }
        let n = (u64::from(len).min(CHUNK as u64)).min(c.size - offset) as usize;
        let mut buf = vec![0u8; n];
        if matches!(file, FileRef::Original { .. }) {
            let mut f = fs::File::open(&c.path)?;
            f.seek(SeekFrom::Start(offset))?;
            f.read_exact(&mut buf)?;
        } else {
            let key = (id.to_owned(), file.clone());
            let cached = self
                .bytes
                .as_ref()
                .filter(|(k, st, _)| *k == key && *st == c.stamp)
                .map(|(_, _, b)| b.clone());
            let b = if let Some(b) = cached {
                b
            } else {
                let b = Arc::new(self.load_bytes(file, &c.path)?);
                if blake3::hash(&b).as_bytes() != &c.hash.0 {
                    return Err(fail(
                        ErrorCode::Changed,
                        "the file changed; ask for it again",
                    ));
                }
                self.bytes = Some((key, c.stamp, b.clone()));
                b
            };
            buf.copy_from_slice(&b[offset as usize..offset as usize + n]);
        }
        match self.shared.read_hook.as_ref().map(|h| h(file, offset)) {
            Some(ReadFault::Drop) => {
                return Ok(Reply {
                    drop: true,
                    ..Reply::ok(Response::Data)
                });
            }
            Some(ReadFault::Corrupt) if n > 0 => buf[0] ^= 0xff,
            _ => {}
        }
        self.shared.sent.fetch_add(n as u64, Ordering::Relaxed);
        *lock(&self.shared.last_transfer) = Some(Instant::now());
        Ok(Reply::with_body(Response::Data, buf))
    }

    /// Push status (`chunk` = None) or one chunk of a new original.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        id: &str,
        name: &str,
        size: u64,
        hash: &str,
        device: &str,
        chunk: Option<(u64, &[u8])>,
    ) -> Handled {
        let hash =
            FileHash::from_hex(hash).ok_or_else(|| fail(ErrorCode::BadRequest, "bad hash"))?;
        let file = valid_file_name(name)?;
        if size > MAX_PUSH_BYTES {
            return Err(fail(
                ErrorCode::BadRequest,
                format!("files over {} GB can't be sent", MAX_PUSH_BYTES >> 30),
            ));
        }
        let folder = format!("From {}", folder_name(device));
        let existing = self.with(id, |_, s| {
            if s.is_external() {
                return Err(fail(
                    ErrorCode::ReadOnly,
                    "this session's originals are a folder opened in place; push to another session",
                ));
            }
            Ok(s.image_by_hash(&hash)?.map(|i| i.rel_path))
        })?;
        if let Some(rel) = existing {
            return Ok(Reply::ok(Response::Pushed {
                rel: rel.as_str().to_owned(),
                duplicate: true,
            }));
        }
        let root = self.root(id)?;
        let dir = root.path.join(PUSH_FOLDER).join(&folder);
        no_symlinks(&root.path, &dir)?;
        let inc = Incoming::in_dir(&dir, size, hash);
        let Some((offset, data)) = chunk else {
            return Ok(Reply::ok(Response::PushState { have: inc.have()? }));
        };
        if !inc.partial.exists() && partial_bytes(&dir) + size > MAX_PARTIAL_BYTES {
            return Err(fail(
                ErrorCode::BadRequest,
                "too many unfinished transfers from this device; finish or wait for them first",
            ));
        }
        let have = inc.write(offset, data)?;
        self.shared
            .received
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        *lock(&self.shared.last_transfer) = Some(Instant::now());
        if have < size {
            return Ok(Reply::ok(Response::PushState { have }));
        }
        if !inc.verify()? {
            return Err(fail(
                ErrorCode::HashMismatch,
                "the received file does not match its hash",
            ));
        }
        let (rel, duplicate) = place_free(&inc, &dir, &file, &format!("{PUSH_FOLDER}/{folder}"))?;
        // A duplicate placed by an earlier try whose indexing failed is
        // indexed now; the device must not be told "delivered" for a file
        // the Mac never lists.
        let indexed = duplicate
            && self.with(id, |_, s| {
                Ok(s.image_by_rel(&RelPath::new(&rel)?)?.is_some())
            })?;
        if !indexed {
            let meta = self.shared.media.meta(&root.path.join(&rel));
            self.with(id, |_, s| {
                s.add_new_originals(vec![NewOriginal {
                    rel: rel.clone(),
                    hash,
                    content_hash: None,
                    meta,
                    replaced: Vec::new(),
                    iptc: IptcMeta::default(),
                }])?;
                Ok(())
            })?;
            lock(&self.shared.changed).insert(root.path.clone());
        }
        Ok(Reply::ok(Response::Pushed { rel, duplicate }))
    }

    /// Merge a device's edited sidecar, all under the session lock: the Mac's
    /// sidecar is read, compared with the device's base and written in one
    /// step, so an edit made on the Mac meanwhile is never lost.
    fn push_sidecar(
        &mut self,
        id: &str,
        rel: &str,
        ordinal: u32,
        push: &SidecarPush,
        device: &str,
    ) -> Handled {
        let rel = RelPath::new(rel)?;
        let remote = parse_sidecar(&serde_json::to_vec(&push.sidecar).expect("JSON value"))?;
        // The base counts only if it is the sidecar it claims to be.
        let base = match (&push.base, &push.based_on) {
            (Some(b), Some(h)) if blake3::hash(b.as_bytes()).to_hex().as_str() == h => {
                Some(parse_sidecar(b.as_bytes())?)
            }
            _ => None,
        };
        let shared = self.shared.clone();
        let (reply, root, touched) = self.with(id, |root, s| {
            let img = s
                .images()?
                .into_iter()
                .find(|i| i.rel_path == rel)
                .ok_or_else(|| {
                    fail(
                        ErrorCode::NotFound,
                        format!("{rel:?} is not in the session"),
                    )
                })?;
            let mut target = None;
            for v in s.variants(img.id)? {
                if s.variant(v)?.ordinal == ordinal {
                    target = Some(v);
                }
            }
            let v =
                target.ok_or_else(|| fail(ErrorCode::NotFound, format!("no variant {ordinal}")))?;
            let path = root.path.join(rel.sidecar_rel(ordinal));
            let local_bytes = fs::read(&path)?;
            let local = parse_sidecar(&local_bytes)?;
            if (remote.variant_id, remote.image_id) != (local.variant_id, local.image_id) {
                return Err(fail(
                    ErrorCode::BadRequest,
                    "the sidecar is for another variant",
                ));
            }
            // A fast-forward needs the verified base to be the Mac's sidecar as
            // it is now; anything else merges, keeping both edits.
            let local_hash = blake3::hash(&local_bytes).to_hex().to_string();
            let fast_forward =
                base.is_some() && push.based_on.as_deref() == Some(local_hash.as_str());
            let remote_newer = push.modified_ms > mtime_ms(&path);
            let (le, re) = (Edit::of(&local), Edit::of(&remote));
            let be = base.as_ref().map(Edit::of);
            let m = merge::merge(be.as_ref(), &le, &re, fast_forward, remote_newer);
            // The losing edit is kept first, so a failure later never loses it.
            let mut kept_ordinal = None;
            let mut touched = vec![v];
            if let Some(k) = &m.kept {
                let nv = s.new_variant(img.id)?;
                s.save_adjustments(nv, &k.adjustments)?;
                let name = if k.remote {
                    format!("Edited on {device}")
                } else {
                    "Edited on this Mac".to_owned()
                };
                s.rename_variant(nv, &name)?;
                kept_ordinal = Some(s.variant(nv)?.ordinal);
                touched.push(nv);
            }
            apply(s, v, &le, &m.edit)?;
            let now = fs::read(&path)?;
            let reply = Reply::with_body(
                Response::SidecarMerged {
                    outcome: m.outcome,
                    hash: blake3::hash(&now).to_hex().to_string(),
                    kept_ordinal,
                },
                now,
            );
            let changed = m.outcome != merge::MergeOutcome::Unchanged;
            if changed {
                // Still under the session lock: no app save can slip in before
                // these variants count as unseen by the app.
                shared.access.device_wrote(&root.path, &touched, device);
            }
            Ok((reply, root.path.clone(), changed))
        })?;
        if touched {
            lock(&shared.changed).insert(root);
        }
        Ok(reply)
    }
}

struct SidecarPush {
    based_on: Option<String>,
    base: Option<String>,
    sidecar: serde_json::Value,
    modified_ms: i64,
}

/// The pending handshake to drop for a newcomer from prefix `from`, given
/// each pending one's (slot, prefix, start): the oldest from `from` when that
/// prefix is at [`MAX_PENDING_PER_IP`]; else, when all [`MAX_PENDING`] are
/// taken, the oldest from the most represented prefix (a flood from a few
/// prefixes pays before a lone real handshake does).
fn victim(pending: &[(u64, IpAddr, Instant)], from: IpAddr) -> Option<u64> {
    let count = |p: IpAddr| pending.iter().filter(|e| e.1 == p).count();
    if count(from) >= MAX_PENDING_PER_IP {
        pending.iter().filter(|e| e.1 == from).min_by_key(|e| e.2)
    } else if pending.len() >= MAX_PENDING {
        pending.iter().max_by_key(|e| (count(e.1), Reverse(e.2)))
    } else {
        None
    }
    .map(|e| e.0)
}

/// A token bucket: [`EVICT_BURST`] tokens, refilled at [`EVICT_PER_SEC`].
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(now: Instant) -> Self {
        Self {
            tokens: EVICT_BURST,
            at: now,
        }
    }

    /// Take a token if there is one.
    fn take(&mut self, now: Instant) -> bool {
        let refill = now.saturating_duration_since(self.at).as_secs_f64() * EVICT_PER_SEC;
        self.tokens = (self.tokens + refill).min(EVICT_BURST);
        self.at = now;
        let ok = self.tokens >= 1.0;
        if ok {
            self.tokens -= 1.0;
        }
        ok
    }
}

fn parse_sidecar(bytes: &[u8]) -> std::result::Result<Sidecar, Fail> {
    Sidecar::from_json(bytes)
        .map(|l| l.sidecar)
        .map_err(|e| fail(ErrorCode::BadRequest, format!("sidecar: {e}")))
}

/// Write the merged edit onto variant `v`, field by field (each an atomic
/// sidecar write through the session, with undo).
fn apply(s: &mut Session, v: VariantId, now: &Edit, to: &Edit) -> lt_session::Result<()> {
    if to.adjustments != now.adjustments {
        s.save_adjustments(v, &to.adjustments)?;
    }
    if to.rating != now.rating {
        s.set_rating(&[v], to.rating)?;
    }
    if to.color_tag != now.color_tag {
        s.set_color_tag(&[v], to.color_tag)?;
    }
    if to.pick != now.pick {
        s.set_pick(&[v], to.pick)?;
    }
    if to.keywords != now.keywords {
        s.set_keywords(&[v], &to.keywords)?;
    }
    Ok(())
}

/// Place a verified push at `dir/name`, or `name-2`, `name-3`… if that is
/// taken by other content. Same content already there: a duplicate (the
/// partial, ours, is dropped). Returns the session-relative path.
fn place_free(inc: &Incoming, dir: &Path, name: &str, rel_dir: &str) -> io::Result<(String, bool)> {
    for n in 1..1000 {
        let file = if n == 1 {
            name.to_owned()
        } else {
            match name.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{stem}-{n}.{ext}"),
                _ => format!("{name}-{n}"),
            }
        };
        let dest = dir.join(&file);
        let rel = format!("{rel_dir}/{file}");
        if transfer::has_content(&dest, &inc.hash)? {
            lt_fsutil::remove_if_exists(&inc.partial)?;
            return Ok((rel, true));
        }
        match inc.place(&dest) {
            Ok(()) => return Ok((rel, false)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free name"))
}

fn valid_file_name(name: &str) -> std::result::Result<String, Fail> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0', ':']);
    if ok {
        Ok(name.to_owned())
    } else {
        Err(fail(
            ErrorCode::BadRequest,
            format!("bad file name {name:?}"),
        ))
    }
}

/// A device's own name for itself, kept short and free of control characters
/// (it is stored, shown in Settings and logged).
fn device_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME)
        .collect();
    let s = s.trim();
    if s.is_empty() {
        "Device".to_owned()
    } else {
        s.to_owned()
    }
}

/// A device name made safe as one folder name.
fn folder_name(device: &str) -> String {
    let s: String = device
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .take(60)
        .collect();
    let s = s.trim().trim_start_matches('.').trim();
    if s.is_empty() {
        "Remote".to_owned()
    } else {
        s.to_owned()
    }
}

/// Refuse a destination reached through a symlink: pushes never leave the root.
fn no_symlinks(root: &Path, dir: &Path) -> std::result::Result<(), Fail> {
    let rel = dir
        .strip_prefix(root)
        .map_err(|_| fail(ErrorCode::BadRequest, "outside"))?;
    let mut p = root.to_path_buf();
    for c in rel.components() {
        p.push(c);
        match fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(fail(ErrorCode::BadRequest, "the destination is a symlink"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether `name` is one of our push partials (`.<16 hex>.partial`).
fn is_push_partial(name: &str) -> bool {
    name.strip_prefix('.')
        .and_then(|n| n.strip_suffix(".partial"))
        .is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Bytes held in push partials in one device's folder.
fn partial_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir).map_or(0, |files| {
        files
            .flatten()
            .filter(|f| is_push_partial(&f.file_name().to_string_lossy()))
            .filter_map(|f| fs::symlink_metadata(f.path()).ok())
            .filter(fs::Metadata::is_file)
            .map(|m| m.len())
            .sum()
    })
}

/// Remove our own push partials (`.<hex>.partial` in `Capture/From …/`)
/// untouched for `age`. Anything else is left alone.
fn remove_stale_partials(capture: &Path, age: Duration) {
    let Ok(dirs) = fs::read_dir(capture) else {
        return;
    };
    for d in dirs.flatten() {
        if !d.file_name().to_string_lossy().starts_with("From ") {
            continue;
        }
        let Ok(files) = fs::read_dir(d.path()) else {
            continue;
        };
        for f in files.flatten() {
            let ours = is_push_partial(&f.file_name().to_string_lossy());
            let old = fs::symlink_metadata(f.path())
                .ok()
                .filter(fs::Metadata::is_file)
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|e| e >= age);
            if ours && old {
                let _ = fs::remove_file(f.path());
            }
        }
    }
}

fn stat_stamp(path: &Path) -> io::Result<(u64, i128)> {
    let m = fs::metadata(path)?;
    if !m.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "not a file"));
    }
    let t = m
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i128);
    Ok((m.len(), t))
}

fn mtime_ms(path: &Path) -> i64 {
    stat_stamp(path).map_or(0, |(_, ns)| (ns / 1_000_000) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshakes_are_dropped_from_the_busiest_prefix_oldest_first() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let ip = |s: &str| net::prefix(s.parse().unwrap());
        // Rotating addresses in one /64 hit the per-address limit.
        let flood = [(1, ip("fe80::1"), at(1)), (2, ip("fe80::2"), at(2))];
        assert_eq!(victim(&flood, ip("fe80::3")), Some(1));
        assert_eq!(victim(&flood, ip("192.168.1.9")), None, "room for others");
        // All taken: the oldest of the most represented prefix goes, not the
        // oldest overall (a real handshake from its own address).
        let mut full = vec![(0, ip("192.168.1.9"), at(0))];
        for i in 0..7u64 {
            let a = format!("fd00:0:0:{}::{i}", i / 2);
            full.push((10 + i, ip(&a), at(10 + i)));
        }
        assert_eq!(full.len(), MAX_PENDING);
        assert_eq!(victim(&full, ip("10.0.0.1")), Some(10));
        // Every prefix alike: the oldest overall.
        let even: Vec<_> = (0..8u64)
            .map(|i| (i, ip(&format!("10.0.0.{i}")), at(100 - i)))
            .collect();
        assert_eq!(victim(&even, ip("10.0.0.200")), Some(7));
    }

    #[test]
    fn evictions_are_budgeted() {
        let t0 = Instant::now();
        let mut b = Bucket::full(t0);
        for _ in 0..8 {
            assert!(b.take(t0));
        }
        assert!(!b.take(t0), "the burst is spent");
        assert!(!b.take(t0 + Duration::from_millis(400)));
        assert!(b.take(t0 + Duration::from_millis(600)), "refilled at 2/s");
        assert!(!b.take(t0 + Duration::from_millis(600)));
        // Refills stop at the burst.
        let later = t0 + Duration::from_secs(3600);
        assert_eq!((0..20).filter(|_| b.take(later)).count(), 8);
    }

    #[test]
    fn names_are_validated_and_sanitized() {
        assert!(valid_file_name("DSC0001.ARW").is_ok());
        for bad in ["", ".hidden", "a/b", "a:b", "a\\b", &"x".repeat(201)] {
            assert_eq!(
                valid_file_name(bad).unwrap_err().code,
                ErrorCode::BadRequest
            );
        }
        assert_eq!(folder_name("Sam's iPad"), "Sam's iPad");
        assert_eq!(folder_name("../a/b:c"), "-a-b-c");
        assert_eq!(folder_name("  ..  "), "Remote");
        assert_eq!(folder_name("x\u{7}"), "x");
    }

    #[test]
    fn free_names_never_replace_and_detect_duplicates() {
        let t = tempfile::tempdir().unwrap();
        let put = |name: &str, data: &[u8]| {
            let h = FileHash(*blake3::hash(data).as_bytes());
            let inc = Incoming::in_dir(t.path(), data.len() as u64, h);
            inc.write(0, data).unwrap();
            assert!(inc.verify().unwrap());
            let r = place_free(&inc, t.path(), name, "Capture/X").unwrap();
            assert!(!inc.partial.exists());
            r
        };
        assert_eq!(put("a.arw", b"1"), ("Capture/X/a.arw".into(), false));
        assert_eq!(put("a.arw", b"2"), ("Capture/X/a-2.arw".into(), false));
        assert_eq!(put("a.arw", b"1"), ("Capture/X/a.arw".into(), true));
        assert_eq!(put("noext", b"3"), ("Capture/X/noext".into(), false));
        assert_eq!(put("noext", b"4"), ("Capture/X/noext-2".into(), false));
        assert_eq!(fs::read(t.path().join("a.arw")).unwrap(), b"1");
        assert_eq!(fs::read(t.path().join("a-2.arw")).unwrap(), b"2");
    }

    #[test]
    fn fails_map_to_codes() {
        let f: Fail = io::Error::from(io::ErrorKind::NotFound).into();
        assert_eq!(f.code, ErrorCode::NotFound);
        let f: Fail = io::Error::from(io::ErrorKind::InvalidInput).into();
        assert_eq!(f.code, ErrorCode::BadRequest);
        let f: Fail = io::Error::other("x").into();
        assert_eq!(f.code, ErrorCode::Internal);
        let f: Fail = RemoteError::Protocol("x".into()).into();
        assert_eq!(f.code, ErrorCode::Internal);
        let f: Fail = RemoteError::Io(io::Error::from(io::ErrorKind::NotFound)).into();
        assert_eq!(f.code, ErrorCode::NotFound);
        let f: Fail = RemoteError::Session(SessionError::NotFound {
            kind: "image",
            id: 1,
        })
        .into();
        assert_eq!(f.code, ErrorCode::NotFound);
        assert!(no_symlinks(Path::new("/a"), Path::new("/b")).is_err());
        assert_eq!(mtime_ms(Path::new("/nonexistent/x")), 0);
        assert!(
            stat_stamp(Path::new("/")).is_err(),
            "a directory is not a file"
        );
    }

    #[test]
    fn only_our_stale_push_partials_are_removed() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("From iPad");
        let other = t.path().join("Day 1");
        fs::create_dir_all(&from).unwrap();
        fs::create_dir_all(&other).unwrap();
        for n in [".0123456789abcdef.partial", ".notours.partial", "A.ARW"] {
            fs::write(from.join(n), b"x").unwrap();
        }
        fs::write(other.join(".0123456789abcdef.partial"), b"x").unwrap();
        remove_stale_partials(t.path(), Duration::from_secs(3600));
        assert!(
            from.join(".0123456789abcdef.partial").exists(),
            "too recent"
        );
        remove_stale_partials(t.path(), Duration::ZERO);
        assert!(!from.join(".0123456789abcdef.partial").exists());
        assert!(from.join(".notours.partial").exists());
        assert!(from.join("A.ARW").exists());
        assert!(
            other.join(".0123456789abcdef.partial").exists(),
            "only in From folders"
        );
        remove_stale_partials(&t.path().join("missing"), Duration::ZERO);
        fs::write(from.join(".fedcba9876543210.partial"), b"12345").unwrap();
        assert_eq!(partial_bytes(&from), 5, "only our partials count");
        assert_eq!(partial_bytes(&t.path().join("missing")), 0);
        assert_eq!(
            format!("{:?}", OwnSessions::default()),
            "OwnSessions { .. }"
        );
    }

    #[test]
    fn raw_media_reads_nothing_from_a_non_raw() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x.arw");
        fs::write(&p, b"not a raw").unwrap();
        assert!(RawMedia.preview(&p).is_err());
        assert_eq!(RawMedia.meta(&p), ImageMetaLite::default());
    }
}
