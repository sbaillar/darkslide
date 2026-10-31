//! rsync discovery, argument building, progress parsing, and process running
//! behind a [`CommandRunner`] seam.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::ArchiveError;
use crate::archiver::SNAPSHOT_REL;

/// Homebrew rsync 3.x (preferred), then the system openrsync.
pub const DEFAULT_RSYNC_PATHS: &[&str] = &["/opt/homebrew/bin/rsync", "/usr/bin/rsync"];

/// Which rsync implementation we are driving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RsyncFlavor {
    /// Samba rsync ≥ 3 (supports `--info=progress2`).
    Rsync3 { version: String },
    /// Older samba rsync (2.x): no `--info`.
    Legacy { version: String },
    /// Apple's openrsync (macOS 15+ `/usr/bin/rsync`), protocol 29.
    Openrsync,
}

impl RsyncFlavor {
    /// Parse `rsync --version` output.
    pub fn from_version_output(out: &str) -> Self {
        if out.contains("openrsync") {
            return Self::Openrsync;
        }
        let version = out
            .split_whitespace()
            .skip_while(|w| *w != "version")
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        let major: u32 = version
            .split('.')
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0);
        if major >= 3 {
            Self::Rsync3 { version }
        } else {
            Self::Legacy { version }
        }
    }

    pub fn supports_progress2(&self) -> bool {
        matches!(self, Self::Rsync3 { .. })
    }
}

/// A command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code; `None` if killed by a signal (e.g. cancelled).
    pub code: Option<i32>,
    pub stderr: String,
}

/// Runs external processes. Tests use a fake; production uses [`SystemRunner`].
pub trait CommandRunner: Send + Sync {
    /// Run `cmd`, calling `on_line` for every stdout line (split on `\n` *and*
    /// `\r`, since rsync redraws progress with carriage returns). Must kill the
    /// process promptly once `cancel` is set.
    fn run(
        &self,
        cmd: &CommandSpec,
        on_line: &mut dyn FnMut(&str),
        cancel: &AtomicBool,
    ) -> io::Result<CommandOutput>;
}

/// [`CommandRunner`] backed by `std::process`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

/// iOS can't start processes, so there is no rsync to run: archiving stays a Mac job.
#[cfg(not(target_os = "macos"))]
impl CommandRunner for SystemRunner {
    fn run(
        &self,
        cmd: &CommandSpec,
        _on_line: &mut dyn FnMut(&str),
        _cancel: &AtomicBool,
    ) -> io::Result<CommandOutput> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{} can't run here: archiving needs macOS",
                cmd.program.display()
            ),
        ))
    }
}

