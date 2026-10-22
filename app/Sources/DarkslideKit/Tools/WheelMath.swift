import CoreGraphics
import Foundation

/// Color-balance wheel geometry. Wheel space: center (0,0), radius 1, y up;
/// hue 0° = red at the right, increasing counter-clockwise; saturation = radius.
public enum WheelMath {
    /// View point (top-left origin) in a square of side `size` → hue/saturation.
    public static func value(at point: CGPoint, size: CGFloat) -> (hue: Double, saturation: Double) {
        let r = Double(size) / 2
        guard r > 0 else { return (0, 0) }
        let x = (Double(point.x) - r) / r
        let y = (r - Double(point.y)) / r
        let sat = min(hypot(x, y), 1)
        guard sat > 1e-6 else { return (0, 0) }
        var hue = atan2(y, x) * 180 / .pi
        if hue < 0 { hue += 360 }
        return (hue, sat)
    }

    /// Inverse of `value(at:size:)`: puck position in view coordinates.
    public static func point(hue: Double, saturation: Double, size: CGFloat) -> CGPoint {
        let r = Double(size) / 2
        let s = min(max(saturation, 0), 1)
        let a = hue * .pi / 180
        return CGPoint(x: r + cos(a) * s * r, y: r - sin(a) * s * r)
    }

    /// HSV (hue in degrees, s and v 0...1) → RGB 0...1.
    public static func rgb(hue: Double, saturation s: Double, value v: Double) -> (r: Double, g: Double, b: Double) {
        var h = hue.truncatingRemainder(dividingBy: 360)
        if h < 0 { h += 360 }
        let c = v * s
        let x = c * (1 - abs((h / 60).truncatingRemainder(dividingBy: 2) - 1))
        let m = v - c
        let (r, g, b): (Double, Double, Double) = switch Int(h / 60) {
        case 0: (c, x, 0)
        case 1: (x, c, 0)
        case 2: (0, c, x)
        case 3: (0, x, c)
        case 4: (x, 0, c)
        default: (c, 0, x)
        }
        return (r + m, g + m, b + m)
    }

    /// Swatch color for a wheel value: the tint at its saturation, brightness from lightness.
    public static func previewColor(_ w: WheelValue) -> (r: Double, g: Double, b: Double) {
        rgb(hue: w.hue, saturation: min(max(w.saturation, 0), 1), value: min(max(0.75 + 0.25 * w.lightness, 0), 1))
    }
}
