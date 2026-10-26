import CoreGraphics
import Foundation
import Observation

public enum ActiveSheet: Identifiable, Equatable, Sendable {
    case importCard(URL?)
    case onboarding
    /// Export the selected photos.
    case export
    /// File ▸ New Session…
    case newSession
    /// File ▸ Open…: recent sessions, then Choose Folder…
    case openSession
    /// Capture One session import (preview, then import).
    case captureOneImport(CaptureOneImportModel)
    /// File ▸ Import Capture One Settings… (recipes, workspaces, styles…).
    case captureOneSettings(C1SettingsImportModel)
    /// File ▸ Import Lightroom… (catalog, XMP sidecars or presets).
    case lightroomImport(LightroomImportModel)
    /// File ▸ Import Aperture or iPhoto Library…
    case appleLibraryImport(AppleLibraryImportModel)
    /// Photo ▸ Merge to HDR… / Merge to Panorama…
    case merge(MergeModel)

    public var id: String {
        switch self {
        case .importCard: "import"
        case .onboarding: "onboarding"
        case .export: "export"
        case .newSession: "newSession"
        case .openSession: "openSession"
        case .captureOneImport: "capture-one-import"
        case .captureOneSettings: "capture-one-settings"
        case .lightroomImport: "lightroom-import"
        case .appleLibraryImport: "apple-library-import"
        case .merge: "merge"
        }
    }
}

public struct Toast: Equatable, Identifiable, Sendable {
    public let id = UUID()
    public var message: String
    public var symbol: String
    public var isError = false
    /// An "Open" button that opens this session (an import that finished after Close).
    public var openSession: SessionInfo?

    public init(_ message: String, symbol: String = "checkmark.circle", isError: Bool = false,
                openSession: SessionInfo? = nil) {
        self.message = message
        self.symbol = symbol
        self.isError = isError
        self.openSession = openSession
    }
}

/// `live` uses real files and never pretends: anything not yet backed by Rust
/// is disabled with an explanation. `demo` (`--demo`) runs on mocks.
public enum AppMode: Sendable, Equatable {
    case live, demo
}

/// The app's single source of UI truth. Main-actor only; all service calls are
/// `async` and run off the main thread inside the services.
@MainActor @Observable
public final class AppModel {
    // MARK: Services
    @ObservationIgnored public let sessions: any SessionStore
    @ObservationIgnored public let images: any ImageProvider
    @ObservationIgnored public let renderer: any Renderer
    @ObservationIgnored public let importer: any ImportService
    @ObservationIgnored public let archive: any ArchiveService
    @ObservationIgnored public let ai: any AIService
    /// Tool runs whose results belong to the open session (matches, the WB picker):
    /// cancelled when another session opens. See `sessionBound`.
    @ObservationIgnored var sessionBoundTasks: [UUID: Task<Void, Never>] = [:]
    /// Horizon for Auto Straighten (Vision; tests inject a fake).
    @ObservationIgnored public var horizonDetector: any HorizonDetecting = VisionHorizonDetector()
    /// The ⌘K "Edit with words" bar.
    public let commandBar = CommandBarModel()
    /// Voice commands (on-device speech; `AppModel.live` wires the real engine).
    public let voice = VoiceController()
    /// lt-export (live) or unavailable (demo); set by `AppModel.make`.
    @ObservationIgnored public var exporter: any ExportService = UnavailableExportService()
    /// Google Photos export destination (live: lt-gphotos); nil in the demo.
    @ObservationIgnored public var googlePhotos: (any GooglePhotosService)?
    /// Immich: developed copies, sidecars and album sync (live: lt-immich, set by `AppModel.make`).
    public let immich: ImmichModel
    /// The optional integrations that are on (Settings ▸ Plugins in the app; every one
    /// in the demo and in tests unless they say otherwise).
    public let features = FeatureGates()
    /// The open Export sheet's model.
    public private(set) var exportModel: ExportModel?
    /// AI Optimize: measured, explained proposals (lt-pipeline); `nil` without files (demo).
    @ObservationIgnored public var optimizer: (any PhotoOptimizing)?
    /// Scene signals for AI Optimize (live: Vision, the sky model; tests inject them).
    @ObservationIgnored public var optimizeSignals: @MainActor (Photo) async -> OptimizeSignals = { _ in OptimizeSignals() }
    /// The scene analysis AI Looks read, per photo (one Vision pass a photo).
    @ObservationIgnored var lookSignals: [PhotoID: OptimizeSignals] = [:]
    /// AI masks for the analysis renders of edits with AI-mask layers.
    @ObservationIgnored public var optimizeMasks: DevelopRenderer.MaskProvider?
    /// The open AI Optimize panel.
    public internal(set) var optimizePanel: OptimizePanelModel?
    /// Each photo's last applied optimize report, as loaded or applied this run.
    public internal(set) var optimizeReports: [PhotoID: OptimizeReport] = [:]
    /// HDR and panorama merges (live: lt-ffi; demo: `MockPhotoMerger`); `nil`: unavailable.
    @ObservationIgnored public var merger: (any PhotoMerging)?
    /// The "Why these settings?" sheet shows this photo's report.
    public var whyReportPhoto: PhotoID?
    /// Rows of the tool sidebar an AI edit changed, per photo (`AIHighlights.swift`).
    public internal(set) var aiMarks: [PhotoID: AIMarks] = [:]
    /// Photos whose highlights this session set (an AI edit, a touch, Clear, or the
    /// stored report's): a report loaded later doesn't replace them.
    @ObservationIgnored var aiMarksSettled: Set<PhotoID> = []
    /// The AI Changes list is open (the focused photo's AI-changed settings).
    public var aiChangesShown = false
    /// Report writes to the sidecars, in order (`newVariant` waits for them).
    @ObservationIgnored var reportSaves: Task<Void, Never>?
    @ObservationIgnored public let thumbnails: ThumbnailCache
    /// Develop tools on the pipeline (live mode): auto WB/tone, WB picker, as-shot
    /// readout, lens profile, edited thumbnails. `nil` in demo mode.
    @ObservationIgnored public var tools: (any DevelopTools)? {
        didSet { thumbnails.tools = tools }
    }
    /// Per-photo develop facts (as-shot WB, lens profile, ISO), filled on demand.
    public internal(set) var developInfo: [PhotoID: PhotoDevelopInfo] = [:]
    /// Debounced, retrying writes of ratings/tags/picks/edits.
    public let saver: SaveCoordinator
    /// Embedded RAW previews (instant viewer + import thumbnails); `nil` in demo mode.
    @ObservationIgnored public let previews: (any PreviewProvider)?
    public let mode: AppMode
    public let settings: AppSettings

