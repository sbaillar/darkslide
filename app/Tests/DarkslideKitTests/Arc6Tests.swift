import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

// MARK: - Fixtures

/// Calls that hang until released (a minute at most: a model that waits for them
/// fails slowly instead of hanging the run).
private final class HungCalls: @unchecked Sendable {
    private let cond = NSCondition()
    private var released = false
    private var left = 0
    func hang() {
        cond.withLock {
            let deadline = Date(timeIntervalSinceNow: 60)
            while !released, cond.wait(until: deadline) {}
            left += 1
        }
    }
    func release() { cond.withLock { released = true; cond.broadcast() } }
    /// Hung calls that have returned.
    var returned: Int { cond.withLock { left } }
}

private func overview(enabled: Bool = true, available: Bool = true, reason: String? = nil, timedOut: Bool = false,
                      sessions: [SessionArchiveStatus] = [], runs: [ArchiveRun] = [],
                      amazon: AmazonStatus = AmazonStatus(folder: nil, running: false, lastRunMs: nil, placed: 0, cloned: 0,
                                                          alreadyMirrored: 0, skippedUnsupported: 0, waiting: 0, failed: [],
                                                          error: nil, folderMissing: false, session: nil),
                      rsync: String? = "/opt/homebrew/bin/rsync (rsync 3.5.1)") -> ArchiveOverview {
    ArchiveOverview(enabled: enabled,
                    target: TargetCheck(path: "/Volumes/pics/Darkslide", available: available, timedOut: timedOut,
                                        reason: reason, fsType: available ? "nfs" : nil),
                    rsync: rsync, rsyncError: nil, sessions: sessions, runs: runs, amazon: amazon)
}

func arcSession(_ name: String = "Trip", running: Bool = false, percent: UInt8? = nil, pending: UInt64 = 0,
                lastSuccess: Int64? = nil, error: String? = nil, notArchivable: String? = nil,
                problems: [String] = [], notResponding: Bool = false, amazon: AmazonStatus? = nil) -> SessionArchiveStatus {
    SessionArchiveStatus(name: name, source: "/x/\(name)", notArchivable: notArchivable, archiveFolder: nil,
                         lastSuccessMs: lastSuccess, lastError: error, problems: problems, pendingChanges: pending,
                         behindSinceMs: pending > 0 ? 1000 : nil, running: running, notResponding: notResponding,
                         percent: percent, bytesPerSec: percent.map { _ in 2_000_000 }, etaSecs: nil,
                         consecutiveFailures: error == nil ? 0 : 2, retryInMs: error == nil ? nil : 60_000, amazon: amazon)
}

private func session(_ name: String = "Trip", running: Bool = false, percent: UInt8? = nil, pending: UInt64 = 0,
                     lastSuccess: Int64? = nil, error: String? = nil, notArchivable: String? = nil) -> SessionArchiveStatus {
    arcSession(name, running: running, percent: percent, pending: pending, lastSuccess: lastSuccess, error: error,
               notArchivable: notArchivable)
}

private let on = ArchiveSettingsSnapshot(enabled: true, target: "/Volumes/pics/Darkslide", intervalMinutes: 10, amazonFolder: nil)

/// Records the hooks the app calls.
final class RecordingArchive: ArchiveService, @unchecked Sendable {
    private let lock = NSLock()
    private var _calls: [String] = []
    var calls: [String] { lock.withLock { _calls } }
    private func log(_ s: String) { lock.withLock { _calls.append(s) } }
    func statusUpdates() -> AsyncStream<ArchiveStatus> { AsyncStream { $0.finish() } }
    func syncNow() async { log("sync") }
    func sessionOpened(_ session: SessionInfo) async { log("open \(session.name)") }
    func sessionClosed(_ session: SessionInfo) async { log("close \(session.name)") }
    func noteChanges(_ n: Int, in session: SessionInfo) async { log("changes \(n) \(session.name)") }
    func shutdown() async { log("shutdown") }
}

