import DarkslideFFI
import Foundation

/// A photo to embed: its preview, stored under `key` (content identity).
public struct SearchJob: Sendable, Equatable {
    public var key: String
    public var url: URL
    public init(key: String, url: URL) {
        self.key = key
        self.url = url
    }
}

public struct SearchEmbedding: Sendable, Equatable {
    public var key: String
    public var vector: [Float]
    public init(key: String, vector: [Float]) {
        self.key = key
        self.vector = vector
    }
}

/// How far indexing got in a session.
public struct SearchIndexStatus: Sendable, Equatable {
    public var embedded: Int
    /// Photos that can be embedded now.
    public var total: Int
    /// Photos still being identified (hashed) in the background.
    public var waiting: Int
    public init(embedded: Int, total: Int, waiting: Int) {
        self.embedded = embedded
        self.total = total
        self.waiting = waiting
    }
}

/// Which results count (cosine similarity): at most `limit`, scoring at least
/// `minScore` and within `window` of the best.
public struct SearchTuning: Sendable, Equatable {
    public var limit: Int
    public var minScore: Float
    public var window: Float
    public init(limit: Int, minScore: Float, window: Float) {
        self.limit = limit
        self.minScore = minScore
        self.window = window
    }

    var record: SearchOptions { SearchOptions(limit: UInt32(clamping: limit), minScore: minScore, window: window) }
}

public struct SearchMatch: Sendable, Equatable {
    public var id: PhotoID
    public var score: Float
    public init(id: PhotoID, score: Float) {
        self.id = id
        self.score = score
    }
}

/// One session's results in "Search all sessions".
public struct SessionMatches: Sendable, Equatable, Identifiable {
    public struct Hit: Sendable, Equatable {
        public var url: URL
        public var score: Float
    }

    /// What to open: the session's folder.
    public var root: URL
    public var name: String
    public var available: Bool
    public var bestScore: Float
    public var hits: [Hit]
    public var id: URL { root }
}

/// The store side of search (seam for tests): vectors per session, ranking.
public protocol SearchBackend: Sendable {
    func jobs(model: String, in session: SessionInfo) async throws -> [SearchJob]
    /// Returns a message when the global index couldn't be updated (the session's search still works).
    func store(_ embeddings: [SearchEmbedding], model: String, in session: SessionInfo) async throws -> String?
    func status(model: String, in session: SessionInfo) async throws -> SearchIndexStatus
    func search(_ query: [Float], model: String, filter: PhotoFilter, in session: SessionInfo,
                tuning: SearchTuning) async throws -> [SearchMatch]
    func similar(to id: PhotoID, model: String, filter: PhotoFilter, in session: SessionInfo,
                 tuning: SearchTuning) async throws -> [SearchMatch]
    func searchAll(_ query: [Float], model: String, tuning: SearchTuning) async throws -> [SessionMatches]
    /// The session was removed from the app's list: it leaves "search all sessions".
    func forget(_ session: SessionInfo) async throws
    /// Zero-shot keywords: `labels` and their text embeddings, most probable first.
    func keywords(for id: PhotoID, labels: [String], vectors: [[Float]], top: Int, minProbability: Float,
                  model: String, in session: SessionInfo) async throws -> [String]
}

public enum SearchBackendError: Error, Equatable, LocalizedError {
    case sessionNotOpen

    public var errorDescription: String? { "Open the session to search it." }
}

extension PhotoFilter {
    /// The criteria Rust can apply while ranking (rating, label, flag); the
    /// browser applies the full filter to the results as well.
    var searchRecord: SmartFilterRecord {
        var r = SmartFilterRecord(minRating: nil, maxRating: nil, tags: [], pick: nil, text: nil, camera: nil, lens: nil,
                                  dateFromMs: nil, dateToMs: nil, isoMin: nil, isoMax: nil, edited: nil, keywords: [])
        if minRating > 0 { r.minRating = UInt8(clamping: minRating) }
        if unratedOnly { r.maxRating = 0 }
        if let colorTag { r.tags = [RustSessionStore.label(colorTag)] }
        switch pick {
        case .picked: r.pick = .picked
        case .unflagged: r.pick = .unflagged
        case .rejected: r.pick = .rejected
        case .all, .unrejected: break
        }
        return r
    }
}

