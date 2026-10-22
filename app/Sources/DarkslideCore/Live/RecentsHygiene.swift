import Foundation

/// Which recent folders are worth listing and reopening: one that's gone, holds no
/// originals any more (a cleaned-out folder with only `.ds/` left), or lives in a
/// temporary directory (a dev or test artifact) is never reopened on its own.
public enum RecentsHygiene {
    /// Temporary locations: recents there are dev or test artifacts.
    public static let temporaryRoots = ["/tmp/", "/private/tmp/", "/private/var/folders/", "/var/folders/"]

    /// `url` is in a temporary directory.
    public static func isTemporary(_ url: URL) -> Bool {
        let path = url.standardizedFileURL.path + "/"
        return temporaryRoots.contains { path.hasPrefix($0) }
    }

    /// Temporary recents are ignored unless this process itself runs in a temporary
    /// support root (tests, snapshot and dev runs), where every folder it opens is one.
    public static var ignoresTemporaryFolders: Bool { !isTemporary(AppPaths.support) }

    /// What counts as a photo or video: the RAW types ds-raw reads (what a session open
    /// lists) and videos.
    static let mediaExtensions = RustRaw.extensions.union(Photo.videoExtensions)

    /// Whether `folder` holds at least one photo or video file. A bounded walk: hidden
    /// folders (`.ds`) and a top-level `Trash/` are skipped, at most `budget` entries and
    /// `depth` levels are looked at. `nil` (no answer) when the budget ran out, or folders
    /// deeper than `depth` (or one that couldn't be read) went unlooked, before a photo was
    /// found.
    static func hasOriginals(_ folder: URL, budget: Int = 5000, depth: Int = 6) -> Bool? {
        let unreadable = Flag()
        guard let walk = FileManager.default.enumerator(at: folder, includingPropertiesForKeys: [.isDirectoryKey],
                                                        options: [.skipsHiddenFiles, .skipsPackageDescendants],
                                                        errorHandler: { _, _ in unreadable.set(); return true }) else {
            return nil
        }
        var seen = 0
        var unlooked = false
        for case let url as URL in walk {
            seen += 1
            if seen > budget { return nil }
            if walk.level == 1, url.lastPathComponent == "Trash" {
                walk.skipDescendants()
                continue
            }
            if mediaExtensions.contains(url.pathExtension.lowercased()) { return true }
            if walk.level >= depth, (try? url.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true {
                walk.skipDescendants()
                unlooked = true
            }
        }
        return unlooked || unreadable.isSet ? nil : false
    }

    /// `folder` is a session Darkslide made (its settings and a Capture folder).
    static func isSessionLayout(_ folder: URL) -> Bool {
        let fm = FileManager.default
        var isDir: ObjCBool = false
        return fm.fileExists(atPath: folder.appendingPathComponent(".ds/settings/session.json").path)
            && fm.fileExists(atPath: folder.appendingPathComponent("Capture").path, isDirectory: &isDir) && isDir.boolValue
    }

    /// Set from the enumerator's error handler.
    private final class Flag: @unchecked Sendable {
        private let lock = NSLock()
        private var on = false
        func set() { lock.withLock { on = true } }
        var isSet: Bool { lock.withLock { on } }
    }

    /// Why `folder` isn't offered as a session, or nil when it is fine — or couldn't be
    /// checked within `timeout` (a slow or hung network drive isn't penalised, and never
    /// stalls the caller). Probes are rationed per volume (`ProbeGate`): a volume that
    /// isn't mounted makes its folders "drive not connected" without a walk, and while a
    /// probe of a hung volume is stuck no new one starts there.
    public static func unavailableReason(_ folder: URL, timeout: Double = 1) async -> SessionInfo.Unavailable? {
        if let root = VolumeMount.root(of: folder) {
            guard let found = await VolumeMount.probe(root, timeout: timeout) else { return nil }
            if !found.mounted { return .driveNotConnected }
        }
        let reason = await ProbeGate.shared.run(key: "recent:\(folder.path)", group: VolumeMount.group(of: folder),
                                                timeout: timeout) { () -> SessionInfo.Unavailable? in
            var isDir: ObjCBool = false
            guard FileManager.default.fileExists(atPath: folder.path, isDirectory: &isDir), isDir.boolValue else {
                return .notFound
            }
            // A session made empty (New Session…) is a session, not a cleaned-out folder.
            return hasOriginals(folder) == false && !isSessionLayout(folder) ? .noPhotos : nil
        }
        return reason ?? nil
    }
}
