//! A folder of XMP sidecars (`IMG_0001.xmp` beside `IMG_0001.ARW`), as Lightroom
//! ("Automatically write changes into XMP"), Camera Raw and Bridge write them,
//! plus the XMP packet embedded in DNG files that have no sidecar.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use ds_core::{PickState, Rating};

use crate::error::{IoAt, LrError, Result};
use crate::limits::MAX_XMP_BYTES;
use crate::model::{LrLibrary, LrPhoto, SourceKind};
use crate::xmp::{XmpData, label_tag, parse_xmp};

const RAW_EXTENSIONS: [&str; 24] = [
    "dng", "arw", "cr2", "cr3", "crw", "nef", "nrw", "raf", "orf", "rw2", "pef", "srw", "x3f",
    "3fr", "erf", "mef", "mrw", "dcr", "kdc", "iiq", "rwl", "srf", "sr2", "gpr",
];

fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Read every sidecar (and embedded DNG packet) under `folder`.
pub fn read_sidecars(folder: &Path, cancel: &dyn Fn() -> bool) -> Result<LrLibrary> {
    let meta = fs::metadata(folder).at(folder)?;
    if !meta.is_dir() {
        return Err(LrError::Xmp {
            path: folder.to_owned(),
            message: "not a folder".into(),
        });
    }
    let mut lib = LrLibrary {
        kind: SourceKind::Sidecars,
        origin: folder.to_owned(),
        id: None,
        written_by: None,
        photos: Vec::new(),
        collections: Vec::new(),
        warnings: Vec::new(),
    };
    let mut seen = 0usize;
    let mut stack = vec![(folder.to_owned(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if cancel() {
            return Err(LrError::Cancelled);
        }
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                lib.warnings
                    .push(format!("{}: {e} (skipped)", dir.display()));
                continue;
            }
        };
        let mut names: Vec<String> = Vec::new();
        for entry in rd.flatten() {
            seen += 1;
            if seen > ds_session::MAX_EXTERNAL_FILES {
                lib.warnings.push(format!(
                    "More than {} files: the rest of the folder was not read",
                    ds_session::MAX_EXTERNAL_FILES
                ));
                stack.clear();
                break;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if depth < ds_session::MAX_EXTERNAL_DEPTH && !name.starts_with('.') {
                    stack.push((entry.path(), depth + 1));
                }
            } else if ft.is_file() && !name.starts_with("._") {
                names.push(name);
            }
        }
        names.sort();
        read_dir_files(folder, &dir, &names, &mut lib, cancel)?;
    }
    lib.photos.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(lib)
}

