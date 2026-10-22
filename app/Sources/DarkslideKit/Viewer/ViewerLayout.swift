import CoreGraphics

/// Where each image goes in the viewer, in view points. Pure; the Metal view
/// converts these to NDC and the SwiftUI overlay uses them for labels/handles.
/// Each pane is laid out in its own (pane-local) space, then offset.
public struct ViewerLayout: Equatable, Sendable {
    public enum Role: Equatable, Sendable { case main, pinned }

    public struct Pane: Equatable, Sendable {
        public var role: Role
        /// The pane's area in the view.
        public var frame: CGRect
        /// The image rect in view coordinates (may extend beyond `frame` when zoomed).
        public var image: CGRect
        /// Before/after split as a view x coordinate, if enabled.
        public var splitX: CGFloat?

        /// Normalized part of the image visible in this pane.
        public var visibleRegion: CGRect {
            ViewerGeometry.visibleRegion(imageRect: image.offsetBy(dx: -frame.minX, dy: -frame.minY), pane: frame.size)
        }

        /// Normalized image coordinate of a view point (unclamped).
        public func normalized(_ p: CGPoint) -> CGPoint {
            guard image.width > 0, image.height > 0 else { return .zero }
            return CGPoint(x: (p.x - image.minX) / image.width, y: (p.y - image.minY) / image.height)
        }

        /// View point of a normalized image coordinate.
        public func viewPoint(_ u: CGPoint) -> CGPoint {
            CGPoint(x: image.minX + u.x * image.width, y: image.minY + u.y * image.height)
        }
    }

    public var panes: [Pane]

    public init(panes: [Pane]) { self.panes = panes }

    /// - Parameters:
    ///   - main: pixel size of the focused photo.
    ///   - pinned: pixel size of the compare-pinned photo (compare mode) or `nil`.
    public static func make(state: ViewerState, main: CGSize, pinned: CGSize?) -> ViewerLayout {
        let view = state.viewSize
        guard view.width > 0, view.height > 0, main.width > 0, main.height > 0 else { return ViewerLayout(panes: []) }
        let frames = ViewerGeometry.panes(count: pinned == nil ? 1 : 2, in: view)
        let roles: [Role] = pinned == nil ? [.main] : [.pinned, .main]
        let sizes = pinned.map { [$0, main] } ?? [main]
        var panes: [Pane] = []
        for i in frames.indices {
            let f = frames[i]
            var r = ViewerGeometry.imageRect(image: sizes[i], view: f.size, backingScale: state.backingScale,
                                             zoom: state.zoom, pan: state.pan)
            r.origin.x += f.minX
            r.origin.y += f.minY
            let split: CGFloat? = state.beforeAfter && roles[i] == .main
                ? r.minX + r.width * CGFloat(min(max(state.split, 0), 1)) : nil
            panes.append(Pane(role: roles[i], frame: f, image: r, splitX: split))
        }
        return ViewerLayout(panes: panes)
    }

    public var main: Pane? { panes.first { $0.role == .main } }

    /// The pane under a view point.
    public func pane(at point: CGPoint) -> Pane? {
        panes.first { $0.frame.contains(point) }
    }

    /// Normalized (0...1) image coordinate under `point` in the main pane, if inside the image.
    public func imagePoint(at point: CGPoint) -> CGPoint? {
        guard let pane = main, pane.frame.contains(point), pane.image.contains(point),
              pane.image.width > 0, pane.image.height > 0 else { return nil }
        return pane.normalized(point)
    }

    /// Split fraction (0...1) for a drag at view x over the main image.
    public func splitFraction(atX x: CGFloat) -> Double? {
        guard let pane = main, pane.image.width > 0 else { return nil }
        return Double(min(max((x - pane.image.minX) / pane.image.width, 0), 1))
    }
}
