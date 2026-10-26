import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

private func scratch(_ tag: String) throws -> URL {
    let d = FileManager.default.temporaryDirectory.appendingPathComponent("lt-session-\(tag)-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

/// `makeModel` with an archive that is never reachable: no archive run starts, so
/// what Close Session asks and says depends only on the test.
@MainActor
private func quietModel(store: (any SessionStore)? = nil) -> AppModel {
    let m = AppModel(sessions: store ?? MockSessionStore(counts: [40, 5]), images: MockImageProvider(latency: .zero),
                     renderer: FakeRenderer(), importer: MockImportService(bytesPerSecond: 1e12),
                     archive: MockArchiveService(target: "/nonexistent/archive", isReachable: { _ in false }),
                     ai: MockAIService(), settings: makeSettings())
    m.watchCards = false
    m.sheet = nil // past onboarding
    return m
}

// MARK: - Names

@Suite struct SessionNameRulesTests {
    @Test func badNamesAreExplained() {
        let taken: Set<String> = ["fall hike"]
        #expect(SessionNameRules.problem("", taken: taken) == "Enter a name for the session.")
        #expect(SessionNameRules.problem("   \n", taken: taken) == "Enter a name for the session.")
        #expect(SessionNameRules.problem(".hidden", taken: taken)?.contains("period") == true)
        #expect(SessionNameRules.problem("a/b", taken: taken) == "A session name can’t contain “/”.")
        #expect(SessionNameRules.problem("a/b:c", taken: taken) == "A session name can’t contain “/” or “:”.")
        #expect(SessionNameRules.problem("a\u{7}b", taken: taken)?.contains("control") == true)
        #expect(SessionNameRules.problem(String(repeating: "é", count: 101), taken: taken) == "That name is too long.")
        #expect(SessionNameRules.problem("Fall HIKE ", taken: taken)?.contains("already exists") == true,
                "duplicates ignore case and surrounding spaces")
        #expect(SessionNameRules.problem("  Spring Hike ", taken: taken) == nil)
        #expect(SessionNameRules.problem(String(repeating: "a", count: 200), taken: taken) == nil)
    }

    @Test func theDefaultNameIsTodaysAndUnique() {
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "UTC")!
        let day = Date(timeIntervalSince1970: 1_791_000_000) // 2026-10-03 UTC
        #expect(SessionNameRules.defaultName(on: day, calendar: cal, taken: []) == "Session 2026-10-03")
        #expect(SessionNameRules.defaultName(on: day, calendar: cal,
                                             taken: ["session 2026-10-03", "session 2026-10-03 2"]) == "Session 2026-10-03 3")
    }

    @Test func pathsShowTheHomeFolderAsTilde() {
        #expect(AppModel.displayPath(URL(fileURLWithPath: "/Users/me/Pictures/Luxtable"), home: "/Users/me") == "~/Pictures/Luxtable")
        #expect(AppModel.displayPath(URL(fileURLWithPath: "/Users/me"), home: "/Users/me") == "~")
        #expect(AppModel.displayPath(URL(fileURLWithPath: "/Users/meg/x"), home: "/Users/me") == "/Users/meg/x")
        #expect(AppModel.displayPath(URL(fileURLWithPath: "/Volumes/pics"), home: "/Users/me") == "/Volumes/pics")
    }

    @Test func closingSaysWhatKeepsRunning() {
        #expect(AppModel.closedMessage("Hike", imports: 0, exporting: false) == "Closed “Hike”")
        #expect(AppModel.closedMessage("Hike", imports: 1, exporting: false) == "Closed “Hike” (the import keeps running)")
        #expect(AppModel.closedMessage("Hike", imports: 0, exporting: true) == "Closed “Hike” (the export keeps running)")
        #expect(AppModel.closedMessage("Hike", imports: 2, exporting: false) == "Closed “Hike” (2 imports keep running)")
        #expect(AppModel.closedMessage("Hike", imports: 1, exporting: true)
                == "Closed “Hike” (the import and the export keep running)")
        #expect(AppModel.closedMessage("Hike", imports: 2, exporting: true, archiving: true)
                == "Closed “Hike” (2 imports, the export and the archive run keep running)")
        #expect(AppModel.isArchiving(.syncing(fraction: -1)) && !AppModel.isArchiving(.idle)
                && !AppModel.isArchiving(.failed("x")))
    }

    @Test func closingWhileJobsRunAsksFirst() {
        #expect(SessionCloseAlert.make(session: "Hike", imports: 0, exporting: false, archiving: false) == nil)
        let one = SessionCloseAlert.make(session: "Hike", imports: 0, exporting: true, archiving: false)
        #expect(one?.title == "The export is still running")
        #expect(one?.message == "Close “Hike” anyway? It keeps running and finishes on its own. "
                + "Nothing is deleted, and the session stays in the Sessions list.")
        let archive = SessionCloseAlert.make(session: "Hike", imports: 0, exporting: false, archiving: true)
        #expect(archive?.title == "The archive run is still running")
        let many = SessionCloseAlert.make(session: "Hike", imports: 2, exporting: false, archiving: true)
        #expect(many?.title == "2 imports and the archive run are still running")
        #expect(many?.message.hasPrefix("Close “Hike” anyway? They keep running and finish on their own.") == true)
        #expect(SessionCloseAlert.make(session: "Hike", imports: 2, exporting: false, archiving: false)?.title
                == "2 imports are still running")
        #expect(SessionCloseAlert.confirm == "Close Session")
    }
}

