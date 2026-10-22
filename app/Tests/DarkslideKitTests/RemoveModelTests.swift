import CoreGraphics
import CoreML
import DarkslideAI
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// An `MLModel` whose predictions are canned (Core ML can't be faked any other way).
final class StubMLModel: MLModel, @unchecked Sendable {
    var output: (MLFeatureProvider) throws -> MLFeatureProvider = { _ in throw InpaintError.noOutput }
    private(set) var inputs: [MLFeatureProvider] = []

    override func prediction(from input: MLFeatureProvider) throws -> MLFeatureProvider {
        inputs.append(input)
        return try output(input)
    }

    override func prediction(from input: MLFeatureProvider, options: MLPredictionOptions) throws -> MLFeatureProvider {
        try prediction(from: input)
    }
}

/// The generic downloadable-model lifecycle (LaMa, face parsing): pinned files, progress,
/// cancel, checksums, compile errors, and a damaged compiled copy fetched again.
@MainActor
@Suite(.serialized) struct DownloadableModelTests {
    final class Files: ModelDownloader, @unchecked Sendable {
        let files: [String: Data]
        let error: Error?
        private let lock = NSLock()
        private var n = 0
        init(_ files: [String: Data], error: Error? = nil) {
            self.files = files
            self.error = error
        }
        var downloads: Int { lock.withLock { n } }

        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            lock.withLock { n += 1 }
            if let error { throw error }
            guard let data = files[url.lastPathComponent] else { throw URLError(.fileDoesNotExist) }
            try data.write(to: destination)
            progress(Int64(data.count) / 2)
            progress(Int64(data.count))
        }
    }

    struct Stalls: ModelDownloader {
        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            progress(1)
            while true { try await Task.sleep(for: .milliseconds(5)) }
        }
    }

    static let data = Data(repeating: 9, count: 300)

    static func descriptor(sha: String? = nil) throws -> DownloadableModelDescriptor {
        let real = try ModelFiles.sha256(of: { () throws -> URL in
            let u = FileManager.default.temporaryDirectory.appendingPathComponent("ds-dm-\(UUID())")
            try data.write(to: u)
            return u
        }())
        return DownloadableModelDescriptor(
            name: "Tiny", displayName: "tiny model",
            files: [DownloadableModelDescriptor.file("https://example.invalid/r/Tiny.mlpackage/", "Data/a.bin", sha ?? real, 300)],
            computeUnits: .cpuOnly, environmentKey: "DS_TINY_MODEL_DIR")
    }

    func root() -> URL { FileManager.default.temporaryDirectory.appendingPathComponent("ds-dm-root-\(UUID())") }

    static let compileDir: @Sendable (URL) async throws -> URL = { pkg in
        let out = pkg.deletingLastPathComponent().appendingPathComponent("c.mlmodelc")
        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        return out
    }

    static let loads: @Sendable (URL, MLModelConfiguration) throws -> MLModel = { _, config in
        #expect(config.computeUnits == .cpuOnly)
        return StubMLModel()
    }

    @Test func theModelsArePinnedToCommits() throws {
        for (d, rev) in [(DownloadableModelDescriptor.lama, DownloadableModelDescriptor.lamaRevision),
                         (.faceParsing, DownloadableModelDescriptor.faceParsingRevision)] {
            #expect(rev.count == 40 && rev.allSatisfy(\.isHexDigit))
            for f in d.files {
                #expect(f.url.absoluteString.contains("/resolve/\(rev)/\(d.name).mlpackage/\(f.path)"))
                #expect(f.sha256.count == 64 && f.size > 0)
            }
        }
        #expect(DownloadableModelDescriptor.lama.totalBytes == 216_647_386)
        #expect(DownloadableModelDescriptor.faceParsing.totalBytes == 53_182_754)
        #expect(DownloadableModelDescriptor.faceParsing.computeUnits == .cpuAndGPU)
        #expect(LamaInpainter.modelID == "lama-5ed76e3")
        // The compiled copy is named by revision: a new pin never loads the old one.
        #expect(DownloadableModelDescriptor.lama.compiledName == "LaMa-5ed76e3799ab.mlmodelc")
        #expect(DownloadableModelDescriptor.faceParsing.compiledName == "FaceParsing-6af21c2f8341.mlmodelc")
        #expect(try Self.descriptor().compiledName == "Tiny.mlmodelc")
    }

    @Test func downloadsWithProgressCompilesAndLoads() async throws {
        let files = Files(["a.bin": Self.data])
        let m = DownloadableModel(root: root(), descriptor: try Self.descriptor(), downloader: files,
                                  compile: Self.compileDir, load: Self.loads)
        #expect(m.state == .notDownloaded && !m.isDownloading)
        var seen: [DownloadableModel.State] = []
        let watch = Task { @MainActor in
            while !Task.isCancelled {
                if seen.last != m.state { seen.append(m.state) }
                await Task.yield()
            }
        }
        let model = try await m.loadedModel()
        watch.cancel()
        #expect(m.state == .ready)
        #expect(seen.contains { if case .downloading = $0 { true } else { false } })
        #expect(FileManager.default.fileExists(atPath: m.compiledURL.path))
        let second = try await m.loadedModel()
        #expect(second === model, "loaded once")
        #expect(files.downloads == 1)
        // A new instance finds the compiled copy: ready without a download.
        let again = DownloadableModel(root: m.root, descriptor: try Self.descriptor(), downloader: files,
                                      compile: Self.compileDir, load: Self.loads)
        #expect(again.state == .ready)
        _ = try await again.prepare()
        #expect(files.downloads == 1)
    }

    @Test func concurrentPreparesShareOneDownload() async throws {
        let files = Files(["a.bin": Self.data])
        let m = DownloadableModel(root: root(), descriptor: try Self.descriptor(), downloader: files, compile: Self.compileDir)
        async let a = m.prepare()
        async let b = m.prepare()
        let (x, y) = try await (a, b)
        #expect(x == y && files.downloads == 1)
    }

    @Test func cancelGoesBackToNotDownloaded() async throws {
        let m = DownloadableModel(root: root(), descriptor: try Self.descriptor(), downloader: Stalls(), compile: Self.compileDir)
        let t = Task { try await m.prepare() }
        #expect(await waitUntil { m.isDownloading })
        m.cancel()
        await #expect(throws: CancellationError.self) { try await t.value }
        #expect(m.state == .notDownloaded)
    }

    @Test func aBadChecksumFails() async throws {
        let m = DownloadableModel(root: root(), descriptor: try Self.descriptor(sha: String(repeating: "0", count: 64)),
                                  downloader: Files(["a.bin": Self.data]), compile: Self.compileDir)
        await #expect(throws: ModelDownloadError.checksum("tiny model", "Data/a.bin")) { try await m.prepare() }
        #expect(m.state == .failed("The downloaded tiny model is damaged (Data/a.bin). Try again."))
        #expect(!FileManager.default.fileExists(atPath: m.compiledURL.path))
    }

    @Test func downloadAndCompileErrorsAreReported() async throws {
        let d = try Self.descriptor()
        let net = DownloadableModel(root: root(), descriptor: d, downloader: Files([:], error: URLError(.notConnectedToInternet)),
                                    compile: Self.compileDir)
        await #expect(throws: ModelDownloadError.self) { try await net.prepare() }
        guard case .failed(let msg) = net.state else { Issue.record("not failed"); return }
        #expect(msg.hasPrefix("Couldn't download the tiny model"))
        let shared = DownloadableModel(root: root(), descriptor: d, downloader: Files([:], error: SegmentationError.download("gone")),
                                       compile: Self.compileDir)
        await #expect(throws: ModelDownloadError.download("tiny model", "gone")) { try await shared.prepare() }
        let compile = DownloadableModel(root: root(), descriptor: d, downloader: Files(["a.bin": Self.data]),
                                        compile: { _ in throw URLError(.cannotParseResponse) })
        await #expect(throws: ModelDownloadError.self) { try await compile.prepare() }
        #expect(ModelDownloadError.compile("x", "y").errorDescription == "Couldn't prepare the x: y")
    }

    @Test func aLoadFailureIsRetriedOnceThenLeftToTheUser() async throws {
        let files = Files(["a.bin": Self.data])
        final class Count: @unchecked Sendable { var n = 0 }
        let count = Count()
        let m = DownloadableModel(root: root(), descriptor: try Self.descriptor(), downloader: files, compile: Self.compileDir,
                                  load: { _, _ in
                                      count.n += 1
                                      if count.n == 1 { throw URLError(.cannotDecodeContentData) }
                                      return StubMLModel()
                                  })
        _ = try await m.loadedModel()
        #expect(files.downloads == 1 && count.n == 2 && m.state == .ready, "loaded on the retry, no second download")
        // Never loads: deleted and failed (never "ready" while unusable), and not fetched
        // again behind the user's back: their Try Again does that.
        let broken = DownloadableModel(root: root(), descriptor: try Self.descriptor(), downloader: files, compile: Self.compileDir,
                                       load: { _, _ in throw URLError(.cannotDecodeContentData) })
        await #expect(throws: ModelDownloadError.self) { _ = try await broken.loadedModel() }
        guard case .failed(let why) = broken.state else { Issue.record("not failed"); return }
        #expect(why.hasPrefix("Couldn't prepare the tiny model") && why.hasSuffix("Try again to download it anew."))
        #expect(!FileManager.default.fileExists(atPath: broken.compiledURL.path))
        #expect(files.downloads == 2, "one download for each model, none behind the user's back")
        _ = try? await broken.prepare()
        #expect(files.downloads == 3, "Try Again downloads it anew")
    }

    @Test func cancelDuringCompileInstallsNothing() async throws {
        final class Flag: @unchecked Sendable {
            let lock = NSLock()
            var compiling = false
        }
        let flag = Flag()
        let m = DownloadableModel(root: root(), descriptor: try Self.descriptor(), downloader: Files(["a.bin": Self.data]),
                                  compile: { pkg in
                                      flag.lock.withLock { flag.compiling = true }
                                      // A compile doesn't notice cancellation: it runs to the end.
                                      let end = Date().addingTimeInterval(0.3)
                                      while Date() < end { try? await Task.sleep(for: .milliseconds(10)) }
                                      return try await Self.compileDir(pkg)
                                  })
        let t = Task { try await m.prepare() }
        #expect(await waitUntil { flag.lock.withLock { flag.compiling } })
        m.cancel()
        await #expect(throws: CancellationError.self) { try await t.value }
        #expect(m.state == .notDownloaded)
        #expect(!FileManager.default.fileExists(atPath: m.compiledURL.path), "a cancelled download is never installed")
    }

    @Test func theEnvironmentPointsAtALocalPackage() throws {
        let local = DownloadableModel.standard(try Self.descriptor(), environment: ["DS_TINY_MODEL_DIR": "/tmp/x.mlpackage"])
        #expect(local.compiledURL.path.hasSuffix("Models/Tiny.mlmodelc"))
        #expect(local.compiledURL.path.hasPrefix(AppPaths.support.path))
        _ = DownloadableModel.standard(try Self.descriptor(), environment: [:])
    }
}

