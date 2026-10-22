import Foundation
import Testing
@testable import DarkslideKit

// final-journeys2 N5, N9, N10 and the context-menu discoverability note.

@MainActor @Suite struct CullFinishJ2Tests {
    func model(_ store: Cul9Store) async -> AppModel {
        let m = makeModel(store: store)
        await m.start()
        return m
    }

    func flag(_ m: AppModel, _ id: PhotoID, _ p: PickState) {
        m.focus(id, extend: false)
        m.setPick(p)
    }

    /// N5: picked, or rated at least the setting (★★★ by default), of the current
    /// view; of the selection when more than one photo is selected. The
    /// confirmation counts exactly what moves and names the rule.
    @Test func selectsFollowTheViewTheSelectionAndTheMinimumRating() async {
        let store = Cul9Store(count: 8)
        let m = await model(store)
        let ids = m.visibleIDs
        for (i, n) in [(0, 1), (1, 3), (2, 5), (4, 5), (5, 2)] { m.rate(ids[i], n) }
        flag(m, ids[3], .picked)
        flag(m, ids[4], .rejected)
        m.selection = []
        #expect(m.settings.selectsMinRating == 3)
        var plan = m.cull.selectsPlan
        #expect(plan.ids == [ids[1], ids[2], ids[3]] && plan.scope == .view, "1★ and 2★ stay; a rejected 5★ stays")
        #expect(plan.message == "Move 3 photos to Selects (Picked, or rated ★★★ and up, in the current view)")
        // A filtered view (⌘5-like): only what it shows.
        m.filter.minRating = 5
        #expect(m.cull.selectIDs == [ids[2]])
        m.filter.minRating = 0
        // A selection of more than one photo: only the selection.
        m.selection = [ids[0], ids[2], ids[3], ids[5]]
        plan = m.cull.selectsPlan
        #expect(plan.ids == [ids[2], ids[3]] && plan.scope == .selection)
        #expect(plan.message == "Move 2 photos to Selects (Picked, or rated ★★★ and up, in the selection)")
        // One selected photo is not a selection scope.
        m.selection = [ids[0]]
        #expect(m.cull.selectsPlan.scope == .view)
        m.selection = []
        m.settings.selectsMinRating = 5
        plan = m.cull.selectsPlan
        #expect(plan.ids == [ids[2], ids[3]])
        #expect(plan.message == "Move 2 photos to Selects (Picked, or rated ★★★★★, in the current view)")
        #expect(CullFinisher.SelectsPlan(ids: [ids[0]], scope: .view, minRating: 4).message.hasPrefix("Move 1 photo to"))

        // The command asks first; the confirmation moves exactly what it counted.
        #expect(CommandCatalog.title(.movePickedToSelects, model: m) == "Move Picked and Rated to Selects…")
        CommandCatalog.perform(.movePickedToSelects, model: m)
        #expect(m.cull.pendingSelects?.ids == [ids[2], ids[3]])
        #expect(await store.movesSnapshot().isEmpty, "nothing moves before the confirmation")
        m.rate(ids[1], 5) // changed while the sheet is up: not counted, not moved
        await m.cull.confirmMoveToSelects()
        #expect(m.cull.pendingSelects == nil)
        #expect(m.photo(ids[2])?.inSelects == true && m.photo(ids[3])?.inSelects == true && m.photo(ids[1])?.inSelects == false)
        #expect(m.toast?.message == "Moved 2 photos to Selects (⌘Z to undo)")
        await m.cull.confirmMoveToSelects() // nothing pending: nothing happens
        #expect(await store.movesSnapshot() == [.selects])
    }

    @Test func nothingToMoveAsksNothingAndInPlaceRefuses() async {
        let store = Cul9Store(count: 3)
        let m = await model(store)
        m.rate(m.visibleIDs[0], 1)
        m.cull.requestMoveToSelects()
        #expect(m.cull.pendingSelects == nil && !CommandCatalog.isEnabled(.movePickedToSelects, model: m))
        let inPlace = Cul9Store(count: 3)
        await inPlace.setCanMove(false)
        let ip = await model(inPlace)
        ip.rate(ip.visibleIDs[0], 5)
        ip.cull.requestMoveToSelects()
        #expect(ip.cull.pendingSelects == nil)
    }

