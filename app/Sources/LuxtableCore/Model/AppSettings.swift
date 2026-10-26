import LuxtableFFI
import Foundation
import Observation

/// How Copy picks the groups it takes (see `AppSettings.copyMode`).
public enum CopyMode: String, CaseIterable, Sendable {
    case edited, custom
}

/// [library] The view the app starts in: the last session (the default), or the Library
/// (every session's photos), which opens no session until one is needed.
public enum StartView: String, CaseIterable, Sendable {
    case session, library

    public var title: String {
        switch self {
        case .session: "Session"
        case .library: "Library (catalog)"
        }
    }
}

public enum AppearancePreference: String, CaseIterable, Sendable {
    case dark = "Dark", light = "Light", system = "System"
}

/// User preferences, persisted in `UserDefaults` (a suite can be injected for tests).
@MainActor @Observable
public final class AppSettings {
    /// Suggested (not defaulted) archive target: a mounted network share, when mounted.
    public static let defaultArchiveTarget = "/Volumes/NAS/Photos"

    @ObservationIgnored private let defaults: UserDefaults
    /// The suite these settings live in (the snapshot suite in `--snapshot` runs,
    /// a test suite in tests): other stores (export recipes) use it too (M4).
    public var userDefaults: UserDefaults { defaults }

    public var onboardingComplete: Bool { didSet { defaults.set(onboardingComplete, forKey: Keys.onboarding) } }
    public var sessionsFolder: URL { didSet { defaults.set(sessionsFolder.path, forKey: Keys.sessions) } }
    public var archiveTarget: String { didSet { defaults.set(archiveTarget, forKey: Keys.archive) } }
    /// Background archiving on/off (it also needs a target).
    public var archiveEnabled: Bool { didSet { defaults.set(archiveEnabled, forKey: Keys.archiveEnabled) } }
    /// Periodic archive check, minutes (rsync runs only when changes are pending).
    public var archiveIntervalMinutes: Int { didSet { defaults.set(archiveIntervalMinutes, forKey: Keys.archiveInterval) } }
    /// Rejected photos (and the session Trash) go to the archive too. On by default:
    /// the archive is the backup (final-journeys2 N2).
    public var archiveRejected: Bool { didSet { defaults.set(archiveRejected, forKey: Keys.archiveRejected) } }
    /// N3: identity of the volume the user confirmed the target on (`nil`: not
    /// recorded; an existing folder still works). A different volume mounted at
    /// the same path is then "offline", never written to.
    public var archiveTargetVolume: String? { didSet { defaults.set(archiveTargetVolume, forKey: Keys.archiveTargetVolume) } }
    /// Settings ▸ Archive: rsync into a folder, or S3-compatible object storage.
    public var archiveDestination: ArchiveDestinationKind {
        didSet { defaults.set(archiveDestination.rawValue, forKey: Keys.archiveDestination) }
    }
    /// Object storage settings. The keys are not here: they live only in the Keychain
    /// (`ArchiveObjectKeys`).
    public var archiveObjectStore: ObjectStoreSettings {
        didSet {
            let o = archiveObjectStore
            defaults.set(o.endpoint, forKey: Keys.objectEndpoint)
            defaults.set(o.region, forKey: Keys.objectRegion)
            defaults.set(o.bucket, forKey: Keys.objectBucket)
            defaults.set(o.prefix, forKey: Keys.objectPrefix)
            defaults.set(o.pathStyle, forKey: Keys.objectPathStyle)
        }
    }
    /// Rejected photos are handed to Amazon Photos too (off by default).
    public var amazonRejected: Bool { didSet { defaults.set(amazonRejected, forKey: Keys.amazonRejected) } }
    /// Folder watched by the Amazon Photos app (PLAN D3 option A); `nil` = off.
    /// Set it with `chooseAmazonFolder(_:)`, which keeps a bookmark (A-M2).
    public var amazonPhotosFolder: URL? { didSet { defaults.set(amazonPhotosFolder?.path, forKey: Keys.amazon) } }
    /// Bookmark of `amazonPhotosFolder`, so a folder moved or renamed in
    /// Finder is followed instead of being recreated at the old path.
    public var amazonPhotosFolderBookmark: Data? {
        didSet { defaults.set(amazonPhotosFolderBookmark, forKey: Keys.amazonBookmark) }
    }
    public var appearance: AppearancePreference { didSet { defaults.set(appearance.rawValue, forKey: Keys.appearance) } }
    /// [library] Settings › General › Open in: the last session or the Library.
    public var startView: StartView { didSet { defaults.set(startView.rawValue, forKey: Keys.startView) } }
    public var autoOpenImport: Bool { didSet { defaults.set(autoOpenImport, forKey: Keys.autoImport) } }
    /// How Copy picks its groups: the source's edited ones (Capture One's auto-select,
    /// the default) or the user's own ticks in `copyGroups`.
    public var copyMode: CopyMode { didSet { defaults.set(copyMode.rawValue, forKey: Keys.copyMode) } }
    /// The Custom ticks (Capture One's adjustment clipboard): what Copy takes and Apply
    /// pastes in `.custom` mode.
    public var copyGroups: Set<AdjustGroup> {
        didSet {
            defaults.set(copyGroups.map(\.rawValue).sorted(), forKey: Keys.copyGroups)
            defaults.removeObject(forKey: Keys.legacyCopyGroups)
        }
    }
    /// Groups Apply pastes relatively (the change, not the value).
    public var copyRelativeGroups: Set<AdjustGroup> {
        didSet { defaults.set(copyRelativeGroups.map(\.rawValue).sorted(), forKey: Keys.relative) }
    }
    /// A pasted Crop & Straighten keeps each photo's own straighten (on by default).
    public var copyKeepsStraighten: Bool { didSet { defaults.set(copyKeepsStraighten, forKey: Keys.keepStraighten) } }
    public var sessionNameTemplate: String { didSet { defaults.set(sessionNameTemplate, forKey: Keys.nameTemplate) } }
    public var renameTemplate: String { didSet { defaults.set(renameTemplate, forKey: Keys.renameTemplate) } }
    /// User-customizable single-key shortcuts (UX9), on top of `keyPreset`'s own keys.
    public var keyBindings: KeyBindings { didSet { defaults.set(keyBindings.dictionary, forKey: Keys.bindings) } }
    /// The keyboard set (Luxtable, Lightroom Classic, Capture One): menu shortcuts and
    /// the keys `keyBindings` starts from. Change it with `useKeyPreset(_:)`.
    public private(set) var keyPreset: KeyPreset { didSet { defaults.set(keyPreset.rawValue, forKey: Keys.keyPreset) } }

