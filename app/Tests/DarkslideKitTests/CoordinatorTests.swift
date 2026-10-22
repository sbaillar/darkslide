import AppKit
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Regression tests for the review round: storage (M4), filter anchoring (H2),
/// auto-advance/sort (UX4), saves (M6/M7), keyboard (M5/L6/UX9/M10), live mode (C1).
@MainActor
@Suite struct CullingFlowTests {
    @Test func focusStaysInPlaceWhenPhotoLeavesFilter() async {
        let m = makeModel()
        await m.start()
        m.filter = PhotoFilter(pick: .unrejected)
        let ids = m.visibleIDs
        m.focus(ids[5], extend: false)
        m.perform(.reject)
        #expect(!m.visibleIDs.contains(ids[5]))
        #expect(m.focusedID == ids[6], "H2: the next photo takes its place, not index 0")
        #expect(m.selection == [ids[6]])
        m.focus(m.visibleIDs.last, extend: false)
        let last = m.focusedID!
        m.perform(.reject)
        #expect(m.focusedID == m.visibleIDs.last && m.focusedID != last, "at the end: previous photo")
    }

    @Test func autoAdvanceAfterRatingAndFlagging() async {
        let m = makeModel()
        m.settings.autoAdvance = true
        await m.start()
        let ids = m.visibleIDs
        m.perform(.rate(3))
        #expect(m.photo(ids[0])?.rating == 3)
        #expect(m.focusedID == ids[1])
        m.perform(.pick)
        #expect(m.focusedID == ids[2])
        m.perform(.tag(.red))
        #expect(m.focusedID == ids[2], "tags don't advance")
        m.perform(.move(.right, extend: true))
        m.perform(.rate(2))
        #expect(m.focusedID == ids[3], "multi-selection: no advance")
    }

    @Test func itemsUpdateInPlaceWithoutListChanges() async {
        let m = makeModel()
        await m.start()
        let before = m.visibleIDs
        let item = m.item(m.focusedID!)!
        let version = m.photosVersion
        m.perform(.rate(4))
        #expect(item.photo.rating == 4, "the observed item mutates in place")
        #expect(m.visibleIDs == before)
        #expect(m.photosVersion == version + 1)
        #expect(m.item(PhotoID(-1)) == nil)
    }

    @Test func sortOrders() async {
        let m = makeModel()
        await m.start()
        m.sort = .filename
        let names = m.visiblePhotos.map(\.filename)
        #expect(names == names.sorted())
        m.sort = .rating
        let ratings = m.visiblePhotos.map(\.rating)
        #expect(ratings == ratings.sorted(by: >))
        m.focus(m.visibleIDs.last, extend: false)
        m.perform(.rate(5))
        let fives = m.visiblePhotos.prefix { $0.rating == 5 }.map(\.id)
        #expect(fives.contains(m.focusedID!), "rating sort re-sorts on change")
        m.sort = .captureTime
        let dates = m.visiblePhotos.map(\.meta.captureDate)
        #expect(dates == dates.sorted())
    }

    @Test func unratedAndUnflaggedFilters() async {
        let m = makeModel()
        await m.start()
        m.filter = PhotoFilter(unratedOnly: true)
        #expect(m.visiblePhotos.allSatisfy { $0.rating == 0 })
        m.filter = PhotoFilter(pick: .unflagged)
        m.perform(.pick)
        #expect(m.visiblePhotos.allSatisfy { $0.pick == PickState.none })
    }

    @Test func panelsToggleAndEscape() async {
        let m = makeModel()
        await m.start()
        m.perform(.togglePanels)
        #expect(m.panelsHidden)
        m.perform(.escape)
        #expect(!m.panelsHidden)
    }
}

