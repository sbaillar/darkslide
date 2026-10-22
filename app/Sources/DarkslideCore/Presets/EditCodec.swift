import Foundation

/// Codable snapshot of an `EditState` (without layers and spots) for preset files.
///
/// Values are stored by stable string keys, so older/newer preset files load:
/// unknown keys are ignored and missing ones keep the identity value.
/// Temporary until presets move into `ds-session` (sidecar-schema presets).
public struct EditSnapshot: Codable, Equatable, Sendable {
    public var values: [String: Double]
    public var curves: [String: [[Double]]]
    /// Colour-editor advanced ranges and skin tone (absent in older files).
    public var ranges: [ColorRangeEdit]?
    public var skin: SkinToneEdit?

    public init(values: [String: Double] = [:], curves: [String: [[Double]]] = [:], ranges: [ColorRangeEdit]? = nil,
                skin: SkinToneEdit? = nil) {
        self.values = values
        self.curves = curves
        self.ranges = ranges
        self.skin = skin
    }
}

public enum EditCodec {
    /// Every scalar of `EditState` (except layers), by stable key.
    // Immutable tables of key paths (KeyPath is not Sendable, but these are never mutated).
    nonisolated(unsafe) static let scalars: [(String, WritableKeyPath<EditState, Double>)] = {
        var list: [(String, WritableKeyPath<EditState, Double>)] = [
            ("wb.temperature", \.whiteBalance.temperature), ("wb.tint", \.whiteBalance.tint),
            ("exposure.ev", \.exposure.ev), ("exposure.contrast", \.exposure.contrast),
            ("exposure.brightness", \.exposure.brightness), ("exposure.saturation", \.exposure.saturation),
            ("presence.vibrance", \.presence.vibrance), ("presence.texture", \.presence.texture),
            ("presence.dehaze", \.presence.dehaze),
            ("hdr.highlights", \.hdr.highlights), ("hdr.shadows", \.hdr.shadows),
            ("hdr.whites", \.hdr.whites), ("hdr.blacks", \.hdr.blacks),
            ("levels.inBlack", \.levels.inBlack), ("levels.inWhite", \.levels.inWhite),
            ("levels.gamma", \.levels.gamma), ("levels.outBlack", \.levels.outBlack),
            ("levels.outWhite", \.levels.outWhite),
            ("detail.clarity", \.detail.clarity), ("detail.structure", \.detail.structure),
            ("detail.sharpenAmount", \.detail.sharpenAmount), ("detail.sharpenRadius", \.detail.sharpenRadius),
            ("detail.sharpenThreshold", \.detail.sharpenThreshold),
            ("noise.luminance", \.noise.luminance), ("noise.color", \.noise.color), ("noise.detail", \.noise.detail),
            ("noise.aiStrength", \.noise.aiStrength),
            ("vignette.amountEV", \.vignette.amountEV), ("vignette.midpoint", \.vignette.midpoint),
            ("lens.distortion", \.lens.distortion), ("lens.lightFalloff", \.lens.lightFalloff),
            ("lens.keystoneVertical", \.lens.keystoneVertical), ("lens.keystoneHorizontal", \.lens.keystoneHorizontal),
            ("crop.angle", \.crop.angle),
            ("fx.blur.amount", \.effects.lensBlur.amount), ("fx.blur.mode", \.effects.lensBlur.modeIndex),
            ("fx.blur.focalRange", \.effects.lensBlur.focalRange),
            ("fx.blur.highlightBoost", \.effects.lensBlur.highlightBoost),
            ("fx.blur.shape", \.effects.lensBlur.shapeIndex),
            ("fx.band.x", \.effects.lensBlur.band.centerX), ("fx.band.y", \.effects.lensBlur.band.centerY),
            ("fx.band.angle", \.effects.lensBlur.band.angleDeg), ("fx.band.width", \.effects.lensBlur.band.width),
            ("fx.band.feather", \.effects.lensBlur.band.feather),
            ("fx.glow.amount", \.effects.glow.amount), ("fx.glow.size", \.effects.glow.size),
            ("fx.grain.amount", \.effects.grain.amount), ("fx.grain.size", \.effects.grain.size),
            ("fx.grain.roughness", \.effects.grain.roughness), ("fx.halation.amount", \.effects.halation.amount),
        ]
        for (name, wheel) in EditCodec.wheels {
            list.append(("cb.\(name).hue", wheel.appending(path: \.hue)))
            list.append(("cb.\(name).saturation", wheel.appending(path: \.saturation)))
            list.append(("cb.\(name).lightness", wheel.appending(path: \.lightness)))
        }
        for i in 0..<ColorEditorEdit.bandHues.count {
            list.append(("ce.\(i).hueShift", \.colorEditor.bands[i].hueShift))
            list.append(("ce.\(i).saturation", \.colorEditor.bands[i].saturation))
            list.append(("ce.\(i).lightness", \.colorEditor.bands[i].lightness))
        }
        return list
    }()

