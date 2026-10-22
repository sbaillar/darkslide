# Lightroom Classic and Camera Raw formats (as Darkslide reads them)

Darkslide imports edits and library data from Adobe Lightroom Classic and Camera Raw. This file records what is read, how each setting maps, and **what has not been confirmed**.

**Provenance.** No Lightroom installation or catalog was available while this was written. Everything below comes from Adobe's public documentation (slider ranges, the XMP `crs:` namespace) and from what open-source catalog and XMP readers document. Nothing was checked against a real `.lrcat`, so every schema detail and every sign or scale that Adobe does not publish is marked **UNCONFIRMED**. The reader is built so an unconfirmed piece that turns out wrong costs that one field (with a warning in the report), never the whole import: every optional table or column is feature-detected, every column is read by its actual SQLite type, and the tests use synthetic catalogs and sidecars built in the test code. No real catalog is committed.

The code is `crates/ds-lr`: `catalog.rs`, `sidecars.rs`, `preset.rs`, `xmp.rs`, `lua.rs` (readers), `map.rs` and `map_local.rs` (settings → Darkslide), `import.rs`, `journal.rs`, `state.rs`, `matching.rs`, `groups.rs` (the idempotent import).

## Sources

| Source | What is read |
|---|---|
| Catalog (`.lrcat`) | Photos with their paths, rating, pick, colour label, rotation, virtual copies, develop settings, keywords, title/caption/copyright, collections. |
| XMP sidecars | A folder of `.xmp` files next to the originals (Lightroom with "Automatically write changes into XMP", Camera Raw and Photoshop write them), plus the XMP packet inside DNG files that have no sidecar. |
| Presets | Lightroom/Camera Raw presets: `.xmp` (settings in the `crs:` namespace) and the older `.lrtemplate` (a Lua table). A file or a folder of them. |

The catalog is never opened in place: it is copied (with a `-wal` or `-journal` beside it) into a temporary directory and the copy is opened, so nothing in the user's folder is locked or written, and a transaction Lightroom left open is rolled back on the copy. Sidecars and presets are read with plain `File::open`.

## The catalog (SQLite) — UNCONFIRMED in detail

