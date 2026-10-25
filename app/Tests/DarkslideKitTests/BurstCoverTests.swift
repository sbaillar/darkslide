import CoreGraphics
import Foundation
import Testing
@testable import DarkslideCore

private func ids(_ raws: [Int64]) -> [PhotoID] { raws.map(PhotoID.init) }

private func burstPhotos() -> [Photo] {
    func photo(_ id: Int64, t: Double, rating: Int = 0) -> Photo {
        var p = samplePhoto(id)
        p.meta.captureDate = Date(timeIntervalSince1970: 1_000 + t)
        p.rating = rating
        return p
    }
    return [photo(1, t: 0), photo(2, t: 0.2, rating: 4), photo(3, t: 0.4), photo(4, t: 10), photo(5, t: 20), photo(6, t: 20.5)]
}

// MARK: - Matching covers to recomputed stacks

@Suite struct StackCoverMatchingTests {
    @Test func aCoverStaysWhileItIsStillInTheStack() {
        let covers = [StackCover(members: ids([3, 1, 2]), cover: PhotoID(2))]
        #expect(covers[0].members == ids([1, 2, 3]), "members are kept sorted")
        #expect(StackCovers.cover(for: ids([1, 2, 3]), in: covers) == PhotoID(2))
        // An import added a frame, a delete took one: the cover is still a member.
        #expect(StackCovers.cover(for: ids([1, 2, 3, 7]), in: covers) == PhotoID(2))
        #expect(StackCovers.cover(for: ids([2, 3]), in: covers) == PhotoID(2))
        // The cover itself went: the usual rule decides.
        #expect(StackCovers.cover(for: ids([1, 3]), in: covers) == nil)
        #expect(StackCovers.cover(for: ids([8, 9]), in: []) == nil)
    }

    @Test func mergedBurstsKeepTheCoverOfTheBiggerOverlapThenTheLatest() {
        let a = StackCover(members: ids([1, 2, 3]), cover: PhotoID(2))
        let b = StackCover(members: ids([4, 5]), cover: PhotoID(5))
        #expect(StackCovers.cover(for: ids([1, 2, 3, 4, 5]), in: [a, b]) == PhotoID(2), "three frames beat two")
        #expect(StackCovers.cover(for: ids([1, 2, 3, 4, 5]), in: [b, a]) == PhotoID(2))
        let c = StackCover(members: ids([6, 7]), cover: PhotoID(7))
        #expect(StackCovers.cover(for: ids([4, 5, 6, 7]), in: [b, c]) == PhotoID(7), "a tie goes to the later choice")
    }

    @Test func choosingReplacesEarlierChoicesForTheBurst() {
        let old = [StackCover(members: ids([1, 2, 3]), cover: PhotoID(2)), StackCover(members: ids([5, 6]), cover: PhotoID(6)),
                   StackCover(members: ids([9, 10]), cover: PhotoID(3))] // a stale entry naming one of the frames
        let new = StackCovers.choosing(PhotoID(1), members: ids([1, 2, 3, 4]), in: old)
        #expect(new == [StackCover(members: ids([5, 6]), cover: PhotoID(6)), StackCover(members: ids([1, 2, 3, 4]), cover: PhotoID(1))])
        #expect(StackCovers.cover(for: ids([1, 2, 3, 4]), in: new) == PhotoID(1))
    }
}

// MARK: - stacks.json

@Suite struct StackCoverFileTests {
    private func tempDir() throws -> URL {
        let d = FileManager.default.temporaryDirectory.appendingPathComponent("ds-stacks-\(UUID())", isDirectory: true)
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        return d
    }

