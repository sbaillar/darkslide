//! Keywords and IPTC fields in exports: an XMP packet (`dc:subject`, `dc:title`…)
//! for JPEG, PNG and TIFF, and an IPTC-IIM block (2:25 Keywords, 2:05 Object Name…,
//! UTF-8) in a Photoshop APP13 segment for JPEG, so both XMP readers (Lightroom,
//! Capture One) and IPTC readers (Photos, Finder) see them. HEIC and the app's JPEG
//! encoder get them through the encoder hook.

use std::fmt::Write;

use ds_core::{IptcField, IptcMeta};

/// JPEG APP1 identifier of an XMP packet.
pub const XMP_JPEG_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

/// PNG iTXt keyword of an XMP packet.
pub const XMP_PNG_KEYWORD: &str = "XML:com.adobe.xmp";

/// TIFF tag of an XMP packet (`XMLPacket`, BYTE).
pub const TAG_XMP: u16 = 700;

/// IIM limits a keyword to 64 bytes.
const IIM_KEYWORD_MAX: usize = 64;

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // XML 1.0 forbids most control characters.
            c if c.is_control() && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// `<rdf:li>` items of a container.
fn items(out: &mut String, values: &[&str], lang: bool) {
    for v in values {
        out.push_str(if lang {
            "      <rdf:li xml:lang=\"x-default\">"
        } else {
            "      <rdf:li>"
        });
        out.push_str(&escape(v));
        out.push_str("</rdf:li>\n");
    }
}

/// An XMP packet carrying the `keywords` as `dc:subject` and the set IPTC fields
/// (Dublin Core, Photoshop, IPTC Core, XMP Rights); `None` when there is nothing.
pub fn packet(keywords: &[String], iptc: &IptcMeta) -> Option<Vec<u8>> {
    if keywords.is_empty() && iptc.is_empty() {
        return None;
    }
    let mut body = String::new();
    // Language alternatives, a sequence (creators), the keyword bag.
    let mut container = |name: &str, kind: &str, values: &[&str]| {
        let values: Vec<&str> = values.iter().copied().filter(|v| !v.is_empty()).collect();
        if values.is_empty() {
            return;
        }
        let _ = writeln!(body, "   <{name}>\n    <rdf:{kind}>");
        items(&mut body, &values, kind == "Alt");
        let _ = writeln!(body, "    </rdf:{kind}>\n   </{name}>");
    };
    container("dc:title", "Alt", &[&iptc.title]);
    container("dc:description", "Alt", &[&iptc.caption]);
    container("dc:creator", "Seq", &[&iptc.creator]);
    container("dc:rights", "Alt", &[&iptc.copyright]);
    let kws: Vec<&str> = keywords.iter().map(String::as_str).collect();
    container("dc:subject", "Bag", &kws);
    container("xmpRights:UsageTerms", "Alt", &[&iptc.usage_terms]);
    for (name, v) in [
        ("photoshop:Headline", &iptc.headline),
        ("photoshop:Credit", &iptc.credit),
        ("photoshop:City", &iptc.city),
        ("photoshop:State", &iptc.state),
        ("photoshop:Country", &iptc.country),
        ("photoshop:Instructions", &iptc.instructions),
        ("Iptc4xmpCore:Location", &iptc.sublocation),
    ] {
        if !v.is_empty() {
            let _ = writeln!(body, "   <{name}>{}</{name}>", escape(v));
        }
    }
    Some(
        format!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
             <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"\n    \
             xmlns:photoshop=\"http://ns.adobe.com/photoshop/1.0/\"\n    \
             xmlns:Iptc4xmpCore=\"http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/\"\n    \
             xmlns:xmpRights=\"http://ns.adobe.com/xap/1.0/rights/\">\n\
             {body}  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>"
        )
        .into_bytes(),
    )
}

/// One IIM dataset: 0x1C, record, dataset, 2-byte big-endian length, data.
fn dataset(out: &mut Vec<u8>, record: u8, id: u8, data: &[u8]) {
    out.extend([0x1C, record, id]);
    out.extend((data.len() as u16).to_be_bytes());
    out.extend(data);
}