    // MARK: Session
    public var sessionList: [SessionInfo] = []
    public private(set) var currentSession: SessionInfo? {
        didSet {
            if currentSession != nil, oldValue == nil { settings.sessionClosed = false }
            if currentSession != oldValue { // only the latest change's serve may land
                serveTask?.cancel()
                serveTask = Task { [weak self] in await self?.serveCurrentSessionOnLink() }
            }
        }
    }
    @ObservationIgnored var serveTask: Task<Void, Never>?
    /// Whether the open session is a Capture One folder (see `refreshCaptureOne()`).
    public internal(set) var captureOne = CaptureOneStatus()
    /// File ▸ Revert Capture One Import is asking for confirmation.
    public var confirmingCaptureOneRevert = false
    /// Whether the open session has a Lightroom import to revert (see `refreshLightroom()`).
    public internal(set) var lightroom = LightroomStatus()
    public var confirmingLightroomRevert = false
    /// Whether the open session has an Aperture / iPhoto import to revert (see `refreshAppleLibrary()`).
    public internal(set) var appleLibrary = AppleLibraryStatus()
    public var confirmingAppleRevert = false
    /// Where imported Lightroom presets go (tests use their own).
    @ObservationIgnored var lightroomPresetStore: PresetStore = .shared
    /// Reads Capture One's settings folder (seam: lt-c1 live, a fake in tests).
    @ObservationIgnored public var c1SettingsReader: any C1SettingsReading = RustC1SettingsReader()
    public private(set) var albums: [Album] = []
    // Live counts per folder; see `selectSmartFolder(_:)`.
    public let smartFolders = SmartFolderController()
    /// Per-photo observable items: a rating keypress mutates one item, so only
    /// that cell re-renders and no 2,000-element array is copied or re-diffed.
    @ObservationIgnored private(set) var items: [PhotoID: PhotoItem] = [:]
    /// Session order (after `sort`).
    @ObservationIgnored var order: [PhotoID] = []
    public var filter = PhotoFilter() {
        didSet {
            // Folder and bar criteria don't stack. (@Observable re-runs
            // didSet on this assignment; the reconciled value is then stable.)
            let reconciled = filter.reconciled(from: oldValue)
            if reconciled != filter {
                filter = reconciled
                return
            }
            if filter != oldValue {
                if filter.place == nil, oldValue.place != nil { map.placeFilterCleared() }
                rebuildVisible()
                search.filterChanged() // rank again under the new filter
            }
        }
    }
    public var sort: PhotoSort = .captureTime {
        didSet { if sort != oldValue { resort() } }
    }
    /// Natural-language search: field state, model, indexing, results.
    @ObservationIgnored public let search = PhotoSearch()
    /// People: face indexing, clusters, names.
    @ObservationIgnored public let people = PeopleModel()
    /// Find (⌘F) across every session.
    @ObservationIgnored public let find = FindModel()
    /// The Map mode: markers, map filter, geotagging.
    @ObservationIgnored public let map = MapModel()
    /// The Mac ↔ iPad link (Settings ▸ Devices); serves the open session.
    public let remote: RemoteLinkModel
    /// [library] Every session's photos in one place.
    @ObservationIgnored public let library = LibraryModel()
    /// Results shown instead of the whole session, best first (with `filter`).
    public internal(set) var searchRanking: SearchRanking? {
        didSet { if searchRanking != oldValue { resort() } }
    }
    /// IDs passing `filter`, in display order. Views iterate this and look up `item(_:)`.
    public private(set) var visibleIDs: [PhotoID] = []
    /// Position of every photo in `order`; `visibleIDs` is ascending in it, so
    /// visible indices are a binary search (no per-change index rebuild).
    @ObservationIgnored var orderIndex: [PhotoID: Int] = [:]
    /// Bumped when any photo's metadata changes (for views that summarize the session).
    public internal(set) var photosVersion = 0
    public var selection: Set<PhotoID> = []
    public internal(set) var focusedID: PhotoID? {
        // The AI Optimize panel belongs to the photo it opened on: moving on (or to
        // another session) closes it, so Apply never lands on a photo not on screen.
        didSet {
            if focusedID != oldValue, optimizePanel != nil { endOptimize() }
            // The burst strip follows focus within its burst and closes when focus leaves it.
            let picker = browserState.picker
            if focusedID != oldValue, picker.isOpen {
                if let f = focusedID, picker.frames.contains(f) { picker.select(f) } else { picker.close() }
            }
        }
    }
    /// Selection anchor for shift-extend.
    @ObservationIgnored var anchorID: PhotoID?
    public internal(set) var edits: [PhotoID: EditState] = [:]
    /// How many depth maps have landed per photo: part of a depth lens blur thumbnail's
    /// identity, so one rendered before its map renders again when the map lands.
    public internal(set) var depthVersions: [PhotoID: Int] = [:]
    public private(set) var isLoading = false

    /// True while an arrow key auto-repeats: the viewer shows embedded previews
    /// only and renders once the key is released.
    public internal(set) var fastNavigation = false
    /// Another session is being opened: edits and ratings are ignored until it is shown, so
    /// none can be saved into the session that is loading (the store points at it early).
    public internal(set) var isSwitchingSession = false
    /// The name of the session being opened, for the "still opening" note.
    @ObservationIgnored var switchingTo: String?
    /// Returns when a switch has taken too long and is given up, so the shown session works
    /// again (a hung network volume). Tests pass a deadline they fire themselves.
    @ObservationIgnored var switchDeadline: @Sendable () async -> Void = { try? await Task.sleep(for: .seconds(30)) }

    // MARK: UI state
    public var browserMode: BrowserMode = .filmstrip {
        didSet { if oldValue == .library, browserMode != .library { openDeferredSession() } } // [library]
    }
    public var toolTab: ToolTab = .exposure
    /// Styles & Presets (about 20 rows) starts collapsed so the tools under it stay in view.
    public var collapsedPanels: Set<ToolPanelID> = ToolWorkspace.defaultCollapsed
    /// Viewer state and rendering (owns frames, zoom/pan, render scheduling).
    public let viewer: ViewerModel
    /// Soft proofing (View ▸ Soft Proof, ⇧S).
    public let softProof: SoftProofModel
    /// Columns currently shown by the grid (for ↑/↓), reported by the view.
    public var gridColumns = 6
    public var sheet: ActiveSheet?
    /// An unfinished import found at launch (journal in its session): the resume prompt.
    public var pendingImport: PendingImportInfo?
    /// The other unfinished imports, offered one after another.
    public private(set) var pendingImportQueue: [PendingImportInfo] = []
    /// Session the next import sheet targets (resuming an unfinished import).
    public var importResumeTarget: URL?
    public var showCheatSheet = false
    public var toast: Toast?
    /// The WB eyedropper is on: it stays on for repeated tries until Esc or Done.
    public var wbPickerActive = false { didSet { if wbPickerActive != oldValue { viewer.pickerLoupeChanged() } } }
    /// Lens blur: the next click on the photo sets the depth in focus (Esc or Done ends it).
    public var focusPickerActive = false
    /// Colour-editor tools on the image (range/skin picker, targeted adjustment).
    public let colorTools = ColorToolState()
    /// C1's "edit selected variants": slider changes apply to the whole selection.
    public var editAllSelected = false
    /// `--snapshot`: the viewer draws frames with SwiftUI instead of Metal so an
    /// in-process window capture includes them.
    public var snapshotMode = false
    @ObservationIgnored var cardCoordinator: CardCoordinator?
    /// Watch for memory cards after `start()` (tests turn this off).
    @ObservationIgnored public var watchCards = true
    public internal(set) var clipboard: AdjustmentClipboard?
    /// The clipboard's layers that Apply pastes (all of them until the Copy options untick some).
    public var clipboardLayerIDs: Set<Int> = []
    /// A paste waiting for as-shot values (relative white balance).
    public internal(set) var pendingPaste: Task<Int, Never>?
    /// Tool panels and filmstrip hidden (F / Tab), viewer only.
    public var panelsHidden = false
    /// B: only the filmstrip hidden (final review: a taller viewer at 1280×800).
    public var filmstripHidden = false
    /// [multiview] The filmstrip's position and rows/columns (dragging its separator), remembered per window.
    public var filmstrip = FilmstripLayout() {
        didSet { if filmstrip != oldValue { settings.setFilmstripLayout(filmstrip, window: AppWindows.main) } }
    }
    /// Thumbnails per row of a multi-line filmstrip (for ↑/↓), reported by the view.
    public var filmstripColumns = 1
    /// Width of the whole viewer area (the viewer itself is one tile of it in multi-view).
    public var viewerAreaWidth: CGFloat = 0
    public internal(set) var history = EditHistory()
    /// Each photo's History panel rows (this session, in memory).
    public internal(set) var photoHistories: [PhotoID: PhotoHistory] = [:]
    /// The row the viewer previews (hovered in the History panel).
    public internal(set) var historyPreview: HistoryNode?
    /// Never "up to date" before the archive service has said so.
    public private(set) var archiveStatus = ArchiveStatus(state: .unavailable("Checking the archive…"),
                                                          target: AppSettings.defaultArchiveTarget, lastSuccess: nil)
    /// The toolbar archive popover (also opened by `--snapshot --archive-popover`).
    public var archivePopoverShown = false
    /// (done, total) while AI Cull runs.
    public internal(set) var cullProgress: (done: Int, total: Int)?
    public internal(set) var cullSuggestions: [PhotoID: CullSuggestion] = [:]

