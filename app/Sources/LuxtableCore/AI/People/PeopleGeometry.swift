import CoreGraphics
import Foundation

/// 8-bit RGBA pixels of an upright (E-frame) image, top-left origin, row-major.
struct PixelImage: Sendable, Equatable {
    let width: Int
    let height: Int
    let rgba: [UInt8]

    init(width: Int, height: Int, rgba: [UInt8]) {
        self.width = width
        self.height = height
        self.rgba = rgba
    }

    /// Draw `image` into sRGB RGBA8 (nil only when Core Graphics can't make the context).
    init?(_ image: CGImage) {
        let w = image.width, h = image.height
        var px = [UInt8](repeating: 0, count: w * h * 4)
        let ok: Bool = px.withUnsafeMutableBytes { buf in
            guard let ctx = CGContext(data: buf.baseAddress, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                                      space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return false }
            ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
            return true
        }
        guard ok else { return nil }
        self.init(width: w, height: h, rgba: px)
    }

    /// Bilinear RGB at pixel-centre coordinates (`x + 0.5` is the centre of column x);
    /// outside the image the nearest edge pixel.
    func sample(_ x: Double, _ y: Double) -> SIMD3<Float> {
        let fx = min(max(x - 0.5, 0), Double(width - 1)), fy = min(max(y - 0.5, 0), Double(height - 1))
        let x0 = Int(fx), y0 = Int(fy)
        let x1 = min(x0 + 1, width - 1), y1 = min(y0 + 1, height - 1)
        let ax = Float(fx - Double(x0)), ay = Float(fy - Double(y0))
        func px(_ x: Int, _ y: Int) -> SIMD3<Float> {
            let i = (y * width + x) * 4
            return SIMD3(Float(rgba[i]), Float(rgba[i + 1]), Float(rgba[i + 2]))
        }
        let top = px(x0, y0) * (1 - ax) + px(x1, y0) * ax
        let bottom = px(x0, y1) * (1 - ax) + px(x1, y1) * ax
        return top * (1 - ay) + bottom * ay
    }
}

/// Vision's coordinates (normalized, bottom-left origin) to E-frame pixels (top-left).
enum VisionGeometry {
    static func point(_ p: CGPoint, width: Int, height: Int) -> CGPoint {
        CGPoint(x: p.x * CGFloat(width), y: (1 - p.y) * CGFloat(height))
    }

    static func rect(_ r: CGRect, width: Int, height: Int) -> CGRect {
        CGRect(x: r.minX * CGFloat(width), y: (1 - r.maxY) * CGFloat(height),
               width: r.width * CGFloat(width), height: r.height * CGFloat(height))
    }

    /// Landmark points already in image pixels with Vision's bottom-left origin
    /// (`pointsInImage(imageSize:)`) flipped to top-left.
    static func flipped(_ p: CGPoint, height: Int) -> CGPoint {
        CGPoint(x: p.x, y: CGFloat(height) - p.y)
    }

    static func centroid(_ pts: [CGPoint]) -> CGPoint? {
        guard !pts.isEmpty else { return nil }
        let s = pts.reduce(CGPoint.zero) { CGPoint(x: $0.x + $1.x, y: $0.y + $1.y) }
        return CGPoint(x: s.x / CGFloat(pts.count), y: s.y / CGFloat(pts.count))
    }
}

/// An upright square crop around a face for face parsing (CelebAMask-HQ framing: the
/// face box is about 58 % of the crop, a little below its centre), rotated by the face's
/// roll so the model sees it upright. Maps crop pixels to frame pixels and back.
struct FaceCrop: Equatable, Sendable {
    /// Crop centre in frame pixels.
    var center: CGPoint
    /// Crop side in frame pixels.
    var side: Double
    /// Clockwise turn of the face in the frame (radians): the crop's "up" points along
    /// (sin a, -cos a) in frame pixels.
    var angle: Double
    /// Model input side (pixels).
    var size: Int

    /// Face side / crop side in CelebAMask-HQ's aligned crops.
    static let faceFraction = 0.58
    /// The face sits this share of the crop side below the crop centre.
    static let faceOffset = 0.08

    /// The crop for a face box (frame pixels) turned by `angle`.
    static func around(face box: CGRect, angle: Double, size: Int = 512) -> FaceCrop {
        let side = Double(max(box.width, box.height)) / faceFraction
        let up = CGPoint(x: sin(angle), y: -cos(angle))
        let c = CGPoint(x: box.midX + up.x * side * faceOffset, y: box.midY + up.y * side * faceOffset)
        return FaceCrop(center: c, side: side, angle: angle, size: size)
    }

    /// The face's clockwise turn from its landmarks: "up" runs from the mouth to the
    /// middle of the eyes, whatever the orientation (also upside down). Nil without them.
    static func angle(leftEye: [CGPoint], rightEye: [CGPoint], mouth: [CGPoint]) -> Double? {
        guard let l = VisionGeometry.centroid(leftEye), let r = VisionGeometry.centroid(rightEye),
              let m = VisionGeometry.centroid(mouth) else { return nil }
        let mid = CGPoint(x: (l.x + r.x) / 2, y: (l.y + r.y) / 2)
        let up = CGPoint(x: mid.x - m.x, y: mid.y - m.y)
        guard hypot(up.x, up.y) > 1e-6 else { return nil }
        return atan2(Double(up.x), Double(-up.y))
    }