    @Test func writesAtomicallyAndReadsBack() throws {
        let dir = try tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let url = dir.appendingPathComponent("stacks.json")
        #expect(try StackCoverFile.read(url).isEmpty, "no file: no covers")
        let covers = [StackCover(members: ids([1, 2]), cover: PhotoID(2)), StackCover(members: ids([7, 8, 9]), cover: PhotoID(9))]
        try StackCoverFile.write(covers, to: url)
        #expect(try StackCoverFile.read(url) == covers)
        try StackCoverFile.write([covers[1]], to: url) // replaced, not appended
        #expect(try StackCoverFile.read(url) == [covers[1]])
        let text = try String(contentsOf: url, encoding: .utf8)
        #expect(text.contains("\"cover\" : 9") && text.contains("\"version\" : 1"), "plain variant ids")
        #expect(try FileManager.default.contentsOfDirectory(atPath: dir.path) == ["stacks.json"], "no temporary file left")
        #expect(StackCoverFile.url(inStore: dir).path.hasSuffix("/.ds/settings/stacks.json"))
    }

    @Test func unreadableOrUnwritableFilesThrow() throws {
        let dir = try tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let url = dir.appendingPathComponent("stacks.json")
        try Data("{not json".utf8).write(to: url)
        #expect(throws: (any Error).self) { try StackCoverFile.read(url) }
        let missing = dir.appendingPathComponent("nope/stacks.json")
        #expect(throws: (any Error).self) { try StackCoverFile.write([], to: missing) }
        // A directory in the way: the rename fails and the temporary file is removed.
        let blocked = dir.appendingPathComponent("blocked")
        try FileManager.default.createDirectory(at: blocked.appendingPathComponent("child"), withIntermediateDirectories: true)
        #expect(throws: (any Error).self) { try StackCoverFile.write([], to: blocked) }
        #expect(Set(try FileManager.default.contentsOfDirectory(atPath: dir.path)) == ["stacks.json", "blocked"])
    }

    /// Live: the cover is written into the session's store and a new store (another launch,
    /// another Mac) reads it back.
    @Test func rustSessionStoreKeepsCoversWithTheSession() async throws {
        let fm = FileManager.default
        guard let folder = try sampleFolderCopy(limit: 3) else { return } // samples/ absent: the file tests above cover the format
        defer { try? fm.removeItem(at: folder) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()))
        let info = try await store.add(folder: folder)
        let photos = try await store.photos(in: info)
        try #require(photos.count == 3)
        #expect(try await store.stackCovers(in: info).isEmpty)
        let covers = [StackCover(members: photos.map(\.id), cover: photos[1].id)]
        try await store.saveStackCovers(covers, in: info)
        #expect(fm.fileExists(atPath: folder.appendingPathComponent(".ds/session/.ds/settings/stacks.json").path))
        let reopened = RustSessionStore(recents: RecentFolders(defaults: testDefaults()))
        await #expect(throws: (any Error).self, "a session that isn't open is never reopened for its covers") {
            try await reopened.saveStackCovers([], in: info)
        }
        let info2 = try await reopened.add(folder: folder)
        #expect(try await reopened.stackCovers(in: info2) == covers)
    }
}

// MARK: - The cover rule: rating first, then the user, then the AI

private func frame(_ id: Int64, rating: Int = 0, pick: PickState = .none) -> Photo {
    var p = samplePhoto(id)
    p.rating = rating
    p.pick = pick
    return p
}

