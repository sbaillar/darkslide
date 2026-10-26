# FFI: the Rust ↔ Swift boundary

`crates/lt-ffi` is the only crate the app links. It uses **UniFFI 0.32** with proc macros (no UDL) and stays thin:
- it converts FFI records to domain types;
- it runs work on the rayon pool;
- it writes pixels straight into caller-owned memory;
- it delivers progress on its own thread.

FFI contract revision: **46** (`FFI_CONTRACT` in Rust, `expectedFFIContract` in Swift; `--self-test` checks that they match). Revision 5: `pick_white_balance` takes a `sample` size. Revision 6: `raw_extensions()`. Revision 7: content-aware Remove and people masks. Revision 8: camera profiles (`profiles`), match exposure / white balance (`matching`), and `load_all_adjustments_json` returns `AllAdjustmentsJson` (edits, plus untouched Neutral variants as ids sharing one JSON). Revision 9: the Capture One calls. Revision 10: `create_session` (New Session). Revision 11: natural-language search (`session::search`, including `forget_session_in_search`). Revision 12: effects — the `Effects` copy group and the depth map for the lens blur. Revision 13: Find and People (`session::find`, `session::people`, `session::places`; `VariantRecord.people`). Revision 14: Find/People merged with effects (combined contract). Revision 17: metadata editing (`session::metadata`; `VariantRecord.metadata`, `ExportSource.metadata`, `ImportOptions.metadata_preset`, `RawMetadata.artist`/`copyright`, and `ImageEncoder.encode` takes the `metadata`). Revision 21: AI Upscale (`upscale`, `SessionHandle.add_derived`, `ExportRecipeSpec.ai_upscale`) together with merges, watermarks and metadata. Revision 22: `begin_upscale(path, name, token)` and `SessionHandle.free_derived_name`. Revision 23: metadata match reasons (integration). Revision 24: soft proofing (`proof::proof_lut`, `DevelopFormat::Rgba16fLinearRec2020`) and `ExportRecipeSpec.output_profile`. Revision 25: the map (`session::map`: `set_geotag`, `global_map_points`; `RawMetadata.gps`, `VariantRecord.geotag`/`file_gps`). Revision 30: the Mac ↔ iPad link (`remote`). Revision 34: Lightroom import (`lr`: `lightroom_source_kind`, `load_lightroom_source` -> `LightroomSource`, `SessionHandle.import_lightroom` / `can_revert_lightroom_import` / `revert_lightroom_import`, `read_lightroom_presets`). Revision 35: `SessionHandle.set_ai_changes`/`ai_changes` (the sidecar's `ai_changes`). Revision 36: the Library (`session::library`: `SessionHandle.sync_library`, `library_refresh`, `library_query`, `library_items`, `library_overview`, `library_edit`, the collection calls; `DevelopThumbnails.cached_thumbnail`). Revision 37: Google Photos export (`gphotos`: `gphotos_authorization_url`, `gphotos_code_from_redirect`, `gphotos_exchange_code`, `gphotos_revoke`, `gphotos_albums`, `gphotos_upload`). Revision 38: plug-ins (`plugins::PluginHost`, `edit_keys()`, `ExportRecipeSpec.post_process`) merged with the link and the Capture One settings. Revision 39: `set_rawler_helper` (the RAW helper). Revision 40: AI Cull reject notes (`CullInput.name`, `CullResultRecord.reject_reasons`/`reject_note`, `VariantRecord.cull_note`, `SessionHandle.set_cull_note`). Revision 41: Aperture and iPhoto import (`apple`: `apple_library_kind`, `load_apple_library` -> `AppleLibrarySource`, `SessionHandle.import_apple_library` / `can_revert_apple_import` / `revert_apple_import`). Revision 42: Immich (`immich`: `ImmichHttp`, `immich_test_connection`, `immich_detect_mapping`, `immich_sync_albums`, `SessionHandle.immich_mark_developed` / `immich_developed` / `immich_develop_plan` / `immich_developed_rendered` / `immich_refresh_sidecars`, `ArchiveManager.set_immich` / `immich_pending`), and richer export metadata: `ExportSource.rating`/`label`/`pick`, `ExportRecipeSpec.label_tags`, and `ImageEncoder.encode` takes the `xmp` packet. Revision 43: optional integrations as bundled plug-ins (`features`: `PluginHost.native_features` / `feature_enabled` / `undecided_features` / `seed_features`, `ArchiveManager.set_plugin_host`). Revision 44: archive destinations (`archive`): `ArchiveConfig.object_store` (`ObjectStoreSettings`), `ArchiveManager.set_object_store(http, credentials)` with the `ArchiveHttp` foreign trait (`ArchiveHttpRequest`/`ArchiveHttpResponse`/`ArchiveHeader`) and `ObjectCredentials`, `archive_object_test_connection` -> `ObjectConnectionTest`, `archive_object_settings_problem`, and `TargetCheck.kind` (`volume`, `local` or `object`). Revision 45: optional integrations (43) merged with archive destinations (44). Revision 46: the rename to Luxtable: crates renamed `lt-*`; the UniFFI namespace is `lt_ffi` (library `liblt_ffi.a`, module `lt_ffiFFI`); `OpenReport.legacy` (a session moved from Darkslide's `.ds` layout to `.lt`) and `relocate_global_index` (after the support folder is copied to its new name).

## Build

```
scripts/ffi.sh          cargo build --release -p lt-ffi, then regenerate the bindings
scripts/build-app.sh    ffi.sh → swift build -c release → dist/Luxtable.app → codesign
scripts/test-app.sh     ffi.sh → swift test --enable-code-coverage → coverage report
```

- `liblt_ffi.a` (staticlib) is copied alone into `app/.rust/lib/`, so the linker can't pick the dylib.
- Bindings are generated in library mode from `target/release/liblt_ffi.dylib` by the crate's own `uniffi-bindgen` binary (`--features bindgen`, built into `target/bindgen/`).
- Generated files are git-ignored and copied only when their content changes:
  - `app/Sources/lt_ffiFFI/{lt_ffiFFI.h, module.modulemap}`
  - `app/Sources/LuxtableFFI/lt_ffi.swift`

## What is exported today (contract 38)

