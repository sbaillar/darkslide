import CoreGraphics
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideKit

/// In-memory `LibraryBackend`: a list of items it filters by session, rating and
/// collection; records what it was asked.
final class FakeLibraryBackend: LibraryBackend, @unchecked Sendable {
    struct State {
        var items: [LibraryItemRecord] = []
        var queries: [LibraryQueryRecord] = []
        var pages: [[Int64]] = []
        var synced: [SessionInfo] = []
        var refreshes = 0
        var refreshChanges = false
        var edits: [([Int64], LibraryEditRecord)] = []
        var editFailures: [String] = []
        var collections: [LibraryCollectionRecord] = []
        var members: [Int64: Set<Int64>] = [:]
        var thumbs: [Int64] = []
        var fail: Error?
    }

    let state = SearchLocked(State())

    func sync(_ session: SessionInfo) async throws { state.withLock { $0.synced.append(session) } }

    func refresh() async throws -> Bool {
        state.withLock { s in
            s.refreshes += 1
            return s.refreshChanges
        }
    }

    func query(_ q: LibraryQueryRecord, first: Int) async throws -> LibraryPage {
        try state.withLock { s in
            if let e = s.fail { throw e }
            s.queries.append(q)
            let kept = s.items.filter { i in
                (q.sessions.isEmpty || q.sessions.contains(i.sessionId)) && i.rating >= q.minRating
                    && (q.collection.map { s.members[$0]?.contains(i.id) ?? false } ?? true)
            }
            return LibraryPage(ids: kept.map(\.id), first: Array(kept.prefix(first)))
        }
    }

    func items(_ ids: [Int64]) async throws -> [LibraryItemRecord] {
        state.withLock { s in
            s.pages.append(ids)
            return ids.compactMap { id in s.items.first { $0.id == id } }
        }
    }

    func overview() async throws -> LibraryOverviewRecord {
        state.withLock { s in
            let collections = s.collections.map { c in
                var c = c
                c.count = UInt64(s.members[c.id]?.count ?? 0)
                return c
            }
            return LibraryOverviewRecord(total: UInt64(s.items.count), sessions: [], days: [], undated: 0, cameras: [],
                                         keywords: [], collections: collections)
        }
    }

    func edit(_ ids: [Int64], _ change: LibraryEditRecord) async throws -> LibraryEditReport {
        state.withLock { s in
            s.edits.append((ids, change))
            for (n, i) in s.items.enumerated() where ids.contains(i.id) {
                switch change {
                case .rating(let stars): s.items[n].rating = stars
                case .label(let l): s.items[n].label = l
                case .flag(let p): s.items[n].pick = p
                }
            }
            return LibraryEditReport(changed: ids, failed: s.editFailures)
        }
    }

    func createCollection(name: String, rules: LibraryQueryRecord?, ids: [Int64]) async throws -> Int64 {
        try state.withLock { s in
            if let e = s.fail { throw e }
            let id = Int64(s.collections.count + 1)
            s.collections.append(LibraryCollectionRecord(id: id, name: name, rules: rules, count: 0))
            s.members[id] = Set(ids)
            return id
        }
    }

    func updateCollection(_ id: Int64, name: String, rules: LibraryQueryRecord?) async throws -> Bool {
        state.withLock { s in
            guard let n = s.collections.firstIndex(where: { $0.id == id }) else { return false }
            s.collections[n].name = name
            return true
        }
    }

    func deleteCollection(_ id: Int64) async throws -> Bool {
        state.withLock { s in
            s.collections.removeAll { $0.id == id }
            return s.members.removeValue(forKey: id) != nil
        }
    }

    func members(of collection: Int64, _ ids: [Int64], remove: Bool) async throws -> Int {
        state.withLock { s in
            if remove { s.members[collection]?.subtract(ids) } else { s.members[collection, default: []].formUnion(ids) }
            return ids.count
        }
    }

    func thumbnail(for item: LibraryItemRecord, maxPixelSize: Int) async throws -> CGImage {
        state.withLock { $0.thumbs.append(item.id) }
        if item.path.hasPrefix("/offline") { throw MockError.notFound(item.path) }
        return try #require(solidImage(width: 3, height: 2))
    }
}

