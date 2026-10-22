import Foundation

/// In-memory sessions with deterministic fake photos (Sony A9 metadata).
/// Stands in for `ds-session` until the session facade is exported.
public actor MockSessionStore: SessionStore, SessionCreating, SessionClosing {
    private var sessions: [SessionInfo]
    private let root: URL
    /// Sessions `close(_:)` released, in order (tests).
    public private(set) var closed: [UUID] = []
    private var photosBySession: [UUID: [Photo]] = [:]
    private var albumsBySession: [UUID: [Album]] = [:]
    private var edits: [PhotoID: EditState] = [:]
    private var optimizeReports: [PhotoID: OptimizeReport] = [:]
    private var storedAIChanges: [PhotoID: String] = [:]
    private let latency: Duration
    public private(set) var saveCount = 0

    /// `counts` photos per session, newest session first.
    public init(counts: [Int] = [2_000, 312, 86], root: URL = URL(fileURLWithPath: "/Users/Shared/Darkslide"),
                latency: Duration = .zero) {
        let names = ["2026-09-21 Fall Hike", "2026-08-06 Beach Trip", "2026-07-04 Backyard"]
        var list: [SessionInfo] = []
        var nextID: Int64 = 1
        for (i, count) in counts.enumerated() {
            let name = i < names.count ? names[i] : "Session \(i + 1)"
            let created = Date(timeIntervalSince1970: 1_790_000_000 - Double(i) * 86_400 * 30)
            let s = SessionInfo(name: name, url: root.appendingPathComponent(name), photoCount: count, createdAt: created)
            list.append(s)
            let photos = Self.makePhotos(count: count, firstID: nextID, start: created)
            nextID += Int64(count)
            photosBySession[s.id] = photos
            albumsBySession[s.id] = [
                Album(name: "Selects", photoIDs: photos.filter { $0.rating >= 4 }.map(\.id)),
                Album(name: "Portfolio", photoIDs: photos.prefix(40).filter { $0.id.raw % 5 == 0 }.map(\.id)),
            ]
        }
        sessions = list
        self.root = root
        self.latency = latency
    }

    /// Deterministic photos: bursts of 1–6 frames, a few pre-rated.
    public static func makePhotos(count: Int, firstID: Int64, start: Date) -> [Photo] {
        var rng = SplitMix64(seed: UInt64(firstID))
        var photos: [Photo] = []
        photos.reserveCapacity(count)
        var t = start
        let lenses = ["FE 24-70mm F2.8 GM", "FE 85mm F1.4 GM", "FE 70-200mm F2.8 GM OSS", "FE 16-35mm F2.8 GM"]
        for i in 0..<count {
            let id = firstID + Int64(i)
            t += rng.next() % 6 == 0 ? Double(rng.next() % 600) : 0.1 + Double(rng.next() % 10) / 20
            let portrait = rng.next() % 5 == 0
            let lens = lenses[Int(rng.next() % UInt64(lenses.count))]
            let shutters = [1.0 / 4000, 1.0 / 1000, 1.0 / 500, 1.0 / 250, 1.0 / 125, 1.0 / 60, 1.0 / 30]
            let meta = PhotoMeta(
                camera: "Sony ILCE-9", lens: lens, iso: [100, 200, 400, 800, 1600, 3200][Int(rng.next() % 6)],
                shutter: shutters[Int(rng.next() % UInt64(shutters.count))],
                aperture: [1.4, 2.0, 2.8, 4.0, 5.6, 8.0][Int(rng.next() % 6)],
                focalLength: Double([24, 35, 50, 70, 85, 135, 200][Int(rng.next() % 7)]),
                captureDate: t, pixelWidth: portrait ? 4000 : 6000, pixelHeight: portrait ? 6000 : 4000,
                fileSize: 48_000_000 + Int64(rng.next() % 2_000_000))
            let rating = rng.next() % 9 == 0 ? Int(rng.next() % 5) + 1 : 0
            photos.append(Photo(id: PhotoID(id), filename: String(format: "DSC%05d.ARW", 1000 + Int(id) % 99_000),
                                meta: meta, rating: rating))
        }
        return photos
    }

    private func delay() async throws {
        if latency > .zero { try await Task.sleep(for: latency) }
    }

    public func listSessions() async throws -> [SessionInfo] {
        try await delay()
        return sessions
    }

    public func photos(in session: SessionInfo) async throws -> [Photo] {
        try await delay()
        guard let p = photosBySession[session.id] else { throw MockError.notFound("session \(session.name)") }
        return p
    }

    public func albums(in session: SessionInfo) async throws -> [Album] {
        albumsBySession[session.id] ?? []
    }

    public func createSession(named name: String) async throws -> SessionInfo {
        try await createSession(named: name, in: root)
    }

    public func createSession(named name: String, in folder: URL) async throws -> SessionInfo {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { throw MockError.invalid("Session name is empty") }
        guard !sessions.contains(where: { $0.name == trimmed }) else { throw MockError.invalid("“\(trimmed)” already exists") }
        let s = SessionInfo(name: trimmed, url: folder.appendingPathComponent(trimmed), photoCount: 0, createdAt: Date())
        sessions.insert(s, at: 0)
        photosBySession[s.id] = []
        return s
    }

    public func newSessionLocation() async -> URL { root }

    public func close(_ session: SessionInfo) async { closed.append(session.id) }

    public func save(photos updated: [Photo]) async throws {
        saveCount += 1
        for p in updated {
            for (sid, list) in photosBySession {
                if let i = list.firstIndex(where: { $0.id == p.id }) {
                    photosBySession[sid]![i] = p
                }
            }
        }
    }

    public func edit(for id: PhotoID) async throws -> EditState {
        edits[id] ?? EditState()
    }

    public func save(edits updated: [PhotoID: EditState]) async throws {
        saveCount += 1
        edits.merge(updated) { _, new in new }
    }

    /// Register a new photo (`filename`, `meta`) right after `source` in `session` (demo merges).
    public func addPhoto(after source: PhotoID, in session: SessionInfo, filename: String, meta: PhotoMeta) throws -> Photo {
        guard var list = photosBySession[session.id] else { throw MockError.notFound("session \(session.name)") }
        let next = (photosBySession.values.flatMap { $0 }.map(\.id.raw).max() ?? 0) + 1
        let photo = Photo(id: PhotoID(next), filename: filename, meta: meta)
        list.insert(photo, at: (list.firstIndex { $0.id == source } ?? list.count - 1) + 1)
        photosBySession[session.id] = list
        return photo
    }
}

