import CoreGraphics
import Foundation
import Observation

/// Per-cell thumbnail state. Owns the load decision that used to live in the
/// cell's view body:
/// - a cache hit is applied synchronously (no placeholder flash while scrolling back);
/// - a load runs in the caller's task, so a cell leaving the lazy container cancels it;
/// - a bucket change keeps the old image as a placeholder until the sharper one lands;
/// - asking again for the same photo+bucket after success does nothing (badge-only
///   changes such as a rating never reload the image).
@MainActor @Observable
public final class ThumbnailLoader {
    public struct Key: Hashable, Sendable {
        public let id: PhotoID
        public let bucket: Int
        /// `ThumbnailCache.fingerprint` of the photo's edit (0 = embedded preview).
        public let edit: Int
        public init(id: PhotoID, bucket: Int, edit: Int = 0) {
            self.id = id
            self.bucket = bucket
            self.edit = edit
        }
    }

    public private(set) var image: CGImage?
    /// The last load finished without an image (and wasn't cancelled).
    public private(set) var failed = false
    /// Key of `image` (or of the in-flight load).
    public private(set) var key: Key?
    @ObservationIgnored public private(set) var loadsStarted = 0
    @ObservationIgnored private let cache: ThumbnailCache

    public init(cache: ThumbnailCache) {
        self.cache = cache
    }

    /// The key of what `photo` shows at `bucket` now (its edit included). A cell uses it
    /// as its `.task` id, so an edit to the photo reloads its thumbnail.
    public func key(for photo: Photo, bucket: Int) -> Key {
        Key(id: photo.id, bucket: bucket, edit: cache.fingerprint(photo))
    }

    /// Bucket for a cell `points` wide at `scale`.
    public nonisolated static func bucket(points: CGFloat, scale: CGFloat) -> Int {
        ThumbnailCache.bucket(for: points * max(scale, 1))
    }

    /// Synchronous path: apply a cached image if there is one. Returns true on a hit.
    @discardableResult
    public func prime(_ photo: Photo, bucket: Int) -> Bool {
        let k = key(for: photo, bucket: bucket)
        if key == k, image != nil { return true }
        guard let hit = cache.cached(photo.id, bucket: bucket, edit: k.edit, version: ThumbnailCache.version(photo)) else {
            return false
        }
        image = hit
        key = k
        failed = false
        return true
    }

    /// Load `photo` at `bucket` unless already showing it. Cancellation-aware:
    /// if the calling task is cancelled the state is left as it was.
    public func load(_ photo: Photo, bucket: Int) async {
        let k = key(for: photo, bucket: bucket)
        if prime(photo, bucket: bucket) { return }
        if key?.id != photo.id { image = nil } // different photo: never show a stale one
        key = k
        failed = false
        loadsStarted += 1
        let img = await cache.image(for: photo, bucket: bucket)
        guard !Task.isCancelled, key == k else { return }
        if let img {
            image = img
        } else {
            failed = true
        }
    }
}
