import DarkslideFFI
import Foundation
import Synchronization
import Testing
@testable import DarkslideKit

// final-journeys2 N1, N2, L4: the archive status never claims more than is true.

private let on = ArchiveSettingsSnapshot(enabled: true, target: "/Volumes/pics/Darkslide", intervalMinutes: 10, amazonFolder: nil)

private func overview(_ sessions: [SessionArchiveStatus]) -> ArchiveOverview {
    ArchiveOverview(enabled: true,
                    target: TargetCheck(path: "/Volumes/pics/Darkslide", available: true, timedOut: false, reason: nil,
                                        fsType: "nfs"),
                    rsync: nil, rsyncError: nil, sessions: sessions, runs: [],
                    amazon: AmazonStatus(folder: nil, running: false, lastRunMs: nil, placed: 0, cloned: 0, alreadyMirrored: 0,
                                         skippedUnsupported: 0, waiting: 0, failed: [], error: nil, folderMissing: false,
                                         session: nil))
}

@Suite struct ArchiveHonestyMappingTests {
    @Test func upToDateOnlyWhenEverySessionsLastRunSucceeded() {
        let s = { (x: [SessionArchiveStatus]) in RustArchiveService.status(overview(x), settings: on).state }
        #expect(s([]) == .idle, "nothing to archive")
        #expect(s([arcSession(lastSuccess: 1_000)]) == .idle)
        // A relaunch knows nothing of this session yet: not "up to date".
        #expect(s([arcSession()]) == .notArchived)
        #expect(s([arcSession(lastSuccess: 1_000), arcSession("B")]) == .notArchived, "the worst session wins")
        // The last run failed (saved across the relaunch): failed, whatever the others say.
        let full = "The archive disk is full (1.2 GB free) — 16 files not archived."
        #expect(s([arcSession(lastSuccess: 1_000), arcSession("B", error: full)]) == .failed(full))
        #expect(ArchiveDisplay.title(.notArchived) == "Not archived yet" && ArchiveDisplay.short(.notArchived) == "Not archived")
        #expect(ArchiveDisplay.symbol(.notArchived) != ArchiveDisplay.symbol(.idle))
        #expect(ArchiveDisplay.long(.notArchived).contains("hasn't been archived"))
        #expect(ArchiveDisplay.canSyncNow(.notArchived))
    }

    /// N1c: the toolbar title never carries the error; the popover says it in words,
    /// and rsync's own text is only the detail.
    @Test func failuresAreInWordsWithTheRawTextAsDetail() {
        var failed = arcSession(error: "The archive disk is full (1.2 GB free) — 16 files not archived.")
        failed.lastErrorDetail = "rsync failed (exit Some(11)): rsync: [receiver] write failed on \"x\": No space left on device (28)"
        let st = RustArchiveService.status(overview([failed]), settings: on)
        #expect(ArchiveDisplay.title(st.state) == "Last sync failed")
        #expect(ArchiveDisplay.long(st.state) == "The archive disk is full (1.2 GB free) — 16 files not archived.")
        let session = try! #require(st.detail?.sessions.first)
        #expect(session.errorDetail?.contains("No space left") == true)
        #expect(ArchiveDisplay.sessionLine(session).hasPrefix("Last sync failed: The archive disk is full"))
        for text in [ArchiveDisplay.title(st.state), ArchiveDisplay.long(st.state), ArchiveDisplay.sessionLine(session)] {
            #expect(!text.contains("rsync") && !text.contains("(28)"), "\(text)")
        }
        let run = ArchiveRun(session: "Trip", trigger: "on demand", outcome: "failed", message: "The archive copy failed.",
                             detail: "rsync failed (exit Some(23))", startedMs: 0, durationMs: 10, warnings: [], problems: [])
        var o = overview([failed])
        o.runs = [run]
        let r = try! #require(RustArchiveService.status(o, settings: on).detail?.runs.first)
        #expect(r.detail == "rsync failed (exit Some(23))" && !ArchiveDisplay.runLine(r).contains("rsync"))
    }

