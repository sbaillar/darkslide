import CoreGraphics
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// An in-memory session store that can finish a cull and report folder moves
/// (the app-side logic of H7 and H1 without files).
actor Cul9Store: SessionStore, SessionFileOrganizing, SessionMoveReporting {
    var photosByID: [PhotoID: Photo]
    var order: [PhotoID]
    var session: SessionInfo
    var canMove = true
    var moves: [(ids: [PhotoID], folder: SessionFolderKind)] = []
    var trashEmptied = 0
    /// Rejected variants hidden by `trashRejected` (as Rust sets their sidecars aside).
    var hiddenPhotos: [PhotoID: (photo: Photo, after: PhotoID?)] = [:]
    /// `emptyTrash` refuses (a volume without a Trash): nothing is deleted.
    var trashRefuses = false
    var nextID: Int64 = 1000
    var handler: (@Sendable (SessionMove) -> Void)?

    init(count: Int, name: String = "Cull") {
        let photos = (1...count).map { i -> Photo in
            var p = samplePhoto(Int64(i))
            p.meta.captureDate = Date(timeIntervalSince1970: 1_790_000_000 + Double(i) * 10)
            return p
        }
        photosByID = Dictionary(uniqueKeysWithValues: photos.map { ($0.id, $0) })
        order = photos.map(\.id)
        session = SessionInfo(name: name, url: URL(fileURLWithPath: "/tmp/\(name)"), photoCount: count, createdAt: Date())
    }

    func listSessions() async throws -> [SessionInfo] { [session] }
    func photos(in session: SessionInfo) async throws -> [Photo] { order.compactMap { photosByID[$0] } }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { session }
    func save(photos updated: [Photo]) async throws {
        for p in updated {
            var q = p
            q.inTrash = photosByID[p.id]?.inTrash ?? false
            q.inSelects = photosByID[p.id]?.inSelects ?? false
            photosByID[p.id] = q
        }
    }
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}

    func canMoveOriginals(in session: SessionInfo) async -> Bool { canMove }
    static func folder(of p: Photo) -> SessionFolderKind { p.inTrash ? .trash : p.inSelects ? .selects : .capture }
    static func file(_ p: Photo) -> String { p.url?.path ?? p.filename }
    func put(_ ids: [PhotoID], in folder: SessionFolderKind) {
        for id in ids {
            photosByID[id]?.inTrash = folder == .trash
            photosByID[id]?.inSelects = folder == .selects
        }
    }
    func move(_ ids: [PhotoID], to folder: SessionFolderKind) async throws -> [FolderMove] {
        guard canMove else { throw MockError.invalid("in place") }
        moves.append((ids, folder))
        let done = ids.compactMap { photosByID[$0] }.map {
            FolderMove(id: $0.id, from: "\(Self.folder(of: $0).rawValue)/\($0.filename)", to: "\(folder.rawValue)/\($0.filename)")
        }
        put(ids, in: folder)
        return done
    }
    /// Rust's rule: a file goes to Trash only when every variant (same file) is rejected.
    func trashRejected(in session: SessionInfo) async throws -> RejectsTrashed {
        guard canMove else { throw MockError.invalid("in place") }
        var done = RejectsTrashed()
        let files = Dictionary(grouping: order.compactMap { photosByID[$0] }.filter { !$0.inTrash }, by: Self.file)
        for vs in files.values.sorted(by: { $0[0].id < $1[0].id }) {
            let rejected = vs.filter { $0.pick == .rejected }
            if rejected.count == vs.count {
                done.moves.append(FolderMove(id: vs[0].id, from: "\(Self.folder(of: vs[0]).rawValue)/\(vs[0].filename)",
                                             to: "trash/\(vs[0].filename)"))
                put(vs.map(\.id), in: .trash)
            } else {
                for p in rejected {
                    let i = order.firstIndex(of: p.id) ?? 0
                    hiddenPhotos[p.id] = (p, i > 0 ? order[i - 1] : nil)
                    photosByID[p.id] = nil
                    order.removeAll { $0 == p.id }
                    done.hidden.append(p.id)
                }
            }
        }
        moves.append((done.moves.map(\.id), .trash))
        return done
    }
    func moveBack(_ back: [FolderMove], hidden: [PhotoID], in session: SessionInfo) async throws {
        for m in back {
            let folder = SessionFolderKind(rawValue: String(m.from.prefix { $0 != "/" })) ?? .capture
            guard let p = photosByID[m.id] else { continue }
            if moves.last?.folder != folder { moves.append(([], folder)) }
            moves[moves.count - 1].ids.append(m.id)
            put(photosByID.values.filter { Self.file($0) == Self.file(p) }.map(\.id), in: folder)
        }
        for id in hidden {
            guard let h = hiddenPhotos.removeValue(forKey: id) else { continue }
            photosByID[id] = h.photo
            order.insert(id, at: h.after.flatMap { order.firstIndex(of: $0) }.map { $0 + 1 } ?? 0)
        }
    }
    func restoreSetAsideVariants(in session: SessionInfo) async throws -> Int {
        let ids = hiddenPhotos.keys.sorted { $0.raw < $1.raw }
        try await moveBack([], hidden: ids, in: session)
        return ids.count
    }
    func setTrashRefuses(_ on: Bool) { trashRefuses = on }
    func emptyTrash(in session: SessionInfo) async throws -> Int {
        if trashRefuses { throw MockError.invalid("The macOS Trash refused the files, so nothing was deleted") }
        let trashed = photosByID.values.filter(\.inTrash)
        let files = Set(trashed.map(Self.file)).count // Rust counts originals
        for p in trashed { photosByID[p.id] = nil }
        order.removeAll { photosByID[$0] == nil }
        trashEmptied += files
        return files
    }
    func newVariant(of id: PhotoID) async throws -> PhotoID {
        guard var p = photosByID[id] else { throw MockError.notFound("x") }
        nextID += 1
        p = Photo(id: PhotoID(nextID), filename: p.filename, meta: p.meta, url: p.url)
        p.variantName = "Variant 2"
        photosByID[p.id] = p
        order.insert(p.id, at: (order.firstIndex(of: id) ?? 0) + 1)
        return p.id
    }
    func onSessionMoved(_ handler: @escaping @Sendable (SessionMove) -> Void) { self.handler = handler }
    func setCanMove(_ on: Bool) { canMove = on }
    func rename(to name: String) {
        let from = session.url
        session = SessionInfo(name: name, url: URL(fileURLWithPath: "/tmp/\(name)"), photoCount: order.count, createdAt: Date())
        handler?(SessionMove(from: from, to: session))
    }
    func movesSnapshot() -> [SessionFolderKind] { moves.map(\.folder) }
}