@MainActor
@Suite struct SaveCoordinatorTests {
    @Test func debouncesIntoOneWrite() async {
        let store = MockSessionStore(counts: [3])
        let s = SaveCoordinator(store: store, debounce: .milliseconds(30))
        var e = EditState()
        for i in 0..<20 {
            e.exposure.ev = Double(i) / 10
            s.enqueue(edits: [PhotoID(1): e])
        }
        #expect(s.state == .pending(1))
        #expect(s.hasUnsaved)
        #expect(await waitUntil { s.state == .saved })
        #expect(await store.saveCount == 1, "20 slider ticks → 1 write")
        #expect(try! await store.edit(for: PhotoID(1)).exposure.ev == 1.9)
        #expect(s.writes == 1 && !s.hasUnsaved)
    }

    @Test func failureStaysVisibleUntilRetrySucceeds() async {
        let store = ToggleFailStore()
        let s = SaveCoordinator(store: store, debounce: .milliseconds(5))
        s.enqueue(photos: [samplePhoto(1)])
        #expect(await waitUntil { if case .failed = s.state { true } else { false } })
        #expect(s.state == .failed("disk full", pending: 1))
        s.enqueue(photos: [samplePhoto(2)])
        #expect(s.state == .failed("disk full", pending: 2), "stays failed, counts new changes")
        await s.flush()
        #expect(s.state == .failed("disk full", pending: 2))
        store.fail = false
        await s.retry()
        #expect(s.state == .saved)
        #expect(store.saved.count == 2)
        await s.flush()
        #expect(s.state == .saved, "nothing pending")
    }

    @Test func modelShowsUnsavedStateInsteadOfToast() async {
        let m = makeModel(store: SaveFailingStore())
        await m.start()
        m.setRating(2)
        await m.saver.flush()
        #expect(m.saver.state == .failed("disk full", pending: 1))
    }
}

/// Fails every write while `fail` is true; records successful writes.
final class ToggleFailStore: SessionStore, @unchecked Sendable {
    var fail = true
    var saved: [Photo] = []
    func listSessions() async throws -> [SessionInfo] { [] }
    func photos(in session: SessionInfo) async throws -> [Photo] { [] }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { throw MockError.invalid("no") }
    func save(photos: [Photo]) async throws {
        if fail { throw MockError.invalid("disk full") }
        saved += photos
    }
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws { if fail { throw MockError.invalid("disk full") } }
}

@MainActor
@Suite struct KeyBindingsTests {
    @Test func defaultsMatchTheCheatSheet() {
        let b = KeyBindings.defaults
        #expect(b.command(for: "p") == .pick && b.command(for: "P") == .pick)
        #expect(b.command(for: "6") == .tag(.red) && b.command(for: "-") == .tag(.purple))
        #expect(b.command(for: "q") == nil)
        let unboundByDefault: Set<BindableCommand> = [.tagOrange, .tagPink, .brushTool, .eraserTool, .gradientTool, .radialTool]
        #expect(BindableCommand.allCases.allSatisfy { (b.key(for: $0) != nil || unboundByDefault.contains($0)) && !$0.title.isEmpty && $0.id == $0.rawValue })
    }

    @Test func rebindingDisplacesAndRoundTrips() {
        var b = KeyBindings.defaults
        #expect(b.bind(.pick, to: "X") == .reject, "x belonged to reject")
        #expect(b.command(for: "x") == .pick && b.key(for: .reject) == nil)
        #expect(b.bind(.reject, to: "z") == nil)
        b.unbind(.unflag)
        #expect(b.command(for: "u") == nil)
        let back = KeyBindings(dictionary: b.dictionary)
        #expect(back == b)
        #expect(KeyBindings(dictionary: ["pick": "pp", "nope": "q"]) == .defaults, "invalid entries fall back to the defaults (N15 merge)")
        #expect(KeyboardRouter.command(for: KeyInput("z"), bindings: b) == .reject)
    }

    @MainActor
    @Test func bindingsPersistInSettings() {
        let s = makeSettings()
        var b = s.keyBindings
        b.bind(.pick, to: "k")
        s.keyBindings = b
        #expect(s.keyBindings.command(for: "k") == .pick)
    }

