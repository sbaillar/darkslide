import CoreGraphics
import Foundation
import SwiftUI
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// H2: a radial gradient renders where the canvas draws it. ds-core/ds-pipeline measure
/// radii in long-edge units on both axes; the overlay, the handles and a new drag use
/// the same units, so on portrait and landscape photos (and a turned ellipse) the
/// darkened pixels are the ones inside the drawn ellipse.
@Suite(.serialized) struct RadialGradientGoldenTests {
    /// Luma of an 8-bit frame, tightly packed.
    static func luma(_ f: RenderedFrame) -> [Double] {
        f.withBytes { p in
            var out = [Double](repeating: 0, count: f.width * f.height)
            for y in 0..<f.height {
                for x in 0..<f.width {
                    let i = y * f.bytesPerRow + x * 4
                    out[y * f.width + x] = (Double(p[i]) + Double(p[i + 1]) + Double(p[i + 2])) / 3
                }
            }
            return out
        }
    }

    /// Distance (in long edges) of `p` from the ellipse's outline: < 0 inside, > 0 outside.
    static func outline(_ p: CGPoint, center c: CGPoint, radius r: CGSize, angle: Double, size: CGSize) -> Double {
        let long = max(size.width, size.height)
        let (dx, dy) = ((p.x - c.x) / long, (p.y - c.y) / long)
        let a = angle * .pi / 180
        let qx = cos(a) * dx + sin(a) * dy, qy = -sin(a) * dx + cos(a) * dy
        return (hypot(qx / r.width, qy / r.height) - 1) * min(r.width, r.height)
    }

