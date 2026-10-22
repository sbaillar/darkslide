//! Data packs: looks (`look@1`), styles (`style@1`) and voice phrases (`phrases@1`).
//!
//! The host validates a pack and hands the app normalized JSON. Edit values use the
//! app's stable `EditCodec` keys, checked against `ds_core::edit_keys` (name, kind and
//! range): an item using a key this Darkslide doesn't know is left out with "needs a
//! newer Darkslide" rather than applied partially.
//!
//! Identity: a third-party look's stored id is `<plugin-id>/<id>` and its preset UUID
//! is derived by the host, so packs can't collide with each other or with Darkslide's
//! own; only bundled (`darkslide.*`) packs set ids and UUIDs verbatim (the migrated
//! built-ins, whose ids sidecars and thumbnails already use).

use std::collections::{BTreeMap, BTreeSet};

use ds_core::edit_keys::{self, KeyKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::Kind;
use crate::text;

/// Adjustment groups a style may name (the app's `AdjustGroup` raw values).
pub const GROUPS: &[&str] = &[
    "profile",
    "whiteBalance",
    "exposure",
    "presence",
    "hdr",
    "levels",
    "curve",
    "colorBalance",
    "colorEditor",
    "detail",
    "noise",
    "vignette",
    "effects",
    "lens",
    "crop",
];
/// Groups a style may add as deltas (`AdjustGroup.supportsRelative`).
const RELATIVE_GROUPS: &[&str] = &["whiteBalance", "exposure", "presence", "hdr"];
const CURVE_CHANNELS: &[&str] = &["RGB", "Luma", "Red", "Green", "Blue"];
pub const MAX_ITEMS: usize = 200;
pub const MAX_FILE_BYTES: usize = 1 << 20;
const MAX_CURVE_POINTS: usize = 32;

/// A one-touch look (`look@1`): scalars only, so its Intensity can scale every field
/// exactly and Intensity 0 restores the photo exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookItem {
    /// Stored in an edit's `look` record.
    pub id: String,
    /// Bundled packs only; derived for third parties.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub values: BTreeMap<String, f64>,
    /// Keys set exactly instead of added (black & white saturation).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exact: Vec<String>,
    /// Not allowed in looks; present only to say why.
    #[serde(default, skip_serializing)]
    curves: Option<Value>,
    #[serde(default, skip_serializing)]
    ranges: Option<Value>,
    #[serde(default, skip_serializing)]
    skin: Option<Value>,
}

/// A style in a section of the Styles panel (`style@1`): what a saved preset holds,
/// with its groups, relative groups and exact fields explicit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StyleItem {
    /// Bundled packs only; derived for third parties.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    pub name: String,
    pub section: String,
    pub groups: Vec<String>,
    #[serde(default)]
    pub relative: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absolute: Vec<String>,
    #[serde(default)]
    pub values: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub curves: BTreeMap<String, Vec<[f64; 2]>>,
    /// Colour-editor advanced ranges and skin tone, as the app encodes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranges: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skin: Option<Value>,
}

/// A voice phrase pack (`phrases@1`): phrase → voice command id, for one language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhrasePack {
    /// BCP 47 language the phrases are in (`en`, `en-GB`, `de`).
    pub language: String,
    pub phrases: BTreeMap<String, String>,
}

/// A pack after validation: the JSON the app reads, and the items left out (with why).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    pub json: String,
    pub skipped: Vec<String>,
}

