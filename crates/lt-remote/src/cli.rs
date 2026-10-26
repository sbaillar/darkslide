//! `lt-remote`: the test client that stands in for the iPad, plus a
//! standalone server for trying the link between two machines.
//!
//! ```text
//! lt-remote [--state DIR] [--name NAME] <command>
//!   id                                  this device's key fingerprint
//!   serve --session DIR… [--bind ADDR] [--pair]
//!   devices | grant FP | readonly FP | revoke FP
//!   pair <QR payload> | pair HOST:PORT CODE
//!   ls [SESSION]
//!   pull SESSION DEST [--originals] [--no-previews] [--no-sidecars]
//!   push SESSION FILE…
//!   push-sidecars SESSION DEST
//! ```
//!
//! `pull` writes `DEST/<rel>` (originals), `DEST/Previews/<rel>.jpg` and
//! `DEST/Sidecars/<rel>.<variant>.json`, plus a pristine copy of each sidecar
//! under `DEST/Sidecars/.base/` that `push-sidecars` sends as the base. A
//! killed `pull` or `push` resumes where it stopped when run again.

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use crate::client::{Client, PullOutcome};
use crate::identity::{Fingerprint, Identity};
use crate::pairing::{DeviceStore, KnownServer, QrPayload, ServerStore};
use crate::server::{Server, ServerConfig, SessionRoot};
use crate::tls::Pin;
use crate::wire::{FileRef, Secret};
use crate::{RemoteError, Result};

/// Default port of `serve` (and of the app).
pub const DEFAULT_PORT: u16 = 47_474;

const USAGE: &str = "usage: lt-remote [--state DIR] [--name NAME] \
<id|serve|devices|grant|readonly|revoke|pair|ls|pull|push|push-sidecars> …";

fn usage(msg: &str) -> RemoteError {
    RemoteError::Protocol(format!("{msg}\n{USAGE}"))
}

struct Opts {
    state: PathBuf,
    name: String,
    args: Vec<String>,
}

impl Opts {
    fn flag(&mut self, f: &str) -> bool {
        let n = self.args.len();
        self.args.retain(|a| a != f);
        self.args.len() != n
    }

    fn values(&mut self, f: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        while let Some(i) = self.args.iter().position(|a| a == f) {
            if i + 1 >= self.args.len() {
                return Err(usage(&format!("{f} needs a value")));
            }
            out.push(self.args.remove(i + 1));
            self.args.remove(i);
        }
        Ok(out)
    }

    fn value(&mut self, f: &str) -> Result<Option<String>> {
        Ok(self.values(f)?.pop())
    }

    fn positional(&self, i: usize, what: &str) -> Result<&str> {
        self.args
            .get(i)
            .map(String::as_str)
            .ok_or_else(|| usage(&format!("missing {what}")))
    }
}

