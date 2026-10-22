import Foundation

/// 256-bin RGB + luma histogram of a display frame, with clipping counts.
/// Computed on the GPU off the main thread (`HistogramEngine`); the CPU
/// binning here is the reference and the no-Metal fallback.
public struct Histogram: Equatable, Sendable {
    public enum Channel: CaseIterable, Sendable { case red, green, blue, luma }

    public private(set) var red = [UInt32](repeating: 0, count: 256)
    public private(set) var green = [UInt32](repeating: 0, count: 256)
    public private(set) var blue = [UInt32](repeating: 0, count: 256)
    public private(set) var luma = [UInt32](repeating: 0, count: 256)
    /// Pixels with any channel at 0 / at 255.
    public private(set) var shadowClipped = 0
    public private(set) var highlightClipped = 0
    public private(set) var pixelCount = 0

    public init() {}

    /// From precomputed bins (the GPU kernel in `HistogramEngine`).
    init(red: [UInt32], green: [UInt32], blue: [UInt32], luma: [UInt32], shadowClipped: Int, highlightClipped: Int,
         pixelCount: Int) {
        self.red = red
        self.green = green
        self.blue = blue
        self.luma = luma
        self.shadowClipped = shadowClipped
        self.highlightClipped = highlightClipped
        self.pixelCount = pixelCount
    }

    /// Bins an 8-bit frame in place (no copy), honouring padded rows. Float
    /// frames get their histogram from the pipeline and bin as empty here.
    public init(frame: RenderedFrame) {
        guard frame.format.isEightBit else {
            self.init()
            return
        }
        self.init()
        frame.withBytes { accumulate($0, width: frame.width, height: frame.height, bytesPerRow: frame.bytesPerRow) }
    }

    /// Packed RGBA bytes (`pixelCount` pixels, 4 bytes each).
    public init(rgba: Data, pixelCount: Int) {
        self.init()
        let n = min(pixelCount, rgba.count / 4)
        rgba.withUnsafeBytes { accumulate($0, width: n, height: n > 0 ? 1 : 0, bytesPerRow: n * 4) }
    }

    private mutating func accumulate(_ buf: UnsafeRawBufferPointer, width: Int, height: Int, bytesPerRow: Int) {
        var r = [UInt32](repeating: 0, count: 256), g = r, b = r, l = r
        var lo = 0, hi = 0
        let p = buf.bindMemory(to: UInt8.self)
        let rows = min(height, bytesPerRow > 0 ? p.count / bytesPerRow : 0)
        for y in 0..<rows {
            let row = y * bytesPerRow
            for x in 0..<width {
                let i = row + x * 4
                let (cr, cg, cb) = (Int(p[i]), Int(p[i + 1]), Int(p[i + 2]))
                r[cr] &+= 1
                g[cg] &+= 1
                b[cb] &+= 1
                // Rec. 709 luma weights in fixed point (sum 256).
                l[(54 * cr + 183 * cg + 19 * cb) >> 8] &+= 1
                if cr == 0 || cg == 0 || cb == 0 { lo += 1 }
                if cr == 255 || cg == 255 || cb == 255 { hi += 1 }
            }
        }
        red = r
        green = g
        blue = b
        luma = l
        shadowClipped = lo
        highlightClipped = hi
        pixelCount = rows * width
    }

    public func bins(_ c: Channel) -> [UInt32] {
        switch c {
        case .red: red
        case .green: green
        case .blue: blue
        case .luma: luma
        }
    }

    /// Bins scaled to 0...1 by the largest bin across RGB (square-root scaled so
    /// small populations stay visible, as photo apps do).
    public func normalized(_ c: Channel) -> [Double] {
        let peak = Double(max(red.max() ?? 0, green.max() ?? 0, blue.max() ?? 0, luma.max() ?? 0))
        guard peak > 0 else { return Array(repeating: 0, count: 256) }
        let s = peak.squareRoot()
        return bins(c).map { Double($0).squareRoot() / s }
    }

    public var highlightClipFraction: Double { pixelCount == 0 ? 0 : Double(highlightClipped) / Double(pixelCount) }
    public var shadowClipFraction: Double { pixelCount == 0 ? 0 : Double(shadowClipped) / Double(pixelCount) }
}
