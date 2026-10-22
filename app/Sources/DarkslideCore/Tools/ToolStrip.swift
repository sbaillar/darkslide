import Foundation

/// The tool-tab strip across the top of the tools pane (Capture One's tool tabs):
/// which tabs fit, and which overflow into the » menu.
public enum ToolStrip {
    /// One tab icon, and the gap after it.
    public static let iconWidth = 28.0, spacing = 2.0
    /// The divider after Favorites (a 1 pt line, 4 pt either side), and its gap.
    public static let dividerWidth = 11.0
    /// The » and + buttons at the end, each with its gap.
    public static let buttonWidth = 26.0

    /// The tabs shown in a strip `width` points wide and the rest (the » menu), both
    /// in strip order. The selected tab always shows, in its own place: room is kept
    /// for it, and the tabs after the first that doesn't fit go in the » menu.
    public static func split(_ tabs: [ToolTab], selected: ToolTab, width: Double)
        -> (visible: [ToolTab], overflow: [ToolTab]) {
        var room = width - buttonWidth
        if tabs.map(cost).reduce(0, +) <= room { return (tabs, []) }
        room -= buttonWidth
        var used = tabs.contains(selected) ? cost(selected) : 0
        var full = false
        var visible: [ToolTab] = []
        for t in tabs {
            if t == selected {
                visible.append(t)
            } else if !full, used + cost(t) <= room {
                used += cost(t)
                visible.append(t)
            } else {
                full = true
            }
        }
        return (visible, tabs.filter { !visible.contains($0) })
    }

    /// Width a tab takes in the strip (with its gap, and the divider after Favorites).
    public static func cost(_ t: ToolTab) -> Double { iconWidth + spacing + (t == .favorites ? dividerWidth : 0) }

    /// Width the strip's contents take: the visible tabs, the » button when some
    /// overflow, and the + button.
    public static func usedWidth(_ split: (visible: [ToolTab], overflow: [ToolTab])) -> Double {
        split.visible.map(cost).reduce(0, +) + (split.overflow.isEmpty ? 0 : buttonWidth) + buttonWidth
    }

    /// Whether any of `tabs` holds a tool an AI edit changed (the » button's dot,
    /// so a change in an overflowed tab isn't hidden).
    public static func hasAIChanges(_ tabs: [ToolTab], panels: (ToolTab) -> [ToolPanelID], changed: Set<ToolPanelID>) -> Bool {
        tabs.contains { AIHighlightStyle.tabHasChanges(panels($0), changed: changed) }
    }
}
