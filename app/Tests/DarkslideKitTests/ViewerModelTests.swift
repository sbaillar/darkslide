import CoreGraphics
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Embedded previews for tests: a solid image per URL, with optional latency.
final class ViewerFakePreviews: PreviewProvider, @unchecked Sendable {
    private let lock = NSLock()
    private var _calls: [URL] = []
    var latency: Duration

    init(latency: Duration = .zero) { self.latency = latency }

    var calls: [URL] { lock.withLock { _calls } }

    func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
        lock.withLock { _calls.append(url) }
        if latency > .zero { try await Task.sleep(for: latency) }
        let ctx = CGContext(data: nil, width: 60, height: 40, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        ctx.setFillColor(CGColor(srgbRed: 0.2, green: 0.4, blue: 0.6, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: 60, height: 40))
        return ctx.makeImage()!
    }
}

/// Mock sessions whose photos have file URLs (so previews load).
struct ViewerURLSessionStore: SessionStore {
    let inner: MockSessionStore
    init(count: Int) { inner = MockSessionStore(counts: [count]) }
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] {
        try await inner.photos(in: session).map { p in
            var p = p
            p.url = URL(fileURLWithPath: "/fake/\(p.id.raw).ARW")
            return p
        }
    }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws {}
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
}

extension ViewerSizedRenderer: CapableRenderer {
    var capabilities: RendererCapabilities { RendererCapabilities(regions: true, floatFrames: false, rendersEdits: true) }
}

/// Renders frames of the requested size (region-aware), cancellation-aware, with latency.
final class ViewerSizedRenderer: Renderer, @unchecked Sendable {
    private let lock = NSLock()
    private var _requests: [RenderRequest] = []
    private var _cancelled = 0
    var latency: Duration

    init(latency: Duration = .zero) { self.latency = latency }
    var requests: [RenderRequest] { lock.withLock { _requests } }
    var cancelled: Int { lock.withLock { _cancelled } }

    func render(_ photo: Photo, edit: EditState, request: RenderRequest) async throws -> RenderedFrame {
        lock.withLock { _requests.append(request) }
        do {
            if latency > .zero { try await Task.sleep(for: latency) }
            try Task.checkCancellation()
        } catch {
            lock.withLock { _cancelled += 1 }
            throw error
        }
        let aspect = photo.meta.aspectRatio
        var w: Int, h: Int
        if let s = request.scale, let r = request.region {
            w = max(1, Int((Double(photo.meta.pixelWidth) * s * r.width).rounded()))
            h = max(1, Int((Double(photo.meta.pixelHeight) * s * r.height).rounded()))
        } else {
            w = request.maxPixelSize
            h = Int((Double(w) / aspect).rounded())
            if aspect < 1 { (w, h) = (Int((Double(request.maxPixelSize) * aspect).rounded()), request.maxPixelSize) }
        }
        let v = UInt8(clamping: 128 + Int(edit.exposure.ev * 40))
        var px = [UInt8](repeating: 255, count: w * h * 4)
        for i in stride(from: 0, to: px.count, by: 4) { px[i] = v; px[i + 1] = 100; px[i + 2] = 50 }
        return RenderedFrame(width: w, height: h, format: .rgba8Srgb, bytesPerRow: w * 4, backing: .data(Data(px)),
                             region: request.region)
    }
}

@MainActor
func makeViewerApp(renderer: any Renderer = ViewerSizedRenderer(), previews: ViewerFakePreviews? = ViewerFakePreviews(),
                   count: Int = 20) -> AppModel {
    let settings = makeSettings()
    settings.onboardingComplete = true
    settings.autoAdvance = false
    let m = AppModel(sessions: ViewerURLSessionStore(count: count), images: MockImageProvider(latency: .zero), renderer: renderer,
                     importer: MockImportService(bytesPerSecond: 1e12),
                     archive: MockArchiveService(target: "/nonexistent", interval: .seconds(3600), step: .milliseconds(1),
                                                 isReachable: { _ in true }),
                     ai: MockAIService(), settings: settings, previews: previews)
    m.watchCards = false
    m.viewer.tileDelay = .zero
    return m
}

