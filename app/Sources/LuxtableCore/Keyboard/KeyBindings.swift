import Foundation

/// Plain-key commands the user can rebind. Arrows, Space, Tab, Esc, ⏎ and
/// ⌘-shortcuts are fixed (menus own ⌘ keys).
public enum BindableCommand: String, CaseIterable, Codable, Sendable, Identifiable {
    case rate0, rate1, rate2, rate3, rate4, rate5
    case tagRed, tagYellow, tagGreen, tagBlue, tagPurple
    /// Capture One's extra labels: unbound by default.
    case tagOrange, tagPink
    case pick, reject, unflag
    case togglePanels, gridMode, viewerMode, beforeAfter, pinCompare, toggleStack
    /// Hide just the filmstrip (Capture One ⌥⌘B): a taller viewer on small screens.
    case toggleFilmstrip
    /// Composition tools.
    case cropTool, straightenTool, colorPickerTool
    /// Mask tools on the selected layer (a new layer when none is selected).
    case brushTool, eraserTool, gradientTool, radialTool
    /// The white balance picker; Survey of the selection; the clipping warning.
    case whiteBalanceTool, survey, toggleClipping

    public var id: String { rawValue }

    public var command: KeyCommand {
        switch self {
        case .rate0: .rate(0)
        case .rate1: .rate(1)
        case .rate2: .rate(2)
        case .rate3: .rate(3)
        case .rate4: .rate(4)
        case .rate5: .rate(5)
        case .tagRed: .tag(.red)
        case .tagYellow: .tag(.yellow)
        case .tagGreen: .tag(.green)
        case .tagBlue: .tag(.blue)
        case .tagPurple: .tag(.purple)
        case .tagOrange: .tag(.orange)
        case .tagPink: .tag(.pink)
        case .pick: .pick
        case .reject: .reject
        case .unflag: .unflag
        case .togglePanels: .togglePanels
        case .gridMode: .gridMode
        case .viewerMode: .viewerMode
        case .beforeAfter: .toggleBeforeAfter
        case .pinCompare: .togglePinCompare
        case .toggleStack: .toggleStack
        case .toggleFilmstrip: .toggleFilmstrip
        case .cropTool: .tool(.crop)
        case .straightenTool: .tool(.straighten)
        case .colorPickerTool: .tool(.colorPicker)
        case .brushTool: .maskTool(.brush)
        case .eraserTool: .maskTool(.eraser)
        case .gradientTool: .maskTool(.linearGradient)
        case .radialTool: .maskTool(.radialGradient)
        case .whiteBalanceTool: .whiteBalancePicker
        case .survey: .survey
        case .toggleClipping: .toggleClipping
        }
    }

    public var title: String {
        switch self {
        case .rate0: "Clear rating"
        case .rate1, .rate2, .rate3, .rate4, .rate5: "Rate \(rawValue.last!) star\(rawValue.last == "1" ? "" : "s")"
        case .tagRed: "Red tag"
        case .tagYellow: "Yellow tag"
        case .tagGreen: "Green tag"
        case .tagBlue: "Blue tag"
        case .tagPurple: "Purple tag"
        case .tagOrange: "Orange tag"
        case .tagPink: "Pink tag"
        case .pick: "Pick"
        case .reject: "Reject"
        case .unflag: "Unflag"
        case .togglePanels: "Hide panels"
        case .gridMode: "Grid"
        case .viewerMode: "Viewer"
        case .beforeAfter: "Before / after"
        case .pinCompare: "Pin for compare"
        case .toggleStack: "Expand / collapse stack"
        case .toggleFilmstrip: "Hide filmstrip"
        case .cropTool: "Crop tool"
        case .straightenTool: "Straighten tool"
        case .colorPickerTool: "Color picker"
        case .brushTool: "Brush mask"
        case .eraserTool: "Erase mask"
        case .gradientTool: "Linear gradient mask"
        case .radialTool: "Radial gradient mask"
        case .whiteBalanceTool: "White balance picker"
        case .survey: "Survey selected"
        case .toggleClipping: "Clipping warning"
        }
    }
}

