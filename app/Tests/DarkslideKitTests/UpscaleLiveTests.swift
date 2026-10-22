import CoreML
import DarkslideFFI
import Foundation
import ImageIO
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// A tiny little-endian linear DNG (`w`×`h`, 3 × 16-bit, a gradient with texture, the
/// A9's colour matrix): a RAW ds-raw decodes, for the Rust side of AI Upscale.
func tinyLinearDNG(_ url: URL, w: Int, h: Int) throws {
    var px: [UInt8] = []
    px.reserveCapacity(w * h * 6)
    for y in 0..<h {
        for x in 0..<w {
            let t = ((x * 7 + y * 3) % 11) * 300
            for v in [2_000 + x * 40 + t, 3_000 + y * 50 + t, 4_000 + t] {
                let s = UInt16(min(v, 40_000))
                px += [UInt8(s & 0xFF), UInt8(s >> 8)]
            }
        }
    }
    func u16(_ v: [Int]) -> [UInt8] { v.flatMap { [UInt8($0 & 0xFF), UInt8(($0 >> 8) & 0xFF)] } }
    func u32(_ v: [Int]) -> [UInt8] { v.flatMap { i in (0..<4).map { UInt8((i >> (8 * $0)) & 0xFF) } } }
    let matrix = [6389, -1703, -378, -4562, 12265, 2587, -670, 1489, 6550]
    let entries: [(tag: Int, type: Int, count: Int, data: [UInt8])] = [
        (0x00FE, 4, 1, u32([0])), (0x0100, 4, 1, u32([w])), (0x0101, 4, 1, u32([h])),
        (0x0102, 3, 3, u16([16, 16, 16])), (0x0103, 3, 1, u16([1])), (0x0106, 3, 1, u16([34_892])),
        (0x010F, 2, 5, Array("SONY\0".utf8)), (0x0110, 2, 7, Array("ILCE-9\0".utf8)),
        (0x0111, 4, 1, u32([8])), (0x0112, 3, 1, u16([1])), (0x0115, 3, 1, u16([3])),
        (0x0116, 4, 1, u32([h])), (0x0117, 4, 1, u32([px.count])),
        (0xC612, 1, 4, [1, 4, 0, 0]), (0xC61D, 4, 3, u32([65_535, 65_535, 65_535])),
        (0xC621, 10, 9, matrix.flatMap { u32([$0]) + u32([10_000]) }),
        (0xC628, 5, 3, u32([500_000, 1_000_000, 1_000_000, 1_000_000, 666_667, 1_000_000])),
        (0xC65A, 3, 1, u16([21])),
    ]
    let ifd = 8 + px.count
    var values = ifd + 2 + 12 * entries.count + 4
    var out: [UInt8] = Array("II".utf8) + u16([42]) + u32([ifd]) + px + u16([entries.count])
    var tail: [UInt8] = []
    for e in entries {
        out += u16([e.tag, e.type]) + u32([e.count])
        if e.data.count <= 4 {
            out += e.data + [UInt8](repeating: 0, count: 4 - e.data.count)
        } else {
            out += u32([values])
            tail += e.data + (e.data.count % 2 == 1 ? [0] : [])
            values += e.data.count + e.data.count % 2
        }
    }
    try Data(out + u32([0]) + tail).write(to: url)
}

