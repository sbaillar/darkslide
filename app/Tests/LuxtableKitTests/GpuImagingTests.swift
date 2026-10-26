import CoreGraphics
import LuxtableAI
import Foundation
import ImageIO
import Metal
import Testing
import UniformTypeIdentifiers
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Test images and pixel readback for the GPU paths.
enum GpuFixtures {
    /// `w`×`h` image, deterministic pseudo-random or smooth pixels.
    static func image(_ w: Int, _ h: Int, noise: Bool = false) -> CGImage {
        var px = [UInt8](repeating: 255, count: w * h * 4)
        var seed: UInt32 = 12345
        for y in 0..<h {
            for x in 0..<w {
                let i = (y * w + x) * 4
                if noise {
                    for c in 0..<3 {
                        seed = seed &* 1_664_525 &+ 1_013_904_223
                        px[i + c] = UInt8(truncatingIfNeeded: seed >> 24)
                    }
                } else {
                    px[i] = UInt8(x * 255 / max(w - 1, 1))
                    px[i + 1] = UInt8(y * 255 / max(h - 1, 1))
                    px[i + 2] = UInt8((x + y) % 256)
                }
            }
        }
        let ctx = CGContext(data: &px, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        return ctx.makeImage()!
    }

    static func jpeg(_ img: CGImage, quality: Double = 0.9) -> Data {
        let data = NSMutableData()
        let dest = CGImageDestinationCreateWithData(data, UTType.jpeg.identifier as CFString, 1, nil)!
        CGImageDestinationAddImage(dest, img, [kCGImageDestinationLossyCompressionQuality: quality] as CFDictionary)
        CGImageDestinationFinalize(dest)
        return data as Data
    }

    /// RGBA8 sRGB pixels of `img` (drawn 1:1).
    static func pixels(_ img: CGImage) -> [UInt8] {
        var px = [UInt8](repeating: 0, count: img.width * img.height * 4)
        let ctx = CGContext(data: &px, width: img.width, height: img.height, bitsPerComponent: 8, bytesPerRow: img.width * 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        ctx.draw(img, in: CGRect(x: 0, y: 0, width: img.width, height: img.height))
        return px
    }

    static func maxDiff(_ a: [UInt8], _ b: [UInt8]) -> Int {
        zip(a, b).enumerated().reduce(0) { m, e in e.offset % 4 == 3 ? m : max(m, abs(Int(e.element.0) - Int(e.element.1))) }
    }

    static func meanDiff(_ a: [UInt8], _ b: [UInt8]) -> Double {
        Double(zip(a, b).reduce(0) { $0 + abs(Int($1.0) - Int($1.1)) }) / Double(max(a.count, 1))
    }
}

// MARK: - GPUImaging (hardware JPEG decode, GPU orientation + resize)

@Suite struct GPUImagingTests {
    @Test func decodesStraightToTheTargetSizeAndFullSize() throws {
        let jpeg = GpuFixtures.jpeg(GpuFixtures.image(640, 320))
        let g = GPUImaging()
        let small = try #require(g.decode(jpeg: jpeg, maxPixelSize: 64, exifOrientation: 1))
        #expect(max(small.width, small.height) == 64 && small.width == 2 * small.height)
        let full = try #require(g.decode(jpeg: jpeg, maxPixelSize: 0, exifOrientation: 1))
        #expect(full.width == 640 && full.height == 320)
        #expect(g.gpuOps == 0, "orientation 1 needs no GPU pass")
        let rotated = try #require(g.decode(jpeg: jpeg, maxPixelSize: 64, exifOrientation: 6))
        #expect(rotated.width == 32 && rotated.height == 64, "EXIF 6 turns portrait")
        #expect(g.gpuOps == 0 && g.cpuFallbacks == 0, "small targets: DCT-scaled decode, CPU orientation (L10)")
        #expect(g.decode(jpeg: Data([1, 2, 3]), maxPixelSize: 64, exifOrientation: 1) == nil)
        #expect(g.decode(jpeg: Data([0xFF, 0xD8, 0xFF, 0xE0]), maxPixelSize: 0, exifOrientation: 1) == nil)
    }

    /// R4-L10: a target above half the source is decoded at full size and oriented +
    /// resized in one GPU pass; the result matches the CPU path, which also serves as
    /// the fallback when the GPU pass fails.
    @Test func largeTargetsOrientAndResizeInOneGPUPass() throws {
        let jpeg = GpuFixtures.jpeg(GpuFixtures.image(640, 320))
        let g = GPUImaging()
        try #require(g.hasGPU)
        let gpu = try #require(g.decode(jpeg: jpeg, maxPixelSize: 400, exifOrientation: 6))
        #expect(gpu.width == 200 && gpu.height == 400 && g.gpuOps == 1 && g.cpuFallbacks == 0)
        let fullTurned = try #require(g.decode(jpeg: jpeg, maxPixelSize: 0, exifOrientation: 8))
        #expect(fullTurned.width == 320 && fullTurned.height == 640 && g.gpuOps == 2)
        #expect(try #require(g.decode(jpeg: jpeg, maxPixelSize: 400, exifOrientation: 1)).width == 400 && g.gpuOps == 3)
        #expect(try #require(g.decode(jpeg: jpeg, maxPixelSize: 5000, exifOrientation: 1)).width == 640, "never upscales")
        let cpuPath = GPUImaging()
        cpuPath.failGPU = true
        let cpu = try #require(cpuPath.decode(jpeg: jpeg, maxPixelSize: 400, exifOrientation: 6))
        #expect(cpu.width == 200 && cpu.height == 400 && cpuPath.cpuFallbacks == 1 && cpuPath.gpuOps == 0)
        #expect(GpuFixtures.meanDiff(GpuFixtures.pixels(gpu), GpuFixtures.pixels(cpu)) < 4)
        let cpuFull = try #require(cpuPath.decode(jpeg: jpeg, maxPixelSize: 0, exifOrientation: 8))
        #expect(cpuFull.width == 320 && cpuFull.height == 640 && cpuPath.cpuFallbacks == 2)
    }

    @Test func gpuOrientationMatchesTheCPUReferenceForAllEXIFValues() throws {
        let img = GpuFixtures.image(6, 4, noise: true)
        let g = GPUImaging()
        try #require(g.hasGPU)
        for exif in UInt16(2)...8 {
            let gpu = try #require(g.orient(img, exif: exif))
            let cpu = try #require(ImageOrientation.apply(exif: exif, to: img))
            #expect(gpu.width == cpu.width && gpu.height == cpu.height, "exif \(exif)")
            #expect(GpuFixtures.maxDiff(GpuFixtures.pixels(gpu), GpuFixtures.pixels(cpu)) <= 2, "exif \(exif)")
        }
        #expect(g.gpuOps == 7 && g.cpuFallbacks == 0)
        #expect(g.orient(img, exif: 1) === img && g.orient(img, exif: 42) === img, "identity and unknown pass through")
    }

    @Test func gpuDownscaleIsCloseToTheCPUResample() throws {
        let img = GpuFixtures.image(800, 400)
        let g = GPUImaging()
        let gpu = try #require(g.downscale(img, longEdge: 200))
        #expect(gpu.width == 200 && gpu.height == 100)
        let cpu = try #require(GPUImaging.cpuDownscale(img, width: 200, height: 100))
        #expect(GpuFixtures.meanDiff(GpuFixtures.pixels(gpu), GpuFixtures.pixels(cpu)) < 3)
        #expect(g.gpuOps == 1)
        #expect(g.downscale(img, longEdge: 800) === img && g.downscale(img, longEdge: 0) === img, "never upscales")
    }

    @Test func injectedGPUFailureFallsBackToTheCPU() throws {
        let g = GPUImaging()
        g.failGPU = true
        let img = GpuFixtures.image(40, 20)
        let o = try #require(g.orient(img, exif: 8))
        #expect(o.width == 20 && o.height == 40)
        let d = try #require(g.downscale(img, longEdge: 10))
        #expect(d.width == 10 && d.height == 5)
        #expect(g.cpuFallbacks == 2 && g.gpuOps == 0)
    }

    @Test func noDeviceMeansCPUOnly() throws {
        let g = GPUImaging(device: nil)
        #expect(!g.hasGPU)
        let d = try #require(g.downscale(GpuFixtures.image(40, 20), longEdge: 20))
        #expect(d.width == 20 && g.cpuFallbacks == 1)
    }
}

// MARK: - HistogramEngine (Metal compute histogram)

@Suite struct HistogramEngineTests {
    static func rgba8(_ w: Int, _ h: Int, rowBytes: Int, seed: UInt32 = 7) -> [UInt8] {
        var px = [UInt8](repeating: 0, count: rowBytes * h)
        var s = seed
        for y in 0..<h {
            for x in 0..<w {
                let i = y * rowBytes + x * 4
                for c in 0..<3 {
                    s = s &* 1_664_525 &+ 1_013_904_223
                    px[i + c] = UInt8(truncatingIfNeeded: s >> 24)
                }
                px[i + 3] = 255
            }
        }
        px[0] = 255; px[1] = 255; px[2] = 255 // one clipped highlight
        px[4] = 0; px[5] = 0; px[6] = 0 // one clipped shadow
        return px
    }

    @Test func gpuMatchesTheCPUReferenceExactlyForDataFrames() throws {
        let gpu = HistogramEngine(), cpu = HistogramEngine(device: nil)
        try #require(gpu.usesGPU)
        #expect(!cpu.usesGPU)
        let f = RenderedFrame(width: 97, height: 61, rgba: Data(Self.rgba8(97, 61, rowBytes: 97 * 4)))
        let a = gpu.histogram(for: f), b = cpu.histogram(for: f)
        #expect(a == b)
        #expect(a.pixelCount == 97 * 61 && a.highlightClipped >= 1 && a.shadowClipped >= 1)
        #expect(gpu.gpuRuns == 1 && gpu.cpuRuns == 0 && cpu.cpuRuns == 1)
    }

    @Test func gpuReadsPaddedMetalBufferFramesInPlaceOrByUpload() throws {
        let device = try #require(MTLCreateSystemDefaultDevice())
        let gpu = HistogramEngine(device: device), cpu = HistogramEngine(device: nil)
        let align = device.minimumLinearTextureAlignment(for: .rgba8Unorm)
        // Aligned rows (zero-copy texture over the buffer) and odd rows (one upload).
        for rowBytes in [((50 * 4 + align - 1) / align + 1) * align, 50 * 4 + 4] {
            let px = Self.rgba8(50, 33, rowBytes: rowBytes, seed: UInt32(rowBytes))
            let buf = try #require(device.makeBuffer(bytes: px, length: px.count, options: .storageModeShared))
            let f = RenderedFrame(width: 50, height: 33, format: .rgba8Srgb, bytesPerRow: rowBytes, backing: .buffer(buf))
            let tex = try #require(gpu.texture(for: f, device: device))
            #expect((tex.buffer != nil) == (rowBytes % align == 0), "rows \(rowBytes)")
            #expect(gpu.histogram(for: f) == cpu.histogram(for: f), "rows \(rowBytes)")
        }
        #expect(gpu.gpuRuns == 2)
    }

    @Test func gpuConvertsLinearP3HalfFloatFramesLikeTheCPU() throws {
        let w = 64, h = 40
        var v = [Float16](repeating: 1, count: w * h * 4)
        var s: UInt32 = 99
        for i in 0..<(w * h) {
            for c in 0..<3 {
                s = s &* 1_664_525 &+ 1_013_904_223
                v[i * 4 + c] = Float16(Float(s >> 8) / Float(1 << 24) * 1.2)
            }
        }
        let data = v.withUnsafeBytes { Data($0) }
        let f = RenderedFrame(width: w, height: h, format: .rgba16fLinearP3, bytesPerRow: w * 8, backing: .data(data))
        let a = HistogramEngine().histogram(for: f), b = HistogramEngine(device: nil).histogram(for: f)
        #expect(a.pixelCount == b.pixelCount)
        // Rounding at bin edges may move a pixel by one bin; the shape must agree.
        for (x, y) in [(a.red, b.red), (a.green, b.green), (a.blue, b.blue), (a.luma, b.luma)] {
            #expect(x.reduce(0, +) == y.reduce(0, +))
            let l1 = zip(x, y).reduce(0) { $0 + abs(Int($1.0) - Int($1.1)) }
            #expect(l1 <= a.pixelCount / 50, "L1 \(l1)")
        }
        #expect(abs(a.highlightClipped - b.highlightClipped) <= a.pixelCount / 100)
        #expect(abs(a.shadowClipped - b.shadowClipped) <= a.pixelCount / 100)
    }

    @Test func injectedGPUFailureFallsBackToCPUBinning() {
        let e = HistogramEngine()
        e.failGPU = true
        #expect(!e.usesGPU)
        let f = RenderedFrame(width: 9, height: 9, rgba: Data(Self.rgba8(9, 9, rowBytes: 36)))
        #expect(e.histogram(for: f) == Histogram(frame: f))
        #expect(e.cpuRuns == 1 && e.gpuRuns == 0)
        let empty = HistogramEngine().histogram(for: RenderedFrame(width: 0, height: 0, rgba: Data()))
        #expect(empty.pixelCount == 0, "an empty frame takes the CPU path")
    }
}

// MARK: - SharedPreviewProvider over encoded previews

final class FakeEncodedSource: EncodedPreviewSource, @unchecked Sendable {
    let jpeg: Data
    let orientation: UInt16
    var fail = false
    private let lock = NSLock()
    private var _calls = 0
    var calls: Int { lock.withLock { _calls } }

    init(jpeg: Data, orientation: UInt16 = 1) {
        self.jpeg = jpeg
        self.orientation = orientation
    }

    func encodedPreview(for url: URL) async throws -> EncodedPreview {
        lock.withLock { _calls += 1 }
        if fail { throw RenderError(.Unsupported(message: "injected")) }
        return EncodedPreview(jpeg: jpeg, orientation: orientation)
    }

    func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
        GPUImaging.shared.decode(jpeg: jpeg, maxPixelSize: maxPixelSize, exifOrientation: orientation)!
    }
}

struct FakeImageProvider: PreviewProvider {
    let image: CGImage
    func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage { image }
}

@Suite struct SharedEncodedPreviewTests {
    let url = URL(fileURLWithPath: "/tmp/a.ARW")