/// Run the CLI with `args` (without the program name). `stop` ends `serve`.
pub fn run(args: Vec<String>, out: &mut dyn Write, stop: &AtomicBool) -> Result<()> {
    let mut o = Opts {
        state: PathBuf::new(),
        name: String::new(),
        args,
    };
    o.state = match o.value("--state")? {
        Some(s) => PathBuf::from(s),
        None => lt_core::env::var_os("LT_REMOTE_STATE")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".luxtable-remote")))
            .ok_or_else(|| usage("no --state and no HOME"))?,
    };
    o.name = o.value("--name")?.unwrap_or_else(|| "lt-remote".into());
    if o.args.is_empty() {
        return Err(usage("missing command"));
    }
    let cmd = o.args.remove(0);
    match cmd.as_str() {
        "id" => {
            let id = Identity::load_or_create(&o.state)?;
            writeln!(out, "{}", id.fingerprint())?;
        }
        "serve" => serve(&mut o, out, stop)?,
        "devices" => {
            for d in DeviceStore::new(&o.state).list()? {
                let access = if d.write { "read-write" } else { "read-only" };
                writeln!(out, "{}  {access}  {}", d.fingerprint, d.name)?;
            }
        }
        "grant" | "readonly" | "revoke" => {
            let fp = device_fp(&o.state, o.positional(0, "device fingerprint")?)?;
            let store = DeviceStore::new(&o.state);
            let found = match cmd.as_str() {
                "grant" => store.set_write(&fp, true)?,
                "readonly" => store.set_write(&fp, false)?,
                _ => store.revoke(&fp)?,
            };
            writeln!(out, "{}", if found { "done" } else { "no such device" })?;
        }
        "pair" => pair(&o, out)?,
        "ls" => {
            let mut c = connect(&mut o)?;
            match o.args.first() {
                None => {
                    for s in c.sessions()? {
                        writeln!(out, "{}  {}", s.id, s.name)?;
                    }
                }
                Some(s) => {
                    let id = session_id(&mut c, s)?;
                    for p in c.photos(&id)? {
                        let variants: Vec<String> =
                            p.variants.iter().map(|v| v.ordinal.to_string()).collect();
                        writeln!(
                            out,
                            "{}  {} bytes  variants {}",
                            p.rel,
                            p.size,
                            variants.join(",")
                        )?;
                    }
                }
            }
        }
        "pull" => pull(&mut o, out)?,
        "push" => {
            let mut c = connect(&mut o)?;
            let id = session_id(&mut c, o.positional(0, "session")?)?;
            for f in &o.args[1..] {
                let r = c.push(&id, Path::new(f))?;
                let what = if r.duplicate { "already there" } else { "sent" };
                writeln!(
                    out,
                    "{f}: {what} as {} (resumed at {})",
                    r.rel, r.resumed_from
                )?;
            }
        }
        "push-sidecars" => push_sidecars(&mut o, out)?,
        other => return Err(usage(&format!("unknown command {other}"))),
    }
    Ok(())
}

fn device_fp(state: &Path, prefix: &str) -> Result<String> {
    let matches: Vec<String> = DeviceStore::new(state)
        .list()?
        .into_iter()
        .map(|d| d.fingerprint)
        .filter(|f| f.starts_with(prefix))
        .collect();
    match &matches[..] {
        [one] => Ok(one.clone()),
        [] => Err(usage(&format!("no paired device {prefix}"))),
        _ => Err(usage(&format!("{prefix} matches several devices"))),
    }
}

fn serve(o: &mut Opts, out: &mut dyn Write, stop: &AtomicBool) -> Result<()> {
    let sessions = o.values("--session")?;
    let bind: SocketAddr = match o.value("--bind")? {
        Some(b) => b.parse().map_err(|_| usage(&format!("bad --bind {b}")))?,
        None => SocketAddr::from(([0u8; 16], DEFAULT_PORT)),
    };
    let pair = o.flag("--pair");
    if sessions.is_empty() {
        return Err(usage("serve needs at least one --session DIR"));
    }
    let server = Server::start(ServerConfig::new(&o.name, &o.state, bind))?;
    server.set_sessions(
        sessions
            .iter()
            .map(|p| SessionRoot {
                path: PathBuf::from(p),
                name: Path::new(p)
                    .file_name()
                    .map_or_else(|| p.clone(), |n| n.to_string_lossy().into_owned()),
            })
            .collect(),
    );
    writeln!(out, "listening on {}", server.addr())?;
    writeln!(out, "fingerprint {}", server.fingerprint())?;
    if pair {
        let t = server.begin_pairing();
        writeln!(out, "pairing code {}", t.code)?;
        let host = lt_core::env::var("LT_REMOTE_HOST").unwrap_or_else(|| "localhost".into());
        writeln!(
            out,
            "qr {}",
            t.qr_payload(&host, server.addr().port(), &server.fingerprint())
        )?;
    }
    out.flush()?;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
    server.stop();
    Ok(())
}