    /// N2: honest counts in the session line and the Amazon line.
    @Test func countsSayWhatWasLeftOut() {
        let now = Date(timeIntervalSince1970: 10_000)
        typealias S = ArchiveDetail.Session
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", lastSuccess: now, originals: 8), now: now)
                == "Up to date · 8 of 8 archived · last sync just now")
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", lastSuccess: now, originals: 8, rejectsExcluded: 3), now: now)
                == "Up to date · 5 archived, 3 rejects excluded · last sync just now")
        #expect(ArchiveDisplay.archivedCounts(S(name: "a", source: "/a", originals: 2, rejectsExcluded: 1))
                == "1 archived, 1 reject excluded")
        #expect(ArchiveDisplay.archivedCounts(S(name: "a", source: "/a")) == nil, "no run counted yet")
        var x = arcSession(lastSuccess: 1_000)
        x.originals = 8
        x.rejectsExcluded = 3
        let mapped = try! #require(RustArchiveService.status(overview([x]), settings: on).detail?.sessions.first)
        #expect(mapped.originals == 8 && mapped.rejectsExcluded == 3)
        let a = ArchiveDetail.Amazon(folder: "/A", lastRun: now, placed: 5, rejectsExcluded: 3)
        #expect(ArchiveDisplay.amazonLine(a) == "Amazon Photos: 5 originals handed to the app (5 new), 3 rejects excluded")
        let amazon = AmazonStatus(folder: "/A", running: false, lastRunMs: 1, placed: 5, cloned: 5, alreadyMirrored: 0,
                                  skippedUnsupported: 0, waiting: 0, failed: [], error: nil, folderMissing: false,
                                  session: "Trip", rejectsExcluded: 3)
        #expect(RustArchiveService.amazon(amazon)?.rejectsExcluded == 3)
    }
}

@Suite struct ArchiveHonestyFollowUpTests {
    @Test func offlineCoversADifferentDiskAtThePath() {
        #expect(ArchiveDisplay.long(.offline)
                == "Target not available (not mounted, or a different disk is at that path). Changes are queued.")
        #expect(ArchiveDisplay.accessibilityLabel(ArchiveStatus(state: .offline, target: "/t", lastSuccess: nil))
                .contains("a different disk"))
    }

    /// N2: the badge tooltip and popover title say when rejects are left out.
    @Test func excludedRejectsShowWithTheBadge() {
        let excluding = ArchiveSettingsSnapshot(enabled: true, target: "/t", intervalMinutes: 10, amazonFolder: nil,
                                                archiveRejected: false)
        let sessions = [ArchiveDetail.Session(name: "a", source: "/a", lastSuccess: Date(), originals: 8, rejectsExcluded: 3),
                        ArchiveDetail.Session(name: "b", source: "/b", lastSuccess: Date(), originals: 2, rejectsExcluded: 1)]
        let st = ArchiveStatus(state: .idle, target: "/t", lastSuccess: Date(), detail: ArchiveDetail(sessions: sessions))
        #expect(ArchiveDisplay.exclusionNote(st, settings: excluding) == "4 rejects excluded")
        #expect(ArchiveDisplay.badgeHelp(st, settings: excluding).hasSuffix("\n4 rejects excluded"))
        let before = ArchiveStatus(state: .idle, target: "/t", lastSuccess: nil)
        #expect(ArchiveDisplay.exclusionNote(before, settings: excluding) == "Rejected photos excluded", "no run counted yet")
        #expect(ArchiveDisplay.exclusionNote(st, settings: on) == nil, "rejects archived: nothing to say")
        let off = ArchiveSettingsSnapshot(enabled: false, target: "/t", intervalMinutes: 10, amazonFolder: nil, archiveRejected: false)
        #expect(ArchiveDisplay.exclusionNote(st, settings: off) == nil)
        #expect(ArchiveDisplay.badgeHelp(st, settings: on) == ArchiveDisplay.explanation(.idle))
    }

    /// N1c: no invented number; a failed session that was never archived says so.
    @Test func aFailureWithoutACountSaysSinceWhen() {
        let now = Date(timeIntervalSince1970: 10_000)
        typealias S = ArchiveDetail.Session
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", lastError: "The archive disk is full."), now: now)
                == "Last sync failed: The archive disk is full. · never archived")
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", lastSuccess: now.addingTimeInterval(-7200),
                                             lastError: "The archive disk is full."), now: now)
                == "Last sync failed: The archive disk is full. · last sync 2 h ago")
    }
}

