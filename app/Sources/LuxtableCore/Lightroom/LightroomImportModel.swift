import Foundation
import Observation

/// File ▸ Import Lightroom…: pick a catalog, a folder of XMP sidecars or presets; the
/// source is read once, a dry run shows what would change (re-run whenever a category
/// is ticked or unticked, the last one wins), then the import runs. Presets need no
/// session. The Lightroom files are only ever read.
@MainActor @Observable
public final class LightroomImportModel {
    public enum Phase: Equatable, Sendable {
        /// Nothing picked yet.
        case choosing
        /// Reading the source (a catalog is copied first: it can take a while).
        case loading
        /// The dry run is running.
        case previewing
        case preview(LRImportSummary)
        case importing
        case done(LRImportSummary)
        case reverting
        case reverted(LRRevertSummary)
        /// A preset source: the presets, ready to import.
        case presets([LRPresetItem])
        case presetsDone(String)
        case failed(String)
    }

    /// Where a catalog folder with photos missing from the session stands.
    public enum FolderState: Equatable, Sendable {
        case inSession(String)
        case available
        case missing
    }

    public struct FolderRow: Equatable, Sendable {
        public var path: String
        public var photos: Int
        public var state: FolderState
    }

    public static let noSession = "Open the folder with your RAW files first (File \u{25B8} Open Folder\u{2026}), then choose "
        + "the Lightroom catalog or sidecar folder again. Edits are imported into the open session."

    public private(set) var phase: Phase = .choosing
    /// The session the edits go into; `nil`: none is open (presets still work).
    public private(set) var session: SessionInfo?
    public private(set) var source: URL?
    public private(set) var kind: LRSourceKind = .unknown
    public private(set) var info: LRSourceInfo?
    /// The ticked categories.
    public private(set) var categories = Set(LRCategory.allCases)
    /// "Overwrite my Luxtable edits on conflicts" (off: conflicts keep Luxtable's values).
    public var overwriteEdits = false
    /// "Replace presets that are already there" (off: they are skipped).
    public var replacePresets = false
    /// Photos each category changes, as of the last dry run that included it.
    public private(set) var counts: [LRCategory: Int] = [:]
    @ObservationIgnored let importer: any LightroomImporting
    @ObservationIgnored let presetStore: PresetStore
    /// Saves pending Luxtable edits, so the import compares against them.
    @ObservationIgnored let flush: @MainActor () async -> Void
    @ObservationIgnored let onImported: @MainActor () async -> Void
    /// Opens a folder as a session in place; the session it became, `nil` if it didn't.
    @ObservationIgnored let openFolderAsSession: @MainActor (URL) async -> SessionInfo?
    /// The sessions the app knows (to tell which catalog folders are in one already).
    @ObservationIgnored let knownSessions: @MainActor () -> [SessionInfo]
    @ObservationIgnored let debounce: Duration
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var refreshTask: Task<Void, Never>?

    init(session: SessionInfo?, importer: any LightroomImporting, presetStore: PresetStore,
         debounce: Duration = .milliseconds(250),
         flush: @escaping @MainActor () async -> Void = {},
         openFolderAsSession: @escaping @MainActor (URL) async -> SessionInfo? = { _ in nil },
         knownSessions: @escaping @MainActor () -> [SessionInfo] = { [] },
         onImported: @escaping @MainActor () async -> Void = {}) {
        self.session = session
        self.importer = importer
        self.presetStore = presetStore
        self.debounce = debounce
        self.flush = flush
        self.openFolderAsSession = openFolderAsSession
        self.knownSessions = knownSessions
        self.onImported = onImported
    }

    public static let revertHint = "Revert Import takes back every Lightroom import since the last revert, "
        + "here or with File \u{25B8} Revert Lightroom Import. Changes you make in Luxtable afterwards are kept."

    public var title: String { "Import Lightroom" }

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

    /// Presets that can be imported.
    public var importablePresets: [LRPresetItem] {
        if case .presets(let p) = phase { return p.filter { $0.unavailable == nil } }
        return []
    }

