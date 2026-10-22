# Plug-ins and extensions

Status: **v1 built** (2026-10-10), revised after the design review (`H1`–`H9` and the v1 MEDs are folded in; the rest is the v2 list at the end). "What v1 ships" lists what works today; the rest is the plan the contracts are shaped for. The code is `crates/ds-plugin` (host, runtime, CLI), `crates/ds-core/src/edit_keys.rs` (the edit vocabulary), the `PostProcess` seam in `crates/ds-export/src/post.rs`, `crates/ds-ffi/src/plugins.rs` (FFI contract 34) and `app/Sources/DarkslideKit/Plugins/`. The reference plug-ins are in `plugins/`.

Darkslide is extended with **plug-ins**: packages named `*.dsplugin` that declare what they add in a manifest. A plug-in can be pure data (looks, styles, voice phrases, later export recipes, profiles, Core ML models) or carry code (a WebAssembly module; later, on macOS, a helper program). The app's own optional features are packaged the same way, so the core stays small and every extension point has at least one real user.

## Goals

- **Adding something is a folder and a manifest.** A looks pack needs no code and no build step. A code plug-in needs one exported function.
- **One mechanism for built-ins and third parties.** Built-in features that are optional by nature ship as bundled plug-ins through the same registry.
- **Safe by default.** A plug-in sees only what its manifest asks for and the user approved. A crash, a hang or a runaway allocation in a plug-in fails that plug-in's step and nothing else. A plug-in can never lose a user's file.
- **iPad.** iPadOS allows no `dlopen` of third-party code and no JIT. **iPad v1 runs data packs only** (looks, styles, voice phrases): the only code kind, `export_post_process`, needs export, which iPad v1 doesn't have (docs/IPAD_PLAN.md). WebAssembly runs there unchanged once iPad has export; helpers stay Mac-only.
- **Stable contracts.** Each extension point has its own versioned contract, so the host can grow without breaking installed plug-ins.

## Threat model

- **Untrusted:** a plug-in's package (signed or not) and its code. Code runs sandboxed in WebAssembly; packages are checked before anything in them is used.
- **Out of scope:** other software running as the same user. It can rewrite `state.json` and the plug-in folders like any of the user's files; the pinning below catches accidents and swapped packages, not same-user malware.

## Package format

A `.dsplugin` is either a **folder** (Finder shows it as one item: the app declares the package type) or a **zip** of the same layout (`ds-plugin pack --zip`), for downloading and mailing.

```
Film Border.dsplugin/
  plugin.toml          the manifest (required)
  looks.json           data files the manifest names
  border.wasm          a code module the manifest names (or border.wat: compiled at load)
  SIGNATURE            optional: an Ed25519 signature over the file list
```

