import DarkslideFFI
import Foundation

/// The libraries the import dialog has loaded, by id: a library's database is copied and
/// read once, then every dry run and the import use the same Rust object. The oldest are
/// dropped beyond a few (a dialog that was never closed properly).
final class AppleLibraries: @unchecked Sendable {
    static let shared = AppleLibraries()
    private let lock = NSLock()
    private var order: [UUID] = []
    private var libraries: [UUID: AppleLibrarySource] = [:]
    static let limit = 4

    func add(_ s: AppleLibrarySource) -> UUID {
        lock.lock()
        defer { lock.unlock() }
        let id = UUID()
        libraries[id] = s
        order.append(id)
        while order.count > Self.limit { libraries[order.removeFirst()] = nil }
        return id
    }

    func get(_ id: UUID) -> AppleLibrarySource? {
        lock.lock()
        defer { lock.unlock() }
        return libraries[id]
    }

    func remove(_ id: UUID) {
        lock.lock()
        defer { lock.unlock() }
        libraries[id] = nil
        order.removeAll { $0 == id }
    }
}

extension RustSessionStore: AppleLibraryImporting {
    public func appleLibraryKind(of url: URL) async -> AppleLibraryKind {
        (try? DarkslideFFI.appleLibraryKind(path: url.path)).flatMap(AppleLibraryKind.init(rawValue:)) ?? .unknown
    }

    public func loadAppleLibrary(_ url: URL) async throws -> AppleLibraryInfo {
        let source = try await Self.call { token in try await DarkslideFFI.loadAppleLibrary(path: url.path, token: token) }
        let s = source.summary()
        let id = AppleLibraries.shared.add(source)
        return AppleLibraryInfo(id: id, kind: AppleLibraryKind(rawValue: s.format) ?? .aperture, origin: s.origin,
                                writtenBy: s.writtenBy, photos: Int(s.photos), albums: Int(s.albums),
                                skippedAlbums: Int(s.skippedAlbums), warnings: s.warnings)
    }

    public func releaseAppleLibrary(_ id: UUID) async {
        AppleLibraries.shared.remove(id)
    }

    /// Like any import, a real one rewrites sidecars underneath: every cached record is dropped.
    public func importAppleLibrary(_ session: SessionInfo, library: UUID, categories: Set<LRCategory>, dryRun: Bool,
                                   overwriteEdits: Bool) async throws -> LRImportSummary {
        guard let h = archiveHandle(for: session) else { throw MockError.notFound("session \(session.name)") }
        guard let src = AppleLibraries.shared.get(library) else { throw MockError.notFound("the library") }
        defer { if !dryRun { forgetAll(in: session.id) } }
        let keys = categories.map(\.rawValue).sorted()
        let record = try await Self.call { token in
            try await h.importAppleLibrary(source: src, categories: keys, dryRun: dryRun, overwriteEdits: overwriteEdits,
                                           token: token)
        }
        return LRImportSummary(record)
    }

    public func canRevertAppleImport(_ session: SessionInfo) async -> Bool {
        archiveHandle(for: session)?.canRevertAppleImport() ?? false
    }

    public func revertAppleImport(_ session: SessionInfo) async throws -> LRRevertSummary? {
        guard let h = archiveHandle(for: session) else { throw MockError.notFound("session \(session.name)") }
        defer { forgetAll(in: session.id) }
        let record = try await Self.call { token in try await h.revertAppleImport(token: token) }
        return record.map {
            LRRevertSummary(restored: Int($0.restored), kept: Int($0.kept), variantsRemoved: Int($0.variantsRemoved),
                            albumsRemoved: Int($0.albumsRemoved), albumsTrimmed: Int($0.albumsTrimmed))
        }
    }
}
