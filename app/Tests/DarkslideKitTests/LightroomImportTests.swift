import Foundation
import Testing
@testable import DarkslideKit

/// A scripted Lightroom importer: `preview` answers dry runs by categories, real runs return `result`.
private actor FakeLR: LightroomImporting {
    var kinds: [String: LRSourceKind] = ["Catalog.lrcat": .catalog, "Sidecars": .sidecars, "Presets": .presets]
    var info = LRSourceInfo(kind: .catalog, origin: "Catalog.lrcat", photos: 120, collections: 3)
    var loadError: Error?
    var preview: @Sendable (Set<LRCategory>) -> LRImportSummary = { c in
        LRImportSummary(changed: Dictionary(uniqueKeysWithValues: c.map { ($0.rawValue, 5) }))
    }
    var result = LRImportSummary(dryRun: false, changed: ["rating": 5])
    var presets: [LRPresetItem] = []
    var presetsError: Error?
    var revertible = false
    var reverted: Result<LRRevertSummary?, Error> = .success(LRRevertSummary(restored: 5))
    /// Sleep before the n-th dry run answers (to make an old answer arrive late).
    var delays: [Int: Duration] = [:]
    private(set) var calls: [(categories: Set<LRCategory>, dryRun: Bool, overwrite: Bool)] = []
    private(set) var loads = 0
    private(set) var released: [UUID] = []
    private(set) var reverts = 0

    func set(preview: (@Sendable (Set<LRCategory>) -> LRImportSummary)? = nil, result: LRImportSummary? = nil,
             presets: [LRPresetItem]? = nil, presetsError: Error? = nil, loadError: Error? = nil, revertible: Bool? = nil,
             reverted: Result<LRRevertSummary?, Error>? = nil, delays: [Int: Duration]? = nil) {
        if let preview { self.preview = preview }
        if let result { self.result = result }
        if let presets { self.presets = presets }
        if let presetsError { self.presetsError = presetsError }
        if let loadError { self.loadError = loadError }
        if let revertible { self.revertible = revertible }
        if let reverted { self.reverted = reverted }
        if let delays { self.delays = delays }
    }

    func lightroomKind(of url: URL) async -> LRSourceKind { kinds[url.lastPathComponent] ?? .unknown }

    func loadLightroomSource(_ url: URL) async throws -> LRSourceInfo {
        loads += 1
        if let loadError { throw loadError }
        return info
    }

    func releaseLightroomSource(_ id: UUID) async { released.append(id) }

    func importLightroom(_ session: SessionInfo, source: UUID, categories: Set<LRCategory>, dryRun: Bool,
                         overwriteEdits: Bool) async throws -> LRImportSummary {
        calls.append((categories, dryRun, overwriteEdits))
        let n = calls.count - 1
        if dryRun {
            if let d = delays[n] { try await Task.sleep(for: d) }
            return preview(categories)
        }
        revertible = result.changeCount > 0
        return result
    }

    func canRevertLightroomImport(_ session: SessionInfo) async -> Bool { revertible }

    func revertLightroomImport(_ session: SessionInfo) async throws -> LRRevertSummary? {
        reverts += 1
        revertible = false
        return try reverted.get()
    }

    func readLightroomPresets(_ url: URL) async throws -> [LRPresetItem] {
        if let presetsError { throw presetsError }
        return presets
    }
}