fn pair(o: &Opts, out: &mut dyn Write) -> Result<()> {
    let first = o.positional(0, "QR payload or HOST:PORT")?;
    let (addr, pin, secret, kind) = match QrPayload::parse(first) {
        Some(q) => (q.addr, Pin::Key(q.fingerprint), q.token, Secret::Token),
        None => (
            first.to_owned(),
            Pin::AnyForPairing,
            o.positional(1, "pairing code")?.to_owned(),
            Secret::Code,
        ),
    };
    let id = Identity::load_or_create(&o.state)?;
    let mut c = Client::connect(addr.as_str(), &id, pin, &o.name)?;
    c.pair(&secret, kind)?;
    ServerStore::new(&o.state).add(KnownServer {
        fingerprint: c.server_fingerprint().to_hex(),
        name: c.server_name.clone(),
        addr,
    })?;
    writeln!(
        out,
        "paired with {} ({})",
        c.server_name,
        c.server_fingerprint().short()
    )?;
    Ok(())
}

/// Connect to `--server FP-prefix` (default: the last Mac paired), at `--addr` if given.
fn connect(o: &mut Opts) -> Result<Client> {
    let want = o.value("--server")?;
    let addr = o.value("--addr")?;
    let servers = ServerStore::new(&o.state).list()?;
    let s = servers
        .iter()
        .rev()
        .find(|s| {
            want.as_ref()
                .is_none_or(|w| s.fingerprint.starts_with(w.as_str()))
        })
        .ok_or_else(|| usage("not paired with that Mac; run `pair` first"))?;
    let fp = Fingerprint::from_hex(&s.fingerprint)
        .ok_or_else(|| RemoteError::Protocol("damaged servers.json".into()))?;
    let id = Identity::load_or_create(&o.state)?;
    Client::connect(
        addr.as_deref().unwrap_or(&s.addr),
        &id,
        Pin::Key(fp),
        &o.name,
    )
}

/// A session by id or by name.
fn session_id(c: &mut Client, s: &str) -> Result<String> {
    c.sessions()?
        .into_iter()
        .find(|x| x.id == s || x.name == s)
        .map(|x| x.id)
        .ok_or_else(|| usage(&format!("no session {s}")))
}

fn pull(o: &mut Opts, out: &mut dyn Write) -> Result<()> {
    let originals = o.flag("--originals");
    let previews = !o.flag("--no-previews");
    let sidecars = !o.flag("--no-sidecars");
    let mut c = connect(o)?;
    let id = session_id(&mut c, o.positional(0, "session")?)?;
    let dest = PathBuf::from(o.positional(1, "destination")?);
    let (mut fetched, mut here, mut kept) = (0, 0, 0);
    let mut count = |r: PullOutcome| match r {
        PullOutcome::Fetched { .. } => fetched += 1,
        PullOutcome::AlreadyHere => here += 1,
    };
    for p in c.photos(&id)? {
        if p.missing {
            continue;
        }
        // Paths come from the Mac: only session paths (no `..`, nothing
        // hidden) are joined onto the destination.
        if lt_session::RelPath::new(&p.rel).is_err() {
            writeln!(out, "{:?}: not a session path; skipped", p.rel)?;
            continue;
        }
        if originals {
            count(c.pull(
                &id,
                &FileRef::Original { rel: p.rel.clone() },
                &dest.join(&p.rel),
            )?);
        }
        if previews {
            let to = dest.join("Previews").join(format!("{}.jpg", p.rel));
            match c.pull(&id, &FileRef::Preview { rel: p.rel.clone() }, &to) {
                Ok(r) => count(r),
                // Not every original has an embedded preview.
                Err(e) if e.code() == Some(crate::ErrorCode::Internal) => {
                    writeln!(out, "{}: no preview ({e})", p.rel)?;
                }
                Err(e) => return Err(e),
            }
        }
        if sidecars {
            for v in &p.variants {
                let file = FileRef::Sidecar {
                    rel: p.rel.clone(),
                    ordinal: v.ordinal,
                };
                let bytes = c.fetch(&id, &file)?;
                if !store_sidecar(&dest, &p.rel, v.ordinal, &bytes)? {
                    kept += 1;
                    writeln!(
                        out,
                        "{} #{}: has local edits; push-sidecars first",
                        p.rel, v.ordinal
                    )?;
                }
            }
        }
    }
    writeln!(
        out,
        "fetched {fetched}, already here {here}, local edits kept {kept}"
    )?;
    Ok(())
}

