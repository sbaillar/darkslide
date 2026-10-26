//! Smart-album criteria (`ZCOLLECTION.ZSMARTCRITERIA`):
//!
//! ```xml
//! <MatchOperator Kind="AND">
//!   <MatchOperator Kind="AND">
//!     <Condition Enabled="YES"><Key>IB_I_BASIC_RATING</Key><Operator>0</Operator><Criterion>5</Criterion></Condition>
//!   </MatchOperator>
//! </MatchOperator>
//! ```

/// One condition of a smart album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub key: String,
    /// C1's operator code (`0` = "is"; the other codes are not documented).
    pub operator: i64,
    pub criterion: String,
}

/// A parsed search: a tree of AND/OR groups. Disabled conditions are dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Criteria {
    All(Vec<Criteria>),
    Any(Vec<Criteria>),
    Condition(Condition),
    /// Something this parser doesn't understand (kept for the report).
    Unknown(String),
}

impl Criteria {
    /// The conditions if the tree only ANDs them (any nesting), else `None`.
    pub fn conjunction(&self) -> Option<Vec<&Condition>> {
        let mut out = Vec::new();
        self.collect_all(&mut out).then_some(out)
    }

    fn collect_all<'a>(&'a self, out: &mut Vec<&'a Condition>) -> bool {
        match self {
            Criteria::All(v) => v.iter().all(|c| c.collect_all(out)),
            Criteria::Any(v) if v.len() == 1 => v[0].collect_all(out),
            Criteria::Condition(c) => {
                out.push(c);
                true
            }
            Criteria::Any(_) | Criteria::Unknown(_) => false,
        }
    }
}

/// Parse a smart album's criteria text. Empty text is `None`.
pub fn parse_criteria(xml: &str) -> Option<Criteria> {
    if xml.trim().is_empty() {
        return None;
    }
    // `node` recurses, and so would the parser on a deep document.
    if let Err(e) = crate::xml::check_depth(xml) {
        return Some(Criteria::Unknown(format!("unreadable criteria: {e}")));
    }
    match roxmltree::Document::parse(xml) {
        Ok(doc) => Some(node(doc.root_element())),
        Err(e) => Some(Criteria::Unknown(format!("unreadable criteria: {e}"))),
    }
}

fn node(n: roxmltree::Node) -> Criteria {
    match n.tag_name().name() {
        "MatchOperator" => {
            let kids: Vec<Criteria> = n
                .children()
                .filter(roxmltree::Node::is_element)
                .filter(|c| !(c.has_tag_name("Condition") && c.attribute("Enabled") == Some("NO")))
                .map(node)
                .collect();
            match n.attribute("Kind") {
                Some(k) if k.eq_ignore_ascii_case("AND") => Criteria::All(kids),
                Some(k) if k.eq_ignore_ascii_case("OR") => Criteria::Any(kids),
                other => Criteria::Unknown(format!("match kind {other:?}")),
            }
        }
        "Condition" => {
            let text = |tag: &str| {
                n.children()
                    .find(|c| c.has_tag_name(tag))
                    .and_then(|c| c.text())
                    .unwrap_or_default()
                    .trim()
                    .to_owned()
            };
            match text("Operator").parse() {
                Ok(operator) => Criteria::Condition(Condition {
                    key: text("Key"),
                    operator,
                    criterion: text("Criterion"),
                }),
                Err(_) => Criteria::Unknown(format!("operator {:?}", text("Operator"))),
            }
        }
        other => Criteria::Unknown(format!("<{other}>")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_nesting_is_unknown_criteria_not_a_crash() {
        let xml = crate::xml::tests::nested(300_000, "MatchOperator");
        let c = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || parse_criteria(&xml))
            .unwrap()
            .join()
            .expect("no stack overflow");
        assert!(matches!(c, Some(Criteria::Unknown(m)) if m.contains("Nested")));
    }

    const FIVE_STARS: &str = r#"<?xml version="1.0"?><MatchOperator Kind="AND"><MatchOperator Kind="AND"><Condition Enabled="YES"><Key>IB_I_BASIC_RATING</Key><Operator>0</Operator><Criterion>5</Criterion></Condition></MatchOperator></MatchOperator>"#;

    #[test]
    fn nested_and_flattens_to_conditions() {
        let c = parse_criteria(FIVE_STARS).unwrap();
        let conds = c.conjunction().unwrap();
        assert_eq!(
            conds,
            vec![&Condition {
                key: "IB_I_BASIC_RATING".into(),
                operator: 0,
                criterion: "5".into()
            }]
        );
    }

    #[test]
    fn disabled_conditions_are_dropped_and_or_is_not_a_conjunction() {
        let xml = r#"<MatchOperator Kind="OR"><Condition Enabled="NO"><Key>A</Key><Operator>0</Operator><Criterion>1</Criterion></Condition><Condition><Key>B</Key><Operator>0</Operator><Criterion>2</Criterion></Condition><Condition><Key>C</Key><Operator>0</Operator><Criterion>3</Criterion></Condition></MatchOperator>"#;
        let c = parse_criteria(xml).unwrap();
        let Criteria::Any(kids) = &c else {
            panic!("{c:?}")
        };
        assert_eq!(kids.len(), 2);
        assert!(c.conjunction().is_none());
        // A single-branch OR is just that branch.
        let one = r#"<MatchOperator Kind="OR"><Condition><Key>B</Key><Operator>0</Operator><Criterion>2</Criterion></Condition></MatchOperator>"#;
        assert_eq!(parse_criteria(one).unwrap().conjunction().unwrap().len(), 1);
    }

    #[test]
    fn unknown_shapes_are_kept_not_guessed() {
        assert_eq!(parse_criteria("  "), None);
        assert!(matches!(parse_criteria("<x"), Some(Criteria::Unknown(_))));
        let c = parse_criteria(r#"<MatchOperator Kind="XOR"/>"#).unwrap();
        assert!(matches!(c, Criteria::Unknown(ref s) if s.contains("XOR")));
        let c = parse_criteria("<Weird/>").unwrap();
        assert!(c.conjunction().is_none());
        let c = parse_criteria(
            r#"<MatchOperator Kind="AND"><Condition><Key>K</Key><Operator>eq</Operator></Condition></MatchOperator>"#,
        )
        .unwrap();
        assert!(c.conjunction().is_none(), "{c:?}");
    }
}
