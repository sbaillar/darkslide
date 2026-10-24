import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// The real store; its close starts only after `closeDelay` (a slow archive/store tail).
private actor LateClose: SessionStore, SessionClosing {
    let inner: RustSessionStore
    let closeDelay: Duration
    init(_ inner: RustSessionStore, closeDelay: Duration) {
        self.inner = inner
        self.closeDelay = closeDelay
    }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func edits(in session: SessionInfo) async throws -> [PhotoID: EditState] { try await inner.edits(in: session) }
    func close(_ session: SessionInfo) async {
        try? await Task.sleep(for: closeDelay)
        await inner.close(session)
    }
}

@MainActor
@Suite(.serialized) struct ReopenAfterBoundedCloseTests {
    private func setUp() throws -> (folder: URL, base: URL, rust: RustSessionStore)? {
        guard let folder = try sampleFolderCopy(limit: 2) else { return nil }
        let base = folder.deletingLastPathComponent().appendingPathComponent("ds-rr3-\(UUID())")
        return (folder, base, RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base))
    }

    private func ratingsOnDisk(_ folder: URL, base: URL) async throws -> [Int] {
        let fresh = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let again = try await fresh.add(folder: folder)
        return try await fresh.photos(in: again).map(\.rating)
    }

    /// The reviewer's repro: the same session reopened after Close's bounded waits gave
    /// up; the old close tail, still running, must not take the reopened handle.
    @Test func reopeningAfterTheBoundedWaitsKeepsItsHandle() async throws {
        guard let (folder, base, rust) = try setUp() else { return }
        defer {
            try? FileManager.default.removeItem(at: folder)
            try? FileManager.default.removeItem(at: base)
        }
        let info = try await rust.add(folder: folder)
        let m = makeModel(store: LateClose(rust, closeDelay: .seconds(2)))
        m.closeTailLimit = .milliseconds(200) // stands in for 10 s
        m.openWaitLimit = .milliseconds(300) // stands in for 15 s
        await m.start()
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        #expect(m.currentSession == nil && m.closing == nil)
        await m.open(info) // picked again; it waits for its own release first
        #expect(m.currentSession?.id == info.id)
        try await Task.sleep(for: .milliseconds(500))
        #expect(await rust.isOpen(info), "the reopened session keeps its handle")
        m.selectAll()
        m.setRating(4)
        await m.saver.flush()
        #expect(m.saver.state == .saved, "\(m.saver.state)")
        #expect(try await ratingsOnDisk(folder, base: base).allSatisfy { $0 == 4 })
        #expect(m.releasing.isEmpty, "released")
        m.stop()
    }

    /// Another session opens without waiting for the slow release.
    @Test func otherSessionsDontWaitForTheRelease() async throws {
        guard let (folder, base, rust) = try setUp(), let other = try sampleFolderCopy(limit: 1) else { return }
        defer {
            try? FileManager.default.removeItem(at: folder)
            try? FileManager.default.removeItem(at: other)
            try? FileManager.default.removeItem(at: base)
        }
        let second = try await rust.add(folder: other)
        _ = try await rust.add(folder: folder)
        let m = makeModel(store: LateClose(rust, closeDelay: .seconds(30)))
        m.closeTailLimit = .milliseconds(200)
        await m.start()
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        let clock = ContinuousClock()
        let start = clock.now
        await m.open(second)
        #expect(m.currentSession?.id == second.id)
        #expect(clock.now - start < TestTiming.bound(.seconds(20)), "not held up by the other session's release")
        m.stop()
    }

    /// The import-finished trigger: an import into the closed session finishes while
    /// its release is still running, and its toast's Open reopens it safely.
    @Test func anImportFinishingIntoItReopensSafely() async throws {
        guard let (folder, base, rust) = try setUp() else { return }
        defer {
            try? FileManager.default.removeItem(at: folder)
            try? FileManager.default.removeItem(at: base)
        }
        let info = try await rust.add(folder: folder)
        let m = makeModel(store: LateClose(rust, closeDelay: .seconds(2)))
        m.closeTailLimit = .milliseconds(200)
        m.openWaitLimit = .milliseconds(300)
        await m.start()
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        await m.importFinished(info)
        #expect(m.currentSession == nil && m.toast?.openSession?.id == info.id)
        await m.openToastSession()
        #expect(m.currentSession?.id == info.id)
        #expect(await rust.isOpen(info))
        // And when the user hadn't closed it (a plain import into it): the same wait.
        m.settings.sessionClosed = false
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        m.settings.sessionClosed = false
        await m.importFinished(info)
        #expect(m.currentSession?.id == info.id)
        try await Task.sleep(for: .milliseconds(500))
        #expect(await rust.isOpen(info))
        m.selectAll()
        m.setRating(2)
        await m.saver.flush()
        #expect(try await ratingsOnDisk(folder, base: base).allSatisfy { $0 == 2 })
        m.stop()
    }
}

