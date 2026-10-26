import AppKit
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// N17 audit: every data-driven control list the views render has a non-empty,
/// unique label in every state the views can be in.
@MainActor
@Suite struct AccessibilityAuditTests {
    func assertLabelled(_ cs: [AccessibleControl], _ context: String) {
        for c in cs {
            #expect(!c.label.trimmingCharacters(in: .whitespaces).isEmpty, "\(context): \(c.id) has no label")
            if let h = c.hint { #expect(!h.isEmpty, "\(context): \(c.id) has an empty hint") }
        }
        #expect(Set(cs.map(\.id)).count == cs.count, "\(context): duplicate ids")
    }

    @Test func toolbarInEveryState() async {
        for mode in [AppMode.demo, .live] {
            let m = makeModel()
            if mode == .live {
                let live = AppModel(sessions: MockSessionStore(counts: [5]), images: MockImageProvider(latency: .zero),
                                    renderer: FakeRenderer(), importer: ReadOnlyCardScanner(),
                                    archive: UnavailableArchiveService(target: ""), ai: MockAIService(),
                                    settings: makeSettings(), mode: .live)
                live.watchCards = false
                let cs = ToolbarControl.allCases.map { $0.control(live) }
                assertLabelled(cs, "live toolbar")
                #expect(cs.first { $0.id == "importCard" }?.label.contains("coming soon") == true)
                continue
            }
            await m.start()
            assertLabelled(ToolbarControl.allCases.map { $0.control(m) }, "empty history")
            m.setRating(3)
            m.copyAdjustments()
            m.viewer.togglePinCompare()
            let cs = ToolbarControl.allCases.map { $0.control(m) }
            assertLabelled(cs, "busy")
            #expect(cs.first { $0.id == "undo" }?.label == "Undo Rate 3")
            #expect(cs.first { $0.id == "compare" }?.label == "Stop comparing")
            #expect(cs.first { $0.id == "apply" }?.hint?.contains("⇧⌘V") == true)
            #expect(cs.first { $0.id == "session" }?.label.hasPrefix("Session: ") == true)
        }
    }

    @Test func aiCullLabelShowsProgress() async {
        let m = makeModel()
        await m.start()
        #expect(ToolbarControl.aiCull.control(m).label == "AI Cull")
    }

    @Test func filterBarControlsInEveryState() {
        let states: [(PhotoFilter, Int, Bool)] = [
            (PhotoFilter(), 0, false), (PhotoFilter(), 3, true), (PhotoFilter(minRating: 3), 0, false),
            ({ var f = PhotoFilter(); f.missingOnly = true; return f }(), 2, false),
        ]
        for (f, missing, compact) in states {
            let items = FilterBarControl.all(filter: f, missingCount: missing, compact: compact)
            assertLabelled(items.map { $0.control(filter: f, missingCount: missing, sort: .rating) }, "filter \(f)")
            #expect(items.contains(.missing) == (missing > 0 || f.missingOnly))
            #expect(items.contains(.clear) == f.isActive)
            #expect(items.contains(.search) == !compact)
            #expect(items.filter { if case .tag = $0 { true } else { false } }.count == 7, "all 7 color labels")
        }
        let m = FilterBarControl.missing.control(filter: PhotoFilter(), missingCount: 12, sort: .captureTime)
        #expect(m.label == "Missing (12)")
        #expect(FilterBarControl.sort.control(filter: PhotoFilter(), missingCount: 0, sort: .filename).label == "Sort by Filename")
        #expect(FilterBarControl.star(3).control(filter: PhotoFilter(minRating: 3), missingCount: 0, sort: .rating).hint?
            .contains("clear") == true)
    }

    @Test func filterBarActions() {
        var f = PhotoFilter()
        FilterBarControl.star(4).apply(to: &f)
        #expect(f.minRating == 4)
        FilterBarControl.unrated.apply(to: &f)
        #expect(f.unratedOnly && f.minRating == 0)
        FilterBarControl.pick(.rejected).apply(to: &f)
        FilterBarControl.tag(.pink).apply(to: &f)
        FilterBarControl.missing.apply(to: &f)
        #expect(f.pick == .rejected && f.colorTag == .pink && f.missingOnly)
        let before = f
        FilterBarControl.search.apply(to: &f)
        FilterBarControl.sort.apply(to: &f)
        #expect(f == before)
        FilterBarControl.clear.apply(to: &f)
        #expect(f == PhotoFilter())
    }

    @Test func stripAndPanelHeaders() {
        for t in ToolTab.allCases {
            assertLabelled([ToolStripControl.tab(t, selected: true), ToolStripControl.tab(t, selected: false)].enumerated()
                .map { AccessibleControl("\($0.offset)", $0.element.label, hint: $0.element.hint) }, "strip \(t)")
            assertLabelled([ToolStripControl.addTool(tabTitle: t.title)], "add tool \(t)")
        }
        for n in [1, 4] { assertLabelled([ToolStripControl.moreTabs(n)], "more tabs \(n)") }
        assertLabelled(ToolPanelID.allCases.map(ToolStripControl.panelMenu), "panel menus")
        #expect(Set(ToolPanelID.allCases.map { ToolStripControl.panelMenu($0).id }).count == ToolPanelID.allCases.count)
        for p in ToolPanelID.allCases {
            for collapsed in [true, false] {
                let cs = ToolStripControl.panelHeader(p, collapsed: collapsed, favorite: collapsed)
                assertLabelled(cs, "header \(p)")
                #expect(cs.count == (p.group == nil ? 2 : 5))
                #expect(cs[0].label.contains(collapsed ? "collapsed" : "expanded"))
            }
        }
    }

