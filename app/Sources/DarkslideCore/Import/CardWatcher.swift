import Foundation

/// Mount/unmount events plus the currently mounted volumes.
@MainActor
public protocol VolumeEventSource: AnyObject {
    func start(onMount: @escaping @MainActor (URL) -> Void, onUnmount: @escaping @MainActor (URL) -> Void)
    func stop()
    /// Enumerated off the main thread (mount tables can include hung network shares).
    func mountedVolumes() async -> [URL]
}

/// Decides whether a volume looks like a camera card: it is local and has a `DCIM/` directory.
/// Mounted disk images (test and dev images, often `-nobrowse`) are never offered on
/// their own (`skipsDiskImages`); the user can still pick one from the import sheet.
///
/// The check runs off the main thread with a timeout. Network volumes are
/// skipped before any path inside them is touched, so a hung NFS mount
/// (a NAS share) can never stall the UI.
public struct DCIMDetector: Sendable {
    public let isDirectory: @Sendable (URL) -> Bool
    /// `nil` = don't check locality (tests that use plain temp folders).
    public let inspector: (any VolumeInspecting)?
    public let timeout: Double
    /// Disk images are not cards (automatic offers).
    public let skipsDiskImages: Bool

    public init(isDirectory: @escaping @Sendable (URL) -> Bool, inspector: (any VolumeInspecting)? = nil,
                timeout: Double = 1, skipsDiskImages: Bool = false) {
        self.isDirectory = isDirectory
        self.inspector = inspector
        self.timeout = timeout
        self.skipsDiskImages = skipsDiskImages
    }

    public static let directoryExists: @Sendable (URL) -> Bool = { url in
        var dir: ObjCBool = false
        return FileManager.default.fileExists(atPath: url.path, isDirectory: &dir) && dir.boolValue
    }

    /// Real volumes: locality + `DCIM/`, never a disk image (what the app offers on its own).
    public static let fileSystem = DCIMDetector(isDirectory: directoryExists, inspector: FileSystemVolumeInspector(),
                                                skipsDiskImages: true)

    /// Real volumes including disk images: the import sheet the user opened.
    public static let fileSystemIncludingImages = DCIMDetector(isDirectory: directoryExists,
                                                               inspector: FileSystemVolumeInspector())

    /// Folders the user picked (no locality requirement).
    public static let folders = DCIMDetector(isDirectory: directoryExists)

    public func isCard(_ volume: URL) async -> Bool {
        if let inspector {
            guard let traits = await inspector.traits(of: volume), traits.isLocal,
                  !(skipsDiskImages && traits.isDiskImage) else { return false }
        }
        let dcim = volume.appendingPathComponent("DCIM", isDirectory: true)
        let check = isDirectory
        return await blockingWithTimeout(timeout) { check(dcim) } ?? false
    }
}

/// Workspace mount notifications (`PlatformWorkspace`): the fallback source; misses `nobrowse` mounts.
/// The center is injectable so tests can post fakes.
@MainActor
public final class WorkspaceVolumeEvents: VolumeEventSource {
    private let center: NotificationCenter
    private var tokens: [NSObjectProtocol] = []

    public init(center: NotificationCenter = PlatformWorkspace.notificationCenter) {
        self.center = center
    }

    public func start(onMount: @escaping @MainActor (URL) -> Void, onUnmount: @escaping @MainActor (URL) -> Void) {
        stop()
        func observe(_ name: Notification.Name, _ handler: @escaping @MainActor (URL) -> Void) -> NSObjectProtocol {
            center.addObserver(forName: name, object: nil, queue: .main) { note in
                guard let url = note.userInfo?[PlatformWorkspace.volumeURLKey] as? URL else { return }
                MainActor.assumeIsolated { handler(url) }
            }
        }
        tokens = [observe(PlatformWorkspace.didMount, onMount),
                  observe(PlatformWorkspace.didUnmount, onUnmount)]
    }

    public func stop() {
        tokens.forEach(center.removeObserver)
        tokens = []
    }

    public func mountedVolumes() async -> [URL] {
        await VolumeList.mounted(includeHidden: false)
    }
}

enum VolumeList {
    /// Mounted volumes except `/`, read on a GCD worker with a timeout.
    static func mounted(includeHidden: Bool) async -> [URL] {
        await blockingWithTimeout(2) {
            PlatformWorkspace.mountedVolumes(includeHidden: includeHidden).filter { $0.path != "/" }
        } ?? []
    }
}