@Suite struct CoverRuleTests {
    @Test func theHighestRatingWins() {
        let d = CoverRule.decide([frame(1, rating: 2), frame(2, rating: 4), frame(3, rating: 3)], chosen: PhotoID(1),
                                 ai: CoverAI(best: [PhotoID(3)], scores: [PhotoID(3): 0.99]))
        #expect(d == CoverDecision(cover: PhotoID(2), source: .rule, candidates: ids([2]), topRating: 4) && !d.isTie,
                "neither the user's 2★ choice nor the AI's 3★ pick beats a 4★ frame")
    }

    @Test func aTieGoesToTheUserThenTheAIThenTheUsualRule() {
        let tied = [frame(1, rating: 3), frame(2, rating: 3), frame(3, rating: 3), frame(4, rating: 1)]
        let ai = CoverAI(best: [PhotoID(3)], scores: [PhotoID(2): 0.9, PhotoID(3): 0.4, PhotoID(4): 1])
        #expect(CoverRule.decide(tied, chosen: PhotoID(2), ai: ai).cover == PhotoID(2))
        #expect(CoverRule.decide(tied, chosen: PhotoID(2), ai: ai).source == .user)
        let byAI = CoverRule.decide(tied, chosen: nil, ai: ai)
        #expect(byAI.cover == PhotoID(3) && byAI.source == .ai && byAI.isTie, "the AI's pick before a higher score")
        #expect(CoverRule.decide(tied, chosen: nil, ai: CoverAI(scores: [PhotoID(1): 0.2, PhotoID(2): 0.7])).cover == PhotoID(2),
                "no pick among them: the better score")
        #expect(CoverRule.decide(tied, chosen: PhotoID(4), ai: CoverAI()) == CoverDecision(
            cover: PhotoID(1), source: .rule, candidates: ids([1, 2, 3]), topRating: 3), "no opinion: the earliest")
        // The usual rule among tied frames: picked before unflagged.
        #expect(CoverRule.decide([frame(1), frame(2, pick: .picked)], chosen: nil).cover == PhotoID(2))
    }

    @Test func rejectsAreNeverTheCoverUnlessAllAre() {
        let d = CoverRule.decide([frame(1, rating: 5, pick: .rejected), frame(2, rating: 1)], chosen: PhotoID(1),
                                 ai: CoverAI(best: [PhotoID(1)]))
        #expect(d.cover == PhotoID(2) && d.candidates == ids([2]))
        let all = CoverRule.decide([frame(1, rating: 1, pick: .rejected), frame(2, rating: 4, pick: .rejected)], chosen: nil)
        #expect(all.cover == PhotoID(2) && all.topRating == 4)
        #expect(CoverRule.candidates([frame(1), frame(2)]) == (ids([1, 2]), 0), "nothing rated: every frame")
    }

    @Test func theStripListsTheHighestRatedFirst() {
        let order = CoverRule.stripOrder([frame(1, rating: 2), frame(2, rating: 5, pick: .rejected), frame(3, rating: 4),
                                          frame(4, rating: 2), frame(5)])
        #expect(order == ids([3, 1, 4, 5, 2]), "by rating, ties in capture order, rejects last")
    }
}

@MainActor
@Suite struct StackCoverStateTests {
    let ps = burstPhotos() // 1, 2 (4★), 3 · 4 · 5, 6

    @Test func ratingsChoicesAndHintsDecideTheTop() {
        let s = StackState()
        s.update(photos: ps, grouping: CaptureTimeGrouping(), best: [PhotoID(3)], covers: [StackCover(members: ids([1, 2, 3]), cover: PhotoID(1))])
        #expect(s.stacks[0].topID == PhotoID(2) && s.stacks[0].coverID == PhotoID(1), "the 4★ frame; the choice is kept")
        #expect(s.stacks[1].topID == PhotoID(5) && s.ai.best == [PhotoID(3)])
        s.collapseAll()
        var byID = Dictionary(uniqueKeysWithValues: ps.map { ($0.id, $0) })
        let lookup: (PhotoID) -> Photo? = { byID[$0] }
        // A 5★ frame takes over at once (no rebuild).
        byID[PhotoID(3)]!.rating = 5
        #expect(s.displayIDs(from: ps.map(\.id), lookup: lookup) == ids([3, 4, 5]))
        // All tied again: the user's choice, kept on disk while it was outranked, is back.
        for i in [1, 2, 3] as [Int64] { byID[PhotoID(i)]!.rating = 3 }
        #expect(s.currentTop(of: s.stacks[0], lookup: lookup) == PhotoID(1))
        #expect(s.decision(of: s.stacks[0], lookup: lookup)?.source == .user)
        // Choice withdrawn: the AI's pick among the tie.
        s.apply(covers: [])
        #expect(s.stacks[0].coverID == nil && s.stacks[0].topID == PhotoID(2), "as built: 2 was the only 4★")
        #expect(s.decision(of: s.stacks[0], lookup: lookup)?.source == .ai)
        #expect(s.displayIDs(from: ps.map(\.id), lookup: lookup).first == PhotoID(3))
        s.apply(covers: []) // unchanged: nothing to do
        #expect(s.collapsed.count == 2, "applying covers keeps the collapse state")
        #expect(s.decision(of: s.stacks[0], lookup: { _ in nil }) == nil)
        #expect(s.currentTop(of: s.stacks[0], lookup: { _ in nil }) == s.stacks[0].topID)
    }

