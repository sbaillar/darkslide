import LuxtableFFI
import Foundation
import Observation

/// The removal engine on the app side: Rust keeps fills in `<support>/Cache/Removals`,
/// fills with PatchMatch until the LaMa model is ready, then with `LamaInpainter`.
/// The model is downloaded the first time the user removes something.
@MainActor @Observable
public final class RemovalModel {
    public static let shared = RemovalModel(model: .standard(.lama))

    public let model: DownloadableModel
    /// The inpainter registered with Rust (nil: PatchMatch).
    public private(set) var registered = false
    /// Bumped each time the engine filling removals changes: what shows removals (the
    /// viewer, the grid's developed thumbnails) renders them again.
    public private(set) var engineVersion = 0
    /// Called on the main actor when the engine changes (the app re-renders the photo on
    /// screen; its fills are cached per engine, so they are made again).
    @ObservationIgnored public var onEngineChange: @MainActor () -> Void = {}
    @ObservationIgnored private let register: @MainActor (DevelopInpainter?) -> Void
    @ObservationIgnored private let setCacheDir: @MainActor (String?) -> Void
    @ObservationIgnored private var loading: Task<Void, Never>?

    init(model: DownloadableModel,
         register: @escaping @MainActor (DevelopInpainter?) -> Void = { setInpainter(inpainter: $0) },
         setCacheDir: @escaping @MainActor (String?) -> Void = { setRemovalCacheDir(dir: $0) }) {
        self.model = model
        self.register = register
        self.setCacheDir = setCacheDir
    }

    /// Where Rust keeps removal fills across launches.
    public static var cacheDirectory: URL {
        AppPaths.support.appendingPathComponent("Cache/Removals", isDirectory: true)
    }

    /// At launch: the fill cache, and LaMa when it was downloaded before.
    public func start(cacheDirectory: URL = RemovalModel.cacheDirectory) {
        try? FileManager.default.createDirectory(at: cacheDirectory, withIntermediateDirectories: true)
        setCacheDir(cacheDirectory.path)
        if model.state == .ready { activate() }
    }

    /// The user removed something: download LaMa if needed (PatchMatch fills meanwhile)
    /// and switch to it when it's ready. Does nothing while a download runs.
    public func noteRemoveUsed() {
        guard !registered else { return }
        if case .failed = model.state { return } // the panel offers Try Again
        activate()
    }

    /// Download (or load) and register LaMa.
    public func activate() {
        guard loading == nil, !registered else { return }
        loading = Task { [weak self] in
            guard let self else { return }
            defer { self.loading = nil }
            guard let m = try? await self.model.loadedModel() else { return }
            self.register(LamaInpainter(model: CoreMLInpaintModel(model: m)))
            self.registered = true
            self.engineVersion += 1
            self.onEngineChange()
        }
    }

    /// Before an export of photos with removals: LaMa on disk is waited for (it registers a
    /// few seconds after launch), so the export fills them as the viewer shows them. Returns
    /// why it can't (the model on disk doesn't load), or nil. Without a downloaded model
    /// both fill with PatchMatch. The wait ends when the calling task (the export) is
    /// cancelled.
    public func exportProblem() async -> String? {
        if registered { return nil }
        guard model.state == .ready else { return nil }
        activate()
        if let loading { await Self.wait(for: loading) }
        if Task.isCancelled { return "the export was cancelled" }
        if registered { return nil }
        return "the removal model couldn't be loaded (\(status)), and its fills would differ from the ones previewed"
    }

    /// Cancel the download; PatchMatch keeps filling.
    public func cancel() {
        model.cancel()
    }

    /// Wait for the current activation (tests).
    func settle() async {
        await loading?.value
    }

    /// Wait for `task`, or until the calling task is cancelled (`task` keeps running).
    nonisolated static func wait(for task: Task<Void, Never>) async {
        let once = OneShot()
        await withTaskCancellationHandler {
            await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
                once.arm(c)
                Task {
                    await task.value
                    once.fire()
                }
            }
        } onCancel: {
            once.fire()
        }
    }

    /// What the Remove panel says about the engine.
    public var status: String {
        switch model.state {
        case .ready: registered ? "Removing with LaMa on this Mac." : "Preparing the removal model…"
        case .notDownloaded: "Basic fill. The first removal downloads a 217 MB model for better fills."
        case .downloading(let f): "Downloading the removal model… \(Int((f * 100).rounded())) %"
        case .failed(let m): m
        }
    }
}

/// A continuation resumed once, by whichever comes first (also before it is armed).
private final class OneShot: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Void, Never>?
    private var fired = false

    func arm(_ c: CheckedContinuation<Void, Never>) {
        let now = lock.withLock {
            if fired { return true }
            continuation = c
            return false
        }
        if now { c.resume() }
    }

    func fire() {
        let c: CheckedContinuation<Void, Never>? = lock.withLock {
            guard !fired else { return nil }
            fired = true
            defer { continuation = nil }
            return continuation
        }
        c?.resume()
    }
}

extension AppModel {
    /// The removal engine changed: the photo on screen renders its removals again (with
    /// the new engine; fills are cached per engine).
    func removalEngineChanged() {
        guard let id = focusedID, !edit(for: id).removals.isEmpty else { return }
        viewer.editsChanged([id])
    }
}
