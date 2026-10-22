import DarkslideCore
import SwiftUI

/// Design tokens. 8 pt spacing grid, a neutral viewer so photos carry the colour, a faint
/// darkroom warmth on the panels and toolbar (`ChromeTint`), one amber accent (the
/// darkslide's pull tab, a safelight).
public enum DS {
    public enum Space {
        public static let xxs: CGFloat = 2
        public static let xs: CGFloat = 4
        public static let s: CGFloat = 8
        public static let m: CGFloat = 16
        public static let l: CGFloat = 24
        public static let xl: CGFloat = 32
    }

    public enum Radius {
        public static let s: CGFloat = 4
        public static let m: CGFloat = 8
        public static let l: CGFloat = 12
    }

    public enum Size {
        public static let toolPanel: CGFloat = 320
        public static let filmstrip: CGFloat = 112
        public static let sliderRow: CGFloat = 22
    }

    /// Warm amber accent (darker in light mode so it reads as text, M10).
    public static let accent = adaptive(\.accent)
    public static let pick = Color(red: 0.36, green: 0.84, blue: 0.47)
    public static let reject = Color(red: 0.95, green: 0.33, blue: 0.33)
    public static let star = adaptive(\.star)
    /// Settings an AI edit changed: their outline and badges (`AIHighlightStyle`).
    public static let aiChange = adaptive(\.aiChange)
    /// Warning text (the star amber, readable in both appearances).
    public static let warningText = adaptive(\.star)

    /// A colour that follows the appearance, from the tested `AccentPalette`.
    static func adaptive(_ token: KeyPath<AccentPalette, DarkslideCore.RGBColor>) -> Color {
        let (dark, light) = (AccentPalette.dark[keyPath: token], AccentPalette.light[keyPath: token])
        return Color.adaptive { $0 ? dark : light }
    }

    /// Histogram / curve box background (M10): the viewer grey, dark in dark mode and light in light.
    public static let plotBackground = grey(\.viewer)

    /// A grey that follows the appearance, from the tested `GreyPalette`.
    static func grey(_ level: KeyPath<GreyPalette, Double>) -> Color {
        let (dark, light) = (GreyPalette.dark[keyPath: level], GreyPalette.light[keyPath: level])
        return Color.adaptive { isDark in
            let v = isDark ? dark : light
            return DarkslideCore.RGBColor(v, v, v)
        }
    }

    /// A panel/toolbar grey with the darkroom warmth (`ChromeTint`).
    static func warm(_ level: KeyPath<GreyPalette, Double>) -> Color {
        let (dark, light) = (GreyPalette.dark[keyPath: level], GreyPalette.light[keyPath: level])
        return Color.adaptive { isDark in
            let c = (isDark ? ChromeTint.dark : ChromeTint.light).apply(isDark ? dark : light)
            return DarkslideCore.RGBColor(c.r, c.g, c.b)
        }
    }

    /// Neutral viewer surround (never tinted).
    public static let viewerBackground = grey(\.viewer)
    public static let panelBackground = warm(\.panel)
    public static let chrome = warm(\.chrome)
    public static let cellBackground = warm(\.cell)
    /// Text tokens: all ≥ 4.5:1 on panel and chrome backgrounds (see PaletteTests).
    public static let text = grey(\.textPrimary)
    public static let textSecondary = grey(\.textSecondary)
    public static let textTertiary = grey(\.textTertiary)
    public static let hairline = Color.primary.opacity(0.09)

    public static func tagColor(_ t: ColorTag) -> Color {
        switch t {
        case .none: .clear
        case .red: Color(red: 0.93, green: 0.30, blue: 0.30)
        case .yellow: adaptive(\.yellowTag)
        case .green: Color(red: 0.35, green: 0.78, blue: 0.40)
        case .blue: Color(red: 0.30, green: 0.55, blue: 0.95)
        case .purple: Color(red: 0.65, green: 0.40, blue: 0.90)
        case .orange: Color(red: 0.98, green: 0.58, blue: 0.20)
        case .pink: Color(red: 0.95, green: 0.45, blue: 0.70)
        }
    }

    public enum Font {
        /// Panel headers: small caps-like, tracked.
        public static let panelTitle = SwiftUI.Font.system(size: 11, weight: .semibold)
        public static let label = SwiftUI.Font.system(size: 11)
        public static let value = SwiftUI.Font.system(size: 11).monospacedDigit()
        public static let caption = SwiftUI.Font.system(size: 10.5)
        public static let title = SwiftUI.Font.system(size: 20, weight: .semibold)
    }
}