    @Test func extractsOnceAndDecodesEachSizeOnTheHardwarePath() async throws {
        let src = FakeEncodedSource(jpeg: GpuFixtures.jpeg(GpuFixtures.image(600, 400)), orientation: 6)
        let p = SharedPreviewProvider(base: src, canonicalEdge: 300)
        let t = try await p.preview(for: url, maxPixelSize: 64)
        #expect(t.height == 64 && t.width < t.height, "decoded at size, then oriented")
        let t2 = try await p.preview(for: url, maxPixelSize: 64)
        #expect(t2 === t, "cache hit")
        let canonical = try await p.preview(for: url, maxPixelSize: 0)
        #expect(max(canonical.width, canonical.height) == 300)
        let big = try await p.preview(for: url, maxPixelSize: 5000)
        #expect(big === canonical, "≥ canonical shares the canonical decode")
        #expect(src.calls == 1 && p.decodes == 1, "one extraction for every size")
        #expect(p.cachedBytes > 0)
    }

    @Test func extractionErrorsPropagateAndRetry() async throws {
        let src = FakeEncodedSource(jpeg: GpuFixtures.jpeg(GpuFixtures.image(64, 64)))
        src.fail = true
        let p = SharedPreviewProvider(base: src)
        await #expect(throws: RenderError.self) { try await p.preview(for: url, maxPixelSize: 32) }
        src.fail = false
        _ = try await p.preview(for: url, maxPixelSize: 32)
        #expect(src.calls == 2, "a failure is not cached")
        let bad = SharedPreviewProvider(base: FakeEncodedSource(jpeg: Data([9, 9, 9])))
        await #expect(throws: RenderError.self) { try await bad.preview(for: url, maxPixelSize: 32) }
    }

