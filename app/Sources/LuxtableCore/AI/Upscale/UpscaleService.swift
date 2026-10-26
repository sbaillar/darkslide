import CoreML
import LuxtableFFI
import Foundation
import Observation

/// A photo's tiles whose `finish()` writes its upscaled DNG (`result`).
public protocol UpscalePhotoTiles: DenoiseTiles {
    /// The DNG `finish()` placed.
    var result: URL? { get }
}

/// The Rust side of AI Upscale (lt-ffi `upscale`). Seam for tests.
public protocol UpscaleBackend: Sendable {
    /// Decode and demosaic `url` and start its DNG next to it, named `name`: its tiles.
    func begin(_ url: URL, name: String) async throws -> any UpscalePhotoTiles
}

/// Stores that add a file made from a photo (AI Upscale's DNG) as a new photo.
public protocol DerivedPhotoAdding: Sendable {
    /// A file name for a photo made from `source` (at `url`), free on disk and in the
    /// session (a photo whose file is missing keeps its name): `<stem>-<suffix>.dng` or
    /// the first free `-N`.
    func freeDerivedName(for url: URL, from source: PhotoID, suffix: String) async throws -> String
    /// Add `url`, made from `source` and inside its session's folder, as a new photo that
    /// starts with the source's edit and metadata. Returns the new photo.
    func addDerived(_ url: URL, from source: PhotoID) async throws -> PhotoID
}

/// lt-ffi's `upscale` module.
public struct RustUpscaleBackend: UpscaleBackend {
    public init() {}

    public func begin(_ url: URL, name: String) async throws -> any UpscalePhotoTiles {
        let token = try CancelToken()
        do {
            let task = try await withTaskCancellationHandler {
                try await beginUpscale(path: url.path, name: name, token: token)
            } onCancel: {
                try? token.cancel()
            }
            return RustUpscaleTiles(task)
        } catch let e as DsError {
            if case .Cancelled = e { throw CancellationError() }
            throw RenderError(e)
        }
    }
}

/// The Rust task (`begin_upscale` or an export's) as tiles. A photo's `finish()` places
/// its DNG; an export's has nothing to finish (Rust takes the image back).
final class RustUpscaleTiles: UpscalePhotoTiles, @unchecked Sendable {
    // The generated object is thread-safe (its methods lock on the Rust side).
    let task: UpscaleTask
    let photo: Bool
    private let lock = NSLock()
    private var placed: URL?

    init(_ task: UpscaleTask, photo: Bool = true) {
        self.task = task
        self.photo = photo
    }

    var result: URL? { lock.withLock { placed } }

    func tileCount() throws -> UInt32 { task.tileCount() }
    func tileSize() throws -> UInt32 { task.tileSize() }

    func readTile(index: UInt32, destPtr: UInt64, destLen: UInt64) throws {
        try task.readTile(index: index, destPtr: destPtr, destLen: destLen)
    }

    func writeTile(index: UInt32, srcPtr: UInt64, srcLen: UInt64, rowStride: UInt64, channelStride: UInt64) throws {
        try task.writeTile(index: index, srcPtr: srcPtr, srcLen: srcLen, rowStride: rowStride, channelStride: channelStride)
    }

    func finish() async throws {
        guard photo else { return }
        let token = try CancelToken()
        let path: String
        do {
            path = try await withTaskCancellationHandler {
                try await task.finish(token: token)
            } onCancel: {
                try? token.cancel()
            }
        } catch DsError.Cancelled {
            throw CancellationError() // before the file was placed: nothing is left
        }
        lock.withLock { placed = URL(fileURLWithPath: path) }
    }
}

/// AI Upscale for the app: Photo ▸ Enhance ▸ Upscale 2× turns each selected photo into a
/// new photo twice its size (a linear DNG next to the original, starting with its edit),
/// one after another, with progress and Cancel. Exports with AI Upscale run the model
/// through `upscaleForExport`.
@MainActor @Observable
public final class UpscaleService {
    /// How far a run is.
    public struct Progress: Equatable, Sendable {
        /// The photo being upscaled (1-based) and how many the run has.
        public var photo: Int
        public var photos: Int
        /// Fraction of the current photo's tiles done.
        public var fraction: Double

