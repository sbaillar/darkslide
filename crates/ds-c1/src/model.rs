//! What a Capture One session holds, independent of where it was read from.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::criteria::Criteria;

/// Settings keys (`.cos` names, e.g. `Exposure`) → raw string values.
pub type Keys = BTreeMap<String, String>;

/// One variant's settings: C1's defaults for the image (`DL`, the "default
/// layer": camera profile, as-shot white balance, EXIF orientation, …) and
/// the user's changes on top (`AL`, the "adjustment layer"). Only `AL`
/// keys are edits; a key's effective value is `AL` if present, else `DL`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    pub dl: Keys,
    pub al: Keys,
}

impl Settings {
    /// Effective value: the adjustment layer's, else the default layer's.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.al
            .get(key)
            .or_else(|| self.dl.get(key))
            .map(String::as_str)
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(parse_f64)
    }

    /// The user's value, only if the adjustment layer sets it.
    pub fn edit_f64(&self, key: &str) -> Option<f64> {
        self.al.get(key).and_then(|v| parse_f64(v))
    }

    pub fn default_f64(&self, key: &str) -> Option<f64> {
        self.dl.get(key).and_then(|v| parse_f64(v))
    }

    /// The adjustment layer sets `key` to something other than the default
    /// layer's value (C1 keeps reset tools there, e.g. a curve back at `0,0;1,1`).
    pub fn is_edit(&self, key: &str) -> bool {
        let Some(v) = self.al.get(key) else {
            return false;
        };
        let Some(d) = self.dl.get(key) else {
            return true;
        };
        let same = v == d
            || parse_list(v)
                .zip(parse_list(d))
                .is_some_and(|(a, b)| a == b)
            || numbers(v).zip(numbers(d)).is_some_and(|(a, b)| a == b);
        !same
    }
}

/// Every number in a value with any separators (`0,0;1,1`, `0|1|0|1`).
fn numbers(s: &str) -> Option<Vec<f64>> {
    s.split([',', ';', '|']).map(parse_f64).collect()
}

pub(crate) fn parse_f64(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// `a;b;c` numbers (white balance multipliers, crops, canvas sizes).
pub(crate) fn parse_list(s: &str) -> Option<Vec<f64>> {
    s.split(';').map(parse_f64).collect()
}

/// Where settings came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The per-image `.cos` file (preferred).
    Cos,
    /// The session database (the image has no readable `.cos`).
    Db,
}

#[derive(Debug, Clone, PartialEq)]
pub struct C1Variant {
    /// C1's variant UUID (stable across C1 sessions and database rebuilds).
    pub uuid: String,
    /// Order among the image's variants (C1 shows them in this order).
    pub position: usize,
    pub settings: Settings,
    pub source: Source,
}

/// The session folder an image lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum C1Folder {
    /// The session folder itself or any folder not listed below.
    Capture,
    Selects,
    Trash,
    Output,
}

