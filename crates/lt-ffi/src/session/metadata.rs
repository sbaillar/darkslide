//! A photo's descriptive metadata (title, caption, credits, location, rights:
//! the IPTC Core fields of `lt_core::IptcMeta`): the record the app edits, and
//! the values a file already carries, read at import as defaults.
//!
//! Defaults come from the camera's EXIF `Artist`/`Copyright`, then (winning
//! where set) the XMP the photo has: its sidecar (`name.xmp`, `name.ARW.xmp`)
//! or, for JPEG/HEIF/TIFF/DNG/PNG, its own packet.

use std::path::Path;
use std::sync::Arc;

use lt_core::{IptcField, IptcMeta};

use super::places::{Listings, own_packet, sidecar_packets, values};
use super::{CancelToken, DsError, SessionHandle, lock, run_job};
use crate::raw::RawMetadata;

/// The editable IPTC fields of a photo; `""` = not set.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct PhotoMetadata {
    pub title: String,
    /// Caption / description.
    pub caption: String,
    pub creator: String,
    pub copyright: String,
    pub credit: String,
    pub headline: String,
    pub city: String,
    /// State / province.
    pub state: String,
    pub country: String,
    pub sublocation: String,
    pub instructions: String,
    pub usage_terms: String,
}

impl From<IptcMeta> for PhotoMetadata {
    fn from(m: IptcMeta) -> Self {
        Self {
            title: m.title,
            caption: m.caption,
            creator: m.creator,
            copyright: m.copyright,
            credit: m.credit,
            headline: m.headline,
            city: m.city,
            state: m.state,
            country: m.country,
            sublocation: m.sublocation,
            instructions: m.instructions,
            usage_terms: m.usage_terms,
        }
    }
}

impl From<PhotoMetadata> for IptcMeta {
    fn from(m: PhotoMetadata) -> Self {
        Self {
            title: m.title,
            caption: m.caption,
            creator: m.creator,
            copyright: m.copyright,
            credit: m.credit,
            headline: m.headline,
            city: m.city,
            state: m.state,
            country: m.country,
            sublocation: m.sublocation,
            instructions: m.instructions,
            usage_terms: m.usage_terms,
        }
    }
}

/// XMP properties of each field, preferred first.
const XMP_FIELDS: [(IptcField, &[&str]); 12] = [
    (IptcField::Title, &["dc:title"]),
    (IptcField::Caption, &["dc:description"]),
    (IptcField::Creator, &["dc:creator"]),
    (IptcField::Copyright, &["dc:rights"]),
    (IptcField::Credit, &["photoshop:Credit"]),
    (IptcField::Headline, &["photoshop:Headline"]),
    (IptcField::City, &["photoshop:City", "Iptc4xmpExt:City"]),
    (
        IptcField::State,
        &["photoshop:State", "Iptc4xmpExt:ProvinceState"],
    ),
    (
        IptcField::Country,
        &["photoshop:Country", "Iptc4xmpExt:CountryName"],
    ),
    (
        IptcField::Sublocation,
        &["Iptc4xmpCore:Location", "Iptc4xmpExt:Sublocation"],
    ),
    (IptcField::Instructions, &["photoshop:Instructions"]),
    (IptcField::UsageTerms, &["xmpRights:UsageTerms"]),
];

/// The IPTC fields an XMP packet records (the first value of each).
pub(crate) fn xmp_iptc(xmp: &str) -> IptcMeta {
    let mut m = IptcMeta::default();
    for (field, names) in XMP_FIELDS {
        if let Some(v) = names.iter().find_map(|n| values(xmp, n).into_iter().next()) {
            *m.get_mut(field) = v;
        }
    }
    m
}

/// The camera's EXIF `Artist` and `Copyright` as creator and copyright.
pub(crate) fn credits(artist: Option<&str>, copyright: Option<&str>) -> IptcMeta {
    IptcMeta {
        creator: artist.unwrap_or_default().to_owned(),
        copyright: copyright.unwrap_or_default().to_owned(),
        ..IptcMeta::default()
    }
}

