import CoreGraphics
import DarkslideAI
import DarkslideFFI
import Foundation
import simd
import Testing
@testable import DarkslideKit

/// A renderer that develops the final geometry like ds-pipeline does, cheaply: `.final`
/// frames are the crop rect (no turns/angle here), with a `FrameMapping`; `.oriented`
/// frames are the whole image. Records requests and prefetches.
final class Vw8GeometryRenderer: CapableRenderer, GeometryPrefetching, @unchecked Sendable {
    private let lock = NSLock()
    private var _requests: [DarkslideKit.RenderRequest] = []
    private var _prefetched: [PhotoID] = []
    private var _prefetchRegions: [CGRect] = []
    var latency: Duration = .zero

    var requests: [DarkslideKit.RenderRequest] { lock.withLock { _requests } }
    var prefetched: [PhotoID] { lock.withLock { _prefetched } }
    var prefetchRegions: [CGRect] { lock.withLock { _prefetchRegions } }

    var capabilities: RendererCapabilities {
        RendererCapabilities(regions: true, floatFrames: true, rendersEdits: true, rendersGeometry: true)
    }

    static func mapping(_ photo: Photo, _ edit: EditState) -> FrameMapping {
        let r = edit.crop.rect
        return FrameMapping(affine: [r.width, 0, r.minX, 0, r.height, r.minY],
                            nativeSize: CGSize(width: CGFloat(photo.meta.pixelWidth) * r.width,
                                               height: CGFloat(photo.meta.pixelHeight) * r.height))
    }

    func render(_ photo: Photo, edit: EditState, request: DarkslideKit.RenderRequest) async throws -> RenderedFrame {
        lock.withLock { _requests.append(request) }
        if latency > .zero { try await Task.sleep(for: latency) }
        try Task.checkCancellation()
        let final = request.geometry == .final
        let m = Self.mapping(photo, edit)
        let size = final ? m.nativeSize : CGSize(width: photo.meta.pixelWidth, height: photo.meta.pixelHeight)
        var w: Int, h: Int
        if let s = request.scale, let r = request.region {
            w = max(1, Int((size.width * s * r.width).rounded()))
            h = max(1, Int((size.height * s * r.height).rounded()))
        } else {
            let k = Double(request.maxPixelSize) / Double(max(size.width, size.height))
            w = max(1, Int((size.width * k).rounded()))
            h = max(1, Int((size.height * k).rounded()))
        }
        let v = UInt8(clamping: 120 + Int(edit.exposure.ev * 40))
        // Solid color, filled with memset_pattern4 (a per-pixel loop is slow in debug builds).
        var px = Data(count: w * h * 4)
        var pattern: [UInt8] = [v, 90, 60, 255]
        px.withUnsafeMutableBytes { memset_pattern4($0.baseAddress!, &pattern, $0.count) }
        var hist: Histogram?
        if request.histogram {
            var r = [UInt32](repeating: 0, count: 256), g = r, b = r, l = r
            r[Int(v)] = UInt32(w * h)
            g[90] = UInt32(w * h)
            b[60] = UInt32(w * h)
            l[100] = UInt32(w * h)
            hist = Histogram(red: r, green: g, blue: b, luma: l, shadowClipped: 0, highlightClipped: 0, pixelCount: w * h)
        }
        let float = request.format == .rgba16fLinearP3
        return RenderedFrame(width: w, height: h, format: float ? .rgba16fLinearP3 : .rgba8Srgb, bytesPerRow: w * (float ? 8 : 4),
                             backing: .data(float ? Data(count: w * h * 8) : px),
                             region: request.region, mapping: final ? m : nil, histogram: hist)
    }

    func prefetch(_ photo: Photo, edit: EditState, request: DarkslideKit.RenderRequest) async -> FrameMapping? {
        lock.withLock {
            _prefetched.append(photo.id)
            if let r = request.region { _prefetchRegions.append(r) }
        }
        return request.geometry == .final ? Self.mapping(photo, edit) : nil
    }
}

@MainActor private func vw8App(_ r: Vw8GeometryRenderer, count: Int = 12) -> AppModel {
    let m = makeViewerApp(renderer: r, count: count)
    m.viewer.settleDelay = .milliseconds(30)
    return m
}

