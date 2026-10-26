import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// M4: culling keypress cost on a 2,000-photo session. Prints the numbers so a
/// before/after comparison is visible in the test log.
@MainActor
@Suite struct BrowserPerfTests {
    @Test func ratingCostOn2000() async {
        let m = makeModel(counts: [2000])
        await m.start()
        #expect(m.visiblePhotos.count == 2000)
        let ids = m.visiblePhotos.map(\.id)

        // (a) rating keypress: model work for one photo.
        var rateTotal = Duration.zero
        let n = 50
        for i in 0..<n {
            m.focus(ids[i * 37 % ids.count], extend: false)
            rateTotal += TestTiming.cpu { m.setRating(i % 5 + 1) }
        }
        let rateMs = rateTotal.inMilliseconds / Double(n)

        // (b) filter rebuild over all 2,000.
        var rebuildTotal = Duration.zero
        for i in 0..<10 {
            rebuildTotal += TestTiming.cpu { m.filter = PhotoFilter(minRating: i % 2) }
        }
        let rebuildMs = rebuildTotal.inMilliseconds / 10

        print(String(format: "M4 perf (2,000 photos): setRating %.3f ms/keypress, filter rebuild %.3f ms", rateMs, rebuildMs))
        // Main-thread CPU time (debug build): the printed numbers are the measurement.
        #expect(rateMs < 50)
        #expect(rebuildMs < 200)
        m.stop()
    }
}