final class Cul9Box<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ v: T) { value = v }
    func withLock<R>(_ f: (inout T) -> R) -> R { lock.withLock { f(&value) } }
}

// MARK: - H6: arrow keys return to culling

@MainActor @Suite struct Cul9FocusTests {
    @Test func aDragHandsTheArrowsBackAClickKeepsThem() {
        #expect(!ControlFocus.keepsFocus(afterPointerMovedBy: 40), "a drag resigns")
        #expect(ControlFocus.keepsFocus(afterPointerMovedBy: 0), "a click keeps focus to nudge")
        #expect(ControlFocus.keepsFocus(afterPointerMovedBy: ControlFocus.dragThreshold - 0.1))
        #expect(!ControlFocus.keepsFocus(afterPointerMovedBy: ControlFocus.dragThreshold))
    }

    @Test func resignAllOnlyWhenSomethingHasFocus() {
        let c = ControlFocus()
        c.resignAll()
        #expect(c.resignGeneration == 0)
        c.focusChanged(true)
        c.resignAll()
        #expect(c.resignGeneration == 1)
    }

    /// Key routing: while a slider has focus, → adjusts it (the model doesn't move);
    /// once the drag ended (the slider resigned), → moves to the next photo again.
    @Test func arrowsRouteToCullingAfterTheSliderResigns() async {
        let m = makeModel(counts: [5])
        await m.start()
        let first = m.focusedID
        let c = ControlFocus()
        c.focusChanged(true) // the drag began: the slider took focus
        let right = KeyInput(special: .right)
        let focused = KeyMonitorPolicy.effectiveFocus(.none, controls: c)
        #expect(focused == .control)
        #expect(!m.handleKey(right, focus: focused, sheetOpen: false, isMainWindow: true))
        #expect(m.focusedID == first, "the slider keeps its arrows while focused")
        // Mouse-up after a drag: the slider resigns (`keepsFocus` false → focused = false).
        if !ControlFocus.keepsFocus(afterPointerMovedBy: 60) { c.focusChanged(false) }
        let after = KeyMonitorPolicy.effectiveFocus(.none, controls: c)
        #expect(after == .none)
        #expect(m.handleKey(right, focus: after, sheetOpen: false, isMainWindow: true))
        #expect(m.focusedID != first, "→ culls again")
        #expect(KeyMonitorPolicy.effectiveFocus(.text, controls: c) == .text)
    }
}

