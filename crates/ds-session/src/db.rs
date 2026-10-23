//! The session's SQLite index: schema, validation, and recovery.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::error::{IoContext, Result, SessionError};

/// Bump whenever the index schema changes; older indexes are rebuilt from sidecars.
pub const INDEX_SCHEMA_VERSION: i64 = 9;

const SCHEMA: &str = "
CREATE TABLE images(
    id INTEGER PRIMARY KEY,
    rel_path TEXT NOT NULL UNIQUE,
    folder INTEGER NOT NULL,
    file_name_lc TEXT NOT NULL,
    hash BLOB NOT NULL,
    file_size INTEGER NOT NULL,
    file_mtime INTEGER,
    file_ino INTEGER,
    as_shot_temp REAL,
    as_shot_tint REAL,
    capture_ms INTEGER,
    camera TEXT,
    camera_lc TEXT,
    lens TEXT,
    lens_lc TEXT,
    iso INTEGER,
    focal REAL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    orientation INTEGER NOT NULL,
    missing INTEGER NOT NULL DEFAULT 0,
    content_hash BLOB,
    sample_hash BLOB,
    file_changed INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX images_hash ON images(hash);
CREATE INDEX images_capture ON images(capture_ms);
CREATE TABLE variants(
    id INTEGER PRIMARY KEY,
    image_id INTEGER NOT NULL REFERENCES images(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    name TEXT NOT NULL,
    rating INTEGER NOT NULL,
    color_tag INTEGER NOT NULL,
    pick INTEGER NOT NULL,
    edited INTEGER NOT NULL,
    custom INTEGER NOT NULL DEFAULT 0,
    keywords TEXT NOT NULL,
    search TEXT NOT NULL,
    sidecar TEXT NOT NULL UNIQUE,
    sidecar_mtime INTEGER NOT NULL,
    sidecar_size INTEGER NOT NULL,
    read_only INTEGER NOT NULL,
    iptc TEXT NOT NULL DEFAULT '',
    geo_lat REAL,
    geo_lon REAL,
    geo_alt REAL,
    cull_note TEXT,
    UNIQUE(image_id, ordinal)
);
CREATE INDEX variants_image ON variants(image_id);
CREATE INDEX variants_rating ON variants(rating);
CREATE TABLE album_members(
    album_id INTEGER NOT NULL,
    variant_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY(album_id, variant_id)
) WITHOUT ROWID;
CREATE TABLE embeddings(
    key BLOB NOT NULL,
    model TEXT NOT NULL,
    vec BLOB NOT NULL,
    PRIMARY KEY(key, model)
) WITHOUT ROWID;
CREATE INDEX images_content ON images(content_hash);
";

/// v6 (Find): detected faces and their vectors (derived data, keyed by content
/// identity like the search embeddings), which originals were scanned, the
/// user's same/different-person links between faces, each variant's people
/// tags and rejections (mirrored from its sidecar), and each original's place
/// (location fields, GPS; derived). Idempotent.
const PEOPLE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS faces(
    key BLOB NOT NULL,
    model TEXT NOT NULL,
    idx INTEGER NOT NULL,
    x REAL NOT NULL,
    y REAL NOT NULL,
    w REAL NOT NULL,
    h REAL NOT NULL,
    quality REAL NOT NULL,
    vec BLOB NOT NULL,
    PRIMARY KEY(key, model, idx)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS face_scans(
    key BLOB NOT NULL,
    model TEXT NOT NULL,
    PRIMARY KEY(key, model)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS face_links(
    a_key BLOB NOT NULL,
    a_idx INTEGER NOT NULL,
    b_key BLOB NOT NULL,
    b_idx INTEGER NOT NULL,
    must INTEGER NOT NULL,
    PRIMARY KEY(a_key, a_idx, b_key, b_idx)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS variant_people(
    variant_id INTEGER NOT NULL REFERENCES variants(id) ON DELETE CASCADE,
    ord INTEGER NOT NULL,
    name TEXT NOT NULL,
    person_id TEXT,
    x REAL,
    y REAL,
    w REAL,
    h REAL,
    rejected INTEGER NOT NULL,
    PRIMARY KEY(variant_id, ord)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS locations(
    key BLOB NOT NULL PRIMARY KEY,
    names TEXT NOT NULL,
    lat REAL,
    lon REAL,
    stamp TEXT NOT NULL
) WITHOUT ROWID;
";

/// Upgrade to v6 in place (new tables only; sidecars written before v6 have
/// no people, so the empty tables are already right). In the merged history
/// v4 is look-and-match's and v5 nl-search's; this runs after them.
const TO_V6: &str = "BEGIN IMMEDIATE; {PEOPLE} PRAGMA user_version=6; COMMIT;";

/// Upgrade a v6 index to v7: `variants.iptc` (title, caption, credits… as JSON) and
/// the title and caption in `variants.search` are filled by re-reading every sidecar
/// on the sync that follows: their recorded mtimes are invalidated. Run by [`to_v7`]
/// only when the column is missing.
const V6_TO_V7: &str = "ALTER TABLE variants ADD COLUMN iptc TEXT NOT NULL DEFAULT '';
    UPDATE variants SET sidecar_mtime = -1;";

/// Upgrade a v2 index in place (new columns only; values come with the next
/// sidecar re-index or verification).
const V2_TO_V3: &str = "BEGIN;
    ALTER TABLE images ADD COLUMN content_hash BLOB;
    ALTER TABLE images ADD COLUMN sample_hash BLOB;
    ALTER TABLE images ADD COLUMN file_changed INTEGER NOT NULL DEFAULT 0;
    PRAGMA user_version=3;
    COMMIT;";

/// Upgrade a v3 index to v4: `variants.custom` (0: the default adjustments; 2:
/// exactly the default with the Neutral profile, every untouched sidecar from before
/// profiles; 1: anything else) is filled by re-reading every sidecar on the sync that
/// follows: their recorded mtimes are invalidated. The originals' verified stats are kept.
/// Run by [`to_v4`] only when the column is missing.
const V3_TO_V4: &str = "ALTER TABLE variants ADD COLUMN custom INTEGER NOT NULL DEFAULT 0;
    UPDATE variants SET sidecar_mtime = -1;";

/// Upgrade a v4 index in place: search embeddings (f16, keyed by content
/// identity, see `embed.rs`) are derived data, so the table starts empty.
/// Idempotent: two openers racing on a v4 index both run it.
const V4_TO_V5: &str = "BEGIN IMMEDIATE;
    CREATE TABLE IF NOT EXISTS embeddings(
        key BLOB NOT NULL,
        model TEXT NOT NULL,
        vec BLOB NOT NULL,
        PRIMARY KEY(key, model)
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS images_content ON images(content_hash);
    PRAGMA user_version=5;
    COMMIT;";

/// v3 -> v4 unless `variants.custom` is already there (another opener upgraded it, or
/// a pre-release search build marked its embeddings-only index v4).
fn to_v4(conn: &Connection) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if !has_column(&tx, "variants", "custom")? {
        tx.execute_batch(V3_TO_V4)?;
    }
    tx.execute_batch("PRAGMA user_version=4")?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = st
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|n| n == column))
}

/// v6 -> v7 unless `variants.iptc` is already there (another opener upgraded it).
fn to_v7(conn: &Connection) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if !has_column(&tx, "variants", "iptc")? {
        tx.execute_batch(V6_TO_V7)?;
    }
    tx.execute_batch("PRAGMA user_version=7")?;
    tx.commit()?;
    Ok(())
}

/// v8 (map): each variant's geotag (sidecar `geotag`), mirrored for the map.
/// v7 is metadata's. See [`ensure_geo_columns`].
fn to_v8(conn: &Connection) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    add_geo_columns(&tx)?;
    tx.execute_batch("PRAGMA user_version=8")?;
    tx.commit()?;
    Ok(())
}

/// v9 (cull notes): why AI Cull rejected a variant (sidecar `cull_note`).
/// Only builds that know the column write the note, so no sidecar has one
/// yet and none needs reading again. Idempotent: two openers racing both run it.
fn to_v9(conn: &Connection) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if !has_column(&tx, "variants", "cull_note")? {
        tx.execute_batch("ALTER TABLE variants ADD COLUMN cull_note TEXT;")?;
    }
    tx.execute_batch(&format!("PRAGMA user_version={INDEX_SCHEMA_VERSION}"))?;
    tx.commit()?;
    Ok(())
}

/// Add the geotag columns when missing (a file upgraded by another build that
/// stamped this version without them heals here). A sidecar may already have a
/// geotag (written by this build while an older one kept the index), so every
/// sidecar is read again on the sync that follows: their recorded mtimes are
/// invalidated. Idempotent: two openers racing both run it.
fn add_geo_columns(tx: &Transaction<'_>) -> Result<()> {
    if !has_column(tx, "variants", "geo_lat")? {
        tx.execute_batch(
            "ALTER TABLE variants ADD COLUMN geo_lat REAL;
             ALTER TABLE variants ADD COLUMN geo_lon REAL;
             ALTER TABLE variants ADD COLUMN geo_alt REAL;
             UPDATE variants SET sidecar_mtime = -1;",
        )?;
    }
    Ok(())
}

/// [`add_geo_columns`] on an index already at this version (checked on every open).
fn ensure_geo_columns(conn: &Connection) -> Result<()> {
    if has_column(conn, "variants", "geo_lat")? {
        return Ok(());
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    add_geo_columns(&tx)?;
    tx.commit()?;
    Ok(())
}

fn to_v6() -> String {
    TO_V6.replace("{PEOPLE}", PEOPLE_SCHEMA)
}

/// Why an existing index was discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// The file did not exist.
    Missing,
    /// It could not be opened or failed `quick_check`; moved aside to the given path.
    Corrupt(PathBuf),
    /// Written by an older (or newer) schema.
    SchemaChanged { found: i64 },
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    conn.execute_batch(
        "PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY;
         PRAGMA trusted_schema=OFF;",
    )?;
    // SQLite's own `lower()` folds ASCII only; the keyword filter needs Unicode.
    conn.create_scalar_function(
        LOWER_FN,
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| Ok(ctx.get::<Option<String>>(0)?.map(|s| s.to_lowercase())),
    )
}

/// SQL name of the Unicode-aware `lower`, registered by [`configure`].
pub(crate) const LOWER_FN: &str = "ds_lower";

fn create(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    conn.execute_batch(&format!(
        "BEGIN; {SCHEMA} {PEOPLE_SCHEMA} PRAGMA user_version={INDEX_SCHEMA_VERSION}; COMMIT;"
    ))?;
    Ok(conn)
}

/// Only these mean the file itself is damaged. Anything else (busy, locked,
/// permission, I/O) must not cause a live index to be moved away.
pub(crate) fn is_corruption(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

enum Checked {
    Ok(Connection, i64),
    Corrupt,
}

fn check(path: &Path) -> rusqlite::Result<Checked> {
    let classify = |e: rusqlite::Error| {
        if is_corruption(&e) {
            Ok(Checked::Corrupt)
        } else {
            Err(e)
        }
    };
    let conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => return classify(e),
    };
    let checked = (|| {
        configure(&conn)?;
        let ok: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok((ok == "ok", version))
    })();
    match checked {
        Ok((true, v)) => Ok(Checked::Ok(conn, v)),
        Ok((false, _)) => Ok(Checked::Corrupt),
        Err(e) => classify(e),
    }
}

/// Remove a database file and its WAL/SHM companions.
fn remove_db_files(path: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{suffix}", path.display()));
        match fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).at(&p),
            _ => {}
        }
    }
    Ok(())
}

