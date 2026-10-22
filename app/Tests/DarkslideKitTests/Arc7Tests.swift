import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideKit

/// Integration A review fixes for the archive and Amazon hand-off (A-H1…A-L5).

private let onSnap = ArchiveSettingsSnapshot(enabled: true, target: "/Volumes/pics/Darkslide", intervalMinutes: 10,
                                             amazonFolder: nil)

private func overview7(available: Bool = true, sessions: [SessionArchiveStatus] = [], runs: [ArchiveRun] = [],
                       amazon: AmazonStatus = AmazonStatus(folder: nil, running: false, lastRunMs: nil, placed: 0, cloned: 0,
                                                           alreadyMirrored: 0, skippedUnsupported: 0, waiting: 0,
                                                           failed: [], error: nil, folderMissing: false, session: nil))
    -> ArchiveOverview {
    ArchiveOverview(enabled: true,
                    target: TargetCheck(path: "/Volumes/pics/Darkslide", available: available, timedOut: false,
                                        reason: available ? nil : "not mounted", fsType: nil),
                    rsync: nil, rsyncError: nil, sessions: sessions, runs: runs, amazon: amazon)
}

@Suite struct Arc7MappingTests {
    /// A-H1: items left out of the archive are "Archived with problems", not
    /// "Archived"; a stuck run is an error; problems win over a stale error.
    @Test func problemsAndNotRespondingAreNeverArchived() throws {
        let s = { (o: ArchiveOverview) in RustArchiveService.status(o, settings: onSnap).state }
        let bad = ["Capture/a.ARW: Permission denied (13)", ".ds/index-snapshot.db: snapshot failed"]
        let st = s(overview7(sessions: [arcSession(pending: 1, error: "2 items not archived", problems: bad)]))
        #expect(st == .problems(count: 2))
        #expect(ArchiveDisplay.title(st) == "Archived with problems" && ArchiveDisplay.short(st) == "Problems")
        #expect(ArchiveDisplay.symbol(st) == "exclamationmark.icloud")
        #expect(ArchiveDisplay.long(st) == "2 items are not in the archive. Retrying.")
        #expect(ArchiveDisplay.long(.problems(count: 1)) == "1 item is not in the archive. Retrying.")
        #expect(ArchiveDisplay.canSyncNow(st))
        #expect(s(overview7(sessions: [arcSession(running: true, notResponding: true)]))
                == .failed("the archive volume is not responding"))
        // Offline beats a stuck run; a healthy run elsewhere shows as syncing.
        #expect(s(overview7(available: false, sessions: [arcSession(notResponding: true)])) == .offline)
        #expect(s(overview7(sessions: [arcSession(running: true, notResponding: true),
                                       arcSession("B", running: true, percent: 10)])) == .syncing(fraction: 0.1))
        // The detail keeps per-file problems and per-session Amazon status.
        let amz = AmazonStatus(folder: "/A", running: false, lastRunMs: 1_000, placed: 2, cloned: 2, alreadyMirrored: 0,
                               skippedUnsupported: 0, waiting: 0, failed: [], error: "Amazon Photos folder missing: /A",
                               folderMissing: true, session: "Trip")
        let run = ArchiveRun(session: "Trip", trigger: "on demand", outcome: "incomplete", message: nil, startedMs: 0,
                             durationMs: 1500, warnings: [], problems: bad)
        let d = try #require(RustArchiveService.status(
            overview7(sessions: [arcSession(problems: bad, amazon: amz)], runs: [run], amazon: amz), settings: onSnap).detail)
        #expect(d.sessions[0].problems == bad && d.sessions[0].amazon?.folderMissing == true)
        #expect(d.runs[0].problems == bad && d.amazon?.folderMissing == true)
    }

    @Test func popoverWordingForTheNewStates() {
        let now = Date(timeIntervalSince1970: 10_000)
        typealias S = ArchiveDetail.Session
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", lastSuccess: now.addingTimeInterval(-60),
                                             retryIn: .seconds(30), problems: ["x: y"]), now: now)
                == "1 item not archived · retry in 30 s · last full sync 1 min ago")
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", problems: ["x", "y"])) == "2 items not archived")
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", running: true, notResponding: true))
            .hasPrefix("Not responding"))
        let inc = ArchiveDetail.Run(session: "T", trigger: "on demand", outcome: "incomplete", started: now,
                                    duration: .seconds(2), problems: ["a", "b"])
        #expect(ArchiveDisplay.runLine(inc) == "T · on demand · incomplete in 2.0 s: 2 not archived")
        #expect(ArchiveDisplay.isProblem(inc))
        let stuck = ArchiveDetail.Run(session: "T", trigger: "changes", outcome: "not responding",
                                      message: "the archive target is not responding", started: now, duration: .seconds(90))
        #expect(ArchiveDisplay.runLine(stuck) == "T · changes · not responding: the archive target is not responding")
        #expect(ArchiveDisplay.isProblem(stuck))
        for fine in ["succeeded", "busy", "cancelled"] {
            #expect(!ArchiveDisplay.isProblem(ArchiveDetail.Run(session: "T", trigger: "x", outcome: fine, started: now,
                                                                 duration: .zero)))
        }
        #expect(ArchiveDisplay.amazonLine(ArchiveDetail.Amazon(folder: "/A", error: "x", folderMissing: true))
            .hasPrefix("Amazon Photos folder missing: /A"))
    }
}

