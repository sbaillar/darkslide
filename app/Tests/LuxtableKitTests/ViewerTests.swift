import CoreGraphics
import LuxtableFFI
import Foundation
import Metal
import simd
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

@Suite struct ViewerGeometryTests {
    let image = CGSize(width: 6000, height: 4000)
    let view = CGSize(width: 1224, height: 824)

    @Test func fitCentersWithMargin() {
        let r = ViewerGeometry.imageRect(image: image, view: view, backingScale: 2, zoom: .fit, pan: CGPoint(x: 99, y: 99))
        #expect(abs(r.width - 1200) < 1e-9 && abs(r.height - 800) < 1e-9)
        #expect(abs(r.midX - 612) < 1e-9 && abs(r.midY - 412) < 1e-9, "pan ignored at fit")
    }

    @Test func scaleIsDevicePixelsPerImagePixel() {
        #expect(ViewerGeometry.scale(image: image, view: view, backingScale: 2, zoom: .actual) == 0.5)
        #expect(ViewerGeometry.scale(image: image, view: view, backingScale: 2, zoom: .scale(2)) == 1)
        #expect(ViewerGeometry.scale(image: image, view: view, backingScale: 0, zoom: .actual) == 1)
        #expect(ViewerGeometry.scale(image: .zero, view: view, backingScale: 2, zoom: .fit) == 1)
        let r = ViewerGeometry.imageRect(image: image, view: view, backingScale: 2, zoom: .actual, pan: .zero)
        #expect(r.size == CGSize(width: 3000, height: 2000))
        #expect(abs(ViewerGeometry.deviceScale(image: image, view: view, backingScale: 2, zoom: .fit) - 0.4) < 1e-9)
        #expect(ZoomMode.actual == .scale(1))
    }

    @Test func zoomedOriginSnapsToDevicePixels() {
        // An odd pane width puts the centered image at a half point; at 100% it must land on the device grid.
        let r = ViewerGeometry.imageRect(image: CGSize(width: 101, height: 101), view: CGSize(width: 100.3, height: 50),
                                         backingScale: 2, zoom: .actual, pan: .zero)
        #expect((r.minX * 2).rounded() == r.minX * 2 && (r.minY * 2).rounded() == r.minY * 2)
        #expect(ViewerGeometry.snap(CGPoint(x: 0.3, y: 0.76), backingScale: 2) == CGPoint(x: 0.5, y: 1))
    }

    @Test func panClampsToEdgesAndCentersSmallAxes() {
        let p = ViewerGeometry.clampPan(CGPoint(x: 5000, y: -5000), imageSize: CGSize(width: 3000, height: 500), view: view)
        #expect(abs(p.x - 888) < 1e-9)
        #expect(p.y == 0, "image shorter than the view stays centered")
    }

    @Test func zoomAtAnchorKeepsThePointUnderIt() {
        let cursor = CGPoint(x: 312, y: 212) // 25% / 25% of the fitted image
        let pan = ViewerGeometry.panForActualZoom(at: cursor, image: image, view: view, backingScale: 2)
        let r = ViewerGeometry.imageRect(image: image, view: view, backingScale: 2, zoom: .actual, pan: pan)
        let u = CGPoint(x: (cursor.x - r.minX) / r.width, y: (cursor.y - r.minY) / r.height)
        #expect(abs(u.x - 0.25) < 1e-3 && abs(u.y - 0.25) < 1e-3)
        #expect(ViewerGeometry.panForActualZoom(at: .zero, image: image, view: .zero, backingScale: 2) == .zero)
        // 100% → 200% around a point.
        let p2 = ViewerGeometry.panForZoom(anchor: cursor, image: image, view: view, backingScale: 2,
                                           from: .actual, fromPan: pan, to: .scale(2))
        let r2 = ViewerGeometry.imageRect(image: image, view: view, backingScale: 2, zoom: .scale(2), pan: p2)
        #expect(abs((cursor.x - r2.minX) / r2.width - 0.25) < 1e-3)
        #expect(ViewerGeometry.panForZoom(anchor: cursor, image: image, view: view, backingScale: 2,
                                          from: .actual, fromPan: pan, to: .fit) == .zero)
        #expect(ViewerGeometry.panForZoom(anchor: cursor, image: .zero, view: view, backingScale: 2,
                                          from: .fit, fromPan: .zero, to: .actual) == .zero)
    }

    @Test func zoomSteps() {
        #expect(ViewerGeometry.step(from: 0.4, fitScale: 0.4, up: true) == .scale(0.5))
        #expect(ViewerGeometry.step(from: 0.5, fitScale: 0.4, up: true) == .scale(1))
        #expect(ViewerGeometry.step(from: 1, fitScale: 0.4, up: true) == .scale(2))
        #expect(ViewerGeometry.step(from: 4, fitScale: 0.4, up: true) == .scale(8), "past the last step doubles")
        #expect(ViewerGeometry.step(from: 16, fitScale: 0.4, up: true) == .scale(16), "capped")
        #expect(ViewerGeometry.step(from: 2, fitScale: 0.4, up: false) == .scale(1))
        #expect(ViewerGeometry.step(from: 1, fitScale: 0.4, up: false) == .scale(0.5))
        #expect(ViewerGeometry.step(from: 0.5, fitScale: 0.4, up: false) == .fit, "below fit → fit")
        #expect(ViewerGeometry.step(from: 0.2, fitScale: 0.1, up: false) == .fit)
    }

    @Test func visibleRegion() {
        let v = ViewerGeometry.visibleRegion(imageRect: CGRect(x: -1000, y: -500, width: 3000, height: 2000),
                                             pane: CGSize(width: 1000, height: 1000))
        #expect(abs(v.minX - 1.0 / 3) < 1e-9 && abs(v.width - 1.0 / 3) < 1e-9 && abs(v.minY - 0.25) < 1e-9 && abs(v.height - 0.5) < 1e-9)
        #expect(ViewerGeometry.visibleRegion(imageRect: .zero, pane: CGSize(width: 1, height: 1)) == .zero)
    }

    @Test func ndcFlipsY() {
        let n = ViewerGeometry.ndc(CGRect(x: 0, y: 0, width: 50, height: 25), in: CGSize(width: 100, height: 100))
        #expect(n == SIMD4(-1, 0.5, 0, 1))
        #expect(ViewerGeometry.ndc(.zero, in: .zero) == .zero)
    }

    @Test func panesSplitTheView() {
        let p = ViewerGeometry.panes(count: 2, in: CGSize(width: 202, height: 100))
        #expect(p == [CGRect(x: 0, y: 0, width: 100, height: 100), CGRect(x: 102, y: 0, width: 100, height: 100)])
        #expect(ViewerGeometry.panes(count: 0, in: CGSize(width: 10, height: 10)).count == 1)
    }
}

