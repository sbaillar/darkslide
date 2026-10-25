import CoreGraphics
import DarkslideFFI
import Foundation
import Observation

/// The filter bar's criteria (the sidebar's scope adds sessions, dates or a collection).
public struct LibraryFilter: Equatable, Sendable {
    public var minRating = 0
    /// Only photos without stars.
    public var unrated = false
    public var label: ColorTag?
    public var pick: PhotoFilter.PickFilter = .all
    public var camera: String?
    public var keyword: String?
    public var text = ""
    public var sort: LibrarySortKey = .captured
    public var descending = false

    public init() {}

    /// Anything narrows the photos (the order aside).
    public var isActive: Bool {
        minRating > 0 || unrated || label != nil || pick != .all || camera != nil || keyword != nil
            || !text.trimmingCharacters(in: .whitespaces).isEmpty
    }
}

/// [library] The Library mode (like Lightroom's Library module): every session's
/// photos in one virtualized grid, with Sessions, Dates and Collections in the
/// sidebar, filters, find by text, and rating, label and flag edits written to
/// the owning session.
///
/// Fast by construction: a query returns only ids (in order) plus the first
/// screen's details; cells ask for their page (`cellAppeared`), which loads it
/// and the next one off the main actor; thumbnails come from the disk cache or
/// the embedded preview, decoded off the main actor and cached (`thumbnails`).
/// Opening shows what the index has at once; the open session's sync and every
/// other changed session's refresh run in the background and reload when done.
/// Details are kept for a bounded number of pages (`maxLoadedPages`): far ones
/// load again when shown.
@MainActor @Observable
public final class LibraryModel {
    public enum Phase: Equatable, Sendable {
        case idle, loading
        case failed(String)
    }

    /// What the sidebar has selected.
    public enum Scope: Hashable, Sendable {
        case all
        case session(Int64)
        /// Several sessions as one (⌘-click in the sidebar), ids sorted.
        case sessions([Int64])
        /// The sessions the user has opened (the recents) that the Library knows, as one.
        case opened
        case year(Int32)
        case month(Int32, UInt32)
        case day(Int32, UInt32, UInt32)
        case collection(Int64)
    }

    /// Details are fetched this many at a time.
    public static let pageSize = 120

    public var scope: Scope = .all {
        didSet { if scope != oldValue { requery() } }
    }
    public var filter = LibraryFilter() {
        didSet {
            guard filter != oldValue else { return }
            requery(debounce: filter.text != oldValue.text ? .milliseconds(150) : .zero)
        }
    }
    /// The photos shown, in order.
    public private(set) var ids: [Int64] = []
    public private(set) var overview: LibraryOverviewRecord?
    public private(set) var phase: Phase = .idle
    public var selection: Set<Int64> = []
    public private(set) var focused: Int64?
    /// Grid columns (the view reports them; prefetching reads a row ahead).
    public var columns = 6
    /// Bumped whenever item details change in place (a cell reads `item(at:)`).
    public private(set) var itemsVersion = 0

    @ObservationIgnored public private(set) var backend: (any LibraryBackend)?
    @ObservationIgnored public let thumbnails = LibraryThumbnails()
    @ObservationIgnored weak var app: AppModel?
    @ObservationIgnored private var items: [Int64: LibraryItemRecord] = [:]
    @ObservationIgnored private var pagesLoaded: Set<Int> = []
    @ObservationIgnored private var pagesLoading: Set<Int> = []
    @ObservationIgnored private var ticket = 0
    @ObservationIgnored private var queryTask: Task<Void, Never>?
    @ObservationIgnored private var syncTask: Task<Void, Never>?
    @ObservationIgnored private var pageTasks: [Int: Task<Void, Never>] = [:]
    @ObservationIgnored private var anchor: Int64?
    @ObservationIgnored private var prefetchedTo = -1
    /// Pages whose details are kept (≈19,000 photos); farther ones are dropped as others load.
    @ObservationIgnored var maxLoadedPages = 160

    public init() {}

    public func configure(backend: any LibraryBackend) {
        self.backend = backend
        thumbnails.backend = backend
    }

    func attach(_ app: AppModel) { self.app = app }

