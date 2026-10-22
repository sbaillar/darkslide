import AppKit
import DarkslideCore
import DarkslideUI
import SwiftUI

/// `Darkslide --snapshot <out.png>`: stage a representative state, capture the
/// main window in-process, write a PNG, and quit.
///
/// In-process capture needs no Screen Recording permission (agent shells and CI
/// don't have it). `cacheDisplay` can't read a CAMetalLayer, so in this mode the
/// viewer draws the same Rust-rendered frame with SwiftUI (`SnapshotImageLayer`).
@MainActor
enum WindowSnapshot {
    static func run(model: AppModel, path: String) async {
        // L9: a snapshot that never finishes (two runs produced no PNG in 150 s) now names
        // the step it was stuck in and exits instead of hanging silently.
        SnapshotWatchdog.step("window appeared")
        stageWindow(model) // L9: `--appearance light|dark`, `--window WxH`
        // Let the session load, the first render land, and thumbnails fill in.
        SnapshotWatchdog.step("first frame")
        _ = await waitFor(seconds: 10) { model.viewer.displayFrame != nil }
        // `--no-stage`: show what the store holds (e.g. ratings persisted by a previous run).
        if let i = CommandLine.arguments.firstIndex(of: "--open"), i + 1 < CommandLine.arguments.count {
            // Stage on the folder being opened, not on the session restored from recents.
            let want = URL(fileURLWithPath: CommandLine.arguments[i + 1]).lastPathComponent
            _ = await waitFor(seconds: 30) {
                model.currentSession?.url.lastPathComponent == want && !model.isLoading && model.viewer.displayFrame != nil
            }
        }
        if !CommandLine.arguments.contains("--no-stage") { stage(model) }
        // `--tab <id>`: which tool tab to show (e.g. `library` for Smart Folders).
        let args = CommandLine.arguments
        if let i = args.firstIndex(of: "--tab"), i + 1 < args.count, let tab = ToolTab(rawValue: args[i + 1]) {
            model.toolTab = tab
        }
        // `--grid`: the grid in place of the viewer.
        if args.contains("--grid") { model.browserMode = .grid }
        SnapshotWatchdog.step("stage edit")
        await stageEdit(model) // [vw8] `--stage-edit`, `--zoom-actual`: a live develop, 1:1
        await showMissing(model) // `--show-missing`: the Missing filter, a missing photo's banner
        SnapshotWatchdog.step("wb loupe")
        await wbLoupe(model) // `--wb-loupe [x,y]`: the WB eyedropper's loupe over the photo
        SnapshotWatchdog.step("archive popover")
        await archivePopover(model) // [arc6] `--archive-popover [--archive-target <dir>]`
        SnapshotWatchdog.step("import card")
        await importCard(model) // [imp6] `--import-card <volume>`: the import sheet mid-import
        SnapshotWatchdog.step("export sheet")
        await exportSheet(model) // `--export-sheet`: the Export sheet on the whole session
        await instruction(model) // [nl9] `--nl "<text>"`: the ⌘K bar applies it; the toast stays up
        await aiChanges(model) // `--ai-changes`: Auto Adjust's outlined tools and the AI Changes list
        await cropDrag(model) // [cmp9] `--crop-drag`: the crop tool mid-drag
        await layersSky(model) // [lay9] `--layers-sky [photo]`: the Layers panel with an AI sky mask
        survey(model) // [cul9] `--survey [n]`: N-up survey of the first n photos, one eliminated
        multiView(model) // [multiview] `--select n`, `--filmstrip-rows r`, `--filmstrip-position left|right`
        customTools(model) // `--tools-custom`: a custom tab and the All Tools tab in the strip
        // A running import keeps moving: capture it soon after it's part-way. [imp6]
        try? await Task.sleep(for: .seconds(CommandLine.arguments.contains("--import-card") ? 0.6 : 2.5))
        SnapshotWatchdog.step("capture")
        let ok = if let pane = argument("--settings") {
            await captureSettings(pane, model: model, to: URL(fileURLWithPath: path))
        } else {
            capture(to: URL(fileURLWithPath: path), model: model)
        }
        SnapshotWatchdog.step("flush")
        await model.saver.flush() // staged ratings/edits reach the sidecars before exit
        let rated = model.photos.filter { $0.rating > 0 }.map { "\($0.filename)=\($0.rating)" }
        print("snapshot: rated \(rated.sorted())")
        print(ok ? "snapshot: wrote \(path)" : "snapshot: FAILED")
        exit(ok ? 0 : 1)
    }

