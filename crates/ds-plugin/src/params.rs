//! Typed contribution parameters: declared in the manifest, drawn by the host,
//! validated before a plug-in ever sees a value.

use serde::Deserialize;
use serde_json::Value;

/// A parameter's type, with its range or choices.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamType {
    Float {
        min: f64,
        max: f64,
    },
    Int {
        min: i64,
        max: i64,
    },
    Bool,
    Choice(Vec<String>),
    /// `#RRGGBB` or `#RRGGBBAA`.
    Color,
    Text {
        max_len: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub key: String,
    pub label: String,
    pub ty: ParamType,
    /// Always valid for `ty`.
    pub default: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawParam {
    key: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    choices: Vec<String>,
    #[serde(default)]
    max_len: Option<usize>,
    default: toml::Value,
}

const TEXT_MAX: usize = 1000;

impl RawParam {
    pub(crate) fn validate(self) -> Result<Param, String> {
        let at = format!("param `{}`", self.key);
        if self.key.is_empty()
            || !self
                .key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!("{at}: keys are lowercase letters, digits and _"));
        }
        let range = |lo: f64, hi: f64| -> Result<(f64, f64), String> {
            let (min, max) = (self.min.unwrap_or(lo), self.max.unwrap_or(hi));
            if !min.is_finite() || !max.is_finite() || max < min {
                Err(format!("{at}: `max` is below `min`"))
            } else {
                Ok((min, max))
            }
        };
        let ty = match self.ty.as_str() {
            "float" => {
                let (min, max) = range(0.0, 1.0)?;
                ParamType::Float { min, max }
            }
            "int" => {
                let (min, max) = range(0.0, 100.0)?;
                if min.fract() != 0.0 || max.fract() != 0.0 {
                    return Err(format!("{at}: an int's `min` and `max` are whole numbers"));
                }
                ParamType::Int {
                    min: min as i64,
                    max: max as i64,
                }
            }
            "bool" => ParamType::Bool,
            "color" => ParamType::Color,
            "choice" => {
                if self.choices.is_empty() || self.choices.iter().any(String::is_empty) {
                    return Err(format!("{at}: a choice needs non-empty `choices`"));
                }
                ParamType::Choice(self.choices)
            }
            "text" => ParamType::Text {
                max_len: self.max_len.unwrap_or(200).min(TEXT_MAX),
            },
            other => {
                return Err(format!(
                    "{at}: unknown type `{other}` (float, int, bool, choice, color, text)"
                ));
            }
        };
        let default = toml_to_json(self.default);
        let default = ty
            .check(&default)
            .map_err(|m| format!("{at}: default {m}"))?;
        Ok(Param {
            label: crate::text::clean(
                &self.label.unwrap_or_else(|| self.key.clone()),
                crate::text::LABEL_MAX,
            ),
            key: self.key,
            ty,
            default,
        })
    }
}