    @Test func aStaleCoverFallsBackToTheRule() {
        let s = StackState()
        // The cover was a frame of this burst that is gone now (deleted, or filtered out).
        s.update(photos: ps, grouping: CaptureTimeGrouping(), covers: [StackCover(members: ids([1, 2, 3, 9]), cover: PhotoID(9))])
        #expect(s.stacks[0].coverID == nil && s.stacks[0].topID == PhotoID(2))
        #expect(PhotoStack(id: PhotoID(1), memberIDs: ids([1, 2]), topID: PhotoID(1), coverID: PhotoID(5)).coverID == nil)
    }

    @Test func browserStateChoosesAndKeepsCoversAcrossRebuilds() {
        let b = BrowserState()
        b.load(covers: [StackCover(members: ids([5, 6]), cover: PhotoID(6))])
        b.aiScores = [PhotoID(5): 0.9]
        b.refresh(photos: ps)
        b.stackBursts = true
        #expect(b.displayIDs(ps.map(\.id)) == ids([2, 4, 6]), "the stored cover of the 5–6 burst beats the AI's score")
        #expect(b.chooseCover(PhotoID(4)) == nil, "not in a burst")
        let saved = b.chooseCover(PhotoID(3))
        #expect(saved?.count == 2 && saved?.last == StackCover(members: ids([1, 2, 3]), cover: PhotoID(3)))
        #expect(b.displayIDs(ps.map(\.id)) == ids([2, 4, 6]), "3 isn't top-rated: 2 (4★) stays the cover")
        b.refresh(photos: Array(ps.reversed())) // regrouped: the choices hold
        #expect(b.stacks.stack(of: PhotoID(3))?.coverID == PhotoID(3) && b.stacks.stack(of: PhotoID(6))?.coverID == PhotoID(6))
        b.reset()
        #expect(b.covers.isEmpty && b.aiScores.isEmpty)
    }
}

// MARK: - The picker model

@MainActor
@Suite struct BurstPickerTests {
    @Test func opensMovesAndCloses() {
        let p = BurstPicker()
        let stack = PhotoStack(id: PhotoID(1), memberIDs: ids([1, 2, 3]), topID: PhotoID(2))
        #expect(!p.isOpen && p.move(1) == nil)
        p.open(stack, focused: PhotoID(9), anchor: CGRect(x: 10, y: 0, width: 50, height: 40))
        #expect(p.isOpen && p.current == PhotoID(2) && p.frames == ids([1, 2, 3]) && p.anchor?.midX == 35)
        #expect(p.move(1) == PhotoID(3) && p.move(1) == PhotoID(3), "clamped at the end")
        #expect(p.move(-5) == PhotoID(1))
        p.select(PhotoID(9))
        #expect(p.current == PhotoID(1), "not a frame: ignored")
        p.open(stack, focused: PhotoID(3))
        #expect(p.current == PhotoID(3), "opens at the focused frame")
        p.noteCover(PhotoID(3))
        #expect(p.confirmedCover == PhotoID(3) && p.confirmations == 1)
        // Rebuilt: the frame went, the strip follows; the burst went, it closes.
        p.sync(PhotoStack(id: PhotoID(1), memberIDs: ids([1, 2]), topID: PhotoID(1)))
        #expect(p.frames == ids([1, 2]) && p.current == PhotoID(1))
        p.sync(nil)
        #expect(!p.isOpen && p.frames.isEmpty && p.current == nil)
        p.sync(stack)
        #expect(!p.isOpen, "a closed strip stays closed")
    }
}

