//! The map: where photos were taken, across every registered session.
//!
//! A variant's position is its geotag (sidecar `geotag`, set by hand on the
//! map) or else its original's GPS (EXIF/XMP, from the places scan). Each
//! session's positions reach the global index with its find rows
//! ([`GlobalIndex::sync_find_items`]); [`GlobalIndex::map_points`] lists them.

use std::path::PathBuf;

use rusqlite::Connection;

use ds_core::PickState;

use crate::db::has_column;
use crate::error::Result;
use crate::filter::pick_from_code;
use crate::global::GlobalIndex;

/// One located original of some session.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalMapPoint {
    pub session_root: PathBuf,
    pub session_name: String,
    /// The original's path (as the app shows it).
    pub path: PathBuf,
    pub latitude: f64,
    pub longitude: f64,
    pub capture_ms: Option<i64>,
    /// The best rating of its variants and the master's flag (for the
    /// browser's filter).
    pub rating: u8,
    pub pick: PickState,
}

/// Global index v4 -> v5 (v4 is metadata's): `find_items.lat`/`lon` (rows
/// get positions on their next sync), then the version.
pub(crate) fn find_items_to_v5(conn: &Connection, version: i64) -> Result<()> {
    ensure_positions(conn)?;
    conn.execute_batch(&format!("PRAGMA user_version={version}"))?;
    Ok(())
}

/// Add `find_items.lat`/`lon` when missing: run on every open, so a file
/// stamped by another build without them heals instead of failing Find's
/// queries. Idempotent: two openers racing both run it.
pub(crate) fn ensure_positions(conn: &Connection) -> Result<()> {
    if has_column(conn, "find_items", "lat")? {
        return Ok(());
    }
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    if !has_column(&tx, "find_items", "lat")? {
        tx.execute_batch(
            "ALTER TABLE find_items ADD COLUMN lat REAL; ALTER TABLE find_items ADD COLUMN lon REAL;",
        )?;
    }
    tx.commit()?;
    Ok(())
}

