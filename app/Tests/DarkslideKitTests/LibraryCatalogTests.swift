import CoreGraphics
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// [library] The catalog view offered: Open in Library at launch (no session loaded),
/// File ▸ Browse All Sessions in Library, the Opened Sessions and ⌘-click multi-session
/// scopes, and the laziness that keeps a big Library fast.
@MainActor
@Suite(.serialized) struct LibraryCatalogTests {
    static func record(_ id: Int64, _ root: String, _ name: String, _ count: UInt64) -> LibrarySessionRecord {
        LibrarySessionRecord(id: id, root: root, name: name, count: count, available: true)
    }

    /// Island (id 1, open, two photos), Home (id 2, ten photos) and Elsewhere (id 3, five
    /// photos, never opened). The recents are Island then Home (GeoStore's list); the index
    /// spells Island's folder through `/private` and Home's with a trailing slash.
    func backend() -> FakeLibraryBackend {
        let b = FakeLibraryBackend()
        b.state.withLock { s in
            s.items = (101...110).map { libraryItem(Int64($0), rating: UInt8($0 % 6)) }
                + [libraryItem(1002, session: 1, root: GeoStore.session.url.path, variant: 2, file: "DSC02.ARW"),
                   libraryItem(1003, session: 1, root: GeoStore.session.url.path, variant: 3, file: "DSC03.ARW")]
                + (501...505).map { libraryItem(Int64($0), session: 3, root: "/tmp/elsewhere") }
            s.sessions = [Self.record(1, "/private/tmp/island", "Island", 2), Self.record(2, "/tmp/home/", "Home", 10),
                          Self.record(3, "/tmp/elsewhere", "Elsewhere", 5)]
        }
        return b
    }

    func model(_ b: FakeLibraryBackend) async -> AppModel {
        let m = makeModel(store: GeoStore())
        m.library.configure(backend: b)
        await m.start()
        let ready = await waitUntil { m.photoCount == 8 && m.currentSession == GeoStore.session }
        #expect(ready)
        return m
    }

    // MARK: - (a) Opened Sessions

    @Test func recentsResolveToTheIndexSessionsAndUnknownOnesAreSkipped() {
        let sessions = [Self.record(5, "/private/tmp/island", "Island", 2), Self.record(9, "/tmp/home/", "Home", 1),
                        Self.record(7, "/elsewhere", "Elsewhere", 1)]
        let ids = LibraryModel.resolve(recents: ["/tmp/island", "/Volumes/Gone/Shoot", "/private/tmp/home",
                                                 "/tmp/./island/../island"], in: sessions)
        #expect(ids == [5, 9], "in the recents' order, each once; a recent the index doesn't know is left out")
        #expect(LibraryModel.resolve(recents: [], in: sessions).isEmpty)
        #expect(LibraryModel.resolve(recents: ["/tmp/island"], in: []).isEmpty)
        #expect(LibraryModel.folderKey("/private/var/x/../y/") == "/var/y")
        #expect(LibraryModel.folderKey("/private/tmp") == "/tmp")
        #expect(LibraryModel.folderKey("/privateer/tmp") == "/privateer/tmp")
        #expect(LibraryModel.folderKey("/private") == "/private")
        #expect(LibraryModel.folderKey("/") == "/")
    }

    @Test func theOpenedSessionsScopeShowsTheRecentsAsOneSession() async {
        let b = backend()
        let m = await model(b)
        let lib = m.library
        lib.scope = .opened
        await lib.settled()
        #expect(lib.openedSessionIDs == [1, 2])
        #expect(lib.query.sessions == [1, 2])
        #expect(b.state.get.queries.last?.sessions == [1, 2])
        #expect(Set(lib.ids) == Set((101...110).map(Int64.init) + [1002, 1003]), "Elsewhere was never opened")
        #expect(lib.title == "Opened Sessions" && lib.openedCount == 12)

        // Filters still apply on top.
        lib.filter.minRating = 4
        await lib.settled()
        #expect(lib.ids == [101, 106, 107] && b.state.get.queries.last?.minRating == 4)
        lib.filter = LibraryFilter()
        await lib.settled()

        // Rating writes through to each photo's own session: the open one through the app.
        lib.selection = [101, 1002]
        lib.rate(5)
        await lib.editsSettled()
        #expect(b.state.get.edits.last?.0 == [101], "Home's photo through its session")
        #expect(m.photo(PhotoID(2))?.rating == 5, "Island's photo through the open session")

        // Opening a photo shows it in its session.
        m.browserMode = .library
        await lib.open(1003)
        #expect(m.browserMode == .filmstrip && m.focusedID == PhotoID(3))
    }

