import CoreGraphics
import DarkslideAI
import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Two sessions whose photo ids overlap (every real session numbers its photos from 1), and a
/// store that points bare ids at the session loaded last, like `RustSessionStore` does.
private actor TwoSessionStore: SessionStore, CurrentSessionPointing, SessionFileOrganizing {
    let a = SessionInfo(name: "A", url: URL(fileURLWithPath: "/tmp/ds-switch-A"), photoCount: 6, createdAt: Date())
    let b = SessionInfo(name: "B", url: URL(fileURLWithPath: "/tmp/ds-switch-B"), photoCount: 6, createdAt: Date())
    private var current: UUID?
    /// (session the store resolved the ids in, ids) per `save(photos:)`.
    private(set) var savedPhotos: [(session: UUID?, ids: [PhotoID])] = []
    private(set) var savedEdits: [(session: UUID?, ids: [PhotoID])] = []
    private var holdingB = false
    private(set) var waitingOnB = false
    private var release: CheckedContinuation<Void, Never>?

    /// While set, `save` throws (the volume is gone, the disk is full).
    private var failSaves = false
    func setFailSaves(_ on: Bool) { failSaves = on }
    /// `moveBack` calls (session the store resolved the ids in).
    private(set) var movedBack: [UUID?] = []

    func listSessions() async throws -> [SessionInfo] { [a, b] }
    func canMoveOriginals(in session: SessionInfo) async -> Bool { true }
    func move(_ ids: [PhotoID], to folder: SessionFolderKind) async throws -> [FolderMove] {
        ids.map { FolderMove(id: $0, from: "Capture/\($0).ARW", to: "\(folder.rawValue)/\($0).ARW") }
    }
    func trashRejected(in session: SessionInfo) async throws -> RejectsTrashed { throw MockError.invalid("no") }
    func moveBack(_ moves: [FolderMove], hidden: [PhotoID], in session: SessionInfo) async throws { movedBack.append(current) }
    func restoreSetAsideVariants(in session: SessionInfo) async throws -> Int { 0 }
    func emptyTrash(in session: SessionInfo) async throws -> Int { 0 }
    func newVariant(of id: PhotoID) async throws -> PhotoID { throw MockError.invalid("no") }

    func photos(in session: SessionInfo) async throws -> [Photo] {
        current = session.id
        let meta = PhotoMeta(camera: "Cam", lens: "", iso: 100, shutter: 0.01, aperture: 2.8, focalLength: 35,
                             captureDate: Date(timeIntervalSince1970: 1_790_000_000), pixelWidth: 6000, pixelHeight: 4000,
                             fileSize: 1)
        return (1...6).map { Photo(id: PhotoID(Int64($0)), filename: "\(session.name)-\($0).ARW", meta: meta) }
    }

    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { throw MockError.invalid("no") }
    func save(photos: [Photo]) async throws {
        if failSaves { throw MockError.invalid("volume gone") }
        savedPhotos.append((current, photos.map(\.id)))
    }
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws { savedEdits.append((current, Array(edits.keys))) }
    func makeCurrent(_ session: SessionInfo) async { current = session.id }

    func edits(in session: SessionInfo) async throws -> [PhotoID: EditState] {
        if holdingB, session.id == b.id {
            waitingOnB = true
            await withCheckedContinuation { release = $0 }
        }
        return [:]
    }

    func holdLoadOfB() { holdingB = true }
    func releaseB() {
        holdingB = false
        release?.resume()
        release = nil
    }
}

/// An AI service that waits at `autoAdjust` and `cull` until released.
private final class GatedAI: AIService, @unchecked Sendable {
    private let lock = NSLock()
    private var waiters: [CheckedContinuation<Void, Never>] = []
    private var open = false
    var waiting: Bool { lock.withLock { !waiters.isEmpty } }

    private func gate() async {
        await withCheckedContinuation { c in
            let go = lock.withLock { () -> Bool in
                if open { return true }
                waiters.append(c)
                return false
            }
            if go { c.resume() }
        }
    }