private struct LRStore: SessionStore, LightroomImporting {
    let inner = MockSessionStore(counts: [3])
    let lr: FakeLR
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func lightroomKind(of url: URL) async -> LRSourceKind { await lr.lightroomKind(of: url) }
    func loadLightroomSource(_ url: URL) async throws -> LRSourceInfo { try await lr.loadLightroomSource(url) }
    func releaseLightroomSource(_ id: UUID) async { await lr.releaseLightroomSource(id) }
    func importLightroom(_ session: SessionInfo, source: UUID, categories: Set<LRCategory>, dryRun: Bool,
                         overwriteEdits: Bool) async throws -> LRImportSummary {
        try await lr.importLightroom(session, source: source, categories: categories, dryRun: dryRun, overwriteEdits: overwriteEdits)
    }
    func canRevertLightroomImport(_ session: SessionInfo) async -> Bool { await lr.canRevertLightroomImport(session) }
    func revertLightroomImport(_ session: SessionInfo) async throws -> LRRevertSummary? { try await lr.revertLightroomImport(session) }
    func readLightroomPresets(_ url: URL) async throws -> [LRPresetItem] { try await lr.readLightroomPresets(url) }
}

private struct Boom: LocalizedError {
    var errorDescription: String? { "the catalog is unreadable" }
}

private let session = SessionInfo(name: "Shoot", url: URL(fileURLWithPath: "/tmp/Shoot"), photoCount: 3, createdAt: Date())
private let catalog = URL(fileURLWithPath: "/tmp/Catalog.lrcat")

@MainActor
private func model(_ fake: FakeLR, session: SessionInfo? = session, presets: PresetStore = PresetStore(fileURL: nil),
                    open: @escaping @MainActor (URL) async -> SessionInfo? = { _ in nil },
                    known: @escaping @MainActor () -> [SessionInfo] = { [] },
                    imported: @escaping @MainActor () async -> Void = {}) -> LightroomImportModel {
    LightroomImportModel(session: session, importer: fake, presetStore: presets, debounce: .zero,
                         openFolderAsSession: open, knownSessions: known, onImported: imported)
}

private func warm(_ name: String = "LR Warm", groups: Set<AdjustGroup> = [.exposure]) -> LRPresetItem {
    LRPresetItem(name: name, group: "Mine", preset: Preset(name: name, groups: groups, edit: EditState()), notes: ["Dehaze: approximate"])
}

