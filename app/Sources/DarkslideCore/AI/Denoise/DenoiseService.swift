import CoreML
import DarkslideFFI
import Foundation
import Observation

/// The Rust side of AI Denoise (results cache, tiles). Seam for tests.
public protocol DenoiseBackend: Sendable {
    /// Results are keyed by this model identity; `nil` turns AI Denoise off.
    func setModel(_ id: String?)
    func setCache(_ dir: URL?, maxBytes: UInt64) throws
    /// Whether `url` has a result for the current model (memory or disk).
    func isReady(_ url: URL) async throws -> Bool
    /// Decode and demosaic `url` and return its tiles.
    func begin(_ url: URL, background: Bool) async throws -> any DenoiseTiles
}

/// ds-ffi's `denoise` module.
public struct RustDenoiseBackend: DenoiseBackend {
    public init() {}

    public func setModel(_ id: String?) { try? setDenoiseModel(modelId: id) }

    public func setCache(_ dir: URL?, maxBytes: UInt64) throws {
        do {
            try setDenoiseCache(dir: dir?.path, maxBytes: maxBytes)
        } catch let e as DsError {
            throw RenderError(e)
        }
    }

    public func isReady(_ url: URL) async throws -> Bool {
        let token = try CancelToken()
        return try await Self.call(token) { try await denoiseReady(path: url.path, token: token) }
    }

    public func begin(_ url: URL, background: Bool) async throws -> any DenoiseTiles {
        let token = try CancelToken()
        let task = try await Self.call(token) { try await beginDenoise(path: url.path, background: background, token: token) }
        return RustDenoiseTiles(task)
    }

    private static func call<T: Sendable>(_ token: CancelToken, _ body: () async throws -> T) async throws -> T {
        do {
            return try await withTaskCancellationHandler {
                try await body()
            } onCancel: {
                try? token.cancel()
            }
        } catch let e as DsError {
            if case .Cancelled = e { throw CancellationError() }
            throw RenderError(e)
        }
    }
}

/// AI Denoise for the app: runs photos through the bundled model
/// one at a time (the photo on screen ahead of a batch), and tells the viewer when a
/// photo's denoised image is ready. Results live in Rust (memory and the disk cache);
/// every develop render of a photo with AI Denoise on uses them.
@MainActor @Observable
public final class DenoiseService {
    public enum PhotoState: Equatable, Sendable {
        case queued
        /// Fraction of the photo's tiles done.
        case running(Double)
        case ready
        case failed(String)
    }

    /// Loads one model instance for a compute unit. A batch (`background`) runs on the
    /// Neural Engine only, leaving the GPU to the viewer; the photo on screen uses the GPU
    /// and the Neural Engine side by side (~3 s for a 24 MP frame on an M4 Pro).
    public typealias ModelLoader = @Sendable (_ compiled: URL, _ units: MLComputeUnits) throws -> any DenoiseTileModel

    /// Disk cache of results: `<support>/DenoiseCache`, at most this many bytes (a 24 MP
    /// result is 144 MB: about 55 photos).
    public static let cacheBytes: UInt64 = 8 << 30

    /// The app's instance, with the model bundled in the app (if this build has one).
    public static let shared = DenoiseService(
        model: DenoiseModelStore(
            package: DenoiseModelDescriptor.bundledPackage(in: .main),
            compiledRoot: AppPaths.support.appendingPathComponent("Models", isDirectory: true)),
        backend: RustDenoiseBackend(),
        cacheDir: AppPaths.support.appendingPathComponent("DenoiseCache", isDirectory: true))

    public let model: DenoiseModelStore
    public private(set) var states: [URL: PhotoState] = [:]
    /// The app whose viewer shows finished photos (set by `AppModel.make`).
    @ObservationIgnored public weak var app: AppModel?

    @ObservationIgnored private let backend: any DenoiseBackend
    @ObservationIgnored private let loadModels: ModelLoader
    @ObservationIgnored private let cacheDir: URL?
    @ObservationIgnored private var configured = false
    @ObservationIgnored private var pending: [(url: URL, background: Bool)] = []
    /// Who waits for the next run of each queued photo.
    @ObservationIgnored private var waiters: [URL: [Waiter]] = [:]
    @ObservationIgnored private var worker: Task<Void, Never>?
    /// The model check `configure` starts (it tells Rust the model's identity when done).
    @ObservationIgnored private var verifying: Task<Bool, Never>?
    @ObservationIgnored private var running: Run?
    /// Loaded model instances by compute units (raw value).
    @ObservationIgnored private var instances: [Int: any DenoiseTileModel] = [:]

    public init(model: DenoiseModelStore, backend: any DenoiseBackend, cacheDir: URL?,
                loadModels: @escaping ModelLoader = { try CoreMLDenoiseModel.load($0, units: $1) }) {
        self.model = model
        self.backend = backend
        self.cacheDir = cacheDir
        self.loadModels = loadModels
    }

