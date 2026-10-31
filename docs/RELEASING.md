# Releasing

## Version and tag

`scripts/release.sh [patch|minor|major|beta] notes.md` bumps `version` in `Cargo.toml`, commits, tags `vX.Y.Z` and pushes `main` and the tag atomically. Then it builds the app and publishes it on GitHub with `notes.md` as the release's body (see below). It checks the notes file and that `gh` is signed in before it changes anything. Enable the pre-push hook that enforces the version bump with `git config core.hooksPath scripts/hooks`.

If the push fails, the commit and tag stay local. Run `scripts/release.sh` again: it sees the unpushed tag and pushes that release without bumping. To abandon it: `git tag -d vX.Y.Z && git reset --hard HEAD~1`. If publishing fails after the push, run it again too: it sees the pushed tag at `HEAD` with no GitHub release and publishes that one.

Betas: `scripts/release.sh beta` makes `X.Y.(Z+1)-beta.1` from a release and `-beta.(N+1)` from a beta; `scripts/release.sh patch` after a beta releases that version (`0.1.19-beta.2` → `0.1.19`). The version goes into `CFBundleShortVersionString` as is (`0.1.19-beta.1`), which is what the in-app updater compares. No other pre-release form (`-rc1`) is supported.

## Publishing on GitHub (what the in-app updater reads)

Luxtable checks `GET https://api.github.com/repos/sbaillar/luxtable/releases` (automatically about once a day, and from Luxtable ▸ Check for Updates… or Settings ▸ Updates). A release is an update only if all of this holds; anything else (drafts, the RAW helper's `rawler-helper-N` releases) is ignored:

- The tag is `v<version>`, exactly the version in `Cargo.toml` (`v0.1.19`, `v0.1.19-beta.1`).
- It has the asset `Luxtable-<version>-macos-arm64.zip`, made with `ditto -c -k --keepParent` from the app `scripts/build-app.sh` built at that tag. GitHub's asset `digest` (SHA-256) must be present: the app checks the download against it and the size, then checks the unzipped app's bundle identifier, that its `CFBundleShortVersionString` is the tag's version, and `codesign --verify --deep --strict`, signed the way the running app is: the same Developer ID team, or the same certificate (a self-signed `Luxtable Dev`); an ad-hoc signed app has no identity to compare, so any valid signature passes. Its download URL must be https on github.com. Any failure installs nothing. A release signed with a new identity can't update the old one: users install it by hand once.
- The body starts with a `## What's new` section of bullets (then `## Install`). Those bullets are the short release notes the app shows before downloading and once after updating, as plain text (bold, code and links are flattened). Without that heading it shows the body's first ten lines.

`scripts/release.sh` does all of this. It builds with `scripts/build-app.sh` at the tag, checks that the app's version is the tag's and that `codesign --verify --deep --strict` passes, zips it into `dist/`, and runs `gh release create --verify-tag` (`--prerelease` for a beta). By hand, it is:

```sh
V=0.1.19                                # the version it tagged
scripts/build-app.sh
ditto -c -k --keepParent dist/Luxtable.app "dist/Luxtable-$V-macos-arm64.zip"
gh release create "v$V" "dist/Luxtable-$V-macos-arm64.zip" --title "Luxtable $V" --notes-file notes.md
```

**A beta** is the same with a `-beta.N` version and tag, published with `gh release create … --prerelease`. Only users who turned on Settings ▸ Updates ▸ Include beta versions are offered it. Versions compare by semver, so `0.1.19-beta.1` < `0.1.19-beta.2` < `0.1.19`: beta testers move on to the release once it is out, and users on the stable channel never see a beta. Never replace a published release's zip: publish a new version instead.

The update installs when the app quits, or at once with Download & Restart. A small relauncher waits for the app to exit, moves the new bundle next to the old one (a copy when it is on another volume), swaps the two with renames in that folder, then moves the old bundle to `~/Library/Application Support/Luxtable/Previous Version/` (one kept, replaced by the next update; if it can't go there it stays next to the new one, hidden) and, after Download & Restart, reopens it. Either way the next launch opens the session and photo the user was on. If the swap fails, the old bundle is put back. When the app's folder isn't writable, the new app is shown in Finder to install by hand.

**The rename.** 0.2.0 is the first release as Luxtable (bundle identifier `app.luxtable.Luxtable`, assets `Luxtable-<version>-macos-arm64.zip`, repository `sbaillar/luxtable`). Copies of Darkslide 0.1 don't update to it (another bundle identifier, another asset name): users install Luxtable by hand once, and its first launch copies Darkslide's preferences, support folder and keys (docs/user-guide/14-migrate.md). The `Luxtable Dev` certificate replaces `Darkslide Dev`; nothing signed with the old one is published.

## The RAW helper

`luxtable-rawler` is published as its own release, `rawler-helper-N`, with one asset, `luxtable-rawler-macos-arm64`, built by `scripts/build-rawler-helper.sh` (which prints its SHA-256 and size). `RawlerHelper.release` in the app pins the URL, the hash and the size, so a new helper is a new tag and a new pin, never a replaced asset. The current one is `rawler-helper-2`, the first under the new name (`rawler-helper-1`, the old name, isn't used any more). Upload exactly the file the script built and pinned:

```sh
scripts/build-rawler-helper.sh          # dist/rawler-helper/luxtable-rawler-macos-arm64, its sha256 and size
gh release create rawler-helper-2 dist/rawler-helper/luxtable-rawler-macos-arm64 dist/rawler-helper/README.md \
  --title "RAW helper 2" --notes "luxtable-rawler: the optional RAW helper (LGPL-2.1, crates/lt-rawler/README.md)."
```

`CFBundleVersion` is `git rev-list --count HEAD`, so build distributable apps from a full clone (`fetch-depth: 0` in CI), and note that a fresh-history repository restarts the count.

## Builds and signing

| Mode | How | Result |
|---|---|---|
| Development | `scripts/build-app.sh` | Signed with the local `Luxtable Dev` identity when it is valid (`security find-identity -v`), else ad hoc. |
| Distribution (opt-in) | `LT_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" scripts/build-app.sh` | Hardened Runtime, secure timestamp, `app/Resources/Luxtable.entitlements`. |
| Notarized (opt-in) | add `LT_NOTARY_PROFILE=<profile>` | Submits with `notarytool --wait`, staples the ticket, runs `spctl --assess`. |

Create the notary profile once, interactively, with `xcrun notarytool store-credentials <profile>`; the credentials live in your keychain, never in the repository or the environment.

**Not tested end to end.** The distribution path needs a paid Apple Developer account, which the automated gates don't have. Everything up to the `codesign` call is exercised by the normal build; the Hardened Runtime launch, the voice feature (microphone entitlement) and notarization still need a first run by the account owner. The app is deliberately not sandboxed (it runs rsync, hdiutil and diskutil and reads folders the user picks); the entitlements file explains each entry.

## What every build checks

`scripts/ffi.sh` maps the home directory, the cargo registry and the repository root to neutral prefixes (`--remap-path-prefix`), `build-app.sh` strips debug info from the bundled binary, and the build fails if `strings` still finds the builder's `$HOME` in it.

## CI

`.github/workflows/ci.yml` pins actions by commit SHA and the Rust toolchain by version (`RUST_TOOLCHAIN`), builds with `--locked`, and runs `cargo deny check advisories` weekly as well as on push. Raise the toolchain and the action SHAs deliberately, together with `rust-version` in `Cargo.toml`.
