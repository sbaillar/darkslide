import DarkslideAI
import Foundation

/// A model's depth output: relative inverse depth, higher = nearer, row-major.
public struct DepthField: Sendable, Equatable {
    public let width: Int
    public let height: Int
    public let values: [Float]

    public init(width: Int, height: Int, values: [Float]) {
        precondition(values.count == width * height, "one value per pixel")
        self.width = width
        self.height = height
        self.values = values
    }

    /// Bilinear sample at normalized `(u, v)` (pixel centres at `(i + 0.5) / n`).
    func sample(_ u: Double, _ v: Double) -> Float {
        DepthComposer.bilinear(values, width, height, u, v)
    }
}

/// Builds the lens blur's depth map (ds-pipeline `MaskSource::depth_map`: 8-bit, E frame,
/// 255 = nearest) from a model's depth and the subject matte.
///
/// - With depth: the field is stretched over the frame (the model sees the frame
///   stretched too) and normalized to its own range; inside the matte it is flattened
///   to the subject's (matte-weighted mean) depth, so the whole subject is in one plane
///   and its edge, hair included, is exactly the matte's.
/// - Without depth (no model): the matte is near (0.9), the rest falls off with the
///   distance from the subject to a background that is nearer toward the bottom of the
///   frame (the ground). Without a matte too, only that gradient.
public enum DepthComposer {
    /// Depth of the subject in the fallback map.
    static let fallbackSubject: Float = 0.9

    public static func compose(depth: DepthField?, matte: VisionMask?, width: Int, height: Int) -> VisionMask {
        let w = max(width, 1), h = max(height, 1)
        let m: [Float] = matte.map { mt in
            let src = mt.bytes.map { Float($0) / 255 }
            return (0..<(w * h)).map { i in
                bilinear(src, mt.width, mt.height, (Double(i % w) + 0.5) / Double(w), (Double(i / w) + 0.5) / Double(h))
            }
        } ?? []
        var out = [Float](repeating: 0, count: w * h)
        if let depth, !depth.values.isEmpty {
            let lo = depth.values.min() ?? 0, hi = depth.values.max() ?? 1
            let span = hi - lo > 1e-6 ? hi - lo : 1
            for i in 0..<(w * h) {
                let d = depth.sample((Double(i % w) + 0.5) / Double(w), (Double(i / w) + 0.5) / Double(h))
                out[i] = (d - lo) / span
            }
            if !m.isEmpty {
                var sum: Float = 0, weight: Float = 0
                for i in out.indices where m[i] >= 0.5 {
                    sum += m[i] * out[i]
                    weight += m[i]
                }
                if weight > 0 {
                    let subject = sum / weight
                    for i in out.indices { out[i] = m[i] * subject + (1 - m[i]) * out[i] }
                }
            }
        } else {
            let dist = m.isEmpty ? nil : distances(m, w, h)
            let reach = Float(0.15 * Double(max(w, h)))
            for i in 0..<(w * h) {
                let y = Float(i / w) + 0.5
                let ground = 0.15 + 0.35 * y / Float(h)
                guard let dist else {
                    out[i] = ground
                    continue
                }
                let f = exp(-dist[i] / reach)
                let around = ground * (1 - f) + 0.75 * f
                out[i] = m[i] * fallbackSubject + (1 - m[i]) * around
            }
        }
        return VisionMask(width: w, height: h, bytes: out.map { UInt8((min(max($0, 0), 1) * 255).rounded()) })
    }

    /// Distance (pixels, chamfer 3-4 approximation) from each pixel to the nearest one
    /// at least half in the matte; `.greatestFiniteMagnitude`-like large values when none is.
    static func distances(_ m: [Float], _ w: Int, _ h: Int) -> [Float] {
        let far: Float = 1e7
        var d = m.map { $0 >= 0.5 ? 0 : far }
        let (a, b): (Float, Float) = (1, 1.4142)
        for y in 0..<h {
            for x in 0..<w {
                var v = d[y * w + x]
                if x > 0 { v = min(v, d[y * w + x - 1] + a) }
                if y > 0 {
                    v = min(v, d[(y - 1) * w + x] + a)
                    if x > 0 { v = min(v, d[(y - 1) * w + x - 1] + b) }
                    if x < w - 1 { v = min(v, d[(y - 1) * w + x + 1] + b) }
                }
                d[y * w + x] = v
            }
        }
        for y in stride(from: h - 1, through: 0, by: -1) {
            for x in stride(from: w - 1, through: 0, by: -1) {
                var v = d[y * w + x]
                if x < w - 1 { v = min(v, d[y * w + x + 1] + a) }
                if y < h - 1 {
                    v = min(v, d[(y + 1) * w + x] + a)
                    if x < w - 1 { v = min(v, d[(y + 1) * w + x + 1] + b) }
                    if x > 0 { v = min(v, d[(y + 1) * w + x - 1] + b) }
                }
                d[y * w + x] = v
            }
        }
        return d
    }

    /// Bilinear sample of a `w × h` plane at normalized `(u, v)`, edges clamped.
    static func bilinear(_ p: [Float], _ w: Int, _ h: Int, _ u: Double, _ v: Double) -> Float {
        guard w > 0, h > 0, p.count == w * h else { return 0 }
        let fx = min(max(u * Double(w) - 0.5, 0), Double(w - 1))
        let fy = min(max(v * Double(h) - 0.5, 0), Double(h - 1))
        let x0 = Int(fx), y0 = Int(fy)
        let x1 = min(x0 + 1, w - 1), y1 = min(y0 + 1, h - 1)
        let tx = Float(fx - Double(x0)), ty = Float(fy - Double(y0))
        let top = p[y0 * w + x0] + (p[y0 * w + x1] - p[y0 * w + x0]) * tx
        let bot = p[y1 * w + x0] + (p[y1 * w + x1] - p[y1 * w + x0]) * tx
        return top + (bot - top) * ty
    }
}
