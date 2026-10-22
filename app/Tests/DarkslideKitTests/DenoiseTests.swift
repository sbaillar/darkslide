import CoreML
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Every suite that touches AI Denoise's process-wide state (the Rust store's model,
/// cache and export provider, `AppDenoiseProvider`) runs under this one serialized
/// parent, so one suite's install never lands in another's export.
@Suite(.serialized) enum DenoiseGlobalStore {}

/// Fake Rust tiles: inputs are `tile index + 1` everywhere (as f16); records what comes
/// back, checks every tile is written once.
class FakeTiles: DenoiseTiles, @unchecked Sendable {
    let count: Int
    let size: Int
    private let lock = NSLock()
    private(set) var written: [Int: [Float16]] = [:]
    private(set) var finished = 0
    var failFinish: Error?
    var readDelay: Duration = .zero
    /// Inputs are `(index + 1) × scale`.
    var scale: Float = 1

    init(count: Int, size: Int = 8) {
        self.count = count
        self.size = size
    }

    func tileCount() throws -> UInt32 { UInt32(count) }
    func tileSize() throws -> UInt32 { UInt32(size) }

    func readTile(index: UInt32, destPtr: UInt64, destLen: UInt64) throws {
        #expect(Int(destLen) == 3 * size * size)
        if readDelay > .zero { Thread.sleep(forTimeInterval: Double(readDelay.components.attoseconds) / 1e18) }
        let p = UnsafeMutablePointer<Float16>(bitPattern: UInt(destPtr))!
        for i in 0..<Int(destLen) { p[i] = Float16((Float(index) + 1) * scale) }
    }

    func writeTile(index: UInt32, srcPtr: UInt64, srcLen: UInt64, rowStride: UInt64, channelStride: UInt64) throws {
        let p = UnsafePointer<Float16>(bitPattern: UInt(srcPtr))!
        var out: [Float16] = []
        for c in 0..<3 {
            for y in 0..<size {
                for x in 0..<size { out.append(p[c * Int(channelStride) + y * Int(rowStride) + x]) }
            }
        }
        try lock.withLock {
            guard written[Int(index)] == nil else { throw DenoiseError.denoise("tile \(index) written twice") }
            written[Int(index)] = out
        }
    }

    func finish() async throws {
        lock.withLock { finished += 1 }
        if let failFinish { throw failFinish }
    }
}

/// Doubles every value; optionally pads rows (like Core ML on the Neural Engine) or fails.
struct DoublingModel: DenoiseTileModel {
    var padding = 0
    var failOn: Int? // fail when the input value (tile index + 1) is this
    var dataType: MLMultiArrayDataType = .float16
    var calls: Counter? = nil

    final class Counter: @unchecked Sendable {
        private let lock = NSLock()
        private var n = 0
        func add() { lock.withLock { n += 1 } }
        var value: Int { lock.withLock { n } }
    }

    func denoise(_ input: MLMultiArray) throws -> MLMultiArray {
        calls?.add()
        let t = input.shape[3].intValue
        let first = input.withUnsafeBytes { $0.load(as: Float16.self) }
        if let failOn, Int(Float(first)) == failOn { throw DenoiseError.model("boom") }
        let row = t + padding
        let strides: [Int] = [3 * t * row, t * row, row, 1]
        let out = MLMultiArray(shape: input.shape.map(\.intValue), dataType: dataType, strides: strides)
        guard dataType == .float16 else { return out }
        input.withUnsafeBytes { src in
            out.withUnsafeMutableBytes { dst, _ in
                let s: UnsafeBufferPointer<Float16> = src.bindMemory(to: Float16.self)
                let d: UnsafeMutableBufferPointer<Float16> = dst.bindMemory(to: Float16.self)
                for c in 0..<3 {
                    for y in 0..<t {
                        for x in 0..<t {
                            let v: Float16 = s[c * t * t + y * t + x]
                            d[c * t * row + y * row + x] = v * 2
                        }
                    }
                }
            }
        }
        return out
    }
}

@Suite struct DenoiseEngineTests {
    @Test func everyTileGoesThroughTheModelsOnceAndThePhotoIsFinished() async throws {
        let tiles = FakeTiles(count: 23)
        let counts = [DoublingModel.Counter(), DoublingModel.Counter()]
        let progress = DoublingModel.Counter()
        try await DenoiseEngine.run(tiles, models: [DoublingModel(calls: counts[0]), DoublingModel(padding: 3, calls: counts[1])]) { f in
            #expect(f > 0 && f <= 1)
            progress.add()
        }
        #expect(tiles.written.count == 23)
        for (i, out) in tiles.written {
            #expect(out.allSatisfy { $0 == Float16(Float(i + 1) * 2) }, "tile \(i) read through its strides")
        }
        #expect(tiles.finished == 1)
        #expect(counts[0].value + counts[1].value == 23)
        #expect(progress.value == 23)
    }

