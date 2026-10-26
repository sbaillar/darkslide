import LuxtableFFI
import Foundation
import Observation

/// People, Lightroom-style: faces found in the background, unnamed clusters
/// to name ("Who is this?"), suggestions to confirm or reject, merge and split,
/// and a person as a browser filter. Names live in the app's people store, so
/// they work in every session; who is on a photo lives in its sidecars.
@MainActor @Observable
public final class PeopleModel {
    /// Measured on an augmentation proxy (two people × 12 variants): clusters
    /// are exact for 0.60–0.75 and merge at 0.55; the defaults lean to precision.
    /// Not measured on real identities (no faces in the sample set).
    public nonisolated static let defaultTuning = PeopleTuning(cluster: 0.70, suggest: 0.72, autoMatch: 0.80)

    public var tuning = PeopleModel.defaultTuning
    /// The open session's people and clusters (nil until loaded).
    public private(set) var overview: PeopleOverview?
    /// Everyone in the people store.
    public private(set) var known: [KnownPerson] = []
    /// Background face scanning: (done, total) while it runs.
    public private(set) var indexing: FaceIndexer.Progress?
    public private(set) var lastReport: FaceIndexer.Report?
    /// The last action's failure, for the panel.
    public private(set) var error: String?
    /// Clusters picked for Merge (by index into `overview.unnamed`).
    public var selected: Set<Int> = []

    @ObservationIgnored public private(set) var backend: (any PeopleBackend)?
    @ObservationIgnored weak var app: AppModel?
    @ObservationIgnored var makeEmbedder: @Sendable () -> any FaceEmbedding = { VisionFaceEmbedder() }
    @ObservationIgnored var makeIndexer: @Sendable (any PeopleBackend, any FaceEmbedding) -> FaceIndexer = {
        FaceIndexer(backend: $0, embedder: $1)
    }
    @ObservationIgnored private var embedder: (any FaceEmbedding)?
    @ObservationIgnored private var indexTask: Task<Void, Never>?
    @ObservationIgnored private var indexRun = 0
    @ObservationIgnored private var indexedSession: UUID?
    /// Bumped by every reload: an older reply is dropped.
    @ObservationIgnored private var ticket = 0
    /// Reload the overview after this many newly scanned photos while indexing.
    @ObservationIgnored var refreshEvery = 32

    public init() {}

    public func configure(backend: any PeopleBackend) {
        self.backend = backend
    }

    func attach(_ app: AppModel) {
        self.app = app
    }

    public var isAvailable: Bool { backend != nil }

    var modelID: String { faceEmbedder.modelID }

    private var faceEmbedder: any FaceEmbedding {
        if let embedder { return embedder }
        let e = makeEmbedder()
        embedder = e
        return e
    }

    /// A line for the status bar while faces are being found.
    public var indexingMessage: String? {
        guard let p = indexing, p.total > 0, p.done < p.total else { return nil }
        return "Finding faces… \(p.done.formatted()) of \(p.total.formatted())"
    }

    // MARK: - Session

    func sessionOpened(_ session: SessionInfo) {
        if indexedSession != session.id {
            sessionClosed()
        }
        startIndexing()
        Task { await reload() }
    }

    func sessionClosed() {
        indexTask?.cancel()
        indexTask = nil
        indexing = nil
        indexedSession = nil
        indexRun += 1
        ticket += 1
        overview = nil
        selected = []
        error = nil
    }

    /// Scan the open session for faces in the background unless already running.
    public func startIndexing() {
        guard let backend, let session = app?.currentSession else { return }
        if indexTask != nil, indexedSession == session.id { return }
        indexTask?.cancel()
        indexedSession = session.id
        indexRun += 1
        let run = indexRun
        let indexer = makeIndexer(backend, faceEmbedder)
        let every = refreshEvery
        let progress: @Sendable (FaceIndexer.Progress) -> Void = { [weak self] p in
            Task { @MainActor [weak self] in
                guard let self, self.indexRun == run else { return }
                self.indexing = p
                if p.done > 0, p.done % every == 0 { await self.reload() }
            }
        }
        let pass = Task.detached(priority: .background) { try await indexer.run(session, progress: progress) }
        indexTask = Task { [weak self] in
            let report = try? await withTaskCancellationHandler { try await pass.value } onCancel: { pass.cancel() }
            guard let self, self.indexRun == run else { return }
            self.lastReport = report
            self.indexing = nil
            self.indexTask = nil
            await self.reload()
        }
    }

    /// Wait for the running scan (tests).
    func waitForIndexing() async {
        await indexTask?.value
    }

    // MARK: - Overview

