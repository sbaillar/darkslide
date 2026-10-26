# Lightroom Classic: manual run

Lightroom Classic's scripting (Lua plug-ins) can't be driven from outside the app, so
this run is manual for import, open and export. The viewer operations use the same
screen driver as the other apps. Settings: docs/BENCHMARKS.md, Equivalent settings.

## Once

- [ ] Preferences ▸ Performance:
  - Use Graphics Processor **Custom**, with display, image processing and export on.
  - Use GPU for preview generation: on.
  - Generate previews in parallel: on.
  - Camera Raw cache: 20 GB.
- [ ] Catalog Settings ▸ File Handling: Standard Preview Size **2560 px**, quality High.
- [ ] Create an empty catalog, `Bench.lrcat`, in a scratch folder.
- [ ] Export preset "Luxtable Bench":
  - JPEG, quality 90, sRGB;
  - no resize, no output sharpening;
  - metadata: All;
  - export to a scratch folder.
- [ ] Calibrate `bench/plans/lightroom.json`:
  - the viewer rectangle, as a fraction of the window;
  - the two exposure-slider click points (Develop ▸ Basic).
- [ ] Give the terminal Screen Recording and Accessibility.

## Each run (3 runs; machine-state checks as in docs/BENCHMARKS.md)

1. [ ] Quit Lightroom. Run `sudo purge`. Make a fresh copy of the staged RAWs with unique
   names:
   `bench/stage.sh <staged-raws> <scratch>/lr-raws-r<k>`
2. [ ] Purge the Camera Raw cache (Preferences ▸ Performance ▸ Purge Cache). Open a fresh
   copy of the empty catalog.
3. [ ] **Import:** File ▸ Import, the copied folder, **Add**, Build Previews: Standard, no
   Smart Previews, Don't import suspected duplicates off. Start a stopwatch when you
   click Import. Stop it when the activity bar's "Building Standard Previews" ends.
4. [ ] **Open:** quit, relaunch, open the catalog. Time from the double-click to the grid
   showing every photo.
5. [ ] **Viewer:** select the first photo in Develop (D), then run:
   `LT_SUPPORT_ROOT=$(mktemp -d) dist/Luxtable.app/Contents/MacOS/Luxtable --bench-drive bench/plans/lightroom.json --out <out>/results/lightroom-screen-r<k>.csv --run <k> --mode cold`
   (Lightroom must be frontmost and the only Lightroom running: the driver stops otherwise.)
6. [ ] **Export:** select all (⌘A), File ▸ Export with Preset ▸ Luxtable Bench. Time from
   the click to the progress bar finishing.

Add the stopwatch times to `<out>/results/lightroom-manual.csv`, one row each (ms):

```
app,operation,mode,run,sample,ms,items,method,previewPx
Lightroom Classic,import,cold,1,1,84210,100,manual,2560
Lightroom Classic,open,cold,1,1,4120,100,manual,
Lightroom Classic,export,cold,1,1,61500,100,manual,
```

Then set Lightroom Classic's `"ran"` to `true` in `results/meta.json` and run
`--bench-report` again. Stopwatch times are good to about ±0.3 s, so use them only
for operations that take several seconds.

**Status:** not run. Lightroom Classic is not installed on the machine the kit was
built on.