| Table | Used columns |
|---|---|
| `AgLibraryRootFolder` | `id_local`, `absolutePath` (ends with `/`). |
| `AgLibraryFolder` | `id_local`, `pathFromRoot` (like `2020/Trip/`, empty for the root), `rootFolder`. |
| `AgLibraryFile` | `id_local`, `baseName`, `extension`, `folder`. The original is `absolutePath + pathFromRoot + baseName + "." + extension`. |
| `Adobe_images` | `id_local`, `id_global` (the photo's stable key), `rating` (NULL = unrated, 0-5), `pick` (1 picked, -1 rejected), `colorLabels`, `orientation`, `masterImage` (non-NULL = virtual copy of that image), `copyName`, `captureTime`, `rootFile`. |
| `Adobe_imageDevelopSettings` | `image`, `text`: `s = { Key = value, … }`, a Lua table. |
| `AgLibraryKeyword`, `AgLibraryKeywordImage` | `name`, `parent`, `includeParents`; `image`, `tag`. |
| `AgLibraryCollection`, `AgLibraryCollectionImage` | `name`, `parent`, `creationId`; `collection`, `image`, `positionInCollection`. |
| `AgLibraryIPTC`, `AgHarvestedIptcMetadata` | caption, copyright, title: found by listing each table's columns. |
| `Adobe_AdditionalMetadata` | `image`, `xmp`: a photo's XMP packet, the fallback source for title, caption and copyright. |
| `Adobe_variablesTable` | `name`, `value`: catalog id and version, when present. |

- **Orientation codes** (UNCONFIRMED, from public readers): `AB` = 1 (normal), `BA` = 2, `CD` = 3 (180°), `DC` = 4, `BD` = 5, `BC` = 6 (90° clockwise), `AC` = 7, `DA` = 8 (270° clockwise).
- **Collections:** `creationId` `com.adobe.ag.library.collection` is a plain collection, `…smart_collection` a smart one (a saved search; Darkslide can't read Lightroom's rules and reports it as left out), `…group` a collection set (only its name is used, in the album's display path). Other kinds and `systemOnly` rows are ignored.
- **Keywords:** the keyword itself plus its ancestors unless its `includeParents` is 0 (the column's absence means "include"); the unnamed root is skipped. Darkslide keywords are flat, so the hierarchy is flattened to these names.
- **Title, caption, copyright:** from `AgLibraryIPTC` or `AgHarvestedIptcMetadata` when they have the column, else from the XMP in `Adobe_AdditionalMetadata`. Which of these a given Lightroom version fills is UNCONFIRMED.
- **Windows catalogs.** Paths in a catalog written on another operating system don't exist here. Matching then falls back to content (if the file is reachable) or to name plus capture time (below).

### Develop settings text

A small parser reads the Lua-table syntax Lightroom writes (`s = { Exposure2012 = 0.5, ToneCurvePV2012 = { 0, 0, 255, 255 }, … }`). It never evaluates anything: only tables, strings, numbers, booleans, `nil` and comments are accepted, a function call or anything else is an error for that photo, and a table with string keys becomes a table, positional entries a list.

## XMP

Settings are read from `rdf:Description` attributes and child elements in the `crs:` namespace (`http://ns.adobe.com/camera-raw-settings/1.0/`).

| XMP | Read as |
|---|---|
| `xmp:Rating` | 0-5; **-1 = rejected** (pick Rejected, rating 0); absent = not touched. |
| `xmp:Label` | Red, Orange, Yellow, Green, Blue, Pink or Purple (any case). Other label text ("Approved") has no colour and is reported, not imported. |
| `dc:subject`, `lr:hierarchicalSubject` | Keywords (the first, else the leaves of the second). |
| `dc:title`, `dc:description`, `dc:rights` | Title, caption, copyright (`x-default`, else the first alternative). |
| `tiff:Orientation` | The orientation Lightroom shows the photo with. |
| `exif:DateTimeOriginal` | Capture time, used only to confirm a match by name. |
| `crs:*` | Develop settings; sequences (curves, masks) become lists. |
| `crs:Name`, `crs:Group`, `crs:UUID` | A preset's name, group and id. |

A DNG's embedded XMP is read from TIFF tag 700 (bounded reads). A sidecar is paired with the original of the same name next to it.

## Mapping

Lightroom writes every setting it knows, defaults included, so **only a value that differs from Lightroom's own default is an edit** (for example Sharpness 40, Radius 1.0, Detail 25, ColorNoiseReduction 25, post-crop vignette midpoint 50 and feather 100, grain size 25 and frequency 50, lens profile scales 100; everything else defaults to 0). A setting at its default leaves Darkslide's own default in place.

Where the two scales differ, the value is calibrated from **Adobe's published slider ranges**, never guessed: Exposure ±5 EV, the 2012 tone sliders and Texture/Clarity/Dehaze/Vibrance/Saturation ±100, Tint ±150, Sharpening 0-150, Radius 0.5-3.0, Edge Masking 0-100, noise sliders 0-100, post-crop vignette ±100. When Adobe publishes no more than a range, the range is spread over Darkslide's (marked *spread*). A scale that makes Lightroom's default land on Darkslide's default is marked *default-matched*.

"Exact" means the same meaning and scale. Everything else is **approximate**: the two engines differ, so the look is close, not identical, and the dry run says so per setting.

### Organisation

| Lightroom | Darkslide | Fidelity |
|---|---|---|
| Rating (0-5) | rating | Exact |
| Colour label | colour tag (7 colours) | Exact; labels without a colour are reported |
| Pick / reject | pick state | Exact |
| Keywords | keywords (flat) | Exact (hierarchy flattened) |
| Title, caption, copyright | title, caption, copyright | Exact |
| Collections | albums | Exact (smart collections are left out and listed) |
| Virtual copies | extra variants of the master's photo | Exact |

### Develop

| Lightroom (key) | Darkslide | Fidelity and notes |
|---|---|---|
| `CropLeft/Top/Right/Bottom`, `CropAngle`, `HasCrop` | crop rectangle and straighten | Exact without an angle. With an angle: **approximate**. The rectangle's meaning for a rotated crop is **UNCONFIRMED** (it is read as the crop box in the unrotated frame) and so is the angle's sign (a positive angle is taken as the crop turned clockwise = content counter-clockwise, as Darkslide's). |
| user rotation (catalog `orientation`, XMP `tiff:Orientation`) | quarter turns | Exact for rotations; a mirrored orientation that differs from the file's can't be expressed and is reported. The crop rectangle is converted from Lightroom's rotated frame back to the EXIF-oriented one. |
| `Temperature`, `Tint`, `WhiteBalance` | Custom white balance (Kelvin, tint) | Approximate (tint scale **UNCONFIRMED**). "As Shot", or a value equal to the camera's, is no edit. Kelvin is only meaningful for raw files: a non-raw file's relative scale is dropped. |
| `Exposure2012`, `Contrast2012` | exposure, contrast | Approximate, 1:1 |
| `Highlights2012` | HDR highlights | Approximate; **sign flipped** (Lightroom's negative recovers, Darkslide's positive does) |
| `Shadows2012`, `Whites2012`, `Blacks2012` | HDR shadows, whites, blacks | Approximate, 1:1 |
| `ToneCurvePV2012` (+ `Red`, `Green`, `Blue`) | RGB curve, channel curves | Approximate: 0-255 points onto 0-1, sorted; the identity is no edit |
| `Texture`, `Clarity2012`, `Dehaze` | texture, clarity, dehaze | Approximate, 1:1 |
| `Vibrance`, `Saturation` | vibrance, saturation | Approximate, 1:1 |
| `HueAdjustment*`, `SaturationAdjustment*`, `LuminanceAdjustment*` (Red, Orange, Yellow, Green, Aqua, Blue, Purple, Magenta) | the eight colour-editor bands (Aqua = cyan) | Approximate; hue ±100 → ±30° (*spread*), saturation and lightness 1:1 |
| `ColorGrade{Shadow,Midtone,Highlight,Global}{Hue,Sat,Lum}`; older `SplitToning*` | colour-balance wheels | Approximate (hue in degrees, saturation and luminance /100). Blending and balance are dropped. |
| `ConvertToGrayscale` | saturation -100 | Approximate; the per-colour mix is dropped |
| `Sharpness` | sharpening amount | Approximate; ×3.5 (*default-matched*: 40 → 140) |
| `SharpenRadius` | sharpening radius | Approximate; ×0.8 (*default-matched*: 1.0 → 0.8) |
| `SharpenEdgeMasking` | sharpening threshold | Approximate; 0-100 → 1-10. **UNCONFIRMED** that the two behave alike. |
| `LuminanceSmoothing`, `ColorNoiseReduction`, `LuminanceNoiseReductionDetail` | noise reduction luminance, colour, detail | Approximate, 1:1 (Lightroom's colour default 25 and detail default 50 are Darkslide's) |
| `PostCropVignetteAmount`, `…Midpoint` | vignette (EV at the corners), midpoint | Approximate; ±100 → ±3 EV (*spread*), midpoint /100 |
| `GrainAmount`, `GrainSize`, `GrainFrequency` | grain amount, size, roughness | Approximate, 1:1 |
| `LensProfileEnable`, `AutoLateralCA` | lens correction on/off, chromatic aberration | Exact when off (on is Darkslide's default) |
| `LensProfileDistortionScale`, `LensProfileVignettingScale` | distortion strength, light falloff | Approximate; /100 and /200 (*default-matched*) |

Dropped, with a note in the report: `SharpenDetail`, `LuminanceNoiseReductionContrast`, `ColorNoiseReductionDetail/Smoothness`, the post-crop vignette's feather, roundness, style and highlight contrast, the parametric tone curve, `Perspective*` and Upright (no calibrated equivalent), manual lens distortion and vignetting, defringe, the chosen lens profile (Darkslide picks its own), the camera profile (Darkslide keeps its own base look), the process-version-2010 tone sliders (`Exposure`, `Contrast`, `Brightness`, `FillLight`, `Shadows`, `HighlightRecovery`: they mean something else), and any other non-default slider nothing maps.

### Local adjustments (masks) — UNCONFIRMED in several details

Each correction becomes a layer named `Lightroom: …` with the mapped sliders: `LocalExposure2012` (exposure), `LocalContrast2012`, `LocalHighlights2012` (sign flipped), `LocalShadows2012`, `LocalWhites2012`, `LocalBlacks2012`, `LocalClarity2012`, `LocalTexture`, `LocalDehaze`, `LocalSaturation`, `LocalSharpness`. `CorrectionAmount` is the layer opacity and `CorrectionActive` its enabled flag. A local temperature or tint is dropped (the relative white balance scale is not calibrated), as is any other slider.

| Lightroom | Darkslide | Notes |
|---|---|---|
| `GradientBasedCorrections`, `Mask/Gradient` (`ZeroX/Y`, `FullX/Y`) | linear gradient (full at `Full`, off at `Zero`) | Coordinates are fractions of the whole uncropped photo as Lightroom shows it (**UNCONFIRMED**), converted to the EXIF-oriented frame. |
| `CircularGradientBasedCorrections`, `Mask/CircularGradient` (`Top/Left/Bottom/Right`, `Angle`, `Feather`) | radial gradient | Radii are scaled to the long edge, the angle follows the frame's rotation. Which side the effect is on is **UNCONFIRMED**: for the classic list `Flipped` false is read as "outside" (mask inverted); in the masking model `MaskInverted` is used. |
| `PaintBasedCorrections`, brush components (`Dabs`: `m x y` starts a stroke, `d x y` continues it; `Radius`, `Flow`, `CenterWeight`) | brush strokes | The dab format and the radius' unit are **UNCONFIRMED**. |
| `Mask/Image` with `MaskSubType` 1 / 2 / 3 | AI subject / sky / background mask | The subtype numbers are **UNCONFIRMED**. Darkslide makes its own AI masks when the photo is rendered; Lightroom's bitmap is not used. |
| other `Mask/Image` (people, objects, a brush bitmap), `Mask/RangeMask`, luminance or colour ranges | none | Dropped with a note. |

Components combine with `MaskBlendMode` 0 add, 1 subtract, 2 intersect (**UNCONFIRMED**). One inversion applies to a whole mask: components with different inversions are reported. At most 64 corrections and 20,000 brush points per photo are read.

## Matching photos

A photo is matched to a Darkslide photo of the open session in this order, and the report counts each:

1. **Path.** For a session opened in place, the original's path under the session's folder (exact, then case-insensitive, then after resolving symlinks such as `/var` → `/private/var`).
2. **Content.** If the file is readable at the recorded path: its BLAKE3 hash against the session's photos of the same size (a photo not hashed yet is hashed on demand, read-only). At most 5,000 files are hashed per import.
3. **Name and capture time.** The same file name (any case) and the same capture time, with exactly one candidate.

A photo with several candidates is ambiguous: it is listed and not imported. Photos matched by none are listed per folder, so the app can offer to open that folder as a session and import there.

## Import behaviour

- **Dry run.** The report lists, per category, how many photos would change, which Lightroom settings map (exactly or approximately), which are dropped and why, per-photo notes, photos not found, and conflicts. Nothing is written, not even the state file.
- **Idempotent, three-way merged.** Each field (rating, colour tag, pick, keywords, title, caption, copyright) and each develop category (crop, tone, white balance, colour, detail, effects, lens, local adjustments) is compared across Darkslide's current value, Lightroom's value and what the last import wrote (`<store>/.ds/settings/lightroom-import.json`). Re-running updates instead of duplicating; a field changed on both sides is a conflict and is kept unless the user chooses to overwrite.
- **Categories.** The user picks which categories to bring over. A category left out is neither touched nor recorded.
- **Local adjustments** only ever replace layers named `Lightroom…`: the user's own layers are never touched.
- **Revert.** Every real import is journaled before it writes (`<store>/.ds/settings/lightroom-import-undo.json`). "Revert Lightroom Import" puts back each field and setting that still holds what the import wrote, removes variants and albums the import created if unchanged, and keeps anything changed in Darkslide since.
- **Presets** are not journaled: they are added to the preset store as absolute presets (existing names are skipped unless the user confirms), and are removed by deleting them.

## Hostile input

A catalog, sidecar or preset can come from anywhere, so each reader is bounded:

- **XML:** nesting is counted with a real tokenizer (`xmlparser`) before the document is parsed (limit 256), and files over 16 MB are refused.
- **Lua:** input over 4 MB, nesting over 64 levels, more than 200,000 values, or numbers that aren't finite (`1e999`) are refused for that photo; the rest of the catalog still reads.
- **SQLite:** only the copy is opened; only real tables are queried (a view could run any query, including an endless recursive one); every query is read-only; the whole read has a 60 s limit enforced by a progress handler, a watcher thread interrupts a running statement on cancel, files over 8 GiB are not copied, and more than 2,000,000 photos are not read. Wrong-typed columns read as "not set". A synthetic catalog of a million photos reads in a few seconds.
- **Folders:** at most 100,000 entries and 8 levels, symbolic links are not followed.

## Not verified

Things that need a real Lightroom catalog, sidecar or preset to confirm: every item marked UNCONFIRMED above; which tables and columns a given Lightroom Classic version fills (title, caption, copyright); `.lrtemplate` presets written by old versions; sidecars from Camera Raw in Photoshop; the Lightroom version string. Where the guess is wrong the import reports a warning or maps nothing; it doesn't write wrong data without saying so, but an unconfirmed *scale or sign* (marked above) can import a slightly different look, which is why those settings are labelled approximate.
