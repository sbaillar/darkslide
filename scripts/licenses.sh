#!/usr/bin/env bash
# Third-party notices: check every dependency's licence (cargo-deny, deny.toml) and
# regenerate THIRD_PARTY_LICENSES.md from what Luxtable.app links (cargo-about on
# lt-ffi, about.toml + about.hbs). build-app.sh bundles the file; the About panel shows it.
#
#   scripts/licenses.sh           check, then rewrite THIRD_PARTY_LICENSES.md
#   scripts/licenses.sh --check   check, and fail if THIRD_PARTY_LICENSES.md is out of date (CI)
#
# Needs the pinned tools (the output is compared byte for byte; cargo-about needs `cli`):
#   cargo install --locked cargo-about@0.9.2 --features cli
#   cargo install --locked cargo-deny@0.20.2
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH" # where `cargo install` puts them

for tool in cargo-about cargo-deny; do
  command -v "$tool" >/dev/null || { echo "error: $tool not found (see the install lines at the top of $0)" >&2; exit 1; }
done

cargo deny --log-level error check licenses bans sources

# Luxtable.app (lt-ffi) must not link rawler (LGPL-2.1): only the separate RAW helper does.
if cargo tree --locked -p lt-ffi -e normal --prefix none 2>/dev/null | grep -q '^rawler '; then
  echo "error: lt-ffi links rawler; the app runs the RAW helper instead (docs/RAWDEC.md)" >&2
  exit 1
fi

OUT=THIRD_PARTY_LICENSES.md
TMP="$(mktemp)"
trap '/bin/rm -f "$TMP"' EXIT
cargo about generate --fail -c about.toml --manifest-path crates/lt-ffi/Cargo.toml about.hbs -o "$TMP"

if [[ "${1:-}" == --check ]]; then
  if ! cmp -s "$TMP" "$OUT"; then
    echo "error: $OUT is out of date: run scripts/licenses.sh and commit it" >&2
    diff -u "$OUT" "$TMP" | head -40 >&2 || true
    exit 1
  fi
  echo "$OUT is up to date"
else
  /bin/cp -f "$TMP" "$OUT"
  echo "wrote $OUT ($(grep -c '^- ' "$OUT") lines of crate credits)"
fi
