import CoreGraphics
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Provider that fails, or blocks until cancelled.
struct ControlledProvider: ImageProvider {
    enum Mode { case fail, hang, hangExcept(PhotoID) }
    let mode: Mode
    func thumbnail(for photo: Photo, maxPixelSize: Int) async throws -> CGImage {
        switch mode {
        case .fail: throw MockError.invalid("unreadable")
        case .hangExcept(let id) where photo.id == id:
            return try await MockImageProvider(latency: .zero).thumbnail(for: photo, maxPixelSize: maxPixelSize)
        case .hang, .hangExcept:
            try await Task.sleep(for: .seconds(30))
            throw MockError.invalid("unreachable")
        }
    }
}

@MainActor
@Suite struct ThumbnailLoaderTests {
    @Test func loadsThenSkipsReloadForSameKey() async {
        let cache = ThumbnailCache(provider: MockImageProvider(latency: .zero))
        let l = ThumbnailLoader(cache: cache)
        let p = samplePhoto()
        await l.load(p, bucket: 256)
        #expect(l.image?.width == 256 && !l.failed && l.key == .init(id: p.id, bucket: 256))
        #expect(l.loadsStarted == 1)
        var rated = p
        rated.rating = 5 // badge-only change
        await l.load(rated, bucket: 256)
        #expect(l.loadsStarted == 1, "a rating change never reloads the image")
        #expect(cache.loads == 1)
    }

    @Test func primeAppliesCacheHitSynchronously() async {
        let cache = ThumbnailCache(provider: MockImageProvider(latency: .zero))
        let p = samplePhoto()
        _ = await cache.image(for: p, bucket: 256)
        let l = ThumbnailLoader(cache: cache)
        #expect(l.prime(p, bucket: 256))
        #expect(l.image != nil && l.loadsStarted == 0)
        #expect(!ThumbnailLoader(cache: cache).prime(samplePhoto(9), bucket: 256))
    }

    @Test func bucketChangeKeepsPlaceholderAndPhotoChangeClears() async {
        let cache = ThumbnailCache(provider: MockImageProvider(latency: .zero))
        let l = ThumbnailLoader(cache: cache)
        let p = samplePhoto()
        await l.load(p, bucket: 256)
        let small = l.image
        await l.load(p, bucket: 512)
        #expect(l.image?.width == 512 && l.image !== small)
        #expect(l.loadsStarted == 2)
    }

    @Test func differentPhotoNeverShowsTheOldImage() async {
        // Photo 1 loads instantly; photo 3 hangs. While 3 loads, 1 must not be shown.
        let l = ThumbnailLoader(cache: ThumbnailCache(provider: ControlledProvider(mode: .hangExcept(PhotoID(1)))))
        await l.load(samplePhoto(1), bucket: 256)
        #expect(l.image != nil)
        let t = Task { await l.load(samplePhoto(3), bucket: 256) }
        #expect(await waitUntil { l.loadsStarted == 2 })
        #expect(l.image == nil && l.key?.id == PhotoID(3))
        t.cancel()
        await t.value
    }

    @Test func cancelledLoadLeavesNoImageAndNoFailure() async {
        let l = ThumbnailLoader(cache: ThumbnailCache(provider: ControlledProvider(mode: .hang)))
        let t = Task { await l.load(samplePhoto(), bucket: 256) }
        #expect(await waitUntil { l.loadsStarted == 1 })
        t.cancel()
        await t.value
        #expect(l.image == nil && !l.failed, "cancellation is not a failure")
    }

    @Test func failureIsReported() async {
        let l = ThumbnailLoader(cache: ThumbnailCache(provider: ControlledProvider(mode: .fail)))
        await l.load(samplePhoto(), bucket: 256)
        #expect(l.failed && l.image == nil)
    }

    @Test func bucketHelper() {
        #expect(ThumbnailLoader.bucket(points: 116, scale: 2) == 256)
        #expect(ThumbnailLoader.bucket(points: 180, scale: 2) == 512)
        #expect(ThumbnailLoader.bucket(points: 300, scale: 0) == 512, "scale floors at 1")
    }
}

@Suite struct StackBuilderTests {
    func photo(_ id: Int64, t: Double, camera: String = "Sony ILCE-9", rating: Int = 0, pick: PickState = .none) -> Photo {
        var p = samplePhoto(id)
        p.meta.captureDate = Date(timeIntervalSince1970: 1_000 + t)
        p.meta.camera = camera
        p.rating = rating
        p.pick = pick
        return p
    }