fn read_dir_files(
    root: &Path,
    dir: &Path,
    names: &[String],
    lib: &mut LrLibrary,
    cancel: &dyn Fn() -> bool,
) -> Result<()> {
    let rel_key = |p: &Path| {
        let rel = p.strip_prefix(root).unwrap_or(p);
        format!("xmp:{}", rel.to_string_lossy())
    };
    let stem_of = |n: &str| {
        Path::new(n)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    // One pass over the names: the non-sidecar files by stem, and the stems with a sidecar.
    let mut siblings: HashMap<String, Vec<&String>> = HashMap::new();
    let mut with_sidecar: HashSet<String> = HashSet::new();
    for n in names {
        if ext_of(n) == "xmp" {
            with_sidecar.insert(stem_of(n));
        } else {
            siblings.entry(stem_of(n)).or_default().push(n);
        }
    }
    for (i, name) in names.iter().enumerate() {
        if i.is_multiple_of(64) && cancel() {
            return Err(LrError::Cancelled);
        }
        match ext_of(name).as_str() {
            "xmp" => {
                let path = dir.join(name);
                let original = siblings
                    .get(&stem_of(name))
                    .and_then(|s| {
                        s.iter()
                            .min_by_key(|n| !RAW_EXTENSIONS.contains(&ext_of(n).as_str()))
                    })
                    .map_or_else(|| path.with_extension(""), |n| dir.join(n));
                match read_text(&path).and_then(|t| parse_xmp(&t).map_err(|m| message(&path, m))) {
                    Ok(x) => push(lib, rel_key(&path), original, &x),
                    Err(e) => lib.warnings.push(format!("{e} (skipped)")),
                }
            }
            "dng" if !with_sidecar.contains(&stem_of(name)) => {
                let path = dir.join(name);
                match embedded_xmp(&path) {
                    Ok(Some(text)) => match parse_xmp(&text) {
                        Ok(x) => push(lib, rel_key(&path), path.clone(), &x),
                        Err(m) => lib
                            .warnings
                            .push(format!("{}: {m} (skipped)", path.display())),
                    },
                    Ok(None) => {}
                    Err(e) => lib.warnings.push(format!("{e} (skipped)")),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn message(path: &Path, message: String) -> LrError {
    LrError::Xmp {
        path: path.to_owned(),
        message,
    }
}

fn read_text(path: &Path) -> Result<String> {
    let f = File::open(path).at(path)?;
    let mut bytes = Vec::new();
    f.take(MAX_XMP_BYTES + 1).read_to_end(&mut bytes).at(path)?;
    if bytes.len() as u64 > MAX_XMP_BYTES {
        return Err(message(
            path,
            format!("More than {MAX_XMP_BYTES} bytes: not read"),
        ));
    }
    String::from_utf8(bytes).map_err(|_| message(path, "not UTF-8 text".into()))
}

fn push(lib: &mut LrLibrary, key: String, path: PathBuf, x: &XmpData) {
    let mut p = LrPhoto {
        key,
        path,
        capture_time_ms: x.capture_time_ms,
        orientation: x.orientation,
        keywords: x.keywords.clone(),
        title: x.title.clone(),
        caption: x.caption.clone(),
        copyright: x.copyright.clone(),
        ..LrPhoto::default()
    };
    match x.rating {
        Some(-1) => {
            p.rating = Some(Rating::default());
            p.pick = Some(PickState::Rejected);
        }
        Some(r) => p.rating = u8::try_from(r).ok().and_then(Rating::new),
        None => {}
    }
    if let Some(label) = &x.label {
        let (tag, other) = label_tag(label);
        p.color_tag = tag;
        p.unmapped_label = other;
    }
    if !x.develop.is_empty() {
        p.develop = Some(x.develop.clone());
    }
    let has_data = p.rating.is_some()
        || p.color_tag.is_some()
        || p.unmapped_label.is_some()
        || p.keywords.is_some()
        || p.title.is_some()
        || p.caption.is_some()
        || p.copyright.is_some()
        || p.develop.is_some();
    if has_data {
        lib.photos.push(p);
    }
}

/// Most IFD entries read from a TIFF's first directory.
const MAX_IFD_ENTRIES: u16 = 4096;

/// The XMP packet (TIFF tag 700) of the TIFF-based file at `path` (a DNG), if
/// it has one. Reads are bounded: the header, one directory, the packet.
pub(crate) fn embedded_xmp(path: &Path) -> Result<Option<String>> {
    let mut f = File::open(path).at(path)?;
    let len = f.metadata().at(path)?.len();
    let mut head = [0u8; 8];
    if f.read_exact(&mut head).is_err() {
        return Ok(None);
    }
    let le = match &head[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return Ok(None),
    };
    let u16_of = |b: &[u8]| {
        if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let u32_of = |b: &[u8]| {
        let a = [b[0], b[1], b[2], b[3]];
        if le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        }
    };
    if u16_of(&head[2..4]) != 42 {
        return Ok(None);
    }
    let ifd = u64::from(u32_of(&head[4..8]));
    if ifd + 2 > len {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(ifd)).at(path)?;
    let mut n = [0u8; 2];
    f.read_exact(&mut n).at(path)?;
    let count = u16_of(&n).min(MAX_IFD_ENTRIES);
    let mut entries = vec![0u8; usize::from(count) * 12];
    if f.read_exact(&mut entries).is_err() {
        return Ok(None);
    }
    for e in entries.as_chunks::<12>().0 {
        if u16_of(&e[..2]) != 700 {
            continue;
        }
        let ty = u16_of(&e[2..4]);
        let n = u64::from(u32_of(&e[4..8]));
        if !(ty == 1 || ty == 7) || n == 0 || n > MAX_XMP_BYTES {
            return Ok(None);
        }
        let off = u64::from(u32_of(&e[8..12]));
        if n <= 4 || off + n > len {
            return Ok(None);
        }
        f.seek(SeekFrom::Start(off)).at(path)?;
        let mut buf = vec![0u8; n as usize];
        f.read_exact(&mut buf).at(path)?;
        return Ok(String::from_utf8(buf).ok());
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use ds_core::ColorTag;

    use super::*;
    use crate::xmp::tests::SIDECAR;

    fn no() -> bool {
        false
    }

    /// A little-endian TIFF with an XMP packet in tag 700.
    pub(crate) fn tiff_with_xmp(xmp: &str) -> Vec<u8> {
        let mut v = b"II*\0".to_vec();
        v.extend_from_slice(&8u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&700u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&(xmp.len() as u32).to_le_bytes());
        v.extend_from_slice(&26u32.to_le_bytes()); // 8 + 2 + 12 + 4
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(xmp.as_bytes());
        v
    }

    #[test]
    fn reads_sidecars_dng_packets_and_pairs_originals() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("Trip");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("A.xmp"), SIDECAR).unwrap();
        fs::write(sub.join("A.JPG"), b"j").unwrap();
        fs::write(sub.join("a.ARW"), b"r").unwrap();
        fs::write(
            d.path().join("B.xmp"),
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>",
        )
        .unwrap(); // no data
        fs::write(d.path().join("C.dng"), tiff_with_xmp(SIDECAR)).unwrap();
        fs::write(d.path().join("D.dng"), tiff_with_xmp(SIDECAR)).unwrap();
        fs::write(
            d.path().join("D.xmp"),
            SIDECAR.replace("Rating=\"4\"", "Rating=\"-1\""),
        )
        .unwrap();
        fs::write(d.path().join("._A.xmp"), b"junk").unwrap();
        fs::write(d.path().join("E.xmp"), "<broken").unwrap();
        fs::write(d.path().join("E.arw"), b"r").unwrap();
        let lib = read_sidecars(d.path(), &no).unwrap();
        assert_eq!(lib.kind, SourceKind::Sidecars);
        let keys: Vec<&str> = lib.photos.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["xmp:C.dng", "xmp:D.xmp", "xmp:Trip/A.xmp"]);
        let a = &lib.photos[2];
        assert_eq!(a.path, sub.join("a.ARW"), "a RAW sibling is preferred");
        assert_eq!(a.rating, Rating::new(4));
        assert_eq!(a.color_tag, Some(ColorTag::Red));
        assert_eq!(a.orientation, Some(6));
        assert_eq!(a.title.as_deref(), Some("A title"));
        assert!(a.develop.as_ref().unwrap().contains_key("Exposure2012"));
        let c = &lib.photos[0];
        assert_eq!(c.path, d.path().join("C.dng"));
        assert_eq!(c.rating, Rating::new(4));
        let dd = &lib.photos[1];
        assert_eq!(dd.pick, Some(PickState::Rejected), "-1 is rejected");
        assert_eq!(dd.rating, Some(Rating::default()));
        assert_eq!(dd.path, d.path().join("D.dng"));
        assert!(
            lib.warnings.iter().any(|w| w.contains("E.xmp")),
            "{:?}",
            lib.warnings
        );
    }

    #[test]
    fn unmapped_labels_and_missing_originals() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("Z.xmp"),
            SIDECAR.replace("\"Red\"", "\"Custom\""),
        )
        .unwrap();
        let lib = read_sidecars(d.path(), &no).unwrap();
        assert_eq!(lib.photos[0].unmapped_label.as_deref(), Some("Custom"));
        assert_eq!(lib.photos[0].color_tag, None);
        assert_eq!(lib.photos[0].path, d.path().join("Z"));
    }

    /// 20k raw+sidecar pairs in one folder were quadratic (a scan of all names per sidecar).
    #[test]
    fn a_folder_of_many_pairs_scales_and_keeps_the_case_of_names() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..20_000 {
            fs::write(d.path().join(format!("IMG_{i}.ARW")), b"x").unwrap();
            fs::write(d.path().join(format!("IMG_{i}.xmp")), SIDECAR).unwrap();
        }
        let started = std::time::Instant::now();
        let lib = read_sidecars(d.path(), &no).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(lib.photos.len(), 20_000);
        assert!(lib.photos.iter().any(|p| p.key == "xmp:IMG_7.xmp"));
        assert!(
            lib.photos
                .iter()
                .all(|p| p.path.extension().unwrap() == "ARW")
        );
    }

    #[test]
    fn folder_walk_limits() {
        let d = tempfile::tempdir().unwrap();
        // Symlinks are not followed (a loop and a link to a file).
        std::os::unix::fs::symlink(d.path(), d.path().join("loop")).unwrap();
        fs::write(d.path().join("real.xmp"), SIDECAR).unwrap();
        std::os::unix::fs::symlink(d.path().join("real.xmp"), d.path().join("link.xmp")).unwrap();
        let lib = read_sidecars(d.path(), &no).unwrap();
        assert_eq!(lib.photos.len(), 1);
        // Too deep: the deepest folder is not entered.
        let mut deep = d.path().to_owned();
        for i in 0..=ds_session::MAX_EXTERNAL_DEPTH + 1 {
            deep = deep.join(format!("d{i}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("far.xmp"), SIDECAR).unwrap();
        let lib = read_sidecars(d.path(), &no).unwrap();
        assert_eq!(lib.photos.len(), 1);
        // Hidden folders are skipped; cancelling stops the walk.
        fs::create_dir_all(d.path().join(".hidden")).unwrap();
        fs::write(d.path().join(".hidden/h.xmp"), SIDECAR).unwrap();
        assert_eq!(read_sidecars(d.path(), &no).unwrap().photos.len(), 1);
        assert!(matches!(
            read_sidecars(d.path(), &|| true),
            Err(LrError::Cancelled)
        ));
        // Not a folder / missing.
        assert!(read_sidecars(&d.path().join("real.xmp"), &no).is_err());
        assert!(read_sidecars(&d.path().join("nope"), &no).is_err());
    }

    #[test]
    fn oversized_and_non_text_sidecars_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let big = File::create(d.path().join("big.xmp")).unwrap();
        big.set_len(MAX_XMP_BYTES + 1).unwrap();
        fs::write(d.path().join("bin.xmp"), [0xff, 0xfe, 0xfd]).unwrap();
        let lib = read_sidecars(d.path(), &no).unwrap();
        assert_eq!(lib.photos.len(), 0);
        assert_eq!(lib.warnings.len(), 2, "{:?}", lib.warnings);
    }

    #[test]
    fn embedded_packets_are_read_with_bounds() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.dng");
        fs::write(&p, tiff_with_xmp("<x></x>")).unwrap();
        assert_eq!(embedded_xmp(&p).unwrap().as_deref(), Some("<x></x>"));
        // Big-endian: same packet.
        let mut be = b"MM\0*".to_vec();
        be.extend_from_slice(&8u32.to_be_bytes());
        be.extend_from_slice(&1u16.to_be_bytes());
        be.extend_from_slice(&700u16.to_be_bytes());
        be.extend_from_slice(&7u16.to_be_bytes());
        be.extend_from_slice(&5u32.to_be_bytes());
        be.extend_from_slice(&26u32.to_be_bytes());
        be.extend_from_slice(&0u32.to_be_bytes());
        be.extend_from_slice(b"<abc>");
        fs::write(&p, &be).unwrap();
        assert_eq!(embedded_xmp(&p).unwrap().as_deref(), Some("<abc>"));
        // A packet that claims to reach past the end of the file.
        let mut bad = tiff_with_xmp("<x></x>");
        bad.truncate(bad.len() - 2);
        fs::write(&p, &bad).unwrap();
        assert_eq!(embedded_xmp(&p).unwrap(), None);
        // A declared size above the limit is never allocated.
        let mut huge = tiff_with_xmp("<x></x>");
        huge[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&p, &huge).unwrap();
        assert_eq!(embedded_xmp(&p).unwrap(), None);
        // Wrong type, no tag, short, not TIFF, BigTIFF, directory past the end.
        let mut wrong = tiff_with_xmp("<x></x>");
        wrong[12..14].copy_from_slice(&3u16.to_le_bytes());
        let mut other = tiff_with_xmp("<x></x>");
        other[10..12].copy_from_slice(&701u16.to_le_bytes());
        let mut past = tiff_with_xmp("<x></x>");
        past[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut tiny = tiff_with_xmp("<x></x>");
        tiny.truncate(12);
        for (name, bytes) in [
            ("wrong", wrong),
            ("other", other),
            ("past", past),
            ("tiny", tiny),
            ("short", b"II".to_vec()),
            ("jpeg", b"\xff\xd8\xff\xe0........".to_vec()),
            ("big", b"II+\0........".to_vec()),
        ] {
            fs::write(&p, bytes).unwrap();
            assert_eq!(embedded_xmp(&p).unwrap(), None, "{name}");
        }
        // Not UTF-8.
        fs::write(
            &p,
            tiff_with_xmp("\u{e9}")
                .iter()
                .map(|b| if *b == 0xc3 { 0xff } else { *b })
                .collect::<Vec<u8>>(),
        )
        .unwrap();
        assert_eq!(embedded_xmp(&p).unwrap(), None);
        assert!(embedded_xmp(&d.path().join("missing")).is_err());
    }
}