    /// Frame pixel of crop pixel `p` (both continuous, top-left origin).
    func toFrame(_ p: CGPoint) -> CGPoint {
        let s = side / Double(size)
        let u = (Double(p.x) - Double(size) / 2) * s, v = (Double(p.y) - Double(size) / 2) * s
        let c = cos(angle), sn = sin(angle)
        return CGPoint(x: Double(center.x) + u * c - v * sn, y: Double(center.y) + u * sn + v * c)
    }

    /// Crop pixel of frame pixel `p` (inverse of `toFrame`).
    func toCrop(_ p: CGPoint) -> CGPoint {
        let dx = Double(p.x - center.x), dy = Double(p.y - center.y)
        let c = cos(angle), sn = sin(angle)
        let u = dx * c + dy * sn, v = -dx * sn + dy * c
        let s = Double(size) / side
        return CGPoint(x: u * s + Double(size) / 2, y: v * s + Double(size) / 2)
    }

    /// Frame pixels the crop covers (its rotated square's bounding box), clipped to `w`×`h`.
    func frameBounds(width w: Int, height h: Int) -> (x0: Int, y0: Int, x1: Int, y1: Int)? {
        let n = CGFloat(size)
        let corners = [CGPoint(x: 0, y: 0), CGPoint(x: n, y: 0), CGPoint(x: n, y: n), CGPoint(x: 0, y: n)].map(toFrame)
        let x0 = max(0, Int(floor(corners.map(\.x).min()!))), x1 = min(w, Int(ceil(corners.map(\.x).max()!)))
        let y0 = max(0, Int(floor(corners.map(\.y).min()!))), y1 = min(h, Int(ceil(corners.map(\.y).max()!)))
        return x0 < x1 && y0 < y1 ? (x0, y0, x1, y1) : nil
    }

    /// The model input: the image resampled into the upright crop (RGBA, opaque).
    func extract(from image: PixelImage) -> [UInt8] {
        extract(from: image, region: CGRect(x: 0, y: 0, width: 1, height: 1), frameWidth: image.width,
                frameHeight: image.height)
    }

    /// The model input sampled from `image`, a render of `region` (normalized) of the
    /// `frameWidth × frameHeight` frame the crop is in (e.g. a high-resolution render of
    /// just the face).
    func extract(from image: PixelImage, region: CGRect, frameWidth fw: Int, frameHeight fh: Int) -> [UInt8] {
        let sx = Double(image.width) / (Double(region.width) * Double(fw))
        let sy = Double(image.height) / (Double(region.height) * Double(fh))
        let ox = Double(region.minX) * Double(fw), oy = Double(region.minY) * Double(fh)
        var out = [UInt8](repeating: 255, count: size * size * 4)
        for v in 0..<size {
            for u in 0..<size {
                let f = toFrame(CGPoint(x: Double(u) + 0.5, y: Double(v) + 0.5))
                let c = image.sample((Double(f.x) - ox) * sx, (Double(f.y) - oy) * sy)
                let i = (v * size + u) * 4
                out[i] = UInt8(min(max(c.x.rounded(), 0), 255))
                out[i + 1] = UInt8(min(max(c.y.rounded(), 0), 255))
                out[i + 2] = UInt8(min(max(c.z.rounded(), 0), 255))
            }
        }
        return out
    }
}

/// Polygons rasterized into soft (anti-aliased by 4×4 supersampling) frame masks.
enum PolygonRaster {
    /// Even-odd point-in-polygon.
    static func contains(_ poly: [CGPoint], _ p: CGPoint) -> Bool {
        guard poly.count >= 3 else { return false }
        var inside = false
        var j = poly.count - 1
        for i in 0..<poly.count {
            let a = poly[i], b = poly[j]
            if (a.y > p.y) != (b.y > p.y), p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x { inside.toggle() }
            j = i
        }
        return inside
    }

    /// Max of `mask` and the polygon's coverage (0...255), in place.
    static func fill(_ poly: [CGPoint], into mask: inout [UInt8], width w: Int, height h: Int) {
        guard poly.count >= 3 else { return }
        let x0 = max(0, Int(floor(poly.map(\.x).min()!))), x1 = min(w - 1, Int(ceil(poly.map(\.x).max()!)))
        let y0 = max(0, Int(floor(poly.map(\.y).min()!))), y1 = min(h - 1, Int(ceil(poly.map(\.y).max()!)))
        guard x0 <= x1, y0 <= y1 else { return }
        for y in y0...y1 {
            for x in x0...x1 {
                var hits = 0
                for sy in 0..<4 {
                    for sx in 0..<4 where contains(poly, CGPoint(x: Double(x) + (Double(sx) + 0.5) / 4,
                                                                 y: Double(y) + (Double(sy) + 0.5) / 4)) {
                        hits += 1
                    }
                }
                let v = UInt8(hits * 255 / 16)
                if v > mask[y * w + x] { mask[y * w + x] = v }
            }
        }
    }
}
