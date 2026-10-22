import CoreGraphics
import Foundation
import Observation

/// The Remove tool's brush (Heal & Clone panel): paint over a distraction, release, and
/// the stroke becomes one `RemovalEdit` that Rust fills.
@MainActor @Observable
public final class RemoveToolState {
    /// Brush diameter relative to the long edge.
    public var size = 0.04
    /// Soft edge of new removals (long-edge units).
    public var feather = RemovalEdit.defaultFeather
    /// The stroke being painted: its photo and points (oriented, normalized); observed,
    /// so the canvas redraws its preview.
    var stroke: (photo: PhotoID, points: [CGPoint])?
    /// Called after a removal is added (downloads LaMa on first use). Seam for tests.
    @ObservationIgnored public var onRemove: @MainActor () -> Void = { RemovalModel.shared.noteRemoveUsed() }

    public nonisolated static let sizeRange = 0.005...0.3

    public init() {}

    /// Minimum spacing between recorded points (normalized).
    static let spacing: CGFloat = 0.002

    /// The removal a finished stroke makes (nil for an empty stroke).
    func removal(points: [CGPoint]) -> RemovalEdit? {
        guard !points.isEmpty else { return nil }
        let stroke = BrushStroke(points: points, size: size, hardness: 1, flow: 1)
        return RemovalEdit(strokes: [stroke], feather: feather)
    }
}

extension ViewerModel {
    /// Remove tool press at viewer-normalized `u` on `photo`.
    func removeBegin(photo: PhotoID, at u: CGPoint) {
        app?.colorTools.removal.stroke = (photo, [oriented(u)])
    }

    /// Pointer moved during a Remove stroke (pane point `p`); false when no stroke runs.
    func removeMove(to p: CGPoint) -> Bool {
        guard let tool = app?.colorTools.removal, var s = tool.stroke, let pane = layout.main else { return false }
        let o = oriented(pane.normalized(p))
        if let last = s.points.last, hypot(last.x - o.x, last.y - o.y) < RemoveToolState.spacing { return true }
        s.points.append(o)
        tool.stroke = s
        return true
    }

    /// The Remove stroke ended: one removal (one undo step) on the photo it started on.
    func removeEnd() -> Bool {
        guard let app, let s = app.colorTools.removal.stroke else { return false }
        let tool = app.colorTools.removal
        tool.stroke = nil
        guard s.photo == app.focusedID, let r = tool.removal(points: s.points) else { return true }
        app.updateEdits([s.photo], label: "Remove") { _, e in e.removals.append(r) }
        tool.onRemove()
        return true
    }

    /// The stroke in progress, for the canvas preview (oriented).
    var removeOverlaySegments: [OverlaySegment] {
        guard let tool = app?.colorTools.removal, let s = tool.stroke, s.photo == app?.focusedID else { return [] }
        return OverlaySegment.segments(s.points, radius: Float(tool.size / 2), erase: false)
    }
}

extension AppModel {
    /// Undo-able "remove the last removal" of the focused photo.
    public func removeLastRemoval() {
        updateEdit(label: "Undo Removal") { e in _ = e.removals.popLast() }
    }
}