// MARK: - The app: setting, strip, cover

@MainActor
@Suite struct BurstCoverModelTests {
    private func key(_ m: AppModel, _ input: KeyInput) -> Bool {
        m.handleKey(input, focus: .none, sheetOpen: false, isMainWindow: true)
    }

    /// A model with stacking on and its biggest burst.
    private func stacked(_ store: (any SessionStore)? = nil) async -> (AppModel, PhotoStack)? {
        let m = makeModel(counts: [60], store: store)
        await m.start()
        m.setStackBursts(true)
        guard let s = m.browserState.stacks.stacks.max(by: { $0.count < $1.count }), s.count >= 3 else {
            Issue.record("mock data has no burst of three")
            return nil
        }
        return (m, s)
    }

    @Test func stackBurstsIsOnByDefaultAndRemembered() async {
        let fresh = AppSettings(defaults: UserDefaults(suiteName: testSuiteName())!)
        #expect(fresh.stackBursts, "on by default")
        fresh.stackBursts = false
        #expect(!AppSettings(defaults: fresh.userDefaults).stackBursts)

        let m = makeModel(counts: [60])
        m.settings.stackBursts = true // what a user's settings say before the browser exists
        #expect(m.browserState.stackBursts)
        await m.start()
        m.setStackBursts(false)
        #expect(!m.settings.stackBursts && !AppSettings(defaults: m.settings.userDefaults).stackBursts)
        m.perform(.toggleStack) // S with stacking off turns it on, and that is remembered too
        #expect(m.browserState.stackBursts && m.settings.stackBursts)
    }

    @Test func turningStackingOffShowsAndWalksEveryFrame() async {
        guard let (m, s) = await stacked() else { return }
        #expect(m.navigationIDs.count < m.visibleIDs.count)
        #expect(m.browserState.displayIDs(m.visibleIDs).count < m.visibleIDs.count)
        m.focus(s.topID, extend: false)
        m.toggleBurstStrip(for: s.topID)
        m.setStackBursts(false)
        #expect(m.navigationIDs == m.visibleIDs && m.browserState.displayIDs(m.visibleIDs) == m.visibleIDs)
        #expect(m.browserState.stacks.badgeCount(for: s.topID) == nil && !m.browserState.picker.isOpen)
        m.focus(m.visibleIDs[0], extend: false)
        for _ in 1..<m.visibleIDs.count { m.perform(.move(.right, extend: false)) }
        #expect(m.focusedID == m.visibleIDs.last, "every frame walked")
        m.setStackBursts(false) // no change
        #expect(!m.browserState.stackBursts)
    }

    /// Every frame of `s` unrated and unflagged: all of them tie for the cover.
    private func level(_ m: AppModel, _ s: PhotoStack) {
        m.updatePhotos(s.memberIDs, label: "Reset") { $0.rating = 0; $0.pick = .none }
    }

