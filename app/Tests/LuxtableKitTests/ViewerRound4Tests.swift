import CoreGraphics
import LuxtableAI
import Foundation
import Metal
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

// MARK: - L9: AI mask overlays

@MainActor
@Suite struct MaskOverlayCacheTests {
    nonisolated static func mask(_ w: Int = 4, _ h: Int = 4, fill: UInt8 = 255) -> VisionMask {
        VisionMask(width: w, height: h, bytes: [UInt8](repeating: fill, count: w * h))
    }

    @Test func keyedByPhotoAndTargetNotShapeIndex() {
        let c = MaskOverlayCache()
        c.set(Self.mask(fill: 200), photo: PhotoID(1), target: .subject)
        c.set(Self.mask(fill: 50), photo: PhotoID(1), target: .person)
        #expect(c.mask(photo: PhotoID(1), target: .subject)?.bytes.first == 200)
        #expect(c.mask(photo: PhotoID(1), target: .person)?.bytes.first == 50)
        // Removing an earlier shape can't make another shape show the wrong bitmap: an edit
        // keeps whatever targets its AI shapes use, regardless of order.
        var e = EditState()
        e.layers = [LayerEdit(id: 0, name: "L", mask: MaskEdit(shapes: [.brush(.add, []), .ai(.add, .person)]))]
        c.prune(photo: PhotoID(1), edit: e)
        #expect(c.mask(photo: PhotoID(1), target: .subject) == nil)
        #expect(c.mask(photo: PhotoID(1), target: .person) != nil)
        #expect(MaskOverlayCache.targets(in: e) == [.person])
        #expect(MaskOverlayCache.targets(in: MaskEdit(shapes: [.ai(.add, .sky), .ai(.subtract, .sky), .ai(.add, .subject)])) == [.sky, .subject])
    }

    @Test func maskBytesCountTowardTheCap() {
        let c = MaskOverlayCache(byteLimit: 150)
        c.set(Self.mask(10, 10), photo: PhotoID(1), target: .subject)
        #expect(c.bytes == 100, "1 byte per pixel; the tint is a shader uniform, not an RGBA copy")
        c.set(Self.mask(10, 10), photo: PhotoID(2), target: .subject)
        #expect(c.count == 1 && c.mask(photo: PhotoID(1), target: .subject) == nil, "LRU evicted")
        #expect(c.bytes <= c.byteLimit)
    }

    @Test func evictRetainAndRemoveAll() {
        let c = MaskOverlayCache()
        for p in 1...3 { c.set(Self.mask(), photo: PhotoID(Int64(p)), target: .subject) }
        c.evict(photo: PhotoID(1))
        #expect(c.count == 2)
        c.retain(photos: [PhotoID(2)])
        #expect(c.count == 1 && c.mask(photo: PhotoID(2), target: .subject) != nil)
        c.removeAll()
        #expect(c.count == 0 && c.bytes == 0)
    }

    @Test func regeneratesOnceOnDemand() async {
        let calls = LockedCounter(), finished = LockedCounter()
        let c = MaskOverlayCache()
        let gen: MaskOverlayCache.Generator = { _, target in
            calls.increment()
            defer { finished.increment() }
            try? await Task.sleep(for: .milliseconds(5))
            return target == .sky ? nil : Self.mask()
        }
        var ready = 0
        #expect(!c.request(photo: PhotoID(1), target: .subject, generator: gen) { ready += 1 })
        #expect(!c.request(photo: PhotoID(1), target: .subject, generator: gen) { ready += 1 }, "second request while in flight")
        #expect(await waitUntil { ready == 1 })
        #expect(calls.value == 1 && c.generated == 1)
        #expect(c.request(photo: PhotoID(1), target: .subject, generator: gen) { ready += 1 }, "cached now")
        _ = c.request(photo: PhotoID(1), target: .sky, generator: gen) { ready += 1 }
        // Deterministic (review M3): wait for the generator itself, then let its result land.
        #expect(await waitUntil { finished.value == 2 })
        try? await Task.sleep(for: .milliseconds(20))
        #expect(ready == 1 && c.mask(photo: PhotoID(1), target: .sky) == nil, "no mask: nothing stored")
        #expect(calls.value == 2)
    }

