import Foundation

/// The tool layout (Capture One workspace): the strip's tabs in order, each
/// with its own tool stack, plus custom tabs. The same tool may sit in several
/// tabs (it is the same edit). Named workspaces also keep the collapsed panels.
/// Pure value logic; `ToolLayout` persists it.
public struct ToolWorkspace: Equatable, Sendable {
    public struct Tab: Equatable, Identifiable, Sendable {
        public var id: ToolTab
        /// nil = the built-in name.
        public var title: String?
        /// nil = the built-in symbol.
        public var symbol: String?
        public var panels: [ToolPanelID]

        public init(id: ToolTab, title: String? = nil, symbol: String? = nil, panels: [ToolPanelID]) {
            self.id = id
            self.title = title
            self.symbol = symbol
            self.panels = panels
        }
    }

    public private(set) var tabs: [Tab]
    /// Panels collapsed when this workspace is loaded (named workspaces only).
    public var collapsed: Set<ToolPanelID>?

    public init(tabs: [Tab], collapsed: Set<ToolPanelID>? = nil) {
        self.tabs = tabs
        self.collapsed = collapsed
    }

    /// N12: Styles & Presets starts collapsed so the tools under it stay in view.
    public static let defaultCollapsed: Set<ToolPanelID> = [.styles, .history]

    /// The built-in layout.
    public static let `default` = ToolWorkspace(tabs: ToolTab.allCases.map { Tab(id: $0, panels: $0.panels) })

    /// Carries over the pre-workspace keys: the tab order and the Favorites list.
    public static func migrated(order: [String]?, favorites: [String]?) -> ToolWorkspace {
        var w = ToolWorkspace.default
        let kept = unique((order ?? []).compactMap(ToolTab.init(rawValue:)).filter { ToolTab.allCases.contains($0) })
        let ids = kept + ToolTab.allCases.filter { !kept.contains($0) }
        w.tabs = ids.map { id in w.tabs.first { $0.id == id }! }
        if let favorites {
            w.tabs[w.index(.favorites)!].panels = unique(favorites.compactMap(ToolPanelID.init(rawValue:)))
        }
        return w
    }

    static func unique<T: Hashable>(_ xs: [T]) -> [T] {
        var seen = Set<T>()
        return xs.filter { seen.insert($0).inserted }
    }

    // MARK: Queries

    public var order: [ToolTab] { tabs.map(\.id) }

    func index(_ tab: ToolTab) -> Int? { tabs.firstIndex { $0.id == tab } }

    public func contains(_ tab: ToolTab) -> Bool { index(tab) != nil }

    public func panels(for tab: ToolTab) -> [ToolPanelID] { index(tab).map { tabs[$0].panels } ?? [] }

    public func title(of tab: ToolTab) -> String { index(tab).flatMap { tabs[$0].title } ?? tab.title }

    /// The title under the familiar-layout names (a tab the user renamed keeps its name).
    public func title(of tab: ToolTab, names: NameSet) -> String { index(tab).flatMap { tabs[$0].title } ?? names.title(of: tab) }

    public func symbol(of tab: ToolTab) -> String { index(tab).flatMap { tabs[$0].symbol } ?? tab.symbol }

    /// Tabs holding `panel`, in strip order.
    public func tabs(containing panel: ToolPanelID) -> [ToolTab] { tabs.filter { $0.panels.contains(panel) }.map(\.id) }

    /// Whether `panel` is in any tab (the Tools menu checkmark).
    public func containsTool(_ panel: ToolPanelID) -> Bool { tabs.contains { $0.panels.contains(panel) } }

    /// Built-in tabs not in the strip (removed by the user, or hidden by default).
    public var hiddenBuiltIns: [ToolTab] { ToolTab.builtIns.filter { !contains($0) } }

    /// `tab` if it is in the strip, else the first tab.
    public func resolve(_ tab: ToolTab) -> ToolTab { contains(tab) ? tab : tabs[0].id }

    // MARK: Tabs

    /// Move `tab` so it lands before `target` (drag in the strip); `nil` = to the end.
    public mutating func moveTab(_ tab: ToolTab, before target: ToolTab?) {
        guard tab != target, let i = index(tab) else { return }
        let t = tabs.remove(at: i)
        tabs.insert(t, at: target.flatMap(index) ?? tabs.count)
    }

    public func canMoveTab(_ tab: ToolTab, by delta: Int) -> Bool {
        guard let i = index(tab) else { return false }
        return delta != 0 && tabs.indices.contains(i + delta)
    }

    /// Move `tab` left (-1) or right (+1) in the strip.
    public mutating func moveTab(_ tab: ToolTab, by delta: Int) {
        guard canMoveTab(tab, by: delta), let i = index(tab) else { return }
        tabs.swapAt(i, i + delta)
    }

