import Foundation

/// The app's window scenes.
public enum AppWindows {
    public static let main = "main"
    /// Tools ▸ Customize Tools….
    public static let customizeTools = "customize-tools"

    /// Whether an `NSWindow.identifier` is the main window's (SwiftUI names a
    /// scene's windows `<scene id>-AppWindow-<n>`).
    public static func isMain(_ identifier: String?) -> Bool {
        guard let identifier else { return false }
        return identifier == main || identifier.hasPrefix(main + "-")
    }
}
