import DarkslideFFI
import Foundation
import Observation

/// Settings › Archive: target, interval, on/off, and the Amazon Photos folder.
/// Every look at a volume runs off the main thread with a timeout.
@MainActor @Observable
public final class ArchiveSettingsModel {
    public enum TargetState: Equatable, Sendable {
        case notSet
        case checking
        case ready(fsType: String?)
        case unavailable(String)
        /// The volume didn't answer (a stale network mount).
        case notResponding
    }

    /// Checks a target (FFI `checkArchiveTarget`, bounded); tests inject fakes.
    public typealias Checker = @Sendable (String) async -> TargetState
    /// `true` when the path exists (run off-main, bounded).
    public typealias Exists = @Sendable (String) -> Bool

    public let settings: AppSettings
    public private(set) var targetState: TargetState = .notSet
    /// `/Volumes/NAS/Photos` when that share is mounted and no target is set.
    public private(set) var suggestion: String?
    /// `nil` until checked (L4: the last answer in this process is shown at once).
    public private(set) var amazonAppInstalled: Bool? = ArchiveSettingsModel.lastAppLookup.value
    /// The chosen Amazon Photos folder isn't there (and couldn't be followed).
    public private(set) var amazonFolderMissing = false
    /// The target folder doesn't exist but its parent is a mounted, writable folder:
    /// "Create It" makes it (C1; the archive creates its root only on the confirmed volume, N3).
    public private(set) var canCreateTarget = false
    /// The target folder itself exists (nil until checked). With ds-archive creating a
    /// missing folder under a mounted volume, the check can say "ready" before it exists:
    /// then the text says it will be created on the first sync.
    public private(set) var targetExists: Bool?
    /// The last "Create It" failure.
    public private(set) var createError: String?

    @ObservationIgnored private let checker: Checker
    @ObservationIgnored private let exists: Exists
    @ObservationIgnored private let appInstalled: @Sendable () -> Bool
    @ObservationIgnored private let timeout: Double
    @ObservationIgnored private let creatable: @Sendable (String) -> Bool
    @ObservationIgnored private let makeFolder: @Sendable (String) throws -> Void
    @ObservationIgnored private let volumeIdentity: ArchiveVolumeIdentity

    public nonisolated static let intervalChoices = [5, 10, 15, 30, 60]
    /// Where to get the official app.
    public nonisolated static let amazonInstallURL = URL(string: "https://www.amazon.com/photos")! // links to the Mac app download
    /// Bundle ids the Amazon Photos Mac app has shipped under.
    public nonisolated static let amazonBundleIDs = ["com.amazon.clouddrive.mac", "com.amazon.photos.mac"]

    public init(settings: AppSettings, checker: @escaping Checker = ArchiveSettingsModel.ffiCheck,
                exists: @escaping Exists = { FileManager.default.fileExists(atPath: $0) },
                appInstalled: @escaping @Sendable () -> Bool = ArchiveSettingsModel.amazonAppFound,
                creatable: @escaping @Sendable (String) -> Bool = ArchiveSettingsModel.canCreate,
                makeFolder: @escaping @Sendable (String) throws -> Void = {
                    try FileManager.default.createDirectory(atPath: $0, withIntermediateDirectories: false)
                },
                volumeIdentity: @escaping ArchiveVolumeIdentity = ArchiveVolume.identity,
                timeout: Double = 2) {
        self.volumeIdentity = volumeIdentity
        self.settings = settings
        self.checker = checker
        self.exists = exists
        self.appInstalled = appInstalled
        self.creatable = creatable
        self.makeFolder = makeFolder
        self.timeout = timeout
    }

    /// Blocking: the folder is missing and its parent exists and is writable (a mounted share).
    public nonisolated static let canCreate: @Sendable (String) -> Bool = { path in
        let fm = FileManager.default
        guard path.hasPrefix("/"), !fm.fileExists(atPath: path) else { return false }
        let parent = (path as NSString).deletingLastPathComponent
        return fm.fileExists(atPath: parent) && fm.isWritableFile(atPath: parent)
    }

    /// L1: what the Amazon hand-off takes.
    public nonisolated static let amazonVideosNote =
        "Photos only: videos are archived with the session but aren't handed to Amazon Photos."

    /// FFI check (the Rust side bounds the file-system call to 2 s).
    public nonisolated static let ffiCheck: Checker = { path in
        guard let r = try? await checkArchiveTarget(path: path, timeoutMs: 2000, token: CancelToken()) else {
            return .unavailable("could not check")
        }
        return state(r)
    }

    nonisolated static func state(_ r: TargetCheck) -> TargetState {
        if r.available { return .ready(fsType: r.fsType) }
        if r.timedOut { return .notResponding }
        return .unavailable(r.reason ?? "unavailable")
    }

    /// Launch Services by bundle id, then `/Applications/Amazon Photos.app`.
    public nonisolated static let amazonAppFound: @Sendable () -> Bool = {
        amazonBundleIDs.contains { PlatformWorkspace.hasApplication(bundleID: $0) }
            || ((try? amazonPhotosAppInstalled()) ?? false)
    }

    /// The Amazon app lookup's last answer in this process (L4).
    nonisolated static let lastAppLookup = AppLookupCache()

    final class AppLookupCache: @unchecked Sendable {
        private let lock = NSLock()
        private var stored: Bool?
        var value: Bool? {
            get { lock.withLock { stored } }
            set { lock.withLock { stored = newValue } }
        }
    }