@Suite struct ViewerLayoutTests {
    func state(_ size: CGSize = CGSize(width: 1224, height: 824)) -> ViewerState {
        var s = ViewerState()
        s.viewSize = size
        return s
    }

    @Test func singlePaneAndImagePoint() {
        let l = ViewerLayout.make(state: state(), main: CGSize(width: 6000, height: 4000), pinned: nil)
        #expect(l.panes.count == 1 && l.panes[0].role == .main && l.panes[0].splitX == nil)
        let u = l.imagePoint(at: CGPoint(x: 612, y: 412))!
        #expect(abs(u.x - 0.5) < 1e-9 && abs(u.y - 0.5) < 1e-9)
        #expect(l.imagePoint(at: CGPoint(x: 2, y: 2)) == nil, "margin is outside the image")
        let p = l.main!
        #expect(p.visibleRegion == CGRect(x: 0, y: 0, width: 1, height: 1))
        let back = p.viewPoint(p.normalized(CGPoint(x: 100, y: 200)))
        #expect(abs(back.x - 100) < 1e-9 && abs(back.y - 200) < 1e-9)
        #expect(l.pane(at: CGPoint(x: 5, y: 5))?.role == .main && l.pane(at: CGPoint(x: -5, y: 5)) == nil)
    }

    @Test func beforeAfterSplit() {
        var s = state()
        s.beforeAfter = true
        s.split = 0.25
        let l = ViewerLayout.make(state: s, main: CGSize(width: 6000, height: 4000), pinned: nil)
        #expect(abs(l.panes[0].splitX! - (12 + 300)) < 1e-9)
        #expect(l.splitFraction(atX: 612) == 0.5)
        #expect(l.splitFraction(atX: -100) == 0 && l.splitFraction(atX: 5000) == 1)
    }

    @Test func compareUsesPaneSpace() {
        var s = state()
        s.zoom = .actual
        s.pan = CGPoint(x: 1e6, y: 0)
        let l = ViewerLayout.make(state: s, main: CGSize(width: 6000, height: 4000), pinned: CGSize(width: 4000, height: 6000))
        #expect(l.panes.map(\.role) == [.pinned, .main])
        #expect(l.panes[1].frame.minX > l.panes[0].frame.maxX)
        let main = l.panes[1]
        // Clamped against the half-width pane, so the right edge of the image meets the pane's right edge.
        #expect(abs(main.image.minX - main.frame.minX) < 1, "left image edge at the pane edge after max pan")
        #expect(main.visibleRegion.minX == 0)
        let fit = ViewerLayout.make(state: state(), main: CGSize(width: 6000, height: 4000), pinned: CGSize(width: 4000, height: 6000))
        #expect(fit.panes[0].image.height > fit.panes[0].image.width, "portrait pinned image")
    }

    @Test func emptyWithoutSize() {
        let l = ViewerLayout.make(state: state(.zero), main: CGSize(width: 10, height: 10), pinned: nil)
        #expect(l.panes.isEmpty && l.imagePoint(at: .zero) == nil && l.splitFraction(atX: 1) == nil && l.main == nil)
        #expect(ViewerLayout.Pane(role: .main, frame: .zero, image: .zero).normalized(.zero) == .zero)
    }
}