    @Test func menuTitlesShowPlainKeys() {
        #expect(MenuActionsTitle.title(.pick) == "Pick\tP")
        var b = KeyBindings.defaults
        b.unbind(.pick)
        #expect(MenuActionsTitle.title(.pick, b) == "Pick")
    }
}

@MainActor
enum MenuActionsTitle {
    static func title(_ c: BindableCommand, _ b: KeyBindings = .defaults) -> String { MenuActions.title(c, bindings: b) }
}

@MainActor
@Suite struct KeyRoutingTests {
    @Test func sharedEditActionsRespectTextEditing() async {
        let m = makeModel()
        await m.start()
        m.setRating(3)
        #expect(MenuActions.perform(.undo, model: m, textEditing: true) == Selector(("undo:")))
        #expect(m.focusedPhoto?.rating == 3, "text field gets ⌘Z, not the photo")
        #expect(MenuActions.perform(.undo, model: m, textEditing: false) == nil)
        #expect(m.focusedPhoto?.rating != 3)
        #expect(MenuActions.perform(.redo, model: m, textEditing: false) == nil)
        #expect(MenuActions.perform(.selectAll, model: m, textEditing: false) == nil)
        #expect(m.selection.count == 40, "⌘A selects every visible photo")
        #expect(MenuActions.perform(.selectAll, model: m, textEditing: true) == #selector(NSText.selectAll(_:)))
        #expect(MenuActions.isEnabled(.undo, model: m, textEditing: false))
        #expect(MenuActions.isEnabled(.redo, model: m, textEditing: true))
        #expect(MenuActions.isEnabled(.selectAll, model: m, textEditing: false))
    }

    @Test func focusedControlsKeepNavigationKeys() {
        let left = KeyInput("", special: .left)
        #expect(KeyMonitorPolicy.shouldHandle(left, focus: .control, sheetOpen: false, isMainWindow: true) == nil)
        #expect(KeyMonitorPolicy.shouldHandle(KeyInput(" ", special: .space), focus: .control, sheetOpen: false,
                                              isMainWindow: true) == nil)
        #expect(KeyMonitorPolicy.shouldHandle(KeyInput("3"), focus: .control, sheetOpen: false, isMainWindow: true) == .rate(3))
        #expect(KeyMonitorPolicy.shouldHandle(KeyInput("", special: .escape), focus: .control, sheetOpen: false,
                                              isMainWindow: true) == .escape)
        #expect(KeyMonitorPolicy.shouldHandle(left, focus: .none, sheetOpen: false, isMainWindow: true) == .move(.left, extend: false))
    }

    @Test func responderClassification() {
        #expect(FocusedResponder.classify(nil) == FocusedResponder.none)
        #expect(FocusedResponder.classify(NSTextView()) == .text)
        #expect(FocusedResponder.classify(NSTextField()) == .text)
        #expect(FocusedResponder.classify(NSSlider()) == .control)
        #expect(FocusedResponder.classify(NSView()) == FocusedResponder.none)
    }

    @Test func handleKeyUsesBindingsAndTracksRepeat() async {
        let m = makeModel()
        await m.start()
        var b = m.settings.keyBindings
        b.bind(.pick, to: "k")
        m.settings.keyBindings = b
        #expect(m.handleKey(KeyInput("k"), focus: .none, sheetOpen: false, isMainWindow: true))
        #expect(m.focusedPhoto?.pick == .picked)
        #expect(!m.handleKey(KeyInput("k"), focus: .text, sheetOpen: false, isMainWindow: true))
        #expect(m.handleKey(KeyInput("", special: .right, isRepeat: true), focus: .none, sheetOpen: false, isMainWindow: true))
        #expect(m.fastNavigation)
        m.setKeyRepeat(false)
        #expect(!m.fastNavigation)
        m.sheet = .onboarding
        #expect(!m.handleKey(KeyInput("k"), focus: .none, sheetOpen: false, isMainWindow: true), "sheets own the keyboard")
    }

    @Test func digitRowIsLayoutIndependent() throws {
        // AZERTY: the "1" key produces "&" unshifted.
        let e = try #require(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                                              windowNumber: 0, context: nil, characters: "&",
                                              charactersIgnoringModifiers: "&", isARepeat: true, keyCode: 18))
        let k = KeyInput(event: e)
        #expect(k.characters == "1" && k.isRepeat)
        let tab = try #require(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                                                windowNumber: 0, context: nil, characters: "\t",
                                                charactersIgnoringModifiers: "\t", isARepeat: false, keyCode: 48))
        #expect(KeyInput(event: tab).special == .tab)
    }
}