/// Validate a data file of `kind` from `plugin_id` (`builtin`: a bundled pack).
/// File-level problems are errors; a bad item is left out and listed in `skipped`.
pub fn normalize(
    kind: Kind,
    plugin_id: &str,
    builtin: bool,
    bytes: &[u8],
) -> Result<Normalized, String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} KB; a pack file holds at most 1 MB",
            bytes.len() >> 10
        ));
    }
    let mut skipped = Vec::new();
    let json = match kind {
        Kind::Looks => {
            let items: Vec<Value> = parse(bytes)?;
            check_count(items.len())?;
            let items = each::<LookItem>(items, "look", &mut skipped);
            let mut ids = BTreeSet::new();
            let mut out = Vec::new();
            for l in items {
                let label = format!("look `{}`", l.id);
                match look(l, plugin_id, builtin) {
                    Err(e) => skipped.push(format!("{label}: {e}")),
                    Ok(l) if !ids.insert(l.id.clone()) => {
                        skipped.push(format!("{label}: the id is used twice"));
                    }
                    Ok(l) => out.push(l),
                }
            }
            serde_json::to_string(&out)
        }
        Kind::Styles => {
            let items: Vec<Value> = parse(bytes)?;
            check_count(items.len())?;
            let items = each::<StyleItem>(items, "style", &mut skipped);
            let mut names = BTreeSet::new();
            let mut out = Vec::new();
            for s in items {
                let label = format!("style `{}`", s.name);
                match style(s, plugin_id, builtin) {
                    Err(e) => skipped.push(format!("{label}: {e}")),
                    Ok(s) if !names.insert((s.section.clone(), s.name.clone())) => {
                        skipped.push(format!(
                            "{label}: two styles have this name in `{}`",
                            s.section
                        ));
                    }
                    Ok(s) => out.push(s),
                }
            }
            serde_json::to_string(&out)
        }
        Kind::VoicePhrases => {
            let mut pack: PhrasePack = parse(bytes)?;
            check_count(pack.phrases.len())?;
            let lang_ok = (2..=12).contains(&pack.language.len())
                && pack
                    .language
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-');
            if !lang_ok {
                return Err(format!(
                    "`language` \"{}\" is not a language code like en or en-GB",
                    pack.language
                ));
            }
            pack.phrases.retain(|p, c| {
                let ok = !p.trim().is_empty() && !c.trim().is_empty() && p.chars().count() <= 60;
                if !ok {
                    skipped.push(format!(
                        "phrase `{p}`: needs 1–60 characters and a command id"
                    ));
                }
                ok
            });
            serde_json::to_string(&pack)
        }
        Kind::ExportPostProcess | Kind::ExportDestination => return Err("not a data kind".into()),
    };
    Ok(Normalized {
        json: json.map_err(|e| e.to_string())?,
        skipped,
    })
}

fn look(mut l: LookItem, plugin_id: &str, builtin: bool) -> Result<LookItem, String> {
    if l.curves.is_some() || l.ranges.is_some() || l.skin.is_some() {
        return Err(
            "a look sets amounts only (no curves, colour ranges or skin tone); make it a style"
                .into(),
        );
    }
    l.name = text::clean(&l.name, text::NAME_MAX);
    l.summary = text::clean(&l.summary, text::SUMMARY_MAX);
    if l.name.is_empty() || !local_item_id(&l.id) {
        return Err("needs a `name` and an `id` of letters, digits, `.` and `-`".into());
    }
    for (k, v) in &l.values {
        check_value(k, *v)?;
        if !edit_keys::edit_key(k).is_some_and(edit_keys::EditKey::allowed_in_looks) {
            return Err(format!("a look can't set `{k}` (styles can)"));
        }
    }
    if let Some(k) = l.exact.iter().find(|k| !l.values.contains_key(*k)) {
        return Err(format!("`exact` names `{k}`, which has no value"));
    }
    if builtin {
        if !l.id.starts_with("look.") {
            return Err("a bundled look's id starts with `look.`".into());
        }
        l.uuid = Some(given_uuid(l.uuid.as_deref())?);
    } else {
        if l.uuid.is_some() {
            return Err("`uuid` is set by Darkslide for plug-in looks; leave it out".into());
        }
        l.uuid = Some(derived_uuid(plugin_id, &format!("look/{}", l.id)));
        l.id = format!("{plugin_id}/{}", l.id);
    }
    Ok(l)
}

