import Foundation

/// Tool-workspace actions that touch both the layout and the window state
/// (selected tab, collapsed panels): the Tools menu, the tab strip and panel
/// context menus, and the Customize Tools window all go through here.
@MainActor
public enum ToolWorkspaceCommands {
    /// Tools menu: every tool, grouped by the tab that holds it by default.
    public static var menuSections: [(tab: ToolTab, panels: [ToolPanelID])] {
        ToolTab.homeTabs.map { ($0, $0.panels) }
    }

    /// The tab the sidebar shows: the selected one, or the first if it was removed.
    public static func selectedTab(model: AppModel, layout: ToolLayout) -> ToolTab {
        layout.workspace.resolve(model.toolTab)
    }

    /// Show `panel`: bring the tool panels back (hidden panels, full screen),
    /// switch to a tab holding it (adding it back to its home tab if no tab
    /// does), expand it and scroll to it.
    public static func reveal(_ panel: ToolPanelID, model: AppModel, layout: ToolLayout) {
        let current = selectedTab(model: model, layout: layout)
        model.toolTab = layout.edit { $0.ensureReachable(panel, preferring: current) }
        show(panel, model: model, layout: layout)
    }

    /// Show the tools an AI edit changed on the focused photo: every one expanded, and the
    /// sidebar on the first in its order (the selected tab's, else the first tab holding
    /// one; a tool no tab holds goes back to its home tab, like any reveal).
    public static func revealAIChanges(model: AppModel, layout: ToolLayout) {
        let changed = model.aiChangedPanels
        let inOrder = ([selectedTab(model: model, layout: layout)] + layout.order).flatMap { layout.panels(for: $0) }
        guard let first = inOrder.first(where: changed.contains) ?? ToolPanelID.allCases.first(where: changed.contains)
        else { return }
        model.collapsedPanels.subtract(changed)
        reveal(first, model: model, layout: layout)
    }

    private static func show(_ panel: ToolPanelID, model: AppModel, layout: ToolLayout) {
        model.panelsHidden = false
        if model.viewer.state.fullScreen { model.viewer.state.fullScreen = false }
        model.collapsedPanels.remove(panel)
        layout.requestReveal(panel)
    }

    /// Add a tool to a tab and show it there.
    public static func add(_ panel: ToolPanelID, to tab: ToolTab, before target: ToolPanelID? = nil,
                           model: AppModel, layout: ToolLayout) {
        layout.edit { $0.insert(panel, in: tab, before: target) }
        model.toolTab = tab
        show(panel, model: model, layout: layout)
    }

    /// The Add Tool menu (the strip's +, a panel's ⋯, an empty tab): a tool already
    /// in `tab` is removed; any other is added after `after` (else at the end) and
    /// shown. Either is one Undo Workspace Change step.
    public static func toggle(_ panel: ToolPanelID, in tab: ToolTab, after: ToolPanelID? = nil,
                              model: AppModel, layout: ToolLayout) {
        let before = layout.workspace, collapsed = model.collapsedPanels
        let inTab = layout.panels(for: tab)
        if inTab.contains(panel) {
            layout.edit { $0.remove(panel, from: tab) }
        } else {
            let next = after.flatMap { inTab.firstIndex(of: $0) }.flatMap { inTab.indices.contains($0 + 1) ? inTab[$0 + 1] : nil }
            add(panel, to: tab, before: next, model: model, layout: layout)
        }
        layout.rememberForUndo(before, collapsed: collapsed)
    }

    /// A tool dropped on a tab icon: added to that tab. Already there (dragged
    /// from that tab's own stack): nothing happens. False when ignored.
    @discardableResult
    public static func drop(_ panel: ToolPanelID, onTab tab: ToolTab, model: AppModel, layout: ToolLayout) -> Bool {
        guard layout.workspace.contains(tab), !layout.panels(for: tab).contains(panel) else { return false }
        add(panel, to: tab, model: model, layout: layout)
        return true
    }

    /// Move a tool to another tab and follow it there.
    public static func move(_ panel: ToolPanelID, from tab: ToolTab, to other: ToolTab, model: AppModel, layout: ToolLayout) {
        layout.edit { $0.move(panel, from: tab, to: other) }
        model.toolTab = other
        layout.requestReveal(panel)
    }

    /// Remove a tab; the selection moves to its neighbour.
    public static func removeTab(_ tab: ToolTab, model: AppModel, layout: ToolLayout) {
        guard layout.workspace.canRemoveTab(tab), let i = layout.order.firstIndex(of: tab) else { return }
        let wasSelected = selectedTab(model: model, layout: layout) == tab
        layout.edit { $0.removeTab(tab) }
        if wasSelected { model.toolTab = layout.order[min(i, layout.order.count - 1)] }
    }

    /// A new custom tab after the selected one, selected.
    @discardableResult
    public static func newTab(title: String, symbol: String? = nil, model: AppModel, layout: ToolLayout) -> ToolTab {
        let current = selectedTab(model: model, layout: layout)
        let tab = layout.edit { $0.addCustomTab(title: title, symbol: symbol, after: current) }
        model.toolTab = tab
        return tab
    }

    /// Put a hidden built-in tab (All Tools, or one the user removed) back, selected.
    public static func showTab(_ tab: ToolTab, model: AppModel, layout: ToolLayout) {
        layout.edit { $0.addBuiltInTab(tab) }
        if layout.workspace.contains(tab) { model.toolTab = tab }
    }

    /// Tools ▸ Show All Tools Tab.
    public static func toggleAllToolsTab(model: AppModel, layout: ToolLayout) {
        if layout.workspace.contains(.allTools) { removeTab(.allTools, model: model, layout: layout) } else { showTab(.allTools, model: model, layout: layout) }
    }

    @discardableResult
    public static func saveWorkspace(named name: String, model: AppModel, layout: ToolLayout) -> Bool {
        layout.saveWorkspace(named: name, collapsed: model.collapsedPanels)
    }

    /// Load a saved workspace: its tabs, tools and collapsed panels. The layout it
    /// replaces stays one Undo Workspace Change away (the toast says so).
    public static func loadWorkspace(named name: String, model: AppModel, layout: ToolLayout) {
        guard let collapsed = layout.loadWorkspace(named: name, collapsed: model.collapsedPanels) else { return }
        apply(collapsed, model: model, layout: layout, message: "Workspace “\(name)”")
    }

    /// The built-in layout, with the default collapsed panels (undoable).
    public static func resetToDefault(model: AppModel, layout: ToolLayout) {
        apply(layout.resetToDefault(collapsed: model.collapsedPanels), model: model, layout: layout, message: "Default tools")
    }

    /// Tools ▸ Workspaces ▸ Undo Workspace Change.
    public static func undoWorkspaceChange(model: AppModel, layout: ToolLayout) {
        guard let collapsed = layout.undoWorkspaceChange() else { return }
        model.collapsedPanels = collapsed
        model.toolTab = selectedTab(model: model, layout: layout)
    }

    private static func apply(_ collapsed: Set<ToolPanelID>, model: AppModel, layout: ToolLayout, message: String) {
        model.collapsedPanels = collapsed
        model.toolTab = selectedTab(model: model, layout: layout)
        if layout.undoWorkspace != nil {
            model.show(Toast("\(message). Undo with Tools ▸ Workspaces ▸ Undo Workspace Change.", symbol: "slider.horizontal.3"))
        }
    }
}