    @ObservationIgnored var archiveTask: Task<Void, Never>?
    /// AI Cull in flight (closing the session cancels it).
    @ObservationIgnored var cullTask: Task<[CullSuggestion], Error>?
    /// AI Denoise (set in live mode): closing a session stops its queued work.
    @ObservationIgnored public weak var denoiser: DenoiseService?
    /// AI Upscale (set in live mode): Photo ▸ Enhance ▸ Upscale 2×.
    @ObservationIgnored public weak var upscaler: UpscaleService?
    /// Asks before Close Session while imports, an export or an archive run are going
    /// (the app's alert; tests answer it). `true`: close. Unset: close without asking.
    @ObservationIgnored public var confirmClose: (@MainActor (SessionCloseAlert) async -> Bool)?
    /// Close Session in flight (another close is ignored; an open waits for it).
    public internal(set) var closing: Task<Void, Never>?
    /// How long Close waits for the store to release the session (a hung volume).
    @ObservationIgnored var closeTailLimit: Duration = .seconds(10)
    /// Closed sessions the store is still releasing (past Close's bound): reopening one
    /// waits for its release, which would otherwise drop the reopened handle.
    @ObservationIgnored var releasing: [UUID: Task<Void, Never>] = [:]
    /// How long an open waits for a close in flight.
    @ObservationIgnored var openWaitLimit: Duration = .seconds(15)
    /// Bumped by each open: an open still waiting (or loading) when a newer one is asked
    /// for gives way to it.
    @ObservationIgnored var openToken = 0

    /// Whether each `/Volumes` volume the session's photos live on is mounted, as last
    /// probed (absent: not probed yet). Remove is offered only on a mounted one; a gone
    /// one is said ("Drive not connected").
    public internal(set) var volumeMounted: [String: Bool] = [:]
    /// Probes a volume root off the main thread (nil: no answer in time). Tests replace it.
    /// Mounted volumes that aren't the one recorded for the session (a clone, a restore,
    /// another disk under the same name): viewed as usual, but nothing on them is removable.
    public internal(set) var volumeMismatch: Set<String> = []
    @ObservationIgnored public var volumeProbe: @Sendable (URL, _ identity: String?) async -> VolumeCheck? = {
        await VolumeMount.probe($0, identity: $1)
    }
    /// Finds which volume a folder is on, off the main thread (nil: not there). Tests replace it.
    @ObservationIgnored public var volumeLocate: @Sendable (URL) async -> VolumeRecord? = { await VolumeMount.probeLocation($0) }
    /// Volumes whose first probe hung (no answer yet).
    @ObservationIgnored var volumeUnanswered: Set<String> = []
    @ObservationIgnored var mountObservers: [NSObjectProtocol] = []
    /// Photos whose store-missing flag was re-checked since a volume last came back.
    @ObservationIgnored var recheckedOriginals: Set<PhotoID> = []
    @ObservationIgnored var recheckingOriginals = false
    @ObservationIgnored var toastTask: Task<Void, Never>?
    /// Where the user was before an update restarted the app (`ResumeState`): `start()`
    /// reopens that session and `open(_:)` focuses that photo, once.
    @ObservationIgnored public var pendingResume: ResumeState?
    /// [library] The session a Library start left closed: it opens when the user leaves the Library.
    @ObservationIgnored public internal(set) var deferredSession: SessionInfo?

    public init(sessions: any SessionStore, images: any ImageProvider, renderer: any Renderer,
                importer: any ImportService, archive: any ArchiveService, ai: any AIService, settings: AppSettings,
                previews: (any PreviewProvider)? = nil, mode: AppMode = .demo) {
        self.previews = previews
        self.mode = mode
        self.sessions = sessions
        self.images = images
        self.renderer = renderer
        self.importer = importer
        self.archive = archive
        self.ai = ai
        self.settings = settings
        filmstrip = settings.filmstripLayout(window: AppWindows.main)
        remote = RemoteLinkModel(defaults: settings.userDefaults)
        immich = ImmichModel(settings: ImmichSettings(defaults: settings.userDefaults))
        thumbnails = ThumbnailCache(provider: images)
        saver = SaveCoordinator(store: sessions)
        viewer = ViewerModel(renderer: renderer, previews: previews)
        softProof = SoftProofModel(store: SoftProofStore(defaults: settings.userDefaults),
                                   catalog: ColorSyncProfileCatalog(), builder: RustProofTableBuilder())
        archiveStatus.target = settings.archiveTarget
        if !settings.onboardingComplete { sheet = .onboarding }
        viewer.app = self
        softProof.onDisplayChange = { [weak self] in self?.viewer.proofChanged() }
        saver.onDrop = { [weak self] m in self?.show(Toast(m, symbol: "exclamationmark.triangle", isError: true)) }
        search.attach(self)
        people.attach(self)
        find.attach(self)
        map.attach(self)
        remote.onSessionsChanged = { [weak self] roots in Task { await self?.linkChanged(roots) } }
        library.attach(self) // [library]
        thumbnails.editedSource = { [weak self] id in self?.edits[id] } // developed thumbnails of edited photos
        thumbnails.depthVersion = { [weak self] id in self?.depthVersions[id] ?? 0 }
        smartFolders.photosProvider = { [weak self] in self?.photos ?? [] }
        smartFolders.onFoldersChanged = { [weak self] in self?.refreshSmartFolderScope() }
        smartFolders.flushBeforeReload = { [weak self] in await self?.saver.flush() }
        colorTools.onModeChanged = { [weak self] in self?.viewer.pickerLoupeChanged() }
        voice.perform = { [weak self] actions, chain in self?.performVoice(actions, chain: &chain) }
        voice.pushToTalk = settings.voicePushToTalk
        voice.pushToTalkKey = settings.voicePushToTalkKey
        voice.customPhrases = VoicePhraseBook.effective(user: settings.voicePhrases).actions
        features.onChange = { [weak self] feature, on in self?.featureChanged(feature, on) }
        // A plug-in turned on or off changes the phrases voice hears.
        PluginStore.shared.onContentChanged = { [weak self] in
            guard let self else { return }
            voice.customPhrases = VoicePhraseBook.effective(user: settings.voicePhrases).actions
        }
        // No plug-in file grant may reach a session (open, or in the list).
        PluginStore.shared.protectedRoots = { [weak self] in
            (self?.sessionList.map(\.url) ?? []) + [self?.currentSession?.url].compactMap { $0 }
        }
    }

    // MARK: - Loading

