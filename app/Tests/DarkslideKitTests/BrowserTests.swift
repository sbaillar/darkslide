import CoreGraphics
import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

@MainActor
@Suite struct ThumbnailCacheTests {
    @Test func cachesByBucket() async {
        let c = ThumbnailCache(provider: MockImageProvider(latency: .zero))
        let p = samplePhoto()
        #expect(c.cached(p, bucket: 256) == nil)
        let a = await c.image(for: p, bucket: 256)
        #expect(a?.width == 256 && c.loads == 1)
        let b = await c.image(for: p, bucket: 256)
        #expect(b === a && c.hits == 1)
        _ = await c.image(for: p, bucket: 512)
        #expect(c.loads == 2)
        c.removeAll()
        #expect(c.cached(p, bucket: 256) == nil)
    }

    @Test func buckets() {
        #expect(ThumbnailCache.bucket(for: 100) == 256)
        #expect(ThumbnailCache.bucket(for: 257) == 512)
        #expect(ThumbnailCache.bucket(for: 900) == 1024)
        #expect(ThumbnailCache.bucket(for: 5000) == 1024)
    }

    @Test func cancelledLoadIsNotCached() async {
        let c = ThumbnailCache(provider: MockImageProvider(latency: .seconds(5)))
        let t = Task { await c.image(for: samplePhoto(), bucket: 256) }
        try? await Task.sleep(for: .milliseconds(20))
        t.cancel()
        #expect(await t.value == nil)
        #expect(c.cached(samplePhoto(), bucket: 256) == nil)
    }

    @Test func twoThousandItemsStayWithinBudget() async {
        // 2,000 grid cells scrolled past: the cache evicts to its byte budget.
        let c = ThumbnailCache(provider: MockImageProvider(latency: .zero), costLimit: 20 << 20)
        let photos = MockSessionStore.makePhotos(count: 2000, firstID: 1, start: Date())
        for p in photos { _ = await c.image(for: p, bucket: 256) }
        let cached = photos.filter { c.cached($0, bucket: 256) != nil }.count
        #expect(c.loads == 2000)
        #expect(cached < 2000 && cached * 256 * 171 * 4 <= (20 << 20) + 256 * 256 * 4, "≈ budget / thumbnail size")
    }
}

