import CryptoKit
import Foundation
import SQLite3
import Testing
@testable import DarkslideKit

/// A scripted importer: dry runs return `preview`, real runs `result`; records each call.
private actor FakeImporter: CaptureOneImporting {
    var preview: Result<C1ImportSummary, Error>
    var result: Result<C1ImportSummary, Error>
    var reverted: Result<C1RevertSummary?, Error> = .success(C1RevertSummary(restored: 2))
    var isC1 = true
    var revertible = false
    private(set) var calls: [(dryRun: Bool, overwrite: Bool)] = []
    private(set) var reverts = 0

    init(preview: Result<C1ImportSummary, Error>, result: Result<C1ImportSummary, Error> = .success(C1ImportSummary(dryRun: false))) {
        self.preview = preview
        self.result = result
    }

    func set(isC1: Bool? = nil, revertible: Bool? = nil, reverted: Result<C1RevertSummary?, Error>? = nil) {
        if let isC1 { self.isC1 = isC1 }
        if let revertible { self.revertible = revertible }
        if let reverted { self.reverted = reverted }
    }

    func isCaptureOneFolder(_ folder: URL) async -> Bool { isC1 }

    func importCaptureOne(_ session: SessionInfo, dryRun: Bool, overwriteEdits: Bool) async throws -> C1ImportSummary {
        calls.append((dryRun, overwriteEdits))
        let r = try (dryRun ? preview : result).get()
        if !dryRun { revertible = r.changeCount > 0 }
        return r
    }

    func canRevertCaptureOneImport(_ session: SessionInfo) async -> Bool { revertible }

    func revertCaptureOneImport(_ session: SessionInfo) async throws -> C1RevertSummary? {
        reverts += 1
        revertible = false
        return try reverted.get()
    }
}

/// The mock store plus a scripted Capture One importer (detection, import, revert).
private struct C1Store: SessionStore, CaptureOneImporting {
    let inner = MockSessionStore(counts: [3])
    let c1: FakeImporter
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func isCaptureOneFolder(_ folder: URL) async -> Bool { await c1.isCaptureOneFolder(folder) }
    func importCaptureOne(_ session: SessionInfo, dryRun: Bool, overwriteEdits: Bool) async throws -> C1ImportSummary {
        try await c1.importCaptureOne(session, dryRun: dryRun, overwriteEdits: overwriteEdits)
    }
    func canRevertCaptureOneImport(_ session: SessionInfo) async -> Bool { await c1.canRevertCaptureOneImport(session) }
    func revertCaptureOneImport(_ session: SessionInfo) async throws -> C1RevertSummary? {
        try await c1.revertCaptureOneImport(session)
    }
}

private struct Boom: LocalizedError {
    var errorDescription: String? { "the database is unreadable" }
}

private let session = SessionInfo(name: "Shoot", url: URL(fileURLWithPath: "/tmp/Shoot"), photoCount: 3, createdAt: Date())

@Suite struct CaptureOneSummaryTests {
    @Test func linesCountWhatIsWrittenAndSkipZeros() {
        var s = C1ImportSummary(variantsCreated: 0, rated: 140, tagged: 1, editsMapped: 151)
        #expect(s.lines == ["140 photos will be rated", "1 photo will be colour-tagged", "151 photos will be given Capture One's edits"])
        s.dryRun = false
        s.picked = 2
        s.keyworded = 3
        s.variantsCreated = 1
        s.albumsCreated = 1
        s.albumsUpdated = 1
        #expect(s.lines.contains("2 photos were picked or unpicked (Capture One's Selects folder)"))
        #expect(s.lines.contains("3 photos were given keywords"))
        #expect(s.lines.contains("1 Capture One variant was added as new variants"))
        #expect(s.lines.contains("2 albums were created or updated"))
        #expect(s.changeCount == 140 + 1 + 151 + 2 + 3 + 1 + 2)
        #expect(C1ImportSummary(variantsCreated: 2).lines.first == "2 Capture One variants will be added as new variants")
        #expect(C1ImportSummary(albumsCreated: 1).lines.first == "1 album will be created or updated")
    }

