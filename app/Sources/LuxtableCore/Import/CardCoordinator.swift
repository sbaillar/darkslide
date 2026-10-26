import Foundation

/// Remembers which volumes were already offered for import (persisted), so a
/// card left in the reader doesn't pop the sheet on every launch.
public protocol OfferedVolumeStore: AnyObject {
    func contains(_ id: String) -> Bool
    func insert(_ id: String)
}

public final class UserDefaultsOfferedStore: OfferedVolumeStore {
    let defaults: UserDefaults
    private let key: String
    /// Bounded so the list can't grow forever (oldest dropped).
    private let limit = 200

    public init(defaults: UserDefaults = .standard, key: String = "offeredCardVolumes") {
        self.defaults = defaults
        self.key = key
    }

    private var ids: [String] { defaults.stringArray(forKey: key) ?? [] }

    public func contains(_ id: String) -> Bool { ids.contains(id) }

    public func insert(_ id: String) {
        var list = ids.filter { $0 != id }
        list.append(id)
        defaults.set(Array(list.suffix(limit)), forKey: key)
    }
}

/// Decides when a detected card opens the import sheet.
///
/// - Live mode only: `--demo` runs on mocks and never auto-opens on a real card.
/// - Respects `settings.autoOpenImport`.
/// - On launch, cards already offered before (by volume UUID) are not offered
///   again; a card inserted while running is always offered.
/// - A card that arrives while a sheet is open waits in a queue until
///   `sheetDidClose()`.
/// - When a card disappears it leaves the queue and any import reading it stops.
@MainActor
public final class CardCoordinator {
    private weak var app: AppModel?
    let store: OfferedVolumeStore
    private let inspector: any VolumeInspecting
    private var watcher: CardWatcher?
    private let makeWatcher: @MainActor (CardCoordinator) -> CardWatcher
    public private(set) var queue: [URL] = []
    private var pending: [Task<Void, Never>] = []

    /// `source`/`detector` default to DiskArbitration (NSWorkspace fallback) and real volumes.
    public init(app: AppModel, source: VolumeEventSource? = nil, detector: DCIMDetector = .fileSystem,
                store: OfferedVolumeStore = UserDefaultsOfferedStore(),
                inspector: any VolumeInspecting = FileSystemVolumeInspector()) {
        self.app = app
        self.store = store
        self.inspector = inspector
        makeWatcher = { c in
            let onCard: @MainActor (URL) -> Void = { [weak c] in c?.cardArrived($0, atLaunch: false) }
            let onExisting: @MainActor (URL) -> Void = { [weak c] in c?.cardArrived($0, atLaunch: true) }
            let onEject: @MainActor (URL) -> Void = { [weak c] in c?.cardRemoved($0) }
            if let source {
                return CardWatcher(source: source, detector: detector, onCard: onCard, onExistingCard: onExisting,
                                   onEject: onEject)
            }
            return CardWatcher(onCard: onCard, onExistingCard: onExisting, onEject: onEject)
        }
    }

    public var isRunning: Bool { watcher?.isRunning ?? false }

    /// Starts watching in live mode; a no-op in demo mode.
    public func start() {
        guard let app, app.mode == .live else { return }
        let w = watcher ?? makeWatcher(self)
        watcher = w
        w.start()
    }

    public func stop() {
        watcher?.stop()
        pending.forEach { $0.cancel() }
        pending = []
    }

    /// Wait for in-flight decisions (tests).
    public func settle() async {
        await watcher?.settle()
        while !pending.isEmpty {
            let batch = pending
            pending = []
            for t in batch { await t.value }
        }
    }

    /// A card was detected (`atLaunch` = already mounted when we started).
    public func cardArrived(_ url: URL, atLaunch: Bool) {
        guard let app, app.mode == .live, app.settings.autoOpenImport else { return }
        let inspector = inspector
        pending.append(Task { [weak self] in
            let id = await inspector.traits(of: url)?.uuid ?? url.standardizedFileURL.path
            self?.decide(url, id: id, atLaunch: atLaunch)
        })
    }

    private func decide(_ url: URL, id: String, atLaunch: Bool) {
        guard let app else { return }
        if atLaunch, store.contains(id) { return }
        store.insert(id)
        if app.sheet == nil {
            app.sheet = .importCard(url)
        } else if case .importCard = app.sheet {
            // C1: a card mounted where the open sheet's card was may be another
            // card: the sheet scans it again (and finds its unfinished import, H2).
            if app.sheet != .importCard(url), !queue.contains(url) { queue.append(url) }
            pending.append(Task { await ImportRegistry.shared.volumeDidAppear(url) })
        } else if !queue.contains(url) {
            queue.append(url)
        }
    }

    /// Call when any sheet closes: present the next queued card.
    public func sheetDidClose() {
        guard let app, app.sheet == nil, !queue.isEmpty else { return }
        app.sheet = .importCard(queue.removeFirst())
    }

    public func cardRemoved(_ url: URL) {
        queue.removeAll { $0 == url }
        ImportRegistry.shared.volumeDidDisappear(url)
        if let app, app.sheet == .importCard(url) {
            app.show(Toast("\(url.lastPathComponent) was removed", symbol: "eject"))
        }
    }
}