public enum MockError: Error, Equatable, LocalizedError {
    case notFound(String)
    case invalid(String)

    public var errorDescription: String? {
        switch self {
        case .notFound(let s): "Not found: \(s)"
        case .invalid(let s): s
        }
    }
}

/// Small deterministic PRNG so mock data is stable across runs and tests.
public struct SplitMix64: RandomNumberGenerator, Sendable {
    private var state: UInt64
    public init(seed: UInt64) { state = seed }
    public mutating func next() -> UInt64 {
        state &+= 0x9E37_79B9_7F4A_7C15
        var z = state
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        return z ^ (z >> 31)
    }

    public mutating func unit() -> Double { Double(next() >> 11) / Double(1 << 53) }
}

// MARK: - AI Optimize reports (in memory)

extension MockSessionStore: OptimizeReportStoring {
    public func optimizeReport(for id: PhotoID) async throws -> OptimizeReport? { optimizeReports[id] }

    public func saveOptimizeReport(_ report: OptimizeReport?, for ids: [PhotoID]) async throws {
        for id in ids { optimizeReports[id] = report }
    }
}

// MARK: - AI changes (in memory)

extension MockSessionStore: AIChangesStoring {
    public func aiChanges(for id: PhotoID) async throws -> String? { storedAIChanges[id] }

    public func saveAIChanges(_ json: String?, for ids: [PhotoID]) async throws {
        for id in ids { storedAIChanges[id] = json }
    }
}
