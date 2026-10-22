import Foundation
import Metal
import Testing
@testable import DarkslideKit

@Suite struct ZPool {
    /// Retired tiles of distinct sizes each get their own pool bucket; nothing ever evicts buckets.
    @Test func poolGrowsWithDistinctTileSizes() throws {
        let r = try MetalViewerRenderer()
        let u = TextureUploader(renderer: r)
        let dev = r.device
        let base = dev.currentAllocatedSize
        for i in 0..<30 {
            let w = 2900 + i, h = 1950 + i
            let f = RenderedFrame(width: w, height: h, rgba: Data(count: w * h * 4))
            _ = u.textureNow(for: f)
            u.retain(only: [])
        }
        let mb = Double(dev.currentAllocatedSize - base) / 1e6
        print("ZPOOL after 30 retired tiles of distinct size: +\(Int(mb)) MB GPU allocated, cached=\(u.cachedIDs.count)")
        // The device counter is shared with tests running in parallel, so assert on the pool itself.
        #expect(u.pooledBytes <= u.poolByteLimit && u.cachedIDs.isEmpty)
    }
}
