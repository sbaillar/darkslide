//! The session database (`<name>.cosessiondb`, SQLite, Core Data–style `Z*`
//! tables). It is never opened in place: [`read_db`] copies it (and a
//! `-journal`/`-wal` beside it) into a temporary directory and reads the copy,
//! so nothing in the user's folder is opened for writing, locked, or given a
//! `-shm` file, and a transaction C1 left open is rolled back on the copy.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};

use crate::criteria::parse_criteria;
use crate::error::{C1Error, IoAt, Result};
use crate::map::key_for_column;
use crate::model::{C1Collection, CollectionKind, Keys, Settings};
use crate::settings::Cancel;

/// Where an image's file is, as the database records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbLocation {
    /// Relative to the session folder (`""` = the folder itself), `/`-separated.
    Relative(String),
    /// An absolute folder (a folder the session references outside itself).
    Absolute(PathBuf),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DbVariant {
    pub uuid: String,
    pub settings: Settings,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DbImage {
    pub uuid: Option<String>,
    pub file_name: String,
    pub location: DbLocation,
    /// In C1's order.
    pub variants: Vec<DbVariant>,
}

impl DbImage {
    /// Path relative to `folder`, if the image is inside it.
    pub fn rel_path(&self, folder: &Path) -> Option<String> {
        match &self.location {
            DbLocation::Relative(dir) => Some(join_rel(dir, &self.file_name)),
            DbLocation::Absolute(dir) => {
                let rest = dir.strip_prefix(folder).ok()?;
                let dir = rest.to_string_lossy().replace('\\', "/");
                Some(join_rel(&dir, &self.file_name))
            }
            DbLocation::Unknown => None,
        }
    }

    /// The absolute path, for images outside any session folder.
    pub fn absolute_path(&self) -> Option<PathBuf> {
        match &self.location {
            DbLocation::Absolute(dir) => Some(dir.join(&self.file_name)),
            _ => None,
        }
    }
}

/// `dir/name` with `.`/empty components dropped and `\` treated as `/`.
pub(crate) fn join_rel(dir: &str, name: &str) -> String {
    dir.replace('\\', "/")
        .split('/')
        .chain(std::iter::once(name))
        .filter(|c| !c.is_empty() && *c != ".")
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DbSession {
    pub uuid: Option<String>,
    pub written_by: Option<String>,
    pub images: Vec<DbImage>,
    /// Albums and smart albums, members as paths relative to the session folder.
    pub collections: Vec<C1Collection>,
    pub keyword_library: Vec<String>,
}

/// Larger session databases (and journals) are not copied (the real one is a
/// few MB per thousand images).
const MAX_DB_BYTES: u64 = 1 << 30;

/// The tables read. Each must be a real table: a view could run any query
/// (a recursive one never ends).
const TABLES: [&str; 12] = [
    "ZDOCUMENTCONTENT",
    "ZVERSIONINFO",
    "ZPATHLOCATION",
    "ZVARIANTLAYER",
    "ZVARIANTMETADATA",
    "ZVARIANT",
    "ZIMAGE",
    "ZENTITIES",
    "ZCOLLECTION",
    "ZIMAGEINCOLLECTION",
    "ZVARIANTINCOLLECTION",
    "ZKEYWORD",
];

/// How much of a database is read before it is refused.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// The whole read (a hostile file can make one statement run forever).
    time: Duration,
    /// Rows of any one query (a real session has a few per variant).
    rows: usize,
    /// Text and blob bytes over the whole read.
    bytes: usize,
}

const LIMITS: Limits = Limits {
    time: Duration::from_secs(60),
    rows: 4_000_000,
    bytes: MAX_DB_BYTES as usize,
};

/// Read the session database at `db` (never in place, see the module docs).
/// Collection members are resolved to paths relative to `folder`.
pub fn read_db(db: &Path, folder: &Path) -> Result<DbSession> {
    let never: Cancel = Arc::new(|| false);
    read_db_cancellable(db, folder, &never)
}

/// [`read_db`], stopping when `cancel` says so (checked before the copy and
/// inside every query).
pub fn read_db_cancellable(db: &Path, folder: &Path, cancel: &Cancel) -> Result<DbSession> {
    read_db_with(db, folder, cancel, LIMITS)
}

fn read_db_with(db: &Path, folder: &Path, cancel: &Cancel, limits: Limits) -> Result<DbSession> {
    let db_err = |message: String| C1Error::Db {
        path: db.to_owned(),
        message,
    };
    if cancel() {
        return Err(C1Error::Cancelled);
    }
    let tmp = tempfile::tempdir().at(&std::env::temp_dir())?;
    let copy = tmp.path().join("session.cosessiondb");
    // Not through a link (`x.cosessiondb -> /dev/zero`), and only a regular file.
    let md = fs::symlink_metadata(db).at(db)?;
    if !md.is_file() {
        return Err(db_err("not a regular file".into()));
    }
    let len = md.len();
    if len > MAX_DB_BYTES {
        return Err(db_err(format!("too large to copy ({len} bytes)")));
    }
    fs::copy(db, &copy).at(db)?;
    for suffix in ["-journal", "-wal"] {
        let side = PathBuf::from(format!("{}{suffix}", db.display()));
        let to = PathBuf::from(format!("{}{suffix}", copy.display()));
        copy_side_file(&side, &to, MAX_DB_BYTES)?;
    }
    let conn = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| db_err(e.to_string()))?;
    // A hostile file's views and triggers must not call SQL functions.
    conn.execute_batch("PRAGMA trusted_schema=OFF")
        .map_err(|e| db_err(e.to_string()))?;
    let deadline = Instant::now() + limits.time;
    let stop = cancel.clone();
    conn.progress_handler(1000, Some(move || stop() || Instant::now() > deadline));
    let reader = Reader {
        conn: &conn,
        limits,
        bytes: Cell::new(0),
    };
    reader.read(folder).map_err(|e| {
        if cancel() {
            C1Error::Cancelled
        } else {
            match e {
                Stop::Sql(_) if Instant::now() > deadline => {
                    db_err("took too long to read: not read".into())
                }
                Stop::Sql(e) => db_err(e.to_string()),
                Stop::Refused(m) => db_err(m),
            }
        }
    })
}

