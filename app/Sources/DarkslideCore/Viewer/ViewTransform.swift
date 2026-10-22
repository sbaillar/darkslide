import CoreGraphics
import Foundation

/// How the viewer displays an oriented image, following the `ds_core::adjust::Crop`
/// contract (the one ds-pipeline renders):
///
/// - **oriented** space: the EXIF-upright image (frames, embedded previews, masks and
///   the stored crop `rect` all live here; normalized 0...1, y down).
/// - **display** space: what the canvas shows: the oriented image rotated about its
///   center so the content turns **counter-clockwise** by `angle` (ds-core
///   `angle_deg`), then turned `quarterTurns` times clockwise. The two commute
///   (both are rotations about the center), and both are rigid in pixels.
///
/// The crop rect is stored in oriented space (canonical). Seen in display space,
/// it is an axis-aligned box with the same center (mapped) and the same pixel size
/// (w/h swapped for odd turns): `displayRect(canonical:)` / `canonicalRect(display:)`.
/// Turning or straightening therefore never changes which content is cropped.
/// The shader implements `toOriented` (`MetalViewerRenderer`).
public struct ViewTransform: Equatable, Sendable {
    /// Clockwise quarter turns, 0...3.
    public var quarterTurns: Int
    /// Straighten, degrees (ds-core `angle_deg`): positive turns the content counter-clockwise.
    public var angle: Double

    public init(quarterTurns: Int = 0, angle: Double = 0) {
        self.quarterTurns = ((quarterTurns % 4) + 4) % 4
        self.angle = angle
    }

    public init(crop: CropEdit) {
        self.init(quarterTurns: crop.quarterTurns, angle: crop.angle)
    }

    public var isIdentity: Bool { quarterTurns == 0 && angle == 0 }
    public var swapsAxes: Bool { quarterTurns % 2 == 1 }

    /// Pixel size of the displayed image for an oriented pixel size.
    public func displaySize(oriented s: CGSize) -> CGSize {
        swapsAxes ? CGSize(width: s.height, height: s.width) : s
    }

    /// On-screen clockwise rotation of the content, degrees (= -angle).
    public var screenClockwise: Double { -angle }

    /// On-screen clockwise rotation, radians, for the shader.
    public var angleRadians: Float { Float(screenClockwise * .pi / 180) }

    /// The whole display transform as one clockwise rotation about the image center, in
    /// degrees: turns and straighten are both rigid rotations about the center, so
    /// `toDisplay` is the oriented image (drawn at `orientedSize(display:)`) turned by this.
    /// Snapshots draw frames this way (they can't capture the Metal shader).
    public var displayRotationDegrees: Double { Double(quarterTurns) * 90 + screenClockwise }

    /// Size to draw the oriented image at, for a displayed (turned) size.
    public func orientedSize(display s: CGSize) -> CGSize {
        swapsAxes ? CGSize(width: s.height, height: s.width) : s
    }

    // MARK: Quarter turns

    static func turn(_ o: CGPoint, _ k: Int) -> CGPoint {
        switch k {
        case 1: CGPoint(x: 1 - o.y, y: o.x)
        case 2: CGPoint(x: 1 - o.x, y: 1 - o.y)
        case 3: CGPoint(x: o.y, y: 1 - o.x)
        default: o
        }
    }

    static func unturn(_ q: CGPoint, _ k: Int) -> CGPoint {
        switch k {
        case 1: CGPoint(x: q.y, y: 1 - q.x)
        case 2: CGPoint(x: 1 - q.x, y: 1 - q.y)
        case 3: CGPoint(x: 1 - q.y, y: q.x)
        default: q
        }
    }

    // MARK: Angle (in pixel-proportional space so the rotation is rigid)

    static func rotate(_ p: CGPoint, degrees: Double, aspect a: CGFloat) -> CGPoint {
        guard degrees != 0 else { return p }
        let r = degrees * .pi / 180
        let (c, s) = (CGFloat(cos(r)), CGFloat(sin(r)))
        let x = (p.x - 0.5) * a, y = p.y - 0.5
        // Clockwise on screen in y-down coordinates.
        return CGPoint(x: (c * x - s * y) / a + 0.5, y: s * x + c * y + 0.5)
    }

