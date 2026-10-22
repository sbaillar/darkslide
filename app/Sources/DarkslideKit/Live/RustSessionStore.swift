import CoreGraphics
import CryptoKit
import DarkslideFFI
import Foundation

/// Stores that can open a plain folder of photos as a session (live mode).
/// First-open progress (stat/register, then background hashing).
public struct OpenProgress: Sendable, Equatable {
    /// `updated`: the background pass changed the photo list (relinked, changed
    /// or new photos): list the session again.
    public enum Phase: String, Sendable { case scanning, registering, hashing, updated }
    public var phase: Phase
    public var done: Int
    public var total: Int
    public init(phase: Phase, done: Int, total: Int) {
        self.phase = phase
        self.done = done
        self.total = total
    }
    public var fraction: Double { total == 0 ? 0 : Double(done) / Double(total) }
}

public protocol FolderSessionOpening: SessionStore {
    /// Open `folder` as a session. `progress` is called off the main actor.
    /// Cancelling the calling task cancels the open (nothing half-written is left behind).
    func add(folder: URL, progress: @escaping @Sendable (OpenProgress) -> Void) async throws -> SessionInfo
    /// Files the last open couldn't read.
    var lastFailures: [URL] { get async }
    /// Forget missing originals (their sidecars/index entries only; never touches files).
    func removeMissing(_ ids: [PhotoID]) async throws
    /// Point a missing photo at its new location (must be the same file by hash).
    func locate(_ id: PhotoID, at url: URL) async throws
    /// "File changed": put back the ratings, tags and edits of the photo this
    /// file replaced (they now describe this file).
    func restorePrevious(_ id: PhotoID) async throws
    /// "File changed": keep it as the new photo it is (clears the badge).
    func dismissFileChanged(_ id: PhotoID) async throws
    /// A missing photo whose only copy is still inside the store (an old
    /// session's migration kept it): move it back into the folder.
    func restoreToFolder(_ id: PhotoID) async throws
    /// "Locate Folder…": the session's folder is now at `folder` (moved, copied, or its
    /// drive mounted elsewhere): reopen it there with the same store, so every rating
    /// and edit carries over. Returns the session at its new place.
    func relocate(_ session: SessionInfo, to folder: URL) async throws -> SessionInfo
}

/// A session's folder was renamed or moved in Finder while open, and the store
/// followed it (final-journeys H1, M3): `to` is the session at its new place.
public struct SessionMove: Sendable, Equatable {
    public let from: URL
    public let to: SessionInfo
    public init(from: URL, to: SessionInfo) {
        self.from = from
        self.to = to
    }
}

/// Stores that follow a session folder renamed while open and say so.
public protocol SessionMoveReporting: Sendable {
    func onSessionMoved(_ handler: @escaping @Sendable (SessionMove) -> Void) async
}

/// Stores that pause background work while the user is culling (R4-M3).
public protocol UserActivityObserving: Sendable {
    /// The user just navigated or rated: background hashing waits until idle.
    func noteUserActivity()
}

extension FolderSessionOpening {
    /// `add(folder:progress:)` without progress reporting.
    public func add(folder: URL) async throws -> SessionInfo {
        try await add(folder: folder, progress: { _ in })
    }
}

extension FolderSessionStore: FolderSessionOpening {
    public func add(folder: URL, progress: @escaping @Sendable (OpenProgress) -> Void) async throws -> SessionInfo {
        try register(folder: folder)
    }
    public func removeMissing(_ ids: [PhotoID]) async throws {}
    public func locate(_ id: PhotoID, at url: URL) async throws {
        throw MockError.invalid("Locate needs a saved session")
    }
    public func restorePrevious(_ id: PhotoID) async throws {
        throw MockError.invalid("Restoring needs a saved session")
    }
    public func dismissFileChanged(_ id: PhotoID) async throws {}
    public func restoreToFolder(_ id: PhotoID) async throws {
        throw MockError.invalid("Restoring needs a saved session")
    }
    public func relocate(_ session: SessionInfo, to folder: URL) async throws -> SessionInfo {
        try register(folder: folder)
    }
}