@MainActor @Suite struct ArchiveHonestySettingsTests {
    /// N2: the archive includes rejects by default (it's the backup), Amazon doesn't;
    /// an explicit "off" from before the split is kept, and carries over to Amazon.
    @Test func rejectDefaultsAndMigration() {
        let fresh = AppSettings(defaults: UserDefaults(suiteName: testSuiteName())!)
        #expect(fresh.archiveRejected && !fresh.amazonRejected)
        #expect(fresh.archiveSnapshot.archiveRejected && !fresh.archiveSnapshot.amazonRejected)
        #expect(RustArchiveService.config(fresh.archiveSnapshot, lockDir: nil, debounce: 60).archiveRejected)

        let offSuite = testSuiteName(), offDefaults = UserDefaults(suiteName: offSuite)!
        offDefaults.set(false, forKey: "archiveRejectedPhotos")
        let off = AppSettings(defaults: offDefaults)
        #expect(!off.archiveRejected && !off.amazonRejected, "the user's explicit choice is kept")

        let onDefaults = UserDefaults(suiteName: testSuiteName())!
        onDefaults.set(true, forKey: "archiveRejectedPhotos")
        let both = AppSettings(defaults: onDefaults)
        #expect(both.archiveRejected && both.amazonRejected, "the old toggle covered Amazon too")

        fresh.amazonRejected = true
        fresh.archiveRejected = false
        let again = AppSettings(defaults: fresh.userDefaults)
        #expect(again.amazonRejected && !again.archiveRejected)
        let c = RustArchiveService.config(again.archiveSnapshot, lockDir: nil, debounce: 60)
        #expect(!c.archiveRejected && c.amazonRejected)
    }

    /// N2 / A2: Delete Rejected… says when the Trash will hold the only copy.
    @Test func deleteRejectedSaysWhenTheArchiveHasNoCopy() {
        let off = ArchiveSettingsSnapshot(enabled: false, target: "/t", intervalMinutes: 10, amazonFolder: nil)
        #expect(CullFinisher.archiveNote(settings: off, state: .idle)?.contains("only copy") == true)
        let excluded = ArchiveSettingsSnapshot(enabled: true, target: "/t", intervalMinutes: 10, amazonFolder: nil,
                                               archiveRejected: false)
        #expect(CullFinisher.archiveNote(settings: excluded, state: .idle)?.contains("aren't archived") == true)
        #expect(CullFinisher.archiveNote(settings: on, state: .failed("x"))?.contains("isn't up to date") == true)
        #expect(CullFinisher.archiveNote(settings: on, state: .idle) == nil, "archived: nothing to add")
        let msg = CullFinisher.deleteMessage(2, archiveNote: "Archiving is off: the Trash holds the only copy.")
        #expect(msg.hasPrefix("2 rejected files go") && msg.hasSuffix("\n\nArchiving is off: the Trash holds the only copy."))
        #expect(CullFinisher.deleteMessage(1) == CullFinisher.deleteMessage(1, archiveNote: nil))
    }

