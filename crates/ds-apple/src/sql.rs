//! Reading a library's SQLite databases without touching them. A database is
//! never opened in place: it is copied (with a `-wal`/`-journal` beside it)
//! into a temporary directory and the copy is read, so nothing inside the
//! library is opened for writing, locked, or given a `-shm` file, and a
//! database Aperture or iPhoto left locked or mid-transaction still reads.
//!
//! The schema is Apple's private one, known from open-source readers (see
//! `docs/APPLE_FORMAT.md`): only real tables are queried (a view in a hostile
//! file could run any query), every column beyond the essentials is
//! feature-detected, the read has a time limit, and each cell is read by its
//! actual SQLite type so a wrong type is "not set", never an error.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};

use crate::error::{AppleError, IoAt, Result};
use crate::limits::{MAX_DB_BYTES, MAX_TEXT, cap_text};

/// Polls `cancel` and the deadline.
pub(crate) struct Ctx<'a> {
    pub path: &'a Path,
    pub cancel: &'a (dyn Fn() -> bool + Sync),
    pub deadline: Instant,
}

impl Ctx<'_> {
    pub fn err(&self, e: &rusqlite::Error) -> AppleError {
        self.check().err().unwrap_or_else(|| AppleError::Db {
            path: self.path.to_owned(),
            message: e.to_string(),
        })
    }

    pub fn check(&self) -> Result<()> {
        if (self.cancel)() {
            Err(AppleError::Cancelled)
        } else if Instant::now() > self.deadline {
            Err(AppleError::Db {
                path: self.path.to_owned(),
                message: "took too long to read: not read".into(),
            })
        } else {
            Ok(())
        }
    }
}

/// Copy `db` (and a `-wal` or `-journal` beside it) into `dir`; the copy's path.
pub(crate) fn copy_db(db: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let md = fs::symlink_metadata(db).at(db)?;
    let refuse = |message: String| AppleError::Db {
        path: db.to_owned(),
        message,
    };
    if !md.is_file() {
        return Err(refuse("not a regular file".into()));
    }
    if md.len() > MAX_DB_BYTES {
        return Err(refuse(format!("too large to copy ({} bytes)", md.len())));
    }
    let copy = dir.join(name);
    fs::copy(db, &copy).at(db)?;
    for suffix in ["-journal", "-wal"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        match fs::symlink_metadata(&side) {
            Ok(m) if m.is_file() && m.len() <= MAX_DB_BYTES => {
                let to = PathBuf::from(format!("{}{suffix}", copy.display()));
                fs::copy(&side, &to).at(&side)?;
            }
            _ => {}
        }
    }
    Ok(copy)
}

/// Open a copy made by [`copy_db`]; its real tables.
pub(crate) fn open(copy: &Path, ctx: &Ctx) -> Result<(Connection, HashSet<String>)> {
    let conn = Connection::open_with_flags(copy, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| ctx.err(&e))?;
    // A hostile file's views and triggers must not call SQL functions.
    conn.execute_batch("PRAGMA trusted_schema=OFF")
        .map_err(|e| ctx.err(&e))?;
    let deadline = ctx.deadline;
    conn.progress_handler(1000, Some(move || Instant::now() > deadline));
    let tables = {
        let mut st = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .map_err(|e| ctx.err(&e))?;
        #[allow(clippy::let_and_return)] // the iterator borrows `st`
        let names = st
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| ctx.err(&e))?
            .collect::<rusqlite::Result<HashSet<String>>>()
            .map_err(|e| ctx.err(&e))?;
        names
    };
    conn.execute_batch("PRAGMA query_only = ON")
        .map_err(|e| ctx.err(&e))?;
    Ok((conn, tables))
}

/// One database being read.
pub(crate) struct Db<'a> {
    pub ctx: Ctx<'a>,
    pub conn: Connection,
    pub tables: HashSet<String>,
}

impl Db<'_> {
    pub fn has(&self, table: &str) -> bool {
        self.tables.contains(table)
    }

    fn columns(&self, table: &str) -> Result<HashSet<String>> {
        // `table` is one of this crate's constants, never a name from the file.
        let mut st = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|e| self.ctx.err(&e))?;
        let names = st
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| self.ctx.err(&e))?
            .collect::<rusqlite::Result<HashSet<String>>>()
            .map_err(|e| self.ctx.err(&e))?;
        Ok(names)
    }

    /// `SELECT` the `wanted` columns of `table` in order, `NULL` for those it lacks.
    pub fn select(&self, table: &str, wanted: &[&str]) -> Result<String> {
        let have = self.columns(table)?;
        let cols: Vec<&str> = wanted
            .iter()
            .map(|c| if have.contains(*c) { *c } else { "NULL" })
            .collect();
        Ok(format!("SELECT {} FROM {table}", cols.join(", ")))
    }

    /// Whether `table` has every column in `cols`.
    pub fn has_columns(&self, table: &str, cols: &[&str]) -> Result<bool> {
        if !self.has(table) {
            return Ok(false);
        }
        let have = self.columns(table)?;
        Ok(cols.iter().all(|c| have.contains(*c)))
    }

    /// Run `sql`, calling `f` per row; checks for cancellation every 1000 rows.
    pub fn each(&self, sql: &str, mut f: impl FnMut(&Row) -> Result<()>) -> Result<()> {
        self.ctx.check()?;
        let mut st = self.conn.prepare(sql).map_err(|e| self.ctx.err(&e))?;
        let mut rows = st.query([]).map_err(|e| self.ctx.err(&e))?;
        let mut n = 0usize;
        while let Some(r) = rows.next().map_err(|e| self.ctx.err(&e))? {
            n += 1;
            if n.is_multiple_of(1000) {
                self.ctx.check()?;
            }
            f(r)?;
        }
        Ok(())
    }
}

