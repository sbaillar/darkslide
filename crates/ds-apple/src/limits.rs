//! Bounds on what a library may make the reader do. A library comes from an
//! old disk or a backup and is read as untrusted input: a deeply nested
//! property list would overflow the stack where it is built or dropped, and a
//! huge one would exhaust memory, so both are refused before parsing.

use std::io::Cursor;

use plist::Value;
use plist::stream::{Event, Reader};

/// Deepest nesting a property list may have.
pub(crate) const MAX_DEPTH: usize = 64;
/// Values in one adjustment's archived parameters.
pub(crate) const MAX_ARCHIVE_EVENTS: usize = 100_000;
/// Values in an `AlbumData.xml` (a 100 000-photo library holds a few million).
pub(crate) const MAX_ALBUMDATA_EVENTS: usize = 60_000_000;
/// Largest `AlbumData.xml` read.
pub(crate) const MAX_ALBUMDATA_BYTES: u64 = 1 << 30;
/// Largest database copied (`Library.apdb`, `Properties.apdb`).
pub(crate) const MAX_DB_BYTES: u64 = 8 << 30;
/// Most photos read from one library.
pub(crate) const MAX_PHOTOS: usize = 2_000_000;
/// Most keywords kept per photo.
pub(crate) const MAX_KEYWORDS: usize = 1000;
/// Longest text kept from one field (titles, captions, names).
pub(crate) const MAX_TEXT: usize = 64 << 10;
/// Largest archived adjustment read.
pub(crate) const MAX_ARCHIVE_BYTES: usize = 4 << 20;

/// `s` cut to [`MAX_TEXT`] bytes at a character boundary.
pub(crate) fn cap_text(mut s: String) -> String {
    if s.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// A property list (XML or binary) after its event stream has been checked
/// for depth and for at most `max_events` values.
pub(crate) fn plist(bytes: &[u8], max_events: usize) -> Result<Value, String> {
    let mut depth = 0usize;
    for (n, ev) in Reader::new(Cursor::new(bytes)).enumerate() {
        if n > max_events {
            return Err(format!("more than {max_events} values: not read"));
        }
        match ev.map_err(|e| e.to_string())? {
            Event::StartArray(_) | Event::StartDictionary(_) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(format!(
                        "nested more than {MAX_DEPTH} levels deep: not read"
                    ));
                }
            }
            Event::EndCollection => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Value::from_reader(Cursor::new(bytes)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested_xml(n: usize) -> String {
        format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\">{}{}</plist>",
            "<array>".repeat(n),
            "</array>".repeat(n)
        )
    }

    #[test]
    fn deep_or_huge_lists_are_refused() {
        assert!(plist(nested_xml(MAX_DEPTH).as_bytes(), 1000).is_ok());
        let deep = plist(nested_xml(MAX_DEPTH + 1).as_bytes(), 1000).unwrap_err();
        assert!(deep.contains("nested"), "{deep}");
        let many = format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><array>{}</array></plist>",
            "<integer>1</integer>".repeat(20)
        );
        assert!(plist(many.as_bytes(), 10).unwrap_err().contains("values"));
        assert!(plist(b"not a plist", 10).is_err());
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let s = "\u{e9}".repeat(MAX_TEXT); // two bytes each
        let cut = cap_text(s);
        assert!(cut.len() <= MAX_TEXT);
        assert!(cut.chars().all(|c| c == '\u{e9}'));
        assert_eq!(cap_text("short".into()), "short");
    }
}
