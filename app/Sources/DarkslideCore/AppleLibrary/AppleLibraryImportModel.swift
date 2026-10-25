import Foundation
import Observation

/// File ▸ Import Aperture or iPhoto Library…: pick a library; it is read once, a dry run
/// shows what would change (re-run whenever a category is ticked or unticked, the last
/// one wins), then the import runs. Photos are matched to the open session's; the
/// library's own originals folder (`Masters`, or `Originals` in an older iPhoto library)
/// can be opened as a session from the report, in place, its store kept outside the
/// library. The library is only ever read.
@MainActor @Observable
public final class AppleLibraryImportModel {
    public enum Phase: Equatable, Sendable {
        /// Nothing picked yet.
        case choosing
        /// Reading the library (its database is copied first: it can take a while).
        case loading
        /// The dry run is running.
        case previewing
        case preview(LRImportSummary)
        case importing
        case done(LRImportSummary)
        case reverting
        case reverted(LRRevertSummary)
        case failed(String)
    }

    public static let noSession = "No session is open to import into. Open the library's originals as a session below, "
        + "or open the folder with your photos first (File \u{25B8} Open Folder\u{2026})."

    public static let revertHint = "Revert Import takes back every Aperture and iPhoto import since the last revert, "
        + "here or with File \u{25B8} Revert Aperture or iPhoto Import. Changes you make in Darkslide afterwards are kept."

    public private(set) var phase: Phase = .choosing
    /// The session the photos go into; `nil`: none is open (the report can open one).
    public private(set) var session: SessionInfo?
    public private(set) var source: URL?
    public private(set) var kind: AppleLibraryKind = .unknown
    public private(set) var info: AppleLibraryInfo?
    /// The ticked categories.
    public private(set) var categories = AppleImportCategories.all
    /// "Overwrite my Darkslide edits on conflicts" (off: conflicts keep Darkslide's values).
    public var overwriteEdits = false
    /// Photos each category changes, as of the last dry run that included it.
    public private(set) var counts: [LRCategory: Int] = [:]
    @ObservationIgnored let importer: any AppleLibraryImporting
    /// Saves pending Darkslide edits, so the import compares against them.
    @ObservationIgnored let flush: @MainActor () async -> Void
    @ObservationIgnored let onImported: @MainActor () async -> Void
    /// Opens a folder as a session in place; the session it became, `nil` if it didn't.
    @ObservationIgnored let openFolderAsSession: @MainActor (URL) async -> SessionInfo?
    /// The sessions the app knows (to tell which folders are in one already).
    @ObservationIgnored let knownSessions: @MainActor () -> [SessionInfo]
    @ObservationIgnored let debounce: Duration
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var refreshTask: Task<Void, Never>?

    init(session: SessionInfo?, importer: any AppleLibraryImporting, debounce: Duration = .milliseconds(250),
         flush: @escaping @MainActor () async -> Void = {},
         openFolderAsSession: @escaping @MainActor (URL) async -> SessionInfo? = { _ in nil },
         knownSessions: @escaping @MainActor () -> [SessionInfo] = { [] },
         onImported: @escaping @MainActor () async -> Void = {}) {
        self.session = session
        self.importer = importer
        self.debounce = debounce
        self.flush = flush
        self.openFolderAsSession = openFolderAsSession
        self.knownSessions = knownSessions
        self.onImported = onImported
    }

    public var title: String { "Import Aperture or iPhoto Library" }

    public var preview: LRImportSummary? {
        if case .preview(let s) = phase { return s }
        return nil
    }

    /// Something to write: changes, or conflicts the user chose to overwrite.
    public var canImport: Bool {
        guard let p = preview else { return false }
        return p.changeCount > 0 || (overwriteEdits && !p.conflicts.isEmpty)
    }

    /// The import ran and wrote something that a revert can take back.
    public var canRevert: Bool {
        if case .done(let s) = phase { return s.changeCount > 0 || s.overwritten > 0 }
        if case .failed = phase { return failedImportCanRevert }
        return false
    }

    /// An import failed part-way and left a journal: what it did write can be taken back.
    public private(set) var failedImportCanRevert = false

