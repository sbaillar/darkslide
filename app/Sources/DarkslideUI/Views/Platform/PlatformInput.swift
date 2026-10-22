#if os(macOS)
import AppKit
#endif

/// Keyboard modifiers held right now, read by pointer gestures mid-drag or on click.
///
/// iOS (unverified): there is no global modifier state. A hardware keyboard's modifiers
/// reach gesture recognizers (`UIGestureRecognizer.modifierFlags`), which the touch views
/// pass in phase 2; until then nothing reads as held.
@MainActor
enum PlatformInput {
    /// Fine adjustment for sliders and scrub fields: Option on the Mac (a second finger on iPad).
    static var fineAdjustActive: Bool { held(.option) }
    /// Shift: extend a selection, stack a look.
    static var shiftDown: Bool { held(.shift) }
    /// Command: toggle one photo in the selection.
    static var commandDown: Bool { held(.command) }

    private enum Key { case option, shift, command }

    private static func held(_ key: Key) -> Bool {
        #if os(macOS)
        let flags = NSEvent.modifierFlags
        switch key {
        case .option: return flags.contains(.option)
        case .shift: return flags.contains(.shift)
        case .command: return flags.contains(.command)
        }
        #else
        return false
        #endif
    }
}
