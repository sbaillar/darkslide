import Foundation
import ImageIO
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Final-journeys 2: a whole card day on main, every UI claim checked on disk.
@MainActor @Suite(.serialized, .enabled(if: FJ.configured, "SKIP: set FJ_ROOT (scratch) and LT_SUPPORT_ROOT to run the final-journeys scenarios")) struct FJ2CardDayTests {
    func sh(_ args: [String], timeout: TimeInterval = 120) async -> String? { await TestCardImage.run(args, timeout: timeout) }

    /// A NAS stand-in mounted by hdiutil at /Volumes/<name> (the real mount detection path).
    func attachNAS(_ image: URL) async -> TestCardImage? {
        guard let out = await sh(["/usr/bin/hdiutil", "attach", "-nobrowse", "-noautoopen", image.path]) else { return nil }
        for line in out.split(separator: "\n") where line.contains("/Volumes/") {
            let dev = String(line.split(whereSeparator: \.isWhitespace).first ?? "")
            let mount = String(line[line.range(of: "/Volumes/")!.lowerBound...]).trimmingCharacters(in: .whitespaces)
            return TestCardImage(image: image, mount: URL(fileURLWithPath: mount), device: dev)
        }
        return nil
    }

    func dims(_ url: URL) -> String {
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil),
              let p = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any] else { return "?" }
        return "\(p[kCGImagePropertyPixelWidth] ?? "?")x\(p[kCGImagePropertyPixelHeight] ?? "?")"
    }

    @Test func cardDay2() async throws {
        guard FJ.sandboxed(), sampleARWs().count >= 8 else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("cardday2")
        try? fm.removeItem(at: root)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        let sessionsDir = root.appendingPathComponent("Sessions")
        let amazonDir = root.appendingPathComponent("Amazon Photos Backup")
        try fm.createDirectory(at: amazonDir, withIntermediateDirectories: true)

        // --- NAS: a volume at /Volumes/FJ2NAS; the archive folder doesn't exist yet.
        let nasImg = root.appendingPathComponent("FJ2NAS.dmg")
        _ = await sh(["/usr/bin/hdiutil", "create", "-quiet", "-size", "3g", "-fs", "APFS", "-volname", "FJ2NAS", nasImg.path])
        guard let nas = await attachNAS(nasImg) else { FJ.log("cd2: NAS attach failed"); return }
        var nasAttached = true
        defer { if nasAttached { Task { _ = await nas.detach() } } }
        let archiveTarget = nas.mount.appendingPathComponent("Luxtable")
        FJ.log("cd2: NAS at \(nas.mount.path) targetExists=\(fm.fileExists(atPath: archiveTarget.path))")

        // --- The card.
        guard let img = await TestCardImage.make(in: root, name: "A9CARD2", sizeMB: 2600) else { FJ.log("cd2: no card"); return }
        var card = img
        defer { Task { _ = await E2EIntegrationTests.checkedDetach(card) } }
        let dcim = card.mount.appendingPathComponent("DCIM/100MSDCF")
        _ = try await Task.detached {
            try FJ.uniqueCopies(count: 40, into: dcim, name: { String(format: "DSC%05d.ARW", 2000 + $0) }, salt: "cd2")
        }.value
        let clips = card.mount.appendingPathComponent("PRIVATE/M4ROOT/CLIP")
        try fm.createDirectory(at: clips, withIntermediateDirectories: true)
        try await makeMP4(clips.appendingPathComponent("C0001.MP4"))
        try Data("junk".utf8).write(to: dcim.appendingPathComponent("JUNK.DAT"))
        try Data([0, 1, 2]).write(to: dcim.appendingPathComponent("._DSC02000.ARW"))
        try Data("x".utf8).write(to: card.mount.appendingPathComponent("PRIVATE/AVCHD.BDM"))
        guard let re = await card.remount() else { return }
        card = re
        let cardBefore = await FJ.listing(card.mount)

        // --- First run: onboarding with the missing archive folder.
        let prefs = "cardday2"
        let s0 = AppSettings(defaults: FJ.defaults(prefs))
        let ob = OnboardingModel(settings: s0)
        ob.sessionsFolder = sessionsDir
        ob.archiveTarget = archiveTarget.path
        await ob.refreshArchiveStatus()
        ob.amazonFolder = amazonDir
        FJ.log("cd2 onboarding: status=\(ob.archiveStatus) canFinish=\(ob.canFinish)")
        try ob.finish()
        FJ.log("cd2 onboarding: finish → targetExists=\(fm.fileExists(atPath: archiveTarget.path)) settings.archive='\(s0.archiveTarget)' enabled=\(s0.archiveEnabled) amazonBookmark=\(s0.amazonPhotosFolderBookmark != nil) archiveRejected=\(s0.archiveRejected)")
        // A second NAS target that is absent (not created by onboarding) → does the archive create it?
        try? fm.removeItem(at: archiveTarget)
        FJ.log("cd2: removed the target again to test the archive's own creation; exists=\(fm.fileExists(atPath: archiveTarget.path))")

        let m = FJ.liveModel(prefs: prefs, sessions: sessionsDir)
        await m.start()
        _ = await waitUntil(timeout: .seconds(5)) { m.archiveStatus.detail != nil }
        FJ.log("cd2 launch: archive='\(ArchiveDisplay.title(m.archiveStatus.state))' long='\(ArchiveDisplay.long(m.archiveStatus.state))'")

        // --- Import.
        let im = ImportModel(model: m, volume: card.mount, ejector: FakeEjector(),
                             inspector: StaticVolumeInspector([card.mount.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "CD2")]))
        await im.scan()
        FJ.log("cd2 import: candidates=\(im.candidates.count) videos=\(im.videoCount) skipped=\(im.skippedUnknown.map { "\($0.url.lastPathComponent): \($0.reason)" }) button='\(ImportWording.importButton(photos: im.selected.count - im.videoCount, videos: im.videoCount))'")
        im.sessionText = "Card Day 2"
        let i0 = Date()
        im.start()
        await im.wait()
        guard case .finished(let summary) = im.phase else { FJ.log("cd2 import FAILED \(im.phase)"); return }
        FJ.log(String(format: "cd2 import: %.1f s '%@' / '%@' / card='%@'", Date().timeIntervalSince(i0), ImportWording.doneTitle(summary), ImportWording.doneDetail(summary), ImportWording.cardLine(summary) ?? ""))
        _ = await waitUntil(timeout: .seconds(30)) { m.currentSession != nil && !m.isLoading }
        let session = try #require(m.currentSession)
        var mismatches = 0
        for p in m.photos where !p.isVideo {
            if let u = p.url, !fm.contentsEqual(atPath: u.path, andPath: dcim.appendingPathComponent(p.filename).path) { mismatches += 1 }
        }
        FJ.log("cd2 session: \(session.url.lastPathComponent) photos=\(m.photoCount) videos=\(m.photos.filter(\.isVideo).count) byteMismatches=\(mismatches) top=\((try? fm.contentsOfDirectory(atPath: session.url.path))?.sorted() ?? [])")
        await m.cull.refresh()
        FJ.log("cd2 cull.canMove=\(m.cull.canMove)")

        // --- AI Cull first (the user's "AI help"), then rate.
        let stills = m.visibleIDs.filter { m.photo($0)?.isVideo != true }
        let c0 = Date()
        await m.runAICull()
        let aiPicks = stills.filter { m.photo($0)?.pick == .picked }
        let aiRejects = stills.filter { m.photo($0)?.pick == .rejected }
        FJ.log(String(format: "cd2 aicull: %.1f s toast='%@' picks=%d rejects=%d", Date().timeIntervalSince(c0), m.toast?.message ?? "", aiPicks.count, aiRejects.count))
        m.settings.autoAdvance = true
        m.focus(stills.first, extend: false)
        var expected: [PhotoID: Int] = [:]
        for (i, id) in stills.enumerated() {
            if m.focusedPhoto?.id != id { m.focus(id, extend: false) }
            let want = [0, 1, 2, 3, 4, 5, 3, 5][i % 8]
            if want == 0 { expected[id] = m.photo(id)?.rating ?? 0; continue }
            m.perform(.rate(want))
            expected[id] = want
        }
        m.settings.autoAdvance = false
        FJ.log("cd2 ratings: \(stills.filter { m.photo($0)?.rating == expected[$0] }.count)/\(stills.count)")
        for n in 1...5 {
            m.showRatingFolder(n)
            FJ.log("cd2 ⌘\(n): shown=\(m.visibleIDs.count) expected=\(expected.values.filter { $0 == n }.count) exact=\(m.visibleIDs.allSatisfy { m.photo($0)?.rating == n }) badge=\(m.smartFolders.ratingFolder(n).flatMap { m.smartFolders.counts[$0.id] } ?? -1)")
        }
        m.selectSmartFolder(nil)
        // User rejects 2 more frames by hand.
        let userRejects = stills.filter { expected[$0] == 0 && m.photo($0)?.pick != .rejected }.prefix(2)
        for id in userRejects { m.focus(id, extend: false); m.perform(.reject) }
        // Picks: the 5-stars.
        for id in stills where expected[id] == 5 { m.focus(id, extend: false); if m.photo(id)?.pick != .picked { m.perform(.pick) } }
        await m.saver.flush()

        // --- Crop + WB on a few photos.
        let editIDs = Array(stills.filter { expected[$0] == 5 }.prefix(3))
        m.focus(editIDs[0], extend: false)
        m.perform(.tool(.crop))
        let toolNow = m.viewer.canvasTool
        m.setCropAspect(.r4x5)
        m.viewer.commitTool()
        m.updateEdit(label: "White Balance") { e in e.whiteBalance.asShot = false; e.whiteBalance.temperature = 5800; e.whiteBalance.tint = 4 }
        m.endEditGesture()
        let e0 = m.edit(for: editIDs[0])
        FJ.log("cd2 crop: tool after C=\(toolNow) rect=\(e0.crop.rect) aspect=\(e0.crop.aspect) wb=\(e0.whiteBalance.temperature)")
        m.focus(editIDs[1], extend: false)
        m.setCropAspect(.r16x9)
        m.updateEdit(label: "Straighten") { $0.crop.angle = 1.5 }
        m.endEditGesture()
        FJ.log("cd2 crop2: rect=\(m.edit(for: editIDs[1]).crop.rect) angle=\(m.edit(for: editIDs[1]).crop.angle)")

        // --- Copy (the toolbar's one click) from the first, apply to the picks.
        m.focus(editIDs[0], extend: false)
        m.perform(.copyAdjustments)
        FJ.log("cd2 copy ticked=\(m.settings.copyGroups.map(\.rawValue).sorted()) toast='\(m.toast?.message ?? "")'")
        let picks = stills.filter { m.photo($0)?.pick == .picked }
        m.selection = Set(picks)
        let applied = m.applyToSelection()
        #expect(await waitUntil(timeout: .seconds(10)) { picks.allSatisfy { m.edit(for: $0).whiteBalance.temperature == 5800 } })
        #expect(picks.filter { !editIDs.contains($0) }.allSatisfy { m.edit(for: $0).crop.rect == CGRect(x: 0, y: 0, width: 1, height: 1) },
                "the crop is per photo: not pasted unless ticked")
        let cropOK = picks.filter { let r = m.edit(for: $0).crop; return r.aspect == e0.crop.aspect && r.rect != CGRect(x: 0, y: 0, width: 1, height: 1) }.count
        FJ.log("cd2 apply: '\(m.clipboardSummary)' changed=\(applied) picks=\(picks.count) wb5800=\(picks.filter { m.edit(for: $0).whiteBalance.temperature == 5800 }.count) cropped4x5=\(cropOK) straightenKept(editIDs[1])=\(m.edit(for: editIDs[1]).crop.angle) toast='\(m.toast?.message ?? "")' undo='\(m.history.undoLabel ?? "")'")

        // --- ⌘K "warmer and a bit brighter" with the picks still selected.
        let target = picks.last!
        m.focus(target, extend: true)
        let beforeAll = Dictionary(uniqueKeysWithValues: picks.map { ($0, m.edit(for: $0)) })
        let depth = m.history.undoStack.count
        let t0 = Date()
        let toast = await m.runInstruction("warmer and a bit brighter")
        let changedIDs = picks.filter { m.edit(for: $0) != beforeAll[$0] }
        let a = m.edit(for: target), b = beforeAll[target]!
        FJ.log(String(format: "cd2 ⌘K: %.0f ms toast='%@' selected=%d changed=%d (focused only? %@) K %.0f→%.0f EV %.2f→%.2f undoSteps=%d",
                      Date().timeIntervalSince(t0) * 1000, toast?.message ?? "", m.selectedIDs.count, changedIDs.count, changedIDs == [target] ? "yes" : "no",
                      b.whiteBalance.temperature, a.whiteBalance.temperature, b.exposure.ev, a.exposure.ev, m.history.undoStack.count - depth))
        m.perform(.undo)
        FJ.log("cd2 ⌘K undo: restored=\(m.edit(for: target) == b)")
        await m.runInstruction("warmer and a bit brighter")
        await m.saver.flush()

        // --- Rejects to the session Trash.
        let rejectedBefore = stills.filter { m.photo($0)?.pick == .rejected }
        let rejectNames = rejectedBefore.compactMap { m.photo($0)?.filename }
        await m.cull.moveRejectedToTrash()
        let trashDir = session.url.appendingPathComponent("Trash")
        let inTrash = (try? fm.contentsOfDirectory(atPath: trashDir.path))?.filter { !$0.hasPrefix(".") } ?? []
        let stillInCapture = rejectNames.filter { fm.fileExists(atPath: session.url.appendingPathComponent("Capture/\($0)").path) }
        FJ.log("cd2 trash: rejected=\(rejectNames.count) toast='\(m.toast?.message ?? "")' onDiskTrash=\(inTrash.count) stillInCapture=\(stillInCapture.count) visible=\(m.visibleIDs.count) photos=\(m.photos.count)")
        // Picks (and rated) to Selects.
        m.showRatingFolder(5)
        let fiveCount = m.visibleIDs.count
        let selectIDs = m.cull.selectIDs.count
        await m.cull.movePickedToSelects()
        let selectsDir = session.url.appendingPathComponent("Selects")
        let inSelects = (try? fm.contentsOfDirectory(atPath: selectsDir.path))?.filter { $0.hasSuffix(".ARW") } ?? []
        FJ.log("cd2 selects: while in ⌘5 (\(fiveCount) shown) the command targets \(selectIDs); toast='\(m.toast?.message ?? "")' onDisk=\(inSelects.count) sidecarsFollow=\(inSelects.allSatisfy { fm.fileExists(atPath: selectsDir.appendingPathComponent(".lt/settings/\($0).json").path) })")
        m.selectSmartFolder(nil)
        // Edits still attached after the move?
        let e0After = m.edit(for: editIDs[0])
        FJ.log("cd2 selects: edit of first still there: wb=\(e0After.whiteBalance.temperature) aspect=\(e0After.crop.aspect) url=\(m.photo(editIDs[0])?.url?.path.replacingOccurrences(of: session.url.path, with: "") ?? "nil")")

        // --- Export picks.
        let picksNow = m.visibleIDs.filter { m.photo($0)?.pick == .picked }
        m.selection = Set(picksNow)
        m.beginExport()
        let ex = try #require(m.exportModel)
        _ = await waitUntil(timeout: .seconds(10)) { ex.destinationReady }
        FJ.log("cd2 export: items=\(ex.items.count) folder=\(ex.defaultFolder.path.replacingOccurrences(of: root.path, with: "<root>")) recipes=\(ex.recipes.map { "\($0.name)\($0.enabled ? "*" : "")" }) note=\(ex.problem ?? "nil")")
        if ex.defaultFolder.standardizedFileURL.path.hasPrefix(FJ.root.standardizedFileURL.path) { // exports only into the sandbox
            let x0 = Date()
            ex.start()
            await ex.waitUntilDone()
            if case .finished(let o) = ex.phase {
                let files = FJ.files(under: ex.defaultFolder)
                let cropped = o.written.first { $0.photoID == editIDs[0] }
                FJ.log(String(format: "cd2 export: %.1f s '%@' onDisk=%d first=%@ %@", Date().timeIntervalSince(x0), ExportModel.summary(o), files.count,
                              cropped.map { "\($0.url.lastPathComponent) \($0.width)x\($0.height) disk=\(dims($0.url))" } ?? "nil", files.prefix(3).description))
            } else { FJ.log("cd2 export: \(ex.phase)") }
        } else { FJ.log("cd2 export: SKIPPED, folder outside scratch") }
        m.endExport()

        // --- Archive + Amazon.
        await m.archive.syncNow()
        let w0 = Date()
        let settled = await waitUntil(timeout: .seconds(240)) {
            ArchiveDisplay.title(m.archiveStatus.state) == "Archive up to date"
                && (m.archiveStatus.detail?.amazon.map { !$0.running && $0.lastRun != nil && $0.waiting == 0 } ?? true)
        }
        FJ.log(String(format: "cd2 archive: settled=%@ after %.1f s", settled ? "yes" : "NO", Date().timeIntervalSince(w0)))
        let st = m.archiveStatus
        FJ.log("cd2 archive: '\(ArchiveDisplay.title(st.state))' / '\(ArchiveDisplay.long(st.state))' runs=\(st.detail?.runs.map(ArchiveDisplay.runLine) ?? []) amazon='\(st.detail?.amazon.map(ArchiveDisplay.amazonLine) ?? "nil")'")
        let archived = FJ.files(under: archiveTarget)
        let aARW = archived.filter { $0.hasSuffix(".ARW") }
        let rejArchived = rejectNames.filter { n in aARW.contains { $0.hasSuffix("/" + n) } }
        FJ.log("cd2 archive fs: targetCreated=\(fm.fileExists(atPath: archiveTarget.path)) files=\(archived.count) ARW=\(aARW.count) (Capture \(aARW.filter { $0.contains("/Capture/") }.count), Selects \(aARW.filter { $0.contains("/Selects/") }.count), Trash \(aARW.filter { $0.contains("/Trash/") }.count)) mp4=\(archived.filter { $0.hasSuffix(".MP4") }.count) jpg=\(archived.filter { $0.lowercased().hasSuffix(".jpg") }.count) rejectsArchived=\(rejArchived.count)/\(rejectNames.count) cache=\(archived.filter { $0.contains("/cache") }.count)")
        let amazon = FJ.files(under: amazonDir).filter { !$0.hasPrefix(".") }
        let rejAmazon = rejectNames.filter { n in amazon.contains { $0.hasSuffix(n) } }
        FJ.log("cd2 amazon fs: \(amazon.count) files ARW=\(amazon.filter { $0.hasSuffix(".ARW") }.count) rejectsMirrored=\(rejAmazon.count)/\(rejectNames.count) sample=\(amazon.prefix(2))")
        FJ.log("cd2 archive settings: '\(ArchiveSettingsModel(settings: m.settings).amazonText)'")

        // --- Eject, then relaunch.
        await im.ejectNow()
        if im.confirmingEject { await im.confirmEject() }
        FJ.log("cd2 card untouched: \(FJ.diff(cardBefore, await FJ.listing(card.mount)))")
        let ratingsByName = Dictionary(uniqueKeysWithValues: m.photos.map { ($0.filename + $0.variantName, $0.rating) })
        let picksByName = Dictionary(uniqueKeysWithValues: m.photos.map { ($0.filename + $0.variantName, $0.pick) })
        let editsByName = Dictionary(uniqueKeysWithValues: m.edits.compactMap { id, e in m.photo(id).map { ($0.filename + $0.variantName, e) } })
        let placeByName = Dictionary(uniqueKeysWithValues: m.photos.map { ($0.filename, "\($0.inTrash ? "T" : "")\($0.inSelects ? "S" : "")") })
        await m.saver.flush()
        FJ.log("cd2 quit: \(await m.quitDecision(timeout: .seconds(5)))")
        await m.shutdownArchive()
        m.stop()

        let r = FJ.liveModel(prefs: prefs, sessions: sessionsDir)
        await r.start()
        _ = await waitUntil(timeout: .seconds(30)) { r.currentSession != nil && !r.isLoading }
        let key: (Photo) -> String = { $0.filename + $0.variantName }
        FJ.log("cd2 relaunch: current='\(r.currentSession?.name ?? "nil")' photos=\(r.photos.count) visible=\(r.visibleIDs.count) ratings \(r.photos.filter { ratingsByName[key($0)] == $0.rating }.count)/\(ratingsByName.count) picks \(r.photos.filter { picksByName[key($0)] == $0.pick }.count)/\(picksByName.count) edits \(editsByName.filter { n, e in r.photos.first { key($0) == n }.map { r.edit(for: $0.id) == e } ?? false }.count)/\(editsByName.count) placement \(r.photos.filter { placeByName[$0.filename] == "\($0.inTrash ? "T" : "")\($0.inSelects ? "S" : "")" }.count)/\(placeByName.count)")
        for n in 1...5 {
            FJ.log("cd2 relaunch ⌘\(n) badge=\(r.smartFolders.ratingFolder(n).flatMap { r.smartFolders.counts[$0.id] } ?? -1) expected(visible)=\(r.photos.filter { $0.rating == n && !$0.inTrash }.count)")
        }
        _ = await waitUntil(timeout: .seconds(10)) { r.archiveStatus.detail != nil }
        FJ.log("cd2 relaunch archive: '\(ArchiveDisplay.title(r.archiveStatus.state))' / '\(ArchiveDisplay.long(r.archiveStatus.state))'")
        await r.cull.refresh()
        FJ.log("cd2 relaunch trash: trashed=\(r.cull.trashedIDs.count) rejectedFileCount=\(r.cull.rejectedFileCount)")
        let fin = FJ.files(under: archiveTarget).filter { $0.hasSuffix(".ARW") }
        FJ.log("cd2 relaunch NAS fs: ARW=\(fin.count) Capture=\(fin.filter { $0.contains("/Capture/") }.count) Selects=\(fin.filter { $0.contains("/Selects/") }.count) Trash=\(fin.filter { $0.contains("/Trash/") }.count) jpg(Output)=\(FJ.files(under: archiveTarget).filter { $0.hasSuffix(".jpg") }.count) top=\(FJ.files(under: archiveTarget).map { String($0.split(separator: "/").prefix(2).joined(separator: "/")) }.reduce(into: Set<String>()) { $0.insert($1) }.sorted())")
        await r.shutdownArchive()
        r.stop()

        // --- NAS goes away: nothing may be created at the bare mount path.
        nasAttached = false
        let det = await nas.detach()
        let m2 = FJ.liveModel(prefs: prefs, sessions: sessionsDir)
        await m2.start()
        _ = await waitUntil(timeout: .seconds(20)) { m2.currentSession != nil && !m2.isLoading }
        if let id = m2.visibleIDs.first { m2.focus(id, extend: false); m2.setRating(2) }
        await m2.saver.flush()
        await m2.archive.syncNow()
        try? await Task.sleep(for: .seconds(4))
        FJ.log("cd2 NAS offline: detached=\(det) archive='\(ArchiveDisplay.title(m2.archiveStatus.state))' / '\(ArchiveDisplay.long(m2.archiveStatus.state))' bareMountDirExists=\(fm.fileExists(atPath: nas.mount.path))")
        await m2.shutdownArchive()
        m2.stop()
    }
}

