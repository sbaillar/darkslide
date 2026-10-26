import Foundation

/// The frame the user chose to stand for a burst (Use as Cover), with the burst's
/// members when it was chosen. Kept with the session (`.lt/settings/stacks.json`),
/// so every Mac opening the session shows the same cover.
public struct StackCover: Codable, Equatable, Sendable {
    /// The burst's frames when the cover was chosen (variant ids, sorted).
    public var members: [PhotoID]
    public var cover: PhotoID

    public init(members: [PhotoID], cover: PhotoID) {
        self.members = members.sorted()
        self.cover = cover
    }

    private enum CodingKeys: String, CodingKey { case members, cover }

    // Plain variant ids in the file.
    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(members: try c.decode([Int64].self, forKey: .members).map(PhotoID.init),
                  cover: PhotoID(try c.decode(Int64.self, forKey: .cover)))
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(members.map(\.raw), forKey: .members)
        try c.encode(cover.raw, forKey: .cover)
    }
}

/// Matching chosen covers to stacks that are recomputed from metadata (an import, a
/// delete or a filter can change a burst's frames). Pure.
public enum StackCovers {
    /// The chosen cover of a stack of `members`: a choice whose cover is still one of
    /// them. When several are (two bursts became one), the one whose members overlap
    /// most wins, then the latest. `nil`: the usual rule picks the top.
    public static func cover(for members: [PhotoID], in covers: [StackCover]) -> PhotoID? {
        let set = Set(members)
        var best: (cover: PhotoID, overlap: Int)?
        for c in covers where set.contains(c.cover) {
            let overlap = c.members.reduce(0) { $0 + (set.contains($1) ? 1 : 0) }
            if overlap >= (best?.overlap ?? 0) { best = (c.cover, overlap) } // later entries win ties
        }
        return best?.cover
    }

    /// `covers` with `cover` chosen for the stack of `members`: earlier choices for any
    /// of its frames go, the new one goes last.
    public static func choosing(_ cover: PhotoID, members: [PhotoID], in covers: [StackCover]) -> [StackCover] {
        let set = Set(members)
        return covers.filter { !set.contains($0.cover) && $0.members.allSatisfy { !set.contains($0) } }
            + [StackCover(members: members, cover: cover)]
    }
}

/// `stacks.json`: the session's chosen covers.
public enum StackCoverFile {
    /// Where a session store keeps it, next to lt-session's `albums.json`.
    public static func url(inStore root: URL) -> URL {
        root.appendingPathComponent(".lt/settings/stacks.json")
    }

    private struct Contents: Codable {
        var version = 1
        var stacks: [StackCover] = []
    }

    /// The covers in `url`; none when the file isn't there. Throws when it can't be read.
    public static func read(_ url: URL) throws -> [StackCover] {
        guard FileManager.default.fileExists(atPath: url.path) else { return [] }
        return try JSONDecoder().decode(Contents.self, from: Data(contentsOf: url)).stacks
    }

    /// Replace `url` atomically: a temporary file next to it, flushed to disk, then renamed over it.
    public static func write(_ covers: [StackCover], to url: URL) throws {
        let enc = JSONEncoder()
        enc.outputFormatting = [.sortedKeys, .prettyPrinted]
        try AtomicFile.write(try enc.encode(Contents(stacks: covers)), to: url)
    }
}

/// Temp file → fsync → rename, in the destination's folder (which must exist).
enum AtomicFile {
    static func write(_ data: Data, to url: URL) throws {
        let tmp = url.deletingLastPathComponent().appendingPathComponent(".\(url.lastPathComponent).\(UUID().uuidString).tmp")
        guard FileManager.default.createFile(atPath: tmp.path, contents: nil) else {
            throw CocoaError(.fileWriteUnknown, userInfo: [NSFilePathErrorKey: url.path])
        }
        do {
            let h = try FileHandle(forWritingTo: tmp)
            defer { try? h.close() }
            try h.write(contentsOf: data)
            try h.synchronize()
            guard rename(tmp.path, url.path) == 0 else {
                throw CocoaError(.fileWriteUnknown, userInfo: [NSFilePathErrorKey: url.path,
                                                               NSUnderlyingErrorKey: POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)])
            }
        } catch {
            try? FileManager.default.removeItem(at: tmp)
            throw error
        }
    }
}

/// Stores that keep chosen covers with the session (`RustSessionStore`: `stacks.json`
/// in the session's store folder).
public protocol StackCoverStoring: SessionStore {
    func storedStackCovers(in session: SessionInfo) async throws -> [StackCover]
    func storeStackCovers(_ covers: [StackCover], in session: SessionInfo) async throws
}

extension SessionStore {
    /// The chosen covers of `session`. Stores without a session folder (demo, the
    /// read-only folder store) keep them in memory for the run.
    public func stackCovers(in session: SessionInfo) async throws -> [StackCover] {
        if let s = self as? any StackCoverStoring { return try await s.storedStackCovers(in: session) }
        return await LocalStackCovers.shared.covers[session.id] ?? []
    }

    public func saveStackCovers(_ covers: [StackCover], in session: SessionInfo) async throws {
        if let s = self as? any StackCoverStoring { return try await s.storeStackCovers(covers, in: session) }
        await LocalStackCovers.shared.set(covers, for: session.id)
    }
}

/// In-memory covers for stores without a session folder (not persisted).
actor LocalStackCovers {
    static let shared = LocalStackCovers()
    private(set) var covers: [UUID: [StackCover]] = [:]

    func set(_ c: [StackCover], for session: UUID) { covers[session] = c }
}