/// Copy a `-journal`/`-wal` beside the database, if there is one: only a
/// regular file (not through a link), no larger than `max` bytes.
pub(crate) fn copy_side_file(side: &Path, to: &Path, max: u64) -> Result<()> {
    let md = match fs::symlink_metadata(side) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        md => md.at(side)?,
    };
    let refuse = |message: String| C1Error::Db {
        path: side.to_owned(),
        message,
    };
    if !md.is_file() {
        return Err(refuse("not a regular file".into()));
    }
    if md.len() > max {
        return Err(refuse(format!("too large to copy ({} bytes)", md.len())));
    }
    fs::copy(side, to).at(side)?;
    Ok(())
}

/// Why a read stopped.
#[derive(Debug)]
enum Stop {
    Sql(rusqlite::Error),
    /// Over a limit, or not shaped like a session database.
    Refused(String),
}

impl From<rusqlite::Error> for Stop {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}

type ReadResult<T> = std::result::Result<T, Stop>;

fn text(v: ValueRef) -> Option<String> {
    match v {
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Null | ValueRef::Blob(_) => None,
    }
}

/// Structural columns of the layer and metadata tables (not settings).
const STRUCTURAL: [&str; 7] = [
    "Z_ENT",
    "Z_PK",
    "Z_OPT",
    "ZINDEX",
    "ZMETADATA",
    "ZVARIANT",
    "ZLAYER",
];

/// A read of an open copy, within [`Limits`].
struct Reader<'a> {
    conn: &'a Connection,
    limits: Limits,
    /// Text and blob bytes seen so far.
    bytes: Cell<usize>,
}

