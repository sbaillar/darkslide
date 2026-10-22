import Foundation

/// [history] What a History row restores: the photo's edit with its AI Optimize report
/// and AI highlights (review H1: a jump must not leave a report claiming undone values).
public struct HistoryState: Equatable, Sendable {
    public var edit: EditState
    public var report: OptimizeReport?
    public var marks: AIMarks?

    public init(edit: EditState, report: OptimizeReport? = nil, marks: AIMarks? = nil) {
        self.edit = edit
        self.report = report
        self.marks = marks
    }
}

/// [history] One row of a photo's History panel: the state right after a step.
/// Rows are immutable and chained to the row before them, so a photo's history is
/// two pointers and an undo step can keep its before/after for many photos cheaply.
public final class HistoryNode: Identifiable, Sendable {
    public let id: UUID
    public let label: String
    public let state: HistoryState
    public let time: Date
    let coalesceKey: String?
    let parent: HistoryNode?
    /// Rows below this one in its chain (the oldest row is 0).
    let depth: Int
    /// The photo's starting row ("Original", "Start", "Current edit").
    let isBase: Bool

    public var edit: EditState { state.edit }

    init(id: UUID = UUID(), label: String, state: HistoryState, time: Date, coalesceKey: String? = nil,
         parent: HistoryNode?, isBase: Bool = false) {
        self.id = id
        self.isBase = isBase
        self.label = label
        self.state = state
        self.time = time
        self.coalesceKey = coalesceKey
        self.parent = parent
        depth = (parent?.depth ?? -1) + 1
    }
}

/// [history] A photo's edit history, like Lightroom's History panel. `top` is the
/// newest row; `current` is the row the photo is at: `top`, or an older row after a
/// jump back. Rows above `current` stay listed until a new edit branches off them.
public struct PhotoHistory: Equatable, Sendable {
    public private(set) var top: HistoryNode?
    public private(set) var current: HistoryNode?

    /// Rows listed (and kept reachable) per photo.
    public static let limit = 100

    public init() {}

    /// Cleared: one "Current edit" row for `state`; the next edit adds rows above it.
    public static func cleared(_ state: HistoryState) -> PhotoHistory {
        var h = PhotoHistory()
        h.top = HistoryNode(label: "Current edit", state: state, time: Date(), parent: nil, isBase: true)
        h.current = h.top
        return h
    }

    public static func == (a: PhotoHistory, b: PhotoHistory) -> Bool { a.top === b.top && a.current === b.current }

    public var isEmpty: Bool { top == nil }

    /// Newest first, at most `limit` rows.
    public var rows: [HistoryNode] {
        var out: [HistoryNode] = []
        var n = top
        while let node = n, out.count < Self.limit {
            out.append(node)
            n = node.parent
        }
        return out
    }

    /// Older steps than `rows` lists were dropped (L4: the panel says so).
    public var isTrimmed: Bool { rows.last.map { !$0.isBase } ?? false }

    /// A row listed above `current`: undone by a jump, gone at the next edit.
    public func isAfterCurrent(_ node: HistoryNode) -> Bool {
        guard let current else { return false }
        return node.depth > current.depth
    }

    /// The photo changed from `before` to `after` in a step labelled `label`. The first
    /// change adds a starting row for `before`. `merging`: this photo is in the undo step
    /// still open (a slider drag going on), so the step replaces the newest row instead of
    /// adding one. Rows above `current` are dropped (the edit branches off).
    public mutating func record(label: String, before: HistoryState, after: HistoryState, coalesceKey: String?,
                                merging: Bool, time: Date = Date()) {
        if merging, let c = current, c === top, coalesceKey != nil, c.coalesceKey == coalesceKey, let parent = c.parent {
            // Same id: the panel keeps the row instead of rebuilding it every tick (L2).
            let node = HistoryNode(id: c.id, label: c.label, state: after, time: time, coalesceKey: coalesceKey, parent: parent)
            top = node
            current = node
            return
        }
        let base = current
            ?? HistoryNode(label: before.edit.isDefault ? "Original" : "Start", state: before, time: time, parent: nil,
                           isBase: true)
        var node = HistoryNode(label: label, state: after, time: time, coalesceKey: coalesceKey, parent: base)
        // Bounded: past twice the limit, copy the newest `limit` rows into a fresh chain so
        // older rows can be freed (steps still holding them go with the 200-step undo limit).
        if node.depth >= 2 * Self.limit { node = Self.rechained(node) }
        top = node
        current = node
    }

    /// Jump to `node` (a listed row); rows above it stay listed.
    public mutating func jump(to node: HistoryNode) { current = node }

    /// The newest `limit` rows of `node`'s chain, re-linked from depth 0 (ids kept).
    private static func rechained(_ node: HistoryNode) -> HistoryNode {
        var chain: [HistoryNode] = []
        var n: HistoryNode? = node
        while let x = n, chain.count < limit {
            chain.append(x)
            n = x.parent
        }
        var parent: HistoryNode?
        for x in chain.reversed() {
            parent = HistoryNode(id: x.id, label: x.label, state: x.state, time: x.time, coalesceKey: x.coalesceKey,
                                 parent: parent, isBase: x.isBase)
        }
        return parent ?? node
    }
}

/// A photo's history across one undo step (undo restores `before`, redo `after`).
public struct HistoryChange: Equatable, Sendable {
    public var before: PhotoHistory
    public var after: PhotoHistory

    public init(before: PhotoHistory, after: PhotoHistory) {
        self.before = before
        self.after = after
    }
}
