import CoreGraphics
import Foundation
import Observation
import simd

/// How many full-resolution pixels a picker averages: a square centred on the pixel
/// under the cursor (Photoshop's and Capture One's "sample size").
public enum PickerSample: String, CaseIterable, Sendable, Identifiable {
    case point, box3, box5, average

    public var id: String { rawValue }

    /// Side of the square, in pixels.
    public var pixels: Int {
        switch self {
        case .point: 1
        case .box3: 3
        case .box5: 5
        case .average: 11
        }
    }

    public var label: String {
        switch self {
        case .point: "1 px"
        case .box3: "3×3"
        case .box5: "5×5"
        case .average: "11×11 average"
        }
    }
}

/// A pixel of the frame the loupe magnifies (the final frame at 1:1, or the oriented
/// image when the renderer doesn't develop geometry).
public struct LoupePixel: Hashable, Sendable {
    public var x: Int
    public var y: Int

    public init(x: Int, y: Int) {
        self.x = x
        self.y = y
    }
}

/// The eyedroppers' magnifier (white balance, colour editor, skin tone): a circle that
/// follows the cursor, magnifying the full-resolution develop around the pixel under
/// it, nearest-neighbour, with the sample area outlined and a readout below.
///
/// Observed only by the loupe overlay: the cursor moving never invalidates the viewer.
/// `ViewerModel` drives it (`updatePickerLoupe(at:)`): it renders a small 1:1 region
/// around the cursor through the pipeline and computes the readout.
@MainActor @Observable
public final class PickerLoupe {
    public enum Kind: Equatable, Sendable {
        case whiteBalance, colorRange, skinTone
    }

    /// A rendered 1:1 region around the cursor.
    public struct Region: Equatable, Sendable {
        public var frame: RenderedFrame
        /// Normalized rectangle of the frame space the region covers.
        public var rect: CGRect
        /// Size of the whole frame space in pixels (the region's frame is 1:1 with it).
        public var space: CGSize
        public var photo: PhotoID
        public var edit: EditState

        /// The region frame's pixel holding the frame-space pixel `p`, if it's inside.
        public func local(_ p: LoupePixel) -> (x: Int, y: Int)? {
            guard rect.width > 0, rect.height > 0, frame.width > 0, frame.height > 0 else { return nil }
            let u = (Double(p.x) + 0.5) / space.width, v = (Double(p.y) + 0.5) / space.height
            let x = Int(((u - rect.minX) / rect.width * Double(frame.width)).rounded(.down))
            let y = Int(((v - rect.minY) / rect.height * Double(frame.height)).rounded(.down))
            guard x >= 0, y >= 0, x < frame.width, y < frame.height else { return nil }
            return (x, y)
        }

        /// Whether the square of `radius` pixels around `p` is inside the region (or the
        /// region reaches the frame's edge there).
        func covers(_ p: LoupePixel, radius: Int) -> Bool {
            let x0 = Int((rect.minX * space.width).rounded()), y0 = Int((rect.minY * space.height).rounded())
            let x1 = Int((rect.maxX * space.width).rounded()), y1 = Int((rect.maxY * space.height).rounded())
            let w = Int(space.width.rounded()), h = Int(space.height.rounded())
            return (p.x - radius >= x0 || x0 == 0) && (p.x + radius < x1 || x1 >= w)
                && (p.y - radius >= y0 || y0 == 0) && (p.y + radius < y1 || y1 >= h)
        }
    }

    /// What the loupe says about the sample under the cursor.
    public struct Readout: Equatable, Sendable {
        /// Mean sRGB, 0–255.
        public var rgb: SIMD3<Int>
        /// A channel of a sampled pixel is at the display's white: not a usable neutral.
        public var clipped: Bool
        public var neutrality: String
        /// White balance only: what a click applies (nil while it's being computed).
        public var whiteBalance: WhiteBalanceEdit?
        /// White balance only: why there's no value (a black or clipped spot).
        public var whiteBalanceError: String?
    }

    /// Which picker it's for (nil: hidden).
    public private(set) var kind: Kind?
    /// Cursor in view points.
    public private(set) var cursor: CGPoint?
    /// The pane, in view points (the loupe flips to stay inside it).
    public private(set) var bounds: CGRect = .zero
    /// The frame-space pixel under the cursor.
    public private(set) var pixel: LoupePixel?
    public internal(set) var region: Region?
    public internal(set) var readout: Readout?
    /// Shown (stored apart from the cursor, so the viewer observes only this).
    public private(set) var shown = false
    /// Screen points per image pixel.
    public var magnification: Double = 9
    public var sample: PickerSample = .box5
    /// Bumped when a settled readout should be spoken (VoiceOver).
    public private(set) var announcement: String?

    nonisolated public static let diameter: CGFloat = 148
    nonisolated public static let magnificationRange: ClosedRange<Double> = 4...24
    /// Height of the readout under the circle.
    nonisolated public static let readoutHeight: CGFloat = 64

    public init() {}

    // MARK: - Driven by the viewer

    func show(_ kind: Kind, cursor: CGPoint, bounds: CGRect, pixel: LoupePixel) {
        if self.kind != kind { self.kind = kind }
        self.cursor = cursor
        if self.bounds != bounds { self.bounds = bounds }
        if self.pixel != pixel { self.pixel = pixel }
        if !shown { shown = true }
    }

