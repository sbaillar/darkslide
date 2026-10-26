import CoreGraphics
import Foundation

/// A content-aware removal (lt-core `Removal`): brush strokes in the oriented frame
/// (normalized; `size` = diameter relative to the long edge) whose area Rust fills
/// (the Core ML LaMa model, or PatchMatch until it is ready). Applied in order.
public struct RemovalEdit: Equatable, Sendable, Identifiable {
    public var id = UUID()
    public var strokes: [BrushStroke]
    /// Soft edge of the fill, long-edge units (0...`maxFeather`).
    public var feather: Double

    /// lt-core `MAX_REMOVAL_FEATHER`.
    public static let maxFeather = 0.02
    public static let defaultFeather = 0.002

    public init(strokes: [BrushStroke], feather: Double = RemovalEdit.defaultFeather) {
        self.strokes = strokes
        self.feather = min(max(feather, 0), Self.maxFeather)
    }

    /// Equality ignores the view identity.
    public static func == (a: RemovalEdit, b: RemovalEdit) -> Bool {
        a.strokes == b.strokes && a.feather == b.feather
    }
}

/// The people sub-part targets (lt-core `AiTarget::is_people`) and their labels.
public enum PeopleTargets {
    /// Targets with a person picker, in menu order.
    public static let parts: [AITarget] = [.person, .face, .skin, .eyes, .teeth, .hair, .clothes]

    /// Whether `target` is masked per person (Person and the six sub-parts).
    public static func isPeople(_ target: AITarget) -> Bool {
        parts.contains(target)
    }

    /// "Hair", or "Hair (Person 2)" for person index 1.
    public static func label(_ target: AITarget, person: Int?) -> String {
        let name = target.rawValue.capitalized
        guard let person else { return name }
        return "\(name) (Person \(person + 1))"
    }

    /// The mask shape for a menu choice: all people (`person == nil`) or one of them.
    /// Non-people targets ignore `person`.
    public static func shape(_ target: AITarget, person: Int?, op: MaskOp = .add) -> MaskShape {
        if let person, isPeople(target) { return .aiPerson(op, target, person: max(0, person)) }
        return .ai(op, target)
    }
}

extension AdjustmentsCodec {
    /// An `ai` shape: `person` (a people target for one person) makes it `.aiPerson`.
    static func aiShape(_ s: JSON, op o: MaskOp) -> MaskShape? {
        guard let t = AITarget(rawValue: s["target"] as? String ?? "") else { return nil }
        if let p = num(s["person"]), p >= 0, PeopleTargets.isPeople(t) { return .aiPerson(o, t, person: Int(p)) }
        return .ai(o, t)
    }

    /// `person_boxes`: `{"1": [x0, y0, x1, y1]}` → index → normalized rect.
    static func personBoxes(_ v: Any?) -> [Int: CGRect] {
        var out: [Int: CGRect] = [:]
        for (k, b) in (v as? JSON) ?? [:] {
            guard let i = Int(k), let c = (b as? [Any])?.compactMap({ num($0) }), c.count == 4, c[2] > c[0], c[3] > c[1] else { continue }
            out[i] = CGRect(x: c[0], y: c[1], width: c[2] - c[0], height: c[3] - c[1])
        }
        return out
    }

    /// The boxes of the people the edit's per-person shapes refer to (nil when none).
    static func personBoxesJSON(_ e: EditState) -> JSON? {
        var used = Set<Int>()
        for layer in e.layers {
            for case .aiPerson(_, _, let p) in layer.mask.shapes { used.insert(p) }
        }
        let out = e.personBoxes.filter { used.contains($0.key) }
        guard !out.isEmpty else { return nil }
        return Dictionary(uniqueKeysWithValues: out.map { i, r in
            (String(i), [Double(r.minX), Double(r.minY), Double(r.maxX), Double(r.maxY)] as Any)
        })
    }

    static func stroke(_ st: JSON) -> BrushStroke {
        BrushStroke(points: points(st["points"]), size: num(st["size"]) ?? 0.05, hardness: num(st["hardness"]) ?? 0.5,
                    flow: num(st["flow"]) ?? 1, erase: st["erase"] as? Bool ?? false,
                    autoMask: st["auto_mask"] as? Bool ?? false)
    }

    static func strokeJSON(_ st: BrushStroke) -> JSON {
        ["points": st.points.map { [$0.x, $0.y] }, "size": st.size, "hardness": st.hardness,
         "flow": st.flow, "erase": st.erase, "auto_mask": st.autoMask]
    }

    static func removals(_ v: Any?) -> [RemovalEdit] {
        ((v as? [Any]) ?? []).compactMap { $0 as? JSON }.map { r in
            RemovalEdit(strokes: ((r["strokes"] as? [Any]) ?? []).compactMap { $0 as? JSON }.map(stroke),
                        feather: num(r["feather"]) ?? RemovalEdit.defaultFeather)
        }
    }

    /// Removals over the stored ones: a removal the app kept as it was keeps its JSON
    /// (fields newer builds may add); changed or new ones are written from the edit.
    static func removalsJSON(_ edits: [RemovalEdit], onto base: Any?) -> [JSON] {
        var unused = ((base as? [Any]) ?? []).compactMap { $0 as? JSON }
        return edits.map { r in
            if let i = unused.firstIndex(where: { removals([$0]).first == r }) { return unused.remove(at: i) }
            return ["strokes": r.strokes.map(strokeJSON), "feather": r.feather]
        }
    }
}
