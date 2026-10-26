import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// A recents store with a fixed list (some unavailable), editable like the live one.
private actor RecentsStore: SessionStore, RecentSessionsEditing {
    private(set) var list: [SessionInfo]
    private(set) var cleared: [[UUID]] = []
    private(set) var opened: [UUID] = []
    init(_ list: [SessionInfo]) { self.list = list }
    func listSessions() async throws -> [SessionInfo] { list }
    func photos(in session: SessionInfo) async throws -> [Photo] { [] }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { throw MockError.invalid("no") }
    func save(photos: [Photo]) async throws {}
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
    func removeFromRecents(_ session: SessionInfo) async { list.removeAll { $0.id == session.id } }
    func clearRecents(keeping sessions: [SessionInfo]) async {
        cleared.append(sessions.map(\.id))
        list.removeAll { s in !sessions.contains { $0.id == s.id } }
    }
    func noteOpened(_ session: SessionInfo) async { opened.append(session.id) }
}

private func info(_ name: String, _ why: SessionInfo.Unavailable? = nil, opened: Date? = nil) -> SessionInfo {
    var s = SessionInfo(name: name, url: URL(fileURLWithPath: "/Users/me/Pictures/\(name)"), photoCount: 3,
                        createdAt: Date(), unavailable: why)
    s.lastOpened = opened
    return s
}

@MainActor
private func model(_ list: [SessionInfo]) async -> (AppModel, RecentsStore) {
    let store = RecentsStore(list)
    let m = makeModel(store: store)
    m.settings.sessionClosed = true // nothing opens on its own
    await m.start()
    return (m, store)
}

