import CoreGraphics
import Foundation

/// Part of an existing crop rectangle a drag grabbed (L8).
public enum CropHandle: Equatable, Sendable {
    /// Inside: move the rectangle.
    case move
    /// Corner 0 top-left, 1 top-right, 2 bottom-right, 3 bottom-left.
    case corner(Int)
    /// Edge 0 top, 1 right, 2 bottom, 3 left.
    case edge(Int)
}

/// An in-progress drag with a canvas tool, in normalized display coordinates.
public struct CanvasDrag: Equatable, Sendable {
    public var tool: CanvasTool
    public var start: CGPoint
    public var current: CGPoint
    /// Brush path so far.
    public var points: [CGPoint]
    /// Crop: the handle grabbed (nil: draw a new rectangle) and the rect at drag start.
    public var cropHandle: CropHandle?
    public var initialCrop: CGRect?

    public init(tool: CanvasTool, start: CGPoint) {
        self.tool = tool
        self.start = start
        current = start
        points = [start]
    }

    mutating func move(to p: CGPoint) {
        current = p
        if tool == .brush || tool == .eraser, points.last.map({ hypot($0.x - p.x, $0.y - p.y) > 0.002 }) ?? true {
            points.append(p)
        }
    }
}

/// What a finished canvas drag does to the edit.
public enum CanvasCommit: Equatable, Sendable {
    case mask(MaskShape)
    case crop(CGRect)
    case straighten(Double)
    case pickColor(CGPoint)
    case pickWhiteBalance(CGPoint)
    /// Lens blur focus at a display-normalized point.
    case pickFocus(CGPoint)
    case none
}

/// Pure geometry for the on-canvas tools (crop, straighten, gradients, brush).
public enum CanvasInteraction {
    /// Minimum drag (normalized) before a shape is created.
    public static let minDrag: CGFloat = 0.005

    /// Map a finished drag to its edit.
    /// - Parameters:
    ///   - imageSize: pixel size of the photo (aspect for crop lock and straighten angles).
    ///   - cropAspect: crop aspect lock (long:short) or nil.
    ///   - brushSize: brush diameter relative to the long edge.
    ///   - cropPortrait: a fixed orientation for the aspect lock (a 4:5 preset stays portrait
    ///     whichever way the drag goes); nil follows the drag.
    public static func commit(_ d: CanvasDrag, imageSize: CGSize, cropAspect: (long: Int, short: Int)?,
                              brushSize: Double = 0.05, cropPortrait: Bool? = nil) -> CanvasCommit {
        let moved = hypot(d.current.x - d.start.x, d.current.y - d.start.y) >= minDrag
        switch d.tool {
        case .pan, .rotate: return .none
        case .colorPicker: return .pickColor(d.current)
        case .wbPicker: return .pickWhiteBalance(d.current)
        case .focusPoint: return .pickFocus(d.current)
        case .brush, .eraser:
            let stroke = BrushStroke(points: d.points, size: brushSize, erase: d.tool == .eraser)
            return .mask(.brush(d.tool == .eraser ? .subtract : .add, [stroke]))
        case .linearGradient:
            return moved ? .mask(.linear(.add, start: d.start, end: d.current)) : .none
        case .radialGradient:
            guard moved else { return .none }
            // A circle through the drag's farther axis (the preview's circle), its radius in
            // long-edge units (lt-core `MaskShape::Radial`).
            let long = max(imageSize.width, imageSize.height, 1)
            let r = max(abs(d.current.x - d.start.x) * imageSize.width, abs(d.current.y - d.start.y) * imageSize.height) / long
            return .mask(.radial(.add, center: d.start, radius: CGSize(width: r, height: r), angle: 0, feather: 0.5))
        case .crop:
            return moved ? .crop(cropRect(for: d, imageSize: imageSize, aspect: cropAspect, portrait: cropPortrait)) : .none
        case .straighten:
            return moved ? .straighten(straightenAngle(from: d.start, to: d.current, imageSize: imageSize)) : .none
        }
    }

    /// Pixel width / height of an aspect lock: `portrait` fixes the orientation, nil takes
    /// the orientation of `w`×`h` pixels (the drag's or the existing rect's).
    static func lockRatio(_ aspect: (long: Int, short: Int), portrait: Bool?, w: CGFloat, h: CGFloat) -> CGFloat {
        let p = portrait ?? (h > w)
        return p ? CGFloat(aspect.short) / CGFloat(aspect.long) : CGFloat(aspect.long) / CGFloat(aspect.short)
    }

