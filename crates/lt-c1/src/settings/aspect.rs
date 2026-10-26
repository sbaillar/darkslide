//! User crop ratios (`Aspect Ratios 8.2/User Aspect Ratios.xml`): an XML
//! property list `{AspectRatios: [...], SelectedAspectRatioUID}`. The real
//! file's list is empty, so an entry's keys are **unconfirmed**: a dictionary
//! with a name (`Name`) and either `Width` + `Height` or a `Ratio`
//! (width / height), or a `"W:H"` string, in any letter case.

use plist::{Dictionary, Value};

use super::Note;

/// One user crop ratio.
#[derive(Debug, Clone, PartialEq)]
pub struct AspectRatioImport {
    /// C1's id for it (`UID`), else its name and ratio: the import's key.
    pub key: String,
    pub name: String,
    pub width: f64,
    pub height: f64,
    /// The Luxtable crop chip with the same ratio (`CropPreset` raw value,
    /// e.g. "4:5"), when there is one: nothing needs importing then.
    pub builtin: Option<&'static str>,
    pub notes: Vec<Note>,
}

/// Luxtable's fixed crop chips.
const BUILT_IN: [(&str, f64, f64); 7] = [
    ("1:1", 1.0, 1.0),
    ("4:5", 4.0, 5.0),
    ("5:4", 5.0, 4.0),
    ("3:2", 3.0, 2.0),
    ("2:3", 2.0, 3.0),
    ("16:9", 16.0, 9.0),
    ("9:16", 9.0, 16.0),
];

fn field<'a>(d: &'a Dictionary, names: &[&str]) -> Option<&'a Value> {
    d.iter()
        .find(|(k, _)| names.iter().any(|n| k.eq_ignore_ascii_case(n)))
        .map(|(_, v)| v)
}

fn number(v: &Value) -> Option<f64> {
    v.as_real()
        .or_else(|| v.as_signed_integer().map(|i| i as f64))
        .or_else(|| v.as_string().and_then(|s| s.trim().parse().ok()))
        .filter(|f: &f64| f.is_finite() && *f > 0.0)
}

/// `"16:9"`, `"16x9"`, `"16 × 9"` → (16, 9).
fn ratio_text(s: &str) -> Option<(f64, f64)> {
    let (w, h) = s.split_once([':', 'x', 'X', '×', '/'])?;
    let (w, h) = (w.trim().parse::<f64>().ok()?, h.trim().parse::<f64>().ok()?);
    (w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()).then_some((w, h))
}

fn entry(v: &Value) -> Option<(Option<String>, String, f64, f64)> {
    if let Some(s) = v.as_string() {
        let (w, h) = ratio_text(s)?;
        return Some((None, s.trim().to_owned(), w, h));
    }
    let d = v.as_dictionary()?;
    let name = field(d, &["Name", "Title"])
        .and_then(Value::as_string)
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let uid = field(d, &["UID", "UUID", "Identifier"])
        .and_then(Value::as_string)
        .map(str::to_owned);
    let (w, h) = match (
        field(d, &["Width"]).and_then(number),
        field(d, &["Height"]).and_then(number),
    ) {
        (Some(w), Some(h)) => (w, h),
        _ => match field(d, &["Ratio", "AspectRatio"]) {
            Some(Value::String(s)) => ratio_text(s)?,
            Some(r) => (number(r)?, 1.0),
            None => name.and_then(ratio_text)?,
        },
    };
    let name = name.map_or_else(|| format!("{} : {}", trim(w), trim(h)), str::to_owned);
    Some((uid, name, w, h))
}

