import CoreGraphics
import Foundation

/// UI-side mirror of `lt_core::Adjustments`, grouped exactly like `AdjustGroup`.
///
/// This is a view model, not the source of truth: once `lt-session` is wired
/// through the FFI, edits are saved to sidecars in Rust and copy/apply runs
/// `lt_core::Clipboard::apply`. Defaults are identity, matching `lt-core`.
public struct EditState: Equatable, Sendable {
    /// The base look; new edits start on Luxtable Standard (`CameraProfile.swift`).
    public var profile: CameraProfileRef = .standard
    public var whiteBalance = WhiteBalanceEdit()
    public var exposure = ExposureEdit()
    /// Vibrance, texture, dehaze (lt-core `presence`).
    public var presence = PresenceEdit()
    public var hdr = HDREdit()
    public var levels = LevelsEdit()
    public var curve = CurveEdit()
    public var colorBalance = ColorBalanceEdit()
    public var colorEditor = ColorEditorEdit()
    public var detail = DetailEdit()
    public var noise = NoiseEdit()
    public var vignette = VignetteEdit()
    public var lens = LensEdit()
    public var crop = CropEdit()
    public var layers: [LayerEdit] = []
    /// Heal and clone spots (lt-core `spots`), in the oriented frame.
    public var spots: [SpotEdit] = []
    /// Content-aware removals (lt-core `removals`), in the oriented frame.
    public var removals: [RemovalEdit] = []
    /// Where the people of the per-person shapes (`MaskShape.aiPerson`) were when they were
    /// masked, by person index: normalized boxes (lt-core `person_boxes`, `PersonMatch`).
    public var personBoxes: [Int: CGRect] = [:]
    /// Lens blur, glow, grain and halation (lt-core `effects`).
    public var effects = EffectsEdit()
    /// The one-touch look last applied, for its Intensity slider (lt-core `look`).
    public var look: AppliedLookEdit?

    public init() {}

    /// Exactly the default edit (Luxtable Standard): what a photo without a stored edit
    /// renders as, so only this may be dropped from `edits`. An untouched Neutral edit
    /// (from before profiles, or picked) must be kept, or it would render as Standard.
    public var isDefault: Bool { self == EditState() }

    /// Unedited, for the edited badge: the default, or an untouched edit from before
    /// profiles (Neutral); the person boxes only qualify shapes, and a look at intensity 0
    /// changes nothing. Not a storage test: see `isDefault`.
    public var isIdentity: Bool {
        var e = self
        e.personBoxes = [:]
        if e.look?.intensity == 0 { e.look = nil }
        var legacy = EditState()
        legacy.profile = .legacy
        return e == EditState() || e == legacy
    }
}

public struct WhiteBalanceEdit: Equatable, Sendable {
    /// `true` = camera as-shot; `temperature`/`tint` are then display-only.
    public var asShot = true
    /// Kelvin, 2000...50000.
    public var temperature: Double = 5500
    /// -150...150.
    public var tint: Double = 0
    public init() {}
}

public struct ExposureEdit: Equatable, Sendable {
    /// Stops, -5...5.
    public var ev: Double = 0
    /// -100...100 for the rest.
    public var contrast: Double = 0
    public var brightness: Double = 0
    public var saturation: Double = 0
    public init() {}
}

/// Lightroom's "Presence" (lt-core `Presence`): all -100...100.
public struct PresenceEdit: Equatable, Sendable {
    public var vibrance: Double = 0
    public var texture: Double = 0
    public var dehaze: Double = 0
    public init(vibrance: Double = 0, texture: Double = 0, dehaze: Double = 0) {
        self.vibrance = vibrance
        self.texture = texture
        self.dehaze = dehaze
    }
}

public struct HDREdit: Equatable, Sendable {
    public var highlights: Double = 0
    public var shadows: Double = 0
    public var whites: Double = 0
    public var blacks: Double = 0
    public init() {}
}

public struct LevelsEdit: Equatable, Sendable {
    public var inBlack: Double = 0
    public var inWhite: Double = 1
    /// Midtone gamma, 0.1...10.
    public var gamma: Double = 1
    public var outBlack: Double = 0
    public var outWhite: Double = 1
    public init() {}
}

