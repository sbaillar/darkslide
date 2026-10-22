import CoreGraphics
import Foundation

/// Byte-bounded in-memory thumbnail cache in front of an `ImageProvider`.
///
/// Loads run inside the caller's task, so when a grid cell scrolls away and
/// SwiftUI cancels its `.task`, the provider sees the cancellation too. Sizes
/// are bucketed so the grid and filmstrip share entries.
///
/// **Edited photos** (live mode): when `tools` and `editedSource` are set, a photo
/// with a non-identity edit is shown developed: rendered by the pipeline at background
/// priority and kept in its disk cache (`DevelopTools.editedThumbnail`). The entry is
/// keyed by the edit's fingerprint, so any edit (a slider, a paste, an undo) shows the
/// new render; unedited photos keep the fast embedded preview.
@MainActor
public final class ThumbnailCache {
    private final class Entry {
        let image: CGImage
        init(_ image: CGImage) { self.image = image }
    }

    private let provider: any ImageProvider
    private let cache = NSCache<NSString, Entry>()
    public private(set) var loads = 0
    public private(set) var hits = 0
    /// Developed thumbnails of edited photos rendered (or read from the disk cache).
    public private(set) var developedLoads = 0

    private static let editHashLimit = 2048
    private var editHashes: [PhotoID: (edit: EditState, hash: Int)] = [:]
    /// Edit encodings done for fingerprints (tests assert an unchanged edit isn't re-encoded).
    private(set) var encodes = 0

    /// Develop tools (live mode); `nil` = embedded previews only.
    public var tools: (any DevelopTools)?
    /// The stored edit of a photo, or `nil`/identity when unedited.
    public var editedSource: (@MainActor (PhotoID) -> EditState?)?
    /// Which engine fills removals (`RemovalModel.engineVersion`): part of the identity of
    /// an edit with removals, so its thumbnail renders again when LaMa takes over.
    public var removalEngine: @MainActor () -> Int = { RemovalModel.shared.engineVersion }
    /// How many depth maps have landed for a photo (`AppModel.depthVersions`): part of the
    /// identity of a depth lens blur, so a thumbnail rendered before its map renders again.
    public var depthVersion: @MainActor (PhotoID) -> Int = { _ in 0 }
    /// A developed thumbnail's load was cancelled (its cell left the screen): the photo's
    /// background work for it (a depth map) can stop.
    public var leftScreen: @MainActor (PhotoID) -> Void = { _ in }

    /// `costLimit` in bytes of decoded RGBA (default 192 MB ≈ 1,100 thumbnails at 256 px).
    public init(provider: any ImageProvider, costLimit: Int = 192 << 20) {
        self.provider = provider
        cache.totalCostLimit = costLimit
    }

    /// Smallest standard bucket that is at least `pixels` (256, 512, 1024).
    public nonisolated static func bucket(for pixels: CGFloat) -> Int {
        [256, 512, 1024].first { CGFloat($0) >= pixels } ?? 1024
    }

    private static func key(_ id: PhotoID, _ bucket: Int, _ edit: Int, _ version: Int = 0) -> NSString {
        (edit == 0 ? "\(id.raw)@\(bucket)v\(version)" : "\(id.raw)@\(bucket)v\(version)#\(edit)") as NSString
    }

    /// Which version of the file a thumbnail shows: a file replaced at the same path
    /// comes back from the store with new metadata (size, capture time, the changed flag),
    /// so its old thumbnail is never served for the rest of the session.
    nonisolated static func version(_ p: Photo) -> Int {
        var h = Hasher()
        h.combine(p.url?.path)
        h.combine(p.meta.fileSize)
        h.combine(p.meta.captureDate)
        h.combine(p.fileChanged)
        return h.finalize()
    }

    /// The edit to develop `photo` with, when it gets a developed thumbnail.
    func developedEdit(for photo: Photo) -> EditState? {
        guard tools != nil, !photo.isVideo, let e = editedSource?(photo.id), !e.isIdentity else { return nil }
        return e
    }

    /// Identity of what `photo`'s thumbnail shows: 0 for the embedded preview, else a
    /// fingerprint of its edit and, for an imported profile, whether it renders (it is
    /// Standard while missing or on another camera: L7). Reading it in a view body
    /// observes the photo's edit.
    public func fingerprint(_ photo: Photo) -> Int {
        guard let e = developedEdit(for: photo) else { return 0 }
        var h = Hasher()
        h.combine(editHash(photo.id, e))
        if case .dcp = e.profile { h.combine(CameraProfileLibrary.fits(e.profile, camera: photo.meta.camera)) }
        if !e.removals.isEmpty { h.combine(removalEngine()) }
        if e.effects.lensBlur.needsDepth { h.combine(depthVersion(photo.id)) }
        let f = h.finalize()
        return f == 0 ? 1 : f
    }

    /// Hash of the edit's encoding, remembered per photo while the edit is unchanged:
    /// a cell's body asks for the fingerprint on every evaluation, and encoding the whole
    /// edit each time is wasted work (a slider drag re-asks at frame rate).
    private func editHash(_ id: PhotoID, _ e: EditState) -> Int {
        if let m = editHashes[id], m.edit == e { return m.hash }
        encodes += 1
        let hash = ((try? AdjustmentsCodec.encode(e)) ?? "\(e)").hashValue
        if editHashes.count >= Self.editHashLimit { editHashes.removeAll(keepingCapacity: true) }
        editHashes[id] = (e, hash)
        return hash
    }

    public func cached(_ id: PhotoID, bucket: Int, edit: Int = 0, version: Int = 0) -> CGImage? {
        cache.object(forKey: Self.key(id, bucket, edit, version))?.image
    }

    /// The cached thumbnail of `photo` (its current edit and file version), if any.
    public func cached(_ photo: Photo, bucket: Int) -> CGImage? {
        cached(photo.id, bucket: bucket, edit: fingerprint(photo), version: Self.version(photo))
    }

    /// Cached image or a fresh load. Returns `nil` on cancellation or failure.
    public func image(for photo: Photo, bucket: Int) async -> CGImage? {
        var edit = fingerprint(photo)
        let version = Self.version(photo)
        if let hit = cached(photo.id, bucket: bucket, edit: edit, version: version) {
            hits += 1
            return hit
        }
        loads += 1
        let loaded: CGImage?
        if edit != 0, let tools, let e = developedEdit(for: photo) {
            developedLoads += 1
            // A failed develop (unsupported file, a transient GPU error) falls back to the
            // preview, cached as the *preview*: the edited key stays empty, so the next
            // request develops again instead of showing the unedited image for good.
            if let d = try? await tools.editedThumbnail(for: photo, edit: e) {
                loaded = d.image
            } else if Task.isCancelled {
                if e.effects.lensBlur.needsDepth { leftScreen(photo.id) }
                return nil
            } else {
                loaded = try? await provider.thumbnail(for: photo, maxPixelSize: bucket)
                edit = 0
            }
        } else {
            loaded = try? await provider.thumbnail(for: photo, maxPixelSize: bucket)
        }
        guard let img = loaded, !Task.isCancelled else { return nil }
        cache.setObject(Entry(img), forKey: Self.key(photo.id, bucket, edit, version), cost: img.bytesPerRow * img.height)
        return img
    }

    public func removeAll() { cache.removeAllObjects() }
}
