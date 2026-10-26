import Foundation
import Observation

/// Drives the import sheet: scan → choose → copy (verified by the service) →
/// create/open the session → eject when the user asks. Never deletes anything
/// from the card, never ejects on its own.
///
/// Card identity (C1): every scan records the card's volume id and a plan only
/// runs against that card; a retry always scans again. A stopped or
/// interrupted import is continued into the same session (H2).
@MainActor @Observable
public final class ImportModel {
    public enum Phase: Equatable, Sendable {
        case idle
        case scanning
        case ready
        case importing
        case finished(ImportSummary)
        case failed(String)
    }

    public enum Destination: Hashable, Sendable {
        case newSession
        case existing(UUID)
    }

    public struct ImportSummary: Equatable, Sendable {
        /// Items copied (or found already in place), verified and registered.
        public var files: Int
        public var bytes: Int64
        public var sessionName: String
        public var seconds: TimeInterval
        public var ejected: Bool
        /// The service verified every file of the plan (`ImportReport.allVerified`).
        public var verified: Bool
        /// Eject was requested but failed (the import itself succeeded).
        public var ejectError: String?
        /// lt-import's `is_complete` (from the service's report; mock: `verified`).
        public var complete = false
        /// Something stays only on the card: "Eject Anyway…" asks first.
        public var needsAcknowledgement = false
        public var scanErrors: [ImportIssue] = []
        public var skippedUnknown: [ImportIssue] = []
        public var failed: [ImportIssue] = []
        public var registerErrors: [ImportIssue] = []
        public var skippedDuplicates = 0
        /// Of `files`, how many are video clips.
        public var videos = 0
        /// The service checks copies (lt-import). The demo mock doesn't.
        public var checkedByService = false
        /// Every media file on the card has a verified library copy.
        public var safeToRemove = false
        /// Exactly what stays only on the card: media without a library copy,
        /// unreadable parts, files that aren't imported (camera databases aside).
        public var stayOnCard: [ImportIssue] = []
        /// Card files whose damaged library copy was replaced by a new import.
        public var damaged: [ImportIssue] = []
    }

    // MARK: Inputs
    public private(set) var volume: URL?
    /// The scanned card's identity (volume UUID), when the service knows it.
    public private(set) var volumeID: String?
    public private(set) var candidates: [ImportCandidate] = []
    /// Parts of the card that couldn't be read (an import can't be complete).
    public private(set) var scanErrors: [ImportIssue] = []
    /// Card files that won't be imported (unknown types, orphan sidecars, camera databases).
    public private(set) var skippedUnknown: [ImportIssue] = []
    /// Files the user unticked.
    public private(set) var excluded: Set<URL> = []
    public var skipDuplicates = true
    public var destination: Destination = .newSession
    public var sessionText = ""
    public var nameTemplate: String
    public var renameTemplate: String
    public var backupFolder: URL?
    /// Subfolder template under the session's `Capture/` (`""`: none).
    public var subfolderTemplate = ""
    /// Re-read the card after copying (catches a flaky reader; doubles card reads).
    public var verifySourceReread = false
    /// The user confirmed "Eject Anyway" (files stay only on the card).
    public var acknowledged = false
    /// The "Eject Anyway…" confirmation is showing.
    public var confirmingEject = false
    /// The service's report for the last run (nil for services that don't verify).
    public private(set) var lastReport: ImportReport?
    /// The volume is local removable media (from its resource values, not its path).
    public private(set) var canEject = false
    /// `false` in the live app until lt-import provides verified copies.
    public let copyEnabled: Bool
    /// Sessions import can continue into (a stopped import's session joins them).
    public private(set) var existingSessions: [SessionInfo]
    /// This card has an unfinished import: continuing it (H2).
    public private(set) var resuming: PendingImportInfo?
    /// Free bytes where the next import would copy to (nil: unknown).
    public private(set) var freeBytes: Int64?
    /// Camera metadata of the files in the rename preview (their real EXIF).
    public private(set) var previewMeta: [URL: ImportFileMeta] = [:]

    // MARK: Progress
    public private(set) var phase: Phase = .idle {
        didSet { if phase != oldValue { Self.log(phase, volume: volumeName) } }
    }