fn style(mut s: StyleItem, plugin_id: &str, builtin: bool) -> Result<StyleItem, String> {
    s.name = text::clean(&s.name, text::NAME_MAX);
    s.section = text::clean(&s.section, text::NAME_MAX);
    if s.name.is_empty() || s.section.is_empty() {
        return Err("needs a `name` and a `section`".into());
    }
    if let Some(g) = s.groups.iter().find(|g| !GROUPS.contains(&g.as_str())) {
        return Err(format!("unknown group `{g}`"));
    }
    if s.groups.is_empty() {
        return Err("`groups` is empty".into());
    }
    if let Some(g) = s
        .relative
        .iter()
        .find(|g| !RELATIVE_GROUPS.contains(&g.as_str()) || !s.groups.contains(g))
    {
        return Err(format!(
            "`relative` group `{g}` must be one of its groups and one of {}",
            RELATIVE_GROUPS.join(", ")
        ));
    }
    for (k, v) in &s.values {
        check_value(k, *v)?;
    }
    if let Some(k) = s.absolute.iter().find(|k| edit_keys::edit_key(k).is_none()) {
        return Err(format!(
            "`absolute` names `{k}`, which this Darkslide doesn't know (needs a newer Darkslide?)"
        ));
    }
    for (ch, pts) in &s.curves {
        if !CURVE_CHANNELS.contains(&ch.as_str()) {
            return Err(format!("unknown curve channel `{ch}`"));
        }
        if pts.len() > MAX_CURVE_POINTS || pts.iter().flatten().any(|v| !(0.0..=1.0).contains(v)) {
            return Err(format!(
                "curve `{ch}`: at most {MAX_CURVE_POINTS} points, each in 0–1"
            ));
        }
    }
    s.uuid = Some(if builtin {
        given_uuid(s.uuid.as_deref())?
    } else if s.uuid.is_some() {
        return Err("`uuid` is set by Darkslide for plug-in styles; leave it out".into());
    } else {
        derived_uuid(plugin_id, &format!("style/{}/{}", s.section, s.name))
    });
    Ok(s)
}

fn check_value(k: &str, v: f64) -> Result<(), String> {
    match edit_keys::edit_key(k) {
        None => Err(format!(
            "uses `{k}`, which this Darkslide doesn't know (needs a newer Darkslide?)"
        )),
        Some(key) if key.kind == KeyKind::Flag && ![0.0, 1.0].contains(&v) => {
            Err(format!("`{k}` is 0 or 1"))
        }
        Some(_) => edit_keys::check_value(k, v).map_or(Ok(()), Err),
    }
}

/// Each item that reads as a `T`; one that doesn't (an unknown field: written for a
/// newer Darkslide, or a typo) is left out with why, like an unknown key.
fn each<T: for<'de> Deserialize<'de>>(
    items: Vec<Value>,
    what: &str,
    skipped: &mut Vec<String>,
) -> Vec<T> {
    items
        .into_iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let label = v
                .get("id")
                .or_else(|| v.get("name"))
                .and_then(Value::as_str)
                .map_or_else(|| format!("{what} {}", i + 1), |n| format!("{what} `{n}`"));
            serde_json::from_value(v)
                .map_err(|e| skipped.push(format!("{label}: {e} (needs a newer Darkslide?)")))
                .ok()
        })
        .collect()
}

fn parse<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes)
        .map_err(|e| format!("line {}, column {}: {e}", e.line(), e.column()))
}

fn check_count(n: usize) -> Result<(), String> {
    if n > MAX_ITEMS {
        Err(format!("{n} items; a pack holds at most {MAX_ITEMS}"))
    } else {
        Ok(())
    }
}

fn local_item_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
}

fn given_uuid(given: Option<&str>) -> Result<String, String> {
    let g = given.ok_or("a bundled item needs its `uuid`")?;
    let shape = g.len() == 36 && [8, 13, 18, 23].iter().all(|&i| g.as_bytes()[i] == b'-');
    if !shape
        || !g
            .chars()
            .filter(|c| *c != '-')
            .all(|c| c.is_ascii_hexdigit())
    {
        return Err(format!("`{g}` is not a UUID"));
    }
    Ok(g.to_ascii_uppercase())
}

/// A name-based UUID (version 8, BLAKE3) from the plug-in id and the item's name.
fn derived_uuid(plugin_id: &str, name: &str) -> String {
    let h = blake3::hash(format!("darkslide-plugin-item\n{plugin_id}\n{name}").as_bytes());
    let mut b: [u8; 16] = h.as_bytes()[..16].try_into().unwrap_or_default();
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    let x = crate::trust::hex(&b).to_ascii_uppercase();
    format!(
        "{}-{}-{}-{}-{}",
        &x[..8],
        &x[8..12],
        &x[12..16],
        &x[16..20],
        &x[20..]
    )
}

#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;
