import CoreGraphics
import LuxtableAI
import LuxtableFFI
import Foundation
import ImageIO
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

// Integration B, export: RAWs are full develops through lt-pipeline (every
// edit, crop, straighten and turn), at each recipe's size.

private var fm: FileManager { .default }

/// Size of the full develop of `url` with `edit` (nil = defaults), from the
/// pipeline's own plan: what an Original-size export writes.
func ex8DevelopedSize(_ url: URL, edit: EditState?) async throws -> (Int, Int) {
    let json = try edit.map { try AdjustmentsCodec.encode($0) } ?? defaultAdjustmentsJson()
    let req = DevelopRequest(maxWidth: 1 << 16, maxHeight: 1 << 16, region: nil, quality: .full, priority: .background,
                             format: .rgba8Srgb, histogram: false, geometry: .final)
    let plan = try await planDevelop(path: url.path, adjustmentsJson: json, request: req, token: try CancelToken())
    return (Int(plan.width), Int(plan.height))
}

/// Mean luminance (0…255) of an image file, drawn small.
func ex8MeanLuma(_ url: URL) -> Double? {
    guard let src = CGImageSourceCreateWithURL(url as CFURL, nil),
          let img = CGImageSourceCreateThumbnailAtIndex(src, 0, [kCGImageSourceCreateThumbnailFromImageAlways: true,
                                                                 kCGImageSourceThumbnailMaxPixelSize: 256] as CFDictionary)
    else { return nil }
    var px = [UInt8](repeating: 0, count: img.width * img.height)
    guard let ctx = CGContext(data: &px, width: img.width, height: img.height, bitsPerComponent: 8, bytesPerRow: img.width,
                              space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue)
    else { return nil }
    ctx.draw(img, in: CGRect(x: 0, y: 0, width: img.width, height: img.height))
    return Double(px.reduce(0) { $0 + Int($1) }) / Double(px.count)
}

private func tempDir(_ tag: String) throws -> URL {
    let d = fm.temporaryDirectory.appendingPathComponent("lt-ex8-\(tag)-\(UUID())", isDirectory: true)
    try fm.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

@MainActor
@Suite struct Ex8RecipeTests {
    /// "Full-size JPEG" is the default again, and the preview-era rename is undone
    /// on load; a user's own recipe with that name and a real size stays.
    @Test func fullSizeJPEGIsBackAndPreviewEraNamesMigrate() {
        #expect(ExportRecipe.defaults[0].name == "Full-size JPEG" && ExportRecipe.defaults[0].sizeMode == .original)
        #expect(ExportRecipe.defaults[0].name == ExportRecipe.fullSizeJPEGName)
        let d = testDefaults()
        let store = ExportRecipeStore(defaults: d)
        var saved = ExportRecipe.defaults
        saved[0].name = "Largest JPEG (preview size)"
        saved.append(ExportRecipe(name: "Largest JPEG (preview size)", sizeMode: .longEdge, sizeValue: 1600))
        store.save(saved)
        let loaded = store.load()
        #expect(loaded[0].name == "Full-size JPEG")
        #expect(loaded.last?.name == "Largest JPEG (preview size)", "a sized recipe of the user's keeps its name")
    }

    /// "Constrain to sensor pixels": off by default, saved, reaches the FFI spec, and
    /// recipes saved before it existed still load.
    @Test func constrainToSensorPixelsIsSavedAndOldRecipesLoad() throws {
        #expect(ExportRecipe.defaults.allSatisfy { !$0.constrainToSensorPixels })
        let r = ExportRecipe(name: "c", constrainToSensorPixels: true)
        let data = try JSONEncoder().encode([r])
        #expect(try JSONDecoder().decode([ExportRecipe].self, from: data) == [r])
        #expect(RustExportService.spec(r, defaultFolder: URL(fileURLWithPath: "/tmp")).constrainToSensorPixels)
        #expect(!RustExportService.spec(ExportRecipe(name: "d"), defaultFolder: URL(fileURLWithPath: "/tmp"))
            .constrainToSensorPixels)
        var old = try #require(try JSONSerialization.jsonObject(with: JSONEncoder().encode(ExportRecipe(name: "old")))
            as? [String: Any])
        old.removeValue(forKey: "constrainToSensorPixels")
        let decoded = try JSONDecoder().decode(ExportRecipe.self, from: JSONSerialization.data(withJSONObject: old))
        #expect(decoded.name == "old" && !decoded.constrainToSensorPixels)
    }
}