    public var isAvailable: Bool { backend != nil }

    // MARK: - Loading

    /// The view appeared: show what the index has, then sync in the background.
    public func appeared() {
        if overview == nil, phase != .loading { requery(debounce: .zero) }
        syncTask?.cancel()
        syncTask = Task { [weak self] in await self?.syncInBackground() }
    }

    /// The open session reaches the index, then every other session that changed;
    /// the view reloads only when something did.
    func syncInBackground() async {
        guard let backend else { return }
        var changed = false
        if let app, let session = app.currentSession {
            await app.saver.flush() // its latest ratings first
            if (try? await backend.sync(session)) != nil { changed = true }
        }
        if (try? await backend.refresh()) == true { changed = true }
        guard !Task.isCancelled, changed else { return }
        await reload()
    }

    /// The query the scope and the filter make.
    public var query: LibraryQueryRecord { query(overview) }

    /// The query, with Opened Sessions resolved against `ov`'s sessions.
    func query(_ ov: LibraryOverviewRecord?) -> LibraryQueryRecord {
        var q = LibraryQueryRecord(sessions: [], fromMs: nil, toMs: nil, minRating: UInt8(clamping: filter.minRating),
                                   unrated: filter.unrated, labels: filter.label.map { [RustSessionStore.label($0)] } ?? [],
                                   pick: Self.pick(filter.pick), cameras: filter.camera.map { [$0] } ?? [],
                                   keywords: filter.keyword.map { [$0] } ?? [], text: filter.text, collection: nil,
                                   sort: filter.sort, descending: filter.descending)
        switch scope {
        case .all: break
        case .session(let id): q.sessions = [id]
        case .sessions(let ids): q.sessions = ids
        case .opened: q.sessions = openedSessionIDs(ov)
        case .collection(let id): q.collection = id
        case .year, .month, .day:
            if let r = LibraryDates.range(scope) { (q.fromMs, q.toMs) = (r.0, r.1) }
        }
        return q
    }

    /// The scope names sessions but none the Library knows (`sessions: []` would mean every one).
    func matchesNoSession(_ q: LibraryQueryRecord) -> Bool {
        switch scope {
        case .sessions, .opened: q.sessions.isEmpty
        default: false
        }
    }

    // MARK: - Opened Sessions

    /// [library] The recents (the sessions list) the Library has indexed, as its session ids,
    /// most recently opened first. Recents it doesn't know (never synced) are left out.
    public var openedSessionIDs: [Int64] { openedSessionIDs(overview) }

    func openedSessionIDs(_ ov: LibraryOverviewRecord?) -> [Int64] {
        guard let app, let ov else { return [] }
        return Self.resolve(recents: app.sessionList.map(\.url.path), in: ov.sessions)
    }

    /// Photos in Opened Sessions (the sidebar's badge).
    public var openedCount: Int {
        let ids = Set(openedSessionIDs)
        return Int((overview?.sessions ?? []).filter { ids.contains($0.id) }.reduce(0) { $0 + $1.count })
    }

    /// Recent folders → the ids of the index's sessions at those folders, in the recents'
    /// order, each once. Compared by spelling only (no file-system access: a recent may be
    /// on a drive that isn't connected).
    nonisolated static func resolve(recents: [String], in sessions: [LibrarySessionRecord]) -> [Int64] {
        var byFolder: [String: Int64] = [:]
        for s in sessions where byFolder[folderKey(s.root)] == nil { byFolder[folderKey(s.root)] = s.id }
        var seen = Set<Int64>()
        return recents.compactMap { r in
            guard let id = byFolder[folderKey(r)], seen.insert(id).inserted else { return nil }
            return id
        }
    }

    /// `path` without `.`/`..`, a trailing `/` or macOS's `/private` prefix of `/var`, `/tmp`
    /// and `/etc`: two spellings of one folder compare equal (like ds-ffi's `same_folder_key`).
    nonisolated static func folderKey(_ path: String) -> String {
        var p = URL(fileURLWithPath: path).standardized.path
        for top in ["/var", "/tmp", "/etc"] where p == "/private" + top || p.hasPrefix("/private" + top + "/") {
            p.removeFirst("/private".count)
        }
        while p.count > 1, p.hasSuffix("/") { p.removeLast() }
        return p
    }

