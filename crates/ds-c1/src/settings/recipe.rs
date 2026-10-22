//! Process recipes (`Recipes143/<name>.xml`): `<OR>` with `<E K V/>` entries.
//! Each maps onto one Darkslide export recipe; what has no equivalent is
//! reported per key. The code tables marked *inferred* in `docs/C1_FORMAT.md`
//! come from C1's default recipes, whose names say what they do.

use std::collections::BTreeMap;

use super::{Note, fingerprint, flat_entries, tokens};
use crate::model::parse_f64;

/// The Darkslide file format a recipe maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeFormat {
    Jpeg,
    /// Darkslide's TIFF (16-bit, uncompressed).
    Tiff16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecipeSize {
    Original,
    LongEdge(u32),
    ShortEdge(u32),
    Percent(f32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeOutputSpace {
    Srgb,
    DisplayP3,
    /// Another ICC profile, by its name (the file name without `.icm`/`.icc`,
    /// e.g. "Adobe RGB (1998)"); the app looks it up among installed profiles.
    Profile(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecipeWatermark {
    /// Text watermark (`None`: an image watermark).
    pub text: Option<String>,
    /// Image watermark file (absolute path).
    pub logo: Option<String>,
    /// Font family; empty = the system font.
    pub font: String,
    /// sRGB `0xRRGGBB`.
    pub color: u32,
    pub opacity: f32,
    /// Darkslide's 3×3 anchor (`topLeft` … `bottomRight`).
    pub anchor: &'static str,
}

/// One C1 recipe as a Darkslide export recipe.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // the app's recipe has these switches
pub struct RecipeImport {
    /// The file's name without `.xml` (the import's key for this recipe).
    pub file: String,
    pub name: String,
    /// C1's order in the recipe list.
    pub sort_index: Option<u32>,
    /// Ticked for export in C1 (the preferences' `enabledRecipeNames`).
    pub enabled: bool,
    pub format: RecipeFormat,
    pub quality: u8,
    pub size: RecipeSize,
    pub allow_upscale: bool,
    pub space: RecipeOutputSpace,
    /// Darkslide's output sharpening, 0…2.
    pub sharpen: f32,
    /// ds-template file name.
    pub file_name_template: String,
    /// `None`: the session's `Output/` folder.
    pub destination: Option<String>,
    pub subfolder: String,
    pub include_metadata: bool,
    pub strip_gps: bool,
    pub watermark: Option<RecipeWatermark>,
    pub notes: Vec<Note>,
    pub fingerprint: String,
}

/// C1's print resolution default, used when a recipe has none.
const DEFAULT_DPI: f64 = 300.0;

/// `OutputFileFormat`: 1 JPEG, 2 TIFF, 5 PSD, 8 JPEG `QuickProof` (from the
/// default recipes' names); anything else becomes JPEG, reported, so the
/// recipe's size, naming and folder still carry over.
fn format(e: &Entries, notes: &mut Vec<Note>) -> Result<RecipeFormat, String> {
    let code = e
        .num("OutputFileFormat")
        .ok_or("No output format (OutputFileFormat)")?;
    Ok(match code as i64 {
        1 => RecipeFormat::Jpeg,
        8 => {
            notes.push(Note::approx(
                "OutputFileFormat",
                "JPEG QuickProof: Darkslide always does a full develop",
            ));
            RecipeFormat::Jpeg
        }
        2 => {
            if e.num("tiffBits").is_some_and(|b| b < 16.0) {
                notes.push(Note::approx(
                    "tiffBits",
                    "TIFF 8-bit: Darkslide writes 16-bit TIFF",
                ));
            }
            if e.flag("tiffCompression", false) {
                notes.push(Note::dropped(
                    "tiffCompression",
                    "TIFF compression: Darkslide's TIFF is uncompressed",
                ));
            }
            RecipeFormat::Tiff16
        }
        5 => {
            notes.push(Note::approx(
                "OutputFileFormat",
                "PSD: Darkslide doesn't write PSD; imported as 16-bit TIFF",
            ));
            RecipeFormat::Tiff16
        }
        n => {
            notes.push(Note::dropped(
                "OutputFileFormat",
                format!("Output format code {n} isn't one Darkslide knows: imported as JPEG"),
            ));
            RecipeFormat::Jpeg
        }
    })
}

/// The file name template and the sub-folder.
fn naming(e: &Entries, notes: &mut Vec<Note>) -> Result<(String, String), String> {
    let (mut file_name, dropped) = tokens::template(
        &tokens::parse(e.get("fileName").unwrap_or_default())?,
        "{name}",
        true,
    );
    if !dropped.is_empty() {
        notes.push(Note::dropped(
            "fileName",
            format!(
                "File name: {} left out (no Darkslide token)",
                tokens::describe(&dropped)
            ),
        ));
    }
    if file_name.trim().is_empty() {
        file_name = "{name}".into();
    }
    let (subfolder, dropped) = tokens::template(
        &tokens::parse(e.get("destinationSubFolder").unwrap_or_default())?,
        "",
        false,
    );
    if !dropped.is_empty() {
        notes.push(Note::dropped(
            "destinationSubFolder",
            format!(
                "Sub-folder: {} left out (Darkslide's sub-folder is plain text)",
                tokens::describe(&dropped)
            ),
        ));
    }
    let subfolder = subfolder
        .split(['/', ':'])
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect::<Vec<_>>()
        .join("/");
    if e.get("destinationSubName")
        .is_some_and(|s| !s.trim().is_empty())
    {
        notes.push(Note::dropped(
            "destinationSubName",
            "Sub-name: not carried over",
        ));
    }
    Ok((file_name, subfolder))
}

/// `outputRootFolder`: 0 the session's Output folder, 3 a chosen folder
/// (`outputRootFolderPath`); other codes are unconfirmed.
fn destination(e: &Entries, notes: &mut Vec<Note>) -> Option<String> {
    match e.num("outputRootFolder").map(|v| v as i64) {
        None | Some(0) => None,
        Some(3) => {
            let path = e
                .get("outputRootFolderPath")
                .map(str::trim)
                .unwrap_or_default();
            if path.starts_with('/') {
                Some(path.to_owned())
            } else {
                notes.push(Note::dropped(
                    "outputRootFolderPath",
                    format!("Output folder \u{201c}{path}\u{201d} isn't a full path: exports go to the session's Output folder"),
                ));
                None
            }
        }
        Some(n) => {
            notes.push(Note::dropped(
                "outputRootFolder",
                format!("Output location (code {n}): exports go to the session's Output folder"),
            ));
            None
        }
    }
}

/// (include metadata, strip GPS). Darkslide has one switch for what C1
/// splits into camera data, copyright, keywords and other IPTC fields.
fn metadata(e: &Entries, notes: &mut Vec<Note>) -> (bool, bool) {
    let include = e.flag("MetadataIncludeCameraData", true);
    let strip_gps = !e.flag("MetadataIncludeGPS", true);
    let differing: Vec<&str> = [
        ("MetadataIncludeCopyright", "copyright"),
        ("MetadataIncludeKeywords", "keywords"),
        ("MetadataIncludeAllOther", "other IPTC fields"),
    ]
    .into_iter()
    .filter(|(k, _)| e.num(k).is_some() && e.flag(k, true) != include)
    .map(|(_, label)| label)
    .collect();
    if !differing.is_empty() {
        notes.push(Note::approx(
            "Metadata",
            format!(
                "Metadata: Capture One {} the {} separately; Darkslide's \"Include metadata\" ({}) covers them",
                if include { "leaves out" } else { "keeps" },
                differing.join(", "),
                if include { "on" } else { "off" }
            ),
        ));
    }
    if e.get("MetadataSelectedLibrariesWhitelist")
        .is_some_and(|s| !s.trim().is_empty())
    {
        notes.push(Note::dropped(
            "MetadataSelectedLibrariesWhitelist",
            "Keyword library filter: not carried over",
        ));
    }
    (include, strip_gps)
}

/// A recipe's entries, with typed reads.
struct Entries(BTreeMap<String, String>);

impl Entries {
    fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str)
    }

    fn num(&self, k: &str) -> Option<f64> {
        self.get(k).and_then(parse_f64)
    }

    fn flag(&self, k: &str, default: bool) -> bool {
        self.num(k).map_or(default, |v| v != 0.0)
    }
}

pub(crate) fn parse(xml: &str, file: &str) -> Result<RecipeImport, String> {
    let e = Entries(flat_entries(xml, &["OR"])?.into_iter().collect());
    let mut notes = Vec::new();
    let name = e
        .get("name")
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(file)
        .to_owned();
    let format = format(&e, &mut notes)?;
    let quality = e
        .num("jpegQuality")
        .map_or(90, |q| q.round().clamp(1.0, 100.0) as u8);
    let dpi = match (e.num("resolution"), e.num("resolutionUnit")) {
        (Some(r), Some(u)) if r > 0.0 && (u - 2.0).abs() < 1e-9 => r * 2.54,
        (Some(r), _) if r > 0.0 => r,
        _ => DEFAULT_DPI,
    };
    let size = size(&e.0, dpi, &mut notes);
    // C1 enlarges unless "Don't upscale" is ticked.
    let allow_upscale = size != RecipeSize::Original && !e.flag("neverUpscale", false);
    let space = output_space(
        e.get("outputProfile").unwrap_or_default().trim(),
        &mut notes,
    );
    let sharpen = sharpening(&e.0, &mut notes);
    let (file_name_template, subfolder) = naming(&e, &mut notes)?;
    let destination = destination(&e, &mut notes);
    let (include_metadata, strip_gps) = metadata(&e, &mut notes);
    let watermark = watermark(&e.0, &mut notes);
    for (key, default, text) in [
        (
            "ProcessedFileOverwriteAction",
            0.0,
            "Existing files: Darkslide never overwrites, it picks a free name",
        ),
        (
            "cropExportMethod",
            0.0,
            "Crop export method: not carried over",
        ),
        ("tileDimension", -1.0, "Tiling: not carried over"),
        ("skipThumbnail", 0.0, "Skip thumbnail: not carried over"),
    ] {
        if e.num(key).is_some_and(|v| (v - default).abs() > 1e-9) {
            notes.push(Note::dropped(key, text));
        }
    }
    Ok(RecipeImport {
        file: file.to_owned(),
        name,
        sort_index: e.num("sortIndex").filter(|v| *v >= 0.0).map(|v| v as u32),
        enabled: false,
        format,
        quality,
        size,
        allow_upscale,
        space,
        sharpen,
        file_name_template,
        destination,
        subfolder,
        include_metadata,
        strip_gps,
        watermark,
        notes,
        fingerprint: fingerprint(xml.as_bytes()),
    })
}

/// `key:value;key:value` (C1's scale values, e.g. `length:2048;unit:0`).
fn fields(v: &str) -> BTreeMap<&str, f64> {
    v.split(';')
        .filter_map(|p| {
            let (k, v) = p.split_once(':')?;
            Some((k.trim(), parse_f64(v)?))
        })
        .collect()
}

/// Pixels for a length in C1's units (0 px, 2 in, 3 cm, 4 mm; 3 is confirmed
/// by the A3 recipe, 2 and 4 are inferred). `None`: a unit Darkslide can't use.
fn pixels(v: f64, unit: f64, dpi: f64) -> Option<f64> {
    match unit as i64 {
        0 => Some(v),
        2 => Some(v * dpi),
        3 => Some(v / 2.54 * dpi),
        4 => Some(v / 25.4 * dpi),
        _ => None,
    }
}

/// C1's scale method (`scaleMethod`: 0 fixed, 1 width, 2 height, 3 dimensions,
/// 4 width × height, 5 long edge, 6 short edge; 0, 1, 3 and 5 are seen in
/// the default recipes, the rest inferred) → Darkslide's edge sizing.
fn size(e: &BTreeMap<String, String>, dpi: f64, notes: &mut Vec<Note>) -> RecipeSize {
    let method = e
        .get("scaleMethod")
        .and_then(|v| parse_f64(v))
        .unwrap_or(0.0) as i64;
    let scale = |key: &str| e.get(key).map(|v| fields(v)).unwrap_or_default();
    let mut edge = |key: &str, dims: &[&str], label: &str, long: bool, exact: bool| {
        let f = scale(key);
        let unit = f.get("unit").copied().unwrap_or(0.0);
        let values: Option<Vec<f64>> = dims
            .iter()
            .map(|d| f.get(d).and_then(|v| pixels(*v, unit, dpi)))
            .collect();
        let Some(px) = values
            .and_then(|v| v.into_iter().reduce(f64::max))
            .filter(|p| *p >= 1.0)
        else {
            notes.push(Note::dropped(
                "scaleMethod",
                format!(
                    "Size ({label}): its unit isn't one Darkslide can use; exported at full size"
                ),
            ));
            return RecipeSize::Original;
        };
        let px = px.round().min(65_500.0) as u32;
        if unit as i64 != 0 {
            notes.push(Note::approx(
                "scaleMethod",
                format!("Size: {label} converted to {px} px at {dpi:.0} dpi"),
            ));
        }
        if !exact {
            notes.push(Note::approx(
                "scaleMethod",
                format!(
                    "Size ({label}): Darkslide sizes by edge, so this becomes the {} edge {px} px",
                    if long { "long" } else { "short" }
                ),
            ));
        }
        if long {
            RecipeSize::LongEdge(px)
        } else {
            RecipeSize::ShortEdge(px)
        }
    };
    match method {
        0 => {
            let amount = scale("fixedScale").get("amount").copied().unwrap_or(1.0);
            if amount <= 0.0 || (amount - 1.0).abs() < 1e-6 {
                RecipeSize::Original
            } else {
                RecipeSize::Percent((amount * 100.0) as f32)
            }
        }
        1 => edge("widthScale", &["width"], "width", true, false),
        2 => edge("heightScale", &["height"], "height", false, false),
        3 => edge(
            "boundingBoxScale",
            &["width", "height"],
            "dimensions",
            true,
            false,
        ),
        4 => edge(
            "widthByHeightScale",
            &["width", "height"],
            "width × height",
            true,
            false,
        ),
        5 => edge("longEdgeScale", &["length"], "long edge", true, true),
        6 => edge("shortEdgeScale", &["length"], "short edge", false, true),
        n => {
            notes.push(Note::dropped(
                "scaleMethod",
                format!("Scale method {n}: not one Darkslide knows; exported at full size"),
            ));
            RecipeSize::Original
        }
    }
}

fn output_space(profile: &str, notes: &mut Vec<Note>) -> RecipeOutputSpace {
    let lower = profile.to_ascii_lowercase();
    if profile.is_empty() {
        notes.push(Note::approx(
            "outputProfile",
            "No output profile: Darkslide exports sRGB",
        ));
        RecipeOutputSpace::Srgb
    } else if lower.contains("srgb") {
        RecipeOutputSpace::Srgb
    } else if lower.contains("display p3") {
        RecipeOutputSpace::DisplayP3
    } else {
        let name = [".icm", ".icc"]
            .iter()
            .find_map(|ext| {
                lower
                    .ends_with(ext)
                    .then(|| &profile[..profile.len() - ext.len()])
            })
            .unwrap_or(profile);
        RecipeOutputSpace::Profile(name.trim().to_owned())
    }
}

/// Output sharpening is never carried over: what `SharpeningOutputControl`'s
/// codes mean is **unconfirmed**. The real data contradicts reading them from
/// the recipe names (code 2 sits on the full-size and archival recipes, 0 on
/// the web ones), so the import exports unsharpened and says so.
fn sharpening(e: &BTreeMap<String, String>, notes: &mut Vec<Note>) -> f32 {
    let num = |k: &str| e.get(k).and_then(|v| parse_f64(v));
    let mode = num("SharpeningOutputControl").unwrap_or(0.0);
    let amount = num("SharpeningOutputAmount").unwrap_or(0.0);
    if mode != 0.0 || amount > 0.0 {
        notes.push(Note::dropped(
            "SharpeningOutputControl",
            format!(
                "Capture One output sharpening (mode {mode:.0}, amount {amount:.0}) not carried over: its settings are unconfirmed, so Darkslide exports without output sharpening"
            ),
        ));
    }
    0.0
}

/// `red:1;green:0.5;blue:0;` → `0xFF8000`.
fn color(v: &str) -> u32 {
    let f = fields(v);
    let c = |k: &str| (f.get(k).copied().unwrap_or(1.0).clamp(0.0, 1.0) * 255.0).round() as u32;
    (c("red") << 16) | (c("green") << 8) | c("blue")
}

/// C1's placement (0…1 across, 0…1 down; the vertical direction is
/// **unconfirmed**) → Darkslide's 3×3 anchor.
fn anchor(h: f64, v: f64) -> &'static str {
    let bin = |x: f64| usize::from(x > 1.0 / 3.0) + usize::from(x > 2.0 / 3.0);
    [
        ["topLeft", "top", "topRight"],
        ["left", "center", "right"],
        ["bottomLeft", "bottom", "bottomRight"],
    ][bin(v)][bin(h)]
}

/// `WatermarkType`: 0 none; text and image are told apart by which of
/// `WatermarkText` / `WatermarkImageFilePath` is set (the codes are unconfirmed).
fn watermark(e: &BTreeMap<String, String>, notes: &mut Vec<Note>) -> Option<RecipeWatermark> {
    let get = |k: &str| e.get(k).map(|s| s.trim()).filter(|s| !s.is_empty());
    let num = |k: &str, d: f64| e.get(k).and_then(|v| parse_f64(v)).unwrap_or(d);
    if num("WatermarkType", 0.0) == 0.0 {
        return None;
    }
    let font = get("WatermarkFontName")
        .filter(|f| !f.starts_with('.'))
        .unwrap_or_default()
        .to_owned();
    if let Some(path) = get("WatermarkImageFilePath") {
        notes.push(Note::approx(
            "WatermarkImageScale",
            "Watermark image size: Darkslide sizes the logo as a share of the width",
        ));
        return Some(RecipeWatermark {
            text: None,
            logo: Some(path.to_owned()),
            font,
            color: 0xFF_FFFF,
            opacity: num("WatermarkImageOpacity", 1.0).clamp(0.0, 1.0) as f32,
            anchor: anchor(
                num("WatermarkHImagePlacement", 0.5),
                num("WatermarkVImagePlacement", 0.5),
            ),
        });
    }
    if let Some(text) = get("WatermarkText") {
        notes.push(Note::approx(
            "WatermarkFontSize",
            "Watermark text size: Darkslide sizes the text as a share of the short edge",
        ));
        return Some(RecipeWatermark {
            text: Some(text.to_owned()),
            logo: None,
            font,
            color: e.get("WatermarkColor").map_or(0xFF_FFFF, |c| color(c)),
            opacity: num("WatermarkTextOpacity", 1.0).clamp(0.0, 1.0) as f32,
            anchor: anchor(
                num("WatermarkHTextPlacement", 0.5),
                num("WatermarkVTextPlacement", 0.5),
            ),
        });
    }
    notes.push(Note::dropped(
        "WatermarkType",
        "Watermark: neither text nor an image file is set",
    ));
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use std::fmt::Write as _;

    use super::*;

    /// A recipe document from `(key, value)` pairs.
    pub(crate) fn doc(pairs: &[(&str, &str)]) -> String {
        let mut s = String::from("<?xml version=\"1.0\"?>\n<OR>\n");
        for (k, v) in pairs {
            let v = v
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;");
            let _ = writeln!(s, "\t<E K=\"{k}\" V=\"{v}\" />");
        }
        s + "</OR>\n"
    }

    const IMAGE_NAME: &str = r#"<TokenName version="2"><Token Type="1" /></TokenName>"#;

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            ("name", "Web"),
            ("OutputFileFormat", "1"),
            ("jpegQuality", "80"),
            ("fileName", IMAGE_NAME),
            ("destinationSubFolder", r#"<TokenName Version="2" />"#),
            ("scaleMethod", "5"),
            ("longEdgeScale", "length:2048;unit:0"),
            ("outputProfile", "sRGB Color Space Profile.icm"),
            ("outputRootFolder", "0"),
            ("MetadataIncludeCameraData", "1"),
            ("MetadataIncludeCopyright", "1"),
            ("MetadataIncludeKeywords", "1"),
            ("MetadataIncludeAllOther", "1"),
            ("MetadataIncludeGPS", "0"),
            ("sortIndex", "1"),
            ("SharpeningOutputControl", "0"),
            ("WatermarkType", "0"),
        ]
    }

    fn with(changes: &[(&'static str, &'static str)]) -> RecipeImport {
        let mut pairs = base();
        for (k, v) in changes {
            pairs.retain(|(pk, _)| pk != k);
            pairs.push((k, v));
        }
        parse(&doc(&pairs), "file").unwrap()
    }

    fn texts(r: &RecipeImport) -> Vec<&str> {
        r.notes.iter().map(|n| n.text.as_str()).collect()
    }

    #[test]
    fn web_jpeg_maps_exactly() {
        let r = with(&[]);
        assert_eq!(r.name, "Web");
        assert_eq!(r.file, "file");
        assert_eq!(r.format, RecipeFormat::Jpeg);
        assert_eq!(r.quality, 80);
        assert_eq!(r.size, RecipeSize::LongEdge(2048));
        assert!(r.allow_upscale, "C1 enlarges unless told not to");
        assert_eq!(r.space, RecipeOutputSpace::Srgb);
        assert_eq!(r.sharpen, 0.0);
        assert_eq!(r.file_name_template, "{name}");
        assert_eq!(r.subfolder, "");
        assert_eq!(r.destination, None);
        assert!(r.include_metadata);
        assert!(r.strip_gps);
        assert_eq!(r.watermark, None);
        assert_eq!(r.sort_index, Some(1));
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        assert_eq!(r.fingerprint.len(), 16);
    }

    #[test]
    fn formats() {
        let r = with(&[
            ("OutputFileFormat", "2"),
            ("tiffBits", "8"),
            ("tiffCompression", "1"),
        ]);
        assert_eq!(r.format, RecipeFormat::Tiff16);
        assert!(texts(&r).iter().any(|t| t.contains("TIFF 8-bit")));
        assert!(
            r.notes
                .iter()
                .any(|n| n.dropped && n.key == "tiffCompression")
        );
        let r = with(&[("OutputFileFormat", "2"), ("tiffBits", "16")]);
        assert_eq!(r.notes.len(), 0);
        let r = with(&[("OutputFileFormat", "5")]);
        assert_eq!(r.format, RecipeFormat::Tiff16);
        assert!(texts(&r)[0].contains("PSD"));
        let r = with(&[("OutputFileFormat", "8")]);
        assert_eq!(r.format, RecipeFormat::Jpeg);
        assert!(texts(&r)[0].contains("QuickProof"));
        let mut pairs = base();
        pairs.retain(|(k, _)| *k != "OutputFileFormat");
        assert!(
            parse(&doc(&pairs), "f")
                .unwrap_err()
                .contains("No output format")
        );
        pairs.push(("OutputFileFormat", "42"));
        let odd = parse(&doc(&pairs), "f").unwrap();
        assert_eq!(odd.format, RecipeFormat::Jpeg);
        assert!(odd.notes[0].dropped && odd.notes[0].text.contains("code 42"));
        assert!(parse("<SL/>", "f").unwrap_err().contains("<OR>"));
        // Quality is clamped; no quality → 90.
        assert_eq!(with(&[("jpegQuality", "250")]).quality, 100);
        let mut pairs = base();
        pairs.retain(|(k, _)| *k != "jpegQuality" && *k != "name");
        let r = parse(&doc(&pairs), "Stem").unwrap();
        assert_eq!((r.quality, r.name.as_str()), (90, "Stem"));
    }

    #[test]
    fn sizes() {
        let full = with(&[("scaleMethod", "0"), ("fixedScale", "amount:1;unit:1")]);
        assert_eq!(full.size, RecipeSize::Original);
        assert!(!full.allow_upscale);
        let half = with(&[("scaleMethod", "0"), ("fixedScale", "amount:0.5;unit:1")]);
        assert_eq!(half.size, RecipeSize::Percent(50.0));
        let w = with(&[("scaleMethod", "1"), ("widthScale", "width:1080;unit:0")]);
        assert_eq!(w.size, RecipeSize::LongEdge(1080));
        assert!(texts(&w)[0].contains("long edge 1080 px"));
        let h = with(&[("scaleMethod", "2"), ("heightScale", "height:800;unit:0")]);
        assert_eq!(h.size, RecipeSize::ShortEdge(800));
        // A3 at 300 dpi: 42 cm → 4961 px.
        let a3 = with(&[
            ("scaleMethod", "3"),
            (
                "boundingBoxScale",
                "height:29.7000007629395;width:42;unit:3",
            ),
            ("resolution", "300"),
            ("resolutionUnit", "1"),
        ]);
        assert_eq!(a3.size, RecipeSize::LongEdge(4961));
        assert!(texts(&a3).iter().any(|t| t.contains("4961 px at 300 dpi")));
        // Pixels per centimetre.
        let cm = with(&[
            ("scaleMethod", "5"),
            ("longEdgeScale", "length:10;unit:3"),
            ("resolution", "100"),
            ("resolutionUnit", "2"),
        ]);
        assert_eq!(cm.size, RecipeSize::LongEdge(1000));
        let inch = with(&[("scaleMethod", "6"), ("shortEdgeScale", "length:2;unit:2")]);
        assert_eq!(inch.size, RecipeSize::ShortEdge(600));
        assert_eq!(
            with(&[
                ("scaleMethod", "4"),
                ("widthByHeightScale", "height:254;width:127;unit:4")
            ])
            .size,
            RecipeSize::LongEdge(3000)
        );
        let bad_unit = with(&[("longEdgeScale", "length:5;unit:1")]);
        assert_eq!(bad_unit.size, RecipeSize::Original);
        assert!(bad_unit.notes[0].dropped);
        let odd = with(&[("scaleMethod", "9")]);
        assert_eq!(odd.size, RecipeSize::Original);
        assert!(texts(&odd)[0].contains("Scale method 9"));
        assert!(!with(&[("neverUpscale", "1")]).allow_upscale);
    }

    #[test]
    fn profiles_and_sharpening() {
        let a = with(&[("outputProfile", "Adobe RGB (1998).icm")]);
        assert_eq!(
            a.space,
            RecipeOutputSpace::Profile("Adobe RGB (1998)".into())
        );
        let p3 = with(&[("outputProfile", "Display P3.icc")]);
        assert_eq!(p3.space, RecipeOutputSpace::DisplayP3);
        let other = with(&[("outputProfile", "Printer Matte")]);
        assert_eq!(
            other.space,
            RecipeOutputSpace::Profile("Printer Matte".into())
        );
        let none = with(&[("outputProfile", "")]);
        assert_eq!(none.space, RecipeOutputSpace::Srgb);
        assert!(texts(&none)[0].contains("sRGB"));

        // Output sharpening's codes are unconfirmed: never carried over, always said.
        for (mode, amount) in [("2", "80"), ("1", "600"), ("3", "0"), ("0", "60")] {
            let r = with(&[
                ("SharpeningOutputControl", mode),
                ("SharpeningOutputAmount", amount),
            ]);
            assert_eq!(r.sharpen, 0.0);
            assert!(r.notes[0].dropped, "{mode}");
            let expected = format!("(mode {mode}, amount {amount}) not carried over");
            assert!(texts(&r)[0].contains(&expected), "{:?}", texts(&r));
        }
        let none = with(&[
            ("SharpeningOutputControl", "0"),
            ("SharpeningOutputAmount", "0"),
        ]);
        assert_eq!(none.notes.len(), 0);
    }

    #[test]
    fn naming_folders_and_metadata() {
        let r = with(&[
            (
                "fileName",
                r#"<TokenName version="2">IMG_<Token Type="1" /><Token Type="11" /></TokenName>"#,
            ),
            (
                "destinationSubFolder",
                r#"<TokenName Version="2">Web/../<Token Type="12" /> Large</TokenName>"#,
            ),
            ("destinationSubName", "x"),
            ("outputRootFolder", "3"),
            ("outputRootFolderPath", "/tmp/Exports"),
            ("MetadataIncludeKeywords", "0"),
            ("MetadataSelectedLibrariesWhitelist", "Lib"),
        ]);
        assert_eq!(r.file_name_template, "IMG_{name}");
        assert_eq!(r.subfolder, "Web/Large");
        assert_eq!(r.destination.as_deref(), Some("/tmp/Exports"));
        let t = texts(&r);
        assert!(
            t.iter().any(|t| t.contains("token type 11 left out")),
            "{t:?}"
        );
        assert!(t.iter().any(|t| t.contains("Sub-folder: token type 12")));
        assert!(t.iter().any(|t| t.contains("Sub-name")));
        assert!(t.iter().any(|t| t.contains("leaves out the keywords")));
        assert!(t.iter().any(|t| t.contains("Keyword library filter")));

        let r = with(&[(
            "fileName",
            r#"<TokenName version="2"><Token Type="7" /></TokenName>"#,
        )]);
        assert_eq!(r.file_name_template, "{name}", "an empty name falls back");
        let r = with(&[("outputRootFolder", "1")]);
        assert_eq!(r.destination, None);
        assert!(texts(&r)[0].contains("code 1"));
        let r = with(&[
            ("outputRootFolder", "3"),
            ("outputRootFolderPath", "relative"),
        ]);
        assert_eq!(r.destination, None);
        assert!(r.notes[0].dropped && texts(&r)[0].contains("\u{201c}relative\u{201d}"));
        let r = with(&[
            ("MetadataIncludeCameraData", "0"),
            ("MetadataIncludeGPS", "1"),
            ("MetadataIncludeKeywords", "0"),
            ("MetadataIncludeAllOther", "0"),
        ]);
        assert!(!r.include_metadata && !r.strip_gps);
        assert!(
            texts(&r)[0].contains("keeps the copyright"),
            "{:?}",
            texts(&r)
        );
    }

    #[test]
    fn invalid_tokens_fail_the_recipe() {
        let mut pairs = base();
        pairs.retain(|(k, _)| *k != "fileName");
        pairs.push(("fileName", "<TokenName>"));
        assert!(
            parse(&doc(&pairs), "f")
                .unwrap_err()
                .contains("Invalid name tokens")
        );
    }

    #[test]
    fn other_keys_reported_when_not_default() {
        let r = with(&[
            ("ProcessedFileOverwriteAction", "1"),
            ("cropExportMethod", "2"),
            ("tileDimension", "-1"),
            ("skipThumbnail", "1"),
        ]);
        let keys: Vec<&str> = r.notes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "ProcessedFileOverwriteAction",
                "cropExportMethod",
                "skipThumbnail"
            ]
        );
        assert!(r.notes.iter().all(|n| n.dropped));
    }

    #[test]
    fn watermarks() {
        let text = with(&[
            ("WatermarkType", "1"),
            ("WatermarkText", "© Me"),
            ("WatermarkColor", "red:1;green:0.5;blue:0;"),
            ("WatermarkTextOpacity", "0.25"),
            ("WatermarkFontName", "Helvetica"),
            ("WatermarkHTextPlacement", "0.9"),
            ("WatermarkVTextPlacement", "0.9"),
        ]);
        let w = text.watermark.unwrap();
        assert_eq!(w.text.as_deref(), Some("© Me"));
        assert_eq!(w.color, 0xFF_8000);
        assert_eq!(w.opacity, 0.25);
        assert_eq!(w.font, "Helvetica");
        assert_eq!(w.anchor, "bottomRight");
        let logo = with(&[
            ("WatermarkType", "2"),
            ("WatermarkImageFilePath", "/tmp/logo.png"),
            ("WatermarkFontName", ".AppleSystemUIFont"),
            ("WatermarkHImagePlacement", "0"),
            ("WatermarkVImagePlacement", "0"),
        ])
        .watermark
        .unwrap();
        assert_eq!(logo.logo.as_deref(), Some("/tmp/logo.png"));
        assert_eq!(
            (logo.anchor, logo.font.as_str(), logo.opacity),
            ("topLeft", "", 1.0)
        );
        let empty = with(&[("WatermarkType", "1")]);
        assert!(empty.watermark.is_none());
        assert!(empty.notes[0].dropped);
        assert_eq!(anchor(0.5, 0.5), "center");
        assert_eq!(anchor(0.0, 1.0), "bottomLeft");
    }
}
