#if os(macOS)
import AppKit
#else
import UIKit
#endif
import MetalKit
import SwiftUI

#if os(macOS)
extension MetalImageView: NSViewRepresentable {
    func makeNSView(context: Context) -> ViewerMTKView { makeView(context.coordinator) }
    func updateNSView(_ view: ViewerMTKView, context: Context) { update(view) }
    static func dismantleNSView(_ view: ViewerMTKView, coordinator: Coordinator) { dismantle(coordinator) }
}

/// MTKView that turns scroll-wheel and pinch into zoom/pan.
final class ViewerMTKView: MTKView {
    var onZoom: ((Double, CGPoint) -> Void)?
    var onScrollPan: ((CGSize) -> Void)?
    /// Scroll while the eyedropper loupe shows: its magnification (true: consumed).
    var onLoupeScroll: ((Double) -> Bool)?

    override var isFlipped: Bool { true }

    private func local(_ e: NSEvent) -> CGPoint { convert(e.locationInWindow, from: nil) }

    override func scrollWheel(with e: NSEvent) {
        let steps = e.hasPreciseScrollingDeltas ? e.scrollingDeltaY / 10 : e.scrollingDeltaY
        if let loupe = onLoupeScroll, loupe(Double(steps)) { return }
        // Trackpad two-finger scroll pans; a mouse wheel (or ⌘/⌥ + scroll) zooms.
        if e.hasPreciseScrollingDeltas, !e.modifierFlags.contains(.command), !e.modifierFlags.contains(.option) {
            onScrollPan?(CGSize(width: e.scrollingDeltaX, height: e.scrollingDeltaY))
        } else {
            let dy = e.hasPreciseScrollingDeltas ? e.scrollingDeltaY / 100 : e.scrollingDeltaY / 10
            onZoom?(pow(2, Double(dy)), local(e))
        }
    }

    override func magnify(with e: NSEvent) {
        onZoom?(1 + Double(e.magnification), local(e))
    }
}

extension MTKView {
    /// Draw once more (the view draws on demand only).
    func requestRedraw() { needsDisplay = true }
    func setLayerOpaque() { layer?.isOpaque = true }
}
#else
// iOS (unverified): the same coordinator and renderer in a UIView. Pinch, pan and
// double-tap recognisers feeding `onZoom`/`onScrollPan` come with the touch viewer (phase 2).
extension MetalImageView: UIViewRepresentable {
    func makeUIView(context: Context) -> ViewerMTKView { makeView(context.coordinator) }
    func updateUIView(_ view: ViewerMTKView, context: Context) { update(view) }
    static func dismantleUIView(_ view: ViewerMTKView, coordinator: Coordinator) { dismantle(coordinator) }
}

final class ViewerMTKView: MTKView {
    var onZoom: ((Double, CGPoint) -> Void)?
    var onScrollPan: ((CGSize) -> Void)?
    var onLoupeScroll: ((Double) -> Bool)?
}

extension MTKView {
    func requestRedraw() { setNeedsDisplay() }
    func setLayerOpaque() { layer.isOpaque = true }
}
#endif
