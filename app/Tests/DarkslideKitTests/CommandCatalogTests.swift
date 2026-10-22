import AppKit
import Foundation
import Testing
@testable import DarkslideKit

@MainActor
@Suite struct CommandCatalogTests {
    func layout() -> ToolLayout {
        let suite = testSuiteName()
        return ToolLayout(defaults: UserDefaults(suiteName: suite)!)
    }

    @Test func titlesDependOnMode() async {
        let demo = makeModel()
        #expect(CommandCatalog.title(.importCard, model: demo) == "Import from Card…")
        let live = AppModel(sessions: MockSessionStore(counts: [3]), images: MockImageProvider(latency: .zero),
                            renderer: FakeRenderer(), importer: MockImportService(),
                            archive: UnavailableArchiveService(target: ""), ai: MockAIService(),
                            settings: makeSettings(), mode: .live)
        #expect(CommandCatalog.title(.importCard, model: live).contains("Coming Soon"))
        #expect(CommandCatalog.title(.fullScreen, model: demo) == "Enter Full Screen")
        demo.viewer.state.fullScreen = true
        #expect(CommandCatalog.title(.fullScreen, model: demo) == "Exit Full Screen")
        for c in MenuCommand.allCases {
            #expect(!CommandCatalog.title(c, model: demo).isEmpty && c.id == c.rawValue)
        }
    }

    @Test func shortcutsAreUnique() {
        let keys = MenuCommand.allCases.compactMap(\.shortcut).map { "\($0.key)-\($0.modifiers.map { "\($0)" }.sorted())" }
        #expect(Set(keys).count == keys.count, "two menu items can't share a shortcut")
        #expect(MenuCommand.importCard.shortcut == .init(key: "i", modifiers: [.command, .shift]))
    }

    @Test func enablementFollowsState() async {
        let m = makeModel()
        let l = layout()
        #expect(!CommandCatalog.isEnabled(.copyAdjustments, model: m, layout: l), "no photo yet")
        #expect(!CommandCatalog.isEnabled(.aiCull, model: m, layout: l))
        #expect(CommandCatalog.isEnabled(.openFolder, model: m, layout: l))
        await m.start()
        #expect(CommandCatalog.isEnabled(.copyAdjustments, model: m, layout: l))
        #expect(CommandCatalog.isEnabled(.aiCull, model: m, layout: l))
        #expect(!CommandCatalog.isEnabled(.applyAdjustments, model: m, layout: l))
        m.updateEdit(label: "ev") { $0.exposure.ev = 0.3 } // Edited mode: something to paste
        m.copyAdjustments()
        #expect(!CommandCatalog.isEnabled(.applyAdjustments, model: m, layout: l), "only the copied photo is selected")
        m.focus(m.visibleIDs[1], extend: false)
        #expect(CommandCatalog.isEnabled(.applyAdjustments, model: m, layout: l))
        m.focus(m.visibleIDs[0], extend: false)
        #expect(CommandCatalog.title(.copyAdjustments, model: m) == "Copy Adjustments", "no dialog: no ellipsis")
        m.toolTab = l.order.first!
        #expect(!CommandCatalog.isEnabled(.moveTabUp, model: m, layout: l))
        #expect(CommandCatalog.isEnabled(.moveTabDown, model: m, layout: l))
        for c in BindableCommand.allCases {
            #expect(CommandCatalog.isEnabled(c, model: m))
        }
        m.focus(nil, extend: false)
        #expect(!CommandCatalog.isEnabled(.pick, model: m) && CommandCatalog.isEnabled(.gridMode, model: m))
    }