    @Test func renderedRadialMatchesTheOverlayOnPortraitAndLandscape() async throws {
        let dev = DevelopRenderer()
        var base = EditState()
        base.lens.enabled = false // mask coordinates map straight onto the oriented frame
        var seen: Set<Bool> = [] // portrait?
        for (i, url) in sampleARWs().enumerated() {
            let photo = Photo(id: PhotoID(Int64(9400 + i)), filename: url.lastPathComponent, meta: samplePhoto().meta, url: url)
            let request = RenderRequest(maxPixelSize: 360, quality: .preview, geometry: .oriented)
            let plain = try await dev.render(photo, edit: base, request: request)
            let portrait = plain.height > plain.width
            guard !seen.contains(portrait) else { continue }
            seen.insert(portrait)
            let size = CGSize(width: plain.width, height: plain.height)
            let lum0 = Self.luma(plain)
            for angle in [0.0, 30.0] {
                let center = CGPoint(x: 0.5, y: 0.45), radius = CGSize(width: 0.3, height: 0.15)
                var e = base
                e.layers = [LayerEdit(id: 1, name: "R", exposureEV: -4, mask: MaskEdit(shapes: [
                    .radial(.add, center: center, radius: radius, angle: angle, feather: 0),
                ]))]
                let lit = try await dev.render(photo, edit: e, request: request)
                #expect(lit.width == plain.width && lit.height == plain.height)
                let lum1 = Self.luma(lit)
                let c = CGPoint(x: center.x * size.width, y: center.y * size.height)
                let path = ViewerOverlayGeometry.radialPath(center: c, radius: radius, angleDegrees: angle, imageSize: size)
                // Pixels bright enough to show the -4 EV, away from the outline (resampling).
                var agree = 0, total = 0, inside = 0
                for y in 0..<lit.height {
                    for x in 0..<lit.width {
                        let k = y * lit.width + x
                        guard lum0[k] > 50 else { continue }
                        let p = CGPoint(x: Double(x) + 0.5, y: Double(y) + 0.5)
                        guard abs(Self.outline(p, center: c, radius: radius, angle: angle, size: size)) > 3 / max(size.width, size.height)
                        else { continue }
                        let darkened = lum0[k] - lum1[k] > 15
                        let drawn = path.contains(p)
                        total += 1
                        if drawn { inside += 1 }
                        if darkened == drawn { agree += 1 }
                    }
                }
                let rate = Double(agree) / Double(max(total, 1))
                print(String(format: "H2 radial %@ %dx%d angle %.0f: %d/%d pixels agree (%.4f), %d inside the overlay",
                             portrait ? "portrait" : "landscape", lit.width, lit.height, angle, agree, total, rate, inside))
                #expect(inside > 500 && total - inside > 500, "the test sees both sides of the outline")
                #expect(rate > 0.98, "\(portrait ? "portrait" : "landscape") \(angle)°: rendered vs overlay \(rate)")
                // The handles sit on the drawn (and rendered) outline.
                let aspect = size.width / size.height
                let shape = MaskShape.radial(.add, center: center, radius: radius, angle: angle, feather: 0)
                for (h, q) in MaskHandles.handles(of: shape, aspect: aspect) where h != .radialCenter {
                    let p = CGPoint(x: q.x * size.width, y: q.y * size.height)
                    #expect(abs(Self.outline(p, center: c, radius: radius, angle: angle, size: size)) < 1e-9, "\(h) on the outline")
                    let inward = CGPoint(x: c.x + (p.x - c.x) * 0.95, y: c.y + (p.y - c.y) * 0.95)
                    let outward = CGPoint(x: c.x + (p.x - c.x) * 1.05, y: c.y + (p.y - c.y) * 1.05)
                    #expect(path.contains(inward) && !path.contains(outward), "\(h) on the drawn ellipse")
                }
            }
        }
        if !sampleARWs().isEmpty { #expect(seen.count == 2, "the samples include a portrait and a landscape photo") }
    }

    @Test func handlesAndDraggingWorkInLongEdgeUnits() {
        // Portrait 2:3: one long edge = the height; the width is 2/3 of it.
        let aspect: CGFloat = 2.0 / 3.0
        let r = MaskShape.radial(.add, center: CGPoint(x: 0.5, y: 0.5), radius: CGSize(width: 0.2, height: 0.1), angle: 0, feather: 0.3)
        let hs = MaskHandles.handles(of: r, aspect: aspect)
        #expect(abs(hs[1].1.x - (0.5 + 0.2 * 1.5)) < 1e-9 && hs[1].1.y == 0.5, "0.2 long edges = 0.3 of the width")
        #expect(abs(hs[2].1.y - 0.6) < 1e-9)
        // Dragging the x handle straight down by 0.2 of the height: 0.2 long edges at 90°.
        guard case .radial(_, _, let rx, let angle, _) = MaskHandles.move(r, handle: .radialEdgeX, to: CGPoint(x: 0.5, y: 0.7),
                                                                         aspect: aspect) else { Issue.record("radial"); return }
        #expect(abs(rx.width - 0.2) < 1e-9 && abs(angle - 90) < 1e-9)
        // A 45° turn is 45° in pixels: the handle moves equal pixel distances in x and y.
        let turned = MaskShape.radial(.add, center: CGPoint(x: 0.5, y: 0.5), radius: CGSize(width: 0.2, height: 0.1), angle: 45, feather: 0)
        let e = MaskHandles.handles(of: turned, aspect: aspect)[1].1
        let size = CGSize(width: 400, height: 600)
        #expect(abs((e.x - 0.5) * size.width - (e.y - 0.5) * size.height) < 1e-9)
        // Landscape 3:2: the long edge is the width.
        let land = MaskHandles.handles(of: r, aspect: 1.5)
        #expect(abs(land[1].1.x - 0.7) < 1e-9 && abs(land[2].1.y - 0.65) < 1e-9)
        // A new drag makes a circle in pixels (long-edge units, equal radii) matching the preview.
        var d = CanvasDrag(tool: .radialGradient, start: CGPoint(x: 0.5, y: 0.5))
        d.move(to: CGPoint(x: 0.5, y: 0.8))
        guard case .mask(.radial(_, _, let rr, _, _)) = CanvasInteraction.commit(d, imageSize: size, cropAspect: nil) else {
            Issue.record("radial commit"); return
        }
        #expect(abs(rr.width - 0.3) < 1e-9 && rr.height == rr.width, "0.3 of the height = 0.3 long edges")
        let circle = ViewerOverlayGeometry.radialPath(center: CGPoint(x: 200, y: 300), radius: rr, angleDegrees: 0, imageSize: size)
            .boundingRect
        #expect(abs(circle.width - 360) < 0.5 && abs(circle.height - 360) < 0.5, "drawn as the 180 px circle the drag showed")
    }
}