@MainActor
@Suite struct OpenRecentTests {
    @Test func entriesAreNewestFirstAndSayWhyOneIsOff() async {
        let (m, _) = await model([info("Today"), info("Card", .driveNotConnected), info("Gone", .notFound),
                                  info("Empty", .noPhotos)])
        let e = m.recentEntries
        #expect(e.map(\.name) == ["Today", "Card", "Gone", "Empty"], "the store's order: newest first")
        #expect(e.map(\.reason) == [nil, "Drive not connected", "Not found", "No photos found"])
        #expect(e[1].menuTitle == "Card (Drive not connected)" && e[0].menuTitle == "Today")
        #expect(SessionInfo.Unavailable.driveNotConnected.folderMissing && SessionInfo.Unavailable.notFound.folderMissing
                && !SessionInfo.Unavailable.noPhotos.folderMissing)
        #expect(e.allSatisfy { !$0.isCurrent })
        m.stop()
    }

    @Test func atMostTenAreListed() async {
        let (m, _) = await model((1...14).map { info("S\($0)") })
        #expect(m.recentEntries.count == AppModel.recentLimit && AppModel.recentLimit == 10)
        #expect(m.recentEntries.first?.name == "S1" && m.recentEntries.last?.name == "S10")
        m.stop()
    }

    @Test func entryDetails() {
        let now = Date(timeIntervalSince1970: 1_791_000_000)
        let e = RecentEntry(session: info("Hike", opened: now.addingTimeInterval(-2 * 86_400)), isCurrent: false)
        #expect(e.openedText(now: now)?.hasPrefix("Opened ") == true)
        #expect(RecentEntry(session: info("Hike"), isCurrent: false).openedText(now: now) == nil)
        #expect(e.path.hasSuffix("Pictures/Hike") && e.id == e.session.id)
    }

    @Test func clearMenuForgetsTheListButNotTheOpenSession() async throws {
        let (m, store) = await model([info("A"), info("B"), info("C", .driveNotConnected)])
        #expect(m.canClearRecents)
        await m.open(m.sessionList[1])
        let open = try #require(m.currentSession)
        #expect(m.recentEntries.first { $0.isCurrent }?.id == open.id)
        #expect(m.sessionList.first { $0.id == open.id }?.lastOpened != nil, "opening stamps its date")
        await m.clearRecents()
        #expect(await store.cleared == [[open.id]])
        #expect(await store.opened == [open.id])
        #expect(m.sessionList.map(\.id) == [open.id], "only the open one is left")
        #expect(m.currentSession?.id == open.id)
        #expect(!m.canClearRecents, "nothing more to clear")
        await m.clearRecents()
        #expect(await store.cleared.count == 1)
        m.stop()
    }

    @Test func storesWithoutAnEditableListCantClear() async {
        let m = makeModel()
        await m.start()
        #expect(!m.canClearRecents)
        await m.clearRecents()
        #expect(m.sessionList.count == 2)
        m.stop()
    }

    @Test func openGoesThroughTheSheetWhenThereAreRecents() async {
        let (m, _) = await model([info("A")])
        #expect(CommandCatalog.perform(.openFolder, model: m) == .none)
        #expect(m.sheet == .openSession)
        m.sheet = nil
        #expect(m.performEmptyStateAction(.openFolder) && m.sheet == .openSession)
        let (empty, _) = await model([])
        #expect(CommandCatalog.perform(.openFolder, model: empty) == .chooseFolder, "no recents: the folder panel")
        #expect(!empty.performEmptyStateAction(.openFolder) && empty.sheet == nil)
        m.stop()
        empty.stop()
    }

    @Test func theSheetMovesAndOpensTheSelection() async throws {
        let (m, _) = await model([info("A"), info("Card", .driveNotConnected), info("B")])
        await m.open(m.sessionList[0])
        let form = OpenSessionModel(app: m)
        #expect(form.selected?.name == "Card", "the first one that isn't already open")
        form.move(-1)
        #expect(form.selected?.name == "A")
        form.move(-1)
        #expect(form.selected?.name == "A", "stays at the top")
        form.move(1)
        form.move(1)
        form.move(1)
        #expect(form.selected?.name == "B", "stays at the bottom")
        #expect(form.canOpen)
        form.selection = nil
        #expect(!form.canOpen)
        form.move(1)
        #expect(form.selected?.name == "A")
        form.selection = nil
        form.move(-1)
        #expect(form.selected?.name == "B")
        // Opening from recents.
        let pick = try #require(form.selected)
        await m.open(pick.session)
        #expect(m.currentSession?.id == pick.id)
        #expect(OpenSessionModel(app: m).selected?.name == "A")
        // Only the open one: it is selected. None: nothing.
        let (one, _) = await model([info("Only")])
        await one.open(one.sessionList[0])
        #expect(OpenSessionModel(app: one).selected?.name == "Only")
        let (none, _) = await model([])
        let empty = OpenSessionModel(app: none)
        empty.move(1)
        #expect(empty.selection == nil)
        m.stop()
        one.stop()
        none.stop()
    }

    /// LOW-D: Clear Menu keeps a session with an unfinished import, so it is still offered.
    @Test func clearMenuKeepsUnfinishedImports() async {
        let pending = info("One")
        let store = RecentsStore([info("A"), SessionInfo(id: pending.id, name: "One", url: URL(fileURLWithPath: "/lib/One"),
                                                        photoCount: 1, createdAt: Date())])
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: PendingImporter(), archive: MockArchiveService(target: "/nonexistent/x", isReachable: { _ in false }),
                         ai: MockAIService(), settings: makeSettings())
        m.watchCards = false
        m.settings.sessionClosed = true
        await m.start()
        await m.clearRecents()
        #expect(m.sessionList.map(\.name) == ["One"])
        await m.checkPendingImports()
        #expect(m.pendingImport?.sessionURL.lastPathComponent == "One", "still offered to resume")
        m.stop()
    }

    @Test func driveNotConnectedEntriesStayClickable() {
        let e = RecentEntry(session: info("Card", .driveNotConnected), isCurrent: false)
        #expect(e.reason == "Drive not connected" && e.menuTitle == "Card (Drive not connected)")
        #expect(SessionInfo.Unavailable.driveNotConnected.label == "drive not connected")
    }
}

/// An importer with an unfinished import into /lib/One.
private struct PendingImporter: ImportService, ImportResuming {
    func scan(volume: URL) async throws -> [ImportCandidate] { [] }
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> { AsyncThrowingStream { $0.finish() } }
    func pendingImports(including sessions: [URL]) async -> [PendingImportInfo] {
        sessions.filter { $0.path == "/lib/One" }.map { PendingImportInfo(sessionURL: $0, done: 1, pending: 2, failed: 0) }
    }
    func discard(_ pending: PendingImportInfo) async throws {}
}