    @Test func aModelErrorStopsTheRunAndNothingIsStored() async {
        let tiles = FakeTiles(count: 40)
        await #expect(throws: DenoiseError.model("boom")) {
            try await DenoiseEngine.run(tiles, models: [DoublingModel(failOn: 5), DoublingModel(failOn: 5)])
        }
        #expect(tiles.finished == 0)
        #expect(tiles.written.count < 40)
        await #expect(throws: DenoiseError.model("no model instance")) {
            try await DenoiseEngine.run(FakeTiles(count: 1), models: [])
        }
    }

    @Test func unexpectedOutputsAreRejected() async {
        await #expect(throws: DenoiseError.self) {
            try await DenoiseEngine.run(FakeTiles(count: 2), models: [DoublingModel(dataType: .float32)])
        }
        let wrongShape = FakeTiles(count: 1, size: 8)
        struct Small: DenoiseTileModel {
            func denoise(_ input: MLMultiArray) throws -> MLMultiArray {
                try MLMultiArray(shape: [1, 3, 4, 4], dataType: .float16)
            }
        }
        await #expect(throws: DenoiseError.self) { try await DenoiseEngine.run(wrongShape, models: [Small()]) }
        struct ColumnMajor: DenoiseTileModel {
            func denoise(_ input: MLMultiArray) throws -> MLMultiArray {
                MLMultiArray(shape: input.shape.map(\.intValue), dataType: .float16, strides: [192, 64, 1, 8])
            }
        }
        await #expect(throws: DenoiseError.model("non-contiguous output rows")) {
            try await DenoiseEngine.run(FakeTiles(count: 1), models: [ColumnMajor()])
        }
    }

    @Test func cancellingStopsAfterTheCurrentTiles() async {
        let tiles = FakeTiles(count: 500)
        tiles.readDelay = .milliseconds(2)
        let run = Task { try await DenoiseEngine.run(tiles, models: [DoublingModel()]) }
        try? await Task.sleep(for: .milliseconds(30))
        run.cancel()
        await #expect(throws: CancellationError.self) { try await run.value }
        #expect(tiles.finished == 0)
        #expect(tiles.written.count < 500)
    }

    /// The stub model (identity, the real model's tensor I/O) through Core ML.
    @Test func theCoreMLAdapterRunsTiles() async throws {
        let compiled = try await MLModel.compileModel(at: DenoiseGlobalStore.DenoiseModelStoreTests.stub())
        defer { try? FileManager.default.removeItem(at: compiled) }
        let tiles = FakeTiles(count: 3, size: 512)
        tiles.scale = 0.2
        try await DenoiseEngine.run(tiles, models: [try CoreMLDenoiseModel.load(compiled, units: .cpuOnly)])
        for (i, out) in tiles.written {
            let want = Float(i + 1) * 0.2
            let worst = out.map { abs(Float($0) - want) }.max()!
            #expect(worst < 0.001, "tile \(i): off by \(worst)")
        }
    }

    @Test func computeUnits() {
        #expect(DenoiseService.units(background: false) == [.cpuAndGPU, .cpuAndNeuralEngine])
        #expect(DenoiseService.units(background: true) == [.cpuAndNeuralEngine], "the GPU stays the viewer's")
    }

    @Test func coreMLModelErrorsAreReported() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-dn-bad-\(UUID()).mlmodelc")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(throws: DenoiseError.self) { try CoreMLDenoiseModel.load(dir, units: .cpuOnly) }
    }
}

/// Fake Rust backend: results by url, tiles from `FakeTiles`, optional failures.
final class FakeBackend: DenoiseBackend, @unchecked Sendable {
    private let lock = NSLock()
    private var ready: Set<URL> = []
    private(set) var models: [String?] = []
    private(set) var caches: [URL?] = []
    private(set) var begun: [(URL, Bool)] = []
    var failBegin: Set<URL> = []
    var tileCount = 3
    var tileDelay: Duration = .zero

    func setModel(_ id: String?) { lock.withLock { models.append(id) } }
    func setCache(_ dir: URL?, maxBytes: UInt64) throws { lock.withLock { caches.append(dir) } }
    func isReady(_ url: URL) async throws -> Bool { lock.withLock { ready.contains(url) } }
    func markReady(_ url: URL) { _ = lock.withLock { ready.insert(url) } }
    var begins: [(URL, Bool)] { lock.withLock { begun } }

    func begin(_ url: URL, background: Bool) async throws -> any DenoiseTiles {
        lock.withLock { begun.append((url, background)) }
        if failBegin.contains(url) { throw DenoiseError.denoise("unreadable \(url.lastPathComponent)") }
        let t = Tiles(count: tileCount) { [weak self] in self?.markReady(url) }
        t.readDelay = tileDelay
        return t
    }

