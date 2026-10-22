# Darkslide: plan

A macOS RAW workflow app: fast culling, sessions, layers, and copy/apply of adjustments. It is designed around the most common complaints photographers have about existing RAW tools.
Working name: **Darkslide**, after the plate that covers film in a holder. Changing it is **D0**.

Status: **plan only. No code yet.** Decision points are tagged **D#** and collected in [§10](#10-decision-points). Each one has a recommended default, and I'll proceed with that default unless you override it.

---

## 1. Goals and non-goals

**Goals (what you actually use)**
- Import from a memory card that *just works*: fast, verified, deduplicated, and never loses a file.
- Very fast browsing and culling of RAW files, where a keypress moves to the next image in under 50 ms.
- Sessions: each import goes into a new session or an existing one you pick.
- Light editing done well: crop/straighten, white balance, exposure/tone, color (balance plus color editor), curves/levels.
- Layers with masks (brush, gradients, luminance/color range, and later AI subject/sky).
- Copy/apply of selected adjustment groups to one image or a selection (all of a session, album or filter by selecting all).
- A background **rsync to an archive** while you work.
- Optional **Amazon Photos sync** (see D3; this one needs a careful decision).
- A native-feeling, good-looking Mac GUI. The core is Rust for speed.

**Non-goals for v1**: tethered capture, printing, a book/web module, a plugin SDK, Windows/Linux, and importing other apps' catalogs. XMP interop is in scope; catalog migration is not.

---

## 2. Complaints this app is designed around

| Common complaint | Design response |
|---|---|
| Slow import and slow first view, a long wait for previews | Show the **embedded JPEG** from the RAW instantly (Photo Mechanic–style). Real renders fill in behind it, visible-first. |
| Laggy sliders, poor GPU use | The whole develop pipeline runs as GPU compute (Metal). A slider drag renders a screen-sized proxy, with a target of **≤16 ms/frame**. Full-res renders only at 1:1 zoom and only for the visible tiles. |
| One huge catalog that bloats, corrupts, and can't live on a NAS | **Sessions are self-contained folders.** Per-image **sidecars are the source of truth**; the SQLite index can be rebuilt from them at any time (D4). |
| Moving files in Finder breaks the catalog ("missing files") | Every file is tracked by **content hash (BLAKE3)** as well as path. A moved or renamed file relinks automatically on rescan. |
| Import dialog is clunky and cards are handled badly | A dedicated card workflow: auto-detect DCIM volumes, parallel copy, checksum verify, dedupe against everything imported before, and safe eject (§5). |
| Pasting a crop onto mixed orientations/aspect ratios goes wrong | Crop paste is **aspect-ratio-aware**: it keeps the ratio and adapts to the image's orientation. Exposure can be pasted as **relative** (+0.3 EV) or **absolute**. |
| Many masks make things slow | Mask bitmaps are cached per layer at proxy and full resolution. Only the dirty layer re-renders. |
| Catalog performance at scale; weak asset management | Sessions stay small by design. A separate lightweight **global index** gives search across all sessions (phase 3). |
| Subscription and cloud lock-in | Local-first, plain files, open sidecar format, XMP export. |
| General: UI freezes during export or sync | All I/O runs on background job queues with priorities. The UI thread never blocks on disk or network. |

---

## 3. Architecture

```
┌──────────────────────────── macOS app (.app) ────────────────────────────┐
│  UI shell (SwiftUI + AppKit, D1)                                          │
│   Browser/grid · Viewer (CAMetalLayer) · Tool tabs · Import sheet ·       │
│   Copy/Apply buttons · Sync status · Preferences                          │
│            │  UniFFI bindings (async, callbacks for progress/events)      │
├────────────▼──────────────────────────────────────────────────────────────┤
│  Rust core (workspace)                                                    │
│   ds-session   sessions, variants, ratings/tags, sidecar I/O, SQLite idx  │
│   ds-import    volume watch, copy+verify, dedupe, rename templates        │
│   ds-raw       decode (rawler; LibRaw fallback, D2), embedded previews,   │
│                EXIF/maker notes                                           │
│   ds-pipeline  GPU develop pipeline (wgpu→Metal compute), color mgmt      │
│   ds-layers    layer stack, mask rasterization, brush strokes             │
│   ds-cache     thumbnail/preview pyramid, mmap'd, LRU, visible-first      │
│   ds-jobs      prioritized work scheduler (tokio + rayon)                 │
│   ds-export    recipes → JPEG/HEIC/TIFF (16-bit)/DNG passthrough          │
│   ds-archive   rsync scheduler/runner                                     │
│   ds-cloud     Amazon Photos adapter (D3)                                 │
└───────────────────────────────────────────────────────────────────────────┘
```

