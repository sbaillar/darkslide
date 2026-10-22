import Foundation
import OSLog
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// User bug: sliders didn't change the picture. The app had reopened a recent folder
/// whose ARWs were gone (only `.ds/session` was left): it showed cached previews, saved
/// the edits, and re-rendered nothing, silently. Missing originals must always be said,
/// recents that can't open must never be reopened on their own, and render failures
/// must be visible (and logged).
@MainActor @Suite(.serialized) struct MissingOriginalsTests {
    /// A live app (ds-session store, ds-pipeline renderer) over `defaults`' recents.
    static func liveApp(defaults: UserDefaults, base: URL, ignoreTemporary: Bool = false) async -> AppModel {
        let store = RustSessionStore(recents: RecentFolders(defaults: defaults), storeBase: base)
        await store.setIgnoresTemporaryRecents(ignoreTemporary)
        let settings = makeSettings()
        settings.onboardingComplete = true
        settings.autoAdvance = false
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: DevelopRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""), ai: MockAIService(),
                         settings: settings, previews: RustPreviewProvider(), mode: .live)
        m.watchCards = false
        m.toastDuration = .seconds(3600)
        m.viewer.tileDelay = .zero
        m.viewer.settleDelay = .milliseconds(20)
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        return m
    }

    static func arws(in dir: URL) throws -> [URL] {
        try FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
            .filter { $0.pathExtension.lowercased() == "arw" }.sorted { $0.path < $1.path }
    }

    static func scratch(_ name: String) -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("\(name)-\(UUID())")
    }

    /// Open a folder, delete one original, focus that photo and move a slider: the
    /// banner says the original is missing (and pulses on the edit), its cells are
    /// badged, the edit is kept, nothing crashes; Remove forgets it.
    @Test func deletedOriginalShowsTheBannerAndASliderPulsesIt() async throws {
        guard let dir = try sampleFolderCopy(limit: 3) else { return }
        let base = Self.scratch("mo-store")
        defer {
            try? FileManager.default.removeItem(at: dir)
            try? FileManager.default.removeItem(at: base)
        }
        let m = await Self.liveApp(defaults: testDefaults(), base: base)
        defer { m.stop() }
        await m.openFolder(dir)
        #expect(await waitUntil(timeout: .seconds(60)) { m.viewer.frame != nil }, "the first photo develops")
        #expect(m.viewer.problem == nil)
        let victim = try #require(m.visibleIDs.dropFirst().first)
        let name = try #require(m.photo(victim)?.filename)
        // Warm the pipeline on it first, so its pixels are cached (the bug's state).
        m.focus(victim, extend: false)
        #expect(await waitUntil(timeout: .seconds(60)) { m.viewer.frame != nil })
        m.focus(m.visibleIDs.first, extend: false)
        try FileManager.default.removeItem(at: dir.appendingPathComponent(name))

        m.focus(victim, extend: false)
        #expect(await waitUntil { m.viewer.problem == .missingOriginal(name: name) }, "\(String(describing: m.viewer.problem))")
        #expect(m.item(victim)?.isOriginalMissing == true, "grid and filmstrip cells badge it")
        #expect(MissingActions.badge(m.photo(victim)!, missing: true) == "Missing")
        #expect(m.viewer.frame == nil, "no develop from cached pixels under the banner")
        #expect(m.visibleIDs.contains(victim), "it stays where it was, focused")

        let pulses = m.viewer.problemPulse
        for i in 1...5 {
            m.updateEdit(label: "Exposure", coalesceKey: "exposure") { $0.exposure.ev = Double(i) / 5 }
        }
        m.endEditGesture()
        #expect(m.viewer.problemPulse >= pulses + 5, "each tick pulses the banner")
        #expect(m.edit(for: victim).exposure.ev == 1, "the edit is kept")
        try await Task.sleep(for: .milliseconds(300))
        #expect(m.viewer.frame == nil && m.viewer.problem == .missingOriginal(name: name))
        await m.saver.flush()
        #expect(m.saver.state == .saved, "saved")

        // The others are unaffected.
        m.focus(m.visibleIDs.first, extend: false)
        #expect(await waitUntil(timeout: .seconds(60)) { m.viewer.frame != nil && m.viewer.problem == nil })
        #expect(!m.sessionOriginalsMissing, "one missing photo is not a missing session")

        // Found missing only in this session: Locate, never Remove (the store hasn't
        // verified it; a drive that dropped off looks the same).
        m.focus(victim, extend: false)
        #expect(m.viewer.problem?.actions.map(\.title) == ["Locate…"])
        await m.removeMissing([victim])
        #expect(m.photo(victim) != nil, "nothing forgotten")
        // Once the store has verified it (the folder re-checked), Remove forgets it (never
        // touches files).
        await m.recheckMissingOriginals()
        #expect(m.photo(victim)?.isMissing == true && m.canRemoveMissing(victim))
        await m.removeMissing([victim])
        #expect(m.photo(victim) == nil && m.photoCount == 2)
        #expect(try Self.arws(in: dir).count == 2)
    }

    /// Recents for a deleted folder and for a folder whose photos are gone are listed,
    /// greyed out with why, and never reopened on launch (not even an older session in
    /// their place): the welcome state shows the list.
    @Test func recentsThatCantOpenAreNeverReopened() async throws {
        guard let kept = try sampleFolderCopy(limit: 1), let deleted = try sampleFolderCopy(limit: 1),
              let emptied = try sampleFolderCopy(limit: 1) else { return }
        let base = Self.scratch("mo-store")
        defer { for u in [kept, deleted, emptied, base] { try? FileManager.default.removeItem(at: u) } }
        let defaults = testDefaults()
        let first = await Self.liveApp(defaults: defaults, base: base)
        for dir in [kept, deleted, emptied] { await first.openFolder(dir) } // `emptied` is the most recent
        first.stop()
        try FileManager.default.removeItem(at: deleted)
        for f in try Self.arws(in: emptied) { try FileManager.default.removeItem(at: f) }

        let m = await Self.liveApp(defaults: defaults, base: base)
        defer { m.stop() }
        await m.start()
        #expect(m.currentSession == nil, "nothing reopened")
        #expect(m.photoCount == 0 && m.toast == nil, "no error, the welcome state")
        #expect(m.emptyState == .recents, "the recents list, with Import and Open Folder")
        let byName = Dictionary(uniqueKeysWithValues: m.sessionList.map { ($0.url.lastPathComponent, $0.unavailable) })
        #expect(byName[emptied.lastPathComponent] == .some(.noPhotos))
        #expect(byName[deleted.lastPathComponent] == .some(.notFound))
        #expect(byName[kept.lastPathComponent] == .some(nil), "a usable folder stays usable")
        #expect(SessionInfo.Unavailable.notFound.label == "not found")

        // Clicking the one that's gone says what happened instead of failing obscurely.
        let gone = try #require(m.sessionList.first { $0.unavailable == .notFound })
        await m.open(gone)
        #expect(m.currentSession == nil)
        #expect(m.toast?.message.contains("Locate Folder") == true, "\(m.toast?.message ?? "")")
    }

    /// Recents in temporary folders (dev and test artifacts, like the user's stale
    /// `/tmp/…/r4-live/Iceland`) are ignored; other missing folders are still listed.
    @Test func temporaryRecentsAreIgnored() async throws {
        guard let temp = try sampleFolderCopy(limit: 1) else { return }
        let base = Self.scratch("mo-store")
        defer {
            try? FileManager.default.removeItem(at: temp)
            try? FileManager.default.removeItem(at: base)
        }
        let defaults = testDefaults()
        let recents = RecentFolders(defaults: defaults)
        recents.add(URL(fileURLWithPath: "/Volumes/ds-test-unplugged-\(UUID())/Shoot"))
        recents.add(URL(fileURLWithPath: "/private/tmp/scratch/r4-live/Iceland"))
        let first = await Self.liveApp(defaults: defaults, base: base)
        await first.openFolder(temp) // a real folder in a temporary directory
        first.stop()
        #expect(recents.folders.count == 3)

        let m = await Self.liveApp(defaults: defaults, base: base, ignoreTemporary: true)
        defer { m.stop() }
        await m.start()
        #expect(m.currentSession == nil && m.toast == nil)
        #expect(m.sessionList.count == 1, "\(m.sessionList.map(\.url.path))")
        #expect(m.sessionList.first?.url.path.hasPrefix("/Volumes/ds-test-unplugged-") == true)
        #expect(m.sessionList.first?.unavailable == .driveNotConnected)
    }

    @Test func temporaryLocations() {
        for p in ["/tmp/x", "/private/tmp/a/r4-live/Iceland", "/private/var/folders/ab/T/x", "/var/folders/x"] {
            #expect(RecentsHygiene.isTemporary(URL(fileURLWithPath: p)), "\(p)")
        }
        for p in ["/Users/me/Pictures/Shoot", "/Volumes/pics/2026", "/tmpfoo/x", "/private/tmpx"] {
            #expect(!RecentsHygiene.isTemporary(URL(fileURLWithPath: p)), "\(p)")
        }
        #expect(!RecentsHygiene.ignoresTemporaryFolders, "tests run in a temporary support root")
    }

    @Test func unavailableReasons() async throws {
        let dir = Self.scratch("mo-hygiene")
        defer { try? FileManager.default.removeItem(at: dir) }
        let fm = FileManager.default
        #expect(await RecentsHygiene.unavailableReason(dir) == .notFound)
        try fm.createDirectory(at: dir.appendingPathComponent(".ds/session"), withIntermediateDirectories: true)
        try fm.createDirectory(at: dir.appendingPathComponent("Trash"), withIntermediateDirectories: true)
        try Data().write(to: dir.appendingPathComponent(".ds/session/X.ARW"))
        try Data().write(to: dir.appendingPathComponent("Trash/rejected.ARW"))
        try Data().write(to: dir.appendingPathComponent("notes.txt"))
        #expect(await RecentsHygiene.unavailableReason(dir) == .noPhotos, ".ds and Trash don't count")
        try fm.createDirectory(at: dir.appendingPathComponent("Capture/Day 1"), withIntermediateDirectories: true)
        try Data().write(to: dir.appendingPathComponent("Capture/Day 1/DSC0001.arw"))
        #expect(await RecentsHygiene.unavailableReason(dir) == nil, "photos in subfolders count")
        #expect(RecentsHygiene.hasOriginals(dir, budget: 1) == nil, "an exhausted budget is no answer")
        let file = dir.appendingPathComponent("notes.txt")
        #expect(await RecentsHygiene.unavailableReason(file) == .notFound, "a file isn't a folder")
    }

    /// A whole session's originals gone (the photos moved elsewhere, `.ds` left behind):
    /// one session-level message, no per-photo noise; Locate Folder relinks it to where
    /// the photos are, with every rating and edit, and leaves the old store in place.
    @Test func wholeSessionMissingShowsOneMessageAndLocateRelinksIt() async throws {
        guard let dir = try sampleFolderCopy(limit: 3) else { return }
        let base = Self.scratch("mo-store")
        let moved = Self.scratch("mo-moved")
        defer { for u in [dir, base, moved] { try? FileManager.default.removeItem(at: u) } }
        let defaults = testDefaults()
        let first = await Self.liveApp(defaults: defaults, base: base)
        await first.openFolder(dir)
        let ids = first.visibleIDs
        let edited = try #require(first.photo(ids[0])?.filename), rated = try #require(first.photo(ids[1])?.filename)
        first.focus(ids[0], extend: false)
        first.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        first.endEditGesture()
        first.focus(ids[1], extend: false)
        first.setRating(4)
        await first.saver.flush()
        first.stop()
        try FileManager.default.createDirectory(at: moved, withIntermediateDirectories: true)
        for f in try Self.arws(in: dir) {
            try FileManager.default.moveItem(at: f, to: moved.appendingPathComponent(f.lastPathComponent))
        }

        let m = await Self.liveApp(defaults: defaults, base: base)
        defer { m.stop() }
        await m.start()
        #expect(m.currentSession == nil, "not reopened")
        let s = try #require(m.sessionList.first)
        #expect(s.unavailable == .noPhotos)
        await m.open(s)
        #expect(m.currentSession?.id == s.id)
        #expect(m.sessionOriginalsMissing)
        #expect(m.viewer.problem == .sessionMissing(folder: dir.lastPathComponent))
        #expect(m.viewer.problem?.isSessionWide == true && m.viewer.problem?.actions.first?.title == "Locate Folder…")
        #expect(m.visibleIDs.isEmpty && m.missingCount == 3, "one message, not three")

        await m.locateSession(s, at: moved)
        #expect(m.currentSession?.url.lastPathComponent == moved.lastPathComponent)
        #expect(m.photoCount == 3 && m.missingCount == 0 && !m.sessionOriginalsMissing)
        #expect(m.viewer.problem == nil)
        let byName = Dictionary(uniqueKeysWithValues: m.photos.map { ($0.filename, $0.id) })
        #expect(m.edit(for: try #require(byName[edited])).exposure.ev == 1, "edits carried over")
        #expect(m.photo(try #require(byName[rated]))?.rating == 4, "ratings carried over")
        #expect(await waitUntil(timeout: .seconds(60)) { m.viewer.frame != nil }, "and it develops again")
        let fm = FileManager.default
        #expect(fm.fileExists(atPath: dir.appendingPathComponent(".ds/session/.ds/settings/session.json").path),
                "the old store is left where it was")
        #expect(fm.fileExists(atPath: moved.appendingPathComponent(".ds/session/.ds/settings/session.json").path))
        #expect(m.sessionList.map(\.url.lastPathComponent) == [moved.lastPathComponent], "the recent follows it")
    }

    /// A folder that's gone entirely (and its store with it) can still be pointed at
    /// where its photos are now: it opens there.
    @Test func locatingAFolderThatIsGoneOpensTheNewOne() async throws {
        guard let dir = try sampleFolderCopy(limit: 1), let other = try sampleFolderCopy(limit: 2) else { return }
        let base = Self.scratch("mo-store")
        defer { for u in [dir, other, base] { try? FileManager.default.removeItem(at: u) } }
        let defaults = testDefaults()
        let first = await Self.liveApp(defaults: defaults, base: base)
        await first.openFolder(dir)
        first.stop()
        try FileManager.default.removeItem(at: dir)
        let m = await Self.liveApp(defaults: defaults, base: base)
        defer { m.stop() }
        await m.start()
        let s = try #require(m.sessionList.first)
        #expect(s.unavailable == .notFound)
        await m.locateSession(s, at: other)
        #expect(m.currentSession?.url.lastPathComponent == other.lastPathComponent && m.photoCount == 2)
    }

    /// Any develop failure shows the banner with its reason and Retry (non-blocking),
    /// is logged under the render category, and a good render clears it.
    @Test func renderFailureShowsRetryAndIsLogged() async throws {
        let r = FakeRenderer()
        let m = makeModel(counts: [4], renderer: r)
        await m.start()
        r.failWith = MockError.invalid("GPU timed out decoding the frame")
        m.viewer.retryRender()
        #expect(await waitUntil { m.viewer.problem.map { "\($0)" }?.contains("GPU timed out") == true },
                "\(String(describing: m.viewer.problem))")
        guard case .renderFailed? = m.viewer.problem else {
            Issue.record("a render failure, not \(String(describing: m.viewer.problem))")
            return
        }
        #expect(m.viewer.problem?.actions.map(\.title) == ["Retry"])
        r.failWith = nil
        m.viewer.retryRender()
        #expect(await waitUntil { m.viewer.problem == nil && m.viewer.frame != nil })

        // The failure is in the unified log (`log show --predicate 'subsystem ==
        // "casa.baillargeon.darkslide"'`). Sandboxed shells can't read the log store at
        // all (no entries of any kind come back): the check then can't run, and says so.
        let store = try OSLogStore(scope: .currentProcessIdentifier)
        let since = store.position(timeIntervalSinceLatestBoot: ProcessInfo.processInfo.systemUptime - 300)
        let entries = try store.getEntries(at: since).compactMap { $0 as? OSLogEntryLog }
        guard !entries.isEmpty else {
            print("MissingOriginalsTests: the log store isn't readable here; the log line can't be checked")
            return
        }
        let lines = entries.filter { $0.subsystem == AppLog.subsystem && $0.category == "render" }.map(\.composedMessage)
        // The file name and the error text are private (hashed), so only the shape is checked.
        #expect(lines.contains { $0.hasPrefix("develop of ") && $0.contains(" failed: ") }, "\(lines.suffix(5))")
    }

    /// A RAW Darkslide can't develop (ds-raw says unsupported: X-Trans, a codec rawler
    /// lacks) is said plainly, without a Retry that can't help; the embedded preview
    /// stays up for culling. Another failure after it is a normal render failure again.
    @Test func unsupportedRawSaysCantDevelopYet() async throws {
        let r = FakeRenderer()
        let m = makeModel(counts: [4], renderer: r)
        await m.start()
        r.failWith = RenderError(.Unsupported(message: "unsupported camera FUJIFILM X-T4: X-Trans sensors"))
        m.viewer.retryRender()
        #expect(await waitUntil { if case .cannotDevelop? = m.viewer.problem { true } else { false } },
                "\(String(describing: m.viewer.problem))")
        let p = try #require(m.viewer.problem)
        #expect(p.message.hasPrefix("Darkslide can’t develop this file yet (unsupported camera FUJIFILM X-T4"))
        #expect(p.message.contains("embedded preview"))
        #expect(p.actions.isEmpty && !p.isSessionWide)
        r.failWith = MockError.invalid("GPU timed out")
        m.viewer.retryRender()
        #expect(await waitUntil { if case .renderFailed? = m.viewer.problem { true } else { false } })
        // Other `.unsupported` failures (not a RAW any more, no preview, invalid levels)
        // are damage, not a format gap: Retry, not "can't develop yet".
        for message in ["not a RAW file: unrecognized file signature", "no embedded preview found",
                        "invalid raw image: black level above white"] {
            r.failWith = RenderError(.Unsupported(message: message))
            m.viewer.retryRender()
            #expect(await waitUntil { m.viewer.problem == .renderFailed(message) }, "\(message)")
        }
        r.failWith = nil
        m.viewer.retryRender()
        #expect(await waitUntil { m.viewer.problem == nil && m.viewer.frame != nil })
    }

    @Test func unsupportedFormatIsOnlyDsRawsUnsupported() {
        #expect(OpenFailures.isUnsupportedFormat(RenderError(.Unsupported(message: "unsupported camera Nikon: HE"))))
        #expect(OpenFailures.isUnsupportedFormat(RenderError(.Unsupported(message: "unsupported file: CR3"))))
        #expect(!OpenFailures.isUnsupportedFormat(RenderError(.Unsupported(message: "no embedded preview found"))))
        #expect(!OpenFailures.isUnsupportedFormat(RenderError(.Io(message: "unsupported camera X"))))
        #expect(!OpenFailures.isUnsupportedFormat(MockError.invalid("unsupported camera X")))
    }

    @Test func bannerContent() {
        let photo = ViewerProblem.missingOriginal(name: "DSC01234.ARW")
        #expect(photo.message.hasPrefix("Original file missing"))
        #expect(photo.message.contains("edits are saved but can’t be previewed or exported"))
        #expect(photo.actions.map(\.title) == ["Locate…"] && !photo.isSessionWide, "not verified: no Remove")
        #expect(ViewerProblem.missingOriginal(name: "x", canRemove: true).actions.map(\.title) == ["Locate…", "Remove…"])
        let session = ViewerProblem.sessionMissing(folder: "Iceland")
        #expect(session.message.contains("“Iceland”") && session.isSessionWide)
        #expect(session.actions.map(\.action) == [.locateFolder, .removeFromRecents])
        #expect(ViewerProblem.renderFailed("x").message.hasSuffix("x"))
        #expect(Set([photo.symbol, session.symbol, ViewerProblem.renderFailed("x").symbol,
                     ViewerProblem.cannotDevelop("x").symbol]).count == 4)
    }

    /// Remove from Recents on a missing session closes it and forgets the entry only.
    @Test func removingAMissingSessionForgetsTheEntryOnly() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        let base = Self.scratch("mo-store")
        defer {
            try? FileManager.default.removeItem(at: dir)
            try? FileManager.default.removeItem(at: base)
        }
        let defaults = testDefaults()
        let m = await Self.liveApp(defaults: defaults, base: base)
        defer { m.stop() }
        await m.openFolder(dir)
        for f in try Self.arws(in: dir) { try FileManager.default.removeItem(at: f) }
        await m.reloadSessions(select: m.currentSession)
        #expect(m.viewer.problem?.isSessionWide == true)
        await m.forgetCurrentSession()
        #expect(m.currentSession == nil && m.sessionList.isEmpty && m.viewer.problem == nil)
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent(".ds/session").path), "nothing deleted")
    }
}