    func releaseAll() {
        let w = lock.withLock { () -> [CheckedContinuation<Void, Never>] in
            open = true
            defer { waiters = [] }
            return waiters
        }
        for c in w { c.resume() }
    }

    func cull(_ photos: [Photo], images: any ImageProvider,
              progress: @escaping @Sendable (Int, Int) -> Void) async throws -> [CullSuggestion] {
        await gate()
        return photos.map { CullSuggestion(photoID: $0.id, score: 0.9, pick: .picked, reasons: ["Sharp"]) }
    }

    func autoAdjust(_ photo: Photo, current: EditState, images: any ImageProvider) async throws -> EditState {
        await gate()
        var e = current
        e.exposure.ev = 1
        return e
    }

    func applyInstruction(_ text: String, to current: EditState) async throws -> EditState { current }
}

@MainActor
private func openA(ai: any AIService = MockAIService()) async -> (AppModel, TwoSessionStore) {
    let store = TwoSessionStore()
    let settings = makeSettings()
    settings.onboardingComplete = true
    settings.autoAdvance = false
    settings.sessionClosed = true
    let m = AppModel(sessions: store, images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                     importer: MockImportService(bytesPerSecond: 1e12),
                     archive: MockArchiveService(target: "/nonexistent/archive", interval: .seconds(3600),
                                                 step: .milliseconds(1), isReachable: { _ in true }),
                     ai: ai, settings: settings)
    m.watchCards = false
    await m.start()
    await m.open(store.a)
    return (m, store)
}

@MainActor
@Suite struct SessionSwitchTests {
    @Test func opensAnotherSessionWithNothingLeftOverFromTheFirst() async throws {
        let (m, store) = await openA()
        let id = PhotoID(3)
        m.focus(id, extend: false)
        // State the first session built up, keyed by its photo ids.
        m.developInfo[id] = PhotoDevelopInfo(asShot: WhiteBalanceValue(kelvin: 5000, tint: 0), lens: "A lens",
                                             lensProfile: nil, iso: 100, renderer: "test")
        await m.runAICull()
        #expect(!m.cullSuggestions.isEmpty)
        m.browserState.bestHints = [id]
        m.aiMarks[id] = AIMarks(changes: [AIField(.exposure, "ev"): AIChange(source: .auto)])
        m.aiMarksSettled = [id]
        m.viewer.masks.set(VisionMask(width: 1, height: 1, bytes: [255]), photo: id, target: .sky)
        #expect(m.viewer.masks.count == 1)

        await m.open(store.b)

        #expect(m.currentSession?.name == "B")
        #expect(m.developInfo.isEmpty && m.cullSuggestions.isEmpty && m.browserState.bestHints.isEmpty)
        #expect(m.aiMarks.isEmpty && m.aiMarksSettled.isEmpty && m.optimizeReports.isEmpty && m.depthVersions.isEmpty)
        #expect(m.viewer.masks.count == 0, "A's sky mask must not render or export on B's photo 3")
        #expect(m.photo(id)?.filename == "B-3.ARW")
        m.stop()
    }

    @Test func reopeningTheSameSessionKeepsItsState() async throws {
        let (m, store) = await openA()
        let id = PhotoID(2)
        m.developInfo[id] = PhotoDevelopInfo(asShot: WhiteBalanceValue(kelvin: 5000, tint: 0), lens: nil,
                                             lensProfile: nil, iso: nil, renderer: "test")
        await m.open(store.a)
        #expect(m.developInfo[id] != nil, "only a different session resets the id-keyed caches")
        m.stop()
    }

