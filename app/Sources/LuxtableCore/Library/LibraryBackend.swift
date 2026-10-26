import CoreGraphics
import LuxtableFFI
import Foundation

/// The store side of the Library (seam for tests).
public protocol LibraryBackend: Sendable {
    /// The open session's photos reach the Library.
    func sync(_ session: SessionInfo) async throws
    /// Sync every other session whose index changed since; whether any did.
    func refresh() async throws -> Bool
    func query(_ q: LibraryQueryRecord, first: Int) async throws -> LibraryPage
    func items(_ ids: [Int64]) async throws -> [LibraryItemRecord]
    func overview() async throws -> LibraryOverviewRecord
    func edit(_ ids: [Int64], _ change: LibraryEditRecord) async throws -> LibraryEditReport
    func createCollection(name: String, rules: LibraryQueryRecord?, ids: [Int64]) async throws -> Int64
    func updateCollection(_ id: Int64, name: String, rules: LibraryQueryRecord?) async throws -> Bool
    func deleteCollection(_ id: Int64) async throws -> Bool
    func members(of collection: Int64, _ ids: [Int64], remove: Bool) async throws -> Int
    /// The photo's thumbnail: its developed one from the disk cache when it is
    /// edited and has one there, else its embedded preview.
    func thumbnail(for item: LibraryItemRecord, maxPixelSize: Int) async throws -> CGImage
}

/// `LibraryBackend` over the global index through the FFI.
public struct RustLibraryBackend: LibraryBackend {
    public let store: RustSessionStore
    public let globalIndex: URL
    public let previews: any PreviewProvider
    /// A developed thumbnail from the disk cache (`nil`: none for that edit).
    public let developed: @Sendable (URL, Int64, String) async -> CGImage?

    public init(store: RustSessionStore, previews: any PreviewProvider, globalIndex: URL = RustImportService.defaultGlobalIndex,
                developed: @escaping @Sendable (URL, Int64, String) async -> CGImage? = { _, _, _ in nil }) {
        self.store = store
        self.previews = previews
        self.globalIndex = globalIndex
        self.developed = developed
    }

    public func sync(_ session: SessionInfo) async throws {
        guard let h = await store.archiveHandle(for: session) else { return }
        let g = globalIndex.path
        _ = try await RustSessionStore.call { token in try await h.syncLibrary(globalIndex: g, token: token) }
    }

    public func refresh() async throws -> Bool {
        let g = globalIndex.path
        let r = try await RustSessionStore.call { token in try await libraryRefresh(globalIndex: g, token: token) }
        return r.synced > 0
    }

    public func query(_ q: LibraryQueryRecord, first: Int) async throws -> LibraryPage {
        let g = globalIndex.path
        return try await RustSessionStore.call { token in
            try await libraryQuery(globalIndex: g, query: q, first: UInt32(clamping: first), token: token)
        }
    }

    public func items(_ ids: [Int64]) async throws -> [LibraryItemRecord] {
        let g = globalIndex.path
        return try await RustSessionStore.call { token in try await libraryItems(globalIndex: g, ids: ids, token: token) }
    }

    public func overview() async throws -> LibraryOverviewRecord {
        let g = globalIndex.path
        return try await RustSessionStore.call { token in try await libraryOverview(globalIndex: g, token: token) }
    }

    public func edit(_ ids: [Int64], _ change: LibraryEditRecord) async throws -> LibraryEditReport {
        let g = globalIndex.path
        return try await RustSessionStore.call { token in
            try await libraryEdit(globalIndex: g, ids: ids, change: change, token: token)
        }
    }

    public func createCollection(name: String, rules: LibraryQueryRecord?, ids: [Int64]) async throws -> Int64 {
        let g = globalIndex.path
        return try await RustSessionStore.call { token in
            try await libraryCreateCollection(globalIndex: g, name: name, rules: rules, ids: ids, token: token)
        }
    }

    public func updateCollection(_ id: Int64, name: String, rules: LibraryQueryRecord?) async throws -> Bool {
        let g = globalIndex.path
        return try await RustSessionStore.call { token in
            try await libraryUpdateCollection(globalIndex: g, id: id, name: name, rules: rules, token: token)
        }
    }

    public func deleteCollection(_ id: Int64) async throws -> Bool {
        let g = globalIndex.path
        return try await RustSessionStore.call { token in try await libraryDeleteCollection(globalIndex: g, id: id, token: token) }
    }

    public func members(of collection: Int64, _ ids: [Int64], remove: Bool) async throws -> Int {
        let g = globalIndex.path
        let n = try await RustSessionStore.call { token in
            try await libraryCollectionMembers(globalIndex: g, id: collection, ids: ids, remove: remove, token: token)
        }
        return Int(n)
    }

    public func thumbnail(for item: LibraryItemRecord, maxPixelSize: Int) async throws -> CGImage {
        let url = URL(fileURLWithPath: item.path)
        if let hash = item.editHash, let img = await developed(url, item.variant, hash) { return img }
        try Task.checkCancellation()
        return try await previews.preview(for: url, maxPixelSize: maxPixelSize)
    }
}

extension RustDevelopTools {
    /// [library] A developed thumbnail straight from the disk cache (never renders).
    public func cachedThumbnail(url: URL, variant: Int64, editHash: String) async -> CGImage? {
        guard let thumbnails else { return nil }
        let jpeg = try? await Self.run {
            try await thumbnails.cachedThumbnail(path: url.path, variant: UInt64(bitPattern: variant), editHash: editHash,
                                                 token: $0)
        }
        return jpeg.flatMap { $0 }.flatMap(ThumbnailJPEG.image)
    }
}
