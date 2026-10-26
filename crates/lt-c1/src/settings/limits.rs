//! Nesting limits, checked before a document is built. Settings files come from
//! wherever the user points the import (a downloaded settings pack too), and a
//! deeply nested document overflows the stack where it's walked or dropped. A
//! stack overflow aborts the app, so such a file is refused before parsing.

use std::io::Cursor;

use plist::Value;
use plist::stream::{Event, Reader};

use crate::xml::{MAX_DEPTH, too_deep};

/// Plist events read before giving up: a binary plist can share objects, so its
/// event stream can be far larger than the file.
const MAX_EVENTS: usize = 4_000_000;

/// A property list (XML or binary), after its event stream has been checked
/// for depth and size.
pub(crate) fn plist(bytes: &[u8]) -> Result<Value, String> {
    let mut depth = 0usize;
    for (n, ev) in Reader::new(Cursor::new(bytes)).enumerate() {
        if n > MAX_EVENTS {
            return Err(format!("More than {MAX_EVENTS} values: not read"));
        }
        match ev.map_err(|e| e.to_string())? {
            Event::StartArray(_) | Event::StartDictionary(_) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(too_deep());
                }
            }
            Event::EndCollection => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Value::from_reader(Cursor::new(bytes)).map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A binary plist of `n` nested one-element arrays, written directly (building
    /// it as a `Value` would itself overflow when dropped). Object i is an array
    /// holding object i+1; the last is an empty array.
    pub(crate) fn nested_binary_plist(n: usize) -> Vec<u8> {
        let mut out = b"bplist00".to_vec();
        let mut offsets = Vec::with_capacity(n);
        for i in 0..n {
            offsets.push(out.len() as u32);
            if i + 1 < n {
                out.push(0xA1);
                out.extend_from_slice(&((i + 1) as u32).to_be_bytes());
            } else {
                out.push(0xA0);
            }
        }
        let table = out.len() as u64;
        for o in offsets {
            out.extend_from_slice(&o.to_be_bytes());
        }
        out.extend_from_slice(&[0; 6]);
        out.push(4); // offset size
        out.push(4); // object reference size
        out.extend_from_slice(&(n as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes()); // top object
        out.extend_from_slice(&table.to_be_bytes());
        out
    }

    #[test]
    fn plist_depth_and_size() {
        let deep = nested_binary_plist(MAX_DEPTH + 2);
        assert!(plist(&deep).unwrap_err().contains("Nested"));
        let ok = nested_binary_plist(10);
        assert!(plist(&ok).unwrap().as_array().is_some());
        let xml = format!(
            "<?xml version=\"1.0\"?><plist version=\"1.0\">{}</plist>",
            crate::xml::tests::nested(MAX_DEPTH + 1, "array")
        );
        assert!(plist(xml.as_bytes()).unwrap_err().contains("Nested"));
        assert!(plist(b"junk").is_err());
        // Shared objects: one array referring to the same child many times over,
        // nested a few levels, expands past the event limit.
        let mut out = b"bplist00".to_vec();
        let levels = 8usize;
        let fan = 14usize; // 0xA0 | 15 would mean "length follows"
        let mut offsets = Vec::new();
        for i in 0..levels {
            offsets.push(out.len() as u32);
            if i + 1 < levels {
                out.push(0xA0 | fan as u8);
                for _ in 0..fan {
                    out.extend_from_slice(&((i + 1) as u32).to_be_bytes());
                }
            } else {
                out.push(0x10);
                out.push(1);
            }
        }
        let table = out.len() as u64;
        for o in &offsets {
            out.extend_from_slice(&o.to_be_bytes());
        }
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&[4, 4]);
        out.extend_from_slice(&(levels as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&table.to_be_bytes());
        assert!(plist(&out).unwrap_err().contains("values"));
    }
}