@MainActor
@Suite struct ViewerModelTests {
    @Test func rendersCoalesceWhileInFlight() async {
        let r = FakeRenderer()
        let m = makeModel(renderer: r)
        await m.start()
        #expect(await waitUntil { m.viewer.renderCount == 1 })
        r.setGated(true)
        m.updateEdit(label: "a", coalesceKey: "a") { $0.exposure.ev = 0.1 }
        #expect(await waitUntil { r.isWaiting })
        for i in 2...20 { m.updateEdit(label: "a", coalesceKey: "a") { $0.exposure.ev = Double(i) / 10 } }
        r.setGated(false)
        r.release()
        #expect(await waitUntil { m.viewer.renderCount == 3 })
        try? await Task.sleep(for: .milliseconds(30))
        #expect(m.viewer.renderCount == 3, "20 slider moves while busy → 1 follow-up render, not 20")
        #expect(r.calls.last?.1.exposure.ev == 2.0, "the follow-up uses the latest edit")
    }

    /// H1: navigation shows the embedded preview immediately, never the previous photo's frame.
    @Test func previewFirstThenRenderAndNeverAStaleFrame() async {
        let r = ViewerSizedRenderer(latency: .milliseconds(300))
        let m = makeViewerApp(renderer: r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 600, height: 400), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        let first = m.focusedID
        #expect(await waitUntil { m.viewer.hasPreview(m.visibleIDs[1]) }, "neighbour prefetched")
        m.perform(.move(.right, extend: false))
        #expect(m.viewer.frame == nil, "the previous photo's render is not shown")
        #expect(m.viewer.displayFrame != nil && m.viewer.showingPreview, "prefetched preview shows synchronously")
        #expect(m.viewer.drawPlan(loupeAt: nil).quads.first?.frame == m.viewer.previewFrame)
        #expect(await waitUntil { m.viewer.frame != nil })
        #expect(m.viewer.frameID == m.focusedID && m.viewer.frameID != first && !m.viewer.showingPreview)
    }

    /// C2/H1: the stale render's Task is cancelled when focus moves.
    @Test func staleRenderIsCancelledOnNavigation() async {
        // The superseded renders run until cancelled, however late a loaded machine notices
        // they started; the last one (latency zero) lands.
        let r = ViewerSizedRenderer(latency: .seconds(60))
        let m = makeViewerApp(renderer: r)
        await m.start()
        // Each move supersedes a render already under way (an unstarted one has nothing to cancel).
        #expect(await waitUntil { r.requests.count >= 1 })
        m.perform(.move(.right, extend: false))
        #expect(await waitUntil { r.requests.count >= 2 })
        r.latency = .zero
        m.perform(.move(.right, extend: false))
        #expect(await waitUntil { r.cancelled >= 2 }, "two superseded renders cancelled")
        #expect(await waitUntil { m.viewer.frame != nil })
        #expect(m.viewer.frameID == m.visibleIDs[2])
    }

    @Test func staleRenderIsDroppedWhenFocusMoves() async {
        let r = FakeRenderer()
        let m = makeModel(renderer: r)
        await m.start()
        #expect(await waitUntil { m.viewer.renderCount == 1 })
        r.setGated(true)
        m.perform(.move(.right, extend: false))
        #expect(await waitUntil { r.isWaiting })
        let second = m.focusedID
        m.perform(.move(.right, extend: false))
        r.setGated(false)
        r.release()
        #expect(await waitUntil { m.viewer.renderCount == 2 })
        #expect(m.viewer.frameID == m.focusedID && m.viewer.frameID != second, "only the current photo's frame is shown")
        m.focus(nil, extend: false)
        #expect(m.viewer.frame == nil && m.viewer.frameID == nil && m.viewer.histogram == nil && m.viewer.displayFrame == nil)
    }

    @Test func prefetchesNeighboursOnly() async {
        let p = ViewerFakePreviews()
        let m = makeViewerApp(previews: p)
        await m.start()
        m.focus(m.visibleIDs[10], extend: false)
        let ids = m.visibleIDs
        #expect(await waitUntil { (7...13).allSatisfy { m.viewer.hasPreview(ids[$0]) } })
        #expect(!m.viewer.hasPreview(ids[15]) && !m.viewer.hasPreview(ids[5]))
        m.viewer.prefetchRadius = 0
        m.viewer.prefetchNeighbours()
    }

    @Test func previewFailureAndNoProvider() async {
        let m = makeModel() // demo: no previews, photos have no URLs
        await m.start()
        #expect(await waitUntil { m.viewer.frame != nil })
        #expect(!m.viewer.showingPreview && m.viewer.navigatorImage != nil)
    }

