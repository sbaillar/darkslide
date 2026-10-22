#if os(macOS)
import AppKit
#else
import UIKit
#endif

/// VoiceOver announcements (`Announcer`'s default sink).
@MainActor
enum PlatformAccessibility {
    static func announce(_ text: String, urgent: Bool) {
        #if os(macOS)
        guard NSWorkspace.shared.isVoiceOverEnabled else { return }
        let level: NSAccessibilityPriorityLevel = urgent ? .high : .medium
        NSAccessibility.post(element: NSApp.mainWindow as Any, notification: .announcementRequested,
                             userInfo: [.announcement: text, .priority: level.rawValue])
        #else
        // iOS (unverified).
        guard UIAccessibility.isVoiceOverRunning else { return }
        UIAccessibility.post(notification: .announcement, argument: text)
        #endif
    }
}