@Suite struct LightroomSummaryTests {
    @Test func linesCountWhatIsWrittenAndSkipZeros() {
        var s = LRImportSummary(variantsCreated: 1, changed: ["rating": 212, "tone": 1, "masks": 2], albumsCreated: 1, albumsUpdated: 1)
        #expect(s.lines == ["212 photos will be rated", "1 photo will be given tone edits", "2 photos will be given local adjustments",
                            "1 virtual copy will be added as new variants", "2 albums will be created or updated"])
        s.dryRun = false
        s.variantsCreated = 2
        s.albumsUpdated = 0
        #expect(s.lines.contains("2 virtual copies were added as new variants"))
        #expect(s.lines.contains("1 album was created or updated"))
        #expect(s.changeCount == 215 + 1)
        #expect(LRImportSummary().lines == ["Everything is already up to date."])
        #expect(!LRImportSummary().hasWork)
        let c = LRImportSummary.Conflict(photo: PhotoID(7), file: "a.ARW", field: "rating")
        let d = LRImportSummary.Conflict(photo: PhotoID(7), file: "a.ARW", field: "white_balance")
        let only = LRImportSummary(conflicts: [c, d])
        #expect(only.hasWork && only.conflictPhotos == 1)
        #expect(only.lines == ["Nothing new apart from the conflicts below."])
        #expect(only.conflictLines == ["a.ARW: rating, white balance"])
        let many = LRImportSummary(conflicts: (1...10).map { .init(photo: PhotoID(Int64($0)), file: "p\($0)", field: "keywords") })
        #expect(many.conflictLines.count == 9 && many.conflictLines.last == "and 2 photos more")
        #expect(LRImportSummary.fieldName("title") == "title")
    }

    @Test func toolsAreGroupedAndMarkedApproximate() {
        let s = LRImportSummary(mapped: [
            .init(key: "Exposure2012", count: 90), .init(key: "Dehaze", approximate: true, count: 20),
            .init(key: "HueAdjustmentRed", approximate: true, count: 5), .init(key: "SaturationAdjustmentBlue", approximate: true, count: 9),
            .init(key: "Temperature", count: 30), .init(key: "Tint", count: 31), .init(key: "Weird", count: 1),
        ])
        #expect(s.toolLines == ["Exposure: 90 photos", "White balance: 31 photos", "Dehaze: 20 photos (approximate)",
                                "HSL: 9 photos (approximate)", "Weird: 1 photo"])
        for (key, tool) in [("Contrast2012", "Contrast"), ("Highlights2012", "Highlights"), ("Shadows2012", "Shadows"),
                            ("Whites2012", "Whites"), ("Blacks2012", "Blacks"), ("Clarity2012", "Clarity"),
                            ("WhiteBalance", "White balance"), ("LuminanceSmoothing", "Noise reduction"),
                            ("ColorNoiseReduction", "Noise reduction"), ("AutoLateralCA", "Chromatic aberration"),
                            ("ToneCurvePV2012", "Tone curve"), ("CropLeft", "Crop"), ("Sharpness", "Sharpening"),
                            ("PostCropVignetteAmount", "Vignette"), ("GrainAmount", "Grain"), ("LensProfileEnable", "Lens corrections"),
                            ("SplitToningShadowHue", "Colour grading"), ("ColorGradeMidtoneSat", "Colour grading"),
                            ("MaskGroupBasedCorrections", "Local adjustments"), ("LocalExposure2012", "Local adjustments")] {
            #expect(LRImportSummary.tool(for: key) == tool, "\(key)")
        }
    }

    @Test func leftOutNotMappedAndNotes() {
        let s = LRImportSummary(
            ambiguous: ["/p/x.ARW"], notFound: ["/p/a.ARW", "/p/b.ARW", "/p/c.ARW", "/p/d.ARW"], notFoundCount: 600,
            dropped: [.init(key: "ParametricShadows", reason: "no equivalent", count: 12)],
            unmappedLabels: ["Review": 2], keptDarkslide: 1, deletedInDarkslide: 2, readOnly: 1,
            skipped: [.init(name: "Smart", reason: "a smart collection")],
            notes: [.init(file: "a.ARW", notes: ["Dehaze approximate", "Parametric curve dropped"])], warnings: ["w"])
        #expect(s.notMappedLines == ["ParametricShadows: 12 photos — no equivalent"])
        #expect(s.leftOutLines == [
            "600 photos not in this session: a.ARW, b.ARW, c.ARW and 1 more",
            "1 photo could be several Darkslide photos (not imported): x.ARW",
            "Label \u{201c}Review\u{201d} has no Darkslide colour: 2 photos left unlabelled",
            "2 variants deleted in Darkslide (not recreated)",
            "1 photo saved by a newer Darkslide (left alone)",
            "1 Darkslide change kept (unchanged in Lightroom since the last import)",
            "\u{201c}Smart\u{201d}: a smart collection",
            "w",
        ])
        #expect(s.noteLines == ["a.ARW: Dehaze approximate; Parametric curve dropped"])
        #expect(LRImportSummary(deletedInDarkslide: 1).leftOutLines == ["1 variant deleted in Darkslide (not recreated)"])
    }

    @Test func matchAndSourceLines() {
        let s = LRImportSummary(photos: 1200, matched: 812, matchedByPath: 800, matchedByHash: 10, matchedByName: 2)
        #expect(s.matchLine == "812 of 1200 photos matched a photo in this session (by path 800, by content 10, by name and capture time 2)")
        #expect(LRImportSummary(photos: 1, matched: 0).matchLine == "0 of 1 photo matched a photo in this session")
        let i = LRSourceInfo(kind: .catalog, origin: "Catalog.lrcat", writtenBy: "13.2", photos: 1200, collections: 1)
        #expect(i.line == "Catalog.lrcat (Lightroom 13.2): 1200 photos, 1 collection")
        #expect(LRSourceInfo(origin: "Shoot", photos: 1).line == "Shoot: 1 photo")
    }

    @Test func revertMessageAndCategories() {
        #expect(LRRevertSummary(restored: 3).message == "Lightroom import reverted: 3 photos restored")
        let r = LRRevertSummary(restored: 1, kept: 2, variantsRemoved: 1, albumsRemoved: 1, albumsTrimmed: 1)
        #expect(r.message == "Lightroom import reverted: 1 photo restored, 1 imported variant removed, 2 albums undone, 2 later Darkslide changes kept")
        #expect(LRCategory.organise.count == 6 && LRCategory.develop.count == 8)
        #expect(Set(LRCategory.allCases.map(\.rawValue)).count == 14)
        #expect(LRCategory.allCases.allSatisfy { !$0.title.isEmpty && !$0.verb.isEmpty })
        #expect(LRCategory(rawValue: "white_balance") == .whiteBalance)
        #expect(LRImportSummary(changed: ["tone": 4]).count(.tone) == 4)
    }
}

