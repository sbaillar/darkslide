import Foundation

/// A saved partial edit (a C1 style / LR preset): which groups it sets and how.
///
/// **Non-destructive by construction** (M9): applying a preset
/// - adds its tone groups (`relative`: white balance, exposure, HDR) as deltas, so a
///   style never resets the photo's own correction (+1 EV stays +1 EV);
/// - in every other group it contains, sets only the fields the preset changes from
///   the identity (a curve channel it draws, a colour band it moves, the clarity it
///   sets…); everything else the photo has stays;
/// - `absoluteFields` (`EditCodec` keys) are set as-is even inside a relative group,
///   for looks that need an exact value (a black & white style sets saturation −100).
///
/// Presets stack: applying several in order composes them (deltas add, later fields win).
public struct Preset: Identifiable, Equatable, Sendable {
    public var id: UUID
    public var name: String
    public var groups: Set<AdjustGroup>
    /// Groups added as deltas (white balance/exposure/HDR only), so presets stack.
    public var relative: Set<AdjustGroup>
    /// Fields set exactly even inside a relative group (`EditCodec` keys).
    public var absoluteFields: Set<String>
    public var edit: EditState
    public var isBuiltIn: Bool
    /// Section in the Styles panel ("Black & White", "Film", …); user presets have none.
    public var category: String?
    /// Apply automatically to new imports (at most one preset has this set).
    public var autoApplyOnImport: Bool

    public init(id: UUID = UUID(), name: String, groups: Set<AdjustGroup>, relative: Set<AdjustGroup> = [],
                absoluteFields: Set<String> = [], edit: EditState, isBuiltIn: Bool = false, category: String? = nil,
                autoApplyOnImport: Bool = false) {
        self.id = id
        self.name = name
        // Layers and spots are per-image; presets never carry them.
        let kept = groups.subtracting([.layers, .spots])
        self.groups = kept
        self.relative = relative.filter(\.supportsRelative).intersection(kept)
        self.absoluteFields = absoluteFields.filter { k in EditCodec.group(of: k).map(kept.contains) ?? false }
        self.edit = edit
        self.isBuiltIn = isBuiltIn
        self.category = category
        self.autoApplyOnImport = autoApplyOnImport
    }

    public var options: PasteOptions { PasteOptions(groups: groups, relative: relative) }

    /// `target` with this preset applied (see the type docs).
    public func apply(to target: EditState) -> EditState {
        var out = target
        if !relative.isEmpty {
            out = AdjustmentClipboard(source: edit).apply(to: out, options: PasteOptions(groups: relative, relative: relative))
        }
        let identity = EditState()
        let fieldGroups = groups.subtracting(relative)
        var set = Set<String>()
        for (key, path) in EditCodec.scalars {
            guard let g = EditCodec.group(of: key) else { continue }
            let exact = absoluteFields.contains(key)
            guard exact || fieldGroups.contains(g) else { continue }
            let v = edit[keyPath: path]
            if exact || v != identity[keyPath: path] {
                out[keyPath: path] = v
                set.insert(key)
            }
        }
        out.look = out.look?.dropping(set.contains) // the style owns the fields it sets
        for (key, path) in EditCodec.flags {
            guard let g = EditCodec.group(of: key) else { continue }
            let exact = absoluteFields.contains(key)
            guard exact || fieldGroups.contains(g) else { continue }
            let v = edit[keyPath: path]
            if exact || v != identity[keyPath: path] { out[keyPath: path] = v }
        }
        if fieldGroups.contains(.curve) {
            for (ch, pts) in edit.curve.points where !pts.isEmpty { out.curve.points[ch] = pts }
        }
        if fieldGroups.contains(.colorEditor) {
            if !edit.colorEditor.advanced.isEmpty { out.colorEditor.advanced = edit.colorEditor.advanced }
            if let skin = edit.colorEditor.skin { out.colorEditor.skin = skin }
        }
        if fieldGroups.contains(.crop), edit.crop.aspect != identity.crop.aspect { out.crop.aspect = edit.crop.aspect }
        return out
    }

    /// Apply several presets in order (stacking: deltas add, later fields win).
    public static func apply(_ stack: [Preset], to target: EditState) -> EditState {
        stack.reduce(target) { $1.apply(to: $0) }
    }

    /// Groups a user preset adds as deltas by default (non-destructive).
    public static func defaultRelative(for groups: Set<AdjustGroup>) -> Set<AdjustGroup> {
        groups.filter(\.supportsRelative).subtracting([.whiteBalance])
    }

    /// "Exposure, Curve" for the row's help text.
    public var summary: String {
        groups.sorted { $0.order < $1.order }.map(\.title).joined(separator: ", ")
    }
}

extension AdjustGroup {
    /// Position in `ds_core::AdjustGroup::ALL`.
    var order: Int { AdjustGroup.allCases.firstIndex(of: self) ?? 0 }
}