impl C1Folder {
    /// The folder of a path relative to the session folder (first component).
    pub fn of(rel: &str) -> Self {
        let first = rel.split('/').next().unwrap_or_default();
        if rel.contains('/') && first.eq_ignore_ascii_case("Selects") {
            Self::Selects
        } else if rel.contains('/') && first.eq_ignore_ascii_case("Trash") {
            Self::Trash
        } else if rel.contains('/') && first.eq_ignore_ascii_case("Output") {
            Self::Output
        } else {
            Self::Capture
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct C1Image {
    /// Path relative to the session folder (`/`-separated), or `None` when
    /// the image lives outside it (C1 sessions can reference other folders).
    pub rel: Option<String>,
    /// The absolute path C1 recorded, for images outside the session folder.
    pub outside: Option<PathBuf>,
    pub file_name: String,
    /// C1's image UUID, when the database knows the image.
    pub uuid: Option<String>,
    pub variants: Vec<C1Variant>,
}

impl C1Image {
    pub fn folder(&self) -> Option<C1Folder> {
        self.rel.as_deref().map(C1Folder::of)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionKind {
    /// A manual album: the listed images/variants.
    Album,
    /// A smart album: a saved search.
    Smart,
}

/// A user collection worth importing (albums and smart albums; folders,
/// groups and projects only contribute their names to the path).
#[derive(Debug, Clone, PartialEq)]
pub struct C1Collection {
    /// Primary key in the session database (stable for that database).
    pub id: i64,
    pub kind: CollectionKind,
    pub name: String,
    /// Names of the enclosing groups/projects, outermost first.
    pub parents: Vec<String>,
    /// Members by image path (relative to the session folder) and, for
    /// variant-level membership, the variant UUID.
    pub members: Vec<(String, Option<String>)>,
    /// A smart album's search; `None` if it had no criteria text.
    pub criteria: Option<Criteria>,
}

impl C1Collection {
    /// "Group / Album" for nested albums.
    pub fn display_name(&self) -> String {
        let mut parts = self.parents.clone();
        parts.push(self.name.clone());
        parts.join(" / ")
    }
}

/// A Capture One session as read from disk.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct C1Session {
    pub folder: PathBuf,
    /// `ZDOCUMENTUUID` of the session database.
    pub uuid: Option<String>,
    /// The C1 build that last wrote the database (`ZVERSIONINFO.ZAUTHOR`).
    pub written_by: Option<String>,
    pub images: Vec<C1Image>,
    pub collections: Vec<C1Collection>,
    /// Names in the database's keyword library (used to split keyword lists).
    pub keyword_library: Vec<String>,
    /// Problems that did not stop the read (unreadable `.cos` files, …).
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_values_prefer_the_adjustment_layer() {
        let mut s = Settings::default();
        s.dl.insert("Exposure".into(), "0".into());
        s.dl.insert("Rotation".into(), "-90".into());
        s.al.insert("Exposure".into(), " 0.5 ".into());
        assert_eq!(s.get_f64("Exposure"), Some(0.5));
        assert_eq!(s.edit_f64("Exposure"), Some(0.5));
        assert_eq!(s.default_f64("Exposure"), Some(0.0));
        assert_eq!(s.get_f64("Rotation"), Some(-90.0));
        assert_eq!(s.edit_f64("Rotation"), None);
        assert_eq!(s.get("Missing"), None);
        s.al.insert("Contrast".into(), "nan".into());
        assert_eq!(
            s.edit_f64("Contrast"),
            None,
            "non-finite values are ignored"
        );
    }

    #[test]
    fn edits_differ_from_the_default_layer() {
        let mut s = Settings::default();
        for (k, d, a) in [
            ("GradationCurveRed", "0,0;1,1", "0,0;1.0,1"),
            ("WhiteBalance", "2.4;1;1.6", "2.4;1.0;1.6"),
            ("Exposure", "0", "0.5"),
            ("Vignetting", "0|1|0|1", "0.01|1|0|1"),
            ("Moire", "0;8", "0;8"),
        ] {
            s.dl.insert(k.into(), d.into());
            s.al.insert(k.into(), a.into());
        }
        s.al.insert("Clarity".into(), "10".into());
        s.al.insert("Name".into(), "x".into());
        s.dl.insert("Name".into(), "y".into());
        assert!(!s.is_edit("GradationCurveRed"));
        assert!(!s.is_edit("WhiteBalance"));
        assert!(!s.is_edit("Moire"));
        assert!(s.is_edit("Exposure"));
        assert!(s.is_edit("Vignetting"));
        assert!(s.is_edit("Clarity"), "no default to compare with");
        assert!(s.is_edit("Name"), "different text");
        assert!(!s.is_edit("Missing"));
    }

    #[test]
    fn lists_parse_or_fail_whole() {
        assert_eq!(parse_list("2.5;1;1.5"), Some(vec![2.5, 1.0, 1.5]));
        assert_eq!(parse_list("2.5;x"), None);
    }

    #[test]
    fn folders_by_first_component() {
        assert_eq!(C1Folder::of("a.ARW"), C1Folder::Capture);
        assert_eq!(C1Folder::of("Capture/a.ARW"), C1Folder::Capture);
        assert_eq!(C1Folder::of("Selects/a.ARW"), C1Folder::Selects);
        assert_eq!(C1Folder::of("selects/x/a.ARW"), C1Folder::Selects);
        assert_eq!(C1Folder::of("Trash/a.ARW"), C1Folder::Trash);
        assert_eq!(C1Folder::of("Output/a.jpg"), C1Folder::Output);
        // A file named like a folder at the top is just a file.
        assert_eq!(C1Folder::of("Trash"), C1Folder::Capture);
        let img = C1Image {
            rel: Some("Selects/b.ARW".into()),
            outside: None,
            file_name: "b.ARW".into(),
            uuid: None,
            variants: vec![],
        };
        assert_eq!(img.folder(), Some(C1Folder::Selects));
    }

    #[test]
    fn nested_album_names() {
        let c = C1Collection {
            id: 1,
            kind: CollectionKind::Album,
            name: "Best".into(),
            parents: vec!["Trip".into(), "Day 1".into()],
            members: vec![],
            criteria: None,
        };
        assert_eq!(c.display_name(), "Trip / Day 1 / Best");
    }
}