    /// One unified-log line per import lifecycle step (`AppLog.import`).
    static func log(_ phase: Phase, volume: String) {
        switch phase {
        case .importing: AppLog.import.notice("import from \(volume, privacy: .private(mask: .hash)) started")
        case .finished(let s):
            AppLog.import.notice("""
                import from \(volume, privacy: .private(mask: .hash)) finished: \(s.files) files, \(s.bytes) bytes \
                into \(s.sessionName, privacy: .private(mask: .hash)), verified \(s.verified)
                """)
        case .failed(let why): AppLog.import.error("import from \(volume, privacy: .private(mask: .hash)) failed: \(why, privacy: .private(mask: .hash))")
        case .idle, .scanning, .ready: break
        }
    }
    public private(set) var progress: ImportProgress?
    public private(set) var stats = TransferStats()
    /// Stop was pressed; the copy stops at its next chunk.
    public private(set) var isStopping = false
    /// Photos the destination session already had before this import (so an
    /// auto-apply preset touches only the new ones).
    public private(set) var priorPhotoIDs: Set<PhotoID> = []

    @ObservationIgnored private let importer: any ImportService
    @ObservationIgnored private let sessions: any SessionStore
    @ObservationIgnored private let ejector: any Ejecting
    @ObservationIgnored private let onImported: @MainActor (SessionInfo) async -> Void
    @ObservationIgnored private let now: @Sendable () -> Date
    @ObservationIgnored private var task: Task<Void, Never>?
    @ObservationIgnored private var auxTask: Task<Void, Never>?
    @ObservationIgnored private let timeZone: TimeZone
    @ObservationIgnored private let inspector: any VolumeInspecting
    /// The session the app shows (reading the destination's photos points the store away).
    @ObservationIgnored private let shownSession: @MainActor () -> SessionInfo?
    /// Opens a folder read-only in the browser ("Browse Card"); `nil` hides the action.
    @ObservationIgnored public private(set) weak var folderOpener: (any FolderOpening)?

    public init(volume: URL?, importer: any ImportService, sessions: any SessionStore, existingSessions: [SessionInfo],
                nameTemplate: String, renameTemplate: String, preferredSession: URL? = nil,
                ejector: any Ejecting = WorkspaceEjector(),
                inspector: any VolumeInspecting = FileSystemVolumeInspector(), copyEnabled: Bool = true,
                folderOpener: (any FolderOpening)? = nil,
                timeZone: TimeZone = .current, now: @escaping @Sendable () -> Date = { Date() },
                registry: ImportRegistry = .shared, shownSession: @escaping @MainActor () -> SessionInfo? = { nil },
                onImported: @escaping @MainActor (SessionInfo) async -> Void) {
        self.inspector = inspector
        self.copyEnabled = copyEnabled
        self.folderOpener = folderOpener
        self.volume = volume
        self.importer = importer
        self.sessions = sessions
        self.existingSessions = existingSessions
        self.nameTemplate = nameTemplate
        self.renameTemplate = renameTemplate
        self.ejector = ejector
        self.timeZone = timeZone
        self.now = now
        self.onImported = onImported
        self.shownSession = shownSession
        if let preferredSession,
           let s = existingSessions.first(where: { $0.url.standardizedFileURL == preferredSession.standardizedFileURL }) {
            destination = .existing(s.id)
        }
        registry.register(self)
    }

