import AppKit
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

private let utc = TimeZone(identifier: "UTC")!
// 2026-09-25 14:03:07 UTC
private let sampleDate = Date(timeIntervalSince1970: 1_790_344_987)

@Suite struct ImportTemplateTests {
    @Test func sessionNameUsesDateAndText() {
        #expect(ImportTemplates.sessionName("{date} {text}", date: sampleDate, text: " Fall Hike ", timeZone: utc)
            == "2026-09-25 Fall Hike")
    }

    @Test func sessionNameCollapsesMissingText() {
        #expect(ImportTemplates.sessionName("{date} {text}", date: sampleDate, text: "", timeZone: utc) == "2026-09-25")
        #expect(ImportTemplates.sessionName("{date}  {time}", date: sampleDate, text: "", timeZone: utc) == "2026-09-25 1403")
    }

    @Test func renameTokens() {
        let r = { (t: String, seq: Int) in
            ImportTemplates.rename(t, original: "DSC04200.ARW", date: sampleDate, seq: seq, camera: "Sony ILCE-9",
                                   text: "Hike", timeZone: utc)
        }
        #expect(r("{original}", 1) == "DSC04200.ARW")
        #expect(r("{camera}_{date}_{seq4}", 7) == "Sony-ILCE-9_20260925_0007.ARW")
        #expect(r("{date}_{time}_{seq}", 12) == "20260925_140307_012.ARW")
        #expect(r("{text}_{seq}", 3) == "Hike_003.ARW")
        #expect(r("{unknown}", 1) == "{unknown}.ARW")
    }

    @Test func renameWithoutExtension() {
        #expect(ImportTemplates.rename("{original}-x", original: "README", date: sampleDate, seq: 1, camera: "", text: "",
                                       timeZone: utc) == "README-x")
    }

    @Test func sanitizeRemovesPathCharacters() {
        #expect(ImportTemplates.sanitize("a/b:c?d") == "a-b-c-d")
        #expect(ImportTemplates.sanitize("  .hidden. ") == "hidden")
        #expect(ImportTemplates.sanitize(" ... ") == "Untitled")
        #expect(ImportTemplates.sessionName("{text}", date: sampleDate, text: "Trip/Day 1", timeZone: utc) == "Trip-Day 1")
    }
}

@Suite struct TransferStatsTests {
    @Test func unknownUntilTwoSamples() {
        var s = TransferStats()
        #expect(s.bytesPerSecond == nil)
        #expect(s.eta(totalBytes: 100) == nil)
        s.record(bytes: 0, at: sampleDate)
        #expect(s.bytesPerSecond == nil)
    }

    @Test func rateAndEta() throws {
        var s = TransferStats(window: 3)
        s.record(bytes: 0, at: sampleDate)
        s.record(bytes: 200, at: sampleDate.addingTimeInterval(1))
        s.record(bytes: 400, at: sampleDate.addingTimeInterval(2))
        #expect(s.bytesPerSecond == 200)
        #expect(try #require(s.eta(totalBytes: 1000)) == 3)
        #expect(s.eta(totalBytes: 100) == 0)
    }

    @Test func movingWindowForgetsOldSamples() {
        var s = TransferStats(window: 3)
        s.record(bytes: 0, at: sampleDate)
        // Slow start, then fast: the average should follow the recent rate.
        s.record(bytes: 10, at: sampleDate.addingTimeInterval(10))
        for i in 1...5 { s.record(bytes: 10 + Int64(i) * 1000, at: sampleDate.addingTimeInterval(10 + Double(i))) }
        #expect(s.samples.count <= 5)
        #expect(abs((s.bytesPerSecond ?? 0) - 1000) < 1)
    }

    @Test func restartClearsHistory() {
        var s = TransferStats()
        s.record(bytes: 500, at: sampleDate)
        s.record(bytes: 900, at: sampleDate.addingTimeInterval(1))
        s.record(bytes: 0, at: sampleDate.addingTimeInterval(2))
        #expect(s.samples.count == 1)
        #expect(s.bytesPerSecond == nil)
    }

    @Test func zeroRateHasNoEta() {
        var s = TransferStats()
        s.record(bytes: 5, at: sampleDate)
        s.record(bytes: 5, at: sampleDate.addingTimeInterval(1))
        #expect(s.eta(totalBytes: 10) == nil)
    }
}

/// A temp folder shaped like a camera card.
struct FakeVolume {
    let url: URL

    init(files: [String] = ["100MSDCF/DSC00001.ARW", "100MSDCF/DSC00002.ARW", "100MSDCF/DSC00002.JPG", "MISC/notes.txt"],
         dcim: Bool = true) throws {
        url = FileManager.default.temporaryDirectory.appendingPathComponent("ds-card-\(UUID().uuidString)")
        let root = dcim ? url.appendingPathComponent("DCIM") : url.appendingPathComponent("Photos")
        for f in files {
            let file = root.appendingPathComponent(f)
            try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data(repeating: 7, count: 1024).write(to: file)
        }
        if files.isEmpty { try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true) }
    }

    func remove() { try? FileManager.default.removeItem(at: url) }
}

