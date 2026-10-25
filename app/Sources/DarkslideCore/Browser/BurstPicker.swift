import CoreGraphics
import Foundation
import Observation

/// The burst strip above the filmstrip: every frame of one collapsed burst, so the
/// user can look through them and choose the stack's cover. ←/→ walk the frames,
/// ⏎ uses the current one as the cover, Esc closes (`AppModel.handleBurstStripKey`);
/// every other key (ratings, flags) still goes to the shown photo.
@MainActor @Observable
public final class BurstPicker {
    /// The open stack (`PhotoStack.id`); nil: closed.
    public private(set) var stackID: PhotoID?
    /// Its frames, in capture order.
    public private(set) var frames: [PhotoID] = []
    /// The frame shown in the viewer (arrows move it).
    public private(set) var current: PhotoID?
    /// The latest cover set from the strip and how many have been set (the strip's
    /// confirming animation runs when this changes).
    public private(set) var confirmedCover: PhotoID?
    public private(set) var confirmations = 0
    /// Where the stack's cell was (window coordinates) when the strip opened: the frames
    /// fan out from there. Nil: from the strip's middle.
    public private(set) var anchor: CGRect?

    public init() {}

    public var isOpen: Bool { stackID != nil }

    /// Open on `stack`, at `focused` when it is one of its frames, else at its cover.
    public func open(_ stack: PhotoStack, focused: PhotoID?, anchor: CGRect? = nil) {
        stackID = stack.id
        self.anchor = anchor
        frames = stack.memberIDs
        current = focused.flatMap { stack.memberIDs.contains($0) ? $0 : nil } ?? stack.topID
        confirmedCover = nil
    }

    public func close() {
        stackID = nil
        frames = []
        current = nil
        confirmedCover = nil
    }

    /// Make `id` (one of the frames) current.
    public func select(_ id: PhotoID) {
        if frames.contains(id) { current = id }
    }

    /// Move `delta` frames along the strip as it is shown (`order`: the frames in strip
    /// order, default capture order), clamped; the frame now current.
    @discardableResult
    public func move(_ delta: Int, in order: [PhotoID]? = nil) -> PhotoID? {
        let order = order ?? frames
        guard let c = current, let i = order.firstIndex(of: c) else { return current }
        current = order[min(max(i + delta, 0), order.count - 1)]
        return current
    }

    /// `id` became the cover (from the strip): the confirming animation's cue.
    func noteCover(_ id: PhotoID) {
        confirmedCover = id
        confirmations += 1
    }

    /// The stacks were rebuilt: follow the open stack's new frames, or close when it is gone.
    func sync(_ stack: PhotoStack?) {
        guard isOpen else { return }
        guard let stack, stack.count >= 2 else { return close() }
        stackID = stack.id
        frames = stack.memberIDs
        if let c = current, !frames.contains(c) { current = stack.topID }
    }
}

extension AppModel {
    /// The filmstrip's "Stack bursts" (and View ▸ Stack Bursts): on, bursts collapse to
    /// stacks; off, every frame is shown and walked. Saved in the settings.
    public func setStackBursts(_ on: Bool) {
        let b = browserState
        guard b.stackBursts != on else { return }
        b.stackBursts = on
        if on { b.refresh(photos: visiblePhotos) }
        refocusIntoNavigation()
    }

    /// The burst strip is on screen: it is open and the filmstrip it sits on is shown.
    public var burstStripShown: Bool {
        browserState.picker.isOpen && browserMode == .filmstrip && !filmstripHidden && !panelsHidden
    }

    /// A collapsed stack's badge in the filmstrip: open its burst strip, or close it when it
    /// is the one open. An expanded stack (or the grid) expands / collapses as before.
    public func toggleBurstStrip(for id: PhotoID, anchor: CGRect? = nil) {
        let b = browserState
        guard let s = b.stacks.stack(of: id) else { return }
        if b.picker.stackID == s.id { return b.picker.close() }
        guard browserMode == .filmstrip, b.stacks.isCollapsed(s) else {
            b.stacks.toggleStack(of: id)
            return refocusIntoNavigation()
        }
        b.picker.open(s, focused: focusedID, anchor: anchor)
        showOpenedFrame()
    }

