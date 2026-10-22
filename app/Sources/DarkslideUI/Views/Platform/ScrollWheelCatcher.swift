#if os(macOS)
import AppKit
#endif
import SwiftUI

#if os(macOS)
/// Receives scroll-wheel events over its area without taking clicks or drags:
/// it only hit-tests while the current event is a scroll.
struct ScrollWheelCatcher: NSViewRepresentable {
    var enabled = true
    let onScroll: (_ deltaY: Double, _ precise: Bool) -> Void

    func makeNSView(context: Context) -> CatcherView {
        let v = CatcherView()
        v.onScroll = onScroll
        v.enabled = enabled
        return v
    }

    func updateNSView(_ v: CatcherView, context: Context) {
        v.onScroll = onScroll
        v.enabled = enabled
    }

    final class CatcherView: NSView {
        var onScroll: ((Double, Bool) -> Void)?
        var enabled = true

        override func hitTest(_ point: NSPoint) -> NSView? {
            enabled && NSApp.currentEvent?.type == .scrollWheel ? super.hitTest(point) : nil
        }

        override func scrollWheel(with event: NSEvent) {
            // Mostly-horizontal gestures (and panel scrolling with no vertical intent) pass through.
            let dy = event.scrollingDeltaY != 0 ? event.scrollingDeltaY : -event.scrollingDeltaX
            guard dy != 0, abs(event.scrollingDeltaY) >= abs(event.scrollingDeltaX) * 0.5 || event.scrollingDeltaY == 0 else {
                super.scrollWheel(with: event)
                return
            }
            onScroll?(Double(dy), event.hasPreciseScrollingDeltas)
        }
    }
}
#else
/// iOS (unverified): trackpad scrolling reaches SwiftUI directly, so the catcher is a no-op.
struct ScrollWheelCatcher: View {
    var enabled = true
    let onScroll: (_ deltaY: Double, _ precise: Bool) -> Void

    var body: some View { Color.clear }
}
#endif