impl GlobalIndex {
    /// Every located original of every session, by session name then path.
    pub fn map_points(&self) -> Result<Vec<GlobalMapPoint>> {
        let mut st = self.conn.prepare_cached(
            "SELECT s.root, s.name, f.path, f.lat, f.lon, f.capture_ms, f.rating, f.pick
             FROM find_items f JOIN sessions s ON s.id = f.session_id
             WHERE f.lat IS NOT NULL AND f.lon IS NOT NULL
             ORDER BY s.name, s.root, f.path",
        )?;
        Ok(st
            .query_map([], |r| {
                Ok(GlobalMapPoint {
                    session_root: PathBuf::from(r.get::<_, String>(0)?),
                    session_name: r.get(1)?,
                    path: PathBuf::from(r.get::<_, String>(2)?),
                    latitude: r.get(3)?,
                    longitude: r.get(4)?,
                    capture_ms: r.get(5)?,
                    rating: r.get(6)?,
                    pick: pick_from_code(r.get(7)?),
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use std::fs;
    use std::path::Path;

    use ds_core::geom::Orientation;
    use ds_core::{FileHash, GeoPoint, PickState};

    use crate::*;

    fn session(dir: &Path) -> (Session, Vec<VariantId>, Vec<FileHash>) {
        let root = dir.join("Iceland");
        let mut s = Session::create(&root, "Iceland").unwrap();
        let (mut vs, mut keys) = (Vec::new(), Vec::new());
        for name in ["a", "b", "c"] {
            let rel = format!("Capture/{name}.ARW");
            fs::write(root.join(&rel), name.as_bytes()).unwrap();
            let h = FileHash(*blake3::hash(name.as_bytes()).as_bytes());
            let meta = ImageMetaLite {
                width: 60,
                height: 40,
                orientation: Orientation::Normal,
                capture_time_ms: Some(1_700_000_000_000),
                ..ImageMetaLite::default()
            };
            let id = s.add_original(&rel, h, meta).unwrap();
            vs.push(s.variants(id).unwrap()[0]);
            keys.push(h);
        }
        (s, vs, keys)
    }

    fn vik() -> GeoPoint {
        GeoPoint::new(63.4, -19.0, Some(3.0)).unwrap()
    }

    /// A geotag lands in the sidecar and the index, survives a rebuild from
    /// the sidecars, can be removed again, and is one undo step.
    #[test]
    fn geotags_are_stored_in_sidecars_and_undoable() {
        let d = tempfile::tempdir().unwrap();
        let (mut s, vs, _) = session(d.path());
        assert_eq!(s.variant(vs[0]).unwrap().geotag, None);
        s.set_geotag(&vs[..2], Some(vik())).unwrap();
        assert_eq!(s.variant(vs[0]).unwrap().geotag, Some(vik()));
        assert_eq!(s.variant(vs[1]).unwrap().geotag, Some(vik()));
        assert_eq!(s.variant(vs[2]).unwrap().geotag, None);
        let rel = RelPath::new("Capture/b.ARW").unwrap().sidecar_rel(1);
        let sidecar = fs::read_to_string(s.root().join(rel)).unwrap();
        assert!(sidecar.contains("\"geotag\": {"), "{sidecar}");
        assert!(sidecar.contains("-19.0"), "{sidecar}");
        // The original is never written.
        assert_eq!(fs::read(s.root().join("Capture/a.ARW")).unwrap(), b"a");

        s.rebuild_index().unwrap();
        assert_eq!(s.variant(vs[1]).unwrap().geotag, Some(vik()));

        s.set_geotag(&vs[1..2], None).unwrap();
        assert_eq!(s.variant(vs[1]).unwrap().geotag, None);
        assert!(s.undo(vs[1]).unwrap());
        assert_eq!(s.variant(vs[1]).unwrap().geotag, Some(vik()));
        assert!(s.redo(vs[1]).unwrap());
        assert_eq!(s.variant(vs[1]).unwrap().geotag, None);

        // A new variant inherits the photo's position.
        let image = s.variant(vs[0]).unwrap().image;
        let v2 = s.new_variant(image).unwrap();
        assert_eq!(s.variant(v2).unwrap().geotag, Some(vik()));
        // A position belongs to the photo: tagging one variant moves every
        // variant of it (and the master leads on the global map).
        let elsewhere = GeoPoint::new(-33.86, 151.21, None).unwrap();
        s.set_geotag(&[v2], Some(elsewhere)).unwrap();
        assert_eq!(s.variant(vs[0]).unwrap().geotag, Some(elsewhere));
        assert_eq!(s.variant(v2).unwrap().geotag, Some(elsewhere));
        assert!(s.undo(vs[0]).unwrap(), "one step for both");
        assert_eq!(s.variant(v2).unwrap().geotag, Some(vik()));
        assert_eq!(s.variant(vs[0]).unwrap().geotag, Some(vik()));
    }

    /// Find rows carry the position: the first variant's geotag wins over the
    /// scanned GPS; the global map lists located originals only, and a v3
    /// global index gains the columns in place.
    #[test]
    fn positions_reach_the_global_map() {
        let d = tempfile::tempdir().unwrap();
        let (mut s, vs, keys) = session(d.path());
        s.store_locations(&[
            (
                keys[0],
                PhotoPlace {
                    gps: Some((10.0, 20.0)),
                    ..PhotoPlace::default()
                },
            ),
            (
                keys[1],
                PhotoPlace {
                    names: vec!["Lihue".into()],
                    gps: Some((1.0, 2.0)),
                    stamp: String::new(),
                },
            ),
        ])
        .unwrap();
        s.set_geotag(&vs[1..2], Some(vik())).unwrap();
        let recs = s.find_records().unwrap();
        assert_eq!(recs[0].gps, Some((10.0, 20.0)), "scanned GPS");
        assert_eq!(recs[1].gps, Some((63.4, -19.0)), "the geotag wins");
        assert_eq!(recs[1].location, ["Lihue"]);
        assert_eq!(recs[2].gps, None);

        let gpath = d.path().join("global.db");
        let mut g = GlobalIndex::open(&gpath).unwrap();
        assert!(g.sync_find_items(s.root(), s.name(), &recs).unwrap());
        assert!(!g.sync_find_items(s.root(), s.name(), &recs).unwrap());
        let pts = g.map_points().unwrap();
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].session_name, "Iceland");
        assert_eq!(pts[0].session_root, s.root());
        assert_eq!(pts[0].path, s.root().join("Capture/a.ARW"));
        assert_eq!((pts[1].latitude, pts[1].longitude), (63.4, -19.0));
        assert_eq!(pts[1].capture_ms, Some(1_700_000_000_000));
        assert_eq!((pts[0].rating, pts[0].pick), (0, PickState::Unflagged));
        drop(g);

        // A v3 file (no titles, no positions): 3 -> 4 (metadata's titles) -> 5
        // (positions) -> 6 (the library), rows kept, positions refilled on the next sync.
        let version = |p: &std::path::Path| -> i64 {
            rusqlite::Connection::open(p)
                .unwrap()
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap()
        };
        let c = rusqlite::Connection::open(&gpath).unwrap();
        c.execute_batch(
            "ALTER TABLE find_items DROP COLUMN lat; ALTER TABLE find_items DROP COLUMN lon;
             ALTER TABLE find_items DROP COLUMN titles; PRAGMA user_version=3;",
        )
        .unwrap();
        drop(c);
        let mut g = GlobalIndex::open(&gpath).unwrap();
        assert_eq!(version(&gpath), 6);
        assert!(crate::db::has_column(&g.conn, "find_items", "titles").unwrap());
        assert!(
            g.map_points().unwrap().is_empty(),
            "kept rows, no positions yet"
        );
        assert!(g.sync_find_items(s.root(), s.name(), &recs).unwrap());
        assert_eq!(g.map_points().unwrap().len(), 2);
        drop(g);
        // A v4 file (metadata's: titles, no positions): 4 -> 5 -> 6 keeps its rows.
        let c = rusqlite::Connection::open(&gpath).unwrap();
        c.execute_batch(
            "ALTER TABLE find_items DROP COLUMN lat; ALTER TABLE find_items DROP COLUMN lon;
             PRAGMA user_version=4;",
        )
        .unwrap();
        drop(c);
        let mut g = GlobalIndex::open(&gpath).unwrap();
        assert_eq!(version(&gpath), 6);
        assert!(g.sync_find_items(s.root(), s.name(), &recs).unwrap());
        assert_eq!(g.map_points().unwrap().len(), 2);
        drop(g);
        // A second opener that read v3 first: no duplicate column.
        let c = rusqlite::Connection::open(&gpath).unwrap();
        c.execute_batch("PRAGMA user_version=3").unwrap();
        drop(c);
        let g = GlobalIndex::open(&gpath).unwrap();
        assert_eq!(g.map_points().unwrap().len(), 2);
        drop(g);
        // A file at this version without the columns (another build stamped it) heals on open.
        let c = rusqlite::Connection::open(&gpath).unwrap();
        c.execute_batch(
            "ALTER TABLE find_items DROP COLUMN lat; ALTER TABLE find_items DROP COLUMN lon;",
        )
        .unwrap();
        drop(c);
        let mut g = GlobalIndex::open(&gpath).unwrap();
        assert!(g.sync_find_items(s.root(), s.name(), &recs).unwrap());
        assert_eq!(g.map_points().unwrap().len(), 2);
    }

    /// Eight openers at once on a v3 global index (3 -> 4 -> 5): all succeed, rows kept.
    #[test]
    fn openers_racing_on_a_v3_global_index_all_succeed() {
        let d = tempfile::tempdir().unwrap();
        let (s, _, _) = session(d.path());
        let gpath = d.path().join("global.db");
        let mut g = GlobalIndex::open(&gpath).unwrap();
        g.sync_find_items(s.root(), s.name(), &s.find_records().unwrap())
            .unwrap();
        drop(g);
        let c = rusqlite::Connection::open(&gpath).unwrap();
        c.execute_batch(
            "ALTER TABLE find_items DROP COLUMN lat; ALTER TABLE find_items DROP COLUMN lon;
             ALTER TABLE find_items DROP COLUMN titles; PRAGMA user_version=3;",
        )
        .unwrap();
        drop(c);
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (p, start) = (gpath.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    GlobalIndex::open(&p).unwrap().map_points().unwrap().len()
                })
            })
            .collect();
        for h in handles {
            assert_eq!(
                h.join().unwrap(),
                0,
                "rows kept, positions on the next sync"
            );
        }
        let c = rusqlite::Connection::open(&gpath).unwrap();
        let rows: i64 = c
            .query_row("SELECT count(*) FROM find_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 3);
    }

    /// N4: a hand-written geotag this build can't read survives a geotag set
    /// and its undo exactly as written.
    #[test]
    fn undo_restores_an_unreadable_geotag_as_written() {
        let d = tempfile::tempdir().unwrap();
        let (s, vs, _) = session(d.path());
        let root = s.root().to_path_buf();
        drop(s);
        let path = root.join(RelPath::new("Capture/a.ARW").unwrap().sidecar_rel(1));
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        v["geotag"] = serde_json::json!("near the lighthouse");
        fs::write(&path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();
        let mut s = Session::open(&root).unwrap();
        assert_eq!(s.variant(vs[0]).unwrap().geotag, None);
        s.set_geotag(&vs[..1], Some(vik())).unwrap();
        assert!(s.undo(vs[0]).unwrap());
        let back: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(back["geotag"], "near the lighthouse");
        assert!(s.redo(vs[0]).unwrap());
        assert_eq!(s.variant(vs[0]).unwrap().geotag, Some(vik()));
    }

    /// M3: a geotag written by this build while an older build (index v6, no
    /// geotag columns) kept the index, with unchanged sidecar stats, is found
    /// after the upgrade.
    #[test]
    fn a_geotag_written_under_an_older_index_is_found_after_the_upgrade() {
        let d = tempfile::tempdir().unwrap();
        let (mut s, vs, _) = session(d.path());
        s.set_geotag(&vs[..1], Some(vik())).unwrap();
        let (root, db) = (s.root().to_path_buf(), s.db_path());
        drop(s);
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "ALTER TABLE variants DROP COLUMN geo_lat; ALTER TABLE variants DROP COLUMN geo_lon;
             ALTER TABLE variants DROP COLUMN geo_alt; PRAGMA user_version=6;",
        )
        .unwrap();
        drop(c);
        let s = Session::open(&root).unwrap();
        assert_eq!(s.variant(vs[0]).unwrap().geotag, Some(vik()));
        assert_eq!(s.variant(vs[1]).unwrap().geotag, None);
    }
}