| Rust module | Exports | Swift wrapper (`app/Sources/LuxtableCore/Rust/`) |
|---|---|---|
| `lib` | `version()`, `ffi_contract()` | `RustCore` |
| `error` | `DsError { InvalidArgument, Cancelled, Io, Unsupported, FolderMoved, Internal }` | `RenderError` (`.kind` + message) |
| `jobs` | `CancelToken` object: `new()`, `cancel()`, `is_cancelled()` | used by every wrapper |
| `events` | `ProgressEvent`, `TaskPhase`, `ProgressListener` (foreign trait) | `ProgressStream` (AsyncStream) |
| `frame` | `RenderRequest`, `FrameFormat`, `NormRect`, `FrameInfo`, `plan_frame(src_w, src_h, request)` | `RustRenderer` maps `LuxtableCore.RenderRequest` |
| `engine` | `Engine`: `new()`, `set_progress_listener`, `flush_progress`, `dropped_progress_events`, `async render_test_pattern_into(src_w, src_h, params, request, dest_ptr, dest_len, token)` | `RustRenderer: Renderer` |
| `raw` | `async raw_metadata(path, token) -> RawMetadata`, `async embedded_preview(path, token) -> EmbeddedPreviewData`, `is_supported_raw(path)`, `raw_extensions()` | `RustRaw.photo(at:id:)`, `RustRaw.isSupported(_:)`, `RustRaw.extensions`, `RustPreviewProvider: PreviewProvider` |
| `clipboard` | `AdjustGroup`, `AdjustGroupInfo`, `adjust_groups()` | `RustCore.groups` |
| `tools` | `develop_info`, `auto_white_balance`, `auto_tone`, `pick_white_balance(path, adjustments_json, x, y, sample, token)`, `white_balance_from_multipliers`, `lens_profiles`, `DevelopThumbnails` | `RustDevelopTools: DevelopTools` |
| `session` | `async open_session(dir, token)`, `async create_session(dir, name, token)`, `async open_folder_in_place(folder, fallback_store, folder_identity, moved, auto_apply, listener, token)`, `volume_identity(folder)`, `note_user_activity()`, `folder_can_hold_session(folder)`, `default_adjustments_json()`, `normalize_adjustments_json(json)`; `SessionHandle`: `name`, `root`, `in_place_folder`, `open_report`, `async variants`, `async hash_pending(listener)`, `async remove_missing(ids)`, `async locate(id, path)`, `async restore_previous(id)`, `async dismiss_file_changed(id)`, `async restore_to_folder(id)`, `async set_auto_apply_preset_json`, ratings/tags/picks, adjustments load/save/batch, `async set_optimize_report(ids, report_json)`, `async optimize_report(id)`, `async set_ai_changes(ids, changes_json)`, `async ai_changes(id)` | `RustSessionStore: FolderSessionOpening, OptimizeReportStoring, AIChangesStoring` |
| `session::search` | `SessionHandle`: `async embedding_jobs(model, global_index)`, `async store_embeddings(model, records, global_index)`, `async embedding_status(model)`, `async search_embedding(model, query, filter, options)`, `async find_similar(model, variant_id, filter, options)`, `async suggest_keywords(model, variant_id, vocabulary)`; `async search_all_sessions(global_index, model, query, options)`, `async forget_session_in_search(global_index, root)`, `user_is_active()`, `ClipTokenizer.from_merges_file(path)` / `encode(text)` | `RustSearchBackend: SearchBackend`, `ClipEncoder` |
| `session::people` | `SessionHandle`: `async face_jobs(model)`, `async store_faces(model, key, faces)`, `async face_status(model)`, `async people_overview(model, people_store, options)`, `async name_faces(model, faces, name, people_store)`, `async reject_faces(model, faces, person_id, name, people_store)`, `async merge_faces(faces)`, `async split_faces(faces, rest)`; `known_people(people_store)`, `async rename_person(session, people_store, person_id, name)` | `RustPeopleBackend: PeopleBackend`, `VisionFaceEmbedder`, `FaceIndexer`, `PeopleModel` |
| `session::places` | `SessionHandle`: `async scan_locations(limit)` | `RustPeopleBackend.scanLocations` |
| `session::map` | `SessionHandle`: `async set_geotag(ids, position)`; `async global_map_points(global_index)` | `RustSessionStore.save(photos:)` (geotags), `RustMapPointSource: MapPointSource`, `MapModel` |
| `session::metadata` | `PhotoMetadata` (IPTC fields); `SessionHandle`: `async set_metadata(ids, metadata)` (undoable; import and folder registration read EXIF artist/copyright and XMP as defaults) | `RustSessionStore` (`Photo.metadata`), `MetadataPresetStore` |
| `session::find` | `SessionHandle`: `async sync_find(global_index, face_model, people_store, suggest_threshold)`; `async find_prompts(text, global_index, people_store, today, model)`, `async find_photos(text, global_index, people_store, today, model, text_vector, place_vectors, options)` | `RustFindBackend: FindBackend`, `FindModel` |
| `optimize` | `async optimize_analyze(path, adjustments_json, signals, prior_report_json, masks, token) -> report JSON`, `optimize_apply(adjustments_json, report_json, selected) -> OptimizeApplied`, `optimize_still_applied(adjustments_json, report_json) -> [id]` | `RustPhotoOptimizer: PhotoOptimizing` |
| `session::merge` | `MergeKind`, `Deghost`, `PanoProjection`, `MergeOptions`, `MergePreview`, `MergeOutcome`; `async merge_preview(paths, options, max_edge, token)`; `SessionHandle`: `async merge_photos(variant_ids, options, listener, token)` (progress kind `"merge"`, permille) | `RustPhotoMerger: PhotoMerging` |
| `c1` | `is_capture_one_folder(folder)`; `SessionHandle`: `async import_capture_one(dry_run, overwrite_edits, token) -> C1ImportReportRecord`, `can_revert_capture_one_import()`, `async revert_capture_one_import(token) -> Option<C1RevertRecord>` | `RustSessionStore: CaptureOneImporting` |
| `plugins` | `PluginHost` object: `open(builtin_dir, user_dir)` (also the host export steps resolve through), `list() -> [PluginRecord]`, `state_error`, `preview(path)` (stages a folder or zip; returns a `token`), `install(token, allow_downgrade)`, `discard(token)`, `forget(id)`, `set_enabled`, `approve_files`, `grant(id, permission, folder, protected)`/`revoke`, `trust_signer`/`untrust_signer`, `allow_unsigned_code`/`set_allow_unsigned_code`, `add_dev_folder`/`remove_dev_folder`/`dev_folders`, `user_folder`, `reload`, `data_packs(kind)`, `post_processors()`; `edit_keys()`; `ExportPostProcessStep` (with `plugin_version`) in `ExportRecipeSpec.post_process` | `PluginStore`, `PluginContent`, `RecipePostProcess`, `LookJSON` |
| `upscale` | `async begin_upscale(path, name, token) -> UpscaleTask` (`tile_count`, `tile_size`, `output_scale`, `read_tile`, `write_tile`, `async finish(token) -> path`), `set_upscale_provider(provider)` (`UpscaleProvider`, foreign trait); `SessionHandle`: `async free_derived_name(source_path, suffix)`, `async add_derived(path, source_variant_id)` | `RustUpscaleBackend`, `AppUpscaleProvider`, `RustSessionStore: DerivedPhotoAdding` |
| `remote` | `RemoteServer`: `start(state_dir, name, port, all_interfaces)`, `port`, `fingerprint`, `set_sessions`, `begin_pairing(host) -> RemotePairing`, `cancel_pairing`, `pairing_open`, `status -> RemoteStatus`, `take_changed_sessions`, `set_open_session(handle, name)`, `stop`; `remote_mark_seen(handle, generation)` (with `AllAdjustmentsJson.device_generation`), `remote_devices(state_dir)`, `remote_revoke(state_dir, fingerprint)`, `remote_set_write(state_dir, fingerprint, write)`, `remote_default_port()` (see [REMOTE.md](REMOTE.md)) | `RustLinkBackend: LinkBackend`, `RemoteLinkModel` |

### Effects and the lens blur's depth map (contract 12)

- `AdjustGroup::Effects` (after `Vignette`): lens blur, glow, grain and halation (`Adjustments::effects`, JSON in `lt_core::effects`). The sidecar's `look` record (the one-touch look last applied, for its Intensity slider) belongs to no group: Copy/Apply never pastes it.
- `DevelopMaskSource::depth_map()` and `ExportMaskSource::depth_map(source_id)`: the photo's depth map (a `DevelopMask` in the E frame, 255 = nearest), or `None` while the app hasn't computed it — a depth-mode lens blur then renders nothing. With no focus point picked, the pipeline focuses on the mean depth under `ai_mask(Subject)` (the 90th depth percentile without one), so the app serves the subject mask with the depth map.

### Content-aware Remove and people masks (contract 7)

