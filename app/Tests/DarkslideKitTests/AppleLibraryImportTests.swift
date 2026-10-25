import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// A scripted Aperture / iPhoto importer: `preview` answers dry runs by categories, real runs return `result`.
private actor FakeApple: AppleLibraryImporting {
    var kinds: [String: AppleLibraryKind] = [
        "Library.aplibrary": .aperture, "iPhoto Library": .iphotoXML, "Photos Library.photoslibrary": .photos,
        "Old.aplibrary": .oldAperture,
    ]
    var info = AppleLibraryInfo(kind: .aperture, origin: "Library.aplibrary", writtenBy: "Aperture 3", photos: 120, albums: 3)
    var loadError: Error?
    var preview: @Sendable (Set<LRCategory>) -> LRImportSummary = { c in
        LRImportSummary(changed: Dictionary(uniqueKeysWithValues: c.map { ($0.rawValue, 5) }))
    }
    var result = LRImportSummary(dryRun: false, changed: ["rating": 5])
    var revertible = false
    var reverted: Result<LRRevertSummary?, Error> = .success(LRRevertSummary(restored: 5))
    var failRealRun = false
    private(set) var calls: [(categories: Set<LRCategory>, dryRun: Bool, overwrite: Bool)] = []
    private(set) var loads = 0
    private(set) var released: [UUID] = []
    private(set) var reverts = 0

    func set(preview: (@Sendable (Set<LRCategory>) -> LRImportSummary)? = nil, loadError: Error? = nil,
             revertible: Bool? = nil, reverted: Result<LRRevertSummary?, Error>? = nil, failRealRun: Bool? = nil) {
        if let preview { self.preview = preview }
        if let loadError { self.loadError = loadError }
        if let revertible { self.revertible = revertible }
        if let reverted { self.reverted = reverted }
        if let failRealRun { self.failRealRun = failRealRun }
    }

    func appleLibraryKind(of url: URL) async -> AppleLibraryKind { kinds[url.lastPathComponent] ?? .unknown }

    func loadAppleLibrary(_ url: URL) async throws -> AppleLibraryInfo {
        loads += 1
        if let loadError { throw loadError }
        return info
    }

    func releaseAppleLibrary(_ id: UUID) async { released.append(id) }

    func importAppleLibrary(_ session: SessionInfo, library: UUID, categories: Set<LRCategory>, dryRun: Bool,
                            overwriteEdits: Bool) async throws -> LRImportSummary {
        calls.append((categories, dryRun, overwriteEdits))
        if dryRun { return preview(categories) }
        if failRealRun {
            revertible = true
            throw Boom()
        }
        revertible = result.changeCount > 0
        return result
    }

    func canRevertAppleImport(_ session: SessionInfo) async -> Bool { revertible }

    func revertAppleImport(_ session: SessionInfo) async throws -> LRRevertSummary? {
        reverts += 1
        revertible = false
        return try reverted.get()
    }
}

private struct AppleStore: SessionStore, AppleLibraryImporting {
    let inner = MockSessionStore(counts: [3])
    let apple: FakeApple
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func appleLibraryKind(of url: URL) async -> AppleLibraryKind { await apple.appleLibraryKind(of: url) }
    func loadAppleLibrary(_ url: URL) async throws -> AppleLibraryInfo { try await apple.loadAppleLibrary(url) }
    func releaseAppleLibrary(_ id: UUID) async { await apple.releaseAppleLibrary(id) }
    func importAppleLibrary(_ session: SessionInfo, library: UUID, categories: Set<LRCategory>, dryRun: Bool,
                            overwriteEdits: Bool) async throws -> LRImportSummary {
        try await apple.importAppleLibrary(session, library: library, categories: categories, dryRun: dryRun,
                                           overwriteEdits: overwriteEdits)
    }
    func canRevertAppleImport(_ session: SessionInfo) async -> Bool { await apple.canRevertAppleImport(session) }
    func revertAppleImport(_ session: SessionInfo) async throws -> LRRevertSummary? { try await apple.revertAppleImport(session) }
}

private struct Boom: LocalizedError {
    var errorDescription: String? { "the library is unreadable" }
}

