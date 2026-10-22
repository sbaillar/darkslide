import CoreGraphics
import simd

public enum ZoomMode: Equatable, Sendable {
    /// Whole image fits the pane.
    case fit
    /// Device pixels per image pixel (1 = 100%, 2 = 200%).
    case scale(Double)

    /// 100%: one image pixel per device pixel.
    public static let actual = ZoomMode.scale(1)
}

/// Pure viewer math (no AppKit): where the image sits in a pane for a given
/// zoom and pan. All coordinates are **pane-local** points with a top-left
/// origin; `pan` is the offset of the image center from the pane center.
public enum ViewerGeometry {
    /// Margin around a fitted image, in points.
    public static let fitMargin: CGFloat = 12
    /// Zoom range, in device pixels per image pixel.
    public static let minScale = 0.02
    public static let maxScale = 16.0
    /// Discrete steps for zoom in/out (after Fit).
    public static let steps: [Double] = [0.25, 0.5, 1, 2, 4]

    /// Points per image pixel.
    public static func scale(image: CGSize, view: CGSize, backingScale: CGFloat, zoom: ZoomMode) -> CGFloat {
        guard image.width > 0, image.height > 0 else { return 1 }
        switch zoom {
        case .fit:
            let w = max(1, view.width - 2 * fitMargin), h = max(1, view.height - 2 * fitMargin)
            return min(w / image.width, h / image.height)
        case .scale(let s):
            return CGFloat(s) / max(backingScale, 1)
        }
    }

    /// Device pixels per image pixel for `zoom` (the fit scale resolved).
    public static func deviceScale(image: CGSize, view: CGSize, backingScale: CGFloat, zoom: ZoomMode) -> Double {
        Double(scale(image: image, view: view, backingScale: backingScale, zoom: zoom) * max(backingScale, 1))
    }

    /// Image rectangle in pane points. At non-fit zoom the origin is snapped to
    /// device pixels so 100%/200% land exactly on the pixel grid.
    public static func imageRect(image: CGSize, view: CGSize, backingScale: CGFloat, zoom: ZoomMode, pan: CGPoint) -> CGRect {
        let s = scale(image: image, view: view, backingScale: backingScale, zoom: zoom)
        let size = CGSize(width: image.width * s, height: image.height * s)
        let p = zoom == .fit ? .zero : clampPan(pan, imageSize: size, view: view)
        var r = CGRect(x: view.width / 2 + p.x - size.width / 2, y: view.height / 2 + p.y - size.height / 2,
                       width: size.width, height: size.height)
        if zoom != .fit { r.origin = snap(r.origin, backingScale: backingScale) }
        return r
    }

    /// Round a point to the device-pixel grid.
    public static func snap(_ p: CGPoint, backingScale: CGFloat) -> CGPoint {
        let b = max(backingScale, 1)
        return CGPoint(x: (p.x * b).rounded() / b, y: (p.y * b).rounded() / b)
    }

    /// Keep the image covering the view on axes where it is larger, centered otherwise.
    public static func clampPan(_ pan: CGPoint, imageSize: CGSize, view: CGSize) -> CGPoint {
        func axis(_ v: CGFloat, _ img: CGFloat, _ vw: CGFloat) -> CGFloat {
            let slack = max(0, (img - vw) / 2)
            return min(max(v, -slack), slack)
        }
        return CGPoint(x: axis(pan.x, imageSize.width, view.width), y: axis(pan.y, imageSize.height, view.height))
    }

    /// Pan that keeps the image point under `anchor` fixed when zooming from
    /// (`from`, `fromPan`) to `to`. Everything is in pane-local points.
    public static func panForZoom(anchor: CGPoint, image: CGSize, view: CGSize, backingScale: CGFloat,
                                  from: ZoomMode, fromPan: CGPoint, to: ZoomMode) -> CGPoint {
        guard to != .fit else { return .zero }
        let old = imageRect(image: image, view: view, backingScale: backingScale, zoom: from, pan: fromPan)
        guard old.width > 0, old.height > 0 else { return .zero }
        let u = CGPoint(x: min(max((anchor.x - old.minX) / old.width, 0), 1),
                        y: min(max((anchor.y - old.minY) / old.height, 0), 1))
        let s = scale(image: image, view: view, backingScale: backingScale, zoom: to)
        let size = CGSize(width: image.width * s, height: image.height * s)
        let raw = CGPoint(x: anchor.x - u.x * size.width - view.width / 2 + size.width / 2,
                          y: anchor.y - u.y * size.height - view.height / 2 + size.height / 2)
        return clampPan(raw, imageSize: size, view: view)
    }

    /// Pan for 100% zoom keeping the point under `cursor` (in the fitted pane) fixed.
    public static func panForActualZoom(at cursor: CGPoint, image: CGSize, view: CGSize, backingScale: CGFloat) -> CGPoint {
        guard view.width > 0, view.height > 0 else { return .zero }
        return panForZoom(anchor: cursor, image: image, view: view, backingScale: backingScale,
                          from: .fit, fromPan: .zero, to: .actual)
    }

    /// Next zoom step up/down from the effective device scale `current`.
    /// Zooming out past the smallest step, or below the fit scale, returns `.fit`.
    public static func step(from current: Double, fitScale: Double, up: Bool) -> ZoomMode {
        let eps = 1e-6
        if up {
            let next = steps.first { $0 > current + eps && $0 > fitScale + eps }
            return .scale(next ?? min(maxScale, current * 2))
        }
        guard let prev = steps.last(where: { $0 < current - eps }), prev > fitScale + eps else { return .fit }
        return .scale(prev)
    }

    /// Normalized rect of the image visible through the pane (0...1, clamped).
    public static func visibleRegion(imageRect r: CGRect, pane: CGSize) -> CGRect {
        guard r.width > 0, r.height > 0 else { return .zero }
        let x0 = max(0, -r.minX / r.width), y0 = max(0, -r.minY / r.height)
        let x1 = min(1, (pane.width - r.minX) / r.width), y1 = min(1, (pane.height - r.minY) / r.height)
        return CGRect(x: x0, y: y0, width: max(0, x1 - x0), height: max(0, y1 - y0))
    }

    /// A view-space rect as Metal NDC `(x0, y0, x1, y1)`; y flips (NDC is y-up).
    public static func ndc(_ rect: CGRect, in view: CGSize) -> SIMD4<Float> {
        guard view.width > 0, view.height > 0 else { return .zero }
        func x(_ v: CGFloat) -> Float { Float(v / view.width * 2 - 1) }
        func y(_ v: CGFloat) -> Float { Float(1 - v / view.height * 2) }
        return SIMD4(x(rect.minX), y(rect.maxY), x(rect.maxX), y(rect.minY))
    }

    /// Split the view into `count` equal side-by-side panes (compare mode).
    public static func panes(count: Int, in view: CGSize, gap: CGFloat = 2) -> [CGRect] {
        let n = max(1, count)
        let w = (view.width - gap * CGFloat(n - 1)) / CGFloat(n)
        return (0..<n).map { CGRect(x: CGFloat($0) * (w + gap), y: 0, width: w, height: view.height) }
    }
}
