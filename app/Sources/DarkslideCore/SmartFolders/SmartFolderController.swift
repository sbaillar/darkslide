import Foundation
import Observation

/// The session's smart folders and their live counts. Counts are computed in
/// full only when a session loads; after that every rating/tag/pick change is
/// an O(folders) delta (old photo vs new photo against each folder).
@MainActor @Observable
public final class SmartFolderController {
    public private(set) var folders: [SmartFolder] = []
    public private(set) var counts: [Int64: Int] = [:]
    /// Last store error (shown by the panel).
    public internal(set) var lastError: String?
    /// Photos the index matched for folders with keyword/text parts.
    @ObservationIgnored private var index: [Int64: Set<PhotoID>] = [:]
    @ObservationIgnored private var store: (any SessionStore)?
    @ObservationIgnored private var session: SessionInfo?
    @ObservationIgnored private var mirrorTask: Task<Void, Never>?
    /// The last queued mirror job: the next one waits for it, so two never touch the manifest at once.
    @ObservationIgnored private var mirrorChain: Task<Void, Never>?
    /// Seconds between the last change and a mirror refresh.
    @ObservationIgnored public var mirrorDelay: Duration = .seconds(1)
    /// Mirror file system (tests inject a fake).
    @ObservationIgnored public var mirrorFS: any MirrorFileSystem = RealMirrorFileSystem()
    /// All photos of the session, for mirror refreshes (set by the app model).
    @ObservationIgnored public var photosProvider: @MainActor () -> [Photo] = { [] }
    /// Called after folders were (re)loaded, so a selected folder's scope can be re-derived.
    @ObservationIgnored public var onFoldersChanged: @MainActor () -> Void = {}
    /// Makes pending rating/tag changes durable before folders are re-read from the store.
    @ObservationIgnored public var flushBeforeReload: @MainActor () async -> Void = {}
    @ObservationIgnored private var materializeTask: Task<Void, Never>?

    public init() {}

    /// Folders shown in the panel: tag folders only while their tag is in use.
    public var visibleFolders: [SmartFolder] {
        folders.filter { !$0.isTagFolder || counts[$0.id, default: 0] > 0 }
    }

    public func folder(_ id: Int64) -> SmartFolder? { folders.first { $0.id == id } }

    /// The built-in "exactly `n` stars" folder (⌘1…⌘5).
    public func ratingFolder(_ n: Int) -> SmartFolder? { folders.first { $0.exactRating == n } }

    public func scope(_ id: Int64) -> SmartFolderScope? {
        folder(id).map { SmartFolderScope(id: $0.id, criteria: $0.criteria, indexMatches: index[$0.id], builtin: $0.builtin) }
    }

    /// The current scope for a previously selected one: same id, or the same
    /// built-in (a tag folder that got its persisted id); `nil` when it's gone.
    public func refreshed(_ old: SmartFolderScope) -> SmartFolderScope? {
        if let s = scope(old.id) { return s }
        guard let b = old.builtin, let f = folders.first(where: { $0.builtin == b }) else { return nil }
        return scope(f.id)
    }

    public func contains(_ f: SmartFolder, _ p: Photo) -> Bool {
        !p.isMissing && f.criteria.matchesLocally(p) && (index[f.id]?.contains(p.id) ?? true)
    }

    // MARK: - Loading

