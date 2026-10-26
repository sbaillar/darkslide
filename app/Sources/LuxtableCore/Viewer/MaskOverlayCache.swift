import CoreGraphics
import LuxtableAI
import Foundation

/// Vision mask bitmaps for drawing AI mask shapes on the canvas (L9).
///
/// **Persistence decision: regenerate, don't store.** An AI shape is stored in the
/// sidecar as `.ai(op, target)` only. Its bitmap is a deterministic function of the
/// photo's embedded preview and the target (the same Vision request on the same
/// pixels), so after a relaunch or reopen the overlay is recomputed on demand by
/// the caller's generator (see `request`) instead of persisting PNGs next to sidecars. lt-pipeline likewise
/// computes masks itself (via the `MaskSource` seam) at render resolution.
///
/// Keys are `(photo, target)`: every `.ai(subject)` shape of a photo shares one
/// bitmap regardless of layer or position, so removing or reordering shapes can
/// never make one shape show another's bitmap. The byte cap counts the 1-byte
/// masks; the tint is applied on the GPU in the viewer shader (no RGBA copy),
/// and the r8 textures are held by `MaskTextureCache` under its own cap.
@MainActor
public final class MaskOverlayCache {
    public static let shared = MaskOverlayCache()

    public struct Key: Hashable, Sendable {
        public let photo: PhotoID
        public let target: AITarget

        public init(photo: PhotoID, target: AITarget) {
            self.photo = photo
            self.target = target
        }
    }

    struct Entry {
        let mask: VisionMask
        var bytes: Int { mask.bytes.count }
    }

    /// Computes a photo's mask for a target (Vision on its preview); nil if unavailable.
    public typealias Generator = @Sendable (PhotoID, AITarget) async -> VisionMask?

    public let byteLimit: Int
    private var entries: [Key: Entry] = [:]
    private var order: [Key] = []
    private var inFlight: Set<Key> = []
    private(set) var bytes = 0
    public private(set) var generated = 0

    public init(byteLimit: Int = 64 << 20) {
        self.byteLimit = byteLimit
    }

    public var count: Int { entries.count }

    public func set(_ mask: VisionMask, photo: PhotoID, target: AITarget) {
        let k = Key(photo: photo, target: target)
        remove(k)
        entries[k] = Entry(mask: mask)
        order.append(k)
        bytes += mask.bytes.count
        trim(keeping: k)
    }

    public func mask(photo: PhotoID, target: AITarget) -> VisionMask? {
        entries[Key(photo: photo, target: target)]?.mask
    }

    /// Ensure the mask for `(photo, target)` exists: regenerate it with `generator` (once,
    /// even if asked repeatedly) and call `onReady` when it lands. Returns true if cached.
    /// The generator is per call, not stored, so the shared cache never holds on to one
    /// viewer's closure.
    @discardableResult
    public func request(photo: PhotoID, target: AITarget, generator: @escaping Generator,
                        onReady: @escaping @MainActor () -> Void) -> Bool {
        let k = Key(photo: photo, target: target)
        if entries[k] != nil { return true }
        guard !inFlight.contains(k) else { return false }
        inFlight.insert(k)
        Task { @MainActor [weak self] in
            let mask = await generator(photo, target)
            guard let self else { return }
            self.inFlight.remove(k)
            if let mask {
                self.generated += 1
                self.set(mask, photo: photo, target: target)
                onReady()
            }
        }
        return false
    }

    /// Drop bitmaps of `photo` for targets no AI shape of the edit uses any more.
    public func prune(photo: PhotoID, edit: EditState) {
        let used = Self.targets(in: edit)
        for k in entries.keys where k.photo == photo && !used.contains(k.target) { remove(k) }
    }

    /// Keep only bitmaps of `photos` (e.g. the session's visible photos); the rest were closed.
    public func retain(photos: Set<PhotoID>) {
        for k in entries.keys where !photos.contains(k.photo) { remove(k) }
    }

    /// Photo closed: free all its bitmaps.
    public func evict(photo: PhotoID) {
        for k in entries.keys where k.photo == photo { remove(k) }
    }

    public func removeAll() {
        entries.removeAll()
        order.removeAll()
        bytes = 0
    }

    /// AI targets used by any layer of `edit`.
    public static func targets(in edit: EditState) -> Set<AITarget> {
        var out: Set<AITarget> = []
        for l in edit.layers {
            for s in l.mask.shapes { if case .ai(_, let t) = s { out.insert(t) } }
        }
        return out
    }

    /// AI targets of one mask, in first-use order.
    public static func targets(in mask: MaskEdit) -> [AITarget] {
        var out: [AITarget] = []
        for s in mask.shapes {
            if case .ai(_, let t) = s, !out.contains(t) { out.append(t) }
        }
        return out
    }

    private func touch(_ k: Key) {
        order.removeAll { $0 == k }
        order.append(k)
    }

    private func trim(keeping k: Key) {
        while bytes > byteLimit, let victim = order.first(where: { $0 != k }) { remove(victim) }
    }

    private func remove(_ k: Key) {
        guard let e = entries.removeValue(forKey: k) else { return }
        bytes -= e.bytes
        order.removeAll { $0 == k }
    }
}
