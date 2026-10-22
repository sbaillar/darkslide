import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// A NAS or card that only dropped off must never cost ratings and edits: Remove is
/// offered only for a photo the store verified missing on a connected drive, and asks
/// first; a gone drive is said ("Drive not connected"), escalates to the session banner,
/// and clears when it's back. Probes of hung volumes never pile up threads.
@MainActor @Suite(.serialized) struct VolumeSafetyTests {
    /// A store over photos at given paths; `back` photos are relinked by a re-open.
    actor Store: FolderSessionOpening {
        var photosByID: [PhotoID: Photo] = [:]
        let session: SessionInfo
        var removed: [PhotoID] = []
        var adds = 0
        var back: Set<PhotoID> = []

        init(folder: URL, files: [String], missing: Set<Int64> = []) {
            session = SessionInfo(name: folder.lastPathComponent, url: folder, photoCount: files.count, createdAt: Date())
            for (i, f) in files.enumerated() {
                var p = samplePhoto(Int64(i + 1))
                p.url = URL(fileURLWithPath: f)
                p.filename = p.url!.lastPathComponent
                p.isMissing = missing.contains(Int64(i + 1))
                photosByID[p.id] = p
            }
        }

        func setBack(_ ids: Set<PhotoID>) { back = ids }
        func listSessions() async throws -> [SessionInfo] { [session] }
        func photos(in session: SessionInfo) async throws -> [Photo] { photosByID.values.sorted { $0.id < $1.id } }
        func albums(in session: SessionInfo) async throws -> [Album] { [] }
        func createSession(named name: String) async throws -> SessionInfo { session }
        func save(photos: [Photo]) async throws {}
        func edit(for id: PhotoID) async throws -> EditState { EditState() }
        func save(edits: [PhotoID: EditState]) async throws {}
        func add(folder: URL, progress: @escaping @Sendable (OpenProgress) -> Void) async throws -> SessionInfo {
            adds += 1
            for id in back { photosByID[id]?.isMissing = false }
            return session
        }
        var lastFailures: [URL] { [] }
        func removeMissing(_ ids: [PhotoID]) async throws {
            removed += ids
            for id in ids { photosByID[id] = nil }
        }
        func locate(_ id: PhotoID, at url: URL) async throws {}
        func restorePrevious(_ id: PhotoID) async throws {}
        func dismissFileChanged(_ id: PhotoID) async throws {}
        func restoreToFolder(_ id: PhotoID) async throws {}
        func relocate(_ session: SessionInfo, to folder: URL) async throws -> SessionInfo { session }
    }

    /// What the probes answer (changed by the test as drives come and go).
    final class Answers: @unchecked Sendable {
        private let lock = NSLock()
        private var _mounted: [String: Bool] = [:]
        private var _exists: [String: Bool] = [:]
        private var _hung = false
        private var _identity: [String: String] = [:]
        /// The identity of the volume mounted at each root now (absent: not compared).
        var identity: [String: String] {
            get { lock.withLock { _identity } }
            set { lock.withLock { _identity = newValue } }
        }
        var mounted: [String: Bool] {
            get { lock.withLock { _mounted } }
            set { lock.withLock { _mounted = newValue } }
        }
        var exists: [String: Bool] {
            get { lock.withLock { _exists } }
            set { lock.withLock { _exists = newValue } }
        }
        var hung: Bool {
            get { lock.withLock { _hung } }
            set { lock.withLock { _hung = newValue } }
        }
        func probe(_ root: URL, _ id: String?) -> VolumeCheck? {
            guard !hung, let up = mounted[root.path] else { return nil }
            guard up, let id, let now = identity[root.path] else { return VolumeCheck(mounted: up, sameVolume: up) }
            return VolumeCheck(mounted: true, sameVolume: id == now)
        }
        func original(_ url: URL) -> OriginalProbe? {
            guard !hung else { return nil }
            let root = VolumeMount.root(of: url)
            let up = root.map { mounted[$0.path] ?? false }
            return OriginalProbe(exists: (up ?? true) && exists[url.path] == true, volumeMounted: up)
        }
    }

    static let vol = "/Volumes/ds-test-pics"