#[cfg(target_os = "macos")]
impl CommandRunner for SystemRunner {
    fn run(
        &self,
        cmd: &CommandSpec,
        on_line: &mut dyn FnMut(&str),
        cancel: &AtomicBool,
    ) -> io::Result<CommandOutput> {
        let mut child = Command::new(&cmd.program)
            .args(&cmd.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("no stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("no stderr"))?;
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            let err_reader = s.spawn(move || {
                let mut buf = String::new();
                let _ = stderr.read_to_string(&mut buf);
                buf
            });
            // Watchdog: kill the child when cancelled; reaps it on exit.
            let watchdog = s.spawn(|| -> io::Result<Option<i32>> {
                loop {
                    if let Some(status) = child.try_wait()? {
                        return Ok(status.code());
                    }
                    if cancel.load(Ordering::Relaxed) || done.load(Ordering::Relaxed) {
                        let _ = child.kill();
                        return Ok(child.wait()?.code());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
            let mut pending = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        for &b in &buf[..n] {
                            if b == b'\n' || b == b'\r' {
                                if !pending.is_empty() {
                                    on_line(&String::from_utf8_lossy(&pending));
                                    pending.clear();
                                }
                            } else {
                                pending.push(b);
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        done.store(true, Ordering::Relaxed);
                        let _ = watchdog.join();
                        return Err(e);
                    }
                }
            }
            if !pending.is_empty() {
                on_line(&String::from_utf8_lossy(&pending));
            }
            let code = watchdog
                .join()
                .map_err(|_| io::Error::other("watchdog panicked"))??;
            let stderr = err_reader.join().unwrap_or_default();
            Ok(CommandOutput { code, stderr })
        })
    }
}

/// One `--info=progress2` line, e.g.
/// `  1,238,099  45%  115.13MB/s  0:00:10 (xfr#5, to-chk=10/20)`.
#[derive(Debug, Clone, PartialEq)]
pub struct RsyncProgress {
    pub bytes: u64,
    pub percent: u8,
    pub bytes_per_sec: f64,
    pub eta: Option<Duration>,
    pub files_transferred: Option<u64>,
    /// (remaining to check, total) from `to-chk=`/`ir-chk=`.
    pub to_check: Option<(u64, u64)>,
}

/// Parse a progress line; `None` for anything else (file names, stats).
pub fn parse_progress(line: &str) -> Option<RsyncProgress> {
    let mut words = line.split_whitespace();
    let bytes: u64 = words.next()?.replace(',', "").parse().ok()?;
    let percent: u8 = words.next()?.strip_suffix('%')?.parse().ok()?;
    let bytes_per_sec = parse_rate(words.next()?)?;
    let eta = words.next().and_then(parse_hms);
    let rest: String = words.collect::<Vec<_>>().join(" ");
    let files_transferred = rest
        .split("xfr#")
        .nth(1)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok());
    let to_check = rest.split("-chk=").nth(1).and_then(|s| {
        let s = s.trim_end_matches(')');
        let (a, b) = s.split_once('/')?;
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    Some(RsyncProgress {
        bytes,
        percent,
        bytes_per_sec,
        eta,
        files_transferred,
        to_check,
    })
}

fn parse_rate(s: &str) -> Option<f64> {
    let s = s.strip_suffix("/s")?;
    let (num, mult) = [("GB", 1e9_f64), ("MB", 1e6), ("kB", 1e3), ("B", 1.0)]
        .iter()
        .find_map(|(suf, m)| s.strip_suffix(suf).map(|n| (n, *m)))?;
    Some(num.replace(',', "").parse::<f64>().ok()? * mult)
}

fn parse_hms(s: &str) -> Option<Duration> {
    let mut secs = 0u64;
    for part in s.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(Duration::from_secs(secs))
}

/// Result of one rsync invocation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncResult {
    /// Non-fatal notes (e.g. exit 24: files vanished during transfer).
    pub warnings: Vec<String>,
    /// Source files rsync could not read (exit 23 on the sending side), one
    /// `"<path relative to the source>: <reason>"` each. They are **not** in
    /// the archive: the run is incomplete, not a success.
    pub skipped: Vec<String>,
}

/// A detected rsync binary plus a runner to execute it.
#[derive(Clone)]
pub struct RsyncRunner {
    runner: Arc<dyn CommandRunner>,
    program: PathBuf,
    flavor: RsyncFlavor,
    modify_window: u32,
}

impl std::fmt::Debug for RsyncRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsyncRunner")
            .field("program", &self.program)
            .field("flavor", &self.flavor)
            .field("modify_window", &self.modify_window)
            .finish_non_exhaustive()
    }
}

impl RsyncRunner {
    /// Pick the first existing binary of [`DEFAULT_RSYNC_PATHS`] and probe its version.
    pub fn detect(runner: Arc<dyn CommandRunner>) -> Result<Self, ArchiveError> {
        let paths: Vec<PathBuf> = DEFAULT_RSYNC_PATHS.iter().map(PathBuf::from).collect();
        Self::detect_from(runner, &paths)
    }