    /// Where the tool panels and Copy / Apply sit (Settings › Familiar layout).
    public var layoutPreset: LayoutPreset { didSet { defaults.set(layoutPreset.rawValue, forKey: Keys.layoutPreset) } }
    /// The names chosen on their own; `nil` follows `layoutPreset`.
    public var namePreset: NameSet? { didSet { defaults.set(namePreset?.rawValue, forKey: Keys.namePreset) } }
    /// The names in use.
    public var names: NameSet { namePreset ?? layoutPreset.names }
    /// The tool panels' side chosen on its own (View ▸ Tools on Left); `nil` follows `layoutPreset`.
    public var toolSidePreset: LayoutPreset.Side? { didSet { defaults.set(toolSidePreset?.rawValue, forKey: Keys.toolSide) } }
    /// The side the tool panels sit on.
    public var toolSide: LayoutPreset.Side { toolSidePreset ?? layoutPreset.toolSide }

    /// Put the tool panels on `side`; the layout's own side goes back to following the layout.
    public func putTools(on side: LayoutPreset.Side) {
        toolSidePreset = side == layoutPreset.toolSide ? nil : side
    }

    /// Switch keyboard sets: the set's own keys replace the current ones (the user's
    /// changes to the previous set don't carry over, so nothing clashes).
    public func useKeyPreset(_ preset: KeyPreset) {
        keyPreset = preset
        keyBindings = preset.bindings
    }
    /// Move to the next photo after rating/flagging (on by default, like C1's auto-advance).
    public var autoAdvance: Bool { didSet { defaults.set(autoAdvance, forKey: Keys.autoAdvance) } }
    /// [map] Map mode may load tiles from Apple Maps (`nil`: not asked yet).
    public var mapTiles: Bool? {
        didSet {
            if let mapTiles { defaults.set(mapTiles, forKey: Keys.mapTiles) } else { defaults.removeObject(forKey: Keys.mapTiles) }
        }
    }
    /// Voice commands listen only while a key is held (off: continuous while on).
    public var voicePushToTalk: Bool { didSet { defaults.set(voicePushToTalk, forKey: Keys.voicePushToTalk) } }
    /// The key held for push-to-talk.
    public var voicePushToTalkKey: PushToTalkKey {
        didSet { defaults.set(voicePushToTalkKey.rawValue, forKey: Keys.voicePushToTalkKey) }
    }
    /// The user's own voice phrases: normalized phrase → command id (`VoicePhraseBook`).
    public var voicePhrases: [String: String] { didSet { defaults.set(voicePhrases, forKey: Keys.voicePhrases) } }
    /// How many pixels the eyedroppers average (white balance, colour editor, skin tone).
    public var pickerSample: PickerSample { didSet { defaults.set(pickerSample.rawValue, forKey: Keys.pickerSample) } }
    /// "Move Picked and Rated to Selects" takes picked photos and those rated at
    /// least this many stars (1–5; review N5: 1★ means "meh" to this user).
    public var selectsMinRating: Int {
        didSet {
            if !(1...5).contains(selectsMinRating) { selectsMinRating = min(max(selectsMinRating, 1), 5) }
            defaults.set(selectsMinRating, forKey: Keys.selectsMinRating)
        }
    }
    /// Written into exported files' EXIF Artist / Copyright ("" = not written).
    public var artist: String { didSet { defaults.set(artist, forKey: Keys.artist) } }
    public var copyright: String { didSet { defaults.set(copyright, forKey: Keys.copyright) } }
    /// [multiview] With several photos selected the viewer shows them all (on by default).
    public var showSelectionInViewer: Bool { didSet { defaults.set(showSelectionInViewer, forKey: Keys.showSelection) } }
    /// Bursts show as collapsed stacks in the filmstrip and grid (the filmstrip's "Stack bursts"; on by default).
    public var stackBursts: Bool { didSet { defaults.set(stackBursts, forKey: Keys.stackBursts) } }

