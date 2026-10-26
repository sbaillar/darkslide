import CoreGraphics
import CoreML
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

final class SearchLocked<T>: @unchecked Sendable {
    private var value: T
    private let lock = NSLock()
    init(_ v: T) { value = v }
    func withLock<R>(_ f: (inout T) throws -> R) rethrows -> R { try lock.withLock { try f(&value) } }
    var get: T { lock.withLock { value } }
}

/// Deterministic encoder: a text or image maps to a unit-ish vector by keywords.
struct FakeEncoder: SearchEncoding {
    var modelID = "fake-clip"
    var failImages = false

    static func vector(_ text: String) -> [Float] {
        var v = [Float](repeating: 0.01, count: 8)
        for (i, w) in ["waterfall", "sea", "car", "beach"].enumerated() where text.lowercased().contains(w) { v[i] = 1 }
        return v
    }

    func imageEmbeddings(_ images: [CGImage]) throws -> [[Float]] {
        if failImages { throw ClipModelError.load("boom") }
        return images.map { img in Self.vector(img.width == 2 ? "waterfall" : img.width == 3 ? "sea" : "other") }
    }

    func textEmbedding(_ text: String) throws -> [Float] { Self.vector(text) }
}

func tinyImage(width: Int) -> CGImage {
    let ctx = CGContext(data: nil, width: width, height: 1, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    return ctx.makeImage()!
}

/// In-memory `SearchBackend`.
final class FakeSearchBackend: SearchBackend, @unchecked Sendable {
    struct State {
        var jobs: [SearchJob] = []
        var stored: [SearchEmbedding] = []
        var waiting = 0
        var waitingAfterPolls = 0
        var polls = 0
        var results: [SearchMatch] = []
        var similar: [SearchMatch] = []
        var groups: [SessionMatches] = []
        var keywords: [String] = []
        var lastFilter: PhotoFilter?
        var lastQuery: [Float]?
        var lastTuning: SearchTuning?
        var fail: Error?
        var globalError: String?
        var delay: Duration?
        var forgotten: [URL] = []
        /// `jobs` waits (ignoring cancellation) until `release()`.
        var hold = false
        var held: [CheckedContinuation<Void, Never>] = []
    }

    let state = SearchLocked(State())

    func release() {
        let held = state.withLock { s in
            s.hold = false
            defer { s.held = [] }
            return s.held
        }
        held.forEach { $0.resume() }
    }

    func jobs(model: String, in session: SessionInfo) async throws -> [SearchJob] {
        if state.get.hold {
            await withCheckedContinuation { c in
                let resume = state.withLock { s in
                    if s.hold { s.held.append(c) }
                    return !s.hold
                }
                if resume { c.resume() }
            }
        }
        return try state.withLock { s in
            if let e = s.fail { throw e }
            let stored = Set(s.stored.map(\.key))
            return s.jobs.filter { !stored.contains($0.key) }
        }
    }

    func store(_ embeddings: [SearchEmbedding], model: String, in session: SessionInfo) async throws -> String? {
        state.withLock { s in
            s.stored += embeddings
            return s.globalError
        }
    }

    func status(model: String, in session: SessionInfo) async throws -> SearchIndexStatus {
        state.withLock { s in
            s.polls += 1
            let waiting = s.polls > s.waitingAfterPolls ? 0 : s.waiting
            return SearchIndexStatus(embedded: s.stored.count, total: s.jobs.count, waiting: waiting)
        }
    }

    func search(_ query: [Float], model: String, filter: PhotoFilter, in session: SessionInfo,
                tuning: SearchTuning) async throws -> [SearchMatch] {
        if let d = state.get.delay { try? await Task.sleep(for: d) }
        return try state.withLock { s in
            if let e = s.fail { throw e }
            s.lastFilter = filter
            s.lastQuery = query
            s.lastTuning = tuning
            return s.results
        }
    }

    func similar(to id: PhotoID, model: String, filter: PhotoFilter, in session: SessionInfo,
                 tuning: SearchTuning) async throws -> [SearchMatch] {
        try state.withLock { s in
            if let e = s.fail { throw e }
            s.lastTuning = tuning
            return s.similar
        }
    }

    func searchAll(_ query: [Float], model: String, tuning: SearchTuning) async throws -> [SessionMatches] {
        state.withLock { $0.groups }
    }

    func forget(_ session: SessionInfo) async throws {
        state.withLock { $0.forgotten.append(session.url) }
    }

    func keywords(for id: PhotoID, labels: [String], vectors: [[Float]], top: Int, minProbability: Float,
                  model: String, in session: SessionInfo) async throws -> [String] {
        state.withLock { s in
            #expect(labels.count == vectors.count)
            return s.keywords
        }
    }
}

/// A model folder that looks installed (the fake encoder doesn't read it).
@MainActor
func installedModels() throws -> ClipModelStore {
    let root = FileManager.default.temporaryDirectory.appendingPathComponent("lt-clip-fake-\(UUID())")
    let store = ClipModelStore(root: root)
    let f = store.files
    try FileManager.default.createDirectory(at: f.imageModel, withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: f.textModel, withIntermediateDirectories: true)
    try Data("#version: 0.2\n".utf8).write(to: f.merges)
    return ClipModelStore(root: root)
}

@MainActor
@Suite(.serialized) struct SearchTests {
    func model(_ backend: FakeSearchBackend, models: ClipModelStore? = nil,
               encoder: @escaping @Sendable (ClipModelFiles) throws -> any SearchEncoding = { _ in FakeEncoder() })
        async throws -> AppModel {
        let m = makeModel(counts: [12, 4])
        m.search.configure(backend: backend, models: try models ?? installedModels())
        m.search.makeEncoder = encoder
        m.search.makeIndexer = { b, e in
            var i = SearchIndexer(backend: b, encoder: e, preview: { _ in tinyImage(width: 2) }, isUserActive: { false })
            i.waitingPoll = .milliseconds(1)
            // Runs here end when the test says (`waitingAfterPolls`), not after unchanged polls.
            i.maxUnchangedPolls = .max
            return i
        }
        await m.start()
        return m
    }

    @Test func rankingOrdersAndDedupes() {
        let r = SearchRanking([SearchMatch(id: PhotoID(3), score: 0.9), SearchMatch(id: PhotoID(1), score: 0.5),
                               SearchMatch(id: PhotoID(3), score: 0.1)])
        #expect(r.ids == [PhotoID(3), PhotoID(1)])
        #expect(r.scores[PhotoID(3)] == 0.9)
        #expect(r.contains(PhotoID(1)) && !r.contains(PhotoID(2)))
        #expect(r.sorted([PhotoID(1), PhotoID(2), PhotoID(3), PhotoID(4)]) == [PhotoID(3), PhotoID(1), PhotoID(2), PhotoID(4)])
    }

    @Test func searchShowsOnlyMatchesBestFirstAndCombinesWithTheFilter() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        let ids = m.photos.map(\.id)
        #expect(ids.count == 12)
        backend.state.withLock { $0.results = [SearchMatch(id: ids[5], score: 0.3), SearchMatch(id: ids[2], score: 0.25),
                                               SearchMatch(id: ids[9], score: 0.24)] }
        m.search.query = "waterfall"
        await m.search.search()
        #expect(m.visibleIDs == [ids[5], ids[2], ids[9]], "only matches, by relevance")
        #expect(m.search.phase == .results(3) && m.search.isActive)
        #expect(backend.state.get.lastQuery == FakeEncoder.vector(PhotoSearch.prompt("waterfall")))
        #expect(PhotoSearch.prompt("red car") == "a photo of red car")

        // The browser's filters still apply to the results (and are passed to the ranking).
        m.focus(ids[2], extend: false)
        m.setRating(4)
        m.filter.minRating = 3
        let results = [ids[5], ids[2], ids[9]]
        #expect(m.visibleIDs == results.filter { (m.photo($0)?.rating ?? 0) >= 3 } && m.visibleIDs.contains(ids[2]))
        m.filter.minRating = 0
        #expect(m.visibleIDs == [ids[5], ids[2], ids[9]])
        // A rating change on a photo outside the results never brings it in.
        m.focus(ids[2], extend: false)
        m.setRating(0)
        #expect(m.visibleIDs.count == 3)
        await m.search.search()
        #expect(backend.state.get.lastFilter == m.filter)

        m.search.query = ""
        m.search.queryChanged(debounce: .zero)
        m.search.clear()
        #expect(m.visibleIDs.count == 12 && m.search.phase == .idle && !m.search.isActive)
        await m.search.search()
        #expect(m.search.phase == .idle, "an empty query shows everything")
    }

    @Test func theWindowCountsOnlyVisiblePhotosAndFilterChangesRankAgain() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        let ids = m.photos.map(\.id)
        // The best match is rejected (hidden by "Not Rejected"): it must not set the window.
        m.focus(ids[0], extend: false)
        m.setPick(.rejected)
        for id in [ids[1], ids[2]] {
            m.focus(id, extend: false)
            m.setPick(.none)
        }
        m.filter.pick = .unrejected
        backend.state.withLock { $0.results = [SearchMatch(id: ids[0], score: 0.5), SearchMatch(id: ids[1], score: 0.3),
                                               SearchMatch(id: ids[2], score: 0.25)] }
        m.search.query = "sea"
        await m.search.search()
        #expect(m.visibleIDs == [ids[1], ids[2]], "window from the best visible match")
        // Loosening the filter ranks again (the hidden photo comes back first).
        m.filter.pick = .all
        #expect(await waitUntil { m.visibleIDs.first == ids[0] })
        #expect(m.visibleIDs == [ids[0]], "now 0.3 is outside the window of 0.5")
        // The limit counts what the filter shows, after the window.
        m.search.tuning = SearchTuning(limit: 2, minScore: 0, window: 1)
        m.search.submit()
        #expect(await waitUntil { m.visibleIDs == [ids[0], ids[1]] })
    }

    @Test func theEncoderLoadsOnceForEveryoneOffTheMainActor() async throws {
        let loads = SearchLocked(0)
        let m = try await model(FakeSearchBackend()) { _ in
            #expect(!Thread.isMainThread)
            loads.withLock { $0 += 1 }
            Thread.sleep(forTimeInterval: 0.05)
            return FakeEncoder()
        }
        async let a = m.search.readyEncoder()
        async let b = m.search.readyEncoder()
        _ = try await (a, b)
        _ = try await m.search.readyEncoder()
        #expect(loads.get == 1)
    }

    @Test func rustReturnsEveryMatchSoSwiftOnlyFiltersNeverLoseOne() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        let ids = m.photos.map(\.id)
        backend.state.withLock { $0.results = [SearchMatch(id: ids[0], score: 0.3)] }
        m.search.query = "sea"
        await m.search.search()
        let text = try #require(backend.state.get.lastTuning)
        #expect(text.limit >= Int(UInt32.max) && text.window >= 1 && text.minScore == m.search.tuning.minScore)
        await m.search.findSimilar(ids[1])
        let similar = try #require(backend.state.get.lastTuning)
        #expect(similar.limit >= Int(UInt32.max) && similar.minScore == m.search.similarTuning.minScore)
    }

    @Test func aCancelledFindSimilarGoesIdle() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        backend.state.withLock { $0.fail = CancellationError() }
        await m.search.findSimilar(try #require(m.photos.first?.id))
        #expect(m.search.phase == .idle)
    }

    @Test func closingTheOpenSessionStopsIndexingAndForgetsItsResults() async throws {
        let backend = FakeSearchBackend()
        backend.state.withLock { s in
            s.waiting = 1
            s.waitingAfterPolls = .max // indexing keeps going until stopped
            s.delay = .seconds(5)
        }
        let m = try await model(backend)
        let s = try #require(m.currentSession)
        m.search.query = "car"
        m.search.submit() // in flight (slow backend) when the session closes
        try await Task.sleep(for: .milliseconds(30))
        m.search.sessionClosed()
        #expect(m.search.phase == .idle && m.searchRanking == nil && m.search.indexing == nil)
        // The cancelled run ends by itself (a call in flight may still land first).
        await m.search.waitForAllIndexingRuns()
        let polls = backend.state.get.polls
        await m.search.waitForAllIndexingRuns()
        #expect(backend.state.get.polls == polls, "the indexer stopped")
        #expect(m.search.phase == .idle, "the stale search never lands")
        // Opening it again indexes again.
        backend.state.withLock { $0.waitingAfterPolls = 0 }
        m.search.sessionOpened(s)
        await m.search.waitForIndexing()
        #expect(backend.state.get.polls > polls)
    }

    @Test func forgettingASessionWaitsForItsCancelledRunToStopWriting() async throws {
        let backend = FakeSearchBackend()
        backend.state.withLock { $0.hold = true }
        let m = try await model(backend) // A's run is inside `jobs` (a Rust write, not cancellable)
        let a = try #require(m.currentSession)
        let b = try #require(m.sessionList.first { $0.id != a.id })
        await m.open(b) // A's run is cancelled but still writing
        let forget = Task { await m.search.sessionForgotten(a) }
        try await Task.sleep(for: .milliseconds(50))
        #expect(backend.state.get.forgotten.isEmpty, "not before A's write ends")
        backend.release()
        await forget.value
        #expect(backend.state.get.forgotten == [a.url])
        await m.search.waitForIndexing()
    }

    @Test func keywordSuggestionsBelongToThePhotoTheyWereMadeFor() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        let ids = m.photos.map(\.id)
        backend.state.withLock { $0.keywords = ["waterfall"] }
        m.focus(ids[0], extend: false)
        await m.search.showKeywordSuggestions(for: ids[0])
        #expect(m.search.keywordSuggestions == .init(photo: ids[0], words: ["waterfall"]))
        // The user moved on: the keyword still goes to the photo it was suggested for.
        m.focus(ids[1], extend: false)
        m.search.addSuggestedKeyword("waterfall")
        #expect(m.photo(ids[0])?.keywords == ["waterfall"] && m.photo(ids[1])?.keywords.isEmpty == true)
        // A reply for a photo that is no longer focused isn't shown.
        m.search.dismissKeywordSuggestions()
        m.focus(ids[2], extend: false)
        let late = Task { await m.search.showKeywordSuggestions(for: ids[2]) }
        m.focus(ids[3], extend: false)
        await late.value
        #expect(m.search.keywordSuggestions == nil)
        m.search.addSuggestedKeyword("forest") // nothing shown: nothing added
        #expect(m.photo(ids[3])?.keywords.isEmpty == true)
    }

    @Test func findSimilarReplacesAPendingTypedSearch() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        let ids = m.photos.map(\.id)
        backend.state.withLock { s in
            s.results = [SearchMatch(id: ids[3], score: 0.3)]
            s.similar = [SearchMatch(id: ids[4], score: 0.8)]
        }
        m.search.query = "car"
        m.search.queryChanged(debounce: .milliseconds(50))
        await m.search.findSimilar(ids[7])
        try await Task.sleep(for: .milliseconds(150))
        #expect(m.search.similarTo == ids[7] && m.visibleIDs == [ids[7], ids[4]], "the typed search never ran")
        #expect(m.search.query.isEmpty)
    }

    @Test func aReplyToAnOlderSearchIsDropped() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        let id = try #require(m.photos.first?.id)
        backend.state.withLock { s in
            s.results = [SearchMatch(id: id, score: 0.3)]
            s.delay = .milliseconds(100)
        }
        m.search.query = "car"
        let slow = Task { await m.search.search() }
        try await Task.sleep(for: .milliseconds(20))
        m.search.query = ""
        m.search.clear()
        await slow.value
        #expect(m.searchRanking == nil && m.visibleIDs.count == 12, "cleared while it ran: stays cleared")
        m.search.query = "car"
        m.search.submit()
        #expect(await waitUntil { m.visibleIDs == [id] })
    }

    @Test func removingASessionFromTheListForgetsItInSearch() async throws {
        let fm = FileManager.default
        let dir = fm.temporaryDirectory.appendingPathComponent("lt-search-forget-\(UUID())", isDirectory: true)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let recents = RecentFolders(defaults: testDefaults())
        recents.add(dir)
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: RustSessionStore(recents: recents), images: MockImageProvider(latency: .zero),
                         renderer: FakeRenderer(), importer: MockImportService(),
                         archive: UnavailableArchiveService(target: ""), ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        let backend = FakeSearchBackend()
        m.search.configure(backend: backend, models: try installedModels())
        await m.reloadSessions()
        let s = try #require(m.sessionList.first)
        await m.removeFromList(s)
        #expect(backend.state.get.forgotten == [s.url])
    }

    @Test func removingTheOpenSessionFromRecentsClosesAndForgetsItInSearch() async throws {
        let fm = FileManager.default
        let dir = fm.temporaryDirectory.appendingPathComponent("lt-search-forget-open-\(UUID())", isDirectory: true)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let recents = RecentFolders(defaults: testDefaults())
        recents.add(dir)
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: RustSessionStore(recents: recents), images: MockImageProvider(latency: .zero),
                         renderer: FakeRenderer(), importer: MockImportService(),
                         archive: UnavailableArchiveService(target: ""), ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        let backend = FakeSearchBackend()
        m.search.configure(backend: backend, models: try installedModels())
        m.search.makeEncoder = { _ in FakeEncoder() }
        await m.reloadSessions()
        let s = try #require(m.sessionList.first)
        await m.open(s)
        m.search.query = "car"
        await m.search.search()
        await m.forgetCurrentSession()
        #expect(m.currentSession == nil && backend.state.get.forgotten == [s.url])
        #expect(m.search.phase == .idle && !m.search.isActive && m.search.indexing == nil)
    }

    @Test func typingSearchesAfterAPause() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        let id = try #require(m.photos.first?.id)
        backend.state.withLock { $0.results = [SearchMatch(id: id, score: 0.3)] }
        m.search.query = "sea"
        m.search.queryChanged(debounce: .milliseconds(1))
        #expect(await waitUntil { m.visibleIDs == [id] })
    }

    @Test func allSessionsGroupsBySessionAndOpensOne() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        let g = SessionMatches(root: URL(fileURLWithPath: "/tmp/Sicily"), name: "Sicily", available: true, bestScore: 0.3,
                               hits: [.init(url: URL(fileURLWithPath: "/tmp/Sicily/a.ARW"), score: 0.3)])
        backend.state.withLock { $0.groups = [g] }
        m.search.scope = .allSessions
        m.search.query = "beach"
        await m.search.search()
        #expect(m.search.sessionResults == [g] && m.search.phase == .results(1))
        #expect(m.searchRanking == nil, "the browser keeps showing this session")
        #expect(g.id == g.root)
        // Demo mode can't open folders (it says so); the results stay to come back to.
        await m.search.open(g)
        #expect(m.search.sessionResults == [g])
        #expect(m.toast?.message.contains("live mode") == true)
    }

    @Test func findSimilarPutsThePhotoFirst() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        let ids = m.photos.map(\.id)
        backend.state.withLock { $0.similar = [SearchMatch(id: ids[4], score: 0.8)] }
        await m.search.findSimilar(ids[7])
        #expect(m.visibleIDs == [ids[7], ids[4]])
        #expect(m.search.similarTo == ids[7] && m.search.isActive)
        backend.state.withLock { $0.similar = [] }
        await m.search.findSimilar(ids[1])
        #expect(m.visibleIDs == [ids[1]])
        #expect(m.toast?.message.contains("isn't indexed") == true)
        backend.state.withLock { $0.fail = SearchBackendError.sessionNotOpen }
        await m.search.findSimilar(ids[1])
        #expect(m.search.phase == .failed("Open the session to search it."))
    }

    @Test func failuresAreShown() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        backend.state.withLock { $0.fail = SearchBackendError.sessionNotOpen }
        m.search.query = "car"
        await m.search.search()
        #expect(m.search.phase == .failed("Open the session to search it."))
        // An installed model that doesn't load is discarded (downloaded again next time).
        let broken = try await model(FakeSearchBackend(), encoder: { _ in throw ClipModelError.load("damaged") })
        await broken.search.waitForIndexing() // the open's indexing pass found it unusable
        #expect(broken.search.models?.state == .notDownloaded)
        broken.search.query = "car"
        await broken.search.search()
        #expect(broken.search.phase == .needsModel, "offers the download again")
    }

    @Test func aTransientLoadFailureKeepsAVerifiedModel() async throws {
        let fx = try ModelFixture()
        defer { try? FileManager.default.removeItem(at: fx.root) }
        let store = ClipModelStore(root: fx.root.appendingPathComponent("Models"), descriptor: fx.descriptor,
                                   downloader: DirectoryModelDownloader(root: fx.remote), compile: ModelFixture.compile)
        _ = try await store.prepare()
        let fails = SearchLocked(true)
        let m = try await model(FakeSearchBackend(), models: store) { _ in
            if fails.get { throw ClipModelError.load("ANE busy") }
            return FakeEncoder()
        }
        await m.search.waitForIndexing()
        #expect(store.isReady && FileManager.default.fileExists(atPath: store.folder.path), "kept")
        fails.withLock { $0 = false }
        _ = try await m.search.readyEncoder()
    }

    @Test func withoutTheModelTheSearchOffersTheDownload() async throws {
        let backend = FakeSearchBackend()
        let files = try ModelFixture()
        let models = ClipModelStore(root: files.root.appendingPathComponent("Models"), descriptor: files.descriptor,
                                    downloader: DirectoryModelDownloader(root: files.remote), compile: ModelFixture.compile)
        let m = try await model(backend, models: models)
        #expect(m.search.isAvailable)
        let id = try #require(m.photos.first?.id)
        backend.state.withLock { $0.results = [SearchMatch(id: id, score: 0.3)] }
        m.search.query = "waterfall"
        await m.search.search()
        #expect(m.search.phase == .needsModel)
        #expect(m.search.indexing == nil, "nothing indexed without the model")
        await m.search.downloadModel()
        #expect(models.state == .ready)
        #expect(m.visibleIDs == [id], "the pending query runs after the download")
        await m.search.waitForIndexing()
        m.search.cancelDownload()
    }

    @Test func aFailedDownloadIsReported() async throws {
        let files = try ModelFixture()
        var bad = files.descriptor
        bad.files[0].sha256 = String(repeating: "0", count: 64)
        let models = ClipModelStore(root: files.root.appendingPathComponent("Models"), descriptor: bad,
                                    downloader: DirectoryModelDownloader(root: files.remote), compile: ModelFixture.compile)
        let m = try await model(FakeSearchBackend(), models: models)
        await m.search.downloadModel()
        guard case .failed(let msg) = m.search.phase else { Issue.record("\(m.search.phase)"); return }
        #expect(msg.contains("damaged"))
    }

    @Test func openingASessionIndexesItInTheBackground() async throws {
        let backend = FakeSearchBackend()
        backend.state.withLock { s in
            s.jobs = (0..<5).map { SearchJob(key: "k\($0)", url: URL(fileURLWithPath: "/x/\($0).ARW")) }
        }
        let m = try await model(backend)
        await m.search.waitForIndexing()
        #expect(backend.state.get.stored.count == 5)
        #expect(m.search.lastIndexReport?.stored == 5)
        #expect(m.search.indexing == nil && m.search.indexingMessage == nil)
        // Opening the same session again finds nothing left to do.
        let s = try #require(m.currentSession)
        await m.open(s)
        await m.search.waitForIndexing()
        #expect(backend.state.get.stored.count == 5)
    }

    @Test func aCancelledIndexingRunNeverReplacesItsSuccessor() async throws {
        let backend = FakeSearchBackend()
        backend.state.withLock { s in
            s.hold = true
            s.waiting = 1
            s.waitingAfterPolls = .max // A keeps indexing until the end of the test
        }
        let m = try await model(backend) // its open started A's first run
        // Each run is held in `jobs` once it got there: wait for that, not for time.
        let held = { backend.state.get.held.count }
        #expect(await waitUntil { held() == 1 }, "A's first run (made before the counter below)")
        let runs = SearchLocked(0)
        let make = m.search.makeIndexer
        m.search.makeIndexer = { b, e in
            runs.withLock { $0 += 1 }
            var i = make(b, e)
            i.maxWaitingPoll = .milliseconds(10)
            return i
        }
        let a = try #require(m.currentSession)
        let b = try #require(m.sessionList.first { $0.id != a.id })
        await m.open(b)
        #expect(await waitUntil { held() == 2 }, "B's run")
        await m.open(a)
        #expect(await waitUntil { held() == 3 }, "the new A run")
        #expect(runs.get == 2)
        backend.release() // the cancelled first A run and B's end now, after the new A run started
        let started = m.search.indexRunsStarted
        await m.open(a) // a refresh: the A run still going is kept
        #expect(m.search.indexRunsStarted == started, "no second indexer for A")
        backend.state.withLock { $0.waitingAfterPolls = 0 }
        await m.search.waitForIndexing()
    }

    @Test func indexingMessageAndKeywords() async throws {
        let backend = FakeSearchBackend()
        let m = try await model(backend)
        #expect(m.search.indexingMessage == nil)
        backend.state.withLock { $0.keywords = ["waterfall", "forest"] }
        let id = try #require(m.photos.first?.id)
        #expect(await m.search.suggestKeywords(for: id) == ["waterfall", "forest"])
        #expect(await m.search.suggestKeywords(for: id) == ["waterfall", "forest"], "vocabulary embedded once")
        #expect(PhotoSearch.keywordVocabulary.contains("waterfall"))
        let none = PhotoSearch()
        #expect(!none.isAvailable)
        #expect(await none.suggestKeywords(for: id).isEmpty)
        none.query = "x"
        await none.search()
        await none.findSimilar(id)
        #expect(none.phase == .idle)
        none.startIndexing()
        await none.downloadModel()
        #expect(none.phase == .failed("Open the session to search it."))
    }

    @Test func filterBecomesARankingRecord() {
        var f = PhotoFilter(minRating: 3, colorTag: .red, pick: .picked)
        var r = f.searchRecord
        #expect(r.minRating == 3 && r.maxRating == nil && r.tags == [.red] && r.pick == .picked)
        f = PhotoFilter(pick: .rejected, unratedOnly: true)
        r = f.searchRecord
        #expect(r.minRating == nil && r.maxRating == 0 && r.pick == .rejected && r.tags.isEmpty)
        #expect(PhotoFilter(pick: .unflagged).searchRecord.pick == .unflagged)
        #expect(PhotoFilter(pick: .unrejected).searchRecord.pick == nil, "the browser applies it")
        #expect(SearchTuning(limit: 5, minScore: 0.2, window: 0.1).record.limit == 5)
        #expect(SearchBackendError.sessionNotOpen.errorDescription?.isEmpty == false)
    }
}