@Suite @MainActor struct LightroomModelTests {
    @Test func chooseLoadsOnceThenRunsTheDryRunWithEveryCategory() async {
        let fake = FakeLR()
        let m = model(fake)
        #expect(m.phase == .choosing && m.title == "Import Lightroom" && !m.canImport)
        await m.choose(catalog)
        #expect(m.kind == .catalog && m.info?.photos == 120)
        #expect(m.preview?.changeCount == 14 * 5 && m.canImport)
        let calls = await fake.calls
        #expect(calls.count == 1 && calls[0].dryRun && calls[0].categories == Set(LRCategory.allCases))
        #expect(await fake.loads == 1)
        #expect(m.label(.rating) == "Rating (5 photos)" && m == m)
    }

    @Test func togglingRerunsTheDryRunWithoutRereadingAndLabelsKeepTheirCounts() async {
        let fake = FakeLR()
        let m = model(fake)
        await m.choose(catalog)
        m.toggle(.rating)
        await m.settle()
        let calls = await fake.calls
        #expect(calls.count == 2 && !calls[1].categories.contains(.rating) && calls[1].categories.count == 13)
        #expect(await fake.loads == 1, "the catalog is read once")
        #expect(m.preview?.count(.rating) == 0)
        #expect(m.label(.rating) == "Rating (5 photos)", "the count from when it was ticked stays")
        m.toggle(.rating)
        await m.settle()
        #expect(m.categories.contains(.rating) && m.preview?.count(.rating) == 5)
    }

    @Test func onlyTheLastToggleSticksEvenWhenAnOlderAnswerArrivesLate() async {
        let fake = FakeLR()
        await fake.set(delays: [1: .milliseconds(300)])
        let m = model(fake)
        await m.choose(catalog)
        m.toggle(.crop) // dry run #1: slow
        try? await Task.sleep(for: .milliseconds(30))
        m.toggle(.lens) // supersedes it
        await m.settle()
        try? await Task.sleep(for: .milliseconds(400))
        #expect(m.preview != nil)
        #expect(!m.categories.contains(.crop) && !m.categories.contains(.lens))
        #expect(m.preview?.count(.crop) == 0 && m.preview?.count(.lens) == 0, "the answer for both toggles, not the late one")
    }

    @Test func importPassesCategoriesAndOverwriteThenReportsAndReloads() async {
        let fake = FakeLR()
        var reloaded = 0
        let m = model(fake, imported: { reloaded += 1 })
        await m.choose(catalog)
        m.toggle(.keywords)
        await m.settle()
        m.overwriteEdits = true
        await m.run()
        #expect(m.phase == .done(LRImportSummary(dryRun: false, changed: ["rating": 5])) && reloaded == 1)
        let last = await fake.calls.last
        #expect(last?.dryRun == false && last?.overwrite == true && last?.categories.contains(.keywords) == false)
        #expect(m.canRevert)
        await m.revert()
        #expect(m.phase == .reverted(LRRevertSummary(restored: 5)) && reloaded == 2 && !m.canRevert)
        await m.revert()
        #expect(await fake.reverts == 1, "nothing left to revert")
    }

