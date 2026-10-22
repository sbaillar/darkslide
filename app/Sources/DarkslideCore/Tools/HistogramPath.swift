import CoreGraphics
import Foundation

/// Geometry helpers for drawing histograms (pure, so they can be tested).
public enum HistogramPath {
    /// Clipping indicator threshold: 0.1 % of pixels.
    public static let clipThreshold = 0.001

    /// A closed polygon for `values` (0...1) in a `width`×`height` box, top-left origin:
    /// starts and ends on the baseline so it can be filled.
    public static func polygon(_ values: [Double], width: CGFloat, height: CGFloat) -> [CGPoint] {
        guard values.count > 1 else { return [] }
        let dx = width / CGFloat(values.count - 1)
        var pts = [CGPoint(x: 0, y: height)]
        pts.reserveCapacity(values.count + 2)
        for (i, v) in values.enumerated() {
            pts.append(CGPoint(x: CGFloat(i) * dx, y: height - CGFloat(min(max(v, 0), 1)) * height))
        }
        pts.append(CGPoint(x: width, y: height))
        return pts
    }

    /// Curve samples (0...1 each) as an open polyline, y up in curve space → top-left view space.
    public static func polyline(_ samples: [Double], width: CGFloat, height: CGFloat) -> [CGPoint] {
        guard samples.count > 1 else { return [] }
        let dx = width / CGFloat(samples.count - 1)
        return samples.enumerated().map { CGPoint(x: CGFloat($0) * dx, y: height - CGFloat($1) * height) }
    }
}

extension Histogram {
    public var clipsHighlights: Bool { highlightClipFraction > HistogramPath.clipThreshold }
    public var clipsShadows: Bool { shadowClipFraction > HistogramPath.clipThreshold }
}

/// Text under the histogram: clipping percentages and the cursor readout.
public enum HistogramReadout {
    /// "0.12 %", "<0.01 %", "0 %".
    public static func clip(_ fraction: Double) -> String {
        guard fraction.isFinite, fraction > 0 else { return "0 %" }
        let pct = fraction * 100
        return pct < 0.01 ? "<0.01 %" : String(format: pct < 10 ? "%.2f %%" : "%.1f %%", pct)
    }

    /// 0...1 sRGB → 8-bit values as photographers read them.
    public static func rgb(_ r: Double, _ g: Double, _ b: Double) -> [String] {
        [r, g, b].map { String(Safe.clampedInt((min(max($0, 0), 1) * 255))) }
    }
}