    @Test func menusAndBindableCommands() async {
        let m = makeModel()
        await m.start()
        let menu = MenuCommand.allCases.map { AccessibleControl($0.rawValue, CommandCatalog.title($0, model: m)) }
        assertLabelled(menu, "menu")
        let keys = BindableCommand.allCases.map { AccessibleControl($0.rawValue, CommandCatalog.title($0, bindings: m.settings.keyBindings)) }
        assertLabelled(keys, "bindable")
        #expect(keys.first { $0.id == "tagOrange" }?.label == "Orange tag", "unbound: no key suffix")
    }

    @Test func missingFlow() async {
        let m = makeModel(counts: [10])
        await m.start()
        #expect(m.missingCount == 0)
        let ids = Array(m.visibleIDs.prefix(3))
        m.updatePhotos(ids, label: "gone") { $0.isMissing = true }
        #expect(m.missingCount == 3)
        m.filter.pick = .unrejected // with any active filter, missing photos are hidden
        #expect(!m.visibleIDs.contains(ids[0]))
        m.filter = PhotoFilter()
        FilterBarControl.missing.apply(to: &m.filter)
        #expect(Set(m.visibleIDs) == Set(ids))
        m.perform(.selectAll)
        #expect(Set(MissingActions.removable(m)) == Set(ids))
        let cs = MissingActions.controls(selected: 3)
        assertLabelled(cs, "missing")
        #expect(cs[0].label == "Remove 3 missing photos" && cs[0].hint?.contains("never touched") == true)
        #expect(MissingActions.controls(selected: 1)[0].label == "Remove 1 missing photo")
        var p = samplePhoto()
        #expect(MissingActions.badge(p) == nil)
        p.fileChanged = true
        #expect(MissingActions.badge(p) == "File changed")
        #expect(PhotoAccessibility.label(p).hasSuffix("file changed"))
        p.isMissing = true
        #expect(MissingActions.badge(p) == "Missing")
    }

    @Test func openProgressCopy() {
        let url = URL(fileURLWithPath: "/Volumes/CARD")
        #expect(OpenProgressText.title(folder: url) == "Opening CARD")
        #expect(OpenProgressText.title(folder: nil) == "Opening folder")
        #expect(OpenProgressText.phase(OpenProgress(phase: .scanning, done: 0, total: 0)) == "Scanning…")
        #expect(OpenProgressText.phase(OpenProgress(phase: .scanning, done: 12, total: 12)) == "Scanning… 12 files")
        #expect(OpenProgressText.phase(OpenProgress(phase: .registering, done: 5, total: 1757)) == "Registering 5 of 1,757")
        let hashing = OpenProgress(phase: .hashing, done: 10, total: 20)
        #expect(OpenProgressText.phase(hashing).hasPrefix("Verifying files in the background"))
        #expect(!OpenProgressText.blocksBrowsing(hashing), "grid is usable while hashing")
        #expect(OpenProgressText.blocksBrowsing(OpenProgress(phase: .registering, done: 0, total: 1)))
        #expect(!OpenProgressText.cancel.label.isEmpty)
    }

    @Test func customControlDescriptions() {
        #expect(ControlDescriptions.wheel("Shadows", WheelValue()).value == "neutral")
        let w = ControlDescriptions.wheel("Shadows", WheelValue(hue: 210, saturation: 0.25, lightness: -0.1))
        #expect(w.label == "Shadows color wheel" && w.value.contains("210 degrees") && w.value.contains("25 percent"))
        #expect(ControlDescriptions.curve([]).value == "straight line")
        #expect(ControlDescriptions.curve([CurvePoint(0.25, 0.3)]).value == "1 point: 64 to 77")
        let l = ControlDescriptions.levels(LevelsEdit())
        #expect(l.label == "Levels" && l.value == "input 0 to 255, gamma 1.00, output 0 to 255")
    }
}

@Suite struct TabFocusPolicyTests {
    let tab = KeyInput("\t", special: .tab)

    @Test func tabTraversesControlsAndFullKeyboardAccess() {
        #expect(KeyMonitorPolicy.passesThrough(tab, focus: .control, fullKeyboardAccess: false), "Tab leaves a focused slider")
        #expect(KeyMonitorPolicy.passesThrough(tab, focus: .text, fullKeyboardAccess: false))
        #expect(KeyMonitorPolicy.passesThrough(tab, focus: .none, fullKeyboardAccess: true), "Full Keyboard Access owns Tab")
        #expect(!KeyMonitorPolicy.passesThrough(tab, focus: .none, fullKeyboardAccess: false), "bare Tab hides panels")
        #expect(!KeyMonitorPolicy.passesThrough(KeyInput("p"), focus: .control, fullKeyboardAccess: true))
        #expect(KeyMonitorPolicy.shouldHandle(tab, focus: .none, sheetOpen: false, isMainWindow: true) == .togglePanels)
        #expect(KeyMonitorPolicy.shouldHandle(tab, focus: .none, sheetOpen: false, isMainWindow: true,
                                              fullKeyboardAccess: true) == nil)
    }
}