@MainActor
@Suite struct LiveModeTests {
    func tempFolder(_ names: [String]) throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("ds-folder-\(UUID())")
        try FileManager.default.createDirectory(at: dir.appendingPathComponent("DCIM/100MSDCF"), withIntermediateDirectories: true)
        for n in names {
            let sub = n.hasPrefix("DSC") ? "DCIM/100MSDCF/" : ""
            try Data([1, 2, 3]).write(to: dir.appendingPathComponent(sub + n))
        }
        return dir
    }

    func store(_ recents: RecentFolders, failing: Set<String> = []) -> FolderSessionStore {
        FolderSessionStore(recents: recents, concurrency: 2, isSupported: { $0.pathExtension.lowercased() == "arw" }) { url, id in
            if failing.contains(url.lastPathComponent) { throw MockError.invalid("bad file") }
            var p = samplePhoto(id.raw)
            p.filename = url.lastPathComponent
            p.url = url
            p.meta.captureDate = Date(timeIntervalSince1970: Double(url.lastPathComponent.hashValue % 1000))
            return p
        }
    }

    @Test func folderSessionsLoadRealFilesInMemoryOnly() async throws {
        let dir = try tempFolder(["a.ARW", "b.arw", "notes.txt", "DSC0001.ARW", "bad.ARW"])
        defer { try? FileManager.default.removeItem(at: dir) }
        let d = UserDefaults(suiteName: testSuiteName())!
        let recents = RecentFolders(defaults: d)
        let s = store(recents, failing: ["bad.ARW"])
        #expect(!s.persistsChanges)
        let info = try await s.add(folder: dir)
        #expect(info.photoCount == 4 && info.name == dir.lastPathComponent)
        #expect(info.id == FolderSessionStore.sessionID(for: dir), "stable id per path")
        let photos = try await s.photos(in: info)
        #expect(Set(photos.map(\.filename)) == ["a.ARW", "b.arw", "DSC0001.ARW"])
        #expect(await s.lastFailures.map(\.lastPathComponent) == ["bad.ARW"])
        #expect(try await s.photos(in: info) == photos, "cached")
        var p = photos[0]
        p.rating = 5
        try await s.save(photos: [p])
        #expect(try await s.photos(in: info).first { $0.id == p.id }?.rating == 5)
        var e = EditState()
        e.exposure.ev = 1
        try await s.save(edits: [p.id: e])
        #expect(try await s.edit(for: p.id) == e)
        #expect(try await s.albums(in: info).isEmpty)
        await #expect(throws: MockError.self) { try await s.createSession(named: "x") }
        await #expect(throws: MockError.self) { try await s.add(folder: dir.appendingPathComponent("nope")) }
        // A new store rediscovers the folder from recents.
        let again = try await store(recents).listSessions()
        #expect(again.map(\.id) == [info.id])
        #expect(recents.folders.map(\.standardizedFileURL.path) == [dir.standardizedFileURL.path])
    }

    @Test func recentsAreBoundedAndDeduplicated() {
        let r = RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!, limit: 2)
        r.add(URL(fileURLWithPath: "/a"))
        r.add(URL(fileURLWithPath: "/b"))
        r.add(URL(fileURLWithPath: "/a"))
        r.add(URL(fileURLWithPath: "/c"))
        #expect(r.folders.map(\.path) == ["/c", "/a"])
    }

    @Test func liveServicesNeverPretend() async throws {
        let dir = try tempFolder(["DSC0001.ARW", "DSC0002.JPG", "x.txt"])
        defer { try? FileManager.default.removeItem(at: dir) }
        let scan = try await ReadOnlyCardScanner().scan(volume: dir)
        #expect(scan.map(\.filename) == ["DSC0001.ARW", "DSC0002.JPG"], "real files only, nothing invented")
        let plan = ImportPlan(candidates: scan, destination: .newSession(name: "x"), renameTemplate: "{original}", backupFolder: nil)
        await #expect(throws: ImportUnavailable.self) {
            for try await _ in ReadOnlyCardScanner().run(plan) {}
        }
        // L3: user wording, no crate names.
        #expect(ImportUnavailable().errorDescription?.contains("isn't available") == true)
        #expect(ImportUnavailable().errorDescription?.contains("ds-") == false)
        var states: [ArchiveStatus] = []
        for await s in UnavailableArchiveService(target: "/x").statusUpdates() { states.append(s) }
        #expect(states.count == 1 && states[0].lastSuccess == nil)
        if case .unavailable(let m) = states[0].state { #expect(m.contains("connected")) } else { Issue.record("expected unavailable") }
        await UnavailableArchiveService(target: "/x").syncNow()
    }

    @Test func embeddedPreviewRendererDrawsThePreview() async throws {
        struct Solid: PreviewProvider {
            func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
                let ctx = CGContext(data: nil, width: 4, height: 2, bitsPerComponent: 8, bytesPerRow: 0,
                                    space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
                ctx.setFillColor(CGColor(srgbRed: 1, green: 0, blue: 0, alpha: 1))
                ctx.fill(CGRect(x: 0, y: 0, width: 4, height: 2))
                return ctx.makeImage()!
            }
        }
        let r = EmbeddedPreviewRenderer(previews: Solid())
        await #expect(throws: MockError.self) { try await r.render(samplePhoto(), edit: EditState(), maxPixelSize: 4) }
        var p = samplePhoto()
        p.url = URL(fileURLWithPath: "/x.ARW")
        let f = try await r.render(p, edit: EditState(), request: RenderRequest(maxPixelSize: 4, scale: 1))
        #expect(f.width == 4 && f.height == 2 && Array(f.rgba.prefix(3)) == [255, 0, 0])
    }

    @Test func previewThumbnailsNeedAFile() async throws {
        struct LocalPreviews: PreviewProvider {
            func preview(for url: URL, maxPixelSize: Int) async throws -> CGImage {
                MockImageProvider.draw(seed: 1, width: maxPixelSize, height: maxPixelSize)!
            }
        }
        let t = PreviewThumbnailProvider(previews: LocalPreviews())
        await #expect(throws: MockError.self) { try await t.thumbnail(for: samplePhoto(), maxPixelSize: 32) }
        var p = samplePhoto()
        p.url = URL(fileURLWithPath: "/x.ARW")
        #expect(try await t.thumbnail(for: p, maxPixelSize: 32).width == 32)
    }

    @Test func openFolderInLiveModeAndDemoRefusal() async throws {
        let dir = try tempFolder(["a.ARW", "b.ARW"])
        defer { try? FileManager.default.removeItem(at: dir) }
        let recents = RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!)
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel(sessions: store(recents), images: MockImageProvider(latency: .zero), renderer: FakeRenderer(),
                         importer: ReadOnlyCardScanner(), archive: UnavailableArchiveService(target: ""),
                         ai: MockAIService(), settings: settings, mode: .live)
        m.watchCards = false
        #expect(!m.persistsChanges)
        await m.openFolder(dir)
        #expect(m.currentSession?.url == dir)
        #expect(m.visibleIDs.count == 2)
        let demo = makeModel()
        #expect(demo.persistsChanges)
        await demo.openFolder(dir)
        #expect(demo.toast?.message.contains("--demo") == true)
    }

    @Test func copyOptionsPersist() {
        let suite = testSuiteName()
        let s = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        s.copyGroups = [.exposure, .crop]
        s.copyRelativeGroups = [.exposure]
        let r = AppSettings(defaults: UserDefaults(suiteName: suite)!)
        #expect(r.copyGroups == [.exposure, .crop] && r.copyRelativeGroups == [.exposure])
        #expect(r.archiveTarget.isEmpty, "L5: no hardcoded archive default")
    }
}

