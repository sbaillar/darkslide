import CoreML
import DarkslideFFI
import Foundation

/// One photo's tiles on the Rust side (`DenoiseTask`): the engine reads each tile's model
/// input into caller memory and hands the output back. Seam for tests.
public protocol DenoiseTiles: AnyObject, Sendable {
    func tileCount() throws -> UInt32
    func tileSize() throws -> UInt32
    func readTile(index: UInt32, destPtr: UInt64, destLen: UInt64) throws
    func writeTile(index: UInt32, srcPtr: UInt64, srcLen: UInt64, rowStride: UInt64, channelStride: UInt64) throws
    /// Stitch and store the result (renders use it from then on).
    func finish() async throws
}

/// A model that denoises one tile: `[1, 3, t, t]` float16 in, the same shape out (its
/// memory layout may be padded: the engine reads it through the output's strides).
public protocol DenoiseTileModel: Sendable {
    func denoise(_ input: MLMultiArray) throws -> MLMultiArray
}

/// A Core ML model instance (one per compute unit: the engine runs them side by side).
public struct CoreMLDenoiseModel: DenoiseTileModel, @unchecked Sendable {
    // MLModel predictions are thread-safe; each instance is driven by one worker.
    let model: MLModel

    public init(model: MLModel) { self.model = model }

    /// Load the compiled model at `url` for `units`.
    public static func load(_ url: URL, units: MLComputeUnits) throws -> CoreMLDenoiseModel {
        let config = MLModelConfiguration()
        config.computeUnits = units
        do {
            return CoreMLDenoiseModel(model: try MLModel(contentsOf: url, configuration: config))
        } catch {
            throw DenoiseError.model(error.localizedDescription)
        }
    }

    public func denoise(_ input: MLMultiArray) throws -> MLMultiArray {
        let out = try model.prediction(from: MLDictionaryFeatureProvider(dictionary: ["input": MLFeatureValue(multiArray: input)]))
        guard let array = out.featureValue(for: "output")?.multiArrayValue else {
            throw DenoiseError.model("no \"output\" tensor")
        }
        return array
    }
}

/// Runs a photo's tiles through tile models, one worker thread per model (the GPU and
/// the Neural Engine work at the same time), then finishes the photo on the Rust side.
public enum DenoiseEngine {
    /// Denoise every tile of `tiles` with `models`, then `finish()`. `progress` gets the
    /// fraction of tiles done (from worker threads). Cancelling the calling task stops
    /// the workers after their current tile and throws `CancellationError` (nothing is
    /// stored). A model error stops every worker and is thrown. `scale`: output tiles are
    /// this many times larger than input tiles (AI Upscale's 2×).
    public static func run(_ tiles: any DenoiseTiles, models: [any DenoiseTileModel], scale: Int = 1,
                           progress: @escaping @Sendable (Double) -> Void = { _ in }) async throws {
        guard !models.isEmpty else { throw DenoiseError.model("no model instance") }
        let count = Int(try tiles.tileCount())
        let size = Int(try tiles.tileSize())
        let run = TileRun(count: count, scale: scale)
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (c: CheckedContinuation<Void, Error>) in
                DispatchQueue.global(qos: .userInitiated).async {
                    DispatchQueue.concurrentPerform(iterations: models.count) { w in
                        run.work(models[w], tiles: tiles, size: size, progress: progress)
                    }
                    c.resume(with: run.result)
                }
            }
        } onCancel: {
            run.cancel()
        }
        try Task.checkCancellation()
        try await tiles.finish()
    }
}

/// Shared state of one run's workers: the next tile, tiles done, the first error.
private final class TileRun: @unchecked Sendable {
    private let lock = NSLock()
    private let count: Int
    private let scale: Int
    private var next = 0
    private var done = 0
    private var failure: Error?
    private var cancelled = false

    init(count: Int, scale: Int) {
        self.count = count
        self.scale = scale
    }

    func cancel() { lock.withLock { cancelled = true } }

    var result: Result<Void, Error> {
        lock.withLock {
            if cancelled { return .failure(CancellationError()) }
            if let failure { return .failure(failure) }
            return .success(())
        }
    }

    /// The next tile to run, or nil when all are taken or the run stopped.
    private func take() -> Int? {
        lock.withLock {
            guard !cancelled, failure == nil, next < count else { return nil }
            defer { next += 1 }
            return next
        }
    }

    private func fail(_ e: Error) {
        lock.withLock { if failure == nil { failure = e } }
    }

    func work(_ model: any DenoiseTileModel, tiles: any DenoiseTiles, size: Int,
              progress: @Sendable (Double) -> Void) {
        let input: MLMultiArray
        do {
            input = try MLMultiArray(shape: [1, 3, NSNumber(value: size), NSNumber(value: size)], dataType: .float16)
        } catch {
            return fail(error)
        }
        let elements = UInt64(3 * size * size)
        while let i = take() {
            do {
                try input.withUnsafeMutableBytes { buf, _ in
                    try tiles.readTile(index: UInt32(i), destPtr: UInt64(UInt(bitPattern: buf.baseAddress)), destLen: elements)
                }
                let out = try model.denoise(input)
                try Self.write(out, tile: i, size: size * scale, to: tiles)
            } catch {
                return fail(error)
            }
            let f = lock.withLock {
                done += 1
                return Double(done) / Double(count)
            }
            progress(f)
        }
    }

    /// Hand a model output (`size`² per channel) to tile `i` through its strides (Core ML
    /// may pad rows).
    static func write(_ out: MLMultiArray, tile i: Int, size: Int, to tiles: any DenoiseTiles) throws {
        let shape = out.shape.map(\.intValue)
        guard out.dataType == .float16, shape.count == 4, shape[1] == 3, shape[2] == size, shape[3] == size else {
            throw DenoiseError.model("unexpected output \(out.dataType.rawValue) \(shape)")
        }
        try out.withUnsafeBytes { buf in
            let strides = out.strides.map(\.intValue)
            guard strides[3] == 1 else { throw DenoiseError.model("non-contiguous output rows") }
            try tiles.writeTile(index: UInt32(i), srcPtr: UInt64(UInt(bitPattern: buf.baseAddress)),
                                srcLen: UInt64(buf.count / 2), rowStride: UInt64(strides[2]),
                                channelStride: UInt64(strides[1]))
        }
    }
}

/// The Rust task (`begin_denoise`) as `DenoiseTiles`.
final class RustDenoiseTiles: DenoiseTiles, @unchecked Sendable {
    // The generated object is thread-safe (its methods lock on the Rust side).
    let task: DenoiseTask

    init(_ task: DenoiseTask) { self.task = task }

    func tileCount() throws -> UInt32 { try task.tileCount() }
    func tileSize() throws -> UInt32 { try task.tileSize() }

    func readTile(index: UInt32, destPtr: UInt64, destLen: UInt64) throws {
        try task.readTile(index: index, destPtr: destPtr, destLen: destLen)
    }

    func writeTile(index: UInt32, srcPtr: UInt64, srcLen: UInt64, rowStride: UInt64, channelStride: UInt64) throws {
        try task.writeTile(index: index, srcPtr: srcPtr, srcLen: srcLen, rowStride: rowStride, channelStride: channelStride)
    }

    func finish() async throws {
        let token = try CancelToken()
        try await withTaskCancellationHandler {
            try await task.finish(token: token)
        } onCancel: {
            try? token.cancel()
        }
    }
}