// MARK: - New Session (mock store)

@MainActor
@Suite struct NewSessionTests {
    @Test func theSheetSuggestsAFreeNameAndValidatesAsYouType() async throws {
        let root = try scratch("new")
        defer { try? FileManager.default.removeItem(at: root) }
        let m = makeModel(store: MockSessionStore(counts: [3], root: root))
        await m.start()
        let today = SessionNameRules.defaultName(taken: [])
        // A folder of that name is already in the sessions folder: the next one is offered.
        try FileManager.default.createDirectory(at: root.appendingPathComponent(today), withIntermediateDirectories: true)
        m.beginNewSession()
        #expect(m.sheet == .newSession)
        let form = NewSessionModel(app: m)
        #expect(form.name == today && form.location == nil)
        await form.load()
        #expect(form.location == root)
        #expect(form.name == "\(today) 2", "the suggestion skips a folder already there")
        #expect(form.canCreate && form.problem == nil)
        form.name = "2026-09-21 fall hike"
        #expect(form.problem?.contains("already exists") == true && !form.canCreate)
        form.name = today.uppercased()
        #expect(form.problem?.contains("already exists") == true, "a folder in the location counts too")
        form.name = "a:b"
        #expect(form.problem?.contains("“:”") == true)
        form.name = ""
        #expect(await !form.create(), "nothing is created from an invalid name")
        #expect(m.sessionList.count == 1)
        // A name typed by the user is kept when the location loads.
        let typed = NewSessionModel(app: m)
        typed.name = "Mine"
        await typed.load()
        #expect(typed.name == "Mine")
        m.stop()
    }

    @Test func createOpensAndSelectsTheEmptySession() async throws {
        let root = try scratch("create")
        defer { try? FileManager.default.removeItem(at: root) }
        let m = makeModel(store: MockSessionStore(counts: [4], root: root))
        await m.start()
        m.selectAll()
        let form = NewSessionModel(app: m)
        form.name = "  Studio  "
        #expect(await form.create())
        #expect(!form.isCreating)
        #expect(m.currentSession?.name == "Studio")
        #expect(m.currentSession?.url == root.appendingPathComponent("Studio"))
        #expect(m.sessionList.first?.name == "Studio", "listed and selected")
        #expect(m.photos.isEmpty && m.selection.isEmpty && m.focusedID == nil, "the empty session's state")
        // Creating it again fails in the store; the sheet says why until the name changes.
        let again = NewSessionModel(app: m)
        again.name = "Studio"
        #expect(again.problem?.contains("already exists") == true)
        m.stop()
    }

