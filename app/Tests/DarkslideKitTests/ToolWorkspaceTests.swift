import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@MainActor
@Suite("Tools: customizable workspace")
struct ToolWorkspaceTests {
    func layout() -> (ToolLayout, UserDefaults) {
        let d = testDefaults()
        return (ToolLayout(defaults: d), d)
    }

    func roundTrip(_ w: ToolWorkspace) throws -> ToolWorkspace {
        try JSONDecoder().decode(ToolWorkspace.self, from: JSONEncoder().encode(w))
    }

    @Test func tabIDs() {
        #expect(ToolTab(rawValue: "exposure") == .exposure && ToolTab(rawValue: "allTools") == .allTools)
        #expect(ToolTab(rawValue: "bogus") == nil && ToolTab(rawValue: "custom-") == nil)
        let c = ToolTab.newCustom()
        #expect(c.isCustom && ToolTab(rawValue: c.rawValue) == c && c != ToolTab.newCustom())
        #expect(c.title == "Custom" && c.panels.isEmpty && ToolTab.customSymbols.contains(c.symbol))
        #expect(!ToolTab.allCases.contains(.allTools) && ToolTab.builtIns.last == .allTools)
        #expect(Set(ToolTab.allTools.panels) == Set(ToolPanelID.allCases) && ToolTab.allTools.panels.count == ToolPanelID.allCases.count)
        #expect(ToolTab.allTools.title == "All Tools" && ToolTab.allTools.symbol != ToolTab.newCustom().symbol)
        for p in ToolPanelID.allCases {
            #expect(p.homeTab.panels.contains(p) && p.homeTab != .favorites, "\(p) has a home tab")
        }
        #expect(ToolPanelID.styles.homeTab == .color && ToolPanelID.whiteBalance.homeTab == .color)
        // Every built-in tab has its own name, symbol and tools (not the custom fallbacks).
        let fallback = ToolTab.newCustom()
        for t in ToolTab.builtIns {
            #expect(t.title != fallback.title && t.symbol != fallback.symbol && !t.panels.isEmpty && !t.isCustom, "\(t.rawValue)")
        }
        #expect(Set(ToolTab.builtIns.map(\.symbol)).count == ToolTab.builtIns.count, "distinct icons on the rail")
    }

    @Test func defaultIsTheBuiltInLayout() {
        let w = ToolWorkspace.default
        #expect(w.order == ToolTab.allCases && !w.contains(.allTools))
        #expect(ToolTab.allCases.allSatisfy { w.panels(for: $0) == $0.panels && w.title(of: $0) == $0.title })
        #expect(ToolPanelID.allCases.allSatisfy(w.containsTool))
        #expect(w.hiddenBuiltIns == [.allTools])
        #expect(w.tabs(containing: .whiteBalance) == [.favorites, .color], "the same tool in two tabs")
        #expect(w.panels(for: .allTools).isEmpty && w.resolve(.allTools) == .favorites && w.resolve(.lens) == .lens)
    }

    @Test func addRemoveReorderAndMoveTools() {
        var w = ToolWorkspace.default
        w.insert(.curve, in: .lens)
        #expect(w.panels(for: .lens) == [.lensCorrection, .vignette, .curve])
        #expect(w.panels(for: .exposure).contains(.curve), "still in Exposure: duplicates are allowed")
        w.insert(.curve, in: .lens, before: .lensCorrection)
        #expect(w.panels(for: .lens) == [.curve, .lensCorrection, .vignette], "inserting a present tool moves it")
        w.insert(.vignette, in: .lens, before: .vignette)
        #expect(w.panels(for: .lens) == [.curve, .lensCorrection, .vignette])
        #expect(!w.canMove(.curve, in: .lens, by: -1) && w.canMove(.curve, in: .lens, by: 1))
        #expect(!w.canMove(.curve, in: .lens, by: 0) && !w.canMove(.hdr, in: .lens, by: 1))
        w.move(.curve, in: .lens, by: 1)
        #expect(w.panels(for: .lens) == [.lensCorrection, .curve, .vignette])
        w.move(.curve, in: .lens, by: 5)
        #expect(w.panels(for: .lens) == [.lensCorrection, .curve, .vignette], "out of range: no-op")
        w.movePanels(in: .lens, fromOffsets: [2], toOffset: 0)
        #expect(w.panels(for: .lens) == [.vignette, .lensCorrection, .curve])
        w.remove(.curve, from: .lens)
        #expect(w.panels(for: .lens) == [.vignette, .lensCorrection])
        w.move(.vignette, from: .lens, to: .details)
        #expect(w.panels(for: .lens) == [.lensCorrection] && w.panels(for: .details).last == .vignette)
        w.move(.whiteBalance, from: .favorites, to: .color)
        #expect(!w.panels(for: .favorites).contains(.whiteBalance))
        #expect(w.panels(for: .color).filter { $0 == .whiteBalance }.count == 1, "already there: not duplicated")
        let before = w
        w.move(.exposure, from: .exposure, to: .allTools)
        w.move(.exposure, from: .exposure, to: .exposure)
        w.insert(.curve, in: .allTools)
        w.remove(.curve, from: .allTools)
        w.movePanels(in: .allTools, fromOffsets: [0], toOffset: 1)
        #expect(w == before, "a tab not on the rail is never edited")
    }

