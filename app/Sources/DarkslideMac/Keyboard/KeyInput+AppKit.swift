import AppKit
import DarkslideCore
import DarkslideUI

extension KeyInput {
    /// Translate a key-down event. Uses `charactersIgnoringModifiers` so ⇧/ gives "?"
    /// only via the real `characters`, and ⌘⇧C still reads as "c".
    public init(event: NSEvent) {
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        let special: Special? = switch event.keyCode {
        case 123: .left
        case 124: .right
        case 125: .down
        case 126: .up
        case 49: .space
        case 53: .escape
        case 36, 76: .returnKey
        case 48: .tab
        default: nil
        }
        var chars = flags.contains(.command) ? (event.charactersIgnoringModifiers ?? "") : (event.characters ?? "")
        // Digit-row keys by position, so ratings/tags work on AZERTY and other layouts (M5).
        if !flags.contains(.command), let positional = Self.digitRow[event.keyCode], chars != "?" {
            chars = positional
        }
        self.init(chars, special: special, command: flags.contains(.command), shift: flags.contains(.shift),
                  option: flags.contains(.option), control: flags.contains(.control),
                  isRepeat: event.type == .keyDown && event.isARepeat)
    }
}

extension FocusedResponder {
    @MainActor
    public static func classify(_ responder: NSResponder?) -> FocusedResponder {
        switch responder {
        case is NSTextView, is NSTextField: .text
        case let c as NSControl where c.acceptsFirstResponder && c.refusesFirstResponder == false: .control
        default: .none
        }
    }
}
