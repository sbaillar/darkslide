import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@MainActor
@Suite("Tools: Capture One–style tab strip")
struct ToolStripTests {
    let tabs = ToolTab.allCases // Favorites first, then ten more

    /// Width every tab needs, with the + button.
    var fullWidth: Double {
        Double(tabs.count) * (ToolStrip.iconWidth + ToolStrip.spacing) + ToolStrip.dividerWidth + ToolStrip.buttonWidth
    }

    @Test func everythingFitsWhenThereIsRoom() {
        #expect(fullWidth == 367)
        for w in [fullWidth, 1000, .infinity] {
            let s = ToolStrip.split(tabs, selected: .color, width: w)
            #expect(s.visible == tabs && s.overflow.isEmpty, "\(w)")
        }
    }

    @Test func narrowStripsOverflowInOrder() {
        // One point short: the » button takes room too, so only the last tab goes.
        let s = ToolStrip.split(tabs, selected: .color, width: fullWidth - 1)
        #expect(s.visible == Array(tabs.dropLast()) && s.overflow == [.output])
        // 200 pt: Favorites (with its divider) and three more fit.
        let n = ToolStrip.split(tabs, selected: .library, width: 200)
        #expect(n.visible == [.favorites, .library, .importArchive, .color])
        #expect(n.overflow == Array(tabs.dropFirst(4)))
        #expect(Set(n.visible + n.overflow) == Set(tabs) && n.visible.count + n.overflow.count == tabs.count)
    }

    @Test func theSelectedTabAlwaysShows() {
        let s = ToolStrip.split(tabs, selected: .output, width: 200)
        #expect(s.visible == [.favorites, .library, .importArchive, .output], "room kept for it, in its own place")
        #expect(s.overflow == [.color, .exposure, .lens, .composition, .details, .layers, .metadata])
        // Too narrow for anything: still the selected tab.
        let tiny = ToolStrip.split(tabs, selected: .lens, width: 10)
        #expect(tiny.visible == [.lens] && tiny.overflow.count == tabs.count - 1 && !tiny.overflow.contains(.lens))
        // A selection not in the strip (stale) is not added.
        let stale = ToolStrip.split([.color, .lens], selected: .allTools, width: 10)
        #expect(stale.visible.isEmpty && stale.overflow == [.color, .lens])
    }

    @Test func theSelectedTabKeepsItsPlace() {
        // Selected in the middle: the tabs after the first that doesn't fit overflow, it stays.
        let s = ToolStrip.split(tabs, selected: .lens, width: 200)
        #expect(s.visible == [.favorites, .library, .importArchive, .lens])
        #expect(s.overflow == [.color, .exposure, .composition, .details, .layers, .metadata, .output])
        // Moving it right moves it on screen too.
        var moved = tabs
        moved.swapAt(tabs.firstIndex(of: .lens)!, tabs.firstIndex(of: .composition)!)
        #expect(ToolStrip.split(moved, selected: .lens, width: 200).visible == [.favorites, .library, .importArchive, .lens])
        let m = ToolStrip.split(moved, selected: .lens, width: 1000)
        #expect(m.visible.firstIndex(of: .lens)! > m.visible.firstIndex(of: .composition)!)
    }

    /// Review MED-1: Favorites (wider, with its divider) moved last and selected.
    @Test func visibleTabsAlwaysFitTheRealPaneWidths() {
        var orders = [tabs, Array(tabs.dropFirst()) + [.favorites]]
        orders.append([.color, .favorites] + tabs.filter { $0 != .color && $0 != .favorites })
        for order in orders {
            for selected in order {
                for w in stride(from: 244.0, through: 464, by: 1) {
                    let s = ToolStrip.split(order, selected: selected, width: w)
                    #expect(ToolStrip.usedWidth(s) <= w, "\(selected) at \(w): \(ToolStrip.usedWidth(s))")
                    #expect(s.visible.contains(selected) && s.visible == order.filter(s.visible.contains), "in order")
                    #expect(s.visible.count + s.overflow.count == order.count)
                }
            }
        }
    }

    @Test func overflowedAIChangesShowOnTheMoreButton() {
        let l = ToolLayout(defaults: testDefaults())
        let s = ToolStrip.split(l.order, selected: .color, width: 304) // the default pane
        #expect(s.overflow.contains(.layers))
        #expect(ToolStrip.hasAIChanges(s.overflow, panels: l.panels(for:), changed: [.layers]))
        #expect(!ToolStrip.hasAIChanges(s.overflow, panels: l.panels(for:), changed: [.whiteBalance]))
        #expect(!ToolStrip.hasAIChanges([], panels: l.panels(for:), changed: [.layers]))
    }

    @Test func withoutFavoritesThereIsNoDivider() {
        let rest = Array(tabs.dropFirst())
        let w = Double(rest.count) * (ToolStrip.iconWidth + ToolStrip.spacing) + ToolStrip.buttonWidth
        #expect(ToolStrip.split(rest, selected: .color, width: w).overflow.isEmpty)
        #expect(!ToolStrip.split(rest, selected: .color, width: w - 1).overflow.isEmpty)
    }

