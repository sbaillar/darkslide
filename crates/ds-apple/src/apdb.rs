//! Aperture 3 and iPhoto 9.3+ libraries: `Database/apdb/Library.apdb`
//! (SQLite), and captions from `Properties.apdb` beside it.
//!
//! - `RKMaster`: the originals. `imagePath` is relative to the library's
//!   `Masters/` folder for a managed original; for a referenced one
//!   (`volumeId` set), relative to the root of the `RKVolume` it is on.
//! - `RKVersion`: what the user sees, several per original. `mainRating` 0-5
//!   (-1: rejected), `isFlagged`, `colorLabelIndex`, `isHidden`, `name`,
//!   `projectUuid` (the project or event), position, rotation.
//! - `RKFolder`: projects and events (`folderType` 2) and the folders around
//!   them (1); `RKAlbum` + `RKAlbumVersion`: albums (`albumSubclass` 3 plain,
//!   2 smart, 1 built in; `albumType` 1 an album, others books, slideshows…).
//! - `RKKeyword` + `RKKeywordForVersion`: keywords.
//! - `RKImageAdjustment`: one row per adjustment of a version (`name`,
//!   `adjIndex`, `data`: see [`crate::archive`]).
//! - `Properties.apdb`: `RKIptcProperty` (`propertyKey` `Caption/Abstract`)
//!   → `RKUniqueString`: captions.
//!
//! Versions in the trash are left out, and so is an original version iPhoto
//! keeps hidden once the photo is edited (`showInLibrary` 0). A photo's first
//! version is its Darkslide photo; the others become extra variants, as
//! Lightroom's virtual copies do.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ds_core::{ColorTag, GeoPoint, PickState, Rating};
use ds_lr::{Develop, LrCollection, LrLibrary, LrPhoto, Setting, SourceKind};

use crate::archive;
use crate::detect::{Format, apdb_path, library_id};
use crate::error::{AppleError, IoAt, Result};
use crate::limits::{MAX_ARCHIVE_BYTES, MAX_KEYWORDS, MAX_PHOTOS};
use crate::map::ROTATION;
use crate::sql::{Ctx, Db, blob_at, copy_db, flag_at, int_at, open, real_at, text_at, watched};

/// How long the whole read may take.
const TIME_LIMIT: Duration = Duration::from_secs(120);
/// Seconds from the Unix epoch to Apple's reference date (2001-01-01 UTC).
const APPLE_EPOCH: i64 = 978_307_200;
/// Longest chain of parent folders followed.
const MAX_ANCESTORS: usize = 64;

struct Master {
    path: PathBuf,
    /// The library's `Masters` folder, for a managed original.
    folder: Option<PathBuf>,
    /// The volume of a referenced original.
    volume: Option<String>,
    stem: String,
}

#[derive(Default)]
struct Version {
    model_id: i64,
    uuid: String,
    master: String,
    name: Option<String>,
    number: i64,
    rating: Option<i64>,
    flagged: Option<bool>,
    label: Option<i64>,
    hidden: bool,
    shown: Option<bool>,
    project: Option<String>,
    place: Option<GeoPoint>,
    date: Option<f64>,
    tz_offset: Option<i64>,
    rotation: Option<f64>,
}

struct Folder {
    model_id: i64,
    name: String,
    kind: Option<i64>,
    parent: Option<String>,
    magic: bool,
    trashed: bool,
}

/// Read an Aperture 3 / iPhoto 9.3+ library at `root`. Referenced originals
/// are looked for under `volumes` (`/Volumes`).
pub(crate) fn read(
    root: &Path,
    format: Format,
    volumes: &Path,
    cancel: &(dyn Fn() -> bool + Sync),
) -> Result<LrLibrary> {
    read_within(root, format, volumes, cancel, TIME_LIMIT)
}