    /// Point Rust at the disk cache and, once the bundled model checks out, at its identity
    /// (results of other models are never used; without a model renders and exports
    /// ignore AI Denoise). Idempotent.
    public func configure() {
        guard !configured else { return }
        configured = true
        try? backend.setCache(cacheDir, maxBytes: Self.cacheBytes)
        guard model.availability == .available else { return }
        verifying = Task { await self.verifiedModel() }
    }

    /// Wait for the background work this service started (the model check, the queue).
    /// Rust's denoise state is process-wide: a test settles before the next one uses it.
    func settle() async {
        _ = await verifying?.value
        while let w = worker { await w.value }
    }

    /// Whether the model is usable, checking it first (and telling Rust its identity).
    @discardableResult
    func verifiedModel() async -> Bool {
        guard (try? await model.verify()) != nil else { return false }
        backend.setModel(model.id)
        return true
    }

    /// Whether `edit` asks for AI Denoise (Rust's rule: the strength rounds to at least 1).
    public nonisolated static func wants(_ edit: EditState) -> Bool {
        edit.noise.aiDenoise && edit.noise.aiStrength.rounded() >= 1
    }

    // MARK: - Queue

    typealias Waiter = (id: UUID, continuation: CheckedContinuation<Void, Error>)

    /// The photo being denoised, and who waits for this run.
    private final class Run {
        let url: URL
        let task: Task<Void, Error>
        var waiters: [Waiter]
        init(url: URL, task: Task<Void, Error>, waiters: [Waiter]) {
            self.url = url
            self.task = task
            self.waiters = waiters
        }
    }

