import DarkslideFFI
import Foundation

/// Run a blocking file-system call on its own thread (not the Swift cooperative
/// pool, not the main thread) and give up waiting after `seconds`.
///
/// Network volumes (the NFS archive share) can hang `stat` for a long time.
/// On timeout the caller gets `nil` right away; the stuck call finishes on its
/// own thread, and its late result is dropped.
public func blockingWithTimeout<T: Sendable>(_ seconds: Double, _ work: @escaping @Sendable () -> T) async -> T? {
    await withCheckedContinuation { (cont: CheckedContinuation<T?, Never>) in
        let once = ResumeOnce(cont)
        // The call gets its own thread, and the timer a private (overcommit) queue: hung
        // calls parked on GCD's width-limited global queues would otherwise starve the
        // timeout itself, so a hung NAS stat could block for its full duration.
        let thread = Thread { once.resume(work()) }
        thread.qualityOfService = .userInitiated
        thread.start()
        blockingTimeoutQueue.asyncAfter(deadline: .now() + seconds) { once.resume(nil) }
    }
}

private let blockingTimeoutQueue = DispatchQueue(label: "darkslide.blocking-timeout", qos: .userInitiated)

private final class ResumeOnce<T: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var cont: CheckedContinuation<T?, Never>?
    init(_ cont: CheckedContinuation<T?, Never>) { self.cont = cont }
    func resume(_ value: T?) {
        let c = lock.withLock { () -> CheckedContinuation<T?, Never>? in
            defer { cont = nil }
            return cont
        }
        c?.resume(returning: value)
    }
}

/// Blocking probes of a slow or hung volume, rationed: one probe in flight per key
/// (callers asking for the same key wait on it), results cached for `ttl`, and no new
/// probe on a volume (`group`) while one there is stuck past its timeout. A hung network
/// share then parks one thread, not one per focus change or per recent folder.
/// `invalidate()` starts a new generation: a probe started before it never answers (or
/// fills the cache for) a caller after it.
public final class ProbeGate: @unchecked Sendable {
    public static let shared = ProbeGate()

    private final class Flight {
        let group: String
        let deadline: Date
        let generation: Int
        var waiters: [(any Sendable) -> Void] = []
        init(group: String, deadline: Date, generation: Int) {
            self.group = group
            self.deadline = deadline
            self.generation = generation
        }
    }

    private let lock = NSLock()
    /// Keyed by probe key and generation.
    private var flights: [String: Flight] = [:]
    private var cache: [String: (until: Date, value: any Sendable)] = [:]
    private var generation = 0
    private let now: @Sendable () -> Date

    public init(now: @escaping @Sendable () -> Date = { Date() }) { self.now = now }

    /// `work`'s result for `key`, or nil when it didn't finish within `timeout` or a probe
    /// on `group` is already stuck (nothing new is started then). A result that lands
    /// after its callers gave up is still cached for the next caller (unless the gate
    /// was invalidated since the probe started).
    public func run<T: Sendable>(key: String, group: String, timeout: Double, ttl: Double = 0,
                                 _ work: @escaping @Sendable () -> T) async -> T? {
        await withCheckedContinuation { (cont: CheckedContinuation<T?, Never>) in
            let once = ResumeOnce(cont)
            let start: (flight: String, generation: Int)?? = lock.withLock {
                let t = now()
                if let hit = cache[key], hit.until > t, let v = hit.value as? T {
                    once.resume(v)
                    return nil
                }
                if flights.values.contains(where: { $0.group == group && $0.deadline <= t }) {
                    once.resume(nil) // stuck: don't park another thread (or waiter) on that volume
                    return nil
                }
                let flight = "\(generation):\(key)"
                if let f = flights[flight] {
                    f.waiters.append { once.resume($0 as? T) }
                    return .some(nil)
                }
                let f = Flight(group: group, deadline: t.addingTimeInterval(timeout), generation: generation)
                f.waiters.append { once.resume($0 as? T) }
                flights[flight] = f
                return (flight, generation)
            }
            guard let start else { return }
            if let (flight, gen) = start.map({ ($0.flight, $0.generation) }) {
                let thread = Thread { [self] in
                    let value = work()
                    let waiters = lock.withLock {
                        let w = flights.removeValue(forKey: flight)?.waiters ?? []
                        if ttl > 0, gen == generation { cache[key] = (now().addingTimeInterval(ttl), value) }
                        return w
                    }
                    for w in waiters { w(value) }
                }
                thread.qualityOfService = .userInitiated
                thread.start()
            }
            blockingTimeoutQueue.asyncAfter(deadline: .now() + timeout) { once.resume(nil) }
        }
    }

    /// Forget cached results, and stop probes already in flight from answering later
    /// callers (a volume mounted or unmounted, or a Remove about to act: answers may
    /// have changed). A probe stuck on a volume still keeps new ones off it.
    public func invalidate() {
        lock.withLock {
            cache.removeAll()
            generation += 1
        }
    }