    @Test func aChosenLocationIsCheckedAndUsed() async throws {
        let root = try scratch("loc")
        defer { try? FileManager.default.removeItem(at: root) }
        let other = root.appendingPathComponent("Elsewhere")
        let today = SessionNameRules.defaultName(taken: [])
        try FileManager.default.createDirectory(at: other.appendingPathComponent(today), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: other.appendingPathComponent("Taken"), withIntermediateDirectories: true)
        let m = makeModel(store: MockSessionStore(counts: [2], root: root))
        await m.start()
        let form = NewSessionModel(app: m)
        await form.load()
        #expect(form.location == root && form.name == today)
        form.name = "Taken"
        #expect(form.problem == nil, "free in the sessions folder")
        await form.choose(other)
        #expect(form.location == other)
        #expect(form.problem?.contains("already exists") == true, "a folder there has that name")
        form.name = today
        #expect(form.problem?.contains("already exists") == true)
        form.name = "Studio"
        #expect(await form.create())
        #expect(m.currentSession?.url == other.appendingPathComponent("Studio"), "created where chosen")
        // The suggestion moves on when the chosen folder already has it.
        let fresh = NewSessionModel(app: m)
        await fresh.choose(other)
        #expect(fresh.name == "\(today) 2")
        // Session-list names stay taken in any folder.
        fresh.name = "studio"
        #expect(fresh.problem?.contains("already exists") == true)
        // Without a creating store, choosing does nothing.
        let none = NewSessionModel(app: makeModel(store: SaveFailingStore()))
        await none.choose(other)
        #expect(none.location == nil)
        m.stop()
    }

    @Test func aStoreErrorIsShownUntilTheNameChanges() async {
        let m = makeModel(store: CreateFailingStore())
        await m.start()
        let form = NewSessionModel(app: m)
        form.name = "Fine"
        #expect(await !form.create())
        #expect(form.problem == "disk is read-only")
        form.name = "Fine 2"
        #expect(form.problem == nil)
        // With a chosen folder the store's other entry point fails the same way.
        await form.choose(URL(fileURLWithPath: "/nonexistent-lt-test/elsewhere"))
        #expect(await !form.create())
        #expect(form.problem == "disk is read-only")
        await form.choose(URL(fileURLWithPath: "/nonexistent-lt-test/again"))
        #expect(form.problem == nil, "choosing clears the error")
        m.stop()
    }

    @Test func storesThatCantCreateDisableNewSession() async {
        let m = makeModel(store: SaveFailingStore())
        #expect(!m.canCreateSession)
        m.beginNewSession()
        #expect(m.sheet == nil)
        #expect(!CommandCatalog.isEnabled(.newSession, model: m))
        #expect(CommandCatalog.help(.newSession, model: m) == "This mode can’t create sessions")
        #expect(m.emptyStateActions == m.emptyState.actions && !m.emptyStateActions.contains(.newSession))
        let form = NewSessionModel(app: m)
        await form.load()
        #expect(form.location == nil)
    }
}

/// A creating store whose `createSession` fails.
private actor CreateFailingStore: SessionCreating {
    let inner = MockSessionStore(counts: [2])
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { throw MockError.invalid("disk is read-only") }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func newSessionLocation() async -> URL { URL(fileURLWithPath: "/nonexistent-lt-test") }
    func createSession(named name: String, in folder: URL) async throws -> SessionInfo {
        throw MockError.invalid("disk is read-only")
    }
}

// MARK: - Close Session (mock store)