func solidImage(width: Int, height: Int) -> CGImage? {
    let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
    ctx?.setFillColor(CGColor(red: 1, green: 0, blue: 0, alpha: 1))
    ctx?.fill(CGRect(x: 0, y: 0, width: width, height: height))
    return ctx?.makeImage()
}

func libraryItem(_ id: Int64, session: Int64 = 2, root: String = "/tmp/home", variant: Int64? = nil, rating: UInt8 = 0,
                 file: String? = nil) -> LibraryItemRecord {
    LibraryItemRecord(id: id, sessionId: session, sessionRoot: root, sessionName: session == 1 ? "Island" : "Home",
                      path: "\(root)/\(file ?? "IMG\(id).ARW")", ordinal: 1, variant: variant ?? id, captureTimeMs: nil,
                      rating: rating, label: .none, pick: .unflagged, camera: "ILCE-9", lens: "", keywords: [], title: "",
                      hasLocation: false, editHash: nil)
}

@MainActor
@Suite(.serialized) struct LibraryTests {
    /// The Island session (GeoStore, photos 1…8) open; the Library has 300 photos
    /// of "Home" (ids 101…400) and Island's photos 2 and 3 (ids 1002, 1003).
    func model(_ b: FakeLibraryBackend = FakeLibraryBackend(), store: GeoStore = GeoStore()) async -> AppModel {
        b.state.withLock { s in
            s.items = (101...400).map { libraryItem(Int64($0), rating: UInt8($0 % 6)) }
                + [libraryItem(1002, session: 1, root: GeoStore.session.url.path, variant: 2, file: "DSC02.ARW"),
                   libraryItem(1003, session: 1, root: GeoStore.session.url.path, variant: 3, file: "DSC03.ARW")]
        }
        let m = makeModel(store: store)
        m.library.configure(backend: b)
        await m.start()
        let ready = await waitUntil { m.photoCount == 8 && m.currentSession == GeoStore.session }
        #expect(ready, "session \(String(describing: m.currentSession?.url.path)) photos \(m.photoCount)")
        return m
    }

    @Test func libraryModeIsInTheModeSwitchAndNeedsNoSession() async {
        let b = FakeLibraryBackend()
        let m = await model(b)
        #expect(m.library.isAvailable)
        m.browserMode = .library
        m.library.appeared()
        await m.library.settled()
        #expect(m.library.count == 302)
        #expect(m.library.item(at: 0)?.id == 101)
        #expect(m.library.item(at: LibraryModel.pageSize) == nil, "only the first page's details")
        #expect(m.library.item(at: -1) == nil && m.library.item(at: 999) == nil)
        #expect(b.state.get.synced == [GeoStore.session], "the open session is synced in the background")
        #expect(b.state.get.refreshes == 1)
        #expect(m.library.countText == "302 photos")
        #expect(m.library.title == "All Photographs")
        // Nothing changed in the background: no second query.
        #expect(b.state.get.queries.count == 2, "the first query, then one after the open session's sync")
        b.state.withLock { $0.refreshChanges = true }
        m.library.appeared()
        await m.library.settled()
        #expect(b.state.get.queries.count == 3)
    }

    @Test func cellsPageTheirDetailsInAndPrefetchThumbnails() async {
        let b = FakeLibraryBackend()
        let m = await model(b)
        let lib = m.library
        await lib.reload()
        lib.columns = 4
        lib.cellAppeared(130, bucket: 256)
        await lib.settled()
        #expect(lib.item(at: 130)?.id == 231)
        #expect(lib.item(at: 2 * LibraryModel.pageSize) == nil, "page 1 only")
        lib.cellAppeared(LibraryModel.pageSize + 100, bucket: 256) // late in page 1: page 2 as well
        await lib.settled()
        #expect(lib.item(at: 2 * LibraryModel.pageSize + 5) != nil)
        let pages = b.state.get.pages.count
        lib.cellAppeared(131, bucket: 256)
        await lib.settled()
        #expect(b.state.get.pages.count == pages, "a loaded page isn't fetched again")
        await lib.thumbnails.prefetched()
        #expect(b.state.get.thumbs.count >= 8, "the next two rows")
        // A new query keeps nothing stale: its pages come back again.
        lib.filter.minRating = 5
        await lib.settled()
        #expect(lib.count == 50 && lib.item(at: 0)?.rating == 5)
        #expect(b.state.get.queries.last?.minRating == 5)
    }

