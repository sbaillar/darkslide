#if os(macOS)
import AppKit
#else
import UIKit
#endif
import DarkslideCore
import SwiftUI

extension Color {
    /// A colour that follows the light/dark appearance: `rgb(isDark)` gives its sRGB components.
    static func adaptive(_ rgb: @escaping @Sendable (_ isDark: Bool) -> DarkslideCore.RGBColor) -> Color {
        #if os(macOS)
        Color(nsColor: NSColor(name: nil) { a in
            let c = rgb(a.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua)
            return NSColor(srgbRed: c.r, green: c.g, blue: c.b, alpha: 1)
        })
        #else
        // iOS (unverified).
        Color(uiColor: UIColor { traits in
            let c = rgb(traits.userInterfaceStyle == .dark)
            return UIColor(red: c.r, green: c.g, blue: c.b, alpha: 1)
        })
        #endif
    }
}
