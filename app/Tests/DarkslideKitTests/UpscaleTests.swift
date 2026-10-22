import CoreML
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// The 2× model double: every output pixel is its input pixel (nearest neighbour), times
/// `gain`; optionally slowed or held per tile (to cancel mid-run) or failing.
struct NearestModel: DenoiseTileModel {
    var gain: Float16 = 1
    var delay: Double = 0
    var gate: TileGate?
    var fail = false

    func denoise(_ input: MLMultiArray) throws -> MLMultiArray {
        if delay > 0 { Thread.sleep(forTimeInterval: delay) }
        gate?.pass()
        if fail { throw DenoiseError.model("boom") }
        let t: Int = input.shape[3].intValue
        let o: Int = 2 * t
        let out = try MLMultiArray(shape: [1, 3, NSNumber(value: o), NSNumber(value: o)], dataType: .float16)
        let gain = gain
        input.withUnsafeBytes { src in
            out.withUnsafeMutableBytes { dst, _ in
                let s: UnsafeBufferPointer<Float16> = src.bindMemory(to: Float16.self)
                let d: UnsafeMutableBufferPointer<Float16> = dst.bindMemory(to: Float16.self)
                for c in 0..<3 {
                    for y in 0..<o {
                        let from: Int = c * t * t + (y / 2) * t
                        let to: Int = c * o * o + y * o
                        for x in 0..<o {
                            let v: Float16 = s[from + x / 2]
                            d[to + x] = v * gain
                        }
                    }
                }
            }
        }
        return out
    }
}

/// Holds every tile until opened, counting the tiles that reached it.
final class TileGate: @unchecked Sendable {
    private let go = DispatchSemaphore(value: 0)
    private let lock = NSLock()
    private var reached = 0
    var entered: Int { lock.withLock { reached } }
    func pass() {
        lock.withLock { reached += 1 }
        go.wait()
        go.signal() // once open, open for every tile after
    }
    func open() { go.signal() }
}

/// Fake photo tiles: inputs are `index + 1`, outputs `2·size` square; `finish()` "writes"
/// the DNG at `dng` (when the run finished every tile).
final class FakeUpscaleTiles: UpscalePhotoTiles, @unchecked Sendable {
    let count: Int
    let size: Int
    let dng: URL
    var readDelay: Double = 0
    /// How long `finish` takes (it can't be cancelled once begun, like the real one).
    var finishDelay: Double = 0
    /// `finish` writes a file at `dng`.
    var writesFile = false
    private let lock = NSLock()
    private var written: [Int: Float16] = [:]
    private var placed: URL?

    init(count: Int, size: Int = 4, dng: URL) {
        self.count = count
        self.size = size
        self.dng = dng
    }

    var result: URL? { lock.withLock { placed } }
    var outputs: [Int: Float16] { lock.withLock { written } }

    func tileCount() throws -> UInt32 { UInt32(count) }
    func tileSize() throws -> UInt32 { UInt32(size) }

    func readTile(index: UInt32, destPtr: UInt64, destLen: UInt64) throws {
        if readDelay > 0 { Thread.sleep(forTimeInterval: readDelay) }
        let p = UnsafeMutablePointer<Float16>(bitPattern: UInt(destPtr))!
        for i in 0..<Int(destLen) { p[i] = Float16(Float(index) + 1) }
    }

    func writeTile(index: UInt32, srcPtr: UInt64, srcLen: UInt64, rowStride: UInt64, channelStride: UInt64) throws {
        #expect(Int(srcLen) >= 3 * 4 * size * size, "a 2× tile")
        #expect(Int(rowStride) == 2 * size)
        let p = UnsafePointer<Float16>(bitPattern: UInt(srcPtr))!
        let last = p[2 * Int(channelStride) + (2 * size - 1) * Int(rowStride) + 2 * size - 1]
        lock.withLock { written[Int(index)] = last }
    }

    func finish() async throws {
        if finishDelay > 0 { usleep(UInt32(finishDelay * 1_000_000)) } // blocking: not cancellable
        guard lock.withLock({ written.count == count }) else { return }
        if writesFile { FileManager.default.createFile(atPath: dng.path, contents: Data("dng".utf8)) }
        lock.withLock { placed = dng }
    }
}

/// Fake Rust backend: tiles per photo, optional failures, and how many runs began.
final class FakeUpscaleBackend: UpscaleBackend, @unchecked Sendable {
    var tileCount = 3
    var readDelay: Double = 0
    var failBegin: Set<URL> = []
    var noResult = false
    var finishDelay: Double = 0
    /// Where the DNGs go (and are written as files); nil: next to the photo, not written.
    var outDir: URL?
    private let lock = NSLock()
    private var urls: [URL] = []
    private var given: [String] = []
    var begun: [URL] { lock.withLock { urls } }
    var names: [String] { lock.withLock { given } }

    func begin(_ url: URL, name: String) async throws -> any UpscalePhotoTiles {
        lock.withLock {
            urls.append(url)
            given.append(name)
        }
        if failBegin.contains(url) { throw UpscaleError.failed("unreadable \(url.lastPathComponent)") }
        let dng = (outDir ?? url.deletingLastPathComponent()).appendingPathComponent(name)
        let t = FakeUpscaleTiles(count: tileCount, dng: noResult ? URL(fileURLWithPath: "/") : dng)
        t.readDelay = readDelay
        t.finishDelay = finishDelay
        t.writesFile = outDir != nil
        if noResult { return NoResult(t) }
        return t
    }

