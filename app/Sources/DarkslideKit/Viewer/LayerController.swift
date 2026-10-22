import Foundation

/// Layer operations for the Layers panel (M10: logic out of the view).
///
/// - One selection: `viewer.selectedLayerID`, tied to the focused photo and
///   reset on navigation (N9). Adding or duplicating selects the new layer.
/// - Every change targets the **focused photo only** (`updateEdits([id])`),
///   never the whole selection, even with Edit All Selected on (N10): layer ids
///   are per-photo, so "delete layer 2" on another photo would delete an
///   unrelated layer.
@MainActor
public struct LayerController {
    let app: AppModel

    public init(app: AppModel) { self.app = app }

    private var viewer: ViewerModel { app.viewer }

    public var layers: [LayerEdit] { app.focusedID.map { app.edit(for: $0).layers } ?? [] }
    public var selected: Int? { viewer.selectedLayerID }

    public func select(_ id: Int?) { viewer.selectedLayerID = id }

    private func edit(_ label: String, key: String? = nil, _ change: (inout EditState) -> Void) {
        guard let id = app.focusedID else { return }
        app.updateEdits([id], label: label, coalesceKey: key) { _, e in change(&e) }
    }

    /// Add a layer and select it. Returns its id.
    @discardableResult
    public func add() -> Int? {
        guard app.focusedID != nil else { return nil }
        var added: Int?
        edit("Add Layer") { e in
            let l = LayerOps.newLayer(after: e.layers)
            added = l.id
            e.layers.append(l)
        }
        select(added)
        return added
    }

    @discardableResult
    public func duplicateSelected() -> Int? {
        guard let id = selected else { return nil }
        var copy: Int?
        edit("Duplicate Layer") { e in copy = LayerOps.duplicate(id, in: &e.layers) }
        select(copy)
        return copy
    }

    public func deleteSelected() {
        guard let id = selected else { return }
        edit("Delete Layer") { LayerOps.remove(id, from: &$0.layers) }
        select(nil)
    }

    public func rename(_ layer: Int, to name: String) {
        edit("Rename Layer") { LayerOps.rename(layer, to: name, in: &$0.layers) }
    }

    /// Change one layer; `key` coalesces a slider drag into one undo step.
    public func update(_ layer: Int, label: String, key: String? = nil, _ change: (inout LayerEdit) -> Void) {
        edit(label, key: key) { e in
            if let i = e.layers.firstIndex(where: { $0.id == layer }) { change(&e.layers[i]) }
        }
    }

    /// Choose a mask tool: make sure a layer is selected (adding one if needed),
    /// then toggle the canvas tool.
    public func chooseTool(_ tool: CanvasTool) {
        if selected == nil { add() }
        viewer.canvasTool = viewer.canvasTool == tool ? .pan : tool
    }

    /// Run an AI mask on the selected layer (or a new one) and select it.
    public func addAIMask(_ target: AITarget, masker: AIMasker = .vision) async throws {
        let layer = try await app.addAIMask(target, toLayer: selected, masker: masker)
        select(layer)
    }

    /// The selected layer, if any.
    public var selectedLayer: LayerEdit? { selected.flatMap { id in layers.first { $0.id == id } } }

    /// Change one of the selected layer's tools (a slider drag is one undo step).
    public func updateTools(label: String, key: String, _ change: (inout LayerToolsEdit) -> Void) {
        guard let id = selected else { return }
        update(id, label: label, key: "\(key)-\(id)") { change(&$0.tools) }
    }

    /// Turn the luminance / colour range refinement of the selected layer's mask on or off.
    public func setLumaRange(_ on: Bool) {
        guard let id = selected else { return }
        update(id, label: on ? "Luma Range" : "Remove Luma Range") { $0.mask.lumaRange = on ? LumaRangeEdit() : nil }
    }

    public func clearColorRange() {
        guard let id = selected else { return }
        update(id, label: "Remove Color Range") { $0.mask.colorRange = nil }
    }

    /// Accessibility label for a layer row.
    public static func accessibilityLabel(_ l: LayerEdit, selected: Bool) -> String {
        var parts = [l.name, l.enabled ? "visible" : "hidden", "opacity \(Int((l.opacity * 100).rounded())) percent",
                     LayerOps.maskSummary(l.mask)]
        if selected { parts.append("selected") }
        return parts.joined(separator: ", ")
    }
}
