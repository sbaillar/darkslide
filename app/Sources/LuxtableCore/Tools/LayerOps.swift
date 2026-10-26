import Foundation

/// Layer-list operations for the Layers panel (pure; applied through `updateEdit`).
public enum LayerOps {
    /// A new layer with the next free id and a unique "Layer N" name.
    public static func newLayer(after layers: [LayerEdit], name: String? = nil) -> LayerEdit {
        LayerEdit(id: nextID(layers), name: uniqueName(name ?? "Layer", in: layers, numberFirst: name == nil))
    }

    static func nextID(_ layers: [LayerEdit]) -> Int { (layers.map(\.id).max() ?? -1) + 1 }

    /// "Layer 3" style (numberFirst) or "Subject", "Subject 2".
    static func uniqueName(_ base: String, in layers: [LayerEdit], numberFirst: Bool) -> String {
        let names = Set(layers.map(\.name))
        if numberFirst {
            var n = layers.count + 1
            while names.contains("\(base) \(n)") { n += 1 }
            return "\(base) \(n)"
        }
        guard names.contains(base) else { return base }
        var n = 2
        while names.contains("\(base) \(n)") { n += 1 }
        return "\(base) \(n)"
    }

    /// Append a copy of layer `id` (new id, "… copy" name). Returns the new id.
    @discardableResult
    public static func duplicate(_ id: Int, in layers: inout [LayerEdit]) -> Int? {
        guard let src = layers.first(where: { $0.id == id }) else { return nil }
        var copy = src
        copy.id = nextID(layers)
        copy.name = uniqueName("\(src.name) copy", in: layers, numberFirst: false)
        layers.append(copy)
        return copy.id
    }

    public static func remove(_ id: Int, from layers: inout [LayerEdit]) {
        layers.removeAll { $0.id == id }
    }

    /// Rename; blank names are ignored.
    public static func rename(_ id: Int, to name: String, in layers: inout [LayerEdit]) {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty, let i = layers.firstIndex(where: { $0.id == id }) else { return }
        layers[i].name = trimmed
    }

    /// Add a mask shape to layer `id`, creating a named layer when `id` is nil or missing.
    /// Returns the layer id that received the shape.
    @discardableResult
    public static func addShape(_ shape: MaskShape, toLayer id: Int?, newLayerName: String,
                                in layers: inout [LayerEdit]) -> Int {
        if let id, let i = layers.firstIndex(where: { $0.id == id }) {
            layers[i].mask.shapes.append(shape)
            return id
        }
        var layer = newLayer(after: layers, name: newLayerName)
        layer.mask.shapes = [shape]
        layers.append(layer)
        return layer.id
    }

    /// Short description of a mask for the layer row ("Subject + Brush").
    public static func maskSummary(_ mask: MaskEdit) -> String {
        guard !mask.shapes.isEmpty else { return "No mask (whole image)" }
        let parts = mask.shapes.map { shape -> String in
            switch shape {
            case .fill: "Fill"
            case .brush(_, let strokes): "Brush (\(strokes.count))"
            case .linear: "Linear"
            case .radial: "Radial"
            case .ai(_, let t): t.rawValue.capitalized
            case .aiPerson(_, let t, let p): PeopleTargets.label(t, person: p)
            }
        }
        return parts.joined(separator: " + ") + (mask.invert ? ", inverted" : "")
    }
}