    @Test func noRegisteredRecentMeansNoPhotosNotEveryPhoto() async {
        let b = backend()
        b.state.withLock { $0.sessions = [Self.record(3, "/tmp/elsewhere", "Elsewhere", 5)] }
        let m = await model(b)
        let lib = m.library
        await lib.reload()
        let queries = b.state.get.queries.count
        lib.scope = .opened
        await lib.settled()
        #expect(lib.count == 0 && lib.openedSessionIDs.isEmpty && lib.openedCount == 0)
        #expect(b.state.get.queries.count == queries, "an empty session list would mean every session: never asked")
        lib.scope = .sessions([])
        await lib.settled()
        #expect(lib.count == 0)
        // Without an app (or before the overview), nothing resolves.
        let alone = LibraryModel()
        #expect(alone.openedSessionIDs.isEmpty)
        alone.scope = .opened
        #expect(alone.title == "Opened Sessions")
    }

    // MARK: - (b) ⌘-click: several sessions as one

    @Test func aMultiSessionScopeQueriesOnlyThoseSessions() async {
        let b = backend()
        let m = await model(b)
        let lib = m.library
        lib.scope = .sessions([1, 3])
        await lib.settled()
        #expect(b.state.get.queries.last?.sessions == [1, 3])
        #expect(Set(lib.ids) == [1002, 1003, 501, 502, 503, 504, 505])
        #expect(lib.title == "Island + Elsewhere")
        lib.scope = .sessions([1, 2, 3])
        #expect(lib.title == "3 Sessions")
        lib.filter.sort = .session
        lib.filter.descending = true
        await lib.settled()
        let q = b.state.get.queries.last
        #expect(q?.sessions == [1, 2, 3] && q?.sort == .session && q?.descending == true)
        // A smart collection made here keeps the sessions.
        await lib.newSmartCollection(named: "Three")
        #expect(b.state.get.collections.last?.rules?.sessions == [1, 2, 3])
    }