@MainActor
@Suite struct CloseSessionTests {
    @Test func menuEnablementFollowsTheOpenSession() async {
        let m = quietModel()
        #expect(!CommandCatalog.isEnabled(.closeSession, model: m), "nothing open yet")
        #expect(CommandCatalog.help(.closeSession, model: m) == "No session is open")
        #expect(CommandCatalog.isEnabled(.newSession, model: m))
        #expect(CommandCatalog.help(.newSession, model: m)?.contains("⇧⌘N") == true)
        await m.start()
        #expect(CommandCatalog.isEnabled(.closeSession, model: m))
        #expect(CommandCatalog.help(.closeSession, model: m)?.contains("Nothing is deleted") == true)
        #expect(CommandCatalog.title(.newSession, model: m) == "New Session…")
        #expect(CommandCatalog.title(.closeSession, model: m) == "Close Session")
        #expect(MenuCommand.newSession.shortcut == .init(key: "n", modifiers: [.command, .shift]))
        #expect(MenuCommand.closeSession.shortcut == .init(key: "w", modifiers: [.command, .option]))
        #expect(MenuCommand.allCases.allSatisfy { $0.shortcut != .init(key: "w", modifiers: [.command]) },
                "⌘W stays Close Window")
        let label = ToolbarControl.closeSession.control(m)
        #expect(label.label == "Close session 2026-09-21 Fall Hike")
        #expect(ToolbarControl.newSession.control(m).label == "New session")
        CommandCatalog.perform(.closeSession, model: m)
        #expect(await waitUntil { m.currentSession == nil })
        #expect(ToolbarControl.closeSession.control(m).label == "Close session")
        CommandCatalog.perform(.newSession, model: m)
        #expect(m.sheet == .newSession)
        m.stop()
    }

    @Test func closeSavesThenClearsEverythingAndDeletesNothing() async throws {
        let store = MockSessionStore(counts: [12, 3])
        let m = quietModel(store: store)
        m.toastDuration = .seconds(60) // the close's toast is read after slow steps
        await m.start()
        let session = try #require(m.currentSession)
        let first = try #require(m.focusedID)
        #expect(await waitUntil { m.viewer.frame != nil })
        m.selectAll()
        m.setRating(4) // pending (debounced) writes
        m.updateEdits([first], label: "Exposure") { _, e in e.exposure.ev = 0.7 }
        m.viewer.state.comparePinned = first
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        // Flushed: the store has the rating and the edit.
        #expect(try await store.photos(in: session).allSatisfy { $0.rating == 4 })
        #expect(try await store.edit(for: first).exposure.ev == 0.7)
        // Cleared.
        #expect(m.currentSession == nil)
        #expect(m.photos.isEmpty && m.visiblePhotos.isEmpty)
        #expect(m.selection.isEmpty && m.focusedID == nil)
        #expect(m.albums.isEmpty && m.edits.isEmpty && m.cullSuggestions.isEmpty)
        #expect(m.viewer.frame == nil && m.viewer.state.comparePinned == nil)
        #expect(!m.history.canUndo, "undo never reaches into the closed session")
        #expect(m.photoHistories.isEmpty, "nor does the History panel")
        #expect(m.smartFolders.folders.isEmpty)
        #expect(!m.cull.canMove)
        #expect(m.captureOne == CaptureOneStatus() && !m.confirmingCaptureOneRevert)
        // Released, kept, nothing deleted.
        #expect(await store.closed == [session.id])
        #expect(m.sessionList.contains { $0.id == session.id })
        #expect(try await store.listSessions().contains { $0.id == session.id })
        #expect(try await store.photos(in: session).count == 12)
        #expect(m.toast?.message == "Closed “\(session.name)”")
        #expect(m.settings.sessionClosed)
        // The no-session state: the recents, with New Session… first.
        #expect(m.emptyState == .recents)
        #expect(m.emptyStateActions == [.newSession, .importCard, .openFolder])
        #expect(EmptyState.Action.newSession.title == "New Session…")
        #expect(m.performEmptyStateAction(.newSession) && m.sheet == .newSession)
        m.sheet = nil
        // Closing again does nothing.
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        #expect(await store.closed == [session.id])
        m.stop()
    }

    @Test func aFailedSaveKeepsTheSessionOpen() async throws {
        let store = SaveFailingStore()
        let m = makeModel(store: store)
        await m.start()
        let session = try #require(m.currentSession)
        let first = try #require(m.focusedID)
        #expect(await waitUntil { m.viewer.frame != nil })
        m.setRating(5)
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        #expect(m.currentSession?.id == session.id, "nothing is dropped unsaved")
        #expect(m.focusedID == first && m.photos.count == 3)
        #expect(m.viewer.frame != nil, "the viewer is left as it was")
        #expect(m.toast?.isError == true && m.toast?.message.contains("couldn’t be saved") == true)
        #expect(!m.settings.sessionClosed)
        m.stop()
    }

    @Test func closeCancelsAICull() async throws {
        let m = AppModel(sessions: MockSessionStore(counts: [400]), images: MockImageProvider(latency: .zero),
                         renderer: FakeRenderer(), importer: MockImportService(bytesPerSecond: 1e12),
                         archive: MockArchiveService(target: "/nonexistent/archive", isReachable: { _ in true }),
                         ai: SlowAIService(), settings: makeSettings())
        m.watchCards = false
        m.sheet = nil // past onboarding
        await m.start()
        let cull = Task { await m.runAICull() }
        #expect(await waitUntil { m.cullProgress != nil && (m.cullProgress?.0 ?? 0) > 0 })
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        await cull.value
        #expect(m.cullProgress == nil)
        #expect(m.cullSuggestions.isEmpty, "a cull that finishes after the close is dropped")
        #expect(m.currentSession == nil)
        m.stop()
    }

    @Test func reopenAfterCloseAndRelaunch() async throws {
        let store = MockSessionStore(counts: [6, 2])
        let m = makeModel(store: store)
        await m.start()
        let session = try #require(m.currentSession)
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        // Relaunch: no session opens on its own.
        let relaunched = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                                  importer: MockImportService(bytesPerSecond: 1e12),
                                  archive: MockArchiveService(target: "/nonexistent/archive", isReachable: { _ in true }),
                                  ai: MockAIService(), settings: AppSettings(defaults: m.settings.userDefaults))
        relaunched.watchCards = false
        await relaunched.start()
        #expect(relaunched.sessionList.count == 2)
        #expect(relaunched.currentSession == nil && relaunched.photos.isEmpty)
        // Reopening it from the list.
        await relaunched.open(session)
        #expect(relaunched.currentSession?.id == session.id && relaunched.photos.count == 6)
        #expect(!relaunched.settings.sessionClosed, "opening a session undoes “don’t reopen”")
        // The next launch reopens as usual.
        let third = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                             importer: MockImportService(bytesPerSecond: 1e12),
                             archive: MockArchiveService(target: "/nonexistent/archive", isReachable: { _ in true }),
                             ai: MockAIService(), settings: AppSettings(defaults: m.settings.userDefaults))
        third.watchCards = false
        await third.start()
        #expect(third.currentSession?.id == session.id)
        m.stop()
        relaunched.stop()
        third.stop()
    }

    @Test func anExportKeepsRunningThroughTheClose() async throws {
        let m = quietModel(store: FileBackedStore(counts: [4]))
        await m.start()
        let fake = Exp6FakeExport()
        fake.hold = true
        m.exporter = fake
        m.selectAll()
        m.beginExport(store: ExportRecipeStore(defaults: testDefaults()))
        let export = try #require(m.exportModel)
        await export.prepared()
        export.start()
        #expect(await waitUntil { export.isRunning })
        #expect(!m.canCloseSession, "not while the export sheet is up (M4)")
        m.sheet = nil // the sheet was dismissed; the export runs on
        // Asked first; Cancel keeps the session open.
        var asked: [SessionCloseAlert] = []
        m.confirmClose = { asked.append($0); return false }
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        #expect(asked.map(\.title) == ["The export is still running"])
        #expect(m.currentSession != nil && m.photos.count == 4, "cancelled: still open")
        m.confirmClose = { asked.append($0); return true }
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        #expect(asked.count == 2)
        #expect(m.currentSession == nil)
        #expect(m.toast?.message.contains("the export keeps running") == true)
        #expect(export.isRunning && m.exportModel === export, "not cancelled")
        fake.hold = false
        #expect(await waitUntil { if case .finished = export.phase { true } else { false } })
        #expect(fake.jobs.count == 1 && fake.jobs[0].items.count == 4)
        m.stop()
    }

    @Test func anImportKeepsRunningThroughTheCloseAndLeavesItClosed() async throws {
        let store = MockSessionStore(counts: [3])
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(bytesPerSecond: 3e9, alreadyImportedEvery: 0, inventedCount: 40),
                         archive: MockArchiveService(target: "/nonexistent/x", isReachable: { _ in false }),
                         ai: MockAIService(), settings: makeSettings())
        m.watchCards = false
        m.sheet = nil // past onboarding
        await m.start()
        let registry = ImportRegistry()
        let session = try #require(m.currentSession)
        let im = ImportModel(volume: URL(fileURLWithPath: "/Volumes/CARD"), importer: m.importer, sessions: store,
                             existingSessions: m.sessionList, nameTemplate: "{date}", renameTemplate: "{name}",
                             ejector: FakeEjector(), inspector: removableInspector, registry: registry) { s in
            await m.importFinished(s)
        }
        await im.scan()
        im.sessionText = "Card Day"
        im.start()
        #expect(await waitUntil { im.phase == .importing })
        var asked: [SessionCloseAlert] = []
        m.confirmClose = { asked.append($0); return true }
        await m.closeSession(imports: registry, timeout: TestTiming.bound(.seconds(5)))
        #expect(asked.map(\.title) == ["The import is still running"])
        #expect(m.currentSession == nil)
        #expect(m.toast?.message.contains("the import keeps running") == true)
        await im.wait()
        guard case .finished(let summary) = im.phase else { Issue.record("\(im.phase)"); return }
        #expect(summary.files == 40, "the import was not stopped")
        #expect(m.currentSession == nil, "closed stays closed, as the alert said")
        #expect(m.sessionList.contains { $0.name == summary.sessionName }, "listed")
        #expect(m.toast?.message == "Import into “\(summary.sessionName)” finished")
        #expect(m.toast?.openSession?.name == summary.sessionName)
        #expect(await store.closed == [session.id])
        // Its Open button.
        await m.openToastSession()
        #expect(m.currentSession?.name == summary.sessionName)
        #expect(!m.settings.sessionClosed)
        // With a session open (or none closed by the user), an import opens its session as before.
        await m.importFinished(session)
        #expect(m.currentSession?.id == session.id)
        m.stop()
    }
}

