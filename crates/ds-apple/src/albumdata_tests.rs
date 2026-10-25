#![allow(clippy::float_cmp)]

use super::*;
use crate::fixtures::{snapshot, write};

/// A library recorded at `/Users/someone/Pictures/iPhoto Library` and now
/// somewhere else.
const RECORDED: &str = "/Users/someone/Pictures/iPhoto Library";

fn album_data(images: &str, rest: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple Computer//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Application Version</key><string>8.1.2 (424)</string>
<key>Archive Path</key><string>{RECORDED}</string>
<key>List of Keywords</key><dict>
  <key>1</key><string>Beach</string><key>2</key><string> Family </string><key>3</key><string></string>
</dict>
<key>Master Image List</key><dict>{images}</dict>
{rest}
</dict></plist>"#
    )
}

const IMAGES: &str = r"
<key>100</key><dict>
  <key>MediaType</key><string>Image</string>
  <key>Caption</key><string>Sandcastle</string>
  <key>Comment</key><string> Built by the kids </string>
  <key>Rating</key><integer>4</integer>
  <key>Roll</key><integer>7</integer>
  <key>ImagePath</key><string>/Users/someone/Pictures/iPhoto Library/Originals/2008/Beach/IMG_1.JPG</string>
  <key>Keywords</key><array><string>1</string><string>2</string><string>1</string><string>3</string><string>99</string><integer>2</integer><real>1</real></array>
  <key>latitude</key><real>43.5</real><key>longitude</key><real>-1.5</real>
</dict>
<key>101</key><dict>
  <key>Caption</key><string>IMG_2</string>
  <key>Rating</key><string>9</string>
  <key>ImagePath</key><string>/Users/someone/Pictures/iPhoto Library/Modified/2008/Beach/IMG_2.jpg</string>
  <key>OriginalPath</key><string>/Users/someone/Pictures/iPhoto Library/Originals/2008/Beach/IMG_2.JPG</string>
  <key>latitude</key><real>0</real><key>longitude</key><real>0</real>
</dict>
<key>102</key><dict>
  <key>ImagePath</key><string>/Volumes/Archive/Elsewhere/IMG_3.JPG</string>
</dict>
<key>103</key><dict>
  <key>ImagePath</key><string>/Users/someone/Pictures/iPhoto Library/../escape.JPG</string>
</dict>
<key>104</key><dict><key>Caption</key><string>no file</string></dict>
<key>105</key><string>not a photo</string>
<key>106</key><dict>
  <key>ImagePath</key><string>Originals/relative/IMG_6.JPG</string>
  <key>Rating</key><real>2</real>
</dict>
";

const LISTS: &str = r"
<key>List of Rolls</key><array>
  <dict><key>RollID</key><integer>7</integer><key>RollName</key><string>Beach day</string>
    <key>KeyList</key><array><string>100</string><string>101</string><string>100</string><string>999</string></array></dict>
  <dict><key>AlbumId</key><integer>8</integer><key>AlbumName</key><string>Elsewhere</string>
    <key>KeyList</key><array><integer>102</integer></array></dict>
  <dict><key>RollName</key><string>no id</string></dict>
</array>
<key>List of Albums</key><array>
  <dict><key>AlbumId</key><integer>1</integer><key>AlbumName</key><string>Photos</string><key>Master</key><true/>
    <key>KeyList</key><array><string>100</string></array></dict>
  <dict><key>AlbumId</key><integer>2</integer><key>AlbumName</key><string>Flagged</string><key>Album Type</key><string>Flagged</string>
    <key>KeyList</key><array><string>100</string></array></dict>
  <dict><key>AlbumId</key><integer>3</integer><key>AlbumName</key><string>Holidays</string><key>Album Type</key><string>Folder</string>
    <key>Parent</key><integer>3</integer></dict>
  <dict><key>AlbumId</key><integer>4</integer><key>AlbumName</key><string>Best</string><key>Album Type</key><string>Regular</string>
    <key>Parent</key><integer>3</integer><key>KeyList</key><array><string>101</string><string>100</string></array></dict>
  <dict><key>AlbumId</key><integer>5</integer><key>AlbumName</key><string>Five stars</string><key>Album Type</key><string>Smart</string></dict>
  <dict><key>AlbumId</key><integer>6</integer><key>AlbumName</key><string>Summer book</string><key>Album Type</key><string>Book</string></dict>
  <dict><key>AlbumId</key><integer>9</integer><key>AlbumName</key><string>Untyped</string><key>Parent</key><integer>77</integer></dict>
  <dict><key>AlbumName</key><string>no id</string></dict>
</array>
";

fn library(xml: &str) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("iPhoto Library");
    write(&root.join("AlbumData.xml"), xml.as_bytes());
    (d, root)
}