// MARK: - Mapping and wording

@Suite struct Arc6MappingTests {
    @Test func stateFollowsTheManager() {
        let s = { (o: ArchiveOverview) in RustArchiveService.status(o, settings: on).state }
        #expect(s(overview(enabled: false)) == .unavailable("Archive off"))
        #expect(RustArchiveService.status(overview(enabled: false),
                                          settings: ArchiveSettingsSnapshot(enabled: true, target: "", intervalMinutes: 10,
                                                                            amazonFolder: nil)).state
                == .unavailable("Archive off: no target set"))
        #expect(s(overview(sessions: [session(running: true, percent: 42)])) == .syncing(fraction: 0.42))
        #expect(s(overview(sessions: [session(running: true)])) == .syncing(fraction: -1), "openrsync: no progress")
        #expect(s(overview(available: false, reason: "/Volumes/pics is not mounted", sessions: [session(pending: 3)])) == .offline)
        #expect(s(overview(available: false, reason: "checking…", sessions: [session(pending: 3)]))
                == .unavailable("Checking the archive…"), "not 'offline' before the first check")
        #expect(s(overview(available: false, reason: "checking…", sessions: [session(lastSuccess: 1_000)]))
                == .unavailable("Checking the archive…"), "never 'up to date' before the first check (M6)")
        #expect(s(overview(sessions: [session(error: "rsync failed (exit 11)")])) == .failed("rsync failed (exit 11)"))
        #expect(s(overview(sessions: [session(pending: 2), session("B", pending: 5)])) == .behind(files: 7))
        #expect(s(overview(sessions: [session(pending: 4, notArchivable: "on a card")])) == .idle, "cards don't count")
        #expect(s(overview(sessions: [session(lastSuccess: 1_000)])) == .idle)
    }

    @Test func detailCarriesSessionsRunsAndAmazon() {
        let run = ArchiveRun(session: "Trip", trigger: "on demand", outcome: "succeeded", message: nil,
                             startedMs: 1_790_000_000_000, durationMs: 1234, warnings: ["vanished"], problems: [])
        let amazon = AmazonStatus(folder: "/A", running: false, lastRunMs: 1_790_000_000_000, placed: 8, cloned: 8,
                                  alreadyMirrored: 2, skippedUnsupported: 1, waiting: 3, failed: ["x: boom"], error: nil,
                                  folderMissing: false, session: "Trip")
        let st = RustArchiveService.status(overview(available: false, reason: "not mounted",
                                                    sessions: [session(lastSuccess: 1_000), session("B", lastSuccess: 5_000)],
                                                    runs: [run], amazon: amazon), settings: on)
        let d = try! #require(st.detail)
        #expect(st.lastSuccess == Date(timeIntervalSince1970: 5), "the newest success")
        #expect(d.targetReason == "not mounted" && d.rsync?.contains("rsync 3.5.1") == true)
        #expect(d.runs.first?.duration == .milliseconds(1234) && d.runs.first?.warnings == ["vanished"])
        #expect(d.amazon == ArchiveDetail.Amazon(folder: "/A", lastRun: Date(timeIntervalSince1970: 1_790_000_000), placed: 8,
                                                 alreadyMirrored: 2, waiting: 3, failed: ["x: boom"]))
        // No Amazon run yet but a folder is set: shown as pending.
        let pendingAmazon = RustArchiveService.status(overview(), settings: ArchiveSettingsSnapshot(
            enabled: true, target: "/t", intervalMinutes: 5, amazonFolder: URL(fileURLWithPath: "/B")))
        #expect(pendingAmazon.detail?.amazon?.folder == "/B" && pendingAmazon.detail?.amazon?.lastRun == nil)
        // rsync missing is shown in its place.
        var o = overview(rsync: nil)
        o.rsyncError = "no rsync found"
        #expect(RustArchiveService.status(o, settings: on).detail?.rsync == "rsync: no rsync found")
    }

    @Test func popoverLines() {
        let now = Date(timeIntervalSince1970: 10_000)
        typealias S = ArchiveDetail.Session
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", notArchivable: "on a card")) == "Not archived: on a card")
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", running: true, fraction: 0.5, bytesPerSecond: 2_000_000))
                .hasPrefix("Syncing 50 % · "))
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", running: true)) == "Syncing")
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", lastError: "boom", retryIn: .seconds(90)), now: now)
                == "Last sync failed: boom · retry in 1 min 30 s · never archived")
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", lastSuccess: now.addingTimeInterval(-120), pending: 1), now: now)
                == "1 change waiting · last sync 2 min ago")
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a", lastSuccess: now), now: now) == "Up to date · last sync just now")
        #expect(ArchiveDisplay.sessionLine(S(name: "a", source: "/a")) == "Not archived yet")
        let ok = ArchiveDetail.Run(session: "Trip", trigger: "on demand", outcome: "succeeded", started: now,
                                   duration: .milliseconds(1234), warnings: ["w"])
        #expect(ArchiveDisplay.runLine(ok) == "Trip · on demand · succeeded in 1.2 s (1 warning)")
        let slow = ArchiveDetail.Run(session: "T", trigger: "interval", outcome: "failed", message: "exit 23", started: now,
                                     duration: .seconds(42))
        #expect(ArchiveDisplay.runLine(slow) == "T · interval · failed in 42 s: exit 23")
        let off = ArchiveDetail.Run(session: "T", trigger: "changes", outcome: "cancelled", started: now, duration: .zero)
        #expect(ArchiveDisplay.runLine(off) == "T · changes · cancelled")
        typealias A = ArchiveDetail.Amazon
        #expect(ArchiveDisplay.amazonLine(A(folder: "/A", running: true)).contains("Handing"))
        #expect(ArchiveDisplay.amazonLine(A(folder: "/A", error: "disk full")) == "Amazon Photos: disk full")
        #expect(ArchiveDisplay.amazonLine(A(folder: "/A")) == "Amazon Photos: after the next import")
        #expect(ArchiveDisplay.amazonLine(A(folder: "/A", lastRun: now, placed: 8, alreadyMirrored: 1, waiting: 2, failed: ["x"]))
                == "Amazon Photos: 9 originals handed to the app (8 new), 2 waiting for verification, 1 failed")
        // Syncing without progress (openrsync).
        #expect(ArchiveDisplay.title(.syncing(fraction: -1)) == "Syncing…")
        #expect(ArchiveDisplay.short(.syncing(fraction: -1)) == "Syncing" && ArchiveDisplay.long(.syncing(fraction: -1)) == "Syncing")
        #expect(ArchiveDisplay.offText(ArchiveSettingsSnapshot(enabled: false, target: "/t", intervalMinutes: 1, amazonFolder: nil))
                == "Archive switched off")
    }

    @MainActor @Test func settingsPersistAndSnapshot() {
        let suite = testSuiteName()
        let a = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(a.archiveEnabled && a.archiveIntervalMinutes == 10 && !a.archiveSnapshot.isActive, "no target yet")
        a.archiveTarget = " /Volumes/pics/Darkslide "
        a.archiveIntervalMinutes = 30
        a.archiveEnabled = false
        let b = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(b.archiveIntervalMinutes == 30 && !b.archiveEnabled)
        #expect(b.archiveSnapshot.target == "/Volumes/pics/Darkslide" && !b.archiveSnapshot.isActive)
        b.archiveEnabled = true
        #expect(b.archiveSnapshot.isActive)
        let cfg = RustArchiveService.config(b.archiveSnapshot, lockDir: "/l", debounce: 60)
        #expect(cfg.enabled && cfg.intervalSecs == 1800 && cfg.debounceSecs == 60 && cfg.lockDir == "/l" && cfg.checkTimeoutMs == 2000)
    }
}