**Why this split.** Rust owns everything performance-critical and everything about correctness: decode, pipeline, file safety, sync. Swift owns what macOS does best: native controls, menus, keyboard handling, drag-and-drop, DiskArbitration/volume events, ColorSync display profiles, Quick Look, and Vision/Core ML masks. The alternative, a pure-Rust GUI, is weighed in **D1**.

**Reference toolchain:** rustc 1.98 (Homebrew), Swift 6.2, Xcode CLI, M4 Pro.

---

## 4. Data model: sessions

A session is a self-contained folder:

```
~/Pictures/Darkslide/2026-09-25 Fall Hike/
  2026-09-25 Fall Hike.dsession     # SQLite index (WAL); rebuildable
  Capture/                          # originals, as imported
    .ds/settings/IMG_0412.CR3.json  # sidecar = source of truth (D4)
  Selects/                          # "move to Selects" target
  Output/                           # export recipes land here
  Trash/                            # session trash (never auto-emptied)
  .ds/cache/                        # thumbs/previews. Excluded from rsync, disposable
```

- **Variant** = an image plus an adjustment stack (virtual copy). Several variants can share one original.
- **Sidecar JSON** holds every adjustment (serde, versioned schema), the layers with their masks (vector strokes, not bitmaps), rating, color tag, keywords, and the crop. The format is plain and diff-able.
- **Session index** (SQLite) holds the fast-query mirror: file hash, EXIF, rating, tag, and a thumbnail pointer. "Rebuild index" re-reads the sidecars.
- **Session favorites/albums**: smart albums (by rating, tag, or date) and manual albums.
- **Global index** (phase 3): `~/Library/Application Support/Darkslide/global.db` lists every session path and file hash. It gives cross-session search and "already imported?" dedupe. It is only an index and is never required to open a session.

---

## 5. Import from a memory card

1. **Detect.** DiskArbitration (Swift) sees a mount that contains `DCIM/`. A non-modal import sheet opens (auto-open can be turned off).
2. **Scan.** List the files, read embedded thumbnails straight off the card, and show a grid right away. Anything **already imported** (hash match in the global index) is greyed out, with a "skip duplicates" toggle that defaults to on.
3. **Choose destination.** Pick an **existing session** or a **new session**. The name template defaults to `{date} {text}`, and the subfolder template is optional. You can also pick a rename template (`{camera}_{date}_{seq}`).
4. **Copy.** Files copy in parallel (bounded; card readers bottleneck at around 2–4 streams). Each file is written to `name.partial`, **BLAKE3-verified against the source**, fsync'd, then renamed. An optional **second copy** goes to a backup location at import time, separate from the rsync archive.
5. **Resume.** A pulled card or a crash leaves an import journal, and the next launch offers to resume.
6. **Finish.** "Eject card" is offered only after every file has verified. The app **never deletes from the card** (D6: optional "format reminder" only).
7. **After import (all background):** extract previews, then render real thumbnails visible-first. Apply the session's **auto-apply preset** if one is set (for example your usual WB/lens/sharpening). Then kick an archive sync.

---

## 6. Culling and browsing (the "fast" part)

- The grid and filmstrip show embedded previews first. A render replaces each one when ready, with no flicker (both have the same dimensions after orientation).
- Keyboard-driven: arrows move; `1–5` sets the rating; `0` clears it; `6–9`/`-` set color tags; `P`/`X` pick/reject; `Space` toggles 100% zoom at the cursor; `F` goes full screen.
- **Compare mode**: a pinned "select" image alongside candidates, with synchronized zoom/pan.
- **Prefetch**: decode the next and previous N images into the GPU cache while you look at the current one.
- Loupe and focus-mask overlay (phase 2).
- Filters: rating ≥, color tag, pick state, camera/lens/focal, and ISO range.

**Perf targets (acceptance tests, on your M4 Pro, 45 MP RAW):**

| Action | Target |
|---|---|
| Card inserted → first thumbnails visible | < 1 s |
| Next image in viewer (embedded preview) | < 50 ms |
| Next image, full develop render at screen res | < 250 ms |
| Slider drag frame | ≤ 16 ms |
| Import 500 files from a UHS-II card | card-bound (≥ 90% of reader throughput) |

---

## 7. Editing tools

