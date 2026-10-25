# Plug-ins

Plug-ins add looks, styles, voice phrases, and export steps. Manage them in Settings ▸ Plugins.

- **Install:** drop a `.dsplugin` folder on the pane, or **Install…**. The sheet shows what the plug-in says it does before anything is installed. Turn each plug-in on or off with **Enabled**.
- **Nothing is granted at install.** Looks, styles and voice phrases are data only. An export step is WebAssembly code that runs sandboxed with time and memory limits; code from an unsigned or untrusted author waits for your approval and never gets file, network or account access silently. If a plug-in's files change, it waits for your approval again.
- Darkslide's own looks and styles are a bundled plug-in. Developers can **Add Development Folder…** to work on a plug-in in place; see `docs/PLUGINS.md`.

![Settings ▸ Plugins.](../screenshots/settings-plugins.png)

## Optional integrations

Four integrations are built into Darkslide as bundled plug-ins, so it stays out of services you don't use. Turn each on or off in **Settings ▸ Plugins ▸ Integrations**; its own Settings pane appears while it is on.

| Integration | What it does | Where to set it up |
|---|---|---|
| **Archive** | Copies sessions to a folder or to S3-compatible object storage in the background; never deletes from the archive. Setup offers it. | Settings ▸ Archive |
| **Amazon Photos** | Copies originals into the folder the Amazon Photos app backs up. | Settings ▸ Amazon Photos |
| **Immich** | Developed copies, XMP sidecars and rule albums for an Immich server that indexes the archive (needs the archive, to a folder). | Settings ▸ Immich |
| **Google Photos** | Uploads a recipe's exports into a Google Photos album. | Settings ▸ Google Photos, then the export sheet |

- **Defaults:** each starts off. If you had already set one up before they became optional, it stays on after the update.
- **Turning one off** hides its Settings pane, menus, badges, panels, toolbar button and export-sheet section, and stops its background work (anything already running finishes first).
- Its settings, sign-ins, keys and files are kept, and turning it back on picks up where it left off.
- They can be turned off but not removed.

See [Archive, Immich and Amazon Photos](11-archive.md#archive-immich-and-amazon-photos) and [Google Photos](10-export.md#google-photos) for how each works.