/// Truncate to at most `max` bytes on a character boundary.
pub(crate) fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// IIM record 2 datasets of the IPTC fields, with their length limits.
/// Usage terms have no IIM dataset (XMP only).
fn iim_fields(iptc: &IptcMeta) -> [(u8, &str, usize); 11] {
    [
        (5, &iptc.title, 64),
        (40, &iptc.instructions, 256),
        (80, &iptc.creator, 32),
        (90, &iptc.city, 32),
        (92, &iptc.sublocation, 32),
        (95, &iptc.state, 32),
        (101, &iptc.country, 64),
        (105, &iptc.headline, 256),
        (110, &iptc.credit, 32),
        (116, &iptc.copyright, 128),
        (120, &iptc.caption, 2000),
    ]
}

/// Largest payload of one JPEG marker segment (the 2-byte length counts itself).
pub const JPEG_SEGMENT_MAX: usize = 65_533;

/// Bytes of an APP13 payload besides the IIM block: `Photoshop 3.0\0`, `8BIM`, the
/// resource id, the empty name, the size, and the even-length pad.
const APP13_OVERHEAD: usize = 14 + 4 + 2 + 2 + 4 + 1;

/// The JPEG APP13 payload (`Photoshop 3.0` image resource 0x0404) with the
/// keywords as IPTC-IIM 2:25 and the IPTC fields, declared UTF-8 (1:90); `None`
/// when there is nothing to write. See [`iptc_app13_fit`] for the size limit.
pub fn iptc_app13(keywords: &[String], iptc: &IptcMeta) -> Option<Vec<u8>> {
    iptc_app13_fit(keywords, iptc, JPEG_SEGMENT_MAX).map(|(a, _)| a)
}

/// [`iptc_app13`] in at most `max` bytes: a payload past one JPEG segment makes the
/// file undecodable, so trailing keywords that do not fit are left out. Also returns
/// how many were.
pub fn iptc_app13_fit(
    keywords: &[String],
    iptc: &IptcMeta,
    max: usize,
) -> Option<(Vec<u8>, usize)> {
    let fields: Vec<_> = iim_fields(iptc)
        .into_iter()
        .filter(|(_, v, _)| !v.is_empty())
        .collect();
    if keywords.is_empty() && fields.is_empty() {
        return None;
    }
    let mut head = Vec::new();
    dataset(&mut head, 1, 90, b"\x1B%G"); // coded character set: UTF-8
    dataset(&mut head, 2, 0, &[0, 4]); // record version 4
    for (id, v, max) in fields.iter().filter(|f| f.0 < 25) {
        dataset(&mut head, 2, *id, truncate(v, *max).as_bytes());
    }
    let mut tail = Vec::new();
    for (id, v, max) in fields.iter().filter(|f| f.0 > 25) {
        dataset(&mut tail, 2, *id, truncate(v, *max).as_bytes());
    }
    let room = max.saturating_sub(APP13_OVERHEAD + head.len() + tail.len());
    let mut kw = Vec::new();
    let mut kept = 0;
    for k in keywords {
        let before = kw.len();
        dataset(&mut kw, 2, 25, truncate(k, IIM_KEYWORD_MAX).as_bytes());
        if kw.len() > room {
            kw.truncate(before);
            break;
        }
        kept += 1;
    }
    let iim_len = head.len() + kw.len() + tail.len();
    let mut out = b"Photoshop 3.0\0".to_vec();
    out.extend(b"8BIM");
    out.extend(0x0404u16.to_be_bytes());
    out.extend([0, 0]); // empty Pascal name, padded to even
    out.extend((iim_len as u32).to_be_bytes());
    out.extend(head);
    out.extend(kw);
    out.extend(tail);
    if iim_len % 2 == 1 {
        out.push(0);
    }
    Some((out, keywords.len() - kept))
}

/// `iptc` with every text field cut to at most `cap` bytes.
fn cut_fields(iptc: &IptcMeta, cap: usize) -> IptcMeta {
    let mut out = iptc.clone();
    for f in IptcField::ALL {
        let v = out.get_mut(f);
        v.truncate(truncate(v, cap).len());
    }
    out
}