    @Test func viewerRegeneratesAIMaskOverlaysAfterReload() async throws {
        let m = makeViewerApp()
        let masker = AIMasker { _, _ in VisionMask(width: 8, height: 8, bytes: [UInt8](repeating: 255, count: 64)) }
        m.viewer.masker = masker
        m.viewer.masks = MaskOverlayCache() // the shared one is used by tests running in parallel
        await m.start()
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        let id = m.focusedID!
        // An AI shape as it comes back from a sidecar: shape only, no bitmap anywhere.
        m.updateEdits([id], label: "seed") { _, e in
            e.layers = [LayerEdit(id: 0, name: "Subject", mask: MaskEdit(shapes: [.ai(.add, .subject)]))]
        }
        m.viewer.selectedLayerID = 0
        #expect(m.viewer.selectedAIMaskKeys.isEmpty, "not generated yet")
        #expect(await waitUntil { !m.viewer.selectedAIMaskKeys.isEmpty }, "regenerated from the preview")
        let key = m.viewer.selectedAIMaskKeys[0]
        #expect(m.viewer.masks.mask(photo: key.photo, target: key.target)?.width == 8)
        // The draw plan hands the mask to the Metal pass (main image quads only).
        m.viewer.setViewerSize(CGSize(width: 600, height: 400), backingScale: 2)
        let plan = m.viewer.drawPlan(loupeAt: nil)
        #expect(plan.quads.first?.masks == [key])
        #expect(m.viewer.masks.generated == 1)
    }

    /// Stored strokes and the live brush/eraser drag go to the shader as segments.
    @Test func brushStrokesReachTheShaderPlan() async throws {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 600, height: 400), backingScale: 2)
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        let id = m.focusedID!
        let stroke = BrushStroke(points: [CGPoint(x: 0.2, y: 0.2), CGPoint(x: 0.4, y: 0.2), CGPoint(x: 0.4, y: 0.6)], size: 0.1)
        m.updateEdits([id], label: "seed") { _, e in
            e.layers = [LayerEdit(id: 0, name: "Brush", mask: MaskEdit(shapes: [.brush(.add, [stroke])]))]
        }
        #expect(m.viewer.overlaySegments.isEmpty, "no layer selected: no overlay")
        m.viewer.selectedLayerID = 0
        let stored = m.viewer.overlaySegments
        #expect(stored.count == 2 && stored.allSatisfy { $0.kind == 0 && abs($0.radius - 0.05) < 1e-6 })
        #expect(stored[0].ab == SIMD4(0.2, 0.2, 0.4, 0.2))
        m.viewer.canvasTool = .eraser
        m.viewer.beginCanvasDrag(at: CGPoint(x: 300, y: 200))
        m.viewer.updateCanvasDrag(to: CGPoint(x: 320, y: 200))
        let live = m.viewer.overlaySegments.dropFirst(2)
        #expect(live.count == 1 && live.first?.kind == 1, "the eraser drag draws blue")
        #expect(m.viewer.drawPlan(loupeAt: nil).quads.first?.segments.count == 3)
        m.viewer.cancelCanvasDrag()
        #expect(m.viewer.overlaySegments.count == 2)
    }
}

final class LockedCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func increment() { lock.withLock { n += 1 } }
    var value: Int { lock.withLock { n } }
}

// MARK: - H6/M3: pipeline seam + live preview buffers