    @Test func nothingToDoAndConflictsOnly() {
        #expect(C1ImportSummary().lines == ["Everything is already up to date."])
        #expect(!C1ImportSummary().hasWork)
        let c = C1ImportSummary.Conflict(photo: PhotoID(7), file: "a.ARW", field: "rating")
        let d = C1ImportSummary.Conflict(photo: PhotoID(7), file: "a.ARW", field: "adjustments")
        let s = C1ImportSummary(conflicts: [c, d])
        #expect(s.hasWork && s.conflictPhotos == 1)
        #expect(s.lines == ["Nothing new apart from the conflicts below."])
    }

    @Test func unmappedAndLeftOutExplainThemselves() {
        let s = C1ImportSummary(
            notFound: ["x.ARW", "y.ARW", "z.ARW", "w.ARW"], outsideFolder: 2114, inTrash: 685,
            unmapped: [.init(key: "Clarity", reason: "Clarity: no equivalent", count: 149)],
            keptDarkslide: 1, deletedInDarkslide: 2, readOnly: 1,
            skipped: [.init(name: "Five Stars", reason: "already in Darkslide")], warnings: ["w"])
        #expect(s.notMappedLines == ["Clarity: 149 photos — Clarity: no equivalent"])
        #expect(s.leftOutLines == [
            "685 photos in Capture One's Trash (not listed in Darkslide)",
            "2114 photos outside this folder (other folders the Capture One session references)",
            "4 photos not found in Darkslide: x.ARW, y.ARW, z.ARW and 1 more",
            "2 variants deleted in Darkslide (not recreated)",
            "1 photo saved by a newer Darkslide (left alone)",
            "1 Darkslide change kept (unchanged in Capture One since the last import)",
            "\u{201c}Five Stars\u{201d}: already in Darkslide",
            "w",
        ])
        #expect(C1ImportSummary(keptDarkslide: 2, deletedInDarkslide: 1).leftOutLines
            == ["1 variant deleted in Darkslide (not recreated)", "2 Darkslide changes kept (unchanged in Capture One since the last import)"])
        #expect(C1ImportSummary.sample(["a", "b"]) == "a, b")
    }

    @Test func toolsConflictsAndRevertMessages() {
        let s = C1ImportSummary(mapped: [
            .init(key: "Exposure", count: 149), .init(key: "UsmAmount", count: 140), .init(key: "UsmRadius", count: 3),
            .init(key: "GradationCurve", count: 54), .init(key: "HighlightRecoveryEx", count: 149),
            .init(key: "WhiteBalance", count: 149), .init(key: "Rotation", count: 29), .init(key: "Crop", count: 30),
            .init(key: "NrAmount", count: 2), .init(key: "TargetShadow", count: 1), .init(key: "ChromaticAberEnabled", count: 1),
            .init(key: "ClarityStructure", count: 1), .init(key: "SomethingNew", count: 1),
        ])
        #expect(s.toolLines == [
            "Exposure: 149 photos", "HDR highlights: 149 photos", "White balance: 149 photos",
            "Sharpening: 140 photos (approximate)", "Curve: 54 photos (approximate)", "Crop: 30 photos (approximate)",
            "Rotation and straighten: 29 photos", "Noise reduction: 2 photos (approximate)",
            "Chromatic aberration: 1 photo", "Levels: 1 photo (approximate)", "SomethingNew: 1 photo",
            "Structure: 1 photo (approximate)",
        ])
        for (key, tool) in [("Contrast", "Contrast"), ("ShadowRecovery", "HDR shadows"), ("WhiteRecovery", "HDR whites"),
                            ("BlackRecovery", "HDR blacks"), ("Clarity", "Clarity"), ("DehazeAmount", "Dehaze"),
                            ("Vignetting", "Vignetting"), ("WhiteBalanceTint", "White balance"), ("GradationCurveRed", "Curve")] {
            #expect(C1ImportSummary.tool(for: key) == tool)
        }
        let conflicts = (1...10).flatMap { i in
            [C1ImportSummary.Conflict(photo: PhotoID(Int64(i)), file: "\(i).ARW", field: "color_tag"),
             C1ImportSummary.Conflict(photo: PhotoID(Int64(i)), file: "\(i).ARW", field: "adjustments")]
        }
        let c = C1ImportSummary(conflicts: conflicts).conflictLines
        #expect(c.count == 9 && c[0] == "1.ARW: colour tag, edits" && c.last == "and 2 photos more")
        #expect(C1ImportSummary(conflicts: [.init(photo: PhotoID(1), file: "a.ARW", field: "rating")]).conflictLines == ["a.ARW: rating"])
        #expect(C1RevertSummary(restored: 1).message == "Capture One import reverted: 1 photo restored")
        #expect(C1RevertSummary(restored: 3, kept: 2, variantsRemoved: 1, albumsRemoved: 1, albumsTrimmed: 1).message
            == "Capture One import reverted: 3 photos restored, 1 imported variant removed, 2 albums undone, 2 later Darkslide changes kept")
        #expect(C1RevertSummary(kept: 1, variantsRemoved: 2, albumsRemoved: 1).message
            == "Capture One import reverted: 0 photos restored, 2 imported variants removed, 1 album undone, 1 later Darkslide change kept")
    }
}