// MARK: - Settings model (every volume look is bounded)

@MainActor @Suite struct Arc6SettingsModelTests {
    @Test func suggestsThePicsShareOnlyWhenMountedAndUnset() async {
        let s = makeSettings()
        let m = ArchiveSettingsModel(settings: s, checker: { _ in .ready(fsType: "nfs") }, exists: { $0 == "/Volumes/NAS" },
                                     appInstalled: { true })
        await m.refresh()
        #expect(m.suggestion == AppSettings.defaultArchiveTarget && m.targetState == .notSet && m.amazonAppInstalled == true)
        #expect(m.targetText.contains("nothing is archived"))
        s.archiveEnabled = false
        await m.useSuggestion()
        #expect(s.archiveTarget == AppSettings.defaultArchiveTarget && s.archiveEnabled && m.suggestion == nil)
        // The fake says only the share exists: ready, and the folder comes with the first sync.
        #expect(m.targetState == .ready(fsType: "nfs") && m.targetText == "Ready: the folder is created on the first sync (nfs).")
        #expect(m.amazonText.contains("Choose the folder"))
        s.amazonPhotosFolder = URL(fileURLWithPath: "/A")
        #expect(m.amazonText.contains("after each import"))
        // Not mounted: no suggestion.
        let s2 = makeSettings()
        let m2 = ArchiveSettingsModel(settings: s2, checker: { _ in .notSet }, exists: { _ in false }, appInstalled: { false })
        await m2.refresh()
        #expect(m2.suggestion == nil && m2.amazonAppInstalled == false && m2.amazonText.contains("isn't installed"))
        await m2.useSuggestion()
        #expect(s2.archiveTarget.isEmpty)
    }