    /// "Rating (212 photos)": the category and what it would change.
    public func label(_ c: LRCategory) -> String {
        guard let n = counts[c], n > 0 else { return c.title }
        return "\(c.title) (\(LRImportSummary.photos(n)))"
    }

    // MARK: Choosing a source

    /// Look at what `url` is, read it, and (for a catalog or sidecars) run the dry run.
    public func choose(_ url: URL) async {
        cancelRefresh()
        if let old = info { await importer.releaseLightroomSource(old.id) }
        info = nil
        source = url
        counts = [:]
        kind = await importer.lightroomKind(of: url)
        switch kind {
        case .unknown:
            phase = .failed("\u{201c}\(url.lastPathComponent)\u{201d} isn't a Lightroom catalog (.lrcat), a folder of XMP sidecars, or presets.")
        case .presets:
            phase = .loading
            do {
                phase = .presets(try await importer.readLightroomPresets(url))
            } catch is CancellationError {
                phase = .failed("Cancelled")
            } catch {
                phase = .failed(error.localizedDescription)
            }
        case .catalog, .sidecars:
            guard session != nil else {
                phase = .failed(Self.noSession)
                return
            }
            phase = .loading
            do {
                info = try await importer.loadLightroomSource(url)
            } catch is CancellationError {
                phase = .failed("Cancelled")
                return
            } catch {
                phase = .failed(error.localizedDescription)
                return
            }
            await refresh(generation: bump())
        }
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
            let s = try await importer.importLightroom(session, source: info.id, categories: ticked, dryRun: true,
                                                       overwriteEdits: false)
            guard g == generation else { return }
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

    /// Import. Conflicts (Luxtable edits since the last import) are kept unless `overwriteEdits`.
    public func run() async {
        guard canImport, let info, let session else { return }
        cancelRefresh()
        phase = .importing
        await flush()
        do {
            let s = try await importer.importLightroom(session, source: info.id, categories: categories, dryRun: false,
                                                       overwriteEdits: overwriteEdits)
            phase = .done(s)
            await onImported()
        } catch {
            phase = .failed(error.localizedDescription)
            await onImported() // part of it may have been written
            failedImportCanRevert = await importer.canRevertLightroomImport(session)
        }
    }

    /// Take the import back (from the result).
    public func revert() async {
        guard canRevert, let session else { return }
        failedImportCanRevert = false
        phase = .reverting
        do {
            phase = .reverted(try await importer.revertLightroomImport(session) ?? LRRevertSummary())
        } catch {
            phase = .failed(error.localizedDescription)
        }
        await onImported()
    }

    // MARK: Folders not in the session

    public var folderRows: [FolderRow] {
        guard let s = preview ?? { if case .done(let d) = phase { return d } else { return nil } }() else { return [] }
        return Self.folderRows(s, sessions: knownSessions())
    }

    /// The report's folders of photos not in the session: in another session, there to
    /// open, or gone (shared with the Aperture / iPhoto import).
    static func folderRows(_ s: LRImportSummary, sessions: [SessionInfo]) -> [FolderRow] {
        s.notFoundFolders.map { f in
            let path = (f.path as NSString).standardizingPath
            if let hit = sessions.first(where: { Self.contains($0.url.standardizedFileURL.path, path) }) {
                return FolderRow(path: f.path, photos: f.photos, state: .inSession(hit.name))
            }
            var isDir: ObjCBool = false
            let exists = FileManager.default.fileExists(atPath: path, isDirectory: &isDir)
            return FolderRow(path: f.path, photos: f.photos, state: exists && isDir.boolValue ? .available : .missing)
        }
    }

    private static func contains(_ root: String, _ path: String) -> Bool {
        path == root || path.hasPrefix(root.hasSuffix("/") ? root : root + "/")
    }

    /// Open a catalog folder as a session in place and carry on importing into it: the
    /// source and the ticked categories stay, the dry run is redone for the new session.
    public func openAsSession(_ folder: String) async {
        guard info != nil, let new = await openFolderAsSession(URL(fileURLWithPath: folder)) else { return }
        session = new
        await refresh(generation: bump())
    }

    // MARK: Presets

    /// Import the presets into My Presets. A name that is already there is skipped
    /// (or replaced, with `replacePresets`).
    public func importPresets() {
        guard case .presets = phase else { return }
        var added = 0, replaced = 0, skipped = 0
        for item in importablePresets {
            let existing = presetStore.userPresets.first { $0.name == item.name }
            if let existing {
                guard replacePresets else {
                    skipped += 1
                    continue
                }
                presetStore.replace(existing.id, with: item.preset)
                replaced += 1
            } else if presetStore.all.contains(where: { $0.name == item.name }) {
                skipped += 1 // a built-in style of that name
            } else {
                presetStore.add(item.preset)
                added += 1
            }
        }
        var parts = ["\(added) preset\(added == 1 ? "" : "s") imported"]
        if replaced > 0 { parts.append("\(replaced) replaced") }
        if skipped > 0 { parts.append("\(skipped) already there") }
        phase = .presetsDone(parts.joined(separator: ", "))
    }

    /// Let go of the loaded source (the sheet closed).
    public func release() async {
        cancelRefresh()
        if let id = info?.id {
            info = nil
            await importer.releaseLightroomSource(id)
        }
    }
}

extension LightroomImportModel: Equatable {
    public nonisolated static func == (a: LightroomImportModel, b: LightroomImportModel) -> Bool { a === b }
}

extension AppModel {
    var lightroomImporter: (any LightroomImporting)? { sessions as? any LightroomImporting }