    /// Denoise `url` and wait until its result is stored. Requests for a photo already
    /// queued or running join it (a run being cancelled is not joined: a fresh one is
    /// queued); `background` requests run after the others. Cancelling the calling task
    /// stops waiting (and the photo's job, when nobody else waits for it).
    public func denoise(_ url: URL, background: Bool = false) async throws {
        let id = UUID()
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (c: CheckedContinuation<Void, Error>) in
                if Task.isCancelled {
                    c.resume(throwing: CancellationError())
                    return
                }
                if let run = running, run.url == url, !run.task.isCancelled {
                    run.waiters.append((id, c))
                } else {
                    waiters[url, default: []].append((id, c))
                }
                enqueue(url, background: background)
            }
        } onCancel: {
            Task { @MainActor [weak self] in self?.stopWaiting(id, for: url) }
        }
    }

    /// A waiter for `url` went away: resume it cancelled; when nobody is left waiting for
    /// a run (the current one, or a queued one), stop it.
    private func stopWaiting(_ id: UUID, for url: URL) {
        if let run = running, run.url == url, let i = run.waiters.firstIndex(where: { $0.id == id }) {
            run.waiters.remove(at: i).continuation.resume(throwing: CancellationError())
            if run.waiters.isEmpty { run.task.cancel() }
            return
        }
        guard var list = waiters[url], let i = list.firstIndex(where: { $0.id == id }) else { return }
        list.remove(at: i).continuation.resume(throwing: CancellationError())
        waiters[url] = list.isEmpty ? nil : list
        if list.isEmpty { dequeue(url) }
    }

    /// Queue `url` (no-op when it is queued, or running and not being cancelled). A
    /// foreground request is the photo on screen: it runs next, and earlier foreground
    /// entries (photos the user moved away from) become background work.
    public func enqueue(_ url: URL, background: Bool) {
        configure()
        if !background {
            for i in pending.indices where pending[i].url != url { pending[i].background = true }
        }
        if let run = running, run.url == url, !run.task.isCancelled { return }
        if let i = pending.firstIndex(where: { $0.url == url }) {
            if !background, pending[i].background { pending[i].background = false }
            return
        }
        pending.append((url, background))
        states[url] = .queued
        if worker == nil {
            worker = Task { await self.drain() }
        }
    }

    /// Stop `url` (queued and running): it is not stored; whoever waits gets
    /// `CancellationError`.
    public func cancel(_ url: URL) {
        if let run = running, run.url == url { run.task.cancel() }
        dequeue(url)
    }

    /// [session close] Stop the work for `urls` that nobody waits for (the viewer's and
    /// "Denoise Selected" requests). A photo an export is waiting for keeps its run.
    public func cancelUnwaited(_ urls: Set<URL>) {
        if let run = running, urls.contains(run.url), run.waiters.isEmpty { run.task.cancel() }
        for url in pending.map(\.url) where urls.contains(url) && (waiters[url] ?? []).isEmpty {
            dequeue(url)
        }
    }

    /// Take `url` out of the queue, its waiters cancelled.
    private func dequeue(_ url: URL) {
        guard let i = pending.firstIndex(where: { $0.url == url }) else { return }
        pending.remove(at: i)
        if running?.url != url { states[url] = nil }
        for w in waiters.removeValue(forKey: url) ?? [] { w.continuation.resume(throwing: CancellationError()) }
    }

    private func drain() async {
        while let next = takeNext() {
            let task = Task { try await self.process(next.url, background: next.background) }
            let run = Run(url: next.url, task: task, waiters: waiters.removeValue(forKey: next.url) ?? [])
            running = run
            let result = await task.result
            running = nil
            switch result {
            case .success:
                states[next.url] = .ready
                photoReady(next.url)
            case .failure(let e) where e is CancellationError:
                // A newcomer may have queued a fresh run meanwhile.
                states[next.url] = pending.contains { $0.url == next.url } ? .queued : nil
            case .failure(let e):
                states[next.url] = .failed((e as? LocalizedError)?.errorDescription ?? e.localizedDescription)
            }
            for w in run.waiters { w.continuation.resume(with: result) }
        }
        worker = nil
    }

    /// The photo on screen first, then foreground requests, then the batch in order.
    private func takeNext() -> (url: URL, background: Bool)? {
        guard !pending.isEmpty else { return nil }
        let focused = app?.focusedID.flatMap { app?.photo($0)?.url }
        let i = pending.firstIndex { $0.url == focused } ?? pending.firstIndex { !$0.background } ?? 0
        return pending.remove(at: i)
    }

    private func process(_ url: URL, background: Bool) async throws {
        states[url] = .running(0)
        let models = try await readyModels(background: background)
        let tiles = try await backend.begin(url, background: background)
        try await DenoiseEngine.run(tiles, models: models) { f in
            Task { @MainActor [weak self] in
                if case .running = self?.states[url] { self?.states[url] = .running(f) }
            }
        }
    }

    /// Compute units of a run: the GPU and the Neural Engine side by side, a batch on the
    /// Neural Engine alone (the GPU stays the viewer's).
    nonisolated static func units(background: Bool) -> [MLComputeUnits] {
        background ? [.cpuAndNeuralEngine] : [.cpuAndGPU, .cpuAndNeuralEngine]
    }

    /// The model instances for a run, compiling and loading them on first use. A compiled
    /// model that doesn't load is compiled again once.
    private func readyModels(background: Bool) async throws -> [any DenoiseTileModel] {
        var attempt = 0
        while true {
            let units = Self.units(background: background)
            let have = units.compactMap { instances[$0.rawValue] }
            if have.count == units.count { return have }
            let compiled = try await model.prepare()
            backend.setModel(model.id)
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

    // MARK: - The app

    /// A photo's result was stored: if it's on screen, render it again.
    private func photoReady(_ url: URL) {
        guard let app, let id = app.focusedID, app.photo(id)?.url == url else { return }
        app.viewer.editsChanged([id])
    }

    /// The focused photo asks for AI Denoise: show whether it is ready, denoise it if not
    /// (the first time, or after its cached result was dropped). Failed photos wait for
    /// `retry`.
    public func checkFocused() async {
        guard let app, let id = app.focusedID, Self.wants(app.edit(for: id)), let url = app.photo(id)?.url else { return }
        configure()
        switch states[url] {
        case .queued, .running, .failed: return
        case .ready, nil: break
        }
        guard await verifiedModel() else { return }
        if (try? await backend.isReady(url)) == true {
            states[url] = .ready
        } else {
            enqueue(url, background: false)
        }
    }

    /// Try a failed photo again.
    public func retry(_ url: URL) {
        states[url] = nil
        enqueue(url, background: false)
    }

    /// Turn AI Denoise on for the selected photos (one undo step) and denoise them in the
    /// background, the focused one first. Returns how many were queued.
    @discardableResult
    public func denoiseSelected() -> Int {
        guard let app, model.availability == .available else { return 0 }
        let ids = app.selectedIDs.filter { app.photo($0)?.isVideo == false }
        guard !ids.isEmpty else { return 0 }
        app.updateEdits(ids, label: "AI Denoise") { _, e in
            e.noise.aiDenoise = true
            if e.noise.aiStrength <= 0 { e.noise.aiStrength = NoiseEdit().aiStrength }
        }
        var queued = 0
        for id in ids {
            guard let url = app.photo(id)?.url else { continue }
            if case .ready = states[url] { continue }
            states[url] = nil // a failed photo is tried again
            enqueue(url, background: id != app.focusedID)
            queued += 1
        }
        return queued
    }

    /// For export, just before a photo renders (`AppDenoiseProvider`, from an export
    /// worker): make sure `url` has its result, denoising it now if needed.
    ///
    /// Returns false when this build has no model (the original is exported). Waits for
    /// the model's check (an export at launch); a damaged model throws, so the photo fails
    /// with that reason. Runs as background work (the Neural Engine), never ahead of the
    /// photo on screen.
    public func ensureForExport(_ url: URL) async throws -> Bool {
        configure()
        if model.availability == .missing { return false }
        guard await verifiedModel() else { throw DenoiseError.damaged }
        if (try? await backend.isReady(url)) == true { return true }
        try await denoise(url, background: true)
        return true
    }
}