    @Test func compressedPreviewsCountTowardTheCapAndAreEvicted() async throws {
        let jpeg = GpuFixtures.jpeg(GpuFixtures.image(1024, 1024, noise: true), quality: 1)
        try #require(jpeg.count > 600_000)
        let src = FakeEncodedSource(jpeg: jpeg)
        let p = SharedPreviewProvider(base: src, canonicalEdge: 128, byteLimit: 1 << 20)
        let b = URL(fileURLWithPath: "/tmp/b.ARW")
        _ = try await p.preview(for: url, maxPixelSize: 32)
        _ = try await p.preview(for: b, maxPixelSize: 32)
        #expect(src.calls == 2)
        _ = try await p.preview(for: url, maxPixelSize: 64)
        #expect(src.calls == 3, "the first JPEG was evicted to stay under the cap")
    }

    @Test func rustDecoderAndProviderOnARealSample() async throws {
        guard let file = sampleARWs().first else { return }
        let direct = try await RustPreviewDecoder().preview(for: file, maxPixelSize: 128)
        #expect(max(direct.width, direct.height) == 128)
        await #expect(throws: (any Error).self) {
            try await RustPreviewDecoder().preview(for: URL(fileURLWithPath: "/nonexistent.ARW"), maxPixelSize: 128)
        }
        let p = RustPreviewProvider()
        _ = try await p.preview(for: file, maxPixelSize: 64)
        _ = try await p.preview(for: file, maxPixelSize: 256)
        #expect(p.decodes == 1 && p.cachedBytes > 0, "one extraction serves both sizes")
    }