// MARK: - Survey

@MainActor @Suite struct Cul9SurveyTests {
    let a = PhotoID(1), b = PhotoID(2), c = PhotoID(3), d = PhotoID(4)

    @Test func eliminateRestoreMoveAndFinish() {
        let s = SurveyModel()
        #expect(!s.begin([a]), "one photo is nothing to compare")
        #expect(s.begin([a, b, a, c, d]) && s.candidates == [a, b, c, d] && s.focused == a)
        s.move(by: -1)
        #expect(s.focused == d)
        s.move(by: 1)
        s.eliminate()
        #expect(s.candidates == [b, c, d] && s.focused == b && s.eliminated == [a])
        s.eliminate(d)
        s.eliminate(c)
        s.eliminate(b)
        #expect(s.candidates == [b], "the last one stays")
        s.restoreLast()
        #expect(s.candidates == [b, c] && s.focused == c)
        s.focus(PhotoID(99))
        #expect(s.focused == c)
        #expect(s.finish() == [b, c] && !s.isActive && s.eliminated.isEmpty)
        // N9: more than a page is paged, never cut short.
        #expect(s.begin(Array((1...20).map { PhotoID(Int64($0)) })) && s.candidates.count == 20
            && s.visible.count == SurveyModel.pageSize)
    }

    @Test func columnsAndKeys() {
        #expect([2, 3, 4, 5, 6, 9, 10, 12].map(SurveyModel.columns(for:)) == [2, 2, 2, 3, 3, 3, 4, 4])
        let s = SurveyModel()
        #expect(s.handle(KeyInput("x")) == nil, "inactive: not the survey's")
        s.begin([a, b, c])
        #expect(s.handle(KeyInput("s", command: true)) == nil, "⌘ shortcuts pass")
        #expect(s.handle(KeyInput("x")) == .handled && s.candidates == [b, c])
        #expect(s.handle(KeyInput("u")) == .handled && s.candidates.count == 3)
        #expect(s.handle(KeyInput(special: .right)) == .handled)
        #expect(s.handle(KeyInput(special: .left)) == .handled)
        #expect(s.handle(KeyInput("3")) == .mark(.rate(3)), "N9: rating keys rate the focused candidate")
        #expect(s.handle(KeyInput("q")) == .handled, "other keys are swallowed")
        #expect(s.handle(KeyInput(special: .returnKey)) == .keep)
        #expect(s.handle(KeyInput(special: .escape)) == .cancel)
    }

    /// The app: ⌥⌘S on a selection, X eliminates, ⏎ picks the survivors (one undo step).
    @Test func surveyTheSelectionAndKeepTheBest() async {
        let m = makeModel(counts: [6])
        await m.start()
        let ids = Array(m.visibleIDs.prefix(4))
        m.selection = Set(ids)
        #expect(CommandCatalog.isEnabled(.survey, model: m))
        #expect(CommandCatalog.title(.survey, model: m) == "Survey Selected")
        CommandCatalog.perform(.survey, model: m)
        #expect(m.survey.isActive && m.survey.candidates == ids)
        #expect(CommandCatalog.title(.survey, model: m) == "Leave Survey")
        let key = { (k: KeyInput) in m.handleKey(k, focus: .none, sheetOpen: false, isMainWindow: true) }
        #expect(key(KeyInput("x")) && key(KeyInput("x")))
        #expect(m.survey.candidates == Array(ids.suffix(2)) && m.focusedID == ids[2])
        #expect(key(KeyInput(special: .right)) && m.focusedID == ids[3])
        #expect(key(KeyInput(special: .returnKey)))
        #expect(!m.survey.isActive)
        #expect(ids.suffix(2).allSatisfy { m.photo($0)?.pick == .picked } && ids.prefix(2).allSatisfy { m.photo($0)?.pick == PickState.none })
        #expect(m.selection == Set(ids.suffix(2)))
        m.undo()
        #expect(ids.allSatisfy { m.photo($0)?.pick == PickState.none }, "one undo step")
        // Esc leaves without changes; ⌥⌘S again leaves too.
        m.selection = Set(ids)
        m.beginSurvey()
        #expect(key(KeyInput(special: .escape)) && !m.survey.isActive)
        m.beginSurvey()
        CommandCatalog.perform(.survey, model: m)
        #expect(!m.survey.isActive)
    }

    /// No selection: the focused photo's burst (frames under a second apart).
    @Test func burstIsTheDefaultAndNothingSaysSo() async {
        // Demo photos are a burst (under a second apart): its members are surveyed.
        let m = makeModel(counts: [4])
        await m.start()
        m.selection = []
        #expect(m.surveyCandidates.count >= 2)
        // 10 s apart: no burst, so a toast explains; a selection works.
        let store = Cul9Store(count: 3)
        let live = makeModel(store: store)
        await live.start()
        live.selection = []
        #expect(live.surveyCandidates.isEmpty)
        live.beginSurvey()
        #expect(!live.survey.isActive && live.toast?.message.contains("two or more") == true)
        live.selection = Set(live.visibleIDs)
        #expect(live.surveyCandidates.count == 3)
    }
}