    /// [multiview] The filmstrip's position and size in the window `window` (one row at the bottom until changed).
    public func filmstripLayout(window: String) -> FilmstripLayout { filmstripLayouts[window] ?? FilmstripLayout() }

    public func setFilmstripLayout(_ layout: FilmstripLayout, window: String) {
        var all = filmstripLayouts
        all[window] = layout
        defaults.set(try? JSONEncoder().encode(all), forKey: Keys.filmstripLayout)
    }

    private var filmstripLayouts: [String: FilmstripLayout] {
        defaults.data(forKey: Keys.filmstripLayout).flatMap { try? JSONDecoder().decode([String: FilmstripLayout].self, from: $0) } ?? [:]
    }

    /// The volume `folder` (a session's) was found on, while it was there (nil: never).
    public func volumeRecord(for folder: URL) -> VolumeRecord? { volumeRecords[folder.standardizedFileURL.path] }

    public func setVolumeRecord(_ record: VolumeRecord, for folder: URL) {
        var all = volumeRecords
        all[folder.standardizedFileURL.path] = record
        defaults.set(try? JSONEncoder().encode(all), forKey: Keys.volumeRecords)
    }

    public func removeVolumeRecord(for folder: URL) {
        var all = volumeRecords
        guard all.removeValue(forKey: folder.standardizedFileURL.path) != nil else { return }
        defaults.set(try? JSONEncoder().encode(all), forKey: Keys.volumeRecords)
    }

    private var volumeRecords: [String: VolumeRecord] {
        defaults.data(forKey: Keys.volumeRecords).flatMap { try? JSONDecoder().decode([String: VolumeRecord].self, from: $0) } ?? [:]
    }

