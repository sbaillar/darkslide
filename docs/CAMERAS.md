# Supported cameras

What Luxtable does with RAWs from each maker, measured by the opt-in compatibility test
`crates/lt-raw/tests/compat.rs` on one or more CC0 sample files per format from
[raw.pixls.us](https://raw.pixls.us) (manifest: `crates/lt-raw/tests/compat/samples.json`).

- **full**: imports, culls from the embedded preview, and develops (full sensor decode).
- **cull only**: imports and culls (metadata, embedded preview, ratings, tags, compare);
  the viewer says "Luxtable can't develop this file yet" instead of developing it.

The **Decoder** column says which decoder develops the format: Luxtable's own (native,
[RAWDEC.md](RAWDEC.md)) or rawler (in the separate RAW helper the app downloads on request; see RAWDEC.md). Native
decodes of the corpus match rawler's sensor data; RAWDEC.md lists the few understood
differences (rawler dithers some value mappings).

| Format | Cull (metadata + embedded preview) | Develop | Decoder |
|---|---|---|---|
| Sony ARW | ✅ | ✅ | native: uncompressed, compressed (cRAW), lossless; colour matrix from rawler's camera DB |
| Nikon NEF | ✅ | ✅, except Z 8 / Z 9 "High Efficiency" (HE/HE*) files: cull only (no decoder has that codec) | native: 14-bit lossless and uncompressed; lossy and 12-bit lossless via rawler; colour matrix from rawler's camera DB |
| Canon CR2 | ✅ | ✅ | native for the EOS 5D Mark IV and 6D Mark II (`ColorData` layouts checked so far), rawler otherwise; colour matrix from rawler's camera DB |
| Canon CR3 | ✅ (1620×1080 `PRVW` preview) | ✅ | rawler |
| Fujifilm RAF, Bayer (GFX) | ✅ | ✅ | rawler |
| Fujifilm RAF, X-Trans (X-T, X-S, X-H, X-Pro, X-E…) | ✅ | ❌ cull only: Luxtable demosaics 2×2 Bayer sensors only | – |
| Olympus / OM System ORF | ✅ (maker-note preview) | ✅ | rawler |
| Panasonic RW2 | ✅ (`JpgFromRaw`) | ✅ | rawler |
| Pentax PEF and DNG | ✅ | ✅ | native (K-3 Mark III fully, colour included; other bodies' matrices from rawler's DB) |
| Leica DNG | ✅ | ✅ | native |
| Phone DNG (Google Pixel, iPhone RAW) | ✅ | ✅ | native |
| Apple ProRAW | ✅ | ✅ (linear RGB DNG) | native |
| Canon CRW, Sigma X3F, Phase One IIQ, ARRI | metadata only | ❌ refused (their sizes aren't bounds-checked before decoding) | – |

A build without the `rawler` feature develops DNGs and the K-3 Mark III PEF only; everything
else is cull only there (see RAWDEC.md, "Camera data"). Other cameras rawler supports should
work the same way as their format above, but only the files below are tested. For CR3, the culling preview is the 1620×1080 `PRVW` JPEG; the full-size
JPEG in the file's first track isn't read, so zooming past that before developing shows the
preview upscaled.

## Results

Release build on an M4 Pro, file in the page cache. Times are one run each: metadata is the
first read of the file's header; the embedded-preview budget is ≤ 10 ms.

| Format | Camera | File | Metadata | Embedded preview | Full decode | Result |
|---|---|---|---|---|---|---|
| DNG | Apple iPhone 12 Pro | `apple-iphone-12-pro-proraw.dng` | ISO 32, 1/5556, f/1.6, 4 mm, 2020-12-29, 0.6 ms | 4032×3024 (5238 KB), 0.4 ms | 4032×3024 LinearRgb, 63 ms | full |
| DNG | Apple iPhone XS | `apple-iphone-xs.dng` | ISO 25, 1/450, f/1.8, 4 mm, 2019-09-07, 0.3 ms | 852×640 (217 KB), 0.1 ms | 4032×3024 Rggb, 15 ms | full |
| CR2 | Canon Canon EOS 5D Mark IV | `canon-eos-5d-mark-iv.cr2` | ISO 100, 1/3200, f/2.8, 40 mm, 2017-01-26, 0.6 ms | 6720×4480 (2094 KB), 0.2 ms | 6720×4480 Rggb, 212 ms | full |
| CR2 | Canon Canon EOS 6D Mark II | `canon-eos-6d-mark-ii.cr2` | ISO 100, 1/100, f/8.0, 35 mm, 2017-08-05, 1.4 ms | 6240×4160 (4385 KB), 0.3 ms | 6240×4160 Rggb, 199 ms | full |
| CR3 | Canon EOS R5 | `canon-eos-r5.cr3` | ISO 100, 1/3, f/5.6, 50 mm, 2021-05-15, 28.3 ms | 1620×1080 (293 KB), 0.0 ms | 8192×5464 Rggb, 172 ms | full |
| CR3 | Canon EOS R6 | `canon-eos-r6.cr3` | ISO 100, 1/4, f/25.0, 200 mm, 2021-05-12, 10.0 ms | 1620×1080 (447 KB), 0.0 ms | 5472×3648 Rggb, 75 ms | full |
| RAF | Fujifilm GFX 50S | `fujifilm-gfx-50s.raf` | ISO 100, 1/280, f/14.0, 63 mm, 2017-05-25, 19.3 ms | 4000×3000 (1907 KB), 0.1 ms | 8256×6192 Rggb, 91 ms | full |
| RAF | Fujifilm X-S10 | `fujifilm-x-s10.raf` | ISO 160, 1/3, f/4.0, 70 mm, 2020-11-28, 9.6 ms | 4416×2944 (2751 KB), 0.2 ms | not yet: X-Trans sensor (6×6 colour filter); Luxtable demosaics 2×2 Bayer sensors only | cull only |
| RAF | Fujifilm X-T4 | `fujifilm-x-t4.raf` | ISO 160, 1/125, f/3.6, 16 mm, 2020-05-08, 10.1 ms | 4416×2944 (4118 KB), 0.3 ms | not yet: X-Trans sensor (6×6 colour filter); Luxtable demosaics 2×2 Bayer sensors only | cull only |
| DNG | Google Pixel 7 Pro | `google-pixel-7-pro.dng` | ISO 142, 1/24, f/3.5, 19 mm, 2022-12-04, 0.3 ms | 1280×964 (210 KB), 0.0 ms | 3866×2908 Bggr, 15 ms | full |
| DNG | Leica Camera AG LEICA M10-R | `leica-m10-r.dng` | ISO 100, 1/60, f/–, 50 mm, 2025-05-09, 0.4 ms | 7840×5184 (3365 KB), 0.2 ms | 7864×5200 Gbrg, 310 ms | full |
| DNG | LEICA CAMERA AG LEICA Q2 | `leica-q2.dng` | ISO 100, 1/500, f/6.3, 28 mm, 2019-04-14, 0.5 ms | 8368×5584 (3814 KB), 0.2 ms | 8368×5584 Gbrg, 34 ms | full |
| NEF | NIKON CORPORATION NIKON D850 | `nikon-d850.nef` | ISO 64, 1/160, f/9.0, 35 mm, 2017-10-15, 0.5 ms | 8256×5504 (5190 KB), 0.3 ms | 8256×5504 Rggb, 253 ms | full |
| NEF | NIKON CORPORATION NIKON Z 6_2 | `nikon-z-6-2.nef` | ISO 100, 1/40, f/6.0, 70 mm, 2020-11-14, 1.7 ms | 6048×4024 (2968 KB), 0.2 ms | 6048×4024 Rggb, 161 ms | full |
| NEF | NIKON CORPORATION NIKON Z 8 | `nikon-z-8-he.nef` | ISO 64, 1/80, f/8.0, 24 mm, 2023-06-02, 0.8 ms | 8256×5504 (5741 KB), 0.3 ms | not yet: High Efficiency (HE/HE*) compression isn't supported by the RAW decoder yet | cull only |
| ORF | Olympus E-M1MarkIII | `olympus-e-m1-mark-iii.orf` | ISO 200, 1/15, f/2.8, 35 mm, 2020-03-16, 6.8 ms | 3200×2400 (947 KB), 0.1 ms | 5184×3888 Rggb, 144 ms | full |
| ORF | OM Digital Solutions OM-1 | `om-system-om-1.orf` | ISO 200, 1/320, f/2.8, 14 mm, 2022-03-05, 9.4 ms | 3200×2400 (939 KB), 0.1 ms | 5184×3888 Rggb, 190 ms | full |
| RW2 | Panasonic DC-GH5 | `panasonic-dc-gh5.rw2` | ISO 400, 1/250, f/5.6, 27 mm, 2017-06-13, 10.7 ms | 1920×1440 (688 KB), 0.1 ms | 5184×3888 Rggb, 7 ms | full |
| RW2 | Panasonic DC-S1R | `panasonic-dc-s1r.rw2` | ISO 100, 1/200, f/8.0, 35 mm, 2019-12-31, 26.2 ms | 1920×1280 (646 KB), 0.0 ms | 8368×5584 Gbrg, 11 ms | full |
| DNG | RICOH IMAGING COMPANY, LTD. PENTAX K-3 Mark III | `pentax-k-3-mark-iii.dng` | ISO 100, 1/1000, f/4.0, 70 mm, 2021-05-17, 0.6 ms | 6192×4128 (3088 KB), 0.2 ms | 6192×4128 Rggb, 172 ms | full |
| PEF | RICOH IMAGING COMPANY, LTD. PENTAX K-3 Mark III | `pentax-k-3-mark-iii.pef` | ISO 100, 1/800, f/4.0, 70 mm, 2021-05-17, 0.6 ms | 6192×4128 (3029 KB), 0.2 ms | 6192×4128 Rggb, 148 ms | full |
| ARW | SONY ILCE-7M3 | `sony-ilce-7m3.arw` | ISO 400, 1/50, f/4.0, 32 mm, 2018-03-13, 0.7 ms | 1616×1080 (1123 KB), 0.1 ms | 6000×4000 Rggb, 15 ms | full |
| ARW | SONY ILCE-7M4 | `sony-ilce-7m4.arw` | ISO 250, 1/50, f/5.6, 50 mm, 2023-10-19, 0.5 ms | 7008×4672 (6801 KB), 0.4 ms | 7008×4672 Rggb, 21 ms | full |

Metadata of the formats read through rawler (CR3, RAF, ORF, RW2) takes 7–30 ms, over the
5 ms budget of the built-in TIFF reader; it is read once per file, at import.

## Regenerate

```sh
scripts/fetch-rawcompat.sh      # ~0.9 GB into ~/Library/Caches/luxtable-rawcompat, SHA-256 verified
LT_RAWCOMPAT_DIR=~/Library/Caches/luxtable-rawcompat LT_RAWCOMPAT_REPORT=/tmp/cameras.md \
  cargo test -p lt-raw --release --test compat -- --nocapture
LT_RAWCOMPAT_DIR=~/Library/Caches/luxtable-rawcompat cargo bench -p lt-raw -- compat   # preview time per file
LT_RAWCOMPAT_DIR=~/Library/Caches/luxtable-rawcompat cargo bench -p lt-raw -- decode/  # native vs rawler decode
```

The same test run includes `native_decoders_match_rawler`, the native/rawler parity check
(RAWDEC.md).

The test fails if any file's metadata or embedded preview can't be read, or if a decode fails
with anything other than "unsupported". Paste the table it writes into the Results section.
