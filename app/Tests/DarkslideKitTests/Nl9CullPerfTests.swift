import DarkslideAI
import Foundation
import Testing
@testable import DarkslideKit

/// AI Cull on many real photos, one preview per `add` against batches of 24 on the
/// GPU analyzer (`add_batch`). Runs only with `NL9_CULL_DIR=<folder of ARWs>`
/// (e.g. 1,700 APFS clones of the samples); prints `NL9CULL` lines.
@MainActor @Suite(.serialized) struct Nl9CullPerfTests {
    @Test(.enabled(if: ProcessInfo.processInfo.environment["NL9_CULL_DIR"] != nil,
                   "SKIP: set NL9_CULL_DIR to a folder of ARWs to time AI Cull"))
    func cullTimingSingleVersusBatched() async throws {
        let dir = try #require(ProcessInfo.processInfo.environment["NL9_CULL_DIR"])
        let urls = try FileManager.default.contentsOfDirectory(at: URL(fileURLWithPath: dir), includingPropertiesForKeys: nil)
            .filter { $0.pathExtension.uppercased() == "ARW" }.sorted { $0.path < $1.path }
        var photos: [Photo] = []
        for (i, u) in urls.enumerated() { photos.append(try await RustRaw.photo(at: u, id: PhotoID(Int64(i + 1)))) }
        let clock = ContinuousClock()
        var results: [[CullSuggestion]] = []
        // `NL9_CONFIGS="1x4,24x8"`: batch × concurrency pairs to time.
        let configs = (ProcessInfo.processInfo.environment["NL9_CONFIGS"] ?? "1x4,24x4,24x8,24x12")
            .split(separator: ",").compactMap { pair -> (Int, Int)? in
                let v = pair.split(separator: "x").compactMap { Int($0) }
                return v.count == 2 ? (v[0], v[1]) : nil
            }
        for (batch, concurrent) in configs {
            let images = PreviewThumbnailProvider(previews: RustPreviewProvider())
            let t = clock.now
            results.append(try await VisionAIService(maxConcurrent: concurrent, batchSize: batch)
                .cull(photos, images: images) { _, _ in })
            let d = clock.now - t
            let s = Double(d.components.seconds) + Double(d.components.attoseconds) / 1e18
            print(String(format: "NL9CULL %d photos, batch %d, %d concurrent: %.1f s (%.1f ms/photo)", photos.count, batch,
                         concurrent, s, s * 1000 / Double(photos.count)))
        }
        #expect(results.allSatisfy { $0 == results[0] }, "batched suggestions equal single adds")
    }

    /// Latency per backend on the review's phrases (`NL9_NL_PERF=1`; Foundation Models
    /// only when Apple Intelligence is available). Prints `NL9NL` lines and each guarded result.
    @Test(.enabled(if: ProcessInfo.processInfo.environment["NL9_NL_PERF"] != nil,
                   "SKIP: set NL9_NL_PERF=1 to time the instruction backends"))
    func instructionLatencyPerBackend() async throws {
        let phrases = ["more contrast", "lift shadows", "black and white", "make it warmer and brighter", "warmer by 400",
                       "brighter by 7 stops", "straighten 2 degrees", "the sky looks washed out", "make it moodier"]
        let clock = ContinuousClock()
        func ms(_ d: Duration) -> Double { Double(d.components.seconds) * 1000 + Double(d.components.attoseconds) / 1e15 }
        var backends: [(String, any NaturalLanguageEditService)] = [("rules", RulesEditService())]
        if FoundationModelsEditService.isAvailable {
            backends.append(("foundation-models (raw)", FoundationModelsEditService()))
            backends.append(("guarded rules+FM", GuardedEditService(models: [FoundationModelsEditService()])))
        }
        for (name, b) in backends {
            var times: [Double] = []
            for p in phrases {
                let t = clock.now
                let i = try? await b.intent(for: p)
                times.append(ms(clock.now - t))
                print("NL9NL \(name) “\(p)” → \(i.map { String(describing: $0) } ?? "error")")
            }
            times.sort()
            print(String(format: "NL9NL %@: median %.1f ms, max %.1f ms", name, times[times.count / 2], times.last!))
        }
    }
}
