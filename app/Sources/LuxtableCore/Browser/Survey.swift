import Foundation
import Observation

/// N-up survey (final-journeys gap 14): choose the best of a burst by
/// elimination. The candidates are shown side by side; X takes one out, ←/→
/// move between them, U puts the last one back, Return keeps what is left (they
/// are picked and selected, one undo step) and Esc leaves without changes.
/// The rating, color tag and pick keys rate the focused candidate (N9).
/// A burst longer than a page is paged, never cut short (N9): ←/→ run on to the
/// next page, and the header says "12 of 40".
@MainActor @Observable
public final class SurveyModel {
    /// Most candidates shown at once (a 4×3 grid at most).
    public static let pageSize = 12

    public private(set) var candidates: [PhotoID] = []
    public private(set) var eliminated: [PhotoID] = []
    public private(set) var focused: PhotoID?

    public init() {}

    public var isActive: Bool { !candidates.isEmpty }

    /// The page the focused candidate is on (0-based).
    public var page: Int {
        guard let f = focused, let i = candidates.firstIndex(of: f) else { return 0 }
        return i / Self.pageSize
    }

    public var pageCount: Int { max(1, (candidates.count + Self.pageSize - 1) / Self.pageSize) }

    /// The candidates shown now (the focused one's page).
    public var visible: [PhotoID] {
        let start = min(page * Self.pageSize, candidates.count)
        return Array(candidates[start..<min(start + Self.pageSize, candidates.count)])
    }

    /// "12 of 40" (shown of remaining), or "5" when everything fits.
    public var countLabel: String {
        pageCount > 1 ? "\(visible.count) of \(candidates.count)" : "\(candidates.count)"
    }

    /// Focus the first candidate of the page `delta` pages away (clamped).
    public func turnPage(by delta: Int) {
        let p = min(max(page + delta, 0), pageCount - 1)
        guard p != page else { return }
        focused = candidates[p * Self.pageSize]
    }

    /// Grid columns for `n` candidates: 2 → 2, 3–4 → 2, 5–6 → 3, 7–9 → 3, more → 4.
    public static func columns(for n: Int) -> Int {
        switch n {
        case ...4: min(max(n, 1), 2)
        case 5...9: 3
        default: 4
        }
    }

    /// Start with `ids` (at least two; all of them, `pageSize` at a time).
    /// Returns false when there's nothing to compare.
    @discardableResult
    public func begin(_ ids: [PhotoID]) -> Bool {
        var seen: Set<PhotoID> = []
        let unique = ids.filter { seen.insert($0).inserted }
        guard unique.count >= 2 else { return false }
        candidates = unique
        eliminated = []
        focused = candidates.first
        return true
    }

    public func focus(_ id: PhotoID) {
        if candidates.contains(id) { focused = id }
    }

    /// Take `id` (default: the focused one) out; the last one can't go.
    public func eliminate(_ id: PhotoID? = nil) {
        guard candidates.count > 1, let id = id ?? focused, let i = candidates.firstIndex(of: id) else { return }
        candidates.remove(at: i)
        eliminated.append(id)
        if focused == id { focused = candidates[min(i, candidates.count - 1)] }
    }

    /// Put the last eliminated one back where the order had it.
    public func restoreLast() {
        guard let id = eliminated.popLast() else { return }
        candidates.append(id)
        focused = id
    }

    public func move(by delta: Int) {
        guard let f = focused, let i = candidates.firstIndex(of: f), !candidates.isEmpty else { return }
        focused = candidates[(i + delta + candidates.count) % candidates.count]
    }

    /// End the survey: the remaining candidates (the "keepers").
    @discardableResult
    public func finish() -> [PhotoID] {
        let kept = candidates
        candidates = []
        eliminated = []
        focused = nil
        return kept
    }

    public enum Action: Equatable, Sendable {
        case keep, cancel, handled
        /// A rating, color tag or flag key for the focused candidate (N9).
        case mark(KeyCommand)
    }

