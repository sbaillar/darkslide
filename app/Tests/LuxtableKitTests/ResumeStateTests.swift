import Foundation
import Testing
@testable import LuxtableCore

/// After an update's restart the app reopens where the user was (`ResumeState`).
@MainActor
@Suite struct ResumeStateTests {
    /// The mock's sessions with every photo id moved by `offset` (an index rebuilt with new
    /// ids), without the photos named in `dropping`.
    struct Renumbered: SessionStore {
        let inner: MockSessionStore
        var offset: Int64 = 1000
        var dropping: Set<String> = []
        func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
        func photos(in session: SessionInfo) async throws -> [Photo] {
            try await inner.photos(in: session).filter { !dropping.contains($0.filename) }.map {
                Photo(id: PhotoID($0.id.raw + offset), filename: $0.filename, meta: $0.meta, rating: $0.rating)
            }
        }
        func albums(in session: SessionInfo) async throws -> [Album] { [] }
        func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
        func save(photos: [Photo]) async throws {}
        func edit(for id: PhotoID) async throws -> EditState { EditState() }
        func save(edits: [PhotoID: EditState]) async throws {}
    }

    /// A first run: on photo 18 of the first session, 17…18 selected, grid, panels hidden.
    static func place(_ store: MockSessionStore) async -> (ResumeState, PhotoID, [PhotoID]) {
        let m = makeModel(store: store)
        await m.start()
        let a = m.visibleIDs[16], b = m.visibleIDs[17]
        m.focus(a, extend: false)
        m.focus(b, extend: true)
        m.browserMode = .grid
        m.panelsHidden = true
        m.filmstripHidden = true
        m.toolTab = .color
        m.filter.pick = .unrejected
        m.viewer.state.fullScreen = true
        return (m.resumeState(updatedTo: "0.1.19", notes: ["New."]), b, [a, b])
    }

    @Test func theNextLaunchIsOnTheSamePhotoInTheSameView() async throws {
        let store = MockSessionStore(counts: [40, 5])
        let (saved, focused, selected) = await Self.place(store)
        #expect(saved.focusedID == focused.raw)
        #expect(saved.updatedTo == "0.1.19")
        // Through the file, as across a relaunch.
        let file = FileManager.default.temporaryDirectory.appendingPathComponent("lt-resume-\(UUID())/resume.json")
        defer { try? FileManager.default.removeItem(at: file.deletingLastPathComponent()) }
        try ResumeStore(url: file).save(saved)
        let r = try #require(ResumeStore(url: file).consume())
        #expect(r == saved)
        let m = makeModel(store: store)
        m.pendingResume = r
        await m.start()
        #expect(m.focusedID == focused)
        #expect(m.selection == Set(selected))
        #expect(m.browserMode == .grid)
        #expect(m.panelsHidden && m.filmstripHidden)
        #expect(m.toolTab == .color)
        #expect(m.filter.pick == .unrejected)
        #expect(m.viewer.state.fullScreen)
        #expect(m.pendingResume == nil, "used once")
        // Opening the session again later: its first photo, as usual.
        let session = try #require(m.currentSession)
        m.focus(m.visibleIDs[3], extend: false)
        await m.open(session)
        #expect(m.focusedID == m.visibleIDs.first)
    }

    @Test func theResumedSessionOpensEvenIfAnotherIsMoreRecent() async throws {
        let store = MockSessionStore(counts: [40, 5])
        let first = makeModel(store: store)
        await first.start()
        await first.open(first.sessionList[1])
        let target = first.visibleIDs[3]
        first.focus(target, extend: false)
        let r = first.resumeState()
        #expect(r.updatedTo == nil)
        let m = makeModel(store: store)
        m.pendingResume = r
        await m.start()
        #expect(m.currentSession?.id == first.sessionList[1].id)
        #expect(m.focusedID == target)
        #expect(m.selection == [target])
    }

    @Test func movedIdsFallBackToTheFileAndTheFolderPath() async throws {
        let (r, focused, _) = await Self.place(MockSessionStore(counts: [40, 5]))
        let name = MockSessionStore.makePhotos(count: 40, firstID: 1, start: Date()).first { $0.id == focused }?.filename
        // Another store: other session ids (the folder's path still matches), other photo ids.
        let m = makeModel(store: Renumbered(inner: MockSessionStore(counts: [40, 5])))
        m.pendingResume = r
        await m.start()
        #expect(m.focusedID == PhotoID(focused.raw + 1000))
        #expect(m.focusedPhoto?.filename == name)
        #expect(m.selection == [PhotoID(focused.raw + 1000)], "the old ids name other photos: only the focused one is kept")
    }

