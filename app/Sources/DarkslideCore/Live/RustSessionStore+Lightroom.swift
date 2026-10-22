import DarkslideFFI
import Foundation

/// The sources the import dialog has loaded, by id: a catalog is copied and read once,
/// then every dry run and the import use the same Rust object. The oldest are dropped
/// beyond a few (a dialog that was never closed properly).
final class LightroomSources: @unchecked Sendable {
    static let shared = LightroomSources()
    private let lock = NSLock()
    private var order: [UUID] = []
    private var sources: [UUID: LightroomSource] = [:]
    static let limit = 4

    func add(_ s: LightroomSource) -> UUID {
        lock.lock()
        defer { lock.unlock() }
        let id = UUID()
        sources[id] = s
        order.append(id)
        while order.count > Self.limit { sources[order.removeFirst()] = nil }
        return id
    }

    func get(_ id: UUID) -> LightroomSource? {
        lock.lock()
        defer { lock.unlock() }
        return sources[id]
    }

    func remove(_ id: UUID) {
        lock.lock()
        defer { lock.unlock() }
        sources[id] = nil
        order.removeAll { $0 == id }
    }
}

extension RustSessionStore: LightroomImporting {
    public func lightroomKind(of url: URL) async -> LRSourceKind {
        (try? DarkslideFFI.lightroomSourceKind(path: url.path)).flatMap(LRSourceKind.init(rawValue:)) ?? .unknown
    }

    public func loadLightroomSource(_ url: URL) async throws -> LRSourceInfo {
        let source = try await Self.call { token in try await DarkslideFFI.loadLightroomSource(path: url.path, token: token) }
        let s = source.summary()
        let id = LightroomSources.shared.add(source)
        return LRSourceInfo(id: id, kind: LRSourceKind(rawValue: s.kind) ?? .catalog, origin: s.origin, writtenBy: s.writtenBy,
                            photos: Int(s.photos), collections: Int(s.collections), smartCollections: Int(s.smartCollections),
                            warnings: s.warnings)
    }

    public func releaseLightroomSource(_ id: UUID) async {
        LightroomSources.shared.remove(id)
    }

    /// Like any import, a real one rewrites sidecars underneath: every cached record is dropped.
    public func importLightroom(_ session: SessionInfo, source: UUID, categories: Set<LRCategory>, dryRun: Bool,
                                overwriteEdits: Bool) async throws -> LRImportSummary {
        guard let h = archiveHandle(for: session) else { throw MockError.notFound("session \(session.name)") }
        guard let src = LightroomSources.shared.get(source) else { throw MockError.notFound("the Lightroom source") }
        defer { if !dryRun { forgetAll(in: session.id) } }
        let keys = categories.map(\.rawValue).sorted()
        let record = try await Self.call { token in
            try await h.importLightroom(source: src, categories: keys, dryRun: dryRun, overwriteEdits: overwriteEdits, token: token)
        }
        return LRImportSummary(record)
    }

    public func canRevertLightroomImport(_ session: SessionInfo) async -> Bool {
        archiveHandle(for: session)?.canRevertLightroomImport() ?? false
    }

    public func revertLightroomImport(_ session: SessionInfo) async throws -> LRRevertSummary? {
        guard let h = archiveHandle(for: session) else { throw MockError.notFound("session \(session.name)") }
        defer { forgetAll(in: session.id) }
        let record = try await Self.call { token in try await h.revertLightroomImport(token: token) }
        return record.map {
            LRRevertSummary(restored: Int($0.restored), kept: Int($0.kept), variantsRemoved: Int($0.variantsRemoved),
                            albumsRemoved: Int($0.albumsRemoved), albumsTrimmed: Int($0.albumsTrimmed))
        }
    }

    public func readLightroomPresets(_ url: URL) async throws -> [LRPresetItem] {
        let records = try await Self.call { token in try await DarkslideFFI.readLightroomPresets(path: url.path, token: token) }
        return records.map(LRPresetItem.init)
    }
}

extension LRPresetItem {
    init(_ r: LrPresetRecord) {
        // An unreadable result is never a preset: it would reset the tools it names.
        let edit = try? AdjustmentsCodec.decode(r.adjustmentsJson).0
        let groups = Set(r.groups.map(AdjustGroup.init(rust:))).subtracting([.layers, .spots])
        let preset = Preset(name: r.name, groups: groups, edit: edit ?? EditState())
        self.init(name: r.name, group: r.group, preset: preset, notes: r.notes,
                  unavailable: edit == nil ? "couldn't be read" : groups.isEmpty ? "nothing in it maps to Darkslide" : nil)
    }
}

extension LRImportSummary {
    init(_ r: LrImportReportRecord) {
        self.init(
            dryRun: r.dryRun, writtenBy: r.writtenBy, photos: Int(r.photos), matched: Int(r.matched),
            matchedByPath: Int(r.matchedByPath), matchedByHash: Int(r.matchedByHash), matchedByName: Int(r.matchedByName),
            ambiguous: r.ambiguous, notFound: r.notFound.map(\.path), notFoundCount: Int(r.notFoundCount),
            notFoundFolders: r.notFoundFolders.map { Folder(path: $0.folder, photos: Int($0.photos)) },
            variantsCreated: Int(r.variantsCreated),
            changed: Dictionary(r.changed.map { ($0.category, Int($0.count)) }, uniquingKeysWith: +),
            mapped: r.mapped.map { Mapped(key: $0.key, approximate: $0.approximate, count: Int($0.count)) },
            dropped: r.dropped.map { Dropped(key: $0.key, reason: $0.reason, count: Int($0.count)) },
            withDropped: Int(r.withDropped),
            unmappedLabels: Dictionary(r.unmappedLabels.map { ($0.name, Int($0.count)) }, uniquingKeysWith: +),
            inSync: Int(r.inSync), keptDarkslide: Int(r.keptDarkslide), deletedInDarkslide: Int(r.deletedInDarkslide),
            conflicts: r.conflicts.map { Conflict(photo: PhotoID($0.variantId), file: $0.file, field: $0.field) },
            overwritten: Int(r.overwritten), readOnly: Int(r.readOnly), albumsCreated: Int(r.albumsCreated),
            albumsUpdated: Int(r.albumsUpdated),
            skipped: r.collectionsSkipped.map { Skipped(name: $0.name, reason: $0.reason) },
            notes: r.notes.map { Note(file: $0.file, notes: $0.notes) }, warnings: r.warnings)
    }
}