extension FJ2CardDayTests {
    /// A1: NAS offline on card day; AI Cull + manual rejects before the first archive run.
    @Test func rejectsBeforeFirstArchive() async throws {
        guard FJ.sandboxed(), sampleARWs().count >= 8 else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("rejects-first")
        try? fm.removeItem(at: root)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        let nasImg = root.appendingPathComponent("FJ2NASB.dmg")
        _ = await sh(["/usr/bin/hdiutil", "create", "-quiet", "-size", "800m", "-fs", "APFS", "-volname", "FJ2NASB", nasImg.path])
        let target = "/Volumes/FJ2NASB/Luxtable"
        let amazonDir = root.appendingPathComponent("Amazon")
        try fm.createDirectory(at: amazonDir, withIntermediateDirectories: true)
        let sessions = root.appendingPathComponent("Sessions")
        let m = FJ.liveModel(prefs: "rejects-first", sessions: sessions) { s in
            s.onboardingComplete = true; s.archiveTarget = target; s.archiveEnabled = true; s.chooseAmazonFolder(amazonDir); s.autoAdvance = false
        }
        await m.start()
        let cardDir = root.appendingPathComponent("CARD")
        _ = try FJ.uniqueCopies(count: 8, into: cardDir.appendingPathComponent("DCIM/100MSDCF"), name: { String(format: "DSC%05d.ARW", 6100 + $0) }, clone: true, salt: "rf")
        let im = ImportModel(model: m, volume: cardDir, ejector: FakeEjector(),
                             inspector: StaticVolumeInspector([cardDir.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: false, isRemovable: false, uuid: "RF")]))
        await im.scan(); im.sessionText = "Offline Day"; im.start(); await im.wait()
        _ = await waitUntil(timeout: .seconds(30)) { m.currentSession != nil && !m.isLoading }
        let ids = m.visibleIDs
        for id in ids.prefix(3) { m.focus(id, extend: false); m.perform(.reject) }
        await m.saver.flush()
        try? await Task.sleep(for: .seconds(2))
        FJ.log("rf: offline archive='\(ArchiveDisplay.title(m.archiveStatus.state))' amazon='\(m.archiveStatus.detail?.amazon.map(ArchiveDisplay.amazonLine) ?? "nil")'")
        // NAS comes back.
        guard let nas = await attachNAS(nasImg) else { return }
        defer { Task { _ = await nas.detach() } }
        await m.archive.syncNow()
        _ = await waitUntil(timeout: .seconds(120)) {
            ArchiveDisplay.title(m.archiveStatus.state) == "Archive up to date"
                && (m.archiveStatus.detail?.amazon.map { !$0.running && $0.lastRun != nil } ?? true)
        }
        let arw = FJ.files(under: URL(fileURLWithPath: target)).filter { $0.hasSuffix(".ARW") }
        let amz = FJ.files(under: amazonDir).filter { $0.hasSuffix(".ARW") }
        FJ.log("rf: back online → '\(ArchiveDisplay.title(m.archiveStatus.state))' / '\(ArchiveDisplay.long(m.archiveStatus.state))' amazon='\(m.archiveStatus.detail?.amazon.map(ArchiveDisplay.amazonLine) ?? "nil")' sessionLines=\(m.archiveStatus.detail?.sessions.map { ArchiveDisplay.sessionLine($0) } ?? []) NAS ARW=\(arw.count)/8 Amazon ARW=\(amz.count)/8 (3 rejected, not trashed)")
        await m.shutdownArchive()
        m.stop()
    }
}

