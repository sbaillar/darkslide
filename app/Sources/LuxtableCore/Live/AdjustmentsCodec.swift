import CoreGraphics
import Foundation

/// `EditState` ↔ `lt_core::Adjustments` JSON (the sidecar schema, snake_case).
///
/// **Lossless:** the app decodes the JSON Rust gave it into a raw dictionary and,
/// on save, overlays only the fields `EditState` models onto that dictionary.
/// Everything else (crop flips, colour-range mask shapes, per-shape feathers,
/// fields added by newer builds) is written back as it was.
public enum AdjustmentsCodec {
    public typealias JSON = [String: Any]

    public enum CodecError: Error, Equatable {
        case notAnObject
        case invalid(String)
    }

    // MARK: - Public API

    /// Parse lt-core JSON. Returns the edit and the raw object for `encode(_:onto:)`.
    public static func decode(_ json: String) throws -> (EditState, JSON) {
        let obj: Any
        do {
            obj = try JSONSerialization.jsonObject(with: Data(json.utf8))
        } catch {
            throw CodecError.invalid(error.localizedDescription)
        }
        guard let dict = obj as? JSON else { throw CodecError.notAnObject }
        return (decode(dict), dict)
    }

    /// JSON for `edit`, starting from `base` (the object last read from Rust).
    public static func encode(_ edit: EditState, onto base: JSON = [:]) throws -> String {
        let obj = overlay(edit, onto: base)
        let data = try JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys])
        return String(decoding: data, as: UTF8.self)
    }

    // MARK: - Decode

    public static func decode(_ a: JSON) -> EditState {
        var e = EditState()
        e.profile = CameraProfileRef(json: a["profile"])
        if let wb = a["white_balance"] as? JSON, wb["mode"] as? String == "custom" {
            e.whiteBalance.asShot = false
            e.whiteBalance.temperature = num(wb["temp_k"]) ?? e.whiteBalance.temperature
            e.whiteBalance.tint = num(wb["tint"]) ?? 0
        }
        if let x = a["exposure"] as? JSON {
            e.exposure.ev = num(x["exposure_ev"]) ?? 0
            e.exposure.contrast = num(x["contrast"]) ?? 0
            e.exposure.brightness = num(x["brightness"]) ?? 0
            e.exposure.saturation = num(x["saturation"]) ?? 0
        }
        e.presence = presence(a["presence"])
        e.spots = ((a["spots"] as? [Any]) ?? []).compactMap { ($0 as? JSON).map(spot) }
        e.removals = removals(a["removals"])
        e.personBoxes = personBoxes(a["person_boxes"])
        if let h = a["hdr"] as? JSON {
            e.hdr.highlights = num(h["highlights"]) ?? 0
            e.hdr.shadows = num(h["shadows"]) ?? 0
            e.hdr.whites = num(h["whites"]) ?? 0
            e.hdr.blacks = num(h["blacks"]) ?? 0
        }
        if let l = a["levels"] as? JSON {
            e.levels.inBlack = num(l["in_black"]) ?? 0
            e.levels.inWhite = num(l["in_white"]) ?? 1
            e.levels.gamma = num(l["gamma"]) ?? 1
            e.levels.outBlack = num(l["out_black"]) ?? 0
            e.levels.outWhite = num(l["out_white"]) ?? 1
        }
        if let c = a["curve"] as? JSON {
            for (ch, key) in curveKeys {
                let pts = points(c[key])
                if !pts.isEmpty { e.curve.points[ch] = pts.map { CurvePoint($0.x, $0.y) } }
            }
        }
        if let cb = a["color_balance"] as? JSON {
            e.colorBalance.master = wheel(cb["master"])
            e.colorBalance.shadows = wheel(cb["shadows"])
            e.colorBalance.midtones = wheel(cb["midtones"])
            e.colorBalance.highlights = wheel(cb["highlights"])
        }
        if let ce = a["color_editor"] as? JSON, let basic = ce["basic"] as? [Any] {
            for (i, r) in basic.prefix(8).enumerated() {
                guard let r = r as? JSON else { continue }
                e.colorEditor.bands[i].hueShift = num(r["hue_shift"]) ?? 0
                e.colorEditor.bands[i].saturation = num(r["saturation"]) ?? 0
                e.colorEditor.bands[i].lightness = num(r["lightness"]) ?? 0
            }
        }
        if let ce = a["color_editor"] as? JSON {
            e.colorEditor.advanced = ((ce["advanced"] as? [Any]) ?? []).compactMap { ($0 as? JSON).map(colorRange) }
            if let skin = ce["skin"] as? JSON {
                e.colorEditor.skin = SkinToneEdit(range: colorRange((skin["range"] as? JSON) ?? [:]),
                                                  hueUniformity: num(skin["hue_uniformity"]) ?? 0,
                                                  saturationUniformity: num(skin["saturation_uniformity"]) ?? 0,
                                                  lightnessUniformity: num(skin["lightness_uniformity"]) ?? 0)
            }
        }
        if let d = a["detail"] as? JSON {
            e.detail.clarity = num(d["clarity"]) ?? 0
            e.detail.structure = num(d["structure"]) ?? 0
            e.detail.sharpenAmount = num(d["sharpen_amount"]) ?? e.detail.sharpenAmount
            e.detail.sharpenRadius = num(d["sharpen_radius"]) ?? e.detail.sharpenRadius
            e.detail.sharpenThreshold = num(d["sharpen_threshold"]) ?? e.detail.sharpenThreshold
        }
        if let n = a["noise"] as? JSON {
            e.noise.luminance = num(n["luminance"]) ?? e.noise.luminance
            e.noise.color = num(n["color"]) ?? e.noise.color
            e.noise.detail = num(n["detail"]) ?? e.noise.detail
            e.noise.aiDenoise = n["ai_denoise"] as? Bool ?? false
            e.noise.aiStrength = num(n["ai_strength"]) ?? e.noise.aiStrength
        }
        if let v = a["vignette"] as? JSON {
            e.vignette.amountEV = num(v["amount_ev"]) ?? 0
            e.vignette.midpoint = num(v["midpoint"]) ?? e.vignette.midpoint
        }
        if let l = a["lens"] as? JSON {
            e.lens.enabled = l["enabled"] as? Bool ?? true
            e.lens.distortion = num(l["distortion"]) ?? 1
            e.lens.chromaticAberration = l["chromatic_aberration"] as? Bool ?? true
            e.lens.lightFalloff = num(l["light_falloff"]) ?? 0.5
            e.lens.keystoneVertical = num(l["keystone_vertical"]) ?? 0
            e.lens.keystoneHorizontal = num(l["keystone_horizontal"]) ?? 0
            e.lens.profile = (l["profile"] as? String).flatMap { $0.isEmpty ? nil : $0 }
        }
        if let c = a["crop"] as? JSON {
            if let r = c["rect"] as? JSON {
                e.crop.rect = CGRect(x: num(r["x"]) ?? 0, y: num(r["y"]) ?? 0, width: num(r["w"]) ?? 1, height: num(r["h"]) ?? 1)
            }
            e.crop.angle = num(c["angle_deg"]) ?? 0
            e.crop.quarterTurns = Safe.int(num(c["quarter_turns"]) ?? 0) ?? 0
            e.crop.aspect = aspect(c["aspect"])
        }
        if let layers = a["layers"] as? [Any] {
            e.layers = layers.compactMap { ($0 as? JSON).map(layer) }
        }
        e.effects = effects(a["effects"])
        e.look = look(a["look"])
        return e
    }

    // MARK: - Encode

    public static func overlay(_ e: EditState, onto base: JSON) -> JSON {
        var a = base
        a["profile"] = e.profile.json
        a["white_balance"] = e.whiteBalance.asShot
            ? ["mode": "as_shot"] as JSON
            : ["mode": "custom", "temp_k": e.whiteBalance.temperature, "tint": e.whiteBalance.tint] as JSON
        a["exposure"] = merge(a["exposure"], [
            "exposure_ev": e.exposure.ev, "contrast": e.exposure.contrast,
            "brightness": e.exposure.brightness, "saturation": e.exposure.saturation,
        ])
        a["presence"] = presenceJSON(e.presence, onto: a["presence"])
        a["spots"] = e.spots.map(spotJSON)
        a["removals"] = removalsJSON(e.removals, onto: a["removals"])
        a["person_boxes"] = personBoxesJSON(e)
        a["hdr"] = merge(a["hdr"], ["highlights": e.hdr.highlights, "shadows": e.hdr.shadows,
                                    "whites": e.hdr.whites, "blacks": e.hdr.blacks])
        a["levels"] = merge(a["levels"], [
            "in_black": e.levels.inBlack, "in_white": e.levels.inWhite, "gamma": e.levels.gamma,
            "out_black": e.levels.outBlack, "out_white": e.levels.outWhite,
        ])
        var curve = (a["curve"] as? JSON) ?? [:]
        for (ch, key) in curveKeys { curve[key] = e.curve.points(for: ch).map { [$0.x, $0.y] } }
        a["curve"] = curve
        var cb = (a["color_balance"] as? JSON) ?? [:]
        cb["master"] = wheelJSON(e.colorBalance.master, onto: cb["master"])
        cb["shadows"] = wheelJSON(e.colorBalance.shadows, onto: cb["shadows"])
        cb["midtones"] = wheelJSON(e.colorBalance.midtones, onto: cb["midtones"])
        cb["highlights"] = wheelJSON(e.colorBalance.highlights, onto: cb["highlights"])
        a["color_balance"] = cb
        var ce = (a["color_editor"] as? JSON) ?? [:]
        let oldBasic = (ce["basic"] as? [Any]) ?? []
        ce["basic"] = (0..<8).map { i -> JSON in
            var r = (i < oldBasic.count ? oldBasic[i] as? JSON : nil) ?? [
                "center_hue": ColorEditorEdit.bandHues[i], "width": 45.0, "smoothness": 0.5,
            ]
            let b = e.colorEditor.bands[i]
            r["hue_shift"] = b.hueShift
            r["saturation"] = b.saturation
            r["lightness"] = b.lightness
            return r
        }
        ce["advanced"] = e.colorEditor.advanced.map { colorRangeJSON($0) }
        if let skin = e.colorEditor.skin {
            ce["skin"] = [
                "range": colorRangeJSON(skin.range), "hue_uniformity": skin.hueUniformity,
                "saturation_uniformity": skin.saturationUniformity, "lightness_uniformity": skin.lightnessUniformity,
            ] as JSON
        } else {
            ce["skin"] = NSNull()
        }
        a["color_editor"] = ce
        a["detail"] = merge(a["detail"], [
            "clarity": e.detail.clarity, "structure": e.detail.structure, "sharpen_amount": e.detail.sharpenAmount,
            "sharpen_radius": e.detail.sharpenRadius, "sharpen_threshold": e.detail.sharpenThreshold,
        ])
        a["noise"] = merge(a["noise"], ["luminance": e.noise.luminance, "color": e.noise.color, "detail": e.noise.detail,
                                        "ai_denoise": e.noise.aiDenoise, "ai_strength": e.noise.aiStrength])
        a["vignette"] = merge(a["vignette"], ["amount_ev": e.vignette.amountEV, "midpoint": e.vignette.midpoint])
        a["lens"] = merge(a["lens"], [
            "enabled": e.lens.enabled, "distortion": e.lens.distortion,
            "chromatic_aberration": e.lens.chromaticAberration, "light_falloff": e.lens.lightFalloff,
            "keystone_vertical": e.lens.keystoneVertical, "keystone_horizontal": e.lens.keystoneHorizontal,
            "profile": e.lens.profile.map { $0 as Any } ?? NSNull(),
        ])
        var crop = (a["crop"] as? JSON) ?? [:]
        crop["rect"] = ["x": e.crop.rect.minX, "y": e.crop.rect.minY, "w": e.crop.rect.width, "h": e.crop.rect.height]
        crop["angle_deg"] = e.crop.angle
        crop["quarter_turns"] = ((e.crop.quarterTurns % 4) + 4) % 4
        // An aspect the UI can't show (e.g. 7:5) decodes as .free; keep it unless changed.
        let baseAspect = aspect(crop["aspect"])
        if !(e.crop.aspect == .free && baseAspect == .free && crop["aspect"] is JSON) {
            crop["aspect"] = e.crop.aspect.ratio.map { ["long": $0.long, "short": $0.short] as JSON } ?? NSNull()
        }
        a["crop"] = crop
        let oldLayers = ((a["layers"] as? [Any]) ?? []).compactMap { $0 as? JSON }
        a["layers"] = e.layers.map { l in
            layerJSON(l, onto: oldLayers.first { num($0["id"]).map(Int.init) == l.id })
        }
        a["effects"] = effectsJSON(e.effects, onto: a["effects"])
        a["look"] = e.look.map(lookJSON) ?? NSNull()
        return a
    }

    // MARK: - Pieces

    static let curveKeys: [(CurveChannel, String)] = [(.rgb, "rgb"), (.luma, "luma"), (.red, "red"), (.green, "green"), (.blue, "blue")]

    /// Far beyond any slider range: a stored number past this (or not finite) is corrupt and
    /// read as absent, so nothing downstream converts it to an integer and traps.
    static let maxMagnitude = 1e6

    static func num(_ v: Any?) -> Double? {
        if let n = v as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID() {
            let d = n.doubleValue
            return d.isFinite && abs(d) <= maxMagnitude ? d : nil
        }
        return nil
    }

    static func merge(_ base: Any?, _ values: JSON) -> JSON {
        var d = (base as? JSON) ?? [:]
        for (k, v) in values { d[k] = v }
        return d
    }

    static func points(_ v: Any?) -> [CGPoint] {
        ((v as? [Any]) ?? []).compactMap { p in
            guard let a = p as? [Any], a.count == 2, let x = num(a[0]), let y = num(a[1]) else { return nil }
            return CGPoint(x: x, y: y)
        }
    }

    static func point(_ v: Any?) -> CGPoint? { points([v as Any]).first }

    static func wheel(_ v: Any?) -> WheelValue {
        let w = (v as? JSON) ?? [:]
        return WheelValue(hue: num(w["hue"]) ?? 0, saturation: num(w["sat"]) ?? 0, lightness: num(w["lum"]) ?? 0)
    }

    static func wheelJSON(_ w: WheelValue, onto base: Any?) -> JSON {
        merge(base, ["hue": w.hue, "sat": w.saturation, "lum": w.lightness])
    }

    static func aspect(_ v: Any?) -> CropAspect {
        guard let a = v as? JSON, let long = num(a["long"]), let short = num(a["short"]), short > 0 else { return .original }
        let known: [CropAspect] = [.r1x1, .r4x5, .r3x2, .r16x9]
        return known.first { c in c.ratio.map { Double($0.long) * short == Double($0.short) * long } ?? false } ?? .free
    }

    static func presence(_ v: Any?) -> PresenceEdit {
        let p = (v as? JSON) ?? [:]
        return PresenceEdit(vibrance: num(p["vibrance"]) ?? 0, texture: num(p["texture"]) ?? 0, dehaze: num(p["dehaze"]) ?? 0)
    }

    static func presenceJSON(_ p: PresenceEdit, onto base: Any?) -> JSON {
        merge(base, ["vibrance": p.vibrance, "texture": p.texture, "dehaze": p.dehaze])
    }

    static func spot(_ s: JSON) -> SpotEdit {
        SpotEdit(kind: SpotKind(rawValue: s["kind"] as? String ?? "") ?? .heal, center: point(s["center"]) ?? CGPoint(x: 0.5, y: 0.5),
                 radius: num(s["radius"]) ?? 0.02, source: point(s["source"]) ?? CGPoint(x: 0.55, y: 0.5),
                 feather: num(s["feather"]) ?? 0.5, opacity: num(s["opacity"]) ?? 1)
    }

    static func spotJSON(_ s: SpotEdit) -> JSON {
        ["kind": s.kind.rawValue, "center": [s.center.x, s.center.y], "radius": s.radius, "source": [s.source.x, s.source.y],
         "feather": s.feather, "opacity": s.opacity]
    }

    static func lumaRange(_ r: JSON) -> LumaRangeEdit {
        LumaRangeEdit(low: num(r["low"]) ?? 0, high: num(r["high"]) ?? 1, lowFalloff: num(r["low_falloff"]) ?? 0.1,
                      highFalloff: num(r["high_falloff"]) ?? 0.1)
    }

    static func lumaRangeJSON(_ r: LumaRangeEdit) -> JSON {
        ["low": r.low, "high": r.high, "low_falloff": r.lowFalloff, "high_falloff": r.highFalloff]
    }

    /// lt-core `ColorRange` (missing fields take lt-core's defaults).
    static func colorRange(_ r: JSON) -> ColorRangeEdit {
        ColorRangeEdit(centerHue: num(r["center_hue"]) ?? 0, width: num(r["width"]) ?? 45,
                       smoothness: num(r["smoothness"]) ?? 0.5, hueShift: num(r["hue_shift"]) ?? 0,
                       saturation: num(r["saturation"]) ?? 0, lightness: num(r["lightness"]) ?? 0)
    }

    static func colorRangeJSON(_ r: ColorRangeEdit) -> JSON {
        ["center_hue": r.centerHue, "width": r.width, "smoothness": r.smoothness, "hue_shift": r.hueShift,
         "saturation": r.saturation, "lightness": r.lightness]
    }

    static func op(_ v: Any?) -> MaskOp { MaskOp(rawValue: v as? String ?? "") ?? .add }

    /// Shapes the app models; `nil` for kinds it doesn't (e.g. `color_range`).
    static func shape(_ s: JSON) -> MaskShape? {
        let o = op(s["op"])
        switch s["kind"] as? String {
        case "fill": return .fill(o)
        case "brush":
            let strokes = ((s["strokes"] as? [Any]) ?? []).compactMap { $0 as? JSON }.map { st in
                BrushStroke(points: points(st["points"]), size: num(st["size"]) ?? 0.05, hardness: num(st["hardness"]) ?? 0.5,
                            flow: num(st["flow"]) ?? 1, erase: st["erase"] as? Bool ?? false,
                            autoMask: st["auto_mask"] as? Bool ?? false)
            }
            return .brush(o, strokes)
        case "linear":
            return .linear(o, start: point(s["start"]) ?? .zero, end: point(s["end"]) ?? .zero)
        case "radial":
            // Radii pass through as stored: long-edge units on both axes (the app uses them as is).
            let r = point(s["radius"]) ?? .zero
            return .radial(o, center: point(s["center"]) ?? .zero, radius: CGSize(width: r.x, height: r.y),
                           angle: num(s["angle_deg"]) ?? 0, feather: num(s["feather"]) ?? 0)
        case "ai":
            return aiShape(s, op: o)
        default: return nil
        }
    }

    static func shapeJSON(_ s: MaskShape) -> JSON {
        switch s {
        case .fill(let o): return ["kind": "fill", "op": o.rawValue]
        case .brush(let o, let strokes):
            return ["kind": "brush", "op": o.rawValue, "strokes": strokes.map { st -> JSON in
                ["points": st.points.map { [$0.x, $0.y] }, "size": st.size, "hardness": st.hardness,
                 "flow": st.flow, "erase": st.erase, "auto_mask": st.autoMask]
            }]
        case .linear(let o, let a, let b):
            return ["kind": "linear", "op": o.rawValue, "start": [a.x, a.y], "end": [b.x, b.y]]
        case .radial(let o, let c, let r, let angle, let feather):
            return ["kind": "radial", "op": o.rawValue, "center": [c.x, c.y], "radius": [r.width, r.height],
                    "angle_deg": angle, "feather": feather]
        case .ai(let o, let t): return ["kind": "ai", "op": o.rawValue, "target": t.rawValue]
        case .aiPerson(let o, let t, let p): return ["kind": "ai", "op": o.rawValue, "target": t.rawValue, "person": p]
        }
    }

    static func layer(_ l: JSON) -> LayerEdit {
        let adj = (l["adjustments"] as? JSON) ?? [:]
        let exp = (adj["exposure"] as? JSON) ?? [:]
        let m = (l["mask"] as? JSON) ?? [:]
        let shapes = ((m["shapes"] as? [Any]) ?? []).compactMap { ($0 as? JSON).flatMap(shape) }
        var layer = LayerEdit(id: Safe.int(num(l["id"]) ?? 0) ?? 0, name: l["name"] as? String ?? "", enabled: l["enabled"] as? Bool ?? true,
                              opacity: num(l["opacity"]) ?? 1, exposureEV: num(exp["exposure_ev"]) ?? 0,
                              mask: MaskEdit(shapes: shapes, invert: m["invert"] as? Bool ?? false, feather: num(m["feather"]) ?? 0,
                                             lumaRange: (m["luma_range"] as? JSON).map(lumaRange),
                                             colorRange: (m["color_range"] as? JSON).map(colorRange)))
        var t = LayerToolsEdit()
        if let wb = adj["white_balance"] as? JSON {
            t.temperature = num(wb["temp_k"]) ?? 0
            t.tint = num(wb["tint"]) ?? 0
        }
        t.contrast = num(exp["contrast"]) ?? 0
        t.brightness = num(exp["brightness"]) ?? 0
        t.saturation = num(exp["saturation"]) ?? 0
        t.presence = presence(adj["presence"])
        if let h = adj["hdr"] as? JSON {
            t.hdr = HDREdit()
            t.hdr.highlights = num(h["highlights"]) ?? 0
            t.hdr.shadows = num(h["shadows"]) ?? 0
            t.hdr.whites = num(h["whites"]) ?? 0
            t.hdr.blacks = num(h["blacks"]) ?? 0
        }
        if let c = adj["curve"] as? JSON {
            for (ch, key) in curveKeys {
                let pts = points(c[key])
                if !pts.isEmpty { t.curve.points[ch] = pts.map { CurvePoint($0.x, $0.y) } }
            }
        }
        if let cb = adj["color_balance"] as? JSON {
            t.colorBalance.master = wheel(cb["master"])
            t.colorBalance.shadows = wheel(cb["shadows"])
            t.colorBalance.midtones = wheel(cb["midtones"])
            t.colorBalance.highlights = wheel(cb["highlights"])
        }
        t.colorRanges = ((adj["color_editor"] as? [Any]) ?? []).compactMap { ($0 as? JSON).map(colorRange) }
        t.clarity = num(adj["clarity"]) ?? 0
        t.structure = num(adj["structure"]) ?? 0
        t.sharpening = num(adj["sharpening"]) ?? 0
        layer.tools = t
        return layer
    }

    static func layerJSON(_ l: LayerEdit, onto base: JSON?) -> JSON {
        var d = base ?? [:]
        d["id"] = l.id
        d["name"] = l.name
        d["enabled"] = l.enabled
        d["opacity"] = l.opacity
        var adj = (d["adjustments"] as? JSON) ?? [:]
        let t = l.tools
        adj["exposure"] = merge(adj["exposure"], [
            "exposure_ev": l.exposureEV, "contrast": t.contrast, "brightness": t.brightness, "saturation": t.saturation,
        ])
        adj["white_balance"] = t.temperature == 0 && t.tint == 0
            ? NSNull() : ["temp_k": t.temperature, "tint": t.tint] as JSON
        adj["presence"] = presenceJSON(t.presence, onto: adj["presence"])
        adj["hdr"] = merge(adj["hdr"], ["highlights": t.hdr.highlights, "shadows": t.hdr.shadows,
                                        "whites": t.hdr.whites, "blacks": t.hdr.blacks])
        var curve = (adj["curve"] as? JSON) ?? [:]
        for (ch, key) in curveKeys { curve[key] = t.curve.points(for: ch).map { [$0.x, $0.y] } }
        adj["curve"] = curve
        var cb = (adj["color_balance"] as? JSON) ?? [:]
        cb["master"] = wheelJSON(t.colorBalance.master, onto: cb["master"])
        cb["shadows"] = wheelJSON(t.colorBalance.shadows, onto: cb["shadows"])
        cb["midtones"] = wheelJSON(t.colorBalance.midtones, onto: cb["midtones"])
        cb["highlights"] = wheelJSON(t.colorBalance.highlights, onto: cb["highlights"])
        adj["color_balance"] = cb
        adj["color_editor"] = t.colorRanges.map(colorRangeJSON)
        adj["clarity"] = t.clarity
        adj["structure"] = t.structure
        adj["sharpening"] = t.sharpening
        d["adjustments"] = adj
        var mask = (d["mask"] as? JSON) ?? [:]
        mask["invert"] = l.mask.invert
        mask["feather"] = l.mask.feather
        mask["luma_range"] = l.mask.lumaRange.map(lumaRangeJSON) ?? NSNull()
        mask["color_range"] = l.mask.colorRange.map(colorRangeJSON) ?? NSNull()
        let oldShapes = ((mask["shapes"] as? [Any]) ?? []).compactMap { $0 as? JSON }
        let known = oldShapes.compactMap(shape)
        if known != l.mask.shapes {
            // Rebuild: the app's shapes in its order, then shapes it doesn't model. A shape
            // the app kept as it was keeps its JSON (fields it doesn't model, e.g. the
            // per-shape feather of brush, AI and colour-range shapes).
            var unused = oldShapes.filter { shape($0) != nil }
            let rebuilt: [JSON] = l.mask.shapes.map { s in
                if let i = unused.firstIndex(where: { shape($0) == s }) { return unused.remove(at: i) }
                return shapeJSON(s)
            }
            mask["shapes"] = rebuilt + oldShapes.filter { shape($0) == nil }
        } else {
            mask["shapes"] = oldShapes
        }
        d["mask"] = mask
        return d
    }
}