    @Test func tabsReorderRenameAndRemove() {
        var w = ToolWorkspace.default
        w.moveTab(.output, before: .favorites)
        #expect(w.order.first == .output)
        w.moveTab(.output, before: nil)
        #expect(w.order.last == .output)
        w.moveTab(.output, before: .output)
        w.moveTab(.allTools, before: .lens)
        #expect(w.order.last == .output && !w.contains(.allTools))
        #expect(!w.canMoveTab(.favorites, by: -1) && w.canMoveTab(.favorites, by: 1) && !w.canMoveTab(.allTools, by: 1))
        w.moveTab(.favorites, by: 1)
        #expect(Array(w.order.prefix(2)) == [.library, .favorites])
        w.moveTabs(fromOffsets: [0], toOffset: 2)
        #expect(Array(w.order.prefix(2)) == [.favorites, .library])
        w.resetTabOrder()
        #expect(w.order == ToolTab.allCases)

        w.rename(.exposure, to: "  Tone  ")
        #expect(w.title(of: .exposure) == "Tone")
        w.rename(.exposure, to: "")
        #expect(w.title(of: .exposure) == "Exposure" && w.tabs[w.index(.exposure)!].title == nil, "empty restores the name")
        w.rename(.exposure, to: "Exposure")
        #expect(w.tabs[w.index(.exposure)!].title == nil)
        w.rename(.exposure, to: String(repeating: "x", count: 100))
        #expect(w.title(of: .exposure).count == 40)
        w.setSymbol(.exposure, "bolt")
        #expect(w.symbol(of: .exposure) == "bolt")
        w.setSymbol(.exposure, "trash.slash.not.curated")
        #expect(w.symbol(of: .exposure) == "bolt", "only curated symbols")
        w.setSymbol(.exposure, "sun.max")
        #expect(w.tabs[w.index(.exposure)!].symbol == nil, "the built-in symbol is the default")

        w.removeTab(.lens)
        #expect(!w.contains(.lens) && w.hiddenBuiltIns == [.lens, .allTools])
        #expect(w.panels(for: .lens).isEmpty && w.title(of: .lens) == "Lens")
        w.addBuiltInTab(.lens)
        #expect(w.order.firstIndex(of: .lens) == ToolTab.allCases.firstIndex(of: .lens), "back in its place")
        #expect(w.panels(for: .lens) == ToolTab.lens.panels)
        w.addBuiltInTab(.lens)
        w.addBuiltInTab(ToolTab.newCustom())
        #expect(w.tabs.count == ToolTab.allCases.count)
        w.toggleAllToolsTab()
        #expect(w.order.last == .allTools && w.panels(for: .allTools).count == ToolPanelID.allCases.count)
        w.toggleAllToolsTab()
        #expect(!w.contains(.allTools))
        w.removeTab(.output)
        w.addBuiltInTab(.output)
        #expect(w.order.last == .output, "nothing after it on the rail: appended")

        // The last tab is never removed.
        var one = ToolWorkspace(tabs: [.init(id: .exposure, panels: [.exposure])])
        #expect(!one.canRemoveTab(.exposure))
        one.removeTab(.exposure)
        #expect(one.order == [.exposure])
    }

