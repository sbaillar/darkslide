# Benchmark kit

Compares Darkslide with Capture One and Lightroom Classic on the same Mac and RAWs.
The method and the settings are in [docs/BENCHMARKS.md](../docs/BENCHMARKS.md).

| File | What it does |
|---|---|
| `compare.sh` | the full comparison, one command: stage, preflight, every app, report |
| `run-darkslide.sh` | Darkslide only: in-app timings (headless), optionally on-screen |
| `run-captureone.sh` | Capture One: AppleScript for import/open/export, the screen driver for the viewer |
| `lightroom-checklist.md` | Lightroom Classic: manual steps plus the screen driver |
| `plans/*.json` | per-app screen-driver plans (viewer region, stimuli) |
| `stage.sh` | stage RAWs by hand (any shell) |
| `lib.sh` | helpers the scripts source (bash only; not for an interactive shell) |
| `fetch-pack.sh` | download the standard test pack, verified against pinned hashes |
| `packs/*.json` | test pack manifests (URLs, BLAKE3/SHA-256, licence, camera) |
| `schema/bench-result-v1.schema.json` | the shareable result format (`result.json`) |

The timing code is part of the app: `Darkslide --bench`, `--bench-drive`,
`--bench-report` and `--bench-share` (app/Sources/DarkslideMac/Bench/).

## Quick start: Darkslide only

```sh
DS_SKIP_MODELS=1 scripts/build-app.sh
bench/stage.sh /path/to/raws /tmp/bench-raws 150    # APFS clones; the source is only read
bench/run-darkslide.sh /tmp/bench-raws /tmp/bench-out --runs 3
DS_SUPPORT_ROOT=$(mktemp -d) dist/Darkslide.app/Contents/MacOS/Darkslide --bench-report /tmp/bench-out/results
```

This needs no permissions and never touches your library or preferences. Each launch
gets a scratch support folder, and the RAWs are cloned per pass.

## The full comparison

Once:

1. In Capture One, create an empty session (File ▸ New Session, import nothing) and
   quit. Its folder is the `--c1-session` template. Each run copies it and never
   writes to it.
2. Give the terminal **Screen Recording** and **Accessibility** (System Settings ▸
   Privacy & Security), and allow it to control Capture One when asked (Automation).
3. Set Capture One's Hardware Acceleration to Auto (display and processing), and its
   preview size to 2560 px.
4. Calibrate `plans/captureone.json` and `plans/darkslide.json` for your window layout
   (docs/BENCHMARKS.md, Screen timing).

Each time:

```sh
sudo -v          # lets cold runs purge the file cache
bench/compare.sh --pack standard --c1-session /path/to/EmptySession --count 150 --runs 3
# or with your own photos (a "custom" result, not ranked):
bench/compare.sh --raws /path/to/150-500-raws --c1-session /path/to/EmptySession --count 150 --runs 3
```

Before you run it: quit Capture One and other apps, plug in power, and don't use the
Mac while it runs (about 30-60 minutes for 150 photos and 3 runs).

The report is written to `$TMPDIR/darkslide-bench/<date>/report.md`, with the raw
samples in `results/*.csv` (one row per sample) and `results/*.json`. A shareable
`result.json` is written beside it, with no names, paths or serial numbers
(docs/BENCHMARKS.md, Sharing results). Nothing is uploaded.

After a Capture One run, Capture One's "last document" points at a bench session the
script deleted. Its next normal launch falls back to its default document; just open
your own session or catalog again.

Options:

- `--no-captureone`: Darkslide only.
- `--no-screen`: in-app timings only.
- `--force`: record a run even if a preflight check fails.

## Results format

One CSV row per sample:

`app,operation,mode,run,sample,ms,items,method`

- `ms` is empty when a held-arrow press was missed.
- `method` is one of `in-app`, `screen`, `applescript` or `manual`.

Manual Lightroom timings go in a CSV of the same shape in `results/`, and
`--bench-report` merges every CSV in the folder.
