#if os(macOS)
import AppKit
#else
import UIKit
#endif
import Foundation
import UniformTypeIdentifiers

/// Modal file pickers, confirmation alerts and the pasteboard for the shared views.
///
/// iOS (unverified): these are imperative on the Mac but presentation-driven on iPad
/// (`fileImporter`, `confirmationDialog`), so the iPad branches answer "cancelled" and the
/// touch views replace the calls in phase 2. They never act without the user.
@MainActor
enum PlatformPanels {
    /// One file or folder the user picks, or nil when cancelled.
    static func chooseItem(files: Bool = true, directories: Bool = false, types: [UTType] = [],
                           createDirectories: Bool = false, prompt: String? = nil, message: String? = nil,
                           directory: URL? = nil) -> URL? {
        #if os(macOS)
        let panel = NSOpenPanel()
        panel.canChooseFiles = files
        panel.canChooseDirectories = directories
        panel.allowsMultipleSelection = false
        if !types.isEmpty { panel.allowedContentTypes = types }
        if createDirectories { panel.canCreateDirectories = true }
        if let prompt { panel.prompt = prompt }
        if let message { panel.message = message }
        if let directory { panel.directoryURL = directory }
        return panel.runModal() == .OK ? panel.url : nil
        #else
        return nil
        #endif
    }

    /// Asks before a destructive action; Return cancels. True only when the user confirms.
    static func confirmDestructive(title: String, message: String, button: String) -> Bool {
        #if os(macOS)
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = title
        alert.informativeText = message
        let confirm = alert.addButton(withTitle: button)
        confirm.hasDestructiveAction = true
        confirm.keyEquivalent = "" // Return cancels: the destructive choice is never the default
        alert.addButton(withTitle: "Cancel").keyEquivalent = "\r"
        return alert.runModal() == .alertFirstButtonReturn
        #else
        return false
        #endif
    }

    /// Puts plain text on the general pasteboard.
    static func copy(_ text: String) {
        #if os(macOS)
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        #else
        UIPasteboard.general.string = text
        #endif
    }
}
