//! Where photos were taken, from their metadata (see `docs/FFI.md`).
//!
//! The Sony A9 records no position, so most photos have none. What is used
//! when present:
//! - **XMP/IPTC location fields** (`Iptc4xmpCore:Location`, `photoshop:City`,
//!   `photoshop:State`, `photoshop:Country`, the IPTC Extension
//!   `Sublocation`/`City`/`ProvinceState`/`CountryName`) from the photo's XMP
//!   sidecar (`name.xmp`, `name.ARW.xmp`, any case: Lightroom, Capture One,
//!   geotaggers) or, for JPEG/HEIF/TIFF/DNG/PNG, the XMP packet in the first
//!   256 KB of the file;
//! - **GPS**: EXIF (`lt_raw::read_metadata`), else XMP `exif:GPSLatitude`
//!   (the one order everywhere: a map geotag, then EXIF, then XMP). A void
//!   fix (`GPSStatus` `V`) or exactly 0, 0 is no position.
//!
//! Positions are kept but not named: naming them needs a reverse geocoder
//! (network) or a gazetteer, and neither ships with the app.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lt_session::PhotoPlace;

use super::{CancelToken, DsError, SessionHandle, lock, run_job};

/// Location fields, most specific first.
const FIELDS: &[&str] = &[
    "Iptc4xmpExt:Sublocation",
    "Iptc4xmpCore:Location",
    "Iptc4xmpExt:City",
    "photoshop:City",
    "Iptc4xmpExt:ProvinceState",
    "photoshop:State",
    "Iptc4xmpExt:CountryName",
    "photoshop:Country",
];

/// File types whose own XMP packet is read (RAWs keep theirs in sidecars).
const EMBEDDED_XMP: &[&str] = &["jpg", "jpeg", "heic", "heif", "tif", "tiff", "dng", "png"];

/// How much of a file is searched for its XMP packet.
const XMP_HEAD: u64 = 256 * 1024;

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        if let Some(c) = c {
            out.push(c);
            rest = &rest[end + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Every value of `field` in an XMP packet: as an attribute (`f="v"`), an
/// element (`<f>v</f>`), or a language alternative inside it.
pub(crate) fn values(xmp: &str, field: &str) -> Vec<String> {
    let mut out = Vec::new();
    for quote in ['"', '\''] {
        let attr = format!("{field}={quote}");
        for (i, _) in xmp.match_indices(&attr) {
            let rest = &xmp[i + attr.len()..];
            if let Some(end) = rest.find(quote) {
                out.push(unescape(&rest[..end]));
            }
        }
    }
    let open = format!("<{field}>");
    let close = format!("</{field}>");
    for (i, _) in xmp.match_indices(&open) {
        let rest = &xmp[i + open.len()..];
        if let Some(end) = rest.find(&close) {
            let inner = &rest[..end];
            // <rdf:Alt><rdf:li xml:lang="x-default">Sicily</rdf:li></rdf:Alt>: the
            // x-default item, else the first.
            let text = match inner.find("<rdf:li") {
                Some(first) => {
                    let li = inner
                        .find("<rdf:li xml:lang=\"x-default\"")
                        .unwrap_or(first);
                    inner[li..]
                        .split_once('>')
                        .and_then(|(_, v)| v.split_once("</rdf:li>"))
                        .map_or("", |(v, _)| v)
                }
                None => inner,
            };
            out.push(unescape(text));
        }
    }
    out.into_iter()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .collect()
}

/// XMP GPS ("21,13.5432N", "21,13,32.6N", "21.2257N"), decimal degrees.
fn xmp_coordinate(v: &str) -> Option<f64> {
    let v = v.trim();
    let hemi = v.chars().last()?.to_ascii_uppercase();
    let sign = match hemi {
        'N' | 'E' => 1.0,
        'S' | 'W' => -1.0,
        _ => return None,
    };
    let parts: Vec<f64> = v[..v.len() - 1]
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .ok()?;
    let deg = match parts.as_slice() {
        [d] => *d,
        [d, m] => d + m / 60.0,
        [d, m, s] => d + m / 60.0 + s / 3600.0,
        _ => return None,
    };
    let limit = if matches!(hemi, 'N' | 'S') {
        90.0
    } else {
        180.0
    };
    (deg.is_finite() && (0.0..=limit).contains(&deg)).then_some(sign * deg)
}

/// The place an XMP packet records.
pub(crate) fn xmp_place(xmp: &str) -> PhotoPlace {
    let mut names: Vec<String> = Vec::new();
    for f in FIELDS {
        for v in values(xmp, f) {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&v)) {
                names.push(v);
            }
        }
    }
    let void = values(xmp, "exif:GPSStatus")
        .first()
        .is_some_and(|v| v.eq_ignore_ascii_case("V"));
    let gps = values(xmp, "exif:GPSLatitude")
        .first()
        .and_then(|v| xmp_coordinate(v))
        .zip(
            values(xmp, "exif:GPSLongitude")
                .first()
                .and_then(|v| xmp_coordinate(v)),
        )
        .filter(|&(lat, lon)| !void && (lat != 0.0 || lon != 0.0));
    PhotoPlace {
        names,
        gps,
        stamp: String::new(),
    }
}