func solidFrame(_ c: [UInt8], w: Int = 4, h: Int = 4) -> RenderedFrame {
    RenderedFrame(width: w, height: h, rgba: Data((0..<(w * h)).flatMap { _ in c + [255] }))
}

func floatFrame(_ v: Float16, w: Int = 4, h: Int = 4) -> RenderedFrame {
    var px = [Float16](repeating: v, count: w * h * 4)
    for i in stride(from: 3, to: px.count, by: 4) { px[i] = 1 }
    let data = px.withUnsafeBytes { Data($0) }
    return RenderedFrame(width: w, height: h, format: .rgba16fLinearP3, bytesPerRow: w * 8, backing: .data(data))
}

@Suite struct ViewerDrawPlanTests {
    func layout(beforeAfter: Bool = false, pinned: Bool = false, zoom: ZoomMode = .fit) -> (ViewerLayout, ViewerState) {
        var s = ViewerState()
        s.viewSize = CGSize(width: 1224, height: 824)
        s.beforeAfter = beforeAfter
        s.zoom = zoom
        return (ViewerLayout.make(state: s, main: CGSize(width: 6000, height: 4000), pinned: pinned ? CGSize(width: 6000, height: 4000) : nil), s)
    }

    @Test func singleFrameIsMinifiedTrilinear() {
        let (l, s) = layout()
        let f = solidFrame([1, 2, 3], w: 6000, h: 4000)
        let plan = ViewerDrawPlan.make(.init(layout: l, viewSize: s.viewSize, backingScale: 2, main: f))
        #expect(plan.quads.count == 1 && !plan.edr)
        #expect(plan.quads[0].frame == f && !plan.quads[0].nearest, "6000 texels into 2400 device px: trilinear")
        #expect(plan.quads[0].colorMatrix == matrix_identity_float3x3)
        #expect(plan.frameIDs == [f.id])
        #expect(ViewerDrawPlan.make(.init(layout: l, viewSize: s.viewSize, backingScale: 2, main: nil)).quads.isEmpty)
    }

    @Test func magnifiedFrameIsNearest() {
        let (l, s) = layout()
        let f = solidFrame([1, 2, 3], w: 600, h: 400)
        let plan = ViewerDrawPlan.make(.init(layout: l, viewSize: s.viewSize, backingScale: 2, main: f))
        #expect(plan.quads[0].nearest)
    }

    @Test func beforeAfterSplitAndCompare() {
        let (l, s) = layout(beforeAfter: true)
        let f = solidFrame([1, 1, 1]), b = solidFrame([2, 2, 2])
        let plan = ViewerDrawPlan.make(.init(layout: l, viewSize: s.viewSize, backingScale: 2, main: f, before: b))
        #expect(plan.quads[0].before == b && abs(plan.quads[0].split) < 1e-6, "split at the image center = NDC 0")
        #expect(plan.frameIDs == [f.id, b.id])
        let (lc, sc) = layout(pinned: true)
        let c = solidFrame([3, 3, 3])
        let cp = ViewerDrawPlan.make(.init(layout: lc, viewSize: sc.viewSize, backingScale: 2, main: f, compare: c))
        #expect(cp.quads.map(\.frame) == [c, f])
        #expect(cp.quads[0].clip.z < cp.quads[1].clip.x, "each pane clipped to its half")
        let noCompare = ViewerDrawPlan.make(.init(layout: lc, viewSize: sc.viewSize, backingScale: 2, main: f))
        #expect(noCompare.quads.map(\.frame) == [f])
    }

    @Test func tileDrawsOverItsRegion() {
        let (l, s) = layout(zoom: .actual)
        let f = solidFrame([1, 1, 1], w: 2400, h: 1600)
        let t = solidFrame([9, 9, 9], w: 1200, h: 800)
        let region = CGRect(x: 0.4, y: 0.4, width: 0.2, height: 0.2)
        let plan = ViewerDrawPlan.make(.init(layout: l, viewSize: s.viewSize, backingScale: 2, main: f, tile: (t, region)))
        #expect(plan.quads.map(\.frame) == [f, t])
        // The tile quad covers the image; the shader keeps only its region (oriented space).
        #expect(plan.quads[1].rect == plan.quads[0].rect)
        #expect(plan.quads[1].region == SIMD4(0.4, 0.4, 0.6, 0.6))
        #expect(plan.quads[1].nearest, "tile at 1:1 is sampled nearest")
    }