    /// Tiles whose finish places nothing.
    final class NoResult: UpscalePhotoTiles, @unchecked Sendable {
        let inner: FakeUpscaleTiles
        init(_ inner: FakeUpscaleTiles) { self.inner = inner }
        var result: URL? { nil }
        func tileCount() throws -> UInt32 { try inner.tileCount() }
        func tileSize() throws -> UInt32 { try inner.tileSize() }
        func readTile(index: UInt32, destPtr: UInt64, destLen: UInt64) throws {
            try inner.readTile(index: index, destPtr: destPtr, destLen: destLen)
        }
        func writeTile(index: UInt32, srcPtr: UInt64, srcLen: UInt64, rowStride: UInt64, channelStride: UInt64) throws {
            try inner.writeTile(index: index, srcPtr: srcPtr, srcLen: srcLen, rowStride: rowStride, channelStride: channelStride)
        }
        func finish() async throws {}
    }
}

/// The mock session with file URLs that takes derived photos (what the Rust store does).
actor DerivingStore: SessionStore, DerivedPhotoAdding {
    let inner: MockSessionStore
    init(counts: [Int] = [6]) { inner = MockSessionStore(counts: counts) }
    private var extra: [Photo] = []
    private var editsByID: [PhotoID: EditState] = [:]
    private(set) var derived: [(URL, PhotoID)] = []
    var failAdd = false
    /// Names the session holds (a photo whose file is gone).
    var taken: Set<String> = []

    func setFailAdd(_ on: Bool) { failAdd = on }
    func setTaken(_ names: Set<String>) { taken = names }

    func freeDerivedName(for url: URL, from source: PhotoID, suffix: String) async throws -> String {
        let stem = url.deletingPathExtension().lastPathComponent
        var n = 0
        while true {
            let name = n == 0 ? "\(stem)-\(suffix).dng" : "\(stem)-\(suffix)-\(n).dng"
            if !taken.contains(name) { return name }
            n += 1
        }
    }

    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] {
        try await inner.photos(in: session).map { p in
            var p = p
            p.url = URL(fileURLWithPath: "/photos/\(p.filename)")
            return p
        } + extra
    }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws {}
    func edit(for id: PhotoID) async throws -> EditState { editsByID[id] ?? EditState() }
    func save(edits: [PhotoID: EditState]) async throws { editsByID.merge(edits) { $1 } }
    func edits(in session: SessionInfo) async throws -> [PhotoID: EditState] { editsByID }

    func addDerived(_ url: URL, from source: PhotoID) async throws -> PhotoID {
        try Task.checkCancellation() // as the Rust store's calls do
        if failAdd { throw MockError.invalid("disk full") }
        derived.append((url, source))
        let id = PhotoID(90_000 + Int64(derived.count))
        let meta = PhotoMeta(camera: "Sony ILCE-9", lens: "", iso: 100, shutter: 0.01, aperture: 2.8, focalLength: 35,
                             captureDate: Date(timeIntervalSince1970: 1_790_000_000), pixelWidth: 12_000, pixelHeight: 8_000,
                             fileSize: 1)
        extra.append(Photo(id: id, filename: url.lastPathComponent, meta: meta, url: url))
        editsByID[id] = editsByID[source] ?? EditState()
        return id
    }
}

@Suite struct UpscaleEngineTests {
    /// The shared tile engine with 2× outputs: every tile through the model once, read back
    /// through the output's own (larger) shape and strides.
    @Test func twoTimesTilesRunThroughTheEngine() async throws {
        let tiles = FakeUpscaleTiles(count: 7, dng: URL(fileURLWithPath: "/tmp/x.dng"))
        try await DenoiseEngine.run(tiles, models: [NearestModel(gain: 2), NearestModel(gain: 2)], scale: 2)
        #expect(tiles.outputs.count == 7)
        for (i, last) in tiles.outputs { #expect(last == Float16(Float(i + 1) * 2)) }
        #expect(tiles.result != nil, "finished")
        // A 1× output for a 2× run is rejected, nothing finished.
        let wrong = FakeUpscaleTiles(count: 2, dng: URL(fileURLWithPath: "/tmp/x.dng"))
        await #expect(throws: DenoiseError.self) { try await DenoiseEngine.run(wrong, models: [DoublingModel()], scale: 2) }
        #expect(wrong.result == nil)
    }

    @Test func progressReadsAsTheToolbarSaysIt() {
        #expect(UpscaleService.Progress(photo: 2, photos: 3, fraction: 0.416).status == "Upscaling 2 of 3, 42%")
        #expect(UpscaleService.Progress(photo: 1, photos: 1, fraction: 1).status == "Upscaling, 100%")
    }

    @Test func errorsDescribeThemselves() {
        #expect(UpscaleError.unavailable("No model.").errorDescription == "No model.")
        #expect(UpscaleError.failed("x").errorDescription == "AI Upscale failed: x")
    }

    /// The pinned checksum in the app and the conversion script are the same, and the
    /// bundled package is found only when the build included it.
    @Test func theAppPinsTheChecksumTheConversionProduces() throws {
        let script = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../../scripts/upscale-model/convert.py").standardizedFileURL
        let text = try String(contentsOf: script, encoding: .utf8)
        #expect(text.contains("EXPECTED_WEIGHTS_SHA256 = \"\(UpscaleModelDescriptor.weightsSHA256)\""))
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-up-bundle-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        #expect(UpscaleModelDescriptor.bundledPackage(in: try #require(Bundle(url: dir))) == nil)
        try FileManager.default.createDirectory(at: dir.appendingPathComponent("Models/DarkslideUpscale.mlpackage"),
                                                withIntermediateDirectories: true)
        #expect(UpscaleModelDescriptor.bundledPackage(in: try #require(Bundle(url: dir)))?.lastPathComponent
            == "DarkslideUpscale.mlpackage")
    }
}

