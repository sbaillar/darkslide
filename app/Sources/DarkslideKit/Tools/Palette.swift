import Foundation

/// Neutral grey levels (sRGB-encoded 0...1) behind the UI color tokens, per
/// appearance. Kept as data so contrast can be unit-tested (WCAG 2.x).
public struct GreyPalette: Equatable, Sendable {
    public var viewer: Double
    public var panel: Double
    public var chrome: Double
    public var cell: Double
    public var textPrimary: Double
    public var textSecondary: Double
    public var textTertiary: Double
    /// The system window background sheets and popovers sit on (not ours: what
    /// `NSColor.windowBackgroundColor` resolves to; a test checks it still does).
    public var window: Double

    /// Tertiary is ≥ 4.5:1 on panel, chrome and window; secondary and primary more so.
    public static let dark = GreyPalette(viewer: 0.105, panel: 0.155, chrome: 0.13, cell: 0.19,
                                         textPrimary: 0.93, textSecondary: 0.68, textTertiary: 0.58, window: 0.118)
    public static let light = GreyPalette(viewer: 0.80, panel: 0.93, chrome: 0.89, cell: 0.97,
                                          textPrimary: 0.10, textSecondary: 0.33, textTertiary: 0.38, window: 1.0)

    /// Background/foreground pairs that must meet 4.5:1 (normal-size text). The viewer
    /// surround carries only primary and secondary text (empty states, banners).
    public var textPairs: [(name: String, background: Double, foreground: Double)] {
        [("panel", panel), ("chrome", chrome), ("window", window)].flatMap { place, bg in
            [("primary on \(place)", bg, textPrimary), ("secondary on \(place)", bg, textSecondary),
             ("tertiary on \(place)", bg, textTertiary)]
        } + [("primary on viewer", viewer, textPrimary), ("secondary on viewer", viewer, textSecondary)]
    }
}

/// Darkslide's darkroom warmth: a slight amber cast on the panels and toolbar only. The
/// viewer surround stays exactly neutral so it never shifts how a photo's colour reads.
/// Offsets are small enough (≤ 0.02 per channel) that the grey contrast checks still hold.
public struct ChromeTint: Equatable, Sendable {
    public var red: Double
    public var green: Double
    public var blue: Double

    public static let dark = ChromeTint(red: 0.014, green: 0.004, blue: -0.010)
    public static let light = ChromeTint(red: 0.008, green: 0.002, blue: -0.014)

    /// The tinted sRGB channels for a grey level, clamped to 0…1.
    public func apply(_ v: Double) -> (r: Double, g: Double, b: Double) {
        (min(max(v + red, 0), 1), min(max(v + green, 0), 1), min(max(v + blue, 0), 1))
    }

    /// The largest change any channel gets.
    public var maxOffset: Double { [red, green, blue].map(abs).max() ?? 0 }
}

/// WCAG relative luminance and contrast ratio.
public enum Contrast {
    /// Relative luminance of an sRGB-encoded channel value (grey: all channels equal).
    public static func luminance(_ v: Double) -> Double {
        let c = min(max(v, 0), 1)
        return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4)
    }

    /// Relative luminance of an sRGB color.
    public static func luminance(r: Double, g: Double, b: Double) -> Double {
        0.2126 * luminance(r) + 0.7152 * luminance(g) + 0.0722 * luminance(b)
    }

    /// (L1 + 0.05) / (L2 + 0.05), ≥ 1, order-independent.
    public static func ratio(_ l1: Double, _ l2: Double) -> Double {
        (max(l1, l2) + 0.05) / (min(l1, l2) + 0.05)
    }

    public static func ratio(grey a: Double, grey b: Double) -> Double {
        ratio(luminance(a), luminance(b))
    }
}

/// An sRGB-encoded colour (0...1 per channel), kept as data for contrast tests.
public struct RGBColor: Equatable, Sendable {
    public var r: Double, g: Double, b: Double
    public init(_ r: Double, _ g: Double, _ b: Double) {
        self.r = r
        self.g = g
        self.b = b
    }

    public var luminance: Double { Contrast.luminance(r: r, g: g, b: b) }

    /// Contrast against a neutral grey level.
    public func contrast(onGrey grey: Double) -> Double { Contrast.ratio(luminance, Contrast.luminance(grey)) }
}

/// The chromatic tokens per appearance (M10). The dark-mode amber and yellow read at
/// about 1.5:1 on the light greys, so light mode uses darker variants: the accent is used
/// as text (>= 4.5:1), stars and the yellow tag are icons (>= 3:1).
public struct AccentPalette: Equatable, Sendable {
    public var accent: RGBColor
    public var star: RGBColor
    public var yellowTag: RGBColor
    /// The outline and badges of settings an AI edit changed: a clear yellow, apart from
    /// the amber accent (an outline: >= 3:1).
    public var aiChange: RGBColor

    public static let dark = AccentPalette(accent: RGBColor(1.0, 0.66, 0.26), star: RGBColor(1.0, 0.80, 0.30),
                                           yellowTag: RGBColor(0.98, 0.80, 0.25), aiChange: RGBColor(1.0, 0.88, 0.16))
    public static let light = AccentPalette(accent: RGBColor(0.55, 0.33, 0.02), star: RGBColor(0.66, 0.45, 0.0),
                                            yellowTag: RGBColor(0.66, 0.45, 0.0), aiChange: RGBColor(0.56, 0.47, 0.0))

    /// Pairs that must meet a minimum contrast: (name, colour, background grey, minimum).
    public func pairs(on greys: GreyPalette) -> [(name: String, color: RGBColor, background: Double, minimum: Double)] {
        [greys.panel, greys.chrome, greys.cell].enumerated().flatMap { i, bg in
            let place = ["panel", "chrome", "cell"][i]
            return [("accent text on \(place)", accent, bg, 4.5),
                    ("star on \(place)", star, bg, 3.0),
                    ("yellow tag on \(place)", yellowTag, bg, 3.0),
                    ("AI change on \(place)", aiChange, bg, 3.0)]
        }
    }
}
