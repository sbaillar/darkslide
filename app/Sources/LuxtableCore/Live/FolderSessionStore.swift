import CoreGraphics
import CryptoKit
import Foundation

/// Live-mode session store until `lt-session` lands: a "session" is a folder of
/// RAW files opened read-only. Metadata comes from lt-raw (via the injected
/// loader). Ratings, tags, picks and edits are kept **in memory only**
/// (`persistsChanges == false`), and the UI says so.
public actor FolderSessionStore: SessionStore {
    public typealias Loader = @Sendable (URL, PhotoID) async throws -> Photo

    private let loader: Loader
    private let isSupported: @Sendable (URL) -> Bool
    private let recents: RecentFolders
    private let concurrency: Int
    private var sessions: [SessionInfo] = []
    private var photosBySession: [UUID: [Photo]] = [:]
    private var edits: [PhotoID: EditState] = [:]
    private var optimizeReports: [PhotoID: OptimizeReport] = [:]
    private var storedAIChanges: [PhotoID: String] = [:]
    private var nextID: Int64 = 1
    /// One stable id per file for the life of the store (N6b).
    private var idByURL: [URL: PhotoID] = [:]
    /// Files that could not be read in the last `photos(in:)` call.
    public private(set) var lastFailures: [URL] = []
    private var stale: Set<UUID> = []

    public init(recents: RecentFolders, concurrency: Int = 8,
                isSupported: @escaping @Sendable (URL) -> Bool, loader: @escaping Loader) {
        self.recents = recents
        self.concurrency = max(1, concurrency)
        self.isSupported = isSupported
        self.loader = loader
    }

    public nonisolated var persistsChanges: Bool { false }

    /// Stable id for a folder path, so a reopened folder keeps its identity.
    static func sessionID(for url: URL) -> UUID {
        let d = SHA256.hash(data: Data(url.standardizedFileURL.path.utf8))
        let b = Array(d.prefix(16))
        return UUID(uuid: (b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]))
    }

    /// Supported files directly in `folder` or in `DCIM/*/` below it, sorted by name.
    func files(in folder: URL) -> [URL] {
        let fm = FileManager.default
        var dirs = [folder]
        let dcim = folder.appendingPathComponent("DCIM")
        if let subs = try? fm.contentsOfDirectory(at: dcim, includingPropertiesForKeys: [.isDirectoryKey]) {
            dirs += subs.filter { (try? $0.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true }
        }
        return dirs.flatMap { dir in
            ((try? fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil, options: [.skipsHiddenFiles])) ?? [])
                .filter(isSupported)
        }
        .sorted { $0.lastPathComponent < $1.lastPathComponent }
    }

    /// Open (or re-open) a folder as a session and remember it.
    public func register(folder: URL) throws -> SessionInfo {
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: folder.path, isDirectory: &isDir), isDir.boolValue else {
            throw MockError.notFound(folder.path)
        }
        let id = Self.sessionID(for: folder)
        let count = files(in: folder).count
        let info = SessionInfo(id: id, name: folder.lastPathComponent, url: folder, photoCount: count, createdAt: Date())
        sessions.removeAll { $0.id == id }
        sessions.insert(info, at: 0)
        // Keep what's loaded (ratings/edits live here); new files are merged on the next photos(in:).
        stale.insert(id)
        recents.add(folder)
        return info
    }

    public func listSessions() async throws -> [SessionInfo] {
        if sessions.isEmpty {
            for url in recents.folders.reversed() where FileManager.default.fileExists(atPath: url.path) {
                _ = try? register(folder: url)
            }
        }
        return sessions
    }

    public func photos(in session: SessionInfo) async throws -> [Photo] {
        let previous = photosBySession[session.id]
        if let previous, !stale.contains(session.id) { return previous }
        stale.remove(session.id)
        let known = Dictionary((previous ?? []).compactMap { p in p.url.map { ($0.standardizedFileURL, p) } },
                               uniquingKeysWith: { a, _ in a })
        let all = files(in: session.url)
        let urls = all.filter { known[$0.standardizedFileURL] == nil }
        var ids: [PhotoID] = []
        for u in urls {
            let key = u.standardizedFileURL
            if let id = idByURL[key] { ids.append(id); continue }
            let id = PhotoID(nextID)
            nextID += 1
            idByURL[key] = id
            ids.append(id)
        }
        let loader = loader
        let limit = concurrency
        var loaded: [Photo] = []
        var failed: [URL] = []
        try await withThrowingTaskGroup(of: (URL, Photo?).self) { group in
            var next = 0
            func addNext() {
                guard next < urls.count else { return }
                let (id, url) = (ids[next], urls[next])
                next += 1
                group.addTask { (url, try? await loader(url, id)) }
            }
            for _ in 0..<limit { addNext() }
            while let (url, photo) = try await group.next() {
                try Task.checkCancellation()
                if let photo { loaded.append(photo) } else { failed.append(url) }
                addNext()
            }
        }
        lastFailures = failed
        // Files still on disk keep their loaded state (ratings, tags, picks).
        let present = Set(all.map(\.standardizedFileURL))
        loaded += known.values.filter { present.contains($0.url!.standardizedFileURL) }
        loaded.sort { ($0.meta.captureDate, $0.filename) < ($1.meta.captureDate, $1.filename) }
        photosBySession[session.id] = loaded
        return loaded
    }

    public func albums(in session: SessionInfo) async throws -> [Album] { [] }

    public func createSession(named name: String) async throws -> SessionInfo {
        throw MockError.invalid("New sessions aren't available here. Open a folder instead.")
    }

    public func save(photos updated: [Photo]) async throws {
        for p in updated {
            for (sid, list) in photosBySession {
                if let i = list.firstIndex(where: { $0.id == p.id }) { photosBySession[sid]![i] = p }
            }
        }
    }

    public func edit(for id: PhotoID) async throws -> EditState { edits[id] ?? EditState() }

    public func save(edits updated: [PhotoID: EditState]) async throws {
        edits.merge(updated) { _, new in new }
    }
}