**Package rules** (checked on every read, install and load; `crates/ds-plugin/src/package.rs`):
- Regular files only: no symbolic links, no hard links. File names are printable ASCII (stricter than NFC normalisation: no Unicode name forms to normalise), unique ignoring case (APFS can't tell `Looks.json` from `looks.json`), and sorted by their UTF-8 bytes wherever order matters. Dotfiles (`.DS_Store`, AppleDouble `._*`, `.git`) and a zip's `__MACOSX` folder are ignored: never hashed, never read.
- At most 512 files and 64 MB; a data file at most 1 MB and 200 items; a module at most 16 MB.
- A zip is read into memory with its names checked (no absolute paths, no `..`, no link entries, sizes capped before inflating), and a single top folder `Name.dsplugin/` is stripped.
- **Read once:** every file is read into memory once, without following links. Hashing, the signature, parsing and compiling all use those bytes; nothing is reopened by path after it was verified.

### The manifest (`plugin.toml`)

```toml
manifest = 1                        # manifest format version (this document: 1)
id = "com.example.film-border"      # reverse-DNS, unique, never reused for another plug-in
name = "Film Border"
version = "1.1.0"                   # the plug-in's own semver
api = 1                             # host API major version it was written for
min_darkslide = "0.2.0"             # optional
authors = ["Jane Doe"]
summary = "A plain border around each exported photo, like a print's margin."
platforms = ["macos"]               # optional; default: every platform

permissions = []                    # see "Permissions"; empty = none

[[contributes.export_post_process]] # one table per contribution
id = "border"                       # unique inside the plug-in
name = "Film Border"
module = "border.wat"
entry = "ds_post_process"
contract = 1
apron_rows = 0                      # rows of context above and below each strip (0–256)
params = [
  { key = "width", label = "Width", type = "float", min = 0.0, max = 0.2, default = 0.03 },
  { key = "color", label = "Colour", type = "color", default = "#000000" },
  { key = "outside", label = "Around the photo", type = "bool", default = true },
]
```

- **TOML** because people write it by hand and it allows comments.
- **Unknown keys and features (the policy):**
  - An unknown *optional* key (top level or inside a contribution) is a **warning** at load, shown in the Plugins pane, so a manifest using a newer optional key still loads on an older Darkslide.
  - Anything *required* that this Darkslide doesn't know is a **refusal**: a newer `manifest` format or host `api`, a `min_darkslide` above this version, an unknown permission, a contract outside the supported range, or a module importing a host function this Darkslide lacks. The plug-in is listed with "needs a newer Darkslide" and loads nothing.
  - An unknown contribution *kind* is skipped with a warning (the rest of the plug-in loads); an unknown key or field inside a data item leaves that item out ("needs a newer Darkslide").
  - At author time (`ds-plugin validate` and `pack`) every unknown key and every warning is an **error**, so typos don't ship.
- **Identity** is `id`. Two packages with one id never load together (see "Clashes"). Ids starting with `darkslide.` are reserved for bundled plug-ins, and a bundled plug-in must use one.
- **Names and text** (`name`, `summary`, param labels, error messages) are cleaned to one line without control or bidirectional-override characters, and capped (name 40, label 40, summary 200, message 300). A third-party name containing "Darkslide", "Built-in", "Apple", "macOS" or "System" is refused. The app shows plug-in text only quoted inside its own sentences ("“Film Border” says: …"), never as a prompt, an alert or a sheet title.
- **Every path** is relative, stays inside the package (no `..`, no absolute paths) and must be one of the package's files.
- **Params** are typed (`float`, `int`, `bool`, `choice`, `color`, `text`) with ranges and defaults. The host draws their UI and validates values, so a plug-in never sees an out-of-range value. Colours are sRGB `#RRGGBB` / `#RRGGBBAA`.

### Signing and trust

A `SIGNATURE` file holds a signer's Ed25519 public key, the name it claims, and a signature over the domain prefix (`darkslide-plugin-sig-v1\0`), **that name and key**, and the byte-sorted list of `(path, BLAKE3, executable bit)` of every file. A signature relabelled with another name fails.

| Level | How | What it may do |
|---|---|---|
| **Built-in** | In `Darkslide.app/Contents/Resources/Plugins/`, and its files are exactly the ones **compiled into the app** (`crates/ds-plugin/build.rs` hashes `plugins/builtin/**` into the binary). A bundled folder that differs doesn't load. No signing key is involved | Anything its manifest declares; no prompts |
| **Signed** | The signature verifies with a key the user trusted | Declared permissions, each granted explicitly |
| **Signed, untrusted** | The signature verifies with a key the user hasn't trusted | Its code waits for Trust Signer (that key, by fingerprint) |
| **Unsigned** | No signature | Data packs install after one confirmation. Code waits for its own **Allow** (this plug-in only; an unsigned update asks again, since anyone could have made it). Its file, network and account permissions additionally need Settings ▸ Plugins ▸ "Allow unsigned code plug-ins" (the authors' switch, which also allows every unsigned plug-in at once); each is still its own Grant, and turning the switch off takes them back |

- **The key is the identity, not the name.** The app shows the key's **fingerprint** (`3F2A-91C0-7D44-E815`) and the name only as "it claims to be “Jane”". Trusted keys are stored by key. Signer names containing "Darkslide" or "Built-in" are refused.
- **The signer is pinned per id** (trust on first use). `state.json` records, per id, the key that first installed it (or "unsigned"). A package with that id signed by another key, or a change between signed and unsigned, is a different plug-in: the install is refused ("signed by a different key … remove the installed one first"), and such a folder dropped into the Plugins folder by hand doesn't load. It never inherits grants. Shadowing (below) only happens between packages from the same signer.
- **Changes are noticed.** At install the host pins every file's BLAKE3; at each load it hashes again. A plug-in whose files changed, or one put in the folder by hand, waits for approval.
- **Keys never enter the repository.** Darkslide is open source: no private key is committed or needed to build. Bundled plug-ins are trusted through the hashes compiled into the binary, so contributors build and test the app with no key at all. Plug-in authors make their own key with `ds-plugin keygen` (kept outside any repository; `*.key.json` is git-ignored here), or develop unsigned with a development folder (below). If a release signing key is ever introduced (for example for a plug-in catalogue), it is read at release time from a git-ignored file or an environment variable, and a contributor's build uses their own dev key in its place.

## Install, enable, disable

- **Location:** `<support>/Plugins/<id>.dsplugin` (support = `~/Library/Application Support/Darkslide`, or `$DS_SUPPORT_ROOT`). Bundled plug-ins load from the app bundle and can't be removed, only disabled.
- **Install** (drop a `.dsplugin` folder or zip on Settings ▸ Plugins, or Install…):
  1. The package is read and **copied into `Plugins/.staging-<random>` first**, read back, and must hash the same. A staged copy is valid for 30 minutes and until the sheet is dismissed or another package is dropped; the pinned signer is checked again when it is installed. Leftover staging folders are removed at open only once they are older than that, so another window's open sheet keeps its copy.
  2. The **staged copy** is validated (manifest, files, modules, signature, pinned signer) and the install sheet shows it: name, version, signer fingerprint, what it adds, the permissions it asks for, and what it replaces.
  3. Install renames exactly that staged copy into place (the old version goes to `Plugins/.replaced/` and is removed once the new one has loaded; if the app stops between the two renames, the old version is put back at the next launch). Cancel deletes it. The user's own folder is never touched, and a change to it after the sheet opened changes nothing.
  Nothing is granted at install.
- **Versions:** the same version with the same files is a no-op; the same version with different files is refused (bump the version); an older version than the one installed is refused unless the user confirms "Install Older Version". An update keeps its enabled state and the grants it still asks for; a permission it adds waits for a Grant. File, network and account grants (and their folders) carry over only to an update signed by the pinned key: an unsigned update, which anyone could have made, waits for them to be granted again.
- **Clashes:** two packages with one id (and one signer): a development folder wins, then the higher version, then the managed copy `<id>.dsplugin`, then the bundled one. The others are listed as "Replaced by another copy" and contribute nothing. A development folder is checked against the pinned signer like any copy, and its enabled state, grants and Allow are kept apart from the installed id's (`dev:<id>` in the state), so it never inherits them.
- **Remove** moves the folder to the Trash, then forgets the id's grants, pinned files and pinned signer (a package put back later is a new install); it keeps only whether it was enabled.
- **Development folders** (Settings ▸ Plugins ▸ Add Development Folder…, needs "Allow unsigned code plug-ins"): a plug-in is loaded in place from the author's folder, never pinned, so edits apply on Reload without re-approval. It is marked **DEV**, wins over an installed copy of its id, and still needs explicit grants. It can't be inside Darkslide's own folders.
- **State:** `<support>/Plugins/state.json` (atomic writes): per id, enabled, granted permissions (with the folder chosen for each `files.*` one), pinned files, pinned signer and version; the trusted keys; "Allow unsigned code plug-ins"; development folders. An unreadable file is moved aside and an empty state used: that only ever takes permissions away.
- **Off the main thread:** the app has one `PluginHost` (`PluginHosts.shared`, also the one export steps resolve through). Every change and reload runs off the main actor; the pane updates when it is done.
- **Settings ▸ Plugins:** one row per package (“name”, version, "Built-in" / "Key 3F2A-… (“Jane”)" / "Unsigned", DEV, what it adds, its state), an enable switch, each decision it waits for with its button (Approve, Trust Signer, Allow, Grant — a `files.*` Grant asks for the folder; Allow allows that one unsigned plug-in), its granted permissions with Revoke, and load problems in plain words. Install…, Reveal Plugins Folder, Reload, Add Development Folder…, Remove or Unlink, and the "Allow unsigned code plug-ins" switch. Changes take effect at once.

## Extension points

Each contribution kind has a contract number. The host supports a range per kind and refuses a contribution outside it. Kinds marked **data** need no code. v1 builds the first four rows and specifies the fifth.

| Kind | Tier | What it adds | Contract |
|---|---|---|---|
| `looks` | data | One-touch looks (Styles ▸ Looks), with Intensity | `look@1`, below |
| `styles` | data | Styles in a named section of the Styles panel | `style@1`, below |
| `voice_phrases` | data | Phrases for voice commands ("keeper" → Pick) | `phrases@1`, below |
| `export_post_process` | code | A step on each exported image at its final size, before the watermark | See "Post-process contract" |
| `export_destination` | code (WASM) | Where exports go: an online service through host-mediated HTTP and OAuth, or a local hand-off folder | Specified; see "Online destinations" |
| `export_recipes` | data | Ready-made export recipes | Later |
| `profiles` | data | ICC and DCP profiles | Later |
| `keywords` | data | A keyword vocabulary | Later |
| `metadata_provider` / `metadata_writer` | code | Suggest or write metadata | Later |
| `import_source` | code (helper) | Photos from elsewhere | Later |
| `event_hook` | code | Session events (`imported`, `rated`, `exported`…) | Later |
| `commands` / `tool_panels` / `viewer_overlay` | data | Declarative commands, panels and guides | Later |
| `ai_models` | data | Core ML models with a fixed I/O contract per role (`upscale@1`) | Later |
| `develop_filter` | code (GPU) | A develop step: WGSL kernel + WASM CPU reference (an interpreter can't meet 16 ms drags) | Later (v3) |

### Data packs

The edit vocabulary is one table, `ds_core::edit_keys`: every `EditCodec` key with its adjustment group, kind (amount, choice, wheel hue/saturation, flag, index) and range. The validator (`ds-plugin validate`) and the loader use it; the app's `EditCodec` must match it key for key (a Swift test fails when they drift).

**`look@1`** (`looks.json`, an array):
```json
{ "id": "warm-fade", "name": "Warm Fade", "summary": "Lifted blacks, a warm cast",
  "values": { "exposure.contrast": -10, "levels.outBlack": 0.05, "cb.master.hue": 40, "cb.master.saturation": 0.08 },
  "exact": [] }
```
- **Amounts only**, so Intensity scales every field exactly and Intensity 0 restores the photo exactly: curves, colour ranges, skin tone, switches, `crop.aspect` and `crop.angle` are refused in looks (make it a style).
- The loader derives what the built-in looks always had, with the same Swift functions: groups are the groups it changes, exposure/presence/HDR add to the photo's own values, and its `exact` keys plus every setting of each effect it uses are set exactly.
- **Identity:** a third-party look is stored as `<plugin-id>/<id>` and its preset UUID is derived by the host from the plug-in id and look id, so packs can't collide with each other or with Darkslide's own. Only bundled (`darkslide.*`) packs set `id` and `uuid` verbatim (the migrated `look.portrait`… and their `…B001`… UUIDs).

**`style@1`** (`styles.json`, an array): a saved preset with everything explicit:
```json
{ "name": "Warm Film", "section": "Film", "groups": ["exposure", "hdr", "curve", "colorBalance"],
  "relative": ["exposure", "hdr"], "absolute": [], "values": { … }, "curves": { "RGB": [[0, 0.05], [0.5, 0.52]] } }
```
Sections appear in the order of first appearance. Third-party UUIDs are derived as for looks; bundled styles carry theirs (`…A010`, `…A004`… in panel order).

**`phrases@1`** (`{ "language": "en", "phrases": { "keeper": "key-pick" } }`): phrase → voice command id. Voice commands are English only today, so a pack in another language is listed, not used. The user's own phrases come first; a pack can't take a phrase that already means something (the phrase book's own rules), and a phrase two packs claim is left out of both, with a note.

**Newer data on an older Darkslide:** an item using a key, a group or a field this Darkslide doesn't know is left out with "needs a newer Darkslide" (listed in the pane) rather than applied partially. Values are checked against each key's range. `ds-plugin validate` turns any left-out item into an error.

### Post-process contract (`export_post_process`, contract 1)

- **When:** on each exported file at its final size (after resizing and output sharpening), on that file's own working copy (never a render shared between recipes), on the 8-bit and the 16-bit paths alike. Plug-in steps run in the recipe's order, then the **watermark, last** (the native provider of the same `ds_export::PostProcess` seam), so a border never covers a mark.
- **Canvas:** a module may export `ds_output_layout(ctx_ptr, ctx_len, width, height, out_ptr) -> i32` and write four u32 at `out_ptr`: the new canvas width and height and where the image goes in it. The host allocates the bigger canvas (black margin, opaque), places the image, and passes it on; `photo_rect(ptr)` tells a module where the photo is. That is how Film Border draws a margin *around* the photo. A canvas may grow to at most twice the image's area (and 100 MP), whatever the steps ask.
- **Strips:** pixels are passed in strips of at most 4 MB plus `apron_rows` rows of context above and below (see "Host API"), so memory stays bounded at any image size.
- **Failure:** a step that fails, traps, runs out of fuel, panics (native or WebAssembly: every call is isolated with `catch_unwind`) or asks for an impossible canvas **fails that file's export**: nothing is written, and the export report names the plug-in and its message. A recipe naming a plug-in that is missing, disabled or not approved **can't start** ("the plug-in step … isn't available"), in the sheet's own check. A contribution that failed three times in a row stops for the rest of the session, so the rest of the batch fails with that message.
- **Versions:** each recipe step remembers the plug-in version it was set up with; after an update the recipe editor says so ("Set up with Film Border 1.0.0; 1.1.0 is installed"). A stored value the update's params no longer allow is clamped into the new range (or the default is used) with a note in the editor; the recipe keeps exporting.
- **Cancel:** the export's Cancel reaches a running step: `progress()` returns 1 (polled every 10 ms by the host), and the host checks it between strips.

## Execution model

**Three tiers, chosen per contribution:**

1. **Data packs** (most plug-ins): read and validated by the host; no code runs; every platform.
2. **WebAssembly in an interpreter** (`wasmi` 2, loop dispatch) for code that transforms data.
   - Pure Rust, no JIT: runs on iPadOS. A module is compiled once per load from the package's bytes.
   - **Sandbox by construction:** a module may import only the host API below. There is no import for files, the network, the clock, or the host's own state (grants, trust, `state.json`), and a module that imports anything else is refused at load.
   - **Time:** fuel on every entry: 10 M instructions for `start`, `ds_api`, `ds_alloc` and `ds_output_layout`, and 5 M + 400 per pixel for each strip. Running out stops the call ("took too long").
   - **Memory:** one linear memory per module (multi-memory is turned off, since the limit is per memory) of at most 64 MB; one table of at most 100 000 elements; one instance per image; and one global budget of 512 MB across all running instances (at most eight at once, whatever the thread count). Modules are at most 16 MB.
   - **Isolation:** every call runs on its own thread with an 8 MB stack, and every trap, panic, bad pointer (buffers are bounds-checked) or bad return value becomes that file's error, naming the plug-in.
   - **Speed:** an interpreter is roughly 10–50× slower than native code: fine for export steps, not for interactive develop filters.
3. **Helpers (macOS only, later)** for integrations that need the OS. A helper is native code with the user's privileges: the signature is the only control, so helpers will always need a signed package, and the doc's earlier promises of file mediation and kill-on-timeout are only as strong as a process can be (see v2: process groups, XPC or a sandbox profile). Uploading to a service does *not* need a helper: online destinations are WASM over the host's HTTP.

Not chosen: native dylibs (no isolation, no iPad), JavaScriptCore (untyped, weaker limits), a JIT runtime (no JIT on iPadOS).

### Permissions

Declared in `permissions`, shown at install, granted one by one, revocable in Settings ▸ Plugins. The app writes every permission sentence; plug-in text never does.

| Permission | Grants |
|---|---|
| `photo_pixels` | The rendered pixels (post-processors always get their output; this is for metadata providers) |
| `photo_metadata` | The photo's file name, camera, lens, ISO and keywords in the call's context |
| `metadata_write` | Proposing metadata changes, through the session's undoable `set_metadata` (later) |
| `files.read:<label>` / `files.write:<label>` | **One folder the user chooses when granting.** The real path is resolved and refused when it is, contains or lies inside the support folder, the app bundle, a bundled or development plug-in folder, the whole disk, the home folder, or any session (open or in the session list). It is checked again at each use, so a link swapped in later fails |
| `network:<host>` | HTTPS to the listed hosts only (exact names or `*.example.com`; no IP literals). The host makes every request, so the list is enforced |
| `account:<name>` | Signing in through the host's OAuth; the token is attached by the host, only to that account's `api_hosts` |
| `clock` | Wall time (later; without it the export's start time) |

Nothing ever grants deleting a file, reading other plug-ins' data, or reaching Darkslide's own state.

## Host API (WebAssembly, API 1)

Modules target `wasm32-unknown-unknown`. Exchange is pointer-and-length over the module's linear memory; structured input is JSON (UTF-8); pixels are raw rows.

The module exports:

```
ds_api() -> i32                         // the host API major it was built for (1)
ds_alloc(len: i32) -> i32               // a buffer for the host to write into (bounds-checked by the host)
ds_free(ptr: i32, len: i32)             // (the host allocates its buffers once per image and never frees)
ds_output_layout(ctx_ptr, ctx_len, width, height, out_ptr) -> i32   // optional: the canvas (four u32 at out_ptr)
ds_post_process(ctx_ptr, ctx_len, px_ptr, width, height, stride, format,
                buf_y0, buf_rows, core_y0, core_rows) -> i32
//   width × height is the whole canvas; the buffer holds rows buf_y0 .. buf_y0+buf_rows,
//   `stride` bytes apart (stride is in bytes); only rows core_y0 .. core_y0+core_rows are
//   this call's to change (the rest is apron context and is not copied back).
//   format 0 = RGBA8 (alpha left alone), 1 = RGB16 (native-endian u16), in the output's
//   space (ctx.output.space: srgb, display-p3 or rec2020); returns 0, or an error code.
//   ctx = {"params": {...}, "output": {"space": "srgb"}, "photo": {...}}  (photo only with photo_metadata)
```

A bump allocator is enough (`plugins/film-border.dsplugin/border.wat`).

The host provides (module `darkslide`):

```
log(level, ptr, len)                    // kept with the call's result (≤ 100 lines)
error(ptr, len)                         // the message shown when the call fails
progress(permille) -> i32               // 1 when the user cancelled: return 1
photo_rect(out_ptr) -> i32              // four u32: where the photo is in the canvas
param_f64(key_ptr, key_len) -> f64      // a float/int param (NaN when missing)
param_i64(key_ptr, key_len) -> i64      // an int param (i64::MIN when missing)
param_bool(key_ptr, key_len) -> i32     // 1 / 0 (−1 when missing)
param_color(key_ptr, key_len) -> i64    // 0xRRGGBBAA, sRGB (−1 when missing)
```

Later, with the contracts that need them: `now_ms`, `http_request` (destinations), `file_read` (a handed-over file), a small `state_get`/`state_set` store. Any language that targets WASM works against this ABI; `ds-plugin pack` compiles `.wat` to `.wasm`.

### Online destinations (OAuth, Keychain, network) — specified in v1

Destinations that upload to a service (Google Photos on `feat/gphotos`, Amazon Photos, SmugMug, a client gallery) run as **WASM**, so they work on iPad and their network access is enforced.

```toml
permissions = ["network:photoslibrary.googleapis.com", "network:oauth2.googleapis.com",
               "network:accounts.google.com", "account:google"]

[[contributes.export_destination]]
id = "google-photos"
module = "gphotos.wasm"
contract = 1
accounts = ["google"]
formats = ["jpeg", "heic"]
max_file_mb = 200

[accounts.google]
api_hosts = ["photoslibrary.googleapis.com"]      # the only hosts the token is ever sent to
auth = "oauth2-pkce"
authorize_url = "https://accounts.google.com/o/oauth2/v2/auth"
token_url = "https://oauth2.googleapis.com/token"
client_id = "1234-abc.apps.googleusercontent.com"   # a public client: PKCE, no secret
scopes = ["https://www.googleapis.com/auth/photoslibrary.appendonly"]
```

- **Tokens go only to the account's own API hosts.** Each `[accounts.x]` lists `api_hosts` (each also a `network:` permission). A request with `"auth": "google"` to any other host — even one on the plug-in's network list — is refused, and a redirect that changes host drops the `Authorization` header. The sign-in sheet shows the hosts ("Google account, used with photoslibrary.googleapis.com").
- **Well-known account names** (`google`, `amazon`, `microsoft`, `dropbox`, `apple`) are reserved to those services' real sign-in hosts: a look-alike plug-in can't run its own OAuth client under a trusted name. Other accounts show the client's domain on the sign-in sheet.
- **Sign-in** is the host's (`ASWebAuthenticationSession`, PKCE S256, a random `state`); the plug-in never sees the code or a token.
- **Keychain:** a generic-password item, service `casa.baillargeon.darkslide.plugin`, account `<plugin id>/<account>`, `AfterFirstUnlockThisDeviceOnly`, not synced; deleted (and revoked) on sign-out or removal. Nothing secret goes in `state.json` or the logs.
- **Requests** (`ds_plugin::destination::check_request`): HTTPS to a granted host, a known method, no host-only headers (`Authorization`, `Cookie`, `Host`…), a body only from a file the job handed over (streamed by the host), with timeouts, retries on 429/5xx and a 4 MB response cap.
- **Local hand-off** (Amazon Photos' watched folder): `files.write:<label>` on the folder the user chooses, written through the host's atomic writes.
- **Traits** live next to their consumer (`ds_export` owns `PostProcess`, and will own `Destination`); `ds-plugin` adapts modules to them, so there is no dependency cycle and wasmi never enters the export crate.

## Versioning rules

- **`manifest`**: the manifest's format. Adding an optional key doesn't change it (older hosts warn and ignore it); renaming or removing a key does.
- **`api`**: the host API major. Within a major the host only adds imports; a module importing something this Darkslide lacks fails to load ("needs a newer Darkslide"). `ds_api()` must return the manifest's `api`.
- **`contract` per kind**: the shape of one extension point. Additive fields don't bump it; a changed meaning does. A data item that uses something this host doesn't know is left out, not half-applied.
- **`FFI_CONTRACT`** (Rust ↔ Swift) is internal; this branch uses 28, and the number is assigned at merge.
- A plug-in's own `version` is semver: updates, downgrades and clashes use it.

## Built-ins as plug-ins

- Optional features become **bundled plug-ins** in `plugins/builtin/`, copied into `Darkslide.app/Contents/Resources/Plugins/` by `scripts/build-app.sh` and **hashed into the binary** at build time; a dev build and the tests read the source tree's folder (same hashes). The user can disable them like any other.
- The registry is in `ds-plugin`; each extension point's trait lives with its consumer (`ds_export::PostProcess`); `ds-ffi` wires the two.
- The Swift side asks the registry over FFI and never reads plug-in folders itself.
- New optional, self-contained features that fit an extension point ship as bundled plug-ins. The core keeps decode, the develop pipeline, sessions and sidecars, import verification, export encoding and the FFI.

## What can move out today

| Feature | Fits as | Effort |
|---|---|---|
| **Looks** (18) and **styles** (20) | `looks` / `style` data packs | **Done in v1** (below) |
| **Voice phrases** | `voice_phrases` packs | Easy: the loader exists; the built-in catalog stays the parser |
| **Watermark** | `export_post_process` | Medium: it is the seam's native provider today; a WASM port needs a host text rasteriser and would be seconds per export in an interpreter, so it stays native |
| **Google Photos destination** (`feat/gphotos`) | `export_destination` (OAuth + network) | Medium: its narrow uploader fits the contract above; it registers as a built-in provider once destinations run, and moves to WASM once the host HTTP/OAuth imports ship |
| **Amazon Photos hand-off** | `export_destination` (local folder) | Medium: a mirror into a watched folder with `files.write` |
| **rsync archive** | helper | Medium–hard: its volume-safety logic must stay in the host |
| **Capture One importer** | `import_source` (native built-in) | Hard: it writes deeply into sessions |
| **AI Upscale model** | `ai_models` (`upscale@1`) | Medium: only where the model comes from changes (~33 MB out of the bundle) |
| **Soft proof**, **Map** | stay core | Not candidates (profiles and overlays can extend them) |

**Reference plug-ins in v1:**
1. **`plugins/builtin/darkslide-looks.dsplugin`** — the 18 looks and 20 styles moved out of Swift, generated from the old tables. `PluginTests` checks every one **field for field** against those tables (kept as test data in `LegacyBuiltIns.swift`: id, name, summary, groups, relative, absolute fields, edit, category, order), applies each to a corrected photo against a recorded fixture, and checks Intensity 0 restores the photo exactly for every look. Look ids and UUIDs are unchanged, so sidecars and thumbnails keep working.
2. **`plugins/film-border.dsplugin`** — a WASM export step written in WAT, the code template: a border around the photo (growing the canvas) or over its edge, any colour with alpha.

## SDK and authoring

- **`ds-plugin` CLI** (`cargo run -p ds-plugin -- <command>`):
  - `new <name> --kind looks|post-process [--id <reverse-dns>] [--dir <parent>]`: a template folder;
  - `validate <folder or zip>`: everything the loader checks, strictly (unknown keys and left-out items are errors), and runs each post-processor with its defaults on 64×64 in both formats, through its layout;
  - `pack <folder> --out <parent>` or `--zip <file.dsplugin>`: a clean copy, `.wat` compiled to `.wasm`;
  - `keygen <signer> --out <key.json>` (never overwrites; written owner-only, 0600; keep the file out of any repository) and `sign <folder> --key <key.json>` (signs exactly the bytes it validated);
  - `contracts`.
- **Looks, 2 minutes:** in Darkslide, set a photo up as you like and click **Styles ▸ Copy as Look JSON** (amounts only; the toast says what a look can't carry). `ds-plugin new "My Looks" --kind looks`, paste the item into `looks.json`, `ds-plugin validate`, and drop the folder on Settings ▸ Plugins. Data packs need no signature.
- **Post-process:** `ds-plugin new "Frame" --kind post-process --id com.example.frame`, turn on "Allow unsigned code plug-ins", **Add Development Folder…** to load it in place, edit `border.wat`, Reload, export. When it's done: `keygen`, `sign`, `pack --zip`.

## What v1 ships

- **`ds-plugin`:** manifest format 1 with the strict/lenient key policy; packages (folder or zip) read once, with the hygiene rules; the registry (bundled folders checked against compiled-in hashes, reserved ids, signer pinned per id, clashes, platforms, development folders); staging installs; downgrade and same-version rules; grants with checked folders; Ed25519 signatures with fingerprints; data packs validated against `ds_core::edit_keys`; the wasmi runtime (strips, canvas layout, fuel per entry, per-instance and global memory, isolation, three strikes); the `export_destination` contract types and checks; the CLI.
- **ds-core:** `edit_keys` (the vocabulary, exported over FFI for the app's drift test).
- **ds-export:** the `PostProcess` seam with canvas growth, panic isolation, watermark last.
- **FFI 28:** `PluginHost` (`open`, `list`, `preview`→staged token, `install`, `discard`, `forget`, `set_enabled`, `approve_files`, `grant(folder, protected)`/`revoke`, `trust_signer`/`untrust_signer`, `set_allow_unsigned_code`, `add_dev_folder`/`remove_dev_folder`/`dev_folders`, `user_folder`, `reload`, `data_packs`, `post_processors`), `edit_keys()`, `ExportRecipeSpec.post_process` (with the plug-in version).
- **App:** Settings ▸ Plugins; looks, styles and voice phrases from the active plug-ins; the recipe editor's Post-processing section; Styles ▸ Copy as Look JSON; bundled plug-ins copied by `build-app.sh`.
- Nothing needs real hardware; all tests use temporary folders.

## v2 list (from the design review)

Not in v1, in rough order:
- **M4 version rules:** require `api` only with a module and check it equals `ds_api()` at load (today only `ds_api()` is checked); `pack` fills `min_darkslide` from what is used; per-contribution `formats` for post-processors (half-float or HDR exports) with conversion or refusal; a per-contract deprecation policy shown in the pane.
- **M9 helpers:** spawn in their own process group (`POSIX_SPAWN_SETSID`) and kill the group, or run them as an XPC service or under a sandbox profile; say plainly they run with the user's privileges.
- **M10 iPad:** `.dsplugin` zip import through the Files picker; the App Review 2.5.2 constraint on downloaded interpreted code in IPAD_PLAN; the look's display name in `AppliedLookEdit` (additive) so a plug-in look applied on the Mac reads right on an iPad without it; offering missing data packs over the Mac–iPad link.
- **M11 authoring:** ship `ds-plugin` inside the app with "Install Command Line Tool"; a typed Rust SDK crate and template with a prebuilt `.wasm`; `ds-plugin run <folder> --in photo.jpg --out out.jpg`; "Save Selected Presets as Look Pack…"; watching development folders for changes instead of Reload.
- **M12 parser input:** icons (≤ 512², decoded off the main thread), ICC/DCP profile and model packs listed as "parsed by the system" with a confirmation.
- **M13 required built-ins:** make `darkslide.looks` hideable but not unloadable, or give `LookSuggester` its candidates from the registry with a `suggest` tag per look; today disabling it empties AI Looks.
- **M15 concurrency:** an immutable registry snapshot per export job (today a job resolves its steps at start and keeps them, but a removal mid-job isn't specified); `progress` setting only an atomic.
- **L1** the FFI contract number is assigned at merge. **L2** `serde_json` `float_roundtrip` for packs (the field-for-field test passes today without it). **L4** a criterion bench for Film Border at 24 MP (RGBA8, RGB16) to set the fuel factor, with a budget in ARCHITECTURE. **L5** `clock` (`now_ms`) when a contract needs it. **L7** localised names and labels (`name.de = "…"`). **L8** `cargo fuzz` targets for the manifest, package (zip) and signature parsers, and a larger adversarial-module suite.
- **Signer names:** fold confusable characters (Unicode UTS #39 skeletons) before the reserved-name check, so homoglyphs of "Darkslide" are refused too (zero-width, control and bidi characters already are). The key is the identity: every place a signer name is shown keeps its fingerprint beside it, in v1 and after.
- Extension points after these: `commands` and `tool_panels`, `metadata_provider`/`event_hook`, running `export_destination` (Google Photos, then Amazon Photos), helpers, `ai_models`, `viewer_overlay`, `develop_filter`.
