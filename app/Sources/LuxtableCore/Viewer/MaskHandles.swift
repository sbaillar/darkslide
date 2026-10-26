import CoreGraphics
import Foundation

/// Editable gradient handles and brush settings for layer masks (Lightroom/C1:
/// gradients stay editable after they're drawn).
public enum MaskHandle: Equatable, Sendable {
    /// Linear gradient: fully on at `start`, fully off at `end`.
    case linearStart, linearEnd
    /// Radial gradient: the centre moves it; the edge handles set the radii.
    case radialCenter, radialEdgeX, radialEdgeY
}

public enum MaskHandles {
    /// Normalized display coordinates → long-edge units, per axis: a radial's radii and
    /// angle live in pixel-proportional space (lt-core `MaskShape::Radial`). `aspect` is
    /// width / height of the displayed image.
    static func longEdgeScale(_ aspect: CGFloat) -> CGSize {
        guard aspect.isFinite, aspect > 0 else { return CGSize(width: 1, height: 1) }
        return aspect >= 1 ? CGSize(width: 1, height: 1 / aspect) : CGSize(width: aspect, height: 1)
    }

    /// Handle positions of `shape` (display space, the shape already converted) on an
    /// image of `aspect` (width / height).
    public static func handles(of shape: MaskShape, aspect: CGFloat) -> [(MaskHandle, CGPoint)] {
        switch shape {
        case .linear(_, let a, let b): [(.linearStart, a), (.linearEnd, b)]
        case .radial(_, let c, let r, let angle, _):
            [(.radialCenter, c), (.radialEdgeX, edge(c, r.width, angle, aspect)),
             (.radialEdgeY, edge(c, r.height, angle + 90, aspect))]
        default: []
        }
    }

    /// The point `r` long edges from `c` in direction `angle` (degrees, clockwise, pixel space).
    static func edge(_ c: CGPoint, _ r: CGFloat, _ angle: Double, _ aspect: CGFloat) -> CGPoint {
        let a = angle * .pi / 180, k = longEdgeScale(aspect)
        return CGPoint(x: c.x + r * CGFloat(cos(a)) / k.width, y: c.y + r * CGFloat(sin(a)) / k.height)
    }

    /// The handle of `shapes` (display space) within `tolerance` of `p`, nearest first.
    public static func hit(_ p: CGPoint, shapes: [MaskShape], aspect: CGFloat, tolerance: CGFloat) -> (index: Int, handle: MaskHandle)? {
        var best: (Int, MaskHandle, CGFloat)?
        for (i, s) in shapes.enumerated() {
            for (h, q) in handles(of: s, aspect: aspect) {
                let d = hypot(q.x - p.x, q.y - p.y)
                if d <= tolerance, best == nil || d < best!.2 { best = (i, h, d) }
            }
        }
        return best.map { ($0.0, $0.1) }
    }

    /// `shape` (display space) with `handle` dragged to `p`, on an image of `aspect`.
    /// Radii and the angle are measured in pixels (long-edge units).
    public static func move(_ shape: MaskShape, handle: MaskHandle, to p: CGPoint, aspect: CGFloat) -> MaskShape {
        let k = longEdgeScale(aspect)
        func offset(_ c: CGPoint) -> (x: CGFloat, y: CGFloat) { ((p.x - c.x) * k.width, (p.y - c.y) * k.height) }
        switch (shape, handle) {
        case (.linear(let op, _, let b), .linearStart): return .linear(op, start: p, end: b)
        case (.linear(let op, let a, _), .linearEnd): return .linear(op, start: a, end: p)
        case (.radial(let op, _, let r, let angle, let f), .radialCenter):
            return .radial(op, center: p, radius: r, angle: angle, feather: f)
        case (.radial(let op, let c, let r, _, let f), .radialEdgeX):
            let d = offset(c)
            return .radial(op, center: c, radius: CGSize(width: max(0.005, hypot(d.x, d.y)), height: r.height),
                           angle: atan2(d.y, d.x) * 180 / .pi, feather: f)
        case (.radial(let op, let c, let r, let angle, let f), .radialEdgeY):
            let d = offset(c)
            return .radial(op, center: c, radius: CGSize(width: r.width, height: max(0.005, hypot(d.x, d.y))),
                           angle: angle, feather: f)
        default: return shape
        }
    }
}

