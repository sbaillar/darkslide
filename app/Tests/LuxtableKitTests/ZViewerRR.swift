import CoreGraphics
import Foundation
import Metal
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Re-review repros (viewer slice). Print evidence; expectations state the correct behaviour.
@MainActor
@Suite(.serialized) struct ZViewerRR {
    // 1. Live mode: EmbeddedPreviewRenderer ignores `region`, so the "1:1 tile" is the whole
    //    preview stretched into the visible region, and the label claims "100%".
    @Test func liveTileIsWholePreviewSquashedIntoRegion() async {
        let m = makeViewerApp(renderer: EmbeddedPreviewRenderer(previews: ViewerFakePreviews()))
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        m.viewer.cursor = CGPoint(x: 612, y: 412)
        m.perform(.zoomToggleAtCursor)
        _ = await waitUntil(timeout: .seconds(1)) { m.viewer.tile != nil }
        if let t = m.viewer.tile {
            print("RR1 tile frame \(t.frame.width)x\(t.frame.height) region \(t.frame.region.map { "\($0)" } ?? "nil") drawn into \(t.region) scale \(t.scale) label \(m.viewer.zoomLabel)")
        }
        #expect(m.viewer.tile == nil || m.viewer.tile!.frame.region != nil, "a tile must cover the region it is drawn into")
        #expect(m.viewer.zoomLabel.contains("preview"), "only a 60x40 preview exists: label must not claim true 100%")
    }

    // 2. A canvas drag that ends after focus moved commits onto the NEW photo.
    @Test func dragEndingAfterFocusChangeHitsOtherPhoto() async {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        let a = m.focusedID!
        let img = m.viewer.layout.main!.image
        m.viewer.canvasTool = .crop
        m.viewer.beginCanvasDrag(at: CGPoint(x: img.minX + 10, y: img.minY + 10))
        m.viewer.updateCanvasDrag(to: CGPoint(x: img.midX, y: img.midY))
        let b = m.visibleIDs[1]
        m.focus(b, extend: false) // arrow key during the drag
        m.viewer.endCanvasDrag()
        print("RR2 crop A \(m.edit(for: a).crop.rect) B \(m.edit(for: b).crop.rect)")
        #expect(m.edit(for: b).crop.rect == CGRect(x: 0, y: 0, width: 1, height: 1), "drag started on A must not crop B")
    }

    // 3. selectedLayerID survives navigation: a stroke on B lands in B's unrelated layer with the same id.
    @Test func selectedLayerLeaksAcrossPhotos() async {
        let m = makeViewerApp()
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        let b = m.visibleIDs[1]
        m.updateEdits([b], label: "seed") { _, e in e.layers = [LayerEdit(id: 0, name: "B dodge")] }
        let img = m.viewer.layout.main!.image
        m.viewer.canvasTool = .brush
        m.viewer.beginCanvasDrag(at: CGPoint(x: img.midX, y: img.midY))
        m.viewer.updateCanvasDrag(to: CGPoint(x: img.midX + 50, y: img.midY))
        m.viewer.endCanvasDrag()
        #expect(m.viewer.selectedLayerID == 0)
        m.focus(b, extend: false)
        // Generous timeout: the full suite runs many renders in parallel on a loaded machine.
        #expect(await waitUntil(timeout: .seconds(15)) { m.viewer.frame != nil })
        m.viewer.beginCanvasDrag(at: CGPoint(x: img.midX, y: img.midY))
        m.viewer.updateCanvasDrag(to: CGPoint(x: img.midX + 50, y: img.midY))
        m.viewer.endCanvasDrag()
        let e = m.edit(for: b)
        print("RR3 B layers: \(e.layers.map { "\($0.id):\($0.name) shapes=\($0.mask.shapes.count)" })")
        #expect(e.layers.first { $0.name == "B dodge" }?.mask.shapes.isEmpty == true, "stroke went into B's own unrelated layer")
    }

    // 4. Texture pool keyed by exact size, never evicted: tiles of varying size accumulate.
    @Test func texturePoolGrowsWithDistinctTileSizes() throws {
        let r = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        let u = TextureUploader(renderer: r)
        let base = r.device.currentAllocatedSize
        for i in 0..<24 {
            let w = 1800 + i * 17, h = 1200 + i * 11
            let f = RenderedFrame(width: w, height: h, rgba: Data(count: w * h * 4))
            _ = u.textureNow(for: f)
            u.retain(only: [])
        }
        let grew = r.device.currentAllocatedSize - base
        print("RR4 pool after 24 distinct tile sizes: +\(grew / 1_048_576) MB GPU memory still held, ready=\(u.cachedIDs.count)")
        // The pool's own bytes: `currentAllocatedSize` is device-wide and also counts the
        // develop pipeline's buffers from tests running in parallel.
        #expect(u.pooledBytes < 64 * 1_048_576, "pool should be bounded in bytes (device grew \(grew))")
    }