/// AI Cull that takes a while per photo (cancellable).
private struct SlowAIService: AIService {
    func cull(_ photos: [Photo], images: any ImageProvider,
              progress: @escaping @Sendable (Int, Int) -> Void) async throws -> [CullSuggestion] {
        for i in photos.indices {
            try await Task.sleep(for: .milliseconds(5))
            progress(i + 1, photos.count)
        }
        return photos.map { CullSuggestion(photoID: $0.id, score: 0.9, pick: .picked, reasons: ["Sharp"]) }
    }

    func autoAdjust(_ photo: Photo, current: EditState, images: any ImageProvider) async throws -> EditState { current }
    func applyInstruction(_ text: String, to current: EditState) async throws -> EditState { current }
}

// MARK: - AI Denoise

extension DenoiseGlobalStore {
    @MainActor
    @Suite(.serialized) struct CloseSessionDenoiseTests {
        @Test func closingStopsOnlyWorkNobodyWaitsFor() async throws {
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("lt-close-dn-\(UUID())")
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            backend.tileCount = 400
            backend.tileDelay = .milliseconds(2)
            let s = DenoiseService(model: DenoiseGlobalStore.DenoiseServiceTests.store(dir), backend: backend,
                                   cacheDir: dir.appendingPathComponent("cache")) { _, _ in DoublingModel() }
            let url = DenoiseGlobalStore.DenoiseServiceTests.url
            s.enqueue(url(1), background: false) // the viewer's photo
            s.enqueue(url(2), background: true) // "Denoise Selected"
            let export = Task { try await s.denoise(url(3), background: true) } // an export waits for this one
            s.enqueue(url(4), background: true) // another session's photo
            #expect(await waitUntil { if case .running = s.states[url(1)] { true } else { false } })
            #expect(await waitUntil { s.states[url(3)] != nil })
            s.cancelUnwaited([url(1), url(2), url(3)])
            #expect(s.states[url(2)] == nil, "dequeued")
            #expect(await waitUntil { s.states[url(1)] == nil }, "the running one stops")
            backend.tileCount = 3
            try await export.value
            #expect(s.states[url(3)] == .ready, "the export's photo still denoises")
            #expect(await waitUntil { s.states[url(4)] == .ready }, "outside the session: untouched")
            #expect(!(try await backend.isReady(url(1))))
            #expect(!(try await backend.isReady(url(2))))
        }
    }
}

