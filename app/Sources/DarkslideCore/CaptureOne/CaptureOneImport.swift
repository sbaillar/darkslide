import Foundation

// Capture One session import (ds-c1 through the FFI `import_capture_one`):
// File ▸ Import Capture One Edits…, and an offer after opening a folder that
// is a Capture One session. The C1 folder is only ever read; a dry run comes
// first, and Darkslide edits made since the last import are only replaced
// when the user says so. File ▸ Revert Capture One Import takes the imports
// back (ds-c1's journal); ⌘Z doesn't, because the import reloads the session.

/// What a Capture One import did, or would do (a dry run). Mirrors ds-c1's report.
public struct C1ImportSummary: Sendable, Equatable {
    public struct Count: Sendable, Equatable {
        public var key: String
        public var count: Int
    }

    public struct Unmapped: Sendable, Equatable {
        public var key: String
        public var reason: String
        public var count: Int
    }

    public struct Conflict: Sendable, Equatable {
        public var photo: PhotoID
        public var file: String
        public var field: String
    }

    public struct Skipped: Sendable, Equatable {
        public var name: String
        public var reason: String
    }

    public var dryRun = true
    public var writtenBy: String?
    public var images = 0
    public var matched = 0
    public var variantsCreated = 0
    public var notFound: [String] = []
    public var outsideFolder = 0
    public var inTrash = 0
    public var inSelects = 0
    public var rated = 0
    public var tagged = 0
    public var picked = 0
    public var keyworded = 0
    public var editsMapped = 0
    public var editedInC1 = 0
    public var withUnmapped = 0
    public var mapped: [Count] = []
    public var unmapped: [Unmapped] = []
    public var inSync = 0
    public var keptDarkslide = 0
    public var deletedInDarkslide = 0
    public var conflicts: [Conflict] = []
    public var overwritten = 0
    public var readOnly = 0
    public var albumsCreated = 0
    public var albumsUpdated = 0
    public var skipped: [Skipped] = []
    public var warnings: [String] = []

    /// Changes the import writes (or would write).
    public var changeCount: Int {
        rated + tagged + picked + keyworded + editsMapped + variantsCreated + albumsCreated + albumsUpdated
    }

    /// Anything to import or decide.
    public var hasWork: Bool { changeCount > 0 || !conflicts.isEmpty }

    /// Photos with an edit the user made in Darkslide that the import would replace.
    public var conflictPhotos: Int { Set(conflicts.map(\.photo)).count }

    /// What is (or would be) brought over, one line each; zero counts are left out.
    public var lines: [String] {
        let verb = dryRun ? "will be" : "were"
        var out: [String] = []
        func add(_ n: Int, _ what: String) {
            if n > 0 { out.append("\(Self.photos(n)) \(what)") }
        }
        add(rated, "\(verb) rated")
        add(tagged, "\(verb) colour-tagged")
        add(picked, "\(verb) picked or unpicked (Capture One's Selects folder)")
        add(keyworded, "\(verb) given keywords")
        add(editsMapped, "\(verb) given Capture One's edits")
        func counted(_ n: Int) -> String { dryRun ? "will be" : n == 1 ? "was" : "were" }
        if variantsCreated > 0 {
            out.append("\(variantsCreated) Capture One variant\(variantsCreated == 1 ? "" : "s") \(counted(variantsCreated)) added as new variants")
        }
        let albums = albumsCreated + albumsUpdated
        if albums > 0 { out.append("\(albums) album\(albums == 1 ? "" : "s") \(counted(albums)) created or updated") }
        if out.isEmpty { out.append(conflicts.isEmpty ? "Everything is already up to date." : "Nothing new apart from the conflicts below.") }
        return out
    }

