import CoreGraphics
import Foundation
import Observation

/// Search results as the browser shows them: only these photos, best first.
public struct SearchRanking: Equatable, Sendable {
    public let ids: [PhotoID]
    private let rank: [PhotoID: Int]
    public let scores: [PhotoID: Float]

    public init(_ matches: [SearchMatch]) {
        var ids: [PhotoID] = []
        var rank: [PhotoID: Int] = [:]
        var scores: [PhotoID: Float] = [:]
        for m in matches where rank[m.id] == nil {
            rank[m.id] = ids.count
            scores[m.id] = m.score
            ids.append(m.id)
        }
        self.ids = ids
        self.rank = rank
        self.scores = scores
    }

    public func contains(_ id: PhotoID) -> Bool { rank[id] != nil }

    /// `ids` in relevance order (photos not in the results go last, in the order given).
    public func sorted(_ ids: [PhotoID]) -> [PhotoID] {
        ids.enumerated().sorted { a, b in
            (rank[a.element] ?? Int.max, a.offset) < (rank[b.element] ?? Int.max, b.offset)
        }.map(\.element)
    }
}

/// Natural-language search (MobileCLIP, on device): the search field's state,
/// the model download, background indexing of the open session, and results
/// (this session, ranked in the browser, or every session, grouped).
@MainActor @Observable
public final class PhotoSearch {
    public enum Scope: String, CaseIterable, Sendable {
        case session = "This Session"
        case allSessions = "All Sessions"
    }

    public enum Phase: Equatable, Sendable {
        case idle
        /// The model isn't on this Mac yet (the search offers to download it).
        case needsModel
        case searching
        /// Results are shown (`count` photos).
        case results(Int)
        case failed(String)
    }

    public var query = ""
    public var scope: Scope = .session
    public private(set) var phase: Phase = .idle
    /// "Search all sessions" results, best session first.
    public private(set) var sessionResults: [SessionMatches] = []
    /// Showing photos similar to this one (instead of a text query).
    public private(set) var similarTo: PhotoID?
    /// Background indexing of the open session: (done, total) while it runs.
    public private(set) var indexing: SearchIndexer.Progress?
    public private(set) var lastIndexReport: SearchIndexer.Report?

    @ObservationIgnored public private(set) var models: ClipModelStore?
    @ObservationIgnored public private(set) var backend: (any SearchBackend)?
    @ObservationIgnored weak var app: AppModel?
    @ObservationIgnored var makeEncoder: @Sendable (ClipModelFiles) throws -> any SearchEncoding = { try ClipEncoder(files: $0) }
    @ObservationIgnored var makeIndexer: @Sendable (any SearchBackend, any SearchEncoding) -> SearchIndexer = {
        SearchIndexer(backend: $0, encoder: $1)
    }
    @ObservationIgnored private var encoder: (any SearchEncoding)?
    @ObservationIgnored private var loadingEncoder: Task<any SearchEncoding, Error>?
    /// Bumped by every search, Find Similar and clear: a reply to an older one is dropped.
    @ObservationIgnored private var ticket = 0
    /// The last session-scope request, re-run when the browser's filter changes.
    @ObservationIgnored private var lastRequest: Request?
    private enum Request { case text(String), similar(PhotoID) }
    @ObservationIgnored private var indexTask: Task<Void, Never>?
    /// Bumped per indexing run: a cancelled run never touches its successor's state.
    @ObservationIgnored private var indexRun = 0
    @ObservationIgnored private var indexedSession: UUID?
    /// Every indexing run not finished yet, by run, with its session: a cancelled run
    /// may still be inside a Rust write that registers its session globally.
    @ObservationIgnored private var liveRuns: [Int: (session: UUID, task: Task<Void, Never>)] = [:]
    @ObservationIgnored private var searchTask: Task<Void, Never>?

