//! Smart filters → SQL over the session index.

use ds_core::{ColorTag, PickState, Rating};
use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};

use crate::relpath::Folder;

/// A browser filter / smart-album definition. Every field is optional; the
/// empty filter matches every variant outside the Trash.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Filter {
    pub min_rating: Option<Rating>,
    /// Inclusive upper bound on the rating. `Some(0 stars)` = unrated only;
    /// equal to `min_rating` = exactly that rating (smart folders "3 stars").
    pub max_rating: Option<Rating>,
    /// Match any of these color tags (empty = any).
    pub tags: Vec<ColorTag>,
    pub pick: Option<PickState>,
    /// Whitespace-separated terms; each must appear (case-insensitive) in the
    /// file name, variant name, keywords, camera, or lens.
    pub text: Option<String>,
    /// Case-insensitive substring of the camera model.
    pub camera: Option<String>,
    pub lens: Option<String>,
    /// Inclusive capture-time range, ms since the Unix epoch.
    pub date_from_ms: Option<i64>,
    pub date_to_ms: Option<i64>,
    pub iso_min: Option<u32>,
    pub iso_max: Option<u32>,
    /// `Some(true)` = only variants with non-identity adjustments.
    pub edited: Option<bool>,
    /// Restrict to one folder; `None` = Capture and Selects (not Trash).
    pub folder: Option<Folder>,
    /// Keywords that must all be present (whole keyword, case-insensitive).
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortKey {
    #[default]
    CaptureTime,
    Name,
    Rating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

impl Sort {
    pub fn by(key: SortKey) -> Self {
        Self {
            key,
            descending: false,
        }
    }
}

pub(crate) fn tag_code(t: ColorTag) -> i64 {
    match t {
        ColorTag::None => 0,
        ColorTag::Red => 1,
        ColorTag::Orange => 2,
        ColorTag::Yellow => 3,
        ColorTag::Green => 4,
        ColorTag::Blue => 5,
        ColorTag::Pink => 6,
        ColorTag::Purple => 7,
    }
}

pub(crate) fn tag_from_code(c: i64) -> ColorTag {
    match c {
        1 => ColorTag::Red,
        2 => ColorTag::Orange,
        3 => ColorTag::Yellow,
        4 => ColorTag::Green,
        5 => ColorTag::Blue,
        6 => ColorTag::Pink,
        7 => ColorTag::Purple,
        _ => ColorTag::None,
    }
}

pub(crate) fn pick_code(p: PickState) -> i64 {
    match p {
        PickState::Unflagged => 0,
        PickState::Picked => 1,
        PickState::Rejected => 2,
    }
}

pub(crate) fn pick_from_code(c: i64) -> PickState {
    match c {
        1 => PickState::Picked,
        2 => PickState::Rejected,
        _ => PickState::Unflagged,
    }
}

/// Escape `%`, `_` and the escape char for a `LIKE ... ESCAPE '\'` pattern.
fn like_contains(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

impl Filter {
    /// Build `SELECT v.id ...` with bound parameters. `album` restricts to a
    /// manual album's members.
    pub(crate) fn to_sql(&self, sort: Sort, album: Option<i64>) -> (String, Vec<SqlValue>) {
        self.to_sql_selecting("v.id", sort, album)
    }

    /// [`Filter::to_sql`] selecting `columns` (of `variants v` / `images i`).
    pub(crate) fn to_sql_selecting(
        &self,
        columns: &str,
        sort: Sort,
        album: Option<i64>,
    ) -> (String, Vec<SqlValue>) {
        let mut sql =
            format!("SELECT {columns} FROM variants v JOIN images i ON i.id = v.image_id WHERE ");
        let mut params: Vec<SqlValue> = Vec::new();
        let mut conds: Vec<String> = Vec::new();
        match self.folder {
            Some(f) => {
                conds.push("i.folder = ?".into());
                params.push(SqlValue::Integer(f.code()));
            }
            None => conds.push(format!("i.folder <> {}", Folder::Trash.code())),
        }
        if let Some(r) = self.min_rating.filter(|r| r.stars() > 0) {
            conds.push("v.rating >= ?".into());
            params.push(SqlValue::Integer(i64::from(r.stars())));
        }
        if let Some(r) = self.max_rating {
            conds.push("v.rating <= ?".into());
            params.push(SqlValue::Integer(i64::from(r.stars())));
        }
        if !self.tags.is_empty() {
            let marks = vec!["?"; self.tags.len()].join(",");
            conds.push(format!("v.color_tag IN ({marks})"));
            params.extend(self.tags.iter().map(|t| SqlValue::Integer(tag_code(*t))));
        }
        if let Some(p) = self.pick {
            conds.push("v.pick = ?".into());
            params.push(SqlValue::Integer(pick_code(p)));
        }
        for term in self.text.iter().flat_map(|t| t.split_whitespace()) {
            conds.push("(i.file_name_lc || char(10) || v.search) LIKE ? ESCAPE '\\'".into());
            params.push(SqlValue::Text(like_contains(&term.to_lowercase())));
        }
        for (col, val) in [("i.camera_lc", &self.camera), ("i.lens_lc", &self.lens)] {
            if let Some(v) = val.as_deref().filter(|v| !v.is_empty()) {
                conds.push(format!("{col} LIKE ? ESCAPE '\\'"));
                params.push(SqlValue::Text(like_contains(&v.to_lowercase())));
            }
        }
        for (cond, val) in [
            ("i.capture_ms >= ?", self.date_from_ms),
            ("i.capture_ms <= ?", self.date_to_ms),
            ("i.iso >= ?", self.iso_min.map(i64::from)),
            ("i.iso <= ?", self.iso_max.map(i64::from)),
        ] {
            if let Some(v) = val {
                conds.push(cond.into());
                params.push(SqlValue::Integer(v));
            }
        }
        for kw in self
            .keywords
            .iter()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
        {
            // `keywords` holds a JSON array; match the JSON-quoted keyword so
            // "sea" doesn't match "seaside".
            let quoted = serde_json::to_string(&kw.to_lowercase()).unwrap_or_default();
            conds.push(format!(
                "{}(v.keywords) LIKE ? ESCAPE '\\'",
                crate::db::LOWER_FN
            ));
            params.push(SqlValue::Text(like_contains(&quoted)));
        }
        if let Some(e) = self.edited {
            conds.push("v.edited = ?".into());
            params.push(SqlValue::Integer(i64::from(e)));
        }
        if let Some(a) = album {
            conds.push("v.id IN (SELECT variant_id FROM album_members WHERE album_id = ?)".into());
            params.push(SqlValue::Integer(a));
        }
        sql.push_str(&conds.join(" AND "));
        let dir = if sort.descending { "DESC" } else { "ASC" };
        let order = match sort.key {
            // NULL capture times sort last ascending; ties broken by name, then variant order.
            SortKey::CaptureTime => {
                format!(
                    " ORDER BY i.capture_ms IS NULL, i.capture_ms {dir}, i.file_name_lc {dir}, v.ordinal"
                )
            }
            SortKey::Name => format!(" ORDER BY i.file_name_lc {dir}, v.ordinal {dir}"),
            SortKey::Rating => format!(
                " ORDER BY v.rating {dir}, i.capture_ms IS NULL, i.capture_ms, i.file_name_lc, v.ordinal"
            ),
        };
        sql.push_str(&order);
        (sql, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for t in [
            ColorTag::None,
            ColorTag::Red,
            ColorTag::Orange,
            ColorTag::Yellow,
            ColorTag::Green,
            ColorTag::Blue,
            ColorTag::Pink,
            ColorTag::Purple,
        ] {
            assert_eq!(tag_from_code(tag_code(t)), t);
        }
        for p in [PickState::Unflagged, PickState::Picked, PickState::Rejected] {
            assert_eq!(pick_from_code(pick_code(p)), p);
        }
    }

    #[test]
    fn like_escapes_wildcards() {
        assert_eq!(like_contains("50%_off\\"), "%50\\%\\_off\\\\%");
    }

    #[test]
    fn empty_filter_excludes_trash_only() {
        let (sql, params) = Filter::default().to_sql(Sort::default(), None);
        assert!(sql.contains("i.folder <> 2"));
        assert_eq!(params, [] as [rusqlite::types::Value; 0]);
    }

    #[test]
    fn every_clause_binds_a_parameter() {
        let f = Filter {
            min_rating: Rating::new(3),
            tags: vec![ColorTag::Red, ColorTag::Blue],
            pick: Some(PickState::Picked),
            text: Some("Beach  Sunset".into()),
            camera: Some("A9".into()),
            lens: Some("GM".into()),
            date_from_ms: Some(1),
            date_to_ms: Some(2),
            iso_min: Some(100),
            iso_max: Some(6400),
            edited: Some(true),
            folder: Some(Folder::Selects),
            max_rating: Rating::new(4),
            keywords: vec!["Sea".into(), "  ".into()],
        };
        for key in [SortKey::CaptureTime, SortKey::Name, SortKey::Rating] {
            let (sql, params) = f.to_sql(
                Sort {
                    key,
                    descending: true,
                },
                Some(9),
            );
            assert_eq!(sql.matches('?').count(), params.len());
            assert!(sql.contains("DESC"));
        }
        let (_, params) = f.to_sql(Sort::default(), None);
        assert_eq!(params.len(), 16);
        assert!(params.contains(&SqlValue::Text("%\"sea\"%".into())));
        assert!(params.contains(&SqlValue::Text("%beach%".into())));
    }

    #[test]
    fn serde_defaults() {
        let f: Filter = serde_json::from_str(r#"{"min_rating":2,"unknown":1}"#).unwrap();
        assert_eq!(f.min_rating, Rating::new(2));
        assert_eq!(f.folder, None);
        let s: Sort = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Sort::by(SortKey::CaptureTime));
    }
}
