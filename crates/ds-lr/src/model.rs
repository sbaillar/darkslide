//! What a Lightroom source holds, independent of where it was read from (a
//! catalog, a folder of XMP sidecars or a preset file).

use std::collections::BTreeMap;
use std::path::PathBuf;

use ds_core::{ColorTag, PickState, Rating};

/// One develop-settings value. Catalogs hold them as Lua values, XMP as
/// attribute text or `rdf:Seq` lists; both are read into this.
#[derive(Debug, Clone, PartialEq)]
pub enum Setting {
    Num(f64),
    Text(String),
    Bool(bool),
    List(Vec<Setting>),
    Table(BTreeMap<String, Setting>),
}

/// Develop settings by Camera Raw name (`Exposure2012`, `CropLeft`, `ToneCurvePV2012`, …).
pub type Develop = BTreeMap<String, Setting>;

impl Setting {
    /// A finite number: a number, or text such as `+0.50` / `-12` / ` 5500 `.
    pub fn num(&self) -> Option<f64> {
        match self {
            Self::Num(n) => Some(*n).filter(|n| n.is_finite()),
            Self::Text(s) => parse_num(s),
            _ => None,
        }
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }

    /// A boolean: `true`/`True`/`1`, a non-zero number, or a Lua boolean.
    pub fn flag(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            Self::Num(n) => Some(*n != 0.0),
            Self::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn list(&self) -> Option<&[Setting]> {
        match self {
            Self::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn table(&self) -> Option<&BTreeMap<String, Setting>> {
        match self {
            Self::Table(t) => Some(t),
            _ => None,
        }
    }
}

/// A finite number from text; a leading `+` is allowed (XMP writes `+0.50`).
pub(crate) fn parse_num(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s.strip_prefix('+').unwrap_or(s);
    s.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// One thing an import can bring over; the import dialog has a checkbox for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Rating,
    /// Colour label.
    Label,
    /// Pick / reject flag.
    Flag,
    Keywords,
    /// Title, caption and copyright.
    Metadata,
    Collections,
    /// Crop, straighten and rotation.
    Crop,
    /// Exposure, contrast, highlights, shadows, whites, blacks and the tone curve.
    Tone,
    WhiteBalance,
    /// Saturation, vibrance, HSL and colour grading.
    Color,
    /// Texture, clarity, dehaze, sharpening and noise reduction.
    Detail,
    /// Post-crop vignette and grain.
    Effects,
    /// Lens profile corrections and chromatic aberration.
    Lens,
    /// Linear, radial and brush adjustments and the AI masks Darkslide can redo.
    Masks,
}

impl Category {
    pub const ALL: [Category; 14] = [
        Self::Rating,
        Self::Label,
        Self::Flag,
        Self::Keywords,
        Self::Metadata,
        Self::Collections,
        Self::Crop,
        Self::Tone,
        Self::WhiteBalance,
        Self::Color,
        Self::Detail,
        Self::Effects,
        Self::Lens,
        Self::Masks,
    ];

    /// Stable name (wire form, state files).
    pub fn key(self) -> &'static str {
        match self {
            Self::Rating => "rating",
            Self::Label => "label",
            Self::Flag => "flag",
            Self::Keywords => "keywords",
            Self::Metadata => "metadata",
            Self::Collections => "collections",
            Self::Crop => "crop",
            Self::Tone => "tone",
            Self::WhiteBalance => "white_balance",
            Self::Color => "color",
            Self::Detail => "detail",
            Self::Effects => "effects",
            Self::Lens => "lens",
            Self::Masks => "masks",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.key() == key)
    }

    /// Edits to the picture (as opposed to organisation).
    pub fn is_develop(self) -> bool {
        matches!(
            self,
            Self::Crop
                | Self::Tone
                | Self::WhiteBalance
                | Self::Color
                | Self::Detail
                | Self::Effects
                | Self::Lens
                | Self::Masks
        )
    }
}

/// How faithfully a Lightroom setting maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Fidelity {
    /// Same meaning and scale.
    Exact,
    /// Same intent, different engine or an uncalibrated scale: close, not identical.
    Approximate,
}

/// A photo in a Lightroom source.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LrPhoto {
    /// Stable across reads and catalogs: the catalog's global id, else a
    /// name derived from the sidecar's path.
    pub key: String,
    /// The original file, as the source records it (it may not exist here).
    pub path: PathBuf,
    /// The master's key, when this is a virtual copy.
    pub master: Option<String>,
    pub copy_name: Option<String>,
    /// Position among the copies of one master (0 for a master).
    pub order: usize,
    /// Capture time, ms since the Unix epoch (camera clock as UTC), when known.
    pub capture_time_ms: Option<i64>,
    /// `None`: the source says nothing, and the field is left alone.
    pub rating: Option<Rating>,
    pub pick: Option<PickState>,
    pub color_tag: Option<ColorTag>,
    /// A colour label with no Darkslide colour ("Approved"), reported not imported.
    pub unmapped_label: Option<String>,
    /// EXIF orientation (1-8) Lightroom shows the photo with, user rotation included.
    pub orientation: Option<u8>,
    pub keywords: Option<Vec<String>>,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub copyright: Option<String>,
    pub develop: Option<Develop>,
}