    /// The setting persists and is clamped to 1…5.
    @Test func selectsMinRatingPersists() {
        let suite = testSuiteName()
        let d = UserDefaults(suiteName: suite)!
        d.removePersistentDomain(forName: suite)
        let s = AppSettings(defaults: d)
        #expect(s.selectsMinRating == 3)
        s.selectsMinRating = 4
        #expect(AppSettings(defaults: d).selectsMinRating == 4)
        s.selectsMinRating = 9
        #expect(s.selectsMinRating == 5 && AppSettings(defaults: d).selectsMinRating == 5)
        d.set(0, forKey: "selectsMinRating")
        #expect(AppSettings(defaults: d).selectsMinRating == 1)
    }

    /// N10: a variant hidden by Move Rejected to Trash comes back with "Restore
    /// from Session Trash" even when the undo record is gone (a relaunch).
    @Test func restoreFromTrashBringsSetAsideVariantsBack() async {
        let store = Cul9Store(count: 2)
        let m = await model(store)
        let (a, b) = (m.visibleIDs[0], m.visibleIDs[1])
        m.focus(a, extend: false)
        await m.cull.newVariant()
        let v = m.focusedID!
        flag(m, v, .rejected)
        flag(m, b, .rejected)
        await m.cull.moveRejectedToTrash()
        #expect(m.photo(v) == nil && m.cull.trashedIDs.count == 1)
        // Relaunch: a new model on the same store has no undo record.
        let m2 = await model(store)
        #expect(m2.cull.lastMove == nil && m2.photo(v) == nil)
        #expect(CommandCatalog.isEnabled(.restoreSessionTrash, model: m2))
        CommandCatalog.perform(.restoreSessionTrash, model: m2)
        #expect(await waitUntil { m2.photo(v) != nil })
        #expect(await waitUntil { m2.toast?.message == "Restored 1 photo and 1 variant from the session Trash" })
        #expect(m2.photo(v)?.pick == .rejected && m2.cull.trashedIDs.isEmpty)
        await m2.cull.restoreFromTrash()
        #expect(m2.toast?.message == "The session Trash is empty")
        #expect(CullFinisher.restoredMessage(files: 0, variants: 2) == "Restored 2 variants from the session Trash")
        #expect(CullFinisher.restoredMessage(files: 3, variants: 0) == "Restored 3 photos from the session Trash")
    }

    /// Discoverability: the cull-finishing commands are in the browser's context menu.
    @Test func cullCommandsAreInTheContextMenu() async {
        let m = await model(Cul9Store(count: 3))
        let items = CommandCatalog.cullContextMenu.compactMap { $0 }
        for c in [MenuCommand.moveRejectedToTrash, .movePickedToSelects, .deleteRejected, .survey, .restoreSessionTrash] {
            #expect(items.contains(c))
        }
        for c in items { #expect(!CommandCatalog.title(c, model: m).isEmpty) }
    }
}

// MARK: - N9: survey pages and rating keys

@MainActor @Suite struct SurveyJ2Tests {
    let ids = (1...40).map { PhotoID(Int64($0)) }

