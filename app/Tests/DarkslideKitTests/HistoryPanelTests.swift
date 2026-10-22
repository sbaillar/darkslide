import Foundation
import Testing
@testable import DarkslideKit

/// [history] The History panel: rows per photo, jumps, branches, the bound, and ⌘Z.
@Suite("Edit history list")
struct PhotoHistoryTests {
    func ev(_ v: Double) -> EditState {
        var e = EditState()
        e.exposure.ev = v
        return e
    }

    func st(_ v: Double) -> HistoryState { HistoryState(edit: ev(v)) }

    @Test func recordsAStartRowThenOneRowPerStepNewestFirst() {
        var h = PhotoHistory()
        #expect(h.isEmpty && h.rows.isEmpty && h.current == nil)
        h.record(label: "Exposure", before: HistoryState(edit: EditState()), after: st(1), coalesceKey: nil, merging: false)
        h.record(label: "Look Cinematic", before: st(1), after: st(2), coalesceKey: nil, merging: false)
        #expect(h.rows.map(\.label) == ["Look Cinematic", "Exposure", "Original"])
        #expect(h.rows.map(\.edit) == [ev(2), ev(1), EditState()])
        #expect(h.current === h.top && !h.isAfterCurrent(h.rows[0]))
        var edited = PhotoHistory()
        edited.record(label: "Exposure", before: st(0.5), after: st(1), coalesceKey: nil, merging: false)
        #expect(edited.rows.last?.label == "Start", "a photo edited before this session starts from its edit")
    }

    @Test func aMergingStepReplacesTheNewestRow() {
        var h = PhotoHistory()
        h.record(label: "Exposure", before: HistoryState(edit: EditState()), after: st(0.1), coalesceKey: "ev", merging: false)
        h.record(label: "Exposure", before: st(0.1), after: st(0.2), coalesceKey: "ev", merging: true)
        h.record(label: "Exposure", before: st(0.2), after: st(0.3), coalesceKey: "ev", merging: true)
        #expect(h.rows.map(\.label) == ["Exposure", "Original"] && h.top?.edit == ev(0.3))
        h.record(label: "Contrast", before: st(0.3), after: st(0.4), coalesceKey: "c", merging: true)
        #expect(h.rows.count == 3, "a different control is its own row")
    }

    @Test func jumpKeepsLaterRowsUntilANewEditBranches() throws {
        var h = PhotoHistory()
        for i in 1...3 { h.record(label: "s\(i)", before: st(Double(i - 1)), after: st(Double(i)), coalesceKey: nil, merging: false) }
        let s1 = try #require(h.rows.first { $0.label == "s1" })
        h.jump(to: s1)
        #expect(h.current === s1 && h.rows.count == 4, "later rows stay listed")
        #expect(h.rows.filter(h.isAfterCurrent).map(\.label) == ["s3", "s2"])
        h.record(label: "new", before: st(1), after: st(9), coalesceKey: nil, merging: false)
        #expect(h.rows.map(\.label) == ["new", "s1", "Original"], "the edit branched off s1")
        #expect(h.current === h.top)
    }

    func chainLength(_ h: PhotoHistory) -> Int {
        var n = 0
        var node = h.top
        while let x = node {
            n += 1
            node = x.parent
        }
        return n
    }