extension DenoiseGlobalStore {
    @MainActor
    @Suite(.serialized) struct CloseSessionDenoiseWiringTests {
        @Test func closingStopsTheSessionsQueuedDenoise() async throws {
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("lt-close-dn2-\(UUID())")
            defer { try? FileManager.default.removeItem(at: dir) }
            let m = makeModel(store: FileBackedStore(counts: [3]))
            await m.start()
            let urls = m.photos.compactMap(\.url)
            #expect(urls.count == 3)
            let backend = FakeBackend()
            backend.tileCount = 400
            backend.tileDelay = .milliseconds(2)
            let dn = DenoiseService(model: DenoiseGlobalStore.DenoiseServiceTests.store(dir), backend: backend,
                                    cacheDir: dir.appendingPathComponent("cache")) { _, _ in DoublingModel() }
            m.denoiser = dn
            for u in urls { dn.enqueue(u, background: true) }
            #expect(await waitUntil { if case .running = dn.states[urls[0]] { true } else { false } })
            await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
            #expect(await waitUntil { urls.allSatisfy { dn.states[$0] == nil } }, "its denoise stopped")
            for u in urls { #expect(!(try await backend.isReady(u))) }
            m.stop()
        }
    }
}

/// The mock's photos, each with a file URL (exports and AI Denoise need one).
private actor FileBackedStore: SessionCreating, SessionClosing {
    let inner: MockSessionStore
    init(counts: [Int]) { inner = MockSessionStore(counts: counts) }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] {
        try await inner.photos(in: session).map { p in
            var p = p
            p.url = URL(fileURLWithPath: "/nonexistent-lt-test/\(p.filename)")
            return p
        }
    }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func newSessionLocation() async -> URL { await inner.newSessionLocation() }
    func createSession(named name: String, in folder: URL) async throws -> SessionInfo {
        try await inner.createSession(named: name, in: folder)
    }
    func close(_ session: SessionInfo) async { await inner.close(session) }
}