    @Test func editsAndRatingsWhileAnotherSessionLoadsAreIgnoredNotSavedIntoIt() async throws {
        let (m, store) = await openA()
        await store.holdLoadOfB()
        m.focus(PhotoID(2), extend: false)
        let opening = Task { await m.open(store.b) }
        #expect(await waitUntil { m.isSwitchingSession })
        var waiting = false
        for _ in 0..<2000 where !waiting {
            waiting = await store.waitingOnB
            if !waiting { try await Task.sleep(for: .milliseconds(5)) }
        }
        #expect(waiting, "B's load is under way: the store already resolves ids in B")

        m.setRating(4)
        m.updateEdit(label: "Exposure") { $0.exposure.ev = 1 }
        m.undo()
        #expect(m.photo(PhotoID(2))?.rating == 0 && m.edit(for: PhotoID(2)) == EditState())
        #expect(m.saver.pendingCount == 0 && !m.history.canUndo)

        await store.releaseB()
        await opening.value
        await m.saver.flush()
        #expect(m.currentSession?.name == "B" && !m.isSwitchingSession)
        #expect(await store.savedPhotos.isEmpty, "nothing reached B's sidecars")
        #expect(await store.savedEdits.isEmpty)

        // And once the switch is done, edits work again, in B.
        m.focus(PhotoID(2), extend: false)
        m.setRating(2)
        await m.saver.flush()
        #expect(await store.savedPhotos.map(\.session) == [store.b.id])
        m.stop()
    }

    @Test func anAICullStartedInOneSessionNeverPicksPhotosOfTheNext() async throws {
        let ai = GatedAI()
        let (m, store) = await openA(ai: ai)
        let culling = Task { await m.runAICull() }
        #expect(await waitUntil { ai.waiting })
        await m.open(store.b)
        ai.releaseAll()
        await culling.value
        #expect(m.photos.allSatisfy { $0.pick == .none }, "B's photos with the same ids stay unflagged")
        #expect(m.cullSuggestions.isEmpty && m.cullProgress == nil)
        #expect(!m.history.canUndo)
        m.stop()
    }

    @Test func autoAdjustStartedInOneSessionNeverEditsTheNext() async throws {
        let ai = GatedAI()
        let (m, store) = await openA(ai: ai)
        m.focus(PhotoID(1), extend: false)
        let adjusting = Task { await m.autoAdjust() }
        #expect(await waitUntil { ai.waiting })
        await m.open(store.b)
        ai.releaseAll()
        await adjusting.value
        #expect(m.edit(for: PhotoID(1)) == EditState() && m.edits.isEmpty)
        m.stop()
    }

    // MARK: Review follow-ups

    @Test func undoInTheNextSessionNeverMovesItsFilesBackFromTheFirstsMove() async throws {
        let (m, store) = await openA()
        m.focus(PhotoID(1), extend: false)
        m.setPick(.picked)
        m.history = EditHistory() // nothing undoable since the move: the move itself is what ⌘Z would undo
        await m.cull.movePickedToSelects()
        #expect(m.cull.undoesMove(historyDepth: 0))
        await m.open(store.b)
        #expect(m.cull.pendingDelete == nil && m.cull.pendingSelects == nil)
        #expect(!m.cull.undoesMove(historyDepth: 0))
        m.undo()
        await m.cull.pendingUndo?.value
        #expect(await store.movedBack.isEmpty, "B's same-numbered photo is never moved to A's recorded path")
        m.stop()
    }

    @Test func aBatchThatCouldNotBeSavedKeepsTheSwitchFromHappening() async throws {
        let (m, store) = await openA()
        await store.setFailSaves(true)
        m.focus(PhotoID(2), extend: false)
        m.setRating(3)
        await m.saver.flush()
        await m.open(store.b)
        #expect(m.currentSession?.name == "A" && !m.isSwitchingSession, "A stays open")
        #expect(m.toast?.isError == true && m.toast?.message.contains("couldn’t be saved") == true)
        await store.setFailSaves(false)
        await m.saver.flush()
        let saved = await store.savedPhotos
        #expect(saved.map(\.session) == [store.a.id], "A's rating reached A, never B's photo 2")
        await m.open(store.b)
        #expect(m.currentSession?.name == "B")
        m.stop()
    }

