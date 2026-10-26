import Foundation

/// Drives that come and go (cards, external disks, network shares): which volumes the
/// open session's photos are on and whether they're mounted, so a photo whose drive merely
/// dropped off is said to be unreachable (never offered for removal), and the store's
/// missing flags are re-checked when a drive or a file comes back.
extension AppModel {
    /// The volume `id`'s original is on (nil: the startup disk, always there).
    func volumeRoot(of id: PhotoID) -> URL? { photo(id)?.url.flatMap(VolumeMount.root(of:)) }

    /// `id`'s volume was found not mounted.
    public func isVolumeGone(_ id: PhotoID) -> Bool {
        guard let root = volumeRoot(of: id) else { return false }
        return volumeMounted[root.path] == false
    }

    /// The volume the open session's folder is on was found not mounted.
    public var sessionVolumeGone: Bool { sessionVolume.map { volumeMounted[$0.path] == false } ?? false }

    /// The volume the open session's folder is on (its recorded mount, else `/Volumes/<name>`).
    var sessionVolume: URL? { currentSession.flatMap { VolumeMount.root(of: $0.url) } }

    /// Remove (which forgets ratings and edits) is offered only for a photo the store
    /// verified missing on a volume known to be mounted: never because a drive dropped off.
    /// Nor on a mounted volume that isn't the session's recorded one.
    public func canRemoveMissing(_ id: PhotoID) -> Bool {
        guard photo(id)?.isMissing == true else { return false }
        guard let root = volumeRoot(of: id) else { return true }
        return volumeMounted[root.path] == true && !volumeMismatch.contains(root.path)
    }

    /// `id` is verified missing, and Remove is held back only because its volume isn't the
    /// recorded one: that volume (what the banner names).
    func otherVolume(of id: PhotoID) -> URL? {
        guard photo(id)?.isMissing == true, let root = volumeRoot(of: id), volumeMounted[root.path] == true,
              volumeMismatch.contains(root.path) else { return nil }
        return root
    }

    /// "Locate Folder…": forget what was recorded about these folders' volumes, so the
    /// open locates (and records) it again.
    func forgetVolumeRecords(_ folders: URL...) {
        for f in folders {
            settings.removeVolumeRecord(for: f)
            VolumeMount.unregister(f)
        }
        volumeMismatch.removeAll()
        ProbeGate.shared.invalidate()
    }

    /// A probe of `root` found it `mounted` (or not): record it, escalate or clear the
    /// session banner, and when it came back, re-check what was missing on it.
    func noteVolume(_ root: URL, mounted: Bool) {
        let was = volumeMounted[root.path]
        let unanswered = volumeUnanswered.remove(root.path) != nil
        guard was != mounted else { return }
        volumeMounted[root.path] = mounted
        AppLog.session.notice("volume \(root.path, privacy: .private(mask: .hash)) \(mounted ? "mounted" : "not mounted", privacy: .public)")
        updateSessionMissing()
        if !mounted {
            if let f = focusedID, volumeRoot(of: f)?.path == root.path { viewer.originalMissing(f) }
        } else if was == false || (was == nil && unanswered) {
            volumeReturned(root) // back (or answering at last after a hung first probe)
        }
    }

    /// The identity recorded for the volume mounted at `root` (nil: none recorded).
    func volumeIdentity(for root: URL) -> String? {
        guard let s = currentSession, let r = settings.volumeRecord(for: s.url), r.mount == root.path else { return nil }
        return r.identity
    }

    /// A check of an original found its volume `root` mounted. Taken as is only where no
    /// identity is recorded; otherwise a probe that compares it decides.
    func originalVolumeMounted(_ root: URL) {
        if volumeIdentity(for: root) == nil {
            noteVolume(root, mounted: true)
        } else if volumeMounted[root.path] != true {
            Task { await refreshVolumes() }
        }
    }