/// Recently opened folders (most recent first), persisted in UserDefaults.
public final class RecentFolders: @unchecked Sendable {
    private let defaults: UserDefaults
    private let key = "recentFolders"
    private let lock = NSLock()
    public let limit: Int

    public init(defaults: UserDefaults = .standard, limit: Int = 12) {
        self.defaults = defaults
        self.limit = limit
    }

    private let bookmarksKey = "recentFolderBookmarks"
    /// Where the list lives (a relaunch in tests reads the same preferences).
    var defaultsForTesting: UserDefaults { defaults }

    /// One entry per folder: keyed by canonical path (symlinks resolved, standardized),
    /// so older duplicate spellings of the same folder collapse (L12). A folder that
    /// was renamed or moved in Finder is followed through its bookmark, so it stays
    /// in the Sessions list across relaunch (final-journeys H1).
    public var folders: [URL] {
        lock.withLock {
            var seen: Set<String> = []
            var list = defaults.stringArray(forKey: key) ?? []
            var marks = defaults.dictionary(forKey: bookmarksKey) as? [String: Data] ?? [:]
            var changed = false
            let out: [URL] = list.indices.compactMap { i in
                var url = Self.canonical(URL(fileURLWithPath: list[i]))
                if !FileManager.default.fileExists(atPath: url.path),
                   let followed = Self.follow(marks[url.path]), followed.path != url.path {
                    marks[followed.path] = marks.removeValue(forKey: url.path)
                    self.moveCount(from: url.path, to: followed.path)
                    url = followed
                    list[i] = followed.path
                    changed = true
                }
                return seen.insert(url.path).inserted ? url : nil
            }
            if changed {
                defaults.set(list, forKey: key)
                defaults.set(marks, forKey: bookmarksKey)
            }
            return out
        }
    }

    public func add(_ url: URL) {
        let canonical = Self.canonical(url)
        let path = canonical.path
        let mark = try? canonical.bookmarkData()
        lock.withLock {
            var list = defaults.stringArray(forKey: key) ?? []
            list.removeAll { Self.canonical(URL(fileURLWithPath: $0)).path == path }
            list.insert(path, at: 0)
            let kept = Array(list.prefix(limit))
            defaults.set(kept, forKey: key)
            var marks = defaults.dictionary(forKey: bookmarksKey) as? [String: Data] ?? [:]
            if let mark { marks[path] = mark }
            marks = marks.filter { kept.contains($0.key) }
            defaults.set(marks, forKey: bookmarksKey)
            var opened = defaults.dictionary(forKey: openedKey) ?? [:]
            opened[path] = now().timeIntervalSince1970
            defaults.set(opened.filter { kept.contains($0.key) }, forKey: openedKey)
        }
    }