    /// Display-normalized point → oriented-normalized point.
    /// `displayAspect` is width/height of the displayed (turned) image.
    public func toOriented(_ t: CGPoint, displayAspect a: CGFloat) -> CGPoint {
        Self.unturn(Self.rotate(t, degrees: -screenClockwise, aspect: a), quarterTurns)
    }

    /// Oriented-normalized point → display-normalized point.
    public func toDisplay(_ o: CGPoint, displayAspect a: CGFloat) -> CGPoint {
        Self.rotate(Self.turn(o, quarterTurns), degrees: screenClockwise, aspect: a)
    }

    // MARK: Crop rect (canonical oriented ↔ display box)

    /// The display-space box of a canonical (oriented) crop rect.
    public func displayRect(canonical r: CGRect, displayAspect a: CGFloat) -> CGRect {
        let c = toDisplay(CGPoint(x: r.midX, y: r.midY), displayAspect: a)
        let (w, h) = swapsAxes ? (r.height, r.width) : (r.width, r.height)
        return CGRect(x: c.x - w / 2, y: c.y - h / 2, width: w, height: h)
    }

    /// The crop ds-pipeline actually renders (canonical, oriented space). A full-frame
    /// crop with a straighten angle is auto-fitted: the largest centered rectangle of the
    /// frame's aspect with no empty corners. Same formula as ds-pipeline
    /// `geometry::inscribed_crop` (angle clamped to ±45°); any other crop is used as is.
    /// `orientedAspect` is width/height of the oriented image.
    public static func renderedRect(canonical r: CGRect, angle: Double, orientedAspect: CGFloat) -> CGRect {
        let angle = angle.isFinite ? min(max(angle, -45), 45) : 0
        guard angle != 0, r == CGRect(x: 0, y: 0, width: 1, height: 1),
              orientedAspect.isFinite, orientedAspect > 0 else { return r }
        let (a, b) = (Double(orientedAspect) * 0.5, 0.5)
        let rad = angle * .pi / 180
        let (s, c) = (abs(sin(rad)), abs(cos(rad)))
        let (ex, ey) = (a * c + b * s, a * s + b * c)
        let k = CGFloat(min(a / ex, b / ey, 1))
        return CGRect(x: 0.5 - 0.5 * k, y: 0.5 - 0.5 * k, width: k, height: k)
    }

    /// Display-space box of what ds-pipeline renders for `crop`. With the pipeline's
    /// `mapping` of the final frame (live mode) the box is read from it: the pipeline's
    /// own auto-fit, including lens distortion and keystone, with no emulation here.
    /// Without one (demo, or before the first plan) `renderedRect`'s straighten-only
    /// auto-fit is used.
    public func renderedDisplayRect(_ crop: CropEdit, displayAspect a: CGFloat, mapping: FrameMapping? = nil) -> CGRect {
        let orientedAspect = swapsAxes ? 1 / a : a
        let canonical = mapping.flatMap { Self.canonicalRect(from: $0, orientedAspect: orientedAspect, quarterTurns: quarterTurns) }
            ?? Self.renderedRect(canonical: crop.rect, angle: crop.angle, orientedAspect: orientedAspect)
        return displayRect(canonical: canonical, displayAspect: a)
    }

