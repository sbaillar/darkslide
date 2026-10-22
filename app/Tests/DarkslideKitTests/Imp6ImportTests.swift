import CoreGraphics
import DarkslideFFI
import Foundation
import ImageIO
import Testing
@testable import DarkslideKit

private func tempDir(_ name: String = "imp6") throws -> URL {
    let d = FileManager.default.temporaryDirectory.appendingPathComponent("\(name)-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

private let card = URL(fileURLWithPath: "/Volumes/A9CARD")

private func candidate(_ name: String, kind: ImportItemKind = .photo) -> ImportCandidate {
    ImportCandidate(url: card.appendingPathComponent("DCIM/\(name)"), size: 10, captureDate: Date(timeIntervalSince1970: 1),
                    alreadyImported: false, kind: kind)
}

/// Pure pieces: template translation, the eject rule, the report mapping, wording.
@Suite struct Imp6LogicTests {
    @Test func appTemplatesTranslateToDsTemplate() {
        #expect(ImportTemplates.dsTemplate("{original}", text: "x") == nil, "keeps the names")
        #expect(ImportTemplates.dsTemplate("  ", text: "x") == nil)
        #expect(ImportTemplates.dsTemplate("{camera}_{date}_{seq4}", text: "") == "{camera}_{date}_{seq:4}")
        #expect(ImportTemplates.dsTemplate("{date}_{time}_{seq}", text: "") == "{date}_{time}_{seq:3}")
        #expect(ImportTemplates.dsTemplate("{text}_{original}", text: " Fall {Hike} ") == "Fall {{Hike}}_{name}",
                "session text is literal (braces escaped)")
    }

    /// H1: eject is never automatic; "Eject Card" only when everything is safe,
    /// otherwise "Eject Anyway…" behind a confirmation.
    @Test func ejectGateSaysSafeOnlyWhenEverythingIsInTheLibrary() {
        var g = ImportEjectGate(verified: true, needsAcknowledgement: false, canEject: true)
        #expect(g.safe && g.offersEject && !g.requiresConfirmation && g.buttonTitle == "Eject Card")
        g.needsAcknowledgement = true
        #expect(!g.safe && g.offersEject && g.requiresConfirmation && g.buttonTitle == "Eject Anyway…",
                "files left on the card: ask first")
        g.acknowledged = true
        #expect(g.offersEject && !g.requiresConfirmation && !g.safe, "confirmed: may eject, still not safe")
        g.ejected = true
        #expect(!g.offersEject)
        let unverified = ImportEjectGate(verified: false, needsAcknowledgement: false, canEject: true)
        #expect(!unverified.safe && unverified.requiresConfirmation, "nothing unverified is ever safe")
        let nas = ImportEjectGate(verified: true, needsAcknowledgement: false, canEject: false)
        #expect(!nas.offersEject)
    }

    @Test func reportsCountThePlansItemsAndCarryTheIssues() {
        let a = candidate("A.ARW"), b = candidate("B.ARW"), clip = candidate("C.MP4", kind: .video)
        let plan = ImportPlan(candidates: [a, b, clip], destination: .newSession(name: "S"), renameTemplate: "",
                              backupFolder: nil, volume: card)
        let result = ImportResult(
            sessionDir: "/lib/S", sessionName: "S", volumeId: "CARD-1", filesPlanned: 4,
            copied: [ImportedRecord(src: a.url.path, dest: "/lib/S/Capture/A.ARW", backup: nil, size: 1, kind: .raw,
                                    resumed: false),
                     ImportedRecord(src: card.appendingPathComponent("DCIM/A.JPG").path, dest: "/lib/S/Capture/A.JPG",
                                    backup: nil, size: 1, kind: .jpeg, resumed: false),
                     ImportedRecord(src: clip.url.path, dest: "/lib/S/Capture/C.MP4", backup: nil, size: 1, kind: .video,
                                    resumed: true)],
            skippedDuplicates: [b.url.path], failed: [], backupFailed: [], quarantined: [], cancelled: false,
            sourceLost: nil, durabilityError: nil, scanErrors: [],
            skippedUnknown: [CardIssue(path: "/Volumes/A9CARD/DCIM/notes.txt", reason: "unknown type")],
            bytesCopied: 3, elapsedMs: 5, isComplete: true, ejectNeedsConfirmation: true, registered: 2,
            registerErrors: [], cardOnly: [], safeToRemove: false, damagedLibraryCopies: [], indexError: nil)
        let r = RustImportService.report(result, plan: plan)
        #expect(r.filesTotal == 3 && r.filesCopied == 3 && r.filesVerified == 3, "sidecars don't count; duplicates do")
        #expect(r.allVerified(for: plan) && r.ejectNeedsConfirmation && r.skippedDuplicates == 1)
        #expect(r.skippedUnknown.map(\.url.lastPathComponent) == ["notes.txt"])
        #expect(r.sessionURL?.path == "/lib/S" && r.sessionName == "S" && r.volumeID == "CARD-1" && !r.safeToRemove)
        var partial = result
        partial.isComplete = false
        partial.failed = [CardIssue(path: b.url.path, reason: "read error")]
        #expect(!RustImportService.report(partial, plan: plan).allVerified(for: plan))
        var cancelled = result
        cancelled.cancelled = true
        #expect(!RustImportService.report(cancelled, plan: plan).allVerified, "a cancelled run is never verified")
    }

    @Test func wordingForIssuesAndResume() {
        #expect(ImportWording.issuesTitle(scanErrors: 0, skipped: 0).isEmpty)
        #expect(ImportWording.issuesTitle(scanErrors: 0, skipped: 1) == "1 file on the card was not imported")
        #expect(ImportWording.issuesTitle(scanErrors: 2, skipped: 0) == "2 parts of the card couldn’t be read")
        #expect(ImportWording.issuesTitle(scanErrors: 1, skipped: 3) == "1 unreadable, 3 not imported")
        #expect(ImportWording.keepCardNote(scanErrors: 1).contains("isn’t complete"))
        #expect(ImportWording.keepCardNote(scanErrors: 0).contains("only on the card"))
        #expect(ImportWording.stopped(done: 2, total: 5).contains("2 of 5"))
        #expect(ImportWording.resumePrompt(sessionName: "Trip", done: 3, pending: 1).contains("“Trip”"))
        let p = PendingImportInfo(sessionURL: URL(fileURLWithPath: "/lib/Trip"), done: 1, pending: 2, failed: 0)
        #expect(p.sessionName == "Trip" && p.id == p.sessionURL)
    }

    @MainActor @Test func toolbarSaysImportWhenCopiesAreVerified() {
        #expect(ToolbarHelp.importTitle(mode: .live, verifies: true) == "Import")
        #expect(ToolbarHelp.importButton(mode: .live, verifies: true).contains("verified byte for byte"))
        #expect(ToolbarHelp.importTitle(mode: .live) == "Import (coming soon)", "a live build without ds-import")
        let m = AppModel.make(mode: .live, settings: makeSettings(), defaults: UserDefaults(suiteName: testSuiteName())!)
        #expect(ToolbarControl.importCard.control(m).label == "Import")
        #expect(m.importer is any ImportVerifying && m.importer is any ImportResuming)
    }

    @Test func servicesWithoutARichScanListOnlyCandidates() async throws {
        let s = try await MockImportService(inventedCount: 2).scanCard(volume: card)
        #expect(s.candidates.count == 2 && s.scanErrors.isEmpty && s.skippedUnknown.isEmpty)
        #expect(ImportCandidate(url: card, size: 4, captureDate: Date(), alreadyImported: false).totalBytes == 4)
    }

    @Test func newSessionFoldersNeverCollide() throws {
        let root = try tempDir()
        defer { try? FileManager.default.removeItem(at: root) }
        #expect(RustImportService.newSessionDir(in: root, name: "Trip").lastPathComponent == "Trip")
        try FileManager.default.createDirectory(at: root.appendingPathComponent("Trip"), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: root.appendingPathComponent("Trip 2"), withIntermediateDirectories: true)
        #expect(RustImportService.newSessionDir(in: root, name: "Trip").lastPathComponent == "Trip 3")
        #expect(!RustImportService.isImportSession(root))
        let utc = TimeZone(identifier: "UTC")!
        #expect(RustImportService.wallClockMs(Date(timeIntervalSince1970: 10), timeZone: utc) == 10_000)
        let plus2 = TimeZone(secondsFromGMT: 7200)!
        #expect(RustImportService.wallClockMs(Date(timeIntervalSince1970: 10), timeZone: plus2) == 7_210_000)
    }
}