/// Live `SessionStore` over `ds-session` through the FFI (N5/N6).
///
/// A session here is a folder opened **in place**: the photos stay the user's
/// files and are only read. The index and sidecars live in
/// `<folder>/.ds/session/` on local writable disks, and otherwise (memory
/// cards, read-only media, network shares) under `storeBase/<volume
/// key>/<path hash>/` (volume key: its UUID, or for network shares the mount
/// source, R4-H1), with nothing written to the volume (see `docs/FFI.md`).
/// Sidecars are the source of truth: ratings, tags, picks and edits are
/// written through Rust and survive reopen and relaunch; `PhotoID` is the
/// stable ds-session variant id (unique per session, so caches are keyed by
/// session and photo).
///
/// Edits cross as `ds_core::Adjustments` JSON via `AdjustmentsCodec`, overlaid
/// on the JSON last read so fields the app doesn't model are preserved.
public actor RustSessionStore: CurrentSessionPointing, FolderSessionOpening, UserActivityObserving, SessionMoveReporting, SessionCreating,
    SessionClosing, DerivedPhotoAdding {
    /// The preset to apply to photos when a folder open first registers them (L1).
    public typealias AutoApplyProvider = @Sendable () async -> Preset?

    struct Key: Hashable {
        let session: UUID
        let photo: PhotoID
    }

    private let recents: RecentFolders
    private let storeBase: URL
    private let autoApply: AutoApplyProvider?
    /// Where new empty sessions go (New Session…); `nil`: this store can't make them.
    private let libraryRoot: RustImportService.LibraryRootProvider?
    private var handles: [UUID: SessionHandle] = [:]
    private var folders: [UUID: URL] = [:]
    private var bookmarks: [UUID: Data] = [:]
    private var stores: [UUID: URL] = [:]
    private var hashing: [UUID: Task<Void, Never>] = [:]
    /// Photos listed per session, and the session listed last (resolves a bare `PhotoID`).
    private var members: [UUID: Set<PhotoID>] = [:]
    /// Photos Remove Missing forgot, per session: a late write for one is skipped.
    private var removed: [UUID: Set<PhotoID>] = [:]
    /// Photos of sessions closed since they were listed: a late write fails loudly.
    private var closedPhotos: Set<PhotoID> = []
    /// How long closing waits for a cancelled identity pass (a hung network volume
    /// may never let it return).
    private var passWaitLimit: Duration = .seconds(2)
    private var current: UUID?
    /// Last known raw adjustments JSON object per photo (for lossless overlay).
    private var raw: [Key: AdjustmentsCodec.JSON] = [:]
    /// [remote] Per session: the device-write generation of the last edits snapshot.
    private var deviceGenerations: [UUID: UInt64] = [:]
    private var edits: [Key: EditState] = [:]
    /// What Rust has, as the app sees it (to write only real changes).
    private var stored: [Key: Photo] = [:]
    public private(set) var lastFailures: [URL] = []
    private var moveHandler: (@Sendable (SessionMove) -> Void)?

    public func onSessionMoved(_ handler: @escaping @Sendable (SessionMove) -> Void) {
        moveHandler = handler
    }

    /// `storeBase`: where sessions go for folders whose volume can't hold them.
    public init(recents: RecentFolders, storeBase: URL = RustSessionStore.defaultStoreBase,
                autoApply: AutoApplyProvider? = nil,
                libraryRoot: RustImportService.LibraryRootProvider? = nil) {
        self.recents = recents
        self.storeBase = storeBase
        self.autoApply = autoApply
        self.libraryRoot = libraryRoot
    }

    public nonisolated var persistsChanges: Bool { true }

    /// `<AppPaths.support>/InPlace`.
    public static var defaultStoreBase: URL { AppPaths.support.appendingPathComponent("InPlace", isDirectory: true) }

    /// What identifies the volume `folder` is on (R4-H1): its UUID, else (network
    /// shares and cards without one) a hash of what is mounted and its file
    /// system type (`smbfs //user@nas/pics`), so two shares never share a key.
    public static func volumeKey(for folder: URL) -> String {
        if let uuid = try? folder.resourceValues(forKeys: [.volumeUUIDStringKey]).volumeUUIDString { return uuid }
        return "vol-" + shortHash((try? volumeIdentity(folder: folder.path)) ?? "unknown")
    }

    /// `folder`'s path on its volume (so a card is found whatever it is mounted as).
    static func pathOnVolume(_ folder: URL) -> String {
        var path = folder.standardizedFileURL.path
        if let root = (try? folder.resourceValues(forKeys: [.volumeURLKey]))?.volume?.standardizedFileURL.path,
           root != "/", path.hasPrefix(root) {
            path = String(path.dropFirst(root.count))
        }
        return path
    }

    static func shortHash(_ s: String) -> String {
        SHA256.hash(data: Data(s.utf8)).prefix(12).map { String(format: "%02x", $0) }.joined()
    }

    /// The identity Rust records in the store: volume key plus path on the
    /// volume. A store made for another folder is never taken over.
    static func folderIdentity(for folder: URL, volumeKey: (URL) -> String = volumeKey(for:)) -> String {
        "\(volumeKey(folder))|\(pathOnVolume(folder))"
    }

    /// The store for `folder` when its volume can't hold one: keyed by the
    /// volume key and the folder's path on that volume.
    static func fallbackStore(for folder: URL, base: URL, volumeKey: (URL) -> String = volumeKey(for:)) -> URL {
        base.appendingPathComponent(volumeKey(folder), isDirectory: true)
            .appendingPathComponent(shortHash(pathOnVolume(folder)), isDirectory: true)
    }

    /// Pause background hashing while the user culls (R4-M3).
    public nonisolated func noteUserActivity() {
        try? DarkslideFFI.noteUserActivity()
    }

    // MARK: - Sessions

    public func add(folder: URL, progress: @escaping @Sendable (OpenProgress) -> Void) async throws -> SessionInfo {
        let url = folder.standardizedFileURL.resolvingSymlinksInPath()
        let id = FolderSessionStore.sessionID(for: url)
        let handle = try await open(id, at: url, store: stores[id] ?? Self.fallbackStore(for: url, base: storeBase),
                                    moved: false, progress: progress)
        let report = try handle.openReport()
        lastFailures = report.failed.map { f in
            let rel = String(f.prefix { $0 != ":" })
            return url.appendingPathComponent(rel.hasPrefix("Capture/") ? String(rel.dropFirst(8)) : rel)
        }
        recents.add(url)
        let count = Int(report.added + report.existing)
        recents.setPhotoCount(count, for: url)
        return SessionInfo(id: id, name: url.lastPathComponent, url: url, photoCount: count, createdAt: Date())
    }

    private func open(_ id: UUID, at url: URL, store: URL, moved: Bool,
                      progress: @escaping @Sendable (OpenProgress) -> Void) async throws -> SessionHandle {
        // [imp6] A session made by import (Internal originals under Capture/): open it as is.
        if RustImportService.isImportSession(url) {
            let handle = try await Self.call { token in try await openSession(dir: url.path, token: token) }
            handles[id] = handle
            folders[id] = url
            bookmarks[id] = try? url.bookmarkData()
            return handle
        }
        let preset = try await autoApply?().map(Self.autoApplyRecord)
        let relay = ProgressRelay(progress)
        let identity = Self.folderIdentity(for: url)
        let handle = try await Self.call { token in
            try await openFolderInPlace(folder: url.path, fallbackStore: store.path, folderIdentity: identity,
                                        moved: moved, autoApply: preset, listener: relay, token: token)
        }
        handles[id] = handle
        folders[id] = url
        stores[id] = store
        bookmarks[id] = try? url.bookmarkData()
        hashing[id]?.cancel()
        hashing[id] = Task {
            // Background identity pass (R4-M1): photos are already listed; when it
            // changed the list (relinked, changed or new photos) the app lists again.
            let report = try? await Self.call { token in try await handle.hashPending(listener: relay, token: token) }
            if let report, report.listingChanged {
                progress(OpenProgress(phase: .updated,
                                      done: Int(report.changed + report.relinked + report.registered), total: 0))
            }
        }
        return handle
    }

    /// "Locate Folder…": `session`'s originals are now in `folder` (moved, copied, or
    /// its drive mounted elsewhere). Its store comes along, so every rating and edit
    /// carries over: an app-owned store (cards, network shares) is reopened for the new
    /// folder; one inside the old folder (`<old>/.ds/session`, left behind when only the
    /// photos moved) is **copied** next to the photos (the original stays where it was).
    /// A folder that already has its own session just opens with it.
    public func relocate(_ session: SessionInfo, to folder: URL) async throws -> SessionInfo {
        let url = RecentFolders.canonical(folder)
        let old = folders[session.id] ?? session.url
        // An imported session is its own folder: it simply opens where it is now.
        let oldStore = RustImportService.isImportSession(url) ? nil : existingStore(of: session.id, folder: old)
        let target = (try? folderCanHoldSession(folder: url.path)) == true
            ? url.appendingPathComponent(Self.inPlaceDir, isDirectory: true)
            : Self.fallbackStore(for: url, base: storeBase)
        var store = target
        var copied: URL?
        if let oldStore, !Self.hasSession(url.appendingPathComponent(Self.inPlaceDir)) {
            if Self.isInside(oldStore, old) {
                if !Self.hasSession(target) {
                    try Self.copyStore(oldStore, to: target)
                    copied = target
                }
            } else {
                store = oldStore // app-owned: reopened where it is, for the new folder
            }
        }
        let newID = FolderSessionStore.sessionID(for: url)
        let handle: SessionHandle
        do {
            handle = try await open(newID, at: url, store: store, moved: oldStore != nil, progress: { _ in })
        } catch {
            if let copied { try? FileManager.default.removeItem(at: copied) } // ours; the original is untouched
            throw error
        }
        let report = try handle.openReport()
        if newID != session.id { handles[session.id] = nil }
        recents.replace(old, with: url)
        let count = Int(report.added + report.existing)
        recents.setPhotoCount(count, for: url)
        AppLog.session.notice("""
            relocated \(old.path, privacy: .private(mask: .hash)) → \(url.path, privacy: .private(mask: .hash)) \
            (store \(report.store, privacy: .private(mask: .hash)), \(report.missing) still missing)
            """)
        return SessionInfo(id: newID, name: url.lastPathComponent, url: url, photoCount: count, createdAt: Date())
    }

    /// Where ds-session keeps an in-place session inside its folder.
    static let inPlaceDir = ".ds/session"

    static func hasSession(_ store: URL) -> Bool {
        FileManager.default.fileExists(atPath: store.appendingPathComponent(".ds/settings/session.json").path)
    }

    static func isInside(_ url: URL, _ folder: URL) -> Bool {
        let (u, f) = (url.standardizedFileURL.path, folder.standardizedFileURL.path)
        return u == f || u.hasPrefix(f.hasSuffix("/") ? f : f + "/")
    }

    /// The store `session` uses or last used: the open handle's, else one inside the
    /// folder, else the app-owned one for that path. Nil when there is none.
    private func existingStore(of session: UUID, folder: URL) -> URL? {
        if let h = handles[session], (try? h.inPlaceFolder()) != nil, let path = try? h.openReport().store {
            return URL(fileURLWithPath: path, isDirectory: true)
        }
        let candidates = [folder.appendingPathComponent(Self.inPlaceDir, isDirectory: true),
                          stores[session] ?? Self.fallbackStore(for: folder, base: storeBase)]
        return candidates.first(where: Self.hasSession)
    }

    /// Copy a session store (index, sidecars, settings) to `target`, atomically: into a
    /// temporary sibling first, renamed into place only once complete. APFS clones the
    /// files, so this is cheap on the same volume.
    static func copyStore(_ from: URL, to target: URL) throws {
        let fm = FileManager.default
        let parent = target.deletingLastPathComponent()
        try fm.createDirectory(at: parent, withIntermediateDirectories: true)
        let partial = parent.appendingPathComponent(".\(target.lastPathComponent).partial-\(UUID().uuidString)")
        do {
            try fm.copyItem(at: from, to: partial)
            try fm.moveItem(at: partial, to: target)
        } catch {
            try? fm.removeItem(at: partial)
            throw error
        }
    }

    /// Wait for background hashing of `session` (tests; the app never needs to).
    func waitForHashing(_ session: UUID) async {
        await hashing[session]?.value
    }

    /// The folder of `id` was renamed or moved while open (R3-L6; imported
    /// sessions too, final-journeys H1): find it by its bookmark and reopen it
    /// there (an in-place folder with the same store). The session is also known
    /// under its new path's id from now on, the Sessions list follows it, and the
    /// app is told (it lists the session again and says so).
    private func reopenMoved(_ id: UUID) async throws -> SessionHandle {
        guard let moved = RecentFolders.follow(bookmarks[id]) else {
            throw MockError.notFound(folders[id]?.path ?? "folder")
        }
        let old = folders[id]
        if let old { recents.replace(old, with: moved) } else { recents.add(moved) }
        let store = stores[id] ?? Self.fallbackStore(for: moved, base: storeBase)
        let handle = try await open(id, at: moved, store: store, moved: true, progress: { _ in })
        let newID = FolderSessionStore.sessionID(for: moved)
        if newID != id {
            handles[newID] = handle
            folders[newID] = moved
            bookmarks[newID] = bookmarks[id]
            stores[newID] = stores[id]
        }
        if let old, old != moved {
            let info = SessionInfo(id: newID, name: moved.lastPathComponent, url: moved,
                                   photoCount: recents.photoCount(moved), createdAt: Date())
            moveHandler?(SessionMove(from: old, to: info))
        }
        return handle
    }

    /// Temporary folders (dev and test artifacts) are left out of the recents.
    public var ignoresTemporaryRecents = RecentsHygiene.ignoresTemporaryFolders

    public func setIgnoresTemporaryRecents(_ on: Bool) { ignoresTemporaryRecents = on }

    /// Temporary recents already logged as ignored (said once, not on every reload).
    private var loggedTemporary: Set<URL> = []

    /// Recent folders (not opened until their photos are requested). A folder that's
    /// gone or holds no photos any more is listed as unavailable (greyed out, never
    /// reopened on its own); temporary ones are left out.
    public func listSessions() async throws -> [SessionInfo] {
        let urls = recents.folders.filter { url in
            guard ignoresTemporaryRecents, RecentsHygiene.isTemporary(url) else { return true }
            if loggedTemporary.insert(url).inserted {
                AppLog.session.notice("ignoring temporary recent folder \(url.path, privacy: .private(mask: .hash))")
            }
            return false
        }
        let reasons = await withTaskGroup(of: (URL, SessionInfo.Unavailable?).self) { group in
            for url in urls { group.addTask { (url, await RecentsHygiene.unavailableReason(url)) } }
            var out: [URL: SessionInfo.Unavailable] = [:]
            for await (url, why) in group { if let why { out[url] = why } }
            return out
        }
        return urls.map { url in
            let id = FolderSessionStore.sessionID(for: url)
            if folders[id] == nil { folders[id] = url }
            var info = SessionInfo(id: id, name: url.lastPathComponent, url: url,
                                   photoCount: recents.photoCount(url), createdAt: Date(), unavailable: reasons[url])
            info.lastOpened = recents.lastOpened(url)
            return info
        }
    }

    public func newSessionLocation() async -> URL { await libraryRoot?() ?? RustImportService.defaultLibraryRoot }

    /// [arc6] The open handle of `session` for archiving (never opens one).
    public func archiveHandle(for session: SessionInfo) -> SessionHandle? {
        handles[session.id]
    }

    private func handle(for session: SessionInfo) async throws -> SessionHandle {
        if let h = handles[session.id] { return h }
        _ = try await add(folder: folders[session.id] ?? session.url, progress: { _ in })
        guard let h = handles[session.id] ?? handles[FolderSessionStore.sessionID(for: session.url.standardizedFileURL.resolvingSymlinksInPath())] else {
            throw MockError.notFound("session \(session.name)")
        }
        handles[session.id] = h
        return h
    }

    /// Run `body` on the session's handle; if its folder moved, reopen and retry once.
    private func perform<T: Sendable>(_ id: UUID,
                                      _ body: @escaping @Sendable (SessionHandle, CancelToken) async throws -> T) async throws -> T {
        guard let h = handles[id] else { throw MockError.notFound("session") }
        do {
            return try await Self.call(mapping: false) { token in try await body(h, token) }
        } catch let e as DsError {
            guard case .FolderMoved = e else { throw Self.mapped(e) }
            let again = try await reopenMoved(id)
            return try await Self.call { token in try await body(again, token) }
        }
    }

    public func photos(in session: SessionInfo) async throws -> [Photo] {
        _ = try await handle(for: session)
        let records = try await perform(session.id) { h, token in try await h.variants(token: token) }
        var out: [Photo] = []
        out.reserveCapacity(records.count)
        for r in records {
            let p = Self.photo(from: r)
            stored[Key(session: session.id, photo: p.id)] = p
            out.append(p)
        }
        members[session.id] = Set(out.map(\.id))
        closedPhotos.subtract(out.map(\.id))
        current = session.id
        if let url = folders[session.id] { recents.setPhotoCount(out.count, for: url) }
        return out
    }

    public func makeCurrent(_ session: SessionInfo) {
        if members[session.id] != nil { current = session.id }
    }

    /// The session bare photo ids resolve in first (tests).
    var currentSessionID: UUID? { current }

    /// The session a bare `PhotoID` belongs to: the one listed last first.
    private func key(for id: PhotoID) -> Key? {
        if let c = current, members[c]?.contains(id) == true { return Key(session: c, photo: id) }
        return members.first { $0.value.contains(id) }.map { Key(session: $0.key, photo: id) }
    }

    private func grouped(_ ids: [PhotoID]) -> [UUID: [PhotoID]] {
        var out: [UUID: [PhotoID]] = [:]
        for id in ids { if let k = key(for: id) { out[k.session, default: []].append(id) } }
        return out
    }

    public func albums(in session: SessionInfo) async throws -> [Album] { [] }

    /// An empty session `<library root>/<name>` (Capture, Selects, Output, Trash), opened
    /// and added to the recents like an imported one.
    public func createSession(named name: String) async throws -> SessionInfo {
        guard let libraryRoot else {
            throw MockError.invalid("New sessions are created by importing from a card. Open a folder instead.")
        }
        return try await createSession(named: name, in: await libraryRoot())
    }

    /// An empty session `<root>/<name>`. A folder already there is never used (nor
    /// anything in it touched).
    public func createSession(named name: String, in root: URL) async throws -> SessionInfo {
        let dir = root.appendingPathComponent(name, isDirectory: true)
        let known = folders.map { SessionInfo(id: $0.key, name: $0.value.lastPathComponent, url: $0.value, photoCount: 0,
                                              createdAt: Date()) }
        // File checks off the actor (a slow drive must not stall the store).
        let (exists, nested, made) = await Task.detached {
            (FileManager.default.fileExists(atPath: dir.path), SessionLocation.problem(root, sessions: known),
             SessionLocation.missing(dir))
        }.value
        guard !exists else { throw MockError.invalid("“\(name)” already exists in \(root.path).") }
        if let nested { throw MockError.invalid(nested) }
        do {
            _ = try await Self.call { token in try await DarkslideFFI.createSession(dir: dir.path, name: name, token: token) }
            return try await add(folder: dir)
        } catch {
            await Task.detached { SessionLocation.cleanUp(made) }.value // only what this create made
            throw error
        }
    }

    /// [session close] Release `session`: its handle (and so its index connection) and
    /// its background identity pass, which keeps what's left for the next open. An
    /// archive run holds its own reference until it ends. Nothing on disk changes; the
    /// session stays in the recents.
    public func close(_ session: SessionInfo) async {
        guard let h = handles[session.id] else { return }
        // A session that moved while open is known under both ids.
        let ids = handles.filter { $0.value === h }.map(\.key)
        var passes: [Task<Void, Never>] = []
        for id in ids {
            handles[id] = nil
            closedPhotos.formUnion(members[id] ?? [])
            members[id] = nil
            if current == id { current = nil }
            if let t = hashing.removeValue(forKey: id) {
                t.cancel()
                passes.append(t)
            }
        }
        let gone = Set(ids)
        stored = stored.filter { !gone.contains($0.key.session) }
        edits = edits.filter { !gone.contains($0.key.session) }
        raw = raw.filter { !gone.contains($0.key.session) }
        // They hold the handle until they return; a pass stuck on a hung volume is left
        // to finish on its own (bounded, so closing never hangs).
        let cancelled = passes
        _ = await Bounded.wait(passWaitLimit) { for t in cancelled { await t.value } }
    }

    func setPassWaitLimit(_ limit: Duration) { passWaitLimit = limit }

    /// Tests: an identity pass for `session` (as an open starts one).
    func installHashingPass(for session: SessionInfo, _ task: Task<Void, Never>) { hashing[session.id] = task }

    /// Whether the store holds `session` open (tests).
    func isOpen(_ session: SessionInfo) -> Bool { handles[session.id] != nil }

    public func removeMissing(_ ids: [PhotoID]) async throws {
        for (sid, list) in grouped(ids) {
            let raws = list.map(\.raw)
            _ = try await perform(sid) { h, token in try await h.removeMissing(ids: raws, token: token) }
            members[sid]?.subtract(list)
            removed[sid, default: []].formUnion(list)
            for id in list {
                let k = Key(session: sid, photo: id)
                stored[k] = nil
                edits[k] = nil
                raw[k] = nil
            }
        }
    }

    public func locate(_ id: PhotoID, at url: URL) async throws {
        guard let k = key(for: id) else { throw MockError.notFound("photo \(id.raw)") }
        let path = url.standardizedFileURL.resolvingSymlinksInPath().path
        try await perform(k.session) { h, token in try await h.locate(variantId: id.raw, path: path, token: token) }
        stored[k]?.url = URL(fileURLWithPath: path)
        stored[k]?.isMissing = false
    }

    public func restorePrevious(_ id: PhotoID) async throws {
        guard let k = key(for: id) else { throw MockError.notFound("photo \(id.raw)") }
        try await perform(k.session) { h, token in try await h.restorePrevious(variantId: id.raw, token: token) }
        forget(k)
    }

    public func dismissFileChanged(_ id: PhotoID) async throws {
        guard let k = key(for: id) else { throw MockError.notFound("photo \(id.raw)") }
        try await perform(k.session) { h, token in try await h.dismissFileChanged(variantId: id.raw, token: token) }
        stored[k]?.fileChanged = false
    }

    public func restoreToFolder(_ id: PhotoID) async throws {
        guard let k = key(for: id) else { throw MockError.notFound("photo \(id.raw)") }
        try await perform(k.session) { h, token in try await h.restoreToFolder(variantId: id.raw, token: token) }
        stored[k]?.isMissing = false
        stored[k]?.copyInStore = false
    }

    /// [upscale] A name for a new photo made from `source`, free on disk and in the session.
    public func freeDerivedName(for url: URL, from source: PhotoID, suffix: String) async throws -> String {
        guard let k = key(for: source) else { throw MockError.notFound("photo \(source.raw)") }
        let path = url.standardizedFileURL.resolvingSymlinksInPath().path
        return try await perform(k.session) { h, token in
            try await h.freeDerivedName(sourcePath: path, suffix: suffix, token: token)
        }
    }

    /// [upscale] Add `url` (AI Upscale's DNG of `source`, in its folder) as a new photo
    /// starting with the source's edit.
    public func addDerived(_ url: URL, from source: PhotoID) async throws -> PhotoID {
        guard let k = key(for: source) else { throw MockError.notFound("photo \(source.raw)") }
        let path = url.standardizedFileURL.resolvingSymlinksInPath().path
        let v = try await perform(k.session) { h, token in
            try await h.addDerived(path: path, sourceVariantId: source.raw, token: token)
        }
        return PhotoID(v)
    }

    /// Drop what this store remembers of a photo (its record changed in Rust).
    private func forget(_ k: Key) {
        stored[k] = nil
        edits[k] = nil
        raw[k] = nil
    }

    /// Drop every record remembered for `session` (Rust rewrote its sidecars: a Capture One import).
    func forgetAll(in session: UUID) {
        for k in Set(stored.keys).union(edits.keys).union(raw.keys) where k.session == session { forget(k) }
    }

    // MARK: - Culling metadata

    public func save(photos changed: [Photo]) async throws {
        var bySession: [UUID: [Photo]] = [:]
        var unsavable: Set<PhotoID> = []
        for p in changed {
            guard let k = key(for: p.id) else {
                if !skipRemoved(p.id) { unsavable.insert(p.id) }
                continue
            }
            bySession[k.session, default: []].append(p)
        }
        for (sid, list) in bySession {
            let plan = Self.writePlan(list, stored: stored.filter { $0.key.session == sid }
                .reduce(into: [:]) { $0[$1.key.photo] = $1.value })
            for (rating, ids) in plan.ratings {
                try await perform(sid) { h, token in try await h.setRating(ids: ids, rating: rating, token: token) }
            }
            for (tag, ids) in plan.tags {
                try await perform(sid) { h, token in try await h.setColorTag(ids: ids, tag: tag, token: token) }
            }
            for (pick, ids) in plan.picks {
                try await perform(sid) { h, token in try await h.setPick(ids: ids, pick: pick, token: token) }
            }
            for (keywords, ids) in plan.keywords {
                try await perform(sid) { h, token in try await h.setKeywords(ids: ids, keywords: keywords, token: token) }
            }
            for (metadata, ids) in plan.metadata {
                try await perform(sid) { h, token in
                    try await h.setMetadata(ids: ids, metadata: Self.record(metadata), token: token)
                }
            }
            for (geotag, ids) in plan.geotags { // [map]
                try await perform(sid) { h, token in try await h.setGeotag(ids: ids, position: geotag?.record, token: token) }
            }
            for p in list { stored[Key(session: sid, photo: p.id)] = p }
        }
        // The rest is written; these never can be (their session is closed).
        if !unsavable.isEmpty { throw UnsavableChanges(ids: unsavable) }
    }

    /// A late write for a photo Remove Missing forgot: skipped (said in the log), since
    /// the user removed it on purpose.
    private func skipRemoved(_ id: PhotoID) -> Bool {
        guard removed.values.contains(where: { $0.contains(id) }) else { return false }
        AppLog.session.notice("skipped a write for removed photo \(id.raw, privacy: .public)")
        return true
    }

    struct WritePlan: Equatable {
        var ratings: [UInt8: [Int64]] = [:]
        var tags: [ColorLabel: [Int64]] = [:]
        var picks: [PickFlag: [Int64]] = [:]
        var keywords: [[String]: [Int64]] = [:]
        var geotags: [GeoPoint?: [Int64]] = [:]
        var metadata: [IPTCMetadata: [Int64]] = [:]
    }

    /// Group changes into batch writes; unchanged fields aren't written.
    static func writePlan(_ list: [Photo], stored: [PhotoID: Photo]) -> WritePlan {
        var plan = WritePlan()
        for p in list.sorted(by: { $0.id < $1.id }) {
            let old = stored[p.id]
            if old?.rating != p.rating { plan.ratings[UInt8(clamping: p.rating), default: []].append(p.id.raw) }
            if old?.colorTag != p.colorTag { plan.tags[label(p.colorTag), default: []].append(p.id.raw) }
            if old?.pick != p.pick { plan.picks[flag(p.pick), default: []].append(p.id.raw) }
            if (old?.keywords ?? []) != p.keywords { plan.keywords[p.keywords, default: []].append(p.id.raw) }
            if (old?.geotag ?? nil) != p.geotag { plan.geotags[p.geotag, default: []].append(p.id.raw) }
            if (old?.metadata ?? IPTCMetadata()) != p.metadata { plan.metadata[p.metadata, default: []].append(p.id.raw) }
        }
        return plan
    }

    // MARK: - Edits

    public func edit(for id: PhotoID) async throws -> EditState {
        guard let k = key(for: id) else { return EditState() }
        if let e = edits[k] { return e }
        let json = try await perform(k.session) { h, token in try await h.loadAdjustmentsJson(variantId: id.raw, token: token) }
        let (e, obj) = try AdjustmentsCodec.decode(json)
        edits[k] = e
        raw[k] = obj
        return e
    }

    public func edits(in session: SessionInfo) async throws -> [PhotoID: EditState] {
        _ = try await handle(for: session)
        let all = try await perform(session.id) { h, token in try await h.loadAllAdjustmentsJson(token: token) }
        deviceGenerations[session.id] = all.deviceGeneration // [remote]
        var out: [PhotoID: EditState] = [:]
        func merge(_ variantId: Int64, _ decoded: (EditState, AdjustmentsCodec.JSON)) {
            let id = PhotoID(variantId)
            let k = Key(session: session.id, photo: id)
            let (e, obj) = decoded
            raw[k] = obj
            edits[k] = e
            members[session.id, default: []].insert(id)
            if !e.isDefault { out[id] = e }
        }
        for a in all.edits { merge(a.variantId, try AdjustmentsCodec.decode(a.json)) }
        // Untouched sidecars from before profiles: one shared "default + Neutral" edit.
        if !all.neutralIds.isEmpty {
            let neutral = try AdjustmentsCodec.decode(all.neutralJson)
            for id in all.neutralIds { merge(id, neutral) }
        }
        return out
    }

    /// One Rust batch (one ds-session undo group) per session.
    public func save(edits updated: [PhotoID: EditState]) async throws {
        var bySession: [UUID: [AdjustmentsJson]] = [:]
        var sent: [Key: String] = [:]
        var unsavable: Set<PhotoID> = []
        for (id, e) in updated.sorted(by: { $0.key < $1.key }) {
            guard let k = key(for: id) else {
                if !skipRemoved(id) { unsavable.insert(id) }
                continue
            }
            let json = try AdjustmentsCodec.encode(e, onto: raw[k] ?? Self.defaultJSON())
            sent[k] = json
            bySession[k.session, default: []].append(AdjustmentsJson(variantId: id.raw, json: json))
        }
        for (sid, batch) in bySession {
            _ = try await perform(sid) { h, token in try await h.applyBatchJson(edits: batch, token: token) }
        }
        for (k, json) in sent {
            if let (e, obj) = try? AdjustmentsCodec.decode(json) {
                raw[k] = obj
                edits[k] = e
            }
        }
        if !unsavable.isEmpty { throw UnsavableChanges(ids: unsavable) }
    }

    /// `ds_core::Adjustments::default()` as a raw object (base for photos never read).
    static func defaultJSON() -> AdjustmentsCodec.JSON {
        guard let s = try? defaultAdjustmentsJson(), let (_, obj) = try? AdjustmentsCodec.decode(s) else { return [:] }
        return obj
    }

    static func autoApplyRecord(_ p: Preset) throws -> AutoApplyPreset {
        AutoApplyPreset(name: p.name, adjustmentsJson: try AdjustmentsCodec.encode(p.edit, onto: defaultJSON()),
                        groups: p.groups.sorted { $0.rawValue < $1.rawValue }.map(\.rust),
                        relative: p.relative.sorted { $0.rawValue < $1.rawValue }.map(\.rust))
    }

    // MARK: - Mapping

    static func photo(from r: VariantRecord) -> Photo {
        let url = URL(fileURLWithPath: r.originalPath)
        let fallback = r.captureTimeMs.map { Date(timeIntervalSince1970: Double($0) / 1000) } ?? Date(timeIntervalSince1970: 0)
        var p: Photo
        if let m = r.meta {
            p = RustRaw.makePhoto(m, url: url, id: PhotoID(r.variantId), fileSize: Int64(r.fileSize), fallbackDate: fallback)
        } else {
            p = Photo(id: PhotoID(r.variantId), filename: r.fileName, meta: PhotoMeta(
                camera: "", lens: "", iso: 0, shutter: 0, aperture: 0, focalLength: 0, captureDate: fallback,
                pixelWidth: Int(r.width), pixelHeight: Int(r.height), fileSize: Int64(r.fileSize)), url: url)
        }
        p.meta.fileSequence = r.fileSequence.map { Int(clamping: $0) }
        if p.meta.pixelWidth == 0 {
            p.meta.pixelWidth = Int(r.width)
            p.meta.pixelHeight = Int(r.height)
        }
        p.rating = Int(r.rating)
        p.colorTag = tag(r.colorTag)
        p.pick = pick(r.pick)
        p.isEdited = r.edited
        p.isMissing = r.missing
        p.fileChanged = r.fileChanged
        p.copyInStore = r.keptInStore
        p.inTrash = r.inTrash // [cul9] H7
        p.inSelects = r.inSelects
        p.variantName = r.name
        p.keywords = r.keywords
        p.metadata = r.metadata.map(metadata) ?? IPTCMetadata()
        p.people = r.people // [find]
        p.geotag = r.geotag.flatMap(GeoPoint.init) // [map]
        // XMP-only positions (once scanned); an EXIF position stays as read, with its altitude.
        if p.meta.gps == nil, let file = r.fileGps.flatMap(GeoPoint.init) { p.meta.gps = file }
        p.meta.captureWallClockMs = r.captureTimeMs ?? p.meta.captureWallClockMs // L7: what Rust filters on
        return p
    }

    /// The FFI record of a photo's IPTC fields, and back.
    static func record(_ m: IPTCMetadata) -> PhotoMetadata {
        PhotoMetadata(title: m.title, caption: m.caption, creator: m.creator, copyright: m.copyright, credit: m.credit,
                      headline: m.headline, city: m.city, state: m.state, country: m.country,
                      sublocation: m.sublocation, instructions: m.instructions, usageTerms: m.usageTerms)
    }

    static func metadata(_ r: PhotoMetadata) -> IPTCMetadata {
        var m = IPTCMetadata()
        (m.title, m.caption, m.creator, m.copyright) = (r.title, r.caption, r.creator, r.copyright)
        (m.credit, m.headline, m.city, m.state) = (r.credit, r.headline, r.city, r.state)
        (m.country, m.sublocation, m.instructions, m.usageTerms) = (r.country, r.sublocation, r.instructions, r.usageTerms)
        return m
    }

    static func tag(_ l: ColorLabel) -> ColorTag {
        switch l {
        case .red: .red
        case .yellow: .yellow
        case .green: .green
        case .blue: .blue
        case .purple: .purple
        case .orange: .orange
        case .pink: .pink
        case .none: .none
        }
    }

    static func label(_ t: ColorTag) -> ColorLabel {
        switch t {
        case .none: .none
        case .red: .red
        case .yellow: .yellow
        case .green: .green
        case .blue: .blue
        case .purple: .purple
        case .orange: .orange
        case .pink: .pink
        }
    }

    static func pick(_ f: PickFlag) -> PickState {
        switch f {
        case .unflagged: .none
        case .picked: .picked
        case .rejected: .rejected
        }
    }

    static func flag(_ p: PickState) -> PickFlag {
        switch p {
        case .none: .unflagged
        case .picked: .picked
        case .rejected: .rejected
        }
    }

    /// Run one FFI call with Swift cancellation forwarded to a fresh `CancelToken`.
    /// Errors become `CancellationError` / `RenderError` unless `mapping` is false
    /// (then `DsError` passes through for the caller to inspect).
    static func call<T: Sendable>(mapping: Bool = true,
                                  _ body: @Sendable (CancelToken) async throws -> T) async throws -> T {
        let token = try CancelToken()
        try Task.checkCancellation()
        do {
            return try await withTaskCancellationHandler {
                try await body(token)
            } onCancel: {
                try? token.cancel()
            }
        } catch let e as DsError {
            if case .Cancelled = e { throw CancellationError() }
            throw mapping ? mapped(e) : e
        }
    }

    static func mapped(_ e: DsError) -> Error {
        if case .Cancelled = e { return CancellationError() }
        return RenderError(e)
    }
}

