import CoreGraphics
import Foundation
import ImageIO
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Final-wave review (golden): for crop, straighten, turns, keystone and lens edits, the
/// export's pixels match the viewer's settled frame (same edit) far better than a 3 px
/// shift would, at the same aspect, with no black corners.
@MainActor
@Suite(.serialized) struct CropExportGoldenTests {
    static func thumb(_ img: CGImage, w: Int, h: Int) -> [Double] {
        let cs = CGColorSpace(name: CGColorSpace.sRGB)!
        let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4, space: cs,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        ctx.interpolationQuality = .high
        ctx.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
        let p = ctx.data!.bindMemory(to: UInt8.self, capacity: w * h * 4)
        return (0..<(w * h)).map { i in (Double(p[i * 4]) + Double(p[i * 4 + 1]) + Double(p[i * 4 + 2])) / 3 }
    }

    static func image(_ f: RenderedFrame) -> CGImage {
        let cs = CGColorSpace(name: f.format.isP3 ? CGColorSpace.displayP3 : CGColorSpace.sRGB)!
        let data = f.withBytes { Data($0.prefix(f.bytesPerRow * f.height)) }
        let provider = CGDataProvider(data: data as CFData)!
        return CGImage(width: f.width, height: f.height, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: f.bytesPerRow,
                       space: cs, bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
                       provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)!
    }

    /// Mean |Δ| of two images at `w`×`h`, and the mean |Δ| when the export is shifted by 3 px (sensitivity).
    static func compare(_ a: CGImage, _ b: CGImage, w: Int, h: Int) -> (diff: Double, shifted: Double, darkCorners: Double) {
        let ta = thumb(a, w: w, h: h), tb = thumb(b, w: w, h: h)
        var d = 0.0, s = 0.0, n = 0.0
        for y in 3..<(h - 3) {
            for x in 3..<(w - 3) {
                d += abs(ta[y * w + x] - tb[y * w + x])
                s += abs(ta[y * w + x] - tb[y * w + x + 3])
                n += 1
            }
        }
        let corners = [tb[0], tb[w - 1], tb[(h - 1) * w], tb[h * w - 1]]
        return (d / n, s / n, corners.min()!)
    }

    @Test func exportMatchesTheScreen() async throws {
        guard !sampleARWs().isEmpty else { return }
        let live = try #require(try await Vw8Live.make(limit: 1))
        defer { live.cleanup() }
        let m = live.app
        let id = try #require(m.focusedID)
        let url = try #require(m.focusedPhoto?.url)
        _ = try #require(await live.settled())
        let cases: [(String, (inout EditState) -> Void)] = [
            ("identity", { _ in }),
            ("4:5 + 5deg", { e in e.crop = CropEdit(); e.crop.aspect = .r4x5; e.crop.angle = 5
                e.crop.rect = CGRect(x: 0.3, y: 0.05, width: 0.45, height: 0.9) }),
            ("fullframe 8deg", { e in e.crop = CropEdit(); e.crop.angle = 8 }),
            ("quarter turn + 3:2 crop", { e in e.crop = CropEdit(); e.crop.quarterTurns = 1
                e.crop.rect = CGRect(x: 0.1, y: 0.2, width: 0.6, height: 0.5) }),
            ("keystone 15 + -4deg", { e in e.crop = CropEdit(); e.crop.angle = -4; e.lens.keystoneVertical = 15 }),
            ("lens off + 6deg", { e in e.crop = CropEdit(); e.lens = LensEdit(); e.lens.enabled = false; e.crop.angle = 6 }),
        ]
        for (name, change) in cases {
            let old = m.viewer.frame
            let before = m.edit(for: id)
            m.updateEdit(label: name) { e in e.lens = LensEdit(); change(&e) }
            m.endEditGesture()
            let f = try #require(await live.settled(after: m.edit(for: id) == before ? nil : old), "\(name): no frame")
            let edit = m.edit(for: id)
            let out = live.dir.appendingPathComponent("golden-\(UUID())")
            let long = max(f.width, f.height)
            let job = ExportJob(items: [ExportJobItem(photoID: id, url: url, seq: 1, edit: edit)],
                                recipes: [ExportRecipe(name: "golden", format: .jpeg, quality: 95, sizeMode: .longEdge,
                                                       sizeValue: Double(long), fileNameTemplate: "{seq}")],
                                defaultFolder: out)
            let o = try await RustExportService().export(job) { _, _ in }
            let w = try #require(o.written.first, "\(name): \(o.failures)")
            let src = try #require(CGImageSourceCreateWithURL(w.url as CFURL, nil))
            let exp = try #require(CGImageSourceCreateImageAtIndex(src, 0, nil))
            let screen = Self.image(f)
            let tw = 96, th = max(8, Int((96.0 * Double(f.height) / Double(f.width)).rounded()))
            let r = Self.compare(screen, exp, w: tw, h: th)
            let r2 = Self.compare(screen, screen, w: tw, h: th)
            print(String(format: "CROPGOLD %@ | screen %dx%d (fmt %@) export %dx%d | ratio screen %.4f export %.4f | mean|Δ| %.2f (3px-shift %.2f, self %.2f) | export darkest corner %.0f",
                         name, f.width, f.height, "\(f.format)", exp.width, exp.height, Double(f.width) / Double(f.height),
                         Double(exp.width) / Double(exp.height), r.diff, r.shifted, r2.diff, r.darkCorners))
            let ta = Self.thumb(screen, w: tw, h: th), tb = Self.thumb(exp, w: tw, h: th)
            let bias = zip(ta, tb).map { $1 - $0 }.reduce(0, +) / Double(ta.count)
            print(String(format: "CROPGOLD %@ signed mean (export - screen) %.2f; screen corners %.0f %.0f %.0f %.0f", name, bias, ta[0], ta[tw - 1], ta[(th - 1) * tw], ta[th * tw - 1]))
            #expect(abs(Double(f.width) / Double(f.height) - Double(exp.width) / Double(exp.height)) < 0.01, "\(name) ratio")
            #expect(r.diff < 6, "\(name): export differs from the screen: \(r.diff)")
            #expect(r.diff < r.shifted * 0.5, "\(name): closer than a 3 px shift (\(r.diff) vs \(r.shifted))")
            #expect(abs(bias) < 4, "\(name): no brightness bias (\(bias))")
            #expect(r.darkCorners > 8, "\(name): no black corners (\(r.darkCorners))")
        }
    }
}