    // 5. Shader compile on the main thread (Coordinator.init in makeNSView).
    @Test func shaderCompileTime() throws {
        let clock = ContinuousClock()
        var times: [Duration] = []
        for _ in 0..<3 { times.append(try clock.measure { _ = try MetalViewerRenderer(pixelFormat: .bgra8Unorm_srgb) }) }
        let edr = try MetalViewerRenderer(pixelFormat: .bgra8Unorm_srgb)
        let t2 = try clock.measure { _ = try edr.pipeline(for: .rgba16Float) }
        print("RR5 MetalViewerRenderer init (makeLibrary(source:) + SDR pipeline): \(times); first EDR pipeline in draw(): \(t2)")
    }

    // 6. Clipping warning never flags highlights on 16F (EDR) frames; 8-bit does.
    @Test func clippingOnFloatFrames() throws {
        let r = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        func frame16(_ v: Float) -> RenderedFrame {
            var halves = [Float16](repeating: Float16(v), count: 8 * 8 * 4)
            for i in stride(from: 3, to: halves.count, by: 4) { halves[i] = 1 }
            let d = halves.withUnsafeBytes { Data($0) }
            return RenderedFrame(width: 8, height: 8, format: .rgba16fLinearP3, bytesPerRow: 8 * 8, backing: .data(d))
        }
        func draw(_ f: RenderedFrame) -> [UInt8] {
            let tex = r.makeTexture(from: f)!
            var p = MetalViewerRenderer.Pane(after: tex, rect: SIMD4(-1, -1, 1, 1))
            p.clippingWarning = true
            p.colorMatrix = ViewerColor.matrix(source: f.format, edrTarget: false)
            return r.renderOffscreen([p], width: 8, height: 8, clear: MTLClearColor())!
        }
        let over = draw(frame16(4.0))
        let sdr = draw(RenderedFrame(width: 8, height: 8, rgba: Data(repeating: 255, count: 256)))
        print("RR6 16F value 4.0 with clipping on -> \(Array(over.prefix(4))); 8-bit 255 -> \(Array(sdr.prefix(4)))")
        #expect(over[0] > 200 && over[1] < 100, "4x over white in a 16F frame should warn")
    }

    // 7. Focus peaking on linear luma: an equal-contrast (perceptual) edge in the shadows never peaks.
    @Test func peakingIgnoresShadowEdges() throws {
        let r = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        func edge(_ lo: UInt8, _ hi: UInt8) -> Int {
            var px = [UInt8](repeating: 255, count: 16 * 16 * 4)
            for y in 0..<16 { for x in 0..<16 { let v = x < 8 ? lo : hi; let i = (y * 16 + x) * 4; px[i] = v; px[i + 1] = v; px[i + 2] = v } }
            let tex = r.makeTexture(from: RenderedFrame(width: 16, height: 16, rgba: Data(px)))!
            var p = MetalViewerRenderer.Pane(after: tex, rect: SIMD4(-1, -1, 1, 1))
            p.focusPeaking = true
            let out = r.renderOffscreen([p], width: 16, height: 16, clear: MTLClearColor())!
            return stride(from: 0, to: out.count, by: 4).filter { out[$0 + 1] > 240 && out[$0] < 160 && out[$0 + 2] < 170 }.count
        }
        let dark = edge(20, 60), bright = edge(160, 230)
        print("RR7 peaked pixels: shadow edge 20->60 sRGB: \(dark); highlight edge 160->230: \(bright)")
        #expect(dark > 0, "a 3x-contrast shadow edge is in focus too")
    }
}

@MainActor
@Suite(.serialized) struct ZViewerRRTiming {
    @Test func mainThreadCosts() async {
        let m = makeViewerApp(count: 2000)
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1512, height: 982), backingScale: 2)
        #expect(await waitUntil { m.viewer.frame != nil })
        let clock = ContinuousClock()
        let ids = m.visibleIDs
        var nav: [Duration] = []
        for i in 1..<60 { nav.append(clock.measure { m.focus(ids[i], extend: false) }) }
        _ = await waitUntil { m.viewer.frame != nil }
        m.viewer.loupe = true
        let hover = clock.measure { for k in 0..<500 { m.viewer.hover(at: CGPoint(x: 300 + k, y: 400)) } } / 500
        let plan = clock.measure { for k in 0..<500 { _ = m.viewer.drawPlan(loupeAt: CGPoint(x: 300 + k, y: 400)) } } / 500
        m.viewer.cursor = CGPoint(x: 700, y: 500)
        m.perform(.zoomToggleAtCursor)
        let pan = clock.measure { for _ in 0..<200 { m.viewer.pan(by: CGSize(width: 3, height: 1)) } } / 200
        let nav0 = clock.measure { _ = m.viewer.navigatorImage }
        let s = nav.sorted()
        print("RRT focus() median \(s[s.count / 2]) max \(s.last!); hover \(hover); drawPlan \(plan); pan \(pan); navigatorImage(first) \(nav0)")
    }
}