    /// A folder that was renamed or moved while open: the new location takes the
    /// old one's place in the list (and its photo count).
    public func replace(_ old: URL, with new: URL) {
        let (from, to) = (Self.canonical(old).path, Self.canonical(new))
        let mark = try? to.bookmarkData()
        lock.withLock {
            var list = defaults.stringArray(forKey: key) ?? []
            if let i = list.firstIndex(where: { Self.canonical(URL(fileURLWithPath: $0)).path == from }) {
                list[i] = to.path
            } else {
                list.insert(to.path, at: 0)
            }
            var seen: Set<String> = []
            list = list.filter { seen.insert(Self.canonical(URL(fileURLWithPath: $0)).path).inserted }
            defaults.set(Array(list.prefix(limit)), forKey: key)
            var marks = defaults.dictionary(forKey: bookmarksKey) as? [String: Data] ?? [:]
            marks[from] = nil
            if let mark { marks[to.path] = mark }
            defaults.set(marks, forKey: bookmarksKey)
            self.moveCount(from: from, to: to.path)
        }
    }

    /// Where a bookmark points now, if that folder exists.
    static func follow(_ bookmark: Data?) -> URL? {
        guard let bookmark else { return nil }
        var stale = false
        guard let url = try? URL(resolvingBookmarkData: bookmark, options: [.withoutUI, .withoutMounting],
                                 bookmarkDataIsStale: &stale) else { return nil }
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: url.path, isDirectory: &isDir), isDir.boolValue else { return nil }
        return canonical(url)
    }

    /// Caller holds `lock`.
    private func moveCount(from: String, to: String) {
        for k in [countsKey, openedKey] {
            var all = defaults.dictionary(forKey: k) ?? [:]
            if let n = all.removeValue(forKey: from) { all[to] = n }
            defaults.set(all, forKey: k)
        }
    }

    /// "Remove from list": forget a folder (never touches the folder itself).
    public func remove(_ url: URL) {
        let path = Self.canonical(url).path
        lock.withLock {
            var list = defaults.stringArray(forKey: key) ?? []
            list.removeAll { Self.canonical(URL(fileURLWithPath: $0)).path == path }
            defaults.set(list, forKey: key)
            var marks = defaults.dictionary(forKey: bookmarksKey) as? [String: Data] ?? [:]
            marks[path] = nil
            defaults.set(marks, forKey: bookmarksKey)
        }
    }

    /// File ▸ Open Recent ▸ Clear Menu: forget every folder but `keeping` (the open
    /// session, unfinished imports). Only the list goes; no folder or session data is touched.
    public func clear(keeping: [URL] = []) {
        let kept = Set(keeping.map { Self.canonical($0).path })
        lock.withLock {
            let list = (defaults.stringArray(forKey: key) ?? []).filter { kept.contains(Self.canonical(URL(fileURLWithPath: $0)).path) }
            defaults.set(list, forKey: key)
            for k in [bookmarksKey, openedKey] {
                defaults.set((defaults.dictionary(forKey: k) ?? [:]).filter { kept.contains($0.key) }, forKey: k)
            }
        }
    }

    /// `url` was opened now (a listed folder only).
    public func stampOpened(_ url: URL) {
        let path = Self.canonical(url).path
        lock.withLock {
            guard (defaults.stringArray(forKey: key) ?? []).contains(where: { Self.canonical(URL(fileURLWithPath: $0)).path == path })
            else { return }
            var opened = defaults.dictionary(forKey: openedKey) ?? [:]
            opened[path] = now().timeIntervalSince1970
            defaults.set(opened, forKey: openedKey)
        }
    }

    static func canonical(_ url: URL) -> URL { url.standardizedFileURL.resolvingSymlinksInPath() }

    private let openedKey = "recentFolderOpened"
    /// The clock `add` stamps (tests replace it).
    var now: @Sendable () -> Date = { Date() }

    /// When `url` was last opened (nil: before this was recorded).
    public func lastOpened(_ url: URL) -> Date? {
        lock.withLock {
            (defaults.dictionary(forKey: openedKey)?[Self.canonical(url).path] as? Double).map(Date.init(timeIntervalSince1970:))
        }
    }

    private let countsKey = "recentFolderCounts"

    /// Photos in `url` when it was last opened (the Sessions list shows it
    /// without opening the folder); 0 if never counted.
    public func photoCount(_ url: URL) -> Int {
        lock.withLock { (defaults.dictionary(forKey: countsKey)?[Self.canonical(url).path] as? Int) ?? 0 }
    }

    public func setPhotoCount(_ count: Int, for url: URL) {
        lock.withLock {
            var all = defaults.dictionary(forKey: countsKey) ?? [:]
            all[Self.canonical(url).path] = count
            defaults.set(all, forKey: countsKey)
        }
    }
}

