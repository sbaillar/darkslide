import CoreGraphics
import Foundation

/// Embedded-preview thumbnails for the import grid, read straight off the card
/// through `PreviewProvider` (ds-raw). Cached by card and path (C1: another
/// card at the same mount point never shows the first card's thumbnails);
/// loads run in the caller's task, so a tile scrolling away cancels its read.
@MainActor
public final class ImportThumbnails {
    private final class Entry {
        let image: CGImage
        init(_ image: CGImage) { self.image = image }
    }

    private let previews: (any PreviewProvider)?
    private let cache = NSCache<NSString, Entry>()
    public let maxPixelSize: Int
    public private(set) var loads = 0

    public init(previews: (any PreviewProvider)?, maxPixelSize: Int = 256, costLimit: Int = 64 << 20) {
        self.previews = previews
        self.maxPixelSize = maxPixelSize
        cache.totalCostLimit = costLimit
    }

    /// Whether real thumbnails are available at all (else tiles show a placeholder).
    public var isAvailable: Bool { previews != nil }

    static func key(_ url: URL, card: String?) -> NSString { "\(card ?? "")|\(url.standardizedFileURL.path)" as NSString }

    /// The cached thumbnail of `url` on the card `card` (its volume id).
    public func cached(_ url: URL, card: String? = nil) -> CGImage? { cache.object(forKey: Self.key(url, card: card))?.image }

    /// Cached or freshly loaded thumbnail; `nil` when unavailable, failed, or cancelled.
    public func thumbnail(for url: URL, card: String? = nil) async -> CGImage? {
        if let hit = cached(url, card: card) { return hit }
        guard let previews else { return nil }
        loads += 1
        // Clips and JPEG/HEIF-only items have no embedded RAW preview.
        var img = await NonRawThumbnail.image(for: url, maxPixelSize: maxPixelSize)
        if img == nil { img = try? await previews.preview(for: url, maxPixelSize: maxPixelSize) }
        guard let img, !Task.isCancelled else { return nil }
        cache.setObject(Entry(img), forKey: Self.key(url, card: card), cost: img.bytesPerRow * img.height)
        return img
    }
}
