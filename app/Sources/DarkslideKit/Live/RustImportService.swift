import DarkslideFFI
import Foundation

/// Live card import through ds-import (`ds-ffi` `scan_card` / `CardScan.import`).
///
/// - Scan: read-only walk of the card; records the card's identity (volume
///   UUID); candidates the library already holds (global index, verified by
///   full BLAKE3) are marked; damaged library copies are reported;
///   unreadable parts and files that won't be imported are returned too.
/// - Run: only against the card that was scanned (C1: a different card is
///   refused and scanned again, never mixed into a run). Copies into a session
///   under the library root (new) or an existing import session, with the
///   verified single-pass copy, optional backup, and optional source re-read;
///   each verified file is registered in the session (a file counts as
///   imported only once registered) and the session is recorded in the global
///   index. Nothing is created when the import can't fit (M1). The report is
///   the only source of "verified" and "safe to remove".
/// - Stop keeps the journal; importing the same card again continues into the
///   same session (H2), even from a new sheet or after relaunch
///   (`pendingImport(forVolume:)`).
public actor RustImportService: ImportService, ImportVerifying, ImportResuming, ImportTemplateRendering,
    ImportDestinationChecking, ImportFileMetaReading, ImportSpaceChecking {
    public typealias AutoApplyProvider = @Sendable () async -> Preset?
    public typealias LibraryRootProvider = @Sendable () async -> URL
    /// The metadata preset set on every imported photo (creator, copyright…), if any.
    public typealias MetadataPresetProvider = @Sendable () async -> IPTCMetadata?

    public nonisolated let globalIndex: URL?
    private let root: LibraryRootProvider
    private let autoApply: AutoApplyProvider?
    private let metadataPreset: MetadataPresetProvider?
    /// Debug/screenshot: slow card reads (`DS_IMPORT_THROTTLE_MS`).
    private let throttleMs: UInt32
    private var scans: [String: CardScan] = [:]
    private var last: (plan: ImportPlan, report: ImportReport)?
    /// The run in flight (a stop still waits for its report).
    private var inflight: (plan: ImportPlan, task: Task<ImportReport?, Never>)?
    /// Card (volume id) → the session an unfinished run of it copied into (H2).
    private var resumeDirs: [String: URL] = [:]

    public init(libraryRoot: URL = RustImportService.defaultLibraryRoot,
                globalIndex: URL? = RustImportService.defaultGlobalIndex,
                autoApply: AutoApplyProvider? = nil, metadataPreset: MetadataPresetProvider? = nil,
                throttleMs: UInt32 = RustImportService.envThrottle) {
        self.init(libraryRoot: { libraryRoot }, globalIndex: globalIndex, autoApply: autoApply,
                  metadataPreset: metadataPreset, throttleMs: throttleMs)
    }

    /// `libraryRoot` is read at each import (H4: the sessions folder setting).
    public init(libraryRoot: @escaping LibraryRootProvider, globalIndex: URL? = RustImportService.defaultGlobalIndex,
                autoApply: AutoApplyProvider? = nil, metadataPreset: MetadataPresetProvider? = nil,
                throttleMs: UInt32 = RustImportService.envThrottle) {
        root = libraryRoot
        self.globalIndex = globalIndex
        self.autoApply = autoApply
        self.metadataPreset = metadataPreset
        self.throttleMs = throttleMs
    }

    /// Where new sessions go now.
    public func libraryRoot() async -> URL { await root() }

    /// `~/Pictures/Darkslide`: the default sessions folder (and where earlier
    /// builds put every import).
    public static var picturesLibrary: URL {
        let pictures = FileManager.default.urls(for: .picturesDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Pictures")
        return pictures.appendingPathComponent("Darkslide", isDirectory: true)
    }

    /// `$DS_LIBRARY_ROOT`, else a scratch library for `--snapshot` runs, else
    /// `sessionsFolder` (H4: the folder chosen in onboarding).
    public static func libraryRoot(sessionsFolder: URL?) -> URL {
        if let env = ProcessInfo.processInfo.environment["DS_LIBRARY_ROOT"], !env.isEmpty {
            return URL(fileURLWithPath: env, isDirectory: true)
        }
        if CommandLine.arguments.contains("--snapshot") {
            return AppPaths.support.appendingPathComponent("Library", isDirectory: true)
        }
        return sessionsFolder ?? picturesLibrary
    }

    /// The library root without settings (tests, fallback).
    public static var defaultLibraryRoot: URL { libraryRoot(sessionsFolder: nil) }

    /// `<AppPaths.support>/global.db`.
    public static var defaultGlobalIndex: URL { AppPaths.support.appendingPathComponent("global.db") }

    public static var envThrottle: UInt32 {
        UInt32(ProcessInfo.processInfo.environment["DS_IMPORT_THROTTLE_MS"] ?? "") ?? 0
    }

    /// A folder ds-import can copy into: a session it created (`<dir>/.ds/settings/session.json`
    /// next to `Capture/`), never a folder opened in place.
    public nonisolated static func isImportSession(_ url: URL) -> Bool {
        let fm = FileManager.default
        var dir: ObjCBool = false
        return fm.fileExists(atPath: url.appendingPathComponent(".ds/settings/session.json").path)
            && fm.fileExists(atPath: url.appendingPathComponent("Capture").path, isDirectory: &dir) && dir.boolValue
    }

    public nonisolated func canImport(into session: SessionInfo) -> Bool { Self.isImportSession(session.url) }

    // MARK: - Scan

    public func scan(volume: URL) async throws -> [ImportCandidate] {
        try await scanCard(volume: volume).candidates
    }

    public func scanCard(volume: URL) async throws -> ImportScan {
        let index = globalIndex?.path
        let scan = try await RustSessionStore.call { token in
            try await DarkslideFFI.scanCard(root: volume.path, globalIndex: index, token: token)
        }
        scans[Self.key(volume)] = scan
        return Self.importScan(try scan.summary())
    }

    static func key(_ volume: URL) -> String { volume.standardizedFileURL.path }

    static func importScan(_ s: CardScanSummary) -> ImportScan {
        ImportScan(candidates: s.candidates.map(candidate), scanErrors: s.scanErrors.map(issue),
                   skippedUnknown: s.skippedUnknown.map(issue), volumeID: s.volumeId)
    }

    static func candidate(_ c: ImportCandidateRecord) -> ImportCandidate {
        ImportCandidate(url: URL(fileURLWithPath: c.path), size: Int64(clamping: c.size),
                        captureDate: Date(timeIntervalSince1970: Double(c.mtimeMs) / 1000),
                        alreadyImported: c.alreadyImported, kind: c.kind == .video ? .video : .photo,
                        sidecars: c.sidecars.map { URL(fileURLWithPath: $0) }, totalBytes: Int64(clamping: c.totalBytes),
                        damagedLibraryCopy: c.damagedLibraryCopy.map { URL(fileURLWithPath: $0) })
    }

    static func issue(_ i: CardIssue) -> ImportIssue {
        ImportIssue(url: URL(fileURLWithPath: i.path), reason: i.reason)
    }

    // MARK: - Run

    public nonisolated func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> {
        AsyncThrowingStream { cont in
            let task = Task { await self.start(plan, cont) }
            cont.onTermination = { _ in task.cancel() }
        }
    }

    /// The report of `plan`'s run; waits for a run in flight (a stopped one too).
    public func report(for plan: ImportPlan) async -> ImportReport? {
        if let inflight, inflight.plan == plan { _ = await inflight.task.value }
        guard let last, last.plan == plan else { return nil }
        return last.report
    }

    /// A free folder for a new session: `name`, then `name 2`, `name 3`…
    static func newSessionDir(in root: URL, name: String) -> URL {
        let fm = FileManager.default
        var dir = root.appendingPathComponent(name, isDirectory: true)
        var n = 2
        while fm.fileExists(atPath: dir.path) {
            dir = root.appendingPathComponent("\(name) \(n)", isDirectory: true)
            n += 1
        }
        return dir
    }

    func options(for plan: ImportPlan) async throws -> ImportOptions {
        let dir: URL, name: String, create: Bool
        switch plan.destination {
        case .newSession(let n):
            if let id = plan.volumeID, let resume = resumeDirs[id], Self.isImportSession(resume) {
                // H2: an interrupted run of this card made a session: continue it.
                dir = resume
            } else {
                let lib = await root()
                try FileManager.default.createDirectory(at: lib, withIntermediateDirectories: true)
                dir = Self.newSessionDir(in: lib, name: n)
            }
            name = dir.lastPathComponent
            create = true
        case .existing(let s):
            guard Self.isImportSession(s.url) else {
                throw MockError.invalid("“\(s.name)” is a folder opened in place. Import into a new session instead.")
            }
            dir = s.url
            name = s.name
            create = false
        }
        let preset = try await autoApply?().map(RustSessionStore.autoApplyRecord)
        let metadata = await metadataPreset?().flatMap { $0.isEmpty ? nil : RustSessionStore.record($0) }
        return ImportOptions(
            sessionDir: dir.path, sessionName: name, createSession: create,
            include: plan.candidates.map(\.url.path),
            subfolder: ImportTemplates.dsTemplate(plan.subfolderTemplate, text: plan.sessionText),
            rename: ImportTemplates.dsTemplate(plan.renameTemplate, text: plan.sessionText),
            backupDest: plan.backupFolder?.path, verifySourceReread: plan.verifySourceReread,
            skipDuplicates: plan.skipDuplicates, globalIndex: globalIndex?.path, autoApply: preset,
            metadataPreset: metadata, throttleMs: throttleMs)
    }

    /// The scan the plan was made from. C1: the card in the reader must be the
    /// scanned one; a scan of another card is never used for this plan.
    private func cardScan(for plan: ImportPlan) async throws -> CardScan {
        guard let volume = plan.volume else { throw MockError.invalid("No card to import from") }
        if let s = scans[Self.key(volume)], plan.volumeID == nil || (try? s.summary().volumeId) == plan.volumeID {
            return s
        }
        let fresh = try await scanCard(volume: volume)
        guard plan.volumeID == nil || fresh.volumeID == plan.volumeID else { throw ImportRunError.cardChanged }
        guard let s = scans[Self.key(volume)] else { throw MockError.notFound(volume.path) }
        return s
    }

    private func start(_ plan: ImportPlan, _ cont: AsyncThrowingStream<ImportProgress, Error>.Continuation) async {
        let task = Task<ImportReport?, Never> { await self.perform(plan, cont) }
        inflight = (plan, task)
        _ = await withTaskCancellationHandler {
            await task.value
        } onCancel: {
            task.cancel()
        }
        if inflight?.plan == plan { inflight = nil }
    }

    private func perform(_ plan: ImportPlan,
                         _ cont: AsyncThrowingStream<ImportProgress, Error>.Continuation) async -> ImportReport? {
        do {
            let scan = try await cardScan(for: plan)
            let opts = try await options(for: plan)
            let relay = ImportProgressRelay(files: plan.candidates.count,
                                            bytes: plan.candidates.reduce(0) { $0 + $1.totalBytes }) { cont.yield($0) }
            let result: ImportResult
            do {
                result = try await RustSessionStore.call(mapping: false) { token in
                    try await scan.`import`(options: opts, listener: relay, token: token)
                }
            } catch let e as DsError {
                throw Self.runError(e)
            }
            let report = Self.report(result, plan: plan)
            last = (plan, report)
            let id = plan.volumeID ?? result.volumeId
            resumeDirs[id] = report.isComplete ? nil : URL(fileURLWithPath: result.sessionDir, isDirectory: true)
            // Resume later needs a fresh scan (the card may have changed).
            if let v = plan.volume { scans[Self.key(v)] = nil }
            cont.yield(ImportProgress(filesDone: report.filesCopied, filesTotal: plan.candidates.count,
                                      bytesDone: Int64(clamping: result.bytesCopied),
                                      bytesTotal: max(relay.bytesTotal, Int64(clamping: result.bytesCopied))))
            cont.finish()
            return report
        } catch {
            if case ImportRunError.cardChanged = error, let v = plan.volume { scans[Self.key(v)] = nil }
            cont.finish(throwing: error)
            return nil
        }
    }

    /// ds-ffi's refusals as the app's errors (card changed, no space).
    static func runError(_ e: DsError) -> Error {
        switch e {
        case .InvalidArgument(let m) where m.hasPrefix("the card changed since it was scanned"):
            return ImportRunError.cardChanged
        case .Io(let m) where m.hasPrefix("not enough free space"):
            return ImportRunError.notEnoughSpace(
                "Not enough free space for this import. Nothing was copied or created; choose fewer files or another sessions folder.")
        default:
            return RustSessionStore.mapped(e)
        }
    }

    /// The app's report from ds-import's: counts are of the plan's photos/clips
    /// (sidecars travel with them); a duplicate the library holds counts as
    /// verified (ds-import compared full hashes). A file that couldn't be
    /// registered is not counted as imported.
    static func report(_ r: ImportResult, plan: ImportPlan) -> ImportReport {
        let primaries = Set(plan.candidates.map(\.url.path))
        let copied = Set(r.copied.map(\.src)).intersection(primaries).count
        let dups = Set(r.skippedDuplicates).intersection(primaries).count
        return ImportReport(
            filesTotal: plan.candidates.count, filesCopied: copied + dups, filesVerified: copied + dups,
            bytesCopied: Int64(clamping: r.bytesCopied), isComplete: r.isComplete,
            ejectNeedsConfirmation: !r.safeToRemove, scanErrors: r.scanErrors.map(issue),
            skippedUnknown: r.skippedUnknown.map(issue),
            failed: (r.failed + r.backupFailed).map(issue), registerErrors: r.registerErrors.map(issue),
            skippedDuplicates: r.skippedDuplicates.count, cancelled: r.cancelled,
            sourceLost: r.sourceLost.map { _ in "The card was removed" },
            sessionURL: URL(fileURLWithPath: r.sessionDir, isDirectory: true), sessionName: r.sessionName,
            cardOnly: r.cardOnly.map(issue), safeToRemove: r.safeToRemove,
            damagedLibraryCopies: r.damagedLibraryCopies.map(issue), volumeID: r.volumeId)
    }

    // MARK: - Resume

    /// Unfinished imports under the library root(s) (the sessions folder and
    /// `~/Pictures/Darkslide`) and in `sessions` (M5).
    public func pendingImports(including sessions: [URL]) async -> [PendingImportInfo] {
        let roots = [await root(), Self.picturesLibrary].map(\.path)
        let known = Array(resumeDirs.values)
        let list = (try? DarkslideFFI.pendingImports(roots: Array(Set(roots)),
                                                     sessions: (sessions + known).map(\.path))) ?? []
        return list.map {
            PendingImportInfo(sessionURL: URL(fileURLWithPath: $0.sessionDir, isDirectory: true), done: Int($0.done),
                              pending: Int($0.pending), failed: Int($0.failed), volumes: $0.volumes)
        }
    }

    public func discard(_ pending: PendingImportInfo) async throws {
        do {
            try DarkslideFFI.discardPendingImport(sessionDir: pending.sessionURL.path, backupDest: nil)
            for (k, v) in resumeDirs where v.standardizedFileURL == pending.sessionURL.standardizedFileURL {
                resumeDirs[k] = nil
            }
        } catch let e as DsError {
            throw RustSessionStore.mapped(e)
        }
    }

    // MARK: - Templates (ds-template, the same code the copy uses)

    public nonisolated func renderRename(_ template: String, original: String, date: Date, seq: Int, camera: String,
                                         text: String) throws -> String {
        guard let t = ImportTemplates.dsTemplate(template, text: text) else { return original }
        return try DarkslideFFI.renderImportTemplate(template: t, fileName: original, captureMs: Self.wallClockMs(date),
                                                     camera: camera.isEmpty ? nil : camera, seq: UInt64(max(0, seq)),
                                                     subfolder: false)
    }

    public nonisolated func renderSubfolder(_ template: String, original: String, date: Date, camera: String,
                                            text: String) throws -> String {
        guard let t = ImportTemplates.dsTemplate(template, text: text) else { return "" }
        return try DarkslideFFI.renderImportTemplate(template: t, fileName: original, captureMs: Self.wallClockMs(date),
                                                     camera: camera.isEmpty ? nil : camera, seq: 1, subfolder: true)
    }

    /// ds-template reads dates as the camera clock (UTC convention): the local
    /// wall-clock time of `date`, written as UTC.
    static func wallClockMs(_ date: Date, timeZone: TimeZone = .current) -> Int64 {
        Int64(((date.timeIntervalSince1970 + Double(timeZone.secondsFromGMT(for: date))) * 1000).rounded())
    }

    /// The inverse of `wallClockMs`: a camera clock (as UTC) read as local time.
    static func date(wallClockMs ms: Int64, timeZone: TimeZone = .current) -> Date {
        let utc = Date(timeIntervalSince1970: Double(ms) / 1000)
        return utc.addingTimeInterval(-Double(timeZone.secondsFromGMT(for: utc)))
    }

    // MARK: - Preview metadata and space

    /// The file's camera and EXIF capture time, as the copy's templates use them.
    public nonisolated func fileMeta(_ url: URL) async -> ImportFileMeta {
        let m = await Task.detached { DarkslideFFI.cardFileMeta(path: url.path) }.value
        return ImportFileMeta(camera: m.camera, date: m.captureMs.map { Self.date(wallClockMs: $0) })
    }

    /// Free bytes where `destination` would be written.
    public func freeBytes(for destination: ImportDestination) async -> Int64? {
        let path: String
        switch destination {
        case .newSession: path = await root().path
        case .existing(let s): path = s.url.path
        }
        return await Task.detached { DarkslideFFI.freeSpace(path: path).map { Int64(clamping: $0) } }.value
    }
}

/// Maps ds-import progress (`"import"`: bytes, `"import.files"`: files) to `ImportProgress`.
final class ImportProgressRelay: ProgressListener, @unchecked Sendable {
    private let lock = NSLock()
    private var files = (done: 0, total: 0)
    private var bytes = (done: Int64(0), total: Int64(0))
    private let yield: @Sendable (ImportProgress) -> Void

    init(files total: Int, bytes totalBytes: Int64, _ yield: @escaping @Sendable (ImportProgress) -> Void) {
        files.total = total
        bytes.total = totalBytes
        self.yield = yield
    }

    var bytesTotal: Int64 { lock.withLock { bytes.total } }

    func onProgress(event: ProgressEvent) {
        let p: ImportProgress? = lock.withLock {
            switch event.kind {
            case "import":
                bytes = (Int64(clamping: event.completed), max(bytes.total, Int64(clamping: event.total)))
            case "import.files":
                files = (Int(clamping: event.completed), Int(clamping: event.total))
            default:
                return nil
            }
            return ImportProgress(filesDone: files.done, filesTotal: files.total, bytesDone: bytes.done, bytesTotal: bytes.total)
        }
        if let p { yield(p) }
    }
}

extension ImportTemplates {
    /// The app's template tokens in ds-template syntax: `{original}` → `{name}`,
    /// `{seq}` → `{seq:3}`, `{seq4}` → `{seq:4}`, `{text}` → the session text
    /// (braces escaped). `{date}`, `{time}`, `{camera}` are shared. `nil` for an
    /// empty template or one that keeps the original name (`{original}`).
    public static func dsTemplate(_ template: String, text: String) -> String? {
        let t = template.trimmingCharacters(in: .whitespaces)
        guard !t.isEmpty, t != "{original}" else { return nil }
        let escaped = text.trimmingCharacters(in: .whitespaces)
            .replacingOccurrences(of: "{", with: "{{").replacingOccurrences(of: "}", with: "}}")
        return t.replacingOccurrences(of: "{original}", with: "{name}")
            .replacingOccurrences(of: "{seq4}", with: "{seq:4}")
            .replacingOccurrences(of: "{seq}", with: "{seq:3}")
            .replacingOccurrences(of: "{text}", with: escaped)
    }
}