    pub fn detect_from(
        runner: Arc<dyn CommandRunner>,
        candidates: &[PathBuf],
    ) -> Result<Self, ArchiveError> {
        let program = candidates
            .iter()
            .find(|p| p.is_file())
            .cloned()
            .ok_or(ArchiveError::RsyncNotFound)?;
        let mut out = String::new();
        let spec = CommandSpec {
            program: program.clone(),
            args: vec!["--version".into()],
        };
        let res = runner
            .run(
                &spec,
                &mut |l| {
                    out.push_str(l);
                    out.push('\n');
                },
                &AtomicBool::new(false),
            )
            .map_err(|e| ArchiveError::Io {
                path: program.clone(),
                source: e,
            })?;
        // openrsync prints its version on stdout, some builds on stderr.
        out.push_str(&res.stderr);
        let flavor = RsyncFlavor::from_version_output(&out);
        if matches!(flavor, RsyncFlavor::Openrsync) {
            tracing::warn!(
                "using openrsync: no --info=progress2; install Homebrew rsync 3.x for progress"
            );
        }
        Ok(Self {
            runner,
            program,
            flavor,
            modify_window: 0,
        })
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn flavor(&self) -> &RsyncFlavor {
        &self.flavor
    }

    /// Arguments for the main archive pass of `src` into `dest`.
    ///
    /// * `-rlt --no-owner --no-group` instead of `-a`: recurse, symlinks,
    ///   mtimes, but no owner/group/perms/devices. The NAS (NFS, root-squashed
    ///   or mapped to one user) rejects chown/chgrp, which made every run exit
    ///   23 with noise; photo permissions carry no meaning in the archive.
    /// * `--partial-dir=.lt-rsync-partial`: an interrupted transfer leaves its
    ///   partial file in that directory, never under the real name, so a
    ///   truncated ARW can never replace a good archived one.
    /// * `--backup --backup-dir=…`: replaced files are kept. Never `--delete`.
    /// * `--timeout=60`: a hung NAS fails the run instead of blocking forever
    ///   (the checksum pass gets [`CHECKSUM_TIMEOUT_SECS`]: hashing a big `.lt/` goes
    ///   quiet on the socket for a while).
    /// * The index snapshot is left to [`Self::snapshot_pass_args`]: it is rewritten
    ///   every run and must not pile its old copies into the backup dir.
    /// * `--modify-window=N` when the target needs it (FAT: 1 s, see
    ///   [`crate::target::mtime_window_for`]). Never `-1`: nanosecond compares
    ///   re-send (and trash) every file on targets with coarser timestamps.
    ///   Same-second sidecar edits are caught by the checksum pass instead.
    /// * rsync 3 only: `--info=progress2`.
    /// * `--` before the paths (every pass), so a path is never read as an option.
    pub fn args(
        &self,
        src: &Path,
        dest: &Path,
        exclude_from: Option<&Path>,
        backup_dir: &str,
    ) -> Vec<OsString> {
        let mut a = self.common_args(exclude_from, backup_dir, TIMEOUT_SECS);
        if self.flavor.supports_progress2() {
            a.push("--info=progress2".into());
        }
        a.push("--".into());
        a.push(with_trailing_slash(src));
        a.push(with_trailing_slash(dest));
        a
    }

    /// Second pass, on every rsync: `--checksum` over the small, frequently
    /// edited files (sidecars and `.lt/` state, never `.lt/cache/`), so an
    /// edit within the same second (same size) still lands, whatever the
    /// mtime resolution of either side. Unchanged files match by content and
    /// are not re-sent.
    pub fn checksum_pass_args(
        &self,
        src: &Path,
        dest: &Path,
        exclude_from: Option<&Path>,
        backup_dir: &str,
    ) -> Vec<OsString> {
        let mut a = self.common_args(exclude_from, backup_dir, CHECKSUM_TIMEOUT_SECS);
        a.push("--checksum".into());
        for inc in ["*/", "*.json", "*.xmp", "*.XMP", ".lt/**"] {
            a.push(format!("--include={inc}").into());
        }
        a.push("--exclude=*".into());
        a.push("--".into());
        a.push(with_trailing_slash(src));
        a.push(with_trailing_slash(dest));
        a
    }

    /// Third pass: only the session DB snapshot, **without** `--backup`. It is a
    /// derived copy rewritten every run (hundreds of MB on a big library), so its
    /// previous version is simply replaced instead of being kept in the trash.
    /// `exclude_from` still applies first, so a failed snapshot stays excluded.
    pub fn snapshot_pass_args(
        &self,
        src: &Path,
        dest: &Path,
        exclude_from: Option<&Path>,
    ) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![
            "-rlt".into(),
            "--no-owner".into(),
            "--no-group".into(),
            format!("--timeout={TIMEOUT_SECS}").into(),
            format!("--partial-dir={PARTIAL_DIR}").into(),
        ];
        if self.modify_window > 0 {
            a.push(format!("--modify-window={}", self.modify_window).into());
        }
        if let Some(f) = exclude_from {
            let mut arg = OsString::from("--exclude-from=");
            arg.push(f);
            a.push(arg);
        }
        for p in ["--include=/.lt/", &format!("--include=/{SNAPSHOT_REL}")] {
            a.push(p.into());
        }
        a.push("--exclude=*".into());
        a.push("--".into());
        a.push(with_trailing_slash(src));
        a.push(with_trailing_slash(dest));
        a
    }

    /// The same runner, with `--modify-window=secs` on every pass (0 = off).
    #[must_use]
    pub fn with_modify_window(mut self, secs: u32) -> Self {
        self.modify_window = secs;
        self
    }

    fn common_args(
        &self,
        exclude_from: Option<&Path>,
        backup_dir: &str,
        timeout_secs: u32,
    ) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![
            "-rlt".into(),
            "--no-owner".into(),
            "--no-group".into(),
            format!("--timeout={timeout_secs}").into(),
            format!("--partial-dir={PARTIAL_DIR}").into(),
            "--backup".into(),
            format!("--backup-dir={backup_dir}").into(),
            "--exclude=.lt/cache/".into(),
            format!("--exclude=/{SNAPSHOT_REL}").into(),
            "--exclude=*.partial".into(),
            format!("--exclude={PARTIAL_DIR}/").into(),
            format!("--exclude={LEGACY_PARTIAL_DIR}/").into(),
        ];
        if self.modify_window > 0 {
            a.push(format!("--modify-window={}", self.modify_window).into());
        }
        if let Some(f) = exclude_from {
            let mut arg = OsString::from("--exclude-from=");
            arg.push(f);
            a.push(arg);
        }
        debug_assert!(!a.iter().any(|x| x.to_string_lossy().starts_with("--del")));
        a
    }