// MARK: - Smart folders (ds-session smart albums; built-ins are created on open)

extension RustSessionStore: SmartFolderStoring {
    public func storedSmartFolders(in session: SessionInfo) async throws -> [SmartFolder] {
        _ = try await handle(for: session)
        let records = try await perform(session.id) { h, token in try await h.smartFolders(token: token) }
        return records.map { r in
            SmartFolder(id: r.id, name: r.name, builtin: r.builtin, mirror: r.mirror, criteria: Self.criteria(r.filter))
        }
    }

    public func storeCreateSmartFolder(named name: String, criteria: SmartCriteria,
                                       in session: SessionInfo) async throws -> SmartFolder {
        _ = try await handle(for: session)
        let filter = Self.record(criteria)
        let id = try await perform(session.id) { h, token in
            try await h.createSmartFolder(name: name, filter: filter, token: token)
        }
        return SmartFolder(id: id, name: name, criteria: criteria)
    }

    public func storeUpdateSmartFolder(_ folder: SmartFolder, in session: SessionInfo) async throws {
        _ = try await handle(for: session)
        let (id, name, filter) = (folder.id, folder.name, Self.record(folder.criteria))
        try await perform(session.id) { h, token in
            try await h.updateSmartFolder(id: id, name: name, filter: filter, token: token)
        }
    }

