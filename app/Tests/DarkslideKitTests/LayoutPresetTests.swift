import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Familiar layouts and names (Settings › Familiar layout): placement and words only.
@Suite struct LayoutPresetTests {
    @Test func placementPerLayout() {
        #expect(LayoutPreset.darkslide.toolSide == .right && LayoutPreset.darkslide.copyApply == .toolbar)
        #expect(LayoutPreset.lightroomClassic.toolSide == .right && LayoutPreset.lightroomClassic.copyApply == .panelFoot)
        #expect(LayoutPreset.captureOne.toolSide == .left && LayoutPreset.captureOne.copyApply == .toolbarEnd)
        #expect(LayoutPreset.allCases.map(\.title) == ["Darkslide", "Lightroom Classic", "Capture One"])
        #expect(LayoutPreset.allCases.allSatisfy { $0.names.rawValue == $0.rawValue && $0.id == $0.rawValue })
    }

    @Test func lightroomNames() {
        let n = NameSet.lightroomClassic
        #expect(n.copyButton == "Copy…" && n.applyButton == "Paste")
        #expect(n.copyMenu == "Copy Settings…" && n.applyMenu == "Paste Settings")
        #expect(n.newVariant == "Create Virtual Copy" && n.layers == "Masks" && n.styles == "Presets")
        #expect(n.title(of: ToolTab.layers) == "Masks" && n.title(of: ToolTab.color) == "Color")
        #expect(n.title(of: ToolPanelID.layers) == "Masks" && n.title(of: ToolPanelID.styles) == "Presets")
        #expect(n.title(of: ToolPanelID.exposure) == ToolPanelID.exposure.title)
        #expect(n.title == "Lightroom Classic")
    }

    @Test func darkslideAndCaptureOneNames() {
        for n: NameSet in [.darkslide, .captureOne] {
            #expect(n.copyButton == "Copy" && n.applyButton == "Apply")
            #expect(n.copyMenu == "Copy Adjustments" && n.applyMenu == "Apply Adjustments")
            #expect(n.newVariant == "New Variant" && n.layers == "Layers")
            #expect(n.title(of: ToolTab.layers) == ToolTab.layers.title)
        }
        #expect(NameSet.darkslide.styles == ToolPanelID.styles.title)
        #expect(NameSet.captureOne.styles == "Styles")
    }

    @MainActor @Test func namesFollowTheLayoutUnlessChosen() {
        let suite = testSuiteName()
        let a = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(a.layoutPreset == .darkslide && a.namePreset == nil && a.names == .darkslide)
        a.layoutPreset = .lightroomClassic
        #expect(a.names == .lightroomClassic, "names follow the layout")
        a.namePreset = .captureOne
        #expect(a.names == .captureOne, "unless chosen on their own")
        let b = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(b.layoutPreset == .lightroomClassic && b.namePreset == .captureOne)
        b.namePreset = nil
        let c = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(c.namePreset == nil && c.names == .lightroomClassic)
    }

    @MainActor @Test func toolSideFollowsTheLayoutUnlessChosen() {
        let suite = testSuiteName()
        let a = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(a.toolSidePreset == nil && a.toolSide == .right)
        a.putTools(on: .left)
        #expect(a.toolSidePreset == .left && a.toolSide == .left, "View ▸ Tools on Left")
        a.layoutPreset = .lightroomClassic
        #expect(a.toolSide == .left, "a chosen side outlasts a layout change")
        let b = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(b.toolSidePreset == .left && b.toolSide == .left, "persisted")
        b.layoutPreset = .captureOne
        b.putTools(on: .left)
        #expect(b.toolSidePreset == nil, "the layout's own side follows the layout again")
        b.putTools(on: .right)
        #expect(b.toolSidePreset == .right && b.toolSide == .right)
        b.toolSidePreset = nil
        let c = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(c.toolSidePreset == nil && c.toolSide == .left)
    }

    @Test func aRenamedTabKeepsItsName() {
        var w = ToolWorkspace.default
        #expect(w.title(of: .layers, names: .lightroomClassic) == "Masks")
        w.rename(.layers, to: "Local")
        #expect(w.title(of: .layers, names: .lightroomClassic) == "Local")
    }

    @MainActor @Test func menuTitlesUseTheNames() {
        let m = makeModel()
        #expect(CommandCatalog.title(MenuCommand.copyAdjustments, model: m) == "Copy Adjustments")
        m.settings.layoutPreset = .lightroomClassic
        #expect(CommandCatalog.title(MenuCommand.copyAdjustments, model: m) == "Copy Settings…")
        #expect(CommandCatalog.title(MenuCommand.applyAdjustments, model: m) == "Paste Settings")
        #expect(CommandCatalog.title(MenuCommand.newVariant, model: m) == "Create Virtual Copy")
    }
}

@MainActor
@Suite struct FamiliarChoiceTests {
    @Test func firstRunChoiceSetsLayoutKeysAndNames() throws {
        let s = makeSettings()
        s.namePreset = .darkslide
        s.putTools(on: .left)
        let o = OnboardingModel(settings: s, fileExists: { _ in true }, createDirectory: { _ in })
        o.next()
        #expect(o.step == .familiar && !o.step.title.isEmpty)
        #expect(o.familiar == .darkslide)
        o.familiar = .lightroomClassic
        #expect(s.layoutPreset == .darkslide, "nothing is written before Finish")
        o.archiveEnabled = false
        try o.finish()
        #expect(s.layoutPreset == .lightroomClassic && s.keyPreset == .lightroomClassic)
        #expect(s.namePreset == nil && s.names == .lightroomClassic)
        #expect(s.toolSidePreset == nil && s.toolSide == .right, "the chosen layout's side")
        #expect(s.keyBindings == KeyPreset.lightroomClassic.bindings)
    }

    @Test func keepingTheCurrentChoiceLeavesOwnKeysAlone() throws {
        let s = makeSettings()
        s.keyBindings.bind(.pick, to: "q")
        let o = OnboardingModel(settings: s, fileExists: { _ in true }, createDirectory: { _ in })
        o.archiveEnabled = false
        try o.finish() // "Show Welcome Again" then Finish must not reset the user's keys
        #expect(s.keyBindings.key(for: .pick) == "q")
    }

    @Test func keySetForEachLayoutAndSummaries() {
        for l in LayoutPreset.allCases {
            #expect(KeyPreset(layout: l).rawValue == l.rawValue)
            #expect(!l.summary.isEmpty && !KeyPreset(layout: l).summary.isEmpty)
        }
    }

    @Test func paneWording() {
        #expect(FamiliarText.keysStatus(.lightroomClassic, KeyPreset.lightroomClassic.bindings) == "Lightroom Classic keys, unchanged")
        var b = KeyPreset.darkslide.bindings
        b.unbind(.survey)
        #expect(FamiliarText.keysStatus(.darkslide, b) == "Darkslide keys + 1 of your own change")
        b.unbind(.toggleClipping)
        #expect(FamiliarText.keysStatus(.darkslide, b) == "Darkslide keys + 2 of your own changes")
        #expect(FamiliarText.namesExample(.lightroomClassic) == "Copy… / Paste · Masks · Presets · Create Virtual Copy")
        #expect(FamiliarText.choiceTitle(.darkslide) == "Darkslide, or nothing yet")
        #expect(FamiliarText.choiceTitle(.captureOne) == "Capture One")
    }
}