    /// Normalized crop rect spanning two corners, honoring an aspect lock
    /// (oriented to the drag, a taller drag gives a portrait crop, unless `portrait`
    /// fixes it), clamped to the image. The locked rect always fits inside the dragged
    /// box, so it never needs clipping at the image edge (L5).
    public static func cropRect(from a: CGPoint, to b: CGPoint, imageSize: CGSize,
                                aspect: (long: Int, short: Int)?, portrait: Bool? = nil) -> CGRect {
        let a = clampUnit(a), b = clampUnit(b)
        var w = abs(b.x - a.x), h = abs(b.y - a.y)
        if let aspect, aspect.long > 0, aspect.short > 0, imageSize.width > 0, imageSize.height > 0 {
            // Work in pixels, fit inside the dragged box.
            let pw = w * imageSize.width, ph = h * imageSize.height
            let ratio = lockRatio(aspect, portrait: portrait, w: pw, h: ph)
            let (fw, fh) = pw / max(ph, 1e-9) > ratio ? (ph * ratio, ph) : (pw, pw / ratio)
            w = fw / imageSize.width
            h = fh / imageSize.height
        }
        let x = b.x >= a.x ? a.x : a.x - w
        let y = b.y >= a.y ? a.y : a.y - h
        return CGRect(x: x, y: y, width: w, height: h).intersection(CGRect(x: 0, y: 0, width: 1, height: 1))
    }

    /// On-screen **clockwise** rotation (degrees, -45...45) that makes the line a→b level
    /// (or plumb, if nearer vertical). Subtract it from a lt-core angle (counter-clockwise).
    public static func straightenAngle(from a: CGPoint, to b: CGPoint, imageSize: CGSize) -> Double {
        let dx = Double((b.x - a.x) * imageSize.width), dy = Double((b.y - a.y) * imageSize.height)
        var deg = atan2(dy, dx) * 180 / .pi // image y is down: positive = line falls to the right
        // Fold into -45...45 relative to the nearest axis.
        while deg > 45 { deg -= 90 }
        while deg < -45 { deg += 90 }
        return -deg
    }

    /// Which handle of `rect` is under `p` (corners win over edges, edges over inside).
    public static func cropHit(_ p: CGPoint, rect r: CGRect, tolerance t: CGSize) -> CropHandle? {
        guard r.width > 0, r.height > 0 else { return nil }
        let nearL = abs(p.x - r.minX) <= t.width, nearR = abs(p.x - r.maxX) <= t.width
        let nearT = abs(p.y - r.minY) <= t.height, nearB = abs(p.y - r.maxY) <= t.height
        let inX = p.x >= r.minX - t.width && p.x <= r.maxX + t.width
        let inY = p.y >= r.minY - t.height && p.y <= r.maxY + t.height
        switch (nearL, nearR, nearT, nearB) {
        case (true, _, true, _): return .corner(0)
        case (_, true, true, _): return .corner(1)
        case (_, true, _, true): return .corner(2)
        case (true, _, _, true): return .corner(3)
        default: break
        }
        if nearT, inX { return .edge(0) }
        if nearR, inY { return .edge(1) }
        if nearB, inX { return .edge(2) }
        if nearL, inY { return .edge(3) }
        return r.contains(p) && !(r.width >= 0.999 && r.height >= 0.999) ? .move : nil
    }

