# AI Denoise model

AI Denoise runs NAFNet-SIDD-width32 as a Core ML float16 package, `DarkslideDenoise.mlpackage`. It is built from source when the app is built and bundled in `Darkslide.app/Contents/Resources/Models/`. Nothing is downloaded at runtime and nothing is published anywhere.

| | |
|---|---|
| Network | NAFNet-SIDD-width32. Chen, Chu, Zhang, Sun, *Simple Baselines for Image Restoration*, ECCV 2022 |
| Architecture | megvii-research/NAFNet@`2b4af71ebe098a92a75910c233a3965a3e93ede4`, rewritten for tracing in `scripts/denoise-model/nafnet.py` |
| Weights | `NAFNet-SIDD-width32.pth` from huggingface.co/mikestealth/nafnet-models@`cd3a91ddc41c9c3a53ee809b3ddd5263c9e94e51`, SHA-256 `89c70e808d1783b6c07911306e106aaf0d4f7f3da8c61078b99ff7f8929a26f4` |
| Licence | MIT, © 2022 megvii-model. It ships in the app as `Contents/Resources/Acknowledgements.md` (`app/Resources/Acknowledgements.md`) |
| Input / output | `input` → `output`: `[1, 3, 512, 512]` float16, planar RGB, sRGB-encoded 0..1 |
| Converted weights | `Data/com.apple.CoreML/weights/weight.bin`, SHA-256 `90a2538b1881275d5aa964b6aa6451baffe32b3167db3a526d9a123a3e818d5f`, 58,326,976 bytes |

## Build

`scripts/build-app.sh` calls `scripts/denoise-model/build-model.sh`. The steps:

1. **Cache check.** If `~/.cache/darkslide/models/<upstream weights SHA-256>/<stamp>/DarkslideDenoise.mlpackage` exists, the build uses it. The stamp hashes `build-model.sh`, `convert.py`, `nafnet.py` and `requirements.txt`, so a change to any of them builds into a new folder; a finished folder is never modified. `DS_MODEL_CACHE` overrides the cache location. Concurrent builds take turns on `/usr/bin/lockf` over the cache.
2. **Venv.** Otherwise the script creates a venv in the cache (`venv-py313`, from `python3.13`). It installs `scripts/denoise-model/requirements.txt` with `pip --require-hashes --only-binary=:all:`. Every dependency is pinned by version and wheel hash: torch 2.14.1, coremltools 9.0, numpy 2.5.3 and their dependencies, for Python 3.13 on macOS arm64.
3. **Convert.** `convert.py`:
   - downloads the pinned weights to a temporary name, renames them when complete, and checks their SHA-256 (a cached copy that doesn't match is downloaded again, once);
   - converts them to float16;
   - checks Core ML against PyTorch float32 on the GPU and the Neural Engine (max error 6e-4 and 1e-3);
   - checks that the converted weights hash to the pinned `EXPECTED_WEIGHTS_SHA256`.
4. **Bundle.** The package is copied into the app, which grows by about 56 MB.

`DS_SKIP_MODELS=1` skips the model for fast dev builds. A build without Python 3.13 or network, or with a failed check, prints a warning and still succeeds without the model.

The weights are reproducible bit for bit: a fresh venv from `requirements.txt` produced the pinned hash. `model.mlmodel` and `Manifest.json` embed a conversion date and random ids, so only the weights are pinned.

## Runtime

- **Loading.** The app takes `Models/DarkslideDenoise.mlpackage` from its bundle. It checks the weights against `DenoiseModelDescriptor.weightsSHA256` (the same value as `convert.py`; a test keeps them equal). It then compiles the package once into `<support>/Models/` and passes the checksum to Rust as the identity of the model's cached results.
- **No model in the build.** The AI Denoise control is disabled with "AI Denoise model not included in this build." Rust never gets a model id, so renders and exports ignore AI Denoise: export doesn't fail, it renders the original.
- **Checksum mismatch.** The model is refused, the control is disabled with an explanation, and the model is never compiled.

## Results, export and the viewer

- **Cached results** are files `<support>/DenoiseCache/<key>.dsdn`, 8 GB at most, least recently used out first. Each one is a 48-byte header (magic, version, size, BLAKE3 of the samples) followed by the samples.
  - The key hashes the photo's sensor data, the model, the denoise version and `PIPELINE_VERSION` (the BASE demosaic the result came from).
  - The export's check before rendering reads only the header and the length.
  - `denoise_ready` (the app asking for the photo on screen or one being exported) reads the samples into memory and verifies the BLAKE3. The render that follows uses that copy, so it isn't a second read. A damaged file therefore makes the app denoise the photo again instead of showing the original.
  - All hashing is serial: it can run inside a blend's one-time initialisation, which must not wait on a thread pool.
  - Timings for a 24 MP result (144 MB, page cache, release build):

    | Step | Time |
    |---|---|
    | Write + fsync | 127 ms |
    | BLAKE3 | 61 ms |
    | Header check | 0.07 ms |
    | Read + verify | 73 ms |
  - A damaged file is deleted. A file that can't be read right now (permissions, I/O) is kept.
  - Temporary files a crash left behind are trimmed after an hour.
- **Export** resolves each photo just before it renders.
  - A photo without a result is denoised then. Rust calls the app's `DenoiseProvider` from the export worker; the work runs in the background on the Neural Engine and never ahead of the photo on screen. A long export therefore never loses early results to the cache's limit.
  - The provider also waits for the model's check when an export starts at launch.
  - With no model in the build, the original is exported. With a damaged model, the photo fails with that reason.
  - Export renders its own blend, exactly the one it resolved. That blend is kept out of the viewer's cache and is dropped after the render.
  - A blend's seeded noise lives exactly as long as the last render holding the blend.
  - Cancelling the export cancels the denoise it is waiting for within ~50 ms.
  - When AI Denoise can't be applied (a failure, or a cancel), that photo fails with the reason. It is never exported without AI Denoise.
- **The viewer.**
  - The photo on screen is always denoised next.
  - Drafts during a slider drag reuse the photo's last blend; the settled render makes the exact strength. A blend is 3 ms; its BASE rebuild is the cost.
  - The viewer's prefetch reads the neighbours' results into memory, so moving to one skips the 144 MB read. Only the original's demosaic (~60 ms) remains.
  - Grid thumbnails show the original.
- **Thumbnails.** Turning AI Denoise on changes a photo's adjustments hash, so its cached thumbnail regenerates once. The thumbnail still shows the original.
- **Presets** carry AI Denoise (on/off and strength) with the Noise Reduction group.

## Benchmarks

`cargo bench -p ds-pipeline --bench denoise` measures one A9 frame (M4 Pro, under load):

| Step | Time |
|---|---|
| `DenoiseJob::new` (demosaic + encode) | 160 ms |
| Reading and writing all 117 tiles | 99 ms |
| `finish` (stitch + tone match) | 140 ms |
| `linear_original` | 61 ms |
| `denoised_raw` at 50% | 2.8 ms |

## fp16 and the LayerNorm prescale

NAFNet's widest activations reach about ±500 on real photos. Squared inside LayerNorm's variance they overflow float16, so a plain fp16 conversion returns noise.

`nafnet.py` divides every LayerNorm input by 16 first. This is exact: LayerNorm is scale-invariant, and its eps is scaled to match. It keeps the variance under 1024.

## Speed (M4 Pro, 512² tiles)

| Compute units | ms per tile | ms per megapixel |
|---|---|---|
| GPU | 41 | 156 |
| Neural Engine | 75 | 286 |
| GPU + Neural Engine, one instance each | 29 | 110 |

A 24 MP A9 frame is 117 tiles: 13 × 9, overlapping by at least 48 px. The photo on screen uses both compute units; a batch uses the Neural Engine only.