    func hide() {
        guard shown || kind != nil else { return }
        shown = false
        kind = nil
        cursor = nil
        pixel = nil
        readout = nil
        announcement = nil
    }

    /// Scroll over the loupe: up magnifies, down shows more.
    public func scroll(by deltaY: Double) {
        guard deltaY.isFinite, deltaY != 0 else { return }
        magnification = min(max(magnification * pow(1.12, deltaY), Self.magnificationRange.lowerBound),
                            Self.magnificationRange.upperBound)
    }

    func announce(_ text: String) {
        if announcement != text { announcement = text }
    }

    /// Pixels from the centre to the circle's edge (plus one, partly visible).
    var visibleRadius: Int { Int((Self.diameter / 2 / magnification).rounded(.up)) + 1 }

    // MARK: - Pure (tested)

    /// Where the circle goes for a cursor at `cursor` in `bounds`: offset up and to the
    /// right so it never covers the pick point, flipped left/below near the edges, with
    /// room for the readout under it.
    nonisolated public static func placement(cursor: CGPoint, bounds: CGRect, diameter d: CGFloat = diameter,
                                             readout: CGFloat = readoutHeight, gap: CGFloat = 22) -> CGRect {
        var x = cursor.x + gap
        if x + d > bounds.maxX { x = cursor.x - gap - d }
        var y = cursor.y - gap - d - readout
        if y < bounds.minY { y = cursor.y + gap }
        // A pane too small for either side: keep it inside, the cursor may be covered.
        x = min(max(x, bounds.minX), max(bounds.minX, bounds.maxX - d))
        y = min(max(y, bounds.minY), max(bounds.minY, bounds.maxY - d - readout))
        return CGRect(x: x, y: y, width: d, height: d)
    }

    /// Mean sRGB (0…1) of the `n`×`n` pixels of `region` around `p`, and whether any of
    /// them has a channel at the display's white (≥ 254 of 255). Nil when `p` isn't in
    /// the region.
    nonisolated public static func measure(_ region: Region, at p: LoupePixel, sample n: Int)
        -> (rgb: SIMD3<Double>, clipped: Bool)? {
        guard let (cx, cy) = region.local(p) else { return nil }
        let f = region.frame
        let r = max(0, (n - 1) / 2)
        let u = CGPoint(x: (Double(cx) + 0.5) / Double(f.width), y: (Double(cy) + 0.5) / Double(f.height))
        guard let rgb = ColorMath.sample(f, at: u, radius: r) else { return nil }
        var clipped = false
        let bpp = f.format.bytesPerPixel
        f.withBytes { (b: UnsafeRawBufferPointer) in
            for y in max(cy - r, 0)...min(cy + r, f.height - 1) {
                for x in max(cx - r, 0)...min(cx + r, f.width - 1) {
                    let i = y * f.bytesPerRow + x * bpp
                    if f.format.isEightBit {
                        if b[i] >= 254 || b[i + 1] >= 254 || b[i + 2] >= 254 { clipped = true }
                    } else {
                        // In the display's primaries (P3: float frames draw into an EDR P3
                        // surface); a working-gamut frame while proofing is converted first.
                        let v = SIMD3((0..<3).map { Float(b.loadUnaligned(fromByteOffset: i + $0 * 2, as: Float16.self)) })
                        let d = ViewerColor.matrix(source: f.format, p3Target: true) * v
                        if max(d.x, max(d.y, d.z)) >= 0.995 { clipped = true }
                    }
                }
            }
        }
        return (rgb, clipped)
    }

    /// "Neutral", or the cast of an sRGB colour (0…1) in words, from its OKLab chroma.
    nonisolated public static func neutrality(_ rgb: SIMD3<Double>) -> String {
        let (a, b) = oklabAB(rgb)
        let chroma = (a * a + b * b).squareRoot()
        if chroma < 0.008 { return "Neutral" }
        let cast: String
        if abs(b) >= abs(a) {
            cast = b > 0 ? "warm" : "cool"
        } else {
            cast = a > 0 ? "magenta" : "green"
        }
        return chroma < 0.025 ? "Nearly neutral, slightly \(cast)" : "Not neutral: \(cast) cast"
    }

    /// OKLab a, b of an sRGB colour (0…1).
    nonisolated static func oklabAB(_ c: SIMD3<Double>) -> (a: Double, b: Double) {
        func lin(_ v: Double) -> Double {
            let x = min(max(v, 0), 1)
            return x <= 0.04045 ? x / 12.92 : pow((x + 0.055) / 1.055, 2.4)
        }
        let (r, g, b) = (lin(c.x), lin(c.y), lin(c.z))
        let l = cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b)
        let m = cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b)
        let s = cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b)
        return (1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
                0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s)
    }

    /// The readout as spoken by VoiceOver.
    nonisolated public static func spoken(_ r: Readout, kind: Kind) -> String {
        var parts = ["Red \(r.rgb.x), green \(r.rgb.y), blue \(r.rgb.z)"]
        if kind == .whiteBalance {
            if let wb = r.whiteBalance {
                parts.append("\(Int(wb.temperature)) Kelvin, tint \(Formatters.signed(wb.tint, decimals: 1))")
            } else if let e = r.whiteBalanceError {
                parts.append(e)
            }
        }
        parts.append(r.neutrality)
        if r.clipped { parts.append("Clipped: pick a neutral that isn't blown out") }
        return parts.joined(separator: ". ")
    }
}
