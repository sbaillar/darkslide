import Foundation

/// A whole keyboard set, chosen in Settings › Familiar layout independently of the
/// layout: people coming from another app keep the keys their hands know.
///
/// Lightroom Classic follows Adobe's "Keyboard shortcuts for Lightroom Classic"
/// (Oct 2025). Capture One takes its cursor-tool letters; its other keys aren't
/// changed until checked against Capture One's own Edit › Edit Keyboard Shortcuts.
public enum KeyPreset: String, CaseIterable, Codable, Sendable, Identifiable {
    case luxtable, lightroomClassic, captureOne

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .luxtable: "Luxtable"
        case .lightroomClassic: "Lightroom Classic"
        case .captureOne: "Capture One"
        }
    }

    /// The key set that goes with a familiar layout (the first-run choice sets both).
    public init(layout: LayoutPreset) {
        switch layout {
        case .luxtable: self = .luxtable
        case .lightroomClassic: self = .lightroomClassic
        case .captureOne: self = .captureOne
        }
    }

    /// A few of the set's keys, for Settings and the first-run choice.
    public var summary: String {
        switch self {
        case .luxtable: "P X U flag, C crop, R straighten, ⇧A auto"
        case .lightroomClassic: "P X U flag, R crop, C compare, K brush, ⌘U auto"
        case .captureOne: "C crop, R rotate, B brush, E erase, G gradient"
        }
    }

    /// The single-key bindings this set starts from (the user's changes go on top).
    public var bindings: KeyBindings {
        var b = KeyBindings.defaults
        switch self {
        case .luxtable:
            break
        case .lightroomClassic:
            // C is Compare and R the Crop tool; straighten lives inside Crop (no key).
            b.unbind(.straightenTool)
            b.bind(.pinCompare, to: "c")
            b.bind(.cropTool, to: "r")
            b.bind(.brushTool, to: "k")
            b.bind(.gradientTool, to: "m")
        case .captureOne:
            // Cursor tools: B draw mask, E erase, G linear and T radial gradient. They
            // take the letters Luxtable gives the filmstrip, viewer and grid (↩ still
            // opens the viewer; the toolbar switches views).
            b.bind(.brushTool, to: "b")
            b.bind(.eraserTool, to: "e")
            b.bind(.gradientTool, to: "g")
            b.bind(.radialTool, to: "t")
        }
        return b
    }

    /// The menu shortcut for `c` in this set; nil: the command has none.
    public func shortcut(_ c: MenuCommand) -> MenuCommand.Shortcut? {
        switch (self, c) {
        case (.lightroomClassic, .autoAdjust): MenuCommand.Shortcut(key: "u", modifiers: [.command])
        case (.lightroomClassic, .exportSelected): MenuCommand.Shortcut(key: "e", modifiers: [.command, .shift])
        case (.lightroomClassic, .newVariant): MenuCommand.Shortcut(key: "'", modifiers: [.command])
        default: c.shortcut
        }
    }

    /// How many commands the user has rebound away from this set's own keys.
    public func changes(in bindings: KeyBindings) -> Int {
        let base = self.bindings
        return BindableCommand.allCases.filter { base.key(for: $0) != bindings.key(for: $0) }.count
    }
}
