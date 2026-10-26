import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// The tool rows an AI edit changed (AI Optimize, ⌘K, Auto Adjust): which rows, their
/// hover texts, how long they stay, and the panel and tab badges.
@MainActor
@Suite struct AIHighlightsTests {
    static let ev = AIField(.exposure, "ev")
    static let vibrance = AIField(.presence, "vibrance")
    static let angle = AIField(.crop, "angle")
    static let temperature = AIField(.whiteBalance, "temperature")
    static let tint = AIField(.whiteBalance, "tint")

    /// Open AI Optimize on the focused photo and apply its default selection.
    static func optimize(_ m: AppModel) async {
        m.beginOptimize()
        await m.optimizePanel?.wait()
        m.optimizePanel?.apply()
    }

    // MARK: Sources

    @Test func optimizeMarksTheRowsItChangedWithTheirExplanations() async throws {
        let m = await optimizeModel()
        let id = try #require(m.focusedID)
        await Self.optimize(m)
        let marks = try #require(m.aiMarks[id])
        #expect(marks.changes.values.allSatisfy { $0.source == .optimize && $0.before == EditState() })
        #expect(marks.fields == [Self.ev, Self.vibrance, Self.angle], "\(marks.fields)")
        #expect(m.aiNote(Self.ev) == "AI changed it to +0.45 (was 0.00) — AI Optimize: Exposure +0.45 EV — the image was about half a stop dark (midtones at 11% vs the 18% target).")
        #expect(m.aiNote(Self.angle)?.contains("° (was 0.0°) — AI Optimize: Straighten −1.8°") == true, "\(m.aiNote(Self.angle) ?? "")")
        #expect(m.aiNote(AIField(.exposure, "contrast")) == nil)
        #expect(m.aiChangedPanels == [.exposure, .presence, .crop])
    }

