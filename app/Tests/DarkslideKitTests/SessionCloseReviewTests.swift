import AppKit
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Forwards to a RustSessionStore; rating writes take `saveDelay` (a slow drive) and
/// closing waits `closeDelay` first (a long Close tail).
private actor SlowRustStore: SessionStore, SessionClosing {
    let inner: RustSessionStore
    let saveDelay: Duration
    let closeDelay: Duration
    init(_ inner: RustSessionStore, saveDelay: Duration = .milliseconds(500), closeDelay: Duration = .zero) {
        self.inner = inner
        self.saveDelay = saveDelay
        self.closeDelay = closeDelay
    }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws {
        try? await Task.sleep(for: saveDelay)
        try await inner.save(photos: photos)
    }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func edits(in session: SessionInfo) async throws -> [PhotoID: EditState] { try await inner.edits(in: session) }
    func close(_ session: SessionInfo) async {
        try? await Task.sleep(for: closeDelay)
        await inner.close(session)
    }
}

private func stores(near folder: URL) -> URL {
    folder.deletingLastPathComponent().appendingPathComponent("ds-review-stores-\(UUID())")
}

// MARK: - H1, M1: the close flush and its tail (real RustSessionStore)

@MainActor
@Suite(.serialized) struct SessionCloseFlushTests {
    @Test func aRatingMadeWhileCloseFlushesIsSaved() async throws {
        guard let folder = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: folder) }
        let base = stores(near: folder)
        defer { try? FileManager.default.removeItem(at: base) }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let info = try await rust.add(folder: folder)
        let m = makeModel(store: SlowRustStore(rust))
        await m.start()
        #expect(m.currentSession?.id == info.id && m.photos.count == 2)
        m.selectAll()
        m.setRating(3)
        // A generous bound: this checks what is written, not how fast (the machine may be busy).
        let close = Task { await m.closeSession(imports: ImportRegistry(), timeout: .seconds(60)) }
        #expect(await waitUntil { m.saver.state == .saving })
        m.setRating(5) // the user keeps culling while the close is writing
        await close.value
        #expect(m.currentSession == nil)
        #expect(!m.saver.hasUnsaved, "nothing left behind")
        let fresh = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let again = try await fresh.add(folder: folder)
        #expect(try await fresh.photos(in: again).allSatisfy { $0.rating == 5 }, "the last rating is on disk")
        m.stop()
    }

    @Test func aRatingMadeWhileCloseFlushesNeverLandsInAnotherSession() async throws {
        guard let a = try sampleFolderCopy(limit: 2), let b = try sampleFolderCopy(limit: 2) else { return }
        defer {
            try? FileManager.default.removeItem(at: a)
            try? FileManager.default.removeItem(at: b)
        }
        let base = stores(near: a)
        defer { try? FileManager.default.removeItem(at: base) }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let infoB = try await rust.add(folder: b)
        let infoA = try await rust.add(folder: a)
        let m = makeModel(store: SlowRustStore(rust))
        await m.start()
        await m.open(infoB) // B was looked at earlier in this run
        await m.open(infoA)
        #expect(m.currentSession?.id == infoA.id && m.photos.count == 2)
        m.selectAll()
        m.setRating(3)
        // A generous bound: this checks what is written, not how fast (the machine may be busy).
        let close = Task { await m.closeSession(imports: ImportRegistry(), timeout: .seconds(60)) }
        #expect(await waitUntil { m.saver.state == .saving })
        m.setRating(5)
        await close.value
        let fresh = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let againB = try await fresh.add(folder: b)
        #expect(try await fresh.photos(in: againB).allSatisfy { $0.rating == 0 }, "B was never rated")
        let againA = try await fresh.add(folder: a)
        #expect(try await fresh.photos(in: againA).allSatisfy { $0.rating == 5 })
        m.stop()
    }

    /// A write for a closed session's photo fails loudly, and never reaches another
    /// session whose photos share its ids.
    @Test func aLateSaveAfterCloseFailsAndTouchesNothingElse() async throws {
        guard let a = try sampleFolderCopy(limit: 2), let b = try sampleFolderCopy(limit: 2) else { return }
        defer {
            try? FileManager.default.removeItem(at: a)
            try? FileManager.default.removeItem(at: b)
        }
        let base = stores(near: a)
        defer { try? FileManager.default.removeItem(at: base) }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let infoB = try await rust.add(folder: b)
        let infoA = try await rust.add(folder: a)
        _ = try await rust.photos(in: infoB)
        var pa = try await rust.photos(in: infoA)
        await rust.close(infoA)
        pa[0].rating = 5
        await #expect(throws: (any Error).self) { try await rust.save(photos: [pa[0]]) }
        var e = EditState()
        e.exposure.ev = 1
        await #expect(throws: (any Error).self) { try await rust.save(edits: [pa[0].id: e]) }
        #expect(try await rust.photos(in: infoB).allSatisfy { $0.rating == 0 })
        #expect(try await rust.edits(in: infoB).isEmpty)
        // Reopened, A takes writes again.
        let reopened = try await rust.photos(in: infoA)
        var p = reopened[0]
        p.rating = 4
        try await rust.save(photos: [p])
        #expect(try await rust.photos(in: infoA).first { $0.id == p.id }?.rating == 4)
    }

    @Test func reopeningDuringTheCloseTailKeepsItsHandle() async throws {
        guard let folder = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: folder) }
        let base = stores(near: folder)
        defer { try? FileManager.default.removeItem(at: base) }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let info = try await rust.add(folder: folder)
        let m = makeModel(store: SlowRustStore(rust, saveDelay: .zero, closeDelay: .milliseconds(400)))
        await m.start()
        let close = Task { await m.closeSession(imports: ImportRegistry()) }
        #expect(await waitUntil { m.currentSession == nil })
        await m.open(info) // picked from the list again while the store is still closing it
        await close.value
        #expect(m.currentSession?.id == info.id)
        #expect(await rust.isOpen(info), "the reopened session keeps its handle")
        m.selectAll()
        m.setRating(2)
        await m.saver.flush()
        #expect(m.saver.state == .saved)
        let fresh = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let again = try await fresh.add(folder: folder)
        #expect(try await fresh.photos(in: again).allSatisfy { $0.rating == 2 })
        m.stop()
    }
}