    /// Tuned on MobileCLIP-S2 with the eight A9 samples ("a photo of …"): what the
    /// query describes scores ≈ 0.19–0.31, everything else ≈ 0.03–0.18 ("waterfall":
    /// 0.25 vs ≤ 0.175; "red car", absent: ≤ 0.10). A floor of 0.18 drops queries
    /// that match nothing; the window keeps weak trailing matches out.
    public var tuning = SearchTuning(limit: 1000, minScore: 0.18, window: 0.09)
    /// "Find similar": image-to-image similarities run higher (samples: the same
    /// waterfall from another angle 0.83–0.84, other scenes 0.40–0.52).
    public var similarTuning = SearchTuning(limit: 200, minScore: 0.6, window: 1)
    /// The text a query is wrapped in (CLIP is trained on captions).
    public nonisolated static func prompt(_ query: String) -> String { "a photo of \(query)" }

    public init() {}

    /// Live mode: search through `backend`, with the model from `models`.
    public func configure(backend: any SearchBackend, models: ClipModelStore) {
        self.backend = backend
        self.models = models
    }

    func attach(_ app: AppModel) {
        self.app = app
    }

    /// [find] The encoder is loaded (Find embeds with it, never downloads).
    var hasEncoder: Bool { encoder != nil }

    /// Search works in this mode (live sessions).
    public var isAvailable: Bool { backend != nil && models != nil }

    public var isActive: Bool { app?.searchRanking != nil || !sessionResults.isEmpty || similarTo != nil }

    /// A line for the status bar while indexing runs.
    public var indexingMessage: String? {
        guard let p = indexing, p.total > 0, p.done < p.total else { return nil }
        return "Indexing for search… \(p.done.formatted()) of \(p.total.formatted())"
    }

    // MARK: - Model

    /// The loaded encoder, downloading the model first if needed (progress in `models.state`).
    func readyEncoder() async throws -> any SearchEncoding {
        if let encoder { return encoder }
        guard let models else { throw SearchBackendError.sessionNotOpen }
        let files = try await models.prepare()
        // Another caller may have loaded it (or started to) while this one waited.
        if let encoder { return encoder }
        if let loadingEncoder { return try await loadingEncoder.value }
        // Loading two ~100 MB Core ML models (first time: an ANE compile) takes
        // seconds: never on the main actor, and once for every caller.
        let make = makeEncoder
        let task = Task.detached(priority: .userInitiated) { try make(files) }
        loadingEncoder = task
        defer { loadingEncoder = nil }
        do {
            let e = try await task.value
            encoder = e
            return e
        } catch {
            // Damaged (the files differ from what was installed): fetch it again next time.
            // Intact (a transient failure, e.g. the ANE busy): keep it and try again later.
            if await !models.verify() { models.discard() }
            throw error
        }
    }

    /// Download the model now (the search field's "Download" button); indexing starts after.
    public func downloadModel() async {
        do {
            _ = try await readyEncoder()
            if phase == .needsModel { phase = .idle }
            startIndexing()
            if !query.trimmingCharacters(in: .whitespaces).isEmpty { await search() }
        } catch is CancellationError {
            phase = .needsModel
        } catch {
            phase = .failed((error as? LocalizedError)?.errorDescription ?? error.localizedDescription)
        }
    }

    public func cancelDownload() {
        models?.cancel()
    }

    // MARK: - Indexing

    /// A session was opened: clear the results and index it (when the model is installed).
    func sessionOpened(_ session: SessionInfo) {
        // A refresh of the same session (after an import, a relink) keeps the results.
        if indexedSession != session.id {
            clear()
            dismissKeywordSuggestions()
            indexTask?.cancel()
            indexTask = nil
            indexing = nil
        }
        startIndexing()
    }

    /// The open session was closed (or forgotten): stop indexing it and drop its results.
    func sessionClosed() {
        indexTask?.cancel()
        indexTask = nil
        indexing = nil
        indexedSession = nil
        indexRun += 1
        clear()
        dismissKeywordSuggestions()
    }