    /// Wired to the app: refresh + open the session, then archive and mirror it.
    public convenience init(model: AppModel, volume: URL?, ejector: any Ejecting = WorkspaceEjector(),
                            inspector: any VolumeInspecting = FileSystemVolumeInspector(),
                            autoApply: @escaping @MainActor () -> Preset? = { PresetStore.shared.autoApplyPreset }) {
        let archive = model.archive
        let me = WeakImportModel()
        let importer = model.importer
        let targets = model.sessionList.filter { (importer as? any ImportDestinationChecking)?.canImport(into: $0) ?? true }
        self.init(volume: volume, importer: importer, sessions: model.sessions, existingSessions: targets,
                  nameTemplate: model.settings.sessionNameTemplate, renameTemplate: model.settings.renameTemplate,
                  preferredSession: model.importResumeTarget,
                  ejector: ejector, inspector: inspector,
                  copyEnabled: model.mode == .demo || importer is any ImportVerifying,
                  folderOpener: (model as AnyObject) as? any FolderOpening,
                  shownSession: { [weak model] in model?.currentSession }) { [weak model] session in
            guard let model else { return }
            model.importResumeTarget = nil
            await model.importFinished(session) // opens it, unless the user closed their session meanwhile
            // L1: "Apply automatically on import" — new photos only, one undo step.
            // lt-import sessions apply it in Rust as each file registers (not twice).
            if !(importer is any ImportVerifying), let preset = autoApply() {
                ImportAutoApply.apply(preset, in: model, prior: me.value?.priorPhotoIDs ?? [], session: session.id)
            }
            // [arc6] archive the session and mirror to Amazon after an import.
            let files = me.value?.lastReport?.filesCopied ?? me.value?.progress?.filesDone ?? 0
            Task { await archive.afterImport(session: session, files: files) }
        }
        me.value = self
    }

    /// The service verifies copies (lt-import). The demo mock doesn't.
    public var verifies: Bool { importer is any ImportVerifying }

    /// Sheet opened without a volume: pick the first mounted card (found off-main), else scan what we have.
    public func findCardIfNeeded(source: VolumeEventSource, detector: DCIMDetector = .fileSystemIncludingImages) async {
        if volume == nil, let card = await CardWatcher.mountedCards(source: source, detector: detector).first {
            await setVolume(card)
        } else {
            await scan()
        }
    }

    public var volumeName: String { volume?.lastPathComponent ?? "No card" }

    // MARK: - Scan

    /// Switch to another card or folder and rescan.
    public func setVolume(_ url: URL) async {
        cancel()
        volume = url
        await scan()
    }

    /// Scan the card again from scratch: new identity, new list, selection reset.
    public func scan() async {
        guard let volume else {
            phase = .idle
            return
        }
        // A rescan (a card mounted again, Retry) can start while this one is still reading:
        // only the latest scan may show its card.
        scanGeneration += 1
        let generation = scanGeneration
        phase = .scanning
        let traits = await inspector.traits(of: volume)
        guard generation == scanGeneration else { return }
        canEject = traits?.canEject ?? false
        do {
            let scan = try await importer.scanCard(volume: volume)
            guard generation == scanGeneration else { return }
            candidates = scan.candidates.sorted { $0.captureDate < $1.captureDate }
            scanErrors = scan.scanErrors
            skippedUnknown = scan.skippedUnknown
            volumeID = scan.volumeID
            excluded = []
            previewMeta = [:]
            phase = .ready
            await findUnfinishedImport()
            refreshAux()
            await settleAux() // the preview shows each file's real camera right away
        } catch {
            guard generation == scanGeneration else { return }
            candidates = []
            scanErrors = []
            skippedUnknown = []
            volumeID = nil
            phase = .failed(error.localizedDescription)
        }
    }

    @ObservationIgnored private var scanGeneration = 0

    /// An unfinished import of *this* card continues into its session.
    private func findUnfinishedImport() async {
        resuming = nil
        guard let volumeID, let r = importer as? any ImportResuming,
              let p = await r.pendingImport(forVolume: volumeID, including: existingSessions.map(\.url)) else { return }
        guard let session = await knownSession(p.sessionURL) else { return }
        resuming = p
        destination = .existing(session.id)
    }

    /// The session at `url` (existing, or opened through the store and added).
    private func knownSession(_ url: URL) async -> SessionInfo? {
        let key = url.standardizedFileURL.resolvingSymlinksInPath()
        if let s = existingSessions.first(where: { $0.url.standardizedFileURL.resolvingSymlinksInPath() == key }) {
            return s
        }
        guard let opener = sessions as? any FolderSessionOpening, let s = try? await opener.add(folder: url) else {
            return nil
        }
        existingSessions.append(s)
        return s
    }