    /// Probes in flight (tests).
    var inFlight: Int { lock.withLock { flights.count } }
}

/// Where a session folder's volume is mounted and which volume it is, recorded while it
/// was there: a folder outside `/Volumes` (`/net`, `~/mnt`, a symlink to a share) gets a
/// real mount check, and another volume mounted under the same name doesn't pass for it.
public struct VolumeRecord: Codable, Equatable, Sendable {
    /// The mount point (`statfs` `f_mntonname`); nil: the startup disk.
    public var mount: String?
    /// The volume UUID (`uuid:…`), or what's mounted (`src:smbfs://user@host/share`) for
    /// a volume without one; nil: not known (any volume mounted there passes).
    public var identity: String?

    public init(mount: String?, identity: String?) {
        self.mount = mount
        self.identity = identity
    }
}

/// What a probe of a volume root found. `sameVolume`: it's the volume recorded for it (or
/// none is recorded); a mismatch only keeps Remove away, everything else works.
public struct VolumeCheck: Equatable, Sendable {
    public var mounted: Bool
    public var sameVolume: Bool

    public init(mounted: Bool, sameVolume: Bool) {
        self.mounted = mounted
        self.sameVolume = sameVolume
    }
}

/// Which volume a path lives on, and whether it's mounted. Only volumes other than the
/// startup disk can go away (cards, external drives, network shares).
public enum VolumeMount {
    /// Folders whose volume was located (`register`): folder path → mount point (nil:
    /// the startup disk). Longest prefix wins over the `/Volumes/<name>` rule.
    private static let known = Known()
    private final class Known: @unchecked Sendable {
        let lock = NSLock()
        var mounts: [String: String?] = [:]
    }

    /// `folder` (and everything under it) is on the volume `record` says.
    public static func register(_ folder: URL, _ record: VolumeRecord) {
        known.lock.withLock { known.mounts[folder.standardizedFileURL.path] = .some(record.mount) }
    }

    /// The mount point of `url`'s volume (nil: the startup disk): per a registered
    /// folder, else `/Volumes/<name>`.
    public static func root(of url: URL) -> URL? {
        let path = url.standardizedFileURL.path
        let hit = known.lock.withLock {
            known.mounts.filter { path == $0.key || path.hasPrefix($0.key.hasSuffix("/") ? $0.key : $0.key + "/") }
                .max { $0.key.count < $1.key.count }
        }
        if let hit { return hit.value.map { URL(fileURLWithPath: $0, isDirectory: true) } }
        let c = url.standardizedFileURL.pathComponents
        guard c.count >= 3, c[0] == "/", c[1] == "Volumes" else { return nil }
        return URL(fileURLWithPath: "/Volumes", isDirectory: true).appendingPathComponent(c[2], isDirectory: true)
    }

    /// The `ProbeGate` group for probes of `url`: its volume.
    public static func group(of url: URL) -> String { root(of: url)?.path ?? "/" }

    /// Blocking (may stat a network share): `root` is a mounted volume, not a leftover
    /// empty mount-point folder.
    public static func isMounted(_ root: URL) -> Bool {
        (try? root.resolvingSymlinksInPath().resourceValues(forKeys: [.isVolumeKey]).isVolume) == true
    }

    /// Blocking: whether `root` is mounted and, when `identity` is known, whether it's that
    /// volume (not another one mounted under the same name, a clone, a restore).
    public static func check(_ root: URL, identity: String?) -> VolumeCheck {
        guard isMounted(root) else { return VolumeCheck(mounted: false, sameVolume: false) }
        guard let identity else { return VolumeCheck(mounted: true, sameVolume: true) }
        return VolumeCheck(mounted: true, sameVolume: locate(root)?.identity == identity)
    }

    /// Blocking: the volume `folder` is on, when `folder` is there (nil: it isn't, or the
    /// volume can't be told: nothing is recorded then).
    public static func locate(_ folder: URL) -> VolumeRecord? {
        let real = folder.resolvingSymlinksInPath()
        guard FileManager.default.fileExists(atPath: real.path),
              let token = archiveVolumeIdentity(path: real.path),
              let id = try? JSONDecoder().decode(StatfsID.self, from: Data(token.utf8)) else { return nil }
        if id.mountedOn == "/" || id.mountedOn == "/System/Volumes/Data" {
            // A `/Volumes` path on the startup disk is a leftover mount-point folder (the
            // share is gone), not where its photos live: nothing is recorded.
            return VolumeMount.root(of: folder) == nil ? VolumeRecord(mount: nil, identity: nil) : nil
        }
        let mount = URL(fileURLWithPath: id.mountedOn, isDirectory: true)
        let uuid = try? mount.resourceValues(forKeys: [.volumeUUIDStringKey]).volumeUUIDString
        return VolumeRecord(mount: id.mountedOn, identity: uuid.map { "uuid:\($0)" } ?? "src:\(id.fsType):\(id.source)")
    }

