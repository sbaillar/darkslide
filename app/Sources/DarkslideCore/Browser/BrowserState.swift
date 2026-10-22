import CoreGraphics
import Foundation
import Observation

/// Browser-only UI state that outlives the grid/filmstrip views (they are
/// swapped when the mode changes): burst stacks and whether stacking is on.
@MainActor @Observable
public final class BrowserState {
    public let stacks = StackState()
    /// How bursts are found: ds-ai grouping in live mode, capture time in demo.
    @ObservationIgnored public let grouping: any StackGrouping
    /// Preferred stack tops (ds-ai's best frames from the last AI Cull).
    @ObservationIgnored public var bestHints: Set<PhotoID> = [] {
        didSet { if bestHints != oldValue { builtFrom = []; builtVersion = nil } }
    }
    /// Show bursts as collapsed stacks.
    public var stackBursts = false {
        didSet { stackBursts ? stacks.collapseAll() : stacks.expandAll() }
    }
    /// Signature of the photo list the stacks were built from.
    @ObservationIgnored private var builtFrom: [PhotoID] = []
    @ObservationIgnored private var builtVersion: Int?
    /// Number of stack rebuilds (tests assert refreshes stay O(1) per keypress).
    @ObservationIgnored public private(set) var rebuilds = 0

    public init(grouping: any StackGrouping = CaptureTimeGrouping()) {
        self.grouping = grouping
    }

    /// Another session opened: stacks and the AI's preferred tops belong to the old one, even
    /// when the new list has the same ids in the same order.
    public func reset() {
        bestHints = []
        builtFrom = []
        builtVersion = nil
        stacks.expandAll()
        stacks.update(photos: [], grouping: grouping, best: [])
    }

    /// Rebuild stacks when the photo list changed (ids or order). O(n); call on
    /// load/filter changes, not per keypress.
    public func refresh(photos: [Photo]) {
        let ids = photos.map(\.id)
        guard ids != builtFrom else { return }
        rebuild(photos, ids: ids)
    }

    /// O(1) when `version` (the caller's photo-list version, e.g.
    /// `AppModel.photosVersion`) is unchanged; otherwise compares ids and
    /// rebuilds only if they changed. `photos` is evaluated only when needed.
    public func refreshIfNeeded(version: Int, photos: () -> [Photo]) {
        guard version != builtVersion else { return }
        builtVersion = version
        let list = photos()
        let ids = list.map(\.id)
        guard ids != builtFrom else { return }
        rebuild(list, ids: ids)
    }

    private func rebuild(_ photos: [Photo], ids: [PhotoID]) {
        builtFrom = ids
        rebuilds += 1
        stacks.update(photos: photos, grouping: grouping, best: bestHints)
        if stackBursts { stacks.collapseAll() }
    }

    /// Ids to lay out, with collapsed stacks folded to one cell.
    public func displayIDs(_ visibleIDs: [PhotoID], lookup: ((PhotoID) -> Photo?)? = nil) -> [PhotoID] {
        stacks.displayIDs(from: visibleIDs, lookup: lookup)
    }
}

extension AppModel {
    /// Browser UI state for this model (lives as long as the model).
    public var browserState: BrowserState { BrowserStateRegistry.state(for: self) }
}

@MainActor
enum BrowserStateRegistry {
    private static let table = NSMapTable<AppModel, BrowserState>.weakToStrongObjects()

    static func state(for model: AppModel) -> BrowserState {
        if let s = table.object(forKey: model) { return s }
        // Live: ds-ai burst grouping (shot numbers resolve whole-second stamps).
        let s = BrowserState(grouping: model.mode == .live ? BurstGrouping() : CaptureTimeGrouping())
        table.setObject(s, forKey: model)
        return s
    }
}

/// Thumbnail placement inside a cell (pure, so portrait sizing is testable).
public enum ThumbnailLayout {
    /// Largest rect with `aspect` (w/h) inside `box` inset by `inset`, centered.
    /// Portraits fill the box height; landscapes fill its width.
    public static func fit(aspect: Double, in box: CGSize, inset: CGFloat = 0) -> CGRect {
        let w = max(0, box.width - 2 * inset), h = max(0, box.height - 2 * inset)
        guard w > 0, h > 0 else { return .zero }
        let a = aspect.isFinite && aspect > 0 ? CGFloat(aspect) : 1.5
        let size = w / h > a ? CGSize(width: h * a, height: h) : CGSize(width: w, height: w / a)
        return CGRect(x: (box.width - size.width) / 2, y: (box.height - size.height) / 2,
                      width: size.width, height: size.height)
    }

    /// Filmstrip cell width for a strip of height `h`: square-ish so portraits aren't tiny.
    public static func filmstripCellWidth(height h: CGFloat) -> CGFloat { (h * 1.15).rounded() }
}