/// AI Upscale through the real Rust side without the model (a nearest-neighbour double
/// on the CPU): the tiles, the DNG next to the photo, the session, and export through
/// the app's provider. Rust's export provider is process-wide: these run one at a time.
@MainActor
@Suite(.serialized) struct UpscaleRustTests {
    func folder() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-up-rust-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try tinyLinearDNG(dir.appendingPathComponent("DSC00001.dng"), w: 300, h: 200)
        return dir
    }

    func visibleFiles(_ dir: URL) throws -> [String] {
        try FileManager.default.contentsOfDirectory(atPath: dir.path).filter { !$0.hasPrefix(".") || $0.hasSuffix(".partial") }
            .sorted()
    }

    @Test func aPhotoUpscalesIntoANewPhotoOfTheSession() async throws {
        let dir = try folder()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!))
        let info = try await store.add(folder: dir)
        let photos = try await store.photos(in: info)
        let src = try #require(photos.first)
        #expect(photos.count == 1 && src.meta.pixelWidth == 300)
        var edit = EditState()
        edit.exposure.ev = 0.4
        try await store.save(edits: [src.id: edit])

        let name = try await store.freeDerivedName(for: try #require(src.url), from: src.id, suffix: "Upscaled")
        #expect(name == "DSC00001-Upscaled.dng")
        let tiles = try await RustUpscaleBackend().begin(try #require(src.url), name: name)
        #expect(try tiles.tileCount() == 2 && tiles.tileSize() == 256)
        try await DenoiseEngine.run(tiles, models: [NearestModel(), NearestModel()], scale: 2)
        let dng = try #require(tiles.result)
        #expect(dng.lastPathComponent == "DSC00001-Upscaled.dng")
        #expect(try visibleFiles(dir) == ["DSC00001-Upscaled.dng", "DSC00001.dng"], "no temp file left")

        let id = try await store.addDerived(dng, from: src.id)
        let again = try await store.photos(in: info)
        let up = try #require(again.first { $0.id == id })
        #expect(again.count == 2)
        #expect((up.meta.pixelWidth, up.meta.pixelHeight) == (600, 400))
        #expect(up.meta.camera.contains("ILCE-9"))
        #expect(try await store.edit(for: id).exposure.ev == 0.4, "starts with the source's edit")
        await #expect(throws: (any Error).self) { _ = try await store.addDerived(dng, from: PhotoID(424_242)) }
        await #expect(throws: (any Error).self) { _ = try await store.addDerived(dng, from: src.id) }
        #expect(try await store.freeDerivedName(for: try #require(src.url), from: src.id, suffix: "Upscaled")
            == "DSC00001-Upscaled-1.dng", "taken now")
        await #expect(throws: (any Error).self) {
            _ = try await store.freeDerivedName(for: dng, from: PhotoID(424_242), suffix: "Upscaled")
        }
    }

    @Test func aCancelledUpscaleLeavesNoFile() async throws {
        let dir = try folder()
        defer { try? FileManager.default.removeItem(at: dir) }
        let tiles = try await RustUpscaleBackend().begin(dir.appendingPathComponent("DSC00001.dng"),
                                                         name: "DSC00001-Upscaled.dng")
        let run = Task { try await DenoiseEngine.run(tiles, models: [NearestModel(delay: 0.3)], scale: 2) }
        try await Task.sleep(for: .milliseconds(50))
        run.cancel()
        await #expect(throws: CancellationError.self) { try await run.value }
        #expect(tiles.result == nil)
        await #expect(throws: (any Error).self) { try await tiles.finish() } // a tile is missing
        #expect(try visibleFiles(dir) == ["DSC00001.dng"])
        await #expect(throws: RenderError.self) {
            _ = try await RustUpscaleBackend().begin(dir.appendingPathComponent("missing.dng"), name: "m.dng")
        }
    }

    static func job(_ raw: URL, out: URL) -> ExportJob {
        ExportJob(items: [ExportJobItem(photoID: PhotoID(1), url: raw, seq: 1)],
                  recipes: [ExportRecipe(name: "Big", format: .png16, sizeMode: .percent, sizeValue: 200,
                                         fileNameTemplate: "{seq}", aiUpscale: true)],
                  defaultFolder: out)
    }

    @Test func anExportEnlargesThroughTheAppsProvider() async throws {
        let dir = try folder()
        defer {
            try? FileManager.default.removeItem(at: dir)
            setUpscaleProvider(provider: nil)
        }
        let raw = dir.appendingPathComponent("DSC00001.dng")
        let runs = DoublingModel.Counter()
        AppUpscaleProvider.install { tiles in
            runs.add()
            try await DenoiseEngine.run(tiles, models: [NearestModel()], scale: 2)
            return true
        }
        let o = try await RustExportService().export(Self.job(raw, out: dir.appendingPathComponent("Out"))) { _, _ in }
        #expect(o.failures.isEmpty && o.written.count == 1, "\(o.failures)")
        #expect(runs.value == 1)
        let png = try #require(o.written.first?.url)
        let img = try #require(CGImageSourceCreateWithURL(png as CFURL, nil).flatMap { CGImageSourceCreateImageAtIndex($0, 0, nil) })
        #expect((img.width, img.height) == (600, 400))

        // No model in this build: resampled to the same size; a failure fails that photo.
        AppUpscaleProvider.install { _ in false }
        let r = try await RustExportService().export(Self.job(raw, out: dir.appendingPathComponent("Out2"))) { _, _ in }
        #expect(r.failures.isEmpty && r.written.count == 1)
        AppUpscaleProvider.install { _ in throw UpscaleError.failed("boom") }
        let f = try await RustExportService().export(Self.job(raw, out: dir.appendingPathComponent("Out3"))) { _, _ in }
        #expect(f.written.isEmpty && f.failures.first?.contains("boom") == true, "\(f.failures)")
        AppUpscaleProvider.install { _ in throw CancellationError() }
        let c = try await RustExportService().export(Self.job(raw, out: dir.appendingPathComponent("Out4"))) { _, _ in }
        #expect(c.written.isEmpty)
    }

    /// Cancelling the export's token cancels the upscale it waits for, quickly.
    @Test func cancellingAnExportStopsItsUpscale() async throws {
        let dir = try folder()
        defer {
            try? FileManager.default.removeItem(at: dir)
            setUpscaleProvider(provider: nil)
        }
        let started = DoublingModel.Counter()
        AppUpscaleProvider.install { tiles in
            started.add()
            try await DenoiseEngine.run(tiles, models: [NearestModel(delay: 0.5)], scale: 2)
            return true
        }
        let raw = dir.appendingPathComponent("DSC00001.dng")
        let export = Task { try await RustExportService().export(Self.job(raw, out: dir.appendingPathComponent("Out"))) { _, _ in } }
        #expect(await waitUntil { started.value > 0 })
        let t0 = ContinuousClock.now
        export.cancel()
        let o = try? await export.value
        #expect(o?.written.isEmpty ?? true)
        #expect(ContinuousClock.now - t0 < .seconds(3), "the tiles in flight finish, no more")
        let out = (try? FileManager.default.contentsOfDirectory(atPath: dir.appendingPathComponent("Out").path)) ?? []
        #expect(out.filter { !$0.hasPrefix(".") }.isEmpty, "nothing exported")
    }
}

