# Importing from a memory card

Insert a card and choose **File ▸ Import from Card…** (`⇧⌘I`) or the **Import** button. The import sheet lists the photos on the card. You can also import from any folder (**Choose Folder…**), or **Browse Card** to look at a card read-only without copying anything.

![An import in progress (demo mode simulates the card).](../screenshots/import-card.png)

## Options

- **Session:** a new session (named for you, or by you) or an existing one.
- **Subfolder** inside the session's Capture folder, with tokens such as `{date}`, `{time}`, `{camera}` and `{text}`.
- **Rename Files** with a template: `{original}`, `{date}`, `{time}`, `{seq}`, `{seq4}`, `{camera}`, `{text}`.
- **Skip duplicates:** files already in any session are not copied again.
- **Backup Copy:** a second destination that receives the same verified files.
- **Re-read the card after copying:** reads every file from the card a second time and compares it. This catches a card reader that returned bad data once, and takes twice as long.

## What makes it safe

- Files are copied in parallel, and each copy's BLAKE3 checksum is verified before the import counts it as done.
- Nothing is ever deleted from the card. When the import finishes you can eject the card safely from the sheet.
- If an import is interrupted (the card is pulled, the Mac sleeps), Luxtable offers to resume it the next time, and unfinished copies are never mistaken for complete ones.
- Metadata presets (for example your name and copyright) can be applied to every import, and the camera's own artist and copyright fields are read.

Photos from Capture One, Lightroom Classic, Aperture and iPhoto come in another way: see [Coming from other apps](14-migrate.md#coming-from-other-apps).