    // MARK: - Sidebar selection

    /// What the sidebar shows selected for `scope` (a multi-session scope: each session).
    public static func sidebarSelection(_ scope: Scope) -> Set<Scope> {
        if case .sessions(let ids) = scope { return Set(ids.map(Scope.session)) }
        return [scope]
    }

    /// The scope a sidebar selection makes: two or more sessions (⌘-click) are one
    /// multi-session scope; a row added next to sessions that isn't a session replaces
    /// them. `nil`: nothing selected (the scope stays).
    public static func scope(sidebar: Set<Scope>, current: Scope) -> Scope? {
        guard !sidebar.isEmpty else { return nil }
        let added = sidebar.subtracting(sidebarSelection(current))
        let sessions = sidebar.compactMap { s -> Int64? in if case .session(let id) = s { id } else { nil } }.sorted()
        if let other = added.first(where: { if case .session = $0 { false } else { true } }) { return other }
        switch sessions.count {
        case 0: return sidebar.first
        case 1: return .session(sessions[0])
        default: return .sessions(sessions)
        }
    }

    nonisolated static func pick(_ p: PhotoFilter.PickFilter) -> LibraryPickFilter {
        switch p {
        case .all: .all
        case .picked: .picked
        case .unflagged: .unflagged
        case .unrejected: .unrejected
        case .rejected: .rejected
        }
    }

    /// Run the query again (after `debounce`, for typing).
    public func requery(debounce: Duration = .zero) {
        queryTask?.cancel()
        queryTask = Task { [weak self] in
            if debounce > .zero {
                try? await Task.sleep(for: debounce)
                guard !Task.isCancelled else { return }
            }
            await self?.reload()
        }
    }

    /// Wait for the pending query (tests).
    func settled() async {
        await queryTask?.value
        await syncTask?.value
        while let (page, t) = pageTasks.first {
            await t.value
            if pageTasks[page] == t { pageTasks[page] = nil }
        }
    }

    /// Query and overview again; details already shown stay until their page is back.
    public func reload() async {
        guard let backend else { return }
        ticket += 1
        let mine = ticket
        if ids.isEmpty { phase = .loading }
        do {
            let p: LibraryPage
            let ov: LibraryOverviewRecord
            if scope == .opened {
                // The recents resolve against the index's sessions: its overview first.
                ov = try await backend.overview()
                let q = query(ov)
                p = matchesNoSession(q) ? LibraryPage(ids: [], first: []) : try await backend.query(q, first: Self.pageSize)
            } else {
                let q = query
                let none = matchesNoSession(q)
                async let page = none ? LibraryPage(ids: [], first: []) : try await backend.query(q, first: Self.pageSize)
                async let o = backend.overview()
                (p, ov) = try await (page, o)
            }
            guard mine == ticket else { return }
            let refetch = pagesLoaded.subtracting([0])
            pagesLoaded = [0]
            pagesLoading = []
            pageTasks = [:] // stale ones drop their replies (ticket)
            if items.count > 20_000 { items = [:] } // bounded: far pages load again when shown
            for i in p.first { items[i.id] = i }
            if ids != p.ids {
                ids = p.ids
                prefetchedTo = -1
            }
            overview = ov
            selection.formIntersection(Set(p.ids))
            if let f = focused, !selection.contains(f) { focused = selection.first }
            itemsVersion += 1
            phase = .idle
            if case .collection(let id) = scope, !ov.collections.contains(where: { $0.id == id }) { scope = .all }
            for page in refetch.sorted() { load(page: page) }
        } catch is CancellationError {
            if mine == ticket { phase = .idle }
        } catch {
            if mine == ticket { phase = .failed(PeopleModel.message(error)) }
        }
    }

    // MARK: - Paging

    public var count: Int { ids.count }

    /// The details of the photo at `index` (`nil` until its page is loaded).
    public func item(at index: Int) -> LibraryItemRecord? {
        guard ids.indices.contains(index) else { return nil }
        return items[ids[index]]
    }

    public func item(_ id: Int64) -> LibraryItemRecord? { items[id] }

