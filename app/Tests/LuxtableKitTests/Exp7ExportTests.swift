import CoreGraphics
import LuxtableFFI
import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

// Integration A review, export (Part C): E-M1, E-M2, E-L1…L6 and M4 (E-H1, the
// preview-size labelling, is gone: exports are full develops, see Ex8ExportTests).

private var fm: FileManager { .default }

private func tempDir(_ tag: String) throws -> URL {
    let d = fm.temporaryDirectory.appendingPathComponent("lt-exp7-\(tag)-\(UUID())", isDirectory: true)
    try fm.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

private func properties(_ url: URL) -> [CFString: Any]? {
    CGImageSourceCreateWithURL(url as CFURL, nil).flatMap { CGImageSourceCopyPropertiesAtIndex($0, 0, nil) } as? [CFString: Any]
}

private func pixelSize(_ url: URL) -> (Int, Int)? {
    guard let p = properties(url), let w = p[kCGImagePropertyPixelWidth] as? Int, let h = p[kCGImagePropertyPixelHeight] as? Int
    else { return nil }
    return (w, h)
}

private func jobItems(_ urls: [URL]) -> [ExportJobItem] {
    urls.enumerated().map { ExportJobItem(photoID: PhotoID(Int64($0.offset + 1)), url: $0.element, seq: $0.offset + 1) }
}

/// A camera-phone style original: `w`×`h` stored pixels (left half dark), EXIF
/// orientation, model, capture time with UTC offset and a GPS position.
private func rasterOriginal(_ url: URL, type: UTType, w: Int, h: Int, orientation: Int) throws {
    let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                        space: CGColorSpace(name: CGColorSpace.sRGB)!,
                        bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
    ctx.setFillColor(CGColor(gray: 0.9, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
    ctx.setFillColor(CGColor(gray: 0.1, alpha: 1))
    ctx.fill(CGRect(x: 0, y: 0, width: w / 2, height: h))
    let dest = try #require(CGImageDestinationCreateWithURL(url as CFURL, type.identifier as CFString, 1, nil))
    let props: [CFString: Any] = [
        kCGImagePropertyOrientation: orientation,
        kCGImagePropertyTIFFDictionary: [kCGImagePropertyTIFFMake: "Apple", kCGImagePropertyTIFFModel: "iPhone 17 Pro"],
        kCGImagePropertyExifDictionary: [kCGImagePropertyExifDateTimeOriginal: "2026:08:09 07:30:05",
                                         kCGImagePropertyExifOffsetTimeOriginal: "-10:00"],
        kCGImagePropertyGPSDictionary: [kCGImagePropertyGPSLatitude: 20.75, kCGImagePropertyGPSLatitudeRef: "N",
                                        kCGImagePropertyGPSLongitude: 156.5, kCGImagePropertyGPSLongitudeRef: "W"],
    ]
    CGImageDestinationAddImage(dest, try #require(ctx.makeImage()), props as CFDictionary)
    #expect(CGImageDestinationFinalize(dest))
}

// MARK: - E-M1: the default export folder is never on a card

@Suite struct Exp7DestinationTests {
    @Test func aVolumeWithDCIMAtItsRootIsACard() throws {
        let root = try tempDir("vol")
        defer { try? fm.removeItem(at: root) }
        #expect(!ExportDestination.looksLikeCard(root, isLocal: true))
        try fm.createDirectory(at: root.appendingPathComponent("DCIM"), withIntermediateDirectories: true)
        #expect(ExportDestination.looksLikeCard(root, isLocal: true))
        #expect(!ExportDestination.looksLikeCard(root, isLocal: false), "a NAS share with a DCIM folder isn't a card")
        // A file named DCIM isn't a card's DCIM directory.
        let other = try tempDir("vol2")
        defer { try? fm.removeItem(at: other) }
        try Data().write(to: other.appendingPathComponent("DCIM"))
        #expect(!ExportDestination.looksLikeCard(other, isLocal: true))
        #expect(ExportDestination.isSafeExportFolder(root), "a temp folder on the boot volume is fine")
    }

    /// A real exFAT card image (DCIM + a sample): a session opened on it exports
    /// to ~/Pictures/Luxtable Exports/<name>, never onto the card.
    @Test func aSessionOnACardImageExportsToPictures() async throws {
        let base = try tempDir("card")
        defer { try? fm.removeItem(at: base) }
        guard let card = await TestCardImage.make(in: base, name: "EXP7CARD", sizeMB: 32) else { return }
        do {
            try fm.createDirectory(at: card.mount.appendingPathComponent("DCIM/100MSDCF"), withIntermediateDirectories: true)
            #expect(!ExportDestination.isSafeExportFolder(card.mount))
            #expect(!ExportDestination.isSafeExportFolder(card.mount.appendingPathComponent("DCIM/100MSDCF")))
            let home = URL(fileURLWithPath: "/Users/someone")
            let s = SessionInfo(name: "EXP7CARD", url: card.mount, photoCount: 1, createdAt: .now)
            #expect(await ExportDestination.resolve(for: s, home: home).path
                == "/Users/someone/Pictures/Luxtable Exports/EXP7CARD")
        }
        await card.detach()
    }
}

// MARK: - E-M2 / E-L1: JPEG and HEIF originals, GPS; videos are left out

@MainActor
@Suite(.serialized) struct Exp7OriginalsTests {
    /// JPEG (orientation 6) and HEIC originals export at their full size,
    /// upright, with camera, UTC offset and GPS; "Remove location" strips GPS.
    @Test func rasterOriginalsExportAtFullSizeWithTheirMetadata() async throws {
        let dir = try tempDir("raster")
        defer { try? fm.removeItem(at: dir) }
        let jpg = dir.appendingPathComponent("IMG_0001.JPG"), heic = dir.appendingPathComponent("IMG_0002.HEIC")
        try rasterOriginal(jpg, type: .jpeg, w: 300, h: 200, orientation: 6)
        try rasterOriginal(heic, type: .heic, w: 300, h: 200, orientation: 1)
        let s = RustExportService()
        #expect(await s.nameSample(for: jpg) == ExportFileNameSample(name: "IMG_0001", camera: "iPhone 17 Pro",
                                                                     captureTime: "2026-08-09T07:30:05"))
        let out = dir.appendingPathComponent("Output")
        let recipes = [ExportRecipe(name: "Keep", format: .jpeg, fileNameTemplate: "{date}_{name}"),
                       ExportRecipe(name: "NoGPS", format: .heic, subfolder: "heic", stripGPS: true)]
        let o = try await s.export(ExportJob(items: jobItems([jpg, heic]), recipes: recipes, defaultFolder: out)) { _, _ in }
        #expect(o.failures.isEmpty, "\(o.failures)")
        #expect(o.written.count == 4)
        let keptJPG = try #require(o.written.first { $0.recipeName == "Keep" && $0.photoID == PhotoID(1) })
        #expect(keptJPG.url.lastPathComponent == "20260809_IMG_0001.jpg")
        #expect((keptJPG.width, keptJPG.height) == (200, 300), "upright")
        #expect(pixelSize(keptJPG.url).map { $0 == (200, 300) } == true)
        let p = try #require(properties(keptJPG.url))
        #expect((p[kCGImagePropertyTIFFDictionary] as? [CFString: Any])?[kCGImagePropertyTIFFModel] as? String == "iPhone 17 Pro")
        #expect((p[kCGImagePropertyExifDictionary] as? [CFString: Any])?[kCGImagePropertyExifOffsetTimeOriginal] as? String
            == "-10:00")
        let gps = try #require(p[kCGImagePropertyGPSDictionary] as? [CFString: Any], "GPS kept")
        #expect((gps[kCGImagePropertyGPSLatitude] as? Double).map { abs($0 - 20.75) < 1e-4 } == true)
        #expect(gps[kCGImagePropertyGPSLongitudeRef] as? String == "W")
        for w in o.written where w.recipeName == "NoGPS" {
            let q = try #require(properties(w.url))
            #expect(q[kCGImagePropertyGPSDictionary] == nil, "\(w.url.lastPathComponent): location removed")
            #expect((q[kCGImagePropertyTIFFDictionary] as? [CFString: Any])?[kCGImagePropertyTIFFModel] as? String
                == "iPhone 17 Pro")
        }
        let heicOut = try #require(o.written.first { $0.recipeName == "Keep" && $0.photoID == PhotoID(2) })
        #expect((heicOut.width, heicOut.height) == (300, 200))
    }

    /// Videos in the selection are left out and the sheet says so; a
    /// video-only selection explains why nothing opens.
    @Test func videosAreLeftOutOfTheExport() async throws {
        let m = makeModel(store: VideoStore())
        await m.start()
        #expect(await waitUntil { m.photos.count == 2 })
        m.exporter = Exp6FakeExport()
        m.selection = Set(m.photos.map(\.id))
        #expect(m.exportItems.count == 1 && m.exportSkippedVideos == 1)
        #expect(m.exportItems.allSatisfy { $0.url.pathExtension == "ARW" })
        m.beginExport(store: ExportRecipeStore(defaults: testDefaults()))
        let e = try #require(m.exportModel)
        #expect(e.items.count == 1 && e.skippedVideosNote == "1 video isn't exported (videos stay as they are).")
        m.endExport()
        let video = try #require(m.photos.first { $0.isVideo })
        m.selection = [video.id]
        m.beginExport(store: ExportRecipeStore(defaults: testDefaults()))
        #expect(m.exportModel == nil && m.sheet == nil)
        #expect(m.toast?.message == "Videos aren't exported. Select photos to export")
    }
}