@MainActor
@Suite struct AppFactoryTests {
    @Test func modeFromArguments() {
        #expect(AppModel.mode(arguments: ["Darkslide"]) == .live)
        #expect(AppModel.mode(arguments: ["Darkslide", "--demo"]) == .demo)
    }

    @Test func liveWiringUsesRealServices() {
        let m = AppModel.make(mode: .live, settings: makeSettings(), defaults: UserDefaults(suiteName: testSuiteName())!)
        #expect(m.mode == .live && m.previews != nil && m.persistsChanges, "live saves to sidecars now")
        #expect(m.sessions is RustSessionStore && m.importer is RustImportService) // [imp6] ds-import
        #expect(m.archive is RustArchiveService && m.renderer is DevelopRenderer) // [arc6] ds-archive, [vw8] ds-pipeline
    }

    @Test func demoWiringUsesMocks() {
        let m = AppModel.make(mode: .demo, settings: makeSettings())
        #expect(m.mode == .demo && m.previews == nil)
        #expect(m.sessions is MockSessionStore && m.importer is MockImportService && m.archive is MockArchiveService)
    }

    @Test func liveOpensRealSamplesWhenPresent() async throws {
        // Never open the shared samples folder itself: opening creates `.ds/` there.
        guard let dir = try sampleFolderCopy() else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let m = AppModel.make(mode: .live, settings: makeSettings(), defaults: UserDefaults(suiteName: testSuiteName())!)
        m.watchCards = false
        await m.openFolder(dir)
        #expect(m.visibleIDs.count >= 1)
        let p = try #require(m.focusedPhoto)
        #expect(p.meta.camera.contains("ILCE-9") && p.url != nil)
        let thumb = try await m.images.thumbnail(for: p, maxPixelSize: 256)
        #expect(max(thumb.width, thumb.height) == 256)
        let frame = try await m.renderer.render(p, edit: EditState(), maxPixelSize: 800)
        // [vw8] a develop render; the width is trimmed to whole texture rows (zero-copy), ≤ 4 px.
        #expect(max(frame.width, frame.height) <= 800 && max(frame.width, frame.height) >= 794 && frame.format == .rgba8Srgb,
                "\(frame.width)x\(frame.height)")
        #expect(p.meta.isPortrait == (frame.height > frame.width), "oriented like the photo")
    }
}