    @Test func boundedToTheLimitAndRechained() throws {
        let limit = PhotoHistory.limit
        var h = PhotoHistory()
        func add(_ i: Int) { h.record(label: "s\(i)", before: st(Double(i - 1)), after: st(Double(i)), coalesceKey: nil, merging: false) }
        for i in 1..<(2 * limit) { add(i) }
        #expect(h.isTrimmed, "the Original row is past the 100 listed")
        let beforeRechain = h
        add(2 * limit) // depth 200: re-chained to the newest 100
        #expect(chainLength(h) == limit)
        #expect(h.rows[1].id == beforeRechain.rows[0].id, "re-chained rows keep their ids (the panel keeps them)")
        #expect(chainLength(beforeRechain) == 2 * limit, "an undo step's snapshot keeps its own chain")
        #expect(beforeRechain.rows.first?.label == "s\(2 * limit - 1)")
        for i in (2 * limit + 1)...(2 * limit + 10) { add(i) }
        let rows = h.rows
        #expect(rows.count == limit && chainLength(h) == limit + 10)
        #expect(rows.first?.label == "s\(2 * limit + 10)" && rows.last?.label == "s\(limit + 11)")
        #expect(h.isTrimmed)
        // Jumps and branches work on re-chained rows.
        let mid = rows[50]
        h.jump(to: mid)
        #expect(h.isAfterCurrent(rows[0]) && !h.isAfterCurrent(rows[99]) && !h.isAfterCurrent(mid))
        h.record(label: "k", before: mid.state, after: st(-1), coalesceKey: "k", merging: false)
        h.record(label: "k", before: st(-1), after: st(-2), coalesceKey: "k", merging: true)
        #expect(h.rows.first?.label == "k" && h.rows.first?.edit == ev(-2) && h.rows[1] === mid)
        var fresh = PhotoHistory()
        fresh.record(label: "a", before: st(0), after: st(1), coalesceKey: nil, merging: false)
        #expect(!fresh.isTrimmed && !PhotoHistory().isTrimmed)
    }

    @Test func aMergeKeepsTheRowIdAndClearedHasOneRow() {
        var h = PhotoHistory()
        h.record(label: "Exposure", before: st(0), after: st(1), coalesceKey: "ev", merging: false)
        let id = h.top?.id
        h.record(label: "Exposure", before: st(1), after: st(2), coalesceKey: "ev", merging: true)
        #expect(h.top?.id == id && h.top?.edit == ev(2))
        let c = PhotoHistory.cleared(st(2))
        #expect(c.rows.map(\.label) == ["Current edit"] && c.current === c.top && !c.isTrimmed)
    }

    @Test func stepsCarryHistoriesThroughMergeEmptinessAndRebase() {
        var a = PhotoHistory()
        a.record(label: "x", before: HistoryState(edit: EditState()), after: st(1), coalesceKey: "k", merging: false)
        var b = a
        b.record(label: "x", before: st(1), after: st(2), coalesceKey: "k", merging: true)
        var s1 = HistoryStep(label: "x", coalesceKey: "k")
        s1.editsBefore[PhotoID(1)] = EditState()
        s1.editsAfter[PhotoID(1)] = ev(1)
        s1.histories[PhotoID(1)] = HistoryChange(before: PhotoHistory(), after: a)
        var s2 = HistoryStep(label: "x", coalesceKey: "k")
        s2.editsBefore[PhotoID(1)] = ev(1)
        s2.editsAfter[PhotoID(1)] = ev(2)
        s2.histories[PhotoID(1)] = HistoryChange(before: a, after: b)
        var h = EditHistory()
        #expect(!h.merges(s1))
        h.record(s1)
        #expect(h.merges(s2))
        h.record(s2)
        #expect(h.undoStack.count == 1 && h.undoStack[0].histories[PhotoID(1)] == HistoryChange(before: PhotoHistory(), after: b))
        // A history-only step (Clear History) is a step; one that changes nothing isn't.
        var clear = HistoryStep(label: "Clear History")
        clear.histories[PhotoID(1)] = HistoryChange(before: b, after: PhotoHistory())
        #expect(!clear.isEmpty)
        clear.histories[PhotoID(1)] = HistoryChange(before: b, after: b)
        #expect(clear.isEmpty)
        h.rebase(onto: [:])
        #expect(h.undoStack.isEmpty, "a photo that left takes its rows with it")
    }
}