    @Test func conflictsOnlyNeedTheOverwriteSwitch() async {
        let fake = FakeLR()
        let c = LRImportSummary.Conflict(photo: PhotoID(1), file: "a.ARW", field: "rating")
        await fake.set(preview: { _ in LRImportSummary(conflicts: [c]) })
        let m = model(fake)
        await m.choose(catalog)
        #expect(!m.canImport)
        await m.run()
        #expect(await fake.calls.count == 1, "nothing to write: no import call")
        m.overwriteEdits = true
        #expect(m.canImport)
    }

    @Test func failuresAndCancelAreShown() async {
        let fake = FakeLR()
        await fake.set(loadError: Boom())
        let m = model(fake)
        await m.choose(catalog)
        #expect(m.phase == .failed("the catalog is unreadable"))
        await fake.set(loadError: CancellationError())
        await m.choose(catalog)
        #expect(m.phase == .failed("Cancelled"))

        var reloaded = 0
        let f = FailingImporter()
        let k = LightroomImportModel(session: session, importer: f, presetStore: PresetStore(fileURL: nil), debounce: .zero, onImported: { reloaded += 1 })
        await k.choose(catalog)
        #expect(k.phase == .failed("the catalog is unreadable"), "the dry run failed")
        let g = FailingImporter(failOnReal: true)
        let q = LightroomImportModel(session: session, importer: g, presetStore: PresetStore(fileURL: nil), debounce: .zero, onImported: { reloaded += 1 })
        await q.choose(catalog)
        await q.run()
        #expect(q.phase == .failed("the catalog is unreadable"))
        #expect(reloaded == 1, "a failed import still reloads")
        #expect(q.canRevert, "what the failed import wrote can be taken back")
        #expect(!k.canRevert, "a failed dry run wrote nothing")
        let c = FailingImporter(cancelDryRun: true)
        let r = LightroomImportModel(session: session, importer: c, presetStore: PresetStore(fileURL: nil), debounce: .zero, onImported: {})
        await r.choose(catalog)
        #expect(r.phase == .failed("Cancelled"))
        let bad = FailingImporter(failOnRevert: true)
        let t = LightroomImportModel(session: session, importer: bad, presetStore: PresetStore(fileURL: nil), debounce: .zero, onImported: {})
        await t.choose(catalog)
        await t.run()
        await t.revert()
        #expect(t.phase == .failed("the catalog is unreadable"))
    }

    @Test func noSessionAndUnknownSources() async {
        let fake = FakeLR()
        let m = model(fake, session: nil)
        await m.choose(catalog)
        #expect(m.phase == .failed(LightroomImportModel.noSession))
        #expect(await fake.loads == 0, "nothing is read without a session to import into")
        await m.choose(URL(fileURLWithPath: "/tmp/notes.txt"))
        guard case .failed(let why) = m.phase else {
            Issue.record("unknown")
            return
        }
        #expect(why.contains("notes.txt"))
        m.toggle(.rating) // no source: nothing is scheduled
        await m.settle()
        #expect(await fake.calls.isEmpty)
        #expect(LightroomImportModel.revertHint.contains("every Lightroom import since the last revert"))
    }

    @Test func choosingAnotherSourceReleasesTheLastAndRelease() async {
        let fake = FakeLR()
        let m = model(fake)
        await m.choose(catalog)
        let id = m.info?.id
        await m.choose(URL(fileURLWithPath: "/tmp/Sidecars"))
        #expect(await fake.released == [id].compactMap { $0 })
        await m.release()
        #expect(await fake.released.count == 2 && m.info == nil)
        await m.release()
        #expect(await fake.released.count == 2)
    }