private let session = SessionInfo(name: "Masters", url: URL(fileURLWithPath: "/tmp/Library.aplibrary/Masters"),
                                  photoCount: 3, createdAt: Date())
private let aperture = URL(fileURLWithPath: "/tmp/Library.aplibrary")

@MainActor
private func model(_ fake: FakeApple, session: SessionInfo? = session,
                    open: @escaping @MainActor (URL) async -> SessionInfo? = { _ in nil },
                    known: @escaping @MainActor () -> [SessionInfo] = { [] },
                    imported: @escaping @MainActor () async -> Void = {}) -> AppleLibraryImportModel {
    AppleLibraryImportModel(session: session, importer: fake, debounce: .zero, openFolderAsSession: open,
                            knownSessions: known, onImported: imported)
}

@Suite struct AppleLibraryTypesTests {
    @Test func kindsSayWhyTheyCantBeRead() {
        #expect(AppleLibraryKind.aperture.isReadable && AppleLibraryKind.iphoto.isReadable && AppleLibraryKind.iphotoXML.isReadable)
        #expect(AppleLibraryKind(rawValue: "iphoto-xml") == .iphotoXML && AppleLibraryKind(rawValue: "old-aperture") == .oldAperture)
        #expect(AppleLibraryKind.aperture.refusal("x") == nil)
        #expect(AppleLibraryKind.photos.refusal("P.photoslibrary")?.contains("is a Photos library") == true)
        #expect(AppleLibraryKind.photos.refusal("P")?.contains("Export") == true)
        #expect(AppleLibraryKind.oldAperture.refusal("A")?.contains("Aperture 1 or 2") == true)
        #expect(AppleLibraryKind.unknown.refusal("notes.txt") == "\u{201c}notes.txt\u{201d} isn't an Aperture or iPhoto library.")
        #expect(!AppleLibraryKind.photos.isReadable && !AppleLibraryKind.unknown.isReadable)
    }

    @Test func infoLineAndCategories() {
        let i = AppleLibraryInfo(origin: "Library.aplibrary", writtenBy: "Aperture 3", photos: 1200, albums: 1)
        #expect(i.line == "Library.aplibrary (Aperture 3): 1200 photos, 1 album")
        #expect(AppleLibraryInfo(origin: "iPhoto Library", photos: 1).line == "iPhoto Library: 1 photo")
        #expect(AppleImportCategories.all.count == 11)
        #expect(!AppleImportCategories.all.contains(.lens) && !AppleImportCategories.all.contains(.masks))
        #expect(AppleImportCategories.title(.metadata) == "Title, caption and place")
        #expect(AppleImportCategories.title(.collections) == "Projects, events and albums")
        #expect(AppleImportCategories.title(.rating) == "Rating")
        // The report speaks of the library, the revert of this import.
        var s = LRImportSummary(keptDarkslide: 1)
        s.source = "the library"
        #expect(s.leftOutLines == ["1 Darkslide change kept (unchanged in the library since the last import)"])
        var r = LRRevertSummary(restored: 2)
        r.what = "Aperture and iPhoto import"
        #expect(r.message == "Aperture and iPhoto import reverted: 2 photos restored")
    }
}

@Suite @MainActor struct AppleLibraryModelTests {
    @Test func chooseLoadsOnceThenRunsTheDryRunWithTheLibrarysCategories() async {
        let fake = FakeApple()
        let m = model(fake)
        #expect(m.phase == .choosing && m.title == "Import Aperture or iPhoto Library" && !m.canImport && m == m)
        await m.choose(aperture)
        #expect(m.kind == .aperture && m.info?.photos == 120)
        #expect(m.preview?.source == "the library")
        #expect(m.preview?.changeCount == 11 * 5 && m.canImport)
        let calls = await fake.calls
        #expect(calls.count == 1 && calls[0].dryRun && calls[0].categories == AppleImportCategories.all)
        #expect(m.label(.collections) == "Projects, events and albums (5 photos)")
        #expect(m.label(.lens) == "Lens corrections", "never counted")
    }