        /// "Upscaling 2 of 3, 42%".
        public var status: String {
            let pct = "\(Int((fraction * 100).rounded()))%"
            return photos > 1 ? "Upscaling \(photo) of \(photos), \(pct)" : "Upscaling, \(pct)"
        }
    }

    /// Loads one model instance for a compute unit.
    public typealias ModelLoader = @Sendable (_ compiled: URL, _ units: MLComputeUnits) throws -> any DenoiseTileModel

    /// The app's instance: the model bundled in the app (`LT_BUNDLE_UPSCALE=1` builds), else
    /// the one downloaded into the support folder, else offered for download.
    public static let shared: UpscaleService = {
        let bundled = UpscaleModelDescriptor.bundledPackage(in: .main)
        let download = bundled == nil ? UpscaleModelDownload.standard() : nil
        return UpscaleService(
            model: DenoiseModelStore(
                package: bundled ?? download?.installedPackage,
                compiledRoot: AppPaths.support.appendingPathComponent("Models", isDirectory: true),
                expectedWeights: UpscaleModelDescriptor.weightsSHA256,
                name: UpscaleModelDescriptor.name, reasons: UpscaleModelDescriptor.reasons),
            backend: RustUpscaleBackend(), download: download)
    }()

    public let model: DenoiseModelStore
    /// The model's download when the build doesn't bundle it (nil: bundled).
    public let download: UpscaleModelDownload?
    /// Asked once before downloading, with `UpscaleModelDescriptor.downloadNotice` (the app
    /// shows an alert; true downloads).
    @ObservationIgnored public var confirmDownload: @MainActor (String) -> Bool = { _ in true }
    /// The run in progress, or nil.
    public private(set) var progress: Progress?
    /// The app whose selection is upscaled (set by `AppModel.live`).
    @ObservationIgnored public weak var app: AppModel?

    @ObservationIgnored private let backend: any UpscaleBackend
    @ObservationIgnored private let loadModels: ModelLoader
    @ObservationIgnored private var run: Task<Void, Never>?
    @ObservationIgnored private var fetching: Task<Void, Never>?
    /// Loaded model instances by compute units (raw value).
    @ObservationIgnored private var instances: [Int: any DenoiseTileModel] = [:]

    public init(model: DenoiseModelStore, backend: any UpscaleBackend, download: UpscaleModelDownload? = nil,
                loadModels: @escaping ModelLoader = { try CoreMLDenoiseModel.load($0, units: $1) }) {
        self.model = model
        self.backend = backend
        self.download = download
        self.loadModels = loadModels
    }

    /// A run is in progress (observable through `progress`).
    public var isRunning: Bool { progress != nil }

    /// The selected stills with a file.
    func targets(in app: AppModel) -> [(id: PhotoID, url: URL)] {
        app.selectedIDs.compactMap { id in
            guard let p = app.photo(id), !p.isVideo, let url = p.url else { return nil }
            return (id, url)
        }
    }

    /// This build downloads the model, and it isn't on this Mac yet or the downloaded copy
    /// failed its check: the menu offers the download (again).
    public var offersDownload: Bool {
        model.availability != .available && download?.isConfigured == true
    }

    /// Why the model can't run now (export recipes resample instead), or nil.
    public var modelUnavailableReason: String? {
        guard offersDownload else { return model.unavailableReason }
        return model.availability == .damaged ? UpscaleModelDescriptor.downloadDamaged : UpscaleModelDescriptor.notDownloaded
    }

    /// Why Upscale 2× can't run on `app`'s selection, or nil.
    public func unavailableReason(in app: AppModel) -> String? {
        if let r = modelUnavailableReason { return r }
        if !(app.sessions is any DerivedPhotoAdding) { return "This session can’t take new photos." }
        if targets(in: app).isEmpty { return "Select a photo to upscale." }
        return nil
    }

    /// The menu item works: a run or download to cancel, a download to start, or photos.
    public func isEnabled(in app: AppModel) -> Bool {
        isRunning || download?.isDownloading == true || offersDownload || unavailableReason(in: app) == nil
    }

    /// The menu item's help.
    public func help(in app: AppModel) -> String {
        if isRunning { return "Stop upscaling; photos already done stay" }
        if download?.isDownloading == true { return "Stop the download; nothing is kept" }
        if offersDownload { return "Download the AI Upscale model (about 34 MB) to this Mac" }
        return unavailableReason(in: app) ?? "Make a new photo twice the size of each selected photo with on-device AI"
    }