    /// Load the session list and open the most recent session.
    public func start() async {
        if let movable = sessions as? any SessionMoveReporting {
            await movable.onSessionMoved { [weak self] move in
                Task { @MainActor in await self?.sessionMoved(move) }
            }
        }
        // Imported profiles register off the main thread before the first render.
        _ = await CameraProfileLibrary.loading?.value
        // [library] Starting in the Library opens no session (its photos aren't needed there).
        let inLibrary = startsInLibrary()
        if inLibrary { browserMode = .library }
        do {
            sessionList = try await sessions.listSessions()
            // Never reopen a folder that's gone or holds no photos: the welcome state lists
            // the recents instead (greyed out with why), and never an older session in
            // its place.
            // Nor one the user closed: the next launch opens none.
            if currentSession == nil, !settings.sessionClosed, let last = sessionToReopen() {
                if let why = last.unavailable {
                    AppLog.session.notice("not reopening \(last.url.path, privacy: .private(mask: .hash)): \(why.label, privacy: .public)")
                } else if inLibrary {
                    deferredSession = last
                } else {
                    await open(last)
                }
            }
        } catch {
            show(error)
        }
        startArchiveUpdates()
        if mode == .live { remote.resume() } // only if the user turned it on
        // Temps of exports interrupted by a quit or crash.
        let prefs = settings.userDefaults
        Task { await ExportPartials.sweepRemembered(defaults: prefs) }
        startCardWatcher()
        startMountWatcher()
        await checkPendingImports()
    }

    /// Offer to resume imports that didn't finish (lt-import journals), in
    /// the library and in every known session, one after another.
    public func checkPendingImports() async {
        guard let resuming = importer as? any ImportResuming else { return }
        let all = await resuming.pendingImports(including: sessionList.map(\.url))
        pendingImport = all.first
        pendingImportQueue = Array(all.dropFirst())
    }

    /// The resume prompt was answered: offer the next unfinished import.
    public func nextPendingImport() {
        pendingImport = pendingImportQueue.isEmpty ? nil : pendingImportQueue.removeFirst()
    }

    /// Resume: open the session and the import sheet aimed at it (the card's
    /// journal makes the copy continue where it stopped).
    public func resumePendingImport(_ p: PendingImportInfo) async {
        pendingImport = nil
        pendingImportQueue = []
        if let opener = sessions as? any FolderSessionOpening, let s = try? await opener.add(folder: p.sessionURL) {
            await reloadSessions(select: s)
        }
        importResumeTarget = p.sessionURL
        sheet = .importCard(nil)
    }

    /// Discard the unfinished import (its journal and `.partial` files only).
    public func discardPendingImport(_ p: PendingImportInfo) async {
        nextPendingImport()
        guard let resuming = importer as? any ImportResuming else { return }
        do {
            try await resuming.discard(p)
            show(Toast("Discarded the unfinished import into “\(p.sessionName)”", symbol: "trash"))
        } catch {
            show(error)
        }
    }

    /// Auto-open the import sheet when a card with DCIM/ mounts (PLAN §5 step 1).
    /// The coordinator only acts in `.live` mode and never on mocks.
    func startCardWatcher() {
        guard watchCards, cardCoordinator == nil else { return }
        cardCoordinator = makeCardCoordinator()
        cardCoordinator?.start()
    }

    /// `false` when ratings and edits are kept in memory only (shown in the status bar).
    public var persistsChanges: Bool { sessions.persistsChanges }

    /// Progress of a folder open in flight (drives the cancellable progress sheet).
    public private(set) var openProgress: OpenProgress?
    public private(set) var openingFolder: URL?
    @ObservationIgnored var openTask: Task<Void, Never>?

    /// Open a folder of RAW files as a session in place (live mode). Shows progress
    /// and can be cancelled with `cancelOpen()`.
    public func openFolder(_ url: URL) async {
        guard let store = sessions as? any FolderSessionOpening else {
            show(Toast("Opening folders works in live mode (run without --demo)", symbol: "folder"))
            return
        }
        openTask?.cancel()
        folderOpenToken += 1
        let folderToken = folderOpenToken
        isLoading = true
        openingFolder = url
        openProgress = OpenProgress(phase: .scanning, done: 0, total: 0)
        let report: @Sendable (OpenProgress) -> Void = { [weak self] p in
            Task { @MainActor in self?.receive(p, for: url) }
        }
        let task = Task { [weak self] in
            do {
                let s = try await store.add(folder: url, progress: report)
                try Task.checkCancellation()
                guard let self else { return }
                self.finishOpening(folderToken)
                await self.reloadSessions(select: s)
                let failures = await store.lastFailures
                if !failures.isEmpty { // name them
                    self.show(Toast(OpenFailures.message(failures), symbol: "exclamationmark.triangle", isError: true))
                }
                Task { await self.offerCaptureOneImport(after: url) }
            } catch is CancellationError {
                // A newer open took over (it shows its own progress): say nothing of this one.
                guard let self, self.finishOpening(folderToken) else { return }
                self.show(Toast("Opening \(url.lastPathComponent) was cancelled", symbol: "xmark.circle"))
            } catch {
                self?.finishOpening(folderToken)
                self?.show(error)
            }
        }
        openTask = task
        await task.value
    }

    /// Whether `token` is the latest folder open (an older one finishing must not clear the
    /// newer one's progress).
    @ObservationIgnored var folderOpenToken = 0

    /// End the folder open `token`; false (and nothing changed) when a newer one replaced it.
    @discardableResult
    func finishOpening(_ token: Int) -> Bool {
        guard token == folderOpenToken else { return false }
        isLoading = false
        // Background hashing keeps reporting after the photos are shown.
        if openProgress?.phase != .hashing { openProgress = nil }
        openingFolder = nil
        openTask = nil
        return true
    }

    /// Route open progress: blocking phases while the open runs; background hashing
    /// afterwards while that folder is the current session (cleared when done).
    func receive(_ p: OpenProgress, for url: URL) {
        let current = currentSession.map { $0.url.standardizedFileURL.resolvingSymlinksInPath() }
        let isCurrent = current == url.standardizedFileURL.resolvingSymlinksInPath()
        if p.phase == .updated { // background pass changed the photo list
            if isCurrent, openingFolder == nil { Task { await self.refreshCurrentSession() } }
            return
        }
        // A closed session's pass (none current) no longer shows.
        guard openingFolder == url || (p.phase == .hashing && currentSession != nil && (isCurrent || openingFolder == nil))
        else { return }
        // Nothing (left) to verify clears it, including an empty pass ("0 of 0").
        openProgress = (p.phase == .hashing && p.done >= p.total) ? nil : p
    }

    /// Cancel a folder open in flight.
    public func cancelOpen() { openTask?.cancel() }

    /// Forget missing originals (index/sidecars only; never touches files).
    /// Only photos `canRemoveMissing` allows: never one whose drive merely dropped off.
    public func removeMissing(_ ids: [PhotoID]) async {
        // Fail closed: probe the drives again first (no earlier answer counts), and forget
        // only photos on a drive that answered "mounted" now. One that dropped off, or
        // hangs, keeps its photos.
        ProbeGate.shared.invalidate()
        let up = await refreshVolumes()
        let ids = ids.filter { id in canRemoveMissing(id) && (volumeRoot(of: id).map { up.contains($0.path) } ?? true) }
        guard let store = sessions as? any FolderSessionOpening, !ids.isEmpty else { return }
        await saver.flush() // their pending changes are written first, not left for photos it forgets
        do {
            try await store.removeMissing(ids)
            if let s = currentSession { await reloadSessions(select: s) }
        } catch {
            show(error)
        }
    }

    /// Point a missing photo at its moved file.
    public func locate(_ id: PhotoID, at url: URL) async {
        guard let store = sessions as? any FolderSessionOpening else { return }
        do {
            try await store.locate(id, at: url)
            if let s = currentSession { await reloadSessions(select: s) }
        } catch {
            show(error)
        }
    }

    /// List the current session again (the background identity pass relinked,
    /// replaced or added photos), keeping the focused photo when it still exists.
    /// `keepingHistory`: after file moves, the undo history carries over
    /// instead of starting fresh as opening a session does.
    func refreshCurrentSession(keepingHistory: Bool = false) async {
        guard let s = currentSession else { return }
        let focused = focusedID
        let kept = history
        let keptRows = photoHistories
        await reloadSessions(select: s)
        if keepingHistory, currentSession?.id == s.id {
            history = kept
            history.rebase(onto: items.mapValues(\.photo))
            photoHistories = keptRows.filter { items[$0.key] != nil }
        }
        if let focused, items[focused] != nil { focus(focused, extend: false) }
    }