// MARK: - Live (lt-session through RustSessionStore)

@MainActor
@Suite(.serialized) struct SessionLifecycleLiveTests {
    @Test func newSessionCreatesAnEmptyLiveSessionAndCloseReleasesIt() async throws {
        let root = try scratch("live")
        defer { try? FileManager.default.removeItem(at: root) }
        let lib = root.appendingPathComponent("Library")
        let defaults = testDefaults()
        let store = RustSessionStore(recents: RecentFolders(defaults: defaults), storeBase: root.appendingPathComponent("stores"),
                                     libraryRoot: { lib })
        let m = makeModel(store: store)
        await m.start()
        #expect(m.currentSession == nil, "nothing recent yet")
        let form = NewSessionModel(app: m)
        await form.load()
        #expect(form.location == lib)
        form.name = "Studio Day"
        #expect(await form.create())
        let s = try #require(m.currentSession)
        #expect(s.name == "Studio Day" && m.photos.isEmpty)
        for sub in ["Capture", "Selects", "Output", "Trash"] {
            #expect(FileManager.default.fileExists(atPath: lib.appendingPathComponent("Studio Day/\(sub)").path), "\(sub)")
        }
        #expect(await store.isOpen(s))
        // The same name again is refused, and the folder left as it was.
        await #expect(throws: (any Error).self) { _ = try await store.createSession(named: "Studio Day") }

        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        #expect(await !store.isOpen(s), "the Rust handle is released")
        #expect(m.currentSession == nil)
        #expect(FileManager.default.fileExists(atPath: lib.appendingPathComponent("Studio Day/.lt").path), "nothing deleted")
        // Still recent, and it reopens.
        let relaunched = RustSessionStore(recents: RecentFolders(defaults: defaults), storeBase: root.appendingPathComponent("stores"))
        #expect(try await relaunched.listSessions().map(\.name) == ["Studio Day"])
        await m.open(s)
        #expect(m.currentSession?.id == s.id)
        #expect(await store.isOpen(s))
        // Closed again, then a new one in a chosen folder (its parents are made).
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        let elsewhere = root.appendingPathComponent("Elsewhere/Shoots")
        let shoot = try await m.createSession(named: "Shoot", in: elsewhere)
        #expect(m.currentSession?.id == shoot.id && !m.settings.sessionClosed)
        #expect(FileManager.default.fileExists(atPath: elsewhere.appendingPathComponent("Shoot/Capture").path))
        #expect(await store.isOpen(shoot))
        #expect(await !store.isOpen(s))
        m.stop()
    }