// MARK: - E-L2, E-L4, M4: the sheet

@MainActor
@Suite(.serialized) struct Exp7SheetTests {
    /// The file-name example uses the first photo's real capture time and
    /// camera, and it is exactly what the export writes.
    @Test func fileNameExampleMatchesWhatTheExportWrites() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        defer { try? fm.removeItem(at: dir) }
        let raw = try #require(try fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil).first)
        let s = RustExportService()
        let e = ExportModel(items: jobItems([raw]), defaultFolder: dir.appendingPathComponent("Output"), service: s,
                            store: ExportRecipeStore(defaults: testDefaults()))
        let recipe = ExportRecipe(name: "Dated", fileNameTemplate: "{date}_{camera}_{seq:2}")
        #expect(e.sampleFileName(recipe) == "unknown_unknown_01.jpg" || e.sampleFileName(recipe)?.hasSuffix("_01.jpg") == true)
        e.prepare(session: nil)
        await e.prepared()
        let sample = try #require(e.nameSample)
        #expect(sample.camera == "ILCE-9" && sample.captureTime != nil)
        let example = try #require(e.sampleFileName(recipe))
        #expect(example.hasPrefix(String(sample.captureTime!.prefix(10)).replacingOccurrences(of: "-", with: "")))
        #expect(example.contains("_ILCE-9_01"), "\(example)")
        let o = try await s.export(ExportJob(items: e.items, recipes: [recipe], defaultFolder: e.defaultFolder)) { _, _ in }
        #expect(o.written.first?.url.lastPathComponent == example)
    }

    /// While metadata is read the sheet says so, then counts photos (E-L4).
    @Test func runningTextShowsTheMetadataPhase() async {
        let f = Exp6FakeExport()
        f.metadataSteps = 2
        f.hold = true
        let urls = (1...3).map { URL(fileURLWithPath: "/tmp/in/IMG\($0).ARW") }
        let m = ExportModel(items: jobItems(urls), defaultFolder: URL(fileURLWithPath: "/tmp/Out"), service: f,
                            store: ExportRecipeStore(defaults: testDefaults()))
        m.start()
        #expect(await waitUntil(timeout: .seconds(20)) { m.readingMetadata && m.metadataDone == 1 })
        #expect(m.runningText == "Reading metadata 1 of 3…")
        f.hold = false
        await m.waitUntilDone()
        #expect(!m.readingMetadata && m.runningText == nil)
        guard case .finished = m.phase else { Issue.record("\(m.phase)"); return }
    }

    /// Live: the service reports the metadata phase first, then photos, and
    /// never reports "N of N" for a cancelled run (E-L4/L5).
    @Test func liveProgressOrdersMetadataBeforePhotos() async throws {
        guard let dir = try sampleFolderCopy(limit: 3) else { return }
        defer { try? fm.removeItem(at: dir) }
        let raws = try fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil).filter { $0.pathExtension == "ARW" }
        let seen = Exp7Box<[ExportProgressUpdate]>([])
        let o = try await RustExportService().export(
            ExportJob(items: jobItems(raws), recipes: [ExportRecipe(name: "Web", sizeMode: .longEdge, sizeValue: 400)],
                      defaultFolder: dir.appendingPathComponent("Output"))) { (u: ExportProgressUpdate) in
            seen.withLock { $0.append(u) }
        }
        #expect(o.written.count == raws.count)
        let ups = seen.withLock { $0 }
        let firstPhoto = ups.firstIndex { !$0.readingMetadata } ?? ups.count
        #expect(ups.prefix(firstPhoto).allSatisfy { $0.readingMetadata })
        #expect(ups.prefix(firstPhoto).contains { $0.done == raws.count }, "every photo's metadata was read: \(ups)")
        #expect(ups.dropFirst(firstPhoto).allSatisfy { !$0.readingMetadata })
        #expect(ups.last == ExportProgressUpdate(done: raws.count, total: raws.count))
    }

    /// M4: ⌘E's recipes live in the app's own defaults suite (the snapshot suite
    /// in `--snapshot` runs, a test suite here), never in `.standard`.
    @Test func recipesAreSavedInTheAppsDefaults() async throws {
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        defer { try? fm.removeItem(at: dir) }
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()),
                                     storeBase: fm.temporaryDirectory.appendingPathComponent("lt-exp7-m4-\(UUID())"))
        let info = try await store.add(folder: dir)
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: MockImportService(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        m.exporter = RustExportService()
        await m.open(info)
        #expect(await waitUntil(timeout: .seconds(10)) { m.visibleIDs.count == 1 })
        m.selection = Set(m.visibleIDs)
        let standardBefore = UserDefaults.standard.data(forKey: ExportRecipeStore.key)
        m.beginExport()
        let e = try #require(m.exportModel)
        e.recipes[0].name = "Mine"
        #expect(settings.userDefaults.data(forKey: ExportRecipeStore.key) != nil)
        #expect(ExportRecipeStore(defaults: settings.userDefaults).load()[0].name == "Mine")
        #expect(UserDefaults.standard.data(forKey: ExportRecipeStore.key) == standardBefore, ".standard untouched")
        await e.prepared()
        #expect(e.destinationReady && e.defaultFolder.lastPathComponent == "Output")
        m.endExport()
    }
}

private final class Exp7Box<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ v: T) { value = v }
    func withLock<R>(_ f: (inout T) -> R) -> R { lock.withLock { f(&value) } }
}
