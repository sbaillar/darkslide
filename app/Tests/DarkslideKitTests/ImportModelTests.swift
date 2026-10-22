import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

private let utc = TimeZone(identifier: "UTC")!

/// Records eject calls; optionally fails.
final class FakeEjector: Ejecting, @unchecked Sendable {
    private let lock = NSLock()
    private var _ejected: [URL] = []
    let error: Error?
    init(error: Error? = nil) { self.error = error }
    var ejected: [URL] { lock.withLock { _ejected } }
    func eject(_ volume: URL) async throws {
        if let error { throw error }
        lock.withLock { _ejected.append(volume) }
    }
}

/// Import service whose stream ends early without an error (a truncated copy).
struct TruncatingImportService: ImportService {
    func scan(volume: URL) async throws -> [ImportCandidate] {
        try await MockImportService(inventedCount: 3).scan(volume: volume)
    }

    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> {
        AsyncThrowingStream { c in
            c.yield(ImportProgress(filesDone: 1, filesTotal: plan.candidates.count, bytesDone: 1, bytesTotal: 3))
            c.finish()
        }
    }
}

struct FailingScanService: ImportService {
    func scan(volume: URL) async throws -> [ImportCandidate] { throw MockError.notFound("card") }
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> { AsyncThrowingStream { $0.finish() } }
}

private struct ImportTestRenderer: Renderer {
    func render(_ photo: Photo, edit: EditState, request: RenderRequest) async throws -> RenderedFrame {
        RenderedFrame(width: 1, height: 1, rgba: Data(count: 4))
    }
}

/// A verifying service (what ds-import will be): wraps the mock and reports verification.
struct VerifyingService: ImportVerifying {
    var inner: MockImportService
    var verifiedAll = true
    func scan(volume: URL) async throws -> [ImportCandidate] { try await inner.scan(volume: volume) }
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> { inner.run(plan) }
    func report(for plan: ImportPlan) async -> ImportReport? {
        let n = plan.candidates.count
        return ImportReport(filesTotal: n, filesCopied: n, filesVerified: verifiedAll ? n : n - 1,
                            bytesCopied: plan.candidates.reduce(0) { $0 + $1.size })
    }
}

/// Records whether `run` was ever called.
final class SpyImportService: ImportService, @unchecked Sendable {
    private let lock = NSLock()
    private var runs = 0
    var runCount: Int { lock.withLock { runs } }
    func scan(volume: URL) async throws -> [ImportCandidate] {
        try await MockImportService(inventedCount: 3).scan(volume: volume)
    }
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> {
        lock.withLock { runs += 1 }
        return MockImportService(bytesPerSecond: 1e13).run(plan)
    }
}

let cardTraits = VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, uuid: "CARD-UUID")
let removableInspector = StaticVolumeInspector(["/Volumes/A9CARD": cardTraits, "/Volumes/CARD": cardTraits])

@MainActor
private final class Harness {
    let store = MockSessionStore(counts: [5])
    var imported: [SessionInfo] = []
    let ejector: FakeEjector

    init(ejector: FakeEjector = FakeEjector()) { self.ejector = ejector }

    func model(volume: URL? = URL(fileURLWithPath: "/Volumes/A9CARD"), service: any ImportService = verified(),
               existing: [SessionInfo] = [], copyEnabled: Bool = true, opener: (any FolderOpening)? = nil) -> ImportModel {
        ImportModel(volume: volume, importer: service, sessions: store, existingSessions: existing,
                    nameTemplate: "{date} {text}", renameTemplate: "{camera}_{seq4}", ejector: ejector,
                    inspector: removableInspector, copyEnabled: copyEnabled, folderOpener: opener,
                    timeZone: utc) { [weak self] s in self?.imported.append(s) }
    }
}

private func verified(count: Int = 10, dupEvery: Int = 4, failAfter: Int? = nil, all: Bool = true) -> VerifyingService {
    VerifyingService(inner: fastService(count: count, dupEvery: dupEvery, failAfter: failAfter), verifiedAll: all)
}