    /// `--tools-custom`: a "Portraits" tab (a tool shared with Favorites, one moved
    /// in), the All Tools tab shown, Portraits selected. Snapshot preferences are private.
    private static func customTools(_ model: AppModel) {
        guard CommandLine.arguments.contains("--tools-custom") else { return }
        let layout = ToolLayout.shared
        let tab = ToolWorkspaceCommands.newTab(title: "Portraits", symbol: "person.crop.square", model: model, layout: layout)
        for p: ToolPanelID in [.whiteBalance, .exposure, .curve, .sharpening] { layout.edit { $0.insert(p, in: tab) } }
        layout.edit { $0.toggleAllToolsTab() }
        model.toolTab = tab
    }

    /// `--ai-changes`: Auto Adjust on the focused photo, then the AI Changes list open over
    /// the viewer and the sidebar on the first changed tool (its rows outlined in yellow).
    private static func aiChanges(_ model: AppModel) async {
        guard CommandLine.arguments.contains("--ai-changes") else { return }
        await model.autoAdjust()
        ToolWorkspaceCommands.revealAIChanges(model: model, layout: .shared)
        model.aiChangesShown = true
        print("snapshot: AI changes \(model.focusedAIChangeRows.map { "\($0.setting) \($0.detail)" })")
    }

