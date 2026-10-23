# Native RAW decoders

`ds-raw` decodes RAW sensor data with its own Apache-2.0 code first (`crates/ds-raw/src/native/`).
The LGPL-2.1 `rawler` crate covers the rest, but Darkslide.app doesn't link it: it runs in the
separate, optional **rawler helper** (`darkslide-rawler`, [crates/ds-rawler](../crates/ds-rawler/README.md)),
which users download from Settings ▸ RAW. This document records
what the native decoders cover, where each one's knowledge comes from, how they compare with
rawler, and what is left.

## Building with and without rawler

| Build | What happens |
|---|---|
| default (no features: the app, `ds-ffi`, `ds-remote`) | No rawler in the binary. Native decoders first; a native result or error is final, so a covered file decodes identically in every build. What they don't cover (format, compression, the camera data of non-DNG cameras, metadata of non-TIFF containers, `write_linear_dng`) goes to the **rawler helper** when one is set (`ds_raw::set_rawler_helper`, the FFI's `set_rawler_helper`, or `DS_RAWLER_HELPER`). Without one it is `RawError::Unsupported` ("… needs the rawler helper, which isn't installed"), which the app shows over the embedded preview with a Get RAW Helper button. |
| `rawler` feature (`ds-rawler`, the tests, the benchmarks) | rawler linked in and used in process; the helper is never asked. |

The helper is one process per request (`crates/ds-raw/src/helper.rs`): the command on its command
line, the RAW's own open descriptor (or an unlinked temporary file) as its stdin, a status byte and
length-prefixed parts on its stdout. Both ends of the protocol live in `helper.rs`; `ds-rawler`'s
`main` only moves bytes. A rawler crash or runaway allocation ends the helper, not the app. Camera
data answers are cached per camera. `crates/ds-rawler/tests/helper.rs` checks the built helper's
decodes against the in-process ones (every corpus file and the A9 samples: identical);
`crates/ds-raw/tests/without_rawler.rs` runs the routing in a build without the feature.

`RawFile::decode_native`, `RawFile::decode_rawler` and `RawFile::decode_with_helper` (hidden from
the docs) run one side only, for the parity tests and the benchmark. Tests that need rawler's
camera data (the synthetic and A9 Sony files) turn the feature on through dev-dependencies.

## Coverage

| Format | Native | Left to the fallback |
|---|---|---|
| DNG | CFA (2×2 Bayer, any `CFAPlaneColor` order) and `LinearRaw` (3 samples); uncompressed 8/16-bit and MSB-packed 1–15-bit strips or tiles; lossless JPEG strips and tiles; Deflate (integer and floating-point predictors, 16/24/32-bit floats scaled to 16 bits); lossy JPEG (8-bit, via `zune-jpeg`); `LinearizationTable`; `BlackLevel` with `BlackLevelRepeatDim`; `WhiteLevel`; `ActiveArea`; `DefaultCropOrigin/Size`; `ColorMatrix1/2` + `CalibrationIlluminant1/2`; `AsShotNeutral` / `AsShotWhiteXY` | 4-colour and non-2×2 CFAs, monochrome `LinearRaw`, uncompressed floats, JPEG XL (DNG 1.7) |
| Sony ARW | uncompressed (the existing fast path); compressed ARW 2 ("cRAW", compression 32767); lossless compressed (compression 7 tiles) when the first tile's frame holds exactly one tile | ARW 1, 12-bit packed ARW 2 variants, anything else |
| Canon CR2 | lossless JPEG with `CR2Slice` columns; levels and WB from `ColorData` layouts 1592 and 1602 (EOS 5D Mark IV, 6D Mark II) | other `ColorData` layouts (other bodies), sRAW/mRAW |
| Nikon NEF | lossless compressed 14-bit (decode table version 0x46); uncompressed 16-bit | lossy compressed, 12-bit lossless, packed uncompressed. "High Efficiency" (HE/HE*, Z 8 / Z 9): unsupported in every build, rawler can't decode it either |
| Pentax PEF | Huffman-compressed with the maker note's code table (0x0220) | PEFs without a table (older bodies' fixed code) |

Not touched (phase 2, still rawler): ORF, RW2, RAF (compressed and uncompressed), CR3 (CRX),
X-Trans, MRW and the rest; and the DNG writer (`write_linear_dng`).

Opcode lists, `BlackLevelDeltaH/V` and `DefaultScale` aren't applied, as rawler doesn't apply
them either (the corpus Pixel 7 Pro and Leica Q2 DNGs carry opcode lists and decode
bit-identically). The colour model is the `ColorMatrix` one of `crate::color`; `ForwardMatrix`,
`CameraCalibration` and DCP profiles aren't read.

## Camera data

DNGs carry their colour matrix, levels and crop. The other formats need per-camera data that
isn't in the file: the colour matrix always, and for some formats the crop or white level.
`crates/ds-raw/src/cameras.rs` looks it up in, in order:

