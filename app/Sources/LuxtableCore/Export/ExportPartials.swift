import Darwin
import Foundation

/// Unfinished export files: lt-export writes each output to a hidden temp
/// `.lt-<pid hex>-<n hex>.partial` in the destination and renames it when complete. An
/// export stopped by quitting (or a crash) leaves the temp behind; these are swept on
/// the next export into that folder and at launch.
///
/// Only files matching exactly that pattern are touched, never anything else, and
/// never temps of a process that is still running (another Luxtable exporting there).
public enum ExportPartials {
    static let foldersKey = "exportFoldersWithPartials"
    /// Folders remembered for the launch sweep.
    static let rememberLimit = 32

    /// Migration source: the prefix Darkslide 0.1 gave the same temps (swept like ours).
    static let legacyPrefix = ".ds-"

    /// Whether `name` is one of our export temps, and its writer's pid.
    static func writerPID(of name: String) -> pid_t? {
        guard name.hasPrefix(".lt-") || name.hasPrefix(legacyPrefix), name.hasSuffix(".partial") else { return nil }
        let middle = name.dropFirst(4).dropLast(8).split(separator: "-", omittingEmptySubsequences: false)
        guard middle.count == 2, !middle[1].isEmpty, middle[1].allSatisfy(\.isHexDigit),
              let pid = Int32(middle[0], radix: 16), pid > 0 else { return nil }
        return pid
    }

    public static func isRunning(_ pid: pid_t) -> Bool {
        kill(pid, 0) == 0 || errno == EPERM
    }

    /// Remove stale export temps directly inside `folder`. Returns the names removed.
    @discardableResult
    public static func sweep(_ folder: URL, isAlive: (pid_t) -> Bool = isRunning) -> [String] {
        guard let names = try? FileManager.default.contentsOfDirectory(atPath: folder.path) else { return [] }
        var removed: [String] = []
        for name in names {
            guard let pid = writerPID(of: name), !isAlive(pid) else { continue }
            let url = folder.appendingPathComponent(name)
            // Regular files only (never follow a symlink someone named like a temp).
            guard let v = try? url.resourceValues(forKeys: [.isRegularFileKey, .isSymbolicLinkKey]),
                  v.isRegularFile == true, v.isSymbolicLink != true else { continue }
            if (try? FileManager.default.removeItem(at: url)) != nil { removed.append(name) }
        }
        return removed
    }

    /// Remember export folders for the next launch's sweep.
    static func remember(_ folders: [URL], defaults: UserDefaults) {
        var list = defaults.stringArray(forKey: foldersKey) ?? []
        for f in folders.map(\.path) where !list.contains(f) { list.append(f) }
        defaults.set(Array(list.suffix(rememberLimit)), forKey: foldersKey)
    }

    /// Whether `folder` can't be listed (an unmounted drive) or still holds an export temp
    /// (a live writer's, or one that couldn't be removed): it is swept again next launch.
    static func needsAnotherSweep(_ folder: URL) -> Bool {
        guard let names = try? FileManager.default.contentsOfDirectory(atPath: folder.path) else { return true }
        return names.contains { writerPID(of: $0) != nil }
    }

    /// The launch sweep over remembered folders (off the main thread). Folders swept clean
    /// are forgotten; one that is unreachable or still holds a temp is kept for next time.
    /// Runs on the caller's actor so `defaults` is never sent; only the folder paths are.
    @discardableResult
    nonisolated(nonsending) public static func sweepRemembered(defaults: UserDefaults) async -> Int {
        let remembered = defaults.stringArray(forKey: foldersKey) ?? []
        guard !remembered.isEmpty else { return 0 }
        let (removed, keep) = await Task.detached(priority: .utility) { () -> (Int, [String]) in
            var removed = 0
            var keep: [String] = []
            for path in remembered {
                let folder = URL(fileURLWithPath: path)
                removed += sweep(folder).count
                if needsAnotherSweep(folder) { keep.append(path) }
            }
            return (removed, keep)
        }.value
        if keep.isEmpty { defaults.removeObject(forKey: foldersKey) } else { defaults.set(keep, forKey: foldersKey) }
        return removed
    }
}
