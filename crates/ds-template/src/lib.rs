//! Filename and subfolder templates shared by import (rename/subfolder) and
//! export (output filename).
//!
//! Syntax: literal text with `{token}` placeholders. `{{` and `}}` are literal
//! braces.
//!
//! | Token | Meaning |
//! |---|---|
//! | `{name}` | original file stem (`DSC01234`) |
//! | `{seq}` / `{seq:4}` | running number, optionally zero-padded to a width |
//! | `{date}` / `{date:%Y-%m-%d}` | capture date (default `%Y%m%d`, any strftime format) |
//! | `{time}` | capture time as `HHMMSS` |
//! | `{camera}` | camera model (`ILCE-9`) |
//! | `{ext}` | extension without the dot (`ARW`) |
//!
//! Rendered values never contain path separators, and every result is NFC
//! normalized so names compare consistently on APFS (which preserves but does
//! not normalize) and on NFS/SMB targets.

use std::fmt::Write as _;
use std::path::PathBuf;

use chrono::NaiveDateTime;
use chrono::format::{Item, StrftimeItems};
use unicode_normalization::UnicodeNormalization;

/// Placeholder rendered for a missing date/time/camera.
pub const UNKNOWN: &str = "unknown";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Lit(String),
    Name,
    Seq(usize),
    Date(String),
    Time,
    Camera,
    Ext,
}

/// A parsed template. Parse once, render many times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    source: String,
    parts: Vec<Part>,
}

/// Values available to a template.
#[derive(Debug, Clone, Copy)]
pub struct Vars<'a> {
    /// Original file stem, without extension.
    pub name: &'a str,
    pub seq: u64,
    /// Capture date/time (camera local time).
    pub datetime: Option<NaiveDateTime>,
    pub camera: Option<&'a str>,
    /// Extension without the dot.
    pub ext: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("unclosed `{{` at byte {0}")]
    Unclosed(usize),
    #[error("unmatched `}}` at byte {0}")]
    Unmatched(usize),
    #[error("unknown token `{{{0}}}`")]
    UnknownToken(String),
    #[error("bad argument for `{{{token}}}`: {arg}")]
    BadArgument { token: String, arg: String },
    #[error("template renders an unsafe path component `{0}`")]
    UnsafeComponent(String),
}

