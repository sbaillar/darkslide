#!/usr/bin/env bash
# Download a test pack (bench/packs/<id>.json) and verify every file against its
# pinned hash. The files are never committed.
#
#   bench/fetch-pack.sh [<manifest>] [<dest>]
#
# Defaults: bench/packs/standard-v1.json into ~/Library/Caches/darkslide-bench/<id>.
# Files already there with the right hash are kept. Each file is verified against its
# pinned SHA-256 (`shasum`, part of macOS) and, when `b3sum` is installed (brew install
# b3sum), its pinned BLAKE3 too. A file that fails verification is deleted and the
# script stops. Prints the folder on success.
set -euo pipefail
cd "$(dirname "$0")/.."
source bench/lib.sh

MANIFEST="${1:-bench/packs/standard-v1.json}"
[[ -f "$MANIFEST" ]] || die "no manifest $MANIFEST"
field() { /usr/bin/plutil -extract "$1" raw -o - "$MANIFEST"; }
ID="$(field id)"
DEST="${2:-$HOME/Library/Caches/darkslide-bench/$ID}"
mkdir -p "$DEST"

# SHA-256 always (the pack hash is built from it); BLAKE3 too when b3sum is installed.
verify() { # verify <file> <blake3> <sha256>
  [[ "$(shasum -a 256 "$1" | awk '{print $1}')" == "$3" ]] || return 1
  if command -v b3sum >/dev/null; then [[ "$(b3sum --no-names "$1")" == "$2" ]]; fi
}

count="$(/usr/bin/plutil -extract files raw -o - "$MANIFEST")"
for i in $(seq 0 $((count - 1))); do
  name="$(field "files.$i.name")"
  url="$(field "files.$i.url")"
  b3="$(field "files.$i.blake3")"
  sha="$(field "files.$i.sha256")"
  lic="$(field "files.$i.licence")"
  [[ "$name" != */* && "$name" != .* ]] || die "bad file name in manifest: $name"
  [[ "$lic" == CC0-1.0 ]] || die "$name: licence $lic is not CC0-1.0"
  if [[ -f "$DEST/$name" ]] && verify "$DEST/$name" "$b3" "$sha"; then
    continue
  fi
  log "fetch $name"
  curl -fL --proto '=https' --proto-redir '=https' --retry 3 --silent --show-error "$url" -o "$DEST/$name.part"
  if ! verify "$DEST/$name.part" "$b3" "$sha"; then
    /bin/rm -f "$DEST/$name.part"
    die "$name: hash mismatch (the download is not the pinned file)"
  fi
  mv -f "$DEST/$name.part" "$DEST/$name"
done
echo "$DEST"