    /// A session folder was renamed or moved in Finder while open and
    /// the store followed it: say so, and list it again at its new place (photo URLs
    /// changed), keeping the focused photo.
    func sessionMoved(_ move: SessionMove) async {
        show(Toast("“\(move.from.lastPathComponent)” was renamed to “\(move.to.name)” — following it",
                   symbol: "folder.badge.gearshape"))
        let wasCurrent = currentSession.map { RecentFolders.canonical($0.url) == RecentFolders.canonical(move.from) } ?? false
        guard wasCurrent else {
            if let list = try? await sessions.listSessions() { sessionList = list }
            return
        }
        let focused = focusedID
        await reloadSessions(select: move.to)
        if let focused, items[focused] != nil { focus(focused, extend: false) }
        // The photo URLs changed: develop the focused photo again from its new place
        // (the old render may have failed on the old path).
        viewer.focusChanged()
    }

    /// Every original of the open session is missing (a removed card, an unmounted drive,
    /// a deleted folder): one session-level message instead of one per photo.
    public private(set) var sessionOriginalsMissing = false

    /// `id`'s original is missing: per the store, or found so since it was listed.
    public func isOriginalMissing(_ id: PhotoID) -> Bool {
        items[id]?.isOriginalMissing == true || isVolumeGone(id)
    }

    /// `id`'s original was found missing (a render failed on it, or it isn't there when
    /// focused) or back again (`missing == false`; a drive plugged in again).
    @discardableResult
    func noteOriginal(_ id: PhotoID, missing: Bool) -> Bool {
        guard let item = items[id], item.originalMissing != missing else { return false }
        item.originalMissing = missing
        if missing {
            AppLog.render.error("original missing: \(item.photo.url?.path ?? "?", privacy: .private(mask: .hash))")
        } else {
            AppLog.render.notice("original back: \(item.photo.url?.path ?? "?", privacy: .private(mask: .hash))")
        }
        updateSessionMissing()
        return true
    }

    /// Escalates to the session banner when every original is gone, or the volume the
    /// session folder is on is (a NAS dropped off, a card pulled mid-session).
    func updateSessionMissing() {
        let stills = items.values.lazy.filter { !$0.photo.isVideo }
        sessionOriginalsMissing = !stills.isEmpty && (sessionVolumeGone || stills.allSatisfy(\.isOriginalMissing))
    }

    /// "Locate Folder…": `session`'s photos are now in `folder` (a moved folder, a drive
    /// mounted elsewhere). Its ratings and edits come along; it opens there.
    public func locateSession(_ session: SessionInfo, at folder: URL) async {
        guard let store = sessions as? any FolderSessionOpening else { return }
        do {
            let moved = try await store.relocate(session, to: folder)
            forgetVolumeRecords(session.url, folder, moved.url) // located again (also at the same path)
            await reloadSessions(select: moved)
            let missing = missingCount
            show(Toast(missing == 0 ? "“\(session.name)” found at \(folder.path)"
                                    : "“\(session.name)” linked to \(folder.path); \(missing) photos still missing",
                       symbol: "folder.badge.questionmark", isError: missing > 0))
        } catch {
            AppLog.session.error("locating \(session.url.path, privacy: .private(mask: .hash)) at \(folder.path, privacy: .private(mask: .hash)) failed: \(error.localizedDescription, privacy: .private(mask: .hash))")
            show(error)
        }
    }

    /// "Remove from Recents" for the open session whose originals are gone: closes it and
    /// forgets the entry (its folder and session data are never touched).
    public func forgetCurrentSession() async {
        guard let s = currentSession, let store = sessions as? any RecentSessionsEditing else { return }
        await saver.flush()
        await store.removeFromRecents(s)
        AppLog.session.notice("removed \(s.url.path, privacy: .private(mask: .hash)) from recents")
        currentSession = nil
        search.sessionClosed() // stop indexing it, drop its results
        people.sessionClosed() // stop scanning faces
        load([])
        focus(nil, extend: false)
        await search.sessionForgotten(s) // not offered in All Sessions results either
        await reloadSessions()
    }

    /// The develop of `id` failed because its RAW is damaged: badge it
    /// (the embedded preview may still show a normal-looking photo).
    func markDamaged(_ id: PhotoID) {
        guard let item = items[id], !item.photo.isDamaged else { return }
        item.photo.isDamaged = true
    }

    /// "File changed" photos: put back the metadata of the photos their files replaced.
    public func restorePrevious(_ ids: [PhotoID]) async {
        await perFolderAction(ids) { store, id in try await store.restorePrevious(id) }
    }

    /// "File changed" photos: keep them as the new photos they are.
    public func keepAsNew(_ ids: [PhotoID]) async {
        await perFolderAction(ids) { store, id in try await store.dismissFileChanged(id) }
    }

    /// Missing photos whose only copy is inside the session store: move it back.
    public func restoreToFolder(_ ids: [PhotoID]) async {
        await perFolderAction(ids) { store, id in try await store.restoreToFolder(id) }
    }

    func perFolderAction(_ ids: [PhotoID],
                                 _ body: (any FolderSessionOpening, PhotoID) async throws -> Void) async {
        guard let store = sessions as? any FolderSessionOpening, !ids.isEmpty else { return }
        await saver.flush()
        do {
            for id in ids { try await body(store, id) }
        } catch {
            show(error)
        }
        await refreshCurrentSession()
    }

    /// Background hashing pauses while the user navigates or rates.
    func noteUserActivity() {
        (sessions as? any UserActivityObserving)?.noteUserActivity()
    }

    /// Missing originals in the current session (for the "Missing" filter badge).
    /// Counted once per `photosVersion` (the filter bar reads it several times per body).
    public var missingCount: Int {
        if missingCountCache.version == photosVersion { return missingCountCache.count }
        let count = order.reduce(0) { $0 + ((items[$1]?.photo.isMissing ?? false) ? 1 : 0) }
        missingCountCache = (photosVersion, count)
        return count
    }
    @ObservationIgnored var missingCountCache: (version: Int, count: Int) = (-1, 0)