/// Directory listings, each read once per scan: lower-cased name → path.
#[derive(Debug, Default)]
pub(crate) struct Listings(HashMap<PathBuf, Vec<(String, PathBuf)>>);

impl Listings {
    fn of(&mut self, dir: &Path) -> &[(String, PathBuf)] {
        self.0.entry(dir.to_owned()).or_insert_with(|| {
            let mut v: Vec<(String, PathBuf)> = fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|e| (e.file_name().to_string_lossy().to_lowercase(), e.path()))
                        .filter(|(_, p)| {
                            p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        })
    }

    /// The photo's XMP sidecars: `name.xmp` and `name.ext.xmp`, any case.
    pub(crate) fn sidecars(&mut self, original: &Path) -> Vec<PathBuf> {
        if let Some(dir) = original.parent() {
            self.of(dir);
        }
        self.cached_sidecars(original)
    }

    /// Read the folders of `originals` now, so [`Self::cached_sidecars`] finds them
    /// (each folder is listed once, however many photos it holds).
    pub(crate) fn prefill<'a>(&mut self, originals: impl IntoIterator<Item = &'a Path>) {
        for p in originals {
            if let Some(dir) = p.parent() {
                self.of(dir);
            }
        }
    }

    /// [`Self::sidecars`] from folders already listed (none for any other folder).
    pub(crate) fn cached_sidecars(&self, original: &Path) -> Vec<PathBuf> {
        let (Some(dir), Some(name)) = (original.parent(), original.file_name()) else {
            return Vec::new();
        };
        let name = name.to_string_lossy().to_lowercase();
        let stem = original
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let wanted = [format!("{stem}.xmp"), format!("{name}.xmp")];
        self.0
            .get(dir)
            .into_iter()
            .flatten()
            .filter(|(n, _)| wanted.contains(n))
            .map(|(_, p)| p.clone())
            .collect()
    }

    /// The sidecars' names, sizes and modification times: what a read depends on.
    pub(crate) fn stamp(&mut self, original: &Path) -> String {
        self.sidecars(original)
            .iter()
            .filter_map(|p| {
                let md = fs::metadata(p).ok()?;
                let t = md
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_nanos();
                Some(format!(
                    "{}:{}:{t}",
                    p.file_name()?.to_string_lossy(),
                    md.len()
                ))
            })
            .collect::<Vec<_>>()
            .join(";")
    }
}

/// The XMP packet in the first [`XMP_HEAD`] bytes of a file.
fn embedded_xmp(path: &Path) -> Option<String> {
    let mut head = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(XMP_HEAD)
        .read_to_end(&mut head)
        .ok()?;
    let text = String::from_utf8_lossy(&head);
    let start = text.find("<x:xmpmeta")?;
    let end = text[start..].find("</x:xmpmeta>")? + start;
    Some(text[start..end].to_owned())
}

/// The XMP packets of the original at `path`: its sidecars', then its own.
pub(crate) fn xmp_packets(path: &Path, dirs: &mut Listings) -> Vec<String> {
    dirs.prefill([path]);
    let mut packets = sidecar_packets(path, dirs);
    packets.extend(own_packet(path));
    packets
}

/// The packets of the original's XMP sidecars (folders listed in `dirs`).
pub(crate) fn sidecar_packets(path: &Path, dirs: &Listings) -> Vec<String> {
    dirs.cached_sidecars(path)
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .collect()
}

