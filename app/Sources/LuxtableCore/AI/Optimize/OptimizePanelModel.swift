import Foundation

/// The AI Optimize panel: analyzes one photo (rows to check, live preview) or several
/// (a compact line per photo), then applies what is checked as one undo step.
@MainActor @Observable
public final class OptimizePanelModel {
    public enum State: Equatable, Sendable {
        case analyzing
        case ready(OptimizeReport)
        case failed(String)
    }

    public struct Item: Identifiable, Equatable, Sendable {
        public let id: PhotoID
        public let filename: String
        public var state: State
        /// Batch: whether this photo's changes are applied.
        public var include: Bool

        public var report: OptimizeReport? {
            if case .ready(let r) = state { r } else { nil }
        }
    }

    public private(set) var items: [Item]
    /// Single photo: the checked change ids.
    public private(set) var selected: Set<Int> = []
    /// The paragraph at the top: Rust's template, built from the same facts as the rows.
    public private(set) var summary = ""
    /// Show the edit as it was (before/after toggle of the live preview).
    public var showBefore = false {
        didSet { if showBefore != oldValue { updatePreview() } }
    }
    /// Applied or cancelled: the panel closes.
    public private(set) var isFinished = false

    /// The edits when the panel opened (what the changes apply to).
    public let bases: [PhotoID: EditState]
    @ObservationIgnored private weak var app: AppModel?
    @ObservationIgnored private let optimizer: any PhotoOptimizing
    @ObservationIgnored private var task: Task<Void, Never>?
    /// Photos analyzed at once (each is up to three GPU probes plus a Vision pass).
    static let concurrency = 2

    init(app: AppModel, ids: [PhotoID], optimizer: any PhotoOptimizing) {
        self.app = app
        self.optimizer = optimizer
        items = ids.map { Item(id: $0, filename: app.photo($0)?.filename ?? "", state: .analyzing, include: true) }
        bases = Dictionary(ids.map { ($0, app.edit(for: $0)) }) { a, _ in a }
    }

    public var isBatch: Bool { items.count > 1 }
    public var isAnalyzing: Bool { items.contains { $0.state == .analyzing } }
    /// Single photo: its report once analyzed.
    public var report: OptimizeReport? { isBatch ? nil : items.first?.report }

    /// "Analyzing 3 of 8…" while a batch runs.
    public var progressText: String? {
        guard isAnalyzing else { return nil }
        guard isBatch else { return "Analyzing…" }
        let done = items.count(where: { $0.state != .analyzing })
        return "Analyzing \(min(done + 1, items.count)) of \(items.count)…"
    }

    /// The Apply button's title ("Apply 4 Changes", "Apply to 3 Photos").
    public var applyTitle: String {
        if isBatch {
            let n = applicableItems.count
            return "Apply to \(n) Photo\(n == 1 ? "" : "s")"
        }
        let n = selected.count
        return n == 0 ? "Apply" : "Apply \(n) Change\(n == 1 ? "" : "s")"
    }

    public var canApply: Bool { isBatch ? !applicableItems.isEmpty : !selected.isEmpty && report != nil }

    var applicableItems: [Item] { items.filter { $0.include && !($0.report?.changes.isEmpty ?? true) } }

    // MARK: - Running

    /// Analyze every photo (two at a time), then show the first result.
    func start() {
        let ids = items.map(\.id)
        task = Task { [weak self] in
            guard let me = self else { return }
            await withTaskGroup(of: Void.self) { group in
                var queue = ids[...]
                func add() {
                    guard let id = queue.popFirst() else { return }
                    group.addTask { await me.analyze(id) }
                }
                for _ in 0..<Self.concurrency { add() }
                while await group.next() != nil { add() }
            }
        }
    }

    /// Wait for the analyses (tests).
    func wait() async { await task?.value }

    private func analyze(_ id: PhotoID) async {
        guard let app, let photo = app.photo(id), let base = bases[id] else { return set(id, .failed("Not found")) }
        do {
            let signals = await app.optimizeSignals(photo)
            try Task.checkCancellation()
            let prior = await app.loadOptimizeReport(for: id)
            let r = try await optimizer.analyze(photo, edit: base, signals: signals, prior: prior, masks: app.optimizeMasks)
            set(id, .ready(r))
            if !isBatch {
                selected = r.defaultSelection
                summary = r.summary
                updatePreview()
            }
        } catch is CancellationError {
        } catch {
            set(id, .failed((error as? LocalizedError)?.errorDescription ?? String(describing: error)))
        }
    }

    private func set(_ id: PhotoID, _ state: State) {
        guard !isFinished, let i = items.firstIndex(where: { $0.id == id }) else { return }
        items[i].state = state
    }

    // MARK: - Choosing

    public func isSelected(_ change: OptimizeChange) -> Bool { selected.contains(change.id) }

    /// Check or uncheck a change; the preview follows.
    public func toggle(_ id: Int) {
        guard report?.changes.contains(where: { $0.id == id }) == true else { return }
        if selected.contains(id) { selected.remove(id) } else { selected.insert(id) }
        updatePreview()
    }

    /// Batch: include or leave out one photo.
    public func setInclude(_ id: PhotoID, _ on: Bool) {
        guard let i = items.firstIndex(where: { $0.id == id }) else { return }
        items[i].include = on
    }

    /// The edit the checked changes give (single photo), or nil before the analysis.
    public var previewEdit: EditState? {
        guard let r = report, let id = items.first?.id, let base = bases[id] else { return nil }
        return try? optimizer.apply(r, selected: selected, to: base).edit
    }

    /// Render the checked changes (or the edit before, with `showBefore`) in the viewer.
    private func updatePreview() {
        guard !isFinished, !isBatch, let app, let id = items.first?.id, app.focusedID == id else { return }
        app.viewer.setPreviewEdit(showBefore ? bases[id] : previewEdit)
    }

    // MARK: - Finishing

    /// Apply the checked changes as one undo step, keep each photo's report, and close.
    public func apply() {
        guard let app, canApply, !isFinished else { return }
        var results: [PhotoID: (edit: EditState, report: OptimizeReport)] = [:]
        for item in isBatch ? applicableItems : Array(items.prefix(1)) {
            guard let r = item.report, let base = bases[item.id] else { continue }
            let chosen = isBatch ? r.defaultSelection : selected
            guard !chosen.isEmpty, let out = try? optimizer.apply(r, selected: chosen, to: base) else { continue }
            results[item.id] = out
        }
        finish()
        app.commitOptimize(results, bases: bases)
    }

    /// Close without changing anything (the preview goes back to the stored edit).
    public func cancel() {
        task?.cancel()
        finish()
    }

    private func finish() {
        guard !isFinished else { return }
        if !isBatch, let app, let id = items.first?.id, app.focusedID == id { app.viewer.setPreviewEdit(nil) }
        isFinished = true
    }
}
