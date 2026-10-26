import Foundation
import LuxtableCore

// MARK: - UserDefaults suites

/// Test UserDefaults suites. Every suite is a *path* suite (an absolute path is a
/// plist file for CFPreferences) under the test run's own folder:
/// `$LT_SUPPORT_ROOT/test-preferences/` (set by scripts/test-app.sh), else a temporary
/// folder. Tests never create domains in `~/Library/Preferences` (the old
/// `luxtable.tests.<UUID>` domains leaked hundreds of plists there, because cfprefsd
/// writes them back after the exit-time delete).
enum TestDefaults {
    /// Prefix of the legacy domains in `~/Library/Preferences` (only swept, never created).
    static let legacyPrefix = "luxtable.tests."
    private static let lock = NSLock()
    nonisolated(unsafe) private static var suites: [String] = []
    nonisolated(unsafe) private static var installed = false

    /// Where this run's suites live.
    static let root: URL = {
        let env = LaunchEnvironment.value("LT_SUPPORT_ROOT").flatMap { $0.isEmpty ? nil : $0 }
        let base = env.map { URL(fileURLWithPath: $0, isDirectory: true) }
            ?? FileManager.default.temporaryDirectory.appendingPathComponent("luxtable-tests-\(getpid())", isDirectory: true)
        let dir = base.appendingPathComponent("test-preferences", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }()

    static func register(_ suite: String) {
        lock.withLock {
            suites.append(suite)
            if !installed {
                installed = true
                atexit { TestDefaults.cleanUp() }
                sweepStale()
            }
        }
    }

    /// Remove every registered suite now (also runs at exit). Returns how many plists were deleted.
    @discardableResult
    static func cleanUp() -> Int {
        let names = lock.withLock { () -> [String] in
            defer { suites.removeAll() }
            return suites
        }
        return names.filter(remove).count
    }

    /// Delete one suite's keys and plist. `true` if a plist was deleted.
    @discardableResult
    static func remove(_ suite: String) -> Bool {
        UserDefaults(suiteName: suite)?.removePersistentDomain(forName: suite)
        CFPreferencesAppSynchronize(suite as CFString)
        return unlink(plistPath(suite)) == 0
    }

    /// Earlier test runs (before path suites) left `luxtable.tests.*` plists in
    /// `~/Library/Preferences`; each run deletes the ones untouched for 10 minutes.
    /// Only that prefix is ever touched.
    @discardableResult
    static func sweepStale(olderThan age: TimeInterval = 600) -> Int {
        let dir = NSHomeDirectory() + "/Library/Preferences"
        let fm = FileManager.default
        let cutoff = Date().addingTimeInterval(-age)
        var removed = 0
        for name in (try? fm.contentsOfDirectory(atPath: dir)) ?? []
        where name.hasPrefix(legacyPrefix) && name.hasSuffix(".plist") {
            let path = dir + "/" + name
            guard let m = (try? fm.attributesOfItem(atPath: path))?[.modificationDate] as? Date, m < cutoff else { continue }
            if unlink(path) == 0 { removed += 1 }
        }
        return removed
    }

    /// The plist of a suite: a path suite is `<path>.plist`; a named domain lives in
    /// `~/Library/Preferences`.
    static func plistPath(_ suite: String) -> String {
        suite.hasPrefix("/") ? suite + ".plist" : NSHomeDirectory() + "/Library/Preferences/\(suite).plist"
    }
}

/// A fresh, registered path suite under this run's test folder.
func testSuiteName() -> String {
    let s = TestDefaults.root.appendingPathComponent(UUID().uuidString).path
    TestDefaults.register(s)
    return s
}

/// A fresh, empty, registered UserDefaults suite.
func testDefaults() -> UserDefaults {
    let s = testSuiteName()
    let d = UserDefaults(suiteName: s)!
    d.removePersistentDomain(forName: s)
    return d
}