    final class Tiles: FakeTiles, @unchecked Sendable {
        let done: @Sendable () -> Void
        init(count: Int, done: @escaping @Sendable () -> Void) {
            self.done = done
            super.init(count: count)
        }
        override func finish() async throws {
            try await super.finish()
            done()
        }
    }
}

extension DenoiseGlobalStore {
    @MainActor
    @Suite(.serialized) struct DenoiseServiceTests {
        /// A store with a fake bundled package (its weights' checksum pinned; compile = copy).
        static func store(_ root: URL, weights: Data = Data(repeating: 1, count: 64), pinned: String? = nil,
                          bundled: Bool = true) -> DenoiseModelStore {
            let pkg = root.appendingPathComponent("bundle/DarkslideDenoise.mlpackage")
            let w = pkg.appendingPathComponent(DenoiseModelDescriptor.weightsPath)
            try? FileManager.default.createDirectory(at: w.deletingLastPathComponent(), withIntermediateDirectories: true)
            try? weights.write(to: w)
            return DenoiseModelStore(package: bundled ? pkg : nil, compiledRoot: root.appendingPathComponent("compiled"),
                                     expectedWeights: pinned ?? sha(weights), compile: { pkg in
                let out = root.appendingPathComponent("tmp-\(UUID()).mlmodelc")
                try FileManager.default.copyItem(at: pkg, to: out)
                return out
            })
        }

        static func sha(_ d: Data) -> String {
            let url = FileManager.default.temporaryDirectory.appendingPathComponent("ds-sha-\(UUID())")
            try? d.write(to: url)
            defer { try? FileManager.default.removeItem(at: url) }
            return (try? ModelFiles.sha256(of: url)) ?? ""
        }

        func root() -> URL { FileManager.default.temporaryDirectory.appendingPathComponent("ds-dn-svc-\(UUID())") }

        func service(_ backend: FakeBackend, root: URL, loads: DoublingModel.Counter? = nil,
                     failLoads: Int = 0, store: DenoiseModelStore? = nil) -> DenoiseService {
            let failures = DoublingModel.Counter()
            return DenoiseService(model: store ?? Self.store(root), backend: backend,
                                  cacheDir: root.appendingPathComponent("cache")) { _, _ in
                loads?.add()
                failures.add()
                if failures.value <= failLoads { throw DenoiseError.model("damaged") }
                return DoublingModel()
            }
        }

        nonisolated static func url(_ n: Int) -> URL { URL(fileURLWithPath: "/photos/P\(n).ARW") }

        @Test func aPhotoIsDenoisedOnceAndItsWaitersAllHearOfIt() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            let loads = DoublingModel.Counter()
            let s = service(backend, root: dir, loads: loads)
            async let a: Void = s.denoise(Self.url(1))
            async let b: Void = s.denoise(Self.url(1)) // joins the first
            _ = try await (a, b)
            #expect(s.states[Self.url(1)] == .ready)
            #expect(backend.begins.count == 1)
            #expect(backend.caches == [dir.appendingPathComponent("cache")], "configured once")
            #expect(backend.models.last == s.model.id)
            #expect(loads.value == 2, "a GPU and a Neural Engine instance")
            try await s.denoise(Self.url(2), background: true)
            #expect(loads.value == 2, "the instances are kept (the Neural Engine one is shared)")
            #expect(backend.begins.last! == (Self.url(2), true))
        }

