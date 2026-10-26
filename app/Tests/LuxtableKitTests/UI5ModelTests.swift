import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Round 5 (R4 review M6, L6, L7, L8, L12, test hygiene).
@Suite struct UI5ModelTests {
    @MainActor
    func matching(_ m: AppModel) -> Set<PhotoID> { Set(m.photos.filter { m.filter.matches($0) }.map(\.id)) }

    // MARK: M6

    @MainActor
    @Test func editingOrDeletingTheSelectedFolderRefreshesTheBrowser() async throws {
        let m = makeModel(counts: [30])
        await m.start()
        let ids = m.visibleIDs
        for (i, id) in ids.prefix(6).enumerated() {
            m.focus(id, extend: false)
            m.setRating(i < 3 ? 2 : 0)
        }
        m.focus(ids[10], extend: false)
        m.setPick(.rejected)
        let f = try #require(await m.smartFolders.create(name: "Rated", criteria: SmartCriteria(minRating: 1),
                                                          photos: m.photos))
        m.selectSmartFolder(f.id)
        let rated = Set(m.photos.filter { $0.rating >= 1 }.map(\.id))
        #expect(Set(m.visibleIDs) == rated)
        var g = f
        g.criteria = SmartCriteria(pick: .rejected)
        await m.smartFolders.update(g, photos: m.photos)
        #expect(m.filter.smartFolder?.criteria == g.criteria, "the browser follows the folder's new rules")
        #expect(Set(m.visibleIDs) == Set(m.photos.filter { $0.pick == .rejected }.map(\.id)))
        #expect(Set(m.visibleIDs) == matching(m))
        await m.smartFolders.delete(g, photos: m.photos)
        #expect(m.filter.smartFolder == nil, "a deleted folder is deselected")
        #expect(m.visibleIDs.count == m.photos.count)
    }

    @MainActor
    @Test func refreshedFindsAPersistedBuiltinByKey() async {
        let m = makeModel(counts: [5])
        await m.start()
        let sf = m.smartFolders
        let five = sf.scope(sf.ratingFolder(5)!.id)!
        #expect(sf.refreshed(five) == five)
        let moved = SmartFolderScope(id: 424_242, criteria: five.criteria, builtin: "rating:5")
        #expect(sf.refreshed(moved)?.id == five.id)
        #expect(sf.refreshed(SmartFolderScope(id: 424_242, criteria: SmartCriteria())) == nil)
    }

    // MARK: L8

    @MainActor
    @Test func smartFolderKeepsSearchTextAndNeverStacksWithBarCriteria() async {
        let m = makeModel(counts: [30])
        await m.start()
        m.filter.text = "DSC"
        m.showRatingFolder(5)
        #expect(m.filter.text == "DSC" && m.filter.smartFolder != nil, "⌘5 keeps the typed search")
        m.filter.text = "DSC0"
        #expect(m.filter.smartFolder != nil, "search text combines with the folder")
        m.filter.pick = .picked
        #expect(m.filter.smartFolder == nil && m.filter.pick == .picked, "bar criteria replace the folder")
        #expect(m.filter.text == "DSC0")
        m.filter.minRating = 2
        m.showRatingFolder(4)
        #expect(m.filter.minRating == 0 && m.filter.pick == .all && m.filter.smartFolder != nil,
                "a folder replaces bar criteria")
        m.selectSmartFolder(nil)
        #expect(m.filter == PhotoFilter(text: "DSC0"))
        #expect(Set(m.visibleIDs) == matching(m))
    }

    @Test func reconcileIsPure() {
        let s = SmartFolderScope(id: 1, criteria: .exactRating(5))
        let old = PhotoFilter(smartFolder: s)
        var new = old
        new.unratedOnly = true
        #expect(new.reconciled(from: old).smartFolder == nil)
        new = old
        new.text = "x"
        #expect(new.reconciled(from: old).smartFolder == s)
        // Selecting a folder while bar criteria are set: the new folder wins (different id).
        let bar = PhotoFilter(minRating: 3)
        #expect(PhotoFilter(minRating: 3, smartFolder: s).reconciled(from: bar).smartFolder == s)
        #expect(PhotoFilter(colorTag: .red).reconciled(from: old).smartFolder == nil)
    }

    // MARK: L7

    @Test func wallClockBoundsRoundTripAndEditorUsesWholeDays() throws {
        let tz = try #require(TimeZone(identifier: "America/New_York"))
        let d = Date(timeIntervalSince1970: 1_786_000_000) // an August instant (EDT, -4h)
        #expect(SmartCriteria.wallClockMs(d, timeZone: tz) == 1_786_000_000_000 - 4 * 3_600_000)
        #expect(SmartCriteria.date(wallClockMs: SmartCriteria.wallClockMs(d, timeZone: tz), timeZone: tz) == d)
        var draft = SmartFolderDraft()
        draft.useDateFrom = true
        draft.useDateTo = true
        draft.dateFrom = d
        draft.dateTo = d
        let c = draft.criteria
        let cal = Calendar.current
        #expect(c.dateFrom == cal.startOfDay(for: d))
        #expect(c.dateTo! > d && cal.isDate(c.dateTo!, inSameDayAs: d))
        #expect(c.dateTo!.addingTimeInterval(0.002) >= cal.date(byAdding: .day, value: 1, to: cal.startOfDay(for: d))!)
        // Local evaluation compares camera wall clocks: a photo at 23:30 on the camera
        // (whatever the offset) is in "that day".
        var p = samplePhoto(1)
        let dayStartWall = SmartCriteria.wallClockMs(cal.startOfDay(for: d))
        p.meta.captureWallClockMs = dayStartWall + (23 * 60 + 30) * 60_000
        p.meta.captureDate = Date(timeIntervalSince1970: 0) // the true instant doesn't matter
        #expect(c.matchesLocally(p))
        p.meta.captureWallClockMs = dayStartWall - 1
        #expect(!c.matchesLocally(p))
        // Without a camera clock (demo photos), the capture date in the local zone is used.
        var q = samplePhoto(2)
        q.meta.captureDate = d
        #expect(q.meta.wallClockMs == SmartCriteria.wallClockMs(d))
    }

    // MARK: L12

    @Test func recentFoldersAreKeyedByCanonicalPathAndRemovable() throws {
        let fm = FileManager.default
        let base = fm.temporaryDirectory.appendingPathComponent("ui5-recents-\(UUID())")
        defer { try? fm.removeItem(at: base) }
        let a = base.appendingPathComponent("samples", isDirectory: true)
        let b = base.appendingPathComponent("copy/samples", isDirectory: true)
        try fm.createDirectory(at: a, withIntermediateDirectories: true)
        try fm.createDirectory(at: b, withIntermediateDirectories: true)
        let link = base.appendingPathComponent("alias")
        try fm.createSymbolicLink(at: link, withDestinationURL: a)
        let d = testDefaults()
        // Legacy duplicates written by older builds collapse on read.
        d.set([a.path, link.path, a.path + "/.", b.path], forKey: "recentFolders")
        let r = RecentFolders(defaults: d)
        #expect(r.folders.count == 2)
        r.add(link)
        r.add(URL(fileURLWithPath: a.path + "/../samples"))
        #expect(r.folders.map(\.path) == [RecentFolders.canonical(a).path, RecentFolders.canonical(b).path])
        r.remove(link)
        #expect(r.folders.map(\.path) == [RecentFolders.canonical(b).path])
    }

    @MainActor
    @Test func sessionsListRemoveFromListAndSubtitle() async throws {
        let fm = FileManager.default
        let dir = fm.temporaryDirectory.appendingPathComponent("ui5-sessions-\(UUID())", isDirectory: true)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        let recents = RecentFolders(defaults: testDefaults())
        recents.add(dir)
        let store = RustSessionStore(recents: recents)
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        await m.reloadSessions()
        let s = try #require(m.sessionList.first)
        #expect(m.canRemoveFromList(s))
        await m.removeFromList(s)
        #expect(m.sessionList.isEmpty && recents.folders.isEmpty)
        #expect(fm.fileExists(atPath: dir.path), "the folder itself is never touched")
        // Demo store has no recents list.
        let demo = makeModel(counts: [3])
        await demo.start()
        #expect(!demo.canRemoveFromList(demo.sessionList[0]))
        await demo.removeFromList(demo.sessionList[0])
        #expect(demo.sessionList.count >= 1)

        let info = SessionInfo(id: UUID(), name: "Trip", url: URL(fileURLWithPath: "/Users/me/Pictures/Trip"),
                               photoCount: 3, createdAt: Date())
        #expect(AppModel.sessionSubtitle(info, home: "/Users/me") == "3 photos · ~/Pictures")
        #expect(AppModel.sessionSubtitle(info, home: "/Users/other") == "3 photos · /Users/me/Pictures")
    }

    @MainActor
    @Test func snapshotRunsUseTheirOwnDefaults() {
        #expect(AppModel.defaults(arguments: ["Luxtable"]) === UserDefaults.standard)
        #expect(AppModel.defaults(arguments: ["Luxtable", "--snapshot", "/tmp/x.png"]) !== UserDefaults.standard)
    }

    // MARK: Test hygiene

    @Test func testSuitesLeaveNoPlistBehind() async throws {
        let suite = testSuiteName()
        #expect(suite.hasPrefix(TestDefaults.root.path), "a path suite in the run's own folder")
        #expect(!suite.contains("/Library/Preferences"), "never a domain in ~/Library/Preferences")
        let d = UserDefaults(suiteName: suite)!
        d.set(42, forKey: "k")
        CFPreferencesAppSynchronize(suite as CFString)
        let path = TestDefaults.plistPath(suite)
        for _ in 0..<40 where !FileManager.default.fileExists(atPath: path) {
            try await Task.sleep(for: .milliseconds(50))
        }
        #expect(FileManager.default.fileExists(atPath: path), "precondition: the suite was written to disk")
        #expect(TestDefaults.remove(suite))
        #expect(!FileManager.default.fileExists(atPath: path))
        #expect(UserDefaults(suiteName: suite)!.object(forKey: "k") == nil)
    }

    /// Leftovers cfprefsd writes back after exit are swept by the next run; a
    /// recent (possibly live, concurrent) suite plist is left alone.
    @Test func staleTestPlistsAreSweptRecentOnesKept() throws {
        let fm = FileManager.default
        let stale = TestDefaults.plistPath(TestDefaults.legacyPrefix + "sweep-stale." + UUID().uuidString)
        let fresh = TestDefaults.plistPath(TestDefaults.legacyPrefix + "sweep-fresh." + UUID().uuidString)
        defer { unlink(fresh); unlink(stale) }
        #expect(fm.createFile(atPath: stale, contents: Data()) && fm.createFile(atPath: fresh, contents: Data()))
        try fm.setAttributes([.modificationDate: Date().addingTimeInterval(-3600)], ofItemAtPath: stale)
        // Any concurrent test process sweeps the same folder, so the count may already be
        // 0 here (the one flaky run); what matters is what's left on disk.
        TestDefaults.sweepStale()
        #expect(!fm.fileExists(atPath: stale) && fm.fileExists(atPath: fresh))
    }
}