/// The import sheet's model against a verifying service's report (C1 + ds-import rules).
@MainActor @Suite struct Imp6EjectGatingTests {
    func model(_ service: ReportingImportService, ejector: FakeEjector) -> (ImportModel, () -> [SessionInfo]) {
        var imported: [SessionInfo] = []
        let m = ImportModel(volume: card, importer: service, sessions: MockSessionStore(counts: [2]), existingSessions: [],
                            nameTemplate: "{date} {text}", renameTemplate: "{original}", ejector: ejector,
                            inspector: removableInspector) { imported.append($0) }
        return (m, { imported })
    }

    static func service(skipped: [ImportIssue] = [], scanErrors: [ImportIssue] = [], complete: Bool = true,
                        cancelled: Bool = false) -> ReportingImportService {
        let cands = [candidate("A.ARW"), candidate("C.MP4", kind: .video)]
        return ReportingImportService(scan: ImportScan(candidates: cands, scanErrors: scanErrors, skippedUnknown: skipped)) { plan in
            let n = plan.candidates.count
            return ImportReport(filesTotal: n, filesCopied: complete ? n : n - 1, filesVerified: complete ? n : n - 1,
                                bytesCopied: 10, isComplete: complete && scanErrors.isEmpty,
                                ejectNeedsConfirmation: !(complete && scanErrors.isEmpty) || !skipped.isEmpty,
                                scanErrors: scanErrors, skippedUnknown: skipped, cancelled: cancelled)
        }
    }