        @Test func thePhotoOnScreenRunsAheadOfABatch() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            backend.tileDelay = .milliseconds(5)
            let s = service(backend, root: dir)
            for i in 1...3 { s.enqueue(Self.url(i), background: true) }
            s.enqueue(Self.url(9), background: false)
            s.enqueue(Self.url(2), background: false) // already queued: moves ahead
            s.enqueue(Self.url(9), background: false) // already queued: no-op
            #expect(s.states[Self.url(3)] == .queued)
            try await s.denoise(Self.url(3), background: true)
            let order = backend.begins.map(\.0)
            // The last on-screen request first; earlier ones (photos moved away from) and the
            // batch then run in order.
            #expect(order == [Self.url(9), Self.url(1), Self.url(2), Self.url(3)], "\(order)")
            #expect(backend.begins.map(\.1) == [false, true, true, true])
        }

        @Test func failuresAreShownAndRetried() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            backend.failBegin = [Self.url(1)]
            let s = service(backend, root: dir)
            await #expect(throws: DenoiseError.self) { try await s.denoise(Self.url(1)) }
            guard case .failed(let m) = s.states[Self.url(1)] else {
                Issue.record("expected a failure")
                return
            }
            #expect(m.contains("unreadable P1.ARW"))
            backend.failBegin = []
            s.retry(Self.url(1))
            #expect(await waitUntil { s.states[Self.url(1)] == .ready })
        }

        @Test func aModelThatDoesntLoadIsCompiledAgainOnce() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            let loads = DoublingModel.Counter()
            let s = service(backend, root: dir, loads: loads, failLoads: 1)
            try await s.denoise(Self.url(1))
            #expect(s.model.availability == .available)
            #expect(loads.value == 3, "one failed load, then both instances")
            // Fails again after recompiling: reported, the photo isn't stored.
            let dir2 = root()
            defer { try? FileManager.default.removeItem(at: dir2) }
            let s2 = service(FakeBackend(), root: dir2, failLoads: 10)
            await #expect(throws: DenoiseError.self) { try await s2.denoise(Self.url(1)) }
        }

        @Test func cancellingAQueuedOrRunningPhotoStoresNothing() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            backend.tileCount = 400
            backend.tileDelay = .milliseconds(2)
            let s = service(backend, root: dir)
            let running = Task { try await s.denoise(Self.url(1)) }
            let queued = Task { try await s.denoise(Self.url(2), background: true) }
            #expect(await waitUntil { if case .running = s.states[Self.url(1)] { true } else { false } })
            s.cancel(Self.url(2))
            await #expect(throws: CancellationError.self) { try await queued.value }
            #expect(s.states[Self.url(2)] == nil)
            s.cancel(Self.url(1))
            await #expect(throws: CancellationError.self) { try await running.value }
            #expect(s.states[Self.url(1)] == nil)
            #expect(!(try await backend.isReady(Self.url(1))))
            s.cancel(Self.url(7)) // nothing: harmless
        }

        @Test func cancellingTheWaitingTaskStopsAPhotoNobodyElseWaitsFor() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            backend.tileCount = 400
            backend.tileDelay = .milliseconds(2)
            let s = service(backend, root: dir)
            let a = Task { try await s.denoise(Self.url(1)) }
            let b = Task { try await s.denoise(Self.url(1)) }
            #expect(await waitUntil { if case .running = s.states[Self.url(1)] { true } else { false } })
            a.cancel()
            await #expect(throws: CancellationError.self) { try await a.value }
            #expect(s.states[Self.url(1)] != nil, "b still waits: the photo goes on")
            b.cancel()
            await #expect(throws: CancellationError.self) { try await b.value }
            #expect(await waitUntil { s.states[Self.url(1)] == nil }, "nobody waits: stopped")
            #expect(!(try await backend.isReady(Self.url(1))))
            let pre = Task { try await s.denoise(Self.url(2)) }
            pre.cancel()
            await #expect(throws: CancellationError.self) { try await pre.value }
        }

        @Test func exportDenoisesAPhotoJustBeforeItRenders() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            backend.markReady(Self.url(1))
            backend.failBegin = [Self.url(3)]
            let s = service(backend, root: dir)
            #expect(try await s.ensureForExport(Self.url(1)))
            #expect(backend.begins.isEmpty, "ready photos aren't redone")
            #expect(try await s.ensureForExport(Self.url(2)))
            #expect(backend.begins.map(\.0) == [Self.url(2)])
            #expect(backend.begins.map(\.1) == [true], "export work runs in the background")
            await #expect(throws: DenoiseError.self) { try await s.ensureForExport(Self.url(3)) }

            // Rust's view: the provider blocks its worker, and failures become that photo's
            // error message; a cancel is a cancel.
            let token = try CancelToken()
            #expect(try AppDenoiseProvider { _ in true }.ensure(path: "/p/A.ARW", cancel: token))
            #expect(try !AppDenoiseProvider { _ in false }.ensure(path: "/p/A.ARW", cancel: token), "no model in this build")
            do {
                _ = try AppDenoiseProvider { _ in throw DenoiseError.damaged }.ensure(path: "/p/A.ARW", cancel: token)
                Issue.record("expected an error")
            } catch let DsError.Unsupported(message) {
                #expect(message == DenoiseError.damaged.errorDescription, "the app's reason, as is")
            }
            #expect(throws: DsError.self) {
                _ = try AppDenoiseProvider { _ in throw CancellationError() }.ensure(path: "/p/A.ARW", cancel: token)
            }
            let seen = DoublingModel.Counter()
            _ = try AppDenoiseProvider { url in
                #expect(url.path == "/p/B.ARW")
                seen.add()
                return true
            }.ensure(path: "/p/B.ARW", cancel: token)
            #expect(seen.value == 1)
        }

        /// Cancelling the export's token cancels the denoise it waits for, quickly.
        @Test func cancellingTheExportCancelsItsDenoise() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            backend.tileCount = 2000
            backend.tileDelay = .milliseconds(2)
            let s = service(backend, root: dir)
            let token = try CancelToken()
            let provider = AppDenoiseProvider { url in try await s.ensureForExport(url) }
            let worker = Task.detached { try provider.ensure(path: Self.url(5).path, cancel: token) }
            #expect(await waitUntil { if case .running = s.states[Self.url(5)] { true } else { false } })
            let t0 = ContinuousClock.now
            try token.cancel()
            await #expect(throws: DsError.self) { try await worker.value }
            #expect(ContinuousClock.now - t0 < TestTiming.bound(.seconds(2)), "well inside the quit bound")
            #expect(await waitUntil { s.states[Self.url(5)] == nil }, "the denoise stopped")
            #expect(!(try await backend.isReady(Self.url(5))))
        }

        /// Someone asking for a photo whose run is being cancelled gets a fresh run, not the
        /// cancellation.
        @Test func aNewcomerToACancelledRunGetsAFreshOne() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            backend.tileCount = 400
            backend.tileDelay = .milliseconds(2)
            let s = service(backend, root: dir)
            let first = Task { try await s.denoise(Self.url(6)) }
            #expect(await waitUntil { if case .running = s.states[Self.url(6)] { true } else { false } })
            s.cancel(Self.url(6))
            backend.tileCount = 3
            try await s.denoise(Self.url(6)) // arrives while the first run winds down
            await #expect(throws: CancellationError.self) { try await first.value }
            #expect(s.states[Self.url(6)] == .ready)
            #expect(backend.begins.filter { $0.0 == Self.url(6) }.count == 2)
        }

        @Test func theFocusedPhotoAlwaysRunsNext() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            backend.tileDelay = .milliseconds(3)
            let s = service(backend, root: dir)
            let m = makeModel(store: URLStore())
            await m.start()
            s.app = m
            let urls = m.visiblePhotos.prefix(5).compactMap(\.url)
            for u in urls.prefix(4) { s.enqueue(u, background: true) }
            // The user lands on the last one while the batch is queued.
            m.focus(m.visiblePhotos[3].id, extend: false)
            try await s.denoise(urls[3], background: true)
            #expect(backend.begins.first?.0 == urls[0] || backend.begins.first?.0 == urls[3])
            #expect(backend.begins.prefix(2).contains { $0.0 == urls[3] }, "on screen: next, not last")
            m.stop()
        }

        @Test func theFocusedPhotoIsDenoisedWhenItAsksAndBatchesTurnItOn() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            let s = service(backend, root: dir)
            let m = makeModel(store: URLStore())
            await m.start()
            s.app = m
            let first = try #require(m.focusedID)
            let url = try #require(m.photo(first)?.url)
            await s.checkFocused()
            #expect(s.states[url] == nil, "AI Denoise off: nothing to do")
            m.updateEdit(label: "AI Denoise") { $0.noise.aiDenoise = true }
            await s.checkFocused()
            #expect(await waitUntil { s.states[url] == .ready })
            let renders = m.viewer.renderCount
            #expect(await waitUntil { m.viewer.renderCount > renders || m.viewer.renderCount > 0 })
            await s.checkFocused()
            #expect(backend.begins.count == 1, "ready: not denoised again")

            // A failed photo waits for Retry.
            let second = m.visiblePhotos[1]
            backend.failBegin = [second.url!]
            m.focus(second.id, extend: false)
            m.updateEdit(label: "AI Denoise") { $0.noise.aiDenoise = true }
            await s.checkFocused()
            #expect(await waitUntil { if case .failed = s.states[second.url!] { true } else { false } })
            await s.checkFocused()
            #expect(backend.begins.filter { $0.0 == second.url! }.count == 1)

            // Select four: AI Denoise on for all, queued in the background (the focused one
            // not); the failed one is tried again.
            backend.failBegin = []
            let ids = Array(m.visiblePhotos.prefix(4).map(\.id))
            m.selection = Set(ids)
            let n = s.denoiseSelected()
            #expect(n == 3, "the ready one isn't queued again")
            #expect(ids.allSatisfy { m.edit(for: $0).noise.aiDenoise })
            #expect(await waitUntil { ids.compactMap { m.photo($0)?.url }.allSatisfy { s.states[$0] == .ready } })
            #expect(backend.begins.first { $0.0 == second.url! && !$0.1 } != nil, "the focused photo isn't a background job")
            s.app = nil
            #expect(s.denoiseSelected() == 0)
            m.stop()
        }
    }
}

