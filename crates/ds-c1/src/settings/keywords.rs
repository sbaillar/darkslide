//! Keyword libraries (`Keywords/`). The real folder is empty, so the file
//! format is **unconfirmed**. Two readable forms are accepted:
//! - a SQLite file with C1's `ZKEYWORD` table (`ZNAME`, `ZPARENT`), the same
//!   table a session database holds its library in. It is copied to a
//!   temporary directory and read there, never opened in place;
//! - a Lightroom keyword text file (C1 imports these): one keyword per line,
//!   tab-indented under its parent, `[Name]` a category, `{Name}` a synonym.
//!
//! Darkslide's keyword list is flat, so a library becomes its keyword names
//! (parents included), without synonyms, in order, each name once.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

use super::{Cancel, MAX_SETTINGS_FILE_BYTES, fingerprint, read_bytes};
use crate::db::copy_side_file;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordLibraryImport {
    /// The file's name (the import's key).
    pub file: String,
    /// The library's name (the file name without its extension).
    pub name: String,
    pub keywords: Vec<String>,
    pub fingerprint: String,
}

const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";
/// A SQLite database's journal files, copied with it.
const SIDECARS: [&str; 2] = ["-journal", "-wal"];

/// A SQLite journal file beside a database (`x.db-wal`, `x.db-shm`, `x.db-journal`).
pub(crate) fn is_db_sidecar(p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ["-journal", "-wal", "-shm"]
        .iter()
        .any(|s| name.ends_with(s))
}

pub(crate) fn read(path: &Path, cancel: &Cancel) -> Result<KeywordLibraryImport, String> {
    let bytes = read_bytes(path)?;
    let names = if bytes.starts_with(SQLITE_MAGIC) {
        from_db(path, cancel)?
    } else {
        let text = String::from_utf8(bytes.clone())
            .map_err(|_| "Not a keyword library Darkslide can read".to_owned())?;
        from_text(text.trim_start_matches('\u{feff}'))
    };
    let mut keywords: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for n in names {
        let n = n.trim();
        if !n.is_empty() && seen.insert(n.to_lowercase()) {
            keywords.push(n.to_owned());
        }
    }
    if keywords.is_empty() {
        return Err("No keywords in this library".into());
    }
    let stem = |p: &Path| p.file_stem().map(|s| s.to_string_lossy().into_owned());
    Ok(KeywordLibraryImport {
        file: path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        name: stem(path).unwrap_or_default(),
        keywords,
        fingerprint: fingerprint(&bytes),
    })
}

/// Names in a `ZKEYWORD` table, parents before children (nested-set order
/// when `ZLEFT` is there, else row order).
/// How long a keyword database may take to read.
const DB_TIME_LIMIT: Duration = Duration::from_secs(5);

