import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Brute-force membership count (the reference the incremental counts must match).
@MainActor
func bruteCounts(_ sf: SmartFolderController, _ photos: [Photo]) -> [Int64: Int] {
    var c: [Int64: Int] = [:]
    for f in sf.folders { c[f.id] = photos.filter { sf.contains(f, $0) }.count }
    return c
}

@MainActor
@Suite struct SmartFolderTests {
    func opened(_ counts: [Int] = [60]) async -> AppModel {
        let m = makeModel(counts: counts)
        await m.start()
        return m
    }

    @Test func builtinsAreExactRatingsUnratedPickedRejectedAndTagsInUse() async throws {
        let m = await opened()
        let sf = m.smartFolders
        #expect(sf.folders.prefix(8).map(\.name) == ["5 stars", "4 stars", "3 stars", "2 stars", "1 star",
                                                     "Unrated", "Picked", "Rejected"])
        #expect(sf.folders.allSatisfy { $0.isBuiltin })
        for n in 1...5 {
            let f = try #require(sf.ratingFolder(n))
            #expect(f.criteria == .exactRating(n))
            #expect(sf.counts[f.id] == m.photos.filter { $0.rating == n }.count, "exactly \(n), not \(n) and up")
        }
        #expect(sf.counts == bruteCounts(sf, m.photos))
        // No tag in use yet: no tag folders.
        #expect(!sf.folders.contains { $0.isTagFolder })
    }

    @Test func ratingMovesAPhotoBetweenFoldersWithCorrectCounts() async {
        let m = await opened()
        let sf = m.smartFolders
        let id = m.visibleIDs.first { m.photo($0)!.rating == 0 }!
        m.focus(id, extend: false)
        let unrated = sf.folders.first { $0.builtin == "unrated" }!.id
        let three = sf.ratingFolder(3)!.id, five = sf.ratingFolder(5)!.id
        let before = sf.counts
        m.setRating(3)
        #expect(sf.counts[three] == before[three]! + 1)
        #expect(sf.counts[unrated] == before[unrated]! - 1)
        m.setRating(5)
        #expect(sf.counts[three] == before[three])
        #expect(sf.counts[five] == before[five]! + 1)
        #expect(sf.counts == bruteCounts(sf, m.photos))
        // Undo goes through the same path.
        m.undo()
        #expect(sf.counts[three] == before[three]! + 1 && sf.counts[five] == before[five])
        m.setTag(.orange)
        let orange = sf.folders.first { $0.builtin == "tag:orange" }
        #expect(orange != nil && sf.counts[orange!.id] == 1, "a tag first used now gets its folder")
        m.setPick(.rejected)
        #expect(sf.counts == bruteCounts(sf, m.photos))
        #expect(sf.visibleFolders.contains { $0.builtin == "tag:orange" })
        m.setTag(.orange) // toggles off
        #expect(!sf.visibleFolders.contains { $0.builtin == "tag:orange" }, "empty tag folders are hidden")
    }

    @Test func clickingAFolderShowsExactlyItsMembersAndFollowsChanges() async {
        let m = await opened()
        let sf = m.smartFolders
        let three = sf.ratingFolder(3)!
        m.selectSmartFolder(three.id)
        let members = m.photos.filter { sf.contains(three, $0) }.map(\.id)
        #expect(m.visibleIDs == members && m.visibleIDs.count == sf.counts[three.id])
        #expect(m.filter.isActive)
        // Rating a member away removes it from the view and the count together.
        let first = m.visibleIDs.first
        if let first {
            m.focus(first, extend: false)
            m.setRating(1)
            #expect(!m.visibleIDs.contains(first) && m.visibleIDs.count == sf.counts[three.id])
        }
        m.selectSmartFolder(nil)
        #expect(!m.filter.isActive && m.visibleIDs.count == m.photoCount)
    }

    @Test func commandShortcutsJumpToExactRatingFolders() async {
        let m = await opened()
        let sf = m.smartFolders
        #expect(MenuCommand.showAll.shortcut == .init(key: "0", modifiers: [.command]))
        #expect(MenuCommand.zoomFit.shortcut == .init(key: "0", modifiers: [.command, .option]), "⌘0 moved to all photos")
        for n in 1...5 {
            let c = MenuCommand(rawValue: "showRating\(n)")!
            #expect(c.shortcut == .init(key: Character("\(n)"), modifiers: [.command]))
            #expect(c.ratingFolder == n)
            #expect(CommandCatalog.isEnabled(c, model: m))
            #expect(CommandCatalog.title(c, model: m).contains("\(n) Star"))
            CommandCatalog.perform(c, model: m)
            #expect(m.filter.smartFolder?.id == sf.ratingFolder(n)!.id)
            #expect(m.visibleIDs.allSatisfy { m.photo($0)!.rating == n })
            #expect(m.visibleIDs.count == sf.counts[sf.ratingFolder(n)!.id])
        }
        #expect(CommandCatalog.isEnabled(.showAll, model: m))
        CommandCatalog.perform(.showAll, model: m)
        #expect(m.filter == PhotoFilter() && m.visibleIDs.count == m.photoCount)
        #expect(CommandCatalog.smartFolderMenu.count == 6 && MenuCommand.zoomIn.ratingFolder == nil)
    }

    @Test func userFoldersCreateEditDeleteAndBuiltinsAreProtected() async throws {
        let m = await opened()
        let sf = m.smartFolders
        var draft = SmartFolderDraft()
        #expect(!draft.canSave)
        draft.name = "  Keepers "
        draft.ratingMode = .atLeast
        draft.rating = 2
        draft.isoMax = "800"
        let f = try #require(await sf.create(name: draft.name, criteria: draft.criteria, photos: m.photos))
        #expect(f.name == "Keepers" && !f.isBuiltin)
        #expect(sf.counts[f.id] == m.photos.filter { $0.rating >= 2 && $0.meta.iso <= 800 }.count)
        var renamed = try #require(sf.folder(f.id))
        renamed.name = "Best"
        renamed.criteria = .exactRating(4)
        await sf.update(renamed, photos: m.photos)
        #expect(sf.folder(f.id)?.name == "Best" && sf.counts[f.id] == m.photos.filter { $0.rating == 4 }.count)
        // Built-ins can't be edited or deleted.
        let five = sf.ratingFolder(5)!
        await sf.delete(five, photos: m.photos)
        #expect(sf.ratingFolder(5) != nil && sf.lastError == SmartFolderError.builtin.errorDescription)
        await #expect(throws: SmartFolderError.builtin) { try await m.sessions.updateSmartFolder(five, in: m.currentSession!) }
        await #expect(throws: SmartFolderError.emptyName) {
            _ = try await m.sessions.createSmartFolder(named: " ", criteria: SmartCriteria(), in: m.currentSession!)
        }
        await sf.delete(renamed, photos: m.photos)
        #expect(sf.folder(f.id) == nil && sf.lastError == nil)
        // Nothing left to update.
        await sf.update(renamed, photos: m.photos)
        #expect(sf.lastError == SmartFolderError.notFound.errorDescription)
    }

    @Test func editorDraftRoundTripsEveryRule() {
        let date = Date(timeIntervalSince1970: 1_790_000_000)
        let cases: [SmartCriteria] = [
            SmartCriteria(),
            .exactRating(4),
            SmartCriteria(minRating: 2),
            SmartCriteria(maxRating: 3),
            SmartCriteria(minRating: 2, maxRating: 4),
            SmartCriteria(maxRating: 0),
            // Whole days (L7): the editor stores start of the first day ... end of the last.
            SmartCriteria(tags: [.red, .pink], pick: .picked, camera: "ILCE-9", lens: "85mm",
                          dateFrom: Calendar.current.startOfDay(for: date),
                          dateTo: Calendar.current.date(byAdding: .day, value: 2, to: Calendar.current.startOfDay(for: date))!
                              .addingTimeInterval(-0.001),
                          isoMin: 100, isoMax: 1600, edited: true,
                          keywords: ["sea", "sunset"]),
            SmartCriteria(pick: PickState.none, text: "DSC", edited: false),
        ]
        for c in cases {
            let f = SmartFolder(id: 7, name: "X", criteria: c)
            let target = SmartFolderEditorTarget.edit(f)
            #expect(target.id == "edit-7")
            #expect(target.draft.criteria == c, "round trip \(c)")
        }
        // Compare criteria, not the whole draft: the unused date pickers default to `Date()`.
        #expect(SmartFolderEditorTarget.new.draft.criteria == SmartFolderDraft().criteria && SmartFolderEditorTarget.new.id == "new")
        var d = SmartFolderDraft()
        d.ratingMode = .between
        d.rating = 5
        d.ratingHigh = 2
        d.keywords = " sea, , Sunset "
        d.isoMin = "abc"
        #expect(d.criteria == SmartCriteria(minRating: 2, maxRating: 5, keywords: ["sea", "Sunset"]))
    }

    @Test func criteriaMatchLocally() {
        var p = samplePhoto(1)
        p.rating = 3
        p.colorTag = .blue
        p.pick = .picked
        p.isEdited = true
        #expect(SmartCriteria().matchesLocally(p))
        #expect(SmartCriteria.exactRating(3).matchesLocally(p) && !SmartCriteria.exactRating(2).matchesLocally(p))
        #expect(!SmartCriteria(maxRating: 0).matchesLocally(p))
        #expect(SmartCriteria(tags: [.blue, .red]).matchesLocally(p) && !SmartCriteria(tags: [.red]).matchesLocally(p))
        #expect(!SmartCriteria(pick: .rejected).matchesLocally(p))
        #expect(SmartCriteria(camera: "ilce", lens: "85MM").matchesLocally(p))
        #expect(!SmartCriteria(camera: "Canon").matchesLocally(p) && !SmartCriteria(lens: "35mm").matchesLocally(p))
        let d = p.meta.captureDate
        #expect(SmartCriteria(dateFrom: d, dateTo: d).matchesLocally(p))
        #expect(!SmartCriteria(dateFrom: d.addingTimeInterval(1)).matchesLocally(p))
        #expect(!SmartCriteria(dateTo: d.addingTimeInterval(-1)).matchesLocally(p))
        #expect(SmartCriteria(isoMin: 200, isoMax: 200).matchesLocally(p))
        #expect(!SmartCriteria(isoMin: 400).matchesLocally(p) && !SmartCriteria(isoMax: 100).matchesLocally(p))
        #expect(!SmartCriteria(edited: false).matchesLocally(p))
        #expect(SmartCriteria(keywords: ["x"]).needsIndex && SmartCriteria(text: "a").needsIndex)
        #expect(!SmartCriteria(text: " ", keywords: [" "]).needsIndex)
        #expect(SmartCriteria(minRating: 2, keywords: ["x"]).indexPart == SmartCriteria(keywords: ["x"]))
        let scope = SmartFolderScope(id: 1, criteria: SmartCriteria(), indexMatches: [PhotoID(2)])
        #expect(!scope.contains(p))
        p.isMissing = true
        #expect(!SmartFolderScope(id: 1, criteria: SmartCriteria()).contains(p), "missing originals are never members")
        let symbols = SmartFolder.builtins(tagsInUse: [.red]).map(\.symbol)
        #expect(symbols == ["star.fill", "star.fill", "star.fill", "star.fill", "star.fill", "star.slash",
                            "flag.fill", "xmark.circle", "circle.fill"])
        #expect(SmartFolder(id: 1, name: "m", criteria: SmartCriteria()).symbol == "gearshape")
    }

    /// Per-keypress count maintenance on a 2,000-photo session: O(folders), < 0.1 ms.
    @Test func countUpdatePerKeypressOn2000PhotosIsUnderAHundredMicroseconds() {
        let photos = MockSessionStore.makePhotos(count: 2_000, firstID: 1, start: Date(timeIntervalSince1970: 1_790_000_000))
        let sf = SmartFolderController()
        var folders = SmartFolder.builtins(tagsInUse: Set(ColorTag.allCases))
        folders.append(SmartFolder(id: 1, name: "Keepers", criteria: SmartCriteria(minRating: 3, isoMax: 800)))
        folders.append(SmartFolder(id: 2, name: "Wide", criteria: SmartCriteria(lens: "16-35", edited: false)))
        sf.set(folders, index: [:], photos: photos)
        #expect(sf.counts == bruteCounts(sf, photos))
        let clock = ContinuousClock()
        var current = photos
        let presses = 20_000
        var elapsed = Duration.zero
        for i in 0..<presses {
            let k = i % current.count
            let old = current[k]
            var new = old
            new.rating = (old.rating + 1) % 6
            let t = clock.now
            sf.photoChanged(old: old, new: new)
            elapsed += clock.now - t
            current[k] = new
        }
        let perPress = elapsed / presses
        let us = Double(perPress.components.attoseconds) / 1e12 + Double(perPress.components.seconds) * 1e6
        print("smart-folder count update: \(String(format: "%.2f", us)) µs per keypress (2,000 photos, \(folders.count) folders)")
        #expect(us < 100)
        #expect(sf.counts == bruteCounts(sf, current), "incremental counts equal a full recount")
    }

    @Test func indexOnlyRulesUseTheStoreMatches() {
        let photos = (1...4).map { samplePhoto(Int64($0)) }
        let sf = SmartFolderController()
        let kw = SmartFolder(id: 9, name: "Sea", criteria: SmartCriteria(minRating: 0, keywords: ["sea"]))
        sf.set([kw], index: [9: [PhotoID(2), PhotoID(4)]], photos: photos)
        #expect(sf.counts[9] == 2)
        #expect(sf.scope(9)?.indexMatches == [PhotoID(2), PhotoID(4)])
        #expect(sf.scope(99) == nil)
        var p2 = photos[1]
        p2.isMissing = true
        sf.photoChanged(old: photos[1], new: p2)
        #expect(sf.counts[9] == 1)
    }
}
