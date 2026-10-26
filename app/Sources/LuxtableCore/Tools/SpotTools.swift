import CoreGraphics
import Foundation

/// Heal/clone spot helpers (pure, tested). Points are normalized in the oriented frame;
/// a spot's radius is relative to the image's long edge.
public enum SpotSource {
    /// Radius (relative to the long edge) as normalized x/y extents for an image of
    /// width/height `aspect`.
    public static func extents(radius: Double, aspect: Double) -> (dx: Double, dy: Double) {
        aspect >= 1 ? (radius, radius * aspect) : (radius / aspect, radius)
    }

    /// Candidate source centres around `center`: 8 directions at 2.2 radii, inside the image.
    public static func candidates(center: CGPoint, radius: Double, aspect: Double) -> [CGPoint] {
        let (dx, dy) = extents(radius: radius, aspect: aspect)
        return (0..<8).compactMap { k -> CGPoint? in
            let a = Double(k) * .pi / 4
            let p = CGPoint(x: center.x + cos(a) * dx * 2.2, y: center.y + sin(a) * dy * 2.2)
            guard p.x - dx >= 0, p.x + dx <= 1, p.y - dy >= 0, p.y + dy <= 1 else { return nil }
            return p
        }
    }

    /// Lightroom-style automatic source: the nearby patch whose colour best matches the
    /// ring around the spot (so the fill blends in). `sample` returns a patch's mean
    /// colour (nil where it can't be read). Falls back to one radius-width to the right.
    public static func auto(center: CGPoint, radius: Double, aspect: Double,
                            sample: (CGPoint, Double) -> SIMD3<Double>?) -> CGPoint {
        let (dx, dy) = extents(radius: radius, aspect: aspect)
        let ring = (0..<8).compactMap { k -> SIMD3<Double>? in
            let a = Double(k) * .pi / 4 + .pi / 8
            return sample(CGPoint(x: center.x + cos(a) * dx * 1.4, y: center.y + sin(a) * dy * 1.4), radius * 0.3)
        }
        let fallback = CGPoint(x: min(center.x + dx * 2.2, 1 - dx), y: center.y)
        guard !ring.isEmpty else { return fallback }
        let target = ring.reduce(SIMD3<Double>(0, 0, 0), +) / Double(ring.count)
        var best: (CGPoint, Double)?
        for p in candidates(center: center, radius: radius, aspect: aspect) {
            guard let c = sample(p, radius) else { continue }
            let d = c - target
            let dist = (d * d).sum()
            if best == nil || dist < best!.1 { best = (p, dist) }
        }
        return best?.0 ?? fallback
    }
}