    /// The crop rect (canonical, oriented) the pipeline rendered, from its final-frame
    /// → crop-space `mapping`: the final frame's center and edge lengths taken back into
    /// crop space (rigid, so the straighten angle doesn't change the lengths), before the
    /// quarter turns. `orientedAspect` is width/height of the oriented image.
    public static func canonicalRect(from mapping: FrameMapping, orientedAspect a: CGFloat, quarterTurns: Int) -> CGRect? {
        guard a.isFinite, a > 0 else { return nil }
        let c = mapping.toCrop(CGPoint(x: 0.5, y: 0.5))
        // Lengths in pixel-proportional crop space (x scaled by the aspect).
        func length(_ p: CGPoint, _ q: CGPoint) -> CGFloat {
            let (u, v) = (mapping.toCrop(p), mapping.toCrop(q))
            return hypot((u.x - v.x) * a, u.y - v.y)
        }
        let across = length(CGPoint(x: 0, y: 0.5), CGPoint(x: 1, y: 0.5))
        let down = length(CGPoint(x: 0.5, y: 0), CGPoint(x: 0.5, y: 1))
        let odd = ((quarterTurns % 4) + 4) % 4 % 2 == 1
        let (wpx, hpx) = odd ? (down, across) : (across, down)
        let (w, h) = (wpx / a, hpx)
        guard w > 0, h > 0, w.isFinite, h.isFinite else { return nil }
        return snapFull(CGRect(x: c.x - w / 2, y: c.y - h / 2, width: w, height: h))
    }

    /// The canonical (oriented) rect of a display-space crop box.
    public func canonicalRect(display r: CGRect, displayAspect a: CGFloat) -> CGRect {
        let c = toOriented(CGPoint(x: r.midX, y: r.midY), displayAspect: a)
        let (w, h) = swapsAxes ? (r.height, r.width) : (r.width, r.height)
        let out = CGRect(x: c.x - w / 2, y: c.y - h / 2, width: w, height: h)
        return Self.snapFull(out)
    }

    /// Float noise around the full frame collapses to exactly full (the identity crop).
    static func snapFull(_ r: CGRect) -> CGRect {
        let full = CGRect(x: 0, y: 0, width: 1, height: 1)
        let eps: CGFloat = 1e-9
        return abs(r.minX) < eps && abs(r.minY) < eps && abs(r.width - 1) < eps && abs(r.height - 1) < eps ? full : r
    }

    /// Bounding box in oriented space of a display-space rect (for tile requests).
    public func orientedBounds(of r: CGRect, displayAspect a: CGFloat) -> CGRect {
        let corners = [CGPoint(x: r.minX, y: r.minY), CGPoint(x: r.maxX, y: r.minY),
                       CGPoint(x: r.minX, y: r.maxY), CGPoint(x: r.maxX, y: r.maxY)].map { toOriented($0, displayAspect: a) }
        let xs = corners.map(\.x), ys = corners.map(\.y)
        return CGRect(x: xs.min()!, y: ys.min()!, width: xs.max()! - xs.min()!, height: ys.max()! - ys.min()!)
            .intersection(CGRect(x: 0, y: 0, width: 1, height: 1))
    }

    /// A mask shape drawn in display space, expressed in oriented space.
    public func toOriented(_ shape: MaskShape, displayAspect a: CGFloat) -> MaskShape {
        func o(_ p: CGPoint) -> CGPoint { toOriented(p, displayAspect: a) }
        switch shape {
        case .fill, .ai, .aiPerson: return shape
        case .brush(let op, let strokes):
            return .brush(op, strokes.map { s in
                var s = s
                s.points = s.points.map(o)
                return s
            })
        case .linear(let op, let s, let e):
            return .linear(op, start: o(s), end: o(e))
        case .radial(let op, let c, let r, let ang, let f):
            let rr = swapsAxes ? CGSize(width: r.height, height: r.width) : r
            return .radial(op, center: o(c), radius: rr, angle: ang - screenClockwise, feather: f)
        }
    }

    /// Inverse of the shape `toOriented` (overlay drawing).
    public func toDisplay(_ shape: MaskShape, displayAspect a: CGFloat) -> MaskShape {
        func d(_ p: CGPoint) -> CGPoint { toDisplay(p, displayAspect: a) }
        switch shape {
        case .fill, .ai, .aiPerson: return shape
        case .brush(let op, let strokes):
            return .brush(op, strokes.map { s in
                var s = s
                s.points = s.points.map(d)
                return s
            })
        case .linear(let op, let s, let e):
            return .linear(op, start: d(s), end: d(e))
        case .radial(let op, let c, let r, let ang, let f):
            let rr = swapsAxes ? CGSize(width: r.height, height: r.width) : r
            return .radial(op, center: d(c), radius: rr, angle: ang + screenClockwise, feather: f)
        }
    }
}
