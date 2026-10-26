import CoreGraphics
import LuxtableFFI
import Foundation
import ImageIO
import Testing
import UniformTypeIdentifiers
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// A merger whose timing and results the test controls.
private actor FakeMerger: PhotoMerging {
    var previewDelay: Duration
    var previewFailure: Error?
    var mergeResult: Result<PhotoID, Error> = .success(PhotoID(999))
    /// The merge waits until cancelled (after reporting 0.3).
    var mergeHangs = false
    var unaligned = 0
    private(set) var started: [PhotoMergeOptions] = []
    private(set) var finished: [PhotoMergeOptions] = []
    private(set) var cancelledPreviews = 0
    private(set) var merges: [(ids: [PhotoID], options: PhotoMergeOptions)] = []

    init(previewDelay: Duration = .zero) { self.previewDelay = previewDelay }

    func set(previewDelay: Duration? = nil, previewFailure: Error?? = nil, mergeResult: Result<PhotoID, Error>? = nil,
             mergeHangs: Bool? = nil, unaligned: Int? = nil) {
        if let unaligned { self.unaligned = unaligned }
        if let previewDelay { self.previewDelay = previewDelay }
        if let previewFailure { self.previewFailure = previewFailure }
        if let mergeResult { self.mergeResult = mergeResult }
        if let mergeHangs { self.mergeHangs = mergeHangs }
    }

    func preview(_ photos: [Photo], options: PhotoMergeOptions, maxEdge: Int) async throws -> PhotoMergePreview {
        started.append(options)
        do {
            try await Task.sleep(for: previewDelay)
        } catch {
            cancelledPreviews += 1
            throw error
        }
        if let previewFailure { throw previewFailure }
        finished.append(options)
        return PhotoMergePreview(image: try MockPhotoMerger.gradient(width: 8, height: 4), width: 6000, height: 4000,
                                 projection: options.kind == .hdr ? .perspective : .cylindrical,
                                 ghostRegions: options.deghost == .off ? 0 : 3, exposureEV: 0.25,
                                 unalignedFrames: unaligned)
    }

    func merge(_ photos: [PhotoID], in session: SessionInfo, options: PhotoMergeOptions,
               progress: @escaping @Sendable (Double) -> Void) async throws -> PhotoID {
        merges.append((photos, options))
        progress(0.3)
        if mergeHangs { try await Task.sleep(for: .seconds(3600)) }
        return try mergeResult.get()
    }
}

@MainActor
private final class Calls {
    var merged: [PhotoID] = []
    var closed = 0
}

@MainActor
@Suite struct MergeTests {
    let session = SessionInfo(name: "S", url: URL(fileURLWithPath: "/tmp/S"), photoCount: 2, createdAt: Date())

    private func model(_ merger: FakeMerger, kind: PhotoMergeKind = .hdr, debounce: Duration = .milliseconds(20),
                       calls: Calls) -> MergeModel {
        MergeModel(kind: kind, session: session, sources: [samplePhoto(1), samplePhoto(2)], merger: merger,
                   debounce: debounce, onMerged: { calls.merged.append($0) }, close: { calls.closed += 1 })
    }

    @Test func defaultsFollowLightroom() {
        let o = PhotoMergeOptions(kind: .panorama)
        #expect(o.align && o.deghost == .off && o.projection == .auto && o.autoCrop)
        #expect(PhotoMergeKind.hdr.title == "Merge to HDR" && PhotoMergeKind.panorama.title == "Merge to Panorama")
        #expect(PhotoMergeOptions.Deghost.allCases.map(\.title) == ["Off", "Low", "Medium", "High"])
        #expect(PhotoMergeOptions.Projection.allCases.map(\.title) == ["Auto", "Spherical", "Cylindrical", "Perspective"])
    }

    @Test func optionChangesPreviewAgainAndCancelTheOldPreview() async {
        let merger = FakeMerger(previewDelay: .milliseconds(300))
        let calls = Calls()
        let m = model(merger, calls: calls)
        m.start()
        #expect(m.isPreviewing)
        // Changes in quick succession: the running preview is cancelled, one more runs.
        m.options.align = false
        m.options.deghost = .low
        m.options.deghost = .high
        m.options.deghost = .high // unchanged: nothing new
        await merger.set(previewDelay: .milliseconds(1))
        #expect(await waitUntil { m.preview != nil && !m.isPreviewing })
        #expect(await merger.cancelledPreviews == 1, "the first preview was cancelled")
        let finished = await merger.finished
        #expect(finished.count == 1 && finished[0].deghost == .high && !finished[0].align)
        #expect(await merger.started.count == 2, "debounced: one preview for three changes")
        #expect(m.preview?.ghostRegions == 3)
        #expect(m.previewSummary == "6000 × 4000 · exposure +0.25 EV · 3 deghosted regions")
    }