    /// A cell at `index` came on screen: its page (and the one after) load,
    /// and the next rows' thumbnails are prefetched.
    public func cellAppeared(_ index: Int, bucket: Int) {
        let page = index / Self.pageSize
        load(page: page)
        if (index % Self.pageSize) > Self.pageSize / 2 { load(page: page + 1) }
        let ahead = index + 2 * max(columns, 1)
        if ahead > prefetchedTo + columns {
            prefetchedTo = ahead
            let next = (index + 1...ahead).compactMap { item(at: $0) }
            thumbnails.prefetch(next, bucket: bucket)
        }
    }

    func load(page: Int) {
        guard let backend, page >= 0, page * Self.pageSize < ids.count,
              !pagesLoaded.contains(page), !pagesLoading.contains(page) else { return }
        let slice = Array(ids[(page * Self.pageSize)..<min(ids.count, (page + 1) * Self.pageSize)])
        pagesLoading.insert(page)
        let mine = ticket
        pageTasks[page] = Task { [weak self] in
            let got = try? await backend.items(slice)
            guard let self, mine == self.ticket else { return }
            self.pageTasks[page] = nil
            self.pagesLoading.remove(page)
            guard let got else { return }
            for i in got { self.items[i.id] = i }
            self.pagesLoaded.insert(page)
            self.dropFarPages(from: page)
            self.itemsVersion += 1
        }
    }

    /// Details loaded (tests: the bound holds while scrolling).
    var loadedDetails: Int { items.count }

    /// Past `maxLoadedPages`, the pages farthest from `page` let go of their details (the
    /// selection's stay: edits need them); they load again when their cells come back.
    private func dropFarPages(from page: Int) {
        let extra = pagesLoaded.count - maxLoadedPages
        guard extra > 0 else { return }
        for far in pagesLoaded.sorted(by: { abs($0 - page) > abs($1 - page) }).prefix(extra) {
            pagesLoaded.remove(far)
            let range = (far * Self.pageSize)..<min(ids.count, (far + 1) * Self.pageSize)
            for id in ids[range] where !selection.contains(id) { items[id] = nil }
        }
    }

    // MARK: - Selection

    /// Click on the photo `id`: select it (⌘ toggles, ⇧ extends from the last click).
    public func click(_ id: Int64, command: Bool = false, shift: Bool = false) {
        if command {
            if selection.contains(id) { selection.remove(id) } else { selection.insert(id) }
            focused = selection.contains(id) ? id : selection.first
            anchor = id
        } else if shift, let anchor, let a = ids.firstIndex(of: anchor), let b = ids.firstIndex(of: id) {
            selection = Set(ids[min(a, b)...max(a, b)])
            focused = id
        } else {
            selection = [id]
            focused = id
            anchor = id
        }
    }

    public func selectAll() { selection = Set(ids) }

    /// The selection in grid order.
    public var selectedIDs: [Int64] { ids.filter(selection.contains) }

    // MARK: - Edits (written to the owning sessions)

    /// Rate the selection (0 clears).
    public func rate(_ stars: Int) {
        let n = min(max(stars, 0), 5)
        edit(.rating(stars: UInt8(n)), label: n == 0 ? "Clear Rating" : "Rate \(n)") { $0.rating = UInt8(n) } photo: { $0.rating = n }
    }

    /// Toggles, like the browser: the tag every selected photo already has is cleared.
    public func setLabel(_ tag: ColorTag) {
        let sel = selectedIDs
        let all = !sel.isEmpty && sel.allSatisfy { items[$0].map { RustSessionStore.tag($0.label) == tag } ?? false }
        let value: ColorTag = all ? .none : tag
        let l = RustSessionStore.label(value)
        edit(.label(label: l), label: "Color Tag \(value.name)") { $0.label = l } photo: { $0.colorTag = value }
    }

    public func setFlag(_ p: PickState) {
        let f = RustSessionStore.flag(p)
        let label = switch p {
        case .picked: "Pick"
        case .rejected: "Reject"
        case .none: "Unflag"
        }
        edit(.flag(pick: f), label: label) { $0.pick = f } photo: { $0.pick = p }
    }

    @ObservationIgnored private var editTask: Task<Void, Never>?