fn sidecar_paths(dest: &Path, rel: &str, ordinal: u32) -> (PathBuf, PathBuf) {
    let name = format!("{rel}.{ordinal}.json");
    let dir = dest.join("Sidecars");
    (dir.join(&name), dir.join(".base").join(name))
}

/// Store a pulled sidecar as the local copy and its base, unless the local
/// copy has edits not pushed yet (then nothing is written; returns false).
fn store_sidecar(dest: &Path, rel: &str, ordinal: u32, bytes: &[u8]) -> Result<bool> {
    let (local, base) = sidecar_paths(dest, rel, ordinal);
    let read = |p: &Path| fs::read(p).ok();
    let edited =
        matches!((read(&local), read(&base)), (Some(l), b) if Some(&l) != b.as_ref() && l != bytes);
    if edited {
        return Ok(false);
    }
    for p in [&base, &local] {
        lt_fsutil::write_atomic(p, bytes).map_err(|e| RemoteError::file(p, e))?;
    }
    Ok(true)
}

fn push_sidecars(o: &mut Opts, out: &mut dyn Write) -> Result<()> {
    let mut c = connect(o)?;
    let id = session_id(&mut c, o.positional(0, "session")?)?;
    let dest = PathBuf::from(o.positional(1, "destination")?);
    let dir = dest.join("Sidecars");
    let mut pushed = 0;
    for local in walk_json(&dir)? {
        let rel_name = local
            .strip_prefix(&dir)
            .expect("under dir")
            .to_string_lossy()
            .into_owned();
        let Some((rel, ordinal)) = rel_name
            .strip_suffix(".json")
            .and_then(|s| s.rsplit_once('.'))
            .and_then(|(r, n)| Some((r.to_owned(), n.parse::<u32>().ok()?)))
        else {
            continue;
        };
        let (_, base_path) = sidecar_paths(&dest, &rel, ordinal);
        let bytes = fs::read(&local).map_err(|e| RemoteError::file(&local, e))?;
        let base = fs::read(&base_path).ok();
        if base.as_ref() == Some(&bytes) {
            continue;
        }
        let json = |b: &[u8]| {
            serde_json::from_slice::<serde_json::Value>(b)
                .map_err(|e| RemoteError::Protocol(format!("{}: {e}", local.display())))
        };
        let modified_ms = fs::metadata(&local)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64);
        let r = c.push_sidecar(
            &id,
            &rel,
            ordinal,
            base.as_deref()
                .map(|b| blake3::hash(b).to_hex().to_string()),
            base.as_deref()
                .map(|b| String::from_utf8_lossy(b).into_owned()),
            json(&bytes)?,
            modified_ms,
        )?;
        for p in [&base_path, &local] {
            lt_fsutil::write_atomic(p, &r.sidecar).map_err(|e| RemoteError::file(p, e))?;
        }
        pushed += 1;
        let kept = r
            .kept_ordinal
            .map(|k| format!(", other edit kept as variant {k}"))
            .unwrap_or_default();
        writeln!(out, "{rel} #{ordinal}: {:?}{kept}", r.outcome)?;
    }
    writeln!(out, "pushed {pushed}")?;
    Ok(())
}

/// `*.json` under `dir`, skipping hidden entries (the `.base` copies).
fn walk_json(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(RemoteError::file(&d, e)),
        };
        for e in entries {
            let p = e?.path();
            if p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "json") {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}
