import DarkslideFFI
import Foundation

extension RustSessionStore: CaptureOneImporting {
    public func isCaptureOneFolder(_ folder: URL) async -> Bool {
        (try? DarkslideFFI.isCaptureOneFolder(folder: folder.path)) ?? false
    }

    public func canRevertCaptureOneImport(_ session: SessionInfo) async -> Bool {
        archiveHandle(for: session)?.canRevertCaptureOneImport() ?? false
    }

    /// Like an import, a revert rewrites sidecars underneath: every cached record is dropped.
    public func revertCaptureOneImport(_ session: SessionInfo) async throws -> C1RevertSummary? {
        guard let h = archiveHandle(for: session) else { throw MockError.notFound("session \(session.name)") }
        defer { forgetAll(in: session.id) }
        let record = try await Self.call { token in try await h.revertCaptureOneImport(token: token) }
        return record.map {
            C1RevertSummary(restored: Int($0.restored), kept: Int($0.kept), variantsRemoved: Int($0.variantsRemoved),
                            albumsRemoved: Int($0.albumsRemoved), albumsTrimmed: Int($0.albumsTrimmed))
        }
    }

    /// ds-c1 import into the open in-place session of a Capture One folder.
    /// After a real import every cached record of the session is dropped:
    /// sidecars changed underneath, and a stale adjustments JSON would
    /// otherwise be the base of the next save.
    public func importCaptureOne(_ session: SessionInfo, dryRun: Bool, overwriteEdits: Bool) async throws -> C1ImportSummary {
        guard let h = archiveHandle(for: session) else { throw MockError.notFound("session \(session.name)") }
        defer { if !dryRun { forgetAll(in: session.id) } }
        let record = try await Self.call { token in
            try await h.importCaptureOne(dryRun: dryRun, overwriteEdits: overwriteEdits, token: token)
        }
        return C1ImportSummary(record)
    }
}

extension C1ImportSummary {
    init(_ r: C1ImportReportRecord) {
        self.init(
            dryRun: r.dryRun, writtenBy: r.writtenBy, images: Int(r.images), matched: Int(r.matched),
            variantsCreated: Int(r.variantsCreated), notFound: r.notFound, outsideFolder: Int(r.outsideFolder),
            inTrash: Int(r.inTrash), inSelects: Int(r.inSelects), rated: Int(r.rated), tagged: Int(r.tagged),
            picked: Int(r.picked), keyworded: Int(r.keyworded), editsMapped: Int(r.editsMapped),
            editedInC1: Int(r.editedInC1), withUnmapped: Int(r.withUnmapped),
            mapped: r.mapped.map { Count(key: $0.key, count: Int($0.count)) },
            unmapped: r.unmapped.map { Unmapped(key: $0.key, reason: $0.reason, count: Int($0.count)) },
            inSync: Int(r.inSync), keptDarkslide: Int(r.keptDarkslide), deletedInDarkslide: Int(r.deletedInDarkslide),
            conflicts: r.conflicts.map { Conflict(photo: PhotoID($0.variantId), file: $0.file, field: $0.field) },
            overwritten: Int(r.overwritten), readOnly: Int(r.readOnly), albumsCreated: Int(r.albumsCreated),
            albumsUpdated: Int(r.albumsUpdated),
            skipped: r.collectionsSkipped.map { Skipped(name: $0.name, reason: $0.reason) },
            warnings: r.warnings)
    }
}