extension DenoiseGlobalStore {
    @MainActor
    @Suite(.serialized) struct DenoiseWithoutModelTests {
        /// A build without the model: the control is off, nothing is queued, exports go on
        /// (the original is exported: Rust's `require` passes without a model).
        @Test func aMissingModelDisablesAIDenoiseAndLeavesExportAlone() async throws {
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-dn-none-\(UUID())")
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            let store = DenoiseServiceTests.store(dir, bundled: false)
            #expect(store.availability == .missing)
            #expect(store.unavailableReason == "AI Denoise model not included in this build.")
            await #expect(throws: DenoiseError.missing) { try await store.prepare() }
            let s = DenoiseService(model: store, backend: backend, cacheDir: nil) { _, _ in DoublingModel() }
            let m = makeModel(store: URLStore())
            await m.start()
            s.app = m
            m.updateEdit(label: "AI Denoise") { $0.noise.aiDenoise = true }
            await s.checkFocused()
            #expect(s.states.isEmpty && backend.begins.isEmpty)
            #expect(s.denoiseSelected() == 0)
            #expect(try await !s.ensureForExport(DenoiseServiceTests.url(1)), "no model in this build: export the original")
            #expect(backend.models.isEmpty, "Rust never gets a model: renders and exports ignore AI Denoise")
            m.stop()
        }