@Suite struct PipelineRendererTests {
    @Test func rendersIntoAMetalBufferWithoutCopies() async throws {
        let device = try #require(MTLCreateSystemDefaultDevice())
        let r = PipelineRenderer(backend: TestPatternPipelineBackend(), device: device)
        #expect(r.capabilities.regions && r.capabilities.floatFrames)
        let photo = samplePhoto()
        let f = try await r.render(photo, edit: EditState(), request: RenderRequest(maxPixelSize: 600))
        #expect(f.width == 600 && f.height == 400 && f.format == .rgba8Srgb)
        guard case .buffer(let buf) = f.backing else {
            Issue.record("expected a Metal buffer backing")
            return
        }
        #expect(buf.length >= f.bytesPerRow * f.height)
        // The viewer wraps that buffer directly (no mips at fit size, no blit).
        let viewer = try MetalViewerRenderer(device: device, pixelFormat: .rgba8Unorm_srgb)
        let tex = try #require(viewer.zeroCopyTexture(for: f))
        #expect(tex.buffer === buf)
        // EDR request → linear P3 half floats.
        let hdr = try await r.render(photo, edit: EditState(), request: RenderRequest(maxPixelSize: 300, format: .rgba16fLinearP3))
        #expect(hdr.format == .rgba16fLinearP3 && hdr.bytesPerRow >= hdr.width * 8)
        #expect(viewer.zeroCopyTexture(for: hdr)?.pixelFormat == .rgba16Float)
        // Tiles: region at scale 1.
        let tile = try await r.render(photo, edit: EditState(),
                                      request: RenderRequest(maxPixelSize: 6000, scale: 1, region: CGRect(x: 0.4, y: 0.4, width: 0.2, height: 0.2)))
        #expect(tile.width == 1200 && tile.height == 800 && tile.region != nil && tile.fullSize == CGSize(width: 6000, height: 4000))
    }

    @Test func cancellationStopsTheBackend() async throws {
        let r = PipelineRenderer(backend: TestPatternPipelineBackend())
        let t = Task { try await r.render(samplePhoto(), edit: EditState(), request: RenderRequest(maxPixelSize: 6000, scale: 1)) }
        t.cancel()
        await #expect(throws: CancellationError.self) { try await t.value }
    }

    @Test func invalidRequestsSurfaceAsRenderErrors() async {
        let r = PipelineRenderer(backend: TestPatternPipelineBackend(), device: nil)
        await #expect(throws: RenderError.self) {
            try await r.render(samplePhoto(), edit: EditState(),
                               request: RenderRequest(maxPixelSize: 100, scale: 1, region: CGRect(x: 0, y: 0, width: 0, height: 0.5)))
        }
    }

    /// A backend that fails mid-render (or sees its task cancelled while rendering).
    struct FailingBackend: PipelineBackend {
        var cancelInside = false
        var capabilities: RendererCapabilities { RendererCapabilities(regions: false, floatFrames: false, rendersEdits: true) }
        func plan(_ photo: Photo, edit: EditState, request: RenderRequest) throws -> PipelineFramePlan {
            PipelineFramePlan(width: 4, height: 2, bytesPerRow: 16, format: .rgba8Srgb, region: nil, fullSize: CGSize(width: 4, height: 2))
        }
        func render(_ photo: Photo, edit: EditState, request: RenderRequest, plan: PipelineFramePlan,
                    into dest: UnsafeMutableRawPointer, length: Int) async throws {
            #expect(length >= 32)
            dest.storeBytes(of: 0xFF, as: UInt8.self)
            if cancelInside { withUnsafeCurrentTask { $0?.cancel() }; return }
            throw MockError.invalid("backend failed")
        }
    }

    @Test func backendFailuresAndLateCancellationDiscardTheBuffer() async throws {
        let failing = PipelineRenderer(backend: FailingBackend())
        await #expect(throws: MockError.self) { try await failing.render(samplePhoto(), edit: EditState(), request: RenderRequest(maxPixelSize: 4)) }
        // Cancelled while the backend ran: the finished bytes are dropped, not shown.
        let late = PipelineRenderer(backend: FailingBackend(cancelInside: true))
        await #expect(throws: CancellationError.self) { try await late.render(samplePhoto(), edit: EditState(), request: RenderRequest(maxPixelSize: 4)) }
        #expect(late.capabilities.rendersEdits && !late.capabilities.regions)
    }

    @Test func heapFallbackWithoutMetal() async throws {
        let r = PipelineRenderer(backend: TestPatternPipelineBackend(), device: nil)
        let f = try await r.render(samplePhoto(), edit: EditState(), request: RenderRequest(maxPixelSize: 90))
        guard case .data = f.backing else {
            Issue.record("expected plain memory")
            return
        }
        #expect(f.width == 90)
    }
}