    /// Fetch folders (and index matches) for `session` and count `photos` in full.
    public func reload(store: any SessionStore, session: SessionInfo, photos: [Photo]) async {
        self.store = store
        self.session = session
        do {
            let loaded = try await store.smartFolders(in: session, photos: photos)
            var idx: [Int64: Set<PhotoID>] = [:]
            for f in loaded where f.criteria.needsIndex {
                if let m = try await store.indexMatches(f.criteria, in: session) { idx[f.id] = m }
            }
            guard self.session?.id == session.id else { return }
            lastError = nil
            set(loaded, index: idx, photos: photos)
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// [session close] Forget the session's folders and stop their refreshes (a pending
    /// Finder mirror refresh runs again when the session next opens; a reload in flight
    /// is dropped when it returns).
    public func sessionClosed() {
        mirrorTask?.cancel()
        mirrorTask = nil
        materializeTask?.cancel()
        materializeTask = nil
        store = nil
        session = nil
        index = [:]
        folders = []
        counts = [:]
        lastError = nil
    }

    /// Replace folders and recount everything (session load only).
    func set(_ loaded: [SmartFolder], index idx: [Int64: Set<PhotoID>], photos: [Photo]) {
        index = idx
        var fs = loaded
        addMissingTagFolders(&fs, tags: Set(photos.lazy.filter { !$0.isMissing }.map(\.colorTag)))
        var c: [Int64: Int] = [:]
        for f in fs { c[f.id] = 0 }
        for p in photos {
            for f in fs where contains(f, p) { c[f.id, default: 0] += 1 }
        }
        folders = fs
        counts = c
        onFoldersChanged()
        scheduleMirror()
    }

    /// Tag folders are created by ds-session when folders are read; a tag first
    /// used during the session gets a local placeholder right away (negative id,
    /// never sent to the store) and its persisted folder shortly after (`materialize`).
    private func addMissingTagFolders(_ fs: inout [SmartFolder], tags: Set<ColorTag>) {
        let have = Set(fs.compactMap(\.builtin))
        for f in SmartFolder.builtins(tagsInUse: tags) where f.isTagFolder && !have.contains(f.builtin!) {
            fs.append(f)
        }
    }

    // MARK: - Incremental counts

    /// One photo changed: adjust each folder's count by at most one. O(folders).
    public func photoChanged(old: Photo, new: Photo) {
        if new.colorTag != .none, old.colorTag != new.colorTag,
           !folders.contains(where: { $0.isTagFolder && $0.criteria.tags == [new.colorTag] }) {
            addMissingTagFolders(&folders, tags: [new.colorTag])
            scheduleMaterialize()
        }
        var c = counts
        var changed = false
        for f in folders {
            let was = contains(f, old), now = contains(f, new)
            if was != now {
                c[f.id, default: 0] += now ? 1 : -1
                changed = true
            }
        }
        if changed {
            counts = c
            if folders.contains(where: \.mirror) { scheduleMirror() }
        }
    }

    /// Re-read folders from a persisting store so placeholder tag folders get real ids.
    private func scheduleMaterialize() {
        guard store is any SmartFolderStoring, materializeTask == nil else { return }
        materializeTask = Task { [weak self] in
            await self?.materialize()
            self?.materializeTask = nil
        }
    }

    /// Flush pending saves, then reload (the store creates missing tag folders).
    public func materialize() async {
        guard let store, let session else { return }
        await flushBeforeReload()
        await reload(store: store, session: session, photos: photosProvider())
    }

    /// Wait for a scheduled tag-folder materialization (tests).
    public func waitForMaterialize() async { await materializeTask?.value }

    /// A folder as the store knows it: placeholders (negative ids) are resolved
    /// to their persisted built-in first. `nil` if it can't be.
    private func persisted(_ folder: SmartFolder) async -> SmartFolder? {
        guard folder.id < 0, store is any SmartFolderStoring else { return folder }
        await materialize()
        return folders.first { $0.builtin != nil && $0.builtin == folder.builtin && $0.id >= 0 }
    }

    // MARK: - Editing

    public func create(name: String, criteria: SmartCriteria, photos: [Photo]) async -> SmartFolder? {
        guard let store, let session else { return nil }
        do {
            let f = try await store.createSmartFolder(named: name, criteria: criteria, in: session)
            await reload(store: store, session: session, photos: photos)
            return f
        } catch {
            lastError = error.localizedDescription
            return nil
        }
    }

    public func update(_ folder: SmartFolder, photos: [Photo]) async {
        guard let store, let session else { return }
        do {
            try await store.updateSmartFolder(folder, in: session)
            await reload(store: store, session: session, photos: photos)
        } catch {
            lastError = error.localizedDescription
        }
    }

    public func delete(_ folder: SmartFolder, photos: [Photo]) async {
        guard let store, let session else { return }
        do {
            try await store.deleteSmartFolder(folder, in: session)
            if folder.mirror { await clearMirror(folder.id) }
            await reload(store: store, session: session, photos: photos)
        } catch {
            lastError = error.localizedDescription
        }
    }

    public func setMirror(_ folder: SmartFolder, on: Bool, photos: [Photo]) async {
        guard let store, let session else { return }
        guard let folder = await persisted(folder) else {
            lastError = SmartFolderError.notFound.localizedDescription
            return
        }
        do {
            try await store.setSmartFolderMirror(folder, on: on, in: session)
            if !on { await clearMirror(folder.id) }
            await reload(store: store, session: session, photos: photos)
        } catch {
            lastError = error.localizedDescription
        }
    }

    // MARK: - Mirror to Finder

    /// Refresh mirrored folders after `mirrorDelay` (restarted by every change).
    func scheduleMirror() {
        mirrorTask?.cancel()
        guard folders.contains(where: \.mirror), let store, let session else { return }
        let delay = mirrorDelay
        mirrorTask = Task { [weak self] in
            try? await Task.sleep(for: delay)
            guard !Task.isCancelled, let self, let root = await store.smartFolderMirrorRoot(for: session) else { return }
            await self.syncMirrors(root: root)
        }
    }

    /// Sync every mirrored folder now (also used by tests).
    public func syncMirrors(root: URL) async {
        let photos = photosProvider()
        let jobs = folders.filter(\.mirror).map { f in
            MirrorFolder(id: f.id, name: f.name, members: photos.filter { contains(f, $0) })
        }
        let mirror = SmartFolderMirror(root: root, fs: mirrorFS)
        let failure: String? = await serialized {
            do {
                let r = try mirror.sync(jobs)
                return r.errors.isEmpty ? nil : Self.mirrorErrorPrefix + ": " + r.errors.joined(separator: "; ")
            } catch {
                return "\(Self.mirrorErrorPrefix): \(SmartFolderMirror.describe(error))"
            }
        }
        // A clean sync only clears the mirror's own earlier error, never an unrelated one.
        if let failure {
            lastError = failure
        } else if lastError?.hasPrefix(Self.mirrorErrorPrefix) == true {
            lastError = nil
        }
    }

    nonisolated private static let mirrorErrorPrefix = "Mirror to Finder"

    /// Runs `work` off the main actor after every earlier mirror job has finished.
    private func serialized<T: Sendable>(_ work: @escaping @Sendable () -> T) async -> T {
        let previous = mirrorChain
        let task = Task.detached {
            await previous?.value
            return work()
        }
        mirrorChain = Task { _ = await task.value }
        return await task.value
    }

    /// Wait for a scheduled mirror refresh (tests).
    public func waitForMirror() async { await mirrorTask?.value }

    private func clearMirror(_ id: Int64) async {
        guard let store, let session else { return }
        let fs = mirrorFS
        guard let root = await store.smartFolderMirrorRoot(for: session) else { return }
        await serialized {
            _ = try? SmartFolderMirror(root: root, fs: fs).clear(id: id)
        }
    }
}