    @Test func customTabs() throws {
        var w = ToolWorkspace.default
        let c = w.addCustomTab(title: "  Portraits ", symbol: "person.crop.square", after: .favorites)
        #expect(c.isCustom && w.order[1] == c)
        #expect(w.title(of: c) == "Portraits" && w.symbol(of: c) == "person.crop.square" && w.panels(for: c).isEmpty)
        w.insert(.whiteBalance, in: c)
        w.insert(.exposure, in: c)
        #expect(w.panels(for: c) == [.whiteBalance, .exposure])
        w.rename(c, to: "")
        #expect(w.title(of: c) == "Custom", "a custom tab keeps a name")
        let d = w.addCustomTab(title: "", symbol: "not-a-symbol")
        #expect(w.order.last == d && w.title(of: d) == "Custom" && w.symbol(of: d) == d.symbol)
        #expect(!w.hiddenBuiltIns.contains(c))
        w.resetTabOrder()
        #expect(Array(w.order.suffix(2)) == [c, d], "custom tabs after the built-ins, in their order")
        let back = try roundTrip(w)
        #expect(back == w)
        w.removeTab(c)
        #expect(!w.contains(c) && !w.hiddenBuiltIns.contains(c))
    }

    @Test func neverUnreachable() {
        var w = ToolWorkspace.default
        #expect(w.ensureReachable(.whiteBalance, preferring: .color) == .color, "the current tab when it holds the tool")
        #expect(w.ensureReachable(.whiteBalance, preferring: .lens) == .favorites, "else the first tab holding it")
        // Removed from every tab: back in its home tab.
        for t in w.tabs(containing: .curve) { w.remove(.curve, from: t) }
        #expect(!w.containsTool(.curve))
        #expect(w.ensureReachable(.curve) == .exposure && w.panels(for: .exposure).last == .curve)
        // Home tab removed too: the tab comes back holding just that tool.
        w.removeTab(.lens)
        #expect(w.ensureReachable(.vignette, preferring: .exposure) == .lens && w.panels(for: .lens) == [.vignette])
        // Home tab back but without the tool (removed after re-adding).
        w.remove(.lensCorrection, from: .lens)
        #expect(w.ensureReachable(.lensCorrection) == .lens && w.panels(for: .lens).last == .lensCorrection)
        #expect(w.order.firstIndex(of: .lens) == ToolTab.allCases.firstIndex(of: .lens), "back in its place")
        // Every tool, from an almost empty workspace.
        var bare = ToolWorkspace(tabs: [.init(id: ToolTab.newCustom(), title: "Mine", panels: [])])
        for p in ToolPanelID.allCases {
            let t = bare.ensureReachable(p)
            #expect(bare.panels(for: t).contains(p), "\(p) reachable")
        }
        #expect(ToolPanelID.allCases.allSatisfy(bare.containsTool))
    }