/// L6 and L7 through the real FFI + lt-session on copies of the sample RAWs.
@Suite(.serialized) struct UI5LiveTests {
    @MainActor
    func model(_ store: RustSessionStore) -> AppModel {
        let settings = makeSettings()
        settings.onboardingComplete = true
        settings.autoAdvance = false
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        return m
    }

    @Test func newlyUsedTagGetsAPersistedFolderBeforeAnyStoreCall() async throws {
        guard let dir = try sampleFolderCopy(limit: 4) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()))
        let info = try await store.add(folder: dir)
        let m = await model(store)
        await m.open(info)
        let sf = await m.smartFolders
        let (found, count) = await MainActor.run { () -> (SmartFolder?, Int?) in
            sf.mirrorFS = FakeMirrorFS()
            sf.mirrorDelay = .milliseconds(5)
            m.focus(m.visibleIDs[0], extend: false)
            m.setTag(.purple)
            let f = sf.folders.first { $0.builtin == "tag:purple" }
            return (f, f.flatMap { sf.counts[$0.id] })
        }
        // Right away: a local placeholder (never sent to Rust)...
        let placeholder = try #require(found)
        #expect(placeholder.id < 0)
        #expect(count == 1)
        // ...toggling Mirror on it resolves the persisted folder first (no NotFound).
        await sf.setMirror(placeholder, on: true, photos: await m.photos)
        await sf.waitForMaterialize()
        #expect(await sf.lastError == nil)
        let real = try #require(await sf.folders.first { $0.builtin == "tag:purple" })
        #expect(real.id > 0 && real.mirror)
        #expect(await sf.counts[real.id] == 1)
        #expect(await sf.folders.allSatisfy { $0.id > 0 })
        // Selected placeholder follows to its persisted id.
        await MainActor.run { m.selectSmartFolder(real.id) }
        #expect(await m.visibleIDs.count == 1)
    }

    @Test func rustDateFilterCountsMatchLocalEvaluation() async throws {
        guard let dir = try sampleFolderCopy(limit: 8) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()))
        let info = try await store.add(folder: dir)
        let m = await model(store)
        await m.open(info)
        let photos = await m.photos
        #expect(photos.allSatisfy { $0.meta.captureWallClockMs != nil })
        for p in photos {
            let bound = SmartCriteria.date(wallClockMs: p.meta.wallClockMs)
            for c in [SmartCriteria(dateFrom: bound), SmartCriteria(dateTo: bound)] {
                let local = photos.filter(c.matchesLocally).count
                let rust = try await store.storeQuery(c, in: info).count
                #expect(rust == local, "bound at \(p.filename)")
            }
        }
        // A whole-day folder over the shoot's first day agrees too.
        var draft = SmartFolderDraft()
        draft.useDateFrom = true
        draft.useDateTo = true
        let first = photos.min { $0.meta.wallClockMs < $1.meta.wallClockMs }!
        draft.dateFrom = SmartCriteria.date(wallClockMs: first.meta.wallClockMs)
        draft.dateTo = draft.dateFrom
        let day = draft.criteria
        #expect(try await store.storeQuery(day, in: info).count == photos.filter(day.matchesLocally).count)
        #expect(photos.filter(day.matchesLocally).count >= 1)
        // Round trip through the record keeps the bound.
        let back = RustSessionStore.criteria(RustSessionStore.record(day))
        #expect(back.dateFrom.map { SmartCriteria.wallClockMs($0) } == day.dateFrom.map { SmartCriteria.wallClockMs($0) })
    }
}
