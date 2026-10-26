import CoreGraphics
import Foundation

/// Synthetic test images drawn in code (no bundled binaries).
enum TestImages {
    static func make(_ w: Int, _ h: Int, _ draw: (CGContext) -> Void) -> CGImage {
        let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        draw(ctx)
        return ctx.makeImage()!
    }

    static func solid(_ w: Int, _ h: Int, gray: CGFloat) -> CGImage {
        make(w, h) { c in
            c.setFillColor(CGColor(srgbRed: gray, green: gray, blue: gray, alpha: 1))
            c.fill(CGRect(x: 0, y: 0, width: w, height: h))
        }
    }

    static func color(_ w: Int, _ h: Int, r: CGFloat, g: CGFloat, b: CGFloat) -> CGImage {
        make(w, h) { c in
            c.setFillColor(CGColor(srgbRed: r, green: g, blue: b, alpha: 1))
            c.fill(CGRect(x: 0, y: 0, width: w, height: h))
        }
    }

    static func checkerboard(_ w: Int, _ h: Int, cell: Int = 4) -> CGImage {
        make(w, h) { c in
            c.setFillColor(CGColor(gray: 1, alpha: 1))
            c.fill(CGRect(x: 0, y: 0, width: w, height: h))
            c.setFillColor(CGColor(gray: 0, alpha: 1))
            for y in stride(from: 0, to: h, by: cell) {
                for x in stride(from: 0, to: w, by: cell) where (x / cell + y / cell) % 2 == 0 {
                    c.fill(CGRect(x: x, y: y, width: cell, height: cell))
                }
            }
        }
    }

    /// Sky over sea with the horizon tilted by `degrees` (positive = rising to the right).
    static func horizon(_ w: Int, _ h: Int, degrees: Double) -> CGImage {
        make(w, h) { c in
            c.setFillColor(CGColor(srgbRed: 0.55, green: 0.75, blue: 0.95, alpha: 1))
            c.fill(CGRect(x: 0, y: 0, width: w, height: h))
            c.translateBy(x: CGFloat(w) / 2, y: CGFloat(h) / 2)
            c.rotate(by: CGFloat(degrees * .pi / 180))
            c.setFillColor(CGColor(srgbRed: 0.05, green: 0.2, blue: 0.35, alpha: 1))
            c.fill(CGRect(x: -CGFloat(w), y: -CGFloat(h) * 2, width: CGFloat(w) * 2, height: CGFloat(h) * 2))
        }
    }

    /// Dark frame with one bright disc centered at (cx, cy) in normalized, bottom-left coordinates.
    static func blob(_ w: Int, _ h: Int, cx: CGFloat, cy: CGFloat, radius: CGFloat = 0.1) -> CGImage {
        make(w, h) { c in
            c.setFillColor(CGColor(gray: 0.08, alpha: 1))
            c.fill(CGRect(x: 0, y: 0, width: w, height: h))
            c.setFillColor(CGColor(srgbRed: 1, green: 0.3, blue: 0.1, alpha: 1))
            let r = radius * CGFloat(min(w, h))
            c.fillEllipse(in: CGRect(x: cx * CGFloat(w) - r, y: cy * CGFloat(h) - r, width: 2 * r, height: 2 * r))
        }
    }

    /// Soft vertical gradient (no edges): the "out of focus" case.
    static func gradient(_ w: Int, _ h: Int) -> CGImage {
        make(w, h) { c in
            let g = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB)!,
                               colors: [CGColor(gray: 0.2, alpha: 1), CGColor(gray: 0.7, alpha: 1)] as CFArray, locations: [0, 1])!
            c.drawLinearGradient(g, start: .zero, end: CGPoint(x: 0, y: h), options: [])
        }
    }

    /// A flat cartoon face (skin oval, eyes with pupils, brows, nose, mouth), big enough
    /// for Vision's face detector. `eyesOpen: false` draws the eyes as thin lines.
    static func face(_ size: Int = 512, eyesOpen: Bool = true) -> CGImage {
        make(size, size) { c in
            let s = CGFloat(size)
            func rgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat) -> CGColor { CGColor(srgbRed: r, green: g, blue: b, alpha: 1) }
            c.setFillColor(rgb(0.35, 0.45, 0.55))
            c.fill(CGRect(x: 0, y: 0, width: s, height: s))
            c.setFillColor(rgb(0.25, 0.17, 0.12)) // hair
            c.fillEllipse(in: CGRect(x: s * 0.22, y: s * 0.35, width: s * 0.56, height: s * 0.6))
            c.setFillColor(rgb(0.93, 0.76, 0.62)) // skin
            c.fillEllipse(in: CGRect(x: s * 0.27, y: s * 0.15, width: s * 0.46, height: s * 0.62))
            for x in [0.38, 0.62] {
                let cx = s * x, cy = s * 0.52
                if eyesOpen {
                    c.setFillColor(rgb(1, 1, 1))
                    c.fillEllipse(in: CGRect(x: cx - s * 0.055, y: cy - s * 0.025, width: s * 0.11, height: s * 0.05))
                    c.setFillColor(rgb(0.2, 0.12, 0.06))
                    c.fillEllipse(in: CGRect(x: cx - s * 0.022, y: cy - s * 0.022, width: s * 0.044, height: s * 0.044))
                } else {
                    c.setFillColor(rgb(0.35, 0.22, 0.15))
                    c.fill(CGRect(x: cx - s * 0.055, y: cy - s * 0.004, width: s * 0.11, height: s * 0.008))
                }
                c.setFillColor(rgb(0.3, 0.2, 0.12)) // brow
                c.fill(CGRect(x: cx - s * 0.06, y: cy + s * 0.06, width: s * 0.12, height: s * 0.015))
            }
            c.setFillColor(rgb(0.82, 0.62, 0.5)) // nose
            c.fillEllipse(in: CGRect(x: s * 0.475, y: s * 0.38, width: s * 0.05, height: s * 0.08))
            c.setFillColor(rgb(0.7, 0.3, 0.3)) // mouth
            c.fillEllipse(in: CGRect(x: s * 0.42, y: s * 0.26, width: s * 0.16, height: s * 0.04))
        }
    }
}
