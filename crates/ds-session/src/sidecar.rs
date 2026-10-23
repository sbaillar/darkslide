//! Per-variant sidecar JSON: the source of truth for edits and organization.

use ds_core::geom::{NormRect, Orientation, Size};
use ds_core::{
    Adjustments, ColorTag, FileHash, GeoPoint, IptcMeta, PickState, Rating, SIDECAR_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The minimal image metadata the session needs for filtering and sorting.
/// Filled by the importer from `ds-raw` metadata; kept independent of it here.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageMetaLite {
    /// Capture time, milliseconds since the Unix epoch (camera clock, treated as UTC).
    pub capture_time_ms: Option<i64>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<u32>,
    pub focal_length_mm: Option<f32>,
    /// Stored (sensor) pixel size, before EXIF orientation.
    pub width: u32,
    pub height: u32,
    pub orientation: Orientation,
    /// The camera's as-shot white balance as (Kelvin, tint), when known.
    /// Lets relative WB paste work on images still at "As Shot".
    pub as_shot_wb: Option<(f32, f32)>,
}

impl ImageMetaLite {
    /// Display-upright size (what crop paste works in).
    pub fn oriented_size(&self) -> Size {
        self.orientation
            .apply_to(Size::new(self.width, self.height))
    }
}

/// One variant's sidecar file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sidecar {
    pub schema_version: u32,
    /// Stable ids; the index is rebuilt with these so albums, undo, and cache keys survive.
    pub variant_id: i64,
    pub image_id: i64,
    /// 1 for the first variant of an image; selects the sidecar file location.
    pub ordinal: u32,
    #[serde(default)]
    pub name: String,
    #[serde(with = "hex_hash")]
    pub hash: FileHash,
    #[serde(default)]
    pub meta: ImageMetaLite,
    #[serde(default)]
    pub adjustments: Adjustments,
    #[serde(default)]
    pub rating: Rating,
    #[serde(default)]
    pub color_tag: ColorTag,
    #[serde(default)]
    pub pick: PickState,
    /// Why AI Cull rejected the photo, e.g. "Out of focus (sharpness 0.12, best
    /// in group 0.71)". Only kept while `pick` is the AI's reject: any change of
    /// `pick` clears it. Additive: absent in older sidecars (no note); written
    /// as `null` when cleared so a rewrite never carries an old note over from
    /// the file.
    #[serde(default)]
    pub cull_note: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Title, caption, credits, location and rights (IPTC Core). Additive:
    /// absent in older sidecars; every field is always written, so a rewrite
    /// never carries a cleared value over from the file.
    #[serde(default)]
    pub iptc: IptcMeta,
    /// BLAKE3 of the original's sensor data only (TIFF-based RAWs), so a
    /// metadata-only rewrite of the file (exiftool, geotaggers) keeps its
    /// identity. `None`: not known (hashed before, or not a TIFF RAW), and
    /// `hash` (the whole file) is the identity.
    #[serde(default, with = "hex_hash_opt")]
    pub content_hash: Option<FileHash>,
    /// Set when this photo's file replaced a different photo at the same
    /// path ("file changed"): the variant ids of the previous photo, whose
    /// sidecars wait in `Trash/.ds/deleted/` so its metadata can be restored.
    #[serde(default)]
    pub replaced: Vec<i64>,
    /// The last applied AI Optimize report (`ds_pipeline::optimize::OptimizeReport`
    /// JSON, kept opaque here), so the app can say later why the edit has its values.
    /// Additive: absent in older sidecars, and older builds keep it on rewrite. Written
    /// as `null` when cleared, so a rewrite doesn't carry the old report over from the
    /// file (unknown keys are merged back).
    #[serde(default)]
    pub optimize_report: Option<Value>,
    /// The tool rows AI edits (AI Optimize, the ⌘K edit bar, Auto Adjust) changed and
    /// their values before (the app's JSON, kept opaque here), so the app can list and
    /// undo them after a relaunch. Additive: absent in older sidecars; written as `null`
    /// when cleared, like `optimize_report`.
    #[serde(default)]
    pub ai_changes: Option<Value>,
    /// People on this photo (confirmed by the user), named as in the app's
    /// people store. Additive: absent in older sidecars, and written as `[]`
    /// when empty so a rewrite never carries old tags over from the file.
    #[serde(default)]
    pub people: Vec<PersonTag>,
    /// "Not this person" answers, so a rejected suggestion doesn't come back.
    #[serde(default)]
    pub people_rejected: Vec<PersonTag>,
    /// Where the photo was taken, set by hand on the map: wins over the
    /// original's GPS in the app and in exports (the original is never
    /// written). Additive: absent in older sidecars; written as `null` when
    /// removed so a rewrite never carries an old position over from the file.
    /// A value that can't be read (edited by hand: out of range, wrong type)
    /// reads as none and never makes the rest of the sidecar unreadable.
    #[serde(default, deserialize_with = "lenient_geotag")]
    pub geotag: Option<GeoPoint>,
    /// The file's `geotag` as written when this build couldn't read it: a
    /// rewrite writes it back unchanged (until a new position is set or the
    /// geotag is removed; undo restores it).
    #[serde(skip)]
    pub geotag_unreadable: Option<Value>,
}

