import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

private func fixPhoto(_ n: Int64, name: String? = nil) -> Photo {
    let meta = PhotoMeta(camera: "Cam", lens: "", iso: 100, shutter: 0.01, aperture: 2.8, focalLength: 35,
                         captureDate: Date(timeIntervalSince1970: 1_790_000_000), pixelWidth: 6000, pixelHeight: 4000,
                         fileSize: 1)
    return Photo(id: PhotoID(n), filename: name ?? "P\(n).ARW", meta: meta)
}

/// A store that repeats an id, as a damaged index could.
private actor DuplicateIDStore: SessionStore {
    let session = SessionInfo(name: "Dup", url: URL(fileURLWithPath: "/tmp/ds-dup"), photoCount: 3, createdAt: Date())
    func listSessions() async throws -> [SessionInfo] { [session] }
    func photos(in session: SessionInfo) async throws -> [Photo] {
        [fixPhoto(1, name: "first.ARW"), fixPhoto(2), fixPhoto(1, name: "second.ARW")]
    }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { throw MockError.invalid("no") }
    func save(photos: [Photo]) async throws {}
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
}

/// Serves a link session per store session, the first one slowly.
private actor LinkStore: SessionStore, LinkSessionProviding {
    let a = SessionInfo(name: "A", url: URL(fileURLWithPath: "/tmp/ds-link-A"), photoCount: 1, createdAt: Date())
    let b = SessionInfo(name: "B", url: URL(fileURLWithPath: "/tmp/ds-link-B"), photoCount: 1, createdAt: Date())
    func listSessions() async throws -> [SessionInfo] { [a, b] }
    func photos(in session: SessionInfo) async throws -> [Photo] { [fixPhoto(1)] }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { throw MockError.invalid("no") }
    func save(photos: [Photo]) async throws {}
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
    func markDeviceEditsSeen(in session: SessionInfo) async {}
    func linkSession(for session: SessionInfo) async -> LinkSession? {
        // A's lookup is slow: it finishes after B's.
        if session.id == a.id { try? await Task.sleep(for: .milliseconds(150)) }
        return LinkSession(root: session.url, name: session.name, handle: NSObject())
    }
}

/// Opens a folder only when told to; a cancelled open throws.
private actor SlowFolderStore: FolderSessionOpening {
    private var released: Set<String> = []
    func listSessions() async throws -> [SessionInfo] { [] }
    func photos(in session: SessionInfo) async throws -> [Photo] { [] }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { throw MockError.invalid("no") }
    func save(photos: [Photo]) async throws {}
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
    var lastFailures: [URL] { [] }
    func removeMissing(_ ids: [PhotoID]) async throws {}
    func locate(_ id: PhotoID, at url: URL) async throws {}
    func restorePrevious(_ id: PhotoID) async throws {}
    func dismissFileChanged(_ id: PhotoID) async throws {}
    func restoreToFolder(_ id: PhotoID) async throws {}
    func relocate(_ session: SessionInfo, to folder: URL) async throws -> SessionInfo { throw MockError.invalid("no") }

    func finish(_ name: String) { released.insert(name) }
    func add(folder: URL, progress: @escaping @Sendable (OpenProgress) -> Void) async throws -> SessionInfo {
        while !released.contains(folder.lastPathComponent) {
            try Task.checkCancellation()
            try await Task.sleep(for: .milliseconds(5))
        }
        return SessionInfo(name: folder.lastPathComponent, url: folder, photoCount: 0, createdAt: Date())
    }
}

/// The first scan waits to be released; later ones answer at once.
private final class OverlappingScans: ImportService, @unchecked Sendable {
    private let lock = NSLock()
    private var calls = 0
    private var gate: CheckedContinuation<Void, Never>?
    var asked: Int { lock.withLock { calls } }
    func scan(volume: URL) async throws -> [ImportCandidate] { try await scanCard(volume: volume).candidates }
    func scanCard(volume: URL) async throws -> ImportScan {
        let n = lock.withLock { () -> Int in
            calls += 1
            return calls
        }
        if n == 1 { await withCheckedContinuation { c in lock.withLock { gate = c } } }
        let files = (0..<(n == 1 ? 1 : 2)).map {
            ImportCandidate(url: volume.appendingPathComponent("DCIM/\($0).ARW"), size: 1,
                            captureDate: Date(timeIntervalSince1970: Double($0)), alreadyImported: false)
        }
        return ImportScan(candidates: files, volumeID: n == 1 ? "OLD" : "NEW")
    }
    func releaseFirst() { lock.withLock { gate }?.resume() }
    func run(_ plan: ImportPlan) -> AsyncThrowingStream<ImportProgress, Error> { AsyncThrowingStream { $0.finish() } }
}

@MainActor
@Suite struct SessionSwitchFixesTests {
    @Test func anOlderScanFinishingLastDoesNotReplaceTheNewerCard() async throws {
        let service = OverlappingScans()
        let m = ImportModel(volume: URL(fileURLWithPath: "/Volumes/CARD"), importer: service, sessions: MockSessionStore(counts: [1]),
                            existingSessions: [], nameTemplate: "{date}", renameTemplate: "{original}") { _ in }
        let older = Task { await m.scan() }
        for _ in 0..<1000 where service.asked < 1 { try await Task.sleep(for: .milliseconds(5)) }
        await m.scan() // the card was mounted again: this scan answers first
        #expect(m.volumeID == "NEW" && m.candidates.count == 2)
        service.releaseFirst()
        await older.value
        #expect(m.volumeID == "NEW" && m.candidates.count == 2, "the stale scan must not put the old card back")
    }