    /// Run the archive pass (plus the checksum pass on openrsync).
    ///
    /// Exit 0 is success. Exit 24 (files vanished during the transfer) is a
    /// warning. Exit 23 when every error is on the sending side (unreadable
    /// source file) lists those files in [`SyncResult::skipped`]: the rest was
    /// archived, but the run is incomplete. Anything touching the destination
    /// is [`ArchiveError::Rsync`].
    pub fn sync(
        &self,
        src: &Path,
        dest: &Path,
        excludes: &[String],
        backup_dir: &str,
        progress: &mut dyn FnMut(&RsyncProgress),
        cancel: &AtomicBool,
    ) -> Result<SyncResult, ArchiveError> {
        // The patterns go in a file (`--exclude-from`), one per line: thousands of
        // rejected originals must not grow the command line toward `ARG_MAX`.
        let file = exclude_file(excludes).map_err(|e| ArchiveError::Io {
            path: std::env::temp_dir(),
            source: e,
        })?;
        let from = file.as_ref().map(tempfile::NamedTempFile::path);
        let passes = [
            self.args(src, dest, from, backup_dir),
            self.checksum_pass_args(src, dest, from, backup_dir),
            self.snapshot_pass_args(src, dest, from),
        ];
        let mut result = SyncResult::default();
        for args in passes {
            let spec = CommandSpec {
                program: self.program.clone(),
                args,
            };
            let out = self
                .runner
                .run(
                    &spec,
                    &mut |line| {
                        if let Some(p) = parse_progress(line) {
                            progress(&p);
                        }
                    },
                    cancel,
                )
                .map_err(|e| ArchiveError::Io {
                    path: self.program.clone(),
                    source: e,
                })?;
            if cancel.load(Ordering::Relaxed) {
                return Err(ArchiveError::Cancelled);
            }
            let r = classify_exit(out.code, &out.stderr, src)?;
            for w in r.warnings {
                if !result.warnings.contains(&w) {
                    result.warnings.push(w);
                }
            }
            for f in r.skipped {
                if !result.skipped.contains(&f) {
                    result.skipped.push(f);
                }
            }
        }
        Ok(result)
    }
}

/// `excludes` written to a temporary `--exclude-from` file (None when empty). A
/// pattern with a line break can't be written to it: it is dropped (with a warning),
/// which can only archive a file that would have been excluded, never lose one.
fn exclude_file(excludes: &[String]) -> std::io::Result<Option<tempfile::NamedTempFile>> {
    use std::io::Write;
    if excludes.is_empty() {
        return Ok(None);
    }
    let mut f = tempfile::Builder::new()
        .prefix("lt-archive-excludes-")
        .suffix(".txt")
        .tempfile()?;
    for e in excludes {
        if e.contains(['\n', '\r']) {
            tracing::warn!(pattern = %e.escape_debug(), "exclude pattern with a line break dropped");
            continue;
        }
        writeln!(f, "{e}")?;
    }
    f.flush()?;
    Ok(Some(f))
}

/// rsync `--timeout` for the main pass and the snapshot pass.
const TIMEOUT_SECS: u32 = 60;
/// `--timeout` for the `--checksum` pass, which hashes every sidecar and `.lt/` file on
/// both ends before sending anything.
const CHECKSUM_TIMEOUT_SECS: u32 = 600;

/// Directory (relative to the destination) holding interrupted transfers.
pub const PARTIAL_DIR: &str = ".lt-rsync-partial";

/// Migration source: version 0.1's partial-transfer folder. Interrupted runs may
/// have left it on the target; it is skipped, never removed.
pub const LEGACY_PARTIAL_DIR: &str = ".ds-rsync-partial";

/// Map an rsync exit to warnings, skipped files or an error (see [`RsyncRunner::sync`]).
fn classify_exit(code: Option<i32>, stderr: &str, src: &Path) -> Result<SyncResult, ArchiveError> {
    match code {
        Some(0) => Ok(SyncResult::default()),
        Some(24) => Ok(SyncResult {
            warnings: vec![format!(
                "some source files vanished during transfer: {}",
                tail(stderr)
            )],
            skipped: Vec::new(),
        }),
        Some(23) if only_source_side_errors(stderr, src) => {
            let mut skipped = parse_source_errors(stderr, src);
            if skipped.is_empty() {
                skipped.push(format!(
                    "some source files could not be read: {}",
                    tail(stderr)
                ));
            }
            Ok(SyncResult {
                warnings: Vec::new(),
                skipped,
            })
        }
        code => Err(ArchiveError::Rsync {
            code,
            stderr: tail(stderr),
        }),
    }
}

