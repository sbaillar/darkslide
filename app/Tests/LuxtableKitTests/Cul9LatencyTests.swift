import Foundation
import Testing
@testable import LuxtableCore
@testable import LuxtableMac
@testable import LuxtableUI

/// Keyboard culling latency on the live path (final-journeys M4): the real app
/// wiring (lt-session, embedded previews through lt-raw, the develop renderer) over
/// 200 photos (APFS clones of the samples).
///
/// Every time is key → the focused photo **itself** on screen (its develop frame or its
/// own embedded preview), never a held previous frame (review M5):
/// - rating with auto-advance: key → the next photo;
/// - cold jumps to photos never shown, outside the prefetch window (nothing cached);
/// - a held arrow (key repeat at 30 Hz): key → that photo (a press the next one
///   overtakes counts 33 ms), and the longest time the viewer showed nothing;
/// - key-up after a hold: the landing photo.
///
/// Numbers are printed (`CUL9LAT`). The targets (p90 ≤ 50 ms, no stall > 100 ms)
/// are asserted with `LT_PERF=1` in a release build (`swift test -c release`).
@MainActor @Suite(.serialized) struct Cul9LatencyTests {
    static let strict = LaunchEnvironment.value("LT_PERF") != nil

    static func ms(_ d: Duration) -> Double {
        Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15
    }

    static func stats(_ v: [Double]) -> (p50: Double, p90: Double, max: Double) {
        let s = v.sorted()
        guard !s.isEmpty else { return (0, 0, 0) }
        return (s[s.count / 2], s[min(s.count - 1, Int(Double(s.count) * 0.9))], s.last!)
    }

    /// 200 clones of the samples in a temp folder.
    static func bigFolder(_ count: Int) throws -> URL? {
        let samples = sampleARWs()
        guard !samples.isEmpty else { return nil }
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("cul9-lat-\(UUID())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        for i in 0..<count {
            let src = samples[i % samples.count]
            try FileManager.default.copyItem(at: src, to: dir.appendingPathComponent(String(format: "L%04d.ARW", i)))
        }
        return dir
    }

    /// The focused photo is on screen.
    static func shown(_ m: AppModel) -> Bool { m.viewer.displayFrame != nil }

    /// The focused photo itself is on screen (not the held previous frame).
    static func exact(_ m: AppModel) -> Bool {
        guard let f = m.focusedID else { return false }
        return m.viewer.frameID == f || (m.viewer.previewID == f && m.viewer.previewFrame != nil)
    }

    @Test func keyboardCullingLatency() async throws {
        guard let dir = try Self.bigFolder(200) else { return }
        defer { try? FileManager.default.removeItem(at: dir) }
        let settings = makeSettings()
        settings.onboardingComplete = true
        let m = AppModel.make(mode: .live, settings: settings, defaults: testDefaults())
        m.watchCards = false
        await m.openFolder(dir)
        m.viewer.setViewerSize(CGSize(width: 1400, height: 900), backingScale: 2)
        #expect(await waitUntil(timeout: .seconds(30)) { m.visibleIDs.count == 200 && Self.shown(m) })
        let clock = ContinuousClock()
        let key = { (k: KeyInput) in _ = m.handleKey(k, focus: .none, sheetOpen: false, isMainWindow: true) }
        /// Key → the focused photo **itself** on screen (review M5: not a held previous
        /// frame); `limit` bounds the wait.
        func untilExact(_ limit: Duration) async -> Double {
            let t = clock.now
            while !Self.exact(m), clock.now - t < limit { try? await Task.sleep(for: .milliseconds(1)) }
            return Self.ms(clock.now - t)
        }
        let limit: Duration = .seconds(30)

        // Rating with auto-advance: 3, the next photo itself on screen.
        settings.autoAdvance = true
        var rate: [Double] = []
        for _ in 0..<40 {
            let before = m.focusedID
            key(KeyInput("3"))
            #expect(m.focusedID != before, "the rating advanced")
            rate.append(await untilExact(limit))
            try? await Task.sleep(for: .milliseconds(120)) // a human pace between ratings
        }

        // Cold: jumps to photos never shown and far outside the prefetch window, so
        // nothing is cached (distinct files: every decode/preview key is per path).
        var cold: [Double] = []
        let ids = m.visibleIDs
        for i in stride(from: 100, to: 200, by: 5) {
            m.focus(ids[i], extend: false)
            cold.append(await untilExact(limit))
        }

        // A held arrow: 40 presses at 30 Hz (key repeat), then key-up.
        var right: [Double] = []
        var blank: Double = 0
        var blankSince: ContinuousClock.Instant?
        m.focus(ids[45], extend: false)
        _ = await untilExact(limit)
        for i in 0..<40 {
            let t = clock.now
            key(KeyInput(special: .right, isRepeat: i > 0))
            var hit = false
            while clock.now - t < .milliseconds(33) {
                if !hit, Self.exact(m) { right.append(Self.ms(clock.now - t)); hit = true }
                if Self.shown(m) {
                    if let s = blankSince { blank = max(blank, Self.ms(clock.now - s)); blankSince = nil }
                } else if blankSince == nil {
                    blankSince = clock.now
                }
                try? await Task.sleep(for: .milliseconds(1))
            }
            if !hit { right.append(33) } // overtaken by the next press
        }
        m.setKeyRepeat(false) // key-up
        let landing = await untilExact(limit)
        if let s = blankSince { blank = max(blank, Self.ms(clock.now - s)) }

        let r = Self.stats(rate), c = Self.stats(cold), x = Self.stats(right)
        #if DEBUG
        let build = "debug"
        #else
        let build = "release"
        #endif
        print(String(format: "CUL9LAT [%@] rate+advance → that photo shown: p50 %.1f ms, p90 %.1f, max %.1f (%d keys)",
                     build, r.p50, r.p90, r.max, rate.count))
        print(String(format: "CUL9LAT [%@] cold jump (uncached) → that photo shown: p50 %.1f ms, p90 %.1f, max %.1f (%d jumps)",
                     build, c.p50, c.p90, c.max, cold.count))
        print(String(format: "CUL9LAT [%@] held arrow (30 Hz): key → that photo shown p50 %.1f ms, p90 %.1f; overtaken %d of %d; longest blank %.1f ms; landing after key-up %.1f ms",
                     build, x.p50, x.p90, right.filter { $0 >= 33 }.count, right.count, blank, landing))
        print("CUL9LAT [\(build)] rate+advance slow presses:", rate.enumerated().filter { $0.element > 50 }.map { "#\($0.offset) \(Int($0.element)) ms" })
        // Every photo shows up (a 30 s bound: debug builds on a loaded machine are slow;
        // the timing targets are checked in release with LT_PERF=1).
        #expect(r.max < 30_000 && c.max < 30_000 && landing < 30_000, "every photo shows up")
        if Self.strict {
            #expect(r.p90 <= 50, "rate+advance p90 \(r.p90) ms")
            #expect(c.p90 <= 100, "cold jump p90 \(c.p90) ms")
            #expect(blank <= 100 && landing <= 100, "no stall over 100 ms: blank \(blank), landing \(landing)")
        }
    }
}