    public func open(_ session: SessionInfo) async {
        deferredSession = nil // [library] a session the user opened replaces the one a Library start kept
        openToken += 1
        let token = openToken
        let switching = currentSession != nil && currentSession?.id != session.id
        if switching { viewer.leaveCanvasTool() } // what a crop or brush commits is the old session's: before the gate
        // Loading from the click on, waits included (a release on a hung volume can take long).
        isLoading = true
        var watchdog: Task<Void, Never>?
        if switching {
            isSwitchingSession = true
            switchingTo = session.name
            let deadline = switchDeadline
            watchdog = Task { [weak self] in
                await deadline()
                guard !Task.isCancelled else { return }
                await self?.abandonSwitch(token, to: session)
            }
        }
        defer {
            watchdog?.cancel()
            if token == openToken {
                isLoading = false
                isSwitchingSession = false
                switchingTo = nil
            }
        }
        // A close in flight finishes first (its store tail would
        // otherwise drop the handle this open reuses).
        if let closing { _ = await Bounded.wait(openWaitLimit) { await closing.value } }
        if let release = releasing[session.id] { await release.value } // this one: until released
        guard token == openToken else { return } // another session was opened meanwhile
        // A folder that isn't there can't open: say where it was and what to do (it may
        // be back since the list was made: a drive plugged in again).
        if session.unavailable?.folderMissing == true, !FileManager.default.fileExists(atPath: session.url.path) {
            show(Toast("“\(session.name)” isn't at \(session.url.deletingLastPathComponent().path) any more "
                       + "(moved, deleted, or its drive isn't connected). Use Locate Folder… to point to it.",
                       symbol: "questionmark.folder", isError: true))
            return
        }
        await saver.flush()
        // A batch that couldn't be written is A's: the store would resolve its ids in the session
        // loaded next, so it never leaves with a switch. Stay on A and say why.
        if switching, saver.hasUnsaved {
            var why = "saving isn't finished"
            var count = saver.pendingCount
            if case .failed(let message, let pending) = saver.state { (why, count) = (message, pending) }
            show(Toast("Still on “\(currentSession?.name ?? "this session")”: \(count) change\(count == 1 ? "" : "s") "
                       + "couldn’t be saved (\(why)), so “\(session.name)” wasn’t opened.",
                       symbol: "exclamationmark.triangle", isError: true))
            return
        }
        do {
            async let p = sessions.photos(in: session)
            async let a = sessions.albums(in: session)
            let (loaded, albums) = try await (p, a)
            let covers: [StackCover]
            do { // after the photos: the store has the session open by then
                covers = try await sessions.stackCovers(in: session)
            } catch { // unreadable stacks.json: stacks show their usual tops
                AppLog.session.error("stack covers unreadable: \(error.localizedDescription, privacy: .private(mask: .hash))")
                covers = []
            }
            let stored = try await sessions.edits(in: session)
            guard token == openToken else {
                // Loading pointed the store at this session: back to the one shown (or,
                // if the newer open hasn't shown its session yet, it re-points when it does).
                if let shown = currentSession { await (sessions as? any CurrentSessionPointing)?.makeCurrent(shown) }
                return
            }
            let previous = currentSession
            // A match or WB pick still running belongs to the previous session.
            for t in sessionBoundTasks.values { t.cancel() }
            sessionBoundTasks = [:]
            // Photo ids are per session (every session numbers from 1): the clipboard, a waiting
            // paste and every cache keyed by id stay with the old one.
            if previous?.id != session.id {
                pendingPaste?.cancel()
                pendingPaste = nil
                clipboard = nil
                clipboardLayerIDs = []
                sessionSwitched()
            }
            currentSession = session
            search.sessionOpened(session) // clear results, index in the background
            people.sessionOpened(session) // faces in the background
            map.sessionChanged()
            archiveSessionChanged(from: previous, to: session)
            immich.sessionOpened()
            self.albums = albums
            browserState.load(covers: covers)
            // Edits come back from the store (sidecars in live mode).
            edits = stored.filter { !$0.value.isDefault }
            history = EditHistory()
            photoHistories = [:]
            filter.smartFolder = nil // ids belong to the previous session
            filter.place = nil // so do a marker's photos (and an area belongs to its map view)
            load(loaded.map { p in
                var p = p
                p.isEdited = !(edits[p.id]?.isIdentity ?? true)
                return p
            })
            selection = []
            focusAfterOpen(session) // the first photo, or the one an update's restart left
            await markDeviceEditsAdopted(session)
            await (sessions as? any CurrentSessionPointing)?.makeCurrent(session) // an older open's late load may have moved it
            await smartFolders.reload(store: sessions, session: session, photos: photos) // full count
            await cull.refresh() // can this session move originals?
            await noteOpened(session) // the recents' "Opened …" date, on every open
            await refreshCaptureOne()
            await refreshLightroom()
            await refreshAppleLibrary()
            Task { await self.refreshVolumes() } // is its drive (and each missing photo's) there?
            AppLog.session.notice("""
                opened \(session.url.path, privacy: .private(mask: .hash)): \(loaded.count) photos, \
                \(loaded.filter(\.isMissing).count) missing
                """)
        } catch {
            guard token == openToken else { return }
            AppLog.session.error("open \(session.url.path, privacy: .private(mask: .hash)) failed: \(error.localizedDescription, privacy: .private(mask: .hash))")
            show(error)
        }
    }

    /// The switch has passed `switchDeadline`: stop waiting for it. The shown session
    /// takes edits again; a late finish of the load finds its token stale and re-points the store.
    func abandonSwitch(_ token: Int, to session: SessionInfo) async {
        guard token == openToken, isSwitchingSession else { return }
        openToken += 1
        isLoading = false
        isSwitchingSession = false
        switchingTo = nil
        // Said with the state change it explains, not after the store has been re-pointed.
        show(Toast("“\(session.name)” is taking too long to open (is its drive connected?). "
                   + "\(currentSession.map { "“\($0.name)”" } ?? "The previous session") is still open.",
                   symbol: "exclamationmark.triangle", isError: true))
        if let shown = currentSession { await (sessions as? any CurrentSessionPointing)?.makeCurrent(shown) }
    }

    /// Whether an edit is refused because a session is loading (and says so).
    func refusingEdits() -> Bool {
        guard isSwitchingSession else { return false }
        show(Toast("Still opening “\(switchingTo ?? "the session")”: that change wasn’t applied.", symbol: "hourglass"))
        return true
    }

    /// Whether `session` is still the one that is open and settled (async results check this
    /// before they apply).
    func isStill(in session: UUID?) -> Bool { currentSession?.id == session && !isSwitchingSession }

    /// Refresh the session list (after an import created one) and optionally open `session`.
    public func reloadSessions(select session: SessionInfo? = nil) async {
        do {
            sessionList = try await sessions.listSessions()
            if let session, let fresh = sessionList.first(where: { $0.id == session.id }) { await open(fresh) }
        } catch {
            show(error)
        }
    }

    /// Forget everything keyed by a photo id of the session being left: ids repeat across
    /// sessions, so a survivor would show or apply one photo's data on another.
    func sessionSwitched() {
        cull.sessionClosed() // finishing a cull: its moves and pending questions are the old session's
        if survey.isActive { survey.finish() }
        upscaler?.cancel()
        recheckedOriginals.removeAll()
        browserState.reset()
        cullTask?.cancel()
        cullTask = nil
        cullProgress = nil
        endOptimize()
        whyReportPhoto = nil
        historyPreview = nil
        viewer.resetSessionCaches()
        developInfo = [:]
        optimizeReports = [:]
        aiMarks = [:]
        aiMarksSettled = []
        lookSignals = [:]
        depthVersions = [:]
        cullSuggestions = [:]
        browserState.bestHints = []
        browserState.aiScores = [:]
    }

    /// Back to "no session": nothing listed, focused or selected; the
    /// session's edits, history and AI suggestions are dropped (they were saved first).
    func clearSessionState() {
        pendingPaste?.cancel()
        pendingPaste = nil
        for t in sessionBoundTasks.values { t.cancel() } // their results belong to this session
        sessionBoundTasks = [:]
        clipboard = nil // its photo ids belong to this session
        clipboardLayerIDs = []
        currentSession = nil
        captureOne = CaptureOneStatus() // the closed session's Capture One state
        confirmingCaptureOneRevert = false
        lightroom = LightroomStatus() // the closed session's Lightroom state
        confirmingLightroomRevert = false
        appleLibrary = AppleLibraryStatus() // and its Aperture / iPhoto state
        confirmingAppleRevert = false
        albums = []
        edits = [:]
        history = EditHistory()
        photoHistories = [:]
        sessionSwitched()
        if openProgress?.phase == .hashing { openProgress = nil }
        filter.smartFolder = nil
        filter.place = nil
        load([])
        selection = []
        anchorID = nil
        focus(nil, extend: false)
    }

    /// What quitting would do to the user's work.
    public enum TerminationDecision: Equatable, Sendable {
        case ok
        /// Writes failed; `pending` changes are not on disk.
        case failed(message: String, pending: Int)
        /// The store keeps changes in memory only and there are changes.
        case unsavedInMemory(count: Int)
        /// Imports are copying; quitting stops them (they resume next time).
        case importRunning(count: Int)
        /// An export is running; quitting stops it (its temps are cleaned up).
        case exportRunning
        /// The last archive run failed: the archive doesn't hold everything.
        case archiveFailed(message: String)
    }