    @Test func moreThanAPageIsPagedNotDropped() {
        let s = SurveyModel()
        #expect(s.begin(ids))
        #expect(s.candidates.count == 40 && s.visible == Array(ids.prefix(12)))
        #expect(s.countLabel == "12 of 40" && s.pageCount == 4 && s.page == 0)
        s.move(by: -1) // wraps to the last one: its page
        #expect(s.focused == ids[39] && s.page == 3 && s.visible == Array(ids[36...]) && s.countLabel == "4 of 40")
        s.move(by: 1)
        #expect(s.page == 0)
        for _ in 0..<12 { s.move(by: 1) }
        #expect(s.focused == ids[12] && s.page == 1, "→ runs on to the next page")
        #expect(s.handle(KeyInput("]")) == .handled && s.page == 2 && s.focused == ids[24])
        #expect(s.handle(KeyInput("[")) == .handled && s.page == 1 && s.focused == ids[12])
        s.turnPage(by: -5)
        #expect(s.page == 0)
        s.turnPage(by: -1)
        #expect(s.page == 0 && s.focused == ids[0], "clamped")
        s.eliminate()
        #expect(s.candidates.count == 39 && s.visible.count == 12 && s.visible.first == ids[1])
        let small = SurveyModel()
        small.begin(Array(ids.prefix(5)))
        #expect(small.countLabel == "5" && small.pageCount == 1 && small.visible.count == 5)
        #expect(small.finish().count == 5 && small.visible.isEmpty && small.page == 0)
    }

    @Test func ratingTagAndFlagKeysAreMarks() {
        let s = SurveyModel()
        s.begin(Array(ids.prefix(3)))
        #expect(s.handle(KeyInput("5")) == .mark(.rate(5)))
        #expect(s.handle(KeyInput("0")) == .mark(.rate(0)))
        #expect(s.handle(KeyInput("p")) == .mark(.pick))
        #expect(s.handle(KeyInput("6")) == .mark(.tag(.red)))
        #expect(s.handle(KeyInput("x")) == .handled && s.candidates.count == 2, "X stays the survey's eliminate")
        #expect(s.handle(KeyInput("u")) == .handled && s.candidates.count == 3, "U stays put back")
        #expect(s.handle(KeyInput("g")) == .handled, "other bound keys are swallowed")
        #expect(s.handle(KeyInput("q")) == .handled)
        var b = KeyBindings.defaults
        b.bind(.reject, to: "r")
        b.bind(.unflag, to: "n")
        #expect(s.handle(KeyInput("r"), bindings: b) == .mark(.reject))
        #expect(s.handle(KeyInput("n"), bindings: b) == .mark(.unflag))
    }

    /// The app: 16 selected → all 16 surveyed; 5 and P rate and pick only the
    /// focused candidate (one undo step each), not the whole selection.
    @Test func keysRateTheFocusedCandidateOnly() async {
        let m = makeModel(store: Cul9Store(count: 16))
        await m.start()
        let all = m.visibleIDs
        m.selection = Set(all)
        m.beginSurvey()
        #expect(m.survey.candidates == all && m.survey.countLabel == "12 of 16")
        let key = { (k: KeyInput) in m.handleKey(k, focus: .none, sheetOpen: false, isMainWindow: true) }
        #expect(key(KeyInput(special: .right)))
        let f = all[1]
        #expect(m.survey.focused == f)
        #expect(key(KeyInput("5")) && m.photo(f)?.rating == 5)
        #expect(key(KeyInput("p")) && m.photo(f)?.pick == .picked)
        #expect(key(KeyInput("6")) && m.photo(f)?.colorTag == .red)
        #expect(key(KeyInput("6")) && m.photo(f)?.colorTag == ColorTag.none, "the same tag again clears it")
        #expect(all.filter { $0 != f }.allSatisfy { m.photo($0)?.rating == 0 && m.photo($0)?.pick == PickState.none })
        #expect(m.survey.focused == f && m.survey.isActive, "no auto-advance, still surveying")
        m.undo()
        m.undo()
        m.undo()
        #expect(m.photo(f)?.pick == PickState.none && m.photo(f)?.rating == 5)
        m.settings.keyBindings.bind(.reject, to: "r")
        #expect(key(KeyInput("r")) && m.photo(f)?.pick == .rejected && m.survey.candidates.count == 16)
        m.settings.keyBindings.bind(.unflag, to: "n")
        #expect(key(KeyInput("n")) && m.photo(f)?.pick == PickState.none)
        #expect(key(KeyInput("x")) && m.survey.candidates.count == 15)
    }
}
