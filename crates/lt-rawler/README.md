# luxtable-rawler

The **rawler helper**: a small, separate program that runs the
[rawler](https://github.com/dnglab/dnglab/tree/main/rawler) RAW decoder for Luxtable.

Luxtable decodes the common RAW formats with its own Apache-2.0 decoders
([docs/RAWDEC.md](../../docs/RAWDEC.md)). rawler, which is licensed under the
**GNU Lesser General Public License 2.1** (LGPL-2.1), covers what those don't yet. To keep
Luxtable itself free of LGPL code, rawler isn't built into the app. It lives in this
separate program, which you install only if you need it.

## Do I need it?

You need it if you shoot any of these:

| You shoot | Without the helper | With it |
|---|---|---|
| Canon CR3, Fujifilm RAF, Olympus/OM ORF, Panasonic RW2, and other formats Luxtable doesn't decode natively | culling from the embedded preview only | develops |
| Sony ARW, Nikon NEF, Canon CR2 (native decoders) | culling from the embedded preview only: these files carry no colour matrix, and Luxtable's own camera table doesn't have one for your camera yet | develops (colour matrix, crop and white level from rawler's camera data) |
| DNG (any camera, including iPhone ProRAW and Pixel), Pentax K-3 Mark III | develops | develops |
| HDR and panorama merges (written as DNG) | not available | available |

Nikon "High Efficiency" NEFs (Z 8, Z 9) don't develop either way: rawler can't decode them.

## Installing

- **In the app:** Settings ▸ RAW ▸ Download RAW Helper, or the button on a photo that
  says it needs the helper. Luxtable downloads `luxtable-rawler-macos-arm64` from this
  repository's GitHub releases, checks its SHA-256 against the value built into the app,
  and installs it in `~/Library/Application Support/Luxtable/Helpers/`.
- **From source:** `scripts/build-rawler-helper.sh --install` (needs Rust).
- **Somewhere else:** set `LT_RAWLER_HELPER=/path/to/luxtable-rawler` in Luxtable's
  environment.

Remove it in Settings ▸ RAW, or by deleting the file. Nothing else changes: photos that
need it go back to their embedded previews.

## How it works

Luxtable runs the helper once per request (decode a file, read a file's metadata, look up a
camera, write a DNG). It sends the open RAW file as the helper's standard input and reads
the answer from its standard output. The protocol is in `crates/lt-raw/src/helper.rs`; this
program only moves bytes. Because it runs in a separate process, a crash in the decoder
ends the helper, never Luxtable.

## Licence

This crate's own source is Apache-2.0, like the rest of Luxtable. The **built program**
statically links rawler (LGPL-2.1, © the rawler authors) and its other dependencies, and is
distributed under the LGPL-2.1's terms:

- rawler's source is at <https://github.com/dnglab/dnglab> (crate `rawler` 0.8.0 on
  crates.io), and the full text of the LGPL-2.1 is at
  <https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html>.
- To relink the program with a modified rawler, change the `rawler` dependency in
  `crates/lt-raw/Cargo.toml` (a path or a git source) and run
  `scripts/build-rawler-helper.sh`. The complete source of everything else in the program
  is in this repository.
- The helper is a separate program that talks to Luxtable over a documented protocol.
  It doesn't change Luxtable's licence, and Luxtable works without it.
