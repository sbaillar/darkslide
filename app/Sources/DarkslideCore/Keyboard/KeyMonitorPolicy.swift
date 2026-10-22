import Foundation

extension KeyInput {
    /// ANSI key codes of the digit row and the minus key (layout independent).
    package static let digitRow: [UInt16: String] = [
        29: "0", 18: "1", 19: "2", 20: "3", 21: "4", 23: "5", 22: "6", 26: "7", 28: "8", 25: "9", 27: "-",
    ]
}

/// What currently has keyboard focus, as far as the culling monitor cares.
public enum FocusedResponder: Equatable, Sendable {
    /// Nothing that consumes keys (viewer, grid, window background).
    case none
    /// A text field/view is editing: it owns every key.
    case text
    /// A focused control (slider, stepper, popup…): it owns arrows, Space and Return.
    case control
}

/// When the window-level key monitor may take a key press.
///
/// Focus-trap rules: never while a text field is editing (field editor is first
/// responder), never while a sheet is open (sheets own the keyboard), never for
/// ⌘ shortcuts (the menu bar handles those, so text fields keep ⌘Z/⌘C), and
/// only for the main window.
public enum KeyMonitorPolicy {
    public static func shouldHandle(_ key: KeyInput, textEditing: Bool, sheetOpen: Bool, isMainWindow: Bool) -> KeyCommand? {
        shouldHandle(key, focus: textEditing ? .text : .none, sheetOpen: sheetOpen, isMainWindow: isMainWindow)
    }

    /// A focused control keeps its own navigation keys (arrows adjust a slider,
    /// Space/Return press a button, Tab moves focus on); culling letters and digits
    /// still work. Tab hides the panels only when nothing is focused and Full
    /// Keyboard Access is off (N17: Tab must never trap keyboard users).
    /// Keys the window must deliver to AppKit untouched (Tab focus traversal, M5/N17).
    /// What has the keyboard: the first responder, else a focused tool control (a
    /// slider keeps ←/→ only while it has focus; after a drag it resigns, H6).
    @MainActor
    public static func effectiveFocus(_ responder: FocusedResponder, controls: ControlFocus = .shared) -> FocusedResponder {
        responder == .none && controls.isToolControlFocused ? .control : responder
    }

    public static func passesThrough(_ key: KeyInput, focus: FocusedResponder, fullKeyboardAccess: Bool) -> Bool {
        key.special == .tab && (focus != .none || fullKeyboardAccess)
    }

    public static func shouldHandle(_ key: KeyInput, focus: FocusedResponder, sheetOpen: Bool, isMainWindow: Bool,
                                    bindings: KeyBindings = .defaults, fullKeyboardAccess: Bool = false) -> KeyCommand? {
        guard isMainWindow, focus != .text, !sheetOpen, !key.command else { return nil }
        if key.special == .tab, focus == .control || fullKeyboardAccess { return nil }
        if focus == .control, let s = key.special, s != .escape { return nil }
        return KeyboardRouter.command(for: key, bindings: bindings)
    }
}