    /// The Darkslide tool each mapped C1 key lands in.
    static func tool(for key: String) -> String {
        switch key {
        case "Exposure", "Contrast", "Brightness", "Saturation": key
        case "HighlightRecoveryEx": "HDR highlights"
        case "ShadowRecovery": "HDR shadows"
        case "WhiteRecovery": "HDR whites"
        case "BlackRecovery": "HDR blacks"
        case "WhiteBalance", "WhiteBalanceTemperature", "WhiteBalanceTint": "White balance"
        case "Crop": "Crop"
        case "Rotation": "Rotation and straighten"
        case "Clarity": "Clarity"
        case "ClarityStructure": "Structure"
        case "DehazeAmount": "Dehaze"
        case "Vignetting": "Vignetting"
        case "NrAmount", "CnrAmount", "NoiseDetailsAmount": "Noise reduction"
        case "Shadow", "Highlight", "TargetShadow", "TargetHighlight": "Levels"
        case "ChromaticAberEnabled": "Chromatic aberration"
        case let k where k.hasPrefix("Usm"): "Sharpening"
        case let k where k.hasPrefix("GradationCurve"): "Curve"
        default: key
        }
    }

    /// Tools whose scale is translated approximately (see docs/C1_FORMAT.md).
    static let approximateTools: Set<String> = ["Clarity", "Structure", "Dehaze", "Vignetting", "Sharpening",
                                                "Noise reduction", "Curve", "Levels", "Crop"]

    /// Mapped edits per Darkslide tool, most photos first: "Exposure: 149 photos".
    /// A tool fed by several keys counts its most frequent key.
    public var toolLines: [String] {
        var counts: [String: Int] = [:]
        for c in mapped { counts[Self.tool(for: c.key), default: 0] = max(counts[Self.tool(for: c.key)] ?? 0, c.count) }
        return counts.sorted { $0.value != $1.value ? $0.value > $1.value : $0.key < $1.key }
            .map { "\($0.key): \(Self.photos($0.value))\(Self.approximateTools.contains($0.key) ? " (approximate)" : "")" }
    }

    /// Darkslide edits the import would replace (with the overwrite option), one line per photo.
    public var conflictLines: [String] {
        var fields: [String: [String]] = [:]
        var files: [String] = []
        for c in conflicts {
            if fields[c.file] == nil { files.append(c.file) }
            fields[c.file, default: []].append(Self.fieldName(c.field))
        }
        let shown = files.prefix(8).map { "\($0): \(fields[$0]!.joined(separator: ", "))" }
        return files.count > 8 ? shown + ["and \(Self.photos(files.count - 8)) more"] : shown
    }

    static func fieldName(_ f: String) -> String {
        switch f {
        case "color_tag": "colour tag"
        case "adjustments": "edits"
        default: f
        }
    }

    /// Settings that have no Darkslide translation: "Clarity: 149 photos (reason)".
    public var notMappedLines: [String] {
        unmapped.map { "\($0.key): \(Self.photos($0.count)) — \($0.reason)" }
    }

    /// Photos and albums left out, and why.
    public var leftOutLines: [String] {
        var out: [String] = []
        if inTrash > 0 { out.append("\(Self.photos(inTrash)) in Capture One's Trash (not listed in Darkslide)") }
        if outsideFolder > 0 { out.append("\(Self.photos(outsideFolder)) outside this folder (other folders the Capture One session references)") }
        if !notFound.isEmpty { out.append("\(Self.photos(notFound.count)) not found in Darkslide: \(Self.sample(notFound))") }
        if deletedInDarkslide > 0 { out.append("\(deletedInDarkslide) variant\(deletedInDarkslide == 1 ? "" : "s") deleted in Darkslide (not recreated)") }
        if readOnly > 0 { out.append("\(Self.photos(readOnly)) saved by a newer Darkslide (left alone)") }
        if keptDarkslide > 0 { out.append("\(keptDarkslide) Darkslide change\(keptDarkslide == 1 ? "" : "s") kept (unchanged in Capture One since the last import)") }
        out += skipped.map { "\u{201c}\($0.name)\u{201d}: \($0.reason)" }
        out += warnings
        return out
    }

    package static func photos(_ n: Int) -> String { n == 1 ? "1 photo" : "\(n) photos" }

    static func sample(_ names: [String], limit: Int = 3) -> String {
        let head = names.prefix(limit).joined(separator: ", ")
        return names.count > limit ? "\(head) and \(names.count - limit) more" : head
    }
}

/// What "Revert Capture One Import" did. Mirrors ds-c1's revert report.
public struct C1RevertSummary: Sendable, Equatable {
    public var restored = 0
    public var kept = 0
    public var variantsRemoved = 0
    public var albumsRemoved = 0
    public var albumsTrimmed = 0