    enum Keys {
        static let volumeRecords = "sessionVolumeRecords"
        static let onboarding = "onboardingComplete"
        static let sessions = "sessionsFolder"
        static let archive = "archiveTarget"
        static let amazon = "amazonPhotosFolder"
        static let amazonBookmark = "amazonPhotosFolderBookmark"
        static let archiveEnabled = "archiveEnabled"
        static let archiveInterval = "archiveIntervalMinutes"
        /// Before N2 this one key covered the archive and Amazon Photos.
        static let archiveRejected = "archiveRejectedPhotos"
        static let amazonRejected = "amazonRejectedPhotos"
        static let archiveTargetVolume = "archiveTargetVolume"
        static let archiveDestination = "archiveDestination"
        static let objectEndpoint = "archiveObjectEndpoint"
        static let objectRegion = "archiveObjectRegion"
        static let objectBucket = "archiveObjectBucket"
        static let objectPrefix = "archiveObjectPrefix"
        static let objectPathStyle = "archiveObjectPathStyle"
        static let appearance = "appearance"
        static let autoImport = "autoOpenImport"
        static let startView = "startView"
        // N6: v2 selections hold crop/spots/layers only when the user ticked them.
        static let copyGroups = "rememberedCopyGroups.v2"
        static let legacyCopyGroups = "rememberedCopyGroups"
        static let relative = "rememberedRelativeGroups"
        static let copyMode = "copyMode"
        static let keepStraighten = "copyKeepsStraighten"
        // The removed Copy/Apply sheet's last options (read once, to migrate them).
        static let lastPaste = "lastPasteOptions.v2"
        static let legacyLastPaste = "lastPasteOptions"
        static let nameTemplate = "sessionNameTemplate"
        static let renameTemplate = "renameTemplate"
        static let autoAdvance = "autoAdvance"
        static let pickerSample = "pickerSample"
        static let voicePushToTalk = "voicePushToTalk"
        static let mapTiles = "mapTiles"
        static let voicePushToTalkKey = "voicePushToTalkKey"
        static let voicePhrases = "voiceCustomPhrases"
        static let selectsMinRating = "selectsMinRating"
        static let bindings = "keyBindings"
        static let keyPreset = "keyPreset"
        static let layoutPreset = "layoutPreset"
        static let namePreset = "namePreset"
        static let toolSide = "toolSide"
        static let artist = "exportArtist"
        static let copyright = "exportCopyright"
        static let showSelection = "showSelectionInViewer"
        static let filmstripLayout = "filmstripLayout"
        static let stackBursts = "stackBursts"
    }

