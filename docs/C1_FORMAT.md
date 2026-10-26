# Capture One session format (as Luxtable reads it)

Worked out for interoperability from a Capture One 23 session the user owns, read-only. That session was last written by C1 16.1.3.10 Pro Mac (`ZVERSIONINFO.ZAUTHOR`), database format 1600. It is a typical shoot: RAW files with some ratings, some edits and some photos in the session Trash. The real session was never opened in place:
- the database was copied and the copy opened with `?mode=ro&immutable=1`;
- the `.cos` files were copied and parsed as copies.

Everything here was observed in session files the user owns. Whatever those files don't show is marked **unconfirmed**. The parser lives in `crates/lt-c1`, and its fixtures are synthetic (`crates/lt-c1/fixtures/`).

## Folder layout

```
<Session>/
  <Session>.cosessiondb      SQLite, the session database (legacy rollback journal)
  Capture/ Selects/ Output/ Trash/
  *.ARW                      images may also sit at the top level (location ".")
  CaptureOne/
    Settings153/<file>.cos   per-image settings (XML); 153 = settings version
    Cache/Proxies, Cache/Thumbnails  (.cof/.cop/.cot, ignored)
  Trash/CaptureOne/Settings153/<file>.cos   each image folder has its own CaptureOne/
```

- Each folder that holds images has a `CaptureOne/Settings<NNN>/` folder for its `.cos` files.
- When several `Settings<NNN>` folders exist, Luxtable reads the highest-numbered one.
- A session can also reference folders outside itself; the real one referenced a folder on another volume. Settings for those images live next to them, and Luxtable doesn't import them.

## The database (`*.cosessiondb`)

The database uses Core Data–style tables. Every row has `Z_PK`, and `Z_ENT` indexes `ZENTITIES`. Timestamps are Unix seconds (floats), not Core Data's 2001 epoch.