    public var message: String {
        var parts = ["Capture One import reverted: \(C1ImportSummary.photos(restored)) restored"]
        if variantsRemoved > 0 { parts.append("\(variantsRemoved) imported variant\(variantsRemoved == 1 ? "" : "s") removed") }
        let albums = albumsRemoved + albumsTrimmed
        if albums > 0 { parts.append("\(albums) album\(albums == 1 ? "" : "s") undone") }
        if kept > 0 { parts.append("\(kept) later Darkslide change\(kept == 1 ? "" : "s") kept") }
        return parts.joined(separator: ", ")
    }
}

/// The store side of the import (the live store; demo mode has none).
public protocol CaptureOneImporting: Sendable {
    /// The folder is a Capture One session folder (a `*.cosessiondb` at its top).
    func isCaptureOneFolder(_ folder: URL) async -> Bool
    func importCaptureOne(_ session: SessionInfo, dryRun: Bool, overwriteEdits: Bool) async throws -> C1ImportSummary
    /// An import wrote something since the last revert.
    func canRevertCaptureOneImport(_ session: SessionInfo) async -> Bool
    /// Take back the imports since the last revert; `nil`: nothing to revert.
    func revertCaptureOneImport(_ session: SessionInfo) async throws -> C1RevertSummary?
}

/// The open session's Capture One status, detected off the main actor when it opens.
public struct CaptureOneStatus: Equatable, Sendable {
    public var session: UUID?
    public var isSession = false
    public var canRevert = false
}

/// The import sheet's state: dry run → preview → import (keeping or
/// overwriting Darkslide edits) → result.
@MainActor @Observable
public final class CaptureOneImportModel {
    public enum Phase: Equatable, Sendable {
        case previewing
        case preview(C1ImportSummary)
        case importing
        case done(C1ImportSummary)
        case reverting
        case reverted(C1RevertSummary)
        case failed(String)
    }

    public let session: SessionInfo
    /// Shown by the app after opening the folder (not chosen from the menu).
    public let offered: Bool
    public private(set) var phase: Phase = .previewing
    /// "Overwrite my Darkslide edits on conflicts" (off: conflicts keep Darkslide's values).
    public var overwriteEdits = false
    @ObservationIgnored let importer: any CaptureOneImporting
    /// Saves pending Darkslide edits, so the import compares against them.
    @ObservationIgnored let flush: @MainActor () async -> Void
    @ObservationIgnored let onImported: @MainActor () async -> Void

    init(session: SessionInfo, importer: any CaptureOneImporting, offered: Bool,
         flush: @escaping @MainActor () async -> Void = {},
         onImported: @escaping @MainActor () async -> Void) {
        self.session = session
        self.importer = importer
        self.offered = offered
        self.flush = flush
        self.onImported = onImported
    }

    /// Under the result: what Revert Import (and File ▸ Revert Capture One Import) takes back.
    public static let revertHint = "Revert Import takes back every Capture One import since the last revert, "
        + "here or with File \u{25B8} Revert Capture One Import. Changes you make in Darkslide afterwards are kept."

    public var title: String {
        offered ? "\u{201c}\(session.name)\u{201d} is a Capture One session" : "Import Capture One Edits"
    }

    /// The dry run's summary while previewing.
    public var preview: C1ImportSummary? {
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
        return false
    }