impl Reader<'_> {
    /// Run `sql` and hand `f` each row, refusing past the row limit or once
    /// the whole read has seen more than the byte limit.
    fn each_row(
        &self,
        sql: &str,
        mut f: impl FnMut(&rusqlite::Row) -> ReadResult<()>,
    ) -> ReadResult<()> {
        let mut st = self.conn.prepare(sql)?;
        let columns = st.column_count();
        let mut rows = st.query([])?;
        let mut n = 0;
        while let Some(r) = rows.next()? {
            n += 1;
            if n > self.limits.rows {
                return Err(Stop::Refused(format!(
                    "more than {} rows in one table: not read",
                    self.limits.rows
                )));
            }
            let mut bytes = self.bytes.get();
            for i in 0..columns {
                if let ValueRef::Text(b) | ValueRef::Blob(b) = r.get_ref(i)? {
                    bytes += b.len();
                }
            }
            self.bytes.set(bytes);
            if bytes > self.limits.bytes {
                return Err(Stop::Refused(format!(
                    "more than {} bytes of text: not read",
                    self.limits.bytes
                )));
            }
            f(r)?;
        }
        Ok(())
    }

    /// The first column of the first row of `sql`, if it is text.
    fn opt_string(&self, sql: &str) -> Option<String> {
        let mut out = None;
        self.each_row(sql, |r| {
            out = r.get(0)?;
            Ok(())
        })
        .ok()?;
        out
    }

    /// Every row of `table` as (key column value, settings keys); NULL/empty cells
    /// are "not set" and left out.
    fn keyed_rows(&self, table: &str, key: &str, skip: &[&str]) -> ReadResult<HashMap<i64, Keys>> {
        let names: Vec<String> = self
            .conn
            .prepare(&format!("SELECT * FROM {table} LIMIT 0"))?
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect();
        let key_idx = names.iter().position(|n| n == key).unwrap_or(0);
        let mut out: HashMap<i64, Keys> = HashMap::new();
        self.each_row(&format!("SELECT * FROM {table}"), |r| {
            let Ok(id) = r.get::<_, i64>(key_idx) else {
                return Ok(());
            };
            let mut keys = Keys::new();
            for (i, n) in names.iter().enumerate() {
                if skip.contains(&n.as_str()) {
                    continue;
                }
                if let Some(v) = text(r.get_ref(i)?).filter(|v| !v.is_empty()) {
                    keys.insert(key_for_column(n), v);
                }
            }
            out.insert(id, keys);
            Ok(())
        })?;
        Ok(out)
    }

    /// Refuse a database where a table read is a view.
    fn check_tables(&self) -> ReadResult<()> {
        let names = TABLES.map(|t| format!("'{t}'")).join(", ");
        let mut views = Vec::new();
        self.each_row(
            &format!(
                "SELECT name FROM sqlite_master WHERE type = 'view' AND upper(name) IN ({names})"
            ),
            |r| {
                views.push(r.get::<_, String>(0)?);
                Ok(())
            },
        )?;
        match views.first() {
            Some(v) => Err(Stop::Refused(format!(
                "{v} is a view, not a table: not read"
            ))),
            None => Ok(()),
        }
    }

    /// `ZPATHLOCATION`: id → where its images are.
    fn read_locations(&self) -> ReadResult<HashMap<i64, DbLocation>> {
        let mut locations = HashMap::new();
        self.each_row(
            "SELECT Z_PK, ZMACROOT, ZRELATIVEPATH, ZISRELATIVE FROM ZPATHLOCATION",
            |r| {
                let root: Option<String> = r.get(1)?;
                let rel: Option<String> = r.get(2)?;
                let is_rel: Option<bool> = r.get(3)?;
                let loc = if is_rel.unwrap_or(false) {
                    DbLocation::Relative(rel.unwrap_or_default())
                } else if let Some(root) = root.filter(|s| !s.is_empty()) {
                    let rel = rel.unwrap_or_default();
                    DbLocation::Absolute(PathBuf::from(root).join(rel.trim_start_matches('/')))
                } else {
                    DbLocation::Unknown
                };
                locations.insert(r.get(0)?, loc);
                Ok(())
            },
        )?;
        Ok(locations)
    }

    fn read(&self, folder: &Path) -> ReadResult<DbSession> {
        self.check_tables()?;
        let mut out = DbSession {
            uuid: self.opt_string("SELECT ZDOCUMENTUUID FROM ZDOCUMENTCONTENT LIMIT 1"),
            written_by: self.opt_string("SELECT ZAUTHOR FROM ZVERSIONINFO LIMIT 1"),
            ..DbSession::default()
        };

        let locations = self.read_locations()?;
        let layers = self.keyed_rows("ZVARIANTLAYER", "Z_PK", &STRUCTURAL)?;
        let metadata = self.keyed_rows("ZVARIANTMETADATA", "ZLAYER", &STRUCTURAL)?;
        let layer = |pk: Option<i64>| -> Keys {
            let Some(pk) = pk else { return Keys::new() };
            let mut k = layers.get(&pk).cloned().unwrap_or_default();
            if let Some(m) = metadata.get(&pk) {
                k.extend(m.iter().map(|(a, b)| (a.clone(), b.clone())));
            }
            k
        };

        let mut variants_of: HashMap<i64, Vec<(i64, DbVariant)>> = HashMap::new();
        let mut variant_uuid: HashMap<i64, String> = HashMap::new();
        let mut order = 0;
        self.each_row(
            "SELECT Z_PK, ZVARIANTUUID, ZIMAGE, ZDEFAULTLAYER, ZADJUSTMENTLAYER FROM ZVARIANT
             ORDER BY ZIMAGE, ZINDEX, Z_PK",
            |r| {
                let (pk, uuid, image): (i64, Option<String>, Option<i64>) =
                    (r.get(0)?, r.get(1)?, r.get(2)?);
                let (Some(uuid), Some(image)) = (uuid.filter(|u| !u.is_empty()), image) else {
                    return Ok(());
                };
                variant_uuid.insert(pk, uuid.clone());
                let settings = Settings {
                    dl: layer(r.get(3)?),
                    al: layer(r.get(4)?),
                };
                order += 1;
                variants_of
                    .entry(image)
                    .or_default()
                    .push((order, DbVariant { uuid, settings }));
                Ok(())
            },
        )?;

        let mut image_rel: HashMap<i64, String> = HashMap::new();
        self.each_row(
            "SELECT Z_PK, ZIMAGEUUID, ZIMAGEFILENAME, ZIMAGELOCATION FROM ZIMAGE ORDER BY Z_PK",
            |r| {
                let pk: i64 = r.get(0)?;
                let Some(file_name) = r.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty())
                else {
                    return Ok(());
                };
                let location = r
                    .get::<_, Option<i64>>(3)?
                    .and_then(|l| locations.get(&l).cloned())
                    .unwrap_or(DbLocation::Unknown);
                let image = DbImage {
                    uuid: r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
                    file_name,
                    location,
                    variants: variants_of
                        .remove(&pk)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(_, v)| v)
                        .collect(),
                };
                if let Some(rel) = image.rel_path(folder) {
                    image_rel.insert(pk, rel);
                }
                out.images.push(image);
                Ok(())
            },
        )?;

        out.collections = self.read_collections(&image_rel, &variant_uuid)?;
        // A session without a keyword table has no library.
        if self
            .conn
            .prepare("SELECT ZNAME FROM ZKEYWORD LIMIT 0")
            .is_ok()
        {
            self.each_row("SELECT ZNAME FROM ZKEYWORD WHERE ZNAME IS NOT NULL", |r| {
                out.keyword_library.push(r.get(0)?);
                Ok(())
            })?;
        }
        Ok(out)
    }

    fn read_collections(
        &self,
        image_rel: &HashMap<i64, String>,
        variant_uuid: &HashMap<i64, String>,
    ) -> ReadResult<Vec<C1Collection>> {
        let mut entities: HashMap<i64, String> = HashMap::new();
        self.each_row("SELECT Z_ENT, ZNAME FROM ZENTITIES", |r| {
            entities.insert(r.get(0)?, r.get(1)?);
            Ok(())
        })?;
        let mut rows: HashMap<i64, Row> = HashMap::new();
        self.each_row(
            "SELECT Z_PK, Z_ENT, ZNAME, ZPARENT, ZSMARTCRITERIA FROM ZCOLLECTION",
            |r| {
                let ent: Option<i64> = r.get(1)?;
                rows.insert(
                    r.get(0)?,
                    Row {
                        entity: ent
                            .and_then(|e| entities.get(&e).cloned())
                            .unwrap_or_default(),
                        name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        parent: r.get(3)?,
                        criteria: r.get(4)?,
                    },
                );
                Ok(())
            },
        )?;
        let mut members: HashMap<i64, Vec<(String, Option<String>)>> = HashMap::new();
        for (sql, variant_level) in [
            (
                "SELECT ZCOLLECTION, ZIMAGE FROM ZIMAGEINCOLLECTION ORDER BY Z_PK",
                false,
            ),
            (
                "SELECT v.ZCOLLECTION, w.ZIMAGE, v.ZVARIANT FROM ZVARIANTINCOLLECTION v JOIN ZVARIANT w ON w.Z_PK = v.ZVARIANT ORDER BY v.Z_PK",
                true,
            ),
        ] {
            if self.conn.prepare(sql).is_err() {
                continue;
            }
            self.each_row(sql, |r| {
                let (coll, image): (i64, i64) = (r.get(0)?, r.get(1)?);
                let Some(rel) = image_rel.get(&image) else {
                    return Ok(());
                };
                let variant = if variant_level {
                    variant_uuid.get(&r.get::<_, i64>(2)?).cloned()
                } else {
                    None
                };
                members
                    .entry(coll)
                    .or_default()
                    .push((rel.clone(), variant));
                Ok(())
            })?;
        }
        let mut out = Vec::new();
        let mut ids: Vec<&i64> = rows.keys().collect();
        ids.sort();
        for id in ids {
            let row = &rows[id];
            let kind = match row.entity.as_str() {
                "AlbumCollection" => CollectionKind::Album,
                "SmartCollection" => CollectionKind::Smart,
                _ => continue,
            };
            let mut parents = Vec::new();
            let mut p = row.parent;
            let mut guard = 0;
            while let Some(pk) = p {
                guard += 1;
                let Some(pr) = rows.get(&pk).filter(|_| guard < 64) else {
                    break;
                };
                // The session's root project (no parent of its own) is not a level.
                if pr.parent.is_some() && !pr.name.is_empty() {
                    parents.insert(0, pr.name.clone());
                }
                p = pr.parent;
            }
            out.push(C1Collection {
                id: *id,
                kind,
                name: row.name.clone(),
                parents,
                members: members.remove(id).unwrap_or_default(),
                criteria: row.criteria.as_deref().and_then(parse_criteria),
            });
        }
        Ok(out)
    }
}

