import CoreGraphics
import DarkslideAI
import DarkslideFFI
import Foundation
import ImageIO
import UniformTypeIdentifiers
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// gpu-audit items on the app side, with before/after timings (printed `GPUAUDIT`).
@Suite struct GpuAuditTests {
    static func ms(_ d: Duration) -> Double { Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15 }

    /// A 2560×1707 16F linear-P3 frame: a smooth gradient with values above 1 (EDR).
    static func floatFrame(width w: Int = 2560, height h: Int = 1707) -> RenderedFrame {
        var bytes = Data(count: w * h * 8)
        bytes.withUnsafeMutableBytes { raw in
            let p = raw.bindMemory(to: Float16.self)
            for y in 0..<h {
                for x in 0..<w {
                    let i = (y * w + x) * 4
                    p[i] = Float16(Float(x) / Float(w) * 1.4)
                    p[i + 1] = Float16(Float(y) / Float(h))
                    p[i + 2] = Float16(0.25)
                    p[i + 3] = 1
                }
            }
        }
        return RenderedFrame(width: w, height: h, format: .rgba16fLinearP3, bytesPerRow: w * 8, backing: .data(bytes))
    }

    /// #1: the navigator image of a float frame is scaled + converted on the GPU and
    /// agrees with the CPU SDR conversion (within a couple of levels).
    @Test func navigatorThumbnailOnTheGPUMatchesTheCPUConversion() throws {
        let f = Self.floatFrame()
        let clock = ContinuousClock()
        let t0 = clock.now
        let cpu = try #require(ColorMath.sdrFrame(f).makeCGImage())
        let before = Self.ms(clock.now - t0)
        _ = NavigatorThumbnail.make(f) // warm the Core Image context
        let t1 = clock.now
        let gpu = try #require(NavigatorThumbnail.make(f))
        let after = Self.ms(clock.now - t1)
        print(String(format: "GPUAUDIT navigator 2560x1707 16F: CPU sdrFrame %.1f ms -> Core Image %.1f ms", before, after))
        #expect(max(gpu.width, gpu.height) == NavigatorThumbnail.maxEdge)
        // Compare a few points against the CPU conversion (scaled coordinates).
        let a = GpuFixtures.pixels(gpu), b = GpuFixtures.pixels(cpu)
        for (fx, fy) in [(0.1, 0.1), (0.5, 0.5), (0.9, 0.3), (0.3, 0.9)] {
            let (gx, gy) = (Int(fx * Double(gpu.width)), Int(fy * Double(gpu.height)))
            let (cx, cy) = (Int(fx * Double(cpu.width)), Int(fy * Double(cpu.height)))
            for c in 0..<3 {
                let dv = abs(Int(a[(gy * gpu.width + gx) * 4 + c]) - Int(b[(cy * cpu.width + cx) * 4 + c]))
                #expect(dv <= 4, "(\(fx),\(fy)) channel \(c): \(dv)")
            }
        }
        // 8-bit frames pass through the same helper too.
        let eight = RenderedFrame(width: 800, height: 400, rgba: Data(repeating: 128, count: 800 * 400 * 4))
        #expect(NavigatorThumbnail.make(eight).map { max($0.width, $0.height) } == NavigatorThumbnail.maxEdge)
    }

    // MARK: #4 non-RAW originals sized on the GPU

    /// The app's decoder with the sized path turned off: `upright_size` fails, so
    /// Rust takes the full-size path (full decode, then ds-export's CPU resize and
    /// sharpen): the behaviour before gpu-audit #4.
    final class FullSizeOnly: ImageEncoder, @unchecked Sendable {
        let inner = ImageIOEncoder()
        func encode(format: EncodedImageFormat, width: UInt32, height: UInt32, rgb16: Data, quality: UInt8,
                    icc: Data, exif: Data?, keywords: [String], metadata: PhotoMetadata) throws -> Data {
            try inner.encode(format: format, width: width, height: height, rgb16: rgb16, quality: quality, icc: icc,
                             exif: exif, keywords: keywords, metadata: metadata)
        }
        func uprightSize(path: String) throws -> ImageDimensions { throw DsError.Io(message: "full size only") }
        func decode(path: String, space: ExportColorSpace, target: DecodeTarget?) throws -> DecodedImage {
            try inner.decode(path: path, space: space, target: nil)
        }
    }

