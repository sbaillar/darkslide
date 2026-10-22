import Foundation

/// What a drop of files/folders onto the window should do (pure; the view
/// modifier just executes it).
public enum DropAction: Equatable, Sendable {
    /// Live: browse this folder read-only (import stays disabled until ds-import).
    case openFolder(URL)
    /// Demo: run the (mock) import sheet on it.
    case importSheet(URL)
    case ignore
}

public enum DropRouter {
    /// Folders are used as-is; dropped files mean "their folder". The first
    /// usable URL wins. `isDirectory` is injected for tests.
    public static func route(_ urls: [URL], mode: AppMode, canOpenFolders: Bool,
                             isDirectory: (URL) -> Bool = DropRouter.isDirectory) -> DropAction {
        guard let first = urls.first(where: \.isFileURL) else { return .ignore }
        let folder = isDirectory(first) ? first : first.deletingLastPathComponent()
        switch mode {
        case .live: return canOpenFolders ? .openFolder(folder) : .ignore
        case .demo: return .importSheet(folder)
        }
    }

    public static func isDirectory(_ url: URL) -> Bool {
        (try? url.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) ?? false
    }
}