/// Run `read` while a watcher thread interrupts the database when `cancel` turns true.
pub(crate) fn watched<T>(
    conn: &Connection,
    cancel: &(dyn Fn() -> bool + Sync),
    read: impl FnOnce() -> T,
) -> T {
    let handle = conn.get_interrupt_handle();
    let finished = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !finished.load(std::sync::atomic::Ordering::Relaxed) {
                if cancel() {
                    handle.interrupt();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let out = read();
        finished.store(true, std::sync::atomic::Ordering::Relaxed);
        out
    })
}

// ----------------------------------------------------------- cell getters ---

pub(crate) fn int_at(r: &Row, i: usize) -> Option<i64> {
    match r.get_ref(i).ok()? {
        ValueRef::Integer(v) => Some(v),
        ValueRef::Real(f) if f.is_finite() && f.abs() < 9e18 => Some(f as i64),
        ValueRef::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn real_at(r: &Row, i: usize) -> Option<f64> {
    match r.get_ref(i).ok()? {
        ValueRef::Integer(v) => Some(v as f64),
        ValueRef::Real(f) => Some(f).filter(|f| f.is_finite()),
        ValueRef::Text(t) => std::str::from_utf8(t)
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite()),
        _ => None,
    }
}

/// A cell as text, cut to `MAX_TEXT` bytes: a hostile file can hold gigabyte cells.
pub(crate) fn text_at(r: &Row, i: usize) -> Option<String> {
    let s = match r.get_ref(i).ok()? {
        ValueRef::Text(t) => String::from_utf8_lossy(&t[..t.len().min(2 * MAX_TEXT)]).into_owned(),
        ValueRef::Integer(v) => v.to_string(),
        _ => return None,
    };
    Some(cap_text(s))
}

pub(crate) fn blob_at(r: &Row, i: usize) -> Option<Vec<u8>> {
    match r.get_ref(i).ok()? {
        ValueRef::Blob(b) => Some(b.to_vec()),
        _ => None,
    }
}

/// A 0/1 flag; `None` when unset.
pub(crate) fn flag_at(r: &Row, i: usize) -> Option<bool> {
    int_at(r, i).map(|v| v != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_are_read_by_their_type() {
        let c = Connection::open_in_memory().unwrap();
        let mut st = c
            .prepare("SELECT 3, 2.5, '7', ' 1.5 ', NULL, x'01', 1e300 * 1e300, 'x', 9e18 * 10")
            .unwrap();
        let mut rows = st.query([]).unwrap();
        let r = rows.next().unwrap().unwrap();
        assert_eq!(int_at(r, 0), Some(3));
        assert_eq!(int_at(r, 1), Some(2));
        assert_eq!(int_at(r, 2), Some(7));
        assert_eq!(int_at(r, 4), None);
        assert_eq!(int_at(r, 8), None);
        assert_eq!(real_at(r, 0), Some(3.0));
        assert_eq!(real_at(r, 3), Some(1.5));
        assert_eq!(real_at(r, 6), None, "infinite");
        assert_eq!(real_at(r, 5), None);
        assert_eq!(text_at(r, 0).as_deref(), Some("3"));
        assert_eq!(text_at(r, 7).as_deref(), Some("x"));
        assert_eq!(text_at(r, 1), None);
        assert_eq!(blob_at(r, 5), Some(vec![1]));
        assert_eq!(blob_at(r, 0), None);
        assert_eq!(flag_at(r, 0), Some(true));
        assert_eq!(flag_at(r, 4), None);
    }

    #[test]
    fn only_regular_files_are_copied() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        assert!(copy_db(d.path(), out.path(), "x").is_err(), "a folder");
        assert!(copy_db(&d.path().join("missing"), out.path(), "x").is_err());
        let db = d.path().join("Library.apdb");
        fs::write(&db, b"db").unwrap();
        fs::write(d.path().join("Library.apdb-wal"), b"wal").unwrap();
        let copy = copy_db(&db, out.path(), "copy.apdb").unwrap();
        assert_eq!(fs::read(&copy).unwrap(), b"db");
        assert_eq!(fs::read(out.path().join("copy.apdb-wal")).unwrap(), b"wal");
    }
}