    @Test func togglingRerunsTheDryRunWithoutRereading() async {
        let fake = FakeApple()
        let m = model(fake)
        await m.choose(aperture)
        m.toggle(.keywords)
        await m.settle()
        let calls = await fake.calls
        #expect(calls.count == 2 && !calls[1].categories.contains(.keywords))
        #expect(await fake.loads == 1, "the library is read once")
        #expect(m.label(.keywords) == "Keywords (5 photos)", "the count from when it was ticked stays")
    }

    @Test func importRevertAndReload() async {
        let fake = FakeApple()
        var reloaded = 0
        let m = model(fake, imported: { reloaded += 1 })
        await m.choose(aperture)
        m.overwriteEdits = true
        await m.run()
        guard case .done(let s) = m.phase else {
            Issue.record("not done: \(m.phase)")
            return
        }
        #expect(s.count(.rating) == 5 && s.source == "the library" && reloaded == 1)
        let last = await fake.calls.last
        #expect(last?.dryRun == false && last?.overwrite == true)
        #expect(m.canRevert)
        await m.revert()
        #expect(m.phase == .reverted(LRRevertSummary(restored: 5, what: "Aperture and iPhoto import")))
        #expect(reloaded == 2 && !m.canRevert)
        await m.revert()
        #expect(await fake.reverts == 1, "nothing left to revert")
    }

    @Test func conflictsOnlyNeedTheOverwriteSwitch() async {
        let fake = FakeApple()
        let c = LRImportSummary.Conflict(photo: PhotoID(1), file: "a.CR2", field: "rating")
        await fake.set(preview: { _ in LRImportSummary(conflicts: [c]) })
        let m = model(fake)
        await m.choose(aperture)
        #expect(!m.canImport)
        await m.run()
        #expect(await fake.calls.count == 1, "nothing to write: no import call")
        m.overwriteEdits = true
        #expect(m.canImport)
    }

    @Test func refusalsFailuresAndCancel() async {
        let fake = FakeApple()
        let m = model(fake)
        await m.choose(URL(fileURLWithPath: "/tmp/Photos Library.photoslibrary"))
        guard case .failed(let why) = m.phase else {
            Issue.record("Photos")
            return
        }
        #expect(why.contains("is a Photos library"))
        #expect(await fake.loads == 0, "a Photos library is never read")
        await m.choose(URL(fileURLWithPath: "/tmp/Old.aplibrary"))
        #expect(m.phase == .failed(AppleLibraryKind.oldAperture.refusal("Old.aplibrary")!))
        await m.choose(URL(fileURLWithPath: "/tmp/notes.txt"))
        #expect(m.phase == .failed(AppleLibraryKind.unknown.refusal("notes.txt")!))
        await fake.set(loadError: Boom())
        await m.choose(aperture)
        #expect(m.phase == .failed("the library is unreadable"))
        await fake.set(loadError: CancellationError())
        await m.choose(aperture)
        #expect(m.phase == .failed("Cancelled"))
        m.toggle(.rating) // no library: nothing is scheduled
        await m.settle()
        #expect(await fake.calls.isEmpty)
        #expect(AppleLibraryImportModel.revertHint.contains("every Aperture and iPhoto import"))
    }

    @Test func aFailedImportCanBeReverted() async {
        let fake = FakeApple()
        await fake.set(failRealRun: true)
        var reloaded = 0
        let m = model(fake, imported: { reloaded += 1 })
        await m.choose(aperture)
        await m.run()
        #expect(m.phase == .failed("the library is unreadable") && reloaded == 1)
        #expect(m.canRevert, "what the failed import wrote can be taken back")
        await fake.set(reverted: .failure(Boom()))
        await m.revert()
        #expect(m.phase == .failed("the library is unreadable") && !m.canRevert)
    }