    @Test func theStripOpensWalksAndSetsTheCover() async throws {
        guard let (m, s) = await stacked() else { return }
        level(m, s)
        let b = m.browserState
        let p = b.picker
        let oldTop = b.stacks.currentTop(of: s, lookup: { m.photo($0) })
        #expect(oldTop == s.memberIDs[0], "all tied, no AI: the earliest")
        m.focus(oldTop, extend: false)
        m.toggleBurstStrip(for: oldTop, anchor: CGRect(x: 0, y: 0, width: 100, height: 80))
        #expect(p.isOpen && p.frames == s.memberIDs && p.current == oldTop && m.burstStripShown)
        #expect(m.burstStripOrder == s.memberIDs, "all tied: capture order")
        let (f0, f1, f2) = (s.memberIDs[0], s.memberIDs[1], s.memberIDs[2])
        #expect(key(m, KeyInput(special: .left)) && m.focusedID == f0, "clamped at the first frame")
        #expect(key(m, KeyInput(special: .right)) && m.focusedID == f1 && p.current == f1)
        #expect(m.canUseAsCover(f1) && m.coverHint(for: f1) == nil)
        #expect(!m.handleBurstStripKey(KeyInput("", special: .right, shift: true)), "⇧→ extends as usual")
        // ⏎: Use as Cover (a tie: the user's pick).
        #expect(key(m, KeyInput(special: .returnKey)))
        #expect(b.stacks.stack(of: f1)?.coverID == f1 && p.confirmations == 1 && p.confirmedCover == f1)
        #expect(m.coverDecision(of: f1)?.source == .user)
        // The filmstrip's stack cell is the new cover.
        #expect(b.displayIDs(m.visibleIDs, lookup: { m.photo($0) }).contains(f1))
        // Rating and flag keys go to the shown frame, which stays shown: 3★ on f2 makes it
        // the cover by itself, first in the strip; the user's choice is kept.
        #expect(key(m, KeyInput(special: .right)) && m.focusedID == f2)
        #expect(key(m, KeyInput("3")) && m.photo(f2)?.rating == 3 && p.isOpen && m.focusedID == f2)
        #expect(key(m, KeyInput("p")) && m.photo(f2)?.pick == .picked && m.focusedID == f2)
        #expect(m.coverDecision(of: f2)?.cover == f2 && m.burstStripOrder.first == f2)
        #expect(b.displayIDs(m.visibleIDs, lookup: { m.photo($0) }).contains(f2))
        // f1 can't be the cover now: the strip says what it takes, and ⏎ on it says so too.
        #expect(!m.canUseAsCover(f1) && m.coverHint(for: f1) == "Rate it ★★★ or higher to make it the cover")
        m.showInBurstStrip(f1)
        #expect(key(m, KeyInput(special: .returnKey)) && p.confirmations == 1 && m.toast?.message.contains("★★★") == true)
        // f2 back to no stars: tied again, and the user's pick is the cover again.
        m.showInBurstStrip(f2)
        #expect(key(m, KeyInput("0")) && m.coverDecision(of: f2)?.cover == f1)
        #expect(key(m, KeyInput(special: .escape)) && !p.isOpen)
        #expect(key(m, KeyInput(special: .returnKey)) && !p.isOpen && p.confirmations == 1, "closed: ⏎ is the viewer's again")
        await b.coverSave?.value
        #expect(try await m.sessions.stackCovers(in: try #require(m.currentSession)).last?.cover == f1)
        // A rejected frame is never offered.
        m.updatePhotos([f0], label: "Reject") { $0.pick = .rejected }
        #expect(m.coverHint(for: f0) == "Unreject it to make it the cover" && m.burstStripOrder.last == nil)
        #expect(m.coverHint(for: PhotoID(999_999)) == nil && !m.canUseAsCover(PhotoID(999_999)))
    }

    @Test func theBadgeAgainOrFocusElsewhereClosesTheStrip() async {
        guard let (m, s) = await stacked() else { return }
        let p = m.browserState.picker
        m.toggleBurstStrip(for: s.memberIDs[0])
        #expect(p.isOpen && m.focusedID.map(s.memberIDs.contains) == true, "the viewer shows its current frame")
        m.toggleBurstStrip(for: s.memberIDs[0])
        #expect(!p.isOpen, "the badge again closes it")
        m.openBurstStrip(for: s.memberIDs[0])
        m.openBurstStrip(for: s.memberIDs[0]) // already open: stays
        #expect(p.isOpen)
        let outside = m.visibleIDs.first { m.browserState.stacks.stack(of: $0) == nil }!
        m.focus(outside, extend: false)
        #expect(!p.isOpen, "focus left the burst")
        m.openBurstStrip(for: outside) // not a collapsed burst
        #expect(!p.isOpen)
        // The S key expands the burst: its frames are in the filmstrip, no strip.
        m.openBurstStrip(for: s.memberIDs[0])
        m.perform(.toggleStack)
        #expect(!p.isOpen && !m.browserState.stacks.isCollapsed(s))
        // An expanded stack's badge (or the grid) expands / collapses as before.
        m.toggleBurstStrip(for: s.memberIDs[0])
        #expect(m.browserState.stacks.isCollapsed(s) && !p.isOpen)
        m.browserMode = .grid
        m.toggleBurstStrip(for: s.memberIDs[0])
        #expect(!m.browserState.stacks.isCollapsed(s) && !p.isOpen)
        m.toggleBurstStrip(for: outside) // not in a burst: nothing
        m.closeBurstStrip()
        #expect(!p.isOpen)
    }