// MARK: - H7: finishing a cull

@MainActor @Suite struct Cul9CullTests {
    func model(_ store: Cul9Store) async -> AppModel {
        let m = makeModel(store: store)
        await m.start()
        return m
    }

    @Test func rejectsToTrashUndoPicksToSelectsAndDelete() async {
        let store = Cul9Store(count: 6)
        let m = await model(store)
        #expect(m.cull.canMove)
        let ids = m.visibleIDs
        m.focus(ids[0], extend: false)
        m.setPick(.rejected)
        m.focus(ids[1], extend: false)
        m.setPick(.rejected)
        m.focus(ids[2], extend: false)
        m.setPick(.picked)
        m.focus(ids[3], extend: false)
        m.setRating(3)
        #expect(m.cull.rejectedIDs == [ids[0], ids[1]] && m.cull.selectIDs == [ids[2], ids[3]])
        #expect(CommandCatalog.isEnabled(.moveRejectedToTrash, model: m) && CommandCatalog.help(.moveRejectedToTrash, model: m) == nil)
        await m.cull.moveRejectedToTrash()
        #expect(!m.visibleIDs.contains(ids[0]) && !m.visibleIDs.contains(ids[1]), "hidden once in the session Trash")
        #expect(m.toast?.message == "Moved 2 rejected photos to the session Trash (⌘Z to undo)")
        #expect(m.cull.trashedIDs.count == 2 && CommandCatalog.isEnabled(.restoreSessionTrash, model: m))
        // ⌘Z right after: back to Capture.
        m.undo()
        #expect(await waitUntil { m.visibleIDs.contains(ids[0]) })
        #expect(await store.movesSnapshot() == [.trash, .capture])
        await m.cull.moveRejectedToTrash()
        await m.cull.restoreFromTrash()
        #expect(m.visibleIDs.contains(ids[1]) && m.cull.trashedIDs.isEmpty)
        await m.cull.movePickedToSelects()
        #expect(m.cull.selectIDs.isEmpty && m.photo(ids[2])?.inSelects == true)
        #expect(m.toast?.message == "Moved 2 photos to Selects (⌘Z to undo)")
        // An edit after the move: ⌘Z undoes the edit, not the move.
        m.focus(ids[4], extend: false)
        m.setRating(2)
        m.undo()
        #expect(m.photo(ids[4])?.rating == 0)
        // Delete Rejected… asks first, then empties into the (fake) Trash.
        CommandCatalog.perform(.deleteRejected, model: m)
        #expect(m.cull.pendingDelete == 2)
        #expect(CullFinisher.deleteMessage(2).contains("macOS Trash") && CullFinisher.deleteMessage(1).contains("goes"))
        await m.cull.confirmDeleteRejected()
        #expect(await store.trashEmptied == 2 && m.photos.count == 4 && m.cull.pendingDelete == nil)
        #expect(m.toast?.message == "Moved 2 rejected files to the Trash")
    }