/// A curve control point on 0...1.
public struct CurvePoint: Equatable, Hashable, Sendable {
    public var x: Double
    public var y: Double
    public init(_ x: Double, _ y: Double) {
        self.x = x
        self.y = y
    }
}

public enum CurveChannel: String, CaseIterable, Sendable {
    case rgb = "RGB", luma = "Luma", red = "Red", green = "Green", blue = "Blue"
}

public struct CurveEdit: Equatable, Sendable {
    /// Per channel; an empty list is the identity line.
    public var points: [CurveChannel: [CurvePoint]] = [:]
    public init() {}

    public func points(for channel: CurveChannel) -> [CurvePoint] {
        points[channel] ?? []
    }
}

/// One color wheel: hue in degrees, saturation 0...1, lightness -1...1.
public struct WheelValue: Equatable, Sendable {
    public var hue: Double = 0
    public var saturation: Double = 0
    public var lightness: Double = 0
    public init(hue: Double = 0, saturation: Double = 0, lightness: Double = 0) {
        self.hue = hue
        self.saturation = saturation
        self.lightness = lightness
    }
}

public struct ColorBalanceEdit: Equatable, Sendable {
    public var master = WheelValue()
    public var shadows = WheelValue()
    public var midtones = WheelValue()
    public var highlights = WheelValue()
    public init() {}
}

public struct ColorBandEdit: Equatable, Sendable {
    public var hueShift: Double = 0
    public var saturation: Double = 0
    public var lightness: Double = 0
    public init() {}
}

public struct ColorEditorEdit: Equatable, Sendable {
    /// Red, orange, yellow, green, cyan, blue, purple, magenta (lt-core `BASIC_BAND_HUES`).
    public static let bandNames = ["Red", "Orange", "Yellow", "Green", "Cyan", "Blue", "Purple", "Magenta"]
    public static let bandHues: [Double] = [0, 30, 60, 120, 180, 240, 270, 300]
    public var bands = Array(repeating: ColorBandEdit(), count: 8)
    /// Advanced mode: picked hue ranges (lt-core `ColorEditor::advanced`).
    public var advanced: [ColorRangeEdit] = []
    /// Skin-tone uniformity (lt-core `ColorEditor::skin`).
    public var skin: SkinToneEdit?
    public init() {}
}

/// A picked hue range (lt-core `ColorRange`): `centerHue`/`width` in degrees select it,
/// `smoothness` 0...1 feathers the edge; shifts are degrees (hue, ±30) and -100...100.
public struct ColorRangeEdit: Equatable, Sendable, Codable {
    public var centerHue: Double
    public var width: Double
    public var smoothness: Double
    public var hueShift: Double
    public var saturation: Double
    public var lightness: Double

    public init(centerHue: Double = 0, width: Double = 45, smoothness: Double = 0.5, hueShift: Double = 0,
                saturation: Double = 0, lightness: Double = 0) {
        self.centerHue = centerHue
        self.width = width
        self.smoothness = smoothness
        self.hueShift = hueShift
        self.saturation = saturation
        self.lightness = lightness
    }
}

/// Skin-tone uniformity (lt-core `SkinTone`): pulls hue/saturation/lightness inside
/// `range` toward its centre; uniformities are 0...100.
public struct SkinToneEdit: Equatable, Sendable, Codable {
    public var range: ColorRangeEdit
    public var hueUniformity: Double
    public var saturationUniformity: Double
    public var lightnessUniformity: Double

    public init(range: ColorRangeEdit = ColorRangeEdit(centerHue: 25, width: 40), hueUniformity: Double = 0,
                saturationUniformity: Double = 0, lightnessUniformity: Double = 0) {
        self.range = range
        self.hueUniformity = hueUniformity
        self.saturationUniformity = saturationUniformity
        self.lightnessUniformity = lightnessUniformity
    }
}

public struct DetailEdit: Equatable, Sendable {
    public var clarity: Double = 0
    public var structure: Double = 0
    public var sharpenAmount: Double = 140
    public var sharpenRadius: Double = 0.8
    public var sharpenThreshold: Double = 1
    public init() {}
}

public struct NoiseEdit: Equatable, Sendable {
    public var luminance: Double = 25
    public var color: Double = 25
    public var detail: Double = 50
    /// AI Denoise (lt-core `ai_denoise`) and its strength 0...100 (`ai_strength`).
    public var aiDenoise = false
    public var aiStrength: Double = 50
    public init() {}
}