    @Test func aHiddenStripLetsKeysThrough() async {
        guard let (m, s) = await stacked() else { return }
        let p = m.browserState.picker
        m.toggleBurstStrip(for: s.memberIDs[0])
        m.filmstripHidden = true
        #expect(!m.burstStripShown && !m.handleBurstStripKey(KeyInput(special: .right)) && p.isOpen)
        m.filmstripHidden = false
        #expect(!m.handleBurstStripKey(KeyInput("", special: .escape, command: true)))
        #expect(!m.handleBurstStripKey(KeyInput("x")), "other keys go on")
        m.browserMode = .grid
        #expect(!m.handleBurstStripKey(KeyInput(special: .right)) && !p.isOpen, "the grid has no strip")
    }

    @Test func theCoverIsStoredWithTheSessionAndComesBack() async throws {
        guard let (m, s) = await stacked() else { return }
        level(m, s)
        await m.saver.flush()
        let cover = s.memberIDs.last!
        m.useAsCover(cover) // e.g. the cell's context menu, strip closed
        #expect(m.focusedID == cover && !m.browserState.picker.isOpen)
        await m.browserState.coverSave?.value
        // Another model on the same store (a relaunch) shows the same cover.
        let m2 = makeModel(counts: [60], store: m.sessions)
        await m2.start()
        m2.setStackBursts(true)
        #expect(m2.browserState.stacks.stack(of: cover)?.topID == cover)
        #expect(m2.browserState.displayIDs(m2.visibleIDs, lookup: { m2.photo($0) }).contains(cover))
        // Surveying the burst starts on its cover.
        m2.focus(s.memberIDs[0], extend: false)
        m2.beginSurvey()
        #expect(m2.survey.focused == cover && m2.focusedID == cover)
        m2.survey.finish()
        m.useAsCover(PhotoID(999_999)) // not a photo in a burst: nothing
        #expect(m.focusedID == cover)
    }

    @Test func coverStoreFailuresAreShownAndAnUnreadableFileIsIgnored() async throws {
        let store = CoverFailingStore()
        guard let (m, s) = await stacked(store) else { return }
        level(m, s)
        #expect(m.browserState.covers.isEmpty, "unreadable covers: the stacks open with their usual tops")
        m.useAsCover(s.memberIDs[1])
        await m.browserState.coverSave?.value
        #expect(m.toast?.isError == true)
        #expect(m.browserState.stacks.stack(of: s.memberIDs[1])?.coverID == s.memberIDs[1], "shown even though unsaved")
    }
}

/// Loads like the mock; its covers can't be read or written.
private struct CoverFailingStore: StackCoverStoring {
    let inner = MockSessionStore(counts: [60])
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { try await inner.save(photos: photos) }
    func edit(for id: PhotoID) async throws -> EditState { try await inner.edit(for: id) }
    func save(edits: [PhotoID: EditState]) async throws { try await inner.save(edits: edits) }
    func storedStackCovers(in session: SessionInfo) async throws -> [StackCover] { throw MockError.invalid("corrupt") }
    func storeStackCovers(_ covers: [StackCover], in session: SessionInfo) async throws { throw MockError.invalid("read-only") }
}