    public init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        onboardingComplete = defaults.bool(forKey: Keys.onboarding)
        let pictures = FileManager.default.urls(for: .picturesDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Pictures")
        sessionsFolder = defaults.string(forKey: Keys.sessions).map { URL(fileURLWithPath: $0) }
            ?? pictures.appendingPathComponent("Luxtable")
        // L5: no default target; onboarding suggests /Volumes/NAS/Photos only if that share exists.
        archiveTarget = defaults.string(forKey: Keys.archive) ?? ""
        archiveEnabled = defaults.object(forKey: Keys.archiveEnabled) as? Bool ?? true
        archiveIntervalMinutes = max(1, defaults.object(forKey: Keys.archiveInterval) as? Int ?? 10)
        // N2: never set → on. Only a value the user set keeps the archive's
        // rejects out; that same choice carries over to Amazon Photos.
        let legacyRejected = defaults.object(forKey: Keys.archiveRejected) as? Bool
        archiveRejected = legacyRejected ?? true
        amazonRejected = defaults.object(forKey: Keys.amazonRejected) as? Bool ?? legacyRejected ?? false
        archiveTargetVolume = defaults.string(forKey: Keys.archiveTargetVolume)
        archiveDestination = defaults.string(forKey: Keys.archiveDestination).flatMap(ArchiveDestinationKind.init) ?? .folder
        archiveObjectStore = ObjectStoreSettings(endpoint: defaults.string(forKey: Keys.objectEndpoint) ?? "",
                                                 region: defaults.string(forKey: Keys.objectRegion) ?? "",
                                                 bucket: defaults.string(forKey: Keys.objectBucket) ?? "",
                                                 prefix: defaults.string(forKey: Keys.objectPrefix) ?? "luxtable/",
                                                 pathStyle: defaults.bool(forKey: Keys.objectPathStyle))
        amazonPhotosFolder = defaults.string(forKey: Keys.amazon).map { URL(fileURLWithPath: $0) }
        amazonPhotosFolderBookmark = defaults.data(forKey: Keys.amazonBookmark)
        appearance = defaults.string(forKey: Keys.appearance).flatMap(AppearancePreference.init) ?? .dark
        autoOpenImport = defaults.object(forKey: Keys.autoImport) as? Bool ?? true
        startView = defaults.string(forKey: Keys.startView).flatMap(StartView.init(rawValue:)) ?? .session
        // N6: older selections may hold crop/spots/layers that were preselected, not ticked: drop those.
        func groups(_ names: [String]?, legacy: Bool) -> Set<AdjustGroup> {
            Set((names ?? []).compactMap(AdjustGroup.init(rawValue:)).filter { !legacy || !$0.isOptIn })
        }
        let remembered = defaults.stringArray(forKey: Keys.copyGroups).map { groups($0, legacy: false) }
            ?? defaults.stringArray(forKey: Keys.legacyCopyGroups).map { groups($0, legacy: true) }
        copyGroups = remembered ?? AdjustGroup.copiedByDefault
        // A remembered sheet selection was the user's own ticks; without one the sheet
        // pre-ticked the edited groups, which `.edited` keeps doing.
        copyMode = defaults.string(forKey: Keys.copyMode).flatMap(CopyMode.init(rawValue:)) ?? (remembered == nil ? .edited : .custom)
        // Without "Remember", the sheet kept relative choices only in its last options.
        let lastPaste = defaults.dictionary(forKey: Keys.lastPaste) ?? defaults.dictionary(forKey: Keys.legacyLastPaste)
        let lastRelative = remembered == nil ? lastPaste.map { groups($0["relative"] as? [String], legacy: false) } : nil
        copyRelativeGroups = lastRelative
            ?? Set((defaults.stringArray(forKey: Keys.relative) ?? []).compactMap(AdjustGroup.init(rawValue:)))
        copyKeepsStraighten = defaults.object(forKey: Keys.keepStraighten) as? Bool ?? true
        sessionNameTemplate = defaults.string(forKey: Keys.nameTemplate) ?? "{date} {text}"
        renameTemplate = defaults.string(forKey: Keys.renameTemplate) ?? "{original}"
        autoAdvance = defaults.object(forKey: Keys.autoAdvance) as? Bool ?? true
        voicePushToTalk = defaults.bool(forKey: Keys.voicePushToTalk)
        mapTiles = defaults.object(forKey: Keys.mapTiles) as? Bool
        voicePushToTalkKey = defaults.string(forKey: Keys.voicePushToTalkKey).flatMap(PushToTalkKey.init(rawValue:)) ?? .rightOption
        voicePhrases = defaults.dictionary(forKey: Keys.voicePhrases) as? [String: String] ?? [:]
        pickerSample = defaults.string(forKey: Keys.pickerSample).flatMap(PickerSample.init(rawValue:)) ?? .box5
        selectsMinRating = min(max(defaults.object(forKey: Keys.selectsMinRating) as? Int ?? 3, 1), 5)
        artist = defaults.string(forKey: Keys.artist) ?? ""
        copyright = defaults.string(forKey: Keys.copyright) ?? ""
        layoutPreset = defaults.string(forKey: Keys.layoutPreset).flatMap(LayoutPreset.init(rawValue:)) ?? .luxtable
        namePreset = defaults.string(forKey: Keys.namePreset).flatMap(NameSet.init(rawValue:))
        toolSidePreset = defaults.string(forKey: Keys.toolSide).flatMap(LayoutPreset.Side.init(rawValue:))
        showSelectionInViewer = defaults.object(forKey: Keys.showSelection) as? Bool ?? true
        stackBursts = defaults.object(forKey: Keys.stackBursts) as? Bool ?? true
        let preset = defaults.string(forKey: Keys.keyPreset).flatMap(KeyPreset.init(rawValue:)) ?? .luxtable
        keyPreset = preset
        keyBindings = (defaults.dictionary(forKey: Keys.bindings) as? [String: String])
            .map { KeyBindings(dictionary: $0, base: preset.bindings) } ?? preset.bindings
        // The migration's choice of mode is made once.
        if defaults.object(forKey: Keys.copyMode) == nil { defaults.set(copyMode.rawValue, forKey: Keys.copyMode) }
        if lastPaste != nil {
            defaults.set(copyRelativeGroups.map(\.rawValue).sorted(), forKey: Keys.relative)
            defaults.removeObject(forKey: Keys.lastPaste)
            defaults.removeObject(forKey: Keys.legacyLastPaste)
        }
    }
}

// MARK: - Amazon Photos folder (A-M2)

extension AppSettings {
    /// Choose (or with `nil`, turn off) the Amazon Photos folder, keeping a
    /// bookmark so a later move in Finder is followed.
    public func chooseAmazonFolder(_ url: URL?) {
        amazonPhotosFolder = url
        amazonPhotosFolderBookmark = url.flatMap { try? $0.bookmarkData(options: [], includingResourceValuesForKeys: nil, relativeTo: nil) }
    }

    /// Where a bookmarked folder is now, and a fresh bookmark when the old one
    /// is stale. `nil` when it can't be resolved, or when the folder went to
    /// the Trash (a deleted folder is missing, never followed). File-system
    /// work: call it off the main thread.
    nonisolated static func followBookmark(_ data: Data) -> (url: URL, fresh: Data?)? {
        var stale = false
        guard let url = try? URL(resolvingBookmarkData: data, options: [.withoutUI, .withoutMounting],
                                 relativeTo: nil, bookmarkDataIsStale: &stale),
              !url.pathComponents.contains(".Trash") else { return nil }
        let fresh = stale ? try? url.bookmarkData(options: [], includingResourceValuesForKeys: nil, relativeTo: nil) : nil
        return (url, fresh)
    }