    /// Run the dry run (nothing is written).
    public func loadPreview() async {
        phase = .previewing
        await flush()
        do {
            phase = .preview(try await importer.importCaptureOne(session, dryRun: true, overwriteEdits: false))
        } catch is CancellationError {
            phase = .failed("Cancelled")
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// Import. Conflicts (Darkslide edits since the last import) are kept unless `overwriteEdits`.
    public func run() async {
        guard canImport else { return }
        phase = .importing
        await flush()
        do {
            let s = try await importer.importCaptureOne(session, dryRun: false, overwriteEdits: overwriteEdits)
            phase = .done(s)
            await onImported()
        } catch {
            phase = .failed(error.localizedDescription)
            await onImported() // part of it may have been written
        }
    }

    /// Take the import back (from the result).
    public func revert() async {
        guard canRevert else { return }
        phase = .reverting
        do {
            phase = .reverted(try await importer.revertCaptureOneImport(session) ?? C1RevertSummary())
        } catch {
            phase = .failed(error.localizedDescription)
        }
        await onImported()
    }
}

extension AppModel {
    var captureOneImporter: (any CaptureOneImporting)? { sessions as? any CaptureOneImporting }

    /// File ▸ Import Capture One Edits… is available: the open folder is a C1 session.
    public var canImportCaptureOne: Bool {
        captureOneImporter != nil && captureOne.isSession && captureOne.session == currentSession?.id
    }

    /// File ▸ Revert Capture One Import is available.
    public var canRevertCaptureOne: Bool { canImportCaptureOne && captureOne.canRevert }

    /// Detect whether the open session is a Capture One folder and has an import to revert
    /// (file and FFI work happens in the store, off the main actor).
    func refreshCaptureOne() async {
        guard let s = currentSession, let importer = captureOneImporter else {
            captureOne = CaptureOneStatus()
            return
        }
        let isC1 = await importer.isCaptureOneFolder(s.url)
        let revert = isC1 ? await importer.canRevertCaptureOneImport(s) : false
        guard currentSession?.id == s.id else { return }
        captureOne = CaptureOneStatus(session: s.id, isSession: isC1, canRevert: revert)
    }

    func captureOneImportModel(offered: Bool) -> CaptureOneImportModel? {
        guard canImportCaptureOne, let session = currentSession, let importer = captureOneImporter else { return nil }
        return CaptureOneImportModel(session: session, importer: importer, offered: offered,
                                     flush: { [weak self] in await self?.saver.flush() }) { [weak self] in
            await self?.saver.flush()
            await self?.refreshCurrentSession()
            await self?.refreshCaptureOne()
        }
    }

    /// File ▸ Import Capture One Edits…: show the sheet and run the dry run.
    public func showCaptureOneImport() {
        guard let m = captureOneImportModel(offered: false) else { return }
        sheet = .captureOneImport(m)
        Task { await m.loadPreview() }
    }

    /// File ▸ Revert Capture One Import: ask first (the confirmation calls `revertCaptureOneImport()`).
    public func requestRevertCaptureOne() {
        confirmingCaptureOneRevert = canRevertCaptureOne
    }

    /// Take back the imports since the last revert (after the confirmation).
    /// Fields changed in Darkslide after the import are kept.
    public func revertCaptureOneImport() async {
        guard canRevertCaptureOne, let session = currentSession, let importer = captureOneImporter else { return }
        await saver.flush()
        let outcome: Toast
        do {
            let r = try await importer.revertCaptureOneImport(session)
            outcome = Toast(r?.message ?? "No Capture One import to revert", symbol: "arrow.uturn.backward")
        } catch {
            outcome = Toast(error.localizedDescription, symbol: "exclamationmark.triangle", isError: true)
        }
        // After the reload, so the toast describes what the browser now shows (and isn't
        // timed out by a slow reload).
        await refreshCurrentSession()
        await refreshCaptureOne()
        show(outcome)
    }

    /// After opening `url`: if it's a Capture One session with something to import,
    /// offer it (once per folder per launch). The dry run happens before asking.
    func offerCaptureOneImport(after url: URL) async {
        let key = url.standardizedFileURL.resolvingSymlinksInPath().path
        guard !snapshotMode, !CaptureOneOffers.offered.contains(key),
              currentSession?.url.standardizedFileURL.resolvingSymlinksInPath().path == key else { return }
        await refreshCaptureOne()
        guard let m = captureOneImportModel(offered: true) else { return }
        CaptureOneOffers.offered.insert(key)
        await m.loadPreview()
        guard m.canImport, sheet == nil, currentSession?.id == m.session.id else { return }
        sheet = .captureOneImport(m)
    }
}

extension CaptureOneImportModel: Equatable {
    public nonisolated static func == (a: CaptureOneImportModel, b: CaptureOneImportModel) -> Bool { a === b }
}

/// Folders already offered an import this launch.
@MainActor enum CaptureOneOffers {
    static var offered: Set<String> = []
}
