import Foundation

/// One undoable step: the before/after of every photo and edit it touched.
/// A paste onto 500 images is still a single step (PLAN §8).
public struct HistoryStep: Equatable, Sendable {
    public var label: String
    public var editsBefore: [PhotoID: EditState] = [:]
    public var editsAfter: [PhotoID: EditState] = [:]
    public var photosBefore: [PhotoID: Photo] = [:]
    public var photosAfter: [PhotoID: Photo] = [:]
    /// AI Optimize reports the step replaced (undo restores `before`, redo `after`), so an
    /// undone optimize leaves no report claiming its values.
    public var reports: [PhotoID: ReportChange] = [:]
    /// The AI highlights the step changed (an AI edit's rows, or rows the user touched).
    public var marks: [PhotoID: MarksChange] = [:]
    /// [history] Each photo's History panel rows before and after the step.
    public var histories: [PhotoID: HistoryChange] = [:]
    /// Changes with the same key merge into one step until the gesture ends (L3).
    public var coalesceKey: String?
    public var time: Date

    public init(label: String, coalesceKey: String? = nil, time: Date = Date()) {
        self.label = label
        self.coalesceKey = coalesceKey
        self.time = time
    }

    public var isEmpty: Bool {
        editsBefore == editsAfter && photosBefore == photosAfter && histories.values.allSatisfy { $0.before == $0.after }
    }
}

/// A photo's optimize report before and after a step (`nil`: none).
public struct ReportChange: Equatable, Sendable {
    public var before: OptimizeReport?
    public var after: OptimizeReport?

    public init(before: OptimizeReport?, after: OptimizeReport?) {
        self.before = before
        self.after = after
    }
}

/// Bounded undo/redo stack.
public struct EditHistory: Sendable {
    /// A gesture is open while updates with the same coalesce key keep arriving;
    /// `endGesture()` (slider/drag release) closes it so the next drag is a new step.
    public private(set) var gestureOpen = false
    public private(set) var undoStack: [HistoryStep] = []
    public private(set) var redoStack: [HistoryStep] = []
    public let limit: Int

    public init(limit: Int = 200) { self.limit = limit }

    public var canUndo: Bool { !undoStack.isEmpty }
    public var canRedo: Bool { !redoStack.isEmpty }
    public var undoLabel: String? { undoStack.last?.label }
    public var redoLabel: String? { redoStack.last?.label }

    /// `record(step)` would merge `step` into the previous step (a gesture going on).
    public func merges(_ step: HistoryStep) -> Bool {
        guard let key = step.coalesceKey, gestureOpen, let last = undoStack.last else { return false }
        return last.coalesceKey == key
    }

    /// Record a step. While a gesture is open, a step with the same coalesce key
    /// merges into the previous one (keeping the oldest "before" and newest "after").
    public mutating func record(_ step: HistoryStep) {
        guard !step.isEmpty else { return }
        redoStack.removeAll()
        if merges(step), var last = undoStack.last {
            last.editsBefore.merge(step.editsBefore) { old, _ in old }
            last.editsAfter.merge(step.editsAfter) { _, new in new }
            last.photosBefore.merge(step.photosBefore) { old, _ in old }
            last.photosAfter.merge(step.photosAfter) { _, new in new }
            for (id, m) in step.marks {
                last.marks[id] = MarksChange(before: last.marks[id]?.before ?? m.before, after: m.after)
            }
            for (id, h) in step.histories {
                last.histories[id] = HistoryChange(before: last.histories[id]?.before ?? h.before, after: h.after)
            }
            last.time = step.time
            undoStack[undoStack.count - 1] = last
            return
        }
        gestureOpen = step.coalesceKey != nil
        undoStack.append(step)
        if undoStack.count > limit { undoStack.removeFirst(undoStack.count - limit) }
    }

    /// The slider/drag was released: the next change starts a new step.
    public mutating func endGesture() { gestureOpen = false }

    /// Pops the step to revert; apply its `…Before` values.
    public mutating func undo() -> HistoryStep? {
        gestureOpen = false
        guard let step = undoStack.popLast() else { return nil }
        redoStack.append(step)
        return step
    }

    /// Pops the step to re-apply; apply its `…After` values.
    public mutating func redo() -> HistoryStep? {
        guard let step = redoStack.popLast() else { return nil }
        undoStack.append(step)
        return step
    }

    /// [history] Clear History (review M3): `id`'s rows are dropped from every step, so no
    /// undo or redo brings them back; the steps themselves (and their edits) stay.
    public mutating func forgetHistories(of id: PhotoID) {
        for i in undoStack.indices { undoStack[i].histories[id] = nil }
        for i in redoStack.indices { redoStack[i].histories[id] = nil }
    }

    /// The photos were listed again after files moved (review M8): steps keep their
    /// culling and edit values, take each photo's file fields (path, folder) from
    /// `current`, and forget photos that left the session (steps left empty go).
    public mutating func rebase(onto current: [PhotoID: Photo]) {
        func fix(_ step: HistoryStep) -> HistoryStep? {
            var s = step
            s.editsBefore = s.editsBefore.filter { current[$0.key] != nil }
            s.editsAfter = s.editsAfter.filter { current[$0.key] != nil }
            s.reports = s.reports.filter { current[$0.key] != nil }
            s.marks = s.marks.filter { current[$0.key] != nil }
            s.histories = s.histories.filter { current[$0.key] != nil }
            s.photosBefore = Self.located(s.photosBefore, current)
            s.photosAfter = Self.located(s.photosAfter, current)
            return s.isEmpty ? nil : s
        }
        undoStack = undoStack.compactMap(fix)
        redoStack = redoStack.compactMap(fix)
    }

    private static func located(_ photos: [PhotoID: Photo], _ current: [PhotoID: Photo]) -> [PhotoID: Photo] {
        photos.compactMapValues { p in
            guard let now = current[p.id] else { return nil }
            var q = p
            q.url = now.url
            q.filename = now.filename
            q.inTrash = now.inTrash
            q.inSelects = now.inSelects
            q.isMissing = now.isMissing
            return q
        }
    }
}