    @Test func imageBasesDownscaleOnTheGPU() async throws {
        let g = GPUImaging()
        let p = SharedPreviewProvider(base: FakeImageProvider(image: GpuFixtures.image(400, 200)), imaging: g)
        let s = try await p.preview(for: url, maxPixelSize: 100)
        #expect(s.width == 100 && s.height == 50)
        #expect(g.gpuOps == 1 && g.cpuFallbacks == 0)
    }
}

// MARK: - Mask / brush overlays in the viewer shader

@MainActor
@Suite struct ShaderOverlayTests {
    static let black = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)

    func render(_ m: MetalViewerRenderer, _ configure: (inout MetalViewerRenderer.Pane) -> Void) throws -> [UInt8] {
        let tex = try #require(m.makeTexture(from: solidFrame([128, 128, 128])))
        var pane = MetalViewerRenderer.Pane(after: tex, rect: SIMD4(-1, -1, 1, 1))
        configure(&pane)
        return try #require(m.renderOffscreen([pane], width: 40, height: 40, clear: Self.black))
    }

    func px(_ out: [UInt8], _ x: Int, _ y: Int) -> [Int] { out[(y * 40 + x) * 4..<(y * 40 + x) * 4 + 3].map(Int.init) }

    @Test func masksAreTintedInTheShader() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        let cache = MaskTextureCache(device: m.device)
        let key = MaskOverlayCache.Key(photo: PhotoID(1), target: .subject)
        var half = [UInt8](repeating: 0, count: 16)
        for y in 0..<4 { half[y * 4 + 2] = 255; half[y * 4 + 3] = 255 } // right half masked
        let mask = try #require(cache.texture(for: key, mask: VisionMask(width: 4, height: 4, bytes: half)))
        let plain = try render(m) { _ in }
        let out = try render(m) { $0.masks = [mask] }
        #expect(px(out, 2, 20) == px(plain, 2, 20), "unmasked side untouched")
        let r = px(out, 37, 20)
        #expect(r[0] > r[1] + 30 && r[0] > r[2] + 30, "masked side tinted red: \(r)")
    }

    @Test func brushSegmentsPaintAndEraseInTheShader() throws {
        let m = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        let paint = OverlaySegment.segments([CGPoint(x: 0.1, y: 0.5), CGPoint(x: 0.9, y: 0.5)], radius: 0.1, erase: false)
        let out = try render(m) { $0.segments = paint }
        let c = px(out, 20, 20)
        #expect(c[0] > c[2] + 30, "on the stroke: red \(c)")
        #expect(px(out, 20, 2) == [128, 128, 128], "off the stroke: untouched")
        let erase = OverlaySegment.segments([CGPoint(x: 0.5, y: 0.5)], radius: 0.1, erase: true)
        let e = px(try render(m) { $0.segments = erase }, 20, 20)
        #expect(e[2] > e[0] + 30, "eraser dab: blue \(e)")
    }

    @Test func segmentGeometryAndDecimation() {
        #expect(MemoryLayout<OverlaySegment>.stride == 32, "matches the shader's Segment")
        #expect(OverlaySegment.segments([], radius: 1, erase: false).isEmpty)
        let dot = OverlaySegment.segments([CGPoint(x: 0.2, y: 0.3)], radius: 0.05, erase: true)
        #expect(dot == [OverlaySegment(a: SIMD2(0.2, 0.3), b: SIMD2(0.2, 0.3), radius: 0.05, kind: 1)])
        let line = OverlaySegment.segments((0...1000).map { CGPoint(x: Double($0) / 1000, y: 0.5) }, radius: 0.01, erase: false)
        #expect(line.count == 1000 && line.allSatisfy { $0.kind == 0 })
        let d = OverlaySegment.decimate(line, to: MetalViewerRenderer.maxSegments)
        #expect(d.count == MetalViewerRenderer.maxSegments)
        #expect(d.first?.ab.x == 0 && d.last?.ab.z == 1, "the decimated stroke keeps its ends")
        #expect(zip(d, d.dropFirst()).allSatisfy { $0.ab.z == $1.ab.x }, "and stays connected")
        #expect(OverlaySegment.decimate(dot, to: 4) == dot)
    }

    @Test func maskTexturesUploadOnceReuseAndEvict() throws {
        let device = try #require(MTLCreateSystemDefaultDevice())
        let cache = MaskTextureCache(device: device, byteLimit: 100)
        let k1 = MaskOverlayCache.Key(photo: PhotoID(1), target: .subject)
        let k2 = MaskOverlayCache.Key(photo: PhotoID(2), target: .sky)
        let m1 = VisionMask(width: 8, height: 8, bytes: [UInt8](repeating: 200, count: 64))
        let t1 = try #require(cache.texture(for: k1, mask: m1))
        #expect(t1.pixelFormat == .r8Unorm && t1.width == 8)
        #expect(cache.texture(for: k1, mask: m1) === t1 && cache.uploads == 1, "same mask: reused")
        let regenerated = VisionMask(width: 8, height: 8, bytes: [UInt8](repeating: 10, count: 64))
        #expect(cache.texture(for: k1, mask: regenerated) !== t1 && cache.uploads == 2, "new pixels: re-uploaded")
        _ = cache.texture(for: k2, mask: m1)
        #expect(cache.cachedBytes == 64, "over the cap: the older key is evicted")
        #expect(cache.texture(for: k2, mask: VisionMask(width: 0, height: 0, bytes: [])) == nil)
        #expect(cache.texture(for: k2, mask: VisionMask(width: 4, height: 4, bytes: [1])) == nil, "short mask")
    }
}