    @Test func floatFramesSwitchToEDRWithMatrices() {
        let (l, s) = layout(pinned: true)
        let f = floatFrame(2), c = solidFrame([1, 1, 1])
        let plan = ViewerDrawPlan.make(.init(layout: l, viewSize: s.viewSize, backingScale: 2, main: f, compare: c))
        #expect(plan.edr)
        #expect(plan.quads[1].colorMatrix == matrix_identity_float3x3, "linear P3 into an EDR P3 target")
        #expect(plan.quads[0].colorMatrix == ViewerColor.srgbToP3, "sRGB frame converted into P3")
        #expect(ViewerColor.matrix(source: .rgba16fLinearP3, edrTarget: false) == ViewerColor.p3ToSrgb)
        let white = ViewerColor.srgbToP3 * SIMD3<Float>(1, 1, 1)
        #expect(abs(white.x - 1) < 1e-4 && abs(white.y - 1) < 1e-4 && abs(white.z - 1) < 1e-4, "white stays white")
    }

    @Test func loupeSamplesAroundTheCursor() {
        let (l, s) = layout()
        let f = solidFrame([1, 1, 1], w: 2400, h: 1600)
        var input = ViewerDrawPlan.Input(layout: l, viewSize: s.viewSize, backingScale: 2, main: f)
        input.imageSize = CGSize(width: 6000, height: 4000)
        input.loupe = .init(center: CGPoint(x: 612, y: 412), radius: 100, scale: 1)
        input.clippingWarning = true
        input.focusPeaking = true
        let plan = ViewerDrawPlan.make(input)
        let q = plan.quads[1]
        #expect(q.circle && q.nearest && q.clippingWarning && q.focusPeaking)
        // 200 pt diameter × 2 = 400 device px of a 6000 px wide image at 100%.
        #expect(abs(Double(q.span.z - q.span.x) - 400.0 / 6000) < 1e-5)
        #expect(abs(Double(q.span.x + q.span.z) / 2 - 0.5) < 1e-5)
        input.loupe = .init(center: CGPoint(x: 1, y: 1))
        #expect(ViewerDrawPlan.make(input).quads.count == 1, "no loupe outside the image")
        // Prefer the tile when it covers the point.
        input.tile = (solidFrame([2, 2, 2], w: 600, h: 400), CGRect(x: 0.25, y: 0.25, width: 0.5, height: 0.5))
        input.loupe = .init(center: CGPoint(x: 612, y: 412), radius: 100, scale: 1)
        let tp = ViewerDrawPlan.make(input)
        #expect(tp.quads.last?.frame == input.tile!.frame)
        #expect(tp.quads.last!.region == SIMD4(0.25, 0.25, 0.75, 0.75), "tile region maps the texture")
        #expect(abs(Double(tp.quads.last!.span.z - tp.quads.last!.span.x) - 400.0 / 6000) < 1e-5, "span is display space")
    }
}