/// `16.0` → "16", `1.85` → "1.85".
fn trim(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The ratios in the file, and a description of each entry that couldn't be read.
pub(crate) fn parse(bytes: &[u8]) -> Result<(Vec<AspectRatioImport>, Vec<String>), String> {
    let root =
        super::limits::plist(bytes).map_err(|e| format!("Invalid aspect ratio list: {e}"))?;
    let list = root
        .as_dictionary()
        .and_then(|d| field(d, &["AspectRatios", "UserAspectRatios"]))
        .and_then(Value::as_array)
        .ok_or("Invalid aspect ratio list: no AspectRatios array")?;
    let mut out = Vec::new();
    let mut bad = Vec::new();
    for (i, v) in list.iter().enumerate() {
        let Some((uid, name, width, height)) = entry(v) else {
            bad.push(format!("Entry {}: not a ratio Luxtable can read", i + 1));
            continue;
        };
        let r = width / height;
        let builtin = BUILT_IN
            .iter()
            .find(|(_, w, h)| (w / h - r).abs() < 1e-3)
            .map(|(n, ..)| *n);
        let mut notes = Vec::new();
        if let Some(b) = builtin {
            notes.push(Note::approx(
                "Ratio",
                format!("Same as Luxtable's {b} crop: nothing to import"),
            ));
        }
        let key = uid.unwrap_or_else(|| format!("{name}|{}:{}", trim(width), trim(height)));
        if out.iter().any(|a: &AspectRatioImport| a.key == key) {
            bad.push(format!(
                "Entry {}: a duplicate of \u{201c}{name}\u{201d}",
                i + 1
            ));
            continue;
        }
        out.push(AspectRatioImport {
            key,
            name,
            width,
            height,
            builtin,
            notes,
        });
    }
    Ok((out, bad))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(entries: Vec<Value>) -> Vec<u8> {
        let mut root = Dictionary::new();
        root.insert("AspectRatios".into(), Value::Array(entries));
        root.insert(
            "SelectedAspectRatioUID".into(),
            Value::String("OriginalID".into()),
        );
        let mut out = Vec::new();
        Value::Dictionary(root).to_writer_xml(&mut out).unwrap();
        out
    }

    fn dict(pairs: &[(&str, Value)]) -> Value {
        let mut d = Dictionary::new();
        for (k, v) in pairs {
            d.insert((*k).into(), v.clone());
        }
        Value::Dictionary(d)
    }

    #[test]
    fn entries_in_every_shape() {
        let (list, bad) = parse(&doc(vec![
            dict(&[
                ("Name", Value::String("Cinema".into())),
                ("Width", Value::Real(2.39)),
                ("Height", Value::Integer(1.into())),
                ("UID", Value::String("U1".into())),
            ]),
            dict(&[
                ("name", Value::String("Square".into())),
                ("ratio", Value::Real(1.0)),
            ]),
            dict(&[("Ratio", Value::String("6x7".into()))]),
            dict(&[("Name", Value::String("65:24".into()))]),
            Value::String("4:5".into()),
            dict(&[("Name", Value::String("Broken".into()))]),
            Value::Integer(3.into()),
            Value::String("4:5".into()),
        ]))
        .unwrap();
        let rows: Vec<(&str, &str, f64, f64, Option<&str>)> = list
            .iter()
            .map(|a| {
                (
                    a.key.as_str(),
                    a.name.as_str(),
                    a.width,
                    a.height,
                    a.builtin,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("U1", "Cinema", 2.39, 1.0, None),
                ("Square|1:1", "Square", 1.0, 1.0, Some("1:1")),
                ("6 : 7|6:7", "6 : 7", 6.0, 7.0, None),
                ("65:24|65:24", "65:24", 65.0, 24.0, None),
                ("4:5|4:5", "4:5", 4.0, 5.0, Some("4:5")),
            ]
        );
        assert!(list[1].notes[0].text.contains("nothing to import"));
        assert_eq!(
            bad,
            [
                "Entry 6: not a ratio Luxtable can read",
                "Entry 7: not a ratio Luxtable can read",
                "Entry 8: a duplicate of \u{201c}4:5\u{201d}"
            ]
        );
    }

    #[test]
    fn empty_and_invalid_files() {
        assert_eq!(parse(&doc(vec![])).unwrap(), (vec![], vec![]));
        assert!(parse(b"<plist").unwrap_err().contains("Invalid"));
        let mut out = Vec::new();
        Value::Dictionary(Dictionary::new())
            .to_writer_xml(&mut out)
            .unwrap();
        assert!(parse(&out).unwrap_err().contains("no AspectRatios"));
        assert_eq!(ratio_text("0:1"), None);
        assert_eq!(trim(1.85), "1.85");
    }
}