    /// The crop a drag describes: a new rectangle, or the grabbed rectangle moved/resized,
    /// always honouring the aspect lock (also while dragging).
    public static func cropRect(for d: CanvasDrag, imageSize: CGSize, aspect: (long: Int, short: Int)?,
                                portrait: Bool? = nil) -> CGRect {
        guard let handle = d.cropHandle, let r0 = d.initialCrop else {
            return cropRect(from: d.start, to: d.current, imageSize: imageSize, aspect: aspect, portrait: portrait)
        }
        let dx = d.current.x - d.start.x, dy = d.current.y - d.start.y
        switch handle {
        case .move:
            return clampRect(r0.offsetBy(dx: dx, dy: dy))
        case .corner(let c):
            // Opposite corner stays; dragged corner follows the pointer.
            let opposite = [CGPoint(x: r0.maxX, y: r0.maxY), CGPoint(x: r0.minX, y: r0.maxY),
                            CGPoint(x: r0.minX, y: r0.minY), CGPoint(x: r0.maxX, y: r0.minY)][c]
            let moving = [CGPoint(x: r0.minX, y: r0.minY), CGPoint(x: r0.maxX, y: r0.minY),
                          CGPoint(x: r0.maxX, y: r0.maxY), CGPoint(x: r0.minX, y: r0.maxY)][c]
            return cropRect(from: opposite, to: CGPoint(x: moving.x + dx, y: moving.y + dy), imageSize: imageSize,
                            aspect: aspect, portrait: portrait)
        case .edge(let e):
            var r = r0
            switch e {
            case 0: r = CGRect(x: r0.minX, y: r0.minY + dy, width: r0.width, height: r0.height - dy)
            case 1: r.size.width += dx
            case 2: r.size.height += dy
            default: r = CGRect(x: r0.minX + dx, y: r0.minY, width: r0.width - dx, height: r0.height)
            }
            r = r.standardized
            if let aspect, aspect.long > 0, aspect.short > 0, imageSize.width > 0, imageSize.height > 0 {
                // Keep the ratio: the dragged edge drives, the other axis follows about the
                // center; near the image edge both shrink together (L5: never clipped,
                // which used to break the ratio).
                let ratio = lockRatio(aspect, portrait: portrait, w: r0.width * imageSize.width,
                                      h: r0.height * imageSize.height)
                let unit = CGRect(x: 0, y: 0, width: 1, height: 1)
                r = r.intersection(unit)
                if e % 2 == 0 {
                    // Height drives; the width may not pass the image edge about the center.
                    let maxW = 2 * min(r.midX, 1 - r.midX)
                    var h = r.height
                    var w = h * imageSize.height * ratio / imageSize.width
                    if w > maxW {
                        w = maxW
                        h = w * imageSize.width / ratio / imageSize.height
                    }
                    let y = e == 0 ? r.maxY - h : r.minY // the edge not dragged stays put
                    r = CGRect(x: r.midX - w / 2, y: y, width: w, height: h)
                } else {
                    let maxH = 2 * min(r.midY, 1 - r.midY)
                    var w = r.width
                    var h = w * imageSize.width / ratio / imageSize.height
                    if h > maxH {
                        h = maxH
                        w = h * imageSize.height * ratio / imageSize.width
                    }
                    let x = e == 3 ? r.maxX - w : r.minX
                    r = CGRect(x: x, y: r.midY - h / 2, width: w, height: h)
                }
            }
            return r.intersection(CGRect(x: 0, y: 0, width: 1, height: 1))
        }
    }

    /// Keep a rect inside the unit square by moving (not resizing) it.
    /// `r` kept inside `area` (the crop tool's image area): moved in when it fits,
    /// else shrunk about its centre (keeping its shape, so an aspect lock holds) until
    /// it does. Display-normalized coordinates.
    public static func confine(_ r: CGRect, to area: CGRect) -> CGRect {
        guard area.width > 0, area.height > 0, !area.contains(r) else { return r }
        let k = min(1, area.width / max(r.width, 1e-9), area.height / max(r.height, 1e-9))
        let (w, h) = (r.width * k, r.height * k)
        let x = min(max(r.midX - w / 2, area.minX), area.maxX - w)
        let y = min(max(r.midY - h / 2, area.minY), area.maxY - h)
        return CGRect(x: x, y: y, width: w, height: h)
    }

    public static func clampRect(_ r: CGRect) -> CGRect {
        let w = min(r.width, 1), h = min(r.height, 1)
        return CGRect(x: min(max(r.minX, 0), 1 - w), y: min(max(r.minY, 0), 1 - h), width: w, height: h)
    }

    static func clampUnit(_ p: CGPoint) -> CGPoint {
        CGPoint(x: min(max(p.x, 0), 1), y: min(max(p.y, 0), 1))
    }

    /// Next quarter-turn count (0...3) after rotating left (-1) or right (+1).
    public static func rotate(_ quarterTurns: Int, by delta: Int) -> Int {
        ((quarterTurns + delta) % 4 + 4) % 4
    }
}

extension CanvasTool {
    /// Short label for tool pickers.
    public var viewerLabel: String {
        switch self {
        case .pan: "Pan"
        case .crop: "Crop"
        case .straighten: "Straighten"
        case .rotate: "Rotate"
        case .colorPicker: "Color Readout"
        case .wbPicker: "White Balance Picker"
        case .focusPoint: "Lens Blur Focus"
        case .brush: "Brush"
        case .eraser: "Eraser"
        case .linearGradient: "Linear Gradient"
        case .radialGradient: "Radial Gradient"
        }
    }

    public var viewerSymbol: String {
        switch self {
        case .pan: "hand.raised"
        case .crop: "crop"
        case .straighten: "level"
        case .rotate: "rotate.right"
        case .colorPicker: "eyedropper.halffull"
        case .wbPicker: "eyedropper"
        case .focusPoint: "scope"
        case .brush: "paintbrush.pointed"
        case .eraser: "eraser"
        case .linearGradient: "square.bottomhalf.filled"
        case .radialGradient: "circle.circle"
        }
    }

    /// Whether the tool edits the selected layer's mask.
    public var editsMask: Bool {
        switch self {
        case .brush, .eraser, .linearGradient, .radialGradient: true
        default: false
        }
    }
}
