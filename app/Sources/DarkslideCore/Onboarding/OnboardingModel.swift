import Foundation
import Observation

/// First-run setup: where sessions live, where the archive goes, optional
/// Amazon Photos folder. Nothing is written until `finish()`.
@MainActor @Observable
public final class OnboardingModel {
    public enum Step: Int, CaseIterable, Sendable {
        case welcome, familiar, sessions, archive, amazon

        public var title: String {
            switch self {
            case .welcome: "Welcome to Darkslide"
            case .familiar: "What did you use before?"
            case .sessions: "Where should sessions live?"
            case .archive: "Archive"
            case .amazon: "Amazon Photos (optional)"
            }
        }
    }

    public enum TargetStatus: Equatable, Sendable {
        /// The target folder exists.
        case ready
        case notMounted(String)
        /// The target doesn't exist and can't be created here (its parent is missing or read-only).
        case missing
        /// The target doesn't exist; its parent does and is writable, so Finish (or Create) makes it.
        case creatable
        /// Not a usable path ("relative/path", "/").
        case invalid(String)
        /// Not checked yet.
        case checking
        /// The check didn't answer in time (hung or slow network mount).
        case unreachable
        /// No target entered.
        case empty
    }

    public var step: Step = .welcome
    public var sessionsFolder: URL
    public var archiveTarget: String
    public var archiveEnabled = true
    public var amazonFolder: URL?
    /// The familiar layout (and its keys and names) to start with.
    public var familiar: LayoutPreset

    @ObservationIgnored private let settings: AppSettings
    /// Result of the last off-main check; see `refreshArchiveStatus()`.
    public private(set) var archiveStatus: TargetStatus = .checking
    @ObservationIgnored private let fileExists: @Sendable (String) -> Bool
    @ObservationIgnored private let timeout: Double
    @ObservationIgnored private let createDirectory: (URL) throws -> Void
    @ObservationIgnored private let isWritable: @Sendable (String) -> Bool
    @ObservationIgnored private let createTarget: (URL) throws -> Void
    @ObservationIgnored private let volumeIdentity: ArchiveVolumeIdentity

    public init(settings: AppSettings,
                fileExists: @escaping @Sendable (String) -> Bool = { FileManager.default.fileExists(atPath: $0) },
                createDirectory: @escaping (URL) throws -> Void = {
                    try FileManager.default.createDirectory(at: $0, withIntermediateDirectories: true)
                },
                isWritable: @escaping @Sendable (String) -> Bool = { FileManager.default.isWritableFile(atPath: $0) },
                createTarget: @escaping (URL) throws -> Void = {
                    // Only the archive folder itself: its parent (the share) must already be there.
                    try FileManager.default.createDirectory(at: $0, withIntermediateDirectories: false)
                },
                volumeIdentity: @escaping ArchiveVolumeIdentity = ArchiveVolume.identity,
                timeout: Double = 1) {
        self.volumeIdentity = volumeIdentity
        self.settings = settings
        self.timeout = timeout
        self.fileExists = fileExists
        self.createDirectory = createDirectory
        self.isWritable = isWritable
        self.createTarget = createTarget
        sessionsFolder = settings.sessionsFolder
        archiveTarget = settings.archiveTarget
        // Archiving is on by default; an empty target asks the user to choose one (or turn it off).
        archiveEnabled = true
        amazonFolder = settings.amazonPhotosFolder
        familiar = settings.layoutPreset
    }

    public var isFirst: Bool { step == .welcome }
    public var isLast: Bool { step == Step.allCases.last }
    public var progress: Double { Double(step.rawValue) / Double(Step.allCases.count - 1) }

    public func next() {
        if let n = Step(rawValue: step.rawValue + 1) { step = n }
    }

    public func back() {
        if let p = Step(rawValue: step.rawValue - 1) { step = p }
    }

    /// The typed target as a path: whitespace trimmed, `~` expanded.
    public nonisolated static func normalized(_ target: String) -> String {
        (target.trimmingCharacters(in: .whitespaces) as NSString).expandingTildeInPath
    }