@MainActor @Suite struct Arc7SettingsTests {
    /// A-M2: the chosen folder is bookmarked; a move in Finder is followed,
    /// a folder deleted (in the Trash, or gone) is reported missing, never recreated.
    @Test func amazonFolderMovesAreFollowedAndMissingFoldersReported() async throws {
        let fm = FileManager.default
        let root = fm.temporaryDirectory.appendingPathComponent("ds-arc7-amz-\(UUID())")
        defer { try? fm.removeItem(at: root) }
        let chosen = root.appendingPathComponent("Amazon Photos")
        try fm.createDirectory(at: chosen, withIntermediateDirectories: true)
        let s = makeSettings()
        s.chooseAmazonFolder(chosen)
        #expect(s.amazonPhotosFolder == chosen && s.amazonPhotosFolderBookmark != nil)
        // Persisted.
        let again = AppSettings(defaults: s.userDefaults)
        #expect(again.amazonPhotosFolderBookmark == s.amazonPhotosFolderBookmark)
        #expect(await s.followMovedAmazonFolder() == false, "not moved")
        // Renamed in Finder: followed.
        let moved = root.appendingPathComponent("Amazon Photos (moved)")
        try fm.moveItem(at: chosen, to: moved)
        #expect(await s.followMovedAmazonFolder())
        #expect(s.amazonPhotosFolder?.standardizedFileURL.path == moved.standardizedFileURL.path)
        let m = ArchiveSettingsModel(settings: s, checker: { _ in .notSet }, appInstalled: { true })
        await m.checkAmazonFolder()
        #expect(!m.amazonFolderMissing && !m.amazonText.hasPrefix("Folder missing"))
        // Deleted: missing (a folder in the Trash is never followed).
        let trash = root.appendingPathComponent(".Trash")
        try fm.createDirectory(at: trash, withIntermediateDirectories: true)
        try fm.moveItem(at: moved, to: trash.appendingPathComponent("Amazon Photos (moved)"))
        await m.checkAmazonFolder()
        #expect(m.amazonFolderMissing && m.amazonText.hasPrefix("Folder missing"))
        #expect(!fm.fileExists(atPath: moved.path), "never recreated")
        // Turned off: no bookmark, nothing missing.
        s.chooseAmazonFolder(nil)
        #expect(s.amazonPhotosFolder == nil && s.amazonPhotosFolderBookmark == nil)
        await m.checkAmazonFolder()
        #expect(!m.amazonFolderMissing)
        #expect(await s.followMovedAmazonFolder() == false)
        #expect(AppSettings.followBookmark(Data([1, 2, 3])) == nil)
    }

    /// A-H4: the target field commits on submit or when it loses focus, not
    /// per keystroke; committing the same value does nothing.
    @Test func targetCommitsOnlyWhenChanged() async {
        let s = makeSettings()
        let checked = Arc7Counter()
        let m = ArchiveSettingsModel(settings: s, checker: { _ in checked.bump(); return .ready(fsType: "apfs") },
                                     exists: { _ in false }, appInstalled: { true })
        await m.commitTarget("  /Volumes/x/Darkslide ")
        #expect(s.archiveTarget == "/Volumes/x/Darkslide" && checked.value == 1)
        await m.commitTarget("/Volumes/x/Darkslide")
        #expect(checked.value == 1, "unchanged: no new check, no settings change")
    }

    /// A-L2: each `--snapshot` run starts from empty preferences; the user's
    /// own domain is never cleared.
    @Test func snapshotDefaultsAreResetPerRun() {
        let suite = testSuiteName()
        let d = UserDefaults(suiteName: suite)!
        d.set("/Volumes/pics/Darkslide", forKey: "archiveTarget")
        d.set("/x/Amazon", forKey: "amazonPhotosFolder")
        AppModel.resetSnapshotDefaults(suite)
        let fresh = UserDefaults(suiteName: suite)!
        #expect(fresh.string(forKey: "archiveTarget") == nil && fresh.string(forKey: "amazonPhotosFolder") == nil)
        AppModel.resetSnapshotDefaults("casa.baillargeon.darkslide") // refused: no effect, no crash
    }
}