    /// ds-archive `VolumeId` (what `archiveVolumeIdentity` returns).
    private struct StatfsID: Decodable {
        var mountedOn: String
        var source: String
        var fsType: String
        enum CodingKeys: String, CodingKey {
            case mountedOn = "mounted_on", source, fsType = "fs_type"
        }
    }

    /// `check`, rationed by `ProbeGate.shared` (nil: no answer in time).
    public static func probe(_ root: URL, identity: String? = nil, timeout: Double = 1) async -> VolumeCheck? {
        await ProbeGate.shared.run(key: "mounted:\(root.path)|\(identity ?? "")", group: root.path, timeout: timeout, ttl: 5) {
            check(root, identity: identity)
        }
    }

    /// Forget what's registered for `folder` (it's located again).
    public static func unregister(_ folder: URL) {
        known.lock.withLock { _ = known.mounts.removeValue(forKey: folder.standardizedFileURL.path) }
    }

    /// `locate`, rationed by `ProbeGate.shared` (nil: not there, or no answer in time).
    public static func probeLocation(_ folder: URL, timeout: Double = 1) async -> VolumeRecord? {
        await ProbeGate.shared.run(key: "locate:\(folder.path)", group: group(of: folder), timeout: timeout) {
            locate(folder)
        } ?? nil
    }
}

/// What a check of an original found: whether the file is there, and whether its volume
/// is mounted (nil: on the startup disk).
public struct OriginalProbe: Equatable, Sendable {
    public var exists: Bool
    public var volumeMounted: Bool?

    public init(exists: Bool, volumeMounted: Bool?) {
        self.exists = exists
        self.volumeMounted = volumeMounted
    }

    /// Blocking: check `url` and its volume.
    public static func check(_ url: URL) -> OriginalProbe {
        let exists = FileManager.default.fileExists(atPath: url.path)
        return OriginalProbe(exists: exists, volumeMounted: VolumeMount.root(of: url).map { exists || VolumeMount.isMounted($0) })
    }

    /// `check`, off the caller's thread, one per original at a time, never piling up on a
    /// hung volume (nil: no answer within a second).
    public static let live: @Sendable (URL) async -> OriginalProbe? = { url in
        await ProbeGate.shared.run(key: "original:\(url.path)", group: VolumeMount.group(of: url), timeout: 1) {
            check(url)
        }
    }
}

/// What a volume is, from its resource values. The NAS share reports `false`
/// for all three flags; an SD card in a reader reports local + ejectable/removable.
public struct VolumeTraits: Equatable, Sendable {
    public var isLocal: Bool
    public var isEjectable: Bool
    public var isRemovable: Bool
    /// Stable identity (survives remounts), used to remember offered cards.
    public var uuid: String?
    /// A mounted disk image (hdiutil, DiskImages2: DiskArbitration reports the device
    /// protocol "Virtual Interface" or the model "Disk Image"), not a card in a reader.
    public var isDiskImage: Bool

    public init(isLocal: Bool, isEjectable: Bool, isRemovable: Bool, uuid: String? = nil, isDiskImage: Bool = false) {
        self.isLocal = isLocal
        self.isEjectable = isEjectable
        self.isRemovable = isRemovable
        self.uuid = uuid
        self.isDiskImage = isDiskImage
    }

    /// "Eject when done" makes sense only for local removable media.
    public var canEject: Bool { isLocal && (isEjectable || isRemovable) }
}

/// Reads `VolumeTraits` off the main thread (a seam for tests).
public protocol VolumeInspecting: Sendable {
    /// `nil` when the volume can't be read in time (slow or hung network mount).
    func traits(of url: URL) async -> VolumeTraits?
}

public struct FileSystemVolumeInspector: VolumeInspecting {
    public var timeout: Double

    public init(timeout: Double = 1) { self.timeout = timeout }

    public func traits(of url: URL) async -> VolumeTraits? {
        await blockingWithTimeout(timeout) {
            let keys: Set<URLResourceKey> = [.volumeIsLocalKey, .volumeIsEjectableKey, .volumeIsRemovableKey,
                                             .volumeUUIDStringKey]
            guard let v = try? url.resourceValues(forKeys: keys) else { return nil }
            return VolumeTraits(isLocal: v.volumeIsLocal ?? false, isEjectable: v.volumeIsEjectable ?? false,
                                isRemovable: v.volumeIsRemovable ?? false, uuid: v.volumeUUIDString,
                                isDiskImage: DiskDescription.isDiskImage(volume: url))
        } ?? nil
    }
}

/// Fixed answers, for tests and previews.
public struct StaticVolumeInspector: VolumeInspecting {
    public var byPath: [String: VolumeTraits]
    public init(_ byPath: [String: VolumeTraits]) { self.byPath = byPath }
    public func traits(of url: URL) async -> VolumeTraits? { byPath[url.standardizedFileURL.path] }
}