    @Test func groupsBurstsByGapAndCamera() {
        let ps = [photo(1, t: 0), photo(2, t: 0.3), photo(3, t: 0.9), photo(4, t: 5),
                  photo(5, t: 5.2, camera: "Other"), photo(6, t: 9), photo(7, t: 9.5)]
        let s = StackBuilder.build(ps)
        #expect(s.map(\.memberIDs) == [[1, 2, 3].map(PhotoID.init), [6, 7].map(PhotoID.init)])
        #expect(s[0].id == PhotoID(1) && s[0].count == 3)
        #expect(StackBuilder.build([photo(1, t: 0)]).isEmpty, "singletons aren't stacks")
        #expect(StackBuilder.build([]).isEmpty)
        #expect(StackBuilder.build([photo(1, t: 0), photo(2, t: 1.0)]).count == 1, "gap is inclusive")
        #expect(StackBuilder.build([photo(1, t: 0), photo(2, t: 1.01)]).isEmpty)
        #expect(StackBuilder.build([photo(2, t: 0.5), photo(1, t: 0)]).count == 1, "reverse order still stacks")
    }

    @Test func topPrefersPickedThenRatingThenFirst() {
        #expect(StackBuilder.top(of: [photo(1, t: 0), photo(2, t: 0)]).id == PhotoID(1))
        #expect(StackBuilder.top(of: [photo(1, t: 0), photo(2, t: 0, rating: 3)]).id == PhotoID(2))
        #expect(StackBuilder.top(of: [photo(1, t: 0, rating: 5), photo(2, t: 0, pick: .picked)]).id == PhotoID(2))
        #expect(StackBuilder.top(of: [photo(1, t: 0, rating: 5, pick: .rejected), photo(2, t: 0)]).id == PhotoID(2))
    }
}

@MainActor
@Suite struct StackStateTests {
    let ps: [Photo] = {
        func photo(_ id: Int64, t: Double, rating: Int = 0) -> Photo {
            var p = samplePhoto(id)
            p.meta.captureDate = Date(timeIntervalSince1970: 1_000 + t)
            p.rating = rating
            return p
        }
        return [photo(1, t: 0), photo(2, t: 0.2, rating: 4), photo(3, t: 0.4), photo(4, t: 10), photo(5, t: 20), photo(6, t: 20.5)]
    }()

    @Test func collapseFoldsStacksToTheirTop() {
        let s = StackState()
        s.update(photos: ps)
        let ids = ps.map(\.id)
        #expect(s.stacks.count == 2)
        #expect(s.displayIDs(from: ids) == ids, "nothing collapsed")
        #expect(s.toggleStack(of: PhotoID(3)))
        #expect(s.displayIDs(from: ids) == [2, 4, 5, 6].map(PhotoID.init), "burst shown by its 4★ top")
        #expect(s.badgeCount(for: PhotoID(2)) == 3 && s.badgeCount(for: PhotoID(4)) == nil)
        #expect(s.badgeCount(for: PhotoID(5)) == nil, "expanded stack has no badge")
        s.collapseAll()
        #expect(s.displayIDs(from: ids) == [2, 4, 5].map(PhotoID.init))
        s.expandAll()
        #expect(s.displayIDs(from: ids) == ids)
        #expect(!s.toggleStack(of: PhotoID(4)), "not stacked")
        #expect(s.stack(of: PhotoID(4)) == nil && s.stack(of: PhotoID(6))?.count == 2)
    }

    @Test func filteredTopFallsBackToFirstVisibleMember() {
        let s = StackState()
        s.update(photos: ps)
        s.collapseAll()
        #expect(s.displayIDs(from: [1, 3, 4].map(PhotoID.init)) == [1, 4].map(PhotoID.init))
    }

    @Test func liveTopFollowsRatings() {
        let s = StackState()
        s.update(photos: ps)
        s.collapseAll()
        var byID = Dictionary(uniqueKeysWithValues: ps.map { ($0.id, $0) })
        byID[PhotoID(3)]!.rating = 5
        #expect(s.displayIDs(from: ps.map(\.id), lookup: { byID[$0] }).first == PhotoID(3))
    }

    @Test func updateKeepsCollapseStateForSurvivingStacks() {
        let s = StackState()
        s.update(photos: ps)
        s.collapseAll()
        s.update(photos: Array(ps.prefix(4)))
        #expect(s.collapsed == [PhotoID(1)], "the 5–6 stack is gone")
        #expect(s.isCollapsed(s.stacks[0]))
    }

