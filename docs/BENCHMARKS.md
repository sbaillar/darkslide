# Benchmarks: Darkslide vs Capture One vs Lightroom Classic

How Darkslide is compared with Capture One and Lightroom Classic on the same Mac and
the same RAW files, so anyone can rerun it. The kit is in `bench/` (bench/README.md);
results are written as CSV/JSON plus a markdown report.

## Method

### What others do

There is no formal standard for timing RAW editors. The closest de facto one is Puget
Systems' **Puget Bench for Lightroom Classic** (1.x, 2025):

- **Test set.** Real camera RAWs, 44-50 MP (Canon R5 II `.CR3`, Panasonic S1R II `.RW2`;
  the extended preset adds Sony A1 II `.ARW` and Nikon Z8 `.NEF`).
- **Operations.** Import 250 photos (Add from location, Minimal previews, no Smart
  Previews), create 100 Smart Previews, export 100 JPEGs (quality 60, no size limit),
  export 100 DNGs, plus AI tasks (masking, Denoise, Super Resolution).
- **Repetitions.** AI tests run on five photos: the first is discarded (model load) and
  the others averaged. Each time is turned into a rate, and the scores are geometric
  means. Puget quotes about 1-2% run-to-run variance overall and 5% per group.
- **Cold state.** The Camera Raw cache is cleared before each loop. The photos are
  copied to a temporary folder with unique names, so Lightroom treats every run as new
  images.
- **Preferences.** Set by hand rather than "Auto": GPU for display, image processing and
  export on, GPU for preview generation on, and generate previews in parallel.
- **Machine.** English OS and app, 16 GB RAM or more (32 GB recommended), 150 GB free.
- **Interactive tests.** Its 0.9x betas also timed interactive tasks: scrolling images
  in the Library and Develop loupes ("Loupe Scroll"). It dropped its Develop brush-lag
  test as "too inconsistent to be reliable", and 1.x times batch tasks only.

Puget has no Capture One benchmark. Capture One's own community tests use a fixed set of
RAWs and a JPEG export at fixed settings, and take the shortest of several runs, because
disk caching and background processes vary. Reviewers who time interactive latency use
a stopwatch or a screen recording. Display-latency tools (high-speed cameras, Typometer)
time from the input to the first changed frame on screen.

This kit follows Puget for the batch operations (fresh copies with unique names,
cleared caches, pinned GPU preferences, a fixed photo count). For interactive latency
it uses a screen recording with change detection, which works the same way for every
app whether or not it can be scripted.

### Operations

| Operation | Stimulus | Done when | Darkslide | Capture One | Lightroom Classic |
|---|---|---|---|---|---|
| Import N with previews | add the folder in place | every photo has its viewer preview at up to 2560 px | in-app | AppleScript `browse` + proxy files on disk | manual (checklist) |
| Session / catalog open | open | all N listed | in-app | AppleScript `open` + `browse` | manual |
| Select → first preview | → after 2 s on a photo | first changed frame | in-app + screen | screen | screen |
| Select → full-quality render | same | end of the first burst of changes followed by 0.5 s of quiet | in-app + screen | screen | screen |
| Next photo, → held | → every 100 ms, 100 presses | the k-th photo change after press k (matched in order), else **missed** | in-app + screen | screen | screen |
| Exposure change → redraw | exposure up ↔ back (see below) | first changed frame | in-app + screen | screen (AppleScript sets ±0.3 EV) | screen (click on the slider) |
| Export 100 JPEGs | start the export | every file written, queue empty | in-app (ds-export) | AppleScript recipe + `process` | manual |

**"First preview"** is whatever the app shows first: an embedded JPEG or its own cached
preview. **"Full quality"** is the settled render.

**Select.** Selecting means pressing → after dwelling 2 s on the previous photo, so
apps that prefetch neighbours get credit, as they do when you step through a shoot.

**Order.** The viewer steps run in sequence on photos the app hasn't shown yet: 20
selects (photos 1-20), then the held arrow from there (photos 21-120). So the set needs
at least 121 photos; `compare.sh` stages 150 by default.

**Held arrow.** The 100 ms press interval is close to macOS's default key repeat.
Responses are matched to presses in order: a frame where at least a quarter of the
viewer changed is a new photo, and the k-th new photo answers the k-th press. So an
app that keeps up but lags 150 ms shows 150 ms; it is not credited with the 50 ms left
until the next press's window. Presses with no photo of their own (the app coalesced or
dropped them) count as **missed**, and the comparison shows them in every cell. An app
that skips straight to the latest photo is charged the later changes.