/// The file's own XMP packet (JPEG, HEIF, TIFF, DNG, PNG; RAWs keep theirs in sidecars).
pub(crate) fn own_packet(path: &Path) -> Option<String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    EMBEDDED_XMP
        .contains(&ext.as_str())
        .then(|| embedded_xmp(path))
        .flatten()
}

/// Where the original at `path` was taken.
pub(crate) fn read_place(path: &Path, dirs: &mut Listings) -> PhotoPlace {
    let mut place = PhotoPlace {
        stamp: dirs.stamp(path),
        ..PhotoPlace::default()
    };
    // EXIF first, then XMP (the same order as the app and exports).
    place.gps = lt_raw::read_metadata(path)
        .ok()
        .and_then(|m| m.gps)
        .map(|g| (g.latitude, g.longitude));
    for xmp in &xmp_packets(path, dirs) {
        let p = xmp_place(xmp);
        for n in p.names {
            if !place.names.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                place.names.push(n);
            }
        }
        place.gps = place.gps.or(p.gps);
    }
    place
}

#[uniffi::export]
impl SessionHandle {
    /// Read the place of up to `limit` photos (XMP location fields, GPS):
    /// those never read, then those whose XMP sidecars changed since. Returns
    /// how many are left.
    pub async fn scan_locations(
        self: Arc<Self>,
        limit: u32,
        token: Arc<CancelToken>,
    ) -> Result<u64, DsError> {
        run_job(token, move |cancelled| {
            let (new, read) = {
                let s = lock(&self.session);
                (s.location_jobs()?, s.location_stamps()?)
            };
            let mut dirs = Listings::default();
            let stale: Vec<_> = read
                .into_iter()
                .filter(|(j, stamp)| dirs.stamp(&j.path) != *stamp)
                .map(|(j, _)| j)
                .collect();
            let todo: Vec<_> = new.into_iter().chain(stale).collect();
            let mut found = Vec::new();
            for j in todo.iter().take(limit as usize) {
                if cancelled() {
                    break;
                }
                found.push((j.key, read_place(&j.path, &mut dirs)));
            }
            let n = found.len();
            lock(&self.session).store_locations(&found)?;
            Ok((todo.len() - n) as u64)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIGHTROOM: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF>
 <rdf:Description rdf:about=""
    photoshop:City="Vik" photoshop:State="Sudurland" photoshop:Country="Iceland"
    Iptc4xmpCore:Location="Reynisfjara &amp; Dyrh&#xF3;laey&#x2019;s arch"
    exif:GPSLatitude="63,25.2N" exif:GPSLongitude="19,0.6W"/>
</rdf:RDF></x:xmpmeta>"#;

    const ELEMENTS: &str = "<x:xmpmeta><rdf:Description>
 <photoshop:City>Husavik</photoshop:City>
 <Iptc4xmpExt:LocationShown><rdf:Bag><rdf:li>
   <Iptc4xmpExt:CountryName><rdf:Alt><rdf:li xml:lang='x-default'>ISL</rdf:li></rdf:Alt></Iptc4xmpExt:CountryName>
   <Iptc4xmpExt:City>husavik</Iptc4xmpExt:City>
 </rdf:li></rdf:Bag></Iptc4xmpExt:LocationShown>
 <exif:GPSLatitude>20,52,30S</exif:GPSLatitude><exif:GPSLongitude>156.6833E</exif:GPSLongitude>
 <photoshop:State>   </photoshop:State>
</rdf:Description></x:xmpmeta>";

    #[test]
    fn location_fields_attributes_and_elements() {
        let p = xmp_place(LIGHTROOM);
        assert_eq!(
            p.names,
            [
                "Reynisfjara & Dyrhólaey’s arch",
                "Vik",
                "Sudurland",
                "Iceland"
            ]
        );
        let (lat, lon) = p.gps.unwrap();
        assert!((lat - 63.42).abs() < 1e-9 && (lon + 19.01).abs() < 1e-9);
        let p = xmp_place(ELEMENTS);
        // IPTC Extension first; the same name in another case once; blanks dropped.
        assert_eq!(p.names, ["husavik", "ISL"]);
        let (lat, lon) = p.gps.unwrap();
        assert!((lat + 20.875).abs() < 1e-9 && (lon - 156.6833).abs() < 1e-9);
        assert_eq!(xmp_place("").names.len(), 0);
        // No fix: void status, or exactly 0, 0.
        let void = LIGHTROOM.replace("exif:GPSLatitude", "exif:GPSStatus=\"V\" exif:GPSLatitude");
        assert_eq!(xmp_place(&void).gps, None);
        assert_eq!(
            xmp_place("<x:xmpmeta exif:GPSLatitude='0N' exif:GPSLongitude='0E'/>").gps,
            None
        );
        assert!(
            xmp_place("<x:xmpmeta exif:GPSLatitude='0N' exif:GPSLongitude='1E'/>")
                .gps
                .is_some()
        );
        assert_eq!(xmp_place("<photoshop:City>unclosed").names.len(), 0);
        assert_eq!(
            values("photoshop:City='Akureyri'", "photoshop:City"),
            ["Akureyri"]
        );
    }

    #[test]
    fn coordinates_and_entities() {
        assert_eq!(xmp_coordinate("10N"), Some(10.0));
        assert_eq!(xmp_coordinate("10,30W"), Some(-10.5));
        assert_eq!(xmp_coordinate("91N"), None);
        assert_eq!(xmp_coordinate("181,0E"), None);
        assert_eq!(xmp_coordinate("10,x,1N"), None);
        assert_eq!(xmp_coordinate("1,2,3,4N"), None);
        assert_eq!(xmp_coordinate("10Q"), None);
        assert_eq!(xmp_coordinate(""), None);
        assert_eq!(
            unescape("a &lt;b&gt; &quot;c&quot; &apos;d&apos; &#65;&#x42;"),
            "a <b> \"c\" 'd' AB"
        );
        assert_eq!(
            unescape("fish & chips &bogus; &#xZZ; &"),
            "fish & chips &bogus; &#xZZ; &"
        );
        assert_eq!(
            unescape("&averyveryverylongentity;"),
            "&averyveryverylongentity;"
        );
    }

    #[test]
    fn sidecars_embedded_packets_and_exif() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path().join("DSC1.ARW");
        fs::write(&raw, b"not a raw").unwrap();
        // No sidecar, unreadable RAW: nothing, quietly.
        assert_eq!(
            read_place(&raw, &mut Listings::default()),
            PhotoPlace::default()
        );
        fs::write(d.path().join("dsc1.XMP"), LIGHTROOM).unwrap();
        fs::write(d.path().join("DSC1.ARW.xmp"), ELEMENTS).unwrap();
        fs::write(d.path().join("other.xmp"), ELEMENTS).unwrap();
        let mut dirs = Listings::default();
        let p = read_place(&raw, &mut dirs);
        assert!(
            p.stamp.contains("DSC1.ARW.xmp:") && p.stamp.contains("dsc1.XMP:"),
            "{}",
            p.stamp
        );
        assert_eq!(
            p.names,
            [
                "husavik",
                "ISL",
                "Reynisfjara & Dyrhólaey’s arch",
                "Vik",
                "Sudurland",
                "Iceland"
            ]
        );
        assert!(p.gps.is_some());
        // A JPEG's own packet; a RAW's bytes are never searched.
        let jpg = d.path().join("phone.jpg");
        fs::write(&jpg, format!("\u{ff}\u{d8}junk{LIGHTROOM}junk")).unwrap();
        assert_eq!(read_place(&jpg, &mut dirs).names[1], "Vik");
        let arw = d.path().join("x.arw");
        fs::write(&arw, LIGHTROOM).unwrap();
        assert_eq!(read_place(&arw, &mut dirs).names.len(), 0);
        assert!(embedded_xmp(&d.path().join("missing.jpg")).is_none());
        assert_eq!(dirs.sidecars(Path::new("/")).len(), 0);
        assert_eq!(dirs.sidecars(&d.path().join("nowhere/x.ARW")).len(), 0);
        assert_eq!(dirs.stamp(&d.path().join("nowhere/x.ARW")), "");
        // A RAW's EXIF position wins over its XMP sidecar's.
        let g = crate::export::tests::tiny_raw_with_gps(d.path(), "G.ARW");
        fs::write(d.path().join("G.xmp"), LIGHTROOM).unwrap();
        let (lat, lon) = read_place(&g, &mut Listings::default()).gps.unwrap();
        assert!(
            (lat + 20.75).abs() < 1e-9 && (lon - 156.5).abs() < 1e-9,
            "{lat} {lon}"
        );
        // A real A9 file: no position recorded.
        if let Some(arw) = lt_testdata::sample_arw() {
            assert_eq!(read_place(&arw, &mut Listings::default()).gps, None);
        }
    }
}
