import Foundation
import Testing
@testable import DarkslideKit

/// Like DiskArbitration: replays every already-mounted volume as "appeared"
/// synchronously inside `start()` (N16).
final class ReplayingVolumeEvents: VolumeEventSource {
    var mounted: [URL] = []
    var onMount: (@MainActor (URL) -> Void)?
    var scanDelay: Duration = .milliseconds(20)

    @MainActor
    func start(onMount: @escaping @MainActor (URL) -> Void, onUnmount: @escaping @MainActor (URL) -> Void) {
        self.onMount = onMount
        for v in mounted { onMount(v) }
    }

    func stop() {}

    func mountedVolumes() async -> [URL] {
        try? await Task.sleep(for: scanDelay)
        return mounted
    }
}

@MainActor @Suite struct CardReplayTests {
    @Test func replayedMountsAreExistingVolumes() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let events = ReplayingVolumeEvents()
        events.mounted = [card.url]
        var new: [URL] = []
        var existing: [URL] = []
        let w = CardWatcher(source: events, detector: testDetector, onCard: { new.append($0) },
                            onExistingCard: { existing.append($0) }, onEject: { _ in })
        w.start()
        await w.settle()
        #expect(existing == [card.url], "replay at registration is not a new insertion")
        #expect(new.isEmpty)
        #expect(w.cards == [card.url])
        // A late replay of a volume the scan listed is still existing.
        #expect(w.isReplay(card.url))
        w.stop()
    }

    @Test func reinsertAfterUnmountIsNew() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let events = FakeVolumeEvents()
        events.mounted = [card.url]
        var new: [URL] = []
        let w = CardWatcher(source: events, detector: testDetector, onCard: { new.append($0) },
                            onExistingCard: { _ in }, onEject: { _ in })
        w.start()
        await w.settle()
        events.onUnmount?(card.url)
        events.onMount?(card.url)
        await w.settle()
        #expect(new == [card.url])
        w.stop()
    }

    @Test func alreadyOfferedCardStaysQuietDespiteReplay() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let store = MemoryOfferedStore()
        store.insert(card.url.standardizedFileURL.path)
        let base = makeModel()
        let app = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                           renderer: FakeRenderer(), importer: MockImportService(),
                           archive: MockArchiveService(target: "/nonexistent/x", isReachable: { _ in false }),
                           ai: MockAIService(), settings: base.settings, mode: .live)
        let events = ReplayingVolumeEvents()
        events.mounted = [card.url]
        let c = CardCoordinator(app: app, source: events, detector: testDetector, store: store,
                                inspector: StaticVolumeInspector([:]))
        c.start()
        await c.settle()
        #expect(app.sheet == nil, "a card offered on a previous launch must not reopen the sheet")
        c.stop()
    }
}
