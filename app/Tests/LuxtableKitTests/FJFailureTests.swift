import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Journey 6: failure modes on the live services.
@MainActor @Suite(.serialized, .enabled(if: FJ.configured, "SKIP: set FJ_ROOT (scratch) and LT_SUPPORT_ROOT to run the final-journeys scenarios")) struct FJFailureTests {
    func sh(_ args: [String]) async -> String? { await TestCardImage.run(args, timeout: 120) }

    /// Open a folder with good, truncated, empty and foreign RAWs; view, thumbnail, export them.
    @Test func damagedAndForeignRaws() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("bad")
        try? fm.removeItem(at: root)
        let dir = root.appendingPathComponent("Mixed Shoot")
        _ = try FJ.uniqueCopies(count: 3, into: dir, name: { String(format: "GOOD%02d.ARW", $0) }, clone: true)
        let src = sampleARWs()[0]
        let data = try Data(contentsOf: src)
        try data.prefix(20_000_000).write(to: dir.appendingPathComponent("TRUNC20M.ARW"))
        try data.prefix(300_000).write(to: dir.appendingPathComponent("TRUNC300K.ARW"))
        try Data().write(to: dir.appendingPathComponent("EMPTY.ARW"))
        var junk = data
        junk.replaceSubrange(200_000..<30_000_000, with: Data(repeating: 0xAB, count: 29_800_000))
        try junk.write(to: dir.appendingPathComponent("GARBLED.ARW"))
        let py = FJ.root.deletingLastPathComponent().appendingPathComponent("mkdng.py").path
        _ = await sh(["/usr/bin/python3", py, dir.appendingPathComponent("CANON_R5.DNG").path, "Canon", "Canon EOS R5"])
        _ = await sh(["/usr/bin/python3", py, dir.appendingPathComponent("UNKNOWN.DNG").path, "Acme", "Acme X1"])
        let before = await FJ.listing(dir)

        let m = FJ.liveModel(prefs: "bad", sessions: root.appendingPathComponent("Sessions")) { $0.onboardingComplete = true }
        m.viewer.tileDelay = .zero
        m.viewer.settleDelay = .milliseconds(20)
        await m.start()
        await m.openFolder(dir)
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        let failures = await (m.sessions as? any FolderSessionOpening)?.lastFailures ?? []
        FJ.log("bad: open toast='\(m.toast?.message ?? "")' photos=\(m.photos.map { "\($0.filename)\($0.isMissing ? "(missing)" : "")" }) lastFailures=\(failures.map(\.lastPathComponent))")
        for p in m.photos {
            m.focus(p.id, extend: false)
            let ok = await waitUntil(timeout: .seconds(25)) { m.viewer.frame != nil || m.viewer.renderError != nil }
            let thumb: String
            do { let t = try await m.images.thumbnail(for: p, maxPixelSize: 256); thumb = "\(t.width)x\(t.height)" }
            catch { thumb = "ERR \(error.localizedDescription)" }
            FJ.log("bad: \(p.filename) cam='\(p.meta.camera)' \(p.meta.pixelWidth)x\(p.meta.pixelHeight) settled=\(ok) frame=\(m.viewer.frame.map { "\($0.width)x\($0.height)" } ?? "nil") preview=\(m.viewer.showingPreview) renderError='\(m.viewer.renderError ?? "")' thumb=\(thumb)")
        }
        // Lens / develop info for the foreign file.
        if let dng = m.photos.first(where: { $0.filename == "CANON_R5.DNG" }) {
            await m.loadDevelopInfo(for: [dng.id])
            FJ.log("bad: DNG develop info: \(String(describing: m.developInfo[dng.id]))")
        }
        // Export them all to a scratch folder.
        let out = root.appendingPathComponent("Export")
        let items = m.photos.enumerated().compactMap { i, p in p.url.map { ExportJobItem(photoID: p.id, url: $0, seq: i + 1, edit: m.edit(for: p.id)) } }
        let recipe = ExportRecipe(name: "FJ", format: .jpeg, quality: 85, sizeMode: .longEdge, sizeValue: 1600, destination: out)
        do {
            let o = try await m.exporter.export(ExportJob(items: items, recipes: [recipe], defaultFolder: out)) { _, _ in }
            FJ.log("bad: export \(ExportModel.summary(o)); failures=\(o.failures)")
        } catch { FJ.log("bad: export threw \(error.localizedDescription)") }
        await m.saver.flush()
        let after = await FJ.listing(dir)
        FJ.log("bad: folder writes after open+view+export: \(FJ.diff(before, after).prefix(20))")
        m.stop()
    }

    /// Rename the open folder in Finder (mv), keep rating; then relaunch.
    @Test func renameWhileOpenInPlace() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("rename")
        try? fm.removeItem(at: root)
        let dir = root.appendingPathComponent("Hike")
        _ = try FJ.uniqueCopies(count: 4, into: dir, name: { String(format: "R%02d.ARW", $0) }, clone: true)
        let m = FJ.liveModel(prefs: "rename", sessions: root.appendingPathComponent("Sessions")) { $0.onboardingComplete = true; $0.autoAdvance = false }
        await m.start()
        await m.openFolder(dir)
        let ids = m.visibleIDs
        m.focus(ids[0], extend: false); m.setRating(3)
        await m.saver.flush()
        let renamed = root.appendingPathComponent("Hike 2026")
        try fm.moveItem(at: dir, to: renamed)
        m.focus(ids[1], extend: false); m.setRating(5)
        m.updateEdit(label: "EV") { $0.exposure.ev = 1 }
        m.endEditGesture()
        await m.saver.flush()
        _ = await waitUntil(timeout: .seconds(5)) { m.toast != nil }
        FJ.log("rename(in-place): saver=\(m.saver.state) toast='\(m.toast?.message ?? "")' current=\(m.currentSession?.url.path ?? "nil") oldExists=\(fm.fileExists(atPath: dir.path)) oldContents=\(fm.fileExists(atPath: dir.path) ? FJ.files(under: dir) : [])")
        m.focus(ids[2], extend: false)
        _ = await waitUntil(timeout: .seconds(10)) { m.viewer.displayFrame != nil || m.viewer.renderError != nil }
        FJ.log("rename(in-place): view after rename frame=\(m.viewer.displayFrame != nil) err='\(m.viewer.renderError ?? "")' url=\(m.focusedPhoto?.url?.path ?? "nil")")
        m.stop()
        let r = FJ.liveModel(prefs: "rename", sessions: root.appendingPathComponent("Sessions"))
        await r.start()
        _ = await waitUntil(timeout: .seconds(20)) { r.currentSession != nil && !r.isLoading }
        FJ.log("rename(in-place) relaunch: current=\(r.currentSession?.url.path ?? "nil") sessions=\(r.sessionList.map { $0.url.lastPathComponent }) toast='\(r.toast?.message ?? "")' ratings=\(r.photos.map { "\($0.filename)=\($0.rating)\($0.isMissing ? "(missing)" : "")" })")
        await r.openFolder(renamed)
        FJ.log("rename(in-place) reopen renamed: ratings=\(r.photos.map { "\($0.filename)=\($0.rating)" }) ev=\(r.photos.map { r.edit(for: $0.id).exposure.ev })")
        r.stop()
    }

    /// Import from a plain-folder "card", then rename the session folder while it is open.
    @Test func renameImportedSessionWhileOpen() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("rename-imp")
        try? fm.removeItem(at: root)
        let cardDir = root.appendingPathComponent("CARD")
        _ = try FJ.uniqueCopies(count: 4, into: cardDir.appendingPathComponent("DCIM/100MSDCF"), name: { String(format: "DSC%05d.ARW", 7000 + $0) }, clone: true)
        let sessions = root.appendingPathComponent("Sessions")
        let m = FJ.liveModel(prefs: "renameimp", sessions: sessions) { $0.onboardingComplete = true; $0.autoAdvance = false }
        await m.start()
        let im = ImportModel(model: m, volume: cardDir, ejector: FakeEjector(),
                             inspector: StaticVolumeInspector([cardDir.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: false, isRemovable: false, uuid: "FOLDER")]))
        await im.scan()
        im.sessionText = "Beach"
        im.start()
        await im.wait()
        FJ.log("rename(imported): import phase=\(im.phase)")
        _ = await waitUntil(timeout: .seconds(20)) { m.currentSession != nil && !m.isLoading }
        guard let s = m.currentSession else { return }
        let ids = m.visibleIDs
        m.focus(ids[0], extend: false); m.setRating(2)
        await m.saver.flush()
        let renamed = s.url.deletingLastPathComponent().appendingPathComponent("Beach RENAMED")
        try fm.moveItem(at: s.url, to: renamed)
        m.focus(ids[1], extend: false); m.setRating(4)
        m.updateEdit(label: "EV") { $0.exposure.ev = -1 }
        m.endEditGesture()
        await m.saver.flush()
        FJ.log("rename(imported): saver=\(m.saver.state) toast='\(m.toast?.message ?? "")' ghostExists=\(fm.fileExists(atPath: s.url.path)) ghost=\(fm.fileExists(atPath: s.url.path) ? FJ.files(under: s.url) : [])")
        m.stop()
        let r = FJ.liveModel(prefs: "renameimp", sessions: sessions)
        await r.start()
        _ = await waitUntil(timeout: .seconds(20)) { r.currentSession != nil && !r.isLoading }
        FJ.log("rename(imported) relaunch: sessions=\(r.sessionList.map { "\($0.name)@\($0.url.lastPathComponent)" }) current=\(r.currentSession?.url.lastPathComponent ?? "nil") ratings=\(r.photos.map { "\($0.filename)=\($0.rating)\($0.isMissing ? "(missing)" : "")" })")
        await r.openFolder(renamed)
        FJ.log("rename(imported) open renamed: ratings=\(r.photos.map { "\($0.filename)=\($0.rating)" }) ev=\(r.photos.map { r.edit(for: $0.id).exposure.ev }) toast='\(r.toast?.message ?? "")'")
        r.stop()
    }

    /// Archive target on a disk image that goes away (NAS offline), and disk-full paths.
    @Test func nasOfflineAndDiskFull() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("offline")
        try? fm.removeItem(at: root)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        let dir = root.appendingPathComponent("Session A")
        _ = try FJ.uniqueCopies(count: 2, into: dir, name: { String(format: "N%02d.ARW", $0) }, clone: true)
        guard let nas = await TestCardImage.make(in: root, name: "FJNAS", sizeMB: 400) else { return }
        defer { Task { await nas.detach() } }
        let target = nas.mount.appendingPathComponent("Luxtable")
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        let m = FJ.liveModel(prefs: "offline", sessions: root.appendingPathComponent("Sessions")) {
            $0.onboardingComplete = true; $0.archiveTarget = target.path; $0.archiveEnabled = true; $0.autoAdvance = false
        }
        await m.start()
        await m.openFolder(dir)
        await m.archive.syncNow()
        _ = await waitUntil(timeout: .seconds(90)) { m.archiveStatus.detail?.runs.contains { $0.outcome == "succeeded" } ?? false }
        FJ.log("nas: first run '\(ArchiveDisplay.title(m.archiveStatus.state))' runs=\(m.archiveStatus.detail?.runs.map(ArchiveDisplay.runLine) ?? []) files on NAS=\(FJ.files(under: target).count): \(FJ.files(under: target).prefix(12))")
        await nas.detach()
        FJ.log("nas: detached; mount dir still exists=\(fm.fileExists(atPath: nas.mount.path))")
        m.focus(m.visibleIDs.first, extend: false); m.setRating(4)
        await m.saver.flush()
        await m.archive.syncNow()
        _ = await waitUntil(timeout: .seconds(30)) { if case .offline = m.archiveStatus.state { return true }; if case .failed = m.archiveStatus.state { return true }; return false }
        let st = m.archiveStatus
        FJ.log("nas offline: title='\(ArchiveDisplay.title(st.state))' short='\(ArchiveDisplay.short(st.state))' long='\(ArchiveDisplay.long(st.state))' expl='\(ArchiveDisplay.explanation(st.state))' ax='\(ArchiveDisplay.accessibilityLabel(st))' runs=\(st.detail?.runs.map(ArchiveDisplay.runLine) ?? [])")
        FJ.log("nas offline: wrote into bare mount dir? \(fm.fileExists(atPath: target.path)) \(fm.fileExists(atPath: nas.mount.path) ? FJ.files(under: nas.mount).prefix(5).description : "")")
        await m.shutdownArchive()
        m.stop()

        // --- Disk full: import into a sessions folder on a tiny volume.
        guard let tiny = await TestCardImage.make(in: root, name: "FJTINY", sizeMB: 150) else { return }
        defer { Task { await tiny.detach() } }
        let cardDir = root.appendingPathComponent("CARD")
        _ = try FJ.uniqueCopies(count: 5, into: cardDir.appendingPathComponent("DCIM/100MSDCF"), name: { String(format: "DSC%05d.ARW", 8000 + $0) }, clone: true)
        let m2 = FJ.liveModel(prefs: "full", sessions: tiny.mount.appendingPathComponent("Sessions")) { $0.onboardingComplete = true }
        await m2.start()
        let im = ImportModel(model: m2, volume: cardDir, ejector: FakeEjector(),
                             inspector: StaticVolumeInspector([cardDir.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: false, isRemovable: false, uuid: "FULL")]))
        await im.scan()
        im.sessionText = "Full"
        FJ.log("diskfull import: lacksSpace=\(im.lacksSpace) canStart=\(im.canStart) free=\(im.freeBytes.map(Formatters.bytes) ?? "nil") line='\(im.lacksSpace ? ImportWording.notEnoughSpace(needed: im.neededBytes, free: im.freeBytes ?? 0) : "")'")
        // Deselect down to what "fits" by the preflight, then fill the disk behind its back.
        while im.lacksSpace, let last = im.selected.last { im.toggle(last) }
        _ = await sh(["/bin/dd", "if=/dev/zero", "of=\(tiny.mount.appendingPathComponent("filler").path)", "bs=1m", "count=\(max(1, Int((im.freeBytes ?? 0) / 1_048_576) - Int(im.neededBytes / 1_048_576) + 5))"])
        im.start()
        await im.wait()
        if case .finished(let s) = im.phase {
            FJ.log("diskfull import: title='\(ImportWording.doneTitle(s))' detail='\(ImportWording.doneDetail(s))' failed=\(s.failed.map { "\($0.url.lastPathComponent): \($0.reason)" }) card='\(ImportWording.cardLine(s) ?? "")'")
        } else { FJ.log("diskfull import: phase=\(im.phase)") }
        FJ.log("diskfull import: partial files left=\(FJ.files(under: tiny.mount).filter { $0.hasSuffix(".partial") })")
        m2.stop()

        // Sidecar save on a full disk: a session on the tiny volume, disk filled, then rate + edit.
        let tsess = tiny.mount.appendingPathComponent("OnTiny")
        try? fm.removeItem(at: tiny.mount.appendingPathComponent("filler"))
        _ = try FJ.uniqueCopies(count: 1, into: tsess, name: { _ in "T00.ARW" })
        let m3 = FJ.liveModel(prefs: "full3", sessions: root.appendingPathComponent("S3")) { $0.onboardingComplete = true; $0.autoAdvance = false }
        await m3.start()
        await m3.openFolder(tsess)
        _ = await sh(["/bin/dd", "if=/dev/zero", "of=\(tiny.mount.appendingPathComponent("filler2").path)", "bs=1m", "count=400"])
        m3.focus(m3.visibleIDs.first, extend: false)
        m3.setRating(5)
        m3.updateEdit(label: "EV") { $0.exposure.ev = 0.5 }
        m3.endEditGesture()
        await m3.saver.flush()
        let q = await m3.quitDecision(timeout: .seconds(5))
        FJ.log("diskfull sidecar: saver=\(m3.saver.state) pending=\(m3.saver.pendingCount) toast='\(m3.toast?.message ?? "")' quit=\(q)")
        m3.stop()

        // Export onto the full volume.
        let exItems = [ExportJobItem(photoID: PhotoID(1), url: dir.appendingPathComponent("N00.ARW"), seq: 1),
                       ExportJobItem(photoID: PhotoID(2), url: dir.appendingPathComponent("N01.ARW"), seq: 2),
                       ExportJobItem(photoID: PhotoID(3), url: dir.appendingPathComponent("N00.ARW"), seq: 3)]
        let dest = tiny.mount.appendingPathComponent("Exports")
        let t0 = Date()
        do {
            let o = try await RustExportService().export(ExportJob(items: exItems, recipes: [ExportRecipe(name: "Full", format: .jpeg, quality: 92, destination: dest)], defaultFolder: dest)) { _, _ in }
            FJ.log(String(format: "diskfull export: %.1f s %@ failures=%@", Date().timeIntervalSince(t0), ExportModel.summary(o), o.failures.description))
        } catch { FJ.log("diskfull export threw: \(error.localizedDescription)") }
    }

    /// Card pulled mid-import, then the resume prompt on relaunch.
    @Test func cardPulledThenResume() async throws {
        guard FJ.sandboxed() else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("pulled")
        try? fm.removeItem(at: root)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        guard var card = await TestCardImage.make(in: root, name: "PULLED", sizeMB: 700) else { return }
        _ = try FJ.uniqueCopies(count: 10, into: card.mount.appendingPathComponent("DCIM/100MSDCF"), name: { String(format: "DSC%05d.ARW", 9000 + $0) })
        guard let re = await card.remount() else { return }
        card = re
        let sessions = root.appendingPathComponent("Sessions")
        setenv("LT_IMPORT_THROTTLE_MS", "40", 1)
        let m = FJ.liveModel(prefs: "pulled", sessions: sessions) { $0.onboardingComplete = true }
        unsetenv("LT_IMPORT_THROTTLE_MS")
        await m.start()
        let im = ImportModel(model: m, volume: card.mount, ejector: FakeEjector(),
                             inspector: StaticVolumeInspector([card.mount.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "PULLED")]))
        await im.scan()
        im.sessionText = "Pulled"
        im.start()
        _ = await waitUntil(timeout: .seconds(120)) { (im.progress?.filesDone ?? 0) >= 3 }
        FJ.log("pulled: at \(im.progress?.filesDone ?? 0)/\(im.progress?.filesTotal ?? 0) files, detaching -force")
        _ = await sh(["/usr/bin/hdiutil", "detach", "-force", card.device])
        im.volumeDidDisappear(card.mount) // what CardCoordinator does on the unmount event
        await im.wait()
        FJ.log("pulled: phase=\(im.phase) current=\(m.currentSession?.name ?? "nil") photos=\(m.photoCount)")
        let partials = FJ.files(under: sessions).filter { $0.contains(".partial") }
        FJ.log("pulled: partials=\(partials.count) sessions on disk=\((try? fm.contentsOfDirectory(atPath: sessions.path)) ?? [])")
        m.stop()
        let r = FJ.liveModel(prefs: "pulled", sessions: sessions)
        await r.start()
        if let p = r.pendingImport {
            FJ.log("pulled relaunch: prompt='\(ImportWording.resumePrompt(sessionName: p.sessionName, done: p.done, pending: p.pending))'")
        } else { FJ.log("pulled relaunch: NO resume prompt") }
        // Re-insert and resume.
        guard let back = await TestCardImage.attach(card.image, at: card.mount) else { return }
        defer { Task { await back.detach() } }
        if let p = r.pendingImport { await r.resumePendingImport(p) }
        let im2 = ImportModel(model: r, volume: back.mount, ejector: FakeEjector(),
                              inspector: StaticVolumeInspector([back.mount.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "PULLED")]))
        await im2.scan()
        FJ.log("resume: destination=\(im2.destination) resuming=\(String(describing: im2.resuming)) selected=\(im2.selected.count) dups=\(im2.duplicateCount)")
        im2.start()
        await im2.wait()
        if case .finished(let s) = im2.phase { FJ.log("resume: '\(ImportWording.doneTitle(s))' / '\(ImportWording.doneDetail(s))' / '\(ImportWording.cardLine(s) ?? "")'") } else { FJ.log("resume: \(im2.phase)") }
        _ = await waitUntil(timeout: .seconds(20)) { !r.isLoading }
        FJ.log("resume: session photos=\(r.photoCount) sessions=\(r.sessionList.map(\.name))")
        r.stop()
    }
}
