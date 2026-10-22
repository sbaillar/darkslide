import CoreGraphics
import DiskArbitration
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

final class MemoryOfferedStore: OfferedVolumeStore {
    var ids: [String] = []
    func contains(_ id: String) -> Bool { ids.contains(id) }
    func insert(_ id: String) { if !ids.contains(id) { ids.append(id) } }
}

@MainActor
private func coordinatorHarness(mode: AppMode, mounted: [URL] = [], store: OfferedVolumeStore = MemoryOfferedStore())
    -> (AppModel, CardCoordinator, FakeVolumeEvents) {
    let app = makeModel()
    let live = AppModel(sessions: MockSessionStore(counts: [1]), images: MockImageProvider(latency: .zero),
                        renderer: FakeRenderer(), importer: MockImportService(),
                        archive: MockArchiveService(target: "/nonexistent/x", isReachable: { _ in false }),
                        ai: MockAIService(), settings: app.settings, mode: mode)
    let events = FakeVolumeEvents()
    events.mounted = mounted
    let c = CardCoordinator(app: live, source: events, detector: testDetector, store: store,
                            inspector: StaticVolumeInspector([:]))
    return (live, c, events)
}

@MainActor @Suite struct CardCoordinatorTests {
    @Test func liveModeOpensSheetForNewCard() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let (app, c, events) = coordinatorHarness(mode: .live)
        c.start()
        #expect(c.isRunning)
        events.onMount?(card.url)
        await c.settle()
        #expect(app.sheet == .importCard(card.url))
        c.stop()
    }

    @Test func demoModeNeverAutoOpens() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let (app, c, _) = coordinatorHarness(mode: .demo, mounted: [card.url])
        c.start()
        #expect(!c.isRunning)
        c.cardArrived(card.url, atLaunch: false)
        await c.settle()
        #expect(app.sheet == nil)
    }

    @Test func respectsAutoOpenSetting() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let (app, c, _) = coordinatorHarness(mode: .live)
        app.settings.autoOpenImport = false
        c.cardArrived(card.url, atLaunch: false)
        await c.settle()
        #expect(app.sheet == nil)
    }

    @Test func alreadyOfferedCardsAreNotReofferedAtLaunch() async throws {
        let card = try FakeVolume()
        defer { card.remove() }
        let store = MemoryOfferedStore()
        let (app, c, events) = coordinatorHarness(mode: .live, mounted: [card.url], store: store)
        c.start()
        await c.settle()
        #expect(app.sheet == .importCard(card.url), "first launch offers it")
        #expect(store.ids == [card.url.standardizedFileURL.path], "no UUID: path is the identity")
        c.stop()
        app.sheet = nil

        // Next launch with the same card still in the reader: silent.
        let (app2, c2, _) = coordinatorHarness(mode: .live, mounted: [card.url], store: store)
        c2.start()
        await c2.settle()
        #expect(app2.sheet == nil)
        // Re-inserting it while running always offers.
        c2.cardArrived(card.url, atLaunch: false)
        await c2.settle()
        #expect(app2.sheet == .importCard(card.url))
        _ = events
    }

    @Test func secondCardQueuesUntilSheetCloses() async throws {
        let a = try FakeVolume()
        let b = try FakeVolume()
        defer { a.remove(); b.remove() }
        let (app, c, _) = coordinatorHarness(mode: .live)
        c.cardArrived(a.url, atLaunch: false)
        await c.settle()
        c.cardArrived(b.url, atLaunch: false)
        c.cardArrived(b.url, atLaunch: false)
        await c.settle()
        #expect(app.sheet == .importCard(a.url))
        #expect(c.queue == [b.url])
        c.sheetDidClose() // sheet still open: nothing
        #expect(app.sheet == .importCard(a.url))
        app.sheet = nil
        c.sheetDidClose()
        #expect(app.sheet == .importCard(b.url))
        #expect(c.queue.isEmpty)
        app.sheet = nil
        c.sheetDidClose()
        #expect(app.sheet == nil)
    }

    @Test func removedCardLeavesQueueAndTellsTheUser() async throws {
        let a = try FakeVolume()
        let b = try FakeVolume()
        defer { a.remove(); b.remove() }
        let (app, c, events) = coordinatorHarness(mode: .live)
        c.start()
        events.onMount?(a.url)
        await c.settle()
        events.onMount?(b.url)
        await c.settle()
        #expect(c.queue == [b.url])
        events.onUnmount?(b.url)
        #expect(c.queue.isEmpty)
        events.onUnmount?(a.url)
        #expect(app.toast?.message.contains("removed") == true)
        c.stop()
    }

    @Test func userDefaultsStoreIsBoundedAndPersistent() {
        let suite = testSuiteName()
        let d = UserDefaults(suiteName: suite)!
        defer { d.removePersistentDomain(forName: suite) }
        let s = UserDefaultsOfferedStore(defaults: d)
        #expect(!s.contains("A"))
        s.insert("A")
        s.insert("A")
        #expect(UserDefaultsOfferedStore(defaults: d).contains("A"))
        for i in 0..<250 { s.insert("V\(i)") }
        #expect(!s.contains("A"), "oldest dropped past the limit")
        #expect(s.contains("V249"))
        #expect((d.stringArray(forKey: "offeredCardVolumes") ?? []).count == 200)
    }
}

