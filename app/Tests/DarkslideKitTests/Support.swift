import CoreGraphics
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Instant renderer that fills frames with a solid color derived from the edit,
/// counts calls, and can be gated to test render coalescing.
final class FakeRenderer: Renderer, @unchecked Sendable {
    private let lock = NSLock()
    private var _calls: [(PhotoID, EditState, Int)] = []
    private var gate: CheckedContinuation<Void, Never>?
    var gated = false
    var failWith: Error?

    var calls: [(PhotoID, EditState, Int)] { lock.withLock { _calls } }

    private var _requests: [RenderRequest] = []
    var requests: [RenderRequest] { lock.withLock { _requests } }

    func render(_ photo: Photo, edit: EditState, request: RenderRequest) async throws -> RenderedFrame {
        let maxPixelSize = request.maxPixelSize
        lock.withLock {
            _calls.append((photo.id, edit, maxPixelSize))
            _requests.append(request)
        }
        if lock.withLock({ gated }) {
            await withCheckedContinuation { c in lock.withLock { gate = c } }
        }
        if let failWith = lock.withLock({ self.failWith }) { throw failWith }
        let v = UInt8(clamping: 128 + Int(edit.exposure.ev * 40))
        let w = 8, h = 6
        var px = [UInt8](repeating: 255, count: w * h * 4)
        for i in stride(from: 0, to: px.count, by: 4) { px[i] = v; px[i + 1] = 100; px[i + 2] = 50 }
        return RenderedFrame(width: w, height: h, rgba: Data(px))
    }

    var isWaiting: Bool { lock.withLock { gate != nil } }

    func release() {
        let g = lock.withLock { () -> CheckedContinuation<Void, Never>? in
            let g = gate
            gate = nil
            return g
        }
        g?.resume()
    }

    func setGated(_ on: Bool) { lock.withLock { gated = on } }
}

struct FailingStore: SessionStore {
    func listSessions() async throws -> [SessionInfo] { throw MockError.notFound("sessions") }
    func photos(in session: SessionInfo) async throws -> [Photo] { throw MockError.notFound("photos") }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { throw MockError.invalid("no") }
    func save(photos: [Photo]) async throws { throw MockError.invalid("disk full") }
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws { throw MockError.invalid("disk full") }
}

@MainActor
func makeSettings() -> AppSettings {
    let suite = testSuiteName()
    let d = UserDefaults(suiteName: suite)!
    d.removePersistentDomain(forName: suite)
    return AppSettings(defaults: d)
}

@MainActor
func makeModel(counts: [Int] = [40, 5], renderer: FakeRenderer = FakeRenderer(),
               store: (any SessionStore)? = nil, onboarded: Bool = true) -> AppModel {
    let settings = makeSettings()
    settings.onboardingComplete = onboarded
    settings.autoAdvance = false
    let model = AppModel(
        sessions: store ?? MockSessionStore(counts: counts),
        images: MockImageProvider(latency: .zero),
        renderer: renderer,
        importer: MockImportService(bytesPerSecond: 1e12),
        archive: MockArchiveService(target: "/nonexistent/archive", interval: .seconds(3600), step: .milliseconds(1),
                                    isReachable: { _ in true }),
        ai: MockAIService(),
        settings: settings)
    model.watchCards = false
    return model
}

/// Poll until `condition` holds (async work hops through the main actor).
///
/// The budget only matters when a test is failing: a passing wait returns as soon as the
/// condition holds. 15 s, because in a full parallel suite on a loaded machine a step that
/// takes ~1 s alone was measured at 10+ s (zoomRequestsTrueOneToOneTiles failed at the
/// old 3 s budget while its earlier 15 s wait passed). Scaled by `TestTiming.scale`.
@MainActor
func waitUntil(timeout: Duration = .seconds(15), _ condition: @MainActor () -> Bool) async -> Bool {
    let clock = ContinuousClock()
    let deadline = clock.now + timeout * TestTiming.scale
    while !condition() {
        if clock.now > deadline { return false }
        try? await Task.sleep(for: .milliseconds(2))
    }
    return true
}

