# Releasing

## Version and tag

`scripts/release.sh [patch|minor|major]` bumps `version` in `Cargo.toml`, commits, tags `vX.Y.Z` and pushes `main` and the tag atomically. Enable the pre-push hook that enforces it with `git config core.hooksPath scripts/hooks`.

If the push fails, the commit and tag stay local. Run `scripts/release.sh` again: it sees the unpushed tag and pushes that release without bumping. To abandon it: `git tag -d vX.Y.Z && git reset --hard HEAD~1`. Pre-release versions (`0.2.0-rc1`) aren't supported.

`CFBundleVersion` is `git rev-list --count HEAD`, so build distributable apps from a full clone (`fetch-depth: 0` in CI), and note that a fresh-history repository restarts the count.

## Builds and signing

| Mode | How | Result |
|---|---|---|
| Development | `scripts/build-app.sh` | Signed with the local `Darkslide Dev` identity when it is valid (`security find-identity -v`), else ad hoc. |
| Distribution (opt-in) | `DS_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" scripts/build-app.sh` | Hardened Runtime, secure timestamp, `app/Resources/Darkslide.entitlements`. |
| Notarized (opt-in) | add `DS_NOTARY_PROFILE=<profile>` | Submits with `notarytool --wait`, staples the ticket, runs `spctl --assess`. |

Create the notary profile once, interactively, with `xcrun notarytool store-credentials <profile>`; the credentials live in your keychain, never in the repository or the environment.

**Not tested end to end.** The distribution path needs a paid Apple Developer account, which the automated gates don't have. Everything up to the `codesign` call is exercised by the normal build; the Hardened Runtime launch, the voice feature (microphone entitlement) and notarization still need a first run by the account owner. The app is deliberately not sandboxed (it runs rsync, hdiutil and diskutil and reads folders the user picks); the entitlements file explains each entry.

## What every build checks

`scripts/ffi.sh` maps the home directory, the cargo registry and the repository root to neutral prefixes (`--remap-path-prefix`), `build-app.sh` strips debug info from the bundled binary, and the build fails if `strings` still finds the builder's `$HOME` in it.

## CI

`.github/workflows/ci.yml` pins actions by commit SHA and the Rust toolchain by version (`RUST_TOOLCHAIN`), builds with `--locked`, and runs `cargo deny check advisories` weekly as well as on push. Raise the toolchain and the action SHAs deliberately, together with `rust-version` in `Cargo.toml`.