1. **Darkslide's own table** (`cameras::TABLE`), with a provenance string per row. The only
   allowed sources are ones that can be redistributed under Apache-2.0. Today that is one row:
   the Pentax K-3 Mark III, whose matrix and crop the camera itself wrote into
   `pentax-k-3-mark-iii.dng` of the CC0 corpus. With it a K-3 III PEF develops without rawler.
2. **rawler's camera database** (with the `rawler` feature), through its public API: matrices,
   crop margins, white levels, CFA phase. The data stays in the LGPL crate; none of it is copied
   into Darkslide.

Without either, a non-DNG file is `Unsupported` ("no colour matrix for this camera"). File data
wins over table data: NEF crop (maker note 0x45), CR2 crop (`SensorInfo`), PEF white level
(maker note 0x7E).

**Owner decision (legal, open):** a rawler-free build can develop DNGs, the K-3 III PEF, and
nothing else from Sony, Canon or Nikon until the table has their matrices. Where those may
legally come from (Adobe DNG Converter output for each camera, the vendors' own DNG output,
measuring with a chart, a permissively licensed dataset, or a licence from a database's
authors) is a question for the project owner and, if needed, counsel. This work deliberately
does not resolve it and copies no matrices from rawler, dcraw, LibRaw or Adobe's SDK.

## Provenance

Clean-room rules for this work: no source of rawler, rawspeed, dcraw, LibRaw, darktable,
RawTherapee or any other GPL/LGPL decoder was opened. Each decoder is written from public
specifications, from inspecting the corpus files' bytes, and from rawler used strictly as a
black box (calling it and comparing its decoded output; its public API documentation was read
to call it). Inspection was done with small scripts in a scratch directory (TIFF/IFD and maker
note dumpers, a lossless JPEG header dumper), not committed.

| Decoder | Based on |
|---|---|
| Lossless JPEG (`native/ljpeg.rs`) | ITU-T T.81 Annex H (process 14): SOF3/DHT/DRI/SOS, Table H.1 predictors, F.2.2.3 decoding, F.12 `EXTEND`, B.1.1.2 fill bytes, restart markers. The table-driven decoder and the branch-free 8-byte refill are general techniques. |
| DNG (`native/dng.rs`, `deflate.rs`, `unpack.rs`) | Adobe DNG Specification 1.7 (chapters 4 and 6), TIFF 6.0 (strips, tiles, `FillOrder` 1 packing, `Predictor`), Adobe TIFF Technical Note 3 (floating-point predictor), RFC 1950/1951 via `miniz_oxide` (MIT/Zlib/Apache-2.0), baseline JPEG via `zune-jpeg` (MIT/Apache-2.0/Zlib). |
| Sony cRAW (`native/arw.rs`) | Corpus inspection: the 16-byte block layout (11-bit max/min, 4-bit positions, 14 × 7-bit offsets with the smallest covering shift) and the `SonyToneCurve` expansion (slopes 1, 2, 4, 8, 16 between breakpoints `0x7010[i] / 4` over `k = 2v`) were fitted to the A7 III file's bytes against rawler's decoded values (`tone_curve_matches_the_corpus_observations` keeps the observed points). Levels and WB come from the pre-existing `sony.rs`. |
| Sony lossless | TIFF tiles of lossless JPEG, as DNG. No corpus sample: unverified against real files, guarded by the frame-equals-tile check. |
| Canon CR2 (`native/cr2.rs`) | Lossless JPEG per T.81; the slice layout (`CR2Slice` = count, width, last width; stream fills slices column-block by column-block) from inspecting both corpus CR2s; `SensorInfo` borders and the `ColorData` indices of WB (63), black (778–781) and specular white (783) found by searching the corpus files' maker notes for rawler's reported values. |
| Nikon NEF (`native/nef.rs`) | Corpus inspection for the maker note (`Nikon\0` + embedded TIFF, tags 0x0C, 0x3D, 0x45, 0x93, 0x96). The predictor structure (two columns left; row starts against the row two above, seeded by the decode table) and the 14-bit lossless Huffman code (`LOSSLESS_14`) were **inferred by black-box search**: a backtracking solver read the D850/Z 6II bitstreams against rawler's decoded pixels (tolerating its ±1 dither) and recovered each size category's code; the result decodes both corpus files completely. |
| Pentax PEF (`native/pef.rs`) | Corpus inspection: the 0x0220 table layout (14-byte header, `u16` codes left-aligned to the width at byte 4, then lengths) and the scheme (as NEF, predictors from 0) were checked by decoding the K-3 III PEF against rawler. |
| Camera table (`cameras.rs`) | The K-3 III's own DNG (CC0). |