impl Template {
    pub fn parse(src: &str) -> Result<Self, TemplateError> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let mut chars = src.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            match c {
                '{' if chars.peek().map(|p| p.1) == Some('{') => {
                    chars.next();
                    lit.push('{');
                }
                '}' if chars.peek().map(|p| p.1) == Some('}') => {
                    chars.next();
                    lit.push('}');
                }
                '}' => return Err(TemplateError::Unmatched(i)),
                '{' => {
                    let rest = &src[i + 1..];
                    let end = rest.find('}').ok_or(TemplateError::Unclosed(i))?;
                    let body = &rest[..end];
                    if !lit.is_empty() {
                        parts.push(Part::Lit(std::mem::take(&mut lit)));
                    }
                    parts.push(parse_token(body)?);
                    // Skip the token body and closing brace.
                    while chars.peek().is_some_and(|&(j, _)| j <= i + 1 + end) {
                        chars.next();
                    }
                }
                _ => lit.push(c),
            }
        }
        if !lit.is_empty() {
            parts.push(Part::Lit(lit));
        }
        Ok(Self {
            source: src.to_owned(),
            parts,
        })
    }

    /// The template text as given to [`Template::parse`].
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// True when the template contains `{ext}`.
    pub fn uses_ext(&self) -> bool {
        self.parts.contains(&Part::Ext)
    }

    /// True when rendering needs capture metadata (`{date}`, `{time}`, `{camera}`),
    /// so callers can skip reading EXIF otherwise.
    pub fn uses_metadata(&self) -> bool {
        self.parts
            .iter()
            .any(|p| matches!(p, Part::Date(_) | Part::Time | Part::Camera))
    }

    /// Render with every substituted value sanitized (no `/`, `:`, control chars).
    /// Literal text is kept verbatim. The result is NFC normalized.
    pub fn render(&self, vars: &Vars<'_>) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Lit(s) => out.push_str(s),
                Part::Name => out.push_str(&sanitize_component(vars.name)),
                Part::Seq(w) => {
                    let _ = write!(out, "{:0w$}", vars.seq, w = *w);
                }
                Part::Date(fmt) => match vars.datetime.and_then(|dt| format_date(dt, fmt)) {
                    Some(s) => out.push_str(&sanitize_component(&s)),
                    None => out.push_str(UNKNOWN),
                },
                Part::Time => match vars.datetime {
                    Some(dt) => {
                        let _ = write!(out, "{}", dt.format("%H%M%S"));
                    }
                    None => out.push_str(UNKNOWN),
                },
                Part::Camera => {
                    out.push_str(&sanitize_component(vars.camera.unwrap_or(UNKNOWN)));
                }
                Part::Ext => out.push_str(&sanitize_component(vars.ext)),
            }
        }
        out.nfc().collect()
    }

    /// Render a single file name. If the template has no `{ext}`, `.ext` is
    /// appended, so `{name}_{seq:4}` gives `DSC01234_0001.ARW`. The whole name is
    /// sanitized; an empty result falls back to the original name.
    pub fn render_file_name(&self, vars: &Vars<'_>) -> String {
        let mut base = sanitize_component(&self.render(vars));
        if base.is_empty() {
            base = sanitize_component(vars.name);
        }
        if self.uses_ext() || vars.ext.is_empty() {
            base
        } else {
            format!("{base}.{}", sanitize_component(vars.ext))
        }
    }

    /// Render a relative subfolder path. Literal `/` separates components.
    /// Empty components are dropped; `.`/`..` are rejected so a template can
    /// never escape the destination.
    pub fn render_subfolder(&self, vars: &Vars<'_>) -> Result<PathBuf, TemplateError> {
        let rendered = self.render(vars);
        let mut path = PathBuf::new();
        for comp in rendered.split('/') {
            let comp = comp.trim();
            if comp.is_empty() {
                continue;
            }
            if comp == "." || comp == ".." {
                return Err(TemplateError::UnsafeComponent(comp.to_owned()));
            }
            path.push(sanitize_component(comp));
        }
        Ok(path)
    }
}

/// Format without panicking (`Display` for a failing format item panics in
/// `to_string`). `None` if the format needs data a `NaiveDateTime` lacks.
fn format_date(dt: NaiveDateTime, fmt: &str) -> Option<String> {
    let mut s = String::new();
    write!(s, "{}", dt.format(fmt)).ok()?;
    Some(s)
}

fn parse_token(body: &str) -> Result<Part, TemplateError> {
    let (name, arg) = match body.split_once(':') {
        Some((n, a)) => (n.trim(), Some(a)),
        None => (body.trim(), None),
    };
    let bad = |arg: &str| TemplateError::BadArgument {
        token: name.to_owned(),
        arg: arg.to_owned(),
    };
    match (name, arg) {
        ("name", None) => Ok(Part::Name),
        ("time", None) => Ok(Part::Time),
        ("camera", None) => Ok(Part::Camera),
        ("ext", None) => Ok(Part::Ext),
        ("seq", None) => Ok(Part::Seq(0)),
        ("seq", Some(a)) => match a.trim().parse::<usize>() {
            Ok(w) if (1..=12).contains(&w) => Ok(Part::Seq(w)),
            _ => Err(bad(a)),
        },
        ("date", None) => Ok(Part::Date("%Y%m%d".to_owned())),
        ("date", Some(a)) => {
            // Strict: reject anything that does not format a plain (zone-less)
            // date/time, e.g. `%Z`/`%z`, which would fail at render time.
            if a.is_empty()
                || StrftimeItems::new(a).any(|it| matches!(it, Item::Error))
                || format_date(NaiveDateTime::default(), a).is_none()
            {
                return Err(bad(a));
            }
            Ok(Part::Date(a.to_owned()))
        }
        ("name" | "time" | "camera" | "ext", Some(a)) => Err(bad(a)),
        _ => Err(TemplateError::UnknownToken(body.to_owned())),
    }
}