public struct VignetteEdit: Equatable, Sendable {
    public var amountEV: Double = 0
    public var midpoint: Double = 0.5
    public init() {}
}

public struct LensEdit: Equatable, Sendable {
    public var enabled = true
    public var distortion: Double = 1
    public var chromaticAberration = true
    /// lt-core default 0.5: matches in-camera shading compensation.
    public var lightFalloff: Double = 0.5
    /// Perspective correction, degrees (lt-core `keystone_vertical`/`_horizontal`, ±30):
    /// positive vertical widens the top, positive horizontal widens the right side.
    public var keystoneVertical: Double = 0
    public var keystoneHorizontal: Double = 0
    /// A profile chosen by hand (lt-core `profile`, a model name from `LensProfiles`);
    /// nil = auto-detect from the lens EXIF.
    public var profile: String?
    public init() {}
}

public enum CropAspect: String, CaseIterable, Sendable {
    case free = "Free", original = "Original", r1x1 = "1:1", r4x5 = "4:5", r3x2 = "3:2", r16x9 = "16:9"
    // Added later: appended so presets' stored indices (EditCodec) keep their meaning.
    case r4x3 = "4:3", r7x5 = "7:5", r8x5 = "8:5"

    /// Long:short, or `nil` for free/original.
    public var ratio: (long: Int, short: Int)? {
        switch self {
        case .free, .original: nil
        case .r1x1: (1, 1)
        case .r4x5: (5, 4)
        case .r3x2: (3, 2)
        case .r16x9: (16, 9)
        case .r4x3: (4, 3)
        case .r7x5: (7, 5)
        case .r8x5: (8, 5)
        }
    }
}

public struct CropEdit: Equatable, Sendable {
    public var aspect: CropAspect = .original
    /// Straighten, degrees -45...45.
    public var angle: Double = 0
    /// Normalized crop rectangle in oriented image coordinates (full image = identity).
    public var rect = CGRect(x: 0, y: 0, width: 1, height: 1)
    /// Extra user rotation in quarter turns, 0...3 (after EXIF orientation).
    public var quarterTurns = 0
    public init() {}
}

public struct LayerEdit: Equatable, Identifiable, Sendable {
    public var id: Int
    public var name: String
    public var enabled = true
    public var opacity: Double = 1
    public var exposureEV: Double = 0
    /// Everything else a layer adjusts (lt-core `LayerAdjustments` besides exposure EV).
    public var tools = LayerToolsEdit()
    /// Shapes combined into the layer's mask (mirrors `lt_core::Mask`).
    public var mask = MaskEdit()

    public init(id: Int, name: String, enabled: Bool = true, opacity: Double = 1, exposureEV: Double = 0,
                mask: MaskEdit = MaskEdit()) {
        self.id = id
        self.name = name
        self.enabled = enabled
        self.opacity = opacity
        self.exposureEV = exposureEV
        self.mask = mask
    }
}

public enum MaskOp: String, Sendable, CaseIterable { case add, subtract, intersect }
public enum AITarget: String, Sendable, CaseIterable {
    case subject, sky, person, background
    /// People sub-parts (see `PeopleTargets`).
    case face, skin, eyes, teeth, hair, clothes
}

/// One brush stroke in normalized image coordinates; `size` is relative to the long edge.
public struct BrushStroke: Equatable, Sendable {
    public var points: [CGPoint]
    public var size: Double
    public var hardness: Double
    public var flow: Double
    public var erase: Bool
    /// Edge-aware: the stroke follows image edges under it (lt-core `auto_mask`).
    public var autoMask: Bool

    public init(points: [CGPoint], size: Double = 0.05, hardness: Double = 0.5, flow: Double = 1, erase: Bool = false,
                autoMask: Bool = false) {
        self.points = points
        self.size = size
        self.hardness = hardness
        self.flow = flow
        self.erase = erase
        self.autoMask = autoMask
    }
}