    @Test func aFailedOrCancelledDryRunIsShown() async {
        struct Failing: AppleLibraryImporting {
            var cancel = false
            func appleLibraryKind(of url: URL) async -> AppleLibraryKind { .iphoto }
            func loadAppleLibrary(_ url: URL) async throws -> AppleLibraryInfo { AppleLibraryInfo() }
            func releaseAppleLibrary(_ id: UUID) async {}
            func importAppleLibrary(_ session: SessionInfo, library: UUID, categories: Set<LRCategory>, dryRun: Bool,
                                    overwriteEdits: Bool) async throws -> LRImportSummary {
                if cancel { throw CancellationError() }
                throw Boom()
            }
            func canRevertAppleImport(_ session: SessionInfo) async -> Bool { false }
            func revertAppleImport(_ session: SessionInfo) async throws -> LRRevertSummary? { nil }
        }
        let a = AppleLibraryImportModel(session: session, importer: Failing(), debounce: .zero)
        await a.choose(aperture)
        #expect(a.phase == .failed("the library is unreadable") && !a.canRevert)
        let b = AppleLibraryImportModel(session: session, importer: Failing(cancel: true), debounce: .zero)
        await b.choose(aperture)
        #expect(b.phase == .failed("Cancelled"))
    }

    @Test func withoutASessionTheLibrarysOriginalsOpenAsOne() async throws {
        let fm = FileManager.default
        let lib = fm.temporaryDirectory.appendingPathComponent("apple-\(UUID())/Library.aplibrary")
        try fm.createDirectory(at: lib.appendingPathComponent("Masters"), withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: lib.deletingLastPathComponent()) }
        let fake = FakeApple()
        let masters = SessionInfo(name: "Masters", url: lib.appendingPathComponent("Masters"), photoCount: 2, createdAt: Date())
        var opened: [URL] = []
        let m = model(fake, session: nil, open: { opened.append($0); return masters })
        #expect(m.originalsFolder == nil, "nothing chosen yet")
        await m.choose(lib)
        #expect(m.phase == .failed(AppleLibraryImportModel.noSession))
        #expect(await fake.loads == 1 && m.info != nil)
        #expect(m.originalsFolder?.lastPathComponent == "Masters")
        m.toggle(.flag) // no session: nothing is scheduled
        await m.settle()
        #expect(await fake.calls.isEmpty)
        await m.openAsSession(m.originalsFolder!.path)
        #expect(opened.last?.lastPathComponent == "Masters" && m.session == masters)
        #expect(m.preview != nil && !m.categories.contains(.flag), "carries on with the ticked categories")
        // An older iPhoto library chosen by its AlbumData.xml.
        let old = lib.deletingLastPathComponent().appendingPathComponent("iPhoto Library")
        try fm.createDirectory(at: old.appendingPathComponent("Originals"), withIntermediateDirectories: true)
        let n = model(fake, session: nil)
        await n.choose(old.appendingPathComponent("AlbumData.xml"))
        #expect(n.originalsFolder == nil, "unknown to the fake: not loaded")
        let k = model(fake, session: nil)
        await k.choose(old)
        #expect(k.originalsFolder?.lastPathComponent == "Originals")
    }

    @Test func foldersNotInTheSessionAndRelease() async throws {
        let fm = FileManager.default
        let there = fm.temporaryDirectory.appendingPathComponent("apple-folder-\(UUID())")
        try fm.createDirectory(at: there, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: there) }
        let fake = FakeApple()
        await fake.set(preview: { c in
            LRImportSummary(notFoundFolders: [.init(path: there.path, photos: 7), .init(path: "/Volumes/Offline Disk/Shoot", photos: 1)],
                            changed: Dictionary(uniqueKeysWithValues: c.map { ($0.rawValue, 1) }))
        })
        let next = SessionInfo(name: "Folder", url: there, photoCount: 7, createdAt: Date())
        let m = model(fake, open: { $0.path == there.path ? next : nil })
        #expect(m.folderRows.isEmpty)
        await m.choose(aperture)
        #expect(m.folderRows == [.init(path: there.path, photos: 7, state: .available),
                                 .init(path: "/Volumes/Offline Disk/Shoot", photos: 1, state: .missing)])
        await m.openAsSession("/Volumes/Offline Disk/Shoot")
        #expect(m.session == session, "it didn't open: the same session")
        await m.openAsSession(there.path)
        #expect(m.session == next)
        let id = m.info?.id
        await m.choose(aperture)
        #expect(await fake.released == [id].compactMap { $0 })
        await m.release()
        #expect(await fake.released.count == 2 && m.info == nil)
        await m.release()
        #expect(await fake.released.count == 2)
        await m.openAsSession(there.path)
        #expect(m.session == next, "nothing loaded: nothing to carry on with")
    }
}