    @Test func inPlaceFoldersNeverMoveFiles() async {
        let store = Cul9Store(count: 3)
        await store.setCanMove(false)
        let m = await model(store)
        m.focus(m.visibleIDs[0], extend: false)
        m.setPick(.rejected)
        #expect(!m.cull.canMove && !CommandCatalog.isEnabled(.moveRejectedToTrash, model: m))
        #expect(CommandCatalog.help(.deleteRejected, model: m) == CullFinisher.inPlaceReason)
        await m.cull.moveRejectedToTrash()
        #expect(m.toast?.message == CullFinisher.inPlaceReason && m.toast?.isError == true)
        m.cull.requestDeleteRejected()
        #expect(m.cull.pendingDelete == nil)
        #expect(await store.movesSnapshot().isEmpty)
    }

    @Test func newVariantIsListedAndFocused() async {
        let store = Cul9Store(count: 2)
        let m = await model(store)
        m.focus(m.visibleIDs[0], extend: false)
        #expect(CommandCatalog.isEnabled(.newVariant, model: m))
        #expect(MenuCommand.newVariant.shortcut == .init(key: "d", modifiers: [.command]))
        await m.cull.newVariant()
        #expect(m.photos.count == 3 && m.focusedPhoto?.variantName == "Variant 2")
        #expect(m.toast?.message.hasPrefix("New variant of") == true)
    }

    @Test func menuTitlesAndMockStoresWithoutFileMoves() async {
        let m = makeModel(counts: [3])
        await m.start()
        for c in CommandCatalog.cullMenu.compactMap({ $0 }) { #expect(!CommandCatalog.title(c, model: m).isEmpty) }
        #expect(!m.cull.canMove, "the demo store doesn't move files")
        #expect(!CommandCatalog.isEnabled(.newVariant, model: m))
        await m.cull.newVariant() // no organizing store: nothing happens
        #expect(m.photos.count == 3)
    }
}

// MARK: - H1/M3: a session folder renamed while open

@MainActor @Suite struct Cul9SessionMoveTests {
    @Test func recentFoldersFollowARenamedFolderAcrossRelaunch() throws {
        let parent = FileManager.default.temporaryDirectory.appendingPathComponent("cul9-rec-\(UUID())")
        let beach = parent.appendingPathComponent("2026-09-26 Beach")
        try FileManager.default.createDirectory(at: beach, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: parent) }
        let defaults = testDefaults()
        let r = RecentFolders(defaults: defaults)
        r.add(beach)
        r.setPhotoCount(7, for: beach)
        let renamed = parent.appendingPathComponent("Beach RENAMED")
        try FileManager.default.moveItem(at: beach, to: renamed)
        // Relaunch: a new instance over the same preferences.
        let again = RecentFolders(defaults: defaults)
        #expect(again.folders.map(\.lastPathComponent) == ["Beach RENAMED"])
        #expect(again.photoCount(renamed) == 7, "the count follows")
        // replace(): the store followed it while open.
        let other = parent.appendingPathComponent("Other")
        try FileManager.default.moveItem(at: renamed, to: other)
        again.replace(renamed, with: other)
        #expect(again.folders.map(\.lastPathComponent) == ["Other"])
        again.remove(other)
        #expect(again.folders.isEmpty)
        #expect(RecentFolders.follow(nil) == nil && RecentFolders.follow(Data([1, 2])) == nil)
    }