    /// Wait for the last edit to be written (tests).
    func editsSettled() async { await editTask?.value }

    /// Photos of the open session change through the app (undo, the browser);
    /// the others are written by their sessions through the backend.
    private func edit(_ change: LibraryEditRecord, label: String, _ patch: (inout LibraryItemRecord) -> Void,
                      photo: @escaping (inout Photo) -> Void) {
        let sel = selectedIDs.compactMap { items[$0] }
        guard let backend, !sel.isEmpty else { return }
        let (open, others) = LibraryModel.split(sel, openRoot: app?.currentSession?.url.path)
        for i in sel {
            guard var item = items[i.id] else { continue }
            patch(&item)
            items[i.id] = item
        }
        itemsVersion += 1
        if let app, !open.isEmpty {
            app.updatePhotos(open.map { PhotoID($0.variant) }, label: label, photo)
        }
        let previous = editTask
        editTask = Task { [weak self] in
            await previous?.value
            var failed: [String] = []
            if !others.isEmpty {
                do {
                    failed = try await backend.edit(others.map(\.id), change).failed
                } catch {
                    failed = [PeopleModel.message(error)]
                }
            }
            guard let self else { return }
            if !failed.isEmpty {
                self.app?.show(Toast("Couldn’t change \(failed.joined(separator: "; "))", symbol: "exclamationmark.triangle",
                                     isError: true))
            }
            if !open.isEmpty, let app = self.app, let session = app.currentSession {
                await app.saver.flush()
                try? await backend.sync(session)
            }
            await self.reload()
        }
    }

    /// Paths compare as the user's folders (`/var` and `/private/var` are one).
    nonisolated static func same(_ a: String, _ b: String) -> Bool {
        a == b || canonical(a) == canonical(b)
    }

    /// `path` with its longest existing part resolved by `realpath` (`/tmp` → `/private/tmp`)
    /// and the rest appended: `resolvingSymlinksInPath` leaves a path that doesn't exist as is.
    nonisolated static func canonical(_ path: String) -> String {
        var head = URL(fileURLWithPath: path).standardizedFileURL
        var tail: [String] = []
        while !FileManager.default.fileExists(atPath: head.path), head.path != "/" {
            tail.insert(head.lastPathComponent, at: 0)
            head.deleteLastPathComponent()
        }
        let resolved = realpath(head.path, nil).map { p in
            defer { free(p) }
            return String(cString: p)
        } ?? head.path
        return tail.reduce(URL(fileURLWithPath: resolved)) { $0.appendingPathComponent($1) }.path
    }

    /// The items of the open session (by its folder) and the others.
    nonisolated static func split(_ items: [LibraryItemRecord], openRoot: String?)
        -> (open: [LibraryItemRecord], others: [LibraryItemRecord]) {
        guard let openRoot else { return ([], items) }
        var verdict: [String: Bool] = [:]
        var open: [LibraryItemRecord] = []
        var others: [LibraryItemRecord] = []
        for i in items {
            let mine = verdict[i.sessionRoot] ?? same(i.sessionRoot, openRoot)
            verdict[i.sessionRoot] = mine
            if mine { open.append(i) } else { others.append(i) }
        }
        return (open, others)
    }

    // MARK: - Open in session

    /// Open the photo's session and show the photo there (the Viewer).
    public func open(_ id: Int64) async {
        guard let app, let item = items[id] else { return }
        let root = URL(fileURLWithPath: item.sessionRoot)
        let isOpen = { app.currentSession.map { Self.same($0.url.path, root.path) } ?? false }
        if !isOpen() { await app.openFolder(root) }
        guard isOpen() else { return }
        let pid = PhotoID(item.variant)
        if app.photo(pid) != nil {
            if !app.visibleIDs.contains(pid) { // hidden by the browser's filter or a search: show everything
                app.search.clear()
                app.filter = PhotoFilter()
            }
            app.focus(pid, extend: false)
        }
        app.browserMode = .filmstrip
    }

    // MARK: - Collections

    public var collections: [LibraryCollectionRecord] { overview?.collections ?? [] }

    /// The collection the sidebar has selected, if it is a plain one.
    public var plainCollection: Int64? {
        guard case .collection(let id) = scope, collections.first(where: { $0.id == id })?.rules == nil else { return nil }
        return id
    }