    @Test func filesOnlyOnTheCardNeedAConfirmedEjectAnyway() async {
        let ej = FakeEjector()
        let notes = ImportIssue(url: card.appendingPathComponent("DCIM/notes.txt"), reason: "unknown type")
        let (m, imported) = model(Self.service(skipped: [notes]), ejector: ej)
        await m.scan()
        #expect(m.hasCardIssues && m.skippedUnknown == [notes] && m.videoCount == 1)
        m.start()
        await m.wait()
        guard case .finished(let s) = m.phase else { #expect(Bool(false), "\(m.phase)"); return }
        #expect(s.verified && s.complete && s.needsAcknowledgement && !s.ejected && !s.safeToRemove)
        #expect(ej.ejected.isEmpty, "never ejected automatically")
        #expect(s.stayOnCard == [notes] && imported().count == 1, "exactly what stays is listed")
        #expect(m.ejectGate?.buttonTitle == "Eject Anyway…" && m.ejectGate?.requiresConfirmation == true)
        await m.ejectNow()
        #expect(ej.ejected.isEmpty && m.confirmingEject, "the first press asks")
        #expect(ImportWording.ejectAnywayMessage(s).contains("notes.txt"))
        await m.confirmEject()
        #expect(ej.ejected == [card] && !m.confirmingEject)
        guard case .finished(let after) = m.phase else { return }
        #expect(after.ejected && m.ejectGate?.offersEject == false)
        #expect(ImportWording.cardLine(after)?.contains("Keep it") == true, "never “safe” when something stays")
    }

    /// Scan errors: never "safe"; "Eject Anyway" after a confirmation. Wording
    /// states per-file facts (the copied file *was* verified).
    @Test func unreadableCardPartsAllowEjectAnywayAfterConfirmation() async {
        let ej = FakeEjector()
        let bad = ImportIssue(url: card.appendingPathComponent("DCIM/101MSDCF"), reason: "unreadable directory")
        let (m, _) = model(Self.service(scanErrors: [bad]), ejector: ej)
        await m.scan()
        m.start()
        await m.wait()
        guard case .finished(let s) = m.phase else { #expect(Bool(false), "\(m.phase)"); return }
        #expect(!s.verified && !s.complete && s.scanErrors == [bad] && !s.safeToRemove)
        #expect(s.stayOnCard.contains(bad))
        #expect(ImportWording.doneDetail(s).contains("copied and verified"), "facts, not “nothing was verified”")
        #expect(ImportWording.doneDetail(s).contains("1 part of the card unreadable"))
        #expect(ImportWording.cardLine(s)?.contains("Keep this card") == true)
        #expect(m.ejectGate?.offersEject == true && m.ejectGate?.requiresConfirmation == true)
        await m.ejectNow()
        #expect(ej.ejected.isEmpty)
        await m.confirmEject()
        #expect(ej.ejected == [card])
    }

    @Test func aCleanVerifiedImportIsSafeAndEjectsOnlyWhenAsked() async {
        let ej = FakeEjector()
        let (m, _) = model(Self.service(), ejector: ej)
        await m.scan()
        m.start()
        await m.wait()
        guard case .finished(let s) = m.phase else { #expect(Bool(false), "\(m.phase)"); return }
        #expect(s.verified && s.safeToRemove && !s.ejected && ej.ejected.isEmpty, "never automatic")
        #expect(ImportWording.cardLine(s)?.contains("safe to remove") == true)
        #expect(m.ejectGate?.buttonTitle == "Eject Card" && m.ejectGate?.requiresConfirmation == false)
        await m.ejectNow()
        #expect(ej.ejected == [card] && !m.confirmingEject)
        guard case .finished(let after) = m.phase else { return }
        #expect(ImportWording.cardLine(after)?.contains("safe to remove") == true)
    }

    @Test func discardingAPendingImportClearsThePrompt() async throws {
        let lib = try tempDir("imp6-discard")
        defer { try? FileManager.default.removeItem(at: lib) }
        let settings = makeSettings()
        let app = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                           renderer: FakeRenderer(), importer: RustImportService(libraryRoot: lib, globalIndex: nil),
                           archive: UnavailableArchiveService(target: ""), ai: MockAIService(), settings: settings)
        app.watchCards = false
        await app.checkPendingImports()
        #expect(app.pendingImport == nil, "no unfinished imports")
        let p = PendingImportInfo(sessionURL: lib.appendingPathComponent("Trip"), done: 1, pending: 1, failed: 0)
        app.pendingImport = p
        await app.discardPendingImport(p)
        #expect(app.pendingImport == nil && app.toast?.message.contains("Discarded") == true)
        // Services without a journal never prompt, and discarding is a no-op.
        let mock = makeModel()
        await mock.checkPendingImports()
        #expect(mock.pendingImport == nil)
        mock.pendingImport = p
        await mock.discardPendingImport(p)
        #expect(mock.pendingImport == nil && mock.toast == nil)
    }

    @Test func aStoppedImportSaysHowToResume() async {
        let (m, imported) = model(Self.service(complete: false, cancelled: true), ejector: FakeEjector())
        await m.scan()
        m.start()
        await m.wait()
        guard case .failed(let msg) = m.phase else { #expect(Bool(false), "\(m.phase)"); return }
        #expect(msg.contains("import again to continue") && imported().isEmpty,
                "no session is created for a stopped run the service placed nowhere")
        #expect(m.lastReport?.cancelled == true)
    }
}

/// Clips: never edited, copied onto, or AI-culled; Space opens Quick Look.
@MainActor @Suite struct Imp6VideoTests {
    @Test func quickLookToggles() {
        let q = VideoQuickLook()
        var shown: [URL?] = []
        q.present = { shown.append($0) }
        let clip = URL(fileURLWithPath: "/tmp/C0001.MP4")
        q.toggle(clip)
        q.toggle(clip)
        #expect(shown == [clip, nil] && q.url == nil)
        #expect(q.numberOfPreviewItems(in: nil) == 0)
        q.toggle(clip)
        #expect(q.numberOfPreviewItems(in: nil) == 1)
        #expect((q.previewPanel(nil, previewItemAt: 0) as? NSURL)?.path == clip.path)
    }

    @Test func clipsAreNotEditableAndSpaceQuickLooksThem() async {
        let m = makeModel(store: VideoStore())
        await m.start()
        #expect(await waitUntil { m.photos.count == 2 })
        let clip = m.photos.first { $0.isVideo }!
        let photo = m.photos.first { !$0.isVideo }!
        #expect(Photo.isVideo(URL(fileURLWithPath: "x.mov")) && !Photo.isVideo(URL(fileURLWithPath: "x.ARW")))
        m.updateEdits([clip.id, photo.id], label: "t") { _, e in e.exposure.ev = 1 }
        #expect(m.edit(for: clip.id).exposure.ev == 0 && m.edit(for: photo.id).exposure.ev == 1)
        m.copyAdjustments(from: clip.id)
        #expect(m.clipboard == nil, "nothing to copy from a clip")
        let shared = VideoQuickLook.shared
        let saved = shared.present
        var shown: [URL?] = []
        shared.present = { shown.append($0) }
        defer { shared.present = saved }
        m.focus(clip.id, extend: false)
        let before = m.browserMode
        m.perform(.zoomToggleAtCursor)
        #expect(shown == [clip.url] && m.browserMode == before, "Space previews the clip")
        m.perform(.zoomToggleAtCursor)
        #expect(shown == [clip.url, nil])
        await m.autoAdjust()
        #expect(m.edit(for: clip.id).isIdentity)
    }

    @Test func posterFramesForClipsAndImagesForJPEGs() async throws {
        let dir = try tempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let clip = dir.appendingPathComponent("C0001.MP4")
        try await makeMP4(clip)
        let poster = try await VideoPoster.image(for: clip, maxPixelSize: 32)
        #expect(max(poster.width, poster.height) <= 32)
        #expect(await NonRawThumbnail.image(for: clip, maxPixelSize: 64) != nil)
        let viaProvider = try await RustPreviewProvider().preview(for: clip, maxPixelSize: 64)
        #expect(viaProvider.width > 0, "the grid and viewer show the poster frame")
        #expect(await NonRawThumbnail.image(for: dir.appendingPathComponent("x.ARW"), maxPixelSize: 64) == nil)
        // A JPEG-only card item.
        let jpg = dir.appendingPathComponent("a.JPG")
        let ctx = CGContext(data: nil, width: 40, height: 20, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        let dest = CGImageDestinationCreateWithURL(jpg as CFURL, "public.jpeg" as CFString, 1, nil)!
        CGImageDestinationAddImage(dest, ctx.makeImage()!, nil)
        #expect(CGImageDestinationFinalize(dest))
        let t = try #require(await NonRawThumbnail.image(for: jpg, maxPixelSize: 16))
        #expect(t.width == 16)
        let thumbs = ImportThumbnails(previews: RustPreviewProvider(), maxPixelSize: 32)
        #expect(await thumbs.thumbnail(for: clip) != nil && thumbs.cached(clip) != nil)
    }
}