/// `SearchBackend` over ds-session through the FFI: the open sessions of a
/// `RustSessionStore` and the global index (`<support>/global.db`).
public struct RustSearchBackend: SearchBackend {
    public let store: RustSessionStore
    public let globalIndex: URL

    public init(store: RustSessionStore, globalIndex: URL = RustImportService.defaultGlobalIndex) {
        self.store = store
        self.globalIndex = globalIndex
    }

    private func handle(_ session: SessionInfo) async throws -> SessionHandle {
        guard let h = await store.archiveHandle(for: session) else { throw SearchBackendError.sessionNotOpen }
        return h
    }

    public func jobs(model: String, in session: SessionInfo) async throws -> [SearchJob] {
        let h = try await handle(session)
        let global = globalIndex.path
        let list = try await RustSessionStore.call { token in
            try await h.embeddingJobs(model: model, globalIndex: global, token: token)
        }
        if let e = list.globalError {
            // This session still indexes; other sessions just don't see it until the global index recovers.
            AppLog.search.error("global search index unavailable: \(e, privacy: .private(mask: .hash))")
        }
        return list.jobs.map { SearchJob(key: $0.key, url: URL(fileURLWithPath: $0.path)) }
    }

    public func store(_ embeddings: [SearchEmbedding], model: String, in session: SessionInfo) async throws -> String? {
        let h = try await handle(session)
        let records = embeddings.map { EmbeddingRecord(key: $0.key, vector: $0.vector) }
        let global = globalIndex.path
        let r = try await RustSessionStore.call { token in
            try await h.storeEmbeddings(model: model, records: records, globalIndex: global, token: token)
        }
        return r.globalError
    }

    public func status(model: String, in session: SessionInfo) async throws -> SearchIndexStatus {
        let h = try await handle(session)
        let s = try await RustSessionStore.call { token in try await h.embeddingStatus(model: model, token: token) }
        return SearchIndexStatus(embedded: Int(s.embedded), total: Int(s.total), waiting: Int(s.waiting))
    }

    public func search(_ query: [Float], model: String, filter: PhotoFilter, in session: SessionInfo,
                       tuning: SearchTuning) async throws -> [SearchMatch] {
        let h = try await handle(session)
        let record = filter.searchRecord
        let hits = try await RustSessionStore.call { token in
            try await h.searchEmbedding(model: model, query: query, filter: record, options: tuning.record, token: token)
        }
        return hits.map { SearchMatch(id: PhotoID($0.variantId), score: $0.score) }
    }

    public func similar(to id: PhotoID, model: String, filter: PhotoFilter, in session: SessionInfo,
                        tuning: SearchTuning) async throws -> [SearchMatch] {
        let h = try await handle(session)
        let record = filter.searchRecord
        let hits = try await RustSessionStore.call { token in
            try await h.findSimilar(model: model, variantId: id.raw, filter: record, options: tuning.record, token: token)
        }
        return hits.map { SearchMatch(id: PhotoID($0.variantId), score: $0.score) }
    }

    public func searchAll(_ query: [Float], model: String, tuning: SearchTuning) async throws -> [SessionMatches] {
        let global = globalIndex.path
        let groups = try await RustSessionStore.call { token in
            try await searchAllSessions(globalIndex: global, model: model, query: query, options: tuning.record, token: token)
        }
        return groups.map { g in
            SessionMatches(root: URL(fileURLWithPath: g.root), name: g.name, available: g.available, bestScore: g.bestScore,
                           hits: g.hits.map { .init(url: URL(fileURLWithPath: $0.path), score: $0.score) })
        }
    }

    public func forget(_ session: SessionInfo) async throws {
        let global = globalIndex.path
        let root = session.url.path
        _ = try await RustSessionStore.call { token in
            try await forgetSessionInSearch(globalIndex: global, root: root, token: token)
        }
    }

    public func keywords(for id: PhotoID, labels: [String], vectors: [[Float]], top: Int, minProbability: Float,
                         model: String, in session: SessionInfo) async throws -> [String] {
        let h = try await handle(session)
        let vocabulary = KeywordVocabulary(labels: labels, labelVectors: vectors.flatMap { $0 },
                                           top: UInt32(clamping: top), minProbability: minProbability)
        let r = try await RustSessionStore.call { token in
            try await h.suggestKeywords(model: model, variantId: id.raw, vocabulary: vocabulary, token: token)
        }
        return r.map(\.keyword)
    }
}