@MainActor
@Suite struct MetalViewerTests {
    @Test func drawsBeforeAfterSplitAndClearsOutside() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        let after = try #require(m.makeTexture(from: solidFrame([200, 30, 30])))
        let before = try #require(m.makeTexture(from: solidFrame([30, 30, 200])))
        let pane = MetalViewerRenderer.Pane(after: after, before: before, rect: SIMD4(-0.5, -1, 0.5, 1), split: 0)
        let out = try #require(m.renderOffscreen([pane], width: 80, height: 20, clear: MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)))
        func px(_ x: Int, _ y: Int) -> [UInt8] { Array(out[(y * 80 + x) * 4..<(y * 80 + x) * 4 + 3]) }
        #expect(px(2, 10) == [0, 0, 0], "outside the image rect is the clear color")
        #expect(px(30, 10) == [30, 30, 200], "left of the split shows before")
        #expect(px(50, 10) == [200, 30, 30], "right of the split shows after")
        #expect(px(40, 10) == [255, 255, 255] || px(39, 10) == [255, 255, 255], "split line")
    }

    @Test func clipRestrictsPanes() throws {
        let m = try MetalViewerRenderer(pixelFormat: .bgra8Unorm_srgb)
        let tex = try #require(m.makeTexture(from: solidFrame([10, 200, 10])))
        let pane = MetalViewerRenderer.Pane(after: tex, rect: SIMD4(-1, -1, 1, 1), clip: SIMD4(-1, -1, 0, 1))
        let out = try #require(m.renderOffscreen([pane], width: 20, height: 4, clear: MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)))
        #expect(Array(out[(2 * 20 + 5) * 4..<(2 * 20 + 5) * 4 + 3]) == [10, 200, 10], "BGRA read back as RGBA")
        #expect(Array(out[(2 * 20 + 15) * 4..<(2 * 20 + 15) * 4 + 3]) == [0, 0, 0])
    }

    /// H3 regression: at 1:1 a one-pixel feature stays exactly one device pixel wide.
    @Test func oneToOneKeepsSinglePixelFeatures() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        var px = [UInt8](repeating: 0, count: 16 * 4 * 4)
        for y in 0..<4 { let i = (y * 16 + 7) * 4; px[i] = 255; px[i + 1] = 255; px[i + 2] = 255 }
        for i in stride(from: 3, to: px.count, by: 4) { px[i] = 255 }
        let tex = try #require(m.makeTexture(from: RenderedFrame(width: 16, height: 4, rgba: Data(px)), mipmapped: true))
        var pane = MetalViewerRenderer.Pane(after: tex, rect: SIMD4(-1, -1, 1, 1))
        pane.nearest = true
        let out = try #require(m.renderOffscreen([pane], width: 16, height: 4, clear: MTLClearColor()))
        let row = (0..<16).map { out[(1 * 16 + $0) * 4] }
        #expect(row.filter { $0 == 255 }.count == 1 && row[7] == 255, "\(row)")
        #expect(row.filter { $0 != 0 && $0 != 255 }.isEmpty, "no blur at 1:1")
        #expect(tex.mipmapLevelCount > 1)
    }

    /// H6 regression: a 16F value above 1.0 survives into an EDR target.
    @Test func floatFramesKeepExtendedRange() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        let tex = try #require(m.makeTexture(from: floatFrame(2.5)))
        #expect(tex.pixelFormat == .rgba16Float)
        let out = try #require(m.renderOffscreenFloat([MetalViewerRenderer.Pane(after: tex, rect: SIMD4(-1, -1, 1, 1))], width: 4, height: 4))
        #expect(abs(out[0] - 2.5) < 0.01 && abs(out[1] - 2.5) < 0.01, "\(Array(out.prefix(4)))")
    }

    @Test func overlaysLoupeAndUV() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        let white = try #require(m.makeTexture(from: solidFrame([255, 255, 255])))
        var clip = MetalViewerRenderer.Pane(after: white, rect: SIMD4(-1, -1, 1, 1))
        clip.clippingWarning = true
        let out = try #require(m.renderOffscreen([clip], width: 4, height: 4, clear: MTLClearColor()))
        #expect(out[0] > 250 && Int(out[0]) - Int(out[1]) > 80 && out[1] == out[2], "clipped highlight tinted red: \(Array(out.prefix(3)))")
        let black = try #require(m.makeTexture(from: solidFrame([0, 0, 0])))
        var sh = MetalViewerRenderer.Pane(after: black, rect: SIMD4(-1, -1, 1, 1))
        sh.clippingWarning = true
        let o2 = try #require(m.renderOffscreen([sh], width: 4, height: 4, clear: MTLClearColor()))
        #expect(o2[2] > o2[0], "clipped shadow tinted blue")
        // Focus peaking marks an edge green.
        var edge = [UInt8](repeating: 0, count: 8 * 8 * 4)
        for y in 0..<8 { for x in 4..<8 { let i = (y * 8 + x) * 4; edge[i] = 255; edge[i + 1] = 255; edge[i + 2] = 255 } }
        for i in stride(from: 3, to: edge.count, by: 4) { edge[i] = 255 }
        let et = try #require(m.makeTexture(from: RenderedFrame(width: 8, height: 8, rgba: Data(edge))))
        var pk = MetalViewerRenderer.Pane(after: et, rect: SIMD4(-1, -1, 1, 1))
        pk.focusPeaking = true
        pk.nearest = true
        let o3 = try #require(m.renderOffscreen([pk], width: 8, height: 8, clear: MTLClearColor()))
        let greens = (0..<64).filter { o3[$0 * 4 + 1] > 200 && o3[$0 * 4] < 160 }.count
        #expect(greens >= 8, "edge column highlighted")
        // Loupe: circular mask discards corners.
        var loupe = MetalViewerRenderer.Pane(after: white, rect: SIMD4(-1, -1, 1, 1))
        loupe.circle = true
        loupe.span = SIMD4(0.25, 0.25, 0.75, 0.75)
        let o4 = try #require(m.renderOffscreen([loupe], width: 20, height: 20, clear: MTLClearColor()))
        #expect(o4[0] == 0, "corner outside the circle")
        #expect(o4[(10 * 20 + 10) * 4] == 255, "center inside")
    }

    /// M3: frames Rust wrote into an MTLBuffer (rows padded to 256 bytes) upload
    /// zero-copy (no mips) or by GPU blit (mipmapped), with the right pixels.
    @Test func bufferBackedFramesWithPaddedRows() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        let (w, h, bpr) = (10, 3, 256)
        let buf = try #require(m.device.makeBuffer(length: bpr * h, options: .storageModeShared))
        let p = buf.contents().assumingMemoryBound(to: UInt8.self)
        for y in 0..<h { for x in 0..<w { let i = y * bpr + x * 4; p[i] = 200; p[i + 1] = UInt8(x * 10); p[i + 2] = 30; p[i + 3] = 255 } }
        let f = RenderedFrame(width: w, height: h, format: .rgba8Srgb, bytesPerRow: bpr, backing: .buffer(buf))
        let zero = try #require(m.makeTexture(from: f))
        #expect(zero.buffer === buf, "zero-copy over the Rust-written buffer")
        let mip = try #require(m.makeTexture(from: f, mipmapped: true))
        #expect(mip.buffer == nil && mip.mipmapLevelCount > 1)
        for tex in [zero, mip] {
            var out = [UInt8](repeating: 0, count: 4)
            tex.getBytes(&out, bytesPerRow: w * 4, from: MTLRegionMake2D(7, 2, 1, 1), mipmapLevel: 0)
            #expect(out[0] == 200 && out[1] == 70 && out[2] == 30)
        }
        #expect(ColorMath.sample(f, at: CGPoint(x: 0.75, y: 0.9), radius: 0).map { Int(($0.y * 255).rounded()) } == 70)
    }

    @Test func noDeviceThrows() {
        #expect(throws: MetalViewerRenderer.SetupError.self) { try MetalViewerRenderer(device: nil) }
        #expect(MetalViewerRenderer.textureFormat(for: .rgba8Srgb) == .rgba8Unorm_srgb)
    }
}