    @Test func sanitizeOnDecode() throws {
        let json = """
        {"tabs":[{"id":"color","panels":["curve","curve","gone","whiteBalance"]},
                 {"id":"color","panels":["exposure"]},
                 {"id":"removedTab","panels":["exposure"]},
                 {"id":"custom-abc","title":"  ","symbol":"evil.symbol","panels":["levels"]},
                 {"id":"exposure","title":"Tone","symbol":"bolt","panels":["exposure"]}],
         "known":["curve","whiteBalance","exposure","levels","gone"],
         "collapsed":["curve","nope"]}
        """
        let w = try JSONDecoder().decode(ToolWorkspace.self, from: Data(json.utf8))
        let custom = ToolTab(rawValue: "custom-abc")!
        #expect(w.order == [.color, custom, .exposure], "unknown and duplicate tabs dropped")
        #expect(w.panels(for: .color).prefix(2) == [.curve, .whiteBalance], "unknown and duplicate tools dropped")
        #expect(w.title(of: custom) == "Custom" && w.symbol(of: custom) == custom.symbol)
        #expect(w.title(of: .exposure) == "Tone" && w.symbol(of: .exposure) == "bolt")
        // Tools this build added since (not in `known`) go to their home tab when it's on the rail.
        #expect(w.panels(for: .color).suffix(3) == [.colorBalance, .colorEditor, .history] && w.panels(for: .color).contains(.styles))
        #expect(w.panels(for: .exposure) == [.exposure, .hdr, .presence], "levels and curve were known (the user removed them)")
        #expect(!w.containsTool(.lensCorrection), "home tab not on the rail: left to the Tools menu")
        #expect(w.collapsed == [.curve])
        // New built-in tabs (not in `knownTabs`) reach upgraded users; new tools also go to All Tools.
        let upgraded = try JSONDecoder().decode(ToolWorkspace.self, from: Data("""
        {"tabs":[{"id":"exposure","panels":["exposure"]},{"id":"allTools","panels":["exposure"]}],
         "known":["exposure","whiteBalance"], "knownTabs":["favorites","exposure","allTools"]}
        """.utf8))
        #expect(upgraded.order == [.library, .importArchive, .color, .exposure, .lens, .composition, .details, .layers,
                                   .metadata, .output, .allTools], "unknown rail tabs added in place; Favorites was known (removed)")
        #expect(upgraded.panels(for: .lens) == ToolTab.lens.panels)
        #expect(upgraded.panels(for: .allTools).count == ToolPanelID.allCases.count - 1, "all but whiteBalance (known, removed)")
        #expect(upgraded.panels(for: .exposure) == [.exposure, .hdr, .levels, .curve, .presence])
        // No `known`: nothing is appended.
        let plain = try JSONDecoder().decode(ToolWorkspace.self, from: Data(#"{"tabs":[{"id":"lens","panels":[]}]}"#.utf8))
        #expect(plain.panels(for: .lens).isEmpty && plain.collapsed == nil)
        // No usable tab at all: the default.
        let empty = try JSONDecoder().decode(ToolWorkspace.self, from: Data(#"{"tabs":[{"id":"zzz","panels":[]}]}"#.utf8))
        #expect(empty == .default)
        // Encoding records what this build knows.
        let obj = try JSONSerialization.jsonObject(with: JSONEncoder().encode(ToolWorkspace.default)) as? [String: Any]
        #expect((obj?["known"] as? [String])?.count == ToolPanelID.allCases.count && obj?["collapsed"] == nil)
        #expect((obj?["knownTabs"] as? [String]) == ToolTab.builtIns.map(\.rawValue))
    }

    @Test func migratesOldKeys() {
        let w = ToolWorkspace.migrated(order: ["output", "output", "bogus", "allTools", "library"], favorites: ["curve", "nope", "curve"])
        #expect(Array(w.order.prefix(2)) == [.output, .library])
        #expect(Set(w.order) == Set(ToolTab.allCases) && w.order.count == ToolTab.allCases.count, "the old rail never had All Tools")
        #expect(w.panels(for: .favorites) == [.curve] && w.panels(for: .exposure) == ToolTab.exposure.panels)
        #expect(ToolWorkspace.migrated(order: nil, favorites: nil) == .default)

        let d = testDefaults()
        d.set(["metadata"], forKey: ToolLayout.orderKey)
        d.set(["levels"], forKey: ToolLayout.favoritesKey)
        let l = ToolLayout(defaults: d)
        #expect(l.order.first == .metadata && l.favorites == [.levels])
        l.edit { $0.insert(.curve, in: .favorites) }
        d.set(["output"], forKey: ToolLayout.orderKey)
        let r = ToolLayout(defaults: d)
        #expect(r.order.first == .metadata && r.favorites == [.levels, .curve], "once migrated, the workspace wins")
        d.set(Data("not json".utf8), forKey: ToolLayout.workspaceKey)
        #expect(ToolLayout(defaults: d).order.first == .output, "an unreadable workspace falls back to the old keys")
    }

    @Test func persistenceRoundTrip() {
        let (l, d) = layout()
        let c = l.edit { $0.addCustomTab(title: "Mine", symbol: "leaf") }
        l.edit { w in
            w.insert(.curve, in: c)
            w.insert(.hdr, in: c, before: .curve)
            w.removeTab(.metadata)
            w.rename(.lens, to: "Optics")
            w.toggleAllToolsTab()
        }
        l.move(.output, before: .favorites)
        let r = ToolLayout(defaults: d)
        #expect(r.workspace == l.workspace)
        #expect(r.panels(for: c) == [.hdr, .curve] && r.workspace.title(of: .lens) == "Optics")
        #expect(r.order.first == .output && !r.order.contains(.metadata) && r.order.contains(.allTools))
        // An edit that changes nothing writes nothing.
        d.removeObject(forKey: ToolLayout.workspaceKey)
        l.edit { $0.insert(.hdr, in: c, before: .curve) }
        #expect(d.data(forKey: ToolLayout.workspaceKey) == nil)
    }

    @Test func favoritesComeBackWithTheirTab() {
        let (l, _) = layout()
        l.edit { $0.removeTab(.favorites) }
        #expect(!l.isFavorite(.curve))
        l.toggleFavorite(.curve)
        #expect(l.order.first == .favorites && l.favorites == [.curve], "only the requested tool comes back")
    }

    @Test func workspacesSaveLoadDeleteReset() {
        let (l, d) = layout()
        let m = makeModel()
        l.edit { $0.insert(.curve, in: .lens) }
        m.collapsedPanels = [.lensCorrection]
        #expect(ToolWorkspaceCommands.saveWorkspace(named: " Retouch ", model: m, layout: l))
        #expect(!l.saveWorkspace(named: "  ", collapsed: []) && !l.saveWorkspace(named: "default", collapsed: []))
        #expect(l.saved.map(\.name) == ["Retouch"] && l.saved[0].workspace.collapsed == [.lensCorrection])

        l.edit { $0.toggleAllToolsTab() }
        m.collapsedPanels = []
        l.saveWorkspace(named: "All", collapsed: [])
        m.toolTab = .allTools

        ToolWorkspaceCommands.loadWorkspace(named: "Retouch", model: m, layout: l)
        #expect(l.panels(for: .lens).last == .curve && !l.workspace.contains(.allTools))
        #expect(m.collapsedPanels == [.lensCorrection] && m.toolTab == l.order[0], "selection moved off the missing tab")
        #expect(l.workspace.collapsed == nil, "the current layout does not carry collapsed states")
        #expect(m.toast?.message.contains("Undo Workspace Change") == true)
        // One level of undo: the layout (and collapsed panels) it replaced.
        ToolWorkspaceCommands.undoWorkspaceChange(model: m, layout: l)
        #expect(l.workspace.contains(.allTools) && m.collapsedPanels.isEmpty && l.undoWorkspace == nil)
        ToolWorkspaceCommands.undoWorkspaceChange(model: m, layout: l)
        #expect(l.workspace.contains(.allTools), "nothing more to undo")
        ToolWorkspaceCommands.loadWorkspace(named: "nope", model: m, layout: l)
        #expect(l.undoWorkspace == nil && l.workspace.contains(.allTools))
        // Any other edit drops the undo (it would discard that edit).
        ToolWorkspaceCommands.loadWorkspace(named: "retouch", model: m, layout: l)
        #expect(l.undoWorkspace != nil && l.panels(for: .lens).last == .curve, "names match ignoring case")
        l.edit { $0.insert(.hdr, in: .lens) }
        #expect(l.undoWorkspace == nil)
        // Loading what is already shown leaves nothing to undo (and says nothing).
        m.toast = nil
        m.collapsedPanels = [.lensCorrection]
        ToolWorkspaceCommands.saveWorkspace(named: "Same", model: m, layout: l)
        ToolWorkspaceCommands.loadWorkspace(named: "Same", model: m, layout: l)
        #expect(l.undoWorkspace == nil && m.toast == nil)

        // Same name in any case replaces, under the new spelling.
        m.collapsedPanels = [.hdr]
        ToolWorkspaceCommands.saveWorkspace(named: "RETOUCH", model: m, layout: l)
        #expect(l.saved.map(\.name) == ["RETOUCH", "All", "Same"] && l.saved[0].workspace.collapsed == [.hdr])

        let r = ToolLayout(defaults: d)
        #expect(r.saved == l.saved, "saved workspaces persist")

        m.toolTab = .lens
        ToolWorkspaceCommands.resetToDefault(model: m, layout: l)
        #expect(l.workspace == .default && m.collapsedPanels == ToolWorkspace.defaultCollapsed && m.toolTab == .lens)
        #expect(l.undoWorkspace?.collapsed == [.hdr])
        ToolWorkspaceCommands.undoWorkspaceChange(model: m, layout: l)
        #expect(l.panels(for: .lens).contains(.hdr) && m.collapsedPanels == [.hdr])

        l.deleteWorkspace(named: "RETOUCH")
        #expect(l.saved.map(\.name) == ["All", "Same"] && ToolLayout(defaults: d).saved.map(\.name) == ["All", "Same"])
        #expect(l.loadWorkspace(named: "RETOUCH", collapsed: []) == nil)
        // Loading returns the collapsed panels it was saved with.
        #expect(l.loadWorkspace(named: "All", collapsed: []) == [])
    }

    @Test func unreadableDataIsKeptAside() throws {
        let d = testDefaults()
        let good = try JSONEncoder().encode([ToolLayout.SavedWorkspace(name: "Good", workspace: .default)])
        var entries = try JSONSerialization.jsonObject(with: good) as! [Any]
        entries.append(["name": 7, "workspace": "broken"])
        let mixed = try JSONSerialization.data(withJSONObject: entries)
        d.set(mixed, forKey: ToolLayout.savedWorkspacesKey)
        d.set(Data("{oops".utf8), forKey: ToolLayout.workspaceKey)
        let l = ToolLayout(defaults: d)
        #expect(l.saved.map(\.name) == ["Good"], "one bad entry doesn't cost the others")
        #expect(d.data(forKey: ToolLayout.savedWorkspacesKey + ToolLayout.backupSuffix) == mixed)
        #expect(d.data(forKey: ToolLayout.workspaceKey + ToolLayout.backupSuffix) == Data("{oops".utf8))
        #expect(l.workspace == .default)
        // Readable data is never copied aside.
        let clean = testDefaults()
        clean.set(good, forKey: ToolLayout.savedWorkspacesKey)
        #expect(ToolLayout(defaults: clean).saved.count == 1 && clean.data(forKey: ToolLayout.savedWorkspacesKey + ToolLayout.backupSuffix) == nil)
        clean.set(Data("nope".utf8), forKey: ToolLayout.savedWorkspacesKey)
        #expect(ToolLayout(defaults: clean).saved.isEmpty && clean.data(forKey: ToolLayout.savedWorkspacesKey + ToolLayout.backupSuffix) != nil)
    }

    @Test func revealSwitchesTabExpandsAndScrolls() {
        let (l, _) = layout()
        let m = makeModel()
        m.toolTab = .lens
        m.collapsedPanels = [.curve, .styles]
        m.panelsHidden = true
        m.viewer.state.fullScreen = true
        ToolWorkspaceCommands.reveal(.curve, model: m, layout: l)
        #expect(m.toolTab == .exposure && !m.collapsedPanels.contains(.curve) && l.revealRequest?.panel == .curve)
        #expect(m.collapsedPanels.contains(.styles))
        #expect(!m.panelsHidden && !m.viewer.state.fullScreen, "the tool panels are shown")
        // Revealing the same tool again is a new request, even if the first was never handled.
        let first = l.revealRequest!
        ToolWorkspaceCommands.reveal(.curve, model: m, layout: l)
        let second = l.revealRequest!
        #expect(second != first && second.panel == .curve)
        l.finishReveal(first)
        #expect(l.revealRequest == second, "a stale request doesn't clear a newer one")
        l.finishReveal(second)
        #expect(l.revealRequest == nil)
        // Prefers the current tab when it already holds the tool.
        m.toolTab = .color
        ToolWorkspaceCommands.reveal(.whiteBalance, model: m, layout: l)
        #expect(m.toolTab == .color && l.revealRequest?.panel == .whiteBalance)
        // A removed tool is added back to its home tab.
        l.edit { $0.remove(.vignette, from: .lens) }
        #expect(!l.workspace.containsTool(.vignette))
        ToolWorkspaceCommands.reveal(.vignette, model: m, layout: l)
        #expect(m.toolTab == .lens && l.panels(for: .lens).last == .vignette)
        // A removed current tab: the sidebar shows the first tab.
        m.toolTab = .metadata
        ToolWorkspaceCommands.removeTab(.metadata, model: m, layout: l)
        #expect(m.toolTab == .output, "the neighbour is selected")
        m.toolTab = .metadata
        #expect(ToolWorkspaceCommands.selectedTab(model: m, layout: l) == l.order[0])
        // A tool that only the removed tab held: the tab comes back with just that tool.
        ToolWorkspaceCommands.reveal(.keywords, model: m, layout: l)
        #expect(m.toolTab == .metadata && l.panels(for: .metadata) == [.keywords])
    }

    @Test func coordinatorActions() {
        let (l, _) = layout()
        let m = makeModel()
        m.toolTab = .color
        let c = ToolWorkspaceCommands.newTab(title: "Mine", model: m, layout: l)
        #expect(m.toolTab == c && l.order[l.order.firstIndex(of: .color)! + 1] == c)
        m.collapsedPanels = [.hdr]
        ToolWorkspaceCommands.add(.hdr, to: c, model: m, layout: l)
        ToolWorkspaceCommands.add(.levels, to: c, before: .hdr, model: m, layout: l)
        #expect(l.panels(for: c) == [.levels, .hdr] && m.collapsedPanels.isEmpty && l.revealRequest?.panel == .levels)
        // Dropped on a rail icon: added; on its own tab's icon (or a tab not on the rail): nothing.
        #expect(ToolWorkspaceCommands.drop(.curve, onTab: c, model: m, layout: l) && l.panels(for: c) == [.levels, .hdr, .curve])
        #expect(!ToolWorkspaceCommands.drop(.levels, onTab: c, model: m, layout: l) && l.panels(for: c) == [.levels, .hdr, .curve])
        #expect(!ToolWorkspaceCommands.drop(.curve, onTab: .allTools, model: m, layout: l) && !l.workspace.contains(.allTools))
        l.edit { $0.remove(.curve, from: c) }
        ToolWorkspaceCommands.move(.levels, from: c, to: .lens, model: m, layout: l)
        #expect(l.panels(for: c) == [.hdr] && l.panels(for: .lens).last == .levels && m.toolTab == .lens)
        // Removing an unselected tab keeps the selection; the last tab can't be removed.
        ToolWorkspaceCommands.removeTab(c, model: m, layout: l)
        #expect(m.toolTab == .lens && !l.order.contains(c))
        let last = l.order.last!
        m.toolTab = last
        ToolWorkspaceCommands.removeTab(last, model: m, layout: l)
        #expect(m.toolTab == l.order.last!, "the last tab removed: its predecessor")
        // All Tools tab, through the menu command.
        #expect(CommandCatalog.isEnabled(.toggleAllToolsTab, model: m, layout: l))
        CommandCatalog.perform(.toggleAllToolsTab, model: m, layout: l)
        #expect(l.order.last == .allTools && m.toolTab == .allTools)
        CommandCatalog.perform(.toggleAllToolsTab, model: m, layout: l)
        #expect(!l.order.contains(.allTools) && m.toolTab != .allTools)
        ToolWorkspaceCommands.showTab(.allTools, model: m, layout: l)
        for t in l.order where t != .allTools { l.edit { $0.removeTab(t) } }
        #expect(l.order == [.allTools] && !CommandCatalog.isEnabled(.toggleAllToolsTab, model: m, layout: l), "never the last tab")
        // Customize Tools… is opened by the view.
        #expect(CommandCatalog.perform(.customizeTools, model: m, layout: l) == .openCustomizeTools)
        #expect(CommandCatalog.isEnabled(.customizeTools, model: m, layout: l))
        #expect(CommandCatalog.title(.customizeTools, model: m) == "Customize Tools…")
    }

    @Test func menuSectionsListEveryToolOnce() {
        let all = ToolWorkspaceCommands.menuSections.flatMap(\.panels)
        #expect(all.count == ToolPanelID.allCases.count && Set(all) == Set(ToolPanelID.allCases))
        #expect(!ToolWorkspaceCommands.menuSections.map(\.tab).contains(.favorites))
    }

    @Test func dragItemsUseTheirOwnTypes() async throws {
        let panel = ToolDragItem.panel(.curve), tab = ToolDragItem.tab(.lens)
        #expect(panel.exportedContentTypes() == [.toolPanel] && tab.exportedContentTypes() == [.toolTab])
        #expect(Set(ToolDragItem.importedContentTypes()) == [.toolPanel, .toolTab])
        #expect(!ToolDragItem.importedContentTypes().contains { $0.conforms(to: .text) || $0.conforms(to: .fileURL) },
                "text and Finder drops are left to other targets")
        #expect(try await ToolDragItem(importing: panel.exported(as: .toolPanel), contentType: .toolPanel) == panel)
        #expect(try await ToolDragItem(importing: tab.exported(as: .toolTab), contentType: .toolTab) == tab)
        await #expect(throws: (any Error).self) { try await ToolDragItem(importing: Data("nope".utf8), contentType: .toolPanel) }
        await #expect(throws: (any Error).self) { try await ToolDragItem(importing: Data("nope".utf8), contentType: .toolTab) }
        #expect(panel.panel == .curve && panel.tab == nil && tab.tab == .lens && tab.panel == nil)
    }

    @Test func tabNameEditsCommitOnlyRealChanges() {
        let (l, _) = layout()
        let m = makeModel()
        var edit = TabNameEdit()
        // Focus, type, commit: renamed.
        edit.begin(.lens, title: l.workspace.title(of: .lens))
        let r = edit.end("Optics")
        #expect(r?.tab == .lens && r?.name == "Optics" && edit.tab == nil)
        // Focus and leave without typing: nothing to write.
        edit.begin(.lens, title: "Lens")
        #expect(edit.end("Lens") == nil && edit.tab == nil)
        #expect(edit.end("Anything") == nil, "no edit in progress")

        // N1: the field is focused, then a footer load/reset replaces the layout.
        l.edit { $0.rename(.color, to: "Colour") }
        ToolWorkspaceCommands.saveWorkspace(named: "Mine", model: m, layout: l)
        edit.begin(.color, title: l.workspace.title(of: .color))
        let shown = l.workspace.title(of: .color)
        ToolWorkspaceCommands.resetToDefault(model: m, layout: l)
        #expect(l.undoWorkspace != nil)
        // The view drops the edit when the workspace changes; a blur then writes nothing.
        edit.cancel()
        if let stale = edit.end(shown) { l.edit { $0.rename(stale.tab, to: stale.name) } }
        #expect(l.workspace == .default, "the stale name is not written into the new layout")
        #expect(l.undoWorkspace != nil, "so the toast's Undo still works")
        // Without the cancel, an unchanged text still writes nothing.
        edit.begin(.color, title: "Color")
        #expect(edit.end("Color") == nil)
    }

    @Test func menuTitlesNumberRepeats() {
        var w = ToolWorkspace.default
        let a = w.addCustomTab(title: "Portraits"), b = w.addCustomTab(title: "portraits")
        w.rename(.lens, to: "Portraits")
        let t = w.menuTitles
        #expect(t[.lens] == "Portraits" && t[a] == "Portraits 2" && t[b] == "portraits 3" && t[.color] == "Color")
        #expect(t.count == w.tabs.count)
    }

    @Test func mainWindowIdentifiers() {
        #expect(AppWindows.isMain("main") && AppWindows.isMain("main-AppWindow-1"))
        #expect(!AppWindows.isMain(nil) && !AppWindows.isMain("customize-tools-AppWindow-1") && !AppWindows.isMain("mainly"))
    }

    @Test func accessibilityLabels() {
        let c = ToolStripControl.tab(.exposure, title: "Tone", selected: false)
        #expect(c.label == "Tone tools" && c.hint == "Show the Tone panels")
        let a = ToolStripControl.arrange(.curve, tabTitle: "Exposure")
        #expect(a.map(\.label) == ["Move Up", "Move Down", "Remove from Exposure"] && Set(a.map(\.id)).count == 3)
        #expect(a.allSatisfy { !($0.hint ?? "").isEmpty })
    }
}