    /// Survey keys; `nil` when the key isn't the survey's (it isn't consumed).
    /// X and U are the survey's own (eliminate, put back); `[` and `]` turn the
    /// page; the user's rating, tag and flag keys become `.mark`.
    public func handle(_ key: KeyInput, bindings: KeyBindings = .defaults) -> Action? {
        guard isActive, !key.command else { return nil }
        switch key.special {
        case .escape: return .cancel
        case .returnKey: return .keep
        case .left, .up:
            move(by: -1)
            return .handled
        case .right, .down, .space:
            move(by: 1)
            return .handled
        default: break
        }
        switch key.characters.lowercased() {
        case "x":
            eliminate()
            return .handled
        case "u":
            restoreLast()
            return .handled
        case "[":
            turnPage(by: -1)
            return .handled
        case "]":
            turnPage(by: 1)
            return .handled
        default:
            if let c = key.characters.first, key.characters.count == 1, let cmd = bindings.command(for: c) {
                switch cmd {
                case .rate, .tag, .pick, .reject, .unflag: return .mark(cmd)
                default: break
                }
            }
            return .handled // every other key is swallowed while surveying
        }
    }
}

extension AppModel {
    /// What a survey starts with: the selection (two or more), else the focused
    /// photo's burst (A9 bursts: frames less than a second apart).
    public var surveyCandidates: [PhotoID] {
        let sel = visibleIDs.filter(selection.contains)
        if sel.count >= 2 { return sel }
        guard let f = focusedID else { return [] }
        if let s = browserState.stacks.stack(of: f), s.count >= 2 { return s.memberIDs }
        let stills = photos.filter { !$0.isVideo && !$0.inTrash }
        let burst = StackBuilder.build(stills).first { $0.memberIDs.contains(f) }
        return burst.map(\.memberIDs) ?? []
    }

    /// Enter survey mode (selection or burst). Says why when there's nothing to compare.
    public func beginSurvey() {
        let burst = visibleIDs.filter(selection.contains).count < 2
        guard survey.begin(surveyCandidates) else {
            show(Toast("Select two or more photos (or a burst) to survey", symbol: "square.grid.2x2"))
            return
        }
        // A burst is surveyed from its chosen cover.
        if burst, let f = focusedID, let d = coverDecision(of: f), d.source == .user { survey.focus(d.cover) }
        if let f = survey.focused { focus(f, extend: false) }
    }

    /// A key while surveying; true when the survey consumed it.
    func handleSurveyKey(_ key: KeyInput) -> Bool {
        guard let action = survey.handle(key, bindings: settings.keyBindings) else { return false }
        switch action {
        case .handled:
            if let f = survey.focused, f != focusedID { focus(f, extend: false) }
        case .mark(let cmd):
            if let f = survey.focused { applySurvey(cmd, to: f) }
        case .cancel:
            survey.finish()
        case .keep:
            keepSurvey()
        }
        return true
    }

    /// A rating, tag or flag key on the focused candidate only (not the whole
    /// selection the survey started from); no auto-advance (N9).
    private func applySurvey(_ cmd: KeyCommand, to id: PhotoID) {
        switch cmd {
        case .rate(let n): rate(id, n)
        case .tag(let t):
            let value: ColorTag = photo(id)?.colorTag == t ? .none : t
            updatePhotos([id], label: "Color Tag \(value.name)") { $0.colorTag = value }
        case .pick: updatePhotos([id], label: "Pick") { $0.pick = .picked }
        case .reject: updatePhotos([id], label: "Reject") { $0.pick = .rejected }
        case .unflag: updatePhotos([id], label: "Unflag") { $0.pick = .none }
        default: break
        }
    }

    /// [cul9] L6: rate one photo from its grid stars (one undo step; focus unchanged).
    public func rate(_ id: PhotoID, _ rating: Int) {
        let r = min(max(rating, 0), 5)
        updatePhotos([id], label: r == 0 ? "Clear Rating" : "Rate \(r)") { $0.rating = r }
    }

    /// Return: the survivors are picked (one undo step) and selected.
    public func keepSurvey() {
        let kept = survey.finish()
        guard !kept.isEmpty else { return }
        updatePhotos(kept, label: kept.count == 1 ? "Keep Best" : "Keep \(kept.count)") { $0.pick = .picked }
        focus(kept[0], extend: false)
        selection = kept.count > 1 ? Set(kept) : []
    }
}
