import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@MainActor @Suite(.serialized, .enabled(if: FJ.configured, "SKIP: set FJ_ROOT (scratch) and DS_SUPPORT_ROOT to run the final-journeys scenarios")) struct FJFollowupTests {
    func sh(_ args: [String]) async -> String? { await TestCardImage.run(args, timeout: 120) }

    /// After a damaged file errors in the viewer, do good photos still develop?
    @Test func renderErrorAfterDamagedFile() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("sticky")
        try? fm.removeItem(at: root)
        let dir = root.appendingPathComponent("Shoot")
        _ = try FJ.uniqueCopies(count: 3, into: dir, name: { String(format: "OK%02d.ARW", $0) }, clone: true, salt: "sticky")
        try (try Data(contentsOf: sampleARWs()[1])).prefix(20_000_000).write(to: dir.appendingPathComponent("AAA_TRUNC.ARW"))
        let m = FJ.liveModel(prefs: "sticky", sessions: root.appendingPathComponent("S")) { $0.onboardingComplete = true }
        m.viewer.tileDelay = .zero
        m.viewer.settleDelay = .milliseconds(20)
        await m.start()
        await m.openFolder(dir)
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        // Good first: it develops.
        let good = m.photos.first { $0.filename == "OK00.ARW" }!.id
        let bad = m.photos.first { $0.filename == "AAA_TRUNC.ARW" }!.id
        let good2 = m.photos.first { $0.filename == "OK01.ARW" }!.id
        m.focus(good, extend: false)
        let g1 = await waitUntil(timeout: .seconds(30)) { m.viewer.frame != nil }
        FJ.log("sticky: good first developed=\(g1) err='\(m.viewer.renderError ?? "")'")
        m.focus(bad, extend: false)
        _ = await waitUntil(timeout: .seconds(30)) { m.viewer.renderError != nil }
        FJ.log("sticky: bad → err='\(m.viewer.renderError ?? "")' preview=\(m.viewer.showingPreview)")
        m.focus(good2, extend: false)
        let g2 = await waitUntil(timeout: .seconds(30)) { m.viewer.frame != nil }
        FJ.log("sticky: next good developed=\(g2) err='\(m.viewer.renderError ?? "")' renders=\(m.viewer.renderCount)")
        m.focus(good, extend: false)
        let g3 = await waitUntil(timeout: .seconds(30)) { m.viewer.frame != nil }
        FJ.log("sticky: back to first good developed=\(g3) err='\(m.viewer.renderError ?? "")'")
        m.stop()
    }

    /// Card pulled mid-import (throttled), then the resume prompt and resume.
    @Test func cardPulledThenResume2() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("pulled2")
        try? fm.removeItem(at: root)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        guard var card = await TestCardImage.make(in: root, name: "PULLED2", sizeMB: 700) else { return }
        _ = try FJ.uniqueCopies(count: 10, into: card.mount.appendingPathComponent("DCIM/100MSDCF"), name: { String(format: "DSC%05d.ARW", 9100 + $0) }, salt: "pulled2")
        guard let re = await card.remount() else { return }
        card = re
        let sessions = root.appendingPathComponent("Sessions")
        setenv("DS_IMPORT_THROTTLE_MS", "30", 1)
        let m = FJ.liveModel(prefs: "pulled2", sessions: sessions) { $0.onboardingComplete = true }
        unsetenv("DS_IMPORT_THROTTLE_MS")
        await m.start()
        let im = ImportModel(model: m, volume: card.mount, ejector: FakeEjector(),
                             inspector: StaticVolumeInspector([card.mount.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "PULLED2")]))
        await im.scan()
        im.sessionText = "Pulled"
        FJ.log("pulled2: selected=\(im.selected.count) dups=\(im.duplicateCount) canStart=\(im.canStart)")
        im.start()
        _ = await waitUntil(timeout: .seconds(120)) { (im.progress?.filesDone ?? 0) >= 3 }
        FJ.log("pulled2: at \(im.progress?.filesDone ?? 0)/\(im.progress?.filesTotal ?? 0) files; detach -force")
        _ = await sh(["/usr/bin/hdiutil", "detach", "-force", card.device])
        await im.wait()
        FJ.log("pulled2: phase(before unmount event)=\(im.phase)")
        im.volumeDidDisappear(card.mount)
        FJ.log("pulled2: phase=\(im.phase) current=\(m.currentSession?.name ?? "nil") photos=\(m.photoCount) sessions=\((try? fm.contentsOfDirectory(atPath: sessions.path)) ?? [])")
        FJ.log("pulled2: partials=\(FJ.files(under: sessions).filter { $0.contains("partial") }) journals=\(FJ.files(under: sessions).filter { $0.lowercased().contains("journal") })")
        m.stop()
        let r = FJ.liveModel(prefs: "pulled2", sessions: sessions)
        await r.start()
        if let p = r.pendingImport {
            FJ.log("pulled2 relaunch: prompt='\(ImportWording.resumePrompt(sessionName: p.sessionName, done: p.done, pending: p.pending))' current=\(r.currentSession?.name ?? "nil") photos=\(r.photoCount)")
        } else { FJ.log("pulled2 relaunch: NO resume prompt; current=\(r.currentSession?.name ?? "nil") photos=\(r.photoCount)") }
        guard let back = await TestCardImage.attach(card.image, at: card.mount) else { return }
        defer { Task { await back.detach() } }
        if let p = r.pendingImport { await r.resumePendingImport(p) }
        let im2 = ImportModel(model: r, volume: back.mount, ejector: FakeEjector(),
                              inspector: StaticVolumeInspector([back.mount.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "PULLED2")]))
        await im2.scan()
        FJ.log("resume2: destination=\(im2.destination) resuming=\(String(describing: im2.resuming)) selected=\(im2.selected.count) dups=\(im2.duplicateCount)")
        im2.start()
        await im2.wait()
        if case .finished(let s) = im2.phase { FJ.log("resume2: '\(ImportWording.doneTitle(s))' / '\(ImportWording.doneDetail(s))' / '\(ImportWording.cardLine(s) ?? "")'") } else { FJ.log("resume2: \(im2.phase)") }
        _ = await waitUntil(timeout: .seconds(20)) { !r.isLoading }
        FJ.log("resume2: session photos=\(r.photoCount) sessions=\(r.sessionList.map(\.name)) filesOnDisk=\(FJ.files(under: sessions).filter { $0.hasSuffix(".ARW") }.count)")
        r.stop()
    }

    /// Imported session renamed in Finder while open: where do later saves go?
    @Test func renameImportedDetail() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("rename-imp2")
        try? fm.removeItem(at: root)
        let cardDir = root.appendingPathComponent("CARD")
        _ = try FJ.uniqueCopies(count: 3, into: cardDir.appendingPathComponent("DCIM/100MSDCF"), name: { String(format: "DSC%05d.ARW", 7100 + $0) }, clone: true, salt: "rimp2")
        let sessions = root.appendingPathComponent("Sessions")
        let m = FJ.liveModel(prefs: "renameimp2", sessions: sessions) { $0.onboardingComplete = true; $0.autoAdvance = false }
        await m.start()
        let im = ImportModel(model: m, volume: cardDir, ejector: FakeEjector(),
                             inspector: StaticVolumeInspector([cardDir.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: false, isRemovable: false, uuid: "F2")]))
        await im.scan()
        im.sessionText = "Beach"
        im.start()
        await im.wait()
        _ = await waitUntil(timeout: .seconds(20)) { m.currentSession != nil && !m.isLoading }
        guard let s = m.currentSession else { FJ.log("rimp2: no session \(im.phase)"); return }
        let ids = m.visibleIDs
        m.focus(ids[0], extend: false); m.setRating(2)
        await m.saver.flush()
        let before = FJ.files(under: s.url).filter { $0.hasSuffix(".json") }
        let renamed = s.url.deletingLastPathComponent().appendingPathComponent("Beach RENAMED")
        try fm.moveItem(at: s.url, to: renamed)
        m.focus(ids[1], extend: false); m.setRating(4)
        m.updateEdit(label: "EV") { $0.exposure.ev = -1 }
        m.endEditGesture()
        await m.saver.flush()
        for _ in 0..<20 { if m.saver.state == .saved { break }; try? await Task.sleep(for: .milliseconds(500)) }
        FJ.log("rimp2: after rename saver=\(m.saver.state) pending=\(m.saver.pendingCount) toast='\(m.toast?.message ?? "")' ghost=\(fm.fileExists(atPath: s.url.path) ? FJ.files(under: s.url) : ["<none>"])")
        let renamedSidecar = renamed.appendingPathComponent("Capture/.ds/settings/DSC07101.ARW.json")
        let text = (try? String(contentsOf: renamedSidecar, encoding: .utf8)) ?? "<missing>"
        FJ.log("rimp2: sidecars before=\(before.count); renamed DSC07101 sidecar has rating 4? \(text.contains("\"rating\": 4")) ev -1? \(text.contains("-1.0"))")
        let q = await m.quitDecision(timeout: .seconds(5))
        FJ.log("rimp2: quit decision=\(q)")
        m.stop()
    }

    /// Auto WB/tone quality: export as-shot vs Auto for the biggest Kelvin swings.
    @Test func autoQualityExports() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("autoq")
        try? fm.removeItem(at: root)
        let dir = root.appendingPathComponent("Shots")
        _ = try FJ.uniqueCopies(count: 8, into: dir, name: { String(format: "AQ%02d.ARW", $0) }, clone: true, salt: "autoq")
        let m = FJ.liveModel(prefs: "autoq", sessions: root.appendingPathComponent("S")) { $0.onboardingComplete = true }
        await m.start()
        await m.openFolder(dir)
        var items: [ExportJobItem] = []
        for (i, id) in m.visibleIDs.enumerated() {
            m.focus(id, extend: false)
            await m.autoAdjust()
            let url = m.photo(id)!.url!
            items.append(ExportJobItem(photoID: id, url: url, seq: i * 2 + 1))
            items.append(ExportJobItem(photoID: id, url: url, seq: i * 2 + 2, edit: m.edit(for: id)))
        }
        let out = FJ.root.deletingLastPathComponent().appendingPathComponent("shots/auto")
        try? fm.removeItem(at: out)
        let o = try await m.exporter.export(ExportJob(items: items, recipes: [ExportRecipe(name: "A", format: .jpeg, quality: 80, sizeMode: .longEdge, sizeValue: 900, fileNameTemplate: "{seq}_{name}", destination: out)], defaultFolder: out)) { _, _ in }
        FJ.log("autoq: \(ExportModel.summary(o)) → \(out.path)")
        m.stop()
    }
}