    /// Load the session's people and clusters, and the people store.
    public func reload() async {
        guard let backend, let session = app?.currentSession else { return }
        ticket += 1
        let mine = ticket
        do {
            let o = try await backend.overview(model: modelID, tuning: tuning, in: session)
            let k = try await backend.knownPeople()
            guard mine == ticket, app?.currentSession?.id == session.id else { return }
            overview = o
            known = k
            selected = selected.filter { $0 < o.unnamed.count }
            app?.setPeople(Self.peopleByPhoto(o))
        } catch is CancellationError {
        } catch {
            if mine == ticket { self.error = Self.message(error) }
        }
    }

    /// Who is on each photo, from the overview's people.
    static func peopleByPhoto(_ o: PeopleOverview) -> [PhotoID: [String]] {
        var out: [PhotoID: [String]] = [:]
        for g in o.people {
            for v in g.variantIds where !(out[PhotoID(v)] ?? []).contains(g.name) {
                out[PhotoID(v), default: []].append(g.name)
            }
        }
        return out
    }

    static func message(_ error: Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
    }

    /// Names that start like `text` (for "Who is this?"), best first.
    public func completions(for text: String) -> [String] {
        let t = text.trimmingCharacters(in: .whitespaces).lowercased()
        guard !t.isEmpty else { return [] }
        return known.map(\.name).filter { $0.lowercased().hasPrefix(t) && $0.lowercased() != t }
    }

    // MARK: - Actions

    /// Run `action`, then reload and make the change findable.
    private func perform(_ action: (any PeopleBackend, SessionInfo) async throws -> Void) async {
        guard let backend, let session = app?.currentSession else { return }
        error = nil
        do {
            try await action(backend, session)
        } catch is CancellationError {
            return
        } catch {
            self.error = Self.message(error)
        }
        await reload()
        let model = modelID
        let suggest = tuning.autoMatch
        // Best effort: Find catches up on the next sync if this fails.
        try? await backend.syncFind(faceModel: model, suggest: suggest, in: session)
    }

    /// "Who is this?": the cluster is `name` (a new person if nobody has that name).
    public func name(_ cluster: FaceCluster, as name: String) async {
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !n.isEmpty else { return }
        let ids = cluster.faces.map(\.id)
        let model = modelID
        await perform { b, s in _ = try await b.name(ids, as: n, model: model, in: s) }
    }

    /// "Is this Alex?" → Yes.
    public func confirm(_ cluster: FaceCluster) async {
        guard let s = cluster.suggestion else { return }
        await name(cluster, as: s.name)
    }

    /// "Is this Alex?" → No (never suggested for these faces again).
    public func reject(_ cluster: FaceCluster) async {
        guard let s = cluster.suggestion else { return }
        let ids = cluster.faces.map(\.id)
        let model = modelID
        await perform { b, session in
            try await b.reject(ids, personID: s.personId, name: s.name, model: model, in: session)
        }
    }

    /// Merge the selected clusters (they cluster as one from now on).
    public func mergeSelected() async {
        guard let o = overview else { return }
        let clusters = selected.sorted().filter { $0 < o.unnamed.count }.map { o.unnamed[$0] }
        guard clusters.count > 1 else { return }
        selected = []
        let ids = clusters.flatMap { $0.faces.map(\.id) }
        await perform { b, s in try await b.merge(ids, in: s) }
    }

    /// Drop a cluster onto a person: they're that person.
    public func merge(_ cluster: FaceCluster, into person: PersonGroup) async {
        await name(cluster, as: person.name)
    }

    /// Split `faces` off their cluster (they stay together, apart from the rest).
    public func split(_ faces: [FaceRecord], from cluster: FaceCluster) async {
        let picked = Set(faces.map(\.id))
        let rest = cluster.faces.map(\.id).filter { !picked.contains($0) }
        guard !picked.isEmpty, !rest.isEmpty else { return }
        let ids = faces.map(\.id)
        await perform { b, s in try await b.split(ids, from: rest, in: s) }
    }

    /// "Not this person": remove faces from a named person (remembered).
    public func remove(_ faces: [FaceRecord], from person: PersonGroup) async {
        guard !faces.isEmpty else { return }
        let ids = faces.map(\.id)
        let model = modelID
        await perform { b, s in
            try await b.reject(ids, personID: person.personId, name: person.name, model: model, in: s)
        }
    }

    public func rename(_ person: PersonGroup, to name: String) async {
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let id = person.personId, !n.isEmpty, n != person.name else { return }
        await perform { b, s in
            if !(try await b.rename(personID: id, to: n, in: s)) {
                throw PeopleError.nameTaken(n)
            }
        }
        if let filtered = app?.filter.person, filtered.caseInsensitiveCompare(person.name) == .orderedSame {
            app?.filter.person = n
        }
    }

    /// Show only this person's photos in the browser (again: show everyone).
    public func show(_ person: PersonGroup) {
        guard let app else { return }
        app.filter.person = app.filter.person == person.name ? nil : person.name
    }
}

public enum PeopleError: Error, Equatable, LocalizedError {
    case nameTaken(String)

    public var errorDescription: String? {
        switch self {
        case .nameTaken(let n): "Someone is already called “\(n)”."
        }
    }
}