/// One `"<path relative to src>: <reason>"` per unreadable source file named
/// in rsync's stderr. Understands rsync 3 (`rsync: [sender] send_files failed
/// to open "/src/a.ARW": Permission denied (13)`) and openrsync
/// (`rsync(123): error: /src/a.ARW: open (2) in /x: Permission denied`).
/// Vanished files and summary lines are not listed.
pub fn parse_source_errors(stderr: &str, src: &Path) -> Vec<String> {
    let src = src.to_string_lossy();
    let src = src.trim_end_matches('/');
    let rel = |p: &str| -> String {
        p.strip_prefix(src)
            .map_or(p, |r| r.trim_start_matches('/'))
            .to_owned()
    };
    let mut out: Vec<String> = Vec::new();
    for line in stderr.lines().map(str::trim) {
        if line.is_empty()
            || line.contains("some files/attrs were not transferred")
            || line.contains("some files vanished")
            || line.contains("file has vanished")
            || line.starts_with("rsync error:")
        {
            continue;
        }
        let entry = if let Some(start) = line.find('"') {
            // rsync 3: the path is quoted, the reason follows `": `.
            let rest = &line[start + 1..];
            let Some(end) = rest.find('"') else { continue };
            let path = &rest[..end];
            let reason = rest[end + 1..]
                .trim_start_matches(':')
                .trim()
                .trim_start_matches("failed:")
                .trim();
            let reason = if reason.is_empty() {
                "unreadable"
            } else {
                reason
            };
            format!("{}: {reason}", rel(path))
        } else if let Some(start) = line.find(src).filter(|_| !src.is_empty()) {
            // openrsync: `…: /src/path: op (errno) in …: reason`.
            let rest = &line[start..];
            let path = rest.split(": ").next().unwrap_or(rest);
            let reason = rest.rsplit(": ").next().unwrap_or("unreadable");
            format!("{}: {reason}", rel(path))
        } else {
            continue;
        };
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

/// True when every error line blames the sending side: rsync 3 tags them
/// `[sender]`; openrsync names the source path. Summary lines are ignored.
fn only_source_side_errors(stderr: &str, src: &Path) -> bool {
    let src = src.to_string_lossy();
    let src = src.trim_end_matches('/');
    let mut errors = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            !l.contains("some files/attrs were not transferred")
                && !l.contains("some files vanished")
        })
        .peekable();
    errors.peek().is_some()
        && errors.all(|l| {
            !l.contains("[receiver]")
                && !l.contains("[generator]")
                && (l.contains("[sender]") || l.contains("file has vanished") || l.contains(src))
        })
}

fn with_trailing_slash(p: &Path) -> OsString {
    let mut s = p.as_os_str().to_owned();
    if !s.to_string_lossy().ends_with('/') {
        s.push("/");
    }
    s
}