**Pipeline** (linear float on GPU, wide-gamut working space):
decode → black/white levels → WB multipliers → demosaic (Bayer: RCD/AMaZE-class; X-Trans: Markesteijn, if needed, D2) → camera profile (matrix/DCP, D5) → exposure → highlight recovery → HDR shadows/highlights (local tone, guided filter) → levels → curve (RGB + luma) → color balance (3-way) → color editor → clarity/structure → **layers composite** → NR → sharpen → crop/rotate → output transform (ColorSync ICC).

**v1 tool tabs** (customizable, with drag-to-rearrange and favorites):
- **Customizing** (workspaces): every tab is a user-ordered stack. Add any tool to any tab (the same tool may sit in several), remove it, drag its header to reorder, move it to another tab, and create, rename and remove tabs. Tools ▸ lists every tool (choosing one shows it, putting it back in its default tab if it was removed, so no tool is ever unreachable), Customize Tools… (⇧⌘T), an All Tools tab (hidden by default), and Workspaces ▸ (Default, saved, save, delete). A named workspace keeps tabs, tools and collapsed panels.
- **Crop / Rotate / Straighten**: aspect presets and a straighten line tool. Keystone correction comes in phase 2.
- **White Balance**: picker, Kelvin + tint, camera-as-shot presets, and a skin-tone picker.
- **Exposure**: exposure, contrast, brightness, saturation.
- **High Dynamic Range**: highlights, shadows, whites, blacks.
- **Levels** and **Curve**.
- **Color Balance**: shadows/midtones/highlights wheels.
- **Color Editor**: basic (8 hue bands) and advanced (picked range with smoothness). Skin tone comes in phase 2.
- **Clarity / Structure**, **Sharpening**, **Noise Reduction**, **Vignetting**.
- **Lens correction** via `lensfun` data (distortion, CA, light falloff).
- **Styles & Presets**: saved partial stacks, with stacking of styles.

