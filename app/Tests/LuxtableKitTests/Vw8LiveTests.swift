import CoreGraphics
import LuxtableAI
import LuxtableFFI
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Mean R, G, B (0...255) of an 8-bit frame.
func vw8Mean(_ f: RenderedFrame) -> SIMD3<Double> {
    var s = SIMD3<Double>(0, 0, 0)
    f.withBytes { p in
        for y in stride(from: 0, to: f.height, by: 2) {
            for x in stride(from: 0, to: f.width, by: 2) {
                let i = y * f.bytesPerRow + x * 4
                s += SIMD3(Double(p[i]), Double(p[i + 1]), Double(p[i + 2]))
            }
        }
    }
    return s / Double(((f.height + 1) / 2) * ((f.width + 1) / 2))
}

/// Live viewer over real samples: lt-session store, the lt-pipeline `DevelopRenderer`.
@MainActor
struct Vw8Live {
    let app: AppModel
    let dir: URL
    let base: URL
    let develop: DevelopRenderer

    static func make(dir existing: URL? = nil, base existingBase: URL? = nil, limit: Int = 2) async throws -> Vw8Live? {
        guard let dir = try existing ?? sampleFolderCopy(limit: limit) else { return nil }
        let base = existingBase ?? FileManager.default.temporaryDirectory.appendingPathComponent("vw8-store-\(UUID())")
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()), storeBase: base)
        let dev = DevelopRenderer()
        let settings = makeSettings()
        settings.onboardingComplete = true
        settings.autoAdvance = false
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: dev,
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""), ai: MockAIService(),
                         settings: settings, previews: RustPreviewProvider(), mode: .live)
        m.watchCards = false
        m.viewer.tileDelay = .zero
        m.viewer.settleDelay = .milliseconds(20)
        await m.openFolder(dir)
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        return Vw8Live(app: m, dir: dir, base: base, develop: dev)
    }

    func cleanup() {
        app.stop()
        try? FileManager.default.removeItem(at: dir)
        try? FileManager.default.removeItem(at: base)
    }

    /// Wait for a settled develop frame (Preview quality) that differs from `after`.
    func settled(after old: RenderedFrame? = nil, timeout: Duration = .seconds(60)) async -> RenderedFrame? {
        let v = app.viewer
        guard await waitUntil(timeout: timeout, {
            v.frame != nil && v.frame != old && v.renderedQuality == .preview && v.frame?.mapping != nil && v.isRenderIdle
        })
        else { return nil }
        return v.frame
    }

    func edit(_ label: String, _ change: (inout EditState) -> Void) async -> RenderedFrame? {
        let old = app.viewer.frame
        app.updateEdit(label: label, change)
        app.endEditGesture()
        return await settled(after: old)
    }
}

