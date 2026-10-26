//! A Lightroom Classic catalog (`.lrcat`, SQLite). It is never opened in
//! place: [`read_catalog`] copies it (and a `-wal`/`-journal` beside it) into a
//! temporary directory and reads the copy, so nothing in the user's folder is
//! opened for writing, locked, or given a `-shm` file, and a transaction
//! Lightroom left open is rolled back on the copy.
//!
//! The schema is Adobe's private one, known from public documentation and
//! open-source readers (see `docs/LR_FORMAT.md`): everything beyond the tables
//! that locate the files is feature-detected, and a missing table or column
//! costs that field with a warning, never the read. Only real tables are
//! queried (a view in a hostile catalog could run any query), the read has a
//! time limit, and every column is read by its actual SQLite type so a wrong
//! type is "not set", never an error.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lt_core::{ColorTag, PickState, Rating};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};

use crate::error::{IoAt, LrError, Result};
use crate::limits::{
    MAX_CATALOG_BYTES, MAX_DEVELOP_TOTAL, MAX_KEYWORDS, MAX_LUA_BYTES, MAX_PHOTOS, MAX_XMP_BYTES,
    cap_text,
};
use crate::lua::parse_lua_table;
use crate::model::{Develop, LrCollection, LrLibrary, LrPhoto, Setting, SourceKind};
use crate::xmp::{label_tag, parse_time_ms, parse_xmp};

/// How long the whole read may take.
const TIME_LIMIT: Duration = Duration::from_secs(60);
/// Longest chain of keyword or collection-set parents followed.
const MAX_ANCESTORS: usize = 64;
/// Warnings of one kind spelled out before "and N more".
const WARN_SAMPLE: usize = 3;

/// Read the catalog at `path`. `cancel` is polled between statements and every
/// thousand rows, and from a watcher thread that interrupts a statement still
/// running when it turns true.
pub fn read_catalog(path: &Path, cancel: &(dyn Fn() -> bool + Sync)) -> Result<LrLibrary> {
    read_catalog_within(path, cancel, TIME_LIMIT)
}

fn read_catalog_within(
    path: &Path,
    cancel: &(dyn Fn() -> bool + Sync),
    limit: Duration,
) -> Result<LrLibrary> {
    let len = fs::metadata(path).at(path)?.len();
    if len > MAX_CATALOG_BYTES {
        return Err(LrError::Db {
            path: path.to_owned(),
            message: format!("too large to copy ({len} bytes)"),
        });
    }
    if cancel() {
        return Err(LrError::Cancelled);
    }
    let tmp = tempfile::tempdir().at(&std::env::temp_dir())?;
    let copy = tmp.path().join("catalog.lrcat");
    fs::copy(path, &copy).at(path)?;
    for suffix in ["-journal", "-wal"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        let to = PathBuf::from(format!("{}{suffix}", copy.display()));
        copy_side_file(&side, &to, MAX_CATALOG_BYTES)?;
    }
    let deadline = Instant::now() + limit;
    let ctx = Ctx {
        path,
        cancel,
        deadline,
    };
    let conn = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|e| ctx.err(&e))?;
    // A hostile file's views and triggers must not call SQL functions.
    conn.execute_batch("PRAGMA trusted_schema=OFF")
        .map_err(|e| ctx.err(&e))?;
    // A hostile file can make one statement run forever.
    conn.progress_handler(1000, Some(move || Instant::now() > deadline));
    let mut reader = Reader {
        ctx,
        conn: &conn,
        tables: HashSet::new(),
        warnings: Vec::new(),
    };
    let handle = conn.get_interrupt_handle();
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                if cancel() {
                    handle.interrupt();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let out = reader.read();
        finished.store(true, Ordering::Relaxed);
        out
    })
}

/// Copy a `-journal`/`-wal` beside the catalog, if there is one: only a
/// regular file (not through a link), no larger than `max` bytes.
fn copy_side_file(side: &Path, to: &Path, max: u64) -> Result<()> {
    let md = match fs::symlink_metadata(side) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        md => md.at(side)?,
    };
    let refuse = |message: String| LrError::Db {
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

struct Ctx<'a> {
    path: &'a Path,
    cancel: &'a dyn Fn() -> bool,
    deadline: Instant,
}

impl Ctx<'_> {
    fn err(&self, e: &rusqlite::Error) -> LrError {
        if (self.cancel)() {
            LrError::Cancelled
        } else if Instant::now() > self.deadline {
            LrError::Db {
                path: self.path.to_owned(),
                message: "took too long to read: not read".into(),
            }
        } else {
            LrError::Db {
                path: self.path.to_owned(),
                message: e.to_string(),
            }
        }
    }

    fn check(&self) -> Result<()> {
        if (self.cancel)() {
            Err(LrError::Cancelled)
        } else if Instant::now() > self.deadline {
            Err(LrError::Db {
                path: self.path.to_owned(),
                message: "took too long to read: not read".into(),
            })
        } else {
            Ok(())
        }
    }
}

// ----------------------------------------------------------- cell getters ---

fn int_at(r: &Row, i: usize) -> Option<i64> {
    match r.get_ref(i).ok()? {
        ValueRef::Integer(v) => Some(v),
        ValueRef::Real(f) if f.is_finite() && f.abs() < 9e18 => Some(f as i64),
        ValueRef::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok(),
        _ => None,
    }
}

fn real_at(r: &Row, i: usize) -> Option<f64> {
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

thread_local! {
    /// Text cells cut to `MAX_TEXT` during this thread's read (one warning at the end).
    static CUT: Cell<usize> = const { Cell::new(0) };
}

/// A cell as text, cut to `MAX_TEXT` bytes: a hostile catalog can hold gigabyte cells.
fn text_at(r: &Row, i: usize) -> Option<String> {
    let s = match r.get_ref(i).ok()? {
        ValueRef::Text(t) => {
            String::from_utf8_lossy(&t[..t.len().min(2 * crate::limits::MAX_TEXT)]).into_owned()
        }
        ValueRef::Integer(v) => v.to_string(),
        ValueRef::Real(f) if f.is_finite() => f.to_string(),
        _ => return None,
    };
    let (s, cut) = cap_text(s);
    if cut {
        CUT.with(|c| c.set(c.get() + 1));
    }
    Some(s)
}

fn is_null(r: &Row, i: usize) -> bool {
    matches!(r.get_ref(i), Ok(ValueRef::Null))
}

/// Lightroom's orientation codes (UNCONFIRMED: from public readers) as EXIF 1-8.
fn orientation_code(r: &Row, i: usize) -> Option<u8> {
    if let ValueRef::Integer(n) = r.get_ref(i).ok()? {
        return u8::try_from(n).ok().filter(|n| (1..=8).contains(n));
    }
    Some(match text_at(r, i)?.trim() {
        "AB" => 1,
        "BA" => 2,
        "CD" => 3,
        "DC" => 4,
        "BD" => 5,
        "BC" => 6,
        "AC" => 7,
        "DA" => 8,
        n => return n.parse::<u8>().ok().filter(|n| (1..=8).contains(n)),
    })
}

/// `a/b/` + `c/` + `name` + `.ext`; the separators are added where missing.
fn join_path(root: &str, folder: &str, base: &str, ext: &str) -> PathBuf {
    let mut s = String::with_capacity(root.len() + folder.len() + base.len() + ext.len() + 2);
    s.push_str(root);
    if !s.is_empty() && !s.ends_with('/') && !folder.is_empty() {
        s.push('/');
    }
    s.push_str(folder);
    if !s.is_empty() && !s.ends_with('/') {
        s.push('/');
    }
    s.push_str(base);
    if !ext.is_empty() {
        s.push('.');
        s.push_str(ext);
    }
    PathBuf::from(s)
}

// ----------------------------------------------------------------- reader ---

struct Reader<'a> {
    ctx: Ctx<'a>,
    conn: &'a Connection,
    tables: HashSet<String>,
    warnings: Vec<String>,
}