| Table | What matters |
|---|---|
| `ZENTITIES` | `Z_ENT` → entity name, e.g. 2 `AlbumCollection`, 3 `FolderCollection`, 5 `SmartCollection`, 7 `ProjectCollection`, 1 `Collection` (group), 17 `VariantAdjustmentLayer`, 18 `VariantDefaultLayer`, 20 `VariantCombinedLayer`. Resolve by name, not by number. |
| `ZDOCUMENTCONTENT` | `ZDOCUMENTUUID` is the session's UUID, and `ZROOTCOLLECTION` its root project. |
| `ZPATHLOCATION` | `ZISRELATIVE=1`: `ZRELATIVEPATH` relative to the session folder (`Capture`, `Selects`, `Output`, `Trash`, `.`). `ZISRELATIVE=0`: `ZMACROOT` + `ZRELATIVEPATH` (absolute). |
| `ZIMAGE` | One row per file: `ZIMAGEUUID`, `ZIMAGEFILENAME`, `ZIMAGELOCATION` → `ZPATHLOCATION`, EXIF columns, and `ZISTRASHED` (0 everywhere; Trash membership is the location). |
| `ZVARIANT` | One row per variant: `ZVARIANTUUID`, `ZIMAGE`, `ZINDEX`, plus `ZDEFAULTLAYER`, `ZADJUSTMENTLAYER` and `ZCOMBINEDSETTINGS` (→ `ZVARIANTLAYER`). `ZCANVASSIZE` is cached `w;h` after lens correction. |
| `ZVARIANTLAYER` | One column per setting (`ZEXPOSURE`, `ZWHITEBALANCE`, `ZCROP`, …). **NULL means "not set"** in the adjustment layer. The combined layer is DL overlaid by AL. |
| `ZVARIANTMETADATA` | Per layer (`ZLAYER` = the layer's `Z_PK`): `ZBASIC_RATING`, `ZCOLOR_TAG_INDEX`, `ZCONTENT_KEYWORDS`, `ZBASIC_LABEL`, IPTC fields. |
| `ZCOLLECTION` | Albums, smart albums (`ZSMARTCRITERIA` XML), folders (`ZFOLDERLOCATION`), groups and projects (`ZPARENT` tree). |
| `ZIMAGEINCOLLECTION`, `ZVARIANTINCOLLECTION` | Album membership by image or by variant. |
| `ZKEYWORD` | Keyword library, a nested set (`ZLEFT`/`ZRIGHT`, `ZPARENT`). Empty in the real session. |

Column names are the `.cos` keys upper-cased with a `Z` prefix (`ZEXPOSURE` ↔ `Exposure`, `ZBASIC_RATING` ↔ `Basic_Rating`), with these exceptions:

| Column | `.cos` key |
|---|---|
| `ZHIGHLIGHTRECOVERY` | `HighlightRecoveryEx` |
| `ZLENSVIGNETTING` | `Vignetting` |
| `ZCROPASPECTRATIONAME` | `CropAspectRatioName` (sic) |
| `ZLEVELSHIGHLIGHT`, `ZLEVELSSHADOW`, `ZLEVELSTARGET*` | `Highlight`, `Shadow`, `Target*` |

In the real session, the database's adjustment layers held exactly the keys of the `.cos` adjustment layers, with the same counts per key.

## `.cos` settings files

```xml
<IMG>
  <E K="Width" V="6000" /> <E K="UUID" V="<image uuid>" /> …   image facts (EXIF, size)
  <VAR Engine="1300">                                         one per variant
    <E K="UUID" V="<variant uuid>" /> …
    <DL> <E K="Rotation" V="-90" /> <E K="WhiteBalance" V="2.566;1;1.496" /> … </DL>
    <AL> <E K="Exposure" V="-0.59" /> <E K="Basic_Rating" V="5" /> … </AL>
    <PHS><PH><E K="URL" V="file:///…jpg"/><E K="Date" V="…"/></PH></PHS>   process history
  </VAR>
</IMG>
```

- **DL (default layer)** holds C1's starting point for the image:
  - the camera profile (`ICCProfile`, `FilmCurve`);
  - the as-shot white balance;
  - lens data;
  - `Rotation` = the EXIF orientation;
  - the default sharpening and noise reduction.
- **AL (adjustment layer)** holds only what the user changed. A key's effective value is its AL value if present, else its DL value.
- C1 also writes computed values into AL that aren't edits:
  - `CalculatedDehazeColor`, `DehazeLevelHigh` and `DehazeLevelLow` (1,408 images, none of them edited);
  - tools reset to their default value (a curve back at `0,0;1,1`).

  So an AL key counts as an edit only when it differs from its DL value.
- The `.cos` file is preferred over the database: it's the per-image source C1 can rebuild a database from. The database is the fallback for an image without a readable `.cos`.

### Keys and how they map

| C1 key (AL) | Luxtable | Notes |
|---|---|---|
| `Basic_Rating` | rating 0–5 | Clamped. |
| `Color_Tag_Index` | colour tag | 0 none, 1 red, 2 orange, 3 yellow, 4 green, 5 blue, 6 pink, 7 purple: the same order as Luxtable's. The key name and the index order (C1's menu order) are **unconfirmed**: the real session has no colour tags. `Basic_Label` with a colour name is the fallback. |
| `Content_Keywords` | keywords | Split on `;`, and also on `,` unless the keyword library has the entry with its comma. A `Parent\|Child` entry gives its leaf. **Separator unconfirmed** (no keywords in the real data). |
| folder `Selects/` | pick = *picked* | C1 has no pick/reject flag. C1's Selects folder is its "pick". |
| folder `Trash/` | — | Luxtable's in-place walk skips a C1 session's `Trash/` (L9), so these images aren't in the Luxtable session. They are counted, not imported. |
| `Exposure` | `exposure.exposure_ev` | EV, clamped ±5. |
| `Contrast`, `Brightness`, `Saturation` | `exposure.*` | Copied by value, clamped ±100. The real data has saturation up to 53.6, so the scale is ±100. The engines differ, so the look is close, not identical. |
| `HighlightRecoveryEx` | `hdr.highlights` = **−value** | C1 23's HDR highlight is negative to recover. The real edits are all highlights −, shadows +, whites −, blacks +, which is Lightroom's convention. Luxtable's highlights recover for positive values. |
| `ShadowRecovery`, `WhiteRecovery`, `BlackRecovery` | `hdr.shadows/whites/blacks` | Same sign. |
| `WhiteBalance` (`R;G;B` multipliers, G = 1) | `WhiteBalance::Custom{K, tint}` | Applied as a ratio to C1's DL (as-shot) multipliers, on top of the RAW's own as-shot, then converted with the camera matrix (`lt_pipeline::color::multipliers_to_wb`). |
| `WhiteBalanceTemperature`, `WhiteBalanceTint` | `Custom{K, tint}` | Only when there are no multipliers. Seen in the C1 vocabulary, not in the real data. |
| `Crop` (`cx;cy;w;h`), `CropAspectRatioName` | `crop.rect`, `crop.aspect` | See below. **Approximate.** |
| `Rotation` | `crop.angle_deg`, `crop.quarter_turns` | See below. |
| `Clarity`, `ClarityStructure` | `detail.clarity`, `detail.structure` | 1:1, ±100. **Approximate**: C1's clarity method (`ClarityMethod`) is ignored. Uncalibrated (a 300 px reference can't show it). |
| `DehazeAmount` | `presence.dehaze` | 1:1, ±100, same sign (negative adds haze). **Approximate**; calibrated (below). `DehazeColor` is not mapped. |
| `Vignetting` (`amount\|method\|…`) | `vignette.amount_ev` | The amount is EV at the corners, 1:1, clamped ±3. The midpoint stays Luxtable's. **Approximate**; calibrated. |
| `UsmAmount`, `UsmRadius`, `UsmThreshold` | `detail.sharpen_amount/radius/threshold` | 1:1 (Luxtable uses C1's scales: 0–1000, 0.2–2.5 px, 0–10). **Approximate**: the kernels differ. Uncalibrated. `UsmHaloControl` and `UsmMethod` are not mapped. |
| `NrAmount`, `CnrAmount`, `NoiseDetailsAmount` | `noise.luminance`, `noise.color`, `noise.detail` | Luminance and colour × 0.5 (C1's default 50 = Luxtable's 25), detail 1:1. **Approximate**. No edits in the real data. Luxtable's AI Denoise is never compared or written. |
| `GradationCurve`, `GradationCurveY`, `…Red/Green/Blue` | `curve.rgb`, `curve.luma`, `curve.red/green/blue` | `x,y;x,y` points on 0–1, sorted; `0,0;1,1` is Luxtable's empty curve. **Approximate**: the curves act on each engine's own tone space. Calibrated. |
| `Shadow`, `Highlight`, `TargetShadow`, `TargetHighlight` | `levels.in_black/in_white/out_black/out_white` | Four values (master and the three channels; order **unconfirmed**). Only levels equal on every channel translate, on 0–1. `Midtone` (scale unconfirmed) and per-channel levels are reported as not mapped. No edits in the real data. |
| `ChromaticAberEnabled` | `lens.chromatic_aberration` | 0 = off. |

A tool's value is C1's when the adjustment layer edits it (differs from the default layer), else Luxtable's default. The scales above are approximate, so the look is close, not identical.

What doesn't map is reported per key with a reason:
- levels midtone and per-channel levels;
- colour balance and colour editor;
- black & white;
- noise reduction's surface smoothness and long-exposure cleaning;
- lens (Luxtable applies its own profile) and keystone (C1's units and sign are **unconfirmed**; there are no keystone edits in the real data);
- sharpening halo suppression and method, and the dehaze colour;
- base characteristics (`FilmCurve`, `ICCProfile`);
- film grain and moiré;
- spots;
- IPTC fields;
- `Flip` (encoding **unconfirmed**).

In the real session every edit maps. The dry run reports nothing unmapped.

C1 local adjustment layers (`LocalAdjustmentsLayer`) are not imported. The real session has none.

### Calibration

The references are C1's own renders: the per-variant thumbnails in `CaptureOne/Cache/Thumbnails/<file>.[<variant uuid>].cot`. They are JPEGs of about 300×450 that show the edits and the whole straightened canvas. The process-history JPEGs (`PHS` URLs) were no longer on disk.

Luxtable rendered each image at twice the thumbnail size and downscaled it. The comparison is the mean ΔE76 in Lab. The crop is left out of the tone comparison and checked on its own.

The engine baseline is an unedited image, photo E: ΔE 7.97, with Luxtable 2.8 L brighter.

| Image | Defaults | Before this mapping | With it |
|---|---|---|---|
| photo A (style A, portrait, straightened) | 25.7 | 14.8 | 14.9 |
| photo B (style B) | 16.7 | 14.3 | **9.1** |
| photo C (style A, crop + straighten) | 14.3 | 13.9 | 14.0 |
| photo D (style B, crop + straighten) | 22.4 | 17.7 | **15.3** |

How much worse ΔE gets with only that tool reset (higher = the tool helps):

| Tool | photo A | photo B | photo C | photo D |
|---|---|---|---|---|
| Exposure | +4.70 | +4.02 | +2.22 | +3.40 |
| HDR | −0.42 | +0.63 | +0.04 | +0.33 |
| White balance | +11.16 | +4.11 | +0.02 | +4.01 |
| Dehaze | −0.02 | +2.39 | −0.09 | +0.76 |
| Curve | — | +2.26 | — | −0.19 |
| Vignetting | −0.01 | +0.82 | 0.00 | +1.42 |
| Clarity / structure | −0.02 | +0.04 | −0.01 | 0.00 |
| Sharpening | — | 0.00 | 0.00 | 0.00 |

- Dehaze: 1:1 is the closest scale on both style B images (×0.5, ×1.5 and ×2 are all worse). On style A (amount 4.1) every scale is within ±0.07.
- Vignetting: 1:1 is closest on photo B. ×1.5–2 is closer on photo D, so 1:1 is kept and marked approximate.
- Clarity and sharpening can't be judged at thumbnail size.
- The crop checked against the thumbnail cut to C1's crop: ΔE 8.7 and 10.5, about the engine baseline.

### Rotation and crop

- `Rotation` is in degrees, positive **clockwise**. It includes the EXIF orientation: in the real data, DL −90 ↔ EXIF 8 and DL +90 ↔ EXIF 6, checked on three files.
- The effective rotation splits into quarter turns plus a straighten angle (−45°…45°):
  - quarter turns beyond the EXIF orientation become Luxtable's clockwise `quarter_turns`;
  - the straighten becomes `angle_deg = −straighten`, because Luxtable's positive angle turns content counter-clockwise.
- `Crop` is `cx;cy;w;h`:
  - The centre is in **sensor (unrotated) pixels**. With the size swapped back to sensor axes, all 15 portrait crops in the data fit the 6000×4000 sensor (within 3 %). Read as display coordinates, only 4 of them would fit.
  - The size is in pixels of C1's **displayed** orientation.
- Luxtable converts the centre through the EXIF orientation into the oriented frame. It normalizes by the sensor size, keeps the rectangle inside the frame, and treats a crop covering the whole frame as none.
- C1's coordinates live in its lens-corrected canvas (`ZCANVASSIZE`, 1–4 % off the sensor size). The mapped crop can therefore be off by about that much at the edges. Luxtable's own lens correction and inscribed-crop rule then apply.

## Smart albums

`ZSMARTCRITERIA` is `<MatchOperator Kind="AND|OR">` groups of `<Condition Enabled="YES"><Key>IB_I_BASIC_RATING</Key><Operator>0</Operator><Criterion>5</Criterion></Condition>`.
- Operator 0 is "is". The other operator codes are **unconfirmed**.
- Only "rating is N" translates, and each one is one of Luxtable's built-in smart folders: C1's default "Five Stars" = the built-in "5 stars".
- "All Images" (no criteria) is every photo.
- Anything else is reported with the condition that didn't translate.
- Manual albums become Luxtable albums, named "Group / Album" when nested.

## Import rules (`lt_c1::import`)

- The session must be the in-place session of the same folder: originals are `Capture/<path in the folder>`.
- C1 variants match Luxtable variants by:
  1. the recorded id;
  2. otherwise by position;
  3. otherwise a new Luxtable variant is created, starting from defaults.
- Each field (rating, tag, pick, keywords, and the mapped tools as one unit) merges three ways against what the last import wrote. That record lives in `<store>/.lt/settings/capture-one-import.json`.
  - Current equals C1: in sync.
  - C1 unchanged since the last import: Luxtable's value is kept.
  - Luxtable unchanged since the last import: written.
  - No record and C1 at its default: left alone.
  - No record and Luxtable at its default: written.
  - Otherwise: **conflict**. It's reported and kept unless the caller passes `overwrite_edits`.
- Re-running is idempotent. A lost or corrupt record is safe, because differing Luxtable values then count as edits.
- Albums are recorded by C1 collection id:
  - re-runs add missing members;
  - an album the user deleted is not recreated.
- The import plans everything first, writing nothing. Only a real run then creates variants and writes. A dry run is the same plan without the writes, so the two report the same. That includes a photo saved by a newer Luxtable, which is read-only and gets no new variant.
- A variant the import creates starts from defaults: every field is written, not inherited from the variant it was copied from.
- A photo whose sidecar can't be read is left out with a warning; the rest of the folder imports.
- If the folder's C1 database is a different session (another `ZDOCUMENTUUID`) than the one last imported, album keys (C1 row ids) are not reused. Albums an earlier import made are found by name instead.
- Settings files over 8 MB and databases over 4 GB are not read.
- Cancelling stops the plan; nothing has been written by then.

## Reverting (`lt_c1::revert`)

- Every real import keeps a journal, `<store>/.lt/settings/capture-one-import-undo.json`. It holds:
  - for each organization field it wrote (rating, tag, pick, keywords), the value before and after;
  - for each adjustment *setting* it changed, the value before and after. A setting is a JSON pointer into the adjustments, such as `/exposure/exposure_ev`. Settings the import didn't change are not recorded, so a revert never touches them: HSL, masks, or a vibrance set between two imports;
  - each variant it created, with every field as the import left it (and as first made, until the import's writes land);
  - the albums it created and the album members it added.
- The journal is written before the import changes anything. A created variant or album is recorded right after it is made, because only then does it have an id. An import that fails part-way can therefore still be reverted.
- Later imports fold into the same journal: the oldest "before" stays, the newest "after" wins. A revert therefore returns to the state before the first import since the last revert.
- A revert:
  - restores only fields and settings that still hold what the import wrote; anything changed in Luxtable since is kept and counted;
  - deletes a variant the import created only if every field still equals what the import left (or what it was made with); otherwise it is kept and counted;
  - deletes the albums the import created when they hold only the imported members, and otherwise removes just those members;
  - removes the members the import added to albums that already existed;
  - restores the import record from before, so a later import treats the photos as never imported;
  - deletes the journal.
- The app: File ▸ Revert Capture One Import (asks first), or Revert Import on the result sheet. Both take back every import since the last revert. ⌘Z doesn't undo an import, because the import reloads the session and starts a fresh undo history.

## Reading safely

- The database is copied (with any `-journal`/`-wal`) to a temporary directory and opened there. The copy is opened read-write, so a hot journal is rolled back on the copy. Nothing in the C1 folder is opened for writing or locked, and no `-shm` is created there.
- `.cos` files are read with `File::open`.
- The Luxtable store of a C1 folder is always the fallback store in Application Support, never inside the folder.
- Tests check the C1 folder byte for byte (BLAKE3 of every file, no new files) after dry runs, imports, re-imports and overwrites.

# Capture One application settings

Reverse-engineered from a real Capture One 16.1.3 install, read-only. Its settings folder held seven export recipes (C1's defaults, one with a custom output folder), one workspace, an empty user crop ratio list, and empty `Styles/`, `Presets60/Camera Focus/`, `Keywords/`, `Search Presets/`, `StyleBrushes/`, `Smart Adjustments/` and `Normalizations/` folders. The `.costyle` and `.copreset` formats were confirmed from the styles and presets bundled with the Capture One 23 app. The parser lives in `lt_c1::settings`, and its fixtures are synthetic (`crates/lt-c1/fixtures/settings/`).

## Where they are

```
~/Library/Application Support/Capture One/
  Recipes143/<name>.xml              export recipes (143 = format version)
  Workspaces/<name>.plist            tool layouts (XML property list)
  Aspect Ratios 8.2/User Aspect Ratios.xml
  Styles/**/*.costyle                styles (sub-folders allowed); *.costylepack are packs
  Presets60/<tool>/*.copreset        presets, one folder per tool
  Keywords/                          keyword libraries
  StyleBrushes/ Smart Adjustments/ Normalizations/ Search Presets/   reported, not imported
~/Library/Preferences/com.captureone.captureone<N>.plist             preferences (binary)
```

- A family of versioned folders (`Recipes100`, `Recipes143`) is read from its highest version only.
- Files over 8 MB aren't read. Hidden files are ignored, and links are not followed.
- A folder that can't be read (or a category name that is a file) is skipped with a reason. The other categories still read.
- Nesting is checked before anything is built, because a deeply nested file would overflow the stack and abort the app. XML is first tokenized with `xmlparser` (a real lexer, so CDATA, comments and processing instructions can't hide markup; input it can't tokenize is refused). Property lists (XML and binary) are read as an event stream first (plist's `stream::Reader`, so plist is pinned to an exact version). Anything nested more than 256 levels, or a plist expanding to more than 4 million values, is skipped with a reason. The session import's `.cos` files and smart-album criteria get the same XML check.
- A keyword database must hold a real `ZKEYWORD` table, not a view. Its queries stop on cancel or after 5 s.
- Cancelling the read stops it between categories and inside a keyword database query.
- Nothing is written there. A keyword library database is copied to a temporary folder before it is opened.

## Export recipes (`Recipes143/*.xml`)

`<OR>` holding `<E K="…" V="…"/>` entries. Each recipe becomes one Luxtable export recipe, keyed by its file name.

| C1 key | Luxtable | Notes |
|---|---|---|
| `name` | name | The file name when empty. |
| `OutputFileFormat` | format | 1 JPEG, 2 TIFF, 5 PSD → TIFF 16-bit (approximate), 8 JPEG QuickProof → JPEG (approximate). Other codes become JPEG, reported, so size, naming and folder still carry over. The codes are inferred from C1's default recipes, whose names give their format; in the real data "TIFF - A3 Print" has code 1, a JPEG. |
| `jpegQuality` | quality | Clamped to 1…100. |
| `tiffBits` | — | 8-bit TIFF is approximate: Luxtable writes 16-bit. |
| `scaleMethod` + `*Scale` | size | 0 fixed (`fixedScale amount`: a factor, 1 = full size), 1 width, 2 height, 3 dimensions (`boundingBoxScale`), 4 width × height, 5 long edge, 6 short edge. 0, 1, 3 and 5 were seen; 2, 4 and 6 are inferred. Width, height and dimensions become an edge (approximate: the larger dimension becomes the long edge). |
| `unit` in a scale | pixels | 0 px, 3 cm (confirmed by the A3 recipe: 42 × 29.7), 2 in and 4 mm (inferred). Converted at `resolution` (`resolutionUnit` 1 px/in, 2 px/cm), default 300 dpi. |
| `neverUpscale` | allow upscale = not set | C1 enlarges unless ticked. |
| `outputProfile` | colour space or ICC profile | A name with "sRGB" → sRGB; "Display P3" → Display P3; any other (e.g. `Adobe RGB (1998).icm`) is looked up among installed profiles by name or file name and embedded (relative colorimetric with black point compensation). Not installed → sRGB, reported. |
| `SharpeningOutputControl`, `SharpeningOutputAmount` | — (no output sharpening) | **Unconfirmed**: the codes don't follow the recipe names. Code 2 is on the full-size, TIFF and PSD recipes, and 0 on the web ones, which still have an amount. So output sharpening is never carried over, and every recipe with a mode or an amount says so ("Capture One output sharpening (mode N, amount A) not carried over"). |
| `fileName` | file name template | `<TokenName version="2"><Token Type="1"/></TokenName>`. Type 1 is Image Name → `{name}` (confirmed by the preferences' `{name: "Image Name", tag: 1}`). Literal text is kept, with braces escaped; other token types are reported by number. An empty result falls back to `{name}`. |
| `destinationSubFolder` | sub-folder | Same tokens; literal text only. |
| `outputRootFolder`, `outputRootFolderPath` | destination | 0 the session's Output folder, 3 the chosen folder (an absolute path; anything else is reported and exports go to the Output folder). Other codes are reported. |
| `MetadataIncludeCameraData`, `MetadataIncludeGPS` | include metadata, strip GPS | Copyright, keywords and other IPTC are separate switches in C1; when they differ from camera data, that's reported as approximate. |
| `WatermarkType` and `Watermark*` | watermark | 0 none. Text (`WatermarkText`, colour `red:…;green:…;blue:…`, opacity, font) or image (`WatermarkImageFilePath`); the placement (0…1 across and down, the vertical direction **unconfirmed**) becomes the nearest of nine anchors. Sizes are approximate. |
| `ProcessedFileOverwriteAction`, `cropExportMethod`, `tileDimension`, `skipThumbnail`, `destinationSubName`, `MetadataSelectedLibrariesWhitelist` | — | Reported when not at their defaults. |
| `sortIndex` | order | |
| `enabled` | — | Every user recipe has 1. Which recipes are ticked for export comes from the preferences' `enabledRecipeNames`. |

## Workspaces (`Workspaces/*.plist`)

An XML property list. Per window, `com.phaseone.captureone.<window>.tools` maps tab ids to `[[tool ids], [collapsed indices], [per-tool options]]`, and `tabConfiguration` lists the tabs shown, in order. Only the main window (`sessionwindow`) is imported. The viewer and live-view windows are reported.

- The second array lists tools C1 shows collapsed. This is **inferred**: it holds Annotations, Overlay and Moiré, which C1 folds by default.
- Each workspace becomes a named Luxtable workspace, "<file> (Capture One)". The current layout isn't changed.
- Tabs: `QuickToolTab` → Favorites, `OrganizeToolTab` → Library, `Color`, `Exposure`, `Lens`, `Composition`, `Details`, `LocalAdjustments` → Layers, `MetaData` → Metadata, `Export`/`Output` → Output. Any other tab becomes a custom tab named after it ("Capture", "Black & White"). A second tab mapping to the same built-in tab becomes a custom tab too.
- Tools (C1 id → Luxtable): `Library` → Smart Folders, Sessions, Albums; `MetadataFilters` → Filters; `Styles`; `WhiteBalance`; `Exposure`; `ShadowHighlight` → HDR; `Levels`; `Curves`; `Clarity`; `Vignetting`; `Sharpening`; `Noise`; `Film Grain` → Effects; `ColorBalance`; `SelectiveColorControl` → Color Editor; `LocalAdjustments` → Layers; `Crop`, `Rotation` → Crop & Straighten; `Perspective` → Lens Correction (Luxtable's keystone); `Metadata`; `Keywords`, `KeywordLibrary` → Keywords; `ExportDialogRecipeList`, `OutputCrossRecipeTokensInspectorTool` → Export.
- Tools with no Luxtable tool are reported: Histogram (Luxtable shows it above every tab), BaseCharacteristics, StyleBrushes, BlackAndWhite, Normalize, LensColorCorrections, Moire, Navigator, Focus, Overlay, Grid, Annotations, and the camera tools. A tab left with no tools is dropped and reported.
- A tool's first appearance decides whether it starts collapsed.

## User crop ratios (`Aspect Ratios 8.2/User Aspect Ratios.xml`)

An XML property list, `{AspectRatios: [...], SelectedAspectRatioUID}`. An entry is keyed by its UID, else by its name and ratio; a repeat is reported. The real list is empty, so an entry's keys are **unconfirmed**. Luxtable reads a dictionary with a name (`Name`) and either `Width` + `Height` or `Ratio` (width ÷ height), or a `"W:H"` string, in any letter case.
- A ratio equal to one of Luxtable's chips (1:1, 4:5, 5:4, 3:2, 2:3, 16:9, 9:16) needs no import.
- Others become custom crop ratios, shown as chips under the crop panel's aspect ratios. A chip crops once to the largest centred box of that ratio. The crop then stays free, because lt-core's aspect lock keeps only the built-in ratios.

## Styles and presets (`.costyle`, `.copreset`)

```xml
<SL Engine="1100">
  <E K="Contrast" V="9.56" />
  <E K="ColorBalanceMidtone" V="1;0.985;0.965" />
  <E K="Name" V="Fashion" />
  <E K="UUID" V="BFCE8278-…" />
  <E K="StyleSource" V="Exposure" />     presets only
</SL>
```

- The keys are a `.cos` adjustment layer's. They go through the session import's mapping (`map_variant`, above), so the numbers match a session import of the same values.
- Each becomes a Luxtable preset of the groups it sets. The values are absolute, as applying a C1 style sets them.
- Styles have no photo, so white-balance multipliers can't be converted and are reported (temperature and tint are carried over). Rating, colour tag and keywords in a style are reported.
- A style with nothing that maps isn't imported. Style packs (`.costylepack`) are reported: install them in Capture One first.
- A preset's tool folder (`Presets60/Exposure/`) groups it in the sheet.

## Keyword libraries (`Keywords/`)

The real folder is empty, so the format is **unconfirmed**. Two forms are read:
- a SQLite file with C1's `ZKEYWORD` table (`ZNAME`, `ZPARENT`, nested set `ZLEFT`/`ZRIGHT`), as in a session database;
- a Lightroom keyword text file, which C1 imports: one keyword per line, tab-indented under its parent, `[Category]`, and `{synonym}` (left out).

Luxtable's keyword list is flat. It holds every keyword name, parents included, once each, ignoring case. The Keywords panel offers the list to add to the selection.
- A database's `-wal`/`-journal` files are copied with it, and aren't listed as libraries.
- Each library is one import item. It adds the keywords not already in the list, and the journal records exactly those, so a revert takes out only what it added.

## Preferences (`com.captureone.captureone<N>.plist`)

A binary property list, read from the newest `<N>`. Only a short list is carried over:

| Key | Luxtable |
|---|---|
| `ImportNamingFormatTokensForSession` | Import rename template. `{name, tag}` pairs; tag 1 (Image Name) → `{original}`. Other tags are reported by name (Camera Counter 11, Name 12 were seen). |
| `proofProfile143` | Soft-proof profile. `1;Adobe RGB (1998).icm`: the part after `;` is matched among installed profiles. |
| `enabledRecipeNames` | Which imported recipes are ticked. |
| `KeyboardShortcutSet` | "Default": C1's rating keys 0–5 are Luxtable's defaults too, so there's nothing to carry over. A custom set is reported. |

`NewSessionLocation`, `Workspace` (the active one), `PreviewSizeInPixels`, `viewerBackgroundColor`, `BeforeAfterMode` and `CreateDefaultCatalogOnStartup` are reported as not carried over. Everything else is window state, recent sessions and per-session view settings.

## Import rules (the app's `C1SettingsImporter`)

- Reading plans everything and writes nothing: the sheet shows, per category, each item and what would happen to it, with notes on what is approximate or left out. Categories are ticked separately.
- Items are keyed by their C1 source (file, entry or keyword) and land in the app's own stores: export recipes, saved workspaces, presets, custom crop ratios, the keyword list, the rename template, and the soft-proof profile.
- The journal is `<support>/capture-one-settings-import.json`. It holds, per item, the Luxtable id, the C1 file's fingerprint, a snapshot of what was written and, for a preference, its value before. It's saved after each write, so an interrupted import can still be reverted. If an item's entry can't be saved, the item is taken out again (a preference is put back) and the import stops.
- A style whose mapped values can't be read isn't importable. It never becomes a preset that would reset the tools it names.
- Re-importing:
  - not imported before and Luxtable has one by that name (or a preference already at that value): left alone;
  - imported before but deleted in Luxtable: not recreated;
  - changed in Luxtable since: kept;
  - unchanged in Luxtable but changed in C1: updated in place;
  - otherwise up to date.
  
  A recipe's "ticked for export" is the user's to change and doesn't count as a change.
- Revert (in the sheet) takes back every import since the last revert. Items still as imported are removed, preferences go back to their earlier values, and anything changed since is kept and counted. It then deletes the journal.