    @Test func scopesAndFiltersMakeTheQuery() async {
        let m = await model()
        let lib = m.library
        var f = LibraryFilter()
        #expect(!f.isActive)
        f.minRating = 3
        f.unrated = true
        f.label = .red
        f.pick = .unrejected
        f.camera = "ILCE-9"
        f.keyword = "Trail"
        f.text = "hike"
        f.sort = .name
        f.descending = true
        #expect(f.isActive)
        lib.filter = f
        lib.scope = .session(2)
        var q = lib.query
        #expect(q.sessions == [2] && q.minRating == 3 && q.unrated && q.labels == [.red] && q.pick == .unrejected)
        #expect(q.cameras == ["ILCE-9"] && q.keywords == ["Trail"] && q.text == "hike" && q.sort == .name && q.descending)
        lib.scope = .collection(7)
        #expect(lib.query.collection == 7 && lib.query.sessions.isEmpty)
        lib.scope = .day(2024, 10, 5)
        q = lib.query
        #expect(q.fromMs == 1_728_086_400_000 && q.toMs == 1_728_172_800_000)
        lib.scope = .month(2024, 12)
        #expect(lib.query.fromMs == 1_733_011_200_000 && lib.query.toMs == 1_735_689_600_000)
        lib.scope = .year(2024)
        #expect(lib.query.fromMs == 1_704_067_200_000 && lib.query.toMs == 1_735_689_600_000)
        #expect(lib.title == "2024")
        #expect(LibraryDates.title(.month(2024, 10)) == "October 2024")
        #expect(LibraryDates.title(.day(2024, 10, 5)) == "October 5, 2024")
        #expect(LibraryDates.title(.all) == "" && LibraryDates.range(.all) == nil)
        #expect(LibraryDates.monthName(13) == "13")
        for (p, r) in [(PhotoFilter.PickFilter.all, LibraryPickFilter.all), (.picked, .picked), (.unflagged, .unflagged),
                       (.rejected, .rejected)] {
            #expect(LibraryModel.pick(p) == r)
        }
        await lib.settled()
    }

    @Test func datesMakeATreeNewestFirst() {
        let days = [LibraryDayRecord(year: 2023, month: 3, day: 1, count: 2),
                    LibraryDayRecord(year: 2024, month: 10, day: 5, count: 2),
                    LibraryDayRecord(year: 2024, month: 10, day: 6, count: 1),
                    LibraryDayRecord(year: 2024, month: 2, day: 9, count: 4)]
        let t = LibraryDates.tree(days)
        #expect(t.map(\.year) == [2024, 2023])
        #expect(t[0].count == 7 && t[0].months.map(\.month) == [10, 2])
        #expect(t[0].months[0].count == 3 && t[0].months[0].days.map(\.day) == [6, 5])
        #expect(LibraryDates.tree([]).isEmpty)
    }

    @Test func clicksSelectToggleAndExtend() async {
        let m = await model()
        let lib = m.library
        await lib.reload()
        lib.click(101)
        #expect(lib.selection == [101] && lib.focused == 101)
        lib.click(104, shift: true)
        #expect(lib.selection == [101, 102, 103, 104] && lib.focused == 104)
        lib.click(102, command: true)
        #expect(lib.selection == [101, 103, 104])
        lib.click(110, command: true)
        #expect(lib.selection.contains(110) && lib.focused == 110)
        #expect(lib.selectedIDs == [101, 103, 104, 110])
        lib.selectAll()
        #expect(lib.selection.count == 302)
        // A query drops what it no longer shows from the selection.
        lib.filter.minRating = 5
        await lib.settled()
        #expect(lib.selection.allSatisfy { lib.ids.contains($0) })
        #expect(lib.countText == "50 of 302 photos")
    }

