import CoreGraphics
import LuxtableFFI
import Foundation

/// Finds and embeds the faces of a session's photos (and reads where they
/// were taken: XMP location fields, GPS), in the background.
///
/// Like `SearchIndexer`: each photo's embedded preview is decoded (the full
/// 1616 px, small faces need it; never the RAW), faces are detected and
/// embedded (`FaceEmbedding`), and stored per content key, so a photo is scanned
/// once whatever it's called. It pauses while the user culls (the same activity
/// gate as background hashing and search indexing), waits for photos still being
/// identified, and runs at background priority; cancel the task to stop (what was
/// stored stays). Every `syncEvery` photos and at the end, the session is synced
/// for Find (faces and suggestions across sessions).
public struct FaceIndexer: Sendable {
    public struct Progress: Sendable, Equatable {
        public var done: Int
        public var total: Int
    }

    public struct Report: Sendable, Equatable {
        public var scanned = 0
        public var faces = 0
        /// Photos whose preview couldn't be read or analyzed (skipped this run).
        public var failed = 0
    }

    public let backend: any PeopleBackend
    public let embedder: any FaceEmbedding
    public let preview: @Sendable (URL) async throws -> CGImage
    public let isUserActive: @Sendable () -> Bool
    /// Find's bar for counting an unconfirmed face as a person (see `PeopleTuning`).
    public var suggestThreshold: Float = PeopleModel.defaultTuning.autoMatch
    public var syncEvery = 64
    /// Photos whose place is read per call (between activity checks).
    public var locationBatch = 32
    public var idlePoll: Duration = .milliseconds(250)
    public var waitingPoll: Duration = .seconds(3)
    public var maxWaitingPoll: Duration = .seconds(60)

    public init(backend: any PeopleBackend, embedder: any FaceEmbedding,
                preview: @escaping @Sendable (URL) async throws -> CGImage = FaceIndexer.embeddedPreview,
                isUserActive: @escaping @Sendable () -> Bool = { (try? userIsActive()) ?? false }) {
        self.backend = backend
        self.embedder = embedder
        self.preview = preview
        self.isUserActive = isUserActive
    }

    /// The camera's embedded preview at full size (1616 px on the A9), upright.
    public static let previewEdge = 1616

    @Sendable public static func embeddedPreview(_ url: URL) async throws -> CGImage {
        try await RustPreviewDecoder().preview(for: url, maxPixelSize: previewEdge)
    }

    /// Scan every photo of `session` not scanned with this model yet.
    public func run(_ session: SessionInfo, progress: @escaping @Sendable (Progress) -> Void = { _ in }) async throws -> Report {
        var report = Report()
        var done = 0
        var attempted = Set<String>()
        var poll = waitingPoll
        var lastWaiting = -1
        var unsynced = 0
        let model = embedder.modelID
        // Places first: cheap (sidecar XMP, EXIF GPS), and Find wants them.
        while true {
            while isUserActive() { try await Task.sleep(for: idlePoll) }
            try Task.checkCancellation()
            if try await backend.scanLocations(limit: locationBatch, in: session) == 0 { break }
        }
        unsynced += 1
        while true {
            let jobs = try await backend.faceJobs(model: model, in: session).filter { !attempted.contains($0.key) }
            if jobs.isEmpty {
                if unsynced > 0 {
                    try await backend.syncFind(faceModel: model, suggest: suggestThreshold, in: session)
                    unsynced = 0
                }
                let status = try await backend.faceStatus(model: model, in: session)
                if status.waiting == 0 { return report }
                poll = Int(status.waiting) == lastWaiting ? min(poll * 2, maxWaitingPoll) : waitingPoll
                lastWaiting = Int(status.waiting)
                try await Task.sleep(for: poll)
                continue
            }
            let total = done + jobs.count
            progress(Progress(done: done, total: total))
            for job in jobs {
                while isUserActive() { try await Task.sleep(for: idlePoll) }
                try Task.checkCancellation()
                attempted.insert(job.key)
                do {
                    let image = try await preview(job.url)
                    try Task.checkCancellation()
                    let embedder = embedder
                    let faces = try embedder.faces(in: image)
                    try await backend.storeFaces(faces, key: job.key, model: model, in: session)
                    report.scanned += 1
                    report.faces += faces.count
                    unsynced += 1
                } catch is CancellationError {
                    throw CancellationError()
                } catch {
                    report.failed += 1
                }
                done += 1
                progress(Progress(done: done, total: total))
                if unsynced >= syncEvery {
                    try await backend.syncFind(faceModel: model, suggest: suggestThreshold, in: session)
                    unsynced = 0
                }
            }
        }
    }
}
