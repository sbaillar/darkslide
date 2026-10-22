import DarkslideAI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Journeys 1, 3 (open in place), 5 (keyboard culling), 7 (AI).
@MainActor @Suite(.serialized, .enabled(if: FJ.configured, "SKIP: set FJ_ROOT (scratch) and DS_SUPPORT_ROOT to run the final-journeys scenarios")) struct FJMiscTests {
    // MARK: 1. First run

    @Test func firstRunOnboarding() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("firstrun")
        try? fm.removeItem(at: root)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        let d = FJ.defaults("firstrun-\(UUID().uuidString.prefix(6))")
        let settings = AppSettings(defaults: d)
        FJ.log("firstrun: defaults sessionsFolder=\(settings.sessionsFolder.path) archive='\(settings.archiveTarget)' enabled=\(settings.archiveEnabled) appearance=\(settings.appearance) autoAdvance=\(settings.autoAdvance)")
        let m = AppModel.make(mode: .live, settings: settings, defaults: d)
        m.watchCards = false
        FJ.log("firstrun: sheet at launch=\(String(describing: m.sheet))")
        // Share present vs absent (simulated with the injected fileExists).
        for present in [true, false] {
            let ob = OnboardingModel(settings: settings, fileExists: { p in p == "/Volumes/pics" ? present : FileManager.default.fileExists(atPath: p) },
                                     createDirectory: { _ in })
            await ob.suggestArchiveTarget()
            FJ.log("firstrun: share present=\(present) → target='\(ob.archiveTarget)' status=\(ob.archiveStatus) canFinish=\(ob.canFinish)")
        }
        // A temp dir as target; a typo'd target; a /Volumes target not mounted.
        let ob = OnboardingModel(settings: settings, createDirectory: { try fm.createDirectory(at: $0, withIntermediateDirectories: true) })
        for t in [root.appendingPathComponent("nas/Darkslide").path, "/Volumes/NotThere/Darkslide", "relative/path", "~/Archive", "/"] {
            ob.archiveTarget = t
            await ob.refreshArchiveStatus()
            FJ.log("firstrun: target '\(t)' → \(ob.archiveStatus) canFinish=\(ob.canFinish)")
        }
        try fm.createDirectory(at: root.appendingPathComponent("nas"), withIntermediateDirectories: true)
        ob.archiveTarget = root.appendingPathComponent("nas/Darkslide").path
        await ob.refreshArchiveStatus()
        ob.sessionsFolder = root.appendingPathComponent("My Sessions")
        ob.amazonFolder = root.appendingPathComponent("Amazon")
        try ob.finish()
        FJ.log("firstrun: after finish sessions=\(settings.sessionsFolder.path) exists=\(fm.fileExists(atPath: settings.sessionsFolder.path)) archive='\(settings.archiveTarget)' amazon=\(settings.amazonPhotosFolder?.path ?? "nil") amazonBookmark=\(settings.amazonPhotosFolderBookmark != nil) amazonExists=\(fm.fileExists(atPath: root.appendingPathComponent("Amazon").path))")
        await m.start()
        FJ.log("firstrun: empty state sessions=\(m.sessionList.count) current=\(m.currentSession?.name ?? "nil") archive='\(ArchiveDisplay.title(m.archiveStatus.state))'/'\(ArchiveDisplay.long(m.archiveStatus.state))' import menu='\(CommandCatalog.title(.importCard, model: m))' cardNote='\(ImportWording.cardPanelNote(verifies: true, live: true))'")
        let asm = ArchiveSettingsModel(settings: settings)
        try? await Task.sleep(for: .seconds(2))
        FJ.log("firstrun: archive settings target='\(asm.targetText)' amazon='\(asm.amazonText)'")
        await m.shutdownArchive()
        m.stop()
    }

    // MARK: 3. Open a Capture One-style session in place

    @Test func openCaptureOneSessionInPlace() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("c1")
        try? fm.removeItem(at: root)
        let c1 = root.appendingPathComponent("2026-08-06 Beach Trip")
        let capture = c1.appendingPathComponent("Capture")
        _ = try FJ.uniqueCopies(count: 6, into: capture, name: { String(format: "DSC%05d.ARW", 6000 + $0) }, clone: true)
        let settingsDir = capture.appendingPathComponent("CaptureOne/Settings153")
        try fm.createDirectory(at: settingsDir, withIntermediateDirectories: true)
        for i in 0..<6 { try Data("<cos/>".utf8).write(to: settingsDir.appendingPathComponent(String(format: "DSC%05d.ARW.cos", 6000 + i))) }
        try fm.createDirectory(at: c1.appendingPathComponent("Selects"), withIntermediateDirectories: true)
        try fm.createDirectory(at: c1.appendingPathComponent("Output"), withIntermediateDirectories: true)
        try Data("sqlite".utf8).write(to: c1.appendingPathComponent("2026-08-06 Beach Trip.cosessiondb"))
        try fm.setAttributes([.posixPermissions: 0o444], ofItemAtPath: capture.appendingPathComponent("DSC06000.ARW").path)
        let before = await FJ.listing(c1)
        let m = FJ.liveModel(prefs: "c1", sessions: root.appendingPathComponent("Sessions")) { $0.onboardingComplete = true; $0.autoAdvance = false }
        await m.start()
        await m.openFolder(c1)
        FJ.log("c1: open root → photos=\(m.photoCount) session='\(m.currentSession?.name ?? "nil")' toast='\(m.toast?.message ?? "")'")
        if m.photoCount == 0 {
            await m.openFolder(capture)
            FJ.log("c1: open Capture/ → photos=\(m.photoCount) session='\(m.currentSession?.name ?? "nil")'")
        }
        _ = await waitUntil(timeout: .seconds(30)) { m.openProgress == nil }
        for (i, id) in m.visibleIDs.enumerated() {
            m.focus(id, extend: false); m.setRating(i % 5 + 1)
        }
        m.focus(m.visibleIDs.first, extend: false)
        m.updateEdit(label: "EV") { $0.exposure.ev = 0.3; $0.crop.rect = CGRect(x: 0.1, y: 0.1, width: 0.8, height: 0.8) }
        m.endEditGesture()
        await m.saver.flush()
        let after = await FJ.listing(c1)
        let changed = FJ.diff(before, after)
        let originalsTouched = changed.filter { $0.hasSuffix(".ARW") || $0.contains(".cos") || $0.contains("cosessiondb") }
        FJ.log("c1: writes inside the C1 folder: \(changed.count) \(changed.prefix(12)); originals/C1 files touched: \(originalsTouched)")
        let support = AppPaths.support
        FJ.log("c1: support root files: \(FJ.files(under: support).filter { $0.contains("InPlace") }.prefix(6))")
        m.stop()
    }

    // MARK: 5. Keyboard-only culling of 200 photos

    @Test func keyboardCulling200() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("cull200")
        if !fm.fileExists(atPath: root.appendingPathComponent("Shoot/C0199.ARW").path) {
            try? fm.removeItem(at: root)
            _ = try FJ.uniqueCopies(count: 200, into: root.appendingPathComponent("Shoot"), name: { String(format: "C%04d.ARW", $0) }, clone: true)
        }
        let m = FJ.liveModel(prefs: "cull200-\(UUID().uuidString.prefix(4))", sessions: root.appendingPathComponent("Sessions")) { $0.onboardingComplete = true; $0.autoAdvance = true }
        m.viewer.tileDelay = .milliseconds(60)
        let o0 = Date()
        await m.start()
        await m.openFolder(root.appendingPathComponent("Shoot"))
        let openSecs = Date().timeIntervalSince(o0)
        m.viewer.setViewerSize(CGSize(width: 1400, height: 900), backingScale: 2)
        FJ.log(String(format: "cull200: open %.2f s photos=%d openProgress=%@", openSecs, m.photoCount, String(describing: m.openProgress)))
        m.focus(m.visibleIDs.first, extend: false)
        _ = await waitUntil(timeout: .seconds(20)) { m.viewer.displayFrame != nil }

        // Keypress → next frame (embedded preview on screen) latency, with auto-advance ratings.
        var lat: [Double] = []
        var developed: [Double] = []
        let clock = ContinuousClock()
        for i in 0..<60 {
            let before = m.focusedID
            let t = clock.now
            let key = KeyInput("\(i % 5 + 1)")
            m.handleKey(key, focus: .none, sheetOpen: false, isMainWindow: true)
            guard m.focusedID != before else { FJ.log("cull200: no advance at \(i)"); break }
            _ = await waitUntil(timeout: .seconds(5)) { m.viewer.displayFrame != nil }
            lat.append(Double((clock.now - t).components.attoseconds) / 1e15 + Double((clock.now - t).components.seconds) * 1000)
            if i % 10 == 0 {
                _ = await waitUntil(timeout: .seconds(10)) { m.viewer.frame != nil }
                developed.append(Double((clock.now - t).components.attoseconds) / 1e15 + Double((clock.now - t).components.seconds) * 1000)
            }
        }
        lat.sort()
        func pct(_ a: [Double], _ p: Double) -> Double { a.isEmpty ? -1 : a[min(a.count - 1, Int(Double(a.count) * p))] }
        FJ.log(String(format: "cull200: key→next preview ms p50=%.1f p90=%.1f max=%.1f n=%d; key→developed ms %@", pct(lat, 0.5), pct(lat, 0.9), lat.last ?? -1, lat.count, developed.map { String(format: "%.0f", $0) }.description))

        // Held arrow (repeat) through 40 photos.
        var rep: [Double] = []
        for _ in 0..<40 {
            let t = clock.now
            m.handleKey(KeyInput(special: .right, isRepeat: true), focus: .none, sheetOpen: false, isMainWindow: true)
            _ = await waitUntil(timeout: .seconds(5)) { m.viewer.displayFrame != nil }
            rep.append(Double((clock.now - t).components.attoseconds) / 1e15 + Double((clock.now - t).components.seconds) * 1000)
        }
        m.setKeyRepeat(false)
        rep.sort()
        FJ.log(String(format: "cull200: held → ms p50=%.1f p90=%.1f max=%.1f", pct(rep, 0.5), pct(rep, 0.9), rep.last ?? -1))

        // Filters / ⌘1–⌘5 / Space zoom / P X U / 0 clear.
        await m.saver.flush()
        let dist = (0...5).map { n in m.photos.filter { $0.rating == n }.count }
        FJ.log("cull200: rating distribution 0…5 = \(dist)")
        m.showRatingFolder(3)
        FJ.log("cull200: ⌘3 shows \(m.visibleIDs.count) (expected \(dist[3])), focus=\(m.focusedPhoto?.filename ?? "nil") rating=\(m.focusedPhoto?.rating ?? -1)")
        m.perform(.zoomToggleAtCursor)
        FJ.log("cull200: Space → zoom=\(m.viewer.state.zoom) label=\(m.viewer.zoomLabel)")
        m.perform(.zoomToggleAtCursor)
        m.perform(.escape)
        m.selectSmartFolder(nil)
        // Grid navigation by keys.
        m.browserMode = .grid
        m.gridColumns = 6
        let g0 = m.visibleIndex(of: m.focusedID!) ?? -1
        m.handleKey(KeyInput(special: .down), focus: .none, sheetOpen: false, isMainWindow: true)
        FJ.log("cull200: grid ↓ moved \(g0) → \(m.visibleIndex(of: m.focusedID!) ?? -1)")
        // Keys while a slider has focus (after dragging exposure).
        let beforeFocus = m.focusedID
        let handled = m.handleKey(KeyInput(special: .right), focus: .control, sheetOpen: false, isMainWindow: true)
        FJ.log("cull200: → with a slider focused handled=\(handled) moved=\(m.focusedID != beforeFocus)")
        let rHandled = m.handleKey(KeyInput("3"), focus: .control, sheetOpen: false, isMainWindow: true)
        FJ.log("cull200: '3' with a slider focused handled=\(rHandled)")
        // Select 10 and rate them at once, then undo.
        m.focus(m.visibleIDs[10], extend: false)
        m.focus(m.visibleIDs[19], extend: true)
        let sel = m.selectedIDs
        m.perform(.rate(2))
        FJ.log("cull200: multi-select \(sel.count) rate 2 → \(sel.filter { m.photo($0)?.rating == 2 }.count) rated; focus moved=\(m.focusedID != m.visibleIDs[19])")
        m.perform(.undo)
        FJ.log("cull200: undo → \(sel.filter { m.photo($0)?.rating == 2 }.count) still 2; toast='\(m.toast?.message ?? "")'")
        m.stop()
    }

    // MARK: 7. AI

    @Test func aiNaturalLanguageAndAuto() async throws {
        guard FJ.sandboxed() else { return }
        FJ.log("ai: FoundationModels available=\(FoundationModelsEditService.isAvailable)")
        let phrases = ["make it warmer and brighter", "crop 4:5", "straighten", "straighten 2 degrees", "level the horizon",
                       "lift shadows", "more contrast", "black and white", "undo that", "warmer by 400", "remove the power lines",
                       "make the sky bluer", "a bit cooler and darker", "brighter by 7 stops", "reset"]
        let fmSvc = FoundationModelsEditService()
        let rules = RulesEditService()
        for p in phrases {
            let t = Date()
            var fmText = ""
            do { fmText = "\(try await fmSvc.intent(for: p))" } catch { fmText = "ERR \(error)" }
            let fmMs = Date().timeIntervalSince(t) * 1000
            var rText = ""
            do { rText = "\(try await rules.intent(for: p))" } catch { rText = "ERR \(error)" }
            FJ.log(String(format: "ai phrase '%@': FM(%.0f ms)=%@ | rules=%@", p, fmMs, compact(fmText), compact(rText)))
        }
        // Through the app on a live photo.
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("ai")
        try? fm.removeItem(at: root)
        let dir = root.appendingPathComponent("Shots")
        _ = try FJ.uniqueCopies(count: 8, into: dir, name: { String(format: "AI%02d.ARW", $0) }, clone: true)
        let m = FJ.liveModel(prefs: "ai", sessions: root.appendingPathComponent("Sessions")) { $0.onboardingComplete = true; $0.autoAdvance = false }
        await m.start()
        await m.openFolder(dir)
        let first = m.visibleIDs[0]
        for p in ["make it warmer and brighter", "crop 4:5", "straighten", "remove the power lines", "undo that"] {
            m.focus(first, extend: false)
            let before = m.edit(for: first)
            m.toast = nil
            await m.applyInstruction(p)
            let after = m.edit(for: first)
            FJ.log("ai app '\(p)': WB \(before.whiteBalance.asShot ? "asShot" : "\(before.whiteBalance.temperature)")→\(after.whiteBalance.asShot ? "asShot" : "\(after.whiteBalance.temperature)/\(after.whiteBalance.tint)") EV \(before.exposure.ev)→\(after.exposure.ev) crop.rect=\(after.crop.rect) aspect=\(after.crop.aspect) angle=\(after.crop.angle) toast='\(m.toast?.message ?? "")' undo='\(m.history.undoLabel ?? "")'")
        }
        // Auto on all 8.
        for id in m.visibleIDs {
            m.focus(id, extend: false)
            await m.loadDevelopInfo(for: [id])
            let asShot = m.developInfo[id]?.asShot
            let t = Date()
            await m.autoAdjust()
            let e = m.edit(for: id)
            FJ.log(String(format: "ai auto %@: %.0f ms asShot=%@ → WB %@ %.0fK/%.0f EV %.2f angle %.2f hdr(sh %.0f hl %.0f) toast='%@'", m.photo(id)!.filename, Date().timeIntervalSince(t) * 1000,
                          asShot.map { "\($0)" } ?? "nil", e.whiteBalance.asShot ? "asShot" : "custom", e.whiteBalance.temperature, e.whiteBalance.tint,
                          e.exposure.ev, e.crop.angle, e.hdr.shadows, e.hdr.highlights, m.toast?.message ?? ""))
        }
        // Vision on this Mac: does analyze succeed?
        let vs = VisionService()
        let p = m.photos[0]
        let img = try await m.images.thumbnail(for: p, maxPixelSize: 1024)
        do {
            let a = try await vs.analyze(img, includeLabels: true)
            FJ.log("ai vision analyze OK: \(String(describing: a).prefix(500))")
        } catch { FJ.log("ai vision analyze THREW: \(error)") }
        do { FJ.log("ai vision horizon: \(String(describing: try await vs.horizonDegrees(img)))") } catch { FJ.log("ai vision horizon THREW \(error)") }
        do { FJ.log("ai vision labels: \(try await vs.labels(img, limit: 6).map(\.identifier))") } catch { FJ.log("ai vision labels THREW \(error)") }
        // AI subject mask.
        let masks = await m.viewer.masksForRender(photo: p.id, targets: [.subject])
        FJ.log("ai subject mask: \(masks.map { "\($0.key): \($0.value.width)x\($0.value.height)" })")
        m.stop()
    }

    private func compact(_ s: String) -> String {
        s.replacingOccurrences(of: "DarkslideAI.", with: "").replacingOccurrences(of: "\n", with: " ").prefix(400).description
    }
}
