import CoreGraphics
import DarkslideFFI
import Foundation
import ImageIO
import Testing
@testable import DarkslideKit

// MARK: - Helpers

private var fm: FileManager { .default }

private func ms(_ d: Duration) -> Double {
    Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15
}

private final class Exp6Box<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ v: T) { value = v }
    func withLock<R>(_ f: (inout T) -> R) -> R { lock.withLock { f(&value) } }
}

private func tempDir(_ tag: String) throws -> URL {
    let d = fm.temporaryDirectory.appendingPathComponent("ds-exp6-\(tag)-\(UUID())", isDirectory: true)
    try fm.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

/// Every file below `dir`, relative, sorted.
private func tree(_ dir: URL) -> [String] {
    let base = dir.resolvingSymlinksInPath().path
    return (fm.enumerator(at: dir, includingPropertiesForKeys: [.isRegularFileKey])?.allObjects as? [URL] ?? [])
        .filter { (try? $0.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile) == true }
        .map { String($0.resolvingSymlinksInPath().path.dropFirst(base.count + 1)) }
        .sorted()
}

private func properties(_ url: URL) -> [CFString: Any]? {
    CGImageSourceCreateWithURL(url as CFURL, nil).flatMap { CGImageSourceCopyPropertiesAtIndex($0, 0, nil) } as? [CFString: Any]
}

private func decoded(_ url: URL) -> CGImage? {
    CGImageSourceCreateWithURL(url as CFURL, nil).flatMap { CGImageSourceCreateImageAtIndex($0, 0, nil) }
}

private func typeOf(_ url: URL) -> String? {
    CGImageSourceCreateWithURL(url as CFURL, nil).flatMap { CGImageSourceGetType($0) as String? }
}

/// Size of the RAW's full develop with default adjustments (what a full-size export writes).
private func developedSize(_ url: URL) async throws -> (Int, Int) {
    try await ex8DevelopedSize(url, edit: nil)
}

private func items(_ urls: [URL]) -> [ExportJobItem] {
    urls.enumerated().map { ExportJobItem(photoID: PhotoID(Int64($0.offset + 1)), url: $0.element, seq: $0.offset + 1) }
}

/// An ExportService double for the model logic.
final class Exp6FakeExport: ExportService, @unchecked Sendable {
    var problem: String?
    var fail: Error?
    var delay: Duration = .zero
    /// While set, the run pauses after reporting the first photo (deterministic "running").
    var hold = false
    var outcome = ExportOutcome(written: [], failures: [], cancelled: false, jpegEncoder: "imageio")
    private(set) var jobs: [ExportJob] = []
    /// What `nameSample` answers.
    var sample: ExportFileNameSample?
    /// Metadata-phase updates sent before the export ones.
    var metadataSteps = 0
    func validate(_ recipes: [ExportRecipe], defaultFolder: URL) -> String? { problem }
    func previewFileName(template: String, sample: ExportFileNameSample, seq: Int, ext: String) -> String? {
        "\(sample.name)-\(seq)\(sample.captureTime.map { "-" + $0 } ?? "").\(ext)"
    }
    func nameSample(for url: URL) async -> ExportFileNameSample? { sample }
    func export(_ job: ExportJob, progress: @escaping @Sendable (ExportProgressUpdate) -> Void) async throws -> ExportOutcome {
        jobs.append(job)
        for i in 0..<metadataSteps {
            progress(ExportProgressUpdate(done: i + 1, total: job.items.count, readingMetadata: true))
            try await Task.sleep(for: delay)
            while hold, i == 0 { try await Task.sleep(for: .milliseconds(5)) }
        }
        for i in 0...job.items.count {
            progress(ExportProgressUpdate(done: i, total: job.items.count))
            try await Task.sleep(for: delay)
            while hold, i == 1 { try await Task.sleep(for: .milliseconds(5)) }
        }
        if let fail { throw fail }
        return outcome
    }
}

// MARK: - Recipes, destination, store

@Suite struct Exp6RecipeTests {
    @Test func directorySubfolderAndSummary() {
        let base = URL(fileURLWithPath: "/tmp/Out")
        var r = ExportRecipe(name: "Web", format: .jpeg, quality: 82, sizeMode: .longEdge, sizeValue: 2048,
                             subfolder: "Web/../x/./2026", stripGPS: true)
        #expect(r.directory(defaultFolder: base).path == "/tmp/Out/Web/x/2026", "no .. or . escape")
        #expect(r.summary == "JPEG 82 · 2048 px · sRGB · no GPS")
        r.destination = URL(fileURLWithPath: "/tmp/Elsewhere")
        r.subfolder = ""
        #expect(r.directory(defaultFolder: base).path == "/tmp/Elsewhere")
        let t = ExportRecipe(name: "T", format: .tiff16, sizeMode: .megapixels, sizeValue: 12, space: .displayP3,
                             includeMetadata: false)
        #expect(t.summary == "TIFF 16-bit · 12 MP · Display P3 · no metadata")
        let p = ExportRecipe(name: "P", format: .png8, sizeMode: .percent, sizeValue: 50)
        #expect(p.summary == "PNG 8-bit · 50 % · sRGB")
        #expect(ExportRecipe.Format.allCases.map(\.fileExtension) == ["jpg", "heic", "tif", "png", "png"])
        #expect(ExportRecipe.SizeMode.allCases.map(\.title).count == 5)
        #expect(ExportRecipe.defaults.filter(\.enabled).count == 1)
    }

    @MainActor @Test func savedRecipesRoundTripAndBadDataFallsBack() {
        let d = testDefaults()
        let store = ExportRecipeStore(defaults: d)
        #expect(store.load() == ExportRecipe.defaults)
        var mine = ExportRecipe.defaults
        mine[0].name = "Mine"
        mine[0].destination = URL(fileURLWithPath: "/tmp/x")
        store.save(mine)
        #expect(ExportRecipeStore(defaults: d).load() == mine)
        d.set(Data("junk".utf8), forKey: ExportRecipeStore.key)
        #expect(store.load() == ExportRecipe.defaults)
        store.save([])
        #expect(store.load() == ExportRecipe.defaults, "an empty list falls back too")
    }

    @Test func defaultDestinationIsTheSessionOutputOrAPicturesFolder() async throws {
        let home = URL(fileURLWithPath: "/Users/someone")
        #expect(await ExportDestination.resolve(for: nil, home: home).path == "/Users/someone/Pictures/Darkslide Exports")
        let dir = try tempDir("dest")
        defer { try? fm.removeItem(at: dir) }
        let s = SessionInfo(name: "Trip", url: dir, photoCount: 1, createdAt: .now)
        #expect(await ExportDestination.resolve(for: s, home: home) == dir.appendingPathComponent("Output", isDirectory: true))
        // A folder we can't write (like a card or a read-only share): a Pictures sub-folder.
        try fm.setAttributes([.posixPermissions: 0o555], ofItemAtPath: dir.path)
        defer { try? fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: dir.path) }
        #expect(await ExportDestination.resolve(for: s, home: home).path == "/Users/someone/Pictures/Darkslide Exports/Trip")
        // A folder that doesn't exist (can't be inspected) falls back too.
        let gone = SessionInfo(name: "Gone", url: dir.appendingPathComponent("nope"), photoCount: 0, createdAt: .now)
        #expect(await ExportDestination.resolve(for: gone, home: home).lastPathComponent == "Gone")
    }

    @Test func recipesConvertToTheFFISpec() {
        let out = URL(fileURLWithPath: "/tmp/Out")
        let cases: [(ExportRecipe, ExportFileFormat, ExportSize)] = [
            (ExportRecipe(name: "a", format: .jpeg, quality: 91), .jpeg(quality: 91), .original),
            (ExportRecipe(name: "b", format: .heic, quality: 300, sizeMode: .longEdge, sizeValue: 2047.6),
             .heic(quality: 255), .longEdge(pixels: 2048)),
            (ExportRecipe(name: "c", format: .tiff16, sizeMode: .shortEdge, sizeValue: 800), .tiff16, .shortEdge(pixels: 800)),
            (ExportRecipe(name: "d", format: .png8, sizeMode: .megapixels, sizeValue: 12), .png8, .megapixels(megapixels: 12)),
            (ExportRecipe(name: "e", format: .png16, sizeMode: .percent, sizeValue: 50), .png16, .percent(percent: 50)),
        ]
        for (r, f, s) in cases {
            let spec = RustExportService.spec(r, defaultFolder: out)
            #expect(spec.format == f && spec.size == s, "\(r.name)")
            #expect(spec.destination == "/tmp/Out" && spec.space == .srgb && spec.includeExif && !spec.stripGps)
        }
        let p3 = RustExportService.spec(ExportRecipe(name: "p", space: .displayP3, sharpen: 0.5, subfolder: "Web",
                                                     includeMetadata: false, stripGPS: true), defaultFolder: out)
        #expect(p3.space == .displayP3 && p3.sharpen == 0.5 && p3.destination == "/tmp/Out/Web" && !p3.includeExif && p3.stripGps)
        #expect(RustExportService.plain("invalid argument: Web: zero edge") == "Web: zero edge")
        #expect(RustExportService.plain("other") == "other")
    }

    @Test func liveValidationAndFileNamePreview() {
        let s = RustExportService()
        let out = URL(fileURLWithPath: "/tmp/Out")
        #expect(s.validate([ExportRecipe(name: "ok")], defaultFolder: out) == nil)
        #expect(s.validate([], defaultFolder: out) == "Choose at least one recipe.")
        let bad = s.validate([ExportRecipe(name: "Web", fileNameTemplate: "{nope}")], defaultFolder: out)
        #expect(bad?.hasPrefix("Web: file name") == true, "\(bad ?? "nil")")
        #expect(s.validate([ExportRecipe(name: "Q", format: .jpeg, quality: 0)], defaultFolder: out) != nil)
        let a9 = ExportFileNameSample(name: "DSC1", camera: "ILCE-9", captureTime: "2026-08-08T17:25:40")
        #expect(s.previewFileName(template: "{name}_{seq:3}", sample: a9, seq: 7, ext: "jpg") == "DSC1_007.jpg")
        #expect(s.previewFileName(template: "{camera}-{name}", sample: a9, seq: 1, ext: "heic") == "ILCE-9-DSC1.heic")
        #expect(s.previewFileName(template: "{date}_{seq:2}", sample: a9, seq: 3, ext: "jpg") == "20260808_03.jpg")
        #expect(s.previewFileName(template: "{oops", sample: a9, seq: 1, ext: "jpg") == nil)
        let demo = UnavailableExportService()
        #expect(demo.validate([], defaultFolder: out)?.contains("live mode") == true)
        #expect(demo.previewFileName(template: "{name}", sample: a9, seq: 1, ext: "jpg") == nil)
    }
}