/// LaMa pre/post-processing with a fake model: the fit into the square, the hole
/// threshold, and known pixels coming back byte for byte.
struct LamaInpainterTests {
    final class Fake: InpaintModel, @unchecked Sendable {
        let side: Int
        var fill: UInt8 = 200
        var fails = false
        var lastMask: [UInt8] = []
        var lastRGB: [UInt8] = []
        init(side: Int) { self.side = side }
        func predict(rgb: [UInt8], mask: [UInt8]) throws -> [UInt8] {
            if fails { throw InpaintError.noOutput }
            lastMask = mask
            lastRGB = rgb
            return [UInt8](repeating: fill, count: side * side * 3)
        }
    }

    static func image(_ w: Int, _ h: Int) -> [UInt8] {
        (0..<(w * h * 3)).map { UInt8(($0 * 7) % 251) }
    }

    @Test func onlyHolesChange() throws {
        let fake = Fake(side: 16)
        let lama = LamaInpainter(model: fake)
        #expect(lama.modelId() == LamaInpainter.modelID && lama.maxEdge() == 16)
        let w = 8, h = 4
        let rgb = Self.image(w, h)
        var mask = [UInt8](repeating: 0, count: w * h)
        mask[5] = 255
        mask[6] = 128
        let out = [UInt8](try #require(lama.inpaint(width: UInt32(w), height: UInt32(h), rgb: Data(rgb), mask: Data(mask))))
        for i in 0..<(w * h) where mask[i] == 0 { #expect(Array(out[i * 3..<i * 3 + 3]) == Array(rgb[i * 3..<i * 3 + 3])) }
        #expect(Array(out[15..<18]) == [200, 200, 200])
        let half = (Int(200) * 128 + Int(rgb[18]) * 127 + 127) / 255
        #expect(Int(out[18]) == half, "a partial mask blends")
        // The fit: 8×4 scales by 2 into the top 16×8 of the square; holes only there.
        #expect(fake.lastMask.count == 256)
        #expect(fake.lastMask[16 * 1 + 10] == 255 && fake.lastMask[16 * 1 + 14] == 0 && fake.lastMask[16 * 12] == 0)
    }

    @Test func degenerateInputs() {
        let fake = Fake(side: 8)
        let lama = LamaInpainter(model: fake)
        #expect(lama.inpaint(width: 2, height: 2, rgb: Data([1]), mask: Data([0, 0, 0, 0])) == nil)
        #expect(lama.inpaint(width: 0, height: 2, rgb: Data(), mask: Data()) == nil)
        let rgb = Data(Self.image(2, 2))
        #expect(lama.inpaint(width: 2, height: 2, rgb: rgb, mask: Data([0, 0, 0, 0])) == rgb, "nothing to fill")
        fake.fails = true
        #expect(lama.inpaint(width: 2, height: 2, rgb: rgb, mask: Data([255, 0, 0, 0])) == nil, "PatchMatch fills it")
    }

    @Test func geometry() {
        let fit = InpaintFit(width: 400, height: 100, side: 800)
        #expect(fit.scale == 2 && fit.fittedWidth == 800 && fit.fittedHeight == 200)
        #expect((0..<8).map { InpaintFit.mirror($0, 3) } == [0, 1, 2, 2, 1, 0, 0, 1])
        #expect(InpaintFit.mirror(-1, 3) == 0 && InpaintFit.mirror(5, 1) == 0)
        // Any coverage in a source block makes the target pixel a hole.
        var m = [UInt8](repeating: 0, count: 16)
        m[5] = 1
        #expect(InpaintFit.resampleMask(m, 4, 4, to: 2, 2) == [255, 0, 0, 0])
        #expect(InpaintFit.resampleMask([0, 255], 2, 1, to: 4, 1) == [0, 0, 255, 255])
        // Box down, bilinear up, identity.
        #expect(InpaintFit.resample([0, 100, 200, 100], 4, 1, channels: 1, to: 2, 1) == [50, 150])
        #expect(InpaintFit.resample([0, 100], 2, 1, channels: 1, to: 4, 1) == [0, 25, 75, 100])
        #expect(InpaintFit.resample([7, 8], 2, 1, channels: 1, to: 2, 1) == [7, 8])
        // The mirrored margin repeats the crop and has no holes.
        let tall = InpaintFit(width: 1, height: 2, side: 4)
        #expect(tall.fittedWidth == 2 && tall.fittedHeight == 4)
        #expect(tall.squareMask([255, 0]) == [255, 255, 0, 0, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        let sq = tall.squareRGB([10, 10, 10, 50, 50, 50])
        #expect(sq.count == 48 && sq[6] == 10 && sq[9] == 10)
    }

    @Test func coreMLWrapperFeedsPixelBuffersAndReadsTheOutput() throws {
        let stub = StubMLModel()
        stub.output = { input in
            #expect(input.featureValue(for: "image")?.imageBufferValue != nil)
            let mask = try #require(input.featureValue(for: "mask")?.imageBufferValue)
            #expect(CVPixelBufferGetPixelFormatType(mask) == kCVPixelFormatType_OneComponent8)
            let out = try PixelBuffers.bgra(rgba: [UInt8](repeating: 77, count: 4 * 4 * 4), width: 4, height: 4)
            return try MLDictionaryFeatureProvider(dictionary: ["output": MLFeatureValue(pixelBuffer: out)])
        }
        let model = CoreMLInpaintModel(model: stub, side: 4)
        let out = try model.predict(rgb: [UInt8](repeating: 1, count: 48), mask: [UInt8](repeating: 255, count: 16))
        #expect(out == [UInt8](repeating: 77, count: 48))
        stub.output = { _ in try MLDictionaryFeatureProvider(dictionary: [:]) }
        #expect(throws: InpaintError.noOutput) { try model.predict(rgb: [UInt8](repeating: 1, count: 48), mask: [UInt8](repeating: 0, count: 16)) }
        stub.output = { _ in
            let out = try PixelBuffers.bgra(rgba: [UInt8](repeating: 1, count: 4 * 2 * 2), width: 2, height: 2)
            return try MLDictionaryFeatureProvider(dictionary: ["output": MLFeatureValue(pixelBuffer: out)])
        }
        #expect(throws: InpaintError.noOutput) { try model.predict(rgb: [UInt8](repeating: 1, count: 48), mask: [UInt8](repeating: 0, count: 16)) }
        #expect(CoreMLInpaintModel(model: stub).side == 800)
    }

    @Test func pixelBuffersRoundTrip() throws {
        let rgba: [UInt8] = [10, 20, 30, 255, 40, 50, 60, 255]
        let pb = try PixelBuffers.bgra(rgba: rgba, width: 2, height: 1)
        let back = PixelBuffers.rgb(pb)
        #expect(back.width == 2 && back.rgb == [10, 20, 30, 40, 50, 60])
        let g = try PixelBuffers.gray([1, 2, 3, 4], width: 2, height: 2)
        CVPixelBufferLockBaseAddress(g, .readOnly)
        let p = CVPixelBufferGetBaseAddress(g)!.assumingMemoryBound(to: UInt8.self)
        #expect(p[0] == 1 && p[CVPixelBufferGetBytesPerRow(g) + 1] == 4)
        CVPixelBufferUnlockBaseAddress(g, .readOnly)
        var argb: CVPixelBuffer?
        CVPixelBufferCreate(nil, 1, 1, kCVPixelFormatType_32ARGB, nil, &argb)
        let a = try #require(argb)
        CVPixelBufferLockBaseAddress(a, [])
        let ap = CVPixelBufferGetBaseAddress(a)!.assumingMemoryBound(to: UInt8.self)
        ap[0] = 255; ap[1] = 1; ap[2] = 2; ap[3] = 3
        CVPixelBufferUnlockBaseAddress(a, [])
        #expect(PixelBuffers.rgb(a).rgb == [1, 2, 3])
    }
}

/// The app side of removal: Rust's fill cache at launch, LaMa registered once ready
/// (downloaded the first time the user removes something), and the Remove tool.
@MainActor
@Suite(.serialized) struct RemovalModelTests {
    final class Calls: @unchecked Sendable {
        var registered: [DevelopInpainter?] = []
        var dirs: [String?] = []
    }

    func make(_ calls: Calls, downloader: any ModelDownloader, root: URL? = nil,
              load: @escaping @Sendable (URL, MLModelConfiguration) throws -> MLModel = { _, _ in StubMLModel() }) throws -> RemovalModel {
        let m = DownloadableModel(root: root ?? FileManager.default.temporaryDirectory.appendingPathComponent("ds-rm-\(UUID())"),
                                  descriptor: try DownloadableModelDescriptor(
                                      name: "LaMa", displayName: "removal model",
                                      files: DownloadableModelTests.descriptor().files, computeUnits: .cpuOnly,
                                      environmentKey: "X"),
                                  downloader: downloader, compile: DownloadableModelTests.compileDir,
                                  load: load)
        return RemovalModel(model: m, register: { calls.registered.append($0) }, setCacheDir: { calls.dirs.append($0) })
    }

    @Test func startSetsTheCacheAndTheFirstRemovalDownloadsLama() async throws {
        let calls = Calls()
        let files = DownloadableModelTests.Files(["a.bin": DownloadableModelTests.data])
        let r = try make(calls, downloader: files)
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-rm-cache-\(UUID())")
        r.start(cacheDirectory: dir)
        #expect(calls.dirs == [dir.path] && FileManager.default.fileExists(atPath: dir.path))
        #expect(calls.registered.isEmpty && files.downloads == 0, "no download until the user removes something")
        #expect(r.status.contains("217 MB"))
        r.noteRemoveUsed()
        await r.settle()
        #expect(r.registered && calls.registered.count == 1)
        #expect(calls.registered.first??.modelId() == LamaInpainter.modelID)
        #expect(r.status == "Removing with LaMa on this Mac.")
        r.noteRemoveUsed()
        #expect(calls.registered.count == 1)
        #expect(RemovalModel.cacheDirectory.path.hasSuffix("Cache/Removals"))
    }

    @Test func aDownloadedModelIsRegisteredAtLaunch() async throws {
        let calls = Calls()
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-rm-\(UUID())")
        try FileManager.default.createDirectory(at: root.appendingPathComponent("LaMa.mlmodelc"), withIntermediateDirectories: true)
        let r = try make(calls, downloader: DownloadableModelTests.Files([:]), root: root)
        #expect(r.status == "Preparing the removal model…")
        r.start(cacheDirectory: root.appendingPathComponent("cache"))
        await r.settle()
        #expect(r.registered)
    }

    /// LaMa taking over is announced once (the app re-renders the photo on screen), and
    /// bumps the version the grid's thumbnails of edits with removals are keyed by.
    @Test func registrationAnnouncesTheNewEngine() async throws {
        let calls = Calls()
        let r = try make(calls, downloader: DownloadableModelTests.Files(["a.bin": DownloadableModelTests.data]))
        var changes = 0
        r.onEngineChange = { changes += 1 }
        #expect(r.engineVersion == 0)
        r.activate()
        await r.settle()
        #expect(r.registered && r.engineVersion == 1 && changes == 1)
        r.activate() // already registered: nothing
        await r.settle()
        #expect(r.engineVersion == 1 && changes == 1)
    }

    /// An export with removals fills them as previewed: it waits for LaMa on disk (it
    /// registers seconds after launch), and says why when that model doesn't load. With
    /// no model downloaded both fill with PatchMatch (and the export downloads nothing).
    @Test func exportsWaitForTheModelOnDisk() async throws {
        let none = try make(Calls(), downloader: DownloadableModelTests.Files([:], error: URLError(.timedOut)))
        #expect(await none.exportProblem() == nil && !none.registered && none.model.state == .notDownloaded)

        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-rm-\(UUID())")
        try FileManager.default.createDirectory(at: root.appendingPathComponent("LaMa.mlmodelc"), withIntermediateDirectories: true)
        let calls = Calls()
        let onDisk = try make(calls, downloader: DownloadableModelTests.Files([:]), root: root)
        #expect(await onDisk.exportProblem() == nil, "waited for LaMa")
        #expect(onDisk.registered && calls.registered.count == 1)
        #expect(await onDisk.exportProblem() == nil)

        let badRoot = FileManager.default.temporaryDirectory.appendingPathComponent("ds-rm-\(UUID())")
        try FileManager.default.createDirectory(at: badRoot.appendingPathComponent("LaMa.mlmodelc"), withIntermediateDirectories: true)
        let broken = try make(Calls(), downloader: DownloadableModelTests.Files([:]), root: badRoot,
                              load: { _, _ in throw URLError(.cannotDecodeContentData) })
        let why = try #require(await broken.exportProblem())
        #expect(why.hasPrefix("the removal model couldn't be loaded") && !broken.registered)
    }

    /// Cancelling an export stops its wait for a slow model load at once (the load goes on).
    @Test func cancellingAnExportStopsWaitingForTheModel() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-rm-\(UUID())")
        try FileManager.default.createDirectory(at: root.appendingPathComponent("LaMa.mlmodelc"), withIntermediateDirectories: true)
        let slow = try make(Calls(), downloader: DownloadableModelTests.Files([:]), root: root,
                            load: { _, _ in Thread.sleep(forTimeInterval: 3); return StubMLModel() })
        let start = Date()
        let wait = Task { await slow.exportProblem() }
        try await Task.sleep(for: .milliseconds(100))
        wait.cancel()
        let why = await wait.value
        #expect(why == "the export was cancelled")
        #expect(Date().timeIntervalSince(start) < 2 * TestTiming.scale, "didn't wait for the load")
        // Already cancelled before waiting: returns at once too.
        let done = Task<Void, Never> {}
        let early = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            await RemovalModel.wait(for: done)
            return Task.isCancelled
        }
        #expect(await early.value)
    }