@Suite struct TextureUploaderTests {
    @Test func uploadsOffMainCachesAndReuses() async throws {
        let r = try MetalViewerRenderer(pixelFormat: .bgra8Unorm_srgb)
        let up = TextureUploader(renderer: r)
        let a = solidFrame([1, 2, 3], w: 64, h: 32)
        let counter = ViewerCounter()
        #expect(up.texture(for: a) { counter.increment() } == nil, "first call schedules, never blocks")
        // The tiny upload may already have landed; either way no second upload is scheduled.
        _ = up.texture(for: a) { counter.increment() }
        up.drain()
        #expect(counter.value == 1, "no duplicate upload while pending")
        let t = try #require(up.texture(for: a) {})
        #expect(t.width == 64 && t.mipmapLevelCount == 1, "fit frames need no mips")
        let mipped = try #require(up.textureNow(for: solidFrame([5, 5, 5], w: 64, h: 32), mipmapped: true))
        #expect(mipped.mipmapLevelCount > 1)
        #expect(up.cachedIDs.contains(a.id))
        // Frame leaves the screen: its texture is pooled and reused for the next same-size frame.
        up.retain(only: [])
        #expect(up.cachedIDs.isEmpty)
        let b = solidFrame([9, 9, 9], w: 64, h: 32)
        let tb = try #require(up.textureNow(for: b))
        #expect(tb === t, "texture reused, not reallocated")
        #expect(up.stats.uploads == 3 && up.stats.reuses == 1 && up.stats.zeroCopy == 0)
        #expect(up.textureNow(for: b) === tb)
    }

    /// M2 measurement: main-thread cost of showing a new 2560×1707 frame, old vs new path.
    @Test func uploadCostMeasurement() throws {
        let r = try MetalViewerRenderer(pixelFormat: .bgra8Unorm_srgb)
        let frames = (0..<3).map { _ in solidFrame([100, 120, 140], w: 2560, h: 1707) }
        let clock = ContinuousClock()
        // Old path: three fresh textures uploaded synchronously on the calling (main) thread per render.
        let old = clock.measure { for f in frames { _ = r.makeTexture(from: f) } }
        let up = TextureUploader(renderer: r)
        let newMain = clock.measure { for f in frames { _ = up.texture(for: f) {} } }
        let bg = clock.measure { up.drain() }
        up.retain(only: [])
        let next = (0..<3).map { _ in solidFrame([1, 1, 1], w: 2560, h: 1707) }
        let reuse = clock.measure { for f in next { _ = up.textureNow(for: f) } }
        print("VIEWER-MEASURE upload 3×2560x1707: old sync on main \(vms(old)) ms; new main-thread \(vms(newMain)) ms (+ \(vms(bg)) ms background); reuse pass \(vms(reuse)) ms, reuses \(up.stats.reuses)")
        #expect(newMain < old)
        #expect(up.stats.reuses == 2, "the 48 MB pool keeps two 17.5 MB textures; the third is freed (N3)")
        #expect(up.pooledBytes <= up.poolByteLimit)
    }
}

@Suite struct CanvasInteractionTests {
    let size = CGSize(width: 6000, height: 4000)

    @Test func cropHonorsAspectAndDirection() {
        let free = CanvasInteraction.cropRect(from: CGPoint(x: 0.1, y: 0.1), to: CGPoint(x: 0.5, y: 0.9), imageSize: size, aspect: nil)
        #expect(abs(free.width - 0.4) < 1e-9 && abs(free.height - 0.8) < 1e-9)
        let r = CanvasInteraction.cropRect(from: CGPoint(x: 0.1, y: 0.1), to: CGPoint(x: 0.9, y: 0.5), imageSize: size, aspect: (3, 2))
        #expect(abs(r.width * 6000 / (r.height * 4000) - 1.5) < 1e-6, "3:2 in pixels")
        let portrait = CanvasInteraction.cropRect(from: CGPoint(x: 0.5, y: 0.9), to: CGPoint(x: 0.3, y: 0.1), imageSize: size, aspect: (5, 4))
        #expect(abs(portrait.width * 6000 / (portrait.height * 4000) - 0.8) < 1e-6, "taller drag → 4:5 portrait")
        #expect(portrait.maxX <= 0.5 + 1e-9 && portrait.maxY <= 0.9 + 1e-9, "anchored at the drag start")
        let clamped = CanvasInteraction.cropRect(from: CGPoint(x: -1, y: -1), to: CGPoint(x: 2, y: 2), imageSize: size, aspect: nil)
        #expect(clamped == CGRect(x: 0, y: 0, width: 1, height: 1))
    }

