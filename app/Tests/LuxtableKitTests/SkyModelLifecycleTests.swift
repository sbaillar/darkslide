import CoreGraphics
import LuxtableAI
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// M9: the sky model's lifecycle. Pinned files, a download that can be cancelled, a
/// compiled model that doesn't load is fetched again (never "ready" forever), and an
/// export never silently drops a sky layer. Also: the AI-mask overlay reads the viewer's
/// own cache, and snapshots draw it.
@MainActor
@Suite(.serialized) struct SkyModelLifecycleTests {
    /// Serves in-memory files; counts downloads.
    final class Files: ModelDownloader, @unchecked Sendable {
        let files: [String: Data]
        private let lock = NSLock()
        private var n = 0
        init(_ files: [String: Data]) { self.files = files }
        var downloads: Int { lock.withLock { n } }

        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            lock.withLock { n += 1 }
            guard let data = files[url.lastPathComponent] else { throw URLError(.fileDoesNotExist) }
            try data.write(to: destination)
            progress(Int64(data.count))
        }
    }

    /// Reports some progress, then waits until it is cancelled.
    struct Stalls: ModelDownloader {
        func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
            progress(1)
            while true { try await Task.sleep(for: .milliseconds(5)) }
        }
    }

    static let files = ["a.bin": Data(repeating: 7, count: 256)]

    func root() -> URL { FileManager.default.temporaryDirectory.appendingPathComponent("lt-sky-life-\(UUID())") }

    /// A compile that "succeeds" with a folder Core ML can't load.
    static let unloadable: @Sendable (URL) async throws -> URL = { pkg in
        let out = pkg.deletingLastPathComponent().appendingPathComponent("out.mlmodelc")
        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        try Data("not a model".utf8).write(to: out.appendingPathComponent("model.mil"))
        return out
    }

    static func image() -> CGImage {
        let ctx = CGContext(data: nil, width: 8, height: 8, bitsPerComponent: 8, bytesPerRow: 32,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        return ctx.makeImage()!
    }

    @Test func theSkyModelIsPinnedToACommit() {
        let rev = DownloadableModelDescriptor.skyRevision
        #expect(rev.count == 40 && rev.allSatisfy(\.isHexDigit))
        for f in DownloadableModelDescriptor.sky.files {
            #expect(f.url.absoluteString.contains("/resolve/\(rev)/DETRResnet50SemanticSegmentationF16.mlpackage/\(f.path)"))
            #expect(!f.url.absoluteString.contains("/resolve/main/"))
        }
    }

    @Test func cancellingStopsTheDownloadAndKeepsNothing() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = SkySegmentation(root: dir, descriptor: Lay9SkyTests.descriptor(Self.files), downloader: Stalls(), compile: { $0 })
        let run = Task { try await s.prepare() }
        #expect(await waitUntil { if case .downloading = s.state { true } else { false } })
        s.cancel()
        do {
            _ = try await run.value
            Issue.record("expected a cancellation")
        } catch {
            #expect(error is CancellationError, "\(error)")
        }
        #expect(s.state == .notDownloaded, "a cancel isn't a failure")
        let left = (try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? []
        #expect(left.isEmpty, "no staging folder or model left: \(left)")
        s.cancel() // nothing running: harmless
        // The next attempt starts over and works.
        let again = SkySegmentation(root: dir, descriptor: Lay9SkyTests.descriptor(Self.files), downloader: Files(Self.files),
                                    compile: { pkg in
                                        let out = pkg.deletingLastPathComponent().appendingPathComponent("out.mlmodelc")
                                        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
                                        return out
                                    })
        _ = try await again.prepare()
        #expect(again.state == .ready)
    }

    @Test func urlSessionDownloadStopsWhenItsTaskIsCancelled() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let src = dir.appendingPathComponent("src.bin"), out = dir.appendingPathComponent("out.bin")
        try Data(repeating: 1, count: 4096).write(to: src)
        let run = Task {
            withUnsafeCurrentTask { $0?.cancel() } // cancelled before the download task starts
            try await URLSessionModelDownloader(schemes: ["file"]).download(src, to: out) { _ in }
        }
        await #expect(throws: CancellationError.self) { try await run.value }
        #expect(!FileManager.default.fileExists(atPath: out.path))
    }

    @Test func aCompiledModelThatDoesntLoadIsFetchedAgainThenReported() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let downloader = Files(Self.files)
        let s = SkySegmentation(root: dir, descriptor: Lay9SkyTests.descriptor(Self.files), downloader: downloader,
                                compile: Self.unloadable)
        // A half-copied compiled model from an earlier run.
        try FileManager.default.createDirectory(at: s.compiledURL, withIntermediateDirectories: true)
        try Data("garbage".utf8).write(to: s.compiledURL.appendingPathComponent("coremldata.bin"))
        let fresh = SkySegmentation(root: dir, descriptor: Lay9SkyTests.descriptor(Self.files), downloader: downloader,
                                    compile: Self.unloadable)
        #expect(fresh.state == .ready, "it exists, so it looks ready until it is used")
        do {
            _ = try await fresh.mask(for: Self.image())
            Issue.record("expected an error")
        } catch let e as ModelDownloadError {
            guard case .compile = e else { Issue.record("\(e)"); return }
        }
        #expect(downloader.downloads == 0, "the damaged copy is deleted; fetching it anew is the user's Try Again")
        #expect(!FileManager.default.fileExists(atPath: fresh.compiledURL.path), "an unloadable model isn't kept")
        guard case .failed = fresh.state else { Issue.record("\(fresh.state)"); return }
        // The next launch doesn't think it's ready.
        #expect(SkySegmentation(root: dir, descriptor: Lay9SkyTests.descriptor(Self.files)).state == .notDownloaded)
    }

    /// No sky in the photo: an empty mask (rendered as nothing). The mask can't be made:
    /// left out, so export can tell the two apart.
    @Test func regeneratedMasksTellNoSkyFromNoModel() async throws {
        let m = makeViewerApp()
        m.viewer.masks = MaskOverlayCache()
        await m.start()
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        let id = try #require(m.focusedID)
        m.viewer.masker = AIMasker { _, _ in nil }
        let none = await m.viewer.masksForRender(photo: id, targets: [.sky])
        #expect(none[.sky].map { $0.coverage == 0 } == true, "no sky: an empty mask")
        struct NoModel: Error {}
        m.viewer.masks = MaskOverlayCache()
        m.viewer.masker = AIMasker { _, _ in throw NoModel() }
        #expect(await m.viewer.masksForRender(photo: id, targets: [.sky]).isEmpty, "no model: missing")
    }

    @Test func exportReportsAPhotoWhoseSkyMaskCantBeMade() async throws {
        var sky = EditState()
        sky.layers = [LayerEdit(id: 1, name: "Sky", exposureEV: -1, mask: MaskEdit(shapes: [.ai(.add, .sky)]))]
        var s = RustExportService()
        s.maskProvider = { _, _ in [:] } // the model can't be fetched
        let out = FileManager.default.temporaryDirectory.appendingPathComponent("lt-sky-export-\(UUID())")
        defer { try? FileManager.default.removeItem(at: out) }
        let url = URL(fileURLWithPath: "/nonexistent/DSC0001.ARW")
        let recipe = ExportRecipe(name: "Web", sizeMode: .longEdge, sizeValue: 400, fileNameTemplate: "{seq}")
        let o = try await s.export(ExportJob(items: [ExportJobItem(photoID: PhotoID(1), url: url, seq: 1, edit: sky)],
                                             recipes: [recipe], defaultFolder: out)) { _, _ in }
        #expect(o.written.isEmpty && o.failures.count == 1)
        #expect(o.failures.first?.hasPrefix("DSC0001: not exported") == true && o.failures.first?.contains("sky model") == true,
                "\(o.failures)")
        #expect(!FileManager.default.fileExists(atPath: out.path), "nothing written")
        let other = RustExportService.maskFailure(ExportJobItem(photoID: PhotoID(2), url: url, seq: 1), targets: [.subject, .person])
        #expect(other.contains("person and subject mask") && other.contains("Vision"))
        // A photo with no sky (an empty mask) isn't missing anything.
        s.maskProvider = { _, targets in Dictionary(uniqueKeysWithValues: targets.map { ($0, VisionMask(width: 1, height: 1, bytes: [0])) }) }
        let (masks, missing) = await s.gatherMasks([1: ExportJobItem(photoID: PhotoID(1), url: url, seq: 1, edit: sky)])
        #expect(masks != nil && missing.isEmpty)

        // With a sample: the photo without AI layers still exports; the one with the sky layer is reported.
        guard let dir = try sampleFolderCopy(limit: 1) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let raw = try #require(try FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
            .first { $0.pathExtension == "ARW" })
        s.maskProvider = { _, _ in [:] }
        let mixed = try await s.export(ExportJob(
            items: [ExportJobItem(photoID: PhotoID(1), url: raw, seq: 1, edit: sky),
                    ExportJobItem(photoID: PhotoID(2), url: raw, seq: 2)],
            recipes: [recipe], defaultFolder: dir.appendingPathComponent("Output"))) { _, _ in }
        #expect(mixed.written.map(\.photoID) == [PhotoID(2)] && mixed.failures.count == 1, "\(mixed.failures)")
        #expect(mixed.failures.first?.hasPrefix(raw.deletingPathExtension().lastPathComponent) == true)
    }

    /// Adding an AI mask stores its bitmap in the viewer's own cache (what the overlay and
    /// renders read), and the snapshot layer's tint shows it.
    @Test func anAddedSkyMaskShowsOnTheSelectedLayer() async throws {
        let m = makeViewerApp()
        let cache = MaskOverlayCache()
        m.viewer.masks = cache
        await m.start()
        #expect(await waitUntil { m.viewer.displayFrame != nil })
        let top = VisionMask(width: 4, height: 4, bytes: (0..<16).map { $0 < 8 ? 255 : 0 })
        let layer = try await m.addAIMask(.sky, toLayer: nil, masker: AIMasker { _, _ in top })
        LayerController(app: m).select(layer)
        #expect(cache.mask(photo: m.focusedID!, target: .sky) == top, "the viewer's cache, not only .shared")
        m.viewer.setViewerSize(CGSize(width: 600, height: 400), backingScale: 2)
        let plan = m.viewer.drawPlan(loupeAt: nil)
        #expect(plan.quads.first?.masks == [MaskOverlayCache.Key(photo: m.focusedID!, target: .sky)])
        let tint = try #require(SnapshotFrameLayer.tint(top))
        #expect(tint.width == 4 && tint.height == 4 && tint.alphaInfo == .premultipliedLast)
        #expect(SnapshotFrameLayer.tint(VisionMask(width: 2, height: 2, bytes: [0])) == nil, "short buffer")
    }
}
