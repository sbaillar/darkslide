import Foundation
import Testing
@testable import DarkslideKit

/// Keyboard sets that match Lightroom Classic and Capture One (Settings › Familiar layout).
@Suite struct KeyPresetTests {
    @Test func darkslideIsTheDefaultsAndGainsLightroomLettersForNewCommands() {
        #expect(KeyPreset.darkslide.bindings == .defaults)
        let d = KeyBindings.defaults
        #expect(d.command(for: "w") == .whiteBalancePicker)
        #expect(d.command(for: "n") == .survey)
        #expect(d.command(for: "j") == .toggleClipping)
        for c: BindableCommand in [.brushTool, .eraserTool, .gradientTool, .radialTool] {
            #expect(d.key(for: c) == nil, "mask tools stay unbound in Darkslide's own set")
        }
    }

    /// Adobe's list: C Compare, R Crop, K Brush, M Graduated filter, W white balance,
    /// N Survey, J clipping, Y before/after, P/X/U flags, G/E grid and loupe, S stack.
    @Test func lightroomClassicKeys() {
        let b = KeyPreset.lightroomClassic.bindings
        let expected: [Character: KeyCommand] = [
            "c": .togglePinCompare, "r": .tool(.crop), "k": .maskTool(.brush), "m": .maskTool(.linearGradient),
            "w": .whiteBalancePicker, "n": .survey, "j": .toggleClipping, "y": .toggleBeforeAfter,
            "p": .pick, "x": .reject, "u": .unflag, "g": .gridMode, "e": .viewerMode, "s": .toggleStack,
            "6": .tag(.red), "7": .tag(.yellow), "8": .tag(.green), "9": .tag(.blue), "0": .rate(0), "5": .rate(5),
        ]
        for (key, command) in expected {
            #expect(b.command(for: key) == command, "\(key)")
        }
        #expect(b.key(for: .straightenTool) == nil, "straighten lives inside Crop in Lightroom")
    }

    @Test func captureOneCursorLetters() {
        let b = KeyPreset.captureOne.bindings
        #expect(b.command(for: "c") == .tool(.crop))
        #expect(b.command(for: "r") == .tool(.straighten))
        #expect(b.command(for: "b") == .maskTool(.brush))
        #expect(b.command(for: "e") == .maskTool(.eraser))
        #expect(b.command(for: "g") == .maskTool(.linearGradient))
        #expect(b.command(for: "t") == .maskTool(.radialGradient))
        #expect(b.command(for: "w") == .whiteBalancePicker)
        for c: BindableCommand in [.toggleFilmstrip, .viewerMode, .gridMode] {
            #expect(b.key(for: c) == nil, "\(c) gave its letter to a cursor tool")
        }
    }

    @Test func everySetHasOneCommandPerKeyAndUniqueMenuShortcuts() {
        for preset in KeyPreset.allCases {
            let keys = preset.bindings.keys.values
            #expect(Set(keys).count == keys.count, "\(preset): a key on two commands")
            let shortcuts = MenuCommand.allCases.compactMap { c in preset.shortcut(c).map { "\($0.key)\($0.modifiers.map { "\($0)" }.sorted())" } }
            #expect(Set(shortcuts).count == shortcuts.count, "\(preset): a menu shortcut on two commands")
        }
    }

    @Test func lightroomMenuShortcuts() {
        let lr = KeyPreset.lightroomClassic
        #expect(lr.shortcut(.autoAdjust) == MenuCommand.Shortcut(key: "u", modifiers: [.command]))
        #expect(lr.shortcut(.exportSelected) == MenuCommand.Shortcut(key: "e", modifiers: [.command, .shift]))
        #expect(lr.shortcut(.newVariant) == MenuCommand.Shortcut(key: "'", modifiers: [.command]))
        // Already Lightroom's: copy / paste settings, paste from previous, import.
        for c: MenuCommand in [.copyAdjustments, .applyAdjustments, .copyFromPrevious, .importCard, .rotateLeft] {
            #expect(lr.shortcut(c) == c.shortcut)
        }
        for c in MenuCommand.allCases {
            #expect(KeyPreset.darkslide.shortcut(c) == c.shortcut)
            #expect(KeyPreset.captureOne.shortcut(c) == c.shortcut, "Capture One menu keys wait for verification")
        }
    }

