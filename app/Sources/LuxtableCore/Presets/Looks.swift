import Foundation

/// A one-touch look (the Styles panel's Looks): a style that may use the effects (lens
/// blur, glow, grain, halation) and whose strength stays adjustable after it is applied.
///
/// Looks set only scalar fields (no curves), so every field they change can be scaled by
/// the Intensity slider exactly. Tone groups (exposure, presence, HDR) add to the photo's
/// own values, like the styles: a look never resets the photo's correction.
public struct Look: Identifiable, Equatable, Sendable {
    /// Stable id, stored in the edit's `look` record (`look.portrait`).
    public let id: String
    public let name: String
    /// One line saying what it does (row help).
    public let summary: String
    public let preset: Preset

    /// `target` with this look at full intensity, and the record of what it changed.
    /// The photo's current look is taken off first (its fields back to the values before
    /// it; ones the user changed since stay theirs), so one look replaces another. With
    /// `stacking` (⇧-click) this one goes on top of it instead. Either way, applying the
    /// look the edit already has is the same as once.
    public func apply(to target: EditState, stacking: Bool = false) -> EditState {
        var base = target
        if let current = target.look, !stacking || current.id == id {
            base = LookIntensity.set(0, on: target)
        }
        base.look = nil
        var out = preset.apply(to: base)
        var from: [String: Double] = [:], to: [String: Double] = [:]
        for (key, path) in EditCodec.scalars {
            let (b, a) = (base[keyPath: path], out[keyPath: path])
            if b != a {
                from[key] = b
                to[key] = a
            }
        }
        out.look = AppliedLookEdit(id: id, intensity: 1, from: from, to: to)
        return out
    }
}

/// The Intensity slider of an applied look.
public enum LookIntensity {
    /// Two values within this (relative) count as the same: they went through the
    /// sidecar's single precision and back.
    static let tolerance = 1e-4

    /// `edit` with its look at intensity `t` (0...1). A field still at the value the look
    /// gave it moves to `from + t·(to − from)` (choices switch: the look's above 0, the old
    /// one at 0; a colour-balance wheel moves in a straight line across the wheel), so
    /// intensity 0 restores the values before the look exactly. A field the user changed
    /// since is theirs: it keeps their value and leaves the look's record, so a round trip
    /// of the slider never loses it. When every field is theirs, the look is gone.
    public static func set(_ t: Double, on edit: EditState) -> EditState {
        guard var look = edit.look else { return edit }
        let t = min(max(t, 0), 1), old = look.intensity
        var out = edit
        var theirs = Set<String>()
        for (key, path) in EditCodec.scalars where !EditCodec.isWheelPolar(key) {
            guard let a = look.to[key], let b = look.from[key] else { continue }
            let cur = edit[keyPath: path]
            if EditCodec.isDiscrete(key) {
                if same(cur, old > 0 ? a : b) { out[keyPath: path] = t > 0 ? a : b } else { theirs.insert(key) }
            } else if same(cur, b + old * (a - b)) {
                out[keyPath: path] = t == 1 ? a : b + t * (a - b)
            } else {
                theirs.insert(key)
            }
        }
        for (name, wheel) in EditCodec.wheels {
            if let w = Self.wheel(edit[keyPath: wheel], look: look, name: name, from: old, to: t) {
                out[keyPath: wheel] = w
            } else {
                theirs.formUnion(["cb.\(name).hue", "cb.\(name).saturation"])
            }
        }
        look.intensity = t
        // A look that owns no field any more is gone (its Intensity row with it).
        out.look = look.dropping(theirs.contains)
        return out
    }

    /// One colour-balance wheel's hue and saturation at intensity `t`, as a point on the
    /// wheel (`saturation·(cos hue, sin hue)`) moving in a straight line from before the
    /// look to the look's: a low intensity is a little of the look's colour over the
    /// photo's, never its hue at full strength. From a neutral wheel that is the look's
    /// hue at once, `t` of its saturation. Nil when the user changed the wheel since (it
    /// is theirs); `cur` when the look doesn't touch it.
    static func wheel(_ cur: WheelValue, look: AppliedLookEdit, name: String, from old: Double, to t: Double) -> WheelValue? {
        let hk = "cb.\(name).hue", sk = "cb.\(name).saturation"
        guard look.to[hk] != nil || look.to[sk] != nil else { return cur }
        let b = WheelValue(hue: look.from[hk] ?? cur.hue, saturation: look.from[sk] ?? cur.saturation)
        let a = WheelValue(hue: look.to[hk] ?? cur.hue, saturation: look.to[sk] ?? cur.saturation)
        func point(_ w: WheelValue) -> (Double, Double) {
            let r = w.hue * .pi / 180
            return (w.saturation * cos(r), w.saturation * sin(r))
        }
        func lerp(_ k: Double) -> (Double, Double) {
            let (pb, pa) = (point(b), point(a))
            return (pb.0 + k * (pa.0 - pb.0), pb.1 + k * (pa.1 - pb.1))
        }
        let ends: WheelValue? = old == 0 ? b : old == 1 ? a : nil
        let untouched = ends.map { same(cur.hue, $0.hue) && same(cur.saturation, $0.saturation) }
            ?? samePoint(point(cur), lerp(old))
        guard untouched else { return nil }
        var out = cur
        if t == 0 || t == 1 {
            let w = t == 0 ? b : a // exact at both ends
            out.hue = w.hue
            out.saturation = w.saturation
            return out
        }
        let (x, y) = lerp(t)
        let sat = (x * x + y * y).squareRoot()
        out.saturation = sat
        // A neutral point has no hue: keep the one it had.
        if sat > 1e-9 {
            let deg = atan2(y, x) * 180 / .pi
            out.hue = deg < 0 ? deg + 360 : deg
        }
        return out
    }