    /// The colour-balance wheels, by the name in their keys (`cb.<name>.hue`).
    nonisolated(unsafe) static let wheels: [(String, WritableKeyPath<EditState, WheelValue>)] = [
        ("master", \.colorBalance.master), ("shadows", \.colorBalance.shadows),
        ("midtones", \.colorBalance.midtones), ("highlights", \.colorBalance.highlights),
    ]

    nonisolated(unsafe) static let flags: [(String, WritableKeyPath<EditState, Bool>)] = [
        ("wb.asShot", \.whiteBalance.asShot), ("lens.enabled", \.lens.enabled),
        ("lens.chromaticAberration", \.lens.chromaticAberration), ("noise.aiDenoise", \.noise.aiDenoise),
    ]

    /// The adjustment group a scalar/flag key belongs to (by its prefix).
    public static func group(of key: String) -> AdjustGroup? {
        let prefix = key.split(separator: ".").first.map(String.init) ?? key
        switch prefix {
        case "wb": return .whiteBalance
        case "exposure": return .exposure
        case "presence": return .presence
        case "hdr": return .hdr
        case "levels": return .levels
        case "cb": return .colorBalance
        case "ce": return .colorEditor
        case "detail": return .detail
        case "noise": return .noise
        case "vignette": return .vignette
        case "lens": return .lens
        case "crop": return .crop
        case "fx": return .effects
        default: return nil
        }
    }

    /// Keys whose values are choices, not amounts: a look's intensity switches them (on
    /// above 0) instead of scaling them.
    public static func isDiscrete(_ key: String) -> Bool {
        key == "fx.blur.mode" || key == "fx.blur.shape"
    }

    /// A colour-balance wheel's hue or saturation: a look's intensity moves the two
    /// together, as a point on the wheel (`LookIntensity.wheel`).
    static func isWheelPolar(_ key: String) -> Bool {
        key.hasPrefix("cb.") && (key.hasSuffix(".hue") || key.hasSuffix(".saturation"))
    }

    public static func encode(_ e: EditState) -> EditSnapshot {
        var s = EditSnapshot()
        if !e.colorEditor.advanced.isEmpty { s.ranges = e.colorEditor.advanced }
        s.skin = e.colorEditor.skin
        for (k, p) in scalars { s.values[k] = e[keyPath: p] }
        for (k, p) in flags { s.values[k] = e[keyPath: p] ? 1 : 0 }
        s.values["crop.aspect"] = Double(CropAspect.allCases.firstIndex(of: e.crop.aspect) ?? 0)
        for (ch, pts) in e.curve.points { s.curves[ch.rawValue] = pts.map { [$0.x, $0.y] } }
        return s
    }

    /// Far beyond any slider range: a stored scalar larger than this is corrupt and ignored.
    static let maxMagnitude = 1e6

    public static func decode(_ s: EditSnapshot) -> EditState {
        var e = EditState()
        e.colorEditor.advanced = s.ranges ?? []
        e.colorEditor.skin = s.skin
        for (k, p) in scalars { if let v = s.values[k], v.isFinite, abs(v) <= maxMagnitude { e[keyPath: p] = v } }
        for (k, p) in flags { if let v = s.values[k] { e[keyPath: p] = v != 0 } }
        if let a = s.values["crop.aspect"], let i = Safe.int(a), CropAspect.allCases.indices.contains(i) {
            e.crop.aspect = CropAspect.allCases[i]
        }
        for (key, pts) in s.curves {
            guard let ch = CurveChannel(rawValue: key) else { continue }
            e.curve.points[ch] = pts.compactMap { $0.count == 2 ? CurvePoint($0[0], $0[1]) : nil }
        }
        return e
    }
}
