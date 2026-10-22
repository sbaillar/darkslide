import CoreGraphics
import DarkslideAI
import DarkslideFFI
import Foundation
import Metal
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Develop tools whose as-shot white balance differs per photo (H4).
final class Rnd9AsShotTools: DevelopTools, @unchecked Sendable {
    private let lock = NSLock()
    var asShot: [PhotoID: WhiteBalanceValue] = [:]
    /// How long each load takes.
    var delay: Duration = .milliseconds(5)
    private var _asked: [PhotoID] = []
    var asked: [PhotoID] { lock.withLock { _asked } }
    /// Loads parked by `hold()` until `release()` (nil: not holding).
    private var held: [CheckedContinuation<Void, Never>]?

    /// Loads wait from now until `release()`: a test can act while they're still loading.
    func hold() { lock.withLock { held = [] } }
    func release() {
        let waiting = lock.withLock { () -> [CheckedContinuation<Void, Never>] in
            defer { held = nil }
            return held ?? []
        }
        waiting.forEach { $0.resume() }
    }

    func info(for photo: Photo) async throws -> PhotoDevelopInfo {
        lock.withLock { _asked.append(photo.id) }
        try await Task.sleep(for: delay) // loading takes a moment
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            let parked = lock.withLock { () -> Bool in
                guard held != nil else { return false }
                held?.append(c)
                return true
            }
            if !parked { c.resume() }
        }
        let wb = lock.withLock { asShot[photo.id] } ?? WhiteBalanceValue(kelvin: 5500, tint: 0)
        return PhotoDevelopInfo(asShot: wb, lens: nil, lensProfile: nil, iso: nil, renderer: "fake")
    }

    func autoWhiteBalance(for photo: Photo) async throws -> WhiteBalanceValue { .init(kelvin: 5000, tint: 0) }
    func autoTone(for photo: Photo, edit: EditState) async throws -> EditState { edit }
    func pickWhiteBalance(for photo: Photo, edit: EditState, at point: CGPoint, sample: Int) async throws -> WhiteBalanceValue {
        .init(kelvin: 5000, tint: 0)
    }

    func editedThumbnail(for photo: Photo, edit: EditState) async throws -> (image: CGImage, cached: Bool) {
        throw DevelopToolsError.unavailable("fake")
    }
}

@MainActor @Suite struct Rnd9PasteTests {
    /// H4, the reviewer's repro: source as-shot 5235 K / −16.5 set to Custom 6282 / −6.5,
    /// target (never visited) as-shot 4971 K / −10.3; the panel's Apply with the
    /// remembered relative WB gives 5966 / −0.3, not the absolute 6282 / −6.5.
    @Test func relativeWhiteBalancePanelApplyLoadsTheTargetsAsShot() async throws {
        let m = makeModel(counts: [6])
        let tools = Rnd9AsShotTools()
        m.tools = tools
        await m.start()
        let ids = Array(m.visibleIDs.prefix(2))
        tools.asShot[ids[0]] = WhiteBalanceValue(kelvin: 5235, tint: -16.5)
        tools.asShot[ids[1]] = WhiteBalanceValue(kelvin: 4971, tint: -10.3)
        m.focus(ids[0], extend: false)
        m.updateEdit(label: "wb") { $0.whiteBalance = .custom(WhiteBalanceValue(kelvin: 6282, tint: -6.5)) }
        m.copyAdjustments()
        m.settings.copyRelativeGroups = [.whiteBalance]
        m.focus(ids[1], extend: false)
        #expect(m.asShot(for: ids[1]) == nil, "the target was never visited")
        #expect(m.applyGroup(.whiteBalance) == 0, "the paste waits for the as-shot values")
        let changed = await m.pendingPaste?.value
        #expect(changed == 1)
        let wb = m.edit(for: ids[1]).whiteBalance
        #expect(!wb.asShot && abs(wb.temperature - 5966) < 1.5 && abs(wb.tint - (-0.3)) < 0.01, "\(wb)")
        #expect(Set(tools.asked).isSuperset(of: ids))
        // Nothing missing any more: the next paste is immediate.
        #expect(m.missingAsShot(for: [ids[1]], options: PasteOptions(groups: [.whiteBalance], relative: [.whiteBalance])).isEmpty)
        // Absolute pastes and pastes without tools never wait.
        #expect(m.missingAsShot(for: [ids[1]], options: PasteOptions(groups: [.whiteBalance], relative: [])).isEmpty)
    }
}