    /// A click on a collapsed stack's cell in the filmstrip: it is focused and its strip opens.
    public func openBurstStrip(for id: PhotoID, anchor: CGRect? = nil) {
        let b = browserState
        guard browserMode == .filmstrip, let s = b.stacks.stack(of: id), b.stacks.isCollapsed(s),
              b.picker.stackID != s.id else { return }
        b.picker.open(s, focused: focusedID, anchor: anchor)
        showOpenedFrame()
    }

    /// The viewer shows the strip's current frame.
    private func showOpenedFrame() {
        if let c = browserState.picker.current, c != focusedID { focus(c, extend: false) }
    }

    public func closeBurstStrip() { browserState.picker.close() }

    /// A frame clicked in the strip: shown in the viewer.
    public func showInBurstStrip(_ id: PhotoID) {
        browserState.picker.select(id)
        focus(id, extend: false)
    }

    /// Why the stack holding `id` shows its cover, from current ratings (nil: not in a burst).
    public func coverDecision(of id: PhotoID) -> CoverDecision? {
        let stacks = browserState.stacks
        guard let s = stacks.stack(of: id) else { return nil }
        return stacks.decision(of: s, lookup: { [weak self] in self?.photo($0) })
    }

    /// The open strip's frames as shown: highest rating first, ties in capture order.
    public var burstStripOrder: [PhotoID] {
        CoverRule.stripOrder(browserState.picker.frames.compactMap { photo($0) })
    }

    /// Whether `id` can be its stack's cover: it is tied for the stack's highest rating.
    public func canUseAsCover(_ id: PhotoID) -> Bool {
        coverDecision(of: id)?.candidates.contains(id) ?? false
    }

    /// Why `id` can't be its stack's cover (nil: it can, or it isn't in a burst).
    public func coverHint(for id: PhotoID) -> String? {
        guard let d = coverDecision(of: id), !d.candidates.contains(id) else { return nil }
        if photo(id)?.pick == .rejected { return "Unreject it to make it the cover" }
        return "Rate it \(String(repeating: "★", count: d.topRating)) or higher to make it the cover"
    }

    /// Use `id` (default: the strip's current frame, else the focused photo) as its stack's
    /// cover, when it is tied for the stack's highest rating (otherwise the hint says what
    /// it takes). The choice is saved with the session, so every Mac opening it shows the
    /// same cover; a frame rated higher later takes over, and the choice is back on a tie.
    public func useAsCover(_ id: PhotoID? = nil) {
        let b = browserState
        guard let id = id ?? b.picker.current ?? focusedID, b.stacks.stack(of: id) != nil else { return }
        if let hint = coverHint(for: id) {
            show(Toast(hint, symbol: "star"))
            return
        }
        guard let covers = b.chooseCover(id) else { return }
        if b.picker.isOpen {
            b.picker.select(id)
            b.picker.noteCover(id)
        }
        focus(id, extend: false)
        guard let session = currentSession else { return }
        let previous = b.coverSave
        let store = sessions
        b.coverSave = Task { [weak self] in
            await previous?.value // saves land in the order they were made
            do {
                try await store.saveStackCovers(covers, in: session)
            } catch {
                self?.show(error)
            }
        }
    }

    /// While the strip is shown it owns ←/→ (walk its frames as shown), ⏎ (Use as Cover)
    /// and Esc (close). Every other key, ratings and flags included, goes on to the shown photo.
    func handleBurstStripKey(_ key: KeyInput) -> Bool {
        let p = browserState.picker
        guard p.isOpen, !key.command, !key.option, !key.control, !key.shift else { return false }
        guard burstStripShown else {
            if browserMode != .filmstrip { p.close() } // the grid has no strip
            return false
        }
        switch key.special {
        case .left, .right:
            if let id = p.move(key.special == .left ? -1 : 1, in: burstStripOrder), id != focusedID { focus(id, extend: false) }
        case .returnKey: useAsCover()
        case .escape: p.close()
        default: return false
        }
        return true
    }
}