/// [`credits`] from a RAW's metadata read.
pub(crate) fn raw_credits(raw: &RawMetadata) -> IptcMeta {
    credits(raw.artist.as_deref(), raw.copyright.as_deref())
}

/// What a photo already says: the `camera` credits, overlaid by `file`'s own XMP
/// packet, then by the XMP sidecars beside `original` (a sidecar a DAM wrote wins
/// over a stale embedded packet). `original` is where the sidecars are (a card's
/// file while importing), `file` the copy; their folders must be listed in `dirs`
/// ([`Listings::prefill`]).
pub(crate) fn iptc_of(original: &Path, file: &Path, camera: IptcMeta, dirs: &Listings) -> IptcMeta {
    let mut m = camera;
    for xmp in own_packet(file)
        .into_iter()
        .chain(sidecar_packets(original, dirs))
    {
        m = m.overlaid(&xmp_iptc(&xmp));
    }
    m
}

#[uniffi::export]
impl SessionHandle {
    /// Replace the IPTC fields of `ids` (trimmed; undoable; never touches the edit).
    pub async fn set_metadata(
        self: Arc<Self>,
        ids: Vec<i64>,
        metadata: PhotoMetadata,
        token: Arc<CancelToken>,
    ) -> Result<(), DsError> {
        run_job(token, move |_| {
            self.check_folder()?;
            Ok(lock(&self.session).set_iptc(&Self::ids(&ids), &metadata.into())?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKET: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF>
        <rdf:Description photoshop:City="Akureyri" photoshop:Headline="Lava &amp; sea">
          <dc:title><rdf:Alt><rdf:li xml:lang="x-default">Flow</rdf:li></rdf:Alt></dc:title>
          <dc:creator><rdf:Seq><rdf:li>Kai</rdf:li><rdf:li>Lei</rdf:li></rdf:Seq></dc:creator>
          <Iptc4xmpExt:CountryName>Iceland</Iptc4xmpExt:CountryName>
          <xmpRights:UsageTerms><rdf:Alt><rdf:li xml:lang="x-default">Editorial</rdf:li></rdf:Alt></xmpRights:UsageTerms>
        </rdf:Description></rdf:RDF></x:xmpmeta>"#;

    #[test]
    fn xmp_fields_are_read() {
        let m = xmp_iptc(PACKET);
        assert_eq!(m.title, "Flow");
        assert_eq!(m.creator, "Kai", "the first creator");
        assert_eq!(m.city, "Akureyri");
        assert_eq!(m.headline, "Lava & sea");
        assert_eq!(m.country, "Iceland", "IPTC Extension fallback");
        assert_eq!(m.usage_terms, "Editorial");
        assert!(m.caption.is_empty() && m.copyright.is_empty());
        // The record converts both ways without loss.
        assert_eq!(IptcMeta::from(PhotoMetadata::from(m.clone())), m);
    }

    #[test]
    fn exif_credits_then_embedded_then_the_xmp_sidecar() {
        let d = tempfile::tempdir().unwrap();
        let raw_path = d.path().join("DSC1.ARW");
        std::fs::write(&raw_path, b"raw").unwrap();
        let exif = RawMetadata {
            artist: Some("Camera Owner".into()),
            copyright: Some("© Camera".into()),
            ..RawMetadata::default()
        };
        let read = |p: &Path| {
            let mut dirs = Listings::default();
            dirs.prefill([p]);
            iptc_of(p, p, raw_credits(&exif), &dirs)
        };
        let m = read(&raw_path);
        assert_eq!(
            (m.creator.as_str(), m.copyright.as_str()),
            ("Camera Owner", "© Camera")
        );
        std::fs::write(d.path().join("DSC1.xmp"), PACKET).unwrap();
        let m = read(&raw_path);
        assert_eq!(m.creator, "Kai", "XMP wins over EXIF");
        assert_eq!(m.copyright, "© Camera", "EXIF fills what XMP leaves out");
        assert_eq!(m.title, "Flow");
        // A folder not listed reads no sidecar (callers list each folder once).
        assert_eq!(
            iptc_of(
                &raw_path,
                &raw_path,
                IptcMeta::default(),
                &Listings::default()
            )
            .title,
            ""
        );

        // A DNG with its own packet and a sidecar: the sidecar wins (review L1).
        let dng = d.path().join("B.dng");
        std::fs::write(
            &dng,
            b"<x:xmpmeta><dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">Stale</rdf:li></rdf:Alt></dc:title>\
              <photoshop:City>Akureyri</photoshop:City></x:xmpmeta>",
        )
        .unwrap();
        assert_eq!(read(&dng).title, "Stale");
        std::fs::write(d.path().join("B.xmp"), PACKET).unwrap();
        let m = read(&dng);
        assert_eq!((m.title.as_str(), m.city.as_str()), ("Flow", "Akureyri"));
        // While importing, the sidecar is read beside the card's original.
        let card = tempfile::tempdir().unwrap();
        let src = card.path().join("DSC1.ARW");
        std::fs::write(&src, b"raw").unwrap();
        std::fs::write(card.path().join("DSC1.ARW.xmp"), PACKET).unwrap();
        let copy = d.path().join("copy.ARW");
        std::fs::write(&copy, b"raw").unwrap();
        let mut dirs = Listings::default();
        dirs.prefill([src.as_path()]);
        assert_eq!(
            iptc_of(&src, &copy, IptcMeta::default(), &dirs).title,
            "Flow"
        );
    }

    #[test]
    fn a_language_alternative_prefers_x_default() {
        let m = xmp_iptc(
            r#"<x:xmpmeta><dc:title><rdf:Alt><rdf:li xml:lang="de">Leuchtturm</rdf:li>
            <rdf:li xml:lang="x-default">Lighthouse</rdf:li></rdf:Alt></dc:title>
            <dc:description><rdf:Alt><rdf:li xml:lang="fr">Phare</rdf:li></rdf:Alt></dc:description></x:xmpmeta>"#,
        );
        assert_eq!(
            (m.title.as_str(), m.caption.as_str()),
            ("Lighthouse", "Phare")
        );
    }

    /// Review M2: opening a folder in place reads the photos' XMP, not only files
    /// that show up later.
    #[test]
    fn opening_a_folder_reads_the_photos_xmp() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path().join("Shoot");
        let h = super::super::people::tests::open(
            &folder,
            &[
                ("a.ARW", &b"a"[..]),
                ("a.xmp", PACKET.as_bytes()),
                ("b.ARW", b"bb"),
            ],
        );
        let s = lock(&h.session);
        let by_name = |n: &str| {
            let img = s
                .images()
                .unwrap()
                .into_iter()
                .find(|i| i.rel_path.as_str().ends_with(n))
                .unwrap();
            s.variant(s.variants(img.id).unwrap()[0]).unwrap().iptc
        };
        assert_eq!(by_name("a.ARW").title, "Flow");
        assert!(by_name("b.ARW").is_empty());
    }

    /// Review M3: a folder is listed once, however many photos are looked up in it.
    #[test]
    fn each_folder_is_listed_once() {
        let d = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..3).map(|i| d.path().join(format!("P{i}.ARW"))).collect();
        let mut dirs = Listings::default();
        dirs.prefill(paths.iter().map(std::path::PathBuf::as_path));
        // A sidecar written after the listing isn't seen: the folder wasn't read again.
        std::fs::write(d.path().join("P1.xmp"), PACKET).unwrap();
        dirs.prefill([paths[1].as_path()]);
        assert!(iptc_of(&paths[1], &paths[1], IptcMeta::default(), &dirs).is_empty());
        let mut fresh = Listings::default();
        fresh.prefill([paths[1].as_path()]);
        assert_eq!(
            iptc_of(&paths[1], &paths[1], IptcMeta::default(), &fresh).title,
            "Flow"
        );
    }
}
