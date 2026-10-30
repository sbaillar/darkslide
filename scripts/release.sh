#!/usr/bin/env bash
# Bump the app version, commit, tag and push main, then build the app and publish it as a
# GitHub release (what the in-app updater reads; docs/RELEASING.md). Every push to main goes
# through this (scripts/hooks/pre-push refuses a push to main that does not raise the version).
#
#   scripts/release.sh notes.md          0.1.0 → 0.1.1
#   scripts/release.sh minor notes.md    0.1.3 → 0.2.0
#   scripts/release.sh major notes.md    0.4.2 → 1.0.0
#   scripts/release.sh beta notes.md     0.1.18 → 0.1.19-beta.1, 0.1.19-beta.1 → 0.1.19-beta.2
#   (patch after a beta releases it: 0.1.19-beta.2 → 0.1.19)
#
# notes.md is the release's body: a "## What's new" section of bullets (shown in the app),
# then "## Install". Publishing needs `gh` signed in to GitHub.
#
# Re-runnable: if the push fails (network, hook, rejected), the release commit and tag stay
# local; run the script again and it pushes that same release instead of bumping again. If
# publishing fails after the push, running it again publishes that release.
set -euo pipefail
cd "$(dirname "$0")/.."

part="patch"
notes=""
for a in "$@"; do
  case "$a" in
    patch | minor | major | beta) part="$a" ;;
    *) notes="$a" ;;
  esac
done
[ -n "$notes" ] || { echo "release: pass the release notes file (docs/RELEASING.md)" >&2; exit 1; }
[ -f "$notes" ] || { echo "release: no notes file '$notes'" >&2; exit 1; }
grep -q "^## What's new" "$notes" || { echo "release: $notes has no '## What's new' section" >&2; exit 1; }
notes="$(cd "$(dirname "$notes")" && pwd)/$(basename "$notes")"
{ command -v gh >/dev/null && gh auth status >/dev/null 2>&1; } || { echo "release: gh is not signed in (gh auth login)" >&2; exit 1; }
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || { echo "release: run on main" >&2; exit 1; }
git diff --quiet && git diff --cached --quiet || { echo "release: commit or stash changes first" >&2; exit 1; }

old="$(/usr/bin/sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[[ "$old" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-beta\.[0-9]+)?$ ]] \
  || { echo "release: version '$old' is not MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-beta.N" >&2; exit 1; }

push() {
  git push --atomic origin main "v$1" || {
    echo "release: push failed. Nothing is lost: fix the cause and run scripts/release.sh again (it pushes v$1 without bumping)." >&2
    echo "         To abandon the release instead: git tag -d v$1 && git reset --hard HEAD~1" >&2
    exit 1
  }
  echo "released v$1"
}

# Build the app at the tag, check it is that version and validly signed, and publish it with
# the notes (a beta as a pre-release). The zip is what the in-app updater downloads.
publish() {
  local zip="dist/Luxtable-$1-macos-arm64.zip" pre=()
  [[ "$1" == *-beta.* ]] && pre=(--prerelease)
  scripts/build-app.sh
  local built
  built="$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" dist/Luxtable.app/Contents/Info.plist)"
  [ "$built" = "$1" ] || { echo "release: built app is $built, not $1" >&2; exit 1; }
  codesign --verify --deep --strict dist/Luxtable.app
  rm -f "$zip"
  ditto -c -k --keepParent dist/Luxtable.app "$zip"
  gh release create "v$1" "$zip" --verify-tag --title "Luxtable $1" --notes-file "$notes" ${pre[@]+"${pre[@]}"} || {
    echo "release: publishing failed; v$1 is pushed. Run scripts/release.sh again to publish it." >&2
    exit 1
  }
  echo "published v$1"
}

# An earlier run committed and tagged v$old but never pushed it: finish that one.
# ls-remote --exit-code returns 2 only for "no such ref"; anything else (offline) is an error.
remote_tag=0
if git rev-parse -q --verify "refs/tags/v$old" >/dev/null; then
  git ls-remote --exit-code --tags origin "refs/tags/v$old" >/dev/null || remote_tag=$?
  [ "$remote_tag" = 0 ] || [ "$remote_tag" = 2 ] || { echo "release: cannot reach origin" >&2; exit 1; }
fi
if [ "$remote_tag" = 2 ]; then
  if [ "$(git rev-parse "v$old^{commit}")" = "$(git rev-parse HEAD)" ]; then
    echo "release: v$old is committed and tagged locally but not pushed; pushing it"
    push "$old"
    publish "$old"
    exit 0
  fi
  echo "release: local tag v$old is not at HEAD and not on origin; delete or move it by hand" >&2
  exit 1
fi
# An earlier run pushed v$old but didn't publish it: publish that one.
if [ "$remote_tag" = 0 ] && git rev-parse -q --verify "refs/tags/v$old" >/dev/null \
  && [ "$(git rev-parse "v$old^{commit}")" = "$(git rev-parse HEAD)" ] && ! gh release view "v$old" >/dev/null 2>&1; then
  echo "release: v$old is pushed but not published; publishing it"
  publish "$old"
  exit 0
fi

git fetch -q origin main
git merge-base --is-ancestor origin/main HEAD || { echo "release: main is behind or diverged from origin/main; merge it first" >&2; exit 1; }

base="${old%%-*}"
beta=""
[ "$base" = "$old" ] || beta="${old##*-beta.}"
IFS=. read -r major minor patch <<<"$base"
suffix=""
case "$part" in
  patch) [ -n "$beta" ] || patch=$((patch + 1)) ;; # a beta's patch is its release
  minor) minor=$((minor + 1)); patch=0 ;;
  major) major=$((major + 1)); minor=0; patch=0 ;;
  beta) if [ -n "$beta" ]; then suffix="-beta.$((beta + 1))"; else patch=$((patch + 1)); suffix="-beta.1"; fi ;;
  *) echo "release: unknown part '$part' (patch|minor|major|beta)" >&2; exit 1 ;;
esac
new="$major.$minor.$patch$suffix"

/usr/bin/sed -i '' "s/^version = \"$old\"/version = \"$new\"/" Cargo.toml
cargo update --workspace --offline --quiet
git commit -q -am "Release v$new"
git tag -a "v$new" -m "Luxtable v$new"
push "$new"
publish "$new"