fn from_db(path: &Path, cancel: &Cancel) -> Result<Vec<String>, String> {
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let copy = tmp.path().join("library.db");
    fs::copy(path, &copy).map_err(|e| e.to_string())?;
    for suffix in SIDECARS {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        let to = PathBuf::from(format!("{}{suffix}", copy.display()));
        copy_side_file(&side, &to, MAX_SETTINGS_FILE_BYTES).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA trusted_schema=OFF")
        .map_err(|e| e.to_string())?;
    // Every query stops on cancel or after the time limit (a hostile file can
    // make one run forever).
    let deadline = Instant::now() + DB_TIME_LIMIT;
    let stop = cancel.clone();
    conn.progress_handler(1000, Some(move || stop() || Instant::now() > deadline));
    let interrupted = |e: rusqlite::Error| {
        if cancel() {
            "Cancelled".to_owned()
        } else if Instant::now() > deadline {
            format!(
                "Took longer than {} s to read: not read",
                DB_TIME_LIMIT.as_secs()
            )
        } else {
            e.to_string()
        }
    };
    // Only a real table: a view could run any query.
    let is_table: bool = conn
        .query_row(
            "SELECT count(*) = 1 FROM sqlite_master WHERE type = 'table' AND name = 'ZKEYWORD'",
            [],
            |r| r.get(0),
        )
        .map_err(interrupted)?;
    if !is_table {
        return Err("Not a Capture One keyword library (no ZKEYWORD table)".into());
    }
    let has_left = conn.prepare("SELECT ZLEFT FROM ZKEYWORD LIMIT 0").is_ok();
    let sql = if has_left {
        "SELECT ZNAME FROM ZKEYWORD WHERE ZNAME IS NOT NULL ORDER BY ZLEFT, Z_PK"
    } else {
        "SELECT ZNAME FROM ZKEYWORD WHERE ZNAME IS NOT NULL ORDER BY Z_PK"
    };
    let mut stmt = conn
        .prepare(sql)
        .map_err(|_| "Not a Capture One keyword library (no ZKEYWORD table)".to_owned())?;
    stmt.query_map([], |r| r.get::<_, String>(0))
        .and_then(Iterator::collect)
        .map_err(interrupted)
}

/// A Lightroom keyword list: `[Category]` keeps its name, `{Synonym}` is left out.
fn from_text(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim_matches(|c: char| c == '\t' || c == '\r' || c == ' '))
        .filter(|l| !(l.starts_with('{') && l.ends_with('}')))
        .map(|l| {
            l.strip_prefix('[')
                .and_then(|l| l.strip_suffix(']'))
                .unwrap_or(l)
                .to_owned()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn no() -> Cancel {
        Arc::new(|| false)
    }

    /// A ZKEYWORD view over a query that never ends: refused, never run.
    #[test]
    fn views_and_endless_queries_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("View.db");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE VIEW ZKEYWORD AS WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n)
             SELECT x AS Z_PK, 'k' AS ZNAME FROM n;",
        )
        .unwrap();
        drop(c);
        let start = Instant::now();
        assert!(read(&p, &no()).unwrap_err().contains("no ZKEYWORD table"));
        assert!(start.elapsed() < Duration::from_secs(2));

        // A real table whose rows are fine but whose read is cancelled stops at once.
        let t = dir.path().join("Big.db");
        let c = Connection::open(&t).unwrap();
        c.execute_batch(
            "CREATE TABLE ZKEYWORD (Z_PK INTEGER PRIMARY KEY, ZNAME VARCHAR);
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n LIMIT 50000)
             INSERT INTO ZKEYWORD SELECT x, 'k' || x FROM n;",
        )
        .unwrap();
        drop(c);
        let cancelled: Cancel = Arc::new(|| true);
        assert_eq!(read(&t, &cancelled).unwrap_err(), "Cancelled");
        assert_eq!(read(&t, &no()).unwrap().keywords.len(), 50_000);
    }

    /// A journal beside the library that is a link is refused, not followed.
    #[test]
    fn linked_journals_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Lib.db");
        Connection::open(&p)
            .unwrap()
            .execute_batch("CREATE TABLE ZKEYWORD (Z_PK INTEGER PRIMARY KEY, ZNAME VARCHAR); INSERT INTO ZKEYWORD VALUES (1, 'k');")
            .unwrap();
        assert_eq!(read(&p, &no()).unwrap().keywords, vec!["k"]);
        std::os::unix::fs::symlink("/dev/zero", dir.path().join("Lib.db-wal")).unwrap();
        let e = read(&p, &no()).unwrap_err();
        assert!(e.contains("-wal: not a regular file"), "{e}");
    }

    #[test]
    fn lightroom_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Travel.txt");
        fs::write(
            &p,
            "\u{feff}[Places]\n\tCoast\n\t\tHarbour\n\t\t{Port}\nbeach\nBeach\n\n\tsunset\r\n",
        )
        .unwrap();
        let k = read(&p, &no()).unwrap();
        assert_eq!(k.name, "Travel");
        assert_eq!(k.file, "Travel.txt");
        assert_eq!(
            k.keywords,
            ["Places", "Coast", "Harbour", "beach", "sunset"]
        );
        assert_eq!(k.fingerprint.len(), 16);
    }

    #[test]
    fn sqlite_library() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Studio.cokeywords");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE ZKEYWORD (Z_ENT INTEGER, Z_PK INTEGER PRIMARY KEY, ZNAME VARCHAR, ZPARENT INTEGER, ZLEFT INTEGER, ZRIGHT INTEGER);
             INSERT INTO ZKEYWORD VALUES (44, 1, 'People', NULL, 1, 6);
             INSERT INTO ZKEYWORD VALUES (44, 3, 'Smith, John', 1, 4, 5);
             INSERT INTO ZKEYWORD VALUES (44, 2, 'Family', 1, 2, 3);
             INSERT INTO ZKEYWORD VALUES (44, 4, NULL, NULL, 7, 8);",
        )
        .unwrap();
        drop(c);
        let before = fs::read(&p).unwrap();
        let k = read(&p, &no()).unwrap();
        assert_eq!(k.keywords, ["People", "Family", "Smith, John"]);
        assert_eq!(
            fs::read(&p).unwrap(),
            before,
            "read from a copy, never in place"
        );
        // No nested set: row order.
        let p2 = dir.path().join("Flat.db");
        let c = Connection::open(&p2).unwrap();
        c.execute_batch(
            "CREATE TABLE ZKEYWORD (Z_PK INTEGER PRIMARY KEY, ZNAME VARCHAR);
             INSERT INTO ZKEYWORD VALUES (2, 'b'); INSERT INTO ZKEYWORD VALUES (1, 'a');",
        )
        .unwrap();
        drop(c);
        assert_eq!(read(&p2, &no()).unwrap().keywords, ["a", "b"]);
        assert!(is_db_sidecar(Path::new("/k/Flat.db-wal")) && is_db_sidecar(Path::new("x-shm")));
        assert!(!is_db_sidecar(&p2));
        // A write-ahead log beside the file is copied with it: its rows are read too.
        let p3 = dir.path().join("Wal.db");
        let c = Connection::open(&p3).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
             CREATE TABLE ZKEYWORD (Z_PK INTEGER PRIMARY KEY, ZNAME VARCHAR);
             INSERT INTO ZKEYWORD VALUES (1, 'only in the wal');",
        )
        .unwrap();
        assert!(dir.path().join("Wal.db-wal").is_file());
        assert_eq!(read(&p3, &no()).unwrap().keywords, ["only in the wal"]);
        drop(c);
    }

    #[test]
    fn unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.db");
        let c = Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE OTHER (A INTEGER);").unwrap();
        drop(c);
        assert!(read(&p, &no()).unwrap_err().contains("no ZKEYWORD"));
        let bin = dir.path().join("x.bin");
        fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
        assert!(read(&bin, &no()).unwrap_err().contains("can read"));
        let empty = dir.path().join("e.txt");
        fs::write(&empty, "{only a synonym}\n").unwrap();
        assert!(read(&empty, &no()).unwrap_err().contains("No keywords"));
        assert!(read(&dir.path().join("missing.txt"), &no()).is_err());
    }
}