    /// Sessions ▸ Remove from List: the session leaves "search all sessions" too
    /// (no "offline" group for a folder the user dropped).
    func sessionForgotten(_ session: SessionInfo) async {
        guard let backend else { return }
        // A cancelled run of this session still writing would register it again after the forget.
        for run in liveRuns.values where run.session == session.id { await run.task.value }
        // Best effort: the list entry is already gone; at worst its group shows until the next index write.
        try? await backend.forget(session)
        // Resolving symlinks touches the disk: off the main actor.
        let roots = sessionResults.map(\.root)
        let gone = await Task.detached(priority: .userInitiated) {
            let target = session.url.standardizedFileURL.resolvingSymlinksInPath()
            return Set(roots.filter { $0.standardizedFileURL.resolvingSymlinksInPath() == target })
        }.value
        sessionResults.removeAll { gone.contains($0.root) }
    }

    /// Index the open session in the background unless it is already running.
    public func startIndexing() {
        guard let backend, let models, models.isReady, let session = app?.currentSession else { return }
        if indexTask != nil, indexedSession == session.id { return }
        indexTask?.cancel()
        indexedSession = session.id
        indexRun += 1
        let run = indexRun
        let task = Task { [weak self] in
            guard let self else { return }
            do {
                let encoder = try await self.readyEncoder()
                let indexer = self.makeIndexer(backend, encoder)
                let pass = Task.detached(priority: .background) {
                    try await indexer.run(session) { p in
                        Task { @MainActor [weak self] in if self?.indexRun == run { self?.indexing = p } }
                    }
                }
                // Cancelling this task (another session opened) must stop the detached pass.
                let report = try await withTaskCancellationHandler { try await pass.value } onCancel: { pass.cancel() }
                self.lastIndexReport = report
            } catch is CancellationError {
                // Another session opened (or this one closed).
            } catch {
                // The next open tries again.
                AppLog.search.error("indexing stopped: \(error.localizedDescription, privacy: .private(mask: .hash))")
            }
            if self.indexRun == run {
                self.indexing = nil
                self.indexTask = nil
            }
            self.liveRuns[run] = nil
        }
        indexTask = task
        liveRuns[run] = (session.id, task)
    }

    /// Wait for the running indexing pass (tests).
    func waitForIndexing() async {
        await indexTask?.value
    }

    /// Wait until no indexing run is left, cancelled ones included (tests).
    func waitForAllIndexingRuns() async {
        while let run = liveRuns.values.first { await run.task.value }
    }

    /// Indexing runs started so far (tests).
    var indexRunsStarted: Int { indexRun }

    // MARK: - Searching