@MainActor
@Suite(.serialized) struct Ex8LiveExportTests {
    /// A real sample through the develop pipeline: Original is exactly the sensor size
    /// (6000×4000) whether lens correction or a straighten auto-fits the frame (it is
    /// scaled back up, Lightroom's rule) or not; "Constrain to sensor pixels" gives the
    /// smaller inscribed size; +1 EV is brighter, a crop halves the size, a quarter
    /// turn swaps it; the UTC offset is written when the camera recorded one (E-L6).
    @Test func fullSizeIsTheFullDevelopAndEveryEditApplies() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        defer { try? fm.removeItem(at: dir) }
        let raw = try #require(try fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
            .first { $0.pathExtension == "ARW" })
        var ev = EditState()
        ev.exposure.ev = 1
        var lensOff = EditState()
        lensOff.lens.enabled = false
        var crop = EditState()
        crop.crop.rect = CGRect(x: 0.25, y: 0.25, width: 0.5, height: 0.5)
        var turned = crop
        turned.crop.quarterTurns = 1
        var straight = EditState()
        straight.crop.angle = 5
        let edits: [(String, EditState?)] = [("plain", nil), ("ev", ev), ("lensoff", lensOff), ("crop", crop),
                                             ("turned", turned), ("straight", straight)]
        let items = edits.enumerated().map { i, e in
            ExportJobItem(photoID: PhotoID(Int64(i + 1)), url: raw, seq: i + 1, edit: e.1)
        }
        let out = dir.appendingPathComponent("Output")
        let recipes = [ExportRecipe(name: ExportRecipe.fullSizeJPEGName, format: .jpeg, quality: 90,
                                    fileNameTemplate: "{seq}"),
                       ExportRecipe(name: "Web", format: .jpeg, quality: 85, sizeMode: .longEdge, sizeValue: 600,
                                    sharpen: 0.4, fileNameTemplate: "{seq}", subfolder: "web"),
                       ExportRecipe(name: "Constrained", format: .jpeg, quality: 85, fileNameTemplate: "{seq}",
                                    subfolder: "constrained", constrainToSensorPixels: true)]
        let t0 = ContinuousClock.now
        let o = try await RustExportService().export(ExportJob(items: items, recipes: recipes, defaultFolder: out)) { _, _ in }
        print("EX8 12 full-size develops + 6 web: \(ContinuousClock.now - t0) (\(o.renderer))")
        #expect(o.failures.isEmpty, "\(o.failures)")
        #expect(o.written.count == 18 && o.renderer.hasPrefix("develop ("))
        func full(_ seq: Int) throws -> ExportedOutput {
            try #require(o.written.first { $0.recipeName == ExportRecipe.fullSizeJPEGName && $0.photoID == PhotoID(Int64(seq)) })
        }
        let meta = try await rawMetadata(path: raw.path, token: try CancelToken())
        let (sw, sh) = (5...8).contains(meta.orientation) ? (4000, 6000) : (6000, 4000)
        for (i, (name, e)) in edits.enumerated() {
            let w = try full(i + 1)
            let want = try await ex8DevelopedSize(raw, edit: e)
            #expect((w.width, w.height) == want, "\(name): \(w.width)×\(w.height) vs develop \(want)")
            let px = try #require(CGImageSourceCreateWithURL(w.url as CFURL, nil)
                .flatMap { CGImageSourceCopyPropertiesAtIndex($0, 0, nil) } as? [CFString: Any])
            #expect(px[kCGImagePropertyPixelWidth] as? Int == w.width && px[kCGImagePropertyPixelHeight] as? Int == w.height)
        }
        let plain = try full(1), lens = try full(3)
        #expect((plain.width, plain.height) == (sw, sh), "lens-corrected: the sensor size")
        #expect((lens.width, lens.height) == (sw, sh), "lens correction off: the sensor size")
        func constrained(_ seq: Int) throws -> ExportedOutput {
            try #require(o.written.first { $0.recipeName == "Constrained" && $0.photoID == PhotoID(Int64(seq)) })
        }
        let c1 = try constrained(1), c3 = try constrained(3), c6 = try constrained(6)
        #expect(c1.width < sw && c1.height < sh && c1.width * c1.height > sw * sh * 9 / 10,
                "constrained, lens-corrected: the inscribed \(c1.width)×\(c1.height)")
        #expect((c3.width, c3.height) == (sw, sh), "nothing auto-fitted: the sensor size either way")
        #expect(c6.width < c1.width && c6.height < c1.height, "straightened and constrained: smaller still")
        print("EX8 constrained to sensor pixels: \(c1.width)×\(c1.height), straightened \(c6.width)×\(c6.height)")
        let (cw, ch) = (try full(4).width, try full(4).height)
        #expect(abs(cw - sw / 2) <= 2 && abs(ch - sh / 2) <= 2, "crop \(cw)×\(ch)")
        #expect((try full(5).width, try full(5).height) == (ch, cw), "a quarter turn swaps the crop")
        let s = try full(6)
        #expect((s.width, s.height) == (sw, sh), "straightened: filled back to the sensor size")
        let l0 = try #require(ex8MeanLuma(plain.url)), l1 = try #require(ex8MeanLuma(try full(2).url))
        print(String(format: "EX8 mean luma 0 EV %.1f, +1 EV %.1f", l0, l1))
        #expect(l1 > l0 * 1.2, "+1 EV is brighter: \(l0) → \(l1)")
        for w in o.written where w.recipeName == "Web" { #expect(max(w.width, w.height) == 600) }
        // Nothing outside Output/ (and the original is untouched).
        let inDir = try fm.contentsOfDirectory(atPath: dir.path).filter { !$0.hasPrefix(".") }.sorted()
        #expect(inDir == ["Output", raw.lastPathComponent], "\(inDir)")
        let exif = (CGImageSourceCreateWithURL(plain.url as CFURL, nil)
            .flatMap { CGImageSourceCopyPropertiesAtIndex($0, 0, nil) } as? [CFString: Any])?[kCGImagePropertyExifDictionary]
            as? [CFString: Any]
        if let off = meta.captureTime?.utcOffsetMinutes {
            let a = abs(Int(off))
            #expect(exif?[kCGImagePropertyExifOffsetTimeOriginal] as? String
                == String(format: "%@%02d:%02d", off < 0 ? "-" : "+", a / 60, a % 60))
        } else {
            #expect(exif?[kCGImagePropertyExifOffsetTimeOriginal] == nil)
        }
        #expect(exif?[kCGImagePropertyExifDateTimeOriginal] != nil)
    }

    /// AI-mask layers export with the viewer's masks (the service's `maskProvider`,
    /// asked once per photo for the targets its edit uses): a +2 EV "subject" layer whose
    /// mask is the left half brightens only the left half of the file.
    @Test func aiMaskLayersExportWithTheViewersMasks() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        defer { try? fm.removeItem(at: dir) }
        let raw = try #require(try fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
            .first { $0.pathExtension == "ARW" })
        var layered = EditState()
        layered.layers = [LayerEdit(id: 1, name: "Subject", exposureEV: 2,
                                    mask: MaskEdit(shapes: [.ai(.add, .subject)]))]
        let asked = Ex8Asked()
        var s = RustExportService()
        s.maskProvider = { id, targets in
            asked.add(id, targets)
            let (w, h) = (64, 48)
            return [.subject: VisionMask(width: w, height: h, bytes: (0..<(w * h)).map { $0 % w < w / 2 ? 255 : 0 })]
        }
        let out = dir.appendingPathComponent("Output")
        let o = try await s.export(ExportJob(
            items: [ExportJobItem(photoID: PhotoID(1), url: raw, seq: 1, edit: layered),
                    ExportJobItem(photoID: PhotoID(2), url: raw, seq: 2)],
            recipes: [ExportRecipe(name: "Web", sizeMode: .longEdge, sizeValue: 600, fileNameTemplate: "{seq}")],
            defaultFolder: out)) { _, _ in }
        #expect(o.failures.isEmpty && o.written.count == 2, "\(o.failures)")
        #expect(asked.calls == [PhotoID(1): [.subject]], "only the photo whose edit has AI shapes")
        // Masks live in the E frame (after EXIF orientation), so its left half is the file's.
        let (l1, r1) = try #require(ex8HalfLumas(out.appendingPathComponent("1.jpg")))
        let (l2, r2) = try #require(ex8HalfLumas(out.appendingPathComponent("2.jpg")))
        print(String(format: "EX8 AI mask layer: left %.1f -> %.1f, right %.1f -> %.1f", l2, l1, r2, r1))
        #expect(l1 > l2 * 1.3, "masked half brighter: \(l2) → \(l1)")
        #expect(abs(r1 - r2) < r2 * 0.1 + 2, "unmasked half about the same: \(r2) → \(r1)")
        // No provider: the service passes no masks (the AI shape is empty).
        #expect(await RustExportService().exportMasks([1: ExportJobItem(photoID: PhotoID(1), url: raw, seq: 1,
                                                                        edit: layered)]) == nil)
        let m = ExportMasks([7: [.sky: VisionMask(width: 2, height: 1, bytes: [0, 255])]])
        let sky = try #require(m.aiMask(sourceId: 7, target: .sky))
        #expect((sky.width, sky.height, sky.data) == (2, 1, Data([0, 255])) && sky.version > 0)
        #expect(m.aiMask(sourceId: 7, target: .subject) == nil && m.aiMask(sourceId: 8, target: .sky) == nil)
    }

    /// The app's ⌘E carries each photo's edits into the export.
    @Test func exportItemsCarryTheEdits() async throws {
        guard let dir = try sampleFolderCopy(limit: 2) else { return }
        defer { try? fm.removeItem(at: dir) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()),
                                     storeBase: fm.temporaryDirectory.appendingPathComponent("lt-ex8-store-\(UUID())"))
        let info = try await store.add(folder: dir)
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        m.exporter = RustExportService()
        await m.open(info)
        #expect(await waitUntil(timeout: .seconds(10)) { m.visibleIDs.count == 2 })
        let first = try #require(m.visibleIDs.first)
        m.focus(first, extend: false)
        m.updateEdit(label: "ev") { $0.exposure.ev = 1 }
        m.selection = Set(m.visibleIDs)
        let items = m.exportItems
        #expect(items.count == 2)
        #expect(items.first { $0.photoID == first }?.edit?.exposure.ev == 1)
        #expect(items.first { $0.photoID != first }?.edit.map(\.isIdentity) ?? true, "unedited photos: defaults")
        // Same file with and without the edit: the edited export is brighter.
        let url = try #require(items.first { $0.photoID == first }?.url)
        let out = try tempDir("items")
        defer { try? fm.removeItem(at: out) }
        let web = ExportRecipe(name: "Web", sizeMode: .longEdge, sizeValue: 400, fileNameTemplate: "{seq}")
        let o = try await RustExportService().export(ExportJob(
            items: [ExportJobItem(photoID: first, url: url, seq: 1, edit: items.first { $0.photoID == first }?.edit),
                    ExportJobItem(photoID: first, url: url, seq: 2)],
            recipes: [web], defaultFolder: out)) { _, _ in }
        #expect(o.failures.isEmpty && o.written.count == 2)
        let edited = try #require(ex8MeanLuma(out.appendingPathComponent("1.jpg")))
        let plain = try #require(ex8MeanLuma(out.appendingPathComponent("2.jpg")))
        #expect(edited > plain * 1.2, "\(plain) → \(edited)")
    }
}

