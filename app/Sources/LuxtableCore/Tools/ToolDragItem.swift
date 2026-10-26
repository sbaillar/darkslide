import CoreTransferable
import Foundation
import UniformTypeIdentifiers

extension UTType {
    /// A tool dragged by its panel header or from Customize Tools (declared in Info.plist).
    public static let toolPanel = UTType(exportedAs: "app.luxtable.Luxtable.tool-panel")
    /// A strip tab dragged to reorder.
    public static let toolTab = UTType(exportedAs: "app.luxtable.Luxtable.tool-tab")
}

/// What the tab strip and tool stacks drag. Their own types, so a drop target for
/// tools never accepts plain text and never takes a Finder folder away from the
/// window's photo drop target.
public enum ToolDragItem: Hashable, Sendable, Transferable {
    case panel(ToolPanelID)
    case tab(ToolTab)

    public var panel: ToolPanelID? { if case .panel(let p) = self { p } else { nil } }
    public var tab: ToolTab? { if case .tab(let t) = self { t } else { nil } }

    public static var transferRepresentation: some TransferRepresentation {
        DataRepresentation(contentType: .toolPanel) { item in
            Data((item.panel?.rawValue ?? "").utf8)
        } importing: { data in
            guard let p = ToolPanelID(rawValue: String(decoding: data, as: UTF8.self)) else { throw CocoaError(.coderInvalidValue) }
            return .panel(p)
        }
        .exportingCondition { $0.panel != nil }
        DataRepresentation(contentType: .toolTab) { item in
            Data((item.tab?.rawValue ?? "").utf8)
        } importing: { data in
            guard let t = ToolTab(rawValue: String(decoding: data, as: UTF8.self)) else { throw CocoaError(.coderInvalidValue) }
            return .tab(t)
        }
        .exportingCondition { $0.tab != nil }
    }
}