fn lenient_geotag<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<GeoPoint>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value::<GeoPoint>(v)
        .ok()
        .filter(GeoPoint::is_valid))
}

/// A person on a photo.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PersonTag {
    pub name: String,
    /// The people store's id (a renamed person keeps it).
    pub id: Option<String>,
    /// Their face, normalized to the upright photo (origin top left);
    /// `None` = somewhere on the photo.
    pub region: Option<NormRect>,
}

impl PersonTag {
    /// The same person: the same store id, else (no id on either side) the
    /// same name, ignoring case.
    pub fn same_person(&self, other: &PersonTag) -> bool {
        match (&self.id, &other.id) {
            (Some(a), Some(b)) => a == b,
            _ => self.name.to_lowercase() == other.name.to_lowercase(),
        }
    }

    /// The same face: regions overlapping by at least half (intersection over
    /// union), or no region on either side.
    pub fn same_region(&self, region: Option<NormRect>) -> bool {
        match (self.region, region) {
            (Some(a), Some(b)) => iou(a, b) >= 0.5,
            (None, None) => true,
            _ => false,
        }
    }
}

/// Intersection over union of two rectangles.
pub fn iou(a: NormRect, b: NormRect) -> f32 {
    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    if w <= 0.0 || h <= 0.0 {
        return 0.0;
    }
    let inter = w * h;
    let union = a.w * a.h + b.w * b.h - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// Result of reading a sidecar.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub sidecar: Sidecar,
    /// Written by a newer build: readable, but must not be rewritten here.
    pub too_new: bool,
    /// The JSON as read (after migration). Rewrites merge onto it so fields
    /// this build does not know survive.
    pub raw: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SidecarError {
    #[error("{0}")]
    Json(String),
    #[error("schema v{0} has no migration path")]
    NoMigration(u32),
    #[error("invalid sidecar: {0}")]
    Invalid(&'static str),
}

/// A step that upgrades a sidecar JSON object from `from` to `from + 1`.
pub type Migration = fn(&mut Value) -> Result<(), String>;

/// Registered upgrades, keyed by the version they start from. Add an entry
/// here whenever [`SIDECAR_SCHEMA_VERSION`] is bumped.
pub const MIGRATIONS: &[(u32, Migration)] = &[];

impl Sidecar {
    pub fn to_json(&self) -> Vec<u8> {
        self.to_json_preserving(None)
    }

    /// Serialize, keeping any keys of `base` (the file as previously read)
    /// that this build does not know, at any depth.
    pub fn to_json_preserving(&self, base: Option<&Value>) -> Vec<u8> {
        // Serializing plain data with string keys cannot fail; non-finite
        // floats become `null`, which the reader tolerates.
        let mut v = serde_json::to_value(self).expect("sidecar serializes");
        if let (Some(raw), None) = (&self.geotag_unreadable, self.geotag) {
            v["geotag"] = raw.clone();
        }
        if let Some(base) = base {
            merge_unknown(&mut v, base);
        }
        serde_json::to_vec_pretty(&v).expect("value serializes")
    }

    /// Parse, migrating older schemas and tolerating unknown fields.
    pub fn from_json(bytes: &[u8]) -> Result<Loaded, SidecarError> {
        Self::from_json_with(bytes, SIDECAR_SCHEMA_VERSION, MIGRATIONS)
    }

    pub(crate) fn from_json_with(
        bytes: &[u8],
        current: u32,
        migrations: &[(u32, Migration)],
    ) -> Result<Loaded, SidecarError> {
        if bytes.len() as u64 > ds_core::limits::MAX_JSON_BYTES {
            return Err(SidecarError::Invalid("file too large"));
        }
        let mut value: Value =
            serde_json::from_slice(bytes).map_err(|e| SidecarError::Json(e.to_string()))?;
        let found = value
            .get("schema_version")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(SidecarError::Invalid("missing schema_version"))?;
        let too_new = found > current;
        if found < current {
            migrate(&mut value, found, current, migrations)?;
        }
        let raw = value.clone();
        // `null` (how serde_json writes NaN/inf) is read leniently by ds-core
        // and sanitized to the field's default.
        let sidecar: Sidecar = match serde_json::from_value(value.clone()) {
            Ok(s) => s,
            // A newer build may use enum variants we don't know (a new mask
            // shape, WB mode...). Keep the organizational fields indexable;
            // the file is read-only here, so nothing is lost.
            Err(_) if too_new && value.get("adjustments").is_some() => {
                value.as_object_mut().map(|o| o.remove("adjustments"));
                serde_json::from_value(value).map_err(|e| SidecarError::Json(e.to_string()))?
            }
            Err(e) => return Err(SidecarError::Json(e.to_string())),
        };
        if sidecar.variant_id == 0 || sidecar.image_id == 0 || sidecar.ordinal == 0 {
            return Err(SidecarError::Invalid("zero id or ordinal"));
        }
        let mut sidecar = sidecar;
        sidecar
            .keywords
            .retain(|k| k.len() <= ds_core::limits::MAX_KEYWORD_BYTES);
        sidecar.keywords.truncate(ds_core::limits::MAX_KEYWORDS);
        sidecar.geotag_unreadable = raw
            .get("geotag")
            .filter(|g| sidecar.geotag.is_none() && !g.is_null())
            .cloned();
        Ok(Loaded {
            sidecar,
            too_new,
            raw: Some(raw),
        })
    }
}

/// Copy into `new` every object member of `base` that `new` lacks, recursing
/// into objects present in both and into arrays of equal length. Every field
/// this build knows is always serialized, so only unknown ones are copied.
fn merge_unknown(new: &mut Value, base: &Value) {
    match (new, base) {
        (Value::Object(n), Value::Object(b)) => {
            // A tagged enum that switched variants: the old variant's fields
            // don't belong to the new one.
            let switched = ["mode", "kind"]
                .iter()
                .any(|t| n.get(*t).is_some_and(|nt| b.get(*t) != Some(nt)));
            if switched {
                return;
            }
            for (k, bv) in b {
                match n.get_mut(k) {
                    Some(nv) => merge_unknown(nv, bv),
                    None => {
                        n.insert(k.clone(), bv.clone());
                    }
                }
            }
        }
        (Value::Array(n), Value::Array(b)) if n.len() == b.len() => {
            for (nv, bv) in n.iter_mut().zip(b) {
                merge_unknown(nv, bv);
            }
        }
        _ => {}
    }
}

fn migrate(
    value: &mut Value,
    from: u32,
    to: u32,
    migrations: &[(u32, Migration)],
) -> Result<(), SidecarError> {
    for v in from..to {
        let step = migrations
            .iter()
            .find(|(start, _)| *start == v)
            .map(|(_, f)| f)
            .ok_or(SidecarError::NoMigration(from))?;
        step(value).map_err(SidecarError::Json)?;
        value["schema_version"] = Value::from(v + 1);
    }
    Ok(())
}

mod hex_hash {
    use ds_core::FileHash;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(h: &FileHash, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&h.to_hex())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<FileHash, D::Error> {
        let s = String::deserialize(d)?;
        FileHash::from_hex(&s).ok_or_else(|| D::Error::custom("hash must be 64 hex chars"))
    }
}

mod hex_hash_opt {
    use ds_core::FileHash;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    #[allow(clippy::ref_option)] // serde's `with` signature
    pub fn serialize<S: Serializer>(h: &Option<FileHash>, s: S) -> Result<S::Ok, S::Error> {
        match h {
            Some(h) => s.serialize_str(&h.to_hex()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<FileHash>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|s| {
                FileHash::from_hex(&s).ok_or_else(|| D::Error::custom("hash must be 64 hex chars"))
            })
            .transpose()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::float_cmp, clippy::unnecessary_wraps)]

    use super::*;

    pub(crate) fn sample() -> Sidecar {
        Sidecar {
            schema_version: SIDECAR_SCHEMA_VERSION,
            variant_id: 7,
            image_id: 3,
            ordinal: 1,
            name: "Variant 1".into(),
            hash: FileHash([0xab; 32]),
            meta: ImageMetaLite {
                capture_time_ms: Some(1_700_000_000_000),
                camera: Some("SONY ILCE-9".into()),
                lens: Some("FE 24-70mm F2.8 GM".into()),
                iso: Some(800),
                focal_length_mm: Some(35.0),
                width: 6000,
                height: 4000,
                orientation: Orientation::Rotate90,
                as_shot_wb: Some((5200.0, 3.0)),
            },
            adjustments: Adjustments::default(),
            rating: Rating::new(4).unwrap(),
            color_tag: ColorTag::Green,
            pick: PickState::Picked,
            cull_note: None,
            keywords: vec!["surf".into(), "日本".into()],
            content_hash: Some(FileHash([0xcd; 32])),
            replaced: vec![11, 12],
            optimize_report: None,
            ai_changes: None,
            people: Vec::new(),
            people_rejected: Vec::new(),
            iptc: IptcMeta::default(),
            geotag: GeoPoint::new(-33.8568, 151.2153, Some(4.5)),
            geotag_unreadable: None,
        }
    }

    #[test]
    fn round_trips() {
        let s = sample();
        let l = Sidecar::from_json(&s.to_json()).unwrap();
        assert_eq!(l.sidecar, s);
        assert!(!l.too_new);
        assert_eq!(s.meta.oriented_size(), Size::new(4000, 6000));
    }

    #[test]
    fn non_finite_values_round_trip_as_defaults() {
        let mut s = sample();
        s.adjustments.exposure.exposure_ev = f32::NAN;
        s.adjustments.levels.in_white = f32::INFINITY;
        s.adjustments.curve.rgb = vec![[0.0, f32::NAN], [1.0, 1.0]];
        let json = String::from_utf8(s.to_json()).unwrap();
        assert!(json.contains("null"), "serde_json writes NaN as null");
        let l = Sidecar::from_json(json.as_bytes()).unwrap();
        let a = &l.sidecar.adjustments;
        assert_eq!(a.exposure.exposure_ev, 0.0);
        assert_eq!(a.levels.in_white, 1.0, "struct default, not 0");
        assert!(a.curve.rgb.iter().flatten().all(|v| v.is_finite()));
        assert_eq!(l.sidecar.rating, s.rating);
    }

    #[test]
    fn rewrite_preserves_unknown_fields_at_any_depth() {
        let mut v: Value = serde_json::from_slice(&sample().to_json()).unwrap();
        v["future_top"] = serde_json::json!({"x": 1});
        v["adjustments"]["exposure"]["future_knob"] = serde_json::json!(0.7);
        v["meta"]["future_meta"] = serde_json::json!("m");
        v["adjustments"]["white_balance"] =
            serde_json::json!({"mode": "custom", "temp_k": 5000.0, "tint": 1.0, "extra": 2});
        let l = Sidecar::from_json(&serde_json::to_vec(&v).unwrap()).unwrap();
        let mut sc = l.sidecar.clone();
        sc.rating = Rating::new(1).unwrap();
        let out: Value = serde_json::from_slice(&sc.to_json_preserving(l.raw.as_ref())).unwrap();
        assert_eq!(out["future_top"], serde_json::json!({"x": 1}));
        assert_eq!(out["adjustments"]["exposure"]["future_knob"], 0.7);
        assert_eq!(out["meta"]["future_meta"], "m");
        assert_eq!(out["adjustments"]["white_balance"]["extra"], 2);
        assert_eq!(out["rating"], 1);
        // Switching an enum variant drops the old variant's fields.
        sc.adjustments.white_balance = ds_core::adjust::WhiteBalance::AsShot;
        let out: Value = serde_json::from_slice(&sc.to_json_preserving(l.raw.as_ref())).unwrap();
        assert_eq!(
            out["adjustments"]["white_balance"],
            serde_json::json!({"mode": "as_shot"})
        );
        let back = Sidecar::from_json(&serde_json::to_vec(&out).unwrap()).unwrap();
        assert_eq!(
            back.sidecar.adjustments.white_balance,
            ds_core::adjust::WhiteBalance::AsShot
        );
        // Arrays of different length are replaced, not merged.
        let mut a = serde_json::json!({"k": [{"x": 1, "u": 9}]});
        merge_unknown(&mut a, &serde_json::json!({"k": [{"x": 1}, {"x": 2}]}));
        assert_eq!(a, serde_json::json!({"k": [{"x": 1, "u": 9}]}));
    }

    #[test]
    fn unknown_fields_ignored_and_missing_optional_fields_default() {
        let json = format!(
            r#"{{"schema_version":1,"variant_id":1,"image_id":2,"ordinal":1,"hash":"{}",
               "future_top":{{"x":1}},"adjustments":{{"exposure":{{"exposure_ev":1.5,"new_knob":3}},"new_tool":{{}}}}}}"#,
            "00".repeat(32)
        );
        let l = Sidecar::from_json(json.as_bytes()).unwrap();
        assert_eq!(l.sidecar.adjustments.exposure.exposure_ev, 1.5);
        assert_eq!(l.sidecar.rating, Rating::default());
        assert_eq!(l.sidecar.keywords, [] as [std::string::String; 0]);
        assert_eq!(l.sidecar.meta, ImageMetaLite::default());
    }

    /// Sidecars from before profiles (no `profile` key) keep the look they were edited
    /// under; a rewrite records it explicitly; a new edit is written as Standard.
    #[test]
    fn sidecars_without_a_profile_read_as_neutral() {
        use ds_core::adjust::Profile;
        let json = format!(
            r#"{{"schema_version":1,"variant_id":1,"image_id":2,"ordinal":1,"hash":"{}",
               "adjustments":{{"exposure":{{"exposure_ev":0.5}}}}}}"#,
            "00".repeat(32)
        );
        let l = Sidecar::from_json(json.as_bytes()).unwrap();
        assert_eq!(l.sidecar.adjustments.profile, Profile::Neutral);
        let again = Sidecar::from_json(&l.sidecar.to_json_preserving(l.raw.as_ref())).unwrap();
        assert_eq!(again.sidecar.adjustments.profile, Profile::Neutral);
        let fresh = sample();
        assert_eq!(fresh.adjustments.profile, Profile::Standard);
        let v: Value = serde_json::from_slice(&fresh.to_json()).unwrap();
        assert_eq!(v["adjustments"]["profile"]["kind"], "standard");
    }

    #[test]
    fn newer_schema_is_readable_but_flagged() {
        let mut v: Value = serde_json::from_slice(&sample().to_json()).unwrap();
        v["schema_version"] = Value::from(SIDECAR_SCHEMA_VERSION + 5);
        v["brand_new"] = Value::from("x");
        let l = Sidecar::from_json(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(l.too_new);
        assert_eq!(l.sidecar.variant_id, 7);

        // Unknown enum variants from the future: organization still loads.
        v["adjustments"]["white_balance"] =
            serde_json::json!({"mode": "spectral", "curve": [1, 2]});
        let l = Sidecar::from_json(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(l.too_new);
        assert_eq!(l.sidecar.rating.stars(), 4);
        assert_eq!(l.sidecar.adjustments, Adjustments::default());
        // ...but the same content claiming the current schema is an error.
        v["schema_version"] = Value::from(SIDECAR_SCHEMA_VERSION);
        assert!(matches!(
            Sidecar::from_json(&serde_json::to_vec(&v).unwrap()),
            Err(SidecarError::Json(_))
        ));
    }

    #[test]
    fn rejects_garbage_and_invalid() {
        assert!(matches!(
            Sidecar::from_json(b"not json"),
            Err(SidecarError::Json(_))
        ));
        assert_eq!(
            Sidecar::from_json(b"{}"),
            Err(SidecarError::Invalid("missing schema_version"))
        );
        let bad_hash =
            r#"{"schema_version":1,"variant_id":1,"image_id":1,"ordinal":1,"hash":"zz"}"#;
        assert!(matches!(
            Sidecar::from_json(bad_hash.as_bytes()),
            Err(SidecarError::Json(_))
        ));
        let zero = format!(
            r#"{{"schema_version":1,"variant_id":0,"image_id":1,"ordinal":1,"hash":"{}"}}"#,
            "0".repeat(64)
        );
        assert_eq!(
            Sidecar::from_json(zero.as_bytes()),
            Err(SidecarError::Invalid("zero id or ordinal"))
        );
        let bad_rating = String::from_utf8(sample().to_json())
            .unwrap()
            .replace("\"rating\": 4", "\"rating\": 9");
        assert!(matches!(
            Sidecar::from_json(bad_rating.as_bytes()),
            Err(SidecarError::Json(_))
        ));
    }

    /// The geotag is optional, removable (written as `null`, so the old
    /// position isn't merged back from the file), and an out-of-range one
    /// reads as none.
    #[test]
    fn geotag_is_additive_and_removable() {
        let s = sample();
        let l = Sidecar::from_json(&s.to_json()).unwrap();
        assert_eq!(l.sidecar.geotag, s.geotag);
        let mut cleared = l.sidecar.clone();
        cleared.geotag = None;
        let out: Value =
            serde_json::from_slice(&cleared.to_json_preserving(l.raw.as_ref())).unwrap();
        assert_eq!(out["geotag"], Value::Null);
        assert_eq!(
            Sidecar::from_json(&serde_json::to_vec(&out).unwrap())
                .unwrap()
                .sidecar
                .geotag,
            None
        );
        // Hand-edited values (out of range, NaN written as null, a string) read
        // as none, the rest of the sidecar still loads, and a rewrite keeps them.
        for bad in [
            serde_json::json!({"latitude": 91.0, "longitude": 0.0}),
            serde_json::json!({"latitude": null, "longitude": 2.0}),
            serde_json::json!("22.2, -159.5"),
        ] {
            let mut v: Value = serde_json::from_slice(&s.to_json()).unwrap();
            v["geotag"] = bad.clone();
            let l = Sidecar::from_json(&serde_json::to_vec(&v).unwrap()).unwrap();
            assert_eq!(l.sidecar.geotag, None, "{bad}");
            assert_eq!(l.sidecar.geotag_unreadable.as_ref(), Some(&bad));
            assert_eq!(l.sidecar.rating, s.rating, "the rest still loads");
            let mut edited = l.sidecar.clone();
            edited.rating = Rating::new(1).unwrap();
            let out: Value =
                serde_json::from_slice(&edited.to_json_preserving(l.raw.as_ref())).unwrap();
            assert_eq!(out["geotag"], bad, "kept as written");
            assert_eq!(out["rating"], 1);
        }
        // An old build's rewrite (unknown key) keeps a valid geotag too.
        let mut old: Value = serde_json::from_slice(&s.to_json()).unwrap();
        old.as_object_mut().unwrap().remove("geotag");
        let mut merged = old.clone();
        merge_unknown(&mut merged, &serde_json::from_slice(&s.to_json()).unwrap());
        assert_eq!(
            Sidecar::from_json(&serde_json::to_vec(&merged).unwrap())
                .unwrap()
                .sidecar
                .geotag,
            s.geotag
        );
    }

    #[test]
    fn migrations_run_in_order() {
        fn v0_to_v1(v: &mut Value) -> Result<(), String> {
            let stars = v
                .as_object_mut()
                .unwrap()
                .remove("stars")
                .ok_or("no stars")?;
            v["rating"] = stars;
            Ok(())
        }
        fn v1_to_v2(v: &mut Value) -> Result<(), String> {
            v["name"] = Value::from("migrated");
            Ok(())
        }
        let migrations: &[(u32, Migration)] = &[(1, v1_to_v2), (0, v0_to_v1)];
        let json = format!(
            r#"{{"schema_version":0,"variant_id":1,"image_id":1,"ordinal":1,"hash":"{}","stars":3}}"#,
            "0".repeat(64)
        );
        let l = Sidecar::from_json_with(json.as_bytes(), 2, migrations).unwrap();
        assert_eq!(l.sidecar.schema_version, 2);
        assert_eq!(l.sidecar.rating.stars(), 3);
        assert_eq!(l.sidecar.name, "migrated");

        let no_path = Sidecar::from_json_with(json.as_bytes(), 3, migrations).unwrap_err();
        assert_eq!(no_path, SidecarError::NoMigration(0));
        let broken = json.replace(r#","stars":3"#, "");
        assert_eq!(
            Sidecar::from_json_with(broken.as_bytes(), 2, migrations),
            Err(SidecarError::Json("no stars".into()))
        );
        // With the real (empty) registry an old sidecar has no path.
        assert_eq!(
            Sidecar::from_json(json.as_bytes()),
            Err(SidecarError::NoMigration(0))
        );
    }
}