// MARK: - ExportModel (sheet logic)

@MainActor
@Suite struct Exp6ModelTests {
    func model(_ service: Exp6FakeExport = Exp6FakeExport(), n: Int = 3) -> ExportModel {
        let urls = (1...max(n, 1)).map { URL(fileURLWithPath: "/tmp/in/IMG\($0).ARW") }
        return ExportModel(items: n == 0 ? [] : items(urls), defaultFolder: URL(fileURLWithPath: "/tmp/Out"),
                           service: service, store: ExportRecipeStore(defaults: testDefaults()))
    }

    @Test func recipeListEditsPersistAndNamesStayUnique() {
        let d = testDefaults()
        let m = ExportModel(items: [], defaultFolder: URL(fileURLWithPath: "/tmp"), service: Exp6FakeExport(),
                            store: ExportRecipeStore(defaults: d))
        #expect(m.recipes == ExportRecipe.defaults && m.selectedRecipeID == m.recipes[0].id)
        m.addRecipe()
        m.addRecipe()
        #expect(m.recipes.suffix(2).map(\.name) == ["New Recipe", "New Recipe 2"])
        m.duplicateSelected()
        #expect(m.recipes.last?.name == "New Recipe 2 copy" && m.selectedRecipe?.name == "New Recipe 2 copy")
        var r = m.selectedRecipe!
        r.quality = 55
        m.selectedRecipe = r
        #expect(m.recipes.last?.quality == 55)
        m.selectedRecipe = nil // ignored
        #expect(ExportRecipeStore(defaults: d).load() == m.recipes, "every edit is saved")
        let id = m.selectedRecipeID!
        m.removeRecipe(id)
        #expect(!m.recipes.contains { $0.id == id } && m.selectedRecipeID == m.recipes.last?.id)
        while m.recipes.count > 1 { m.removeRecipe(m.recipes[0].id) }
        m.removeRecipe(m.recipes[0].id)
        #expect(m.recipes.count == 1, "the last recipe stays")
        m.selectedRecipeID = nil
        m.duplicateSelected()
        #expect(m.recipes.count == 1)
    }