    /// Menu title: how many photos, the run's Cancel, or the model's download.
    public func title(in app: AppModel) -> String {
        if isRunning { return "Cancel Upscale" }
        if case .downloading(let f) = download?.state {
            return "Cancel AI Upscale Model Download (\(Int((f * 100).rounded()))%)"
        }
        if offersDownload { return "Download AI Upscale Model…" }
        let n = targets(in: app).count
        return n > 1 ? "Upscale \(n) Photos 2×" : "Upscale 2×"
    }

    /// Upscale the selected photos 2× in the background, one after another (or cancel
    /// the run in progress). Each becomes a new photo next to its original, starting with
    /// its edit; the last one is shown when done.
    public func toggle() {
        if isRunning { return cancel() }
        if let download, download.isDownloading { return download.cancel() }
        if offersDownload { return fetchModel() }
        guard let app, unavailableReason(in: app) == nil else { return }
        let photos = targets(in: app)
        progress = Progress(photo: 1, photos: photos.count, fraction: 0)
        run = Task { await self.upscale(photos, in: app) }
    }

    /// Stop the run: the photo in progress leaves no file; photos already done stay.
    public func cancel() {
        run?.cancel()
    }

    /// Wait for the run or download in progress (tests).
    func settle() async {
        await run?.value
        await fetching?.value
    }

    /// Download the model (after the user agrees to its terms) and use it from now on.
    private func fetchModel() {
        guard let download, fetching == nil, confirmDownload(UpscaleModelDescriptor.downloadNotice) else { return }
        fetching = Task {
            defer { fetching = nil }
            do {
                model.install(package: try await download.download())
                app?.show(Toast("AI Upscale model installed: Upscale 2× is ready", symbol: "arrow.up.left.and.arrow.down.right"))
            } catch is CancellationError {
            } catch {
                app?.show(error)
            }
        }
    }

    private func upscale(_ photos: [(id: PhotoID, url: URL)], in app: AppModel) async {
        defer {
            run = nil
            progress = nil
        }
        var added: [PhotoID] = []
        let session = app.currentSession?.id // ids mean nothing in any other session
        for (i, p) in photos.enumerated() {
            progress = Progress(photo: i + 1, photos: photos.count, fraction: 0)
            do {
                guard app.isStill(in: session) else { break }
                guard let store = app.sessions as? any DerivedPhotoAdding else {
                    throw UpscaleError.unavailable("This session can’t take new photos.")
                }
                let name = try await store.freeDerivedName(for: p.url, from: p.id, suffix: "Upscaled")
                let dng = try await upscale(p.url, name: name)
                added.append(try await Self.add(dng, from: p.id, to: store, app: app, session: session))
                if Task.isCancelled { break }
            } catch is CancellationError {
                break
            } catch {
                app.show(error)
                break
            }
        }
        guard !added.isEmpty, app.isStill(in: session) else { return }
        await app.refreshCurrentSession(keepingHistory: true)
        if let last = added.last, app.photo(last) != nil { app.focus(last, extend: false) }
        let name = added.count == 1 ? (app.photo(added[0])?.filename ?? "the new photo") : "\(added.count) new photos"
        app.show(Toast("Upscaled 2×: \(name)", symbol: "arrow.up.left.and.arrow.down.right"))
    }

    /// Add the placed DNG to the session. Not cancellable: a cancel that arrives once the
    /// file is placed must not leave it behind, unlisted. If the session refuses it, the
    /// file (ours, just made) is removed.
    static func add(_ dng: URL, from source: PhotoID, to store: any DerivedPhotoAdding,
                    app: AppModel, session: UUID?) async throws -> PhotoID {
        let saver = app.saver
        let adding = Task { @MainActor in
            await saver.flush() // the new photo starts with the edit as it is now
            // The store resolves `source` in the open session: if that changed, it is not this photo.
            guard app.isStill(in: session) else { throw CancellationError() }
            return try await store.addDerived(dng, from: source)
        }
        do {
            return try await adding.value
        } catch {
            try? FileManager.default.removeItem(at: dng)
            throw error
        }
    }