@Suite struct Rnd9ExportQuitTests {
    func tempDir() throws -> URL {
        let d = FileManager.default.temporaryDirectory.appendingPathComponent("rnd9-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        return d
    }

    /// M4: only our temps of dead writers are swept; photos, other hidden files,
    /// symlinks and a live writer's temps stay.
    @Test func staleExportTempsAreSweptAndNothingElse() throws {
        let d = try tempDir()
        defer { try? FileManager.default.removeItem(at: d) }
        let me = String(getpid(), radix: 16)
        let names = [".ds-7fffff0-1.partial", ".ds-7fffff0-a2.partial", ".ds-\(me)-3.partial", "photo.jpg",
                     ".ds-zz-1.partial", ".ds-7fffff0-1.tmp", ".ds-7fffff0.partial", "x.ds-7fffff0-1.partial"]
        for n in names { try Data("x".utf8).write(to: d.appendingPathComponent(n)) }
        try FileManager.default.createSymbolicLink(at: d.appendingPathComponent(".ds-7fffff1-1.partial"),
                                                   withDestinationURL: d.appendingPathComponent("photo.jpg"))
        let removed = ExportPartials.sweep(d, isAlive: { $0 == getpid() })
        #expect(Set(removed) == [".ds-7fffff0-1.partial", ".ds-7fffff0-a2.partial"])
        let left = Set(try FileManager.default.contentsOfDirectory(atPath: d.path))
        #expect(left == Set(names).subtracting(removed).union([".ds-7fffff1-1.partial"]))
        #expect(ExportPartials.writerPID(of: ".ds-1f-2.partial") == 0x1f)
        #expect(ExportPartials.writerPID(of: ".ds--2.partial") == nil)
        #expect(ExportPartials.isRunning(getpid()))
        #expect(ExportPartials.sweep(d.appendingPathComponent("missing")).isEmpty)
    }

    @Test func launchSweepCoversRememberedFolders() async throws {
        let d = try tempDir()
        defer { try? FileManager.default.removeItem(at: d) }
        let defaults = testDefaults()
        try Data("x".utf8).write(to: d.appendingPathComponent(".ds-7ffffe0-1.partial"))
        ExportPartials.remember([d, d], defaults: defaults)
        #expect(defaults.stringArray(forKey: ExportPartials.foldersKey) == [d.path])
        #expect(await ExportPartials.sweepRemembered(defaults: defaults) == 1)
        #expect(defaults.stringArray(forKey: ExportPartials.foldersKey) == nil)
        #expect(await ExportPartials.sweepRemembered(defaults: defaults) == 0)
    }

    @Test func launchSweepKeepsUnreachableFoldersForNextTime() async throws {
        let d = try tempDir()
        defer { try? FileManager.default.removeItem(at: d) }
        let defaults = testDefaults()
        let unmounted = d.appendingPathComponent("Volumes-gone")
        ExportPartials.remember([unmounted, d], defaults: defaults)
        #expect(await ExportPartials.sweepRemembered(defaults: defaults) == 0)
        #expect(defaults.stringArray(forKey: ExportPartials.foldersKey) == [unmounted.path], "only the unreachable folder stays")
        // A live writer's temp is not removed, so its folder is swept again later.
        let busy = d.appendingPathComponent("busy")
        try FileManager.default.createDirectory(at: busy, withIntermediateDirectories: true)
        let live = String(format: ".ds-%x-1.partial", getpid())
        try Data("x".utf8).write(to: busy.appendingPathComponent(live))
        defaults.removeObject(forKey: ExportPartials.foldersKey)
        ExportPartials.remember([busy], defaults: defaults)
        #expect(await ExportPartials.sweepRemembered(defaults: defaults) == 0)
        #expect(defaults.stringArray(forKey: ExportPartials.foldersKey) == [busy.path])
    }

    @Test func quitAlertForARunningExport() {
        let a = TerminationFlow.alert(for: .exportRunning)
        #expect(a?.choices == [.cancel, .stopExportAndQuit] && a?.title == "An export is running")
        #expect(TerminationFlow.reply(for: .stopExportAndQuit) == .terminate)
        #expect(TerminationFlow.Choice.stopExportAndQuit.title == "Stop Export and Quit")
    }
}

/// An export service that runs until cancelled.
final class Rnd9EndlessExport: ExportService, @unchecked Sendable {
    func validate(_ recipes: [ExportRecipe], defaultFolder: URL) -> String? { nil }
    func previewFileName(template: String, sample: ExportFileNameSample, seq: Int, ext: String) -> String? { "x.jpg" }
    func export(_ job: ExportJob, progress: @escaping @Sendable (ExportProgressUpdate) -> Void) async throws -> ExportOutcome {
        while true { try await Task.sleep(for: .milliseconds(10)) }
    }
}

@MainActor @Suite struct Rnd9ExportQuitModelTests {
    /// M4: quitting during an export asks first, then cancels it within the bound.
    @Test func quittingDuringAnExportAsksAndStopsIt() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("rnd9-exp-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        for n in ["a", "b"] { try Data([0]).write(to: dir.appendingPathComponent("\(n).ARW")) }
        let store = FolderSessionStore(recents: RecentFolders(defaults: testDefaults()),
                                       isSupported: { $0.pathExtension == "ARW" },
                                       loader: { url, id in
                                           var p = samplePhoto(id.raw)
                                           p.url = url
                                           return p
                                       })
        let m = makeModel(store: store)
        await m.start()
        await m.openFolder(dir)
        m.exporter = Rnd9EndlessExport()
        m.selection = Set(m.visibleIDs)
        m.beginExport(store: ExportRecipeStore(defaults: testDefaults()))
        let e = try #require(m.exportModel)
        #expect(await waitUntil(timeout: .seconds(10)) { e.canStart })
        e.start()
        #expect(e.isRunning)
        // Its own import registry: other suites import in parallel through `.shared`.
        let imports = ImportRegistry()
        #expect(await m.quitDecision(timeout: TestTiming.bound(.seconds(30)), imports: imports) == .exportRunning)
        let t0 = ContinuousClock.now
        #expect(await m.stopExportsForQuit(timeout: TestTiming.bound(.seconds(10))))
        #expect(ContinuousClock.now - t0 < TestTiming.bound(.seconds(10)))
        #expect(!e.isRunning)
        #expect(await m.quitDecision(timeout: TestTiming.bound(.seconds(30)), imports: imports) == .ok)
    }
}

@Suite struct Rnd9RenderPlumbingTests {
    /// M2: frame memory is recycled; a block comes back when its buffer is released.
    @Test func frameMemoryIsPooled() throws {
        guard let device = MTLCreateSystemDefaultDevice() else { return }
        let pool = FrameMemoryPool(keepPerSize: 2)
        let len = 5_000_000
        for _ in 0..<20 {
            let b = try #require(pool.buffer(length: len, device: device))
            #expect(b.buffer.length >= len)
            memset(b.pointer, 7, len)
        }
        #expect(pool.allocations == 1, "one block, reused 19 times: \(pool.allocations)")
        // Several alive at once need several blocks; only keepPerSize are kept after.
        var alive: [MTLBuffer] = []
        for _ in 0..<4 { alive.append(try #require(pool.buffer(length: len, device: device)).buffer) }
        #expect(pool.allocations == 4)
        alive.removeAll()
        #expect(pool.idle == 2)
        let tiny = try #require(pool.buffer(length: 1, device: device))
        #expect(tiny.buffer.length == Int(getpagesize()))
        #expect(FrameMemoryPool.rounded(Int(getpagesize()) + 1) == 2 * Int(getpagesize()))
    }

    @Test func transientGPUErrorsAreRetriedOthersNot() {
        #expect(ViewerModel.isTransientGPUError(RenderError(.Internal(message: "GPU error: device lost"))))
        #expect(!ViewerModel.isTransientGPUError(RenderError(.Io(message: "gone"))))
        #expect(!ViewerModel.isTransientGPUError(CancellationError()))
    }

    /// A3: the backend is read without blocking; the warm-up creates it off the main thread.
    @Test func backendWarmsUpOffTheMainThread() async {
        _ = DevelopRenderer.backend // never blocks, whatever the state
        await DevelopRenderer.warmUp().value
        let b = DevelopRenderer.backend
        #expect(!b.name.isEmpty && b.name != "GPU (starting)")
    }

    /// L2: a file replaced at the same path with a preserved mtime is a new version.
    @Test func infoCacheFollowsReplacedFiles() throws {
        let d = FileManager.default.temporaryDirectory.appendingPathComponent("rnd9-v-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: d) }
        let a = d.appendingPathComponent("a.ARW"), b = d.appendingPathComponent("b.ARW")
        try Data("one".utf8).write(to: a)
        try Data("two".utf8).write(to: b)
        let before = RustDevelopTools.FileVersion(a)
        let mtime = try FileManager.default.attributesOfItem(atPath: a.path)[.modificationDate] as? Date
        _ = try FileManager.default.replaceItemAt(a, withItemAt: b)
        try FileManager.default.setAttributes([.modificationDate: mtime as Any], ofItemAtPath: a.path)
        #expect(RustDevelopTools.FileVersion(a) != before)
    }
}

/// M5 on the live path: an edited thumbnail with an AI-mask layer asks the app for the
/// mask and shows it.
@MainActor @Suite(.serialized) struct Rnd9LiveThumbnailTests {
    @Test func aiMaskLayersShowInEditedThumbnails() async throws {
        guard let url = sampleARWs().first else { return }
        let cache = FileManager.default.temporaryDirectory.appendingPathComponent("rnd9-thumbs-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: cache) }
        let tools = RustDevelopTools(thumbnailCache: cache)
        let asked = Rnd9Locked<Set<AITarget>>([])
        tools.maskProvider = { _, targets in
            asked.withLock { $0.formUnion(targets) }
            // A full mask (the whole frame is the "subject").
            return Dictionary(uniqueKeysWithValues: targets.map { ($0, VisionMask(width: 4, height: 4, bytes: [UInt8](repeating: 255, count: 16))) })
        }
        var p = samplePhoto(1)
        p.url = url
        var edit = EditState()
        edit.layers = [LayerEdit(id: 0, name: "Subject", exposureEV: 2, mask: MaskEdit(shapes: [.ai(.add, .subject)]))]
        let masked = try await tools.editedThumbnail(for: p, edit: edit)
        #expect(asked.withLock { $0 } == [.subject])
        let plain = try await RustDevelopTools(thumbnailCache: nil).editedThumbnailOrNil(for: p, edit: edit)
        // Without a provider the AI layer is empty: the masked thumbnail is brighter.
        if let plain {
            #expect(Self.meanGreen(masked.image) > Self.meanGreen(plain) + 10)
        }
    }

    static func meanGreen(_ img: CGImage) -> Double {
        let w = img.width, h = img.height
        var px = [UInt8](repeating: 0, count: w * h * 4)
        let ctx = CGContext(data: &px, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        ctx.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
        return stride(from: 1, to: px.count, by: 4).reduce(0.0) { $0 + Double(px[$1]) } / Double(w * h)
    }
}

extension RustDevelopTools {
    /// Test helper: an edited thumbnail from a fresh temp cache, without a mask provider.
    func editedThumbnailOrNil(for photo: Photo, edit: EditState) async throws -> CGImage? {
        let cache = FileManager.default.temporaryDirectory.appendingPathComponent("rnd9-plain-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: cache) }
        return try await RustDevelopTools(thumbnailCache: cache).editedThumbnail(for: photo, edit: edit).image
    }
}

final class Rnd9Locked<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: T
    init(_ v: T) { value = v }
    func withLock<R>(_ body: (inout T) -> R) -> R { lock.withLock { body(&value) } }
}

@MainActor @Suite struct Rnd9ThumbnailCacheTests {
    /// A5: a transient develop failure shows the preview but doesn't pin it under the
    /// edited key (the next request develops again); a replaced file is a new version.
    @Test func failedDevelopsRetryAndReplacedFilesReload() async {
        let c = ThumbnailCache(provider: MockImageProvider(latency: .zero))
        let tools = FakeDevelopTools()
        tools.fail = true
        c.tools = tools
        var edit = EditState()
        edit.exposure.ev = 1
        let shown = edit
        c.editedSource = { _ in shown }
        var p = samplePhoto()
        #expect(await c.image(for: p, bucket: 256) != nil, "the preview while the develop fails")
        #expect(c.cached(p, bucket: 256) == nil, "not cached as the edited thumbnail")
        tools.fail = false
        _ = await c.image(for: p, bucket: 256)
        #expect(c.developedLoads == 2 && tools.thumbs.count == 1, "developed on the next request")
        #expect(c.cached(p, bucket: 256) != nil)
        p.meta.fileSize += 1 // the file was replaced at the same path
        #expect(c.cached(p, bucket: 256) == nil)
        #expect(ThumbnailCache.version(p) != ThumbnailCache.version(samplePhoto()))
    }
}

@Suite struct Rnd9DrawPlanTests {
    /// L6: on an EDR display the surface stays extended-range for 8-bit previews too.
    @Test func edrSurfaceIsStableAcrossPreviewAndDevelopFrames() {
        let view = CGSize(width: 1000, height: 600)
        let layout = ViewerLayout(panes: [.init(role: .main, frame: CGRect(origin: .zero, size: view),
                                                image: CGRect(x: 50, y: 0, width: 900, height: 600), splitX: 500)])
        let preview = RenderedFrame(width: 30, height: 20, rgba: Data(count: 30 * 20 * 4))
        var input = ViewerDrawPlan.Input(layout: layout, viewSize: view, backingScale: 2, main: preview)
        #expect(!ViewerDrawPlan.make(input).edr, "an SDR display keeps an SDR surface")
        input.edrDisplay = true
        let plan = ViewerDrawPlan.make(input)
        #expect(plan.edr && plan.quads.count == 1, "EDR display: the preview draws into the EDR surface")
    }
}
