import CoreGraphics
import Foundation
import Testing
@testable import DarkslideKit

/// `DS_PERF=1` runs the full harness (30 steps per drag, both display sizes); in the
/// normal suite a short version runs so the heavy GPU load doesn't skew the timing
/// tests that run in parallel.
private let fullPerf = ProcessInfo.processInfo.environment["DS_PERF"] != nil

/// Live frame-time harness for the develop pipeline in the viewer (prints `VW8PERF`
/// lines; bounds are loose so a loaded machine doesn't fail CI, the numbers are the
/// report). Each drag is 30 scripted steps rendered as the viewer does during a drag:
/// Draft quality, Interactive priority, at the fit size of the display.
@MainActor
@Suite(.serialized) struct Vw8PerfTests {
    static func ms(_ d: Duration) -> Double { Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15 }

    static func stats(_ v: [Double]) -> (median: Double, p90: Double, p99: Double) {
        let s = v.sorted()
        func q(_ p: Double) -> Double { s[min(s.count - 1, Int((Double(s.count - 1) * p).rounded()))] }
        return (q(0.5), q(0.9), q(0.99))
    }

    /// The drags: name and the edit at step `i` of `n`.
    static let drags: [(String, (inout EditState, Int, Int) -> Void)] = [
        ("white balance", { e, i, n in
            e.whiteBalance.asShot = false
            e.whiteBalance.temperature = 4500 + 3000 * Double(i) / Double(n)
        }),
        ("exposure", { e, i, n in e.exposure.ev = 1.5 * Double(i) / Double(n) }),
        ("curve", { e, i, n in e.curve.points[.rgb] = [CurvePoint(0.25, 0.25 - 0.1 * Double(i) / Double(n)), CurvePoint(0.75, 0.8)] }),
        ("HDR", { e, i, n in
            e.hdr.shadows = 60 * Double(i) / Double(n)
            e.hdr.highlights = -60 * Double(i) / Double(n)
        }),
        ("brush", { e, i, _ in
            // A brush layer whose stroke grows every step (+0.5 EV where painted).
            let pts = (0...i).map { k in CGPoint(x: 0.2 + 0.02 * Double(k), y: 0.5 + 0.1 * sin(Double(k) / 3)) }
            var l = LayerEdit(id: 7, name: "Brush", exposureEV: 0.5)
            l.mask.shapes = [.brush(.add, [BrushStroke(points: pts, size: 0.06)])]
            e.layers = [l]
        }),
    ]

    @Test func dragFrameTimesAtDisplaySize() async throws {
        guard let live = try await Vw8Live.make(limit: 3) else { return }
        defer { live.cleanup() }
        _ = await live.settled()
        let photo = try #require(live.app.focusedPhoto)
        let v = live.app.viewer
        let clock = ContinuousClock()
        var lines: [String] = []
        // Fit sizes: a 13" class window, and a 5K display full screen (2560x1440 pt @2x).
        let sizes: [(String, CGSize)] = [("window 1224x824@2x", CGSize(width: 1224, height: 824)),
                                         ("5K 2560x1440@2x", CGSize(width: 2560, height: 1440))]
        for (label, view) in fullPerf ? sizes : [sizes[1]] {
            v.setViewerSize(view, backingScale: 2)
            let size = v.renderPixelSize
            for (name, step) in Self.drags {
                var times: [Double] = []
                var e = live.app.focusedEdit
                let n = fullPerf ? 30 : 3
                // One warm-up (a new stage configuration may compile pipelines).
                for i in 0...n {
                    step(&e, i, n)
                    let req = RenderRequest(maxPixelSize: size, format: .rgba8Srgb, quality: .draft, priority: .interactive,
                                            geometry: .final, histogram: true)
                    let t = clock.now
                    _ = try await live.develop.render(photo, edit: e, request: req)
                    if i > 0 { times.append(Self.ms(clock.now - t)) }
                }
                let s = Self.stats(times)
                lines.append(String(format: "VW8PERF %@ (fit %d px) %@ drag: median %.1f ms, p90 %.1f, p99 %.1f",
                                    label, size, name, s.median, s.p90, s.p99))
                // Frame-time gates only in the isolated perf run (DS_PERF=1): in the parallel suite
                // every other test shares the GPU and medians of 0.8-1 s were measured.
                if fullPerf { #expect(s.p90 < 16.7, "\(name) at \(label): \(s)") }
            }
        }
        let st = live.develop.stats
        lines.append(String(format: "VW8PERF memory: %.1f MB per 5K frame; %d frames, %d frame-memory allocations (pooled), "
                            + "CPU copies: %d Rust readback (1 per GPU frame) + %d Swift",
                            Double(st.lastBytes) / 1e6, st.frames, st.allocations, st.rustCopies, st.swiftCopies))
        lines.forEach { print($0) }
        #expect(st.swiftCopies == 0)
        // M2: frame memory is recycled, not allocated per frame.
        #expect(st.allocations < st.frames / 2, "\(st)")
    }

    @Test func viewerEditToFrameAndNavigation() async throws {
        let clock = ContinuousClock()
        let t0 = clock.now
        // A fresh copy of the samples: new paths, so the first develop decodes cold.
        guard let live = try await Vw8Live.make(limit: 4) else { return }
        defer { live.cleanup() }
        let v = live.app.viewer
        _ = await live.settled()
        print(String(format: "VW8PERF open folder → first develop frame (cold decode): %.0f ms", Self.ms(clock.now - t0)))
        v.setViewerSize(CGSize(width: 2560, height: 1440), backingScale: 2)
        _ = await live.settled()
        // Edit → develop frame on screen, through the viewer (draft while dragging).
        var times: [Double] = []
        for i in 1...(fullPerf ? 20 : 3) {
            let old = v.frame
            let t = clock.now
            live.app.updateEdit(label: "ev", coalesceKey: "ev") { $0.exposure.ev = Double(i) * 0.05 }
            _ = await waitUntil(timeout: .seconds(10)) { v.frame != nil && v.frame != old }
            times.append(Self.ms(clock.now - t))
        }
        let s = Self.stats(times)
        print(String(format: "VW8PERF viewer 5K edit→frame (draft): median %.1f ms, p90 %.1f, p99 %.1f", s.median, s.p90, s.p99))
        live.app.endEditGesture()
        _ = await live.settled()
        // Navigation: the next photo was prefetched (decode + BASE + mapping) while we
        // edited; the one after that is cold.
        for label in ["prefetched neighbour", "next neighbour"] {
            try? await Task.sleep(for: .seconds(2))
            let t = clock.now
            live.app.perform(.move(.right, extend: false))
            let preview = await waitUntil(timeout: .seconds(10)) { v.previewFrame != nil }
            let tp = Self.ms(clock.now - t)
            let knownEarly = v.frame == nil && v.mapping != nil
            _ = await live.settled()
            let td = Self.ms(clock.now - t)
            print(String(format: "VW8PERF navigation to %@: preview %.0f ms, first develop frame %.0f ms (mapping known before: %@)",
                         label, tp, td, knownEarly ? "yes" : "no"))
            #expect(preview)
        }
    }
}
