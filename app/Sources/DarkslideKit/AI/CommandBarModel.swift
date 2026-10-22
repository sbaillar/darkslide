import Foundation
import Observation

/// The ⌘K "Edit with words" bar (C4): a field with suggestions that sends its text
/// through `AppModel.runInstruction` (guarded parser → edit, straighten, toast).
@MainActor @Observable
public final class CommandBarModel {
    public var isPresented = false
    public var text = ""
    /// The highlighted suggestion (↑/↓ move it, ⏎ takes it when the field is empty).
    public var highlighted = 0
    public private(set) var isRunning = false
    /// The toast of the last instruction (what changed, what wasn't understood).
    public private(set) var lastResult: Toast?

    public init() {}

    /// Starter phrases (each is understood by the rule parser alone).
    public static let suggestions = [
        "warmer and a bit brighter", "more contrast", "lift the shadows", "recover the highlights",
        "cooler", "a bit darker", "black and white", "more saturation", "straighten", "crop 4:5", "crop 1:1",
        "undo", "reset",
    ]

    /// Suggestions matching what's typed (all of them for an empty field), at most 6.
    public var filtered: [String] {
        let t = text.lowercased().trimmingCharacters(in: .whitespaces)
        let list = t.isEmpty ? Self.suggestions
            : Self.suggestions.filter { $0.hasPrefix(t) } + Self.suggestions.filter { !$0.hasPrefix(t) && $0.contains(t) }
        return Array(list.prefix(6))
    }

    public func present() {
        text = ""
        highlighted = 0
        isPresented = true
    }

    public func dismiss() {
        isPresented = false
    }

    public func moveHighlight(by delta: Int) {
        let n = filtered.count
        guard n > 0 else { return }
        highlighted = ((highlighted + delta) % n + n) % n
    }

    /// What ⏎ runs: the typed text, else the highlighted suggestion.
    public var submission: String? {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if !t.isEmpty { return t }
        let f = filtered
        return f.indices.contains(highlighted) ? f[highlighted] : nil
    }

    /// Run the instruction on `model` and close the bar.
    @discardableResult
    public func submit(to model: AppModel, text override: String? = nil) async -> Toast? {
        guard !isRunning, let instruction = override ?? submission else { return nil }
        isRunning = true
        defer { isRunning = false }
        isPresented = false
        let result = await model.runInstruction(instruction)
        lastResult = result
        text = ""
        return result
    }
}
