//! Capture One naming tokens: `<TokenName version="2"><Token Type="1" /></TokenName>`
//! (recipe file names and sub-folders), or `{name, tag}` pairs (import naming in
//! the preferences). Only "Image Name" (type 1, confirmed by the preferences'
//! `{name: "Image Name", tag: 1}`) has a Luxtable token; literal text is kept.
//! Other types are reported by number (their names are unconfirmed).

/// One piece of a C1 name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    Text(String),
    Typed(u32),
}

/// C1's "Image Name" token.
pub(crate) const IMAGE_NAME: u32 = 1;

/// Parse a `TokenName` document. Text between tokens, and a token's `Text`
/// attribute (**unconfirmed**: no literal text in the real recipes), are literals.
pub(crate) fn parse(xml: &str) -> Result<Vec<Token>, String> {
    if xml.trim().is_empty() {
        return Ok(Vec::new());
    }
    crate::xml::check_depth(xml).map_err(|e| format!("Name tokens: {e}"))?;
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("Invalid name tokens: {e}"))?;
    let root = doc.root_element();
    if !root.has_tag_name("TokenName") {
        return Err(format!(
            "Name tokens: root is <{}>, not <TokenName>",
            root.tag_name().name()
        ));
    }
    let mut out = Vec::new();
    for n in root.children() {
        if n.is_text() {
            let t = n.text().unwrap_or_default();
            if !t.trim().is_empty() {
                out.push(Token::Text(t.to_owned()));
            }
        } else if n.has_tag_name("Token") {
            if let Some(t) = n.attribute("Text").filter(|t| !t.is_empty()) {
                out.push(Token::Text(t.to_owned()));
            } else if let Some(ty) = n.attribute("Type").and_then(|t| t.trim().parse().ok()) {
                out.push(Token::Typed(ty));
            }
        }
    }
    Ok(out)
}

/// A Luxtable template for `tokens`: Image Name becomes `image_name`, text is
/// kept (with `{`/`}` doubled when `escape_braces`, lt-template's syntax).
/// Returns the template and the token types that have no translation.
pub(crate) fn template(
    tokens: &[Token],
    image_name: &str,
    escape_braces: bool,
) -> (String, Vec<u32>) {
    let mut out = String::new();
    let mut dropped = Vec::new();
    for t in tokens {
        match t {
            Token::Text(s) if escape_braces => {
                out.push_str(&s.replace('{', "{{").replace('}', "}}"));
            }
            Token::Text(s) => out.push_str(s),
            Token::Typed(IMAGE_NAME) => out.push_str(image_name),
            Token::Typed(n) => {
                if !dropped.contains(n) {
                    dropped.push(*n);
                }
            }
        }
    }
    (out, dropped)
}

/// "token types 11 and 12" for a note.
pub(crate) fn describe(types: &[u32]) -> String {
    let list: Vec<String> = types.iter().map(ToString::to_string).collect();
    match list.as_slice() {
        [one] => format!("token type {one}"),
        [init @ .., last] => format!("token types {} and {last}", init.join(", ")),
        [] => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_name_and_text() {
        let t = parse(r#"<TokenName version="2">web_<Token Type="1" /><Token Text="-{x}" /><Token Type="11" /></TokenName>"#)
            .unwrap();
        assert_eq!(
            t,
            vec![
                Token::Text("web_".into()),
                Token::Typed(1),
                Token::Text("-{x}".into()),
                Token::Typed(11)
            ]
        );
        assert_eq!(
            template(&t, "{name}", true),
            ("web_{name}-{{x}}".into(), vec![11])
        );
        assert_eq!(
            template(&t, "{original}", false),
            ("web_{original}-{x}".into(), vec![11])
        );
    }

    #[test]
    fn empty_and_invalid() {
        assert_eq!(parse("").unwrap(), vec![]);
        assert_eq!(parse(r#"<TokenName Version="2" />"#).unwrap(), vec![]);
        assert!(parse("<Other/>").unwrap_err().contains("not <TokenName>"));
        assert!(parse("<TokenName>").unwrap_err().contains("Invalid"));
        // A token without a usable type is ignored.
        assert_eq!(
            parse(r#"<TokenName><Token Type="x"/></TokenName>"#).unwrap(),
            vec![]
        );
    }

    #[test]
    fn describes_types() {
        assert_eq!(describe(&[11]), "token type 11");
        assert_eq!(describe(&[11, 12, 3]), "token types 11, 12 and 3");
        assert_eq!(describe(&[]), "");
        // Repeats are reported once.
        let (_, d) = template(&[Token::Typed(4), Token::Typed(4)], "{name}", true);
        assert_eq!(d, vec![4]);
    }
}
