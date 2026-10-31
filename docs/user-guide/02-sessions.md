# Sessions

Everything in Luxtable happens in a session: a folder of photos and their sidecars. Each import usually becomes a new session, but you can import into an existing one, or open any folder of photos as it is.

![Your edits are written to the photo's sidecar. The indexes that make browsing and Library fast are built from the sidecars and can be rebuilt.](img/sidecars.svg)

## New, open and close

- **New Session** (**File ▸ New Session…**, `⇧⌘N`): give it a name and a location. It is created with Capture, Selects, Output and Trash folders, ready for an import or for photos dragged in.
- **Open** (`⌘O`): choose a recent session, or **Choose Folder…** to open any folder. **File ▸ Open Recent** lists recent sessions; one on a drive that isn't connected is shown greyed out with the reason. **Browse All Sessions in Library** in the same sheet opens the [Library](05-library.md#library-mode) instead.
- **Close Session** (`⌥⌘W`): closes it. Nothing is deleted, and it stays in the Sessions list.

## Folders opened in place

When you open a folder that isn't a Luxtable session, Luxtable works on it in place: it never moves, renames or changes your files, and writes only its own sidecars and index. Renamed or edited files are recognised again when you reopen the folder. Commands that would move files, such as moving rejects to the session Trash, are turned off for such folders and say why.

## What is in a session folder

```text
My Session/
  Capture/      originals
  Selects/      picks moved out of Capture
  Output/       exports, unless a recipe says otherwise
  Trash/        the session trash (restorable)
  .lt/          sidecars (the source of truth) and the rebuildable index
```

Because a session is self-contained, you can move it to another drive, copy it to another Mac, or archive it, and it opens the same way.

A folder opened in place has just one `.lt/` folder at its top, with the same contents; your photos stay where they are.

## Disconnected drives and missing files

- Sessions on a drive that isn't connected stay listed, marked as disconnected. Nothing is hidden or removed.
- If an original has been moved or renamed, Luxtable finds it again by its content. **Locate…** and **Locate Folder…** point it to files you moved yourself.
- **Remove Missing** is only offered when Luxtable can prove the file is really gone from the right disk.