        /// Weights that don't match the pinned checksum are refused.
        @Test func aDamagedModelIsRefused() async throws {
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-dn-bad-\(UUID())")
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = FakeBackend()
            let store = DenoiseServiceTests.store(dir, pinned: String(repeating: "0", count: 64))
            #expect(store.availability == .available, "not checked yet")
            let s = DenoiseService(model: store, backend: backend, cacheDir: nil) { _, _ in DoublingModel() }
            s.configure()
            #expect(await waitUntil { store.availability == .damaged })
            #expect(store.unavailableReason?.contains("damaged") == true)
            await #expect(throws: DenoiseError.damaged) { try await store.prepare() }
            await #expect(throws: DenoiseError.damaged) { try await s.denoise(DenoiseServiceTests.url(1)) }
            #expect(!FileManager.default.fileExists(atPath: store.compiledURL.path), "never compiled")
            #expect(backend.models.isEmpty)
            // An export with AI Denoise on fails that photo, saying why.
            await #expect(throws: DenoiseError.damaged) { try await s.ensureForExport(DenoiseServiceTests.url(1)) }
        }
    }
}

/// The mock session with a file URL per photo.
struct URLStore: SessionStore {
    let inner = MockSessionStore(counts: [6])
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] {
        try await inner.photos(in: session).map { p in
            var p = p
            p.url = URL(fileURLWithPath: "/photos/\(p.filename)")
            return p
        }
    }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws {}
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
}

extension DenoiseGlobalStore {
    @MainActor
    @Suite(.serialized) struct DenoiseModelStoreTests {
        /// The stub package (test fixture).
        static func stub() throws -> URL {
            try #require(Bundle.module.url(forResource: "Fixtures/DenoiseStub", withExtension: "mlpackage"))
        }

        func root() -> URL { FileManager.default.temporaryDirectory.appendingPathComponent("ds-dn-model-\(UUID())") }

        @Test func theBundledPackageIsFoundOnlyWhenTheBuildIncludedIt() throws {
            #expect(DenoiseModelDescriptor.weightsSHA256.count == 64)
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            #expect(DenoiseModelDescriptor.bundledPackage(in: try #require(Bundle(url: dir))) == nil, "not in this build")
            try FileManager.default.createDirectory(at: dir.appendingPathComponent("Models/DarkslideDenoise.mlpackage"),
                                                    withIntermediateDirectories: true)
            let b = try #require(Bundle(url: dir))
            #expect(DenoiseModelDescriptor.bundledPackage(in: b)?.lastPathComponent == "DarkslideDenoise.mlpackage")
            #expect(DenoiseModelDescriptor.bundledPackage(in: Bundle(for: FakeTiles.self)) == nil)
        }

        /// The pinned checksum in the app and the conversion script are the same.
        @Test func theAppPinsTheChecksumTheConversionProduces() throws {
            let script = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
                .appendingPathComponent("../../../scripts/denoise-model/convert.py").standardizedFileURL
            let text = try String(contentsOf: script, encoding: .utf8)
            #expect(text.contains("EXPECTED_WEIGHTS_SHA256 = \"\(DenoiseModelDescriptor.weightsSHA256)\""))
        }

        /// A bundled package that checks out loads: verified, compiled once, loaded by Core ML.
        @Test func aBundledPackageLoads() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let pkg = try Self.stub()
            let sha = try ModelFiles.sha256(of: pkg.appendingPathComponent(DenoiseModelDescriptor.weightsPath))
            let store = DenoiseModelStore(package: pkg, compiledRoot: dir, expectedWeights: sha)
            #expect(store.availability == .available && store.unavailableReason == nil && store.id == sha)
            async let a = store.prepare()
            async let b = store.prepare() // joins the compile in progress
            let (u1, u2) = try await (a, b)
            #expect(u1 == u2 && u1 == store.compiledURL)
            #expect(try await store.prepare() == u1, "compiled once")
            _ = try CoreMLDenoiseModel.load(u1, units: .cpuOnly)
            store.discard()
            #expect(!FileManager.default.fileExists(atPath: u1.path))
            #expect(try await store.prepare() == u1, "compiled again")
        }