extension MaskShape {
    /// A brush shape with the current brush settings on its strokes (other shapes unchanged).
    public func applyingBrush(hardness: Double, flow: Double, autoMask: Bool) -> MaskShape {
        guard case .brush(let op, let strokes) = self else { return self }
        return .brush(op, strokes.map { s in
            var s = s
            s.hardness = hardness
            s.flow = flow
            s.autoMask = autoMask && !s.erase
            return s
        })
    }
}

/// Brush and spot size steps for [ and ] (a geometric scale, like Lightroom).
public enum BrushSteps {
    public static func step(_ size: Double, by n: Int, range: ClosedRange<Double>) -> Double {
        (size * pow(1.25, Double(n))).clamped(range.lowerBound, range.upperBound)
    }
}

extension AppModel {
    /// [ / ]: resize the brush while a brush tool is active, or new heal/clone spots.
    public func stepBrushSize(_ n: Int) {
        if case .spot? = colorTools.mode {
            colorTools.spotRadius = BrushSteps.step(colorTools.spotRadius, by: n, range: 0.005...0.2)
        } else if colorTools.mode == .remove {
            colorTools.removal.size = BrushSteps.step(colorTools.removal.size, by: n, range: RemoveToolState.sizeRange)
        } else if [.brush, .eraser].contains(viewer.canvasTool) {
            viewer.brushSize = BrushSteps.step(viewer.brushSize, by: n, range: 0.005...0.5)
        }
    }
}

/// A gradient handle being dragged on the selected layer.
struct MaskHandleDrag {
    let photo: PhotoID
    let layer: Int
    let index: Int
    let handle: MaskHandle
}

extension ViewerModel {
    /// Gradient tools with a selected layer: pressing on a handle edits that gradient
    /// instead of drawing a new one.
    func maskHandleBegin(at p: CGPoint) -> Bool {
        guard [.linearGradient, .radialGradient].contains(activeTool), let app, let id = app.focusedID,
              let layer = selectedLayerID, let pane = layout.main,
              let shapes = app.edit(for: id).layers.first(where: { $0.id == layer })?.mask.shapes else { return false }
        let aspect = pane.image.width / max(pane.image.height, 1e-6)
        let display = shapes.map { transform.toDisplay($0, displayAspect: aspect) }
        let tol = 8 / max(pane.image.width, 1)
        guard let hit = MaskHandles.hit(pane.normalized(p), shapes: display, aspect: aspect, tolerance: tol) else { return false }
        app.colorTools.handleDrag = MaskHandleDrag(photo: id, layer: layer, index: hit.index, handle: hit.handle)
        return true
    }

    func maskHandleMove(to p: CGPoint) -> Bool {
        guard let app, let d = app.colorTools.handleDrag else { return false }
        guard d.photo == app.focusedID, let pane = layout.main else {
            app.colorTools.handleDrag = nil
            return true
        }
        let aspect = pane.image.width / max(pane.image.height, 1e-6)
        let u = pane.normalized(p)
        let t = transform
        app.updateEdits([d.photo], label: "Edit Gradient", coalesceKey: "gradient-handle") { _, e in
            guard let li = e.layers.firstIndex(where: { $0.id == d.layer }), d.index < e.layers[li].mask.shapes.count else { return }
            let shown = t.toDisplay(e.layers[li].mask.shapes[d.index], displayAspect: aspect)
            e.layers[li].mask.shapes[d.index] = t.toOriented(MaskHandles.move(shown, handle: d.handle, to: u, aspect: aspect),
                                                            displayAspect: aspect)
        }
        return true
    }

    func maskHandleEnd() -> Bool {
        guard let app, app.colorTools.handleDrag != nil else { return false }
        app.colorTools.handleDrag = nil
        app.endEditGesture()
        return true
    }
}