    @Test func theSidebarSelectionMakesTheScope() {
        typealias S = LibraryModel.Scope
        #expect(LibraryModel.sidebarSelection(.sessions([1, 2])) == [S.session(1), .session(2)])
        #expect(LibraryModel.sidebarSelection(.opened) == [S.opened])
        // ⌘-click a second session: both, as one.
        #expect(LibraryModel.scope(sidebar: [.session(2), .session(1)], current: .session(2)) == .sessions([1, 2]))
        #expect(LibraryModel.scope(sidebar: [.session(1), .session(2), .session(4)], current: .sessions([1, 2]))
            == .sessions([1, 2, 4]))
        // ⌘-click one off again: the one left.
        #expect(LibraryModel.scope(sidebar: [.session(1)], current: .sessions([1, 2])) == .session(1))
        // ⌘-click a session next to All: that session.
        #expect(LibraryModel.scope(sidebar: [.all, .session(3)], current: .all) == .session(3))
        // A date or Opened Sessions added next to sessions replaces them.
        #expect(LibraryModel.scope(sidebar: [.session(1), .session(2), .year(2024)], current: .sessions([1, 2]))
            == .year(2024))
        #expect(LibraryModel.scope(sidebar: [.session(1), .opened], current: .session(1)) == .opened)
        // A plain click.
        #expect(LibraryModel.scope(sidebar: [.collection(4)], current: .session(1)) == .collection(4))
        #expect(LibraryModel.scope(sidebar: [], current: .all) == nil)
    }

    @Test func switchingScopesNeverSyncsInTheForeground() async {
        let b = backend()
        let m = await model(b)
        let lib = m.library
        lib.appeared() // the one background sync and refresh
        await lib.settled()
        let (synced, refreshes) = (b.state.get.synced.count, b.state.get.refreshes)
        for scope in [LibraryModel.Scope.opened, .sessions([1, 2, 3]), .session(2), .all, .opened] {
            lib.scope = scope
            await lib.settled()
        }
        #expect(b.state.get.synced.count == synced && b.state.get.refreshes == refreshes)
        #expect(b.state.get.pages.isEmpty, "each query's first page came with it")
    }

    // MARK: - (c) Lazy over thousands of photos

    @Test func onlyTheFirstPageAndNoThumbnailsUntilCellsAppear() async {
        let b = FakeLibraryBackend()
        b.state.withLock { $0.items = (1...5_000).map { libraryItem(Int64($0), session: Int64(1 + $0 % 7)) } }
        let lib = LibraryModel()
        lib.configure(backend: b)
        lib.appeared()
        await lib.settled()
        #expect(lib.count == 5_000)
        #expect(lib.item(at: LibraryModel.pageSize - 1) != nil && lib.item(at: LibraryModel.pageSize) == nil,
                "the first page's details came with the ids")
        #expect(b.state.get.pages.isEmpty, "no other page asked for")
        #expect(b.state.get.thumbs.isEmpty && lib.thumbnails.loads == 0, "no thumbnail before a cell shows")
        #expect(b.state.get.synced.isEmpty && b.state.get.refreshes == 1, "no session open: a background refresh only")
        #expect(lib.loadedDetails == LibraryModel.pageSize)

        // The first screen (4 rows of 6) shows: only the next two rows are prefetched.
        lib.columns = 6
        for i in 0..<24 { lib.cellAppeared(i, bucket: 256) }
        await lib.thumbnails.prefetched()
        await lib.settled()
        let ahead = Set(lib.ids[0..<(24 + 2 * 6)])
        #expect(!b.state.get.thumbs.isEmpty && b.state.get.thumbs.allSatisfy(ahead.contains))
        #expect(b.state.get.pages.isEmpty, "the first page is already here")

        // A jump far down asks for that page only.
        lib.cellAppeared(4_000, bucket: 256)
        await lib.settled()
        #expect(b.state.get.pages == [Array(lib.ids[3_960..<4_080])])
        #expect(lib.item(at: 4_000) != nil && lib.item(at: 2_000) == nil)
    }

    @Test func detailsStayBoundedWhileScrollingAndComeBackWhenShown() async {
        let b = FakeLibraryBackend()
        b.state.withLock { $0.items = (1...3_000).map { libraryItem(Int64($0)) } }
        let lib = LibraryModel()
        lib.configure(backend: b)
        lib.maxLoadedPages = 3
        await lib.reload()
        lib.selection = [5] // on page 0: an edit needs its details, so they stay
        for page in 0..<(3_000 / LibraryModel.pageSize) {
            lib.cellAppeared(page * LibraryModel.pageSize, bucket: 256)
            await lib.settled()
        }
        #expect(lib.loadedDetails <= 3 * LibraryModel.pageSize + 1)
        #expect(lib.item(at: 2_999) != nil && lib.item(at: 10) == nil && lib.item(5) != nil)
        let asked = b.state.get.pages.count
        lib.cellAppeared(10, bucket: 256) // scrolled back: page 0 loads again
        await lib.settled()
        #expect(b.state.get.pages.count == asked + 1 && lib.item(at: 10) != nil)
    }

    @Test func aThumbnailForACellThatScrolledAwayIsDropped() async {
        let b = FakeLibraryBackend()
        b.state.withLock { $0.thumbDelay = .seconds(30) }
        let t = LibraryThumbnails()
        t.backend = b
        let item = libraryItem(1)
        let cell = Task { await t.image(for: item, bucket: 256) }
        let started = await waitUntil { b.state.get.thumbs == [1] }
        #expect(started)
        cell.cancel() // the cell left the screen
        #expect(await cell.value == nil)
        #expect(t.cached(item, bucket: 256) == nil)
        // A newer prefetch replaces the older one, which stops.
        t.prefetch([libraryItem(2), libraryItem(3)], bucket: 256)
        t.prefetch([libraryItem(4)], bucket: 256)
        b.state.withLock { $0.thumbDelay = .zero }
        await t.prefetched()
        #expect(t.cached(libraryItem(4), bucket: 256) != nil && t.cached(libraryItem(3), bucket: 256) == nil)
    }

    // MARK: - (d) Open in Library

    @Test func openInLibraryStartsWithoutLoadingASession() async {
        let b = backend()
        let m = makeModel(store: GeoStore())
        m.settings.startView = .library
        #expect(AppSettings(defaults: m.settings.userDefaults).startView == .library, "persisted")
        m.library.configure(backend: b)
        await m.start()
        #expect(m.browserMode == .library)
        #expect(m.currentSession == nil && m.photoCount == 0 && !m.isLoading, "no session's photos loaded")
        #expect(m.deferredSession == GeoStore.session)
        m.library.appeared()
        await m.library.settled()
        #expect(m.library.count == 17)
        #expect(b.state.get.synced.isEmpty && b.state.get.refreshes == 1, "nothing to sync but in the background")
        #expect(m.library.openedSessionIDs == [1, 2], "the recents are known without opening one")
        // Leaving the Library opens the session it would have opened.
        m.browserMode = .grid
        let opened = await waitUntil { m.currentSession == GeoStore.session && m.photoCount == 8 }
        #expect(opened && m.deferredSession == nil)
    }

    @Test func aSessionOpenedFromTheLibraryReplacesTheDeferredOne() async {
        let m = makeModel(store: GeoStore())
        m.settings.startView = .library
        await m.start()
        await m.open(GeoStore.other)
        #expect(m.deferredSession == nil && m.currentSession == GeoStore.other)
        m.browserMode = .filmstrip
        #expect(m.currentSession == GeoStore.other)
        #expect(StartView.allCases.map(\.title) == ["Session", "Library (catalog)"])
        #expect(AppSettings(defaults: makeSettings().userDefaults).startView == .session, "the default keeps today's start")
    }

    @Test func anUpdateRestartKeepsWhereTheUserWas() async {
        // In the Library with no session open: the next launch is the Library again.
        let a = makeModel(store: GeoStore())
        a.browserMode = .library
        let r = a.resumeState(updatedTo: "9.9.9")
        #expect(r.sessionID == nil && r.browserMode == "library")
        let b = makeModel(store: GeoStore())
        b.pendingResume = r
        await b.start()
        #expect(b.browserMode == .library && b.currentSession == nil)

        // A session that was open wins over Open in Library: it reopens where the user was.
        let c = makeModel(store: GeoStore())
        c.settings.startView = .library
        var withSession = ResumeState()
        withSession.sessionID = GeoStore.other.id
        withSession.sessionPath = GeoStore.other.url.path
        withSession.browserMode = BrowserMode.grid.rawValue
        c.pendingResume = withSession
        await c.start()
        #expect(c.currentSession == GeoStore.other && c.browserMode == .grid)
        #expect(c.deferredSession == nil)
    }

    @Test func theCatalogIsOfferedInTheFileMenu() async {
        let m = makeModel()
        #expect(CommandCatalog.title(.showLibrary, model: m) == "Browse All Sessions in Library")
        #expect(MenuCommand.showLibrary.shortcut == .init(key: "l", modifiers: [.command, .shift]))
        #expect(CommandCatalog.isEnabled(.showLibrary, model: m))
        #expect(CommandCatalog.help(.showLibrary, model: m)?.contains("⇧⌘L") == true)
        #expect(CommandCatalog.perform(.showLibrary, model: m) == .none)
        #expect(m.browserMode == .library)
    }
}