    /// "Rating (212 photos)": the category and what it would change.
    public func label(_ c: LRCategory) -> String {
        let t = AppleImportCategories.title(c)
        guard let n = counts[c], n > 0 else { return t }
        return "\(t) (\(LRImportSummary.photos(n)))"
    }

    // MARK: Choosing a library

    /// Look at what `url` is, read it, and run the dry run (or, with no session open,
    /// show where its originals are so one can be opened).
    public func choose(_ url: URL) async {
        cancelRefresh()
        if let old = info { await importer.releaseAppleLibrary(old.id) }
        info = nil
        source = url
        counts = [:]
        kind = await importer.appleLibraryKind(of: url)
        if let why = kind.refusal(url.lastPathComponent) {
            phase = .failed(why)
            return
        }
        phase = .loading
        do {
            info = try await importer.loadAppleLibrary(url)
        } catch is CancellationError {
            phase = .failed("Cancelled")
            return
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        guard session != nil else {
            phase = .failed(Self.noSession)
            return
        }
        await refresh(generation: bump())
    }

    /// The folder of the library's own originals, when the library is chosen: opening it
    /// as a session is how a library's photos come into Darkslide.
    public var originalsFolder: URL? {
        guard let source, info != nil else { return nil }
        let root = source.lastPathComponent == "AlbumData.xml" ? source.deletingLastPathComponent() : source
        let fm = FileManager.default
        return ["Masters", "Originals"].map { root.appendingPathComponent($0, isDirectory: true) }
            .first { fm.fileExists(atPath: $0.path) }
    }

    // MARK: Dry run

    private func bump() -> Int {
        generation += 1
        return generation
    }

    private func cancelRefresh() {
        generation += 1
        refreshTask?.cancel()
        refreshTask = nil
    }

    /// Tick or untick a category: the dry run is redone (the last change wins).
    public func toggle(_ c: LRCategory) {
        if categories.contains(c) { categories.remove(c) } else { categories.insert(c) }
        guard info != nil, session != nil else { return }
        let g = bump()
        refreshTask?.cancel()
        let wait = debounce
        refreshTask = Task { [weak self] in
            try? await Task.sleep(for: wait)
            guard !Task.isCancelled else { return }
            await self?.refresh(generation: g)
        }
    }

    /// Wait for the scheduled dry run (tests).
    public func settle() async {
        await refreshTask?.value
    }

    private func refresh(generation g: Int) async {
        guard g == generation, let info, let session else { return }
        phase = .previewing
        await flush()
        let ticked = categories
        do {
            var s = try await importer.importAppleLibrary(session, library: info.id, categories: ticked, dryRun: true,
                                                          overwriteEdits: false)
            guard g == generation else { return }
            s.source = "the library"
            for c in ticked { counts[c] = s.count(c) }
            phase = .preview(s)
        } catch is CancellationError {
            guard g == generation else { return }
            phase = .failed("Cancelled")
        } catch {
            guard g == generation else { return }
            phase = .failed(error.localizedDescription)
        }
    }

    // MARK: Import and revert

    /// Import. Conflicts (Darkslide edits since the last import) are kept unless `overwriteEdits`.
    public func run() async {
        guard canImport, let info, let session else { return }
        cancelRefresh()
        phase = .importing
        await flush()
        do {
            var s = try await importer.importAppleLibrary(session, library: info.id, categories: categories, dryRun: false,
                                                          overwriteEdits: overwriteEdits)
            s.source = "the library"
            phase = .done(s)
            await onImported()
        } catch {
            phase = .failed(error.localizedDescription)
            await onImported() // part of it may have been written
            failedImportCanRevert = await importer.canRevertAppleImport(session)
        }
    }

    /// Take the import back (from the result).
    public func revert() async {
        guard canRevert, let session else { return }
        failedImportCanRevert = false
        phase = .reverting
        do {
            var r = try await importer.revertAppleImport(session) ?? LRRevertSummary()
            r.what = "Aperture and iPhoto import"
            phase = .reverted(r)
        } catch {
            phase = .failed(error.localizedDescription)
        }
        await onImported()
    }

    // MARK: Folders not in the session

    public var folderRows: [LightroomImportModel.FolderRow] {
        guard let s = preview ?? { if case .done(let d) = phase { return d } else { return nil } }() else { return [] }
        return LightroomImportModel.folderRows(s, sessions: knownSessions())
    }

    /// Open a folder (the library's originals, or one a referenced original is in) as a
    /// session in place and carry on importing into it: the library and the ticked
    /// categories stay, the dry run is redone for the new session.
    public func openAsSession(_ folder: String) async {
        guard info != nil, let new = await openFolderAsSession(URL(fileURLWithPath: folder)) else { return }
        session = new
        await refresh(generation: bump())
    }

    /// Let go of the loaded library (the sheet closed).
    public func release() async {
        cancelRefresh()
        if let id = info?.id {
            info = nil
            await importer.releaseAppleLibrary(id)
        }
    }
}

extension AppleLibraryImportModel: Equatable {
    public nonisolated static func == (a: AppleLibraryImportModel, b: AppleLibraryImportModel) -> Bool { a === b }
}

extension AppModel {
    var appleImporter: (any AppleLibraryImporting)? { sessions as? any AppleLibraryImporting }