@MainActor
final class FakeVolumeEvents: VolumeEventSource {
    var mounted: [URL] = []
    var onMount: (@MainActor (URL) -> Void)?
    var onUnmount: (@MainActor (URL) -> Void)?
    var stopped = 0

    func start(onMount: @escaping @MainActor (URL) -> Void, onUnmount: @escaping @MainActor (URL) -> Void) {
        self.onMount = onMount
        self.onUnmount = onUnmount
    }

    func stop() { stopped += 1 }
    func mountedVolumes() async -> [URL] { mounted }
}

/// Folders count as local cards when they have DCIM/.
let testDetector = DCIMDetector(isDirectory: DCIMDetector.directoryExists)

@MainActor @Suite struct CardWatcherTests {
    @Test func detectorNeedsDCIMDirectory() async throws {
        let card = try FakeVolume()
        let folder = try FakeVolume(dcim: false)
        defer { card.remove(); folder.remove() }
        #expect(await DCIMDetector.folders.isCard(card.url))
        #expect(await !DCIMDetector.folders.isCard(folder.url))
        // A file named DCIM is not a card.
        let file = FileManager.default.temporaryDirectory.appendingPathComponent("ds-file-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: file, withIntermediateDirectories: true)
        try Data().write(to: file.appendingPathComponent("DCIM"))
        defer { try? FileManager.default.removeItem(at: file) }
        #expect(await !DCIMDetector.folders.isCard(file))
    }

    @Test func networkVolumesAreSkippedWithoutTouchingThem() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let touched = TouchCounter()
        let nas = StaticVolumeInspector([card.url.standardizedFileURL.path: VolumeTraits(isLocal: false, isEjectable: false, isRemovable: false)])
        let d = DCIMDetector(isDirectory: { touched.hit(); return DCIMDetector.directoryExists($0) }, inspector: nas)
        #expect(await !d.isCard(card.url))
        #expect(touched.count == 0, "the DCIM path on a network share is never stat'ed")
        let unknown = DCIMDetector(isDirectory: { _ in true }, inspector: StaticVolumeInspector([:]))
        #expect(await !unknown.isCard(card.url), "no traits (timed out) = not a card")
        let local = DCIMDetector(isDirectory: DCIMDetector.directoryExists,
                                 inspector: StaticVolumeInspector([card.url.standardizedFileURL.path: VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true)]))
        #expect(await local.isCard(card.url))
    }

    /// Disk images (test and dev images mounted -nobrowse) are never offered on their own;
    /// the import sheet the user opened still finds them.
    @Test func diskImagesAreNotOfferedAsCards() async throws {
        #expect(DiskDescription.isDiskImage(["DAMediaModel": "x", "DADeviceProtocol": "Virtual Interface"]))
        #expect(DiskDescription.isDiskImage(["DADeviceModel": "Disk Image "]))
        #expect(!DiskDescription.isDiskImage(["DADeviceProtocol": "USB", "DADeviceModel": "SD Card Reader"]))
        #expect(!DiskDescription.isDiskImage([:]))
        let card = try FakeVolume()
        defer { card.remove() }
        let image = StaticVolumeInspector([card.url.standardizedFileURL.path:
                                           VolumeTraits(isLocal: true, isEjectable: true, isRemovable: true, isDiskImage: true)])
        let auto = DCIMDetector(isDirectory: DCIMDetector.directoryExists, inspector: image, skipsDiskImages: true)
        let asked = DCIMDetector(isDirectory: DCIMDetector.directoryExists, inspector: image)
        #expect(await !auto.isCard(card.url))
        #expect(await asked.isCard(card.url))
        #expect(DCIMDetector.fileSystem.skipsDiskImages && !DCIMDetector.fileSystemIncludingImages.skipsDiskImages)
        // A real exFAT image with DCIM/: DiskArbitration says it's an image.
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-dmg-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        guard let img = await TestCardImage.make(in: dir, name: "DSIMG", sizeMB: 32) else { return }
        try FileManager.default.createDirectory(at: img.mount.appendingPathComponent("DCIM"), withIntermediateDirectories: true)
        let traits = await FileSystemVolumeInspector(timeout: 5).traits(of: img.mount)
        #expect(traits?.isDiskImage == true, "\(String(describing: traits))")
        #expect(await !DCIMDetector.fileSystem.isCard(img.mount), "never auto-offered")
        #expect(await DCIMDetector.fileSystemIncludingImages.isCard(img.mount), "the sheet still finds it")
        _ = await img.detach()
    }

    @Test func hungStatTimesOut() async {
        // The stat hangs until released after the check, and would say "card": a `false`
        // can only come from the 0.1 s timeout (no wall-clock bound to flake under load).
        let hung = DispatchSemaphore(value: 0)
        let d = DCIMDetector(isDirectory: { _ in hung.wait(); return true }, timeout: 0.1)
        #expect(await !d.isCard(URL(fileURLWithPath: "/Volumes/hung")))
        hung.signal()
        let v = await blockingWithTimeout(1) { 42 }
        #expect(v == 42)
    }

    @Test func reportsMountedCardsOnStartAndNewMounts() async throws {
        let card = try FakeVolume()
        let card2 = try FakeVolume()
        let other = try FakeVolume(dcim: false)
        defer { card.remove(); card2.remove(); other.remove() }
        let events = FakeVolumeEvents()
        events.mounted = [card.url, other.url]
        var cards: [URL] = []
        var existing: [URL] = []
        var ejected: [URL] = []
        let w = CardWatcher(source: events, detector: testDetector, onCard: { cards.append($0) },
                            onExistingCard: { existing.append($0) }, onEject: { ejected.append($0) })
        w.start()
        w.start() // idempotent
        await w.settle()
        #expect(existing == [card.url])
        #expect(cards.isEmpty)
        events.onMount?(other.url)
        events.onMount?(card.url) // duplicate mount notification
        events.onMount?(card2.url)
        await w.settle()
        #expect(cards == [card2.url])
        #expect(w.cards == [card.url, card2.url])
        events.onUnmount?(card.url)
        #expect(ejected == [card.url])
        #expect(w.cards == [card2.url])
        w.stop()
        w.stop()
        #expect(events.stopped == 1)
        #expect(!w.isRunning)
        #expect(await CardWatcher.mountedCards(source: events, detector: testDetector) == [card.url])
    }

    @Test func existingCardsDefaultToOnCard() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let events = FakeVolumeEvents()
        events.mounted = [card.url]
        var cards: [URL] = []
        let w = CardWatcher(source: events, detector: testDetector, onCard: { cards.append($0) }, onEject: { _ in })
        w.start()
        await w.settle()
        #expect(cards == [card.url])
        w.stop()
    }

    @Test func workspaceEventsForwardNotifications() async throws {
        let center = NotificationCenter()
        let events = WorkspaceVolumeEvents(center: center)
        let card = try FakeVolume()
        defer { card.remove() }
        var mounted: [URL] = []
        var unmounted: [URL] = []
        events.start(onMount: { mounted.append($0) }, onUnmount: { unmounted.append($0) })
        center.post(name: NSWorkspace.didMountNotification, object: nil,
                    userInfo: [NSWorkspace.volumeURLUserInfoKey: card.url])
        center.post(name: NSWorkspace.didMountNotification, object: nil, userInfo: [:]) // ignored
        center.post(name: NSWorkspace.didUnmountNotification, object: nil,
                    userInfo: [NSWorkspace.volumeURLUserInfoKey: card.url])
        #expect(mounted == [card.url])
        #expect(unmounted == [card.url])
        events.stop()
        center.post(name: NSWorkspace.didMountNotification, object: nil,
                    userInfo: [NSWorkspace.volumeURLUserInfoKey: card.url])
        #expect(mounted.count == 1)
        #expect(await !events.mountedVolumes().contains(URL(fileURLWithPath: "/")))
    }

    @Test func defaultInitUsesDiskArbitration() async {
        let w = CardWatcher(onCard: { _ in }, onEject: { _ in })
        w.start()
        #expect(w.isRunning)
        await w.settle()
        w.stop()
    }
}

