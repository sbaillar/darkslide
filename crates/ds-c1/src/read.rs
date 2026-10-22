//! Read a whole C1 session folder: the database for images, variants and
//! collections, and each image's `.cos` file for its settings (preferred over
//! the database copy when both exist; C1 writes both).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Larger `.cos` files are not read (real ones are a few kB).
pub(crate) const MAX_COS_BYTES: u64 = 8 << 20;

use crate::cos::parse_cos;
use crate::db::{join_rel, read_db_cancellable};
use crate::error::{C1Error, IoAt, Result};
use crate::model::{C1Image, C1Session, C1Variant, Settings, Source};
use crate::settings::Cancel;

/// Deepest folder level searched for `CaptureOne/` settings folders.
const MAX_DEPTH: usize = 8;

/// The `*.cosessiondb` at the top of `folder` (the one named after the
/// folder if there are several, else the first by name).
pub fn find_session_db(folder: &Path) -> Option<PathBuf> {
    let mut dbs: Vec<PathBuf> = fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| x.eq_ignore_ascii_case("cosessiondb"))
        })
        .collect();
    dbs.sort();
    let named = folder.file_name().map(|n| n.to_string_lossy().into_owned());
    let pos = dbs.iter().position(|p| {
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned());
        stem.zip(named.as_ref())
            .is_some_and(|(s, n)| s.eq_ignore_ascii_case(n))
    });
    let i = pos.unwrap_or(0);
    (i < dbs.len()).then(|| dbs.swap_remove(i))
}

/// Read the session in `folder` (read-only; see the crate docs).
pub fn read_session(folder: &Path) -> Result<C1Session> {
    let never: Cancel = Arc::new(|| false);
    read_session_cancellable(folder, &never)
}

/// [`read_session`], stopping when `cancel` says so (checked inside every
/// database query).
pub fn read_session_cancellable(folder: &Path, cancel: &Cancel) -> Result<C1Session> {
    let db_path = find_session_db(folder).ok_or_else(|| C1Error::NotASession(folder.to_owned()))?;
    let db = read_db_cancellable(&db_path, folder, cancel)?;
    let mut out = C1Session {
        folder: folder.to_owned(),
        uuid: db.uuid,
        written_by: db.written_by,
        collections: db.collections,
        keyword_library: db.keyword_library,
        ..C1Session::default()
    };

    let mut cos: BTreeMap<String, PathBuf> = BTreeMap::new();
    find_cos(folder, "", 0, &mut cos).at(folder)?;

    let mut by_rel: HashMap<String, usize> = HashMap::new();
    for img in db.images {
        let rel = img.rel_path(folder);
        let outside = if rel.is_none() {
            img.absolute_path()
        } else {
            None
        };
        let variants = img
            .variants
            .into_iter()
            .enumerate()
            .map(|(position, v)| C1Variant {
                uuid: v.uuid,
                position,
                settings: v.settings,
                source: Source::Db,
            })
            .collect();
        if let Some(r) = &rel {
            by_rel.insert(r.clone(), out.images.len());
        }
        out.images.push(C1Image {
            outside,
            rel,
            file_name: img.file_name,
            uuid: img.uuid,
            variants,
        });
    }

    for (rel, path) in cos {
        let parsed = match ds_fsutil::read_regular_capped(&path, MAX_COS_BYTES, false)
            .map_err(|e| e.to_string())
            .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()))
            .and_then(|x| parse_cos(&x))
        {
            Ok(p) => p,
            Err(e) => {
                out.warnings.push(format!(
                    "{}: {e} (settings read from the database)",
                    path.display()
                ));
                continue;
            }
        };
        let idx = match by_rel.get(&rel) {
            Some(i) => *i,
            // A settings file for an image the database doesn't list: only
            // if the image is there (C1 leaves .cos files of deleted images).
            None if folder.join(&rel).is_file() => {
                by_rel.insert(rel.clone(), out.images.len());
                out.images.push(C1Image {
                    file_name: rel.rsplit('/').next().unwrap_or_default().to_owned(),
                    uuid: parsed.image.get("UUID").cloned(),
                    rel: Some(rel.clone()),
                    outside: None,
                    variants: Vec::new(),
                });
                out.images.len() - 1
            }
            None => continue,
        };
        let image = &mut out.images[idx];
        for (i, v) in parsed.variants.into_iter().enumerate() {
            let slot = match &v.uuid {
                Some(u) => image.variants.iter().position(|x| &x.uuid == u),
                // No UUID: the variant in the same position, if any.
                None => (i < image.variants.len()).then_some(i),
            };
            set_variant(image, slot, v.uuid, v.settings, &rel, i);
        }
    }
    Ok(out)
}