fn toml_to_json(v: toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

impl ParamType {
    /// `v` if it is valid for this type (an int given as 3.0 becomes 3), else why not.
    pub fn check(&self, v: &Value) -> Result<Value, String> {
        match (self, v) {
            (Self::Float { min, max }, Value::Number(n)) => {
                let x = n.as_f64().unwrap_or(f64::NAN);
                if x.is_finite() && (*min..=*max).contains(&x) {
                    Ok(Value::from(x))
                } else {
                    Err(format!("{x} is outside {min}–{max}"))
                }
            }
            (Self::Int { min, max }, Value::Number(n)) => {
                let x = n.as_f64().unwrap_or(f64::NAN);
                if x.fract() == 0.0 && (*min as f64..=*max as f64).contains(&x) {
                    Ok(Value::from(x as i64))
                } else {
                    Err(format!("{x} is not a whole number in {min}–{max}"))
                }
            }
            (Self::Bool, Value::Bool(_)) => Ok(v.clone()),
            (Self::Choice(c), Value::String(s)) if c.contains(s) => Ok(v.clone()),
            (Self::Choice(c), _) => Err(format!("must be one of {}", c.join(", "))),
            (Self::Color, Value::String(s)) if parse_color(s).is_some() => Ok(v.clone()),
            (Self::Color, _) => Err("must be a colour like #RRGGBB or #RRGGBBAA".into()),
            (Self::Text { max_len }, Value::String(s)) if s.chars().count() <= *max_len => {
                Ok(v.clone())
            }
            (Self::Text { max_len }, _) => {
                Err(format!("must be text of at most {max_len} characters"))
            }
            _ => Err(format!("has the wrong type ({v})")),
        }
    }
}

impl ParamType {
    /// A number clamped into this range (an int rounded); `None` for anything else.
    fn nearest(&self, v: &Value) -> Option<Value> {
        let x = v.as_f64().filter(|x| x.is_finite())?;
        match self {
            Self::Float { min, max } => Some(Value::from(x.clamp(*min, *max))),
            Self::Int { min, max } => Some(Value::from((x.round() as i64).clamp(*min, *max))),
            _ => None,
        }
    }
}

/// `#RRGGBB` / `#RRGGBBAA` → RGBA bytes.
#[must_use]
pub fn parse_color(s: &str) -> Option<[u8; 4]> {
    let hex = s.strip_prefix('#')?;
    if !(hex.len() == 6 || hex.len() == 8) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([
        byte(0)?,
        byte(2)?,
        byte(4)?,
        if hex.len() == 8 { byte(6)? } else { 255 },
    ])
}

/// The values a call gets: every declared param, the caller's value where it is valid,
/// else the default. Unknown keys are dropped; invalid values are errors.
/// The values a call gets: every declared param, the caller's value where it is valid,
/// else the nearest valid one (a number clamped into a range an update narrowed) or the
/// default, with a note for each change. Unknown keys are dropped. A recipe never stops
/// exporting because a plug-in update changed a param.
#[must_use]
pub fn resolve_lenient(
    params: &[Param],
    given: &serde_json::Map<String, Value>,
) -> (serde_json::Map<String, Value>, Vec<String>) {
    let mut out = serde_json::Map::new();
    let mut notes = Vec::new();
    for p in params {
        let v = match given.get(&p.key) {
            None => p.default.clone(),
            Some(v) => match p.ty.check(v) {
                Ok(v) => v,
                Err(why) => {
                    let fixed = p.ty.nearest(v).unwrap_or_else(|| p.default.clone());
                    notes.push(format!("{}: {why}; {fixed} is used", p.label));
                    fixed
                }
            },
        };
        out.insert(p.key.clone(), v);
    }
    (out, notes)
}

/// As [`resolve_lenient`], but any invalid value is an error (`ds-plugin validate`).
pub fn resolve(
    params: &[Param],
    given: &serde_json::Map<String, Value>,
) -> Result<serde_json::Map<String, Value>, String> {
    let (out, notes) = resolve_lenient(params, given);
    notes.into_iter().next().map_or(Ok(out), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(toml_text: &str) -> RawParam {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn float_param_checks_its_range() {
        let p = raw("key = \"width\"\ntype = \"float\"\nmin = 0.0\nmax = 0.1\ndefault = 0.02")
            .validate()
            .unwrap();
        assert_eq!(p.label, "width");
        assert_eq!(p.ty.check(&json!(0.05)).unwrap(), json!(0.05));
        assert!(p.ty.check(&json!(0.5)).unwrap_err().contains("outside"));
        assert!(p.ty.check(&json!("x")).is_err());
        let e = raw("key = \"w\"\ntype = \"float\"\nmin = 1.0\nmax = 0.0\ndefault = 0.5")
            .validate()
            .unwrap_err();
        assert!(e.contains("below"), "{e}");
        let e = raw("key = \"w\"\ntype = \"float\"\ndefault = 2.0")
            .validate()
            .unwrap_err();
        assert!(e.contains("default"), "{e}");
    }

    #[test]
    fn int_choice_bool_color_text() {
        let p = raw("key = \"n\"\ntype = \"int\"\nmin = 1\nmax = 9\ndefault = 3")
            .validate()
            .unwrap();
        assert_eq!(p.default, json!(3));
        assert_eq!(p.ty.check(&json!(4.0)).unwrap(), json!(4));
        assert!(p.ty.check(&json!(4.5)).is_err());
        assert!(
            raw("key = \"n\"\ntype = \"int\"\nmin = 0.5\nmax = 9\ndefault = 3")
                .validate()
                .is_err()
        );
        let c = raw("key = \"pos\"\ntype = \"choice\"\nchoices = [\"a\", \"b\"]\ndefault = \"b\"")
            .validate()
            .unwrap();
        assert!(c.ty.check(&json!("c")).unwrap_err().contains("one of a, b"));
        assert!(
            raw("key = \"pos\"\ntype = \"choice\"\ndefault = \"b\"")
                .validate()
                .is_err()
        );
        let b = raw("key = \"on\"\ntype = \"bool\"\ndefault = true")
            .validate()
            .unwrap();
        assert!(b.ty.check(&json!(1)).is_err());
        let col = raw("key = \"c\"\ntype = \"color\"\ndefault = \"#ff000080\"")
            .validate()
            .unwrap();
        assert!(col.ty.check(&json!("red")).is_err());
        let t = raw("key = \"t\"\ntype = \"text\"\nmax_len = 3\ndefault = \"abc\"")
            .validate()
            .unwrap();
        assert!(t.ty.check(&json!("abcd")).is_err());
        assert!(
            raw("key = \"t\"\ntype = \"blob\"\ndefault = 1")
                .validate()
                .unwrap_err()
                .contains("unknown type")
        );
        assert!(
            raw("key = \"Bad Key\"\ntype = \"bool\"\ndefault = true")
                .validate()
                .is_err()
        );
    }

    #[test]
    fn colors_parse() {
        assert_eq!(parse_color("#102030"), Some([16, 32, 48, 255]));
        assert_eq!(parse_color("#10203040"), Some([16, 32, 48, 64]));
        assert_eq!(parse_color("102030"), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#zz2030"), None);
    }

    #[test]
    fn resolve_fills_defaults_and_refuses_bad_values() {
        let w =
            raw("key = \"width\"\nlabel = \"Width\"\ntype = \"float\"\nmax = 0.1\ndefault = 0.02")
                .validate()
                .unwrap();
        let c = raw("key = \"color\"\ntype = \"color\"\ndefault = \"#000000\"")
            .validate()
            .unwrap();
        let params = [w, c];
        let given = json!({"width": 0.05, "other": 1})
            .as_object()
            .unwrap()
            .clone();
        let out = resolve(&params, &given).unwrap();
        assert_eq!(out.get("width"), Some(&json!(0.05)));
        assert_eq!(out.get("color"), Some(&json!("#000000")));
        assert!(!out.contains_key("other"));
        let bad = json!({"width": 7}).as_object().unwrap().clone();
        assert!(resolve(&params, &bad).unwrap_err().starts_with("Width:"));
    }

    /// N7: a value an update's narrower range no longer allows is clamped (an int
    /// rounded), a value of the wrong kind falls back to the default, each with a note;
    /// the strict form refuses.
    #[test]
    fn narrowed_ranges_clamp_with_a_note() {
        let w =
            raw("key = \"width\"\nlabel = \"Width\"\ntype = \"float\"\nmax = 0.1\ndefault = 0.02")
                .validate()
                .unwrap();
        let n = raw("key = \"n\"\ntype = \"int\"\nmin = 1\nmax = 5\ndefault = 2")
            .validate()
            .unwrap();
        let c = raw("key = \"color\"\ntype = \"color\"\ndefault = \"#000000\"")
            .validate()
            .unwrap();
        let params = [w, n, c];
        let given = json!({"width": 0.5, "n": 7.6, "color": 3});
        let Value::Object(given) = given else {
            unreachable!()
        };
        let (out, notes) = resolve_lenient(&params, &given);
        assert_eq!(out.get("width"), Some(&json!(0.1)));
        assert_eq!(out.get("n"), Some(&json!(5)));
        assert_eq!(out.get("color"), Some(&json!("#000000")));
        assert_eq!(notes.len(), 3);
        assert!(
            notes[0].starts_with("Width:") && notes[0].ends_with("0.1 is used"),
            "{notes:?}"
        );
        assert!(resolve(&params, &given).is_err());
    }
}