    @Test func mergeReportsProgressThenRefreshesAndCloses() async {
        let merger = FakeMerger()
        let calls = Calls()
        let m = model(merger, calls: calls)
        m.start()
        #expect(await waitUntil { m.preview != nil })
        m.merge()
        #expect(m.isMerging && !m.canMerge)
        #expect(await waitUntil { calls.closed == 1 })
        #expect(calls.merged == [PhotoID(999)] && m.progress == nil && m.error == nil)
        let merges = await merger.merges
        #expect(merges.count == 1 && merges[0].ids == [PhotoID(1), PhotoID(2)] && merges[0].options.kind == .hdr)
    }

    @Test func cancelStopsAMergeThenClosesTheSheet() async {
        let merger = FakeMerger()
        await merger.set(mergeHangs: true)
        let calls = Calls()
        let m = model(merger, calls: calls)
        m.merge()
        #expect(await waitUntil { m.progress == 0.3 }, "progress reaches the sheet")
        m.options.align = false // (the sheet disables the options while merging)
        #expect(!m.isPreviewing, "no preview while merging")
        m.cancel()
        #expect(await waitUntil { !m.isMerging })
        #expect(calls.closed == 0 && calls.merged.isEmpty && m.error == nil, "cancelled: the sheet stays, no error")
        m.cancel()
        #expect(calls.closed == 1, "Cancel outside a merge closes the sheet")
    }

    @Test func errorsAreShownAndClearedByTheNextPreview() async {
        let merger = FakeMerger()
        await merger.set(previewFailure: .some(MockError.invalid("The photos don’t overlap.")))
        let calls = Calls()
        let m = model(merger, kind: .panorama, calls: calls)
        m.start()
        #expect(await waitUntil { m.error != nil })
        // A small preview can fail to stitch what the full-size merge stitches: Merge stays on.
        #expect(m.error == "The photos don’t overlap." && m.canMerge && !m.isPreviewing)
        await merger.set(previewFailure: .some(nil))
        m.options.projection = .spherical
        #expect(await waitUntil { m.preview != nil })
        #expect(m.error == nil && m.canMerge)
        await merger.set(mergeResult: .failure(MockError.invalid("disk full")))
        m.merge()
        #expect(await waitUntil { m.error == "disk full" })
        #expect(!m.isMerging && m.canMerge && calls.closed == 0, "a failed merge keeps the sheet and can be retried")
    }

    @Test func aFailedMergeRestartsTheInterruptedPreview() async {
        let merger = FakeMerger(previewDelay: .seconds(3600))
        await merger.set(mergeResult: .failure(MockError.invalid("no")))
        let calls = Calls()
        let m = model(merger, calls: calls)
        m.start()
        var tries = 0
        while await merger.started.isEmpty, tries < Int(5000 * TestTiming.scale) {
            tries += 1
            try? await Task.sleep(for: .milliseconds(2))
        }
        #expect(await merger.started.count == 1, "the first preview is running")
        await merger.set(previewDelay: .zero)
        m.merge()
        #expect(await waitUntil { m.preview != nil }, "previewed again after the failed merge")
        #expect(await merger.cancelledPreviews == 1 && m.error == "no")
    }

    @Test func kindSpecificTitles() async {
        let calls = Calls()
        let m = model(FakeMerger(), kind: .panorama, calls: calls)
        #expect(m.title == "Merge to Panorama (2 Photos)" && m.kind == .panorama)
        #expect(m.autoProjectionTitle == "Auto" && m.previewSummary == nil)
        m.start()
        #expect(await waitUntil { m.preview != nil })
        #expect(m.autoProjectionTitle == "Auto (Cylindrical)")
        #expect(m.previewSummary == "6000 × 4000 · exposure +0.25 EV", "no ghost regions for a panorama")
        let hdr = model(FakeMerger(), kind: .hdr, calls: calls)
        #expect(hdr.title == "Merge to HDR (2 Photos)")
        #expect(hdr == hdr && hdr != m)
    }

    // MARK: - App wiring