    /// A hung share (stat never returns) and a hung app lookup cost only the timeout:
    /// the model answers while the calls are still stuck (not a wall-clock bound, which
    /// a loaded machine overshoots; a model that waited would take the full minute).
    @Test func hungVolumeLooksAreBounded() async {
        let s = makeSettings()
        let hung = HungCalls()
        defer { hung.release() }
        let m = ArchiveSettingsModel(settings: s, checker: { _ in .notResponding },
                                     exists: { _ in hung.hang(); return true },
                                     appInstalled: { hung.hang(); return true }, timeout: 0.2)
        let clock = ContinuousClock()
        let t0 = clock.now
        await m.refresh()
        let took = clock.now - t0
        #expect(hung.returned == 0, "never waits for the hung calls")
        // A few 0.2 s timeouts: generous for a loaded machine, but a refresh that
        // ignored its timeout (and sat out the minute) still fails.
        #expect(took < .seconds(30), "\(took)")
        #expect(m.suggestion == nil && m.amazonAppInstalled == false)
        await m.setTarget("/Volumes/hung/Darkslide")
        #expect(hung.returned == 0)
        #expect(m.targetState == .notResponding && m.targetText.contains("isn't responding"))
    }

    @Test func targetStatesAndTheRealFFICheck() async throws {
        let s = makeSettings()
        let m = ArchiveSettingsModel(settings: s)
        await m.setTarget("/Volumes/ds-no-such-share/Darkslide")
        guard case .unavailable(let why) = m.targetState else { Issue.record("\(m.targetState)"); return }
        #expect(why.contains("not mounted") && m.targetText.contains("queued"))
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-arc6-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        await m.setTarget(dir.path)
        #expect(m.targetState == .ready(fsType: "apfs"))
        #expect(ArchiveSettingsModel.state(TargetCheck(path: "/x", available: false, timedOut: true, reason: nil, fsType: nil))
                == .notResponding)
        #expect(ArchiveSettingsModel.state(TargetCheck(path: "/x", available: false, timedOut: false, reason: nil, fsType: nil))
                == .unavailable("unavailable"))
        _ = ArchiveSettingsModel.amazonAppFound() // Launch Services + /Applications, whichever answers
        #expect(ArchiveSettingsModel.intervalChoices.contains(s.archiveIntervalMinutes))
    }
}

// MARK: - App hooks

