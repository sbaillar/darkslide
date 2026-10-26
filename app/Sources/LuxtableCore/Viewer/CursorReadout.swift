import CoreGraphics
import Foundation
import Observation
import simd

/// Pixel value under the cursor. A separate observable object so hover updates
/// only redraw views that show the readout, never the Metal surface.
@MainActor @Observable
public final class CursorReadout {
    /// sRGB-encoded 0...1.
    public private(set) var rgb: SIMD3<Double>?
    /// CIE L*a*b* (D65).
    public private(set) var lab: SIMD3<Double>?
    /// Normalized image coordinate.
    public private(set) var position: CGPoint?
    /// Value pinned by the color-picker tool.
    public private(set) var pinned: SIMD3<Double>?

    public init() {}

    func update(rgb: SIMD3<Double>?, at position: CGPoint?) {
        guard rgb != self.rgb || position != self.position else { return }
        self.rgb = rgb
        self.position = position
        lab = rgb.map(ColorMath.lab(fromSRGB:))
    }

    func pin(_ rgb: SIMD3<Double>?) { pinned = rgb }

    /// "R 128  G 64  B 32" for the HUD.
    public var rgbText: String? {
        rgb.map { String(format: "R %3.0f  G %3.0f  B %3.0f", $0.x * 255, $0.y * 255, $0.z * 255) }
    }

    /// "L 54  a 8  b -12".
    public var labText: String? {
        // Round first (+ 0 turns -0 into 0) so neutral grey reads "a 0", not "a -0".
        lab.map { String(format: "L %.0f  a %.0f  b %.0f", $0.x.rounded() + 0, $0.y.rounded() + 0, $0.z.rounded() + 0) }
    }
}

/// sRGB ↔ Lab conversions (pure).
public enum ColorMath {
    public static func linear(_ v: Double) -> Double {
        v <= 0.04045 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4)
    }

    /// sRGB-encoded (0...1) → CIE L*a*b* with a D65 white.
    public static func lab(fromSRGB c: SIMD3<Double>) -> SIMD3<Double> {
        let r = linear(c.x), g = linear(c.y), b = linear(c.z)
        let x = 0.4124564 * r + 0.3575761 * g + 0.1804375 * b
        let y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b
        let z = 0.0193339 * r + 0.1191920 * g + 0.9503041 * b
        func f(_ t: Double) -> Double { t > 216.0 / 24389 ? cbrt(t) : (24389.0 / 27 * t + 16) / 116 }
        let fx = f(x / 0.95047), fy = f(y / 1.0), fz = f(z / 1.08883)
        return SIMD3(116 * fy - 16, 500 * (fx - fy), 200 * (fy - fz))
    }

    public static func encode(_ v: Double) -> Double {
        let c = min(max(v, 0), 1)
        return c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1 / 2.4) - 0.055
    }

    /// Linear Display P3 → sRGB-encoded 0...1 (clamped to SDR).
    public static func srgb(fromLinearP3 c: SIMD3<Double>) -> SIMD3<Double> {
        srgb(fromLinear: c, m: ViewerColor.p3ToSrgb)
    }

    /// Linear colour → sRGB-encoded, through `m` (the colour's primaries → sRGB).
    static func srgb(fromLinear c: SIMD3<Double>, m: simd_float3x3) -> SIMD3<Double> {
        let l = SIMD3(Double(m[0][0]) * c.x + Double(m[1][0]) * c.y + Double(m[2][0]) * c.z,
                      Double(m[0][1]) * c.x + Double(m[1][1]) * c.y + Double(m[2][1]) * c.z,
                      Double(m[0][2]) * c.x + Double(m[1][2]) * c.y + Double(m[2][2]) * c.z)
        return SIMD3(encode(l.x), encode(l.y), encode(l.z))
    }

    /// Average of a (2r+1)² patch of a frame at a normalized point, sRGB-encoded 0...1.
    /// 16F (linear P3) frames are converted to sRGB and clamped to SDR white.
    public static func sample(_ f: RenderedFrame, at u: CGPoint, radius: Int = 2) -> SIMD3<Double>? {
        guard f.width > 0, f.height > 0 else { return nil }
        let cx = min(max(Int(u.x * CGFloat(f.width)), 0), f.width - 1)
        let cy = min(max(Int(u.y * CGFloat(f.height)), 0), f.height - 1)
        var sum = SIMD3<Double>(0, 0, 0)
        var n = 0.0
        let isFloat = f.format.isFloat
        let toSrgb = ViewerColor.matrix(source: f.format, p3Target: false)
        f.withBytes { (p: UnsafeRawBufferPointer) in
            for y in max(cy - radius, 0)...min(cy + radius, f.height - 1) {
                for x in max(cx - radius, 0)...min(cx + radius, f.width - 1) {
                    if isFloat {
                        let i = y * f.bytesPerRow + x * 8
                        let h = (0..<3).map { Double(Float(p.loadUnaligned(fromByteOffset: i + $0 * 2, as: Float16.self))) }
                        sum += srgb(fromLinear: SIMD3(h[0], h[1], h[2]), m: toSrgb)
                    } else if f.format == .rgba8DisplayP3 {
                        // P3 primaries, sRGB transfer: to linear P3, then sRGB-encoded.
                        let i = y * f.bytesPerRow + x * 4
                        func lin(_ v: UInt8) -> Double {
                            let c = Double(v) / 255
                            return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
                        }
                        sum += srgb(fromLinearP3: SIMD3(lin(p[i]), lin(p[i + 1]), lin(p[i + 2])))
                    } else {
                        let i = y * f.bytesPerRow + x * 4
                        sum += SIMD3(Double(p[i]), Double(p[i + 1]), Double(p[i + 2])) / 255
                    }
                    n += 1
                }
            }
        }
        return sum / n
    }

    /// 8-bit sRGB copy of a 16F frame (histogram, navigator); 8-bit frames pass through.
    public static func sdrFrame(_ f: RenderedFrame) -> RenderedFrame {
        guard f.format.isFloat else { return f }
        var out = [UInt8](repeating: 255, count: f.width * f.height * 4)
        // Encode through a LUT over half-float bit patterns would be faster; this runs off-main.
        let m = ViewerColor.matrix(source: f.format, p3Target: false)
        f.withBytes { (p: UnsafeRawBufferPointer) in
            for y in 0..<f.height {
                for x in 0..<f.width {
                    let i = y * f.bytesPerRow + x * 8
                    let r = Float(p.loadUnaligned(fromByteOffset: i, as: Float16.self))
                    let g = Float(p.loadUnaligned(fromByteOffset: i + 2, as: Float16.self))
                    let b = Float(p.loadUnaligned(fromByteOffset: i + 4, as: Float16.self))
                    let l = m * SIMD3(r, g, b)
                    let o = (y * f.width + x) * 4
                    out[o] = UInt8((encode(Double(l.x)) * 255).rounded())
                    out[o + 1] = UInt8((encode(Double(l.y)) * 255).rounded())
                    out[o + 2] = UInt8((encode(Double(l.z)) * 255).rounded())
                }
            }
        }
        return RenderedFrame(width: f.width, height: f.height, rgba: Data(out))
    }
}
