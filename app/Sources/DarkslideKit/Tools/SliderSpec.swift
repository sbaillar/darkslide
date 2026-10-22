import Foundation

/// How a tool value is shown in its numeric field.
public enum ValueFormat: Equatable, Sendable {
    /// +0.30 / -1.25 / 0.00.
    case signedDecimal(Int)
    /// 0.8, 140.
    case decimal(Int)
    /// +25 / -10 / 0.
    case signedInteger
    case integer
    /// 5,500 K.
    case kelvin
    /// 0...1 shown as 0–100 %.
    case percent
    /// +1.5°.
    case degrees
    /// A lens blur amount 0...100 read as an aperture: Off, f/16 … f/1.2.
    case fStop

    public func string(_ v: Double) -> String {
        switch self {
        case .signedDecimal(let d): Formatters.signed(v, decimals: d)
        case .decimal(let d): String(format: "%.\(d)f", v)
        case .signedInteger: Formatters.signed(v.rounded(), decimals: 0)
        case .integer: String(Safe.clampedInt(v))
        case .kelvin: Formatters.kelvin(v)
        case .percent: "\(Safe.clampedInt(v * 100)) %"
        case .degrees: Formatters.signed(v, decimals: 1) + "°"
        case .fStop: v <= 0 ? "Off" : String(format: "f/%.1f", LensBlurEdit.fNumber(amount: v))
        }
    }

    /// Parse user input for this format ("5,500 K", "+0.3", "80 %", "-2°"). `nil` if not a number.
    public func parse(_ text: String) -> Double? {
        if self == .fStop {
            let t = text.lowercased().trimmingCharacters(in: .whitespaces)
            if t == "off" { return 0 }
            if t.hasPrefix("f"), let f = Double(t.filter { "0123456789.".contains($0) }), f > 0 {
                return LensBlurEdit.amount(fNumber: f)
            }
        }
        let cleaned = text.filter { "0123456789.-+".contains($0) }
        guard !cleaned.isEmpty, let v = Double(cleaned.hasPrefix("+") ? String(cleaned.dropFirst()) : cleaned) else {
            return nil
        }
        return self == .percent ? v / 100 : v
    }
}

/// Everything a slider needs: range, identity value, step, display format.
public struct SliderSpec: Equatable, Sendable {
    public let label: String
    public let range: ClosedRange<Double>
    public let defaultValue: Double
    public let step: Double
    public let format: ValueFormat
    public let help: String

    public init(_ label: String, _ range: ClosedRange<Double>, default defaultValue: Double = 0, step: Double = 1,
                format: ValueFormat = .signedInteger, help: String = "") {
        self.label = label
        self.range = range
        self.defaultValue = defaultValue
        self.step = step
        self.format = format
        self.help = help
    }

    /// Clamp into range and snap to `step`.
    public func normalize(_ v: Double) -> Double {
        let clamped = min(max(v.isFinite ? v : defaultValue, range.lowerBound), range.upperBound)
        return min(max(Self.snap(clamped, step: step), range.lowerBound), range.upperBound)
    }

    /// Round `v` to a multiple of `step` without binary noise (0.3, not 0.29999999999999998).
    public static func snap(_ v: Double, step: Double) -> Double {
        guard step > 0 else { return v }
        let n = (v / step).rounded()
        let inverse = (1 / step).rounded()
        return step < 1 && abs(inverse * step - 1) < 1e-9 ? n / inverse : n * step
    }

    /// Value for a double-click reset.
    public var resetValue: Double { defaultValue }

    public func isDefault(_ v: Double) -> Bool { abs(v - defaultValue) < step / 2 }

    /// Bipolar sliders (negative to positive, like Exposure) fill from the default outwards.
    public var isBipolar: Bool { range.lowerBound < 0 && range.upperBound > 0 }

    /// Value → 0...1 along the track.
    public func fraction(_ v: Double) -> Double {
        let span = range.upperBound - range.lowerBound
        return span > 0 ? min(max((v - range.lowerBound) / span, 0), 1) : 0
    }

    /// 0...1 along the track → normalized value.
    public func value(atFraction f: Double) -> Double {
        normalize(range.lowerBound + min(max(f, 0), 1) * (range.upperBound - range.lowerBound))
    }

    public func string(_ v: Double) -> String { format.string(v) }

    /// Parse and normalize typed input; `nil` leaves the value unchanged.
    public func parse(_ text: String) -> Double? { format.parse(text).map(normalize) }
}