    @Test func performRoutesToTheModel() async {
        let m = makeModel()
        let l = layout()
        await m.start()
        #expect(CommandCatalog.perform(.openFolder, model: m, layout: l) == MenuCommand.Effect.none, "recents: the Open sheet")
        #expect(m.sheet == .openSession)
        m.sheet = nil
        #expect(CommandCatalog.perform(.importCard, model: m, layout: l) == MenuCommand.Effect.none)
        #expect(m.sheet == .importCard(nil))
        m.sheet = nil
        CommandCatalog.perform(.copyAdjustments, model: m, layout: l)
        #expect(m.sheet == nil && m.clipboard?.sourceID == m.focusedID)
        CommandCatalog.perform(.applyAdjustments, model: m, layout: l)
        #expect(m.sheet == nil && m.toast?.message.hasPrefix("No adjustments to apply") == true, "an unedited photo copies nothing")
        CommandCatalog.perform(.rotateRight, model: m, layout: l)
        #expect(m.focusedEdit.crop.quarterTurns == 1)
        CommandCatalog.perform(.rotateLeft, model: m, layout: l)
        #expect(m.focusedEdit.crop.quarterTurns == 0)
        CommandCatalog.perform(.fullScreen, model: m, layout: l)
        #expect(m.viewer.state.fullScreen)
        CommandCatalog.perform(.cheatSheet, model: m, layout: l)
        #expect(m.showCheatSheet)
        CommandCatalog.perform(.zoomActual, model: m, layout: l)
        CommandCatalog.perform(.zoomFit, model: m, layout: l)
        #expect(m.viewer.state.zoom == .fit)
        CommandCatalog.perform(.zoomIn, model: m, layout: l)
        CommandCatalog.perform(.zoomOut, model: m, layout: l)
        CommandCatalog.perform(.copyFromPrevious, model: m, layout: l)
        CommandCatalog.perform(.autoAdjust, model: m, layout: l)
        CommandCatalog.perform(.aiCull, model: m, layout: l)
        // Keyboard tab reordering (N17).
        m.toolTab = l.order[2]
        let tab = m.toolTab
        CommandCatalog.perform(.moveTabUp, model: m, layout: l)
        #expect(l.order[1] == tab)
        CommandCatalog.perform(.moveTabDown, model: m, layout: l)
        CommandCatalog.perform(.moveTabDown, model: m, layout: l)
        #expect(l.order[3] == tab)
    }

    @Test func menusListEveryBindableCommandOnce() {
        let listed = CommandCatalog.photoMenu.compactMap { $0 } + CommandCatalog.viewMenu + CommandCatalog.toolMenu
            + CommandCatalog.keyOnly
        #expect(Set(listed) == Set(BindableCommand.allCases))
        #expect(listed.count == BindableCommand.allCases.count)
        #expect(CommandCatalog.title(.pick, bindings: .defaults) == "Pick\tP")
    }

    @Test func toolbarHelpUsesLiveBindings() {
        var b = KeyBindings.defaults
        #expect(ToolbarHelp.beforeAfter(b) == "Before / after split (Y)")
        b.bind(.beforeAfter, to: "b")
        #expect(ToolbarHelp.beforeAfter(b) == "Before / after split (B)")
        b.unbind(.pinCompare)
        #expect(ToolbarHelp.compare(b) == "Pin this photo and compare others against it")
        #expect(ToolbarHelp.viewerGrid(.defaults) == "Viewer (E) or Grid (G)")
        #expect(ToolbarHelp.pick(.defaults) == "Pick (P)" && ToolbarHelp.reject(.defaults) == "Reject (X)")
        #expect(ToolbarHelp.stack(.defaults).hasSuffix("(S)"))
        #expect(ToolbarHelp.autoAdjust().contains("⇧A"))
        #expect(ToolbarHelp.importTitle(mode: .live) == "Import (coming soon)" && ToolbarHelp.importTitle(mode: .demo) == "Import")
        #expect(ToolbarHelp.importButton(mode: .live).contains("coming soon") && !ToolbarHelp.importButton(mode: .live).contains("ds-"))
        #expect(ToolbarHelp.importButton(mode: .demo).contains("not verified"))
    }
}

@MainActor
@Suite struct TerminationFlowTests {
    @Test func alertsOnlyWhenWorkWouldBeLost() {
        #expect(TerminationFlow.alert(for: .ok) == nil)
        let failed = TerminationFlow.alert(for: .failed(message: "disk full", pending: 3))!
        #expect(failed.title == "3 changes couldn’t be saved" && failed.message.contains("disk full"))
        #expect(failed.choices == [.retry, .cancel, .quitAnyway] && failed.isCritical)
        #expect(TerminationFlow.alert(for: .failed(message: "x", pending: 1))!.title == "1 change couldn’t be saved")
        let mem = TerminationFlow.alert(for: .unsavedInMemory(count: 12))!
        #expect(mem.title.contains("12") && mem.choices == [.cancel, .quitAnyway])
        #expect(TerminationFlow.alert(for: .unsavedInMemory(count: 1))!.title.contains("1 rating and edit"))
        #expect(TerminationFlow.Choice.allCases.allSatisfy { !$0.title.isEmpty })
    }

    @Test func repliesMapChoices() {
        #expect(TerminationFlow.reply(for: .quitAnyway) == .terminate)
        #expect(TerminationFlow.reply(for: .cancel) == .cancel)
        #expect(TerminationFlow.reply(for: .retry) == .retry)
    }

