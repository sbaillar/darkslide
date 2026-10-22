import Foundation

/// Where things sit in the main window, chosen in Settings › Familiar layout. The look
/// stays Darkslide's; only placement follows the app someone is coming from.
public enum LayoutPreset: String, CaseIterable, Codable, Sendable, Identifiable {
    case darkslide, lightroomClassic, captureOne

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .darkslide: "Darkslide"
        case .lightroomClassic: "Lightroom Classic"
        case .captureOne: "Capture One"
        }
    }

    /// One line for Settings and the first-run choice.
    public var summary: String {
        switch self {
        case .darkslide: "Tools on the right, Copy and Apply in the toolbar"
        case .lightroomClassic: "Tools on the right, Copy… and Paste below them"
        case .captureOne: "Tools on the left, Copy and Apply at the far right"
        }
    }

    /// The window side the tool panels sit on.
    public enum Side: String, Sendable { case left, right }

    /// Where Copy / Apply live.
    public enum CopyApplyPlace: Sendable {
        /// First of the toolbar's trailing items (the last to overflow).
        case toolbar
        /// The far right of the toolbar (Capture One).
        case toolbarEnd
        /// The foot of the tool panels (Lightroom Classic's Copy… / Paste).
        case panelFoot
    }

    public var toolSide: Side {
        switch self {
        case .darkslide, .lightroomClassic: .right
        case .captureOne: .left
        }
    }

    public var copyApply: CopyApplyPlace {
        switch self {
        case .darkslide: .toolbar
        case .lightroomClassic: .panelFoot
        case .captureOne: .toolbarEnd
        }
    }

    /// The names that go with this layout unless the user picks others.
    public var names: NameSet {
        switch self {
        case .darkslide: .darkslide
        case .lightroomClassic: .lightroomClassic
        case .captureOne: .captureOne
        }
    }
}

/// What a few things are called, so a renamed control is still recognisable.
public enum NameSet: String, CaseIterable, Codable, Sendable, Identifiable {
    case darkslide, lightroomClassic, captureOne

    public var id: String { rawValue }

    public var title: String { LayoutPreset(rawValue: rawValue)?.title ?? rawValue }

    /// The Copy / Apply buttons.
    public var copyButton: String { self == .lightroomClassic ? "Copy…" : "Copy" }
    public var applyButton: String { self == .lightroomClassic ? "Paste" : "Apply" }

    /// The same commands in the menus.
    public var copyMenu: String { self == .lightroomClassic ? "Copy Settings…" : "Copy Adjustments" }
    public var applyMenu: String { self == .lightroomClassic ? "Paste Settings" : "Apply Adjustments" }

    public var newVariant: String { self == .lightroomClassic ? "Create Virtual Copy" : "New Variant" }

    /// The Layers tab and panel.
    public var layers: String { self == .lightroomClassic ? "Masks" : "Layers" }

    /// The looks / styles / presets panel.
    public var styles: String {
        switch self {
        case .darkslide: "Styles & Presets"
        case .lightroomClassic: "Presets"
        case .captureOne: "Styles"
        }
    }

    /// A built-in tab's title under these names.
    public func title(of tab: ToolTab) -> String { tab == .layers ? layers : tab.title }

    /// A tool panel's title under these names.
    public func title(of panel: ToolPanelID) -> String {
        switch panel {
        case .layers: layers
        case .styles: styles
        default: panel.title
        }
    }
}