@MainActor @Suite struct CaptureOneImportModelTests {
    @Test func previewThenImportKeepingEdits() async {
        let fake = FakeImporter(preview: .success(C1ImportSummary(rated: 3)), result: .success(C1ImportSummary(dryRun: false, rated: 3)))
        var reloaded = 0
        let m = CaptureOneImportModel(session: session, importer: fake, offered: false) { reloaded += 1 }
        #expect(m.title == "Import Capture One Edits")
        #expect(m.phase == .previewing && !m.canImport)
        await m.loadPreview()
        #expect(m.preview?.rated == 3 && m.canImport)
        await m.run()
        #expect(m.phase == .done(C1ImportSummary(dryRun: false, rated: 3)))
        #expect(reloaded == 1)
        let calls = await fake.calls
        #expect(calls.map(\.dryRun) == [true, false] && calls.map(\.overwrite) == [false, false])
        #expect(m == m && m != CaptureOneImportModel(session: session, importer: fake, offered: false) {})
    }

    @Test func pendingEditsAreSavedBeforeTheDryRunAndTheImport() async {
        let fake = FakeImporter(preview: .success(C1ImportSummary(rated: 1)), result: .success(C1ImportSummary(dryRun: false, rated: 1)))
        var flushes = 0
        var callsAtFlush: [Int] = []
        let m = CaptureOneImportModel(session: session, importer: fake, offered: true, flush: {
            flushes += 1
            callsAtFlush.append(await fake.calls.count)
        }) {}
        await m.loadPreview()
        await m.run()
        #expect(flushes == 2 && callsAtFlush == [0, 1], "each flush comes before its import call")
    }

    @Test func theRevertHintCoversEveryImportSinceTheLastRevert() {
        #expect(CaptureOneImportModel.revertHint.contains("every Capture One import since the last revert"))
    }

    @Test func overwriteIsPassedThroughAndNothingToDoDoesNothing() async {
        let conflict = C1ImportSummary.Conflict(photo: PhotoID(1), file: "a.ARW", field: "rating")
        let fake = FakeImporter(preview: .success(C1ImportSummary(conflicts: [conflict])))
        let m = CaptureOneImportModel(session: session, importer: fake, offered: true) {}
        #expect(m.title == "\u{201c}Shoot\u{201d} is a Capture One session")
        await m.loadPreview()
        #expect(!m.canImport, "conflicts only, overwriting off: nothing to write")
        await m.run()
        #expect(await fake.calls.count == 1)
        m.overwriteEdits = true
        #expect(m.canImport)
        await m.run()
        #expect(await fake.calls.last?.overwrite == true)

        let idle = FakeImporter(preview: .success(C1ImportSummary()))
        let m2 = CaptureOneImportModel(session: session, importer: idle, offered: false) {}
        await m2.loadPreview()
        #expect(!m2.canImport)
        await m2.run()
        #expect(await idle.calls.count == 1, "no real run without anything to import")
    }