private func fastService(count: Int = 10, dupEvery: Int = 4, failAfter: Int? = nil) -> MockImportService {
    MockImportService(bytesPerSecond: 1e13, alreadyImportedEvery: dupEvery, inventedCount: count, failAfter: failAfter)
}

@MainActor @Suite struct ImportModelTests {
    @Test func scanSortsAndSkipsDuplicatesByDefault() async {
        let h = Harness()
        let m = h.model()
        #expect(m.phase == .idle)
        #expect(!m.canEject, "unknown until the volume's traits are read")
        await m.scan()
        #expect(m.canEject, "local removable media")
        #expect(m.verifies)
        #expect(m.phase == .ready)
        #expect(m.candidates.count == 10)
        #expect(m.duplicateCount == 3) // 0, 4, 8
        #expect(m.selected.count == 7)
        #expect(m.totalBytes == 10 * 48_500_000)
        #expect(m.selectedBytes == 7 * 48_500_000)
        m.skipDuplicates = false
        #expect(m.selected.count == 10)
        #expect(m.volumeName == "A9CARD")
    }

    @Test func toggleAndSelectAllNone() async {
        let h = Harness()
        let m = h.model()
        await m.scan()
        let dup = m.candidates.first { $0.alreadyImported }!
        let fresh = m.candidates.first { !$0.alreadyImported }!
        #expect(m.isLocked(dup))
        m.toggle(dup) // locked: no effect
        #expect(!m.isSelected(dup))
        m.toggle(fresh)
        #expect(!m.isSelected(fresh))
        m.toggle(fresh)
        #expect(m.isSelected(fresh))
        m.selectNone()
        #expect(m.selected.isEmpty)
        #expect(!m.canStart)
        m.selectAll()
        #expect(m.selected.count == 7)
    }

    @Test func namingAndPreview() async {
        let h = Harness()
        let m = h.model()
        await m.scan()
        m.sessionText = "Fall Hike"
        let day = ImportTemplates.format(m.candidates[0].captureDate, "yyyy-MM-dd", utc)
        #expect(m.newSessionName == "\(day) Fall Hike")
        #expect(m.destinationName == m.newSessionName)
        // The mock service reads no EXIF: no camera is guessed (L4; the live service reads it).
        #expect(m.renamePreview() == ["_0001.ARW", "_0002.ARW", "_0003.ARW"])
        #expect(m.renamePreview(count: 1).count == 1)
    }

    @Test func noVolumeStaysIdle() async {
        let h = Harness()
        let m = h.model(volume: nil)
        await m.scan()
        #expect(m.phase == .idle)
        #expect(m.volumeName == "No card")
        #expect(m.sessionDate.timeIntervalSinceNow < 1)
    }

    @Test func scanFailureAndRetry() async {
        let h = Harness()
        let m = h.model(service: FailingScanService())
        await m.scan()
        #expect(m.phase == .failed("Not found: card"))
        m.retry()
        // retry rescans asynchronously; give it a moment.
        for _ in 0..<50 where m.phase != .failed("Not found: card") || m.phase == .scanning {
            try? await Task.sleep(for: .milliseconds(10))
        }
        #expect(m.candidates.isEmpty)
    }

    /// H1: a clean verified import is "safe to remove", but the card is ejected
    /// only when the user presses Eject.
    @Test func importsIntoNewSessionAndEjectsOnlyWhenAsked() async throws {
        let h = Harness()
        let m = h.model()
        await m.scan()
        m.sessionText = "Hike"
        m.start()
        #expect(m.phase == .importing)
        #expect(m.isBusy)
        await m.wait()
        guard case .finished(let s) = m.phase else { Issue.record("not finished: \(m.phase)"); return }
        #expect(s.files == 7)
        #expect(s.bytes == 7 * 48_500_000)
        #expect(!s.ejected && s.verified && s.safeToRemove)
        #expect(s.ejectError == nil)
        #expect(s.sessionName.hasSuffix(" Hike"))
        #expect(ImportWording.cardLine(s)?.contains("safe to remove") == true)
        #expect(h.ejector.ejected.isEmpty, "never automatic")
        await m.ejectNow()
        #expect(h.ejector.ejected == [URL(fileURLWithPath: "/Volumes/A9CARD")])
        #expect(h.imported.map(\.name) == [s.sessionName])
        #expect(m.progress?.filesDone == 7)
        let sessions = try await h.store.listSessions()
        #expect(sessions.first?.name == s.sessionName)
    }

