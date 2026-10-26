import CoreGraphics
import LuxtableFFI
import Foundation

/// Embeds a session's photos for search, in the background.
///
/// Each photo's **embedded preview** is decoded small (no RAW develop) and run
/// through the image encoder in batches; vectors go to the session and the global
/// index, keyed by content, so a photo is embedded once whatever it's called or
/// wherever it lives. Indexing pauses while the user culls (the same activity
/// gate as background hashing) and waits for photos that are still being
/// identified. Run it from a background-priority task; cancel the task to stop
/// (what was stored stays).
public struct SearchIndexer: Sendable {
    public struct Progress: Sendable, Equatable {
        public var done: Int
        public var total: Int
    }

    public struct Report: Sendable, Equatable {
        public var stored = 0
        /// Photos whose preview couldn't be read (skipped this run).
        public var failed = 0
        /// The global index couldn't be updated ("Search all sessions" misses them).
        public var globalError: String?
    }

    public let backend: any SearchBackend
    public let encoder: any SearchEncoding
    /// A small upright preview of the photo at a URL.
    public let preview: @Sendable (URL) async throws -> CGImage
    /// The user navigated or rated a moment ago.
    public let isUserActive: @Sendable () -> Bool
    public var batchSize = 16
    public var concurrency = 4
    public var idlePoll: Duration = .milliseconds(250)
    public var waitingPoll: Duration = .seconds(3)
    public var maxWaitingPoll: Duration = .seconds(60)
    /// Looks in a row that found the same photos still waiting (files that can't be
    /// read): the run ends, and the next open tries again (about 8 minutes at the defaults).
    public var maxUnchangedPolls = 10

    public init(backend: any SearchBackend, encoder: any SearchEncoding,
                preview: (@Sendable (URL) async throws -> CGImage)? = nil,
                isUserActive: @escaping @Sendable () -> Bool = { (try? userIsActive()) ?? false }) {
        self.backend = backend
        self.encoder = encoder
        // Not a default-argument function value: called from the task group in `previews`,
        // the default one crashed the task allocator ("freed pointer was not the last
        // allocation", Swift 6.2, as in `DownloadableModel`); a closure formed here is fine.
        self.preview = preview ?? { try await SearchIndexer.embeddedPreview($0) }
        self.isUserActive = isUserActive
    }

    /// Long edge of the decoded preview: the encoder center-crops 256 px from
    /// the short side, so a 3:2 frame needs 384.
    public static let previewEdge = 384

    /// The RAW's embedded JPEG, decoded at `previewEdge` and rotated upright
    /// (uncached: indexing never evicts what the viewer holds).
    @Sendable public static func embeddedPreview(_ url: URL) async throws -> CGImage {
        try await RustPreviewDecoder().preview(for: url, maxPixelSize: previewEdge)
    }

    /// Embed every photo of `session` that has no vector yet.
    public func run(_ session: SessionInfo, progress: @escaping @Sendable (Progress) -> Void = { _ in }) async throws -> Report {
        var report = Report()
        var done = 0
        // Every key tried this run: one that comes back (unreadable preview, a
        // vector the store refused) isn't tried again until the next run.
        var attempted = Set<String>()
        var poll = waitingPoll
        var lastWaiting = -1
        var unchanged = 0
        while true {
            let jobs = try await backend.jobs(model: encoder.modelID, in: session).filter { !attempted.contains($0.key) }
            if jobs.isEmpty {
                // Photos still being hashed get keys later: look again then, less
                // often while the count doesn't move (a file that can't be read
                // stays unhashed until the next open).
                let status = try await backend.status(model: encoder.modelID, in: session)
                if status.waiting == 0 { return report }
                unchanged = status.waiting == lastWaiting ? unchanged + 1 : 0
                if unchanged >= maxUnchangedPolls { return report }
                poll = unchanged > 0 ? min(poll * 2, maxWaitingPoll) : waitingPoll
                lastWaiting = status.waiting
                try await Task.sleep(for: poll)
                continue
            }
            let total = done + jobs.count
            progress(Progress(done: done, total: total))
            for start in stride(from: 0, to: jobs.count, by: max(1, batchSize)) {
                let batch = Array(jobs[start..<min(start + max(1, batchSize), jobs.count)])
                try await waitWhileUserActive()
                let loaded = try await previews(batch)
                attempted.formUnion(batch.map(\.key))
                let ready = batch.compactMap { job in loaded[job.key].map { (job.key, $0) } }
                report.failed += batch.count - ready.count
                if !ready.isEmpty {
                    try Task.checkCancellation()
                    let vectors = try encoder.imageEmbeddings(ready.map(\.1))
                    let embeddings = zip(ready, vectors).map { SearchEmbedding(key: $0.0, vector: $1) }
                    if let e = try await backend.store(embeddings, model: encoder.modelID, in: session) {
                        report.globalError = e
                    }
                    report.stored += embeddings.count
                }
                done += batch.count
                progress(Progress(done: done, total: total))
            }
        }
    }

    private func waitWhileUserActive() async throws {
        while isUserActive() {
            try await Task.sleep(for: idlePoll)
        }
        try Task.checkCancellation()
    }

    /// Previews of `batch`, `concurrency` at a time; unreadable ones are left out.
    private func previews(_ batch: [SearchJob]) async throws -> [String: CGImage] {
        let load = preview
        let width = max(1, concurrency)
        return try await withThrowingTaskGroup(of: (String, CGImage?).self) { group in
            var out: [String: CGImage] = [:]
            for (i, job) in batch.enumerated() {
                if i >= width, let (key, image) = try await group.next() {
                    if let image { out[key] = image }
                }
                group.addTask {
                    do {
                        return (job.key, try await load(job.url))
                    } catch is CancellationError {
                        throw CancellationError()
                    } catch {
                        return (job.key, nil)
                    }
                }
            }
            for try await (key, image) in group {
                if let image { out[key] = image }
            }
            return out
        }
    }
}
