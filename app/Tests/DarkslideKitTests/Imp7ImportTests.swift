import CoreGraphics
import DarkslideFFI
import Foundation
import Testing
@testable import DarkslideKit

// Integration A review fixes on the import side: C1, H1–H4, M1–M5, L1–L5.

private let card = URL(fileURLWithPath: "/Volumes/IMP7CARD")

private func tempDir(_ name: String) throws -> URL {
    let d = FileManager.default.temporaryDirectory.appendingPathComponent("\(name)-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

/// A checked detach (`TestCardImage.detach`).
func detachChecked(_ c: TestCardImage) async -> Bool { await c.detach() }

/// Run `body`, then detach every image (checked) whatever happened: a
/// deferred `Task` may never run before the test process exits (L6).
@MainActor
func withImages<T>(_ images: [TestCardImage], _ body: () async throws -> T) async throws -> T {
    do {
        let r = try await body()
        for i in images { #expect(await i.detach(), "detached: \(i.device)") }
        return r
    } catch {
        for i in images { _ = await i.detach() }
        throw error
    }
}

/// `n` sample ARWs, starting at sample `from`, as `DSC00100…` under DCIM.
func fillRaws(_ root: URL, from: Int, count n: Int) async throws {
    let samples = Array(sampleARWs().dropFirst(from).prefix(n))
    let dcim = root.appendingPathComponent("DCIM/100MSDCF")
    try FileManager.default.createDirectory(at: dcim, withIntermediateDirectories: true)
    try await Task.detached {
        for (i, s) in samples.enumerated() {
            try FileManager.default.copyItem(at: s, to: dcim.appendingPathComponent(String(format: "DSC%05d.ARW", 100 + i)))
        }
    }.value
}

@MainActor
private func sheet(_ volume: URL, service: any ImportService, store: any SessionStore, existing: [SessionInfo] = [],
                   ejector: FakeEjector = FakeEjector(), registry: ImportRegistry = .shared,
                   imported: @escaping @MainActor (SessionInfo) async -> Void = { _ in }) -> ImportModel {
    let traits = VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "IMP7")
    return ImportModel(volume: volume, importer: service, sessions: store, existingSessions: existing,
                       nameTemplate: "{text}", renameTemplate: "{original}", ejector: ejector,
                       inspector: StaticVolumeInspector([volume.standardizedFileURL.path: traits]), registry: registry,
                       onImported: imported)
}

// MARK: - Live (real FFI, real exFAT card images)

@MainActor @Suite(.serialized) struct Imp7LiveTests {
    /// C1 (the reviewer's swap): card A and card B share the volume name and the
    /// file names but not the content. B at A's mount point is scanned again —
    /// a plan made on A is refused, never run on B — and B imports completely.
    @Test func anotherCardAtTheSameMountPointIsScannedAgainNeverMixed() async throws {
        guard sampleARWs().count >= 7 else { return }
        let root = try tempDir("imp7-swap")
        defer { try? FileManager.default.removeItem(at: root) }
        let aDir = root.appendingPathComponent("a"), bDir = root.appendingPathComponent("b")
        try FileManager.default.createDirectory(at: aDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: bDir, withIntermediateDirectories: true)
        guard let a0 = await TestCardImage.make(in: aDir, name: "SAMECARD") else { return }
        try await fillRaws(a0.mount, from: 0, count: 3)
        guard let b0 = await TestCardImage.make(in: bDir, name: "SAMECARD") else {
            _ = await detachChecked(a0)
            return
        }
        try await fillRaws(b0.mount, from: 3, count: 4)
        #expect(await detachChecked(b0))
        guard let a = await a0.remount() else { return }
        let lib = root.appendingPathComponent("Library")
        let store = RustSessionStore(recents: RecentFolders(defaults: testDefaults()),
                                     storeBase: root.appendingPathComponent("stores"))
        let service = RustImportService(libraryRoot: lib, globalIndex: root.appendingPathComponent("g.db"))
        let registry = ImportRegistry()
        var opened: SessionInfo?
        let m = sheet(a.mount, service: service, store: store, registry: registry) { opened = $0 }
        await m.scan()
        let idA = try #require(m.volumeID)
        #expect(m.candidates.count == 3)
        // Swap: A out (checked), B in at the same mount point.
        #expect(await detachChecked(a))
        guard let b = await TestCardImage.attach(b0.image, at: a.mount) else { return }
        try await withImages([b]) { try await swapImport(m, b: b, idA: idA, lib: lib, store: store) { opened } }
    }

    private func swapImport(_ m: ImportModel, b: TestCardImage, idA: String, lib: URL, store: RustSessionStore,
                            opened: () -> SessionInfo?) async throws {
        let before = await cardListing(b.mount)
        // The stale sheet presses Import: refused, and the card in the reader is shown.
        m.sessionText = "Swap"
        m.start()
        await m.wait()
        guard case .failed(let msg) = m.phase else { Issue.record("\(m.phase)"); return }
        #expect(msg.contains("different card"), "\(msg)")
        #expect(m.volumeID != nil && m.volumeID != idA, "scanned again: another card")
        #expect(m.candidates.count == 4, "card B's four photos, not A's list")
        #expect(!FileManager.default.fileExists(atPath: lib.appendingPathComponent("Swap").path), "nothing created")
        // Try Again: a fresh scan, then B imports completely and is safe to remove.
        m.retry()
        await m.wait()
        #expect(m.phase == .ready)
        m.start()
        await m.wait()
        guard case .finished(let s) = m.phase else { Issue.record("\(m.phase)"); return }
        #expect(s.files == 4 && s.verified && s.safeToRemove && s.stayOnCard.isEmpty, "\(s)")
        let session = try #require(opened())
        let photos = try await store.photos(in: session)
        #expect(photos.count == 4)
        let bytes = Set(try photos.map { try Data(contentsOf: #require($0.url)) })
        let bFiles = try FileManager.default.contentsOfDirectory(atPath: b.mount.appendingPathComponent("DCIM/100MSDCF").path)
        let bBytes = Set(try bFiles.map { try Data(contentsOf: b.mount.appendingPathComponent("DCIM/100MSDCF/\($0)")) })
        #expect(bytes == bBytes, "only card B's content")
        #expect(await cardListing(b.mount) == before, "card B untouched")
    }

    /// H2: Stop, then Import again (same sheet, or a new sheet after relaunch)
    /// continues into the same session; no "Stop 2".
    @Test func stopThenImportAgainContinuesIntoTheSameSession() async throws {
        try await Imp6LiveImportTests.withEnv { env in
            let slow = env.service(throttleMs: 40)
            let m = sheet(env.card.mount, service: slow, store: env.store)
            await m.scan()
            m.sessionText = "Stop"
            m.start()
            #expect(await waitUntil(timeout: .seconds(30)) { (m.progress?.bytesDone ?? 0) > 0 })
            m.cancel()
            #expect(m.isStopping)
            await m.wait()
            guard case .failed(let msg) = m.phase else { Issue.record("\(m.phase)"); return }
            #expect(msg.contains("Import stopped after") && msg.contains("import again to continue"), "\(msg)")
            guard case .existing(let id) = m.destination, let session = m.existingSessions.first(where: { $0.id == id })
            else { Issue.record("a retry must target the stopped import's session"); return }
            #expect(session.name == "Stop")
            // A new sheet with a new service (a relaunch) finds this card's unfinished import.
            let fresh = env.service()
            let pending = await fresh.pendingImports()
            let volumeID = try #require(m.volumeID)
            #expect(pending.contains { $0.volumes.contains(volumeID) }, "\(pending)")
            var done: SessionInfo?
            let m2 = sheet(env.card.mount, service: fresh, store: env.store) { done = $0 }
            await m2.scan()
            #expect(m2.resuming?.sessionName == "Stop")
            guard case .existing(let id2) = m2.destination else { Issue.record("\(m2.destination)"); return }
            #expect(m2.existingSessions.first { $0.id == id2 }?.url.standardizedFileURL.path
                    == session.url.standardizedFileURL.path)
            m2.start()
            await m2.wait()
            guard case .finished(let s) = m2.phase else { Issue.record("\(m2.phase)"); return }
            #expect(s.verified && s.sessionName == "Stop")
            let target = try #require(done)
            #expect(try await env.store.photos(in: target).count == 4, "each item once")
            let dirs = try FileManager.default.contentsOfDirectory(atPath: env.library.path).filter { !$0.hasPrefix(".") }
            #expect(dirs == ["Stop"], "no second session: \(dirs)")
            #expect(await fresh.pendingImports().isEmpty)
        }
    }

    /// H3: a file the session can't add is not imported (not "safe", listed as
    /// only on the card, the journal stays); opening the session adds it.
    @Test func aFileTheSessionCantAddIsNotImportedAndOpeningRecoversIt() async throws {
        try await Imp6LiveImportTests.withEnv { env in
            let service = env.service()
            var opened: SessionInfo?
            let first = sheet(env.card.mount, service: service, store: env.store) { opened = $0 }
            await first.scan()
            first.sessionText = "Reg"
            for c in first.candidates.dropFirst() { first.toggle(c) }
            first.start()
            await first.wait()
            let session = try #require(opened)
            let settings = session.url.appendingPathComponent("Capture/.ds/settings")
            #expect(FileManager.default.fileExists(atPath: settings.path))
            try FileManager.default.setAttributes([.posixPermissions: 0o555], ofItemAtPath: settings.path)
            let second = sheet(env.card.mount, service: service, store: env.store, existing: [session])
            await second.scan()
            second.destination = .existing(session.id)
            second.start()
            await second.wait()
            try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: settings.path)
            guard case .finished(let s) = second.phase else { Issue.record("\(second.phase)"); return }
            #expect(s.registerErrors.count == 3 && !s.verified && !s.safeToRemove, "\(s)")
            #expect(ImportWording.doneDetail(s).contains("copied but not added to the session"))
            #expect(s.stayOnCard.filter { $0.reason.contains("not added") }.count == 3, "\(s.stayOnCard)")
            #expect(second.ejectGate?.buttonTitle == "Eject Anyway…")
            #expect(await service.pendingImports(including: [session.url]).count == 1, "journal kept")
            // Opening the session (a fresh store) adds the copied files.
            let reopened = RustSessionStore(recents: env.recents, storeBase: env.root.appendingPathComponent("stores2"))
            let info = try await reopened.add(folder: session.url)
            #expect(try await reopened.photos(in: info).count == 4)
        }
    }

    /// M1: the sessions folder on a volume too small for the card: the sheet
    /// says so and can't start; the service refuses before creating anything.
    @Test func aDestinationThatCantHoldTheImportCreatesNothing() async throws {
        try await Imp6LiveImportTests.withEnv { env in
            let dmg = env.root.appendingPathComponent("small.dmg")
            guard await TestCardImage.run(["/usr/bin/hdiutil", "create", "-quiet", "-size", "40m", "-fs", "APFS",
                                           "-volname", "IMP7LIB", dmg.path]) != nil,
                  let small = await TestCardImage.attach(dmg, at: env.root.appendingPathComponent("small-mnt"))
            else { return }
            try await withImages([small]) { try await tooSmall(env, small) }
        }
    }

    private func tooSmall(_ env: Imp6LiveImportTests.Env, _ small: TestCardImage) async throws {
        do {
            let lib = small.mount.appendingPathComponent("Library")
            let service = RustImportService(libraryRoot: lib, globalIndex: env.index)
            let m = sheet(env.card.mount, service: service, store: env.store)
            await m.scan()
            m.sessionText = "Full"
            await m.settleAux()
            #expect(m.lacksSpace && !m.canStart, "free \(m.freeBytes ?? -1) for \(m.neededBytes)")
            // The service's own preflight (the real guard).
            var failure: Error?
            do {
                for try await _ in service.run(m.makePlan()) {}
            } catch {
                failure = error
            }
            guard case .notEnoughSpace(let why) = failure as? ImportRunError else {
                Issue.record("expected notEnoughSpace, got \(String(describing: failure))")
                return
            }
            #expect(why.contains("Nothing was copied or created"))
            #expect(!FileManager.default.fileExists(atPath: lib.appendingPathComponent("Full").path), "no empty session")
        }
    }

    /// L4: the preview uses each file's real camera and capture time.
    @Test func renamePreviewUsesTheFilesRealCamera() async throws {
        guard let sample = sampleARWs().first else { return }
        let meta = await RustImportService(libraryRoot: FileManager.default.temporaryDirectory, globalIndex: nil)
            .fileMeta(sample)
        #expect(meta.camera == "ILCE-9" && meta.date != nil)
    }
}

// MARK: - Logic

private final class CountingScans: ImportService, ImportVerifying, ImportFileMetaReading, ImportSpaceChecking,
    @unchecked Sendable {
    let lock = NSLock()
    var scans = 0
    var volumeID = "CARD-A"
    var free: Int64?
    func scan(volume: URL) async throws -> [ImportCandidate] { try await scanCard(volume: volume).candidates }
    func scanCard(volume: URL) async throws -> ImportScan {
        let id = lock.withLock { () -> String in
            scans += 1
            return volumeID
        }
        let c = ImportCandidate(url: volume.appendingPathComponent("DCIM/A.ARW"), size: 100, captureDate: Date(timeIntervalSince1970: 0),
                                alreadyImported: false)
        let v = ImportCandidate(url: volume.appendingPathComponent("DCIM/C.MP4"), size: 50, captureDate: Date(timeIntervalSince1970: 1),
                                alreadyImported: false, kind: .video,
                                damagedLibraryCopy: URL(fileURLWithPath: "/lib/C.MP4"))
        return ImportScan(candidates: [c, v], volumeID: id)
    }
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> {
        AsyncThrowingStream { c in
            let t = Task {
                try? await Task.sleep(for: .seconds(30))
                c.finish()
            }
            c.onTermination = { _ in t.cancel() }
        }
    }
    func report(for plan: ImportPlan) async -> ImportReport? {
        ImportReport(filesTotal: plan.candidates.count, filesCopied: 0, filesVerified: 0, bytesCopied: 0,
                     cancelled: true, sessionURL: nil)
    }
    func fileMeta(_ url: URL) async -> ImportFileMeta {
        ImportFileMeta(camera: "ILCE-1", date: Date(timeIntervalSince1970: 86_400 * 400))
    }
    func freeBytes(for destination: ImportDestination) async -> Int64? { lock.withLock { free } }
}

/// Journals in two sessions (M5), one resumed by `CARD-A`.
private struct TwoPending: ImportService, ImportResuming {
    let seen = Imp7Box<[URL]>([])
    func scan(volume: URL) async throws -> [ImportCandidate] { [] }
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> { AsyncThrowingStream { $0.finish() } }
    func pendingImports(including sessions: [URL]) async -> [PendingImportInfo] {
        seen.value = sessions
        return [PendingImportInfo(sessionURL: URL(fileURLWithPath: "/lib/One"), done: 1, pending: 2, failed: 0, volumes: ["CARD-A"]),
                PendingImportInfo(sessionURL: URL(fileURLWithPath: "/lib/Two"), done: 0, pending: 1, failed: 0, volumes: ["CARD-B"])]
    }
    func discard(_ pending: PendingImportInfo) async throws {}
}

final class Imp7Box<T>: @unchecked Sendable {
    private let lock = NSLock()
    private var v: T
    init(_ v: T) { self.v = v }
    var value: T {
        get { lock.withLock { v } }
        set { lock.withLock { v = newValue } }
    }
}

@MainActor @Suite struct Imp7LogicTests {
    /// C1: a card mounted where the sheet's card was is scanned again (another
    /// card may be there); thumbnails are keyed by card.
    @Test func reinsertAtTheSameMountPointRescans() async throws {
        let service = CountingScans()
        let registry = ImportRegistry()
        let m = sheet(card, service: service, store: MockSessionStore(counts: [1]), registry: registry)
        await m.scan()
        #expect(m.volumeID == "CARD-A" && service.scans == 1)
        service.volumeID = "CARD-B"
        await registry.volumeDidAppear(card)
        #expect(m.volumeID == "CARD-B" && service.scans == 2, "the new card's identity")
        await registry.volumeDidAppear(URL(fileURLWithPath: "/Volumes/OTHER"))
        #expect(service.scans == 2, "someone else's card")
        // Coordinator: an import sheet is open for this card → the sheet rescans.
        let app = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                           renderer: FakeRenderer(), importer: service, archive: UnavailableArchiveService(target: ""),
                           ai: MockAIService(), settings: makeSettings(), mode: .live)
        app.sheet = .importCard(card)
        let c = CardCoordinator(app: app, store: MemoryOfferedStore(),
                                inspector: StaticVolumeInspector([card.path: VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true)]))
        ImportRegistry.shared.register(m)
        app.settings.autoOpenImport = true
        c.cardArrived(card, atLaunch: false)
        await c.settle()
        #expect(service.scans == 3)
        // Thumbnails of one card never show for another at the same path.
        let thumbs = ImportThumbnails(previews: FakePreviews())
        let url = card.appendingPathComponent("DCIM/A.ARW")
        #expect(await thumbs.thumbnail(for: url, card: "CARD-A") != nil)
        #expect(thumbs.cached(url, card: "CARD-A") != nil && thumbs.cached(url, card: "CARD-B") == nil)
    }

    /// L3 + L4 + M1: damaged copies are announced, the preview reads the real
    /// camera, and a selection that can't fit can't start.
    @Test func sheetAnnouncesDamageReadsCamerasAndChecksSpace() async {
        let service = CountingScans()
        let m = sheet(card, service: service, store: MockSessionStore(counts: [1]))
        m.renameTemplate = "{camera}_{original}"
        await m.scan()
        await m.settleAux()
        #expect(m.damagedCount == 1)
        #expect(m.renamePreview(count: 1) == ["ILCE-1_A.ARW"], "\(m.renamePreview(count: 1))")
        #expect(!m.lacksSpace && m.canStart)
        service.free = 120 // 150 bytes selected
        m.refreshAux()
        await m.settleAux()
        #expect(m.lacksSpace && !m.canStart)
        #expect(ImportWording.notEnoughSpace(needed: 150, free: 120).contains("Not enough space"))
        #expect(ImportWording.readyLine(bytes: 1, destination: "S", verifies: true, free: 1_000).contains("free"))
    }

    /// M3: quitting during an import asks, then stops it (bounded); the stopped
    /// import keeps its journal (the service reports it cancelled).
    @Test func quittingDuringAnImportAsksAndStopsIt() async throws {
        let registry = ImportRegistry()
        let service = CountingScans()
        let m = sheet(card, service: service, store: MockSessionStore(counts: [1]), registry: registry)
        await m.scan()
        m.sessionText = "Q"
        m.start()
        let app = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                           renderer: FakeRenderer(), importer: service, archive: UnavailableArchiveService(target: ""),
                           ai: MockAIService(), settings: makeSettings())
        #expect(await app.quitDecision(imports: registry) == .importRunning(count: 1))
        let alert = try #require(TerminationFlow.alert(for: .importRunning(count: 1)))
        #expect(alert.title == ImportWording.quitImportTitle && alert.choices == [.cancel, .stopImportAndQuit])
        #expect(TerminationFlow.reply(for: .stopImportAndQuit) == .terminate)
        #expect(TerminationFlow.Choice.stopImportAndQuit.title == "Stop Import and Quit")
        #expect(await app.stopImportsForQuit(timeout: .seconds(10), imports: registry))
        guard case .failed(let msg) = m.phase else { Issue.record("\(m.phase)"); return }
        #expect(msg.contains("import again to continue"))
        #expect(await app.quitDecision(imports: registry) == .ok)
        #expect(await ImportRegistry().stopAll(), "nothing running")
    }

    /// M5: every unfinished import is offered, one after another, and the
    /// known sessions are searched too.
    @Test func everyUnfinishedImportIsOffered() async {
        let service = TwoPending()
        let app = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                           renderer: FakeRenderer(), importer: service, archive: UnavailableArchiveService(target: ""),
                           ai: MockAIService(), settings: makeSettings())
        app.watchCards = false
        await app.start()
        #expect(app.pendingImport?.sessionName == "One" && app.pendingImportQueue.count == 1)
        #expect(service.seen.value == app.sessionList.map(\.url))
        app.nextPendingImport()
        #expect(app.pendingImport?.sessionName == "Two")
        await app.discardPendingImport(app.pendingImport!)
        #expect(app.pendingImport == nil)
        #expect(await service.pendingImport(forVolume: "CARD-B")?.sessionName == "Two")
        #expect(await service.pendingImport(forVolume: "CARD-C") == nil)
    }

    /// M4: offered cards are remembered in the app's own defaults suite.
    @Test func offeredCardsLiveInTheAppsDefaults() {
        let settings = makeSettings()
        let app = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                           renderer: FakeRenderer(), importer: MockImportService(),
                           archive: UnavailableArchiveService(target: ""), ai: MockAIService(), settings: settings)
        let c = app.makeCardCoordinator()
        let store = c.store as? UserDefaultsOfferedStore
        #expect(store?.defaults === settings.userDefaults && store?.defaults !== UserDefaults.standard)
    }

    /// H4: new sessions go to the sessions folder (read at each import); the
    /// old default library is still searched for unfinished imports.
    @Test func newSessionsGoToTheSessionsFolder() async throws {
        let a = try tempDir("imp7-libA"), b = try tempDir("imp7-libB")
        defer {
            try? FileManager.default.removeItem(at: a)
            try? FileManager.default.removeItem(at: b)
        }
        let folder = Imp7Box(a)
        let service = RustImportService(libraryRoot: { folder.value }, globalIndex: nil)
        let plan = ImportPlan(candidates: [], destination: .newSession(name: "Trip"), renameTemplate: "", backupFolder: nil,
                              volume: card)
        #expect(try await service.options(for: plan).sessionDir.hasPrefix(a.path))
        folder.value = b
        #expect(try await service.options(for: plan).sessionDir.hasPrefix(b.path), "the setting is read live")
        #expect(await service.libraryRoot() == b)
        #expect(RustImportService.libraryRoot(sessionsFolder: a) == a || ProcessInfo.processInfo.environment["DS_LIBRARY_ROOT"] != nil)
        #expect(RustImportService.picturesLibrary.path.hasSuffix("Pictures/Darkslide"))
        let space = await service.freeBytes(for: .newSession(name: "x"))
        #expect((space ?? 0) > 0)
    }

    /// L1: auto-apply never touches clips; L4/M2 wording.
    @Test func autoApplySkipsClipsAndWordingIsPerFile() async {
        let m = makeModel(store: VideoStore())
        await m.start()
        #expect(await waitUntil { m.photos.count == 2 })
        var e = EditState()
        e.exposure.ev = 0.5
        #expect(ImportAutoApply.apply(Preset(name: "P", groups: [.exposure], edit: e), in: m, prior: []) == 1)
        #expect(m.photos.filter(\.isVideo).allSatisfy { m.edit(for: $0.id).isIdentity })
        #expect(ImportWording.items(photos: 1, videos: 0) == "1 photo")
        #expect(ImportWording.items(photos: 0, videos: 2) == "2 videos")
        #expect(ImportWording.stopped(done: 1, total: 1).contains("1 of 1 file."))
        let removed = ImportWording.cardRemoved("SONY")
        #expect(removed.contains("SONY was removed") && !removed.contains("/") && !removed.contains("os error"))
        #expect(ImportWording.resumeNote(sessionName: "Trip", left: 2).contains("2 files left"))
        #expect(ImportWording.damagedNote(1).contains("It is imported again"))
        #expect(ImportWording.ejectAnywayTitle(0) == "Eject the card?")
        #expect(ImportWording.stayOnCardTitle(1) == "1 file is only on the card")
        var s = ImportModel.ImportSummary(files: 0, bytes: 0, sessionName: "S", seconds: 1, ejected: false, verified: true,
                                          ejectError: nil)
        s.checkedByService = true
        s.skippedDuplicates = 2
        #expect(ImportWording.doneTitle(s) == "Everything was already in the library")
        #expect(ImportWording.doneDetail(s).contains("Nothing new was copied"))
        s.skippedDuplicates = 0
        #expect(ImportWording.doneTitle(s) == "Nothing was imported")
        #expect(ImportWording.ejectAnywayMessage(s) == "Keep the card until the import is complete.")
        s.failed = [ImportIssue(url: card, reason: "x")]
        #expect(ImportWording.doneDetail(s).contains("1 file failed"))
        #expect(ImportRunError.cardChanged.localizedDescription.contains("different card"))
        #expect(ImportRunError.notEnoughSpace("why").localizedDescription == "why")
        #expect(RustImportService.date(wallClockMs: RustImportService.wallClockMs(Date(timeIntervalSince1970: 1000)))
                == Date(timeIntervalSince1970: 1000))
    }

    /// Service errors from ds-ffi map to the sheet's messages.
    @Test func ffiRefusalsMapToImportErrors() {
        #expect(RustImportService.runError(.InvalidArgument(message: "the card changed since it was scanned: x"))
                as? ImportRunError == .cardChanged)
        if case .notEnoughSpace = RustImportService.runError(.Io(message: "not enough free space on the volume")) as? ImportRunError {
        } else { Issue.record("no space") }
        #expect(!(RustImportService.runError(.Io(message: "disk on fire")) is ImportRunError))
    }
}