    @Test func importIntoExistingSessionWithoutEject() async throws {
        let h = Harness()
        let existing = try await h.store.listSessions()
        let m = h.model(existing: existing)
        await m.scan()
        m.destination = .existing(existing[0].id)
        #expect(m.destinationName == existing[0].name)
        if case .existing(let s) = m.makePlan().destination { #expect(s == existing[0]) } else { Issue.record("wrong dest") }
        m.start()
        await m.wait()
        guard case .finished(let s) = m.phase else { Issue.record("not finished"); return }
        #expect(!s.ejected)
        #expect(s.sessionName == existing[0].name)
        #expect(h.ejector.ejected.isEmpty)
        #expect(try await h.store.listSessions().count == existing.count)
    }

    @Test func unknownExistingSessionCannotStart() async {
        let h = Harness()
        let m = h.model()
        await m.scan()
        m.destination = .existing(UUID())
        #expect(!m.canStart)
        #expect(m.destinationName == "—")
        if case .newSession = m.makePlan().destination {} else { Issue.record("expected fallback to new session") }
        m.start()
        #expect(m.phase == .ready)
    }

    @Test func ejectFailureStillCompletes() async {
        let h = Harness(ejector: FakeEjector(error: MockError.invalid("Volume in use")))
        let m = h.model()
        await m.scan()
        m.start()
        await m.wait()
        await m.ejectNow()
        guard case .finished(let s) = m.phase else { Issue.record("not finished"); return }
        #expect(!s.ejected)
        #expect(s.ejectError == "Volume in use")
    }

    @Test func pulledCardFailsAndNeverEjects() async {
        let h = Harness()
        let m = h.model(service: fastService(failAfter: 2))
        await m.scan()
        m.start()
        await m.wait()
        #expect(m.phase == .failed("The card was removed during import"))
        #expect(h.ejector.ejected.isEmpty)
        #expect(h.imported.isEmpty)
        m.retry() // always a fresh scan
        await m.wait()
        #expect(m.phase == .ready)
        m.retry() // no-op when not failed
        #expect(m.phase == .ready)
    }

    @Test func truncatedStreamIsAFailure() async {
        let h = Harness()
        let m = h.model(service: TruncatingImportService())
        await m.scan()
        m.skipDuplicates = false
        m.start()
        await m.wait()
        #expect(m.phase == .failed(ImportFailure.incomplete(done: 1, total: 3).localizedDescription))
        #expect(h.ejector.ejected.isEmpty)
    }

    /// H2: Stop says what happened and how to continue.
    @Test func stopSaysHowToContinue() async {
        let h = Harness()
        let slow = MockImportService(bytesPerSecond: 48_500_000 / 2, inventedCount: 6, failAfter: nil)
        let m = h.model(service: slow)
        await m.scan()
        m.start()
        try? await Task.sleep(for: .milliseconds(30))
        m.cancel()
        #expect(m.isStopping && !m.isCancellable)
        await m.wait()
        guard case .failed(let msg) = m.phase else { Issue.record("\(m.phase)"); return }
        #expect(msg.contains("Import stopped after") && msg.contains("import again to continue"))
        #expect(!m.isStopping)
        #expect(h.ejector.ejected.isEmpty)
        #expect(h.imported.isEmpty)
    }

    @Test func throughputAndEtaDuringImport() async {
        let h = Harness()
        let m = h.model()
        await m.scan()
        #expect(m.throughput == nil)
        #expect(m.eta == nil)
        m.start()
        await m.wait()
        // Everything copied: rate known (or instant), nothing left.
        if let eta = m.eta { #expect(eta == 0) }
    }

    @Test func setVolumeRescans() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let h = Harness()
        let m = h.model(volume: nil, service: MockImportService())
        await m.setVolume(card.url)
        #expect(!m.canEject) // temp folder: no volume traits
        #expect(m.candidates.count == 3)
        #expect(m.phase == .ready)
    }

    @Test func ejectDecisionUsesVolumeTraitsNotPaths() {
        #expect(cardTraits.canEject)
        #expect(!VolumeTraits(isLocal: false, isEjectable: false, isRemovable: false).canEject, "NAS share")
        #expect(!VolumeTraits(isLocal: false, isEjectable: true, isRemovable: true).canEject)
        #expect(VolumeTraits(isLocal: true, isEjectable: false, isRemovable: true).canEject)
        #expect(!VolumeTraits(isLocal: true, isEjectable: false, isRemovable: false).canEject, "internal disk")
    }

    @Test func networkVolumeUnderVolumesIsNotEjectable() async {
        let h = Harness()
        let nas = URL(fileURLWithPath: "/Volumes/pics")
        let m = ImportModel(volume: nas, importer: verified(), sessions: h.store, existingSessions: [],
                            nameTemplate: "{date}", renameTemplate: "{original}", ejector: h.ejector,
                            inspector: StaticVolumeInspector(["/Volumes/pics": VolumeTraits(isLocal: false, isEjectable: false, isRemovable: false)])) { _ in }
        await m.scan()
        #expect(!m.canEject)
        m.start()
        await m.wait()
        guard case .finished(let s) = m.phase else { Issue.record("not finished"); return }
        #expect(m.ejectGate?.offersEject == false)
        await m.ejectNow() // a non-ejectable volume is never ejected
        #expect(!s.ejected && h.ejector.ejected.isEmpty)
    }

    @Test func unverifiedImportNeverClaimsVerifiedOrEjects() async {
        let h = Harness()
        for service in [fastService() as any ImportService, verified(all: false)] {
            let m = h.model(service: service)
            m.sessionText = UUID().uuidString // distinct session names per run
            await m.scan()
            m.start()
            await m.wait()
            guard case .finished(let s) = m.phase else { Issue.record("not finished"); return }
            #expect(!s.verified && !s.ejected && !s.safeToRemove)
            let text = [ImportWording.doneTitle(s), ImportWording.doneDetail(s), ImportWording.cardLine(s) ?? ""]
                .joined(separator: " ").lowercased()
            #expect(!text.contains("safe to remove"))
            #expect(text.contains("keep this card"))
            if s.checkedByService {
                #expect(s.files == 6, "only the verified files count (7 selected, 6 verified)")
            } else {
                #expect(text.contains("not verified") && !text.contains("and verified"))
            }
        }
        #expect(h.ejector.ejected.isEmpty)
        #expect(!ImportWording.readyLine(bytes: 1, destination: "x", verifies: false).contains("verify"))
    }

    @Test func liveModeNeverCopies() async {
        let h = Harness()
        let spy = SpyImportService()
        let m = h.model(service: spy, copyEnabled: false)
        await m.scan()
        #expect(m.phase == .ready && m.candidates.count == 3, "the card can still be looked at")
        #expect(!m.canStart)
        m.start()
        #expect(m.phase == .ready)
        await m.wait()
        #expect(spy.runCount == 0)
        #expect(h.imported.isEmpty && h.ejector.ejected.isEmpty)
    }

    @Test func liveAppModelDisablesCopy() async {
        let settings = makeSettings()
        let spy = SpyImportService()
        let app = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                           renderer: ImportTestRenderer(), importer: spy,
                           archive: MockArchiveService(target: "/nonexistent/x", isReachable: { _ in false }),
                           ai: MockAIService(), settings: settings, mode: .live)
        let m = ImportModel(model: app, volume: URL(fileURLWithPath: "/Volumes/CARD"), inspector: removableInspector)
        #expect(!m.copyEnabled)
        await m.scan()
        m.start()
        await m.wait()
        #expect(spy.runCount == 0)
        #expect(ImportWording.liveUnavailableMessage.contains("never copies"))
    }