@MainActor
@Suite struct MissingVisibilityTests {
    @Test func missingPhotosHiddenByDefaultShownUnderMissingFilter() async {
        let store = MissingStore()
        let m = makeModel(store: store)
        await m.start()
        #expect(m.photoCount == 3 && m.missingCount == 1)
        #expect(m.visibleIDs.count == 2, "missing original hidden in the default view")
        m.filter.missingOnly = true
        #expect(m.visibleIDs == [PhotoID(2)])
    }
}

/// Three photos, #2 missing.
struct MissingStore: SessionStore {
    let session = SessionInfo(name: "S", url: URL(fileURLWithPath: "/tmp/s"), photoCount: 3, createdAt: Date())
    func listSessions() async throws -> [SessionInfo] { [session] }
    func photos(in session: SessionInfo) async throws -> [Photo] {
        (1...3).map { i in
            var p = samplePhoto(Int64(i))
            p.meta.captureDate = Date(timeIntervalSince1970: Double(i))
            p.isMissing = i == 2
            return p
        }
    }
    func albums(in session: SessionInfo) async throws -> [Album] { [] }
    func createSession(named name: String) async throws -> SessionInfo { session }
    func save(photos: [Photo]) async throws {}
    func edit(for id: PhotoID) async throws -> EditState { EditState() }
    func save(edits: [PhotoID: EditState]) async throws {}
}