/// Last few lines of stderr, for error messages.
fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Fake runner: records commands, replays scripted stdout + exit code.
    pub(crate) struct FakeRunner {
        pub calls: Mutex<Vec<CommandSpec>>,
        pub version: String,
        pub lines: Vec<String>,
        pub code: Option<i32>,
        pub stderr: String,
        pub io_error: bool,
    }

    impl FakeRunner {
        pub fn new(version: &str) -> Self {
            Self {
                calls: Mutex::default(),
                version: version.into(),
                lines: vec![],
                code: Some(0),
                stderr: String::new(),
                io_error: false,
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            cmd: &CommandSpec,
            on_line: &mut dyn FnMut(&str),
            _: &AtomicBool,
        ) -> io::Result<CommandOutput> {
            self.calls.lock().unwrap().push(cmd.clone());
            if cmd.args == ["--version"] {
                on_line(&self.version);
                return Ok(CommandOutput {
                    code: Some(0),
                    stderr: String::new(),
                });
            }
            if self.io_error {
                return Err(io::Error::other("spawn failed"));
            }
            for l in &self.lines {
                on_line(l);
            }
            Ok(CommandOutput {
                code: self.code,
                stderr: self.stderr.clone(),
            })
        }
    }

    fn exe(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"").unwrap();
        p
    }

    #[test]
    fn flavor_detection() {
        assert_eq!(
            RsyncFlavor::from_version_output(
                "openrsync: protocol version 29\nrsync version 2.6.9 compatible"
            ),
            RsyncFlavor::Openrsync
        );
        let v3 = RsyncFlavor::from_version_output(
            "rsync  version 3.4.1  protocol version 32\nCopyright",
        );
        assert_eq!(
            v3,
            RsyncFlavor::Rsync3 {
                version: "3.4.1".into()
            }
        );
        assert!(v3.supports_progress2());
        let old = RsyncFlavor::from_version_output("rsync  version 2.6.9  protocol version 29");
        assert_eq!(
            old,
            RsyncFlavor::Legacy {
                version: "2.6.9".into()
            }
        );
        assert!(!old.supports_progress2());
        assert_eq!(
            RsyncFlavor::from_version_output("garbage"),
            RsyncFlavor::Legacy {
                version: String::new()
            }
        );
    }

    #[test]
    fn detect_prefers_first_existing_candidate() {
        let t = tempfile::tempdir().unwrap();
        let brew = t.path().join("brew-rsync");
        let sys = exe(t.path(), "sys-rsync");
        let fake = Arc::new(FakeRunner::new("openrsync: protocol version 29"));
        let r = RsyncRunner::detect_from(fake.clone(), &[brew.clone(), sys.clone()]).unwrap();
        assert_eq!(r.program(), sys);
        assert_eq!(r.flavor(), &RsyncFlavor::Openrsync);
        exe(t.path(), "brew-rsync");
        let fake = Arc::new(FakeRunner::new("rsync  version 3.4.1  protocol version 32"));
        let r = RsyncRunner::detect_from(fake, &[brew.clone(), sys]).unwrap();
        assert_eq!(r.program(), brew);
        assert!(format!("{r:?}").contains("Rsync3"));
        assert!(matches!(
            RsyncRunner::detect_from(Arc::new(FakeRunner::new("")), &[]),
            Err(ArchiveError::RsyncNotFound)
        ));
    }

    #[test]
    fn detect_default_paths_finds_system_rsync() {
        // Every macOS has /usr/bin/rsync; Linux CI may not.
        if Path::new("/usr/bin/rsync").exists() {
            let r = RsyncRunner::detect(Arc::new(SystemRunner)).unwrap();
            assert!(r.program().exists());
        }
    }

    #[test]
    fn args_are_safe_and_flavor_specific() {
        let t = tempfile::tempdir().unwrap();
        let p = exe(t.path(), "rsync");
        let v3 = RsyncRunner::detect_from(
            Arc::new(FakeRunner::new("rsync  version 3.2.7")),
            std::slice::from_ref(&p),
        )
        .unwrap();
        let a: Vec<String> = v3
            .args(
                Path::new("/s/sess"),
                Path::new("/Volumes/pics/Luxtable/sess/"),
                Some(Path::new("/tmp/ex.txt")),
                ".lt-archive-trash/T",
            )
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert_eq!(
            a,
            [
                "-rlt",
                "--no-owner",
                "--no-group",
                "--timeout=60",
                "--partial-dir=.lt-rsync-partial",
                "--backup",
                "--backup-dir=.lt-archive-trash/T",
                "--exclude=.lt/cache/",
                "--exclude=/.lt/index-snapshot.db",
                "--exclude=*.partial",
                "--exclude=.lt-rsync-partial/",
                "--exclude=.ds-rsync-partial/",
                "--exclude-from=/tmp/ex.txt",
                "--info=progress2",
                "--",
                "/s/sess/",
                "/Volumes/pics/Luxtable/sess/",
            ]
        );
        // Never a nanosecond window; the checksum pass runs on rsync 3 too.
        assert!(!a.iter().any(|x| x.starts_with("--modify-window")));
        assert!(
            v3.checksum_pass_args(Path::new("/a"), Path::new("/b"), None, "x")
                .contains(&"--checksum".into())
        );
        // A FAT target gets a 1 s window on both passes.
        let fat = v3.clone().with_modify_window(1);
        assert!(
            fat.args(Path::new("/a"), Path::new("/b"), None, "x")
                .contains(&"--modify-window=1".into())
        );
        assert!(
            fat.checksum_pass_args(Path::new("/a"), Path::new("/b"), None, "x")
                .contains(&"--modify-window=1".into())
        );
        assert!(format!("{fat:?}").contains("modify_window: 1"));
        let open = RsyncRunner::detect_from(Arc::new(FakeRunner::new("openrsync")), &[p]).unwrap();
        let a = open.args(Path::new("/a"), Path::new("/b"), None, "x");
        assert!(
            !a.iter()
                .any(|x| x == "--info=progress2" || x == "--modify-window=-1")
        );
        assert!(!a.iter().any(|x| x.to_string_lossy().contains("delete")));
        assert!(!a.iter().any(|x| x == "-a" || x == "--partial"));
        let cs: Vec<String> = open
            .checksum_pass_args(Path::new("/a"), Path::new("/b"), None, "x")
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        let pos = |x: &str| cs.iter().position(|a| a == x).unwrap();
        assert!(cs.contains(&"--checksum".to_owned()));
        // Excludes (cache) must precede the .lt/** include; catch-all exclude last.
        assert!(pos("--exclude=.lt/cache/") < pos("--include=.lt/**"));
        assert!(pos("--include=*.json") < pos("--exclude=*"));
        // Paths after "--": a name starting with "-" is never read as an option.
        assert_eq!(&cs[cs.len() - 3..], ["--", "/a/", "/b/"]);
        // The slow-to-hash checksum pass tolerates a longer silence.
        assert!(cs.contains(&"--timeout=600".to_owned()));
        assert!(!cs.contains(&"--timeout=60".to_owned()));
        // The snapshot is never backed up; its own pass replaces it in place.
        for pass in [
            open.args(Path::new("/a"), Path::new("/b"), None, "x"),
            open.checksum_pass_args(Path::new("/a"), Path::new("/b"), None, "x"),
        ] {
            assert!(pass.contains(&"--exclude=/.lt/index-snapshot.db".into()));
        }
        let sn: Vec<String> = open
            .snapshot_pass_args(Path::new("/a"), Path::new("/b"), Some(Path::new("/ex")))
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert!(!sn.iter().any(|x| x.starts_with("--backup")), "{sn:?}");
        let pos = |x: &str| sn.iter().position(|a| a == x).unwrap();
        assert!(pos("--exclude-from=/ex") < pos("--include=/.lt/index-snapshot.db"));
        assert!(pos("--include=/.lt/index-snapshot.db") < pos("--exclude=*"));
        assert_eq!(&sn[sn.len() - 3..], ["--", "/a/", "/b/"]);
        let sn = open.clone().with_modify_window(2);
        assert!(
            sn.snapshot_pass_args(Path::new("/a"), Path::new("/b"), None)
                .contains(&"--modify-window=2".into())
        );
    }

    #[test]
    fn exit_classification() {
        let src = Path::new("/s/sess");
        assert_eq!(
            classify_exit(Some(0), "", src).unwrap(),
            SyncResult::default()
        );
        assert_eq!(
            classify_exit(Some(24), "file has vanished: x", src)
                .unwrap()
                .warnings
                .len(),
            1
        );
        // rsync 3: unreadable source file.
        let v3 = "rsync: [sender] send_files failed to open \"/s/sess/b.ARW\": Permission denied (13)\n\
                  rsync error: some files/attrs were not transferred (see previous errors) (code 23) at main.c(1412) [sender=3.5.1]";
        let r = classify_exit(Some(23), v3, src).unwrap();
        assert!(
            r.warnings.is_empty(),
            "an unreadable original is not a mere warning"
        );
        assert_eq!(r.skipped, ["b.ARW: Permission denied (13)"]);
        // openrsync: names the source path.
        let open = "rsync(87962): error: /s/sess/b.ARW: open (2) in /x: Permission denied";
        assert_eq!(
            classify_exit(Some(23), open, src).unwrap().skipped,
            ["b.ARW: Permission denied"]
        );
        // Several files and a folder; duplicates collapse.
        let many = "rsync: [sender] send_files failed to open \"/s/sess/Capture/a.ARW\": Permission denied (13)\n\
                    rsync: [sender] opendir \"/s/sess/Private\" failed: Permission denied (13)\n\
                    rsync: [sender] send_files failed to open \"/s/sess/Capture/a.ARW\": Permission denied (13)";
        assert_eq!(
            parse_source_errors(many, src),
            [
                "Capture/a.ARW: Permission denied (13)",
                "Private: Permission denied (13)"
            ]
        );
        // A line that names no file keeps the stderr tail, so nothing is silent.
        let odd = "rsync: [sender] something odd happened";
        let r = classify_exit(Some(23), odd, src).unwrap();
        assert!(r.skipped[0].contains("something odd"), "{r:?}");
        assert_eq!(
            parse_source_errors("rsync: [sender] \"unterminated", src),
            [] as [std::string::String; 0]
        );
        assert_eq!(
            parse_source_errors("rsync: [sender] x \"/s/sess/q\"", src),
            ["q: unreadable"]
        );
        // Destination-side trouble is an error.
        for bad in [
            "rsync: write failed on \"/Volumes/pics/x\": No space left on device (28)",
            "rsync: [receiver] mkstemp failed: Permission denied (13)",
            "rsync: [sender] send_files failed to open \"/s/sess/a\"\nrsync: [generator] failed to set times",
            "",
        ] {
            assert!(
                matches!(
                    classify_exit(Some(23), bad, src),
                    Err(ArchiveError::Rsync { code: Some(23), .. })
                ),
                "{bad}"
            );
        }
        assert!(classify_exit(Some(12), open, src).is_err());
        assert!(classify_exit(None, "", src).is_err());
    }

    #[test]
    fn progress_parsing() {
        let p = parse_progress("      1,238,099  45%  115.13MB/s    0:00:10 (xfr#5, to-chk=10/20)")
            .unwrap();
        assert_eq!(p.bytes, 1_238_099);
        assert_eq!(p.percent, 45);
        assert!((p.bytes_per_sec - 115.13e6).abs() < 1.0);
        assert_eq!(p.eta, Some(Duration::from_secs(10)));
        assert_eq!(p.files_transferred, Some(5));
        assert_eq!(p.to_check, Some((10, 20)));
        let p = parse_progress("  32,768 100%   31.25kB/s    1:02:03 (xfr#1, ir-chk=1000/1005)")
            .unwrap();
        assert_eq!(p.eta, Some(Duration::from_secs(3723)));
        assert_eq!(p.to_check, Some((1000, 1005)));
        let p = parse_progress("0   0%    0.00B/s    0:00:00").unwrap();
        assert_eq!((p.files_transferred, p.to_check), (None, None));
        assert!((parse_progress("5 1% 2.5GB/s").unwrap().bytes_per_sec - 2.5e9).abs() < 1.0);
        for junk in [
            "sending incremental file list",
            "Capture/DSC0001.ARW",
            "",
            "12 abc",
            "1 5% fast",
            "1 5% 3XB/s",
            "1 5% 3/s",
        ] {
            assert!(parse_progress(junk).is_none(), "{junk}");
        }
    }

    #[test]
    fn sync_maps_exit_codes_and_reports_progress() {
        let t = tempfile::tempdir().unwrap();
        let p = exe(t.path(), "rsync");
        let mut fake = FakeRunner::new("rsync  version 3.4.1");
        fake.lines = vec![
            "file.ARW".into(),
            "  100 50%  1.00MB/s 0:00:01 (xfr#1, to-chk=1/2)".into(),
        ];
        let r = RsyncRunner::detect_from(Arc::new(fake), std::slice::from_ref(&p)).unwrap();
        let mut seen = vec![];
        let ok = r
            .sync(
                Path::new("/a"),
                Path::new("/b"),
                &[],
                "x",
                &mut |p| seen.push(p.bytes),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(ok.warnings, [] as [std::string::String; 0]);
        assert_eq!(seen, [100, 100, 100], "main, checksum and snapshot passes");

        let run = |code, io_error, cancel: bool| {
            let mut f = FakeRunner::new("rsync  version 3.4.1");
            f.code = code;
            f.io_error = io_error;
            f.stderr =
                "l1\nl2\nl3\nl4\nl5\nrsync error: some files could not be transferred (code 23)"
                    .into();
            let r = RsyncRunner::detect_from(Arc::new(f), std::slice::from_ref(&p)).unwrap();
            r.sync(
                Path::new("/a"),
                Path::new("/b"),
                &[],
                "x",
                &mut |_| {},
                &AtomicBool::new(cancel),
            )
        };
        assert_eq!(run(Some(24), false, false).unwrap().warnings.len(), 1);
        match run(Some(23), false, false) {
            Err(ArchiveError::Rsync {
                code: Some(23),
                stderr,
            }) => {
                assert!(
                    stderr.contains("code 23") && !stderr.contains("l1"),
                    "{stderr}"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            run(None, false, false),
            Err(ArchiveError::Rsync { code: None, .. })
        ));
        assert!(matches!(
            run(Some(0), true, false),
            Err(ArchiveError::Io { .. })
        ));
        assert!(matches!(
            run(Some(0), false, true),
            Err(ArchiveError::Cancelled)
        ));
    }

    #[test]
    fn system_runner_streams_lines_and_codes() {
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "printf 'a\\rb\\nc'; echo err >&2; exit 3".into(),
            ],
        };
        let mut lines = vec![];
        let out = SystemRunner
            .run(
                &spec,
                &mut |l| lines.push(l.to_owned()),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(lines, ["a", "b", "c"]);
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stderr.trim(), "err");
        let missing = CommandSpec {
            program: "/nonexistent/rsync".into(),
            args: vec![],
        };
        assert!(
            SystemRunner
                .run(&missing, &mut |_| {}, &AtomicBool::new(false))
                .is_err()
        );
    }

    #[test]
    fn system_runner_kills_on_cancel() {
        let spec = CommandSpec {
            program: "/bin/sleep".into(),
            args: vec!["30".into()],
        };
        let t0 = std::time::Instant::now();
        let out = SystemRunner
            .run(&spec, &mut |_| {}, &AtomicBool::new(true))
            .unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert_eq!(out.code, None, "killed by signal");
    }
}