    /// List reordering (Customize Tools): `List.onMove` offsets.
    public mutating func moveTabs(fromOffsets from: IndexSet, toOffset to: Int) {
        tabs = Self.moved(tabs, from: from, to: to)
    }

    static func moved<T>(_ xs: [T], from: IndexSet, to: Int) -> [T] {
        let moving = from.filter(xs.indices.contains).map { xs[$0] }
        var rest = xs.enumerated().filter { !from.contains($0.offset) }.map(\.element)
        rest.insert(contentsOf: moving, at: min(to - from.count { $0 < to }, rest.count))
        return rest
    }

    /// Built-in tabs back in their default order; custom tabs after them.
    public mutating func resetTabOrder() {
        let rank = { (t: Tab) in ToolTab.builtIns.firstIndex(of: t.id) ?? ToolTab.builtIns.count }
        tabs = tabs.enumerated().sorted { (rank($0.element), $0.offset) < (rank($1.element), $1.offset) }.map(\.element)
    }

    /// A new, empty custom tab after the selected one (or at the end).
    @discardableResult
    public mutating func addCustomTab(title: String, symbol: String? = nil, after: ToolTab? = nil) -> ToolTab {
        let id = ToolTab.newCustom()
        let name = Self.cleanTitle(title) ?? "Custom"
        let tab = Tab(id: id, title: name, symbol: Self.cleanSymbol(symbol) ?? id.symbol, panels: [])
        tabs.insert(tab, at: after.flatMap(index).map { $0 + 1 } ?? tabs.count)
        return id
    }

    /// Put a hidden built-in tab back near its default place, with `panels`
    /// (default: its built-in tools).
    public mutating func addBuiltInTab(_ tab: ToolTab, panels: [ToolPanelID]? = nil) {
        guard !tab.isCustom, !contains(tab), let rank = ToolTab.builtIns.firstIndex(of: tab) else { return }
        let next = ToolTab.builtIns[(rank + 1)...].lazy.compactMap(index).first
        tabs.insert(Tab(id: tab, panels: Self.unique(panels ?? tab.panels)), at: next ?? tabs.count)
    }

    public func canRemoveTab(_ tab: ToolTab) -> Bool { contains(tab) && tabs.count > 1 }

    /// Remove a tab (never the last one). Its tools stay reachable from the Tools menu.
    public mutating func removeTab(_ tab: ToolTab) {
        guard canRemoveTab(tab), let i = index(tab) else { return }
        tabs.remove(at: i)
    }

    /// Rename; an empty name restores the built-in one ("Custom" for a custom tab).
    public mutating func rename(_ tab: ToolTab, to title: String) {
        guard let i = index(tab) else { return }
        let t = Self.cleanTitle(title)
        tabs[i].title = tab.isCustom ? (t ?? "Custom") : (t == tab.title ? nil : t)
    }

    public mutating func setSymbol(_ tab: ToolTab, _ symbol: String) {
        guard let i = index(tab), let s = Self.cleanSymbol(symbol) else { return }
        tabs[i].symbol = !tab.isCustom && s == tab.symbol ? nil : s
    }