    @Test func browseOpensTheCardReadOnly() async {
        let h = Harness()
        let opener = FakeOpener()
        let m = h.model(copyEnabled: false, opener: opener)
        #expect(m.canBrowse)
        await m.browse()
        #expect(opener.opened == [URL(fileURLWithPath: "/Volumes/A9CARD")])
        let none = h.model(volume: nil, opener: opener)
        #expect(!none.canBrowse)
        await none.browse()
        #expect(opener.opened.count == 1)
        #expect(!h.model().canBrowse, "no opener: action hidden")
    }

    @Test func cardRemovalStopsImport() async {
        // A unique volume path: the registry is process-wide and tests run in parallel.
        let vol = URL(fileURLWithPath: "/Volumes/REMOVAL-\(UUID().uuidString)")
        let h = Harness()
        let slow = VerifyingService(inner: MockImportService(bytesPerSecond: 48_500_000 / 2, inventedCount: 6))
        let m = h.model(volume: vol, service: slow)
        await m.scan()
        m.start()
        try? await Task.sleep(for: .milliseconds(20))
        ImportRegistry.shared.volumeDidDisappear(URL(fileURLWithPath: "/Volumes/OTHER-\(UUID().uuidString)"))
        #expect(m.phase == .importing, "someone else's card")
        ImportRegistry.shared.volumeDidDisappear(vol)
        await m.wait()
        guard case .failed(let msg) = m.phase else { Issue.record("expected failure, got \(m.phase)"); return }
        #expect(msg.contains("removed") && msg.contains("Nothing was deleted"))
        #expect(h.ejector.ejected.isEmpty && h.imported.isEmpty)
        #expect(ImportRegistry.shared.count >= 1)
        let idle = h.model(volume: nil)
        idle.volumeDidDisappear(vol)
        #expect(idle.phase == .idle)
    }