    /// The export side: photos with removals fail with the reason, the others export.
    @Test func exportFailsPhotosWhoseRemovalsWouldDiffer() async throws {
        var withRemoval = EditState()
        withRemoval.removals = [RemovalEdit(strokes: [BrushStroke(points: [CGPoint(x: 0.5, y: 0.5)])])]
        let items: [UInt64: ExportJobItem] = [
            1: ExportJobItem(photoID: PhotoID(1), url: URL(fileURLWithPath: "/tmp/DSC1.ARW"), seq: 1, edit: withRemoval),
            2: ExportJobItem(photoID: PhotoID(2), url: URL(fileURLWithPath: "/tmp/DSC2.ARW"), seq: 2, edit: EditState()),
            3: ExportJobItem(photoID: PhotoID(3), url: URL(fileURLWithPath: "/tmp/DSC3.ARW"), seq: 3, edit: nil),
        ]
        var e = RustExportService()
        #expect(await e.removalFailures(items).isEmpty, "no check: as before")
        e.removalCheck = { nil }
        #expect(await e.removalFailures(items).isEmpty)
        e.removalCheck = { "the model is broken" }
        let f = await e.removalFailures(items)
        #expect(f.keys.sorted() == [1])
        #expect(f[1] == "DSC1: not exported. Its removals can't be filled as previewed: the model is broken.")
        #expect(await e.removalFailures([2: items[2]!]).isEmpty, "nothing has removals: no check")
        // Through `export`: the only photo fails before anything renders.
        let job = ExportJob(items: [items[1]!], recipes: [ExportRecipe(name: "Web")], defaultFolder: FileManager.default.temporaryDirectory)
        let out = try await e.export(job) { _ in }
        #expect(out.written.isEmpty && out.failures == [f[1]!])
    }

