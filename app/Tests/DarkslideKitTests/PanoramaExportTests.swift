import CoreGraphics
import CoreImage
import Foundation
import ImageIO
import Metal
import Testing
import UniformTypeIdentifiers
@testable import DarkslideKit

/// Review H4 (critical): Metal aborts the process when asked for a texture past the
/// device limit (16384). Originals that large (phone panoramas) must take the CPU path
/// in export, and every image-sized Metal allocation in the app checks first.
@Suite(.serialized) struct PanoramaExportTests {
    static func jpeg(width w: Int, height h: Int, in dir: URL) throws -> URL {
        let ctx = try #require(CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                                         space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                         bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue))
        // A horizontal ramp, so a wrong resize shows.
        for x in stride(from: 0, to: w, by: 250) {
            ctx.setFillColor(CGColor(red: Double(x) / Double(w), green: 0.5, blue: 0.8, alpha: 1))
            ctx.fill(CGRect(x: x, y: 0, width: 250, height: h))
        }
        let url = dir.appendingPathComponent("pano\(w)x\(h).jpg")
        let d = try #require(CGImageDestinationCreateWithURL(url as CFURL, UTType.jpeg.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(d, try #require(ctx.makeImage()), nil)
        #expect(CGImageDestinationFinalize(d))
        return url
    }

    /// Sized and original-size exports of originals below, just past and far past the
    /// texture limit all succeed at the right size (before the fix: SIGABRT past 16384).
    @Test func panoramasExportInsteadOfCrashing() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-pano-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        for (w, h) in [(16000, 800), (17000, 800), (20000, 3000)] {
            let url = try Self.jpeg(width: w, height: h, in: dir)
            for (name, mode, value) in [("sized", ExportRecipe.SizeMode.longEdge, 2048.0),
                                        ("original", ExportRecipe.SizeMode.original, 0.0)] {
                let job = ExportJob(items: [ExportJobItem(photoID: PhotoID(1), url: url, seq: 1, edit: nil)],
                                    recipes: [ExportRecipe(name: "p", format: .jpeg, quality: 85, sizeMode: mode,
                                                           sizeValue: value, fileNameTemplate: "{seq}-\(name)")],
                                    defaultFolder: dir.appendingPathComponent("out\(w)"))
                let o = try await RustExportService().export(job) { _, _ in }
                #expect(o.failures.isEmpty, "\(w)x\(h) \(name): \(o.failures)")
                let want = name == "sized" ? (2048, Int((Double(h) * 2048 / Double(w)).rounded())) : (w, h)
                #expect(o.written.map { (Int($0.width), Int($0.height)) }.first.map { $0 == want } == true,
                        "\(w)x\(h) \(name): \(o.written.map { "\($0.width)x\($0.height)" })")
            }
        }
    }

    @Test func oversizeAllocationsAreRefusedNotAttempted() throws {
        guard let device = MTLCreateSystemDefaultDevice() else { return }
        let edge = MetalLimits.maxTextureEdge(device)
        #expect(edge >= 8192)
        #expect(MetalLimits.canMakeTexture(device, width: edge, height: 10))
        #expect(!MetalLimits.canMakeTexture(device, width: edge + 1, height: 10))
        #expect(!MetalLimits.canMakeTexture(device, width: 10, height: edge + 1))
        #expect(!MetalLimits.canMakeTexture(device, width: 0, height: 10))
        #expect(MetalLimits.canMakeBuffer(device, length: 1 << 20))
        #expect(!MetalLimits.canMakeBuffer(device, length: device.maxBufferLength + 1))
        #expect(!MetalLimits.canMakeBuffer(device, length: 0))
        // Viewer upload, histogram and mask textures: nil instead of an abort.
        let wide = RenderedFrame(width: edge + 8, height: 4, rgba: Data(count: (edge + 8) * 4 * 4))
        let viewer = try MetalViewerRenderer(pixelFormat: .rgba8Unorm_srgb)
        #expect(viewer.makeTexture(from: wide) == nil)
        #expect(viewer.renderOffscreen([], width: edge + 1, height: 4, clear: .init()) == nil)
        #expect(HistogramEngine.shared.texture(for: wide, device: device) == nil)
        // The export resampler: refuses (so Rust takes the CPU path) and throws, never aborts.
        let r = try #require(ExportResampler.shared)
        #expect(r.canResample(width: 6000, height: 4000))
        #expect(!r.canResample(width: edge + 1, height: 800))
        #expect(!r.canResample(width: 12000, height: 9000), "past the memory budget")
        let img = CIImage(color: .gray).cropped(to: CGRect(x: 0, y: 0, width: edge + 1, height: 2))
        #expect(throws: (any Error).self) {
            _ = try r.resample(img, space: CGColorSpace(name: CGColorSpace.sRGB)!, width: 100, height: 1, sharpen: 0)
        }
    }
}