    @Test func problemsGateTheExportButton() {
        let f = Exp6FakeExport()
        #expect(model(f, n: 0).problem == "Select photos to export.")
        let m = model(f)
        #expect(m.problem == nil && m.canStart && m.outputCount == 3)
        for i in m.recipes.indices { m.recipes[i].enabled = false }
        #expect(m.problem == "Turn on at least one recipe." && !m.canStart)
        m.recipes[0].enabled = true
        f.problem = "Web: zero edge"
        #expect(m.problem == "Web: zero edge" && !m.canStart)
        #expect(m.sampleFileName(m.recipes[0]) == "IMG1-1.jpg", "file name until the sample is read")
        #expect(model(f, n: 0).sampleFileName(m.recipes[0]) == nil)
    }

    @Test func runReportsProgressThenTheOutcome() async {
        let f = Exp6FakeExport()
        f.hold = true
        let out = URL(fileURLWithPath: "/tmp/Out/IMG1.jpg")
        f.outcome = ExportOutcome(written: [ExportedOutput(photoID: PhotoID(1), recipeName: "Full", url: out,
                                                           width: 10, height: 8, bytes: 2_048)],
                                  failures: ["IMG2 · Full: disk full"], cancelled: false, jpegEncoder: "imageio")
        let m = model(f)
        var revealed: [URL] = []
        m.reveal = { revealed = $0 }
        m.start()
        #expect(m.isRunning && !m.canStart)
        m.start() // ignored while running
        #expect(await waitUntil(timeout: .seconds(20)) { if case .running(1, 3) = m.phase { true } else { false } })
        m.reset() // ignored while running
        #expect(m.isRunning)
        f.hold = false
        await m.waitUntilDone()
        #expect(m.phase == .finished(f.outcome))
        #expect(f.jobs.count == 1 && f.jobs[0].recipes == m.enabledRecipes && f.jobs[0].items.count == 3)
        #expect(ExportModel.summary(f.outcome) == "1 file · 2 KB · 1 failed")
        m.showInFinder([])
        #expect(revealed.isEmpty)
        m.showInFinder([out])
        #expect(revealed == [out])
        m.reset()
        #expect(m.phase == .editing && m.startedAt != nil)
    }

