import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Ejects only after a delay, so a test can cancel mid-eject.
final class SlowEjector: Ejecting, @unchecked Sendable {
    private let lock = NSLock()
    private var _ejected: [URL] = []
    var ejected: [URL] { lock.withLock { _ejected } }
    func eject(_ volume: URL) async throws {
        try? await Task.sleep(for: .milliseconds(150))
        lock.withLock { _ejected.append(volume) }
    }
}

struct FullyVerifying: ImportVerifying {
    var inner = MockImportService(bytesPerSecond: 1e13, alreadyImportedEvery: 0, inventedCount: 4)
    var reportedTotal: Int?
    func scan(volume: URL) async throws -> [ImportCandidate] { try await inner.scan(volume: volume) }
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> { inner.run(plan) }
    func report(for plan: ImportPlan) async -> ImportReport? {
        let n = reportedTotal ?? plan.candidates.count
        return ImportReport(filesTotal: n, filesCopied: n, filesVerified: n, bytesCopied: 1)
    }
}

@MainActor
@Suite struct ImportHonestyTests {
    @Test func liveWordingNeverClaimsVerifiedCopies() {
        #expect(!ImportWording.cardPanelNote(verifies: false, live: true).contains("verified,"))
        #expect(ImportWording.cardPanelNote(verifies: false, live: true).contains("coming soon"))
        #expect(ImportWording.cardPanelNote(verifies: false, live: false).contains("not verified"))
        #expect(ImportWording.cardPanelNote(verifies: true, live: true).contains("verified"))
        #expect(!ImportWording.onboardingImport(verifies: false).text.contains("Copies are verified"))
        #expect(ImportWording.onboardingImport(verifies: true).text.contains("verified"))
        #expect(ImportWording.backupNote(verifies: false).contains("not verified"))
        #expect(ImportWording.backupNote(verifies: true) == "A second verified copy, written during import.")
        #expect(ImportWording.stoppedWhileFinishing(sessionName: "S", ejected: false).contains("not ejected"))
        #expect(ImportWording.stoppedWhileFinishing(sessionName: "S", ejected: true).contains("was ejected"))
        let c = ImportCandidate(url: URL(fileURLWithPath: "/V/DCIM/A.ARW"), size: 1_000_000, captureDate: Date(),
                                alreadyImported: false)
        #expect(ImportWording.tileLabel(c, selected: true, locked: false).hasSuffix("selected"))
        #expect(ImportWording.tileLabel(c, selected: false, locked: false).hasSuffix("not selected"))
        #expect(ImportWording.tileLabel(c, selected: false, locked: true).hasSuffix("already imported"))
    }

    @Test func verifiedMustCoverThePlan() {
        let cands = (0..<3).map {
            ImportCandidate(url: URL(fileURLWithPath: "/V/\($0).ARW"), size: 1, captureDate: Date(), alreadyImported: false)
        }
        let plan = ImportPlan(candidates: cands, destination: .newSession(name: "x"), renameTemplate: "{original}",
                              backupFolder: nil)
        #expect(ImportReport(filesTotal: 3, filesCopied: 3, filesVerified: 3, bytesCopied: 3).allVerified(for: plan))
        #expect(!ImportReport(filesTotal: 2, filesCopied: 2, filesVerified: 2, bytesCopied: 2).allVerified(for: plan),
                "a stale/partial report for fewer files never counts")
    }

    @Test func staleReportDoesNotEject() async {
        let ejector = FakeEjector()
        let m = ImportModel(volume: URL(fileURLWithPath: "/Volumes/CARD"), importer: FullyVerifying(reportedTotal: 1),
                            sessions: MockSessionStore(counts: [1]), existingSessions: [], nameTemplate: "{text}",
                            renameTemplate: "{original}", ejector: ejector, inspector: removableInspector) { _ in }
        await m.scan()
        m.sessionText = "S"
        m.start()
        await m.wait()
        guard case .finished(let s) = m.phase else {
            Issue.record("expected finished, got \(m.phase)")
            return
        }
        #expect(!s.verified && !s.ejected && ejector.ejected.isEmpty)
    }

    /// Stopping right after the copy never ejects (eject is never automatic).
    @Test func stopAtTheEndNeverEjects() async {
        let ejector = SlowEjector()
        var imported: [SessionInfo] = []
        let m = ImportModel(volume: URL(fileURLWithPath: "/Volumes/CARD"), importer: FullyVerifying(),
                            sessions: MockSessionStore(counts: [1]), existingSessions: [], nameTemplate: "{text}",
                            renameTemplate: "{original}", ejector: ejector, inspector: removableInspector) {
            imported.append($0)
        }
        await m.scan()
        m.sessionText = "Trip"
        #expect(m.canEject)
        m.start()
        #expect(m.isCancellable)
        #expect(await waitUntil { m.progress?.filesDone == m.progress?.filesTotal && m.progress?.filesTotal ?? 0 > 0 })
        m.cancel()
        await m.wait()
        #expect(ejector.ejected.isEmpty, "never ejected by itself")
        if case .finished(let s) = m.phase { #expect(!s.ejected) }
        #expect(imported.count <= 1)
    }

    @Test func autoApplyPresetTouchesOnlyNewPhotos() async {
        let m = makeModel(counts: [6])
        await m.start()
        let old = Set(m.visibleIDs.prefix(3))
        var e = EditState()
        e.exposure.ev = 0.7
        let preset = Preset(name: "Base", groups: [.exposure], edit: e)
        let changed = ImportAutoApply.apply(preset, in: m, prior: old)
        #expect(changed == 3)
        #expect(m.visibleIDs.filter { m.edit(for: $0).exposure.ev == 0.7 }.count == 3)
        #expect(old.allSatisfy { m.edit(for: $0).isIdentity })
        #expect(m.history.undoLabel == "Auto-apply “Base”" && m.history.undoStack.count == 1, "one undo step")
        #expect(ImportAutoApply.apply(preset, in: m, prior: Set(m.visibleIDs)) == 0)
    }

    @Test func demoImportAppliesTheAutoPreset() async {
        let m = makeModel(counts: [2])
        await m.start()
        var e = EditState()
        e.exposure.ev = 0.4
        let preset = Preset(name: "Auto", groups: [.exposure], edit: e)
        let im = ImportModel(model: m, volume: URL(fileURLWithPath: "/Volumes/CARD"), ejector: FakeEjector(),
                             inspector: removableInspector, autoApply: { preset })
        await im.scan()
        im.sessionText = "New"
        im.start()
        await im.wait()
        #expect(m.currentSession?.name.contains("New") == true)
        // The mock store creates an empty session: nothing to apply, and nothing breaks.
        #expect(m.visibleIDs.allSatisfy { m.edit(for: $0).exposure.ev == 0.4 })
        #expect(im.priorPhotoIDs.isEmpty)
    }
}