    @Test func addToolMenuToggles() {
        let l = ToolLayout(defaults: testDefaults())
        let m = makeModel()
        let tab = ToolWorkspaceCommands.newTab(title: "Mine", model: m, layout: l)
        // Unchecked: added at the end, selected, expanded and scrolled to.
        m.collapsedPanels = [.curve]
        ToolWorkspaceCommands.toggle(.exposure, in: tab, model: m, layout: l)
        ToolWorkspaceCommands.toggle(.curve, in: tab, model: m, layout: l)
        #expect(l.panels(for: tab) == [.exposure, .curve] && m.toolTab == tab)
        #expect(m.collapsedPanels.isEmpty && l.revealRequest?.panel == .curve)
        // From a panel's menu: after that panel.
        ToolWorkspaceCommands.toggle(.levels, in: tab, after: .exposure, model: m, layout: l)
        ToolWorkspaceCommands.toggle(.hdr, in: tab, after: .curve, model: m, layout: l)
        #expect(l.panels(for: tab) == [.exposure, .levels, .curve, .hdr])
        // Checked: removed, in one choice; other tabs keep it.
        ToolWorkspaceCommands.toggle(.levels, in: tab, model: m, layout: l)
        ToolWorkspaceCommands.toggle(.exposure, in: tab, after: .curve, model: m, layout: l)
        #expect(l.panels(for: tab) == [.curve, .hdr] && l.panels(for: .exposure).contains(.exposure))
        // And back again.
        ToolWorkspaceCommands.toggle(.exposure, in: tab, model: m, layout: l)
        #expect(l.panels(for: tab) == [.curve, .hdr, .exposure])
        // Removing the last tool leaves an empty tab (its Add Tool button shows).
        for p in l.panels(for: tab) { ToolWorkspaceCommands.toggle(p, in: tab, model: m, layout: l) }
        #expect(l.panels(for: tab).isEmpty && l.workspace.contains(tab))
    }

    @Test func togglesAreOneUndoStep() {
        let l = ToolLayout(defaults: testDefaults())
        let m = makeModel()
        let start = l.workspace
        m.collapsedPanels = [.curve]
        ToolWorkspaceCommands.toggle(.curve, in: .lens, model: m, layout: l)
        #expect(l.panels(for: .lens).last == .curve && l.undoWorkspace != nil && m.collapsedPanels.isEmpty)
        ToolWorkspaceCommands.undoWorkspaceChange(model: m, layout: l)
        #expect(l.workspace == start && m.collapsedPanels == [.curve] && l.undoWorkspace == nil)
        // A removal too; the next toggle replaces the step.
        ToolWorkspaceCommands.toggle(.vignette, in: .lens, model: m, layout: l)
        ToolWorkspaceCommands.toggle(.lensCorrection, in: .lens, model: m, layout: l)
        #expect(l.panels(for: .lens).isEmpty)
        ToolWorkspaceCommands.undoWorkspaceChange(model: m, layout: l)
        #expect(l.panels(for: .lens) == [.lensCorrection])
        // Nothing changed (a tab not in the strip): no step.
        ToolWorkspaceCommands.toggle(.curve, in: .allTools, model: m, layout: l)
        #expect(l.undoWorkspace == nil)
    }

    @Test func togglingInFavorites() {
        let l = ToolLayout(defaults: testDefaults())
        let m = makeModel()
        #expect(l.isFavorite(.exposure) && !l.isFavorite(.curve))
        ToolWorkspaceCommands.toggle(.exposure, in: .favorites, model: m, layout: l)
        ToolWorkspaceCommands.toggle(.curve, in: .favorites, after: .whiteBalance, model: m, layout: l)
        #expect(!l.isFavorite(.exposure) && l.isFavorite(.curve), "the ★ on other tabs follows")
        #expect(l.favorites == [.whiteBalance, .curve, .styles, .history] && m.toolTab == .favorites)
        #expect(l.panels(for: .exposure).contains(.exposure), "only Favorites lost it")
    }

    @Test func labels() {
        let add = ToolStripControl.addTool(tabTitle: "Portraits")
        #expect(add.label == "Add or remove tools" && add.hint?.contains("Portraits") == true)
        #expect(ToolStripControl.moreTabs(1).hint == "1 more tab" && ToolStripControl.moreTabs(3).hint == "3 more tabs")
        let menu = ToolStripControl.panelMenu(.curve)
        #expect(menu.label == "Curve options" && menu.id == "panelmenu-curve")
        let m = makeModel()
        #expect(CommandCatalog.title(.moveTabUp, model: m) == "Move Tool Tab Left")
        #expect(CommandCatalog.title(.moveTabDown, model: m) == "Move Tool Tab Right")
    }
}