@MainActor @Suite struct DiskArbitrationTests {
    @Test func descriptionParsing() {
        let url = URL(fileURLWithPath: "/Volumes/UNTITLED")
        let card = DiskDescription([kDADiskDescriptionVolumePathKey as String: url,
                                    kDADiskDescriptionVolumeNetworkKey as String: false])
        #expect(card.reportableVolume == url)
        let nas = DiskDescription([kDADiskDescriptionVolumePathKey as String: URL(fileURLWithPath: "/Volumes/pics"),
                                   kDADiskDescriptionVolumeNetworkKey as String: true])
        #expect(nas.isNetwork && nas.reportableVolume == nil)
        #expect(DiskDescription([:]).reportableVolume == nil)
        #expect(DiskDescription(volumePath: URL(fileURLWithPath: "/"), isNetwork: false).reportableVolume == nil)
    }

    @Test func transitions() {
        let a = URL(fileURLWithPath: "/Volumes/A")
        let b = URL(fileURLWithPath: "/Volumes/B")
        #expect(DiskArbitrationVolumeEvents.transition(previous: nil, current: a) == .mounted(a))
        #expect(DiskArbitrationVolumeEvents.transition(previous: a, current: nil) == .unmounted(a))
        #expect(DiskArbitrationVolumeEvents.transition(previous: a, current: b) == .mounted(b))
        #expect(DiskArbitrationVolumeEvents.transition(previous: a, current: a) == nil)
        #expect(DiskArbitrationVolumeEvents.transition(previous: nil, current: nil) == nil)
    }

    @Test func liveSessionStartsAndStops() async throws {
        let events = try #require(DiskArbitrationVolumeEvents())
        var mounted: [URL] = []
        events.start(onMount: { mounted.append($0) }, onUnmount: { _ in })
        // Existing disks appear asynchronously on the main queue; none is "/" or a network share.
        try await Task.sleep(for: .milliseconds(200))
        #expect(!mounted.contains(URL(fileURLWithPath: "/")))
        events.stop()
        events.stop() // idempotent
        let volumes = await events.mountedVolumes()
        #expect(!volumes.contains(URL(fileURLWithPath: "/")))
    }
}

@Suite struct DropRouterTests {
    let folder = URL(fileURLWithPath: "/Users/me/Shoot", isDirectory: true)
    let file = URL(fileURLWithPath: "/Users/me/Shoot/DSC0001.ARW")

    func isDir(_ u: URL) -> Bool { u == folder }

    @Test func liveOpensFoldersReadOnly() {
        #expect(DropRouter.route([folder], mode: .live, canOpenFolders: true, isDirectory: isDir) == .openFolder(folder))
        #expect(DropRouter.route([file], mode: .live, canOpenFolders: true, isDirectory: isDir) == .openFolder(folder))
        #expect(DropRouter.route([folder], mode: .live, canOpenFolders: false, isDirectory: isDir) == .ignore)
    }

    @Test func demoImports() {
        #expect(DropRouter.route([file], mode: .demo, canOpenFolders: false, isDirectory: isDir) == .importSheet(folder))
    }