    @Test func aPhotoThatIsGoneFallsBackToTheFirst() async throws {
        let (r, focused, _) = await Self.place(MockSessionStore(counts: [40, 5]))
        let name = try #require(r.focusedName)
        let m = makeModel(store: Renumbered(inner: MockSessionStore(counts: [40, 5]), offset: 0, dropping: [name]))
        m.pendingResume = r
        await m.start()
        #expect(m.photo(focused) == nil)
        #expect(m.focusedID == m.visibleIDs.first)
        #expect(m.browserMode == .grid, "the rest of the place is still restored")
        #expect(m.viewer.state.fullScreen)
        #expect(m.pendingResume == nil)
    }

    @Test func aFilterThatWouldHideThePhotoGivesWay() async throws {
        let store = MockSessionStore(counts: [40, 5])
        var (r, focused, _) = await Self.place(store)
        r.filter?.minRating = 5 // the photo isn't rated 5 any more
        r.filter?.colorTag = ColorTag.red.rawValue
        let m = makeModel(store: store)
        m.pendingResume = r
        await m.start()
        #expect(m.focusedID == focused)
        #expect(!m.filter.isActive)
    }

    @Test func aSessionThatIsGoneOpensTheMostRecent() async throws {
        var r = ResumeState()
        r.sessionID = UUID()
        r.sessionPath = "/nowhere/Gone"
        r.focusedID = 3
        let m = makeModel()
        m.pendingResume = r
        await m.start()
        #expect(m.currentSession?.id == m.sessionList.first?.id)
        #expect(m.focusedID == m.visibleIDs.first)
        #expect(m.pendingResume == nil)
    }

    @Test func noSessionOpenKeepsOnlyTheUpdate() {
        let m = makeModel()
        let r = m.resumeState(updatedTo: "1.0.0", notes: ["A"])
        #expect(r.sessionID == nil && r.focusedID == nil && r.browserMode == nil)
        #expect(r.updatedTo == "1.0.0" && r.notes == ["A"])
    }

    @Test func thePhotoIsMatchedByIdThenPathThenName() {
        func photo(_ id: Int64, _ name: String, _ path: String?) -> Photo {
            var p = samplePhoto(id)
            p.filename = name
            p.url = path.map { URL(fileURLWithPath: $0) }
            return p
        }
        let photos = [photo(1, "A.ARW", "/s/A.ARW"), photo(2, "B.ARW", "/s/B.ARW"), photo(3, "B.ARW", "/s/x/B.ARW")]
        var r = ResumeState()
        r.focusedID = 2
        r.focusedPath = "/s/B.ARW"
        r.focusedName = "B.ARW"
        #expect(AppModel.resumedPhoto(r, in: photos) == PhotoID(2))
        r.focusedID = 1 // id 1 is another file now: the path wins
        #expect(AppModel.resumedPhoto(r, in: photos) == PhotoID(2))
        r.focusedPath = "/moved/B.ARW" // the path changed too: the name
        #expect(AppModel.resumedPhoto(r, in: photos) == PhotoID(2))
        r.focusedName = "C.ARW"
        #expect(AppModel.resumedPhoto(r, in: photos) == nil)
        r = ResumeState()
        r.focusedID = 3 // only an id (a synthetic photo): taken as is
        #expect(AppModel.resumedPhoto(r, in: photos) == PhotoID(3))
        #expect(AppModel.resumedPhoto(ResumeState(), in: photos) == nil)
    }

    @Test func theStoreIsReadOnce() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("lt-resume-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = ResumeStore(url: dir.appendingPathComponent("deep/resume.json"))
        #expect(store.consume() == nil)
        var r = ResumeState()
        r.focusedName = "X.ARW"
        try store.save(r)
        #expect(store.consume() == r)
        #expect(store.consume() == nil)
        // Something unreadable is dropped, never replayed.
        try Data("not json".utf8).write(to: store.url)
        #expect(store.consume() == nil)
        #expect(!FileManager.default.fileExists(atPath: store.url.path))
        #expect(ResumeStore().url.lastPathComponent == "resume.json")
    }
}