    @Test func straightenFoldsToNearestAxis() {
        let a = CanvasInteraction.straightenAngle(from: CGPoint(x: 0, y: 0.5), to: CGPoint(x: 1, y: 0.5 + 0.1 * 1.5), imageSize: size)
        #expect(a < 0 && abs(a + atan(0.1) * 180 / .pi) < 1e-6, "falling horizon → rotate counter-clockwise")
        let v = CanvasInteraction.straightenAngle(from: CGPoint(x: 0.5, y: 0), to: CGPoint(x: 0.5, y: 1), imageSize: size)
        #expect(abs(v) < 1e-9, "a plumb line needs no rotation")
        let w = CanvasInteraction.straightenAngle(from: CGPoint(x: 1, y: 0.5), to: CGPoint(x: 0, y: 0.5), imageSize: size)
        #expect(abs(w) < 1e-9)
    }

    @Test func commitsPerTool() {
        var d = CanvasDrag(tool: .brush, start: CGPoint(x: 0.1, y: 0.1))
        d.move(to: CGPoint(x: 0.1005, y: 0.1))
        d.move(to: CGPoint(x: 0.2, y: 0.2))
        #expect(d.points.count == 2, "tiny moves are not recorded")
        guard case .mask(.brush(.add, let strokes)) = CanvasInteraction.commit(d, imageSize: size, cropAspect: nil, brushSize: 0.1) else {
            Issue.record("brush")
            return
        }
        #expect(strokes[0].size == 0.1 && !strokes[0].erase)
        var e = CanvasDrag(tool: .eraser, start: .zero)
        e.move(to: CGPoint(x: 0.1, y: 0))
        if case .mask(.brush(.subtract, let s)) = CanvasInteraction.commit(e, imageSize: size, cropAspect: nil) { #expect(s[0].erase) } else { Issue.record("eraser") }
        var g = CanvasDrag(tool: .linearGradient, start: CGPoint(x: 0.5, y: 0))
        g.move(to: CGPoint(x: 0.5, y: 0.6))
        #expect(CanvasInteraction.commit(g, imageSize: size, cropAspect: nil) == .mask(.linear(.add, start: CGPoint(x: 0.5, y: 0), end: CGPoint(x: 0.5, y: 0.6))))
        var rg = CanvasDrag(tool: .radialGradient, start: CGPoint(x: 0.5, y: 0.5))
        rg.move(to: CGPoint(x: 0.6, y: 0.5))
        if case .mask(.radial(_, _, let r, _, _)) = CanvasInteraction.commit(rg, imageSize: size, cropAspect: nil) {
            #expect(abs(r.width - 0.1) < 1e-9 && r.height == r.width, "a circle, in long-edge units")
        } else { Issue.record("radial") }
        let still = CanvasDrag(tool: .linearGradient, start: .zero)
        #expect(CanvasInteraction.commit(still, imageSize: size, cropAspect: nil) == .none)
        #expect(CanvasInteraction.commit(CanvasDrag(tool: .radialGradient, start: .zero), imageSize: size, cropAspect: nil) == .none)
        #expect(CanvasInteraction.commit(CanvasDrag(tool: .crop, start: .zero), imageSize: size, cropAspect: nil) == .none)
        #expect(CanvasInteraction.commit(CanvasDrag(tool: .straighten, start: .zero), imageSize: size, cropAspect: nil) == .none)
        #expect(CanvasInteraction.commit(CanvasDrag(tool: .pan, start: .zero), imageSize: size, cropAspect: nil) == .none)
        #expect(CanvasInteraction.commit(CanvasDrag(tool: .colorPicker, start: CGPoint(x: 0.3, y: 0.3)), imageSize: size, cropAspect: nil) == .pickColor(CGPoint(x: 0.3, y: 0.3)))
        #expect(CanvasInteraction.commit(CanvasDrag(tool: .wbPicker, start: .zero), imageSize: size, cropAspect: nil) == .pickWhiteBalance(.zero))
        var c = CanvasDrag(tool: .crop, start: .zero)
        c.move(to: CGPoint(x: 0.5, y: 0.5))
        #expect(CanvasInteraction.commit(c, imageSize: size, cropAspect: nil) == .crop(CGRect(x: 0, y: 0, width: 0.5, height: 0.5)))
        var st = CanvasDrag(tool: .straighten, start: CGPoint(x: 0, y: 0.5))
        st.move(to: CGPoint(x: 1, y: 0.5))
        #expect(CanvasInteraction.commit(st, imageSize: size, cropAspect: nil) == .straighten(-0.0) || CanvasInteraction.commit(st, imageSize: size, cropAspect: nil) == .straighten(0))
    }

    @Test func rotationAndToolMetadata() {
        #expect(CanvasInteraction.rotate(0, by: -1) == 3 && CanvasInteraction.rotate(3, by: 1) == 0 && CanvasInteraction.rotate(1, by: 1) == 2)
        for t in CanvasTool.allCases { #expect(!t.viewerLabel.isEmpty && !t.viewerSymbol.isEmpty) }
        #expect(CanvasTool.allCases.filter(\.editsMask) == [.brush, .eraser, .linearGradient, .radialGradient])
    }
}

@Suite struct ColorMathTests {
    @Test func labOfReferenceColors() {
        let white = ColorMath.lab(fromSRGB: SIMD3(1, 1, 1))
        #expect(abs(white.x - 100) < 0.01 && abs(white.y) < 0.01 && abs(white.z) < 0.01)
        let black = ColorMath.lab(fromSRGB: SIMD3(0, 0, 0))
        #expect(abs(black.x) < 1e-6)
        let red = ColorMath.lab(fromSRGB: SIMD3(1, 0, 0))
        #expect(abs(red.x - 53.24) < 0.1 && abs(red.y - 80.09) < 0.2 && abs(red.z - 67.20) < 0.2)
        let gray = ColorMath.lab(fromSRGB: SIMD3(0.5, 0.5, 0.5))
        #expect(abs(gray.x - 53.39) < 0.1)
    }

    @Test func samplingFrames() {
        let f = solidFrame([255, 0, 128], w: 10, h: 10)
        let c = ColorMath.sample(f, at: CGPoint(x: 0.5, y: 0.5))!
        #expect(c.x == 1 && c.y == 0 && abs(c.z - 128.0 / 255) < 1e-9)
        let white = try! #require(ColorMath.sample(floatFrame(1), at: .zero))
        #expect(abs(white.x - 1) < 1e-4 && abs(white.y - 1) < 1e-4 && abs(white.z - 1) < 1e-4, "16F linear P3 white → sRGB white")
        #expect(ColorMath.sample(f, at: CGPoint(x: 5, y: -5)) != nil, "clamped")
    }

    @MainActor
    @Test func readoutTexts() {
        let r = CursorReadout()
        #expect(r.rgbText == nil && r.labText == nil)
        r.update(rgb: SIMD3(1, 0.5, 0), at: CGPoint(x: 0.1, y: 0.2))
        #expect(r.rgbText == "R 255  G 128  B   0")
        #expect(r.labText?.hasPrefix("L ") == true && r.position == CGPoint(x: 0.1, y: 0.2))
        r.pin(SIMD3(0, 0, 0))
        #expect(r.pinned == SIMD3(0, 0, 0))
    }
}

@Suite struct ViewerMathTests {
    @Test func zoomLabelIsHonest() {
        #expect(ViewerModel.zoomLabel(zoom: .fit, effective: 0.4, density: 0.4) == "Fit")
        #expect(ViewerModel.zoomLabel(zoom: .actual, effective: 1, density: 0.4) == "100% (preview)")
        #expect(ViewerModel.zoomLabel(zoom: .actual, effective: 1, density: 1) == "100%")
        #expect(ViewerModel.zoomLabel(zoom: .scale(2), effective: 2, density: 1) == "200%", "200% magnifies true 1:1 pixels")
        #expect(ViewerModel.zoomLabel(zoom: .scale(0.5), effective: 0.5, density: 0.4) == "50% (preview)")
    }

    @Test func imageSizePrefersMetadataUnlessAspectDisagrees() {
        let meta = CGSize(width: 6000, height: 4000)
        #expect(ViewerModel.imageSize(meta: meta, frame: nil) == meta)
        #expect(ViewerModel.imageSize(meta: meta, frame: CGSize(width: 1616, height: 1080)) == meta)
        #expect(ViewerModel.imageSize(meta: meta, frame: CGSize(width: 1080, height: 1620)) == CGSize(width: 4000, height: 6000),
                "unrotated metadata corrected to the frame's portrait orientation")
        #expect(ViewerModel.imageSize(meta: meta, frame: CGSize(width: 8, height: 6)) == meta, "only a 90° swap is trusted")
        #expect(ViewerModel.imageSize(meta: .zero, frame: CGSize(width: 10, height: 5)) == CGSize(width: 10, height: 5))
    }

    @Test func previewFrameFromCGImage() throws {
        let ctx = CGContext(data: nil, width: 3, height: 2, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        ctx.setFillColor(CGColor(srgbRed: 1, green: 0, blue: 0, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: 3, height: 2))
        let f = try #require(PreviewFrames.convert(ctx.makeImage()!))
        #expect(f.width == 3 && f.height == 2 && Array(f.rgba.prefix(3)) == [255, 0, 0])
    }
}

/// Thread-safe counter for callbacks from background queues.
final class ViewerCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func increment() { lock.withLock { n += 1 } }
    var value: Int { lock.withLock { n } }
}

func vms(_ d: Duration) -> String {
    let (s, a) = d.components
    return String(format: "%.2f", Double(s) * 1000 + Double(a) / 1e15)
}
