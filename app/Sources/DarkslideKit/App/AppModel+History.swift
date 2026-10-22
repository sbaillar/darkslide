import Foundation

/// [history] The History panel: the focused photo's edit steps, newest on top. Rows are
/// kept per photo but move with ⌘Z: every edit step records each photo's rows before and
/// after, so undo and redo (and an Edit All Selected step across many photos) restore them.
extension AppModel {
    /// The focused photo's history (empty with nothing focused or nothing edited yet).
    public var focusedHistory: PhotoHistory {
        focusedID.flatMap { photoHistories[$0] } ?? PhotoHistory()
    }

    /// A photo's edit, optimize report and AI highlights now (what a row records).
    func historyState(_ id: PhotoID) -> HistoryState {
        HistoryState(edit: edit(for: id), report: optimizeReports[id], marks: aiMarks[id])
    }

    /// Put the focused photo back to `node`'s state (edit, report, highlights) as one undo
    /// step. Rows above it stay listed until the next edit branches off.
    public func jumpToHistory(_ node: HistoryNode) {
        guard let f = focusedID, let rows = photoHistories[f], rows.rows.contains(where: { $0 === node }) else { return }
        var after = rows
        after.jump(to: node)
        var step = HistoryStep(label: "Go to \(node.label)")
        let now = historyState(f)
        let to = node.state
        if now.edit != to.edit {
            step.editsBefore[f] = now.edit
            step.editsAfter[f] = to.edit
        }
        if now.report != to.report { step.reports[f] = ReportChange(before: now.report, after: to.report) }
        if now.marks != to.marks { step.marks[f] = MarksChange(before: now.marks, after: to.marks) }
        step.histories[f] = HistoryChange(before: rows, after: after)
        commit(step)
    }

    /// Clear History (review M3): the focused photo's rows go and it shows one "Current
    /// edit" row. Not an undo step: ⌘Z and ⇧⌘Z keep their steps, and none brings the rows back.
    public func clearHistory() {
        guard let f = focusedID, let rows = photoHistories[f], rows.rows.count > 1 || rows.current?.isBase != true
        else { return }
        history.forgetHistories(of: f)
        photoHistories[f] = .cleared(historyState(f))
        dropStaleHistoryPreview()
    }

    /// Hover preview: the viewer renders `node`'s state (nil restores), like hovering a style.
    /// The current row's state is already shown: no render (L3).
    public func previewHistory(_ node: HistoryNode?) {
        historyPreview = node
        guard optimizePanel == nil else { return } // AI Optimize owns the preview
        viewer.setPreviewEdit(node.flatMap { n in focusedID.map { edit(for: $0) } == n.edit ? nil : n.edit })
    }

    /// The previewed row left the focused photo's list (⌘Z, a branch, a clear) or the
    /// photo lost focus: stop previewing it (review M2), so the viewer shows the real edit.
    func dropStaleHistoryPreview() {
        guard let p = historyPreview, !focusedHistory.rows.contains(where: { $0.id == p.id }) else { return }
        previewHistory(nil)
    }
}
