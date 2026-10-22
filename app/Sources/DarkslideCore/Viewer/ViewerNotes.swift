import Foundation

/// Honest HUD wording about what the viewer shows (N11, L2, demo note).
public enum ViewerNotes {
    public struct Note: Equatable, Sendable {
        public let text: String
        public let help: String
    }

    /// Note for edits the renderer can't show, or nil when everything shown is rendered.
    public static func editsNotShown(_ groups: Set<AdjustGroup>, capabilities c: RendererCapabilities) -> Note? {
        guard !groups.isEmpty else { return nil }
        if !c.rendersEdits {
            return Note(text: "Edits saved, not shown in this preview",
                        help: "The viewer shows the camera's embedded JPEG. Your adjustments are kept and render when the full develop is available.")
        }
        let names = AdjustGroup.allCases.filter(groups.contains).map(\.title)
        let list = names.count > 2 ? "\(names.prefix(2).joined(separator: ", ")) +\(names.count - 2)" : names.joined(separator: ", ")
        return Note(text: "Not shown yet: \(list)",
                    help: "The test renderer only shows exposure, saturation and white balance. \(names.joined(separator: ", ")) are saved and render in the full develop.")
    }

    /// Note under the styles list: whether hovering a style previews it.
    public static func stylesHover(capabilities c: RendererCapabilities) -> String {
        c.rendersEdits
            ? "Click to apply; styles stack. Hover to preview. Styles are stored on this Mac."
            : "Click to apply; styles stack. Hover previews need the full develop. Styles are stored on this Mac."
    }
}