    static func same(_ x: Double, _ y: Double) -> Bool {
        abs(x - y) <= tolerance * max(1, abs(x), abs(y))
    }

    static func samePoint(_ p: (Double, Double), _ q: (Double, Double)) -> Bool {
        same(p.0, q.0) && same(p.1, q.1)
    }
}

extension AppliedLookEdit {
    /// The record without the fields `overwritten` says something else has set anew (a
    /// group reset or pasted, a style, an AI edit): the look no longer owns them, so its
    /// Intensity leaves them alone. Nil when the look owns no field any more.
    /// A wheel's hue and saturation go together (they move as one point): either one
    /// set anew takes both.
    func dropping(_ overwritten: (String) -> Bool) -> AppliedLookEdit? {
        func gone(_ key: String) -> Bool {
            guard EditCodec.isWheelPolar(key) else { return overwritten(key) }
            let wheel = key.split(separator: ".").dropLast().joined(separator: ".")
            return overwritten(wheel + ".hue") || overwritten(wheel + ".saturation")
        }
        var l = self
        l.from = from.filter { !gone($0.key) }
        l.to = to.filter { !gone($0.key) }
        return l.to.isEmpty ? nil : l
    }
}

/// The one-touch looks, in panel order: the active plug-ins' looks (Luxtable's own are
/// the bundled `luxtable.looks` plug-in, plugins/builtin/luxtable-looks.ltplugin).
public enum LookPack {
    public static let category = "Looks"

    public static var all: [Look] { PluginContent.current.looks }

    /// Every setting of each effect a look uses, even one left at its default: a look's
    /// blur, glow or grain is all its own (Portrait after Tilt-Shift is a depth blur with
    /// Portrait's bokeh; Warm Negative after Noir has its own fine grain).
    static func effectFields(_ e: EditState) -> Set<String> {
        let fx = e.effects
        var keys: Set<String> = []
        if fx.lensBlur.amount > 0 { keys.formUnion(["fx.blur.amount", "fx.blur.mode", "fx.blur.focalRange", "fx.blur.highlightBoost"]) }
        if fx.glow.amount > 0 { keys.formUnion(["fx.glow.amount", "fx.glow.size"]) }
        if fx.grain.amount > 0 { keys.formUnion(["fx.grain.amount", "fx.grain.size", "fx.grain.roughness"]) }
        if fx.halation.amount > 0 { keys.insert("fx.halation.amount") }
        return keys
    }

    public static func look(id: String) -> Look? { all.first { $0.id == id } }

    /// The look whose preset this is (the panel's rows are presets).
    public static func look(preset: UUID) -> Look? { all.first { $0.preset.id == preset } }

    /// `target` as a click on `preset`'s row leaves it: a look's row replaces the photo's
    /// look with this one, any other row applies the preset.
    public static func applying(_ preset: Preset, to target: EditState) -> EditState {
        look(preset: preset.id)?.apply(to: target) ?? preset.apply(to: target)
    }
}

extension AppModel {
    /// Apply a look to the focused image (or all selected with Edit All Selected): one undo
    /// step. It replaces each photo's look, or with `stacking` goes on top of it.
    public func applyLook(_ look: Look, stacking: Bool = false) {
        updateEdit(label: "Look \(look.name)") { e in e = look.apply(to: e, stacking: stacking) }
    }

    /// The focused edit with `look` applied (hover preview).
    public func previewEdit(applying look: Look) -> EditState? {
        focusedID.map { look.apply(to: edit(for: $0)) }
    }

    /// The Intensity slider of the focused photo's look (with Edit All Selected, every
    /// selected photo's own look): a drag is one undo step.
    public func setLookIntensity(_ t: Double) {
        guard let id = focusedID, edit(for: id).look != nil else { return }
        updateEdit(label: "Look Intensity", coalesceKey: "look-intensity") { e in
            e = LookIntensity.set(t, on: e)
        }
    }
}