@MainActor
@Suite struct OpenWhileReleasingTests {
    /// A click on a session still being released (a hung volume) shows it's loading, and
    /// a session opened meanwhile isn't replaced when that release finally ends.
    @Test func aNewerOpenWinsOverOneWaitingForItsRelease() async {
        let m = makeModel(counts: [3, 2])
        await m.start()
        let (a, b) = (m.sessionList[0], m.sessionList[1])
        let (gate, released) = AsyncStream<Void>.makeStream()
        m.releasing[a.id] = Task { for await _ in gate {} }
        let first = Task { await m.open(a) }
        #expect(await waitUntil(timeout: .seconds(2)) { m.isLoading }, "the click shows it's working")
        await m.open(b)
        #expect(m.currentSession?.id == b.id)
        released.finish()
        await first.value
        #expect(m.currentSession?.id == b.id, "the older click doesn't replace the newer one")
        #expect(!m.isLoading)
        await m.open(a)
        #expect(m.currentSession?.id == a.id, "a later open of it still works")
        m.stop()
    }
}

/// The real store; loading one session's photos waits until `release()` (a slow drive).
private actor HeldLoad: SessionStore, CurrentSessionPointing {
    let inner: RustSessionStore
    private var held: UUID?
    private(set) var holding = false
    private let gate: AsyncStream<Void>
    private let opened: AsyncStream<Void>.Continuation
    init(_ inner: RustSessionStore) {
        self.inner = inner
        (gate, opened) = AsyncStream<Void>.makeStream()
    }
    func hold(_ id: UUID) { held = id }
    func release() { opened.finish() }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] {
        if session.id == held {
            holding = true
            for await _ in gate {}
        }
        return try await inner.photos(in: session)
    }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func edits(in session: SessionInfo) async throws -> [PhotoID: EditState] { try await inner.edits(in: session) }
    func makeCurrent(_ session: SessionInfo) async { await inner.makeCurrent(session) }
}

@MainActor
@Suite struct OpenSupersededAfterLoadingTests {
    /// A's open loaded A (the store now resolves bare photo ids in A) after B was opened
    /// and shown: the store is pointed back at B, so B's ratings never land on A's photos.
    @Test func theStoreIsPointedBackAtTheShownSession() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-held-\(UUID())", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: root.appendingPathComponent("stores"))
        let a = try await rust.createSession(named: "A", in: root)
        let b = try await rust.createSession(named: "B", in: root)
        let store = HeldLoad(rust)
        await store.hold(a.id)
        let m = makeModel(store: store)
        let first = Task { await m.open(a) }
        for _ in 0..<10_000 where await !store.holding { await Task.yield() }
        #expect(await store.holding)
        await m.open(b)
        #expect(m.currentSession?.id == b.id)
        await store.release()
        await first.value
        #expect(m.currentSession?.id == b.id && !m.isLoading)
        #expect(await rust.currentSessionID == b.id, "bare photo ids resolve in the shown session")
        m.stop()
    }

    /// An import into another session reads that session's photos (which points the store
    /// at it): the store is pointed back at the session shown.
    @Test func anImportIntoAnotherSessionLeavesTheStoreOnTheShownSession() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-import-\(UUID())", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let rust = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: root.appendingPathComponent("stores"))
        let a = try await rust.createSession(named: "A", in: root)
        let b = try await rust.createSession(named: "B", in: root)
        let m = makeModel(store: rust)
        await m.open(b)
        #expect(await rust.currentSessionID == b.id)
        let im = ImportModel(volume: URL(fileURLWithPath: "/Volumes/CARD"),
                             importer: MockImportService(bytesPerSecond: 3e9, alreadyImportedEvery: 0, inventedCount: 2),
                             sessions: rust, existingSessions: [a, b], nameTemplate: "{date}", renameTemplate: "{name}",
                             preferredSession: a.url, ejector: FakeEjector(), inspector: removableInspector,
                             registry: ImportRegistry(), shownSession: { m.currentSession }) { _ in }
        await im.scan()
        #expect(im.destination == .existing(a.id))
        im.start()
        await im.wait()
        #expect(await rust.currentSessionID == b.id, "B's bare photo ids still resolve in B")
        m.stop()
    }
}

// MARK: - Dropped changes are shown, not silent

/// A store that says some changes can never be written.
private actor DroppingStore: SessionStore {
    let inner = MockSessionStore(counts: [3])
    var drop = true
    func setDrop(_ on: Bool) { drop = on }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws {
        if drop { throw UnsavableChanges(ids: Set(photos.map(\.id))) }
        try await inner.save(photos: photos)
    }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
}

@MainActor
@Suite struct DroppedChangesTests {
    @Test func aDropIsAPersistentNotSavedStateWithAToast() async throws {
        let store = DroppingStore()
        let m = makeModel(store: store)
        await m.start()
        let id = try #require(m.focusedID)
        m.setRating(3)
        await m.saver.flush()
        guard case .dropped(let message, let count) = m.saver.state else { Issue.record("\(m.saver.state)"); return }
        #expect(count == 1 && message.contains("closed"))
        #expect(m.toast?.isError == true && m.toast?.message.contains("closed") == true)
        #expect(!m.saver.hasUnsaved, "nothing to retry: close and quit aren't held up")
        // Later saves work, and the notice stays until dismissed.
        await store.setDrop(false)
        m.setRating(5)
        await m.saver.flush()
        #expect(m.saver.state == .dropped(message, count: 1))
        #expect(try await store.photos(in: try #require(m.currentSession)).first { $0.id == id }?.rating == 5)
        m.saver.dismissDropped()
        #expect(m.saver.state == .saved && m.saver.dropped == 0)
        m.saver.dismissDropped()
        #expect(m.saver.state == .saved)
        // Close isn't refused by a drop notice.
        await store.setDrop(true)
        m.setRating(1)
        await m.saver.flush()
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        #expect(m.currentSession == nil)
        m.stop()
    }
}