    /// If the Amazon Photos folder was moved or renamed, follow it (bounded,
    /// off the main thread). Returns whether the folder changed.
    @discardableResult
    public func followMovedAmazonFolder(timeout: Double = 2) async -> Bool {
        guard let data = amazonPhotosFolderBookmark, let current = amazonPhotosFolder else { return false }
        guard let found = await blockingWithTimeout(timeout, { AppSettings.followBookmark(data) }) ?? nil else { return false }
        if let fresh = found.fresh { amazonPhotosFolderBookmark = fresh }
        guard found.url.standardizedFileURL.path != current.standardizedFileURL.path else { return false }
        amazonPhotosFolder = found.url
        amazonPhotosFolderBookmark = (try? found.url.bookmarkData(options: [], includingResourceValuesForKeys: nil, relativeTo: nil))
            ?? amazonPhotosFolderBookmark
        return true
    }
}

/// The archive settings the background service needs, read on the main actor.
public struct ArchiveSettingsSnapshot: Sendable, Equatable {
    public var enabled: Bool
    public var target: String
    public var intervalMinutes: Int
    public var amazonFolder: URL?
    /// N2: rejected photos are archived (on by default: the archive is the backup)…
    public var archiveRejected: Bool
    /// …and handed to Amazon Photos (off by default).
    public var amazonRejected: Bool
    /// N3: the confirmed target's volume identity (see `AppSettings.archiveTargetVolume`).
    public var targetVolume: String?
    /// Object storage instead of the folder `target` (nil: the folder).
    public var objectStore: ObjectStoreSettings?

    public init(enabled: Bool, target: String, intervalMinutes: Int, amazonFolder: URL?, archiveRejected: Bool = true,
                amazonRejected: Bool = false, targetVolume: String? = nil, objectStore: ObjectStoreSettings? = nil) {
        self.objectStore = objectStore
        self.targetVolume = targetVolume
        self.enabled = enabled
        self.target = target
        self.intervalMinutes = intervalMinutes
        self.amazonFolder = amazonFolder
        self.archiveRejected = archiveRejected
        self.amazonRejected = amazonRejected
    }

    /// Archiving runs only when switched on and a target (a folder, or a bucket) is set.
    public var isActive: Bool { enabled && hasTarget }

    public var hasTarget: Bool {
        if let objectStore { return !objectStore.bucket.trimmingCharacters(in: .whitespaces).isEmpty }
        return !target.trimmingCharacters(in: .whitespaces).isEmpty
    }
}

extension AppSettings {
    public var archiveSnapshot: ArchiveSettingsSnapshot {
        ArchiveSettingsSnapshot(enabled: archiveEnabled, target: archiveTarget.trimmingCharacters(in: .whitespaces),
                                intervalMinutes: archiveIntervalMinutes, amazonFolder: amazonPhotosFolder,
                                archiveRejected: archiveRejected, amazonRejected: amazonRejected,
                                targetVolume: archiveTargetVolume,
                                objectStore: archiveDestination == .object ? archiveObjectStore : nil)
    }

    /// N3: the user confirmed the archive target (onboarding Finish, Settings):
    /// record the identity of the volume it is on (off the main thread, bounded).
    /// Only an explicit confirmation records it: a periodic check never adopts
    /// whatever volume is mounted there now.
    public func confirmArchiveTarget(volumeIdentity: @escaping ArchiveVolumeIdentity = ArchiveVolume.identity,
                                     timeout: Double = 2) async {
        let target = archiveTarget.trimmingCharacters(in: .whitespaces)
        guard !target.isEmpty else {
            archiveTargetVolume = nil
            return
        }
        let identity = await blockingWithTimeout(timeout) { volumeIdentity(target) } ?? nil
        guard target == archiveTarget.trimmingCharacters(in: .whitespaces) else { return } // changed meanwhile
        archiveTargetVolume = identity
    }
}

/// N3: the identity (volume UUID / device) of the volume holding a path, `nil`
/// when it can't be read. Blocking: call it off the main thread.
public typealias ArchiveVolumeIdentity = @Sendable (String) -> String?

public enum ArchiveVolume {
    /// lt-ffi `archive_volume_identity` (lt-archive `VolumeId`).
    public static let identity: ArchiveVolumeIdentity = { archiveVolumeIdentity(path: $0) }
}