fn read_within(
    root: &Path,
    format: Format,
    volumes: &Path,
    cancel: &(dyn Fn() -> bool + Sync),
    limit: Duration,
) -> Result<LrLibrary> {
    let db_path = apdb_path(root).ok_or_else(|| AppleError::NotALibrary {
        path: root.to_owned(),
        message: "no Database/apdb/Library.apdb".into(),
    })?;
    if cancel() {
        return Err(AppleError::Cancelled);
    }
    let tmp = tempfile::tempdir().at(&std::env::temp_dir())?;
    let copy = copy_db(&db_path, tmp.path(), "Library.apdb")?;
    let deadline = Instant::now() + limit;
    let ctx = Ctx {
        path: &db_path,
        cancel,
        deadline,
    };
    let (conn, tables) = open(&copy, &ctx).map_err(|e| match e {
        AppleError::Db { path, message } => AppleError::NotALibrary { path, message },
        e => e,
    })?;
    let db = Db { ctx, conn, tables };
    if !db.has("RKVersion") || !db.has("RKMaster") {
        if db.has("ZRKVERSION") {
            return Err(AppleError::Unsupported {
                path: root.to_owned(),
                message: "This library was written by Aperture 1 or 2, which Darkslide can't \
                          read. Open it once in Aperture 3 to upgrade it, then choose it again."
                    .into(),
            });
        }
        return Err(AppleError::NotALibrary {
            path: db_path.clone(),
            message: "no RKVersion or RKMaster table".into(),
        });
    }
    let mut r = Reader {
        root,
        format,
        volumes,
        db: &db,
        warnings: Vec::new(),
        sides: Vec::new(),
    };
    let mut lib = watched(&db.conn, cancel, || r.read())?;
    let version_ids: Vec<i64> = r.sides.iter().map(|s| s.model_id).collect();
    // Captions are in a second database; without it the rest still imports.
    let captions = db_path.with_file_name("Properties.apdb");
    if captions.is_file() {
        match read_captions(&captions, tmp.path(), cancel, deadline) {
            Ok(None) => {}
            Ok(Some(by_version)) => {
                for (p, v) in lib.photos.iter_mut().zip(&version_ids) {
                    p.caption = Some(by_version.get(v).cloned().unwrap_or_default());
                }
            }
            Err(AppleError::Cancelled) => return Err(AppleError::Cancelled),
            Err(e) => lib.warnings.push(format!("Captions: {e} (not read)")),
        }
    }
    Ok(lib)
}

/// One adjustment of a photo: (index, name, data, enabled, masked).
type AdjRow = (i64, String, Data, bool, bool);

/// An adjustment's archived parameters, as read from its row.
enum Data {
    None,
    Bytes(Vec<u8>),
    /// Over [`MAX_ARCHIVE_BYTES`]: never read.
    TooLarge,
}

/// Whether `name` is a single folder name, safe to join under `/Volumes`.
fn volume_name_ok(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    !name.contains('/')
        && !name.contains('\0')
        && matches!(
            (parts.next(), parts.next()),
            (Some(std::path::Component::Normal(_)), None)
        )
}

struct Reader<'a> {
    root: &'a Path,
    format: Format,
    volumes: &'a Path,
    db: &'a Db<'a>,
    warnings: Vec<String>,
    /// Per photo, in the same order: what the reader needs besides it.
    sides: Vec<Side>,
}

/// A photo's version, as the library's other tables refer to it.
struct Side {
    model_id: i64,
    project: Option<String>,
}