    public func storeDeleteSmartFolder(_ id: Int64, in session: SessionInfo) async throws {
        _ = try await handle(for: session)
        try await perform(session.id) { h, token in try await h.deleteSmartFolder(id: id, token: token) }
    }

    public func storeSetMirror(_ id: Int64, on: Bool, in session: SessionInfo) async throws {
        _ = try await handle(for: session)
        try await perform(session.id) { h, token in try await h.setSmartFolderMirror(id: id, on: on, token: token) }
    }

    public func storeQuery(_ criteria: SmartCriteria, in session: SessionInfo) async throws -> Set<PhotoID> {
        _ = try await handle(for: session)
        let filter = Self.record(criteria)
        let ids = try await perform(session.id) { h, token in try await h.queryFilter(filter: filter, token: token) }
        return Set(ids.map { PhotoID($0) })
    }

    /// `<folder>/Smart Folders` when the session store is on the folder's volume,
    /// else next to the store in Application Support (card, read-only, network).
    public func storeMirrorRoot(for session: SessionInfo) async -> URL? {
        guard let h = try? await handle(for: session), let root = try? h.root() else { return nil }
        let store = URL(fileURLWithPath: root, isDirectory: true)
        if let folder = (try? h.inPlaceFolder()).flatMap({ $0 }),
           store.standardizedFileURL.path.hasPrefix(URL(fileURLWithPath: folder).standardizedFileURL.path + "/") {
            return URL(fileURLWithPath: folder, isDirectory: true).appendingPathComponent("Smart Folders", isDirectory: true)
        }
        return store.appendingPathComponent("Smart Folders", isDirectory: true)
    }

