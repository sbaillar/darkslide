import Foundation

/// Named white balance presets (Kelvin + tint), as in camera menus.
public enum WhiteBalancePreset: String, CaseIterable, Sendable, Identifiable {
    case daylight = "Daylight", cloudy = "Cloudy", shade = "Shade", tungsten = "Tungsten"
    case fluorescent = "Fluorescent", flash = "Flash"

    public var id: String { rawValue }

    public var temperature: Double {
        switch self {
        case .daylight: 5500
        case .cloudy: 6500
        case .shade: 7500
        case .tungsten: 2850
        case .fluorescent: 3800
        case .flash: 5900
        }
    }

    public var tint: Double {
        switch self {
        case .fluorescent: 21
        case .flash: 3
        default: 0
        }
    }

    public var symbol: String {
        switch self {
        case .daylight: "sun.max"
        case .cloudy: "cloud"
        case .shade: "building.2"
        case .tungsten: "lightbulb"
        case .fluorescent: "light.panel"
        case .flash: "bolt"
        }
    }

    public func applied(to wb: WhiteBalanceEdit) -> WhiteBalanceEdit {
        var o = wb
        o.asShot = false
        o.temperature = temperature
        o.tint = tint
        return o
    }
}

/// The WB eyedropper: click a spot that should be neutral grey.
///
/// Sign convention (Lightroom-style): the temperature setting describes the
/// light to correct for, so a warm/orange sample needs a LOWER Kelvin to
/// neutralize, and a green sample needs a positive (magenta) tint.
public enum WhiteBalancePicker {
    /// Mired change per unit of ln(red/blue) in linear light (≈ daylight→tungsten spans ln 2.5).
    static let miredPerLogRB = 140.0
    /// Tint units per unit of ln(green / sqrt(red·blue)).
    static let tintPerLogG = 120.0

    public static func neutralize(_ wb: WhiteBalanceEdit, sampledRGB: (Double, Double, Double)) -> WhiteBalanceEdit {
        func linear(_ v: Double) -> Double {
            let c = min(max(v, 0), 1)
            let l = c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
            return max(l, 1e-4)
        }
        let (r, g, b) = (linear(sampledRGB.0), linear(sampledRGB.1), linear(sampledRGB.2))
        let base = wb.asShot ? 5500 : wb.temperature
        let mired = 1e6 / base + miredPerLogRB * log(r / b)
        var o = wb
        o.asShot = false
        o.temperature = min(max(1e6 / max(mired, 20), 2000), 50000)
        o.tint = min(max((wb.asShot ? 0 : wb.tint) + tintPerLogG * log(g / (r * b).squareRoot()), -150), 150)
        return o
    }
}
