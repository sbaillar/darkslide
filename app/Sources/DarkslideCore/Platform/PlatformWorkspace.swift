#if os(macOS)
import AppKit
#else
import Foundation
#endif

/// The Mac's workspace services (Finder, Launch Services, volume mounts) behind one seam.
/// iPadOS has no Finder, no other apps to look up and no user-visible mounts.
public enum PlatformWorkspace {
    #if os(macOS)
    /// Where volume mount and unmount notifications are posted.
    public static var notificationCenter: NotificationCenter { NSWorkspace.shared.notificationCenter }
    public static var didMount: Notification.Name { NSWorkspace.didMountNotification }
    public static var didUnmount: Notification.Name { NSWorkspace.didUnmountNotification }
    /// The mounted or unmounted volume's URL in a mount notification's `userInfo`.
    public static var volumeURLKey: String { NSWorkspace.volumeURLUserInfoKey }

    /// Show `urls` selected in the Finder.
    public static func reveal(_ urls: [URL]) { NSWorkspace.shared.activateFileViewerSelecting(urls) }

    /// Whether Launch Services knows an app with this bundle identifier.
    public static func hasApplication(bundleID: String) -> Bool {
        NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID) != nil
    }

    /// Unmount and eject a volume (blocks while the disk flushes).
    public static func unmountAndEject(_ volume: URL) throws { try NSWorkspace.shared.unmountAndEjectDevice(at: volume) }

    /// Mounted volumes, without the hidden ones unless `includeHidden`.
    public static func mountedVolumes(includeHidden: Bool) -> [URL] {
        FileManager.default.mountedVolumeURLs(includingResourceValuesForKeys: nil,
                                              options: includeHidden ? [] : [.skipHiddenVolumes]) ?? []
    }

    /// The user's home folder (not the sandbox container).
    public static var homeDirectory: URL { FileManager.default.homeDirectoryForCurrentUser }

    /// The computer's name as the user set it (Sharing settings).
    public static var computerName: String? { Host.current().localizedName }
    #else
    // iOS (unverified): no mounts to watch (a private center nothing posts to), no Finder,
    // no other apps; cards come through ImageCaptureCore (docs/IPAD_PLAN.md, phase 4).
    public static let notificationCenter = NotificationCenter()
    public static var didMount: Notification.Name { Notification.Name("DarkslideVolumeDidMount") }
    public static var didUnmount: Notification.Name { Notification.Name("DarkslideVolumeDidUnmount") }
    public static var volumeURLKey: String { "DarkslideVolumeURL" }
    public static func reveal(_ urls: [URL]) {}
    public static func hasApplication(bundleID: String) -> Bool { false }
    public static func unmountAndEject(_ volume: URL) throws { throw CocoaError(.featureUnsupported) }
    public static func mountedVolumes(includeHidden: Bool) -> [URL] { [] }
    public static var homeDirectory: URL { URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true) }
    public static var computerName: String? { nil }
    #endif
}
