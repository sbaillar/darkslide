//! XMP packets: the sidecar next to a RAW (`IMG_0001.xmp`), the packet inside
//! a DNG, a preset, a catalog's per-photo packet. Read with roxmltree after a
//! depth check; Camera Raw settings are in the `crs:` namespace, written as
//! attributes of `rdf:Description` or as child elements (lists as
//! `rdf:Seq`/`rdf:Bag`, language alternatives as `rdf:Alt`).

use std::collections::{BTreeMap, HashSet};

use roxmltree::{Document, Node, ParsingOptions};

use crate::limits::{MAX_KEYWORDS, MAX_XMP_BYTES, cap_text, check_depth};
use crate::model::{Develop, Setting, parse_num};

const NS_CRS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";
const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
const NS_TIFF: &str = "http://ns.adobe.com/tiff/1.0/";
const NS_EXIF: &str = "http://ns.adobe.com/exif/1.0/";
const NS_LR: &str = "http://ns.adobe.com/lightroom/1.0/";
const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// `crs:` properties that describe a preset, not a setting.
const PRESET_META: [&str; 13] = [
    "Name",
    "ShortName",
    "SortName",
    "Group",
    "UUID",
    "PresetType",
    "Cluster",
    "SupportsAmount",
    "SupportsColor",
    "SupportsMonochrome",
    "SupportsHighDynamicRange",
    "SupportsNormalDynamicRange",
    "SupportsSceneReferred",
];

/// What an XMP packet says that the import uses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XmpData {
    /// `xmp:Rating`; -1 means rejected.
    pub rating: Option<i64>,
    pub label: Option<String>,
    /// `tiff:Orientation`, 1-8.
    pub orientation: Option<u8>,
    pub keywords: Option<Vec<String>>,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub copyright: Option<String>,
    pub capture_time_ms: Option<i64>,
    /// The `crs:` settings, preset descriptors removed.
    pub develop: Develop,
    /// Preset descriptors (`crs:Name`, `crs:Group`, `crs:UUID`).
    pub preset_name: Option<String>,
    pub preset_group: Option<String>,
    pub preset_uuid: Option<String>,
}

/// Parse one XMP packet (the text between or including the `xpacket` markers).
pub fn parse_xmp(text: &str) -> Result<XmpData, String> {
    if text.len() as u64 > MAX_XMP_BYTES {
        return Err(format!("More than {MAX_XMP_BYTES} bytes: not read"));
    }
    let text = text.trim_start_matches('\u{feff}');
    let text = match text.find("</x:xmpmeta>") {
        Some(end) => &text[..end + "</x:xmpmeta>".len()],
        None => text,
    };
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    check_depth(text)?;
    let doc = Document::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 2_000_000,
            entity_resolver: None,
        },
    )
    .map_err(|e| format!("Invalid XMP: {e}"))?;
    let mut out = XmpData::default();
    let mut hierarchical: Option<Vec<String>> = None;
    let descriptions = doc.descendants().filter(|n| {
        n.is_element()
            && n.tag_name().name() == "Description"
            && n.tag_name().namespace() == Some(NS_RDF)
    });
    for d in descriptions {
        for (ns, name, value) in props(d) {
            match (ns.as_str(), name.as_str()) {
                (NS_CRS, "Name") => out.preset_name = value.text().map(str::to_owned),
                (NS_CRS, "Group") => out.preset_group = value.text().map(str::to_owned),
                (NS_CRS, "UUID") => out.preset_uuid = value.text().map(str::to_owned),
                (NS_CRS, n) if PRESET_META.contains(&n) => {}
                (NS_CRS, n) => {
                    out.develop.insert(n.to_owned(), value);
                }
                (NS_XMP, "Rating") => {
                    out.rating = value.num().map(|n| n.round().clamp(-1.0, 5.0) as i64);
                }
                (NS_XMP, "Label") => out.label = value.text().map(|s| capped(s.trim())),
                (NS_TIFF, "Orientation") => {
                    out.orientation = value
                        .num()
                        .filter(|n| (1.0..=8.0).contains(n))
                        .map(|n| n as u8);
                }
                (NS_DC, "subject") => out.keywords = Some(texts(&value)),
                (NS_LR, "hierarchicalSubject") => hierarchical = Some(texts(&value)),
                (NS_DC, "title") => out.title = value.text().map(capped),
                (NS_DC, "description") => out.caption = value.text().map(capped),
                (NS_DC, "rights") => out.copyright = value.text().map(capped),
                (NS_EXIF, "DateTimeOriginal") => {
                    out.capture_time_ms = value.text().and_then(parse_time_ms);
                }
                (NS_XMP, "CreateDate") if out.capture_time_ms.is_none() => {
                    out.capture_time_ms = value.text().and_then(parse_time_ms);
                }
                _ => {}
            }
        }
    }
    if out.keywords.is_none() {
        out.keywords = hierarchical.map(|h| {
            let mut leaves: Vec<String> = Vec::new();
            let mut seen: HashSet<&str> = HashSet::new();
            for l in h.iter().filter_map(|p| p.rsplit('|').next()).map(str::trim) {
                if leaves.len() >= MAX_KEYWORDS {
                    break;
                }
                if !l.is_empty() && seen.insert(l) {
                    leaves.push(l.to_owned());
                }
            }
            leaves
        });
    }
    Ok(out)
}

