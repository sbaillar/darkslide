import Foundation

/// The Customize Tools Name field's edit: which tab it renames and the name it
/// showed when editing began. Only a real change is committed, so an edit that
/// outlived a workspace load, reset or undo never writes a stale name.
public struct TabNameEdit: Equatable, Sendable {
    public private(set) var tab: ToolTab?
    public private(set) var original = ""

    public init() {}

    /// The field gained focus showing `title` for `tab`.
    public mutating func begin(_ tab: ToolTab, title: String) {
        self.tab = tab
        original = title
    }

    /// Editing ends with `text`: the rename to apply, if the text changed.
    public mutating func end(_ text: String) -> (tab: ToolTab, name: String)? {
        defer { cancel() }
        guard let tab, text != original else { return nil }
        return (tab, text)
    }

    /// Drop the edit (the layout was replaced under it).
    public mutating func cancel() {
        tab = nil
        original = ""
    }
}
