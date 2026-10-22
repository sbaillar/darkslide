import Foundation

/// What keeps the viewer from showing the focused photo's develop: the banner over it
/// says so, with what can be done. Never a silent viewer.
public enum ViewerProblem: Equatable, Sendable {
    /// Every original of the open session is missing (a removed card, an unmounted
    /// drive, a deleted folder), or the drive its folder is on (`volume`) isn't
    /// connected: one session-level message.
    case sessionMissing(folder: String, volume: String? = nil)
    /// The focused photo's original is missing or unreadable. `canRemove`: the store
    /// verified it missing on a drive that is connected (Remove is offered).
    case missingOriginal(name: String, canRemove: Bool = false)
    /// The focused photo's original is verified missing, but the disk mounted as its
    /// `volume` isn't the one recorded for the session (a clone, a restore, another disk
    /// under the same name): Remove is held back until Locate Folder confirms it.
    case missingOnOtherVolume(name: String, volume: String)
    /// The drive the focused photo's original is on isn't connected: nothing to remove.
    case driveDisconnected(volume: String)
    /// The develop render failed (GPU, decode): the reason, with Retry.
    case renderFailed(String)
    /// The file is a RAW Darkslide can't develop yet (an X-Trans sensor, a codec
    /// rawler lacks): the reason. It stays in the session and is culled from its
    /// embedded preview; retrying won't help, so there is no button.
    case cannotDevelop(String)

    public enum Action: String, Sendable, CaseIterable {
        case locateFolder, removeFromRecents, locateFile, removePhoto, retry
    }

    public var message: String {
        switch self {
        case .sessionMissing(let folder, let volume?):
            "Drive not connected — reconnect “\(volume)” to see the originals of “\(folder)”: "
                + "edits are saved but can’t be previewed or exported."
        case .sessionMissing(let folder, nil):
            "The originals of “\(folder)” aren’t there (a removed card, an unmounted drive or a deleted folder): "
                + "edits are saved but can’t be previewed or exported."
        case .missingOriginal(let name, _):
            "Original file missing (\(name)): edits are saved but can’t be previewed or exported."
        case .missingOnOtherVolume(let name, let volume):
            "Original file missing (\(name)). Remove isn’t offered: “\(volume)” isn’t the disk this session was "
                + "recorded on (a clone or a restore?). Locate Folder… confirms it."
        case .driveDisconnected(let volume):
            "Drive not connected — reconnect “\(volume)”: edits are saved but can’t be previewed or exported."
        case .renderFailed(let reason):
            "This photo couldn’t be developed: \(reason)"
        case .cannotDevelop(let reason):
            "Darkslide can’t develop this file yet (\(reason)). You’re seeing its embedded preview: "
                + "you can still rate, tag and compare it."
        }
    }

    /// The banner's buttons, in order, with their titles.
    public var actions: [(action: Action, title: String)] {
        switch self {
        case .sessionMissing: [(.locateFolder, "Locate Folder…"), (.removeFromRecents, "Remove from Recents")]
        case .missingOriginal(_, true): [(.locateFile, "Locate…"), (.removePhoto, "Remove…")]
        case .missingOriginal(_, false): [(.locateFile, "Locate…")]
        case .missingOnOtherVolume: [(.locateFile, "Locate…"), (.locateFolder, "Locate Folder…")]
        case .driveDisconnected: []
        case .renderFailed: [(.retry, "Retry")]
        case .cannotDevelop: []
        }
    }

    public var symbol: String {
        switch self {
        case .sessionMissing: "externaldrive.badge.questionmark"
        case .missingOriginal, .missingOnOtherVolume: "questionmark.folder"
        case .driveDisconnected: "externaldrive.badge.xmark"
        case .renderFailed: "exclamationmark.triangle"
        case .cannotDevelop: "photo.badge.exclamationmark"
        }
    }

    /// Shown over the grid too (it isn't about one photo).
    public var isSessionWide: Bool {
        if case .sessionMissing = self { return true }
        return false
    }
}