/// Make a string safe as a single path component: path separators, `:`, and
/// control characters become `_`, a leading `.` becomes `_` (so results are
/// never hidden files), surrounding whitespace is trimmed, and the result is NFC.
pub fn sanitize_component(s: &str) -> String {
    let mut out: String = s
        .trim()
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '\0') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .nfc()
        .collect();
    if out.starts_with('.') {
        out.replace_range(0..1, "_");
    }
    out
}

/// Key for detecting name collisions the way APFS (default: case-insensitive,
/// normalization-insensitive) and most NAS shares do: NFC + lowercase.
pub fn collision_key(name: &str) -> String {
    name.nfc().flat_map(char::to_lowercase).collect()
}

/// Insert `-n` before the extension: `DSC0001.ARW` → `DSC0001-1.ARW`.
/// Names without an extension (or dot-files) get the suffix at the end.
pub fn with_suffix(file_name: &str, n: u32) -> String {
    match file_name.rfind('.') {
        Some(dot) if dot > 0 => format!("{}-{n}{}", &file_name[..dot], &file_name[dot..]),
        _ => format!("{file_name}-{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 8, 6)
            .unwrap()
            .and_hms_opt(14, 3, 9)
            .unwrap()
    }

    fn vars() -> Vars<'static> {
        Vars {
            name: "DSC01234",
            seq: 7,
            datetime: Some(dt()),
            camera: Some("ILCE-9"),
            ext: "ARW",
        }
    }

    #[test]
    fn renders_every_token() {
        let t = Template::parse("{camera}_{date}_{time}_{seq:4}_{name}.{ext}").unwrap();
        assert_eq!(
            t.render(&vars()),
            "ILCE-9_20260806_140309_0007_DSC01234.ARW"
        );
        assert!(t.uses_ext());
        assert!(t.uses_metadata());
        assert!(
            !Template::parse("{name}_{seq}.{ext}")
                .unwrap()
                .uses_metadata()
        );
        assert!(Template::parse("{camera}").unwrap().uses_metadata());
        assert_eq!(t.as_str(), "{camera}_{date}_{time}_{seq:4}_{name}.{ext}");
    }

    #[test]
    fn custom_date_format_and_unpadded_seq() {
        let t = Template::parse("{date:%Y-%m-%d} {seq}").unwrap();
        assert_eq!(t.render(&vars()), "2026-08-06 7");
    }

    #[test]
    fn escaped_braces_and_unicode_literals() {
        let t = Template::parse("{{x}} é_{name}").unwrap();
        assert_eq!(t.render(&vars()), "{x} é_DSC01234");
    }

    #[test]
    fn zone_formats_are_rejected_at_parse() {
        // Zone specifiers parse as strftime but cannot render a NaiveDateTime:
        // they must be rejected up front, never panic later.
        for zoned in [
            "{date:%Z}",
            "{date:%z}",
            "{date:%Y-%:z}",
            "{date:%s}x{date:%Z}",
        ] {
            assert!(
                matches!(
                    Template::parse(zoned),
                    Err(TemplateError::BadArgument { .. })
                ),
                "{zoned}"
            );
        }
    }

    #[test]
    fn render_never_panics_on_unrenderable_format() {
        // Bypass parse validation to prove render itself is total.
        let t = Template {
            source: String::new(),
            parts: vec![Part::Date("%Z".into())],
        };
        assert_eq!(t.render(&vars()), UNKNOWN);
    }

    #[test]
    fn missing_values_render_unknown() {
        let t = Template::parse("{date}-{time}-{camera}").unwrap();
        let v = Vars {
            datetime: None,
            camera: None,
            ..vars()
        };
        assert_eq!(t.render(&v), "unknown-unknown-unknown");
    }

    #[test]
    fn parse_errors() {
        assert_eq!(Template::parse("a{name"), Err(TemplateError::Unclosed(1)));
        assert_eq!(Template::parse("a}b"), Err(TemplateError::Unmatched(1)));
        assert_eq!(
            Template::parse("{nope}"),
            Err(TemplateError::UnknownToken("nope".into()))
        );
        assert!(matches!(
            Template::parse("{seq:x}"),
            Err(TemplateError::BadArgument { .. })
        ));
        assert!(matches!(
            Template::parse("{seq:0}"),
            Err(TemplateError::BadArgument { .. })
        ));
        assert!(matches!(
            Template::parse("{date:%Q}"),
            Err(TemplateError::BadArgument { .. })
        ));
        assert!(matches!(
            Template::parse("{date:}"),
            Err(TemplateError::BadArgument { .. })
        ));
        assert!(matches!(
            Template::parse("{name:3}"),
            Err(TemplateError::BadArgument { .. })
        ));
        // Errors display usefully.
        assert_eq!(
            TemplateError::UnknownToken("x".into()).to_string(),
            "unknown token `{x}`"
        );
    }

    #[test]
    fn file_name_appends_ext_unless_template_has_it() {
        let t = Template::parse("{name}_{seq:3}").unwrap();
        assert_eq!(t.render_file_name(&vars()), "DSC01234_007.ARW");
        let t = Template::parse("{name}.{ext}").unwrap();
        assert_eq!(t.render_file_name(&vars()), "DSC01234.ARW");
        let v = Vars { ext: "", ..vars() };
        assert_eq!(
            Template::parse("{name}").unwrap().render_file_name(&v),
            "DSC01234"
        );
    }

    #[test]
    fn file_name_never_contains_separators_or_hides() {
        let t = Template::parse("a/b:{camera}").unwrap();
        let v = Vars {
            camera: Some("../evil\u{7}"),
            ..vars()
        };
        let name = t.render_file_name(&v);
        assert!(!name.contains('/') && !name.contains(':'), "{name}");
        assert_eq!(name, "a_b__._evil_.ARW");
        let t = Template::parse(".{name}").unwrap();
        assert_eq!(t.render_file_name(&vars()), "_DSC01234.ARW");
        // Empty render falls back to the original name.
        let t = Template::parse("   ").unwrap();
        assert_eq!(t.render_file_name(&vars()), "DSC01234.ARW");
    }

    #[test]
    fn subfolder_splits_and_rejects_traversal() {
        let t = Template::parse("{date:%Y}/{date:%m-%d}//{camera}").unwrap();
        assert_eq!(
            t.render_subfolder(&vars()).unwrap(),
            PathBuf::from("2026/08-06/ILCE-9")
        );
        let t = Template::parse("a/../b").unwrap();
        assert_eq!(
            t.render_subfolder(&vars()),
            Err(TemplateError::UnsafeComponent("..".into()))
        );
        // A value can never inject a separator, so `{camera}` = ".." is inert.
        let t = Template::parse("{camera}").unwrap();
        let v = Vars {
            camera: Some(".."),
            ..vars()
        };
        assert_eq!(t.render_subfolder(&v).unwrap(), PathBuf::from("_."));
        assert_eq!(
            Template::parse("")
                .unwrap()
                .render_subfolder(&vars())
                .unwrap(),
            PathBuf::new()
        );
    }

    #[test]
    fn nfd_input_becomes_nfc() {
        let nfd = "Cafe\u{301}";
        let t = Template::parse(nfd).unwrap();
        assert_eq!(t.render(&vars()), "Caf\u{e9}");
        let v = Vars {
            name: nfd,
            ..vars()
        };
        assert_eq!(Template::parse("{name}").unwrap().render(&v), "Caf\u{e9}");
    }

    #[test]
    fn collision_key_folds_case_and_normalization() {
        assert_eq!(
            collision_key("Caf\u{e9}.ARW"),
            collision_key("CAFE\u{301}.arw")
        );
        assert_ne!(collision_key("a.arw"), collision_key("b.arw"));
    }

    #[test]
    fn suffixes() {
        assert_eq!(with_suffix("DSC0001.ARW", 1), "DSC0001-1.ARW");
        assert_eq!(with_suffix("a.b.JPG", 2), "a.b-2.JPG");
        assert_eq!(with_suffix("noext", 3), "noext-3");
        assert_eq!(with_suffix(".hidden", 1), ".hidden-1");
    }
}
