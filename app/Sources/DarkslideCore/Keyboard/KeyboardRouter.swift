import Foundation

/// Everything the keyboard can do. `AppModel.perform(_:)` executes these.
public enum KeyCommand: Equatable, Sendable {
    case move(Direction, extend: Bool)
    case rate(Int)
    case tag(ColorTag)
    case pick, reject, unflag
    case zoomToggleAtCursor
    case toggleFullScreen
    /// F / Tab: hide tool panels and filmstrip (C1/LR); real full screen is ⌃⌘F.
    case togglePanels
    /// B: hide just the filmstrip (the tool panels stay).
    case toggleFilmstrip
    /// S: expand/collapse the burst stack of the focused photo.
    case toggleStack
    case copyAdjustments, applyAdjustments, copyFromPrevious
    case undo, redo
    case showCheatSheet
    case gridMode, viewerMode
    case toggleBeforeAfter
    case togglePinCompare
    /// [cmp9] Enter/leave a canvas tool (crop C, straighten R, color picker I).
    case tool(CanvasTool)
    /// A mask tool (brush, eraser, gradients) on the selected layer, adding one if needed.
    case maskTool(CanvasTool)
    case whiteBalancePicker
    case survey
    case toggleClipping
    case auto
    /// ⇧S: soft proof on/off.
    case toggleSoftProof
    case selectAll
    case escape
    /// [lay9] [ / ]: smaller / larger brush (or heal/clone spot).
    case brushSize(Int)

    public enum Direction: Sendable { case left, right, up, down }
}

/// A key press, decoupled from `NSEvent` so routing is unit-testable.
public struct KeyInput: Equatable, Sendable {
    public enum Special: Sendable { case left, right, up, down, space, escape, returnKey, tab }

    public var characters: String
    public var special: Special?
    public var command = false
    public var shift = false
    public var option = false
    public var control = false
    /// Auto-repeat (key held down).
    public var isRepeat = false

    public init(_ characters: String = "", special: Special? = nil, command: Bool = false, shift: Bool = false,
                option: Bool = false, control: Bool = false, isRepeat: Bool = false) {
        self.isRepeat = isRepeat
        self.characters = characters
        self.special = special
        self.command = command
        self.shift = shift
        self.option = option
        self.control = control
    }
}

/// Maps key presses to commands (PLAN §6 culling keys + §8 copy/apply).
public enum KeyboardRouter {
    public static func command(for key: KeyInput, bindings: KeyBindings = .defaults) -> KeyCommand? {
        if key.control { return nil }
        if let special = key.special {
            guard !key.command, !key.option else { return nil }
            switch special {
            case .left: return .move(.left, extend: key.shift)
            case .right: return .move(.right, extend: key.shift)
            case .up: return .move(.up, extend: key.shift)
            case .down: return .move(.down, extend: key.shift)
            case .space: return key.shift ? nil : .zoomToggleAtCursor
            case .escape: return .escape
            case .returnKey: return .viewerMode
            case .tab: return key.shift ? nil : .togglePanels
            }
        }
        let c = key.characters.lowercased()
        if key.command {
            switch (c, key.shift, key.option) {
            case ("c", true, false): return .copyAdjustments
            case ("v", true, false): return .applyAdjustments
            case ("v", false, true): return .copyFromPrevious
            case ("z", false, false): return .undo
            case ("z", true, false): return .redo
            case ("a", false, false): return .selectAll
            default: return nil
            }
        }
        if key.option { return nil }
        guard let ch = key.characters.first, key.characters.count == 1 else { return nil }
        if ch == "?" { return .showCheatSheet }
        if key.shift, c == "a" { return .auto }
        if key.shift, c == "s" { return .toggleSoftProof }
        if ch == "[" { return .brushSize(-1) } // [lay9]
        if ch == "]" { return .brushSize(1) }
        // Otherwise shift is ignored (caps lock / shifted letters still cull).
        return bindings.command(for: ch)
    }

