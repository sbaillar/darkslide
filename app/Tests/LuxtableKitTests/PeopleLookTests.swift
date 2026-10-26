import CoreGraphics
import CoreML
import LuxtableAI
import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Visual checks (LOOK) of people masks and LaMa on real photos. Runs only when
/// `LT_LOOK_DIR` names an output folder; `LT_LOOK_IMAGES` lists photos (`:`-separated,
/// RAWs decode through their embedded preview), `LT_FACE_PARSING_PACKAGE` and
/// `LT_LAMA_PACKAGE` local `.mlpackage` copies. Writes PNG overlays to look at.
@MainActor @Suite(.serialized) struct PeopleLookTests {
    static let env = ProcessInfo.processInfo.environment
    static var out: URL? { env["LT_LOOK_DIR"].map { URL(fileURLWithPath: $0, isDirectory: true) } }

    nonisolated static func load(_ path: String, maxPixelSize: Int = 1024) -> CGImage? {
        guard let src = CGImageSourceCreateWithURL(URL(fileURLWithPath: path) as CFURL, nil) else { return nil }
        let opts: [CFString: Any] = [kCGImageSourceCreateThumbnailFromImageAlways: true,
                                     kCGImageSourceCreateThumbnailWithTransform: true,
                                     kCGImageSourceThumbnailMaxPixelSize: maxPixelSize]
        return CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary)
    }

    static func write(_ rgba: [UInt8], width w: Int, height h: Int, to url: URL) {
        var px = rgba
        let img = px.withUnsafeMutableBytes { buf in
            CGContext(data: buf.baseAddress, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                      space: CGColorSpace(name: CGColorSpace.sRGB)!,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!.makeImage()!
        }
        let d = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)!
        CGImageDestinationAddImage(d, img, nil)
        CGImageDestinationFinalize(d)
    }

    /// `mask` tinted over `image` (red), in a `cols`-wide tile grid with the others.
    static func overlay(_ image: PixelImage, _ mask: [UInt8], tint: SIMD3<Float> = SIMD3(255, 40, 40)) -> [UInt8] {
        var out = image.rgba
        for i in 0..<(image.width * image.height) {
            let a = Float(mask[i]) / 255 * 0.6
            for k in 0..<3 { out[i * 4 + k] = UInt8(Float(out[i * 4 + k]) * (1 - a) + tint[k] * a) }
        }
        return out
    }

    static func faceParser() async throws -> (any FaceParser)? {
        guard let pkg = env["LT_FACE_PARSING_PACKAGE"] else { return nil }
        let m = DownloadableModel(root: FileManager.default.temporaryDirectory.appendingPathComponent("lt-look-fp"),
                                        descriptor: .faceParsing, downloader: LocalModelDownloader(package: URL(fileURLWithPath: pkg)))
        let model = try await m.loadedModel()
        return CoreMLFaceParser(model: model)
    }

    @Test func peopleMasksOnPhotos() async throws {
        guard let out = Self.out, let list = Self.env["LT_LOOK_IMAGES"] else { return }
        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        let parser = try await Self.faceParser()
        let service = PeopleMaskService(parser: { parser })
        for path in list.split(separator: ":").map(String.init) {
            guard let img = Self.load(path), let px = PixelImage(img) else { continue }
            let t0 = Date()
            let masks = try await service.masks(for: img)
            let ms = Date().timeIntervalSince(t0) * 1000
            let name = URL(fileURLWithPath: path).deletingPathExtension().lastPathComponent
            print("LOOK \(name) \(px.width)x\(px.height) persons=\(masks.persons.count) \(Int(ms)) ms")
            for t in PeopleTargets.parts {
                guard let m = masks.all[t] else { continue }
                print("  \(t.rawValue) coverage \(String(format: "%.4f", m.coverage))")
                Self.write(Self.overlay(px, m.bytes), width: px.width, height: px.height,
                           to: out.appendingPathComponent("\(name)-\(t.rawValue).png"))
            }
            // Hair edge zoom: the hair mask's bounding box, padded.
            if let hair = masks.all[.hair], hair.coverage > 0 {
                var x0 = px.width, y0 = px.height, x1 = 0, y1 = 0
                for i in 0..<(px.width * px.height) where hair.bytes[i] > 64 {
                    x0 = min(x0, i % px.width); x1 = max(x1, i % px.width); y0 = min(y0, i / px.width); y1 = max(y1, i / px.width)
                }
                let pad = 12
                x0 = max(0, x0 - pad); y0 = max(0, y0 - pad); x1 = min(px.width - 1, x1 + pad); y1 = min(px.height - 1, y1 + pad)
                let cw = x1 - x0 + 1, ch = y1 - y0 + 1, s = max(1, 400 / max(cw, ch))
                var zoom = [UInt8](repeating: 255, count: cw * s * ch * s * 4)
                let tinted = Self.overlay(px, hair.bytes, tint: SIMD3(40, 255, 40))
                for y in 0..<(ch * s) {
                    for x in 0..<(cw * s) {
                        let si = ((y0 + y / s) * px.width + x0 + x / s) * 4, di = (y * cw * s + x) * 4
                        for k in 0..<4 { zoom[di + k] = tinted[si + k] }
                    }
                }
                Self.write(zoom, width: cw * s, height: ch * s, to: out.appendingPathComponent("\(name)-hair-zoom.png"))
            }
        }
    }

    @Test func lamaOnACrop() async throws {
        guard let out = Self.out, let pkg = Self.env["LT_LAMA_PACKAGE"], let list = Self.env["LT_LOOK_IMAGES"],
              let path = list.split(separator: ":").first.map(String.init),
              let img = Self.load(path, maxPixelSize: 2048), let px = PixelImage(img) else { return }
        let m = DownloadableModel(root: FileManager.default.temporaryDirectory.appendingPathComponent("lt-look-lama"),
                                        descriptor: .lama, downloader: LocalModelDownloader(package: URL(fileURLWithPath: pkg)))
        let model = try await m.loadedModel()
        let inpainter = LamaInpainter(model: CoreMLInpaintModel(model: model))
        // A crop of the frame's centre-bottom with a vertical bar hole.
        let w = min(640, px.width), h = min(480, px.height)
        let ox = (px.width - w) / 2, oy = px.height - h
        var rgb = [UInt8](repeating: 0, count: w * h * 3), mask = [UInt8](repeating: 0, count: w * h)
        for y in 0..<h {
            for x in 0..<w {
                let s = ((oy + y) * px.width + ox + x) * 4
                for k in 0..<3 { rgb[(y * w + x) * 3 + k] = px.rgba[s + k] }
                if abs(x - w / 2) < 40, abs(y - h / 2) < 120 { mask[y * w + x] = 255 }
            }
        }
        var times: [Double] = []
        var result: Data?
        for _ in 0..<3 {
            let t0 = Date()
            result = inpainter.inpaint(width: UInt32(w), height: UInt32(h), rgb: Data(rgb), mask: Data(mask))
            times.append(Date().timeIntervalSince(t0) * 1000)
        }
        print("LOOK lama \(w)x\(h) ms \(times.map { Int($0) })")
        let filled = try #require(result)
        func rgba(_ b: [UInt8]) -> [UInt8] {
            var o = [UInt8](repeating: 255, count: w * h * 4)
            for i in 0..<(w * h) { for k in 0..<3 { o[i * 4 + k] = b[i * 3 + k] } }
            return o
        }
        var holed = rgb
        for i in 0..<(w * h) where mask[i] > 0 { holed[i * 3] = 255; holed[i * 3 + 1] = 0; holed[i * 3 + 2] = 255 }
        Self.write(rgba(holed), width: w, height: h, to: out.appendingPathComponent("lama-input.png"))
        Self.write(rgba([UInt8](filled)), width: w, height: h, to: out.appendingPathComponent("lama-output.png"))
        // Raw model polarity check: an inverted mask must change the known area instead.
        let raw = try CoreMLInpaintModel(model: model).predict(rgb: InpaintFit(width: w, height: h, side: 800).squareRGB(rgb),
                                                                mask: [UInt8](repeating: 0, count: 800 * 800))
        var diff = 0
        let sq = InpaintFit(width: w, height: h, side: 800).squareRGB(rgb)
        for i in 0..<raw.count { diff += abs(Int(raw[i]) - Int(sq[i])) }
        print("LOOK lama empty-mask mean abs diff \(Double(diff) / Double(raw.count))")
    }
}