impl Reader<'_> {
    fn read(&mut self) -> Result<LrLibrary> {
        let masters = self.masters()?;
        let versions = self.versions()?;
        let folders = self.folders()?;
        let mut photos = self.photos(&masters, &versions);
        let index: HashMap<i64, usize> = self
            .sides
            .iter()
            .enumerate()
            .map(|(i, s)| (s.model_id, i))
            .collect();
        self.keywords(&mut photos, &index)?;
        self.adjustments(&mut photos)?;
        let mut collections = projects(&folders, &photos, &self.sides);
        collections.extend(self.albums(&folders, &photos, &index)?);
        self.missing_volumes(&masters);
        let written_by = match self.format {
            Format::Aperture => "Aperture 3",
            _ => "iPhoto 9.3 or later",
        };
        Ok(LrLibrary {
            kind: SourceKind::Catalog,
            origin: self.root.to_owned(),
            id: Some(library_id(self.root)),
            written_by: Some(written_by.to_owned()),
            photos,
            collections,
            warnings: std::mem::take(&mut self.warnings),
        })
    }

    fn volumes(&self) -> Result<HashMap<i64, String>> {
        let mut out = HashMap::new();
        if !self.db.has("RKVolume") {
            return Ok(out);
        }
        let sql = self.db.select("RKVolume", &["modelId", "name"])?;
        self.db.each(&sql, |r| {
            // A volume is one folder under `/Volumes`: a name that is empty,
            // absolute or has `/` or `..` in it would point somewhere else, so
            // its originals are left as on a volume the library doesn't name.
            if let (Some(id), Some(name)) = (int_at(r, 0), text_at(r, 1))
                && volume_name_ok(&name)
            {
                out.insert(id, name);
            }
            Ok(())
        })?;
        Ok(out)
    }

    fn masters(&mut self) -> Result<HashMap<String, Master>> {
        let volumes = self.volumes()?;
        let managed = self.root.join("Masters");
        let sql = self
            .db
            .select("RKMaster", &["uuid", "imagePath", "volumeId", "isInTrash"])?;
        let mut out = HashMap::new();
        let mut unknown_volume = 0usize;
        self.db.each(&sql, |r| {
            let (Some(uuid), Some(rel)) = (text_at(r, 0), text_at(r, 1)) else {
                return Ok(());
            };
            if flag_at(r, 3) == Some(true) {
                return Ok(());
            }
            let rel = rel.trim_start_matches('/');
            // `..` would leave the library or the volume.
            if rel.is_empty() || Path::new(rel).components().any(|c| c.as_os_str() == "..") {
                return Ok(());
            }
            let stem = Path::new(rel)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let m = match int_at(r, 2) {
                None => Master {
                    path: managed.join(rel),
                    folder: Some(managed.clone()),
                    volume: None,
                    stem,
                },
                Some(v) => {
                    let Some(name) = volumes.get(&v) else {
                        unknown_volume += 1;
                        return Ok(());
                    };
                    Master {
                        path: self.volumes.join(name).join(rel),
                        folder: None,
                        volume: Some(name.clone()),
                        stem,
                    }
                }
            };
            out.insert(uuid, m);
            Ok(())
        })?;
        if unknown_volume > 0 {
            self.warnings.push(format!(
                "{unknown_volume} referenced original{} on a volume the library doesn't name: not imported",
                if unknown_volume == 1 { "" } else { "s" }
            ));
        }
        Ok(out)
    }

    fn versions(&self) -> Result<Vec<Version>> {
        let sql = self.db.select(
            "RKVersion",
            &[
                "modelId",
                "uuid",
                "masterUuid",
                "name",
                "versionNumber",
                "mainRating",
                "isFlagged",
                "colorLabelIndex",
                "isHidden",
                "isInTrash",
                "showInLibrary",
                "projectUuid",
                "latitude",
                "longitude",
                "exifLatitude",
                "exifLongitude",
                "imageDate",
                "imageTimeZoneOffsetSeconds",
                "rotation",
            ],
        )?;
        let mut out = Vec::new();
        self.db.each(&sql, |r| {
            let (Some(model_id), Some(uuid), Some(master)) =
                (int_at(r, 0), text_at(r, 1), text_at(r, 2))
            else {
                return Ok(());
            };
            if flag_at(r, 9) == Some(true) {
                return Ok(());
            }
            let point = |a: usize, b: usize| {
                let (lat, lon) = (real_at(r, a)?, real_at(r, b)?);
                // 0,0 is how an unset position is often stored.
                (lat != 0.0 || lon != 0.0)
                    .then(|| GeoPoint::new(lat, lon, None))
                    .flatten()
            };
            out.push(Version {
                model_id,
                uuid,
                master,
                name: text_at(r, 3).map(|s| s.trim().to_owned()),
                number: int_at(r, 4).unwrap_or(0),
                rating: int_at(r, 5),
                flagged: flag_at(r, 6),
                label: int_at(r, 7),
                hidden: flag_at(r, 8) == Some(true),
                shown: flag_at(r, 10),
                project: text_at(r, 11),
                place: point(12, 13).or_else(|| point(14, 15)),
                date: real_at(r, 16),
                tz_offset: int_at(r, 17),
                rotation: real_at(r, 18),
            });
            if out.len() > MAX_PHOTOS {
                return Err(AppleError::Db {
                    path: self.db.ctx.path.to_owned(),
                    message: format!("more than {MAX_PHOTOS} photos: not read"),
                });
            }
            Ok(())
        })?;
        Ok(out)
    }

    fn folders(&self) -> Result<HashMap<String, Folder>> {
        let mut out = HashMap::new();
        if !self.db.has("RKFolder") {
            return Ok(out);
        }
        let sql = self.db.select(
            "RKFolder",
            &[
                "uuid",
                "modelId",
                "name",
                "folderType",
                "parentFolderUuid",
                "isMagic",
                "isInTrash",
            ],
        )?;
        self.db.each(&sql, |r| {
            if let (Some(uuid), Some(model_id)) = (text_at(r, 0), int_at(r, 1)) {
                out.insert(
                    uuid,
                    Folder {
                        model_id,
                        name: text_at(r, 2).unwrap_or_default(),
                        kind: int_at(r, 3),
                        parent: text_at(r, 4),
                        magic: flag_at(r, 5) == Some(true),
                        trashed: flag_at(r, 6) == Some(true),
                    },
                );
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// The photos, with their [`Side`]s in `self.sides`.
    fn photos(&mut self, masters: &HashMap<String, Master>, versions: &[Version]) -> Vec<LrPhoto> {
        let mut by_master: BTreeMap<&str, Vec<&Version>> = BTreeMap::new();
        let mut orphans = 0usize;
        for v in versions {
            match masters.get_key_value(v.master.as_str()) {
                Some((k, _)) => by_master.entry(k.as_str()).or_default().push(v),
                None => orphans += 1,
            }
        }
        if orphans > 0 {
            self.warnings.push(format!(
                "{orphans} version{} whose original isn't in the library (or is in its trash): not imported",
                if orphans == 1 { "" } else { "s" }
            ));
        }
        let mut photos = Vec::new();
        for (master_uuid, mut vs) in by_master {
            let m = &masters[master_uuid];
            vs.sort_by_key(|v| (v.number, v.model_id));
            // iPhoto keeps the unedited version hidden once a photo is edited.
            if vs.iter().any(|v| v.shown != Some(false)) {
                vs.retain(|v| v.shown != Some(false));
            }
            let first = vs[0].uuid.clone();
            for (order, v) in vs.into_iter().enumerate() {
                let master = (order > 0).then(|| first.clone());
                photos.push(photo(m, v, order, master, self.format == Format::IPhoto));
                self.sides.push(Side {
                    model_id: v.model_id,
                    project: v.project.clone(),
                });
            }
        }
        photos
    }

    fn keywords(&mut self, photos: &mut [LrPhoto], index: &HashMap<i64, usize>) -> Result<()> {
        if !self.db.has_columns("RKKeyword", &["modelId", "name"])?
            || !self
                .db
                .has_columns("RKKeywordForVersion", &["versionId", "keywordId"])?
        {
            return Ok(());
        }
        let mut names: HashMap<i64, String> = HashMap::new();
        self.db.each("SELECT modelId, name FROM RKKeyword", |r| {
            if let (Some(id), Some(n)) = (int_at(r, 0), text_at(r, 1)) {
                let n = n.trim();
                if !n.is_empty() {
                    names.insert(id, n.to_owned());
                }
            }
            Ok(())
        })?;
        let mut capped = 0usize;
        self.db.each(
            "SELECT versionId, keywordId FROM RKKeywordForVersion",
            |r| {
                let (Some(&i), Some(name)) = (
                    int_at(r, 0).and_then(|v| index.get(&v)),
                    int_at(r, 1).and_then(|k| names.get(&k)),
                ) else {
                    return Ok(());
                };
                let k = photos[i].keywords.get_or_insert_with(Vec::new);
                if k.len() >= MAX_KEYWORDS {
                    capped += 1;
                } else if !k.contains(name) {
                    k.push(name.clone());
                }
                Ok(())
            },
        )?;
        if capped > 0 {
            self.warnings.push(format!(
                "{capped} keywords over the limit of {MAX_KEYWORDS} a photo were not imported"
            ));
        }
        // Photos with none: the library says "no keywords" (an import clears stale ones).
        for p in photos.iter_mut() {
            p.keywords.get_or_insert_with(Vec::new);
        }
        Ok(())
    }

    fn adjustments(&mut self, photos: &mut [LrPhoto]) -> Result<()> {
        if !self
            .db
            .has_columns("RKImageAdjustment", &["versionUuid", "name"])?
        {
            return Ok(());
        }
        let by_uuid: HashMap<String, usize> = photos
            .iter()
            .enumerate()
            .map(|(i, p)| (p.key.clone(), i))
            .collect();
        let sql = self.db.select(
            "RKImageAdjustment",
            &["versionUuid", "name", "adjIndex", "isEnabled", "maskUuid"],
        )?;
        // A blob over the limit is never loaded: SQLite reads its length from
        // the record without its contents, and `CASE` leaves the data out.
        let data = if self.db.has_columns("RKImageAdjustment", &["data"])? {
            format!("CASE WHEN length(data) <= {MAX_ARCHIVE_BYTES} THEN data END, length(data)")
        } else {
            "NULL, NULL".to_owned()
        };
        let sql = sql.replacen(" FROM ", &format!(", {data} FROM "), 1);
        let mut rows: HashMap<usize, Vec<AdjRow>> = HashMap::new();
        self.db.each(&sql, |r| {
            let (Some(i), Some(name)) = (
                text_at(r, 0).and_then(|u| by_uuid.get(&u).copied()),
                text_at(r, 1),
            ) else {
                return Ok(());
            };
            let data = match (blob_at(r, 5), int_at(r, 6)) {
                (Some(b), _) => Data::Bytes(b),
                (None, Some(n)) if usize::try_from(n).map_or(true, |n| n > MAX_ARCHIVE_BYTES) => {
                    Data::TooLarge
                }
                _ => Data::None,
            };
            rows.entry(i).or_default().push((
                int_at(r, 2).unwrap_or(0),
                name,
                data,
                flag_at(r, 3) != Some(false),
                text_at(r, 4).is_some_and(|m| !m.is_empty()),
            ));
            Ok(())
        })?;
        let mut unreadable = 0usize;
        for (i, mut list) in rows {
            list.sort_by_key(|(idx, ..)| *idx);
            let dev = photos[i].develop.get_or_insert_with(Develop::new);
            let mut count: HashMap<String, usize> = HashMap::new();
            for (_, name, data, enabled, masked) in list {
                let n = count.entry(name.clone()).or_default();
                *n += 1;
                let op = if *n == 1 { name } else { format!("{name}~{n}") };
                dev.insert(format!("{op}.present"), Setting::Bool(true));
                if !enabled {
                    dev.insert(format!("{op}.enabled"), Setting::Bool(false));
                }
                if masked {
                    dev.insert(format!("{op}.mask"), Setting::Bool(true));
                }
                let decoded = match data {
                    Data::Bytes(d) => {
                        // Each decode is bounded; between them, stop when asked.
                        self.db.ctx.check()?;
                        archive::decode(&d)
                    }
                    Data::TooLarge => Err("too large".into()),
                    Data::None => continue,
                };
                match decoded {
                    Ok(params) => {
                        for (k, v) in params {
                            dev.insert(format!("{op}.{k}"), v);
                        }
                    }
                    Err(_) => unreadable += 1,
                }
            }
        }
        if unreadable > 0 {
            self.warnings.push(format!(
                "{unreadable} adjustment{} couldn't be read: listed as not imported",
                if unreadable == 1 { "" } else { "s" }
            ));
        }
        Ok(())
    }

    fn albums(
        &self,
        folders: &HashMap<String, Folder>,
        photos: &[LrPhoto],
        index: &HashMap<i64, usize>,
    ) -> Result<Vec<LrCollection>> {
        if !self.db.has_columns("RKAlbum", &["modelId"])? {
            return Ok(Vec::new());
        }
        let sql = self.db.select(
            "RKAlbum",
            &[
                "modelId",
                "name",
                "albumType",
                "albumSubclass",
                "folderUuid",
                "isMagic",
                "isInTrash",
            ],
        )?;
        let mut albums: Vec<LrCollection> = Vec::new();
        self.db.each(&sql, |r| {
            let Some(id) = int_at(r, 0) else {
                return Ok(());
            };
            if flag_at(r, 5) == Some(true) || flag_at(r, 6) == Some(true) {
                return Ok(());
            }
            let (kind, subclass) = (int_at(r, 2), int_at(r, 3));
            let (smart, unsupported) = match subclass {
                Some(2) => (
                    true,
                    Some("a smart album (a saved search): Darkslide can't read its rules".into()),
                ),
                Some(3) | None if matches!(kind, None | Some(1)) => (false, None),
                Some(3) | None => (
                    false,
                    Some(
                        "a book, slideshow, light table or web page: Darkslide has no album for it"
                            .into(),
                    ),
                ),
                // Built in (the library, the trash, "Flagged"…).
                _ => return Ok(()),
            };
            albums.push(LrCollection {
                id,
                name: text_at(r, 1).unwrap_or_default(),
                parents: text_at(r, 4)
                    .map(|f| ancestry(folders, &f, true))
                    .unwrap_or_default(),
                smart,
                members: Vec::new(),
                unsupported,
            });
            Ok(())
        })?;
        if albums.is_empty()
            || !self
                .db
                .has_columns("RKAlbumVersion", &["albumId", "versionId"])?
        {
            return Ok(albums);
        }
        let at: HashMap<i64, usize> = albums.iter().enumerate().map(|(i, a)| (a.id, i)).collect();
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        // In the order they were added, when the table says.
        let sql = if self.db.has_columns("RKAlbumVersion", &["modelId"])? {
            "SELECT albumId, versionId FROM RKAlbumVersion ORDER BY modelId"
        } else {
            "SELECT albumId, versionId FROM RKAlbumVersion"
        };
        self.db.each(sql, |r| {
            if let (Some(&a), Some(&p)) = (
                int_at(r, 0).and_then(|a| at.get(&a)),
                int_at(r, 1).and_then(|v| index.get(&v)),
            ) && seen.insert((a, p))
            {
                albums[a].members.push(photos[p].key.clone());
            }
            Ok(())
        })?;
        Ok(albums)
    }

    /// Say which volumes of referenced originals aren't connected.
    fn missing_volumes(&mut self, masters: &HashMap<String, Master>) {
        let mut missing: BTreeMap<&str, usize> = BTreeMap::new();
        for m in masters.values() {
            if let Some(v) = &m.volume
                && !self.volumes.join(v).is_dir()
            {
                *missing.entry(v.as_str()).or_default() += 1;
            }
        }
        for (v, n) in missing {
            self.warnings.push(format!(
                "{n} referenced original{} on \u{201c}{v}\u{201d}, which isn't connected: \
                 connect it and choose the library again to import {}",
                if n == 1 { " is" } else { "s are" },
                if n == 1 { "it" } else { "them" }
            ));
        }
    }
}

/// The names of the folders around `uuid`, outermost first. With `inclusive`,
/// `uuid` itself is one of them (an album's folder or project).
fn ancestry(folders: &HashMap<String, Folder>, uuid: &str, inclusive: bool) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let mut at = if inclusive {
        Some(uuid.to_owned())
    } else {
        folders.get(uuid).and_then(|f| f.parent.clone())
    };
    while let Some(u) = at.take() {
        if names.len() >= MAX_ANCESTORS || !seen.insert(u.clone()) {
            break;
        }
        let Some(f) = folders.get(&u) else { break };
        if !f.magic && !f.name.trim().is_empty() {
            names.push(f.name.trim().to_owned());
        }
        at.clone_from(&f.parent);
    }
    names.reverse();
    names
}

/// Projects (Aperture) and events (iPhoto): `RKFolder` rows of type 2, with the
/// versions whose `projectUuid` names them. Their ids are negative, apart from albums'.
fn projects(
    folders: &HashMap<String, Folder>,
    photos: &[LrPhoto],
    sides: &[Side],
) -> Vec<LrCollection> {
    let mut members: HashMap<&str, Vec<String>> = HashMap::new();
    for (p, side) in photos.iter().zip(sides) {
        if let Some(proj) = side.project.as_deref() {
            members.entry(proj).or_default().push(p.key.clone());
        }
    }
    let mut out: Vec<LrCollection> = folders
        .iter()
        .filter(|(_, f)| f.kind == Some(2) && !f.magic && !f.trashed)
        .filter_map(|(uuid, f)| {
            let m = members.remove(uuid.as_str())?;
            Some(LrCollection {
                id: -f.model_id,
                name: f.name.trim().to_owned(),
                parents: ancestry(folders, uuid, false),
                smart: false,
                members: m,
                unsupported: None,
            })
        })
        .collect();
    out.sort_by_key(|c| -c.id);
    out
}

/// `iphoto`: the library is iPhoto's, where Hidden is the user hiding a photo (a reject).
fn photo(m: &Master, v: &Version, order: usize, master: Option<String>, iphoto: bool) -> LrPhoto {
    let rejected = v.rating == Some(-1) || (iphoto && v.hidden);
    let rating = match v.rating {
        Some(-1) => Rating::new(0),
        Some(n) => u8::try_from(n).ok().and_then(Rating::new),
        None => None,
    };
    let pick = if rejected {
        Some(PickState::Rejected)
    } else {
        v.flagged.map(|f| {
            if f {
                PickState::Picked
            } else {
                PickState::Unflagged
            }
        })
    };
    let (color_tag, unmapped_label) = label(v.label);
    let title = v
        .name
        .clone()
        .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case(&m.stem));
    let capture_time_ms = match (v.date, v.tz_offset) {
        (Some(d), Some(off)) if d.abs() < 1e11 => {
            Some(((d as i64) + APPLE_EPOCH + off).saturating_mul(1000))
        }
        _ => None,
    };
    let develop = v
        .rotation
        .filter(|r| r.abs() > 1e-6)
        .map(|r| Develop::from([(ROTATION.to_owned(), Setting::Num(r))]));
    LrPhoto {
        key: v.uuid.clone(),
        path: m.path.clone(),
        master,
        copy_name: (order > 0).then(|| v.name.clone()).flatten(),
        order,
        capture_time_ms,
        rating,
        pick,
        color_tag,
        unmapped_label,
        title: Some(title.unwrap_or_default()),
        geotag: v.place,
        folder: m.folder.clone(),
        develop,
        ..LrPhoto::default()
    }
}

/// Aperture's colour labels, in its menu's order (UNCONFIRMED: Apple never
/// documented `colorLabelIndex`; 0 or none is no label).
fn label(i: Option<i64>) -> (Option<ColorTag>, Option<String>) {
    match i {
        None | Some(0) => (Some(ColorTag::None), None),
        Some(1) => (Some(ColorTag::Red), None),
        Some(2) => (Some(ColorTag::Orange), None),
        Some(3) => (Some(ColorTag::Yellow), None),
        Some(4) => (Some(ColorTag::Green), None),
        Some(5) => (Some(ColorTag::Blue), None),
        Some(6) => (Some(ColorTag::Purple), None),
        Some(7) => (Some(ColorTag::None), Some("Gray".into())),
        Some(n) => (Some(ColorTag::None), Some(format!("Label {n}"))),
    }
}

fn read_captions(
    path: &Path,
    tmp: &Path,
    cancel: &(dyn Fn() -> bool + Sync),
    deadline: Instant,
) -> Result<Option<HashMap<i64, String>>> {
    let copy = copy_db(path, tmp, "Properties.apdb")?;
    let ctx = Ctx {
        path,
        cancel,
        deadline,
    };
    let (conn, tables) = open(&copy, &ctx)?;
    let db = Db { ctx, conn, tables };
    let mut out = HashMap::new();
    if !db.has_columns("RKIptcProperty", &["versionId", "propertyKey", "stringId"])?
        || !db.has_columns("RKUniqueString", &["modelId", "stringProperty"])?
    {
        return Ok(None);
    }
    watched(&db.conn, cancel, || {
        db.each(
            "SELECT i.versionId, s.stringProperty FROM RKIptcProperty i \
             JOIN RKUniqueString s ON i.stringId = s.modelId \
             WHERE i.propertyKey = 'Caption/Abstract'",
            |r| {
                if let (Some(v), Some(s)) = (int_at(r, 0), text_at(r, 1)) {
                    out.insert(v, s.trim().to_owned());
                }
                Ok(())
            },
        )
    })?;
    Ok(Some(out))
}

#[cfg(test)]
#[path = "apdb_tests.rs"]
mod tests;