/// A fake two-package "model" served from a local folder, with a compile step
/// that just makes the `.mlmodelc` folder.
struct ModelFixture {
    let root: URL
    let remote: URL
    let descriptor: ClipModelDescriptor

    init() throws {
        let fm = FileManager.default
        root = fm.temporaryDirectory.appendingPathComponent("lt-clip-fixture-\(UUID())")
        remote = root.appendingPathComponent("remote")
        var files: [ClipModelDescriptor.File] = []
        for (path, body) in [("a_image.mlpackage/Manifest.json", "image"), ("a_text.mlpackage/Manifest.json", "text"),
                             ("merges.txt", "#version: 0.2\n")] {
            let src = remote.appendingPathComponent(path)
            try fm.createDirectory(at: src.deletingLastPathComponent(), withIntermediateDirectories: true)
            let data = Data(body.utf8)
            try data.write(to: src)
            let local = path.replacingOccurrences(of: "a_image", with: "image").replacingOccurrences(of: "a_text", with: "text")
            files.append(.init(path: local, url: URL(string: "https://example.invalid/x/resolve/abc/\(path)")!,
                               sha256: try ModelFiles.sha256(of: src), size: Int64(data.count)))
        }
        descriptor = ClipModelDescriptor(id: "fake-clip", name: "Fake", files: files, dimensions: 8)
    }