    /// File ▸ Import Aperture or iPhoto Library… is available (live mode).
    public var canImportAppleLibrary: Bool { appleImporter != nil }

    /// File ▸ Revert Aperture or iPhoto Import is available.
    public var canRevertAppleLibrary: Bool {
        appleImporter != nil && appleLibrary.session == currentSession?.id && appleLibrary.canRevert
    }

    func refreshAppleLibrary() async {
        guard let s = currentSession, let importer = appleImporter else {
            appleLibrary = AppleLibraryStatus()
            return
        }
        let revert = await importer.canRevertAppleImport(s)
        guard currentSession?.id == s.id else { return }
        appleLibrary = AppleLibraryStatus(session: s.id, canRevert: revert)
    }

    func appleLibraryImportModel() -> AppleLibraryImportModel? {
        guard let importer = appleImporter else { return nil }
        return AppleLibraryImportModel(
            session: currentSession, importer: importer,
            flush: { [weak self] in await self?.saver.flush() },
            openFolderAsSession: { [weak self] url in await self?.openFolderForAppleLibrary(url) },
            knownSessions: { [weak self] in self?.sessionList ?? [] }) { [weak self] in
            await self?.saver.flush()
            await self?.refreshCurrentSession()
            await self?.refreshAppleLibrary()
        }
    }

    /// Open `url` as a session in place; the session it is now, `nil` if it didn't open.
    func openFolderForAppleLibrary(_ url: URL) async -> SessionInfo? {
        guard let s = await openFolderForLightroom(url) else { return nil }
        await refreshAppleLibrary()
        return s
    }

    /// File ▸ Import Aperture or iPhoto Library…: show the sheet; the user chooses the library in it.
    public func showAppleLibraryImport() {
        guard let m = appleLibraryImportModel() else { return }
        sheet = .appleLibraryImport(m)
    }

    /// File ▸ Revert Aperture or iPhoto Import: ask first (the confirmation calls `revertAppleLibraryImport()`).
    public func requestRevertAppleLibrary() {
        confirmingAppleRevert = canRevertAppleLibrary
    }

    /// Take back the imports since the last revert (after the confirmation).
    public func revertAppleLibraryImport() async {
        guard canRevertAppleLibrary, let session = currentSession, let importer = appleImporter else { return }
        await saver.flush()
        let outcome: Toast
        do {
            if var r = try await importer.revertAppleImport(session) {
                r.what = "Aperture and iPhoto import"
                outcome = Toast(r.message, symbol: "arrow.uturn.backward")
            } else {
                outcome = Toast("No Aperture or iPhoto import to revert", symbol: "arrow.uturn.backward")
            }
        } catch {
            outcome = Toast(error.localizedDescription, symbol: "exclamationmark.triangle", isError: true)
        }
        await refreshCurrentSession()
        await refreshAppleLibrary()
        show(outcome)
    }
}