    /// N1b: quitting after a failed archive run asks first.
    @Test func quitAsksAfterAFailedArchiveRun() {
        #expect(AppModel.archiveQuitDecision(.failed("The archive disk is full.")) == .archiveFailed(message: "The archive disk is full."))
        #expect(AppModel.archiveQuitDecision(.problems(count: 2)) == .archiveFailed(message: "2 items are not in the archive."))
        for ok: ArchiveStatus.State in [.idle, .behind(files: 3), .offline, .syncing(fraction: 0.5), .notArchived, .unavailable("off")] {
            #expect(AppModel.archiveQuitDecision(ok) == .ok)
        }
        let alert = try! #require(TerminationFlow.alert(for: .archiveFailed(message: "The archive disk is full.")))
        #expect(alert.title == "The last archive run failed" && alert.message.hasPrefix("The archive disk is full."))
        #expect(alert.choices == [.cancel, .quitAnyway] && !alert.isCritical)
    }

    /// Before the first status arrives, the toolbar says "Checking", never "up to date".
    @Test func theModelStartsCheckingNotUpToDate() {
        #expect(makeModel().archiveStatus.state == .unavailable("Checking the archive…"))
    }

    /// N3 (app side): only an explicit confirmation records the target's volume
    /// identity; a later check never adopts whatever is mounted there now.
    @Test func confirmingTheTargetRecordsItsVolume() async {
        let s = makeSettings()
        let current = Arc7Counter()
        let m = ArchiveSettingsModel(settings: s, checker: { _ in .ready(fsType: "nfs") }, exists: { _ in true },
                                     appInstalled: { true },
                                     volumeIdentity: { "vol-\($0)-\(current.value)" }, timeout: 1)
        await m.setTarget("/Volumes/pics/Darkslide")
        #expect(s.archiveTargetVolume == "vol-/Volumes/pics/Darkslide-0")
        #expect(s.archiveSnapshot.targetVolume == "vol-/Volumes/pics/Darkslide-0")
        current.bump() // another volume mounted at the same path
        await m.refresh()
        #expect(s.archiveTargetVolume == "vol-/Volumes/pics/Darkslide-0", "a refresh never re-records")
        #expect(AppSettings(defaults: s.userDefaults).archiveTargetVolume == "vol-/Volumes/pics/Darkslide-0", "persisted")
        await m.setTarget("")
        #expect(s.archiveTargetVolume == nil)
        // A hung lookup records nothing, within the timeout.
        let hung = ArchiveSettingsModel(settings: s, checker: { _ in .notSet }, exists: { _ in true }, appInstalled: { true },
                                        volumeIdentity: { _ in Thread.sleep(forTimeInterval: 5); return "late" }, timeout: 0.2)
        await hung.setTarget("/Volumes/other/Darkslide")
        #expect(s.archiveTargetVolume == nil)
        // Onboarding Finish records it too.
        let o = OnboardingModel(settings: s, fileExists: { _ in true }, createDirectory: { _ in },
                                volumeIdentity: { _ in "onboarded" })
        o.archiveTarget = "/Volumes/pics/Darkslide"
        try? o.finish()
        await o.recordTargetVolume()
        #expect(s.archiveTargetVolume == "onboarded")
        #expect(ArchiveVolume.identity("/") != nil, "the boot volume has an identity (ds-ffi archive_volume_identity)")
    }

    /// L4: the Amazon line never waits on the target check, and says nothing
    /// misleading before the lookup answers.
    @Test func amazonAppLookupDoesNotWaitForTheTargetCheck() async {
        let s = AppSettings(defaults: UserDefaults(suiteName: testSuiteName())!)
        s.archiveTarget = "/Volumes/pics/Darkslide"
        // The target check hangs until released (deterministic: no wall-clock race).
        let released = Mutex(false)
        let m = ArchiveSettingsModel(settings: s, checker: { _ in
            for _ in 0..<6000 where !released.withLock({ $0 }) { try? await Task.sleep(for: .milliseconds(5)) }
            return .ready(fsType: "nfs")
        }, exists: { _ in false }, appInstalled: { true })
        #expect(!m.amazonText.contains("Checking"), "a neutral line until the lookup answers")
        let refresh = Task { await m.refresh() }
        #expect(await waitUntil(timeout: .seconds(30)) { m.amazonAppInstalled == true }, "shown while the target check hangs")
        #expect(m.targetState != .ready(fsType: "nfs"), "the target check is still running")
        released.withLock { $0 = true }
        await refresh.value
        #expect(ArchiveSettingsModel.lastAppLookup.value != nil, "later windows start from the last answer")
        #expect(ArchiveSettingsModel(settings: s).amazonAppInstalled != nil)
    }
}