    @Test func fastNavigationShowsPreviewsWithoutRendering() async {
        let r = ViewerSizedRenderer()
        let m = makeViewerApp(renderer: r)
        await m.start()
        #expect(await waitUntil { m.viewer.frame != nil })
        let before = r.requests.count
        m.viewer.setFastNavigation(true)
        m.viewer.setFastNavigation(true)
        for _ in 0..<5 { m.perform(.move(.right, extend: false)) }
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        #expect(r.requests.count == before, "no develop renders during key repeat")
        #expect(m.viewer.showingPreview)
        m.viewer.setFastNavigation(false)
        #expect(await waitUntil { m.viewer.frame != nil })
        #expect(r.requests.count == before + 1, "one render where you landed")
    }

    @Test func previewEditRendersTheHoveredStyle() async {
        let r = ViewerSizedRenderer()
        let m = makeViewerApp(renderer: r)
        await m.start()
        #expect(await waitUntil { m.viewer.frame != nil })
        var e = EditState()
        e.exposure.ev = 2
        m.viewer.setPreviewEdit(e)
        m.viewer.setPreviewEdit(e)
        #expect(await waitUntil { m.viewer.frame.map { $0.rgba[0] } == 208 })
        m.viewer.setPreviewEdit(nil)
        #expect(await waitUntil { m.viewer.frame.map { $0.rgba[0] } == 128 })
        #expect(m.focusedEdit.isIdentity, "hover preview never edits")
    }

    /// H1: the WB picker only samples the focused photo's develop render.
    @Test func wbPickerIgnoresPreviewsAndOtherPhotos() async {
        let r = ViewerSizedRenderer(latency: .milliseconds(400))
        let m = makeViewerApp(renderer: r)
        await m.start()
        #expect(await waitUntil { m.viewer.frame != nil })
        #expect(m.viewer.sampleFrame(at: CGPoint(x: 0.5, y: 0.5)) != nil)
        m.perform(.move(.right, extend: false))
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        #expect(m.viewer.sampleFrame(at: CGPoint(x: 0.5, y: 0.5)) == nil, "preview only → no WB sample")
        m.wbPickerActive = true
        m.viewer.pickWhiteBalance(at: CGPoint(x: 0.5, y: 0.5))
        // Nothing picked from a preview; the picker stays on for another try (loupe UX).
        #expect(m.wbPickerActive && m.focusedEdit.isIdentity)
    }

    @Test func wbPickerSamplesTheFrame() async {
        let m = makeModel()
        await m.start()
        #expect(await waitUntil { m.viewer.frame != nil })
        let rgb = m.viewer.sampleFrame(at: CGPoint(x: 0.5, y: 0.5))!
        #expect(abs(rgb.0 - 128.0 / 255) < 1e-9 && abs(rgb.1 - 100.0 / 255) < 1e-9)
        #expect(m.viewer.sampleFrame(at: CGPoint(x: -3, y: 9)) != nil, "clamped to the frame")
        m.wbPickerActive = true
        #expect(m.viewer.activeTool == .wbPicker)
        m.viewer.pickWhiteBalance(at: CGPoint(x: 0.5, y: 0.5))
        #expect(m.wbPickerActive && !m.focusedEdit.whiteBalance.asShot, "applied; the picker stays on until Esc or Done")
    }

