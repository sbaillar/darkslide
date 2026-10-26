import Foundation
import Testing
import LuxtableFFI
@testable import LuxtableCore

/// The first launch after the rename: preferences, the support folder and Keychain keys
/// come over from the old app's places, which are never changed; environment variables
/// keep their old spelling for one release.
@MainActor @Suite struct LegacyMigrationTests {
    static func temp(_ name: String) -> URL {
        let u = FileManager.default.temporaryDirectory.appendingPathComponent("lt-legacy-\(name)-\(UUID())", isDirectory: true)
        try? FileManager.default.createDirectory(at: u, withIntermediateDirectories: true)
        return u
    }

    // MARK: - Preferences

    @Test func preferencesAreCopiedOnceWithoutOverwriting() throws {
        let suite = testSuiteName()
        let d = try #require(UserDefaults(suiteName: suite))
        d.removePersistentDomain(forName: suite)
        d.set("mine", forKey: "artist")
        let pictures = Self.temp("pictures")
        defer { try? FileManager.default.removeItem(at: pictures) }
        let old: [String: Any] = ["artist": "theirs", "archiveTarget": "/Volumes/NAS/Photos", "rating": 3]
        let r = LegacyMigration.migrateDefaults(into: d, domain: suite, old: old, pictures: pictures)
        #expect(r.defaultsCopied == 2)
        #expect(d.string(forKey: "artist") == "mine", "a value already set is kept")
        #expect(d.string(forKey: "archiveTarget") == "/Volumes/NAS/Photos")
        #expect(d.integer(forKey: "rating") == 3)
        #expect(r.sessionsFolder == nil, "no old sessions folder: nothing to keep")
        // A second launch does nothing, even for a key reset since.
        d.removeObject(forKey: "rating")
        #expect(LegacyMigration.migrateDefaults(into: d, domain: suite, old: old, pictures: pictures) == .init())
        #expect(d.object(forKey: "rating") == nil)
        // Nothing to copy from: nothing happens.
        #expect(LegacyMigration.migrateDefaults(into: d, domain: suite, old: nil, pictures: pictures) == .init())
    }

