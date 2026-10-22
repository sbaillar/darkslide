import Foundation

/// Simulated rsync archive: syncs periodically, reports progress, and goes
/// offline when the target folder's volume is missing (like an unmounted NAS).
public final class MockArchiveService: ArchiveService, @unchecked Sendable {
    private let target: String
    private let interval: Duration
    private let step: Duration
    private let isReachable: @Sendable (String) -> Bool
    private let lock = NSLock()
    private var continuations: [UUID: AsyncStream<ArchiveStatus>.Continuation] = [:]
    private var current: ArchiveStatus
    private var loop: Task<Void, Never>?

    public init(target: String, interval: Duration = .seconds(600), step: Duration = .milliseconds(120),
                pending: Int = 14, isReachable: @escaping @Sendable (String) -> Bool = MockArchiveService.volumeMounted) {
        self.target = target
        self.interval = interval
        self.step = step
        self.isReachable = isReachable
        current = ArchiveStatus(state: pending > 0 ? .behind(files: pending) : .idle, target: target, lastSuccess: nil)
    }

    deinit { loop?.cancel() }

    /// `/Volumes/<name>/…` targets need that volume; anything else just needs the parent folder.
    public static func volumeMounted(_ path: String) -> Bool {
        let comps = URL(fileURLWithPath: path).pathComponents
        if comps.count >= 3, comps[1] == "Volumes" {
            return FileManager.default.fileExists(atPath: "/Volumes/\(comps[2])")
        }
        return FileManager.default.fileExists(atPath: URL(fileURLWithPath: path).deletingLastPathComponent().path)
    }

    public var status: ArchiveStatus { lock.withLock { current } }

    public func statusUpdates() -> AsyncStream<ArchiveStatus> {
        let id = UUID()
        let (stream, cont) = AsyncStream<ArchiveStatus>.makeStream()
        lock.withLock {
            continuations[id] = cont
            cont.yield(current)
            if loop == nil {
                loop = Task { [weak self] in
                    while !Task.isCancelled {
                        await self?.syncNow()
                        try? await Task.sleep(for: self?.interval ?? .seconds(600))
                    }
                }
            }
        }
        cont.onTermination = { [weak self] _ in self?.lock.withLock { _ = self?.continuations.removeValue(forKey: id) } }
        return stream
    }

    public func syncNow() async {
        guard isReachable(target) else {
            publish { $0.state = .offline }
            return
        }
        for i in 1...10 {
            if Task.isCancelled { return }
            publish { $0.state = .syncing(fraction: Double(i) / 10) }
            try? await Task.sleep(for: step)
        }
        publish {
            $0.state = .idle
            $0.lastSuccess = Date()
        }
    }

    private func publish(_ change: (inout ArchiveStatus) -> Void) {
        let (value, conts) = lock.withLock { () -> (ArchiveStatus, [AsyncStream<ArchiveStatus>.Continuation]) in
            change(&current)
            return (current, Array(continuations.values))
        }
        for c in conts { c.yield(value) }
    }
}