    @Test func commandBarMarksTheRowsItChangedWithThePrompt() async throws {
        let m = await Nl9AppTests.app()
        let id = try #require(m.focusedID)
        _ = await m.runInstruction("warmer and a bit brighter")
        let marks = try #require(m.aiMarks[id])
        #expect(marks.changes.values.allSatisfy { $0.source == .command("warmer and a bit brighter") })
        #expect(marks.fields.isSuperset(of: [Self.ev, Self.temperature]) && !marks.fields.contains(AIField(.exposure, "contrast")))
        #expect(m.aiNote(Self.ev)?.hasPrefix("AI changed it to +") == true)
        #expect(m.aiNote(Self.ev)?.hasSuffix(" (was 0.00) — ⌘K: “warmer and a bit brighter”") == true)
        #expect(m.aiNote(Self.temperature)?.hasSuffix(" K (was As Shot) — ⌘K: “warmer and a bit brighter”") == true,
                "\(m.aiNote(Self.temperature) ?? "")")
        // "reset" through the bar is the user's reset, not an AI edit: the rows it changes clear.
        _ = await m.runInstruction("reset")
        #expect(m.aiMarks[id] == nil)
    }

    @Test func autoAdjustAddsItsRowsToAnEarlierSet() async throws {
        let m = await optimizeModel()
        let id = try #require(m.focusedID)
        await Self.optimize(m)
        await m.autoAdjust()
        let marks = try #require(m.aiMarks[id])
        // The mock's auto: custom WB 5200 K, +0.3 EV, shadows +20, highlights −15.
        let auto: Set<AIField> = [Self.temperature, Self.tint, Self.ev, AIField(.hdr, "shadows"), AIField(.hdr, "highlights")]
        #expect(marks.fields == auto.union([Self.vibrance, Self.angle]), "\(marks.fields)")
        #expect(auto.allSatisfy { marks.changes[$0]?.source == .auto })
        #expect(marks.changes[Self.vibrance]?.source == .optimize, "Optimize's rows Auto left alone stay")
        // Exposure was AI's twice: its value before AI is still the one before Optimize.
        #expect(m.aiNote(Self.ev) == "AI changed it to +0.30 (was 0.00) — Auto Adjust")
        // Undoing Auto brings Optimize's set back.
        m.undo()
        #expect(m.aiMarks[id]?.fields == [Self.ev, Self.vibrance, Self.angle])
        #expect(m.aiMarks[id]?.changes[Self.ev]?.source == .optimize)
    }

    // MARK: Lifetime

    @Test func touchingARowClearsOnlyThatRowAndUndoBringsItBack() async throws {
        let m = await optimizeModel()
        let id = try #require(m.focusedID)
        await Self.optimize(m)
        m.updateEdit(label: "Contrast") { $0.exposure.contrast = 10 } // not an AI row: nothing changes
        #expect(m.aiMarks[id]?.fields == [Self.ev, Self.vibrance, Self.angle])
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        #expect(m.aiMarks[id]?.fields == [Self.vibrance, Self.angle])
        m.undo()
        #expect(m.aiMarks[id]?.fields == [Self.ev, Self.vibrance, Self.angle], "the touch is undone, its highlight is back")
        m.redo()
        #expect(m.aiMarks[id]?.fields == [Self.vibrance, Self.angle])
        // Touching every row clears the set.
        m.updateEdit(label: "Vibrance") { $0.presence.vibrance = 0 }
        m.updateEdit(label: "Straighten") { $0.crop.angle = 0 }
        #expect(m.aiMarks[id] == nil && m.aiChangedPanels.isEmpty)
    }

    @Test func aDragThatTouchesRowsIsOneStepForTheHighlightsToo() async throws {
        let m = await optimizeModel()
        let id = try #require(m.focusedID)
        await Self.optimize(m)
        m.updateEdit(label: "Drag", coalesceKey: "k") { $0.exposure.ev = 0.6 }
        m.updateEdit(label: "Drag", coalesceKey: "k") { $0.presence.vibrance = 5 }
        m.endEditGesture()
        #expect(m.aiMarks[id]?.fields == [Self.angle])
        m.undo()
        #expect(m.aiMarks[id]?.fields == [Self.ev, Self.vibrance, Self.angle])
    }

    @Test func undoRemovesAnAIStepsHighlightsAndRedoRestoresThem() async throws {
        let m = await optimizeModel()
        let id = try #require(m.focusedID)
        await Self.optimize(m)
        let applied = try #require(m.aiMarks[id])
        m.undo()
        #expect(m.aiMarks[id] == nil && m.aiChangedPanels.isEmpty)
        m.redo()
        #expect(m.aiMarks[id] == applied)
    }

    @Test func clearRemovesTheTargetsHighlightsFromTheMenu() async throws {
        let m = await optimizeModel()
        let id = try #require(m.focusedID)
        #expect(!CommandCatalog.isEnabled(.clearAIHighlights, model: m))
        await Self.optimize(m)
        #expect(CommandCatalog.title(.clearAIHighlights, model: m) == "Clear AI Highlights")
        #expect(CommandCatalog.isEnabled(.clearAIHighlights, model: m) && MenuCommand.clearAIHighlights.shortcut == nil)
        CommandCatalog.perform(.clearAIHighlights, model: m)
        #expect(m.aiMarks[id] == nil && !m.canClearAIHighlights)
        #expect(m.edit(for: id).exposure.ev == 0.45, "the values stay; only the highlights go")
    }

    @Test func eachPhotoShowsItsOwnSet() async throws {
        let m = await optimizeModel()
        let (a, b) = (m.visibleIDs[0], m.visibleIDs[1])
        await Self.optimize(m)
        m.focus(b, extend: false)
        #expect(m.focusedAIMarks == nil && m.aiNote(Self.ev) == nil)
        await m.autoAdjust()
        #expect(m.focusedAIMarks?.changes[Self.ev]?.source == .auto)
        m.focus(a, extend: false)
        #expect(m.focusedAIMarks?.changes[Self.ev]?.source == .optimize && m.aiNote(Self.vibrance) != nil)
        #expect(m.aiMarks[b]?.changes.values.allSatisfy { $0.source == .auto } == true)
    }

    /// A photo not AI edited this session: its stored report's changes that the edit still
    /// has are highlighted (Optimize's persistence; ⌘K and Auto are per session).
    @Test func storedReportsHighlightWhatIsStillApplied() async throws {
        let store = MockSessionStore(counts: [6])
        let m = await optimizeModel(store: store)
        let (a, b, c) = (m.visibleIDs[0], m.visibleIDs[1], m.visibleIDs[2])
        let report = try OptimizeReport(json: ReportJSON.typical)
        let (edit, marked) = try RustPhotoOptimizer().apply(report, selected: [0, 1, 3], to: EditState())
        try await store.saveOptimizeReport(marked, for: [a, b, c])
        m.updateEdits([a, b, c], label: "Load") { id, e in
            e = edit
            if id == b { e.presence.vibrance = 30 } // changed since
        }
        #expect(m.aiMarks.isEmpty)
        _ = await m.loadOptimizeReport(for: a)
        #expect(m.aiMarks[a]?.fields == [Self.ev, Self.vibrance, Self.angle])
        #expect(m.aiMarks[a]?.changes[Self.ev]?.explanation?.hasPrefix("Exposure +0.45 EV") == true)
        #expect(m.aiNote(Self.ev)?.hasPrefix("AI changed it to +0.45 — AI Optimize: Exposure +0.45 EV") == true,
                "no “was”: the report doesn't say")
        #expect(m.aiMarks[a]?.changes[Self.ev]?.before == nil, "a report doesn't say what the value was")
        _ = await m.loadOptimizeReport(for: b)
        #expect(m.aiMarks[b]?.fields == [Self.ev, Self.angle])
        // A photo AI edited this session keeps its own set.
        m.focus(c, extend: false)
        await m.autoAdjust()
        _ = await m.loadOptimizeReport(for: c)
        #expect(m.aiMarks[c]?.changes.values.allSatisfy { $0.source == .auto } == true)
    }

    @Test func aStoredToggleBackAtItsDefaultMarksItsRow() throws {
        let report = try OptimizeReport(json: ReportJSON.typical)
        let (edit, marked) = try RustPhotoOptimizer().apply(report, selected: [2], to: {
            var e = EditState()
            e.lens.enabled = false
            return e
        }())
        #expect(edit.lens.enabled)
        let m = try #require(AIMarks(report: marked, stillApplied: [2], edit: edit))
        #expect(m.fields == [AIFieldMap.lensEnabled])
        #expect(AIMarks(report: marked, stillApplied: [], edit: edit) == nil)
    }

    // MARK: Rows and badges

    @Test func slidersFindTheirRowsByKeyPath() {
        #expect(AIFieldMap.field(for: \.exposure.ev) == Self.ev)
        #expect(AIFieldMap.field(for: \.colorEditor.bands[3].saturation) == AIField(.colorEditor, "band3.saturation"))
        #expect(AIFieldMap.field(for: \.layerlessProbe) == nil)
        // Every slider row and named row is unique.
        let fields = AIFieldMap.entries.map(\.field)
        #expect(Set(fields).count == fields.count)
        for f in [AIFieldMap.levels, AIFieldMap.curve, AIFieldMap.colorBalance, AIFieldMap.aiDenoise, AIFieldMap.lensEnabled,
                  AIFieldMap.chromaticAberration, AIFieldMap.cropFrame, AIFieldMap.layers] {
            #expect(fields.contains(f))
        }
        var e = EditState()
        e.levels.inBlack = 0.05
        e.crop.aspect = .r4x5
        e.layers = [LayerEdit(id: 1, name: "Sky")]
        e.noise.aiDenoise = true
        e.colorBalance.master.hue = 30
        e.curve.points[.rgb] = [CurvePoint(0, 0.1), CurvePoint(1, 1)]
        e.lens.chromaticAberration = false
        #expect(AIFieldMap.changed(EditState(), e) == [AIFieldMap.levels, AIFieldMap.cropFrame, AIFieldMap.layers,
                                                       AIFieldMap.aiDenoise, AIFieldMap.colorBalance, AIFieldMap.curve,
                                                       AIFieldMap.chromaticAberration])
        #expect(AIFieldMap.changed(e, e).isEmpty)
    }

    @Test func badgesRollUpToThePanelAndEveryTabHoldingIt() async throws {
        let m = await optimizeModel()
        await Self.optimize(m)
        let changed = m.aiChangedPanels
        #expect(changed == [.exposure, .presence, .crop])
        #expect(AIHighlightStyle.tabHasChanges(ToolTab.exposure.panels, changed: changed))
        #expect(AIHighlightStyle.tabHasChanges(ToolTab.composition.panels, changed: changed))
        #expect(AIHighlightStyle.tabHasChanges(ToolTab.favorites.panels, changed: changed), "Favorites holds Exposure too")
        #expect(!AIHighlightStyle.tabHasChanges(ToolTab.lens.panels, changed: changed))
        #expect(!AIHighlightStyle.tabHasChanges([], changed: changed))
        // A panel moved into another tab still badges it (keyed by panel, not position).
        #expect(AIHighlightStyle.tabHasChanges([.lensCorrection, .presence], changed: changed))
    }

    /// Tabs and stacks are the user's: a tool moved into a custom tab badges that tab, and
    /// the tab it left doesn't.
    @Test func badgesFollowTheUsersArrangement() async throws {
        let m = await optimizeModel()
        await Self.optimize(m)
        let layout = ToolLayout(defaults: testDefaults())
        let mine = layout.edit { $0.addCustomTab(title: "Mine") }
        layout.edit { $0.move(.presence, from: .exposure, to: mine) }
        layout.edit { $0.remove(.exposure, from: .exposure) }
        layout.edit { $0.remove(.exposure, from: .favorites) }
        let changed = m.aiChangedPanels
        #expect(AIHighlightStyle.tabHasChanges(layout.panels(for: mine), changed: changed))
        #expect(!AIHighlightStyle.tabHasChanges(layout.panels(for: .exposure), changed: changed), "\(layout.panels(for: .exposure))")
        #expect(!AIHighlightStyle.tabHasChanges(layout.panels(for: .favorites), changed: changed))
    }

    /// Show AI-Changed Tools: every changed tool expanded and the sidebar revealing the
    /// first (through `requestReveal`), in a custom tab too; a tool no tab holds goes home.
    @Test func showAIChangedToolsRevealsThem() async throws {
        let m = await optimizeModel()
        let layout = ToolLayout(defaults: testDefaults())
        #expect(!CommandCatalog.isEnabled(.showAIChanges, model: m, layout: layout))
        await Self.optimize(m)
        #expect(CommandCatalog.title(.showAIChanges, model: m) == "Show AI-Changed Tools")
        let mine = layout.edit { $0.addCustomTab(title: "Mine") }
        layout.edit { $0.insert(.presence, in: mine) }
        m.toolTab = mine
        m.collapsedPanels = [.presence, .exposure, .lensCorrection]
        CommandCatalog.perform(.showAIChanges, model: m, layout: layout)
        #expect(m.toolTab == mine && layout.revealRequest?.panel == .presence)
        #expect(m.collapsedPanels == [.lensCorrection])
        // Only the straighten left, and Crop removed from every tab: it goes back home.
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 0 }
        m.updateEdit(label: "Vibrance") { $0.presence.vibrance = 0 }
        #expect(m.aiChangedPanels == [.crop])
        for tab in layout.order where layout.panels(for: tab).contains(.crop) { layout.edit { $0.remove(.crop, from: tab) } }
        m.toolTab = .lens
        CommandCatalog.perform(.showAIChanges, model: m, layout: layout)
        #expect(m.toolTab == .composition && layout.panels(for: .composition).contains(.crop))
        #expect(layout.revealRequest?.panel == .crop)
        // Nothing changed: nothing to show.
        m.updateEdit(label: "Straighten") { $0.crop.angle = 0 }
        let request = layout.revealRequest
        ToolWorkspaceCommands.revealAIChanges(model: m, layout: layout)
        #expect(layout.revealRequest == request)
    }

    // MARK: Accessibility

    @Test func voiceOverSaysChangedByAI() {
        #expect(AIHighlightStyle.accessibilityValue("+0.45 EV") == "+0.45 EV, changed by AI")
        #expect(AIHighlightStyle.accessibilityValue("") == "changed by AI")
        #expect(AIHighlightStyle.containerSuffix == ", has settings changed by AI")
    }

    /// The outline and badge (the AI yellow) are ≥ 3:1 on the panel and on the tinted row;
    /// the row's texts stay ≥ 4.5:1 on the tint. The yellow is not the amber accent.
    @Test func highlightContrast() {
        for (name, accents, greys) in [("dark", AccentPalette.dark, GreyPalette.dark), ("light", AccentPalette.light, GreyPalette.light)] {
            let tint = AIHighlightStyle.tinted(accents.aiChange, onGrey: greys.panel)
            #expect(accents.aiChange.contrast(onGrey: greys.panel) >= 3, "\(name) AI yellow on panel")
            #expect(Contrast.ratio(accents.aiChange.luminance, tint.luminance) >= 3, "\(name) AI yellow on tint")
            #expect(accents.aiChange.g > accents.accent.g + 0.1, "\(name): yellower than the accent")
            for text in [greys.textPrimary, greys.textSecondary] {
                #expect(Contrast.ratio(Contrast.luminance(text), tint.luminance) >= 4.5, "\(name) text \(text) on tint")
            }
        }
    }
}

private extension EditState {
    /// A Double key path no row shows.
    var layerlessProbe: Double {
        get { 0 }
        set {}
    }
}
