import CoreGraphics
import Foundation

/// Procedural "landscape" thumbnails, seeded by photo id so bursts look alike.
/// Honours cancellation and simulates decode latency, like the real cache would.
public struct MockImageProvider: ImageProvider {
    public var latency: Duration

    public init(latency: Duration = .milliseconds(15)) {
        self.latency = latency
    }

    public func thumbnail(for photo: Photo, maxPixelSize: Int) async throws -> CGImage {
        if latency > .zero { try await Task.sleep(for: latency) }
        try Task.checkCancellation()
        let long = max(16, maxPixelSize)
        let (w, h) = photo.meta.isPortrait
            ? (Int(Double(long) / 1.5), long) : (long, Int(Double(long) / 1.5))
        guard let img = Self.draw(seed: photo.id.raw, width: w, height: h) else {
            throw MockError.invalid("could not draw thumbnail")
        }
        return img
    }

    /// Photos in the same burst (ids / 4) share a scene with small variations.
    static func draw(seed: Int64, width: Int, height: Int) -> CGImage? {
        let cs = CGColorSpace(name: CGColorSpace.sRGB)!
        guard let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: cs, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue) else { return nil }
        var scene = SplitMix64(seed: UInt64(seed / 4) &* 7919)
        var frame = SplitMix64(seed: UInt64(seed) &* 104_729)
        let (w, h) = (CGFloat(width), CGFloat(height))
        func rgb(_ r: Double, _ g: Double, _ b: Double) -> CGColor { CGColor(colorSpace: cs, components: [r, g, b, 1])! }

        // Sky: palette by scene (dawn, noon, dusk, night, overcast).
        let palettes: [[(Double, Double, Double)]] = [
            [(0.98, 0.72, 0.45), (0.55, 0.60, 0.85)],
            [(0.62, 0.80, 0.97), (0.20, 0.45, 0.85)],
            [(0.99, 0.55, 0.30), (0.35, 0.18, 0.45)],
            [(0.10, 0.12, 0.25), (0.02, 0.03, 0.08)],
            [(0.78, 0.80, 0.82), (0.52, 0.56, 0.60)],
        ]
        let pal = palettes[Int(scene.next() % UInt64(palettes.count))]
        let exposure = 0.85 + frame.unit() * 0.3
        let sky = CGGradient(colorsSpace: cs, colors: pal.map { rgb($0.0 * exposure, $0.1 * exposure, $0.2 * exposure) } as CFArray,
                             locations: [0, 1])!
        ctx.drawLinearGradient(sky, start: CGPoint(x: 0, y: h * 0.3), end: CGPoint(x: 0, y: h), options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])

        // Sun or moon.
        let sunX = w * (0.2 + scene.unit() * 0.6) + CGFloat(frame.unit() - 0.5) * w * 0.02
        let sunY = h * (0.55 + scene.unit() * 0.3)
        let r = min(w, h) * (0.05 + scene.unit() * 0.05)
        ctx.setFillColor(rgb(1, 0.95, 0.8))
        ctx.fillEllipse(in: CGRect(x: sunX - r, y: sunY - r, width: 2 * r, height: 2 * r))

        // Mountain ridges, back to front, darker in front.
        let ridges = 3
        for layer in 0..<ridges {
            let base = h * (0.55 - CGFloat(layer) * 0.14)
            let amp = h * (0.12 + scene.unit() * 0.1)
            let tone = 0.45 - Double(layer) * 0.14
            let tint = pal[1]
            ctx.setFillColor(rgb(tint.0 * tone, tint.1 * tone + 0.03 * Double(layer), tint.2 * tone * 0.8))
            ctx.move(to: CGPoint(x: 0, y: 0))
            var x: CGFloat = 0
            var y = base
            ctx.addLine(to: CGPoint(x: 0, y: y))
            while x < w {
                x += w / CGFloat(6 + Int(scene.next() % 6))
                y = base + CGFloat(scene.unit() - 0.3) * amp
                ctx.addLine(to: CGPoint(x: min(x, w), y: y))
            }
            ctx.addLine(to: CGPoint(x: w, y: 0))
            ctx.closePath()
            ctx.fillPath()
        }
        return ctx.makeImage()
    }
}