final class TouchCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var n = 0
    func hit() { lock.withLock { n += 1 } }
    var count: Int { lock.withLock { n } }
}

@Suite struct MockImportServiceTests {
    @Test func scansRealDCIMFolder() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let found = try await MockImportService().scan(volume: card.url)
        #expect(found.map(\.filename) == ["DSC00001.ARW", "DSC00002.ARW", "DSC00002.JPG"])
        #expect(found.allSatisfy { $0.size == 1024 && !$0.alreadyImported })
    }

    @Test func inventsARollForEmptyCards() async throws {
        let found = try await MockImportService(alreadyImportedEvery: 5, inventedCount: 12)
            .scan(volume: URL(fileURLWithPath: "/nonexistent-card"))
        #expect(found.count == 12)
        #expect(found.filter(\.alreadyImported).count == 3)
    }

    @Test func failAfterSimulatesPulledCard() async throws {
        let svc = MockImportService(bytesPerSecond: 1e12, inventedCount: 4, failAfter: 2)
        let files = try await svc.scan(volume: URL(fileURLWithPath: "/x"))
        let plan = ImportPlan(candidates: files, destination: .newSession(name: "x"), renameTemplate: "", backupFolder: nil)
        var last: ImportProgress?
        await #expect(throws: MockError.self) {
            for try await p in svc.run(plan) { last = p }
        }
        #expect(last?.filesDone == 2)
    }
}