@Suite @MainActor struct AppleLibraryAppModelTests {
    @Test func menuStatusRevertConfirmationAndToasts() async throws {
        let fake = FakeApple()
        let m = makeModel(store: AppleStore(apple: fake))
        await m.start()
        let s = try #require(m.currentSession)
        #expect(m.canImportAppleLibrary && !m.canRevertAppleLibrary)
        m.showAppleLibraryImport()
        guard case .appleLibraryImport(let sheet)? = m.sheet else {
            Issue.record("no sheet")
            return
        }
        #expect(sheet.session == s && m.sheet?.id == "apple-library-import")
        await sheet.choose(aperture)
        await sheet.run()
        #expect(m.canRevertAppleLibrary, "the import wrote something: File ▸ Revert is enabled")
        m.sheet = nil

        m.requestRevertAppleLibrary()
        #expect(m.confirmingAppleRevert)
        #expect(await fake.reverts == 0, "nothing reverted before the confirmation")
        m.confirmingAppleRevert = false
        await m.revertAppleLibraryImport()
        #expect(m.toast?.message == "Aperture and iPhoto import reverted: 5 photos restored")
        #expect(!m.canRevertAppleLibrary)
        m.requestRevertAppleLibrary()
        #expect(!m.confirmingAppleRevert)
        await m.revertAppleLibraryImport()
        #expect(await fake.reverts == 1, "disabled: nothing to revert")

        await fake.set(revertible: true, reverted: .success(nil))
        await m.refreshAppleLibrary()
        await m.revertAppleLibraryImport()
        #expect(m.toast?.message == "No Aperture or iPhoto import to revert")
        await fake.set(revertible: true, reverted: .failure(Boom()))
        await m.refreshAppleLibrary()
        await m.revertAppleLibraryImport()
        #expect(m.toast?.isError == true)
        // The mock store can't open folders: nothing opens.
        #expect(await m.openFolderForAppleLibrary(URL(fileURLWithPath: "/tmp/nowhere")) == nil)
        await m.closeSession()
        #expect(m.appleLibrary == AppleLibraryStatus() && !m.confirmingAppleRevert)
    }

    @Test func demoModeHasNoAppleImport() async {
        let m = makeModel()
        #expect(!m.canImportAppleLibrary && !m.canRevertAppleLibrary)
        m.showAppleLibraryImport()
        #expect(m.sheet == nil)
        await m.refreshAppleLibrary()
        #expect(m.appleLibrary == AppleLibraryStatus())
        await m.revertAppleLibraryImport()
        #expect(m.toast == nil)
    }
}

// MARK: - Live: the real FFI on a synthetic iPhoto library (AlbumData.xml)

@Suite(.serialized) struct AppleLibraryLiveTests {
    /// An older iPhoto library: `AlbumData.xml` naming one photo under `Originals/` (a
    /// sample ARW when the samples are there, else a stand-in no session can match).
    private func library(_ dir: URL, photo: URL?) throws -> (root: URL, originals: URL) {
        let fm = FileManager.default
        let root = dir.appendingPathComponent("iPhoto Library")
        let originals = root.appendingPathComponent("Originals")
        let roll = originals.appendingPathComponent("2008/Roll 1")
        try fm.createDirectory(at: roll, withIntermediateDirectories: true)
        let name = photo?.lastPathComponent ?? "IMG_0001.JPG"
        if let photo { try fm.copyItem(at: photo, to: roll.appendingPathComponent(name)) } else {
            try Data("stand-in".utf8).write(to: roll.appendingPathComponent(name))
        }
        let xml = """
        <?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
        <key>Application Version</key><string>8.1.2</string>
        <key>Archive Path</key><string>/Users/someone/Pictures/iPhoto Library</string>
        <key>List of Keywords</key><dict><key>1</key><string>Garden</string></dict>
        <key>Master Image List</key><dict><key>7</key><dict>
          <key>ImagePath</key><string>/Users/someone/Pictures/iPhoto Library/Originals/2008/Roll 1/\(name)</string>
          <key>Rating</key><integer>4</integer><key>Keywords</key><array><string>1</string></array>
        </dict></dict>
        <key>List of Rolls</key><array><dict><key>RollID</key><integer>1</integer>
          <key>RollName</key><string>Roll 1</string><key>KeyList</key><array><string>7</string></array></dict></array>
        </dict></plist>
        """
        try xml.write(to: root.appendingPathComponent("AlbumData.xml"), atomically: true, encoding: .utf8)
        return (root, originals)
    }