        @Test func aCompileFailureIsReported() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            let pkg = try Self.stub()
            let sha = try ModelFiles.sha256(of: pkg.appendingPathComponent(DenoiseModelDescriptor.weightsPath))
            let store = DenoiseModelStore(package: pkg, compiledRoot: dir, expectedWeights: sha,
                                          compile: { _ in throw URLError(.cannotDecodeContentData) })
            await #expect(throws: DenoiseError.self) { try await store.prepare() }
            #expect(store.availability == .available, "not the model's fault")
        }

        /// A package without its weights file counts as damaged.
        @Test func aPackageWithoutWeightsIsRefused() async throws {
            let dir = root()
            defer { try? FileManager.default.removeItem(at: dir) }
            try FileManager.default.createDirectory(at: dir.appendingPathComponent("p.mlpackage"), withIntermediateDirectories: true)
            let store = DenoiseModelStore(package: dir.appendingPathComponent("p.mlpackage"), compiledRoot: dir)
            await #expect(throws: DenoiseError.damaged) { try await store.verify() }
            #expect(store.availability == .damaged)
        }
    }
}

@Suite struct DenoiseCodecTests {
    @Test func aiDenoiseRoundTripsThroughTheSidecarJSON() throws {
        var e = EditState()
        e.noise.aiDenoise = true
        e.noise.aiStrength = 73
        let json = try AdjustmentsCodec.encode(e)
        #expect(json.contains("\"ai_denoise\":true"))
        let back = try AdjustmentsCodec.decode(json).0
        #expect(back.noise.aiDenoise)
        #expect(back.noise.aiStrength == 73)
        #expect(!(try AdjustmentsCodec.decode(#"{"noise": {"luminance": 10}}"#).0).noise.aiDenoise, "older sidecars: off")
        #expect(DenoiseService.wants(back))
        var zero = back
        zero.noise.aiStrength = 0
        #expect(!DenoiseService.wants(zero))
        #expect(AdjustGroup.noise.differs(EditState(), back))
    }

    @Test func presetsCarryAIDenoise() {
        var e = EditState()
        e.noise.aiDenoise = true
        e.noise.aiStrength = 64
        let back = EditCodec.decode(EditCodec.encode(e))
        #expect(back.noise.aiDenoise && back.noise.aiStrength == 64)
        #expect(EditCodec.group(of: "noise.aiDenoise") == .noise)
        var half = e
        half.noise.aiStrength = 0.4
        #expect(!DenoiseService.wants(half), "rounds to 0 like Rust")
        half.noise.aiStrength = 0.6
        #expect(DenoiseService.wants(half))
    }

    @Test func errorsDescribeThemselves() {
        for e in [DenoiseError.missing, .damaged, .compile("x"), .model("x"), .denoise("x")] {
            #expect(e.errorDescription?.contains("AI Denoise") == true)
        }
    }
}

extension DenoiseGlobalStore {
    /// The Rust backend without a model or a file (the live suite covers the real path).
    @Suite(.serialized) struct RustDenoiseBackendTests {
        @Test func errorsComeBackAsRenderErrors() async throws {
            let b = RustDenoiseBackend()
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-dn-rust-\(UUID())")
            defer { try? FileManager.default.removeItem(at: dir) }
            try b.setCache(dir, maxBytes: 1 << 20)
            #expect(FileManager.default.fileExists(atPath: dir.path))
            #expect(throws: RenderError.self) { try b.setCache(URL(fileURLWithPath: "/dev/null/x"), maxBytes: 1) }
            let missing = URL(fileURLWithPath: "/nonexistent/ds-denoise/P.ARW")
            await #expect(throws: RenderError.self) { try await b.isReady(missing) }
            await #expect(throws: RenderError.self) { try await b.begin(missing, background: true) }
            try b.setCache(nil, maxBytes: 0)
        }

    }
}

extension DenoiseGlobalStore {
    /// A real `export_batch` through the app's provider: the stub model (identity) on the
    /// CPU, the real Rust tiles, cache and export, on a real A9 frame. Skips without samples.
    @MainActor
    @Suite(.serialized) struct DenoiseExportTests {
        /// The stub model, slowed per tile when asked (to cancel mid-run).
        struct Slow: DenoiseTileModel {
            let inner: any DenoiseTileModel
            let delay: Double
            func denoise(_ input: MLMultiArray) throws -> MLMultiArray {
                if delay > 0 { Thread.sleep(forTimeInterval: delay) }
                return try inner.denoise(input)
            }
        }