/// Mean luminance (0…255) of the left and right halves of an image file.
func ex8HalfLumas(_ url: URL) -> (Double, Double)? {
    guard let src = CGImageSourceCreateWithURL(url as CFURL, nil),
          let img = CGImageSourceCreateImageAtIndex(src, 0, nil) else { return nil }
    let (w, h) = (img.width, img.height)
    var px = [UInt8](repeating: 0, count: w * h)
    guard let ctx = CGContext(data: &px, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w,
                              space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue)
    else { return nil }
    ctx.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
    var (l, r) = (0, 0)
    for y in 0..<h {
        for x in 0..<w where x < w * 2 / 5 || x >= w * 3 / 5 {
            if x < w / 2 { l += Int(px[y * w + x]) } else { r += Int(px[y * w + x]) }
        }
    }
    let n = Double(h * (w * 2 / 5))
    return (Double(l) / n, Double(r) / Double(h * (w - w * 3 / 5)))
}

/// What a mask provider was asked for.
final class Ex8Asked: @unchecked Sendable {
    private let lock = NSLock()
    private var value: [PhotoID: Set<AITarget>] = [:]
    var calls: [PhotoID: Set<AITarget>] { lock.withLock { value } }
    func add(_ id: PhotoID, _ t: Set<AITarget>) { lock.withLock { value[id, default: []].formUnion(t) } }
}