    @Test func aSupersededFolderOpenLeavesTheNewerOnesProgressAlone() async throws {
        let store = SlowFolderStore()
        let m = makeModel(store: store)
        m.settings.sessionClosed = true
        await m.start()
        let one = URL(fileURLWithPath: "/tmp/ds-open-one"), two = URL(fileURLWithPath: "/tmp/ds-open-two")
        let first = Task { await m.openFolder(one) }
        #expect(await waitUntil { m.openingFolder == one })
        let second = Task { await m.openFolder(two) }
        #expect(await waitUntil { m.openingFolder == two })
        await first.value // the first open ends (cancelled)
        #expect(m.openingFolder == two && m.isLoading && m.openProgress != nil, "the sheet stays up for the newer open")
        #expect(m.toast?.message.contains("cancelled") != true)
        m.cancelOpen() // still reaches the newer open
        await second.value
        #expect(!m.isLoading && m.openingFolder == nil)
        #expect(m.toast?.message.contains("ds-open-two") == true)
        m.stop()
    }

    @Test func aStoreThatRepeatsAnIdLoadsWithoutTrapping() async {
        let store = DuplicateIDStore()
        let m = makeModel(store: store)
        m.settings.sessionClosed = true
        await m.start()
        await m.open(store.session)
        #expect(m.photos.count == 2 && m.photos.map(\.filename).sorted() == ["P2.ARW", "first.ARW"])
        m.sort = .rating // re-sorting rebuilds the index from `order`
        #expect(m.visibleIDs.count == 2)
        m.stop()
    }

    @Test func theLinkServesTheLatestSessionEvenWhenAnOlderLookupFinishesLast() async throws {
        let store = LinkStore()
        let m = makeModel(store: store)
        m.settings.sessionClosed = true
        await m.start()
        await m.open(store.a)
        await m.open(store.b)
        #expect(await waitUntil { m.remote.served?.name == "B" })
        try await Task.sleep(for: .milliseconds(400)) // A's slow lookup has finished by now
        #expect(m.remote.served?.name == "B", "A's late lookup didn't take it back")
        m.stop()
    }

    @Test func oneKeyOnTwoCommandsGoesToTheFirstOnlyAndIsNotRandom() {
        for _ in 0..<20 {
            let b = KeyBindings(dictionary: ["pick": "x", "reject": "x", KeyBindings.versionKey: "2"])
            #expect(b.key(for: .pick) == "x" && b.key(for: .reject) == nil)
        }
    }

    @Test func aSnapshotRunNeverWipesADomainThatIsNotItsOwn() throws {
        let name = "ds-fix-foreign-\(UUID().uuidString)"
        let foreign = try #require(UserDefaults(suiteName: name))
        foreign.set("keep", forKey: "k")
        defer { foreign.removePersistentDomain(forName: name) }
        _ = AppModel.defaults(arguments: ["D", "--snapshot", "x.png", "--defaults-suite", name])
        #expect(foreign.string(forKey: "k") == "keep")
        // Its own snapshot suite is still emptied.
        let own = try #require(UserDefaults(suiteName: AppModel.snapshotSuite))
        own.set("stale", forKey: "k")
        _ = AppModel.defaults(arguments: ["D", "--snapshot", "x.png"])
        #expect(own.string(forKey: "k") == nil)
        own.removePersistentDomain(forName: AppModel.snapshotSuite)
    }

    @Test func aBenchPlanWithANegativeCountIsRefusedAtLoad() throws {
        let step = BenchPlan.Step(firstChange: .exposure, stimuli: [.click(x: 0.5, y: 0.5)], count: -1, interval: 1)
        let plan = BenchPlan(app: "A", process: "A", viewer: .init(x: 0, y: 0, w: 1, h: 1), steps: [step])
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("ds-plan-\(UUID()).json")
        defer { try? FileManager.default.removeItem(at: url) }
        try JSONEncoder().encode(plan).write(to: url)
        #expect(throws: DecodingError.self) { try BenchPlan.load(url) }
        var ok = plan
        ok.steps[0].count = 2
        try JSONEncoder().encode(ok).write(to: url)
        #expect(try BenchPlan.load(url) == ok)
    }

    @Test func anAppNameThatCouldBreakOutOfTheScriptIsRefused() async {
        let step = BenchPlan.Step(firstChange: .exposure, stimuli: [.script(source: "tell application \"{app}\" to x")],
                                  count: 1, interval: 1)
        let plan = BenchPlan(app: "A", process: "A", viewer: .init(x: 0, y: 0, w: 1, h: 1), steps: [step],
                             documentCheck: "tell application \"{app}\" to name of front document")
        for bad in ["C1\" & (do shell script \"x\") & \"", "C1\\"] {
            let screen = ScriptedBenchScreen()
            await #expect(throws: BenchDriver.DriveError.badAppName) {
                _ = try await BenchDriver(plan: plan, screen: screen, appName: bad, expectDocument: "Bench")
                    .run(run: 1, mode: .cold) { _ in }
            }
            #expect(screen.sent.isEmpty)
        }
        #expect(BenchDriver.DriveError.badAppName.description.contains("--app-name"))
    }
}
