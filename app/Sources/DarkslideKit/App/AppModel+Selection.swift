import CoreGraphics
import Foundation
import Observation

extension AppModel {
    // MARK: - Focus and selection

    /// Focus `id`; with `extend`, select the range from the anchor.
    public func focus(_ id: PhotoID?, extend: Bool) {
        noteUserActivity()
        guard let id else {
            focusedID = nil
            viewer.focusChanged()
            return
        }
        if extend, let anchor = anchorID, let a = visibleIndex(anchor), let b = visibleIndex(id) {
            selection = Set(visibleIDs[min(a, b)...max(a, b)])
        } else {
            anchorID = id
            selection = [id]
        }
        let changed = focusedID != id
        focusedID = id
        if changed {
            viewer.focusChanged()
            Task { await DenoiseService.shared.checkFocused() } // AI Denoise: re-run if its result was dropped
        }
    }

    /// [multiview] Focus another photo of the selection, keeping the selection (a click
    /// on a multi-view tile).
    public func focusWithinSelection(_ id: PhotoID) {
        guard selection.contains(id), focusedID != id else { return }
        noteUserActivity()
        focusedID = id
        viewer.focusChanged()
        Task { await DenoiseService.shared.checkFocused() }
    }

    /// Cmd-click toggling in the browser.
    public func toggleSelection(_ id: PhotoID) {
        if selection.contains(id), selection.count > 1 {
            selection.remove(id)
        } else {
            selection.insert(id)
            anchorID = id
            if focusedID != id {
                focusedID = id
                viewer.focusChanged()
            }
        }
    }

    /// IDs the arrow keys walk: collapsed stacks count once.
    var navigationIDs: [PhotoID] {
        let b = browserState
        guard b.stackBursts else { return visibleIDs }
        b.refreshIfNeeded(version: photosVersion) { photos } // O(1) when nothing changed
        return b.displayIDs(visibleIDs, lookup: { [weak self] in self?.photo($0) })
    }

    /// Position of `id` in the navigation list; a hidden stack member maps to the
    /// entry that represents its stack, anything else to the nearest preceding entry.
    func navIndex(of id: PhotoID?, in nav: [PhotoID]) -> Int? {
        guard let id else { return nil }
        if let i = nav.firstIndex(of: id) { return i }
        if let stack = browserState.stacks.stack(of: id),
           let i = nav.firstIndex(where: { stack.memberIDs.contains($0) }) { return i }
        guard let v = visibleIndex(id) else { return nil }
        return nav.lastIndex { (visibleIndex($0) ?? Int.max) <= v } ?? 0
    }

    /// With stacks on, keep focus on something the browser shows (after a collapse
    /// or when a collapsed stack's top frame changed).
    func refocusIntoNavigation() {
        guard browserState.stackBursts, let f = focusedID else { return }
        let nav = navigationIDs
        guard !nav.contains(f), let i = navIndex(of: f, in: nav) else { return }
        focus(nav[i], extend: false)
    }

    func move(_ dir: KeyCommand.Direction, extend: Bool) {
        let nav = browserState.stackBursts ? navigationIDs : visibleIDs
        guard !nav.isEmpty else { return }
        if nav.count != visibleIDs.count {
            let step = (dir == .left || dir == .up) ? -1 : 1
            guard let cur = navIndex(of: focusedID, in: nav) else {
                focus(nav[0], extend: extend)
                return
            }
            let next = min(max(cur + step, 0), nav.count - 1)
            focus(nav[next], extend: extend)
            return
        }
        let current = focusedID.flatMap { visibleIndex($0) } ?? -1
        let delta: Int
        switch dir {
        case .left: delta = -1
        case .right: delta = 1
        case .up: delta = -verticalStep
        case .down: delta = verticalStep
        }
        let next = current < 0 ? 0 : min(max(current + delta, 0), visibleIDs.count - 1)
        focus(visibleIDs[next], extend: extend)
    }
}