    /// Record (once, while it's there) which volume the open session's folder is on, and
    /// register it so its photos' mount is known even outside `/Volumes`.
    func locateSessionVolume() async {
        guard let s = currentSession else { return }
        if let r = settings.volumeRecord(for: s.url) {
            VolumeMount.register(s.url, r)
            return
        }
        guard mode == .live, let r = await volumeLocate(s.url), currentSession?.url == s.url else { return }
        settings.setVolumeRecord(r, for: s.url)
        VolumeMount.register(s.url, r)
    }

    private func volumeReturned(_ root: URL) {
        recheckedOriginals.removeAll()
        var storeMissing = false
        for item in items.values where item.photo.url.flatMap(VolumeMount.root(of:))?.path == root.path {
            item.originalMissing = false
            storeMissing = storeMissing || item.photo.isMissing
        }
        updateSessionMissing()
        if storeMissing {
            Task { await recheckMissingOriginals() }
        } else {
            viewer.originalBack()
        }
    }

    /// Probe the volumes of the open session, of its missing photos and of any seen before
    /// (each off the main thread, rationed per volume by `ProbeGate`). Returns the roots
    /// that answered "mounted" and the recorded volume in this round: one that hung, or
    /// isn't the recorded volume, isn't in it.
    @discardableResult
    public func refreshVolumes() async -> Set<String> {
        await locateSessionVolume()
        var roots = Set(volumeMounted.keys)
        if let r = sessionVolume { roots.insert(r.path) }
        for item in items.values where item.isOriginalMissing {
            if let r = item.photo.url.flatMap(VolumeMount.root(of:)) { roots.insert(r.path) }
        }
        let probe = volumeProbe
        var up: Set<String> = []
        for path in roots.sorted() {
            let root = URL(fileURLWithPath: path, isDirectory: true)
            guard let found = await probe(root, volumeIdentity(for: root)) else {
                if volumeMounted[path] == nil { volumeUnanswered.insert(path) }
                continue
            }
            if found.mounted, found.sameVolume { up.insert(path) }
            if found.mounted, !found.sameVolume {
                if volumeMismatch.insert(path).inserted {
                    AppLog.session.notice("volume \(path, privacy: .private(mask: .hash)) isn't the recorded one: Remove is held back")
                }
            } else {
                volumeMismatch.remove(path)
            }
            noteVolume(root, mounted: found.mounted)
        }
        return up
    }

    /// The focused photo's original is there although the store says it's missing (a
    /// drive or a file came back): re-check the store, once per photo (a file that is
    /// back but can't be relinked doesn't loop).
    func storeMissingFound(_ id: PhotoID) {
        guard recheckedOriginals.insert(id).inserted else { return }
        Task { await recheckMissingOriginals() }
    }

    /// Open the session folder again (the store relinks originals that are back), then
    /// list it again keeping the focused photo and the undo history. One at a time.
    func recheckMissingOriginals() async {
        guard !recheckingOriginals, let s = currentSession, let store = sessions as? any FolderSessionOpening else { return }
        recheckingOriginals = true
        defer { recheckingOriginals = false }
        await saver.flush()
        do {
            _ = try await store.add(folder: s.url, progress: { _ in })
        } catch {
            AppLog.session.error("re-checking missing originals failed: \(error.localizedDescription, privacy: .private(mask: .hash))")
            return
        }
        await refreshCurrentSession(keepingHistory: true)
    }

    /// A drive mounted or unmounted: answers about volumes may have changed.
    func volumesChanged() {
        ProbeGate.shared.invalidate()
        Task { await refreshVolumes() }
    }

    /// Follow mounts and unmounts (live mode only).
    func startMountWatcher() {
        guard mode == .live, mountObservers.isEmpty else { return }
        let center = PlatformWorkspace.notificationCenter
        mountObservers = [PlatformWorkspace.didMount, PlatformWorkspace.didUnmount].map { name in
            center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                Task { @MainActor in self?.volumesChanged() }
            }
        }
    }

    func stopMountWatcher() {
        for o in mountObservers { PlatformWorkspace.notificationCenter.removeObserver(o) }
        mountObservers = []
    }
}