@MainActor
@Suite("Edit history panel")
struct HistoryPanelModelTests {
    @Test func rowsFollowEditsAndADragIsOneRow() async {
        let m = makeModel()
        await m.start()
        #expect(m.focusedHistory.isEmpty)
        m.applyLook(LookPack.look(id: "look.cinematic")!)
        for v in [0.1, 0.2, 0.3] { m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = v } }
        m.endEditGesture()
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.5 }
        m.endEditGesture()
        #expect(m.focusedHistory.rows.map(\.label) == ["Exposure", "Exposure", "Look Cinematic", "Original"])
        #expect(m.focusedHistory.rows[1].edit.exposure.ev == 0.3, "the drag's row holds where it ended")
        // ⌘Z takes the newest row off, ⇧⌘Z puts it back.
        m.undo()
        #expect(m.focusedHistory.rows.count == 3 && m.focusedEdit.exposure.ev == 0.3)
        m.redo()
        #expect(m.focusedHistory.rows.count == 4 && m.focusedEdit.exposure.ev == 0.5)
        // Culling isn't an edit: no row.
        m.setRating(3)
        #expect(m.focusedHistory.rows.count == 4)
    }

    @Test func jumpIsOneUndoStepAndANewEditBranches() async throws {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        m.updateEdit(label: "Contrast") { $0.exposure.contrast = 20 }
        m.updateEdit(label: "Saturation") { $0.exposure.saturation = 10 }
        let full = m.focusedEdit
        let exposure = try #require(m.focusedHistory.rows.first { $0.label == "Exposure" })
        let depth = m.history.undoStack.count
        m.jumpToHistory(exposure)
        #expect(m.focusedEdit == exposure.edit && m.focusedEdit.exposure.contrast == 0)
        #expect(m.history.undoStack.count == depth + 1 && m.history.undoLabel == "Go to Exposure")
        #expect(m.focusedHistory.current === exposure && m.focusedHistory.rows.count == 4, "later rows stay")
        // Undo the jump: back to the newest state and row.
        m.undo()
        #expect(m.focusedEdit == full && m.focusedHistory.current === m.focusedHistory.top)
        m.redo()
        #expect(m.focusedEdit == exposure.edit && m.focusedHistory.current === exposure)
        // A new edit branches: Contrast and Saturation go.
        m.updateEdit(label: "Highlights") { $0.hdr.highlights = -30 }
        #expect(m.focusedHistory.rows.map(\.label) == ["Highlights", "Exposure", "Original"])
        // Undoing past the branch brings them back, exactly as they were.
        m.undo()
        m.undo()
        #expect(m.focusedEdit == full && m.focusedHistory.rows.map(\.label) == ["Saturation", "Contrast", "Exposure", "Original"])
        // Jumping to the row it's already at changes nothing (and keeps redo).
        let steps = m.history.undoStack.count
        #expect(m.history.redoStack.count == 2)
        m.jumpToHistory(try #require(m.focusedHistory.top))
        #expect(m.history.undoStack.count == steps && m.history.redoStack.count == 2)
        // A row from another photo isn't this photo's.
        let other = HistoryNode(label: "x", state: HistoryState(edit: EditState()), time: Date(), parent: nil)
        m.jumpToHistory(other)
        #expect(m.focusedEdit == full && m.history.undoStack.count == steps && m.history.canRedo)
    }

    /// Review H1: a jump restores the AI Optimize report and highlights with the edit.
    @Test func jumpRestoresTheOptimizeReportAndHighlights() async throws {
        let m = await optimizeModel()
        let f = try #require(m.focusedID)
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 0.2 }
        m.beginOptimize()
        await m.optimizePanel?.wait()
        m.optimizePanel?.apply()
        let report = try #require(m.optimizeReports[f])
        let marks = try #require(m.aiMarks[f])
        let optimized = try #require(m.focusedHistory.top)
        #expect(optimized.state.report == report && optimized.state.marks == marks)
        let exposure = try #require(m.focusedHistory.rows.first { $0.label == "Exposure" })
        m.jumpToHistory(exposure)
        #expect(m.optimizeReports[f] == nil && m.aiMarks[f] == nil && m.focusedEdit.exposure.ev == 0.2)
        m.jumpToHistory(optimized)
        #expect(m.optimizeReports[f] == report && m.aiMarks[f] == marks)
        m.undo()
        #expect(m.optimizeReports[f] == nil && m.aiMarks[f] == nil, "⌘Z of the jump forward")
        m.undo()
        #expect(m.optimizeReports[f] == report && m.aiMarks[f] == marks, "⌘Z of the jump back")
    }

    /// Review M1: a burst that moves to another photo doesn't merge into that photo's
    /// older row with the same control.
    @Test func aRowMergesOnlyWhenItsPhotoIsInTheOpenStep() async throws {
        let m = makeModel()
        await m.start()
        let a = try #require(m.focusedID)
        m.perform(.move(.right, extend: false))
        let b = try #require(m.focusedID)
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.5 }
        m.endEditGesture()
        m.focus(a, extend: false)
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.1 } // gesture still open
        m.focus(b, extend: false)
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.7 }
        m.endEditGesture()
        #expect(m.photoHistories[b]?.rows.map(\.edit.exposure.ev) == [0.7, 0.5, 0])
        // Edit All Selected: a photo that first changes mid-drag gets its own row.
        m.editAllSelected = true
        m.selection = [a, b]
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.7 } // b unchanged, a changes
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.9 } // b joins mid-drag
        m.endEditGesture()
        #expect(m.photoHistories[b]?.rows.map(\.edit.exposure.ev) == [0.9, 0.7, 0.5, 0])
        #expect(m.photoHistories[a]?.rows.map(\.edit.exposure.ev) == [0.9, 0.1, 0])
    }

    @Test func editAllSelectedAddsARowToEachPhotoAndUndoesTogether() async {
        let m = makeModel()
        await m.start()
        m.perform(.move(.right, extend: true))
        m.editAllSelected = true
        let ids = m.selectedIDs
        #expect(ids.count == 2)
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.4 }
        m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = 0.6 }
        m.endEditGesture()
        for id in ids {
            #expect(m.photoHistories[id]?.rows.map(\.label) == ["Exposure", "Original"])
            #expect(m.photoHistories[id]?.top?.edit.exposure.ev == 0.6)
        }
        m.undo()
        #expect(ids.allSatisfy { m.photoHistories[$0] == nil }, "one ⌘Z, every photo's row")
        m.redo()
        #expect(ids.allSatisfy { m.photoHistories[$0]?.rows.count == 2 })
        // A jump moves only the focused photo.
        let start = m.focusedHistory.rows[1]
        m.jumpToHistory(start)
        let others = ids.filter { $0 != m.focusedID }
        #expect(m.focusedEdit.isDefault && others.allSatisfy { m.edit(for: $0).exposure.ev == 0.6 })
    }

    /// Review M3: Clear History isn't an undo step; it leaves ⌘Z and ⇧⌘Z alone and they
    /// never bring the rows back.
    @Test func clearHistoryKeepsTheEditAndTheUndoStacks() async throws {
        let m = makeModel()
        await m.start()
        m.clearHistory()
        #expect(m.focusedHistory.isEmpty && !m.history.canUndo, "nothing to clear")
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        m.updateEdit(label: "Contrast") { $0.exposure.contrast = 5 }
        m.updateEdit(label: "Saturation") { $0.exposure.saturation = 5 }
        m.undo()
        let (undo, redo) = (m.history.undoStack.count, m.history.redoStack.count)
        m.clearHistory()
        #expect(m.focusedHistory.rows.map(\.label) == ["Current edit"] && m.focusedEdit.exposure.contrast == 5)
        #expect(m.history.undoStack.count == undo && m.history.redoStack.count == redo && m.history.undoLabel == "Contrast")
        m.clearHistory()
        #expect(m.history.undoStack.count == undo, "a cleared list clears to itself")
        m.undo()
        #expect(m.focusedEdit.exposure.contrast == 0)
        #expect(m.focusedHistory.rows.map(\.label) == ["Current edit"] && m.focusedHistory.top?.edit == m.focusedEdit,
                "⌘Z doesn't bring the cleared rows back; the row follows the edit")
        m.redo()
        m.redo()
        #expect(m.focusedEdit.exposure.saturation == 5 && m.focusedHistory.top?.edit == m.focusedEdit)
        m.updateEdit(label: "Highlights") { $0.hdr.highlights = -10 }
        #expect(m.focusedHistory.rows.map(\.label) == ["Highlights", "Current edit"])
        // A photo with no rows has nothing to clear.
        m.focus(m.visibleIDs[3], extend: false)
        m.clearHistory()
        #expect(m.focusedHistory.isEmpty)
    }

    @Test func hoverPreviewsARowInTheViewer() async throws {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        let original = try #require(m.focusedHistory.rows.last)
        m.previewHistory(original)
        #expect(m.viewer.previewEdit == EditState() && m.focusedEdit.exposure.ev == 1, "hover doesn't edit")
        #expect(m.historyPreview === original)
        m.previewHistory(nil)
        #expect(m.viewer.previewEdit == nil && m.historyPreview == nil)
        // The current row's state is already shown: no preview render.
        m.previewHistory(try #require(m.focusedHistory.top))
        #expect(m.viewer.previewEdit == nil)
    }

    /// Review M2: the previewed row leaving the list (⌘Z, a branch) ends the preview.
    @Test func aPreviewEndsWhenItsRowGoes() async throws {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        m.updateEdit(label: "Contrast") { $0.exposure.contrast = 10 }
        let exposure = try #require(m.focusedHistory.rows.first { $0.label == "Exposure" })
        let contrast = try #require(m.focusedHistory.top)
        m.previewHistory(exposure)
        #expect(m.viewer.previewEdit == exposure.edit)
        m.undo() // Contrast goes; the previewed Exposure row stays
        #expect(m.historyPreview === exposure)
        m.redo()
        m.jumpToHistory(exposure)
        m.previewHistory(contrast) // a dimmed row
        #expect(m.viewer.previewEdit == contrast.edit)
        m.updateEdit(label: "Highlights") { $0.hdr.highlights = -10 } // the branch drops it
        #expect(m.historyPreview == nil && m.viewer.previewEdit == nil)
        m.previewHistory(exposure)
        #expect(m.viewer.previewEdit == exposure.edit)
        m.clearHistory()
        #expect(m.historyPreview == nil && m.viewer.previewEdit == nil)
    }

    @Test func showHistoryRevealsThePanelOnItsTab() async {
        let m = makeModel()
        await m.start()
        let l = ToolLayout(defaults: testDefaults())
        #expect(ToolPanelID.history.homeTab == .color && ToolTab.favorites.panels.last == .history)
        #expect(ToolWorkspace.defaultCollapsed.contains(.history))
        #expect(CommandCatalog.isEnabled(.showHistory, model: m, layout: l))
        #expect(CommandCatalog.title(.showHistory, model: m) == "Show History" && MenuCommand.showHistory.shortcut == nil)
        m.collapsedPanels.insert(.history)
        CommandCatalog.perform(.showHistory, model: m, layout: l)
        #expect(l.panels(for: m.toolTab).contains(.history) && !m.collapsedPanels.contains(.history))
    }

    @Test func rowsLastForTheSessionOnly() async throws {
        let m = makeModel()
        await m.start()
        let f = try #require(m.focusedID)
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        m.photoHistories[PhotoID(999_999)] = PhotoHistory.cleared(HistoryState(edit: EditState()))
        await m.refreshCurrentSession(keepingHistory: true)
        #expect(m.photoHistories[f]?.rows.count == 2, "file moves keep the rows, like ⌘Z")
        #expect(m.photoHistories[PhotoID(999_999)] == nil, "a photo that left takes its rows")
        await m.refreshCurrentSession()
        #expect(m.photoHistories.isEmpty, "opening a session starts fresh, like ⌘Z")
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 2 }
        m.clearSessionState()
        #expect(m.photoHistories.isEmpty)
    }
}