final class Arc7Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func bump() { lock.withLock { n += 1 } }
    var value: Int { lock.withLock { n } }
}

// MARK: - Live path (real FFI, real rsync)

@Suite(.serialized) struct Arc7LiveTests {
    final class Box: @unchecked Sendable {
        private let lock = NSLock()
        private var value: ArchiveSettingsSnapshot
        init(_ v: ArchiveSettingsSnapshot) { value = v }
        var snapshot: ArchiveSettingsSnapshot {
            get { lock.withLock { value } }
            set { lock.withLock { value = newValue } }
        }
    }

    func waitFor(_ seconds: Double, _ cond: () -> Bool) async -> Bool {
        let end = Date().addingTimeInterval(seconds)
        while !cond(), Date() < end { try? await Task.sleep(for: .milliseconds(50)) }
        return cond()
    }

    /// A-H1 on the live path: an original rsync can't read shows as
    /// "Archived with problems" with the file listed; A-H4: a settings change
    /// (the Amazon folder) keeps that state; A-M2: a missing Amazon folder
    /// asks the app to follow its bookmark and is never created.
    @Test func unreadableOriginalShowsProblemsAndSettingsKeepIt() async throws {
        guard !geteuidIsRoot(), let dir = try sampleFolderCopy(limit: 2) else { return }
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory.appendingPathComponent("ds-arc7-live-\(UUID())")
        let files = try fm.contentsOfDirectory(atPath: dir.path).filter { $0.hasSuffix(".ARW") }.sorted()
        let locked = dir.appendingPathComponent(files[1])
        defer {
            try? fm.setAttributes([.posixPermissions: 0o644], ofItemAtPath: locked.path)
            try? fm.removeItem(at: dir)
            try? fm.removeItem(at: tmp)
        }
        let target = tmp.appendingPathComponent("archive")
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        let store = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!),
                                     storeBase: tmp.appendingPathComponent("stores"))
        let info = try await store.add(folder: dir)
        await store.waitForHashing(info.id)
        try fm.setAttributes([.posixPermissions: 0], ofItemAtPath: locked.path)
        let missingAmazon = tmp.appendingPathComponent("Amazon Photos (gone)")
        let box = Box(ArchiveSettingsSnapshot(enabled: true, target: target.path, intervalMinutes: 60, amazonFolder: nil))
        let followed = Arc7Counter()
        let service = RustArchiveService(initial: box.snapshot, settings: { box.snapshot },
                                         handles: { await store.archiveHandle(for: $0) },
                                         manifest: tmp.appendingPathComponent("support/manifest.json"),
                                         lockDir: tmp.appendingPathComponent("locks").path, tick: .milliseconds(50),
                                         followAmazonFolder: { followed.bump() }, watchVolumes: false)
        let updates = service.statusUpdates()
        let watcher = Task { for await _ in updates {} }
        defer { watcher.cancel() }
        await service.sessionOpened(info)
        await service.afterImport(session: info, files: 2)
        #expect(await waitFor(60) { service.snapshot().state == .problems(count: 1) }, "\(service.snapshot().state)")
        let s = try #require(service.snapshot().detail?.sessions.first)
        #expect(s.problems.count == 1 && s.problems[0].hasPrefix(files[1] + ": ") && s.problems[0].contains("Permission denied"),
                "\(s.problems)")
        #expect(ArchiveDisplay.sessionLine(s).hasPrefix("1 item not archived"))
        // A settings change (an Amazon folder that doesn't exist) keeps the problem state.
        box.snapshot = ArchiveSettingsSnapshot(enabled: true, target: target.path, intervalMinutes: 60, amazonFolder: missingAmazon)
        await service.refresh()
        #expect(service.snapshot().state == .problems(count: 1))
        await service.syncNow() // the hand-off runs, finds the folder missing
        #expect(await waitFor(30) { followed.value >= 1 }, "the app is asked to follow the bookmark")
        #expect(!fm.fileExists(atPath: missingAmazon.path), "never created")
        let a = try #require(service.snapshot().detail?.sessions.first?.amazon)
        #expect(a.folderMissing && ArchiveDisplay.amazonLine(a).hasPrefix("Amazon Photos folder missing"))
        // Readable again: a clean run clears it.
        try fm.setAttributes([.posixPermissions: 0o644], ofItemAtPath: locked.path)
        await service.syncNow()
        #expect(await waitFor(60) { service.snapshot().state == .idle }, "\(service.snapshot().state)")
        await service.volumesChanged()
        await service.shutdown()
    }

    private func geteuidIsRoot() -> Bool { geteuid() == 0 }
}