    @Test func browserStateRefreshAndStackToggle() async {
        let m = makeModel(counts: [40])
        await m.start()
        let b = m.browserState
        #expect(m.browserState === b, "one state per model")
        #expect(makeModel().browserState !== b)
        b.refresh(photos: ps)
        #expect(b.stacks.stacks.count == 2)
        b.stackBursts = true
        #expect(b.displayIDs(ps.map(\.id)).count == 3)
        b.refresh(photos: ps) // same list: no rebuild, still collapsed
        #expect(b.stacks.collapsed.count == 2)
        b.refresh(photos: Array(ps.reversed()))
        #expect(b.stacks.collapsed.count == 2, "rebuild re-collapses while stacking is on")
        b.stackBursts = false
        #expect(b.displayIDs(ps.map(\.id)) == ps.map(\.id))
    }
}

@Suite struct FilterBarLogicTests {
    @Test func starThresholdTogglesAndExcludesUnrated() {
        var f = PhotoFilter()
        f.toggleUnrated()
        #expect(f.unratedOnly)
        f.toggleMinRating(3)
        #expect(f.minRating == 3 && !f.unratedOnly)
        f.toggleMinRating(3)
        #expect(f.minRating == 0)
        f.toggleMinRating(9)
        #expect(f.minRating == 5)
        f.toggleUnrated()
        #expect(f.unratedOnly && f.minRating == 0)
        f.toggleUnrated()
        #expect(!f.unratedOnly)
    }

    @Test func tagChipsToggle() {
        var f = PhotoFilter()
        f.toggleTag(.red)
        #expect(f.colorTag == .red)
        f.toggleTag(.blue)
        #expect(f.colorTag == .blue)
        f.toggleTag(.blue)
        #expect(f.colorTag == nil)
    }

    @Test func activeCriteriaCountsEach() {
        #expect(PhotoFilter().activeCriteria == 0)
        let f = PhotoFilter(minRating: 2, colorTag: .red, pick: .picked, text: "dsc", unratedOnly: true)
        #expect(f.activeCriteria == 5)
        #expect(PhotoFilter(text: "   ").activeCriteria == 0)
        #expect(PhotoFilter.barPickOptions == [.all, .picked, .unflagged, .rejected])
    }

    @Test func summaryText() {
        #expect(FilterSummary.text(shown: 2000, total: 2000, filtered: false) == "2,000 photos")
        #expect(FilterSummary.text(shown: 1, total: 1, filtered: false) == "1 photo")
        #expect(FilterSummary.text(shown: 37, total: 2000, filtered: true) == "37 of 2,000")
    }

    @Test func sortOrders() {
        var a = samplePhoto(1), b = samplePhoto(2), c = samplePhoto(3)
        a.filename = "DSC10.ARW"; b.filename = "DSC9.ARW"; c.filename = "DSC100.ARW"
        a.meta.captureDate = Date(timeIntervalSince1970: 30)
        b.meta.captureDate = Date(timeIntervalSince1970: 10)
        c.meta.captureDate = Date(timeIntervalSince1970: 10)
        a.rating = 2; b.rating = 5; c.rating = 2
        let ps = [a, b, c]
        #expect(PhotoSort.captureTime.apply(to: ps).map(\.id) == [b.id, c.id, a.id], "ties keep incoming order")
        #expect(PhotoSort.filename.apply(to: ps).map(\.filename) == ["DSC9.ARW", "DSC10.ARW", "DSC100.ARW"], "numeric-aware")
        #expect(PhotoSort.rating.apply(to: ps).map(\.id) == [b.id, a.id, c.id])
        #expect(PhotoSort.allCases.allSatisfy { !$0.symbol.isEmpty })
    }
}

@Suite struct ThumbnailLayoutTests {
    @Test func portraitsFillHeightLandscapesFillWidth() {
        let box = CGSize(width: 140, height: 100)
        let port = ThumbnailLayout.fit(aspect: 2.0 / 3.0, in: box, inset: 8)
        #expect(port.height == 84 && abs(port.width - 56) < 1e-9)
        #expect(abs(port.midX - 70) < 1e-9 && abs(port.midY - 50) < 1e-9)
        let land = ThumbnailLayout.fit(aspect: 3, in: box, inset: 8)
        #expect(land.width == 124 && abs(land.height - 124.0 / 3) < 1e-9)
        #expect(ThumbnailLayout.fit(aspect: .nan, in: box).width == 140, "bad aspect falls back to 3:2")
        #expect(ThumbnailLayout.fit(aspect: 1.5, in: CGSize(width: 10, height: 10), inset: 8) == .zero)
        #expect(ThumbnailLayout.filmstripCellWidth(height: 100) == 115)
    }
}
