#if os(macOS)
import AppKit
@preconcurrency import Quartz
#else
import Foundation
#endif

/// Space on a video clip: Quick Look plays it (videos are never developed).
/// The panel itself sits behind `present`, so the routing is testable.
@MainActor
public final class VideoQuickLook: NSObject {
    public static let shared = VideoQuickLook()

    /// Currently previewed clip.
    public private(set) var url: URL?
    /// Shows (`URL`) or hides (`nil`) the panel. Default: the shared `QLPreviewPanel`.
    public var present: (URL?) -> Void

    override public init() {
        present = { _ in }
        super.init()
        #if os(macOS)
        present = { [weak self] url in self?.showPanel(url) }
        #endif
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
}

#if os(macOS)
extension VideoQuickLook: QLPreviewPanelDataSource {
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
#endif
