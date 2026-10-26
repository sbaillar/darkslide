//! Descriptive metadata of a photo: the common IPTC Core fields the user edits
//! (title, caption, credits, location, rights). Kept in the sidecar and written
//! into exports as XMP/IPTC (lt-export `xmp`). An empty string is "not set".

use serde::{Deserialize, Serialize};

/// The editable IPTC Core fields of a photo.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IptcMeta {
    pub title: String,
    /// Caption / description.
    pub caption: String,
    pub creator: String,
    pub copyright: String,
    pub credit: String,
    pub headline: String,
    pub city: String,
    /// State / province.
    pub state: String,
    pub country: String,
    pub sublocation: String,
    /// Special instructions.
    pub instructions: String,
    /// Rights usage terms (XMP only; IPTC-IIM has no such dataset).
    pub usage_terms: String,
}

/// One field of [`IptcMeta`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IptcField {
    Title,
    Caption,
    Creator,
    Copyright,
    Credit,
    Headline,
    City,
    State,
    Country,
    Sublocation,
    Instructions,
    UsageTerms,
}

impl IptcField {
    pub const ALL: [IptcField; 12] = [
        Self::Title,
        Self::Caption,
        Self::Creator,
        Self::Copyright,
        Self::Credit,
        Self::Headline,
        Self::City,
        Self::State,
        Self::Country,
        Self::Sublocation,
        Self::Instructions,
        Self::UsageTerms,
    ];
}

impl IptcMeta {
    pub fn get(&self, f: IptcField) -> &str {
        match f {
            IptcField::Title => &self.title,
            IptcField::Caption => &self.caption,
            IptcField::Creator => &self.creator,
            IptcField::Copyright => &self.copyright,
            IptcField::Credit => &self.credit,
            IptcField::Headline => &self.headline,
            IptcField::City => &self.city,
            IptcField::State => &self.state,
            IptcField::Country => &self.country,
            IptcField::Sublocation => &self.sublocation,
            IptcField::Instructions => &self.instructions,
            IptcField::UsageTerms => &self.usage_terms,
        }
    }

    pub fn get_mut(&mut self, f: IptcField) -> &mut String {
        match f {
            IptcField::Title => &mut self.title,
            IptcField::Caption => &mut self.caption,
            IptcField::Creator => &mut self.creator,
            IptcField::Copyright => &mut self.copyright,
            IptcField::Credit => &mut self.credit,
            IptcField::Headline => &mut self.headline,
            IptcField::City => &mut self.city,
            IptcField::State => &mut self.state,
            IptcField::Country => &mut self.country,
            IptcField::Sublocation => &mut self.sublocation,
            IptcField::Instructions => &mut self.instructions,
            IptcField::UsageTerms => &mut self.usage_terms,
        }
    }

    /// No field is set.
    pub fn is_empty(&self) -> bool {
        IptcField::ALL.iter().all(|&f| self.get(f).is_empty())
    }

    /// Every field trimmed (what is stored).
    #[must_use]
    pub fn trimmed(mut self) -> Self {
        for f in IptcField::ALL {
            let v = self.get_mut(f);
            let t = v.trim();
            if t.len() != v.len() {
                *v = t.to_owned();
            }
        }
        self
    }

    /// `self` with every field `over` sets (a preset, or values read from a
    /// file over the camera's): `over`'s empty fields leave `self`'s alone.
    #[must_use]
    pub fn overlaid(mut self, over: &IptcMeta) -> Self {
        for f in IptcField::ALL {
            let v = over.get(f).trim();
            if !v.is_empty() {
                v.clone_into(self.get_mut(f));
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_and_set_reach_every_field() {
        let mut m = IptcMeta::default();
        assert!(m.is_empty());
        for (i, f) in IptcField::ALL.iter().enumerate() {
            *m.get_mut(*f) = format!("v{i}");
        }
        for (i, f) in IptcField::ALL.iter().enumerate() {
            assert_eq!(m.get(*f), format!("v{i}"));
        }
        assert!(!m.is_empty());
        // Each field is distinct storage.
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 12);
        assert_eq!(json["usage_terms"], "v11");
    }

    #[test]
    fn overlay_sets_only_what_the_preset_sets() {
        let base = IptcMeta {
            title: "Beach".into(),
            creator: "Camera Owner".into(),
            ..IptcMeta::default()
        };
        let preset = IptcMeta {
            creator: " Alexa ".into(),
            copyright: "© 2026 Alexa".into(),
            title: "   ".into(),
            ..IptcMeta::default()
        };
        let m = base.overlaid(&preset);
        assert_eq!(m.title, "Beach", "a blank preset field leaves the photo's");
        assert_eq!(m.creator, "Alexa");
        assert_eq!(m.copyright, "© 2026 Alexa");
    }

    #[test]
    fn trimmed_and_lenient_json() {
        let m = IptcMeta {
            city: "  Akureyri ".into(),
            ..IptcMeta::default()
        }
        .trimmed();
        assert_eq!(m.city, "Akureyri");
        // Missing fields read as empty; unknown ones are ignored.
        let m: IptcMeta = serde_json::from_str(r#"{"title":"T","future":1}"#).unwrap();
        assert_eq!(m.title, "T");
        assert_eq!(m.caption.len(), 0);
    }
}