    @Test func editsGoThroughTheOpenSessionOrTheOwningSession() async {
        let b = FakeLibraryBackend()
        let store = GeoStore()
        let m = await model(b, store: store)
        let lib = m.library
        await lib.reload()
        lib.cellAppeared(301, bucket: 256)
        await lib.settled()
        lib.selection = [101, 1002, 1003]
        m.browserMode = .library
        m.perform(.rate(4)) // the keys rate the Library's selection here
        await lib.editsSettled()
        #expect(lib.item(101)?.rating == 4)
        #expect(b.state.get.edits.map(\.0) == [[101]], "only the other session's photo through the backend")
        #expect(m.photo(PhotoID(2))?.rating == 4 && m.photo(PhotoID(3))?.rating == 4, "the open session's, as the browser")
        #expect(store.saved.contains { $0.id == PhotoID(2) && $0.rating == 4 })
        #expect(m.history.undoLabel == "Rate 4", "undoable in the session")
        #expect(b.state.get.synced.count >= 1)

        // Labels toggle like the browser's: the label every selected photo has is cleared.
        lib.selection = [101]
        m.perform(.tag(.red))
        await lib.editsSettled()
        #expect(lib.item(101)?.label == .red)
        m.perform(.tag(.red))
        await lib.editsSettled()
        #expect(lib.item(101)?.label == ColorLabel.none)
        lib.selection = [101, 1003]
        m.perform(.pick)
        await lib.editsSettled()
        #expect(lib.item(101)?.pick == .picked && m.photo(PhotoID(3))?.pick == .picked)
        m.perform(.reject)
        m.perform(.unflag)
        await lib.editsSettled()
        #expect(lib.item(101)?.pick == .unflagged && m.photo(PhotoID(3))?.pick == PickState.none)
        m.perform(.rate(0))
        await lib.editsSettled()
        #expect(m.history.undoLabel == "Clear Rating")

        b.state.withLock { $0.editFailures = ["Home: offline"] }
        lib.selection = [101]
        lib.rate(2)
        await lib.editsSettled()
        #expect(m.toast?.message.contains("Home: offline") == true && m.toast?.isError == true)
        lib.selection = []
        lib.rate(3) // nothing selected: nothing happens
        await lib.editsSettled()
        // The browser's own keys still rate the browser outside the Library.
        m.browserMode = .grid
        m.focus(PhotoID(5), extend: false)
        m.perform(.rate(1))
        #expect(m.photo(PhotoID(5))?.rating == 1)
    }

    @Test func openInSessionShowsThePhoto() async {
        let m = await model()
        let lib = m.library
        await lib.reload()
        lib.cellAppeared(301, bucket: 256)
        await lib.settled()
        m.browserMode = .library
        m.filter.minRating = 5 // hides photo 3
        await lib.open(1003)
        #expect(m.browserMode == .filmstrip && m.focusedID == PhotoID(3))
        #expect(m.filter.minRating == 0, "the browser's filter is cleared to show it")
        // Another session: demo mode can't open folders (a toast says so); nothing else happens.
        m.browserMode = .library
        await lib.open(101)
        #expect(m.browserMode == .library)
        await lib.open(-1)
    }

    @Test func collectionsAreMadeFilledAndRemoved() async {
        let b = FakeLibraryBackend()
        let m = await model(b)
        let lib = m.library
        await lib.reload()
        lib.selection = [101, 102]
        await lib.newCollection(named: "Best")
        #expect(lib.scope == .collection(1))
        #expect(lib.title == "Best" && lib.plainCollection == 1)
        #expect(lib.ids == [101, 102])
        // Added from All (the selection is of what the grid shows), then shown again.
        lib.scope = .all
        await lib.settled()
        lib.selection = [103]
        await lib.addSelection(to: 1)
        lib.scope = .collection(1)
        await lib.settled()
        #expect(lib.ids == [101, 102, 103])
        lib.selection = [101]
        await lib.removeSelectionFromCollection()
        #expect(lib.ids == [102, 103])
        await lib.renameCollection(1, to: "Keepers")
        #expect(lib.collections.map(\.name) == ["Keepers"])

        lib.scope = .session(2)
        lib.filter.minRating = 4
        await lib.newSmartCollection(named: "Good")
        let rules = b.state.get.collections.last?.rules
        #expect(rules?.minRating == 4 && rules?.sessions == [2])
        #expect(lib.scope == .collection(2) && lib.filter == LibraryFilter(), "shows it, the filter now in its rules")
        #expect(lib.plainCollection == nil, "a smart one")
        await lib.removeSelectionFromCollection() // not a plain one: nothing
        await lib.deleteCollection(2)
        #expect(lib.scope == .all)
        await lib.deleteCollection(1)
        #expect(lib.collections.isEmpty)
        // A deleted collection the sidebar still had selected falls back to All.
        lib.scope = .collection(99)
        await lib.settled()
        #expect(lib.scope == .all)

        b.state.withLock { $0.fail = MockError.invalid("index busy") }
        await lib.newCollection(named: "X")
        #expect(m.toast?.isError == true)
        await lib.reload()
        #expect(lib.phase == .failed(PeopleModel.message(MockError.invalid("index busy"))))
    }