    @Test func aStoreWithoutALibraryCantCreate() async throws {
        let root = try scratch("nolib")
        defer { try? FileManager.default.removeItem(at: root) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: root)
        await #expect(throws: (any Error).self) { _ = try await store.createSession(named: "X") }
        #expect(await store.newSessionLocation() == RustImportService.defaultLibraryRoot)
        #expect(try FileManager.default.contentsOfDirectory(atPath: root.path).isEmpty)
        // A chosen folder still works.
        let s = try await store.createSession(named: "X", in: root.appendingPathComponent("chosen"))
        #expect(s.name == "X")
        #expect(try FileManager.default.contentsOfDirectory(atPath: root.path).contains("chosen"))
    }

    /// A folder opened in place (real A9 files, copied to a temp folder): closing flushes
    /// its sidecars, releases the handle, and leaves the files alone.
    @Test func closingAnInPlaceFolderFlushesItsSidecars() async throws {
        guard let folder = try sampleFolderCopy(limit: 2) else { return }
        defer { try? FileManager.default.removeItem(at: folder) }
        let stores = folder.appendingPathComponent(".test-stores")
        let before = Set(try FileManager.default.contentsOfDirectory(atPath: folder.path))
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: stores)
        let info = try await store.add(folder: folder)
        let m = makeModel(store: store)
        await m.start()
        #expect(m.currentSession?.id == info.id && m.photos.count == 2)
        m.selectAll()
        m.setRating(3) // still pending when Close is chosen
        await m.closeSession(imports: ImportRegistry(), timeout: TestTiming.bound(.seconds(5)))
        #expect(await !store.isOpen(info))
        #expect(m.currentSession == nil && m.photos.isEmpty)
        // Flushed to the sidecars: a fresh store reads the ratings back.
        let fresh = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: stores)
        let again = try await fresh.add(folder: folder)
        #expect(try await fresh.photos(in: again).allSatisfy { $0.rating == 3 })
        #expect(Set(try FileManager.default.contentsOfDirectory(atPath: folder.path)).isSuperset(of: before),
                "nothing deleted")
        m.stop()
    }
}
