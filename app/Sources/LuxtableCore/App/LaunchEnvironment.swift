import Foundation

/// Environment variables the app (and its tests and scripts) read: `LT_*`. The `DS_*`
/// spelling they had before the rename is still accepted for one release, so a CI
/// configuration or a habit doesn't break silently; `LT_*` wins when both are set.
public enum LaunchEnvironment {
    /// Migration source: the prefix the variables had before the rename.
    static let legacyPrefix = "DS_"

    /// The value of `name` (`LT_…`), else of its old `DS_…` spelling.
    public static func value(_ name: String,
                             in environment: [String: String] = ProcessInfo.processInfo.environment) -> String? {
        if let v = environment[name] { return v }
        guard name.hasPrefix("LT_") else { return nil }
        return environment[legacyPrefix + name.dropFirst(3)]
    }
}