    @Test func findCardIfNeededPicksAMountedCard() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let events = FakeVolumeEvents()
        events.mounted = [card.url]
        let h = Harness()
        let m = h.model(volume: nil, service: MockImportService())
        await m.findCardIfNeeded(source: events, detector: testDetector)
        #expect(m.volume == card.url && m.candidates.count == 3)
        let none = h.model(volume: nil)
        await none.findCardIfNeeded(source: FakeVolumeEvents(), detector: testDetector)
        #expect(none.phase == .idle)
    }

    @Test func appModelWiringReloadsAndOpensSession() async throws {
        let suite = testSuiteName()
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let settings = AppSettings(defaults: defaults)
        let store = MockSessionStore(counts: [3])
        let app = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: ImportTestRenderer(),
                           importer: fastService(count: 4, dupEvery: 0),
                           archive: MockArchiveService(target: "/nonexistent/x", isReachable: { _ in false }),
                           ai: MockAIService(), settings: settings)
        await app.start()
        let ejector = FakeEjector()
        let m = ImportModel(model: app, volume: URL(fileURLWithPath: "/Volumes/CARD"), ejector: ejector)
        #expect(m.existingSessions.count == 1)
        #expect(m.nameTemplate == settings.sessionNameTemplate)
        await m.scan()
        m.sessionText = "Wired"
        m.start()
        await m.wait()
        guard case .finished(let s) = m.phase else { Issue.record("not finished \(m.phase)"); return }
        #expect(app.currentSession?.name == s.sessionName)
        #expect(app.sessionList.count == 2)
        app.stop()
    }
}

@MainActor
final class FakeOpener: FolderOpening {
    var opened: [URL] = []
    func openFolder(_ url: URL) async { opened.append(url) }
}
