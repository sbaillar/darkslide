import CoreGraphics
import Foundation

/// One detected face with the culling-relevant signals.
public struct FaceInfo: Sendable, Equatable {
    /// Normalized, Vision coordinates (origin bottom-left).
    public var boundingBox: CGRect
    /// Vision capture quality 0...1 (higher = sharper, better lit, better pose); `nil` if unavailable.
    public var captureQuality: Double?
    /// Eye aspect ratios; `nil` when the landmark was missing.
    public var leftEyeAspect: Double?
    public var rightEyeAspect: Double?

    public init(boundingBox: CGRect, captureQuality: Double?, leftEyeAspect: Double?, rightEyeAspect: Double?) {
        self.boundingBox = boundingBox
        self.captureQuality = captureQuality
        self.leftEyeAspect = leftEyeAspect
        self.rightEyeAspect = rightEyeAspect
    }

    /// `true`/`false` when both eyes were measured, `nil` otherwise.
    public var eyesOpen: Bool? { EyeMetrics.eyesOpen(left: leftEyeAspect, right: rightEyeAspect) }
}

/// Eye-openness from landmark contours.
public enum EyeMetrics {
    /// Below this height/width ratio an eye counts as closed. Open eyes measure
    /// roughly 0.25–0.4 on Vision's eye contours; closed ones collapse to < 0.15.
    public static let closedThreshold = 0.18

    /// Eye aspect ratio: the contour's height over its width, measured along the
    /// eye's own axis (the line through its two farthest-apart points) so a
    /// tilted head doesn't read as an open eye. `nil` for fewer than 3 points or
    /// a degenerate contour.
    public static func aspectRatio(_ points: [CGPoint]) -> Double? {
        guard points.count >= 3 else { return nil }
        var (a, b, best) = (points[0], points[0], -1.0)
        for i in 0..<points.count {
            for j in (i + 1)..<points.count {
                let d = hypot(points[i].x - points[j].x, points[i].y - points[j].y)
                if d > best { (a, b, best) = (points[i], points[j], d) }
            }
        }
        guard best > 1e-9 else { return nil }
        let (ux, uy) = ((b.x - a.x) / best, (b.y - a.y) / best)
        var lo = Double.infinity, hi = -Double.infinity
        for p in points {
            // Signed distance from the axis (perpendicular component).
            let d = Double((p.x - a.x) * -uy + (p.y - a.y) * ux)
            lo = min(lo, d)
            hi = max(hi, d)
        }
        return (hi - lo) / Double(best)
    }

    /// Probability (0...1) that both eyes are open, for lt-ai's `FaceSignal.eyes_open`:
    /// a linear ramp of the mean ratio from `closedRatio` (0) to `openRatio` (1).
    /// `nil` when either eye is unknown.
    public static func openProbability(left: Double?, right: Double?, closedRatio: Double = 0.12,
                                       openRatio: Double = 0.24) -> Double? {
        guard let left, let right, openRatio > closedRatio else { return nil }
        return min(max(((left + right) / 2 - closedRatio) / (openRatio - closedRatio), 0), 1)
    }

    /// Open if the mean of both ratios clears the threshold; `nil` if either is unknown.
    public static func eyesOpen(left: Double?, right: Double?, threshold: Double = closedThreshold) -> Bool? {
        guard let left, let right else { return nil }
        return (left + right) / 2 >= threshold
    }
}

/// Composition helpers.
public enum SmartCrop {
    /// The largest rect of `aspect` (width/height, in pixels) inside an image of
    /// `imageSize`, centered on the union of `salient` rects (normalized) and
    /// shifted to stay inside the image. Returned normalized, same coordinate
    /// convention as the input. With no salient rects it centers on the image.
    public static func suggest(aspect: Double, imageSize: CGSize, salient: [CGRect]) -> CGRect {
        guard aspect > 0, imageSize.width > 0, imageSize.height > 0 else { return CGRect(x: 0, y: 0, width: 1, height: 1) }
        let imgAspect = imageSize.width / imageSize.height
        // Crop size in normalized units.
        let (w, h): (Double, Double) = aspect >= imgAspect ? (1, imgAspect / aspect) : (aspect / imgAspect, 1)
        let focus = salient.reduce(CGRect.null) { $0.union($1) }
        let center = focus.isNull ? CGPoint(x: 0.5, y: 0.5) : CGPoint(x: focus.midX, y: focus.midY)
        let x = min(max(center.x - w / 2, 0), 1 - w)
        let y = min(max(center.y - h / 2, 0), 1 - h)
        return CGRect(x: x, y: y, width: w, height: h)
    }

    /// Parse "4:5", "16x9", "1/1" into width/height. `nil` for anything else.
    public static func parseAspect(_ s: String) -> Double? {
        let parts = s.split(whereSeparator: { ":x/×".contains($0) }).map { Double($0.trimmingCharacters(in: .whitespaces)) }
        guard parts.count == 2, let a = parts[0], let b = parts[1], a > 0, b > 0 else { return nil }
        return a / b
    }
}