type Cols = HashSet<String>;

struct Row0 {
    name: String,
    parent: Option<i64>,
    kind: Kind,
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Plain,
    Smart,
    Set,
    Other,
}

impl Reader<'_> {
    fn e(&self, e: &rusqlite::Error) -> LrError {
        self.ctx.err(e)
    }

    fn columns(&self, table: &str) -> Result<Cols> {
        if !self.tables.contains(table) {
            return Ok(Cols::new());
        }
        // `table` is one of this file's own constants, never a name from the catalog.
        let mut st = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|e| self.e(&e))?;
        let names = st
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(|e| self.e(&e))?
            .collect::<rusqlite::Result<Cols>>()
            .map_err(|e| self.e(&e))?;
        Ok(names)
    }

    /// A reason not to read `what`, as a warning.
    fn skip(&mut self, what: &str, why: &str) {
        self.warnings.push(format!("{what}: {why} (not read)"));
    }

    fn read(&mut self) -> Result<LrLibrary> {
        let not_catalog = |m: String| LrError::NotACatalog {
            path: self.ctx.path.to_owned(),
            message: m,
        };
        self.tables = {
            let mut st = self
                .conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
                .map_err(|e| not_catalog(e.to_string()))?;
            // Bound to a name: the iterator borrows `st`, which drops first.
            #[allow(clippy::let_and_return)]
            let rows = st
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| not_catalog(e.to_string()))?
                .collect::<rusqlite::Result<HashSet<String>>>()
                .map_err(|e| not_catalog(e.to_string()))?;
            rows
        };
        self.conn
            .execute_batch("PRAGMA query_only = ON")
            .map_err(|e| self.e(&e))?;
        for t in [
            "Adobe_images",
            "AgLibraryFile",
            "AgLibraryFolder",
            "AgLibraryRootFolder",
        ] {
            if !self.tables.contains(t) {
                return Err(not_catalog(format!("no {t} table")));
            }
        }
        CUT.with(|c| c.set(0));
        let (mut photos, index) = self.photos()?;
        self.develop(&mut photos, &index)?;
        self.keywords(&mut photos, &index)?;
        self.metadata(&mut photos, &index)?;
        let collections = self.collections(&photos, &index)?;
        let (id, written_by) = self.variables()?;
        let cut = CUT.with(Cell::take);
        if cut > 0 {
            self.warnings.push(format!(
                "{cut} text values longer than {} KB were cut",
                crate::limits::MAX_TEXT >> 10
            ));
        }
        Ok(LrLibrary {
            kind: SourceKind::Catalog,
            origin: self.ctx.path.to_owned(),
            id,
            written_by,
            photos,
            collections,
            warnings: std::mem::take(&mut self.warnings),
        })
    }

    /// Photos with their paths, flags and ratings, ordered by local id, and
    /// local id → index.
    #[allow(clippy::too_many_lines)]
    fn photos(&mut self) -> Result<(Vec<LrPhoto>, HashMap<i64, usize>)> {
        let img = self.columns("Adobe_images")?;
        let need = [
            ("Adobe_images", "rootFile"),
            ("AgLibraryFile", "folder"),
            ("AgLibraryFile", "baseName"),
            ("AgLibraryFile", "extension"),
            ("AgLibraryFolder", "rootFolder"),
            ("AgLibraryFolder", "pathFromRoot"),
            ("AgLibraryRootFolder", "absolutePath"),
        ];
        for (t, c) in need {
            if !self.columns(t)?.contains(c) {
                return Err(LrError::NotACatalog {
                    path: self.ctx.path.to_owned(),
                    message: format!("no {t}.{c} column"),
                });
            }
        }
        let col = |c: &str| {
            if img.contains(c) {
                format!("i.{c}")
            } else {
                "NULL".to_owned()
            }
        };
        let sql = format!(
            "SELECT i.id_local, {gid}, {rating}, {pick}, {label}, {orient}, {master}, {copy}, {time}, \
                    rf.absolutePath, fo.pathFromRoot, f.baseName, f.extension \
             FROM Adobe_images i \
             JOIN AgLibraryFile f ON f.id_local = i.rootFile \
             JOIN AgLibraryFolder fo ON fo.id_local = f.folder \
             JOIN AgLibraryRootFolder rf ON rf.id_local = fo.rootFolder \
             ORDER BY i.id_local",
            gid = col("id_global"),
            rating = col("rating"),
            pick = col("pick"),
            label = col("colorLabels"),
            orient = col("orientation"),
            master = col("masterImage"),
            copy = col("copyName"),
            time = col("captureTime"),
        );
        let has = |c: &str| img.contains(c);
        let mut st = self.conn.prepare(&sql).map_err(|e| self.e(&e))?;
        let mut rows = st.query([]).map_err(|e| self.e(&e))?;
        let mut photos: Vec<LrPhoto> = Vec::new();
        let mut index: HashMap<i64, usize> = HashMap::new();
        let mut masters: Vec<Option<i64>> = Vec::new();
        let mut skipped = 0usize;
        let mut capped = false;
        while let Some(r) = rows.next().map_err(|e| self.e(&e))? {
            if photos.len().is_multiple_of(1024) {
                self.ctx.check()?;
            }
            if photos.len() >= MAX_PHOTOS {
                capped = true;
                break;
            }
            let Some(id) = int_at(r, 0) else {
                skipped += 1;
                continue;
            };
            let (Some(root), Some(base)) = (text_at(r, 9), text_at(r, 11)) else {
                skipped += 1;
                continue;
            };
            let folder = text_at(r, 10).unwrap_or_default();
            let ext = text_at(r, 12).unwrap_or_default();
            let mut p = LrPhoto {
                key: text_at(r, 1)
                    .filter(|g| !g.is_empty())
                    .unwrap_or_else(|| format!("lrcat:{id}")),
                path: join_path(&root, &folder, &base, &ext),
                copy_name: text_at(r, 7).filter(|n| !n.is_empty()),
                capture_time_ms: text_at(r, 8).and_then(|t| parse_time_ms(&t)),
                orientation: if has("orientation") {
                    orientation_code(r, 5)
                } else {
                    None
                },
                ..LrPhoto::default()
            };
            if has("rating") {
                p.rating = match r.get_ref(2) {
                    Ok(ValueRef::Null) => Some(Rating::default()),
                    _ => real_at(r, 2).and_then(|f| Rating::new(f.round().clamp(0.0, 5.0) as u8)),
                };
            }
            if has("pick") {
                p.pick = if is_null(r, 3) {
                    Some(PickState::Unflagged)
                } else {
                    real_at(r, 3).map(|f| {
                        if f > 0.0 {
                            PickState::Picked
                        } else if f < 0.0 {
                            PickState::Rejected
                        } else {
                            PickState::Unflagged
                        }
                    })
                };
            }
            if has("colorLabels") {
                if is_null(r, 4) {
                    p.color_tag = Some(ColorTag::None);
                } else if let Some(t) = text_at(r, 4) {
                    if t.trim().is_empty() {
                        p.color_tag = Some(ColorTag::None);
                    } else {
                        (p.color_tag, p.unmapped_label) = label_tag(&t);
                    }
                }
            }
            masters.push(if has("masterImage") {
                int_at(r, 6)
            } else {
                None
            });
            index.insert(id, photos.len());
            photos.push(p);
        }
        if skipped > 0 {
            self.warnings.push(format!(
                "{skipped} photos with an unreadable path were skipped"
            ));
        }
        if capped {
            self.warnings.push(format!(
                "More than {MAX_PHOTOS} photos: the rest of the catalog was not read"
            ));
        }
        // Virtual copies: the master's key and their order among its copies.
        let mut next_order: HashMap<usize, usize> = HashMap::new();
        for (i, m) in masters.iter().enumerate() {
            let Some(mi) = m.and_then(|m| index.get(&m).copied()).filter(|&mi| mi != i) else {
                continue;
            };
            let order = next_order.entry(mi).or_insert(0);
            *order += 1;
            photos[i].master = Some(photos[mi].key.clone());
            photos[i].order = *order;
        }
        Ok((photos, index))
    }

    /// Develop settings (`Adobe_imageDevelopSettings.text`, Lua tables).
    #[allow(clippy::too_many_lines)]
    fn develop(&mut self, photos: &mut [LrPhoto], index: &HashMap<i64, usize>) -> Result<()> {
        let cols = self.columns("Adobe_imageDevelopSettings")?;
        if !(cols.contains("image") && cols.contains("text")) {
            self.skip("Develop settings", "no Adobe_imageDevelopSettings table");
            return Ok(());
        }
        let mut st = self
            .conn
            .prepare("SELECT image, text FROM Adobe_imageDevelopSettings")
            .map_err(|e| self.e(&e))?;
        let mut rows = st.query([]).map_err(|e| self.e(&e))?;
        let mut failures: Vec<String> = Vec::new();
        let mut total = 0usize;
        let mut over_total = 0usize;
        while let Some(r) = rows.next().map_err(|e| self.e(&e))? {
            self.ctx.check()?;
            let Some(&pi) = int_at(r, 0).and_then(|id| index.get(&id)).as_ref().copied() else {
                continue;
            };
            let name = &photos[pi].path;
            let bytes: &[u8] = match r.get_ref(1) {
                Ok(ValueRef::Text(t) | ValueRef::Blob(t)) => t,
                _ => continue,
            };
            let result = if bytes.len() > MAX_LUA_BYTES {
                Err(format!("More than {MAX_LUA_BYTES} bytes: not read"))
            } else if total + bytes.len() > MAX_DEVELOP_TOTAL {
                over_total += 1;
                continue;
            } else {
                total += bytes.len();
                std::str::from_utf8(bytes)
                    .map_err(|_| "not text".to_owned())
                    .and_then(parse_lua_table)
                    .and_then(|v| match v {
                        Setting::Table(t) => Ok(t),
                        Setting::List(l) if l.is_empty() => Ok(Develop::new()),
                        _ => Err("not a table of settings".to_owned()),
                    })
            };
            match result {
                Ok(dev) => photos[pi].develop = Some(dev),
                Err(m) => failures.push(format!(
                    "{}: develop settings {m}",
                    name.file_name()
                        .map_or_else(String::new, |f| f.to_string_lossy().into_owned())
                )),
            }
        }
        self.aggregate("develop settings could not be read", &failures);
        if over_total > 0 {
            self.warnings.push(format!(
                "{over_total} photos' develop settings were not read: more than {} MB of settings in all",
                MAX_DEVELOP_TOTAL >> 20
            ));
        }
        Ok(())
    }

    /// One warning for many similar failures: a few spelled out, then a count.
    fn aggregate(&mut self, what: &str, failures: &[String]) {
        if failures.is_empty() {
            return;
        }
        let shown = failures[..failures.len().min(WARN_SAMPLE)].join("; ");
        self.warnings.push(if failures.len() > WARN_SAMPLE {
            format!(
                "{} {what}: {shown} and {} more",
                failures.len(),
                failures.len() - WARN_SAMPLE
            )
        } else {
            format!("{} {what}: {shown}", failures.len())
        });
    }

    #[allow(clippy::too_many_lines)]
    /// Keywords, with their parents when the keyword says to include them.
    fn keywords(&mut self, photos: &mut [LrPhoto], index: &HashMap<i64, usize>) -> Result<()> {
        let kw = self.columns("AgLibraryKeyword")?;
        let ki = self.columns("AgLibraryKeywordImage")?;
        if !(kw.contains("id_local")
            && kw.contains("name")
            && ki.contains("image")
            && ki.contains("tag"))
        {
            self.skip("Keywords", "no keyword tables");
            return Ok(());
        }
        let parent = if kw.contains("parent") {
            "parent"
        } else {
            "NULL"
        };
        let include = if kw.contains("includeParents") {
            "includeParents"
        } else {
            "NULL"
        };
        let export = if kw.contains("includeOnExport") {
            "includeOnExport"
        } else {
            "NULL"
        };
        let mut st = self
            .conn
            .prepare(&format!(
                "SELECT id_local, name, {parent}, {include}, {export} FROM AgLibraryKeyword"
            ))
            .map_err(|e| self.e(&e))?;
        // id -> (name, parent, include parents, may be exported)
        let mut words: HashMap<i64, (Option<String>, Option<i64>, bool, bool)> = HashMap::new();
        let mut rows = st.query([]).map_err(|e| self.e(&e))?;
        while let Some(r) = rows.next().map_err(|e| self.e(&e))? {
            let Some(id) = int_at(r, 0) else { continue };
            let name = text_at(r, 1)
                .map(|n| n.trim().to_owned())
                .filter(|n| !n.is_empty());
            words.insert(
                id,
                (
                    name,
                    int_at(r, 2),
                    int_at(r, 3) != Some(0),
                    int_at(r, 4) != Some(0),
                ),
            );
        }
        drop(rows);
        drop(st);
        let mut st = self
            .conn
            .prepare("SELECT image, tag FROM AgLibraryKeywordImage")
            .map_err(|e| self.e(&e))?;
        let mut rows = st.query([]).map_err(|e| self.e(&e))?;
        let mut per_photo: Vec<Vec<String>> = vec![Vec::new(); photos.len()];
        let mut seen: HashSet<(usize, i64)> = HashSet::new();
        let mut private: HashSet<i64> = HashSet::new();
        let mut capped = 0usize;
        let mut n = 0usize;
        while let Some(r) = rows.next().map_err(|e| self.e(&e))? {
            n += 1;
            if n.is_multiple_of(1024) {
                self.ctx.check()?;
            }
            let (Some(pi), Some(tag)) = (
                int_at(r, 0).and_then(|i| index.get(&i).copied()),
                int_at(r, 1),
            ) else {
                continue;
            };
            let list = &mut per_photo[pi];
            // Each keyword adds itself and, while its own flag says so, its parents.
            let mut cur = Some(tag);
            for _ in 0..MAX_ANCESTORS {
                let Some(id) = cur else { break };
                let Some((name, up, include_parents, exportable)) = words.get(&id) else {
                    break;
                };
                if let Some(name) = name
                    && seen.insert((pi, id))
                {
                    if !exportable {
                        private.insert(id);
                    } else if list.len() >= MAX_KEYWORDS {
                        capped += 1;
                    } else {
                        list.push(name.clone());
                    }
                }
                if !include_parents {
                    break;
                }
                cur = *up;
            }
        }
        if !private.is_empty() {
            self.warnings.push(format!(
                "{} private keywords (not included on export) were not imported",
                private.len()
            ));
        }
        if capped > 0 {
            self.warnings.push(format!(
                "{capped} keywords over the limit of {MAX_KEYWORDS} a photo were not imported"
            ));
        }
        for (p, k) in photos.iter_mut().zip(per_photo) {
            p.keywords = Some(k);
        }
        Ok(())
    }

    /// Title, caption and copyright: from the IPTC tables where they have the
    /// columns, else from each photo's stored XMP.
    #[allow(clippy::too_many_lines)]
    fn metadata(&mut self, photos: &mut [LrPhoto], index: &HashMap<i64, usize>) -> Result<()> {
        #[derive(Clone, Copy)]
        enum F {
            Title,
            Caption,
            Copyright,
        }
        let fields = [
            ("title", F::Title),
            ("caption", F::Caption),
            ("copyright", F::Copyright),
        ];
        let mut source: HashMap<&str, Vec<(&str, F)>> = HashMap::new();
        let mut assigned: Vec<&str> = Vec::new();
        for table in ["AgLibraryIPTC", "AgHarvestedIptcMetadata"] {
            let cols = self.columns(table)?;
            if !cols.contains("image") {
                continue;
            }
            for (c, f) in fields {
                if cols.contains(c) && !assigned.contains(&c) {
                    assigned.push(c);
                    source.entry(table).or_default().push((c, f));
                }
            }
        }
        let set = |p: &mut LrPhoto, f: F, v: Option<String>| match f {
            F::Title => p.title = v,
            F::Caption => p.caption = v,
            F::Copyright => p.copyright = v,
        };
        for (table, cols) in &source {
            let list: Vec<&str> = cols.iter().map(|(c, _)| *c).collect();
            let mut st = self
                .conn
                .prepare(&format!("SELECT image, {} FROM {table}", list.join(", ")))
                .map_err(|e| self.e(&e))?;
            let mut rows = st.query([]).map_err(|e| self.e(&e))?;
            let mut n = 0usize;
            while let Some(r) = rows.next().map_err(|e| self.e(&e))? {
                n += 1;
                if n.is_multiple_of(1024) {
                    self.ctx.check()?;
                }
                let Some(pi) = int_at(r, 0).and_then(|i| index.get(&i).copied()) else {
                    continue;
                };
                for (i, (_, f)) in cols.iter().enumerate() {
                    set(
                        &mut photos[pi],
                        *f,
                        Some(text_at(r, i + 1).unwrap_or_default()),
                    );
                }
            }
        }
        if assigned.len() == fields.len() {
            return Ok(());
        }
        let am = self.columns("Adobe_AdditionalMetadata")?;
        if !(am.contains("image") && am.contains("xmp")) {
            if assigned.is_empty() {
                self.skip("Titles, captions and copyright", "no IPTC tables");
            }
            return Ok(());
        }
        let mut st = self
            .conn
            .prepare("SELECT image, xmp FROM Adobe_AdditionalMetadata")
            .map_err(|e| self.e(&e))?;
        let mut rows = st.query([]).map_err(|e| self.e(&e))?;
        let (mut n, mut failed) = (0usize, 0usize);
        while let Some(r) = rows.next().map_err(|e| self.e(&e))? {
            n += 1;
            if n.is_multiple_of(1024) {
                self.ctx.check()?;
            }
            let Some(pi) = int_at(r, 0).and_then(|i| index.get(&i).copied()) else {
                continue;
            };
            let bytes: &[u8] = match r.get_ref(1) {
                Ok(ValueRef::Text(t) | ValueRef::Blob(t)) => t,
                _ => continue,
            };
            if bytes.is_empty() {
                continue;
            }
            let parsed = if bytes.len() as u64 > MAX_XMP_BYTES {
                None
            } else {
                std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|t| parse_xmp(t).ok())
            };
            let Some(x) = parsed else {
                failed += 1;
                continue;
            };
            for (c, f) in fields {
                if !assigned.contains(&c) {
                    let v = match f {
                        F::Title => x.title.clone(),
                        F::Caption => x.caption.clone(),
                        F::Copyright => x.copyright.clone(),
                    };
                    set(&mut photos[pi], f, v);
                }
            }
        }
        if failed > 0 {
            self.warnings
                .push(format!("{failed} photos' stored XMP could not be read (titles, captions and copyright left out)"));
        }
        Ok(())
    }

    /// Collections and smart collections (collection sets give their names to the path).
    #[allow(clippy::too_many_lines)]
    fn collections(
        &mut self,
        photos: &[LrPhoto],
        index: &HashMap<i64, usize>,
    ) -> Result<Vec<LrCollection>> {
        let cc = self.columns("AgLibraryCollection")?;
        let ci = self.columns("AgLibraryCollectionImage")?;
        if !(cc.contains("id_local") && cc.contains("name")) {
            self.skip("Collections", "no AgLibraryCollection table");
            return Ok(Vec::new());
        }
        let col = |c: &str| {
            if cc.contains(c) {
                c.to_owned()
            } else {
                "NULL".to_owned()
            }
        };
        let sql = format!(
            "SELECT id_local, name, {}, {}, {} FROM AgLibraryCollection",
            col("parent"),
            col("creationId"),
            col("systemOnly")
        );
        let mut rows0: HashMap<i64, Row0> = HashMap::new();
        let mut order: Vec<i64> = Vec::new();
        let mut st = self.conn.prepare(&sql).map_err(|e| self.e(&e))?;
        let mut rows = st.query([]).map_err(|e| self.e(&e))?;
        while let Some(r) = rows.next().map_err(|e| self.e(&e))? {
            let (Some(id), Some(name)) = (int_at(r, 0), text_at(r, 1)) else {
                continue;
            };
            let system = int_at(r, 4).is_some_and(|s| s != 0);
            let kind = match text_at(r, 3).as_deref() {
                _ if system => Kind::Other,
                None | Some("com.adobe.ag.library.collection") => Kind::Plain,
                Some(c) if c.ends_with("smart_collection") => Kind::Smart,
                Some(c) if c.ends_with("group") => Kind::Set,
                Some(_) => Kind::Other,
            };
            order.push(id);
            rows0.insert(
                id,
                Row0 {
                    name,
                    parent: int_at(r, 2),
                    kind,
                },
            );
        }
        drop(rows);
        drop(st);
        let mut members: HashMap<i64, Vec<(f64, usize)>> = HashMap::new();
        if ci.contains("collection") && ci.contains("image") {
            let pos = if ci.contains("positionInCollection") {
                "positionInCollection"
            } else {
                "NULL"
            };
            let mut st = self
                .conn
                .prepare(&format!(
                    "SELECT collection, image, {pos} FROM AgLibraryCollectionImage"
                ))
                .map_err(|e| self.e(&e))?;
            let mut rows = st.query([]).map_err(|e| self.e(&e))?;
            let mut n = 0usize;
            while let Some(r) = rows.next().map_err(|e| self.e(&e))? {
                n += 1;
                if n.is_multiple_of(1024) {
                    self.ctx.check()?;
                }
                let (Some(c), Some(pi)) = (
                    int_at(r, 0),
                    int_at(r, 1).and_then(|i| index.get(&i).copied()),
                ) else {
                    continue;
                };
                if rows0.get(&c).is_some_and(|c| c.kind == Kind::Plain) {
                    members
                        .entry(c)
                        .or_default()
                        .push((real_at(r, 2).unwrap_or(f64::MAX), pi));
                }
            }
        }
        let mut out = Vec::new();
        for id in order {
            let row = &rows0[&id];
            if !matches!(row.kind, Kind::Plain | Kind::Smart) {
                continue;
            }
            let mut parents: Vec<String> = Vec::new();
            let mut up = row.parent;
            for _ in 0..MAX_ANCESTORS {
                let Some(p) = up.and_then(|u| rows0.get(&u)) else {
                    break;
                };
                if p.kind == Kind::Set {
                    parents.push(p.name.clone());
                }
                up = p.parent;
            }
            parents.reverse();
            let mut m = members.remove(&id).unwrap_or_default();
            m.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut keys: Vec<String> = Vec::with_capacity(m.len());
            let mut seen: HashSet<usize> = HashSet::with_capacity(m.len());
            for (i, (_, pi)) in m.into_iter().enumerate() {
                if i.is_multiple_of(1024) {
                    self.ctx.check()?;
                }
                if seen.insert(pi) {
                    keys.push(photos[pi].key.clone());
                }
            }
            out.push(LrCollection {
                id,
                name: row.name.clone(),
                parents,
                smart: row.kind == Kind::Smart,
                members: keys,
                unsupported: None,
            });
        }
        Ok(out)
    }

    /// The catalog's id and the Lightroom version that last wrote it, best effort.
    fn variables(&mut self) -> Result<(Option<String>, Option<String>)> {
        let cols = self.columns("Adobe_variablesTable")?;
        if !(cols.contains("name") && cols.contains("value")) {
            return Ok((None, None));
        }
        let mut st = self
            .conn
            .prepare("SELECT name, value FROM Adobe_variablesTable")
            .map_err(|e| self.e(&e))?;
        let mut rows = st.query([]).map_err(|e| self.e(&e))?;
        let (mut id, mut version) = (None, None);
        let mut n = 0usize;
        while let Some(r) = rows.next().map_err(|e| self.e(&e))? {
            n += 1;
            if n > 10_000 {
                break;
            }
            let (Some(name), Some(value)) = (text_at(r, 0), text_at(r, 1)) else {
                continue;
            };
            let value: String = value.trim().chars().take(200).collect();
            if value.is_empty() {
                continue;
            }
            match name.to_ascii_lowercase().as_str() {
                "adobe_storeproviderid" | "aglibrarycatalogid" | "catalogid" => id = Some(value),
                "adobe_lastloadedappversion" => version = Some(value),
                "adobe_dbversion" if version.is_none() => version = Some(value),
                _ => {}
            }
        }
        Ok((id, version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no() -> bool {
        false
    }

    /// The tables the reader uses, as a synthetic catalog (no real catalog is ever committed).
    pub(crate) const SCHEMA: &str = "
        CREATE TABLE AgLibraryRootFolder(id_local INTEGER PRIMARY KEY, id_global TEXT, absolutePath TEXT, name TEXT, relativePathFromCatalog TEXT);
        CREATE TABLE AgLibraryFolder(id_local INTEGER PRIMARY KEY, id_global TEXT, parentId INTEGER, pathFromRoot TEXT, rootFolder INTEGER);
        CREATE TABLE AgLibraryFile(id_local INTEGER PRIMARY KEY, id_global TEXT, baseName TEXT, extension TEXT, folder INTEGER, originalFilename TEXT);
        CREATE TABLE Adobe_images(id_local INTEGER PRIMARY KEY, id_global TEXT, rating REAL, pick REAL, colorLabels TEXT, orientation TEXT, masterImage INTEGER, copyName TEXT, captureTime TEXT, rootFile INTEGER, fileFormat TEXT);
        CREATE TABLE Adobe_imageDevelopSettings(id_local INTEGER PRIMARY KEY, image INTEGER, text TEXT, processVersion TEXT);
        CREATE TABLE AgLibraryKeyword(id_local INTEGER PRIMARY KEY, name TEXT, parent INTEGER, includeParents INTEGER);
        CREATE TABLE AgLibraryKeywordImage(id_local INTEGER PRIMARY KEY, image INTEGER, tag INTEGER);
        CREATE TABLE AgLibraryIPTC(id_local INTEGER PRIMARY KEY, image INTEGER, caption TEXT, copyright TEXT);
        CREATE TABLE Adobe_AdditionalMetadata(id_local INTEGER PRIMARY KEY, image INTEGER, xmp TEXT);
        CREATE TABLE AgLibraryCollection(id_local INTEGER PRIMARY KEY, name TEXT, parent INTEGER, creationId TEXT, systemOnly INTEGER);
        CREATE TABLE AgLibraryCollectionImage(id_local INTEGER PRIMARY KEY, collection INTEGER, image INTEGER, positionInCollection REAL);
        CREATE TABLE Adobe_variablesTable(id_local INTEGER PRIMARY KEY, name TEXT, value TEXT);
        INSERT INTO AgLibraryRootFolder VALUES (1, 'R', '/photos/', 'photos', '');
        INSERT INTO AgLibraryFolder VALUES (1, 'F1', NULL, '', 1), (2, 'F2', 1, 'Trip/', 1);
    ";

    pub(crate) fn fixture() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("Test.lrcat");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute_batch(
            "INSERT INTO AgLibraryFile VALUES (1, 'f1', 'A', 'ARW', 2, 'A.ARW'), (2, 'f2', 'B', 'DNG', 1, 'B.DNG'), (3, 'f3', 'A', 'ARW', 2, 'A.ARW');
             INSERT INTO Adobe_images VALUES
               (10, 'GA', 4.0, 1, 'Red', 'BC', NULL, NULL, '2020-05-01T12:00:00', 1, 'RAW'),
               (11, 'GB', NULL, -1, 'Custom', 'AB', NULL, NULL, NULL, 2, 'DNG'),
               (12, 'GA2', 2, 0, '', 'DA', 10, 'Copy 1', '2020-05-01T12:00:00', 1, 'RAW'),
               (13, NULL, 'x', NULL, NULL, 'ZZ', 999, '', 'garbage', 3, 'RAW');
             INSERT INTO Adobe_imageDevelopSettings VALUES
               (1, 10, 's = { Exposure2012 = 0.5, CropLeft = 0.1, ToneCurvePV2012 = { 0, 0, 255, 255 } }', '11.0'),
               (2, 12, 's = { Exposure2012 = -1 }', '11.0'),
               (3, 11, 's = { A = os.execute(1) }', '11.0'),
               (4, 13, 's = {}', '11.0');
             INSERT INTO AgLibraryKeyword VALUES (1, NULL, NULL, 1), (2, 'Places', 1, 1), (3, 'Coast', 2, 1), (4, 'sea', 3, 1), (5, 'solo', 2, 0), (6, 'loop', 7, 1), (7, 'loop2', 6, 1);
             INSERT INTO AgLibraryKeywordImage VALUES (1, 10, 4), (2, 10, 5), (3, 12, 7), (4, 999, 4);
             INSERT INTO AgLibraryIPTC VALUES (1, 10, 'A caption', '(c) someone'), (2, 11, NULL, NULL);
             INSERT INTO Adobe_AdditionalMetadata VALUES (1, 10, '<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><rdf:RDF><rdf:Description><dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">A title</rdf:li></rdf:Alt></dc:title></rdf:Description></rdf:RDF></x:xmpmeta>'), (2, 11, '<broken');
             INSERT INTO AgLibraryCollection VALUES
               (1, 'Set', NULL, 'com.adobe.ag.library.group', 0),
               (2, 'Best', 1, 'com.adobe.ag.library.collection', 0),
               (3, 'Smart', 1, 'com.adobe.ag.library.smart_collection', 0),
               (4, 'Quick Collection', NULL, 'com.adobe.ag.library.simple_collection', 0),
               (5, 'Hidden', NULL, 'com.adobe.ag.library.collection', 1);
             INSERT INTO AgLibraryCollectionImage VALUES (1, 2, 11, 2.0), (2, 2, 10, 1.0), (3, 2, 10, 3.0), (4, 5, 10, 1.0), (5, 99, 10, 1.0);
             INSERT INTO Adobe_variablesTable VALUES (1, 'Adobe_DBVersion', '1500000'), (2, 'Adobe_storeProviderID', 'CAT-1'), (3, 'Adobe_lastLoadedAppVersion', '13.2'), (4, 'x', NULL);",
        )
        .unwrap();
        (d, p)
    }

    #[test]
    fn reads_a_synthetic_catalog() {
        let (_d, p) = fixture();
        let lib = read_catalog(&p, &no).unwrap();
        assert_eq!(lib.kind, SourceKind::Catalog);
        assert_eq!(lib.id.as_deref(), Some("CAT-1"));
        assert_eq!(lib.written_by.as_deref(), Some("13.2"));
        assert_eq!(lib.photos.len(), 4);
        let a = &lib.photos[0];
        assert_eq!(a.key, "GA");
        assert_eq!(a.path, PathBuf::from("/photos/Trip/A.ARW"));
        assert_eq!(a.rating, Rating::new(4));
        assert_eq!(a.pick, Some(PickState::Picked));
        assert_eq!(a.color_tag, Some(ColorTag::Red));
        assert_eq!(a.orientation, Some(6));
        assert_eq!(a.capture_time_ms, Some(1_588_334_400_000));
        assert_eq!(
            a.keywords.as_deref(),
            Some(
                &[
                    "sea".to_owned(),
                    "Coast".into(),
                    "Places".into(),
                    "solo".into()
                ][..]
            )
        );
        assert_eq!(a.caption.as_deref(), Some("A caption"));
        assert_eq!(a.copyright.as_deref(), Some("(c) someone"));
        assert_eq!(a.title.as_deref(), Some("A title"), "from the stored XMP");
        let dev = a.develop.as_ref().unwrap();
        assert_eq!(dev["Exposure2012"], Setting::Num(0.5));
        assert_eq!(dev["ToneCurvePV2012"].list().unwrap().len(), 4);
        let b = &lib.photos[1];
        assert_eq!(b.path, PathBuf::from("/photos/B.DNG"));
        assert_eq!(b.rating, Some(Rating::default()), "NULL is unrated");
        assert_eq!(b.pick, Some(PickState::Rejected));
        assert_eq!(
            (b.color_tag, b.unmapped_label.as_deref()),
            (None, Some("Custom"))
        );
        assert!(
            b.develop.is_none(),
            "a settings string that isn't data is dropped"
        );
        assert_eq!(b.caption.as_deref(), Some(""));
        let v = &lib.photos[2];
        assert_eq!(v.master.as_deref(), Some("GA"));
        assert_eq!((v.copy_name.as_deref(), v.order), (Some("Copy 1"), 1));
        assert_eq!(v.color_tag, Some(ColorTag::None));
        assert_eq!(v.orientation, Some(8));
        assert_eq!(
            v.keywords.as_deref(),
            Some(&["loop2".to_owned(), "loop".into()][..]),
            "parent loops end"
        );
        assert_eq!(
            v.develop.as_ref().unwrap()["Exposure2012"],
            Setting::Num(-1.0)
        );
        let odd = &lib.photos[3];
        assert_eq!(odd.key, "lrcat:13");
        assert_eq!(
            (odd.rating, odd.pick, odd.orientation, odd.capture_time_ms),
            (None, Some(PickState::Unflagged), None, None)
        );
        assert_eq!(odd.master, None, "a master that isn't there");
        assert_eq!(odd.develop, Some(Develop::new()));
        let names: Vec<(String, Vec<String>, bool, Vec<String>)> = lib
            .collections
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    c.parents.clone(),
                    c.smart,
                    c.members.clone(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                (
                    "Best".to_owned(),
                    vec!["Set".to_owned()],
                    false,
                    vec!["GA".to_owned(), "GB".to_owned()]
                ),
                ("Smart".to_owned(), vec!["Set".to_owned()], true, vec![]),
            ]
        );
        let w = lib.warnings.join("\n");
        assert!(w.contains("develop settings"), "{w}");
        assert!(w.contains("stored XMP"), "{w}");
    }

    #[test]
    fn missing_pieces_cost_features_not_the_read() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("Bare.lrcat");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE AgLibraryRootFolder(id_local INTEGER PRIMARY KEY, absolutePath TEXT);
             CREATE TABLE AgLibraryFolder(id_local INTEGER PRIMARY KEY, pathFromRoot TEXT, rootFolder INTEGER);
             CREATE TABLE AgLibraryFile(id_local INTEGER PRIMARY KEY, baseName TEXT, extension TEXT, folder INTEGER);
             CREATE TABLE Adobe_images(id_local INTEGER PRIMARY KEY, rootFile INTEGER);
             INSERT INTO AgLibraryRootFolder VALUES (1, 'D:/pics');
             INSERT INTO AgLibraryFolder VALUES (1, 'a', 1);
             INSERT INTO AgLibraryFile VALUES (1, 'x', '', 1);
             INSERT INTO Adobe_images VALUES (5, 1);",
        )
        .unwrap();
        drop(c);
        let lib = read_catalog(&p, &no).unwrap();
        assert_eq!(lib.photos.len(), 1);
        assert_eq!(lib.photos[0].path, PathBuf::from("D:/pics/a/x"));
        assert_eq!(lib.photos[0].rating, None);
        assert_eq!(lib.id, None);
        assert_eq!(lib.collections.len(), 0);
        let w = lib.warnings.join("\n");
        for what in ["Develop settings", "Keywords", "Titles", "Collections"] {
            assert!(w.contains(what), "{what}: {w}");
        }
    }

    #[test]
    fn not_a_catalog_and_hostile_files() {
        let d = tempfile::tempdir().unwrap();
        // Not SQLite at all.
        let junk = d.path().join("junk.lrcat");
        fs::write(
            &junk,
            b"this is not a database, just some text that is long enough",
        )
        .unwrap();
        assert!(read_catalog(&junk, &no).is_err());
        // A database without the Lightroom tables.
        let other = d.path().join("other.lrcat");
        Connection::open(&other)
            .unwrap()
            .execute_batch("CREATE TABLE t(x)")
            .unwrap();
        assert!(matches!(
            read_catalog(&other, &no),
            Err(LrError::NotACatalog { .. })
        ));
        // A required table that is a view over a query that never ends is never run.
        let view = d.path().join("view.lrcat");
        let c = Connection::open(&view).unwrap();
        c.execute_batch(
            "CREATE TABLE AgLibraryRootFolder(id_local, absolutePath);
             CREATE TABLE AgLibraryFolder(id_local, pathFromRoot, rootFolder);
             CREATE TABLE AgLibraryFile(id_local, baseName, extension, folder);
             CREATE VIEW Adobe_images AS WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n)
               SELECT x AS id_local, x AS rootFile FROM n;",
        )
        .unwrap();
        drop(c);
        let started = Instant::now();
        assert!(matches!(
            read_catalog(&view, &no),
            Err(LrError::NotACatalog { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(20));
        // An optional table that is a view is ignored.
        let (_d2, p) = fixture();
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "DROP TABLE AgLibraryKeyword;
             CREATE VIEW AgLibraryKeyword AS WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n)
               SELECT x AS id_local, 'k' AS name, NULL AS parent, 1 AS includeParents FROM n;",
        )
        .unwrap();
        drop(c);
        let lib = read_catalog(&p, &no).unwrap();
        assert_eq!(lib.photos[0].keywords, None);
        assert!(lib.warnings.iter().any(|w| w.contains("Keywords")));
    }

    #[test]
    fn wrong_typed_columns_are_not_set_and_never_fatal() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("Types.lrcat");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute_batch(
            "INSERT INTO AgLibraryFile VALUES (1, 'f', 'A', 'ARW', 1, ''), (2, 'g', X'00FF', 5, 1, ''), (3, 'h', 'C', 'ARW', 1, '');
             INSERT INTO Adobe_images VALUES
               (1, X'0102', X'ff', 'high', 3, 77, 'abc', X'00', 12345, 1, ''),
               (2, 'G2', 3.6, 1.5, 'Blue', 'BD', NULL, NULL, '2020-01-01', 2, ''),
               (3, 'G3', 9, -2.5, 'Pink', 6, 3, NULL, NULL, 3, '');
             INSERT INTO Adobe_imageDevelopSettings VALUES (1, 'one', X'ffff', ''), (2, 1, 7, ''), (3, 2, X'73203d207b207d', ''), (4, 3, NULL, '');
             INSERT INTO AgLibraryKeyword VALUES (1, 'k', 'x', 'y');
             INSERT INTO AgLibraryKeywordImage VALUES (1, 'a', 1), (2, 1, X'00'), (3, 2, 1);
             INSERT INTO AgLibraryCollection VALUES (1, X'00', 'p', 5, 'z'), (2, 'C', 9, 7, NULL);
             INSERT INTO AgLibraryCollectionImage VALUES (1, 2, 2, 'later'), (2, 'x', 2, 1);
             INSERT INTO Adobe_variablesTable VALUES (1, X'00', 'v'), (2, 'Adobe_storeProviderID', X'00');",
        )
        .unwrap();
        drop(c);
        let lib = read_catalog(&p, &no).unwrap();
        // Photo 1's file name is fine; photo 2's base name is a BLOB, so it is skipped.
        assert_eq!(lib.photos.len(), 2, "{:?}", lib.warnings);
        let one = &lib.photos[0];
        assert_eq!(one.rating, None, "a BLOB rating");
        assert_eq!(one.pick, None, "text that isn't a number");
        assert_eq!(one.color_tag, None);
        assert_eq!(one.unmapped_label.as_deref(), Some("3"));
        assert_eq!(one.orientation, None);
        assert_eq!(one.capture_time_ms, None);
        assert_eq!(one.key, "lrcat:1", "a BLOB global id");
        assert!(one.develop.is_none());
        let three = &lib.photos[1];
        assert_eq!(three.rating, Rating::new(5), "clamped");
        assert_eq!(three.pick, Some(PickState::Rejected));
        assert_eq!(three.orientation, Some(6), "numeric orientation");
        assert_eq!(three.master, None, "its master is itself");
        assert!(lib.warnings.iter().any(|w| w.contains("unreadable path")));
        assert_eq!(lib.id, None);
        let c = lib.collections.first();
        assert!(c.is_none() || c.unwrap().members.is_empty(), "{c:?}");
    }

    #[test]
    fn develop_settings_that_are_too_big_are_dropped_with_one_warning() {
        let (_d, p) = fixture();
        let c = Connection::open(&p).unwrap();
        let big = format!("s = {{ a = \"{}\" }}", "x".repeat(MAX_LUA_BYTES));
        c.execute(
            "UPDATE Adobe_imageDevelopSettings SET text = ?1 WHERE image = 10",
            [big],
        )
        .unwrap();
        drop(c);
        let lib = read_catalog(&p, &no).unwrap();
        assert!(lib.photos[0].develop.is_none());
        assert!(
            lib.warnings.iter().any(|w| w.contains("bytes")),
            "{:?}",
            lib.warnings
        );
    }

    #[test]
    fn side_files_are_read_from_the_copy_and_the_original_is_untouched() {
        let (d, p) = fixture();
        let before = fs::read(&p).unwrap();
        let mut journal = p.clone().into_os_string();
        journal.push("-journal");
        fs::write(&journal, b"").unwrap();
        let lib = read_catalog(&p, &no).unwrap();
        assert_eq!(lib.photos.len(), 4);
        assert_eq!(fs::read(&p).unwrap(), before);
        let names: Vec<String> = fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| n.starts_with("Test.lrcat")),
            "{names:?}"
        );
    }

    #[test]
    fn side_files_are_regular_and_capped() {
        let (d, p) = fixture();
        // A -wal that is a link (to a device, a huge file...) is refused.
        let wal = PathBuf::from(format!("{}-wal", p.display()));
        std::os::unix::fs::symlink("/dev/zero", &wal).unwrap();
        let e = read_catalog(&p, &no).unwrap_err();
        assert!(e.to_string().contains("-wal: not a regular file"), "{e}");
        fs::remove_file(&wal).unwrap();
        // Over the cap: refused before anything is copied.
        let journal = d.path().join("j");
        fs::write(&journal, b"12345").unwrap();
        let to = d.path().join("copy");
        let e = copy_side_file(&journal, &to, 4).unwrap_err();
        assert!(e.to_string().contains("too large to copy (5 bytes)"), "{e}");
        assert!(!to.exists());
        copy_side_file(&journal, &to, 5).unwrap();
        assert_eq!(fs::read(&to).unwrap(), b"12345");
        assert_eq!(read_catalog(&p, &no).unwrap().photos.len(), 4);
    }

    #[test]
    fn cancel_and_missing_file() {
        let (_d, p) = fixture();
        assert!(matches!(
            read_catalog(&p, &|| true),
            Err(LrError::Cancelled)
        ));
        assert!(matches!(
            read_catalog(&p.with_extension("nope"), &no),
            Err(LrError::Io { .. })
        ));
    }

    /// A catalog with `n` photos, built with SQL only.
    fn big_catalog(n: usize) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("Big.lrcat");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute_batch(&format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {n})
               INSERT INTO AgLibraryFile SELECT x, 'f' || x, 'IMG_' || x, 'ARW', 2, NULL FROM c;
             WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {n})
               INSERT INTO Adobe_images SELECT x, 'G' || x, x % 6, 0, '', 'AB', NULL, NULL, '2020-01-01T00:00:00', x, 'RAW' FROM c;"
        ))
        .unwrap();
        (d, p)
    }

    /// A cancel that turns true while the catalog is being read stops the read
    /// (the watcher thread interrupts the running statement).
    #[test]
    fn a_cancel_during_the_read_stops_it() {
        let (_d, p) = big_catalog(400_000);
        let t0 = Instant::now();
        let out = read_catalog(&p, &|| t0.elapsed() > Duration::from_millis(5));
        assert!(matches!(out, Err(LrError::Cancelled)), "{out:?}");
        assert!(t0.elapsed() < Duration::from_secs(30));
    }

    fn exec(p: &Path, sql: &str) {
        Connection::open(p).unwrap().execute_batch(sql).unwrap();
    }

    /// One collection holding every photo, and one photo with 100k keywords: both
    /// were quadratic.
    #[test]
    fn big_collections_and_keyword_lists_scale() {
        let (_d, p) = big_catalog(200_000);
        exec(
            &p,
            "INSERT INTO AgLibraryCollection VALUES (1, 'All', NULL, 'com.adobe.ag.library.collection', 0);
             WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 200000)
               INSERT INTO AgLibraryCollectionImage SELECT x, 1, x, x FROM c;
             WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100000)
               INSERT INTO AgLibraryKeyword SELECT x, 'kw' || x, NULL, 1 FROM c;
             WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100000)
               INSERT INTO AgLibraryKeywordImage SELECT x, 1, x FROM c;",
        );
        let started = Instant::now();
        let lib = read_catalog(&p, &no).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(lib.collections[0].members.len(), 200_000);
        assert_eq!(lib.photos[0].keywords.as_ref().unwrap().len(), MAX_KEYWORDS);
        assert!(lib.warnings.iter().any(|w| w.contains("over the limit")));
    }

    #[test]
    fn private_keywords_and_parent_flags() {
        let (_d, p) = big_catalog(3);
        exec(
            &p,
            "ALTER TABLE AgLibraryKeyword ADD COLUMN includeOnExport INTEGER;
             INSERT INTO AgLibraryKeyword VALUES (1, 'Places', NULL, 1, 1), (2, 'Paris', 1, 1, 1),
               (3, 'Secret', NULL, 1, 0), (4, 'Leaf', 2, 0, 1);
             INSERT INTO AgLibraryKeywordImage VALUES (1, 1, 2), (2, 2, 3), (3, 3, 4), (4, 1, 3);",
        );
        let lib = read_catalog(&p, &no).unwrap();
        assert_eq!(
            lib.photos[0].keywords,
            Some(vec!["Paris".into(), "Places".into()])
        );
        assert_eq!(lib.photos[1].keywords, Some(vec![]), "private keyword");
        assert_eq!(
            lib.photos[2].keywords,
            Some(vec!["Leaf".into()]),
            "Leaf's own flag stops the chain"
        );
        assert!(
            lib.warnings
                .iter()
                .any(|w| w.starts_with("1 private keywords")),
            "{:?}",
            lib.warnings
        );
    }

    #[test]
    fn huge_text_cells_are_cut() {
        let (_d, p) = big_catalog(1);
        let big = "é".repeat(100_000);
        exec(
            &p,
            &format!("INSERT INTO AgLibraryIPTC VALUES (1, 1, '{big}', 'c');"),
        );
        let lib = read_catalog(&p, &no).unwrap();
        let caption = lib.photos[0].caption.as_ref().unwrap();
        assert!(caption.len() <= crate::limits::MAX_TEXT && caption.chars().all(|c| c == 'é'));
        assert!(lib.warnings.iter().any(|w| w.contains("were cut")));
    }

    /// A photo that only the `-wal` holds is read, and the original's three
    /// files are not changed.
    #[test]
    fn a_wal_only_photo_is_seen_and_the_original_is_untouched() {
        let (_d, p) = big_catalog(1);
        let live = Connection::open(&p).unwrap();
        live.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
            .unwrap();
        live.execute_batch(
            "INSERT INTO AgLibraryFile VALUES (2, 'f2', 'WAL_ONLY', 'ARW', 2, NULL);
             INSERT INTO Adobe_images VALUES (2, 'G2', 1, 0, '', 'AB', NULL, NULL, NULL, 2, 'RAW');",
        )
        .unwrap();
        let state = || {
            ["", "-wal", "-shm"].map(|s| {
                fs::read(format!("{}{s}", p.display()))
                    .ok()
                    .map(|b| blake3::hash(&b))
            })
        };
        let before = state();
        assert!(before[1].is_some(), "the photo is in the -wal");
        let lib = read_catalog(&p, &no).unwrap();
        assert!(lib.photos.iter().any(|x| x.path.ends_with("WAL_ONLY.ARW")));
        assert_eq!(state(), before);
        drop(live);
    }

    #[test]
    fn a_million_photos_read_within_a_bounded_time() {
        let (_d, p) = big_catalog(1_000_000);
        let started = Instant::now();
        let lib = read_catalog(&p, &no).unwrap();
        let took = started.elapsed();
        assert_eq!(lib.photos.len(), 1_000_000);
        assert_eq!(
            lib.photos[999_999].path,
            PathBuf::from("/photos/Trip/IMG_1000000.ARW")
        );
        assert!(took < Duration::from_secs(45), "took {took:?}");
    }

    #[test]
    fn the_time_limit_stops_a_long_read() {
        let (_d, p) = big_catalog(50_000);
        let err = read_catalog_within(&p, &no, Duration::ZERO).unwrap_err();
        assert!(err.to_string().contains("too long"), "{err}");
    }

    #[test]
    fn paths_join_with_one_separator() {
        assert_eq!(join_path("/r/", "a/", "n", "x"), PathBuf::from("/r/a/n.x"));
        assert_eq!(join_path("/r", "a", "n", ""), PathBuf::from("/r/a/n"));
        assert_eq!(join_path("/r/", "", "n", "x"), PathBuf::from("/r/n.x"));
        assert_eq!(join_path("", "", "n", "x"), PathBuf::from("n.x"));
    }
}