    /// Flush pending saves (bounded by `timeout`) and report whether it's safe to quit.
    public func prepareToTerminate(timeout: Duration = .seconds(5)) async -> TerminationDecision {
        let finished = await flushSaves(within: timeout)
        if !finished { return .failed(message: "Saving is taking too long.", pending: saver.pendingCount) }
        if case .failed(let m, let n) = saver.state { return .failed(message: m, pending: n) }
        if !persistsChanges {
            let changed = edits.count + photos.filter { $0.rating > 0 || $0.pick != .none || $0.colorTag != .none }.count
            if changed > 0 { return .unsavedInMemory(count: changed) }
        }
        return .ok
    }

    /// Flush pending saves; false when that takes longer than `timeout` (the flush goes
    /// on). A race that really returns at the timeout: a task group would wait for the
    /// flush child (awaiting an unstructured task ignores cancellation), so a hung store
    /// used to block quitting for as long as the save took.
    func flushSaves(within timeout: Duration) async -> Bool {
        let saver = saver
        return await withCheckedContinuation { cont in
            let once = FirstResult()
            Task { @MainActor in
                await saver.flush()
                if once.claim() { cont.resume(returning: true) }
            }
            Task {
                try? await Task.sleep(for: timeout)
                if once.claim() { cont.resume(returning: false) }
            }
        }
    }

    /// Called by the window when any sheet closes (the card coordinator presents a queued card).
    public func sheetDidClose() {
        cardCoordinator?.sheetDidClose()
    }

    public func stop() {
        stopMountWatcher()
        cardCoordinator?.stop()
        cardCoordinator = nil
        archiveTask?.cancel()
        viewer.stop()
    }

    // Archive scheduling: the current session is registered, changes are
    // counted (archived after a quiet period), the previous session gets a final
    // archive when another one opens, and quitting cancels a running rsync.
    func archiveSessionChanged(from previous: SessionInfo?, to session: SessionInfo) {
        guard previous?.id != session.id else { return }
        let archive = archive
        Task {
            if let previous { await archive.sessionClosed(previous) }
            await archive.sessionOpened(session)
        }
    }

    func noteArchiveChanges(_ n: Int) {
        guard n > 0, let session = currentSession else { return }
        let archive = archive
        Task { await archive.noteChanges(n, in: session) }
        immich.photosChanged() // developed copies follow edits, ratings and keywords
    }

    /// Quit (after saves): stop background archiving, cancelling a running rsync (bounded).
    public func shutdownArchive() async {
        archiveTask?.cancel()
        await archive.shutdown()
    }

    func startArchiveUpdates() {
        archiveTask?.cancel()
        let stream = archive.statusUpdates()
        archiveTask = Task { [weak self] in
            for await status in stream {
                self?.archiveStatus = status
            }
        }
    }

    /// Any photo in the session is missing its original (kept O(1) for rebuilds).
    @ObservationIgnored var hasMissing = false

    func load(_ loaded: [Photo]) {
        // A store that repeats an id: the first photo wins everywhere (items, order, index).
        var seen = Set<PhotoID>()
        let loaded = loaded.filter { seen.insert($0.id).inserted }
        hasMissing = loaded.contains { $0.isMissing || $0.inTrash } // trashed photos are hidden too
        items = Dictionary(uniqueKeysWithValues: loaded.map { ($0.id, PhotoItem($0)) })
        updateSessionMissing()
        setOrder(sortedIDs(loaded.map(\.id)))
        photosVersion += 1
        rebuildVisible()
    }

    func sortedIDs(_ ids: [PhotoID]) -> [PhotoID] {
        if let searchRanking { return searchRanking.sorted(sortedBySort(ids)) } // relevance first
        return sortedBySort(ids)
    }

    func sortedBySort(_ ids: [PhotoID]) -> [PhotoID] {
        switch sort {
        case .captureTime:
            return ids.sorted { (items[$0]!.photo.meta.captureDate, $0) < (items[$1]!.photo.meta.captureDate, $1) }
        case .filename:
            return ids.sorted { (items[$0]!.photo.filename, $0) < (items[$1]!.photo.filename, $1) }
        case .rating:
            return ids.sorted { (-items[$0]!.photo.rating, $0) < (-items[$1]!.photo.rating, $1) }
        }
    }

    func setOrder(_ ids: [PhotoID]) {
        order = ids
        orderIndex = Dictionary(uniqueKeysWithValues: ids.enumerated().map { ($1, $0) })
    }

    func resort() {
        // The focused photo's position before `orderIndex` changes (H2 refocus).
        let oldIndex = focusedID.flatMap { visibleIndex($0) }
        setOrder(sortedIDs(order))
        rebuildVisible(oldIndex: oldIndex)
    }

    /// Index of `id` in `visibleIDs`, O(log n).
    func visibleIndex(_ id: PhotoID) -> Int? {
        indexIn(visibleIDs, id)
    }

    /// Patch `visibleIDs` for a few changed photos instead of re-filtering all of them.
    func patchVisible(_ changed: [PhotoID]) {
        let oldIndex = focusedID.flatMap { visibleIndex($0) }
        var ids = visibleIDs
        for id in changed {
            guard let p = items[id]?.photo else { continue }
            let matches = filter.matches(p) && searchRanking?.contains(id) != false
            if let i = indexIn(ids, id) {
                if !matches { ids.remove(at: i) }
            } else if matches, let target = orderIndex[id] {
                let at = lowerBound(ids, target)
                ids.insert(id, at: at)
            }
        }
        visibleIDs = ids
        refocusIfFiltered(oldIndex: oldIndex)
    }

    /// First position in `ids` (ascending in `order`) whose order index is >= `target`.
    func lowerBound(_ ids: [PhotoID], _ target: Int) -> Int {
        var lo = 0, hi = ids.count
        while lo < hi {
            let mid = (lo + hi) / 2
            // `ids` may still hold photos a reload just dropped (a relisted session whose
            // ids changed): never trap on them; `indexIn` re-checks the hit.
            if (orderIndex[ids[mid]] ?? Int.max) < target { lo = mid + 1 } else { hi = mid }
        }
        return lo
    }

    func indexIn(_ ids: [PhotoID], _ id: PhotoID) -> Int? {
        guard let target = orderIndex[id] else { return nil }
        let i = lowerBound(ids, target)
        return i < ids.count && ids[i] == id ? i : nil
    }

    func refocusIfFiltered(oldIndex: Int?) {
        if let f = focusedID, visibleIndex(f) == nil {
            selection.remove(f)
            let ids = visibleIDs
            let next = oldIndex.flatMap { ids.isEmpty ? nil : ids[min($0, ids.count - 1)] }
            focus(next, extend: false)
        }
    }

    /// Recompute `visibleIDs`. If the focused photo dropped out of the filter,
    /// focus moves to the photo now at its old position (Lightroom-style),
    /// never back to the start.
    func rebuildVisible(oldIndex known: Int?? = nil) {
        let oldIndex = known ?? focusedID.flatMap { visibleIndex($0) }
        // Missing originals are hidden unless the Missing filter is on, so filter whenever any exist.
        let ids = (filter.isActive || hasMissing || searchRanking != nil) // only the results
            ? order.filter { filter.matches(items[$0]!.photo) && searchRanking?.contains($0) != false } : order
        if ids != visibleIDs { visibleIDs = ids }
        refocusIfFiltered(oldIndex: oldIndex)
    }

    // MARK: - Lookup