    @Test func menuNeedsTwoPhotosAndAMerger() async {
        let store = MockSessionStore(counts: [10])
        let m = makeModel(store: store)
        await m.start()
        let ids = m.visibleIDs
        m.focus(ids[0], extend: false)
        #expect(!CommandCatalog.isEnabled(.mergeHDR, model: m), "no merger")
        m.merger = MockPhotoMerger(store: store, step: .zero)
        #expect(!CommandCatalog.isEnabled(.mergeHDR, model: m) && !CommandCatalog.isEnabled(.mergePanorama, model: m),
                "one photo")
        m.toggleSelection(ids[1])
        #expect(CommandCatalog.isEnabled(.mergeHDR, model: m) && CommandCatalog.isEnabled(.mergePanorama, model: m))
        #expect(CommandCatalog.title(.mergeHDR, model: m) == "Merge to HDR…")
        #expect(CommandCatalog.title(.mergePanorama, model: m) == "Merge to Panorama…")
        #expect(MenuCommand.mergeHDR.shortcut == .init(key: "h", modifiers: [.command, .shift]))
        #expect(MenuCommand.mergePanorama.shortcut == .init(key: "m", modifiers: [.command, .shift]))
        let edit = KeyboardRouter.cheatSheet().first { $0.section == "Edit" }!.rows
        #expect(edit.contains { $0.keys == "⇧⌘H" && $0.action.contains("HDR") })
        #expect(edit.contains { $0.keys == "⇧⌘M" && $0.action.contains("Panorama") })
        #expect(CommandCatalog.mergeMenu == [.mergeHDR, .mergePanorama])
        m.sheet = .openSession
        #expect(!CommandCatalog.isEnabled(.mergeHDR, model: m), "not over another sheet")
        m.beginMerge(.hdr)
        #expect(m.sheet == .openSession)
        m.sheet = nil
        CommandCatalog.perform(.mergePanorama, model: m)
        guard case .merge(let sheet)? = m.sheet else {
            Issue.record("no merge sheet")
            return
        }
        #expect(sheet.kind == .panorama && sheet.sources.map(\.id) == [ids[0], ids[1]])
        #expect(ActiveSheet.merge(sheet).id == "merge")
        sheet.cancel()
        #expect(m.sheet == nil)
    }

    @Test func onlyRawFilesCanBeMerged() async throws {
        let store = MockSessionStore(counts: [10])
        let m = makeModel(store: store)
        m.merger = MockPhotoMerger(store: store, step: .zero)
        await m.start()
        let ids = m.visibleIDs
        let session = try #require(m.currentSession)
        let jpeg = try await store.addPhoto(after: ids[0], in: session, filename: "IMG_0001.JPG", meta: samplePhoto().meta)
        await m.refreshCurrentSession()
        m.focus(ids[0], extend: false)
        m.toggleSelection(jpeg.id)
        #expect(m.mergeSources.map(\.id) == [ids[0]])
        #expect(!CommandCatalog.isEnabled(.mergeHDR, model: m), "a JPEG doesn't count")
        m.toggleSelection(ids[1])
        #expect(CommandCatalog.isEnabled(.mergeHDR, model: m))
        #expect(m.mergeSources.map(\.id) == [ids[0], ids[1]])
    }

    @Test func unalignedFramesAreReported() async {
        let calls = Calls()
        let merger = FakeMerger()
        await merger.set(unaligned: 2)
        let m = model(merger, kind: .hdr, calls: calls)
        m.start()
        #expect(await waitUntil { m.preview != nil })
        #expect(m.previewSummary?.hasSuffix("2 frames not aligned") == true, "\(m.previewSummary ?? "")")
    }

    @Test func mergeAddsTheNewPhotoAndSelectsIt() async {
        let store = MockSessionStore(counts: [10])
        let m = makeModel(store: store)
        m.merger = MockPhotoMerger(store: store, step: .zero)
        await m.start()
        let ids = m.visibleIDs
        m.focus(ids[2], extend: false)
        m.toggleSelection(ids[3])
        CommandCatalog.perform(.mergeHDR, model: m)
        guard case .merge(let sheet)? = m.sheet else {
            Issue.record("no merge sheet")
            return
        }
        #expect(await waitUntil { sheet.preview != nil })
        #expect(sheet.preview?.projection == .perspective && sheet.preview?.exposureEV == 0.5)
        sheet.merge()
        #expect(await waitUntil { m.sheet == nil })
        #expect(m.photoCount == 11)
        let new = m.focusedPhoto
        #expect(new.map { !ids.contains($0.id) } == true && new?.filename.hasSuffix("-HDR.dng") == true)
        #expect(m.selection == Set(new.map { [$0.id] } ?? []))
    }

    // MARK: - The demo merger

    @Test func mockMergerSizesAndRefusals() async throws {
        let store = MockSessionStore(counts: [10])
        let merger = MockPhotoMerger(store: store, step: .zero)
        let photos = Array(MockSessionStore.makePhotos(count: 8, firstID: 1, start: Date()).filter { !$0.meta.isPortrait }.prefix(3))
        var o = PhotoMergeOptions(kind: .panorama)
        let w = photos.reduce(0) { $0 + $1.meta.pixelWidth }
        #expect(MockPhotoMerger.size(of: photos, options: o) == (Int(Double(w) * 0.8) * 95 / 100, 3600))
        o.autoCrop = false
        #expect(MockPhotoMerger.size(of: photos, options: o) == (Int(Double(w) * 0.8), 4000))
        let p = try await merger.preview(photos, options: o, maxEdge: 600)
        #expect(p.projection == .cylindrical && abs(max(p.image.width, p.image.height) - 600) <= 1 && p.ghostRegions == 0)
        o.projection = .perspective
        #expect(try await merger.preview(photos, options: o, maxEdge: 600).projection == .perspective)
        o.projection = .auto
        let many = MockSessionStore.makePhotos(count: 7, firstID: 1, start: Date())
        #expect(try await merger.preview(many, options: o, maxEdge: 600).projection == .spherical)
        var hdr = PhotoMergeOptions(kind: .hdr)
        hdr.deghost = .medium
        #expect(try await merger.preview(photos, options: hdr, maxEdge: 600).ghostRegions == 2)
        await #expect(throws: MockError.self) { try await merger.preview([photos[0]], options: hdr, maxEdge: 600) }
        let session = try await store.listSessions()[0]
        await #expect(throws: MockError.self) {
            try await merger.merge([PhotoID(1)], in: session, options: hdr) { _ in }
        }
        let other = SessionInfo(name: "gone", url: URL(fileURLWithPath: "/tmp/gone"), photoCount: 0, createdAt: Date())
        await #expect(throws: MockError.self) {
            try await store.addPhoto(after: PhotoID(1), in: other, filename: "x.dng", meta: photos[0].meta)
        }
    }

    // MARK: - The live merger (lt-ffi)

    @Test func liveOptionsMapToTheFFI() {
        for p in PhotoMergeOptions.Projection.allCases {
            #expect(PhotoMergeOptions.Projection(p.ffi) == p)
        }
        #expect(PhotoMergeOptions.Deghost.allCases.map(\.ffi) == [.off, .low, .medium, .high])
        var o = PhotoMergeOptions(kind: .panorama)
        o.align = false
        o.deghost = .low
        o.projection = .spherical
        o.autoCrop = false
        let f = o.ffi
        #expect(f.kind == .panorama && !f.align && f.deghost == .low && f.projection == .spherical && !f.autoCrop)
        #expect(PhotoMergeOptions(kind: .hdr).ffi.kind == .hdr)
    }

    @Test func liveDecodesTheJPEG() throws {
        #expect(RustPhotoMerger.decode(Data([1, 2, 3])) == nil)
        let data = NSMutableData()
        let dest = try #require(CGImageDestinationCreateWithData(data, UTType.jpeg.identifier as CFString, 1, nil))
        CGImageDestinationAddImage(dest, try MockPhotoMerger.gradient(width: 12, height: 6), nil)
        #expect(CGImageDestinationFinalize(dest))
        let image = try #require(RustPhotoMerger.decode(data as Data))
        #expect(image.width == 12 && image.height == 6)
    }

    @Test func liveErrors() async throws {
        let merger = RustPhotoMerger(handles: { _ in nil })
        do {
            _ = try await merger.preview([samplePhoto(1), samplePhoto(2)], options: .init(kind: .hdr), maxEdge: 800)
            Issue.record("a photo without a file can't be merged")
        } catch let e as RenderError {
            #expect(e.kind == .invalidArgument && e.message.contains("has no file"))
        }
        var a = samplePhoto(1), b = samplePhoto(2)
        a.url = URL(fileURLWithPath: "/nonexistent/a.ARW")
        b.url = URL(fileURLWithPath: "/nonexistent/b.ARW")
        await #expect(throws: (any Error).self) {
            try await merger.preview([a, b], options: .init(kind: .panorama), maxEdge: 800)
        }
        await #expect(throws: MockError.notFound("session S")) {
            try await merger.merge([PhotoID(1), PhotoID(2)], in: session, options: .init(kind: .hdr)) { _ in }
        }
    }

    /// Unknown photos in a real (empty) session: lt-session refuses, and the error reaches Swift.
    @Test func liveMergeOfUnknownPhotosFails() async throws {
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory.appendingPathComponent("lt-merge-\(UUID())")
        let dir = tmp.appendingPathComponent("folder")
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: tmp) }
        let store = RustSessionStore(recents: RecentFolders(defaults: UserDefaults(suiteName: testSuiteName())!),
                                     storeBase: tmp.appendingPathComponent("stores"))
        let info = try await store.add(folder: dir)
        let merger = RustPhotoMerger(handles: { await store.archiveHandle(for: $0) })
        await #expect(throws: RenderError.self) {
            try await merger.merge([PhotoID(1), PhotoID(2)], in: info, options: .init(kind: .hdr)) { _ in }
        }
    }
}