Pre-existing code that this work builds on (`sony.rs` levels/WB and the SR2 decryption, the TIFF
walker, the Sony uncompressed fast path) predates the clean-room rules; `sony.rs` cites dcraw for
the SR2 cipher's algorithm. Whether that needs a clean re-derivation is noted here for the owner.

## Parity with rawler

`crates/ds-raw/tests/compat.rs` `native_decoders_match_rawler` decodes every corpus file
(`DS_RAWCOMPAT_DIR`, see CAMERAS.md) and two A9 samples both ways and asserts identical sensor
data: size (the crop), CFA, black and white levels and every sample. Understood differences are
listed in the test (`KNOWN`) and reported, not failed:

| File | Result |
|---|---|
| `apple-iphone-xs.dng`, `google-pixel-7-pro.dng`, `leica-m10-r.dng`, `leica-q2.dng`, `pentax-k-3-mark-iii.dng` | identical |
| `canon-eos-5d-mark-iv.cr2`, `canon-eos-6d-mark-ii.cr2` | identical |
| A9 `SKB06204.ARW`, `SKB06368.ARW` (uncompressed) | identical |
| `apple-iphone-12-pro-proraw.dng` | rawler dithers `LinearizationTable` output by up to ±½ table step (and wraps two 65535 samples to 0); native maps exactly as the DNG spec defines |
| `sony-ilce-7m3.arw`, `sony-ilce-7m4.arw` | rawler dithers cRAW values across each tone-curve step (|Δ| ≤ 8); native returns the curve value |
| `nikon-d850.nef`, `nikon-z-6-2.nef` | rawler dithers decoded values down by 0 or 1; native returns the decoded value |
| `pentax-k-3-mark-iii.pef` | samples identical; white 16379 (the camera's maker note, as in its DNG) vs rawler's database 16375 |

Colour (WB and `cam_to_xyz`) is identical on every file except the K-3 III PEF, which uses the
camera table's in-camera-DNG matrix instead of rawler's database matrix.

## Performance

Criterion, `cargo bench -p ds-raw --bench raw -- decode/` with `DS_RAWCOMPAT_DIR` set: full decode
to `RawImage`, release, M4 Pro, file in page cache, median of 10.

| File | Format | Native (ms) | rawler (ms) | Speed-up |
|---|---|---:|---:|---:|
| `apple-iphone-12-pro-proraw.dng` | DNG LinearRaw, LJ92 tiles | 24.0 | 93.3 | 3.9× |
| `apple-iphone-xs.dng` | DNG CFA, LJ92 tiles | 10.1 | 26.4 | 2.6× |
| `google-pixel-7-pro.dng` | DNG CFA, LJ92 tiles | 18.1 | 24.9 | 1.4× |
| `leica-m10-r.dng` | DNG CFA, LJ92 strip | 154.5 | 300.6 | 1.9× |
| `leica-q2.dng` | DNG CFA, uncompressed | 4.1 | 11.3 | 2.8× |
| `pentax-k-3-mark-iii.dng` | DNG CFA, LJ92 strip | 112.4 | 237.0 | 2.1× |
| `canon-eos-5d-mark-iv.cr2` | CR2 LJ92 slices | 225.7 | 367.6 | 1.6× |
| `canon-eos-6d-mark-ii.cr2` | CR2 LJ92 slices | 244.1 | 265.5 | 1.1× |
| `nikon-d850.nef` | NEF 14-bit lossless | 120.2 | 233.8 | 1.9× |
| `nikon-z-6-2.nef` | NEF 14-bit lossless | 128.8 | 216.9 | 1.7× |
| `pentax-k-3-mark-iii.pef` | PEF Huffman | 100.7 | 183.9 | 1.8× |
| `sony-ilce-7m3.arw` | ARW cRAW | 11.4 | 18.1 | 1.6× |
| `sony-ilce-7m4.arw` | ARW cRAW | 16.4 | 29.6 | 1.8× |
| A9 `samples/*.ARW` (`a9_arw/decode` vs `rawler_decode_baseline`) | ARW uncompressed | 1.26 | 3.24 | 2.6× |

Rows decode in parallel bands (uncompressed, cRAW), tiles and strips in parallel (DNG), CR2
slices are reassembled in parallel; single-stream codecs (CR2, NEF, PEF, single-strip DNG) are
serial in their entropy decoding, as they are in rawler, and win on a faster Huffman decoder.

## Phase 2

- ORF (Olympus/OM), RW2 (Panasonic), RAF compressed and uncompressed (Fujifilm), CR3 (Canon CRX).
- X-Trans demosaic (the pipeline, not the decoder).
- Replace rawler's DNG writer behind `write_linear_dng`.
- Native metadata for CR3/RAF/ORF/RW2 (today through rawler's header parsers).
- More `ColorData` layouts (CR2), lossy and 12-bit NEF, the fixed PEF code of older Pentax bodies.
- Camera data for non-DNG cameras (owner decision above), so Sony, Nikon and Canon files develop
  without the helper.