    /// "Discard" on the resume banner: drop the unfinished copies, import fresh.
    public func discardUnfinished() async {
        guard let p = resuming, let r = importer as? any ImportResuming else { return }
        do {
            try await r.discard(p)
            resuming = nil
            destination = .newSession
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// The card has files this import won't take (or can't read): shown before
    /// starting, and they decide the eject wording afterwards.
    public var hasCardIssues: Bool { !scanErrors.isEmpty || !skippedUnknown.isEmpty }
    public var videoCount: Int { candidates.filter { $0.kind == .video }.count }
    /// Candidates whose damaged library copy will be replaced (L3).
    public var damagedCount: Int { candidates.filter { $0.damagedLibraryCopy != nil }.count }

    // MARK: - Selection

    public func isSelected(_ c: ImportCandidate) -> Bool {
        !excluded.contains(c.url) && !(skipDuplicates && c.alreadyImported)
    }

    /// Duplicates can't be ticked while "Skip duplicates" is on.
    public func isLocked(_ c: ImportCandidate) -> Bool { skipDuplicates && c.alreadyImported }

    public func toggle(_ c: ImportCandidate) {
        guard !isLocked(c), !isBusy else { return }
        if excluded.contains(c.url) { excluded.remove(c.url) } else { excluded.insert(c.url) }
        refreshAux()
    }

    public func selectAll() {
        excluded = []
        refreshAux()
    }

    public func selectNone() {
        excluded = Set(candidates.map(\.url))
        refreshAux()
    }

    public var selected: [ImportCandidate] { candidates.filter(isSelected) }
    public var selectedBytes: Int64 { selected.reduce(0) { $0 + $1.totalBytes } }
    public var totalBytes: Int64 { candidates.reduce(0) { $0 + $1.size } }
    public var duplicateCount: Int { candidates.filter(\.alreadyImported).count }

    /// Bytes the import has to write (a backup doubles them on its own volume).
    public var neededBytes: Int64 { selectedBytes }

    /// The selection can't fit where it would go (M1): Import is disabled.
    public var lacksSpace: Bool {
        guard let freeBytes, verifies else { return false }
        return neededBytes > freeBytes
    }

    // MARK: - Naming

    /// Earliest capture among the selection (or all), for `{date}`.
    public var sessionDate: Date {
        (selected.isEmpty ? candidates : selected).map(\.captureDate).min() ?? now()
    }

    public var newSessionName: String {
        ImportTemplates.sessionName(nameTemplate, date: sessionDate, text: sessionText, timeZone: timeZone)
    }

    public var destinationName: String {
        switch destination {
        case .newSession: newSessionName
        case .existing(let id): existingSessions.first { $0.id == id }?.name ?? "—"
        }
    }

    /// Renamed filenames of the first `count` selected files, with each file's
    /// real camera and capture time when the service reads them (L4).
    public func renamePreview(count: Int = 3) -> [String] {
        selected.prefix(count).enumerated().map { i, c in
            let meta = previewMeta[c.url]
            let date = meta?.date ?? c.captureDate
            let camera = meta?.camera ?? ""
            if let r = importer as? any ImportTemplateRendering {
                return (try? r.renderRename(renameTemplate, original: c.filename, date: date, seq: i + 1,
                                           camera: camera, text: sessionText)) ?? "Invalid template"
            }
            return ImportTemplates.rename(renameTemplate, original: c.filename, date: date, seq: i + 1,
                                          camera: camera, text: sessionText, timeZone: timeZone)
        }
    }

    /// Subfolder for the first selected file (`nil`: no subfolder template).
    public var subfolderPreview: String? {
        let t = subfolderTemplate.trimmingCharacters(in: .whitespaces)
        guard !t.isEmpty, let c = selected.first ?? candidates.first else { return nil }
        guard let r = importer as? any ImportTemplateRendering else { return t }
        let meta = previewMeta[c.url]
        return (try? r.renderSubfolder(t, original: c.filename, date: meta?.date ?? c.captureDate,
                                       camera: meta?.camera ?? "", text: sessionText)) ?? "Invalid template"
    }

    /// Reload the rename preview's metadata and the free space (off the main actor).
    public func refreshAux() {
        auxTask?.cancel()
        let want = Array(selected.prefix(3).map(\.url)).filter { previewMeta[$0] == nil }
        let reader = importer as? any ImportFileMetaReading
        let space = importer as? any ImportSpaceChecking
        let dest = makePlan().destination
        auxTask = Task { [weak self] in
            if let reader {
                for url in want {
                    let m = await reader.fileMeta(url)
                    guard !Task.isCancelled else { return }
                    self?.previewMeta[url] = m
                }
            }
            if let space {
                let free = await space.freeBytes(for: dest)
                guard !Task.isCancelled else { return }
                self?.freeBytes = free
            }
        }
    }

    /// Wait for the preview metadata and space check (tests).
    public func settleAux() async { await auxTask?.value }

    // MARK: - Import

    public var isBusy: Bool { phase == .importing || phase == .scanning }

    public var canStart: Bool {
        guard copyEnabled, phase == .ready, !selected.isEmpty, !lacksSpace else { return false }
        if case .existing(let id) = destination { return existingSessions.contains { $0.id == id } }
        return true
    }

    public func makePlan() -> ImportPlan {
        let dest: ImportDestination
        switch destination {
        case .newSession: dest = .newSession(name: newSessionName)
        case .existing(let id): dest = existingSessions.first { $0.id == id }.map(ImportDestination.existing)
            ?? .newSession(name: newSessionName)
        }
        return ImportPlan(candidates: selected, destination: dest, renameTemplate: renameTemplate, backupFolder: backupFolder,
                          volume: volume, subfolderTemplate: subfolderTemplate, verifySourceReread: verifySourceReread,
                          skipDuplicates: skipDuplicates, sessionText: sessionText, volumeID: volumeID)
    }

    public var throughput: Double? { stats.bytesPerSecond }
    public var eta: TimeInterval? { progress.flatMap { stats.eta(totalBytes: $0.bytesTotal) } }

    public func start() {
        guard canStart else { return }
        let plan = makePlan()
        phase = .importing
        isStopping = false
        progress = ImportProgress(filesDone: 0, filesTotal: plan.candidates.count, bytesDone: 0,
                                  bytesTotal: plan.candidates.reduce(0) { $0 + $1.totalBytes })
        stats = TransferStats()
        acknowledged = false
        lastReport = nil
        task = Task { [weak self] in await self?.run(plan) }
    }

    /// Await the running import (tests, the sheet, and quitting).
    public func wait() async { await task?.value }

    /// Stop: the copy stops at its next chunk; verified copies and the journal
    /// stay, so importing again continues where it stopped.
    public func cancel() {
        guard let task else { return }
        if phase == .importing { isStopping = true }
        task.cancel()
    }

    /// Whether Stop is still meaningful (copying, or finishing up after the copy).
    public var isCancellable: Bool { phase == .importing && !isStopping }

    private func run(_ plan: ImportPlan) async {
        let started = now()
        priorPhotoIDs = []
        if case .existing(let s) = plan.destination, let ids = try? await sessions.photos(in: s).map(\.id) {
            priorPhotoIDs = Set(ids)
            // Reading them pointed the store at the destination: back at the session shown.
            if let shown = shownSession(), shown.id != s.id {
                await (sessions as? any CurrentSessionPointing)?.makeCurrent(shown)
            }
        }
        defer {
            task = nil
            isStopping = false
        }
        var last: ImportProgress?
        do {
            for try await p in importer.run(plan) {
                progress = p
                last = p
                stats.record(bytes: p.bytesDone, at: now())
            }
        } catch is CancellationError {
            // Stopped: the verifying service still reports what the run did.
        } catch let e as ImportRunError {
            if e == .cardChanged {
                // Never mix two cards: show the card in the reader instead.
                await scan()
                phase = .failed(e.localizedDescription)
            } else {
                phase = .failed(e.localizedDescription)
            }
            return
        } catch {
            if !Task.isCancelled {
                phase = .failed(error.localizedDescription)
                return
            }
        }
        // Verification comes only from the service's report; no report = not verified.
        if let verifying = importer as? any ImportVerifying {
            let report = await verifying.report(for: plan)
            lastReport = report
            guard let report else {
                phase = Task.isCancelled ? .failed(ImportWording.stopped(done: last?.filesDone ?? 0, total: plan.candidates.count))
                    : .failed("The import didn’t report back. Nothing was deleted from the card; import again to continue.")
                return
            }
            // Finish outside the (possibly cancelled) task: after Stop the session
            // the copies went into still has to be opened and targeted.
            let wasStopped = Task.isCancelled
            await Task { [weak self] in
                await self?.finishVerified(plan, report: report, started: started, stopped: wasStopped)
            }.value
            return
        }
        await finishUnverified(plan, last: last, started: started)
    }

    /// Demo mock: nothing is verified, so nothing is ever called safe.
    private func finishUnverified(_ plan: ImportPlan, last: ImportProgress?, started: Date) async {
        if Task.isCancelled {
            phase = .failed(ImportWording.stopped(done: last?.filesDone ?? 0, total: plan.candidates.count))
            return
        }
        guard let last, last.filesDone == plan.candidates.count else {
            phase = .failed(ImportFailure.incomplete(done: last?.filesDone ?? 0, total: plan.candidates.count)
                .localizedDescription)
            return
        }
        do {
            let session: SessionInfo
            switch plan.destination {
            case .newSession(let name): session = try await sessions.createSession(named: name)
            case .existing(let s): session = s
            }
            var s = ImportSummary(files: last.filesDone, bytes: last.bytesDone, sessionName: session.name,
                                  seconds: now().timeIntervalSince(started), ejected: false, verified: false, ejectError: nil)
            s.videos = plan.candidates.filter { $0.kind == .video }.count
            s.needsAcknowledgement = true
            phase = .finished(s)
            await onImported(session)
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// A verifying service (lt-import) finished or stopped: open the session it
    /// filled, make a retry continue into it, and decide about the card from
    /// its report only.
    private func finishVerified(_ plan: ImportPlan, report: ImportReport, started: Date, stopped wasStopped: Bool) async {
        // Stopped (Stop, a pulled card, or a service that only noticed the task
        // being cancelled): the journal stays; never create a session for it.
        let stopped = report.cancelled || report.sourceLost != nil || wasStopped
        var session: SessionInfo?
        do {
            session = try await destinationSession(plan, report: report, create: !stopped)
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        // H2: whatever happened, importing again goes into this session.
        if let session {
            if !existingSessions.contains(where: { $0.id == session.id }) { existingSessions.append(session) }
            destination = .existing(session.id)
        }
        if stopped {
            let message = report.sourceLost != nil ? ImportWording.cardRemoved(volumeName)
                : ImportWording.stopped(done: report.filesCopied, total: plan.candidates.count)
            if phase == .importing { phase = .failed(message) }
            if let session { await onImported(session) }
            return
        }
        resuming = nil
        let verified = report.allVerified(for: plan)
        let copiedVideos = plan.candidates.filter { $0.kind == .video }.count
        var summary = ImportSummary(files: min(report.filesCopied, report.filesVerified), bytes: report.bytesCopied,
                                    sessionName: session?.name ?? report.sessionName ?? destinationName,
                                    seconds: now().timeIntervalSince(started), ejected: false, verified: verified,
                                    ejectError: nil)
        summary.complete = report.isComplete
        summary.checkedByService = true
        summary.safeToRemove = report.safeToRemove && verified
        summary.scanErrors = report.scanErrors
        summary.skippedUnknown = report.skippedUnknown
        summary.failed = report.failed
        summary.registerErrors = report.registerErrors
        summary.skippedDuplicates = report.skippedDuplicates
        summary.videos = min(copiedVideos, report.filesCopied)
        summary.damaged = report.damagedLibraryCopies
        summary.stayOnCard = report.cardOnly + report.scanErrors
            + report.skippedUnknown.filter { !$0.reason.contains("camera database") }
        summary.needsAcknowledgement = !summary.safeToRemove
        acknowledged = false
        // Cancelled while opening the session: the copy is complete, but the
        // import didn't finish as asked, so say exactly what happened.
        if isStopping {
            phase = .failed(ImportWording.stoppedWhileFinishing(sessionName: summary.sessionName, ejected: false))
            if let session { await onImported(session) }
            return
        }
        phase = .finished(summary)
        if let session { await onImported(session) }
    }

    /// The session the service copied into, as the store knows it.
    private func destinationSession(_ plan: ImportPlan, report: ImportReport, create: Bool) async throws -> SessionInfo? {
        if case .existing(let s) = plan.destination, report.sessionURL == nil || report.sessionURL == s.url { return s }
        if let url = report.sessionURL {
            if let known = existingSessions.first(where: { $0.url.standardizedFileURL == url.standardizedFileURL }) {
                return known
            }
            if let opener = sessions as? any FolderSessionOpening { return try await opener.add(folder: url) }
        }
        if create, case .newSession(let name) = plan.destination { return try await sessions.createSession(named: name) }
        return nil
    }

    /// The eject rule for the finished import (see `ImportEjectGate`).
    public var ejectGate: ImportEjectGate? {
        guard case .finished(let s) = phase else { return nil }
        return ImportEjectGate(verified: s.verified, needsAcknowledgement: s.needsAcknowledgement,
                               acknowledged: acknowledged, canEject: canEject && volume != nil, ejected: s.ejected)
    }

    /// The eject button: "Eject Card" when every file is safe; otherwise the
    /// first press asks ("Eject Anyway…") and only a confirmed one ejects.
    public func ejectNow() async {
        guard let gate = ejectGate, gate.offersEject, let volume, case .finished(var s) = phase else { return }
        if gate.requiresConfirmation {
            confirmingEject = true
            return
        }
        confirmingEject = false
        do {
            try await ejector.eject(volume)
            s.ejected = true
            s.ejectError = nil
        } catch {
            s.ejectError = error.localizedDescription
        }
        phase = .finished(s)
    }

    /// "Eject Anyway" confirmed.
    public func confirmEject() async {
        acknowledged = true
        await ejectNow()
    }

    /// The card went away (unplugged or ejected elsewhere): stop any copy.
    public func volumeDidDisappear(_ url: URL) {
        guard let volume, volume.standardizedFileURL == url.standardizedFileURL else { return }
        let wasImporting = phase == .importing
        cancel()
        if wasImporting || phase == .ready || phase == .scanning {
            phase = .failed(ImportWording.cardRemoved(volume.lastPathComponent))
        }
    }

    /// A card was mounted where this sheet's card was: it may be another card
    /// (C1), so scan it again unless an import is running.
    public func volumeDidAppear(_ url: URL) async {
        guard let volume, volume.standardizedFileURL == url.standardizedFileURL, phase != .importing else { return }
        await scan()
    }

    /// "Browse Card": open the volume read-only in the browser.
    public func browse() async {
        guard let volume, let folderOpener else { return }
        await folderOpener.openFolder(volume)
    }

    public var canBrowse: Bool { volume != nil && folderOpener != nil }

    /// Back to the options after a failure: always a fresh scan (the card in
    /// the reader may be another one), selection reset. The service resumes
    /// from its journal into the same session.
    public func retry() {
        guard case .failed = phase else { return }
        task = Task { [weak self] in await self?.scan() }
    }
}

public enum ImportFailure: Error, Equatable, LocalizedError {
    case incomplete(done: Int, total: Int)

    public var errorDescription: String? {
        switch self {
        case .incomplete(let d, let t): "Import stopped after \(d) of \(t) files. Nothing was removed from the card."
        }
    }
}

/// Import services that render rename/subfolder templates exactly as the copy
/// will (lt-template), for the sheet's live preview.
public protocol ImportTemplateRendering: Sendable {
    func renderRename(_ template: String, original: String, date: Date, seq: Int, camera: String, text: String) throws -> String
    func renderSubfolder(_ template: String, original: String, date: Date, camera: String, text: String) throws -> String
}

/// Camera and capture time of a card file, as the import's templates see them.
public struct ImportFileMeta: Equatable, Sendable {
    public var camera: String?
    public var date: Date?

    public init(camera: String?, date: Date?) {
        self.camera = camera
        self.date = date
    }
}

/// Services that read a card file's EXIF for the rename preview (L4).
public protocol ImportFileMetaReading: Sendable {
    func fileMeta(_ url: URL) async -> ImportFileMeta
}

/// Services that know how much space a destination has (M1).
public protocol ImportSpaceChecking: Sendable {
    func freeBytes(for destination: ImportDestination) async -> Int64?
}

/// Import services that only copy into some sessions (lt-import: sessions it
/// created, never a folder opened in place).
public protocol ImportDestinationChecking: Sendable {
    func canImport(into session: SessionInfo) -> Bool
}

/// An import that didn't finish (its journal is still in the session).
public struct PendingImportInfo: Equatable, Sendable, Identifiable {
    public var id: URL { sessionURL }
    public var sessionURL: URL
    public var done: Int
    public var pending: Int
    public var failed: Int
    /// The card(s) that resume it (volume ids).
    public var volumes: [String]

    public init(sessionURL: URL, done: Int, pending: Int, failed: Int, volumes: [String] = []) {
        self.sessionURL = sessionURL
        self.done = done
        self.pending = pending
        self.failed = failed
        self.volumes = volumes
    }

    public var sessionName: String { sessionURL.lastPathComponent }
}

/// Import services with a resumable journal (lt-import).
public protocol ImportResuming: Sendable {
    /// Unfinished imports in the library and in `sessions` (M5).
    func pendingImports(including sessions: [URL]) async -> [PendingImportInfo]
    /// Abandon it: removes the journal and unfinished `.partial` files only.
    func discard(_ pending: PendingImportInfo) async throws
}

extension ImportResuming {
    public func pendingImports() async -> [PendingImportInfo] { await pendingImports(including: []) }

    /// The unfinished import this card continues (H2), if any.
    public func pendingImport(forVolume id: String, including sessions: [URL] = []) async -> PendingImportInfo? {
        await pendingImports(including: sessions).first { $0.volumes.contains(id) }
    }
}

/// Opens a folder read-only in the browser. `AppModel` conforms (its `openFolder(_:)`).
@MainActor
public protocol FolderOpening: AnyObject {
    func openFolder(_ url: URL) async
}

/// Live import models, so the card coordinator can stop one whose card
/// disappears, rescan one whose card came back, and quitting can stop them.
@MainActor
public final class ImportRegistry {
    public static let shared = ImportRegistry()
    private let models = NSHashTable<ImportModel>.weakObjects()

    public init() {}

    public func register(_ model: ImportModel) { models.add(model) }

    public func volumeDidDisappear(_ url: URL) {
        for m in models.allObjects { m.volumeDidDisappear(url) }
    }

    public func volumeDidAppear(_ url: URL) async {
        for m in models.allObjects { await m.volumeDidAppear(url) }
    }

    public var count: Int { models.allObjects.count }

    /// The import models currently alive (snapshot hook, tests).
    public var live: [ImportModel] { models.allObjects }

    /// Imports copying right now (M3).
    public var running: [ImportModel] { models.allObjects.filter { $0.phase == .importing } }

    /// Stop every running import and wait (bounded) until each has saved its
    /// journal. Returns `false` when one didn't stop in time.
    public func stopAll(timeout: Duration = .seconds(5)) async -> Bool {
        let running = running
        guard !running.isEmpty else { return true }
        for m in running { m.cancel() }
        return await withCheckedContinuation { cont in
            let once = OnceFlag()
            Task { @MainActor in
                for m in running { await m.wait() }
                if once.claim() { cont.resume(returning: true) }
            }
            Task {
                try? await Task.sleep(for: timeout)
                if once.claim() { cont.resume(returning: false) }
            }
        }
    }
}

/// First caller wins (a bounded wait's two racers).
private final class OnceFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false

    func claim() -> Bool {
        lock.withLock {
            defer { done = true }
            return !done
        }
    }
}

/// Weak back-reference used by the convenience initializer's completion closure.
@MainActor
final class WeakImportModel {
    weak var value: ImportModel?
}

/// Applies the auto-apply preset to the photos an import added (L1).
@MainActor
public enum ImportAutoApply {
    /// IDs in the (now open) session that weren't there before the import.
    public static func newPhotoIDs(in model: AppModel, prior: Set<PhotoID>) -> [PhotoID] {
        model.visibleIDs.filter { !prior.contains($0) }
    }

    @discardableResult
    public static func apply(_ preset: Preset, in model: AppModel, prior: Set<PhotoID>, session: UUID? = nil) -> Int {
        // `session`: the import's. Another one may have been opened since; its ids are not these.
        if let session, model.currentSession?.id != session { return 0 }
        let ids = newPhotoIDs(in: model, prior: prior).filter { model.photo($0)?.isVideo != true }
        guard !ids.isEmpty else { return 0 }
        model.updateEdits(ids, label: "Auto-apply “\(preset.name)”") { _, e in e = preset.apply(to: e) }
        return ids.count
    }
}
