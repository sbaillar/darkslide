import AppKit

/// Menu-bar routing, outside the (coverage-excluded) `Commands` body (M10).
///
/// Standard editing shortcuts are shared with text fields: when one is editing,
/// ⌘Z/⇧⌘Z/⌘A go to the field (responder chain); otherwise they act on photos.
@MainActor
public enum MenuActions {
    public enum Shared: Sendable {
        case undo, redo, selectAll

        var selector: Selector {
            switch self {
            case .undo: Selector(("undo:"))
            case .redo: Selector(("redo:"))
            case .selectAll: #selector(NSText.selectAll(_:))
            }
        }
    }

    /// Returns the selector to send down the responder chain, or `nil` if the model handled it.
    @discardableResult
    public static func perform(_ action: Shared, model: AppModel, textEditing: Bool) -> Selector? {
        if textEditing { return action.selector }
        switch action {
        case .undo: model.undo()
        case .redo: model.redo()
        case .selectAll: model.selectAll()
        }
        return nil
    }

    public static func isEnabled(_ action: Shared, model: AppModel, textEditing: Bool) -> Bool {
        if textEditing { return true }
        switch action {
        case .undo: return model.history.canUndo
        case .redo: return model.history.canRedo
        case .selectAll: return !model.visibleIDs.isEmpty
        }
    }

    /// Menu title with the user's plain-key binding shown, e.g. "Pick    P".
    /// (Plain keys are handled by the key monitor, so menus display rather than bind them.)
    public static func title(_ command: BindableCommand, bindings: KeyBindings) -> String {
        guard let k = bindings.key(for: command) else { return command.title }
        return "\(command.title)\t\(String(k).uppercased())"
    }
}