    /// The photo on screen renders again when the engine changes, if it has removals.
    @Test func theEngineChangeReRendersAPhotoWithRemovals() async throws {
        let m = makeViewerApp()
        await m.start()
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        #expect(await waitUntil { m.viewer.isRenderIdle })
        let before = m.viewer.renderCount
        m.removalEngineChanged()
        try await Task.sleep(for: .milliseconds(50))
        #expect(m.viewer.renderCount == before, "no removals: nothing to render again")
        let id = try #require(m.focusedID)
        m.updateEdits([id], label: "Remove") { _, e in
            e.removals = [RemovalEdit(strokes: [BrushStroke(points: [CGPoint(x: 0.5, y: 0.5)])])]
        }
        #expect(await waitUntil { m.viewer.isRenderIdle && m.viewer.renderCount > before })
        let edited = m.viewer.renderCount
        m.removalEngineChanged()
        #expect(await waitUntil { m.viewer.renderCount > edited })
    }

    /// The grid's developed thumbnail of an edit with removals is keyed by the engine too:
    /// LaMa taking over shows its fills there as well.
    @Test func thumbnailsOfRemovalsFollowTheEngine() {
        let c = ThumbnailCache(provider: MockImageProvider(latency: .zero))
        c.tools = FakeDevelopTools()
        var engine = 0
        c.removalEngine = { engine }
        var shown = EditState()
        shown.exposure.ev = 1
        c.editedSource = { _ in shown }
        let p = samplePhoto()
        let plain = c.fingerprint(p)
        engine = 1
        #expect(c.fingerprint(p) == plain, "no removals: the engine doesn't matter")
        shown.removals = [RemovalEdit(strokes: [BrushStroke(points: [CGPoint(x: 0.5, y: 0.5)])])]
        let before = c.fingerprint(p)
        engine = 2
        #expect(c.fingerprint(p) != before && c.fingerprint(p) != 0)
        #expect(ThumbnailCache(provider: MockImageProvider(latency: .zero)).removalEngine() == RemovalModel.shared.engineVersion)
    }