    @Test func presetsImportSkipOrReplaceByName() async {
        let fake = FakeLR()
        let store = PresetStore(fileURL: nil)
        let existing = store.add(Preset(name: "LR Warm", groups: [.exposure], edit: EditState()))
        let builtIn = Preset.builtIns.first?.name ?? "x"
        await fake.set(presets: [warm("LR Warm"), warm("LR Cool"), warm(builtIn),
                                 LRPresetItem(name: "Empty", preset: Preset(name: "Empty", groups: [], edit: EditState()),
                                              unavailable: "nothing in it maps to Darkslide")])
        let m = model(fake, session: nil, presets: store)
        await m.choose(URL(fileURLWithPath: "/tmp/Presets"))
        #expect(m.importablePresets.map(\.name) == ["LR Warm", "LR Cool", builtIn])
        m.importPresets()
        #expect(m.phase == .presetsDone("1 preset imported, 2 already there"))
        #expect(store.userPresets.map(\.name) == ["LR Warm", "LR Cool"])

        await m.choose(URL(fileURLWithPath: "/tmp/Presets"))
        m.replacePresets = true
        m.importPresets()
        #expect(m.phase == .presetsDone("0 presets imported, 2 replaced, 1 already there"))
        #expect(store.userPresets.first { $0.name == "LR Warm" }?.id == existing.id, "replaced in place")
        m.importPresets()
        #expect(m.phase == .presetsDone("0 presets imported, 2 replaced, 1 already there"), "only from the list")

        await fake.set(presetsError: Boom())
        await m.choose(URL(fileURLWithPath: "/tmp/Presets"))
        #expect(m.phase == .failed("the catalog is unreadable"))
        await fake.set(presetsError: CancellationError())
        await m.choose(URL(fileURLWithPath: "/tmp/Presets"))
        #expect(m.phase == .failed("Cancelled"))
    }

    @Test func foldersNotInTheSessionAndOpeningOneCarriesOn() async throws {
        let fm = FileManager.default
        let there = fm.temporaryDirectory.appendingPathComponent("lr-folder-\(UUID())")
        try fm.createDirectory(at: there, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: there) }
        let file = fm.temporaryDirectory.appendingPathComponent("lr-file-\(UUID())")
        try Data("x".utf8).write(to: file)
        defer { try? fm.removeItem(at: file) }
        let other = SessionInfo(name: "Trip", url: URL(fileURLWithPath: "/Volumes/pics/Trip"), photoCount: 1, createdAt: Date())
        let fake = FakeLR()
        await fake.set(preview: { c in
            LRImportSummary(notFoundFolders: [.init(path: there.path, photos: 7), .init(path: "/Volumes/pics/Trip/Day1", photos: 3),
                                              .init(path: "/Volumes/gone/x", photos: 1), .init(path: file.path, photos: 2)],
                            changed: Dictionary(uniqueKeysWithValues: c.map { ($0.rawValue, 1) }))
        })
        let next = SessionInfo(name: "Folder", url: there, photoCount: 7, createdAt: Date())
        var opened: [URL] = []
        let m = model(fake, open: { opened.append($0); return $0.path == there.path ? next : nil }, known: { [other] })
        #expect(m.folderRows.isEmpty)
        await m.choose(catalog)
        #expect(m.folderRows == [
            .init(path: there.path, photos: 7, state: .available),
            .init(path: "/Volumes/pics/Trip/Day1", photos: 3, state: .inSession("Trip")),
            .init(path: "/Volumes/gone/x", photos: 1, state: .missing),
            .init(path: file.path, photos: 2, state: .missing),
        ])
        m.toggle(.keywords)
        await m.settle()
        await m.openAsSession("/Volumes/gone/x")
        #expect(m.session == session, "the folder didn't open: still the same session")
        await m.openAsSession(there.path)
        #expect(opened.last?.path == there.path && m.session == next)
        #expect(!m.categories.contains(.keywords), "the ticked categories carry over")
        #expect(await fake.calls.last?.categories == m.categories)
        #expect(m.preview != nil)
        await m.release()
        await m.openAsSession(there.path)
        #expect(opened.count == 2, "no source loaded: nothing to carry on with")
    }
}