    @Sendable static func compile(_ pkg: URL) async throws -> URL {
        #expect(FileManager.default.fileExists(atPath: pkg.appendingPathComponent("Manifest.json").path))
        let out = pkg.deletingLastPathComponent().appendingPathComponent("\(UUID()).mlmodelc")
        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        return out
    }
}

@MainActor
@Suite(.serialized) struct ClipModelStoreTests {
    @Test func installsVerifiesAndIsFoundNextLaunch() async throws {
        let fx = try ModelFixture()
        defer { try? FileManager.default.removeItem(at: fx.root) }
        let root = fx.root.appendingPathComponent("Models")
        let store = ClipModelStore(root: root, descriptor: fx.descriptor, downloader: DirectoryModelDownloader(root: fx.remote),
                                   compile: ModelFixture.compile)
        #expect(store.state == .notDownloaded && !store.isReady)
        async let a = store.prepare()
        async let b = store.prepare()
        let (fa, fb) = try await (a, b)
        #expect(fa == fb && store.state == .ready)
        for u in fa.all { #expect(FileManager.default.fileExists(atPath: u.path)) }
        let staging = try FileManager.default.contentsOfDirectory(atPath: root.path).filter { $0.hasPrefix(".download") }
        #expect(staging.isEmpty)
        #expect(ClipModelStore(root: root, descriptor: fx.descriptor).state == .ready, "found on the next launch")
        _ = try await store.prepare()
        #expect(await store.verify(), "the install recorded what it wrote")
        try Data("#damaged\n".utf8).write(to: fa.merges)
        #expect(await !store.verify())
        store.discard()
        #expect(store.state == .notDownloaded && !FileManager.default.fileExists(atPath: store.folder.path))
        #expect(await !store.verify(), "nothing installed")
    }

    @Test func leftoverStagingFoldersAreSwept() async throws {
        let fx = try ModelFixture()
        defer { try? FileManager.default.removeItem(at: fx.root) }
        let root = fx.root.appendingPathComponent("Models")
        let leftover = root.appendingPathComponent(ClipModelStore.stagingPrefix + "crashed", isDirectory: true)
        // Another model's staging folder is never taken, however long a slow download has
        // left its top folder untouched (the sweep once removed any `.download-*` idle for an hour).
        let inFlight = root.appendingPathComponent(".download-another-model", isDirectory: true)
        let old = [FileAttributeKey.modificationDate: Date(timeIntervalSinceNow: -7200)]
        try FileManager.default.createDirectory(at: leftover, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: inFlight, withIntermediateDirectories: true)
        try FileManager.default.setAttributes(old, ofItemAtPath: inFlight.path)
        let store = ClipModelStore(root: root, descriptor: fx.descriptor, downloader: DirectoryModelDownloader(root: fx.remote),
                                   compile: ModelFixture.compile)
        _ = try await store.prepare()
        #expect(!FileManager.default.fileExists(atPath: leftover.path))
        #expect(FileManager.default.fileExists(atPath: inFlight.path), "another model's download in flight is kept")
        // Already installed (no download this launch): swept all the same.
        try FileManager.default.createDirectory(at: leftover, withIntermediateDirectories: true)
        let next = ClipModelStore(root: root, descriptor: fx.descriptor, downloader: DirectoryModelDownloader(root: fx.remote),
                                  compile: ModelFixture.compile)
        _ = try await next.prepare()
        #expect(!FileManager.default.fileExists(atPath: leftover.path) && next.isReady)
    }

    @Test func damagedMissingAndUncompilableDownloadsFail() async throws {
        let fx = try ModelFixture()
        defer { try? FileManager.default.removeItem(at: fx.root) }
        var bad = fx.descriptor
        bad.files[1].sha256 = String(repeating: "0", count: 64)
        let damaged = ClipModelStore(root: fx.root.appendingPathComponent("A"), descriptor: bad,
                                     downloader: DirectoryModelDownloader(root: fx.remote), compile: ModelFixture.compile)
        await #expect(throws: ClipModelError.checksum("text.mlpackage/Manifest.json")) { try await damaged.prepare() }
        guard case .failed(let m) = damaged.state else { Issue.record("\(damaged.state)"); return }
        #expect(m.contains("damaged"))

        let missing = ClipModelStore(root: fx.root.appendingPathComponent("B"), descriptor: fx.descriptor,
                                     downloader: DirectoryModelDownloader(root: fx.root.appendingPathComponent("nowhere")),
                                     compile: ModelFixture.compile)
        do {
            _ = try await missing.prepare()
            Issue.record("expected a download error")
        } catch let e as ClipModelError {
            if case .download = e {} else { Issue.record("\(e)") }
        }

        struct Boom: Error {}
        let noCompile = ClipModelStore(root: fx.root.appendingPathComponent("C"), descriptor: fx.descriptor,
                                       downloader: DirectoryModelDownloader(root: fx.remote), compile: { _ in throw Boom() })
        do {
            _ = try await noCompile.prepare()
            Issue.record("expected a compile error")
        } catch let e as ClipModelError {
            if case .compile = e {} else { Issue.record("\(e)") }
        }
        for e in [ClipModelError.download("x"), .checksum("y"), .compile("z"), .load("w")] {
            #expect(e.errorDescription?.contains("search model") == true)
        }
    }

    /// The URLSession downloader's HTTP errors and other failures become download errors.
    @Test func downloaderErrorsAreDownloadErrors() async throws {
        struct Failing: ModelDownloader {
            let error: Error
            func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
                throw error
            }
        }
        let fx = try ModelFixture()
        defer { try? FileManager.default.removeItem(at: fx.root) }
        for (i, (error, expected)) in [(SegmentationError.download("HTTP 404 for merges.txt"), "HTTP 404 for merges.txt"),
                                       (SegmentationError.compile("odd"), "odd")].enumerated() {
            let s = ClipModelStore(root: fx.root.appendingPathComponent("H\(i)"), descriptor: fx.descriptor,
                                   downloader: Failing(error: error), compile: ModelFixture.compile)
            do {
                _ = try await s.prepare()
                Issue.record("expected an error")
            } catch let e as ClipModelError {
                guard case .download(let m) = e else { Issue.record("\(e)"); continue }
                #expect(m.contains(expected))
            }
        }
        let generic = ClipModelStore(root: fx.root.appendingPathComponent("G"), descriptor: fx.descriptor,
                                     downloader: Failing(error: URLError(.notConnectedToInternet)), compile: ModelFixture.compile)
        await #expect(throws: ClipModelError.self) { try await generic.prepare() }
    }