> **Unvalidated:** the 25% "new photo" threshold has not been checked on real
> recordings yet. An app that refines a photo in place (embedded preview, then its own
> render; Darkslide does this) can cross it. A later press would then be credited with
> an earlier change, which flatters that app.
>
> As a guard, a held-arrow step that detects more photo changes than presses is marked
> **invalid**: its samples are recorded as `screen-invalid`, never compared, and the log
> says so.
>
> Before any held-arrow numbers are published, validate the threshold on a real
> recording of each app. Step through the frames, check that every detected change is a
> new photo, and adjust `switchFraction` if needed.

**Import "done".** Every app must have the preview its viewer shows for every photo, at
up to 2560 px on the long edge:

| App | Preview |
|---|---|
| Capture One | its proxies at the preview size set in Preferences ▸ Image (2560 px) |
| Lightroom Classic | Standard previews at 2560 px |
| Darkslide | the embedded preview decoded at up to 2560 px, which it shows before developing |

Each app's real preview size is recorded (`previewPx` in the results, `previewSizePx`
in the shareable result). A camera's embedded JPEG is often smaller: about 1616 px on
the Sony A9. When sizes differ, the report shows it.

**Exposure stimulus.** The input path differs by app, and the difference is part of
the number:

| App | Stimulus | Includes |
|---|---|---|
| Capture One | an AppleScript `set exposure … to 0.3` / `0` | Apple Event IPC; no UI path |
| Lightroom Classic | a click on the Exposure slider track | the UI path |
| Darkslide | a click on the Exposure slider track | the UI path |

Calibrate the two click points per machine so they set +0.3 EV and 0. Click once, read
the Exposure field, and move the point until it reads +0.30; do the same for 0.00.
Scripts are compiled before the run, so no sample includes compile time.

Capture One has no exposure shortcut to post as a key. Compare its exposure numbers
with that caveat, or calibrate a click for it too (replace the script stimuli in
`plans/captureone.json` with two `click` points).

### What is compared

Samples timed by different methods are never pooled.

- **Viewer operations** are compared only when timed on screen: key, click or script
  in, pixels out, for every app.
- Darkslide's in-app viewer timings (from the call to the frame being ready to draw, no
  event path, no compositor) go in a separate **Internal timings** table, labelled
  Darkslide-internal and not comparable across apps.
- **Batch operations** (import, open, export) take seconds. They are compared by each
  app's one wall-clock method: Darkslide's in-app clock, Capture One's AppleScript
  polling (about 0.1 s resolution), and Lightroom's stopwatch. The method is named in
  each cell.
- `compare.sh --no-screen` makes no cross-app comparison: with Capture One it refuses
  to run, and alone it reports Darkslide only.

### Equivalent settings

| Setting | Darkslide | Capture One 23 | Lightroom Classic |
|---|---|---|---|
| Preview mode | embedded preview first, then develop | proxies at the preview size in Preferences ▸ Image (set 2560 px) | **Standard** previews for the main run; Embedded & Sidecar as a second tier; 1:1 not used (Puget: Minimal) |
| GPU | always (Metal) | Preferences ▸ General ▸ Hardware Acceleration: Display **Auto**, Processing **Auto** (checked by the script) | Preferences ▸ Performance: Use Graphics Processor **Custom**, display + image processing + export on; GPU for preview generation on; generate previews in parallel |
| Caches | fresh support folder per launch; the session index lives in the copied folder | fresh session copy per run (its cache is inside); global cache size left as is | Camera Raw cache 20 GB, purged before each cold run (Preferences ▸ Performance) |
| Import mode | open the folder in place | browse the folder in place (no copy) | Add (no copy), no Smart Previews, no duplicates check |
| Export | full size, sRGB, JPEG quality 90, no output sharpening, metadata on | recipe: JPEG, quality 90, sRGB Color Space Profile, Fixed 100%, no output sharpening | JPEG, quality 90, sRGB, no resize, no output sharpening |
| Files | APFS clones on the internal SSD, unique names per run | same | same |