@MainActor
@Suite struct Vw8ViewerFlowTests {
    @Test func dragsRenderDraftsThenSettleToPreview() async {
        let r = Vw8GeometryRenderer()
        let m = vw8App(r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil && m.viewer.renderedQuality == .preview })
        #expect(r.requests.last?.geometry == .final && r.requests.last?.histogram == true)
        let n = r.requests.count
        m.viewer.settleDelay = .milliseconds(250)
        // A drag: an edit every 20 ms (each draft renders once the previous one is done).
        for i in 1...5 {
            m.updateEdit(label: "ev", coalesceKey: "ev") { $0.exposure.ev = Double(i) * 0.1 }
            try? await Task.sleep(for: .milliseconds(20))
        }
        #expect(await waitUntil { r.requests.count > n && r.requests[n...].contains { $0.quality == .draft } },
                "\(r.requests.map(\.quality)) n=\(n)")
        #expect(await waitUntil { m.viewer.renderedQuality == .preview && r.requests.last?.quality == .preview })
        // At most one render per edit plus the refine (a busy renderer coalesces further).
        #expect(r.requests.count - n <= 5 + 2, "\(r.requests.map(\.quality))")
        let f = m.viewer.frame
        #expect(f?.rgba.first == UInt8(clamping: 120 + Int(0.5 * 40)), "the settled frame shows the last edit")
    }

    @Test func developedDisplayUsesTheFramesGeometryAndTheCropToolTheOriented() async {
        let r = Vw8GeometryRenderer()
        let m = vw8App(r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        m.updateEdit(label: "Crop") { $0.crop.rect = CGRect(x: 0.25, y: 0.1, width: 0.5, height: 0.8) }
        m.endEditGesture()
        #expect(await waitUntil { m.viewer.frame?.mapping?.affine[2] == 0.25 && m.viewer.renderedQuality == .preview })
        let v = m.viewer
        #expect(v.developedGeometry && v.mapping != nil && v.transform == ViewTransform())
        #expect(v.cropRect == CGRect(x: 0, y: 0, width: 1, height: 1), "the frame is already cropped: no box")
        // Display (0,0) is the crop's top-left in crop space, and back.
        let o = v.oriented(.zero)
        #expect(abs(o.x - 0.25) < 1e-9 && abs(o.y - 0.1) < 1e-9)
        let d = v.displayed(CGPoint(x: 0.5, y: 0.5))
        #expect(abs(d.x - 0.5) < 1e-9 && abs(d.y - 0.5) < 1e-9)
        // The layout fits the final frame's native size.
        let size = try? #require(v.mapping?.nativeSize)
        #expect(size?.width == CGFloat(m.focusedPhoto!.meta.pixelWidth) * 0.5)
        // The draw plan draws the developed frame as display space, no turn/straighten.
        let plan = v.drawPlan(loupeAt: nil)
        #expect(plan.quads.first?.displaySpace == true && plan.quads.first?.quarterTurns == 0)
        #expect(plan.quads.first?.cropAffine?.count == 6)
        // The crop tool edits the oriented image: an oriented render, the emulated transform, a crop box.
        let n = r.requests.count
        v.canvasTool = .crop
        #expect(!v.developedGeometry && v.mapping == nil)
        #expect(await waitUntil { r.requests.count > n && r.requests.last?.geometry == .oriented })
        #expect(await waitUntil { v.frame != nil && v.frame?.mapping == nil })
        #expect(v.cropRect != CGRect(x: 0, y: 0, width: 1, height: 1))
        // Cropping on the oriented image: the final frame is planned for each settled
        // edit, so the box is the one the pipeline renders.
        m.updateEdit(label: "Crop") { $0.crop.rect = CGRect(x: 0.3, y: 0.1, width: 0.4, height: 0.8) }
        #expect(await waitUntil { v.currentFinalMapping?.affine[2] == 0.3 })
        #expect(abs(v.cropRect.minX - 0.3) < 1e-6 && abs(v.cropRect.width - 0.4) < 1e-6, "\(v.cropRect)")
        v.canvasTool = .pan
        #expect(await waitUntil { v.frame?.mapping != nil })
        // A selected layer shows its overlay on the oriented image too.
        m.updateEdit(label: "Layer") { $0.layers.append(LayerEdit(id: 1, name: "L")) }
        v.selectedLayerID = 1
        #expect(!v.developedGeometry)
        v.selectedLayerID = nil
        #expect(v.developedGeometry)
    }

    @Test func neighboursArePrefetchedAndThePreviewTakesTheirGeometry() async {
        let r = Vw8GeometryRenderer()
        r.latency = .milliseconds(150)
        let m = vw8App(r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        let next = m.visibleIDs[1]
        #expect(await waitUntil { r.prefetched.contains(next) })
        m.perform(.move(.right, extend: false))
        #expect(m.focusedID == next)
        // Before the develop render lands: the preview is shown through the prefetched mapping.
        #expect(await waitUntil { m.viewer.previewFrame != nil })
        #expect(m.viewer.frame == nil && m.viewer.mapping != nil, "geometry known before the render")
        let plan = m.viewer.drawPlan(loupeAt: nil)
        #expect(plan.quads.first?.displaySpace == false && plan.quads.first?.cropAffine != nil)
        #expect(await waitUntil { m.viewer.frame != nil })
    }

    @Test func framesBringTheirHistogramNoSecondPass() async {
        let r = Vw8GeometryRenderer()
        let m = vw8App(r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 800, height: 600), backingScale: 1)
        #expect(await waitUntil { m.viewer.frame != nil && m.viewer.renderedQuality == .preview })
        let f = m.viewer.frame
        #expect(await waitUntil { m.viewer.histogram == f?.histogram })
        #expect(m.viewer.histogram?.pixelCount == (f?.width ?? 0) * (f?.height ?? 0))
    }

    @Test func tilesAreFullQualityRegionsOfTheFinalFrame() async {
        let r = Vw8GeometryRenderer()
        let m = vw8App(r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame?.mapping != nil && m.viewer.renderedQuality == .preview })
        m.viewer.zoom(to: .actual)
        let t = m.viewer.tileRequest()
        #expect(t?.quality == .full && t?.geometry == .final && t?.scale == 1)
        #expect(await waitUntil { m.viewer.tile != nil })
        #expect(m.viewer.zoomLabel == "100%", "no “(preview)” with the pipeline")
        // Pan slack: the pipeline is warmed for a region around the tile (3x, in the background).
        let tileRegion = try? #require(t?.region)
        #expect(await waitUntil { r.prefetchRegions.contains { $0.contains(tileRegion ?? .zero) && $0.width > (tileRegion?.width ?? 1) } })
        let plan = m.viewer.drawPlan(loupeAt: nil)
        #expect(plan.quads.count == 2 && plan.quads[1].displaySpace)
    }

    @Test func formatFollowsTheDisplay() async {
        let r = Vw8GeometryRenderer()
        let m = vw8App(r)
        #expect(m.viewer.renderFormat == .rgba8Srgb)
        m.viewer.displayWideGamut = true
        #expect(m.viewer.renderFormat == .rgba8DisplayP3)
        m.viewer.edrHeadroom = 4
        #expect(m.viewer.renderFormat == .rgba16fLinearP3)
        let plain = makeViewerApp()
        plain.viewer.displayWideGamut = true
        #expect(plain.viewer.renderFormat == .rgba8Srgb, "only develop renderers make P3 frames")
    }

    @Test func pickersSampleTheDevelopedFrameWhereTheCursorIs() async {
        let r = Vw8GeometryRenderer()
        let m = vw8App(r)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1000, height: 700), backingScale: 2)
        m.updateEdit(label: "Crop") { $0.crop.rect = CGRect(x: 0.5, y: 0.5, width: 0.5, height: 0.5) }
        #expect(await waitUntil { m.viewer.frame?.mapping != nil && m.viewer.renderedQuality == .preview })
        // Display (0.1, 0.1) is inside the developed frame even though crop space is (0.55, 0.55).
        #expect(m.viewer.sampleFrame(at: CGPoint(x: 0.1, y: 0.1)) != nil)
        m.wbPickerActive = true
        m.viewer.pickWhiteBalance(at: CGPoint(x: 0.1, y: 0.1))
        #expect(!m.focusedEdit.whiteBalance.asShot, "neutralized from the developed pixels")
        #expect(m.viewer.backendNote == nil)
    }
}

