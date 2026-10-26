//! Find-query seam: free text ("let's find pics of alex in iceland last
//! summer") → typed [`FindQuery`] (people, places, dates, rating/flag and the
//! descriptive rest for the image-text model).
//!
//! Same shape as the [`crate::intent`] edit seam: backends implement
//! [`FindBackend`], and [`RuleBasedFindParser`] is the deterministic offline
//! parser. Parsing only *classifies*; whether a place or person exists in the
//! library is decided by the caller against its index (a person term carries
//! the known people it matched, so "alex" with no match can be reported as
//! "no one named Alex yet").
//!
//! Dates follow the session convention: capture times are the camera's wall
//! clock treated as UTC, so ranges are civil days × 86 400 000 ms with no time
//! zone.

mod dates;
mod names;
mod rules;

use serde::{Deserialize, Serialize};

pub use lt_core::date::{CivilDate, DateFilter, days_from_civil};
pub use names::{PersonMatch, PersonMatchKind, fold, match_people};
pub use rules::RuleBasedFindParser;

/// A date phrase and what it means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateTerm {
    /// As typed ("last summer").
    pub phrase: String,
    pub filter: DateFilter,
}

/// A star-rating condition (inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RatingQuery {
    pub min: Option<u8>,
    pub max: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagQuery {
    Picked,
    Rejected,
    Unflagged,
}

/// A person named in the query and the known people it may mean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonTerm {
    /// As typed, lower-cased ("alex").
    pub term: String,
    /// Known people it matched, best first; empty = no one by that name yet.
    pub matches: Vec<PersonMatch>,
}

/// A parsed find query. Every part is optional; `text` is what is left for
/// the image-text model once the structured parts and filler are removed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FindQuery {
    pub people: Vec<PersonTerm>,
    /// Place terms, lower-cased ("iceland", "new york"). Not resolved here.
    pub places: Vec<String>,
    pub date: Option<DateTerm>,
    pub rating: Option<RatingQuery>,
    pub flag: Option<FlagQuery>,
    /// Descriptive remainder ("beach at sunset"), lower-cased; may be empty.
    pub text: String,
}

impl FindQuery {
    /// Nothing at all was asked for.
    pub fn is_empty(&self) -> bool {
        self.people.is_empty()
            && self.places.is_empty()
            && self.date.is_none()
            && self.rating.is_none()
            && self.flag.is_none()
            && self.text.is_empty()
    }
}

/// What the parser may know about the library. Nothing is required.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryContext {
    /// Names in the people store ("Alex", "Alexander Lee").
    pub known_people: Vec<String>,
    /// Place names the library knows as places (photo location fields,
    /// folder and session titles without their dates), matched as whole
    /// phrases wherever they appear in the query.
    pub known_places: Vec<String>,
    /// Today's date, for "last summer", "this year", "yesterday".
    /// Without it, relative dates are left in the text.
    pub today: Option<CivilDate>,
}

/// Turns query text into a [`FindQuery`]. Object-safe, so backends can be
/// chained or swapped (a language model in the app, these rules offline).
pub trait FindBackend: Send + Sync {
    /// Short identifier for logs ("rules").
    fn name(&self) -> &str;
    fn parse(&self, text: &str, ctx: &QueryContext) -> FindQuery;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query() {
        assert!(FindQuery::default().is_empty());
        let q = FindQuery {
            text: "beach".into(),
            ..FindQuery::default()
        };
        assert!(!q.is_empty());
    }

    #[test]
    fn serde_shapes() {
        let f = DateFilter::months(4);
        let s = serde_json::to_string(&f).unwrap();
        assert_eq!(s, r#"{"from_ms":null,"to_ms":null,"months":4}"#);
        let q: FindQuery = serde_json::from_str(r#"{"text":"x"}"#).unwrap();
        assert_eq!(q.text, "x");
        let c: QueryContext = serde_json::from_str("{}").unwrap();
        assert!(c.today.is_none());
    }
}
