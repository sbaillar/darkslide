# Aperture and iPhoto libraries (as Luxtable reads them)

Luxtable imports ratings, flags, labels, keywords, titles, captions, places, the edits that map, and projects, events and albums from Apple's discontinued photo apps: Aperture 3 and iPhoto (6 and later). This file records what is read, how each field maps, and **what has not been confirmed**.

**Provenance.** No Aperture or iPhoto installation or library was available while this was written. Apple never published either format. Everything below comes from open-source readers of these libraries: osxphotos (MIT; it reads the same `RK…` schema in Photos 1–4 libraries, which descend from Aperture's), aperture2darktable and ApertureExporter (Apache-2.0; projects, versions, archived adjustments) and iphoto2xmp (read for the iPhoto 9.x tables, no code taken). It also draws on a public schema dump of a Photos 1.x `Library.apdb` and on published descriptions of `AlbumData.xml`. Details only one source shows, or none, are marked **UNCONFIRMED**. The reader is built so that a wrong guess costs one field, with a warning or a "not imported" line in the report, and never the whole import. Every optional table and column is feature-detected, every column is read by its actual SQLite type, and the tests use synthetic libraries built in the test code. No real library is committed.

The code is `crates/lt-apple`: `detect.rs` (what a path is), `sql.rs` (copying and reading the databases), `apdb.rs` (Aperture 3 / iPhoto 9.3+), `albumdata.rs` (older iPhoto), `archive.rs` (archived adjustment parameters), `map.rs` (adjustments → Luxtable). The import itself is lt-lr's engine (`lt_lr::import` with `lt_apple::FLAVOR`).

## Formats

| Library | How it is recognised | Read from |
|---|---|---|
| Aperture 3 (`.aplibrary`) | `Database/apdb/Library.apdb` (or `Database/Library.apdb`) | `Library.apdb`, `Properties.apdb` |
| iPhoto 9.3 and later (`.photolibrary`, `.migratedphotolibrary` after Photos migrated it) | the same database layout | the same |
| iPhoto 6 to 9.2 | `AlbumData.xml` at the library's top | `AlbumData.xml` |
| Photos (`.photoslibrary`, or `database/Photos.sqlite` / `database/photos.db`) | refused: "… is a Photos library, which Luxtable doesn't read" | — |
| Aperture 1 or 2 (`Aperture.aplib`, or `ZRKVERSION` instead of `RKVersion`) | refused: open it once in Aperture 3 to upgrade it | — |

The library is never written to. Its databases are copied, with any `-wal` or `-journal` beside them, into a temporary directory, and the copies are read. Nothing inside the package is locked, given a `-shm` file or changed, and a database Aperture left locked or mid-transaction still reads (as committed). `AlbumData.xml` is read with plain `File::open`. Only real tables are queried, the read has a time limit and can be cancelled, and property lists are checked for depth and size before they are parsed.

## Aperture 3 / iPhoto 9.3+ (`Library.apdb`)

| Table | Columns used |
|---|---|
| `RKMaster` | `uuid`, `imagePath`, `volumeId`, `isInTrash` |
| `RKVolume` | `modelId`, `name` |
| `RKVersion` | `modelId`, `uuid`, `masterUuid`, `name`, `versionNumber`, `mainRating`, `isFlagged`, `colorLabelIndex`, `isHidden`, `isInTrash`, `showInLibrary`, `projectUuid`, `latitude`/`longitude`, `exifLatitude`/`exifLongitude`, `imageDate`, `imageTimeZoneOffsetSeconds`, `rotation` |
| `RKFolder` | `uuid`, `modelId`, `name`, `folderType` (2: a project or event; 1: a folder), `parentFolderUuid`, `isMagic`, `isInTrash` |
| `RKAlbum`, `RKAlbumVersion` | `modelId`, `name`, `albumType` (1: an album), `albumSubclass` (3: plain, 2: smart, 1: built in), `folderUuid`, `isMagic`, `isInTrash`; `albumId`, `versionId` |
| `RKKeyword`, `RKKeywordForVersion` | `modelId`, `name`; `versionId`, `keywordId` |
| `RKImageAdjustment` | `versionUuid`, `name`, `adjIndex`, `data`; `isEnabled` and `maskUuid` when present (**UNCONFIRMED**) |
| `Properties.apdb`: `RKIptcProperty`, `RKUniqueString` | `versionId`, `propertyKey` = `Caption/Abstract`, `stringId`; `modelId`, `stringProperty` |

**Originals.** A managed original is `<library>/Masters/<imagePath>`. A referenced one (`volumeId` set) is `/Volumes/<RKVolume.name>/<imagePath>`. An original on a volume that isn't connected is not found, like a missing original in the Lightroom import: the report lists it by folder and warns "N referenced originals are on "Disk", which isn't connected". An original in the trash, or whose path would leave its folder through `..`, is left out.

**Versions.** A version in the trash is left out. So is an original version iPhoto keeps hidden once the photo has been edited (`showInLibrary` 0), when the photo has a shown one. A photo's first version (by `versionNumber`) is its Luxtable photo; the others become extra variants, as Lightroom's virtual copies do.

## Older iPhoto (`AlbumData.xml`)

An XML property list. `Archive Path` is where the library was when iPhoto wrote it, and image paths that start with it are moved onto where the library is now (a library copied to another disk still reads). `Master Image List` maps an id to `ImagePath`, `OriginalPath`, `Caption`, `Comment`, `Rating`, `Keywords` (ids into `List of Keywords`) and `latitude`/`longitude` (iPhoto '09 and later, **UNCONFIRMED**). `List of Rolls` lists the events (`RollID`/`AlbumId`, `RollName`/`AlbumName`, `KeyList`). `List of Albums` lists the albums (`AlbumId`, `AlbumName`, `Album Type`, `Parent`, `KeyList`), and the album of type `Flagged` lists the flagged photos.

**Edited photos.** When `OriginalPath` is set, `ImagePath` is the edited copy under `Modified/`. iPhoto kept the edit only as that rendered JPEG, so the **original** is imported and the photo is listed as "iPhoto edit: … the original is imported unedited".

## What maps

| Aperture / iPhoto | Luxtable |
|---|---|
| Rating 0–5 | Rating |
| Rating -1 (Aperture's reject) | Rejected, rating 0 |
| Hidden (iPhoto) | Rejected |
| Flagged | Picked (a reject wins over a flag) |
| Colour label 1–6: red, orange, yellow, green, blue, purple (`colorLabelIndex`, **UNCONFIRMED**: the order of Aperture's label menu, 0 none) | Colour tag |
| Colour label 7, grey | Not imported, reported ("Label Gray has no Luxtable colour") |
| Keywords | Keywords (the keyword itself, not its parents) |
| Version name (Aperture) / title (iPhoto `Caption`), unless it is the file's own name | Title |
| Caption (`Caption/Abstract`; iPhoto `Comment`) | Caption |
| Position (the version's, else the file's) | The photo's geotag (with the metadata category) |
| Capture date with its time-zone offset | Used only to match photos by name and time |
| Projects (Aperture) and events (iPhoto) | Albums named after them, inside the folders around them ("Travel / Paris") |
| Albums | Albums ("Paris / Best" for an album inside a project) |
| Smart albums | Not imported, reported (a saved search Luxtable can't read) |
| Books, slideshows, light tables, web pages | Not imported, reported |
| Faces | Not imported (deferred: face names are not read) |

Edits (`RKImageAdjustment.data` is an `NSKeyedArchiver` archive of the adjustment's parameters, decoded as the open-source readers do). The parameter names follow Core Image's `input<Name>` convention and are **UNCONFIRMED** one by one. A parameter is looked up without its `input` prefix, ignoring case. An adjustment whose expected parameters aren't there is listed as "its settings weren't recognised", never guessed at. Everything is approximate: Apple's engine is not Luxtable's.

| Aperture / iPhoto adjustment | Luxtable |
|---|---|
| Exposure: Exposure (EV) | Exposure (clamped to ±5 EV) |
| Exposure: Recovery (0–1.5) | Highlights (0–100) |
| Exposure: Black Point | Blacks (×-2) |
| Exposure: Brightness (-2–2) | Brightness (×50) |
| Enhance: Contrast (-1–1) | Contrast (×100) |
| Enhance: Definition (0–1) | Clarity (×100) |
| Enhance: Saturation (0–2, 1 neutral) | Saturation ((v-1)×100) |
| Enhance: Vibrancy (-1–1) | Vibrance (×100) |
| Highlights & Shadows: Highlights, Shadows (0–100) | Highlights, Shadows |
| White Balance: temperature (K) and tint, RAW files | Custom white balance |
| Straighten (degrees; positive turns the picture counter-clockwise, **UNCONFIRMED**) | Crop angle |
| Crop (pixels, origin bottom-left, as iphoto2xmp describes it) | Crop rectangle |
| Black & White | Saturation -100 (the red/green/blue mix is reported) |
| Rotation 180° | Two quarter turns |

**Not imported, and listed in the report:** Levels, Curves, Color (per hue), sharpening (Sharpen, Edge Sharpen), Noise Reduction, Vignette and Devignette, Retouch, Spot & Patch, Red Eye, Sepia, Color Monochrome, Monochrome Mixer, Chromatic Aberration, iOS edits, every brushed-in adjustment (`maskUuid`), a second instance of an adjustment, quarter-turn rotations (90° or 270°: the direction isn't confirmed), a crop on a quarter-turned photo, a white balance on a non-RAW file, and iPhoto 6–9.2 edits. RAW Fine Tuning (`RKRawDecodeOperation`) is the decoder's own settings, on every RAW photo, and is skipped silently. The advanced sliders of Highlights & Shadows (radius, mid contrast, tonal widths) and Enhance's tint wheels are not read.

## The import

The same engine as the Lightroom import (`lt_lr::import`), with the same three-way merge, categories, dry run, conflicts, notes and revert. It keeps its own record (`<store>/.lt/settings/apple-import.json`) and journal (`apple-import-undo.json`), so reverting an Aperture import never takes back a Lightroom one, or the reverse. Photos are matched to the session's by path (a session opened in place on the library's `Masters` or `Originals` folder), by content (BLAKE3 of the file where the library says it is) and by name and capture time. A folder inside a library package can be opened as a session: its index and sidecars are then kept in Luxtable's own store, never inside the library.
