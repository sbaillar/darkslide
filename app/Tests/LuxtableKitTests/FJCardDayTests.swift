import CryptoKit
import Foundation
import ImageIO
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Journeys 2 + 3: card day, then return day, on the live services.
@MainActor @Suite(.serialized, .enabled(if: FJ.configured, "SKIP: set FJ_ROOT (scratch) and LT_SUPPORT_ROOT to run the final-journeys scenarios")) struct FJCardDayTests {
    @Test func cardDayThenReturnDay() async throws {
        guard FJ.sandboxed(), sampleARWs().count >= 8 else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("cardday")
        try? fm.removeItem(at: root)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        let sessionsDir = root.appendingPathComponent("Sessions")
        let archiveDir = root.appendingPathComponent("NAS/Luxtable")
        try fm.createDirectory(at: archiveDir.deletingLastPathComponent(), withIntermediateDirectories: true)
        let amazonDir = root.appendingPathComponent("Amazon Photos Backup")
        try fm.createDirectory(at: amazonDir, withIntermediateDirectories: true)

        // --- The card: exFAT, 40 unique ARWs + MP4 + junk.
        guard let img = await TestCardImage.make(in: root, name: "A9CARD", sizeMB: 2600) else {
            FJ.log("card: SKIP no disk image"); return
        }
        var card = img
        defer { Task { _ = await E2EIntegrationTests.checkedDetach(card) } }
        let dcim = card.mount.appendingPathComponent("DCIM/100MSDCF")
        let t0 = Date()
        _ = try await Task.detached {
            try FJ.uniqueCopies(count: 40, into: dcim, name: { String(format: "DSC%05d.ARW", 1000 + $0) })
        }.value
        let clips = card.mount.appendingPathComponent("PRIVATE/M4ROOT/CLIP")
        try fm.createDirectory(at: clips, withIntermediateDirectories: true)
        try await makeMP4(clips.appendingPathComponent("C0001.MP4"))
        try Data("junk".utf8).write(to: card.mount.appendingPathComponent("DCIM/100MSDCF/JUNK.DAT"))
        try Data([0, 1, 2]).write(to: card.mount.appendingPathComponent("DCIM/100MSDCF/._DSC01000.ARW"))
        guard let re = await card.remount() else { FJ.log("card: remount failed"); return }
        card = re
        FJ.log(String(format: "card: filled in %.1f s", Date().timeIntervalSince(t0)))
        let cardBefore = await FJ.listing(card.mount)

        // --- Launch (first-run settings done: sessions folder, archive, Amazon).
        let m = FJ.liveModel(prefs: "cardday", sessions: sessionsDir) { s in
            s.onboardingComplete = true
            s.archiveTarget = archiveDir.path
            s.archiveEnabled = true
            s.chooseAmazonFolder(amazonDir)
        }
        await m.start()
        FJ.log("launch: sessions=\(m.sessionList.count) current=\(m.currentSession?.name ?? "nil") sheet=\(String(describing: m.sheet)) archive=\(ArchiveDisplay.title(m.archiveStatus.state))")

        // --- Import sheet.
        let ejector = FakeEjector()
        let traits = VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "FJCARD")
        let im = ImportModel(model: m, volume: card.mount, ejector: ejector,
                             inspector: StaticVolumeInspector([card.mount.standardizedFileURL.path: traits]))
        await im.scan()
        FJ.log("import: phase=\(im.phase) candidates=\(im.candidates.count) videos=\(im.videoCount) dup=\(im.duplicateCount) skippedUnknown=\(im.skippedUnknown.map { "\($0.url.lastPathComponent): \($0.reason)" }) scanErrors=\(im.scanErrors.count) free=\(im.freeBytes.map(Formatters.bytes) ?? "nil")")
        FJ.log("import: issuesTitle='\(ImportWording.issuesTitle(scanErrors: im.scanErrors.count, skipped: im.skippedUnknown.count))' button='\(ImportWording.importButton(photos: im.selected.count - im.videoCount, videos: im.videoCount))' ready='\(ImportWording.readyLine(bytes: im.selectedBytes, destination: im.sessionText, verifies: im.verifies, free: im.freeBytes))'")
        im.sessionText = "Card Day"
        let i0 = Date()
        im.start()
        await im.wait()
        let importSecs = Date().timeIntervalSince(i0)
        guard case .finished(let summary) = im.phase else { FJ.log("import: FAILED \(im.phase)"); return }
        FJ.log(String(format: "import: %.1f s; title='%@' detail='%@' card='%@' safe=%d verified=%d stayOnCard=%@ gate=%@",
                      importSecs, ImportWording.doneTitle(summary), ImportWording.doneDetail(summary),
                      ImportWording.cardLine(summary) ?? "nil", summary.safeToRemove ? 1 : 0, summary.verified ? 1 : 0,
                      summary.stayOnCard.map { $0.url.lastPathComponent }.description, im.ejectGate?.buttonTitle ?? "nil"))
        _ = await waitUntil(timeout: .seconds(30)) { m.currentSession != nil && !m.isLoading }
        let session = try #require(m.currentSession)
        FJ.log("session: name='\(session.name)' url=\(session.url.path) photos=\(m.photoCount) visible=\(m.visibleIDs.count) videos=\(m.photos.filter(\.isVideo).count)")

        // Check the claim "copied and verified": every card file byte-equal in the session.
        let sessionFiles = FJ.files(under: session.url)
        FJ.log("session files (\(sessionFiles.count)): \(sessionFiles.filter { !$0.contains("/.lt/") && !$0.hasPrefix(".lt") }.prefix(6))… hidden=\(sessionFiles.filter { $0.contains(".lt") }.count)")
        var mismatches = 0, found = 0
        for u in (try? fm.contentsOfDirectory(at: dcim, includingPropertiesForKeys: nil)) ?? [] where u.pathExtension == "ARW" && !u.lastPathComponent.hasPrefix("._") {
            guard let p = m.photos.first(where: { $0.filename == u.lastPathComponent }), let pu = p.url else { continue }
            found += 1
            if !fm.contentsEqual(atPath: u.path, andPath: pu.path) { mismatches += 1 }
        }
        FJ.log("verify-claim: \(found)/40 ARWs found in session by name, \(mismatches) byte mismatches")

        // --- Keyboard culling: ratings with auto-advance.
        m.settings.autoAdvance = true
        m.browserMode = .grid
        m.focus(m.visibleIDs.first, extend: false)
        let stills = m.visibleIDs.filter { m.photo($0)?.isVideo != true }
        var expected: [PhotoID: Int] = [:]
        for (i, id) in stills.enumerated() {
            let want = i % 6 // 0…5
            if m.focusedPhoto?.id != id { FJ.log("autoadvance: focus drifted at \(i): \(m.focusedPhoto?.filename ?? "nil") != \(m.photo(id)?.filename ?? "")"); m.focus(id, extend: false) }
            m.perform(.rate(want))
            if want == 0 { m.focus(stills[min(i + 1, stills.count - 1)], extend: false) } // 0 = clear (no change) doesn't advance
            expected[id] = want
        }
        let rated = stills.filter { (m.photo($0)?.rating ?? -1) == expected[$0] }.count
        FJ.log("ratings: \(rated)/\(stills.count) as keyed")
        await m.saver.flush()

        // Smart folders: exact counts.
        for n in 1...5 {
            let want = expected.values.filter { $0 == n }.count
            let folder = m.smartFolders.ratingFolder(n)
            let count = folder.flatMap { m.smartFolders.counts[$0.id] }
            m.showRatingFolder(n)
            let shown = m.visibleIDs.count
            let exact = m.visibleIDs.allSatisfy { m.photo($0)?.rating == n }
            FJ.log("smartfolder ⌘\(n): name='\(folder?.name ?? "nil")' badge=\(count.map(String.init) ?? "nil") shown=\(shown) expected=\(want) allExact=\(exact)")
        }
        // Change a rating while inside ⌘3: the photo should leave and focus move on.
        m.showRatingFolder(3)
        let before3 = m.visibleIDs.count
        let f3 = m.focusedPhoto?.filename
        m.settings.autoAdvance = false
        m.perform(.rate(4))
        FJ.log("smartfolder ⌘3 re-rate: \(before3) -> \(m.visibleIDs.count) visible; focus \(f3 ?? "nil") -> \(m.focusedPhoto?.filename ?? "nil"); badge3=\(m.smartFolders.ratingFolder(3).flatMap { m.smartFolders.counts[$0.id] } ?? -1) badge4=\(m.smartFolders.ratingFolder(4).flatMap { m.smartFolders.counts[$0.id] } ?? -1)")
        m.perform(.undo)
        FJ.log("smartfolder ⌘3 after undo: \(m.visibleIDs.count) visible, toast='\(m.toast?.message ?? "")'")
        m.selectSmartFolder(nil)
        FJ.log("smart folders listed: \(m.smartFolders.visibleFolders.map { "\($0.name)=\(m.smartFolders.counts[$0.id] ?? -1)" })")

        // Pick / reject by key.
        m.focus(stills[0], extend: false); m.perform(.pick)
        m.focus(stills[1], extend: false); m.perform(.reject)
        m.focus(stills[5], extend: false); m.perform(.pick) // a 5-star

        // --- Burst stacks.
        m.perform(.toggleStack)
        let stacks = m.browserState.stacks.stacks
        FJ.log("stacks: \(stacks.count) stacks, sizes=\(stacks.map(\.count).sorted(by: >)), nav=\(m.navigationIDs.count) of \(m.visibleIDs.count)")
        m.perform(.toggleStack) // expand focused

        // --- AI Cull.
        let fiveStarUnflagged = stills.filter { m.photo($0)?.rating == 5 && m.photo($0)?.pick == PickState.none }
        let c0 = Date()
        await m.runAICull()
        let cullSecs = Date().timeIntervalSince(c0)
        let picks = stills.filter { m.photo($0)?.pick == .picked }.count
        let rejects = stills.filter { m.photo($0)?.pick == .rejected }.count
        let rejected5 = fiveStarUnflagged.filter { m.photo($0)?.pick == .rejected }.count
        FJ.log(String(format: "aicull: %.1f s toast='%@' picks=%d rejects=%d; 5★-unflagged rejected=%d of %d; suggestions=%d",
                      cullSecs, m.toast?.message ?? "", picks, rejects, rejected5, fiveStarUnflagged.count, m.cullSuggestions.count))
        let reasons = Dictionary(grouping: m.cullSuggestions.values.flatMap(\.reasons), by: { $0 }).mapValues(\.count)
        FJ.log("aicull reasons: \(reasons.sorted { $0.value > $1.value }.prefix(12))")
        // Whole groups rejected? (group by sample source = i % 8, i.e. identical frames)
        var groupRejected = 0
        for g in 0..<8 {
            let members = stills.enumerated().filter { $0.offset % 8 == g }.map(\.element)
            if members.allSatisfy({ m.photo($0)?.pick == .rejected }) { groupRejected += 1 }
        }
        FJ.log("aicull: \(groupRejected) of 8 identical-frame groups entirely rejected")

        // --- Copy WB + exposure from one photo, apply to all.
        let src = stills[2]
        m.focus(src, extend: false)
        m.updateEdit(label: "WB") { e in
            e.whiteBalance.asShot = false
            e.whiteBalance.temperature = 6500
            e.whiteBalance.tint = 6
            e.exposure.ev = 0.7
        }
        m.endEditGesture()
        m.perform(.copyAdjustments)
        FJ.log("copy: groups ticked=\(m.settings.copyGroups.map(\.rawValue).sorted()) toast='\(m.toast?.message ?? "")'")
        let changed = m.applyClipboard(to: m.photos.map(\.id), options: PasteOptions(groups: [.whiteBalance, .exposure]))
        let allWB = stills.allSatisfy { let e = m.edit(for: $0); return !e.whiteBalance.asShot && e.whiteBalance.temperature == 6500 && e.exposure.ev == 0.7 }
        FJ.log("apply-all: changed=\(changed) toast='\(m.toast?.message ?? "")' allHaveWB+EV=\(allWB) videosEdited=\(m.photos.filter { $0.isVideo && m.edits[$0.id] != nil }.count) undo='\(m.history.undoLabel ?? "")'")
        await m.saver.flush()
        let sidecars = FJ.files(under: session.url).filter { $0.hasSuffix(".json") }
        FJ.log("sidecars: \(sidecars.count) json files, e.g. \(sidecars.prefix(2))")
        if let sc = sidecars.first(where: { $0.contains("DSC01003") }) {
            let text = (try? String(contentsOf: session.url.appendingPathComponent(sc), encoding: .utf8)) ?? ""
            FJ.log("sidecar sample \(sc): \(text.prefix(600).replacingOccurrences(of: "\n", with: " "))")
        }

        // A crop on one photo (persist check).
        let cropID = stills[4]
        m.focus(cropID, extend: false)
        m.updateEdit(label: "Crop") { e in e.crop.rect = CGRect(x: 0.1, y: 0.1, width: 0.8, height: 0.8); e.crop.angle = 2 }
        m.endEditGesture()
        await m.saver.flush()

        // --- Export selects (picked) as JPEG.
        let pickedIDs = m.visibleIDs.filter { m.photo($0)?.pick == .picked }
        m.selection = Set(pickedIDs)
        m.beginExport()
        let ex = try #require(m.exportModel)
        _ = await waitUntil(timeout: .seconds(10)) { ex.destinationReady }
        FJ.log("export: items=\(ex.items.count) (picked \(pickedIDs.count)) folder=\(ex.defaultFolder.path) recipes=\(ex.recipes.map { "\($0.name)\($0.enabled ? "*" : "")" }) problem=\(ex.problem ?? "nil")")
        guard ex.defaultFolder.path.contains("scratchpad") else { FJ.log("export: ABORT, folder outside scratch"); m.endExport(); return }
        let e0 = Date()
        ex.start()
        await ex.waitUntilDone()
        let exportSecs = Date().timeIntervalSince(e0)
        if case .finished(let o) = ex.phase {
            FJ.log(String(format: "export: %.1f s, summary='%@' failures=%@", exportSecs, ExportModel.summary(o), o.failures.map { "\($0)" }.description))
            let onDisk = FJ.files(under: ex.defaultFolder)
            let dims = o.written.prefix(3).map { "\($0.url.lastPathComponent) \($0.width)x\($0.height)" }
            FJ.log("export: on disk \(onDisk.count) files \(onDisk.prefix(4)); dims \(dims); cropped photo exported? \(o.written.contains { $0.photoID == cropID })")
        } else {
            FJ.log("export: phase \(ex.phase)")
        }
        m.endExport()

        // --- Archive + Amazon.
        await m.archive.syncNow()
        let a0 = Date()
        _ = await waitUntil(timeout: .seconds(180)) {
            (m.archiveStatus.detail?.runs.contains { $0.outcome == "succeeded" } ?? false)
                && (m.archiveStatus.detail?.amazon.map { !$0.running } ?? true)
        }
        let st = m.archiveStatus
        FJ.log(String(format: "archive: %.1f s state='%@' long='%@' runs=%@ amazon=%@", Date().timeIntervalSince(a0),
                      ArchiveDisplay.title(st.state), ArchiveDisplay.long(st.state), (st.detail?.runs.map(ArchiveDisplay.runLine) ?? []).description,
                      st.detail?.amazon.map(ArchiveDisplay.amazonLine) ?? "nil"))
        FJ.log("archive sessions: \(st.detail?.sessions.map { ArchiveDisplay.sessionLine($0) } ?? [])")
        let archived = FJ.files(under: archiveDir)
        let archivedARW = archived.filter { $0.hasSuffix(".ARW") }.count
        let archivedJPG = archived.filter { $0.lowercased().hasSuffix(".jpg") }.count
        FJ.log("archive fs: \(archived.count) files, ARW=\(archivedARW) jpg=\(archivedJPG) mp4=\(archived.filter { $0.hasSuffix(".MP4") }.count) json=\(archived.filter { $0.hasSuffix(".json") }.count) cache=\(archived.filter { $0.contains("cache") }.count) top=\(Set(archived.map { String($0.split(separator: "/").prefix(2).joined(separator: "/")) }).sorted().prefix(8))")
        let amazon = FJ.files(under: amazonDir).filter { !$0.hasPrefix(".") }
        FJ.log("amazon fs: \(amazon.count) files: ARW=\(amazon.filter { $0.hasSuffix(".ARW") }.count) mp4=\(amazon.filter { $0.hasSuffix(".MP4") }.count) rejectsMirrored=\(stills.filter { m.photo($0)?.pick == .rejected }.filter { id in amazon.contains { $0.hasSuffix(m.photo(id)!.filename) } }.count) sample=\(amazon.prefix(3))")

        // --- Eject flow.
        FJ.log("eject: gate=\(String(describing: im.ejectGate)) title='\(im.ejectGate?.buttonTitle ?? "nil")'")
        await im.ejectNow()
        if im.confirmingEject {
            if case .finished(let s) = im.phase {
                FJ.log("eject: confirm '\(ImportWording.ejectAnywayTitle(s.stayOnCard.count))' / '\(ImportWording.ejectAnywayMessage(s))'")
            }
            await im.confirmEject()
        }
        if case .finished(let s) = im.phase { FJ.log("eject: ejected=\(ejector.ejected.count) cardLine='\(ImportWording.cardLine(s) ?? "")'") }
        let cardAfter = await FJ.listing(card.mount)
        FJ.log("card untouched: \(FJ.diff(cardBefore, cardAfter))")

        // Snapshot state for return day.
        let ratingsByName = Dictionary(uniqueKeysWithValues: m.photos.map { ($0.filename, $0.rating) })
        let picksByName = Dictionary(uniqueKeysWithValues: m.photos.map { ($0.filename, $0.pick) })
        let editsByName = Dictionary(uniqueKeysWithValues: m.edits.compactMap { id, e in m.photo(id).map { ($0.filename, e) } })
        let archiveTitle = ArchiveDisplay.title(m.archiveStatus.state)
        await m.saver.flush()
        let quit = await m.quitDecision(timeout: .seconds(5))
        FJ.log("quit decision: \(quit)")
        await m.shutdownArchive()
        m.stop()

        // ================= Return day: relaunch =================
        let r = FJ.liveModel(prefs: "cardday", sessions: sessionsDir)
        await r.start()
        _ = await waitUntil(timeout: .seconds(30)) { r.currentSession != nil && !r.isLoading }
        FJ.log("relaunch: current='\(r.currentSession?.name ?? "nil")' sessions=\(r.sessionList.map(\.name)) photos=\(r.photoCount) pending=\(r.pendingImport.map { "\($0.sessionName)" } ?? "nil") sheet=\(String(describing: r.sheet))")
        let rRatings = r.photos.filter { ratingsByName[$0.filename] == $0.rating }.count
        let rPicks = r.photos.filter { picksByName[$0.filename] == $0.pick }.count
        let rEdits = editsByName.filter { name, e in r.photos.first { $0.filename == name }.map { r.edit(for: $0.id) == e } ?? false }.count
        let cropBack = r.photos.first { $0.filename == m.photo(cropID)?.filename }.map { r.edit(for: $0.id).crop }
        FJ.log("relaunch persisted: ratings \(rRatings)/\(ratingsByName.count) picks \(rPicks)/\(picksByName.count) edits \(rEdits)/\(editsByName.count) crop=\(cropBack.map { "\($0.rect) \($0.angle)" } ?? "nil")")
        for n in 1...5 {
            FJ.log("relaunch ⌘\(n) badge=\(r.smartFolders.ratingFolder(n).flatMap { r.smartFolders.counts[$0.id] } ?? -1) expected=\(r.photos.filter { $0.rating == n }.count)")
        }
        _ = await waitUntil(timeout: .seconds(10)) { r.archiveStatus.lastSuccess != nil || r.archiveStatus.detail != nil }
        FJ.log("relaunch archive: '\(ArchiveDisplay.title(r.archiveStatus.state))' (was '\(archiveTitle)') lastSuccess=\(String(describing: r.archiveStatus.lastSuccess)) cullSuggestions=\(r.cullSuggestions.count)")
        await r.shutdownArchive()
        r.stop()
    }
}