- `DevelopInpainter` (foreign trait, the app's Core ML LaMa): `model_id()`, `max_edge()`, `inpaint(width, height, rgb, mask) -> Option<Vec<u8>>` (display-encoded RGB8 and an 8-bit mask, 255 = fill; `None` falls back to the built-in fill). Called synchronously from render threads.
- `set_inpainter(Option<DevelopInpainter>)`: used by every later render (viewer, thumbnails, export, prefetch). `removal_engine()` names what fills new removals. `set_removal_cache_dir(Option<String>)` keeps fills on disk (the app passes `<support>/Cache/Removals`).
- `DevelopAiTarget` gained `Face, Skin, Eyes, Teeth, Hair, Clothes`; `DevelopMaskSource::ai_person_mask(target, person)` and `ExportMaskSource::ai_person_mask(source_id, target, person)` serve one person's mask (all people stay on `ai_mask`).

### Frames: request → plan → render into caller memory

```rust
enum FrameFormat { Rgba8Srgb, Rgba16fLinearP3 }            // 4 or 8 bytes per pixel
struct RenderRequest { max_long_edge: u32, scale: Option<f32>, region: Option<NormRect>, format: FrameFormat }
struct FrameInfo { width, height, bytes_per_row, format, full_width, full_height, origin_x, origin_y, region: Option<NormRect> }
fn plan_frame(source_width: u32, source_height: u32, request: RenderRequest) -> Result<FrameInfo, DsError>
```

- `scale: None` is a fit render: the whole image with its long edge `min(max_long_edge, source long edge)`. It never upscales.
- `scale: Some(1.0)` is true 100%: one output pixel per source pixel. `region` cuts a tile out of that full-size image. The viewer's 1:1 path requests only the visible region.
- The largest frame edge is 16 384 px, and the largest virtual full image is 65 536 px. A 1:1 request larger than 16 384 px is refused unless it asks for a region.
- Rows are padded to 256 bytes (`bytes_per_row`). That is what Metal wants for buffer-backed textures.
- `Rgba16fLinearP3` is IEEE half, linear light, Display P3 primaries, D65. Values above 1.0 are EDR headroom (+1 EV white = 2.0). Alpha is 1.0.
- `render_*_into` writes rows straight into `dest_ptr`. The Swift side allocates a shared-storage `MTLBuffer` and passes `contents()`.

**Memory contract for `dest_ptr`.** From the call until its future completes, `dest_ptr .. dest_ptr + dest_len` must stay valid and writable, and the caller must not read or write it. The length must be at least `FrameInfo::byte_len()`.
- `RustRenderer` keeps the buffer alive across the `await` (`FrameDestination`).
- Rust validates non-null and the length, and turns the address into a slice exactly once, in `Dest::slice` (`// SAFETY:` comment there).
- `run_job` does not complete, or drop, before the worker has released the slice (`WaitForWorker`). So memory is safe even if the future is abandoned.

**Copies per rendered frame**
- Before (contract 1): Rust `Vec` → `RustBuffer` → Swift `Data` → `MTLTexture.replace`. That is 3 full CPU copies plus the texture upload.
- Now: Rust writes into the `MTLBuffer` the texture is made from. That is **0 CPU copies**.
- Next target is IOSurface: an `IOSurface`-backed buffer shared with the display compositor, for zero-copy EDR presentation and cross-process previews. The FFI shape stays the same (pointer + length + stride).

### Cancellation

UniFFI 0.32's Swift bindings never call `rust_future_cancel`, so cancelling a Swift `Task` alone does not reach Rust. So:
1. Every async export takes `token: CancelToken`.
2. Swift wraps each call in `withTaskCancellationHandler { try await … } onCancel: { token.cancel() }`.
3. Workers poll the token between bands of 64 rows (renders), or before starting (RAW reads).
4. On cancellation Rust returns `DsError::Cancelled`, which Swift rethrows as `CancellationError`. A 1:1 6000×4000 render stops within one band (well under 5 ms on an M4 Pro). Only the bands already done were written: see `cancelled_render_writes_only_a_few_bands` (Swift) and `cancellation_stops_between_bands_leaving_later_rows_untouched` (Rust).

Dropping the Rust future also cancels, for other runtimes. It waits for the worker, per the memory contract above.

### Panics

`run_job` wraps the work in `catch_unwind`, so a panic (a bug, or a panicking dependency) becomes `DsError::Internal { message: "render worker panicked: …" }` instead of aborting the app. That matters because rayon aborts the process on a panicking `spawn`.
- A oneshot `Canceled` (the worker vanished without a result) also maps to `Internal`, not `Cancelled`.
- Swift shows Internal errors: `RenderError.kind == .internalError`, message prefixed "Internal error:", and `ViewerModel.renderError` displays it.

Sync exports that can fail (or could panic) also return `Result` and run inside `guard` (`catch_unwind`), so Swift calls them with `try`, never `try!`. The exceptions are the constructors of `CancelToken`/`Engine`/`CullSession` and `CancelToken.cancel/is_cancelled`, `Engine.set_progress_listener/flush_progress`: they only touch atomics and poison-tolerant mutexes (they cannot panic), and they are called from `onCancel:` handlers and initialisers across the app.

### Progress

- Workers never call Swift. They enqueue `ProgressEvent`s on an unbounded FIFO, and one delivery thread per `Engine` (`lt-progress`) calls the listener in order, never while holding a lock.
- While 64 `Running` events are undelivered, new `Running` events are dropped (counted in `dropped_progress_events()`). Lifecycle events (`Started`, `Finished`/`Cancelled`/`Failed`) are never dropped, so a task always visibly ends.
- A panicking listener doesn't stop delivery.
- `flush_progress()` waits until everything queued so far was delivered (tests, teardown).
- Swift: `ProgressStream` is the listener. It yields into an `AsyncStream` with `.bufferingNewest(n)`, so there is no `Task` per event.
- The engine holds the listener strongly, and `ProgressStream` references nothing, so there is no cycle. Call `finish()` or `setProgressListener(nil)` on teardown.

### RAW access (lt-raw)

- `raw_metadata`: make/model/lens, ISO, shutter (seconds + display string), aperture, focal (+35 mm), capture time (wall clock + UTC offset if recorded), EXIF orientation, **oriented** width/height, serial, shutter count, AF point (normalized, oriented), shot number, and burst sequence number.
- Swift maps it with `RustRaw.makePhoto`. Without an offset, the camera clock counts as local time.
- `embedded_preview`: the largest embedded JPEG as stored, plus its size and EXIF orientation. It crosses the FFI as `Data`, one copy of about 1 MB on the A9. Swift decodes it with ImageIO (downscaling during decode), rotates it upright with Core Graphics, and keeps an LRU (`RustPreviewProvider(cacheLimit:)`).
- Errors:
  - `Io`: missing, truncated, or malformed file.
  - `Unsupported`: not a RAW, unsupported camera, or no preview.
  - `Internal`: decoder failure.

### White-balance picker (`tools`)

`pick_white_balance` returns the Kelvin/tint that neutralizes the `sample`×`sample` full-resolution pixels centred on the pixel under `(x, y)` (normalized in the final, cropped and turned frame; the box shifts inside the frame at its edges; `sample` ≤ 31, `0` = the older 0.8%-of-frame patch). The pixels are a 1:1 `Quality::Full` region render, so they are exactly the pixels the viewer's loupe shows and outlines. It is deterministic: the app shows the value in the loupe and the click applies that same value (`tests: sampled_picker_reads_the_exact_pixels_and_neutralizes_them`).

### Sessions and open-in-place (lt-session)

- **External originals.** Opening a folder in place creates a session whose originals are the user's files (`Originals::External(folder)`); nothing is linked, copied, moved or deleted. `move_to`, `empty_trash`, relink-by-move and deleting originals are refused for these sessions (`Unsupported`).
- **Where the session lives.** `<folder>/.lt/session/` when the folder is on a local, writable APFS/HFS+ volume (`statfs`: not `MNT_RDONLY`, `MNT_LOCAL`, and writable). Otherwise (FAT/exFAT cards, read-only media, SMB/NFS) it goes to `fallback_store`, which Swift sets to `~/Library/Application Support/Luxtable/InPlace/<volume key>/<hash of the path on that volume>/`. The volume key is the volume UUID, or for volumes without one (SMB/NFS shares, some cards) `vol-<hash>` of `volume_identity(folder)` (`statfs` mount source and file system type, e.g. `net:smbfs://user@nas/pics`), so two shares never share a store (R4-H1). Swift also passes `folder_identity` (volume key plus path); the store records it and a fallback store made for another folder is never taken over (a separate store is made next to it) unless `moved` is set (a bookmark proved the folder moved). Nothing is written to the volume. Without `fallback_store`, such folders fail with `Unsupported`.
- **First open.** The volume is checked before anything is created. A failed or cancelled first open removes what it created, including a `.lt` it made (but never a store another process created meanwhile, L4). Files are registered by stat and listed at once. Hashing (BLAKE3) runs afterwards via `hash_pending` (progress `kind` "hash", determinate). Progress kinds for the open are "open.scan" and "open.register". A RAW that can't be read is reported in `OpenReport.failed` (L11).
- **Walk limits.** The walk goes at most 8 levels deep. It skips hidden entries, `.lt`, and bundles (`.app`, `.photoslibrary`, …), and stops at 100,000 files; `OpenReport.truncated_at` then carries the count seen.
- **Metadata** is read once per file and cached in `<store>/.lt/cache/raw-meta.json`, keyed by path, size and mtime, so listing never re-reads RAW headers.
  - 1,757 A9 files: the first open plus listing takes 0.54 s warm; a reopen plus listing takes 35 ms; background hashing takes 64 s (87 GB).
- **Identity** (R4-H2/H3, M1/M2). Two hashes per original: the whole file (`Sidecar::hash`, the import's dedupe key) and the sensor data only (`Sidecar::content_hash`, BLAKE3 over `lt_raw::image_data_ranges`: the raw IFD's strips/tiles of TIFF-based RAWs; `None` for other files). An open only looks at `(size, mtime, inode)`:
  - **Unchanged stat:** the same file. On FAT/exFAT (inodes are reused) hashed files also get a cheap sample check (first/last 64 KiB) in the background.
  - **Renamed** (same size, mtime and inode at a new path): followed at once, sidecars move with it.
  - **Changed stat:** kept provisionally (`OpenReport.to_verify`) and checked by `hash_pending`: the same bytes, or the same sensor data with rewritten metadata (exiftool, geotaggers), keep everything (the new whole-file hash is stored); a real change sets the old metadata aside (sidecars to the session Trash) and registers the file with `file_changed = true`, persisted in its sidecar (`replaced`: the previous variant ids) until `restore_previous` (the old ids, ratings and edits come back and describe this file) or `dismiss_file_changed`. A file that can't be read is never "different": it is kept as it was and retried on the next open.
  - **Missing file:** new files of the same size are held back (`OpenReport.held`) and hashed in the background; a match by whole-file or sensor-data hash is relinked. Identical duplicates are paired by name (same trailing number, then longest common part) only when that is unambiguous; otherwise they are reported in `HashReport.ambiguous` and never guessed or swapped. Unmatched files are registered as new. `remove_missing` forgets photos whose file is still gone (L5); `locate` points a photo at a file with the same whole-file or sensor-data hash.
  - **Not yet hashed, same size and mtime (a copied folder, a remounted card) or same size and inode (a `touch`):** treated as the same file.
  - **Known limit (L2):** an in-place rewrite that keeps size and mtime (`exiftool -overwrite_original_in_place -P`) is not noticed on APFS/HFS+ (stat-based identity; on FAT/exFAT the sample check catches it). Such tools only rewrite metadata, so ratings and edits still describe the same pixels.
- **Background pass** (`hash_pending`, R4-M3). Runs on its own thread with two utility-QoS readers (`F_NOCACHE`, plain buffered reads, not the rayon pool previews and saves use), takes the session lock only to record results in batches, and pauses while the app reports user activity (`note_user_activity()`, 500 ms idle gate). `HashReport.listing_changed` tells the app to list again. Cancelling keeps the remaining work for the next call.
  - 1,757 unique A9 files (APFS clones, 81 GB logical): reopen after a Finder bulk rename 268 ms (was 60.8 s), after `touch` of every file 93 ms (was 59.9 s); the background check then takes about 60 s. While it hashes: `embedded_preview` p95 1.0 ms and `set_rating` p95 8.0 ms (were 326 ms and 221 ms).
- **Folder moved while open.** Writes return `FolderMoved`. Swift resolves the folder's bookmark, reopens it at the new path with the same store, and retries.
- **Migration.** Round-3 hard-link sessions are converted to external ones on open. A `Capture/` link is removed only when it is the same inode as a regular (non-symlink) original outside the store with `nlink > 1`, re-checked just before removal (L1). A plain copy whose bytes equal the folder's file at the same path is a verified duplicate and removed (a copied round-3 folder, `OpenReport.duplicates_removed`). Anything else is the only copy: kept, reported in `OpenReport.kept_in_store`, flagged `kept_in_store` on its missing photo, and `restore_to_folder` moves it back (never over an existing file). Journal replay never writes through a symlink (L3). In a Capture One session folder (a `*.cosessiondb` at the top) `Trash/`, `Output/` and `CaptureOne/` are not listed (L9).
- **Batches.** Writes that touch more than one sidecar are journaled (`<store>/.lt/journal/`) and rolled forward on the next open after a crash.
- **New empty session** (`create_session`, File ▸ New Session…). Makes `<dir>` with `Capture/`, `Selects/`, `Output/`, `Trash/` and its settings, and opens it. Swift passes `<sessions folder>/<name>`, or `<chosen folder>/<name>` when the user picks a location. A `dir` that exists and isn't empty, or a name a folder can't have, is `InvalidArgument`; nothing is overwritten. Closing a session in Swift (File ▸ Close Session) just drops the `SessionHandle` once pending writes are flushed; there is no close call.
- **Auto-apply (L1).** `open_folder_in_place(auto_apply:)` stores the preset in the session and applies it to newly registered files only.
- **Schema.** Sidecar schema 1; the crop convention is the one documented on `lt_core::adjust::Crop`. The new `content_hash`, `replaced` and `optimize_report` fields are additive (serde defaults; `optimize_report` is written as `null` when cleared so a rewrite can't merge the old one back). `ai_changes` (the app's AI Changes list: each AI step's rows and the edit before it, opaque JSON) is additive the same way. The index is v3 (new columns; a v2 index is upgraded in place, keeping verified stats).

### AI Optimize (`optimize`)

- `optimize_analyze` measures the photo with its edit and returns `lt_pipeline::optimize::OptimizeReport` as JSON: proposed changes (field, old/new value, a machine-readable reason with its measurements, a confidence, a templated explanation), the fields left alone and why, and a summary.
- `signals` (`OptimizeSignals`) carry the app's scene analysis in the E frame: faces and their area, Vision's horizon, the salient subject, the sky mask and its share, scene tags (`landscape`, `portrait`, `night`, `snow`, `beach`, `sunset`, `backlit`), and whether the AI Denoise model is there.
- `prior_report_json` is the photo's last applied report (the sidecar's `optimize_report`). A report remembers who set each value (`owned`): what any earlier optimize set and the user hasn't touched since is the optimizer's, and a default the user restored over an optimizer value is the user's. So later runs propose nothing new and never call the optimizer's values the user's.
- The probes render the original RAW (never an AI Denoise blend) and honour the call's `CancelToken` between renders. With AI Denoise on, the measurements are therefore of the noisier original: on a noisy frame the extreme percentiles (clipping, crushing, the levels end points) read slightly wider, and local contrast, texture and the dark-channel haze score slightly higher than the denoised photo has, so the levels proposal is a little more conservative and clarity/dehaze slightly smaller. The noise rules measure the RAW anyway, so they are unaffected.
- `optimize_apply` applies the `selected` change ids and returns the adjustments plus the report marked with what was applied (what the app stores).
- `optimize_still_applied` lists the applied changes whose values the edit still has ("Why these settings?" shows only those after an undo, a paste or a slider move).
- Values and texts come from deterministic rules and templates. No language model writes or rephrases them: a rephrasing can't be checked for meaning (a flipped direction or a number on the wrong setting reads as plausible), so the app shows the templates.

### Capture One import (lt-c1)

- `SessionHandle.import_capture_one` works on the in-place session of a Capture One session folder. Any other session gets `InvalidArgument`, and a folder that moved gets `FolderMoved`. The C1 folder is only read: the database is copied to a temp dir and the `.cos` files are read. See `docs/C1_FORMAT.md`.
- `dry_run` writes nothing and returns the same report a real run would. Cancelling (the token) stops the import while it plans, before anything is written.
- A field changed in Luxtable since the last import, and also changed in Capture One, comes back in `conflicts` and is kept. `overwrite_edits` replaces those fields.
- White balance converts through the original's camera matrix (`lt_raw::color_info`) relative to C1's as-shot multipliers. If the original can't be read, white balance is listed in `unmapped`.
- The report counts:
  - rated / tagged / picked / keyworded / `edits_mapped` (what was or would be written);
  - `mapped` and `unmapped` keys (with reasons, most frequent first);
  - images in C1's Trash, outside the folder, or not found;
  - `in_sync` and `kept_luxtable`;
  - albums created / updated, and `collections_skipped` (with reasons).
- `revert_capture_one_import` takes back the imports since the last revert (see "Reverting" in `docs/C1_FORMAT.md`). It returns `None` when there is nothing to revert. `can_revert_capture_one_import` says whether there is.
- Contract 8 adds the Capture One calls (import, detection, revert).
- Swift: after a real import or a revert, `RustSessionStore` drops its cached records of the session (`forgetAll(in:)`), and `AppModel` reloads it. Detection (`is_capture_one_folder`) runs in the store when a session opens, never on the main actor; `AppModel.captureOne` caches the result for the menus.
- UI:
  - File ▸ Import Capture One Edits…, and an offer after opening a C1 folder when the dry run finds something to import (once per folder per launch).
  - The sheet shows the dry run: counts per field and per tool, the unmapped keys with counts, and the conflicts per photo. A toggle, off by default, reads "Overwrite my Luxtable edits on conflicts".
  - Then the import, then the result, with Revert Import.
  - File ▸ Revert Capture One Import.

### Lightroom import (`lr`)

Mirrors the Capture One calls, for a Lightroom catalog, a folder of XMP sidecars or presets (format notes: `docs/LR_FORMAT.md`). Lightroom's files are only read.
- `lightroom_source_kind(path)` says `catalog`, `sidecars`, `presets` or `unknown` without reading the source fully.
- `load_lightroom_source(path, token)` reads a catalog (copied and the copy opened) or a sidecar folder **once** into a `LightroomSource` object; `summary()` gives its counts and warnings. The Swift side keeps the object so every dry run and the import reuse it: a multi-GB catalog is copied a single time.
- `SessionHandle.import_lightroom(source, categories, dry_run, overwrite_edits, token)` matches the source's photos to the open session (any session; path matching needs one opened in place) and returns `LrImportReportRecord`: counts per category, the settings that mapped (with exact/approximate) and those dropped (with reasons), per-photo notes, photos not found (the first 500, plus the true count) and per folder, ambiguous matches, conflicts and warnings. `categories` are `lt_lr::Category` keys (`rating`, `label`, `flag`, `keywords`, `metadata`, `collections`, `crop`, `tone`, `white_balance`, `color`, `detail`, `effects`, `lens`, `masks`); one left out is neither touched nor recorded. A dry run writes nothing.
- `can_revert_lightroom_import()` and `revert_lightroom_import(token)` take back the imports since the last revert (`None`: nothing to revert).
- `read_lightroom_presets(path, token)` maps each preset to Luxtable adjustments (`adjustments_json`, the groups it sets, notes on what was dropped or approximate). The app saves them through its own preset store.

### Aperture and iPhoto import (`apple`)

The Lightroom calls' twin for Apple's discontinued photo apps (format notes: `docs/APPLE_FORMAT.md`). The library is only read.
- `apple_library_kind(path)` says `aperture`, `iphoto` (9.3 and later), `iphoto-xml` (6 to 9.2), `photos` (a Photos library: not supported), `old-aperture` (Aperture 1 or 2: not supported) or `unknown`, from file names only.
- `load_apple_library(path, token)` reads the library **once** (its databases copied and the copies read) into an `AppleLibrarySource`; `summary()` gives the format, name, the app that wrote it, photos, albums (projects, events and albums), skipped albums (smart albums, books, slideshows) and warnings (such as originals on a volume that isn't connected). A Photos library or an Aperture 1/2 one is `Unsupported`, with a message that says so; anything else that isn't a library is `InvalidArgument`.
- `SessionHandle.import_apple_library(source, categories, dry_run, overwrite_edits, token)` runs lt-lr's engine with `lt_apple::FLAVOR` and returns an `LrImportReportRecord`, exactly as `import_lightroom` does (same category keys; `lens`, `effects` and `masks` never apply).
- `can_revert_apple_import()` and `revert_apple_import(token)` take back the Aperture / iPhoto imports since the last revert, separately from Lightroom's.
- `open_folder_in_place` never puts a session's store inside an Aperture, iPhoto or Photos library package (like a Capture One session folder): it goes to the fallback store.
- After a real import or a revert the Swift store drops its cached records of the session, as for Capture One.

### AI Denoise (`denoise`)

The app runs the model, and Rust does everything around it.

**Setup.**
- `set_denoise_model(model_id)`: the model's identity, the SHA-256 of its weights. `None` turns AI Denoise off.
- `set_denoise_provider(provider)`: the app's AI Denoise, called by export for a photo without a result.
- `set_denoise_cache(dir, max_bytes)`: the disk cache. Results go to `<dir>/<key>.dsdn`, written atomically. The least recently used file is removed first.

**Running a photo.**
- `begin_denoise(path, background, token)` returns a `DenoiseTask` with these methods:
  - `tile_count`, `tile_size`;
  - `read_tile(i, dest_ptr, dest_len)`: planar f16 model input into caller memory;
  - `write_tile(i, src_ptr, src_len, row_stride, channel_stride)`: the model output, read through its strides;
  - `async finish(token)`: stitch and store.
- Tile calls take about 1 ms and may come from any thread. Each tile is written once.

**Result keys.** Results are keyed by BLAKE3 of the decoded sensor data, plus the model id and `lt_pipeline::denoise::VERSION`. A rename, a re-tag or a copy of the file still finds the result. Another model or version misses.

**Rendering.**
- Every develop render passes through `denoise::effective`. With AI Denoise on and a result available, it renders the blend, cached per photo and strength (2 kept), and keeps the original's linear image for re-blends.
- Without a result it renders the original.
- Export calls `denoise::for_export` just before each photo renders. A missing result is made then: Rust calls the app's `DenoiseProvider` (`set_denoise_provider`), a foreign trait, from the export worker. Export then renders exactly the blend it resolved, or fails that photo with the reason. With no model set (a build without the model), it exports the original.
- `async denoise_ready(path, token)` tells the app whether a result exists.

### AI Upscale (`upscale`)

The app runs the model (RealESRGAN_x2plus, `docs/UPSCALE_MODEL.md`), and Rust does everything around it.

- `SessionHandle.free_derived_name(source_path, suffix)`: `<stem>-<suffix>.dng`, or the first free `-N`, free both on disk and in the session (a photo whose file is missing, or a sidecar left behind, keeps its name).
- `begin_upscale(path, name, token)` decodes the RAW (cached), demosaics it and starts its DNG next to it, named `name` (a file taken meanwhile gets the next `-N`). It returns an `UpscaleTask`:
  - `tile_count`, `tile_size` (256, the input), `output_scale` (2);
  - `read_tile(i, dest_ptr, dest_len)`: planar f16 model input into caller memory;
  - `write_tile(i, src_ptr, src_len, row_stride, channel_stride)`: the 512² output, read through its strides. Rows every written tile completed are stitched and streamed into the DNG before it returns;
  - `async finish(token) -> path`: completes the DNG (fsync, no-replace rename) and returns its path. Cancelled before the file is placed, it leaves nothing; once placed, the path is returned (the app then adds the photo, uncancellably). A task dropped unfinished leaves no file.
- `SessionHandle.add_derived(path, source_variant_id)`: adds the file (inside the session's folder) as a new photo starting with the source's adjustments and IPTC metadata, no undo step; returns its variant id.
- Export: `set_upscale_provider(provider)` installs the app's `UpscaleProvider`. A recipe with `ai_upscale` whose size is larger than the render calls `upscale(task, cancel)` from the export worker with the render's tiles (the rows stay in memory), then resizes the 2× image. `Ok(false)` (no model), no provider, or a 2× past 256 MP (a render over 64 MP): resampled. A batch with AI Upscale runs one worker. An error fails that photo; cancelling the export cancels `cancel`.

### Natural-language search (lt-session embeddings)

- **Model.** Apple MobileCLIP-S2 (Core ML, huggingface.co/apple/coreml-mobileclip at `3e0a7bf…`, Apple sample-code licence for the weights) plus OpenAI CLIP's `merges.txt` (MIT): 199 MB, downloaded on first use into `<support>/Models/MobileCLIP-S2/` (`ClipModelStore`: SHA-256 per file, progress, cancel, both packages compiled in a staging folder and moved into place at once). `LT_CLIP_MODEL_DIR` serves a local copy instead of the network.
- **Indexing** (`SearchIndexer`, Swift): `embedding_jobs` lists originals without a vector (one per content; vectors the global index has are reused), each embedded preview is decoded at 384 px and center-cropped to 256 by the image encoder (batches of 16, Neural Engine), `store_embeddings` writes the session and the global index. It pauses while `user_is_active()` (the hashing gate), waits for originals still being hashed (`EmbeddingStatus.waiting`; polled every 3 s, doubling up to 60 s while the count doesn't move), and starts whenever a session opens (after an import too) once the model is installed.
- **Query.** `ClipTokenizer.encode("a photo of …")` → text encoder → `search_embedding` with the browser's rating/label/flag criteria (`SmartFilterRecord`); the app shows only the results, best first (`AppModel.searchRanking`), still under the full filter; the relevance window is applied to what that filter leaves, and a filter change ranks again. Replies to a search that was cleared or superseded, or that belongs to another session, are dropped. During indexing each batch is added to the global index (only that batch's originals are read); the session's full list is re-synced whenever jobs are listed and not written when unchanged. `forget_session_in_search(global_index, root)` (Sessions ▸ Remove from List) drops the session's search items and the vectors no other session uses, so no "offline" group is left; its originals stay for import dedupe. `search_all_sessions` groups by session (`root` is what to open: the in-place folder or the imported session). Tuning on the samples: floor 0.18, window 0.09 (text); Find Similar floor 0.6.
- **Errors.** A global-index failure doesn't fail the session (`StoreReport.global_error`); bad keys and filters are `InvalidArgument`.

### Map (lt-session geotags, global index)
- **Positions.** One order everywhere (map, global map, export): the geotag, then the file's EXIF GPS, then its XMP GPS. `RawMetadata.gps` is the EXIF position (the browser's metadata cache re-reads entries cached before GPS; an older build rewriting the cache costs one re-read per open). `VariantRecord.file_gps` is what the places scan read (EXIF, else XMP). A void fix (`GPSStatus` `V`) or exactly 0, 0 is no position. `VariantRecord.geotag` is the position set on the map: sidecar `geotag` (additive, `null` when removed, a value that can't be read reads as none, is kept as written and never makes the sidecar unreadable; schema stays 1), mirrored in the index (`variants.geo_lat/lon/alt`, index v8) and never written to the original; exports get the photo's position as `ExportSource.gps` (a recipe's `strip_gps` still removes it).
- **Geotagging.** `set_geotag(ids, position)` (`None` removes it) applies to every variant of the photos (a position belongs to the photo) as one undo step; positions out of range are `InvalidArgument`. The app geotags through `save(photos:)` like ratings and keywords.
- **Schema.** Index v8 (after metadata's v7) and global v5 (after metadata's v4) add columns only, on every open when missing (a file stamped by another build heals); the index upgrade re-reads every sidecar once (a sidecar may hold a geotag written while an older build kept the index); a lower version is never wiped.
- **All Sessions.** `sync_find` records each original's position on its find row (`find_items.lat/lon`: the master variant's geotag, else the scanned GPS); `global_map_points(global_index)` lists every session's located originals with their rating and flag (the browser's filter applies). `MapModel` syncs the open session first and shows its own photos from the browser.
- **App.** `MapModel` (non-View): Web Mercator grid clustering off the main actor, only within a screen of the view and at most 500 markers (cells grow until they fit); a click filters the browser to a marker's photos (`PhotoFilter.place`, dropped when the session changes), the view or a drawn rectangle filters to an area (west from its left edge, so wider than half the world works); drops from the filmstrip (`PhotoDragItem`: ids plus session and app instance, refused on mismatch) geotag through a `MapSurface` (MapKit's projection in the app, a flat projection in tests). Tiles load from Apple Maps only after a one-time notice (`AppSettings.mapTiles`; Settings ▸ General); Show All never frames closer than about 20 km. Offline MapKit is expected to show a plain background with the markers; not verified on a disconnected Mac.

### Library (lt-session global index v6)
- **Rows.** `library_items`: one row per variant outside the Trash of every registered session, original present (rel path, variant ordinal and id, capture time, rating, label, flag, camera, lens, keywords, title (else caption), has a position, the sidecar's stamp, the hash of its edit). `library_sources` records each session's store (what `Session::open` takes), its external originals folder and its index stamp (`<name>.ltsession` and its WAL: size and mtime). Sidecars stay the source of truth: rows are rebuilt from each session, and a sync writes only the rows that changed (nothing when none did). An edited variant's sidecar is read for its edit hash only when its stamp moved.
- **Sync.** `SessionHandle.sync_library(global)` for the open session (root = the folder the app opens). `library_refresh(global)` walks every registered session: a store whose index stamp is unchanged is skipped (a stat), a changed one is opened and synced, a session never synced is looked for at its root (a session folder, or `.lt/session` of an in-place folder; fallback stores are only known once the app opened them), an offline one is counted unavailable and keeps its rows. The global index is locked only to read what it knows and to write, never while a session is read.
- **Queries.** `library_query(global, query, first)` returns the kept ids in order and the first `first` ones' details; `library_items(global, ids)` pages in the rest. Criteria (all must hold): sessions, capture time `[from, to)`, min rating or unrated, labels (any), flag filter, cameras (any), keywords (all, any case), text (every word starts a word of the file name, title, keywords, camera, lens or session name; accents and case folded), a collection. Orders: capture time (undated last), rating, file name (numbers as numbers), session. Queries run on an in-memory copy of the rows (names interned, the capture order built at load, the name and session orders and the text index built on first use), reloaded when `library_generation` moves; the Library's own edits patch it in place.
- **Edits.** `library_edit(global, ids, change)` opens each owning session from its store and writes the rating, label or flag to its sidecars (atomic, one undo step in that session; two Sessions on one folder are safe), then patches the rows. A session that can't be written is reported (`failed`), the others are still written. The app routes photos of the open session through `AppModel.updatePhotos` instead (undo, browser), then syncs.
- **Collections.** `library_create_collection(name, rules, ids)` (with `rules` a smart one), `library_update_collection`, `library_delete_collection`, `library_collection_members(id, ids, remove)`. Plain collections list their photos by session, rel path and ordinal (a resync never loses them; the global index is their only copy: a corrupt global index that is replaced loses plain collections, never photos); smart ones store a `LibraryQuery` (JSON) and are rebuilt from the rows.
- **Thumbnails.** `DevelopThumbnails.cached_thumbnail(path, variant, edit_hash)` reads a developed thumbnail from the disk cache without rendering (edits with removals or a depth blur are keyed differently and fall back); otherwise the app shows the embedded preview.
- **Schema.** Global v6 (after the map's v5) adds the Library tables; rows fill on each session's next sync or the next refresh.

### People and Find (lt-session faces, people store, global index)
- **Faces.** `FaceIndexer` (background priority, paused while the user culls, waits for unhashed photos): `scan_locations` in batches, then per `face_jobs` entry the embedded preview at 1616 px → `VisionFaceEmbedder` (Vision face rectangles rev 3 with roll, capture quality, `VNGenerateImageFeaturePrint` rev 2 on a 1.4× square crop rotated upright, 224 px; 768 floats) → `store_faces(key, faces)` (boxes normalized to the upright photo, origin top left). `sync_find` every 64 photos and at the end.
- **People.** `people_overview`: named groups (sidecar tags matched to faces by box overlap; every tagged variant listed, so the browser's `Photo.people` follows without a reload), unnamed faces clustered by average linkage under the user's links (merge = must-link, split / different names / rejections = cannot-link), each cluster with "Is this …?" when its mean similarity to a known person's confirmed faces (store + this session) is ≥ the suggest threshold and nobody rejected that person there. `name_faces` adds the person to `people.json` if new (stable id, rename aliases, ≤ 64 confirmed face vectors) and tags every variant's sidecar (`people`, with the face box); `reject_faces` writes `people_rejected`. Defaults (`PeopleModel.defaultTuning`): cluster 0.70, suggest 0.72, set on an augmentation proxy (no labeled faces on the build Mac).
- **Find.** `find_prompts` → the app embeds the description and each place term with MobileCLIP when installed ("a photo of …") → `find_photos` parses (rules, `lt_ai::find`), resolves people against the store (names + aliases) and places against the library, and returns how it understood the query, sessions (name/folder/keyword matches, or ≥ 3 hits) and photos, each with `MatchReasonRecord`s. Without the model, people, places, dates, ratings and flags still work; the panel says descriptions need the model. `FindModel.run` first syncs the open session (no write when unchanged).
- **Places.** XMP sidecars (`name.xmp`, `name.ext.xmp`) and, for JPEG/HEIF/TIFF/DNG/PNG, the file's own XMP packet: IPTC/Photoshop location fields; GPS from XMP or EXIF. Read once per content and again when a sidecar's size or time changes. GPS positions are not named (no geocoder ships).

### Immich (`immich`, lt-immich)

The archive (`archive`) goes to a folder or, when `ArchiveConfig.object_store` is set, to S3-compatible object storage: requests go through the app's `ArchiveHttp` (`URLSession`; https, or http for a local server; no redirects; every response header returned), and the keys come from the Keychain through `ArchiveManager.set_object_store(http, credentials)` (`ObjectCredentials`, whose `Debug` leaves out the secret). `archive_object_test_connection` (Settings ▸ Archive ▸ Test Connection) does `HeadBucket`, a small `PutObject` under `<prefix>.lt-archive-test/` and a `HeadObject`; refused keys, a missing bucket, a skewed clock or bad settings are `InvalidArgument`, no answer is `Io`. `archive_object_settings_problem` says what is wrong with typed settings. `TargetCheck.kind` is `volume`, `local` or `object`. With object storage, Immich's album sync is skipped (`immich_sync_albums` returns a report with that note) and nothing is queued for it; the Amazon Photos hand-off doesn't depend on the destination.

Immich indexes the archive as a read-only external library; nothing is uploaded. Requests go through the app's `ImmichHttp` (`URLSession`; http and https, no redirects); the API key comes from the Keychain in each `ImmichServer` and is never logged. Developed copies are rendered by the app's export (it has the AI masks): `immich_develop_plan` lists the marked photos whose copy is missing or out of date (a fingerprint of edits, rating, label, pick, keywords, IPTC fields, position and the options), the app exports each into `staging_dir` (JPEG, mozjpeg so Rust writes the XMP) and hands the file to `immich_developed_rendered`, which moves it to `Developed/<original's path>.jpg` in the archived folder. `immich_refresh_sidecars` writes `IMG_0001.ARW.xmp` next to originals (never over another app's sidecar); `ArchiveManager.set_immich` makes every archive run do it first and queues the archived session for `immich_sync_albums`, which keeps the rule albums in step (only album membership and library scans are written to Immich; only assets Luxtable added are ever removed).

### Smart folders (lt-session smart albums)

- **Storage.** Smart folders are smart albums in `<store>/.lt/settings/albums.json`. `Album` has two serde-default fields: `builtin: Option<String>` and `mirror: bool`. Older files load unchanged.
- **Built-ins.** `open_folder_in_place`, `open_session` and `smart_folders()` call `ensure_builtin_albums`, which adds any built-in that is missing. So a built-in deleted from the file comes back on the next open. The built-ins, each with a stable key:
  - `rating:5` … `rating:1`: "5 stars" … "1 star". These are exact matches (`min_rating == max_rating`), never "N and up".
  - `unrated`: `max_rating = 0`.
  - `picked` and `rejected`.
  - `tag:<color>`, one per color tag in use outside the Trash.
- **Protection.** `delete_smart_folder` and `update_smart_folder` refuse built-ins (`SessionError::BuiltinAlbum` → `InvalidArgument`). `update_smart_folder` on a manual album is `InvalidArgument`. A blank name is `InvalidArgument`.
- **Exports** (all `async`, all take a `CancelToken`):
  - `smart_folders() -> [SmartFolderRecord { id, name, builtin, mirror, filter, count }]`: smart albums only, built-ins first in canonical order.
  - `smart_folder_members(id) -> [variant id]`.
  - `query_filter(filter) -> [variant id]`.
  - `create_smart_folder(name, filter) -> id`.
  - `update_smart_folder(id, name, filter)`.
  - `delete_smart_folder(id)`.
  - `set_smart_folder_mirror(id, on)`.
- **`SmartFilterRecord`** mirrors `lt_session::Filter`, and every field is optional or empty:
  - `min_rating`/`max_rating` (0–5, else `InvalidArgument`);
  - `tags` (any of);
  - `pick`;
  - `text`, `camera`, `lens`;
  - `date_from_ms`/`date_to_ms`;
  - `iso_min`/`iso_max`;
  - `edited`;
  - `keywords` (all of; a whole keyword, case-insensitive; blank entries are ignored).
- **Swift split.** Swift evaluates everything except `keywords` and `text` locally, so a count costs O(folders) per rating keypress. It asks `query_filter` only for those two parts, once per session load. Counts are recomputed in full only on load.
- **Mirror to Finder** is Swift-only (`SmartFolderMirror`). It writes symlinks, never copies or hard links, to `<folder>/Smart Folders/<name>/` when the store is on the folder's volume, and otherwise to `<fallback store>/Smart Folders/<name>/`. It deletes only symlinks whose parent is that folder. The folder walk ignores symlinks (`DirEntry::file_type` doesn't follow them), so the mirror never adds photos.

## Deferred contracts (served once lt-pipeline / lt-session / lt-import / lt-archive land)

These are the exact shapes the app is written against. Swift-side protocols already exist in `Services/Protocols.swift`. Records above (`RenderRequest`, `FrameInfo`, `CancelToken`, `ProgressEvent`, `DsError`) are reused unchanged. Every `async` below takes a `CancelToken` and follows the job, progress, and memory rules above.

### Session (`lt-session`) → `SessionStore`

```rust
#[uniffi::export] impl Engine {
    async fn open_session(&self, dir: String, token) -> Result<Arc<Session>, DsError>;          // rebuilds the index if stale
    async fn create_session(&self, parent: String, name: String, token) -> Result<Arc<Session>, DsError>;
    fn recent_sessions(&self) -> Vec<SessionSummary>;                                            // { id, name, path, photo_count, created_unix }
}
#[derive(uniffi::Object)] struct Session;
#[uniffi::export] impl Session {
    fn info(&self) -> SessionSummary;
    fn photos(&self) -> Vec<VariantRecord>;       // { id: u64, path, filename, meta: RawMetadata, rating: u8, color_tag: ColorTag, pick: PickState, edited: bool }
    fn albums(&self) -> Vec<AlbumRecord>;         // { id: String, name, variant_ids: Vec<u64>, smart: Option<FilterRecord> }
    fn set_rating(&self, ids: Vec<u64>, rating: u8) -> Result<(), DsError>;      // batch = one undo step, sidecars written atomically
    fn set_color_tag(&self, ids: Vec<u64>, tag: ColorTag) -> Result<(), DsError>;
    fn set_pick(&self, ids: Vec<u64>, pick: PickState) -> Result<(), DsError>;
    fn load_adjustments(&self, id: u64) -> Result<AdjustmentsRecord, DsError>;   // serde-mirrored lt_core::Adjustments
    fn save_adjustments(&self, edits: Vec<VariantEdit>, coalesce_key: Option<String>) -> Result<(), DsError>; // one undo step
    fn paste(&self, source: u64, targets: Vec<u64>, options: PasteOptionsRecord) -> Result<u32, DsError>;     // lt_core::Clipboard::apply, one undo step
    fn undo(&self) -> Result<Option<String>, DsError>;   // label of the undone step
    fn redo(&self) -> Result<Option<String>, DsError>;
    fn presets(&self) -> Vec<PresetRecord>;              // { id, name, groups: Vec<AdjustGroup>, adjustments: AdjustmentsRecord, auto_apply: bool }
    fn save_preset(&self, preset: PresetRecord) -> Result<(), DsError>;
    fn delete_preset(&self, id: String) -> Result<(), DsError>;
}
```
`ColorTag`/`PickState` mirror `lt_core::meta`. Swift `PasteOptions` maps 1:1 to `PasteOptionsRecord { groups, relative, layer_ids, replace_layers }`.

### Rendering (`lt-pipeline`) → `Renderer`, `ImageProvider`

```rust
#[uniffi::export] impl Session {
    async fn render_into(&self, id: u64, adjustments: Option<AdjustmentsRecord>, request: RenderRequest,
                         dest_ptr: u64, dest_len: u64, token: Arc<CancelToken>) -> Result<FrameInfo, DsError>;
    async fn thumbnail(&self, id: u64, max_edge: u32, token) -> Result<ThumbnailRecord, DsError>;   // { jpeg: Vec<u8>, width, height } from lt-cache
    async fn histogram(&self, id: u64, token) -> Result<HistogramRecord, DsError>;                // 256 bins R,G,B,L + clip counts (display-referred)
    async fn auto_adjust(&self, id: u64, token) -> Result<AdjustmentsRecord, DsError>;            // auto WB + auto levels (+ straighten when asked)
}
```
- `render_into` has exactly the layout and memory contract of `render_test_pattern_into`. `adjustments: None` uses the saved edit; `Some` renders a preview edit (slider drags, preset hover).
- `plan_frame` stays the sizing call. Its source size is the oriented sensor size from `RawMetadata`.
- 16F frames are what the viewer's EDR path consumes. The histogram is computed in Rust on the display transform.

### Import (`lt-import`) → `ImportService`

```rust
#[uniffi::export] impl Engine {
    async fn scan_volume(&self, root: String, token) -> Result<Vec<CandidateRecord>, DsError>;  // { path, size, capture_unix, already_imported: bool, sidecar_paths }
    fn plan_import(&self, candidates: Vec<CandidateRecord>, dest: ImportDestination, rename_template: String,
                   backup_dir: Option<String>, skip_duplicates: bool) -> Result<ImportPlanRecord, DsError>;
    async fn run_import(&self, plan: ImportPlanRecord, token) -> Result<ImportReport, DsError>;   // progress: kind "import", completed/total in bytes
    fn pending_import_journals(&self) -> Vec<ImportPlanRecord>;                                  // resume after crash / pulled card
}
struct ImportReport { copied: u32, verified: u32, skipped_duplicates: u32, failed: Vec<ImportFailure>, session_path: String, all_verified: bool }
```
**Verified-only success.** The UI may say "safe to eject" or "verified" only when `all_verified == true` and `verified == copied`. Any other outcome, including cancellation, an error, or a partial result, shows what failed and never offers eject as "safe". Nothing is ever deleted from the card.

### Archive (`lt-archive`) → `ArchiveService`

```rust
#[uniffi::export] impl Engine {
    fn archive_status(&self) -> ArchiveStatusRecord;     // { state: Idle|Syncing{fraction}|Behind{files}|Offline|Failed{message}, target, last_success_unix }
    async fn archive_sync_now(&self, token) -> Result<ArchiveStatusRecord, DsError>;   // progress: kind "archive"
    fn set_archive_target(&self, path: String) -> Result<(), DsError>;
}
```

### AI (`lt-ai`)

```rust
fn cull_scores(signals: Vec<VisionSignalsRecord>) -> Vec<CullScoreRecord>;   // Vision runs in Swift; Rust merges signals + sharpness tiles
async fn similarity_groups(&self, ids: Vec<u64>, token) -> Result<Vec<Vec<u64>>, DsError>;
fn edit_intent_to_adjustments(intent: EditIntentRecord, current: AdjustmentsRecord) -> AdjustmentsRecord;
```

## Rules for new exports

- Pixel buffers go into caller memory via `plan_frame` + `*_into`, never as a returned `Vec<u8>`. Small encoded blobs (JPEG thumbnails) may cross as `Vec<u8>`.
- Every fallible call returns `Result<_, DsError>`. Add a variant only when the UI would act differently on it.
- Anything that can take more than about 1 ms is `async`, takes a `CancelToken`, and runs through `run_job`.
- Swift never loops over pixels it received from Rust, except the display-sized 8-bit histogram until `lt-pipeline` provides one.

## Testing

- Rust: `cargo test -p lt-ffi` (coverage ≥ 90%, excluding `src/bin/` and `benches/`).
  - Tests cover cancellation, drop-waits-for-worker, panics, progress backpressure and order, tiles matching full renders, the f16 conversion, and RAW mapping.
  - Real-sample tests run when `samples/` exists; set `LT_SAMPLES_DIR` from a worktree.
  - Benchmarks: `cargo bench -p lt-ffi --bench render`.
- Swift: `RustTests.swift` exercises the linked library.
  - Buffer-backed and heap frames, tiles, 16F, task cancellation (latency and bands written), `ProgressStream`, error mapping, and RAW metadata and previews (on real samples when present).
  - The EXIF orientation table is checked against the spec.
- `Luxtable --self-test` runs 8 checks end to end, including Metal and cancellation.
