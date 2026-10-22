import AppKit

/// Menu key equivalents (L6): ⌥⌘W is Close Session's, never the system's Close All, so
/// with Close Session disabled the keys do nothing rather than close the window (which
/// quits this one-window app).
public enum MenuKeys {
    /// The keys only the app's own items may hold, by item title.
    static let reserved: [(key: String, modifiers: NSEvent.ModifierFlags, owner: String)] = [
        ("w", [.command, .option], CommandCatalogTitles.closeSession),
        ("n", [.command, .shift], CommandCatalogTitles.newSession),
    ]

    /// Take the reserved keys off any other item (the item stays, without its key).
    public static func reserveSessionKeys(in menu: NSMenu) {
        for item in all(menu) {
            for r in reserved where matches(item, r.key, r.modifiers) && item.title != r.owner {
                item.keyEquivalent = ""
                item.keyEquivalentModifierMask = []
            }
        }
    }

    /// `menu` is `root` or one of its submenus (the main menu's notifications only).
    public static func isInside(_ menu: NSMenu, _ root: NSMenu) -> Bool {
        var m: NSMenu? = menu
        while let current = m {
            if current === root { return true }
            m = current.supermenu
        }
        return false
    }

    /// Every item with a key equivalent: "⌥⌘W  File ▸ Close Session".
    public static func dump(_ menu: NSMenu) -> [String] {
        walk(menu, path: []).compactMap { item, path in
            guard !item.keyEquivalent.isEmpty else { return nil }
            return "\(symbols(item.keyEquivalentModifierMask))\(item.keyEquivalent.uppercased())  "
                + (path + [item.title]).joined(separator: " ▸ ")
        }
    }

    /// Key equivalents held by more than one item (each listed once).
    public static func duplicates(in menu: NSMenu) -> [String] {
        var seen: [String: Int] = [:]
        for item in all(menu) where !item.keyEquivalent.isEmpty {
            seen["\(symbols(item.keyEquivalentModifierMask))\(item.keyEquivalent.lowercased())", default: 0] += 1
        }
        return seen.filter { $0.value > 1 }.map(\.key).sorted()
    }

    private static func matches(_ item: NSMenuItem, _ key: String, _ mods: NSEvent.ModifierFlags) -> Bool {
        item.keyEquivalent.lowercased() == key
            && item.keyEquivalentModifierMask.intersection(.deviceIndependentFlagsMask) == mods
    }

    private static func symbols(_ m: NSEvent.ModifierFlags) -> String {
        (m.contains(.control) ? "⌃" : "") + (m.contains(.option) ? "⌥" : "") + (m.contains(.shift) ? "⇧" : "")
            + (m.contains(.command) ? "⌘" : "")
    }

    private static func all(_ menu: NSMenu) -> [NSMenuItem] { walk(menu, path: []).map(\.0) }

    private static func walk(_ menu: NSMenu, path: [String]) -> [(NSMenuItem, [String])] {
        menu.items.flatMap { item -> [(NSMenuItem, [String])] in
            guard let sub = item.submenu else { return [(item, path)] }
            return walk(sub, path: path + [item.title])
        }
    }
}

/// The session commands' menu titles (shared with `CommandCatalog`).
enum CommandCatalogTitles {
    static let newSession = "New Session…"
    static let closeSession = "Close Session"
}
