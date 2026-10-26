import CoreGraphics
import Foundation

/// `effects` and `look` (lt-core `effects.rs`), decoded and overlaid losslessly like
/// the rest of the sidecar: keys `EditState` doesn't model are written back as they were.
extension AdjustmentsCodec {
    static func effects(_ v: Any?) -> EffectsEdit {
        var e = EffectsEdit()
        guard let fx = v as? JSON else { return e }
        if let lb = fx["lens_blur"] as? JSON {
            let d = LensBlurEdit()
            e.lensBlur.amount = num(lb["amount"]) ?? d.amount
            e.lensBlur.mode = lb["mode"].map { ($0 as? String).map(BlurMode.read) ?? .newer } ?? d.mode
            e.lensBlur.focus = point(lb["focus"])
            e.lensBlur.focalRange = num(lb["focal_range"]) ?? d.focalRange
            e.lensBlur.highlightBoost = num(lb["highlight_boost"]) ?? d.highlightBoost
            e.lensBlur.shape = BokehShape(rawValue: lb["shape"] as? String ?? "") ?? d.shape
            if let b = lb["plane"] as? JSON {
                e.lensBlur.band.center = point(b["center"]) ?? d.band.center
                e.lensBlur.band.angleDeg = num(b["angle_deg"]) ?? d.band.angleDeg
                e.lensBlur.band.width = num(b["width"]) ?? d.band.width
                e.lensBlur.band.feather = num(b["feather"]) ?? d.band.feather
            }
        }
        if let g = fx["glow"] as? JSON {
            e.glow.amount = num(g["amount"]) ?? 0
            e.glow.size = num(g["size"]) ?? GlowEdit().size
        }
        if let g = fx["grain"] as? JSON {
            e.grain.amount = num(g["amount"]) ?? 0
            e.grain.size = num(g["size"]) ?? GrainEdit().size
            e.grain.roughness = num(g["roughness"]) ?? GrainEdit().roughness
        }
        if let h = fx["halation"] as? JSON { e.halation.amount = num(h["amount"]) ?? 0 }
        return e
    }

    static func effectsJSON(_ e: EffectsEdit, onto base: Any?) -> JSON {
        var fx = (base as? JSON) ?? [:]
        let lb = e.lensBlur
        let old = fx["lens_blur"] as? JSON
        // A newer build's shape reads as the default here, and its mode as `.newer`; each
        // is written back as it was unless the user picked another one (never silently
        // rewritten).
        func choice<T: RawRepresentable & Equatable>(_ key: String, _ v: T, default d: T) -> Any where T.RawValue == String {
            if v == d, let raw = old?[key] as? String, T(rawValue: raw) == nil { return raw }
            return v.rawValue
        }
        let mode: Any = lb.mode == .newer ? (old?["mode"] ?? LensBlurEdit().mode.rawValue) : lb.mode.rawValue
        var blur = merge(fx["lens_blur"], [
            "amount": lb.amount, "mode": mode,
            "focal_range": lb.focalRange, "highlight_boost": lb.highlightBoost,
            "shape": choice("shape", lb.shape, default: LensBlurEdit().shape),
        ])
        if let f = lb.focus { blur["focus"] = [f.x, f.y] } else { blur.removeValue(forKey: "focus") }
        blur["plane"] = merge(blur["plane"], [
            "center": [lb.band.center.x, lb.band.center.y], "angle_deg": lb.band.angleDeg,
            "width": lb.band.width, "feather": lb.band.feather,
        ])
        fx["lens_blur"] = blur
        fx["glow"] = merge(fx["glow"], ["amount": e.glow.amount, "size": e.glow.size])
        fx["grain"] = merge(fx["grain"], ["amount": e.grain.amount, "size": e.grain.size, "roughness": e.grain.roughness])
        fx["halation"] = merge(fx["halation"], ["amount": e.halation.amount])
        return fx
    }

    static func look(_ v: Any?) -> AppliedLookEdit? {
        guard let l = v as? JSON, let id = l["id"] as? String, !id.isEmpty else { return nil }
        func values(_ m: Any?) -> [String: Double] {
            ((m as? JSON) ?? [:]).compactMapValues { num($0) }
        }
        return AppliedLookEdit(id: id, intensity: num(l["intensity"]) ?? 1, from: values(l["from"]), to: values(l["to"]))
    }

    static func lookJSON(_ l: AppliedLookEdit) -> JSON {
        ["id": l.id, "intensity": l.intensity, "from": l.from, "to": l.to]
    }
}