// MARK: - Live: a failed run, then a relaunch (real FFI, a fake rsync that hits ENOSPC)

@Suite(.serialized) struct ArchiveHonestyLiveTests {
    func waitFor(_ seconds: Double, _ cond: () -> Bool) async -> Bool {
        let end = Date().addingTimeInterval(seconds)
        while !cond(), Date() < end { try? await Task.sleep(for: .milliseconds(50)) }
        return cond()
    }

    @Test func aFailedRunIsStillFailedAfterARelaunch() async throws {
        guard let dir = try sampleFolderCopy(limit: 2) else { return }
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory.appendingPathComponent("ds-honesty-\(UUID())")
        defer {
            try? fm.removeItem(at: dir)
            try? fm.removeItem(at: tmp)
        }
        let target = tmp.appendingPathComponent("archive")
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        let rsync = tmp.appendingPathComponent("fake-rsync")
        try """
        #!/bin/sh
        if [ "$1" = --version ]; then echo 'rsync  version 3.4.1  protocol version 32'; exit 0; fi
        echo 'rsync: [receiver] write failed on "/x/DSC0001.ARW": No space left on device (28)' >&2
        exit 11
        """.write(to: rsync, atomically: true, encoding: .utf8)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: rsync.path)
        let store = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!),
                                     storeBase: tmp.appendingPathComponent("stores"))
        let info = try await store.add(folder: dir)
        let settings = ArchiveSettingsSnapshot(enabled: true, target: target.path, intervalMinutes: 60, amazonFolder: nil)
        func launch() -> RustArchiveService {
            RustArchiveService(initial: settings, settings: { settings }, handles: { await store.archiveHandle(for: $0) },
                               manifest: tmp.appendingPathComponent("support/manifest.json"),
                               lockDir: tmp.appendingPathComponent("locks").path, tick: .milliseconds(50),
                               rsyncPath: rsync.path, watchVolumes: false)
        }
        let first = launch()
        await first.sessionOpened(info)
        await first.afterImport(session: info, files: 2)
        #expect(await waitFor(60) { if case .failed = first.snapshot().state { true } else { false } }, "\(first.snapshot().state)")
        let failed = first.snapshot()
        #expect(ArchiveDisplay.title(failed.state) == "Last sync failed")
        #expect(ArchiveDisplay.long(failed.state).hasPrefix("The archive disk is full ("), "\(ArchiveDisplay.long(failed.state))")
        #expect(ArchiveDisplay.long(failed.state).hasSuffix(" free) — 2 files not archived."))
        #expect(failed.detail?.sessions.first?.errorDetail?.contains("No space left on device") == true)
        #expect(AppModel.archiveQuitDecision(failed.state) != .ok, "N1b")
        await first.shutdown()

        // Relaunch: from the first status on, never "up to date".
        let second = launch()
        await second.sessionOpened(info)
        var seen: [ArchiveStatus.State] = []
        let end = Date().addingTimeInterval(3)
        while Date() < end {
            seen.append(second.snapshot().state)
            try? await Task.sleep(for: .milliseconds(50))
        }
        #expect(!seen.contains(.idle), "\(Set(seen.map { "\($0)" }))")
        let st = second.snapshot()
        guard case .failed(let why) = st.state else { Issue.record("\(st.state)"); return }
        #expect(why.hasPrefix("The archive disk is full"))
        let line = ArchiveDisplay.sessionLine(try #require(st.detail?.sessions.first))
        #expect(line.hasPrefix("Last sync failed: The archive disk is full"), "\(line)")
        await second.shutdown()
    }
}
