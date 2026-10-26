import AppKit
import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

@Suite struct KeyboardBindingTests {
    /// B hides just the filmstrip (final review: a taller viewer on small screens).
    @MainActor @Test func bHidesTheFilmstrip() {
        #expect(KeyBindings.defaults.command(for: "b") == .toggleFilmstrip)
        let m = makeModel()
        #expect(!m.filmstripHidden && CommandCatalog.isEnabled(.toggleFilmstrip, model: m))
        m.perform(.toggleFilmstrip)
        #expect(m.filmstripHidden && !m.panelsHidden)
        m.browserMode = .grid
        #expect(!CommandCatalog.isEnabled(.toggleFilmstrip, model: m), "no filmstrip in the grid")
        #expect(CommandCatalog.viewMenu.contains(.toggleFilmstrip))
    }

    @Test func savedBindingsGainNewDefaultCommands() {
        var d = KeyBindings.defaults.dictionary
        d["toggleStack"] = nil // saved by a build before toggleStack existed
        let b = KeyBindings(dictionary: d)
        #expect(b.command(for: "s") == .toggleStack, "N15: new commands get their default key")
    }

    @Test func explicitUnbindsSurviveAndUserKeysWin() {
        var b = KeyBindings.defaults
        b.unbind(.unflag)
        #expect(b.dictionary["unflag"] == "", "unbound is stored explicitly")
        let back = KeyBindings(dictionary: b.dictionary)
        #expect(back.key(for: .unflag) == nil && back == b)
        // A user who put Pick on "s" before toggleStack existed keeps it; toggleStack stays unbound.
        let old = KeyBindings(dictionary: ["pick": "s"])
        #expect(old.command(for: "s") == .pick)
        #expect(old.key(for: .toggleStack) == nil)
        #expect(old.key(for: .reject) == "x", "everything else defaults")
        #expect(KeyBindings(dictionary: ["pick": "pp", "nope": "q"]) == .defaults, "invalid entries are ignored")
        #expect(KeyBindings(dictionary: ["pick": "P"]).key(for: .pick) == "p")
    }

    @Test func recordingRejectsFixedAndModifiedKeys() {
        typealias K = KeyInput
        #expect(KeyBindings.recordable(K("", special: .escape)) == .failure(.fixedKey("Esc")))
        #expect(KeyBindings.recordable(K("\t", special: .tab)) == .failure(.fixedKey("Tab")))
        #expect(KeyBindings.recordable(K("", special: .returnKey)) == .failure(.fixedKey("Return")))
        #expect(KeyBindings.recordable(K(" ", special: .space)) == .failure(.fixedKey("Space")))
        #expect(KeyBindings.recordable(K("", special: .left)) == .failure(.fixedKey("An arrow key")))
        #expect(KeyBindings.recordable(K("p", command: true)) == .failure(.modified))
        #expect(KeyBindings.recordable(K("p", option: true)) == .failure(.modified))
        #expect(KeyBindings.recordable(K("?", shift: true)) == .failure(.reserved("?")))
        #expect(KeyBindings.recordable(K("ab")) == .failure(.notASingleKey))
        #expect(KeyBindings.recordable(K("\u{7f}")) == .failure(.notASingleKey))
        #expect(KeyBindings.recordable(K("K", shift: true)) == .success("k"))
        for r in [KeyBindings.Rejection.fixedKey("Tab"), .modified, .reserved("?"), .notASingleKey] {
            #expect(!r.message.isEmpty)
        }
    }

    @MainActor
    @Test func recorderUsesKeyPositionOnAZERTY() throws {
        // AZERTY: the "1" key sends "&"; the monitor reads it as "1", so the binding must too.
        let e = try #require(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                                              windowNumber: 0, context: nil, characters: "&",
                                              charactersIgnoringModifiers: "&", isARepeat: false, keyCode: 18))
        let out = ShortcutRecorder.record(KeyInput(event: e), for: .pick, in: .defaults)
        #expect(out.bindings.key(for: .pick) == "1")
        #expect(out.note == "“1” moved from Rate 1 star." && out.done)
        #expect(out.bindings.command(for: "1") == .pick)
    }

    @Test func recorderOutcomes() {
        let esc = ShortcutRecorder.record(KeyInput("", special: .escape), for: .pick, in: .defaults)
        #expect(esc.bindings == .defaults && esc.note == nil && esc.done, "Esc cancels")
        let tab = ShortcutRecorder.record(KeyInput("\t", special: .tab), for: .pick, in: .defaults)
        #expect(tab.bindings == .defaults && tab.note?.contains("Tab") == true)
        let free = ShortcutRecorder.record(KeyInput("q"), for: .pick, in: .defaults)
        #expect(free.bindings.key(for: .pick) == "q" && free.note == nil)
        #expect(ShortcutRecorder.keyTitle(.pick, in: .defaults) == "P")
        var b = KeyBindings.defaults
        b.unbind(.pick)
        #expect(ShortcutRecorder.keyTitle(.pick, in: b) == "—")
    }

    @Test func cheatSheetShowsLiveBindings() {
        var b = KeyBindings.defaults
        let sheet = KeyboardRouter.cheatSheet(bindings: b)
        #expect(sheet.map(\.section) == ["Navigate", "Cull", "View", "Compose", "Edit", "Voice"])
        let rows = sheet.flatMap(\.rows)
        #expect(rows.contains { $0.keys == "P / X / U" })
        #expect(rows.contains { $0.keys == "1 2 3 4 5" })
        b.bind(.pick, to: "k")
        b.unbind(.toggleStack)
        let rows2 = KeyboardRouter.cheatSheet(bindings: b).flatMap(\.rows)
        #expect(rows2.contains { $0.keys == "K / X / U" }, "rebinding shows in the sheet")
        #expect(rows2.contains { $0.keys == "—" && $0.action == "Expand / collapse stack" })
        #expect(!rows2.contains { $0.action.contains("image") }, "photos, not images")
    }

    @Test func hintsAndHelp() {
        var b = KeyBindings.defaults
        #expect(b.hint(.pick) == "(P)" && b.help("Pick", .pick) == "Pick (P)")
        b.unbind(.pick)
        #expect(b.hint(.pick).isEmpty && b.help("Pick", .pick) == "Pick")
        #expect(KeyBindings.display("-") == "-")
    }

    @Test func tabNeverTrapsKeyboardUsers() {
        let tab = KeyInput("\t", special: .tab)
        #expect(KeyMonitorPolicy.shouldHandle(tab, focus: .control, sheetOpen: false, isMainWindow: true) == nil,
                "a focused control passes Tab on to the next control")
        #expect(KeyMonitorPolicy.shouldHandle(tab, focus: .none, sheetOpen: false, isMainWindow: true,
                                              fullKeyboardAccess: true) == nil,
                "with Full Keyboard Access, Tab moves focus")
        #expect(KeyMonitorPolicy.shouldHandle(tab, focus: .none, sheetOpen: false, isMainWindow: true) == .togglePanels)
        #expect(KeyMonitorPolicy.shouldHandle(KeyInput("", special: .escape), focus: .control, sheetOpen: false,
                                              isMainWindow: true) == .escape)
    }
}