    static func app(_ store: Store, _ answers: Answers, renderer: FakeRenderer = FakeRenderer(),
                    record: VolumeRecord? = nil, located: VolumeRecord? = nil) async -> AppModel {
        let settings = makeSettings()
        if let record { settings.setVolumeRecord(record, for: store.session.url) }
        settings.onboardingComplete = true
        settings.autoAdvance = false
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: renderer,
                         importer: MockImportService(bytesPerSecond: 1e12),
                         archive: MockArchiveService(target: "/nonexistent", interval: .seconds(3600), step: .milliseconds(1),
                                                     isReachable: { _ in true }),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        m.toastDuration = .seconds(3600)
        m.volumeProbe = { answers.probe($0, $1) }
        m.volumeLocate = { _ in located }
        m.viewer.originalProbe = { answers.original($0) }
        await m.start()
        return m
    }

    static func files(_ dir: String, _ n: Int) -> [String] { (1...n).map { "\(dir)/IMG_\($0).ARW" } }

    /// The NAS dropped off before the session opened: every photo reads as missing, but
    /// the drive is what's gone. Session banner naming the drive; nothing removable.
    @Test func aDroppedDriveIsSaidAndNeverOffersRemove() async throws {
        let store = Store(folder: URL(fileURLWithPath: "\(Self.vol)/Shoot"), files: Self.files("\(Self.vol)/Shoot", 3),
                          missing: [1, 2, 3])
        let answers = Answers()
        answers.mounted = [Self.vol: false]
        let m = await Self.app(store, answers)
        defer { m.stop() }
        #expect(await waitUntil { m.volumeMounted[Self.vol] == false })
        #expect(m.sessionOriginalsMissing && m.sessionVolumeGone)
        let problem = try #require(m.viewer.problem)
        #expect(problem == .sessionMissing(folder: "Shoot", volume: "ds-test-pics"))
        #expect(problem.message.hasPrefix("Drive not connected — reconnect “ds-test-pics”"), "\(problem.message)")
        #expect(problem.actions.map(\.action) == [.locateFolder, .removeFromRecents])
        let ids = m.photos.map(\.id)
        #expect(!ids.contains(where: m.canRemoveMissing))
        m.filter.missingOnly = true
        m.selectAll()
        #expect(MissingActions.removable(m).isEmpty, "the Missing bar's Remove is disabled too")
        await m.removeMissing(ids)
        #expect(await store.removed.isEmpty, "nothing forgotten")
        #expect(m.photoCount == 3)
    }

    /// Remove is offered (and works) only for a store-verified missing photo on a mounted
    /// drive; one found missing only in this session (not verified) gets Locate only.
    @Test func removeNeedsAStoreVerifiedPhotoOnAMountedDrive() async throws {
        let dir = "\(Self.vol)/Shoot"
        let store = Store(folder: URL(fileURLWithPath: dir), files: Self.files(dir, 3), missing: [2])
        let answers = Answers()
        answers.mounted = [Self.vol: true]
        answers.exists = ["\(dir)/IMG_1.ARW": true, "\(dir)/IMG_3.ARW": true]
        let m = await Self.app(store, answers)
        defer { m.stop() }
        #expect(await waitUntil { m.volumeMounted[Self.vol] == true })
        #expect(!m.sessionOriginalsMissing)
        m.filter.missingOnly = true
        m.focus(PhotoID(2), extend: false)
        #expect(m.canRemoveMissing(PhotoID(2)))
        #expect(m.viewer.problem == .missingOriginal(name: "IMG_2.ARW", canRemove: true))
        #expect(m.viewer.problem?.actions.map(\.title) == ["Locate…", "Remove…"])
        m.selectAll()
        #expect(MissingActions.removable(m) == [PhotoID(2)])

        // Photo 3 is deleted mid-session: missing, but not verified by the store.
        m.filter.missingOnly = false
        answers.exists["\(dir)/IMG_3.ARW"] = false
        m.focus(PhotoID(3), extend: false)
        #expect(await waitUntil { m.viewer.problem == .missingOriginal(name: "IMG_3.ARW", canRemove: false) })
        #expect(m.viewer.problem?.actions.map(\.title) == ["Locate…"])

        // The drive dropped off with no unmount notification (a hung share): Remove
        // probes again first and forgets nothing.
        answers.mounted[Self.vol] = false
        await m.removeMissing([PhotoID(2)])
        #expect(await store.removed.isEmpty && m.volumeMounted[Self.vol] == false)
        answers.mounted[Self.vol] = true
        await m.refreshVolumes()
        #expect(await waitUntil { m.volumeMounted[Self.vol] == true && !m.recheckingOriginals })
        await m.removeMissing([PhotoID(2), PhotoID(3)])
        #expect(await store.removed == [PhotoID(2)], "only the verified one")

        let c = MissingActions.confirmRemove(count: 2)
        #expect(c.message.hasPrefix("This deletes ratings and edits for 2 photos"))
        #expect(MissingActions.confirmRemove(count: 1).title == "Remove 1 photo from the session?")
    }