/// A `ZCOLLECTION` row.
struct Row {
    entity: String,
    name: String,
    parent: Option<i64>,
    criteria: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn relative_and_absolute_locations() {
        let folder = Path::new("/Users/me/Pictures/Shoot");
        let img = |location| DbImage {
            uuid: None,
            file_name: "a.ARW".into(),
            location,
            variants: vec![],
        };
        assert_eq!(
            img(DbLocation::Relative(".".into()))
                .rel_path(folder)
                .as_deref(),
            Some("a.ARW")
        );
        assert_eq!(
            img(DbLocation::Relative("Capture".into()))
                .rel_path(folder)
                .as_deref(),
            Some("Capture/a.ARW")
        );
        assert_eq!(
            img(DbLocation::Relative("Capture\\Day 1".into()))
                .rel_path(folder)
                .as_deref(),
            Some("Capture/Day 1/a.ARW")
        );
        let inside = img(DbLocation::Absolute(folder.join("Selects")));
        assert_eq!(inside.rel_path(folder).as_deref(), Some("Selects/a.ARW"));
        let outside = img(DbLocation::Absolute("/Volumes/media/pics".into()));
        assert_eq!(outside.rel_path(folder), None);
        assert_eq!(
            outside.absolute_path(),
            Some(PathBuf::from("/Volumes/media/pics/a.ARW"))
        );
        assert_eq!(img(DbLocation::Unknown).rel_path(folder), None);
        assert_eq!(img(DbLocation::Unknown).absolute_path(), None);
    }