    static func cleanTitle(_ s: String?) -> String? {
        let t = (s ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        return t.isEmpty ? nil : String(t.prefix(40))
    }

    /// Only the curated symbols (or a built-in tab's own) are accepted.
    static func cleanSymbol(_ s: String?) -> String? {
        guard let s, ToolTab.customSymbols.contains(s) || ToolTab.builtIns.contains(where: { $0.symbol == s }) else { return nil }
        return s
    }

    // MARK: Tools

    /// Add `panel` to `tab` before `target` (`nil` = at the end); if it is already
    /// there, move it (drag to reorder).
    public mutating func insert(_ panel: ToolPanelID, in tab: ToolTab, before target: ToolPanelID? = nil) {
        guard let i = index(tab), panel != target else { return }
        var ps = tabs[i].panels.filter { $0 != panel }
        ps.insert(panel, at: target.flatMap { ps.firstIndex(of: $0) } ?? ps.count)
        tabs[i].panels = ps
    }

    public mutating func remove(_ panel: ToolPanelID, from tab: ToolTab) {
        guard let i = index(tab) else { return }
        tabs[i].panels.removeAll { $0 == panel }
    }

    /// List reordering of one tab's tools (Customize Tools).
    public mutating func movePanels(in tab: ToolTab, fromOffsets from: IndexSet, toOffset to: Int) {
        guard let i = index(tab) else { return }
        tabs[i].panels = Self.moved(tabs[i].panels, from: from, to: to)
    }

    public func canMove(_ panel: ToolPanelID, in tab: ToolTab, by delta: Int) -> Bool {
        guard let j = panels(for: tab).firstIndex(of: panel) else { return false }
        return delta != 0 && panels(for: tab).indices.contains(j + delta)
    }

    /// Move a tool up (-1) or down (+1) within its tab.
    public mutating func move(_ panel: ToolPanelID, in tab: ToolTab, by delta: Int) {
        guard canMove(panel, in: tab, by: delta), let i = index(tab), let j = tabs[i].panels.firstIndex(of: panel) else { return }
        tabs[i].panels.swapAt(j, j + delta)
    }

    /// Move a tool to another tab (to its end; kept in place if it is already there).
    public mutating func move(_ panel: ToolPanelID, from tab: ToolTab, to other: ToolTab) {
        guard tab != other, contains(other) else { return }
        remove(panel, from: tab)
        if !panels(for: other).contains(panel) { insert(panel, in: other) }
    }

    /// Never unreachable: the tab to show `panel` in. `preferring` (the current
    /// tab) if it holds the tool, else the first tab that does; else the tool goes
    /// back to its home tab (put back in the strip, holding just this tool, if it
    /// was removed).
    public mutating func ensureReachable(_ panel: ToolPanelID, preferring: ToolTab? = nil) -> ToolTab {
        if let p = preferring, panels(for: p).contains(panel) { return p }
        if let t = tabs(containing: panel).first { return t }
        let home = panel.homeTab
        if contains(home) { insert(panel, in: home) } else { addBuiltInTab(home, panels: [panel]) }
        return home
    }

    /// Tab titles for menus, with repeated names numbered ("Portraits", "Portraits 2").
    public var menuTitles: [ToolTab: String] {
        var seen: [String: Int] = [:]
        return Dictionary(uniqueKeysWithValues: tabs.map { t in
            let name = title(of: t.id)
            let n = seen[name.lowercased(), default: 0] + 1
            seen[name.lowercased()] = n
            return (t.id, n == 1 ? name : "\(name) \(n)")
        })
    }

    /// Show or hide the All Tools tab ("all tools stacked in one tab").
    public mutating func toggleAllToolsTab() {
        if contains(.allTools) { removeTab(.allTools) } else { addBuiltInTab(.allTools) }
    }
}

// MARK: Persistence

extension ToolWorkspace: Codable {
    private struct StoredTab: Codable {
        var id: String
        var title: String?
        var symbol: String?
        var panels: [String]
    }

    private enum Keys: String, CodingKey { case tabs, known, knownTabs, collapsed }

    /// Stored ids are sanitized: unknown tabs and tools are dropped and duplicates
    /// removed. What this build added since the layout was saved reaches the user:
    /// new tabs (not in `knownTabs`) are put in the strip, and new tools (not in
    /// `known`) are appended to their home tab and the All Tools tab, when in the strip.
    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let stored = try c.decode([StoredTab].self, forKey: .tabs)
        let known = try c.decodeIfPresent([String].self, forKey: .known)
        var seen = Set<ToolTab>()
        tabs = stored.compactMap { s in
            guard let id = ToolTab(rawValue: s.id), seen.insert(id).inserted else { return nil }
            let title = Self.cleanTitle(s.title)
            return Tab(id: id, title: id.isCustom ? title ?? "Custom" : title, symbol: Self.cleanSymbol(s.symbol),
                       panels: Self.unique(s.panels.compactMap(ToolPanelID.init(rawValue:))))
        }
        if tabs.isEmpty { tabs = Self.default.tabs }
        if let knownTabs = try c.decodeIfPresent([String].self, forKey: .knownTabs) {
            for t in ToolTab.allCases where !knownTabs.contains(t.rawValue) { addBuiltInTab(t) }
        }
        if let known {
            for p in ToolPanelID.allCases where !known.contains(p.rawValue) {
                for t in [p.homeTab, .allTools] {
                    if let i = index(t), !tabs[i].panels.contains(p) { tabs[i].panels.append(p) }
                }
            }
        }
        collapsed = try c.decodeIfPresent([String].self, forKey: .collapsed).map { Set($0.compactMap(ToolPanelID.init(rawValue:))) }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        try c.encode(tabs.map { StoredTab(id: $0.id.rawValue, title: $0.title, symbol: $0.symbol, panels: $0.panels.map(\.rawValue)) },
                     forKey: .tabs)
        try c.encode(ToolPanelID.allCases.map(\.rawValue), forKey: .known)
        try c.encode(ToolTab.builtIns.map(\.rawValue), forKey: .knownTabs)
        try c.encodeIfPresent(collapsed.map { $0.map(\.rawValue).sorted() }, forKey: .collapsed)
    }
}