@Suite struct Vw8LogicTests {
    @Test func frameMappingRoundTrips() throws {
        let m = FrameMapping(affine: [0, 0.5, 0.1, -0.5, 0, 0.6], nativeSize: CGSize(width: 10, height: 20))
        let c = m.toCrop(CGPoint(x: 0.3, y: 0.7))
        let back = try #require(m.toFrame(c))
        #expect(abs(back.x - 0.3) < 1e-12 && abs(back.y - 0.7) < 1e-12)
        #expect(FrameMapping(affine: [0, 0, 0, 0, 0, 0], nativeSize: .zero).toFrame(.zero) == nil)
    }

    @Test func regionsWidenToWholeTextureRowsNeverNarrow() {
        let req = DarkslideKit.RenderRequest(maxPixelSize: 6000, scale: 1, region: CGRect(x: 0.2, y: 0.2, width: 0.1001, height: 0.1))
        let s = DevelopRenderer.sized(req, native: CGSize(width: 6000, height: 4000), alignment: 4)
        #expect(s.width % 4 == 0 && s.width >= 601, "\(s)")
        #expect(abs((s.region?.width ?? 0) * 6000 - CGFloat(s.width)) < 1e-6)
        // At the right edge the region shifts left instead of leaving the frame.
        let edge = DarkslideKit.RenderRequest(maxPixelSize: 6000, scale: 1, region: CGRect(x: 0.9, y: 0, width: 0.1001, height: 0.1))
        let e = DevelopRenderer.sized(edge, native: CGSize(width: 6000, height: 4000), alignment: 4)
        #expect((e.region?.maxX ?? 2) <= 1 + 1e-9)
        // Fit renders pass the long edge through.
        let fit = DevelopRenderer.sized(DarkslideKit.RenderRequest(maxPixelSize: 1234), native: CGSize(width: 6000, height: 4000))
        #expect(fit.width == 1234 && fit.region == nil)
    }