    @Test func cancelAndFailureEndTheRun() async {
        let f = Exp6FakeExport()
        f.delay = .seconds(5)
        let m = model(f)
        m.start()
        m.cancel()
        await m.waitUntilDone()
        guard case .finished(let o) = m.phase else { Issue.record("\(m.phase)"); return }
        #expect(o.cancelled && ExportModel.summary(o) == "0 files · Zero KB · cancelled")
        let g = Exp6FakeExport()
        g.fail = RenderError(.Io(message: "disk gone"))
        let n = model(g)
        n.start()
        await n.waitUntilDone()
        #expect(n.phase == .failed("disk gone"))
    }
}

// MARK: - App wiring

@MainActor
@Suite(.serialized) struct Exp6AppTests {
    @Test func exportCommandIsMenuOnlyAndNeedsFiles() async {
        let m = makeModel()
        await m.start()
        #expect(MenuCommand.exportSelected.shortcut == .init(key: "e", modifiers: [.command]))
        #expect(CommandCatalog.title(.exportSelected, model: m) == "Export…")
        // Demo photos have no files: disabled, and ⌘E says why.
        #expect(!CommandCatalog.isEnabled(.exportSelected, model: m))
        CommandCatalog.perform(.exportSelected, model: m)
        #expect(m.sheet == nil && m.exportModel == nil && m.toast?.message == "Select photos to export")
        m.selection = Set(m.visibleIDs.prefix(3))
        #expect(CommandCatalog.title(.exportSelected, model: m) == "Export 3 Photos…")
        // Plain E stays the viewer key (no ⌘E conflict in the key router).
        #expect(KeyboardRouter.command(for: KeyInput("e", command: true)) == nil)
    }