/// Mask shapes (normalized, oriented image coordinates), as in `lt_core::MaskShape`.
public enum MaskShape: Equatable, Sendable {
    case fill(MaskOp)
    case brush(MaskOp, [BrushStroke])
    case linear(MaskOp, start: CGPoint, end: CGPoint)
    /// `radius`: both semi-axes in units of the oriented image's long edge (a circle is
    /// `r × r` on any aspect); `angle` turns it clockwise in pixel space.
    case radial(MaskOp, center: CGPoint, radius: CGSize, angle: Double, feather: Double)
    case ai(MaskOp, AITarget)
    /// A people target for one person (index in `PeopleMasks` order, largest first).
    case aiPerson(MaskOp, AITarget, person: Int)
}

public struct MaskEdit: Equatable, Sendable {
    public var shapes: [MaskShape] = []
    public var invert = false
    public var feather: Double = 0
    /// Refinements applied on top of the shapes (lt-core `luma_range` / `color_range`).
    public var lumaRange: LumaRangeEdit?
    public var colorRange: ColorRangeEdit?
    public init(shapes: [MaskShape] = [], invert: Bool = false, feather: Double = 0, lumaRange: LumaRangeEdit? = nil,
                colorRange: ColorRangeEdit? = nil) {
        self.shapes = shapes
        self.invert = invert
        self.feather = feather
        self.lumaRange = lumaRange
        self.colorRange = colorRange
    }
}

/// Luminance-range mask refinement (lt-core `LumaRange`): 0...1 bounds with soft falloffs.
public struct LumaRangeEdit: Equatable, Sendable {
    public var low: Double = 0
    public var high: Double = 1
    public var lowFalloff: Double = 0.1
    public var highFalloff: Double = 0.1
    public init(low: Double = 0, high: Double = 1, lowFalloff: Double = 0.1, highFalloff: Double = 0.1) {
        self.low = low
        self.high = high
        self.lowFalloff = lowFalloff
        self.highFalloff = highFalloff
    }
}

/// A layer's tools besides exposure EV (lt-core `LayerAdjustments`): white-balance shift,
/// tone, presence, HDR, colour, detail and local sharpening (negative softens).
public struct LayerToolsEdit: Equatable, Sendable {
    /// White-balance shift relative to the global one: Kelvin delta and tint delta.
    public var temperature: Double = 0
    public var tint: Double = 0
    public var contrast: Double = 0
    public var brightness: Double = 0
    public var saturation: Double = 0
    public var presence = PresenceEdit()
    public var hdr = HDREdit()
    public var colorBalance = ColorBalanceEdit()
    public var curve = CurveEdit()
    public var colorRanges: [ColorRangeEdit] = []
    public var clarity: Double = 0
    public var structure: Double = 0
    public var sharpening: Double = 0
    public init() {}
}

public enum SpotKind: String, CaseIterable, Sendable {
    case heal, clone
}

/// A heal or clone spot (lt-core `Spot`): normalized oriented-frame points, `radius`
/// relative to the long edge, `feather` 0...1 of the radius.
public struct SpotEdit: Equatable, Sendable, Identifiable {
    public var id = UUID()
    public var kind: SpotKind = .heal
    public var center = CGPoint(x: 0.5, y: 0.5)
    public var radius: Double = 0.02
    public var source = CGPoint(x: 0.55, y: 0.5)
    public var feather: Double = 0.5
    public var opacity: Double = 1
    public init(kind: SpotKind = .heal, center: CGPoint = CGPoint(x: 0.5, y: 0.5), radius: Double = 0.02,
                source: CGPoint = CGPoint(x: 0.55, y: 0.5), feather: Double = 0.5, opacity: Double = 1) {
        self.kind = kind
        self.center = center
        self.radius = radius
        self.source = source
        self.feather = feather
        self.opacity = opacity
    }

    /// Equality ignores the view identity.
    public static func == (a: SpotEdit, b: SpotEdit) -> Bool {
        a.kind == b.kind && a.center == b.center && a.radius == b.radius && a.source == b.source
            && a.feather == b.feather && a.opacity == b.opacity
    }
}

/// C1-style cursor tools acting on the viewer canvas.
public enum CanvasTool: String, Sendable, CaseIterable {
    case pan, crop, straighten, rotate, colorPicker, wbPicker, brush, eraser, linearGradient, radialGradient
    /// Lens blur: a click sets the depth in focus (`LensBlurEdit.focus`).
    case focusPoint
}