    @Test func requestsMapOntoTheFFI() {
        let r = DarkslideKit.RenderRequest(maxPixelSize: 100, format: .rgba8DisplayP3, quality: .draft, priority: .background,
                              geometry: .final, histogram: true)
        let f = DevelopRenderer.ffiRequest(r, maxWidth: 100, maxHeight: 80, region: CGRect(x: 0.1, y: 0.2, width: 0.3, height: 0.4),
                                           histogram: true)
        #expect(f.quality == .draft && f.priority == .background && f.format == .rgba8DisplayP3 && f.geometry == .final)
        #expect(f.region?.w == 0.3 && f.histogram && f.maxHeight == 80)
        #expect(DevelopRenderer.ffiFormat(.rgba16fLinearP3) == .rgba16fLinearP3 && DevelopRenderer.ffiFormat(.rgba8Srgb) == .rgba8Srgb)
        #expect(DevelopRenderer.ffiQuality(.full) == .full && DevelopRenderer.ffiQuality(.preview) == .preview)
    }

    @Test func colorMatricesFollowTheTargetPrimaries() {
        #expect(ViewerColor.matrix(source: .rgba8Srgb, p3Target: false) == matrix_identity_float3x3)
        #expect(ViewerColor.matrix(source: .rgba8DisplayP3, p3Target: true) == matrix_identity_float3x3)
        #expect(ViewerColor.matrix(source: .rgba8DisplayP3, p3Target: false) == ViewerColor.p3ToSrgb)
        #expect(ViewerColor.matrix(source: .rgba8Srgb, p3Target: true) == ViewerColor.srgbToP3)
        #expect(ViewerColor.matrix(source: .rgba16fLinearP3, edrTarget: true) == matrix_identity_float3x3)
        #expect(FramePixelFormat.rgba8DisplayP3.bytesPerPixel == 4 && FramePixelFormat.rgba8DisplayP3.isP3)
        #expect(MetalViewerRenderer.textureFormat(for: .rgba8DisplayP3) == .rgba8Unorm_srgb)
    }