    @Test func theOldDefaultSessionsFolderStaysTheSessionsFolder() throws {
        let pictures = Self.temp("pictures")
        defer { try? FileManager.default.removeItem(at: pictures) }
        let oldFolder = pictures.appendingPathComponent(LegacyMigration.legacyName)
        try FileManager.default.createDirectory(at: oldFolder.appendingPathComponent("2026-05-01 Hike"),
                                                withIntermediateDirectories: true)
        let suite = testSuiteName()
        let d = try #require(UserDefaults(suiteName: suite))
        d.removePersistentDomain(forName: suite)
        let r = LegacyMigration.migrateDefaults(into: d, domain: suite, old: ["x": 1], pictures: pictures)
        #expect(r.sessionsFolder?.path == oldFolder.path)
        #expect(d.string(forKey: "sessionsFolder") == oldFolder.path)
        #expect(AppSettings(defaults: d).sessionsFolder.path == oldFolder.path)
        #expect(FileManager.default.fileExists(atPath: oldFolder.appendingPathComponent("2026-05-01 Hike").path),
                "nothing in it moved")
        // One the user chose is theirs; a new default folder already in use wins too.
        for chosen in [true, false] {
            let s = testSuiteName()
            let e = try #require(UserDefaults(suiteName: s))
            e.removePersistentDomain(forName: s)
            if chosen { e.set("/Volumes/Photos/Sessions", forKey: "sessionsFolder") } else {
                try FileManager.default.createDirectory(at: pictures.appendingPathComponent("Luxtable"),
                                                        withIntermediateDirectories: true)
            }
            #expect(LegacyMigration.migrateDefaults(into: e, domain: s, old: ["x": 1], pictures: pictures).sessionsFolder == nil)
        }
    }

    // MARK: - The support folder

    @Test func theSupportFolderIsCopiedAndTheOldOneStays() throws {
        let base = Self.temp("support")
        defer { try? FileManager.default.removeItem(at: base) }
        let fm = FileManager.default
        let old = base.appendingPathComponent("Old"), new = base.appendingPathComponent("New")
        let files = ["global.db": "db", "global.db-wal": "wal", "resume.json": "{}", "Plugins/state.json": "{}",
                     "Helpers/old-rawler": "bin", "InPlace/v/a/.ds/settings/session.json": "{}",
                     "Updates/Ready/x": "app", "Previous Version/y": "app"]
        for (rel, text) in files {
            let u = old.appendingPathComponent(rel)
            try fm.createDirectory(at: u.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data(text.utf8).write(to: u)
        }
        try fm.createDirectory(at: new, withIntermediateDirectories: true) // empty: made by something else
        var relocated: [(URL, URL)] = []
        let r = LegacyMigration.migrateSupport(from: old, to: new) { relocated.append(($0, $1)); return 2 }
        #expect(r.copied && r.relocated == 2 && r.problems.isEmpty)
        #expect(relocated.count == 1 && relocated.first?.0 == old && relocated.first?.1 == new)
        for (rel, text) in files {
            #expect(try String(contentsOf: old.appendingPathComponent(rel), encoding: .utf8) == text, "\(rel) stays")
            let copied = try? String(contentsOf: new.appendingPathComponent(rel), encoding: .utf8)
            if rel.hasPrefix("Updates") || rel.hasPrefix("Previous Version") {
                #expect(copied == nil, "the old app's update leftovers aren't copied")
            } else {
                #expect(copied == text, "\(rel) copied")
            }
        }
        #expect(try fm.contentsOfDirectory(atPath: base.path).sorted() == ["New", "Old"], "no staging left")
        // A second launch does nothing.
        try Data("changed".utf8).write(to: new.appendingPathComponent("resume.json"))
        let again = LegacyMigration.migrateSupport(from: old, to: new) { _, _ in Issue.record("relocated again"); return 0 }
        #expect(!again.copied)
        #expect(try String(contentsOf: new.appendingPathComponent("resume.json"), encoding: .utf8) == "changed")
        // No old folder, or the same folder: nothing.
        #expect(!LegacyMigration.migrateSupport(from: base.appendingPathComponent("none"), to: new) { _, _ in 0 }.copied)
        #expect(!LegacyMigration.migrateSupport(from: new, to: new) { _, _ in 0 }.copied)
    }

    @Test func aCopyThatFailsLeavesNothingHalfDone() throws {
        let base = Self.temp("support-fail")
        let fm = FileManager.default
        let old = base.appendingPathComponent("Old"), new = base.appendingPathComponent("New")
        let locked = old.appendingPathComponent("Locked")
        try fm.createDirectory(at: locked, withIntermediateDirectories: true)
        try Data("x".utf8).write(to: locked.appendingPathComponent("secret"))
        try Data("db".utf8).write(to: old.appendingPathComponent("global.db"))
        try fm.setAttributes([.posixPermissions: 0o000], ofItemAtPath: locked.path)
        defer {
            try? fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: locked.path)
            try? fm.removeItem(at: base)
        }
        let r = LegacyMigration.migrateSupport(from: old, to: new) { _, _ in 0 }
        #expect(!r.copied && r.problems.count == 1)
        #expect(!fm.fileExists(atPath: new.path), "no half copy in place")
        #expect(try fm.contentsOfDirectory(atPath: base.path) == ["Old"], "the staging copy is gone")
        // The global index not following is said, but the copy stands.
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: locked.path)
        struct Nope: Error {}
        let late = LegacyMigration.migrateSupport(from: old, to: new) { _, _ in throw Nope() }
        #expect(late.copied && late.relocated == 0 && late.problems.count == 1)
    }

    // MARK: - Keychain

    /// A Keychain item in memory that counts its lookups.
    final class Counting: @unchecked Sendable {
        private let lock = NSLock()
        private var value: String?
        private(set) var reads = 0
        var failing = false
        init(_ value: String? = nil) { self.value = value }
        var stored: String? { lock.withLock { value } }
        var item: KeychainItem {
            KeychainItem(read: { self.lookup().value }, write: { v in
                self.lock.withLock { self.value = v.flatMap { $0.isEmpty ? nil : $0 } }
                return true
            }, lookup: { self.lookup() })
        }
        private func lookup() -> KeychainLookup {
            lock.withLock {
                reads += 1
                if failing { return .failed }
                return value.map(KeychainLookup.found) ?? .notFound
            }
        }
    }

    static func flag() -> (KeychainItem.MigrationFlag, TestKeychain.Flag) {
        let f = TestKeychain.Flag()
        return (KeychainItem.MigrationFlag(get: { f.value }, set: { f.value = true }), f)
    }

    @Test func aKeyIsCopiedOnFirstUseAndNeverBroughtBackAfterwards() {
        let old = Counting("old-key"), new = Counting()
        let (done, settled) = Self.flag()
        let item = new.item.migrating(from: old.item, done: done)
        #expect(old.reads == 0, "nothing is read before the key is needed")
        #expect(item.read() == "old-key")
        #expect(new.stored == "old-key" && old.stored == "old-key", "copied; the old one stays")
        #expect(settled.value)
        #expect(item.read() == "old-key")
        #expect(old.reads == 1, "only the first use looks at the old item")
        // Signed out: the key is gone and doesn't come back from the old item.
        #expect(item.write(nil))
        #expect(item.read() == nil && item.lookup() == .notFound)
        #expect(old.reads == 1 && old.stored == "old-key")
    }

    @Test func aKeyChainThatCantBeReadIsAskedAgain() {
        let old = Counting("k"), new = Counting()
        old.failing = true
        let (done, settled) = Self.flag()
        let item = new.item.migrating(from: old.item, done: done)
        #expect(item.lookup() == .notFound)
        #expect(!settled.value, "not settled: asked again next time")
        old.failing = false
        #expect(item.read() == "k")
        // Nothing saved before the rename: settled at once, never looked at again.
        let none = Counting(), fresh = Counting()
        let (done2, settled2) = Self.flag()
        let other = fresh.item.migrating(from: none.item, done: done2)
        #expect(other.read() == nil && settled2.value)
        _ = other.read()
        #expect(none.reads == 1)
        // A key saved in the new place first wins; the old one is never read.
        let new3 = Counting("new"), old3 = Counting("old")
        let (done3, _) = Self.flag()
        #expect(new3.item.migrating(from: old3.item, done: done3).read() == "new" && old3.reads == 0)
        // Copying failed: the key is still used this time.
        let ro = KeychainItem(read: { nil }, write: { _ in false })
        let (done4, settled4) = Self.flag()
        #expect(ro.migrating(from: Counting("o").item, done: done4).read() == "o" && !settled4.value)
    }

    @Test func theAppsItemsComeFromTheOldServices() {
        #expect(KeychainItem.legacyService(ImmichSettings.service) == "casa.baillargeon.darkslide.immich")
        #expect(KeychainItem.legacyService(GooglePhotosSettings.service) == "casa.baillargeon.darkslide.google-photos")
        #expect(KeychainItem.legacyService(ArchiveObjectKeys.service) == "casa.baillargeon.darkslide.archive")
        #expect(KeychainItem.legacyService(JevSettings.service) == "casa.baillargeon.darkslide.jev")
        #expect(KeychainItem.legacyService("com.example.other") == nil)
        // In a test run the items live in memory: an old item there is found through the new one.
        let account = "test-\(UUID())"
        #expect(KeychainItem.stored(service: "casa.baillargeon.darkslide.immich", account: account).write("abc"))
        let item = KeychainItem.keychain(service: ImmichSettings.service, account: account)
        #expect(item.read() == "abc")
        #expect(KeychainItem.stored(service: ImmichSettings.service, account: account).read() == "abc")
        #expect(KeychainItem.keychain(service: "com.example.other", account: account).read() == nil)
    }

    /// Settings objects of the integrations never touch the Keychain until a key is asked
    /// for: an integration that is off never reads (or migrates) its key.
    @Test func settingUpTheIntegrationsReadsNoKey() {
        let keys = [Counting("a"), Counting("b"), Counting("c"), Counting("d"), Counting("e")]
        let defaults = testDefaults()
        _ = ImmichSettings(defaults: defaults, apiKey: keys[0].item)
        _ = GooglePhotosSettings(defaults: defaults, clientSecret: keys[1].item, refreshToken: keys[2].item)
        _ = ArchiveObjectKeys(accessKeyID: keys[3].item, secret: keys[4].item)
        #expect(keys.allSatisfy { $0.reads == 0 })
    }

    // MARK: - Sessions from before the rename

    @Test func anOldSessionIsRecognizedAndMovesToLtWhenOpened() async throws {
        let base = Self.temp("session")
        defer { try? FileManager.default.removeItem(at: base) }
        let fm = FileManager.default
        let dir = base.appendingPathComponent("Hike")
        _ = try await LuxtableFFI.createSession(dir: dir.path, name: "Hike", token: CancelToken())
        // What Darkslide 0.1 left: `.ds` and `Hike.dsession`.
        try fm.moveItem(at: dir.appendingPathComponent(".lt"), to: dir.appendingPathComponent(".ds"))
        try fm.moveItem(at: dir.appendingPathComponent("Hike.ltsession"), to: dir.appendingPathComponent("Hike.dsession"))
        #expect(SessionDataFolder.hasSettings(dir) && SessionDataFolder.exists(in: dir))
        #expect(RustImportService.isImportSession(dir))
        #expect(RecentsHygiene.isSessionLayout(dir))
        #expect(RustSessionStore.hasSession(dir))
        #expect(SessionLocation.problem(dir.appendingPathComponent("Capture/sub"), sessions: []) != nil,
                "inside an old session too")
        let h = try await LuxtableFFI.openSession(dir: dir.path, token: CancelToken())
        #expect(try h.openReport().legacy == "migrated")
        #expect(fm.fileExists(atPath: dir.appendingPathComponent(".lt/settings/session.json").path))
        #expect(!fm.fileExists(atPath: dir.appendingPathComponent(".ds").path))
        #expect(!SessionDataFolder.hasSettings(base))
        for l in ["migrated", "both", ""] { RustSessionStore.logLegacy(l, dir) }
    }

    // MARK: - Environment variables

    @Test func environmentVariablesKeepTheirOldNamesForARelease() {
        #expect(LaunchEnvironment.value("LT_X", in: ["LT_X": "new", "DS_X": "old"]) == "new")
        #expect(LaunchEnvironment.value("LT_X", in: ["DS_X": "old"]) == "old")
        #expect(LaunchEnvironment.value("OTHER", in: ["DS_OTHER": "old"]) == nil)
        #expect(LaunchEnvironment.value("LT_Y", in: [:]) == nil)
        #expect(AppPaths.support(environment: ["DS_SUPPORT_ROOT": "/tmp/x"], arguments: []).path == "/tmp/x")
    }
}
