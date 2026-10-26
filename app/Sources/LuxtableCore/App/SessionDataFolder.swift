import Foundation

/// The data folder lt-session keeps in a session (`.lt`: settings, cache, journal) and
/// the one sessions had before the rename. An old session is moved to `.lt` by
/// lt-session the first time it is opened; until then it is still recognized here.
public enum SessionDataFolder {
    public static let name = ".lt"
    /// Migration source: the data folder Darkslide 0.1 kept in each session.
    public static let legacyName = ".ds"

    /// Whether `dir` holds a session's settings, in the current layout or the old one.
    public static func hasSettings(_ dir: URL, fm: FileManager = .default) -> Bool {
        [name, legacyName].contains {
            fm.fileExists(atPath: dir.appendingPathComponent($0).appendingPathComponent("settings/session.json").path)
        }
    }

    /// Whether `dir` has a data folder (either name).
    public static func exists(in dir: URL, fm: FileManager = .default) -> Bool {
        [name, legacyName].contains { fm.fileExists(atPath: dir.appendingPathComponent($0).path) }
    }
}
