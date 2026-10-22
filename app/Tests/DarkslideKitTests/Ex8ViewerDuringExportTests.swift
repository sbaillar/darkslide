import CoreGraphics
import DarkslideAI
import DarkslideFFI
import Foundation
import ImageIO
import Testing
@testable import DarkslideCore
@testable import DarkslideMac
@testable import DarkslideUI

/// The viewer's slider drag (Draft, Interactive, 2560 px, a new exposure every frame)
/// through the app's own `DevelopRenderer` (FFI → shared GPU renderer → Metal buffer)
/// while `RustExportService` writes full-size JPEGs of the samples at Background
/// priority.
///
/// Gate: p90 ≤ 20 ms (p99 around 40 ms is expected: a frame that lands while an export
/// pass holds the GPU waits for that pass). The Swift suites run in parallel and share
/// the GPU, so an absolute bound alone would flake: the same drag is timed idle right
/// before, and when that idle p90 already shows a loaded machine (> 15 ms) the bound is
/// relative, the export may cost at most 50 % over it. One re-measure absorbs a transient
/// spike from another suite. The isolated Rust test (`viewer_during_export.rs`) keeps the
/// absolute gate.
@MainActor
@Suite(.serialized) struct Ex8ViewerDuringExportTests {
    static func pct(_ v: [Double], _ p: Int) -> Double { v.isEmpty ? 0 : v[min(v.count - 1, v.count * p / 100)] }

    /// M6: the isolated perf pass (`DS_PERF=1`, serial, idle machine) holds the real
    /// gate, 20 ms (a slightly slow idle drag, > 15 ms p90, gets 1.5x its idle p90, capped
    /// at 30 ms). In the parallel suite the GPU is shared with every other test, so there
    /// it is a regression guard only: at most 1.5x the idle p90, capped at 500 ms (idle
    /// p90 of 300 ms was seen there).
    static func bound(idle p90: Double, perf: Bool = ProcessInfo.processInfo.environment["DS_PERF"] != nil) -> Double {
        if perf { return p90 <= 15 ? 20 : min(p90 * 1.5, 30) }
        return min(max(20, p90 * 1.5), 500)
    }

    /// Drag frames of `photo` until `stop()` (or `limit` frames); sorted ms per frame.
    static func drag(_ dev: DevelopRenderer, _ photo: Photo, limit: Int, stop: () -> Bool = { false }) async throws -> [Double] {
        let req = RenderRequest(maxPixelSize: 2560, quality: .draft, priority: .interactive, geometry: .final,
                                histogram: true)
        var times: [Double] = []
        let clock = ContinuousClock()
        for i in 0..<limit where !stop() {
            var e = EditState()
            e.exposure.ev = Double(i % 40) * 0.05 - 1
            let t = clock.now
            let f = try await dev.render(photo, edit: e, request: req)
            let d = clock.now - t
            times.append(Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15)
            #expect(f.histogram != nil)
        }
        return times.sorted()
    }

    /// One measurement: idle drag, then the same drag while a full-size export runs.
    static func measure(_ live: Vw8Live, _ photo: Photo, exporting: [URL], round: Int)
        async throws -> (idle: [Double], busy: [Double], exportSecs: Double) {
        _ = try await drag(live.develop, photo, limit: 5) // warm
        let idle = try await drag(live.develop, photo, limit: 60)
        let out = live.dir.appendingPathComponent("ex8-bench-\(round)")
        let items = exporting.enumerated().map { ExportJobItem(photoID: PhotoID(Int64($0.offset + 1)), url: $0.element,
                                                               seq: $0.offset + 1) }
        let done = Ex8Flag()
        let t0 = Date()
        let export = Task.detached {
            defer { done.set() }
            return try await RustExportService().export(ExportJob(
                items: items, recipes: [ExportRecipe(name: "Full", quality: 90, sharpen: 0.5, fileNameTemplate: "{seq}")],
                defaultFolder: out)) { _, _ in }
        }
        try await Task.sleep(for: .milliseconds(300))
        let busy = try await drag(live.develop, photo, limit: 100_000) { done.isSet }
        let o = try await export.value
        #expect(o.failures.isEmpty && o.written.count == items.count, "\(o.failures)")
        return (idle, busy, Date().timeIntervalSince(t0))
    }

    @Test func viewerDragStaysFastWhileAFullSizeExportRuns() async throws {
        let samples = sampleARWs()
        guard samples.count >= 4, let live = try await Vw8Live.make(limit: 1) else { return }
        defer { live.cleanup() }
        let photo = try #require(live.app.visibleIDs.first.flatMap { live.app.photo($0) })
        let exporting = Array(samples.suffix(from: 1))
        var pass = false
        for round in 0..<2 where !pass {
            let (idle, busy, secs) = try await Self.measure(live, photo, exporting: exporting, round: round)
            let bound = Self.bound(idle: Self.pct(idle, 90))
            print(String(format: "EX8 viewer drag (Swift, Draft 2560) idle p50 %.1f p90 %.1f p99 %.1f ms | during export "
                         + "p50 %.1f p90 %.1f p99 %.1f max %.1f ms (%d frames, bound %.1f) | export %d photos full size "
                         + "in %.2f s", Self.pct(idle, 50), Self.pct(idle, 90), Self.pct(idle, 99), Self.pct(busy, 50),
                         Self.pct(busy, 90), Self.pct(busy, 99), busy.last ?? 0, busy.count, bound, exporting.count, secs))
            // The viewer kept rendering for the whole export: its frames cover the time the
            // export ran (a frame count alone flaked when the export was quick).
            let rendering = busy.reduce(0, +) / 1000
            #expect(busy.count >= 3 && rendering >= (secs - 0.3) * 0.5,
                    "the viewer kept rendering: \(busy.count) frames, \(rendering) s of \(secs - 0.3) s")
            pass = Self.pct(busy, 90) <= bound
        }
        #expect(pass, "viewer p90 over its bound during export in two rounds")
    }
}

/// A flag set from the export task, read by the drag loop.
final class Ex8Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    var isSet: Bool { lock.withLock { value } }
    func set() { lock.withLock { value = true } }
}