    /// Rows for the `?` cheat sheet, grouped, with the user's current keys (N15).
    public static func cheatSheet(bindings: KeyBindings = .defaults) -> [(section: String, rows: [(keys: String, action: String)])] {
        func k(_ c: BindableCommand) -> String { bindings.key(for: c).map(KeyBindings.display) ?? "—" }
        func ks(_ cs: [BindableCommand], _ sep: String = " / ") -> String { cs.map(k).joined(separator: sep) }
        return [
            ("Navigate", [("← → ↑ ↓", "Previous / next photo"), ("⇧ + arrows", "Extend selection"),
                          (ks([.gridMode, .viewerMode]), "Grid / Viewer"), ("↩", "Open in viewer"), ("⌘A", "Select all")]),
            ("Cull", [(ks([.rate1, .rate2, .rate3, .rate4, .rate5], " "), "Rate 1–5 stars"), (k(.rate0), "Clear rating"),
                      (ks([.tagRed, .tagYellow, .tagGreen, .tagBlue, .tagPurple], " "), "Red, yellow, green, blue, purple tag"),
                      (ks([.tagOrange, .tagPink], " "), "Orange, pink tag (unbound unless you assign keys)"),
                      (ks([.pick, .reject, .unflag]), "Pick / Reject / Unflag")]),
            ("View", [("Space", "100% at cursor"), ("\(k(.togglePanels)) / Tab", "Hide panels"), ("⌃⌘F", "Full screen"),
                      (k(.beforeAfter), "Before / after"), (k(.pinCompare), "Pin for compare"),
                      (k(.toggleStack), "Expand / collapse stack"),
                      ("← → ↩ Esc", "Burst strip: frames, use as cover, close"), (k(.survey), "Survey the selection"),
                      (k(.toggleClipping), "Clipping warning"), ("⇧S", "Soft proof on / off"),
                      ("Esc", "Back / close")]),
            ("Compose", [(k(.cropTool), "Crop tool (⏎ apply, Esc cancel, X swap orientation)"),
                         (k(.straightenTool), "Straighten tool (drag along the horizon)"),
                         (k(.colorPickerTool), "Color readout picker"),
                         (k(.whiteBalanceTool), "White balance picker"),
                         (ks([.brushTool, .eraserTool]), "Brush / erase mask on the selected layer"),
                         (ks([.gradientTool, .radialTool]), "Linear / radial gradient mask")]
                            .filter { $0.keys.contains { $0.isLetter || $0.isNumber } }), // no key bound: not listed
            ("Edit", [("⌘K", "Edit with words (“warmer, a bit brighter”)"), ("⇧A", "Auto adjust"), ("⌥⌘A", "AI Optimize (explained suggestions)"), ("⌘⇧C", "Copy the edited adjustments (▾ beside Copy: Edited or Custom)"),
                      ("⌘⇧V", "Apply them to the selected photos but the copied one (one ⌘Z)"),
                      ("⌥⌘V", "Copy from previous"), ("⌥⇧⌘M", "Match total exposures to the focused photo"),
                      ("⇧⌘H", "Merge to HDR (2+ photos)"), ("⇧⌘M", "Merge to Panorama (2+ photos)"),
                      ("⌘Z / ⇧⌘Z", "Undo / Redo"), ("?", "This cheat sheet")]),
            ("Voice", [("⌃⌘V", "Voice commands on / off; then say:"),
                       ("“next” “back”", "Next / previous photo (also “first”, “last”)"),
                       ("“next 3” “go to 12”", "Move several, go to a photo (“the 4th photo”)"),
                       ("“reject the next 3”", "Several photos at once, one undo step"),
                       ("“five” “3 stars”", "Rate (“clear”: no stars)"), ("“pick” “reject”", "Flag (“unflag”)"),
                       ("“red” … “pink”", "Color tag"), ("“five next”", "Rate, then next"),
                       ("“grid” “fit”", "Grid, viewer, zoom in / out, fit, hundred percent"),
                       ("“undo” “auto”", "Undo, redo, copy / apply adjustments, auto adjust"),
                       ("“edit warmer”", "Edit with words"),
                       ("“what can I say?”", "Every voice command (also View ▸ What Can I Say?)")]),
        ]
    }
}