    @Test func p3FramesDrawIntoAWideSurfaceOnWideDisplays() {
        let f = RenderedFrame(width: 4, height: 4, format: .rgba8DisplayP3, bytesPerRow: 16,
                              backing: .data(Data(repeating: 128, count: 64)))
        let layout = ViewerLayout.make(state: {
            var s = ViewerState()
            s.viewSize = CGSize(width: 100, height: 100)
            return s
        }(), main: CGSize(width: 4, height: 4), pinned: nil)
        var input = ViewerDrawPlan.Input(layout: layout, viewSize: CGSize(width: 100, height: 100), backingScale: 1, main: f)
        #expect(!ViewerDrawPlan.make(input).wide, "an sRGB surface converts P3 frames")
        #expect(ViewerDrawPlan.make(input).quads.first?.colorMatrix == ViewerColor.p3ToSrgb)
        input.wideGamut = true
        let plan = ViewerDrawPlan.make(input)
        #expect(plan.wide && !plan.edr && plan.quads.first?.colorMatrix == matrix_identity_float3x3)
        #expect(f.makeCGImage()?.colorSpace?.name == CGColorSpace.displayP3)
        // Sampling a P3 frame reports sRGB-encoded values (P3 mid gray stays gray).
        let c = ColorMath.sample(f, at: CGPoint(x: 0.5, y: 0.5))
        #expect(c.map { abs($0.x - 128.0 / 255) < 0.01 && abs($0.x - $0.y) < 0.01 } == true)
    }

    @Test func labelsNotesAndBeforeFraming() {
        #expect(ViewerModel.zoomLabel(zoom: .actual, effective: 1, density: 0.3) == "100% (preview)")
        #expect(ViewerModel.zoomLabel(zoom: .actual, effective: 1, density: 0.3, developed: true) == "100%")
        // L7: an auto-fitted frame scaled back to the sensor size is upsampled at 1:1.
        #expect(ViewerModel.zoomLabel(zoom: .actual, effective: 1, density: 1, developed: true, sensorDensity: 0.97)
                == "100% (upsampled 3%)")
        #expect(ViewerModel.zoomLabel(zoom: .actual, effective: 1, density: 1, developed: true, sensorDensity: 0.787)
                == "100% (upsampled 27%)")
        #expect(ViewerModel.zoomLabel(zoom: .scale(0.5), effective: 0.5, density: 1, developed: true, sensorDensity: 0.97)
                == "50%", "no upsampling below 1:1 of the sensor")
        #expect(ViewerModel.zoomLabel(zoom: .actual, effective: 1, density: 1, developed: true, sensorDensity: 1) == "100%")
        var e = EditState()
        e.crop.rect = CGRect(x: 0.1, y: 0.1, width: 0.5, height: 0.5)
        e.crop.quarterTurns = 1
        e.exposure.ev = 1
        let b = ViewerModel.beforeEdit(for: e, developed: true)
        #expect(b.crop == e.crop && b.exposure.ev == 0, "same framing, default tones")
        #expect(ViewerModel.beforeEdit(for: e, developed: false) == EditState())
        #expect(GeometryKey(e) == GeometryKey(b) && GeometryKey(e) != GeometryKey(EditState()))
        let c = RendererCapabilities(regions: true, floatFrames: true, rendersEdits: true, rendersGeometry: true, cpuFallback: true)
        #expect(c.cpuFallback && c.rendersGeometry)
    }

    @Test func masksAreHandedToThePipelineByTarget() {
        var e = EditState()
        var layer = LayerEdit(id: 1, name: "Sky")
        layer.mask.shapes = [.ai(.add, .sky), .ai(.add, .subject), .fill(.add)]
        e.layers = [layer]
        var off = LayerEdit(id: 2, name: "Off", enabled: false)
        off.mask.shapes = [.ai(.add, .person)]
        e.layers.append(off)
        #expect(DevelopRenderer.aiTargets(in: e) == [.sky, .subject], "disabled layers don't ask for masks")
        let src = StaticMasks([.sky: VisionMask(width: 2, height: 1, bytes: [255, 0])])
        #expect(src.aiMask(target: .sky)?.data == Data([255, 0]) && src.aiMask(target: .sky)?.width == 2)
        #expect(src.aiMask(target: .subject) == nil && src.aiMask(target: .person) == nil && src.aiMask(target: .background) == nil)
    }
}