**JPEG quality.** All three apps take 0-100 and their encoders differ (mozjpeg in
Darkslide, Apple ImageIO or the vendor's own elsewhere). The kit uses the same nominal
90 everywhere and logs the average output size so the mapping can be checked. If the
sizes differ by more than 15%, pick the quality that matches Darkslide's size and
record it in the report.

### Machine state

The checks run at the start and are recorded in the report. A failing check aborts the
run unless you pass `--force`:

- on AC power, Low Power Mode off;
- the files on the internal SSD;
- Spotlight (`mds`, `mdworker`), `mediaanalysisd`, `photoanalysisd` and Time Machine
  below 5% CPU;
- a 1-minute load average no higher than a quarter of the core count.

Also: quit every other app, don't touch the Mac during the run, and keep the display
awake.

### Cold and warm runs

- **Cold:** a freshly launched app, the OS file cache purged (`sudo purge`), and fresh
  copies of the photos and of the session/support folder, so no app cache knows them.
- **Warm:** a second pass in the same process on another fresh copy, with the OS and
  GPU caches hot. Darkslide runs both passes. Capture One and Lightroom Classic report
  cold runs only (each run relaunches the app).

Without passwordless `sudo`, the file cache can't be purged. The first pass is then
labelled **warm** and the report says so.

**Repetitions.** There are R launches per app (default 3). Batch operations give one
sample per launch. Latency operations give 20 to 100 samples per launch.

The report gives, per app, operation, mode and method:
- the **median**;
- the **nearest-rank p90**, or the **max** when n < 10 (with 3 runs a "p90" would just
  be the worst run);
- min, max and **n**;
- missed presses;
- the time per photo, for batch operations.

Medians resist outliers, which is why they are used rather than means.

### Screen timing

`Darkslide --bench-drive <plan>` drives one app and times its viewer:

1. It finds the app's main window and activates it. Exactly one instance of the app
   may be running, or the pid it launched is given.
2. It records the viewer region with ScreenCaptureKit at up to 120 fps, scaled on the
   GPU to a 192 px wide luma grid.
3. It posts each stimulus (CGEvent key or click, or an AppleScript) and notes the host
   time.

**Safety checks before every stimulus.**
- The frontmost app must be the one under test, or the run stops. A key or click never
  lands in another app, including your own Darkslide.
- A plan's AppleScripts name the app as `{app}`, filled in from `--app-name` (never a
  hard-coded version).
- Before each script, the plan's `documentCheck` must return the bench copy's name
  (`--expect-document`). `run-captureone.sh` gives every run's session copy a unique
  `DarkslideBench-<time>-r<k>` name and checks it before every script it sends. So a
  script never touches your own catalog or session.
- Preflight refuses to start while another Darkslide or any Capture One is open.

Frame timestamps are on the same host clock, so a latency is a frame's presentation
time minus the stimulus time. It includes the event path, the app and the compositor,
but not the display's scan-out (the same for every app).

A frame counts as changed when at least 1% of the grid moved more than 16 luma
levels. That is above scaling noise and a blinking cursor. The resolution is one
compositor frame (8.3 ms at 120 Hz, 16.7 ms at 60 Hz).

Each plan (`bench/plans/*.json`) sets two things:

- the viewer rectangle, as a fraction of the window;
- the stimuli for each step.

Calibrate a plan once per machine and layout. The driver logs the region it records,
which you can check with `screencapture -R<x,y,w,h> region.png`. For the click
stimuli, put the slider's positions in the plan.

Darkslide is timed on screen too, from this checkout's build, launched with a scratch
support folder and driven by pid. Its in-app numbers measure the same operations
without the compositor: from the call to the moment the viewer has the photo's pixels
ready to draw.

### What it doesn't measure

- AI features: Darkslide's are on-device and differ from the others.
- Tethering, printing, catalog search.
- Memory use.
- Battery.

## Sharing results (planned)

A Geekbench-style public results browser is planned, where people upload their camera,
Mac and scores to compare. No upload or server exists yet. The kit already writes the
result in the format the browser would take, and uses the photo set that makes results
from different Macs comparable.

### The result format

`bench/compare.sh` writes `<out>/result.json` (`Darkslide --bench-share`). It is
versioned JSON, defined by `bench/schema/bench-result-v1.schema.json` (JSON Schema
2020-12, no extra fields allowed):

| Field | Contents |
|---|---|
| `schema`, `schemaVersion` | `darkslide-bench-result`, `1`. A breaking change gets a new version and a new schema file. |
| `date` | the day of the run, `YYYY-MM-DD`, no time of day |
| `machine` | Mac model id (`Mac16,8`), chip, CPU cores (performance, efficiency), RAM in GB, macOS version and build |
| `cameras` | make, model, megapixels and the number of distinct files, for each camera in the set |
| `testPack` | pack id (`standard-v1`, or `custom`), its content hash, and whether it is ranked |
| `settings` | photos, runs, whether the file cache was purged, import mode, GPU, export format/size/colour space/JPEG quality/sharpening, select dwell, press interval, settle time |
| `apps[]` | name, version, build, the real preview size its import built; for each operation, mode and timing method (never pooled): whether it is comparable across apps, n, missed, median, p90 (ms), and time per photo for batch operations |

**What it never contains:**
- serial numbers (camera or Mac);
- file names or paths;
- GPS or any other photo metadata beyond make, model and size;
- user, computer or host names;
- photos or thumbnails.