    /// A new collection holding the selection; the sidebar shows it.
    public func newCollection(named name: String) async {
        await collectionCall {
            let id = try await $0.createCollection(name: name, rules: nil, ids: self.selectedIDs)
            self.scope = .collection(id)
        }
    }

    /// A new smart collection from what the Library shows now (scope and filter).
    public func newSmartCollection(named name: String) async {
        var rules = query
        if case .collection = scope { rules.collection = nil }
        await collectionCall {
            let id = try await $0.createCollection(name: name, rules: rules, ids: [])
            self.filter = LibraryFilter()
            self.scope = .collection(id)
        }
    }

    public func addSelection(to collection: Int64) async {
        let ids = selectedIDs
        await collectionCall { _ = try await $0.members(of: collection, ids, remove: false) }
    }

    /// Take the selection out of the plain collection shown.
    public func removeSelectionFromCollection() async {
        guard let id = plainCollection else { return }
        let ids = selectedIDs
        await collectionCall { _ = try await $0.members(of: id, ids, remove: true) }
    }

    public func renameCollection(_ id: Int64, to name: String) async {
        await collectionCall { _ = try await $0.updateCollection(id, name: name, rules: nil) }
    }

    public func deleteCollection(_ id: Int64) async {
        await collectionCall {
            _ = try await $0.deleteCollection(id)
            if self.scope == .collection(id) { self.scope = .all }
        }
    }

    private func collectionCall(_ body: (any LibraryBackend) async throws -> Void) async {
        guard let backend else { return }
        do {
            try await body(backend)
            // A scope change in `body` already queued a query; the newest one wins, so wait for it.
            requery()
            await queryTask?.value
        } catch {
            app?.show(Toast(PeopleModel.message(error), symbol: "exclamationmark.triangle", isError: true))
        }
    }

    // MARK: - Wording

    /// "1,234 photos" (of how many in the Library when filtered).
    public var countText: String {
        let n = ids.count.formatted()
        let noun = ids.count == 1 ? "photo" : "photos"
        guard let total = overview?.total, Int(total) != ids.count else { return "\(n) \(noun)" }
        return "\(n) of \(Int(total).formatted()) \(Int(total) == 1 ? "photo" : "photos")"
    }

    /// The sidebar selection's title.
    public var title: String {
        switch scope {
        case .all: "All Photographs"
        case .session(let id): overview?.sessions.first { $0.id == id }?.name ?? "Session"
        case .sessions(let ids) where ids.count == 2:
            ids.map { id in overview?.sessions.first { $0.id == id }?.name ?? "Session" }.joined(separator: " + ")
        case .sessions(let ids): "\(ids.count) Sessions"
        case .opened: "Opened Sessions"
        case .collection(let id): collections.first { $0.id == id }?.name ?? "Collection"
        case .year, .month, .day: LibraryDates.title(scope)
        }
    }
}

extension AppModel {
    /// [library] File ▸ Browse All Sessions in Library (⇧⌘L) and the Open sheet's button.
    public func showLibrary() { browserMode = .library }
}

/// [library] Wording and keys for the Library's cells (outside the views, tested).
public enum LibraryText {
    /// VoiceOver: "DSC0001.ARW, Fall Hike, 3 stars, picked, red label".
    public static func label(_ i: LibraryItemRecord) -> String {
        var parts = [URL(fileURLWithPath: i.path).lastPathComponent, i.sessionName]
        if i.rating > 0 { parts.append(RatingText.label(Int(i.rating))) }
        switch i.pick {
        case .picked: parts.append("picked")
        case .rejected: parts.append("rejected")
        case .unflagged: break
        }
        if i.label != .none { parts.append("\(RustSessionStore.tag(i.label).name.lowercased()) label") }
        return parts.joined(separator: ", ")
    }

    /// What a cell's thumbnail shows: the photo, its edit and the size (a rating never reloads it).
    public static func thumbKey(_ i: LibraryItemRecord?, _ bucket: Int) -> String? {
        i.map { "\($0.id)@\(bucket)#\($0.editHash ?? "")" }
    }
}