extension FJ2CardDayTests {
    /// The archive fails (NAS full); after a relaunch, what does the status say?
    @Test func archiveFailureAcrossRelaunch() async throws {
        guard FJ.sandboxed(), sampleARWs().count >= 8 else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("nasfull")
        try? fm.removeItem(at: root)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        let nasImg = root.appendingPathComponent("FJ2NASC.dmg")
        _ = await sh(["/usr/bin/hdiutil", "create", "-quiet", "-size", "300m", "-fs", "APFS", "-volname", "FJ2NASC", nasImg.path])
        guard let nas = await attachNAS(nasImg) else { return }
        let target = nas.mount.appendingPathComponent("Luxtable").path
        let sessions = root.appendingPathComponent("Sessions")
        let amazonDir = root.appendingPathComponent("Amazon")
        try fm.createDirectory(at: amazonDir, withIntermediateDirectories: true)
        let m = FJ.liveModel(prefs: "nasfull", sessions: sessions) { s in
            s.onboardingComplete = true; s.archiveTarget = target; s.archiveEnabled = true; s.chooseAmazonFolder(amazonDir)
        }
        await m.start()
        let cardDir = root.appendingPathComponent("CARD")
        _ = try FJ.uniqueCopies(count: 10, into: cardDir.appendingPathComponent("DCIM/100MSDCF"), name: { String(format: "DSC%05d.ARW", 6300 + $0) }, clone: true, salt: "nf")
        let im = ImportModel(model: m, volume: cardDir, ejector: FakeEjector(),
                             inspector: StaticVolumeInspector([cardDir.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: false, isRemovable: false, uuid: "NF")]))
        await im.scan(); im.sessionText = "Full NAS"; im.start(); await im.wait()
        _ = await waitUntil(timeout: .seconds(30)) { m.currentSession != nil && !m.isLoading }
        await m.archive.syncNow()
        _ = await waitUntil(timeout: .seconds(90)) { m.archiveStatus.detail?.runs.contains { $0.outcome != "succeeded" } ?? false }
        try? await Task.sleep(for: .seconds(5))
        FJ.log("nasfull: '\(ArchiveDisplay.title(m.archiveStatus.state).prefix(160))' amazon='\(m.archiveStatus.detail?.amazon.map(ArchiveDisplay.amazonLine) ?? "nil")' amazonFiles=\(FJ.files(under: amazonDir).filter { $0.hasSuffix(".ARW") }.count) NAS ARW=\(FJ.files(under: URL(fileURLWithPath: target)).filter { $0.hasSuffix(".ARW") }.count)/10")
        let q = await m.quitDecision(timeout: .seconds(5))
        FJ.log("nasfull: quit decision with a failed archive = \(q)")
        await m.shutdownArchive()
        m.stop()
        let r = FJ.liveModel(prefs: "nasfull", sessions: sessions)
        await r.start()
        _ = await waitUntil(timeout: .seconds(30)) { r.currentSession != nil && !r.isLoading }
        for s in [1, 5, 15, 30] {
            try? await Task.sleep(for: .seconds(s == 1 ? 1 : (s == 5 ? 4 : (s == 15 ? 10 : 15))))
            FJ.log("nasfull relaunch t≈\(s)s: '\(ArchiveDisplay.title(r.archiveStatus.state).prefix(140))' / sessions=\(r.archiveStatus.detail?.sessions.map { ArchiveDisplay.sessionLine($0) } ?? []) lastSuccess=\(String(describing: r.archiveStatus.lastSuccess))")
        }
        await r.archive.syncNow()
        try? await Task.sleep(for: .seconds(8))
        FJ.log("nasfull relaunch after Sync Now: '\(ArchiveDisplay.title(r.archiveStatus.state).prefix(160))'")
        await r.shutdownArchive()
        r.stop()
        _ = await nas.detach()
    }
}