    /// Re-check the target, the suggestion, the Amazon app and folder. The app lookup
    /// runs alongside the target check and shows as soon as it answers, not after
    /// the target check (L4: the text used to say "Checking…" for up to 2 s).
    public func refresh() async {
        let installed = appInstalled, timeout = timeout
        let app = Task { @MainActor [weak self] in
            let found = await blockingWithTimeout(timeout) { installed() }
            if let found { Self.lastAppLookup.value = found } // a timeout isn't an answer
            self?.amazonAppInstalled = found ?? false
        }
        await Task.yield() // the lookup starts before the (possibly slow) target check
        await checkTarget()
        await app.value
        await suggest()
        await checkAmazonFolder()
    }

    /// Follow a moved Amazon folder (bookmark), else report it missing (A-M2).
    public func checkAmazonFolder() async {
        await settings.followMovedAmazonFolder(timeout: timeout)
        guard let folder = settings.amazonPhotosFolder?.path else {
            amazonFolderMissing = false
            return
        }
        let exists = exists
        amazonFolderMissing = !(await blockingWithTimeout(timeout) { exists(folder) } ?? false)
    }

    /// Commit the typed target (Return or leaving the field; not per keystroke).
    public func commitTarget(_ typed: String) async {
        let t = typed.trimmingCharacters(in: .whitespaces)
        guard t != settings.archiveTarget.trimmingCharacters(in: .whitespaces) else { return }
        await setTarget(t)
    }

    public func checkTarget() async {
        let target = settings.archiveTarget.trimmingCharacters(in: .whitespaces)
        guard !target.isEmpty else {
            targetState = .notSet
            return
        }
        targetState = .checking
        let result = await checker(target)
        guard target == settings.archiveTarget.trimmingCharacters(in: .whitespaces) else { return } // typed on
        targetState = result
        let exists = exists, creatable = creatable
        targetExists = await blockingWithTimeout(timeout) { exists(target) }
        if case .ready = result {
            canCreateTarget = false // the archive creates it on its first sync
        } else {
            canCreateTarget = await blockingWithTimeout(timeout) { creatable(target) } ?? false
        }
    }

    /// "Create It": make the archive folder the user chose (never done on its own, C1).
    public func createTarget() async {
        let target = settings.archiveTarget.trimmingCharacters(in: .whitespaces)
        guard canCreateTarget, !target.isEmpty else { return }
        let make = makeFolder
        let outcome: String?? = await blockingWithTimeout(timeout) {
            do {
                try make(target)
                return String?.none
            } catch {
                return error.localizedDescription
            }
        }
        switch outcome {
        case .none: createError = "The volume didn't answer. Try again when it's back."
        case .some(let message): createError = message
        }
        await checkTarget()
        await settings.confirmArchiveTarget(volumeIdentity: volumeIdentity, timeout: timeout) // N3
    }

    /// Offer `/Volumes/NAS/Photos` only when no target is set and the share is mounted.
    public func suggest() async {
        guard settings.archiveTarget.trimmingCharacters(in: .whitespaces).isEmpty else {
            suggestion = nil
            return
        }
        let share = URL(fileURLWithPath: AppSettings.defaultArchiveTarget).deletingLastPathComponent().path
        let exists = exists
        let mounted = await blockingWithTimeout(timeout) { exists(share) } ?? false
        suggestion = mounted && settings.archiveTarget.isEmpty ? AppSettings.defaultArchiveTarget : nil
    }

    /// Use the suggested share folder; choosing it is explicit, so it's created when missing (C1).
    public func useSuggestion() async {
        guard let suggestion else { return }
        settings.archiveTarget = suggestion
        settings.archiveEnabled = true
        self.suggestion = nil
        await checkTarget()
        if canCreateTarget { await createTarget() } else { await settings.confirmArchiveTarget(volumeIdentity: volumeIdentity, timeout: timeout) }
    }

    public func setTarget(_ path: String) async {
        settings.archiveTarget = path
        await checkTarget()
        await settings.confirmArchiveTarget(volumeIdentity: volumeIdentity, timeout: timeout) // N3
        await suggest()
    }

    public var targetText: String {
        switch targetState {
        case .notSet: "No archive target: nothing is archived."
        case .checking: "Checking…"
        case .ready(let fs) where targetExists == false:
            "Ready: the folder is created on the first sync" + (fs.map { " (\($0))" } ?? "") + "."
        case .ready(let fs): "Ready" + (fs.map { " (\($0))" } ?? "")
        case .unavailable(let why) where canCreateTarget:
            "This folder doesn't exist yet (\(why)). Create it to start archiving."
        case .unavailable(let why): "Not available: \(why). Changes are queued until it is."
        case .notResponding: "The volume isn't responding. Changes are queued; the app keeps working."
        }
    }

    public var amazonText: String {
        if amazonFolderMissing, let f = settings.amazonPhotosFolder {
            return "Folder missing: \(f.path). It was moved or deleted, or its disk isn't connected. "
                + "Darkslide never recreates it: choose it again."
        }
        return switch amazonAppInstalled {
        case nil: "Originals are handed to the folder the Amazon Photos app backs up."
        case true?: settings.amazonPhotosFolder == nil
            ? "Amazon Photos is installed. Choose the folder it backs up."
            : "Originals are handed to Amazon Photos after each import."
        case false?: "The Amazon Photos app isn't installed. It uploads the folder; Darkslide only fills it."
        }
    }
}