    @Test func runRetriesUntilSavedOrAnswered() async {
        var decisions: [AppModel.TerminationDecision] = [.failed(message: "x", pending: 1), .ok]
        var asked = 0
        let quit = await TerminationFlow.run(decide: { decisions.removeFirst() }, ask: { _ in
            asked += 1
            return .retry
        })
        #expect(quit && asked == 1, "retry flushed successfully, then quit")
        let cancelled = await TerminationFlow.run(decide: { .unsavedInMemory(count: 2) }, ask: { _ in .cancel })
        #expect(!cancelled)
        let forced = await TerminationFlow.run(decide: { .unsavedInMemory(count: 2) }, ask: { _ in .quitAnyway })
        #expect(forced)
        #expect(await TerminationFlow.run(decide: { .ok }, ask: { _ in Issue.record("no alert"); return .cancel }))
    }

    @Test func realModelQuitDecision() async {
        let m = makeModel()
        await m.start()
        #expect(await m.prepareToTerminate(timeout: .seconds(2)) == .ok)
    }
}

@MainActor
@Suite struct PanelChromeTests {
    @Test func rules() {
        #expect(PanelChrome.headerButtonOpacity(hovering: true) == 1)
        #expect(PanelChrome.headerButtonOpacity(hovering: false) >= 0.35, "N17: never invisible")
        #expect(PanelChrome.disclosureLabel("Exposure", collapsed: true) == "Exposure, collapsed")
        #expect(PanelChrome.disclosureLabel("Exposure", collapsed: false) == "Exposure, expanded")
        #expect(PanelChrome.histogramTitle(rendersEdits: false) == "Preview histogram")
        #expect(PanelChrome.histogramTitle(rendersEdits: true) == "Histogram")
        #expect(PanelChrome.photoCount(1) == "1 photo" && PanelChrome.photoCount(2000) == "2,000 photos")
    }

    @Test func tabKeyboardReordering() {
        let l = ToolLayout(defaults: UserDefaults(suiteName: testSuiteName())!)
        let first = l.order[0], second = l.order[1]
        #expect(!l.canMove(first, by: -1) && l.canMove(first, by: 1) && !l.canMove(first, by: 0))
        l.move(first, by: 1)
        #expect(l.order[0] == second && l.order[1] == first)
        l.move(first, by: -1)
        #expect(l.order[0] == first)
        l.move(first, by: -1) // no-op at the top
        #expect(l.order[0] == first)
        let last = l.order.last!
        #expect(!l.canMove(last, by: 1))
    }

    @Test func archiveDisplay() {
        let states: [ArchiveStatus.State] = [.idle, .syncing(fraction: 0.5), .behind(files: 1), .behind(files: 3), .offline,
                                             .failed("boom"), .unavailable("later")]
        for s in states {
            #expect(!ArchiveDisplay.short(s).isEmpty && !ArchiveDisplay.long(s).isEmpty && !ArchiveDisplay.symbol(s).isEmpty)
            #expect(!ArchiveDisplay.title(s).isEmpty)
        }
        #expect(ArchiveDisplay.short(.unavailable("x")) == "Archive off")
        #expect(ArchiveDisplay.long(.behind(files: 1)) == "1 file waiting")
        #expect(ArchiveDisplay.explanation(.unavailable("x")).contains("Settings › Archive"))
        #expect(ArchiveDisplay.explanation(.idle).contains("never deletes"))
        #expect(!ArchiveDisplay.canSyncNow(.unavailable("x")) && !ArchiveDisplay.canSyncNow(.syncing(fraction: 0.1)))
        #expect(ArchiveDisplay.canSyncNow(.offline))
        #expect(ArchiveDisplay.accessibilityLabel(ArchiveStatus(state: .idle, target: "/x", lastSuccess: nil)) == "Archive: Up to date")
    }

    @Test func gestureIdleEnderFiresOncePerBurst() async {
        let e = GestureIdleEnder(delay: .milliseconds(20))
        var ended = 0
        for _ in 0..<5 { e.touch { ended += 1 } }
        #expect(e.isActive)
        #expect(await waitUntil { ended == 1 })
        #expect(!e.isActive)
        try? await Task.sleep(for: .milliseconds(40))
        #expect(ended == 1, "a burst of nudges is one gesture")
        e.touch { ended += 1 }
        e.flush()
        #expect(ended == 2 && !e.isActive)
        e.flush()
        #expect(ended == 2, "flush without input does nothing")
    }

    @Test func sliderGesturesAreSeparateUndoSteps() async {
        let m = makeModel()
        await m.start()
        for v in [0.1, 0.2, 0.3] { m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = v } }
        m.endEditGesture()
        for v in [0.4, 0.5] { m.updateEdit(label: "Exposure", coalesceKey: "ev") { $0.exposure.ev = v } }
        #expect(m.history.undoStack.count == 2, "two drags = two undo steps (L3)")
        m.undo()
        #expect(abs(m.focusedEdit.exposure.ev - 0.3) < 1e-9)
    }
}