/// Thumbnails from embedded RAW previews (live mode).
public struct PreviewThumbnailProvider: ImageProvider {
    let previews: any PreviewProvider

    public init(previews: any PreviewProvider) { self.previews = previews }

    public func thumbnail(for photo: Photo, maxPixelSize: Int) async throws -> CGImage {
        guard let url = photo.url else { throw MockError.notFound("file for \(photo.filename)") }
        return try await previews.preview(for: url, maxPixelSize: maxPixelSize)
    }
}

/// Live mode before `lt-import`: lists what's on a card (read-only) and refuses to copy.
public struct ReadOnlyCardScanner: ImportService {
    public static let extensions: Set<String> = ["arw", "cr3", "cr2", "nef", "raf", "dng", "orf", "rw2", "jpg", "jpeg", "heic"]

    public init() {}

    public func scan(volume: URL) async throws -> [ImportCandidate] {
        try Self.list(volume)
    }

    /// Synchronous walk (the async caller runs it off the main actor).
    static func list(_ volume: URL) throws -> [ImportCandidate] {
        let fm = FileManager.default
        let root = fm.fileExists(atPath: volume.appendingPathComponent("DCIM").path) ? volume.appendingPathComponent("DCIM") : volume
        guard let e = fm.enumerator(at: root, includingPropertiesForKeys: [.fileSizeKey, .contentModificationDateKey],
                                    options: [.skipsHiddenFiles]) else { return [] }
        var out: [ImportCandidate] = []
        for case let url as URL in e where Self.extensions.contains(url.pathExtension.lowercased()) {
            try Task.checkCancellation()
            let v = try? url.resourceValues(forKeys: [.fileSizeKey, .contentModificationDateKey])
            out.append(ImportCandidate(url: url, size: Int64(v?.fileSize ?? 0),
                                       captureDate: v?.contentModificationDate ?? Date(), alreadyImported: false))
        }
        return out.sorted { $0.url.path < $1.url.path }
    }

    public func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> {
        AsyncThrowingStream { $0.finish(throwing: ImportUnavailable()) }
    }
}

public struct ImportUnavailable: Error, LocalizedError, Equatable {
    public init() {}
    public var errorDescription: String? {
        "Verified import isn't available here. Luxtable doesn't copy from cards unless it can verify every file."
    }
}

/// An archive that isn't connected (tests, stores without lt-archive): says so honestly.
public struct UnavailableArchiveService: ArchiveService {
    public let target: String
    public init(target: String) { self.target = target }

    public func statusUpdates() -> AsyncStream<ArchiveStatus> {
        AsyncStream { c in
            c.yield(ArchiveStatus(state: .unavailable("Archiving isn't connected here; nothing is archived."),
                                  target: target, lastSuccess: nil))
            c.finish()
        }
    }

    public func syncNow() async {}
}

extension FolderSessionStore: RecentSessionsEditing {
    public func removeFromRecents(_ session: SessionInfo) async { recents.remove(session.url) }
    public func clearRecents(keeping sessions: [SessionInfo]) async { recents.clear(keeping: sessions.map(\.url)) }
    public func noteOpened(_ session: SessionInfo) async { recents.stampOpened(session.url) }
}

// MARK: - AI Optimize reports (in memory)

extension FolderSessionStore: OptimizeReportStoring {
    public func optimizeReport(for id: PhotoID) async throws -> OptimizeReport? { optimizeReports[id] }

    public func saveOptimizeReport(_ report: OptimizeReport?, for ids: [PhotoID]) async throws {
        for id in ids { optimizeReports[id] = report }
    }
}

// MARK: - AI changes (in memory)

extension FolderSessionStore: AIChangesStoring {
    public func aiChanges(for id: PhotoID) async throws -> String? { storedAIChanges[id] }

    public func saveAIChanges(_ json: String?, for ids: [PhotoID]) async throws {
        for id in ids { storedAIChanges[id] = json }
    }
}