/// Fails where told: the dry run, the real run, the revert; or reports a cancelled dry run.
private struct FailingImporter: LightroomImporting {
    var failOnReal = false
    var cancelDryRun = false
    var failOnRevert = false
    func lightroomKind(of url: URL) async -> LRSourceKind { .catalog }
    func loadLightroomSource(_ url: URL) async throws -> LRSourceInfo { LRSourceInfo() }
    func releaseLightroomSource(_ id: UUID) async {}
    func importLightroom(_ session: SessionInfo, source: UUID, categories: Set<LRCategory>, dryRun: Bool,
                         overwriteEdits: Bool) async throws -> LRImportSummary {
        if cancelDryRun { throw CancellationError() }
        if dryRun, !failOnReal, !failOnRevert { throw Boom() }
        if !dryRun, failOnReal { throw Boom() }
        return LRImportSummary(dryRun: dryRun, changed: ["rating": 1])
    }
    func canRevertLightroomImport(_ session: SessionInfo) async -> Bool { true }
    func revertLightroomImport(_ session: SessionInfo) async throws -> LRRevertSummary? { throw Boom() }
    func readLightroomPresets(_ url: URL) async throws -> [LRPresetItem] { [] }
}

@Suite @MainActor struct LightroomAppModelTests {
    @Test func menuStatusRevertConfirmationAndToasts() async throws {
        let fake = FakeLR()
        let m = makeModel(store: LRStore(lr: fake))
        await m.start()
        let s = try #require(m.currentSession)
        #expect(m.canImportLightroom && !m.canRevertLightroom)
        m.showLightroomImport()
        guard case .lightroomImport(let sheet)? = m.sheet else {
            Issue.record("no sheet")
            return
        }
        #expect(sheet.session == s && m.sheet?.id == "lightroom-import")
        await sheet.choose(catalog)
        await sheet.run()
        #expect(m.canRevertLightroom, "the import wrote something: File ▸ Revert is enabled")
        m.sheet = nil

        m.requestRevertLightroom()
        #expect(m.confirmingLightroomRevert)
        #expect(await fake.reverts == 0, "nothing reverted before the confirmation")
        m.confirmingLightroomRevert = false
        await m.revertLightroomImport()
        #expect(m.toast?.message == "Lightroom import reverted: 5 photos restored")
        #expect(!m.canRevertLightroom)
        m.requestRevertLightroom()
        #expect(!m.confirmingLightroomRevert)
        await m.revertLightroomImport()
        #expect(await fake.reverts == 1, "disabled: nothing to revert")

        await fake.set(revertible: true, reverted: .success(nil))
        await m.refreshLightroom()
        await m.revertLightroomImport()
        #expect(m.toast?.message == "No Lightroom import to revert")
        await fake.set(revertible: true, reverted: .failure(Boom()))
        await m.refreshLightroom()
        await m.revertLightroomImport()
        #expect(m.toast?.isError == true)
    }

    @Test func presetsGoToTheModelsPresetStoreAndOpeningAFolderNeedsALiveStore() async throws {
        let fake = FakeLR()
        await fake.set(presets: [warm()])
        let m = makeModel(store: LRStore(lr: fake))
        let presets = PresetStore(fileURL: nil)
        m.lightroomPresetStore = presets
        await m.start()
        m.showLightroomImport()
        guard case .lightroomImport(let sheet)? = m.sheet else {
            Issue.record("no sheet")
            return
        }
        await sheet.choose(URL(fileURLWithPath: "/tmp/Presets"))
        sheet.importPresets()
        #expect(presets.userPresets.map(\.name) == ["LR Warm"])
        // The mock store can't open folders: nothing opens, and the answer is no session.
        #expect(await m.openFolderForLightroom(URL(fileURLWithPath: "/tmp/nowhere")) == nil)
        #expect(m.toast?.message.contains("live mode") == true)
        #expect(m.knownSessionsForTest.count >= 1)
    }