    @Test func cancellingADownloadKeepsNothing() async throws {
        struct Slow: ModelDownloader {
            func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
                progress(1)
                try await Task.sleep(for: .seconds(30))
            }
        }
        let fx = try ModelFixture()
        defer { try? FileManager.default.removeItem(at: fx.root) }
        let root = fx.root.appendingPathComponent("Models")
        let s = ClipModelStore(root: root, descriptor: fx.descriptor, downloader: Slow(), compile: ModelFixture.compile)
        let t = Task { try await s.prepare() }
        #expect(await waitUntil { if case .downloading = s.state { true } else { false } })
        s.cancel()
        await #expect(throws: CancellationError.self) { try await t.value }
        #expect(s.state == .notDownloaded)
        let left = (try? FileManager.default.contentsOfDirectory(atPath: root.path)) ?? []
        #expect(left.isEmpty, "\(left)")
    }

    @Test func directoryDownloaderMapsThePinnedPath() async throws {
        let fx = try ModelFixture()
        defer { try? FileManager.default.removeItem(at: fx.root) }
        let d = DirectoryModelDownloader(root: fx.remote)
        let dst = fx.root.appendingPathComponent("m.txt")
        let got = SearchLocked<Int64>(0)
        try await d.download(URL(string: "https://h/x/resolve/abc/merges.txt")!, to: dst, progress: { b in got.withLock { $0 = b } })
        #expect(try String(contentsOf: dst, encoding: .utf8) == "#version: 0.2\n" && got.get == 14)
        await #expect(throws: ClipModelError.self) {
            try await d.download(URL(string: "https://h/x/merges.txt")!, to: dst, progress: { _ in })
        }
    }

    @Test func theRealDescriptorIsPinned() {
        let d = ClipModelDescriptor.mobileCLIPS2
        #expect(d.files.count == 7 && d.dimensions == 512 && d.id == "mobileclip-s2")
        #expect(d.totalBytes == 199_217_586)
        #expect(d.files.allSatisfy { $0.url.absoluteString.contains("/resolve/") && !$0.url.absoluteString.contains("/main/") })
        #expect(d.files.allSatisfy { $0.sha256.count == 64 })
    }

    @Test func encoderReportsAMissingModelAndReadsAnyFloatOutput() throws {
        let fx = try ModelFixture()
        defer { try? FileManager.default.removeItem(at: fx.root) }
        #expect(throws: ClipModelError.self) { try ClipEncoder(files: ClipModelFiles(root: fx.root)) }
        for type in [MLMultiArrayDataType.float32, .float16, .double] {
            let a = try MLMultiArray(shape: [1, 3], dataType: type)
            for i in 0..<3 { a[i] = NSNumber(value: Double(i) + 0.5) }
            #expect(ClipEncoder.floats(a) == [0.5, 1.5, 2.5], "\(type.rawValue)")
        }
        let p = try MLDictionaryFeatureProvider(dictionary: ["x": MLFeatureValue(double: 1)])
        #expect(throws: ClipModelError.self) { try ClipEncoder.vector(p) }
    }
}

