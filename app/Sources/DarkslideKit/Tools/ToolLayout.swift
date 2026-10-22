import Foundation
import Observation

/// The user's tool workspace (`ToolWorkspace`) and their named workspaces,
/// persisted in `UserDefaults` (a suite is injected in tests).
@MainActor @Observable
public final class ToolLayout {
    /// Before workspaces: the tab order and the Favorites list (read once to migrate).
    public static let orderKey = "toolTabOrder"
    public static let favoritesKey = "favoritePanels"
    public static let workspaceKey = "toolWorkspace"
    public static let savedWorkspacesKey = "toolWorkspaces"
    /// Stored data that could not be read (wholly or partly) is kept here before it is overwritten.
    public static let backupSuffix = ".unreadable"
    /// The built-in workspace's name (reserved: not usable for a saved one).
    public static let defaultWorkspaceName = "Default"

    /// The app's layout, in the app's preferences (`AppModel.appDefaults`, M4).
    public static let shared = ToolLayout(defaults: AppModel.appDefaults)

    @ObservationIgnored private let defaults: UserDefaults
    var defaultsForTesting: UserDefaults { defaults }

    public private(set) var workspace: ToolWorkspace
    /// Named workspaces, in the order saved.
    public private(set) var saved: [SavedWorkspace]
    /// The tool the sidebar should scroll to (Tools menu reveal). Each request is
    /// new (a serial), so revealing the same tool twice scrolls twice.
    public private(set) var revealRequest: RevealRequest?
    @ObservationIgnored private var revealSerial = 0
    /// A field the Customize Tools window should focus when it opens or is brought forward.
    public var customizeFocus: CustomizeFocus?
    /// The layout before the last workspace load or reset (Tools ▸ Workspaces ▸
    /// Undo Workspace Change), with its collapsed panels. Any other edit clears it.
    public private(set) var undoWorkspace: ToolWorkspace?

    public struct RevealRequest: Equatable, Sendable {
        public let panel: ToolPanelID
        let serial: Int
    }

    public enum CustomizeFocus: Sendable { case tabName, workspaceName }