#[test]
fn photos_albums_and_events() {
    let (_d, root) = library(&album_data(IMAGES, LISTS));
    let before = snapshot(&root);
    let lib = read(&root, &|| false).unwrap();
    assert_eq!(snapshot(&root), before);
    assert_eq!(lib.written_by.as_deref(), Some("iPhoto 8.1.2 (424)"));
    let keys: Vec<&str> = lib.photos.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(keys, ["100", "101", "102", "106"]);
    let p = &lib.photos[0];
    let originals = root.join("Originals");
    assert_eq!(
        p.path,
        originals.join("2008/Beach/IMG_1.JPG"),
        "moved onto the library's place"
    );
    assert_eq!(p.folder.as_deref(), Some(originals.as_path()));
    assert_eq!(p.title.as_deref(), Some("Sandcastle"));
    assert_eq!(p.caption.as_deref(), Some("Built by the kids"));
    assert_eq!(p.rating, Rating::new(4));
    assert_eq!(p.pick, Some(PickState::Picked), "in the Flagged album");
    assert_eq!(
        p.keywords.as_deref(),
        Some(&["Beach".to_owned(), "Family".to_owned()][..])
    );
    assert_eq!(p.geotag.unwrap().longitude, -1.5);
    assert!(p.develop.is_none());
    // Edited in iPhoto: the original, noted.
    let e = &lib.photos[1];
    assert_eq!(e.path, originals.join("2008/Beach/IMG_2.JPG"));
    assert!(e.develop.as_ref().unwrap().contains_key(IPHOTO_EDIT));
    assert_eq!(
        e.title.as_deref(),
        Some(""),
        "the file's own name is no title"
    );
    assert_eq!(e.rating, None, "out of range");
    assert_eq!(e.pick, Some(PickState::Unflagged));
    assert!(e.geotag.is_none());
    // Referenced elsewhere: kept where it is.
    let r = &lib.photos[2];
    assert_eq!(
        r.path,
        PathBuf::from("/Volumes/Archive/Elsewhere/IMG_3.JPG")
    );
    assert!(r.folder.is_none());
    assert_eq!(
        lib.photos[3].path,
        root.join("Originals/relative/IMG_6.JPG")
    );
    assert_eq!(lib.photos[3].rating, Rating::new(2));
    let w = lib.warnings.join("\n");
    assert!(w.contains("1 photo path leaving the library"), "{w}");
    assert!(w.contains("1 photo was edited in iPhoto"), "{w}");

    let by = |n: &str| lib.collections.iter().find(|c| c.name == n).unwrap();
    let beach = by("Beach day");
    assert_eq!(
        (beach.id, beach.members.clone()),
        (-8, vec!["100".to_owned(), "101".to_owned()])
    );
    assert_eq!(by("Elsewhere").members, ["102"]);
    let best = by("Best");
    assert_eq!(
        best.parents,
        ["Holidays"],
        "a folder that is its own parent stops"
    );
    assert_eq!(best.members, ["101", "100"]);
    assert!(by("Five stars").smart);
    assert!(
        by("Summer book")
            .unsupported
            .as_deref()
            .unwrap()
            .contains("book")
    );
    assert!(by("Untyped").unsupported.is_none());
    for gone in ["Photos", "Flagged", "Holidays", "no id"] {
        assert!(lib.collections.iter().all(|c| c.name != gone), "{gone}");
    }
}

#[test]
fn a_library_without_lists_or_an_archive_path() {
    let xml = r#"<?xml version="1.0"?><plist version="1.0"><dict>
      <key>Master Image List</key><dict>
        <key>1</key><dict><key>ImagePath</key><string>Originals/a.JPG</string></dict>
      </dict></dict></plist>"#;
    let (_d, root) = library(xml);
    let lib = read(&root, &|| false).unwrap();
    assert_eq!(lib.written_by.as_deref(), Some("iPhoto"));
    assert_eq!(lib.photos[0].path, root.join("Originals/a.JPG"));
    assert!(lib.collections.is_empty() && lib.warnings.is_empty());
}

#[test]
fn damaged_files_are_errors() {
    let (_d, root) = library("<plist><array/></plist>");
    assert!(matches!(
        read(&root, &|| false),
        Err(AppleError::Xml { .. })
    ));
    let (_d, root) = library("<plist><dict></dict></plist>");
    assert!(matches!(
        read(&root, &|| false),
        Err(AppleError::NotALibrary { .. })
    ));
    let (_d, root) = library("not xml at all <<<");
    assert!(matches!(
        read(&root, &|| false),
        Err(AppleError::Xml { .. })
    ));
    let d = tempfile::tempdir().unwrap();
    assert!(matches!(
        read(d.path(), &|| false),
        Err(AppleError::Io { .. })
    ));
    let (_d, root) = library(&album_data(IMAGES, LISTS));
    assert!(matches!(read(&root, &|| true), Err(AppleError::Cancelled)));
    let calls = std::cell::Cell::new(0);
    let later = || {
        calls.set(calls.get() + 1);
        calls.get() > 1
    };
    assert!(matches!(read(&root, &later), Err(AppleError::Cancelled)));
}

#[test]
fn values_are_read_whatever_their_type() {
    let d: Dictionary = [
        ("i".to_owned(), Value::Integer(3.into())),
        ("r".to_owned(), Value::Real(2.9)),
        ("s".to_owned(), Value::String(" 5 ".into())),
        ("b".to_owned(), Value::Boolean(true)),
        ("big".to_owned(), Value::Real(1e300)),
        ("bad".to_owned(), Value::String("x".into())),
    ]
    .into_iter()
    .collect();
    assert_eq!(int(&d, "i"), Some(3));
    assert_eq!(int(&d, "r"), Some(2));
    assert_eq!(int(&d, "s"), Some(5));
    assert_eq!(int(&d, "b"), None);
    assert_eq!(int(&d, "big"), None);
    assert_eq!(real(&d, "i"), Some(3.0));
    assert_eq!(real(&d, "s"), Some(5.0));
    assert_eq!(real(&d, "bad"), None);
    assert_eq!(real(&d, "b"), None);
    assert_eq!(real(&d, "missing"), None);
}