    @Test func failuresAreShownAndAFailedImportStillReloads() async {
        let bad = FakeImporter(preview: .failure(Boom()))
        let m = CaptureOneImportModel(session: session, importer: bad, offered: false) {}
        await m.loadPreview()
        #expect(m.phase == .failed("the database is unreadable"))
        #expect(m.preview == nil)

        let cancelled = FakeImporter(preview: .failure(CancellationError()))
        let c = CaptureOneImportModel(session: session, importer: cancelled, offered: false) {}
        await c.loadPreview()
        #expect(c.phase == .failed("Cancelled"))

        let late = FakeImporter(preview: .success(C1ImportSummary(rated: 1)), result: .failure(Boom()))
        var reloaded = 0
        let l = CaptureOneImportModel(session: session, importer: late, offered: false) { reloaded += 1 }
        await l.loadPreview()
        await l.run()
        #expect(l.phase == .failed("the database is unreadable") && reloaded == 1)
    }

    @Test func demoModeHasNoImport() async {
        let m = makeModel()
        #expect(!m.canImportCaptureOne && !m.canRevertCaptureOne)
        m.showCaptureOneImport()
        #expect(m.sheet == nil)
        await m.refreshCaptureOne()
        #expect(m.captureOne == CaptureOneStatus())
        await m.revertCaptureOneImport()
        #expect(m.toast == nil)
    }

    @Test func revertFromTheResultThenNothingLeft() async {
        let fake = FakeImporter(preview: .success(C1ImportSummary(rated: 2)), result: .success(C1ImportSummary(dryRun: false, rated: 2)))
        var reloaded = 0
        let m = CaptureOneImportModel(session: session, importer: fake, offered: false) { reloaded += 1 }
        #expect(!m.canRevert)
        await m.revert()
        #expect(await fake.reverts == 0, "nothing imported yet")
        await m.loadPreview()
        await m.run()
        #expect(m.canRevert)
        await m.revert()
        #expect(m.phase == .reverted(C1RevertSummary(restored: 2)) && reloaded == 2 && !m.canRevert)

        // An import that wrote nothing has nothing to revert; a failing revert is shown (and reloads).
        let none = FakeImporter(preview: .success(C1ImportSummary(rated: 1)), result: .success(C1ImportSummary(dryRun: false, inSync: 4)))
        let n = CaptureOneImportModel(session: session, importer: none, offered: false) {}
        await n.loadPreview()
        await n.run()
        #expect(!n.canRevert)
        let bad = FakeImporter(preview: .success(C1ImportSummary(rated: 1)), result: .success(C1ImportSummary(dryRun: false, overwritten: 1)))
        await bad.set(reverted: .failure(Boom()))
        var after = 0
        let b = CaptureOneImportModel(session: session, importer: bad, offered: false) { after += 1 }
        await b.loadPreview()
        await b.run()
        #expect(b.canRevert)
        await b.revert()
        #expect(b.phase == .failed("the database is unreadable") && after == 2)
        let gone = FakeImporter(preview: .success(C1ImportSummary(rated: 1)), result: .success(C1ImportSummary(dryRun: false, rated: 1)))
        await gone.set(reverted: .success(nil))
        let g = CaptureOneImportModel(session: session, importer: gone, offered: false) {}
        await g.loadPreview()
        await g.run()
        await g.revert()
        #expect(g.phase == .reverted(C1RevertSummary()))
    }