    public func photo(_ id: PhotoID) -> Photo? { items[id]?.photo }
    /// The observable item for `id` (cells observe it, so badges update without list diffs).
    public func item(_ id: PhotoID) -> PhotoItem? { items[id] }
    /// Number of photos in the session, O(1).
    public var photoCount: Int { order.count }
    /// All photos in session order. O(n): not for per-keypress paths.
    public var photos: [Photo] { order.compactMap { items[$0]?.photo } }
    /// Visible photos in display order. O(n): prefer `visibleIDs` + `item(_:)`.
    public var visiblePhotos: [Photo] { visibleIDs.compactMap { items[$0]?.photo } }
    public var focusedPhoto: Photo? { focusedID.flatMap(photo) }
    public func edit(for id: PhotoID) -> EditState { edits[id] ?? EditState() }
    public func visibleIndex(of id: PhotoID) -> Int? { visibleIndex(id) }

    /// Selection as ordered photos (focused first if nothing else is selected).
    public var selectedIDs: [PhotoID] {
        let ids = selection.isEmpty ? Set(focusedID.map { [$0] } ?? []) : selection
        if ids.count == 1, let only = ids.first { return visibleIndex(only) != nil ? [only] : [] }
        return visibleIDs.filter(ids.contains)
    }

    // MARK: - Feedback

    /// How long a toast stays up (tests shorten it).
    @ObservationIgnored public var toastDuration: Duration = .seconds(2.5)
    /// Waits out a toast (tests inject an immediate or controlled clock).
    @ObservationIgnored var toastSleep: @Sendable (Duration) async throws -> Void = { try await Task.sleep(for: $0) }

    public func show(_ toast: Toast) {
        self.toast = toast
        toastTask?.cancel()
        // One with a button stays long enough to be clicked.
        let duration = toast.openSession == nil ? toastDuration : toastDuration * 4
        toastTask = Task { [weak self, sleep = toastSleep, duration] in
            try? await sleep(duration)
            if !Task.isCancelled { self?.toast = nil }
        }
    }

    public func show(_ error: Error) {
        show(Toast(error.localizedDescription, symbol: "exclamationmark.triangle", isError: true))
    }
}

/// One photo as the browser observes it. Mutated in place by `AppModel`.
@MainActor @Observable
public final class PhotoItem: Identifiable {
    public let id: PhotoID
    public var photo: Photo
    /// Its original was found missing while the session is open (a render failed on a
    /// deleted, moved or unmounted file): badged, and the viewer says so.
    public var originalMissing = false
    /// Its developed copy for Immich, if it has one (`ImmichModel`).
    public var immichCopy: ImmichCopyBadge?

    /// The original isn't there (per the store, or found so since).
    public var isOriginalMissing: Bool { originalMissing || photo.isMissing }

    init(_ photo: Photo) {
        id = photo.id
        self.photo = photo
    }
}

extension AppModel: FolderOpening {}

// MARK: - Export

extension AppModel {
    /// The selected photos (focused if none) that have a file, in display order;
    /// videos are left out (they aren't exported).
    public var exportItems: [ExportJobItem] {
        selectedIDs.compactMap { id in photo(id).flatMap { p in p.isVideo ? nil : p.url.map { (p, $0) } } }
            .enumerated().map {
                ExportJobItem(photoID: $1.0.id, url: $1.1, seq: $0 + 1, edit: edit(for: $1.0.id), keywords: $1.0.keywords,
                              metadata: $1.0.metadata, location: $1.0.location, rating: $1.0.rating,
                              colorTag: $1.0.colorTag, pick: $1.0.pick)
            }
    }

    /// Selected videos (not exported; the sheet says so).
    public var exportSkippedVideos: Int { selectedIDs.count { photo($0)?.isVideo == true } }

    /// Open the Export sheet for the selection (⌘E). Recipes are saved in the
    /// app's own defaults suite; the default folder is resolved off the main
    /// thread and is never on a card.
    public func beginExport(store: ExportRecipeStore? = nil) {
        let items = exportItems
        guard !items.isEmpty else {
            let videos = exportSkippedVideos
            show(Toast(videos > 0 ? "Videos aren't exported. Select photos to export" : "Select photos to export",
                       symbol: "square.and.arrow.up", isError: true))
            return
        }
        let model = ExportModel(items: items, defaultFolder: nil, service: exporter,
                                store: store ?? ExportRecipeStore(defaults: settings.userDefaults),
                                skippedVideos: exportSkippedVideos, previewSource: watermarkPreviewSource(items),
                                googlePhotos: googlePhotos,
                                googlePhotosOn: { [weak self] in self?.features.isOn(.googlePhotos) ?? false })
        model.aiUpscaleUnavailable = upscaler.map { $0.modelUnavailableReason } ?? UpscaleModelDescriptor.reasons.missing
        model.prepare(session: currentSession)
        exportModel = model
        sheet = .export
    }

    /// The photo the recipe editor previews watermarks on: the focused photo, else the
    /// first one exported; its 1024 px thumbnail (with its edits).
    func watermarkPreviewSource(_ items: [ExportJobItem]) -> WatermarkPreviewSource? {
        let focused = focusedPhoto.flatMap { $0.isVideo ? nil : $0 }
        guard let photo = focused ?? items.first.flatMap({ photo($0.photoID) }), let url = photo.url else { return nil }
        let thumbnails = thumbnails
        return WatermarkPreviewSource(url: url) { await thumbnails.image(for: photo, bucket: 1024) }
    }

    /// The Export sheet closed (a running export keeps going only while the sheet is open).
    public func endExport() {
        if let recipes = exportModel?.recipes { WatermarkLogos.prune(keeping: recipes) }
        exportModel?.cancel()
        exportModel = nil
        if sheet == .export { sheet = nil }
    }
}

/// Resumes a continuation once: the first caller of `claim()` wins.
final class FirstResult: @unchecked Sendable {
    private let lock = NSLock()
    private var taken = false
    func claim() -> Bool { lock.withLock { defer { taken = true }; return !taken } }
}

// MARK: - Quitting during an import

extension AppModel {
    /// The save decision, then: imports still copying need a question first.
    public func quitDecision(timeout: Duration = .seconds(5), imports: ImportRegistry = .shared) async -> TerminationDecision {
        let d = await prepareToTerminate(timeout: timeout)
        guard d == .ok else { return d }
        let running = imports.running.count
        if running > 0 { return .importRunning(count: running) }
        if exportModel?.isRunning == true { return .exportRunning }
        return features.isOn(.archive) ? Self.archiveQuitDecision(archiveStatus.state) : .ok
    }

    /// Quitting after a failed archive run asks first (the photos are safe
    /// here, but the archive doesn't have them all).
    nonisolated static func archiveQuitDecision(_ state: ArchiveStatus.State) -> TerminationDecision {
        switch state {
        case .failed(let m): .archiveFailed(message: m)
        case .problems(let n): .archiveFailed(message: "\(n) item\(n == 1 ? " is" : "s are") not in the archive.")
        default: .ok
        }
    }

    /// Stop a running export before quitting (bounded).
    @discardableResult
    public func stopExportsForQuit(timeout: Duration = .seconds(5)) async -> Bool {
        await exportModel?.stopForQuit(timeout: timeout) ?? true
    }

    /// Stop running imports (bounded): each saves its journal and resumes next time.
    @discardableResult
    public func stopImportsForQuit(timeout: Duration = .seconds(5), imports: ImportRegistry = .shared) async -> Bool {
        await imports.stopAll(timeout: timeout)
    }
}

extension AppModel {
    /// Offered cards live in the app's defaults (the snapshot suite in
    /// `--snapshot` runs), never in `UserDefaults.standard` directly.
    func makeCardCoordinator() -> CardCoordinator {
        CardCoordinator(app: self, store: UserDefaultsOfferedStore(defaults: settings.userDefaults))
    }
}
