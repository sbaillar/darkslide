# Darkslide: engineering rules

A macOS RAW photo workflow app (Capture One–style sessions, fast culling, layers, copy/apply) with a Rust core and a SwiftUI shell. See `PLAN.md` for the product and `docs/ARCHITECTURE.md` for crate contracts.

## Non-negotiables
- **Performance first for RAW.** Every hot path has a criterion bench and a budget (see ARCHITECTURE.md §Budgets). Never allocate per-pixel, never block the UI thread, prefer SIMD-friendly loops over `f32` slices, parallelize with rayon by rows/tiles. Measure before and after; report numbers in the PR/commit message.
- **Coverage ≥ 90% lines** across the Rust workspace: `scripts/coverage.sh` must pass. Tests must assert behavior, not just execute code. Target the logic, then add error-path tests. Code that genuinely cannot run in a test (GPU device absent, FFI glue) goes behind a small seam with a test double. Do not game coverage with `#[coverage(off)]` or trivial tests.
- **Zero warnings:** `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check`.
- **No `unsafe`** outside `ds-ffi` and `ds-pipeline::gpu` buffer mapping (each block needs a `// SAFETY:` comment).
- **Never lose a user's file.** Writes are atomic (write temp → fsync → rename). Imports verify BLAKE3 before reporting success. Nothing ever deletes from a memory card or from the archive.
- **Sidecars are the source of truth**; SQLite is a rebuildable index.

## Layout
```
crates/ds-core      plain data types (adjustments, clipboard, raw container). No I/O.
crates/ds-raw       RAW decode (rawler), embedded previews, EXIF
crates/ds-pipeline  develop pipeline: CPU reference + wgpu/Metal GPU renderer, auto tools
crates/ds-merge     HDR merge and panorama stitching (registration, linear merge, projection, blending)
crates/ds-session   sessions, variants, sidecars, SQLite index, presets, global index
crates/ds-c1        Capture One session import (read-only parsers, mapping, idempotent import)
crates/ds-lr        Lightroom Classic / Camera Raw import (catalog, XMP, presets; mapping, idempotent import)
crates/ds-apple     Aperture 3 / iPhoto library import (Library.apdb, AlbumData.xml; mapping; ds-lr's import engine)
crates/ds-cache     thumbnail/preview cache + prioritized job scheduler
crates/ds-import    card detection, verified copy, dedupe, journal/resume
crates/ds-archive   rsync archive scheduler, Amazon Photos folder hand-off
crates/ds-export    export recipes and encoders
crates/ds-gphotos   Google Photos export destination (OAuth/PKCE, uploads, albums) behind an HTTP seam
crates/ds-immich    Immich: album sync behind an HTTP seam (fake server), path mapping, developed copies + XMP sidecar files
crates/ds-remote    Mac ↔ iPad link: paired mutual-TLS server, client library, `ds-remote` CLI
crates/ds-ai        culling scores, similarity grouping, NL edit parsing seam
crates/ds-plugin    plug-in host: manifests, registry, install/trust, wasmi runtime, ds-plugin CLI (docs/PLUGINS.md)
crates/ds-ffi       UniFFI facade consumed by the Swift app
app/                SwiftPM macOS app (SwiftUI + AppKit + Metal + Vision)
samples/            real Sony A9 (ILCE-9) ARW files, git-ignored; tests that need them skip if absent
```

## Conventions
- Rust 2024, workspace lints (clippy pedantic warn). Errors: `thiserror` in libraries, `anyhow` only in binaries/tests.
- Put crate-specific dependencies in the crate's own `Cargo.toml` (versions pinned there). Only add to root `[workspace.dependencies]` when two or more crates share a dep.
- Tests needing sample RAWs: use `ds_testdata::sample_arw()`-style helpers that return `None` when `samples/` is missing, so CI without samples still passes, **but** synthetic tests must cover the same logic for coverage.
- Tooling on this Mac: Homebrew rust (no rustup), `cargo llvm-cov` with Homebrew `llvm@23` (the script sets `LLVM_COV`). Shell aliases: `sed`→`sd`, `ls`→eza, `du`→dust. Use `/usr/bin/sed`, `/bin/ls`, `/usr/bin/du` in scripts.
- Git identity is set per-repo. Work on feature branches; commits end with the Co-Authored-By trailer.
- **Every push to main raises the version.** Merge locally, then push with `scripts/release.sh` (patch by default; `minor`/`major` as args): it bumps `[workspace.package] version`, commits, tags `vX.Y.Z` and pushes. `scripts/hooks/pre-push` (enabled via `git config core.hooksPath scripts/hooks`) refuses a push to main that keeps the same version. Feature-branch pushes are unaffected.
- **Darkslide will be open-sourced.** Treat every commit as public: no personal paths, hostnames, names, real photos or GPS in code, fixtures, docs or commit messages; no secrets or API client IDs in the repo (read them at build time from git-ignored files or env vars, with a documented way for contributors to supply their own); new dependencies and bundled models need licences compatible with redistribution (check before adding). See `docs/OPEN_SOURCE.md` once it lands.