        /// Counts the Rust backend's denoise runs.
        final class Counting: DenoiseBackend, @unchecked Sendable {
            let inner = RustDenoiseBackend()
            private let lock = NSLock()
            private var n = 0
            var begun: Int { lock.withLock { n } }
            func setModel(_ id: String?) { inner.setModel(id) }
            func setCache(_ dir: URL?, maxBytes: UInt64) throws { try inner.setCache(dir, maxBytes: maxBytes) }
            func isReady(_ url: URL) async throws -> Bool { try await inner.isReady(url) }
            func begin(_ url: URL, background: Bool) async throws -> any DenoiseTiles {
                lock.withLock { n += 1 }
                return try await inner.begin(url, background: background)
            }
        }

        func service(_ dir: URL, backend: Counting, delay: Double = 0) throws -> DenoiseService {
            let stub = try DenoiseModelStoreTests.stub()
            let sha = try ModelFiles.sha256(of: stub.appendingPathComponent(DenoiseModelDescriptor.weightsPath))
            return DenoiseService(model: DenoiseModelStore(package: stub, compiledRoot: dir, expectedWeights: sha),
                                  backend: backend, cacheDir: nil) { url, _ in
                Slow(inner: try CoreMLDenoiseModel.load(url, units: .cpuOnly), delay: delay)
            }
        }

        static func job(_ raw: URL, out: URL) -> ExportJob {
            var ai = EditState()
            ai.noise.aiDenoise = true
            ai.noise.aiStrength = 80
            return ExportJob(items: [ExportJobItem(photoID: PhotoID(1), url: raw, seq: 1, edit: ai)],
                             recipes: [ExportRecipe(name: "Web", sizeMode: .longEdge, sizeValue: 600, fileNameTemplate: "{seq}")],
                             defaultFolder: out)
        }

        @Test func anExportDenoisesThroughTheAppJustBeforeRendering() async throws {
            guard let raw = sampleARWs().first else { return }
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-dn-export-\(UUID())")
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = Counting()
            let s = try service(dir, backend: backend)
            AppDenoiseProvider.install { try await s.ensureForExport($0) }
            defer {
                AppDenoiseProvider.uninstall()
                backend.setModel(nil)
            }
            let o = try await RustExportService().export(Self.job(raw, out: dir.appendingPathComponent("Out"))) { _, _ in }
            #expect(o.failures.isEmpty && o.written.count == 1, "\(o.failures)")
            #expect(backend.begun == 1, "denoised by the app, during the export")
            #expect(try await backend.isReady(raw))
            await s.settle() // its model check must not land in the next test
        }

        @Test func cancellingAnExportStopsTheDenoiseItWaitsFor() async throws {
            guard let raw = sampleARWs().first else { return }
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-dn-export-\(UUID())")
            defer { try? FileManager.default.removeItem(at: dir) }
            let backend = Counting()
            let s = try service(dir, backend: backend, delay: 0.2) // 117 tiles: ~25 s uncancelled
            AppDenoiseProvider.install { try await s.ensureForExport($0) }
            defer {
                AppDenoiseProvider.uninstall()
                backend.setModel(nil)
            }
            let export = Task { try await RustExportService().export(Self.job(raw, out: dir.appendingPathComponent("Out"))) { _, _ in } }
            #expect(await waitUntil(timeout: .seconds(60)) { s.states.values.contains { if case .running = $0 { true } else { false } } })
            let t0 = ContinuousClock.now
            export.cancel()
            _ = try? await export.value
            #expect(ContinuousClock.now - t0 < TestTiming.bound(.seconds(3)), "the export returns within the quit bound")
            #expect(await waitUntil { s.states.isEmpty }, "the denoise was cancelled, not finished")
            #expect(!(try await backend.isReady(raw)))
            await s.settle() // its model check must not land in the next test
        }

        /// The Rust tiles of a real A9 frame through a pass-through model: every tile read and
        /// written through caller memory, the result stored, the photo then ready.
        @Test func aRealPhotosTilesRoundTrip() async throws {
            guard let url = sampleARWs().first else { return }
            struct PassThrough: DenoiseTileModel {
                func denoise(_ input: MLMultiArray) throws -> MLMultiArray { input }
            }
            let b = RustDenoiseBackend()
            b.setModel("swift-test-pass-through")
            defer { b.setModel(nil) }
            #expect(try await !b.isReady(url))
            let tiles = try await b.begin(url, background: true)
            #expect(try tiles.tileCount() == 117)
            try await DenoiseEngine.run(tiles, models: [PassThrough(), PassThrough()])
            #expect(try await b.isReady(url))
            await #expect(throws: (any Error).self) { try await tiles.finish() } // finished already
        }
    }
}
