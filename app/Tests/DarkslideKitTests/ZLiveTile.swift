import CoreGraphics
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@MainActor @Suite struct ZLiveTile {
    /// Live mode: 100% zoom asks EmbeddedPreviewRenderer for a region tile; it returns the whole preview.
    @Test func liveTileIsWholePreviewSquashedIntoRegion() async {
        let m = makeViewerApp(renderer: EmbeddedPreviewRenderer(previews: ViewerFakePreviews()))
        await m.start()
        m.viewer.setViewerSize(CGSize(width: 1224, height: 824), backingScale: 2)
        _ = await waitUntil { m.viewer.frame != nil }
        m.viewer.cursor = CGPoint(x: 612, y: 412)
        m.perform(.zoomToggleAtCursor)
        let req = m.viewer.tileRequest()
        _ = await waitUntil { m.viewer.tile != nil }
        let t = m.viewer.tile
        print("ZLIVE req=\(String(describing: req)) tile frame=\(t.map { "\($0.frame.width)x\($0.frame.height) region=\(String(describing: $0.frame.region))" } ?? "nil") tile.region=\(String(describing: t?.region)) label=\(m.viewer.zoomLabel)")
        #expect(t == nil || t!.frame.region != nil, "tile holds a whole-image preview but is drawn into a sub-region")
    }
}