    @Test func theAppSaysSoAndListsTheSessionAgain() async throws {
        let store = Cul9Store(count: 3, name: "2026-09-26 Beach")
        let m = makeModel(store: store)
        m.toastSleep = { _ in try await Task.sleep(for: .seconds(3600)) } // toasts stay up: no timing race
        await m.start()
        let focused = m.visibleIDs[1]
        m.focus(focused, extend: false)
        // The develop of the old path failed (the folder was renamed under it).
        let r = try #require(m.renderer as? FakeRenderer)
        #expect(await waitUntil { m.viewer.frame != nil })
        r.failWith = CocoaError(.fileNoSuchFile)
        m.viewer.requestRender()
        // Said, never silent: the original isn't where the photo says (missing-originals UX).
        #expect(await waitUntil { m.viewer.problem == .missingOriginal(name: m.photo(focused)!.filename) })
        r.failWith = nil
        let before = r.calls.count
        await store.rename(to: "Beach RENAMED")
        #expect(await waitUntil { m.currentSession?.name == "Beach RENAMED" })
        #expect(m.toast?.message == "“2026-09-26 Beach” was renamed to “Beach RENAMED” — following it")
        // The reopen sets the session before it restores the focus: wait for the whole move.
        #expect(await waitUntil { m.focusedID == focused }, "focus kept")
        // Final review M3: the viewer develops it again from the new place; no stale error.
        #expect(await waitUntil { r.calls.count > before && m.viewer.renderError == nil && m.viewer.problem == nil })
        // A session that isn't open: the list refreshes, nothing reopens.
        await m.sessionMoved(SessionMove(from: URL(fileURLWithPath: "/tmp/elsewhere"),
                                          to: SessionInfo(name: "X", url: URL(fileURLWithPath: "/tmp/X"), photoCount: 0, createdAt: Date())))
        #expect(m.currentSession?.name == "Beach RENAMED")
    }

    /// Live (H1): an imported session renamed in Finder while open. The next rating
    /// follows it (no raw error, no ghost folder at the old path), the app is told,
    /// and a relaunch still lists it, with the rating from before and after.
    @Test func importedSessionRenamedWhileOpenIsFollowedLive() async throws {
        try await Imp6LiveImportTests.withEnv { env in
            var opened: SessionInfo?
            let m = Imp6LiveImportTests().importModel(env, service: env.service(), ejector: FakeEjector()) { opened = $0 }
            await m.scan()
            m.sessionText = "Beach"
            m.start()
            await m.wait()
            let session = try #require(opened)
            let store = env.store
            let moved = Cul9Box<[SessionMove]>([])
            await store.onSessionMoved { mv in moved.withLock { $0.append(mv) } }
            var photos = try await store.photos(in: session).filter { !$0.isVideo }
            photos[0].rating = 2
            try await store.save(photos: [photos[0]])
            let renamed = session.url.deletingLastPathComponent().appendingPathComponent("Beach RENAMED")
            try FileManager.default.moveItem(at: session.url, to: renamed)
            photos[1].rating = 5
            try await store.save(photos: [photos[1]]) // follows the renamed folder
            #expect(!FileManager.default.fileExists(atPath: session.url.path), "no ghost folder at the old path")
            let mv = try #require(moved.withLock { $0.first })
            #expect(mv.from.lastPathComponent == session.url.lastPathComponent && mv.to.name == "Beach RENAMED")
            let relaunched = RustSessionStore(recents: RecentFolders(defaults: env.recents.defaultsForTesting),
                                              storeBase: env.root.appendingPathComponent("stores"))
            let list = try await relaunched.listSessions()
            let again = try #require(list.first { $0.name == "Beach RENAMED" }, "\(list.map(\.name))")
            let ratings = try await relaunched.photos(in: again).map(\.rating)
            #expect(ratings.contains(2) && ratings.contains(5), "\(ratings)")
            print("CUL9 H1 live: imported session renamed while open, followed")
        }
    }