/// [`packet`] in at most `max` bytes (a JPEG segment holds one packet). Long text
/// fields are shortened first, to the largest cap that lets every keyword fit; only
/// then are trailing keywords left out. The second value says what was cut, if
/// anything, so the export can report it instead of losing it silently.
pub fn packet_fit(
    keywords: &[String],
    iptc: &IptcMeta,
    max: usize,
) -> Option<(Vec<u8>, Option<String>)> {
    let full = packet(keywords, iptc)?;
    if full.len() <= max {
        return Some((full, None));
    }
    let fits = |kw: &[String], m: &IptcMeta| packet(kw, m).filter(|p| p.len() <= max);
    let mut cap = 8192;
    let (fitted, kept, cap) = loop {
        let cut = cut_fields(iptc, cap);
        if let Some(p) = fits(keywords, &cut) {
            break (Some(p), keywords.len(), cap);
        }
        if cap == 0 {
            // No text left: drop keywords from the end until the rest fits.
            let (mut lo, mut hi) = (0, keywords.len());
            while lo < hi {
                let mid = (lo + hi).div_ceil(2);
                if packet(&keywords[..mid], &cut).is_none_or(|p| p.len() <= max) {
                    lo = mid;
                } else {
                    hi = mid - 1;
                }
            }
            break (packet(&keywords[..lo], &cut), lo, 0);
        }
        cap = if cap > 64 { cap / 2 } else { 0 };
    };
    let mut note = Vec::new();
    if cap
        < IptcField::ALL
            .iter()
            .map(|f| iptc.get(*f).len())
            .max()
            .unwrap_or(0)
    {
        note.push("long text shortened".to_owned());
    }
    if kept < keywords.len() {
        note.push(format!(
            "{} of {} keywords left out",
            keywords.len() - kept,
            keywords.len()
        ));
    }
    fitted.map(|p| (p, Some(format!("XMP: {}", note.join(", ")))))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn unescape(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&")
    }

    /// The `<rdf:li>` values inside `<name>…</name>`, or its plain text (a
    /// minimal reader for tests).
    pub(crate) fn xmp_values(packet: &[u8], name: &str) -> Vec<String> {
        let s = std::str::from_utf8(packet).unwrap();
        let Some((_, rest)) = s.split_once(&format!("<{name}>")) else {
            return Vec::new();
        };
        let inner = rest.split_once(&format!("</{name}>")).unwrap().0;
        if !inner.contains("<rdf:li") {
            return vec![unescape(inner)];
        }
        inner
            .split("<rdf:li")
            .skip(1)
            .map(|p| {
                unescape(
                    p.split_once('>')
                        .unwrap()
                        .1
                        .split("</rdf:li>")
                        .next()
                        .unwrap(),
                )
            })
            .collect()
    }

    /// The `dc:subject` items of a packet, unescaped.
    pub(crate) fn subjects(packet: &[u8]) -> Vec<String> {
        xmp_values(packet, "dc:subject")
    }

    /// The record 2 datasets of an APP13 payload, in order.
    pub(crate) fn iim_datasets(app13: &[u8]) -> Vec<(u8, String)> {
        assert!(app13.starts_with(b"Photoshop 3.0\08BIM\x04\x04\0\0"));
        let len = u32::from_be_bytes(app13[22..26].try_into().unwrap()) as usize;
        let mut iim = &app13[26..26 + len];
        let mut out = Vec::new();
        while !iim.is_empty() {
            assert_eq!(iim[0], 0x1C);
            let n = u16::from_be_bytes([iim[3], iim[4]]) as usize;
            if iim[1] == 2 && iim[2] != 0 {
                out.push((iim[2], String::from_utf8(iim[5..5 + n].to_vec()).unwrap()));
            }
            iim = &iim[5 + n..];
        }
        out
    }

    /// The 2:25 keywords of an APP13 payload.
    pub(crate) fn iim_keywords(app13: &[u8]) -> Vec<String> {
        iim_datasets(app13)
            .into_iter()
            .filter(|d| d.0 == 25)
            .map(|d| d.1)
            .collect()
    }

    pub(crate) fn iptc() -> IptcMeta {
        IptcMeta {
            title: "Morning <Surf>".into(),
            caption: "Surfers & friends at dawn".into(),
            creator: "Alexa".into(),
            copyright: "© 2026 Alexa".into(),
            credit: "Darkslide".into(),
            headline: "Big swell".into(),
            city: "Husavik".into(),
            state: "Nordurland".into(),
            country: "Iceland".into(),
            sublocation: "Skjalfandi Bay".into(),
            instructions: "No crops".into(),
            usage_terms: "Editorial use only".into(),
        }
    }

    #[test]
    fn keywords_round_trip_through_xmp_and_iim() {
        let kw: Vec<String> = [
            "beach",
            "Crete & <friends>",
            "\"quoted\"",
            "café",
            "x\u{1}y",
        ]
        .map(String::from)
        .to_vec();
        let none = IptcMeta::default();
        let p = packet(&kw, &none).unwrap();
        assert!(
            std::str::from_utf8(&p)
                .unwrap()
                .starts_with("<?xpacket begin")
        );
        assert_eq!(
            subjects(&p),
            ["beach", "Crete & <friends>", "\"quoted\"", "café", "xy"]
        );
        assert!(
            xmp_values(&p, "dc:title").is_empty(),
            "unset fields are left out"
        );
        let a = iptc_app13(&kw, &none).unwrap();
        assert_eq!(iim_keywords(&a), kw);
        assert_eq!(a.len() % 2, 0, "the resource is padded to an even length");
        // A long keyword is cut at 64 bytes on a character boundary.
        let long = vec!["é".repeat(40)];
        assert_eq!(
            iim_keywords(&iptc_app13(&long, &none).unwrap()),
            ["é".repeat(32)]
        );
        assert!(packet(&[], &none).is_none() && iptc_app13(&[], &none).is_none());
    }

    #[test]
    fn iptc_fields_round_trip_through_xmp_and_iim() {
        let m = iptc();
        let p = packet(&[], &m).unwrap();
        for (name, want) in [
            ("dc:title", "Morning <Surf>"),
            ("dc:description", "Surfers & friends at dawn"),
            ("dc:creator", "Alexa"),
            ("dc:rights", "© 2026 Alexa"),
            ("xmpRights:UsageTerms", "Editorial use only"),
            ("photoshop:Headline", "Big swell"),
            ("photoshop:Credit", "Darkslide"),
            ("photoshop:City", "Husavik"),
            ("photoshop:State", "Nordurland"),
            ("photoshop:Country", "Iceland"),
            ("photoshop:Instructions", "No crops"),
            ("Iptc4xmpCore:Location", "Skjalfandi Bay"),
        ] {
            assert_eq!(xmp_values(&p, name), [want], "{name}");
        }
        assert_eq!(subjects(&p).len(), 0);
        let a = iptc_app13(&["k".into()], &m).unwrap();
        let ds = iim_datasets(&a);
        let ids: Vec<u8> = ds.iter().map(|d| d.0).collect();
        assert_eq!(
            ids,
            [5, 25, 40, 80, 90, 92, 95, 101, 105, 110, 116, 120],
            "datasets in number order, no usage terms"
        );
        assert_eq!(ds[0].1, "Morning <Surf>");
        assert_eq!(ds[11].1, "Surfers & friends at dawn");
        // Only a field: still a packet and a block.
        let one = IptcMeta {
            city: "Akureyri".into(),
            ..IptcMeta::default()
        };
        assert_eq!(
            xmp_values(&packet(&[], &one).unwrap(), "photoshop:City"),
            ["Akureyri"]
        );
        assert_eq!(
            iim_datasets(&iptc_app13(&[], &one).unwrap()),
            [(90, "Akureyri".into())]
        );
        // IIM length limits (the creator is 32 bytes), cut on a character boundary.
        let long = IptcMeta {
            creator: "x".repeat(40),
            city: "©".repeat(20),
            title: "東".repeat(30),
            ..IptcMeta::default()
        };
        let ds = iim_datasets(&iptc_app13(&[], &long).unwrap());
        assert_eq!(ds[0], (5, "東".repeat(21)), "64 bytes: 21 three-byte chars");
        assert_eq!(ds[1].1.len(), 32);
        assert_eq!(ds[2], (90, "©".repeat(16)));
    }
}
