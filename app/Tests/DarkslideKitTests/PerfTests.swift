import Foundation
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// Culling cost on a 2,000-photo session (M4). Prints numbers. The bounds are on the
/// main thread's **CPU time** (a keypress's own work), so a loaded machine preempting
/// the test can't fail them (review M3); wall time is printed alongside.
@MainActor
@Suite struct CullingPerfTests {
    static func threadCPU() -> Duration { TestTiming.threadCPU() }

    @Test func ratingKeypressCostOn2000() async {
        let m = makeModel(counts: [2000])
        await m.start()
        let clock = ContinuousClock()
        var worst = Duration.zero, worstWall = Duration.zero
        var total = Duration.zero
        for i in 0..<50 {
            m.focus(m.visibleIDs[i * 20], extend: false)
            let (t, c) = (clock.now, Self.threadCPU())
            m.perform(.rate(i % 5 + 1))
            let d = Self.threadCPU() - c
            worstWall = max(worstWall, clock.now - t)
            total += d
            worst = max(worst, d)
        }
        print("PERF rating keypress (2000 photos): mean \(total / 50) CPU, worst \(worst) CPU (\(worstWall) wall)")
        #expect(worst < TestTiming.bound(.milliseconds(20)))
    }

    @Test func filteredRatingCostOn2000() async {
        let m = makeModel(counts: [2000])
        await m.start()
        m.filter = PhotoFilter(pick: .unrejected)
        let c = Self.threadCPU()
        for _ in 0..<50 { m.perform(.reject) }
        let d = (Self.threadCPU() - c) / 50
        print("PERF reject with active filter (2000 photos): mean \(d) CPU")
        #expect(d < TestTiming.bound(.milliseconds(20)))
    }
}
