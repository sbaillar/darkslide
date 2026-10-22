import AppKit
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@Suite struct KeyboardRouterTests {
    typealias K = KeyInput

    @Test func cullingKeys() {
        #expect(KeyboardRouter.command(for: K("0")) == .rate(0))
        for n in 1...5 { #expect(KeyboardRouter.command(for: K("\(n)")) == .rate(n)) }
        #expect(KeyboardRouter.command(for: K("6")) == .tag(.red))
        #expect(KeyboardRouter.command(for: K("7")) == .tag(.yellow))
        #expect(KeyboardRouter.command(for: K("8")) == .tag(.green))
        #expect(KeyboardRouter.command(for: K("9")) == .tag(.blue))
        #expect(KeyboardRouter.command(for: K("-")) == .tag(.purple))
        #expect(KeyboardRouter.command(for: K("p")) == .pick)
        #expect(KeyboardRouter.command(for: K("P", shift: true)) == .pick)
        #expect(KeyboardRouter.command(for: K("x")) == .reject)
        #expect(KeyboardRouter.command(for: K("u")) == .unflag)
    }

    @Test func viewKeys() {
        #expect(KeyboardRouter.command(for: K(" ", special: .space)) == .zoomToggleAtCursor)
        #expect(KeyboardRouter.command(for: K(" ", special: .space, shift: true)) == nil)
        #expect(KeyboardRouter.command(for: K("f")) == .togglePanels)
        #expect(KeyboardRouter.command(for: K("", special: .tab)) == .togglePanels)
        #expect(KeyboardRouter.command(for: K("", special: .tab, shift: true)) == nil)
        #expect(KeyboardRouter.command(for: K("g")) == .gridMode)
        #expect(KeyboardRouter.command(for: K("e")) == .viewerMode)
        #expect(KeyboardRouter.command(for: K("", special: .returnKey)) == .viewerMode)
        #expect(KeyboardRouter.command(for: K("y")) == .toggleBeforeAfter)
        // [cmp9] C crops (Capture One); pin-compare moved to K.
        #expect(KeyboardRouter.command(for: K("k")) == .togglePinCompare)
        #expect(KeyboardRouter.command(for: K("C", shift: true)) == .tool(.crop), "shift is ignored")
        #expect(KeyboardRouter.command(for: K("?", shift: true)) == .showCheatSheet)
        #expect(KeyboardRouter.command(for: K("A", shift: true)) == .auto)
        #expect(KeyboardRouter.command(for: K("r")) == .tool(.straighten))
        #expect(KeyboardRouter.command(for: K("a")) == nil)
        #expect(KeyboardRouter.command(for: K("", special: .escape)) == .escape)
    }

    @Test func arrows() {
        #expect(KeyboardRouter.command(for: K("", special: .left)) == .move(.left, extend: false))
        #expect(KeyboardRouter.command(for: K("", special: .right, shift: true)) == .move(.right, extend: true))
        #expect(KeyboardRouter.command(for: K("", special: .up)) == .move(.up, extend: false))
        #expect(KeyboardRouter.command(for: K("", special: .down)) == .move(.down, extend: false))
        #expect(KeyboardRouter.command(for: K("", special: .left, command: true)) == nil)
        #expect(KeyboardRouter.command(for: K("", special: .left, option: true)) == nil)
    }

    @Test func commandShortcuts() {
        #expect(KeyboardRouter.command(for: K("c", command: true, shift: true)) == .copyAdjustments)
        #expect(KeyboardRouter.command(for: K("v", command: true, shift: true)) == .applyAdjustments)
        #expect(KeyboardRouter.command(for: K("v", command: true, option: true)) == .copyFromPrevious)
        #expect(KeyboardRouter.command(for: K("z", command: true)) == .undo)
        #expect(KeyboardRouter.command(for: K("Z", command: true, shift: true)) == .redo)
        #expect(KeyboardRouter.command(for: K("a", command: true)) == .selectAll)
        #expect(KeyboardRouter.command(for: K("c", command: true)) == nil, "plain ⌘C stays with the system")
        #expect(KeyboardRouter.command(for: K("q", command: true)) == nil)
    }

    @Test func ignoredKeys() {
        #expect(KeyboardRouter.command(for: K("1", control: true)) == nil)
        #expect(KeyboardRouter.command(for: K("p", option: true)) == nil)
        #expect(KeyboardRouter.command(for: K("q")) == nil)
        #expect(KeyboardRouter.command(for: K("ab")) == nil)
        #expect(KeyboardRouter.command(for: K("")) == nil)
    }

    @Test func cheatSheetCoversEverySection() {
        #expect(KeyboardRouter.cheatSheet().map(\.section) == ["Navigate", "Cull", "View", "Compose", "Edit", "Voice"])
        #expect(KeyboardRouter.cheatSheet().allSatisfy { !$0.rows.isEmpty })
    }

    @Test func colorTagKeysRoundTrip() {
        for tag in ColorTag.allCases {
            if let key = tag.key {
                #expect(ColorTag.forKey(key) == tag)
            } else {
                #expect([ColorTag.none, .orange, .pink].contains(tag), "only none, orange and pink have no default key")
            }
            #expect(!tag.name.isEmpty)
        }
        #expect(ColorTag.forKey("z") == nil)
    }
}

@Suite struct KeyMonitorPolicyTests {
    @Test func focusTrapRules() {
        let p = KeyInput("p")
        #expect(KeyMonitorPolicy.shouldHandle(p, textEditing: false, sheetOpen: false, isMainWindow: true) == .pick)
        #expect(KeyMonitorPolicy.shouldHandle(p, textEditing: true, sheetOpen: false, isMainWindow: true) == nil,
                "typing in a text field never rates/culls")
        #expect(KeyMonitorPolicy.shouldHandle(p, textEditing: false, sheetOpen: true, isMainWindow: true) == nil)
        #expect(KeyMonitorPolicy.shouldHandle(p, textEditing: false, sheetOpen: false, isMainWindow: false) == nil)
        let undo = KeyInput("z", command: true)
        #expect(KeyMonitorPolicy.shouldHandle(undo, textEditing: false, sheetOpen: false, isMainWindow: true) == nil,
                "⌘ shortcuts go through the menu bar")
    }

    @MainActor
    @Test func keyInputFromNSEvent() throws {
        func event(_ chars: String, _ ignoring: String, code: UInt16, flags: NSEvent.ModifierFlags = []) throws -> NSEvent {
            try #require(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
                                          windowNumber: 0, context: nil, characters: chars,
                                          charactersIgnoringModifiers: ignoring, isARepeat: false, keyCode: code))
        }
        #expect(KeyInput(event: try event("3", "3", code: 20)) == KeyInput("3"))
        #expect(KeyInput(event: try event("", "", code: 123)).special == .left)
        #expect(KeyInput(event: try event("", "", code: 124, flags: .shift)) == KeyInput("", special: .right, shift: true))
        #expect(KeyInput(event: try event("", "", code: 125)).special == .down)
        #expect(KeyInput(event: try event("", "", code: 126)).special == .up)
        #expect(KeyInput(event: try event(" ", " ", code: 49)).special == .space)
        #expect(KeyInput(event: try event("\u{1b}", "\u{1b}", code: 53)).special == .escape)
        #expect(KeyInput(event: try event("\r", "\r", code: 36)).special == .returnKey)
        let cmd = KeyInput(event: try event("\u{3}", "c", code: 8, flags: [.command, .shift]))
        #expect(cmd == KeyInput("c", command: true, shift: true))
        #expect(KeyboardRouter.command(for: cmd) == .copyAdjustments)
        let opt = KeyInput(event: try event("√", "v", code: 9, flags: [.command, .option, .control]))
        #expect(opt.option && opt.control && opt.command && opt.characters == "v")
    }
}