@MainActor @Suite struct Arc6AppHookTests {
    @Test func openRateSwitchAndQuitReachTheArchive() async {
        let rec = RecordingArchive()
        let settings = makeSettings()
        settings.onboardingComplete = true
        settings.autoAdvance = false
        let m = AppModel(sessions: MockSessionStore(counts: [5, 3]), images: MockImageProvider(latency: .zero),
                         renderer: FakeRenderer(), importer: MockImportService(bytesPerSecond: 1e12), archive: rec,
                         ai: MockAIService(), settings: settings)
        m.watchCards = false
        await m.start()
        let first = m.currentSession!.name
        #expect(await waitUntil { rec.calls.contains("open \(first)") })
        m.setRating(3)
        #expect(await waitUntil { rec.calls.contains("changes 1 \(first)") })
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.3 }
        #expect(await waitUntil { rec.calls.filter { $0.hasPrefix("changes") }.count >= 2 })
        let other = m.sessionList.first { $0.id != m.currentSession?.id }!
        await m.open(other)
        #expect(await waitUntil { rec.calls.contains("close \(first)") && rec.calls.contains("open \(other.name)") })
        await m.open(other) // same session: no close/open churn
        let n = rec.calls.count
        await m.shutdownArchive()
        #expect(rec.calls.last == "shutdown" && rec.calls.count == n + 1)
    }

    @Test func defaultHooksAreHarmless() async {
        let mock = MockArchiveService(target: "/nonexistent", interval: .seconds(3600), step: .milliseconds(1),
                                      isReachable: { _ in true })
        let s = SessionInfo(name: "s", url: URL(fileURLWithPath: "/s"), photoCount: 0, createdAt: Date())
        await mock.sessionOpened(s)
        await mock.sessionClosed(s)
        await mock.noteChanges(3, in: s)
        await mock.afterImport(session: s, files: 3) // default: sync now
        #expect(mock.status.lastSuccess != nil)
        await mock.shutdown()
    }
}

// MARK: - Live path (real FFI, real rsync, real sessions)

@Suite(.serialized) struct Arc6LiveTests {
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

