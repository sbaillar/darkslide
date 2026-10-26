//! The client end: what the iPad app does, in Rust, for the CLI and tests.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use lt_core::FileHash;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use serde_json::Value;

use crate::identity::{Fingerprint, Identity};
use crate::merge::MergeOutcome;
use crate::pairing::{self, Exchange};
use crate::tls::{self, Pin};
use crate::transfer::{self, Incoming};
use crate::wire::{
    self, CHUNK, ErrorCode, FileRef, PROTOCOL_VERSION, PhotoInfo, Request, Response, Secret,
    SessionInfo,
};
use crate::{RemoteError, Result};

/// Whole-transfer retries after a hash mismatch or a file that changed.
pub const MAX_RETRIES: u32 = 2;
/// Largest file [`Client::fetch`] holds in memory (sidecars are a few KB).
pub const MAX_FETCH: u64 = 16 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// A connection to a Mac.
pub struct Client {
    tls: StreamOwned<ClientConnection, TcpStream>,
    me: Fingerprint,
    server: Fingerprint,
    /// The Mac's name.
    pub server_name: String,
    pub paired: bool,
    pub write: bool,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("server", &self.server)
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    /// Received and verified; `resumed_from` > 0 when a `.partial` was continued.
    Fetched {
        bytes: u64,
        resumed_from: u64,
        retries: u32,
    },
    /// The destination already has this content.
    AlreadyHere,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    /// Where the Mac put it (session-relative).
    pub rel: String,
    /// The session already had this content.
    pub duplicate: bool,
    pub resumed_from: u64,
    pub retries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarPushed {
    pub outcome: MergeOutcome,
    /// The Mac's sidecar after the merge, and its hash (the next `based_on`).
    pub sidecar: Vec<u8>,
    pub hash: String,
    pub kept_ordinal: Option<u32>,
}

fn unexpected(r: &Response) -> RemoteError {
    RemoteError::Protocol(format!("unexpected answer {r:?}"))
}

impl Client {
    /// Connect, verify the Mac's key against `pin`, and say hello as `name`.
    pub fn connect(addr: impl ToSocketAddrs, id: &Identity, pin: Pin, name: &str) -> Result<Self> {
        let addrs: Vec<SocketAddr> = addr.to_socket_addrs()?.collect();
        let mut last = None;
        let mut sock = None;
        for a in addrs {
            match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
                Ok(s) => {
                    sock = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let sock = sock.ok_or_else(|| {
            last.unwrap_or_else(|| std::io::Error::other("no address to connect to"))
        })?;
        sock.set_read_timeout(Some(IO_TIMEOUT))?;
        sock.set_write_timeout(Some(IO_TIMEOUT))?;
        let (cfg, verifier) = tls::client_config(id, pin)?;
        let name_ = ServerName::try_from(tls::SERVER_NAME).expect("valid DNS name");
        let mut tls = StreamOwned::new(ClientConnection::new(cfg, name_)?, sock);
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock)?;
        }
        let server = verifier
            .seen
            .lock()
            .expect("pin lock")
            .ok_or_else(|| RemoteError::Protocol("the Mac sent no key".into()))?;
        let mut c = Self {
            tls,
            me: id.fingerprint(),
            server,
            server_name: String::new(),
            paired: false,
            write: false,
        };
        match c.call(
            &Request::Hello {
                version: PROTOCOL_VERSION,
                name: name.to_owned(),
            },
            &[],
        )? {
            (
                Response::Welcome {
                    name,
                    paired,
                    write,
                    ..
                },
                _,
            ) => {
                c.server_name = name;
                c.paired = paired;
                c.write = write;
                Ok(c)
            }
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// The Mac's key as seen in the handshake.
    pub fn server_fingerprint(&self) -> Fingerprint {
        self.server
    }

    /// One raw request and its reply (the typed methods are built on it).
    pub fn call(&mut self, req: &Request, body: &[u8]) -> Result<(Response, Vec<u8>)> {
        wire::send(&mut self.tls, req, body)?;
        match wire::recv::<Response>(&mut self.tls)? {
            (Response::Error { code, message }, _) => Err(RemoteError::Refused { code, message }),
            ok => Ok(ok),
        }
    }

    /// Pair with the code (typed) or token (from the QR code) shown on the Mac.
    pub fn pair(&mut self, secret: &str, kind: Secret) -> Result<()> {
        let (ex, msg) = Exchange::start(secret, true);
        let spake = match self.call(
            &Request::PairStart {
                secret: kind,
                spake: msg,
            },
            &[],
        )? {
            (Response::PairChallenge { spake }, _) => spake,
            (r, _) => return Err(unexpected(&r)),
        };
        let key = ex.finish(&spake)?;
        let confirm = pairing::confirm(&key, "client", &self.me, &self.server);
        let (got, name) = match self.call(&Request::PairFinish { confirm }, &[])? {
            (Response::Paired { confirm, name }, _) => (confirm, name),
            (r, _) => return Err(unexpected(&r)),
        };
        if !pairing::confirms_match(
            &got,
            &pairing::confirm(&key, "server", &self.me, &self.server),
        ) {
            return Err(RemoteError::Pairing(
                "the Mac could not prove it knows the code".into(),
            ));
        }
        self.server_name = name;
        self.paired = true;
        Ok(())
    }

    pub fn sessions(&mut self) -> Result<Vec<SessionInfo>> {
        match self.call(&Request::ListSessions, &[])? {
            (Response::Sessions { sessions }, _) => Ok(sessions),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Every photo of the session, fetched a page at a time.
    pub fn photos(&mut self, session: &str) -> Result<Vec<PhotoInfo>> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let req = Request::ListPhotos {
                session: session.to_owned(),
                after: after.clone(),
                limit: 0,
            };
            let (photos, next) = match self.call(&req, &[])? {
                (Response::Photos { photos, next }, _) => (photos, next),
                (r, _) => return Err(unexpected(&r)),
            };
            all.extend(photos);
            match next {
                // A page that doesn't advance would loop forever.
                Some(n) if after.as_deref() != Some(&n) => after = Some(n),
                Some(_) => return Err(RemoteError::Protocol("photo list does not advance".into())),
                None => return Ok(all),
            }
        }
    }

    /// Size and BLAKE3 of a file on the Mac.
    pub fn stat(&mut self, session: &str, file: &FileRef) -> Result<(u64, FileHash)> {
        let req = Request::Stat {
            session: session.to_owned(),
            file: file.clone(),
        };
        match self.call(&req, &[])? {
            (Response::File { size, hash }, _) => Ok((
                size,
                FileHash::from_hex(&hash)
                    .ok_or_else(|| RemoteError::Protocol(format!("bad hash {hash}")))?,
            )),
            (r, _) => Err(unexpected(&r)),
        }
    }

    fn read(
        &mut self,
        session: &str,
        file: &FileRef,
        hash: &FileHash,
        offset: u64,
    ) -> Result<Vec<u8>> {
        let req = Request::Read {
            session: session.to_owned(),
            file: file.clone(),
            hash: hash.to_hex(),
            offset,
            len: CHUNK as u32,
        };
        match self.call(&req, &[])? {
            (Response::Data, body) if !body.is_empty() => Ok(body),
            (r, _) => Err(unexpected(&r)),
        }
    }

    /// Fetch `file` to `dest`: resumes a `.partial` left by an earlier
    /// attempt, verifies the whole file's BLAKE3, and retries from scratch
    /// on a mismatch. Never replaces a different file at `dest`.
    pub fn pull(&mut self, session: &str, file: &FileRef, dest: &Path) -> Result<PullOutcome> {
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir).map_err(|e| RemoteError::file(dir, e))?;
        }
        let mut retries = 0;
        loop {
            let (size, hash) = self.stat(session, file)?;
            if transfer::has_content(dest, &hash)? {
                return Ok(PullOutcome::AlreadyHere);
            }
            if fs::symlink_metadata(dest).is_ok() {
                return Err(RemoteError::Exists(dest.to_path_buf()));
            }
            let inc = Incoming::for_dest(dest, size, hash);
            let resumed_from = inc.have()?;
            let mut off = resumed_from;
            let mut changed = false;
            while off < size {
                match self.read(session, file, &hash, off) {
                    Ok(data) => off = inc.write(off, &data)?,
                    Err(e) if e.code() == Some(ErrorCode::Changed) => {
                        changed = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            if !changed && inc.verify()? {
                inc.place(dest).map_err(|e| RemoteError::file(dest, e))?;
                return Ok(PullOutcome::Fetched {
                    bytes: size - resumed_from,
                    resumed_from,
                    retries,
                });
            }
            lt_fsutil::remove_if_exists(&inc.partial)?;
            retries += 1;
            if retries > MAX_RETRIES {
                return Err(RemoteError::Protocol(format!(
                    "{} did not arrive intact after {MAX_RETRIES} retries",
                    file.rel()
                )));
            }
        }
    }

    /// A small file (a sidecar) in memory, verified.
    pub fn fetch(&mut self, session: &str, file: &FileRef) -> Result<Vec<u8>> {
        let (size, hash) = self.stat(session, file)?;
        if size > MAX_FETCH {
            return Err(RemoteError::Protocol(format!(
                "{} is {size} bytes; pull it to a file instead",
                file.rel()
            )));
        }
        let mut out = Vec::with_capacity(size as usize);
        while (out.len() as u64) < size {
            let chunk = self.read(session, file, &hash, out.len() as u64)?;
            out.extend_from_slice(&chunk);
        }
        if blake3::hash(&out).as_bytes() != &hash.0 {
            return Err(RemoteError::Protocol(format!(
                "{} arrived damaged",
                file.rel()
            )));
        }
        Ok(out)
    }

    /// Send a new original to a session (needs write access). Resumes what
    /// the Mac already has; retries from scratch if the Mac's check fails.
    pub fn push(&mut self, session: &str, src: &Path) -> Result<PushOutcome> {
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| RemoteError::Protocol(format!("{} has no usable name", src.display())))?
            .to_owned();
        let size = fs::metadata(src)
            .map_err(|e| RemoteError::file(src, e))?
            .len();
        let hash = lt_import::full_hash(src).map_err(|e| RemoteError::file(src, e))?;
        let mut f = File::open(src).map_err(|e| RemoteError::file(src, e))?;
        let mut retries = 0;
        loop {
            let status = Request::PushStatus {
                session: session.to_owned(),
                name: name.clone(),
                size,
                hash: hash.to_hex(),
            };
            let resumed_from = match self.call(&status, &[])? {
                (Response::PushState { have }, _) => have,
                (Response::Pushed { rel, duplicate }, _) => {
                    return Ok(PushOutcome {
                        rel,
                        duplicate,
                        resumed_from: 0,
                        retries,
                    });
                }
                (r, _) => return Err(unexpected(&r)),
            };
            let mut off = resumed_from;
            let mut buf = vec![0u8; CHUNK];
            loop {
                f.seek(SeekFrom::Start(off))?;
                let n = read_up_to(&mut f, &mut buf[..CHUNK.min((size - off) as usize)])?;
                let req = Request::PushChunk {
                    session: session.to_owned(),
                    name: name.clone(),
                    size,
                    hash: hash.to_hex(),
                    offset: off,
                };
                match self.call(&req, &buf[..n]) {
                    Ok((Response::PushState { have }, _)) if have > off => off = have,
                    Ok((Response::Pushed { rel, duplicate }, _)) => {
                        return Ok(PushOutcome {
                            rel,
                            duplicate,
                            resumed_from,
                            retries,
                        });
                    }
                    Ok((r, _)) => return Err(unexpected(&r)),
                    Err(e)
                        if e.code() == Some(ErrorCode::HashMismatch) && retries < MAX_RETRIES =>
                    {
                        retries += 1;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    /// Send an edited sidecar (needs write access); the Mac merges it.
    #[allow(clippy::too_many_arguments)]
    pub fn push_sidecar(
        &mut self,
        session: &str,
        rel: &str,
        ordinal: u32,
        based_on: Option<String>,
        base: Option<String>,
        sidecar: Value,
        modified_ms: i64,
    ) -> Result<SidecarPushed> {
        let req = Request::PushSidecar {
            session: session.to_owned(),
            rel: rel.to_owned(),
            ordinal,
            based_on,
            base,
            sidecar,
            modified_ms,
        };
        match self.call(&req, &[])? {
            (
                Response::SidecarMerged {
                    outcome,
                    hash,
                    kept_ordinal,
                },
                body,
            ) => Ok(SidecarPushed {
                outcome,
                sidecar: body,
                hash,
                kept_ordinal,
            }),
            (r, _) => Err(unexpected(&r)),
        }
    }
}

fn read_up_to(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
