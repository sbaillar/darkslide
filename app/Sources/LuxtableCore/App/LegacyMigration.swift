import Foundation
import LuxtableFFI

/// The first launch after the rename: the app had another name in 0.1, with its own
/// preferences domain, support folder and Keychain services. Luxtable copies what it
/// finds there into its own places and never deletes or changes the old ones, so going
/// back to version 0.1 still works. Sessions move to `.lt` on their own, when opened
/// (lt-session); Keychain keys are copied the first time each is needed
/// (`KeychainItem.keychain`), so an integration that is off never reads its old key.
///
/// - Preferences: every key of the old domain the new one doesn't have, once.
/// - The sessions folder: a user who never chose one used the old sessions folder in `~/Pictures`; it
///   stays their sessions folder (nothing in it is moved).
/// - The support folder: copied (APFS clones) to the new name when the new one isn't
///   there yet, then put in place with one rename; the global index's records of the
///   stores kept in it follow. The old app's update leftovers aren't copied.
public enum LegacyMigration {
    /// Migration source: the old app's bundle identifier (its preferences domain).
    public static let legacyBundleID = "casa.baillargeon.darkslide"
    /// Migration source: the old app's folder name (support folder, default sessions folder).
    public static let legacyName = "Darkslide"
    /// Set once the preferences were copied (a key reset later isn't copied again).
    static let defaultsDoneKey = "legacyMigration.defaultsCopied"
    /// Support-folder entries that belong to the old app's updater, not to the user.
    static let notCopied: Set<String> = ["Updates", "Previous Version", ".DS_Store"]

    /// What the launch migration did, for the log.
    public struct Report: Equatable, Sendable {
        public var defaultsCopied = 0
        public var sessionsFolder: URL?
        public var supportCopied = false
        public var storesRelocated = 0
        public var problems: [String] = []
    }

    /// The app's launch: only for the user's own preferences and support folder (never a
    /// `--defaults-suite`, `LT_SUPPORT_ROOT`, `--snapshot` or test run).
    @discardableResult
    @MainActor public static func runAtLaunch() -> Report {
        guard !AppModel.usesPrivateDefaults, Bundle.main.bundleIdentifier == "app.luxtable.Luxtable" else { return Report() }
        let fm = FileManager.default
        let pictures = fm.urls(for: .picturesDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Pictures")
        var report = migrateDefaults(into: .standard, domain: "app.luxtable.Luxtable",
                                     old: UserDefaults.standard.persistentDomain(forName: legacyBundleID),
                                     pictures: pictures)
        let support = AppPaths.support
        let old = support.deletingLastPathComponent().appendingPathComponent(legacyName, isDirectory: true)
        let copied = migrateSupport(from: old, to: support) { from, to in
            Int(try LuxtableFFI.relocateGlobalIndex(index: to.appendingPathComponent("global.db").path,
                                                    from: from.path, to: to.path))
        }
        report.supportCopied = copied.copied
        report.storesRelocated = copied.relocated
        report.problems += copied.problems
        if report != Report() {
            AppLog.migration.notice("""
                from the earlier app: \(report.defaultsCopied) preferences, support folder \
                \(report.supportCopied ? "copied" : "not copied") (\(report.storesRelocated) stores followed), \
                sessions folder \(report.sessionsFolder == nil ? "unchanged" : "kept")
                """)
            for p in report.problems {
                AppLog.migration.error("\(p, privacy: .private(mask: .hash))")
            }
        }
        return report
    }

    /// Copy the keys of the old domain (`old`) that `defaults` (the new `domain`) doesn't
    /// have, once. A user who never chose a sessions folder keeps the old default one.
    static func migrateDefaults(into defaults: UserDefaults, domain: String, old: [String: Any]?,
                                pictures: URL, fm: FileManager = .default) -> Report {
        var report = Report()
        guard let old, !old.isEmpty, !defaults.bool(forKey: defaultsDoneKey) else { return report }
        let mine = defaults.persistentDomain(forName: domain) ?? [:]
        for (key, value) in old.sorted(by: { $0.key < $1.key }) where mine[key] == nil {
            defaults.set(value, forKey: key)
            report.defaultsCopied += 1
        }
        let sessionsKey = "sessionsFolder"
        if defaults.object(forKey: sessionsKey) == nil {
            var isDir: ObjCBool = false
            let oldFolder = pictures.appendingPathComponent(legacyName, isDirectory: true)
            if fm.fileExists(atPath: oldFolder.path, isDirectory: &isDir), isDir.boolValue,
               !fm.fileExists(atPath: pictures.appendingPathComponent("Luxtable").path) {
                defaults.set(oldFolder.path, forKey: sessionsKey)
                report.sessionsFolder = oldFolder
            }
        }
        defaults.set(true, forKey: defaultsDoneKey)
        return report
    }

    /// Copy the old support folder `old` to `new` when `new` doesn't exist yet (or is
    /// empty): into a hidden sibling first, then one rename, so `new` never holds half a
    /// copy. `old` is never changed. `relocate(old, new)` then fixes paths recorded in the copy.
    static func migrateSupport(from old: URL, to new: URL, fm: FileManager = .default,
                               relocate: (URL, URL) throws -> Int) -> (copied: Bool, relocated: Int, problems: [String]) {
        var isDir: ObjCBool = false
        guard fm.fileExists(atPath: old.path, isDirectory: &isDir), isDir.boolValue,
              old.standardizedFileURL.path != new.standardizedFileURL.path else { return (false, 0, []) }
        if let existing = try? fm.contentsOfDirectory(atPath: new.path),
           !existing.allSatisfy({ $0 == ".DS_Store" }) {
            return (false, 0, [])
        } else if fm.fileExists(atPath: new.path) {
            try? fm.removeItem(at: new) // empty: nothing of the user's
        }
        let staging = new.deletingLastPathComponent()
            .appendingPathComponent(".\(new.lastPathComponent).migrating-\(UUID().uuidString)", isDirectory: true)
        do {
            try fm.createDirectory(at: staging, withIntermediateDirectories: true)
            for name in try fm.contentsOfDirectory(atPath: old.path) where !notCopied.contains(name) {
                try fm.copyItem(at: old.appendingPathComponent(name), to: staging.appendingPathComponent(name))
            }
            try fm.moveItem(at: staging, to: new)
        } catch {
            try? fm.removeItem(at: staging) // our own partial copy; the old folder is untouched
            return (false, 0, ["The support folder couldn’t be copied from \(old.path): \(error.localizedDescription)"])
        }
        do {
            return (true, try relocate(old, new), [])
        } catch {
            return (true, 0, ["The global index couldn’t follow the copied support folder: \(error.localizedDescription)"])
        }
    }
}