    @Test func aSwitchThatNeverFinishesIsGivenUpAndTheShownSessionWorksAgain() async throws {
        let (m, store) = await openA()
        m.toastDuration = .seconds(60) // the give-up toast is read after a slow wait
        let (deadline, giveUp) = AsyncStream<Void>.makeStream()
        m.switchDeadline = { for await _ in deadline { return } }
        await store.holdLoadOfB()
        m.focus(PhotoID(2), extend: false)
        let opening = Task { await m.open(store.b) }
        #expect(await waitUntil { m.isSwitchingSession })
        m.setRating(4)
        #expect(m.toast?.message.contains("Still opening") == true, "a refused change says so")
        #expect(m.photo(PhotoID(2))?.rating == 0)
        giveUp.yield()
        #expect(await waitUntil { !m.isSwitchingSession && !m.isLoading })
        #expect(m.currentSession?.name == "A" && m.toast?.message.contains("taking too long") == true)
        m.setRating(4)
        await m.saver.flush()
        #expect(await store.savedPhotos.map(\.session) == [store.a.id], "the store points at A again")
        await store.releaseB() // the hung load finally returns: it must not swap in
        await opening.value
        #expect(m.currentSession?.name == "A")
        m.stop()
    }

    @Test func switchingEndsSurveyForgetsRechecksAndRebuildsStacks() async throws {
        let (m, store) = await openA()
        m.browserState.refreshIfNeeded(version: m.photosVersion) { m.photos }
        let built = m.browserState.rebuilds
        m.recheckedOriginals = [PhotoID(2)]
        #expect(m.survey.begin(Array(m.visibleIDs.prefix(3))))
        await m.open(store.b)
        #expect(!m.survey.isActive)
        #expect(m.recheckedOriginals.isEmpty)
        // B has the same ids in the same order: A's stacks must still not be reused.
        m.browserState.refreshIfNeeded(version: m.photosVersion) { m.photos }
        #expect(m.browserState.rebuilds > built)
        m.stop()
    }

    @Test func styleTilesOfAnotherSessionsPhotoWithTheSameIdAreNotReused() async throws {
        let renders = StyleThumbnails(render: { _, _ in GatedPreviews().solid() })
        var a = photo(1), b = photo(1)
        a.url = URL(fileURLWithPath: "/tmp/A/IMG_1.ARW")
        b.url = URL(fileURLWithPath: "/tmp/B/IMG_1.ARW")
        let preset = Preset(name: "P", groups: [.exposure], edit: EditState())
        renders.refresh(photo: a, edit: EditState(), presets: [preset])
        #expect(await waitUntil { renders.images[preset.id] != nil })
        renders.refresh(photo: b, edit: EditState(), presets: [])
        #expect(renders.images.isEmpty, "A's tile must not show under B's photo")
    }

    @Test func anImportsAutoApplyLeavesAnotherOpenSessionAlone() async throws {
        let (m, store) = await openA()
        m.focus(PhotoID(1), extend: false)
        var e = EditState()
        e.exposure.ev = 1
        let preset = Preset(name: "P", groups: [.exposure], edit: e)
        #expect(ImportAutoApply.apply(preset, in: m, prior: [], session: store.b.id) == 0)
        #expect(m.edits.isEmpty)
        #expect(ImportAutoApply.apply(preset, in: m, prior: [], session: store.a.id) == 6)
        m.stop()
    }

    // MARK: ViewerPreviewCache

    private actor GatedPreviews: PreviewProvider {
        private var queue: [CheckedContinuation<Void, Never>] = []
        private(set) var calls = 0
        func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
            calls += 1
            await withCheckedContinuation { queue.append($0) }
            return solid()
        }
        func releaseOldest() { if !queue.isEmpty { queue.removeFirst().resume() } }
        nonisolated func solid() -> CGImage {
            let ctx = CGContext(data: nil, width: 4, height: 4, bitsPerComponent: 8, bytesPerRow: 0,
                                space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
            ctx.setFillColor(CGColor(red: 1, green: 0, blue: 0, alpha: 1))
            ctx.fill(CGRect(x: 0, y: 0, width: 4, height: 4))
            return ctx.makeImage()!
        }
    }

    private func photo(_ n: Int64) -> Photo {
        let meta = PhotoMeta(camera: "Cam", lens: "", iso: 100, shutter: 0.01, aperture: 2.8, focalLength: 35,
                             captureDate: Date(timeIntervalSince1970: 0), pixelWidth: 6000, pixelHeight: 4000, fileSize: 1)
        return Photo(id: PhotoID(n), filename: "P\(n).ARW", meta: meta, url: URL(fileURLWithPath: "/tmp/ds-p\(n).ARW"))
    }