/// Open the index, creating or replacing it when it is missing, corrupt, or
/// from another schema version. `Some(recovery)` means it must be repopulated
/// from the sidecars.
pub fn open_or_recreate(path: &Path) -> Result<(Connection, Option<Recovery>)> {
    if !path.exists() {
        remove_db_files(path)?;
        return Ok((create(path)?, Some(Recovery::Missing)));
    }
    match check(path)? {
        Checked::Ok(conn, v) if v == INDEX_SCHEMA_VERSION => {
            ensure_geo_columns(&conn)?;
            Ok((conn, None))
        }
        // v2 -> v3 -> v4 -> v5 -> v6 -> v7 -> v8 -> v9 only add columns, tables
        // and an index: keep the verified file stats (no re-hash). v4 is
        // look-and-match's, v5 search's, v6 Find's (people, faces, places), v7
        // metadata's (IPTC), v8 the map's (geotags), v9 AI Cull's notes.
        Checked::Ok(conn, v @ 2..=8) => {
            if v == 2 {
                conn.execute_batch(V2_TO_V3)?;
            }
            if v <= 4 {
                to_v4(&conn)?;
                conn.execute_batch(V4_TO_V5)?;
            }
            if v <= 5 {
                conn.execute_batch(&to_v6())?;
            }
            if v <= 6 {
                to_v7(&conn)?;
            }
            if v <= 7 {
                to_v8(&conn)?;
            }
            to_v9(&conn)?;
            Ok((conn, None))
        }
        // A newer build may be using it right now; don't pull it away.
        Checked::Ok(_, found) if found > INDEX_SCHEMA_VERSION => Err(SessionError::IndexTooNew {
            found,
            supported: INDEX_SCHEMA_VERSION,
        }),
        Checked::Ok(conn, found) => {
            drop(conn);
            remove_db_files(path)?;
            Ok((create(path)?, Some(Recovery::SchemaChanged { found })))
        }
        Checked::Corrupt => {
            // Keep the damaged file for diagnosis; it is only an index.
            let aside = PathBuf::from(format!("{}.corrupt", path.display()));
            fs::rename(path, &aside).at(path)?;
            remove_db_files(path)?;
            Ok((create(path)?, Some(Recovery::Corrupt(aside))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_never_trusts_schema_defined_functions() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let trusted = |c: &Connection| -> i64 {
            c.query_row("PRAGMA trusted_schema", [], |r| r.get(0))
                .unwrap()
        };
        let (c, _) = open_or_recreate(&p).unwrap();
        assert_eq!(trusted(&c), 0, "created");
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None);
        assert_eq!(trusted(&c), 0, "reopened");
        let g = crate::global::GlobalIndex::open(&d.path().join("global.db")).unwrap();
        assert_eq!(trusted(&g.conn), 0, "global index");
    }

    #[test]
    fn creates_then_reopens_cleanly() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, Some(Recovery::Missing));
        let mode: String = c
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(c);
        let (_, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn garbage_file_is_moved_aside() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        fs::write(&p, vec![0x5a; 8192]).unwrap();
        let (_, r) = open_or_recreate(&p).unwrap();
        let aside = d.path().join("x.dsession.corrupt");
        assert_eq!(r, Some(Recovery::Corrupt(aside.clone())));
        assert_eq!(fs::read(aside).unwrap(), vec![0x5a; 8192]);
    }

    #[test]
    fn unreadable_or_newer_index_is_an_error_not_corruption() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        drop(open_or_recreate(&p).unwrap());
        fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
        let e = open_or_recreate(&p).unwrap_err();
        assert!(matches!(e, SessionError::Sqlite(_)), "{e}");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            !d.path().join("x.dsession.corrupt").exists(),
            "not moved aside"
        );

        let c = Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA user_version=99").unwrap();
        drop(c);
        assert!(matches!(
            open_or_recreate(&p),
            Err(SessionError::IndexTooNew { found: 99, .. })
        ));
        assert!(p.exists());
        assert!(!is_corruption(&rusqlite::Error::InvalidQuery));
    }

    /// The v6 schema (Find): today's without `variants.iptc` (v7) and the
    /// geotag columns (v8).
    fn v6_schema() -> String {
        format!(
            "{} {PEOPLE_SCHEMA}",
            v7_schema().replace("\n    iptc TEXT NOT NULL DEFAULT '',", "")
        )
    }

    /// The v7 schema (metadata): v8's without the geotag columns.
    fn v7_schema() -> String {
        let v8 = v8_schema();
        let v7 = v8.replace(
            "\n    geo_lat REAL,\n    geo_lon REAL,\n    geo_alt REAL,",
            "",
        );
        assert_ne!(v7, v8);
        v7
    }

    /// The v8 schema (map): today's without `variants.cull_note` (v9).
    fn v8_schema() -> String {
        let v8 = SCHEMA.replace("\n    cull_note TEXT,", "");
        assert_ne!(v8, SCHEMA);
        v8
    }

    /// A v8 index (the map's builds) is kept and gains the AI Cull note column
    /// without re-reading sidecars (no sidecar has a note before v9).
    #[test]
    fn a_v8_index_gains_the_cull_note_column() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(&format!(
            "{} {PEOPLE_SCHEMA} PRAGMA user_version=8; {ONE_PHOTO}
             INSERT INTO variants(id, image_id, ordinal, name, rating, color_tag, pick, edited, custom,
                keywords, search, sidecar, sidecar_mtime, sidecar_size, read_only, geo_lat, geo_lon)
                VALUES (5, 1, 1, '', 3, 0, 0, 0, 0, '[]', '', 'Capture/.ds/a.json', 1234, 10, 0, 1.5, 2.5);",
            v8_schema()
        ))
        .unwrap();
        assert!(!has_column(&c, "variants", "cull_note").unwrap());
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!((r, version(&c)), (None, INDEX_SCHEMA_VERSION));
        let row: (i64, i64, Option<f64>, Option<String>) = c
            .query_row(
                "SELECT rating, sidecar_mtime, geo_lat, cull_note FROM variants",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(row, (3, 1234, Some(1.5), None), "kept as it was");
        drop(c);
        // A second opener that read v8 before the first upgraded it: no duplicate column.
        let c = Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA user_version=8").unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!((r, version(&c)), (None, INDEX_SCHEMA_VERSION));
        assert_eq!(count(&c, "variants"), 1);
    }

    /// The v4 schema (look and match): v6's without the search tables (v5).
    fn v4_schema() -> String {
        let v6 = v6_schema();
        let at = v6.find("CREATE TABLE embeddings").unwrap();
        let v4 = v6[..at].to_owned();
        assert!(!v4.contains("images_content"));
        v4
    }

    /// The v3 schema: v4's without `variants.custom`.
    fn v3_schema() -> String {
        let v3 = v4_schema().replace("\n    custom INTEGER NOT NULL DEFAULT 0,", "");
        assert_ne!(v3, v4_schema());
        v3
    }

    const ONE_PHOTO: &str = "INSERT INTO images(id, rel_path, folder, file_name_lc, hash, file_size, file_mtime,
            file_ino, width, height, orientation) VALUES (1, 'Capture/a', 0, 'a', x'00', 1, 42, 7, 1, 1, 1);";

    fn version(c: &Connection) -> i64 {
        c.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn count(c: &Connection, table: &str) -> i64 {
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    /// Find's v6 tables are there, empty (`count` fails on a missing table).
    fn assert_find_tables(c: &Connection) {
        for t in [
            "faces",
            "face_scans",
            "face_links",
            "variant_people",
            "locations",
        ] {
            assert_eq!(count(c, t), 0, "{t}");
        }
    }

    #[test]
    fn a_look_and_match_v4_index_gains_search_and_keeps_its_variants() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(&format!("{} PRAGMA user_version=4;", v4_schema()))
            .unwrap();
        c.execute_batch(&format!(
            "{ONE_PHOTO}
             INSERT INTO variants(id, image_id, ordinal, name, rating, color_tag, pick, edited, custom,
                keywords, search, sidecar, sidecar_mtime, sidecar_size, read_only)
                VALUES (5, 1, 0, '', 0, 0, 0, 1, 2, '[]', '', 'Capture/.ds/a.json', 1234, 10, 0);"
        ))
        .unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None, "kept, not rebuilt");
        let row: (i64, i64) = c
            .query_row("SELECT custom, sidecar_mtime FROM variants", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(
            row,
            (2, -1),
            "look's values kept; sidecars re-read (v7's IPTC fields, v8's geotags)"
        );
        assert_eq!((count(&c, "images"), count(&c, "embeddings")), (1, 0));
        assert!(has_column(&c, "images", "content_hash").unwrap());
        let indexed: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'images_content'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 1);
        assert_eq!(version(&c), INDEX_SCHEMA_VERSION);
        assert_find_tables(&c);

        drop(c);
        // Two openers racing on a v4 file: the second runs V4_TO_V5 again.
        let c = Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA user_version=4").unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!((r, version(&c)), (None, INDEX_SCHEMA_VERSION));
        assert_eq!(count(&c, "variants"), 1);
    }

    #[test]
    fn a_pre_release_search_v4_index_gains_the_custom_column() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        // feat/nl-search builds marked v3 + embeddings as v4 (no `variants.custom`).
        let c = Connection::open(&p).unwrap();
        c.execute_batch(&format!(
            "{} CREATE TABLE embeddings(key BLOB NOT NULL, model TEXT NOT NULL, vec BLOB NOT NULL,
                PRIMARY KEY(key, model)) WITHOUT ROWID;
             INSERT INTO embeddings VALUES (x'01', 'm', x'00');
             PRAGMA user_version=4;",
            v3_schema()
        ))
        .unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None);
        assert!(has_column(&c, "variants", "custom").unwrap());
        assert_eq!(count(&c, "embeddings"), 1, "its vectors are kept");
        assert_eq!(version(&c), INDEX_SCHEMA_VERSION);
        assert_find_tables(&c);
    }

    #[test]
    fn v3_index_upgrades_through_v4_v5_to_v6() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(&format!(
            "{} PRAGMA user_version=3; {ONE_PHOTO}",
            v3_schema()
        ))
        .unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None, "kept, not rebuilt");
        assert!(has_column(&c, "variants", "custom").unwrap());
        drop(c);
        // A second opener that read v3 before the first upgraded it: no duplicate column.
        let c = Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA user_version=3").unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None);
        let images: i64 = c
            .query_row("SELECT count(*) FROM images", [], |r| r.get(0))
            .unwrap();
        let embeddings: i64 = c
            .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!((images, embeddings), (1, 0));
        assert_find_tables(&c);
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn a_v6_index_gains_the_geotag_columns_and_keeps_its_rows() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(&format!(
            "{} PRAGMA user_version=6; {ONE_PHOTO}
             INSERT INTO variants(id, image_id, ordinal, name, rating, color_tag, pick, edited, custom,
                keywords, search, sidecar, sidecar_mtime, sidecar_size, read_only)
                VALUES (5, 1, 1, '', 3, 0, 0, 0, 0, '[]', '', 'Capture/.ds/a.json', 1234, 10, 0);
             INSERT INTO locations VALUES (x'01', 'Sicily', 37.5, 14.0, '');",
            v6_schema()
        ))
        .unwrap();
        assert!(!has_column(&c, "variants", "geo_lat").unwrap());
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None, "kept, not rebuilt");
        assert_eq!(version(&c), INDEX_SCHEMA_VERSION);
        assert!(
            has_column(&c, "variants", "iptc").unwrap(),
            "6 -> 7 -> 8: metadata's step ran"
        );
        let row: (i64, i64, Option<f64>, Option<f64>, Option<f64>) = c
            .query_row(
                "SELECT rating, sidecar_mtime, geo_lat, geo_lon, geo_alt FROM variants",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (3, -1, None, None, None),
            "kept; its sidecar is read again (it may hold a geotag)"
        );
        assert_eq!(count(&c, "locations"), 1, "places kept");
        drop(c);
        // A second opener that read v6 before the first upgraded it: no duplicate column.
        let c = Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA user_version=6").unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!((r, version(&c)), (None, INDEX_SCHEMA_VERSION));
        assert_eq!(count(&c, "variants"), 1);
        drop(c);
        // A file stamped with this version but without the columns (another
        // build's numbering) heals on open instead of failing every query.
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "ALTER TABLE variants DROP COLUMN geo_lat; ALTER TABLE variants DROP COLUMN geo_lon;
             ALTER TABLE variants DROP COLUMN geo_alt;",
        )
        .unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None);
        assert!(has_column(&c, "variants", "geo_alt").unwrap());
    }

    /// A v7 index (metadata's builds) is kept and upgraded, never wiped.
    #[test]
    fn a_v7_index_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(&format!(
            "{} {PEOPLE_SCHEMA} PRAGMA user_version=7; {ONE_PHOTO}
             INSERT INTO embeddings VALUES (x'01', 'm', x'00');",
            v7_schema()
        ))
        .unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None, "kept, not rebuilt");
        assert_eq!(version(&c), INDEX_SCHEMA_VERSION);
        assert_eq!((count(&c, "images"), count(&c, "embeddings")), (1, 1));
        assert!(has_column(&c, "variants", "geo_lat").unwrap());
    }

    /// Eight openers at once on a v6 file: every one succeeds, nothing is
    /// rebuilt, and the columns are added once.
    #[test]
    fn openers_racing_on_a_v6_index_all_succeed() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        // A real index is in WAL mode (as `create` leaves it).
        let c = Connection::open(&p).unwrap();
        let _: String = c
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .unwrap();
        c.execute_batch(&format!(
            "{} PRAGMA user_version=6; {ONE_PHOTO}",
            v6_schema()
        ))
        .unwrap();
        drop(c);
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (p, start) = (p.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    let (c, r) = open_or_recreate(&p).unwrap();
                    (r, version(&c), count(&c, "images"))
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), (None, INDEX_SCHEMA_VERSION, 1));
        }
        let c = Connection::open(&p).unwrap();
        let geo: i64 = c
            .query_row(
                "SELECT count(*) FROM pragma_table_info('variants') WHERE name LIKE 'geo_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(geo, 3);
    }

    #[test]
    fn v4_and_v5_indexes_gain_the_people_tables() {
        for found in [4, 5] {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("x.dsession");
            let c = Connection::open(&p).unwrap();
            c.execute_batch(&format!("{} PRAGMA user_version={found};", v6_schema()))
                .unwrap();
            c.execute(
                "INSERT INTO images(id, rel_path, folder, file_name_lc, hash, file_size, file_mtime, file_ino,
                    width, height, orientation) VALUES (1, 'Capture/a', 0, 'a', x'00', 1, 42, 7, 1, 1, 1)",
                [],
            )
            .unwrap();
            drop(c);
            let (c, r) = open_or_recreate(&p).unwrap();
            assert_eq!(r, None, "kept, not rebuilt");
            for table in [
                "faces",
                "face_scans",
                "face_links",
                "variant_people",
                "locations",
                "images",
            ] {
                let n: i64 = c
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                    .unwrap();
                assert_eq!(n, i64::from(table == "images"), "{table}");
            }
            let v: i64 = c
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, INDEX_SCHEMA_VERSION);
            // A second opener that read the old version runs it again.
            c.execute_batch(&to_v6()).unwrap();
        }
    }

    #[test]
    fn v6_index_gains_iptc_and_rereads_its_sidecars() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(&format!(
            "{} {PEOPLE_SCHEMA} PRAGMA user_version=6; {ONE_PHOTO}
             INSERT INTO variants(id, image_id, ordinal, name, rating, color_tag, pick, edited, custom,
                keywords, search, sidecar, sidecar_mtime, sidecar_size, read_only)
                VALUES (5, 1, 0, '', 3, 0, 0, 1, 1, '[\"a\"]', 'a', 'Capture/.ds/a.json', 1234, 10, 0);
             INSERT INTO embeddings VALUES (x'01', 'm', x'00');",
            v6_schema()
        ))
        .unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None, "kept, not rebuilt");
        assert_eq!(version(&c), INDEX_SCHEMA_VERSION);
        let row: (i64, String, i64, i64) = c
            .query_row(
                "SELECT rating, iptc, sidecar_mtime, (SELECT file_mtime FROM images) FROM variants",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (3, String::new(), -1, 42),
            "row kept, sidecar re-read pending, original stats kept"
        );
        assert_eq!(count(&c, "embeddings"), 1, "derived data kept");
        drop(c);
        // A second opener that read v6 before the first upgraded it: no duplicate column.
        let c = Connection::open(&p).unwrap();
        c.execute_batch("PRAGMA user_version=6").unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!((r, version(&c)), (None, INDEX_SCHEMA_VERSION));
        assert_eq!(count(&c, "variants"), 1);
    }

    #[test]
    fn v2_index_is_upgraded_in_place() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let v2 = v3_schema().replace(
            ",\n    content_hash BLOB,\n    sample_hash BLOB,\n    file_changed INTEGER NOT NULL DEFAULT 0",
            "",
        );
        assert_ne!(v2, v3_schema());
        let c = Connection::open(&p).unwrap();
        c.execute_batch(&format!("{v2} PRAGMA user_version=2;"))
            .unwrap();
        c.execute(
            "INSERT INTO images(id, rel_path, folder, file_name_lc, hash, file_size, file_mtime, file_ino,
                width, height, orientation) VALUES (1, 'Capture/a', 0, 'a', x'00', 1, 42, 7, 1, 1, 1)",
            [],
        )
        .unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None, "kept, not rebuilt");
        let row: (i64, Option<Vec<u8>>, i64) = c
            .query_row(
                "SELECT file_mtime, content_hash, file_changed FROM images WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (42, None, 0));
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn v3_index_is_upgraded_and_its_sidecars_reread() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let v3 = v3_schema();
        let c = Connection::open(&p).unwrap();
        c.execute_batch(&format!("{v3} PRAGMA user_version=3;"))
            .unwrap();
        c.execute_batch(
            "INSERT INTO images(id, rel_path, folder, file_name_lc, hash, file_size, file_mtime, file_ino,
                width, height, orientation) VALUES (1, 'Capture/a', 0, 'a', x'00', 1, 42, 7, 1, 1, 1);
             INSERT INTO variants(id, image_id, ordinal, name, rating, color_tag, pick, edited, keywords,
                search, sidecar, sidecar_mtime, sidecar_size, read_only)
                VALUES (5, 1, 0, '', 0, 0, 0, 0, '[]', '', 'Capture/.ds/a.json', 1234, 10, 0);",
        )
        .unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, None, "kept, not rebuilt");
        let row: (i64, i64, i64) = c
            .query_row(
                "SELECT v.custom, v.sidecar_mtime, i.file_mtime FROM variants v JOIN images i ON i.id = v.image_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (0, -1, 42),
            "sidecar re-read pending, original stats kept"
        );
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, INDEX_SCHEMA_VERSION);
    }

    #[test]
    fn old_schema_is_recreated() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.dsession");
        let c = Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE t(x); PRAGMA user_version=0;")
            .unwrap();
        drop(c);
        let (c, r) = open_or_recreate(&p).unwrap();
        assert_eq!(r, Some(Recovery::SchemaChanged { found: 0 }));
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='t'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
}