    @Test func detectionOfferMenuAndRevertCommand() async throws {
        let fake = FakeImporter(preview: .success(C1ImportSummary(rated: 3)), result: .success(C1ImportSummary(dryRun: false, rated: 3)))
        let m = makeModel(store: C1Store(c1: fake))
        await m.start()
        let s = try #require(m.currentSession)
        #expect(m.captureOne == CaptureOneStatus(session: s.id, isSession: true, canRevert: false))
        #expect(m.canImportCaptureOne && !m.canRevertCaptureOne)

        // The offer after opening: a dry run first, then the sheet.
        await m.offerCaptureOneImport(after: s.url)
        guard case .captureOneImport(let sheet)? = m.sheet else {
            Issue.record("no offer")
            return
        }
        #expect(sheet.offered && sheet.preview?.rated == 3)
        await sheet.run()
        #expect(m.canRevertCaptureOne, "the import wrote something: File ▸ Revert is enabled")
        m.sheet = nil
        await m.offerCaptureOneImport(after: s.url)
        #expect(m.sheet == nil, "offered once per folder per launch")

        // File ▸ Revert asks first.
        m.requestRevertCaptureOne()
        #expect(m.confirmingCaptureOneRevert)
        #expect(await fake.reverts == 0, "nothing reverted before the confirmation")
        m.confirmingCaptureOneRevert = false
        await m.revertCaptureOneImport()
        #expect(m.toast?.message == "Capture One import reverted: 2 photos restored")
        var reverts = await fake.reverts
        #expect(!m.canRevertCaptureOne && reverts == 1)
        m.requestRevertCaptureOne()
        #expect(!m.confirmingCaptureOneRevert, "nothing to revert: no confirmation")
        await m.revertCaptureOneImport()
        reverts = await fake.reverts
        #expect(reverts == 1, "disabled: nothing to revert")

        // A revert that finds nothing, or fails, says so.
        await fake.set(revertible: true, reverted: .success(nil))
        await m.refreshCaptureOne()
        await m.revertCaptureOneImport()
        #expect(m.toast?.message == "No Capture One import to revert")
        await fake.set(revertible: true, reverted: .failure(Boom()))
        await m.refreshCaptureOne()
        await m.revertCaptureOneImport()
        #expect(m.toast?.isError == true)

        // The menu: a fresh dry run each time.
        m.showCaptureOneImport()
        guard case .captureOneImport(let menu)? = m.sheet else {
            Issue.record("menu")
            return
        }
        #expect(!menu.offered)

        // Not a Capture One folder: no menu, no offer.
        await fake.set(isC1: false)
        await m.refreshCaptureOne()
        #expect(!m.canImportCaptureOne && !m.canRevertCaptureOne)
        m.sheet = nil
        m.showCaptureOneImport()
        #expect(m.sheet == nil)
    }
}

// MARK: - Live: the real FFI on a synthetic C1 session of sample ARWs

private func repoFile(_ rel: String) -> URL {
    URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("../../../\(rel)").standardizedFileURL
}

/// The synthetic C1 session of `crates/ds-c1/fixtures` with sample ARWs as its images.
private func c1Folder() throws -> URL? {
    let arws = sampleARWs()
    guard arws.count >= 4 else { return nil }
    let fm = FileManager.default
    let dir = fm.temporaryDirectory.appendingPathComponent("c1-live-\(UUID())/Shoot")
    for sub in ["Selects/CaptureOne/Settings153", "Trash/CaptureOne/Settings153", "CaptureOne/Settings153", "Output"] {
        try fm.createDirectory(at: dir.appendingPathComponent(sub), withIntermediateDirectories: true)
    }
    for (i, rel) in ["a.ARW", "b.ARW", "Selects/c.ARW", "Trash/d.ARW"].enumerated() {
        try fm.copyItem(at: arws[i], to: dir.appendingPathComponent(rel))
    }
    for (cos, dest) in [("a.ARW.cos", "CaptureOne/Settings153"), ("c.ARW.cos", "Selects/CaptureOne/Settings153"),
                        ("d.ARW.cos", "Trash/CaptureOne/Settings153")] {
        try fm.copyItem(at: repoFile("crates/ds-c1/fixtures/\(cos)"), to: dir.appendingPathComponent("\(dest)/\(cos)"))
    }
    let sql = try String(contentsOf: repoFile("crates/ds-c1/fixtures/session.sql"), encoding: .utf8)
    var db: OpaquePointer?
    guard sqlite3_open(dir.appendingPathComponent("Shoot.cosessiondb").path, &db) == SQLITE_OK else { return nil }
    defer { sqlite3_close(db) }
    guard sqlite3_exec(db, sql, nil, nil, nil) == SQLITE_OK else { return nil }
    return dir
}