    /// Live: a real folder session → ⌘E → sheet model with the selection, default
    /// Output/ in the folder → export through the FFI → files there.
    @Test func liveExportFromTheSelection() async throws {
        guard let dir = try sampleFolderCopy(limit: 3) else { return }
        defer { try? fm.removeItem(at: dir) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()),
                                     storeBase: fm.temporaryDirectory.appendingPathComponent("ds-exp6-store-\(UUID())"))
        let info = try await store.add(folder: dir)
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        m.exporter = RustExportService()
        await m.open(info)
        #expect(await waitUntil(timeout: .seconds(10)) { m.visibleIDs.count == 3 })
        m.selection = Set(m.visibleIDs)
        #expect(CommandCatalog.isEnabled(.exportSelected, model: m))
        let before = tree(dir)
        m.beginExport(store: ExportRecipeStore(defaults: testDefaults()))
        let e = try #require(m.exportModel)
        #expect(e.problem == "Checking the export folder…" || e.destinationReady)
        await e.prepared()
        #expect(m.sheet == .export && e.items.count == 3 && e.items.map(\.seq) == [1, 2, 3])
        #expect(e.defaultFolder.standardizedFileURL.path == dir.appendingPathComponent("Output").standardizedFileURL.path)
        e.recipes = [ExportRecipe(name: "Web", format: .jpeg, quality: 80, sizeMode: .longEdge, sizeValue: 600,
                                  fileNameTemplate: "{seq:2}_{name}")]
        e.start()
        await e.waitUntilDone()
        guard case .finished(let o) = e.phase else { Issue.record("\(e.phase)"); return }
        #expect(o.written.count == 3 && o.failures.isEmpty && o.renderer.hasPrefix("develop ("), "\(o.renderer)")
        let added = Set(tree(dir)).subtracting(before).filter { !$0.hasPrefix(".ds/") }
        #expect(added == Set(m.visibleIDs.enumerated().map { i, id in
            String(format: "Output/%02d_%@.jpg", i + 1, (m.photo(id)!.filename as NSString).deletingPathExtension)
        }), "\(added)")
        m.endExport()
        #expect(m.exportModel == nil && m.sheet == nil)
    }
}

// MARK: - Live export through the FFI (real samples, ImageIO encoders)