// MARK: - L1: a bounded flush, one close at a time

/// A mock store whose rating writes take `delay`.
private actor DelayStore: SessionStore, SessionClosing {
    let inner = MockSessionStore(counts: [3])
    let delay: Duration
    private(set) var closed = 0
    init(delay: Duration) { self.delay = delay }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try? await Task.sleep(for: delay) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws {}
    func close(_ session: SessionInfo) async { closed += 1 }
}

@MainActor
@Suite struct SessionCloseGuardTests {
    @Test func aFlushThatTakesTooLongKeepsTheSessionOpen() async throws {
        let store = DelayStore(delay: .seconds(3))
        let m = makeModel(store: store)
        await m.start()
        let session = try #require(m.currentSession)
        m.selectAll()
        m.setRating(4)
        await m.closeSession(imports: ImportRegistry(), timeout: .milliseconds(200))
        #expect(m.currentSession?.id == session.id, "still open")
        #expect(m.toast?.isError == true && m.toast?.message.contains("taking too long") == true)
        #expect(await store.closed == 0)
        m.stop()
    }

    @Test func aSecondCloseWhileOneRunsIsIgnored() async throws {
        let store = DelayStore(delay: .milliseconds(300))
        let m = makeModel(store: store)
        await m.start()
        m.selectAll()
        m.setRating(2)
        let first = Task { await m.closeSession(imports: ImportRegistry()) }
        #expect(await waitUntil { !m.canCloseSession })
        #expect(!CommandCatalog.isEnabled(.closeSession, model: m), "⌥⌘W is off while closing")
        await m.closeSession(imports: ImportRegistry())
        await first.value
        #expect(await store.closed == 1)
        #expect(m.currentSession == nil)
        m.stop()
    }

