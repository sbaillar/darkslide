# Coming from other apps

Darkslide reads other apps' files and never modifies them. Each import starts with a dry run that shows what will change, what is approximate and what is left out, with a checkbox per category, and each can be reverted. Re-importing updates only what is still as imported, never your own changes.

## Capture One

- **File ▸ Import Capture One Edits…**: the ratings, colour tags, keywords, picks and supported adjustments of a Capture One session.
- **File ▸ Import Capture One Settings…**: export recipes, the tool workspace, crop ratios, styles and presets, keyword libraries, import naming and the soft-proof profile.
- **Revert Capture One Import…** takes an import back.

## Lightroom Classic

- **File ▸ Import Lightroom…**: a Lightroom Classic catalog (`.lrcat`, read from a copy), a folder of XMP sidecars, or Lightroom and Camera Raw presets. Ratings, labels, flags, keywords, titles, captions, collections, virtual copies (as variants), crop, and the develop settings Darkslide has an equivalent for.
- **Revert Lightroom Import…** takes an import back.

## Aperture and iPhoto

**File ▸ Import Aperture or iPhoto Library…** reads an Aperture 3 library or an iPhoto library (iPhoto 6 and later). Choose the library in the sheet; the dry run then lists what will come in.

- **What comes over:** ratings, rejects and hidden photos, flags, colour labels, keywords, titles, captions, places, and projects, events and albums.
- **Edits that map:** exposure, recovery, contrast, definition, saturation, vibrancy, highlights and shadows, white balance on RAW files, straighten, crop and black & white.
- **What doesn't:** levels, curves, retouching, brushed-in adjustments, sharpening, noise reduction, vignettes, faces and smart albums. The dry run lists every one it finds.
- An older iPhoto library kept edits only as rendered copies, so its originals come in unedited.
- The library is only read. Its originals open in place as a session, with Darkslide's data kept outside the library. Originals on a disk that isn't connected are listed until you connect it.
- **File ▸ Revert Aperture or iPhoto Import…** takes the import back. It never touches a Lightroom import, and changes you made in Darkslide afterwards are kept.
- Photos libraries aren't supported. `docs/APPLE_FORMAT.md` has the details of the mapping.

## Familiar keys and layout

Settings ▸ Familiar also moves panels, keyboard shortcuts and names to match Lightroom Classic or Capture One.