fn set_variant(
    image: &mut C1Image,
    slot: Option<usize>,
    uuid: Option<String>,
    settings: Settings,
    rel: &str,
    i: usize,
) {
    if let Some(s) = slot {
        image.variants[s].settings = settings;
        image.variants[s].source = Source::Cos;
    } else {
        let position = image.variants.len();
        image.variants.push(C1Variant {
            uuid: uuid.unwrap_or_else(|| format!("cos:{rel}#{i}")),
            position,
            settings,
            source: Source::Cos,
        });
    }
}

/// Collect `<rel dir>/CaptureOne/Settings*/<file>.cos` → (`<rel dir>/<file>`,
/// path). When several `Settings*` folders hold the same file (C1 versions),
/// the highest-numbered one wins.
fn find_cos(
    folder: &Path,
    rel: &str,
    depth: usize,
    out: &mut BTreeMap<String, PathBuf>,
) -> std::io::Result<()> {
    let dir = folder.join(rel);
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) if depth > 0 => return Ok(()),
        Err(e) => return Err(e),
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if name == "CaptureOne" {
            let mut settings: Vec<(u32, PathBuf)> = fs::read_dir(e.path())
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|s| {
                    let n = s.file_name().to_string_lossy().into_owned();
                    let num = n.strip_prefix("Settings")?.parse().ok()?;
                    Some((num, s.path()))
                })
                .collect();
            settings.sort();
            for (_, sdir) in settings {
                for f in fs::read_dir(&sdir).into_iter().flatten().flatten() {
                    let n = f.file_name().to_string_lossy().into_owned();
                    // Never a link or special file (a hostile folder's `x.cos -> /dev/zero`).
                    if let Some(image) = n.strip_suffix(".cos").filter(|i| !i.is_empty())
                        && f.file_type().is_ok_and(|t| t.is_file())
                    {
                        out.insert(join_rel(rel, image), f.path());
                    }
                }
            }
        } else if depth < MAX_DEPTH && !(depth == 0 && name.eq_ignore_ascii_case("Output")) {
            find_cos(folder, &join_rel(rel, &name), depth + 1, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use crate::model::C1Folder;

    #[test]
    fn finds_the_database_named_after_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("Shoot");
        fs::create_dir(&f).unwrap();
        assert_eq!(find_session_db(&f), None);
        assert!(matches!(read_session(&f), Err(C1Error::NotASession(_))));
        fs::write(f.join("A.cosessiondb"), b"").unwrap();
        assert_eq!(find_session_db(&f), Some(f.join("A.cosessiondb")));
        fs::write(f.join("shoot.COSESSIONDB"), b"").unwrap();
        assert_eq!(find_session_db(&f), Some(f.join("shoot.COSESSIONDB")));
        fs::create_dir(f.join("dir.cosessiondb")).unwrap();
        assert_eq!(find_session_db(&dir.path().join("missing")), None);
    }

    #[test]
    fn merges_database_and_settings_files() {
        let dir = tempfile::tempdir().unwrap();
        fixtures::build_session(dir.path(), &|_| b"raw".to_vec());
        let s = read_session(dir.path()).unwrap();
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
        assert_eq!(s.uuid.as_deref(), Some("DOC-UUID-1"));
        let rels: Vec<_> = s.images.iter().map(|i| i.rel.clone()).collect();
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
        assert_eq!(
            s.images[4].outside,
            Some(PathBuf::from("/Volumes/Elsewhere/pics/e.ARW"))
        );
        let a = &s.images[0];
        assert_eq!(
            a.variants.iter().map(|v| v.source).collect::<Vec<_>>(),
            vec![Source::Cos, Source::Cos]
        );
        assert_eq!(
            a.variants[0].settings.edit_f64("Exposure"),
            Some(0.5),
            ".cos wins over the database (0.25)"
        );
        assert_eq!(s.images[1].variants[0].source, Source::Db, "no .cos for b");
        assert_eq!(
            s.images[1].variants[0].settings.edit_f64("Brightness"),
            Some(10.0)
        );
        assert_eq!(s.images[2].folder(), Some(C1Folder::Selects));
        assert_eq!(
            s.images[2].variants[0].settings.get("Basic_Rating"),
            Some("2")
        );
        assert_eq!(s.images[3].folder(), Some(C1Folder::Trash));
        assert_eq!(s.collections.len(), 4);
    }

    #[test]
    fn settings_files_that_are_links_or_devices_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fixtures::build_session(dir.path(), &|_| b"raw".to_vec());
        let cos = dir.path().join("CaptureOne/Settings153/a.ARW.cos");
        fs::remove_file(&cos).unwrap();
        // Would allocate forever if followed.
        std::os::unix::fs::symlink("/dev/zero", &cos).unwrap();
        let s = read_session(dir.path()).unwrap();
        let sources: Vec<_> = s.images[0].variants.iter().map(|v| v.source).collect();
        assert!(
            !sources.is_empty() && sources.iter().all(|&x| x == Source::Db),
            "settings come from the database, the link was never read: {sources:?}"
        );
    }

    #[test]
    fn unreadable_settings_fall_back_and_orphans_need_their_image() {
        let dir = tempfile::tempdir().unwrap();
        fixtures::build_session(dir.path(), &|_| b"raw".to_vec());
        let settings = dir.path().join("CaptureOne/Settings153");
        fs::write(settings.join("a.ARW.cos"), "<IMG>").unwrap();
        // A newer Settings folder overrides an older one.
        let newer = dir.path().join("Selects/CaptureOne/Settings160");
        fs::create_dir_all(&newer).unwrap();
        fs::write(
            newer.join("c.ARW.cos"),
            fixtures::COS_C.replace(r#"V="2""#, r#"V="3""#),
        )
        .unwrap();
        // Orphan settings: one whose image is gone, one whose image the database doesn't know.
        fs::write(settings.join("gone.ARW.cos"), fixtures::COS_D).unwrap();
        fs::write(
            settings.join("new.ARW.cos"),
            fixtures::COS_D
                .replace("VAR-D1", "VAR-N1")
                .replace("IMG-D", "IMG-N"),
        )
        .unwrap();
        fs::write(dir.path().join("new.ARW"), b"raw").unwrap();
        // Something in Output never counts.
        let out = dir.path().join("Output/CaptureOne/Settings153");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("x.jpg.cos"), fixtures::COS_D).unwrap();
        let s = read_session(dir.path()).unwrap();
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("a.ARW.cos"), "{:?}", s.warnings);
        assert_eq!(s.images[0].variants[0].source, Source::Db);
        assert_eq!(
            s.images[0].variants[0].settings.edit_f64("Exposure"),
            Some(0.25)
        );
        assert_eq!(
            s.images[2].variants[0].settings.get("Basic_Rating"),
            Some("3")
        );
        let new = s
            .images
            .iter()
            .find(|i| i.rel.as_deref() == Some("new.ARW"))
            .unwrap();
        assert_eq!(new.uuid.as_deref(), Some("IMG-N"));
        assert_eq!(new.variants[0].uuid, "VAR-N1");
        assert!(
            s.images
                .iter()
                .all(|i| i.rel.as_deref() != Some("gone.ARW"))
        );
        assert!(
            s.images
                .iter()
                .all(|i| i.rel.as_deref() != Some("Output/x.jpg"))
        );
    }

    #[test]
    fn variants_without_uuid_match_by_position_or_are_added() {
        let mut img = C1Image {
            rel: Some("a".into()),
            outside: None,
            file_name: "a".into(),
            uuid: None,
            variants: vec![],
        };
        set_variant(&mut img, None, None, Settings::default(), "a", 0);
        assert_eq!(img.variants[0].uuid, "cos:a#0");
        set_variant(&mut img, Some(0), None, Settings::default(), "a", 0);
        assert_eq!(img.variants.len(), 1);
        let dir = tempfile::tempdir().unwrap();
        fixtures::build_session(dir.path(), &|_| b"raw".to_vec());
        let no_uuid = fixtures::COS_C.replace(r#"<E K="UUID" V="VAR-C1" />"#, "");
        fs::write(
            dir.path().join("Selects/CaptureOne/Settings153/c.ARW.cos"),
            no_uuid,
        )
        .unwrap();
        let s = read_session(dir.path()).unwrap();
        let c = &s.images[2];
        assert_eq!(c.variants.len(), 1);
        assert_eq!(
            (c.variants[0].uuid.as_str(), c.variants[0].source),
            ("VAR-C1", Source::Cos)
        );
    }

    #[test]
    fn an_unreadable_folder_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = BTreeMap::new();
        assert!(find_cos(&dir.path().join("missing"), "", 0, &mut out).is_err());
        assert!(find_cos(dir.path(), "missing", 1, &mut out).is_ok());
    }
}