    @Test func titlesAndChangeCount() {
        #expect(KeyPreset.allCases.map(\.title) == ["Darkslide", "Lightroom Classic", "Capture One"])
        var b = KeyPreset.lightroomClassic.bindings
        #expect(KeyPreset.lightroomClassic.changes(in: b) == 0)
        b.bind(.toggleFilmstrip, to: "f") // displaces Hide panels: two changes
        #expect(KeyPreset.lightroomClassic.changes(in: b) == 2)
        #expect(KeyPreset.darkslide.changes(in: KeyPreset.lightroomClassic.bindings) > 0)
    }

    @MainActor @Test func choosingASetPersistsAndReloadsOnItsOwnKeys() {
        let suite = testSuiteName()
        let a = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(a.keyPreset == .darkslide && a.keyBindings == .defaults)
        a.useKeyPreset(.lightroomClassic)
        #expect(a.keyBindings == KeyPreset.lightroomClassic.bindings)
        a.keyBindings.bind(.toggleFilmstrip, to: "f")
        let b = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(b.keyPreset == .lightroomClassic)
        #expect(b.keyBindings.key(for: .toggleFilmstrip) == "f", "the user's change survives")
        #expect(b.keyBindings.command(for: "r") == .tool(.crop))
        b.useKeyPreset(.darkslide)
        #expect(b.keyBindings == .defaults, "switching sets starts from that set's keys")
    }

    @Test func savedKeysGainNewCommandsFromTheirOwnSet() {
        var d = KeyPreset.lightroomClassic.bindings.dictionary
        d["brushTool"] = nil // saved before the brush key existed
        let b = KeyBindings(dictionary: d, base: KeyPreset.lightroomClassic.bindings)
        #expect(b.command(for: "k") == .maskTool(.brush))
    }

    @MainActor @Test func newToolKeysDriveTheModel() async {
        let m = makeViewerApp()
        await m.start()
        #expect(m.photos.count >= 2)
        m.focus(m.photos[0].id, extend: false)
        m.browserMode = .grid
        #expect(CommandCatalog.isEnabled(.brushTool, model: m))
        m.perform(.maskTool(.brush))
        #expect(m.browserMode == .filmstrip && m.viewer.canvasTool == .brush)
        #expect(LayerController(app: m).layers.count == 1, "the brush needs a layer: one is added")
        m.perform(.maskTool(.brush))
        #expect(m.viewer.canvasTool == .pan, "the same key leaves the tool")

        #expect(!m.wbPickerActive)
        m.perform(.whiteBalancePicker)
        #expect(m.wbPickerActive)

        let clip = m.viewer.clippingWarning
        m.perform(.toggleClipping)
        #expect(m.viewer.clippingWarning != clip)

        m.selection = [m.photos[0].id]
        #expect(!CommandCatalog.isEnabled(.survey, model: m) || !m.surveyCandidates.isEmpty)
        m.selection = [m.photos[0].id, m.photos[1].id]
        #expect(CommandCatalog.isEnabled(.survey, model: m))
        m.perform(.survey)
        #expect(m.survey.isActive)
        m.perform(.survey)
        #expect(!m.survey.isActive)
    }

    @MainActor @Test func toolKeysDoNothingWithoutAPhoto() {
        let m = makeModel() // photos load asynchronously: nothing is focused yet
        #expect(m.focusedPhoto == nil)
        m.perform(.maskTool(.brush))
        m.perform(.whiteBalancePicker)
        #expect(m.viewer.canvasTool == .pan && !m.wbPickerActive)
        #expect(!CommandCatalog.isEnabled(.brushTool, model: m) && CommandCatalog.isEnabled(.toggleClipping, model: m))
        m.perform(.survey)
        #expect(!m.survey.isActive)
    }

    @MainActor @Test func menusListTheNewTools() {
        for c: BindableCommand in [.whiteBalanceTool, .brushTool, .eraserTool, .gradientTool, .radialTool] {
            #expect(CommandCatalog.toolMenu.contains(c))
            #expect(!c.title.isEmpty)
        }
        #expect(BindableCommand.survey.title == "Survey selected" && BindableCommand.toggleClipping.title == "Clipping warning")
    }
}