/// A catalog collection (plain or smart) or collection set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LrCollection {
    /// Stable within the catalog (its local id).
    pub id: i64,
    pub name: String,
    /// Names of the enclosing collection sets, outermost first.
    pub parents: Vec<String>,
    /// Smart collection: a saved search, which Darkslide can't import.
    pub smart: bool,
    /// Members as photo keys, in the collection's order.
    pub members: Vec<String>,
}

impl LrCollection {
    /// "Set / Collection" for nested collections.
    pub fn display_name(&self) -> String {
        let mut parts = self.parents.clone();
        parts.push(self.name.clone());
        parts.join(" / ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Catalog,
    Sidecars,
}

/// Everything read from a catalog or a folder of sidecars.
#[derive(Debug, Clone, PartialEq)]
pub struct LrLibrary {
    pub kind: SourceKind,
    /// The `.lrcat` or the folder that was read.
    pub origin: PathBuf,
    /// Identifies the source across imports (catalog: its global id, if it has one).
    pub id: Option<String>,
    /// The Lightroom version that wrote the source, when it says.
    pub written_by: Option<String>,
    pub photos: Vec<LrPhoto>,
    pub collections: Vec<LrCollection>,
    /// Problems that did not stop the read.
    pub warnings: Vec<String>,
}

/// A Lightroom / Camera Raw preset.
#[derive(Debug, Clone, PartialEq)]
pub struct LrPreset {
    pub name: String,
    pub group: Option<String>,
    pub uuid: Option<String>,
    pub develop: Develop,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_numbers_and_flags_from_text_or_values() {
        assert_eq!(Setting::Text("+0.50".into()).num(), Some(0.5));
        assert_eq!(Setting::Text(" -12 ".into()).num(), Some(-12.0));
        assert_eq!(Setting::Num(3.0).num(), Some(3.0));
        assert_eq!(Setting::Num(f64::NAN).num(), None);
        assert_eq!(Setting::Text("inf".into()).num(), None);
        assert_eq!(Setting::Text("x".into()).num(), None);
        assert_eq!(Setting::Bool(true).num(), None);
        assert_eq!(Setting::Text("True".into()).flag(), Some(true));
        assert_eq!(Setting::Text("0".into()).flag(), Some(false));
        assert_eq!(Setting::Text("maybe".into()).flag(), None);
        assert_eq!(Setting::Num(2.0).flag(), Some(true));
        assert_eq!(Setting::Bool(false).flag(), Some(false));
        assert_eq!(Setting::List(vec![]).flag(), None);
        assert_eq!(Setting::Text("a".into()).text(), Some("a"));
        assert!(Setting::Num(1.0).text().is_none());
        assert!(Setting::List(vec![Setting::Num(1.0)]).list().is_some());
        assert!(Setting::Num(1.0).list().is_none());
        assert!(Setting::Table(BTreeMap::new()).table().is_some());
        assert!(Setting::Num(1.0).table().is_none());
    }

    #[test]
    fn categories_round_trip_their_keys() {
        for c in Category::ALL {
            assert_eq!(Category::from_key(c.key()), Some(c));
        }
        assert_eq!(Category::from_key("nope"), None);
        assert!(Category::Tone.is_develop());
        assert!(!Category::Rating.is_develop());
        assert_eq!(
            Category::ALL.iter().filter(|c| c.is_develop()).count(),
            8,
            "the develop groups"
        );
    }

    #[test]
    fn nested_collection_names() {
        let c = LrCollection {
            name: "Best".into(),
            parents: vec!["Trip".into(), "Day 1".into()],
            ..LrCollection::default()
        };
        assert_eq!(c.display_name(), "Trip / Day 1 / Best");
    }
}
