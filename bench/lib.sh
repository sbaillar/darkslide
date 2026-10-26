# shellcheck shell=bash
# Shared helpers for the benchmark scripts (bash; sourced by them, never by an
# interactive shell: `die` exits). Use bench/stage.sh to stage photos by hand.

source "$(dirname "${BASH_SOURCE[0]}")/../scripts/lib/env-compat.sh"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# RAW extensions the kit stages (the apps under test all read these).
RAW_GLOB='\.(arw|cr2|cr3|nef|nrw|raf|rw2|orf|dng|pef|srw|3fr|iiq|erf|rwl)$'

# stage <source> <dest> <count> [cycle]: copy the first <count> RAWs (by name) of
# <source> into the new folder <dest>. <source> is only read. APFS clones when on the
# same volume, so staging costs no disk space. With `cycle`, a small set (a test pack)
# is repeated until there are <count> files, each named `<n>-<name>`.
stage() {
  local src="$1" dest="$2" count="$3" cycle="${4:-}"
  [[ -d "$src" ]] || die "no folder $src"
  [[ ! -e "$dest" ]] || die "$dest already exists"
  local files=() f
  while IFS= read -r f; do files+=("$f"); done < <(/bin/ls -1 "$src" | grep -Ei "$RAW_GLOB" | LC_ALL=C sort)
  [[ "${#files[@]}" -gt 0 ]] || die "no RAW files in $src"
  local total="${#files[@]}"
  if [[ "$cycle" == cycle ]]; then
    total="$count"
  elif [[ "$count" -gt 0 && "$count" -lt "$total" ]]; then
    total="$count"
  fi
  mkdir -p "$dest"
  local i name
  for ((i = 0; i < total; i++)); do
    f="${files[i % ${#files[@]}]}"
    name="$f"
    [[ "$cycle" == cycle ]] && name="$(printf '%04d-%s' $((i + 1)) "$f")"
    /bin/cp -c "$src/$f" "$dest/$name" 2>/dev/null || /bin/cp "$src/$f" "$dest/$name"
  done
  echo "$total"
}

# lt_tool <binary> <args…>: run a Luxtable binary in one of its tool modes
# (--bench…) with a scratch support folder and so private preferences, never the
# user's own library: if the build didn't know the mode, it would start the app.
lt_tool() {
  local bin="$1" support rc=0
  shift
  support="$(mktemp -d -t luxtable-bench-tool)"
  LT_SUPPORT_ROOT="$support" "$bin" "$@" || rc=$?
  /bin/rm -rf "${support:?}"
  return "$rc"
}

# c1_app: the Capture One to drive: $C1_APP, else the one installed ("Capture One" or
# "Capture One 23"). Two installed and no C1_APP: stop (a script must never pick one).
c1_app() {
  if [[ -n "${C1_APP:-}" ]]; then
    [[ -d "/Applications/$C1_APP.app" ]] || die "C1_APP: /Applications/$C1_APP.app not found"
    echo "$C1_APP"
    return
  fi
  local found=() n
  for n in "Capture One" "Capture One 23"; do [[ -d "/Applications/$n.app" ]] && found+=("$n"); done
  [[ "${#found[@]}" == 1 ]] || die "set C1_APP to the Capture One to use (found ${#found[@]} in /Applications)"
  echo "${found[0]}"
}

# purge_cache: drop the OS file cache for a cold run. Needs sudo without a prompt
# (`sudo -v` once before the run); returns 1 when it couldn't.
purge_cache() {
  sudo -n /usr/sbin/purge 2>/dev/null
}

# is_internal_ssd <path>: the path is on an internal solid-state disk.
is_internal_ssd() {
  local dev info
  dev="$(df "$1" | tail -1 | awk '{print $1}')"
  info="$(diskutil info "$dev" 2>/dev/null)"
  grep -Eq 'Device Location: +Internal|Internal: +Yes' <<<"$info" && grep -Eq 'Solid State: +Yes' <<<"$info"
}

# preflight <path>: machine-state checks (one line each on stdout, prefixed ok/WARN).
preflight() {
  if pmset -g batt | grep -q "AC Power"; then echo "ok: on AC power"; else echo "WARN: on battery"; fi
  if pmset -g | grep -Eq 'lowpowermode +1'; then echo "WARN: Low Power Mode is on"; else echo "ok: Low Power Mode off"; fi
  if is_internal_ssd "$1"; then echo "ok: files on the internal SSD"; else echo "WARN: files are not on an internal SSD"; fi
  local busy
  busy="$(ps -Ao pcpu=,comm= | awk '$1 > 5 && ($2 ~ /mds|mdworker|mediaanalysisd|photoanalysisd|backupd/) {printf "%s %s%%; ", $2, $1}')"
  if [[ -z "$busy" ]]; then echo "ok: Spotlight, media analysis and Time Machine quiet"; else echo "WARN: busy: $busy"; fi
  # Events go to the frontmost app: no other Luxtable or Capture One may be open.
  if pgrep -xq Luxtable; then echo "WARN: Luxtable is running: quit it"; else echo "ok: no Luxtable running"; fi
  if pgrep -fq "/Applications/Capture One"; then echo "WARN: Capture One is running: quit it"; else echo "ok: Capture One not running"; fi
  local load cores
  load="$(sysctl -n vm.loadavg | awk '{print $2}')"
  cores="$(sysctl -n hw.ncpu)"
  if awk -v l="$load" -v c="$cores" 'BEGIN { exit !(l > c / 4) }'; then
    echo "WARN: busy: 1-minute load average $load on $cores cores"
  else
    echo "ok: 1-minute load average $load"
  fi
}

# json_string <text>: a JSON string literal.
json_string() {
  local s="$1"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  printf '"%s"' "$s"
}