// MARK: - Live recents (RecentFolders, RecentsHygiene, RustSessionStore)

@MainActor
@Suite(.serialized) struct OpenRecentLiveTests {
    @Test func recentFoldersRecordWhenOpenedAndClearKeepsOnlyTheOpenOne() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("lt-recents-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let dirs = try ["A", "B", "C"].map { n in
            let d = root.appendingPathComponent(n)
            try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
            return d
        }
        let recents = RecentFolders(defaults: testDefaults())
        var clock = Date(timeIntervalSince1970: 1_000)
        recents.now = { [clock] in clock }
        recents.add(dirs[0])
        clock = Date(timeIntervalSince1970: 2_000)
        recents.now = { [clock] in clock }
        recents.add(dirs[1])
        recents.add(dirs[2])
        #expect(recents.folders.map(\.lastPathComponent) == ["C", "B", "A"])
        #expect(recents.lastOpened(dirs[0]) == Date(timeIntervalSince1970: 1_000))
        #expect(recents.lastOpened(dirs[1]) == Date(timeIntervalSince1970: 2_000))
        #expect(recents.lastOpened(root) == nil)
        // A renamed folder keeps its date.
        let moved = root.appendingPathComponent("A2")
        try FileManager.default.moveItem(at: dirs[0], to: moved)
        recents.replace(dirs[0], with: moved)
        #expect(recents.lastOpened(moved) == Date(timeIntervalSince1970: 1_000))
        // Stamped again on every open (a cached handle too); unlisted folders aren't added.
        clock = Date(timeIntervalSince1970: 3_000)
        recents.now = { [clock] in clock }
        recents.stampOpened(moved)
        #expect(recents.lastOpened(moved) == Date(timeIntervalSince1970: 3_000))
        recents.stampOpened(root)
        #expect(recents.lastOpened(root) == nil && recents.folders.count == 3)
        recents.clear(keeping: [dirs[1]])
        #expect(recents.folders.map(\.lastPathComponent) == ["B"])
        #expect(recents.lastOpened(dirs[2]) == nil && recents.lastOpened(dirs[1]) != nil)
        recents.clear()
        #expect(recents.folders.isEmpty)
        #expect(try FileManager.default.contentsOfDirectory(atPath: root.path).sorted() == ["A2", "B", "C"],
                "no folder was touched")
    }

    @Test func theLiveStoreListsDatesAndReasonsAndClears() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("lt-recents-live-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let photos = root.appendingPathComponent("Shoot")
        try FileManager.default.createDirectory(at: photos, withIntermediateDirectories: true)
        try Data().write(to: photos.appendingPathComponent("DSC0001.ARW"))
        let empty = root.appendingPathComponent("Empty")
        try FileManager.default.createDirectory(at: empty, withIntermediateDirectories: true)
        let card = URL(fileURLWithPath: "/Volumes/lt-test-nowhere-\(UUID())/DCIM")
        let defaults = testDefaults()
        let recents = RecentFolders(defaults: defaults)
        for d in [card, empty, photos] { recents.add(d) }
        let store = RustSessionStore(recents: recents, storeBase: root.appendingPathComponent("stores"))
        await store.setIgnoresTemporaryRecents(false)
        let m = makeModel(store: store)
        m.settings.sessionClosed = true
        await m.start()
        let e = m.recentEntries
        #expect(e.map(\.name) == ["Shoot", "Empty", "DCIM"])
        #expect(e.map(\.reason) == [nil, "No photos found", "Drive not connected"])
        #expect(e.allSatisfy { $0.lastOpened != nil })
        await m.clearRecents()
        #expect(m.sessionList.isEmpty && RecentFolders(defaults: defaults).folders.isEmpty)
        #expect(FileManager.default.fileExists(atPath: photos.appendingPathComponent("DSC0001.ARW").path), "nothing deleted")
        m.stop()
    }
}
