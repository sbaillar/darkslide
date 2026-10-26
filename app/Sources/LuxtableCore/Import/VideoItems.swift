import AVFoundation
import CoreGraphics
import Foundation
import ImageIO

/// Poster frames for video clips (grid, viewer and import thumbnails), from
/// AVFoundation's hardware decoder.
public enum VideoPoster {
    /// A frame near the start (1 s in, or the first frame of a shorter clip), at most
    /// `maxPixelSize` on the long edge (0 = full size).
    public static func image(for url: URL, maxPixelSize: Int) async throws -> CGImage {
        let asset = AVURLAsset(url: url)
        let gen = AVAssetImageGenerator(asset: asset)
        gen.appliesPreferredTrackTransform = true
        if maxPixelSize > 0 { gen.maximumSize = CGSize(width: maxPixelSize, height: maxPixelSize) }
        let duration = (try? await asset.load(.duration)) ?? .zero
        let at = duration.seconds > 2 ? CMTime(seconds: 1, preferredTimescale: 600) : .zero
        return try await gen.image(at: at).image
    }
}

/// Thumbnails for card items that aren't RAW: JPEG/HEIF through ImageIO,
/// clips through `VideoPoster`. `nil` for RAWs (they use embedded previews).
public enum NonRawThumbnail {
    public static let imageExtensions: Set<String> = ["jpg", "jpeg", "heic", "heif", "hif"]

    public static func image(for url: URL, maxPixelSize: Int) async -> CGImage? {
        if Photo.isVideo(url) { return try? await VideoPoster.image(for: url, maxPixelSize: maxPixelSize) }
        guard imageExtensions.contains(url.pathExtension.lowercased()),
              let src = CGImageSourceCreateWithURL(url as CFURL, [kCGImageSourceShouldCache: false] as CFDictionary)
        else { return nil }
        let opts: [CFString: Any] = [kCGImageSourceCreateThumbnailFromImageAlways: true,
                                     kCGImageSourceCreateThumbnailWithTransform: true,
                                     kCGImageSourceThumbnailMaxPixelSize: max(1, maxPixelSize)]
        return CGImageSourceCreateThumbnailAtIndex(src, 0, opts as CFDictionary)
    }
}