@Suite struct SearchIndexerTests {
    func session() -> SessionInfo {
        SessionInfo(id: UUID(), name: "S", url: URL(fileURLWithPath: "/s"), photoCount: 0, createdAt: Date())
    }

    @Test func embedsInBatchesSkipsUnreadablePreviewsAndWaitsForHashing() async throws {
        let backend = FakeSearchBackend()
        backend.state.withLock { s in
            s.jobs = (0..<7).map { SearchJob(key: "k\($0)", url: URL(fileURLWithPath: "/x/\($0).ARW")) }
            s.waiting = 3
            s.waitingAfterPolls = 2
            s.globalError = "global index busy"
        }
        var indexer = SearchIndexer(backend: backend, encoder: FakeEncoder(), preview: { url in
            if url.lastPathComponent == "3.ARW" { throw ClipModelError.load("unreadable") }
            return tinyImage(width: url.lastPathComponent == "0.ARW" ? 2 : 3)
        }, isUserActive: { false })
        indexer.batchSize = 3
        indexer.concurrency = 2
        indexer.waitingPoll = .milliseconds(1)
        let seen = SearchLocked<[SearchIndexer.Progress]>([])
        let report = try await indexer.run(session()) { p in seen.withLock { $0.append(p) } }
        #expect(report.stored == 6 && report.failed == 1 && report.globalError == "global index busy")
        let stored = backend.state.get.stored
        #expect(Set(stored.map(\.key)) == Set(["k0", "k1", "k2", "k4", "k5", "k6"]))
        #expect(stored.first { $0.key == "k0" }?.vector == FakeEncoder.vector("waterfall"))
        #expect(seen.get.first == SearchIndexer.Progress(done: 0, total: 7))
        #expect(seen.get.contains(SearchIndexer.Progress(done: 7, total: 7)))
        #expect(backend.state.get.polls == 3, "looked again while photos were being hashed")
    }

