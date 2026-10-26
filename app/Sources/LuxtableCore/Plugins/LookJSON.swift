import Foundation

/// Styles ▸ Copy as Look JSON: the focused photo's edit as one item of a looks pack
/// (`look@1`, docs/PLUGINS.md), to paste into a plug-in's `looks.json`. A look holds
/// amounts only (so its Intensity scales it exactly): curves, colour ranges, skin tone,
/// switches, the crop and layers are left out, and `leftOut` says which the edit had.
public enum LookJSON {
    /// One look item, pretty-printed with sorted keys.
    public static func item(from edit: EditState, name: String) -> String {
        let identity = EditState()
        var values: [String: Double] = [:]
        for (key, path) in EditCodec.scalars where key != "crop.angle" {
            let v = edit[keyPath: path]
            if v != identity[keyPath: path] { values[key] = v }
        }
        let object: [String: Any] = ["id": id(for: name), "name": name, "summary": "", "values": values]
        let data = (try? JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys])) ?? Data()
        return String(data: data, encoding: .utf8) ?? "{}"
    }

    /// What the edit has that a look can't carry ("curves", "layers"…), for the note.
    public static func leftOut(_ edit: EditState) -> [String] {
        let identity = EditState()
        var out: [String] = []
        if edit.curve.points.values.contains(where: { !$0.isEmpty }) { out.append("curves") }
        if !edit.colorEditor.advanced.isEmpty || edit.colorEditor.skin != nil { out.append("colour ranges") }
        if EditCodec.flags.contains(where: { edit[keyPath: $0.1] != identity[keyPath: $0.1] }) { out.append("switches") }
        if edit.crop != identity.crop { out.append("the crop") }
        if !edit.layers.isEmpty { out.append("layers") }
        return out
    }

    /// `warm-fade` from "Warm Fade!".
    static func id(for name: String) -> String {
        let parts = name.lowercased().map { $0.isASCII && ($0.isLetter || $0.isNumber) ? $0 : "-" }
        let id = String(parts).split(separator: "-").joined(separator: "-")
        return id.isEmpty ? "look" : String(id.prefix(64))
    }
}