    /// Upscale `url` into its DNG named `name`; returns the DNG. Cancelling stops after
    /// the tiles in flight and leaves no file.
    func upscale(_ url: URL, name: String) async throws -> URL {
        let models = try await readyModels(units: [.cpuAndGPU, .cpuAndNeuralEngine])
        let tiles = try await backend.begin(url, name: name)
        try await DenoiseEngine.run(tiles, models: models, scale: 2) { f in
            Task { @MainActor [weak self] in
                if self?.progress != nil { self?.progress?.fraction = f }
            }
        }
        guard let dng = tiles.result else { throw UpscaleError.failed("no file was written") }
        return dng
    }

    /// For export (`AppUpscaleProvider`, from an export worker): run `tiles` (the rendered
    /// image) now on the Neural Engine. Returns false when this build has no model (the
    /// export resamples); a damaged model throws, so the photo fails with that reason.
    public func upscaleForExport(_ tiles: any DenoiseTiles) async throws -> Bool {
        if model.availability == .missing { return false }
        let models = try await readyModels(units: [.cpuAndNeuralEngine])
        try await DenoiseEngine.run(tiles, models: models, scale: 2)
        return true
    }

    /// Model instances for `units`, verifying, compiling and loading them on first use.
    /// A compiled model that doesn't load is compiled again once.
    private func readyModels(units: [MLComputeUnits]) async throws -> [any DenoiseTileModel] {
        var attempt = 0
        while true {
            let have = units.compactMap { instances[$0.rawValue] }
            if have.count == units.count { return have }
            let compiled: URL
            do {
                compiled = try await model.prepare()
            } catch DenoiseError.missing {
                throw UpscaleError.unavailable(UpscaleModelDescriptor.reasons.missing)
            } catch DenoiseError.damaged {
                throw UpscaleError.unavailable(modelUnavailableReason ?? UpscaleModelDescriptor.reasons.damaged)
            }
            let load = loadModels
            do {
                let models = try await Task.detached(priority: .userInitiated) {
                    try units.map { try load(compiled, $0) }
                }.value
                for (u, m) in zip(units, models) { instances[u.rawValue] = m }
                return models
            } catch {
                attempt += 1
                instances = [:]
                model.discard()
                if attempt > 1 { throw error }
            }
        }
    }
}

/// The app's AI Upscale for Rust's export (`set_upscale_provider`): Rust calls `upscale`
/// from an export worker with the rendered image's tiles, and this blocks that worker until
/// the model ran over them. A failure fails that photo, not the export; cancelling the
/// export (its token) cancels the run within ~50 ms.
final class AppUpscaleProvider: UpscaleProvider, @unchecked Sendable {
    // Immutable; `upscale` keeps its per-call state local.
    let runOne: @Sendable (any DenoiseTiles) async throws -> Bool

    init(_ runOne: @escaping @Sendable (any DenoiseTiles) async throws -> Bool) {
        self.runOne = runOne
    }

    /// Make it Rust's provider for exports.
    static func install(_ runOne: @escaping @Sendable (any DenoiseTiles) async throws -> Bool) {
        setUpscaleProvider(provider: AppUpscaleProvider(runOne))
    }

    func upscale(task: UpscaleTask, cancel: CancelToken) throws -> Bool {
        let done = DispatchSemaphore(value: 0)
        let outcome = Outcome()
        let work = runOne
        let tiles = RustUpscaleTiles(task, photo: false)
        let run = Task.detached {
            do {
                outcome.set(.success(try await work(tiles)))
            } catch {
                outcome.set(.failure(error))
            }
            done.signal()
        }
        while done.wait(timeout: .now() + .milliseconds(50)) == .timedOut {
            if (try? cancel.isCancelled()) == true { run.cancel() }
        }
        switch outcome.result {
        case .success(let available): return available
        case .failure(let e) where e is CancellationError: throw DsError.Cancelled
        case .failure(let e): throw DsError.Unsupported(message: (e as? LocalizedError)?.errorDescription ?? "\(e)")
        case nil: throw DsError.Internal(message: "AI Upscale ended without a result")
        }
    }

    private final class Outcome: @unchecked Sendable {
        private let lock = NSLock()
        private var stored: Result<Bool, Error>?
        func set(_ r: Result<Bool, Error>) { lock.withLock { stored = r } }
        var result: Result<Bool, Error>? { lock.withLock { stored } }
    }
}