    /// M4: nothing replaces a sheet that is up (an export would be cancelled).
    @Test func sessionCommandsWaitForTheOpenSheet() async throws {
        let m = makeModel()
        await m.start()
        m.sheet = .export
        #expect(!CommandCatalog.isEnabled(.newSession, model: m))
        #expect(!CommandCatalog.isEnabled(.closeSession, model: m))
        #expect(!m.canOpenRecent)
        CommandCatalog.perform(.newSession, model: m)
        #expect(m.sheet == .export)
        #expect(!m.beginOpen() && m.sheet == .export)
        #expect(CommandCatalog.perform(.openFolder, model: m) == .none, "no folder panel over the sheet either")
        await m.closeSession(imports: ImportRegistry())
        #expect(m.currentSession != nil && m.sheet == .export)
        m.sheet = nil
        #expect(CommandCatalog.isEnabled(.newSession, model: m) && CommandCatalog.isEnabled(.closeSession, model: m))
        #expect(m.canOpenRecent)
        m.stop()
    }

    /// L5: the closed session's identity pass no longer shows progress.
    @Test func hashingTicksOfAClosedSessionAreIgnored() async throws {
        let m = makeModel()
        await m.start()
        let url = try #require(m.currentSession?.url)
        await m.closeSession(imports: ImportRegistry())
        m.receive(OpenProgress(phase: .hashing, done: 1, total: 10), for: url)
        #expect(m.openProgress == nil)
        m.stop()
    }
}

// MARK: - M2, M3, L3: where new sessions go, and what a new one looks like

@MainActor
@Suite(.serialized) struct NewSessionLocationTests {
    @Test func aNewEmptySessionIsAvailableAndReopensNextLaunch() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-review-new-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let lib = root.appendingPathComponent("Library")
        let defaults = testDefaults()
        let store = RustSessionStore(recents: RecentFolders(defaults: defaults), storeBase: root.appendingPathComponent("s"),
                                     libraryRoot: { lib })
        let m = makeModel(store: store)
        await m.start()
        let s = try await m.createSession(named: "Empty Shoot")
        #expect(await RecentsHygiene.unavailableReason(s.url) == nil, "an empty session is still a session")
        // An empty folder that isn't a session still says so.
        let plain = root.appendingPathComponent("Plain")
        try FileManager.default.createDirectory(at: plain, withIntermediateDirectories: true)
        #expect(await RecentsHygiene.unavailableReason(plain) == .noPhotos)
        m.stop()
        let relaunched = makeModel(store: RustSessionStore(recents: RecentFolders(defaults: defaults),
                                                            storeBase: root.appendingPathComponent("s")))
        relaunched.settings.sessionClosed = false
        await relaunched.start()
        #expect(relaunched.currentSession?.name == "Empty Shoot")
        relaunched.stop()
    }

    @Test func aSessionCantBeCreatedInsideAnother() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-review-nest-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let lib = root.appendingPathComponent("Library")
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: root.appendingPathComponent("s"),
                                     libraryRoot: { lib })
        let m = makeModel(store: store)
        await m.start()
        let outer = try await m.createSession(named: "Outer")
        let capture = outer.url.appendingPathComponent("Capture")
        // The store refuses it.
        await #expect(throws: (any Error).self) { _ = try await store.createSession(named: "Inner", in: capture) }
        await #expect(throws: (any Error).self) { _ = try await store.createSession(named: "Inner", in: outer.url) }
        #expect(!FileManager.default.fileExists(atPath: capture.appendingPathComponent("Inner").path))
        // The sheet says so before Create.
        let form = NewSessionModel(app: m)
        form.name = "Inner"
        await form.choose(capture)
        #expect(form.problem?.contains("inside") == true && !form.canCreate)
        await form.choose(lib)
        #expect(form.problem == nil)
        // A photo folder opened in place (listed in the session list) counts too.
        #expect(SessionLocation.problem(outer.url.appendingPathComponent("Selects/x"), sessions: [outer]) != nil)
        #expect(SessionLocation.problem(lib, sessions: [outer]) == nil)
        m.stop()
    }

    @Test func cloudFoldersGetAWarning() {
        let home = "/Users/me"
        #expect(SessionLocation.cloudWarning(URL(fileURLWithPath: "\(home)/Library/Mobile Documents/com~apple~CloudDocs/S"))
            != nil)
        #expect(SessionLocation.cloudWarning(URL(fileURLWithPath: "\(home)/Library/CloudStorage/Dropbox/S")) != nil)
        #expect(SessionLocation.cloudWarning(URL(fileURLWithPath: "\(home)/Dropbox/Shoots")) != nil)
        #expect(SessionLocation.cloudWarning(URL(fileURLWithPath: "\(home)/Pictures/Darkslide")) == nil)
    }

    @Test func aFailedCreateRemovesOnlyWhatItMade() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-review-clean-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let existing = root.appendingPathComponent("Existing")
        try FileManager.default.createDirectory(at: existing, withIntermediateDirectories: true)
        try Data("keep".utf8).write(to: existing.appendingPathComponent("keep.txt"))
        let dir = existing.appendingPathComponent("New/Deeper/Session")
        let made = SessionLocation.missing(dir)
        #expect(made.map(\.lastPathComponent) == ["Session", "Deeper", "New"])
        try FileManager.default.createDirectory(at: dir.appendingPathComponent("Capture"), withIntermediateDirectories: true)
        SessionLocation.cleanUp(made)
        #expect(!FileManager.default.fileExists(atPath: existing.appendingPathComponent("New").path))
        #expect(FileManager.default.fileExists(atPath: existing.appendingPathComponent("keep.txt").path))
        // A parent someone else filled meanwhile stays.
        let made2 = SessionLocation.missing(dir)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try Data().write(to: existing.appendingPathComponent("New/theirs.txt"))
        SessionLocation.cleanUp(made2)
        #expect(FileManager.default.fileExists(atPath: existing.appendingPathComponent("New/theirs.txt").path))
        #expect(!FileManager.default.fileExists(atPath: existing.appendingPathComponent("New/Deeper").path))
    }
}