@MainActor
@Suite(.serialized) struct Vw8LiveTests {
    @Test func warmerWhiteBalanceRaisesRedAndLowersBlue() async throws {
        guard let live = try await Vw8Live.make() else { return }
        defer { live.cleanup() }
        let first = try #require(await live.settled())
        #expect(first.histogram != nil && first.histogram?.pixelCount == first.width * first.height,
                "the frame brings its histogram")
        #expect(live.app.viewer.histogram == first.histogram)
        let cool = try #require(await live.edit("WB") {
            $0.whiteBalance.asShot = false
            $0.whiteBalance.temperature = 4500
        })
        let warm = try #require(await live.edit("WB") { $0.whiteBalance.temperature = 8000 })
        let (a, b) = (vw8Mean(cool), vw8Mean(warm))
        #expect(b.x > a.x + 3 && b.z < a.z - 3, "warmer: R up, B down: \(a) → \(b)")
        // M2, counted honestly: no Swift-side copy; one Rust readback copy per GPU frame
        // (wgpu can't render into a foreign buffer).
        let st0 = live.develop.stats
        #expect(st0.frames > 0 && st0.swiftCopies == 0, "\(st0)")
        #expect(st0.rustCopies == (DevelopRenderer.backend.gpu ? st0.frames : 0) && st0.copies == st0.rustCopies)
        // EDR screen: 16F linear P3 frames; wide-gamut SDR: 8-bit P3 frames.
        let v = live.app.viewer
        let sdr = v.frame
        v.edrHeadroom = 4
        let edr = try #require(await live.settled(after: sdr))
        #expect(edr.format == .rgba16fLinearP3 && edr.histogram != nil && edr.bytesPerRow == edr.width * 8)
        v.edrHeadroom = 1
        v.displayWideGamut = true
        let p3 = try #require(await live.settled(after: edr))
        #expect(p3.format == .rgba8DisplayP3)
        #expect(live.develop.stats.swiftCopies == 0)
    }

    @Test func oneToOneIsThePipelinesFullRegion() async throws {
        guard let live = try await Vw8Live.make() else { return }
        defer { live.cleanup() }
        _ = try #require(await live.settled())
        let v = live.app.viewer
        v.cursor = CGPoint(x: 612, y: 412)
        v.zoom(to: .actual, anchor: v.cursor)
        #expect(await waitUntil(timeout: .seconds(60)) { v.tile != nil })
        let tile = try #require(v.tile)
        #expect(tile.scale == 1 && tile.frame.mapping != nil)
        // L7: the default lens correction auto-fits the frame, so 1:1 is slightly upsampled.
        #expect(v.zoomLabel == ((v.sensorDensity ?? 1) < 0.99 ? "100% (upsampled \(Int(((1 / v.sensorDensity! - 1) * 100).rounded()))%)" : "100%"))
        #expect(v.zoomLabel.hasPrefix("100%") && !v.zoomLabel.contains("preview"))
        // Its size is the region at one output pixel per sensor pixel.
        let native = try #require(v.mapping?.nativeSize)
        #expect(abs(Double(tile.frame.width) - Double(tile.region.width * native.width)) <= 2,
                "\(tile.frame.width) vs \(tile.region.width * native.width)")
        // Independent reference: a Full-quality region render straight from the FFI.
        let photo = try #require(live.app.focusedPhoto)
        let json = try AdjustmentsCodec.encode(live.app.focusedEdit)
        let r = tile.region
        let req = DevelopRequest(maxWidth: UInt32(tile.frame.width), maxHeight: UInt32(tile.frame.height),
                                 region: NormRect(x: Float(r.minX), y: Float(r.minY), w: Float(r.width), h: Float(r.height)),
                                 quality: .full, priority: .interactive, format: .rgba8Srgb, histogram: false, geometry: .final)
        let count = tile.frame.width * tile.frame.height * 4
        let mem = UnsafeMutableRawPointer.allocate(byteCount: count, alignment: 16)
        defer { mem.deallocate() }
        let out = try await renderDevelop(path: photo.url!.path, adjustmentsJson: json, request: req,
                                          destPtr: UInt64(UInt(bitPattern: mem)), destLen: UInt64(count), masks: nil,
                                          token: CancelToken())
        let buf = [UInt8](UnsafeRawBufferPointer(start: mem, count: count))
        #expect(Int(out.width) == tile.frame.width && Int(out.height) == tile.frame.height)
        var diff = 0.0
        tile.frame.withBytes { t in
            for i in stride(from: 0, to: buf.count, by: 4) { diff += abs(Double(t[i]) - Double(buf[i])) }
        }
        #expect(diff / Double(buf.count / 4) < 0.5, "the viewer's 1:1 pixels are the pipeline's: \(diff)")
    }

    @Test func editsPersistAndRenderTheSameAfterARelaunch() async throws {
        guard let live = try await Vw8Live.make() else { return }
        defer { try? FileManager.default.removeItem(at: live.dir); try? FileManager.default.removeItem(at: live.base) }
        _ = try #require(await live.settled())
        let edited = try #require(await live.edit("EV") { $0.exposure.ev = 1 })
        await live.app.saver.flush()
        let id = try #require(live.app.focusedPhoto?.filename)
        live.app.stop()
        // Relaunch: a new store and app over the same folder and session store.
        let again = try #require(try await Vw8Live.make(dir: live.dir, base: live.base))
        #expect(again.app.focusedPhoto?.filename == id)
        #expect(again.app.focusedEdit.exposure.ev == 1)
        let f = try #require(await again.settled())
        #expect(f.width == edited.width && f.height == edited.height)
        let (a, b) = (vw8Mean(edited), vw8Mean(f))
        #expect(abs(a.x - b.x) < 0.5 && abs(a.y - b.y) < 0.5 && abs(a.z - b.z) < 0.5, "\(a) vs \(b)")
        again.app.stop()
    }

    @Test func theFinalFrameIsCroppedAndTurnedByThePipeline() async throws {
        guard let live = try await Vw8Live.make() else { return }
        defer { live.cleanup() }
        let full = try #require(await live.settled())
        let f = try #require(await live.edit("Crop") {
            $0.crop.rect = CGRect(x: 0.1, y: 0.2, width: 0.5, height: 0.4)
            $0.crop.quarterTurns = 1
        })
        let m = try #require(f.mapping)
        // One clockwise turn: the output's top-left corner is the crop's bottom-left.
        let tl = m.toCrop(.zero)
        #expect(abs(tl.x - 0.1) < 0.01 && abs(tl.y - 0.6) < 0.01, "\(tl)")
        // Width/height swapped by the turn: the frame is the crop at the photo's aspect.
        let photo = try #require(live.app.focusedPhoto)
        let cropAspect = (0.5 * Double(photo.meta.pixelWidth)) / (0.4 * Double(photo.meta.pixelHeight))
        #expect(abs(Double(f.height) / Double(f.width) - cropAspect) < 0.03, "\(f.width)x\(f.height) vs \(cropAspect)")
        #expect(f.width != full.width || f.height != full.height)
        // The crop tool works on the oriented frame and shows the crop box.
        live.app.viewer.canvasTool = .crop
        #expect(await waitUntil(timeout: .seconds(30)) { live.app.viewer.frame != nil && live.app.viewer.frame?.mapping == nil })
        #expect(live.app.viewer.cropRect != CGRect(x: 0, y: 0, width: 1, height: 1))
    }

    /// The crop tool shows the image area: the full frame as the pipeline auto-fits it
    /// (lens correction on: inset from the oriented frame, whose curved edges it hides),
    /// for a user crop too, and not outside the crop tool.
    @Test func theCropToolShowsTheAutoFittedImageArea() async throws {
        guard let live = try await Vw8Live.make() else { return }
        defer { live.cleanup() }
        _ = try #require(await live.settled())
        let v = live.app.viewer
        #expect(v.cropImageArea == nil)
        v.canvasTool = .crop
        #expect(await waitUntil(timeout: .seconds(30)) { v.cropImageArea != nil })
        let area = try #require(v.cropImageArea)
        let full = CGRect(x: 0, y: 0, width: 1, height: 1)
        #expect(full.contains(area) && area.width < 0.995 && area.width > 0.8, "\(area)")
        #expect(abs(area.midX - 0.5) < 0.01 && abs(area.midY - 0.5) < 0.01)
        // A full crop's box is the image area itself.
        #expect(await waitUntil(timeout: .seconds(30)) { abs(v.cropRect.width - area.width) < 0.002 })
        // A user crop keeps the same image area.
        _ = await live.edit("Crop") { $0.crop.rect = CGRect(x: 0.2, y: 0.2, width: 0.5, height: 0.5) }
        v.canvasTool = .crop
        #expect(v.cropImageArea.map { abs($0.width - area.width) < 0.002 } == true)
        v.canvasTool = .pan
        #expect(v.cropImageArea == nil)
    }

    @Test func aiMasksReachThePipeline() async throws {
        guard let live = try await Vw8Live.make() else { return }
        defer { live.cleanup() }
        _ = try #require(await live.settled())
        let asked = Box<Set<AITarget>>([])
        live.develop.maskProvider = { _, targets in
            asked.value = targets
            return Dictionary(uniqueKeysWithValues: targets.map { ($0, VisionMask(width: 8, height: 8, bytes: [UInt8](repeating: 255, count: 64))) })
        }
        let before = try #require(await live.edit("Layer") {
            var l = LayerEdit(id: 1, name: "Subject", exposureEV: 0)
            l.mask.shapes = [.ai(.add, .subject)]
            $0.layers = [l]
        })
        let after = try #require(await live.edit("Layer EV") { $0.layers[0].exposureEV = 1.5 })
        #expect(asked.value == [.subject])
        #expect(vw8Mean(after).y > vw8Mean(before).y + 5, "the masked layer brightens")
        // The app-wired provider regenerates masks with Vision (here: the cache is used when present).
        let cached = await live.app.viewer.masksForRender(photo: live.app.focusedID!, targets: [])
        #expect(cached.isEmpty)
    }
}

final class Box<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var _value: T
    init(_ v: T) { _value = v }
    var value: T {
        get { lock.withLock { _value } }
        set { lock.withLock { _value = newValue } }
    }
}