    /// Whether `target` is usable now (pure; runs off-main). Ready only when the folder
    /// itself exists (the archive creates its root only on the confirmed volume, N3); a missing folder
    /// whose parent is there and writable is `.creatable` (Finish or "Create It" makes it).
    /// A `/Volumes/<share>` target whose share isn't mounted is fine: syncs queue until it appears.
    public nonisolated static func status(for target: String, fileExists: (String) -> Bool,
                                          isWritable: (String) -> Bool = { _ in true }) -> TargetStatus {
        let path = normalized(target)
        guard !path.isEmpty else { return .empty }
        guard path.hasPrefix("/") else { return .invalid("Use a full path, like /Volumes/NAS/Photos.") }
        let url = URL(fileURLWithPath: path).standardizedFileURL
        let comps = url.pathComponents
        let wholeDisk = comps.count <= 1 || (comps[1] == "Volumes" && comps.count <= 3)
        guard !wholeDisk else { return .invalid("Choose a folder, not a whole disk.") }
        if comps.count >= 3, comps[1] == "Volumes", !fileExists("/Volumes/\(comps[2])") {
            return .notMounted(comps[2])
        }
        if fileExists(url.path) { return .ready }
        let parent = url.deletingLastPathComponent().path
        return fileExists(parent) && isWritable(parent) ? .creatable : .missing
    }

    /// Re-check the archive target on a worker thread with a timeout (never stats NFS on main).
    public func refreshArchiveStatus() async {
        let target = archiveTarget
        let exists = fileExists
        let writable = isWritable
        archiveStatus = .checking
        let result = await blockingWithTimeout(timeout) { Self.status(for: target, fileExists: exists, isWritable: writable) }
        guard target == archiveTarget else { return } // the user typed on; a newer check will run
        archiveStatus = result ?? .unreachable
    }

    /// Suggest `/Volumes/NAS/Photos` only when that share is actually mounted (checked off-main).
    public func suggestArchiveTarget() async {
        guard archiveTarget.trimmingCharacters(in: .whitespaces).isEmpty else { return }
        let exists = fileExists
        let share = URL(fileURLWithPath: AppSettings.defaultArchiveTarget).deletingLastPathComponent().path
        if await blockingWithTimeout(timeout, { exists(share) }) == true, archiveTarget.isEmpty {
            archiveTarget = AppSettings.defaultArchiveTarget
            archiveEnabled = true
        }
        await refreshArchiveStatus()
    }

    /// Finish needs a sessions folder and, with archiving on, a target that exists, can be
    /// created, or sits on a share that isn't mounted right now (M6).
    public var canFinish: Bool {
        guard !sessionsFolder.path.isEmpty else { return false }
        guard archiveEnabled else { return true }
        switch archiveStatus {
        case .ready, .creatable, .notMounted, .unreachable: return true
        case .missing, .invalid, .empty, .checking: return false
        }
    }

    /// "Create It": make the archive folder now (the user asked; its parent must be there).
    public func createArchiveTarget() async throws {
        guard archiveStatus == .creatable else { return }
        try createTarget(URL(fileURLWithPath: Self.normalized(archiveTarget)))
        await refreshArchiveStatus()
    }

    /// Persist choices, create the sessions folder, and create the archive folder when the
    /// user chose one that doesn't exist yet on a mounted, writable volume (C1).
    public func finish() throws {
        try createDirectory(sessionsFolder)
        let target = archiveEnabled ? Self.normalized(archiveTarget) : ""
        if archiveEnabled, archiveStatus == .creatable {
            try createTarget(URL(fileURLWithPath: target))
        }
        settings.sessionsFolder = sessionsFolder
        settings.archiveTarget = target
        settings.chooseAmazonFolder(amazonFolder) // M7: keeps a bookmark so a moved folder is followed
        if familiar != settings.layoutPreset || KeyPreset(layout: familiar) != settings.keyPreset {
            settings.layoutPreset = familiar
            settings.namePreset = nil
            settings.toolSidePreset = nil
            settings.useKeyPreset(KeyPreset(layout: familiar))
        }
        settings.onboardingComplete = true
    }

    /// N3: after Finish, record the confirmed target's volume identity (bounded, off-main).
    public func recordTargetVolume() async {
        await settings.confirmArchiveTarget(volumeIdentity: volumeIdentity, timeout: timeout)
    }
}
