import CoreGraphics
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Natural-language search end to end on the real stack: a folder of A9 samples
/// opened in place (ds-session through the FFI), the global index, and — when
/// the model files are on this Mac (`DS_CLIP_MODEL_DIR`: the two MobileCLIP-S2
/// `.mlpackage`s and `merges.txt`) — the real encoders.
@MainActor
@Suite(.serialized) struct SearchLiveTests {
    struct Live {
        let dir: URL
        let folder: URL
        let model: AppModel
        let store: RustSessionStore
        let backend: RustSearchBackend
        let photos: [String: PhotoID]
    }

    /// The samples copied (APFS clones) into a temp folder and opened in place.
    static func open(_ samples: [URL], models: ClipModelStore? = nil, name: String = "Iceland",
                     prepare: (AppModel) -> Void = { _ in }) async throws -> Live {
        let fm = FileManager.default
        let dir = fm.temporaryDirectory.appendingPathComponent("ds-search-\(UUID())", isDirectory: true)
        let folder = dir.appendingPathComponent(name, isDirectory: true)
        try fm.createDirectory(at: folder, withIntermediateDirectories: true)
        for s in samples { try fm.copyItem(at: s, to: folder.appendingPathComponent(s.lastPathComponent)) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()),
                                     storeBase: dir.appendingPathComponent("InPlace", isDirectory: true))
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        let backend = RustSearchBackend(store: store, globalIndex: dir.appendingPathComponent("global.db"))
        m.search.configure(backend: backend, models: models ?? ClipModelStore(root: dir.appendingPathComponent("Models")))
        prepare(m)
        await m.openFolder(folder)
        let ids = Dictionary(m.photos.map { ($0.filename, $0.id) }, uniquingKeysWith: { a, _ in a })
        return Live(dir: dir, folder: folder, model: m, store: store, backend: backend, photos: ids)
    }

    static var modelDir: URL? {
        guard let env = ProcessInfo.processInfo.environment["DS_CLIP_MODEL_DIR"] else { return nil }
        let u = URL(fileURLWithPath: env)
        let need = ["mobileclip_s2_image.mlpackage/Manifest.json", "mobileclip_s2_text.mlpackage/Manifest.json", "merges.txt"]
        return need.allSatisfy { FileManager.default.fileExists(atPath: u.appendingPathComponent($0).path) } ? u : nil
    }

    static let waterfalls: Set<String> = ["SKB06204.ARW", "SKB06368.ARW", "SKB06426.ARW"]
    /// Every shot with the sea in it (the cliff shots have it along the bottom).
    static let withSea: Set<String> = ["SKB06577.ARW", "SKB07062.ARW", "SKB07132.ARW", "SKB08229.ARW", "SKB08351.ARW"]

    func names(_ m: AppModel, _ n: Int) -> [String] {
        m.visibleIDs.prefix(n).compactMap { m.photo($0)?.filename }
    }

    /// Orientation as the "embedding": portrait vs landscape previews.
    struct ShapeEncoder: SearchEncoding {
        var modelID = "shape-test"
        func imageEmbeddings(_ images: [CGImage]) throws -> [[Float]] {
            images.map { $0.height > $0.width ? [0.05, 1, 0, 0] : [1, 0.05, 0, 0] }
        }
        func textEmbedding(_ text: String) throws -> [Float] { text.contains("portrait") ? [0, 1, 0, 0] : [1, 0, 0, 0] }
    }

    /// The FFI backend on real files (no model needed): index through the real
    /// preview path, search with the filters, find similar, all sessions, keywords.
    @Test func rustBackendIndexesSearchesAndSharesAcrossSessions() async throws {
        let samples = sampleARWs()
        guard samples.count >= 4 else {
            print("SKIP: rustBackendIndexesSearchesAndSharesAcrossSessions — no A9 samples")
            return
        }
        let live = try await Self.open(Array(samples.prefix(4)), models: try installedModels()) { m in
            m.search.makeEncoder = { _ in ShapeEncoder() }
            // Photos are hashed in the background after the open; the indexer waits for them.
            m.search.makeIndexer = { b, e in
                var i = SearchIndexer(backend: b, encoder: e, isUserActive: { false })
                i.waitingPoll = .milliseconds(50)
                return i
            }
        }
        defer { try? FileManager.default.removeItem(at: live.dir) }
        let m = live.model
        let session = try #require(m.currentSession)
        m.search.startIndexing()
        await m.search.waitForIndexing()
        let status = try await live.backend.status(model: "shape-test", in: session)
        #expect(status == SearchIndexStatus(embedded: 4, total: 4, waiting: 0))
        #expect(try await live.backend.jobs(model: "shape-test", in: session).isEmpty)

        let portraits = Set(m.photos.filter { $0.meta.pixelHeight > $0.meta.pixelWidth }.map(\.id))
        let tuning = SearchTuning(limit: 10, minScore: 0.5, window: 1)
        let hits = try await live.backend.search([0, 1, 0, 0], model: "shape-test", filter: PhotoFilter(), in: session,
                                                 tuning: tuning)
        #expect(Set(hits.map(\.id)) == portraits, "\(hits)")
        // With a filter: rate one portrait and ask for 3 stars and up.
        let rated = try #require(portraits.first)
        m.focus(rated, extend: false)
        m.setRating(3)
        await m.saver.flush()
        let filtered = try await live.backend.search([0, 1, 0, 0], model: "shape-test",
                                                     filter: PhotoFilter(minRating: 3), in: session, tuning: tuning)
        #expect(filtered.map(\.id) == [rated])
        let similar = try await live.backend.similar(to: rated, model: "shape-test", filter: PhotoFilter(), in: session,
                                                     tuning: tuning)
        #expect(Set(similar.map(\.id)) == portraits.subtracting([rated]))
        let groups = try await live.backend.searchAll([0, 1, 0, 0], model: "shape-test", tuning: tuning)
        #expect(groups.count == 1)
        #expect(groups.first?.name == "Iceland" && groups.first?.available == true)
        #expect(groups.first?.root.resolvingSymlinksInPath() == live.folder.resolvingSymlinksInPath())
        #expect(groups.first?.hits.count == portraits.count)
        let kw = try await live.backend.keywords(for: rated, labels: ["portrait", "landscape"],
                                                 vectors: [[0, 1, 0, 0], [1, 0, 0, 0]], top: 1, minProbability: 0,
                                                 model: "shape-test", in: session)
        #expect(kw == ["portrait"])
        let stored = try await live.backend.store([SearchEmbedding(key: String(repeating: "ab", count: 32), vector: [1, 0, 0, 0])],
                                                  model: "shape-test", in: session)
        #expect(stored == nil, "the global index took it")

        // Through the app: "Open Session" from all-sessions results.
        m.search.scope = .allSessions
        m.search.query = "portrait"
        await m.search.search()
        #expect(m.search.sessionResults.count == 1)
        let g = try #require(m.search.sessionResults.first)
        await m.search.open(g, photo: g.hits.first?.url)
        #expect(m.focusedPhoto?.url?.lastPathComponent == g.hits.first?.url.lastPathComponent)

        // Sessions ▸ Remove from List: it leaves All Sessions results (no offline group).
        let info = try #require(m.sessionList.first { $0.url.lastPathComponent == "Iceland" })
        // Opening it again above started another indexing pass, whose jobs sync the session
        // into the global index: let it finish first, as `sessionForgotten` does.
        await m.search.waitForAllIndexingRuns()
        try await live.backend.forget(info)
        #expect(try await live.backend.searchAll([0, 1, 0, 0], model: "shape-test", tuning: tuning).isEmpty)

        // A session that isn't open can't be searched.
        let other = SessionInfo(id: UUID(), name: "Closed", url: live.dir, photoCount: 0, createdAt: Date())
        await #expect(throws: SearchBackendError.sessionNotOpen) {
            try await live.backend.jobs(model: "shape-test", in: other)
        }
    }

    /// The real MobileCLIP-S2 on the A9 samples: install from a local copy (checksums,
    /// compile), index the session in the background, then rank.
    @Test func realModelRanksWaterfallsAndTheSea() async throws {
        let samples = sampleARWs()
        guard let local = Self.modelDir, samples.count >= 8 else {
            print("SKIP: realModelRanksWaterfallsAndTheSea — set DS_CLIP_MODEL_DIR and provide the 8 A9 samples")
            return
        }
        let fm = FileManager.default
        let models = ClipModelStore(root: fm.temporaryDirectory.appendingPathComponent("ds-clip-\(UUID())"),
                                    downloader: DirectoryModelDownloader(root: local))
        defer { models.discard() }
        let live = try await Self.open(samples, models: models)
        defer { try? fm.removeItem(at: live.dir) }
        let m = live.model
        let clock = ContinuousClock()
        let t0 = clock.now
        await m.search.downloadModel()
        #expect(models.state == .ready)
        print("SEARCHPERF install (copy, verify, compile): \(clock.now - t0)")
        let t1 = clock.now
        await m.search.waitForIndexing()
        print("SEARCHPERF index 8 photos (incl. waiting for the hash pass): \(clock.now - t1)")
        let session = try #require(m.currentSession)
        let status = try await live.backend.status(model: "mobileclip-s2", in: session)
        #expect(status == SearchIndexStatus(embedded: 8, total: 8, waiting: 0))

        let probe = try m.search.encoderForTests()
        for q in ["waterfall", "ocean", "waterfall with people", "kids on the beach at sunset", "red car", "mountains"] {
            let encoder = probe
            let v = try encoder.textEmbedding(PhotoSearch.prompt(q))
            let all = try await live.backend.search(v, model: encoder.modelID, filter: PhotoFilter(), in: session,
                                                    tuning: SearchTuning(limit: 100, minScore: -1, window: 2))
            let line = all.map { h in "\(m.photo(h.id)!.filename.prefix(8)) \(String(format: "%.3f", h.score))" }
            print("SEARCHRANK \(q): \(line.joined(separator: ", "))")
        }

        let wf = try #require(live.photos["SKB06204.ARW"])
        let encoder = try m.search.encoderForTests()
        let similar = try await live.backend.similar(to: wf, model: encoder.modelID, filter: PhotoFilter(), in: session,
                                                     tuning: SearchTuning(limit: 100, minScore: -1, window: 2))
        print("SEARCHRANK similar to SKB06204: " + similar.map { "\(m.photo($0.id)!.filename.prefix(8)) \(String(format: "%.3f", $0.score))" }.joined(separator: ", "))

        // Indexing throughput as the indexer runs it (embedded preview → 384 px
        // decode → image encoder in batches of 16, 4 decodes at a time), 96 photos.
        let bulk = FakeSearchBackend()
        let files = try FileManager.default.contentsOfDirectory(at: live.folder, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension == "ARW" }
        bulk.state.withLock { s in s.jobs = (0..<96).map { SearchJob(key: "k\($0)", url: files[$0 % files.count]) } }
        let indexer = SearchIndexer(backend: bulk, encoder: encoder, isUserActive: { false })
        let t2 = clock.now
        let bulkReport = try await Task.detached(priority: .utility) { try await indexer.run(session) }.value
        let seconds = Double((clock.now - t2).components.attoseconds) / 1e18 + Double((clock.now - t2).components.seconds)
        let rate = Double(bulkReport.stored) / seconds
        print(String(format: "SEARCHPERF indexing throughput: %.1f photos/s (%d in %.2f s)", rate, bulkReport.stored, seconds))
        #expect(bulkReport.stored == 96)
        #expect(rate >= 15, "budget ≥ 30/s on an idle M4 Pro; this floor only catches regressions under load")

        // Through the search field: only matches, best first.
        m.search.query = "waterfall"
        let t3 = clock.now
        await m.search.search()
        print("SEARCHPERF query (text encoder + rank, 8 photos): \(clock.now - t3)")
        #expect(Set(names(m, 8)) == Self.waterfalls, "waterfall: \(names(m, 8))")
        #expect(m.search.phase == .results(3))
        m.search.query = "ocean"
        await m.search.search()
        let ocean = names(m, 8)
        #expect(ocean.first == "SKB07062.ARW", "ocean: \(ocean)")
        #expect(!ocean.isEmpty && Set(ocean).isSubset(of: Self.withSea), "ocean: \(ocean)")
        m.search.query = "red car"
        await m.search.search()
        #expect(m.visibleIDs.isEmpty && m.search.phase == .results(0), "nothing like it here: \(names(m, 8))")

        // Find similar: the photo itself, then the other waterfalls.
        await m.search.findSimilar(wf)
        #expect(names(m, 1) == ["SKB06204.ARW"])
        #expect(Set(names(m, 3).dropFirst()).isSubset(of: Self.waterfalls), "similar: \(names(m, 8))")
        // Keyword suggestions (zero-shot).
        m.focus(wf, extend: false)
        let kw = await m.search.suggestKeywords(for: wf)
        print("SEARCHRANK keywords for SKB06204: \(kw)")
        #expect(kw.contains("waterfall"), "\(kw)")
    }
}

extension PhotoSearch {
    /// The loaded encoder (tests).
    func encoderForTests() throws -> any SearchEncoding {
        guard let models, models.isReady else { throw ClipModelError.load("not installed") }
        return try makeEncoder(models.files)
    }
}
