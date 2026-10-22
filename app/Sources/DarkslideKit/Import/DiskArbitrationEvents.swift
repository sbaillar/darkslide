import DiskArbitration
import Foundation

/// A DiskArbitration description dictionary reduced to what card detection needs.
public struct DiskDescription: Equatable, Sendable {
    public var volumePath: URL?
    public var isNetwork: Bool

    public init(volumePath: URL?, isNetwork: Bool) {
        self.volumePath = volumePath
        self.isNetwork = isNetwork
    }

    /// Parse `DADiskCopyDescription` output (pure, so it is unit-tested with plain dictionaries).
    public init(_ dict: [String: Any]) {
        volumePath = dict[kDADiskDescriptionVolumePathKey as String] as? URL
        isNetwork = (dict[kDADiskDescriptionVolumeNetworkKey as String] as? Bool) ?? false
    }

    /// Whether a DiskArbitration description is a mounted disk image rather than a
    /// physical disk: device protocol "Virtual Interface" or model "Disk Image".
    public static func isDiskImage(_ dict: [String: Any]) -> Bool {
        let proto = dict[kDADiskDescriptionDeviceProtocolKey as String] as? String
        let model = (dict[kDADiskDescriptionDeviceModelKey as String] as? String)?
            .trimmingCharacters(in: .whitespaces)
        return proto == "Virtual Interface" || model == "Disk Image"
    }

    /// Whether the volume at `url` is on a mounted disk image (false when DiskArbitration
    /// can't tell).
    public static func isDiskImage(volume url: URL) -> Bool {
        guard let session = DASessionCreate(kCFAllocatorDefault),
              let disk = DADiskCreateFromVolumePath(kCFAllocatorDefault, session, url as CFURL),
              let dict = DADiskCopyDescription(disk) as? [String: Any] else { return false }
        return isDiskImage(dict)
    }

    /// The volume to report, or `nil` for unmounted disks and network shares.
    public var reportableVolume: URL? {
        guard !isNetwork, let volumePath, volumePath.path != "/" else { return nil }
        return volumePath
    }
}

/// DiskArbitration volume events. Unlike `NSWorkspace` notifications this sees
/// `nobrowse` mounts, which some card readers and camera tethers use.
///
/// "Mounted" = a disk appeared with a volume path, or an existing disk's
/// volume path changed to non-nil; "unmounted" = the path went away or the
/// disk disappeared. Network volumes are never reported. Callbacks arrive on
/// the main queue.
@MainActor
public final class DiskArbitrationVolumeEvents: VolumeEventSource {
    private let session: DASession
    private var box: UnsafeMutableRawPointer?
    /// Last known volume path per disk (BSD name), to report unmounts.
    private var paths: [String: URL] = [:]

    final class Handlers: @unchecked Sendable { // touched only on the main queue
        weak var owner: DiskArbitrationVolumeEvents?
        let onMount: @MainActor (URL) -> Void
        let onUnmount: @MainActor (URL) -> Void
        init(onMount: @escaping @MainActor (URL) -> Void, onUnmount: @escaping @MainActor (URL) -> Void) {
            self.onMount = onMount
            self.onUnmount = onUnmount
        }
    }

    /// `nil` when DiskArbitration is unavailable (caller falls back to NSWorkspace).
    public init?() {
        guard let s = DASessionCreate(kCFAllocatorDefault) else { return nil }
        session = s
    }

    public func start(onMount: @escaping @MainActor (URL) -> Void, onUnmount: @escaping @MainActor (URL) -> Void) {
        stop()
        let handlers = Handlers(onMount: onMount, onUnmount: onUnmount)
        handlers.owner = self
        // The context is retained on purpose and never released: DiskArbitration may
        // still deliver an already-queued callback after unregistering, and a
        // released context would dangle. One small object per start() is the cost.
        let ctx = Unmanaged.passRetained(handlers).toOpaque()
        box = ctx
        DASessionSetDispatchQueue(session, DispatchQueue.main)
        DARegisterDiskAppearedCallback(session, nil, Self.appeared, ctx)
        DARegisterDiskDescriptionChangedCallback(session, nil, [kDADiskDescriptionVolumePathKey] as CFArray,
                                                 Self.changed, ctx)
        DARegisterDiskDisappearedCallback(session, nil, Self.disappeared, ctx)
    }

    public func stop() {
        guard let ctx = box else { return }
        Unmanaged<Handlers>.fromOpaque(ctx).takeUnretainedValue().owner = nil
        DAUnregisterCallback(session, unsafeBitCast(Self.appeared, to: UnsafeMutableRawPointer.self), ctx)
        DAUnregisterCallback(session, unsafeBitCast(Self.changed, to: UnsafeMutableRawPointer.self), ctx)
        DAUnregisterCallback(session, unsafeBitCast(Self.disappeared, to: UnsafeMutableRawPointer.self), ctx)
        DASessionSetDispatchQueue(session, nil)
        box = nil
        paths = [:]
    }

    private static let appeared: DADiskAppearedCallback = { disk, ctx in dispatch(disk, ctx, disappeared: false) }
    private static let changed: DADiskDescriptionChangedCallback = { disk, _, ctx in dispatch(disk, ctx, disappeared: false) }
    private static let disappeared: DADiskDisappearedCallback = { disk, ctx in dispatch(disk, ctx, disappeared: true) }

    public func mountedVolumes() async -> [URL] {
        await VolumeList.mounted(includeHidden: true)
    }

    nonisolated private static func dispatch(_ disk: DADisk, _ ctx: UnsafeMutableRawPointer?, disappeared: Bool) {
        guard let ctx else { return }
        let desc = DiskDescription((DADiskCopyDescription(disk) as? [String: Any]) ?? [:])
        let bsd = DADiskGetBSDName(disk).map { String(cString: $0) } ?? "?"
        let handlers = Unmanaged<Handlers>.fromOpaque(ctx).takeUnretainedValue()
        MainActor.assumeIsolated {
            handlers.owner?.handle(bsd: bsd, description: desc, disappeared: disappeared, handlers: handlers)
        }
    }

    private func handle(bsd: String, description: DiskDescription, disappeared: Bool, handlers: Handlers) {
        let event = Self.transition(previous: paths[bsd], current: disappeared ? nil : description.reportableVolume)
        paths[bsd] = disappeared ? nil : description.reportableVolume
        switch event {
        case .mounted(let url): handlers.onMount(url)
        case .unmounted(let url): handlers.onUnmount(url)
        case .none: break
        }
    }

    public enum Transition: Equatable, Sendable {
        case mounted(URL), unmounted(URL)
    }

    /// Pure state machine: what a path change for one disk means.
    public nonisolated static func transition(previous: URL?, current: URL?) -> Transition? {
        switch (previous, current) {
        case (nil, let c?): .mounted(c)
        case (let p?, nil): .unmounted(p)
        case (let p?, let c?) where p != c: .mounted(c)
        default: nil
        }
    }
}