/// Watches for memory cards. Reports cards already mounted at `start()`
/// (`onExistingCard`), then every newly mounted card (`onCard`), and every unmount.
@MainActor
public final class CardWatcher {
    private let source: VolumeEventSource
    private let detector: DCIMDetector
    private let onCard: @MainActor (URL) -> Void
    private let onExistingCard: @MainActor (URL) -> Void
    private let onEject: @MainActor (URL) -> Void
    public private(set) var cards: [URL] = []
    public private(set) var isRunning = false
    /// Checks in flight (tests await them).
    private var pending: [Task<Void, Never>] = []
    /// DiskArbitration replays every already-mounted disk as "appeared" right
    /// after registration. Until the initial mount scan has finished, mount events
    /// count as existing volumes (so the offered-volume check applies), and so
    /// does any mount that the initial scan listed.
    private var initialScanDone = false
    private var initialVolumes: Set<URL> = []

    public init(source: VolumeEventSource, detector: DCIMDetector = .fileSystem,
                onCard: @escaping @MainActor (URL) -> Void, onExistingCard: (@MainActor (URL) -> Void)? = nil,
                onEject: @escaping @MainActor (URL) -> Void) {
        self.source = source
        self.detector = detector
        self.onCard = onCard
        self.onExistingCard = onExistingCard ?? onCard
        self.onEject = onEject
    }

    /// Production wiring: DiskArbitration (sees `nobrowse` mounts), NSWorkspace as fallback.
    public convenience init(onCard: @escaping @MainActor (URL) -> Void, onExistingCard: (@MainActor (URL) -> Void)? = nil,
                            onEject: @escaping @MainActor (URL) -> Void) {
        #if os(macOS)
        let source: VolumeEventSource = DiskArbitrationVolumeEvents() ?? WorkspaceVolumeEvents()
        #else
        let source: VolumeEventSource = WorkspaceVolumeEvents() // iOS (unverified): no mounts; phase 4 uses ImageCaptureCore
        #endif
        self.init(source: source, onCard: onCard, onExistingCard: onExistingCard, onEject: onEject)
    }

    public func start() {
        guard !isRunning else { return }
        isRunning = true
        initialScanDone = false
        initialVolumes = []
        source.start(onMount: { [weak self] in self?.mounted($0) },
                     onUnmount: { [weak self] in self?.unmounted($0) })
        let source = source
        pending.append(Task { [weak self] in
            let volumes = await source.mountedVolumes()
            self?.initialScanFinished(volumes)
        })
    }

    private func initialScanFinished(_ volumes: [URL]) {
        initialVolumes = Set(volumes.map(\.standardizedFileURL))
        initialScanDone = true
        for v in volumes { check(v, existing: true) }
    }

    /// Whether a mount event is a replay of a volume that was there before `start()`.
    func isReplay(_ url: URL) -> Bool {
        !initialScanDone || initialVolumes.contains(url.standardizedFileURL)
    }

    private func mounted(_ url: URL) {
        check(url, existing: isReplay(url))
    }

    public func stop() {
        guard isRunning else { return }
        isRunning = false
        source.stop()
        pending.forEach { $0.cancel() }
        pending = []
    }

    /// Wait for outstanding checks (tests).
    public func settle() async {
        while !pending.isEmpty {
            let batch = pending
            pending = []
            for t in batch { await t.value }
        }
    }

    private func check(_ url: URL, existing: Bool) {
        let detector = detector
        pending.append(Task { [weak self] in
            guard await detector.isCard(url) else { return }
            self?.found(url, existing: existing)
        })
    }

    private func found(_ url: URL, existing: Bool) {
        guard isRunning, !cards.contains(url) else { return }
        cards.append(url)
        if existing { onExistingCard(url) } else { onCard(url) }
    }

    private func unmounted(_ url: URL) {
        // A volume that comes back after an unmount is a new insertion.
        initialVolumes.remove(url.standardizedFileURL)
        cards.removeAll { $0 == url }
        onEject(url)
    }

    /// Cards mounted right now (for opening the import sheet without a notification).
    public static func mountedCards(source: VolumeEventSource, detector: DCIMDetector = .fileSystem) async -> [URL] {
        var out: [URL] = []
        for v in await source.mountedVolumes() where await detector.isCard(v) { out.append(v) }
        return out
    }
}

/// Ejects a volume after a verified import. Never deletes anything.
public protocol Ejecting: Sendable {
    func eject(_ volume: URL) async throws
}

public struct WorkspaceEjector: Ejecting {
    public init() {}

    public func eject(_ volume: URL) async throws {
        // unmountAndEjectDevice blocks while the disk flushes; keep it off the main thread.
        try await Task.detached(priority: .utility) {
            try PlatformWorkspace.unmountAndEject(volume)
        }.value
    }
}