@MainActor
@Suite(.serialized) struct UpscaleServiceTests {
    func root() -> URL { FileManager.default.temporaryDirectory.appendingPathComponent("ds-up-svc-\(UUID())") }

    /// A store with a fake bundled package (compile = copy), under its own name and reasons.
    func store(_ root: URL, bundled: Bool = true, pinned: String? = nil) -> DenoiseModelStore {
        let weights = Data(repeating: 7, count: 64)
        let pkg = root.appendingPathComponent("bundle/DarkslideUpscale.mlpackage")
        let w = pkg.appendingPathComponent(DenoiseModelDescriptor.weightsPath)
        try? FileManager.default.createDirectory(at: w.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? weights.write(to: w)
        return DenoiseModelStore(package: bundled ? pkg : nil, compiledRoot: root.appendingPathComponent("compiled"),
                                 expectedWeights: pinned ?? DenoiseGlobalStore.DenoiseServiceTests.sha(weights),
                                 name: UpscaleModelDescriptor.name, reasons: UpscaleModelDescriptor.reasons,
                                 compile: { pkg in
            let out = root.appendingPathComponent("tmp-\(UUID()).mlmodelc")
            try FileManager.default.copyItem(at: pkg, to: out)
            return out
        })
    }

    func service(_ backend: FakeUpscaleBackend, root: URL, model: DenoiseModelStore? = nil,
                 gate: TileGate? = nil, failLoads: Int = 0, loads: DoublingModel.Counter? = nil) -> UpscaleService {
        let failures = DoublingModel.Counter()
        return UpscaleService(model: model ?? store(root), backend: backend) { _, _ in
            loads?.add()
            failures.add()
            if failures.value <= failLoads { throw DenoiseError.model("damaged") }
            return NearestModel(gate: gate)
        }
    }

    func app(_ s: UpscaleService, store: any SessionStore = DerivingStore()) async -> AppModel {
        let m = makeModel(store: store)
        m.toastSleep = { _ in try await Task.sleep(for: .seconds(3600)) } // toasts stay to be read
        await m.start()
        s.app = m
        m.upscaler = s
        return m
    }

    @Test func selectedPhotosBecomeNewPhotosWithTheirEditsAndTheLastIsShown() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let backend = FakeUpscaleBackend()
        let loads = DoublingModel.Counter()
        let s = service(backend, root: dir, loads: loads)
        let store = DerivingStore()
        let m = await app(s, store: store)
        let first = m.visibleIDs[0], second = m.visibleIDs[1]
        m.focus(first, extend: false)
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 0.6 }
        m.focus(second, extend: true)
        #expect(CommandCatalog.title(.upscale, model: m) == "Upscale 2 Photos 2×")
        #expect(CommandCatalog.isEnabled(.upscale, model: m))
        #expect(CommandCatalog.help(.upscale, model: m)?.contains("twice the size") == true)
        CommandCatalog.perform(.upscale, model: m)
        #expect(s.isRunning && s.progress?.photos == 2)
        #expect(CommandCatalog.title(.upscale, model: m) == "Cancel Upscale")
        #expect(CommandCatalog.help(.upscale, model: m)?.contains("Stop") == true)
        await s.settle()
        #expect(!s.isRunning && s.progress == nil)
        let derived = await store.derived
        #expect(derived.map(\.1) == [first, second], "one after another, in order")
        #expect(derived.map(\.0.lastPathComponent).allSatisfy { $0.hasSuffix("-Upscaled.dng") })
        #expect(backend.begun.count == 2)
        #expect(loads.value == 2, "a GPU and a Neural Engine instance, loaded once")
        // Listed, starting with the source's edit (saved first), and the last one is shown.
        let shown = try #require(m.focusedPhoto)
        #expect(shown.filename == derived[1].0.lastPathComponent)
        let firstNew = try #require(m.visibleIDs.first { m.photo($0)?.filename == derived[0].0.lastPathComponent })
        #expect(m.edit(for: firstNew).exposure.ev == 0.6)
        #expect(m.toast?.message.contains("Upscaled 2×: 2 new photos") == true)
        // One photo: named in the toast.
        m.focus(first, extend: false)
        s.toggle()
        await s.settle()
        #expect(m.toast?.message == "Upscaled 2×: \(derived[0].0.lastPathComponent)")
    }

    @Test func cancellingStopsTheRunAndAddsNothingMore() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let backend = FakeUpscaleBackend()
        backend.tileCount = 100
        let gate = TileGate() // the first tile is held until the cancel: no race with the run
        let s = service(backend, root: dir, gate: gate)
        let store = DerivingStore()
        let m = await app(s, store: store)
        s.toggle()
        #expect(await waitUntil(timeout: .seconds(60)) { gate.entered > 0 }) // liveness only: the gate holds it
        CommandCatalog.perform(.upscale, model: m) // "Cancel Upscale"
        gate.open() // a run that ignored the cancel would now finish and add the photo
        await s.settle()
        #expect(!s.isRunning)
        #expect(await store.derived.isEmpty, "the photo in progress leaves nothing")
        #expect(m.toast == nil)
        s.cancel() // idle: harmless
    }

    @Test func unavailableUpscaleSaysWhy() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        // No model in this build.
        let missing = service(FakeUpscaleBackend(), root: dir, model: store(dir, bundled: false))
        let m = await app(missing)
        #expect(missing.unavailableReason(in: m) == UpscaleModelDescriptor.reasons.missing)
        #expect(!CommandCatalog.isEnabled(.upscale, model: m))
        #expect(CommandCatalog.help(.upscale, model: m) == UpscaleModelDescriptor.reasons.missing)
        missing.toggle()
        #expect(!missing.isRunning)
        await #expect(throws: UpscaleError.unavailable(UpscaleModelDescriptor.reasons.missing)) {
            _ = try await missing.upscale(URL(fileURLWithPath: "/photos/a.ARW"), name: "a-Upscaled.dng")
        }
        // A store that can't take new photos; no selection.
        let s = service(FakeUpscaleBackend(), root: dir)
        let plain = await app(s, store: URLStore())
        #expect(s.unavailableReason(in: plain)?.contains("can’t take new photos") == true)
        let m2 = await app(s)
        m2.focus(nil, extend: false)
        m2.selection = []
        #expect(s.unavailableReason(in: m2) == "Select a photo to upscale.")
        #expect(s.title(in: m2) == "Upscale 2×")
        // No upscaler at all (demo, tests): off, with the reason.
        let bare = makeModel()
        #expect(CommandCatalog.title(.upscale, model: bare) == "Upscale 2×")
        #expect(!CommandCatalog.isEnabled(.upscale, model: bare))
        #expect(CommandCatalog.help(.upscale, model: bare) == UpscaleModelDescriptor.reasons.missing)
        CommandCatalog.perform(.upscale, model: bare)
    }

    @Test func aDamagedModelIsRefusedWithItsReason() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let s = service(FakeUpscaleBackend(), root: dir, model: store(dir, pinned: String(repeating: "0", count: 64)))
        await #expect(throws: UpscaleError.unavailable(UpscaleModelDescriptor.reasons.damaged)) {
            _ = try await s.upscale(URL(fileURLWithPath: "/photos/a.ARW"), name: "a-Upscaled.dng")
        }
        #expect(s.model.unavailableReason == UpscaleModelDescriptor.reasons.damaged)
        await #expect(throws: UpscaleError.self) { _ = try await s.upscaleForExport(FakeTiles(count: 1)) }
    }

    @Test func failuresStopTheRunAndAreShown() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let backend = FakeUpscaleBackend()
        let s = service(backend, root: dir)
        let store = DerivingStore()
        let m = await app(s, store: store)
        let url = try #require(m.focusedPhoto?.url)
        backend.failBegin = [url]
        s.toggle()
        await s.settle()
        #expect(m.toast?.isError == true && m.toast?.message.contains("unreadable") == true)
        #expect(await store.derived.isEmpty)
        // The session refuses the new photo.
        backend.failBegin = []
        await store.setFailAdd(true)
        s.toggle()
        await s.settle()
        #expect(m.toast?.message.contains("disk full") == true)
        // The run wrote no file.
        backend.noResult = true
        await #expect(throws: UpscaleError.failed("no file was written")) { _ = try await s.upscale(url, name: "x.dng") }
    }

    @Test func aModelThatDoesntLoadIsCompiledAgainOnce() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let loads = DoublingModel.Counter()
        let s = service(FakeUpscaleBackend(), root: dir, failLoads: 1, loads: loads)
        _ = try await s.upscale(URL(fileURLWithPath: "/photos/a.ARW"), name: "a-Upscaled.dng")
        #expect(loads.value == 3, "one failed load, then both instances")
        let s2 = service(FakeUpscaleBackend(), root: dir, failLoads: 10)
        await #expect(throws: DenoiseError.self) { _ = try await s2.upscale(URL(fileURLWithPath: "/photos/a.ARW"), name: "a-Upscaled.dng") }
    }

    @Test func exportsRunTheModelOnTheNeuralEngineOrResample() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let units = UnitsSeen()
        let s = UpscaleService(model: store(dir), backend: FakeUpscaleBackend()) { _, u in
            units.add(u)
            return NearestModel()
        }
        let tiles = FakeUpscaleTiles(count: 3, dng: dir)
        #expect(try await s.upscaleForExport(tiles))
        #expect(tiles.outputs.count == 3)
        #expect(units.all == [.cpuAndNeuralEngine], "the GPU stays the viewer's")
        let none = service(FakeUpscaleBackend(), root: dir, model: store(dir, bundled: false))
        #expect(try await !none.upscaleForExport(FakeUpscaleTiles(count: 1, dng: dir)), "no model: resampled")
        // The sheet says what the toggle does, and why it can't.
        let m = await app(s)
        m.beginExport(store: ExportRecipeStore(defaults: UserDefaults(suiteName: testSuiteName())!))
        #expect(m.exportModel?.aiUpscaleUnavailable == nil)
        #expect(m.exportModel?.aiUpscaleHelp.contains("upscaled 2×") == true)
        m.endExport()
        m.upscaler = nil
        m.beginExport(store: ExportRecipeStore(defaults: UserDefaults(suiteName: testSuiteName())!))
        #expect(m.exportModel?.aiUpscaleHelp.contains("resampled") == true)
        m.endExport()
    }

    final class UnitsSeen: @unchecked Sendable {
        private let lock = NSLock()
        private var seen: [MLComputeUnits] = []
        func add(_ u: MLComputeUnits) { lock.withLock { seen.append(u) } }
        var all: [MLComputeUnits] { lock.withLock { seen } }
    }

    /// Review U-M1: a cancel while the DNG is being finished (once begun, the file gets
    /// placed) still adds the photo: no file is left behind, unlisted.
    @Test func aCancelDuringFinishStillAddsThePlacedPhoto() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let backend = FakeUpscaleBackend()
        backend.finishDelay = 0.5
        let s = service(backend, root: dir)
        let store = DerivingStore()
        let m = await app(s, store: store)
        s.toggle()
        #expect(await waitUntil { (s.progress?.fraction ?? 0) >= 1 }, "every tile done: finishing")
        s.cancel()
        await s.settle()
        #expect(await store.derived.count == 1, "the placed DNG is in the session")
        #expect(m.toast?.isError != true)
    }

    /// A run in flight over a session switch adds nothing: the store would resolve the
    /// source id in the other session.
    @Test func aRunInFlightOverASessionSwitchAddsNothing() async throws {
        let dir = root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let backend = FakeUpscaleBackend()
        backend.finishDelay = 1
        let s = service(backend, root: dir)
        let store = DerivingStore(counts: [6, 4])
        let m = await app(s, store: store)
        s.toggle()
        #expect(await waitUntil { (s.progress?.fraction ?? 0) >= 1 }, "every tile done: finishing")
        await m.open(m.sessionList[1])
        await s.settle()
        #expect(await store.derived.isEmpty, "nothing was added to the session that was switched to")
        #expect(m.toast?.message.contains("Upscaled") != true)
    }

    /// Review U-M2: the name comes from the session (a photo whose file is gone keeps
    /// its name); a refused photo's file is removed.
    @Test func theNameIsTheSessionsAndARefusedPhotosFileIsRemoved() async throws {
        let dir = root()
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let backend = FakeUpscaleBackend()
        backend.outDir = dir
        let s = service(backend, root: dir)
        let store = DerivingStore()
        let m = await app(s, store: store)
        let stem = try #require(m.focusedPhoto?.url).deletingPathExtension().lastPathComponent
        await store.setTaken(["\(stem)-Upscaled.dng"])
        s.toggle()
        await s.settle()
        #expect(backend.names == ["\(stem)-Upscaled-1.dng"])
        #expect(await store.derived.first?.0.lastPathComponent == "\(stem)-Upscaled-1.dng")
        await store.setFailAdd(true)
        s.toggle()
        await s.settle()
        #expect(m.toast?.message.contains("disk full") == true)
        let left = try FileManager.default.contentsOfDirectory(atPath: dir.path).filter { $0.hasSuffix(".dng") }
        #expect(left == ["\(stem)-Upscaled-1.dng"], "the refused photo's DNG is gone: \(left)")
    }

    @Test func recipesCarryAIUpscale() throws {
        var r = ExportRecipe(name: "Big", sizeMode: .percent, sizeValue: 200)
        #expect(!r.aiUpscale && !r.summary.contains("AI Upscale"))
        r.aiUpscale = true
        #expect(r.summary.contains("AI Upscale"))
        let back = try JSONDecoder().decode(ExportRecipe.self, from: JSONEncoder().encode(r))
        #expect(back.aiUpscale)
        var old = try JSONSerialization.jsonObject(with: JSONEncoder().encode(r)) as! [String: Any]
        old["aiUpscale"] = nil
        let legacy = try JSONDecoder().decode(ExportRecipe.self, from: JSONSerialization.data(withJSONObject: old))
        #expect(!legacy.aiUpscale, "recipes saved before it existed: off")
        #expect(RustExportService.spec(r, defaultFolder: URL(fileURLWithPath: "/tmp")).aiUpscale)
    }
}