How that is enforced:
- every field is built from a whitelist; nothing is copied from free text such as
  `meta.json`'s notes or preflight lines;
- the Mac is read through `sysctl` model and chip keys only;
- the camera fields come from each file's make, model and pixel size (the rest of the
  metadata, including the body serial, is dropped);
- before writing, `BenchShare.privacyProblems` checks every string. It looks for paths,
  file extensions, e-mail addresses, the user's login and full name, the home folder
  and the host name, and over-long strings. A result that fails is not written.

### The standard test pack

`bench/packs/standard-v1.json` pins six full-resolution RAWs from six cameras: Nikon
Z 6II and Z 7II, Pentax K-1 Mark II, and Sony A7 III, A7R IV and A1. They range from 24
to 61 MP, about 290 MB in all.

v1 holds only the formats Darkslide previews today: Sony ARW, Nikon NEF and Pentax PEF.
The kit found that Darkslide can't yet extract an embedded preview from these formats,
so their import step fails:

| Format | Example file |
|---|---|
| Canon CR3 | EOS R5 |
| Fujifilm RAF | X-T4 |
| Olympus ORF | E-M1 Mark III |
| Panasonic RW2 | S1R |
| Leica DNG | M10-R |

Those cameras join in `standard-v2` once Darkslide reads them.

- **Source and licence.** All come from raw.pixls.us, and each file's licence was
  checked in the site's repository listing: CC0 1.0 for every one. Capture One 23 and
  Lightroom Classic read all of them.
- **Fetching.** `bench/fetch-pack.sh` downloads them by URL into
  `~/Library/Caches/darkslide-bench/<id>`. It verifies each file against its pinned
  BLAKE3 hash (with `b3sum`) or its pinned SHA-256 (with `shasum`), and refuses a
  mismatch. The files are never committed.
- **Size of a run.** A run needs at least 121 photos, so the pack is repeated to
  `--count` (150) as APFS clones with unique names. Each camera gets an equal share.
- **Pack hash.** The pack hash is the SHA-256 over the sorted SHA-256 digests of the
  distinct files. It doesn't depend on names, order or the repeats.
  `Darkslide --bench-share` recomputes it from the files that were actually used. Only
  when it matches the manifest is the result tagged with the pack's id.
- **Ranking.** A result is **ranked** only when it ran the verified standard pack with
  at least `minimumPhotos`.

Run it with `bench/compare.sh --pack standard`, which means the current standard pack
(`standard-v1`; `--pack` alone or `--pack <id>` work too). It downloads into the cache,
verifies the files and runs the kit. A new pack, or a
changed file, gets a new id (`standard-v2`): results are only compared within one pack.

**Your own photos.** `bench/compare.sh --raws <folder>` gives a result with
`testPack.id = "custom"`. The hash still identifies the set, so you can compare your
own runs over time, but it is never ranked against other machines.

Note that a custom hash is a stable fingerprint of your own photo set. Results with
the same hash are linkable to each other, and to anyone who has the same files. Leave
it out (or don't share custom results) if that matters.

### Upload and leaderboard (planned, not built)

**Opt-in only.**
- Nothing is sent unless the user asks.
- A future `bench/compare.sh --upload`, or a button in Darkslide's benchmark view, would
  first show the exact `result.json` and ask.
- The upload carries that file and nothing else: no account, no device identifier.
- The service keeps the submission time and a random id that can be used to delete it.

**Checks on arrival.**
- The schema is validated.
- The privacy check runs again on the server.
- The pack hash must match a published pack for the result to be ranked.
- Results from unreleased app builds are kept but not ranked.

**Leaderboard.**
- One board per test pack and operation (and mode: cold, warm), grouped by Mac model
  id and chip, with filters by app, app version, macOS and camera.
- Ranked by the **median** time (lower is better), on `comparable` entries only; p90 and
  n are shown beside it.
- Results with fewer than 3 runs, or with missed presses above 10%, are listed but not
  ranked.
- Each Mac model's score is the median of its submissions.

**Outliers.**
- A submission is flagged, and kept out of the ranking, when its median is more than
  3 MAD (median absolute deviation) from its Mac model's median for that operation.
- It is also flagged when its runs disagree, with a p90/median ratio above 3.
- Flagged results stay visible and marked, so a misconfigured machine (battery, Low
  Power Mode, Spotlight indexing) is explained, not hidden.

**Custom results.** Results with `testPack.id = "custom"` can be kept privately to
compare one's own runs, and are never on a public board.

## Results

No results are in the repo. Run `bench/compare.sh` (bench/README.md) and keep the
`report.md` it writes together with the `results/` folder.