@Suite(.serialized) struct Exp6LiveExportTests {
    @Test func jpegHeicAndPngFromRealSamples() async throws {
        let raws = sampleARWs().prefix(2)
        guard raws.count == 2 else { return }
        let root = try tempDir("live")
        defer { try? fm.removeItem(at: root) }
        let src = root.appendingPathComponent("src")
        try fm.createDirectory(at: src, withIntermediateDirectories: true)
        let urls = try raws.map { u -> URL in
            let d = src.appendingPathComponent(u.lastPathComponent)
            try fm.copyItem(at: u, to: d)
            return d
        }
        let out = root.appendingPathComponent("Output")
        let recipes = [
            ExportRecipe(name: "Full", format: .jpeg, quality: 90, fileNameTemplate: "{camera}_{name}"),
            ExportRecipe(name: "HEIC", format: .heic, quality: 80, sizeMode: .longEdge, sizeValue: 1000,
                         space: .displayP3, subfolder: "HEIC"),
            ExportRecipe(name: "PNG", format: .png8, sizeMode: .percent, sizeValue: 50, includeMetadata: false),
        ]
        let seen = Exp6Box<[Int]>([])
        let o = try await RustExportService(hardwareJPEG: true).export(ExportJob(items: items(urls), recipes: recipes, defaultFolder: out)) { d, _ in
            seen.withLock { $0.append(d) }
        }
        #expect(o.failures.isEmpty, "\(o.failures)")
        #expect(o.written.count == 6 && !o.cancelled && o.renderer.hasPrefix("develop (") && o.jpegEncoder == "imageio")
        #expect(seen.withLock { $0.last } == 2)
        let names = urls.map { $0.deletingPathExtension().lastPathComponent }
        var expected: [String] = names.map { "Output/HEIC/\($0).heic" }
        expected += names.map { "Output/ILCE-9_\($0).jpg" }
        expected += names.map { "Output/\($0).png" }
        expected += urls.map { "src/\($0.lastPathComponent)" }
        #expect(tree(root) == expected.sorted(), "nothing outside Output/, no temp files")
        for w in o.written {
            let (pw, ph) = try await developedSize(w.url.deletingLastPathComponent().lastPathComponent == "HEIC"
                ? urls[names.firstIndex(of: w.url.deletingPathExtension().lastPathComponent)!]
                : urls[names.firstIndex { w.url.lastPathComponent.contains($0) }!])
            let img = try #require(decoded(w.url), "\(w.url.lastPathComponent) decodes")
            #expect(img.width == w.width && img.height == w.height)
            let props = try #require(properties(w.url))
            switch w.recipeName {
            case "Full":
                #expect(typeOf(w.url) == "public.jpeg")
                #expect((w.width, w.height) == (pw, ph), "the full develop, upright")
                let tiff = props[kCGImagePropertyTIFFDictionary] as? [CFString: Any]
                #expect(tiff?[kCGImagePropertyTIFFModel] as? String == "ILCE-9")
                #expect((props[kCGImagePropertyExifDictionary] as? [CFString: Any])?[kCGImagePropertyExifDateTimeOriginal] != nil)
                #expect((props[kCGImagePropertyOrientation] as? Int ?? 1) == 1)
            case "HEIC":
                #expect(typeOf(w.url) == "public.heic")
                #expect(max(w.width, w.height) == 1000 && (w.width > w.height) == (pw > ph))
                #expect((props[kCGImagePropertyProfileName] as? String)?.contains("P3") == true, "\(props[kCGImagePropertyProfileName] ?? "none")")
                #expect((props[kCGImagePropertyTIFFDictionary] as? [CFString: Any])?[kCGImagePropertyTIFFModel] as? String == "ILCE-9")
            default:
                #expect(typeOf(w.url) == "public.png")
                #expect(w.width == (pw + 1) / 2 || w.width == pw / 2)
                #expect((props[kCGImagePropertyTIFFDictionary] as? [CFString: Any])?[kCGImagePropertyTIFFModel] == nil,
                        "no metadata")
            }
        }
        // mozjpeg by default (faster at equal size and PSNR, see the benchmark).
        let m = try await RustExportService().export(
            ExportJob(items: items(Array(urls.prefix(1))), recipes: [recipes[0]], defaultFolder: root.appendingPathComponent("moz"))) { _, _ in }
        #expect(m.jpegEncoder == "mozjpeg" && m.written.count == 1)
    }

    /// GPS through the app's encoder: kept by default, gone with "Remove location".
    @Test func gpsIsKeptOrStrippedByTheImageIOEncoders() async throws {
        guard let raw = sampleARWs().first else { return }
        let root = try tempDir("gps")
        defer { try? fm.removeItem(at: root) }
        func recipe(_ name: String, _ format: ExportFileFormat, strip: Bool) -> ExportRecipeSpec {
            ExportRecipeSpec(name: name, format: format, size: .longEdge(pixels: 400), allowUpscale: false, space: .srgb,
                             sharpen: 0, fileNameTemplate: "\(name)", destination: root.path, includeExif: true,
                             stripGps: strip)
        }
        let report = try await exportBatch(
            sources: [ExportSource(id: 1, path: raw.path, seq: 1, gps: ExportGps(latitude: 20.75, longitude: -156.25, altitudeM: 30),
                                   artist: "Alexa", copyright: nil)],
            recipes: [recipe("jk", .jpeg(quality: 85), strip: false), recipe("js", .jpeg(quality: 85), strip: true),
                      recipe("hk", .heic(quality: 70), strip: false), recipe("hs", .heic(quality: 70), strip: true)],
            encoder: ImageIOEncoder(), hardwareJpeg: true, listener: nil, token: try CancelToken())
        #expect(report.failed.isEmpty, "\(report.failed)")
        #expect(report.written.count == 4 && report.jpegEncoder == "imageio" && report.renderer.hasPrefix("develop ("))
        for w in report.written {
            let url = URL(fileURLWithPath: w.path)
            let props = try #require(properties(url))
            let gps = props[kCGImagePropertyGPSDictionary] as? [CFString: Any]
            let stripped = url.deletingPathExtension().lastPathComponent.hasSuffix("s")
            if stripped {
                #expect(gps == nil, "\(url.lastPathComponent): GPS removed")
            } else {
                #expect(abs((gps?[kCGImagePropertyGPSLatitude] as? Double ?? 0) - 20.75) < 0.001, "\(url.lastPathComponent)")
                #expect(gps?[kCGImagePropertyGPSLongitudeRef] as? String == "W")
            }
            #expect((props[kCGImagePropertyTIFFDictionary] as? [CFString: Any])?[kCGImagePropertyTIFFArtist] as? String == "Alexa")
        }
    }

    static let noMetadata = RustSessionStore.record(IPTCMetadata())

    @Test func imageIOEncoderRejectsBadInputAndReportsFailures() throws {
        let e = ImageIOEncoder()
        #expect(throws: DsError.self) {
            try e.encode(format: .jpeg, width: 4, height: 4, rgb16: Data(count: 10), quality: 80, icc: Data(), exif: nil, keywords: [], metadata: Self.noMetadata)
        }
        let px = Data(count: 4 * 4 * 6)
        let jpeg = try e.encode(format: .jpeg, width: 4, height: 4, rgb16: px, quality: 80, icc: Data(), exif: nil, keywords: [], metadata: Self.noMetadata)
        #expect(jpeg.prefix(2) == Data([0xFF, 0xD8]))
        let heic = try e.encode(format: .heic, width: 64, height: 64, rgb16: Data(count: 64 * 64 * 6), quality: 50,
                                icc: Data(), exif: Data("not tiff".utf8), keywords: ["beach", "Crete"],
                                metadata: Self.noMetadata)
        #expect(heic.count > 100)
        // Keywords land in the file's IPTC (ImageIO also mirrors them into XMP dc:subject).
        let src = try #require(CGImageSourceCreateWithData(heic as CFData, nil))
        let iptc = (CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any])?[kCGImagePropertyIPTCDictionary]
        #expect((iptc as? [CFString: Any])?[kCGImagePropertyIPTCKeywords] as? [String] == ["beach", "Crete"])
        #expect(ImageIOEncoder.metadataDictionaries(exifBlob: Data("junk".utf8)).isEmpty)
        #expect(ImageIOEncoder.metadataDictionaries(exifBlob: Data(count: 70_000)).isEmpty, "over 64 KB")
        e.failWith = "encoder down"
        #expect(throws: DsError.self) {
            try e.encode(format: .jpeg, width: 4, height: 4, rgb16: px, quality: 80, icc: Data(), exif: nil, keywords: [], metadata: Self.noMetadata)
        }
    }

    @Test func cancelStopsBetweenPhotos() async throws {
        guard let raw = sampleARWs().first else { return }
        let root = try tempDir("cancel")
        defer { try? fm.removeItem(at: root) }
        let src = root.appendingPathComponent("src")
        try fm.createDirectory(at: src, withIntermediateDirectories: true)
        // 40 clones of one sample.
        let urls = try (1...40).map { i -> URL in
            let d = src.appendingPathComponent(String(format: "C%03d.ARW", i))
            try fm.copyItem(at: raw, to: d)
            return d
        }
        let out = root.appendingPathComponent("Output")
        let job = ExportJob(items: items(urls), recipes: [ExportRecipe(name: "T", format: .tiff16)], defaultFolder: out)
        let task = Task { try await RustExportService().export(job) { _, _ in } }
        try await Task.sleep(for: .milliseconds(100))
        task.cancel()
        // Cancelled before the job started (nothing written) or mid-batch:
        // files already written stay and the outcome says it was cancelled.
        let written: [String]
        do {
            let o = try await task.value
            #expect(o.cancelled && o.failures.isEmpty)
            written = tree(out)
            #expect(written.count == o.written.count, "\(written.count) written")
        } catch is CancellationError {
            written = tree(out)
            #expect(written.isEmpty)
        }
        #expect(written.count < 40, "\(written.count) of 40: stopped early")
        #expect(written.allSatisfy { $0.hasSuffix(".tif") }, "no temp files")
    }

    /// Hardware JPEG / HEIC (ImageIO) vs mozjpeg on full-size develops (24 MP class):
    /// ms per photo for the whole export (develop + encode + write), bytes, and
    /// PSNR against a lossless 16-bit TIFF of the same develop (centre 1024² crop).
    @Test func encoderBenchmark() async throws {
        let raws = Array(sampleARWs().prefix(2))
        guard !raws.isEmpty else { return }
        let root = try tempDir("bench")
        defer { try? fm.removeItem(at: root) }
        let clock = ContinuousClock()
        let sources = raws.enumerated().map { ExportSource(id: UInt64($0.offset), path: $0.element.path, seq: 1,
                                                           gps: nil, artist: nil, copyright: nil) }
        func export(_ label: String, _ format: ExportFileFormat, hw: Bool) async throws -> (ExportReport, Double) {
            let dest = root.appendingPathComponent(label)
            let spec = ExportRecipeSpec(name: label, format: format, size: .original, allowUpscale: false, space: .srgb,
                                        sharpen: 0, fileNameTemplate: "{name}", destination: dest.path, includeExif: true,
                                        stripGps: false)
            let t = clock.now
            let rep = try await exportBatch(sources: sources, recipes: [spec], encoder: ImageIOEncoder(), hardwareJpeg: hw,
                                            listener: nil, token: try CancelToken())
            #expect(rep.failed.isEmpty && rep.written.count == raws.count, "\(rep.failed)")
            return (rep, ms(clock.now - t) / Double(raws.count))
        }
        _ = try await export("warm", .jpeg(quality: 80), hw: false)
        let (tiff, tiffMs) = try await export("tiff", .tiff16, hw: false)
        var refs: [String: CGImage] = [:]
        for w in tiff.written {
            let u = URL(fileURLWithPath: w.path)
            refs[u.deletingPathExtension().lastPathComponent] = decoded(u).flatMap(Self.centre)
        }
        print(String(format: "EXPORTBENCH full-size TIFF16 (reference): %.0f ms/photo, %dx%d",
                     tiffMs, Int(tiff.written[0].width), Int(tiff.written[0].height)))
        for q in [80, 90] as [UInt8] {
            for (label, format, hw) in [("mozjpeg q\(q)", ExportFileFormat.jpeg(quality: q), false),
                                        ("imageio-jpeg q\(q)", .jpeg(quality: q), true),
                                        ("imageio-heic q\(q)", .heic(quality: q), true)] {
                let (rep, perPhoto) = try await export(label, format, hw: hw)
                let bytes = rep.written.map(\.bytes).reduce(0, +) / UInt64(max(rep.written.count, 1))
                var psnr = 0.0
                for w in rep.written {
                    let u = URL(fileURLWithPath: w.path)
                    if let a = refs[u.deletingPathExtension().lastPathComponent], let b = decoded(u).flatMap(Self.centre) {
                        psnr += Self.psnr(a, b)
                    }
                }
                psnr /= Double(max(rep.written.count, 1))
                print(String(format: "EXPORTBENCH full-size %@: %.0f ms/photo (whole export), %d KB, PSNR %.2f dB",
                             label, perPhoto, Int(bytes / 1024), psnr))
            }
        }
    }

    /// The centre 1024×1024 of `i` (PSNR on a crop keeps the benchmark quick).
    static func centre(_ i: CGImage) -> CGImage? {
        let s = min(1024, i.width, i.height)
        return i.cropping(to: CGRect(x: (i.width - s) / 2, y: (i.height - s) / 2, width: s, height: s))
    }

    /// PSNR (dB) of `b` against `a` over RGB (same size, 8-bit sRGB draw).
    static func psnr(_ a: CGImage, _ b: CGImage) -> Double {
        guard a.width == b.width, a.height == b.height else { return 0 }
        func rgba(_ i: CGImage) -> [UInt8] {
            var px = [UInt8](repeating: 0, count: i.width * i.height * 4)
            let ctx = CGContext(data: &px, width: i.width, height: i.height, bitsPerComponent: 8, bytesPerRow: i.width * 4,
                                space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
            ctx.draw(i, in: CGRect(x: 0, y: 0, width: i.width, height: i.height))
            return px
        }
        let (x, y) = (rgba(a), rgba(b))
        var se = 0.0
        for i in stride(from: 0, to: x.count, by: 4) {
            for c in 0..<3 { let d = Double(x[i + c]) - Double(y[i + c]); se += d * d }
        }
        let mse = se / Double(a.width * a.height * 3)
        return mse == 0 ? 99 : 10 * log10(255 * 255 / mse)
    }
}