    @Test func ignoresNonFiles() {
        #expect(DropRouter.route([], mode: .live, canOpenFolders: true) == .ignore)
        #expect(DropRouter.route([URL(string: "https://example.com")!], mode: .live, canOpenFolders: true) == .ignore)
        #expect(DropRouter.isDirectory(FileManager.default.temporaryDirectory))
        #expect(!DropRouter.isDirectory(URL(fileURLWithPath: "/nonexistent-\(UUID())")))
    }
}

struct FakePreviews: PreviewProvider {
    var fail = false
    func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
        if fail { throw MockError.invalid("no preview") }
        let ctx = CGContext(data: nil, width: maxPixelSize, height: maxPixelSize / 2, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        return ctx.makeImage()!
    }
}

@MainActor @Suite struct ImportThumbnailTests {
    @Test func loadsAndCaches() async {
        let t = ImportThumbnails(previews: FakePreviews(), maxPixelSize: 64)
        let url = URL(fileURLWithPath: "/Volumes/CARD/DCIM/1.ARW")
        #expect(t.isAvailable && t.cached(url) == nil)
        let a = await t.thumbnail(for: url)
        #expect(a?.width == 64)
        let b = await t.thumbnail(for: url)
        #expect(b === a && t.loads == 1)
    }

    @Test func unavailableOrFailingFallsBackToPlaceholder() async {
        let none = ImportThumbnails(previews: nil)
        #expect(!none.isAvailable)
        #expect(await none.thumbnail(for: URL(fileURLWithPath: "/x.ARW")) == nil)
        let failing = ImportThumbnails(previews: FakePreviews(fail: true))
        #expect(await failing.thumbnail(for: URL(fileURLWithPath: "/x.ARW")) == nil)
    }
}

@Suite struct ImportWordingTests {
    func summary(verified: Bool, ejected: Bool = false, ejectError: String? = nil, safe: Bool = false,
                 videos: Int = 0) -> ImportModel.ImportSummary {
        var s = ImportModel.ImportSummary(files: 3, bytes: 3_000_000, sessionName: "S", seconds: 5, ejected: ejected,
                                          verified: verified, ejectError: ejectError)
        s.checkedByService = true
        s.safeToRemove = safe
        s.needsAcknowledgement = !safe
        s.videos = videos
        return s
    }

    @Test func verifiedWording() {
        let s = summary(verified: true, safe: true)
        #expect(ImportWording.doneTitle(s) == "Imported 3 photos")
        #expect(ImportWording.doneTitle(summary(verified: true, videos: 1)) == "Imported 2 photos and 1 video")
        #expect(ImportWording.doneTitle(summary(verified: true, videos: 3)) == "Imported 3 videos")
        #expect(ImportWording.doneDetail(s).contains("3 files copied and verified"))
        #expect(ImportWording.cardLine(s)?.contains("safe to remove") == true)
        #expect(ImportWording.cardLine(summary(verified: true, ejected: true, safe: true))?.contains("safe to remove") == true)
        #expect(ImportWording.cardLine(summary(verified: true, ejected: true))?.contains("Keep it") == true,
                "ejected anyway: never called safe")
        #expect(ImportWording.cardLine(summary(verified: true))?.contains("Keep this card") == true)
        #expect(ImportWording.cardLine(summary(verified: true, ejectError: "busy")) == "Couldn’t eject the card: busy")
        #expect(ImportWording.importButton(photos: 1, videos: 0) == "Import 1 Photo")
        #expect(ImportWording.importButton(photos: 2, videos: 1) == "Import 2 Photos and 1 Video")
        #expect(ImportWording.importButton(photos: 0, videos: 0) == "Import")
        #expect(ImportWording.readyLine(bytes: 1, destination: "S", verifies: true).contains("copy and verify"))
    }

    @Test func ejectHelpAndReports() {
        #expect(ImportWording.ejectHelp(canEject: true).contains("verified"))
        #expect(ImportWording.ejectHelp(canEject: false).contains("local removable"))
        #expect(ImportReport(filesTotal: 2, filesCopied: 2, filesVerified: 2, bytesCopied: 1).allVerified)
        #expect(!ImportReport(filesTotal: 2, filesCopied: 1, filesVerified: 2, bytesCopied: 1).allVerified)
        #expect(!ImportReport(filesTotal: 0, filesCopied: 0, filesVerified: 0, bytesCopied: 0).allVerified)
    }
}
