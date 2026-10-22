//! A parser for the Lua table syntax Lightroom stores develop settings and
//! `.lrtemplate` presets in (`s = { Exposure2012 = 0.5, ToneCurve = { 0, 0 } }`).
//! It never evaluates anything: the grammar is tables, strings, numbers,
//! booleans and `nil`, and every other construct (calls, operators,
//! identifiers as values, `function`) is an error. Input size, nesting depth
//! and node count are capped, and a declared length is never allocated.

use std::collections::BTreeMap;

use crate::limits::MAX_LUA_BYTES;
use crate::model::Setting;

/// Deepest table nesting accepted.
const MAX_NEST: usize = 64;
/// Values and keys read before giving up.
const MAX_NODES: usize = 200_000;

/// Parse `text`: an optional `name =` or `return` prefix, then one table.
/// A table with only positional entries is a [`Setting::List`], any other a
/// [`Setting::Table`] (positional entries of a mixed table are keyed `"1"`,
/// `"2"`, …). `nil` entries are left out.
pub fn parse_lua_table(text: &str) -> Result<Setting, String> {
    if text.len() > MAX_LUA_BYTES {
        return Err(format!("More than {MAX_LUA_BYTES} bytes: not read"));
    }
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
        nodes: 0,
    };
    p.skip_ws()?;
    if p.eat_word("return") {
        p.skip_ws()?;
    } else if p.peek().is_some_and(is_ident_start) {
        p.ident();
        p.skip_ws()?;
        p.expect(b'=')?;
        p.skip_ws()?;
    }
    if p.peek() != Some(b'{') {
        return Err(p.err("expected a table"));
    }
    let v = p.value(0)?;
    p.skip_ws()?;
    p.eat(b';');
    p.skip_ws()?;
    if p.i != p.s.len() {
        return Err(p.err("unexpected text after the table"));
    }
    v.ok_or_else(|| "the value is nil".to_owned())
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    nodes: usize,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> String {
        format!("Lua: {what} at byte {}", self.i)
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.eat(b) {
            Ok(())
        } else {
            Err(self.err(&format!("expected `{}`", b as char)))
        }
    }

    fn count(&mut self) -> Result<(), String> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            Err(format!("More than {MAX_NODES} values: not read"))
        } else {
            Ok(())
        }
    }

    /// Whitespace and `--` comments (line and `--[[ ]]` block).
    fn skip_ws(&mut self) -> Result<(), String> {
        loop {
            while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
                self.i += 1;
            }
            if self.s[self.i..].starts_with(b"--") {
                self.i += 2;
                if let Some(level) = self.long_bracket_open() {
                    self.long_string(level)?;
                } else {
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.i += 1;
                    }
                }
            } else {
                return Ok(());
            }
        }
    }

    /// At `[` `=`* `[`: consume the opener and return the number of `=`.
    fn long_bracket_open(&mut self) -> Option<usize> {
        let rest = &self.s[self.i..];
        if rest.first() != Some(&b'[') {
            return None;
        }
        let level = rest[1..].iter().take_while(|&&b| b == b'=').count();
        if rest.get(1 + level) == Some(&b'[') {
            self.i += level + 2;
            Some(level)
        } else {
            None
        }
    }

    fn long_string(&mut self, level: usize) -> Result<String, String> {
        let mut close = vec![b']'];
        close.extend(std::iter::repeat_n(b'=', level));
        close.push(b']');
        let rest = &self.s[self.i..];
        let end = rest
            .windows(close.len())
            .position(|w| w == close.as_slice())
            .ok_or_else(|| self.err("unterminated long string or comment"))?;
        let mut body = &rest[..end];
        if body.first() == Some(&b'\n') {
            body = &body[1..];
        }
        let out = String::from_utf8_lossy(body).into_owned();
        self.i += end + close.len();
        Ok(out)
    }

    fn eat_word(&mut self, w: &str) -> bool {
        let b = w.as_bytes();
        let after = self.s.get(self.i + b.len()).copied();
        if self.s[self.i..].starts_with(b)
            && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.i += b.len();
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> String {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    fn string(&mut self, quote: u8) -> Result<String, String> {
        self.i += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(b) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            self.i += 1;
            match b {
                _ if b == quote => break,
                b'\n' => return Err(self.err("newline in string")),
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return Err(self.err("unterminated string"));
                    };
                    self.i += 1;
                    match e {
                        b'n' | b'\n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'a' | b'b' | b'f' | b'v' => {}
                        b'0'..=b'9' => {
                            let mut n = u32::from(e - b'0');
                            for _ in 0..2 {
                                match self.peek() {
                                    Some(d @ b'0'..=b'9') => {
                                        n = n * 10 + u32::from(d - b'0');
                                        self.i += 1;
                                    }
                                    _ => break,
                                }
                            }
                            let byte = u8::try_from(n).map_err(|_| self.err("bad escape"))?;
                            out.push(byte);
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(b),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn number(&mut self) -> Result<f64, String> {
        let start = self.i;
        let neg = self.eat(b'-');
        if !neg {
            self.eat(b'+');
        }
        let v = if self.s[self.i..].starts_with(b"0x") || self.s[self.i..].starts_with(b"0X") {
            self.i += 2;
            let h = self.i;
            while self.peek().is_some_and(|b| b.is_ascii_hexdigit()) {
                self.i += 1;
            }
            let digits = std::str::from_utf8(&self.s[h..self.i]).unwrap_or_default();
            u64::from_str_radix(digits, 16).map_err(|_| self.err("bad hex number"))? as f64
        } else {
            while self.peek().is_some_and(|b| {
                b.is_ascii_digit()
                    || matches!(b, b'.' | b'e' | b'E')
                    || ((b == b'-' || b == b'+') && matches!(self.s[self.i - 1], b'e' | b'E'))
            }) {
                self.i += 1;
            }
            std::str::from_utf8(&self.s[start..self.i])
                .ok()
                .and_then(|t| t.trim_start_matches(['+', '-']).parse::<f64>().ok())
                .ok_or_else(|| self.err("bad number"))?
        };
        let v = if neg { -v } else { v };
        if v.is_finite() {
            Ok(v)
        } else {
            Err(self.err("number out of range"))
        }
    }

    /// One value; `None` for `nil`.
    fn value(&mut self, depth: usize) -> Result<Option<Setting>, String> {
        self.count()?;
        self.skip_ws()?;
        let Some(b) = self.peek() else {
            return Err(self.err("unexpected end"));
        };
        match b {
            b'{' => self.table(depth).map(Some),
            b'"' | b'\'' => self.string(b).map(|s| Some(Setting::Text(s))),
            b'[' => {
                let level = self
                    .long_bracket_open()
                    .ok_or_else(|| self.err("unexpected `[`"))?;
                self.long_string(level).map(|s| Some(Setting::Text(s)))
            }
            b'-' | b'+' | b'.' | b'0'..=b'9' => self.number().map(|n| Some(Setting::Num(n))),
            _ if is_ident_start(b) => {
                if self.eat_word("true") {
                    Ok(Some(Setting::Bool(true)))
                } else if self.eat_word("false") {
                    Ok(Some(Setting::Bool(false)))
                } else if self.eat_word("nil") {
                    Ok(None)
                } else {
                    Err(self.err("only tables, strings, numbers, booleans and nil are allowed"))
                }
            }
            _ => Err(self.err("unexpected character")),
        }
    }

    fn table(&mut self, depth: usize) -> Result<Setting, String> {
        if depth >= MAX_NEST {
            return Err(format!("Nested more than {MAX_NEST} tables deep: not read"));
        }
        self.expect(b'{')?;
        let mut positional: Vec<Setting> = Vec::new();
        let mut named: BTreeMap<String, Setting> = BTreeMap::new();
        // Positional entries keep their index even when an earlier one is nil.
        let mut index = 0usize;
        loop {
            self.skip_ws()?;
            if self.eat(b'}') {
                break;
            }
            self.count()?;
            let key = self.entry_key()?;
            if let Some(k) = key {
                if let Some(v) = self.value(depth + 1)? {
                    named.insert(k, v);
                }
            } else {
                index += 1;
                if let Some(v) = self.value(depth + 1)? {
                    positional.push(v);
                    if positional.len() != index {
                        // A nil earlier in the list: keep what follows keyed by index.
                        named.insert(
                            index.to_string(),
                            positional.pop().unwrap_or(Setting::Bool(false)),
                        );
                    }
                }
            }
            self.skip_ws()?;
            if !(self.eat(b',') || self.eat(b';')) {
                self.skip_ws()?;
                self.expect(b'}')?;
                break;
            }
        }
        if named.is_empty() {
            return Ok(Setting::List(positional));
        }
        for (i, v) in positional.into_iter().enumerate() {
            named.insert((i + 1).to_string(), v);
        }
        Ok(Setting::Table(named))
    }

    /// `name =`, `["name"] =` or `[1] =` before a value; `None` when the entry is positional.
    fn entry_key(&mut self) -> Result<Option<String>, String> {
        match self.peek() {
            Some(b) if is_ident_start(b) => {
                let save = self.i;
                let name = self.ident();
                self.skip_ws()?;
                if self.peek() == Some(b'=') && self.s.get(self.i + 1) != Some(&b'=') {
                    self.i += 1;
                    Ok(Some(name))
                } else {
                    self.i = save;
                    Ok(None)
                }
            }
            Some(b'[') if !matches!(self.s.get(self.i + 1), Some(b'[' | b'=')) => {
                self.i += 1;
                self.skip_ws()?;
                let key = match self.peek() {
                    Some(q @ (b'"' | b'\'')) => self.string(q)?,
                    Some(b'-' | b'+' | b'.' | b'0'..=b'9') => {
                        let n = self.number()?;
                        if n.fract() == 0.0 && n.abs() < 1e15 {
                            format!("{}", n as i64)
                        } else {
                            n.to_string()
                        }
                    }
                    _ => return Err(self.err("unsupported table key")),
                };
                self.skip_ws()?;
                self.expect(b']')?;
                self.skip_ws()?;
                self.expect(b'=')?;
                Ok(Some(key))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(pairs: &[(&str, Setting)]) -> Setting {
        Setting::Table(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    }

    #[test]
    fn parses_a_develop_settings_table() {
        let text = "s = {\n\tAutoLateralCA = 1,\n\tExposure2012 = 0.5,\n\tCropAngle = -1.25e0,\n\
                    \tWhiteBalance = \"As Shot\",\n\tToneCurvePV2012 = {\n\t\t0,\n\t\t0,\n\t\t255,\n\t\t255,\n\t},\n\
                    \tConvertToGrayscale = false,\n\t[\"Odd Key\"] = 'x\\ty',\n\tGone = nil,\n}\n";
        let v = parse_lua_table(text).unwrap();
        let tab = v.table().unwrap();
        assert_eq!(tab["Exposure2012"], Setting::Num(0.5));
        assert_eq!(tab["CropAngle"], Setting::Num(-1.25));
        assert_eq!(tab["WhiteBalance"], Setting::Text("As Shot".into()));
        assert_eq!(tab["ConvertToGrayscale"], Setting::Bool(false));
        assert_eq!(tab["Odd Key"], Setting::Text("x\ty".into()));
        assert!(!tab.contains_key("Gone"));
        assert_eq!(
            tab["ToneCurvePV2012"],
            Setting::List([0.0, 0.0, 255.0, 255.0].map(Setting::Num).to_vec())
        );
    }

    #[test]
    fn prefixes_comments_and_long_strings() {
        let v = parse_lua_table(
            "-- header\nreturn { a = [[one\ntwo]], --[[ block ]] b = [==[x]]y]==]; }",
        )
        .unwrap();
        assert_eq!(
            v,
            t(&[
                ("a", Setting::Text("one\ntwo".into())),
                ("b", Setting::Text("x]]y".into()))
            ])
        );
        assert_eq!(
            parse_lua_table("{ 1, 2 }").unwrap(),
            Setting::List(vec![Setting::Num(1.0), Setting::Num(2.0)])
        );
        assert_eq!(parse_lua_table("{}").unwrap(), Setting::List(vec![]));
        assert_eq!(parse_lua_table("s = { }; ").unwrap(), Setting::List(vec![]));
        // Mixed tables key their positional entries by index; numeric keys likewise.
        let m = parse_lua_table("{ 7, x = 1, [3] = 9 }").unwrap();
        assert_eq!(
            m,
            t(&[
                ("1", Setting::Num(7.0)),
                ("3", Setting::Num(9.0)),
                ("x", Setting::Num(1.0))
            ])
        );
        // A nil in the middle of a list keeps later entries at their index.
        let g = parse_lua_table("{ 1, nil, 3 }").unwrap();
        assert_eq!(g, t(&[("1", Setting::Num(1.0)), ("3", Setting::Num(3.0))]));
        assert_eq!(
            parse_lua_table("{ 0x10, +2, .5, \"\\65\\x\" }")
                .unwrap()
                .list()
                .unwrap()[0],
            Setting::Num(16.0)
        );
        assert!(parse_lua_table("{ x == 1 }").is_err());
    }

    #[test]
    fn refuses_everything_that_is_not_data() {
        for bad in [
            "s = os.execute('x')",
            "s = { a = f() }",
            "s = { a = 1 + 2 }",
            "s = { a = function() end }",
            "s = { a = b }",
            "s = { a = 1e999 }",
            "s = { a = -1e999 }",
            "s = { a = 0x }",
            "s = { a = 1.2.3 }",
            "s = { a = \"open }",
            "s = { a = \"line\nbreak\" }",
            "s = { a = [[never closed }",
            "s = { a = 1",
            "s = { a = 1 } trailing",
            "s = nil",
            "s = 5",
            "s = { [ {} ] = 1 }",
            "s = { a = \"\\999\" }",
            "--[[ never closed",
            "",
            "s = {} } ",
            "s = { a 1 }",
            "{ ] }",
            "{ [",
        ] {
            assert!(parse_lua_table(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn nesting_size_and_node_limits() {
        let deep = |n: usize| format!("s = {}{}", "{".repeat(n), "}".repeat(n));
        assert!(parse_lua_table(&deep(MAX_NEST)).is_ok());
        assert!(
            parse_lua_table(&deep(MAX_NEST + 1))
                .unwrap_err()
                .contains("deep")
        );
        // Far deeper than the stack could take: still an error, not an overflow.
        assert!(parse_lua_table(&deep(1_000_000)).is_err());
        let big = format!("s = {{ a = \"{}\" }}", "x".repeat(MAX_LUA_BYTES));
        assert!(parse_lua_table(&big).unwrap_err().contains("bytes"));
        let many = format!("s = {{{}}}", "1,".repeat(MAX_NODES));
        assert!(parse_lua_table(&many).unwrap_err().contains("values"));
        // Hostile input of random bytes never panics.
        let junk: String = (0..5000u32)
            .map(|i| char::from((i * 31 % 95 + 32) as u8))
            .collect();
        assert!(parse_lua_table(&junk).is_err());
        assert!(parse_lua_table("s = { a = \"\u{e9}\u{4e2d}\" }").is_ok());
    }
}