    static func record(_ c: SmartCriteria) -> SmartFilterRecord {
        let ms = { (d: Date?) in d.map { SmartCriteria.wallClockMs($0) } } // L7: camera wall clock, like capture_time_ms
        return SmartFilterRecord(
            minRating: c.minRating.map { UInt8(clamping: $0) }, maxRating: c.maxRating.map { UInt8(clamping: $0) },
            tags: ColorTag.allCases.filter(c.tags.contains).map(label), pick: c.pick.map(flag),
            text: c.text, camera: c.camera, lens: c.lens, dateFromMs: ms(c.dateFrom), dateToMs: ms(c.dateTo),
            isoMin: c.isoMin.map { UInt32(clamping: $0) }, isoMax: c.isoMax.map { UInt32(clamping: $0) },
            edited: c.edited, keywords: c.keywords)
    }

    static func criteria(_ r: SmartFilterRecord) -> SmartCriteria {
        let date = { (ms: Int64?) in ms.map { SmartCriteria.date(wallClockMs: $0) } }
        return SmartCriteria(
            minRating: r.minRating.map(Int.init), maxRating: r.maxRating.map(Int.init), tags: Set(r.tags.map(tag)),
            pick: r.pick.map(pick), text: r.text, camera: r.camera, lens: r.lens, dateFrom: date(r.dateFromMs),
            dateTo: date(r.dateToMs), isoMin: r.isoMin.map(Int.init), isoMax: r.isoMax.map(Int.init),
            edited: r.edited, keywords: r.keywords)
    }
}