    /// Run `query` in the current scope.
    public func search() async {
        let text = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return clear() }
        guard let backend, let models else { return }
        guard models.isReady || encoder != nil else {
            phase = .needsModel
            return
        }
        ticket += 1
        let mine = ticket
        let scope = scope
        let sessionID = app?.currentSession?.id
        phase = .searching
        similarTo = nil
        do {
            let encoder = try await readyEncoder()
            let vector = try await Task.detached(priority: .userInitiated) {
                try encoder.textEmbedding(Self.prompt(text))
            }.value
            switch scope {
            case .session:
                guard let app, let session = app.currentSession, session.id == sessionID else { return }
                // The window applies after the browser's full filter (in `show`): ask Rust for
                // everything above the floor that passes the criteria it knows.
                let hits = try await backend.search(vector, model: encoder.modelID, filter: app.filter,
                                                    in: session, tuning: wide(tuning))
                guard mine == ticket, app.currentSession?.id == sessionID else { return }
                sessionResults = []
                lastRequest = .text(text)
                show(hits, tuning: tuning)
            case .allSessions:
                let groups = try await backend.searchAll(vector, model: encoder.modelID, tuning: tuning)
                guard mine == ticket else { return }
                app?.searchRanking = nil
                lastRequest = nil
                sessionResults = groups
                phase = .results(groups.reduce(0) { $0 + $1.hits.count })
            }
        } catch is CancellationError {
            if mine == ticket { phase = .idle }
        } catch {
            if mine == ticket { phase = .failed((error as? LocalizedError)?.errorDescription ?? error.localizedDescription) }
        }
    }

    /// `t` without its window or limit: both apply after the browser's full filter
    /// (not-rejected, missing, filename text, smart folders are Swift-only), so Rust
    /// returns every match above the floor (a few bytes each).
    private func wide(_ t: SearchTuning) -> SearchTuning {
        SearchTuning(limit: Int(UInt32.max), minScore: t.minScore, window: 2)
    }

    /// The browser's filter changed while results are shown: rank again under it.
    func filterChanged() {
        guard app?.searchRanking != nil, let request = lastRequest else { return }
        switch request {
        case .text: queryChanged(debounce: .milliseconds(150))
        case .similar(let id): startSimilar(id)
        }
    }

    /// Search as the user types (debounced).
    public func queryChanged(debounce: Duration = .milliseconds(350)) {
        searchTask?.cancel()
        searchTask = Task { [weak self] in
            try? await Task.sleep(for: debounce)
            guard !Task.isCancelled else { return }
            await self?.search()
        }
    }

    /// Search now (Return in the field, a scope change): through the same task as typing.
    public func submit() {
        queryChanged(debounce: .zero)
    }

    /// Photos that look like `id` (image-to-image), in this session.
    public func findSimilar(_ id: PhotoID) async {
        await startSimilar(id).value
    }

    /// Find Similar through the same task as typing: it replaces a pending search.
    @discardableResult
    private func startSimilar(_ id: PhotoID) -> Task<Void, Never> {
        searchTask?.cancel()
        let task = Task { [weak self] in
            guard let self else { return }
            await self.similar(id)
        }
        searchTask = task
        return task
    }

    private func similar(_ id: PhotoID) async {
        guard let backend, let app, let session = app.currentSession else { return }
        let model = encoder?.modelID ?? models?.descriptor.id ?? ClipModelDescriptor.mobileCLIPS2.id
        ticket += 1
        let mine = ticket
        phase = .searching
        do {
            let hits = try await backend.similar(to: id, model: model, filter: app.filter, in: session,
                                                 tuning: wide(similarTuning))
            guard mine == ticket, app.currentSession?.id == session.id else { return }
            similarTo = id // before emptying the field: the field keeps these results
            query = ""
            sessionResults = []
            lastRequest = .similar(id)
            // The photo itself first, then what looks like it.
            show([SearchMatch(id: id, score: 1)] + hits, tuning: similarTuning, keep: id)
            if hits.isEmpty { app.show(Toast("This photo isn't indexed for search yet.", symbol: "magnifyingglass")) }
        } catch is CancellationError {
            if mine == ticket { phase = .idle }
        } catch {
            if mine == ticket { phase = .failed((error as? LocalizedError)?.errorDescription ?? error.localizedDescription) }
        }
    }

    /// Show `hits` in the browser: those passing its full filter, within the
    /// window of the best of them, at most `limit` (`keep` always stays).
    private func show(_ hits: [SearchMatch], tuning: SearchTuning, keep: PhotoID? = nil) {
        guard let app else { return }
        let visible = hits.filter { h in h.id == keep || app.photo(h.id).map(app.filter.matches) == true }
        let best = visible.first { $0.id != keep }?.score ?? 0
        let kept = visible.filter { $0.id == keep || $0.score >= best - tuning.window }
            .prefix(tuning.limit + (keep == nil ? 0 : 1))
        let ranking = SearchRanking(Array(kept))
        app.searchRanking = ranking
        phase = .results(ranking.ids.count)
    }

    /// Back to the whole session.
    public func clear() {
        ticket += 1
        lastRequest = nil
        searchTask?.cancel()
        app?.searchRanking = nil
        sessionResults = []
        similarTo = nil
        phase = .idle
    }

    /// "Open session" on an all-sessions result: open it and show `photo` (or its best hit).
    public func open(_ group: SessionMatches, photo: URL? = nil) async {
        guard let app else { return }
        let wanted = sessionResults
        await app.openFolder(group.root)
        sessionResults = wanted // opening cleared them; keep the list to come back to
        // Match by name first (cheap), then confirm the full path (symlinks touch the disk: off the main actor).
        guard let photo = photo ?? group.hits.first?.url else { return }
        let name = photo.lastPathComponent
        let candidates = app.visibleIDs.compactMap { app.photo($0) }
            .filter { $0.filename == name }.compactMap { p in p.url.map { (p.id, $0) } }
        let hit = await Task.detached(priority: .userInitiated) {
            let target = photo.standardizedFileURL.resolvingSymlinksInPath()
            return candidates.first { $0.1.standardizedFileURL.resolvingSymlinksInPath() == target }?.0
        }.value
        if let hit { app.focus(hit, extend: false) }
    }

    // MARK: - Keywords

    /// Everyday subjects offered as keyword suggestions (zero-shot).
    public static let keywordVocabulary = [
        "beach", "ocean", "waterfall", "river", "lake", "mountain", "forest", "desert", "snow", "sky", "clouds",
        "sunset", "sunrise", "night", "city", "street", "building", "bridge", "car", "boat", "airplane", "bicycle",
        "train", "people", "portrait", "child", "family", "crowd", "dog", "cat", "bird", "horse", "wildlife",
        "flower", "tree", "food", "party", "wedding", "sports", "concert", "interior", "landscape", "cliffs",
        "swimming", "hiking",
    ]

    @ObservationIgnored private var vocabularyVectors: [[Float]]?

    /// Suggest Keywords… results, for the photo they were made for.
    public struct KeywordSuggestions: Equatable, Sendable {
        public let photo: PhotoID
        public let words: [String]
    }

    public private(set) var keywordSuggestions: KeywordSuggestions?
    @ObservationIgnored private var keywordTicket = 0

    /// Suggest keywords for `id`; shown only if it is still the focused photo and no
    /// newer request was made.
    public func showKeywordSuggestions(for id: PhotoID) async {
        keywordTicket += 1
        let mine = keywordTicket
        let words = await suggestKeywords(for: id)
        guard mine == keywordTicket, app?.focusedID == id else { return }
        keywordSuggestions = KeywordSuggestions(photo: id, words: words)
    }

    /// Add a suggested keyword to the photo it was suggested for (not whatever is
    /// selected now).
    public func addSuggestedKeyword(_ keyword: String) {
        guard let s = keywordSuggestions, let app, app.photo(s.photo) != nil else { return }
        let new = Keywords.parse(keyword)
        app.updatePhotos([s.photo], label: "Add Keywords") { $0.keywords = Keywords.merge($0.keywords, adding: new) }
    }

    public func dismissKeywordSuggestions() {
        keywordTicket += 1
        keywordSuggestions = nil
    }

    /// Keyword suggestions for a photo (needs the model and the photo indexed).
    public func suggestKeywords(for id: PhotoID, top: Int = 5) async -> [String] {
        guard let backend, let session = app?.currentSession, let encoder = try? await readyEncoder() else { return [] }
        if vocabularyVectors == nil {
            let labels = Self.keywordVocabulary
            vocabularyVectors = try? await Task.detached(priority: .userInitiated) {
                try labels.map { try encoder.textEmbedding(Self.prompt($0)) }
            }.value
        }
        guard let vectors = vocabularyVectors else { return [] }
        return (try? await backend.keywords(for: id, labels: Self.keywordVocabulary, vectors: vectors, top: top,
                                            minProbability: 0.08, model: encoder.modelID, in: session)) ?? []
    }
}