    @Test func pausesWhileTheUserIsActiveAndStopsWhenCancelled() async throws {
        let backend = FakeSearchBackend()
        backend.state.withLock { $0.jobs = [SearchJob(key: "a", url: URL(fileURLWithPath: "/a.ARW"))] }
        let active = SearchLocked(true)
        var indexer = SearchIndexer(backend: backend, encoder: FakeEncoder(), preview: { _ in tinyImage(width: 2) },
                                    isUserActive: { active.get })
        indexer.idlePoll = .milliseconds(5)
        let runner = indexer
        let info = session()
        let task = Task { try await runner.run(info) }
        try await Task.sleep(for: .milliseconds(60))
        #expect(backend.state.get.stored.isEmpty, "waits while the user culls")
        active.withLock { $0 = false }
        let report = try await task.value
        #expect(report.stored == 1)

        backend.state.withLock { s in
            s.jobs.append(SearchJob(key: "b", url: URL(fileURLWithPath: "/b.ARW")))
        }
        active.withLock { $0 = true }
        let cancelled = Task { try await runner.run(info) }
        try await Task.sleep(for: .milliseconds(20))
        cancelled.cancel()
        await #expect(throws: CancellationError.self) { try await cancelled.value }
        #expect(backend.state.get.stored.count == 1)
    }