func samplePhoto(_ id: Int64 = 1, portrait: Bool = false) -> Photo {
    Photo(id: PhotoID(id), filename: "DSC0\(id).ARW", meta: PhotoMeta(
        camera: "Sony ILCE-9", lens: "FE 85mm F1.4 GM", iso: 200, shutter: 1.0 / 250, aperture: 2.8,
        focalLength: 85, captureDate: Date(timeIntervalSince1970: 1_790_000_000),
        pixelWidth: portrait ? 4000 : 6000, pixelHeight: portrait ? 6000 : 4000, fileSize: 48_000_000))
}

/// Loads like the mock but every save fails.
struct SaveFailingStore: SessionStore {
    let inner = MockSessionStore(counts: [3])
    func listSessions() async throws -> [SessionInfo] { try await inner.listSessions() }
    func photos(in session: SessionInfo) async throws -> [Photo] { try await inner.photos(in: session) }
    func albums(in session: SessionInfo) async throws -> [Album] { try await inner.albums(in: session) }
    func createSession(named name: String) async throws -> SessionInfo { try await inner.createSession(named: name) }
    func save(photos: [Photo]) async throws { throw MockError.invalid("disk full") }
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws { throw MockError.invalid("disk full") }
}

/// Real Sony A9 samples for live-path tests (`$DS_SAMPLES_DIR`, the repo's
/// `samples/`, or ~/src/darkslide/samples). Empty when absent: tests that need
/// them return early, like `ds_testdata::sample_arw()`.
func sampleARWs() -> [URL] {
    let fm = FileManager.default
    var dirs: [URL] = []
    if let env = ProcessInfo.processInfo.environment["DS_SAMPLES_DIR"] { dirs.append(URL(fileURLWithPath: env)) }
    let repo = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        .appendingPathComponent("../../../samples").standardizedFileURL
    dirs += [repo, URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("src/darkslide/samples")]
    for d in dirs {
        let files = ((try? fm.contentsOfDirectory(at: d.resolvingSymlinksInPath(), includingPropertiesForKeys: nil)) ?? [])
            .filter { $0.pathExtension.lowercased() == "arw" }
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
        if !files.isEmpty { return files }
    }
    return []
}

/// A fresh temp folder holding copies of the samples (APFS clones, so free), so
/// live tests can create `.ds/` session data without touching the real samples folder.
func sampleFolderCopy(limit: Int = 8) throws -> URL? {
    let src = sampleARWs().prefix(limit)
    guard !src.isEmpty else { return nil }
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-live-\(UUID())")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    for u in src {
        let dst = dir.appendingPathComponent(u.lastPathComponent)
        try FileManager.default.copyItem(at: u, to: dst)
    }
    return dir
}

/// Timing in tests that must not flake on a loaded machine (review M3): bound a
/// synchronous step by the thread's **CPU time** (preemption doesn't count), and scale
/// wall-clock bounds by how oversubscribed the machine is.
enum TestTiming {
    /// CPU time of the calling thread.
    static func threadCPU() -> Duration {
        .nanoseconds(Int64(clock_gettime_nsec_np(CLOCK_THREAD_CPUTIME_ID)))
    }

    /// CPU time `body` takes on this thread.
    static func cpu(_ body: () -> Void) -> Duration {
        let c = threadCPU()
        body()
        return threadCPU() - c
    }

    /// ≥ 1: the 1-minute load average per core (1 on an idle machine).
    static var loadFactor: Double {
        var l = [0.0, 0, 0]
        guard getloadavg(&l, 3) > 0 else { return 1 }
        return max(1, l[0] / Double(ProcessInfo.processInfo.activeProcessorCount))
    }

    /// ≥ 1: how much slower than a developer Mac this machine runs the suite
    /// (`DS_TEST_TIME_SCALE`; CI's 3-core runners run ~1,900 parallel tests). Hang
    /// detectors and wall-clock bounds stretch by it; CPU-time budgets don't need to.
    static let scale: Double = max(1, Double(ProcessInfo.processInfo.environment["DS_TEST_TIME_SCALE"] ?? "") ?? 1)
}
