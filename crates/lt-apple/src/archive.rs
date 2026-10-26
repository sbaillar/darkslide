//! The parameters of one Aperture / iPhoto adjustment. `RKImageAdjustment.data`
//! holds a binary property list: an `NSKeyedArchiver` archive of a dictionary
//! (parameter name → value; a nested dictionary for grouped parameters), as
//! open-source readers of these libraries decode it. Each value is read into a
//! [`Setting`] under its name, nested names joined with `.`; objects that are
//! not plain values (vectors, colours, masks) are kept as their class name, so
//! the mapping can tell they are there without guessing at their contents.

use std::collections::HashSet;

use lt_lr::Setting;
use plist::{Dictionary, Value};

use crate::limits::{MAX_ARCHIVE_BYTES, MAX_ARCHIVE_EVENTS, MAX_DEPTH, cap_text, plist};

/// Most parameters read from one adjustment.
const MAX_PARAMS: usize = 4096;
/// Most steps (objects visited, references followed) one decode may take. An
/// archive may share an object between several parents, so a chain of objects
/// that each refer to the next twice would otherwise take 2^depth steps.
const MAX_STEPS: usize = 4 * MAX_ARCHIVE_EVENTS;

/// The parameters of one archived adjustment, by (possibly dotted) name.
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<(String, Setting)>, String> {
    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(format!("too large ({} bytes)", bytes.len()));
    }
    let root = plist(bytes, MAX_ARCHIVE_EVENTS)?;
    let mut out = Vec::new();
    match keyed(&root) {
        Some((objects, top)) => {
            let mut d = Decoder {
                objects,
                out: &mut out,
                seen: HashSet::new(),
                steps: 0,
            };
            d.object("", top, 0)?;
        }
        None => plain("", &root, 0, &mut out)?,
    }
    Ok(out)
}

/// `$objects` and the root's index, when `root` is a keyed archive.
fn keyed(root: &Value) -> Option<(&[Value], usize)> {
    let d = root.as_dictionary()?;
    let objects = d.get("$objects")?.as_array()?;
    let top = d.get("$top")?.as_dictionary()?;
    let uid = top.get("root").or_else(|| top.values().next())?;
    Some((objects, uid_index(uid)?))
}

fn uid_index(v: &Value) -> Option<usize> {
    match v {
        Value::Uid(u) => usize::try_from(u.get()).ok(),
        _ => None,
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

fn push(out: &mut Vec<(String, Setting)>, key: String, s: Setting) -> Result<(), String> {
    if out.len() >= MAX_PARAMS {
        return Err(format!("more than {MAX_PARAMS} parameters"));
    }
    out.push((key, s));
    Ok(())
}

fn scalar(v: &Value) -> Option<Setting> {
    Some(match v {
        Value::Boolean(b) => Setting::Bool(*b),
        Value::Real(r) => Setting::Num(*r),
        Value::Integer(i) => Setting::Num(
            i.as_signed()
                .map(|n| n as f64)
                .or_else(|| i.as_unsigned().map(|n| n as f64))?,
        ),
        Value::String(s) => Setting::Text(cap_text(s.clone())),
        _ => return None,
    })
}

/// A property list that is not an archive: its dictionary, flattened.
fn plain(
    prefix: &str,
    v: &Value,
    depth: usize,
    out: &mut Vec<(String, Setting)>,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("nested too deep".into());
    }
    match v {
        Value::Dictionary(d) => {
            for (k, v) in d {
                plain(&join(prefix, k), v, depth + 1, out)?;
            }
            Ok(())
        }
        Value::Array(a) => {
            let items = a.iter().filter_map(scalar).collect();
            push(out, prefix.to_owned(), Setting::List(items))
        }
        v => match scalar(v) {
            Some(s) => push(out, prefix.to_owned(), s),
            None => Ok(()),
        },
    }
}

struct Decoder<'a> {
    objects: &'a [Value],
    out: &'a mut Vec<(String, Setting)>,
    /// Objects on the current path: a cycle is never followed.
    seen: HashSet<usize>,
    /// Steps taken so far, against [`MAX_STEPS`].
    steps: usize,
}

