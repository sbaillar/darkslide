import Foundation

// Smart-folder persistence on `SessionStore`. `RustSessionStore` keeps them in
// the lt-session (albums.json, built-ins created on open). Stores without a
// session index (demo, the read-only folder store) fall back to an in-memory
// registry so the feature still works there, without persistence.

extension SessionStore {
    /// Smart folders of `session`: built-ins first, then the user's. `photos` is
    /// the session's current photos (used by the in-memory fallback for tags in use).
    public func smartFolders(in session: SessionInfo, photos: [Photo]) async throws -> [SmartFolder] {
        if let s = self as? any SmartFolderStoring { return try await s.storedSmartFolders(in: session) }
        return await LocalSmartFolders.shared.folders(session.id, tagsInUse: Set(photos.map(\.colorTag)))
    }

    public func createSmartFolder(named name: String, criteria: SmartCriteria, in session: SessionInfo) async throws -> SmartFolder {
        let name = try SmartFolderNames.validated(name)
        if let s = self as? any SmartFolderStoring { return try await s.storeCreateSmartFolder(named: name, criteria: criteria, in: session) }
        return await LocalSmartFolders.shared.create(session.id, name: name, criteria: criteria)
    }

    public func updateSmartFolder(_ folder: SmartFolder, in session: SessionInfo) async throws {
        guard !folder.isBuiltin else { throw SmartFolderError.builtin }
        _ = try SmartFolderNames.validated(folder.name)
        if let s = self as? any SmartFolderStoring { return try await s.storeUpdateSmartFolder(folder, in: session) }
        try await LocalSmartFolders.shared.update(session.id, folder)
    }

    public func deleteSmartFolder(_ folder: SmartFolder, in session: SessionInfo) async throws {
        guard !folder.isBuiltin else { throw SmartFolderError.builtin }
        if let s = self as? any SmartFolderStoring { return try await s.storeDeleteSmartFolder(folder.id, in: session) }
        await LocalSmartFolders.shared.delete(session.id, folder.id)
    }

    public func setSmartFolderMirror(_ folder: SmartFolder, on: Bool, in session: SessionInfo) async throws {
        if let s = self as? any SmartFolderStoring { return try await s.storeSetMirror(folder.id, on: on, in: session) }
        await LocalSmartFolders.shared.setMirror(session.id, folder.id, on: on)
    }

    /// Photos matching the index-only parts of `criteria` (keywords/text), or
    /// `nil` when the store can't answer (then those parts are ignored).
    public func indexMatches(_ criteria: SmartCriteria, in session: SessionInfo) async throws -> Set<PhotoID>? {
        guard criteria.needsIndex, let s = self as? any SmartFolderStoring else { return nil }
        return try await s.storeQuery(criteria.indexPart, in: session)
    }

    /// Where "Mirror to Finder" folders for `session` go (`nil` = not available).
    public func smartFolderMirrorRoot(for session: SessionInfo) async -> URL? {
        guard let s = self as? any SmartFolderStoring else { return nil }
        return await s.storeMirrorRoot(for: session)
    }
}

/// Implemented by stores that persist smart folders in the session.
public protocol SmartFolderStoring: SessionStore {
    func storedSmartFolders(in session: SessionInfo) async throws -> [SmartFolder]
    func storeCreateSmartFolder(named name: String, criteria: SmartCriteria, in session: SessionInfo) async throws -> SmartFolder
    func storeUpdateSmartFolder(_ folder: SmartFolder, in session: SessionInfo) async throws
    func storeDeleteSmartFolder(_ id: Int64, in session: SessionInfo) async throws
    func storeSetMirror(_ id: Int64, on: Bool, in session: SessionInfo) async throws
    func storeQuery(_ criteria: SmartCriteria, in session: SessionInfo) async throws -> Set<PhotoID>
    func storeMirrorRoot(for session: SessionInfo) async -> URL?
}

public enum SmartFolderError: Error, Equatable, LocalizedError {
    case builtin
    case emptyName
    case notFound

    public var errorDescription: String? {
        switch self {
        case .builtin: "Built-in smart folders can't be changed or deleted."
        case .emptyName: "Give the smart folder a name."
        case .notFound: "That smart folder no longer exists."
        }
    }
}

enum SmartFolderNames {
    static func validated(_ name: String) throws -> String {
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !n.isEmpty else { throw SmartFolderError.emptyName }
        return n
    }
}

/// In-memory smart folders for stores without a session index (not persisted).
actor LocalSmartFolders {
    static let shared = LocalSmartFolders()
    private var user: [UUID: [SmartFolder]] = [:]
    private var mirrors: [UUID: Set<Int64>] = [:]
    private var nextID: Int64 = 1

    func folders(_ session: UUID, tagsInUse: Set<ColorTag>) -> [SmartFolder] {
        let on = mirrors[session] ?? []
        return (SmartFolder.builtins(tagsInUse: tagsInUse) + (user[session] ?? [])).map { f in
            var f = f
            f.mirror = on.contains(f.id)
            return f
        }
    }

    func create(_ session: UUID, name: String, criteria: SmartCriteria) -> SmartFolder {
        let f = SmartFolder(id: nextID, name: name, criteria: criteria)
        nextID += 1
        user[session, default: []].append(f)
        return f
    }

    func update(_ session: UUID, _ folder: SmartFolder) throws {
        guard let i = user[session]?.firstIndex(where: { $0.id == folder.id }) else { throw SmartFolderError.notFound }
        user[session]![i] = folder
    }

    func delete(_ session: UUID, _ id: Int64) {
        user[session]?.removeAll { $0.id == id }
        mirrors[session]?.remove(id)
    }

    func setMirror(_ session: UUID, _ id: Int64, on: Bool) {
        if on { mirrors[session, default: []].insert(id) } else { mirrors[session]?.remove(id) }
    }
}
