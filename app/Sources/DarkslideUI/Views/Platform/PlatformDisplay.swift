#if os(macOS)
import AppKit
#else
import UIKit
#endif
import SwiftUI

/// What the screen showing the app can display.
@MainActor
enum PlatformDisplay {
    #if os(macOS)
    private static var screen: NSScreen? { NSApp.keyWindow?.screen ?? NSScreen.main }

    /// The screen covers Display P3 (8-bit renders are P3 there).
    static func wideGamut() -> Bool { screen?.canRepresent(.p3) ?? false }

    /// EDR headroom (1 = SDR only).
    static func edrHeadroom() -> Double { Double(screen?.maximumPotentialExtendedDynamicRangeColorComponentValue ?? 1) }

    /// Posted when the window moves to another screen (re-read the two values above).
    static let screenChanged = NSWindow.didChangeScreenNotification
    #else
    // iOS (unverified): one built-in screen. P3 on every iPad that runs iPadOS 26.
    static func wideGamut() -> Bool { UIScreen.main.traitCollection.displayGamut == .P3 }
    static func edrHeadroom() -> Double { Double(UIScreen.main.potentialEDRHeadroom) }
    static let screenChanged = UIScreen.didConnectNotification
    #endif
}
