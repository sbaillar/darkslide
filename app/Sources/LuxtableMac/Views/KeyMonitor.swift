import AppKit
import LuxtableCore
import LuxtableUI
import SwiftUI

/// Installs a local key-down monitor for culling keys while this view's window is key.
/// Removed when the view goes away (no leaked monitor, weak model reference).
struct KeyMonitor: NSViewRepresentable {
    let model: AppModel

    func makeNSView(context: Context) -> MonitorView {
        let v = MonitorView()
        v.model = model
        return v
    }

    func updateNSView(_ view: MonitorView, context: Context) {
        view.model = model
    }

    static func dismantleNSView(_ view: MonitorView, coordinator: ()) {
        view.removeMonitor()
    }

    final class MonitorView: NSView {
        weak var model: AppModel?
        private var monitor: Any?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            removeMonitor()
            guard window != nil else { return }
            monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp, .leftMouseDown]) { [weak self] event in
                // AppKit calls local monitors on the main thread.
                let handled = MainActor.assumeIsolated { self?.handle(event) ?? false }
                return handled ? nil : event
            }
        }

        /// True when the key press was consumed.
        private func handle(_ event: NSEvent) -> Bool {
            guard let model, let window, event.window === window else { return false }
            switch event.type {
            case .keyUp:
                model.setKeyRepeat(false)
                return false
            case .leftMouseDown:
                // M5: clicking the viewer/grid ends text editing so culling keys work again.
                if FocusedResponder.classify(window.firstResponder) == .text,
                   FocusedResponder.classify(window.contentView?.hitTest(event.locationInWindow)) != .text {
                    window.makeFirstResponder(nil)
                }
                return false
            default:
                let focus = KeyMonitorPolicy.effectiveFocus(FocusedResponder.classify(window.firstResponder))
                let key = KeyInput(event: event)
                // M5/N17: Tab moves focus between controls (and always with Full Keyboard Access).
                if KeyMonitorPolicy.passesThrough(key, focus: focus, fullKeyboardAccess: NSApp.isFullKeyboardAccessEnabled) {
                    return false
                }
                return model.handleKey(key, focus: focus,
                                       sheetOpen: window.attachedSheet != nil, isMainWindow: true)
            }
        }

        func removeMonitor() {
            if let monitor { NSEvent.removeMonitor(monitor) }
            monitor = nil
        }
    }
}
