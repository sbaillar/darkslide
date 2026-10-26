import CoreGraphics
import Foundation

/// Pixel statistics of a (small) image, used by culling and auto adjust.
///
/// The image is downsampled into an sRGB RGBA8 buffer of at most `maxEdge`
/// pixels on the long edge first, so the cost is bounded (~65k pixels at 256)
/// no matter how large the input is.
public struct ImageStatistics: Sendable, Equatable {
    public var width: Int
    public var height: Int
    /// Mean linear-light RGB (0...1).
    public var meanLinearRGB: SIMD3<Double>
    /// Mean sRGB-encoded luma (0...1).
    public var meanLuma: Double
    /// Fraction of pixels with luma ≥ 250/255 and ≤ 5/255.
    public var highlightClip: Double
    public var shadowClip: Double
    /// Variance of the 4-neighbour Laplacian on 8-bit luma: a standard focus measure.
    public var laplacianVariance: Double

    public init(width: Int, height: Int, meanLinearRGB: SIMD3<Double>, meanLuma: Double, highlightClip: Double,
                shadowClip: Double, laplacianVariance: Double) {
        self.width = width
        self.height = height
        self.meanLinearRGB = meanLinearRGB
        self.meanLuma = meanLuma
        self.highlightClip = highlightClip
        self.shadowClip = shadowClip
        self.laplacianVariance = laplacianVariance
    }

    /// Downsample `image` and measure it. Returns `nil` for an empty image.
    public init?(image: CGImage, maxEdge: Int = 256) {
        guard let (rgba, w, h) = Self.rgba8(image, maxEdge: maxEdge) else { return nil }
        self.init(rgba: rgba, width: w, height: h)
    }

    /// Measure a tightly packed RGBA8 sRGB buffer.
    public init(rgba: [UInt8], width: Int, height: Int) {
        precondition(rgba.count >= width * height * 4, "buffer too small")
        let n = width * height
        var luma = [Double](repeating: 0, count: n)
        var sum = SIMD3<Double>(0, 0, 0)
        var lumaSum = 0.0, hi = 0, lo = 0
        let lut = Self.linearLUT
        for i in 0..<n {
            let (r, g, b) = (Int(rgba[i * 4]), Int(rgba[i * 4 + 1]), Int(rgba[i * 4 + 2]))
            sum += SIMD3(lut[r], lut[g], lut[b])
            let y = 0.2126 * Double(r) + 0.7152 * Double(g) + 0.0722 * Double(b)
            luma[i] = y
            lumaSum += y
            if y >= 250 { hi += 1 }
            if y <= 5 { lo += 1 }
        }
        let count = Double(max(n, 1))
        self.init(width: width, height: height, meanLinearRGB: sum / count, meanLuma: lumaSum / count / 255,
                  highlightClip: Double(hi) / count, shadowClip: Double(lo) / count,
                  laplacianVariance: Self.laplacianVariance(luma, width: width, height: height))
    }

    /// Variance of the discrete Laplacian (interior pixels only). 0 for images under 3×3.
    public static func laplacianVariance(_ luma: [Double], width: Int, height: Int) -> Double {
        guard width >= 3, height >= 3 else { return 0 }
        var sum = 0.0, sumSq = 0.0
        for y in 1..<(height - 1) {
            let row = y * width
            for x in 1..<(width - 1) {
                let i = row + x
                let l = luma[i - 1] + luma[i + 1] + luma[i - width] + luma[i + width] - 4 * luma[i]
                sum += l
                sumSq += l * l
            }
        }
        let n = Double((width - 2) * (height - 2))
        let mean = sum / n
        return max(0, sumSq / n - mean * mean)
    }

    static let linearLUT: [Double] = (0..<256).map { i in
        let v = Double(i) / 255
        return v <= 0.040_45 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4)
    }

    /// Draw `image` into an sRGB RGBA8 buffer no larger than `maxEdge`.
    static func rgba8(_ image: CGImage, maxEdge: Int) -> ([UInt8], Int, Int)? {
        guard image.width > 0, image.height > 0 else { return nil }
        let scale = min(1, Double(maxEdge) / Double(max(image.width, image.height)))
        let w = max(1, Int((Double(image.width) * scale).rounded()))
        let h = max(1, Int((Double(image.height) * scale).rounded()))
        var buf = [UInt8](repeating: 0, count: w * h * 4)
        let ok = buf.withUnsafeMutableBytes { raw -> Bool in
            guard let ctx = CGContext(data: raw.baseAddress, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                                      space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                      bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { return false }
            ctx.interpolationQuality = .medium
            ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
            return true
        }
        return ok ? (buf, w, h) : nil
    }
}