    @Test func failuresAndCancelKeepPatchMatch() async throws {
        let calls = Calls()
        let bad = try make(calls, downloader: DownloadableModelTests.Files([:], error: URLError(.timedOut)))
        bad.noteRemoveUsed()
        await bad.settle()
        #expect(!bad.registered && bad.status.hasPrefix("Couldn't download"))
        bad.noteRemoveUsed() // failed: the panel's Try Again, not every stroke
        await bad.settle()
        let stalled = try make(calls, downloader: DownloadableModelTests.Stalls())
        stalled.activate()
        #expect(await waitUntil { stalled.model.isDownloading })
        #expect(stalled.status.hasPrefix("Downloading the removal model"))
        stalled.cancel()
        await stalled.settle()
        #expect(!stalled.registered && stalled.model.state == .notDownloaded && calls.registered.isEmpty)
    }

    @Test func theRemoveToolAddsOneRemovalPerStroke() async throws {
        let m = makeViewerApp()
        await m.start()
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        m.viewer.setViewerSize(CGSize(width: 600, height: 400), backingScale: 2)
        var used = 0
        m.colorTools.removal.onRemove = { used += 1 }
        m.colorTools.mode = .remove
        let pane = try #require(m.viewer.layout.main)
        let a = CGPoint(x: pane.image.midX, y: pane.image.midY)
        m.viewer.beginCanvasDrag(at: a)
        m.viewer.updateCanvasDrag(to: CGPoint(x: a.x + 0.1, y: a.y)) // too close: not recorded
        m.viewer.updateCanvasDrag(to: CGPoint(x: a.x + 30, y: a.y + 10))
        #expect(!m.viewer.removeOverlaySegments.isEmpty)
        #expect(m.viewer.overlaySegments.count >= m.viewer.removeOverlaySegments.count)
        m.viewer.endCanvasDrag()
        let r = try #require(m.focusedEdit.removals.first)
        #expect(m.focusedEdit.removals.count == 1 && used == 1)
        #expect(r.strokes.count == 1 && r.strokes[0].points.count == 2 && r.strokes[0].size == 0.04)
        #expect(r.feather == RemovalEdit.defaultFeather && m.history.undoLabel == "Remove")
        #expect(m.colorTools.removal.stroke == nil && m.viewer.removeOverlaySegments.isEmpty)
        // [ and ] size the brush; Esc leaves the tool.
        m.stepBrushSize(1)
        #expect(abs(m.colorTools.removal.size - 0.05) < 1e-9)
        m.removeLastRemoval()
        #expect(m.focusedEdit.removals.isEmpty)
        m.viewer.beginCanvasDrag(at: a)
        m.colorTools.cancel()
        #expect(m.colorTools.removal.stroke == nil && m.colorTools.mode == nil)
        #expect(!m.viewer.removeMove(to: a) && !m.viewer.removeEnd())
        #expect(m.colorTools.removal.removal(points: []) == nil)
    }
}