/// A download that never finishes until cancelled.
private struct StalledDownloader: ModelDownloader {
    func download(_ url: URL, to destination: URL, progress: @escaping @Sendable (Int64) -> Void) async throws {
        progress(1)
        try await Task.sleep(for: .seconds(3600))
    }
}

/// [osscleanup] AI Upscale's model downloaded on first use: pinned files, checked, installed
/// in one rename; the menu offers it, and Upscale 2× works once it's there.
@MainActor
@Suite(.serialized) struct UpscaleDownloadTests {
    let base = URL(string: "https://models.example.invalid/x/DarkslideUpscale.mlpackage/")!

    static let manifest = Data(#"{"itemInfoEntries": {"A": {"path": "com.apple.CoreML/model.mlmodel"}}, "rootModelIdentifier": "A"}"#.utf8)

    /// A published package (three files) and its file list: the model spec and weights
    /// pinned, the manifest checked for its shape (like `UpscaleModelDescriptor.files`).
    func published(_ dir: URL, damage: Bool = false, manifest: Data = manifest) throws -> (URL, [DownloadableModelDescriptor.File]) {
        let pkg = dir.appendingPathComponent("published/DarkslideUpscale.mlpackage")
        let contents = ["Manifest.json": manifest, "Data/com.apple.CoreML/model.mlmodel": Data(repeating: 1, count: 10),
                        DenoiseModelDescriptor.weightsPath: Data(repeating: 7, count: 64)]
        var files: [DownloadableModelDescriptor.File] = []
        for (path, data) in contents.sorted(by: { $0.key < $1.key }) {
            let f = pkg.appendingPathComponent(path)
            try FileManager.default.createDirectory(at: f.deletingLastPathComponent(), withIntermediateDirectories: true)
            try data.write(to: f)
            let sha = path == "Manifest.json" ? ""
                : damage && path.hasSuffix("model.mlmodel") ? String(repeating: "0", count: 64)
                : DenoiseGlobalStore.DenoiseServiceTests.sha(data)
            files.append(.init(path: path, url: base.appendingPathComponent(path), sha256: sha, size: Int64(data.count)))
        }
        return (pkg, files)
    }

    func visible(_ dir: URL) -> [String] {
        ((try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? []).sorted()
    }

    @Test func theDescriptorPinsThePublishedPackage() {
        let files = UpscaleModelDescriptor.files(base: base)
        #expect(files.map(\.path) == ["Manifest.json", "Data/com.apple.CoreML/model.mlmodel", DenoiseModelDescriptor.weightsPath])
        #expect(files.last?.sha256 == UpscaleModelDescriptor.weightsSHA256 && files.last?.size == 33_451_648)
        #expect(files[0].sha256.isEmpty && files[1].sha256.count == 64, "the manifest's UUIDs differ per conversion: not pinned")
        #expect(files[0].url.absoluteString == base.absoluteString + "Manifest.json")
        // No hosting yet: nothing to download unless the environment names it.
        #expect(UpscaleModelDescriptor.downloadBase == nil)
        #expect(UpscaleModelDescriptor.downloadURL(environment: [:]) == nil)
        #expect(UpscaleModelDescriptor.downloadURL(environment: ["DS_UPSCALE_MODEL_URL": ""]) == nil)
        #expect(UpscaleModelDescriptor.downloadURL(environment: ["DS_UPSCALE_MODEL_URL": base.absoluteString]) == base)
        #expect(!UpscaleModelDownload.standard(environment: [:]).isConfigured)
        #expect(UpscaleModelDownload.standard(environment: ["DS_UPSCALE_MODEL_URL": base.absoluteString]).isConfigured)
        #expect(UpscaleModelDescriptor.downloadNotice.contains("non-commercial research"))
    }

    @Test func aDownloadIsCheckedAndInstalledInOneStep() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-up-dl-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        let (pkg, files) = try published(dir)
        let root = dir.appendingPathComponent("Models")
        let d = UpscaleModelDownload(root: root, files: files, downloader: LocalModelDownloader(package: pkg))
        #expect(d.state == .notDownloaded && d.installedPackage == nil && d.isConfigured)
        let url = try await d.download()
        #expect(url == d.packageURL && d.state == .installed && d.installedPackage == url)
        #expect(visible(root) == ["DarkslideUpscale.mlpackage"], "no staging folder left")
        #expect(FileManager.default.contentsEqual(atPath: url.appendingPathComponent(DenoiseModelDescriptor.weightsPath).path,
                                                  andPath: pkg.appendingPathComponent(DenoiseModelDescriptor.weightsPath).path))
        // Found at the next launch, which also sweeps a download a crash cut short (and
        // leaves other models' staging alone).
        let stale = root.appendingPathComponent(UpscaleModelDownload.stagingPrefix + "crashed")
        let other = root.appendingPathComponent(".download-lama")
        for d in [stale, other] { try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true) }
        let relaunched = UpscaleModelDownload(root: root, files: files, downloader: LocalModelDownloader(package: pkg))
        #expect(relaunched.state == .installed)
        _ = try await relaunched.download() // the first download sweeps its own leftovers only
        #expect(visible(root) == [".download-lama", "DarkslideUpscale.mlpackage"])
        // Downloading again (a damaged copy) swaps the package in place.
        try Data("bad".utf8).write(to: url.appendingPathComponent(DenoiseModelDescriptor.weightsPath))
        _ = try await d.download()
        #expect(FileManager.default.contentsEqual(atPath: url.appendingPathComponent(DenoiseModelDescriptor.weightsPath).path,
                                                  andPath: pkg.appendingPathComponent(DenoiseModelDescriptor.weightsPath).path))
        #expect(visible(root) == [".download-lama", "DarkslideUpscale.mlpackage"])
    }

    @Test func theManifestMayNameOnlyThePinnedItems() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-up-mf-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        func accepts(_ json: String) throws -> Bool {
            let url = dir.appendingPathComponent("\(UUID()).json")
            try Data(json.utf8).write(to: url)
            return UpscaleModelDescriptor.isPackageManifest(url)
        }
        func manifest(_ paths: [String], root: String = "A") -> String {
            let entries = paths.enumerated().map { #""\#(["A", "B", "C"][$0.offset])": {"path": "\#($0.element)"}"# }
            return #"{"itemInfoEntries": {\#(entries.joined(separator: ", "))}, "rootModelIdentifier": "\#(root)"}"#
        }
        #expect(try accepts(manifest(["com.apple.CoreML/model.mlmodel", "com.apple.CoreML/weights"])))
        #expect(try accepts(String(decoding: Self.manifest, as: UTF8.self)))
        // The bundled stub's real manifest (Xcode-written UUIDs, author, description).
        let stub = try #require(Bundle.module.url(forResource: "Fixtures/DenoiseStub", withExtension: "mlpackage"))
        #expect(UpscaleModelDescriptor.isPackageManifest(stub.appendingPathComponent("Manifest.json")))
        for bad in [manifest(["com.apple.CoreML/model.mlmodel", "../../../.ssh/authorized_keys"]),
                    manifest(["/etc/passwd"]), manifest(["com.apple.CoreML/../model.mlmodel"]),
                    manifest(["com.apple.CoreML/other.mlmodel"]),
                    manifest(["com.apple.CoreML/model.mlmodel"], root: "Z"),
                    #"{"itemInfoEntries": {}, "rootModelIdentifier": "A"}"#,
                    #"{"itemInfoEntries": {"A": {"name": "no path"}}, "rootModelIdentifier": "A"}"#,
                    #"{"itemInfoEntries": {"A": 1}, "rootModelIdentifier": "A"}"#, "{}", "not json"] {
            #expect(try !accepts(bad), "\(bad)")
        }
    }

    @Test func aDamagedOrCancelledDownloadInstallsNothing() async throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-up-dl-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        let (pkg, files) = try published(dir, damage: true)
        let root = dir.appendingPathComponent("Models")
        let bad = UpscaleModelDownload(root: root, files: files, downloader: LocalModelDownloader(package: pkg))
        let spec = "Data/com.apple.CoreML/model.mlmodel"
        await #expect(throws: ModelDownloadError.checksum("AI Upscale model", spec)) { _ = try await bad.download() }
        #expect(bad.state == .failed("The downloaded AI Upscale model is damaged (\(spec)). Try again."))
        #expect(visible(root).isEmpty, "nothing installed, no staging left")
        // A manifest that isn't a package manifest.
        let (odd, oddFiles) = try published(dir.appendingPathComponent("odd"), manifest: Data("{}".utf8))
        let oddDownload = UpscaleModelDownload(root: root, files: oddFiles, downloader: LocalModelDownloader(package: odd))
        await #expect(throws: ModelDownloadError.checksum("AI Upscale model", "Manifest.json")) { _ = try await oddDownload.download() }
        #expect(visible(root).isEmpty)
        // A file the source doesn't have.
        let gone = UpscaleModelDownload(root: root, files: files, downloader: LocalModelDownloader(package: dir))
        await #expect(throws: ModelDownloadError.self) { _ = try await gone.download() }
        if case .failed(let m) = gone.state { #expect(m.hasPrefix("Couldn't download the AI Upscale model")) } else {
            Issue.record("\(gone.state)")
        }
        // Cancelled mid-download.
        let slow = UpscaleModelDownload(root: root, files: files, downloader: StalledDownloader())
        let run = Task { try await slow.download() }
        #expect(await waitUntil { slow.isDownloading })
        slow.cancel()
        await #expect(throws: CancellationError.self) { _ = try await run.value }
        #expect(slow.state == .notDownloaded && visible(root).isEmpty)
        // Not configured.
        let none = UpscaleModelDownload(root: root, files: nil)
        await #expect(throws: UpscaleError.unavailable(UpscaleModelDescriptor.reasons.missing)) { _ = try await none.download() }
    }

    @Test func theMenuOffersTheDownloadThenUpscales() async throws {
        let tests = UpscaleServiceTests()
        let dir = tests.root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let (pkg, files) = try published(dir)
        let download = UpscaleModelDownload(root: dir.appendingPathComponent("Models"), files: files,
                                            downloader: LocalModelDownloader(package: pkg))
        let store = DenoiseModelStore(package: nil, compiledRoot: dir.appendingPathComponent("compiled"),
                                      expectedWeights: DenoiseGlobalStore.DenoiseServiceTests.sha(Data(repeating: 7, count: 64)),
                                      name: UpscaleModelDescriptor.name, reasons: UpscaleModelDescriptor.reasons,
                                      compile: { p in
            let out = dir.appendingPathComponent("tmp-\(UUID()).mlmodelc")
            try FileManager.default.copyItem(at: p, to: out)
            return out
        })
        let s = UpscaleService(model: store, backend: FakeUpscaleBackend(), download: download) { _, _ in NearestModel() }
        let m = await tests.app(s)
        #expect(s.offersDownload && s.modelUnavailableReason == UpscaleModelDescriptor.notDownloaded)
        #expect(CommandCatalog.title(.upscale, model: m) == "Download AI Upscale Model…")
        #expect(CommandCatalog.isEnabled(.upscale, model: m))
        #expect(CommandCatalog.help(.upscale, model: m)?.contains("34 MB") == true)
        // Declined: nothing happens.
        var asked: [String] = []
        s.confirmDownload = { asked.append($0); return false }
        CommandCatalog.perform(.upscale, model: m)
        await s.settle()
        #expect(asked == [UpscaleModelDescriptor.downloadNotice] && download.state == .notDownloaded)
        // Agreed: downloaded, installed, and Upscale 2× is ready.
        s.confirmDownload = { _ in true }
        CommandCatalog.perform(.upscale, model: m)
        await s.settle()
        #expect(download.state == .installed && !s.offersDownload && s.model.availability == .available)
        #expect(m.toast?.message.contains("AI Upscale model installed") == true)
        #expect(CommandCatalog.title(.upscale, model: m) == "Upscale 2×")
        CommandCatalog.perform(.upscale, model: m)
        await s.settle()
        #expect(m.toast?.message.hasPrefix("Upscaled 2×") == true, "the model checks and runs: \(m.toast?.message ?? "")")
    }

    @Test func aRunningDownloadCanBeCancelledFromTheMenuAndAFailureIsShown() async throws {
        let tests = UpscaleServiceTests()
        let dir = tests.root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let (_, files) = try published(dir)
        let slow = UpscaleModelDownload(root: dir.appendingPathComponent("Models"), files: files, downloader: StalledDownloader())
        let s = UpscaleService(model: tests.store(dir, bundled: false), backend: FakeUpscaleBackend(), download: slow)
        let m = await tests.app(s)
        CommandCatalog.perform(.upscale, model: m)
        #expect(await waitUntil { slow.isDownloading })
        #expect(CommandCatalog.title(.upscale, model: m).hasPrefix("Cancel AI Upscale Model Download ("))
        #expect(CommandCatalog.help(.upscale, model: m) == "Stop the download; nothing is kept")
        #expect(CommandCatalog.isEnabled(.upscale, model: m))
        CommandCatalog.perform(.upscale, model: m) // cancel
        await s.settle()
        #expect(slow.state == .notDownloaded && m.toast == nil && s.offersDownload)
        // A failure is shown, and the menu offers the download again.
        let broken = UpscaleModelDownload(root: dir.appendingPathComponent("Models"), files: files,
                                          downloader: LocalModelDownloader(package: dir))
        let s2 = UpscaleService(model: tests.store(dir, bundled: false), backend: FakeUpscaleBackend(), download: broken)
        let m2 = await tests.app(s2)
        s2.toggle()
        await s2.settle()
        #expect(m2.toast?.isError == true && m2.toast?.message.contains("Couldn't download") == true)
        #expect(s2.offersDownload && s2.title(in: m2) == "Download AI Upscale Model…")
    }

    @Test func aDamagedDownloadedModelIsOfferedAgain() async throws {
        let tests = UpscaleServiceTests()
        let dir = tests.root()
        defer { try? FileManager.default.removeItem(at: dir) }
        let (pkg, files) = try published(dir)
        let models = dir.appendingPathComponent("Models")
        let download = UpscaleModelDownload(root: models, files: files, downloader: LocalModelDownloader(package: pkg))
        _ = try await download.download()
        // The installed copy no longer matches the pinned weights.
        try Data("bad".utf8).write(to: download.packageURL.appendingPathComponent(DenoiseModelDescriptor.weightsPath))
        let store = tests.store(dir, bundled: false)
        store.install(package: download.packageURL)
        let s = UpscaleService(model: store, backend: FakeUpscaleBackend(), download: download) { _, _ in NearestModel() }
        let m = await tests.app(s)
        #expect(!s.offersDownload, "not known to be damaged until checked")
        await #expect(throws: UpscaleError.unavailable(UpscaleModelDescriptor.downloadDamaged)) {
            _ = try await s.upscale(URL(fileURLWithPath: "/photos/a.ARW"), name: "a-Upscaled.dng")
        }
        #expect(s.offersDownload && s.modelUnavailableReason == UpscaleModelDescriptor.downloadDamaged)
        #expect(CommandCatalog.title(.upscale, model: m) == "Download AI Upscale Model…")
        CommandCatalog.perform(.upscale, model: m)
        await s.settle()
        #expect(s.model.availability == .available && !s.offersDownload)
        CommandCatalog.perform(.upscale, model: m)
        await s.settle()
        #expect(m.toast?.message.hasPrefix("Upscaled 2×") == true, "\(m.toast?.message ?? "")")
    }

    @Test func theAboutPanelShowsBothCreditFiles() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-ack-\(UUID())")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try "# Models\n".write(to: dir.appendingPathComponent("Acknowledgements.md"), atomically: true, encoding: .utf8)
        let bundle = try #require(Bundle(url: dir))
        #expect(Acknowledgements.text(in: bundle) == "# Models\n\n\nTHIRD_PARTY_LICENSES.md is missing from this build.\n")
        try "# Crates\n".write(to: dir.appendingPathComponent("THIRD_PARTY_LICENSES.md"), atomically: true, encoding: .utf8)
        #expect(Acknowledgements.text(in: try #require(Bundle(url: dir))) == "# Models\n\n\n# Crates\n")
    }
}
