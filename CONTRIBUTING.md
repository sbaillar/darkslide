# Contributing to Darkslide

Thanks for helping. Darkslide is a macOS RAW photo app with a Rust core (`crates/`) and a SwiftUI app (`app/`). Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the crate contracts and [CLAUDE.md](CLAUDE.md) for the engineering rules (performance budgets, coverage, no `unsafe` outside `ds-ffi` and GPU buffer mapping, atomic writes, never losing a user's file).

## Toolchain

- macOS 26 on Apple silicon, with the Xcode Command Line Tools (Swift 6.2). Xcode.app isn't needed.
- Rust 1.90 or newer (rustup or Homebrew).
- For the coverage gates: `cargo-llvm-cov`. With Homebrew Rust, which has no `llvm-tools` component, install `llvm` and set `LLVM_BIN` (default `/opt/homebrew/opt/llvm@22/bin`) for `scripts/coverage.sh`.
- For the licence checks: `cargo install --locked cargo-about@0.9.2 --features cli` and `cargo install --locked cargo-deny@0.20.2` (pinned: `scripts/licenses.sh --check` compares the generated file byte for byte).
- Python 3.13 only if you build the AI models yourself (`scripts/denoise-model/`, `scripts/upscale-model/`).

## Build without secrets, models or sample photos

Nothing in the build needs a secret, a signing certificate or the maintainers' photos:

```sh
DS_SKIP_MODELS=1 scripts/build-app.sh --test   # build dist/Darkslide.app and run its self-test
DS_SKIP_MODELS=1 scripts/build-app.sh --run    # build and launch
dist/Darkslide.app/Contents/MacOS/Darkslide --demo   # synthetic photos; touches none of your files
```

- `DS_SKIP_MODELS=1` skips converting the AI Denoise model (AI Denoise then shows as unavailable). The AI Upscale model is never bundled unless `DS_BUNDLE_UPSCALE=1`; the app offers to download it.
- Without a "Darkslide Dev" signing identity the app is signed ad hoc. `scripts/make-signing-identity.sh` creates one so macOS keeps the app's permissions across rebuilds. Signing a distributable build (Developer ID, notarization) is opt-in and described in [docs/RELEASING.md](docs/RELEASING.md).
- Tests that need real RAW files read them from the git-ignored `samples/` folder (or `$DS_SAMPLES_DIR`) and skip when it's absent. Synthetic tests cover the same logic, so every gate passes without samples.
- Point dev and test runs at a scratch support folder with `DS_SUPPORT_ROOT=$(mktemp -d)`, so they never touch your real Darkslide data.

## Gates

Every change must pass:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                                    # or scripts/coverage.sh: line coverage ≥ 90 %
DS_SUPPORT_ROOT=$(mktemp -d) scripts/test-app.sh          # Swift tests; non-View coverage ≥ 90 %
DS_SKIP_MODELS=1 scripts/build-app.sh --test              # app self-test
scripts/licenses.sh --check                               # dependency licences; THIRD_PARTY_LICENSES.md current
```

- If you change the Rust↔Swift interface, run `scripts/ffi.sh` and see [docs/FFI.md](docs/FFI.md).
- New Rust code needs tests that assert behaviour. Code that can't run in a test (no GPU, FFI glue) goes behind a small seam with a test double. Swift logic belongs outside View files and needs tests too.
- The 90 % Rust coverage gate counts GPU and real-sample tests, so it's reached on a Mac with sample RAWs. CI reports it without enforcing it.
- A new dependency must use a licence `deny.toml` allows; run `scripts/licenses.sh` and commit the regenerated `THIRD_PARTY_LICENSES.md`. A new bundled or downloaded model needs a licence compatible with redistribution and an entry in `app/Resources/Acknowledgements.md`.
- No personal paths, host names, people's names, real photos or GPS positions in code, fixtures, docs or commit messages. No secrets or API client IDs: read them at build time from git-ignored files or environment variables, and let the feature turn off without them.

## Sign your commits (DCO)

Contributions are accepted under the project's licence, Apache-2.0 ([LICENSE](LICENSE)), with a [Developer Certificate of Origin](https://developercertificate.org) sign-off instead of a CLA. Add a `Signed-off-by` line to every commit:

```sh
git commit -s -m "Fix the histogram's clipping counts"
```

It certifies that you wrote the change, or otherwise have the right to submit it under the project's licence. Use your real name and an email address you can be reached at.

## Relinking rawler

Darkslide statically links [rawler](https://github.com/dnglab/dnglab), which is licensed under the LGPL-2.1. To build the app with a modified rawler, add to the workspace `Cargo.toml`:

```toml
[patch.crates-io]
rawler = { path = "../dnglab/rawler" }
```

then run `scripts/build-app.sh`. The whole app is rebuilt and relinked against your rawler.

## Releases (maintainers)

Enable the repository's git hooks once with `git config core.hooksPath scripts/hooks`. Every push to `main` raises the version: maintainers merge locally and push with `scripts/release.sh`, and `scripts/hooks/pre-push` refuses a push to `main` that keeps the same version. Contributors' branches and pull requests are unaffected.