    /// Open a folder of sample copies, archive it to a temp target after an
    /// "import", and hand its originals to a temp Amazon folder.
    @Test func afterImportArchivesAndMirrorsOnTheLivePath() async throws {
        guard let dir = try sampleFolderCopy() else { return }
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory.appendingPathComponent("ds-arc6-live-\(UUID())")
        defer {
            try? fm.removeItem(at: dir)
            try? fm.removeItem(at: tmp)
        }
        let target = tmp.appendingPathComponent("archive"), amazon = tmp.appendingPathComponent("Amazon Photos")
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        try fm.createDirectory(at: amazon, withIntermediateDirectories: true) // chosen by the user: it exists
        let store = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!),
                                     storeBase: tmp.appendingPathComponent("stores"))
        let info = try await store.add(folder: dir)
        await store.waitForHashing(info.id) // Amazon gets verified originals only
        let box = Box(ArchiveSettingsSnapshot(enabled: true, target: target.path, intervalMinutes: 60, amazonFolder: amazon))
        let service = RustArchiveService(initial: box.snapshot, settings: { box.snapshot },
                                         handles: { await store.archiveHandle(for: $0) },
                                         manifest: tmp.appendingPathComponent("support/manifest.json"),
                                         lockDir: tmp.appendingPathComponent("locks").path, tick: .milliseconds(50))
        let updates = service.statusUpdates()
        var seen: [ArchiveStatus.State] = []
        let watcher = Task { for await s in updates { seen.append(s.state) } ; return seen }
        await service.sessionOpened(info)
        let clock = ContinuousClock()
        let t0 = clock.now
        await service.afterImport(session: info, files: 8)
        #expect(await waitFor(60) { service.snapshot().detail?.runs.contains { $0.outcome == "succeeded" } == true })
        // The hand-off runs in the background (serialized): wait for it too.
        #expect(await waitFor(60) { (service.snapshot().detail?.amazon?.placed ?? 0) > 0 })
        let took = clock.now - t0
        let st = service.snapshot()
        print("ARC6 live: archive + amazon of \(info.photoCount) photos: \(took); \(st.detail?.rsync ?? "?")")
        #expect(st.state == .idle && st.lastSuccess != nil, "\(st.state)")
        // The session's own folder on the target (`<name> (<id>)`, A-H3).
        let folder = try #require(st.detail?.sessions.first?.archiveFolder)
        #expect(folder.hasPrefix(dir.lastPathComponent + " ("), "\(folder)")
        let dest = target.appendingPathComponent(folder)
        for f in try fm.contentsOfDirectory(atPath: dir.path) where f.hasSuffix(".ARW") {
            #expect(fm.contentsEqual(atPath: dir.appendingPathComponent(f).path, andPath: dest.appendingPathComponent(f).path))
        }
        #expect(fm.fileExists(atPath: dest.appendingPathComponent(".ds/index-snapshot.db").path))
        let a = try #require(st.detail?.amazon)
        #expect(a.placed == info.photoCount && a.failed.isEmpty && a.error == nil, "\(a)")
        let mirrored = try fm.contentsOfDirectory(atPath: amazon.appendingPathComponent(info.name).path)
        #expect(mirrored.count == info.photoCount)
        // Settings changes are applied on the next tick: switched off → "Archive switched off".
        box.snapshot = ArchiveSettingsSnapshot(enabled: false, target: target.path, intervalMinutes: 60, amazonFolder: nil)
        #expect(await waitFor(5) { service.status.state == .unavailable("Archive switched off") })
        // Changes and close go through; quitting stops the manager promptly.
        await service.noteChanges(2, in: info)
        await service.sessionClosed(info)
        let t1 = clock.now
        #expect(await service.stop(), "the manager stopped within its own 2 s timeout")
        // Hang guard only: the timeout is the manager's; a loaded test pool adds hops.
        #expect(clock.now - t1 < .seconds(15))
        watcher.cancel()
        _ = await watcher.value
        // The originals were never touched.
        for f in try fm.contentsOfDirectory(atPath: dir.path) where f.hasSuffix(".ARW") {
            let attrs = try fm.attributesOfItem(atPath: dir.appendingPathComponent(f).path)
            #expect((attrs[.referenceCount] as? Int) == 1)
        }
    }

    /// An unmounted share: every call returns at once and the badge says offline.
    @Test func offlineTargetNeverBlocks() async throws {
        let tmp = FileManager.default.temporaryDirectory.appendingPathComponent("ds-arc6-off-\(UUID())")
        defer { try? FileManager.default.removeItem(at: tmp) }
        let snap = ArchiveSettingsSnapshot(enabled: true, target: "/Volumes/ds-no-such-share/Darkslide", intervalMinutes: 10,
                                           amazonFolder: nil)
        let service = RustArchiveService(initial: snap, settings: { snap }, handles: { _ in nil },
                                         lockDir: tmp.path, tick: .milliseconds(50))
        let s = SessionInfo(name: "s", url: tmp, photoCount: 0, createdAt: Date())
        let clock = ContinuousClock()
        let t0 = clock.now
        await service.sessionOpened(s) // no handle: ignored
        await service.afterImport(session: s, files: 1)
        await service.noteChanges(1, in: s)
        await service.sessionClosed(s)
        await service.syncNow()
        #expect(clock.now - t0 < .milliseconds(200) * TestTiming.loadFactor, "returns at once (load-scaled)")
        #expect(await waitFor(10) { service.snapshot().state == .offline })
        var st = service.snapshot()
        let statusCost = TestTiming.cpu { st = service.snapshot() }
        print("ARC6 status with an offline target: \(statusCost) CPU")
        #expect(statusCost < .milliseconds(20))
        #expect(st.detail?.targetReason?.contains("not mounted") == true)
        await service.shutdown()
        await service.refresh() // after shutdown: still answers
    }
}