    /// Live (H7): an imported session through the app. Rejects go to the session's
    /// Trash/ folder (hidden, ⌘Z brings them back), picks to Selects, a new variant
    /// is listed. (Delete Rejected… is covered with the fake store and in Rust: here
    /// it would put test files in the user's real Trash.)
    @Test func finishingACullOnALiveImportedSession() async throws {
        try await Imp6LiveImportTests.withEnv { env in
            var opened: SessionInfo?
            let im = Imp6LiveImportTests().importModel(env, service: env.service(), ejector: FakeEjector()) { opened = $0 }
            await im.scan()
            im.sessionText = "Cull"
            im.start()
            await im.wait()
            let session = try #require(opened)
            let m = makeModel(store: env.store)
            await m.open(session)
            #expect(m.cull.canMove)
            let stills = m.visibleIDs.filter { m.photo($0)?.isVideo == false }
            #expect(stills.count >= 3)
            m.focus(stills[0], extend: false)
            m.setPick(.rejected)
            m.focus(stills[1], extend: false)
            m.setPick(.picked)
            await m.cull.moveRejectedToTrash()
            #expect(!m.visibleIDs.contains(stills[0]) && m.cull.trashedIDs == [stills[0]])
            let trashed = try #require(m.photo(stills[0])?.url)
            #expect(trashed.path.contains("/Trash/") && FileManager.default.fileExists(atPath: trashed.path))
            m.undo()
            #expect(await waitUntil { m.visibleIDs.contains(stills[0]) })
            #expect(m.photo(stills[0])?.url?.path.contains("/Capture/") == true && m.photo(stills[0])?.pick == .rejected)
            await m.cull.movePickedToSelects()
            #expect(m.photo(stills[1])?.inSelects == true && m.photo(stills[1])?.url?.path.contains("/Selects/") == true)
            m.focus(stills[2], extend: false)
            await m.cull.newVariant()
            #expect(m.focusedPhoto?.variantName.isEmpty == false && m.focusedPhoto?.filename == m.photo(stills[2])?.filename)
            print("CUL9 H7 live: trash, undo, selects, variant")
        }
    }

    /// A folder opened in place can't move its files.
    @Test func inPlaceSessionsRefuseFileMovesLive() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()),
                                     storeBase: dir.appendingPathComponent("stores"))
        let info = try await store.add(folder: dir)
        #expect(await !store.canMoveOriginals(in: info))
        let id = try #require(try await store.photos(in: info).first?.id)
        await #expect(throws: (any Error).self) { _ = try await store.move([id], to: .trash) }
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent(sampleARWs()[0].lastPathComponent).path))
    }

    /// Live: an in-place folder renamed while open (M3): the next write follows it,
    /// the store reports the move, and a relaunch still lists it with its ratings.
    @Test func inPlaceFolderRenamedWhileOpenIsFollowedLive() async throws {
        guard let dir = try sampleFolderCopy(limit: 2) else { return }
        let parent = dir.deletingLastPathComponent().appendingPathComponent("cul9-mv-\(UUID())")
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        let folder = parent.appendingPathComponent("Hike")
        try FileManager.default.moveItem(at: dir, to: folder)
        defer { try? FileManager.default.removeItem(at: parent) }
        let defaults = testDefaults()
        let base = parent.appendingPathComponent("stores")
        let store = RustSessionStore(recents: RecentFolders(defaults: defaults), storeBase: base)
        let moved = Cul9Box<[SessionMove]>([])
        await store.onSessionMoved { m in moved.withLock { $0.append(m) } }
        let info = try await store.add(folder: folder)
        var photos = try await store.photos(in: info)
        photos[0].rating = 3
        try await store.save(photos: [photos[0]])
        let renamed = parent.appendingPathComponent("Hike 2026")
        try FileManager.default.moveItem(at: folder, to: renamed)
        photos[1].rating = 5
        try await store.save(photos: [photos[1]]) // follows the folder, no error
        let m = try #require(moved.withLock { $0.first })
        #expect(m.from.lastPathComponent == "Hike" && m.to.url.lastPathComponent == "Hike 2026")
        let relaunched = RustSessionStore(recents: RecentFolders(defaults: defaults), storeBase: base)
        let list = try await relaunched.listSessions()
        #expect(list.map(\.name) == ["Hike 2026"])
        let again = try await relaunched.photos(in: try #require(list.first))
        #expect(Set(again.map(\.rating)) == [3, 5], "both ratings, before and after the rename")
        #expect(!FileManager.default.fileExists(atPath: folder.path), "no ghost folder")
    }
}

// MARK: - M2, L6, L7: wording, stars, histogram, the 1:1 tile

