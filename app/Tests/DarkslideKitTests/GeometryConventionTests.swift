import CoreGraphics
import Foundation
import Testing
@testable import DarkslideAI
@testable import DarkslideKit

/// R3-H2: the app stores crops in the ds-core / ds-pipeline convention and the viewer
/// displays exactly what ds-pipeline would render.
@MainActor
@Suite struct GeometryConventionTests {
    /// Independent reference, transcribed from ds-pipeline `geometry::plan` (not from
    /// ViewTransform): output point (normalized in the final, turned frame) → oriented
    /// pixel. Turns are undone first (clockwise turns), then the crop-local point is
    /// rotated by `angle` about the crop center with the y-down matrix [[c,-s],[s,c]].
    /// A full-frame rect with an angle is first auto-fitted like `plan` does it: bisect the
    /// largest centered scale whose rotated edges (17 samples each) stay inside the frame
    /// with `plan`'s 0.5 px slack (R4-H5).
    static func pipelineSource(out: CGPoint, rect: CGRect, angleDeg: Double, turns: Int, oriented: CGSize) -> CGPoint {
        let (ew, eh) = (Double(oriented.width), Double(oriented.height))
        let rect = autoFit(rect, angleDeg: angleDeg, ew: ew, eh: eh)
        let (cx0, cy0) = (Double(rect.minX) * ew, Double(rect.minY) * eh)
        let (cw, ch) = (Double(rect.width) * ew, Double(rect.height) * eh)
        let t = turns % 4
        let (fw, fh) = t % 2 == 1 ? (ch, cw) : (cw, ch)
        var p = (Double(out.x) * fw, Double(out.y) * fh)
        var dims = (fw, fh)
        for _ in 0..<t { // inverse of one clockwise turn: (fx, fy) -> (fy, preH - fx)
            let preH = dims.0
            p = (p.1, preH - p.0)
            dims = (dims.1, dims.0)
        }
        let r = angleDeg * .pi / 180
        let (s, c) = (sin(r), cos(r))
        let (lx, ly) = (p.0 - cw / 2, p.1 - ch / 2)
        return CGPoint(x: c * lx - s * ly + cx0 + cw / 2, y: s * lx + c * ly + cy0 + ch / 2)
    }

    static func autoFit(_ rect: CGRect, angleDeg: Double, ew: Double, eh: Double) -> CGRect {
        let angle = min(max(angleDeg, -45), 45)
        guard rect == CGRect(x: 0, y: 0, width: 1, height: 1), angle != 0 else { return rect }
        let r = angle * .pi / 180
        let (s, c) = (sin(r), cos(r))
        func fits(_ k: Double) -> Bool {
            let (hx, hy) = (ew * 0.5 * k, eh * 0.5 * k)
            for i in 0...16 {
                let t = Double(i) / 16
                for (u, v) in [(t, 0.0), (t, 1.0), (0.0, t), (1.0, t)] {
                    let (lx, ly) = ((2 * u - 1) * hx, (2 * v - 1) * hy)
                    let (x, y) = (ew * 0.5 + c * lx - s * ly, eh * 0.5 + s * lx + c * ly)
                    if x < -0.5 || y < -0.5 || x > ew + 0.5 || y > eh + 0.5 { return false }
                }
            }
            return true
        }
        if fits(1) { return rect }
        var (lo, hi) = (0.0, 1.0)
        for _ in 0..<30 {
            let m = (lo + hi) / 2
            if fits(m) { lo = m } else { hi = m }
        }
        let k = max(lo, 1e-3)
        return CGRect(x: 0.5 - 0.5 * k, y: 0.5 - 0.5 * k, width: k, height: k)
    }

    /// The viewer: a point inside the displayed crop box → oriented pixel.
    static func viewerSource(out: CGPoint, crop: CropEdit, oriented: CGSize) -> CGPoint {
        let t = ViewTransform(crop: crop)
        let d = t.displaySize(oriented: oriented)
        let a = d.width / d.height
        let box = t.renderedDisplayRect(crop, displayAspect: a)
        let u = CGPoint(x: box.minX + out.x * box.width, y: box.minY + out.y * box.height)
        let o = t.toOriented(u, displayAspect: a)
        return CGPoint(x: o.x * oriented.width, y: o.y * oriented.height)
    }

