//! Size and nesting limits for everything read from a Lightroom source. A
//! deeply nested document overflows the stack where it is parsed or walked,
//! and a stack overflow aborts the app, so nesting is counted first with a
//! real tokenizer (`xmlparser`, the lexer roxmltree's own was derived from).
//! It is iterative and understands CDATA, comments, processing instructions
//! and declarations, so markup-looking text inside them isn't counted. Input
//! it can't tokenize is refused: the check fails closed.

/// Far deeper than anything Lightroom writes (masks nest about 8 levels).
pub(crate) const MAX_DEPTH: usize = 256;
/// One XMP packet (a sidecar, an embedded packet, a catalog's per-photo XMP).
pub(crate) const MAX_XMP_BYTES: u64 = 16 << 20;
/// One photo's develop settings in a catalog, or one preset file.
pub(crate) const MAX_LUA_BYTES: usize = 4 << 20;
/// A catalog is copied before it is read; bigger ones are refused.
pub(crate) const MAX_CATALOG_BYTES: u64 = 8 << 30;
/// Photos read from one catalog.
pub(crate) const MAX_PHOTOS: usize = 2_000_000;
/// Longest text field kept (keyword, title, caption, copyright, label, path part).
pub(crate) const MAX_TEXT: usize = 64 << 10;
/// Most keywords kept per photo.
pub(crate) const MAX_KEYWORDS: usize = 1000;
/// Develop-settings text parsed from one catalog, in total.
pub(crate) const MAX_DEVELOP_TOTAL: usize = 512 << 20;

/// `s` cut to [`MAX_TEXT`] bytes at a character boundary; `true` when it was cut.
pub(crate) fn cap_text(mut s: String) -> (String, bool) {
    if s.len() <= MAX_TEXT {
        return (s, false);
    }
    let mut end = MAX_TEXT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    (s, true)
}

pub(crate) fn too_deep() -> String {
    format!("Nested more than {MAX_DEPTH} levels deep: not read")
}

/// Refuse XML nested deeper than [`MAX_DEPTH`], or that can't be tokenized.
pub fn check_depth(xml: &str) -> Result<(), String> {
    use xmlparser::{ElementEnd, Token, Tokenizer};
    let mut depth = 0usize;
    for token in Tokenizer::from(xml) {
        match token.map_err(|e| format!("Invalid XML: {e}"))? {
            Token::ElementStart { .. } => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(too_deep());
                }
            }
            Token::ElementEnd {
                end: ElementEnd::Close(..) | ElementEnd::Empty,
                ..
            } => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `n` nested `tag` elements.
    pub(crate) fn nested(n: usize, tag: &str) -> String {
        format!(
            "{}{}",
            format!("<{tag}>").repeat(n),
            format!("</{tag}>").repeat(n)
        )
    }

    #[test]
    fn depth_is_counted_by_a_real_tokenizer() {
        assert!(check_depth(&nested(MAX_DEPTH, "a")).is_ok());
        assert!(
            check_depth(&nested(MAX_DEPTH + 1, "a"))
                .unwrap_err()
                .contains("Nested")
        );
        let flat = format!(
            "<?xml version=\"1.0\"?><!-- <a> > --><OR>{}</OR>",
            "<E K=\"a>b\" V='x' />".repeat(10_000)
        );
        assert!(check_depth(&flat).is_ok());
    }

    #[test]
    fn cdata_and_processing_instructions_dont_hide_nesting() {
        for unit in ["<a><![CDATA[></z>]]>", "<a><?p ></z>?>"] {
            let deep = format!("<r>{}</r>", unit.repeat(MAX_DEPTH + 1));
            assert!(check_depth(&deep).unwrap_err().contains("Nested"), "{unit}");
        }
        assert!(check_depth("<a><![CDATA[<b><c>]]></a>").is_ok());
    }

    #[test]
    fn untokenizable_input_is_refused() {
        assert!(
            check_depth("<a attr=\"open")
                .unwrap_err()
                .contains("Invalid XML")
        );
        assert!(check_depth("<a><!-- never closed").is_err());
    }
}