    /// Waits (bounded) until the provider has been asked `n` times.
    private func providerAsked(_ provider: GatedPreviews, _ n: Int) async -> Bool {
        for _ in 0..<1000 {
            if await provider.calls >= n { return true }
            try? await Task.sleep(for: .milliseconds(5))
        }
        return false
    }

    @Test func aCancelledPreviewLoadDoesNotStealTheRestartedLoadsWaiter() async throws {
        let provider = GatedPreviews()
        let cache = ViewerPreviewCache(provider: provider)
        let p = photo(1)
        cache.load(p) { _ in }
        #expect(await providerAsked(provider, 1))
        cache.cancel(except: [])          // jumped away
        var shown: ViewerPreviewCache.Entry?
        var answered = false
        cache.load(p) { shown = $0; answered = true } // and back
        #expect(await providerAsked(provider, 2))
        // Let both decodes finish, the cancelled one first.
        await provider.releaseOldest()
        try await Task.sleep(for: .milliseconds(50))
        #expect(!answered, "the cancelled load's tail must not answer the restarted load's waiter")
        await provider.releaseOldest()
        #expect(await waitUntil { answered })
        #expect(shown != nil, "the restarted load's callback gets its preview, not nil")
        #expect(cache.entry(p.id) != nil && !cache.isLoadingAnything)
    }

    @Test func removeAllForgetsEveryFrameAndStopsLoads() async throws {
        let provider = GatedPreviews()
        let cache = ViewerPreviewCache(provider: provider)
        cache.load(photo(1)) { _ in }
        #expect(await providerAsked(provider, 1))
        await provider.releaseOldest()
        #expect(await waitUntil { cache.entry(PhotoID(1)) != nil })
        var answer: ViewerPreviewCache.Entry? = nil
        var answered = false
        cache.load(photo(2)) { answer = $0; answered = true }
        cache.removeAll()
        #expect(cache.entry(PhotoID(1)) == nil && cache.bytes == 0 && !cache.isLoadingAnything)
        #expect(answered && answer == nil, "a waiter on a dropped load hears nil")
    }

    // MARK: Image decode cap

    @Test func aFullDecodeOverThePixelBudgetIsRefused() throws {
        let gpu = GPUImaging(device: nil)
        let img = GatedPreviews().solid()
        let out = NSMutableData()
        let dest = try #require(CGImageDestinationCreateWithData(out, UTType.jpeg.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(dest, img, nil)
        #expect(CGImageDestinationFinalize(dest))
        let jpeg = out as Data
        #expect(gpu.decode(jpeg: jpeg, maxPixelSize: 0, exifOrientation: 1) != nil)
        gpu.maxPixels = 8 // the 4x4 source claims 16 pixels
        #expect(gpu.decode(jpeg: jpeg, maxPixelSize: 0, exifOrientation: 1) == nil)
        #expect(gpu.decode(jpeg: jpeg, maxPixelSize: 4, exifOrientation: 1) == nil)
    }

    // MARK: Create cleanup

    @Test func aFailedCreateLeavesAFolderSomeoneElseMadeAlone() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ds-switch-clean-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let dir = root.appendingPathComponent("Trip")
        let made = SessionLocation.missing(dir)
        // Another process made it (with its own files) between the check and our create.
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try Data("mine".utf8).write(to: dir.appendingPathComponent("IMG_0001.ARW"))
        SessionLocation.cleanUp(made)
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("IMG_0001.ARW").path))
        // A scaffold with a stray file inside a scaffold folder is also left.
        let dir2 = root.appendingPathComponent("Trip2")
        let made2 = SessionLocation.missing(dir2)
        try FileManager.default.createDirectory(at: dir2.appendingPathComponent("Capture"), withIntermediateDirectories: true)
        try Data("x".utf8).write(to: dir2.appendingPathComponent("Capture/a.ARW"))
        SessionLocation.cleanUp(made2)
        #expect(FileManager.default.fileExists(atPath: dir2.appendingPathComponent("Capture/a.ARW").path))
    }
}