extension FJ2CardDayTests {
    /// After a failed run and a relaunch (1-minute interval), is the session ever retried on its own?
    @Test func archiveRetryAfterRelaunch() async throws {
        guard FJ.sandboxed(), sampleARWs().count >= 8 else { return }
        let fm = FileManager.default
        let root = FJ.root.appendingPathComponent("retry")
        try? fm.removeItem(at: root)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        let nasImg = root.appendingPathComponent("FJ2NASD.dmg")
        _ = await sh(["/usr/bin/hdiutil", "create", "-quiet", "-size", "800m", "-fs", "APFS", "-volname", "FJ2NASD", nasImg.path])
        let target = "/Volumes/FJ2NASD/Luxtable"
        let sessions = root.appendingPathComponent("Sessions")
        let m = FJ.liveModel(prefs: "retry", sessions: sessions) { s in
            s.onboardingComplete = true; s.archiveTarget = target; s.archiveEnabled = true; s.archiveIntervalMinutes = 1
        }
        await m.start()
        let cardDir = root.appendingPathComponent("CARD")
        _ = try FJ.uniqueCopies(count: 4, into: cardDir.appendingPathComponent("DCIM/100MSDCF"), name: { String(format: "DSC%05d.ARW", 6400 + $0) }, clone: true, salt: "rt")
        let im = ImportModel(model: m, volume: cardDir, ejector: FakeEjector(),
                             inspector: StaticVolumeInspector([cardDir.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: false, isRemovable: false, uuid: "RT")]))
        await im.scan(); im.sessionText = "Retry"; im.start(); await im.wait()
        _ = await waitUntil(timeout: .seconds(30)) { m.currentSession != nil && !m.isLoading }
        await m.archive.syncNow()
        try? await Task.sleep(for: .seconds(3))
        FJ.log("retry: NAS unmounted → '\(ArchiveDisplay.title(m.archiveStatus.state))'")
        await m.shutdownArchive()
        m.stop()
        // NAS comes back while the app is closed; relaunch.
        guard let nas = await attachNAS(nasImg) else { return }
        let r = FJ.liveModel(prefs: "retry", sessions: sessions)
        await r.start()
        _ = await waitUntil(timeout: .seconds(30)) { r.currentSession != nil && !r.isLoading }
        for t in [5, 30, 75, 100] {
            try? await Task.sleep(for: .seconds(t == 5 ? 5 : (t == 30 ? 25 : (t == 75 ? 45 : 25))))
            let arw = FJ.files(under: URL(fileURLWithPath: target)).filter { $0.hasSuffix(".ARW") }.count
            FJ.log("retry relaunch t≈\(t)s: '\(ArchiveDisplay.title(r.archiveStatus.state))' sessions=\(r.archiveStatus.detail?.sessions.map { ArchiveDisplay.sessionLine($0) } ?? []) NAS ARW=\(arw)/4")
        }
        await r.shutdownArchive()
        r.stop()
        _ = await nas.detach()
    }
}