    /// File ▸ Import Lightroom… is available (presets need no session; the rest does).
    public var canImportLightroom: Bool { lightroomImporter != nil }

    /// File ▸ Revert Lightroom Import is available.
    public var canRevertLightroom: Bool { lightroomImporter != nil && lightroom.session == currentSession?.id && lightroom.canRevert }

    func refreshLightroom() async {
        guard let s = currentSession, let importer = lightroomImporter else {
            lightroom = LightroomStatus()
            return
        }
        let revert = await importer.canRevertLightroomImport(s)
        guard currentSession?.id == s.id else { return }
        lightroom = LightroomStatus(session: s.id, canRevert: revert)
    }

    func lightroomImportModel() -> LightroomImportModel? {
        guard let importer = lightroomImporter else { return nil }
        return LightroomImportModel(
            session: currentSession, importer: importer, presetStore: lightroomPresetStore,
            flush: { [weak self] in await self?.saver.flush() },
            openFolderAsSession: { [weak self] url in await self?.openFolderForLightroom(url) },
            knownSessions: { [weak self] in self?.sessionList ?? [] }) { [weak self] in
            await self?.saver.flush()
            await self?.refreshCurrentSession()
            await self?.refreshLightroom()
        }
    }

    /// Open `url` as a session in place; the session it is now, `nil` if it didn't open.
    func openFolderForLightroom(_ url: URL) async -> SessionInfo? {
        await openFolder(url)
        guard let s = currentSession,
              s.url.standardizedFileURL.resolvingSymlinksInPath() == url.standardizedFileURL.resolvingSymlinksInPath() else { return nil }
        await refreshLightroom()
        return s
    }

    /// File ▸ Import Lightroom…: show the sheet; the user chooses the source in it.
    public func showLightroomImport() {
        guard let m = lightroomImportModel() else { return }
        sheet = .lightroomImport(m)
    }

    /// File ▸ Revert Lightroom Import: ask first (the confirmation calls `revertLightroomImport()`).
    public func requestRevertLightroom() {
        confirmingLightroomRevert = canRevertLightroom
    }

    /// Take back the imports since the last revert (after the confirmation).
    public func revertLightroomImport() async {
        guard canRevertLightroom, let session = currentSession, let importer = lightroomImporter else { return }
        await saver.flush()
        let outcome: Toast
        do {
            let r = try await importer.revertLightroomImport(session)
            outcome = Toast(r?.message ?? "No Lightroom import to revert", symbol: "arrow.uturn.backward")
        } catch {
            outcome = Toast(error.localizedDescription, symbol: "exclamationmark.triangle", isError: true)
        }
        await refreshCurrentSession()
        await refreshLightroom()
        show(outcome)
    }
}
