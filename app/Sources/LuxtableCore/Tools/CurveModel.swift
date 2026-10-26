import Foundation

/// Editable tone curve: control points on 0...1, sorted by x, with fixed-x
/// endpoints. Drawn (and later rendered) as a monotone cubic (Fritsch–Carlson),
/// so the curve never overshoots between points.
public struct CurveModel: Equatable, Sendable {
    public static let identity = [CurvePoint(0, 0), CurvePoint(1, 1)]
    /// Minimum horizontal gap between neighbouring points.
    public static let minGap = 0.01
    /// How far outside the unit square a dragged point must go to be deleted.
    public static let deleteMargin = 0.08

    public private(set) var points: [CurvePoint]

    /// From stored points (`[]` = identity). Endpoints are added if missing.
    public init(stored: [CurvePoint]) {
        var p = stored.sorted { $0.x < $1.x }.map { CurvePoint(min(max($0.x, 0), 1), min(max($0.y, 0), 1)) }
        if p.first?.x != 0 { p.insert(CurvePoint(0, 0), at: 0) }
        if p.last?.x != 1 { p.append(CurvePoint(1, 1)) }
        points = p
    }

    /// What to store in `CurveEdit`: empty when the curve is the identity line.
    public var stored: [CurvePoint] { points == Self.identity ? [] : points }

    public var isIdentity: Bool { points == Self.identity }

    func isEndpoint(_ i: Int) -> Bool { i == 0 || i == points.count - 1 }

    /// Index of the nearest point within `radius` (in curve units), if any.
    public func hitTest(_ p: CurvePoint, radius: Double) -> Int? {
        var best: (Int, Double)?
        for (i, q) in points.enumerated() {
            let d = hypot(q.x - p.x, q.y - p.y)
            if d <= radius, d < (best?.1 ?? .infinity) { best = (i, d) }
        }
        return best?.0
    }

    /// Insert a point keeping x order. Returns its index, or `nil` if too close
    /// (horizontally) to an existing point.
    @discardableResult
    public mutating func insert(_ p: CurvePoint) -> Int? {
        let q = CurvePoint(min(max(p.x, 0), 1), min(max(p.y, 0), 1))
        guard !points.contains(where: { abs($0.x - q.x) < Self.minGap }) else { return nil }
        let i = points.firstIndex { $0.x > q.x } ?? points.count
        points.insert(q, at: i)
        return i
    }

    /// Move point `i`. Endpoints keep their x; inner points stay between neighbours.
    @discardableResult
    public mutating func move(_ i: Int, to p: CurvePoint) -> CurvePoint {
        guard points.indices.contains(i) else { return p }
        let y = min(max(p.y, 0), 1)
        let x: Double
        if isEndpoint(i) {
            x = points[i].x
        } else {
            let lo = points[i - 1].x + Self.minGap, hi = points[i + 1].x - Self.minGap
            x = min(max(p.x, lo), hi)
        }
        points[i] = CurvePoint(x, y)
        return points[i]
    }

    /// Remove an inner point. Endpoints can't be removed.
    @discardableResult
    public mutating func remove(_ i: Int) -> Bool {
        guard points.indices.contains(i), !isEndpoint(i) else { return false }
        points.remove(at: i)
        return true
    }

    /// A point dragged this far outside the square is deleted on release (Lightroom behaviour).
    public static func isDraggedOut(_ p: CurvePoint) -> Bool {
        p.x < -deleteMargin || p.x > 1 + deleteMargin || p.y < -deleteMargin || p.y > 1 + deleteMargin
    }

    /// Fritsch–Carlson tangents for the current points.
    func tangents() -> [Double] {
        let n = points.count
        guard n >= 2 else { return [0] }
        var d = [Double](repeating: 0, count: n - 1)
        for k in 0..<(n - 1) {
            d[k] = (points[k + 1].y - points[k].y) / (points[k + 1].x - points[k].x)
        }
        var m = [Double](repeating: 0, count: n)
        m[0] = d[0]
        m[n - 1] = d[n - 2]
        for k in 1..<(n - 1) {
            m[k] = d[k - 1] * d[k] <= 0 ? 0 : (d[k - 1] + d[k]) / 2
        }
        for k in 0..<(n - 1) {
            if d[k] == 0 {
                m[k] = 0
                m[k + 1] = 0
                continue
            }
            let a = m[k] / d[k], b = m[k + 1] / d[k]
            let s = a * a + b * b
            if s > 9 {
                let t = 3 / s.squareRoot()
                m[k] = t * a * d[k]
                m[k + 1] = t * b * d[k]
            }
        }
        return m
    }

    /// Curve value at `x` (0...1), clamped to 0...1.
    public func evaluate(_ x: Double) -> Double {
        evaluate(x, tangents: tangents())
    }

    func evaluate(_ x: Double, tangents m: [Double]) -> Double {
        let xc = min(max(x, 0), 1)
        guard let k = points.indices.dropLast().last(where: { points[$0].x <= xc }) else { return points[0].y }
        let (p0, p1) = (points[k], points[k + 1])
        let h = p1.x - p0.x
        let t = (xc - p0.x) / h
        let t2 = t * t, t3 = t2 * t
        let v = (2 * t3 - 3 * t2 + 1) * p0.y + (t3 - 2 * t2 + t) * h * m[k]
            + (-2 * t3 + 3 * t2) * p1.y + (t3 - t2) * h * m[k + 1]
        return min(max(v, 0), 1)
    }

    /// `count` evenly spaced samples over 0...1 (256 = one per 8-bit level).
    public func samples(count: Int = 256) -> [Double] {
        let m = tangents()
        let n = max(count, 2)
        return (0..<n).map { evaluate(Double($0) / Double(n - 1), tangents: m) }
    }
}