    #[test]
    fn reads_the_synthetic_session_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("Shoot.cosessiondb");
        fixtures::build_db(&db);
        let s = read_db(&db, dir.path()).unwrap();
        assert_eq!(s.uuid.as_deref(), Some("DOC-UUID-1"));
        assert_eq!(s.written_by.as_deref(), Some("16.1.3.10 Pro Mac"));
        let rels: Vec<_> = s.images.iter().map(|i| i.rel_path(dir.path())).collect();
        assert_eq!(
            rels,
            vec![
                Some("a.ARW".into()),
                Some("b.ARW".into()),
                Some("Selects/c.ARW".into()),
                Some("Trash/d.ARW".into()),
                None
            ]
        );
        let a = &s.images[0];
        assert_eq!(a.uuid.as_deref(), Some("IMG-A"));
        assert_eq!(a.variants.len(), 2, "two variants, in C1's order");
        assert_eq!(a.variants[0].uuid, "VAR-A1");
        let st = &a.variants[0].settings;
        assert_eq!(st.al.get("Exposure").map(String::as_str), Some("0.25"));
        assert_eq!(
            st.al.get("HighlightRecoveryEx").map(String::as_str),
            Some("-20"),
            "column renamed to the .cos key"
        );
        assert_eq!(
            st.al.get("Basic_Rating").map(String::as_str),
            Some("4"),
            "metadata joins its layer"
        );
        assert_eq!(st.al.get("Color_Tag_Index").map(String::as_str), Some("1"));
        assert!(!st.al.contains_key("Contrast"), "NULL = not set");
        assert_eq!(st.dl.get("Rotation").map(String::as_str), Some("0"));
        // Albums and smart albums only; nested names; members by path.
        let names: Vec<_> = s
            .collections
            .iter()
            .map(C1Collection::display_name)
            .collect();
        assert_eq!(
            names,
            vec!["Five Stars", "All Images", "Trip / Favourites", "Picks"]
        );
        let fav = &s.collections[2];
        assert_eq!(fav.kind, CollectionKind::Album);
        assert_eq!(
            fav.members,
            vec![("a.ARW".into(), None), ("b.ARW".into(), None)]
        );
        let picks = &s.collections[3];
        assert_eq!(picks.members, vec![("a.ARW".into(), Some("VAR-A2".into()))]);
        assert!(s.collections[0].criteria.is_some());
        assert!(s.collections[1].criteria.is_none());
        assert_eq!(s.keyword_library, vec!["Beach", "Smith, John"]);
        // The fixture folder holds nothing but the database: no -shm, no journal.
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn a_hot_journal_is_rolled_back_on_the_copy_and_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("S.cosessiondb");
        fixtures::build_db(&db);
        // A stray journal that isn't a valid hot journal is ignored by SQLite;
        // what matters is that it's copied, read, and left untouched.
        let journal = dir.path().join("S.cosessiondb-journal");
        fs::write(&journal, b"").unwrap();
        let before = fs::read(&db).unwrap();
        read_db(&db, dir.path()).unwrap();
        assert_eq!(fs::read(&db).unwrap(), before);
        assert!(journal.exists());
    }

    #[test]
    fn a_database_that_is_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.cosessiondb");
        fixtures::build_db(&real);
        let link = dir.path().join("S.cosessiondb");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let e = read_db(&link, dir.path()).unwrap_err();
        assert!(e.to_string().contains("not a regular file"), "{e}");
    }

    #[test]
    fn errors_name_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.cosessiondb");
        assert!(matches!(
            read_db(&missing, dir.path()),
            Err(C1Error::Io { .. })
        ));
        let junk = dir.path().join("junk.cosessiondb");
        fs::write(&junk, b"not sqlite at all, just bytes").unwrap();
        let e = read_db(&junk, dir.path()).unwrap_err();
        assert!(matches!(e, C1Error::Db { .. }), "{e}");
        assert!(e.to_string().contains("junk.cosessiondb"));
        // A valid SQLite file without C1's tables.
        let other = dir.path().join("other.cosessiondb");
        Connection::open(&other)
            .unwrap()
            .execute_batch("CREATE TABLE t(x);")
            .unwrap();
        assert!(matches!(
            read_db(&other, dir.path()),
            Err(C1Error::Db { .. })
        ));
    }

    /// The fixture database at `dir/name` with `sql` run on it.
    fn db_with(dir: &Path, name: &str, sql: &str) -> PathBuf {
        let db = dir.join(name);
        fixtures::build_db(&db);
        Connection::open(&db).unwrap().execute_batch(sql).unwrap();
        db
    }

    fn never() -> Cancel {
        Arc::new(|| false)
    }

    /// An endless view under a table's name (any case) is refused, never run.
    #[test]
    fn tables_that_are_views_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for (table, cols) in [
            ("ZVARIANTLAYER", "x AS Z_PK"),
            (
                "zimage",
                "x AS Z_PK, 'u' AS ZIMAGEUUID, printf('%.*c', 100000, 'a') AS ZIMAGEFILENAME, 1 AS ZIMAGELOCATION",
            ),
        ] {
            let db = db_with(
                dir.path(),
                &format!("{table}.cosessiondb"),
                &format!(
                    "DROP TABLE {table};
                     CREATE VIEW {table} AS WITH RECURSIVE n(x) AS
                       (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT {cols} FROM n;"
                ),
            );
            let start = Instant::now();
            let e = read_db(&db, dir.path()).unwrap_err();
            assert!(e.to_string().contains(&format!("{table} is a view")), "{e}");
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }

    /// A real table of 50 000 layers: enough work for the progress handler.
    const BIG: &str =
        "WITH RECURSIVE n(x) AS (SELECT 10000 UNION ALL SELECT x + 1 FROM n LIMIT 50000)
         INSERT INTO ZVARIANTLAYER (Z_PK, ZCROP) SELECT x, 'crop' || x FROM n;";

    #[test]
    fn rows_text_and_time_are_limited() {
        let dir = tempfile::tempdir().unwrap();
        let db = db_with(dir.path(), "Big.cosessiondb", BIG);
        let read = |limits| read_db_with(&db, dir.path(), &never(), limits);
        let s = read(LIMITS).unwrap();
        assert_eq!(s.images.len(), 5, "under the limits it reads as before");

        let e = read(Limits {
            rows: 1000,
            ..LIMITS
        })
        .unwrap_err();
        assert!(e.to_string().contains("more than 1000 rows"), "{e}");
        let e = read(Limits {
            bytes: 100,
            ..LIMITS
        })
        .unwrap_err();
        assert!(e.to_string().contains("more than 100 bytes"), "{e}");
        let e = read(Limits {
            time: Duration::ZERO,
            ..LIMITS
        })
        .unwrap_err();
        assert!(e.to_string().contains("took too long"), "{e}");
        assert!(matches!(e, C1Error::Db { .. }));
    }

    #[test]
    fn a_read_stops_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let db = db_with(dir.path(), "Big.cosessiondb", BIG);
        let up_front: Cancel = Arc::new(|| true);
        assert!(matches!(
            read_db_cancellable(&db, dir.path(), &up_front),
            Err(C1Error::Cancelled)
        ));
        // Cancelled after the copy: the progress handler stops the query.
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = calls.clone();
        let later: Cancel =
            Arc::new(move || count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0);
        assert!(matches!(
            read_db_cancellable(&db, dir.path(), &later),
            Err(C1Error::Cancelled)
        ));
        assert!(
            calls.load(std::sync::atomic::Ordering::Relaxed) > 2,
            "asked mid-read"
        );
    }

    #[test]
    fn side_files_are_regular_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("S.cosessiondb");
        fixtures::build_db(&db);
        // A -wal that is a link (to a device, a huge file...) is refused.
        let wal = dir.path().join("S.cosessiondb-wal");
        std::os::unix::fs::symlink("/dev/zero", &wal).unwrap();
        let e = read_db(&db, dir.path()).unwrap_err();
        assert!(e.to_string().contains("-wal: not a regular file"), "{e}");
        fs::remove_file(&wal).unwrap();
        fs::create_dir(&wal).unwrap();
        assert!(read_db(&db, dir.path()).is_err(), "nor a folder");
        fs::remove_dir(&wal).unwrap();
        // Over the cap: refused before anything is copied.
        let journal = dir.path().join("S.cosessiondb-journal");
        fs::write(&journal, b"12345").unwrap();
        let to = dir.path().join("copy");
        let e = copy_side_file(&journal, &to, 4).unwrap_err();
        assert!(e.to_string().contains("too large to copy (5 bytes)"), "{e}");
        assert!(!to.exists());
        copy_side_file(&journal, &to, 5).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"12345");
        // None there: nothing to copy.
        copy_side_file(&dir.path().join("none"), &dir.path().join("x"), 5).unwrap();
        assert!(!dir.path().join("x").exists());
    }

    #[test]
    fn rel_joins_drop_dots() {
        assert_eq!(join_rel("", "a"), "a");
        assert_eq!(join_rel("./x/", "a"), "x/a");
    }
}
