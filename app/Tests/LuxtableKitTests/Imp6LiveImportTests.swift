import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Live path: a real exFAT card image with the sample ARWs and an MP4, the real
/// FFI (lt-import + lt-session), the real session store. The card must be
/// byte-identical afterwards.
@MainActor @Suite(.serialized) struct Imp6LiveImportTests {
    struct Env {
        let root: URL
        let card: TestCardImage
        let library: URL
        let index: URL
        let recents: RecentFolders
        let store: RustSessionStore

        func service(throttleMs: UInt32 = 0) -> RustImportService {
            RustImportService(libraryRoot: library, globalIndex: index, throttleMs: throttleMs)
        }

        func cleanup() async {
            await card.detach()
            try? FileManager.default.removeItem(at: root)
        }
    }

    /// `nil` (skip) without samples or when disk images are unavailable.
    static func env() async throws -> Env? {
        guard sampleARWs().count >= 3 else { return nil }
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("imp6-live-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        guard let img = await TestCardImage.make(in: root) else {
            try? FileManager.default.removeItem(at: root)
            return nil
        }
        _ = try await fillCard(img.mount)
        // What the card holds, not the page cache.
        guard let card = await img.remount() else {
            try? FileManager.default.removeItem(at: root)
            return nil
        }
        let recents = RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!)
        return Env(root: root, card: card, library: root.appendingPathComponent("Library"),
                   index: root.appendingPathComponent("global.db"), recents: recents,
                   store: RustSessionStore(recents: recents, storeBase: root.appendingPathComponent("stores")))
    }

    /// Run `body` with a fresh card and library; always detaches and cleans up.
    static func withEnv(_ body: (Env) async throws -> Void) async throws {
        guard let env = try await env() else { return }
        do {
            try await body(env)
        } catch {
            await env.cleanup()
            throw error
        }
        await env.cleanup()
    }

    func importModel(_ env: Env, service: RustImportService, ejector: FakeEjector,
                     existing: [SessionInfo] = [], preferred: URL? = nil,
                     onImported: @escaping @MainActor (SessionInfo) async -> Void) -> ImportModel {
        let traits = VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "IMP6")
        return ImportModel(volume: env.card.mount, importer: service, sessions: env.store, existingSessions: existing,
                           nameTemplate: "{date} {text}", renameTemplate: "{camera}_{seq4}", preferredSession: preferred,
                           ejector: ejector, inspector: StaticVolumeInspector([env.card.mount.standardizedFileURL.path: traits]),
                           onImported: onImported)
    }

    @Test func cardImportsIntoANewSessionVerifiedRegisteredAndUntouched() async throws {
        try await Self.withEnv { env in try await newSession(env) }
    }

    func newSession(_ env: Env) async throws {
        let before = await cardListing(env.card.mount)
        let ejector = FakeEjector()
        var opened: SessionInfo?
        let m = importModel(env, service: env.service(), ejector: ejector) { opened = $0 }
        await m.scan()
        #expect(m.phase == .ready)
        #expect(m.candidates.count == 4 && m.videoCount == 1, "\(m.candidates.map(\.filename))")
        #expect(m.skippedUnknown.contains { $0.url.lastPathComponent == "notes.txt" } && m.scanErrors.isEmpty)
        #expect(m.renamePreview(count: 1).first?.hasPrefix("ILCE-9_0001.") == true, "\(m.renamePreview())")
        m.sessionText = "Iceland"
        m.subfolderTemplate = "{date}"
        #expect(m.subfolderPreview?.count == 8, "yyyymmdd: \(m.subfolderPreview ?? "nil")")
        m.verifySourceReread = true
        let started = Date()
        m.start()
        await m.wait()
        let secs = Date().timeIntervalSince(started)
        guard case .finished(let s) = m.phase else { #expect(Bool(false), "\(m.phase)"); return }
        #expect(s.verified && s.complete, "\(s)")
        #expect(s.needsAcknowledgement, "notes.txt stays on the card")
        #expect(!s.ejected && ejector.ejected.isEmpty, "never auto-ejected before “I understand”")
        let report = try #require(m.lastReport)
        print(String(format: "IMP6 import from exFAT image: %d items, %.1f MB in %.2f s = %.1f MB/s (with source re-read)",
                     report.filesCopied, Double(report.bytesCopied) / 1e6, secs,
                     Double(report.bytesCopied) / 1e6 / secs))
        // The session: registered, clips included, Internal originals under Capture/<date>/.
        let session = try #require(opened)
        #expect(RustImportService.isImportSession(session.url) && session.name.contains("Iceland"))
        let photos = try await env.store.photos(in: session)
        #expect(photos.count == 4 && photos.filter(\.isVideo).count == 1)
        #expect(photos.allSatisfy { $0.url?.path.contains("/Capture/") == true })
        #expect(photos.filter { !$0.isVideo }.allSatisfy { $0.filename.hasPrefix("ILCE-9_") }, "\(photos.map(\.filename))")
        // Ratings work and persist (sidecars in the session).
        var first = try #require(photos.first { !$0.isVideo })
        first.rating = 4
        try await env.store.save(photos: [first])
        let reopened = RustSessionStore(recents: env.recents, storeBase: env.root.appendingPathComponent("stores"))
        let again = try await reopened.photos(in: session)
        #expect(again.first { $0.id == first.id }?.rating == 4)
        // Eject only after acknowledging.
        m.acknowledged = true
        await m.ejectNow()
        #expect(ejector.ejected == [env.card.mount])
        #expect(await cardListing(env.card.mount) == before, "the card is byte-identical")

        // Scan again: everything is in the library now.
        let m2 = importModel(env, service: env.service(), ejector: FakeEjector()) { _ in }
        await m2.scan()
        #expect(m2.candidates.allSatisfy(\.alreadyImported) && m2.duplicateCount == 4)
        #expect(m2.selected.isEmpty && !m2.canStart, "nothing new to import")
    }

    @Test func aStoppedImportIsOfferedForResumeAtLaunchAndFinishes() async throws {
        try await Self.withEnv { env in try await resume(env) }
    }

    func resume(_ env: Env) async throws {
        let before = await cardListing(env.card.mount)
        // Slow enough (200 ms per chunk read) that the cancel always lands mid-copy, even
        // on a loaded machine (review M3: at 40 ms a busy run could finish first).
        let slow = env.service(throttleMs: 200)
        let m = importModel(env, service: slow, ejector: FakeEjector()) { _ in }
        await m.scan()
        m.sessionText = "Resume"
        m.start()
        #expect(await waitUntil(timeout: .seconds(30)) { (m.progress?.bytesDone ?? 0) > 0 })
        m.cancel()
        // The Rust side stops at its next chunk; the journal stays. Wait for the stopped
        // run to end, so the resume below never races it (review M3).
        await m.wait()
        var pending: [PendingImportInfo] = []
        let deadline = Date().addingTimeInterval(30)
        while pending.first?.pending ?? 0 == 0, Date() < deadline {
            try await Task.sleep(for: .milliseconds(100))
            pending = await slow.pendingImports()
        }
        let p = try #require(pending.first, "no journal left: the import finished before the cancel (\(String(describing: m.phase)))")
        #expect(p.sessionName.contains("Resume") && p.pending > 0)

        // Launch: the app offers to resume; Resume opens the session and aims the sheet at it.
        let settings = makeSettings()
        settings.onboardingComplete = true
        let app = AppModel(sessions: env.store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                           importer: env.service(), archive: UnavailableArchiveService(target: ""), ai: MockAIService(),
                           settings: settings, mode: .live)
        app.watchCards = false
        await app.checkPendingImports()
        #expect(app.pendingImport?.sessionURL.standardizedFileURL == p.sessionURL.standardizedFileURL)
        await app.resumePendingImport(try #require(app.pendingImport))
        #expect(app.pendingImport == nil && app.sheet == .importCard(nil))
        #expect(app.importResumeTarget != nil && app.sessionList.contains { $0.url.standardizedFileURL == p.sessionURL.standardizedFileURL })

        var done: SessionInfo?
        let r = importModel(env, service: env.service(), ejector: FakeEjector(),
                            existing: app.sessionList.filter { RustImportService.isImportSession($0.url) },
                            preferred: app.importResumeTarget) { done = $0 }
        guard case .existing = r.destination else { #expect(Bool(false), "resume targets the session: \(r.destination)"); return }
        await r.scan()
        #expect(r.canStart, "the rest of the card is offered: \(r.phase)")
        r.start()
        await r.wait()
        guard case .finished(let s) = r.phase else { #expect(Bool(false), "\(r.phase)"); return }
        #expect(s.verified && s.complete)
        let session = try #require(done)
        let photos = try await env.store.photos(in: session)
        #expect(photos.count == 4, "each item once: \(photos.map(\.filename))")
        #expect(await slow.pendingImports().isEmpty)
        #expect(await cardListing(env.card.mount) == before)
    }

    @Test func discardRemovesOnlyTheJournal() async throws {
        let lib = FileManager.default.temporaryDirectory.appendingPathComponent("imp6-lib-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: lib) }
        let service = RustImportService(libraryRoot: lib, globalIndex: nil)
        #expect(await service.pendingImports().isEmpty, "no library yet")
        let p = PendingImportInfo(sessionURL: lib.appendingPathComponent("None"), done: 0, pending: 1, failed: 0)
        try await service.discard(p) // nothing there: nothing to do
        #expect(!service.canImport(into: SessionInfo(name: "x", url: lib, photoCount: 0, createdAt: Date())))
        #expect(try service.renderRename("{original}", original: "DSC1.ARW", date: Date(), seq: 1, camera: "", text: "")
                == "DSC1.ARW")
        #expect(try service.renderSubfolder("", original: "a.ARW", date: Date(), camera: "", text: "") == "")
        #expect(throws: (any Error).self) {
            try service.renderRename("{nope}", original: "a.ARW", date: Date(), seq: 1, camera: "", text: "")
        }
        // Rendering matches the app's own template engine for the presets (same names on disk as in the preview).
        let date = Date(timeIntervalSince1970: 1_786_008_605)
        for t in ImportTemplates.renamePresets {
            let rust = try service.renderRename(t, original: "DSC01234.ARW", date: date, seq: 7, camera: "ILCE-9", text: "Hike")
            let swift = ImportTemplates.rename(t, original: "DSC01234.ARW", date: date, seq: 7, camera: "ILCE-9", text: "Hike")
            #expect(rust == swift, "\(t): \(rust) vs \(swift)")
        }
        // An import into a folder opened in place is refused before anything is copied.
        let plan = ImportPlan(candidates: [], destination: .existing(SessionInfo(name: "x", url: lib, photoCount: 0,
                                                                                createdAt: Date())),
                              renameTemplate: "", backupFolder: nil, volume: lib)
        await #expect(throws: (any Error).self) { _ = try await service.options(for: plan) }
        var threw = false
        do { for try await _ in service.run(plan) {} } catch { threw = true }
        #expect(threw)
        #expect(await service.report(for: plan) == nil)
    }
}
