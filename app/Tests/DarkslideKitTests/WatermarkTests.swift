import CoreGraphics
import CoreText
import DarkslideFFI
import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers
@testable import DarkslideKit

// Export watermarks: the saved recipe field, the Core Text renderer, the FFI
// spec, the editor's live preview and the end-to-end export.

private func tempDir() throws -> URL {
    let d = FileManager.default.temporaryDirectory.appendingPathComponent("ds-wm-\(UUID())", isDirectory: true)
    try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

/// A solid sRGB image.
private func solid(_ w: Int, _ h: Int, gray: CGFloat) -> CGImage {
    let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpace(name: CGColorSpace.sRGB)!,
                        bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
    ctx.setFillColor(CGColor(gray: gray, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
    return ctx.makeImage()!
}

/// RGBA8 sRGB pixels of `image`, top row first.
private func pixels(_ image: CGImage) -> (w: Int, data: [UInt8]) {
    let (w, h) = (image.width, image.height)
    var data = [UInt8](repeating: 0, count: w * h * 4)
    data.withUnsafeMutableBytes { buf in
        let ctx = CGContext(data: buf.baseAddress, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
    }
    return (w, data)
}

private func red(_ p: (w: Int, data: [UInt8]), _ x: Int, _ y: Int) -> UInt8 { p.data[(y * p.w + x) * 4] }

@Suite struct WatermarkRecipeTests {
    /// A recipe saved before watermarks loads without one, and a recipe without one
    /// saves exactly as before (no `watermark` key).
    @Test func recipesWithoutAWatermarkAreUnchanged() throws {
        let r = ExportRecipe(name: "Web", sizeMode: .longEdge, sizeValue: 2048)
        let json = try JSONEncoder().encode(r)
        let object = try #require(try JSONSerialization.jsonObject(with: json) as? [String: Any])
        #expect(object["watermark"] == nil)
        #expect(try JSONDecoder().decode(ExportRecipe.self, from: json) == r)
        // A list saved by an older build (written out by hand: no watermark field).
        let old = """
        [{"id":"6F9619FF-8B86-D011-B42D-00C04FC964FF","name":"Old","enabled":true,"format":"jpeg","quality":90,
          "sizeMode":"original","sizeValue":0,"allowUpscale":false,"space":"sRGB","sharpen":0,
          "fileNameTemplate":"{name}","subfolder":"","includeMetadata":true,"stripGPS":false}]
        """
        let loaded = try JSONDecoder().decode([ExportRecipe].self, from: Data(old.utf8))
        #expect(loaded.count == 1 && loaded[0].watermark == nil && loaded[0].name == "Old")
    }

    @Test @MainActor func watermarksAreSavedWithTheRecipes() throws {
        let wm = ExportWatermark(kind: .logo, text: "© {year}", fontFamily: "Helvetica Neue", textSize: 0.05,
                                 color: 0x123456, shadow: false, logo: URL(fileURLWithPath: "/tmp/logo.png"),
                                 logoScale: 0.3, anchor: .topLeft, inset: 0.01, opacity: 0.4)
        let r = ExportRecipe(name: "Marked", watermark: wm)
        let back = try JSONDecoder().decode(ExportRecipe.self, from: JSONEncoder().encode(r))
        #expect(back.watermark == wm)
        let store = ExportRecipeStore(defaults: testDefaults())
        store.save([r, ExportRecipe(name: "Plain")])
        let loaded = store.load()
        #expect(loaded.map(\.watermark) == [wm, nil])
        #expect(r.summary.hasSuffix("logo watermark"))
        // Fields a later build added decode with their defaults.
        let partial = try JSONDecoder().decode(ExportWatermark.self, from: Data(#"{"kind":"logo"}"#.utf8))
        var want = ExportWatermark()
        want.kind = .logo
        #expect(partial == want)
    }

    @Test func colourWellRoundTripsThroughSRGB() throws {
        var wm = ExportWatermark(color: 0xFF8000)
        let c = try #require(wm.cgColor.components)
        #expect(abs(c[0] - 1) < 1e-6 && abs(c[1] - 128.0 / 255) < 1e-6 && c[2] == 0)
        wm.cgColor = CGColor(srgbRed: 0, green: 0.2, blue: 1, alpha: 1)
        #expect(wm.color == 0x0033FF)
        // A Display P3 colour is converted (P3 white is sRGB white).
        wm.cgColor = CGColor(colorSpace: CGColorSpace(name: CGColorSpace.displayP3)!, components: [1, 1, 1, 1])!
        #expect(wm.color == 0xFFFFFF)
    }

    @Test func specCarriesEveryField() {
        let anchors: [(ExportWatermark.Anchor, WatermarkAnchor)] = [
            (.topLeft, .topLeft), (.top, .top), (.topRight, .topRight), (.left, .left), (.center, .center),
            (.right, .right), (.bottomLeft, .bottomLeft), (.bottom, .bottom), (.bottomRight, .bottomRight),
        ]
        #expect(anchors.count == ExportWatermark.Anchor.allCases.count)
        for (a, want) in anchors {
            #expect(WatermarkPreview.spec(ExportWatermark(anchor: a)).anchor == want)
        }
        let text = WatermarkPreview.spec(ExportWatermark(text: "{filename}", fontFamily: "Avenir", textSize: 0.05,
                                                         color: 0xABCDEF, shadow: false, inset: 0.02, opacity: 0.5))
        #expect(text.content == .text(text: "{filename}", font: "Avenir", size: 0.05, color: 0xABCDEF, shadow: false))
        #expect(text.inset == 0.02 && text.opacity == 0.5)
        let logo = WatermarkPreview.spec(ExportWatermark(kind: .logo, logo: URL(fileURLWithPath: "/a/b.png"), logoScale: 0.25))
        #expect(logo.content == .logo(path: "/a/b.png", scale: 0.25))
        #expect(WatermarkPreview.spec(ExportWatermark(kind: .logo)).content == .logo(path: "", scale: 0.15))
        let spec = RustExportService.spec(ExportRecipe(name: "R", watermark: ExportWatermark()),
                                          defaultFolder: URL(fileURLWithPath: "/tmp/out"))
        #expect(spec.watermark == WatermarkPreview.spec(ExportWatermark()))
        #expect(RustExportService.spec(ExportRecipe(name: "R"), defaultFolder: URL(fileURLWithPath: "/tmp")).watermark == nil)
    }

    /// A recipe naming a font this Mac lacks says so instead of exporting in the system font.
    @Test func aMissingFontIsReported() {
        let s = RustExportService()
        let folder = URL(fileURLWithPath: "/tmp/out")
        let missing = ExportRecipe(name: "Web", watermark: ExportWatermark(fontFamily: "No Such Font Family"))
        #expect(s.validate([missing], defaultFolder: folder)
            == "Web: the watermark font No Such Font Family isn't installed on this Mac.")
        let ok = ExportRecipe(name: "Web", watermark: ExportWatermark(fontFamily: "Helvetica"))
        #expect(s.validate([ok], defaultFolder: folder) == nil)
        // A logo recipe's leftover font name doesn't matter.
        let logo = ExportRecipe(name: "Web", watermark: ExportWatermark(kind: .logo, fontFamily: "No Such Font Family"))
        #expect(s.validate([logo], defaultFolder: folder)?.contains("font") != true)
    }

    @Test func aLogoMustBeChosen() {
        let s = RustExportService()
        let folder = URL(fileURLWithPath: "/tmp/out")
        let msg = s.validate([ExportRecipe(name: "Web", watermark: ExportWatermark(kind: .logo))], defaultFolder: folder)
        #expect(msg?.contains("Web: choose a PNG file for the watermark logo") == true, "\(msg ?? "nil")")
        #expect(s.validate([ExportRecipe(name: "Web", watermark: ExportWatermark())], defaultFolder: folder) == nil)
    }
}

@Suite struct CoreTextWatermarkTests {
    let r = CoreTextWatermarkRenderer()

    @Test func rendersOneUprightLineAsCoverage() throws {
        let m = try r.rasterize(text: "T", font: "Helvetica", pixelSize: 100)
        let font = CoreTextWatermarkRenderer.font(family: "Helvetica", size: 100)
        // The typographic box (ascent + descent), snapped outward to whole pixels.
        let typographic = CTFontGetAscent(font) + CTFontGetDescent(font)
        #expect(Double(m.height) >= typographic && Double(m.height) <= typographic.rounded(.up) + 1)
        #expect(m.width > 20 && m.coverage.count == Int(m.width * m.height))
        let bytes = [UInt8](m.coverage)
        #expect(bytes.contains(255) && bytes.contains(0))
        // The T's bar is near the top: the row with the most ink is in the upper half.
        let rows = (0..<Int(m.height)).map { y in bytes[y * Int(m.width)..<(y + 1) * Int(m.width)].reduce(0) { $0 + Int($1) } }
        let peak = try #require(rows.indices.max { rows[$0] < rows[$1] })
        #expect(peak < Int(m.height) / 2, "row \(peak) of \(m.height)")
        // Below the baseline (the descent) a T has no ink.
        let descent = Int(CTFontGetDescent(font))
        #expect(rows.suffix(max(descent - 1, 1)).allSatisfy { $0 == 0 })
    }

    @Test func sizeScalesTheMask() throws {
        let a = try r.rasterize(text: "Alexa", font: "", pixelSize: 20)
        let b = try r.rasterize(text: "Alexa", font: "", pixelSize: 80)
        #expect(abs(Double(b.width) / Double(a.width) - 4) < 0.3)
        #expect(abs(Double(b.height) / Double(a.height) - 4) < 0.3)
    }

    @Test func unknownFamiliesUseTheSystemFont() throws {
        let sys = try r.rasterize(text: "Abc", font: "", pixelSize: 40)
        let unknown = try r.rasterize(text: "Abc", font: "No Such Font Family", pixelSize: 40)
        #expect(unknown == sys)
        let helvetica = try r.rasterize(text: "Abc", font: "Helvetica", pixelSize: 40)
        #expect(CTFontCopyFamilyName(CoreTextWatermarkRenderer.font(family: "helvetica", size: 10)) as String == "Helvetica")
        #expect(helvetica != sys)
    }

    /// Ink outside the typographic box (a script font's swashes) is kept, not clipped.
    @Test func overhangingGlyphsAreNotClipped() throws {
        let font = CoreTextWatermarkRenderer.font(family: "Zapfino", size: 60)
        guard CTFontCopyFamilyName(font) as String == "Zapfino" else { return } // not installed
        let m = try r.rasterize(text: "fJ", font: "Zapfino", pixelSize: 60)
        let typographic = Int((CTFontGetAscent(font) + CTFontGetDescent(font)).rounded(.up))
        let line = CTLineCreateWithAttributedString(NSAttributedString(string: "fJ", attributes: [
            NSAttributedString.Key(kCTFontAttributeName as String): font]))
        let advance = Int(CTLineGetTypographicBounds(line, nil, nil, nil).rounded(.up))
        #expect(Int(m.height) >= typographic && Int(m.width) >= advance)
        #expect(Int(m.width) > advance || Int(m.height) > typographic, "\(m.width)×\(m.height) vs \(advance)×\(typographic)")
    }

    @Test func installedFamiliesAreRecognised() {
        #expect(CoreTextWatermarkRenderer.isInstalled("Helvetica"))
        #expect(CoreTextWatermarkRenderer.isInstalled("helvetica"))
        #expect(!CoreTextWatermarkRenderer.isInstalled("No Such Font Family"))
    }

    @Test func badInputsThrow() {
        #expect(throws: DsError.self) { try r.rasterize(text: "x", font: "", pixelSize: 0) }
        #expect(throws: DsError.self) { try r.rasterize(text: "x", font: "", pixelSize: .nan) }
        #expect(throws: DsError.self) { try r.rasterize(text: "", font: "", pixelSize: 20) }
    }
}

@Suite struct WatermarkPreviewTests {
    @Test func rgbaRoundTripsAndDownscales() throws {
        let big = solid(3000, 1500, gray: 1)
        let (w, h, data) = try #require(WatermarkPreview.rgba(big))
        #expect(w == 1024 && h == 512 && data.count == w * h * 4)
        #expect(data.prefix(4) == Data([255, 255, 255, 255]))
        let back = try #require(WatermarkPreview.image(width: w, height: h, rgba: data))
        #expect(back.width == 1024 && red(pixels(back), 5, 5) == 255)
        #expect(WatermarkPreview.image(width: 2, height: 2, rgba: Data(count: 15)) == nil)
    }

    /// The preview is drawn by ds-export, exactly as the export draws it.
    @Test func previewDrawsTheTextWhereTheExportWould() throws {
        let s = RustExportService()
        let wm = ExportWatermark(text: "{filename}", textSize: 0.2, color: 0xFFFFFF, shadow: false, anchor: .bottomRight,
                                 inset: 0.05, opacity: 1)
        let px = try #require(WatermarkPreview.pixels(solid(400, 200, gray: 0)))
        let r = s.watermarkPreview(px, watermark: wm, sample: ExportFileNameSample(name: "IMG_1"))
        #expect(r.message == nil)
        let out = try #require(r.image)
        let p = pixels(out)
        #expect(out.width == 400 && out.height == 200)
        // 40 px text in the bottom-right corner, 10 px from the edges; the rest stays black.
        let lit: [(Int, Int)] = (0..<200).flatMap { y in (0..<400).filter { red(p, $0, y) > 128 }.map { ($0, y) } }
        #expect(!lit.isEmpty)
        #expect(lit.allSatisfy { $0.0 >= 200 && $0.0 < 390 && $0.1 >= 150 && $0.1 < 190 }, "\(lit.first!)…")
        #expect(red(p, 10, 10) == 0)
    }

    /// A blank line (no copyright) and a preview that can't be drawn both say why.
    @Test func previewExplainsABlankLineAndErrors() throws {
        let s = RustExportService()
        let px = try #require(WatermarkPreview.pixels(solid(40, 20, gray: 0)))
        let blank = s.watermarkPreview(px, watermark: ExportWatermark(text: "{copyright}"),
                                       sample: ExportFileNameSample(name: "A"))
        #expect(blank.image != nil && blank.message == WatermarkPreview.blankMessage)
        // The photo's own EXIF copyright fills it (as the export does).
        let filled = s.watermarkPreview(px, watermark: ExportWatermark(text: "{copyright}"),
                                        sample: ExportFileNameSample(name: "A", copyright: "© Jane"))
        #expect(filled.message == nil)
        let noLogo = s.watermarkPreview(px, watermark: ExportWatermark(kind: .logo), sample: ExportFileNameSample(name: ""))
        #expect(noLogo.image == nil && noLogo.message == "choose a PNG file for the watermark logo")
        let none = UnavailableExportService().watermarkPreview(px, watermark: ExportWatermark(),
                                                               sample: ExportFileNameSample(name: ""))
        #expect(none.image == nil && none.message == nil)
    }
}

/// Records preview draws (on which thread, with which sample).
private final class PreviewFake: ExportService, @unchecked Sendable {
    private let lock = NSLock()
    private var _draws: [(ExportWatermark, ExportFileNameSample, Bool)] = []
    var draws: [(ExportWatermark, ExportFileNameSample, Bool)] { lock.withLock { _draws } }
    var sample: ExportFileNameSample?
    func validate(_ recipes: [ExportRecipe], defaultFolder: URL) -> String? { nil }
    func previewFileName(template: String, sample: ExportFileNameSample, seq: Int, ext: String) -> String? { nil }
    func nameSample(for url: URL) async -> ExportFileNameSample? { sample }
    func export(_ job: ExportJob, progress: @escaping @Sendable (ExportProgressUpdate) -> Void) async throws -> ExportOutcome {
        ExportOutcome(written: [], failures: [], cancelled: false, jpegEncoder: "")
    }
    func watermarkPreview(_ pixels: WatermarkPreviewPixels, watermark: ExportWatermark,
                          sample: ExportFileNameSample) -> WatermarkPreviewResult {
        lock.withLock { _draws.append((watermark, sample, Thread.isMainThread)) }
        return WatermarkPreviewResult(image: WatermarkPreview.image(width: pixels.width, height: pixels.height,
                                                                    rgba: pixels.rgba), message: "drawn")
    }
}

/// Counts loads of the preview photo; `gate` holds them until opened.
private final class LoadCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    private var _open = true
    var open: Bool {
        get { lock.withLock { _open } }
        set { lock.withLock { _open = newValue } }
    }
    var count: Int { lock.withLock { n } }
    func load(_ image: CGImage) async -> CGImage? {
        lock.withLock { n += 1 }
        while !open { try? await Task.sleep(for: .milliseconds(2)) }
        return image
    }
}

@MainActor
@Suite struct WatermarkSheetTests {
    fileprivate func model(_ f: PreviewFake, _ loads: LoadCounter, defaults: UserDefaults = testDefaults(),
               folder: URL? = URL(fileURLWithPath: "/tmp/Out")) -> ExportModel {
        let url = URL(fileURLWithPath: "/tmp/in/DSC07.ARW")
        let image = solid(8, 8, gray: 0.5)
        let m = ExportModel(items: [ExportJobItem(photoID: PhotoID(1), url: url, seq: 1)], defaultFolder: folder,
                            service: f, store: ExportRecipeStore(defaults: defaults),
                            previewSource: WatermarkPreviewSource(url: url) { await loads.load(image) })
        m.previewDebounce = .zero
        return m
    }

    /// Drawn off the main thread and stored; the editor only reads it. The photo is
    /// loaded once, and only when a watermark is previewed.
    @Test func thePreviewIsDrawnOffTheMainThreadAndStored() async {
        let f = PreviewFake()
        let loads = LoadCounter()
        let defaults = testDefaults()
        defaults.set("© Me", forKey: AppSettings.Keys.copyright)
        let m = model(f, loads, defaults: defaults)
        await m.updateWatermarkPreview(ExportRecipe(name: "Plain"))
        #expect(loads.count == 0 && f.draws.isEmpty, "no watermark: nothing loaded or drawn")
        var r = ExportRecipe(name: "R", watermark: ExportWatermark())
        #expect(m.watermarkPreview(for: r.watermark) == nil)
        await m.updateWatermarkPreview(r)
        #expect(m.watermarkPreview(for: r.watermark)?.message == "drawn")
        #expect(m.watermarkPreview(for: r.watermark)?.image?.width == 8)
        #expect(f.draws.count == 1 && f.draws[0].2 == false, "drawn off the main thread")
        #expect(f.draws[0].1.copyright == "© Me" && f.draws[0].1.name == "DSC07")
        r.watermark?.opacity = 0.3
        #expect(m.watermarkPreview(for: r.watermark) == nil, "a stale preview isn't shown for another watermark")
        await m.updateWatermarkPreview(r)
        #expect(f.draws.count == 2 && loads.count == 1, "the photo is loaded once")
        #expect(m.previewPixels?.width == 8)
    }

    /// Settings' credit wins; without it, the photo's own EXIF credit (as the export).
    @Test func thePreviewUsesThePhotosOwnCreditAsFallback() async {
        let f = PreviewFake()
        f.sample = ExportFileNameSample(name: "DSC07", captureTime: "2024-01-02T03:04:05", artist: "Jane",
                                        copyright: "© Jane")
        let m = model(f, LoadCounter())
        await m.updateWatermarkPreview(ExportRecipe(name: "R", watermark: ExportWatermark()))
        #expect(f.draws.first?.1 == f.sample)
    }

    /// An edit during the debounce cancels the earlier draw (`.task(id:)`).
    @Test func editsDuringTheDebounceDrawOnce() async {
        let f = PreviewFake()
        let m = model(f, LoadCounter())
        m.previewDebounce = .seconds(30)
        let first = Task { await m.updateWatermarkPreview(ExportRecipe(name: "R", watermark: ExportWatermark())) }
        first.cancel()
        await first.value
        m.previewDebounce = .zero
        await m.updateWatermarkPreview(ExportRecipe(name: "R", watermark: ExportWatermark(opacity: 0.5)))
        #expect(f.draws.count == 1 && f.draws[0].0.opacity == 0.5)
    }

    /// The folder check doesn't wait for the preview photo (W4).
    @Test func prepareDoesNotWaitForThePreviewPhoto() async {
        let f = PreviewFake()
        let loads = LoadCounter()
        loads.open = false
        let m = model(f, loads, folder: nil)
        m.prepare(session: nil, timeout: 1)
        await m.prepared()
        #expect(m.destinationReady)
        #expect(loads.count == 0)
        let preview = Task { await m.updateWatermarkPreview(ExportRecipe(name: "R", watermark: ExportWatermark())) }
        #expect(await waitUntil { loads.count == 1 })
        loads.open = true
        await preview.value
        #expect(f.draws.count == 1)
    }

    @Test func theSummarySaysWhenWatermarksWereLeftOut() {
        let o = ExportOutcome(written: [], failures: [], cancelled: false, jpegEncoder: "", watermarkSkipped: 3)
        #expect(ExportModel.summary(o).hasSuffix("3 without watermark (blank text)"))
        #expect(!ExportModel.summary(ExportOutcome(written: [], failures: [], cancelled: false, jpegEncoder: ""))
            .contains("watermark"))
    }

    /// The sheet previews on the focused photo (else the first exported one).
    @Test func previewPhotoIsTheFocusedOne() async throws {
        let m = makeModel(store: URLStore())
        await m.start()
        #expect(await waitUntil { m.visibleIDs.count == 6 })
        let ids = m.visibleIDs
        m.focus(ids[3], extend: false)
        let items = [ExportJobItem(photoID: ids[0], url: URL(fileURLWithPath: "/photos/a.ARW"), seq: 1)]
        let source = try #require(m.watermarkPreviewSource(items))
        #expect(source.url == m.photo(ids[3])?.url)
        #expect(await source.load() != nil)
        m.focus(nil, extend: false)
        #expect(m.watermarkPreviewSource(items)?.url == m.photo(ids[0])?.url)
        #expect(m.watermarkPreviewSource([]) == nil)
        m.stop()
    }
}

@Suite(.serialized) struct WatermarkExportTests {
    /// End to end: a JPEG original exported with a text watermark (Core Text through
    /// ds-export) at two sizes: the same relative place and size in both.
    @Test func exportsCarryTheWatermarkAtEverySize() async throws {
        let dir = try tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let src = dir.appendingPathComponent("IMG_9.jpg")
        let dest = try #require(CGImageDestinationCreateWithURL(src as CFURL, UTType.jpeg.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(dest, solid(800, 400, gray: 0), nil)
        #expect(CGImageDestinationFinalize(dest))
        let wm = ExportWatermark(text: "WWWW", textSize: 0.1, color: 0xFFFFFF, shadow: false, anchor: .topLeft,
                                 inset: 0.05, opacity: 1)
        let full = ExportRecipe(name: "Full", format: .png8, fileNameTemplate: "{name}_full", watermark: wm)
        let half = ExportRecipe(name: "Half", format: .png8, sizeMode: .percent, sizeValue: 50,
                                fileNameTemplate: "{name}_half", watermark: wm)
        // `{copyright}` with none in Settings or the file: written without, and counted.
        let blank = ExportRecipe(name: "Blank", format: .png8, fileNameTemplate: "{name}_blank",
                                 watermark: ExportWatermark(text: "{copyright}"))
        let o = try await RustExportService().export(
            ExportJob(items: [ExportJobItem(photoID: PhotoID(1), url: src, seq: 1)], recipes: [full, half, blank],
                      defaultFolder: dir.appendingPathComponent("Out"))) { (_: ExportProgressUpdate) in }
        #expect(o.failures.isEmpty, "\(o.failures)")
        #expect(o.written.count == 3 && o.watermarkSkipped == 1)
        func box(_ name: String) throws -> (CGFloat, CGFloat, CGFloat, CGFloat) {
            let url = dir.appendingPathComponent("Out/IMG_9_\(name).png")
            let img = try #require(CGImageSourceCreateWithURL(url as CFURL, nil).flatMap {
                CGImageSourceCreateImageAtIndex($0, 0, nil)
            })
            let p = pixels(img)
            let lit: [(Int, Int)] = (0..<img.height).flatMap { y in (0..<img.width).filter { red(p, $0, y) > 128 }.map { ($0, y) } }
            let xs = lit.map(\.0), ys = lit.map(\.1)
            let w = CGFloat(img.width)
            return (CGFloat(xs.min() ?? 0) / w, CGFloat(ys.min() ?? 0) / w, CGFloat(xs.max() ?? 0) / w,
                    CGFloat(ys.max() ?? 0) / w)
        }
        let a = try box("full"), b = try box("half")
        #expect(a.2 > a.0 && a.3 > a.1, "text drawn: \(a)")
        #expect(a.0 >= 0.05 * 400 / 800 - 0.01, "inside the margin: \(a)")
        for (x, y) in [(a.0, b.0), (a.1, b.1), (a.2, b.2), (a.3, b.3)] {
            #expect(abs(x - y) < 0.01, "full \(a) vs half \(b)")
        }
    }
}

private func writePNG(_ url: URL, gray: CGFloat) throws {
    let dest = try #require(CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil))
    CGImageDestinationAddImage(dest, solid(4, 4, gray: gray), nil)
    #expect(CGImageDestinationFinalize(dest))
}

@Suite struct WatermarkLogoImportTests {
    /// The recipe keeps its own copy: moving or deleting the original PNG doesn't break it.
    @Test func aChosenLogoIsCopiedAndSurvivesTheOriginalMoving() throws {
        let dir = try tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let original = dir.appendingPathComponent("Desktop/logo.png")
        try FileManager.default.createDirectory(at: original.deletingLastPathComponent(), withIntermediateDirectories: true)
        try writePNG(original, gray: 1)
        let store = dir.appendingPathComponent("Support/Watermarks")
        let copy = try WatermarkLogos.importLogo(original, into: store)
        #expect(copy.deletingLastPathComponent().standardizedFileURL == store.standardizedFileURL)
        #expect(copy.lastPathComponent.hasPrefix("logo-") && copy.pathExtension == "png")
        #expect(try Data(contentsOf: copy) == Data(contentsOf: original))
        let recipe = ExportRecipe(name: "Web", watermark: ExportWatermark(kind: .logo, logo: copy))
        try FileManager.default.moveItem(at: original, to: dir.appendingPathComponent("moved.png"))
        #expect(RustExportService().validate([recipe], defaultFolder: dir) == nil, "the copy is still there")
        // The store holds just the logo: no temp file is left behind.
        #expect(try FileManager.default.contentsOfDirectory(atPath: store.path) == [copy.lastPathComponent])
    }

    @Test func theSameLogoIsStoredOnceAndADifferentOneNeverReplacesIt() throws {
        let dir = try tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = dir.appendingPathComponent("W")
        let a = dir.appendingPathComponent("logo.png")
        try writePNG(a, gray: 1)
        let first = try WatermarkLogos.importLogo(a, into: store)
        #expect(try WatermarkLogos.importLogo(a, into: store) == first)
        // A different logo under the same name gets its own copy; the first is untouched.
        let before = try Data(contentsOf: first)
        try writePNG(a, gray: 0)
        let second = try WatermarkLogos.importLogo(a, into: store)
        #expect(second != first)
        #expect(try Data(contentsOf: first) == before)
        #expect(try FileManager.default.contentsOfDirectory(atPath: store.path).count == 2)
    }

    @Test func onlyPNGsAreImported() throws {
        let dir = try tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let jpg = dir.appendingPathComponent("logo.png")
        try Data("not a png".utf8).write(to: jpg)
        #expect(throws: WatermarkLogos.ImportError.notPNG("logo.png")) {
            try WatermarkLogos.importLogo(jpg, into: dir.appendingPathComponent("W"))
        }
        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent("W").path))
        #expect(throws: (any Error).self) {
            try WatermarkLogos.importLogo(dir.appendingPathComponent("none.png"), into: dir)
        }
    }

    /// Copies no recipe uses any more are deleted; used ones and other files stay.
    @Test func unusedCopiesArePruned() throws {
        let dir = try tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = dir.appendingPathComponent("W")
        let a = dir.appendingPathComponent("a.png"), b = dir.appendingPathComponent("b.png")
        try writePNG(a, gray: 1)
        try writePNG(b, gray: 0)
        let ca = try WatermarkLogos.importLogo(a, into: store)
        let cb = try WatermarkLogos.importLogo(b, into: store)
        try Data("note".utf8).write(to: store.appendingPathComponent("README.txt"))
        let recipes = [ExportRecipe(name: "R", watermark: ExportWatermark(kind: .logo, logo: ca)), ExportRecipe(name: "P")]
        WatermarkLogos.prune(keeping: recipes, in: store)
        let left = Set(try FileManager.default.contentsOfDirectory(atPath: store.path))
        #expect(left == [ca.lastPathComponent, "README.txt"])
        #expect(!FileManager.default.fileExists(atPath: cb.path))
        #expect(FileManager.default.fileExists(atPath: a.path) && FileManager.default.fileExists(atPath: b.path),
                "originals untouched")
        WatermarkLogos.prune(keeping: [], in: dir.appendingPathComponent("missing")) // no folder: nothing to do
    }

    /// The store lives under the app's support folder (`DS_SUPPORT_ROOT` in tests).
    @Test func theStoreIsInTheSupportFolder() {
        #expect(WatermarkLogos.folder == AppPaths.support.appendingPathComponent("Watermarks", isDirectory: true))
        #expect(Set(ExportWatermark.Anchor.allCases.map(\.title)).count == 9)
        #expect(ExportWatermark.Kind.allCases.map(\.title) == ["Text", "Logo"])
    }
}