/// A colour label's name as a Luxtable colour (Bridge's default set, Select, Second,
/// Approved, Review and To Do, counts as red, yellow, green, blue and purple), or the
/// text back when it has none ("Custom"). Blank text is neither.
pub(crate) fn label_tag(text: &str) -> (Option<lt_core::ColorTag>, Option<String>) {
    use lt_core::ColorTag;
    let t = text.trim();
    let tag = match t.to_ascii_lowercase().as_str() {
        "" => return (None, None),
        "red" | "select" => ColorTag::Red,
        "orange" => ColorTag::Orange,
        "yellow" | "second" => ColorTag::Yellow,
        "green" | "approved" => ColorTag::Green,
        "blue" | "review" => ColorTag::Blue,
        "pink" => ColorTag::Pink,
        "purple" | "to do" => ColorTag::Purple,
        _ => return (None, Some(t.to_owned())),
    };
    (Some(tag), None)
}

fn capped(s: &str) -> String {
    cap_text(s.to_owned()).0
}

fn texts(s: &Setting) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for t in s
        .list()
        .unwrap_or_default()
        .iter()
        .filter_map(Setting::text)
    {
        let t = t.trim();
        if out.len() >= MAX_KEYWORDS {
            break;
        }
        if !t.is_empty() && seen.insert(t) {
            out.push(capped(t));
        }
    }
    out
}

/// `(namespace, local name, value)` of the attributes and child elements of a node.
fn props(d: Node) -> Vec<(String, String, Setting)> {
    let mut out = Vec::new();
    for a in d.attributes() {
        if let Some(ns) = a.namespace() {
            out.push((
                ns.to_owned(),
                a.name().to_owned(),
                Setting::Text(a.value().to_owned()),
            ));
        }
    }
    for c in d.children().filter(Node::is_element) {
        if let Some(ns) = c.tag_name().namespace() {
            out.push((ns.to_owned(), c.tag_name().name().to_owned(), to_setting(c)));
        }
    }
    out
}

fn is_rdf(n: Node, name: &str) -> bool {
    n.is_element() && n.tag_name().namespace() == Some(NS_RDF) && n.tag_name().name() == name
}

fn own_text(n: Node) -> String {
    n.children()
        .filter(Node::is_text)
        .filter_map(|t| t.text())
        .collect::<String>()
        .trim()
        .to_owned()
}