    @Test func viewerMatchesThePipelineThroughTheSidecarJSON() throws {
        let sizes = [CGSize(width: 6000, height: 4000), CGSize(width: 4000, height: 6000)] // landscape + portrait
        let rects = [CGRect(x: 0.1, y: 0.2, width: 0.5, height: 0.4), CGRect(x: 0, y: 0, width: 1, height: 1)]
        for size in sizes {
            for rect in rects {
                for turns in 0..<4 {
                    for angle in [0.0, 7.5, -12.0] {
                        var e = EditState()
                        e.crop.rect = rect
                        e.crop.angle = angle
                        e.crop.quarterTurns = turns
                        // Through the real sidecar codec (ds-core Adjustments JSON).
                        let (back, _) = try AdjustmentsCodec.decode(AdjustmentsCodec.encode(e))
                        for out in [CGPoint(x: 0, y: 0), CGPoint(x: 1, y: 0), CGPoint(x: 0.3, y: 0.8), CGPoint(x: 1, y: 1)] {
                            let ref = Self.pipelineSource(out: out, rect: back.crop.rect, angleDeg: back.crop.angle,
                                                          turns: back.crop.quarterTurns, oriented: size)
                            let view = Self.viewerSource(out: out, crop: back.crop, oriented: size)
                            // plan bisects with 0.5 px corner slack, inscribed_crop is exact: <= 1.5 px apart.
                            let tol = rect == CGRect(x: 0, y: 0, width: 1, height: 1) && angle != 0 ? 1.5 : 0.05
                            #expect(abs(ref.x - view.x) < tol && abs(ref.y - view.y) < tol,
                                    "size \(size) rect \(rect) turns \(turns) angle \(angle) out \(out): pipeline \(ref) viewer \(view)")
                        }
                    }
                }
            }
        }
    }

    /// R4-H5: numbers from the review (6000x4000: 0.928 of each side at 3°, 0.842 at 7.5°),
    /// the viewer's crop box on a straightened full frame, and ds-pipeline's own test
    /// (`inscribed_crop(10°, 3000x2000)` within 3 px of `plan`'s auto-fit width).
    @Test func fullFrameStraightenShowsThePipelineAutoFit() async {
        for (deg, k) in [(3.0, 0.928), (7.5, 0.842), (-7.5, 0.842)] {
            let r = ViewTransform.renderedRect(canonical: CGRect(x: 0, y: 0, width: 1, height: 1), angle: deg, orientedAspect: 1.5)
            #expect(abs(r.width - k) < 0.001 && abs(r.height - k) < 0.001 && abs(r.midX - 0.5) < 1e-12, "\(deg): \(r)")
            let ref = Self.autoFit(CGRect(x: 0, y: 0, width: 1, height: 1), angleDeg: deg, ew: 6000, eh: 4000)
            #expect(abs(ref.width - r.width) * 6000 < 1.5, "plan (0.5 px slack) and inscribed_crop agree within 1.5 px")
        }
        let ten = ViewTransform.renderedRect(canonical: CGRect(x: 0, y: 0, width: 1, height: 1), angle: 10, orientedAspect: 1.5)
        #expect(abs(Self.autoFit(CGRect(x: 0, y: 0, width: 1, height: 1), angleDeg: 10, ew: 3000, eh: 2000).width - ten.width) * 3000 < 3)
        // No angle, an explicit crop, or a clamped huge angle.
        let full = CGRect(x: 0, y: 0, width: 1, height: 1)
        #expect(ViewTransform.renderedRect(canonical: full, angle: 0, orientedAspect: 1.5) == full)
        let user = CGRect(x: 0.1, y: 0.1, width: 0.5, height: 0.5)
        #expect(ViewTransform.renderedRect(canonical: user, angle: 12, orientedAspect: 1.5) == user)
        #expect(ViewTransform.renderedRect(canonical: full, angle: 400, orientedAspect: 1)
                == ViewTransform.renderedRect(canonical: full, angle: 45, orientedAspect: 1))
        #expect(ViewTransform.renderedRect(canonical: full, angle: .nan, orientedAspect: 1) == full)
        // The viewer's crop box for a straightened, uncropped photo is the auto-fit.
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "Straighten") { $0.crop.angle = 7.5 }
        let box = m.viewer.cropRect
        #expect(box.width < 0.95 && abs(box.width - box.height) < 1e-9 && abs(box.midX - 0.5) < 1e-9, "\(box)")
        #expect(m.focusedEdit.crop.rect == full, "the stored crop stays full (ds-pipeline auto-fits it)")
    }

    @Test func pipelineReferenceMatchesItsOwnTests() {
        // ds-pipeline quarter_turns_and_flips: one clockwise turn, output top-left shows
        // the source bottom-left.
        let p = Self.pipelineSource(out: .zero, rect: CGRect(x: 0, y: 0, width: 1, height: 1), angleDeg: 0, turns: 1,
                                    oriented: CGSize(width: 400, height: 200))
        #expect(abs(p.x) < 1e-9 && abs(p.y - 200) < 1e-9)
        // straighten_rotates_about_crop_center: +45°, a point right of center samples below-right
        // (h 0.999999 like the pipeline test, so the full-frame auto-fit stays out of it).
        let q = Self.pipelineSource(out: CGPoint(x: 0.6, y: 0.5), rect: CGRect(x: 0, y: 0, width: 1, height: 0.999_999),
                                    angleDeg: 45, turns: 0, oriented: CGSize(width: 1000, height: 1000))
        let d = 100 * 0.5.squareRoot()
        #expect(abs(q.x - (500 + d)) < 1e-3 && abs(q.y - (500 + d)) < 1e-3)
    }

    @Test func cropThenRotateKeepsTheCroppedContent() async {
        let m = makeModel()
        await m.start()
        m.updateEdit(label: "Crop") { $0.crop.rect = CGRect(x: 0, y: 0, width: 0.5, height: 1) } // oriented left half
        m.viewer.rotate(by: 1)
        #expect(m.focusedEdit.crop.rect == CGRect(x: 0, y: 0, width: 0.5, height: 1), "stored crop is canonical")
        // Seen on screen after one clockwise turn, the left half is the top half.
        let box = m.viewer.cropRect
        #expect(abs(box.minX) < 1e-9 && abs(box.minY) < 1e-9 && abs(box.width - 1) < 1e-9 && abs(box.height - 0.5) < 1e-9, "\(box)")
        m.viewer.rotate(by: -1)
        #expect(m.viewer.cropRect == CGRect(x: 0, y: 0, width: 0.5, height: 1))
    }

    @Test func straightenKeepsTheCropCenterOnTheSameContent() async {
        let m = makeModel()
        await m.start()
        let rect = CGRect(x: 0.2, y: 0.3, width: 0.3, height: 0.3)
        m.updateEdit(label: "Crop") { $0.crop.rect = rect }
        m.updateEdit(label: "Straighten") { $0.crop.angle = 10 }
        #expect(m.focusedEdit.crop.rect == rect)
        // The displayed box's center maps back to the same oriented point.
        let box = m.viewer.cropRect
        let back = m.viewer.oriented(CGPoint(x: box.midX, y: box.midY))
        #expect(abs(back.x - rect.midX) < 1e-9 && abs(back.y - rect.midY) < 1e-9)
    }

    @Test func displayAndCanonicalRectsAreInverse() {
        for turns in 0..<4 {
            for angle in [0.0, 20, -33] {
                let t = ViewTransform(quarterTurns: turns, angle: angle)
                let r = CGRect(x: 0.15, y: 0.25, width: 0.4, height: 0.3)
                let d = t.displayRect(canonical: r, displayAspect: 1.5)
                let back = t.canonicalRect(display: d, displayAspect: 1.5)
                #expect(abs(back.minX - r.minX) < 1e-9 && abs(back.minY - r.minY) < 1e-9
                        && abs(back.width - r.width) < 1e-9 && abs(back.height - r.height) < 1e-9)
                if turns % 2 == 1 { #expect(abs(d.width - r.height) < 1e-12) }
            }
        }
        // Float noise around the full frame snaps to the identity crop.
        #expect(ViewTransform.snapFull(CGRect(x: 1e-12, y: -1e-12, width: 1 + 1e-12, height: 1)) == CGRect(x: 0, y: 0, width: 1, height: 1))
    }

    @Test func positiveAngleTurnsContentCounterClockwiseOnScreen() {
        // A point right of center moves *up* on screen (y decreases) for a CCW turn.
        let t = ViewTransform(angle: 30)
        let p = t.toDisplay(CGPoint(x: 0.8, y: 0.5), displayAspect: 1)
        #expect(p.y < 0.5 && p.x < 0.8)
        #expect(t.screenClockwise == -30 && abs(Double(t.angleRadians) + 30 * .pi / 180) < 1e-6)
    }

    @Test func canvasCropAndStraightenStoreTheCanonicalForm() async {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        m.viewer.rotate(by: 1)
        let img = m.viewer.layout.main!.image
        m.viewer.canvasTool = .crop
        // Drag the full crop's bottom edge up to the middle: the top half of the (turned) display.
        m.viewer.beginCanvasDrag(at: CGPoint(x: img.midX, y: img.maxY))
        m.viewer.updateCanvasDrag(to: CGPoint(x: img.midX, y: img.midY))
        m.viewer.endCanvasDrag()
        let r = m.focusedEdit.crop.rect
        // Display top half after one clockwise turn = oriented left half.
        #expect(abs(r.minX) < 0.01 && abs(r.minY) < 0.01 && abs(r.width - 0.5) < 0.01 && abs(r.height - 1) < 0.01, "\(r)")
        // Straighten tool: a line rising to the right needs a clockwise on-screen turn,
        // i.e. a negative ds-core angle.
        m.viewer.canvasTool = .straighten
        m.viewer.beginCanvasDrag(at: CGPoint(x: img.minX + 10, y: img.midY))
        m.viewer.updateCanvasDrag(to: CGPoint(x: img.maxX - 10, y: img.midY - 40))
        m.viewer.endCanvasDrag()
        #expect(m.focusedEdit.crop.angle < 0)
        // Keyboard nudge right = clockwise = angle decreases.
        let before = m.focusedEdit.crop.angle
        #expect(m.viewer.nudgeCanvas(dx: 1, dy: 0))
        #expect(m.focusedEdit.crop.angle < before)
        // Crop nudge keeps canonical storage.
        m.viewer.canvasTool = .crop
        let r0 = m.focusedEdit.crop.rect
        m.viewer.nudgeCrop(dx: 10, dy: 0)
        #expect(m.focusedEdit.crop.rect != r0)
        #expect(abs(m.focusedEdit.crop.rect.width - r0.width) < 1e-9, "a move keeps the size")
    }

    /// Physical check with Apple Vision: a horizon rising 6° to the right must be
    /// levelled (not doubled) by auto-straighten under the ds-core convention.
    @Test func autoStraightenLevelsARisingHorizon() async throws {
        let (w, h) = (800, 600)
        let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        ctx.setFillColor(CGColor(srgbRed: 0.5, green: 0.7, blue: 1, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
        let tilt = 6.0 * .pi / 180
        let yl = Double(h) / 2 - tan(tilt) * Double(w) / 2, yr = Double(h) / 2 + tan(tilt) * Double(w) / 2 // CG is y-up
        ctx.setFillColor(CGColor(srgbRed: 0.2, green: 0.35, blue: 0.1, alpha: 1))
        ctx.move(to: .zero)
        ctx.addLine(to: CGPoint(x: 0, y: yl))
        ctx.addLine(to: CGPoint(x: Double(w), y: yr))
        ctx.addLine(to: CGPoint(x: w, y: 0))
        ctx.closePath()
        ctx.fillPath()
        let image = try #require(ctx.makeImage())
        let horizon = try #require(try await VisionService().horizonDegrees(image))
        #expect(horizon > 4 && horizon < 8, "VisionService reports the tilt as rises-to-the-right positive: \(horizon)")
        // Straighten is explicit (NL "straighten" / the tool's Auto button), never part of Auto.
        let e = EditState().applying(EditIntent(straighten: true), horizonDegrees: horizon)
        #expect(e.crop.angle < 0, "ds-core: level a CCW tilt with a clockwise (negative) angle")
        // In the viewer, the horizon's endpoints (top-left origin) end up at the same height.
        let t = ViewTransform(crop: e.crop)
        let a = t.toDisplay(CGPoint(x: 0, y: 1 - yl / Double(h)), displayAspect: 800.0 / 600.0)
        let b = t.toDisplay(CGPoint(x: 1, y: 1 - yr / Double(h)), displayAspect: 800.0 / 600.0)
        #expect(abs(a.y - b.y) < 0.01, "levelled: \(a) \(b)")
        // The intent path uses the same sign.
        let i = EditState().applying(EditIntent(straighten: true), horizonDegrees: horizon)
        #expect(abs(i.crop.angle - e.crop.angle) < 0.1)
    }
}

/// GOLDEN (integration B): an app-written sidecar (crop + quarter turn + straighten +
/// keystone, lens correction on) rendered by the real ds-pipeline puts a marker exactly
/// where the viewer's overlay geometry says, and the overlay's crop box is the rectangle
/// the pipeline rendered (read from its mapping, no emulation), including the full-frame
/// auto-fit that keystone and lens distortion change.
@Suite(.serialized) struct GoldenGeometryTests {
    struct Case: Sendable {
        let name: String
        let setUp: @Sendable (inout EditState) -> Void
        let userRect: Bool
    }

    static let cases = [
        Case(name: "crop + turn + straighten + keystone", setUp: { e in
            e.crop.rect = CGRect(x: 0.2, y: 0.18, width: 0.55, height: 0.5)
            e.crop.angle = 6
            e.crop.quarterTurns = 1
            e.lens.keystoneVertical = 8
        }, userRect: true),
        Case(name: "full frame: straighten + keystone auto-fit", setUp: { e in
            e.crop.angle = -4
            e.crop.quarterTurns = 3
            e.lens.keystoneVertical = 10
            e.lens.keystoneHorizontal = -5
        }, userRect: false),
    ]

    /// Mean position (frame-normalized) of the pixels the marker layer brightened.
    static func markerCentroid(_ base: RenderedFrame, _ lit: RenderedFrame) -> CGPoint? {
        guard base.width == lit.width, base.height == lit.height else { return nil }
        let (a, b) = (base.rgba, lit.rgba)
        var (sx, sy, n) = (0.0, 0.0, 0.0)
        for y in 0..<base.height {
            for x in 0..<base.width {
                let i = y * base.bytesPerRow + x * 4
                let d = (Int(b[i]) + Int(b[i + 1]) + Int(b[i + 2])) - (Int(a[i]) + Int(a[i + 1]) + Int(a[i + 2]))
                if d > 90 {
                    sx += Double(x) + 0.5
                    sy += Double(y) + 0.5
                    n += 1
                }
            }
        }
        guard n > 20 else { return nil }
        return CGPoint(x: sx / n / Double(base.width), y: sy / n / Double(base.height))
    }

    @Test func pipelineRenderMatchesTheOverlayGeometry() async throws {
        guard let url = sampleARWs().first else { return }
        let photo = try await RustRaw.photo(at: url, id: PhotoID(1))
        let renderer = DevelopRenderer()
        let marker = CGPoint(x: 0.47, y: 0.43) // crop space (E-normalized)
        for c in Self.cases {
            var e = EditState()
            c.setUp(&e)
            // App-written sidecar: what the app stores is what the pipeline renders.
            let (edit, _) = try AdjustmentsCodec.decode(AdjustmentsCodec.encode(e))
            #expect(edit.lens.keystoneVertical == e.lens.keystoneVertical && edit.crop.quarterTurns == e.crop.quarterTurns)
            var marked = edit
            marked.layers = [LayerEdit(id: 1, name: "marker", exposureEV: 4,
                                       mask: MaskEdit(shapes: [.radial(.add, center: marker, radius: CGSize(width: 0.012, height: 0.012),
                                                                       angle: 0, feather: 0)]))]
            let request = RenderRequest(maxPixelSize: 900, format: .rgba8Srgb, geometry: .final)
            let base = try await renderer.render(photo, edit: edit, request: request)
            let lit = try await renderer.render(photo, edit: marked, request: request)
            let mapping = try #require(base.mapping, "\(c.name): final frames carry their mapping")
            // 1. The marker lands where the mapping puts its crop-space point.
            let seen = try #require(Self.markerCentroid(base, lit), "\(c.name): marker visible")
            let expected = try #require(mapping.toFrame(marker))
            #expect(abs(seen.x - expected.x) < 0.008 && abs(seen.y - expected.y) < 0.008,
                    "\(c.name): marker at \(seen), mapping says \(expected)")
            // 2. The overlay (oriented display: ViewTransform turns and straightens) draws its
            //    crop box on exactly the pipeline's rendered rectangle.
            let (plan, _) = try await renderer.plan(photo, edit: edit, request: request)
            let orientedAspect = CGFloat(plan.sourceWidth) / CGFloat(plan.sourceHeight)
            let t = ViewTransform(crop: edit.crop)
            let displayAspect = t.swapsAxes ? 1 / orientedAspect : orientedAspect
            let box = t.renderedDisplayRect(edit.crop, displayAspect: displayAspect, mapping: mapping)
            let boxCorners = [CGPoint(x: box.minX, y: box.minY), CGPoint(x: box.maxX, y: box.minY),
                              CGPoint(x: box.minX, y: box.maxY), CGPoint(x: box.maxX, y: box.maxY)]
                .map { t.toOriented($0, displayAspect: displayAspect) }
            let frameCorners = [CGPoint(x: 0, y: 0), CGPoint(x: 1, y: 0), CGPoint(x: 0, y: 1), CGPoint(x: 1, y: 1)]
                .map(mapping.toCrop)
            for f in frameCorners {
                let d = boxCorners.map { hypot(($0.x - f.x) * orientedAspect, $0.y - f.y) }.min() ?? 1
                #expect(d < 1e-3, "\(c.name): frame corner \(f) vs overlay corners \(boxCorners)")
            }
            print("GOLDEN \(c.name): marker \(seen) vs mapping \(expected); frame \(base.width)x\(base.height)")
            let rendered = try #require(ViewTransform.canonicalRect(from: mapping, orientedAspect: orientedAspect,
                                                                    quarterTurns: edit.crop.quarterTurns))
            if c.userRect {
                // An explicit crop is rendered as stored.
                #expect(abs(rendered.minX - edit.crop.rect.minX) < 1e-3 && abs(rendered.minY - edit.crop.rect.minY) < 1e-3
                        && abs(rendered.width - edit.crop.rect.width) < 1e-3 && abs(rendered.height - edit.crop.rect.height) < 1e-3,
                        "\(rendered) vs \(edit.crop.rect)")
            } else {
                // The full-frame auto-fit depends on keystone and the lens profile: the
                // straighten-only emulation would draw a different (too large) box.
                let emulated = ViewTransform.renderedRect(canonical: edit.crop.rect, angle: edit.crop.angle,
                                                          orientedAspect: orientedAspect)
                #expect(rendered.width < emulated.width - 0.005 || rendered.height < emulated.height - 0.005,
                        "keystone/lens shrink the auto-fit: \(rendered) vs \(emulated)")
                #expect(abs(rendered.midX - 0.5) < 0.02 && abs(rendered.midY - 0.5) < 0.02)
            }
        }
    }

    /// Pure: the rect read back from an affine equals the crop that made it (all turns).
    @Test func canonicalRectFromASyntheticMapping() {
        let rect = CGRect(x: 0.1, y: 0.2, width: 0.5, height: 0.4)
        let a: CGFloat = 1.5
        for turns in 0..<4 {
            for angle in [0.0, 12] {
                // Frame (u, v) → crop space through the viewer's own transform.
                let t = ViewTransform(quarterTurns: turns, angle: angle)
                let displayAspect = t.swapsAxes ? 1 / a : a
                let box = t.displayRect(canonical: rect, displayAspect: displayAspect)
                func toCrop(_ p: CGPoint) -> CGPoint {
                    t.toOriented(CGPoint(x: box.minX + p.x * box.width, y: box.minY + p.y * box.height), displayAspect: displayAspect)
                }
                let o = toCrop(.zero), ux = toCrop(CGPoint(x: 1, y: 0)), uy = toCrop(CGPoint(x: 0, y: 1))
                let m = FrameMapping(affine: [ux.x - o.x, uy.x - o.x, o.x, ux.y - o.y, uy.y - o.y, o.y],
                                     nativeSize: CGSize(width: 100, height: 100))
                let back = ViewTransform.canonicalRect(from: m, orientedAspect: a, quarterTurns: turns)!
                #expect(abs(back.minX - rect.minX) < 1e-9 && abs(back.width - rect.width) < 1e-9
                        && abs(back.minY - rect.minY) < 1e-9 && abs(back.height - rect.height) < 1e-9, "\(turns) \(angle): \(back)")
                let viaMapping = t.renderedDisplayRect(CropEdit(), displayAspect: displayAspect, mapping: m)
                #expect(abs(viaMapping.minX - box.minX) < 1e-9 && abs(viaMapping.width - box.width) < 1e-9)
            }
        }
        let degenerate = FrameMapping(affine: [0, 0, 0.5, 0, 0, 0.5], nativeSize: .zero)
        #expect(ViewTransform.canonicalRect(from: degenerate, orientedAspect: 1, quarterTurns: 0) == nil)
        #expect(ViewTransform.canonicalRect(from: degenerate, orientedAspect: 0, quarterTurns: 0) == nil)
    }
}