**Layers (phase 2)**
- Adjustment layers support a subset of tools per layer (exposure, WB, color balance, color editor, clarity, curve), plus opacity.
- Masks: brush (with auto-mask edge detection), linear and radial gradients, luminosity range, color range, invert, and add/subtract combine.
- AI masks (subject, sky, person) come from **Apple Vision** on-device (`VNGenerateForegroundInstanceMaskRequest`, person segmentation). No cloud, no model licensing.
- Healing/clone layers are **phase 3** (they're harder, and you said you do light edits).

---

## 8. Copy / Apply

Copy and apply adjustments:

- **⌘⇧C** (the large Copy button at the top right) copies in one click, like Capture One. By default it takes the groups edited on the photo, leaving out the per-photo ones (crop & straighten, heal & clone, layers); its ▾ switches to **Custom** checkboxes per group (layers per layer), kept between launches.
- **⌘⇧V** (Apply) pastes onto the **selection**, never onto the copied photo itself. For a whole session, album or filter, select all (⌘A) in that view first: collapsed stacks and photos the filter hides are not pasted onto.
- Per-group paste mode: **absolute** (default) or **relative** (exposure/temperature deltas), so a batch keeps its individual corrections.
- Aspect-aware crop paste (see §2).
- **Edit-all-selected** toggle: with it on, a slider move applies live to every selected image.
- "Copy from previous" (⌥⌘V) for sequential shooting.
- Every apply is **one undo step**, even across 500 images.
- Save a clipboard as a **preset**, which can also be auto-applied on import.

---

## 9. Archive (rsync) and Amazon Photos

### 9a. rsync archive
- **Target:** a mounted folder the user chooses, typically a NAS share (for example `/Volumes/Archive/Darkslide/`). If the NAS already backs that share up to a second machine, the archive gets **3 copies with no new infrastructure**. rsync runs on the Mac against the mounted share, never on the NAS, so no SSH keys are needed.
- **What syncs:** `Capture/`, `Selects/`, `Output/`, the sidecars, and a **consistent snapshot of the session DB** taken with the SQLite backup API into `.ds/index-snapshot.db`. The live WAL file is never copied. `.ds/cache/` is excluded.
- **When:** after each import, every N minutes (default 10) while a session has changes (FSEvents-debounced), and on session close.
- **Safety:** never `--delete` by default. Deletions and overwritten files go to `--backup-dir=.ds-archive-trash/{date}` on the target. It runs `--partial` and uses a lock file per target.
- **Binary:** macOS now ships **openrsync** (protocol 29). The app will bundle or require **Homebrew rsync 3.x** for `--info=progress2`, reliable `--partial`, and better perf (D7).
- **Offline target** (volume unmounted, NAS down): changes queue, a menu-bar badge shows "archive behind by N files", and the sync retries with backoff. Editing is never blocked. A notification after 24 h behind is optional.

### 9b. Amazon Photos (D3, **needs your call**)
Amazon has **no public Photos API**. The Amazon Drive developer API was closed off years ago, and Drive itself shut down at the end of 2023. "Impersonating Amazon Photos" therefore means one of these:

| Option | How | Risk |
|---|---|---|
| **A. Folder hand-off (recommended)** | Darkslide exports to a watched folder. The **official Amazon Photos Mac app** backs that folder up. Darkslide only manages what lands in the folder. | None; officially supported. The Amazon app must be installed and running. |
| B. Private web API | Call the service's undocumented web endpoints. | Against the terms of service and can break without notice. **Rejected; never built.** |
| C. Automate the official app | Drive the official app's upload through UI automation. | Brittle; no real advantage over A. |

What to upload is a separate choice (**D8**). Prime's unlimited full-resolution photo storage covers RAW, so uploading **originals** is free. But what you *view* in Amazon Photos looks better as **exported JPEG/HEIC of rated/picked images**. Default: exported selects (rating ≥ 3 or picked) via recipe "Amazon", full-res HEIC.

---

## 10. Decision points

| # | Decision | Options | Recommended default |
|---|---|---|---|
| **D0** | Name | Darkslide / something else | Darkslide (working name) |
| **D1** | GUI stack | (a) **SwiftUI/AppKit shell + Rust core via UniFFI**; (b) pure Rust with **GPUI** (Zed's framework: fast and pretty, but young, thin docs, weak on native controls/accessibility); (c) egui/iced (fast to build, won't feel like a Mac app) | **(a)**. "Nice GUI" plus macOS-only favors native. All hot paths are still Rust. |
| **D2** | RAW decoder | (a) **rawler** (pure Rust, from dnglab; good CR3/NEF/ARW/RAF/DNG coverage); (b) LibRaw via FFI (widest coverage, C++); (c) Apple CIRAWFilter (great support, but not our pipeline or look) | **rawler primary + LibRaw fallback**. **Depends on your camera(s): see Q1.** |
| **D3** | Amazon Photos | A folder hand-off / B private API / C UI automation | **A**. B is rejected. |
| **D4** | Source of truth for edits | **Sidecars + rebuildable index** / database only | **Sidecars**. This is the main anti-corruption and NAS-friendly design choice. |
| **D5** | Color science ("look") | (a) matrices from rawler/Adobe coefficients (neutral, flat); (b) **load Adobe DCP profiles** (from the free Adobe DNG Converter); (c) build our own film-like base curve per camera | **(a) plus a tuned base tone curve for v1, DCP support in phase 2.** Note: other vendors' proprietary per-camera profiles are not used or copied. |
| **D6** | Card handling after import | Never touch / offer "delete from card after verify" | **Never touch**. Format in camera. |
| **D7** | rsync binary | Bundle rsync 3.x (GPLv3, ok as a separate executable) / require Homebrew / use system openrsync | **Require Homebrew rsync, fall back to openrsync** with a warning. |
| **D8** | What goes to Amazon | Originals / exported selects / both | **Decided: backup only → originals (RAW).** |
| **D9** | Distribution | Personal (ad-hoc signed; note the TCC quirks with ad-hoc bundles) / Developer ID + notarized | **Developer ID if you have one**; **no App Sandbox** (it complicates removable volumes, network shares, and spawning rsync). |
| **D10** | Min macOS | 15 / 26 / 27 | **26**. It's personal, and newer Vision/SwiftUI APIs matter. |
| **D11** | Repo | A private Git host with CI (macOS runner needed for app builds; Linux runner can test the Rust core) / a public GitHub repository | Private first; public on GitHub under Apache-2.0 (docs/OPEN_SOURCE.md). |

**Questions only you can answer**
- **Q1. Answered:** Sony A9 (first gen, ILCE-9), ARW. Sample RAWs live in the git-ignored `samples/` folder.
- ~~Q1~~ Which camera(s) and RAW formats? Fuji X-Trans changes the demosaic work a lot, and CR3/ARW/NEF/DNG are all fine. **Please drop 5–10 sample RAWs somewhere** (for example `~/src/darkslide/samples/`, git-ignored) for the phase-0 spike.
- **Q2.** Typical shoot size and resolution (hundreds or thousands of images; 24 MP or 45–61 MP)? This sizes the caches.
- **Q3. Answered:** compatible with reading Capture One session information (ratings, tags, keywords and supported adjustments), read-only, with a dry run and revert.
- **Q4. Answered:** backup only.
- ~~Q4~~ Is Amazon Photos for *viewing/sharing* (then JPEG/HEIC selects) or as a *backup of originals*? (Your archive already covers backup.)

---

## 10b. AI features ("super easy")

| Feature | Model | Where it runs |
|---|---|---|
| Subject / person / background masks | Apple Vision `VNGenerateForegroundInstanceMaskRequest`, `VNGeneratePersonSegmentationRequest` | built into macOS, on-device |
| Sky mask (+ other scene classes) | SegFormer-B2 fine-tuned on ADE20K (has a *sky* class), ONNX ~100 MB | on-device via ONNX Runtime / Core ML EP |
| Auto-cull: eyes closed, faces | Vision face landmarks + capture quality (`VNDetectFaceCaptureQualityRequest`) | built-in |
| Auto-cull: aesthetics / "utility shot" | Vision `VNCalculateImageAestheticsScoresRequest` | built-in (macOS 15+) |
| Auto-cull: blur, clipping, bursts / near-duplicates | algorithmic (Laplacian variance, clipping stats) + Vision `VNGenerateImageFeaturePrintRequest` | built-in / Rust |
| Auto keywords | Vision `VNClassifyImageRequest` | built-in |
| Natural-language search ("kids on the beach at sunset") | Apple **MobileCLIP-S2** (Core ML, ~100 MB) | on-device |
| Auto straighten | Vision `VNDetectHorizonRequest` | built-in |
| Smart crop suggestion | Vision attention saliency | built-in |
| Auto WB / auto tone | algorithmic (gray-world + white-patch, histogram) | Rust |
| Natural-language edits ("warmer, crop 4:5, lift shadows") | Apple **Foundation Models** on-device LLM (macOS 26) with guided generation to a typed `EditIntent`; **TypeSafe Jev** as the classifier/extractor per standing preference when online | on-device / Jev API |
| AI denoise (phase 3) | NAFNet (SIDD) ONNX ~70 MB | on-device |

Nothing leaves the Mac except optional Jev calls (text only, never pixels).

## 11. Phases

**Phase 0: de-risking spike (≈1–2 weeks of agent time).** Each item has a pass/fail check:
1. Decode your sample RAWs with rawler, and with LibRaw where needed. → verify: all samples decode, and the output matches the embedded JPEG in geometry and orientation.
2. Embedded-preview extraction benchmark. → verify: < 20 ms/file from SSD.
3. wgpu/Metal pipeline (WB + exposure + curve + demosaic) at screen res. → verify: ≤ 16 ms/frame on a 45 MP file.
4. SwiftUI shell showing a Rust-rendered texture in a CAMetalLayer via UniFFI. → verify: 60 fps pan/zoom.
5. rsync to `/Volumes/pics` with the snapshot DB. → verify: target restores to an openable session.

**Phase 1: MVP (what you use daily).** Sessions, card import (§5), browser/culling (§6), v1 tools except layers (§7), copy/apply (§8), export recipes, rsync archive (§9a).

**Phase 2.** Layers and masks (including Vision AI masks), advanced color editor and skin tone, keystone, lens profiles, DCP profiles, compare mode, focus mask, styles.

**Phase 3.** Global cross-session index and search, Amazon Photos (per D3), healing/clone, XMP export, and anything the MVP showed was missing.

**Testing approach.** The Rust core is unit-tested, with golden-image tests for the pipeline (per-pixel ΔE tolerance), property tests on sidecar round-trips, and an import test that simulates a pulled card mid-copy. The perf targets in §6 run as benchmarks (criterion) in CI.

---

## 12. Risks

- **Color quality is the long pole.** Getting a "pleasing default look" per camera takes iteration, and D5 is where that happens.
- **Demosaic/NR quality vs. speed on GPU**: a proven algorithm set (RCD, AMaZE-lite, non-local means / wavelet NR) keeps this manageable, but it is real work.
- **Amazon option B** could break or flag the account. That's why it isn't the default.
- **Scope creep**: a full professional RAW suite is decades of work. The phases above cut toward *your* workflow (cull → WB/crop/color → copy/apply → archive).
- **Legal**: functional similarity is fine. Don't copy other products' names, icons, or proprietary profiles.