/// An element's value: text, a list (`rdf:Seq`/`rdf:Bag`), the default
/// language of an `rdf:Alt`, or a table of its attributes and children.
fn to_setting(n: Node) -> Setting {
    let kids: Vec<Node> = n.children().filter(Node::is_element).collect();
    if let [only] = kids.as_slice() {
        if is_rdf(*only, "Alt") {
            let items: Vec<Node> = only.children().filter(|c| is_rdf(*c, "li")).collect();
            let chosen = items
                .iter()
                .find(|li| {
                    li.attribute(("http://www.w3.org/XML/1998/namespace", "lang"))
                        == Some("x-default")
                })
                .or_else(|| items.first());
            return chosen.map_or_else(|| Setting::Text(String::new()), |li| to_setting(*li));
        }
        if is_rdf(*only, "Seq") || is_rdf(*only, "Bag") {
            return Setting::List(
                only.children()
                    .filter(|c| is_rdf(*c, "li"))
                    .map(to_setting)
                    .collect(),
            );
        }
    }
    let attrs: Vec<_> = n
        .attributes()
        .filter(|a| {
            a.namespace() != Some("http://www.w3.org/XML/1998/namespace")
                && a.namespace() != Some("http://www.w3.org/2000/xmlns/")
        })
        .filter(|a| a.namespace() != Some(NS_RDF))
        .collect();
    if kids.is_empty() && attrs.is_empty() {
        return Setting::Text(own_text(n));
    }
    let mut table: BTreeMap<String, Setting> = BTreeMap::new();
    for a in attrs {
        table.insert(a.name().to_owned(), Setting::Text(a.value().to_owned()));
    }
    for k in kids {
        table.insert(k.tag_name().name().to_owned(), to_setting(k));
    }
    Setting::Table(table)
}

