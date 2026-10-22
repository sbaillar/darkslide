import CoreGraphics
import Foundation

/// Lens blur, glow, grain and halation (ds-core `Effects`). Defaults are the identity.
public struct EffectsEdit: Equatable, Sendable {
    public var lensBlur = LensBlurEdit()
    public var glow = GlowEdit()
    public var grain = GrainEdit()
    public var halation = HalationEdit()
    public init() {}

    /// Nothing renders (every amount is 0).
    public var isOff: Bool {
        lensBlur.amount <= 0 && glow.amount <= 0 && grain.amount <= 0 && halation.amount <= 0
    }
}

/// Where the lens blur's sharp zone comes from (ds-core `BlurMode`).
public enum BlurMode: String, CaseIterable, Sendable {
    /// The photo's depth map (AI depth refined by the subject masks).
    case depth
    /// A straight sharp band across the frame (tilt-shift miniature).
    case plane
    /// A mode from a newer version of the app: it renders no blur here, and the sidecar
    /// keeps the newer mode until the user picks one of these.
    case newer = "_newer"

    public var title: String {
        switch self {
        case .depth: "Depth"
        case .plane: "Tilt-Shift"
        case .newer: "Newer"
        }
    }

    /// The modes the panel offers.
    public static let pickable: [BlurMode] = [.depth, .plane]

    /// A sidecar's mode: a known name, else (a newer version's) `.newer`.
    public static func read(_ raw: String) -> BlurMode {
        BlurMode(rawValue: raw).flatMap { $0 == .newer ? nil : $0 } ?? .newer
    }
}

/// Shape of the out-of-focus highlights (ds-core `BokehShape`).
public enum BokehShape: String, CaseIterable, Sendable {
    case circle, blades, catEye = "cat_eye"

    public var title: String {
        switch self {
        case .circle: "Circle"
        case .blades: "Blades"
        case .catEye: "Swirl"
        }
    }
}

/// The tilt-shift band (ds-core `FocusBand`): `center` E-normalized, `angleDeg`
/// counter-clockwise, `width` (half the sharp band) and `feather` normalized to the long edge.
public struct FocusBandEdit: Equatable, Sendable {
    public var center = CGPoint(x: 0.5, y: 0.55)
    public var angleDeg: Double = 0
    public var width: Double = 0.06
    public var feather: Double = 0.22
    public init() {}
}

/// Depth (or tilt-shift) lens blur (ds-core `LensBlur`): sliders 0...100.
public struct LensBlurEdit: Equatable, Sendable {
    public var amount: Double = 0
    public var mode: BlurMode = .depth
    /// E-normalized point whose depth is in focus; nil focuses on the subject.
    public var focus: CGPoint?
    public var focalRange: Double = 20
    public var highlightBoost: Double = 30
    public var shape: BokehShape = .circle
    public var band = FocusBandEdit()
    public init() {}

    /// The f-number the Amount slider reads as: f/16 at 1 to f/1.2 at 100 (log scale).
    public static func fNumber(amount: Double) -> Double {
        16 * pow(1.2 / 16, min(max(amount, 0), 100) / 100)
    }

    /// A picked focus point (oriented-normalized), kept inside the frame.
    public static func focusPoint(_ p: CGPoint) -> CGPoint {
        CGPoint(x: min(max(p.x, 0), 1), y: min(max(p.y, 0), 1))
    }

    /// The amount an f-number reads as (inverse of `fNumber`).
    public static func amount(fNumber f: Double) -> Double {
        min(max(100 * log(f / 16) / log(1.2 / 16), 0), 100)
    }

    /// Whether renders need the photo's depth map.
    public var needsDepth: Bool { amount > 0 && mode == .depth }
}

/// Soft glow (ds-core `Glow`).
public struct GlowEdit: Equatable, Sendable {
    public var amount: Double = 0
    public var size: Double = 50
    public init() {}
}

/// Film grain (ds-core `Grain`).
public struct GrainEdit: Equatable, Sendable {
    public var amount: Double = 0
    public var size: Double = 25
    public var roughness: Double = 50
    public init() {}
}

/// Halation (ds-core `Halation`).
public struct HalationEdit: Equatable, Sendable {
    public var amount: Double = 0
    public init() {}
}

/// The one-touch look last applied (ds-core `AppliedLook`): each field it changed, by
/// `EditCodec` key, before the look (`from`) and at full intensity (`to`).
public struct AppliedLookEdit: Equatable, Sendable {
    public var id: String
    /// 0...1.
    public var intensity: Double
    public var from: [String: Double]
    public var to: [String: Double]

    public init(id: String, intensity: Double = 1, from: [String: Double] = [:], to: [String: Double] = [:]) {
        self.id = id
        self.intensity = intensity
        self.from = from
        self.to = to
    }
}

// Scalar views of the choices and the band centre, for `EditCodec` keys (presets, looks).
extension LensBlurEdit {
    public var modeIndex: Double {
        get { Double(BlurMode.allCases.firstIndex(of: mode) ?? 0) }
        set { mode = BlurMode.allCases[min(max(Int(newValue.rounded()), 0), BlurMode.allCases.count - 1)] }
    }

    public var shapeIndex: Double {
        get { Double(BokehShape.allCases.firstIndex(of: shape) ?? 0) }
        set { shape = BokehShape.allCases[min(max(Int(newValue.rounded()), 0), BokehShape.allCases.count - 1)] }
    }
}

extension FocusBandEdit {
    public var centerX: Double {
        get { center.x }
        set { center.x = newValue }
    }

    public var centerY: Double {
        get { center.y }
        set { center.y = newValue }
    }
}