/// Single-key bindings, one command per key.
public struct KeyBindings: Equatable, Sendable {
    public private(set) var keys: [BindableCommand: Character]

    public static let defaults = KeyBindings(keys: [
        .rate0: "0", .rate1: "1", .rate2: "2", .rate3: "3", .rate4: "4", .rate5: "5",
        .tagRed: "6", .tagYellow: "7", .tagGreen: "8", .tagBlue: "9", .tagPurple: "-",
        .pick: "p", .reject: "x", .unflag: "u",
        .togglePanels: "f", .gridMode: "g", .viewerMode: "e", .beforeAfter: "y", .pinCompare: "k", .toggleStack: "s",
        .toggleFilmstrip: "b",
        // C crops (Capture One), R straightens (user request; was A, now unbound),
        // I picks a color; pin-compare moved from C to K.
        .cropTool: "c", .straightenTool: "r", .colorPickerTool: "i",
        // Lightroom's letters, free in Luxtable's own set; the mask tools stay unbound.
        .whiteBalanceTool: "w", .survey: "n", .toggleClipping: "j",
    ])

    public init(keys: [BindableCommand: Character]) { self.keys = keys }

    public func key(for c: BindableCommand) -> Character? { keys[c] }

    public func command(for key: Character) -> KeyCommand? {
        let k = Character(key.lowercased())
        return keys.first { $0.value == k }?.key.command
    }

    /// Bind `key` to `command`; a command that had this key loses it and is returned.
    @discardableResult
    public mutating func bind(_ command: BindableCommand, to key: Character) -> BindableCommand? {
        let k = Character(key.lowercased())
        let displaced = keys.first { $0.value == k && $0.key != command }?.key
        if let displaced { keys[displaced] = nil }
        keys[command] = k
        return displaced
    }

    public mutating func unbind(_ command: BindableCommand) { keys[command] = nil }

    /// Keys reserved for fixed commands (can't be rebound).
    public static let reserved: Set<Character> = ["?", " "]

    /// Why a key press can't become a binding.
    public enum Rejection: Error, Equatable, Sendable {
        /// Esc, Tab, Return, arrows, Space: fixed navigation keys that never reach bindings.
        case fixedKey(String)
        /// ⌘/⌥/⌃ combinations belong to the menus.
        case modified
        /// `?` and similar keys have a fixed meaning.
        case reserved(Character)
        case notASingleKey

        public var message: String {
            switch self {
            case .fixedKey(let name): "\(name) is a fixed navigation key and can't be rebound."
            case .modified: "Shortcuts with ⌘, ⌥ or ⌃ belong to the menus. Press a single key."
            case .reserved(let c): "“\(c)” is reserved."
            case .notASingleKey: "Press a single letter, digit or symbol key."
            }
        }
    }

    /// The character a recorded key press binds to. Records the *logical* key the
    /// monitor will later see (`KeyInput(event:)`: digit row by position, so AZERTY works).
    public static func recordable(_ key: KeyInput) -> Result<Character, Rejection> {
        if let special = key.special {
            let name = switch special {
            case .escape: "Esc"
            case .tab: "Tab"
            case .returnKey: "Return"
            case .space: "Space"
            case .left, .right, .up, .down: "An arrow key"
            }
            return .failure(.fixedKey(name))
        }
        if key.command || key.option || key.control { return .failure(.modified) }
        guard key.characters.count == 1, let ch = key.characters.first,
              !ch.isWhitespace, !ch.isNewline, ch.unicodeScalars.allSatisfy({ !CharacterSet.controlCharacters.contains($0) })
        else { return .failure(.notASingleKey) }
        let lower = Character(ch.lowercased())
        if reserved.contains(lower) { return .failure(.reserved(lower)) }
        return .success(lower)
    }

    /// Short hint for tooltips, e.g. "(P)"; empty when unbound.
    public func hint(_ c: BindableCommand) -> String {
        key(for: c).map { "(\(Self.display($0)))" } ?? ""
    }