    @Test func thumbnailsAreCachedAndOfflineOnesFail() async {
        let b = FakeLibraryBackend()
        let t = LibraryThumbnails()
        t.backend = b
        let a = libraryItem(1)
        #expect(t.cached(a, bucket: 256) == nil)
        #expect(await t.image(for: a, bucket: 256) != nil)
        #expect(t.cached(a, bucket: 256) != nil && t.loads == 1)
        _ = await t.image(for: a, bucket: 256)
        #expect(t.loads == 1, "a hit")
        var edited = a
        edited.editHash = "ab"
        #expect(t.cached(edited, bucket: 256) == nil, "another edit, another entry")
        #expect(await t.image(for: libraryItem(2, root: "/offline"), bucket: 256) == nil)
        t.prefetch([libraryItem(3), libraryItem(4), a], bucket: 256)
        await t.prefetched()
        #expect(t.cached(libraryItem(4), bucket: 256) != nil)
        t.prefetch([a], bucket: 256) // all cached: nothing to do
        t.removeAll()
        #expect(t.cached(a, bucket: 256) == nil)
        let none = LibraryThumbnails()
        #expect(await none.image(for: a, bucket: 256) == nil, "no backend")
    }

    @Test func wordingForCellsAndCounts() {
        var i = libraryItem(5, file: "DSC5.ARW")
        #expect(LibraryText.label(i) == "DSC5.ARW, Home")
        i.rating = 3
        i.pick = .picked
        i.label = .red
        #expect(LibraryText.label(i) == "DSC5.ARW, Home, \(RatingText.label(3)), picked, red label")
        i.pick = .rejected
        #expect(LibraryText.label(i).contains("rejected"))
        #expect(LibraryText.thumbKey(nil, 256) == nil)
        #expect(LibraryText.thumbKey(i, 256) == "5@256#")
        #expect(LibraryModel.same("/tmp/x", "/private/tmp/x"))
        #expect(!LibraryModel.same("/tmp/x", "/tmp/y"))
        let (open, others) = LibraryModel.split([libraryItem(1, root: "/tmp/k"), libraryItem(2, root: "/tmp/h")],
                                                openRoot: "/private/tmp/k")
        #expect(open.map(\.id) == [1] && others.map(\.id) == [2])
        #expect(LibraryModel.split([libraryItem(1)], openRoot: nil).others.count == 1)
    }

    @Test func theRustBackendPrefersADevelopedThumbnail() async throws {
        struct Previews: PreviewProvider {
            func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
                try #require(solidImage(width: 4, height: 4))
            }
        }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-lib-\(UUID())", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: dir)
        let developed = try #require(solidImage(width: 2, height: 2))
        let b = RustLibraryBackend(store: store, previews: Previews(), globalIndex: dir.appendingPathComponent("g.db")) { _, v, hash in
            v == 7 && hash == "ab" ? developed : nil
        }
        var item = libraryItem(1, variant: 7)
        #expect(try await b.thumbnail(for: item, maxPixelSize: 256).width == 4, "unedited: the embedded preview")
        item.editHash = "ab"
        #expect(try await b.thumbnail(for: item, maxPixelSize: 256).width == 2, "edited: the developed one")
        item.editHash = "cd"
        #expect(try await b.thumbnail(for: item, maxPixelSize: 256).width == 4, "not rendered yet: the preview")
        let plain = RustLibraryBackend(store: store, previews: Previews())
        #expect(try await plain.thumbnail(for: item, maxPixelSize: 256).width == 4)
        let tools = RustDevelopTools(thumbnailCache: dir.appendingPathComponent("Thumbs"))
        #expect(await tools.cachedThumbnail(url: dir.appendingPathComponent("x.ARW"), variant: 1, editHash: String(repeating: "0", count: 64)) == nil)
        let none = RustDevelopTools(thumbnailCache: nil)
        #expect(await none.cachedThumbnail(url: dir, variant: 1, editHash: "0") == nil)
    }
}