    public struct SavedWorkspace: Codable, Equatable, Sendable {
        public var name: String
        public var workspace: ToolWorkspace
    }

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        let data = defaults.data(forKey: Self.workspaceKey)
        if let data, let w = try? JSONDecoder().decode(ToolWorkspace.self, from: data) {
            workspace = w
        } else {
            if let data { defaults.set(data, forKey: Self.workspaceKey + Self.backupSuffix) }
            workspace = .migrated(order: defaults.stringArray(forKey: Self.orderKey),
                                  favorites: defaults.stringArray(forKey: Self.favoritesKey))
        }
        // Lossy per entry: one unreadable workspace doesn't cost the others.
        let savedData = defaults.data(forKey: Self.savedWorkspacesKey)
        let entries = savedData.flatMap { try? JSONDecoder().decode([Lossy].self, from: $0) }
        saved = entries?.compactMap(\.value) ?? []
        if let savedData, entries == nil || saved.count < entries!.count {
            defaults.set(savedData, forKey: Self.savedWorkspacesKey + Self.backupSuffix)
        }
    }

    private struct Lossy: Decodable {
        let value: SavedWorkspace?
        init(from decoder: any Decoder) throws { value = try? SavedWorkspace(from: decoder) }
    }

    /// Change the workspace; persisted when it changed.
    @discardableResult
    public func edit<R>(_ change: (inout ToolWorkspace) -> R) -> R {
        var w = workspace
        let r = change(&w)
        if w != workspace {
            workspace = w
            undoWorkspace = nil
            defaults.set(try? JSONEncoder().encode(w), forKey: Self.workspaceKey)
        }
        return r
    }

    /// Ask the sidebar to scroll to `panel`.
    public func requestReveal(_ panel: ToolPanelID) {
        revealSerial += 1
        revealRequest = RevealRequest(panel: panel, serial: revealSerial)
    }

    /// The sidebar handled `request` (a newer one stays pending).
    public func finishReveal(_ request: RevealRequest) {
        if revealRequest == request { revealRequest = nil }
    }

    // MARK: Tab strip (pre-workspace API)

    /// Tab order.
    public var order: [ToolTab] { workspace.order }
    /// Panels shown in the Favorites tab, in order.
    public var favorites: [ToolPanelID] { workspace.panels(for: .favorites) }

    /// Panels shown for `tab`.
    public func panels(for tab: ToolTab) -> [ToolPanelID] { workspace.panels(for: tab) }

    /// Move `tab` so it lands before `target` (drag in the strip); `nil` = to the end.
    public func move(_ tab: ToolTab, before target: ToolTab?) { edit { $0.moveTab(tab, before: target) } }

    /// Keyboard reordering (N17): whether `tab` can move `delta` places.
    public func canMove(_ tab: ToolTab, by delta: Int) -> Bool { workspace.canMoveTab(tab, by: delta) }

    /// Move `tab` left (-1) or right (+1) in the strip.
    public func move(_ tab: ToolTab, by delta: Int) { edit { $0.moveTab(tab, by: delta) } }

    public func resetOrder() { edit { $0.resetTabOrder() } }

    public func isFavorite(_ panel: ToolPanelID) -> Bool { favorites.contains(panel) }

    /// Add to or remove from Favorites (putting the Favorites tab back if it was removed).
    public func toggleFavorite(_ panel: ToolPanelID) {
        edit { w in
            if w.panels(for: .favorites).contains(panel) {
                w.remove(panel, from: .favorites)
            } else {
                if w.contains(.favorites) { w.insert(panel, in: .favorites) } else { w.addBuiltInTab(.favorites, panels: [panel]) }
            }
        }
    }

    // MARK: Workspaces

    static func cleanName(_ name: String) -> String? {
        let n = name.trimmingCharacters(in: .whitespacesAndNewlines)
        return n.isEmpty || n.caseInsensitiveCompare(defaultWorkspaceName) == .orderedSame ? nil : String(n.prefix(60))
    }

    /// The saved workspace called `name`, ignoring case.
    public func savedIndex(named name: String) -> Int? {
        saved.firstIndex { $0.name.caseInsensitiveCompare(name) == .orderedSame }
    }

    /// Save the current layout (and `collapsed`) under `name`, replacing one of
    /// the same name in any case. False for an empty or reserved name.
    @discardableResult
    public func saveWorkspace(named name: String, collapsed: Set<ToolPanelID>) -> Bool {
        guard let n = Self.cleanName(name) else { return false }
        var w = workspace
        w.collapsed = collapsed
        if let i = savedIndex(named: n) { saved[i] = .init(name: n, workspace: w) } else { saved.append(.init(name: n, workspace: w)) }
        persistSaved()
        return true
    }

    /// Make `name` the current layout, remembering the one it replaces (with
    /// `collapsed`, the current collapsed panels) for undo. Returns its collapsed
    /// panels, or nil if there is no such workspace.
    public func loadWorkspace(named name: String, collapsed: Set<ToolPanelID>) -> Set<ToolPanelID>? {
        guard let w = savedIndex(named: name).map({ saved[$0].workspace }) else { return nil }
        return replace(with: w, collapsed: collapsed)
    }

    /// The built-in layout (the Default workspace); undoable like a load.
    @discardableResult
    public func resetToDefault(collapsed: Set<ToolPanelID>) -> Set<ToolPanelID> {
        var w = ToolWorkspace.default
        w.collapsed = ToolWorkspace.defaultCollapsed
        return replace(with: w, collapsed: collapsed)
    }

    /// Back to the layout before the last load or reset; returns its collapsed panels.
    public func undoWorkspaceChange() -> Set<ToolPanelID>? {
        guard let w = undoWorkspace else { return nil }
        let collapsed = w.collapsed ?? ToolWorkspace.defaultCollapsed
        edit { $0 = w; $0.collapsed = nil }
        undoWorkspace = nil
        return collapsed
    }

    private func replace(with new: ToolWorkspace, collapsed: Set<ToolPanelID>) -> Set<ToolPanelID> {
        var before = workspace
        before.collapsed = collapsed
        let target = new.collapsed ?? ToolWorkspace.defaultCollapsed
        edit { $0 = new; $0.collapsed = nil }
        if before.tabs != workspace.tabs || collapsed != target { undoWorkspace = before }
        return target
    }

    /// Save `w` as the workspace `name` (replacing one of the same name in any case),
    /// leaving the current layout alone. Returns the name used; nil for an empty or
    /// reserved name.
    @discardableResult
    public func storeWorkspace(_ w: ToolWorkspace, named name: String) -> String? {
        guard let n = Self.cleanName(name) else { return nil }
        if let i = savedIndex(named: n) { saved[i] = .init(name: n, workspace: w) } else { saved.append(.init(name: n, workspace: w)) }
        persistSaved()
        return n
    }

    /// Make the change since `before` (with `collapsed`, its collapsed panels) one
    /// Undo Workspace Change step, like a load. Nothing when the tabs didn't change.
    public func rememberForUndo(_ before: ToolWorkspace, collapsed: Set<ToolPanelID>) {
        guard before.tabs != workspace.tabs else { return }
        var b = before
        b.collapsed = collapsed
        undoWorkspace = b
    }

    public func deleteWorkspace(named name: String) {
        saved.removeAll { $0.name == name }
        persistSaved()
    }

    private func persistSaved() {
        defaults.set(try? JSONEncoder().encode(saved), forKey: Self.savedWorkspacesKey)
    }
}