impl<'a> Decoder<'a> {
    fn step(&mut self) -> Result<(), String> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(format!("more than {MAX_STEPS} steps to decode: not read"));
        }
        Ok(())
    }

    fn get(&self, i: usize) -> Result<&'a Value, String> {
        let objects: &'a [Value] = self.objects;
        objects
            .get(i)
            .ok_or_else(|| format!("object {i} is missing"))
    }

    fn class_name(&self, d: &Dictionary) -> Option<String> {
        let c = self.objects.get(uid_index(d.get("$class")?)?)?;
        let name = c.as_dictionary()?.get("$classname")?.as_string()?;
        Some(name.to_owned())
    }

    fn object(&mut self, key: &str, i: usize, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("nested too deep".into());
        }
        self.step()?;
        let v = self.get(i)?;
        // `$null` is object 0 in every archive.
        if i == 0 && v.as_string() == Some("$null") {
            return Ok(());
        }
        if let Some(s) = scalar(v) {
            return push(self.out, key.to_owned(), s);
        }
        let Some(d) = v.as_dictionary() else {
            return Ok(());
        };
        if !self.seen.insert(i) {
            return Err("an object contains itself".into());
        }
        let r = self.dictionary(key, d, depth);
        self.seen.remove(&i);
        r
    }

    fn dictionary(&mut self, key: &str, d: &'a Dictionary, depth: usize) -> Result<(), String> {
        let refs = |name: &str| -> Option<Vec<usize>> {
            d.get(name)?.as_array()?.iter().map(uid_index).collect()
        };
        match (refs("NS.keys"), refs("NS.objects")) {
            (Some(keys), Some(vals)) => {
                for (k, v) in keys.into_iter().zip(vals) {
                    self.step()?;
                    let Some(name) = self.get(k)?.as_string().map(str::to_owned) else {
                        continue;
                    };
                    self.object(&join(key, &name), v, depth + 1)?;
                }
                Ok(())
            }
            (None, Some(vals)) => {
                let mut items = Vec::new();
                for v in vals {
                    self.step()?;
                    if let Some(s) = scalar(self.get(v)?) {
                        items.push(s);
                    }
                }
                push(self.out, key.to_owned(), Setting::List(items))
            }
            _ => {
                // A number boxed as an object, or a class Luxtable doesn't read.
                if let Some(s) = d.get("NS.number").and_then(scalar) {
                    return push(self.out, key.to_owned(), s);
                }
                let class = self.class_name(d).unwrap_or_else(|| "object".into());
                push(
                    self.out,
                    key.to_owned(),
                    Setting::Text(format!("<{class}>")),
                )
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use plist::Uid;

    /// An `NSKeyedArchiver` archive of a dictionary, the way Foundation writes one:
    /// `$objects[0]` is `$null`, the root dictionary refers to keys and values by UID.
    pub(crate) fn archive(params: &[(&str, Value)]) -> Vec<u8> {
        let uid = |n: usize| Value::Uid(Uid::new(n as u64));
        let mut objects = vec![Value::String("$null".into()), Value::Boolean(false)];
        let class = Value::Dictionary(Dictionary::from_iter([
            (
                "$classname".to_owned(),
                Value::String("NSMutableDictionary".into()),
            ),
            (
                "$classes".to_owned(),
                Value::Array(vec![
                    Value::String("NSMutableDictionary".into()),
                    Value::String("NSDictionary".into()),
                    Value::String("NSObject".into()),
                ]),
            ),
        ]));
        let (mut keys, mut vals) = (Vec::new(), Vec::new());
        for (k, v) in params {
            objects.push(Value::String((*k).to_owned()));
            keys.push(uid(objects.len() - 1));
            objects.push(v.clone());
            vals.push(uid(objects.len() - 1));
        }
        objects.push(class);
        let class_at = objects.len() - 1;
        objects[1] = Value::Dictionary(Dictionary::from_iter([
            ("NS.keys".to_owned(), Value::Array(keys)),
            ("NS.objects".to_owned(), Value::Array(vals)),
            ("$class".to_owned(), uid(class_at)),
        ]));
        let root = Value::Dictionary(Dictionary::from_iter([
            (
                "$archiver".to_owned(),
                Value::String("NSKeyedArchiver".into()),
            ),
            ("$version".to_owned(), Value::Integer(100_000.into())),
            (
                "$top".to_owned(),
                Value::Dictionary(Dictionary::from_iter([("root".to_owned(), uid(1))])),
            ),
            ("$objects".to_owned(), Value::Array(objects)),
        ]));
        let mut out = Vec::new();
        root.to_writer_binary(&mut out).unwrap();
        out
    }

    fn get<'a>(p: &'a [(String, Setting)], k: &str) -> Option<&'a Setting> {
        p.iter().find(|(n, _)| n == k).map(|(_, s)| s)
    }

    #[test]
    fn keyed_archives_decode_to_named_values() {
        let bytes = archive(&[
            ("inputExposure", Value::Real(0.5)),
            ("inputRecovery", Value::Integer(1.into())),
            ("enabled", Value::Boolean(true)),
            ("name", Value::String("x".into())),
        ]);
        let p = decode(&bytes).unwrap();
        assert_eq!(get(&p, "inputExposure"), Some(&Setting::Num(0.5)));
        assert_eq!(get(&p, "inputRecovery"), Some(&Setting::Num(1.0)));
        assert_eq!(get(&p, "enabled"), Some(&Setting::Bool(true)));
        assert_eq!(get(&p, "name"), Some(&Setting::Text("x".into())));
    }

    /// Nested dictionaries, arrays, boxed numbers, other classes, `$null`, and
    /// broken references.
    #[test]
    fn nested_objects_and_damage() {
        let uid = |n: u64| Value::Uid(Uid::new(n));
        let dict = |pairs: Vec<(&str, Value)>| {
            Value::Dictionary(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
        };
        let objects = vec![
            Value::String("$null".into()), // 0
            dict(vec![
                (
                    "NS.keys",
                    Value::Array(vec![uid(2), uid(3), uid(4), uid(5), uid(6), uid(14)]),
                ),
                (
                    "NS.objects",
                    Value::Array(vec![uid(7), uid(9), uid(10), uid(11), uid(0), uid(7)]),
                ),
            ]), // 1
            Value::String("inner".into()), // 2
            Value::String("list".into()),  // 3
            Value::String("boxed".into()), // 4
            Value::String("vector".into()), // 5
            Value::String("nothing".into()), // 6
            dict(vec![
                ("NS.keys", Value::Array(vec![uid(8)])),
                ("NS.objects", Value::Array(vec![uid(12)])),
            ]), // 7
            Value::String("x".into()),     // 8
            dict(vec![(
                "NS.objects",
                Value::Array(vec![uid(12), uid(13), uid(7)]),
            )]), // 9
            dict(vec![("NS.number", Value::Real(2.5))]), // 10
            dict(vec![("$class", uid(15))]), // 11
            Value::Real(1.0),              // 12
            Value::Integer((-3).into()),   // 13
            Value::Data(vec![1]),          // 14: a key that isn't text
            dict(vec![("$classname", Value::String("CIVector".into()))]), // 15
        ];
        let root = dict(vec![
            ("$top", dict(vec![("root", uid(1))])),
            ("$objects", Value::Array(objects.clone())),
        ]);
        let mut bytes = Vec::new();
        root.to_writer_binary(&mut bytes).unwrap();
        let p = decode(&bytes).unwrap();
        assert_eq!(get(&p, "inner.x"), Some(&Setting::Num(1.0)));
        assert_eq!(
            get(&p, "list"),
            Some(&Setting::List(vec![Setting::Num(1.0), Setting::Num(-3.0)]))
        );
        assert_eq!(get(&p, "boxed"), Some(&Setting::Num(2.5)));
        assert_eq!(get(&p, "vector"), Some(&Setting::Text("<CIVector>".into())));
        assert!(get(&p, "nothing").is_none());

        // A reference past the end, and an object that contains itself.
        let mut broken = objects.clone();
        broken[9] = dict(vec![
            ("NS.keys", Value::Array(vec![uid(8)])),
            ("NS.objects", Value::Array(vec![uid(99)])),
        ]);
        let mut b = Vec::new();
        dict(vec![
            ("$top", dict(vec![("root", uid(9))])),
            ("$objects", Value::Array(broken)),
        ])
        .to_writer_binary(&mut b)
        .unwrap();
        assert!(decode(&b).unwrap_err().contains("missing"));
        let mut cyclic = objects;
        cyclic[7] = dict(vec![
            ("NS.keys", Value::Array(vec![uid(8)])),
            ("NS.objects", Value::Array(vec![uid(7)])),
        ]);
        let mut b = Vec::new();
        dict(vec![
            ("$top", dict(vec![("root", uid(7))])),
            ("$objects", Value::Array(cyclic)),
        ])
        .to_writer_binary(&mut b)
        .unwrap();
        assert!(decode(&b).unwrap_err().contains("itself"));
    }

    /// Each level refers to the next twice: 2^64 paths through 64 objects. The
    /// decode must give up quickly instead of walking them all.
    #[test]
    fn shared_objects_cannot_multiply_the_work() {
        let uid = |n: usize| Value::Uid(Uid::new(n as u64));
        let levels = MAX_DEPTH;
        // 0: $null, 1: the key, 2..: the levels; the last refers to $null.
        let mut objects = vec![Value::String("$null".into()), Value::String("k".into())];
        for l in 0..levels {
            let next = if l + 1 == levels { 0 } else { l + 3 };
            objects.push(Value::Dictionary(Dictionary::from_iter([
                ("NS.keys".to_owned(), Value::Array(vec![uid(1), uid(1)])),
                (
                    "NS.objects".to_owned(),
                    Value::Array(vec![uid(next), uid(next)]),
                ),
            ])));
        }
        let root = Value::Dictionary(Dictionary::from_iter([
            (
                "$top".to_owned(),
                Value::Dictionary(Dictionary::from_iter([("root".to_owned(), uid(2))])),
            ),
            ("$objects".to_owned(), Value::Array(objects)),
        ]));
        let mut b = Vec::new();
        root.to_writer_binary(&mut b).unwrap();
        let t = std::time::Instant::now();
        let e = decode(&b).unwrap_err();
        assert!(e.contains("steps"), "{e}");
        assert!(
            t.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn plain_dictionaries_are_read_too() {
        let v = Value::Dictionary(Dictionary::from_iter([
            ("a".to_owned(), Value::Real(1.0)),
            (
                "b".to_owned(),
                Value::Dictionary(Dictionary::from_iter([(
                    "c".to_owned(),
                    Value::Array(vec![Value::Boolean(true), Value::Data(vec![])]),
                )])),
            ),
            ("d".to_owned(), Value::Data(vec![1])),
        ]));
        let mut b = Vec::new();
        v.to_writer_binary(&mut b).unwrap();
        let p = decode(&b).unwrap();
        assert_eq!(get(&p, "a"), Some(&Setting::Num(1.0)));
        assert_eq!(
            get(&p, "b.c"),
            Some(&Setting::List(vec![Setting::Bool(true)]))
        );
        assert!(get(&p, "d").is_none());
    }

    #[test]
    fn junk_and_oversized_data_are_errors() {
        assert!(decode(b"junk").is_err());
        assert!(
            decode(&vec![0u8; MAX_ARCHIVE_BYTES + 1])
                .unwrap_err()
                .contains("large")
        );
        let many: Vec<(String, Value)> = (0..=MAX_PARAMS)
            .map(|i| (format!("k{i}"), Value::Real(1.0)))
            .collect();
        let refs: Vec<(&str, Value)> = many.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        assert!(decode(&archive(&refs)).unwrap_err().contains("parameters"));
    }
}