    private func contents(_ dir: URL) -> [String: Data] {
        var out: [String: Data] = [:]
        let e = FileManager.default.enumerator(at: dir, includingPropertiesForKeys: nil)
        while let u = e?.nextObject() as? URL {
            if let d = try? Data(contentsOf: u) { out[u.path] = d }
        }
        return out
    }

    @Test func aLibraryIsDetectedReadImportedAndRevertedWithoutBeingWritten() async throws {
        let fm = FileManager.default
        let dir = fm.temporaryDirectory.appendingPathComponent("apple-live-\(UUID())")
        defer { try? fm.removeItem(at: dir) }
        let (root, originals) = try library(dir, photo: sampleARWs().first)
        let before = contents(root)
        let store = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!),
                                     storeBase: dir.appendingPathComponent("support"))
        #expect(await store.appleLibraryKind(of: root) == .iphotoXML)
        #expect(await store.appleLibraryKind(of: dir) == .unknown)
        let photos = dir.appendingPathComponent("Photos Library.photoslibrary")
        try fm.createDirectory(at: photos, withIntermediateDirectories: true)
        #expect(await store.appleLibraryKind(of: photos) == .photos)
        await #expect(throws: (any Error).self) { try await store.loadAppleLibrary(photos) }

        let info = try await store.loadAppleLibrary(root)
        #expect(info.kind == .iphotoXML && info.photos == 1 && info.albums == 1)
        #expect(info.writtenBy == "iPhoto 8.1.2" && info.origin == "iPhoto Library")
        // The library's originals open as a session whose store is outside the library.
        let session = try await store.add(folder: originals)
        #expect(!fm.fileExists(atPath: originals.appendingPathComponent(".ds").path), "nothing is written in the library")
        let dry = try await store.importAppleLibrary(session, library: info.id, categories: AppleImportCategories.all,
                                                     dryRun: true, overwriteEdits: false)
        #expect(dry.dryRun && dry.photos == 1)
        if sampleARWs().isEmpty {
            #expect(dry.matched == 0 && dry.notFoundCount == 1, "a stand-in isn't a photo Darkslide registers")
        } else {
            #expect(dry.matched == 1 && dry.count(.rating) == 1 && dry.count(.keywords) == 1)
            let real = try await store.importAppleLibrary(session, library: info.id, categories: [.rating, .collections],
                                                          dryRun: false, overwriteEdits: false)
            #expect(real.count(.rating) == 1 && real.albumsCreated == 1)
            #expect(await store.canRevertAppleImport(session))
            let reverted = try await store.revertAppleImport(session)
            #expect(reverted?.restored == 1)
            #expect(try await store.revertAppleImport(session) == nil)
        }
        #expect(contents(root) == before, "the library is byte-identical")
        await store.releaseAppleLibrary(info.id)
        await #expect(throws: (any Error).self) {
            try await store.importAppleLibrary(session, library: info.id, categories: [.rating], dryRun: true, overwriteEdits: false)
        }
        let nowhere = SessionInfo(name: "x", url: dir, photoCount: 0, createdAt: Date())
        await #expect(throws: (any Error).self) {
            try await store.importAppleLibrary(nowhere, library: info.id, categories: [.rating], dryRun: true, overwriteEdits: false)
        }
        await #expect(throws: (any Error).self) { try await store.revertAppleImport(nowhere) }
        #expect(!(await store.canRevertAppleImport(nowhere)))
    }
}
