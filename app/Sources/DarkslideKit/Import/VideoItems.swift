import AppKit
import AVFoundation
import CoreGraphics
import Foundation
import ImageIO
@preconcurrency import Quartz

/// Space on a video clip: Quick Look plays it (videos are never developed).
/// The panel itself sits behind `present`, so the routing is testable.
@MainActor
public final class VideoQuickLook: NSObject, QLPreviewPanelDataSource {
    public static let shared = VideoQuickLook()

    /// Currently previewed clip.
    public private(set) var url: URL?
    /// Shows (`URL`) or hides (`nil`) the panel. Default: the shared `QLPreviewPanel`.
    public var present: (URL?) -> Void

    override public init() {
        present = { _ in }
        super.init()
        present = { [weak self] url in self?.showPanel(url) }
    }

    /// Space toggles: the same clip again closes the preview.
    public func toggle(_ clip: URL) {
        if url == clip {
            url = nil
            present(nil)
        } else {
            url = clip
            present(clip)
        }
    }

    private func showPanel(_ clip: URL?) {
        guard let panel = QLPreviewPanel.shared() else { return }
        guard clip != nil else {
            panel.orderOut(nil)
            return
        }
        panel.dataSource = self
        panel.reloadData()
        panel.makeKeyAndOrderFront(nil)
    }

    public nonisolated func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int {
        MainActor.assumeIsolated { url == nil ? 0 : 1 }
    }

    public nonisolated func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> (any QLPreviewItem)! {
        let path = MainActor.assumeIsolated { url?.path }
        return path.map { NSURL(fileURLWithPath: $0) }
    }
}

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