@MainActor @Suite struct Cul9MiscTests {
    @Test func openFailuresAreNamed() {
        let urls = ["EMPTY.ARW", "B.ARW", "C.ARW", "D.ARW", "E.ARW"].map { URL(fileURLWithPath: "/x/\($0)") }
        #expect(OpenFailures.message(Array(urls.prefix(1))).hasPrefix("Couldn’t read EMPTY.ARW:"))
        #expect(OpenFailures.message(urls).contains("EMPTY.ARW, B.ARW, C.ARW and 2 more"))
        #expect(OpenFailures.message(urls).hasSuffix("Copy them from the card again."))
        #expect(OpenFailures.message([]).isEmpty)
        let damaged = RenderError(.Io(message: OpenFailures.damagedMessage))
        #expect(OpenFailures.isDamaged(damaged) && !OpenFailures.isDamaged(MockError.invalid("x")))
        #expect(damaged.message == OpenFailures.damagedMessage, "lt-ffi's wording, shown as is")
    }

    @Test func ratingWording() {
        #expect(RatingText.label(1) == "1 star" && RatingText.label(3) == "3 stars" && RatingText.label(0) == "Unrated")
        #expect(RatingText.rating(afterClicking: 4, current: 2) == 4)
        #expect(RatingText.rating(afterClicking: 4, current: 4) == 0, "the same star clears")
        #expect(RatingText.rating(afterClicking: 9, current: 0) == 5)
    }

    @Test func gridStarsRateOnePhoto() async {
        let m = makeModel(counts: [3])
        await m.start()
        let id = m.visibleIDs[2]
        let focused = m.focusedID
        m.rate(id, 4)
        #expect(m.photo(id)?.rating == 4 && m.focusedID == focused)
        m.rate(id, 9)
        #expect(m.photo(id)?.rating == 5)
        m.undo()
        #expect(m.photo(id)?.rating == 4)
    }

    @Test func histogramHasASpokenValue() {
        #expect(HistogramText.value(nil) == "No image")
        #expect(HistogramText.value(Histogram()) == "No image")
        let dark = Histogram(rgba: Data([UInt8](repeating: 10, count: 400).enumerated().map { $0.offset % 4 == 3 ? 255 : $0.element }),
                             pixelCount: 100)
        #expect(HistogramText.value(dark).hasPrefix("Mostly dark"), "\(HistogramText.value(dark))")
        let bright = Histogram(rgba: Data([UInt8](repeating: 255, count: 400)), pixelCount: 100)
        #expect(HistogramText.value(bright).contains("100.0% clipped highlights"), "\(HistogramText.value(bright))")
    }

    /// A develop that fails because the RAW is damaged badges the photo.
    @Test func aDamagedRAWIsBadged() async {
        let r = FakeRenderer()
        r.failWith = RenderError(.Io(message: OpenFailures.damagedMessage))
        let m = makeModel(renderer: r)
        await m.start()
        #expect(await waitUntil { m.focusedPhoto?.isDamaged == true })
        #expect(m.viewer.renderError == OpenFailures.damagedMessage)
    }

    /// L7: before/after keeps the 1:1 tile, on the "after" side of the split.
    @Test func beforeAfterKeepsTheTile() {
        let view = CGSize(width: 1000, height: 600)
        let layout = ViewerLayout(panes: [.init(role: .main, frame: CGRect(origin: .zero, size: view),
                                                image: CGRect(x: 50, y: 0, width: 900, height: 600), splitX: 500)])
        let frame = { (w: Int) in RenderedFrame(width: w, height: w * 2 / 3, rgba: Data(count: w * (w * 2 / 3) * 4)) }
        var input = ViewerDrawPlan.Input(layout: layout, viewSize: view, backingScale: 2, main: frame(1000),
                                         tile: (frame(1200), CGRect(x: 0.4, y: 0.4, width: 0.2, height: 0.2)),
                                         before: frame(1000))
        input.imageSize = CGSize(width: 6000, height: 4000)
        let plan = ViewerDrawPlan.make(input)
        #expect(plan.quads.count == 2, "main (with before) + the tile")
        let main = plan.quads[0], tile = plan.quads[1]
        #expect(main.before != nil && tile.before == nil)
        #expect(tile.clip.x >= main.split, "the tile stays right of the split")
        input.before = nil
        let noSplit = ViewerDrawPlan.make(input)
        #expect(noSplit.quads[1].clip.x == noSplit.quads[0].clip.x)
    }
}
