import CoreGraphics
import Foundation
import Metal
import Testing
@testable import DarkslideKit

/// User bug: "when I adjust the slider, the picture flashes crazy". During an edit
/// gesture the viewer must keep the last good developed frame until a newer one is
/// ready, and never present the embedded preview, an empty pane, an older edit, or a
/// different quality mid-drag; the Draft→Preview refine happens once, after the drag.
@MainActor @Suite(.serialized) struct DragFlickerTests {
    /// Mean luma (0…255) of an 8-bit frame, or of a 16F frame mapped to SDR.
    static func meanLuma(_ f: RenderedFrame) -> Double {
        f.withBytes { raw in
            var sum = 0.0, n = 0.0
            let step = max(1, (f.width * f.height) / 20_000)
            var i = 0
            while i < f.width * f.height {
                let x = i % f.width, y = i / f.width
                let o = y * f.bytesPerRow + x * f.format.bytesPerPixel
                if f.format.isEightBit {
                    sum += 0.2126 * Double(raw[o]) + 0.7152 * Double(raw[o + 1]) + 0.0722 * Double(raw[o + 2])
                } else {
                    let p = raw.baseAddress!.advanced(by: o).assumingMemoryBound(to: Float16.self)
                    sum += 255 * min(1, 0.2126 * Double(p[0]) + 0.7152 * Double(p[1]) + 0.0722 * Double(p[2]))
                }
                n += 1
                i += step
            }
            return sum / max(n, 1)
        }
    }

    /// A 60-tick exposure drag on a real sample through the app: every frame put on
    /// screen during the drag is a Draft of a newer request than the last, the viewer
    /// never falls back to the preview or to nothing, brightness only goes up (no dip
    /// back to the unedited look), and exactly one Preview refine follows the drag.
    @Test func a60TickSliderDragPresentsMonotonicDraftsThenOneRefine() async throws {
        guard let live = try await Vw8Live.make(limit: 1) else { return }
        defer { live.cleanup() }
        let m = live.app, v = m.viewer
        v.settleDelay = .milliseconds(120)
        // The lost-mouse-up fallback ends a gesture after 1 s without a tick; a loaded
        // machine can stall this loop that long, which isn't a lost mouse-up.
        v.gestureSettleFallback = .seconds(60)
        let start = try #require(await live.settled())
        let baseLuma = Self.meanLuma(start)
        var presented: [ViewerModel.Presented] = []
        v.onPresent = { presented.append($0) }
        var fellBack = 0, blank = 0
        for i in 1...60 {
            m.updateEdit(label: "Exposure", coalesceKey: "exposure") { $0.exposure.ev = Double(i) / 40 }
            // Sample what's on screen a few times between ticks (a drag delivers ~120 Hz).
            for _ in 0..<3 {
                if v.displayFrame == nil { blank += 1 } else if v.frame == nil { fellBack += 1 }
                try await Task.sleep(for: .milliseconds(3))
            }
            // A user pausing mid-drag longer than the idle delay must not get a refine.
            if i == 30 { try await Task.sleep(for: .milliseconds(400)) }
        }
        let duringDrag = presented
        m.endEditGesture()
        #expect(await waitUntil(timeout: .seconds(30)) { presented.contains { !$0.duringGesture && $0.quality == .preview } })
        try await Task.sleep(for: .milliseconds(500))
        let after = presented.dropFirst(duringDrag.count)

        #expect(!duringDrag.isEmpty, "the drag showed frames while it ran")
        #expect(blank == 0 && fellBack == 0, "never blank (\(blank)) or the embedded preview (\(fellBack)) mid-drag")
        #expect(duringDrag.allSatisfy { $0.quality == .draft && $0.duringGesture }, "\(duringDrag.map(\.quality))")
        let serials = duringDrag.map(\.serial)
        #expect(serials == serials.sorted() && Set(serials).count == serials.count, "newer each time: \(serials)")
        #expect(after.filter { $0.quality == .preview }.count == 1, "one refine: \(after.map(\.quality))")
        // Brightness follows the drag: never a dip back towards the unedited frame.
        let lumas = duringDrag.map { Self.meanLuma($0.frame) }
        let dips = zip(lumas, lumas.dropFirst()).filter { $1 < $0 - 0.75 }
        print(String(format: "FLICKER drag: %d drafts, luma %.1f → %.1f (base %.1f), refine %@",
                     lumas.count, lumas.first ?? 0, lumas.last ?? 0, baseLuma, "\(after.map(\.quality))"))
        #expect(dips.isEmpty, "luma dipped: \(dips)")
        #expect((lumas.first ?? 0) > baseLuma, "the first draft is already edited")
    }

    /// A pane whose new frame isn't uploaded yet keeps the texture it showed (same
    /// region and aspect); a different region or aspect is never stretched in; a slot
    /// not drawn forgets its texture (it may be recycled).
    @Test func heldTexturesBridgeAnUploadNeverAStretch() throws {
        guard let device = MTLCreateSystemDefaultDevice() else { return }
        func tex() -> MTLTexture {
            device.makeTexture(descriptor: .texture2DDescriptor(pixelFormat: .rgba8Unorm, width: 4, height: 2, mipmapped: false))!
        }
        func frame(_ w: Int, _ h: Int) -> RenderedFrame { RenderedFrame(width: w, height: h, rgba: Data(count: w * h * 4)) }
        var held = HeldTextures()
        let full = SIMD4<Float>(0, 0, 1, 1)
        let a = frame(400, 200), b = frame(800, 400), c = frame(400, 400)
        let ta = tex()
        #expect(held.resolve(slot: 0, frame: a, region: full, ready: ta).texture === ta)
        let bridged = held.resolve(slot: 0, frame: b, region: full, ready: nil)
        #expect(bridged.texture === ta && bridged.held == a.id, "the last frame stays while b uploads")
        #expect(held.resolve(slot: 0, frame: c, region: full, ready: nil).texture == nil, "other aspect: not stretched")
        #expect(held.resolve(slot: 0, frame: b, region: full, ready: nil).texture == nil, "forgotten once not drawn")
        let tb = tex()
        _ = held.resolve(slot: 0, frame: b, region: full, ready: tb)
        #expect(held.resolve(slot: 0, frame: a, region: SIMD4(0, 0, 0.5, 0.5), ready: nil).texture == nil, "other region")
        _ = held.resolve(slot: 1, frame: a, region: full, ready: ta)
        held.trim(to: 1)
        #expect(held.slots.keys.sorted() == [])
    }

    /// Develop frames in Metal buffers are wrapped in the same pass they arrive (no
    /// upload hop, so nothing to bridge).
    @Test func bufferFramesAreReadyImmediately() async throws {
        guard let live = try await Vw8Live.make(limit: 1) else { return }
        defer { live.cleanup() }
        let f = try #require(await live.settled())
        guard case .buffer = f.backing else {
            Issue.record("develop frames should be Metal-buffer backed")
            return
        }
        let uploader = TextureUploader(renderer: try MetalViewerRenderer(pixelFormat: .bgra8Unorm_srgb))
        #expect(uploader.texture(for: f, onReady: {}) != nil, "wrapped synchronously")
        #expect(uploader.stats.zeroCopy == 1)
    }
}