/// AI Upscale end to end with the real model (`$DS_UPSCALE_MODEL_DIR`: the package
/// `scripts/upscale-model/build-model.sh` prints, or a built `Darkslide.app`), Core ML on
/// the GPU and Neural Engine and the Rust side. Skips without the model. With the samples
/// it also times a real A9 frame (a copy: the DNG is written next to it).
@MainActor
@Suite(.serialized) struct UpscaleLiveTests {
    static var package: URL? {
        guard let path = ProcessInfo.processInfo.environment["DS_UPSCALE_MODEL_DIR"] else { return nil }
        let url = URL(fileURLWithPath: path)
        guard url.pathExtension == "app" else { return url }
        return Bundle(url: url).flatMap(UpscaleModelDescriptor.bundledPackage(in:))
    }

    /// Peak resident memory of this process (bytes).
    static func peakRSS() -> Int {
        var u = rusage()
        getrusage(RUSAGE_SELF, &u)
        return u.ru_maxrss
    }

    @Test func theModelUpscalesAPhotoIntoADNG() async throws {
        guard let package = Self.package else {
            print("AI Upscale live: no model package, skipped")
            return
        }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-up-live-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = UpscaleService(
            model: DenoiseModelStore(package: package, compiledRoot: dir.appendingPathComponent("Models"),
                                     expectedWeights: UpscaleModelDescriptor.weightsSHA256,
                                     name: UpscaleModelDescriptor.name, reasons: UpscaleModelDescriptor.reasons),
            backend: RustUpscaleBackend())
        let tiny = dir.appendingPathComponent("tiny.dng")
        try tinyLinearDNG(tiny, w: 300, h: 200)
        let clock = ContinuousClock()
        var t = clock.now
        let dng = try await s.upscale(tiny, name: "tiny-Upscaled.dng")
        print("AI Upscale live: first use (verify + compile + load + 2 tiles) \(clock.now - t)")
        let meta = try await rawMetadata(path: dng.path, token: CancelToken())
        #expect(meta.width == 600 && meta.height == 400)

        guard let sample = sampleARWs().first else { return }
        let copy = dir.appendingPathComponent(sample.lastPathComponent)
        try FileManager.default.copyItem(at: sample, to: copy)
        let rss = Self.peakRSS()
        t = clock.now
        let big = try await s.upscale(copy, name: copy.deletingPathExtension().lastPathComponent + "-Upscaled.dng")
        let took = clock.now - t
        let m = try await rawMetadata(path: big.path, token: CancelToken())
        let bytes = (try FileManager.default.attributesOfItem(atPath: big.path)[.size] as? Int) ?? 0
        print("AI Upscale live: \(sample.lastPathComponent) → \(m.width ?? 0)x\(m.height ?? 0) in \(took), "
            + "\(bytes >> 20) MB DNG, peak RSS \(Self.peakRSS() >> 20) MB (+\((Self.peakRSS() - rss) >> 20) MB)")
        #expect(Set([m.width, m.height]) == [12_000, 8_000], "twice the A9's 6000×4000")

        // The new photo develops like the original: the same picture at preview size.
        let a = try await RustRaw.photo(at: copy, id: PhotoID(1))
        let b = try await RustRaw.photo(at: big, id: PhotoID(2))
        let (fa, fb) = (try await Self.small(a), try await Self.small(b))
        #expect((fa.width, fa.height) == (fb.width, fb.height))
        let diff = zip(Self.bytes(fa), Self.bytes(fb)).map { abs(Int($0) - Int($1)) }
        let mean = Double(diff.reduce(0, +)) / Double(max(diff.count, 1))
        print(String(format: "AI Upscale live: 512 px renders of the original and the upscale differ by %.2f/255 on average", mean))
        #expect(mean < 4, "same tone, colour and framing")

        if let out = ProcessInfo.processInfo.environment["DS_UPSCALE_CROPS_DIR"] {
            let dir = URL(fileURLWithPath: out)
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            let region = CGRect(x: 0.45, y: 0.45, width: 0.05, height: 0.05)
            for (name, p) in [("original", a), ("upscaled", b)] {
                DenoiseGlobalStore.DenoiseLiveTests.write(try await DenoiseGlobalStore.DenoiseLiveTests.crop(p, EditState(), region),
                                                          dir.appendingPathComponent("\(name).png"))
            }
        }
    }

    static func small(_ p: Photo) async throws -> RenderedFrame {
        try await DevelopRenderer().render(p, edit: EditState(), request: RenderRequest(maxPixelSize: 512, quality: .full))
    }

    static func bytes(_ f: RenderedFrame) -> [UInt8] {
        var px = [UInt8](repeating: 0, count: f.width * f.height * 4)
        f.withBytes { raw in
            for y in 0..<f.height {
                for i in 0..<(f.width * 4) { px[y * f.width * 4 + i] = raw[y * f.bytesPerRow + i] }
            }
        }
        return px
    }
}