/// The slider catalog. Defaults equal `EditState()` identity (tested).
public enum Specs {
    public static let temperature = SliderSpec("Kelvin", 2000...12000, default: 5500, step: 10, format: .kelvin,
                                               help: "Color temperature of the light. Lower cools, higher warms.")
    public static let tint = SliderSpec("Tint", -150...150, help: "Green (–) to magenta (+) balance.")
    public static let exposure = SliderSpec("Exposure", -5...5, step: 0.01, format: .signedDecimal(2),
                                            help: "Brightness in stops (EV).")
    public static let contrast = SliderSpec("Contrast", -100...100, help: "Separation between darks and lights.")
    public static let brightness = SliderSpec("Brightness", -100...100, help: "Midtone brightness, keeps black and white points.")
    public static let saturation = SliderSpec("Saturation", -100...100, help: "Overall color intensity.")
    public static let highlights = SliderSpec("Highlights", -100...100, help: "Recover (–) or boost (+) bright areas.")
    public static let shadows = SliderSpec("Shadows", -100...100, help: "Lift (+) or deepen (–) dark areas.")
    public static let whites = SliderSpec("White", -100...100, help: "Set the white point.")
    public static let blacks = SliderSpec("Black", -100...100, help: "Set the black point.")
    public static let clarity = SliderSpec("Clarity", -100...100, help: "Local midtone contrast.")
    public static let structure = SliderSpec("Structure", -100...100, help: "Fine-detail contrast.")
    public static let sharpenAmount = SliderSpec("Amount", 0...1000, default: 140, format: .integer,
                                                 help: "Output sharpening strength.")
    public static let sharpenRadius = SliderSpec("Radius", 0.2...2.5, default: 0.8, step: 0.1, format: .decimal(1),
                                                 help: "Size of the edges sharpened, in pixels.")
    public static let sharpenThreshold = SliderSpec("Threshold", 0...10, default: 1, step: 0.1, format: .decimal(1),
                                                    help: "Skip low-contrast detail (noise) below this level.")
    public static let noiseLuminance = SliderSpec("Luminance", 0...100, default: 25, format: .integer,
                                                  help: "Smooth brightness noise.")
    public static let noiseColor = SliderSpec("Color", 0...100, default: 25, format: .integer, help: "Remove color speckles.")
    public static let noiseDetail = SliderSpec("Detail", 0...100, default: 50, format: .integer,
                                               help: "Keep more fine texture when reducing noise.")
    public static let vignetteAmount = SliderSpec("Amount", -3...3, step: 0.01, format: .signedDecimal(2),
                                                  help: "Darken (–) or lighten (+) the corners, in stops.")
    public static let vignetteMidpoint = SliderSpec("Midpoint", 0...1, default: 0.5, step: 0.01, format: .percent,
                                                    help: "Where the falloff begins.")
    public static let distortion = SliderSpec("Distortion", 0...1, default: 1, step: 0.01, format: .percent,
                                              help: "Strength of the lens-profile distortion correction.")
    public static let lightFalloff = SliderSpec("Light Falloff", 0...1, default: 0.5, step: 0.01, format: .percent,
                                                help: "Strength of the corner-shading correction.")
    public static let keystoneVertical = SliderSpec("Vertical", -30...30, step: 0.1, format: .degrees,
                                                    help: "Correct converging verticals: + widens the top.")
    public static let keystoneHorizontal = SliderSpec("Horizontal", -30...30, step: 0.1, format: .degrees,
                                                      help: "Correct horizontal perspective: + widens the right side.")
    public static let straighten = SliderSpec("Rotation", -45...45, step: 0.1, format: .degrees,
                                              help: "Straighten the horizon.")
    public static let blurAmount = SliderSpec("Amount", 0...100, format: .fStop,
                                              help: "How much the out-of-focus parts blur, as a lens aperture (f/1.2 = the most).")
    public static let focalRange = SliderSpec("Focal Range", 0...100, default: 20, format: .integer,
                                              help: "How much depth around the focus stays sharp.")
    public static let highlightBoost = SliderSpec("Bokeh", 0...100, default: 30, format: .integer,
                                                  help: "How strongly bright out-of-focus lights bloom into discs.")
    public static let bandPosition = SliderSpec("Position", 0...1, default: 0.55, step: 0.01, format: .percent,
                                                help: "Height of the sharp band in the frame.")
    public static let bandAngle = SliderSpec("Angle", -90...90, step: 0.5, format: .degrees, help: "Tilt of the sharp band.")
    public static let bandWidth = SliderSpec("Width", 0...0.5, default: 0.06, step: 0.005, format: .percent,
                                             help: "Half the height of the sharp band, of the long edge.")
    public static let bandFeather = SliderSpec("Transition", 0.005...1, default: 0.22, step: 0.005, format: .percent,
                                               help: "How far the blur takes to reach its full amount.")
    public static let glowAmount = SliderSpec("Amount", 0...100, format: .integer, help: "A soft glow over the photo.")
    public static let glowSize = SliderSpec("Size", 0...100, default: 50, format: .integer, help: "How far the glow spreads.")
    public static let grainAmount = SliderSpec("Amount", 0...100, format: .integer, help: "Film grain strength.")
    public static let grainSize = SliderSpec("Size", 0...100, default: 25, format: .integer, help: "Grain size.")
    public static let grainRoughness = SliderSpec("Roughness", 0...100, default: 50, format: .integer,
                                                  help: "Even (0) to clumpy, uneven grain (100).")
    public static let halationAmount = SliderSpec("Halation", 0...100, format: .integer,
                                                  help: "The red glow film shows around bright highlights.")
    public static let wheelLightness = SliderSpec("Lightness", -1...1, step: 0.01, format: .signedDecimal(2),
                                                  help: "Darken or brighten this tonal range.")
    public static let bandHue = SliderSpec("Hue", -30...30, step: 0.5, format: .degrees, help: "Shift the hue of this color range.")
    public static let bandSaturation = SliderSpec("Saturation", -100...100, help: "Saturation of this color range.")
    public static let bandLightness = SliderSpec("Lightness", -100...100, help: "Lightness of this color range.")
    public static let layerOpacity = SliderSpec("Opacity", 0...1, default: 1, step: 0.01, format: .percent,
                                                help: "How strongly this layer applies.")
    public static let layerExposure = SliderSpec("Exposure", -5...5, step: 0.01, format: .signedDecimal(2),
                                                 help: "Layer exposure in stops.")
}