/// `2020-05-01T12:00:00[.123][Z|+hh:mm]` (or date only) as ms since the Unix
/// epoch, the clock read as UTC (zones are ignored: the camera's clock is what
/// Luxtable records too).
pub(crate) fn parse_time_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, t),
        None => (s, ""),
    };
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || !(1..=9999).contains(&y) {
        return None;
    }
    let time = time.trim_end_matches('Z');
    let time = time.split(['+', '-']).next().unwrap_or_default();
    let mut t = time.split(':');
    let h: i64 = t
        .next()
        .filter(|x| !x.is_empty())
        .map_or(Some(0), |x| x.parse().ok())?;
    let mi: i64 = t.next().map_or(Some(0), |x| x.parse().ok())?;
    let sec = t.next().map_or(Some(0.0), parse_num)?;
    if !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0.0..61.0).contains(&sec) {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60) as f64 * 1000.0 + sec * 1000.0).round() as i64)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lt_core::ColorTag;

    /// A sidecar as Lightroom writes it: attributes plus Seq/Bag/Alt children.
    pub(crate) const SIDECAR: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:tiff="http://ns.adobe.com/tiff/1.0/"
    xmlns:exif="http://ns.adobe.com/exif/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
   xmp:Rating="4"
   xmp:Label="Red"
   tiff:Orientation="6"
   exif:DateTimeOriginal="2020-05-01T12:00:00.5"
   crs:Version="15.0"
   crs:Exposure2012="+0.50"
   crs:CropLeft="0.1"
   crs:HasCrop="True">
   <dc:title><rdf:Alt><rdf:li xml:lang="fr">Titre</rdf:li><rdf:li xml:lang="x-default">A title</rdf:li></rdf:Alt></dc:title>
   <dc:description><rdf:Alt><rdf:li xml:lang="x-default">A caption</rdf:li></rdf:Alt></dc:description>
   <dc:rights><rdf:Alt><rdf:li xml:lang="x-default">(c) someone</rdf:li></rdf:Alt></dc:rights>
   <dc:subject><rdf:Bag><rdf:li>sea</rdf:li><rdf:li>boat</rdf:li><rdf:li>sea</rdf:li></rdf:Bag></dc:subject>
   <lr:hierarchicalSubject><rdf:Bag><rdf:li>Places|Coast|sea</rdf:li></rdf:Bag></lr:hierarchicalSubject>
   <crs:ToneCurvePV2012><rdf:Seq><rdf:li>0, 0</rdf:li><rdf:li>255, 255</rdf:li></rdf:Seq></crs:ToneCurvePV2012>
   <crs:MaskGroupBasedCorrections><rdf:Seq><rdf:li crs:What="Correction" crs:LocalExposure2012="0.5">
     <crs:CorrectionMasks><rdf:Seq><rdf:li crs:What="Mask/Gradient" crs:ZeroX="0.1"/></rdf:Seq></crs:CorrectionMasks>
   </rdf:li></rdf:Seq></crs:MaskGroupBasedCorrections>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;

    #[test]
    fn reads_a_lightroom_sidecar() {
        let x = parse_xmp(SIDECAR).unwrap();
        assert_eq!(x.rating, Some(4));
        assert_eq!(x.label.as_deref(), Some("Red"));
        assert_eq!(x.orientation, Some(6));
        assert_eq!(x.title.as_deref(), Some("A title"));
        assert_eq!(x.caption.as_deref(), Some("A caption"));
        assert_eq!(x.copyright.as_deref(), Some("(c) someone"));
        assert_eq!(x.keywords, Some(vec!["sea".to_owned(), "boat".to_owned()]));
        assert_eq!(x.capture_time_ms, Some(1_588_334_400_500));
        assert_eq!(x.develop["Exposure2012"], Setting::Text("+0.50".into()));
        assert_eq!(x.develop["Exposure2012"].num(), Some(0.5));
        assert_eq!(x.develop["HasCrop"].flag(), Some(true));
        assert_eq!(
            x.develop["ToneCurvePV2012"],
            Setting::List(vec![
                Setting::Text("0, 0".into()),
                Setting::Text("255, 255".into())
            ])
        );
        let masks = x.develop["MaskGroupBasedCorrections"].list().unwrap();
        let first = masks[0].table().unwrap();
        assert_eq!(first["What"].text(), Some("Correction"));
        assert_eq!(first["LocalExposure2012"].num(), Some(0.5));
        let cm = first["CorrectionMasks"].list().unwrap();
        assert_eq!(cm[0].table().unwrap()["What"].text(), Some("Mask/Gradient"));
        assert!(x.preset_name.is_none());
    }

    #[test]
    fn hierarchical_keywords_and_preset_descriptors() {
        let x = parse_xmp(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
              xmlns:lr="http://ns.adobe.com/lightroom/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">
             <rdf:RDF><rdf:Description crs:UUID="ABC" crs:PresetType="Normal" crs:SupportsAmount="True" crs:Contrast2012="+10">
              <crs:Name><rdf:Alt><rdf:li xml:lang="x-default">Punchy</rdf:li></rdf:Alt></crs:Name>
              <crs:Group><rdf:Alt><rdf:li xml:lang="x-default">Mine</rdf:li></rdf:Alt></crs:Group>
              <lr:hierarchicalSubject><rdf:Bag><rdf:li>A|B|leaf</rdf:li><rdf:li>A|B|leaf</rdf:li><rdf:li>solo</rdf:li></rdf:Bag></lr:hierarchicalSubject>
             </rdf:Description></rdf:RDF></x:xmpmeta>"#,
        )
        .unwrap();
        assert_eq!(x.preset_name.as_deref(), Some("Punchy"));
        assert_eq!(x.preset_group.as_deref(), Some("Mine"));
        assert_eq!(x.preset_uuid.as_deref(), Some("ABC"));
        assert_eq!(x.develop.keys().collect::<Vec<_>>(), ["Contrast2012"]);
        assert_eq!(x.keywords, Some(vec!["leaf".to_owned(), "solo".to_owned()]));
    }

    #[test]
    fn odd_values_are_ignored_not_fatal() {
        let x = parse_xmp(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
              xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:tiff="http://ns.adobe.com/tiff/1.0/"
              xmlns:exif="http://ns.adobe.com/exif/1.0/">
             <rdf:RDF><rdf:Description xmp:Rating="x" tiff:Orientation="99" exif:DateTimeOriginal="garbage" xmp:CreateDate="2021-01-02"/>
             <rdf:Description xmp:Rating="-1"/></rdf:RDF></x:xmpmeta>"#,
        )
        .unwrap();
        assert_eq!(x.rating, Some(-1));
        assert_eq!(x.orientation, None);
        assert_eq!(x.capture_time_ms, Some(1_609_545_600_000));
        assert_eq!(
            parse_xmp("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>").unwrap(),
            XmpData::default()
        );
    }

    #[test]
    fn hostile_packets_are_refused() {
        let deep = format!("<r>{}</r>", crate::limits::tests::nested(300, "a"));
        assert!(parse_xmp(&deep).unwrap_err().contains("Nested"));
        assert!(parse_xmp("<a><b></a>").unwrap_err().contains("Invalid"));
        assert!(parse_xmp("not xml").is_err());
        let dtd = "<!DOCTYPE a [<!ENTITY x \"y\">]><a>&x;</a>";
        assert!(parse_xmp(dtd).is_err());
        let huge = " ".repeat(MAX_XMP_BYTES as usize + 1);
        assert!(parse_xmp(&huge).unwrap_err().contains("bytes"));
    }

    #[test]
    fn bridge_default_labels_have_colours() {
        for (name, tag) in [
            ("Select", ColorTag::Red),
            ("second", ColorTag::Yellow),
            ("Approved", ColorTag::Green),
            ("Review", ColorTag::Blue),
            ("To Do", ColorTag::Purple),
        ] {
            assert_eq!(label_tag(name), (Some(tag), None), "{name}");
        }
    }

    /// 100k keywords were quadratic and unbounded; titles are cut.
    #[test]
    fn keyword_lists_and_text_are_bounded() {
        let mut items = String::new();
        for i in 0..100_000 {
            items.push_str("<rdf:li>k");
            items.push_str(&i.to_string());
            items.push_str("</rdf:li>");
        }
        let big = "a".repeat(200_000);
        let text = format!(
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:dc="http://purl.org/dc/elements/1.1/"><rdf:Description><dc:subject><rdf:Bag>{items}</rdf:Bag></dc:subject><dc:title><rdf:Alt><rdf:li xml:lang="x-default">{big}</rdf:li></rdf:Alt></dc:title></rdf:Description></rdf:RDF></x:xmpmeta>"#
        );
        let started = std::time::Instant::now();
        let x = parse_xmp(&text).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
        assert_eq!(x.keywords.unwrap().len(), crate::limits::MAX_KEYWORDS);
        assert_eq!(x.title.unwrap().len(), crate::limits::MAX_TEXT);
    }

    #[test]
    fn labels() {
        use lt_core::ColorTag;
        assert_eq!(label_tag(" Purple "), (Some(ColorTag::Purple), None));
        assert_eq!(label_tag("orange"), (Some(ColorTag::Orange), None));
        assert_eq!(label_tag("Custom"), (None, Some("Custom".to_owned())));
        assert_eq!(label_tag("  "), (None, None));
    }

    #[test]
    fn times() {
        assert_eq!(parse_time_ms("1970-01-01T00:00:00"), Some(0));
        assert_eq!(
            parse_time_ms("2020-05-01T12:00:00Z"),
            Some(1_588_334_400_000)
        );
        assert_eq!(
            parse_time_ms("2020-05-01T12:00:00+02:00"),
            Some(1_588_334_400_000)
        );
        assert_eq!(parse_time_ms("2020-05-01 12:00"), Some(1_588_334_400_000));
        assert_eq!(parse_time_ms("2020-05-01"), Some(1_588_291_200_000));
        for bad in [
            "",
            "2020",
            "2020-13-01",
            "2020-05-01T25:00:00",
            "x-y-z",
            "2020-05-01T1:x",
        ] {
            assert_eq!(parse_time_ms(bad), None, "{bad}");
        }
    }
}
