//! `.cos` files: Capture One's per-image settings, XML of the shape
//!
//! ```xml
//! <IMG>
//!   <E K="Width" V="6000" />            image facts (EXIF, size, UUID)
//!   <VAR Engine="1300">                 one per variant
//!     <E K="UUID" V="…" />              variant facts
//!     <DL> <E K="Rotation" V="-90" /> … </DL>   default layer
//!     <AL> <E K="Exposure" V="0.5" /> … </AL>   adjustment layer (edits)
//!     <PHS> <PH> … </PH> </PHS>         process history (ignored)
//!   </VAR>
//! </IMG>
//! ```

use crate::model::{Keys, Settings};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CosVariant {
    /// The variant's `UUID` entry.
    pub uuid: Option<String>,
    pub settings: Settings,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CosFile {
    /// Image-level entries (`Width`, `Exp_Date`, `UUID`, …).
    pub image: Keys,
    pub variants: Vec<CosVariant>,
}

/// Parse a `.cos` document. Unknown elements are ignored; a document that
/// isn't well-formed XML, is nested too deep (see `xml.rs`), or whose root
/// isn't `IMG` is an error (`Err(text)`).
pub fn parse_cos(xml: &str) -> Result<CosFile, String> {
    crate::xml::check_depth(xml)?;
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    if root.tag_name().name() != "IMG" {
        return Err(format!(
            "root element is <{}>, not <IMG>",
            root.tag_name().name()
        ));
    }
    let mut out = CosFile::default();
    for child in root.children().filter(roxmltree::Node::is_element) {
        match child.tag_name().name() {
            "E" => entry(child, &mut out.image),
            "VAR" => {
                let mut info = Keys::new();
                let mut settings = Settings::default();
                for n in child.children().filter(roxmltree::Node::is_element) {
                    match n.tag_name().name() {
                        "E" => entry(n, &mut info),
                        "DL" => entries(n, &mut settings.dl),
                        "AL" => entries(n, &mut settings.al),
                        _ => {}
                    }
                }
                out.variants.push(CosVariant {
                    uuid: info.remove("UUID").filter(|u| !u.trim().is_empty()),
                    settings,
                });
            }
            _ => {}
        }
    }
    Ok(out)
}

fn entries(layer: roxmltree::Node, into: &mut Keys) {
    for n in layer.children().filter(|n| n.has_tag_name("E")) {
        entry(n, into);
    }
}

fn entry(n: roxmltree::Node, into: &mut Keys) {
    if let (Some(k), Some(v)) = (n.attribute("K"), n.attribute("V")) {
        into.insert(k.to_owned(), v.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_nesting_is_refused_before_parsing() {
        let xml = format!("<IMG>{}</IMG>", crate::xml::tests::nested(300_000, "VAR"));
        let err = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || parse_cos(&xml).unwrap_err())
            .unwrap()
            .join()
            .expect("no stack overflow");
        assert!(err.contains("Nested"), "{err}");
    }
    use crate::fixtures;

    #[test]
    fn parses_image_entries_and_both_layers() {
        let f = parse_cos(fixtures::COS_A).unwrap();
        assert_eq!(f.image["Width"], "6000");
        assert_eq!(f.image["UUID"], "IMG-A");
        assert_eq!(f.variants.len(), 2);
        let v = &f.variants[0];
        assert_eq!(v.uuid.as_deref(), Some("VAR-A1"));
        assert_eq!(v.settings.dl["WhiteBalance"], "2.4;1;1.6");
        assert_eq!(v.settings.al["Exposure"], "0.5");
        assert_eq!(v.settings.get("Basic_Rating"), Some("4"));
        assert_eq!(
            v.settings.al["Content_Keywords"], "Beach & Sun;Family",
            "entities decoded"
        );
        assert!(
            !v.settings.al.contains_key("URL"),
            "process history is not a layer"
        );
        assert!(
            !v.settings.al.contains_key("ProductID"),
            "variant facts are not settings"
        );
    }

    #[test]
    fn several_variants() {
        let f = parse_cos(fixtures::COS_A).unwrap();
        let v = &f.variants[1];
        assert_eq!(v.uuid.as_deref(), Some("VAR-A2"));
        assert_eq!(v.settings.al.len(), 1);
        assert_eq!(v.settings.get("Color_Tag_Index"), Some("4"));
        assert_eq!(v.settings.get("Basic_Rating"), Some("0"));
        let c = parse_cos(fixtures::COS_C).unwrap();
        assert_eq!(c.variants.len(), 1);
    }

    #[test]
    fn blank_uuid_is_none_and_incomplete_entries_are_skipped() {
        let f = parse_cos(
            r#"<?xml version="1.0"?><IMG><E K="Only"/><VAR><E K="UUID" V=" "/><DL><E V="1"/><X/></DL></VAR><Other/></IMG>"#,
        )
        .unwrap();
        assert!(f.image.is_empty());
        assert_eq!(f.variants[0].uuid, None);
        assert!(f.variants[0].settings.dl.is_empty());
    }

    #[test]
    fn rejects_malformed_or_foreign_xml() {
        assert!(parse_cos("<IMG><E K='a' V='1'>").is_err());
        let e = parse_cos("<catalog/>").unwrap_err();
        assert!(e.contains("<catalog>"), "{e}");
        assert!(parse_cos("").is_err());
    }
}