    static func tempDir(_ tag: String) throws -> URL {
        let d = FileManager.default.temporaryDirectory.appendingPathComponent("ds-gpuaudit-\(tag)-\(UUID())")
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        return d
    }

    static func write(_ img: CGImage, to url: URL, type: UTType, orientation: Int, quality: Double = 0.95) throws {
        let dest = try #require(CGImageDestinationCreateWithURL(url as CFURL, type.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(dest, img, [kCGImagePropertyOrientation: orientation,
                                               kCGImageDestinationLossyCompressionQuality: quality] as CFDictionary)
        #expect(CGImageDestinationFinalize(dest))
    }

    /// A 24 MP original: a sample RAW's own decode, or (without samples) a
    /// synthetic scene with gradients, edges and fine texture.
    static func bigOriginal() throws -> CGImage {
        if let raw = sampleARWs().first,
           let src = CGImageSourceCreateWithURL(raw as CFURL, nil),
           let img = CGImageSourceCreateImageAtIndex(src, 0, nil) {
            return img
        }
        let (w, h) = (6000, 4000)
        let ctx = try #require(CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                                         space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                         bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue))
        let p = ctx.data!.bindMemory(to: UInt8.self, capacity: w * h * 4)
        for y in 0..<h {
            for x in 0..<w {
                let i = (y * w + x) * 4
                let n = UInt8(truncatingIfNeeded: (x &* 7919 ^ y &* 104_729) >> 3) & 31
                p[i] = UInt8(x * 200 / w) &+ n
                p[i + 1] = ((x / 40 + y / 40) % 2 == 0) ? 200 : 40
                p[i + 2] = UInt8(y * 220 / h) &+ n
            }
        }
        return try #require(ctx.makeImage())
    }

    /// Interleaved 16-bit RGB of a file (as stored: no color conversion for an sRGB file).
    static func rgb16(_ url: URL) throws -> (Int, Int, [UInt16]) {
        let src = try #require(CGImageSourceCreateWithURL(url as CFURL, nil))
        let img = try #require(CGImageSourceCreateImageAtIndex(src, 0, nil))
        let (w, h) = (img.width, img.height)
        let ctx = try #require(CGContext(data: nil, width: w, height: h, bitsPerComponent: 16, bytesPerRow: w * 8,
                                         space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                         bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue
                                             | ImageIOEncoder.nativeByteOrder16.rawValue))
        ctx.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
        let p = ctx.data!.bindMemory(to: UInt16.self, capacity: w * h * 4)
        var out = [UInt16](repeating: 0, count: w * h * 3)
        for i in 0..<(w * h) { for c in 0..<3 { out[i * 3 + c] = p[i * 4 + c] } }
        return (w, h, out)
    }

    /// sRGB-encoded 16-bit → CIE Lab (D65).
    static func lab(_ r: UInt16, _ g: UInt16, _ b: UInt16) -> (Double, Double, Double) {
        func lin(_ v: UInt16) -> Double {
            let c = Double(v) / 65535
            return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
        }
        let (R, G, B) = (lin(r), lin(g), lin(b))
        let x = (0.4124564 * R + 0.3575761 * G + 0.1804375 * B) / 0.95047
        let y = 0.2126729 * R + 0.7151522 * G + 0.0721750 * B
        let z = (0.0193339 * R + 0.1191920 * G + 0.9503041 * B) / 1.08883
        func f(_ t: Double) -> Double { t > 216.0 / 24389 ? cbrt(t) : (24389.0 / 27 * t + 16) / 116 }
        return (116 * f(y) - 16, 500 * (f(x) - f(y)), 200 * (f(y) - f(z)))
    }

    /// CIEDE2000 (kL = kC = kH = 1).
    static func deltaE2000(_ a: (Double, Double, Double), _ b: (Double, Double, Double)) -> Double {
        let (L1, a1, b1) = a, (L2, a2, b2) = b
        let deg = Double.pi / 180
        let Cb = (hypot(a1, b1) + hypot(a2, b2)) / 2
        let G = 0.5 * (1 - sqrt(pow(Cb, 7) / (pow(Cb, 7) + pow(25, 7))))
        let (ap1, ap2) = ((1 + G) * a1, (1 + G) * a2)
        let (C1, C2) = (hypot(ap1, b1), hypot(ap2, b2))
        func hue(_ b: Double, _ a: Double) -> Double {
            if a == 0 && b == 0 { return 0 }
            let h = atan2(b, a) / deg
            return h < 0 ? h + 360 : h
        }
        let (h1, h2) = (hue(b1, ap1), hue(b2, ap2))
        let dL = L2 - L1, dC = C2 - C1
        var dh = h2 - h1
        if C1 * C2 == 0 { dh = 0 } else if dh > 180 { dh -= 360 } else if dh < -180 { dh += 360 }
        let dH = 2 * sqrt(C1 * C2) * sin(dh / 2 * deg)
        let Lb = (L1 + L2) / 2, Cpb = (C1 + C2) / 2
        var hb = h1 + h2
        if C1 * C2 != 0 {
            hb = abs(h1 - h2) > 180 ? (h1 + h2 + (h1 + h2 < 360 ? 360 : -360)) / 2 : (h1 + h2) / 2
        }
        let T = 1 - 0.17 * cos((hb - 30) * deg) + 0.24 * cos(2 * hb * deg) + 0.32 * cos((3 * hb + 6) * deg)
            - 0.20 * cos((4 * hb - 63) * deg)
        let dTheta = 30 * exp(-pow((hb - 275) / 25, 2))
        let Rc = 2 * sqrt(pow(Cpb, 7) / (pow(Cpb, 7) + pow(25, 7)))
        let Sl = 1 + 0.015 * pow(Lb - 50, 2) / sqrt(20 + pow(Lb - 50, 2))
        let Sc = 1 + 0.045 * Cpb, Sh = 1 + 0.015 * Cpb * T
        let Rt = -sin(2 * dTheta * deg) * Rc
        return sqrt(pow(dL / Sl, 2) + pow(dC / Sc, 2) + pow(dH / Sh, 2) + Rt * (dC / Sc) * (dH / Sh))
    }

    @Test func deltaE2000MatchesSharmaReferenceValues() {
        // Sharma, Wu, Dalal (2005) test pairs 1, 7 and 17.
        #expect(abs(Self.deltaE2000((50, 2.6772, -79.7751), (50, 0, -82.7485)) - 2.0425) < 1e-4)
        #expect(abs(Self.deltaE2000((50, 0, 0), (50, -1, 2)) - 2.3669) < 1e-4)
        #expect(abs(Self.deltaE2000((50, 2.5, 0), (73, 25, -18)) - 27.1492) < 1e-4)
    }

    /// #4: a 24 MP JPEG (EXIF orientation 6) exported at 2048 px long edge with
    /// output sharpening: the GPU path (hardware decode + one Core Image pass)
    /// matches the CPU path (full decode + ds-export resize + sharpen) within
    /// ΔE2000 mean 0.1, and is timed against it (printed `GPUAUDIT`).
    @Test func sizedRasterExportMatchesTheCPUPath() async throws {
        let root = try Self.tempDir("raster")
        defer { try? FileManager.default.removeItem(at: root) }
        let jpg = root.appendingPathComponent("IMG_0001.JPG")
        try Self.write(Self.bigOriginal(), to: jpg, type: .jpeg, orientation: 6)
        let clock = ContinuousClock()
        func export(_ label: String, _ encoder: ImageEncoder, sharpen: Float = 0.5) async throws -> (URL, Double) {
            let dest = root.appendingPathComponent(label)
            let spec = ExportRecipeSpec(name: label, format: .tiff16, size: .longEdge(pixels: 2048),
                                        allowUpscale: false, space: .srgb, sharpen: sharpen, fileNameTemplate: "{name}",
                                        destination: dest.path, includeExif: false, stripGps: false)
            let t = clock.now
            let rep = try await exportBatch(sources: [ExportSource(id: 1, path: jpg.path, seq: 1, gps: nil, artist: nil,
                                                                   copyright: nil)],
                                            recipes: [spec], encoder: encoder, hardwareJpeg: false, listener: nil,
                                            token: try CancelToken())
            #expect(rep.failed.isEmpty && rep.written.count == 1, "\(rep.failed)")
            return (URL(fileURLWithPath: try #require(rep.written.first).path), Self.ms(clock.now - t))
        }
        _ = try await export("warm-gpu", ImageIOEncoder())
        _ = try await export("warm-cpu", FullSizeOnly())
        var (gpuMs, cpuMs) = ([Double](), [Double]())
        var files: (URL, URL)?
        for i in 0..<3 {
            let (g, gt) = try await export("gpu\(i)", ImageIOEncoder())
            let (c, ct) = try await export("cpu\(i)", FullSizeOnly())
            gpuMs.append(gt)
            cpuMs.append(ct)
            files = (g, c)
        }
        let (g, c) = try #require(files)
        let (gw, gh, gp) = try Self.rgb16(g), (cw, ch, cp) = try Self.rgb16(c)
        #expect((gw, gh) == (cw, ch) && (gw, gh) == (1365, 2048), "\(gw)x\(gh) vs \(cw)x\(ch): upright, exact size")
        var sum = 0.0, worst = 0.0
        for i in stride(from: 0, to: gw * gh, by: 1) {
            let d = Self.deltaE2000(Self.lab(gp[i * 3], gp[i * 3 + 1], gp[i * 3 + 2]),
                                    Self.lab(cp[i * 3], cp[i * 3 + 1], cp[i * 3 + 2]))
            sum += d
            worst = max(worst, d)
        }
        let mean = sum / Double(gw * gh)
        print(String(format: "GPUAUDIT raster export 24MP JPEG -> 2048px TIFF16 + sharpen: CPU path %.0f ms (min of 3) -> GPU path %.0f ms; ΔE2000 mean %.3f max %.2f",
                     cpuMs.min()!, gpuMs.min()!, mean, worst))
        #expect(mean <= 0.1, "ΔE2000 mean \(mean)")
    }

    /// #4: the sized decode is exact for all 8 EXIF orientations and keeps 16-bit
    /// precision (a full-size target with no sharpening equals the full decode).
    @Test func sizedDecodeIsExactForEveryOrientationAt16Bits() throws {
        let root = try Self.tempDir("orient")
        defer { try? FileManager.default.removeItem(at: root) }
        let (w, h) = (48, 20)
        let ctx = try #require(CGContext(data: nil, width: w, height: h, bitsPerComponent: 16, bytesPerRow: w * 8,
                                         space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                         bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue
                                             | ImageIOEncoder.nativeByteOrder16.rawValue))
        let p = ctx.data!.bindMemory(to: UInt16.self, capacity: w * h * 4)
        for y in 0..<h {
            for x in 0..<w {
                let i = (y * w + x) * 4
                p[i] = UInt16(x * 1300 + y * 7)      // fine 16-bit steps
                p[i + 1] = UInt16(y * 3000 + x)
                p[i + 2] = (x < 6 && y < 4) ? 65535 : 1000 // a marked corner
            }
        }
        let img = try #require(ctx.makeImage())
        let e = ImageIOEncoder()
        for o in 1...8 {
            let url = root.appendingPathComponent("o\(o).tif")
            try Self.write(img, to: url, type: .tiff, orientation: o)
            let dims = try e.uprightSize(path: url.path)
            #expect(dims == (o >= 5 ? ImageDimensions(width: 20, height: 48) : ImageDimensions(width: 48, height: 20)))
            let full = try e.decode(path: url.path, space: .srgb, target: nil)
            let sized = try e.decode(path: url.path, space: .srgb,
                                     target: DecodeTarget(width: dims.width, height: dims.height, sharpen: 0))
            #expect((sized.width, sized.height) == (full.width, full.height), "orientation \(o)")
            let a = full.rgb16.withUnsafeBytes { Array($0.bindMemory(to: UInt16.self)) }
            let b = sized.rgb16.withUnsafeBytes { Array($0.bindMemory(to: UInt16.self)) }
            let worst = zip(a, b).map { abs(Int($0) - Int($1)) }.max() ?? .max
            #expect(worst <= 2, "orientation \(o): max diff \(worst) of 65535")
            #expect(Set(b).count > 1000, "16-bit levels kept")
        }
        // An unreadable file.
        #expect(throws: DsError.self) { try e.uprightSize(path: root.appendingPathComponent("none.jpg").path) }
    }

    // MARK: #5 mask versions

    /// Every mask made gets a new version (the pipeline's raster cache key); copies
    /// keep it, equality is by content, and the viewer/export adapters pass it on.
    @Test func everyGeneratedMaskGetsANewVersion() {
        let a = VisionMask(width: 2, height: 1, bytes: [0, 255])
        let b = VisionMask(width: 2, height: 1, bytes: [0, 255])
        let copy = a
        #expect(a == b, "equal by content")
        #expect(b.version > a.version, "a new mask, a new version")
        #expect(copy.version == a.version)
        let d = StaticMasks([.subject: a]).aiMask(target: .subject)
        #expect(d?.version == a.version && d?.data == Data([0, 255]))
    }
}