/// Forwards open/hash progress events from Rust to an `OpenProgress` callback.
final class ProgressRelay: ProgressListener, @unchecked Sendable {
    private let report: @Sendable (OpenProgress) -> Void

    init(_ report: @escaping @Sendable (OpenProgress) -> Void) { self.report = report }

    static func progress(_ e: ProgressEvent) -> OpenProgress? {
        let phase: OpenProgress.Phase
        switch e.kind {
        case "open.scan": phase = .scanning
        case "open.register": phase = .registering
        case "hash": phase = .hashing
        default: return nil
        }
        return OpenProgress(phase: phase, done: Int(clamping: e.completed), total: Int(clamping: e.total))
    }

    func onProgress(event: ProgressEvent) {
        if let p = Self.progress(event) { report(p) }
    }
}

// MARK: - Sessions list (L12)

extension RustSessionStore: RecentSessionsEditing {
    public func removeFromRecents(_ session: SessionInfo) async { recents.remove(folders[session.id] ?? session.url) }
    public func clearRecents(keeping sessions: [SessionInfo]) async {
        recents.clear(keeping: sessions.map { folders[$0.id] ?? $0.url })
    }
    public func noteOpened(_ session: SessionInfo) async { recents.stampOpened(folders[session.id] ?? session.url) }
}