    /// A photo on another drive that isn't connected: "Drive not connected", no buttons
    /// to remove it; the session itself (on the startup disk) isn't escalated.
    @Test func aPhotoOnAnUnpluggedDriveSaysReconnect() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("vs-\(UUID())").path
        let store = Store(folder: URL(fileURLWithPath: dir), files: ["\(dir)/IMG_1.ARW", "/Volumes/ds-test-card/IMG_2.ARW"])
        let answers = Answers()
        answers.mounted = ["/Volumes/ds-test-card": false]
        answers.exists = ["\(dir)/IMG_1.ARW": true]
        let m = await Self.app(store, answers)
        defer { m.stop() }
        m.focus(PhotoID(2), extend: false)
        #expect(await waitUntil { m.viewer.problem == .driveDisconnected(volume: "ds-test-card") })
        let p = try #require(m.viewer.problem)
        #expect(p.actions.isEmpty && !p.isSessionWide && p.symbol == "externaldrive.badge.xmark")
        #expect(p.message.hasPrefix("Drive not connected — reconnect “ds-test-card”"))
        #expect(m.isOriginalMissing(PhotoID(2)) && !m.sessionOriginalsMissing)
        #expect(!m.canRemoveMissing(PhotoID(2)))
    }

    /// Unplugged mid-session: escalates to the session banner; plugged back in: the store
    /// re-checks its missing photos and the banner clears.
    @Test func midSessionUnplugEscalatesAndClearsWhenBack() async throws {
        let dir = "\(Self.vol)/Shoot"
        let store = Store(folder: URL(fileURLWithPath: dir), files: Self.files(dir, 3))
        let answers = Answers()
        answers.mounted = [Self.vol: true]
        answers.exists = Dictionary(uniqueKeysWithValues: Self.files(dir, 3).map { ($0, true) })
        let r = FakeRenderer()
        let m = await Self.app(store, answers, renderer: r)
        defer { m.stop() }
        #expect(await waitUntil { m.volumeMounted[Self.vol] == true && m.viewer.frame != nil })
        #expect(m.viewer.problem == nil)

        answers.mounted[Self.vol] = false
        m.volumesChanged() // NSWorkspace.didUnmountNotification
        #expect(await waitUntil { m.sessionOriginalsMissing })
        #expect(m.viewer.problem == .sessionMissing(folder: "Shoot", volume: "ds-test-pics"))
        #expect(m.viewer.frame == nil, "no develop under the banner")
        #expect(m.isOriginalMissing(PhotoID(1)))

        answers.mounted[Self.vol] = true
        m.volumesChanged() // didMount
        #expect(await waitUntil { !m.sessionOriginalsMissing && m.viewer.problem == nil })
        #expect(await waitUntil { m.viewer.frame != nil }, "it develops again")
    }

    /// The session opened while its drive was away (the store marked everything missing):
    /// the drive mounts, the store re-checks (re-opens the folder), the banner clears.
    @Test func storeMissingPhotosAreRecheckedWhenTheDriveMounts() async throws {
        let dir = "\(Self.vol)/Shoot"
        let store = Store(folder: URL(fileURLWithPath: dir), files: Self.files(dir, 2), missing: [1, 2])
        let answers = Answers()
        answers.mounted = [Self.vol: false]
        let m = await Self.app(store, answers)
        defer { m.stop() }
        #expect(await waitUntil { m.sessionOriginalsMissing && m.volumeMounted[Self.vol] == false })
        await store.setBack([PhotoID(1), PhotoID(2)])
        answers.mounted[Self.vol] = true
        answers.exists = Dictionary(uniqueKeysWithValues: Self.files(dir, 2).map { ($0, true) })
        m.volumesChanged()
        #expect(await waitUntil { !m.sessionOriginalsMissing && m.missingCount == 0 })
        #expect(await store.adds == 1)
        #expect(m.viewer.problem == nil)
    }

    /// Focusing a store-missing photo whose file is there re-checks the store, once (a file
    /// that can't be relinked doesn't loop).
    @Test func focusRechecksAStoreMissingPhotoThatIsBack() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("vs-\(UUID())").path
        let store = Store(folder: URL(fileURLWithPath: dir), files: Self.files(dir, 2), missing: [2])
        let answers = Answers()
        answers.exists = Dictionary(uniqueKeysWithValues: Self.files(dir, 2).map { ($0, true) })
        let m = await Self.app(store, answers)
        defer { m.stop() }
        m.filter.missingOnly = true
        m.focus(PhotoID(2), extend: false)
        for _ in 0..<1500 where await store.adds == 0 { try await Task.sleep(for: .milliseconds(10)) }
        #expect(await store.adds == 1)
        #expect(await waitUntil { !m.recheckingOriginals })
        #expect(m.photo(PhotoID(2))?.isMissing == true, "the store couldn't relink it")
        m.focus(PhotoID(2), extend: false)
        m.viewer.verifyFocusedOriginal()
        try await Task.sleep(for: .milliseconds(100))
        #expect(await store.adds == 1, "re-checked once")

        await store.setBack([PhotoID(2)])
        m.recheckedOriginals.removeAll() // as when a drive comes back
        m.viewer.verifyFocusedOriginal() // (it's still focused: as on the next focus)
        #expect(await waitUntil { m.photo(PhotoID(2))?.isMissing == false })
    }

    /// "No such file" about something other than the original is a render failure (with
    /// Retry), not a missing-original banner; a hung check still says something.
    @Test func unrelatedNoSuchFileIsARenderFailure() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("vs-\(UUID())").path
        let store = Store(folder: URL(fileURLWithPath: dir), files: Self.files(dir, 2))
        let answers = Answers()
        answers.exists = Dictionary(uniqueKeysWithValues: Self.files(dir, 2).map { ($0, true) })
        let r = FakeRenderer()
        let m = await Self.app(store, answers, renderer: r)
        defer { m.stop() }
        #expect(await waitUntil { m.viewer.frame != nil })
        r.failWith = CocoaError(.fileNoSuchFile)
        m.viewer.retryRender()
        #expect(await waitUntil { if case .renderFailed? = m.viewer.problem { true } else { false } },
                "\(String(describing: m.viewer.problem))")
        #expect(!m.isOriginalMissing(PhotoID(1)))

        answers.hung = true
        m.viewer.retryRender()
        #expect(await waitUntil { if case .renderFailed? = m.viewer.problem { true } else { false } }, "said, never silent")

        answers.hung = false
        answers.exists["\(dir)/IMG_1.ARW"] = false
        m.viewer.retryRender()
        #expect(await waitUntil { m.viewer.problem == .missingOriginal(name: "IMG_1.ARW") })
    }

    @Test func bannerWording() {
        let s = ViewerProblem.sessionMissing(folder: "Iceland", volume: "pics")
        #expect(s.message.contains("reconnect “pics”") && s.message.contains("“Iceland”") && s.isSessionWide)
        #expect(ViewerProblem.sessionMissing(folder: "Iceland").message.hasPrefix("The originals of “Iceland”"))
        let d = ViewerProblem.driveDisconnected(volume: "pics")
        #expect(d.actions.isEmpty && !d.isSessionWide)
    }

    /// Remove fails closed: a share that hangs (no answer) keeps its photos, although
    /// the last answer said "mounted".
    @Test func removeFailsClosedWhileTheShareHangs() async throws {
        let dir = "\(Self.vol)/Shoot"
        let store = Store(folder: URL(fileURLWithPath: dir), files: Self.files(dir, 2), missing: [2])
        let answers = Answers()
        answers.mounted = [Self.vol: true]
        answers.exists = ["\(dir)/IMG_1.ARW": true]
        let m = await Self.app(store, answers)
        defer { m.stop() }
        #expect(await waitUntil { m.volumeMounted[Self.vol] == true })
        #expect(m.canRemoveMissing(PhotoID(2)))
        answers.hung = true
        await m.removeMissing([PhotoID(2)])
        #expect(await store.removed.isEmpty, "no fresh answer: nothing forgotten")
        #expect(m.volumeMounted[Self.vol] == true, "a hang isn't a change")
        answers.hung = false
        await m.removeMissing([PhotoID(2)])
        #expect(await store.removed == [PhotoID(2)])
    }

    /// Another disk mounted under the session's volume name (a clone, a restore): the
    /// session views and renders as usual; only Remove is held back, and the banner of a
    /// missing photo says why. Locate Folder (to the same path) records the disk again.
    @Test func aDifferentVolumeUnderTheSameNameOnlyHoldsBackRemove() async throws {
        let dir = "\(Self.vol)/Shoot"
        let store = Store(folder: URL(fileURLWithPath: dir), files: Self.files(dir, 2), missing: [2])
        let answers = Answers()
        answers.mounted = [Self.vol: true]
        answers.identity = [Self.vol: "uuid:CLONE"]
        answers.exists = ["\(dir)/IMG_1.ARW": true]
        let clone = VolumeRecord(mount: Self.vol, identity: "uuid:CLONE")
        let m = await Self.app(store, answers, record: VolumeRecord(mount: Self.vol, identity: "uuid:PICS"), located: clone)
        defer { m.stop() }
        #expect(await waitUntil { m.volumeMismatch.contains(Self.vol) })
        #expect(m.volumeMounted[Self.vol] == true && !m.sessionVolumeGone && !m.sessionOriginalsMissing)
        m.focus(PhotoID(1), extend: false)
        #expect(await waitUntil { m.viewer.frame != nil && m.viewer.problem == nil }, "views and renders")
        #expect(m.emptyState != .originalsMissing)

        #expect(!m.canRemoveMissing(PhotoID(2)))
        m.focus(PhotoID(2), extend: false)
        let p = try #require(m.viewer.problem)
        #expect(p == .missingOnOtherVolume(name: "IMG_2.ARW", volume: "ds-test-pics"))
        #expect(p.message.contains("Remove isn’t offered") && p.symbol == "questionmark.folder" && !p.isSessionWide)
        #expect(p.actions.map(\.action) == [.locateFile, .locateFolder])
        await m.removeMissing([PhotoID(2)])
        #expect(await store.removed.isEmpty)

        // Locate Folder… to the same path: the disk is recorded again, Remove comes back.
        try await m.locateSession(#require(m.currentSession), at: URL(fileURLWithPath: dir))
        #expect(await waitUntil { m.settings.volumeRecord(for: URL(fileURLWithPath: dir)) == clone })
        #expect(await waitUntil { !m.volumeMismatch.contains(Self.vol) && m.canRemoveMissing(PhotoID(2)) })
        await m.removeMissing([PhotoID(2)])
        #expect(await store.removed == [PhotoID(2)])
    }

    /// A session folder outside `/Volumes` (a `/net` automount, a symlink to a share) gets
    /// a real mount check from its recorded mount point, located once while it was there.
    @Test func aFolderOutsideVolumesIsCheckedAtItsRecordedMount() async throws {
        let mount = "/net/ds-test-\(UUID())"
        let dir = "\(mount)/Shoot"
        let store = Store(folder: URL(fileURLWithPath: dir), files: Self.files(dir, 2), missing: [1, 2])
        let answers = Answers()
        answers.mounted = [mount: false]
        let record = VolumeRecord(mount: mount, identity: "src:nfs:nas:/export")
        let m = await Self.app(store, answers, located: record)
        defer { m.stop() }
        #expect(await waitUntil { m.volumeMounted[mount] == false })
        #expect(m.settings.volumeRecord(for: URL(fileURLWithPath: dir)) == record, "recorded")
        #expect(m.volumeRoot(of: PhotoID(1))?.path == mount)
        #expect(VolumeMount.group(of: URL(fileURLWithPath: "\(dir)/IMG_1.ARW")) == mount, "its own probe group")
        #expect(m.viewer.problem == .sessionMissing(folder: "Shoot", volume: URL(fileURLWithPath: mount).lastPathComponent))
        #expect(!m.canRemoveMissing(PhotoID(1)))
    }

    /// The first probe hung (no answer): when the drive answers later, the store's missing
    /// photos are still re-checked and the banner clears.
    @Test func aHungFirstProbeStillRechecksWhenTheDriveAnswers() async throws {
        let dir = "\(Self.vol)/Shoot"
        let store = Store(folder: URL(fileURLWithPath: dir), files: Self.files(dir, 2), missing: [1, 2])
        let answers = Answers()
        answers.hung = true
        let m = await Self.app(store, answers)
        defer { m.stop() }
        #expect(await waitUntil { m.volumeUnanswered.contains(Self.vol) })
        #expect(m.volumeMounted[Self.vol] == nil && m.sessionOriginalsMissing)
        await store.setBack([PhotoID(1), PhotoID(2)])
        answers.hung = false
        answers.mounted = [Self.vol: true]
        answers.exists = Dictionary(uniqueKeysWithValues: Self.files(dir, 2).map { ($0, true) })
        m.volumesChanged()
        #expect(await waitUntil { !m.sessionOriginalsMissing && m.missingCount == 0 })
        #expect(await store.adds == 1)
    }

    // MARK: - Probes

    @Test func volumeRoots() {
        #expect(VolumeMount.root(of: URL(fileURLWithPath: "/Volumes/pics/2026/a.ARW"))?.path == "/Volumes/pics")
        #expect(VolumeMount.root(of: URL(fileURLWithPath: "/Volumes/pics"))?.path == "/Volumes/pics")
        #expect(VolumeMount.root(of: URL(fileURLWithPath: "/Volumes")) == nil)
        #expect(VolumeMount.root(of: URL(fileURLWithPath: "/Users/me/Pictures/a.ARW")) == nil)
        #expect(VolumeMount.group(of: URL(fileURLWithPath: "/Users/me")) == "/")
        #expect(VolumeMount.isMounted(URL(fileURLWithPath: "/")))
        let nowhere = URL(fileURLWithPath: "/Volumes/ds-test-nowhere-\(UUID())")
        #expect(!VolumeMount.isMounted(nowhere))
        #expect(OriginalProbe.check(nowhere.appendingPathComponent("x.ARW")) == OriginalProbe(exists: false, volumeMounted: false))
        let here = URL(fileURLWithPath: #filePath)
        #expect(OriginalProbe.check(here) == OriginalProbe(exists: true, volumeMounted: nil))
    }

    @Test func liveProbesAnswer() async {
        let nowhere = URL(fileURLWithPath: "/Volumes/ds-test-nowhere-\(UUID())")
        #expect(await VolumeMount.probe(nowhere)?.mounted == false)
        #expect(await OriginalProbe.live(URL(fileURLWithPath: #filePath))?.exists == true)
    }

    final class Counter: @unchecked Sendable {
        private let lock = NSLock()
        private var n = 0
        func bump() { lock.withLock { n += 1 } }
        var value: Int { lock.withLock { n } }
    }

    final class Clock: @unchecked Sendable {
        private let lock = NSLock()
        private var t = Date(timeIntervalSince1970: 0)
        func advance(_ s: Double) { lock.withLock { t = t.addingTimeInterval(s) } }
        var now: Date { lock.withLock { t } }
    }

    /// Concurrent callers of one key share one probe; results are cached for the TTL.
    @Test func probeGateRunsOnePerKeyAndCaches() async {
        let clock = Clock()
        let gate = ProbeGate(now: { clock.now })
        let runs = Counter()
        async let a = gate.run(key: "k", group: "g", timeout: 2, ttl: 5) { () -> Int in
            runs.bump()
            Thread.sleep(forTimeInterval: 0.1)
            return 7
        }
        async let b = gate.run(key: "k", group: "g", timeout: 2, ttl: 5) { () -> Int in
            runs.bump()
            Thread.sleep(forTimeInterval: 0.1)
            return 7
        }
        let (x, y) = await (a, b)
        #expect(x == 7 && y == 7)
        #expect(runs.value == 1)
        #expect(await gate.run(key: "k", group: "g", timeout: 2, ttl: 5) { () -> Int in runs.bump(); return 9 } == 7, "cached")
        clock.advance(6)
        #expect(await gate.run(key: "k", group: "g", timeout: 2, ttl: 5) { () -> Int in runs.bump(); return 9 } == 9, "expired")
        gate.invalidate()
        #expect(await gate.run(key: "k", group: "g", timeout: 2) { () -> Int in 10 } == 10)
        #expect(gate.inFlight == 0)
    }

    /// A probe stuck past its timeout: callers get nil, and no new probe (thread) starts on
    /// that volume until it finishes; other volumes are unaffected. Its late answer is kept.
    @Test func aStuckVolumeParksOneProbeNotOnePerCall() async throws {
        let clock = Clock()
        let gate = ProbeGate(now: { clock.now })
        let release = DispatchSemaphore(value: 0)
        let runs = Counter()
        let first = await gate.run(key: "a", group: "/Volumes/nas", timeout: 0.05, ttl: 60) { () -> Bool in
            runs.bump()
            release.wait()
            return true
        }
        #expect(first == nil, "timed out")
        clock.advance(1)
        for i in 0..<20 {
            let r = await gate.run(key: "b\(i)", group: "/Volumes/nas", timeout: 0.05) { () -> Bool in
                runs.bump()
                return true
            }
            #expect(r == nil)
        }
        #expect(runs.value == 1 && gate.inFlight == 1, "no thread per call")
        #expect(await gate.run(key: "c", group: "/", timeout: 1) { true } == true, "another volume still answers")
        release.signal()
        #expect(await waitUntil { gate.inFlight == 0 })
        #expect(await gate.run(key: "a", group: "/Volumes/nas", timeout: 0.05, ttl: 60) { false } == true, "the late answer")
    }

    /// `invalidate()` makes a new generation: a probe already in flight answers its own
    /// callers but never a later one, and doesn't fill the cache.
    @Test func aProbeStartedBeforeInvalidateNeverAnswersAfterIt() async throws {
        let gate = ProbeGate()
        let release = DispatchSemaphore(value: 0)
        async let stale = gate.run(key: "k", group: "g", timeout: 5, ttl: 60) { () -> Bool in
            release.wait()
            return true
        }
        #expect(await waitUntil { gate.inFlight == 1 })
        gate.invalidate()
        #expect(await gate.run(key: "k", group: "g", timeout: 5, ttl: 60) { false } == false, "a fresh probe ran")
        release.signal()
        #expect(await stale == true, "its own caller still gets it")
        #expect(await waitUntil { gate.inFlight == 0 })
        #expect(await gate.run(key: "k", group: "g", timeout: 5, ttl: 60) { true } == false, "the fresh answer is cached, not the stale one")
    }

    @Test func locateAndRegister() throws {
        let tmp = FileManager.default.temporaryDirectory
        #expect(VolumeMount.locate(tmp) == VolumeRecord(mount: nil, identity: nil), "the startup disk")
        #expect(VolumeMount.locate(URL(fileURLWithPath: "/Volumes/ds-test-nowhere-\(UUID())")) == nil)
        #expect(VolumeMount.check(URL(fileURLWithPath: "/"), identity: nil) == VolumeCheck(mounted: true, sameVolume: true))
        #expect(VolumeMount.check(URL(fileURLWithPath: "/"), identity: "uuid:nope") == VolumeCheck(mounted: true, sameVolume: false))
        #expect(VolumeMount.check(URL(fileURLWithPath: "/Volumes/ds-test-nowhere-\(UUID())"), identity: nil).mounted == false)
        // A `/Volumes` path that resolves to the startup disk (a leftover mount-point
        // folder) is never recorded as being on it.
        #expect(VolumeMount.locate(URL(fileURLWithPath: "/Volumes/Macintosh HD/private/tmp")) == nil)
        let folder = URL(fileURLWithPath: "/Users/ds-test-\(UUID())/Pics")
        #expect(VolumeMount.root(of: folder) == nil)
        VolumeMount.register(folder, VolumeRecord(mount: "/Volumes/pics", identity: nil))
        #expect(VolumeMount.root(of: folder.appendingPathComponent("a/b.ARW"))?.path == "/Volumes/pics", "a symlinked folder")
        let boot = URL(fileURLWithPath: "/Volumes/Macintosh HD-ds-\(UUID())/x")
        VolumeMount.register(boot, VolumeRecord(mount: nil, identity: nil))
        #expect(VolumeMount.root(of: boot.appendingPathComponent("a.ARW")) == nil, "registered on the startup disk")
        let settings = makeSettings()
        settings.setVolumeRecord(VolumeRecord(mount: "/net/a", identity: "uuid:1"), for: folder)
        #expect(settings.volumeRecord(for: folder) == VolumeRecord(mount: "/net/a", identity: "uuid:1"))
        #expect(settings.volumeRecord(for: tmp) == nil)
        settings.removeVolumeRecord(for: folder)
        settings.removeVolumeRecord(for: folder)
        #expect(settings.volumeRecord(for: folder) == nil)
        VolumeMount.unregister(folder)
        #expect(VolumeMount.root(of: folder) == nil)
    }

    // MARK: - Recents

    /// A sub-folder the walk couldn't read: unknown, never "no photos".
    @Test func anUnreadableSubfolderIsUnknown() throws {
        let fm = FileManager.default
        let dir = fm.temporaryDirectory.appendingPathComponent("vs-unread-\(UUID())")
        let locked = dir.appendingPathComponent("locked")
        try fm.createDirectory(at: locked.appendingPathComponent("inner"), withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        try fm.setAttributes([.posixPermissions: 0o000], ofItemAtPath: locked.path)
        let answer = RecentsHygiene.hasOriginals(dir)
        try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: locked.path)
        #expect(answer == nil)
        #expect(RecentsHygiene.hasOriginals(dir) == false, "readable: looked everywhere")
    }

    /// Folders of any RAW type ds-raw reads count (PEF, not just the card scanner's list),
    /// and a walk that couldn't look everywhere is "unknown", never "no photos".
    @Test func recentsHygieneUsesTheSupportedTypesAndNeverGuesses() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("vs-hyg-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        let fm = FileManager.default
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        try Data().write(to: dir.appendingPathComponent("IMGP0001.PEF"))
        #expect(RustRaw.extensions.contains("pef"))
        #expect(RecentsHygiene.hasOriginals(dir) == true)
        #expect(await RecentsHygiene.unavailableReason(dir) == nil)

        let deep = dir.appendingPathComponent("deep")
        let leaf = (1...8).reduce(deep) { u, i in u.appendingPathComponent("L\(i)") }
        try fm.createDirectory(at: leaf, withIntermediateDirectories: true)
        try Data().write(to: leaf.appendingPathComponent("DSC0001.ARW"))
        #expect(RecentsHygiene.hasOriginals(deep, depth: 3) == nil, "deeper than looked: unknown")
        #expect(RecentsHygiene.hasOriginals(deep, depth: 20) == true)
        let empty = dir.appendingPathComponent("empty/a")
        try fm.createDirectory(at: empty, withIntermediateDirectories: true)
        #expect(RecentsHygiene.hasOriginals(dir.appendingPathComponent("empty")) == false, "looked everywhere: none")
        #expect(await RecentsHygiene.unavailableReason(URL(fileURLWithPath: "/Volumes/ds-test-nowhere-\(UUID())/Shoot"))
            == .driveNotConnected, "a volume that isn't mounted")
    }
}