// MARK: - L6: ⌥⌘W belongs to Close Session

@MainActor
@Suite struct MenuKeyTests {
    @Test func closeAllNeverKeepsOptionCommandW() {
        let main = NSMenu()
        let fileItem = NSMenuItem(title: "File", action: nil, keyEquivalent: "")
        let file = NSMenu(title: "File")
        fileItem.submenu = file
        main.addItem(fileItem)
        let close = NSMenuItem(title: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        file.addItem(close)
        let closeAll = NSMenuItem(title: "Close All", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        closeAll.keyEquivalentModifierMask = [.command, .option]
        closeAll.isAlternate = true
        file.addItem(closeAll)
        let session = NSMenuItem(title: "Close Session", action: nil, keyEquivalent: "w")
        session.keyEquivalentModifierMask = [.command, .option]
        session.isEnabled = false
        file.addItem(session)
        #expect(MenuKeys.duplicates(in: main).count == 1)
        MenuKeys.reserveSessionKeys(in: main)
        #expect(MenuKeys.duplicates(in: main).isEmpty)
        let dump = MenuKeys.dump(main)
        #expect(dump.contains("⌥⌘W  File ▸ Close Session"))
        #expect(dump.contains("⌘W  File ▸ Close"))
        #expect(!dump.contains { $0.hasSuffix("Close All") })
        #expect(file.items.contains { $0.title == "Close All" }, "the item stays, without the key")
        #expect(MenuKeys.isInside(file, main) && MenuKeys.isInside(main, main) && !MenuKeys.isInside(NSMenu(), main))
        // The app's own catalog: no other command takes ⌥⌘W or ⇧⌘N.
        let shortcuts = MenuCommand.allCases.compactMap(\.shortcut)
        #expect(shortcuts.filter { $0 == MenuCommand.closeSession.shortcut }.count == 1)
        #expect(shortcuts.filter { $0 == MenuCommand.newSession.shortcut }.count == 1)
    }
}