// MARK: - Finishing a cull (final-journeys H7)

/// A session folder originals can be moved into (sessions created by import).
public enum SessionFolderKind: String, Sendable, CaseIterable {
    case capture, selects, trash

    var rust: SessionFolder {
        switch self {
        case .capture: .capture
        case .selects: .selects
        case .trash: .trash
        }
    }
}

/// One original moved between session folders (paths relative to the session),
/// named by one of its photos: what undo needs to put it back where it was.
public struct FolderMove: Equatable, Sendable {
    public var id: PhotoID
    public var from: String
    public var to: String

    public init(id: PhotoID, from: String, to: String) {
        self.id = id
        self.from = from
        self.to = to
    }
}

/// What "Move Rejected to Trash" did.
public struct RejectsTrashed: Equatable, Sendable {
    /// Originals whose every variant is rejected, now in the session's `Trash/`.
    public var moves: [FolderMove] = []
    /// Rejected variants of photos with a kept variant: hidden (recoverable).
    public var hidden: [PhotoID] = []
    /// It stopped part-way: why (`moves` and `hidden` are what was done).
    public var error: String?

    public init(moves: [FolderMove] = [], hidden: [PhotoID] = [], error: String? = nil) {
        self.moves = moves
        self.hidden = hidden
        self.error = error
    }
}