    /// H3/UX2: zoom steps, 100% requests a 1:1 tile of the visible region, label turns honest.
    @Test func zoomRequestsTrueOneToOneTiles() async throws {
        let r = ViewerSizedRenderer()
        let m = makeViewerApp(renderer: r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        // Wait for the layout and the render at its size to settle (the image size is
        // known once the photo loads), then read the fit size (review M3: no race).
        #expect(await waitUntil {
            m.viewer.layout.main != nil && m.viewer.frame.map { max($0.width, $0.height) == m.viewer.renderPixelSize } ?? false
        })
        let fitEdge = m.viewer.renderPixelSize
        #expect(m.viewer.zoomLabel == "Fit" && m.viewer.tileRequest() == nil)
        let fit = m.viewer.layout.main!.image
        #expect(fitEdge == Int((max(fit.width, fit.height) * 2).rounded(.up)), "fit render at the fitted image's device size (no mips needed)")
        m.viewer.cursor = CGPoint(x: 612, y: 412)
        m.perform(.zoomToggleAtCursor)
        #expect(m.viewer.state.zoom == .actual)
        let req = m.viewer.tileRequest()
        #expect(req?.scale == 1)
        #expect(req.map { $0.region!.width < 0.9 && $0.region!.contains(CGPoint(x: 0.5, y: 0.5)) } == true)
        // Generous timeout: the full suite runs hundreds of renders in parallel.
        #expect(await waitUntil(timeout: .seconds(15)) { m.viewer.tile != nil })
        #expect(m.viewer.zoomLabel == "100%", "true 1:1 pixels on screen")
        let tile = try #require(m.viewer.tile)
        #expect(tile.frame.width > 2000 && tile.scale == 1)
        let plan = m.viewer.drawPlan(loupeAt: nil)
        #expect(plan.quads.count == 2 && plan.quads[1].frame == tile.frame && plan.quads[1].nearest)
        // Already covered: panning a little doesn't re-render.
        let n = r.requests.count
        m.viewer.pan(by: CGSize(width: 5, height: 0))
        try? await Task.sleep(for: .milliseconds(30))
        #expect(r.requests.count == n, "\(r.requests)")
        // 200% reuses the 1:1 tile (magnified), 50% goes back to fit density.
        m.viewer.zoomIn()
        #expect(m.viewer.state.zoom == .scale(2) && m.viewer.zoomLabel == "200%")
        m.viewer.zoomOut()
        m.viewer.zoomOut()
        #expect(m.viewer.state.zoom == .scale(0.5))
        m.viewer.zoomOut()
        #expect(m.viewer.state.zoom == .fit)
        // Editing drops the tile; the label is honest until the new tile lands.
        m.viewer.zoom(to: .actual)
        m.updateEdit(label: "ev") { $0.exposure.ev = 1 }
        #expect(m.viewer.tile == nil && m.viewer.zoomLabel == "100% (preview)")
        #expect(await waitUntil { m.viewer.tile != nil && m.viewer.frame.map { $0.rgba[0] } == 168 })
    }

    @Test func continuousZoomClampsAndFallsBackToFit() async {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        // Wait for the develop render (not the fake preview, whose aspect differs).
        #expect(await waitUntil { m.viewer.displayFrame != nil && !m.viewer.showingPreview })
        let fit = m.viewer.effectiveScale
        #expect(fit > 0 && fit < 1)
        m.viewer.zoom(by: 3, at: CGPoint(x: 612, y: 412))
        #expect(abs(m.viewer.effectiveScale - fit * 3) < 1e-6)
        m.viewer.zoom(by: 1000, at: nil)
        #expect(m.viewer.state.zoom == .scale(ViewerGeometry.maxScale))
        m.viewer.zoom(by: 1e-6, at: nil)
        #expect(m.viewer.state.zoom == .fit)
        m.viewer.zoom(by: .nan, at: nil)
        #expect(m.viewer.state.zoom == .fit)
    }

    /// H4: pan and zoom-at-cursor work in pane space (compare panes are half width).
    @Test func comparePanClampsToThePane() async {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1202, height: 800), backingScale: 2)
        m.perform(.togglePinCompare)
        m.perform(.move(.right, extend: false))
        m.viewer.state.comparePinned = m.visibleIDs[0]
        #expect(await waitUntil { m.viewer.compareFrame != nil })
        let paneW: CGFloat = 600
        m.viewer.cursor = CGPoint(x: 601 + 300, y: 400)
        m.viewer.zoom(to: .actual, anchor: m.viewer.cursor)
        m.viewer.pan(by: CGSize(width: 1e6, height: 0))
        let img = m.focusedPhoto!.pixelSize
        #expect(m.viewer.state.pan.x == (img.width / 2 - paneW) / 2, "slack against the 600 pt pane, not the view")
        let main = m.viewer.layout.main!
        #expect(abs(main.image.minX - main.frame.minX) < 1)
    }

    @Test func zoomAtCursorKeepsThePointAndPanClamps() async {
        let m = makeModel()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1200, height: 800), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        m.viewer.cursor = CGPoint(x: 100, y: 100)
        m.perform(.zoomToggleAtCursor)
        #expect(m.viewer.state.zoom == .actual)
        #expect(m.viewer.state.pan.x > 0 && m.viewer.state.pan.y > 0, "zoomed toward the top-left corner")
        let before = m.viewer.state.pan
        m.viewer.pan(by: CGSize(width: -50, height: 0))
        #expect(m.viewer.state.pan.x == before.x - 50)
        m.viewer.pan(by: CGSize(width: 1e6, height: 1e6))
        let p = m.focusedPhoto!.meta
        let slack = CGPoint(x: (CGFloat(p.pixelWidth) / 2 - 1200) / 2, y: (CGFloat(p.pixelHeight) / 2 - 800) / 2)
        #expect(m.viewer.state.pan == slack, "clamped to the image edge (100% image is half the pixel size in points)")
        m.perform(.escape)
        #expect(m.viewer.state.zoom == .fit && m.viewer.state.pan == .zero)
        m.viewer.pan(by: CGSize(width: 10, height: 10))
        #expect(m.viewer.state.pan == .zero, "no panning at fit")
        m.browserMode = .grid
        m.perform(.zoomToggleAtCursor)
        #expect(m.browserMode == .filmstrip, "space in the grid opens the viewer")
    }

    @Test func navigatorCentersOnAPoint() async {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        m.viewer.center(on: CGPoint(x: 0.9, y: 0.9))
        #expect(m.viewer.state.pan == .zero, "no-op at fit")
        m.viewer.zoom(to: .actual)
        m.viewer.center(on: CGPoint(x: 0.5, y: 0.5))
        #expect(m.viewer.state.pan == .zero)
        m.viewer.center(on: CGPoint(x: 0, y: 0))
        let v = m.viewer.layout.main!.visibleRegion
        #expect(v.minX == 0 && v.minY == 0, "clamped to the corner")
        #expect(m.viewer.navigatorImage != nil)
    }

    /// M1: hover updates the readout without touching observed viewer state.
    @Test func hoverUpdatesReadoutOnly() async {
        let m = makeModel()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        let version = m.viewer.displayVersion
        let state = m.viewer.state
        var redraws = 0
        m.viewer.onCursorRedraw = { redraws += 1 }
        m.viewer.hover(at: CGPoint(x: 612, y: 412))
        #expect(m.viewer.cursorReadout.rgb.map { abs($0.x - 128.0 / 255) < 1e-9 } == true)
        #expect(m.viewer.cursorReadout.lab != nil)
        #expect(m.viewer.displayVersion == version && m.viewer.state == state, "hover never invalidates the viewer")
        #expect(redraws == 0, "no Metal redraw without the loupe")
        m.viewer.loupe = true
        m.viewer.hover(at: CGPoint(x: 600, y: 400))
        #expect(redraws == 1)
        #expect(m.viewer.drawPlan(loupeAt: m.viewer.cursor).quads.last?.circle == true)
        m.viewer.hover(at: nil)
        #expect(m.viewer.cursorReadout.rgb == nil && redraws == 2)
        m.viewer.clippingWarning = true
        m.viewer.focusPeaking = true
        #expect(m.viewer.drawPlan(loupeAt: nil).quads.first?.clippingWarning == true)
    }

    @Test func canvasToolsCommitIntoTheEdit() async {
        let m = makeModel()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        let img = m.viewer.layout.main!.image
        func at(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: img.minX + x * img.width, y: img.minY + y * img.height) }
        // Pan tool: no drag.
        m.viewer.beginCanvasDrag(at: at(0.1, 0.1))
        #expect(m.viewer.activeDrag == nil && m.viewer.endCanvasDrag() == .none)
        // Brush without a layer creates one and selects it.
        m.viewer.canvasTool = .brush
        m.viewer.beginCanvasDrag(at: at(0.2, 0.2))
        m.viewer.updateCanvasDrag(to: at(0.3, 0.3))
        m.viewer.updateCanvasDrag(to: at(0.4, 0.3))
        m.viewer.endCanvasDrag()
        #expect(m.focusedEdit.layers.count == 1 && m.viewer.selectedLayerID == m.focusedEdit.layers[0].id)
        // A gradient goes into the selected layer.
        m.viewer.canvasTool = .linearGradient
        m.viewer.beginCanvasDrag(at: at(0.5, 0))
        m.viewer.updateCanvasDrag(to: at(0.5, 0.5))
        m.viewer.endCanvasDrag()
        #expect(m.focusedEdit.layers.count == 1 && m.focusedEdit.layers[0].mask.shapes.count == 2)
        #expect(m.viewer.selectedMask?.shapes.count == 2)
        // Straighten.
        m.viewer.canvasTool = .straighten
        m.viewer.beginCanvasDrag(at: at(0, 0.5))
        m.viewer.updateCanvasDrag(to: at(1, 0.6))
        m.viewer.endCanvasDrag()
        // The line falls to the right: levelling it turns the content counter-clockwise,
        // a positive ds-core angle.
        #expect(m.focusedEdit.crop.angle > 0)
        // Crop honours the edit's aspect lock.
        m.updateEdit(label: "aspect") { $0.crop.aspect = .r1x1 }
        m.viewer.canvasTool = .crop
        m.viewer.beginCanvasDrag(at: at(0.1, 0.1))
        m.viewer.updateCanvasDrag(to: at(0.9, 0.5))
        m.viewer.endCanvasDrag()
        let c = m.viewer.cropRect
        let px = m.focusedPhoto!.pixelSize
        #expect(abs(c.width * px.width - c.height * px.height) < 1, "1:1 in pixels")
        // Colour picker pins a value; cancel drops a drag.
        m.viewer.canvasTool = .colorPicker
        m.viewer.beginCanvasDrag(at: at(0.5, 0.5))
        m.viewer.endCanvasDrag()
        #expect(m.viewer.cursorReadout.pinned != nil)
        m.viewer.canvasTool = .radialGradient
        m.viewer.beginCanvasDrag(at: at(0.5, 0.5))
        m.viewer.cancelCanvasDrag()
        #expect(m.viewer.activeDrag == nil)
        // Rotate.
        m.viewer.rotate(by: -1)
        #expect(m.viewer.rotation == 3)
        m.focus(nil, extend: false)
        #expect(m.viewer.endCanvasDrag() == .none && m.viewer.selectedMask == nil && m.viewer.rotation == 0)
        m.viewer.rotate(by: 1)
    }

    @Test func renderFailureIsReportedAndRecovers() async {
        let r = FakeRenderer()
        r.failWith = MockError.invalid("GPU lost")
        let m = makeModel(renderer: r)
        await m.start()
        #expect(await waitUntil { m.viewer.renderError == "GPU lost" })
        r.failWith = nil
        m.perform(.move(.right, extend: false))
        #expect(await waitUntil { m.viewer.renderError == nil && m.viewer.frame != nil })
    }

    @Test func beforeAfterIsCachedPerPhotoAndCompareRenders() async {
        let r = ViewerSizedRenderer()
        let m = makeViewerApp(renderer: r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 600, height: 400), backingScale: 2)
        #expect(await waitUntil { m.viewer.renderCount >= 1 })
        m.perform(.toggleBeforeAfter)
        #expect(await waitUntil { m.viewer.beforeFrame != nil })
        #expect(m.viewer.drawPlan(loupeAt: nil).quads.first?.before != nil) // no trap when a slow run times out
        let n = r.requests.count
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.5 }
        #expect(await waitUntil { m.viewer.frame.map { $0.rgba[0] } == 148 })
        #expect(r.requests.count == n + 1, "the before frame is cached, not re-rendered per edit")
        m.perform(.togglePinCompare)
        #expect(m.viewer.state.comparePinned == m.focusedID)
        #expect(await waitUntil { m.viewer.compareFrame != nil })
        m.perform(.togglePinCompare)
        #expect(m.viewer.state.comparePinned == nil)
    }

    @Test func viewerSizeChangeRerendersOnlyWhenGrowing() async {
        let r = FakeRenderer()
        let m = makeModel(renderer: r)
        await m.start()
        #expect(await waitUntil { m.viewer.renderCount == 1 })
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        #expect(await waitUntil { m.viewer.renderCount == 2 })
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        try? await Task.sleep(for: .milliseconds(20))
        #expect(m.viewer.renderCount == 2)
    }

    /// Measurement: time from navigation to first pixels, preview vs develop render.
    @Test func navigationLatencyMeasurement() async {
        let clock = ContinuousClock()
        let m = makeViewerApp(renderer: RustRenderer(), previews: ViewerFakePreviews(latency: .milliseconds(8)))
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1280, height: 850), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        #expect(await waitUntil { m.viewer.hasPreview(m.visibleIDs[1]) })
        let t0 = clock.now
        m.perform(.move(.right, extend: false))
        let previewAt = m.viewer.displayFrame != nil ? clock.now - t0 : nil
        #expect(previewAt != nil)
        #expect(await waitUntil { m.viewer.frame != nil })
        let renderAt = clock.now - t0
        let t1 = clock.now
        _ = try? await RustRenderer().render(m.focusedPhoto!, edit: EditState(), maxPixelSize: m.viewer.renderPixelSize)
        print("VIEWER-MEASURE direct RustRenderer \(m.viewer.renderPixelSize) px: \(vms(clock.now - t1)) ms")
        print("VIEWER-MEASURE navigation → first pixels: prefetched preview \(vms(previewAt ?? .zero)) ms; develop render (Rust, 2560 px) \(vms(renderAt)) ms")
        #expect((previewAt ?? .seconds(9)) < renderAt)
    }
}
