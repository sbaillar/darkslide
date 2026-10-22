//! The map (see `docs/FFI.md`): geotags set by hand, and every session's
//! located photos.
//!
//! - [`SessionHandle::set_geotag`]: the position goes into the sidecars of
//!   every variant of the photos (`geotag`, undoable) and never into the
//!   original; `VariantRecord.geotag` lists it. One order everywhere: the
//!   geotag, then the file's EXIF GPS, then its XMP GPS
//!   (`VariantRecord.file_gps` once scanned, else `RawMetadata.gps`), in the
//!   app and in exports (`ExportSource.gps`).
//! - [`global_map_points`]: the located originals of every session, as their
//!   last `sync_find` left them (a geotag, else GPS from the places scan).

use std::path::Path;
use std::sync::Arc;

use super::search::with_global;
use super::{CancelToken, DsError, PickFlag, SessionHandle, lock, run_job};
use crate::raw::GeoPosition;

/// A located original of some session.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct MapPointRecord {
    /// What to open for this session (the in-place folder or the imported session).
    pub session_root: String,
    pub session_name: String,
    /// The original.
    pub path: String,
    pub latitude: f64,
    pub longitude: f64,
    pub capture_time_ms: Option<i64>,
    /// Best rating of its variants and the master's flag (the browser's filter applies).
    pub rating: u8,
    pub pick: PickFlag,
}

#[uniffi::export]
impl SessionHandle {
    /// Set where `ids` were taken, or with `None` remove the geotag (the
    /// file's own GPS shows again). One undo step.
    pub async fn set_geotag(
        self: Arc<Self>,
        ids: Vec<i64>,
        position: Option<GeoPosition>,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        let point = position.map(GeoPosition::to_point).transpose()?;
        run_job(token, move |_| {
            self.check_folder()?;
            Ok(lock(&self.session).set_geotag(&Self::ids(&ids), point)?)
        })
        .await
    }
}

/// Every session's located photos (see the module docs).
#[uniffi::export]
pub async fn global_map_points(
    global_index: String,
    token: Arc<CancelToken>,
) -> Result<Vec<MapPointRecord>, DsError> {
    run_job(token, move |_| {
        let points = with_global(Path::new(&global_index), |g| Ok(g.map_points()?))?;
        Ok(points
            .into_iter()
            .map(|p| MapPointRecord {
                session_root: p.session_root.to_string_lossy().into_owned(),
                session_name: p.session_name,
                path: p.path.to_string_lossy().into_owned(),
                latitude: p.latitude,
                longitude: p.longitude,
                capture_time_ms: p.capture_ms,
                rating: p.rating,
                pick: p.pick.into(),
            })
            .collect())
    })
    .await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use futures_executor::block_on;

    use super::super::people::tests::{FP, open};
    use super::super::tests::{by_name, list};
    use super::*;

    fn tok() -> Arc<CancelToken> {
        CancelToken::new()
    }

    const VIK: GeoPosition = GeoPosition {
        latitude: 63.4,
        longitude: -19.0,
        altitude_m: Some(3.0),
    };

    /// Set, list, remove, reject out-of-range positions; the synced session
    /// reaches the global map; the original is never written.
    #[test]
    fn geotags_round_trip_and_reach_the_global_map() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.db").to_string_lossy().into_owned();
        let people = dir
            .path()
            .join("people.json")
            .to_string_lossy()
            .into_owned();
        let folder = dir.path().join("Iceland");
        let h = open(&folder, &[("a.ARW", &b"a"[..]), ("b.ARW", b"bb")]);
        let v = list(&h);
        let (a, b) = (
            by_name(&v, "a.ARW").variant_id,
            by_name(&v, "b.ARW").variant_id,
        );
        assert_eq!(by_name(&v, "a.ARW").geotag, None);

        block_on(h.clone().set_geotag(vec![a, b], Some(VIK), tok())).unwrap();
        let v = list(&h);
        assert_eq!(by_name(&v, "a.ARW").geotag, Some(VIK));
        block_on(h.clone().set_geotag(vec![b], None, tok())).unwrap();
        let v = list(&h);
        assert_eq!(by_name(&v, "b.ARW").geotag, None);
        let bad = GeoPosition {
            latitude: f64::NAN,
            ..VIK
        };
        assert!(matches!(
            block_on(h.clone().set_geotag(vec![a], Some(bad), tok())),
            Err(DsError::InvalidArgument { .. })
        ));
        assert_eq!(by_name(&list(&h), "a.ARW").geotag, Some(VIK), "unchanged");
        assert_eq!(std::fs::read(folder.join("a.ARW")).unwrap(), b"a");

        // The file's own position (here XMP) once the places scan read it.
        assert_eq!(by_name(&list(&h), "b.ARW").file_gps, None);
        std::fs::write(
            folder.join("b.xmp"),
            "<x:xmpmeta exif:GPSLatitude='10,30N' exif:GPSLongitude='20W'/>",
        )
        .unwrap();
        block_on(h.clone().set_rating(vec![b], 4, tok())).unwrap();
        while block_on(h.clone().scan_locations(64, tok())).unwrap() > 0 {}
        let fg = by_name(&list(&h), "b.ARW").file_gps.unwrap();
        assert_eq!(
            (fg.latitude, fg.longitude, fg.altitude_m),
            (10.5, -20.0, None)
        );

        assert_eq!(
            block_on(global_map_points(global.clone(), tok()))
                .unwrap()
                .len(),
            0
        );
        block_on(
            h.clone()
                .sync_find(global.clone(), FP.into(), people, 0.6, tok()),
        )
        .unwrap();
        let pts = block_on(global_map_points(global, tok())).unwrap();
        assert_eq!(pts.len(), 2, "a by its geotag, b by its file's position");
        assert_eq!(pts[0].session_name, "Iceland");
        assert!(pts[0].path.ends_with("a.ARW"), "{}", pts[0].path);
        assert_eq!((pts[0].latitude, pts[0].longitude), (63.4, -19.0));
        assert!(pts[0].capture_time_ms.is_some());
        assert_eq!(
            (pts[1].latitude, pts[1].rating, pts[1].pick),
            (10.5, 4, PickFlag::Unflagged)
        );

        let t = tok();
        t.cancel();
        assert_eq!(
            block_on(global_map_points("/nowhere/g.db".into(), t)),
            Err(DsError::Cancelled)
        );
    }
}