@Suite struct PreviewFramesTests {
    static func image(_ w: Int, _ h: Int) -> CGImage {
        let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        ctx.setFillColor(CGColor(srgbRed: 1, green: 0, blue: 0, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
        return ctx.makeImage()!
    }

    @Test func liveFramesAreMetalBuffersTheViewerWrapsWithoutCopying() throws {
        let device = try #require(MTLCreateSystemDefaultDevice())
        let f = try #require(PreviewFrames.convert(Self.image(1616, 1080), device: device))
        guard case .buffer(let buf) = f.backing else {
            Issue.record("expected a Metal buffer")
            return
        }
        #expect(f.bytesPerRow % device.minimumLinearTextureAlignment(for: .rgba8Unorm_srgb) == 0)
        #expect(f.bytesPerRow >= 1616 * 4)
        let tex = try #require(try MetalViewerRenderer(device: device).zeroCopyTexture(for: f))
        #expect(tex.buffer === buf && tex.width == 1616)
        // Pixels land in padded rows: last pixel of row 1.
        let px = f.withBytes { p in (0..<4).map { p[f.bytesPerRow + 1615 * 4 + $0] } }
        #expect(px == [255, 0, 0, 255])
        #expect(Histogram(frame: f).pixelCount == 1616 * 1080)
    }

    @Test func plainMemoryWithoutMetalAndBadImages() throws {
        let f = try #require(PreviewFrames.convert(Self.image(3, 2), device: nil))
        #expect(f.bytesPerRow == 12)
        guard case .data = f.backing else {
            Issue.record("expected plain memory")
            return
        }
        #expect(PreviewFrames.alignedRowBytes(width: 3, device: nil) == 12)
    }

    @Test func sharedCacheConvertsOncePerImageAndIsBounded() throws {
        let pf = PreviewFrames(byteLimit: 500_000)
        let a = Self.image(200, 200), b = Self.image(300, 300)
        let fa = try #require(pf.frame(for: a))
        #expect(pf.frame(for: a) == fa && pf.conversions == 1)
        _ = pf.frame(for: b)
        #expect(pf.conversions == 2)
        #expect(pf.cachedBytes <= 500_000, "over the cap the oldest frame goes")
        _ = pf.frame(for: a)
        #expect(pf.conversions == 3, "a was evicted by b")
        pf.removeAll()
        #expect(pf.cachedBytes == 0)
    }

    @Test func embeddedPreviewRendererUsesTheSharedFrames() async throws {
        let pf = PreviewFrames()
        let r = EmbeddedPreviewRenderer(previews: ViewerFakePreviews(), frames: pf)
        var p = samplePhoto()
        await #expect(throws: MockError.self) { try await r.render(p, edit: EditState(), maxPixelSize: 100) }
        p.url = URL(fileURLWithPath: "/x.ARW")
        let f1 = try await r.render(p, edit: EditState(), maxPixelSize: 100)
        #expect(f1.width == 60 && f1.height == 40)
        #expect(try EmbeddedPreviewRenderer.frame(from: PreviewFramesTests.image(5, 5)).width == 5)
        #expect(RendererCapabilities.of(r) == RendererCapabilities(regions: false, floatFrames: false, rendersEdits: false))
    }
}

// MARK: - Coverage of viewer helpers

@MainActor
@Suite struct LayerControllerTests {
    @Test func addDuplicateRenameDeleteTargetOnlyTheFocusedPhoto() async {
        let m = makeModel()
        await m.start()
        m.perform(.move(.right, extend: true))
        m.editAllSelected = true
        let other = m.selectedIDs.first { $0 != m.focusedID }!
        let lc = LayerController(app: m)
        let a = lc.add()
        #expect(a == 0 && lc.selected == 0 && lc.layers.count == 1)
        let b = lc.duplicateSelected()
        #expect(b == 1 && lc.selected == 1 && lc.layers.count == 2)
        lc.rename(1, to: "Sky")
        #expect(lc.layers.last?.name == "Sky")
        lc.update(1, label: "Opacity", key: "op") { $0.opacity = 0.5 }
        #expect(lc.layers.last?.opacity == 0.5)
        lc.deleteSelected()
        #expect(lc.layers.map(\.id) == [0] && lc.selected == nil)
        #expect(m.edit(for: other).layers.isEmpty, "N10: never touches other photos")
        #expect(lc.duplicateSelected() == nil)
        lc.deleteSelected() // nothing selected: no-op
        #expect(lc.layers.count == 1)
    }

    @Test func toolsAndAIMasksSelectALayer() async throws {
        let m = makeModel()
        await m.start()
        let lc = LayerController(app: m)
        lc.chooseTool(.brush)
        #expect(m.viewer.canvasTool == .brush && lc.selected == 0, "a layer was added for the tool")
        lc.chooseTool(.brush)
        #expect(m.viewer.canvasTool == .pan, "same tool toggles off")
        let masker = AIMasker { _, _ in VisionMask(width: 4, height: 4, bytes: [UInt8](repeating: 255, count: 16)) }
        try await lc.addAIMask(.subject, masker: masker)
        #expect(lc.layers.first { $0.id == lc.selected }?.mask.shapes.contains(.ai(.add, .subject)) == true)
        let label = LayerController.accessibilityLabel(LayerEdit(id: 0, name: "Sky", enabled: false, opacity: 0.4), selected: true)
        #expect(label.contains("Sky") && label.contains("hidden") && label.contains("40 percent") && label.hasSuffix("selected"))
        m.focus(nil, extend: false)
        #expect(LayerController(app: m).add() == nil && LayerController(app: m).layers.isEmpty)
    }
}

@Suite struct ViewerNotesTests {
    @Test func honestNotesPerRenderer() {
        let preview = RendererCapabilities(regions: false, floatFrames: false, rendersEdits: false)
        let pattern = RendererCapabilities(regions: true, floatFrames: true, rendersEdits: true, renderedGroups: [.exposure])
        #expect(ViewerNotes.editsNotShown([], capabilities: preview) == nil)
        #expect(ViewerNotes.editsNotShown([.crop], capabilities: preview)?.text.contains("not shown") == true)
        let one = ViewerNotes.editsNotShown([.curve], capabilities: pattern)
        #expect(one?.text == "Not shown yet: Curve")
        let many = ViewerNotes.editsNotShown([.curve, .crop, .levels, .hdr], capabilities: pattern)
        #expect(many?.text.hasSuffix("+2") == true && many?.help.contains("Crop") == true)
        #expect(ViewerNotes.stylesHover(capabilities: pattern).contains("Hover to preview"))
        #expect(ViewerNotes.stylesHover(capabilities: preview).contains("need the full develop"))
    }
}

@Suite struct CanvasAndReadoutTests {
    @Test func edgeHandlesHonourTheAspectLock() {
        let size = CGSize(width: 600, height: 400)
        var d = CanvasDrag(tool: .crop, start: CGPoint(x: 0.9, y: 0.5))
        d.cropHandle = .edge(1)
        d.initialCrop = CGRect(x: 0.2, y: 0.2, width: 0.6, height: 0.6)
        d.move(to: CGPoint(x: 0.8, y: 0.5))
        let free = CanvasInteraction.cropRect(for: d, imageSize: size, aspect: nil)
        #expect(abs(free.width - 0.5) < 1e-9 && abs(free.height - 0.6) < 1e-9)
        let locked = CanvasInteraction.cropRect(for: d, imageSize: size, aspect: (1, 1))
        #expect(abs(locked.width * 600 - locked.height * 400) < 1e-6, "square in pixels")
        for e in 0..<4 {
            var d2 = CanvasDrag(tool: .crop, start: .zero)
            d2.cropHandle = .edge(e)
            d2.initialCrop = CGRect(x: 0.2, y: 0.2, width: 0.6, height: 0.6)
            d2.move(to: CGPoint(x: 0.05, y: 0.05))
            let r = CanvasInteraction.cropRect(for: d2, imageSize: size, aspect: (3, 2))
            #expect(r.width > 0 && r.height > 0 && r.maxX <= 1 && r.maxY <= 1)
        }
        #expect(CanvasInteraction.clampRect(CGRect(x: 0.8, y: -0.1, width: 0.5, height: 2)) == CGRect(x: 0.5, y: 0, width: 0.5, height: 1))
        // Crops stay inside the crop tool's image area: moved in when they fit, else shrunk
        // about their centre with their shape kept (an aspect lock holds).
        let area = CGRect(x: 0.05, y: 0.05, width: 0.9, height: 0.9)
        let inside = CGRect(x: 0.2, y: 0.2, width: 0.3, height: 0.3)
        #expect(CanvasInteraction.confine(inside, to: area) == inside)
        let moved = CanvasInteraction.confine(CGRect(x: 0, y: 0.2, width: 0.4, height: 0.5), to: area)
        #expect(abs(moved.minX - 0.05) < 1e-9 && abs(moved.width - 0.4) < 1e-9 && abs(moved.height - 0.5) < 1e-9)
        let big = CanvasInteraction.confine(CGRect(x: 0, y: 0, width: 1, height: 0.8), to: area)
        #expect(area.insetBy(dx: -1e-9, dy: -1e-9).contains(big))
        #expect(abs(big.width / big.height - 1 / 0.8) < 1e-9 && abs(big.width - 0.9) < 1e-9)
    }

    @Test func toolLabelsAndMaskFlags() {
        for t in CanvasTool.allCases {
            #expect(!t.viewerLabel.isEmpty && !t.viewerSymbol.isEmpty)
        }
        #expect(CanvasTool.brush.editsMask && CanvasTool.radialGradient.editsMask && !CanvasTool.crop.editsMask)
        #expect(CanvasInteraction.rotate(0, by: -1) == 3 && CanvasInteraction.rotate(3, by: 1) == 0)
        // Commits for every tool.
        var d = CanvasDrag(tool: .radialGradient, start: CGPoint(x: 0.5, y: 0.5))
        d.move(to: CGPoint(x: 0.6, y: 0.5))
        if case .mask(.radial(_, _, let r, _, _)) = CanvasInteraction.commit(d, imageSize: CGSize(width: 600, height: 400), cropAspect: nil) {
            #expect(abs(r.width - 0.1) < 1e-9 && r.height == r.width, "a circle, in long-edge units")
        } else { Issue.record("radial") }
        d.tool = .linearGradient
        #expect(CanvasInteraction.commit(d, imageSize: CGSize(width: 1, height: 1), cropAspect: nil) != .none)
        d.tool = .rotate
        #expect(CanvasInteraction.commit(d, imageSize: CGSize(width: 1, height: 1), cropAspect: nil) == .none)
        var brush = CanvasDrag(tool: .eraser, start: .zero)
        brush.move(to: CGPoint(x: 0.1, y: 0))
        brush.move(to: CGPoint(x: 0.1005, y: 0)) // too close: not added
        #expect(brush.points.count == 2)
        if case .mask(.brush(let op, let strokes)) = CanvasInteraction.commit(brush, imageSize: CGSize(width: 1, height: 1), cropAspect: nil) {
            #expect(op == .subtract && strokes.first?.erase == true)
        } else { Issue.record("eraser") }
        let still = CanvasDrag(tool: .straighten, start: .zero)
        #expect(CanvasInteraction.commit(still, imageSize: CGSize(width: 1, height: 1), cropAspect: nil) == .none)
        #expect(CanvasInteraction.straightenAngle(from: .zero, to: CGPoint(x: 0.01, y: 1), imageSize: CGSize(width: 100, height: 100)) < 1)
    }

    @MainActor
    @Test func readoutTextsLabAndPinning() {
        let r = CursorReadout()
        #expect(r.rgbText == nil && r.labText == nil)
        r.update(rgb: SIMD3(1, 1, 1), at: CGPoint(x: 0.5, y: 0.5))
        #expect(r.rgbText == "R 255  G 255  B 255")
        #expect(r.lab.map { abs($0.x - 100) < 0.01 && abs($0.y) < 0.01 } == true)
        #expect(r.labText == "L 100  a 0  b 0")
        r.update(rgb: SIMD3(1, 1, 1), at: CGPoint(x: 0.5, y: 0.5)) // unchanged: no-op
        r.pin(SIMD3(0, 0, 0))
        #expect(r.pinned == SIMD3(0, 0, 0))
        r.update(rgb: nil, at: nil)
        #expect(r.rgb == nil && r.lab == nil)
    }

    @Test func floatFramesSampleAndConvertToSDR() {
        let half: [Float16] = [0.5, 0.5, 0.5, 1, 4, 0, 0, 1]
        let data = half.withUnsafeBytes { Data($0) }
        let f = RenderedFrame(width: 2, height: 1, format: .rgba16fLinearP3, bytesPerRow: 16, backing: .data(data))
        let gray = ColorMath.sample(f, at: CGPoint(x: 0.1, y: 0.5), radius: 0)!
        #expect(abs(gray.x - ColorMath.encode(0.5)) < 0.01)
        let sdr = ColorMath.sdrFrame(f)
        #expect(sdr.format == .rgba8Srgb && sdr.rgba[4] == 255, "EDR red clamps to SDR white")
        let eight = RenderedFrame(width: 1, height: 1, rgba: Data([10, 20, 30, 255]))
        #expect(ColorMath.sdrFrame(eight) == eight)
        #expect(ColorMath.sample(RenderedFrame(width: 0, height: 0, rgba: Data()), at: .zero) == nil)
        #expect(ColorMath.srgb(fromLinearP3: SIMD3(1, 1, 1)).x > 0.99)
    }

    @Test func radialOverlayPathHonoursTheAngle() {
        let flat = ViewerOverlayGeometry.radialPath(center: CGPoint(x: 50, y: 50), radius: CGSize(width: 0.4, height: 0.1),
                                                   angleDegrees: 0, imageSize: CGSize(width: 100, height: 100)).boundingRect
        let turned = ViewerOverlayGeometry.radialPath(center: CGPoint(x: 50, y: 50), radius: CGSize(width: 0.4, height: 0.1),
                                                     angleDegrees: 90, imageSize: CGSize(width: 100, height: 100)).boundingRect
        #expect(abs(flat.width - 80) < 0.5 && abs(flat.height - 20) < 0.5)
        #expect(abs(turned.width - 20) < 0.5 && abs(turned.height - 80) < 0.5)
    }

    @Test func viewTransformShapesAndBounds() {
        let t = ViewTransform(quarterTurns: 1, angle: 10)
        let a: CGFloat = 1.5
        let shapes: [MaskShape] = [
            .brush(.add, [BrushStroke(points: [CGPoint(x: 0.2, y: 0.3)])]),
            .linear(.add, start: CGPoint(x: 0.1, y: 0.1), end: CGPoint(x: 0.9, y: 0.4)),
            .radial(.add, center: CGPoint(x: 0.4, y: 0.6), radius: CGSize(width: 0.2, height: 0.1), angle: 5, feather: 0.3),
            .fill(.add), .ai(.add, .subject),
        ]
        for s in shapes {
            let back = t.toDisplay(t.toOriented(s, displayAspect: a), displayAspect: a)
            switch (s, back) {
            case (.brush(_, let x), .brush(_, let y)):
                #expect(abs(x[0].points[0].x - y[0].points[0].x) < 1e-9)
            case (.linear(_, let s1, _), .linear(_, let s2, _)):
                #expect(abs(s1.x - s2.x) < 1e-9 && abs(s1.y - s2.y) < 1e-9)
            case (.radial(_, let c1, let r1, let a1, _), .radial(_, let c2, let r2, let a2, _)):
                #expect(abs(c1.x - c2.x) < 1e-9 && r1 == r2 && abs(a1 - a2) < 1e-9)
            default:
                #expect(s == back)
            }
        }
        let b = t.orientedBounds(of: CGRect(x: 0.4, y: 0.4, width: 0.2, height: 0.2), displayAspect: a)
        #expect(b.width > 0 && b.height > 0 && b.maxX <= 1)
        #expect(ViewTransform(quarterTurns: -1).quarterTurns == 3 && ViewTransform().isIdentity)
        #expect(ViewTransform(quarterTurns: 1).displaySize(oriented: CGSize(width: 6, height: 4)) == CGSize(width: 4, height: 6))
        for k in 0..<4 {
            let p = CGPoint(x: 0.2, y: 0.7)
            let q = ViewTransform.unturn(ViewTransform.turn(p, k), k)
            #expect(abs(q.x - p.x) < 1e-12 && abs(q.y - p.y) < 1e-12)
        }
    }
}