    /// "Pick (P)" style help text.
    public func help(_ text: String, _ c: BindableCommand) -> String {
        let h = hint(c)
        return h.isEmpty ? text : "\(text) \(h)"
    }

    /// Key as shown to people.
    public static func display(_ k: Character) -> String { String(k).uppercased() }

    // MARK: persistence ([command rawValue: key string], "" = explicitly unbound)

    /// Saved with the bindings: which defaults they were saved under. 2: straighten
    /// moved from A to R. Saves without it predate that.
    static let versionKey = "_version"
    static let version = 2

    public var dictionary: [String: String] {
        var d = Dictionary(uniqueKeysWithValues: BindableCommand.allCases.map { ($0.rawValue, keys[$0].map { String($0) } ?? "") })
        d[Self.versionKey] = String(Self.version)
        return d
    }

    /// Restores saved bindings on top of `base` (the chosen key set's own keys):
    /// commands missing from the saved dictionary (added in a later build) get their
    /// base key unless the user already uses that key elsewhere; `""` keeps a command unbound.
    public init(dictionary: [String: String], base: KeyBindings = .defaults) {
        var k: [BindableCommand: Character] = [:]
        var explicit: Set<BindableCommand> = []
        var dictionary = dictionary
        // Saved before the crop tool existed with pin-compare on its old default C:
        // C now crops, pin-compare takes its new default K (a user's own choice is kept).
        if dictionary[BindableCommand.cropTool.rawValue] == nil, dictionary[BindableCommand.pinCompare.rawValue] == "c" {
            dictionary[BindableCommand.pinCompare.rawValue] = nil
        }
        // Saved before straighten moved to R: an A there can't be told from the old
        // default, so it moves to R, unless the user put R on another command (then the
        // A stays, no clash). Saves since then carry the version: an A is a choice.
        let straighten = BindableCommand.straightenTool.rawValue
        if Int(dictionary[Self.versionKey] ?? "") ?? 1 < 2, dictionary[straighten] == "a",
           !dictionary.contains(where: { $0.key != straighten && $0.value == "r" }) {
            dictionary[straighten] = "r"
        }
        // In command order, so a hand-edited file with one key on two commands always gives it
        // to the same one; the other is left unbound.
        for c in BindableCommand.allCases {
            guard let key = dictionary[c.rawValue] else { continue }
            if key.isEmpty {
                explicit.insert(c)
            } else if key.count == 1, let ch = key.first {
                explicit.insert(c)
                let lower = Character(ch.lowercased())
                if !k.values.contains(lower) { k[c] = lower }
            }
        }
        let used = Set(k.values)
        for (c, def) in base.keys where !explicit.contains(c) && !used.contains(def) {
            k[c] = def
        }
        keys = k
    }
}

/// The Shortcuts preference pane's recording logic (kept out of the view).
public enum ShortcutRecorder {
    public struct Outcome: Equatable, Sendable {
        public var bindings: KeyBindings
        public var note: String?
        /// Recording should stop (a key was bound, rejected, or Esc cancelled).
        public var done: Bool
    }

    /// Apply one key press while recording `command`.
    public static func record(_ key: KeyInput, for command: BindableCommand, in bindings: KeyBindings) -> Outcome {
        if key.special == .escape, !key.command, !key.option, !key.control {
            return Outcome(bindings: bindings, note: nil, done: true)
        }
        switch KeyBindings.recordable(key) {
        case .failure(let why):
            return Outcome(bindings: bindings, note: why.message, done: true)
        case .success(let ch):
            var b = bindings
            let displaced = b.bind(command, to: ch)
            let note = displaced.map { "“\(KeyBindings.display(ch))” moved from \($0.title)." }
            return Outcome(bindings: b, note: note, done: true)
        }
    }

    /// Button title for a command's current key.
    public static func keyTitle(_ command: BindableCommand, in bindings: KeyBindings) -> String {
        bindings.key(for: command).map(KeyBindings.display) ?? "—"
    }
}