    @Test func photosThatStayUnhashedArePolledLessAndLess() async throws {
        let backend = FakeSearchBackend()
        backend.state.withLock { s in
            s.waiting = 2
            s.waitingAfterPolls = .max
        }
        var indexer = SearchIndexer(backend: backend, encoder: FakeEncoder(), isUserActive: { false })
        indexer.waitingPoll = .milliseconds(1)
        indexer.maxWaitingPoll = .seconds(1)
        indexer.maxUnchangedPolls = .max
        let runner = indexer
        let info = session()
        let task = Task { try await runner.run(info) }
        try await Task.sleep(for: .milliseconds(120))
        task.cancel()
        await #expect(throws: CancellationError.self) { try await task.value }
        // 1, 2, 4 … 64 ms: about 7 looks in 120 ms (not ~120).
        #expect(backend.state.get.polls <= 10)
    }

    @Test func photosThatNeverGetHashedEndTheRunAfterAWhile() async throws {
        let backend = FakeSearchBackend()
        backend.state.withLock { s in
            s.waiting = 2
            s.waitingAfterPolls = .max
        }
        var indexer = SearchIndexer(backend: backend, encoder: FakeEncoder(), isUserActive: { false })
        indexer.waitingPoll = .milliseconds(1)
        indexer.maxUnchangedPolls = 3
        let report = try await indexer.run(session())
        #expect(report.stored == 0 && backend.state.get.polls == 4, "the first look, then 3 unchanged")
    }

    @Test func encoderAndBackendFailuresEndTheRun() async throws {
        let backend = FakeSearchBackend()
        backend.state.withLock { $0.jobs = [SearchJob(key: "a", url: URL(fileURLWithPath: "/a.ARW"))] }
        let failing = SearchIndexer(backend: backend, encoder: FakeEncoder(failImages: true),
                                    preview: { _ in tinyImage(width: 2) }, isUserActive: { false })
        await #expect(throws: ClipModelError.self) { try await failing.run(session()) }
        backend.state.withLock { $0.fail = SearchBackendError.sessionNotOpen }
        await #expect(throws: SearchBackendError.self) { try await failing.run(session()) }
        #expect(SearchIndexer.previewEdge == 384)
        // The real preview path reports an unreadable file.
        await #expect(throws: (any Error).self) {
            try await SearchIndexer.embeddedPreview(URL(fileURLWithPath: "/nonexistent/x.ARW"))
        }
    }
}
