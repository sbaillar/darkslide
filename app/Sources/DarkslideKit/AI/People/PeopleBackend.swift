import DarkslideFFI
import Foundation

/// Face thresholds (mean cosine similarity of face vectors). The embedding is
/// a general image descriptor, not an identity model, so a guess fails safe:
/// `suggest` only asks ("Is this Alex?"; nothing is tagged until confirmed),
/// and Find counts an unconfirmed face as "Alex?" only above the higher
/// `autoMatch` bar.
public struct PeopleTuning: Sendable, Equatable {
    public var cluster: Float
    public var suggest: Float
    public var autoMatch: Float
    public init(cluster: Float, suggest: Float, autoMatch: Float) {
        self.cluster = cluster
        self.suggest = suggest
        self.autoMatch = autoMatch
    }

    var record: PeopleOptions { PeopleOptions(clusterThreshold: cluster, suggestThreshold: suggest) }
}

/// The store side of People (seam for tests): faces per session, clusters,
/// names in the app's people store, and Find's sync.
public protocol PeopleBackend: Sendable {
    func faceJobs(model: String, in session: SessionInfo) async throws -> [SearchJob]
    func storeFaces(_ faces: [FoundFace], key: String, model: String, in session: SessionInfo) async throws
    func faceStatus(model: String, in session: SessionInfo) async throws -> FaceStatus
    func overview(model: String, tuning: PeopleTuning, in session: SessionInfo) async throws -> PeopleOverview
    /// These faces are `name` (a new person if nobody has that name).
    func name(_ faces: [FaceId], as name: String, model: String, in session: SessionInfo) async throws -> KnownPerson
    /// These faces aren't this person.
    func reject(_ faces: [FaceId], personID: String?, name: String, model: String, in session: SessionInfo) async throws
    func merge(_ faces: [FaceId], in session: SessionInfo) async throws
    func split(_ faces: [FaceId], from rest: [FaceId], in session: SessionInfo) async throws
    func rename(personID: String, to name: String, in session: SessionInfo?) async throws -> Bool
    func knownPeople() async throws -> [KnownPerson]
    /// Make the session's photos, people and faces findable from every session.
    func syncFind(faceModel: String, suggest: Float, in session: SessionInfo) async throws
    /// Read the place (XMP location fields, GPS) of up to `limit` photos; returns how many are left.
    func scanLocations(limit: Int, in session: SessionInfo) async throws -> Int
}

extension AppPaths {
    /// The app's people (names, confirmed faces): `<support>/people.json`.
    public static var peopleStore: URL { support.appendingPathComponent("people.json") }
}

/// `PeopleBackend` over ds-session through the FFI.
public struct RustPeopleBackend: PeopleBackend {
    public let store: RustSessionStore
    public let globalIndex: URL
    public let peopleStore: URL

    public init(store: RustSessionStore, globalIndex: URL = RustImportService.defaultGlobalIndex,
                peopleStore: URL = AppPaths.peopleStore) {
        self.store = store
        self.globalIndex = globalIndex
        self.peopleStore = peopleStore
    }

    private func handle(_ session: SessionInfo) async throws -> SessionHandle {
        guard let h = await store.archiveHandle(for: session) else { throw SearchBackendError.sessionNotOpen }
        return h
    }

    public func faceJobs(model: String, in session: SessionInfo) async throws -> [SearchJob] {
        let h = try await handle(session)
        let jobs = try await RustSessionStore.call { token in try await h.faceJobs(model: model, token: token) }
        return jobs.map { SearchJob(key: $0.key, url: URL(fileURLWithPath: $0.path)) }
    }

    public func storeFaces(_ faces: [FoundFace], key: String, model: String, in session: SessionInfo) async throws {
        let h = try await handle(session)
        let records = faces.map {
            DetectedFace(rect: FaceRect(x: Float($0.rect.minX), y: Float($0.rect.minY), w: Float($0.rect.width),
                                        h: Float($0.rect.height)),
                         quality: $0.quality, vector: $0.vector)
        }
        _ = try await RustSessionStore.call { token in
            try await h.storeFaces(model: model, key: key, faces: records, token: token)
        }
    }

    public func faceStatus(model: String, in session: SessionInfo) async throws -> FaceStatus {
        let h = try await handle(session)
        return try await RustSessionStore.call { token in try await h.faceStatus(model: model, token: token) }
    }

    public func overview(model: String, tuning: PeopleTuning, in session: SessionInfo) async throws -> PeopleOverview {
        let h = try await handle(session)
        let people = peopleStore.path
        return try await RustSessionStore.call { token in
            try await h.peopleOverview(model: model, peopleStore: people, options: tuning.record, token: token)
        }
    }

    public func name(_ faces: [FaceId], as name: String, model: String, in session: SessionInfo) async throws -> KnownPerson {
        let h = try await handle(session)
        let people = peopleStore.path
        return try await RustSessionStore.call { token in
            try await h.nameFaces(model: model, faces: faces, name: name, peopleStore: people, token: token)
        }
    }

    public func reject(_ faces: [FaceId], personID: String?, name: String, model: String,
                       in session: SessionInfo) async throws {
        let h = try await handle(session)
        let people = peopleStore.path
        try await RustSessionStore.call { token in
            try await h.rejectFaces(model: model, faces: faces, personId: personID, name: name, peopleStore: people,
                                    token: token)
        }
    }

    public func merge(_ faces: [FaceId], in session: SessionInfo) async throws {
        let h = try await handle(session)
        try await RustSessionStore.call { token in try await h.mergeFaces(faces: faces, token: token) }
    }

    public func split(_ faces: [FaceId], from rest: [FaceId], in session: SessionInfo) async throws {
        let h = try await handle(session)
        try await RustSessionStore.call { token in try await h.splitFaces(faces: faces, rest: rest, token: token) }
    }

    public func rename(personID: String, to name: String, in session: SessionInfo?) async throws -> Bool {
        let h: SessionHandle? = if let session { await store.archiveHandle(for: session) } else { nil }
        let people = peopleStore.path
        return try await RustSessionStore.call { token in
            try await renamePerson(session: h, peopleStore: people, personId: personID, name: name, token: token)
        }
    }

    public func knownPeople() async throws -> [KnownPerson] {
        let people = peopleStore.path
        return try await Task.detached(priority: .userInitiated) {
            do { return try DarkslideFFI.knownPeople(peopleStore: people) } catch let e as DsError {
                throw RustSessionStore.mapped(e)
            }
        }.value
    }

    public func scanLocations(limit: Int, in session: SessionInfo) async throws -> Int {
        let h = try await handle(session)
        let left = try await RustSessionStore.call { token in
            try await h.scanLocations(limit: UInt32(clamping: limit), token: token)
        }
        return Int(left)
    }

    public func syncFind(faceModel: String, suggest: Float, in session: SessionInfo) async throws {
        let h = try await handle(session)
        let global = globalIndex.path
        let people = peopleStore.path
        _ = try await RustSessionStore.call { token in
            try await h.syncFind(globalIndex: global, faceModel: faceModel, peopleStore: people,
                                 suggestThreshold: suggest, token: token)
        }
    }
}