private func digest(_ dir: URL) -> [String: String] {
    var out: [String: String] = [:]
    let e = FileManager.default.enumerator(at: dir, includingPropertiesForKeys: [.isRegularFileKey])
    while let u = e?.nextObject() as? URL {
        if let d = try? Data(contentsOf: u) {
            out[u.path] = SHA256.hash(data: d).map { String(format: "%02x", $0) }.joined()
        }
    }
    return out
}

@Suite(.serialized) struct CaptureOneLiveTests {
    @MainActor
    func model(_ store: RustSessionStore) -> AppModel {
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        m.toastSleep = { _ in try await Task.sleep(for: .seconds(3600)) } // toasts stay up: no timing race
        return m
    }

    @Test func offerPreviewImportAndReload() async throws {
        guard let dir = try c1Folder() else { return }
        defer { try? FileManager.default.removeItem(at: dir.deletingLastPathComponent()) }
        let before = digest(dir)
        let store = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!))
        let info = try await store.add(folder: dir)
        let m = await model(store)
        await m.open(info)
        #expect(await m.canImportCaptureOne)
        #expect(await m.photos.count == 3, "a, b and Selects/c (C1's Trash isn't listed)")

        await m.offerCaptureOneImport(after: dir)
        guard case .captureOneImport(let sheet)? = await m.sheet else {
            Issue.record("no offer")
            return
        }
        let preview = await sheet.preview
        #expect(preview?.rated == 3 && preview?.inTrash == 1 && preview?.variantsCreated == 1 && preview?.albumsCreated == 2)
        #expect(preview?.unmapped.contains { $0.key == "WhiteBalance" } == false, "real ARWs: converted")
        #expect(await sheet.offered)

        await sheet.run()
        guard case .done(let done) = await sheet.phase else {
            Issue.record("\(await sheet.phase)")
            return
        }
        #expect(!done.dryRun && done.rated == 3)
        let ratings = await Dictionary(m.photos.map { ($0.filename, $0.rating) }, uniquingKeysWith: max)
        #expect(ratings["a.ARW"] == 4 && ratings["b.ARW"] == 3 && ratings["c.ARW"] == 2)
        #expect(await m.photos.first { $0.filename == "c.ARW" }?.pick == .picked)
        let edit = try await store.edit(for: m.photos.first { $0.filename == "a.ARW" && $0.rating == 4 }!.id)
        #expect(!edit.isIdentity, "the edit cache was dropped and re-read")

        // Offered once per folder; the menu still works and shows an up-to-date preview.
        await MainActor.run { m.sheet = nil }
        await m.offerCaptureOneImport(after: dir)
        #expect(await m.sheet == nil)
        await m.showCaptureOneImport()
        guard case .captureOneImport(let again)? = await m.sheet else {
            Issue.record("menu")
            return
        }
        await again.loadPreview()
        #expect(await again.preview?.hasWork == false)

        // File ▸ Revert Capture One Import puts everything back; importing again redoes it.
        #expect(await m.canRevertCaptureOne)
        await m.revertCaptureOneImport()
        let toast = await m.toast?.message
        #expect(toast?.hasPrefix("Capture One import reverted: 3 photos restored") == true, "\(toast ?? "no toast")")
        #expect(await m.canRevertCaptureOne == false)
        #expect(await m.photos.allSatisfy { $0.rating == 0 && $0.pick != .picked })
        let reverted = try await store.edit(for: m.photos.first { $0.filename == "a.ARW" }!.id)
        #expect(reverted.isIdentity, "the edit cache was dropped and re-read")
        await again.loadPreview()
        #expect(await again.preview?.rated == 3)
        #expect(digest(dir) == before, "the Capture One folder is byte-identical")

        // A store without that session open can't import.
        let other = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!))
        await #expect(throws: (any Error).self) { try await other.importCaptureOne(info, dryRun: true, overwriteEdits: false) }
    }

    @Test func plainFoldersAreNotOffered() async throws {
        guard let dir = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!))
        let info = try await store.add(folder: dir)
        let m = await model(store)
        await m.open(info)
        #expect(await !m.canImportCaptureOne)
        await m.offerCaptureOneImport(after: dir)
        #expect(await m.sheet == nil)
    }
}
