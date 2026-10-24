# Releasing

## Version and tag

`scripts/release.sh [patch|minor|major|beta]` bumps `version` in `Cargo.toml`, commits, tags `vX.Y.Z` and pushes `main` and the tag atomically. Enable the pre-push hook that enforces it with `git config core.hooksPath scripts/hooks`.

If the push fails, the commit and tag stay local. Run `scripts/release.sh` again: it sees the unpushed tag and pushes that release without bumping. To abandon it: `git tag -d vX.Y.Z && git reset --hard HEAD~1`.

Betas: `scripts/release.sh beta` makes `X.Y.(Z+1)-beta.1` from a release and `-beta.(N+1)` from a beta; `scripts/release.sh patch` after a beta releases that version (`0.1.19-beta.2` → `0.1.19`). The version goes into `CFBundleShortVersionString` as is (`0.1.19-beta.1`), which is what the in-app updater compares. No other pre-release form (`-rc1`) is supported.

## Publishing on GitHub (what the in-app updater reads)

Darkslide checks `GET https://api.github.com/repos/sbaillar/darkslide/releases` (automatically about once a day, and from Darkslide ▸ Check for Updates… or Settings ▸ Updates). A release is an update only if all of this holds; anything else (drafts, the RAW helper's `rawler-helper-N` releases) is ignored:

- The tag is `v<version>`, exactly the version in `Cargo.toml` (`v0.1.19`, `v0.1.19-beta.1`).
- It has the asset `Darkslide-<version>-macos-arm64.zip`, made with `ditto -c -k --keepParent` from the app `scripts/build-app.sh` built at that tag. GitHub's asset `digest` (SHA-256) must be present: the app checks the download against it and the size, then checks the unzipped app's bundle identifier, that its `CFBundleShortVersionString` is the tag's version, and `codesign --verify --deep --strict`. Any failure installs nothing.
- The body starts with a `## What's new` section of bullets (then `## Install`). Those bullets are the short release notes the app shows before downloading and once after updating, as plain text (bold, code and links are flattened). Without that heading it shows the body's first ten lines.

```sh
scripts/release.sh                      # or: scripts/release.sh beta
V=0.1.19                                # the version it printed
scripts/build-app.sh
ditto -c -k --keepParent dist/Darkslide.app "Darkslide-$V-macos-arm64.zip"
gh release create "v$V" "Darkslide-$V-macos-arm64.zip" --title "Darkslide $V" --notes-file notes.md
```

**A beta** is the same with a `-beta.N` version and tag, published with `gh release create … --prerelease`. Only users who turned on Settings ▸ Updates ▸ Include beta versions are offered it. Versions compare by semver, so `0.1.19-beta.1` < `0.1.19-beta.2` < `0.1.19`: beta testers move on to the release once it is out, and users on the stable channel never see a beta. Never replace a published release's zip: publish a new version instead.

The update installs when the app quits, or at once with Download & Restart. A small relauncher waits for the app to exit, moves the old bundle to `~/Library/Application Support/Darkslide/Previous Version/` (one kept, replaced by the next update), moves the new one into the old one's place and, after Download & Restart, reopens it. Either way the next launch opens the session and photo the user was on. If that move fails, the old bundle is put back. When the app's folder isn't writable, the new app is shown in Finder to install by hand.

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
