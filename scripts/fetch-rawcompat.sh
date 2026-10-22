#!/usr/bin/env bash
# Download the RAW compatibility samples (crates/ds-raw/tests/compat/samples.json:
# CC0 files from raw.pixls.us, one or more per major format) and verify each
# against its pinned SHA-256. The files are never committed.
#
#   scripts/fetch-rawcompat.sh [<dest>]
#
# Default dest: ~/Library/Caches/darkslide-rawcompat. Files already there with
# the right hash are kept; a download that fails verification is deleted and
# the script stops. Prints the folder on success. Then run the opt-in test:
#
#   DS_RAWCOMPAT_DIR=<dest> cargo test -p ds-raw --test compat -- --nocapture
set -euo pipefail
cd "$(dirname "$0")/.."

MANIFEST=crates/ds-raw/tests/compat/samples.json
DEST="${1:-$HOME/Library/Caches/darkslide-rawcompat}"
mkdir -p "$DEST"
field() { /usr/bin/plutil -extract "$1" raw -o - "$MANIFEST"; }
die() { echo "fetch-rawcompat: $*" >&2; exit 1; }

count="$(field files)"
for i in $(seq 0 $((count - 1))); do
  name="$(field "files.$i.name")"
  url="$(field "files.$i.url")"
  sha="$(field "files.$i.sha256")"
  lic="$(field "files.$i.licence")"
  [[ "$name" != */* && "$name" != .* ]] || die "bad file name in manifest: $name"
  [[ "$lic" == CC0-1.0 ]] || die "$name: licence $lic is not CC0-1.0"
  if [[ -f "$DEST/$name" ]] && [[ "$(shasum -a 256 "$DEST/$name" | awk '{print $1}')" == "$sha" ]]; then
    continue
  fi
  echo "fetch $name" >&2
  curl -fL --retry 3 --silent --show-error "$url" -o "$DEST/$name.part"
  if [[ "$(shasum -a 256 "$DEST/$name.part" | awk '{print $1}')" != "$sha" ]]; then
    /bin/rm -f "$DEST/$name.part"
    die "$name: hash mismatch (the download is not the pinned file)"
  fi
  mv -f "$DEST/$name.part" "$DEST/$name"
done
echo "$DEST"
