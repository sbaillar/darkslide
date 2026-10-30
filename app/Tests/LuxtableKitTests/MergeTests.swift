import CoreGraphics
import LuxtableAI
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
                                 unalignedFrames: unaligned,
                                 facesReplaced: options.kind == .burst && !options.bestShot.isEmpty ? 1 : 0)
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
        #expect(edit.contains { $0.keys == "⇧⌘B" && $0.action.contains("Best Shot") })
        #expect(CommandCatalog.mergeMenu == [.mergeHDR, .mergePanorama, .mergeBestShot])
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

    // MARK: - Best Shot

    @Test func bestShotFindsTheFacesBeforeItsFirstPreview() async {
        let merger = FakeMerger()
        let calls = Calls()
        let face = { (x: Double, open: Double) in
            BestShotFrame.Face(bounds: CGRect(x: x, y: 0.2, width: 0.2, height: 0.3), eyesOpen: open, quality: 0.6)
        }
        let frames = [BestShotFrame(faces: [face(0.4, 0.1)], score: 0.8), BestShotFrame(faces: [face(0.41, 0.9)])]
        let gate = AsyncStream<Void>.makeStream()
        let m = MergeModel(kind: .burst, session: session, sources: [samplePhoto(1), samplePhoto(2)], merger: merger,
                           debounce: .milliseconds(20), analyze: { photos in
                               for await _ in gate.stream { break }
                               return photos.count == 2 ? frames : []
                           }, onMerged: { calls.merged.append($0) }, close: { calls.closed += 1 })
        #expect(m.title == "Merge to Best Shot (2 Photos)")
        m.start()
        #expect(m.isAnalyzing && m.isPreviewing && !m.canMerge)
        #expect(m.previewSummary == "Looking for faces\u{2026}")
        m.merge()
        #expect(!m.isMerging, "not before the faces are found")
        gate.continuation.yield()
        #expect(await waitUntil { m.preview != nil && !m.isPreviewing })
        #expect(!m.isAnalyzing && m.canMerge && m.options.bestShot == frames)
        let started = await merger.started
        #expect(started.count == 1 && started.first?.bestShot == frames, "one preview, with the faces")
        #expect(m.previewSummary == "6000 × 4000 · exposure +0.25 EV · 1 face improved")
        m.merge()
        #expect(await waitUntil { calls.closed == 1 })
        #expect(await merger.merges.first?.options.bestShot == frames)
    }

    @Test func bestShotWithoutFacesStillMerges() async {
        let calls = Calls()
        let merger = FakeMerger()
        // One face list for two photos can't be matched to them: none are sent.
        let m = MergeModel(kind: .burst, session: session, sources: [samplePhoto(1), samplePhoto(2)], merger: merger,
                           analyze: { _ in [BestShotFrame(faces: [], score: 0.5)] },
                           onMerged: { calls.merged.append($0) }, close: { calls.closed += 1 })
        m.start()
        #expect(await waitUntil { m.preview != nil && !m.isPreviewing })
        #expect(m.options.bestShot.isEmpty && m.previewSummary?.hasSuffix("nothing to improve") == true)
        // No analyzer (not a Best Shot, or none given): the preview starts at once.
        let plain = MergeModel(kind: .burst, session: session, sources: [samplePhoto(1), samplePhoto(2)], merger: merger,
                               onMerged: { _ in }, close: {})
        plain.start()
        #expect(!plain.isAnalyzing && plain.isPreviewing)
        #expect(await waitUntil { plain.preview != nil })
        // Closing the sheet while Vision looks stops it.
        let gate = AsyncStream<Void>.makeStream()
        let closing = MergeModel(kind: .burst, session: session, sources: [samplePhoto(1), samplePhoto(2)], merger: merger,
                                 analyze: { _ in
                                     for await _ in gate.stream { break }
                                     return []
                                 }, onMerged: { _ in }, close: { calls.closed += 1 })
        closing.start()
        closing.cancel()
        gate.continuation.yield()
        #expect(calls.closed == 1 && closing.preview == nil)
    }

    @Test func bestShotSummarySaysWhatWasTaken() throws {
        let image = try MockPhotoMerger.gradient(width: 2, height: 2)
        func parts(faces: Int = 0, regions: Int = 0, kept: Int = 0, unaligned: Int = 0) -> [String] {
            MergeModel.bestShotParts(PhotoMergePreview(image: image, width: 1, height: 1, projection: .perspective,
                                                       unalignedFrames: unaligned, facesReplaced: faces,
                                                       regionsReplaced: regions, facesKept: kept))
        }
        #expect(parts() == ["the best frame as it is: nothing to improve"])
        #expect(parts(faces: 1, regions: 3) == ["1 face improved", "3 sharper regions"])
        #expect(parts(faces: 2, regions: 1, kept: 1, unaligned: 2)
                == ["2 faces improved", "1 sharper region", "1 face moved too much to swap", "2 frames not used"])
    }

    @Test func bestShotFacesComeUprightFromVision() {
        let f = BestShotFrame.Face(FaceInfo(boundingBox: CGRect(x: 0.2, y: 0.1, width: 0.3, height: 0.4), captureQuality: 0.6,
                                            leftEyeAspect: 0.3, rightEyeAspect: 0.26))
        #expect(abs(f.bounds.minY - 0.5) < 1e-9 && f.bounds.minX == 0.2 && f.bounds.width == 0.3 && f.bounds.height == 0.4,
                "Vision's origin is bottom-left")
        #expect(f.eyesOpen == 1 && f.quality == 0.6)
        let half = BestShotFrame.Face(FaceInfo(boundingBox: .zero, captureQuality: nil, leftEyeAspect: 0.1, rightEyeAspect: nil))
        #expect(half.eyesOpen == nil && half.quality == nil, "one eye unknown: unknown")
    }

    @Test func bestShotOfABurstBecomesItsCover() async throws {
        let store = MockSessionStore(counts: [60])
        let m = makeModel(store: store)
        await m.start()
        m.setStackBursts(true)
        let s = try #require(m.browserState.stacks.stacks.max { $0.count < $1.count })
        m.focus(s.memberIDs[0], extend: false)
        #expect(!CommandCatalog.isEnabled(.mergeBestShot, model: m), "no merger")
        m.merger = MockPhotoMerger(store: store, step: .zero)
        #expect(m.bestShotSources.map(\.id) == s.memberIDs, "one photo of a burst: its whole burst")
        #expect(CommandCatalog.isEnabled(.mergeBestShot, model: m) && !CommandCatalog.isEnabled(.mergeHDR, model: m))
        #expect(CommandCatalog.title(.mergeBestShot, model: m) == "Merge to Best Shot…")
        #expect(MenuCommand.mergeBestShot.shortcut == .init(key: "b", modifiers: [.command, .shift]))
        #expect(CommandCatalog.help(.mergeBestShot, model: m)?.contains("open eyes") == true)
        CommandCatalog.perform(.mergeBestShot, model: m)
        guard case .merge(let sheet)? = m.sheet else {
            Issue.record("no merge sheet")
            return
        }
        #expect(sheet.kind == .burst && sheet.sources.map(\.id) == s.memberIDs)
        #expect(await waitUntil { sheet.preview != nil && !sheet.isAnalyzing && !sheet.isPreviewing })
        #expect(sheet.options.bestShot.count == s.count, "one per frame (Vision found no face in the demo's images)")
        sheet.merge()
        #expect(await waitUntil { m.sheet == nil })
        let shot = try #require(m.focusedPhoto)
        #expect(shot.filename.hasSuffix("-Best.dng") && !s.memberIDs.contains(shot.id))
        m.browserState.refresh(photos: m.visiblePhotos) // as the browser does when the list changes
        let stack = try #require(m.browserState.stacks.stack(of: s.memberIDs[0]))
        #expect(stack.memberIDs.contains(shot.id) && stack.bestShotID == shot.id)
        #expect(m.browserState.stacks.currentTop(of: stack, lookup: { m.photo($0) }) == shot.id)
        #expect(m.coverDecision(of: shot.id)?.source == .bestShot)
        await m.browserState.coverSave?.value
        let saved = try await store.stackCovers(in: try #require(m.currentSession))
        #expect(saved == [StackCover(members: s.memberIDs + [shot.id], cover: shot.id, bestShot: true)])
        // The Best Shot isn't merged into the next one.
        #expect(!m.bestShotSources.contains { $0.id == shot.id } && m.bestShotSources.count == s.count)
        // The user's choice outranks it.
        let top = try #require(m.coverDecision(of: shot.id)?.candidates.first { $0 != shot.id })
        m.useAsCover(top)
        #expect(m.coverDecision(of: shot.id)?.cover == top && m.coverDecision(of: shot.id)?.source == .user)
    }

    @Test func rightClickingABurstMergesItsFramesWhateverIsSelected() async throws {
        let store = MockSessionStore(counts: [60])
        let m = makeModel(store: store)
        await m.start()
        m.setStackBursts(true)
        let s = try #require(m.browserState.stacks.stacks.max { $0.count < $1.count })
        let other = try #require(m.visibleIDs.first { m.browserState.stacks.stack(of: $0) == nil })
        m.focus(other, extend: false)
        #expect(!m.canMergeBestShot(ofStackOf: s.memberIDs[0]), "no merger")
        m.merger = MockPhotoMerger(store: store, step: .zero)
        #expect(m.canMergeBestShot(ofStackOf: s.memberIDs[0]) && !m.canMergeBestShot(ofStackOf: other), "a burst only")
        #expect(!m.canMergeBestShot(ofStackOf: PhotoID(999_999)))
        m.mergeBestShot(ofStackOf: other)
        #expect(m.sheet == nil)
        m.mergeBestShot(ofStackOf: s.memberIDs[1])
        guard case .merge(let sheet)? = m.sheet else {
            Issue.record("no merge sheet")
            return
        }
        #expect(sheet.kind == .burst && sheet.sources.map(\.id) == s.memberIDs, "the burst's frames, not the selection")
        #expect(m.selectedIDs == [other] && !m.canMergeBestShot(ofStackOf: s.memberIDs[0]), "one sheet at a time")
        sheet.cancel()
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
        #expect(PhotoMergeOptions(kind: .burst).ffi.kind == .burst)
        var b = PhotoMergeOptions(kind: .burst)
        b.bestShot = [BestShotFrame(faces: [.init(bounds: CGRect(x: 0.25, y: 0.5, width: 0.125, height: 0.25),
                                                  eyesOpen: 0.75, quality: nil)], score: 0.5),
                      BestShotFrame()]
        let signals = b.burstSignals
        #expect(signals.count == 2 && signals[0].score == 0.5 && signals[1].faces.isEmpty && signals[1].score == nil)
        #expect(signals[0].faces == [BurstFaceSignal(bounds: NormRect(x: 0.25, y: 0.5, w: 0.125, h: 0.25),
                                                     eyesOpen: 0.75, quality: nil)])
        #expect(PhotoMergeOptions(kind: .hdr).burstSignals.isEmpty)
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