    /// [cul9] `--survey [n]`: select the first n visible photos (default 6, 4–6 shown
    /// after one is eliminated with X), enter survey mode and take one out.
    private static func survey(_ model: AppModel) {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: "--survey") else { return }
        let n = i + 1 < args.count ? Int(args[i + 1]) ?? 6 : 6
        model.selection = Set(model.visibleIDs.prefix(max(n, 2)))
        model.beginSurvey()
        _ = model.handleKey(KeyInput("x"), focus: .none, sheetOpen: false, isMainWindow: true)
    }

    /// [multiview] `--select n`: the first n visible photos selected, the second focused
    /// (multi-view); `--filmstrip-position bottom|left|right`; `--filmstrip-rows r`: the
    /// filmstrip dragged to r rows (columns at the side).
    private static func multiView(_ model: AppModel) {
        if let p = argument("--filmstrip-position").flatMap(FilmstripPosition.init(rawValue:)) { model.placeFilmstrip(p) }
        if let r = argument("--filmstrip-rows").flatMap(Int.init) { model.resizeFilmstrip(to: r, ended: true) }
        guard let n = argument("--select").flatMap(Int.init), n > 0 else { return }
        let ids = Array(model.visibleIDs.prefix(n))
        guard let first = ids.first else { return }
        model.focus(first, extend: false)
        model.selection = Set(ids)
        if ids.count > 1 { model.focusWithinSelection(ids[1]) }
    }

    /// [cmp9] `--crop-drag`: 4:5 on the focused photo, the crop tool, and a corner drag
    /// held part-way (handles, thirds and the dimmed outside visible), not committed.
    private static func cropDrag(_ model: AppModel) async {
        guard CommandLine.arguments.contains("--crop-drag") else { return }
        model.toolTab = .composition
        model.setCropAspect(.r4x5)
        model.perform(.tool(.crop))
        _ = await waitFor(seconds: 30) { model.viewer.layout.main != nil && model.viewer.displayFrame != nil }
        guard let img = model.viewer.layout.main?.image else { return }
        let box = model.viewer.cropRect
        let corner = CGPoint(x: img.minX + box.maxX * img.width, y: img.minY + box.maxY * img.height)
        model.viewer.beginCanvasDrag(at: corner)
        model.viewer.updateCanvasDrag(to: CGPoint(x: corner.x - img.width * box.width * 0.25,
                                                  y: corner.y - img.height * box.height * 0.25))
        print("snapshot: crop drag \(model.viewer.cropDragPreview.map { "\($0)" } ?? "none")")
    }

    /// The value after `flag` on the command line.
    static func argument(_ flag: String, in args: [String] = CommandLine.arguments) -> String? {
        guard let i = args.firstIndex(of: flag), i + 1 < args.count else { return nil }
        return args[i + 1]
    }

    /// `"1280x800"` → 1280×800 points (nil when malformed or too small for the window minimum).
    static func windowSize(_ text: String) -> CGSize? {
        let parts = text.lowercased().split(separator: "x").compactMap { Double($0) }
        guard parts.count == 2, parts[0] >= 800, parts[1] >= 500 else { return nil }
        return CGSize(width: parts[0], height: parts[1])
    }

    /// `--appearance light|dark` and `--window WxH` (L9): stage the look and the size;
    /// `--layout` and `--keys` (darkslide, lightroomClassic, captureOne): a familiar layout.
    private static func stageWindow(_ model: AppModel) {
        if let l = argument("--layout").flatMap(LayoutPreset.init(rawValue:)) { model.settings.layoutPreset = l }
        if let k = argument("--keys").flatMap(KeyPreset.init(rawValue:)) { model.settings.useKeyPreset(k) }
        if let a = argument("--appearance")?.lowercased(), a == "light" || a == "dark" {
            model.settings.appearance = a == "light" ? .light : .dark
            NSApp.appearance = NSAppearance(named: a == "light" ? .aqua : .darkAqua)
        }
        if let size = argument("--window").flatMap(windowSize),
           let window = NSApp.windows.first(where: { $0.isVisible && $0.identifier?.rawValue.contains("main") == true })
            ?? NSApp.mainWindow {
            window.setContentSize(size)
            window.center()
        }
    }

    /// `--stage-edit`: develop the focused photo live through ds-pipeline with a warm
    /// white balance, +exposure, a crop and a straighten, and show the histogram;
    /// `--zoom-actual`: then zoom to 1:1 at the image center once the Full region lands.
    private static func stageEdit(_ model: AppModel) async {
        let args = CommandLine.arguments
        guard args.contains("--stage-edit") || args.contains("--zoom-actual") else { return }
        let v = model.viewer
        if args.contains("--stage-edit") {
            model.updateEdit(label: "Snapshot") { e in
                e.whiteBalance.asShot = false
                e.whiteBalance.temperature = 7400
                e.whiteBalance.tint = 8
                e.exposure.ev = 0.6
                e.exposure.contrast = 12
                e.hdr.shadows = 25
                e.crop.rect = CGRect(x: 0.07, y: 0.05, width: 0.86, height: 0.86)
                e.crop.angle = -3
            }
            model.endEditGesture()
            model.toolTab = .exposure
        }
        let old = v.frame
        _ = await waitFor(seconds: 60) {
            v.frame != nil && (v.frame != old || !args.contains("--stage-edit")) && v.frame?.mapping != nil
                && v.renderedQuality == .preview
        }
        if args.contains("--zoom-actual"), let pane = v.layout.main {
            v.cursor = CGPoint(x: pane.image.midX, y: pane.image.midY)
            v.zoom(to: .actual, anchor: v.cursor)
            _ = await waitFor(seconds: 60) { v.tile != nil }
        }
        let f = v.frame
        print("snapshot: develop \(f.map { "\($0.width)x\($0.height)" } ?? "none") \(DevelopRenderer.backend.name); "
              + "zoom \(v.zoomLabel); tile \(v.tile.map { "\($0.frame.width)x\($0.frame.height) @\($0.scale)" } ?? "none")")
    }

    /// `--show-missing`: turn the Missing filter on and focus the first missing photo,
    /// so its banner shows (photos whose originals are gone are hidden otherwise).
    private static func showMissing(_ model: AppModel) async {
        guard CommandLine.arguments.contains("--show-missing") else { return }
        model.filter.missingOnly = true
        _ = await waitFor(seconds: 10) { !model.visibleIDs.isEmpty }
        model.focus(model.visibleIDs.first, extend: false)
        _ = await waitFor(seconds: 10) { model.viewer.problem != nil }
        print("snapshot: problem \(model.viewer.problem.map { $0.message } ?? "none")")
    }

    /// `--wb-loupe [x,y]`: the white-balance eyedropper on, the cursor at (x, y) of the
    /// image (fractions, default 0.5,0.4), once its loupe and readout are in.
    private static func wbLoupe(_ model: AppModel) async {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: "--wb-loupe") else { return }
        let xy = (i + 1 < args.count ? args[i + 1] : "").split(separator: ",").compactMap { Double($0) }
        let (fx, fy) = xy.count == 2 ? (xy[0], xy[1]) : (0.5, 0.4)
        let v = model.viewer
        model.toolTab = .color
        model.wbPickerActive = true
        _ = await waitFor(seconds: 60) { v.frame?.mapping != nil && v.renderedQuality == .preview && v.layout.main != nil }
        guard let img = v.layout.main?.image else { return }
        v.hover(at: CGPoint(x: img.minX + img.width * fx, y: img.minY + img.height * fy))
        _ = await waitFor(seconds: 60) { v.pickerLoupe.region != nil && v.pickerLoupe.readout?.whiteBalance != nil }
        print("snapshot: loupe \(v.pickerLoupe.readout.map { PickerLoupe.spoken($0, kind: .whiteBalance) } ?? "none")")
    }

    /// `--archive-popover`: archive the open session (to `--archive-target <dir>`
    /// when given) and show the toolbar popover once a run finished, so the
    /// capture shows real status.
    private static func archivePopover(_ model: AppModel) async {
        let args = CommandLine.arguments
        guard args.contains("--archive-popover") else { return }
        if let i = args.firstIndex(of: "--archive-target"), i + 1 < args.count {
            model.settings.archiveTarget = args[i + 1]
            model.settings.archiveEnabled = true
        }
        // `--amazon-folder <dir>`: also hand the originals to a (temp) Amazon Photos folder.
        if let i = args.firstIndex(of: "--amazon-folder"), i + 1 < args.count {
            model.settings.amazonPhotosFolder = URL(fileURLWithPath: args[i + 1])
        }
        try? await Task.sleep(for: .seconds(1.2)) // the new settings reach the service
        _ = await waitFor(seconds: 30) { model.openProgress == nil } // originals verified (hashed)
        if let s = model.currentSession {
            await model.archive.sessionOpened(s)
            await model.archive.afterImport(session: s, files: model.photos.count) // archive now (+ Amazon)
        }
        let end = Date().addingTimeInterval(90)
        var nudged = Date()
        while Date() < end, !(model.archiveStatus.detail?.runs.contains { $0.outcome != "busy" } ?? false) {
            if Date().timeIntervalSince(nudged) > 10 { // e.g. the target check was still running
                await model.archive.syncNow()
                nudged = Date()
            }
            try? await Task.sleep(for: .milliseconds(250))
        }
        try? await Task.sleep(for: .seconds(1.2)) // the status stream catches up
        model.archivePopoverShown = true
        print("snapshot: archive \(ArchiveDisplay.title(model.archiveStatus.state)); runs \(model.archiveStatus.detail?.runs.map(ArchiveDisplay.runLine) ?? [])")
    }

    /// Draw open popovers (separate windows) over the main window capture; the
    /// canvas grows when a popover sticks out of the window.
    private static func compositePopovers(over window: NSWindow, into rep: NSBitmapImageRep) -> NSBitmapImageRep {
        let popovers = NSApp.windows.filter {
            $0 !== window && $0.isVisible && String(describing: type(of: $0)).contains("Popover")
        }
        guard !popovers.isEmpty else { return rep }
        let union = popovers.reduce(window.frame) { $0.union($1.frame) }
        let scale = CGFloat(rep.pixelsWide) / max(window.frame.width, 1)
        guard let out = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(union.width * scale),
                                         pixelsHigh: Int(union.height * scale), bitsPerSample: 8, samplesPerPixel: 4,
                                         hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0,
                                         bitsPerPixel: 0),
              let ctx = NSGraphicsContext(bitmapImageRep: out) else { return rep }
        out.size = union.size
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ctx
        NSColor.windowBackgroundColor.setFill()
        NSRect(origin: .zero, size: union.size).fill()
        let place = { (f: NSRect) in NSRect(x: f.minX - union.minX, y: f.minY - union.minY, width: f.width, height: f.height) }
        rep.draw(in: place(window.frame), from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: false, hints: nil)
        for p in popovers {
            guard let v = p.contentView, let img = v.bitmapImageRepForCachingDisplay(in: v.bounds) else { continue }
            v.cacheDisplay(in: v.bounds, to: img)
            let origin = v.convert(NSPoint.zero, to: nil) // content view inside the popover window
            let r = NSRect(x: p.frame.minX + origin.x, y: p.frame.minY + (v.isFlipped ? origin.y - v.bounds.height : origin.y),
                           width: v.bounds.width, height: v.bounds.height)
            let frame = NSBezierPath(roundedRect: place(r), xRadius: 12, yRadius: 12)
            NSColor.windowBackgroundColor.setFill()
            frame.fill()
            NSColor.separatorColor.setStroke()
            frame.stroke()
            img.draw(in: place(r), from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: false, hints: nil)
        }
        NSGraphicsContext.restoreGraphicsState()
        return out
    }

    /// `--layers-sky [name]`: focus the photo whose name starts with `name` (default: the one
    /// in focus), add an AI Sky mask on a new layer that darkens and dehazes the sky, and
    /// show the Layers panel.
    private static func layersSky(_ model: AppModel) async {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: "--layers-sky") else { return }
        if i + 1 < args.count, !args[i + 1].hasPrefix("--") {
            let name = args[i + 1]
            _ = await waitFor(seconds: 30) { model.photos.contains { $0.filename.hasPrefix(name) } }
            if let p = model.photos.first(where: { $0.filename.hasPrefix(name) }) { model.focus(p.id, extend: false) }
        }
        model.toolTab = .layers
        _ = await waitFor(seconds: 30) { model.viewer.displayFrame != nil }
        let layers = LayerController(app: model)
        do {
            try await layers.addAIMask(.sky)
            if let id = layers.selected {
                layers.update(id, label: "Sky") { l in
                    l.exposureEV = -0.7
                    l.tools.presence.dehaze = 30
                    l.tools.saturation = 20
                }
            }
            let mask = model.focusedID.flatMap { model.viewer.masks.mask(photo: $0, target: .sky) }
            print("snapshot: sky mask \(mask.map { "\($0.width)x\($0.height) coverage \($0.coverage)" } ?? "none")")
        } catch {
            print("snapshot: sky mask failed: \(error.localizedDescription)")
        }
        _ = await waitFor(seconds: 60) { model.viewer.renderedQuality == .preview }
    }

    private static func waitFor(seconds: Double, _ cond: () -> Bool) async -> Bool {
        let end = Date().addingTimeInterval(seconds)
        while !cond(), Date() < end { try? await Task.sleep(for: .milliseconds(50)) }
        return cond()
    }

    /// A culling session in progress: a few rated/tagged/picked frames, an edit on the focused one.
    private static func stage(_ m: AppModel) {
        let ids = Array(m.visibleIDs.prefix(14))
        guard ids.count >= 7 else { return }
        let advance = m.settings.autoAdvance
        m.settings.autoAdvance = false
        defer { m.settings.autoAdvance = advance }
        m.focus(ids[1], extend: false)
        m.setPick(.picked)
        m.setRating(4)
        m.focus(ids[3], extend: false)
        m.setPick(.rejected)
        m.focus(ids[6], extend: false)
        m.setTag(.green)
        m.setRating(5)
        m.focus(ids[4], extend: false)
        m.setRating(3)
        m.updateEdit(label: "Snapshot") { e in
            e.exposure.ev = 0.35
            e.exposure.contrast = 12
            e.exposure.saturation = 18
            e.hdr.shadows = 24
            e.hdr.highlights = -30
            e.whiteBalance.asShot = false
            e.whiteBalance.temperature = 6100
            e.whiteBalance.tint = 4
            e.curve.points[.rgb] = [CurvePoint(0.25, 0.21), CurvePoint(0.75, 0.80)]
        }
        m.toolTab = .exposure
    }

    /// `--export-sheet`: select every photo and open the Export sheet (recipes as saved,
    /// "Full-size JPEG" first), once its destination is resolved.
    /// `--nl "<text>"`: open the ⌘K bar, type the text, apply it; the result toast is
    /// kept up for the capture.
    private static func instruction(_ model: AppModel) async {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: "--nl"), i + 1 < args.count else { return }
        model.toastDuration = .seconds(60)
        model.commandBar.present()
        model.commandBar.text = args[i + 1]
        try? await Task.sleep(for: .milliseconds(400))
        let toast = await model.commandBar.submit(to: model)
        _ = await waitFor(seconds: 20) { model.viewer.renderedQuality == .preview }
        // Show the bar with the phrase it just applied, next to the result toast.
        model.commandBar.present()
        model.commandBar.text = args[i + 1]
        print("snapshot: nl \(toast?.message ?? "no result")")
    }

    private static func exportSheet(_ model: AppModel) async {
        guard CommandLine.arguments.contains("--export-sheet") else { return }
        model.selection = Set(model.visibleIDs)
        model.beginExport()
        _ = await waitFor(seconds: 10) { model.exportModel?.canStart == true }
        print("snapshot: export \(model.exportModel?.recipes.map(\.name) ?? [])")
    }

    /// `--import-card <volume>`: open the import sheet on that card, start the import,
    /// and wait until it is part-way (`DS_SNAPSHOT_IMPORT_AT`, default 0.35 of the bytes;
    /// slow it down with `DS_IMPORT_THROTTLE_MS`) so the capture shows a live import.
    private static func importCard(_ model: AppModel) async {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: "--import-card"), i + 1 < args.count else { return }
        let url = URL(fileURLWithPath: args[i + 1])
        model.sheet = .importCard(url)
        var found: ImportModel?
        _ = await waitFor(seconds: 60) {
            found = ImportRegistry.shared.live.first {
                $0.volume?.standardizedFileURL == url.standardizedFileURL && $0.phase == .ready
            }
            return found != nil
        }
        guard let importer = found else {
            print("snapshot: import sheet never became ready")
            return
        }
        importer.sessionText = "Beach Trip"
        importer.start()
        let at = Double(ProcessInfo.processInfo.environment["DS_SNAPSHOT_IMPORT_AT"] ?? "") ?? 0.35
        _ = await waitFor(seconds: 120) { (importer.progress?.fraction ?? 0) >= at || importer.phase != .importing }
        print("snapshot: import at \(Int((importer.progress?.fraction ?? 0) * 100))% (\(importer.phase))")
    }

    /// Draw an attached sheet (the import sheet) over the window, where it sits.
    private static func compositeSheet(of window: NSWindow, into rep: NSBitmapImageRep) {
        guard let sheet = window.attachedSheet, let content = sheet.contentView else { return }
        let v = content.superview ?? content
        guard let srep = v.bitmapImageRepForCachingDisplay(in: v.bounds) else { return }
        v.cacheDisplay(in: v.bounds, to: srep)
        guard let cg = srep.cgImage, let ctx = NSGraphicsContext(bitmapImageRep: rep) else { return }
        let origin = CGPoint(x: sheet.frame.minX - window.frame.minX, y: sheet.frame.minY - window.frame.minY)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ctx
        ctx.cgContext.setFillColor(CGColor(gray: 0, alpha: 0.25))
        ctx.cgContext.fill(CGRect(origin: .zero, size: window.frame.size))
        ctx.cgContext.draw(cg, in: CGRect(origin: origin, size: sheet.frame.size))
        NSGraphicsContext.restoreGraphicsState()
    }

    /// `--settings general|archive|devices|plugins`: that Settings pane, hosted in a window of
    /// its own and captured in place of the main window (the Settings scene can't be opened
    /// from here).
    private static func captureSettings(_ pane: String, model: AppModel, to url: URL) async -> Bool {
        let root: AnyView? = switch pane {
        case "general": AnyView(SettingsView(settings: model.settings))
        case "archive": AnyView(ArchiveSettingsView(model: ArchiveSettingsModel(settings: model.settings)))
        case "devices": AnyView(DevicesSettingsView(model: model.remote))
        case "plugins": AnyView(PluginsSettingsView(store: PluginStore.shared))
        default: nil
        }
        guard let root else { return false }
        let host = NSHostingView(rootView: root.frame(width: 560).preferredColorScheme(model.settings.appearance.colorScheme))
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: host.fittingSize), styleMask: [.titled],
                              backing: .buffered, defer: false)
        window.title = pane.capitalized
        window.contentView = host
        window.center()
        window.orderFront(nil)
        try? await Task.sleep(for: .seconds(1.5))
        window.setContentSize(host.fittingSize)
        try? await Task.sleep(for: .milliseconds(300))
        guard let view = host.superview,
              let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return false }
        view.cacheDisplay(in: view.bounds, to: rep)
        guard let png = rep.representation(using: .png, properties: [:]) else { return false }
        return (try? png.write(to: url)) != nil
    }

    private static func capture(to url: URL, model: AppModel) -> Bool {
        guard let window = NSApp.windows.first(where: { $0.isVisible && $0.identifier?.rawValue.contains("main") == true })
            ?? NSApp.mainWindow ?? NSApp.windows.first(where: \.isVisible),
            let content = window.contentView else { return false }
        let view = content.superview ?? content // include the toolbar/title bar
        guard let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return false }
        view.cacheDisplay(in: view.bounds, to: rep)
        // The viewer's frame is drawn by `SnapshotFrameLayer` (under the overlays).
        compositeSheet(of: window, into: rep) // [imp6]
        let final = compositePopovers(over: window, into: rep) // [arc6] last: it may grow the canvas
        guard let png = final.representation(using: .png, properties: [:]) else { return false }
        return (try? png.write(to: url)) != nil
    }
}

/// Exits a snapshot run that doesn't finish in time, naming the step it was in (L9).
/// Runs on its own thread, so it fires even when the main thread is blocked.
enum SnapshotWatchdog {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var current = "launch (the main window never appeared)"

    nonisolated(unsafe) private static var started = false

    static func step(_ name: String) { lock.withLock { current = name } }
    static var currentStep: String { lock.withLock { current } }

    static func start(seconds: Double) {
        guard lock.withLock({ () -> Bool in defer { started = true }; return !started }) else { return }
        DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + seconds) {
            print("snapshot: TIMED OUT after \(Int(seconds)) s in step “\(currentStep)”")
            exit(3)
        }
    }
}
