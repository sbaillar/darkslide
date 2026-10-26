//! Nesting limit for XML read from Capture One files (settings files, `.cos`
//! files, smart-album criteria). A deeply nested document overflows the stack
//! where it's parsed or walked, and a stack overflow aborts the app, so the
//! depth is counted first with a real XML tokenizer (`xmlparser`, the lexer
//! roxmltree's own was derived from). It is iterative and understands CDATA,
//! comments, processing instructions and declarations, so markup-looking text
//! inside them isn't counted. Input it can't tokenize is refused: the check
//! fails closed rather than letting the parser see what it didn't.

/// Far deeper than anything Capture One writes (`.cos` files nest 4 levels).
pub(crate) const MAX_DEPTH: usize = 256;

pub(crate) fn too_deep() -> String {
    format!("Nested more than {MAX_DEPTH} levels deep: not read")
}

/// Refuse XML nested deeper than [`MAX_DEPTH`], or that can't be tokenized.
pub(crate) fn check_depth(xml: &str) -> Result<(), String> {
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
        // Siblings, empty elements, quoted `>`, comments and declarations don't nest.
        let flat = format!(
            "<?xml version=\"1.0\"?><!-- <a> > --><OR>{}</OR>",
            "<E K=\"a>b\" V='x' />".repeat(10_000)
        );
        assert!(check_depth(&flat).is_ok());
    }

    /// The review's bypasses of a hand-written scanner: a `>` inside CDATA or a
    /// processing instruction, followed by a closing tag that is really text.
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