/// Stores that can finish a cull: move originals between the session's folders,
/// empty its Trash into the macOS Trash, and make variants. Only sessions created
/// by import move files; a folder opened in place holds the user's own files.
public protocol SessionFileOrganizing: Sendable {
    func canMoveOriginals(in session: SessionInfo) async -> Bool
    /// Move the originals of `ids` into `folder`; returns the originals moved.
    func move(_ ids: [PhotoID], to folder: SessionFolderKind) async throws -> [FolderMove]
    /// "Move Rejected to Trash": an original goes to the session's Trash only when
    /// every variant of it is rejected; other photos lose only their rejected variants.
    func trashRejected(in session: SessionInfo) async throws -> RejectsTrashed
    /// Undo a move: each original back where it came from (a free name there if
    /// taken meanwhile), and the hidden variants back.
    func moveBack(_ moves: [FolderMove], hidden: [PhotoID], in session: SessionInfo) async throws
    /// Every variant set aside in the session Trash back next to its photo (found
    /// on disk, so after a relaunch too); returns how many came back.
    func restoreSetAsideVariants(in session: SessionInfo) async throws -> Int
    /// Hand the session's `Trash/` to the macOS Trash; returns how many originals left.
    func emptyTrash(in session: SessionInfo) async throws -> Int
    /// A new variant of `id` (its edits copied); returns the new photo's id.
    func newVariant(of id: PhotoID) async throws -> PhotoID
}

extension RustSessionStore: SessionFileOrganizing {
    public func canMoveOriginals(in session: SessionInfo) async -> Bool {
        guard let h = try? await handle(for: session) else { return false }
        return h.canMoveOriginals()
    }

    public func move(_ ids: [PhotoID], to folder: SessionFolderKind) async throws -> [FolderMove] {
        var moved: [FolderMove] = []
        for (sid, list) in grouped(ids) {
            let raws = list.map(\.raw)
            let rust = folder.rust
            let records = try await perform(sid) { h, token in
                try await h.moveToFolder(ids: raws, folder: rust, token: token)
            }
            moved += records.map(Self.folderMove)
            for id in list { forget(Key(session: sid, photo: id)) }
        }
        return moved
    }

    public func trashRejected(in session: SessionInfo) async throws -> RejectsTrashed {
        _ = try await handle(for: session)
        let r = try await perform(session.id) { h, token in try await h.trashRejected(token: token) }
        let done = RejectsTrashed(moves: r.moved.map(Self.folderMove), hidden: r.hidden.map { PhotoID($0) }, error: r.error)
        for id in done.moves.map(\.id) + done.hidden { forget(Key(session: session.id, photo: id)) }
        return done
    }

    public func moveBack(_ moves: [FolderMove], hidden: [PhotoID], in session: SessionInfo) async throws {
        _ = try await handle(for: session)
        let records = moves.map { FolderMoveRecord(variantId: $0.id.raw, from: $0.from, to: $0.to) }
        let raws = hidden.map(\.raw)
        _ = try await perform(session.id) { h, token in
            try await h.moveBack(moves: records, hidden: raws, token: token)
        }
        for id in moves.map(\.id) + hidden { forget(Key(session: session.id, photo: id)) }
    }

    public func restoreSetAsideVariants(in session: SessionInfo) async throws -> Int {
        _ = try await handle(for: session)
        return Int(try await perform(session.id) { h, token in try await h.restoreSetAsideVariants(token: token) })
    }

    static func folderMove(_ r: FolderMoveRecord) -> FolderMove {
        FolderMove(id: PhotoID(r.variantId), from: r.from, to: r.to)
    }

    public func emptyTrash(in session: SessionInfo) async throws -> Int {
        _ = try await handle(for: session)
        return Int(try await perform(session.id) { h, token in try await h.emptyTrash(token: token) })
    }

    public func newVariant(of id: PhotoID) async throws -> PhotoID {
        guard let k = key(for: id) else { throw MockError.notFound("photo \(id.raw)") }
        let v = try await perform(k.session) { h, token in try await h.newVariant(variantId: id.raw, token: token) }
        return PhotoID(v)
    }
}

// MARK: - AI Optimize reports (sidecar `optimize_report`)

extension RustSessionStore: OptimizeReportStoring {
    public func optimizeReport(for id: PhotoID) async throws -> OptimizeReport? {
        guard let k = key(for: id) else { return nil }
        let json = try await perform(k.session) { h, token in try await h.optimizeReport(variantId: id.raw, token: token) }
        return try json.map(OptimizeReport.init(json:))
    }

    public func saveOptimizeReport(_ report: OptimizeReport?, for ids: [PhotoID]) async throws {
        var bySession: [UUID: [Int64]] = [:]
        for id in ids { if let k = key(for: id) { bySession[k.session, default: []].append(id.raw) } }
        let json = report?.json
        for (sid, raws) in bySession {
            try await perform(sid) { h, token in try await h.setOptimizeReport(ids: raws, reportJson: json, token: token) }
        }
    }
}

// MARK: - AI changes (sidecar `ai_changes`)

extension RustSessionStore: AIChangesStoring {
    public func aiChanges(for id: PhotoID) async throws -> String? {
        guard let k = key(for: id) else { return nil }
        return try await perform(k.session) { h, token in try await h.aiChanges(variantId: id.raw, token: token) }
    }

    public func saveAIChanges(_ json: String?, for ids: [PhotoID]) async throws {
        var bySession: [UUID: [Int64]] = [:]
        for id in ids { if let k = key(for: id) { bySession[k.session, default: []].append(id.raw) } }
        for (sid, raws) in bySession {
            try await perform(sid) { h, token in try await h.setAiChanges(ids: raws, changesJson: json, token: token) }
        }
    }
}

extension RustSessionStore: LinkSessionProviding {
    /// [remote] The session's store folder and handle, for the link server.
    public func linkSession(for session: SessionInfo) async -> LinkSession? {
        guard let h = try? await handle(for: session), let root = try? h.root() else { return nil }
        return LinkSession(root: URL(fileURLWithPath: root, isDirectory: true), name: session.name, handle: h)
    }

    public func markDeviceEditsSeen(in session: SessionInfo) async {
        // Only the device edits the adopted snapshot held: later pushes stay unseen.
        guard let h = try? await handle(for: session), let generation = deviceGenerations[session.id] else { return }
        remoteMarkSeen(handle: h, generation: generation)
    }
}