    @Test func demoModeHasNoLightroomImport() async {
        let m = makeModel()
        #expect(!m.canImportLightroom && !m.canRevertLightroom)
        m.showLightroomImport()
        #expect(m.sheet == nil)
        await m.refreshLightroom()
        #expect(m.lightroom == LightroomStatus())
        await m.revertLightroomImport()
        #expect(m.toast == nil)
    }
}

extension AppModel {
    fileprivate var knownSessionsForTest: [SessionInfo] { sessionList }
}

// MARK: - Live: the real FFI on a folder of sample ARWs with XMP sidecars

@Suite(.serialized) struct LightroomLiveTests {
    @MainActor
    func appModel(_ store: RustSessionStore) -> AppModel {
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        m.toastSleep = { _ in try await Task.sleep(for: .seconds(3600)) }
        return m
    }

    private func xmp(_ attrs: String) -> String {
        """
        <x:xmpmeta xmlns:x="adobe:ns:meta/" xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" \
        xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/">\
        <rdf:RDF><rdf:Description \(attrs)/></rdf:RDF></x:xmpmeta>
        """
    }

    @Test func sidecarsPreviewImportRevertAndPresets() async throws {
        guard let dir = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let arws = try FileManager.default.contentsOfDirectory(atPath: dir.path).filter { $0.lowercased().hasSuffix(".arw") }.sorted()
        guard let first = arws.first else { return }
        let stem = (first as NSString).deletingPathExtension
        try xmp(#"xmp:Rating="4" crs:Exposure2012="+0.50""#).write(to: dir.appendingPathComponent("\(stem).xmp"), atomically: true, encoding: .utf8)
        let preset = FileManager.default.temporaryDirectory.appendingPathComponent("lr-presets-\(UUID())")
        try FileManager.default.createDirectory(at: preset, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: preset) }
        try xmp(#"crs:PresetType="Normal" crs:Name="Bright" crs:Exposure2012="+0.30""#)
            .write(to: preset.appendingPathComponent("Bright.xmp"), atomically: true, encoding: .utf8)

        let store = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!))
        let info = try await store.add(folder: dir)
        let m = await appModel(store)
        await m.open(info)
        #expect(await m.canImportLightroom)
        #expect(await store.lightroomKind(of: dir) == .sidecars)
        #expect(await store.lightroomKind(of: preset.appendingPathComponent("Bright.xmp")) == .presets)
        #expect(await store.lightroomKind(of: dir.appendingPathComponent("nope")) == .unknown)

        let source = try await store.loadLightroomSource(dir)
        #expect(source.kind == .sidecars && source.photos == 1)
        let dry = try await store.importLightroom(info, source: source.id, categories: [.rating], dryRun: true, overwriteEdits: false)
        #expect(dry.dryRun && dry.matched == 1 && dry.count(.rating) == 1)
        let real = try await store.importLightroom(info, source: source.id, categories: [.rating], dryRun: false, overwriteEdits: false)
        #expect(real.count(.rating) == 1)
        #expect(await store.canRevertLightroomImport(info))
        let reverted = try await store.revertLightroomImport(info)
        #expect(reverted?.restored == 1)
        #expect(try await store.revertLightroomImport(info) == nil)
        await store.releaseLightroomSource(source.id)
        await #expect(throws: (any Error).self) {
            try await store.importLightroom(info, source: source.id, categories: [.rating], dryRun: true, overwriteEdits: false)
        }

        let items = try await store.readLightroomPresets(preset)
        #expect(items.map(\.name) == ["Bright"])
        await #expect(throws: (any Error).self) { try await store.loadLightroomSource(dir.appendingPathComponent("nope")) }
    }
}
