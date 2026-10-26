import CoreGraphics
import LuxtableAI
import Foundation
import Testing

/// Vision performs are bounded process-wide: an unbounded fan-out (AI Cull batches plus
/// masks and Auto) blocked every GCD worker inside Vision's own capacity queues and hung
/// the process.
@Suite struct VisionLimiterTests {
    final class Peak: @unchecked Sendable {
        private let lock = NSLock()
        private var now = 0
        private(set) var max = 0
        func enter() { lock.withLock { now += 1; max = Swift.max(max, now) } }
        func leave() { lock.withLock { now -= 1 } }
    }

    @Test func atMostLimitHoldersAndEveryoneFinishes() async {
        let limiter = AsyncLimiter(3)
        let peak = Peak()
        await withTaskGroup(of: Void.self) { g in
            for _ in 0..<40 {
                g.addTask {
                    await limiter.acquire()
                    peak.enter()
                    try? await Task.sleep(for: .milliseconds(2))
                    peak.leave()
                    await limiter.release()
                }
            }
        }
        #expect(peak.max <= 3 && peak.max >= 1)
        #expect(await limiter.holders == 0)
    }

    @Test func realVisionCallsUnderAFanOutAllComplete() async throws {
        let ctx = CGContext(data: nil, width: 64, height: 48, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
        ctx.setFillColor(CGColor(srgbRed: 0.3, green: 0.6, blue: 0.9, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: 64, height: 48))
        let image = try #require(ctx.makeImage())
        let vision = VisionService()
        let done = try await withThrowingTaskGroup(of: Bool.self) { g in
            for _ in 0..<48 { g.addTask { _ = try await vision.aesthetics(image); return true } }
            var n = 0
            for try await ok in g where ok { n += 1 }
            return n
        }
        #expect(done == 48)
    }
}
